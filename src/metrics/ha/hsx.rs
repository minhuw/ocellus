use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metrics::common::BYTES_PER_CACHE_LINE;
use crate::metrics::uncore::hsx::{
    self, HsxConfigBox, HsxUncoreScope, HsxUncoreSpec, HsxUncoreUnit, events_per_second,
    measurement_round_count, scale_to_enabled,
};
use crate::pmu_config::types::{BoxType, CountAccumulator, PmuConfig, UncoreCounts};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

const HA_CONFIG_BOX: HsxConfigBox = HsxConfigBox {
    box_type: BoxType::Ha,
    name: "HA",
    unit: "unit",
    fixed: false,
};

const HA_EVENT_GROUPS: [HsxHaEventGroup; 1] = [HsxHaEventGroup {
    events: [
        HsxHaEventSpec::new(HsxHaEventKind::LocalRead, 0x01, 0x01),
        HsxHaEventSpec::new(HsxHaEventKind::RemoteRead, 0x01, 0x02),
        HsxHaEventSpec::new(HsxHaEventKind::LocalWrite, 0x01, 0x04),
        HsxHaEventSpec::new(HsxHaEventKind::RemoteWrite, 0x01, 0x08),
    ],
}];

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct HsxHaMetricsByScope {
    pub local_read_bytes_per_second: f64,
    pub local_read_ratio: f64,
    pub local_write_bytes_per_second: f64,
    pub local_write_ratio: f64,
    pub remote_read_bytes_per_second: f64,
    pub remote_write_bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: HsxUncoreScope,
}

impl HsxHaMetricsByScope {
    fn from_measurements(
        scope: HsxUncoreScope,
        measurements: &BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>,
    ) -> Result<Self, String> {
        let local_read = required_measurement(measurements, HsxHaEventKind::LocalRead)?;
        let remote_read = required_measurement(measurements, HsxHaEventKind::RemoteRead)?;
        let local_write = required_measurement(measurements, HsxHaEventKind::LocalWrite)?;
        let remote_write = required_measurement(measurements, HsxHaEventKind::RemoteWrite)?;
        let read_count = scaled_count(local_read) + scaled_count(remote_read);
        let write_count = scaled_count(local_write) + scaled_count(remote_write);

        Ok(Self {
            local_read_bytes_per_second: bytes_per_second(local_read),
            local_read_ratio: hsx::ratio(scaled_count(local_read), read_count),
            local_write_bytes_per_second: bytes_per_second(local_write),
            local_write_ratio: hsx::ratio(scaled_count(local_write), write_count),
            remote_read_bytes_per_second: bytes_per_second(remote_read),
            remote_write_bytes_per_second: bytes_per_second(remote_write),
            scope,
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HsxHaMetrics {
    pub scopes: Vec<HsxHaMetricsByScope>,
}

impl HsxHaMetrics {
    fn from_measurements(
        measurements: BTreeMap<HsxUncoreScope, BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>>,
    ) -> Result<Self, String> {
        measurements
            .into_iter()
            .map(|(scope, scope_measurements)| {
                HsxHaMetricsByScope::from_measurements(scope, &scope_measurements)
            })
            .collect::<Result<_, _>>()
            .map(|scopes| Self { scopes })
    }
}

#[derive(Debug)]
pub struct HsxHaCollector {
    units: Vec<HsxHaUnit>,
    config: Option<CountAccumulator>,
    next_group: usize,
}

impl HsxHaCollector {
    /// The home agents of each package that PMU configs can program.
    /// Unit id: 0 for D18:F1, 1 for D18:F5.
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        hsx::unit_map(self.units.iter().map(HsxHaUnit::config_unit))
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        // The default sample resets and programs every home agent on every
        // slice.
        let Some(config) = config else {
            self.config = None;
            return Ok(());
        };
        HA_CONFIG_BOX.check_config(&config.program, &self.config_units())?;
        self.config = Some(CountAccumulator::new(config));

        Ok(())
    }

    /// Samples the set config for `interval`. Its counts are cumulative since
    /// the config was set.
    pub async fn sample_config(&mut self, interval: Duration) -> Result<UncoreCounts, String> {
        let accumulator = self
            .config
            .as_mut()
            .ok_or_else(|| "no PMU config is set for HSX HA".to_string())?;
        let units = self
            .units
            .iter()
            .map(|unit| (unit.config_unit(), &unit.unit))
            .collect::<Vec<_>>();

        HA_CONFIG_BOX
            .sample_config(&units, accumulator, interval)
            .await
    }
}

impl HsxHaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        Ok(Self {
            units: discover_units(architecture.intel_server_model())?,
            config: None,
            next_group: 0,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxHaMetrics, String> {
        if interval.is_zero() {
            return Err("HSX HA measure interval must be non-zero".to_string());
        }

        let mut measurements = HsxHaMeasurementAccumulator::new();

        for slice in self.schedule(interval) {
            program_units(&self.units, slice.group)?;

            let started_at = Instant::now();
            unfreeze_units(&self.units)?;
            tokio::time::sleep(slice.duration).await;
            freeze_units(&self.units)?;

            read_units(
                &self.units,
                HsxHaMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        HsxHaMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % HA_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<HsxHaMeasurementSlice> {
        let group_count = HA_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(HsxHaMeasurementSlice {
                    duration: slice_duration,
                    group: HA_EVENT_GROUPS[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct HsxHaPrometheusMetrics {
    local_read_bytes_per_second: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
    local_read_ratio: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
    local_write_bytes_per_second: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
    local_write_ratio: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
    remote_read_bytes_per_second: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
    remote_write_bytes_per_second: Family<HsxHaScopeLabels, Gauge<f64, AtomicU64>>,
}

impl HsxHaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            local_read_bytes_per_second: Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            local_read_ratio: Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            local_write_bytes_per_second:
                Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            local_write_ratio: Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            remote_read_bytes_per_second:
                Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            remote_write_bytes_per_second:
                Family::<HsxHaScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_haswell_ha_local_read_bytes_per_second",
            "Interval-derived Haswell HA local read request bandwidth in bytes per second",
            metrics.local_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_read_ratio",
            "Haswell HA local read request ratio",
            metrics.local_read_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_write_bytes_per_second",
            "Interval-derived Haswell HA local write request bandwidth in bytes per second",
            metrics.local_write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_write_ratio",
            "Haswell HA local write request ratio",
            metrics.local_write_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_remote_read_bytes_per_second",
            "Interval-derived Haswell HA remote read request bandwidth in bytes per second",
            metrics.remote_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_remote_write_bytes_per_second",
            "Interval-derived Haswell HA remote write request bandwidth in bytes per second",
            metrics.remote_write_bytes_per_second.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: HsxHaMetrics) {
        for scope in metrics.scopes {
            let labels = HsxHaScopeLabels::from_scope(scope.scope);

            self.local_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.local_read_bytes_per_second);
            self.local_read_ratio
                .get_or_create(&labels)
                .set(scope.local_read_ratio);
            self.local_write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.local_write_bytes_per_second);
            self.local_write_ratio
                .get_or_create(&labels)
                .set(scope.local_write_ratio);
            self.remote_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.remote_read_bytes_per_second);
            self.remote_write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.remote_write_bytes_per_second);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct HsxHaScopeLabels {
    package: String,
}

impl HsxHaScopeLabels {
    fn from_scope(scope: HsxUncoreScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct HsxHaUnit {
    /// Index in `HA_UNITS`, kept when the other home agent is absent.
    id: u16,
    scope: HsxUncoreScope,
    unit: HsxUncoreUnit,
}

impl HsxHaUnit {
    fn new(
        id: u16,
        location: metal::pci::PciLocation,
        scope: HsxUncoreScope,
    ) -> Result<Self, String> {
        Ok(Self {
            id,
            scope,
            unit: HsxUncoreUnit::new(location)?,
        })
    }

    /// The home agent's package and unit id in PMU configs.
    fn config_unit(&self) -> (u16, u16) {
        (self.scope.package_id as u16, self.id)
    }

    fn program(&self, group: HsxHaEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            self.unit
                .program_counter(counter_index, event.event, event.umask)?;
        }

        Ok(())
    }

    fn read(&self) -> Result<HsxHaUnitReading, String> {
        Ok(HsxHaUnitReading {
            counters: [
                self.unit.read_counter(0)?,
                self.unit.read_counter(1)?,
                self.unit.read_counter(2)?,
                self.unit.read_counter(3)?,
            ],
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxHaUnitReading {
    counters: [u64; hsx::COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxHaEventGroup {
    events: [HsxHaEventSpec; hsx::COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxHaEventSpec {
    event: u8,
    kind: HsxHaEventKind,
    umask: u8,
}

impl HsxHaEventSpec {
    const fn new(kind: HsxHaEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxHaEventKind {
    LocalRead,
    LocalWrite,
    RemoteRead,
    RemoteWrite,
}

#[derive(Clone, Copy, Debug)]
struct HsxHaEventMeasurement {
    enabled: Duration,
    running: Duration,
    value: u64,
}

impl HsxHaEventMeasurement {
    fn add(&mut self, value: u64, running: Duration) {
        self.running += running;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxHaMeasurement {
    enabled: Duration,
    group: HsxHaEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxHaMeasurementSlice {
    duration: Duration,
    group: HsxHaEventGroup,
}

#[derive(Debug, Default)]
struct HsxHaMeasurementAccumulator {
    measurements: BTreeMap<HsxUncoreScope, BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>>,
}

impl HsxHaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: HsxUncoreScope,
        kind: HsxHaEventKind,
        value: u64,
        measurement: HsxHaMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| event_measurement.add(value, measurement.running))
            .or_insert(HsxHaEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<HsxUncoreScope, BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>> {
        self.measurements
    }
}

fn discover_units(model: IntelServerCpuModel) -> Result<Vec<HsxHaUnit>, String> {
    let spec = HsxUncoreSpec::from_model(model)
        .ok_or_else(|| format!("HSX HA collection is not supported for {model:?}"))?;
    let bus_scopes = hsx::bus_scopes(spec)?;
    let mut units = Vec::with_capacity(bus_scopes.len() * metal::arch::hsx::pci::HA_UNITS.len());

    for bus_scope in bus_scopes {
        for (id, location) in
            hsx::present_units(&metal::arch::hsx::pci::HA_UNITS, |(device, function)| {
                metal::pci::find_intel_device_at_address_on_bus(device, function, bus_scope.bus)
                    .ok()
            })
        {
            units.push(HsxHaUnit::new(id, location, bus_scope.scope)?);
        }
    }

    if units.is_empty() {
        return Err(format!("failed to discover any {} HA units", spec.name));
    }

    Ok(units)
}

fn program_units(units: &[HsxHaUnit], group: HsxHaEventGroup) -> Result<(), String> {
    for unit in units {
        unit.unit.freeze_and_reset()?;
    }

    for unit in units {
        unit.program(group)?;
    }

    Ok(())
}

fn read_units(
    units: &[HsxHaUnit],
    measurement: HsxHaMeasurement,
    measurements: &mut HsxHaMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<HsxUncoreScope, Vec<HsxHaUnitReading>>::new();

    for unit in units {
        readings.entry(unit.scope).or_default().push(unit.read()?);
    }

    for (scope, unit_readings) in readings {
        for counter_index in 0..measurement.group.events.len() {
            let event = measurement.group.events[counter_index];
            let value = unit_readings
                .iter()
                .map(|reading| reading.counters[counter_index])
                .sum();

            measurements.add(scope, event.kind, value, measurement);
        }
    }

    Ok(())
}

fn freeze_units(units: &[HsxHaUnit]) -> Result<(), String> {
    for unit in units {
        unit.unit.freeze()?;
    }

    Ok(())
}

fn unfreeze_units(units: &[HsxHaUnit]) -> Result<(), String> {
    for unit in units {
        unit.unit.unfreeze()?;
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>,
    kind: HsxHaEventKind,
) -> Result<&HsxHaEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("HSX HA measurement {kind:?} is missing"))
}

fn bytes_per_second(measurement: &HsxHaEventMeasurement) -> f64 {
    events_per_second(scaled_count(measurement), measurement.enabled) * BYTES_PER_CACHE_LINE
}

fn scaled_count(measurement: &HsxHaEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::uncore::hsx::config_fixtures::*;
    use crate::pmu_config::types::{ChaFilters, CountUnit, CounterSlot, Grouping};

    #[test]
    fn computes_hsx_ha_metrics() {
        let metrics = HsxHaMetrics::from_measurements(BTreeMap::from([(
            test_scope(),
            BTreeMap::from([
                measurement(HsxHaEventKind::LocalRead, 100, 100),
                measurement(HsxHaEventKind::RemoteRead, 300, 100),
                measurement(HsxHaEventKind::LocalWrite, 200, 100),
                measurement(HsxHaEventKind::RemoteWrite, 200, 100),
            ]),
        )]))
        .unwrap();

        let ha = metrics.scopes[0];

        assert_eq!(ha.local_read_bytes_per_second, 64_000.0);
        assert_eq!(ha.remote_read_bytes_per_second, 192_000.0);
        assert_eq!(ha.local_write_bytes_per_second, 128_000.0);
        assert_eq!(ha.remote_write_bytes_per_second, 128_000.0);
        assert_eq!(ha.local_read_ratio, 0.25);
        assert_eq!(ha.local_write_ratio, 0.5);
    }

    #[test]
    fn sums_events_over_the_units_of_a_package() {
        let mut measurements = HsxHaMeasurementAccumulator::new();
        let measurement = HsxHaMeasurement {
            enabled: Duration::from_millis(100),
            group: HA_EVENT_GROUPS[0],
            running: Duration::from_millis(50),
        };

        for value in [10, 30] {
            measurements.add(test_scope(), HsxHaEventKind::LocalRead, value, measurement);
        }
        let measurements = measurements.into_measurements();
        let local_read = measurements[&test_scope()][&HsxHaEventKind::LocalRead];

        assert_eq!(local_read.value, 40);
        assert_eq!(local_read.running, Duration::from_millis(100));
        assert_eq!(local_read.enabled, Duration::from_millis(100));
    }

    #[test]
    fn schedules_the_request_group_in_bounded_slices() {
        let collector = test_collector();

        let slices = collector.schedule(Duration::from_millis(100));
        assert_eq!(
            slices,
            vec![HsxHaMeasurementSlice {
                duration: Duration::from_millis(100),
                group: HA_EVENT_GROUPS[0],
            }]
        );

        let slices = collector.schedule(Duration::from_secs(1));
        assert_eq!(slices.len(), 10);
        assert!(
            slices
                .iter()
                .all(|slice| slice.duration == Duration::from_millis(100)
                    && slice.group == HA_EVENT_GROUPS[0])
        );
    }

    #[test]
    fn programs_three_counters_on_unit_1() {
        let program = snoop_program();
        let ids = [(0, 0), (0, 1), (1, 0), (1, 1)];
        let slices = hsx::config_schedule(&program, Duration::from_secs(1));

        assert_eq!(
            hsx::slice_units(&ids, &program, &slices[0]).unwrap(),
            vec![(1, 0)]
        );
        assert_eq!(
            HA_CONFIG_BOX.unit_writes(&program.groups[0]).unwrap(),
            vec![
                (0xf4, 0x3_0103),
                (0xd8, 0x42_0121),
                (0xdc, 0x42_0221),
                (0xe0, 0),
                (0xe4, 0x42_1001),
            ]
        );
    }

    #[test]
    fn numbers_home_agents_by_function() {
        // D18:F1 is absent.
        let present = hsx::present_units(&metal::arch::hsx::pci::HA_UNITS, |(device, function)| {
            (function != 1).then_some(metal::pci::PciLocation {
                bus: 0x7f,
                device,
                function,
                group: 0,
            })
        });
        assert_eq!(
            present
                .iter()
                .map(|(id, location)| (*id, location.device, location.function))
                .collect::<Vec<_>>(),
            vec![(1, 18, 5)]
        );

        let mut collector = test_collector();
        collector.units = vec![
            test_unit(0, 0, &[0; 0x100]),
            test_unit(0, 1, &[0; 0x100]),
            test_unit(1, 1, &[0; 0x100]),
        ];
        assert_eq!(
            collector.config_units(),
            BTreeMap::from([(0, BTreeSet::from([0, 1])), (1, BTreeSet::from([1]))])
        );
    }

    #[test]
    fn rejects_configs_home_agents_cannot_count() {
        let host = BTreeMap::from([(0, BTreeSet::from([0, 1])), (1, BTreeSet::from([1]))]);
        HA_CONFIG_BOX.check_config(&snoop_program(), &host).unwrap();

        let mut imc = snoop_program();
        imc.box_type = BoxType::Imc;
        let mut package = snoop_program();
        package.groups[0].units.insert(2, BTreeSet::from([0]));
        let mut unit = snoop_program();
        unit.groups[0].units.insert(1, BTreeSet::from([0]));
        let mut fixed = snoop_program();
        fixed.groups[0]
            .counters
            .push(config_counter(CounterSlot::Fixed, 0x40_0000));
        let mut slot = snoop_program();
        slot.groups[0]
            .counters
            .push(config_counter(CounterSlot::General(4), 0x40_0121));
        let mut filters = snoop_program();
        filters.groups[0].filters = Some(ChaFilters {
            filter0: 0,
            filter1: 0,
        });

        for (program, error) in [
            (imc, "Haswell-EP/Broadwell-EP HA cannot count a imc config"),
            (
                package,
                "HA config group 0 runs on package 2, which has no HA units on this host",
            ),
            (
                unit,
                "HA config group 0 runs on unit 0 of package 1, which this host does not have",
            ),
            (
                fixed,
                "HA config group 0: Haswell-EP/Broadwell-EP HA units have no counter slot fixed",
            ),
            (
                slot,
                "HA config group 0: Haswell-EP/Broadwell-EP HA units have no counter slot 4",
            ),
            (
                filters,
                "HA config group 0 has filters, which HA units do not have",
            ),
        ] {
            assert_eq!(
                HA_CONFIG_BOX.check_config(&program, &host),
                Err(error.to_string())
            );
        }
    }

    #[test]
    fn counts_a_config_on_its_home_agents_and_restarts_when_it_changes() {
        let mut config_space = [0_u8; 0x100];
        for (offset, value) in [(0xa0, 7_u64), (0xa8, 9), (0xb8, (0xff << 48) | 11)] {
            config_space[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        for offset in [0xe0, 0xf0] {
            config_space[offset..offset + 4].copy_from_slice(&0x40_0001_u32.to_le_bytes());
        }
        let mut collector = test_collector();
        collector.units = vec![
            test_unit(0, 0, &config_space),
            test_unit(0, 1, &config_space),
        ];
        let runtime = tokio::runtime::Runtime::new().unwrap();

        collector.set_config(Some(config(snoop_program()))).unwrap();
        for factor in [1, 2] {
            let counts = runtime
                .block_on(collector.sample_config(Duration::from_millis(1)))
                .unwrap();
            assert_eq!(
                count_values(&counts),
                vec![
                    (0, CountUnit::Box(1), CounterSlot::General(0), 7 * factor),
                    (0, CountUnit::Box(1), CounterSlot::General(1), 9 * factor),
                    (0, CountUnit::Box(1), CounterSlot::General(3), 11 * factor),
                ]
            );
        }

        let registers = |unit: &HsxHaUnit| {
            [0xf4, 0xd8, 0xdc, 0xe0, 0xe4, 0xf0].map(|offset| unit.unit.read_dword(offset).unwrap())
        };
        // Unit 1 ends frozen with the group's controls and its F0 untouched;
        // unit 0 is in no group.
        assert_eq!(
            registers(&collector.units[1]),
            [0x3_0100, 0x42_0121, 0x42_0221, 0, 0x42_1001, 0x40_0001]
        );
        assert_eq!(
            registers(&collector.units[0]),
            [0, 0, 0, 0x40_0001, 0, 0x40_0001]
        );

        collector.set_config(Some(config(snoop_program()))).unwrap();
        let restarted = collector.config.as_ref().unwrap().counts();
        assert!(count_values(&restarted).iter().all(|count| count.3 == 0));
        let counts = runtime
            .block_on(collector.sample_config(Duration::from_millis(1)))
            .unwrap();
        assert_eq!(count_values(&counts)[0].3, 7);

        collector.set_config(None).unwrap();
        assert!(!collector.has_config());
    }

    /// Snoop responses RspI and RspS, and local InvItoE requests, on home
    /// agent 1 of package 0.
    fn snoop_program() -> crate::pmu_config::types::ProgrammedBoxType {
        config_program(
            BoxType::Ha,
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x40_0121),
                    config_counter(CounterSlot::General(1), 0x40_0221),
                    config_counter(CounterSlot::General(3), 0x40_1001),
                ],
                BTreeMap::from([(0, BTreeSet::from([1]))]),
            )],
        )
    }

    fn test_unit(package_id: u32, id: u16, config_space: &[u8]) -> HsxHaUnit {
        HsxHaUnit {
            id,
            scope: HsxUncoreScope { package_id },
            unit: HsxUncoreUnit::from_config_space(config_space),
        }
    }

    fn measurement(
        kind: HsxHaEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (HsxHaEventKind, HsxHaEventMeasurement) {
        (
            kind,
            HsxHaEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn test_collector() -> HsxHaCollector {
        HsxHaCollector {
            units: Vec::new(),
            config: None,
            next_group: 0,
        }
    }

    fn test_scope() -> HsxUncoreScope {
        HsxUncoreScope { package_id: 0 }
    }
}
