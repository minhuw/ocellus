use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::msr::Msr;
use crate::metrics::pcu::{
    PcuClockMetrics, PcuCoreCState, PcuCoreCStateMetrics, PcuCycleRatioMetrics,
    PcuFrequencyLimitMetrics, PcuFrequencyLimitReason, PcuPackageCStateLabels,
    PcuPackageFrequencyLimitLabels, PcuPackageLabels, PcuPackageScope,
    PcuPackageThermalThrottleLabels, PcuThermalThrottleSource, cycle_ratio, occupancy_average,
    register_frequency,
};
use crate::metrics::uncore::hsx::HsxUncoreScope;
use crate::metrics::uncore::skx::wrapping_delta;

const COUNTER_COUNT: usize = 4;
const COUNTER_WIDTH: u32 = 48;
const PCU_COUNTER_BASE: u64 = 0xc36;
const PCU_CONTROL_BASE: u64 = 0xc30;
const PCU_BOX_CONTROL: u64 = 0xc24;
const PCU_FILTER: u64 = 0xc34;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 0;
const UNIT_FREEZE_BIT: u64 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u64 = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbPcuArchitecture {
    Ivb,
    Snb,
}

impl SnbPcuArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::IvyTown => Some(Self::Ivb),
            IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
            _ => None,
        }
    }

    fn event_groups(self) -> &'static [SnbPcuEventGroup] {
        match self {
            Self::Ivb => &IVB_PCU_EVENT_GROUPS,
            Self::Snb => &SNB_PCU_EVENT_GROUPS,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ivb => "Ivy Bridge-EP",
            Self::Snb => "Sandy Bridge-EP",
        }
    }

    const fn unit_freeze(self) -> u64 {
        UNIT_FREEZE_BIT | self.unit_freeze_enable()
    }

    const fn unit_freeze_and_reset(self) -> u64 {
        UNIT_CONTROL_RESET_BIT
            | UNIT_COUNTER_RESET_BIT
            | UNIT_FREEZE_BIT
            | self.unit_freeze_enable()
    }

    const fn unit_freeze_enable(self) -> u64 {
        match self {
            Self::Ivb => 0,
            Self::Snb => UNIT_FREEZE_ENABLE_BIT,
        }
    }

    const fn unit_unfreeze(self) -> u64 {
        self.unit_freeze_enable()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnbPcuEventKind {
    Clockticks,
    CoreCState(PcuCoreCState),
    FrequencyLimit(PcuFrequencyLimitReason),
    FrequencyTransition,
    MemoryPhaseShedding,
    PackageCState(PcuCoreCState),
    ThermalThrottle(PcuThermalThrottleSource),
    Unused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbPcuEventSpec {
    event: u8,
    ext_sel: bool,
    kind: SnbPcuEventKind,
    occ_sel: u8,
}

impl SnbPcuEventSpec {
    const fn new(kind: SnbPcuEventKind, event: u8) -> Self {
        Self {
            event,
            ext_sel: false,
            kind,
            occ_sel: 0,
        }
    }

    const fn ext(kind: SnbPcuEventKind, event: u8) -> Self {
        Self {
            event,
            ext_sel: true,
            kind,
            occ_sel: 0,
        }
    }

    const fn occupancy(c_state: PcuCoreCState, occ_sel: u8) -> Self {
        Self {
            event: 0x80,
            ext_sel: false,
            kind: SnbPcuEventKind::CoreCState(c_state),
            occ_sel,
        }
    }

    const fn unused() -> Self {
        Self::new(SnbPcuEventKind::Unused, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbPcuEventGroup {
    events: [SnbPcuEventSpec; COUNTER_COUNT],
}

const SNB_PCU_EVENT_GROUPS: [SnbPcuEventGroup; 4] = [
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::new(SnbPcuEventKind::Clockticks, 0x00),
            SnbPcuEventSpec::occupancy(PcuCoreCState::C0, 0x01),
            SnbPcuEventSpec::occupancy(PcuCoreCState::C3, 0x02),
            SnbPcuEventSpec::occupancy(PcuCoreCState::C6, 0x03),
        ],
    },
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Power),
                0x05,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Thermal),
                0x04,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Os),
                0x06,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Current),
                0x07,
            ),
        ],
    },
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::ext(SnbPcuEventKind::FrequencyTransition, 0x00),
            SnbPcuEventSpec::ext(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::IoP),
                0x01,
            ),
            SnbPcuEventSpec::ext(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::PerfP),
                0x02,
            ),
            SnbPcuEventSpec::new(SnbPcuEventKind::MemoryPhaseShedding, 0x2f),
        ],
    },
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::new(
                SnbPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::InternalProchot),
                0x09,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::ExternalProchot),
                0x0a,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::VrHot),
                0x32,
            ),
            SnbPcuEventSpec::unused(),
        ],
    },
];

const IVB_PCU_EVENT_GROUPS: [SnbPcuEventGroup; 5] = [
    SNB_PCU_EVENT_GROUPS[0],
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Power),
                0x05,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Thermal),
                0x04,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Os),
                0x06,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Current),
                0x07,
            ),
        ],
    },
    SnbPcuEventGroup {
        events: [
            SnbPcuEventSpec::new(SnbPcuEventKind::FrequencyTransition, 0x60),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::IoP),
                0x61,
            ),
            SnbPcuEventSpec::new(
                SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::PerfP),
                0x62,
            ),
            SnbPcuEventSpec::new(SnbPcuEventKind::MemoryPhaseShedding, 0x2f),
        ],
    },
    SNB_PCU_EVENT_GROUPS[3],
    SnbPcuEventGroup {
        events: [
            // IvyTown hardware and Linux perf encode these without PCU ev_sel_ext.
            SnbPcuEventSpec::new(SnbPcuEventKind::PackageCState(PcuCoreCState::C0), 0x2a),
            SnbPcuEventSpec::new(SnbPcuEventKind::PackageCState(PcuCoreCState::C3), 0x2c),
            SnbPcuEventSpec::new(SnbPcuEventKind::PackageCState(PcuCoreCState::C6), 0x2d),
            SnbPcuEventSpec::unused(),
        ],
    },
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnbPcuMetrics {
    pub clocks: Vec<PcuClockMetrics<PcuPackageScope>>,
    pub core_c_states: Vec<PcuCoreCStateMetrics<PcuPackageScope>>,
    pub frequency_limits: Vec<PcuFrequencyLimitMetrics<PcuPackageScope>>,
    pub frequency_transition: Vec<PcuCycleRatioMetrics<PcuPackageScope, &'static str>>,
    pub memory_phase_shedding: Vec<PcuCycleRatioMetrics<PcuPackageScope, &'static str>>,
    pub package_c_states: Vec<crate::metrics::pcu::PcuPackageCStateMetrics<PcuPackageScope>>,
    pub thermal_throttles: Vec<PcuCycleRatioMetrics<PcuPackageScope, PcuThermalThrottleSource>>,
}

impl SnbPcuMetrics {
    fn from_measurements(
        measurements: BTreeMap<PcuPackageScope, BTreeMap<SnbPcuEventKind, SnbPcuMeasurement>>,
    ) -> Result<Self, String> {
        let mut clocks = Vec::new();
        let mut core_c_states = Vec::new();
        let mut frequency_limits = Vec::new();
        let mut frequency_transition = Vec::new();
        let mut memory_phase_shedding = Vec::new();
        let mut package_c_states = Vec::new();
        let mut thermal_throttles = Vec::new();

        for (scope, measurements) in measurements {
            let clockticks = required_measurement(&measurements, SnbPcuEventKind::Clockticks)?;
            clocks.push(PcuClockMetrics {
                frequency_hz: event_rate(clockticks),
                scope,
            });

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) = measurements.get(&SnbPcuEventKind::CoreCState(c_state)) {
                    core_c_states.push(PcuCoreCStateMetrics {
                        average_cores: occupancy_average(measurement.value, measurement.ticks),
                        c_state,
                        scope,
                    });
                }
            }

            for reason in [
                PcuFrequencyLimitReason::Power,
                PcuFrequencyLimitReason::Thermal,
                PcuFrequencyLimitReason::Os,
                PcuFrequencyLimitReason::Current,
                PcuFrequencyLimitReason::IoP,
                PcuFrequencyLimitReason::PerfP,
            ] {
                if let Some(measurement) =
                    measurements.get(&SnbPcuEventKind::FrequencyLimit(reason))
                {
                    frequency_limits.push(PcuFrequencyLimitMetrics {
                        ratio: cycle_ratio(measurement.value, measurement.ticks),
                        reason,
                        scope,
                    });
                }
            }

            if let Some(measurement) = measurements.get(&SnbPcuEventKind::FrequencyTransition) {
                frequency_transition.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "frequency_transition",
                });
            }

            if let Some(measurement) = measurements.get(&SnbPcuEventKind::MemoryPhaseShedding) {
                memory_phase_shedding.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "memory_phase_shedding",
                });
            }

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) =
                    measurements.get(&SnbPcuEventKind::PackageCState(c_state))
                {
                    package_c_states.push(crate::metrics::pcu::PcuPackageCStateMetrics {
                        c_state,
                        ratio: cycle_ratio(measurement.value, measurement.ticks),
                        scope,
                    });
                }
            }

            for source in [
                PcuThermalThrottleSource::InternalProchot,
                PcuThermalThrottleSource::ExternalProchot,
                PcuThermalThrottleSource::VrHot,
            ] {
                if let Some(measurement) =
                    measurements.get(&SnbPcuEventKind::ThermalThrottle(source))
                {
                    thermal_throttles.push(PcuCycleRatioMetrics {
                        ratio: cycle_ratio(measurement.value, measurement.ticks),
                        scope,
                        source,
                    });
                }
            }
        }

        Ok(Self {
            clocks,
            core_c_states,
            frequency_limits,
            frequency_transition,
            memory_phase_shedding,
            package_c_states,
            thermal_throttles,
        })
    }
}

#[derive(Debug)]
pub struct SnbPcuCollector {
    architecture: SnbPcuArchitecture,
    next_group: usize,
    packages: Vec<SnbPcuPackage>,
}

impl SnbPcuCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = SnbPcuArchitecture::from_model(model).ok_or_else(|| {
            format!("Sandy/Ivy Bridge-EP PCU collection is not supported for {model:?}")
        })?;
        let packages = discover_packages(architecture)?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            architecture,
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SnbPcuMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} PCU measure interval must be non-zero",
                self.architecture.name()
            ));
        }

        let mut measurements = SnbPcuMeasurementAccumulator::new();
        let groups = self.architecture.event_groups();
        let slice_count = groups.len();
        let slice_duration = interval.div_f64(slice_count as f64);
        let clock_group = groups[0];

        program_packages(&self.packages, clock_group)?;

        let started_at = Instant::now();
        unfreeze_packages(&self.packages)?;
        tokio::time::sleep(slice_duration).await;
        freeze_packages(&self.packages)?;

        read_packages(
            &self.packages,
            started_at.elapsed(),
            clock_group,
            None,
            &mut measurements,
        )?;

        let clock_estimates = measurements.clock_estimates()?;
        let multiplexed_groups = &groups[1..];

        for offset in 0..multiplexed_groups.len() {
            let group = multiplexed_groups[(self.next_group + offset) % multiplexed_groups.len()];
            program_packages(&self.packages, group)?;

            let started_at = Instant::now();
            unfreeze_packages(&self.packages)?;
            tokio::time::sleep(slice_duration).await;
            freeze_packages(&self.packages)?;

            read_packages(
                &self.packages,
                started_at.elapsed(),
                group,
                Some(&clock_estimates),
                &mut measurements,
            )?;
        }

        self.next_group = (self.next_group + 1) % multiplexed_groups.len();

        SnbPcuMetrics::from_measurements(measurements.into_measurements())
    }
}

#[derive(Clone, Copy, Debug)]
struct SnbPcuUnit {
    architecture: SnbPcuArchitecture,
    cpu: u32,
}

impl SnbPcuUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_freeze())
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_freeze_and_reset())
    }

    fn program(self, group: SnbPcuEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        msr.write(PCU_FILTER, 0)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(pcu_control_offset(counter_index), counter_control(event))?;
        }
        Ok(())
    }

    fn read(self, group: SnbPcuEventGroup) -> Result<SnbPcuUnitReading, String> {
        let mut counters = BTreeMap::new();
        for (counter_index, event) in group.events.into_iter().enumerate() {
            if !matches!(event.kind, SnbPcuEventKind::Unused) {
                counters.insert(
                    event.kind,
                    mask_counter(
                        Msr::open_readonly(self.cpu)?.read(pcu_counter_offset(counter_index))?,
                    ),
                );
            }
        }
        Ok(SnbPcuUnitReading { counters })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_unfreeze())
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(PCU_BOX_CONTROL, value)
    }

    fn probe_writable(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        msr.write(PCU_BOX_CONTROL, self.architecture.unit_freeze())?;
        msr.write(PCU_FILTER, 0)?;
        msr.write(PCU_CONTROL_BASE, 0)?;
        Ok(())
    }
}

#[derive(Debug)]
struct SnbPcuPackage {
    scope: PcuPackageScope,
    unit: SnbPcuUnit,
}

#[derive(Debug)]
struct SnbPcuUnitReading {
    counters: BTreeMap<SnbPcuEventKind, u64>,
}

#[derive(Clone, Copy, Debug)]
struct SnbPcuMeasurement {
    running: Duration,
    ticks: u64,
    value: u64,
}

#[derive(Clone, Copy, Debug)]
struct SnbPcuClockEstimate {
    running: Duration,
    ticks: u64,
}

impl SnbPcuClockEstimate {
    fn ticks_for(self, running: Duration) -> u64 {
        if self.running.is_zero() {
            return 0;
        }

        (self.ticks as f64 * running.as_secs_f64() / self.running.as_secs_f64()) as u64
    }
}

#[derive(Debug, Default)]
struct SnbPcuMeasurementAccumulator {
    measurements: BTreeMap<PcuPackageScope, BTreeMap<SnbPcuEventKind, SnbPcuMeasurement>>,
}

impl SnbPcuMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: PcuPackageScope,
        kind: SnbPcuEventKind,
        value: u64,
        ticks: u64,
        running: Duration,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|measurement| {
                measurement.value += value;
                measurement.ticks += ticks;
                measurement.running += running;
            })
            .or_insert(SnbPcuMeasurement {
                running,
                ticks,
                value,
            });
    }

    fn clock_estimates(&self) -> Result<BTreeMap<PcuPackageScope, SnbPcuClockEstimate>, String> {
        let mut estimates = BTreeMap::new();

        for (scope, measurements) in &self.measurements {
            let clockticks = required_measurement(measurements, SnbPcuEventKind::Clockticks)?;
            estimates.insert(
                *scope,
                SnbPcuClockEstimate {
                    running: clockticks.running,
                    ticks: clockticks.value,
                },
            );
        }

        Ok(estimates)
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<PcuPackageScope, BTreeMap<SnbPcuEventKind, SnbPcuMeasurement>> {
        self.measurements
    }
}

#[derive(Debug)]
pub struct SnbPcuPrometheusMetrics {
    core_c_state_average_cores: Family<PcuPackageCStateLabels, Gauge<f64, AtomicU64>>,
    frequency: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    frequency_limit_ratio: Family<PcuPackageFrequencyLimitLabels, Gauge<f64, AtomicU64>>,
    frequency_transition_ratio: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    memory_phase_shedding_ratio: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    package_c_state_ratio: Family<PcuPackageCStateLabels, Gauge<f64, AtomicU64>>,
    thermal_throttle_ratio: Family<PcuPackageThermalThrottleLabels, Gauge<f64, AtomicU64>>,
}

impl SnbPcuPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            core_c_state_average_cores: Family::default(),
            frequency: register_frequency(registry, "Interval-derived PCU PCLK frequency in hertz"),
            frequency_limit_ratio: Family::default(),
            frequency_transition_ratio: Family::default(),
            memory_phase_shedding_ratio: Family::default(),
            package_c_state_ratio: Family::default(),
            thermal_throttle_ratio: Family::default(),
        };

        registry.register(
            "ocellus_pcu_core_c_state_average_cores",
            "Average number of cores in the PCU-observed core C-state during the interval",
            metrics.core_c_state_average_cores.clone(),
        );
        registry.register(
            "ocellus_pcu_frequency_limit_ratio",
            "Ratio of PCU cycles where the given reason limited processor frequency",
            metrics.frequency_limit_ratio.clone(),
        );
        registry.register(
            "ocellus_pcu_frequency_transition_ratio",
            "Ratio of PCU cycles spent changing processor frequency",
            metrics.frequency_transition_ratio.clone(),
        );
        registry.register(
            "ocellus_pcu_memory_phase_shedding_ratio",
            "Ratio of PCU cycles with memory phase shedding active",
            metrics.memory_phase_shedding_ratio.clone(),
        );
        registry.register(
            "ocellus_pcu_package_c_state_ratio",
            "Ratio of PCU cycles spent in the package C-state",
            metrics.package_c_state_ratio.clone(),
        );
        registry.register(
            "ocellus_pcu_thermal_throttle_ratio",
            "Ratio of PCU cycles spent under the thermal throttle source",
            metrics.thermal_throttle_ratio.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SnbPcuMetrics) {
        for metric in metrics.clocks {
            self.frequency
                .get_or_create(&PcuPackageLabels::new(metric.scope))
                .set(metric.frequency_hz);
        }
        for metric in metrics.core_c_states {
            self.core_c_state_average_cores
                .get_or_create(&PcuPackageCStateLabels::new(metric.scope, metric.c_state))
                .set(metric.average_cores);
        }
        for metric in metrics.frequency_limits {
            self.frequency_limit_ratio
                .get_or_create(&PcuPackageFrequencyLimitLabels::new(
                    metric.scope,
                    metric.reason,
                ))
                .set(metric.ratio);
        }
        for metric in metrics.frequency_transition {
            self.frequency_transition_ratio
                .get_or_create(&PcuPackageLabels::new(metric.scope))
                .set(metric.ratio);
        }
        for metric in metrics.memory_phase_shedding {
            self.memory_phase_shedding_ratio
                .get_or_create(&PcuPackageLabels::new(metric.scope))
                .set(metric.ratio);
        }
        for metric in metrics.package_c_states {
            self.package_c_state_ratio
                .get_or_create(&PcuPackageCStateLabels::new(metric.scope, metric.c_state))
                .set(metric.ratio);
        }
        for metric in metrics.thermal_throttles {
            self.thermal_throttle_ratio
                .get_or_create(&PcuPackageThermalThrottleLabels::new(
                    metric.scope,
                    metric.source,
                ))
                .set(metric.ratio);
        }
    }
}

fn counter_control(event: SnbPcuEventSpec) -> u64 {
    if matches!(event.kind, SnbPcuEventKind::Unused) {
        return 0;
    }

    u64::from(event.event)
        | (u64::from(event.occ_sel) << 14)
        | ((event.ext_sel as u64) << 21)
        | (1 << 17)
        | (1 << 20)
        | (1 << 22)
}

fn discover_packages(architecture: SnbPcuArchitecture) -> Result<Vec<SnbPcuPackage>, String> {
    let packages = package_leaders()?
        .into_iter()
        .map(|(scope, cpu)| SnbPcuPackage {
            scope,
            unit: SnbPcuUnit { architecture, cpu },
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any Sandy/Ivy Bridge-EP PCU packages".to_string());
    }

    Ok(packages)
}

fn package_leaders() -> Result<Vec<(PcuPackageScope, u32)>, String> {
    let mut leaders = BTreeMap::new();

    for topology in crate::metal::topology::cpu_topologies()? {
        let package_id = topology
            .level_id(crate::metal::topology::TopologyLevelKind::Package)
            .ok_or_else(|| "CPU topology is missing package level".to_string())?;
        leaders
            .entry(PcuPackageScope::from_hsx_scope(HsxUncoreScope {
                package_id,
            }))
            .or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err("failed to discover any CPU package leaders".to_string());
    }

    Ok(leaders.into_iter().collect())
}

fn event_rate(measurement: &SnbPcuMeasurement) -> f64 {
    let elapsed = measurement.running.as_secs_f64();
    if elapsed == 0.0 {
        0.0
    } else {
        measurement.value as f64 / elapsed
    }
}

fn freeze_packages(packages: &[SnbPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }
    Ok(())
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << COUNTER_WIDTH) - 1)
}

const fn pcu_control_offset(counter_index: usize) -> u64 {
    PCU_CONTROL_BASE + counter_index as u64
}

const fn pcu_counter_offset(counter_index: usize) -> u64 {
    PCU_COUNTER_BASE + counter_index as u64
}

fn probe_writable_msrs(packages: &[SnbPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.probe_writable()?;
    }
    Ok(())
}

fn program_packages(packages: &[SnbPcuPackage], group: SnbPcuEventGroup) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
        package.unit.program(group)?;
    }
    Ok(())
}

fn read_packages(
    packages: &[SnbPcuPackage],
    running: Duration,
    group: SnbPcuEventGroup,
    clock_estimates: Option<&BTreeMap<PcuPackageScope, SnbPcuClockEstimate>>,
    measurements: &mut SnbPcuMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let reading = package.unit.read(group)?;
        let ticks = match reading.counters.get(&SnbPcuEventKind::Clockticks) {
            Some(clockticks) => wrapping_delta(0, *clockticks, COUNTER_WIDTH),
            None => clock_estimates
                .and_then(|estimates| estimates.get(&package.scope))
                .ok_or_else(|| {
                    format!(
                        "PCU clock estimate is missing for package {}",
                        package.scope.package_id
                    )
                })?
                .ticks_for(running),
        };

        for (kind, value) in reading.counters {
            let value = wrapping_delta(0, value, COUNTER_WIDTH);
            measurements.add(package.scope, kind, value, ticks, running);
        }
    }
    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<SnbPcuEventKind, SnbPcuMeasurement>,
    kind: SnbPcuEventKind,
) -> Result<&SnbPcuMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("PCU measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[SnbPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandy_bridge_box_control_keeps_freeze_enable_bit() {
        assert_eq!(
            SnbPcuArchitecture::Snb.unit_freeze(),
            UNIT_FREEZE_BIT | UNIT_FREEZE_ENABLE_BIT
        );
        assert_eq!(
            SnbPcuArchitecture::Snb.unit_freeze_and_reset(),
            UNIT_CONTROL_RESET_BIT
                | UNIT_COUNTER_RESET_BIT
                | UNIT_FREEZE_BIT
                | UNIT_FREEZE_ENABLE_BIT
        );
        assert_eq!(
            SnbPcuArchitecture::Snb.unit_unfreeze(),
            UNIT_FREEZE_ENABLE_BIT
        );
    }

    #[test]
    fn ivy_bridge_box_control_drops_freeze_enable_bit() {
        assert_eq!(SnbPcuArchitecture::Ivb.unit_freeze(), UNIT_FREEZE_BIT);
        assert_eq!(
            SnbPcuArchitecture::Ivb.unit_freeze_and_reset(),
            UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT
        );
        assert_eq!(SnbPcuArchitecture::Ivb.unit_unfreeze(), 0);
    }

    #[test]
    fn sandy_bridge_frequency_transition_uses_ext_select() {
        let event = SNB_PCU_EVENT_GROUPS[2].events[0];

        assert_eq!(event.event, 0x00);
        assert!(event.ext_sel);
        assert_ne!(counter_control(event) & (1 << 21), 0);
    }

    #[test]
    fn ivy_bridge_frequency_transition_uses_native_event() {
        let event = IVB_PCU_EVENT_GROUPS[2].events[0];

        assert_eq!(event.event, 0x60);
        assert!(!event.ext_sel);
        assert_eq!(counter_control(event) & (1 << 21), 0);
    }

    #[test]
    fn occupancy_events_encode_occ_sel() {
        let c6 = SNB_PCU_EVENT_GROUPS[0].events[3];

        assert_eq!(c6.event, 0x80);
        assert_eq!((counter_control(c6) >> 14) & 0x03, 0x03);
    }

    #[test]
    fn sandy_bridge_does_not_collect_package_c_states() {
        for group in SNB_PCU_EVENT_GROUPS {
            for event in group.events {
                assert!(!matches!(event.kind, SnbPcuEventKind::PackageCState(_)));
            }
        }
    }

    #[test]
    fn ivy_bridge_package_c_states_do_not_use_ext_select() {
        let group = IVB_PCU_EVENT_GROUPS[4];

        for event in &group.events[..3] {
            assert!(matches!(event.kind, SnbPcuEventKind::PackageCState(_)));
            assert!(!event.ext_sel);
            assert_eq!(counter_control(*event) & (1 << 21), 0);
        }
        assert_eq!(group.events[0].event, 0x2a);
        assert_eq!(group.events[1].event, 0x2c);
        assert_eq!(group.events[2].event, 0x2d);
    }

    #[test]
    fn clock_estimate_scales_ticks_to_other_slices() {
        let estimate = SnbPcuClockEstimate {
            running: Duration::from_millis(20),
            ticks: 16_000_000,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 8_000_000);
        assert_eq!(estimate.ticks_for(Duration::from_millis(40)), 32_000_000);
    }

    #[test]
    fn zero_running_clock_estimate_returns_zero_ticks() {
        let estimate = SnbPcuClockEstimate {
            running: Duration::ZERO,
            ticks: 1,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 0);
    }

    #[test]
    fn derives_ratios_from_measurements() {
        let metrics = SnbPcuMetrics::from_measurements(BTreeMap::from([(
            PcuPackageScope { package_id: 0 },
            BTreeMap::from([
                (
                    SnbPcuEventKind::Clockticks,
                    SnbPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 100,
                    },
                ),
                (
                    SnbPcuEventKind::CoreCState(PcuCoreCState::C0),
                    SnbPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 250,
                    },
                ),
                (
                    SnbPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Power),
                    SnbPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 25,
                    },
                ),
            ]),
        )]))
        .unwrap();

        assert_eq!(metrics.clocks[0].frequency_hz, 100.0);
        assert_eq!(metrics.core_c_states[0].average_cores, 2.5);
        assert_eq!(metrics.frequency_limits[0].ratio, 0.25);
    }
}
