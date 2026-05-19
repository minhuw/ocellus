use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::pci::{PciBus, PciDevice, PciDeviceSpec, PciVendorDevice};
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE};

const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const FIXED_COUNTER_ENABLE_BIT: u32 = 1 << 22;
const FIXED_COUNTER_RESET_BIT: u32 = 1 << 19;
const SNB_IMC_COUNTER_WIDTH: u32 = 48;
const SNB_IMC_CTL_OFFSETS: [u64; 4] = [0xd8, 0xdc, 0xe0, 0xe4];
const SNB_IMC_CTR_OFFSETS: [u64; 4] = [0xa0, 0xa8, 0xb0, 0xb8];
const SNB_IMC_DCLK_CTL_OFFSET: u64 = 0xf0;
const SNB_IMC_DCLK_CTR_OFFSET: u64 = 0xd0;
const SNB_IMC_UNIT_CTL_OFFSET: u64 = 0xf4;
const UBOX_GID_OFFSET: u64 = 0x54;
const UBOX_LOCAL_NODE_ID_OFFSET: u64 = 0x40;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u32 = 1 << 16;

const FIXED_COUNTER_RESET_AND_ENABLE: u32 = FIXED_COUNTER_RESET_BIT | FIXED_COUNTER_ENABLE_BIT;

const SNB_IMC_CHANNELS: [PciDeviceSpec; 4] = [
    PciDeviceSpec {
        device: 16,
        function: 0,
        device_id: 0x3cb0,
    },
    PciDeviceSpec {
        device: 16,
        function: 1,
        device_id: 0x3cb1,
    },
    PciDeviceSpec {
        device: 16,
        function: 4,
        device_id: 0x3cb4,
    },
    PciDeviceSpec {
        device: 16,
        function: 5,
        device_id: 0x3cb5,
    },
];

const IVB_IMC_CHANNELS: [PciDeviceSpec; 8] = [
    PciDeviceSpec {
        device: 16,
        function: 4,
        device_id: 0x0eb4,
    },
    PciDeviceSpec {
        device: 16,
        function: 5,
        device_id: 0x0eb5,
    },
    PciDeviceSpec {
        device: 16,
        function: 0,
        device_id: 0x0eb0,
    },
    PciDeviceSpec {
        device: 16,
        function: 1,
        device_id: 0x0eb1,
    },
    PciDeviceSpec {
        device: 30,
        function: 4,
        device_id: 0x0ef4,
    },
    PciDeviceSpec {
        device: 30,
        function: 5,
        device_id: 0x0ef5,
    },
    PciDeviceSpec {
        device: 30,
        function: 0,
        device_id: 0x0ef0,
    },
    PciDeviceSpec {
        device: 30,
        function: 1,
        device_id: 0x0ef1,
    },
];

const SNB_IMC_EVENT_GROUPS: [SnbImcEventGroup; 3] = [
    SnbImcEventGroup::read_write_queue(),
    SnbImcEventGroup {
        events: [
            SnbImcEventSpec::disabled(),
            SnbImcEventSpec::average(SnbImcEventKind::WpqFull, 0x22, 0x00),
            SnbImcEventSpec::sum(SnbImcEventKind::ReadCas, 0x04, 0x03),
            SnbImcEventSpec::sum(SnbImcEventKind::WriteCas, 0x04, 0x0c),
        ],
    },
    SnbImcEventGroup {
        events: [
            SnbImcEventSpec::sum(SnbImcEventKind::Activate, 0x01, 0x00),
            SnbImcEventSpec::sum(SnbImcEventKind::PageMissPrecharge, 0x02, 0x01),
            SnbImcEventSpec::disabled(),
            SnbImcEventSpec::disabled(),
        ],
    },
];

const IVB_IMC_EVENT_GROUPS: [SnbImcEventGroup; 3] = [
    SnbImcEventGroup::read_write_queue(),
    SnbImcEventGroup {
        events: [
            SnbImcEventSpec::average(SnbImcEventKind::WpqFull, 0x22, 0x00),
            SnbImcEventSpec::sum(SnbImcEventKind::ReadCas, 0x04, 0x03),
            SnbImcEventSpec::sum(SnbImcEventKind::WriteCas, 0x04, 0x0c),
            SnbImcEventSpec::disabled(),
        ],
    },
    SnbImcEventGroup {
        events: [
            SnbImcEventSpec::sum(SnbImcEventKind::Activate, 0x01, 0x0b),
            SnbImcEventSpec::sum(SnbImcEventKind::PageMissPrecharge, 0x02, 0x01),
            SnbImcEventSpec::disabled(),
            SnbImcEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbImcArchitecture {
    Ivb,
    Snb,
}

impl SnbImcArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::IvyTown => Some(Self::Ivb),
            IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
            _ => None,
        }
    }

    const fn channel_specs(self) -> &'static [PciDeviceSpec] {
        match self {
            Self::Ivb => &IVB_IMC_CHANNELS,
            Self::Snb => &SNB_IMC_CHANNELS,
        }
    }

    const fn event_groups(self) -> &'static [SnbImcEventGroup] {
        match self {
            Self::Ivb => &IVB_IMC_EVENT_GROUPS,
            Self::Snb => &SNB_IMC_EVENT_GROUPS,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ivb => "Ivy Bridge-EP",
            Self::Snb => "Sandy Bridge-EP",
        }
    }

    const fn ubox_device_id(self) -> u16 {
        match self {
            Self::Ivb => 0x0e1e,
            Self::Snb => 0x3ce0,
        }
    }

    const fn unit_freeze(self) -> u32 {
        UNIT_FREEZE_BIT | self.unit_freeze_enable()
    }

    const fn unit_freeze_and_reset(self) -> u32 {
        self.unit_freeze_enable()
            | UNIT_CONTROL_RESET_BIT
            | UNIT_COUNTER_RESET_BIT
            | UNIT_FREEZE_BIT
    }

    const fn unit_freeze_enable(self) -> u32 {
        match self {
            Self::Ivb => 0,
            Self::Snb => UNIT_FREEZE_ENABLE_BIT,
        }
    }

    const fn unit_unfreeze(self) -> u32 {
        self.unit_freeze_enable()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SnbImcScope {
    pub package_id: u32,
}

impl SnbImcScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SnbImcScopeMetrics {
    pub activate_commands_per_second: f64,
    pub frequency_hz: f64,
    pub page_miss_precharge_commands_per_second: f64,
    pub read_bytes_per_second: f64,
    pub read_cas_commands_per_second: f64,
    pub rpq_non_empty_ratio: f64,
    #[serde(flatten)]
    pub scope: SnbImcScope,
    pub write_bytes_per_second: f64,
    pub write_cas_commands_per_second: f64,
    pub wpq_full_ratio: f64,
    pub wpq_non_empty_ratio: f64,
}

impl SnbImcScopeMetrics {
    fn from_measurements(
        scope: SnbImcScope,
        measurements: &BTreeMap<SnbImcEventKind, SnbImcEventMeasurement>,
    ) -> Result<Self, String> {
        let activate = required_measurement(measurements, SnbImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(measurements, SnbImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(measurements, SnbImcEventKind::ReadCas)?;
        let read_insert = required_measurement(measurements, SnbImcEventKind::ReadInsert)?;
        let read_queue_non_empty =
            required_measurement(measurements, SnbImcEventKind::RpqNonEmpty)?;
        let write_cas = required_measurement(measurements, SnbImcEventKind::WriteCas)?;
        let write_queue_full = required_measurement(measurements, SnbImcEventKind::WpqFull)?;
        let write_queue_non_empty =
            required_measurement(measurements, SnbImcEventKind::WpqNonEmpty)?;

        Ok(Self {
            activate_commands_per_second: command_rate(activate),
            frequency_hz: frequency_hz(read_insert),
            page_miss_precharge_commands_per_second: command_rate(page_miss_precharge),
            read_bytes_per_second: bytes_per_second(read_cas),
            read_cas_commands_per_second: command_rate(read_cas),
            rpq_non_empty_ratio: queue_cycle_ratio(read_queue_non_empty),
            scope,
            write_bytes_per_second: bytes_per_second(write_cas),
            write_cas_commands_per_second: command_rate(write_cas),
            wpq_full_ratio: queue_cycle_ratio(write_queue_full),
            wpq_non_empty_ratio: queue_cycle_ratio(write_queue_non_empty),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnbImcMetrics {
    pub scopes: Vec<SnbImcScopeMetrics>,
}

impl SnbImcMetrics {
    fn from_measurements(
        measurements: BTreeMap<SnbImcScope, BTreeMap<SnbImcEventKind, SnbImcEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(SnbImcScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SnbImcCollector {
    architecture: SnbImcArchitecture,
    channels: Vec<SnbImcChannel>,
    next_group: usize,
}

impl SnbImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = SnbImcArchitecture::from_model(model).ok_or_else(|| {
            format!("Sandy/Ivy Bridge-EP IMC collection is not supported for {model:?}")
        })?;

        Ok(Self {
            architecture,
            channels: discover_channels(architecture)?,
            next_group: 0,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SnbImcMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IMC measure interval must be non-zero",
                self.architecture.name()
            ));
        }

        let mut measurements = SnbImcMeasurementAccumulator::new();
        let channels = &self.channels;

        for slice in self.schedule(interval) {
            program_channels(channels, slice.group)?;

            let started_at = Instant::now();
            unfreeze_channels(channels)?;
            tokio::time::sleep(slice.duration).await;
            freeze_channels(channels)?;

            read_channels(
                channels,
                SnbImcMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        SnbImcMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.architecture.event_groups().len();
    }

    fn schedule(&self, interval: Duration) -> Vec<SnbImcMeasurementSlice> {
        let event_groups = self.architecture.event_groups();
        let group_count = event_groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(SnbImcMeasurementSlice {
                    duration: slice_duration,
                    group: event_groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct SnbImcPrometheusMetrics {
    activate_commands_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    page_miss_precharge_commands_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_bytes_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_cas_commands_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_non_empty_ratio: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_bytes_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_cas_commands_per_second: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_full_ratio: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_non_empty_ratio: Family<SnbImcScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SnbImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            activate_commands_per_second:
                Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            page_miss_precharge_commands_per_second: Family::<
                SnbImcScopeLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            read_bytes_per_second: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_cas_commands_per_second:
                Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_non_empty_ratio: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_bytes_per_second: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_cas_commands_per_second:
                Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_full_ratio: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_non_empty_ratio: Family::<SnbImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
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
            "ocellus_imc_rpq_non_empty_ratio",
            "Average IMC read pending queue non-empty cycle ratio",
            metrics.rpq_non_empty_ratio.clone(),
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
            "ocellus_imc_wpq_full_ratio",
            "Average IMC write pending queue full cycle ratio",
            metrics.wpq_full_ratio.clone(),
        );
        registry.register(
            "ocellus_imc_wpq_non_empty_ratio",
            "Average IMC write pending queue non-empty cycle ratio",
            metrics.wpq_non_empty_ratio.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SnbImcMetrics) {
        for scope in metrics.scopes {
            let labels = SnbImcScopeLabels::from_scope(scope.scope);

            self.activate_commands_per_second
                .get_or_create(&labels)
                .set(scope.activate_commands_per_second);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.page_miss_precharge_commands_per_second
                .get_or_create(&labels)
                .set(scope.page_miss_precharge_commands_per_second);
            self.read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.read_bytes_per_second);
            self.read_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.read_cas_commands_per_second);
            self.rpq_non_empty_ratio
                .get_or_create(&labels)
                .set(scope.rpq_non_empty_ratio);
            self.write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.write_bytes_per_second);
            self.write_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.write_cas_commands_per_second);
            self.wpq_full_ratio
                .get_or_create(&labels)
                .set(scope.wpq_full_ratio);
            self.wpq_non_empty_ratio
                .get_or_create(&labels)
                .set(scope.wpq_non_empty_ratio);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbImcScopeLabels {
    package: String,
}

impl SnbImcScopeLabels {
    fn from_scope(scope: SnbImcScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct SnbImcChannel {
    architecture: SnbImcArchitecture,
    device: PciDevice,
    scope: SnbImcScope,
}

impl SnbImcChannel {
    fn new(spec: SnbImcChannelSpec) -> Result<Self, String> {
        Ok(Self {
            architecture: spec.architecture,
            device: PciDevice::open(spec.location)?,
            scope: spec.scope,
        })
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.device.write_u32(
            SNB_IMC_UNIT_CTL_OFFSET,
            self.architecture.unit_freeze_and_reset(),
        )
    }

    fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32(SNB_IMC_UNIT_CTL_OFFSET, self.architecture.unit_freeze())
    }

    fn program(&self, group: SnbImcEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            if event.kind.is_none() {
                continue;
            }

            self.device.write_u32(
                SNB_IMC_CTL_OFFSETS[counter_index],
                counter_control(event.event, event.umask),
            )?;
        }

        self.device
            .write_u32(SNB_IMC_DCLK_CTL_OFFSET, FIXED_COUNTER_RESET_AND_ENABLE)
    }

    fn read(&self) -> Result<SnbImcChannelReading, String> {
        Ok(SnbImcChannelReading {
            counters: [
                self.read_counter(0)?,
                self.read_counter(1)?,
                self.read_counter(2)?,
                self.read_counter(3)?,
            ],
            ticks: self.device.read_u64(SNB_IMC_DCLK_CTR_OFFSET)?,
        })
    }

    fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
        self.device
            .read_u64(SNB_IMC_CTR_OFFSETS[counter_index])
            .map(mask_counter)
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32(SNB_IMC_UNIT_CTL_OFFSET, self.architecture.unit_unfreeze())
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbImcChannelReading {
    counters: [u64; 4],
    ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct SnbImcChannelSpec {
    architecture: SnbImcArchitecture,
    location: metal::pci::PciLocation,
    scope: SnbImcScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbImcEventGroup {
    events: [SnbImcEventSpec; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbImcEventSpec {
    aggregate: SnbImcAggregate,
    event: u8,
    kind: Option<SnbImcEventKind>,
    umask: u8,
}

impl SnbImcEventSpec {
    const fn average(kind: SnbImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            aggregate: SnbImcAggregate::Average,
            event,
            kind: Some(kind),
            umask,
        }
    }

    const fn disabled() -> Self {
        Self {
            aggregate: SnbImcAggregate::Disabled,
            event: 0,
            kind: None,
            umask: 0,
        }
    }

    const fn sum(kind: SnbImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            aggregate: SnbImcAggregate::Sum,
            event,
            kind: Some(kind),
            umask,
        }
    }
}

impl SnbImcEventGroup {
    const fn read_write_queue() -> Self {
        Self {
            events: [
                SnbImcEventSpec::sum(SnbImcEventKind::ReadInsert, 0x10, 0x00),
                SnbImcEventSpec::sum(SnbImcEventKind::WriteInsert, 0x20, 0x00),
                SnbImcEventSpec::average(SnbImcEventKind::RpqNonEmpty, 0x11, 0x00),
                SnbImcEventSpec::average(SnbImcEventKind::WpqNonEmpty, 0x21, 0x00),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbImcAggregate {
    Average,
    Disabled,
    Sum,
}

impl SnbImcAggregate {
    fn aggregate(self, values: impl Iterator<Item = u64>) -> u64 {
        match self {
            Self::Average => average_u64(values),
            Self::Disabled => 0,
            Self::Sum => values.sum(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbImcEventKind {
    Activate,
    PageMissPrecharge,
    ReadCas,
    ReadInsert,
    RpqNonEmpty,
    WpqFull,
    WpqNonEmpty,
    WriteCas,
    WriteInsert,
}

#[derive(Clone, Copy, Debug)]
struct SnbImcEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl SnbImcEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbImcMeasurement {
    enabled: Duration,
    group: SnbImcEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbImcMeasurementSlice {
    duration: Duration,
    group: SnbImcEventGroup,
}

#[derive(Debug, Default)]
struct SnbImcMeasurementAccumulator {
    measurements: BTreeMap<SnbImcScope, BTreeMap<SnbImcEventKind, SnbImcEventMeasurement>>,
}

impl SnbImcMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: SnbImcScope,
        kind: SnbImcEventKind,
        value: u64,
        ticks: u64,
        measurement: SnbImcMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(SnbImcEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<SnbImcScope, BTreeMap<SnbImcEventKind, SnbImcEventMeasurement>> {
        self.measurements
    }
}

fn discover_channels(architecture: SnbImcArchitecture) -> Result<Vec<SnbImcChannel>, String> {
    let channel_specs = discover_channel_specs(architecture)?;
    let mut channels = Vec::with_capacity(channel_specs.len());

    for spec in channel_specs {
        channels.push(SnbImcChannel::new(spec)?);
    }

    Ok(channels)
}

fn discover_channel_specs(
    architecture: SnbImcArchitecture,
) -> Result<Vec<SnbImcChannelSpec>, String> {
    let bus_scopes = imc_bus_scopes(architecture)?;
    let channel_specs = architecture.channel_specs();
    let locations = metal::pci::find_intel_devices_matching_any_spec(architecture.channel_specs())?;
    let devices_by_location = metal::pci::find_intel_devices()?;

    let discovered_buses = imc_channel_buses(architecture)?;
    if discovered_buses.len() < bus_scopes.len() {
        return Err(format!(
            "discovered {} {} IMC buses for {} CPU packages",
            discovered_buses.len(),
            architecture.name(),
            bus_scopes.len()
        ));
    }

    let mut channels = Vec::with_capacity(locations.len());

    for bus_scope in bus_scopes {
        let bus_locations: Vec<_> = locations
            .iter()
            .copied()
            .filter(|location| {
                location.group == bus_scope.bus.group && location.bus == bus_scope.bus.bus
            })
            .collect();

        if bus_locations.is_empty() {
            return Err(format!(
                "failed to discover {} IMC channels on PCI bus {}",
                architecture.name(),
                bus_scope.bus
            ));
        }

        for location in bus_locations {
            let device = devices_by_location
                .iter()
                .find(|device| device.location == location)
                .copied()
                .ok_or_else(|| format!("failed to resolve device id for {location}"))?;
            channel_specs
                .iter()
                .position(|spec| matches_spec(device, spec))
                .ok_or_else(|| {
                    format!("unexpected {} IMC channel {location}", architecture.name())
                })?;

            channels.push(SnbImcChannelSpec {
                architecture,
                location,
                scope: bus_scope.scope,
            });
        }
    }

    if channels.is_empty() {
        return Err(format!(
            "failed to discover any {} IMC channels",
            architecture.name()
        ));
    }

    Ok(channels)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbImcBusScope {
    bus: PciBus,
    scope: SnbImcScope,
}

fn imc_bus_scopes(architecture: SnbImcArchitecture) -> Result<Vec<SnbImcBusScope>, String> {
    let scopes = imc_scopes()?;
    let socket_buses = imc_socket_buses(architecture, scopes.len())?;

    if socket_buses.len() != scopes.len() {
        return Err(format!(
            "discovered {} {} IMC buses for {} CPU packages",
            socket_buses.len(),
            architecture.name(),
            scopes.len()
        ));
    }

    socket_buses
        .into_iter()
        .map(|socket_bus| {
            let scope = scopes
                .get(socket_bus.package_index)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "failed to map {} IMC socket index {} to a CPUID package",
                        architecture.name(),
                        socket_bus.package_index
                    )
                })?;

            Ok(SnbImcBusScope {
                bus: socket_bus.bus,
                scope,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbImcSocketBus {
    bus: PciBus,
    package_index: usize,
}

fn imc_socket_buses(
    architecture: SnbImcArchitecture,
    socket_count: usize,
) -> Result<Vec<SnbImcSocketBus>, String> {
    let mut buses = package_buses_from_uboxes(architecture)?;

    if buses.len() < socket_count {
        buses = imc_channel_buses(architecture)?;
    }

    if buses.len() < socket_count {
        return Err(format!(
            "discovered {} {} IMC buses for {socket_count} CPU packages",
            buses.len(),
            architecture.name()
        ));
    }

    buses.truncate(socket_count);

    Ok(buses
        .into_iter()
        .enumerate()
        .map(|(package_index, bus)| SnbImcSocketBus { bus, package_index })
        .collect())
}

fn package_buses_from_uboxes(architecture: SnbImcArchitecture) -> Result<Vec<PciBus>, String> {
    let mut buses = Vec::new();

    for location in
        metal::pci::find_intel_devices_matching_device_id(architecture.ubox_device_id())?
    {
        let device = PciDevice::open_readonly(location)?;
        let local_node_id = device.read_u32(UBOX_LOCAL_NODE_ID_OFFSET)? & 0x7;
        let node_mapping = device.read_u32(UBOX_GID_OFFSET)?;
        let Some(package_index) = package_index_from_node_mapping(local_node_id, node_mapping)
        else {
            continue;
        };

        buses.push((
            usize::try_from(package_index).unwrap_or(usize::MAX),
            PciBus {
                bus: location.bus,
                group: location.group,
            },
        ));
    }

    buses.sort_by_key(|(package_index, bus)| (*package_index, bus.group, bus.bus));
    buses.dedup_by_key(|(package_index, _)| *package_index);

    Ok(buses.into_iter().map(|(_, bus)| bus).collect())
}

fn imc_channel_buses(architecture: SnbImcArchitecture) -> Result<Vec<PciBus>, String> {
    let locations = metal::pci::find_intel_devices_matching_any_spec(architecture.channel_specs())?;
    let mut buses = Vec::<PciBus>::new();

    for location in locations {
        let bus = PciBus {
            bus: location.bus,
            group: location.group,
        };

        if !buses.contains(&bus) {
            buses.push(bus);
        }
    }

    Ok(buses)
}

fn package_index_from_node_mapping(local_node_id: u32, node_mapping: u32) -> Option<u32> {
    (0..8).find(|package_index| ((node_mapping >> (package_index * 3)) & 0x7) == local_node_id)
}

fn matches_spec(device: PciVendorDevice, spec: &PciDeviceSpec) -> bool {
    spec.device == device.location.device
        && spec.function == device.location.function
        && spec.device_id == device.device_id
}

fn imc_scopes() -> Result<Vec<SnbImcScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(SnbImcScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any Sandy/Ivy Bridge-EP IMC scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
}

fn program_channels(channels: &[SnbImcChannel], group: SnbImcEventGroup) -> Result<(), String> {
    for channel in channels {
        channel.freeze_and_reset()?;
    }

    for channel in channels {
        channel.program(group)?;
    }

    Ok(())
}

fn read_channels(
    channels: &[SnbImcChannel],
    measurement: SnbImcMeasurement,
    measurements: &mut SnbImcMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<SnbImcScope, Vec<SnbImcChannelReading>>::new();

    for channel in channels {
        readings
            .entry(channel.scope)
            .or_default()
            .push(channel.read()?);
    }

    for (scope, channel_readings) in readings {
        let ticks = average_u64(channel_readings.iter().map(|reading| reading.ticks));

        for counter_index in 0..measurement.group.events.len() {
            let event = measurement.group.events[counter_index];
            let Some(kind) = event.kind else {
                continue;
            };
            let value = event.aggregate.aggregate(
                channel_readings
                    .iter()
                    .map(|reading| reading.counters[counter_index]),
            );

            measurements.add(scope, kind, value, ticks, measurement);
        }
    }

    Ok(())
}

fn freeze_channels(channels: &[SnbImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.freeze()?;
    }

    Ok(())
}

fn unfreeze_channels(channels: &[SnbImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unfreeze()?;
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<SnbImcEventKind, SnbImcEventMeasurement>,
    kind: SnbImcEventKind,
) -> Result<&SnbImcEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IMC measurement {kind:?} is missing"))
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

fn bytes_per_second(measurement: &SnbImcEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    ) * BYTES_PER_CACHE_LINE
}

fn command_rate(measurement: &SnbImcEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn counter_control(event: u8, umask: u8) -> u32 {
    u32::from(event)
        | (u32::from(umask) << 8)
        | COUNTER_RESET_BIT
        | COUNTER_OVERFLOW_ENABLE_BIT
        | COUNTER_ENABLE_BIT
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn frequency_hz(measurement: &SnbImcEventMeasurement) -> f64 {
    events_per_second(measurement.ticks, measurement.running)
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << SNB_IMC_COUNTER_WIDTH) - 1)
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn queue_cycle_ratio(measurement: &SnbImcEventMeasurement) -> f64 {
    ratio(measurement.value, measurement.ticks)
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_snb_imc_metrics() {
        let scope = test_scope();
        let metrics = SnbImcMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(SnbImcEventKind::Activate, 1_000, 1_000, 100),
                measurement(SnbImcEventKind::PageMissPrecharge, 700, 1_000, 100),
                measurement(SnbImcEventKind::ReadCas, 2_000, 1_000, 100),
                measurement(SnbImcEventKind::ReadInsert, 200, 1_000, 100),
                measurement(SnbImcEventKind::RpqNonEmpty, 400, 1_000, 100),
                measurement(SnbImcEventKind::WriteCas, 3_000, 1_000, 100),
                measurement(SnbImcEventKind::WriteInsert, 300, 1_000, 100),
                measurement(SnbImcEventKind::WpqFull, 30, 1_000, 100),
                measurement(SnbImcEventKind::WpqNonEmpty, 600, 1_000, 100),
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
        assert_eq!(scope_metrics.rpq_non_empty_ratio, 0.4);
        assert_eq!(scope_metrics.wpq_full_ratio, 0.03);
        assert_eq!(scope_metrics.wpq_non_empty_ratio, 0.6);
    }

    #[test]
    fn averages_queue_cycle_events_across_channels() {
        let group = SNB_IMC_EVENT_GROUPS[0];
        let readings = [
            SnbImcChannelReading {
                counters: [100, 200, 400, 600],
                ticks: 1_000,
            },
            SnbImcChannelReading {
                counters: [300, 400, 800, 1_000],
                ticks: 2_000,
            },
        ];

        assert_eq!(
            group.events[0]
                .aggregate
                .aggregate(readings.iter().map(|reading| reading.counters[0])),
            400
        );
        assert_eq!(
            group.events[2]
                .aggregate
                .aggregate(readings.iter().map(|reading| reading.counters[2])),
            600
        );
    }

    #[test]
    fn schedules_short_interval_once_per_group() {
        let collector = test_collector(SnbImcArchitecture::Snb);
        let slices = collector.schedule(Duration::from_millis(100));

        assert_eq!(slices.len(), SNB_IMC_EVENT_GROUPS.len());
        assert_eq!(slices[0].group, SNB_IMC_EVENT_GROUPS[0]);
        assert_eq!(slices[1].group, SNB_IMC_EVENT_GROUPS[1]);
        assert_eq!(slices[2].group, SNB_IMC_EVENT_GROUPS[2]);
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector(SnbImcArchitecture::Snb);

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            SNB_IMC_EVENT_GROUPS.to_vec()
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                SNB_IMC_EVENT_GROUPS[1],
                SNB_IMC_EVENT_GROUPS[2],
                SNB_IMC_EVENT_GROUPS[0],
            ]
        );
    }

    #[test]
    fn uses_documented_snb_ivb_imc_event_encodings() {
        assert_event(SnbImcEventKind::ReadInsert, 0x10, 0x00);
        assert_event(SnbImcEventKind::WriteInsert, 0x20, 0x00);
        assert_event(SnbImcEventKind::RpqNonEmpty, 0x11, 0x00);
        assert_event(SnbImcEventKind::WpqNonEmpty, 0x21, 0x00);
        assert_event(SnbImcEventKind::WpqFull, 0x22, 0x00);
        assert_event(SnbImcEventKind::ReadCas, 0x04, 0x03);
        assert_event(SnbImcEventKind::WriteCas, 0x04, 0x0c);
        assert_event_in(&SNB_IMC_EVENT_GROUPS, SnbImcEventKind::Activate, 0x01, 0x00);
        assert_event_in(&IVB_IMC_EVENT_GROUPS, SnbImcEventKind::Activate, 0x01, 0x0b);
        assert_event(SnbImcEventKind::PageMissPrecharge, 0x02, 0x01);
    }

    #[test]
    fn uses_snb_ivb_pci_device_ids() {
        assert_eq!(
            spec_addresses(&SNB_IMC_CHANNELS),
            vec![
                (16, 0, 0x3cb0),
                (16, 1, 0x3cb1),
                (16, 4, 0x3cb4),
                (16, 5, 0x3cb5),
            ]
        );
        assert_eq!(SnbImcArchitecture::Snb.ubox_device_id(), 0x3ce0);
        assert_eq!(
            spec_addresses(&IVB_IMC_CHANNELS),
            vec![
                (16, 4, 0x0eb4),
                (16, 5, 0x0eb5),
                (16, 0, 0x0eb0),
                (16, 1, 0x0eb1),
                (30, 4, 0x0ef4),
                (30, 5, 0x0ef5),
                (30, 0, 0x0ef0),
                (30, 1, 0x0ef1),
            ]
        );
        assert_eq!(SnbImcArchitecture::Ivb.ubox_device_id(), 0x0e1e);
    }

    #[test]
    fn encodes_snb_ivb_unit_control_values() {
        assert_eq!(SnbImcArchitecture::Snb.unit_freeze(), 0x10100);
        assert_eq!(SnbImcArchitecture::Snb.unit_freeze_and_reset(), 0x10103);
        assert_eq!(SnbImcArchitecture::Snb.unit_unfreeze(), 0x10000);
        assert_eq!(SnbImcArchitecture::Ivb.unit_freeze(), 0x100);
        assert_eq!(SnbImcArchitecture::Ivb.unit_freeze_and_reset(), 0x103);
        assert_eq!(SnbImcArchitecture::Ivb.unit_unfreeze(), 0);
    }

    #[test]
    fn encodes_event_and_fixed_counter_control_values() {
        assert_eq!(
            counter_control(0x10, 0x20),
            0x10 | (0x20 << 8) | (1 << 17) | (1 << 20) | (1 << 22)
        );
        assert_eq!(FIXED_COUNTER_RESET_AND_ENABLE, (1 << 19) | (1 << 22));
    }

    #[test]
    fn maps_node_id_to_package_index() {
        assert_eq!(package_index_from_node_mapping(0, 0b010_001_000), Some(0));
        assert_eq!(package_index_from_node_mapping(1, 0b010_001_000), Some(1));
        assert_eq!(package_index_from_node_mapping(2, 0b010_001_000), Some(2));
        assert_eq!(package_index_from_node_mapping(3, 0b010_001_000), None);
    }

    #[test]
    fn wraps_48_bit_counters() {
        assert_eq!(mask_counter((1_u64 << 50) | 7), 7);
    }

    #[test]
    fn scales_counts_to_enabled_time() {
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::from_millis(100)),
            1_000
        );
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::ZERO),
            0
        );
    }

    #[test]
    fn accumulates_repeated_multiplex_slices() {
        let mut accumulator = SnbImcMeasurementAccumulator::new();
        let scope = test_scope();

        accumulator.add(
            scope,
            SnbImcEventKind::ReadCas,
            100,
            1_000,
            test_measurement(Duration::from_secs(1), Duration::from_millis(100)),
        );
        accumulator.add(
            scope,
            SnbImcEventKind::ReadCas,
            200,
            2_000,
            test_measurement(Duration::from_secs(1), Duration::from_millis(200)),
        );

        let measurements = accumulator.into_measurements();
        let measurement = measurements
            .get(&scope)
            .unwrap()
            .get(&SnbImcEventKind::ReadCas)
            .unwrap();

        assert_eq!(measurement.value, 300);
        assert_eq!(measurement.ticks, 3_000);
        assert_eq!(measurement.enabled, Duration::from_secs(1));
        assert_eq!(measurement.running, Duration::from_millis(300));
    }

    fn measurement(
        kind: SnbImcEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (SnbImcEventKind, SnbImcEventMeasurement) {
        (
            kind,
            SnbImcEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                ticks,
                value,
            },
        )
    }

    fn assert_event(kind: SnbImcEventKind, event: u8, umask: u8) {
        assert_event_in(&SNB_IMC_EVENT_GROUPS, kind, event, umask);
        assert_event_in(&IVB_IMC_EVENT_GROUPS, kind, event, umask);
    }

    fn assert_event_in(
        event_groups: &[SnbImcEventGroup],
        kind: SnbImcEventKind,
        event: u8,
        umask: u8,
    ) {
        let actual = event_groups
            .iter()
            .flat_map(|group| group.events)
            .find(|event| event.kind == Some(kind))
            .unwrap();

        assert_eq!(actual.event, event);
        assert_eq!(actual.umask, umask);
    }

    fn spec_addresses(specs: &[PciDeviceSpec]) -> Vec<(u8, u8, u16)> {
        specs
            .iter()
            .map(|spec| (spec.device, spec.function, spec.device_id))
            .collect()
    }

    fn slice_groups(slices: Vec<SnbImcMeasurementSlice>) -> Vec<SnbImcEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }

    fn test_collector(architecture: SnbImcArchitecture) -> SnbImcCollector {
        SnbImcCollector {
            architecture,
            channels: Vec::new(),
            next_group: 0,
        }
    }

    fn test_measurement(enabled: Duration, running: Duration) -> SnbImcMeasurement {
        SnbImcMeasurement {
            enabled,
            group: SNB_IMC_EVENT_GROUPS[0],
            running,
        }
    }

    fn test_scope() -> SnbImcScope {
        SnbImcScope { package_id: 0 }
    }
}
