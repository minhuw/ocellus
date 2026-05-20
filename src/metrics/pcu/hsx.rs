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
const PCU_COUNTER_BASE: u64 = 0x717;
const PCU_CONTROL_BASE: u64 = 0x711;
const PCU_BOX_CONTROL: u64 = 0x710;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 0;
const UNIT_FREEZE_BIT: u64 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u64 = 1 << 16;

const UNIT_FREEZE: u64 = UNIT_FREEZE_BIT | UNIT_FREEZE_ENABLE_BIT;
const UNIT_FREEZE_AND_RESET: u64 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT | UNIT_FREEZE_ENABLE_BIT;
const UNIT_UNFREEZE: u64 = UNIT_FREEZE_ENABLE_BIT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HsxPcuArchitecture {
    Bdx,
    BdxDe,
    Hsx,
}

impl HsxPcuArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::BroadwellDe => Some(Self::BdxDe),
            IntelServerCpuModel::BroadwellXeon => Some(Self::Bdx),
            IntelServerCpuModel::HaswellXeon => Some(Self::Hsx),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bdx => "Broadwell",
            Self::BdxDe => "Broadwell-DE",
            Self::Hsx => "Haswell",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxPcuEventKind {
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
struct HsxPcuEventSpec {
    event: u8,
    kind: HsxPcuEventKind,
    umask: u8,
}

impl HsxPcuEventSpec {
    const fn new(kind: HsxPcuEventKind, event: u8) -> Self {
        Self {
            event,
            kind,
            umask: 0,
        }
    }

    const fn occupancy(c_state: PcuCoreCState, umask: u8) -> Self {
        Self {
            event: 0x80,
            kind: HsxPcuEventKind::CoreCState(c_state),
            umask,
        }
    }

    const fn unused() -> Self {
        Self::new(HsxPcuEventKind::Unused, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxPcuEventGroup {
    events: [HsxPcuEventSpec; COUNTER_COUNT],
}

const HSX_PCU_EVENT_GROUPS: [HsxPcuEventGroup; 5] = [
    HsxPcuEventGroup {
        events: [
            HsxPcuEventSpec::new(HsxPcuEventKind::Clockticks, 0x00),
            HsxPcuEventSpec::occupancy(PcuCoreCState::C0, 0x40),
            HsxPcuEventSpec::occupancy(PcuCoreCState::C3, 0x80),
            HsxPcuEventSpec::occupancy(PcuCoreCState::C6, 0xc0),
        ],
    },
    HsxPcuEventGroup {
        events: [
            HsxPcuEventSpec::new(
                HsxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Power),
                0x05,
            ),
            HsxPcuEventSpec::new(
                HsxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Thermal),
                0x04,
            ),
            HsxPcuEventSpec::new(
                HsxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Os),
                0x06,
            ),
            HsxPcuEventSpec::new(
                HsxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::IoP),
                0x73,
            ),
        ],
    },
    HsxPcuEventGroup {
        events: [
            HsxPcuEventSpec::new(HsxPcuEventKind::FrequencyTransition, 0x74),
            HsxPcuEventSpec::new(HsxPcuEventKind::MemoryPhaseShedding, 0x2f),
            HsxPcuEventSpec::new(
                HsxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::InternalProchot),
                0x09,
            ),
            HsxPcuEventSpec::new(
                HsxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::ExternalProchot),
                0x0a,
            ),
        ],
    },
    HsxPcuEventGroup {
        events: [
            HsxPcuEventSpec::new(
                HsxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::VrHot),
                0x42,
            ),
            HsxPcuEventSpec::unused(),
            HsxPcuEventSpec::unused(),
            HsxPcuEventSpec::unused(),
        ],
    },
    HsxPcuEventGroup {
        events: [
            HsxPcuEventSpec::new(HsxPcuEventKind::PackageCState(PcuCoreCState::C0), 0x2a),
            HsxPcuEventSpec::new(HsxPcuEventKind::PackageCState(PcuCoreCState::C3), 0x2c),
            HsxPcuEventSpec::new(HsxPcuEventKind::PackageCState(PcuCoreCState::C6), 0x2d),
            HsxPcuEventSpec::unused(),
        ],
    },
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct HsxPcuMetrics {
    pub clocks: Vec<PcuClockMetrics<PcuPackageScope>>,
    pub core_c_states: Vec<PcuCoreCStateMetrics<PcuPackageScope>>,
    pub frequency_limits: Vec<PcuFrequencyLimitMetrics<PcuPackageScope>>,
    pub frequency_transition: Vec<PcuCycleRatioMetrics<PcuPackageScope, &'static str>>,
    pub memory_phase_shedding: Vec<PcuCycleRatioMetrics<PcuPackageScope, &'static str>>,
    pub package_c_states: Vec<crate::metrics::pcu::PcuPackageCStateMetrics<PcuPackageScope>>,
    pub thermal_throttles: Vec<PcuCycleRatioMetrics<PcuPackageScope, PcuThermalThrottleSource>>,
}

impl HsxPcuMetrics {
    fn from_measurements(
        measurements: BTreeMap<PcuPackageScope, BTreeMap<HsxPcuEventKind, HsxPcuMeasurement>>,
    ) -> Result<Self, String> {
        let mut clocks = Vec::new();
        let mut core_c_states = Vec::new();
        let mut frequency_limits = Vec::new();
        let mut frequency_transition = Vec::new();
        let mut memory_phase_shedding = Vec::new();
        let mut package_c_states = Vec::new();
        let mut thermal_throttles = Vec::new();

        for (scope, measurements) in measurements {
            let clockticks = required_measurement(&measurements, HsxPcuEventKind::Clockticks)?;
            clocks.push(PcuClockMetrics {
                frequency_hz: event_rate(clockticks),
                scope,
            });

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) = measurements.get(&HsxPcuEventKind::CoreCState(c_state)) {
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
                PcuFrequencyLimitReason::IoP,
            ] {
                if let Some(measurement) =
                    measurements.get(&HsxPcuEventKind::FrequencyLimit(reason))
                {
                    frequency_limits.push(PcuFrequencyLimitMetrics {
                        ratio: cycle_ratio(measurement.value, measurement.ticks),
                        reason,
                        scope,
                    });
                }
            }

            if let Some(measurement) = measurements.get(&HsxPcuEventKind::FrequencyTransition) {
                frequency_transition.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "frequency_transition",
                });
            }
            if let Some(measurement) = measurements.get(&HsxPcuEventKind::MemoryPhaseShedding) {
                memory_phase_shedding.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "memory_phase_shedding",
                });
            }

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) =
                    measurements.get(&HsxPcuEventKind::PackageCState(c_state))
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
                    measurements.get(&HsxPcuEventKind::ThermalThrottle(source))
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
pub struct HsxPcuCollector {
    architecture: HsxPcuArchitecture,
    next_group: usize,
    packages: Vec<HsxPcuPackage>,
}

impl HsxPcuCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = HsxPcuArchitecture::from_model(model)
            .ok_or_else(|| format!("HSX/BDX PCU collection is not supported for {model:?}"))?;
        let packages = discover_packages()?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            architecture,
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxPcuMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} PCU measure interval must be non-zero",
                self.architecture.name()
            ));
        }

        let mut measurements = HsxPcuMeasurementAccumulator::new();
        let slice_count = HSX_PCU_EVENT_GROUPS.len();
        let slice_duration = interval.div_f64(slice_count as f64);
        let clock_group = HSX_PCU_EVENT_GROUPS[0];

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
        let multiplexed_groups = &HSX_PCU_EVENT_GROUPS[1..];

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

        HsxPcuMetrics::from_measurements(measurements.into_measurements())
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxPcuUnit {
    cpu: u32,
}

impl HsxPcuUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE)
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE_AND_RESET)
    }

    fn program(self, group: HsxPcuEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(pcu_control_offset(counter_index), counter_control(event))?;
        }
        Ok(())
    }

    fn read(self, group: HsxPcuEventGroup) -> Result<HsxPcuUnitReading, String> {
        let mut counters = BTreeMap::new();
        for (counter_index, event) in group.events.into_iter().enumerate() {
            if !matches!(event.kind, HsxPcuEventKind::Unused) {
                counters.insert(
                    event.kind,
                    mask_counter(
                        Msr::open_readonly(self.cpu)?.read(pcu_counter_offset(counter_index))?,
                    ),
                );
            }
        }
        Ok(HsxPcuUnitReading { counters })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(UNIT_UNFREEZE)
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(PCU_BOX_CONTROL, value)
    }

    fn probe_writable(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        msr.write(PCU_BOX_CONTROL, UNIT_FREEZE)?;
        msr.write(PCU_CONTROL_BASE, 0)?;
        Ok(())
    }
}

#[derive(Debug)]
struct HsxPcuPackage {
    scope: PcuPackageScope,
    unit: HsxPcuUnit,
}

#[derive(Debug)]
struct HsxPcuUnitReading {
    counters: BTreeMap<HsxPcuEventKind, u64>,
}

#[derive(Clone, Copy, Debug)]
struct HsxPcuMeasurement {
    running: Duration,
    ticks: u64,
    value: u64,
}

#[derive(Clone, Copy, Debug)]
struct HsxPcuClockEstimate {
    running: Duration,
    ticks: u64,
}

impl HsxPcuClockEstimate {
    fn ticks_for(self, running: Duration) -> u64 {
        if self.running.is_zero() {
            return 0;
        }

        (self.ticks as f64 * running.as_secs_f64() / self.running.as_secs_f64()) as u64
    }
}

#[derive(Debug, Default)]
struct HsxPcuMeasurementAccumulator {
    measurements: BTreeMap<PcuPackageScope, BTreeMap<HsxPcuEventKind, HsxPcuMeasurement>>,
}

impl HsxPcuMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: PcuPackageScope,
        kind: HsxPcuEventKind,
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
            .or_insert(HsxPcuMeasurement {
                running,
                ticks,
                value,
            });
    }

    fn clock_estimates(&self) -> Result<BTreeMap<PcuPackageScope, HsxPcuClockEstimate>, String> {
        let mut estimates = BTreeMap::new();

        for (scope, measurements) in &self.measurements {
            let clockticks = required_measurement(measurements, HsxPcuEventKind::Clockticks)?;
            estimates.insert(
                *scope,
                HsxPcuClockEstimate {
                    running: clockticks.running,
                    ticks: clockticks.value,
                },
            );
        }

        Ok(estimates)
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<PcuPackageScope, BTreeMap<HsxPcuEventKind, HsxPcuMeasurement>> {
        self.measurements
    }
}

#[derive(Debug)]
pub struct HsxPcuPrometheusMetrics {
    core_c_state_average_cores: Family<PcuPackageCStateLabels, Gauge<f64, AtomicU64>>,
    frequency: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    frequency_limit_ratio: Family<PcuPackageFrequencyLimitLabels, Gauge<f64, AtomicU64>>,
    frequency_transition_ratio: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    memory_phase_shedding_ratio: Family<PcuPackageLabels, Gauge<f64, AtomicU64>>,
    package_c_state_ratio: Family<PcuPackageCStateLabels, Gauge<f64, AtomicU64>>,
    thermal_throttle_ratio: Family<PcuPackageThermalThrottleLabels, Gauge<f64, AtomicU64>>,
}

impl HsxPcuPrometheusMetrics {
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

    pub fn update(&self, metrics: HsxPcuMetrics) {
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

fn counter_control(event: HsxPcuEventSpec) -> u64 {
    if matches!(event.kind, HsxPcuEventKind::Unused) {
        return 0;
    }

    u64::from(event.event) | (u64::from(event.umask) << 8) | (1 << 17) | (1 << 20) | (1 << 22)
}

fn discover_packages() -> Result<Vec<HsxPcuPackage>, String> {
    let packages = package_leaders()?
        .into_iter()
        .map(|(scope, cpu)| HsxPcuPackage {
            scope,
            unit: HsxPcuUnit { cpu },
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any HSX/BDX PCU packages".to_string());
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

fn event_rate(measurement: &HsxPcuMeasurement) -> f64 {
    let elapsed = measurement.running.as_secs_f64();
    if elapsed == 0.0 {
        0.0
    } else {
        measurement.value as f64 / elapsed
    }
}

fn freeze_packages(packages: &[HsxPcuPackage]) -> Result<(), String> {
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

fn probe_writable_msrs(packages: &[HsxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.probe_writable()?;
    }
    Ok(())
}

fn program_packages(packages: &[HsxPcuPackage], group: HsxPcuEventGroup) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
        package.unit.program(group)?;
    }
    Ok(())
}

fn read_packages(
    packages: &[HsxPcuPackage],
    running: Duration,
    group: HsxPcuEventGroup,
    clock_estimates: Option<&BTreeMap<PcuPackageScope, HsxPcuClockEstimate>>,
    measurements: &mut HsxPcuMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let reading = package.unit.read(group)?;
        let ticks = match reading.counters.get(&HsxPcuEventKind::Clockticks) {
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
    measurements: &BTreeMap<HsxPcuEventKind, HsxPcuMeasurement>,
    kind: HsxPcuEventKind,
) -> Result<&HsxPcuMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("PCU measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[HsxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_events_encode_umask() {
        let c6 = HSX_PCU_EVENT_GROUPS[0].events[3];

        assert_eq!(c6.event, 0x80);
        assert_eq!((counter_control(c6) >> 8) & 0xff, 0xc0);
    }

    #[test]
    fn haswell_broadwell_do_not_include_current_limit_event() {
        assert!(!HSX_PCU_EVENT_GROUPS.iter().any(|group| {
            group.events.iter().any(|event| {
                matches!(
                    event.kind,
                    HsxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Current)
                )
            })
        }));
    }

    #[test]
    fn clock_estimate_scales_ticks_to_other_slices() {
        let estimate = HsxPcuClockEstimate {
            running: Duration::from_millis(20),
            ticks: 20_000_000,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 10_000_000);
        assert_eq!(estimate.ticks_for(Duration::from_millis(40)), 40_000_000);
    }

    #[test]
    fn zero_running_clock_estimate_returns_zero_ticks() {
        let estimate = HsxPcuClockEstimate {
            running: Duration::ZERO,
            ticks: 1,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 0);
    }

    #[test]
    fn derives_ratios_from_measurements() {
        let metrics = HsxPcuMetrics::from_measurements(BTreeMap::from([(
            PcuPackageScope { package_id: 0 },
            BTreeMap::from([
                (
                    HsxPcuEventKind::Clockticks,
                    HsxPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 100,
                    },
                ),
                (
                    HsxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::VrHot),
                    HsxPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 40,
                    },
                ),
            ]),
        )]))
        .unwrap();

        assert_eq!(metrics.clocks[0].frequency_hz, 100.0);
        assert_eq!(metrics.thermal_throttles[0].ratio, 0.4);
    }
}
