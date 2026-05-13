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
const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const COUNTER_WIDTH: u32 = 48;
const FIXED_COUNTER_ENABLE_BIT: u32 = 1 << 22;
const FIXED_COUNTER_RESET_BIT: u32 = 1 << 19;
const SERVER_MEM_BAR_OFFSET: u64 = 0xd8;
const SERVER_MC_CH_PMON_BASE_ADDR: u64 = 0x22800;
const SERVER_MC_CH_PMON_BOX_CTL_OFFSET: u64 = 0x00;
const SERVER_MC_CH_PMON_CTR0_OFFSET: u64 = 0x08;
const SERVER_MC_CH_PMON_CTL0_OFFSET: u64 = 0x40;
const SERVER_MC_CH_PMON_FIXED_CTR_OFFSET: u64 = 0x38;
const SERVER_MC_CH_PMON_FIXED_CTL_OFFSET: u64 = 0x54;
const SERVER_MC_CH_PMON_STEP: u64 = 0x4000;
const SERVER_UBOX0_DEVICE: u8 = 0;
const SERVER_UBOX0_DEVICE_IDS: [u16; 2] = [0x3451, 0x3251];
const SERVER_UBOX0_FUNCTION: u8 = 1;
const SERVER_UBOX0_MMIO_BASE_OFFSET: u64 = 0xd0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u32 = 1 << 16;

const FIXED_COUNTER_RESET_AND_ENABLE: u32 = FIXED_COUNTER_RESET_BIT | FIXED_COUNTER_ENABLE_BIT;
const UNIT_FREEZE: u32 = UNIT_FREEZE_ENABLE_BIT | UNIT_FREEZE_BIT;
const UNIT_FREEZE_AND_RESET: u32 =
    UNIT_FREEZE_ENABLE_BIT | UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT;
const UNIT_UNFREEZE: u32 = UNIT_FREEZE_ENABLE_BIT;

const ICX_IMC_EVENT_GROUPS: [IcxImcEventGroup; 3] = [
    IcxImcEventGroup {
        events: [
            IcxImcEventSpec::sum(IcxImcEventKind::ReadOccupancy, 0x80, 0x00),
            IcxImcEventSpec::sum(IcxImcEventKind::ReadOccupancy, 0x81, 0x00),
            IcxImcEventSpec::sum(IcxImcEventKind::WriteOccupancy, 0x82, 0x00),
            IcxImcEventSpec::sum(IcxImcEventKind::WriteOccupancy, 0x83, 0x00),
        ],
    },
    IcxImcEventGroup {
        events: [
            IcxImcEventSpec::sum(IcxImcEventKind::ReadInsert, 0x10, 0x03),
            IcxImcEventSpec::sum(IcxImcEventKind::WriteInsert, 0x20, 0x03),
            IcxImcEventSpec::sum(IcxImcEventKind::ReadCas, 0x04, 0x0f),
            IcxImcEventSpec::sum(IcxImcEventKind::WriteCas, 0x04, 0x30),
        ],
    },
    IcxImcEventGroup {
        events: [
            IcxImcEventSpec::sum(IcxImcEventKind::Activate, 0x01, 0x0b),
            IcxImcEventSpec::sum(IcxImcEventKind::PageMissPrecharge, 0x02, 0x0c),
            IcxImcEventSpec::disabled(),
            IcxImcEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct IcxImcScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl IcxImcScope {
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
pub struct IcxImcScopeMetrics {
    #[serde(flatten)]
    pub scope: IcxImcScope,
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

impl IcxImcScopeMetrics {
    fn from_measurements(
        scope: IcxImcScope,
        measurements: &BTreeMap<IcxImcEventKind, IcxImcEventMeasurement>,
    ) -> Result<Self, String> {
        let read_insert = required_measurement(measurements, IcxImcEventKind::ReadInsert)?;
        let write_insert = required_measurement(measurements, IcxImcEventKind::WriteInsert)?;
        let read_occupancy = required_measurement(measurements, IcxImcEventKind::ReadOccupancy)?;
        let write_occupancy = required_measurement(measurements, IcxImcEventKind::WriteOccupancy)?;
        let activate = required_measurement(measurements, IcxImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(measurements, IcxImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(measurements, IcxImcEventKind::ReadCas)?;
        let write_cas = required_measurement(measurements, IcxImcEventKind::WriteCas)?;

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
pub struct IcxImcMetrics {
    pub scopes: Vec<IcxImcScopeMetrics>,
}

impl IcxImcMetrics {
    fn from_measurements(
        measurements: BTreeMap<IcxImcScope, BTreeMap<IcxImcEventKind, IcxImcEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(IcxImcScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct IcxImcCollector {
    channels: Vec<IcxImcChannel>,
    next_group: usize,
    spec: IcxImcSpec,
}

impl IcxImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        if !matches!(model, IntelServerCpuModel::IceLakeXeon) {
            return Err(format!(
                "Ice Lake-SP IMC collection is not supported for {model:?}"
            ));
        }

        let spec = icx_imc_spec();
        Ok(Self {
            channels: discover_channels(spec)?,
            next_group: 0,
            spec,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxImcMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IMC measure interval must be non-zero",
                self.spec.name
            ));
        }

        let mut measurements = IcxImcMeasurementAccumulator::new();
        let channels = &self.channels;

        for slice in self.schedule(interval) {
            program_channels(channels, slice.group)?;

            let started_at = Instant::now();
            unfreeze_channels(channels)?;
            tokio::time::sleep(slice.duration).await;
            freeze_channels(channels)?;

            read_channels(
                channels,
                IcxImcMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        IcxImcMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.spec.event_groups.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IcxImcMeasurementSlice> {
        let group_count = self.spec.event_groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IcxImcMeasurementSlice {
                    duration: slice_duration,
                    group: self.spec.event_groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct IcxImcPrometheusMetrics {
    activate_commands_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    page_miss_precharge_commands_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_cas_commands_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_bytes_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_residency_seconds: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_occupancy_entries: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_cas_commands_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_bytes_per_second: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_residency_seconds: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_occupancy_entries: Family<IcxImcScopeLabels, Gauge<f64, AtomicU64>>,
}

impl IcxImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            activate_commands_per_second:
                Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            page_miss_precharge_commands_per_second: Family::<
                IcxImcScopeLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            read_cas_commands_per_second:
                Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_bytes_per_second: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_residency_seconds: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_occupancy_entries: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_cas_commands_per_second:
                Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_bytes_per_second: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_residency_seconds: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_occupancy_entries: Family::<IcxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
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

    pub fn update(&self, metrics: IcxImcMetrics) {
        for scope in metrics.scopes {
            let labels = IcxImcScopeLabels::from_scope(scope.scope);

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
struct IcxImcMeasurementSlice {
    duration: Duration,
    group: IcxImcEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct IcxImcScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl IcxImcScopeLabels {
    fn from_scope(scope: IcxImcScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct IcxImcChannel {
    pmon: IcxImcPmon,
    scope: IcxImcScope,
}

impl IcxImcChannel {
    fn freeze_and_reset(&self) -> Result<(), String> {
        self.pmon.write_unit_control(UNIT_FREEZE_AND_RESET)
    }

    fn freeze(&self) -> Result<(), String> {
        self.pmon.write_unit_control(UNIT_FREEZE)
    }

    fn program(&self, group: IcxImcEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            self.pmon
                .write_counter_control(counter_index, counter_control(event))?;
        }

        self.pmon
            .write_fixed_control(FIXED_COUNTER_RESET_AND_ENABLE)
    }

    fn read(&self) -> Result<IcxImcChannelReading, String> {
        Ok(IcxImcChannelReading {
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
        self.pmon.write_unit_control(UNIT_UNFREEZE)
    }
}

#[derive(Debug)]
struct IcxImcPmon {
    counters: [u64; COUNTER_COUNT],
    controls: [u64; COUNTER_COUNT],
    fixed_counter: u64,
    fixed_control: u64,
    mmio: metal::mmio::Mmio,
    unit_control: u64,
}

impl IcxImcPmon {
    fn legacy(base: u64) -> Result<Self, String> {
        Ok(Self {
            counters: std::array::from_fn(|index| SERVER_MC_CH_PMON_CTR0_OFFSET + index as u64 * 8),
            controls: std::array::from_fn(|index| SERVER_MC_CH_PMON_CTL0_OFFSET + index as u64 * 4),
            fixed_counter: SERVER_MC_CH_PMON_FIXED_CTR_OFFSET,
            fixed_control: SERVER_MC_CH_PMON_FIXED_CTL_OFFSET,
            mmio: metal::mmio::Mmio::open(base)?,
            unit_control: SERVER_MC_CH_PMON_BOX_CTL_OFFSET,
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
struct IcxImcChannelReading {
    counters: [u64; COUNTER_COUNT],
    ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxImcEventGroup {
    events: [IcxImcEventSpec; COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcxImcEventSpec {
    event: u8,
    kind: Option<IcxImcEventKind>,
    umask: u8,
}

impl IcxImcEventSpec {
    pub const fn sum(kind: IcxImcEventKind, event: u8, umask: u8) -> Self {
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
pub enum IcxImcEventKind {
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
struct IcxImcEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl IcxImcEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxImcMeasurement {
    enabled: Duration,
    group: IcxImcEventGroup,
    running: Duration,
}

#[derive(Debug, Default)]
struct IcxImcMeasurementAccumulator {
    measurements: BTreeMap<IcxImcScope, BTreeMap<IcxImcEventKind, IcxImcEventMeasurement>>,
}

impl IcxImcMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: IcxImcScope,
        kind: IcxImcEventKind,
        value: u64,
        ticks: u64,
        measurement: IcxImcMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(IcxImcEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<IcxImcScope, BTreeMap<IcxImcEventKind, IcxImcEventMeasurement>> {
        self.measurements
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxImcSpec {
    channels: IcxImcChannels,
    event_groups: &'static [IcxImcEventGroup],
    name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcxImcChannels {
    UboxBars {
        bars_per_package: usize,
        channels_per_bar: usize,
    },
}

fn bytes_per_second(cache_lines: u64, duration: Duration) -> f64 {
    events_per_second(cache_lines, duration) * BYTES_PER_CACHE_LINE
}

fn command_rate(measurement: &IcxImcEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn counter_control(event: IcxImcEventSpec) -> u32 {
    if event.kind.is_none() {
        return 0;
    }

    let event_select = u32::from(event.event) | (u32::from(event.umask) << 8);
    event_select | COUNTER_RESET_BIT | COUNTER_OVERFLOW_ENABLE_BIT | COUNTER_ENABLE_BIT
}

fn discover_channels(spec: IcxImcSpec) -> Result<Vec<IcxImcChannel>, String> {
    match spec.channels {
        IcxImcChannels::UboxBars {
            bars_per_package,
            channels_per_bar,
        } => discover_channels_from_ubox_bars(bars_per_package, channels_per_bar),
    }
}

fn discover_channels_from_ubox_bars(
    bars_per_package: usize,
    channels_per_bar: usize,
) -> Result<Vec<IcxImcChannel>, String> {
    let scopes = imc_scopes()?;
    let uboxes = metal::pci::find_intel_devices_at_address_matching_device_ids(
        SERVER_UBOX0_DEVICE,
        SERVER_UBOX0_FUNCTION,
        &SERVER_UBOX0_DEVICE_IDS,
    )?;

    if uboxes.len() != scopes.len() {
        return Err(format!(
            "discovered {} UBOX0 devices for {} CPU packages",
            uboxes.len(),
            scopes.len()
        ));
    }

    let mut channels = Vec::with_capacity(scopes.len() * bars_per_package * channels_per_bar);
    for (socket_index, ubox) in uboxes.into_iter().enumerate() {
        let scope = scopes.get(socket_index).copied().ok_or_else(|| {
            format!("failed to map UBOX0 socket index {socket_index} to a CPUID package")
        })?;
        let device = metal::pci::PciDevice::open_readonly(ubox.location)?;
        let mmio_base = device.read_u32(SERVER_UBOX0_MMIO_BASE_OFFSET)?;

        for bar_index in 0..bars_per_package {
            let memory_offset =
                device.read_u32(SERVER_MEM_BAR_OFFSET + u64::try_from(bar_index).unwrap() * 4)?;
            let memory_bar = server_memory_bar(mmio_base, memory_offset);
            if memory_bar == 0 {
                return Err(format!(
                    "server memory BAR {bar_index} for UBOX0 {} is zero",
                    ubox.location
                ));
            }

            for channel_index in 0..channels_per_bar {
                let base = memory_bar
                    + SERVER_MC_CH_PMON_BASE_ADDR
                    + u64::try_from(channel_index).unwrap() * SERVER_MC_CH_PMON_STEP;
                channels.push(IcxImcChannel {
                    pmon: IcxImcPmon::legacy(base)?,
                    scope,
                });
            }
        }
    }

    if channels.is_empty() {
        return Err("failed to discover any IMC channels from UBOX memory BARs".to_string());
    }

    Ok(channels)
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn frequency_hz(measurement: &IcxImcEventMeasurement) -> f64 {
    events_per_second(measurement.ticks, measurement.running)
}

fn freeze_channels(channels: &[IcxImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.freeze()?;
    }

    Ok(())
}

fn imc_scopes() -> Result<Vec<IcxImcScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(IcxImcScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any IMC scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
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

fn program_channels(channels: &[IcxImcChannel], group: IcxImcEventGroup) -> Result<(), String> {
    for channel in channels {
        channel.freeze_and_reset()?;
    }

    for channel in channels {
        channel.program(group)?;
    }

    Ok(())
}

fn queue_residency_seconds(
    occupancy: &IcxImcEventMeasurement,
    insert: &IcxImcEventMeasurement,
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
    channels: &[IcxImcChannel],
    measurement: IcxImcMeasurement,
    measurements: &mut IcxImcMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<IcxImcScope, Vec<IcxImcChannelReading>>::new();

    for channel in channels {
        readings
            .entry(channel.scope)
            .or_default()
            .push(channel.read()?);
    }

    for (scope, channel_readings) in readings {
        let ticks = average_u64(channel_readings.iter().map(|reading| reading.ticks));

        let mut event_values = BTreeMap::<IcxImcEventKind, u64>::new();

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

fn required_measurement(
    measurements: &BTreeMap<IcxImcEventKind, IcxImcEventMeasurement>,
    kind: IcxImcEventKind,
) -> Result<&IcxImcEventMeasurement, String> {
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

fn icx_imc_spec() -> IcxImcSpec {
    IcxImcSpec {
        channels: IcxImcChannels::UboxBars {
            bars_per_package: 4,
            channels_per_bar: 3,
        },
        event_groups: &ICX_IMC_EVENT_GROUPS,
        name: "Ice Lake-SP",
    }
}

fn server_memory_bar(mmio_base: u32, memory_offset: u32) -> u64 {
    (u64::from(mmio_base) & ((1_u64 << 29) - 1)) << 23
        | (u64::from(memory_offset) & ((1_u64 << 11) - 1)) << 12
}

fn unfreeze_channels(channels: &[IcxImcChannel]) -> Result<(), String> {
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

    #[test]
    fn uses_imc_event_encodings() {
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::ReadInsert,
            0x10,
            0x03,
        );
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::WriteInsert,
            0x20,
            0x03,
        );
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::ReadOccupancy,
            0x80,
            0x00,
        );
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::ReadOccupancy,
            0x81,
            0x00,
        );
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::WriteOccupancy,
            0x82,
            0x00,
        );
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::WriteOccupancy,
            0x83,
            0x00,
        );
        assert_event(&ICX_IMC_EVENT_GROUPS, IcxImcEventKind::ReadCas, 0x04, 0x0f);
        assert_event(&ICX_IMC_EVENT_GROUPS, IcxImcEventKind::WriteCas, 0x04, 0x30);
        assert_event(&ICX_IMC_EVENT_GROUPS, IcxImcEventKind::Activate, 0x01, 0x0b);
        assert_event(
            &ICX_IMC_EVENT_GROUPS,
            IcxImcEventKind::PageMissPrecharge,
            0x02,
            0x0c,
        );
    }

    #[test]
    fn computes_server_bar_from_ubox_registers() {
        assert_eq!(
            server_memory_bar(0x0000_0123, 0x0000_0456),
            (0x123_u64 << 23) | (0x456_u64 << 12)
        );
        assert_eq!(server_memory_bar(0, 0), 0);
    }

    #[test]
    fn ice_lake_uses_three_channels_per_bar() {
        let spec = icx_imc_spec();
        assert_eq!(
            spec.channels,
            IcxImcChannels::UboxBars {
                bars_per_package: 4,
                channels_per_bar: 3,
            }
        );
    }

    #[test]
    fn computes_multiplexed_metrics() {
        let scope = IcxImcScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        };
        let metrics = IcxImcMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(IcxImcEventKind::ReadInsert, 200, 1_000, 100),
                measurement(IcxImcEventKind::WriteInsert, 300, 1_000, 100),
                measurement(IcxImcEventKind::ReadOccupancy, 400, 1_000, 100),
                measurement(IcxImcEventKind::WriteOccupancy, 600, 1_000, 100),
                measurement(IcxImcEventKind::ReadCas, 2_000, 1_000, 100),
                measurement(IcxImcEventKind::WriteCas, 3_000, 1_000, 100),
                measurement(IcxImcEventKind::Activate, 1_000, 1_000, 100),
                measurement(IcxImcEventKind::PageMissPrecharge, 700, 1_000, 100),
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
        let occupancy = IcxImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(400),
            ticks: 400,
            value: 800,
        };
        let insert = IcxImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(200),
            ticks: 200,
            value: 2_000_000,
        };

        assert_close(queue_residency_seconds(&occupancy, &insert), 0.0000002);
    }

    fn assert_event(groups: &[IcxImcEventGroup], kind: IcxImcEventKind, event: u8, umask: u8) {
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
        kind: IcxImcEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (IcxImcEventKind, IcxImcEventMeasurement) {
        (
            kind,
            IcxImcEventMeasurement {
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
}
