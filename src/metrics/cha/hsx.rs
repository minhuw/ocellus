use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::arch::skx::pmon;
use crate::metal::msr::Msr;
use crate::metal::topology::TopologyLevelKind;
use crate::metrics::cha::{
    CHA_COUNTER_COUNT, ChaCacheState, ChaEventKind, ChaEventMeasurement, ChaLlcLookupMetrics,
    ChaLlcVictimMetrics, ChaLookupOperation, ChaMultiplexMode, ChaScopeMetrics,
    ChaTransactionLabel, ChaTransactionMetrics, ChaTransactionResult, ChaTransactionResultMetrics,
    bytes_per_second, linux_uncore_unit_ids, llc_victim_metrics, required_measurement,
    scale_measurement_value,
};
use crate::metrics::uncore::hsx::{self, HsxUncoreScope};
use crate::metrics::uncore::skx::{UncoreScope, queue_residency_seconds, ratio};

const HSX_CBO_EVENT_GROUP_COUNT: usize = 21;
const HSX_CBO_EXPORTED_TRANSACTION_COUNT: usize = 7;
const HSX_MAX_CBO_COUNT: usize = 32;

const CBO_COUNTER_BASE: u64 = 0x0e08;
const CBO_CONTROL_BASE: u64 = 0x0e01;
const CBO_FILTER0_BASE: u64 = 0x0e05;
const CBO_FILTER1_BASE: u64 = 0x0e06;
const CBO_UNIT_CONTROL_BASE: u64 = 0x0e00;
const CBO_UNIT_STRIDE: u64 = 0x10;

const CBO_FILTER0_STATE_SHIFT: u32 = 17;
const CBO_FILTER0_THREAD_ID_SHIFT: u32 = 0;
const CBO_FILTER1_OPCODE_SHIFT: u32 = 20;
const CBO_PCIE_REQUEST_TID: u16 = 0x3e;
const CBO_TID_ENABLE_BIT: u32 = 1 << 19;

const TOR_INSERTS_EVENT: u8 = 0x35;
const TOR_OCCUPANCY_EVENT: u8 = 0x36;
const TOR_OPCODE_UMASK: u8 = 0x01;
const TOR_MISS_OPCODE_UMASK: u8 = 0x03;

const HSX_LLC_LOOKUP_STATES: [ChaCacheState; 5] = [
    ChaCacheState::I,
    ChaCacheState::S,
    ChaCacheState::E,
    ChaCacheState::M,
    ChaCacheState::F,
];

const HSX_CBO_EXPORTED_TRANSACTIONS: [HsxTransactionKind; HSX_CBO_EXPORTED_TRANSACTION_COUNT] = [
    HsxTransactionKind::IoPciRdCur,
    HsxTransactionKind::PciRfo,
    HsxTransactionKind::PciItoM,
    HsxTransactionKind::IaRfo,
    HsxTransactionKind::IaCrd,
    HsxTransactionKind::IaDrd,
    HsxTransactionKind::IaItoM,
];

const HSX_CBO_EVENT_GROUPS: [HsxChaEventGroup; HSX_CBO_EVENT_GROUP_COUNT] = [
    HsxChaEventGroup::frequency(),
    HsxChaEventGroup::llc_lookup(ChaCacheState::I),
    HsxChaEventGroup::llc_lookup(ChaCacheState::S),
    HsxChaEventGroup::llc_lookup(ChaCacheState::E),
    HsxChaEventGroup::llc_lookup(ChaCacheState::M),
    HsxChaEventGroup::llc_lookup(ChaCacheState::F),
    HsxChaEventGroup::transaction(HsxTransactionKind::IoPciRdCur, 0x19e, false, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::IoPciRdCur, 0x19e, false, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::PciRfo, 0x180, true, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::PciRfo, 0x180, true, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::PciItoM, 0x1c8, true, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::PciItoM, 0x1c8, true, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::TotalRfo, 0x180, false, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::TotalRfo, 0x180, false, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::IaCrd, 0x181, false, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::IaCrd, 0x181, false, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::IaDrd, 0x182, false, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::IaDrd, 0x182, false, true),
    HsxChaEventGroup::transaction(HsxTransactionKind::TotalItoM, 0x1c8, false, false),
    HsxChaEventGroup::transaction(HsxTransactionKind::TotalItoM, 0x1c8, false, true),
    HsxChaEventGroup::llc_victims(),
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct HsxChaMetrics {
    pub llc_lookups: Vec<ChaLlcLookupMetrics>,
    pub llc_victims: Vec<ChaLlcVictimMetrics>,
    pub scopes: Vec<ChaScopeMetrics>,
    pub transaction_results: Vec<ChaTransactionResultMetrics>,
    pub transactions: Vec<ChaTransactionMetrics>,
}

impl HsxChaMetrics {
    fn from_measurements(
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

            llc_lookups.extend(hsx_llc_lookup_metrics(scope, &scope_measurements)?);
            llc_victims.extend(llc_victim_metrics(
                scope,
                &scope_measurements,
                &[
                    ChaCacheState::M,
                    ChaCacheState::E,
                    ChaCacheState::S,
                    ChaCacheState::F,
                ],
            )?);

            let transaction_scope_metrics = hsx_transaction_metrics(scope, &scope_measurements)?;
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
pub struct HsxChaPrometheusMetrics {
    frequency_hz: Family<HsxChaScopeLabels, Gauge<f64, AtomicU64>>,
    llc_lookup_bytes_per_second: Family<HsxChaLlcLookupLabels, Gauge<f64, AtomicU64>>,
    llc_victims_per_second: Family<HsxChaStateLabels, Gauge<f64, AtomicU64>>,
    transaction_bandwidth_bytes_per_second: Family<HsxChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_hit_rate: Family<HsxChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_latency_seconds: Family<HsxChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_result_bandwidth_bytes_per_second:
        Family<HsxChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_inserts_per_second:
        Family<HsxChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_latency_seconds:
        Family<HsxChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_occupancy_entries:
        Family<HsxChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
}

impl HsxChaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            frequency_hz: Family::<HsxChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            llc_lookup_bytes_per_second:
                Family::<HsxChaLlcLookupLabels, Gauge<f64, AtomicU64>>::default(),
            llc_victims_per_second: Family::<HsxChaStateLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_bandwidth_bytes_per_second: Family::<
                HsxChaTransactionLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_hit_rate: Family::<HsxChaTransactionLabels, Gauge<f64, AtomicU64>>::default(
            ),
            transaction_latency_seconds:
                Family::<HsxChaTransactionLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_result_bandwidth_bytes_per_second: Family::<
                HsxChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_inserts_per_second: Family::<
                HsxChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_latency_seconds: Family::<
                HsxChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_occupancy_entries: Family::<
                HsxChaTransactionResultLabels,
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

    pub fn update(&self, metrics: HsxChaMetrics) {
        for scope in metrics.scopes {
            self.frequency_hz
                .get_or_create(&HsxChaScopeLabels::from_scope(scope.scope))
                .set(scope.frequency_hz);
        }

        for metric in metrics.llc_lookups {
            self.llc_lookup_bytes_per_second
                .get_or_create(&HsxChaLlcLookupLabels::from_metric(metric))
                .set(metric.bytes_per_second);
        }

        for metric in metrics.llc_victims {
            self.llc_victims_per_second
                .get_or_create(&HsxChaStateLabels::from_llc_victim(metric))
                .set(metric.per_second);
        }

        for metric in metrics.transaction_results {
            let labels = HsxChaTransactionResultLabels::from_metric(metric);

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
            let labels = HsxChaTransactionLabels::from_metric(metric);

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
struct HsxChaScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl HsxChaScopeLabels {
    fn from_scope(scope: UncoreScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct HsxChaStateLabels {
    die: String,
    die_group: String,
    package: String,
    state: String,
}

impl HsxChaStateLabels {
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
struct HsxChaLlcLookupLabels {
    die: String,
    die_group: String,
    operation: String,
    package: String,
    state: String,
}

impl HsxChaLlcLookupLabels {
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
struct HsxChaTransactionLabels {
    die: String,
    die_group: String,
    package: String,
    transaction: String,
}

impl HsxChaTransactionLabels {
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
struct HsxChaTransactionResultLabels {
    die: String,
    die_group: String,
    package: String,
    result: String,
    transaction: String,
}

impl HsxChaTransactionResultLabels {
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
pub struct HsxChaCollector {
    multiplex_mode: ChaMultiplexMode,
    next_group: usize,
    next_partition_offset: usize,
    packages: Vec<HsxChaPackage>,
}

impl HsxChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages,
        })
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        if let Err(error) = self.validate_multiplex_mode(mode) {
            eprintln!("ocellus: disabling Haswell/Broadwell CHA spatial multiplexing: {error}");
            self.multiplex_mode = ChaMultiplexMode::Temporal;
            return;
        }

        self.multiplex_mode = mode;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxChaMetrics, String> {
        if interval.is_zero() {
            return Err("Haswell/Broadwell CHA measure interval must be non-zero".to_string());
        }

        let mut measurements = HsxChaMeasurementAccumulator::new();
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

        HsxChaMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_schedule(&mut self, measured_slice_count: usize) {
        self.next_group =
            (self.next_group + self.multiplex_mode.partitions()) % HSX_CBO_EVENT_GROUPS.len();
        self.next_partition_offset = self
            .next_partition_offset
            .wrapping_add(measured_slice_count);
    }

    #[cfg(test)]
    fn rotate_group(&mut self) {
        self.rotate_schedule(1);
    }

    fn schedule(&self, interval: Duration) -> Vec<HsxChaMeasurementSlice> {
        let group_count = HSX_CBO_EVENT_GROUPS.len();
        let partitions = self.multiplex_mode.partitions();
        let slice_count_per_round = group_count.div_ceil(partitions);
        let round_count = measurement_round_count(interval, slice_count_per_round);
        let slice_count = slice_count_per_round * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for round in 0..round_count {
            for slice_index in 0..slice_count_per_round {
                let first_group_offset = slice_index * partitions;
                let groups = self.slice_groups(first_group_offset, partitions, group_count);

                slices.push(HsxChaMeasurementSlice {
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
        first_group_offset: usize,
        partitions: usize,
        group_count: usize,
    ) -> [Option<HsxChaEventGroup>; CHA_COUNTER_COUNT] {
        let mut groups = [None; CHA_COUNTER_COUNT];

        for (partition, group) in groups.iter_mut().enumerate().take(partitions) {
            let group_offset = first_group_offset + partition;
            if group_offset < group_count {
                let group_index = (self.next_group + group_offset) % group_count;
                *group = Some(HSX_CBO_EVENT_GROUPS[group_index]);
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
struct HsxChaUnitReading {
    counters: [u64; CHA_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct HsxChaUnit {
    cpu: u32,
    id: usize,
}

impl HsxChaUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE))
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE_AND_RESET))
    }

    fn program(self, group: HsxChaEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;

        msr.write(
            cbo_filter0_offset(self.id),
            u64::from(group.filter0.value()),
        )?;
        msr.write(
            cbo_filter1_offset(self.id),
            u64::from(group.filter1.value()),
        )?;

        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(
                cbo_control_offset(self.id, counter_index),
                u64::from(counter_control(
                    event.event,
                    event.umask,
                    event.thread_filter,
                )),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<HsxChaUnitReading, String> {
        Ok(HsxChaUnitReading {
            counters: [
                self.read_counter(0).map(hsx::mask_counter)?,
                self.read_counter(1).map(hsx::mask_counter)?,
                self.read_counter(2).map(hsx::mask_counter)?,
                self.read_counter(3).map(hsx::mask_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_UNFREEZE))
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
            u64::from(pmon::UNIT_FREEZE),
        )?;
        msr.write(cbo_filter0_offset(self.id), 0)?;
        msr.write(cbo_filter1_offset(self.id), 0)?;
        msr.write(cbo_control_offset(self.id, 0), 0)?;
        Ok(())
    }

    fn probe_group(self, group: HsxChaEventGroup) -> Result<(), String> {
        self.freeze_and_reset()?;
        self.program(group)
    }
}

#[derive(Debug)]
struct HsxChaPackage {
    scope: HsxUncoreScope,
    units: Vec<HsxChaUnit>,
}

impl HsxChaPackage {
    fn new(scope: HsxUncoreScope, units: Vec<HsxChaUnit>) -> Self {
        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxChaEventSpec {
    event: u8,
    kind: HsxChaEventKind,
    thread_filter: bool,
    umask: u8,
}

impl HsxChaEventSpec {
    const fn new(kind: HsxChaEventKind, event: u8, umask: u8, thread_filter: bool) -> Self {
        Self {
            event,
            kind,
            thread_filter,
            umask,
        }
    }

    const fn clockticks() -> Self {
        Self::new(HsxChaEventKind::Clockticks, 0x00, 0x00, false)
    }

    const fn transaction_clockticks(
        transaction: HsxTransactionKind,
        counter_kind: HsxTorCounterKind,
    ) -> Self {
        Self::new(
            HsxChaEventKind::TransactionClockticks(transaction, counter_kind),
            0x00,
            0x00,
            false,
        )
    }

    const fn unused() -> Self {
        Self::new(HsxChaEventKind::Unused, 0x00, 0x00, false)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxChaEventGroup {
    events: [HsxChaEventSpec; CHA_COUNTER_COUNT],
    filter0: HsxChaFilter0,
    filter1: HsxChaFilter1,
}

impl HsxChaEventGroup {
    const fn frequency() -> Self {
        Self {
            events: [
                HsxChaEventSpec::clockticks(),
                HsxChaEventSpec::unused(),
                HsxChaEventSpec::unused(),
                HsxChaEventSpec::unused(),
            ],
            filter0: HsxChaFilter0::none(),
            filter1: HsxChaFilter1::none(),
        }
    }

    const fn llc_lookup(state: ChaCacheState) -> Self {
        Self {
            events: [
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcLookup(state, ChaLookupOperation::Read),
                    0x34,
                    0x03,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcLookup(state, ChaLookupOperation::Write),
                    0x34,
                    0x05,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcLookup(state, ChaLookupOperation::RemoteSnoop),
                    0x34,
                    0x09,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                    0x34,
                    0x11,
                    false,
                ),
            ],
            filter0: HsxChaFilter0::llc_lookup_state(state),
            filter1: HsxChaFilter1::none(),
        }
    }

    const fn llc_victims() -> Self {
        Self {
            events: [
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcVictim(ChaCacheState::M),
                    0x37,
                    0x01,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcVictim(ChaCacheState::E),
                    0x37,
                    0x02,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcVictim(ChaCacheState::S),
                    0x37,
                    0x04,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::LlcVictim(ChaCacheState::F),
                    0x37,
                    0x08,
                    false,
                ),
            ],
            filter0: HsxChaFilter0::none(),
            filter1: HsxChaFilter1::none(),
        }
    }

    const fn transaction(
        transaction: HsxTransactionKind,
        opcode: u16,
        thread_filter: bool,
        miss: bool,
    ) -> Self {
        let result = if miss {
            HsxTorCounterKind::Miss
        } else {
            HsxTorCounterKind::Total
        };
        let umask = if miss {
            TOR_MISS_OPCODE_UMASK
        } else {
            TOR_OPCODE_UMASK
        };

        Self {
            events: [
                HsxChaEventSpec::new(
                    HsxChaEventKind::TransactionOccupancy(transaction, result),
                    TOR_OCCUPANCY_EVENT,
                    umask,
                    thread_filter,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::TransactionInsert(transaction, result),
                    TOR_INSERTS_EVENT,
                    umask,
                    thread_filter,
                ),
                HsxChaEventSpec::transaction_clockticks(transaction, result),
                HsxChaEventSpec::unused(),
            ],
            filter0: HsxChaFilter0::thread_filter(thread_filter),
            filter1: HsxChaFilter1::opcode(opcode),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxChaFilter0 {
    state: u16,
    thread_id: u16,
}

impl HsxChaFilter0 {
    const fn none() -> Self {
        Self {
            state: 0,
            thread_id: 0,
        }
    }

    const fn llc_lookup_state(state: ChaCacheState) -> Self {
        Self {
            state: hsx_llc_lookup_state_bits(state),
            thread_id: 0,
        }
    }

    const fn thread_filter(enabled: bool) -> Self {
        Self {
            state: 0,
            thread_id: if enabled { CBO_PCIE_REQUEST_TID } else { 0 },
        }
    }

    const fn value(self) -> u32 {
        ((self.state as u32) << CBO_FILTER0_STATE_SHIFT)
            | ((self.thread_id as u32) << CBO_FILTER0_THREAD_ID_SHIFT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxChaFilter1 {
    opcode: u16,
}

impl HsxChaFilter1 {
    const fn none() -> Self {
        Self { opcode: 0 }
    }

    const fn opcode(opcode: u16) -> Self {
        Self { opcode }
    }

    const fn value(self) -> u32 {
        (self.opcode as u32) << CBO_FILTER1_OPCODE_SHIFT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HsxChaEventKind {
    Clockticks,
    LlcLookup(ChaCacheState, ChaLookupOperation),
    LlcVictim(ChaCacheState),
    TransactionClockticks(HsxTransactionKind, HsxTorCounterKind),
    TransactionInsert(HsxTransactionKind, HsxTorCounterKind),
    TransactionOccupancy(HsxTransactionKind, HsxTorCounterKind),
    Unused,
}

impl HsxChaEventKind {
    const fn is_clockticks(self) -> bool {
        matches!(self, Self::Clockticks | Self::TransactionClockticks(_, _))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HsxMeasurementKind {
    Exported(ChaEventKind),
    TransactionClockticks(HsxTransactionKind, HsxTorCounterKind),
    TransactionInsert(HsxTransactionKind, HsxTorCounterKind),
    TransactionOccupancy(HsxTransactionKind, HsxTorCounterKind),
}

impl HsxMeasurementKind {
    const fn transaction_insert(
        transaction: HsxTransactionKind,
        counter_kind: HsxTorCounterKind,
    ) -> Self {
        Self::TransactionInsert(transaction, counter_kind)
    }

    const fn transaction_clockticks(
        transaction: HsxTransactionKind,
        counter_kind: HsxTorCounterKind,
    ) -> Self {
        Self::TransactionClockticks(transaction, counter_kind)
    }

    const fn transaction_occupancy(
        transaction: HsxTransactionKind,
        counter_kind: HsxTorCounterKind,
    ) -> Self {
        Self::TransactionOccupancy(transaction, counter_kind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxTorCounterKind {
    Miss,
    Total,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxTransactionKind {
    IaCrd,
    IaDrd,
    IaItoM,
    IaRfo,
    IoPciRdCur,
    PciItoM,
    PciRfo,
    TotalItoM,
    TotalRfo,
}

impl HsxTransactionKind {
    const fn label(self) -> ChaTransactionLabel {
        match self {
            Self::IaCrd => ChaTransactionLabel::new("ia_crd"),
            Self::IaDrd => ChaTransactionLabel::new("ia_drd"),
            Self::IaItoM => ChaTransactionLabel::new("ia_itom"),
            Self::IaRfo => ChaTransactionLabel::new("ia_rfo"),
            Self::IoPciRdCur => ChaTransactionLabel::new("io_pcirdcur"),
            Self::PciItoM => ChaTransactionLabel::new("pcie_itom"),
            Self::PciRfo => ChaTransactionLabel::new("pcie_rfo"),
            Self::TotalItoM => ChaTransactionLabel::new("total_itom"),
            Self::TotalRfo => ChaTransactionLabel::new("total_rfo"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxChaMeasurement {
    enabled: Duration,
    represented_unit_count: u64,
    running: Duration,
    unit_scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxChaMeasurementSlice {
    duration: Duration,
    groups: [Option<HsxChaEventGroup>; CHA_COUNTER_COUNT],
    partition_offset: usize,
    partition_width: usize,
}

#[derive(Debug, Default)]
struct HsxChaMeasurementAccumulator {
    exported_measurements: BTreeMap<HsxUncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    transaction_clockticks:
        BTreeMap<(HsxUncoreScope, HsxTransactionKind, HsxTorCounterKind), ChaEventMeasurement>,
    transaction_inserts:
        BTreeMap<(HsxUncoreScope, HsxTransactionKind, HsxTorCounterKind), ChaEventMeasurement>,
    transaction_occupancy:
        BTreeMap<(HsxUncoreScope, HsxTransactionKind, HsxTorCounterKind), ChaEventMeasurement>,
}

impl HsxChaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: HsxUncoreScope,
        kind: HsxChaEventKind,
        value: u64,
        measurement: HsxChaMeasurement,
    ) {
        if kind == HsxChaEventKind::Unused {
            return;
        }

        let scaled_value = if kind.is_clockticks() {
            value
        } else {
            (value as f64 * measurement.unit_scale) as u64
        };

        let kind = match kind {
            HsxChaEventKind::Clockticks => {
                HsxMeasurementKind::Exported(ChaEventKind::EvictionClockticks)
            }
            HsxChaEventKind::LlcLookup(state, operation) => {
                HsxMeasurementKind::Exported(ChaEventKind::LlcLookup(state, operation))
            }
            HsxChaEventKind::LlcVictim(state) => {
                HsxMeasurementKind::Exported(ChaEventKind::LlcVictim(state))
            }
            HsxChaEventKind::TransactionClockticks(transaction, result) => {
                HsxMeasurementKind::transaction_clockticks(transaction, result)
            }
            HsxChaEventKind::TransactionInsert(transaction, result) => {
                HsxMeasurementKind::transaction_insert(transaction, result)
            }
            HsxChaEventKind::TransactionOccupancy(transaction, result) => {
                HsxMeasurementKind::transaction_occupancy(transaction, result)
            }
            HsxChaEventKind::Unused => unreachable!(),
        };

        self.add_measurement(scope, kind, scaled_value, measurement);
    }

    fn into_measurements(
        mut self,
    ) -> BTreeMap<HsxUncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>> {
        self.derive_ia_transaction_measurements();
        self.export_transaction_measurements();

        self.exported_measurements
    }

    fn derive_ia_transaction_measurements(&mut self) {
        for counter_kind in [HsxTorCounterKind::Total, HsxTorCounterKind::Miss] {
            derive_transaction_clockticks(
                &mut self.transaction_clockticks,
                HsxTransactionKind::TotalRfo,
                HsxTransactionKind::IaRfo,
                counter_kind,
            );
            derive_transaction_counts(
                &mut self.transaction_inserts,
                HsxTransactionKind::TotalRfo,
                HsxTransactionKind::PciRfo,
                HsxTransactionKind::IaRfo,
                counter_kind,
            );
            derive_transaction_counts(
                &mut self.transaction_occupancy,
                HsxTransactionKind::TotalRfo,
                HsxTransactionKind::PciRfo,
                HsxTransactionKind::IaRfo,
                counter_kind,
            );
            derive_transaction_clockticks(
                &mut self.transaction_clockticks,
                HsxTransactionKind::TotalItoM,
                HsxTransactionKind::IaItoM,
                counter_kind,
            );
            derive_transaction_counts(
                &mut self.transaction_inserts,
                HsxTransactionKind::TotalItoM,
                HsxTransactionKind::PciItoM,
                HsxTransactionKind::IaItoM,
                counter_kind,
            );
            derive_transaction_counts(
                &mut self.transaction_occupancy,
                HsxTransactionKind::TotalItoM,
                HsxTransactionKind::PciItoM,
                HsxTransactionKind::IaItoM,
                counter_kind,
            );
        }
    }

    fn export_transaction_measurements(&mut self) {
        let mut transaction_scopes = Vec::new();

        for &(scope, transaction, counter_kind) in self.transaction_inserts.keys() {
            if counter_kind == HsxTorCounterKind::Total
                && HSX_CBO_EXPORTED_TRANSACTIONS.contains(&transaction)
            {
                transaction_scopes.push((scope, transaction));
            }
        }

        for (scope, transaction) in transaction_scopes {
            let Some(total_clockticks) = self
                .transaction_clockticks
                .get(&(scope, transaction, HsxTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_clockticks) = self
                .transaction_clockticks
                .get(&(scope, transaction, HsxTorCounterKind::Miss))
                .copied()
            else {
                continue;
            };
            let Some(total_inserts) = self
                .transaction_inserts
                .get(&(scope, transaction, HsxTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_inserts) = self
                .transaction_inserts
                .get(&(scope, transaction, HsxTorCounterKind::Miss))
                .copied()
            else {
                continue;
            };
            let Some(total_occupancy) = self
                .transaction_occupancy
                .get(&(scope, transaction, HsxTorCounterKind::Total))
                .copied()
            else {
                continue;
            };
            let Some(miss_occupancy) = self
                .transaction_occupancy
                .get(&(scope, transaction, HsxTorCounterKind::Miss))
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
        transaction: HsxTransactionKind,
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
        kind: HsxMeasurementKind,
        value: u64,
        measurement: HsxChaMeasurement,
    ) {
        let event_measurement = ChaEventMeasurement {
            enabled: measurement.enabled,
            represented_unit_count: measurement.represented_unit_count,
            running: measurement.running,
            value,
        };

        match kind {
            HsxMeasurementKind::Exported(kind) => add_measurement(
                self.exported_measurements
                    .entry(scope)
                    .or_default()
                    .entry(kind),
                event_measurement,
            ),
            HsxMeasurementKind::TransactionClockticks(transaction, counter_kind) => {
                add_measurement(
                    self.transaction_clockticks
                        .entry((scope, transaction, counter_kind)),
                    event_measurement,
                )
            }
            HsxMeasurementKind::TransactionInsert(transaction, counter_kind) => add_measurement(
                self.transaction_inserts
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            HsxMeasurementKind::TransactionOccupancy(transaction, counter_kind) => add_measurement(
                self.transaction_occupancy
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
        }
    }
}

fn derive_transaction_clockticks(
    measurements: &mut BTreeMap<
        (HsxUncoreScope, HsxTransactionKind, HsxTorCounterKind),
        ChaEventMeasurement,
    >,
    total_transaction: HsxTransactionKind,
    derived_transaction: HsxTransactionKind,
    counter_kind: HsxTorCounterKind,
) {
    let totals: Vec<_> = measurements
        .iter()
        .filter_map(|(&(scope, transaction, kind), &measurement)| {
            if transaction == total_transaction && kind == counter_kind {
                Some((scope, measurement))
            } else {
                None
            }
        })
        .collect();

    for (scope, total) in totals {
        measurements.insert((scope, derived_transaction, counter_kind), total);
    }
}

fn derive_transaction_counts(
    measurements: &mut BTreeMap<
        (HsxUncoreScope, HsxTransactionKind, HsxTorCounterKind),
        ChaEventMeasurement,
    >,
    total_transaction: HsxTransactionKind,
    excluded_transaction: HsxTransactionKind,
    derived_transaction: HsxTransactionKind,
    counter_kind: HsxTorCounterKind,
) {
    let totals: Vec<_> = measurements
        .iter()
        .filter_map(|(&(scope, transaction, kind), &measurement)| {
            if transaction == total_transaction && kind == counter_kind {
                Some((scope, measurement))
            } else {
                None
            }
        })
        .collect();

    for (scope, total) in totals {
        let Some(excluded) = measurements
            .get(&(scope, excluded_transaction, counter_kind))
            .copied()
        else {
            continue;
        };

        measurements.insert(
            (scope, derived_transaction, counter_kind),
            derived_measurement(total, excluded),
        );
    }
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

#[derive(Debug)]
struct HsxTransactionScopeMetrics {
    results: Vec<ChaTransactionResultMetrics>,
    totals: Vec<ChaTransactionMetrics>,
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<HsxChaPackage>, String> {
    if !matches!(
        model,
        IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon
    ) {
        return Err(format!(
            "Haswell/Broadwell CHA collection is not supported for {model:?}"
        ));
    }

    let mut packages = Vec::new();

    for (scope, cpu) in package_leaders()? {
        packages.push(HsxChaPackage::new(scope, discover_units(cpu)?));
    }

    if packages.is_empty() {
        return Err("failed to discover any Haswell/Broadwell CHA packages".to_string());
    }

    Ok(packages)
}

fn package_leaders() -> Result<Vec<(HsxUncoreScope, u32)>, String> {
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
        return Err("failed to discover any Haswell/Broadwell CHA package leaders".to_string());
    }

    Ok(leaders.into_iter().collect())
}

fn discover_units(cpu: u32) -> Result<Vec<HsxChaUnit>, String> {
    let msr = Msr::open_readonly(cpu)?;
    let mut units = Vec::new();

    for id in hsx_cbo_unit_ids()? {
        if msr.read(cbo_unit_control_offset(id)).is_ok()
            && msr.read(cbo_counter_offset(id, 0)).is_ok()
            && msr.read(cbo_control_offset(id, 0)).is_ok()
            && msr.read(cbo_filter0_offset(id)).is_ok()
            && msr.read(cbo_filter1_offset(id)).is_ok()
        {
            let unit = HsxChaUnit { cpu, id };
            if let Err(error) = unit.probe_writable() {
                eprintln!("ocellus: skipping Haswell/Broadwell CBo {id} on CPU {cpu}: {error}");
                continue;
            }
            if let Err(error) = probe_unit_groups(unit) {
                eprintln!("ocellus: skipping Haswell/Broadwell CBo {id} on CPU {cpu}: {error}");
                continue;
            }

            units.push(unit);
        }
    }

    if units.is_empty() {
        return Err(format!(
            "failed to discover any Haswell/Broadwell CBo units on CPU {cpu}"
        ));
    }

    Ok(units)
}

fn hsx_cbo_unit_ids() -> Result<Vec<usize>, String> {
    match linux_uncore_unit_ids(&["uncore_cbox_"], HSX_MAX_CBO_COUNT) {
        Ok(ids) => Ok(ids),
        Err(error) => {
            eprintln!(
                "ocellus: falling back to MSR probing for Haswell/Broadwell CBo discovery: {error}"
            );
            Ok((0..HSX_MAX_CBO_COUNT).collect())
        }
    }
}

fn program_packages(
    packages: &[HsxChaPackage],
    slice: HsxChaMeasurementSlice,
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

fn probe_unit_groups(unit: HsxChaUnit) -> Result<(), String> {
    for group in HSX_CBO_EVENT_GROUPS {
        unit.probe_group(group)?;
    }

    Ok(())
}

fn read_packages(
    packages: &[HsxChaPackage],
    enabled: Duration,
    running: Duration,
    slice: HsxChaMeasurementSlice,
    measurements: &mut HsxChaMeasurementAccumulator,
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
                    HsxChaMeasurement {
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

fn freeze_packages(packages: &[HsxChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn unfreeze_packages(packages: &[HsxChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn probe_writable_msrs(packages: &[HsxChaPackage]) -> Result<(), String> {
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

fn hsx_llc_lookup_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaLlcLookupMetrics>, String> {
    let mut metrics = Vec::new();

    for state in HSX_LLC_LOOKUP_STATES {
        for operation in [
            ChaLookupOperation::Read,
            ChaLookupOperation::Write,
            ChaLookupOperation::RemoteSnoop,
            ChaLookupOperation::Any,
        ] {
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

fn hsx_transaction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<HsxTransactionScopeMetrics, String> {
    let mut results = Vec::new();
    let mut totals = Vec::new();

    for transaction in HSX_CBO_EXPORTED_TRANSACTIONS {
        let hit = hsx_transaction_result_metrics(
            scope,
            measurements,
            transaction,
            ChaTransactionResult::Hit,
        )?;
        let miss = hsx_transaction_result_metrics(
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

    Ok(HsxTransactionScopeMetrics { results, totals })
}

fn hsx_transaction_result_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    transaction: HsxTransactionKind,
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

fn counter_control(event: u8, umask: u8, thread_filter: bool) -> u32 {
    pmon::counter_control(event, umask, true) | if thread_filter { CBO_TID_ENABLE_BIT } else { 0 }
}

const fn hsx_llc_lookup_state_bits(state: ChaCacheState) -> u16 {
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

fn cha_partition(unit_index: usize, slice: HsxChaMeasurementSlice, unit_count: usize) -> usize {
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
    fn computes_hsx_cha_metrics() {
        let scope = HsxUncoreScope { package_id: 0 };
        let metrics =
            HsxChaMetrics::from_measurements(BTreeMap::from([(scope, test_measurements())]))
                .unwrap();

        assert_eq!(metrics.scopes[0].frequency_hz, 10_000.0);
        assert_eq!(metrics.llc_lookups.len(), 20);
        assert_eq!(metrics.llc_lookups[0].bytes_per_second, 320_000.0);
        assert_eq!(metrics.llc_victims.len(), 4);
        assert_eq!(metrics.llc_victims[0].per_second, 400.0);
        assert_eq!(
            metrics.transaction_results.len(),
            HSX_CBO_EXPORTED_TRANSACTIONS.len() * 2
        );
        assert_eq!(metrics.transaction_results[0].inserts_per_second, 3_000.0);
        assert_eq!(
            metrics.transactions.len(),
            HSX_CBO_EXPORTED_TRANSACTIONS.len()
        );
        assert!((metrics.transactions[0].hit_rate - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn derives_ia_transactions_from_total_and_pcie_transactions() {
        let scope = HsxUncoreScope { package_id: 0 };
        let mut measurements = HsxChaMeasurementAccumulator::new();
        let measurement = HsxChaMeasurement {
            enabled: Duration::from_millis(100),
            represented_unit_count: 1,
            running: Duration::from_millis(100),
            unit_scale: 1.0,
        };

        for (transaction, total_insert, miss_insert, total_occupancy, miss_occupancy) in [
            (HsxTransactionKind::TotalRfo, 1_000, 600, 1_500, 900),
            (HsxTransactionKind::PciRfo, 250, 100, 500, 400),
            (HsxTransactionKind::TotalItoM, 2_000, 1_200, 3_000, 1_800),
            (HsxTransactionKind::PciItoM, 500, 200, 700, 300),
        ] {
            measurements.add(
                scope,
                HsxChaEventKind::TransactionInsert(transaction, HsxTorCounterKind::Total),
                total_insert,
                measurement,
            );
            measurements.add(
                scope,
                HsxChaEventKind::TransactionInsert(transaction, HsxTorCounterKind::Miss),
                miss_insert,
                measurement,
            );
            measurements.add(
                scope,
                HsxChaEventKind::TransactionOccupancy(transaction, HsxTorCounterKind::Total),
                total_occupancy,
                measurement,
            );
            measurements.add(
                scope,
                HsxChaEventKind::TransactionOccupancy(transaction, HsxTorCounterKind::Miss),
                miss_occupancy,
                measurement,
            );
            measurements.add(
                scope,
                HsxChaEventKind::TransactionClockticks(transaction, HsxTorCounterKind::Total),
                1_000,
                measurement,
            );
            measurements.add(
                scope,
                HsxChaEventKind::TransactionClockticks(transaction, HsxTorCounterKind::Miss),
                1_000,
                measurement,
            );
        }

        let measurements = measurements.into_measurements().remove(&scope).unwrap();

        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaRfo.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            250
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaRfo.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            500
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaRfo.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            500
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaRfo.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            500
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaItoM.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            500
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaItoM.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            1_000
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaItoM.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            800
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaItoM.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            1_500
        );
    }

    #[test]
    fn derives_hsx_hits_from_total_minus_miss() {
        let scope = HsxUncoreScope { package_id: 0 };
        let mut measurements = HsxChaMeasurementAccumulator::new();
        let measurement = HsxChaMeasurement {
            enabled: Duration::from_millis(100),
            represented_unit_count: 1,
            running: Duration::from_millis(100),
            unit_scale: 1.0,
        };

        measurements.add(
            scope,
            HsxChaEventKind::TransactionInsert(HsxTransactionKind::IaCrd, HsxTorCounterKind::Total),
            1_000,
            measurement,
        );
        measurements.add(
            scope,
            HsxChaEventKind::TransactionInsert(HsxTransactionKind::IaCrd, HsxTorCounterKind::Miss),
            250,
            measurement,
        );
        measurements.add(
            scope,
            HsxChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaCrd,
                HsxTorCounterKind::Total,
            ),
            2_000,
            measurement,
        );
        measurements.add(
            scope,
            HsxChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaCrd,
                HsxTorCounterKind::Miss,
            ),
            700,
            measurement,
        );
        measurements.add(
            scope,
            HsxChaEventKind::TransactionClockticks(
                HsxTransactionKind::IaCrd,
                HsxTorCounterKind::Total,
            ),
            1_000,
            measurement,
        );
        measurements.add(
            scope,
            HsxChaEventKind::TransactionClockticks(
                HsxTransactionKind::IaCrd,
                HsxTorCounterKind::Miss,
            ),
            1_000,
            measurement,
        );

        let measurements = measurements.into_measurements().remove(&scope).unwrap();

        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaCrd.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            750
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionInsert(
                HsxTransactionKind::IaCrd.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            250
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaCrd.label(),
                ChaTransactionResult::Hit,
            )]
                .value,
            1_300
        );
        assert_eq!(
            measurements[&ChaEventKind::TransactionOccupancy(
                HsxTransactionKind::IaCrd.label(),
                ChaTransactionResult::Miss,
            )]
                .value,
            700
        );
    }

    #[test]
    fn encodes_hsx_cbo_filters() {
        assert_eq!(HsxChaFilter0::none().value(), 0);
        assert_eq!(
            HsxChaFilter0::llc_lookup_state(ChaCacheState::M).value(),
            0x08 << 17
        );
        assert_eq!(HsxChaFilter0::thread_filter(true).value(), 0x3e);
        assert_eq!(HsxChaFilter1::opcode(0x182).value(), 0x18200000);
    }

    #[test]
    fn uses_hsx_cbo_address_map() {
        assert_eq!(cbo_unit_control_offset(0), 0x0e00);
        assert_eq!(cbo_control_offset(0, 0), 0x0e01);
        assert_eq!(cbo_filter0_offset(0), 0x0e05);
        assert_eq!(cbo_filter1_offset(0), 0x0e06);
        assert_eq!(cbo_counter_offset(0, 0), 0x0e08);

        assert_eq!(cbo_unit_control_offset(23), 0x0f70);
        assert_eq!(cbo_control_offset(23, 3), 0x0f74);
        assert_eq!(cbo_counter_offset(23, 3), 0x0f7b);
    }

    #[test]
    fn uses_hsx_tor_event_encodings() {
        let group = HsxChaEventGroup::transaction(HsxTransactionKind::IaCrd, 0x181, false, true);

        assert_eq!(
            group.events,
            [
                HsxChaEventSpec::new(
                    HsxChaEventKind::TransactionOccupancy(
                        HsxTransactionKind::IaCrd,
                        HsxTorCounterKind::Miss,
                    ),
                    0x36,
                    0x03,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::TransactionInsert(
                        HsxTransactionKind::IaCrd,
                        HsxTorCounterKind::Miss,
                    ),
                    0x35,
                    0x03,
                    false,
                ),
                HsxChaEventSpec::new(
                    HsxChaEventKind::TransactionClockticks(
                        HsxTransactionKind::IaCrd,
                        HsxTorCounterKind::Miss,
                    ),
                    0x00,
                    0x00,
                    false,
                ),
                HsxChaEventSpec::unused(),
            ]
        );
    }

    #[test]
    fn programs_hsx_tor_occupancy_only_on_counter_zero() {
        for group in HSX_CBO_EVENT_GROUPS {
            for (counter, event) in group.events.iter().enumerate() {
                if event.event == TOR_OCCUPANCY_EVENT {
                    assert_eq!(counter, 0);
                }
            }
        }
    }

    #[test]
    fn uses_hsx_pcm_style_pcie_itom_filtering() {
        assert_eq!(
            HSX_CBO_EVENT_GROUPS[10].filter1,
            HsxChaFilter1::opcode(0x1c8)
        );
        assert_eq!(
            HSX_CBO_EVENT_GROUPS[10].filter0,
            HsxChaFilter0::thread_filter(true)
        );
        assert_eq!(
            HSX_CBO_EVENT_GROUPS[18].filter1,
            HsxChaFilter1::opcode(0x1c8)
        );
    }

    #[test]
    fn uses_documented_hsx_transaction_filters() {
        let cases = [
            (
                6,
                HsxTransactionKind::IoPciRdCur,
                0x19e,
                false,
                HsxTorCounterKind::Total,
            ),
            (
                7,
                HsxTransactionKind::IoPciRdCur,
                0x19e,
                false,
                HsxTorCounterKind::Miss,
            ),
            (
                8,
                HsxTransactionKind::PciRfo,
                0x180,
                true,
                HsxTorCounterKind::Total,
            ),
            (
                9,
                HsxTransactionKind::PciRfo,
                0x180,
                true,
                HsxTorCounterKind::Miss,
            ),
            (
                10,
                HsxTransactionKind::PciItoM,
                0x1c8,
                true,
                HsxTorCounterKind::Total,
            ),
            (
                11,
                HsxTransactionKind::PciItoM,
                0x1c8,
                true,
                HsxTorCounterKind::Miss,
            ),
            (
                12,
                HsxTransactionKind::TotalRfo,
                0x180,
                false,
                HsxTorCounterKind::Total,
            ),
            (
                13,
                HsxTransactionKind::TotalRfo,
                0x180,
                false,
                HsxTorCounterKind::Miss,
            ),
            (
                14,
                HsxTransactionKind::IaCrd,
                0x181,
                false,
                HsxTorCounterKind::Total,
            ),
            (
                15,
                HsxTransactionKind::IaCrd,
                0x181,
                false,
                HsxTorCounterKind::Miss,
            ),
            (
                16,
                HsxTransactionKind::IaDrd,
                0x182,
                false,
                HsxTorCounterKind::Total,
            ),
            (
                17,
                HsxTransactionKind::IaDrd,
                0x182,
                false,
                HsxTorCounterKind::Miss,
            ),
            (
                18,
                HsxTransactionKind::TotalItoM,
                0x1c8,
                false,
                HsxTorCounterKind::Total,
            ),
            (
                19,
                HsxTransactionKind::TotalItoM,
                0x1c8,
                false,
                HsxTorCounterKind::Miss,
            ),
        ];

        for (index, transaction, opcode, thread_filter, counter_kind) in cases {
            let group = HSX_CBO_EVENT_GROUPS[index];
            let expected_umask = match counter_kind {
                HsxTorCounterKind::Total => TOR_OPCODE_UMASK,
                HsxTorCounterKind::Miss => TOR_MISS_OPCODE_UMASK,
            };

            assert_eq!(group.filter0, HsxChaFilter0::thread_filter(thread_filter));
            assert_eq!(group.filter1, HsxChaFilter1::opcode(opcode));
            assert_eq!(
                group.events,
                [
                    HsxChaEventSpec::new(
                        HsxChaEventKind::TransactionOccupancy(transaction, counter_kind),
                        TOR_OCCUPANCY_EVENT,
                        expected_umask,
                        thread_filter,
                    ),
                    HsxChaEventSpec::new(
                        HsxChaEventKind::TransactionInsert(transaction, counter_kind),
                        TOR_INSERTS_EVENT,
                        expected_umask,
                        thread_filter,
                    ),
                    HsxChaEventSpec::transaction_clockticks(transaction, counter_kind),
                    HsxChaEventSpec::unused(),
                ]
            );
        }
    }

    #[test]
    fn tid_filters_hsx_pcie_rfo_and_itom_only() {
        for group_index in [6, 7, 12, 13, 18, 19] {
            assert_eq!(
                HSX_CBO_EVENT_GROUPS[group_index].filter0,
                HsxChaFilter0::none()
            );
            assert!(
                HSX_CBO_EVENT_GROUPS[group_index]
                    .events
                    .iter()
                    .all(|event| !event.thread_filter)
            );
        }

        for group_index in [8, 9, 10, 11] {
            assert_eq!(
                HSX_CBO_EVENT_GROUPS[group_index].filter0,
                HsxChaFilter0::thread_filter(true)
            );
            assert!(
                HSX_CBO_EVENT_GROUPS[group_index]
                    .events
                    .iter()
                    .take(2)
                    .all(|event| event.thread_filter)
            );
        }
    }

    #[test]
    fn schedules_short_interval_once_per_group() {
        let collector = test_collector();
        let slices = collector.schedule(Duration::from_millis(100));

        assert_eq!(slices.len(), HSX_CBO_EVENT_GROUP_COUNT);
        assert_eq!(slices[0].groups[0], Some(HSX_CBO_EVENT_GROUPS[0]));
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        collector.rotate_group();
        assert_eq!(
            collector.schedule(Duration::from_millis(100))[0].groups[0],
            Some(HSX_CBO_EVENT_GROUPS[1])
        );
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

    fn test_collector() -> HsxChaCollector {
        HsxChaCollector {
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages: vec![HsxChaPackage::new(
                HsxUncoreScope { package_id: 0 },
                (0..CHA_COUNTER_COUNT)
                    .map(|id| HsxChaUnit { cpu: 0, id })
                    .collect(),
            )],
        }
    }

    fn test_measurements() -> BTreeMap<ChaEventKind, ChaEventMeasurement> {
        let mut measurements =
            BTreeMap::from([measurement(ChaEventKind::EvictionClockticks, 1_000, 100)]);

        for state in HSX_LLC_LOOKUP_STATES {
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
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                    800,
                    100,
                ),
            ]);
        }

        for (state, value) in [
            (ChaCacheState::M, 40),
            (ChaCacheState::E, 20),
            (ChaCacheState::S, 30),
            (ChaCacheState::F, 10),
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

        for transaction in HSX_CBO_EXPORTED_TRANSACTIONS {
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
