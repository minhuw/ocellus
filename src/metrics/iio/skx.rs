use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::arch::skx::pmon;
use crate::metal::msr::Msr;
use crate::metrics::common::topology_label;
use crate::metrics::uncore::skx::{
    SKX_IIO_STACK_COUNT, SKX_UNCORE_COUNTER_WIDTH, SkxIioStack, UncoreScope, events_per_second,
    frequency_hz, mask_counter, measurement_round_count, queue_residency_seconds, ratio,
    scale_to_enabled, uncore_leaders, wrapping_delta,
};
use crate::pmu_config::types::{
    BoxType, CountAccumulator, CounterSlot, Grouping, PmuConfig, ProgrammedBoxType,
    ProgrammedGroup, UncoreCounts,
};

const IIO_COUNTER_COUNT: usize = 4;
const IIO_PCIE_PORT_COUNT: usize = 4;
const IIO_UNIT_COUNT: usize = SKX_IIO_STACK_COUNT;

const IIO_CLOCK_COUNTER_BASE: u64 = 0x0a45;
const IIO_COUNTER_BASE: u64 = 0x0a41;
const IIO_CONTROL_BASE: u64 = 0x0a48;
const IIO_FREE_RUNNING_COUNTER_WIDTH: u32 = 36;
const IIO_PCIE_READ_COUNTER_BASE: u64 = 0x0b00;
const IIO_PCIE_WRITE_COUNTER_BASE: u64 = 0x0b04;
const IIO_PCIE_COUNTER_STRIDE: u64 = 0x10;
const IIO_UNIT_CONTROL_BASE: u64 = 0x0a40;
const IIO_UNIT_STRIDE: u64 = 0x20;
const PCIE_COUNTER_BYTES: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IioEventKind {
    Clockticks,
    CompletionInserts,
    CompletionOccupancy,
    L1Miss,
    L2Miss,
    L3Miss,
    TlbHit,
    TlbMiss,
    Unused,
    VtdAccess,
    VtdClockticks,
    VtdOccupancy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IioEventSpec {
    channel_mask: u8,
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
        channel_mask: u8,
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

const SKX_IIO_EVENT_GROUPS: [IioEventGroup; 3] = [
    IioEventGroup {
        events: [
            IioEventSpec::sum(IioEventKind::TlbMiss, 0x41, 0x20, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::L1Miss, 0x41, 0x04, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::L2Miss, 0x41, 0x08, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::L3Miss, 0x41, 0x10, 0x00, 0x00),
        ],
    },
    IioEventGroup {
        events: [
            IioEventSpec::sum(IioEventKind::TlbHit, 0x41, 0x01, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::VtdAccess, 0x41, 0xff, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::VtdOccupancy, 0x40, 0x00, 0x00, 0x00),
            IioEventSpec::sum(IioEventKind::VtdClockticks, 0x01, 0x00, 0x00, 0x00),
        ],
    },
    IioEventGroup {
        events: [
            IioEventSpec::unused(),
            IioEventSpec::sum(IioEventKind::CompletionInserts, 0xc2, 0x03, 0x0f, 0x04),
            IioEventSpec::sum(IioEventKind::CompletionOccupancy, 0xd5, 0x0f, 0x00, 0x04),
            IioEventSpec::sum(IioEventKind::Clockticks, 0x01, 0x00, 0x00, 0x00),
        ],
    },
];

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IioPciePortMetrics {
    pub port_id: u32,
    pub read_bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub stack: SkxIioStack,
    pub write_bytes_per_second: f64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct IioScopeMetrics {
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub completion_inserts_per_second: f64,
    pub completion_latency_seconds: f64,
    pub completion_occupancy_entries: f64,
    pub frequency_hz: f64,
    pub l1_misses_per_second: f64,
    pub l2_misses_per_second: f64,
    pub l3_misses_per_second: f64,
    pub stack: SkxIioStack,
    pub tlb_hits_per_second: f64,
    pub tlb_misses_per_second: f64,
    pub vtd_accesses_per_second: f64,
    pub vtd_latency_seconds: f64,
    pub vtd_occupancy_entries: f64,
}

impl IioScopeMetrics {
    fn from_measurements(
        stack_scope: IioStackScope,
        measurements: &BTreeMap<IioEventKind, IioEventMeasurement>,
    ) -> Result<Self, String> {
        let clockticks = required_measurement(measurements, IioEventKind::Clockticks)?;
        let completion_inserts =
            required_measurement(measurements, IioEventKind::CompletionInserts)?;
        let completion_occupancy =
            required_measurement(measurements, IioEventKind::CompletionOccupancy)?;
        let l1_miss = required_measurement(measurements, IioEventKind::L1Miss)?;
        let l2_miss = required_measurement(measurements, IioEventKind::L2Miss)?;
        let l3_miss = required_measurement(measurements, IioEventKind::L3Miss)?;
        let tlb_hit = required_measurement(measurements, IioEventKind::TlbHit)?;
        let tlb_miss = required_measurement(measurements, IioEventKind::TlbMiss)?;
        let vtd_access = required_measurement(measurements, IioEventKind::VtdAccess)?;
        let vtd_clockticks = required_measurement(measurements, IioEventKind::VtdClockticks)?;
        let vtd_occupancy = required_measurement(measurements, IioEventKind::VtdOccupancy)?;

        Ok(Self {
            scope: stack_scope.scope,
            completion_inserts_per_second: event_rate(completion_inserts),
            completion_latency_seconds: completion_latency_seconds(
                completion_occupancy,
                completion_inserts,
                clockticks,
            ),
            completion_occupancy_entries: ratio(completion_occupancy.value, clockticks.value),
            frequency_hz: frequency_hz(clockticks.value, clockticks.running),
            l1_misses_per_second: event_rate(l1_miss),
            l2_misses_per_second: event_rate(l2_miss),
            l3_misses_per_second: event_rate(l3_miss),
            stack: stack_scope.stack,
            tlb_hits_per_second: event_rate(tlb_hit),
            tlb_misses_per_second: event_rate(tlb_miss),
            vtd_accesses_per_second: event_rate(vtd_access),
            vtd_latency_seconds: vtd_latency_seconds(vtd_occupancy, vtd_access, vtd_clockticks),
            vtd_occupancy_entries: ratio(
                scale_to_enabled(
                    vtd_occupancy.value,
                    vtd_occupancy.enabled,
                    vtd_occupancy.running,
                ),
                scale_to_enabled(
                    vtd_clockticks.value,
                    vtd_clockticks.enabled,
                    vtd_clockticks.running,
                ),
            ),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SkxIioMetrics {
    pub ports: Vec<IioPciePortMetrics>,
    pub scopes: Vec<IioScopeMetrics>,
}

impl SkxIioMetrics {
    fn from_measurements(
        measurements: BTreeMap<IioStackScope, BTreeMap<IioEventKind, IioEventMeasurement>>,
        ports: Vec<IioPciePortMetrics>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (stack_scope, scope_measurements) in measurements {
            scopes.push(IioScopeMetrics::from_measurements(
                stack_scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { ports, scopes })
    }
}

#[derive(Debug)]
pub struct SkxIioCollector {
    config: Option<CountAccumulator>,
    next_group: usize,
    packages: Vec<IioPackage>,
}

impl SkxIioCollector {
    /// The IIO stacks of each package that PMU configs can program.
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        self.packages
            .iter()
            .map(|package| {
                (
                    package.scope.package_id as u16,
                    package
                        .units
                        .iter()
                        .map(|unit| unit.stack.id() as u16)
                        .collect(),
                )
            })
            .collect()
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        if let Some(config) = &config {
            check_config(
                config,
                BoxType::Iio,
                &self.config_units(),
                IIO_COUNTER_COUNT,
            )?;
        }

        self.config = config.map(CountAccumulator::new);
        // The free-running PCIe counters count on: no delta may span a config.
        self.discard_readings();
        Ok(())
    }

    /// Samples the set config for `interval`. Its counts are cumulative since
    /// the config was set.
    pub async fn sample_config(&mut self, interval: Duration) -> Result<UncoreCounts, String> {
        if interval.is_zero() {
            return Err("IIO measure interval must be non-zero".to_string());
        }
        let accumulator = self
            .config
            .as_mut()
            .ok_or_else(|| "IIO has no PMU config to sample".to_string())?;
        let program = accumulator.config().program.clone();

        for slice in config_schedule(&program, interval) {
            let units = config_targets(&program, &slice)
                .into_iter()
                .map(|target| Ok((config_unit(&self.packages, target)?, target)))
                .collect::<Result<Vec<_>, String>>()?;
            for (unit, target) in &units {
                unit.write_registers(&iio_config_writes(unit.stack, target.group))?;
            }

            let started_at = Instant::now();
            for (unit, _) in &units {
                unit.unfreeze()?;
            }
            tokio::time::sleep(slice.duration).await;
            for (unit, _) in &units {
                unit.freeze()?;
            }
            let running = started_at.elapsed();

            let mut readings = Vec::new();
            for (unit, target) in &units {
                for counter in &target.group.counters {
                    let counter_index = general_counter_index(counter.slot, IIO_COUNTER_COUNT)?;
                    readings.push(target.reading(counter.slot, unit.read_counter(counter_index)?));
                }
            }
            fold_config_slice(accumulator, &slice, &readings, running)?;
        }

        Ok(accumulator.counts())
    }
}

impl SkxIioCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            config: None,
            next_group: 0,
            packages,
        })
    }

    #[cfg(test)]
    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(IntelServerCpuModel::SkylakeXeon)
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SkxIioMetrics, String> {
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

        SkxIioMetrics::from_measurements(measurements.into_measurements(), ports)
    }

    /// Forgets the last PCIe readings, so the next sample reports no PCIe
    /// bandwidth rather than one averaged over a pause.
    pub fn discard_readings(&mut self) {
        for package in &mut self.packages {
            package.free_running.previous = None;
        }
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % SKX_IIO_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<IioMeasurementSlice> {
        let group_count = SKX_IIO_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(IioMeasurementSlice {
                    duration: slice_duration,
                    group: SKX_IIO_EVENT_GROUPS[rotated_index],
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
    fn from_port(port: IioPciePortMetrics) -> Self {
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
    fn from_scope(scope: UncoreScope, stack: SkxIioStack) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            package: scope.package_id.to_string(),
            stack: stack.label().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct SkxIioPrometheusMetrics {
    completion_inserts_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_latency_seconds: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    completion_occupancy_entries: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    l1_misses_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    l2_misses_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    l3_misses_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    pcie_read_bytes_per_second: Family<IioPciePortLabels, Gauge<f64, AtomicU64>>,
    pcie_write_bytes_per_second: Family<IioPciePortLabels, Gauge<f64, AtomicU64>>,
    tlb_hits_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    tlb_misses_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    vtd_accesses_per_second: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    vtd_latency_seconds: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
    vtd_occupancy_entries: Family<IioScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SkxIioPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            completion_inserts_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            completion_latency_seconds: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            completion_occupancy_entries: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            frequency_hz: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            l1_misses_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            l2_misses_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            l3_misses_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            pcie_read_bytes_per_second: Family::<IioPciePortLabels, Gauge<f64, AtomicU64>>::default(
            ),
            pcie_write_bytes_per_second:
                Family::<IioPciePortLabels, Gauge<f64, AtomicU64>>::default(),
            tlb_hits_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            tlb_misses_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            vtd_accesses_per_second: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            vtd_latency_seconds: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
            vtd_occupancy_entries: Family::<IioScopeLabels, Gauge<f64, AtomicU64>>::default(),
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
            "ocellus_iio_l1_misses_per_second",
            "Interval-derived IIO L1 misses per second",
            metrics.l1_misses_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_l2_misses_per_second",
            "Interval-derived IIO L2 misses per second",
            metrics.l2_misses_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_l3_misses_per_second",
            "Interval-derived IIO L3 misses per second",
            metrics.l3_misses_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_pcie_read_bytes_per_second",
            "IIO free-running PCIe read bandwidth in bytes per second",
            metrics.pcie_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_pcie_write_bytes_per_second",
            "IIO free-running PCIe write bandwidth in bytes per second",
            metrics.pcie_write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_tlb_hits_per_second",
            "Interval-derived IIO TLB hits per second",
            metrics.tlb_hits_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_tlb_misses_per_second",
            "Interval-derived IIO TLB misses per second",
            metrics.tlb_misses_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_vtd_accesses_per_second",
            "Interval-derived IIO VT-d accesses per second",
            metrics.vtd_accesses_per_second.clone(),
        );
        registry.register(
            "ocellus_iio_vtd_latency_seconds",
            "Interval-derived IIO VT-d access latency in seconds",
            metrics.vtd_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_iio_vtd_occupancy_entries",
            "Average IIO VT-d occupancy in entries",
            metrics.vtd_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SkxIioMetrics) {
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
            self.l1_misses_per_second
                .get_or_create(&labels)
                .set(scope.l1_misses_per_second);
            self.l2_misses_per_second
                .get_or_create(&labels)
                .set(scope.l2_misses_per_second);
            self.l3_misses_per_second
                .get_or_create(&labels)
                .set(scope.l3_misses_per_second);
            self.tlb_hits_per_second
                .get_or_create(&labels)
                .set(scope.tlb_hits_per_second);
            self.tlb_misses_per_second
                .get_or_create(&labels)
                .set(scope.tlb_misses_per_second);
            self.vtd_accesses_per_second
                .get_or_create(&labels)
                .set(scope.vtd_accesses_per_second);
            self.vtd_latency_seconds
                .get_or_create(&labels)
                .set(scope.vtd_latency_seconds);
            self.vtd_occupancy_entries
                .get_or_create(&labels)
                .set(scope.vtd_occupancy_entries);
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
    fn new(cpu: u32, scope: UncoreScope) -> Self {
        let units = SkxIioStack::ALL
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

    fn sample(&mut self, scope: UncoreScope) -> Result<Vec<IioPciePortMetrics>, String> {
        let current = (Instant::now(), self.read()?);
        self.advance(scope, current)
    }

    fn advance(
        &mut self,
        scope: UncoreScope,
        current: (Instant, Vec<u64>),
    ) -> Result<Vec<IioPciePortMetrics>, String> {
        let previous = match self.previous.replace(current.clone()) {
            Some(previous) => previous,
            None => return Ok(Vec::new()),
        };

        pcie_metrics_from_readings(scope, previous, current)
    }

    fn read(&self) -> Result<Vec<u64>, String> {
        let mut values = Vec::with_capacity(IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2);

        for stack in SkxIioStack::ALL {
            for port_index in 0..IIO_PCIE_PORT_COUNT {
                values.push(read_pcie_counter(
                    self.cpu,
                    iio_pcie_read_counter_offset(stack, port_index),
                )?);
            }
        }
        for stack in SkxIioStack::ALL {
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
    stack: SkxIioStack,
}

impl IioUnit {
    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE_AND_RESET))
    }

    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE))
    }

    fn program(self, group: IioEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            let control = if event.kind == IioEventKind::Unused {
                0
            } else {
                pmon::iio_counter_control(
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
            ticks: Msr::open_readonly(self.cpu)?
                .read(iio_clock_counter_offset(self.stack))
                .map(mask_iio_clock_counter)?,
        })
    }

    fn stack(self) -> SkxIioStack {
        self.stack
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_UNFREEZE))
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(iio_counter_offset(self.stack, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(iio_unit_control_offset(self.stack), value)
    }

    fn write_registers(self, writes: &[(u64, u64)]) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for &(address, value) in writes {
            msr.write(address, value)?;
        }

        Ok(())
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
    stack: SkxIioStack,
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
        stack: SkxIioStack,
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

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<IioPackage>, String> {
    if !matches!(model, IntelServerCpuModel::SkylakeXeon) {
        return Err(format!("IIO collection is not supported for {model:?}"));
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

fn vtd_latency_seconds(
    occupancy: &IioEventMeasurement,
    accesses: &IioEventMeasurement,
    clockticks: &IioEventMeasurement,
) -> f64 {
    let occupancy = scale_to_enabled(occupancy.value, occupancy.enabled, occupancy.running);
    let access_count = scale_to_enabled(accesses.value, accesses.enabled, accesses.running);
    let clockticks = scale_to_enabled(clockticks.value, clockticks.enabled, clockticks.running);

    queue_residency_seconds(occupancy, access_count, clockticks, accesses.enabled)
}

fn freeze_packages(packages: &[IioPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn iio_clock_counter_offset(stack: SkxIioStack) -> u64 {
    iio_unit_offset(IIO_CLOCK_COUNTER_BASE, stack)
}

fn iio_control_offset(stack: SkxIioStack, counter_index: usize) -> u64 {
    iio_unit_offset(IIO_CONTROL_BASE, stack) + counter_index as u64
}

fn iio_counter_offset(stack: SkxIioStack, counter_index: usize) -> u64 {
    iio_unit_offset(IIO_COUNTER_BASE, stack) + counter_index as u64
}

fn iio_pcie_read_counter_offset(stack: SkxIioStack, port_index: usize) -> u64 {
    iio_pcie_counter_offset(IIO_PCIE_READ_COUNTER_BASE, stack, port_index)
}

fn iio_pcie_write_counter_offset(stack: SkxIioStack, port_index: usize) -> u64 {
    iio_pcie_counter_offset(IIO_PCIE_WRITE_COUNTER_BASE, stack, port_index)
}

fn iio_unit_control_offset(stack: SkxIioStack) -> u64 {
    iio_unit_offset(IIO_UNIT_CONTROL_BASE, stack)
}

fn iio_pcie_counter_offset(base: u64, stack: SkxIioStack, port_index: usize) -> u64 {
    base + IIO_PCIE_COUNTER_STRIDE * stack.id() as u64 + port_index as u64
}

fn iio_unit_offset(base: u64, stack: SkxIioStack) -> u64 {
    base + IIO_UNIT_STRIDE * stack.id() as u64
}

fn mask_iio_clock_counter(counter: u64) -> u64 {
    mask_counter(counter, IIO_FREE_RUNNING_COUNTER_WIDTH)
}

fn mask_iio_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn pcie_metrics_from_readings(
    scope: UncoreScope,
    previous: (Instant, Vec<u64>),
    current: (Instant, Vec<u64>),
) -> Result<Vec<IioPciePortMetrics>, String> {
    if previous.1.len() != IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2
        || current.1.len() != IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2
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
    let write_offset = IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT;

    for stack in SkxIioStack::ALL {
        let stack_index = stack.id();
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

            ports.push(IioPciePortMetrics {
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

                if event.kind != IioEventKind::Unused {
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
    }

    Ok(())
}

fn read_pcie_counter(cpu: u32, address: u64) -> Result<u64, String> {
    Msr::open_readonly(cpu)?
        .read(address)
        .map(|counter| mask_counter(counter, IIO_FREE_RUNNING_COUNTER_WIDTH))
}

fn read_pcie_ports(packages: &mut [IioPackage]) -> Result<Vec<IioPciePortMetrics>, String> {
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

fn config_unit(packages: &[IioPackage], target: ConfigTarget<'_>) -> Result<IioUnit, String> {
    packages
        .iter()
        .filter(|package| package.scope.package_id as u16 == target.package)
        .flat_map(|package| &package.units)
        .find(|unit| unit.stack.id() as u16 == target.unit)
        .copied()
        .ok_or_else(|| {
            format!(
                "IIO stack {} of package {} does not exist",
                target.unit, target.package
            )
        })
}

/// The writes that freeze and reset an IIO stack and program `group` on it.
fn iio_config_writes(stack: SkxIioStack, group: &ProgrammedGroup) -> Vec<(u64, u64)> {
    let mut writes = vec![(
        iio_unit_control_offset(stack),
        u64::from(pmon::UNIT_FREEZE_AND_RESET),
    )];
    for counter_index in 0..IIO_COUNTER_COUNT {
        writes.push((
            iio_control_offset(stack, counter_index),
            config_counter_control(group, counter_index),
        ));
    }

    writes
}

// PMU config helpers shared with the SKX IRP collector.

/// One slice of a config sample: the groups that count in it, by position in
/// the config, and for how long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigSlice {
    pub(crate) duration: Duration,
    pub(crate) groups: Vec<usize>,
}

/// A unit that a config slice programs, and its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConfigTarget<'a> {
    pub(crate) package: u16,
    pub(crate) unit: u16,
    pub(crate) group: &'a ProgrammedGroup,
}

impl ConfigTarget<'_> {
    pub(crate) fn reading(self, slot: CounterSlot, raw: u64) -> ConfigReading {
        ConfigReading {
            package: self.package,
            unit: self.unit,
            group: self.group.index,
            slot,
            raw,
        }
    }
}

/// A counter as read at the end of a slice, before masking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConfigReading {
    pub(crate) package: u16,
    pub(crate) unit: u16,
    pub(crate) group: u8,
    pub(crate) slot: CounterSlot,
    pub(crate) raw: u64,
}

/// Checks that `config` programs `box_type` on units of `host_units`, with no
/// filters and only the box's `counter_count` general counters.
pub(crate) fn check_config(
    config: &PmuConfig,
    box_type: BoxType,
    host_units: &BTreeMap<u16, BTreeSet<u16>>,
    counter_count: usize,
) -> Result<(), String> {
    let label = box_type.label();
    if config.program.box_type != box_type {
        return Err(format!(
            "the {label} collector cannot run a config for {}",
            config.program.box_type.label()
        ));
    }

    for group in &config.program.groups {
        let path = format!("{label} config {} group {}", config.config_id, group.index);
        if group.filters.is_some() {
            return Err(format!("{path}: {label} groups take no filters"));
        }
        for counter in &group.counters {
            general_counter_index(counter.slot, counter_count)
                .map_err(|error| format!("{path}: {error}"))?;
        }
        for (package, units) in &group.units {
            let host = host_units
                .get(package)
                .ok_or_else(|| format!("{path}: package {package} has no {label} boxes"))?;
            if let Some(unit) = units.difference(host).next() {
                return Err(format!(
                    "{path}: unit {unit} does not exist in package {package}"
                ));
            }
        }
    }

    Ok(())
}

/// The control value of general counter `counter_index` in `group`, with the
/// reset bit added, or 0 for a counter the group does not use.
pub(crate) fn config_counter_control(group: &ProgrammedGroup, counter_index: usize) -> u64 {
    group
        .counters
        .iter()
        .find(|counter| counter.slot == CounterSlot::General(counter_index as u8))
        .map_or(0, |counter| {
            counter.control | u64::from(pmon::COUNTER_RESET)
        })
}

/// The slices of one sample: all groups at once for the whole interval, or
/// for temporal grouping each group in turn, in rounds.
pub(crate) fn config_schedule(program: &ProgrammedBoxType, interval: Duration) -> Vec<ConfigSlice> {
    let group_count = program.groups.len();
    match program.grouping {
        Grouping::Fixed | Grouping::Spatial => vec![ConfigSlice {
            duration: interval,
            groups: (0..group_count).collect(),
        }],
        Grouping::Temporal => {
            let round_count = measurement_round_count(interval, group_count);
            let slice_count = group_count * round_count;
            let duration = interval.div_f64(slice_count as f64);

            (0..round_count)
                .flat_map(|_| 0..group_count)
                .map(|group| ConfigSlice {
                    duration,
                    groups: vec![group],
                })
                .collect()
        }
    }
}

/// The units the groups of `slice` program, in group and then package and
/// unit order.
pub(crate) fn config_targets<'a>(
    program: &'a ProgrammedBoxType,
    slice: &ConfigSlice,
) -> Vec<ConfigTarget<'a>> {
    let mut targets = Vec::new();
    for &position in &slice.groups {
        let group = &program.groups[position];
        for (&package, units) in &group.units {
            for &unit in units {
                targets.push(ConfigTarget {
                    package,
                    unit,
                    group,
                });
            }
        }
    }

    targets
}

/// Adds one slice's readings, masked to the counter width, and its running
/// time to every group of the slice in every package the group runs in.
pub(crate) fn fold_config_slice(
    accumulator: &mut CountAccumulator,
    slice: &ConfigSlice,
    readings: &[ConfigReading],
    running: Duration,
) -> Result<(), String> {
    let groups = &accumulator.config().program.groups;
    let running_groups = slice
        .groups
        .iter()
        .flat_map(|&position| {
            let group = &groups[position];
            group.units.keys().map(|&package| (package, group.index))
        })
        .collect::<Vec<_>>();

    for reading in readings {
        accumulator.add_count(
            reading.package,
            reading.unit,
            reading.group,
            reading.slot,
            mask_counter(reading.raw, SKX_UNCORE_COUNTER_WIDTH),
        )?;
    }
    for (package, group) in running_groups {
        accumulator.add_running(package, group, running)?;
    }

    Ok(())
}

/// The index of general counter `slot` among the box's `counter_count`.
pub(crate) fn general_counter_index(
    slot: CounterSlot,
    counter_count: usize,
) -> Result<usize, String> {
    match slot {
        CounterSlot::General(index) if usize::from(index) < counter_count => Ok(usize::from(index)),
        _ => Err(format!(
            "slot {} is not one of the {counter_count} general counters",
            slot.label()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::pmu_config::types::{
        Aggregate, ChaFilters, CountUnit, EventLabels, GroupRunning, ProgrammedCounter, UnitCount,
    };

    const CONTROL: u64 = 0x50_0001;

    #[test]
    fn computes_iio_scope_metrics() {
        let scope = test_scope();
        let metrics = SkxIioMetrics::from_measurements(
            BTreeMap::from([(
                test_stack_scope(scope, SkxIioStack::Pcie0),
                BTreeMap::from([
                    measurement(IioEventKind::Clockticks, 1_000, 1_000, 100),
                    measurement(IioEventKind::CompletionInserts, 200, 1_000, 100),
                    measurement(IioEventKind::CompletionOccupancy, 400, 1_000, 100),
                    measurement(IioEventKind::L1Miss, 20, 1_000, 100),
                    measurement(IioEventKind::L2Miss, 30, 1_000, 100),
                    measurement(IioEventKind::L3Miss, 40, 1_000, 100),
                    measurement(IioEventKind::TlbHit, 70, 1_000, 100),
                    measurement(IioEventKind::TlbMiss, 80, 1_000, 100),
                    measurement(IioEventKind::VtdAccess, 100, 1_000, 100),
                    measurement(IioEventKind::VtdClockticks, 1_000, 1_000, 100),
                    measurement(IioEventKind::VtdOccupancy, 250, 1_000, 100),
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
        assert_eq!(scope_metrics.l1_misses_per_second, 200.0);
        assert_eq!(scope_metrics.l2_misses_per_second, 300.0);
        assert_eq!(scope_metrics.l3_misses_per_second, 400.0);
        assert_eq!(scope_metrics.stack, SkxIioStack::Pcie0);
        assert_eq!(scope_metrics.tlb_hits_per_second, 700.0);
        assert_eq!(scope_metrics.tlb_misses_per_second, 800.0);
        assert_eq!(scope_metrics.vtd_accesses_per_second, 1_000.0);
        assert_eq!(scope_metrics.vtd_latency_seconds, 0.00025);
        assert_eq!(scope_metrics.vtd_occupancy_entries, 0.25);
    }

    #[test]
    fn computes_pcie_port_metrics() {
        let scope = test_scope();
        let previous = (
            Instant::now(),
            vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2],
        );
        let mut current_values = vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2];
        current_values[0] = 100;
        current_values[IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT] = 200;
        let current = (previous.0 + Duration::from_millis(100), current_values);
        let ports = pcie_metrics_from_readings(scope, previous, current).unwrap();

        assert_eq!(ports[0].read_bytes_per_second, 4_000.0);
        assert_eq!(ports[0].stack, SkxIioStack::CbdmaDmi);
        assert_eq!(ports[0].write_bytes_per_second, 8_000.0);
    }

    #[test]
    fn computes_no_pcie_bandwidth_from_readings_before_a_pause() {
        let scope = test_scope();
        let mut collector = test_collector();
        collector.packages.push(IioPackage::new(0, scope));
        let start = Instant::now();
        let reading = |milliseconds, read_count| {
            let mut values = vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2];
            values[0] = read_count;
            (start + Duration::from_millis(milliseconds), values)
        };
        collector.packages[0]
            .free_running
            .advance(scope, reading(0, 0))
            .unwrap();

        collector.discard_readings();

        // The window ran for a second in between.
        let resumed = collector.packages[0]
            .free_running
            .advance(scope, reading(1_000, 1_000_000))
            .unwrap();
        assert!(resumed.is_empty());
        let ports = collector.packages[0]
            .free_running
            .advance(scope, reading(1_100, 1_000_100))
            .unwrap();
        assert_eq!(ports[0].read_bytes_per_second, 4_000.0);
    }

    #[test]
    fn uses_full_skx_iio_stack_address_map() {
        assert_eq!(iio_unit_control_offset(SkxIioStack::CbdmaDmi), 0x0a40);
        assert_eq!(iio_counter_offset(SkxIioStack::CbdmaDmi, 0), 0x0a41);
        assert_eq!(iio_clock_counter_offset(SkxIioStack::CbdmaDmi), 0x0a45);
        assert_eq!(iio_control_offset(SkxIioStack::CbdmaDmi, 0), 0x0a48);

        assert_eq!(iio_unit_control_offset(SkxIioStack::Pcie0), 0x0a60);
        assert_eq!(iio_unit_control_offset(SkxIioStack::Mcp1), 0x0ae0);
        assert_eq!(
            iio_pcie_read_counter_offset(SkxIioStack::CbdmaDmi, 0),
            0x0b00
        );
        assert_eq!(iio_pcie_read_counter_offset(SkxIioStack::Mcp1, 3), 0x0b53);
        assert_eq!(iio_pcie_write_counter_offset(SkxIioStack::Mcp1, 3), 0x0b57);
    }

    #[test]
    fn uses_documented_completion_buffer_events() {
        let completion_group = SKX_IIO_EVENT_GROUPS[2];

        assert_eq!(
            completion_group.events[1],
            IioEventSpec::sum(IioEventKind::CompletionInserts, 0xc2, 0x03, 0x0f, 0x04)
        );
        assert_eq!(
            completion_group.events[2],
            IioEventSpec::sum(IioEventKind::CompletionOccupancy, 0xd5, 0x0f, 0x00, 0x04)
        );
    }

    #[test]
    fn uses_documented_vtd_events() {
        let miss_group = SKX_IIO_EVENT_GROUPS[0];
        let vtd_group = SKX_IIO_EVENT_GROUPS[1];

        assert_eq!(
            miss_group.events[0],
            IioEventSpec::sum(IioEventKind::TlbMiss, 0x41, 0x20, 0x00, 0x00)
        );
        assert_eq!(
            miss_group.events[1],
            IioEventSpec::sum(IioEventKind::L1Miss, 0x41, 0x04, 0x00, 0x00)
        );
        assert_eq!(
            miss_group.events[2],
            IioEventSpec::sum(IioEventKind::L2Miss, 0x41, 0x08, 0x00, 0x00)
        );
        assert_eq!(
            miss_group.events[3],
            IioEventSpec::sum(IioEventKind::L3Miss, 0x41, 0x10, 0x00, 0x00)
        );
        assert_eq!(
            vtd_group.events[0],
            IioEventSpec::sum(IioEventKind::TlbHit, 0x41, 0x01, 0x00, 0x00)
        );
        assert_eq!(
            vtd_group.events[1],
            IioEventSpec::sum(IioEventKind::VtdAccess, 0x41, 0xff, 0x00, 0x00)
        );
        assert_eq!(
            vtd_group.events[2],
            IioEventSpec::sum(IioEventKind::VtdOccupancy, 0x40, 0x00, 0x00, 0x00)
        );
        assert_eq!(
            vtd_group.events[3],
            IioEventSpec::sum(IioEventKind::VtdClockticks, 0x01, 0x00, 0x00, 0x00)
        );
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                SKX_IIO_EVENT_GROUPS[0],
                SKX_IIO_EVENT_GROUPS[1],
                SKX_IIO_EVENT_GROUPS[2],
            ]
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                SKX_IIO_EVENT_GROUPS[1],
                SKX_IIO_EVENT_GROUPS[2],
                SKX_IIO_EVENT_GROUPS[0],
            ]
        );
    }

    #[test]
    fn supports_only_skylake_xeon_uncore_spec() {
        assert!(SkxIioCollector::is_supported(&test_architecture(0x55)));
        assert!(!SkxIioCollector::is_supported(&test_architecture(0xcf)));
    }

    #[test]
    fn addresses_config_units_by_package_and_stack_id() {
        let mut collector = test_collector();
        collector.packages.push(IioPackage::new(0, test_scope()));
        collector.packages.push(IioPackage::new(
            28,
            UncoreScope {
                package_id: 1,
                ..test_scope()
            },
        ));

        let stacks = BTreeSet::from([0, 1, 2, 3, 4, 5]);
        assert_eq!(
            collector.config_units(),
            BTreeMap::from([(0, stacks.clone()), (1, stacks)])
        );
        let group = test_group(0, &[(0, CONTROL)], &[1], &[2]);
        let unit = config_unit(
            &collector.packages,
            ConfigTarget {
                package: 1,
                unit: 2,
                group: &group,
            },
        )
        .unwrap();
        assert_eq!((unit.cpu, unit.stack), (28, SkxIioStack::Pcie1));
    }

    #[test]
    fn writes_config_group_on_stack_2() {
        // Event 0xc2, umask 0x03, ch_mask 0x0f at bit 36, fc_mask 0x04 at bit 44.
        let control = pmon::iio_counter_control_without_reset(0xc2, 0x03, 0x0f, 0x04, true);
        assert_eq!(control, 0x40f0_0050_03c2);
        let group = test_group(0, &[(0, control), (2, CONTROL)], &[0], &[2]);

        assert_eq!(
            iio_config_writes(SkxIioStack::Pcie1, &group),
            vec![
                (0x0a80, 0x3_0103),
                (0x0a88, 0x40f0_0052_03c2),
                (0x0a89, 0),
                (0x0a8a, 0x52_0001),
                (0x0a8b, 0),
            ]
        );
    }

    #[test]
    fn schedules_config_slices() {
        let interval = Duration::from_secs(1);
        let fixed = test_config(
            Grouping::Fixed,
            Aggregate::Sum,
            vec![test_group(0, &[(0, CONTROL)], &[0, 1], &[0, 2])],
        );
        assert_eq!(
            config_schedule(&fixed.program, interval),
            vec![slice(1_000, &[0])]
        );

        let spatial = test_config(
            Grouping::Spatial,
            Aggregate::Sum,
            vec![
                test_group(0, &[(0, CONTROL)], &[0], &[1]),
                test_group(1, &[(0, CONTROL)], &[0], &[2, 3]),
            ],
        );
        assert_eq!(
            config_schedule(&spatial.program, interval),
            vec![slice(1_000, &[0, 1])]
        );

        let temporal = test_config(
            Grouping::Temporal,
            Aggregate::Sum,
            vec![
                test_group(0, &[(0, CONTROL)], &[0], &[2]),
                test_group(1, &[(0, CONTROL)], &[0], &[2]),
            ],
        );
        assert_eq!(
            config_schedule(&temporal.program, interval),
            [0, 1]
                .repeat(5)
                .into_iter()
                .map(|group| slice(100, &[group]))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            config_schedule(&temporal.program, Duration::from_millis(50)),
            vec![slice(25, &[0]), slice(25, &[1])]
        );
    }

    #[test]
    fn lists_the_units_of_a_slice() {
        let config = test_config(
            Grouping::Spatial,
            Aggregate::Sum,
            vec![
                test_group(0, &[(0, CONTROL)], &[0, 1], &[1]),
                test_group(1, &[(0, CONTROL)], &[0, 1], &[2, 3]),
            ],
        );
        let groups = &config.program.groups;

        let targets = config_targets(&config.program, &slice(1_000, &[0, 1]))
            .into_iter()
            .map(|target| (target.package, target.unit, target.group.index))
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                (0, 1, 0),
                (1, 1, 0),
                (0, 2, 1),
                (0, 3, 1),
                (1, 2, 1),
                (1, 3, 1),
            ]
        );
        assert_eq!(
            config_targets(&config.program, &slice(1_000, &[1]))[0],
            ConfigTarget {
                package: 0,
                unit: 2,
                group: &groups[1],
            }
        );
    }

    #[test]
    fn folds_config_readings_into_cumulative_counts() {
        for aggregate in [Aggregate::Box, Aggregate::Sum] {
            let mut accumulator = CountAccumulator::new(test_config(
                Grouping::Spatial,
                aggregate,
                vec![
                    test_group(0, &[(0, CONTROL), (3, CONTROL)], &[0], &[1, 2]),
                    test_group(1, &[(1, CONTROL)], &[0], &[4]),
                ],
            ));
            let readings = [
                reading(1, 0, 0, 10),
                // Bits above the 48-bit counter are dropped.
                reading(2, 0, 0, (1 << 48) | 5),
                reading(1, 0, 3, 7),
                reading(2, 0, 3, 0),
                reading(4, 1, 1, 100),
            ];

            // Two samples of one slice each.
            for _ in 0..2 {
                fold_config_slice(
                    &mut accumulator,
                    &slice(1_000, &[0, 1]),
                    &readings,
                    Duration::from_millis(250),
                )
                .unwrap();
            }
            let counts = accumulator.counts();

            let expected = match aggregate {
                Aggregate::Box => vec![
                    count(CountUnit::Box(1), 0, 0, 20),
                    count(CountUnit::Box(1), 0, 3, 14),
                    count(CountUnit::Box(2), 0, 0, 10),
                    count(CountUnit::Box(2), 0, 3, 0),
                    count(CountUnit::Box(4), 1, 1, 200),
                ],
                Aggregate::Sum => vec![
                    count(CountUnit::Sum, 0, 0, 30),
                    count(CountUnit::Sum, 0, 3, 14),
                    count(CountUnit::Sum, 1, 1, 200),
                ],
            };
            assert_eq!(counts.counts, expected);
            assert_eq!(counts.running, vec![running(0, 0.5), running(1, 0.5)]);
        }
    }

    #[test]
    fn folds_temporal_running_time_per_group() {
        let mut accumulator = CountAccumulator::new(test_config(
            Grouping::Temporal,
            Aggregate::Sum,
            vec![
                test_group(0, &[(0, CONTROL)], &[0], &[2]),
                test_group(1, &[(0, CONTROL)], &[0], &[2]),
            ],
        ));

        for _ in 0..2 {
            fold_config_slice(
                &mut accumulator,
                &slice(500, &[0]),
                &[reading(2, 0, 0, 3)],
                Duration::from_millis(250),
            )
            .unwrap();
            fold_config_slice(
                &mut accumulator,
                &slice(500, &[1]),
                &[reading(2, 1, 0, 4)],
                Duration::from_millis(500),
            )
            .unwrap();
        }
        let counts = accumulator.counts();

        assert_eq!(
            counts.counts,
            vec![
                count(CountUnit::Sum, 0, 0, 6),
                count(CountUnit::Sum, 1, 0, 8)
            ]
        );
        assert_eq!(counts.running, vec![running(0, 0.5), running(1, 1.0)]);
    }

    #[test]
    fn rejects_configs_iio_cannot_run() {
        let mut collector = test_collector();
        collector.packages.push(IioPackage::new(0, test_scope()));
        let valid = || {
            test_config(
                Grouping::Fixed,
                Aggregate::Box,
                vec![test_group(0, &[(3, CONTROL)], &[0], &[2, 5])],
            )
        };
        collector.set_config(Some(valid())).unwrap();

        let mut other_box = valid();
        other_box.program.box_type = BoxType::Irp;
        let mut other_package = valid();
        other_package.program.groups[0].units = BTreeMap::from([(1, BTreeSet::from([2]))]);
        let mut other_unit = valid();
        other_unit.program.groups[0].units = BTreeMap::from([(0, BTreeSet::from([2, 6]))]);
        let mut fifth_counter = valid();
        fifth_counter.program.groups[0].counters[0].slot = CounterSlot::General(4);
        let mut fixed_counter = valid();
        fixed_counter.program.groups[0].counters[0].slot = CounterSlot::Fixed;
        let mut filters = valid();
        filters.program.groups[0].filters = Some(ChaFilters {
            filter0: 0,
            filter1: 0,
        });

        for (config, error) in [
            (other_box, "the iio collector cannot run a config for irp"),
            (
                other_package,
                "iio config 7 group 0: package 1 has no iio boxes",
            ),
            (
                other_unit,
                "iio config 7 group 0: unit 6 does not exist in package 0",
            ),
            (
                fifth_counter,
                "iio config 7 group 0: slot 4 is not one of the 4 general counters",
            ),
            (
                fixed_counter,
                "iio config 7 group 0: slot fixed is not one of the 4 general counters",
            ),
            (filters, "iio config 7 group 0: iio groups take no filters"),
        ] {
            assert_eq!(collector.set_config(Some(config)), Err(error.to_string()));
            // The config set before stays.
            assert_eq!(collector.config.as_ref().unwrap().config(), &valid());
        }
    }

    #[test]
    fn setting_a_config_restarts_counts_and_discards_pcie_readings() {
        let scope = test_scope();
        let mut collector = test_collector();
        collector.packages.push(IioPackage::new(0, scope));
        let config = test_config(
            Grouping::Fixed,
            Aggregate::Box,
            vec![test_group(0, &[(0, CONTROL)], &[0], &[2])],
        );
        let read_pcie = |collector: &mut SkxIioCollector| {
            collector.packages[0]
                .free_running
                .advance(
                    scope,
                    (
                        Instant::now(),
                        vec![0; IIO_UNIT_COUNT * IIO_PCIE_PORT_COUNT * 2],
                    ),
                )
                .unwrap();
        };

        read_pcie(&mut collector);
        collector.set_config(Some(config.clone())).unwrap();
        assert!(collector.has_config());
        assert!(collector.packages[0].free_running.previous.is_none());

        let accumulator = collector.config.as_mut().unwrap();
        accumulator
            .add_count(0, 2, 0, CounterSlot::General(0), 5)
            .unwrap();
        accumulator
            .add_running(0, 0, Duration::from_secs(1))
            .unwrap();
        collector.set_config(Some(config)).unwrap();
        let counts = collector.config.as_ref().unwrap().counts();
        assert_eq!(counts.counts, vec![count(CountUnit::Box(2), 0, 0, 0)]);
        assert_eq!(counts.running, vec![running(0, 0.0)]);

        read_pcie(&mut collector);
        collector.set_config(None).unwrap();
        assert!(!collector.has_config());
        assert!(collector.packages[0].free_running.previous.is_none());
    }

    #[test]
    fn samples_no_config_unless_one_is_set() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut collector = test_collector();

        assert_eq!(
            runtime.block_on(collector.sample_config(Duration::from_millis(1))),
            Err("IIO has no PMU config to sample".to_string())
        );
    }

    fn count(unit: CountUnit, group: u8, slot: u8, value: u64) -> UnitCount {
        UnitCount {
            package: 0,
            unit,
            group,
            slot: CounterSlot::General(slot),
            value,
        }
    }

    fn reading(unit: u16, group: u8, slot: u8, raw: u64) -> ConfigReading {
        ConfigReading {
            package: 0,
            unit,
            group,
            slot: CounterSlot::General(slot),
            raw,
        }
    }

    fn running(group: u8, seconds: f64) -> GroupRunning {
        GroupRunning {
            package: 0,
            group,
            seconds,
        }
    }

    fn slice(milliseconds: u64, groups: &[usize]) -> ConfigSlice {
        ConfigSlice {
            duration: Duration::from_millis(milliseconds),
            groups: groups.to_vec(),
        }
    }

    fn test_config(
        grouping: Grouping,
        aggregate: Aggregate,
        groups: Vec<ProgrammedGroup>,
    ) -> PmuConfig {
        PmuConfig {
            config_id: 7,
            started_at: SystemTime::UNIX_EPOCH,
            program: ProgrammedBoxType {
                box_type: BoxType::Iio,
                grouping,
                aggregate,
                groups,
            },
        }
    }

    fn test_group(
        index: u8,
        counters: &[(u8, u64)],
        packages: &[u16],
        units: &[u16],
    ) -> ProgrammedGroup {
        ProgrammedGroup {
            index,
            counters: counters
                .iter()
                .map(|&(slot, control)| ProgrammedCounter {
                    slot: CounterSlot::General(slot),
                    control,
                    labels: EventLabels::new(),
                })
                .collect(),
            filters: None,
            units: packages
                .iter()
                .map(|&package| (package, units.iter().copied().collect()))
                .collect(),
        }
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

    fn slice_groups(slices: Vec<IioMeasurementSlice>) -> Vec<IioEventGroup> {
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

    fn test_collector() -> SkxIioCollector {
        SkxIioCollector {
            config: None,
            next_group: 0,
            packages: Vec::new(),
        }
    }

    fn test_scope() -> UncoreScope {
        UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }

    fn test_stack_scope(scope: UncoreScope, stack: SkxIioStack) -> IioStackScope {
        IioStackScope { scope, stack }
    }
}
