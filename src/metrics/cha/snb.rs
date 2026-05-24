use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::msr::Msr;
use crate::metal::topology::TopologyLevelKind;
use crate::metrics::cha::{
    CHA_COUNTER_COUNT, ChaCacheState, ChaEventKind, ChaEventMeasurement, ChaLlcLookupMetrics,
    ChaLlcVictimMetrics, ChaLookupOperation, ChaMultiplexMode, ChaScopeMetrics,
    ChaTransactionLabel, ChaTransactionMetrics, ChaTransactionResult, ChaTransactionResultMetrics,
    bytes_per_second, llc_victim_metrics, required_measurement, scale_measurement_value,
};
use crate::metrics::uncore::hsx::{self, HsxUncoreScope};
use crate::metrics::uncore::skx::{UncoreScope, queue_residency_seconds, ratio};

const SNB_CBO_EVENT_GROUP_COUNT: usize = 19;
const SNB_CBO_EXPORTED_TRANSACTION_COUNT: usize = 3;
const SNB_MAX_CBO_COUNT: usize = 32;

const CBO_COUNTER_BASE: u64 = 0x0d16;
const CBO_CONTROL_BASE: u64 = 0x0d10;
const CBO_FILTER0_BASE: u64 = 0x0d14;
const CBO_FILTER1_BASE: u64 = 0x0d1a;
const CBO_UNIT_CONTROL_BASE: u64 = 0x0d04;
const CBO_UNIT_STRIDE: u64 = 0x20;

const COUNTER_ENABLE_BIT: u64 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u64 = 1 << 20;
const COUNTER_RESET_BIT: u64 = 1 << 17;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 0;
const UNIT_FREEZE_BIT: u64 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u64 = 1 << 16;
const TOR_INSERTS_EVENT: u8 = 0x35;
const TOR_OCCUPANCY_EVENT: u8 = 0x36;
const TOR_OPCODE_UMASK: u8 = 0x01;
const TOR_MISS_OPCODE_UMASK: u8 = 0x03;

const SNB_LLC_LOOKUP_OPERATIONS: [ChaLookupOperation; 3] = [
    ChaLookupOperation::Read,
    ChaLookupOperation::Write,
    ChaLookupOperation::RemoteSnoop,
];
const IVB_LLC_LOOKUP_OPERATIONS: [ChaLookupOperation; 4] = [
    ChaLookupOperation::Read,
    ChaLookupOperation::Write,
    ChaLookupOperation::RemoteSnoop,
    ChaLookupOperation::Any,
];
const SNB_IVB_LLC_LOOKUP_STATES: [ChaCacheState; 5] = [
    ChaCacheState::I,
    ChaCacheState::S,
    ChaCacheState::E,
    ChaCacheState::M,
    ChaCacheState::F,
];
const SNB_IVB_LLC_VICTIM_STATES: [ChaCacheState; 3] =
    [ChaCacheState::M, ChaCacheState::E, ChaCacheState::S];

const SNB_CBO_EXPORTED_TRANSACTIONS: [SnbTransactionKind; SNB_CBO_EXPORTED_TRANSACTION_COUNT] = [
    SnbTransactionKind::IaRfo,
    SnbTransactionKind::IaDrd,
    SnbTransactionKind::IaItoM,
];

const SNB_CBO_EVENT_GROUPS: [SnbChaEventGroup; SNB_CBO_EVENT_GROUP_COUNT] = [
    SnbChaEventGroup::frequency(),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Snb, ChaCacheState::I),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Snb, ChaCacheState::I),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Snb, ChaCacheState::S),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Snb, ChaCacheState::S),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Snb, ChaCacheState::E),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Snb, ChaCacheState::E),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Snb, ChaCacheState::M),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Snb, ChaCacheState::M),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Snb, ChaCacheState::F),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Snb, ChaCacheState::F),
    SnbChaEventGroup::llc_victims_m_e(),
    SnbChaEventGroup::llc_victims_s(),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaRfo,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaRfo,
        SnbTorCounterKind::Miss,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaDrd,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaDrd,
        SnbTorCounterKind::Miss,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaItoM,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Snb,
        SnbTransactionKind::IaItoM,
        SnbTorCounterKind::Miss,
    ),
];

const IVB_CBO_EVENT_GROUPS: [SnbChaEventGroup; SNB_CBO_EVENT_GROUP_COUNT] = [
    SnbChaEventGroup::frequency(),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Ivb, ChaCacheState::I),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Ivb, ChaCacheState::I),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Ivb, ChaCacheState::S),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Ivb, ChaCacheState::S),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Ivb, ChaCacheState::E),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Ivb, ChaCacheState::E),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Ivb, ChaCacheState::M),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Ivb, ChaCacheState::M),
    SnbChaEventGroup::llc_lookup_read_write(SnbChaArchitecture::Ivb, ChaCacheState::F),
    SnbChaEventGroup::llc_lookup_remote_any(SnbChaArchitecture::Ivb, ChaCacheState::F),
    SnbChaEventGroup::llc_victims_m_e(),
    SnbChaEventGroup::llc_victims_s(),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaRfo,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaRfo,
        SnbTorCounterKind::Miss,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaDrd,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaDrd,
        SnbTorCounterKind::Miss,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaItoM,
        SnbTorCounterKind::Total,
    ),
    SnbChaEventGroup::transaction(
        SnbChaArchitecture::Ivb,
        SnbTransactionKind::IaItoM,
        SnbTorCounterKind::Miss,
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbChaArchitecture {
    Ivb,
    Snb,
}

impl SnbChaArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::IvyTown => Some(Self::Ivb),
            IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
            _ => None,
        }
    }

    const fn event_groups(self) -> &'static [SnbChaEventGroup] {
        match self {
            Self::Ivb => &IVB_CBO_EVENT_GROUPS,
            Self::Snb => &SNB_CBO_EVENT_GROUPS,
        }
    }

    const fn filter_spec(self) -> SnbChaFilterSpec {
        match self {
            Self::Ivb => SnbChaFilterSpec {
                opcode_shift: 52,
                state_shift: 17,
                thread_id_shift: 0,
            },
            Self::Snb => SnbChaFilterSpec {
                opcode_shift: 23,
                state_shift: 18,
                thread_id_shift: 0,
            },
        }
    }

    const fn has_filter1(self) -> bool {
        matches!(self, Self::Ivb)
    }

    const fn llc_lookup_operations(self) -> &'static [ChaLookupOperation] {
        match self {
            Self::Ivb => &IVB_LLC_LOOKUP_OPERATIONS,
            Self::Snb => &SNB_LLC_LOOKUP_OPERATIONS,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ivb => "Ivy Bridge-EP",
            Self::Snb => "Sandy Bridge-EP",
        }
    }

    const fn unit_freeze(self) -> u64 {
        match self {
            Self::Ivb => UNIT_FREEZE_BIT,
            Self::Snb => UNIT_FREEZE_ENABLE_BIT | UNIT_FREEZE_BIT,
        }
    }

    const fn unit_freeze_and_reset(self) -> u64 {
        match self {
            Self::Ivb => UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT,
            Self::Snb => {
                UNIT_FREEZE_ENABLE_BIT
                    | UNIT_FREEZE_BIT
                    | UNIT_CONTROL_RESET_BIT
                    | UNIT_COUNTER_RESET_BIT
            }
        }
    }

    const fn unit_unfreeze(self) -> u64 {
        match self {
            Self::Ivb => 0,
            Self::Snb => UNIT_FREEZE_ENABLE_BIT,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnbChaMetrics {
    pub llc_lookups: Vec<ChaLlcLookupMetrics>,
    pub llc_victims: Vec<ChaLlcVictimMetrics>,
    pub scopes: Vec<ChaScopeMetrics>,
    pub transaction_results: Vec<ChaTransactionResultMetrics>,
    pub transactions: Vec<ChaTransactionMetrics>,
}

impl SnbChaMetrics {
    fn from_measurements(
        architecture: SnbChaArchitecture,
        measurements: BTreeMap<HsxUncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut llc_lookups = Vec::new();
        let mut llc_victims = Vec::new();
        let mut scopes = Vec::with_capacity(measurements.len());
        let mut transaction_results = Vec::new();
        let mut transactions = Vec::new();

        for (scope, scope_measurements) in measurements {
            let scope = to_skx_scope(scope);
            let clockticks =
                required_measurement(&scope_measurements, ChaEventKind::EvictionClockticks)?;

            scopes.push(ChaScopeMetrics {
                frequency_hz: hsx::frequency_hz(clockticks.value, clockticks.running),
                scope,
            });

            llc_lookups.extend(snb_llc_lookup_metrics(
                architecture,
                scope,
                &scope_measurements,
            )?);
            llc_victims.extend(llc_victim_metrics(
                scope,
                &scope_measurements,
                &SNB_IVB_LLC_VICTIM_STATES,
            )?);

            let transaction_scope_metrics = snb_transaction_metrics(scope, &scope_measurements)?;
            transaction_results.extend(transaction_scope_metrics.results);
            transactions.extend(transaction_scope_metrics.totals);
        }

        Ok(Self {
            llc_lookups,
            llc_victims,
            scopes,
            transaction_results,
            transactions,
        })
    }
}

#[derive(Debug)]
pub struct SnbChaPrometheusMetrics {
    frequency_hz: Family<SnbChaScopeLabels, Gauge<f64, AtomicU64>>,
    llc_lookup_bytes_per_second: Family<SnbChaLlcLookupLabels, Gauge<f64, AtomicU64>>,
    llc_victims_per_second: Family<SnbChaStateLabels, Gauge<f64, AtomicU64>>,
    transaction_bandwidth_bytes_per_second: Family<SnbChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_hit_rate: Family<SnbChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_latency_seconds: Family<SnbChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_result_bandwidth_bytes_per_second:
        Family<SnbChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_inserts_per_second:
        Family<SnbChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_latency_seconds:
        Family<SnbChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_occupancy_entries:
        Family<SnbChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
}

impl SnbChaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            frequency_hz: Family::<SnbChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            llc_lookup_bytes_per_second:
                Family::<SnbChaLlcLookupLabels, Gauge<f64, AtomicU64>>::default(),
            llc_victims_per_second: Family::<SnbChaStateLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_bandwidth_bytes_per_second: Family::<
                SnbChaTransactionLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_hit_rate: Family::<SnbChaTransactionLabels, Gauge<f64, AtomicU64>>::default(
            ),
            transaction_latency_seconds:
                Family::<SnbChaTransactionLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_result_bandwidth_bytes_per_second: Family::<
                SnbChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_inserts_per_second: Family::<
                SnbChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_latency_seconds: Family::<
                SnbChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_occupancy_entries: Family::<
                SnbChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
        };

        registry.register(
            "ocellus_cha_frequency_hz",
            "Interval-derived CHA clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_cha_llc_lookup_bytes_per_second",
            "Interval-derived CHA LLC lookup bandwidth in bytes per second",
            metrics.llc_lookup_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_llc_victims_per_second",
            "Interval-derived CHA LLC victims per second",
            metrics.llc_victims_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_bandwidth_bytes_per_second",
            "Interval-derived CHA transaction bandwidth in bytes per second",
            metrics.transaction_bandwidth_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_hit_rate",
            "Interval-derived CHA transaction hit rate",
            metrics.transaction_hit_rate.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_latency_seconds",
            "Interval-derived CHA transaction latency in seconds",
            metrics.transaction_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_bandwidth_bytes_per_second",
            "Interval-derived CHA transaction result bandwidth in bytes per second",
            metrics
                .transaction_result_bandwidth_bytes_per_second
                .clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_inserts_per_second",
            "Interval-derived CHA transaction result inserts per second",
            metrics.transaction_result_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_latency_seconds",
            "Interval-derived CHA transaction result residency latency in seconds",
            metrics.transaction_result_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_occupancy_entries",
            "Average CHA transaction result occupancy in entries",
            metrics.transaction_result_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SnbChaMetrics) {
        for scope in metrics.scopes {
            self.frequency_hz
                .get_or_create(&SnbChaScopeLabels::from_scope(scope.scope))
                .set(scope.frequency_hz);
        }

        for metric in metrics.llc_lookups {
            self.llc_lookup_bytes_per_second
                .get_or_create(&SnbChaLlcLookupLabels::from_metric(metric))
                .set(metric.bytes_per_second);
        }

        for metric in metrics.llc_victims {
            self.llc_victims_per_second
                .get_or_create(&SnbChaStateLabels::from_llc_victim(metric))
                .set(metric.per_second);
        }

        for metric in metrics.transaction_results {
            let labels = SnbChaTransactionResultLabels::from_metric(metric);

            self.transaction_result_bandwidth_bytes_per_second
                .get_or_create(&labels)
                .set(metric.bandwidth_bytes_per_second);
            self.transaction_result_inserts_per_second
                .get_or_create(&labels)
                .set(metric.inserts_per_second);
            self.transaction_result_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
            self.transaction_result_occupancy_entries
                .get_or_create(&labels)
                .set(metric.occupancy_entries);
        }

        for metric in metrics.transactions {
            let labels = SnbChaTransactionLabels::from_metric(metric);

            self.transaction_bandwidth_bytes_per_second
                .get_or_create(&labels)
                .set(metric.bandwidth_bytes_per_second);
            self.transaction_hit_rate
                .get_or_create(&labels)
                .set(metric.hit_rate);
            self.transaction_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbChaScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl SnbChaScopeLabels {
    fn from_scope(scope: UncoreScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbChaStateLabels {
    die: String,
    die_group: String,
    package: String,
    state: String,
}

impl SnbChaStateLabels {
    fn from_llc_victim(metric: ChaLlcVictimMetrics) -> Self {
        Self {
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbChaLlcLookupLabels {
    die: String,
    die_group: String,
    operation: String,
    package: String,
    state: String,
}

impl SnbChaLlcLookupLabels {
    fn from_metric(metric: ChaLlcLookupMetrics) -> Self {
        Self {
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
            operation: metric.operation.label().to_string(),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbChaTransactionLabels {
    die: String,
    die_group: String,
    package: String,
    transaction: String,
}

impl SnbChaTransactionLabels {
    fn from_metric(metric: ChaTransactionMetrics) -> Self {
        Self {
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
            package: metric.scope.package_id.to_string(),
            transaction: metric.transaction.as_str().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbChaTransactionResultLabels {
    die: String,
    die_group: String,
    package: String,
    result: String,
    transaction: String,
}

impl SnbChaTransactionResultLabels {
    fn from_metric(metric: ChaTransactionResultMetrics) -> Self {
        Self {
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
            package: metric.scope.package_id.to_string(),
            result: metric.result.label().to_string(),
            transaction: metric.transaction.as_str().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct SnbChaCollector {
    architecture: SnbChaArchitecture,
    multiplex_mode: ChaMultiplexMode,
    next_group: usize,
    next_partition_offset: usize,
    packages: Vec<SnbChaPackage>,
}

impl SnbChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = SnbChaArchitecture::from_model(model).ok_or_else(|| {
            format!("Sandy/Ivy Bridge-EP CHA collection is not supported for {model:?}")
        })?;
        let packages = discover_packages(architecture)?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            architecture,
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages,
        })
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        if let Err(error) = self.validate_multiplex_mode(mode) {
            eprintln!("ocellus: disabling Sandy/Ivy Bridge-EP CHA spatial multiplexing: {error}");
            self.multiplex_mode = ChaMultiplexMode::Temporal;
            return;
        }

        self.multiplex_mode = mode;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SnbChaMetrics, String> {
        if interval.is_zero() {
            return Err("Sandy/Ivy Bridge-EP CHA measure interval must be non-zero".to_string());
        }

        let mut measurements = SnbChaMeasurementAccumulator::new();
        let packages = &self.packages;

        let slices = self.schedule(interval);
        let measured_slice_count = slices.len();

        for slice in slices {
            program_packages(packages, slice)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                interval,
                started_at.elapsed(),
                slice,
                &mut measurements,
            )?;
        }

        self.rotate_schedule(measured_slice_count);

        SnbChaMetrics::from_measurements(self.architecture, measurements.into_measurements())
    }

    fn rotate_schedule(&mut self, measured_slice_count: usize) {
        self.next_group = (self.next_group + self.multiplex_mode.partitions())
            % self.architecture.event_groups().len();
        self.next_partition_offset = self
            .next_partition_offset
            .wrapping_add(measured_slice_count);
    }

    fn schedule(&self, interval: Duration) -> Vec<SnbChaMeasurementSlice> {
        let event_groups = self.architecture.event_groups();
        let group_count = event_groups.len();
        let partitions = self.multiplex_mode.partitions();
        let slice_count_per_round = group_count.div_ceil(partitions);
        let round_count = measurement_round_count(interval, slice_count_per_round);
        let slice_count = slice_count_per_round * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for round in 0..round_count {
            for slice_index in 0..slice_count_per_round {
                let first_group_offset = slice_index * partitions;
                let groups =
                    self.slice_groups(event_groups, first_group_offset, partitions, group_count);

                slices.push(SnbChaMeasurementSlice {
                    duration: slice_duration,
                    groups,
                    partition_offset: self.next_partition_offset
                        + (round * slice_count_per_round)
                        + slice_index,
                    partition_width: partitions,
                });
            }
        }

        slices
    }

    fn slice_groups(
        &self,
        event_groups: &'static [SnbChaEventGroup],
        first_group_offset: usize,
        partitions: usize,
        group_count: usize,
    ) -> [Option<SnbChaEventGroup>; CHA_COUNTER_COUNT] {
        let mut groups = [None; CHA_COUNTER_COUNT];

        for (partition, group) in groups.iter_mut().enumerate().take(partitions) {
            let group_offset = first_group_offset + partition;
            if group_offset < group_count {
                let group_index = (self.next_group + group_offset) % group_count;
                *group = Some(event_groups[group_index]);
            }
        }

        groups
    }

    fn validate_multiplex_mode(&self, mode: ChaMultiplexMode) -> Result<(), String> {
        let partitions = mode.partitions();

        if partitions == 0 || partitions > CHA_COUNTER_COUNT {
            return Err(format!(
                "CHA spatial partitions must be between 1 and {CHA_COUNTER_COUNT}"
            ));
        }

        for package in &self.packages {
            if partitions > package.units.len() {
                return Err(format!(
                    "CHA spatial partitions ({partitions}) exceed discovered CBo units ({}) for package {:?}",
                    package.units.len(),
                    package.scope
                ));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbChaUnitReading {
    counters: [u64; CHA_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct SnbChaUnit {
    architecture: SnbChaArchitecture,
    cpu: u32,
    id: usize,
}

impl SnbChaUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_freeze())
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_freeze_and_reset())
    }

    fn program(self, group: SnbChaEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        let filter = group.filter.values();

        msr.write(cbo_filter0_offset(self.id), filter.filter0)?;
        if self.architecture.has_filter1() {
            msr.write(cbo_filter1_offset(self.id), filter.filter1)?;
        }

        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(
                cbo_control_offset(self.id, counter_index),
                counter_control(event.event, event.umask),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<SnbChaUnitReading, String> {
        Ok(SnbChaUnitReading {
            counters: [
                self.read_counter(0).map(mask_counter)?,
                self.read_counter(1).map(mask_counter)?,
                self.read_counter(2).map(mask_counter)?,
                self.read_counter(3).map(mask_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_unfreeze())
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(cbo_counter_offset(self.id, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(cbo_unit_control_offset(self.id), value)
    }

    fn probe_writable(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;

        msr.write(
            cbo_unit_control_offset(self.id),
            self.architecture.unit_freeze(),
        )?;
        msr.write(cbo_filter0_offset(self.id), 0)?;
        if self.architecture.has_filter1() {
            msr.write(cbo_filter1_offset(self.id), 0)?;
        }
        msr.write(cbo_control_offset(self.id, 0), 0)?;
        Ok(())
    }

    fn probe_group(self, group: SnbChaEventGroup) -> Result<(), String> {
        self.freeze_and_reset()?;
        self.program(group)
    }
}

#[derive(Debug)]
struct SnbChaPackage {
    scope: HsxUncoreScope,
    units: Vec<SnbChaUnit>,
}

impl SnbChaPackage {
    fn new(scope: HsxUncoreScope, units: Vec<SnbChaUnit>) -> Self {
        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaFilterSpec {
    opcode_shift: u32,
    state_shift: u32,
    thread_id_shift: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaEventSpec {
    event: u8,
    kind: SnbChaEventKind,
    umask: u8,
}

impl SnbChaEventSpec {
    const fn new(kind: SnbChaEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }

    const fn clockticks() -> Self {
        Self::new(SnbChaEventKind::Clockticks, 0x00, 0x00)
    }

    const fn transaction_clockticks(
        transaction: SnbTransactionKind,
        counter_kind: SnbTorCounterKind,
    ) -> Self {
        Self::new(
            SnbChaEventKind::TransactionClockticks(transaction, counter_kind),
            0x00,
            0x00,
        )
    }

    const fn unused() -> Self {
        Self::new(SnbChaEventKind::Unused, 0x00, 0x00)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaEventGroup {
    events: [SnbChaEventSpec; CHA_COUNTER_COUNT],
    filter: SnbChaFilter,
}

impl SnbChaEventGroup {
    const fn frequency() -> Self {
        Self {
            events: [
                SnbChaEventSpec::clockticks(),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::none(),
        }
    }

    const fn llc_lookup_read_write(architecture: SnbChaArchitecture, state: ChaCacheState) -> Self {
        Self {
            events: [
                SnbChaEventSpec::new(
                    SnbChaEventKind::LlcLookup(state, ChaLookupOperation::Read),
                    0x34,
                    0x03,
                ),
                SnbChaEventSpec::new(
                    SnbChaEventKind::LlcLookup(state, ChaLookupOperation::Write),
                    0x34,
                    0x05,
                ),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::llc_lookup_state(architecture, state),
        }
    }

    const fn llc_lookup_remote_any(architecture: SnbChaArchitecture, state: ChaCacheState) -> Self {
        let any = if matches!(architecture, SnbChaArchitecture::Ivb) {
            SnbChaEventSpec::new(
                SnbChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                0x34,
                0x11,
            )
        } else {
            SnbChaEventSpec::unused()
        };

        Self {
            events: [
                SnbChaEventSpec::new(
                    SnbChaEventKind::LlcLookup(state, ChaLookupOperation::RemoteSnoop),
                    0x34,
                    0x09,
                ),
                any,
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::llc_lookup_state(architecture, state),
        }
    }

    const fn llc_victims_m_e() -> Self {
        Self {
            events: [
                SnbChaEventSpec::new(SnbChaEventKind::LlcVictim(ChaCacheState::M), 0x37, 0x01),
                SnbChaEventSpec::new(SnbChaEventKind::LlcVictim(ChaCacheState::E), 0x37, 0x02),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::none(),
        }
    }

    const fn llc_victims_s() -> Self {
        Self {
            events: [
                SnbChaEventSpec::new(SnbChaEventKind::LlcVictim(ChaCacheState::S), 0x37, 0x04),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::none(),
        }
    }

    const fn transaction(
        architecture: SnbChaArchitecture,
        transaction: SnbTransactionKind,
        counter_kind: SnbTorCounterKind,
    ) -> Self {
        let umask = match counter_kind {
            SnbTorCounterKind::Total => TOR_OPCODE_UMASK,
            SnbTorCounterKind::Miss => TOR_MISS_OPCODE_UMASK,
        };

        Self {
            events: [
                SnbChaEventSpec::new(
                    SnbChaEventKind::TransactionOccupancy(transaction, counter_kind),
                    TOR_OCCUPANCY_EVENT,
                    umask,
                ),
                SnbChaEventSpec::new(
                    SnbChaEventKind::TransactionInsert(transaction, counter_kind),
                    TOR_INSERTS_EVENT,
                    umask,
                ),
                SnbChaEventSpec::transaction_clockticks(transaction, counter_kind),
                SnbChaEventSpec::unused(),
            ],
            filter: SnbChaFilter::opcode(architecture, transaction.opcode()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaFilter {
    opcode: u16,
    spec: SnbChaFilterSpec,
    state: u16,
    thread_id: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaFilterValues {
    filter0: u64,
    filter1: u64,
}

impl SnbChaFilter {
    const fn none() -> Self {
        Self {
            opcode: 0,
            spec: SnbChaFilterSpec {
                opcode_shift: 0,
                state_shift: 0,
                thread_id_shift: 0,
            },
            state: 0,
            thread_id: 0,
        }
    }

    const fn llc_lookup_state(architecture: SnbChaArchitecture, state: ChaCacheState) -> Self {
        Self {
            opcode: 0,
            spec: architecture.filter_spec(),
            state: snb_llc_lookup_state_bits(state),
            thread_id: 0,
        }
    }

    const fn opcode(architecture: SnbChaArchitecture, opcode: u16) -> Self {
        Self {
            opcode,
            spec: architecture.filter_spec(),
            state: 0,
            thread_id: 0,
        }
    }

    const fn value(self) -> u64 {
        ((self.thread_id as u64) << self.spec.thread_id_shift)
            | ((self.state as u64) << self.spec.state_shift)
            | ((self.opcode as u64) << self.spec.opcode_shift)
    }

    const fn values(self) -> SnbChaFilterValues {
        let value = self.value();

        SnbChaFilterValues {
            filter0: value & 0xffff_ffff,
            filter1: value >> 32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbChaEventKind {
    Clockticks,
    LlcLookup(ChaCacheState, ChaLookupOperation),
    LlcVictim(ChaCacheState),
    TransactionClockticks(SnbTransactionKind, SnbTorCounterKind),
    TransactionInsert(SnbTransactionKind, SnbTorCounterKind),
    TransactionOccupancy(SnbTransactionKind, SnbTorCounterKind),
    Unused,
}

impl SnbChaEventKind {
    const fn is_clockticks(self) -> bool {
        matches!(self, Self::Clockticks | Self::TransactionClockticks(_, _))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbMeasurementKind {
    Exported(ChaEventKind),
    TransactionClockticks(SnbTransactionKind, SnbTorCounterKind),
    TransactionInsert(SnbTransactionKind, SnbTorCounterKind),
    TransactionOccupancy(SnbTransactionKind, SnbTorCounterKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbTorCounterKind {
    Miss,
    Total,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbTransactionKind {
    IaDrd,
    IaItoM,
    IaRfo,
}

impl SnbTransactionKind {
    const fn label(self) -> ChaTransactionLabel {
        match self {
            Self::IaDrd => ChaTransactionLabel::new("ia_drd"),
            Self::IaItoM => ChaTransactionLabel::new("ia_itom"),
            Self::IaRfo => ChaTransactionLabel::new("ia_rfo"),
        }
    }

    const fn opcode(self) -> u16 {
        match self {
            Self::IaDrd => 0x182,
            Self::IaItoM => 0x1c8,
            Self::IaRfo => 0x180,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbChaMeasurement {
    enabled: Duration,
    represented_unit_count: u64,
    running: Duration,
    unit_scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbChaMeasurementSlice {
    duration: Duration,
    groups: [Option<SnbChaEventGroup>; CHA_COUNTER_COUNT],
    partition_offset: usize,
    partition_width: usize,
}

#[derive(Debug, Default)]
struct SnbChaMeasurementAccumulator {
    exported_measurements: BTreeMap<HsxUncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    transaction_clockticks:
        BTreeMap<(HsxUncoreScope, SnbTransactionKind, SnbTorCounterKind), ChaEventMeasurement>,
    transaction_inserts:
        BTreeMap<(HsxUncoreScope, SnbTransactionKind, SnbTorCounterKind), ChaEventMeasurement>,
    transaction_occupancy:
        BTreeMap<(HsxUncoreScope, SnbTransactionKind, SnbTorCounterKind), ChaEventMeasurement>,
}

impl SnbChaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: HsxUncoreScope,
        kind: SnbChaEventKind,
        value: u64,
        measurement: SnbChaMeasurement,
    ) {
        if kind == SnbChaEventKind::Unused {
            return;
        }

        let scaled_value = if kind.is_clockticks() {
            value
        } else {
            (value as f64 * measurement.unit_scale) as u64
        };

        let kind = match kind {
            SnbChaEventKind::Clockticks => {
                SnbMeasurementKind::Exported(ChaEventKind::EvictionClockticks)
            }
            SnbChaEventKind::LlcLookup(state, operation) => {
                SnbMeasurementKind::Exported(ChaEventKind::LlcLookup(state, operation))
            }
            SnbChaEventKind::LlcVictim(state) => {
                SnbMeasurementKind::Exported(ChaEventKind::LlcVictim(state))
            }
            SnbChaEventKind::TransactionClockticks(transaction, result) => {
                SnbMeasurementKind::TransactionClockticks(transaction, result)
            }
            SnbChaEventKind::TransactionInsert(transaction, result) => {
                SnbMeasurementKind::TransactionInsert(transaction, result)
            }
            SnbChaEventKind::TransactionOccupancy(transaction, result) => {
                SnbMeasurementKind::TransactionOccupancy(transaction, result)
            }
            SnbChaEventKind::Unused => unreachable!(),
        };

        self.add_measurement(scope, kind, scaled_value, measurement);
    }

    fn into_measurements(
        mut self,
    ) -> BTreeMap<HsxUncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>> {
        self.export_transaction_measurements();
        self.exported_measurements
    }

    fn export_transaction_measurements(&mut self) {
        let mut transaction_scopes = Vec::new();

        for &(scope, transaction, counter_kind) in self.transaction_inserts.keys() {
            if counter_kind == SnbTorCounterKind::Total
                && SNB_CBO_EXPORTED_TRANSACTIONS.contains(&transaction)
            {
                transaction_scopes.push((scope, transaction));
            }
        }

        for (scope, transaction) in transaction_scopes {
            let Some(total_clockticks) = self
                .transaction_clockticks
                .get(&(scope, transaction, SnbTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_clockticks) = self
                .transaction_clockticks
                .get(&(scope, transaction, SnbTorCounterKind::Miss))
                .copied()
            else {
                continue;
            };
            let Some(total_inserts) = self
                .transaction_inserts
                .get(&(scope, transaction, SnbTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_inserts) = self
                .transaction_inserts
                .get(&(scope, transaction, SnbTorCounterKind::Miss))
                .copied()
            else {
                continue;
            };
            let Some(total_occupancy) = self
                .transaction_occupancy
                .get(&(scope, transaction, SnbTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_occupancy) = self
                .transaction_occupancy
                .get(&(scope, transaction, SnbTorCounterKind::Miss))
                .copied()
            else {
                continue;
            };

            self.insert_transaction_measurements(
                scope,
                transaction,
                ChaTransactionResult::Hit,
                total_clockticks,
                derived_measurement(total_inserts, miss_inserts),
                derived_measurement(total_occupancy, miss_occupancy),
            );
            self.insert_transaction_measurements(
                scope,
                transaction,
                ChaTransactionResult::Miss,
                miss_clockticks,
                miss_inserts,
                miss_occupancy,
            );
        }
    }

    fn insert_transaction_measurements(
        &mut self,
        scope: HsxUncoreScope,
        transaction: SnbTransactionKind,
        result: ChaTransactionResult,
        clockticks: ChaEventMeasurement,
        inserts: ChaEventMeasurement,
        occupancy: ChaEventMeasurement,
    ) {
        let scope_measurements = self.exported_measurements.entry(scope).or_default();

        scope_measurements.insert(
            ChaEventKind::TransactionClockticks(transaction.label(), result),
            clockticks,
        );
        scope_measurements.insert(
            ChaEventKind::TransactionInsert(transaction.label(), result),
            inserts,
        );
        scope_measurements.insert(
            ChaEventKind::TransactionOccupancy(transaction.label(), result),
            occupancy,
        );
    }

    fn add_measurement(
        &mut self,
        scope: HsxUncoreScope,
        kind: SnbMeasurementKind,
        value: u64,
        measurement: SnbChaMeasurement,
    ) {
        let event_measurement = ChaEventMeasurement {
            enabled: measurement.enabled,
            represented_unit_count: measurement.represented_unit_count,
            running: measurement.running,
            value,
        };

        match kind {
            SnbMeasurementKind::Exported(kind) => add_measurement(
                self.exported_measurements
                    .entry(scope)
                    .or_default()
                    .entry(kind),
                event_measurement,
            ),
            SnbMeasurementKind::TransactionClockticks(transaction, counter_kind) => {
                add_measurement(
                    self.transaction_clockticks
                        .entry((scope, transaction, counter_kind)),
                    event_measurement,
                )
            }
            SnbMeasurementKind::TransactionInsert(transaction, counter_kind) => add_measurement(
                self.transaction_inserts
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            SnbMeasurementKind::TransactionOccupancy(transaction, counter_kind) => add_measurement(
                self.transaction_occupancy
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
        }
    }
}

#[derive(Debug)]
struct SnbTransactionScopeMetrics {
    results: Vec<ChaTransactionResultMetrics>,
    totals: Vec<ChaTransactionMetrics>,
}

fn discover_packages(architecture: SnbChaArchitecture) -> Result<Vec<SnbChaPackage>, String> {
    let mut packages = Vec::new();

    for (scope, cpu) in package_leaders(architecture)? {
        packages.push(SnbChaPackage::new(
            scope,
            discover_units(architecture, cpu)?,
        ));
    }

    if packages.is_empty() {
        return Err("failed to discover any Sandy/Ivy Bridge-EP CHA packages".to_string());
    }

    Ok(packages)
}

fn package_leaders(architecture: SnbChaArchitecture) -> Result<Vec<(HsxUncoreScope, u32)>, String> {
    let mut leaders = BTreeMap::new();

    for topology in crate::metal::topology::cpu_topologies()? {
        let scope = HsxUncoreScope {
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        };
        leaders.entry(scope).or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err(format!(
            "failed to discover any {} CHA package leaders",
            architecture.name()
        ));
    }

    Ok(leaders.into_iter().collect())
}

fn discover_units(architecture: SnbChaArchitecture, cpu: u32) -> Result<Vec<SnbChaUnit>, String> {
    let msr = Msr::open_readonly(cpu)?;
    let mut units = Vec::new();

    for id in 0..SNB_MAX_CBO_COUNT {
        if msr.read(cbo_unit_control_offset(id)).is_ok()
            && msr.read(cbo_counter_offset(id, 0)).is_ok()
            && msr.read(cbo_control_offset(id, 0)).is_ok()
            && msr.read(cbo_filter0_offset(id)).is_ok()
            && (!architecture.has_filter1() || msr.read(cbo_filter1_offset(id)).is_ok())
        {
            let unit = SnbChaUnit {
                architecture,
                cpu,
                id,
            };
            if let Err(error) = unit.probe_writable() {
                eprintln!(
                    "ocellus: skipping {} CBo {id} on CPU {cpu}: {error}",
                    architecture.name()
                );
                continue;
            }
            if let Err(error) = probe_unit_groups(architecture, unit) {
                eprintln!(
                    "ocellus: skipping {} CBo {id} on CPU {cpu}: {error}",
                    architecture.name()
                );
                continue;
            }

            units.push(unit);
        }
    }

    if units.is_empty() {
        return Err(format!(
            "failed to discover any {} CBo units on CPU {cpu}",
            architecture.name()
        ));
    }

    Ok(units)
}

fn program_packages(
    packages: &[SnbChaPackage],
    slice: SnbChaMeasurementSlice,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze_and_reset()?;
        }
    }

    for package in packages {
        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = cha_partition(unit_index, slice, package.units.len());
            if let Some(group) = slice.groups[partition] {
                unit.program(group)?;
            }
        }
    }

    Ok(())
}

fn probe_unit_groups(architecture: SnbChaArchitecture, unit: SnbChaUnit) -> Result<(), String> {
    for group in architecture.event_groups() {
        unit.probe_group(*group)?;
    }

    Ok(())
}

fn read_packages(
    packages: &[SnbChaPackage],
    enabled: Duration,
    running: Duration,
    slice: SnbChaMeasurementSlice,
    measurements: &mut SnbChaMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let mut partition_counters = [[0_u64; CHA_COUNTER_COUNT]; CHA_COUNTER_COUNT];
        let mut partition_unit_counts = [0_u64; CHA_COUNTER_COUNT];

        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = cha_partition(unit_index, slice, package.units.len());
            if slice.groups[partition].is_none() {
                continue;
            }

            let reading = unit.read()?;
            partition_unit_counts[partition] += 1;

            for (counter, value) in partition_counters[partition]
                .iter_mut()
                .zip(reading.counters)
            {
                *counter += value;
            }
        }

        for (partition, group) in slice.groups.into_iter().enumerate() {
            let Some(group) = group else {
                continue;
            };

            let unit_count = partition_unit_counts[partition];
            if unit_count == 0 {
                continue;
            }

            let unit_scale = package.units.len() as f64 / unit_count as f64;

            for (counter_index, counter) in partition_counters[partition].into_iter().enumerate() {
                let event = group.events[counter_index];
                let value = if event.kind.is_clockticks() {
                    counter / unit_count
                } else {
                    counter
                };

                measurements.add(
                    package.scope,
                    event.kind,
                    value,
                    SnbChaMeasurement {
                        enabled,
                        represented_unit_count: package.units.len() as u64,
                        running,
                        unit_scale,
                    },
                );
            }
        }
    }

    Ok(())
}

fn freeze_packages(packages: &[SnbChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn unfreeze_packages(packages: &[SnbChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn probe_writable_msrs(packages: &[SnbChaPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn derived_measurement(
    total: ChaEventMeasurement,
    excluded: ChaEventMeasurement,
) -> ChaEventMeasurement {
    ChaEventMeasurement {
        running: total.running.max(excluded.running),
        value: total.value.saturating_sub(excluded.value),
        ..total
    }
}

fn snb_llc_lookup_metrics(
    architecture: SnbChaArchitecture,
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaLlcLookupMetrics>, String> {
    let mut metrics = Vec::new();

    for state in SNB_IVB_LLC_LOOKUP_STATES {
        for &operation in architecture.llc_lookup_operations() {
            metrics.push(ChaLlcLookupMetrics {
                bytes_per_second: bytes_per_second(required_measurement(
                    measurements,
                    ChaEventKind::LlcLookup(state, operation),
                )?),
                operation,
                scope,
                state,
            });
        }
    }

    Ok(metrics)
}

fn snb_transaction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<SnbTransactionScopeMetrics, String> {
    let mut results = Vec::new();
    let mut totals = Vec::new();

    for transaction in SNB_CBO_EXPORTED_TRANSACTIONS {
        let hit = snb_transaction_result_metrics(
            scope,
            measurements,
            transaction,
            ChaTransactionResult::Hit,
        )?;
        let miss = snb_transaction_result_metrics(
            scope,
            measurements,
            transaction,
            ChaTransactionResult::Miss,
        )?;
        let hit_inserts = required_measurement(
            measurements,
            ChaEventKind::TransactionInsert(transaction.label(), ChaTransactionResult::Hit),
        )?;
        let miss_inserts = required_measurement(
            measurements,
            ChaEventKind::TransactionInsert(transaction.label(), ChaTransactionResult::Miss),
        )?;
        let hit_insert_count = scale_measurement_value(hit_inserts) as f64;
        let miss_insert_count = scale_measurement_value(miss_inserts) as f64;
        let total_insert_count = hit_insert_count + miss_insert_count;

        totals.push(ChaTransactionMetrics {
            bandwidth_bytes_per_second: hit.bandwidth_bytes_per_second
                + miss.bandwidth_bytes_per_second,
            hit_rate: ratio(hit_insert_count as u64, total_insert_count as u64),
            latency_seconds: if total_insert_count == 0.0 {
                0.0
            } else {
                ((hit.latency_seconds * hit_insert_count)
                    + (miss.latency_seconds * miss_insert_count))
                    / total_insert_count
            },
            scope,
            transaction: transaction.label(),
        });
        results.push(hit);
        results.push(miss);
    }

    Ok(SnbTransactionScopeMetrics { results, totals })
}

fn snb_transaction_result_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    transaction: SnbTransactionKind,
    result: ChaTransactionResult,
) -> Result<ChaTransactionResultMetrics, String> {
    let clockticks = required_measurement(
        measurements,
        ChaEventKind::TransactionClockticks(transaction.label(), result),
    )?;
    let inserts = required_measurement(
        measurements,
        ChaEventKind::TransactionInsert(transaction.label(), result),
    )?;
    let occupancy = required_measurement(
        measurements,
        ChaEventKind::TransactionOccupancy(transaction.label(), result),
    )?;
    let clocktick_count = scale_measurement_value(clockticks);
    let insert_count = scale_measurement_value(inserts);
    let occupancy_count = scale_measurement_value(occupancy);

    Ok(ChaTransactionResultMetrics {
        bandwidth_bytes_per_second: bytes_per_second(inserts),
        inserts_per_second: hsx::events_per_second(insert_count, inserts.enabled),
        latency_seconds: queue_residency_seconds(
            occupancy_count,
            insert_count,
            clocktick_count,
            clockticks.enabled,
        ),
        occupancy_entries: ratio(occupancy_count, clocktick_count),
        result,
        scope,
        transaction: transaction.label(),
    })
}

fn counter_control(event: u8, umask: u8) -> u64 {
    u64::from(event)
        | (u64::from(umask) << 8)
        | COUNTER_RESET_BIT
        | COUNTER_OVERFLOW_ENABLE_BIT
        | COUNTER_ENABLE_BIT
}

const fn snb_llc_lookup_state_bits(state: ChaCacheState) -> u16 {
    match state {
        ChaCacheState::I => 0x01,
        ChaCacheState::S => 0x02,
        ChaCacheState::E => 0x04,
        ChaCacheState::M => 0x08,
        ChaCacheState::F => 0x10,
        ChaCacheState::All | ChaCacheState::SfE | ChaCacheState::SfM | ChaCacheState::SfS => 0x00,
    }
}

fn cbo_control_offset(cbo_id: usize, counter_index: usize) -> u64 {
    cbo_unit_offset(CBO_CONTROL_BASE, cbo_id) + counter_index as u64
}

fn cbo_counter_offset(cbo_id: usize, counter_index: usize) -> u64 {
    cbo_unit_offset(CBO_COUNTER_BASE, cbo_id) + counter_index as u64
}

fn cbo_filter0_offset(cbo_id: usize) -> u64 {
    cbo_unit_offset(CBO_FILTER0_BASE, cbo_id)
}

fn cbo_filter1_offset(cbo_id: usize) -> u64 {
    cbo_unit_offset(CBO_FILTER1_BASE, cbo_id)
}

fn cbo_unit_control_offset(cbo_id: usize) -> u64 {
    cbo_unit_offset(CBO_UNIT_CONTROL_BASE, cbo_id)
}

fn cbo_unit_offset(base: u64, cbo_id: usize) -> u64 {
    base + CBO_UNIT_STRIDE * cbo_id as u64
}

fn cha_partition(unit_index: usize, slice: SnbChaMeasurementSlice, unit_count: usize) -> usize {
    let rotated_unit_index = (unit_index + slice.partition_offset) % unit_count;
    rotated_unit_index * slice.partition_width / unit_count
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos =
        crate::metrics::common::DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << 44) - 1)
}

fn add_measurement<K: Ord>(
    entry: std::collections::btree_map::Entry<'_, K, ChaEventMeasurement>,
    measurement: ChaEventMeasurement,
) {
    entry
        .and_modify(|event_measurement| {
            event_measurement.running += measurement.running;
            event_measurement.represented_unit_count = measurement.represented_unit_count;
            event_measurement.value += measurement.value;
        })
        .or_insert(measurement);
}

fn to_skx_scope(scope: HsxUncoreScope) -> UncoreScope {
    UncoreScope {
        die_group_id: 0,
        die_id: 0,
        package_id: scope.package_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_snb_cha_metrics() {
        let scope = HsxUncoreScope { package_id: 0 };
        let metrics = SnbChaMetrics::from_measurements(
            SnbChaArchitecture::Snb,
            BTreeMap::from([(scope, test_measurements(SnbChaArchitecture::Snb))]),
        )
        .unwrap();

        assert_eq!(metrics.scopes[0].frequency_hz, 10_000.0);
        assert_eq!(metrics.llc_lookups.len(), 15);
        assert!(
            metrics
                .llc_lookups
                .iter()
                .any(|metric| metric.state == ChaCacheState::I)
        );
        assert!(
            metrics
                .llc_lookups
                .iter()
                .any(|metric| metric.state == ChaCacheState::F)
        );
        assert_eq!(metrics.llc_lookups[0].bytes_per_second, 320_000.0);
        assert_eq!(metrics.llc_victims.len(), 3);
        assert_eq!(metrics.llc_victims[0].per_second, 400.0);
        assert_eq!(
            metrics.transaction_results.len(),
            SNB_CBO_EXPORTED_TRANSACTIONS.len() * 2
        );
        assert_eq!(metrics.transaction_results[0].inserts_per_second, 3_000.0);
        assert_eq!(
            metrics.transactions.len(),
            SNB_CBO_EXPORTED_TRANSACTIONS.len()
        );
        assert!((metrics.transactions[0].hit_rate - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn computes_ivb_cha_metrics_with_any_llc_lookup() {
        let scope = HsxUncoreScope { package_id: 0 };
        let metrics = SnbChaMetrics::from_measurements(
            SnbChaArchitecture::Ivb,
            BTreeMap::from([(scope, test_measurements(SnbChaArchitecture::Ivb))]),
        )
        .unwrap();

        assert_eq!(metrics.llc_lookups.len(), 20);
        assert!(
            metrics
                .llc_lookups
                .iter()
                .any(|metric| metric.operation == ChaLookupOperation::Any)
        );
    }

    #[test]
    fn derives_snb_hits_from_total_minus_miss() {
        let scope = HsxUncoreScope { package_id: 0 };
        let mut measurements = SnbChaMeasurementAccumulator::new();
        let measurement = SnbChaMeasurement {
            enabled: Duration::from_millis(100),
            represented_unit_count: 1,
            running: Duration::from_millis(100),
            unit_scale: 1.0,
        };

        measurements.add(
            scope,
            SnbChaEventKind::TransactionInsert(SnbTransactionKind::IaDrd, SnbTorCounterKind::Total),
            1_000,
            measurement,
        );
        measurements.add(
            scope,
            SnbChaEventKind::TransactionInsert(SnbTransactionKind::IaDrd, SnbTorCounterKind::Miss),
            250,
            measurement,
        );
        measurements.add(
            scope,
            SnbChaEventKind::TransactionOccupancy(
                SnbTransactionKind::IaDrd,
                SnbTorCounterKind::Total,
            ),
            2_000,
            measurement,
        );
        measurements.add(
            scope,
            SnbChaEventKind::TransactionOccupancy(
                SnbTransactionKind::IaDrd,
                SnbTorCounterKind::Miss,
            ),
            700,
            measurement,
        );
        measurements.add(
            scope,
            SnbChaEventKind::TransactionClockticks(
                SnbTransactionKind::IaDrd,
                SnbTorCounterKind::Total,
            ),
            1_000,
            measurement,
        );
        measurements.add(
            scope,
            SnbChaEventKind::TransactionClockticks(
                SnbTransactionKind::IaDrd,
                SnbTorCounterKind::Miss,
            ),
            1_000,
            measurement,
        );

        let measurements = measurements.into_measurements().remove(&scope).unwrap();

        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                SnbTransactionKind::IaDrd.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            750
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                SnbTransactionKind::IaDrd.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            1_300
        );
    }

    #[test]
    fn encodes_snb_and_ivb_cbo_filters() {
        assert_eq!(SnbChaFilter::none().value(), 0);
        assert_eq!(
            SnbChaFilter::llc_lookup_state(SnbChaArchitecture::Snb, ChaCacheState::M).value(),
            0x08 << 18
        );
        assert_eq!(
            SnbChaFilter::llc_lookup_state(SnbChaArchitecture::Ivb, ChaCacheState::M).value(),
            0x08 << 17
        );
        assert_eq!(
            SnbChaFilter::opcode(SnbChaArchitecture::Snb, 0x182).value(),
            0x182 << 23
        );
        assert_eq!(
            SnbChaFilter::opcode(SnbChaArchitecture::Ivb, 0x182).value(),
            0x182_u64 << 52
        );
        assert_eq!(
            SnbChaFilter::opcode(SnbChaArchitecture::Snb, 0x182).values(),
            SnbChaFilterValues {
                filter0: 0x182 << 23,
                filter1: 0,
            }
        );
        assert_eq!(
            SnbChaFilter::opcode(SnbChaArchitecture::Ivb, 0x182).values(),
            SnbChaFilterValues {
                filter0: 0,
                filter1: 0x182 << 20,
            }
        );
    }

    #[test]
    fn uses_snb_cbo_address_map() {
        assert_eq!(cbo_unit_control_offset(0), 0x0d04);
        assert_eq!(cbo_control_offset(0, 0), 0x0d10);
        assert_eq!(cbo_filter0_offset(0), 0x0d14);
        assert_eq!(cbo_filter1_offset(0), 0x0d1a);
        assert_eq!(cbo_counter_offset(0, 0), 0x0d16);

        assert_eq!(cbo_unit_control_offset(7), 0x0de4);
        assert_eq!(cbo_control_offset(7, 3), 0x0df3);
        assert_eq!(cbo_filter1_offset(7), 0x0dfa);
        assert_eq!(cbo_counter_offset(7, 3), 0x0df9);
    }

    #[test]
    fn uses_documented_snb_ivb_event_encodings() {
        let group = SnbChaEventGroup::transaction(
            SnbChaArchitecture::Snb,
            SnbTransactionKind::IaDrd,
            SnbTorCounterKind::Miss,
        );

        assert_eq!(
            group.events,
            [
                SnbChaEventSpec::new(
                    SnbChaEventKind::TransactionOccupancy(
                        SnbTransactionKind::IaDrd,
                        SnbTorCounterKind::Miss,
                    ),
                    0x36,
                    0x03,
                ),
                SnbChaEventSpec::new(
                    SnbChaEventKind::TransactionInsert(
                        SnbTransactionKind::IaDrd,
                        SnbTorCounterKind::Miss,
                    ),
                    0x35,
                    0x03,
                ),
                SnbChaEventSpec::transaction_clockticks(
                    SnbTransactionKind::IaDrd,
                    SnbTorCounterKind::Miss,
                ),
                SnbChaEventSpec::unused(),
            ]
        );
    }

    #[test]
    fn programs_snb_tor_occupancy_only_on_counter_zero() {
        for architecture in [SnbChaArchitecture::Snb, SnbChaArchitecture::Ivb] {
            for group in architecture.event_groups() {
                for (counter, event) in group.events.iter().enumerate() {
                    if event.event == TOR_OCCUPANCY_EVENT {
                        assert_eq!(counter, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn schedules_short_interval_once_per_group() {
        let collector = test_collector(SnbChaArchitecture::Snb);
        let slices = collector.schedule(Duration::from_millis(100));

        assert_eq!(slices.len(), SNB_CBO_EVENT_GROUP_COUNT);
        assert_eq!(slices[0].groups[0], Some(SNB_CBO_EVENT_GROUPS[0]));
    }

    fn measurement(
        kind: ChaEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (ChaEventKind, ChaEventMeasurement) {
        (
            kind,
            ChaEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                represented_unit_count: 1,
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn test_collector(architecture: SnbChaArchitecture) -> SnbChaCollector {
        SnbChaCollector {
            architecture,
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages: vec![SnbChaPackage::new(
                HsxUncoreScope { package_id: 0 },
                (0..CHA_COUNTER_COUNT)
                    .map(|id| SnbChaUnit {
                        architecture,
                        cpu: 0,
                        id,
                    })
                    .collect(),
            )],
        }
    }

    fn test_measurements(
        architecture: SnbChaArchitecture,
    ) -> BTreeMap<ChaEventKind, ChaEventMeasurement> {
        let mut measurements =
            BTreeMap::from([measurement(ChaEventKind::EvictionClockticks, 1_000, 100)]);

        for state in SNB_IVB_LLC_LOOKUP_STATES {
            measurements.extend([
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Read),
                    500,
                    100,
                ),
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Write),
                    200,
                    100,
                ),
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::RemoteSnoop),
                    100,
                    100,
                ),
            ]);

            if architecture == SnbChaArchitecture::Ivb {
                measurements.insert(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                    ChaEventMeasurement {
                        enabled: Duration::from_millis(100),
                        represented_unit_count: 1,
                        running: Duration::from_millis(100),
                        value: 800,
                    },
                );
            }
        }

        for (state, value) in [
            (ChaCacheState::M, 40),
            (ChaCacheState::E, 20),
            (ChaCacheState::S, 30),
        ] {
            measurements.insert(
                ChaEventKind::LlcVictim(state),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value,
                },
            );
        }

        for transaction in SNB_CBO_EXPORTED_TRANSACTIONS {
            let transaction = transaction.label();

            measurements.insert(
                ChaEventKind::TransactionInsert(transaction, ChaTransactionResult::Hit),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 300,
                },
            );
            measurements.insert(
                ChaEventKind::TransactionOccupancy(transaction, ChaTransactionResult::Hit),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 400,
                },
            );
            measurements.insert(
                ChaEventKind::TransactionClockticks(transaction, ChaTransactionResult::Hit),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 1_000,
                },
            );
            measurements.insert(
                ChaEventKind::TransactionInsert(transaction, ChaTransactionResult::Miss),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 600,
                },
            );
            measurements.insert(
                ChaEventKind::TransactionOccupancy(transaction, ChaTransactionResult::Miss),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 800,
                },
            );
            measurements.insert(
                ChaEventKind::TransactionClockticks(transaction, ChaTransactionResult::Miss),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value: 1_000,
                },
            );
        }

        measurements
    }
}
