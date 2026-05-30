use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
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
const RESCTRL_INFO_L3_MON: &str = "info/L3_MON";
const RESCTRL_INFO_PERF_PKG_MON: &str = "info/PERF_PKG_MON";
const RESCTRL_L3_OCCUPANCY: &str = "llc_occupancy";
const RESCTRL_MBM_TOTAL: &str = "mbm_total_bytes";
const RESCTRL_MBM_LOCAL: &str = "mbm_local_bytes";
const RESCTRL_MON_GROUP_PREFIX: &str = "ocellus-";
const RESCTRL_NUM_RMIDS: &str = "num_rmids";

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
    backend: RdtBackend,
}

#[derive(Debug)]
enum RdtBackend {
    Msr(MsrRdtCollector),
    Resctrl(ResctrlRdtCollector),
}

impl RdtCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let capabilities = supported_rdt_capabilities(architecture)?;

        if let Some(mount_point) = resctrl_mount_point()? {
            Ok(Self {
                backend: RdtBackend::Resctrl(ResctrlRdtCollector::new(mount_point)?),
            })
        } else {
            Ok(Self {
                backend: RdtBackend::Msr(MsrRdtCollector::new(capabilities)?),
            })
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        supported_rdt_capabilities(architecture).is_ok()
            && match resctrl_mount_point() {
                Ok(Some(mount_point)) => ResctrlRdtCollector::is_supported(&mount_point),
                Ok(None) => true,
                Err(_) => false,
            }
    }

    pub fn sample(&mut self) -> Result<Option<RdtMetrics>, String> {
        match &mut self.backend {
            RdtBackend::Msr(collector) => collector.sample(),
            RdtBackend::Resctrl(collector) => collector.sample(),
        }
    }
}

#[derive(Debug)]
struct MsrRdtCollector {
    capabilities: RdtCapabilities,
    domains: Vec<RdtDomain>,
    original_rmid_snc_configs: Vec<RdtSavedMsr>,
    previous: Option<RdtReading>,
    restore_msr_state: bool,
}

impl MsrRdtCollector {
    fn new(capabilities: RdtCapabilities) -> Result<Self, String> {
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

    fn sample(&mut self) -> Result<Option<RdtMetrics>, String> {
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

impl Drop for MsrRdtCollector {
    fn drop(&mut self) {
        if self.restore_msr_state && !resctrl_is_mounted().unwrap_or(true) {
            restore_domains(&self.domains);
            restore_snc_configs(&self.original_rmid_snc_configs);
        }
    }
}

#[derive(Debug)]
struct ResctrlRdtCollector {
    features: ResctrlMonitorFeatures,
    groups: Vec<ResctrlMonitorGroup>,
    previous: Option<RdtReading>,
}

impl ResctrlRdtCollector {
    fn new(mount_point: PathBuf) -> Result<Self, String> {
        let features = ResctrlMonitorFeatures::from_resctrl(&mount_point)?;
        if !features.occupancy {
            return Err(format!(
                "Linux resctrl at {} does not expose {RESCTRL_L3_OCCUPANCY}",
                mount_point.display()
            ));
        }

        let domains = resctrl_rdt_domains()?;
        let groups = create_resctrl_monitor_groups(&mount_point, &domains, &features)?;
        if groups.is_empty() {
            return Err(format!(
                "Linux resctrl at {} has no RDT domains available for Ocellus monitor groups",
                mount_point.display()
            ));
        }

        Ok(Self {
            features,
            groups,
            previous: None,
        })
    }

    fn is_supported(mount_point: &Path) -> bool {
        ResctrlMonitorFeatures::from_resctrl(mount_point).is_ok_and(|features| features.occupancy)
    }

    fn read(&self) -> Result<RdtReading, String> {
        let mut scopes = Vec::with_capacity(self.groups.len());
        for group in &self.groups {
            scopes.push(read_resctrl_monitor_group(group, self.features)?);
        }

        Ok(RdtReading {
            at: Instant::now(),
            scopes,
        })
    }

    fn sample(&mut self) -> Result<Option<RdtMetrics>, String> {
        let current = self.read()?;
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(None),
        };

        Ok(Some(RdtMetrics::from_readings(
            previous,
            current,
            u64::MAX,
        )?))
    }
}

impl Drop for ResctrlRdtCollector {
    fn drop(&mut self) {
        for group in &self.groups {
            let _ = remove_resctrl_monitor_group(&group.path);
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

fn supported_rdt_capabilities(architecture: &Architecture) -> Result<RdtCapabilities, String> {
    let model = architecture.intel_server_model();
    rdt_capabilities()
        .filter(|capabilities| supported_model_for_rdt_monitoring(model, capabilities))
        .ok_or_else(|| {
            format!(
                "RDT monitoring is not supported by this processor model or CPUID capabilities: {:?}",
                model
            )
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

#[derive(Clone, Copy, Debug)]
struct ResctrlMonitorFeatures {
    local_bandwidth: bool,
    num_rmids: usize,
    occupancy: bool,
    total_bandwidth: bool,
}

impl ResctrlMonitorFeatures {
    fn from_resctrl(mount_point: &Path) -> Result<Self, String> {
        let l3_mon_path = mount_point.join(RESCTRL_INFO_L3_MON);
        let mon_features_path = mount_point.join(RESCTRL_INFO_L3_MON).join("mon_features");
        let mon_features = read_optional_string(&mon_features_path)?.ok_or_else(|| {
            format!(
                "Linux resctrl at {} is missing L3 monitoring features",
                mount_point.display()
            )
        })?;
        let mut num_rmids = read_resctrl_num_rmids(&l3_mon_path)?;
        if let Some(perf_pkg_num_rmids) =
            read_optional_resctrl_num_rmids(&mount_point.join(RESCTRL_INFO_PERF_PKG_MON))?
        {
            num_rmids = num_rmids.min(perf_pkg_num_rmids);
        }

        Ok(Self::parse_with_num_rmids(&mon_features, num_rmids))
    }

    #[cfg(test)]
    fn parse(mon_features: &str) -> Self {
        Self::parse_with_num_rmids(mon_features, usize::MAX)
    }

    fn parse_with_num_rmids(mon_features: &str, num_rmids: usize) -> Self {
        let features = mon_features.split_whitespace().collect::<BTreeSet<_>>();

        Self {
            local_bandwidth: features.contains(RESCTRL_MBM_LOCAL),
            num_rmids,
            occupancy: features.contains(RESCTRL_L3_OCCUPANCY),
            total_bandwidth: features.contains(RESCTRL_MBM_TOTAL),
        }
    }
}

#[derive(Debug)]
struct ResctrlMonitorGroup {
    l3_domains: Vec<String>,
    path: PathBuf,
    scope: RdtScope,
}

#[derive(Clone, Debug)]
struct ResctrlL3Domain {
    cache_id: u32,
    name: String,
    sub_node_id: Option<u32>,
}

#[derive(Clone, Debug)]
struct ResctrlControlGroup {
    cpus: Vec<u32>,
    path: PathBuf,
    tasks_assigned: bool,
}

#[derive(Debug)]
struct ResctrlMonitorAssignment {
    cpus: Vec<u32>,
    path: PathBuf,
    tasks_assigned: bool,
}

fn create_resctrl_monitor_groups(
    mount_point: &Path,
    domains: &[RdtDomain],
    features: &ResctrlMonitorFeatures,
) -> Result<Vec<ResctrlMonitorGroup>, String> {
    create_resctrl_monitor_groups_with_l3_domain_mapper(
        mount_point,
        domains,
        features,
        resctrl_l3_domains_for_cpus,
    )
}

fn create_resctrl_monitor_groups_with_l3_domain_mapper(
    mount_point: &Path,
    domains: &[RdtDomain],
    features: &ResctrlMonitorFeatures,
    mut l3_domains_for_cpus: impl FnMut(&[u32], &[String]) -> Result<Vec<String>, String>,
) -> Result<Vec<ResctrlMonitorGroup>, String> {
    let l3_domains = discover_resctrl_l3_domains(mount_point)?;
    if l3_domains.is_empty() {
        return Err(format!(
            "Linux resctrl at {} has no L3 monitor domains",
            mount_point.display()
        ));
    }

    let group_names = domains
        .iter()
        .map(resctrl_monitor_group_name)
        .collect::<BTreeSet<_>>();

    let control_groups = discover_resctrl_control_groups(mount_point)?;
    reject_task_assigned_resctrl_control_groups(mount_point, &control_groups)?;
    let monitor_assignments = discover_resctrl_monitor_assignments(mount_point)?;
    reject_task_assigned_resctrl_monitor_groups(&monitor_assignments)?;
    remove_stale_ocellus_groups_by_name(mount_point, &group_names)?;
    let monitor_assignments = discover_resctrl_monitor_assignments(mount_point)?;
    reject_task_assigned_resctrl_monitor_groups(&monitor_assignments)?;
    let free_rmids =
        free_resctrl_monitor_rmids(features.num_rmids, &control_groups, &monitor_assignments);

    let mut groups = Vec::new();
    let mut created_paths = Vec::new();
    let mut conflicts = Vec::new();
    let mut capacity_skips = Vec::new();
    for (domain_index, domain) in domains.iter().enumerate() {
        if let Some(conflict) = conflicting_resctrl_monitor_assignment(domain, &monitor_assignments)
        {
            conflicts.push(format!(
                "CPU(s) {} already assigned to {}",
                format_cpu_list(&domain.cpus),
                conflict.display()
            ));
            continue;
        }

        if groups.len() >= free_rmids {
            capacity_skips.extend(
                domains[domain_index..]
                    .iter()
                    .map(|domain| format_cpu_list(&domain.cpus)),
            );
            break;
        }

        let group_l3_domains = match l3_domains_for_cpus(&domain.cpus, &l3_domains) {
            Ok(group_l3_domains) if !group_l3_domains.is_empty() => group_l3_domains,
            Ok(_) => {
                cleanup_resctrl_monitor_groups(&created_paths);
                return Err(format!(
                    "failed to map CPU(s) {} to any Linux resctrl L3 monitor domain",
                    format_cpu_list(&domain.cpus)
                ));
            }
            Err(error) => {
                cleanup_resctrl_monitor_groups(&created_paths);
                return Err(error);
            }
        };

        let control_group = match resctrl_control_group_for_domain(domain, &control_groups) {
            Ok(control_group) => control_group,
            Err(error) => {
                cleanup_resctrl_monitor_groups(&created_paths);
                return Err(error);
            }
        };
        let group_name = resctrl_monitor_group_name(domain);
        let group_path = control_group.path.join("mon_groups").join(&group_name);
        if let Err(error) = std::fs::create_dir(&group_path) {
            if resctrl_monitor_rmid_exhausted(&error) {
                capacity_skips.extend(
                    domains[domain_index..]
                        .iter()
                        .map(|domain| format_cpu_list(&domain.cpus)),
                );
                break;
            }
            cleanup_resctrl_monitor_groups(&created_paths);
            return Err(format!(
                "failed to create {}: {error}",
                group_path.display()
            ));
        }
        created_paths.push(group_path.clone());
        let cpu_list = format_cpu_list(&domain.cpus);
        if let Err(error) = std::fs::write(group_path.join("cpus_list"), format!("{cpu_list}\n")) {
            cleanup_resctrl_monitor_groups(&created_paths);
            return Err(format!(
                "failed to assign CPUs {cpu_list} to {}: {error}",
                group_path.display()
            ));
        }

        groups.push(ResctrlMonitorGroup {
            l3_domains: group_l3_domains,
            path: group_path,
            scope: domain.scope,
        });
    }

    if groups.is_empty() && !capacity_skips.is_empty() {
        return Err(format!(
            "all RDT domains require new resctrl monitor groups, but Linux resctrl has no free monitor RMIDs: {} available, {} occupied by control/monitor groups",
            features.num_rmids,
            occupied_resctrl_monitor_rmids(&control_groups, &monitor_assignments)
        ));
    }

    if groups.is_empty() && !conflicts.is_empty() {
        return Err(format!(
            "all RDT domains conflict with existing resctrl monitor groups: {}",
            conflicts.join("; ")
        ));
    }

    if !conflicts.is_empty() {
        eprintln!(
            "ocellus: skipping {} RDT domain(s) with existing resctrl monitor assignments: {}",
            conflicts.len(),
            conflicts.join("; ")
        );
    }

    if !capacity_skips.is_empty() {
        eprintln!(
            "ocellus: skipping {} RDT domain(s) because Linux resctrl has only {free_rmids} free monitor RMID(s): CPU(s) {}",
            capacity_skips.len(),
            capacity_skips.join("; ")
        );
    }

    if features.total_bandwidth && !features.local_bandwidth {
        eprintln!(
            "ocellus: Linux resctrl exposes {RESCTRL_MBM_TOTAL} but not {RESCTRL_MBM_LOCAL}; remote RDT bandwidth will be omitted"
        );
    }

    Ok(groups)
}

fn read_resctrl_monitor_group(
    group: &ResctrlMonitorGroup,
    features: ResctrlMonitorFeatures,
) -> Result<RdtScopeReading, String> {
    if !group.path.is_dir() {
        return Err(format!(
            "Ocellus resctrl monitor group {} disappeared",
            group.path.display()
        ));
    }

    let l3_occupancy_bytes = if features.occupancy {
        sum_resctrl_counters(group, RESCTRL_L3_OCCUPANCY)?.map(|value| value as f64)
    } else {
        None
    };
    let total_memory_bandwidth = if features.total_bandwidth {
        sum_resctrl_counters(group, RESCTRL_MBM_TOTAL)?
    } else {
        None
    };
    let local_memory_bandwidth = if features.local_bandwidth {
        sum_resctrl_counters(group, RESCTRL_MBM_LOCAL)?
    } else {
        None
    };

    Ok(RdtScopeReading {
        conversion_factor_bytes: 1.0,
        counters: RdtCounters {
            local_memory_bandwidth,
            total_memory_bandwidth,
        },
        l3_occupancy_bytes,
        scope: group.scope,
    })
}

fn sum_resctrl_counters(group: &ResctrlMonitorGroup, counter: &str) -> Result<Option<u64>, String> {
    if group.l3_domains.is_empty() {
        return Ok(None);
    }

    let mut total = 0_u64;
    for l3_domain in &group.l3_domains {
        let counter_path = group.path.join("mon_data").join(l3_domain).join(counter);
        let Some(value) = read_resctrl_counter(&counter_path)? else {
            return Ok(None);
        };
        total = total.wrapping_add(value);
    }

    Ok(Some(total))
}

fn read_resctrl_counter(path: &Path) -> Result<Option<u64>, String> {
    let Some(value) = read_optional_string(path)? else {
        return Ok(None);
    };

    parse_resctrl_counter_value(&value)
        .map_err(|error| format!("failed to parse {} value: {error}", path.display()))
}

fn parse_resctrl_counter_value(value: &str) -> Result<Option<u64>, String> {
    let value = value.trim();
    if ["unavailable", "unassigned", "error"]
        .iter()
        .any(|status| value.eq_ignore_ascii_case(status))
    {
        return Ok(None);
    }

    value
        .parse::<u64>()
        .map(Some)
        .map_err(|error| format!("{value:?}: {error}"))
}

fn read_resctrl_num_rmids(info_path: &Path) -> Result<usize, String> {
    let num_rmids_path = info_path.join(RESCTRL_NUM_RMIDS);
    let value = read_optional_string(&num_rmids_path)?.ok_or_else(|| {
        format!(
            "Linux resctrl at {} is missing {RESCTRL_NUM_RMIDS}",
            info_path.display()
        )
    })?;

    parse_resctrl_num_rmids(&value).map_err(|error| {
        format!(
            "failed to parse {} value: {error}",
            num_rmids_path.display()
        )
    })
}

fn read_optional_resctrl_num_rmids(info_path: &Path) -> Result<Option<usize>, String> {
    let num_rmids_path = info_path.join(RESCTRL_NUM_RMIDS);
    let Some(value) = read_optional_string(&num_rmids_path)? else {
        return Ok(None);
    };

    parse_resctrl_num_rmids(&value).map(Some).map_err(|error| {
        format!(
            "failed to parse {} value: {error}",
            num_rmids_path.display()
        )
    })
}

fn parse_resctrl_num_rmids(value: &str) -> Result<usize, String> {
    let value = value.trim();
    let num_rmids = value
        .parse::<usize>()
        .map_err(|error| format!("{value:?}: {error}"))?;
    if num_rmids == 0 {
        return Err("Linux resctrl reports zero monitor RMIDs".to_string());
    }

    Ok(num_rmids)
}

fn discover_resctrl_control_groups(mount_point: &Path) -> Result<Vec<ResctrlControlGroup>, String> {
    let mut groups = Vec::new();
    discover_resctrl_control_groups_recursive(mount_point, &mut groups)?;
    groups.sort_by(|left, right| {
        left.cpus
            .len()
            .cmp(&right.cpus.len())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(groups)
}

fn discover_resctrl_control_groups_recursive(
    path: &Path,
    groups: &mut Vec<ResctrlControlGroup>,
) -> Result<(), String> {
    if let Some(cpus) = read_optional_cpu_list(&path.join("cpus_list"))? {
        let tasks_assigned = resctrl_tasks_assigned(&path.join("tasks"))?;
        groups.push(ResctrlControlGroup {
            cpus,
            path: path.to_path_buf(),
            tasks_assigned,
        });
    }

    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to read {} entry: {error}", path.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read {} file type: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "info" || name == "mon_groups" || name == "mon_data" {
            continue;
        }

        discover_resctrl_control_groups_recursive(&entry.path(), groups)?;
    }

    Ok(())
}

fn discover_resctrl_monitor_assignments(
    mount_point: &Path,
) -> Result<Vec<ResctrlMonitorAssignment>, String> {
    let mut assignments = Vec::new();
    discover_resctrl_monitor_assignments_recursive(mount_point, &mut assignments)?;
    Ok(assignments)
}

fn discover_resctrl_monitor_assignments_recursive(
    path: &Path,
    assignments: &mut Vec<ResctrlMonitorAssignment>,
) -> Result<(), String> {
    let mon_groups = path.join("mon_groups");
    if mon_groups.is_dir() {
        let entries = std::fs::read_dir(&mon_groups)
            .map_err(|error| format!("failed to read {}: {error}", mon_groups.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("failed to read {} entry: {error}", mon_groups.display())
            })?;
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "failed to read {} file type: {error}",
                    entry.path().display()
                )
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let group_path = entry.path();
            let cpus = read_optional_cpu_list(&group_path.join("cpus_list"))?.unwrap_or_default();
            let tasks_assigned = resctrl_tasks_assigned(&group_path.join("tasks"))?;
            assignments.push(ResctrlMonitorAssignment {
                cpus,
                path: group_path,
                tasks_assigned,
            });
        }
    }

    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to read {} entry: {error}", path.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read {} file type: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "info" || name == "mon_groups" || name == "mon_data" {
            continue;
        }

        discover_resctrl_monitor_assignments_recursive(&entry.path(), assignments)?;
    }

    Ok(())
}

fn discover_resctrl_l3_domains(mount_point: &Path) -> Result<Vec<String>, String> {
    let mon_data = mount_point.join("mon_data");
    let entries = std::fs::read_dir(&mon_data)
        .map_err(|error| format!("failed to read {}: {error}", mon_data.display()))?;
    let mut domains = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("failed to read {} entry: {error}", mon_data.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read {} file type: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("mon_L3_") {
            let subdomains = discover_resctrl_l3_subdomains(&entry.path(), name)?;
            if subdomains.is_empty() {
                domains.push(name.to_string());
            } else {
                domains.extend(subdomains);
            }
        }
    }

    domains.sort();
    Ok(domains)
}

fn discover_resctrl_l3_subdomains(l3_path: &Path, l3_name: &str) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(l3_path)
        .map_err(|error| format!("failed to read {}: {error}", l3_path.display()))?;
    let mut subdomains = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("failed to read {} entry: {error}", l3_path.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read {} file type: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("mon_sub_L3_") {
            subdomains.push(format!("{l3_name}/{name}"));
        }
    }

    subdomains.sort();
    Ok(subdomains)
}

fn resctrl_l3_domains_for_cpus(cpus: &[u32], available: &[String]) -> Result<Vec<String>, String> {
    resctrl_l3_domains_for_cpus_with_topology(cpus, available, resctrl_l3_cache_id, cpu_numa_node)
}

fn resctrl_l3_domains_for_cpus_with_topology(
    cpus: &[u32],
    available: &[String],
    mut l3_cache_id: impl FnMut(u32) -> Result<u32, String>,
    mut numa_node: impl FnMut(u32) -> Result<Option<u32>, String>,
) -> Result<Vec<String>, String> {
    let domains_by_id = resctrl_l3_domains_by_id(available)?;
    let mut domains = BTreeSet::new();
    for cpu in cpus {
        let cache_id = l3_cache_id(*cpu)?;
        let candidates = domains_by_id.get(&cache_id).ok_or_else(|| {
            format!(
                "CPU {cpu} belongs to L3 cache id {cache_id}, but Linux resctrl exposes L3 monitor domains {}",
                available.join(", ")
            )
        })?;
        let domain =
            resctrl_l3_domain_for_cpu(*cpu, cache_id, candidates, available, &mut numa_node)?;
        domains.insert(domain.name.clone());
    }

    Ok(domains.into_iter().collect())
}

fn resctrl_l3_domain_for_cpu<'a>(
    cpu: u32,
    cache_id: u32,
    candidates: &'a [ResctrlL3Domain],
    available: &[String],
    numa_node: &mut impl FnMut(u32) -> Result<Option<u32>, String>,
) -> Result<&'a ResctrlL3Domain, String> {
    if candidates.iter().any(|domain| domain.sub_node_id.is_some()) {
        let Some(node_id) = numa_node(cpu)? else {
            return Err(format!(
                "CPU {cpu} belongs to L3 cache id {cache_id}, but Linux resctrl exposes SNC L3 monitor domains {} and the CPU NUMA node is unavailable",
                available.join(", ")
            ));
        };
        return candidates
            .iter()
            .find(|domain| domain.sub_node_id == Some(node_id))
            .ok_or_else(|| {
                format!(
                    "CPU {cpu} belongs to L3 cache id {cache_id} and NUMA node {node_id}, but Linux resctrl exposes L3 monitor domains {}",
                    available.join(", ")
                )
            });
    }

    candidates.first().ok_or_else(|| {
        format!(
            "CPU {cpu} belongs to L3 cache id {cache_id}, but Linux resctrl exposes no matching L3 monitor domains"
        )
    })
}

fn resctrl_l3_domains_by_id(
    available: &[String],
) -> Result<BTreeMap<u32, Vec<ResctrlL3Domain>>, String> {
    let mut domains = BTreeMap::new();
    for domain in available {
        let domain = parse_resctrl_l3_domain(domain)?;
        let entry: &mut Vec<ResctrlL3Domain> = domains.entry(domain.cache_id).or_default();
        if let Some(existing) = entry.iter().find(|existing| {
            existing.sub_node_id == domain.sub_node_id
                || (existing.sub_node_id.is_none() && domain.sub_node_id.is_none())
        }) {
            match domain.sub_node_id {
                Some(node_id) => {
                    return Err(format!(
                        "Linux resctrl exposes duplicate L3 monitor domain id {} and SNC node id {node_id}: {}, {}",
                        domain.cache_id, existing.name, domain.name
                    ));
                }
                None => {
                    return Err(format!(
                        "Linux resctrl exposes duplicate L3 monitor domain id {}: {}, {}",
                        domain.cache_id, existing.name, domain.name
                    ));
                }
            }
        }
        entry.push(domain);
    }

    Ok(domains)
}

fn parse_resctrl_l3_domain(domain: &str) -> Result<ResctrlL3Domain, String> {
    let Some((l3_domain, sub_domain)) = domain.split_once('/') else {
        return Ok(ResctrlL3Domain {
            cache_id: parse_resctrl_l3_domain_id(domain)?,
            name: domain.to_string(),
            sub_node_id: None,
        });
    };
    if sub_domain.contains('/') {
        return Err(format!(
            "invalid Linux resctrl L3 monitor domain {domain:?}"
        ));
    }

    Ok(ResctrlL3Domain {
        cache_id: parse_resctrl_l3_domain_id(l3_domain)?,
        name: domain.to_string(),
        sub_node_id: Some(parse_resctrl_l3_subdomain_id(sub_domain)?),
    })
}

fn parse_resctrl_l3_domain_id(domain: &str) -> Result<u32, String> {
    let Some(id) = domain.strip_prefix("mon_L3_") else {
        return Err(format!(
            "invalid Linux resctrl L3 monitor domain {domain:?}"
        ));
    };

    id.parse::<u32>()
        .or_else(|_| u32::from_str_radix(id, 16))
        .map_err(|error| format!("invalid Linux resctrl L3 monitor domain {domain:?}: {error}"))
}

fn parse_resctrl_l3_subdomain_id(domain: &str) -> Result<u32, String> {
    let Some(id) = domain.strip_prefix("mon_sub_L3_") else {
        return Err(format!(
            "invalid Linux resctrl L3 monitor subdomain {domain:?}"
        ));
    };

    id.parse::<u32>()
        .or_else(|_| u32::from_str_radix(id, 16))
        .map_err(|error| format!("invalid Linux resctrl L3 monitor subdomain {domain:?}: {error}"))
}

fn resctrl_l3_cache_id(cpu: u32) -> Result<u32, String> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index3/id");
    let value = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {path}: {error}"))?;
    let id = value
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("failed to parse {path} value {value:?}: {error}"))?;

    Ok(id)
}

#[cfg(test)]
fn resctrl_l3_domain_name(id: u32) -> String {
    format!("mon_L3_{id:02}")
}

fn reject_task_assigned_resctrl_control_groups(
    mount_point: &Path,
    groups: &[ResctrlControlGroup],
) -> Result<(), String> {
    let groups = groups
        .iter()
        .filter(|group| group.tasks_assigned && group.path != mount_point)
        .map(|group| group.path.display().to_string())
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(());
    }

    Err(format!(
        "existing non-root resctrl control group(s) have task assignments; refusing CPU-based RDT monitoring because control-group task RMIDs take precedence over CPU monitor groups: {}",
        groups.join(", ")
    ))
}

fn reject_task_assigned_resctrl_monitor_groups(
    assignments: &[ResctrlMonitorAssignment],
) -> Result<(), String> {
    let groups = assignments
        .iter()
        .filter(|assignment| assignment.tasks_assigned)
        .map(|assignment| assignment.path.display().to_string())
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(());
    }

    Err(format!(
        "existing resctrl monitor group(s) have task assignments; refusing CPU-based RDT monitoring because task monitor groups take precedence over CPU monitor groups: {}",
        groups.join(", ")
    ))
}

fn conflicting_resctrl_monitor_assignment<'a>(
    domain: &RdtDomain,
    assignments: &'a [ResctrlMonitorAssignment],
) -> Option<&'a Path> {
    assignments
        .iter()
        .find(|assignment| cpu_lists_overlap(&domain.cpus, &assignment.cpus))
        .map(|assignment| assignment.path.as_path())
}

fn free_resctrl_monitor_rmids(
    num_rmids: usize,
    control_groups: &[ResctrlControlGroup],
    monitor_assignments: &[ResctrlMonitorAssignment],
) -> usize {
    num_rmids.saturating_sub(occupied_resctrl_monitor_rmids(
        control_groups,
        monitor_assignments,
    ))
}

fn occupied_resctrl_monitor_rmids(
    control_groups: &[ResctrlControlGroup],
    monitor_assignments: &[ResctrlMonitorAssignment],
) -> usize {
    control_groups
        .len()
        .saturating_add(monitor_assignments.len())
}

fn resctrl_monitor_rmid_exhausted(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == nix::errno::Errno::ENOSPC as i32
                || code == nix::errno::Errno::EBUSY as i32
    )
}

fn resctrl_control_group_for_domain<'a>(
    domain: &RdtDomain,
    groups: &'a [ResctrlControlGroup],
) -> Result<&'a ResctrlControlGroup, String> {
    groups
        .iter()
        .find(|group| cpus_are_subset(&domain.cpus, &group.cpus))
        .ok_or_else(|| {
            format!(
                "failed to find resctrl control group for CPU(s) {}",
                format_cpu_list(&domain.cpus)
            )
        })
}

fn remove_stale_ocellus_groups_by_name(
    path: &Path,
    group_names: &BTreeSet<String>,
) -> Result<(), String> {
    let mon_groups = path.join("mon_groups");
    if mon_groups.is_dir() {
        let entries = std::fs::read_dir(&mon_groups)
            .map_err(|error| format!("failed to read {}: {error}", mon_groups.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("failed to read {} entry: {error}", mon_groups.display())
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !group_names.contains(name) {
                continue;
            }
            let group_path = entry.path();
            let cpus = read_optional_cpu_list(&group_path.join("cpus_list"))?.unwrap_or_default();
            let tasks_assigned = resctrl_tasks_assigned(&group_path.join("tasks"))?;
            if !cpus.is_empty() || tasks_assigned {
                continue;
            }
            match remove_resctrl_monitor_group(&group_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to remove stale Ocellus resctrl monitor group {}: {error}",
                        group_path.display()
                    ));
                }
            }
        }
    }

    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to read {} entry: {error}", path.display()))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read {} file type: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "info" || name == "mon_groups" || name == "mon_data" {
            continue;
        }

        remove_stale_ocellus_groups_by_name(&entry.path(), group_names)?;
    }

    Ok(())
}

fn cleanup_resctrl_monitor_groups(paths: &[PathBuf]) {
    for path in paths.iter().rev() {
        let _ = remove_resctrl_monitor_group(path);
    }
}

fn remove_resctrl_monitor_group(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                std::fs::remove_dir_all(path)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(not(test))]
    {
        std::fs::remove_dir(path)
    }
}

fn resctrl_monitor_group_name(domain: &RdtDomain) -> String {
    format!(
        "{RESCTRL_MON_GROUP_PREFIX}p{}-c{}-cpu{}",
        domain.scope.package_id, domain.scope.core_id, domain.scope.cpu
    )
}

fn read_optional_cpu_list(path: &Path) -> Result<Option<Vec<u32>>, String> {
    read_optional_string(path)?
        .map(|value| {
            parse_cpu_list(&value).map_err(|error| {
                format!(
                    "failed to parse {} value {value:?}: {error}",
                    path.display()
                )
            })
        })
        .transpose()
}

fn resctrl_tasks_assigned(path: &Path) -> Result<bool, String> {
    Ok(read_optional_string(path)?
        .as_deref()
        .is_some_and(|value| value.split_whitespace().next().is_some()))
}

fn read_optional_string(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

fn cpus_are_subset(cpus: &[u32], possible_superset: &[u32]) -> bool {
    cpus.iter().all(|cpu| possible_superset.contains(cpu))
}

fn cpu_lists_overlap(left: &[u32], right: &[u32]) -> bool {
    left.iter().any(|cpu| right.contains(cpu))
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

fn resctrl_rdt_domains() -> Result<Vec<RdtDomain>, String> {
    rdt_domains_from_topologies_with_optional_rmid(
        metal::topology::cpu_topologies()?,
        None,
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

#[cfg(test)]
fn resctrl_rdt_domains_from_topologies_with_numa(
    topologies: impl IntoIterator<Item = CpuTopology>,
    cache_domain: impl FnMut(&CpuTopology) -> Result<RdtCacheDomain, String>,
    numa_node: impl FnMut(u32) -> Result<Option<u32>, String>,
) -> Result<Vec<RdtDomain>, String> {
    rdt_domains_from_topologies_with_optional_rmid(topologies, None, cache_domain, numa_node)
}

fn rdt_domains_from_topologies_with_numa(
    topologies: impl IntoIterator<Item = CpuTopology>,
    max_rmid: u32,
    mut cache_domain: impl FnMut(&CpuTopology) -> Result<RdtCacheDomain, String>,
    mut numa_node: impl FnMut(u32) -> Result<Option<u32>, String>,
) -> Result<Vec<RdtDomain>, String> {
    rdt_domains_from_topologies_with_optional_rmid(
        topologies,
        Some(max_rmid),
        &mut cache_domain,
        &mut numa_node,
    )
}

fn rdt_domains_from_topologies_with_optional_rmid(
    topologies: impl IntoIterator<Item = CpuTopology>,
    max_rmid: Option<u32>,
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
        let physical_rmid = match max_rmid {
            Some(max_rmid) => {
                physical_rmid(rmid, builder.node_id, snc_nodes_per_l3_cache, max_rmid).ok_or_else(
                    || {
                        format!(
                            "RDT monitoring exposes {max_rmid} physical RMIDs across {snc_nodes_per_l3_cache} SNC nodes, but {} RDT monitoring domains were discovered",
                            index + 1
                        )
                    },
                )?
            }
            None => rmid,
        };

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

fn format_cpu_list(cpus: &[u32]) -> String {
    if cpus.is_empty() {
        return String::new();
    }

    let mut cpus = cpus.to_vec();
    cpus.sort_unstable();
    cpus.dedup();

    let mut ranges = Vec::new();
    let mut start = cpus[0];
    let mut previous = cpus[0];
    for cpu in cpus.into_iter().skip(1) {
        if cpu == previous + 1 {
            previous = cpu;
            continue;
        }

        ranges.push(format_cpu_range(start, previous));
        start = cpu;
        previous = cpu;
    }
    ranges.push(format_cpu_range(start, previous));

    ranges.join(",")
}

fn format_cpu_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
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

fn resctrl_mount_point() -> Result<Option<PathBuf>, String> {
    Ok(resctrl_mount_points()?
        .into_iter()
        .next()
        .map(PathBuf::from))
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
    fn formats_cpu_lists() {
        assert_eq!(format_cpu_list(&[]), "");
        assert_eq!(format_cpu_list(&[0, 1, 2, 4, 6, 7]), "0-2,4,6-7");
        assert_eq!(format_cpu_list(&[4, 2, 3, 2]), "2-4");
    }

    #[test]
    fn parses_resctrl_l3_domain_ids() {
        assert_eq!(parse_resctrl_l3_domain_id("mon_L3_00"), Ok(0));
        assert_eq!(parse_resctrl_l3_domain_id("mon_L3_10"), Ok(10));
        assert!(parse_resctrl_l3_domain_id("mon_L2_00").is_err());
        assert_eq!(parse_resctrl_l3_subdomain_id("mon_sub_L3_00"), Ok(0));
        assert_eq!(
            parse_resctrl_l3_domain("mon_L3_02/mon_sub_L3_03")
                .unwrap()
                .sub_node_id,
            Some(3)
        );
    }

    #[test]
    fn parses_resctrl_monitor_features() {
        let features =
            ResctrlMonitorFeatures::parse("llc_occupancy\nmbm_total_bytes\nmbm_local_bytes\n");

        assert!(features.occupancy);
        assert!(features.total_bandwidth);
        assert!(features.local_bandwidth);

        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");

        assert!(features.occupancy);
        assert!(!features.total_bandwidth);
        assert!(!features.local_bandwidth);
    }

    #[test]
    fn parses_resctrl_num_rmids() {
        assert_eq!(parse_resctrl_num_rmids("7\n"), Ok(7));
        assert!(parse_resctrl_num_rmids("0\n").is_err());
        assert!(parse_resctrl_num_rmids("not-a-number\n").is_err());
    }

    #[test]
    fn reads_resctrl_monitor_features_and_rmid_capacity() {
        let root = unique_test_dir("resctrl-rdt-features");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("info/PERF_PKG_MON")).unwrap();
        std::fs::write(
            root.join("info/L3_MON/mon_features"),
            "llc_occupancy\nmbm_total_bytes\nmbm_local_bytes\n",
        )
        .unwrap();
        std::fs::write(root.join("info/L3_MON/num_rmids"), "8\n").unwrap();
        std::fs::write(root.join("info/PERF_PKG_MON/num_rmids"), "6\n").unwrap();

        let features = ResctrlMonitorFeatures::from_resctrl(&root).unwrap();

        assert!(features.occupancy);
        assert!(features.total_bandwidth);
        assert!(features.local_bandwidth);
        assert_eq!(features.num_rmids, 6);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn selects_resctrl_control_group_containing_domain_cpus() {
        let domain = test_domain(vec![2, 3], test_scope());
        let root = ResctrlControlGroup {
            cpus: vec![0, 1, 2, 3],
            path: PathBuf::from("/sys/fs/resctrl"),
            tasks_assigned: false,
        };
        let child = ResctrlControlGroup {
            cpus: vec![2, 3],
            path: PathBuf::from("/sys/fs/resctrl/latency"),
            tasks_assigned: false,
        };
        let groups = vec![child.clone(), root];

        let selected = resctrl_control_group_for_domain(&domain, &groups).unwrap();

        assert_eq!(selected.path, child.path);
    }

    #[test]
    fn rejects_task_assigned_resctrl_control_groups() {
        let root = unique_test_dir("resctrl-rdt-task-control-conflict");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("latency")).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("tasks"), "1\n").unwrap();
        std::fs::write(root.join("latency/cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("latency/tasks"), "123\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");
        let domains = vec![test_domain(vec![0], test_scope())];

        let error = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap_err();

        assert!(error.contains("control group"));
        assert!(!root.join("latency/mon_groups/ocellus-p0-c0-cpu0").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detects_resctrl_monitor_assignment_conflicts() {
        let domain = test_domain(vec![2, 3], test_scope());
        let assignments = vec![ResctrlMonitorAssignment {
            cpus: vec![1, 2],
            path: PathBuf::from("/sys/fs/resctrl/mon_groups/other"),
            tasks_assigned: false,
        }];

        let conflict = conflicting_resctrl_monitor_assignment(&domain, &assignments).unwrap();

        assert_eq!(conflict, Path::new("/sys/fs/resctrl/mon_groups/other"));
    }

    #[test]
    fn rejects_existing_resctrl_task_monitor_groups() {
        let root = unique_test_dir("resctrl-rdt-task-mon-conflict");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups/task-only")).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        std::fs::write(root.join("mon_groups/task-only/cpus_list"), "\n").unwrap();
        std::fs::write(root.join("mon_groups/task-only/tasks"), "123\n").unwrap();
        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");
        let domains = vec![test_domain(vec![0], test_scope())];

        let error = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap_err();

        assert!(error.contains("task assignments"));
        assert!(!root.join("mon_groups/ocellus-p0-c0-cpu0").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn names_resctrl_monitor_groups_by_scope() {
        let domain = test_domain(
            vec![4, 5],
            RdtScope {
                core_id: 2,
                cpu: 4,
                die_group_id: None,
                die_id: None,
                package_id: 1,
            },
        );

        assert_eq!(resctrl_monitor_group_name(&domain), "ocellus-p1-c2-cpu4");
    }

    #[test]
    fn converts_resctrl_readings_to_existing_rdt_metrics() {
        let previous = RdtReading {
            at: Instant::now(),
            scopes: vec![RdtScopeReading {
                conversion_factor_bytes: 1.0,
                counters: RdtCounters {
                    local_memory_bandwidth: Some(100),
                    total_memory_bandwidth: Some(200),
                },
                l3_occupancy_bytes: Some(4096.0),
                scope: test_scope(),
            }],
        };
        let current = RdtReading {
            at: previous.at + Duration::from_secs(2),
            scopes: vec![RdtScopeReading {
                conversion_factor_bytes: 1.0,
                counters: RdtCounters {
                    local_memory_bandwidth: Some(160),
                    total_memory_bandwidth: Some(320),
                },
                l3_occupancy_bytes: Some(8192.0),
                scope: test_scope(),
            }],
        };

        let metrics = RdtMetrics::from_readings(previous, current, u64::MAX).unwrap();
        let scope = metrics.scopes[0];

        assert_eq!(scope.l3_occupancy_bytes, Some(8192.0));
        assert_eq!(scope.total_memory_bandwidth_bytes_per_second, Some(60.0));
        assert_eq!(scope.local_memory_bandwidth_bytes_per_second, Some(30.0));
        assert_eq!(scope.remote_memory_bandwidth_bytes_per_second, Some(30.0));
    }

    #[test]
    fn treats_unavailable_resctrl_counter_as_absent() {
        assert_eq!(parse_resctrl_counter_value("Unavailable\n"), Ok(None));
        assert_eq!(parse_resctrl_counter_value("Unassigned\n"), Ok(None));
        assert_eq!(parse_resctrl_counter_value("Error\n"), Ok(None));
        assert_eq!(parse_resctrl_counter_value("123\n"), Ok(Some(123)));
        assert!(parse_resctrl_counter_value("not-a-number").is_err());
    }

    #[test]
    fn sums_resctrl_counters_across_l3_domains() {
        let root = unique_test_dir("resctrl-rdt-sum");
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_01")).unwrap();
        std::fs::write(root.join("mon_data/mon_L3_00/llc_occupancy"), "100\n").unwrap();
        std::fs::write(root.join("mon_data/mon_L3_01/llc_occupancy"), "200\n").unwrap();
        let group = ResctrlMonitorGroup {
            l3_domains: vec!["mon_L3_00".to_string(), "mon_L3_01".to_string()],
            path: root.clone(),
            scope: test_scope(),
        };

        assert_eq!(
            sum_resctrl_counters(&group, RESCTRL_L3_OCCUPANCY),
            Ok(Some(300))
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_resctrl_l3_subdomains_instead_of_snc_aggregates() {
        let root = unique_test_dir("resctrl-rdt-snc-discover");
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00/mon_sub_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00/mon_sub_L3_01")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_01")).unwrap();

        let domains = discover_resctrl_l3_domains(&root).unwrap();

        assert_eq!(
            domains,
            vec![
                "mon_L3_00/mon_sub_L3_00".to_string(),
                "mon_L3_00/mon_sub_L3_01".to_string(),
                "mon_L3_01".to_string()
            ]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn maps_resctrl_snc_monitor_groups_by_l3_cache_and_numa_node() {
        let available = vec![
            "mon_L3_00".to_string(),
            "mon_L3_00/mon_sub_L3_00".to_string(),
            "mon_L3_00/mon_sub_L3_01".to_string(),
        ];

        let domains =
            resctrl_l3_domains_for_cpus_with_topology(&[7], &available, |_| Ok(0), |_| Ok(Some(1)))
                .unwrap();

        assert_eq!(domains, vec!["mon_L3_00/mon_sub_L3_01"]);
    }

    #[test]
    fn maps_resctrl_monitor_groups_to_assigned_l3_domains() {
        let root = unique_test_dir("resctrl-rdt-assigned-l3");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_01")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups")).unwrap();
        std::fs::write(root.join("cpus_list"), "0,1\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");
        let domains = vec![
            test_domain(vec![0], test_scope()),
            test_domain(
                vec![1],
                RdtScope {
                    core_id: 1,
                    cpu: 1,
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
            ),
        ];

        let groups = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_l3_domains_for_cpus,
        )
        .unwrap();

        assert_eq!(groups[0].l3_domains, vec!["mon_L3_00"]);
        assert_eq!(groups[1].l3_domains, vec!["mon_L3_01"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn limits_resctrl_monitor_groups_to_free_kernel_rmids() {
        let root = unique_test_dir("resctrl-rdt-rmid-capacity");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_01")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups")).unwrap();
        std::fs::write(root.join("cpus_list"), "0-2\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        let features = ResctrlMonitorFeatures::parse_with_num_rmids("llc_occupancy\n", 2);
        let domains = vec![
            test_domain(vec![0], test_scope()),
            test_domain(
                vec![1],
                RdtScope {
                    core_id: 1,
                    cpu: 1,
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
            ),
        ];

        let groups = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_l3_domains_for_cpus,
        )
        .unwrap();

        assert_eq!(groups.len(), 1);
        assert!(root.join("mon_groups/ocellus-p0-c0-cpu0").exists());
        assert!(!root.join("mon_groups/ocellus-p0-c1-cpu1").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn counts_existing_resctrl_monitor_groups_against_free_rmids() {
        let root = unique_test_dir("resctrl-rdt-rmid-capacity-existing");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups/other")).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        std::fs::write(root.join("mon_groups/other/cpus_list"), "\n").unwrap();
        let features = ResctrlMonitorFeatures::parse_with_num_rmids("llc_occupancy\n", 2);
        let domains = vec![test_domain(vec![0], test_scope())];

        let error = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap_err();

        assert!(error.contains("no free monitor RMIDs"));
        assert!(!root.join("mon_groups/ocellus-p0-c0-cpu0").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn treats_partial_resctrl_counter_sums_as_absent() {
        let root = unique_test_dir("resctrl-rdt-partial-sum");
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_01")).unwrap();
        std::fs::write(root.join("mon_data/mon_L3_00/mbm_total_bytes"), "100\n").unwrap();
        std::fs::write(
            root.join("mon_data/mon_L3_01/mbm_total_bytes"),
            "Unavailable\n",
        )
        .unwrap();
        let group = ResctrlMonitorGroup {
            l3_domains: vec!["mon_L3_00".to_string(), "mon_L3_01".to_string()],
            path: root.clone(),
            scope: test_scope(),
        };

        assert_eq!(sum_resctrl_counters(&group, RESCTRL_MBM_TOTAL), Ok(None));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removes_only_exact_stale_ocellus_monitor_group_names() {
        let root = unique_test_dir("resctrl-rdt-stale");
        std::fs::create_dir_all(root.join("mon_groups/ocellus-p0-c0-cpu0")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups/ocellus-user")).unwrap();
        let group_names = BTreeSet::from(["ocellus-p0-c0-cpu0".to_string()]);

        remove_stale_ocellus_groups_by_name(&root, &group_names).unwrap();

        assert!(!root.join("mon_groups/ocellus-p0-c0-cpu0").exists());
        assert!(root.join("mon_groups/ocellus-user").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removes_stale_resctrl_groups_before_counting_free_rmids() {
        let root = unique_test_dir("resctrl-rdt-stale-rmid-capacity");
        let stale_path = root.join("mon_groups/ocellus-p0-c0-cpu0");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(&stale_path).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        std::fs::write(stale_path.join("cpus_list"), "\n").unwrap();
        let features = ResctrlMonitorFeatures::parse_with_num_rmids("llc_occupancy\n", 2);
        let domains = vec![test_domain(vec![0], test_scope())];

        let groups = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(stale_path.join("cpus_list")).unwrap(),
            "0\n"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_assigned_ocellus_monitor_group_names_as_conflicts() {
        let root = unique_test_dir("resctrl-rdt-active-ocellus-conflict");
        let group_path = root.join("mon_groups/ocellus-p0-c0-cpu0");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(&group_path).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        std::fs::write(group_path.join("cpus_list"), "0\n").unwrap();
        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");
        let domains = vec![test_domain(vec![0], test_scope())];

        let error = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap_err();

        assert!(error.contains("already assigned"));
        assert!(group_path.exists());
        assert_eq!(
            std::fs::read_to_string(group_path.join("cpus_list")).unwrap(),
            "0\n"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleans_up_resctrl_monitor_groups_when_later_domain_mapping_fails() {
        let root = unique_test_dir("resctrl-rdt-map-fail");
        std::fs::create_dir_all(root.join("info/L3_MON")).unwrap();
        std::fs::create_dir_all(root.join("mon_data/mon_L3_00")).unwrap();
        std::fs::create_dir_all(root.join("mon_groups")).unwrap();
        std::fs::write(root.join("cpus_list"), "0\n").unwrap();
        std::fs::write(root.join("info/L3_MON/mon_features"), "llc_occupancy\n").unwrap();
        let features = ResctrlMonitorFeatures::parse("llc_occupancy\n");
        let domains = vec![
            test_domain(vec![0], test_scope()),
            test_domain(
                vec![1],
                RdtScope {
                    core_id: 1,
                    cpu: 1,
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
            ),
        ];

        let error = create_resctrl_monitor_groups_with_l3_domain_mapper(
            &root,
            &domains,
            &features,
            test_first_l3_domain_for_cpus,
        )
        .unwrap_err();

        assert!(error.contains("failed to find resctrl control group"));
        assert!(!root.join("mon_groups/ocellus-p0-c0-cpu0").exists());

        let _ = std::fs::remove_dir_all(root);
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
    fn discovers_resctrl_domains_without_msr_rmid_capacity() {
        let msr_limited_topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 1, 0, 0),
        ];
        let error = rdt_domains_from_topologies_with_numa(
            msr_limited_topologies,
            3,
            |_| Ok(RdtCacheDomain::SharedCpuList(vec![0, 1])),
            |cpu| Ok(Some(cpu)),
        )
        .unwrap_err();
        assert!(error.contains("across 2 SNC nodes"));

        let topologies = [
            test_topology(0, 0, 0, 0, 0, 0),
            test_topology(1, 0, 0, 1, 0, 0),
        ];

        let domains = resctrl_rdt_domains_from_topologies_with_numa(
            topologies,
            |_| Ok(RdtCacheDomain::SharedCpuList(vec![0, 1])),
            |cpu| Ok(Some(cpu)),
        )
        .unwrap();

        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].cpus, vec![0]);
        assert_eq!(domains[0].rmid, 1);
        assert_eq!(domains[0].physical_rmid, 1);
        assert_eq!(domains[1].cpus, vec![1]);
        assert_eq!(domains[1].rmid, 2);
        assert_eq!(domains[1].physical_rmid, 2);
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

    fn test_domain(cpus: Vec<u32>, scope: RdtScope) -> RdtDomain {
        RdtDomain {
            cpus,
            original_pqr_assoc: Vec::new(),
            physical_rmid: 1,
            rmid: 1,
            scope,
            snc_nodes_per_l3_cache: 1,
        }
    }

    fn test_l3_domains_for_cpus(cpus: &[u32], available: &[String]) -> Result<Vec<String>, String> {
        let available = available
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let domains = cpus
            .iter()
            .map(|cpu| resctrl_l3_domain_name(cpu % 2))
            .filter(|domain| available.contains(domain.as_str()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Ok(domains)
    }

    fn test_first_l3_domain_for_cpus(
        _cpus: &[u32],
        available: &[String],
    ) -> Result<Vec<String>, String> {
        Ok(available.first().cloned().into_iter().collect())
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ocellus-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
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
