use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::msr::Msr;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::{MetricEvent, MetricUpdate};

const ENERGY_STATUS_WIDTH: u32 = 32;
const MSR_DRAM_ENERGY_STATUS: u64 = 0x619;
const MSR_PKG_ENERGY_STATUS: u64 = 0x611;
const MSR_RAPL_POWER_UNIT: u64 = 0x606;
const SERVER_DRAM_ENERGY_UNIT_JOULES: f64 = 0.0000153;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RaplDomainKind {
    Package,
    Dram,
}

impl RaplDomainKind {
    fn label(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Dram => "dram",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RaplDomainKey {
    domain: RaplDomainKind,
    scope: RaplScope,
}

#[derive(Clone, Copy, Debug)]
struct RaplDomain {
    key: RaplDomainKey,
    cpu: u32,
    energy_unit_joules: f64,
    status_msr: u64,
}

#[derive(Clone, Debug)]
struct RaplReading {
    at: Instant,
    energy_raw: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct RaplScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl RaplScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            die_group_id: topology.level_id(TopologyLevelKind::DieGroup).unwrap_or(0),
            die_id: topology.level_id(TopologyLevelKind::Die).unwrap_or(0),
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct RaplLeader {
    cpu: u32,
    scope: RaplScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct RaplDomainMetrics {
    pub domain: RaplDomainKind,
    pub energy_joules_total: f64,
    pub power_watts: f64,
    #[serde(flatten)]
    pub scope: RaplScope,
}

impl RaplDomainMetrics {
    fn from_readings(
        domain: RaplDomain,
        previous_energy_raw: u64,
        current_energy_raw: u64,
        elapsed: f64,
        totals: &mut BTreeMap<RaplDomainKey, f64>,
    ) -> Self {
        let consumed_raw = wrapping_delta(previous_energy_raw, current_energy_raw);
        let consumed_joules = consumed_raw as f64 * domain.energy_unit_joules;
        let key = domain.key;
        let energy_joules_total = update_total_energy(totals, key, consumed_joules);

        Self {
            domain: key.domain,
            energy_joules_total,
            power_watts: consumed_joules / elapsed,
            scope: key.scope,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RaplMetrics {
    pub domains: Vec<RaplDomainMetrics>,
}

impl RaplMetrics {
    fn from_readings(
        domain_specs: &[RaplDomain],
        previous: RaplReading,
        current: RaplReading,
        totals: &mut BTreeMap<RaplDomainKey, f64>,
    ) -> Result<Self, String> {
        let elapsed = current
            .at
            .checked_duration_since(previous.at)
            .ok_or_else(|| "RAPL sample timestamp moved backwards".to_string())?
            .as_secs_f64();

        if elapsed == 0.0 {
            return Err("RAPL sample elapsed time is zero".to_string());
        }

        if previous.energy_raw.len() != domain_specs.len()
            || current.energy_raw.len() != domain_specs.len()
        {
            return Err("RAPL reading length does not match discovered domain count".to_string());
        }

        let mut domains = Vec::with_capacity(domain_specs.len());
        for ((domain, previous_energy_raw), current_energy_raw) in domain_specs
            .iter()
            .zip(previous.energy_raw)
            .zip(current.energy_raw)
        {
            domains.push(RaplDomainMetrics::from_readings(
                *domain,
                previous_energy_raw,
                current_energy_raw,
                elapsed,
                totals,
            ));
        }

        Ok(Self { domains })
    }
}

#[derive(Debug)]
pub struct RaplCollector {
    domains: Vec<RaplDomain>,
    previous: Option<RaplReading>,
    totals: BTreeMap<RaplDomainKey, f64>,
}

impl RaplCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        Ok(Self {
            totals: BTreeMap::new(),
            domains: discover_domains(architecture.intel_server_model())?,
            previous: None,
        })
    }

    fn read(&self) -> Result<RaplReading, String> {
        let mut energy_raw = Vec::with_capacity(self.domains.len());
        for domain in &self.domains {
            let msr = Msr::open_readonly(domain.cpu)?;
            energy_raw.push(msr.read(domain.status_msr)? & u64::from(u32::MAX));
        }

        Ok(RaplReading {
            at: Instant::now(),
            energy_raw,
        })
    }

    pub fn sample(&mut self) -> Result<Option<RaplMetrics>, String> {
        let current = self.read()?;
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(None),
        };

        let metrics =
            RaplMetrics::from_readings(&self.domains, previous, current, &mut self.totals)?;

        Ok(Some(metrics))
    }
}

#[derive(Debug)]
pub struct RaplTask {
    collector: RaplCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl RaplTask {
    pub fn new(
        collector: RaplCollector,
        interval: Duration,
        events: mpsc::Sender<MetricEvent>,
    ) -> Self {
        Self {
            collector,
            events,
            interval,
        }
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            match self.collector.sample() {
                Ok(Some(rapl)) => {
                    if self
                        .events
                        .send(MetricEvent::Update(MetricUpdate::Rapl(rapl)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = self.events.send(MetricEvent::Failure(error)).await;
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct RaplDomainLabels {
    die: String,
    die_group: String,
    domain: &'static str,
    package: String,
}

#[derive(Debug)]
pub struct RaplPrometheusMetrics {
    energy_joules: Family<RaplDomainLabels, Counter<f64, AtomicU64>>,
    power_watts: Family<RaplDomainLabels, Gauge<f64, AtomicU64>>,
}

impl RaplPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let energy_joules = Family::<RaplDomainLabels, Counter<f64, AtomicU64>>::default();
        let power_watts = Family::<RaplDomainLabels, Gauge<f64, AtomicU64>>::default();

        registry.register(
            "ocellus_rapl_energy_joules",
            "Total RAPL domain energy consumed since exporter start in joules",
            energy_joules.clone(),
        );
        registry.register(
            "ocellus_rapl_power_watts",
            "Interval-derived RAPL domain power in watts",
            power_watts.clone(),
        );

        Self {
            energy_joules,
            power_watts,
        }
    }

    pub fn update(&self, metrics: RaplMetrics) {
        for domain in metrics.domains {
            let labels = RaplDomainLabels {
                die_group: domain.scope.die_group_id.to_string(),
                die: domain.scope.die_id.to_string(),
                domain: domain.domain.label(),
                package: domain.scope.package_id.to_string(),
            };
            let counter = self.energy_joules.get_or_create(&labels);
            let delta = domain.energy_joules_total - counter.get();
            if delta > 0.0 {
                counter.inc_by(delta);
            }
            self.power_watts
                .get_or_create(&labels)
                .set(domain.power_watts);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RaplDomainSpec {
    kind: RaplDomainKind,
    status_msr: u64,
}

fn domain_specs() -> [RaplDomainSpec; 2] {
    [
        RaplDomainSpec {
            kind: RaplDomainKind::Package,
            status_msr: MSR_PKG_ENERGY_STATUS,
        },
        RaplDomainSpec {
            kind: RaplDomainKind::Dram,
            status_msr: MSR_DRAM_ENERGY_STATUS,
        },
    ]
}

fn available_domains(
    model: IntelServerCpuModel,
    leader: RaplLeader,
    package_energy_unit_joules: f64,
) -> Result<Vec<RaplDomain>, String> {
    domain_specs()
        .into_iter()
        .map(|spec| {
            let energy_unit_joules = match spec.kind {
                RaplDomainKind::Dram if model.has_fixed_dram_energy_unit() => {
                    SERVER_DRAM_ENERGY_UNIT_JOULES
                }
                _ => package_energy_unit_joules,
            };

            Msr::open_readonly(leader.cpu)?.read(spec.status_msr)?;

            Ok(RaplDomain {
                cpu: leader.cpu,
                energy_unit_joules,
                key: RaplDomainKey {
                    domain: spec.kind,
                    scope: leader.scope,
                },
                status_msr: spec.status_msr,
            })
        })
        .collect()
}

fn discover_domains(model: IntelServerCpuModel) -> Result<Vec<RaplDomain>, String> {
    let leaders = rapl_leaders()?;
    let mut domains = Vec::with_capacity(leaders.len());

    for leader in leaders {
        let rapl_power_unit = Msr::open_readonly(leader.cpu)?.read(MSR_RAPL_POWER_UNIT)?;
        let package_energy_unit_joules = energy_unit_joules(rapl_power_unit);
        domains.extend(available_domains(
            model,
            leader,
            package_energy_unit_joules,
        )?);
    }

    if domains.is_empty() {
        return Err("no CPU packages found for RAPL collection".to_string());
    }

    Ok(domains)
}

fn rapl_leaders() -> Result<Vec<RaplLeader>, String> {
    let mut leaders = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        leaders
            .entry(RaplScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err("failed to discover any RAPL scope leaders".to_string());
    }

    Ok(leaders
        .into_iter()
        .map(|(scope, cpu)| RaplLeader { cpu, scope })
        .collect())
}

fn energy_unit_joules(rapl_power_unit: u64) -> f64 {
    let energy_status_unit = (rapl_power_unit >> 8) & 0x1f;
    1.0 / (1_u64 << energy_status_unit) as f64
}

fn update_total_energy(
    totals: &mut BTreeMap<RaplDomainKey, f64>,
    key: RaplDomainKey,
    consumed_joules: f64,
) -> f64 {
    let total = totals.entry(key).or_default();
    *total += consumed_joules;
    *total
}

fn wrapping_delta(previous: u64, current: u64) -> u64 {
    current.wrapping_sub(previous) & ((1_u64 << ENERGY_STATUS_WIDTH) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_energy_unit_from_rapl_power_unit() {
        let rapl_power_unit = 14_u64 << 8;

        assert_eq!(energy_unit_joules(rapl_power_unit), 1.0 / 16384.0);
    }

    #[test]
    fn defines_package_and_dram_domains() {
        assert_eq!(domain_specs().len(), 2);
    }

    #[test]
    fn computes_wrapping_energy_delta() {
        assert_eq!(wrapping_delta(u64::from(u32::MAX) - 4, 10), 15);
    }

    #[test]
    fn accumulates_total_energy_by_scope() {
        let mut totals = BTreeMap::new();

        assert_eq!(
            update_total_energy(&mut totals, domain_key(RaplDomainKind::Package, 0, 0), 1.5),
            1.5
        );
        assert_eq!(
            update_total_energy(&mut totals, domain_key(RaplDomainKind::Package, 0, 0), 2.0),
            3.5
        );
        assert_eq!(
            update_total_energy(&mut totals, domain_key(RaplDomainKind::Dram, 0, 0), 4.0),
            4.0
        );
        assert_eq!(
            update_total_energy(&mut totals, domain_key(RaplDomainKind::Package, 0, 1), 8.0),
            8.0
        );
    }

    #[test]
    fn keeps_die_groups_distinct() {
        let scope = RaplScope {
            die_group_id: 2,
            die_id: 0,
            package_id: 0,
        };

        assert_ne!(scope, domain_key(RaplDomainKind::Package, 0, 0).scope);
    }

    fn domain_key(domain: RaplDomainKind, package_id: u32, die_id: u32) -> RaplDomainKey {
        RaplDomainKey {
            domain,
            scope: RaplScope {
                die_group_id: 0,
                die_id,
                package_id,
            },
        }
    }
}
