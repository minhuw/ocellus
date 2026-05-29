use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::msr::Msr;
use crate::metrics::common::topology_label;
use crate::metrics::uncore::skx::{
    SKX_UNCORE_COUNTER_WIDTH, UncoreScope, events_per_second, frequency_hz, mask_counter,
    measurement_round_count, queue_residency_seconds, ratio, scale_to_enabled, uncore_leaders,
    wrapping_delta,
};

const COUNTER_ENABLE_BIT: u64 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u64 = 1 << 20;
const COUNTER_RESET_BIT: u64 = 1 << 17;
const IIO_CHANNEL_MASK_SHIFT: u32 = 36;
const IIO_FUNCTION_CLASS_MASK_SHIFT: u32 = 48;
const IIO_COUNTER_COUNT: usize = 4;
const IIO_PCIE_PORT_COUNT: usize = 8;
const IIO_UNIT_COUNT: usize = 6;
const IIO_FREE_RUNNING_COUNTER_WIDTH: u32 = 48;
const PCIE_COUNTER_BYTES: f64 = 32.0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_RESERVED_BITS: u32 = 0b11 << 16;

const UNIT_FREEZE: u32 = UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
const UNIT_FREEZE_AND_RESET: u32 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
const UNIT_UNFREEZE: u32 = UNIT_RESERVED_BITS;

const ICX_IIO_STACKS: [IcxIioStack; IIO_UNIT_COUNT] = [
    IcxIioStack::new(0, "pcie0", 0x0a50, 0x0a51, 0x0a58, 0x0aa0),
    IcxIioStack::new(1, "pcie1", 0x0a70, 0x0a71, 0x0a78, 0x0ab0),
    IcxIioStack::new(2, "mcp", 0x0a90, 0x0a91, 0x0a98, 0x0ac0),
    IcxIioStack::new(3, "pcie2", 0x0ae0, 0x0ae1, 0x0ae8, 0x0b30),
    IcxIioStack::new(4, "pcie3", 0x0b00, 0x0b01, 0x0b08, 0x0b40),
    IcxIioStack::new(5, "cbdma_dmi", 0x0b20, 0x0b21, 0x0b28, 0x0b50),
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
    Unused,
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
    const fn unused() -> Self {
        Self {
            channel_mask: 0,
            event: 0,
            function_class_mask: 0,
            kind: IioEventKind::Unused,
            umask: 0,
        }
    }

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

const ICX_IIO_EVENT_GROUPS: [IioEventGroup; 3] = [
    IioEventGroup {
        events: [
            IioEventSpec::sum(IioEventKind::InboundWriteDwords, 0x83, 0x01, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::InboundReadDwords, 0x83, 0x04, 0x00ff, 0x07),
            IioEventSpec::sum(IioEventKind::CompletionOccupancy, 0xd5, 0xff, 0x0000, 0x04),
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
            IioEventSpec::sum(IioEventKind::CompletionInserts, 0xc2, 0x03, 0x00ff, 0x04),
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00),
        ],
    },
    IioEventGroup {
        events: [
            IioEventSpec::unused(),
            IioEventSpec::unused(),
            IioEventSpec::unused(),
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IcxIioStack {
    id: usize,
    label: &'static str,
    unit_control: u64,
    counter_base: u64,
    control_base: u64,
    pcie_read_base: u64,
}

impl IcxIioStack {
    const fn new(
        id: usize,
        label: &'static str,
        unit_control: u64,
        counter_base: u64,
        control_base: u64,
        pcie_read_base: u64,
    ) -> Self {
        Self {
            id,
            label,
            unit_control,
            counter_base,
            control_base,
            pcie_read_base,
        }
    }

    pub const fn id(self) -> usize {
        self.id
    }

    pub const fn label(self) -> &'static str {
        self.label
    }
}

impl serde::Serialize for IcxIioStack {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IcxIioPciePortMetrics {
    pub port_id: u32,
    pub read_bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub stack: IcxIioStack,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IcxIioScopeMetrics {
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub completion_inserts_per_second: f64,
    pub completion_latency_seconds: f64,
    pub completion_occupancy_entries: f64,
    pub frequency_hz: f64,
    pub stack: IcxIioStack,
    pub inbound_read_bytes_per_second: f64,
    pub inbound_reads_per_second: f64,
    pub inbound_write_bytes_per_second: f64,
    pub inbound_writes_per_second: f64,
}

impl IcxIioScopeMetrics {
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
pub struct IcxIioMetrics {
    pub ports: Vec<IcxIioPciePortMetrics>,
    pub scopes: Vec<IcxIioScopeMetrics>,
}

impl IcxIioMetrics {
    fn from_measurements(
        measurements: BTreeMap<IioStackScope, BTreeMap<IioEventKind, IioEventMeasurement>>,
        ports: Vec<IcxIioPciePortMetrics>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(IcxIioScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { ports, scopes })
    }
}

#[derive(Debug)]
pub struct IcxIioCollector {
    next_group: usize,
    packages: Vec<IioPackage>,
}

impl IcxIioCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
        })
    }

    #[cfg(test)]
    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(IntelServerCpuModel::IceLakeXeon)
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxIioMetrics, String> {
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

        IcxIioMetrics::from_measurements(measurements.into_measurements(), ports)
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % ICX_IIO_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IioMeasurementSlice> {
        let group_count = ICX_IIO_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IioMeasurementSlice {
                    duration: slice_duration,
                    group: ICX_IIO_EVENT_GROUPS[rotated_index],
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
    fn from_port(port: IcxIioPciePortMetrics) -> Self {
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
    fn from_scope(scope: UncoreScope, stack: IcxIioStack) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct IcxIioPrometheusMetrics {
    completion_inserts_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_latency_seconds: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_occupancy_entries: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_read_bytes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_reads_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_write_bytes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    inbound_writes_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_bytes_per_second: Family<IioPciePortLabels, Gauge<f64, AtomicU64>>,
}

impl IcxIioPrometheusMetrics {
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
        metrics
    }

    pub fn update(&self, metrics: IcxIioMetrics) {
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
    fn new(cpu: u32, scope: UncoreScope) -> Self {
        let units = ICX_IIO_STACKS
            .into_iter()
            .map(|stack| IioUnit { cpu, stack })
            .collect();

        Self {
            free_running: IioFreeRunningCounters::new(cpu),
            scope,
            units,
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

    fn sample(&mut self, scope: UncoreScope) -> Result<Vec<IcxIioPciePortMetrics>, String> {
        let current = (Instant::now(), self.read()?);
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(Vec::new()),
        };

        pcie_metrics_from_readings(scope, previous, current)
    }

    fn read(&self) -> Result<Vec<u64>, String> {
        let mut values = Vec::with_capacity(IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT);

        for stack in ICX_IIO_STACKS {
            for port_index in 0..IIO_PCIE_PORT_COUNT {
                values.push(read_pcie_counter(
                    self.cpu,
                    iio_pcie_read_counter_offset(stack, port_index),
                )?);
            }
        }

        Ok(values)
    }
}

#[derive(Clone, Copy, Debug)]
struct IioUnit {
    cpu: u32,
    stack: IcxIioStack,
}

impl IioUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))
    }

    fn program(self, group: IioEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            let control = if event.kind == IioEventKind::Unused {
                0
            } else {
                iio_counter_control(
                    event.event,
                    event.umask,
                    event.channel_mask,
                    event.function_class_mask,
                    true,
                )
            };

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
        })
    }

    fn stack(self) -> IcxIioStack {
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
    stack: IcxIioStack,
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
        stack: IcxIioStack,
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
    overflow_enabled: bool,
) -> u64 {
    let overflow = if overflow_enabled {
        COUNTER_OVERFLOW_ENABLE_BIT
    } else {
        0
    };

    u64::from(event)
        | (u64::from(umask) << 8)
        | COUNTER_RESET_BIT
        | overflow
        | COUNTER_ENABLE_BIT
        | (u64::from(channel_mask) << IIO_CHANNEL_MASK_SHIFT)
        | (u64::from(function_class_mask) << IIO_FUNCTION_CLASS_MASK_SHIFT)
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<IioPackage>, String> {
    if !matches!(model, IntelServerCpuModel::IceLakeXeon) {
        return Err(format!(
            "Ice Lake IIO collection is not supported for {model:?}"
        ));
    }

    let leaders = uncore_leaders()?;
    let packages = leaders
        .into_iter()
        .map(|leader| IioPackage::new(leader.cpu, leader.scope))
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any IIO packages".to_string());
    }

    Ok(packages)
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

fn iio_control_offset(stack: IcxIioStack, counter_index: usize) -> u64 {
    stack.control_base + counter_index as u64
}

fn iio_counter_offset(stack: IcxIioStack, counter_index: usize) -> u64 {
    stack.counter_base + counter_index as u64
}

fn iio_pcie_read_counter_offset(stack: IcxIioStack, port_index: usize) -> u64 {
    stack.pcie_read_base + port_index as u64
}

fn iio_unit_control_offset(stack: IcxIioStack) -> u64 {
    stack.unit_control
}

fn mask_iio_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn pcie_metrics_from_readings(
    scope: UncoreScope,
    previous: (Instant, Vec<u64>),
    current: (Instant, Vec<u64>),
) -> Result<Vec<IcxIioPciePortMetrics>, String> {
    if previous.1.len() != IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT
        || current.1.len() != IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT
    {
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

    let mut ports = Vec::with_capacity(IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT);

    for stack in ICX_IIO_STACKS {
        let stack_index = stack.id();
        for port_index in 0..IIO_PCIE_PORT_COUNT {
            let index = stack_index * IIO_PCIE_PORT_COUNT + port_index;
            let read_delta = wrapping_delta(
                previous.1[index],
                current.1[index],
                IIO_FREE_RUNNING_COUNTER_WIDTH,
            );

            ports.push(IcxIioPciePortMetrics {
                port_id: port_index as u32,
                read_bytes_per_second: read_delta as f64 * PCIE_COUNTER_BYTES / elapsed_seconds,
                scope,
                stack,
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
            let group_ticks = group_ticks(measurement.group, reading)?;

            for counter_index in 0..IIO_COUNTER_COUNT {
                let event = measurement.group.events[counter_index];

                if event.kind != IioEventKind::Unused {
                    measurements.add(
                        package.scope,
                        unit.stack(),
                        event.kind,
                        reading.counters[counter_index],
                        group_ticks,
                        measurement,
                    );
                }
            }
        }
    }

    Ok(())
}

fn group_ticks(group: IioEventGroup, reading: IioUnitReading) -> Result<u64, String> {
    group
        .events
        .into_iter()
        .enumerate()
        .find_map(|(counter_index, event)| {
            (event.kind == IioEventKind::Clockticks).then_some(reading.counters[counter_index])
        })
        .ok_or_else(|| "IIO event group is missing interval clockticks".to_string())
}

fn read_pcie_counter(cpu: u32, address: u64) -> Result<u64, String> {
    Msr::open_readonly(cpu)?
        .read(address)
        .map(|counter| mask_counter(counter, IIO_FREE_RUNNING_COUNTER_WIDTH))
}

fn read_pcie_ports(packages: &mut [IioPackage]) -> Result<Vec<IcxIioPciePortMetrics>, String> {
    let mut ports = Vec::new();

    for package in packages {
        ports.extend(package.free_running.sample(package.scope)?);
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
    fn computes_icx_iio_scope_metrics() {
        let scope = test_scope();
        let metrics = IcxIioMetrics::from_measurements(
            BTreeMap::from([(
                test_stack_scope(scope, ICX_IIO_STACKS[0]),
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
        assert_eq!(scope_metrics.stack, ICX_IIO_STACKS[0]);
    }

    #[test]
    fn computes_icx_completion_occupancy_from_matching_ticks() {
        let scope = test_scope();
        let metrics = IcxIioMetrics::from_measurements(
            BTreeMap::from([(
                test_stack_scope(scope, ICX_IIO_STACKS[0]),
                BTreeMap::from([
                    measurement(IioEventKind::Clockticks, 3_000, 3_000, 300),
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

        assert_eq!(metrics.scopes[0].completion_occupancy_entries, 0.4);
    }

    #[test]
    fn computes_icx_pcie_port_metrics() {
        let scope = test_scope();
        let previous = (
            Instant::now(),
            vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT],
        );
        let mut current_values = vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT];
        current_values[0] = 100;
        let current = (previous.0 + Duration::from_millis(100), current_values);
        let ports = pcie_metrics_from_readings(scope, previous, current).unwrap();

        assert_eq!(ports[0].read_bytes_per_second, 32_000.0);
        assert_eq!(ports[0].stack, ICX_IIO_STACKS[0]);
    }

    #[test]
    fn uses_documented_icx_iio_address_map() {
        assert_eq!(iio_unit_control_offset(ICX_IIO_STACKS[0]), 0x0a50);
        assert_eq!(iio_counter_offset(ICX_IIO_STACKS[0], 0), 0x0a51);
        assert_eq!(iio_control_offset(ICX_IIO_STACKS[0], 0), 0x0a58);
        assert_eq!(iio_unit_control_offset(ICX_IIO_STACKS[5]), 0x0b20);
        assert_eq!(iio_pcie_read_counter_offset(ICX_IIO_STACKS[5], 7), 0x0b57);
        assert_eq!(
            ICX_IIO_EVENT_GROUPS[0].events[3],
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00)
        );
        assert_eq!(
            ICX_IIO_EVENT_GROUPS[1].events[3],
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00)
        );
        assert_eq!(
            ICX_IIO_EVENT_GROUPS[2].events[3],
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x0000, 0x00)
        );
    }

    #[test]
    fn encodes_icx_iio_counter_control() {
        assert_eq!(
            iio_counter_control(0x83, 0x01, 0x00ff, 0x07, true),
            0x83 | (0x01 << 8)
                | COUNTER_RESET_BIT
                | COUNTER_OVERFLOW_ENABLE_BIT
                | COUNTER_ENABLE_BIT
                | (0x00ff_u64 << 36)
                | (0x07_u64 << 48)
        );
    }

    #[test]
    fn encodes_icx_unit_control() {
        assert_eq!(UNIT_FREEZE, 0x30100);
        assert_eq!(UNIT_FREEZE_AND_RESET, 0x30103);
        assert_eq!(UNIT_UNFREEZE, 0x30000);
    }

    #[test]
    fn uses_programmed_clockticks_counter_for_group_ticks() {
        let reading = IioUnitReading {
            counters: [11, 22, 33, 44],
        };

        assert_eq!(group_ticks(ICX_IIO_EVENT_GROUPS[0], reading).unwrap(), 44);
    }

    #[test]
    fn supports_only_ice_lake_xeon_iio() {
        assert!(IcxIioCollector::is_supported(&test_architecture(0x6a)));
        assert!(!IcxIioCollector::is_supported(&test_architecture(0x55)));
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

    fn test_architecture(model: u8) -> Architecture {
        Architecture {
            brand: "test".to_string(),
            family: 6,
            features: crate::arch::ArchitectureFeatures::default(),
            model,
            vendor: "GenuineIntel".to_string(),
        }
    }

    fn test_scope() -> UncoreScope {
        UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }

    fn test_stack_scope(scope: UncoreScope, stack: IcxIioStack) -> IioStackScope {
        IioStackScope { scope, stack }
    }
}
