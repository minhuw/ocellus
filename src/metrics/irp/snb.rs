use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::pci::{PciBus, PciDevice};
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::DEFAULT_MAX_SLICE;

const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const IRP_COUNTER_COUNT: usize = 4;
const IRP_COUNTER_WIDTH: u32 = 48;
const IRP_CONTROL_OFFSETS: [u64; IRP_COUNTER_COUNT] = [0xd8, 0xdc, 0xe0, 0xe4];
const IRP_COUNTER_OFFSETS: [u64; IRP_COUNTER_COUNT] = [0xa0, 0xb0, 0xb8, 0xc0];
const IRP_UNIT_CONTROL_OFFSET: u64 = 0xf4;
const UBOX_GID_OFFSET: u64 = 0x54;
const UBOX_LOCAL_NODE_ID_OFFSET: u64 = 0x40;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u32 = 1 << 16;

const SNB_IRP_EVENT_GROUPS: [SnbIrpEventGroup; 2] = [
    SnbIrpEventGroup {
        events: [
            SnbIrpEventSpec::sum(SnbIrpEventKind::PcieInboundReads, 0x15, 0x01),
            SnbIrpEventSpec::sum(SnbIrpEventKind::PcieInboundWrites, 0x15, 0x02),
            SnbIrpEventSpec::sum(SnbIrpEventKind::LostOwnership, 0x16, 0x01),
            SnbIrpEventSpec::sum(SnbIrpEventKind::Clockticks, 0x00, 0x00),
        ],
    },
    SnbIrpEventGroup {
        events: [
            SnbIrpEventSpec::sum(SnbIrpEventKind::TotalIrpOccupancy, 0x12, 0x01),
            SnbIrpEventSpec::disabled(),
            SnbIrpEventSpec::disabled(),
            SnbIrpEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbIrpArchitecture {
    Ivb,
    Snb,
}

impl SnbIrpArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::IvyTown => Some(Self::Ivb),
            IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
            _ => None,
        }
    }

    const fn irp_device_id(self) -> u16 {
        match self {
            Self::Ivb => 0x0e39,
            Self::Snb => 0x3c40,
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
pub struct SnbIrpScope {
    pub package_id: u32,
}

impl SnbIrpScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SnbIrpScopeMetrics {
    pub frequency_hz: f64,
    pub io_write_conflict_ratio: f64,
    pub pcie_inbound_reads_per_second: f64,
    pub pcie_inbound_writes_per_second: f64,
    #[serde(flatten)]
    pub scope: SnbIrpScope,
    pub total_irp_occupancy_entries: f64,
}

impl SnbIrpScopeMetrics {
    fn from_measurements(
        scope: SnbIrpScope,
        measurements: &BTreeMap<SnbIrpEventKind, SnbIrpEventMeasurement>,
    ) -> Result<Self, String> {
        let clockticks = required_measurement(measurements, SnbIrpEventKind::Clockticks)?;
        let lost_ownership = required_measurement(measurements, SnbIrpEventKind::LostOwnership)?;
        let pcie_inbound_reads =
            required_measurement(measurements, SnbIrpEventKind::PcieInboundReads)?;
        let pcie_inbound_writes =
            required_measurement(measurements, SnbIrpEventKind::PcieInboundWrites)?;
        let total_irp_occupancy =
            required_measurement(measurements, SnbIrpEventKind::TotalIrpOccupancy)?;
        let lost_ownership_count = scale_to_enabled(
            lost_ownership.value,
            lost_ownership.enabled,
            lost_ownership.running,
        );
        let write_insert_count = scale_to_enabled(
            pcie_inbound_writes.value,
            pcie_inbound_writes.enabled,
            pcie_inbound_writes.running,
        );

        Ok(Self {
            frequency_hz: frequency_hz(clockticks),
            io_write_conflict_ratio: ratio(lost_ownership_count, write_insert_count),
            pcie_inbound_reads_per_second: event_rate(pcie_inbound_reads),
            pcie_inbound_writes_per_second: event_rate(pcie_inbound_writes),
            scope,
            total_irp_occupancy_entries: occupancy_entries(total_irp_occupancy, clockticks),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnbIrpMetrics {
    pub scopes: Vec<SnbIrpScopeMetrics>,
}

impl SnbIrpMetrics {
    fn from_measurements(
        measurements: BTreeMap<SnbIrpScope, BTreeMap<SnbIrpEventKind, SnbIrpEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(SnbIrpScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SnbIrpCollector {
    architecture: SnbIrpArchitecture,
    next_group: usize,
    packages: Vec<SnbIrpPackage>,
}

impl SnbIrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = SnbIrpArchitecture::from_model(model).ok_or_else(|| {
            format!("Sandy/Ivy Bridge-EP IRP collection is not supported for {model:?}")
        })?;
        let packages = discover_packages(architecture)?;
        probe_writable_pci(&packages)?;

        Ok(Self {
            architecture,
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SnbIrpMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IRP measure interval must be non-zero",
                self.architecture.name()
            ));
        }

        let mut measurements = SnbIrpMeasurementAccumulator::new();
        let packages = &self.packages;

        for slice in self.schedule(interval) {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                SnbIrpMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        SnbIrpMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % SNB_IRP_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<SnbIrpMeasurementSlice> {
        let group_count = SNB_IRP_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(SnbIrpMeasurementSlice {
                    duration: slice_duration,
                    group: SNB_IRP_EVENT_GROUPS[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct SnbIrpPrometheusMetrics {
    frequency_hz: Family<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>,
    io_write_conflict_ratio: Family<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_reads_per_second: Family<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_writes_per_second: Family<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>,
    total_irp_occupancy_entries: Family<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SnbIrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            frequency_hz: Family::<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            io_write_conflict_ratio: Family::<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_reads_per_second:
                Family::<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_writes_per_second:
                Family::<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            total_irp_occupancy_entries:
                Family::<SnbIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_irp_frequency_hz",
            "Interval-derived IRP clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_irp_io_write_conflict_ratio",
            "Interval-derived IRP I/O write conflict ratio from lost ownership over PCIe inbound writes",
            metrics.io_write_conflict_ratio.clone(),
        );
        registry.register(
            "ocellus_irp_pcie_inbound_reads_per_second",
            "Interval-derived IRP PCIe inbound reads per second",
            metrics.pcie_inbound_reads_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_pcie_inbound_writes_per_second",
            "Interval-derived IRP PCIe inbound writes per second",
            metrics.pcie_inbound_writes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_total_occupancy_entries",
            "Average total IRP read and write occupancy in entries",
            metrics.total_irp_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SnbIrpMetrics) {
        for scope in metrics.scopes {
            let labels = SnbIrpScopeLabels::from_scope(scope.scope);

            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.io_write_conflict_ratio
                .get_or_create(&labels)
                .set(scope.io_write_conflict_ratio);
            self.pcie_inbound_reads_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_reads_per_second);
            self.pcie_inbound_writes_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_writes_per_second);
            self.total_irp_occupancy_entries
                .get_or_create(&labels)
                .set(scope.total_irp_occupancy_entries);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SnbIrpScopeLabels {
    package: String,
}

impl SnbIrpScopeLabels {
    fn from_scope(scope: SnbIrpScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct SnbIrpPackage {
    scope: SnbIrpScope,
    unit: SnbIrpUnit,
}

#[derive(Debug)]
struct SnbIrpUnit {
    architecture: SnbIrpArchitecture,
    device: PciDevice,
}

impl SnbIrpUnit {
    fn new(
        architecture: SnbIrpArchitecture,
        location: metal::pci::PciLocation,
    ) -> Result<Self, String> {
        Ok(Self {
            architecture,
            device: PciDevice::open(location)?,
        })
    }

    fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IRP_UNIT_CONTROL_OFFSET, self.architecture.unit_freeze())
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.device.write_u32(
            IRP_UNIT_CONTROL_OFFSET,
            self.architecture.unit_freeze_and_reset(),
        )
    }

    fn program(&self, group: SnbIrpEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            let value = match event.kind {
                SnbIrpEventKind::Disabled => 0,
                _ => counter_control(event.event, event.umask),
            };

            self.device
                .write_u32(IRP_CONTROL_OFFSETS[counter_index], value)?;
        }

        Ok(())
    }

    fn read(&self) -> Result<SnbIrpUnitReading, String> {
        let mut counters = [0; IRP_COUNTER_COUNT];

        for counter_index in 0..IRP_COUNTER_COUNT {
            counters[counter_index] =
                mask_irp_counter(self.device.read_u64(IRP_COUNTER_OFFSETS[counter_index])?);
        }

        Ok(SnbIrpUnitReading { counters })
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IRP_UNIT_CONTROL_OFFSET, self.architecture.unit_unfreeze())
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbIrpUnitReading {
    counters: [u64; IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbIrpEventGroup {
    events: [SnbIrpEventSpec; IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbIrpEventSpec {
    event: u8,
    kind: SnbIrpEventKind,
    umask: u8,
}

impl SnbIrpEventSpec {
    const fn disabled() -> Self {
        Self {
            event: 0,
            kind: SnbIrpEventKind::Disabled,
            umask: 0,
        }
    }

    const fn sum(kind: SnbIrpEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbIrpEventKind {
    Clockticks,
    Disabled,
    LostOwnership,
    PcieInboundReads,
    PcieInboundWrites,
    TotalIrpOccupancy,
}

#[derive(Clone, Copy, Debug)]
struct SnbIrpEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl SnbIrpEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbIrpMeasurement {
    enabled: Duration,
    group: SnbIrpEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbIrpMeasurementSlice {
    duration: Duration,
    group: SnbIrpEventGroup,
}

#[derive(Debug, Default)]
struct SnbIrpMeasurementAccumulator {
    measurements: BTreeMap<SnbIrpScope, BTreeMap<SnbIrpEventKind, SnbIrpEventMeasurement>>,
}

impl SnbIrpMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: SnbIrpScope,
        kind: SnbIrpEventKind,
        value: u64,
        measurement: SnbIrpMeasurement,
    ) {
        if kind == SnbIrpEventKind::Disabled {
            return;
        }

        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(SnbIrpEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<SnbIrpScope, BTreeMap<SnbIrpEventKind, SnbIrpEventMeasurement>> {
        self.measurements
    }
}

fn counter_control(event: u8, umask: u8) -> u32 {
    u32::from(event)
        | (u32::from(umask) << 8)
        | COUNTER_RESET_BIT
        | COUNTER_OVERFLOW_ENABLE_BIT
        | COUNTER_ENABLE_BIT
}

fn discover_packages(architecture: SnbIrpArchitecture) -> Result<Vec<SnbIrpPackage>, String> {
    let scopes = irp_scopes()?;
    let bus_scopes = irp_bus_scopes(architecture, &scopes).unwrap_or_default();
    let locations =
        metal::pci::find_intel_devices_matching_device_id(architecture.irp_device_id())?;
    let mut packages = Vec::with_capacity(locations.len());

    for (location_index, location) in locations.iter().copied().enumerate() {
        let scope = bus_scopes
            .iter()
            .find(|bus_scope| bus_scope.matches(location))
            .or_else(|| {
                bus_scopes
                    .iter()
                    .filter(|bus_scope| bus_scope.bus.group == location.group)
                    .filter(|bus_scope| bus_scope.bus.bus >= location.bus)
                    .min_by_key(|bus_scope| bus_scope.bus.bus)
            })
            .map(|bus_scope| bus_scope.scope)
            .or_else(|| scopes.get(location_index).copied())
            .ok_or_else(|| format!("failed to map {location} to a CPU package"))?;

        packages.push(SnbIrpPackage {
            scope,
            unit: SnbIrpUnit::new(architecture, location)?,
        });
    }

    if packages.is_empty() {
        return Err(format!(
            "failed to discover any {} IRP devices with device id 0x{:x}",
            architecture.name(),
            architecture.irp_device_id()
        ));
    }

    Ok(packages)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbIrpBusScope {
    bus: PciBus,
    scope: SnbIrpScope,
}

impl SnbIrpBusScope {
    fn matches(self, location: metal::pci::PciLocation) -> bool {
        self.bus.group == location.group && self.bus.bus == location.bus
    }
}

fn irp_bus_scopes(
    architecture: SnbIrpArchitecture,
    scopes: &[SnbIrpScope],
) -> Result<Vec<SnbIrpBusScope>, String> {
    let mut bus_scopes = Vec::new();

    for (package_index, bus) in package_buses_from_uboxes(architecture)? {
        let scope = scopes.get(package_index).copied().ok_or_else(|| {
            format!(
                "failed to map {} UBox package index {package_index} to a CPUID package",
                architecture.name()
            )
        })?;

        bus_scopes.push(SnbIrpBusScope { bus, scope });
    }

    Ok(bus_scopes)
}

fn package_buses_from_uboxes(
    architecture: SnbIrpArchitecture,
) -> Result<Vec<(usize, PciBus)>, String> {
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
    Ok(buses)
}

fn package_index_from_node_mapping(local_node_id: u32, node_mapping: u32) -> Option<u32> {
    (0..8).find(|package_index| ((node_mapping >> (package_index * 3)) & 0x7) == local_node_id)
}

fn event_rate(measurement: &SnbIrpEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn frequency_hz(measurement: &SnbIrpEventMeasurement) -> f64 {
    events_per_second(measurement.value, measurement.running)
}

fn freeze_packages(packages: &[SnbIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }

    Ok(())
}

fn irp_scopes() -> Result<Vec<SnbIrpScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(SnbIrpScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any Sandy/Ivy Bridge-EP IRP scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
}

fn mask_irp_counter(counter: u64) -> u64 {
    counter & ((1_u64 << IRP_COUNTER_WIDTH) - 1)
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let minimum_round_count = 1;
    let round_count = interval.as_nanos().div_ceil(DEFAULT_MAX_SLICE.as_nanos());

    usize::try_from(round_count)
        .unwrap_or(usize::MAX)
        .max(minimum_round_count)
        .div_ceil(group_count)
}

fn occupancy_entries(
    occupancy: &SnbIrpEventMeasurement,
    clockticks: &SnbIrpEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    ratio(occupancy, clockticks)
}

fn probe_writable_pci(packages: &[SnbIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }

    Ok(())
}

fn program_packages(packages: &[SnbIrpPackage], group: SnbIrpEventGroup) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
    }

    for package in packages {
        package.unit.program(group)?;
    }

    Ok(())
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn read_packages(
    packages: &[SnbIrpPackage],
    measurement: SnbIrpMeasurement,
    measurements: &mut SnbIrpMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let reading = package.unit.read()?;

        for counter_index in 0..IRP_COUNTER_COUNT {
            let event = measurement.group.events[counter_index];
            measurements.add(
                package.scope,
                event.kind,
                reading.counters[counter_index],
                measurement,
            );
        }
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<SnbIrpEventKind, SnbIrpEventMeasurement>,
    kind: SnbIrpEventKind,
) -> Result<&SnbIrpEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("Sandy/Ivy Bridge-EP IRP measurement {kind:?} is missing"))
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

fn unfreeze_packages(packages: &[SnbIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_snb_irp_metrics() {
        let scope = SnbIrpScope { package_id: 0 };
        let metrics = SnbIrpMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(SnbIrpEventKind::Clockticks, 1_000, 100),
                measurement(SnbIrpEventKind::LostOwnership, 150, 100),
                measurement(SnbIrpEventKind::PcieInboundReads, 250, 100),
                measurement(SnbIrpEventKind::PcieInboundWrites, 600, 100),
                measurement(SnbIrpEventKind::TotalIrpOccupancy, 500, 100),
            ]),
        )]))
        .unwrap();

        let scope_metrics = metrics.scopes[0];
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(scope_metrics.io_write_conflict_ratio, 0.25);
        assert_eq!(scope_metrics.pcie_inbound_reads_per_second, 2_500.0);
        assert_eq!(scope_metrics.pcie_inbound_writes_per_second, 6_000.0);
        assert_eq!(scope_metrics.total_irp_occupancy_entries, 0.5);
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector(SnbIrpArchitecture::Snb);

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_secs(1))),
            SNB_IRP_EVENT_GROUPS
                .into_iter()
                .chain(SNB_IRP_EVENT_GROUPS)
                .chain(SNB_IRP_EVENT_GROUPS)
                .chain(SNB_IRP_EVENT_GROUPS)
                .chain(SNB_IRP_EVENT_GROUPS)
                .collect::<Vec<_>>()
        );

        collector.rotate_group();
        let mut expected = SNB_IRP_EVENT_GROUPS.to_vec();
        expected.rotate_left(1);
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            expected
        );
    }

    #[test]
    fn uses_documented_snb_ivb_irp_event_encodings() {
        assert_event(SnbIrpEventKind::Clockticks, 0x00, 0x00);
        assert_event(SnbIrpEventKind::PcieInboundReads, 0x15, 0x01);
        assert_event(SnbIrpEventKind::PcieInboundWrites, 0x15, 0x02);
        assert_event(SnbIrpEventKind::LostOwnership, 0x16, 0x01);
        assert_event(SnbIrpEventKind::TotalIrpOccupancy, 0x12, 0x01);
    }

    #[test]
    fn uses_snb_ivb_irp_device_ids() {
        assert_eq!(SnbIrpArchitecture::Snb.irp_device_id(), 0x3c40);
        assert_eq!(SnbIrpArchitecture::Ivb.irp_device_id(), 0x0e39);
        assert_eq!(SnbIrpArchitecture::Snb.ubox_device_id(), 0x3ce0);
        assert_eq!(SnbIrpArchitecture::Ivb.ubox_device_id(), 0x0e1e);
    }

    #[test]
    fn encodes_snb_ivb_unit_control_values() {
        assert_eq!(SnbIrpArchitecture::Snb.unit_freeze(), 0x10100);
        assert_eq!(SnbIrpArchitecture::Snb.unit_freeze_and_reset(), 0x10103);
        assert_eq!(SnbIrpArchitecture::Snb.unit_unfreeze(), 0x10000);
        assert_eq!(SnbIrpArchitecture::Ivb.unit_freeze(), 0x100);
        assert_eq!(SnbIrpArchitecture::Ivb.unit_freeze_and_reset(), 0x103);
        assert_eq!(SnbIrpArchitecture::Ivb.unit_unfreeze(), 0);
    }

    #[test]
    fn uses_snb_ivb_irp_pci_address_map() {
        assert_eq!(IRP_UNIT_CONTROL_OFFSET, 0xf4);
        assert_eq!(IRP_CONTROL_OFFSETS, [0xd8, 0xdc, 0xe0, 0xe4]);
        assert_eq!(IRP_COUNTER_OFFSETS, [0xa0, 0xb0, 0xb8, 0xc0]);
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
        assert_eq!(mask_irp_counter((1_u64 << 50) | 7), 7);
    }

    fn assert_event(kind: SnbIrpEventKind, event: u8, umask: u8) {
        let event_spec = SNB_IRP_EVENT_GROUPS
            .iter()
            .flat_map(|group| group.events)
            .find(|event_spec| event_spec.kind == kind)
            .unwrap();

        assert_eq!(event_spec.event, event);
        assert_eq!(event_spec.umask, umask);
    }

    fn measurement(
        kind: SnbIrpEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (SnbIrpEventKind, SnbIrpEventMeasurement) {
        (
            kind,
            SnbIrpEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn slice_groups(slices: Vec<SnbIrpMeasurementSlice>) -> Vec<SnbIrpEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }

    fn test_collector(architecture: SnbIrpArchitecture) -> SnbIrpCollector {
        SnbIrpCollector {
            architecture,
            next_group: 0,
            packages: Vec::new(),
        }
    }
}
