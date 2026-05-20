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
    PcuFrequencyLimitMetrics, PcuFrequencyLimitReason, PcuPackageCStateMetrics, PcuScope,
    PcuScopeCStateLabels, PcuScopeFrequencyLimitLabels, PcuScopeLabels,
    PcuScopeThermalThrottleLabels, PcuThermalThrottleSource, cycle_ratio, occupancy_average,
    register_frequency,
};
use crate::metrics::uncore::skx::{uncore_leaders, wrapping_delta};

const COUNTER_COUNT: usize = 4;
const COUNTER_WIDTH: u32 = 48;
const PCU_COUNTER_BASE: u64 = 0x717;
const PCU_CONTROL_BASE: u64 = 0x711;
const PCU_BOX_CONTROL: u64 = 0x710;
const PCU_FILTER: u64 = 0x715;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 0;
const UNIT_FREEZE_BIT: u64 = 1 << 8;

const UNIT_FREEZE: u64 = UNIT_FREEZE_BIT;
const UNIT_FREEZE_AND_RESET: u64 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT;
const UNIT_UNFREEZE: u64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SkxPcuEventKind {
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
struct SkxPcuEventSpec {
    event: u8,
    kind: SkxPcuEventKind,
    umask: u8,
}

impl SkxPcuEventSpec {
    const fn new(kind: SkxPcuEventKind, event: u8) -> Self {
        Self {
            event,
            kind,
            umask: 0,
        }
    }

    const fn occupancy(c_state: PcuCoreCState, umask: u8) -> Self {
        Self {
            event: 0x80,
            kind: SkxPcuEventKind::CoreCState(c_state),
            umask,
        }
    }

    const fn unused() -> Self {
        Self::new(SkxPcuEventKind::Unused, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SkxPcuEventGroup {
    events: [SkxPcuEventSpec; COUNTER_COUNT],
}

const SKX_PCU_EVENT_GROUPS: [SkxPcuEventGroup; 5] = [
    SkxPcuEventGroup {
        events: [
            SkxPcuEventSpec::new(SkxPcuEventKind::Clockticks, 0x00),
            SkxPcuEventSpec::occupancy(PcuCoreCState::C0, 0x40),
            SkxPcuEventSpec::occupancy(PcuCoreCState::C3, 0x80),
            SkxPcuEventSpec::occupancy(PcuCoreCState::C6, 0xc0),
        ],
    },
    SkxPcuEventGroup {
        events: [
            SkxPcuEventSpec::new(
                SkxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Power),
                0x05,
            ),
            SkxPcuEventSpec::new(
                SkxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::Thermal),
                0x04,
            ),
            SkxPcuEventSpec::new(
                SkxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::IoP),
                0x73,
            ),
            SkxPcuEventSpec::unused(),
        ],
    },
    SkxPcuEventGroup {
        events: [
            SkxPcuEventSpec::new(SkxPcuEventKind::FrequencyTransition, 0x74),
            SkxPcuEventSpec::new(SkxPcuEventKind::MemoryPhaseShedding, 0x2f),
            SkxPcuEventSpec::new(
                SkxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::InternalProchot),
                0x09,
            ),
            SkxPcuEventSpec::new(
                SkxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::ExternalProchot),
                0x0a,
            ),
        ],
    },
    SkxPcuEventGroup {
        events: [
            SkxPcuEventSpec::new(
                SkxPcuEventKind::ThermalThrottle(PcuThermalThrottleSource::VrHot),
                0x42,
            ),
            SkxPcuEventSpec::unused(),
            SkxPcuEventSpec::unused(),
            SkxPcuEventSpec::unused(),
        ],
    },
    SkxPcuEventGroup {
        events: [
            SkxPcuEventSpec::new(SkxPcuEventKind::PackageCState(PcuCoreCState::C0), 0x2a),
            SkxPcuEventSpec::new(SkxPcuEventKind::PackageCState(PcuCoreCState::C3), 0x2c),
            SkxPcuEventSpec::new(SkxPcuEventKind::PackageCState(PcuCoreCState::C6), 0x2d),
            SkxPcuEventSpec::unused(),
        ],
    },
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct SkxPcuMetrics {
    pub clocks: Vec<PcuClockMetrics<PcuScope>>,
    pub core_c_states: Vec<PcuCoreCStateMetrics<PcuScope>>,
    pub frequency_limits: Vec<PcuFrequencyLimitMetrics<PcuScope>>,
    pub frequency_transition: Vec<PcuCycleRatioMetrics<PcuScope, &'static str>>,
    pub memory_phase_shedding: Vec<PcuCycleRatioMetrics<PcuScope, &'static str>>,
    pub package_c_states: Vec<PcuPackageCStateMetrics<PcuScope>>,
    pub thermal_throttles: Vec<PcuCycleRatioMetrics<PcuScope, PcuThermalThrottleSource>>,
}

impl SkxPcuMetrics {
    fn from_measurements(
        measurements: BTreeMap<PcuScope, BTreeMap<SkxPcuEventKind, SkxPcuMeasurement>>,
    ) -> Result<Self, String> {
        let mut clocks = Vec::new();
        let mut core_c_states = Vec::new();
        let mut frequency_limits = Vec::new();
        let mut frequency_transition = Vec::new();
        let mut memory_phase_shedding = Vec::new();
        let mut package_c_states = Vec::new();
        let mut thermal_throttles = Vec::new();

        for (scope, measurements) in measurements {
            let clockticks = required_measurement(&measurements, SkxPcuEventKind::Clockticks)?;
            clocks.push(PcuClockMetrics {
                frequency_hz: event_rate(clockticks),
                scope,
            });

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) = measurements.get(&SkxPcuEventKind::CoreCState(c_state)) {
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
                PcuFrequencyLimitReason::IoP,
            ] {
                if let Some(measurement) =
                    measurements.get(&SkxPcuEventKind::FrequencyLimit(reason))
                {
                    frequency_limits.push(PcuFrequencyLimitMetrics {
                        ratio: cycle_ratio(measurement.value, measurement.ticks),
                        reason,
                        scope,
                    });
                }
            }

            if let Some(measurement) = measurements.get(&SkxPcuEventKind::FrequencyTransition) {
                frequency_transition.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "frequency_transition",
                });
            }
            if let Some(measurement) = measurements.get(&SkxPcuEventKind::MemoryPhaseShedding) {
                memory_phase_shedding.push(PcuCycleRatioMetrics {
                    ratio: cycle_ratio(measurement.value, measurement.ticks),
                    scope,
                    source: "memory_phase_shedding",
                });
            }

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C3, PcuCoreCState::C6] {
                if let Some(measurement) =
                    measurements.get(&SkxPcuEventKind::PackageCState(c_state))
                {
                    package_c_states.push(PcuPackageCStateMetrics {
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
                    measurements.get(&SkxPcuEventKind::ThermalThrottle(source))
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
pub struct SkxPcuCollector {
    next_group: usize,
    packages: Vec<SkxPcuPackage>,
}

impl SkxPcuCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            next_group: 0,
            packages,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SkxPcuMetrics, String> {
        if interval.is_zero() {
            return Err("Skylake/Cascade Lake PCU measure interval must be non-zero".to_string());
        }

        let mut measurements = SkxPcuMeasurementAccumulator::new();
        let slice_count = SKX_PCU_EVENT_GROUPS.len();
        let slice_duration = interval.div_f64(slice_count as f64);
        let clock_group = SKX_PCU_EVENT_GROUPS[0];

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
        let multiplexed_groups = &SKX_PCU_EVENT_GROUPS[1..];

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

        SkxPcuMetrics::from_measurements(measurements.into_measurements())
    }
}

#[derive(Clone, Copy, Debug)]
struct SkxPcuUnit {
    cpu: u32,
}

impl SkxPcuUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE)
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE_AND_RESET)
    }

    fn program(self, group: SkxPcuEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        msr.write(PCU_FILTER, 0)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(pcu_control_offset(counter_index), counter_control(event))?;
        }
        Ok(())
    }

    fn read(self, group: SkxPcuEventGroup) -> Result<SkxPcuUnitReading, String> {
        let mut counters = BTreeMap::new();
        for (counter_index, event) in group.events.into_iter().enumerate() {
            if !matches!(event.kind, SkxPcuEventKind::Unused) {
                counters.insert(
                    event.kind,
                    mask_counter(
                        Msr::open_readonly(self.cpu)?.read(pcu_counter_offset(counter_index))?,
                    ),
                );
            }
        }
        Ok(SkxPcuUnitReading { counters })
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
        msr.write(PCU_FILTER, 0)?;
        msr.write(PCU_CONTROL_BASE, 0)?;
        Ok(())
    }
}

#[derive(Debug)]
struct SkxPcuPackage {
    scope: PcuScope,
    unit: SkxPcuUnit,
}

#[derive(Debug)]
struct SkxPcuUnitReading {
    counters: BTreeMap<SkxPcuEventKind, u64>,
}

#[derive(Clone, Copy, Debug)]
struct SkxPcuMeasurement {
    running: Duration,
    ticks: u64,
    value: u64,
}

#[derive(Clone, Copy, Debug)]
struct SkxPcuClockEstimate {
    running: Duration,
    ticks: u64,
}

impl SkxPcuClockEstimate {
    fn ticks_for(self, running: Duration) -> u64 {
        if self.running.is_zero() {
            return 0;
        }

        (self.ticks as f64 * running.as_secs_f64() / self.running.as_secs_f64()) as u64
    }
}

#[derive(Debug, Default)]
struct SkxPcuMeasurementAccumulator {
    measurements: BTreeMap<PcuScope, BTreeMap<SkxPcuEventKind, SkxPcuMeasurement>>,
}

impl SkxPcuMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: PcuScope,
        kind: SkxPcuEventKind,
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
            .or_insert(SkxPcuMeasurement {
                running,
                ticks,
                value,
            });
    }

    fn clock_estimates(&self) -> Result<BTreeMap<PcuScope, SkxPcuClockEstimate>, String> {
        let mut estimates = BTreeMap::new();

        for (scope, measurements) in &self.measurements {
            let clockticks = required_measurement(measurements, SkxPcuEventKind::Clockticks)?;
            estimates.insert(
                *scope,
                SkxPcuClockEstimate {
                    running: clockticks.running,
                    ticks: clockticks.value,
                },
            );
        }

        Ok(estimates)
    }

    fn into_measurements(self) -> BTreeMap<PcuScope, BTreeMap<SkxPcuEventKind, SkxPcuMeasurement>> {
        self.measurements
    }
}

#[derive(Debug)]
pub struct SkxPcuPrometheusMetrics {
    core_c_state_average_cores: Family<PcuScopeCStateLabels, Gauge<f64, AtomicU64>>,
    frequency: Family<PcuScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_limit_ratio: Family<PcuScopeFrequencyLimitLabels, Gauge<f64, AtomicU64>>,
    frequency_transition_ratio: Family<PcuScopeLabels, Gauge<f64, AtomicU64>>,
    memory_phase_shedding_ratio: Family<PcuScopeLabels, Gauge<f64, AtomicU64>>,
    package_c_state_ratio: Family<PcuScopeCStateLabels, Gauge<f64, AtomicU64>>,
    thermal_throttle_ratio: Family<PcuScopeThermalThrottleLabels, Gauge<f64, AtomicU64>>,
}

impl SkxPcuPrometheusMetrics {
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

    pub fn update(&self, metrics: SkxPcuMetrics) {
        for metric in metrics.clocks {
            self.frequency
                .get_or_create(&PcuScopeLabels::new(metric.scope))
                .set(metric.frequency_hz);
        }
        for metric in metrics.core_c_states {
            self.core_c_state_average_cores
                .get_or_create(&PcuScopeCStateLabels::new(metric.scope, metric.c_state))
                .set(metric.average_cores);
        }
        for metric in metrics.frequency_limits {
            self.frequency_limit_ratio
                .get_or_create(&PcuScopeFrequencyLimitLabels::new(
                    metric.scope,
                    metric.reason,
                ))
                .set(metric.ratio);
        }
        for metric in metrics.frequency_transition {
            self.frequency_transition_ratio
                .get_or_create(&PcuScopeLabels::new(metric.scope))
                .set(metric.ratio);
        }
        for metric in metrics.memory_phase_shedding {
            self.memory_phase_shedding_ratio
                .get_or_create(&PcuScopeLabels::new(metric.scope))
                .set(metric.ratio);
        }
        for metric in metrics.package_c_states {
            self.package_c_state_ratio
                .get_or_create(&PcuScopeCStateLabels::new(metric.scope, metric.c_state))
                .set(metric.ratio);
        }
        for metric in metrics.thermal_throttles {
            self.thermal_throttle_ratio
                .get_or_create(&PcuScopeThermalThrottleLabels::new(
                    metric.scope,
                    metric.source,
                ))
                .set(metric.ratio);
        }
    }
}

fn counter_control(event: SkxPcuEventSpec) -> u64 {
    if matches!(event.kind, SkxPcuEventKind::Unused) {
        return 0;
    }

    u64::from(event.event) | (u64::from(event.umask) << 8) | (1 << 17) | (1 << 20) | (1 << 22)
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<SkxPcuPackage>, String> {
    if !matches!(model, IntelServerCpuModel::SkylakeXeon) {
        return Err(format!("PCU collection is not supported for {model:?}"));
    }

    let packages = uncore_leaders()?
        .into_iter()
        .map(|leader| SkxPcuPackage {
            scope: PcuScope::from_uncore_scope(leader.scope),
            unit: SkxPcuUnit { cpu: leader.cpu },
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any SKX/CLX PCU packages".to_string());
    }

    Ok(packages)
}

fn event_rate(measurement: &SkxPcuMeasurement) -> f64 {
    let elapsed = measurement.running.as_secs_f64();
    if elapsed == 0.0 {
        0.0
    } else {
        measurement.value as f64 / elapsed
    }
}

fn freeze_packages(packages: &[SkxPcuPackage]) -> Result<(), String> {
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

fn probe_writable_msrs(packages: &[SkxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.probe_writable()?;
    }
    Ok(())
}

fn program_packages(packages: &[SkxPcuPackage], group: SkxPcuEventGroup) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
        package.unit.program(group)?;
    }
    Ok(())
}

fn read_packages(
    packages: &[SkxPcuPackage],
    running: Duration,
    group: SkxPcuEventGroup,
    clock_estimates: Option<&BTreeMap<PcuScope, SkxPcuClockEstimate>>,
    measurements: &mut SkxPcuMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let reading = package.unit.read(group)?;
        let ticks = match reading.counters.get(&SkxPcuEventKind::Clockticks) {
            Some(clockticks) => wrapping_delta(0, *clockticks, COUNTER_WIDTH),
            None => clock_estimates
                .and_then(|estimates| estimates.get(&package.scope))
                .ok_or_else(|| {
                    format!(
                        "PCU clock estimate is missing for package {} die_group {} die {}",
                        package.scope.package_id, package.scope.die_group_id, package.scope.die_id
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
    measurements: &BTreeMap<SkxPcuEventKind, SkxPcuMeasurement>,
    kind: SkxPcuEventKind,
) -> Result<&SkxPcuMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("PCU measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[SkxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_control_does_not_use_freeze_enable_bit() {
        assert_eq!(UNIT_FREEZE, UNIT_FREEZE_BIT);
        assert_eq!(
            UNIT_FREEZE_AND_RESET,
            UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT
        );
        assert_eq!(UNIT_UNFREEZE, 0);
    }

    #[test]
    fn occupancy_events_encode_umask() {
        let c6 = SKX_PCU_EVENT_GROUPS[0].events[3];

        assert_eq!(c6.event, 0x80);
        assert_eq!((counter_control(c6) >> 8) & 0xff, 0xc0);
    }

    #[test]
    fn unverified_frequency_limit_events_are_not_collected() {
        for group in SKX_PCU_EVENT_GROUPS {
            for event in group.events {
                assert!(!matches!(
                    event.kind,
                    SkxPcuEventKind::FrequencyLimit(
                        PcuFrequencyLimitReason::Current
                            | PcuFrequencyLimitReason::Os
                            | PcuFrequencyLimitReason::PerfP
                    )
                ));
            }
        }
    }

    #[test]
    fn mcp_prochot_is_not_exported_as_os_frequency_limit() {
        for group in SKX_PCU_EVENT_GROUPS {
            for event in group.events {
                assert_ne!(event.event, 0x06);
            }
        }
    }

    #[test]
    fn clock_estimate_scales_ticks_to_other_slices() {
        let estimate = SkxPcuClockEstimate {
            running: Duration::from_millis(20),
            ticks: 22_000_000,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 11_000_000);
        assert_eq!(estimate.ticks_for(Duration::from_millis(40)), 44_000_000);
    }

    #[test]
    fn zero_running_clock_estimate_returns_zero_ticks() {
        let estimate = SkxPcuClockEstimate {
            running: Duration::ZERO,
            ticks: 1,
        };

        assert_eq!(estimate.ticks_for(Duration::from_millis(10)), 0);
    }

    #[test]
    fn derives_ratios_from_measurements() {
        let scope = PcuScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        };
        let metrics = SkxPcuMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                (
                    SkxPcuEventKind::Clockticks,
                    SkxPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 100,
                    },
                ),
                (
                    SkxPcuEventKind::CoreCState(PcuCoreCState::C0),
                    SkxPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 250,
                    },
                ),
                (
                    SkxPcuEventKind::FrequencyLimit(PcuFrequencyLimitReason::IoP),
                    SkxPcuMeasurement {
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
