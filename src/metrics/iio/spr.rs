use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metal::msr::Msr;
use crate::metrics::common::topology_label;
use crate::metrics::uncore::skx::{
    SKX_UNCORE_COUNTER_WIDTH, UncoreScope, events_per_second, frequency_hz, mask_counter,
    measurement_round_count, queue_residency_seconds, ratio, scale_to_enabled, uncore_leaders,
    wrapping_delta,
};

const IIO_CHANNEL_MASK_SHIFT: u32 = 36;
const IIO_FUNCTION_CLASS_MASK_SHIFT: u32 = 48;
const IIO_COUNTER_COUNT: usize = 4;
const IIO_PCIE_PORT_COUNT: usize = 8;
const IIO_UNIT_COUNT: usize = 12;
const IIO_FREE_RUNNING_COUNTER_WIDTH: u32 = 48;
const SPR_IIO_DISCOVERY_BOX_TYPE: u16 = 1;
const PCIE_COUNTER_BYTES: f64 = 32.0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 9;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 8;
const UNIT_FREEZE_BIT: u32 = 1 << 0;

const UNIT_FREEZE: u32 = UNIT_FREEZE_BIT;
const UNIT_FREEZE_AND_COUNTER_RESET: u32 = UNIT_FREEZE_BIT | UNIT_COUNTER_RESET_BIT;
const UNIT_FREEZE_AND_CONTROL_RESET: u32 = UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT;
const UNIT_UNFREEZE: u32 = 0;

pub const SPR_IIO_STACKS: [SprIioStack; IIO_UNIT_COUNT] = [
    SprIioStack::new(0, "m2iosf0", 0x3000, 0x3008, 0x3002, 0x340e, 0x3800, 0x3808),
    SprIioStack::new(1, "m2iosf1", 0x3010, 0x3018, 0x3012, 0x341e, 0x3810, 0x3818),
    SprIioStack::new(2, "m2iosf2", 0x3020, 0x3028, 0x3022, 0x342e, 0x3820, 0x3828),
    SprIioStack::new(3, "m2iosf3", 0x3030, 0x3038, 0x3032, 0x343e, 0x3830, 0x3838),
    SprIioStack::new(4, "m2iosf4", 0x3040, 0x3048, 0x3042, 0x344e, 0x3840, 0x3848),
    SprIioStack::new(5, "m2iosf5", 0x3050, 0x3058, 0x3052, 0x345e, 0x3850, 0x3858),
    SprIioStack::new(6, "m2iosf6", 0x3060, 0x3068, 0x3062, 0x346e, 0x3860, 0x3868),
    SprIioStack::new(7, "m2iosf7", 0x3070, 0x3078, 0x3072, 0x347e, 0x3870, 0x3878),
    SprIioStack::new(8, "m2iosf8", 0x3080, 0x3088, 0x3082, 0x348e, 0x3880, 0x3888),
    SprIioStack::new(9, "m2iosf9", 0x3090, 0x3098, 0x3092, 0x349e, 0x3890, 0x3898),
    SprIioStack::new(
        10, "m2iosf10", 0x30a0, 0x30a8, 0x30a2, 0x34ae, 0x38a0, 0x38a8,
    ),
    SprIioStack::new(
        11, "m2iosf11", 0x30b0, 0x30b8, 0x30b2, 0x34be, 0x38b0, 0x38b8,
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IioEventKind {
    Clockticks,
    CompletionInserts,
    CompletionOccupancy,
    InboundReadDwords,
    InboundReadTransactions,
    InboundWriteDwords,
    InboundWriteTransactions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IioEventSpec {
    channel_mask: u16,
    event: u8,
    function_class_mask: u8,
    kind: IioEventKind,
    umask: u8,
}

impl IioEventSpec {
    const fn sum(
        kind: IioEventKind,
        event: u8,
        umask: u8,
        channel_mask: u16,
        function_class_mask: u8,
    ) -> Self {
        Self {
            channel_mask,
            event,
            function_class_mask,
            kind,
            umask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IioEventGroup {
    events: [IioEventSpec; IIO_COUNTER_COUNT],
}

const SPR_IIO_EVENT_GROUPS: [IioEventGroup; 2] = [
    IioEventGroup {
        events: [
            IioEventSpec::sum(IioEventKind::InboundWriteDwords, 0x83, 0x01, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::InboundReadDwords, 0x83, 0x04, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::CompletionOccupancy, 0xd5, 0xff, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00),
        ],
    },
    IioEventGroup {
        events: [
            IioEventSpec::sum(
                IioEventKind::InboundWriteTransactions,
                0x84,
                0x01,
                0x00ff,
                0x07,
            ),
            IioEventSpec::sum(
                IioEventKind::InboundReadTransactions,
                0x84,
                0x04,
                0x00ff,
                0x07,
            ),
            IioEventSpec::sum(IioEventKind::CompletionInserts, 0xc2, 0x04, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SprIioStack {
    id: usize,
    label: &'static str,
    unit_control: u64,
    counter_base: u64,
    control_base: u64,
    clock_counter: u64,
    pcie_read_base: u64,
    pcie_write_base: u64,
}

impl SprIioStack {
    const fn new(
        id: usize,
        label: &'static str,
        unit_control: u64,
        counter_base: u64,
        control_base: u64,
        clock_counter: u64,
        pcie_read_base: u64,
        pcie_write_base: u64,
    ) -> Self {
        Self {
            id,
            label,
            unit_control,
            counter_base,
            control_base,
            clock_counter,
            pcie_read_base,
            pcie_write_base,
        }
    }

    pub const fn label(self) -> &'static str {
        self.label
    }

    fn from_box_id(box_id: u16) -> Option<Self> {
        SPR_IIO_STACKS
            .iter()
            .copied()
            .find(|stack| stack.id == usize::from(box_id))
    }
}

impl serde::Serialize for SprIioStack {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SprIioPciePortMetrics {
    pub port_id: u32,
    pub read_bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub stack: SprIioStack,
    pub write_bytes_per_second: f64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SprIioScopeMetrics {
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub completion_inserts_per_second: f64,
    pub completion_latency_seconds: f64,
    pub completion_occupancy_entries: f64,
    pub frequency_hz: f64,
    pub stack: SprIioStack,
    pub inbound_read_bytes_per_second: f64,
    pub inbound_reads_per_second: f64,
    pub inbound_write_bytes_per_second: f64,
    pub inbound_writes_per_second: f64,
}

impl SprIioScopeMetrics {
    fn from_measurements(
        stack_scope: IioStackScope,
        measurements: &BTreeMap<IioEventKind, IioEventMeasurement>,
    ) -> Result<Self, String> {
        let clockticks = required_measurement(measurements, IioEventKind::Clockticks)?;
        let completion_inserts =
            required_measurement(measurements, IioEventKind::CompletionInserts)?;
        let completion_occupancy =
            required_measurement(measurements, IioEventKind::CompletionOccupancy)?;
        let inbound_read_dwords =
            required_measurement(measurements, IioEventKind::InboundReadDwords)?;
        let inbound_read_transactions =
            required_measurement(measurements, IioEventKind::InboundReadTransactions)?;
        let inbound_write_dwords =
            required_measurement(measurements, IioEventKind::InboundWriteDwords)?;
        let inbound_write_transactions =
            required_measurement(measurements, IioEventKind::InboundWriteTransactions)?;

        Ok(Self {
            scope: stack_scope.scope,
            completion_inserts_per_second: event_rate(completion_inserts),
            completion_latency_seconds: completion_latency_seconds(
                completion_occupancy,
                completion_inserts,
                clockticks,
            ),
            completion_occupancy_entries: ratio(
                completion_occupancy.value,
                completion_occupancy.ticks,
            ),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            inbound_read_bytes_per_second: dwords_per_second(inbound_read_dwords),
            inbound_reads_per_second: event_rate(inbound_read_transactions),
            inbound_write_bytes_per_second: dwords_per_second(inbound_write_dwords),
            inbound_writes_per_second: event_rate(inbound_write_transactions),
            stack: stack_scope.stack,
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SprIioMetrics {
    pub ports: Vec<SprIioPciePortMetrics>,
    pub scopes: Vec<SprIioScopeMetrics>,
}

impl SprIioMetrics {
    fn from_measurements(
        measurements: BTreeMap<IioStackScope, BTreeMap<IioEventKind, IioEventMeasurement>>,
        ports: Vec<SprIioPciePortMetrics>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(SprIioScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { ports, scopes })
    }
}

#[derive(Debug)]
pub struct SprIioCollector {
    next_group: usize,
    packages: Vec<IioPackage>,
}

impl SprIioCollector {
    pub fn new() -> Result<Self, String> {
        let packages = discover_packages()?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SprIioMetrics, String> {
        if interval.is_zero() {
            return Err("IIO measure interval must be non-zero".to_string());
        }

        let mut measurements = IioMeasurementAccumulator::new();
        let schedule = self.schedule(interval);
        let packages = &mut self.packages;

        for slice in schedule {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                IioMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        let ports = read_pcie_ports(packages)?;
        self.rotate_group();

        SprIioMetrics::from_measurements(measurements.into_measurements(), ports)
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % SPR_IIO_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IioMeasurementSlice> {
        let group_count = SPR_IIO_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IioMeasurementSlice {
                    duration: slice_duration,
                    group: SPR_IIO_EVENT_GROUPS[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IioMeasurementSlice {
    duration: Duration,
    group: IioEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct IioPciePortLabels {
    die: String,
    die_group: String,
    package: String,
    port: String,
    stack: String,
}

impl IioPciePortLabels {
    fn from_port(port: SprIioPciePortMetrics) -> Self {
        Self {
            die: topology_label(port.scope.die_id),
            die_group: topology_label(port.scope.die_group_id),
            package: port.scope.package_id.to_string(),
            port: port.port_id.to_string(),
            stack: port.stack.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct IioScopeLabels {
    die: String,
    die_group: String,
    package: String,
    stack: String,
}

impl IioScopeLabels {
    fn from_scope(scope: UncoreScope, stack: SprIioStack) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct SprIioPrometheusMetrics {
    completion_inserts_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_latency_seconds: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_occupancy_entries: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_read_bytes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_reads_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_write_bytes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_writes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_bytes_per_second: Family<IioPciePortLabels, Gauge<f64, AtomicU64>>,
    pcie_write_bytes_per_second: Family<IioPciePortLabels, Gauge<f64, AtomicU64>>,
}

impl SprIioPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            completion_inserts_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            completion_latency_seconds: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            completion_occupancy_entries: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            frequency_hz: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            inbound_read_bytes_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            inbound_reads_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            inbound_write_bytes_per_second:
                Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            inbound_writes_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_read_bytes_per_second: Family::<IioPciePortLabels, Gauge<f64, AtomicU64>>::default(
            ),
            pcie_write_bytes_per_second:
                Family::<IioPciePortLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_iio_completion_inserts_per_second",
            "Interval-derived IIO completion inserts per second",
            metrics.completion_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_completion_latency_seconds",
            "Interval-derived IIO completion residency latency in seconds",
            metrics.completion_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_iio_completion_occupancy_entries",
            "Average IIO completion occupancy in entries",
            metrics.completion_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_iio_frequency_hz",
            "Interval-derived IIO clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_iio_inbound_read_bytes_per_second",
            "Interval-derived IIO PCIe inbound read payload bandwidth in bytes per second",
            metrics.inbound_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_inbound_reads_per_second",
            "Interval-derived IIO PCIe inbound read transactions per second",
            metrics.inbound_reads_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_inbound_write_bytes_per_second",
            "Interval-derived IIO PCIe inbound write payload bandwidth in bytes per second",
            metrics.inbound_write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_inbound_writes_per_second",
            "Interval-derived IIO PCIe inbound write transactions per second",
            metrics.inbound_writes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_pcie_read_bytes_per_second",
            "IIO free-running PCIe inbound bandwidth in bytes per second",
            metrics.pcie_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_pcie_write_bytes_per_second",
            "IIO free-running PCIe outbound bandwidth in bytes per second",
            metrics.pcie_write_bytes_per_second.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SprIioMetrics) {
        for scope in metrics.scopes {
            let labels = IioScopeLabels::from_scope(scope.scope, scope.stack);

            self.completion_inserts_per_second
                .get_or_create(&labels)
                .set(scope.completion_inserts_per_second);
            self.completion_latency_seconds
                .get_or_create(&labels)
                .set(scope.completion_latency_seconds);
            self.completion_occupancy_entries
                .get_or_create(&labels)
                .set(scope.completion_occupancy_entries);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.inbound_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.inbound_read_bytes_per_second);
            self.inbound_reads_per_second
                .get_or_create(&labels)
                .set(scope.inbound_reads_per_second);
            self.inbound_write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.inbound_write_bytes_per_second);
            self.inbound_writes_per_second
                .get_or_create(&labels)
                .set(scope.inbound_writes_per_second);
        }

        for port in metrics.ports {
            let labels = IioPciePortLabels::from_port(port);

            self.pcie_read_bytes_per_second
                .get_or_create(&labels)
                .set(port.read_bytes_per_second);
            self.pcie_write_bytes_per_second
                .get_or_create(&labels)
                .set(port.write_bytes_per_second);
        }
    }
}

#[derive(Debug)]
struct IioPackage {
    free_running: IioFreeRunningCounters,
    scope: UncoreScope,
    units: Vec<IioUnit>,
}

impl IioPackage {
    fn new(cpu: u32, scope: UncoreScope, stacks: Vec<SprIioStack>) -> Self {
        Self {
            free_running: IioFreeRunningCounters::new(cpu),
            scope,
            units: stacks
                .into_iter()
                .map(|stack| IioUnit { cpu, stack })
                .collect(),
        }
    }

    fn cpu(&self) -> u32 {
        self.free_running.cpu
    }
}

#[derive(Debug)]
struct IioFreeRunningCounters {
    cpu: u32,
    previous: Option<(Instant, Vec<u64>)>,
}

impl IioFreeRunningCounters {
    fn new(cpu: u32) -> Self {
        Self {
            cpu,
            previous: None,
        }
    }

    fn sample(
        &mut self,
        scope: UncoreScope,
        stacks: &[SprIioStack],
    ) -> Result<Vec<SprIioPciePortMetrics>, String> {
        let current = (Instant::now(), self.read(stacks)?);
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(Vec::new()),
        };

        pcie_metrics_from_readings(scope, stacks, previous, current)
    }

    fn read(&self, stacks: &[SprIioStack]) -> Result<Vec<u64>, String> {
        let mut values = Vec::with_capacity(stacks.len() * IIO_PCIE_PORT_COUNT * 2);

        for stack in stacks.iter().copied() {
            for port_index in 0..IIO_PCIE_PORT_COUNT {
                values.push(read_pcie_counter(
                    self.cpu,
                    iio_pcie_read_counter_offset(stack, port_index),
                )?);
            }
        }
        for stack in stacks.iter().copied() {
            for port_index in 0..IIO_PCIE_PORT_COUNT {
                values.push(read_pcie_counter(
                    self.cpu,
                    iio_pcie_write_counter_offset(stack, port_index),
                )?);
            }
        }
        Ok(values)
    }
}

#[derive(Clone, Copy, Debug)]
struct IioUnit {
    cpu: u32,
    stack: SprIioStack,
}

impl IioUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))?;
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_CONTROL_RESET))?;
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_COUNTER_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))
    }

    fn program(self, group: IioEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            let control = iio_counter_control(
                event.event,
                event.umask,
                event.channel_mask,
                event.function_class_mask,
                true,
            );

            msr.write(iio_control_offset(self.stack, counter_index), control)?;
        }

        Ok(())
    }

    fn read(self) -> Result<IioUnitReading, String> {
        Ok(IioUnitReading {
            counters: [
                self.read_counter(0).map(mask_iio_counter)?,
                self.read_counter(1).map(mask_iio_counter)?,
                self.read_counter(2).map(mask_iio_counter)?,
                self.read_counter(3).map(mask_iio_counter)?,
            ],
            ticks: Msr::open_readonly(self.cpu)?
                .read(iio_clock_counter_offset(self.stack))
                .map(mask_iio_clock_counter)?,
        })
    }

    fn stack(self) -> SprIioStack {
        self.stack
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_UNFREEZE))
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(iio_counter_offset(self.stack, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(iio_unit_control_offset(self.stack), value)
    }
}

#[derive(Clone, Copy, Debug)]
struct IioUnitReading {
    counters: [u64; IIO_COUNTER_COUNT],
    ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct IioEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl IioEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct IioMeasurement {
    enabled: Duration,
    group: IioEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IioStackScope {
    scope: UncoreScope,
    stack: SprIioStack,
}

#[derive(Debug, Default)]
struct IioMeasurementAccumulator {
    measurements: BTreeMap<IioStackScope, BTreeMap<IioEventKind, IioEventMeasurement>>,
}

impl IioMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: UncoreScope,
        stack: SprIioStack,
        kind: IioEventKind,
        value: u64,
        ticks: u64,
        measurement: IioMeasurement,
    ) {
        self.measurements
            .entry(IioStackScope { scope, stack })
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(IioEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<IioStackScope, BTreeMap<IioEventKind, IioEventMeasurement>> {
        self.measurements
    }
}

fn dwords_per_second(measurement: &IioEventMeasurement) -> f64 {
    event_rate(measurement) * 4.0
}

fn iio_counter_control(
    event: u8,
    umask: u8,
    channel_mask: u16,
    function_class_mask: u8,
    _overflow_enabled: bool,
) -> u64 {
    u64::from(event)
        | (u64::from(umask) << 8)
        | (u64::from(channel_mask) << IIO_CHANNEL_MASK_SHIFT)
        | (u64::from(function_class_mask) << IIO_FUNCTION_CLASS_MASK_SHIFT)
}

fn discover_packages() -> Result<Vec<IioPackage>, String> {
    let stacks_by_scope = discover_iio_stacks()?;
    let leaders = uncore_leaders()?;
    let packages = leaders
        .into_iter()
        .filter_map(|leader| {
            stacks_by_scope
                .iter()
                .find(|(scope, _)| *scope == leader.scope)
                .map(|(_, stacks)| IioPackage::new(leader.cpu, leader.scope, stacks.clone()))
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any IIO packages".to_string());
    }

    Ok(packages)
}

fn discover_iio_stacks() -> Result<Vec<(UncoreScope, Vec<SprIioStack>)>, String> {
    let socket_boxes =
        crate::metrics::uncore::spr::discover_uncore_boxes(SPR_IIO_DISCOVERY_BOX_TYPE)?;
    let mut stacks_by_scope = Vec::new();

    for socket_boxes in socket_boxes {
        let mut stacks = socket_boxes
            .boxes
            .into_iter()
            .filter(|box_pmu| box_pmu.access_type == 0)
            .filter_map(|box_pmu| SprIioStack::from_box_id(box_pmu.box_id))
            .collect::<Vec<_>>();
        stacks.sort_by_key(|stack| stack.id);
        stacks.dedup();

        if !stacks.is_empty() {
            stacks_by_scope.push((socket_boxes.scope, stacks));
        }
    }

    if stacks_by_scope.is_empty() {
        return Err(
            "failed to discover any Sapphire Rapids IIO stacks from PMU discovery".to_string(),
        );
    }

    Ok(stacks_by_scope)
}

fn event_rate(measurement: &IioEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn completion_latency_seconds(
    occupancy: &IioEventMeasurement,
    inserts: &IioEventMeasurement,
    clockticks: &IioEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let insert_count = scale_to_enabled(inserts.value, inserts.enabled, inserts.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    queue_residency_seconds(occupancy, insert_count, clockticks, inserts.enabled)
}

fn freeze_packages(packages: &[IioPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn iio_clock_counter_offset(stack: SprIioStack) -> u64 {
    stack.clock_counter
}

fn iio_control_offset(stack: SprIioStack, counter_index: usize) -> u64 {
    stack.control_base + counter_index as u64
}

fn iio_counter_offset(stack: SprIioStack, counter_index: usize) -> u64 {
    stack.counter_base + counter_index as u64
}

fn iio_pcie_read_counter_offset(stack: SprIioStack, port_index: usize) -> u64 {
    stack.pcie_read_base + port_index as u64
}

fn iio_pcie_write_counter_offset(stack: SprIioStack, port_index: usize) -> u64 {
    stack.pcie_write_base + port_index as u64
}

fn iio_unit_control_offset(stack: SprIioStack) -> u64 {
    stack.unit_control
}

fn mask_iio_clock_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn mask_iio_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn pcie_metrics_from_readings(
    scope: UncoreScope,
    stacks: &[SprIioStack],
    previous: (Instant, Vec<u64>),
    current: (Instant, Vec<u64>),
) -> Result<Vec<SprIioPciePortMetrics>, String> {
    let expected_count = stacks.len() * IIO_PCIE_PORT_COUNT * 2;
    if previous.1.len() != expected_count || current.1.len() != expected_count {
        return Err("IIO PCIe reading length does not match counter count".to_string());
    }

    let elapsed = current
        .0
        .checked_duration_since(previous.0)
        .ok_or_else(|| "IIO PCIe sample timestamp moved backwards".to_string())?;
    let elapsed_seconds = elapsed.as_secs_f64();
    if elapsed_seconds == 0.0 {
        return Err("IIO PCIe sample elapsed time is zero".to_string());
    }

    let mut ports = Vec::with_capacity(stacks.len() * IIO_PCIE_PORT_COUNT);
    let write_offset = stacks.len() * IIO_PCIE_PORT_COUNT;

    for (stack_index, stack) in stacks.iter().copied().enumerate() {
        for port_index in 0..IIO_PCIE_PORT_COUNT {
            let index = stack_index * IIO_PCIE_PORT_COUNT + port_index;
            let read_delta = wrapping_delta(
                previous.1[index],
                current.1[index],
                IIO_FREE_RUNNING_COUNTER_WIDTH,
            );
            let write_delta = wrapping_delta(
                previous.1[write_offset + index],
                current.1[write_offset + index],
                IIO_FREE_RUNNING_COUNTER_WIDTH,
            );
            ports.push(SprIioPciePortMetrics {
                port_id: port_index as u32,
                read_bytes_per_second: read_delta as f64 * PCIE_COUNTER_BYTES / elapsed_seconds,
                scope,
                stack,
                write_bytes_per_second: write_delta as f64 * PCIE_COUNTER_BYTES / elapsed_seconds,
            });
        }
    }

    Ok(ports)
}

fn program_packages(packages: &[IioPackage], group: IioEventGroup) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze_and_reset()?;
        }
    }

    for package in packages {
        for unit in &package.units {
            unit.program(group)?;
        }
    }

    Ok(())
}

fn read_packages(
    packages: &[IioPackage],
    measurement: IioMeasurement,
    measurements: &mut IioMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            let reading = unit.read()?;

            for counter_index in 0..IIO_COUNTER_COUNT {
                let event = measurement.group.events[counter_index];

                measurements.add(
                    package.scope,
                    unit.stack(),
                    event.kind,
                    reading.counters[counter_index],
                    reading.ticks,
                    measurement,
                );
            }
        }
    }

    Ok(())
}

fn read_pcie_counter(cpu: u32, address: u64) -> Result<u64, String> {
    Msr::open_readonly(cpu)?
        .read(address)
        .map(|counter| mask_counter(counter, IIO_FREE_RUNNING_COUNTER_WIDTH))
}

fn read_pcie_ports(packages: &mut [IioPackage]) -> Result<Vec<SprIioPciePortMetrics>, String> {
    let mut ports = Vec::new();

    for package in packages {
        let stacks = package
            .units
            .iter()
            .map(|unit| unit.stack())
            .collect::<Vec<_>>();
        ports.extend(package.free_running.sample(package.scope, &stacks)?);
    }

    Ok(ports)
}

fn probe_writable_msrs(packages: &[IioPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<IioEventKind, IioEventMeasurement>,
    kind: IioEventKind,
) -> Result<&IioEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IIO measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[IioPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_spr_iio_scope_metrics() {
        let scope = test_scope();
        let metrics = SprIioMetrics::from_measurements(
            BTreeMap::from([(
                test_stack_scope(scope, SPR_IIO_STACKS[0]),
                BTreeMap::from([
                    measurement(IioEventKind::Clockticks, 1_000, 1_000, 100),
                    measurement(IioEventKind::CompletionInserts, 200, 1_000, 100),
                    measurement(IioEventKind::CompletionOccupancy, 400, 1_000, 100),
                    measurement(IioEventKind::InboundReadDwords, 300, 1_000, 100),
                    measurement(IioEventKind::InboundReadTransactions, 30, 1_000, 100),
                    measurement(IioEventKind::InboundWriteDwords, 500, 1_000, 100),
                    measurement(IioEventKind::InboundWriteTransactions, 50, 1_000, 100),
                ]),
            )]),
            Vec::new(),
        )
        .unwrap();

        let scope_metrics = metrics.scopes[0];
        assert_eq!(scope_metrics.completion_inserts_per_second, 2_000.0);
        assert_eq!(scope_metrics.completion_latency_seconds, 0.0002);
        assert_eq!(scope_metrics.completion_occupancy_entries, 0.4);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(scope_metrics.inbound_read_bytes_per_second, 12_000.0);
        assert_eq!(scope_metrics.inbound_reads_per_second, 300.0);
        assert_eq!(scope_metrics.inbound_write_bytes_per_second, 20_000.0);
        assert_eq!(scope_metrics.inbound_writes_per_second, 500.0);
        assert_eq!(scope_metrics.stack, SPR_IIO_STACKS[0]);
    }

    #[test]
    fn computes_spr_pcie_port_metrics() {
        let scope = test_scope();
        let previous = (
            Instant::now(),
            vec![0; SPR_IIO_STACKS.len() * IIO_PCIE_PORT_COUNT * 2],
        );
        let mut current_values = vec![0; SPR_IIO_STACKS.len() * IIO_PCIE_PORT_COUNT * 2];
        current_values[0] = 100;
        current_values[SPR_IIO_STACKS.len() * IIO_PCIE_PORT_COUNT] = 200;
        let current = (previous.0 + Duration::from_millis(100), current_values);
        let ports = pcie_metrics_from_readings(scope, &SPR_IIO_STACKS, previous, current).unwrap();

        assert_eq!(ports[0].read_bytes_per_second, 32_000.0);
        assert_eq!(ports[0].stack, SPR_IIO_STACKS[0]);
        assert_eq!(ports[0].write_bytes_per_second, 64_000.0);
    }

    #[test]
    fn reads_spr_pcie_ports_for_selected_stacks() {
        let scope = test_scope();
        let stacks = [SPR_IIO_STACKS[10]];
        let previous = (
            Instant::now(),
            vec![0; stacks.len() * IIO_PCIE_PORT_COUNT * 2],
        );
        let mut current_values = vec![0; stacks.len() * IIO_PCIE_PORT_COUNT * 2];
        current_values[0] = 100;
        current_values[stacks.len() * IIO_PCIE_PORT_COUNT] = 200;
        let current = (previous.0 + Duration::from_millis(100), current_values);
        let ports = pcie_metrics_from_readings(scope, &stacks, previous, current).unwrap();

        assert_eq!(ports.len(), IIO_PCIE_PORT_COUNT);
        assert_eq!(ports[0].read_bytes_per_second, 32_000.0);
        assert_eq!(ports[0].stack, SPR_IIO_STACKS[10]);
        assert_eq!(ports[0].write_bytes_per_second, 64_000.0);
    }

    #[test]
    fn maps_discovered_spr_iio_box_ids_to_stacks() {
        assert_eq!(SprIioStack::from_box_id(0), Some(SPR_IIO_STACKS[0]));
        assert_eq!(SprIioStack::from_box_id(10), Some(SPR_IIO_STACKS[10]));
        assert_eq!(SprIioStack::from_box_id(12), None);
    }

    #[test]
    fn uses_spr_iio_address_map() {
        assert_eq!(iio_unit_control_offset(SPR_IIO_STACKS[0]), 0x3000);
        assert_eq!(iio_counter_offset(SPR_IIO_STACKS[0], 0), 0x3008);
        assert_eq!(iio_clock_counter_offset(SPR_IIO_STACKS[0]), 0x340e);
        assert_eq!(iio_control_offset(SPR_IIO_STACKS[0], 0), 0x3002);
        assert_eq!(iio_unit_control_offset(SPR_IIO_STACKS[11]), 0x30b0);
        assert_eq!(iio_pcie_read_counter_offset(SPR_IIO_STACKS[9], 7), 0x3897);
        assert_eq!(iio_pcie_write_counter_offset(SPR_IIO_STACKS[9], 7), 0x389f);
    }

    #[test]
    fn schedules_perfmon_experimental_spr_completion_occupancy_event() {
        assert_eq!(
            SPR_IIO_EVENT_GROUPS[0].events[2],
            IioEventSpec::sum(IioEventKind::CompletionOccupancy, 0xd5, 0xff, 0x00ff, 0x07)
        );
        assert_eq!(
            SPR_IIO_EVENT_GROUPS[1].events[2],
            IioEventSpec::sum(IioEventKind::CompletionInserts, 0xc2, 0x04, 0x00ff, 0x07)
        );
    }

    #[test]
    fn encodes_spr_unit_control() {
        assert_eq!(UNIT_FREEZE, 0x001);
        assert_eq!(UNIT_FREEZE_AND_CONTROL_RESET, 0x101);
        assert_eq!(UNIT_FREEZE_AND_COUNTER_RESET, 0x201);
        assert_eq!(UNIT_UNFREEZE, 0x000);
    }

    #[test]
    fn encodes_spr_iio_counter_control() {
        assert_eq!(
            iio_counter_control(0x83, 0x01, 0x00ff, 0x07, true),
            0x83 | (0x01 << 8) | (0x00ff_u64 << 36) | (0x07_u64 << 48)
        );
    }

    fn measurement(
        kind: IioEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (IioEventKind, IioEventMeasurement) {
        (
            kind,
            IioEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                ticks,
                value,
            },
        )
    }

    fn test_scope() -> UncoreScope {
        UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }

    fn test_stack_scope(scope: UncoreScope, stack: SprIioStack) -> IioStackScope {
        IioStackScope { scope, stack }
    }
}
