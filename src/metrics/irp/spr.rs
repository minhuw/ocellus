use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metal;
use crate::metal::msr::Msr;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};

const DEFAULT_MAX_SLICE: Duration = Duration::from_millis(100);
const SPR_IRP_COUNTER_COUNT: usize = 2;
const SPR_IRP_COUNTER_OFFSET: u64 = 0x0008;
const SPR_IRP_COUNTER_WIDTH: u32 = 48;
const SPR_IRP_CONTROL_OFFSET: u64 = 0x0002;
const SPR_IRP_UNIT_CONTROL_OFFSETS: [u64; 12] = [
    0x3400, 0x3410, 0x3420, 0x3430, 0x3440, 0x3450, 0x3460, 0x3470, 0x3480, 0x3490, 0x34a0, 0x34b0,
];
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 9;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 8;
const UNIT_FREEZE_BIT: u32 = 1 << 0;

const UNIT_FREEZE: u32 = UNIT_FREEZE_BIT;
const UNIT_FREEZE_AND_COUNTER_RESET: u32 = UNIT_FREEZE_BIT | UNIT_COUNTER_RESET_BIT;
const UNIT_FREEZE_AND_CONTROL_RESET: u32 = UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT;
const UNIT_UNFREEZE: u32 = 0;

const SPR_IRP_STACKS: [SprIrpStack; 12] = [
    SprIrpStack::new(0, "m2iosf0"),
    SprIrpStack::new(1, "m2iosf1"),
    SprIrpStack::new(2, "m2iosf2"),
    SprIrpStack::new(3, "m2iosf3"),
    SprIrpStack::new(4, "m2iosf4"),
    SprIrpStack::new(5, "m2iosf5"),
    SprIrpStack::new(6, "m2iosf6"),
    SprIrpStack::new(7, "m2iosf7"),
    SprIrpStack::new(8, "m2iosf8"),
    SprIrpStack::new(9, "m2iosf9"),
    SprIrpStack::new(10, "m2iosf10"),
    SprIrpStack::new(11, "m2iosf11"),
];

const SPR_IRP_EVENT_GROUPS: [SprIrpEventGroup; 5] = [
    SprIrpEventGroup {
        events: [
            SprIrpEventSpec::sum(SprIrpEventKind::TotalIrpOccupancy, 0x0f, 0x04),
            SprIrpEventSpec::sum(SprIrpEventKind::FafOccupancy, 0x19, 0x00),
        ],
    },
    SprIrpEventGroup {
        events: [
            SprIrpEventSpec::sum(SprIrpEventKind::WriteInserts, 0x11, 0x08),
            SprIrpEventSpec::sum(SprIrpEventKind::LostFwd, 0x1f, 0x10),
        ],
    },
    SprIrpEventGroup {
        events: [
            SprIrpEventSpec::sum(SprIrpEventKind::FafInserts, 0x18, 0x00),
            SprIrpEventSpec::sum(SprIrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
    SprIrpEventGroup {
        events: [
            SprIrpEventSpec::sum(SprIrpEventKind::AllHitM, 0x12, 0x78),
            SprIrpEventSpec::sum(SprIrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
    SprIrpEventGroup {
        events: [
            SprIrpEventSpec::sum(SprIrpEventKind::FafFull, 0x17, 0x00),
            SprIrpEventSpec::sum(SprIrpEventKind::Clockticks, 0x01, 0x00),
        ],
    },
];

const SPR_EMR_IRP_SPEC: SprIrpSpec = SprIrpSpec {
    counter_offset: SPR_IRP_COUNTER_OFFSET,
    control_offset: SPR_IRP_CONTROL_OFFSET,
    event_groups: &SPR_IRP_EVENT_GROUPS,
    name: "SPR/EMR",
    stacks: &SPR_IRP_STACKS,
    unit_control_offsets: &SPR_IRP_UNIT_CONTROL_OFFSETS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SprIrpEventKind {
    AllHitM,
    Clockticks,
    FafFull,
    FafInserts,
    FafOccupancy,
    LostFwd,
    TotalIrpOccupancy,
    WriteInserts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SprUncoreScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl SprUncoreScope {
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
struct SprUncoreLeader {
    cpu: u32,
    scope: SprUncoreScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SprIrpEventSpec {
    pub event: u8,
    pub kind: SprIrpEventKind,
    pub umask: u8,
}

impl SprIrpEventSpec {
    pub const fn sum(kind: SprIrpEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SprIrpEventGroup {
    pub events: [SprIrpEventSpec; SPR_IRP_COUNTER_COUNT],
}

impl SprIrpEventGroup {
    fn events(self) -> [SprIrpEventSpec; SPR_IRP_COUNTER_COUNT] {
        self.events
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SprIrpStack {
    id: usize,
    label: &'static str,
}

impl SprIrpStack {
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

impl serde::Serialize for SprIrpStack {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SprIrpSpec {
    pub counter_offset: u64,
    pub control_offset: u64,
    pub event_groups: &'static [SprIrpEventGroup],
    pub name: &'static str,
    pub stacks: &'static [SprIrpStack],
    pub unit_control_offsets: &'static [u64],
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SprIrpScopeMetrics {
    #[serde(flatten)]
    pub scope: SprUncoreScope,
    pub all_hit_m_snoop_responses_per_second: f64,
    pub faf_full_ratio: f64,
    pub faf_occupancy_entries: f64,
    pub pcie_inbound_reads_per_second: f64,
    pub frequency_hz: f64,
    pub io_write_conflict_ratio: f64,
    pub stack: SprIrpStack,
    pub total_irp_occupancy_entries: f64,
    pub pcie_inbound_writes_per_second: f64,
    pub pcie_inbound_write_latency_seconds: f64,
}

impl SprIrpScopeMetrics {
    fn from_measurements(
        stack_scope: SprIrpStackScope,
        measurements: &BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>,
    ) -> Result<Self, String> {
        let clockticks = required_measurement(measurements, SprIrpEventKind::Clockticks)?;
        let lost_fwd = optional_measurement(measurements, SprIrpEventKind::LostFwd);
        let total_irp_occupancy =
            required_measurement(measurements, SprIrpEventKind::TotalIrpOccupancy)?;
        let write_inserts = required_measurement(measurements, SprIrpEventKind::WriteInserts)?;
        let faf_occupancy = optional_measurement(measurements, SprIrpEventKind::FafOccupancy);
        let lost_fwd_count = scale_optional_to_enabled(lost_fwd);
        let write_insert_count = scale_measurement_to_enabled(write_inserts);

        Ok(Self {
            scope: stack_scope.scope,
            all_hit_m_snoop_responses_per_second: optional_event_rate(optional_measurement(
                measurements,
                SprIrpEventKind::AllHitM,
            )),
            faf_full_ratio: optional_ratio_to_clockticks(
                optional_measurement(measurements, SprIrpEventKind::FafFull),
                clockticks,
            ),
            faf_occupancy_entries: optional_occupancy_entries(faf_occupancy, clockticks),
            pcie_inbound_reads_per_second: optional_event_rate(optional_measurement(
                measurements,
                SprIrpEventKind::FafInserts,
            )),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            io_write_conflict_ratio: ratio(lost_fwd_count, write_insert_count),
            stack: stack_scope.stack,
            total_irp_occupancy_entries: occupancy_entries(total_irp_occupancy, clockticks),
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
pub struct SprIrpMetrics {
    pub scopes: Vec<SprIrpScopeMetrics>,
}

impl SprIrpMetrics {
    fn from_measurements(
        measurements: BTreeMap<SprIrpStackScope, BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(SprIrpScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SprIrpCollector {
    next_group: usize,
    packages: Vec<SprIrpPackage>,
    spec: SprIrpSpec,
}

impl SprIrpCollector {
    pub fn new() -> Result<Self, String> {
        let packages = discover_packages(SPR_EMR_IRP_SPEC)?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
            spec: SPR_EMR_IRP_SPEC,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SprIrpMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} IRP measure interval must be non-zero",
                self.spec.name
            ));
        }

        let mut measurements = SprIrpMeasurementAccumulator::new();
        let packages = &self.packages;

        for slice in self.schedule(interval) {
            program_packages(packages, slice.group)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                SprIrpMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        SprIrpMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.spec.event_groups.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<SprIrpMeasurementSlice> {
        let group_count = self.spec.event_groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(SprIrpMeasurementSlice {
                    duration: slice_duration,
                    group: self.spec.event_groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprIrpMeasurementSlice {
    duration: Duration,
    group: SprIrpEventGroup,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SprIrpScopeLabels {
    die: String,
    die_group: String,
    package: String,
    stack: String,
}

impl SprIrpScopeLabels {
    fn from_scope(scope: SprUncoreScope, stack: SprIrpStack) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct SprIrpPrometheusMetrics {
    all_hit_m_snoop_responses_per_second: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    faf_full_ratio: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    faf_occupancy_entries: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_reads_per_second: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    io_write_conflict_ratio: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    total_irp_occupancy_entries: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_writes_per_second: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_inbound_write_latency_seconds: Family<SprIrpScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SprIrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            all_hit_m_snoop_responses_per_second:
                Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            faf_full_ratio: Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            faf_occupancy_entries: Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_reads_per_second:
                Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            io_write_conflict_ratio: Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            total_irp_occupancy_entries:
                Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_writes_per_second:
                Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_inbound_write_latency_seconds:
                Family::<SprIrpScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_irp_all_hit_m_snoop_responses_per_second",
            "Interval-derived IRP snoop responses that hit modified lines per second",
            metrics.all_hit_m_snoop_responses_per_second.clone(),
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
            "ocellus_irp_total_occupancy_entries",
            "Average total IRP read and write occupancy in entries",
            metrics.total_irp_occupancy_entries.clone(),
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

    pub fn update(&self, metrics: SprIrpMetrics) {
        for scope in metrics.scopes {
            let labels = SprIrpScopeLabels::from_scope(scope.scope, scope.stack);

            self.all_hit_m_snoop_responses_per_second
                .get_or_create(&labels)
                .set(scope.all_hit_m_snoop_responses_per_second);
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
            self.total_irp_occupancy_entries
                .get_or_create(&labels)
                .set(scope.total_irp_occupancy_entries);
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
struct SprIrpPackage {
    scope: SprUncoreScope,
    units: Vec<SprIrpUnit>,
}

impl SprIrpPackage {
    fn new(cpu: u32, scope: SprUncoreScope, spec: SprIrpSpec) -> Self {
        let units = spec
            .stacks
            .iter()
            .copied()
            .map(|stack| SprIrpUnit { cpu, spec, stack })
            .collect();

        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct SprIrpUnit {
    cpu: u32,
    spec: SprIrpSpec,
    stack: SprIrpStack,
}

impl SprIrpUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))?;
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_CONTROL_RESET))?;
        self.write_unit_control(u64::from(UNIT_FREEZE_AND_COUNTER_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(UNIT_FREEZE))
    }

    fn program(self, group: SprIrpEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events().into_iter().enumerate() {
            msr.write(
                irp_control_offset(self.spec, self.stack, counter_index),
                u64::from(counter_control(event.event, event.umask)),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<SprIrpUnitReading, String> {
        Ok(SprIrpUnitReading {
            counters: [
                self.read_counter(0).map(mask_spr_irp_counter)?,
                self.read_counter(1).map(mask_spr_irp_counter)?,
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
struct SprIrpUnitReading {
    counters: [u64; SPR_IRP_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct SprIrpEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl SprIrpEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct SprIrpMeasurement {
    enabled: Duration,
    group: SprIrpEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SprIrpStackScope {
    scope: SprUncoreScope,
    stack: SprIrpStack,
}

#[derive(Debug, Default)]
struct SprIrpMeasurementAccumulator {
    measurements: BTreeMap<SprIrpStackScope, BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>>,
}

impl SprIrpMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: SprUncoreScope,
        stack: SprIrpStack,
        kind: SprIrpEventKind,
        value: u64,
        measurement: SprIrpMeasurement,
    ) {
        self.measurements
            .entry(SprIrpStackScope { scope, stack })
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(SprIrpEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<SprIrpStackScope, BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>> {
        self.measurements
    }
}

fn counter_control(event: u8, umask: u8) -> u32 {
    u32::from(event) | (u32::from(umask) << 8)
}

fn discover_packages(spec: SprIrpSpec) -> Result<Vec<SprIrpPackage>, String> {
    let leaders = uncore_leaders()?;
    let packages = leaders
        .into_iter()
        .map(|leader| SprIrpPackage::new(leader.cpu, leader.scope, spec))
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

fn event_rate(measurement: &SprIrpEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn freeze_packages(packages: &[SprIrpPackage]) -> Result<(), String> {
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

pub fn irp_control_offset(spec: SprIrpSpec, stack: SprIrpStack, counter_index: usize) -> u64 {
    irp_unit_control_offset(spec, stack) + spec.control_offset + counter_index as u64
}

pub fn irp_counter_offset(spec: SprIrpSpec, stack: SprIrpStack, counter_index: usize) -> u64 {
    irp_unit_control_offset(spec, stack) + spec.counter_offset + counter_index as u64
}

pub fn irp_unit_control_offset(spec: SprIrpSpec, stack: SprIrpStack) -> u64 {
    spec.unit_control_offsets[stack.id()]
}

fn mask_spr_irp_counter(counter: u64) -> u64 {
    mask_counter(counter, SPR_IRP_COUNTER_WIDTH)
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
    occupancy: &SprIrpEventMeasurement,
    clockticks: &SprIrpEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    ratio(occupancy, clockticks)
}

fn optional_event_rate(measurement: Option<&SprIrpEventMeasurement>) -> f64 {
    measurement.map(event_rate).unwrap_or(0.0)
}

fn optional_measurement(
    measurements: &BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>,
    kind: SprIrpEventKind,
) -> Option<&SprIrpEventMeasurement> {
    measurements.get(&kind)
}

fn optional_occupancy_entries(
    occupancy: Option<&SprIrpEventMeasurement>,
    clockticks: &SprIrpEventMeasurement,
) -> f64 {
    occupancy
        .map(|occupancy| occupancy_entries(occupancy, clockticks))
        .unwrap_or(0.0)
}

fn optional_ratio_to_clockticks(
    cycles: Option<&SprIrpEventMeasurement>,
    clockticks: &SprIrpEventMeasurement,
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
    total_occupancy: &SprIrpEventMeasurement,
    faf_occupancy: Option<&SprIrpEventMeasurement>,
    inserts: &SprIrpEventMeasurement,
    clockticks: &SprIrpEventMeasurement,
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

fn probe_writable_msrs(packages: &[SprIrpPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn program_packages(packages: &[SprIrpPackage], group: SprIrpEventGroup) -> Result<(), String> {
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
    packages: &[SprIrpPackage],
    measurement: SprIrpMeasurement,
    measurements: &mut SprIrpMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            let reading = unit.read()?;

            for counter_index in 0..SPR_IRP_COUNTER_COUNT {
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
    measurements: &BTreeMap<SprIrpEventKind, SprIrpEventMeasurement>,
    kind: SprIrpEventKind,
) -> Result<&SprIrpEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IRP measurement {kind:?} is missing"))
}

fn scale_measurement_to_enabled(measurement: &SprIrpEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

fn scale_optional_to_enabled(measurement: Option<&SprIrpEventMeasurement>) -> u64 {
    measurement.map(scale_measurement_to_enabled).unwrap_or(0)
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

fn unfreeze_packages(packages: &[SprIrpPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn uncore_leaders() -> Result<Vec<SprUncoreLeader>, String> {
    let mut leaders = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        leaders
            .entry(SprUncoreScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err("failed to discover any SPR/EMR uncore scope leaders".to_string());
    }

    Ok(leaders
        .into_iter()
        .map(|(scope, cpu)| SprUncoreLeader { cpu, scope })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_spr_irp_stack_address_map() {
        assert_eq!(
            irp_unit_control_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[0]),
            0x3400
        );
        assert_eq!(
            irp_counter_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[0], 0),
            0x3408
        );
        assert_eq!(
            irp_control_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[0], 0),
            0x3402
        );
        assert_eq!(
            irp_unit_control_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[11]),
            0x34b0
        );
        assert_eq!(
            irp_counter_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[11], 1),
            0x34b9
        );
        assert_eq!(
            irp_control_offset(SPR_EMR_IRP_SPEC, SPR_IRP_STACKS[11], 1),
            0x34b3
        );
    }

    #[test]
    fn uses_spr_irp_event_encodings() {
        assert_event(SprIrpEventKind::TotalIrpOccupancy, 0x0f, 0x04);
        assert_event(SprIrpEventKind::FafOccupancy, 0x19, 0x00);
        assert_event(SprIrpEventKind::WriteInserts, 0x11, 0x08);
        assert_event(SprIrpEventKind::LostFwd, 0x1f, 0x10);
        assert_event(SprIrpEventKind::FafInserts, 0x18, 0x00);
        assert_event(SprIrpEventKind::Clockticks, 0x01, 0x00);
        assert_event(SprIrpEventKind::AllHitM, 0x12, 0x78);
        assert_event(SprIrpEventKind::FafFull, 0x17, 0x00);
    }

    #[test]
    fn uses_spr_event_control_bits() {
        assert_eq!(counter_control(0x0f, 0x04), 0x040f);
        assert_eq!(counter_control(0x11, 0x08), 0x0811);
    }

    #[test]
    fn uses_spr_unit_control_bits() {
        assert_eq!(UNIT_FREEZE, 0x001);
        assert_eq!(UNIT_FREEZE_AND_CONTROL_RESET, 0x101);
        assert_eq!(UNIT_FREEZE_AND_COUNTER_RESET, 0x201);
        assert_eq!(UNIT_UNFREEZE, 0x000);
    }

    fn assert_event(kind: SprIrpEventKind, event: u8, umask: u8) {
        let event_spec = SPR_IRP_EVENT_GROUPS
            .iter()
            .flat_map(|group| group.events)
            .find(|event_spec| event_spec.kind == kind)
            .unwrap();

        assert_eq!(event_spec.event, event);
        assert_eq!(event_spec.umask, umask);
    }
}
