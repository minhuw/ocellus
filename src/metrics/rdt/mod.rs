use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::msr::Msr;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::topology_label;
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

const IA32_QM_EVTSEL: u64 = 0xC8D;
const IA32_QM_CTR: u64 = 0xC8E;
const IA32_PQR_ASSOC: u64 = 0xC8F;
const MSR_RMID_SNC_CONFIG: u64 = 0xCA0;
const RMID_LOCALIZED_DISTRIBUTION_MODE_ENABLE: u64 = 1;

const PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";
const RESCTRL_FS_TYPE: &str = "resctrl";

const CPUID_RDT_MONITORING: u32 = 0x0F;
const CPUID_RDT_L3_MONITORING_SUBLEAF: u32 = 1;
const MBM_COUNTER_WIDTH_BASE: u32 = 24;
const MBM_COUNTER_WIDTH_MASK: u32 = 0xff;

const L3_OCCUPANCY_EVENT: u64 = 0x01;
const TOTAL_MEMORY_BANDWIDTH_EVENT: u64 = 0x02;
const LOCAL_MEMORY_BANDWIDTH_EVENT: u64 = 0x03;

const QM_CTR_DATA_MASK: u64 = (1_u64 << 62) - 1;
const QM_CTR_UNAVAILABLE_BIT: u64 = 1_u64 << 62;
const QM_CTR_ERROR_BIT: u64 = 1_u64 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct RdtScope {
    pub core_id: u32,
    pub cpu: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub die_group_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub die_id: Option<u32>,
    pub package_id: u32,
}

impl RdtScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            core_id: package_local_core_id(topology)?,
            cpu: topology.cpu,
            die_group_id: topology.level_id(TopologyLevelKind::DieGroup),
            die_id: topology.level_id(TopologyLevelKind::Die),
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RdtDomainKey {
    package_id: u32,
    die_group_id: u32,
    die_id: u32,
    node_id: u32,
    core_id: u32,
    cache: RdtCacheDomain,
}

impl RdtDomainKey {
    fn from_scope(scope: RdtScope, node_id: u32, cache: RdtCacheDomain) -> Self {
        Self {
            package_id: scope.package_id,
            die_group_id: scope.die_group_id.unwrap_or(0),
            die_id: scope.die_id.unwrap_or(0),
            node_id,
            core_id: scope.core_id,
            cache,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RdtCacheDomain {
    SharedCpuList(Vec<u32>),
    Topology { module_id: u32, tile_id: u32 },
}

impl RdtCacheDomain {
    fn cpus_in_scope(
        &self,
        topology_cpu: u32,
        scope: RdtScope,
        node_id: u32,
        scopes_by_cpu: &BTreeMap<u32, RdtScope>,
        node_ids_by_cpu: &BTreeMap<u32, u32>,
    ) -> Vec<u32> {
        match self {
            Self::SharedCpuList(cpus) => cpus
                .iter()
                .copied()
                .filter(|cpu| {
                    let Some(cpu_scope) = scopes_by_cpu.get(cpu) else {
                        return false;
                    };
                    let Some(cpu_node_id) = node_ids_by_cpu.get(cpu) else {
                        return false;
                    };

                    same_rdt_scope(*cpu_scope, scope) && *cpu_node_id == node_id
                })
                .collect(),
            Self::Topology { .. } => vec![topology_cpu],
        }
    }
}

#[derive(Clone, Debug)]
struct RdtDomainBuilder {
    cpus: Vec<u32>,
    node_id: u32,
    scope: RdtScope,
}

#[derive(Clone, Debug)]
struct RdtDomain {
    cpus: Vec<u32>,
    original_pqr_assoc: Vec<RdtCpuAssociation>,
    physical_rmid: u32,
    rmid: u32,
    scope: RdtScope,
    snc_nodes_per_l3_cache: u32,
}

#[derive(Clone, Copy, Debug)]
struct RdtCpuAssociation {
    cpu: u32,
    value: u64,
}

#[derive(Clone, Copy, Debug)]
struct RdtSavedMsr {
    cpu: u32,
    value: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RdtCapabilities {
    conversion_factor_bytes: u64,
    local_bandwidth: bool,
    mbm_counter_mask: u64,
    max_rmid: u32,
    occupancy: bool,
    rmid_mask: u64,
    total_bandwidth: bool,
}

#[derive(Clone, Copy, Debug)]
struct RdtCounters {
    local_memory_bandwidth: Option<u64>,
    total_memory_bandwidth: Option<u64>,
}

#[derive(Clone, Debug)]
struct RdtReading {
    at: Instant,
    scopes: Vec<RdtScopeReading>,
}

#[derive(Clone, Debug)]
struct RdtScopeReading {
    conversion_factor_bytes: f64,
    counters: RdtCounters,
    l3_occupancy_bytes: Option<f64>,
    scope: RdtScope,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RdtMetrics {
    pub scopes: Vec<RdtScopeMetrics>,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct RdtScopeMetrics {
    pub l3_occupancy_bytes: Option<f64>,
    pub local_memory_bandwidth_bytes_per_second: Option<f64>,
    pub remote_memory_bandwidth_bytes_per_second: Option<f64>,
    #[serde(flatten)]
    pub scope: RdtScope,
    pub total_memory_bandwidth_bytes_per_second: Option<f64>,
}

impl RdtMetrics {
    fn from_readings(
        previous: RdtReading,
        current: RdtReading,
        mbm_counter_mask: u64,
    ) -> Result<Self, String> {
        let elapsed = current
            .at
            .checked_duration_since(previous.at)
            .ok_or_else(|| "RDT sample timestamp moved backwards".to_string())?
            .as_secs_f64();

        if elapsed == 0.0 {
            return Err("RDT sample elapsed time is zero".to_string());
        }

        if previous.scopes.len() != current.scopes.len() {
            return Err("RDT reading length does not match discovered scope count".to_string());
        }

        let mut scopes = Vec::with_capacity(current.scopes.len());
        for (previous, current) in previous.scopes.iter().zip(&current.scopes) {
            if previous.scope != current.scope {
                return Err("RDT reading scope order changed between samples".to_string());
            }
            if previous.conversion_factor_bytes != current.conversion_factor_bytes {
                return Err("RDT conversion factor changed between samples".to_string());
            }

            scopes.push(RdtScopeMetrics::from_readings(
                previous,
                current,
                elapsed,
                mbm_counter_mask,
            ));
        }

        Ok(Self { scopes })
    }
}

impl RdtScopeMetrics {
    fn from_readings(
        previous: &RdtScopeReading,
        current: &RdtScopeReading,
        elapsed: f64,
        mbm_counter_mask: u64,
    ) -> Self {
        let total_memory_bandwidth_bytes_per_second = bandwidth_bytes_per_second(
            previous.counters.total_memory_bandwidth,
            current.counters.total_memory_bandwidth,
            current.conversion_factor_bytes,
            mbm_counter_mask,
            elapsed,
        );
        let local_memory_bandwidth_bytes_per_second = bandwidth_bytes_per_second(
            previous.counters.local_memory_bandwidth,
            current.counters.local_memory_bandwidth,
            current.conversion_factor_bytes,
            mbm_counter_mask,
            elapsed,
        );
        let remote_memory_bandwidth_bytes_per_second = match (
            total_memory_bandwidth_bytes_per_second,
            local_memory_bandwidth_bytes_per_second,
        ) {
            (Some(total), Some(local)) => Some((total - local).max(0.0)),
            _ => None,
        };

        Self {
            l3_occupancy_bytes: current.l3_occupancy_bytes,
            local_memory_bandwidth_bytes_per_second,
            remote_memory_bandwidth_bytes_per_second,
            scope: current.scope,
            total_memory_bandwidth_bytes_per_second,
        }
    }
}

#[derive(Debug)]
pub struct RdtCollector {
    capabilities: RdtCapabilities,
    domains: Vec<RdtDomain>,
    original_rmid_snc_configs: Vec<RdtSavedMsr>,
    previous: Option<RdtReading>,
    restore_msr_state: bool,
}

impl RdtCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let capabilities = rdt_capabilities()
            .filter(|capabilities| supported_model_for_rdt_monitoring(model, capabilities))
            .ok_or_else(|| {
                format!(
                    "RDT monitoring is not supported by this processor model or CPUID capabilities: {:?}",
                    model
                )
            })?;
        ensure_resctrl_unmounted()?;
        let mut domains = rdt_domains(capabilities.max_rmid)?;
        let original_rmid_snc_configs = initialize_snc_rmid_sharing(&domains)?;

        if let Err(error) = initialize_domains(&mut domains, capabilities.rmid_mask) {
            restore_snc_configs_if_resctrl_unmounted(&original_rmid_snc_configs);
            return Err(error);
        }

        Ok(Self {
            capabilities,
            domains,
            original_rmid_snc_configs,
            previous: None,
            restore_msr_state: true,
        })
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        let model = architecture.intel_server_model();

        rdt_capabilities()
            .is_some_and(|capabilities| supported_model_for_rdt_monitoring(model, &capabilities))
            && !resctrl_is_mounted().unwrap_or(true)
    }

    fn read(&self) -> Result<RdtReading, String> {
        let mut scopes = Vec::with_capacity(self.domains.len());
        for domain in &self.domains {
            scopes.push(read_domain(domain, self.capabilities)?);
        }

        Ok(RdtReading {
            at: Instant::now(),
            scopes,
        })
    }

    pub fn sample(&mut self) -> Result<Option<RdtMetrics>, String> {
        if let Err(error) = ensure_resctrl_unmounted() {
            self.restore_msr_state = false;
            return Err(error);
        }

        let current = self.read()?;
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(None),
        };

        Ok(Some(RdtMetrics::from_readings(
            previous,
            current,
            self.capabilities.mbm_counter_mask,
        )?))
    }
}

impl Drop for RdtCollector {
    fn drop(&mut self) {
        if self.restore_msr_state && !resctrl_is_mounted().unwrap_or(true) {
            restore_domains(&self.domains);
            restore_snc_configs(&self.original_rmid_snc_configs);
        }
    }
}

#[derive(Debug)]
pub struct RdtTask {
    collector: RdtCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl RdtTask {
    pub fn new(
        collector: RdtCollector,
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
                Ok(Some(rdt)) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Rdt(Box::new(
                            rdt,
                        )))))
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
struct RdtScopeLabels {
    core: String,
    cpu: String,
    die: String,
    die_group: String,
    package: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct RdtMemoryBandwidthLabels {
    core: String,
    cpu: String,
    die: String,
    die_group: String,
    package: String,
    traffic: &'static str,
}

#[derive(Debug)]
pub struct RdtPrometheusMetrics {
    l3_occupancy_bytes: Family<RdtScopeLabels, Gauge<f64, AtomicU64>>,
    memory_bandwidth_bytes_per_second: Family<RdtMemoryBandwidthLabels, Gauge<f64, AtomicU64>>,
}

impl RdtPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        if !metadata.collectors.rdt_supported {
            return None;
        }

        let l3_occupancy_bytes = Family::<RdtScopeLabels, Gauge<f64, AtomicU64>>::default();
        let memory_bandwidth_bytes_per_second =
            Family::<RdtMemoryBandwidthLabels, Gauge<f64, AtomicU64>>::default();

        registry.register(
            "ocellus_rdt_l3_occupancy_bytes",
            "RDT CMT L3 cache occupancy in bytes for the monitored RDT domain",
            l3_occupancy_bytes.clone(),
        );
        registry.register(
            "ocellus_rdt_memory_bandwidth_bytes_per_second",
            "RDT MBM memory bandwidth in bytes per second for the monitored RDT domain",
            memory_bandwidth_bytes_per_second.clone(),
        );

        Some(Self {
            l3_occupancy_bytes,
            memory_bandwidth_bytes_per_second,
        })
    }

    pub fn update(&self, metrics: RdtMetrics) {
        for scope_metrics in metrics.scopes {
            let scope_labels = RdtScopeLabels {
                core: scope_metrics.scope.core_id.to_string(),
                cpu: scope_metrics.scope.cpu.to_string(),
                die: topology_label(scope_metrics.scope.die_id),
                die_group: topology_label(scope_metrics.scope.die_group_id),
                package: scope_metrics.scope.package_id.to_string(),
            };

            if let Some(l3_occupancy_bytes) = scope_metrics.l3_occupancy_bytes {
                self.l3_occupancy_bytes
                    .get_or_create(&scope_labels)
                    .set(l3_occupancy_bytes);
            }

            let bandwidths = [
                (
                    "total",
                    scope_metrics.total_memory_bandwidth_bytes_per_second,
                ),
                (
                    "local",
                    scope_metrics.local_memory_bandwidth_bytes_per_second,
                ),
                (
                    "remote",
                    scope_metrics.remote_memory_bandwidth_bytes_per_second,
                ),
            ];
            for (traffic, bytes_per_second) in bandwidths {
                if let Some(bytes_per_second) = bytes_per_second {
                    self.memory_bandwidth_bytes_per_second
                        .get_or_create(&RdtMemoryBandwidthLabels {
                            core: scope_metrics.scope.core_id.to_string(),
                            cpu: scope_metrics.scope.cpu.to_string(),
                            die: topology_label(scope_metrics.scope.die_id),
                            die_group: topology_label(scope_metrics.scope.die_group_id),
                            package: scope_metrics.scope.package_id.to_string(),
                            traffic,
                        })
                        .set(bytes_per_second);
                }
            }
        }
    }
}

fn rdt_capabilities() -> Option<RdtCapabilities> {
    let cpuid = raw_cpuid::CpuId::new();

    if !cpuid
        .get_extended_feature_info()
        .is_some_and(|features| features.has_rdtm())
    {
        return None;
    }

    let info = cpuid.get_rdt_monitoring_info()?;
    let l3 = info.l3_monitoring()?;
    let max_rmid = info.rmid_range().min(l3.maximum_rmid_range());

    Some(RdtCapabilities {
        conversion_factor_bytes: u64::from(l3.conversion_factor()),
        local_bandwidth: l3.has_local_bandwidth_monitoring(),
        mbm_counter_mask: mbm_counter_mask(mbm_counter_width()),
        max_rmid,
        occupancy: l3.has_occupancy_monitoring(),
        rmid_mask: rmid_mask(max_rmid),
        total_bandwidth: l3.has_total_bandwidth_monitoring(),
    })
    .filter(|capabilities| {
        capabilities.occupancy
            && capabilities.conversion_factor_bytes > 0
            && capabilities.max_rmid > 0
    })
}

fn supported_model_for_rdt_monitoring(
    model: IntelServerCpuModel,
    capabilities: &RdtCapabilities,
) -> bool {
    if !capabilities.occupancy {
        return false;
    }

    matches!(
        model,
        IntelServerCpuModel::HaswellXeon
            | IntelServerCpuModel::BroadwellXeon
            | IntelServerCpuModel::BroadwellDe
            | IntelServerCpuModel::KnightsLanding
            | IntelServerCpuModel::SkylakeXeon
            | IntelServerCpuModel::IceLakeXeon
            | IntelServerCpuModel::SapphireRapids
            | IntelServerCpuModel::EmeraldRapids
    )
}

fn initialize_domains(domains: &mut [RdtDomain], rmid_mask: u64) -> Result<(), String> {
    for domain_index in 0..domains.len() {
        let result = initialize_domain(&mut domains[domain_index], rmid_mask);

        if let Err(error) = result {
            restore_domains_if_resctrl_unmounted(domains);
            return Err(error);
        }
    }

    Ok(())
}

fn initialize_domain(domain: &mut RdtDomain, rmid_mask: u64) -> Result<(), String> {
    for cpu in &domain.cpus {
        let msr = Msr::open(*cpu)?;
        let current = msr.read(IA32_PQR_ASSOC)?;
        let current_rmid = pqr_assoc_rmid(current, rmid_mask);
        if current_rmid != 0 {
            return Err(format!(
                "CPU {cpu} already has RDT RMID {current_rmid}; refusing to overwrite existing RMID assignments"
            ));
        }

        domain.original_pqr_assoc.push(RdtCpuAssociation {
            cpu: *cpu,
            value: current,
        });
        write_pqr_assoc_rmid(&msr, current, domain.rmid, rmid_mask)?;
    }

    Ok(())
}

fn restore_domains_if_resctrl_unmounted(domains: &[RdtDomain]) {
    if !resctrl_is_mounted().unwrap_or(true) {
        restore_domains(domains);
    }
}

fn restore_domains(domains: &[RdtDomain]) {
    for domain in domains {
        for association in &domain.original_pqr_assoc {
            let Ok(msr) = Msr::open(association.cpu) else {
                continue;
            };
            let _ = msr.write(IA32_PQR_ASSOC, association.value);
        }
    }
}

fn initialize_snc_rmid_sharing(domains: &[RdtDomain]) -> Result<Vec<RdtSavedMsr>, String> {
    let mut saved_configs = Vec::new();
    let mut package_cpus: BTreeMap<u32, u32> = BTreeMap::new();

    for domain in domains {
        if domain.snc_nodes_per_l3_cache <= 1 {
            continue;
        }

        package_cpus
            .entry(domain.scope.package_id)
            .and_modify(|cpu| *cpu = (*cpu).min(domain.scope.cpu))
            .or_insert(domain.scope.cpu);
    }

    for (package_id, cpu) in package_cpus {
        let msr = match Msr::open(cpu) {
            Ok(msr) => msr,
            Err(error) => {
                restore_snc_configs_if_resctrl_unmounted(&saved_configs);
                return Err(error);
            }
        };
        let current = match msr.read(MSR_RMID_SNC_CONFIG) {
            Ok(current) => current,
            Err(error) => {
                restore_snc_configs_if_resctrl_unmounted(&saved_configs);
                return Err(format!(
                    "failed to read RDT SNC RMID mode for package {package_id}: {error}"
                ));
            }
        };
        saved_configs.push(RdtSavedMsr {
            cpu,
            value: current,
        });

        let sharing_mode = rmid_snc_sharing_mode_value(current);
        if sharing_mode != current {
            if let Err(error) = msr.write(MSR_RMID_SNC_CONFIG, sharing_mode) {
                restore_snc_configs_if_resctrl_unmounted(&saved_configs);
                return Err(format!(
                    "failed to enable RDT SNC RMID sharing mode for package {package_id}: {error}"
                ));
            }
        }
    }

    Ok(saved_configs)
}

fn restore_snc_configs_if_resctrl_unmounted(configs: &[RdtSavedMsr]) {
    if !resctrl_is_mounted().unwrap_or(true) {
        restore_snc_configs(configs);
    }
}

fn restore_snc_configs(configs: &[RdtSavedMsr]) {
    for config in configs {
        let Ok(msr) = Msr::open(config.cpu) else {
            continue;
        };
        let _ = msr.write(MSR_RMID_SNC_CONFIG, config.value);
    }
}

fn rmid_snc_sharing_mode_value(current: u64) -> u64 {
    current & !RMID_LOCALIZED_DISTRIBUTION_MODE_ENABLE
}

fn read_domain(
    domain: &RdtDomain,
    capabilities: RdtCapabilities,
) -> Result<RdtScopeReading, String> {
    assign_domain_rmid(domain, capabilities.rmid_mask)?;
    let msr = Msr::open(domain.scope.cpu)?;
    let conversion_factor_bytes =
        domain_conversion_factor_bytes(capabilities.conversion_factor_bytes, domain);

    let l3_occupancy_bytes = if capabilities.occupancy {
        read_monitoring_value(&msr, domain.physical_rmid, L3_OCCUPANCY_EVENT)?
            .map(|value| value as f64 * conversion_factor_bytes)
    } else {
        None
    };

    let total_memory_bandwidth = if capabilities.total_bandwidth {
        read_monitoring_value(&msr, domain.physical_rmid, TOTAL_MEMORY_BANDWIDTH_EVENT)?
    } else {
        None
    };
    let local_memory_bandwidth = if capabilities.local_bandwidth {
        read_monitoring_value(&msr, domain.physical_rmid, LOCAL_MEMORY_BANDWIDTH_EVENT)?
    } else {
        None
    };

    Ok(RdtScopeReading {
        conversion_factor_bytes,
        counters: RdtCounters {
            local_memory_bandwidth,
            total_memory_bandwidth,
        },
        l3_occupancy_bytes,
        scope: domain.scope,
    })
}

fn domain_conversion_factor_bytes(conversion_factor_bytes: u64, domain: &RdtDomain) -> f64 {
    conversion_factor_bytes as f64 / f64::from(domain.snc_nodes_per_l3_cache.max(1))
}

fn assign_domain_rmid(domain: &RdtDomain, rmid_mask: u64) -> Result<(), String> {
    for cpu in &domain.cpus {
        let msr = Msr::open(*cpu)?;
        assign_rmid(&msr, domain.rmid, rmid_mask)?;
    }

    Ok(())
}

fn assign_rmid(msr: &Msr, rmid: u32, rmid_mask: u64) -> Result<(), String> {
    let current = msr.read(IA32_PQR_ASSOC)?;

    write_pqr_assoc_rmid(msr, current, rmid, rmid_mask)
}

fn write_pqr_assoc_rmid(msr: &Msr, current: u64, rmid: u32, rmid_mask: u64) -> Result<(), String> {
    let next = (current & !rmid_mask) | (u64::from(rmid) & rmid_mask);

    if next != current {
        msr.write(IA32_PQR_ASSOC, next)?;
    }

    Ok(())
}

fn pqr_assoc_rmid(value: u64, rmid_mask: u64) -> u64 {
    value & rmid_mask
}

fn read_monitoring_value(msr: &Msr, rmid: u32, event: u64) -> Result<Option<u64>, String> {
    msr.write(IA32_QM_EVTSEL, event_select_value(rmid, event))?;
    parse_monitoring_counter(msr.read(IA32_QM_CTR)?, event, rmid)
}

fn event_select_value(rmid: u32, event: u64) -> u64 {
    (u64::from(rmid) << 32) | event
}

fn parse_monitoring_counter(counter: u64, event: u64, rmid: u32) -> Result<Option<u64>, String> {
    if counter & QM_CTR_ERROR_BIT != 0 {
        return Err(format!(
            "RDT monitoring counter reported an unsupported event or RMID: event=0x{event:x}, rmid={rmid}"
        ));
    }

    if counter & QM_CTR_UNAVAILABLE_BIT != 0 {
        return Ok(None);
    }

    Ok(Some(counter & QM_CTR_DATA_MASK))
}

fn rdt_domains(max_rmid: u32) -> Result<Vec<RdtDomain>, String> {
    rdt_domains_from_topologies_with_numa(
        metal::topology::cpu_topologies()?,
        max_rmid,
        rdt_cache_domain,
        cpu_numa_node,
    )
}

#[cfg(test)]
fn rdt_domains_from_topologies(
    topologies: impl IntoIterator<Item = CpuTopology>,
    max_rmid: u32,
    cache_domain: impl FnMut(&CpuTopology) -> Result<RdtCacheDomain, String>,
) -> Result<Vec<RdtDomain>, String> {
    rdt_domains_from_topologies_with_numa(topologies, max_rmid, cache_domain, |_| Ok(None))
}

fn rdt_domains_from_topologies_with_numa(
    topologies: impl IntoIterator<Item = CpuTopology>,
    max_rmid: u32,
    mut cache_domain: impl FnMut(&CpuTopology) -> Result<RdtCacheDomain, String>,
    mut numa_node: impl FnMut(u32) -> Result<Option<u32>, String>,
) -> Result<Vec<RdtDomain>, String> {
    let mut domains: BTreeMap<RdtDomainKey, RdtDomainBuilder> = BTreeMap::new();
    let topologies = topologies.into_iter().collect::<Vec<_>>();
    let mut scopes_by_cpu = BTreeMap::new();
    let mut node_ids_by_cpu = BTreeMap::new();

    for topology in &topologies {
        let scope = RdtScope::from_topology(topology)?;
        let node_id = numa_node(topology.cpu)?.or(scope.die_id).unwrap_or(0);

        scopes_by_cpu.insert(topology.cpu, scope);
        node_ids_by_cpu.insert(topology.cpu, node_id);
    }

    for topology in topologies {
        let scope = *scopes_by_cpu
            .get(&topology.cpu)
            .ok_or_else(|| format!("CPU {} is missing RDT scope", topology.cpu))?;
        let node_id = *node_ids_by_cpu
            .get(&topology.cpu)
            .ok_or_else(|| format!("CPU {} is missing NUMA node", topology.cpu))?;
        let cache = cache_domain(&topology)?;
        let cpus = cache.cpus_in_scope(
            topology.cpu,
            scope,
            node_id,
            &scopes_by_cpu,
            &node_ids_by_cpu,
        );
        let key = RdtDomainKey::from_scope(scope, node_id, cache);
        let builder = domains.entry(key).or_insert_with(|| RdtDomainBuilder {
            cpus: Vec::new(),
            node_id,
            scope,
        });

        if topology.cpu < builder.scope.cpu {
            builder.scope.cpu = topology.cpu;
            builder.node_id = node_id;
        }
        builder.cpus.extend(cpus);
    }

    if domains.is_empty() {
        return Err("failed to discover any RDT monitoring domains".to_string());
    }

    let snc_nodes_by_cache = snc_nodes_by_cache(&domains);
    let mut result = Vec::with_capacity(domains.len());
    for (index, (key, mut builder)) in domains.into_iter().enumerate() {
        let snc_nodes_per_l3_cache = snc_nodes_by_cache
            .get(&key.cache)
            .copied()
            .unwrap_or(1)
            .max(1);
        let rmid = u32::try_from(index + 1)
            .map_err(|error| format!("RDT RMID index overflowed: {error}"))?;
        let physical_rmid = physical_rmid(rmid, builder.node_id, snc_nodes_per_l3_cache, max_rmid)
            .ok_or_else(|| {
                format!(
                    "RDT monitoring exposes {max_rmid} physical RMIDs across {snc_nodes_per_l3_cache} SNC nodes, but {} RDT monitoring domains were discovered",
                    index + 1
                )
            })?;

        builder.cpus.sort_unstable();
        builder.cpus.dedup();

        result.push(RdtDomain {
            cpus: builder.cpus,
            original_pqr_assoc: Vec::new(),
            physical_rmid,
            rmid,
            scope: builder.scope,
            snc_nodes_per_l3_cache,
        });
    }

    Ok(result)
}

fn same_rdt_scope(left: RdtScope, right: RdtScope) -> bool {
    left.package_id == right.package_id
        && left.die_group_id == right.die_group_id
        && left.die_id == right.die_id
        && left.core_id == right.core_id
}

fn package_local_core_id(topology: &CpuTopology) -> Result<u32, String> {
    let package_shift = topology
        .levels
        .iter()
        .find(|level| level.kind == TopologyLevelKind::Package)
        .map(|level| level.shift)
        .ok_or_else(|| "CPU topology is missing package level".to_string())?;
    let smt_shift = topology
        .levels
        .iter()
        .find(|level| level.kind == TopologyLevelKind::Smt)
        .map(|level| level.shift)
        .unwrap_or(0);

    if package_shift < smt_shift {
        return Err(format!(
            "CPU topology package shift {package_shift} is below SMT shift {smt_shift}"
        ));
    }

    let core_width = package_shift - smt_shift;
    if core_width == 0 {
        return Ok(0);
    }

    let core_mask = if core_width >= u32::BITS {
        u32::MAX
    } else {
        (1_u32 << core_width) - 1
    };

    Ok((topology.x2apic_id >> smt_shift) & core_mask)
}

fn snc_nodes_by_cache(
    domains: &BTreeMap<RdtDomainKey, RdtDomainBuilder>,
) -> BTreeMap<RdtCacheDomain, u32> {
    let mut nodes_by_cache: BTreeMap<RdtCacheDomain, Vec<u32>> = BTreeMap::new();

    for (key, builder) in domains {
        nodes_by_cache
            .entry(key.cache.clone())
            .or_default()
            .push(builder.node_id);
    }

    nodes_by_cache
        .into_iter()
        .map(|(cache, mut nodes)| {
            nodes.sort_unstable();
            nodes.dedup();
            (cache, nodes.len().max(1) as u32)
        })
        .collect()
}

fn physical_rmid(
    logical_rmid: u32,
    node_id: u32,
    snc_nodes_per_l3_cache: u32,
    max_physical_rmid: u32,
) -> Option<u32> {
    let snc_nodes_per_l3_cache = snc_nodes_per_l3_cache.max(1);
    let logical_rmid_count = (max_physical_rmid + 1) / snc_nodes_per_l3_cache;
    if logical_rmid_count == 0 || logical_rmid >= logical_rmid_count {
        return None;
    }

    Some(logical_rmid + (node_id % snc_nodes_per_l3_cache) * logical_rmid_count)
}

fn rdt_cache_domain(topology: &CpuTopology) -> Result<RdtCacheDomain, String> {
    if let Some(cpus) = shared_l3_cpu_list(topology.cpu)? {
        return Ok(RdtCacheDomain::SharedCpuList(cpus));
    }

    Ok(RdtCacheDomain::Topology {
        module_id: topology.level_id(TopologyLevelKind::Module).unwrap_or(0),
        tile_id: topology.level_id(TopologyLevelKind::Tile).unwrap_or(0),
    })
}

fn cpu_numa_node(cpu: u32) -> Result<Option<u32>, String> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}");
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read {path}: {error}")),
    };

    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read {path} entry: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(node) = name.strip_prefix("node") else {
            continue;
        };
        let Ok(node) = node.parse::<u32>() else {
            continue;
        };

        return Ok(Some(node));
    }

    Ok(None)
}

fn shared_l3_cpu_list(cpu: u32) -> Result<Option<Vec<u32>>, String> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index3/shared_cpu_list");
    let cpulist = match std::fs::read_to_string(&path) {
        Ok(cpulist) => cpulist,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read {path}: {error}")),
    };
    let cpus = parse_cpu_list(&cpulist)
        .map_err(|error| format!("failed to parse {path} value {cpulist:?}: {error}"))?;

    if cpus.is_empty() {
        Ok(None)
    } else {
        Ok(Some(cpus))
    }
}

fn parse_cpu_list(cpulist: &str) -> Result<Vec<u32>, String> {
    let mut cpus = Vec::new();

    for item in cpulist.trim().split(',').filter(|item| !item.is_empty()) {
        match item.split_once('-') {
            Some((start, end)) => {
                let start = parse_cpu_id(start)?;
                let end = parse_cpu_id(end)?;
                if end < start {
                    return Err(format!("invalid descending CPU range {item}"));
                }
                cpus.extend(start..=end);
            }
            None => cpus.push(parse_cpu_id(item)?),
        }
    }

    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

fn parse_cpu_id(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|error| format!("invalid CPU id {value:?}: {error}"))
}

fn rmid_mask(max_rmid: u32) -> u64 {
    bit_width_mask(rmid_width(max_rmid))
}

fn rmid_width(max_rmid: u32) -> u32 {
    if max_rmid == 0 {
        0
    } else {
        u32::BITS - max_rmid.leading_zeros()
    }
}

fn mbm_counter_mask(width: u32) -> u64 {
    bit_width_mask(width.min(62))
}

fn mbm_counter_width() -> u32 {
    let l3_monitoring = raw_cpuid::cpuid!(CPUID_RDT_MONITORING, CPUID_RDT_L3_MONITORING_SUBLEAF);
    MBM_COUNTER_WIDTH_BASE + (l3_monitoring.eax & MBM_COUNTER_WIDTH_MASK)
}

fn bit_width_mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else if width == 0 {
        0
    } else {
        (1_u64 << width) - 1
    }
}

fn bandwidth_bytes_per_second(
    previous: Option<u64>,
    current: Option<u64>,
    conversion_factor_bytes: f64,
    counter_mask: u64,
    elapsed: f64,
) -> Option<f64> {
    let delta = current?.wrapping_sub(previous?) & counter_mask;

    Some(delta as f64 * conversion_factor_bytes / elapsed)
}

fn ensure_resctrl_unmounted() -> Result<(), String> {
    let mount_points = resctrl_mount_points()?;
    if mount_points.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Linux resctrl is mounted at {}; refusing MSR-based RDT RMID assignment to avoid corrupting kernel-managed RMID state",
        mount_points.join(", ")
    ))
}

fn resctrl_is_mounted() -> Result<bool, String> {
    Ok(!resctrl_mount_points()?.is_empty())
}

fn resctrl_mount_points() -> Result<Vec<String>, String> {
    let mountinfo = std::fs::read_to_string(PROC_SELF_MOUNTINFO)
        .map_err(|error| format!("failed to read {PROC_SELF_MOUNTINFO}: {error}"))?;

    Ok(parse_resctrl_mount_points(&mountinfo))
}

fn parse_resctrl_mount_points(mountinfo: &str) -> Vec<String> {
    mountinfo
        .lines()
        .filter(|line| mountinfo_fs_type(line) == Some(RESCTRL_FS_TYPE))
        .filter_map(mountinfo_mount_point)
        .map(decode_mountinfo_path)
        .collect()
}

fn mountinfo_fs_type(line: &str) -> Option<&str> {
    line.split_once(" - ")?.1.split_whitespace().next()
}

fn mountinfo_mount_point(line: &str) -> Option<&str> {
    line.split_whitespace().nth(4)
}

fn decode_mountinfo_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        let mut value = 0_u32;
        let mut digits = 0;
        while digits < 3 {
            let Some(next) = chars.peek().copied() else {
                break;
            };
            let Some(digit) = next.to_digit(8) else {
                break;
            };
            chars.next();
            value = value * 8 + digit;
            digits += 1;
        }

        if digits == 3
            && let Some(decoded) = char::from_u32(value)
        {
            result.push(decoded);
        } else {
            result.push('\\');
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_event_select_value() {
        assert_eq!(
            event_select_value(7, L3_OCCUPANCY_EVENT),
            0x0000_0007_0000_0001
        );
    }

    #[test]
    fn preserves_clos_bits_with_rmid_mask() {
        let current = 0x0000_0005_0000_1234_u64;
        let mask = rmid_mask(2047);
        let next = (current & !mask) | (0x3a_u64 & mask);

        assert_eq!(next, 0x0000_0005_0000_103a);
    }

    #[test]
    fn extracts_pqr_assoc_rmid() {
        let current = 0x0000_0005_0000_1234_u64;

        assert_eq!(pqr_assoc_rmid(current, rmid_mask(2047)), 0x234);
    }

    #[test]
    fn computes_rmid_width_and_mask_from_max_rmid() {
        assert_eq!(rmid_width(0), 0);
        assert_eq!(rmid_width(1), 1);
        assert_eq!(rmid_width(2), 2);
        assert_eq!(rmid_width(1023), 10);
        assert_eq!(rmid_mask(0), 0);
        assert_eq!(rmid_mask(1023), 0x3ff);
        assert_eq!(rmid_mask(1024), 0x7ff);
    }

    #[test]
    fn computes_mbm_counter_mask_from_width() {
        assert_eq!(mbm_counter_mask(24), 0x00ff_ffff);
        assert_eq!(mbm_counter_mask(32), 0xffff_ffff);
        assert_eq!(mbm_counter_mask(62), QM_CTR_DATA_MASK);
        assert_eq!(mbm_counter_mask(63), QM_CTR_DATA_MASK);
    }

    #[test]
    fn parses_monitoring_counter_status_bits() {
        assert_eq!(
            parse_monitoring_counter(0x1234, L3_OCCUPANCY_EVENT, 1).unwrap(),
            Some(0x1234)
        );
        assert_eq!(
            parse_monitoring_counter(QM_CTR_UNAVAILABLE_BIT, L3_OCCUPANCY_EVENT, 1).unwrap(),
            None
        );
        assert!(parse_monitoring_counter(QM_CTR_ERROR_BIT, L3_OCCUPANCY_EVENT, 1).is_err());
    }

    #[test]
    fn computes_bandwidth_from_wrapping_counter_delta() {
        let counter_mask = mbm_counter_mask(24);
        let bytes_per_second = bandwidth_bytes_per_second(
            Some(counter_mask - 9),
            Some(10),
            64.0,
            counter_mask,
            Duration::from_millis(100).as_secs_f64(),
        )
        .unwrap();

        assert_eq!(bytes_per_second, 12_800.0);
    }

    #[test]
    fn parses_shared_cpu_lists() {
        assert_eq!(parse_cpu_list("0-2,4,6-7\n"), Ok(vec![0, 1, 2, 4, 6, 7]));
        assert_eq!(parse_cpu_list("4,2,2,3\n"), Ok(vec![2, 3, 4]));
        assert_eq!(parse_cpu_list("\n"), Ok(vec![]));
        assert!(parse_cpu_list("3-1").is_err());
    }

    #[test]
    fn detects_resctrl_mount_points() {
        let mountinfo = "\
25 20 0:22 / /sys rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw
36 25 0:31 / /sys/fs/resctrl rw,relatime - resctrl resctrl rw
37 25 0:32 / /mnt/resctrl\\040test rw,relatime - resctrl resctrl rw
";

        assert_eq!(
            parse_resctrl_mount_points(mountinfo),
            vec![
                "/sys/fs/resctrl".to_string(),
                "/mnt/resctrl test".to_string()
            ]
        );
    }

    #[test]
    fn assigns_one_rdt_domain_per_core_inside_shared_l3() {
        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 0, 0, 0),
            test_topology(2, 0, 0, 0, 0, 0),
            test_topology(3, 0, 0, 0, 0, 0),
        ];

        let domains = rdt_domains_from_topologies(topologies, 8, |topology| {
            Ok(RdtCacheDomain::SharedCpuList(if topology.cpu < 2 {
                vec![0, 1]
            } else {
                vec![2, 3]
            }))
        })
        .unwrap();

        assert_eq!(domains.len(), 4);
        assert_eq!(domains[0].cpus, vec![0]);
        assert_eq!(domains[0].rmid, 1);
        assert_eq!(domains[0].scope.cpu, 0);
        assert_eq!(domains[1].cpus, vec![1]);
        assert_eq!(domains[1].rmid, 2);
        assert_eq!(domains[1].scope.cpu, 1);
        assert_eq!(domains[2].cpus, vec![2]);
        assert_eq!(domains[2].rmid, 3);
        assert_eq!(domains[2].scope.cpu, 2);
        assert_eq!(domains[3].cpus, vec![3]);
        assert_eq!(domains[3].rmid, 4);
        assert_eq!(domains[3].scope.cpu, 3);
    }

    #[test]
    fn groups_smt_siblings_by_core_inside_shared_l3() {
        let topologies = [
            test_topology_with_core(0, 0, 0, 0, 0, 0, 0, 0),
            test_topology_with_core(1, 0, 0, 0, 0, 0, 0, 1),
            test_topology_with_core(2, 0, 0, 0, 0, 0, 1, 2),
            test_topology_with_core(3, 0, 0, 0, 0, 0, 1, 3),
        ];

        let domains = rdt_domains_from_topologies(topologies, 8, |_| {
            Ok(RdtCacheDomain::SharedCpuList(vec![0, 1, 2, 3]))
        })
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].cpus, vec![0, 1]);
        assert_eq!(domains[0].scope.core_id, 0);
        assert_eq!(domains[0].scope.cpu, 0);
        assert_eq!(domains[1].cpus, vec![2, 3]);
        assert_eq!(domains[1].scope.core_id, 1);
        assert_eq!(domains[1].scope.cpu, 2);
    }

    #[test]
    fn keeps_repeated_local_core_ids_distinct_across_topology_levels() {
        let topologies = [
            test_topology_with_core(0, 0, 0, 0, 0, 0, 0, 0b0000),
            test_topology_with_core(1, 0, 0, 0, 1, 0, 0, 0b0100),
            test_topology_with_core(2, 0, 0, 0, 0, 1, 0, 0b1000),
        ];

        let domains = rdt_domains_from_topologies(topologies, 8, |_| {
            Ok(RdtCacheDomain::SharedCpuList(vec![0, 1, 2]))
        })
        .unwrap();

        assert_eq!(domains.len(), 3);
        assert_eq!(domains[0].cpus, vec![0]);
        assert_eq!(domains[0].scope.core_id, 0);
        assert_eq!(domains[1].cpus, vec![1]);
        assert_eq!(domains[1].scope.core_id, 2);
        assert_eq!(domains[2].cpus, vec![2]);
        assert_eq!(domains[2].scope.core_id, 4);
    }

    #[test]
    fn filters_shared_l3_cpu_list_to_current_scope() {
        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 1, 0, 0),
        ];

        let domains = rdt_domains_from_topologies(topologies, 8, |_| {
            Ok(RdtCacheDomain::SharedCpuList(vec![0, 1]))
        })
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].cpus, vec![0]);
        assert_eq!(domains[1].cpus, vec![1]);
    }

    #[test]
    fn keeps_die_domains_distinct_when_cache_key_matches() {
        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 1, 0, 0),
        ];

        let domains = rdt_domains_from_topologies(topologies, 8, |_| {
            Ok(RdtCacheDomain::SharedCpuList(vec![0, 1]))
        })
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].scope.die_id, Some(0));
        assert_eq!(domains[1].scope.die_id, Some(1));
    }

    #[test]
    fn translates_logical_rmid_to_snc_physical_rmid() {
        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 1, 0, 0),
        ];

        let domains = rdt_domains_from_topologies_with_numa(
            topologies,
            15,
            |_| Ok(RdtCacheDomain::SharedCpuList(vec![0, 1])),
            |cpu| Ok(Some(cpu)),
        )
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].rmid, 1);
        assert_eq!(domains[0].physical_rmid, 1);
        assert_eq!(domains[0].snc_nodes_per_l3_cache, 2);
        assert_eq!(domains[1].rmid, 2);
        assert_eq!(domains[1].physical_rmid, 10);
        assert_eq!(domains[1].snc_nodes_per_l3_cache, 2);
    }

    #[test]
    fn keeps_snc_nodes_distinct_when_scope_and_cache_key_match() {
        let topologies = [
            test_topology_with_core(0, 0, 0, 0, 0, 0, 0, 0),
            test_topology_with_core(1, 0, 0, 0, 0, 0, 0, 1),
        ];

        let domains = rdt_domains_from_topologies_with_numa(
            topologies,
            15,
            |_| Ok(RdtCacheDomain::SharedCpuList(vec![0, 1])),
            |cpu| Ok(Some(cpu)),
        )
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].cpus, vec![0]);
        assert_eq!(domains[0].rmid, 1);
        assert_eq!(domains[0].physical_rmid, 1);
        assert_eq!(domains[0].snc_nodes_per_l3_cache, 2);
        assert_eq!(domains[1].cpus, vec![1]);
        assert_eq!(domains[1].rmid, 2);
        assert_eq!(domains[1].physical_rmid, 10);
        assert_eq!(domains[1].snc_nodes_per_l3_cache, 2);
    }

    #[test]
    fn computes_snc_physical_rmid() {
        assert_eq!(physical_rmid(1, 0, 2, 15), Some(1));
        assert_eq!(physical_rmid(2, 1, 2, 15), Some(10));
        assert_eq!(physical_rmid(2, 3, 2, 15), Some(10));
        assert_eq!(physical_rmid(8, 0, 2, 15), None);
    }

    #[test]
    fn clears_snc_localized_distribution_mode() {
        assert_eq!(rmid_snc_sharing_mode_value(0), 0);
        assert_eq!(
            rmid_snc_sharing_mode_value(RMID_LOCALIZED_DISTRIBUTION_MODE_ENABLE),
            0
        );
        assert_eq!(
            rmid_snc_sharing_mode_value(0b101 | RMID_LOCALIZED_DISTRIBUTION_MODE_ENABLE),
            0b100
        );
    }

    #[test]
    fn scales_conversion_factor_by_snc_node_count() {
        let domain = RdtDomain {
            cpus: vec![0],
            original_pqr_assoc: Vec::new(),
            physical_rmid: 1,
            rmid: 1,
            scope: test_scope(),
            snc_nodes_per_l3_cache: 2,
        };

        assert_eq!(domain_conversion_factor_bytes(64, &domain), 32.0);
    }

    #[test]
    fn rejects_more_rdt_domains_than_nonzero_rmids() {
        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 0, 0, 0),
        ];

        let error = rdt_domains_from_topologies(topologies, 1, |topology| {
            Ok(RdtCacheDomain::SharedCpuList(vec![topology.cpu]))
        })
        .unwrap_err();

        assert!(error.contains("RDT monitoring exposes 1 physical RMIDs"));
    }

    #[test]
    fn derives_remote_bandwidth_from_total_and_local() {
        let previous = RdtScopeReading {
            conversion_factor_bytes: 64.0,
            counters: RdtCounters {
                local_memory_bandwidth: Some(5),
                total_memory_bandwidth: Some(10),
            },
            l3_occupancy_bytes: Some(128.0),
            scope: test_scope(),
        };
        let current = RdtScopeReading {
            conversion_factor_bytes: 64.0,
            counters: RdtCounters {
                local_memory_bandwidth: Some(8),
                total_memory_bandwidth: Some(20),
            },
            l3_occupancy_bytes: Some(256.0),
            scope: test_scope(),
        };

        let metrics = RdtScopeMetrics::from_readings(&previous, &current, 2.0, u64::MAX);

        assert_eq!(metrics.l3_occupancy_bytes, Some(256.0));
        assert_eq!(metrics.total_memory_bandwidth_bytes_per_second, Some(320.0));
        assert_eq!(metrics.local_memory_bandwidth_bytes_per_second, Some(96.0));
        assert_eq!(
            metrics.remote_memory_bandwidth_bytes_per_second,
            Some(224.0)
        );
    }

    #[test]
    fn uses_per_domain_conversion_factor_for_bandwidth() {
        let previous = RdtScopeReading {
            conversion_factor_bytes: 32.0,
            counters: RdtCounters {
                local_memory_bandwidth: Some(0),
                total_memory_bandwidth: Some(0),
            },
            l3_occupancy_bytes: None,
            scope: test_scope(),
        };
        let current = RdtScopeReading {
            conversion_factor_bytes: 32.0,
            counters: RdtCounters {
                local_memory_bandwidth: Some(4),
                total_memory_bandwidth: Some(10),
            },
            l3_occupancy_bytes: None,
            scope: test_scope(),
        };

        let metrics = RdtScopeMetrics::from_readings(&previous, &current, 2.0, u64::MAX);

        assert_eq!(metrics.total_memory_bandwidth_bytes_per_second, Some(160.0));
        assert_eq!(metrics.local_memory_bandwidth_bytes_per_second, Some(64.0));
        assert_eq!(metrics.remote_memory_bandwidth_bytes_per_second, Some(96.0));
    }

    #[test]
    fn reports_cmt_support_only_for_known_rdt_models() {
        let capabilities = RdtCapabilities {
            conversion_factor_bytes: 64,
            local_bandwidth: false,
            mbm_counter_mask: QM_CTR_DATA_MASK,
            max_rmid: 1023,
            occupancy: true,
            rmid_mask: 1023,
            total_bandwidth: false,
        };

        assert!(!supported_model_for_rdt_monitoring(
            IntelServerCpuModel::SandyBridgeEp,
            &capabilities
        ));
        assert!(!supported_model_for_rdt_monitoring(
            IntelServerCpuModel::IvyTown,
            &capabilities
        ));
        assert!(supported_model_for_rdt_monitoring(
            IntelServerCpuModel::HaswellXeon,
            &capabilities
        ));
        assert!(supported_model_for_rdt_monitoring(
            IntelServerCpuModel::SkylakeXeon,
            &capabilities
        ));
    }

    fn test_scope() -> RdtScope {
        RdtScope {
            core_id: 0,
            cpu: 0,
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }

    fn test_topology(
        cpu: u32,
        package_id: u32,
        die_group_id: u32,
        die_id: u32,
        module_id: u32,
        tile_id: u32,
    ) -> CpuTopology {
        let x2apic_id = cpu << 1;
        test_topology_with_core(
            cpu,
            package_id,
            die_group_id,
            die_id,
            module_id,
            tile_id,
            cpu,
            x2apic_id,
        )
    }

    fn test_topology_with_core(
        cpu: u32,
        package_id: u32,
        die_group_id: u32,
        die_id: u32,
        module_id: u32,
        tile_id: u32,
        core_id: u32,
        x2apic_id: u32,
    ) -> CpuTopology {
        CpuTopology {
            cpu,
            levels: vec![
                crate::metal::topology::TopologyLevel {
                    id: 0,
                    kind: TopologyLevelKind::Smt,
                    shift: 1,
                },
                crate::metal::topology::TopologyLevel {
                    id: core_id,
                    kind: TopologyLevelKind::Core,
                    shift: 2,
                },
                crate::metal::topology::TopologyLevel {
                    id: module_id,
                    kind: TopologyLevelKind::Module,
                    shift: 3,
                },
                crate::metal::topology::TopologyLevel {
                    id: tile_id,
                    kind: TopologyLevelKind::Tile,
                    shift: 4,
                },
                crate::metal::topology::TopologyLevel {
                    id: die_id,
                    kind: TopologyLevelKind::Die,
                    shift: 5,
                },
                crate::metal::topology::TopologyLevel {
                    id: die_group_id,
                    kind: TopologyLevelKind::DieGroup,
                    shift: 6,
                },
                crate::metal::topology::TopologyLevel {
                    id: package_id,
                    kind: TopologyLevelKind::Package,
                    shift: 6,
                },
            ],
            x2apic_id,
        }
    }
}
