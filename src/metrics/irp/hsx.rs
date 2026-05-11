use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::arch::skx::pmon;
use crate::metal::pci::PciDevice;
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE};
use crate::metrics::uncore::hsx::{
    HsxUncoreScope, events_per_second, frequency_hz, ratio, scale_to_enabled,
};

const IRP_COUNTER_COUNT: usize = 4;
const IRP_COUNTER_WIDTH: u32 = 48;
const IRP0_COUNTER0_OFFSET: u64 = 0xa0;
const IRP0_COUNTER1_OFFSET: u64 = 0xb0;
const IRP1_COUNTER0_OFFSET: u64 = 0xb8;
const IRP1_COUNTER1_OFFSET: u64 = 0xc0;
const IRP_CONTROL_OFFSETS: [u64; IRP_COUNTER_COUNT] = [0xd8, 0xdc, 0xe0, 0xe4];
const IRP_COUNTER_OFFSETS: [u64; IRP_COUNTER_COUNT] = [
    IRP0_COUNTER0_OFFSET,
    IRP0_COUNTER1_OFFSET,
    IRP1_COUNTER0_OFFSET,
    IRP1_COUNTER1_OFFSET,
];
const IRP_UNIT_CONTROL_OFFSET: u64 = 0xf4;

const HSX_IRP_EVENT_GROUPS: [HsxIrpEventGroup; 4] = [
    HsxIrpEventGroup {
        events: [
            HsxIrpEventSpec::sum(HsxIrpEventKind::PcieReadCurrent, 0x13, 0x01),
            HsxIrpEventSpec::sum(HsxIrpEventKind::CoreRead, 0x13, 0x02),
            HsxIrpEventSpec::sum(HsxIrpEventKind::DemandRead, 0x13, 0x04),
            HsxIrpEventSpec::sum(HsxIrpEventKind::ReadForOwnership, 0x13, 0x08),
        ],
    },
    HsxIrpEventGroup {
        events: [
            HsxIrpEventSpec::sum(HsxIrpEventKind::PciItoM, 0x13, 0x10),
            HsxIrpEventSpec::sum(HsxIrpEventKind::PciDcaHint, 0x13, 0x20),
            HsxIrpEventSpec::sum(HsxIrpEventKind::WbMtoI, 0x13, 0x40),
            HsxIrpEventSpec::sum(HsxIrpEventKind::ClFlush, 0x13, 0x80),
        ],
    },
    HsxIrpEventGroup {
        events: [
            HsxIrpEventSpec::sum(HsxIrpEventKind::TotalIrpOccupancy, 0x12, 0x01),
            HsxIrpEventSpec::sum(HsxIrpEventKind::PcieInboundReads, 0x16, 0x01),
            HsxIrpEventSpec::sum(HsxIrpEventKind::PcieInboundWrites, 0x16, 0x02),
            HsxIrpEventSpec::sum(HsxIrpEventKind::LostFwd, 0x15, 0x10),
        ],
    },
    HsxIrpEventGroup {
        events: [
            HsxIrpEventSpec::sum(HsxIrpEventKind::Clockticks, 0x00, 0x00),
            HsxIrpEventSpec::disabled(),
            HsxIrpEventSpec::disabled(),
            HsxIrpEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxIrpEventKind {
    ClFlush,
    Clockticks,
    CoreRead,
    DemandRead,
    Disabled,
    LostFwd,
    PciDcaHint,
    PciItoM,
    PcieInboundReads,
    PcieInboundWrites,
    PcieReadCurrent,
    ReadForOwnership,
    TotalIrpOccupancy,
    WbMtoI,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxIrpEventSpec {
    event: u8,
    kind: HsxIrpEventKind,
    umask: u8,
}

impl HsxIrpEventSpec {
    const fn disabled() -> Self {
        Self {
            event: 0,
            kind: HsxIrpEventKind::Disabled,
            umask: 0,
        }
    }

    const fn sum(kind: HsxIrpEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxIrpEventGroup {
    events: [HsxIrpEventSpec; IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct HsxIrpScopeMetrics {
    #[serde(flatten)]
    pub scope: HsxUncoreScope,
    pub clflush_bytes_per_second: f64,
    pub core_read_bytes_per_second: f64,
    pub demand_read_bytes_per_second: f64,
    pub frequency_hz: f64,
    pub io_write_conflict_ratio: f64,
    pub pci_dca_hint_bytes_per_second: f64,
    pub pci_itom_bytes_per_second: f64,
    pub pcie_inbound_reads_per_second: f64,
    pub pcie_inbound_writes_per_second: f64,
    pub pcie_read_current_bytes_per_second: f64,
    pub read_for_ownership_bytes_per_second: f64,
    pub total_irp_occupancy_entries: f64,
    pub wbmtoi_bytes_per_second: f64,
}

impl HsxIrpScopeMetrics {
    fn from_measurements(
        scope: HsxUncoreScope,
        measurements: &BTreeMap<HsxIrpEventKind, HsxIrpEventMeasurement>,
    ) -> Result<Self, String> {
        let clflush = required_measurement(measurements, HsxIrpEventKind::ClFlush)?;
        let clockticks = required_measurement(measurements, HsxIrpEventKind::Clockticks)?;
        let core_read = required_measurement(measurements, HsxIrpEventKind::CoreRead)?;
        let demand_read = required_measurement(measurements, HsxIrpEventKind::DemandRead)?;
        let lost_fwd = required_measurement(measurements, HsxIrpEventKind::LostFwd)?;
        let pci_dca_hint = required_measurement(measurements, HsxIrpEventKind::PciDcaHint)?;
        let pci_itom = required_measurement(measurements, HsxIrpEventKind::PciItoM)?;
        let pcie_inbound_reads =
            required_measurement(measurements, HsxIrpEventKind::PcieInboundReads)?;
        let pcie_inbound_writes =
            required_measurement(measurements, HsxIrpEventKind::PcieInboundWrites)?;
        let pcie_read_current =
            required_measurement(measurements, HsxIrpEventKind::PcieReadCurrent)?;
        let read_for_ownership =
            required_measurement(measurements, HsxIrpEventKind::ReadForOwnership)?;
        let total_irp_occupancy =
            required_measurement(measurements, HsxIrpEventKind::TotalIrpOccupancy)?;
        let wbmtoi = required_measurement(measurements, HsxIrpEventKind::WbMtoI)?;
        let lost_fwd_count = scale_to_enabled(lost_fwd.value, lost_fwd.enabled, lost_fwd.running);
        let write_insert_count = scale_to_enabled(
            pcie_inbound_writes.value,
            pcie_inbound_writes.enabled,
            pcie_inbound_writes.running,
        );

        Ok(Self {
            scope,
            clflush_bytes_per_second: bytes_per_second(clflush),
            core_read_bytes_per_second: bytes_per_second(core_read),
            demand_read_bytes_per_second: bytes_per_second(demand_read),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            io_write_conflict_ratio: ratio(lost_fwd_count, write_insert_count),
            pci_dca_hint_bytes_per_second: bytes_per_second(pci_dca_hint),
            pci_itom_bytes_per_second: bytes_per_second(pci_itom),
            pcie_inbound_reads_per_second: event_rate(pcie_inbound_reads),
            pcie_inbound_writes_per_second: event_rate(pcie_inbound_writes),
            pcie_read_current_bytes_per_second: bytes_per_second(pcie_read_current),
            read_for_ownership_bytes_per_second: bytes_per_second(read_for_ownership),
            total_irp_occupancy_entries: occupancy_entries(total_irp_occupancy, clockticks),
            wbmtoi_bytes_per_second: bytes_per_second(wbmtoi),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HsxIrpMetrics {
    pub scopes: Vec<HsxIrpScopeMetrics>,
}

impl HsxIrpMetrics {
    fn from_measurements(
        measurements: BTreeMap<HsxUncoreScope, BTreeMap<HsxIrpEventKind, HsxIrpEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(HsxIrpScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct HsxIrpCollector {
    next_group: usize,
    packages: Vec<HsxIrpPackage>,
}

impl HsxIrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let packages = discover_packages(model)?;
        probe_writable_pci(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxIrpMetrics, String> {
        if interval.is_zero() {
            return Err("Haswell/Broadwell IRP measure interval must be non-zero".to_string());
        }

        let mut measurements = HsxIrpMeasurementAccumulator::new();
        let packages = &self.packages;

        for slice in self.schedule(interval) {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                HsxIrpMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        HsxIrpMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % HSX_IRP_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<HsxIrpMeasurementSlice> {
        let group_count = HSX_IRP_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(HsxIrpMeasurementSlice {
                    duration: slice_duration,
                    group: HSX_IRP_EVENT_GROUPS[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxIrpMeasurementSlice {
    duration: Duration,
    group: HsxIrpEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct HsxIrpScopeLabels {
    package: String,
}

impl HsxIrpScopeLabels {
    fn from_scope(scope: HsxUncoreScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
pub struct HsxIrpPrometheusMetrics {
    clflush_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    core_read_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    demand_read_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    io_write_conflict_ratio: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pci_dca_hint_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pci_itom_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_reads_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_writes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_current_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    read_for_ownership_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    total_irp_occupancy_entries: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    wbmtoi_bytes_per_second: Family<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>,
}

impl HsxIrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            clflush_bytes_per_second: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            core_read_bytes_per_second: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            demand_read_bytes_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            io_write_conflict_ratio: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pci_dca_hint_bytes_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pci_itom_bytes_per_second: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            pcie_inbound_reads_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_writes_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_read_current_bytes_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_for_ownership_bytes_per_second:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            total_irp_occupancy_entries:
                Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wbmtoi_bytes_per_second: Family::<HsxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
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
            "ocellus_irp_frequency_hz",
            "Interval-derived IRP clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_irp_io_write_conflict_ratio",
            "Interval-derived IRP I/O write conflict ratio from lost forwards over PCIe inbound writes",
            metrics.io_write_conflict_ratio.clone(),
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

        metrics
    }

    pub fn update(&self, metrics: HsxIrpMetrics) {
        for scope in metrics.scopes {
            let labels = HsxIrpScopeLabels::from_scope(scope.scope);

            self.clflush_bytes_per_second
                .get_or_create(&labels)
                .set(scope.clflush_bytes_per_second);
            self.core_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.core_read_bytes_per_second);
            self.demand_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.demand_read_bytes_per_second);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.io_write_conflict_ratio
                .get_or_create(&labels)
                .set(scope.io_write_conflict_ratio);
            self.pci_dca_hint_bytes_per_second
                .get_or_create(&labels)
                .set(scope.pci_dca_hint_bytes_per_second);
            self.pci_itom_bytes_per_second
                .get_or_create(&labels)
                .set(scope.pci_itom_bytes_per_second);
            self.pcie_inbound_reads_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_reads_per_second);
            self.pcie_inbound_writes_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_writes_per_second);
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
        }
    }
}

#[derive(Debug)]
struct HsxIrpPackage {
    scope: HsxUncoreScope,
    unit: HsxIrpUnit,
}

#[derive(Debug)]
struct HsxIrpUnit {
    device: PciDevice,
}

impl HsxIrpUnit {
    fn new(location: metal::pci::PciLocation) -> Result<Self, String> {
        Ok(Self {
            device: PciDevice::open(location)?,
        })
    }

    fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IRP_UNIT_CONTROL_OFFSET, pmon::UNIT_FREEZE)
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.device
            .write_u32(IRP_UNIT_CONTROL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)
    }

    fn program(&self, group: HsxIrpEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            if event.kind == HsxIrpEventKind::Disabled {
                self.device
                    .write_u32(IRP_CONTROL_OFFSETS[counter_index], 0)?;
                continue;
            }

            self.device.write_u32(
                IRP_CONTROL_OFFSETS[counter_index],
                pmon::counter_control(event.event, event.umask, true),
            )?;
        }

        Ok(())
    }

    fn read(&self) -> Result<HsxIrpUnitReading, String> {
        let mut counters = [0; IRP_COUNTER_COUNT];

        for counter_index in 0..IRP_COUNTER_COUNT {
            counters[counter_index] =
                mask_irp_counter(self.device.read_u64(IRP_COUNTER_OFFSETS[counter_index])?);
        }

        Ok(HsxIrpUnitReading { counters })
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IRP_UNIT_CONTROL_OFFSET, pmon::UNIT_UNFREEZE)
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxIrpUnitReading {
    counters: [u64; IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct HsxIrpEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl HsxIrpEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxIrpMeasurement {
    enabled: Duration,
    group: HsxIrpEventGroup,
    running: Duration,
}

#[derive(Debug, Default)]
struct HsxIrpMeasurementAccumulator {
    measurements: BTreeMap<HsxUncoreScope, BTreeMap<HsxIrpEventKind, HsxIrpEventMeasurement>>,
}

impl HsxIrpMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: HsxUncoreScope,
        kind: HsxIrpEventKind,
        value: u64,
        measurement: HsxIrpMeasurement,
    ) {
        if kind == HsxIrpEventKind::Disabled {
            return;
        }

        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(HsxIrpEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<HsxUncoreScope, BTreeMap<HsxIrpEventKind, HsxIrpEventMeasurement>> {
        self.measurements
    }
}

fn bytes_per_second(measurement: &HsxIrpEventMeasurement) -> f64 {
    event_rate(measurement) * BYTES_PER_CACHE_LINE
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<HsxIrpPackage>, String> {
    if !matches!(
        model,
        IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon
    ) {
        return Err(format!(
            "Haswell/Broadwell IRP collection is not supported for {model:?}"
        ));
    }

    let locations = metal::arch::hsx::pci::irp_locations(model)?;
    let mut packages = Vec::with_capacity(locations.len());

    for socket_location in locations {
        packages.push(HsxIrpPackage {
            scope: HsxUncoreScope {
                package_id: socket_location.package_id,
            },
            unit: HsxIrpUnit::new(socket_location.location)?,
        });
    }

    Ok(packages)
}

fn event_rate(measurement: &HsxIrpEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn freeze_packages(packages: &[HsxIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }

    Ok(())
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
    occupancy: &HsxIrpEventMeasurement,
    clockticks: &HsxIrpEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    ratio(occupancy, clockticks)
}

fn probe_writable_pci(packages: &[HsxIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }

    Ok(())
}

fn program_packages(packages: &[HsxIrpPackage], group: HsxIrpEventGroup) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
    }

    for package in packages {
        package.unit.program(group)?;
    }

    Ok(())
}

fn read_packages(
    packages: &[HsxIrpPackage],
    measurement: HsxIrpMeasurement,
    measurements: &mut HsxIrpMeasurementAccumulator,
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
    measurements: &BTreeMap<HsxIrpEventKind, HsxIrpEventMeasurement>,
    kind: HsxIrpEventKind,
) -> Result<&HsxIrpEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("Haswell/Broadwell IRP measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[HsxIrpPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_hsx_irp_metrics() {
        let scope = HsxUncoreScope { package_id: 0 };
        let metrics = HsxIrpMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(HsxIrpEventKind::ClFlush, 100, 100),
                measurement(HsxIrpEventKind::Clockticks, 1_000, 100),
                measurement(HsxIrpEventKind::CoreRead, 200, 100),
                measurement(HsxIrpEventKind::DemandRead, 300, 100),
                measurement(HsxIrpEventKind::LostFwd, 150, 100),
                measurement(HsxIrpEventKind::PciDcaHint, 400, 100),
                measurement(HsxIrpEventKind::PciItoM, 200, 100),
                measurement(HsxIrpEventKind::PcieInboundReads, 250, 100),
                measurement(HsxIrpEventKind::PcieInboundWrites, 600, 100),
                measurement(HsxIrpEventKind::PcieReadCurrent, 300, 100),
                measurement(HsxIrpEventKind::ReadForOwnership, 400, 100),
                measurement(HsxIrpEventKind::TotalIrpOccupancy, 500, 100),
                measurement(HsxIrpEventKind::WbMtoI, 500, 100),
            ]),
        )]))
        .unwrap();

        let scope_metrics = metrics.scopes[0];
        assert_eq!(scope_metrics.clflush_bytes_per_second, 64_000.0);
        assert_eq!(scope_metrics.core_read_bytes_per_second, 128_000.0);
        assert_eq!(scope_metrics.demand_read_bytes_per_second, 192_000.0);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(scope_metrics.io_write_conflict_ratio, 0.25);
        assert_eq!(scope_metrics.pci_dca_hint_bytes_per_second, 256_000.0);
        assert_eq!(scope_metrics.pci_itom_bytes_per_second, 128_000.0);
        assert_eq!(scope_metrics.pcie_inbound_reads_per_second, 2_500.0);
        assert_eq!(scope_metrics.pcie_inbound_writes_per_second, 6_000.0);
        assert_eq!(scope_metrics.pcie_read_current_bytes_per_second, 192_000.0);
        assert_eq!(scope_metrics.read_for_ownership_bytes_per_second, 256_000.0);
        assert_eq!(scope_metrics.total_irp_occupancy_entries, 0.5);
        assert_eq!(scope_metrics.wbmtoi_bytes_per_second, 320_000.0);
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = HsxIrpCollector {
            next_group: 0,
            packages: Vec::new(),
        };

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_secs(1))),
            HSX_IRP_EVENT_GROUPS
                .into_iter()
                .chain(HSX_IRP_EVENT_GROUPS)
                .chain(HSX_IRP_EVENT_GROUPS)
                .collect::<Vec<_>>()
        );

        collector.rotate_group();
        let mut expected = HSX_IRP_EVENT_GROUPS.to_vec();
        expected.rotate_left(1);
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            expected
        );
    }

    #[test]
    fn uses_hsx_irp_event_encodings() {
        assert_event(HsxIrpEventKind::PcieReadCurrent, 0x13, 0x01);
        assert_event(HsxIrpEventKind::CoreRead, 0x13, 0x02);
        assert_event(HsxIrpEventKind::DemandRead, 0x13, 0x04);
        assert_event(HsxIrpEventKind::ReadForOwnership, 0x13, 0x08);
        assert_event(HsxIrpEventKind::PciItoM, 0x13, 0x10);
        assert_event(HsxIrpEventKind::PciDcaHint, 0x13, 0x20);
        assert_event(HsxIrpEventKind::WbMtoI, 0x13, 0x40);
        assert_event(HsxIrpEventKind::ClFlush, 0x13, 0x80);
        assert_event(HsxIrpEventKind::TotalIrpOccupancy, 0x12, 0x01);
        assert_event(HsxIrpEventKind::PcieInboundReads, 0x16, 0x01);
        assert_event(HsxIrpEventKind::PcieInboundWrites, 0x16, 0x02);
        assert_event(HsxIrpEventKind::LostFwd, 0x15, 0x10);
        assert_event(HsxIrpEventKind::Clockticks, 0x00, 0x00);
    }

    #[test]
    fn uses_hsx_irp_pci_address_map() {
        assert_eq!(IRP_UNIT_CONTROL_OFFSET, 0xf4);
        assert_eq!(IRP_CONTROL_OFFSETS, [0xd8, 0xdc, 0xe0, 0xe4]);
        assert_eq!(IRP_COUNTER_OFFSETS, [0xa0, 0xb0, 0xb8, 0xc0]);
    }

    fn assert_event(kind: HsxIrpEventKind, event: u8, umask: u8) {
        let event_spec = HSX_IRP_EVENT_GROUPS
            .iter()
            .flat_map(|group| group.events)
            .find(|event_spec| event_spec.kind == kind)
            .unwrap();

        assert_eq!(event_spec.event, event);
        assert_eq!(event_spec.umask, umask);
    }

    fn measurement(
        kind: HsxIrpEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (HsxIrpEventKind, HsxIrpEventMeasurement) {
        (
            kind,
            HsxIrpEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn slice_groups(slices: Vec<HsxIrpMeasurementSlice>) -> Vec<HsxIrpEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }
}
