use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::msr::Msr;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::BYTES_PER_CACHE_LINE;

const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const DEFAULT_MAX_SLICE: Duration = Duration::from_millis(100);
const ICX_IRP_COUNTER_COUNT: usize = 2;
const ICX_IRP_COUNTER_OFFSET: u64 = 0x0001;
const ICX_IRP_COUNTER_WIDTH: u32 = 48;
const ICX_IRP_CONTROL_OFFSET: u64 = 0x0003;
const ICX_IRP_UNIT_CONTROL_OFFSETS: [u64; 6] = [0x0a4a, 0x0a6a, 0x0a8a, 0x0ada, 0x0afa, 0x0b1a];
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_RESERVED_BITS: u32 = 0b11 << 16;

const UNIT_FREEZE: u32 = UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
const UNIT_FREEZE_AND_RESET: u32 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
const UNIT_UNFREEZE: u32 = UNIT_RESERVED_BITS;

const ICX_IRP_STACKS: [IcxIrpStack; 6] = [
    IcxIrpStack::new(0, "pcie0"),
    IcxIrpStack::new(1, "pcie1"),
    IcxIrpStack::new(2, "mcp"),
    IcxIrpStack::new(3, "pcie2"),
    IcxIrpStack::new(4, "pcie3"),
    IcxIrpStack::new(5, "cbdma_dmi"),
];

const ICX_IRP_EVENT_GROUPS: [IcxIrpEventGroup; 7] = [
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::PcieReadCurrent, 0x10, 0x01),
            IcxIrpEventSpec::sum(IcxIrpEventKind::ReadForOwnership, 0x10, 0x08),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::PciItoM, 0x10, 0x10),
            IcxIrpEventSpec::sum(IcxIrpEventKind::WbMtoI, 0x10, 0x40),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::ClFlush, 0x10, 0x80),
            IcxIrpEventSpec::sum(IcxIrpEventKind::TotalIrpOccupancy, 0x0f, 0x04),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::FafOccupancy, 0x19, 0x00),
            IcxIrpEventSpec::sum(IcxIrpEventKind::WriteInserts, 0x11, 0x08),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::LostFwd, 0x1f, 0x10),
            IcxIrpEventSpec::sum(IcxIrpEventKind::FafInserts, 0x18, 0x00),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::AllHitM, 0x12, 0x78),
            IcxIrpEventSpec::sum(IcxIrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
    IcxIrpEventGroup {
        events: [
            IcxIrpEventSpec::sum(IcxIrpEventKind::FafFull, 0x17, 0x00),
            IcxIrpEventSpec::sum(IcxIrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
];

const ICX_IRP_SPEC: IcxIrpSpec = IcxIrpSpec {
    counter_offset: ICX_IRP_COUNTER_OFFSET,
    control_offset: ICX_IRP_CONTROL_OFFSET,
    event_groups: &ICX_IRP_EVENT_GROUPS,
    name: "Ice Lake",
    stacks: &ICX_IRP_STACKS,
    unit_control_offsets: &ICX_IRP_UNIT_CONTROL_OFFSETS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IcxIrpEventKind {
    AllHitM,
    ClFlush,
    Clockticks,
    FafFull,
    FafInserts,
    FafOccupancy,
    LostFwd,
    PciItoM,
    PcieReadCurrent,
    ReadForOwnership,
    TotalIrpOccupancy,
    WbMtoI,
    WriteInserts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct IcxUncoreScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl IcxUncoreScope {
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

#[derive(Clone, Copy, Debug)]
struct IcxUncoreLeader {
    cpu: u32,
    scope: IcxUncoreScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcxIrpEventSpec {
    pub event: u8,
    pub kind: IcxIrpEventKind,
    pub umask: u8,
}

impl IcxIrpEventSpec {
    pub const fn sum(kind: IcxIrpEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcxIrpEventGroup {
    pub events: [IcxIrpEventSpec; ICX_IRP_COUNTER_COUNT],
}

impl IcxIrpEventGroup {
    fn events(self) -> [IcxIrpEventSpec; ICX_IRP_COUNTER_COUNT] {
        self.events
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IcxIrpStack {
    id: usize,
    label: &'static str,
}

impl IcxIrpStack {
    pub const fn new(id: usize, label: &'static str) -> Self {
        Self { id, label }
    }

    pub const fn id(self) -> usize {
        self.id
    }

    pub const fn label(self) -> &'static str {
        self.label
    }
}

impl serde::Serialize for IcxIrpStack {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IcxIrpSpec {
    pub counter_offset: u64,
    pub control_offset: u64,
    pub event_groups: &'static [IcxIrpEventGroup],
    pub name: &'static str,
    pub stacks: &'static [IcxIrpStack],
    pub unit_control_offsets: &'static [u64],
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IcxIrpScopeMetrics {
    #[serde(flatten)]
    pub scope: IcxUncoreScope,
    pub all_hit_m_snoop_responses_per_second: f64,
    pub clflush_bytes_per_second: f64,
    pub faf_full_ratio: f64,
    pub faf_occupancy_entries: f64,
    pub pcie_inbound_reads_per_second: f64,
    pub frequency_hz: f64,
    pub io_write_conflict_ratio: f64,
    pub pci_itom_bytes_per_second: f64,
    pub pcie_read_current_bytes_per_second: f64,
    pub read_for_ownership_bytes_per_second: f64,
    pub stack: IcxIrpStack,
    pub total_irp_occupancy_entries: f64,
    pub wbmtoi_bytes_per_second: f64,
    pub pcie_inbound_writes_per_second: f64,
    pub pcie_inbound_write_latency_seconds: f64,
}

impl IcxIrpScopeMetrics {
    fn from_measurements(
        stack_scope: IcxIrpStackScope,
        measurements: &BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>,
    ) -> Result<Self, String> {
        let clockticks = required_measurement(measurements, IcxIrpEventKind::Clockticks)?;
        let lost_fwd = optional_measurement(measurements, IcxIrpEventKind::LostFwd);
        let total_irp_occupancy =
            required_measurement(measurements, IcxIrpEventKind::TotalIrpOccupancy)?;
        let write_inserts = required_measurement(measurements, IcxIrpEventKind::WriteInserts)?;
        let faf_occupancy = optional_measurement(measurements, IcxIrpEventKind::FafOccupancy);
        let lost_fwd_count = scale_optional_to_enabled(lost_fwd);
        let write_insert_count = scale_measurement_to_enabled(write_inserts);

        Ok(Self {
            scope: stack_scope.scope,
            all_hit_m_snoop_responses_per_second: optional_event_rate(optional_measurement(
                measurements,
                IcxIrpEventKind::AllHitM,
            )),
            clflush_bytes_per_second: optional_bytes_per_second(optional_measurement(
                measurements,
                IcxIrpEventKind::ClFlush,
            )),
            faf_full_ratio: optional_ratio_to_clockticks(
                optional_measurement(measurements, IcxIrpEventKind::FafFull),
                clockticks,
            ),
            faf_occupancy_entries: optional_occupancy_entries(faf_occupancy, clockticks),
            pcie_inbound_reads_per_second: optional_event_rate(optional_measurement(
                measurements,
                IcxIrpEventKind::FafInserts,
            )),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            io_write_conflict_ratio: ratio(lost_fwd_count, write_insert_count),
            pci_itom_bytes_per_second: optional_bytes_per_second(optional_measurement(
                measurements,
                IcxIrpEventKind::PciItoM,
            )),
            pcie_read_current_bytes_per_second: optional_bytes_per_second(optional_measurement(
                measurements,
                IcxIrpEventKind::PcieReadCurrent,
            )),
            read_for_ownership_bytes_per_second: optional_bytes_per_second(optional_measurement(
                measurements,
                IcxIrpEventKind::ReadForOwnership,
            )),
            stack: stack_scope.stack,
            total_irp_occupancy_entries: occupancy_entries(total_irp_occupancy, clockticks),
            wbmtoi_bytes_per_second: optional_bytes_per_second(optional_measurement(
                measurements,
                IcxIrpEventKind::WbMtoI,
            )),
            pcie_inbound_writes_per_second: event_rate(write_inserts),
            pcie_inbound_write_latency_seconds: pcie_inbound_write_latency_seconds(
                total_irp_occupancy,
                faf_occupancy,
                write_inserts,
                clockticks,
            ),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct IcxIrpMetrics {
    pub scopes: Vec<IcxIrpScopeMetrics>,
}

impl IcxIrpMetrics {
    fn from_measurements(
        measurements: BTreeMap<IcxIrpStackScope, BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(IcxIrpScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct IcxIrpCollector {
    next_group: usize,
    packages: Vec<IcxIrpPackage>,
    spec: IcxIrpSpec,
}

impl IcxIrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        if !matches!(model, IntelServerCpuModel::IceLakeXeon) {
            return Err(format!(
                "Ice Lake IRP collection is not supported for {model:?}"
            ));
        }

        let packages = discover_packages(ICX_IRP_SPEC)?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
            spec: ICX_IRP_SPEC,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxIrpMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IRP measure interval must be non-zero",
                self.spec.name
            ));
        }

        let mut measurements = IcxIrpMeasurementAccumulator::new();
        let packages = &self.packages;

        for slice in self.schedule(interval) {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                IcxIrpMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        IcxIrpMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.spec.event_groups.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IcxIrpMeasurementSlice> {
        let group_count = self.spec.event_groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IcxIrpMeasurementSlice {
                    duration: slice_duration,
                    group: self.spec.event_groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxIrpMeasurementSlice {
    duration: Duration,
    group: IcxIrpEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct IcxIrpScopeLabels {
    die: String,
    die_group: String,
    package: String,
    stack: String,
}

impl IcxIrpScopeLabels {
    fn from_scope(scope: IcxUncoreScope, stack: IcxIrpStack) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct IcxIrpPrometheusMetrics {
    all_hit_m_snoop_responses_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    clflush_bytes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    faf_full_ratio: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    faf_occupancy_entries: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_reads_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    io_write_conflict_ratio: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pci_itom_bytes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_current_bytes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    read_for_ownership_bytes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    total_irp_occupancy_entries: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    wbmtoi_bytes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_writes_per_second: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_write_latency_seconds: Family<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>,
}

impl IcxIrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            all_hit_m_snoop_responses_per_second:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            clflush_bytes_per_second: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            faf_full_ratio: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            faf_occupancy_entries: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_reads_per_second:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            io_write_conflict_ratio: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pci_itom_bytes_per_second: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            pcie_read_current_bytes_per_second:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_for_ownership_bytes_per_second:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            total_irp_occupancy_entries:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wbmtoi_bytes_per_second: Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_writes_per_second:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_write_latency_seconds:
                Family::<IcxIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_irp_all_hit_m_snoop_responses_per_second",
            "Interval-derived IRP snoop responses that hit modified lines per second",
            metrics.all_hit_m_snoop_responses_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_clflush_bytes_per_second",
            "Interval-derived IRP CLFlush bandwidth in bytes per second",
            metrics.clflush_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_faf_full_ratio",
            "Interval-derived fraction of IRP fire-and-forget queue full cycles",
            metrics.faf_full_ratio.clone(),
        );
        registry.register(
            "ocellus_irp_faf_occupancy_entries",
            "Average IRP fire-and-forget queue occupancy in entries",
            metrics.faf_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_irp_pcie_inbound_reads_per_second",
            "Interval-derived IRP PCIe inbound reads per second",
            metrics.pcie_inbound_reads_per_second.clone(),
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
            "ocellus_irp_pcie_inbound_writes_per_second",
            "Interval-derived IRP PCIe inbound writes per second",
            metrics.pcie_inbound_writes_per_second.clone(),
        );
        registry.register(
            "ocellus_irp_pcie_inbound_write_latency_seconds",
            "Interval-derived IRP inbound write residency latency in seconds",
            metrics.pcie_inbound_write_latency_seconds.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: IcxIrpMetrics) {
        for scope in metrics.scopes {
            let labels = IcxIrpScopeLabels::from_scope(scope.scope, scope.stack);

            self.all_hit_m_snoop_responses_per_second
                .get_or_create(&labels)
                .set(scope.all_hit_m_snoop_responses_per_second);
            self.clflush_bytes_per_second
                .get_or_create(&labels)
                .set(scope.clflush_bytes_per_second);
            self.faf_full_ratio
                .get_or_create(&labels)
                .set(scope.faf_full_ratio);
            self.faf_occupancy_entries
                .get_or_create(&labels)
                .set(scope.faf_occupancy_entries);
            self.pcie_inbound_reads_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_reads_per_second);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.io_write_conflict_ratio
                .get_or_create(&labels)
                .set(scope.io_write_conflict_ratio);
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
            self.pcie_inbound_writes_per_second
                .get_or_create(&labels)
                .set(scope.pcie_inbound_writes_per_second);
            self.pcie_inbound_write_latency_seconds
                .get_or_create(&labels)
                .set(scope.pcie_inbound_write_latency_seconds);
        }
    }
}

#[derive(Debug)]
struct IcxIrpPackage {
    scope: IcxUncoreScope,
    units: Vec<IcxIrpUnit>,
}

impl IcxIrpPackage {
    fn new(cpu: u32, scope: IcxUncoreScope, spec: IcxIrpSpec) -> Self {
        let units = spec
            .stacks
            .iter()
            .copied()
            .map(|stack| IcxIrpUnit { cpu, spec, stack })
            .collect();

        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxIrpUnit {
    cpu: u32,
    spec: IcxIrpSpec,
    stack: IcxIrpStack,
}

impl IcxIrpUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))
    }

    fn program(self, group: IcxIrpEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events().into_iter().enumerate() {
            msr.write(
                irp_control_offset(self.spec, self.stack, counter_index),
                u64::from(counter_control(event.event, event.umask, true)),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<IcxIrpUnitReading, String> {
        Ok(IcxIrpUnitReading {
            counters: [
                self.read_counter(0).map(mask_icx_irp_counter)?,
                self.read_counter(1).map(mask_icx_irp_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_UNFREEZE))
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(irp_counter_offset(self.spec, self.stack, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(irp_unit_control_offset(self.spec, self.stack), value)
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxIrpUnitReading {
    counters: [u64; ICX_IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct IcxIrpEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl IcxIrpEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxIrpMeasurement {
    enabled: Duration,
    group: IcxIrpEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IcxIrpStackScope {
    scope: IcxUncoreScope,
    stack: IcxIrpStack,
}

#[derive(Debug, Default)]
struct IcxIrpMeasurementAccumulator {
    measurements: BTreeMap<IcxIrpStackScope, BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>>,
}

impl IcxIrpMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: IcxUncoreScope,
        stack: IcxIrpStack,
        kind: IcxIrpEventKind,
        value: u64,
        measurement: IcxIrpMeasurement,
    ) {
        self.measurements
            .entry(IcxIrpStackScope { scope, stack })
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(IcxIrpEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<IcxIrpStackScope, BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>> {
        self.measurements
    }
}

fn bytes_per_second(measurement: &IcxIrpEventMeasurement) -> f64 {
    event_rate(measurement) * BYTES_PER_CACHE_LINE
}

fn counter_control(event: u8, umask: u8, overflow_enabled: bool) -> u32 {
    let overflow = if overflow_enabled {
        COUNTER_OVERFLOW_ENABLE_BIT
    } else {
        0
    };

    u32::from(event) | (u32::from(umask) << 8) | COUNTER_RESET_BIT | overflow | COUNTER_ENABLE_BIT
}

fn discover_packages(spec: IcxIrpSpec) -> Result<Vec<IcxIrpPackage>, String> {
    let leaders = uncore_leaders()?;
    let packages = leaders
        .into_iter()
        .map(|leader| IcxIrpPackage::new(leader.cpu, leader.scope, spec))
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err(format!("failed to discover any {} IRP packages", spec.name));
    }

    Ok(packages)
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn event_rate(measurement: &IcxIrpEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn freeze_packages(packages: &[IcxIrpPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn frequency_hz(ticks: u64, duration: Duration) -> f64 {
    events_per_second(ticks, duration)
}

pub fn irp_control_offset(spec: IcxIrpSpec, stack: IcxIrpStack, counter_index: usize) -> u64 {
    irp_unit_control_offset(spec, stack) + spec.control_offset + counter_index as u64
}

pub fn irp_counter_offset(spec: IcxIrpSpec, stack: IcxIrpStack, counter_index: usize) -> u64 {
    irp_unit_control_offset(spec, stack) + spec.counter_offset + counter_index as u64
}

pub fn irp_unit_control_offset(spec: IcxIrpSpec, stack: IcxIrpStack) -> u64 {
    spec.unit_control_offsets[stack.id()]
}

fn mask_icx_irp_counter(counter: u64) -> u64 {
    mask_counter(counter, ICX_IRP_COUNTER_WIDTH)
}

fn mask_counter(counter: u64, width: u32) -> u64 {
    counter & ((1_u64 << width) - 1)
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn occupancy_entries(
    occupancy: &IcxIrpEventMeasurement,
    clockticks: &IcxIrpEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    ratio(occupancy, clockticks)
}

fn optional_bytes_per_second(measurement: Option<&IcxIrpEventMeasurement>) -> f64 {
    measurement.map(bytes_per_second).unwrap_or(0.0)
}

fn optional_event_rate(measurement: Option<&IcxIrpEventMeasurement>) -> f64 {
    measurement.map(event_rate).unwrap_or(0.0)
}

fn optional_measurement(
    measurements: &BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>,
    kind: IcxIrpEventKind,
) -> Option<&IcxIrpEventMeasurement> {
    measurements.get(&kind)
}

fn optional_occupancy_entries(
    occupancy: Option<&IcxIrpEventMeasurement>,
    clockticks: &IcxIrpEventMeasurement,
) -> f64 {
    occupancy
        .map(|occupancy| occupancy_entries(occupancy, clockticks))
        .unwrap_or(0.0)
}

fn optional_ratio_to_clockticks(
    cycles: Option<&IcxIrpEventMeasurement>,
    clockticks: &IcxIrpEventMeasurement,
) -> f64 {
    ratio(
        cycles.map(scale_measurement_to_enabled).unwrap_or(0),
        scale_measurement_to_enabled(clockticks),
    )
}

fn queue_residency_seconds(occupancy: u64, inserts: u64, ticks: u64, duration: Duration) -> f64 {
    if inserts == 0 || ticks == 0 {
        return 0.0;
    }

    let seconds_per_tick = duration.as_secs_f64() / ticks as f64;
    seconds_per_tick * occupancy as f64 / inserts as f64
}

fn pcie_inbound_write_latency_seconds(
    total_occupancy: &IcxIrpEventMeasurement,
    faf_occupancy: Option<&IcxIrpEventMeasurement>,
    inserts: &IcxIrpEventMeasurement,
    clockticks: &IcxIrpEventMeasurement,
) -> f64 {
    let total_occupancy = scale_measurement_to_enabled(total_occupancy);
    let faf_occupancy = faf_occupancy.map(scale_measurement_to_enabled).unwrap_or(0);
    let write_occupancy = total_occupancy.saturating_sub(faf_occupancy);
    let insert_count = scale_measurement_to_enabled(inserts);
    let clockticks = scale_measurement_to_enabled(clockticks);

    queue_residency_seconds(write_occupancy, insert_count, clockticks, inserts.enabled)
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn probe_writable_msrs(packages: &[IcxIrpPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn program_packages(packages: &[IcxIrpPackage], group: IcxIrpEventGroup) -> Result<(), String> {
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
    packages: &[IcxIrpPackage],
    measurement: IcxIrpMeasurement,
    measurements: &mut IcxIrpMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            let reading = unit.read()?;

            for counter_index in 0..ICX_IRP_COUNTER_COUNT {
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
    measurements: &BTreeMap<IcxIrpEventKind, IcxIrpEventMeasurement>,
    kind: IcxIrpEventKind,
) -> Result<&IcxIrpEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IRP measurement {kind:?} is missing"))
}

fn scale_measurement_to_enabled(measurement: &IcxIrpEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

fn scale_optional_to_enabled(measurement: Option<&IcxIrpEventMeasurement>) -> u64 {
    measurement.map(scale_measurement_to_enabled).unwrap_or(0)
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

fn unfreeze_packages(packages: &[IcxIrpPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn uncore_leaders() -> Result<Vec<IcxUncoreLeader>, String> {
    let mut leaders = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        leaders
            .entry(IcxUncoreScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err("failed to discover any Ice Lake uncore scope leaders".to_string());
    }

    Ok(leaders
        .into_iter()
        .map(|(scope, cpu)| IcxUncoreLeader { cpu, scope })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_icx_irp_stack_address_map() {
        assert_eq!(
            irp_unit_control_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[0]),
            0x0a4a
        );
        assert_eq!(
            irp_counter_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[0], 0),
            0x0a4b
        );
        assert_eq!(
            irp_control_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[0], 0),
            0x0a4d
        );
        assert_eq!(
            irp_unit_control_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[5]),
            0x0b1a
        );
        assert_eq!(
            irp_counter_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[5], 1),
            0x0b1c
        );
        assert_eq!(
            irp_control_offset(ICX_IRP_SPEC, ICX_IRP_STACKS[5], 1),
            0x0b1e
        );
    }

    #[test]
    fn uses_icx_irp_stack_labels() {
        assert_eq!(ICX_IRP_STACKS[0].label(), "pcie0");
        assert_eq!(ICX_IRP_STACKS[1].label(), "pcie1");
        assert_eq!(ICX_IRP_STACKS[2].label(), "mcp");
        assert_eq!(ICX_IRP_STACKS[3].label(), "pcie2");
        assert_eq!(ICX_IRP_STACKS[4].label(), "pcie3");
        assert_eq!(ICX_IRP_STACKS[5].label(), "cbdma_dmi");
    }

    #[test]
    fn uses_icx_irp_event_encodings() {
        assert_event(IcxIrpEventKind::PcieReadCurrent, 0x10, 0x01);
        assert_event(IcxIrpEventKind::ReadForOwnership, 0x10, 0x08);
        assert_event(IcxIrpEventKind::PciItoM, 0x10, 0x10);
        assert_event(IcxIrpEventKind::WbMtoI, 0x10, 0x40);
        assert_event(IcxIrpEventKind::ClFlush, 0x10, 0x80);
        assert_event(IcxIrpEventKind::TotalIrpOccupancy, 0x0f, 0x04);
        assert_event(IcxIrpEventKind::WriteInserts, 0x11, 0x08);
        assert_event(IcxIrpEventKind::LostFwd, 0x1f, 0x10);
        assert_event(IcxIrpEventKind::FafInserts, 0x18, 0x00);
        assert_event(IcxIrpEventKind::Clockticks, 0x01, 0x00);
        assert_event(IcxIrpEventKind::AllHitM, 0x12, 0x78);
        assert_event(IcxIrpEventKind::FafFull, 0x17, 0x00);
    }

    fn assert_event(kind: IcxIrpEventKind, event: u8, umask: u8) {
        let event_spec = ICX_IRP_EVENT_GROUPS
            .iter()
            .flat_map(|group| group.events)
            .find(|event_spec| event_spec.kind == kind)
            .unwrap();

        assert_eq!(event_spec.event, event);
        assert_eq!(event_spec.umask, umask);
    }
}
