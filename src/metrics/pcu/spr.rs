use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metal::msr::Msr;
use crate::metrics::pcu::{
    PcuClockMetrics, PcuCoreCState, PcuCoreCStateMetrics, PcuScope, PcuScopeCStateLabels,
    PcuScopeLabels, occupancy_average, register_frequency,
};
use crate::metrics::uncore::skx::{UncoreLeader, UncoreScope, uncore_leaders};

const COUNTER_COUNT: usize = 4;
const PCU_DISCOVERY_BOX_TYPE: u16 = 4;
const PCU_DISCOVERY_ACCESS_TYPE_MSR: u8 = 0;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 9;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 8;
const UNIT_FREEZE_BIT: u64 = 1 << 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SprPcuEventKind {
    Clockticks,
    CoreCState(PcuCoreCState),
    Unused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprPcuEventSpec {
    event: u8,
    kind: SprPcuEventKind,
}

impl SprPcuEventSpec {
    const fn new(kind: SprPcuEventKind, event: u8) -> Self {
        Self { event, kind }
    }

    const fn unused() -> Self {
        Self::new(SprPcuEventKind::Unused, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprPcuEventGroup {
    events: [SprPcuEventSpec; COUNTER_COUNT],
}

const SPR_PCU_EVENT_GROUP: SprPcuEventGroup = SprPcuEventGroup {
    events: [
        SprPcuEventSpec::new(SprPcuEventKind::Clockticks, 0x01),
        SprPcuEventSpec::new(SprPcuEventKind::CoreCState(PcuCoreCState::C0), 0x35),
        SprPcuEventSpec::new(SprPcuEventKind::CoreCState(PcuCoreCState::C6), 0x37),
        SprPcuEventSpec::unused(),
    ],
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct SprPcuMetrics {
    pub clocks: Vec<PcuClockMetrics<PcuScope>>,
    pub core_c_states: Vec<PcuCoreCStateMetrics<PcuScope>>,
}

impl SprPcuMetrics {
    fn from_measurements(
        measurements: BTreeMap<PcuScope, BTreeMap<SprPcuEventKind, SprPcuMeasurement>>,
    ) -> Result<Self, String> {
        let mut clocks = Vec::new();
        let mut core_c_states = Vec::new();

        for (scope, measurements) in measurements {
            let clockticks = required_measurement(&measurements, SprPcuEventKind::Clockticks)?;
            clocks.push(PcuClockMetrics {
                frequency_hz: event_rate(clockticks.value, clockticks.running),
                scope,
            });

            for c_state in [PcuCoreCState::C0, PcuCoreCState::C6] {
                if let Some(measurement) = measurements.get(&SprPcuEventKind::CoreCState(c_state)) {
                    core_c_states.push(PcuCoreCStateMetrics {
                        average_cores: occupancy_average(measurement.value, measurement.ticks),
                        c_state,
                        scope,
                    });
                }
            }
        }

        Ok(Self {
            clocks,
            core_c_states,
        })
    }
}

#[derive(Debug)]
pub struct SprPcuCollector {
    packages: Vec<SprPcuPackage>,
}

impl SprPcuCollector {
    pub fn new() -> Result<Self, String> {
        let packages = discover_packages()?;
        probe_writable_msrs(&packages)?;

        Ok(Self { packages })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SprPcuMetrics, String> {
        if interval.is_zero() {
            return Err("SPR/EMR PCU measure interval must be non-zero".to_string());
        }

        program_packages(&self.packages)?;

        let started_at = Instant::now();
        unfreeze_packages(&self.packages)?;
        tokio::time::sleep(interval).await;
        freeze_packages(&self.packages)?;

        let mut measurements = SprPcuMeasurementAccumulator::new();
        read_packages(&self.packages, started_at.elapsed(), &mut measurements)?;

        SprPcuMetrics::from_measurements(measurements.into_measurements())
    }
}

#[derive(Debug)]
struct SprPcuUnit {
    control_offset: u64,
    counter_offset: u64,
    counter_width: u32,
    cpu: u32,
    unit_control: u64,
}

impl SprPcuUnit {
    fn new(cpu: u32, box_pmu: crate::metrics::uncore::spr::UncoreBoxDiscovery) -> Self {
        Self {
            control_offset: u64::from(box_pmu.control_offset),
            counter_offset: u64::from(box_pmu.counter_offset),
            counter_width: u32::from(box_pmu.bit_width),
            cpu,
            unit_control: box_pmu.box_control,
        }
    }

    fn freeze(&self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE_BIT)
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT)?;
        self.write_unit_control(UNIT_FREEZE_BIT | UNIT_COUNTER_RESET_BIT)
    }

    fn program(&self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for (counter_index, event) in SPR_PCU_EVENT_GROUP.events.into_iter().enumerate() {
            msr.write(
                self.control_address(counter_index),
                u64::from(counter_control(event)),
            )?;
        }
        Ok(())
    }

    fn read(&self) -> Result<BTreeMap<SprPcuEventKind, u64>, String> {
        let mut counters = BTreeMap::new();
        for (counter_index, event) in SPR_PCU_EVENT_GROUP.events.into_iter().enumerate() {
            if !matches!(event.kind, SprPcuEventKind::Unused) {
                counters.insert(
                    event.kind,
                    mask_counter(
                        Msr::open_readonly(self.cpu)?.read(self.counter_address(counter_index))?,
                        self.counter_width,
                    ),
                );
            }
        }
        Ok(counters)
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.write_unit_control(0)
    }

    fn write_unit_control(&self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(self.unit_control, value)
    }

    fn control_address(&self, counter_index: usize) -> u64 {
        self.unit_control + self.control_offset + counter_index as u64
    }

    fn counter_address(&self, counter_index: usize) -> u64 {
        self.unit_control + self.counter_offset + counter_index as u64
    }

    fn probe_writable(&self) -> Result<(), String> {
        self.freeze_and_reset()?;
        self.unfreeze()
    }
}

#[derive(Debug)]
struct SprPcuPackage {
    scope: PcuScope,
    unit: SprPcuUnit,
}

#[derive(Clone, Copy, Debug)]
struct SprPcuMeasurement {
    running: Duration,
    ticks: u64,
    value: u64,
}

#[derive(Debug, Default)]
struct SprPcuMeasurementAccumulator {
    measurements: BTreeMap<PcuScope, BTreeMap<SprPcuEventKind, SprPcuMeasurement>>,
}

impl SprPcuMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: PcuScope,
        kind: SprPcuEventKind,
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
            .or_insert(SprPcuMeasurement {
                running,
                ticks,
                value,
            });
    }

    fn into_measurements(self) -> BTreeMap<PcuScope, BTreeMap<SprPcuEventKind, SprPcuMeasurement>> {
        self.measurements
    }
}

#[derive(Debug)]
pub struct SprPcuPrometheusMetrics {
    core_c_state_average_cores: Family<PcuScopeCStateLabels, Gauge<f64, AtomicU64>>,
    frequency: Family<PcuScopeLabels, Gauge<f64, AtomicU64>>,
}

impl SprPcuPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            core_c_state_average_cores: Family::default(),
            frequency: register_frequency(registry, "Interval-derived PCU PCLK frequency in hertz"),
        };

        registry.register(
            "ocellus_pcu_core_c_state_average_cores",
            "Average number of cores in the PCU-observed core C-state during the interval",
            metrics.core_c_state_average_cores.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SprPcuMetrics) {
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
    }
}

fn counter_control(event: SprPcuEventSpec) -> u32 {
    if matches!(event.kind, SprPcuEventKind::Unused) {
        return 0;
    }

    u32::from(event.event)
}

fn discover_packages() -> Result<Vec<SprPcuPackage>, String> {
    let socket_boxes = crate::metrics::uncore::spr::discover_uncore_boxes(PCU_DISCOVERY_BOX_TYPE)?;
    let leaders = uncore_leaders()?;
    let mut packages = Vec::new();

    for socket in socket_boxes {
        let Some(cpu) = leader_for_scope(&leaders, socket.scope) else {
            continue;
        };

        for box_pmu in socket
            .boxes
            .into_iter()
            .filter(|box_pmu| box_pmu.access_type == PCU_DISCOVERY_ACCESS_TYPE_MSR)
        {
            packages.push(SprPcuPackage {
                scope: PcuScope::from_uncore_scope(socket.scope),
                unit: SprPcuUnit::new(cpu, box_pmu),
            });
        }
    }

    if packages.is_empty() {
        return Err("failed to discover any SPR/EMR PCU boxes from PMU discovery".to_string());
    }

    Ok(packages)
}

fn leader_for_scope(leaders: &[UncoreLeader], scope: UncoreScope) -> Option<u32> {
    leaders
        .iter()
        .find(|leader| leader.scope == scope)
        .map(|leader| leader.cpu)
}

fn event_rate(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();
    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn freeze_packages(packages: &[SprPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }
    Ok(())
}

fn mask_counter(counter: u64, width: u32) -> u64 {
    if width >= 64 {
        counter
    } else {
        counter & ((1_u64 << width) - 1)
    }
}

fn counter_delta(previous: u64, current: u64, width: u32) -> u64 {
    if width >= 64 {
        current.wrapping_sub(previous)
    } else {
        current.wrapping_sub(previous) & ((1_u64 << width) - 1)
    }
}

fn probe_writable_msrs(packages: &[SprPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.probe_writable()?;
    }
    Ok(())
}

fn program_packages(packages: &[SprPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
        package.unit.program()?;
    }
    Ok(())
}

fn read_packages(
    packages: &[SprPcuPackage],
    running: Duration,
    measurements: &mut SprPcuMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let counters = package.unit.read()?;
        let ticks = counters
            .get(&SprPcuEventKind::Clockticks)
            .copied()
            .ok_or_else(|| {
                format!(
                    "PCU clockticks measurement is missing for package {} die_group {:?} die {:?}",
                    package.scope.package_id, package.scope.die_group_id, package.scope.die_id
                )
            })?;

        for (kind, value) in counters {
            let value = counter_delta(0, value, package.unit.counter_width);
            measurements.add(package.scope, kind, value, ticks, running);
        }
    }
    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<SprPcuEventKind, SprPcuMeasurement>,
    kind: SprPcuEventKind,
) -> Result<&SprPcuMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("PCU measurement {kind:?} is missing"))
}

fn unfreeze_packages(packages: &[SprPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainline_spr_emr_events_are_clockticks_c0_and_c6() {
        assert_eq!(SPR_PCU_EVENT_GROUP.events[0].event, 0x01);
        assert_eq!(SPR_PCU_EVENT_GROUP.events[1].event, 0x35);
        assert_eq!(SPR_PCU_EVENT_GROUP.events[2].event, 0x37);
        assert!(matches!(
            SPR_PCU_EVENT_GROUP.events[3].kind,
            SprPcuEventKind::Unused
        ));
    }

    #[test]
    fn spr_emr_msr_counter_control_uses_raw_event_config() {
        let c0 = SPR_PCU_EVENT_GROUP.events[1];

        assert_eq!(counter_control(c0), 0x35);
    }

    #[test]
    fn spr_emr_msr_addresses_use_discovered_box_control_base() {
        let unit = SprPcuUnit {
            control_offset: 0x20,
            counter_offset: 0x08,
            counter_width: 48,
            cpu: 0,
            unit_control: 0x1c00,
        };

        assert_eq!(unit.control_address(0), 0x1c20);
        assert_eq!(unit.control_address(3), 0x1c23);
        assert_eq!(unit.counter_address(0), 0x1c08);
        assert_eq!(unit.counter_address(3), 0x1c0b);
    }

    #[test]
    fn masks_discovered_counter_widths() {
        assert_eq!(mask_counter(u64::MAX, 64), u64::MAX);
        assert_eq!(mask_counter((1_u64 << 50) | 7, 48), 7);
    }

    #[test]
    fn computes_counter_delta_for_discovered_widths() {
        assert_eq!(counter_delta(u64::MAX - 4, 2, 64), 7);
        assert_eq!(counter_delta((1_u64 << 48) - 4, 2, 48), 6);
    }

    #[test]
    fn derives_core_c_state_average() {
        let scope = PcuScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };
        let metrics = SprPcuMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                (
                    SprPcuEventKind::Clockticks,
                    SprPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 100,
                    },
                ),
                (
                    SprPcuEventKind::CoreCState(PcuCoreCState::C6),
                    SprPcuMeasurement {
                        running: Duration::from_secs(1),
                        ticks: 100,
                        value: 300,
                    },
                ),
            ]),
        )]))
        .unwrap();

        assert_eq!(metrics.clocks[0].frequency_hz, 100.0);
        assert_eq!(metrics.core_c_states[0].average_cores, 3.0);
    }
}
