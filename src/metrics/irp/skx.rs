use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::arch::skx::pmon;
use crate::metal::msr::Msr;
use crate::metrics::uncore::skx::{
    BYTES_PER_CACHE_LINE, SKX_UNCORE_COUNTER_WIDTH, SkxIioStack, UncoreScope, events_per_second,
    frequency_hz, mask_counter, measurement_round_count, queue_residency_seconds, ratio,
    scale_to_enabled, uncore_leaders,
};
use crate::metrics::{MetricEvent, MetricUpdate};

const IRP_COUNTER_COUNT: usize = 2;

const IRP_COUNTER_BASE: u64 = 0x0a59;
const IRP_CONTROL_BASE: u64 = 0x0a5b;
const IRP_UNIT_CONTROL_BASE: u64 = 0x0a58;
const IRP_UNIT_STRIDE: u64 = 0x20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IrpEventKind {
    ClFlush,
    Clockticks,
    CoreRead,
    DemandRead,
    FafOccupancy,
    PciDcaHint,
    PciItoM,
    PcieReadCurrent,
    ReadForOwnership,
    TotalIrpOccupancy,
    WbMtoI,
    WriteInserts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IrpEventSpec {
    event: u8,
    kind: IrpEventKind,
    umask: u8,
}

impl IrpEventSpec {
    const fn sum(kind: IrpEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IrpEventGroup {
    events: [IrpEventSpec; IRP_COUNTER_COUNT],
}

impl IrpEventGroup {
    fn events(self) -> [IrpEventSpec; IRP_COUNTER_COUNT] {
        self.events
    }
}

const SKX_IRP_EVENT_GROUPS: [IrpEventGroup; 6] = [
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::PcieReadCurrent, 0x10, 0x01),
            IrpEventSpec::sum(IrpEventKind::CoreRead, 0x10, 0x02),
        ],
    },
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::DemandRead, 0x10, 0x04),
            IrpEventSpec::sum(IrpEventKind::ReadForOwnership, 0x10, 0x08),
        ],
    },
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::PciItoM, 0x10, 0x10),
            IrpEventSpec::sum(IrpEventKind::PciDcaHint, 0x10, 0x20),
        ],
    },
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::WbMtoI, 0x10, 0x40),
            IrpEventSpec::sum(IrpEventKind::ClFlush, 0x10, 0x80),
        ],
    },
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::TotalIrpOccupancy, 0x0f, 0x04),
            IrpEventSpec::sum(IrpEventKind::FafOccupancy, 0x19, 0x00),
        ],
    },
    IrpEventGroup {
        events: [
            IrpEventSpec::sum(IrpEventKind::WriteInserts, 0x11, 0x08),
            IrpEventSpec::sum(IrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
];

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IrpScopeMetrics {
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub clflush_bytes_per_second: f64,
    pub core_read_bytes_per_second: f64,
    pub demand_read_bytes_per_second: f64,
    pub faf_occupancy_entries: f64,
    pub frequency_hz: f64,
    pub pci_dca_hint_bytes_per_second: f64,
    pub pci_itom_bytes_per_second: f64,
    pub pcie_read_current_bytes_per_second: f64,
    pub read_for_ownership_bytes_per_second: f64,
    pub stack: SkxIioStack,
    pub total_irp_occupancy_entries: f64,
    pub wbmtoi_bytes_per_second: f64,
    pub write_inserts_per_second: f64,
    pub write_latency_seconds: f64,
}

impl IrpScopeMetrics {
    fn from_measurements(
        stack_scope: IrpStackScope,
        measurements: &BTreeMap<IrpEventKind, IrpEventMeasurement>,
    ) -> Result<Self, String> {
        let clflush = required_measurement(measurements, IrpEventKind::ClFlush)?;
        let clockticks = required_measurement(measurements, IrpEventKind::Clockticks)?;
        let core_read = required_measurement(measurements, IrpEventKind::CoreRead)?;
        let demand_read = required_measurement(measurements, IrpEventKind::DemandRead)?;
        let faf_occupancy = required_measurement(measurements, IrpEventKind::FafOccupancy)?;
        let pci_dca_hint = required_measurement(measurements, IrpEventKind::PciDcaHint)?;
        let pci_itom = required_measurement(measurements, IrpEventKind::PciItoM)?;
        let pcie_read_current = required_measurement(measurements, IrpEventKind::PcieReadCurrent)?;
        let read_for_ownership =
            required_measurement(measurements, IrpEventKind::ReadForOwnership)?;
        let total_irp_occupancy =
            required_measurement(measurements, IrpEventKind::TotalIrpOccupancy)?;
        let wbmtoi = required_measurement(measurements, IrpEventKind::WbMtoI)?;
        let write_inserts = required_measurement(measurements, IrpEventKind::WriteInserts)?;

        Ok(Self {
            scope: stack_scope.scope,
            clflush_bytes_per_second: bytes_per_second(clflush),
            core_read_bytes_per_second: bytes_per_second(core_read),
            demand_read_bytes_per_second: bytes_per_second(demand_read),
            faf_occupancy_entries: occupancy_entries(faf_occupancy, clockticks),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            pci_dca_hint_bytes_per_second: bytes_per_second(pci_dca_hint),
            pci_itom_bytes_per_second: bytes_per_second(pci_itom),
            pcie_read_current_bytes_per_second: bytes_per_second(pcie_read_current),
            read_for_ownership_bytes_per_second: bytes_per_second(read_for_ownership),
            stack: stack_scope.stack,
            total_irp_occupancy_entries: occupancy_entries(total_irp_occupancy, clockticks),
            wbmtoi_bytes_per_second: bytes_per_second(wbmtoi),
            write_inserts_per_second: events_per_second(
                scale_to_enabled(
                    write_inserts.value,
                    write_inserts.enabled,
                    write_inserts.running,
                ),
                write_inserts.enabled,
            ),
            write_latency_seconds: write_latency_seconds(
                total_irp_occupancy,
                faf_occupancy,
                write_inserts,
                clockticks,
            ),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct IrpMetrics {
    pub scopes: Vec<IrpScopeMetrics>,
}

impl IrpMetrics {
    fn from_measurements(
        measurements: BTreeMap<IrpStackScope, BTreeMap<IrpEventKind, IrpEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(IrpScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct IrpCollector {
    next_group: usize,
    packages: Vec<IrpPackage>,
}

impl IrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
        })
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            architecture.intel_server_model(),
            IntelServerCpuModel::SkylakeXeon
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IrpMetrics, String> {
        if interval.is_zero() {
            return Err("IRP measure interval must be non-zero".to_string());
        }

        let mut measurements = IrpMeasurementAccumulator::new();
        let packages = &self.packages;

        for slice in self.schedule(interval) {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                IrpMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        IrpMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % SKX_IRP_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IrpMeasurementSlice> {
        let group_count = SKX_IRP_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IrpMeasurementSlice {
                    duration: slice_duration,
                    group: SKX_IRP_EVENT_GROUPS[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IrpMeasurementSlice {
    duration: Duration,
    group: IrpEventGroup,
}

#[derive(Debug)]
pub struct IrpTask {
    collector: IrpCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl IrpTask {
    pub fn new(
        collector: IrpCollector,
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
        loop {
            match self.collector.sample(self.interval).await {
                Ok(irp) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Irp(irp))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = self.events.send(MetricEvent::Failure(error)).await;
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct IrpScopeLabels {
    die: String,
    die_group: String,
    package: String,
    stack: String,
}

impl IrpScopeLabels {
    fn from_scope(scope: UncoreScope, stack: SkxIioStack) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct IrpPrometheusMetrics {
    clflush_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    core_read_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    demand_read_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    faf_occupancy_entries: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    pci_dca_hint_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    pci_itom_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_current_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    read_for_ownership_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    total_irp_occupancy_entries: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    wbmtoi_bytes_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    write_inserts_per_second: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
    write_latency_seconds: Family<IrpScopeLabels, Gauge<f64, AtomicU64>>,
}

impl IrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            clflush_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            core_read_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            demand_read_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            faf_occupancy_entries: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pci_dca_hint_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            pci_itom_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_read_current_bytes_per_second:
                Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_for_ownership_bytes_per_second:
                Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            total_irp_occupancy_entries: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wbmtoi_bytes_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_inserts_per_second: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_latency_seconds: Family::<IrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_irp_clflush_bytes_per_second",
            "Interval-derived IRP CLFlush bandwidth in bytes per second",
            metrics.clflush_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_core_read_bytes_per_second",
            "Interval-derived IRP core read bandwidth in bytes per second",
            metrics.core_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_demand_read_bytes_per_second",
            "Interval-derived IRP demand read bandwidth in bytes per second",
            metrics.demand_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_faf_occupancy_entries",
            "Average IRP fire-and-forget queue occupancy in entries",
            metrics.faf_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_irp_frequency_hz",
            "Interval-derived IRP clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_irp_pci_dca_hint_bytes_per_second",
            "Interval-derived IRP PCI DCA hint bandwidth in bytes per second",
            metrics.pci_dca_hint_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_pci_itom_bytes_per_second",
            "Interval-derived IRP PCI ItoM bandwidth in bytes per second",
            metrics.pci_itom_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_pcie_read_current_bytes_per_second",
            "Interval-derived IRP PCIe read current bandwidth in bytes per second",
            metrics.pcie_read_current_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_read_for_ownership_bytes_per_second",
            "Interval-derived IRP RFO bandwidth in bytes per second",
            metrics.read_for_ownership_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_total_occupancy_entries",
            "Average total IRP read and write occupancy in entries",
            metrics.total_irp_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_irp_wbmtoi_bytes_per_second",
            "Interval-derived IRP WbMtoI bandwidth in bytes per second",
            metrics.wbmtoi_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_write_inserts_per_second",
            "Interval-derived IRP inbound write fast-path inserts per second",
            metrics.write_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_write_latency_seconds",
            "Interval-derived IRP inbound write residency latency in seconds",
            metrics.write_latency_seconds.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: IrpMetrics) {
        for scope in metrics.scopes {
            let labels = IrpScopeLabels::from_scope(scope.scope, scope.stack);

            self.clflush_bytes_per_second
                .get_or_create(&labels)
                .set(scope.clflush_bytes_per_second);
            self.core_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.core_read_bytes_per_second);
            self.demand_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.demand_read_bytes_per_second);
            self.faf_occupancy_entries
                .get_or_create(&labels)
                .set(scope.faf_occupancy_entries);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.pci_dca_hint_bytes_per_second
                .get_or_create(&labels)
                .set(scope.pci_dca_hint_bytes_per_second);
            self.pci_itom_bytes_per_second
                .get_or_create(&labels)
                .set(scope.pci_itom_bytes_per_second);
            self.pcie_read_current_bytes_per_second
                .get_or_create(&labels)
                .set(scope.pcie_read_current_bytes_per_second);
            self.read_for_ownership_bytes_per_second
                .get_or_create(&labels)
                .set(scope.read_for_ownership_bytes_per_second);
            self.total_irp_occupancy_entries
                .get_or_create(&labels)
                .set(scope.total_irp_occupancy_entries);
            self.wbmtoi_bytes_per_second
                .get_or_create(&labels)
                .set(scope.wbmtoi_bytes_per_second);
            self.write_inserts_per_second
                .get_or_create(&labels)
                .set(scope.write_inserts_per_second);
            self.write_latency_seconds
                .get_or_create(&labels)
                .set(scope.write_latency_seconds);
        }
    }
}

#[derive(Debug)]
struct IrpPackage {
    scope: UncoreScope,
    units: Vec<IrpUnit>,
}

impl IrpPackage {
    fn new(cpu: u32, scope: UncoreScope) -> Self {
        let units = SkxIioStack::ALL
            .into_iter()
            .map(|stack| IrpUnit { cpu, stack })
            .collect();

        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct IrpUnit {
    cpu: u32,
    stack: SkxIioStack,
}

impl IrpUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE_AND_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE))
    }

    fn program(self, group: IrpEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events().into_iter().enumerate() {
            msr.write(
                irp_control_offset(self.stack, counter_index),
                u64::from(pmon::counter_control(event.event, event.umask, true)),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<IrpUnitReading, String> {
        Ok(IrpUnitReading {
            counters: [
                self.read_counter(0).map(mask_irp_counter)?,
                self.read_counter(1).map(mask_irp_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_UNFREEZE))
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(irp_counter_offset(self.stack, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(irp_unit_control_offset(self.stack), value)
    }
}

#[derive(Clone, Copy, Debug)]
struct IrpUnitReading {
    counters: [u64; IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct IrpEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl IrpEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct IrpMeasurement {
    enabled: Duration,
    group: IrpEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IrpStackScope {
    scope: UncoreScope,
    stack: SkxIioStack,
}

#[derive(Debug, Default)]
struct IrpMeasurementAccumulator {
    measurements: BTreeMap<IrpStackScope, BTreeMap<IrpEventKind, IrpEventMeasurement>>,
}

impl IrpMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: UncoreScope,
        stack: SkxIioStack,
        kind: IrpEventKind,
        value: u64,
        measurement: IrpMeasurement,
    ) {
        self.measurements
            .entry(IrpStackScope { scope, stack })
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(IrpEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<IrpStackScope, BTreeMap<IrpEventKind, IrpEventMeasurement>> {
        self.measurements
    }
}

fn bytes_per_second(measurement: &IrpEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    ) * BYTES_PER_CACHE_LINE
}

fn occupancy_entries(occupancy: &IrpEventMeasurement, clockticks: &IrpEventMeasurement) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    ratio(occupancy, clockticks)
}

fn write_latency_seconds(
    total_occupancy: &IrpEventMeasurement,
    faf_occupancy: &IrpEventMeasurement,
    inserts: &IrpEventMeasurement,
    clockticks: &IrpEventMeasurement,
) -> f64 {
    let total_occupancy = scale_to_enabled(
        total_occupancy.value,
        total_occupancy.enabled,
        total_occupancy.running,
    );
    let faf_occupancy = scale_to_enabled(
        faf_occupancy.value,
        faf_occupancy.enabled,
        faf_occupancy.running,
    );
    let write_occupancy = total_occupancy.saturating_sub(faf_occupancy);
    let insert_count = scale_to_enabled(inserts.value, inserts.enabled, inserts.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    queue_residency_seconds(write_occupancy, insert_count, clockticks, inserts.enabled)
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<IrpPackage>, String> {
    if !matches!(model, IntelServerCpuModel::SkylakeXeon) {
        return Err(format!("IRP collection is not supported for {model:?}"));
    }

    let leaders = uncore_leaders()?;
    let packages = leaders
        .into_iter()
        .map(|leader| IrpPackage::new(leader.cpu, leader.scope))
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any IRP packages".to_string());
    }

    Ok(packages)
}

fn freeze_packages(packages: &[IrpPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn irp_control_offset(stack: SkxIioStack, counter_index: usize) -> u64 {
    irp_unit_offset(IRP_CONTROL_BASE, stack) + counter_index as u64
}

fn irp_counter_offset(stack: SkxIioStack, counter_index: usize) -> u64 {
    irp_unit_offset(IRP_COUNTER_BASE, stack) + counter_index as u64
}

fn irp_unit_control_offset(stack: SkxIioStack) -> u64 {
    irp_unit_offset(IRP_UNIT_CONTROL_BASE, stack)
}

fn irp_unit_offset(base: u64, stack: SkxIioStack) -> u64 {
    base + IRP_UNIT_STRIDE * stack.id() as u64
}

fn mask_irp_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn probe_writable_msrs(packages: &[IrpPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn program_packages(packages: &[IrpPackage], group: IrpEventGroup) -> Result<(), String> {
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
    packages: &[IrpPackage],
    measurement: IrpMeasurement,
    measurements: &mut IrpMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            let reading = unit.read()?;

            for counter_index in 0..IRP_COUNTER_COUNT {
                let event = measurement.group.events()[counter_index];
                measurements.add(
                    package.scope,
                    unit.stack,
                    event.kind,
                    reading.counters[counter_index],
                    measurement,
                );
            }
        }
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<IrpEventKind, IrpEventMeasurement>,
    kind: IrpEventKind,
) -> Result<&IrpEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IRP measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[IrpPackage]) -> Result<(), String> {
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
    fn computes_irp_metrics() {
        let scope = test_scope();
        let metrics = IrpMetrics::from_measurements(BTreeMap::from([(
            test_stack_scope(scope, SkxIioStack::Pcie0),
            BTreeMap::from([
                measurement(IrpEventKind::ClFlush, 100, 100),
                measurement(IrpEventKind::Clockticks, 1_000, 100),
                measurement(IrpEventKind::CoreRead, 200, 100),
                measurement(IrpEventKind::DemandRead, 300, 100),
                measurement(IrpEventKind::FafOccupancy, 200, 100),
                measurement(IrpEventKind::PciDcaHint, 400, 100),
                measurement(IrpEventKind::PciItoM, 200, 100),
                measurement(IrpEventKind::PcieReadCurrent, 300, 100),
                measurement(IrpEventKind::ReadForOwnership, 400, 100),
                measurement(IrpEventKind::TotalIrpOccupancy, 500, 100),
                measurement(IrpEventKind::WbMtoI, 500, 100),
                measurement(IrpEventKind::WriteInserts, 600, 100),
            ]),
        )]))
        .unwrap();

        let scope_metrics = metrics.scopes[0];
        assert_eq!(scope_metrics.clflush_bytes_per_second, 64_000.0);
        assert_eq!(scope_metrics.core_read_bytes_per_second, 128_000.0);
        assert_eq!(scope_metrics.demand_read_bytes_per_second, 192_000.0);
        assert_eq!(scope_metrics.faf_occupancy_entries, 0.2);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(scope_metrics.pci_dca_hint_bytes_per_second, 256_000.0);
        assert_eq!(scope_metrics.pci_itom_bytes_per_second, 128_000.0);
        assert_eq!(scope_metrics.pcie_read_current_bytes_per_second, 192_000.0);
        assert_eq!(scope_metrics.read_for_ownership_bytes_per_second, 256_000.0);
        assert_eq!(scope_metrics.stack, SkxIioStack::Pcie0);
        assert_eq!(scope_metrics.total_irp_occupancy_entries, 0.5);
        assert_eq!(scope_metrics.wbmtoi_bytes_per_second, 320_000.0);
        assert_eq!(scope_metrics.write_inserts_per_second, 6_000.0);
        assert_eq!(scope_metrics.write_latency_seconds, 0.00005);
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_secs(1))),
            vec![
                SKX_IRP_EVENT_GROUPS[0],
                SKX_IRP_EVENT_GROUPS[1],
                SKX_IRP_EVENT_GROUPS[2],
                SKX_IRP_EVENT_GROUPS[3],
                SKX_IRP_EVENT_GROUPS[4],
                SKX_IRP_EVENT_GROUPS[5],
                SKX_IRP_EVENT_GROUPS[0],
                SKX_IRP_EVENT_GROUPS[1],
                SKX_IRP_EVENT_GROUPS[2],
                SKX_IRP_EVENT_GROUPS[3],
                SKX_IRP_EVENT_GROUPS[4],
                SKX_IRP_EVENT_GROUPS[5],
            ]
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                SKX_IRP_EVENT_GROUPS[1],
                SKX_IRP_EVENT_GROUPS[2],
                SKX_IRP_EVENT_GROUPS[3],
                SKX_IRP_EVENT_GROUPS[4],
                SKX_IRP_EVENT_GROUPS[5],
                SKX_IRP_EVENT_GROUPS[0],
            ]
        );
    }

    #[test]
    fn uses_full_skx_irp_stack_address_map() {
        assert_eq!(irp_unit_control_offset(SkxIioStack::CbdmaDmi), 0x0a58);
        assert_eq!(irp_counter_offset(SkxIioStack::CbdmaDmi, 0), 0x0a59);
        assert_eq!(irp_control_offset(SkxIioStack::CbdmaDmi, 0), 0x0a5b);

        assert_eq!(irp_unit_control_offset(SkxIioStack::Pcie0), 0x0a78);
        assert_eq!(irp_unit_control_offset(SkxIioStack::Mcp1), 0x0af8);
        assert_eq!(irp_counter_offset(SkxIioStack::Mcp1, 1), 0x0afa);
        assert_eq!(irp_control_offset(SkxIioStack::Mcp1, 1), 0x0afc);
    }

    #[test]
    fn supports_only_skylake_xeon_uncore_spec() {
        assert!(IrpCollector::is_supported(&test_architecture(0x55)));
        assert!(!IrpCollector::is_supported(&test_architecture(0xcf)));
    }

    fn measurement(
        kind: IrpEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (IrpEventKind, IrpEventMeasurement) {
        (
            kind,
            IrpEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn slice_groups(slices: Vec<IrpMeasurementSlice>) -> Vec<IrpEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
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

    fn test_collector() -> IrpCollector {
        IrpCollector {
            next_group: 0,
            packages: Vec::new(),
        }
    }

    fn test_scope() -> UncoreScope {
        UncoreScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        }
    }

    fn test_stack_scope(scope: UncoreScope, stack: SkxIioStack) -> IrpStackScope {
        IrpStackScope { scope, stack }
    }
}
