use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE};

const COUNTER_COUNT: usize = 4;
const COUNTER_WIDTH: u32 = 48;
const FIXED_COUNTER_ENABLE_BIT: u32 = 1 << 22;
const FIXED_COUNTER_RESET_BIT: u32 = 1 << 19;
const SERVER_MC_CH_PMON_FIXED_CTR_OFFSET: u64 = 0x38;
const SERVER_MC_CH_PMON_FIXED_CTL_OFFSET: u64 = 0x54;
const SPR_IMC_BOX_TYPE: u16 = 6;
const SPR_UNIT_COUNTER_RESET_BIT: u32 = 1 << 9;
const SPR_UNIT_CONTROL_RESET_BIT: u32 = 1 << 8;
const SPR_UNIT_FREEZE_BIT: u32 = 1 << 0;
const UNCORE_DISCOVERY_DVSEC_ID_PMON: u16 = 1;
const UNCORE_EXT_CAP_ID_DISCOVERY: u16 = 0x23;

const FIXED_COUNTER_RESET_AND_ENABLE: u32 = FIXED_COUNTER_RESET_BIT | FIXED_COUNTER_ENABLE_BIT;
const SPR_UNIT_FREEZE: u32 = SPR_UNIT_FREEZE_BIT;
const SPR_UNIT_FREEZE_AND_CONTROL_RESET: u32 = SPR_UNIT_FREEZE_BIT | SPR_UNIT_CONTROL_RESET_BIT;
const SPR_UNIT_FREEZE_AND_COUNTER_RESET: u32 = SPR_UNIT_FREEZE_BIT | SPR_UNIT_COUNTER_RESET_BIT;
const SPR_UNIT_UNFREEZE: u32 = 0;

const SPR_IMC_EVENT_GROUPS: [SprImcEventGroup; 3] = [
    SprImcEventGroup {
        events: [
            SprImcEventSpec::sum(SprImcEventKind::ReadOccupancy, 0x80, 0x00),
            SprImcEventSpec::sum(SprImcEventKind::ReadOccupancy, 0x81, 0x00),
            SprImcEventSpec::sum(SprImcEventKind::WriteOccupancy, 0x82, 0x00),
            SprImcEventSpec::sum(SprImcEventKind::WriteOccupancy, 0x83, 0x00),
        ],
    },
    SprImcEventGroup {
        events: [
            SprImcEventSpec::sum(SprImcEventKind::ReadInsert, 0x10, 0x03),
            SprImcEventSpec::sum(SprImcEventKind::WriteInsert, 0x20, 0x03),
            SprImcEventSpec::sum(SprImcEventKind::ReadCas, 0x05, 0xcf),
            SprImcEventSpec::sum(SprImcEventKind::WriteCas, 0x05, 0xf0),
        ],
    },
    SprImcEventGroup {
        events: [
            SprImcEventSpec::sum(SprImcEventKind::Activate, 0x02, 0xff),
            SprImcEventSpec::sum(SprImcEventKind::PageMissPrecharge, 0x03, 0x33),
            SprImcEventSpec::disabled(),
            SprImcEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SprImcScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl SprImcScope {
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

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SprImcScopeMetrics {
    #[serde(flatten)]
    pub scope: SprImcScope,
    pub activate_commands_per_second: f64,
    pub frequency_hz: f64,
    pub page_miss_precharge_commands_per_second: f64,
    pub read_cas_commands_per_second: f64,
    pub read_bytes_per_second: f64,
    pub rpq_residency_seconds: f64,
    pub rpq_occupancy_entries: f64,
    pub write_cas_commands_per_second: f64,
    pub write_bytes_per_second: f64,
    pub wpq_residency_seconds: f64,
    pub wpq_occupancy_entries: f64,
}

impl SprImcScopeMetrics {
    fn from_measurements(
        scope: SprImcScope,
        measurements: &BTreeMap<SprImcEventKind, SprImcEventMeasurement>,
    ) -> Result<Self, String> {
        let read_insert = required_measurement(measurements, SprImcEventKind::ReadInsert)?;
        let write_insert = required_measurement(measurements, SprImcEventKind::WriteInsert)?;
        let read_occupancy = required_measurement(measurements, SprImcEventKind::ReadOccupancy)?;
        let write_occupancy = required_measurement(measurements, SprImcEventKind::WriteOccupancy)?;
        let activate = required_measurement(measurements, SprImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(measurements, SprImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(measurements, SprImcEventKind::ReadCas)?;
        let write_cas = required_measurement(measurements, SprImcEventKind::WriteCas)?;

        Ok(Self {
            scope,
            activate_commands_per_second: command_rate(activate),
            frequency_hz: frequency_hz(read_insert),
            page_miss_precharge_commands_per_second: command_rate(page_miss_precharge),
            read_cas_commands_per_second: command_rate(read_cas),
            read_bytes_per_second: bytes_per_second(
                scale_to_enabled(read_cas.value, read_cas.enabled, read_cas.running),
                read_cas.enabled,
            ),
            rpq_residency_seconds: queue_residency_seconds(read_occupancy, read_insert),
            rpq_occupancy_entries: ratio(read_occupancy.value, read_occupancy.ticks),
            write_cas_commands_per_second: command_rate(write_cas),
            write_bytes_per_second: bytes_per_second(
                scale_to_enabled(write_cas.value, write_cas.enabled, write_cas.running),
                write_cas.enabled,
            ),
            wpq_residency_seconds: queue_residency_seconds(write_occupancy, write_insert),
            wpq_occupancy_entries: ratio(write_occupancy.value, write_occupancy.ticks),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SprImcMetrics {
    pub scopes: Vec<SprImcScopeMetrics>,
}

impl SprImcMetrics {
    fn from_measurements(
        measurements: BTreeMap<SprImcScope, BTreeMap<SprImcEventKind, SprImcEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(SprImcScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SprImcCollector {
    channels: Vec<SprImcChannel>,
    next_group: usize,
    spec: SprImcSpec,
}

impl SprImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        if !matches!(model, IntelServerCpuModel::SapphireRapids) {
            return Err(format!(
                "Sapphire Rapids IMC collection is not supported for {model:?}"
            ));
        }

        let spec = spr_imc_spec();
        Ok(Self {
            channels: discover_channels(spec)?,
            next_group: 0,
            spec,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SprImcMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IMC measure interval must be non-zero",
                self.spec.name
            ));
        }

        let mut measurements = SprImcMeasurementAccumulator::new();
        let channels = &self.channels;

        for slice in self.schedule(interval) {
            program_channels(channels, slice.group)?;

            let started_at = Instant::now();
            unfreeze_channels(channels)?;
            tokio::time::sleep(slice.duration).await;
            freeze_channels(channels)?;

            read_channels(
                channels,
                SprImcMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        SprImcMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.spec.event_groups.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<SprImcMeasurementSlice> {
        let group_count = self.spec.event_groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(SprImcMeasurementSlice {
                    duration: slice_duration,
                    group: self.spec.event_groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct SprImcPrometheusMetrics {
    activate_commands_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    page_miss_precharge_commands_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_cas_commands_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_bytes_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_residency_seconds: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_occupancy_entries: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_cas_commands_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_bytes_per_second: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_residency_seconds: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_occupancy_entries: Family<SprImcScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SprImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            activate_commands_per_second:
                Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            page_miss_precharge_commands_per_second: Family::<
                SprImcScopeLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            read_cas_commands_per_second:
                Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_bytes_per_second: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_residency_seconds: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_occupancy_entries: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_cas_commands_per_second:
                Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_bytes_per_second: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_residency_seconds: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_occupancy_entries: Family::<SprImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_imc_activate_commands_per_second",
            "Interval-derived IMC activate commands per second",
            metrics.activate_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_frequency_hz",
            "Interval-derived IMC DCLK frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_imc_page_miss_precharge_commands_per_second",
            "Interval-derived IMC page-miss precharge commands per second",
            metrics.page_miss_precharge_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_read_cas_commands_per_second",
            "Interval-derived IMC read CAS commands per second",
            metrics.read_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_read_bytes_per_second",
            "Interval-derived IMC read bandwidth in bytes per second",
            metrics.read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_rpq_residency_seconds",
            "Interval-derived IMC read pending queue residency in seconds",
            metrics.rpq_residency_seconds.clone(),
        );
        registry.register(
            "ocellus_imc_rpq_occupancy_entries",
            "Average IMC read pending queue occupancy in entries",
            metrics.rpq_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_imc_write_cas_commands_per_second",
            "Interval-derived IMC write CAS commands per second",
            metrics.write_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_write_bytes_per_second",
            "Interval-derived IMC write bandwidth in bytes per second",
            metrics.write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_wpq_residency_seconds",
            "Interval-derived IMC write pending queue residency in seconds",
            metrics.wpq_residency_seconds.clone(),
        );
        registry.register(
            "ocellus_imc_wpq_occupancy_entries",
            "Average IMC write pending queue occupancy in entries",
            metrics.wpq_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SprImcMetrics) {
        for scope in metrics.scopes {
            let labels = SprImcScopeLabels::from_scope(scope.scope);

            self.activate_commands_per_second
                .get_or_create(&labels)
                .set(scope.activate_commands_per_second);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.page_miss_precharge_commands_per_second
                .get_or_create(&labels)
                .set(scope.page_miss_precharge_commands_per_second);
            self.read_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.read_cas_commands_per_second);
            self.read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.read_bytes_per_second);
            self.rpq_residency_seconds
                .get_or_create(&labels)
                .set(scope.rpq_residency_seconds);
            self.rpq_occupancy_entries
                .get_or_create(&labels)
                .set(scope.rpq_occupancy_entries);
            self.write_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.write_cas_commands_per_second);
            self.write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.write_bytes_per_second);
            self.wpq_residency_seconds
                .get_or_create(&labels)
                .set(scope.wpq_residency_seconds);
            self.wpq_occupancy_entries
                .get_or_create(&labels)
                .set(scope.wpq_occupancy_entries);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprImcMeasurementSlice {
    duration: Duration,
    group: SprImcEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SprImcScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl SprImcScopeLabels {
    fn from_scope(scope: SprImcScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct SprImcChannel {
    pmon: SprImcPmon,
    scope: SprImcScope,
}

impl SprImcChannel {
    fn freeze_and_reset(&self) -> Result<(), String> {
        self.pmon.write_unit_control(SPR_UNIT_FREEZE)?;
        self.pmon
            .write_unit_control(SPR_UNIT_FREEZE_AND_CONTROL_RESET)?;
        self.pmon
            .write_unit_control(SPR_UNIT_FREEZE_AND_COUNTER_RESET)
    }

    fn freeze(&self) -> Result<(), String> {
        self.pmon.write_unit_control(SPR_UNIT_FREEZE)
    }

    fn program(&self, group: SprImcEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            self.pmon
                .write_counter_control(counter_index, counter_control(event))?;
        }

        self.pmon
            .write_fixed_control(FIXED_COUNTER_RESET_AND_ENABLE)
    }

    fn read(&self) -> Result<SprImcChannelReading, String> {
        Ok(SprImcChannelReading {
            counters: [
                self.pmon.read_counter(0)?,
                self.pmon.read_counter(1)?,
                self.pmon.read_counter(2)?,
                self.pmon.read_counter(3)?,
            ],
            ticks: self.pmon.read_fixed_counter()?,
        })
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.pmon.write_unit_control(SPR_UNIT_UNFREEZE)
    }
}

#[derive(Debug)]
struct SprImcPmon {
    counters: [u64; COUNTER_COUNT],
    controls: [u64; COUNTER_COUNT],
    fixed_counter: u64,
    fixed_control: u64,
    mmio: metal::mmio::Mmio,
    unit_control: u64,
}

impl SprImcPmon {
    fn discovered(
        box_control: u64,
        control_offset: u8,
        counter_offset: u8,
    ) -> Result<Self, String> {
        Ok(Self {
            counters: std::array::from_fn(|index| u64::from(counter_offset) + index as u64 * 8),
            controls: std::array::from_fn(|index| u64::from(control_offset) + index as u64 * 4),
            fixed_counter: SERVER_MC_CH_PMON_FIXED_CTR_OFFSET,
            fixed_control: SERVER_MC_CH_PMON_FIXED_CTL_OFFSET,
            mmio: metal::mmio::Mmio::open(box_control)?,
            unit_control: 0,
        })
    }

    fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
        self.mmio
            .read_u64(self.counters[counter_index])
            .map(mask_counter)
    }

    fn read_fixed_counter(&self) -> Result<u64, String> {
        self.mmio.read_u64(self.fixed_counter).map(mask_counter)
    }

    fn write_counter_control(&self, counter_index: usize, value: u32) -> Result<(), String> {
        self.mmio.write_u32(self.controls[counter_index], value)
    }

    fn write_fixed_control(&self, value: u32) -> Result<(), String> {
        self.mmio.write_u32(self.fixed_control, value)
    }

    fn write_unit_control(&self, value: u32) -> Result<(), String> {
        self.mmio.write_u32(self.unit_control, value)
    }
}

#[derive(Clone, Copy, Debug)]
struct SprImcChannelReading {
    counters: [u64; COUNTER_COUNT],
    ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprImcEventGroup {
    events: [SprImcEventSpec; COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SprImcEventSpec {
    event: u8,
    kind: Option<SprImcEventKind>,
    umask: u8,
}

impl SprImcEventSpec {
    pub const fn sum(kind: SprImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            event,
            kind: Some(kind),
            umask,
        }
    }

    pub const fn disabled() -> Self {
        Self {
            event: 0,
            kind: None,
            umask: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SprImcEventKind {
    Activate,
    PageMissPrecharge,
    ReadCas,
    ReadInsert,
    ReadOccupancy,
    WriteCas,
    WriteInsert,
    WriteOccupancy,
}

#[derive(Clone, Copy, Debug)]
struct SprImcEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl SprImcEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct SprImcMeasurement {
    enabled: Duration,
    group: SprImcEventGroup,
    running: Duration,
}

#[derive(Debug, Default)]
struct SprImcMeasurementAccumulator {
    measurements: BTreeMap<SprImcScope, BTreeMap<SprImcEventKind, SprImcEventMeasurement>>,
}

impl SprImcMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: SprImcScope,
        kind: SprImcEventKind,
        value: u64,
        ticks: u64,
        measurement: SprImcMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(SprImcEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<SprImcScope, BTreeMap<SprImcEventKind, SprImcEventMeasurement>> {
        self.measurements
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprImcSpec {
    channels: SprImcChannels,
    event_groups: &'static [SprImcEventGroup],
    name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprImcChannels {
    Discovery { box_type: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UncoreDiscoveryVsec {
    address: u32,
    cap_id: u16,
    cap_next: u16,
    entry_id: u16,
    tbir: u8,
}

impl UncoreDiscoveryVsec {
    fn from_words(first: u64, second: u64) -> Self {
        Self {
            address: ((second >> 35) & ((1_u64 << 29) - 1)) as u32,
            cap_id: (first & 0xffff) as u16,
            cap_next: ((first >> 20) & 0x0fff) as u16,
            entry_id: (second & 0xffff) as u16,
            tbir: ((second >> 32) & 0x07) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UncoreGlobalDiscovery {
    max_units: u16,
    stride: u8,
}

impl UncoreGlobalDiscovery {
    fn from_words(words: [u64; 3]) -> Self {
        Self {
            max_units: ((words[0] >> 16) & 0x03ff) as u16,
            stride: ((words[0] >> 8) & 0xff) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UncoreBoxDiscovery {
    access_type: u8,
    bit_width: u8,
    box_control: u64,
    box_type: u16,
    counter_offset: u8,
    control_offset: u8,
    num_registers: u8,
}

impl UncoreBoxDiscovery {
    fn from_words(words: [u64; 3]) -> Self {
        Self {
            access_type: ((words[0] >> 62) & 0x03) as u8,
            bit_width: ((words[0] >> 16) & 0xff) as u8,
            box_control: words[1],
            box_type: (words[2] & 0xffff) as u16,
            counter_offset: ((words[0] >> 24) & 0xff) as u8,
            control_offset: ((words[0] >> 8) & 0xff) as u8,
            num_registers: (words[0] & 0xff) as u8,
        }
    }

    fn is_valid(self) -> bool {
        self.num_registers != 0 && self.box_control != 0
    }
}

fn bytes_per_second(cache_lines: u64, duration: Duration) -> f64 {
    events_per_second(cache_lines, duration) * BYTES_PER_CACHE_LINE
}

fn command_rate(measurement: &SprImcEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn counter_control(event: SprImcEventSpec) -> u32 {
    if event.kind.is_none() {
        return 0;
    }

    u32::from(event.event) | (u32::from(event.umask) << 8)
}

fn discover_channels(spec: SprImcSpec) -> Result<Vec<SprImcChannel>, String> {
    match spec.channels {
        SprImcChannels::Discovery { box_type } => discover_channels_from_discovery(box_type),
    }
}

fn discover_channels_from_discovery(box_type: u16) -> Result<Vec<SprImcChannel>, String> {
    let socket_boxes = discover_uncore_boxes(box_type)?;

    let mut channels = Vec::new();
    for socket_boxes in socket_boxes {
        for box_pmu in socket_boxes.boxes {
            channels.push(SprImcChannel {
                pmon: SprImcPmon::discovered(
                    box_pmu.box_control,
                    box_pmu.control_offset,
                    box_pmu.counter_offset,
                )?,
                scope: socket_boxes.scope,
            });
        }
    }

    if channels.is_empty() {
        return Err("failed to discover any IMC channels from PMU discovery".to_string());
    }

    Ok(channels)
}

fn discover_uncore_boxes(box_type: u16) -> Result<Vec<UncoreDiscoverySocketBoxes>, String> {
    let topologies = metal::topology::cpu_topologies()?;
    let mut sockets = Vec::new();
    for discovered_device in metal::pci::find_intel_devices()? {
        let Ok(device) = metal::pci::PciDevice::open_readonly(discovered_device.location) else {
            continue;
        };

        let mut offset = 0x100;
        loop {
            let Ok(first_word) = device.read_u64(offset) else {
                break;
            };
            if first_word == 0 {
                break;
            }
            let Ok(second_word) = device.read_u64(offset + 8) else {
                break;
            };
            let vsec = UncoreDiscoveryVsec::from_words(first_word, second_word);

            if vsec.cap_id == UNCORE_EXT_CAP_ID_DISCOVERY
                && vsec.entry_id == UNCORE_DISCOVERY_DVSEC_ID_PMON
            {
                let bar_offset = 0x10 + u64::from(vsec.tbir) * 4;
                let bar = discovery_bar(&device, bar_offset)?;
                if bar != 0 {
                    let scope = pci_device_scope(discovered_device.location, &topologies)?;
                    sockets.push(UncoreDiscoverySocketBoxes {
                        boxes: discover_uncore_boxes_from_bar(bar, box_type)?,
                        scope,
                    });
                }
            }

            let next_offset = u64::from(vsec.cap_next & !0x03);
            if next_offset == 0 || next_offset == offset {
                break;
            }
            offset = next_offset;
        }
    }

    Ok(sockets)
}

#[derive(Clone, Debug)]
struct UncoreDiscoverySocketBoxes {
    boxes: Vec<UncoreBoxDiscovery>,
    scope: SprImcScope,
}

fn pci_device_scope(
    location: metal::pci::PciLocation,
    topologies: &[CpuTopology],
) -> Result<SprImcScope, String> {
    let local_cpus = metal::pci::local_cpus(location)?;
    scope_from_local_cpus(&local_cpus, topologies).ok_or_else(|| {
        format!("failed to map PCI device {location} local CPUs to a CPU topology scope")
    })
}

fn scope_from_local_cpus(local_cpus: &[u32], topologies: &[CpuTopology]) -> Option<SprImcScope> {
    for cpu in local_cpus {
        if let Some(topology) = topologies.iter().find(|topology| topology.cpu == *cpu) {
            return SprImcScope::from_topology(topology).ok();
        }
    }

    None
}

fn discovery_bar(device: &metal::pci::PciDevice, offset: u64) -> Result<u64, String> {
    let low = device.read_u32(offset)?;
    let high = if low & 0x04 != 0 {
        device.read_u32(offset + 4)?
    } else {
        0
    };

    Ok(decode_discovery_bar(low, high))
}

fn decode_discovery_bar(low: u32, high: u32) -> u64 {
    (u64::from(low) | (u64::from(high) << 32)) & !0xfff
}

fn discover_uncore_boxes_from_bar(
    bar: u64,
    box_type: u16,
) -> Result<Vec<UncoreBoxDiscovery>, String> {
    let mmio = metal::mmio::Mmio::open(bar)?;
    let global = UncoreGlobalDiscovery::from_words(read_discovery_words(&mmio, 0)?);
    let stride = u64::from(global.stride) * 8;
    let mut boxes = Vec::new();

    for unit_index in 0..global.max_units {
        let words = read_discovery_words(&mmio, u64::from(unit_index + 1) * stride)?;
        if words[0] == 0 && words[1] == 0 {
            continue;
        }

        let box_pmu = UncoreBoxDiscovery::from_words(words);
        if box_pmu.is_valid()
            && box_pmu.access_type == 1
            && box_pmu.bit_width <= 64
            && box_pmu.num_registers >= COUNTER_COUNT as u8
            && box_pmu.box_type == box_type
        {
            boxes.push(box_pmu);
        }
    }

    Ok(boxes)
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn frequency_hz(measurement: &SprImcEventMeasurement) -> f64 {
    events_per_second(measurement.ticks, measurement.running)
}

fn freeze_channels(channels: &[SprImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.freeze()?;
    }

    Ok(())
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << COUNTER_WIDTH) - 1)
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn program_channels(channels: &[SprImcChannel], group: SprImcEventGroup) -> Result<(), String> {
    for channel in channels {
        channel.freeze_and_reset()?;
    }

    for channel in channels {
        channel.program(group)?;
    }

    Ok(())
}

fn queue_residency_seconds(
    occupancy: &SprImcEventMeasurement,
    insert: &SprImcEventMeasurement,
) -> f64 {
    if occupancy.ticks == 0 || insert.value == 0 || insert.running.is_zero() {
        return 0.0;
    }

    let average_occupancy = ratio(occupancy.value, occupancy.ticks);
    let insert_rate = insert.value as f64 / insert.running.as_secs_f64();
    average_occupancy / insert_rate
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn read_channels(
    channels: &[SprImcChannel],
    measurement: SprImcMeasurement,
    measurements: &mut SprImcMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<SprImcScope, Vec<SprImcChannelReading>>::new();

    for channel in channels {
        readings
            .entry(channel.scope)
            .or_default()
            .push(channel.read()?);
    }

    for (scope, channel_readings) in readings {
        let ticks = average_u64(channel_readings.iter().map(|reading| reading.ticks));

        let mut event_values = BTreeMap::<SprImcEventKind, u64>::new();

        for counter_index in 0..measurement.group.events.len() {
            let event = measurement.group.events[counter_index];
            let Some(kind) = event.kind else {
                continue;
            };
            let value: u64 = channel_readings
                .iter()
                .map(|reading| reading.counters[counter_index])
                .sum();

            *event_values.entry(kind).or_default() += value;
        }

        for (kind, value) in event_values {
            measurements.add(scope, kind, value, ticks, measurement);
        }
    }

    Ok(())
}

fn read_discovery_words(mmio: &metal::mmio::Mmio, offset: u64) -> Result<[u64; 3], String> {
    Ok([
        mmio.read_u64(offset)?,
        mmio.read_u64(offset + 8)?,
        mmio.read_u64(offset + 16)?,
    ])
}

fn required_measurement(
    measurements: &BTreeMap<SprImcEventKind, SprImcEventMeasurement>,
    kind: SprImcEventKind,
) -> Result<&SprImcEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IMC measurement {kind:?} is missing"))
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

fn spr_imc_spec() -> SprImcSpec {
    SprImcSpec {
        channels: SprImcChannels::Discovery {
            box_type: SPR_IMC_BOX_TYPE,
        },
        event_groups: &SPR_IMC_EVENT_GROUPS,
        name: "Sapphire Rapids",
    }
}

fn unfreeze_channels(channels: &[SprImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unfreeze()?;
    }

    Ok(())
}

fn average_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut count = 0;
    let mut sum = 0_u64;

    for value in values {
        count += 1;
        sum += value;
    }

    if count == 0 { 0 } else { sum / count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metal::topology::TopologyLevel;

    #[test]
    fn uses_imc_event_encodings() {
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::ReadInsert,
            0x10,
            0x03,
        );
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::WriteInsert,
            0x20,
            0x03,
        );
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::ReadOccupancy,
            0x80,
            0x00,
        );
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::ReadOccupancy,
            0x81,
            0x00,
        );
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::WriteOccupancy,
            0x82,
            0x00,
        );
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::WriteOccupancy,
            0x83,
            0x00,
        );
        assert_event(&SPR_IMC_EVENT_GROUPS, SprImcEventKind::ReadCas, 0x05, 0xcf);
        assert_event(&SPR_IMC_EVENT_GROUPS, SprImcEventKind::WriteCas, 0x05, 0xf0);
        assert_event(&SPR_IMC_EVENT_GROUPS, SprImcEventKind::Activate, 0x02, 0xff);
        assert_event(
            &SPR_IMC_EVENT_GROUPS,
            SprImcEventKind::PageMissPrecharge,
            0x03,
            0x33,
        );
    }

    #[test]
    fn decodes_uncore_discovery_vsec() {
        let first = UNCORE_EXT_CAP_ID_DISCOVERY as u64 | (0x120_u64 << 20);
        let second = UNCORE_DISCOVERY_DVSEC_ID_PMON as u64
            | (0x24_u64 << 16)
            | (0x03_u64 << 24)
            | (3_u64 << 32)
            | (0x12345_u64 << 35);
        let vsec = UncoreDiscoveryVsec::from_words(first, second);

        assert_eq!(vsec.address, 0x12345);
        assert_eq!(vsec.cap_id, UNCORE_EXT_CAP_ID_DISCOVERY);
        assert_eq!(vsec.cap_next, 0x120);
        assert_eq!(vsec.entry_id, UNCORE_DISCOVERY_DVSEC_ID_PMON);
        assert_eq!(vsec.tbir, 3);
    }

    #[test]
    fn decodes_64_bit_discovery_bar() {
        assert_eq!(
            decode_discovery_bar(0x1234_5004, 0x0000_0078),
            0x78_1234_5000
        );
    }

    #[test]
    fn maps_pci_device_scope_from_local_cpu_topology() {
        let topologies = [
            topology(0, 0, 0, 0),
            topology(1, 0, 0, 0),
            topology(8, 1, 0, 0),
        ];
        let scope = scope_from_local_cpus(&[8], &topologies).unwrap();

        assert_eq!(scope.package_id, 1);
    }

    #[test]
    fn decodes_uncore_discovery_box() {
        let words = [
            4_u64 | (0x40_u64 << 8) | (64_u64 << 16) | (0x08_u64 << 24) | (1_u64 << 62),
            0x1234_5000,
            SPR_IMC_BOX_TYPE as u64,
        ];
        let box_pmu = UncoreBoxDiscovery::from_words(words);

        assert_eq!(box_pmu.access_type, 1);
        assert_eq!(box_pmu.bit_width, 64);
        assert_eq!(box_pmu.box_control, 0x1234_5000);
        assert_eq!(box_pmu.box_type, SPR_IMC_BOX_TYPE);
        assert_eq!(box_pmu.counter_offset, 0x08);
        assert_eq!(box_pmu.control_offset, 0x40);
        assert_eq!(box_pmu.num_registers, 4);
    }

    #[test]
    fn computes_multiplexed_metrics() {
        let scope = SprImcScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        };
        let metrics = SprImcMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(SprImcEventKind::ReadInsert, 200, 1_000, 100),
                measurement(SprImcEventKind::WriteInsert, 300, 1_000, 100),
                measurement(SprImcEventKind::ReadOccupancy, 400, 1_000, 100),
                measurement(SprImcEventKind::WriteOccupancy, 600, 1_000, 100),
                measurement(SprImcEventKind::ReadCas, 2_000, 1_000, 100),
                measurement(SprImcEventKind::WriteCas, 3_000, 1_000, 100),
                measurement(SprImcEventKind::Activate, 1_000, 1_000, 100),
                measurement(SprImcEventKind::PageMissPrecharge, 700, 1_000, 100),
            ]),
        )]))
        .unwrap();

        let scope_metrics = metrics.scopes[0];

        assert_eq!(scope_metrics.activate_commands_per_second, 10_000.0);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(
            scope_metrics.page_miss_precharge_commands_per_second,
            7_000.0
        );
        assert_eq!(scope_metrics.read_cas_commands_per_second, 20_000.0);
        assert_eq!(scope_metrics.read_bytes_per_second, 1_280_000.0);
        assert_eq!(scope_metrics.write_cas_commands_per_second, 30_000.0);
        assert_eq!(scope_metrics.write_bytes_per_second, 1_920_000.0);
        assert_eq!(scope_metrics.rpq_occupancy_entries, 0.4);
        assert_eq!(scope_metrics.wpq_occupancy_entries, 0.6);
        assert_close(scope_metrics.rpq_residency_seconds, 0.0002);
        assert_close(scope_metrics.wpq_residency_seconds, 0.0002);
    }

    #[test]
    fn computes_residency_from_occupancy_dclk_and_insert_rate() {
        let occupancy = SprImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(400),
            ticks: 400,
            value: 800,
        };
        let insert = SprImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(200),
            ticks: 200,
            value: 2_000_000,
        };

        assert_close(queue_residency_seconds(&occupancy, &insert), 0.0000002);
    }

    fn assert_event(groups: &[SprImcEventGroup], kind: SprImcEventKind, event: u8, umask: u8) {
        let event_spec = groups
            .iter()
            .flat_map(|group| group.events)
            .find(|event_spec| {
                event_spec.kind == Some(kind)
                    && event_spec.event == event
                    && event_spec.umask == umask
            })
            .unwrap();

        assert_eq!(event_spec.event, event);
        assert_eq!(event_spec.umask, umask);
    }

    fn measurement(
        kind: SprImcEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (SprImcEventKind, SprImcEventMeasurement) {
        (
            kind,
            SprImcEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                ticks,
                value,
            },
        )
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    fn topology(cpu: u32, package_id: u32, die_id: u32, die_group_id: u32) -> CpuTopology {
        CpuTopology {
            cpu,
            levels: vec![
                TopologyLevel {
                    id: die_group_id,
                    kind: TopologyLevelKind::DieGroup,
                    shift: 0,
                },
                TopologyLevel {
                    id: die_id,
                    kind: TopologyLevelKind::Die,
                    shift: 0,
                },
                TopologyLevel {
                    id: package_id,
                    kind: TopologyLevelKind::Package,
                    shift: 0,
                },
            ],
            x2apic_id: 0,
        }
    }
}
