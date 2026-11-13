use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metrics::common::BYTES_PER_CACHE_LINE;
use crate::metrics::uncore::hsx::{
    self, HsxConfigBox, HsxUncoreScope, HsxUncoreSpec, HsxUncoreUnit, average_u64,
    events_per_second, measurement_round_count, scale_to_enabled,
};
use crate::pmu_config::types::{BoxType, CountAccumulator, PmuConfig, UncoreCounts};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

const IMC_CONFIG_BOX: HsxConfigBox = HsxConfigBox {
    box_type: BoxType::Imc,
    name: "IMC",
    unit: "channel",
    fixed: true,
};

const IMC_EVENT_GROUPS: [HsxImcEventGroup; 2] = [
    HsxImcEventGroup {
        events: [
            HsxImcEventSpec::sum(HsxImcEventKind::ReadCas, 0x04, 0x03),
            HsxImcEventSpec::sum(HsxImcEventKind::WriteCas, 0x04, 0x0c),
            HsxImcEventSpec::sum(HsxImcEventKind::Activate, 0x01, 0x0b),
            HsxImcEventSpec::sum(HsxImcEventKind::PageMissPrecharge, 0x02, 0x01),
        ],
    },
    HsxImcEventGroup {
        events: [
            HsxImcEventSpec::average(HsxImcEventKind::RpqNonEmpty, 0x11, 0x00),
            HsxImcEventSpec::average(HsxImcEventKind::WpqNonEmpty, 0x21, 0x00),
            HsxImcEventSpec::average(HsxImcEventKind::WpqFull, 0x22, 0x00),
            HsxImcEventSpec::disabled(),
        ],
    },
];

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct HsxImcMetricsByScope {
    pub activate_commands_per_second: f64,
    pub frequency_hz: f64,
    pub page_miss_precharge_commands_per_second: f64,
    pub read_cas_commands_per_second: f64,
    pub read_bytes_per_second: f64,
    pub rpq_non_empty_ratio: f64,
    #[serde(flatten)]
    pub scope: HsxUncoreScope,
    pub write_cas_commands_per_second: f64,
    pub write_bytes_per_second: f64,
    pub wpq_full_ratio: f64,
    pub wpq_non_empty_ratio: f64,
}

impl HsxImcMetricsByScope {
    fn from_measurements(
        scope: HsxUncoreScope,
        imc_measurements: &BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
    ) -> Result<Self, String> {
        let activate = required_measurement(imc_measurements, HsxImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(imc_measurements, HsxImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(imc_measurements, HsxImcEventKind::ReadCas)?;
        let read_queue = required_measurement(imc_measurements, HsxImcEventKind::RpqNonEmpty)?;
        let write_cas = required_measurement(imc_measurements, HsxImcEventKind::WriteCas)?;
        let write_queue = required_measurement(imc_measurements, HsxImcEventKind::WpqNonEmpty)?;
        let write_queue_full = required_measurement(imc_measurements, HsxImcEventKind::WpqFull)?;

        Ok(Self {
            activate_commands_per_second: event_rate(activate),
            frequency_hz: frequency_hz(read_cas),
            page_miss_precharge_commands_per_second: event_rate(page_miss_precharge),
            read_cas_commands_per_second: event_rate(read_cas),
            read_bytes_per_second: bytes_per_second(read_cas),
            rpq_non_empty_ratio: queue_cycle_ratio(read_queue),
            scope,
            write_cas_commands_per_second: event_rate(write_cas),
            write_bytes_per_second: bytes_per_second(write_cas),
            wpq_full_ratio: queue_cycle_ratio(write_queue_full),
            wpq_non_empty_ratio: queue_cycle_ratio(write_queue),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HsxImcMetrics {
    pub scopes: Vec<HsxImcMetricsByScope>,
}

impl HsxImcMetrics {
    fn from_measurements(
        imc_measurements: BTreeMap<
            HsxUncoreScope,
            BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
        >,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(imc_measurements.len());

        for (scope, scope_measurements) in imc_measurements {
            scopes.push(HsxImcMetricsByScope::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct HsxImcCollector {
    channels: Vec<HsxImcChannel>,
    config: Option<CountAccumulator>,
    next_group: usize,
}

impl HsxImcCollector {
    /// The IMC channels of each package that PMU configs can program.
    /// Unit id: the channel's index in the model's IMC channel table, 0..=7 from D20:F0 to D24:F1.
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        hsx::unit_map(self.channels.iter().map(HsxImcChannel::config_unit))
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        // The default sample resets every channel, which clears its controls,
        // and programs its group and DCLK, on every slice.
        let Some(config) = config else {
            self.config = None;
            return Ok(());
        };
        IMC_CONFIG_BOX.check_config(&config.program, &self.config_units())?;
        self.config = Some(CountAccumulator::new(config));

        Ok(())
    }

    /// Samples the set config for `interval`; `cycle` sets the group temporal
    /// groups start from. Its counts are cumulative since the config was set.
    pub async fn sample_config(
        &mut self,
        interval: Duration,
        cycle: u64,
    ) -> Result<UncoreCounts, String> {
        let accumulator = self
            .config
            .as_mut()
            .ok_or_else(|| "no PMU config is set for HSX IMC".to_string())?;
        let units = self
            .channels
            .iter()
            .map(|channel| (channel.config_unit(), &channel.unit))
            .collect::<Vec<_>>();

        IMC_CONFIG_BOX
            .sample_config(&units, accumulator, interval, cycle)
            .await
    }
}

impl HsxImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        Ok(Self {
            channels: discover_channels(model)?,
            config: None,
            next_group: 0,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxImcMetrics, String> {
        if interval.is_zero() {
            return Err("HSX IMC measure interval must be non-zero".to_string());
        }

        let mut imc_measurements = HsxImcMeasurementAccumulator::new();

        for slice in self.schedule(interval) {
            program_channels(&self.channels, slice.group)?;

            let started_at = Instant::now();
            unfreeze_channels(&self.channels)?;
            tokio::time::sleep(slice.duration).await;
            freeze_channels(&self.channels)?;

            read_channels(
                &self.channels,
                HsxImcMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut imc_measurements,
            )?;
        }

        self.rotate_group();

        HsxImcMetrics::from_measurements(imc_measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % IMC_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<HsxImcMeasurementSlice> {
        let groups = IMC_EVENT_GROUPS;
        let group_count = groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(HsxImcMeasurementSlice {
                    duration: slice_duration,
                    group: groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Debug)]
pub struct HsxImcPrometheusMetrics {
    activate_commands_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    page_miss_precharge_commands_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_cas_commands_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_non_empty_ratio: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_cas_commands_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_full_ratio: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_non_empty_ratio: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
}

impl HsxImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            activate_commands_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            page_miss_precharge_commands_per_second: Family::<
                HsxImcScopeLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            read_cas_commands_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_bytes_per_second: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_non_empty_ratio: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_cas_commands_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_bytes_per_second: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_full_ratio: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_non_empty_ratio: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_haswell_imc_activate_commands_per_second",
            "Interval-derived Haswell IMC activate commands per second",
            metrics.activate_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_frequency_hz",
            "Interval-derived Haswell IMC DCLK frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_page_miss_precharge_commands_per_second",
            "Interval-derived Haswell IMC page-miss precharge commands per second",
            metrics.page_miss_precharge_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_read_cas_commands_per_second",
            "Interval-derived Haswell IMC read CAS commands per second",
            metrics.read_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_read_bytes_per_second",
            "Interval-derived Haswell IMC read bandwidth in bytes per second",
            metrics.read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_rpq_non_empty_ratio",
            "Haswell IMC read pending queue non-empty cycle ratio",
            metrics.rpq_non_empty_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_write_cas_commands_per_second",
            "Interval-derived Haswell IMC write CAS commands per second",
            metrics.write_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_write_bytes_per_second",
            "Interval-derived Haswell IMC write bandwidth in bytes per second",
            metrics.write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_wpq_full_ratio",
            "Haswell IMC write pending queue full cycle ratio",
            metrics.wpq_full_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_imc_wpq_non_empty_ratio",
            "Haswell IMC write pending queue non-empty cycle ratio",
            metrics.wpq_non_empty_ratio.clone(),
        );
        metrics
    }

    pub fn update(&self, metrics: HsxImcMetrics) {
        for scope in metrics.scopes {
            let labels = HsxImcScopeLabels::from_scope(scope.scope);

            self.activate_commands_per_second
                .get_or_create(&labels)
                .set(scope.activate_commands_per_second);
            self.frequency_hz
                .get_or_create(&labels)
                .set(scope.frequency_hz);
            self.page_miss_precharge_commands_per_second
                .get_or_create(&labels)
                .set(scope.page_miss_precharge_commands_per_second);
            self.read_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.read_cas_commands_per_second);
            self.read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.read_bytes_per_second);
            self.rpq_non_empty_ratio
                .get_or_create(&labels)
                .set(scope.rpq_non_empty_ratio);
            self.write_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.write_cas_commands_per_second);
            self.write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.write_bytes_per_second);
            self.wpq_full_ratio
                .get_or_create(&labels)
                .set(scope.wpq_full_ratio);
            self.wpq_non_empty_ratio
                .get_or_create(&labels)
                .set(scope.wpq_non_empty_ratio);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct HsxImcScopeLabels {
    package: String,
}

impl HsxImcScopeLabels {
    fn from_scope(scope: HsxUncoreScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct HsxImcChannel {
    /// Index in the model's IMC channel table, kept when other channels
    /// are absent.
    id: u16,
    scope: HsxUncoreScope,
    unit: HsxUncoreUnit,
}

impl HsxImcChannel {
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

    /// The channel's package and unit id in PMU configs.
    fn config_unit(&self) -> (u16, u16) {
        (self.scope.package_id as u16, self.id)
    }

    fn program(&self, group: HsxImcEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            let Some(_) = event.kind else {
                continue;
            };

            self.unit
                .program_counter(counter_index, event.event, event.umask)?;
        }

        self.unit.reset_and_enable_fixed_counter()
    }

    fn read(&self) -> Result<HsxImcChannelReading, String> {
        Ok(HsxImcChannelReading {
            counters: [
                self.unit.read_counter(0)?,
                self.unit.read_counter(1)?,
                self.unit.read_counter(2)?,
                self.unit.read_counter(3)?,
            ],
            ticks: self.unit.read_fixed_counter()?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxImcChannelReading {
    counters: [u64; hsx::COUNTER_COUNT],
    ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxImcEventGroup {
    events: [HsxImcEventSpec; hsx::COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxImcEventSpec {
    aggregate: HsxImcAggregate,
    event: u8,
    kind: Option<HsxImcEventKind>,
    umask: u8,
}

impl HsxImcEventSpec {
    const fn average(kind: HsxImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            aggregate: HsxImcAggregate::Average,
            event,
            kind: Some(kind),
            umask,
        }
    }

    const fn disabled() -> Self {
        Self {
            aggregate: HsxImcAggregate::Disabled,
            event: 0,
            kind: None,
            umask: 0,
        }
    }

    const fn sum(kind: HsxImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            aggregate: HsxImcAggregate::Sum,
            event,
            kind: Some(kind),
            umask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HsxImcAggregate {
    Average,
    Disabled,
    Sum,
}

impl HsxImcAggregate {
    fn aggregate(self, values: impl Iterator<Item = u64>) -> u64 {
        match self {
            Self::Average => average_u64(values),
            Self::Disabled => 0,
            Self::Sum => values.sum(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HsxImcEventKind {
    Activate,
    PageMissPrecharge,
    ReadCas,
    RpqNonEmpty,
    WpqFull,
    WpqNonEmpty,
    WriteCas,
}

#[derive(Clone, Copy, Debug)]
struct HsxImcEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl HsxImcEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct HsxImcMeasurement {
    enabled: Duration,
    group: HsxImcEventGroup,
    running: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxImcMeasurementSlice {
    duration: Duration,
    group: HsxImcEventGroup,
}

#[derive(Debug, Default)]
struct HsxImcMeasurementAccumulator {
    measurements: BTreeMap<HsxUncoreScope, BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>>,
}

impl HsxImcMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: HsxUncoreScope,
        kind: HsxImcEventKind,
        value: u64,
        ticks: u64,
        measurement: HsxImcMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(HsxImcEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<HsxUncoreScope, BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>> {
        self.measurements
    }
}

fn discover_channels(model: IntelServerCpuModel) -> Result<Vec<HsxImcChannel>, String> {
    let spec = HsxUncoreSpec::from_model(model)
        .ok_or_else(|| format!("HSX IMC collection is not supported for {model:?}"))?;
    let bus_scopes = hsx::bus_scopes(spec)?;
    let mut channels = Vec::new();

    for bus_scope in bus_scopes {
        for (id, location) in hsx::present_units(spec.imc_channels, |channel| {
            metal::pci::find_intel_device_matching_spec_on_bus(channel, bus_scope.bus).ok()
        }) {
            channels.push(HsxImcChannel::new(id, location, bus_scope.scope)?);
        }
    }

    if channels.is_empty() {
        return Err(format!("failed to discover any {} IMC channels", spec.name));
    }

    Ok(channels)
}

fn program_channels(channels: &[HsxImcChannel], group: HsxImcEventGroup) -> Result<(), String> {
    for channel in channels {
        channel.unit.freeze_and_reset()?;
    }

    for channel in channels {
        channel.program(group)?;
    }

    Ok(())
}

fn read_channels(
    channels: &[HsxImcChannel],
    measurement: HsxImcMeasurement,
    measurements: &mut HsxImcMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<HsxUncoreScope, Vec<HsxImcChannelReading>>::new();

    for channel in channels {
        readings
            .entry(channel.scope)
            .or_default()
            .push(channel.read()?);
    }

    for (scope, channel_readings) in readings {
        let ticks = average_u64(channel_readings.iter().map(|reading| reading.ticks));

        for counter_index in 0..measurement.group.events.len() {
            let event = measurement.group.events[counter_index];
            let Some(kind) = event.kind else {
                continue;
            };
            let value = event.aggregate.aggregate(
                channel_readings
                    .iter()
                    .map(|reading| reading.counters[counter_index]),
            );

            measurements.add(scope, kind, value, ticks, measurement);
        }
    }

    Ok(())
}

fn freeze_channels(channels: &[HsxImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unit.freeze()?;
    }

    Ok(())
}

fn unfreeze_channels(channels: &[HsxImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unit.unfreeze()?;
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
    kind: HsxImcEventKind,
) -> Result<&HsxImcEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("HSX IMC measurement {kind:?} is missing"))
}

fn bytes_per_second(measurement: &HsxImcEventMeasurement) -> f64 {
    event_rate(measurement) * BYTES_PER_CACHE_LINE
}

fn event_rate(measurement: &HsxImcEventMeasurement) -> f64 {
    events_per_second(scaled_count(measurement), measurement.enabled)
}

fn frequency_hz(measurement: &HsxImcEventMeasurement) -> f64 {
    hsx::frequency_hz(measurement.ticks, measurement.running)
}

fn queue_cycle_ratio(measurement: &HsxImcEventMeasurement) -> f64 {
    hsx::ratio(measurement.value, measurement.ticks)
}

fn scaled_count(measurement: &HsxImcEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::uncore::hsx::config_fixtures::*;
    use crate::pmu_config::types::{ChaFilters, CountUnit, CounterSlot, Grouping};

    #[test]
    fn computes_hsx_imc_metrics() {
        let scope = test_scope();
        let metrics = HsxImcMetrics::from_measurements(BTreeMap::from([(
            scope,
            BTreeMap::from([
                measurement(HsxImcEventKind::Activate, 1_000, 1_000, 100),
                measurement(HsxImcEventKind::PageMissPrecharge, 700, 1_000, 100),
                measurement(HsxImcEventKind::ReadCas, 2_000, 1_000, 100),
                measurement(HsxImcEventKind::WriteCas, 3_000, 1_000, 100),
                measurement(HsxImcEventKind::RpqNonEmpty, 400, 1_000, 100),
                measurement(HsxImcEventKind::WpqNonEmpty, 600, 1_000, 100),
                measurement(HsxImcEventKind::WpqFull, 20, 1_000, 100),
            ]),
        )]))
        .unwrap();

        let imc = metrics.scopes[0];

        assert_eq!(imc.activate_commands_per_second, 10_000.0);
        assert_eq!(imc.page_miss_precharge_commands_per_second, 7_000.0);
        assert_eq!(imc.read_bytes_per_second, 1_280_000.0);
        assert_eq!(imc.write_bytes_per_second, 1_920_000.0);
        assert_eq!(imc.rpq_non_empty_ratio, 0.4);
        assert_eq!(imc.wpq_non_empty_ratio, 0.6);
        assert_eq!(imc.wpq_full_ratio, 0.02);
    }

    #[test]
    fn averages_queue_cycle_events_across_channels() {
        let group = IMC_EVENT_GROUPS[1];
        let readings = [
            HsxImcChannelReading {
                counters: [400, 600, 10, 20],
                ticks: 1_000,
            },
            HsxImcChannelReading {
                counters: [400, 600, 10, 20],
                ticks: 1_000,
            },
        ];

        for counter_index in 0..group.events.len() {
            if group.events[counter_index].kind.is_none() {
                continue;
            }

            assert_eq!(
                group.events[counter_index].aggregate.aggregate(
                    readings
                        .iter()
                        .map(|reading| reading.counters[counter_index])
                ),
                readings[0].counters[counter_index]
            );
        }
    }

    #[test]
    fn schedules_short_interval_once_per_group() {
        let collector = test_collector();
        let slices = collector.schedule(Duration::from_millis(100));

        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].group, IMC_EVENT_GROUPS[0]);
        assert_eq!(slices[1].group, IMC_EVENT_GROUPS[1]);
        assert!(
            slices
                .iter()
                .all(|slice| slice.duration == Duration::from_millis(50))
        );
    }

    #[test]
    fn gives_each_group_half_of_a_long_interval() {
        let slices = test_collector().schedule(Duration::from_secs(1));

        assert_eq!(slices.len(), 10);
        for (position, slice) in slices.iter().enumerate() {
            assert_eq!(slice.duration, Duration::from_millis(100));
            assert_eq!(slice.group, IMC_EVENT_GROUPS[position % 2]);
        }
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![IMC_EVENT_GROUPS[0], IMC_EVENT_GROUPS[1]]
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![IMC_EVENT_GROUPS[1], IMC_EVENT_GROUPS[0]]
        );
    }

    fn measurement(
        kind: HsxImcEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (HsxImcEventKind, HsxImcEventMeasurement) {
        (
            kind,
            HsxImcEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                ticks,
                value,
            },
        )
    }

    fn slice_groups(slices: Vec<HsxImcMeasurementSlice>) -> Vec<HsxImcEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }

    #[test]
    fn programs_two_general_counters_and_dclk_on_channel_3() {
        let program = config_program(
            BoxType::Imc,
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x40_0304),
                    config_counter(CounterSlot::General(3), 0x40_0c04),
                    config_counter(CounterSlot::Fixed, 0x40_0000),
                ],
                BTreeMap::from([(1, BTreeSet::from([3]))]),
            )],
        );
        let ids = [(0, 0), (0, 3), (1, 0), (1, 1), (1, 3)];
        let slices = hsx::config_schedule(&program, Duration::from_secs(1), 0);

        assert_eq!(
            hsx::slice_units(&ids, &program, &slices[0]).unwrap(),
            vec![(4, 0)]
        );
        assert_eq!(
            IMC_CONFIG_BOX.unit_writes(&program.groups[0]).unwrap(),
            vec![
                (0xf4, 0x3_0103),
                (0xd8, 0x42_0304),
                (0xdc, 0),
                (0xe0, 0),
                (0xe4, 0x42_0c04),
                (0xf0, 0x48_0000),
            ]
        );
    }

    #[test]
    fn clears_dclk_control_of_groups_without_fixed() {
        let group = config_group(
            0,
            vec![config_counter(CounterSlot::General(1), 0x40_0304)],
            BTreeMap::from([(0, BTreeSet::from([0]))]),
        );

        assert_eq!(
            IMC_CONFIG_BOX.unit_writes(&group).unwrap(),
            vec![
                (0xf4, 0x3_0103),
                (0xd8, 0),
                (0xdc, 0x42_0304),
                (0xe0, 0),
                (0xe4, 0),
                (0xf0, 0),
            ]
        );
    }

    #[test]
    fn numbers_channels_by_their_index_in_the_channel_table() {
        // D21:F0, the third channel of the table, is absent.
        let present =
            hsx::present_units(&metal::arch::hsx::pci::BROADWELL_IMC_CHANNELS, |channel| {
                (channel.device != 21 || channel.function != 0).then_some(metal::pci::PciLocation {
                    bus: 0x7f,
                    device: channel.device,
                    function: channel.function,
                    group: 0,
                })
            });
        assert_eq!(
            present
                .iter()
                .map(|(id, location)| (*id, location.device, location.function))
                .collect::<Vec<_>>(),
            vec![
                (0, 20, 0),
                (1, 20, 1),
                (3, 21, 1),
                (4, 23, 0),
                (5, 23, 1),
                (6, 24, 0),
                (7, 24, 1),
            ]
        );

        let mut collector = test_collector();
        collector.channels = present
            .iter()
            .map(|&(id, _)| test_channel(1, id, &[0; 0x100]))
            .collect();
        assert_eq!(
            collector.config_units(),
            BTreeMap::from([(1, BTreeSet::from([0, 1, 3, 4, 5, 6, 7]))])
        );
    }

    #[test]
    fn rejects_configs_imc_channels_cannot_count() {
        let host = BTreeMap::from([(0, BTreeSet::from([0, 1, 3])), (1, (0..8).collect())]);
        let valid = || {
            config_program(
                BoxType::Imc,
                Grouping::Fixed,
                vec![config_group(
                    0,
                    vec![
                        config_counter(CounterSlot::General(3), 0x40_0304),
                        config_counter(CounterSlot::Fixed, 0x40_0000),
                    ],
                    BTreeMap::from([(0, BTreeSet::from([0, 3])), (1, BTreeSet::from([7]))]),
                )],
            )
        };
        IMC_CONFIG_BOX.check_config(&valid(), &host).unwrap();

        let mut ha = valid();
        ha.box_type = BoxType::Ha;
        let mut package = valid();
        package.groups[0].units.insert(2, BTreeSet::from([0]));
        let mut channel = valid();
        channel.groups[0].units.insert(0, BTreeSet::from([2]));
        let mut slot = valid();
        slot.groups[0]
            .counters
            .push(config_counter(CounterSlot::General(4), 0x40_0304));
        let mut control = valid();
        control.groups[0].counters[0].control = 1 << 32;
        let mut filters = valid();
        filters.groups[0].filters = Some(ChaFilters {
            filter0: 0,
            filter1: 0,
        });

        for (program, error) in [
            (ha, "Haswell-EP/Broadwell-EP IMC cannot count a ha config"),
            (
                package,
                "IMC config group 0 runs on package 2, which has no IMC channels on this host",
            ),
            (
                channel,
                "IMC config group 0 runs on channel 2 of package 0, which this host does not have",
            ),
            (
                slot,
                "IMC config group 0: Haswell-EP/Broadwell-EP IMC channels have no counter slot 4",
            ),
            (
                control,
                "IMC config group 0 slot 3 control 0x100000000 does not fit in 32 bits",
            ),
            (
                filters,
                "IMC config group 0 has filters, which IMC channels do not have",
            ),
        ] {
            assert_eq!(
                IMC_CONFIG_BOX.check_config(&program, &host),
                Err(error.to_string())
            );
        }
    }

    #[test]
    fn counts_a_config_on_its_channels_only() {
        let mut collector = test_collector();
        collector.channels = vec![
            test_channel(1, 0, &test_config_space()),
            test_channel(1, 3, &test_config_space()),
        ];
        collector
            .set_config(Some(config(dclk_and_cas_program())))
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        for (cas, dclk) in [(5, 1_000), (10, 2_000)] {
            let counts = runtime
                .block_on(collector.sample_config(Duration::from_millis(1), 0))
                .unwrap();
            assert_eq!(
                count_values(&counts),
                vec![
                    (1, CountUnit::Box(3), CounterSlot::General(0), cas),
                    (1, CountUnit::Box(3), CounterSlot::Fixed, dclk),
                ]
            );
            assert!(counts.running[0].seconds >= 0.001);
        }

        let registers = |channel: &HsxImcChannel| {
            [0xf4, 0xd8, 0xdc, 0xe0, 0xe4, 0xf0]
                .map(|offset| channel.unit.read_dword(offset).unwrap())
        };
        // Channel 3 ends frozen, with the group's controls; channel 0 is in
        // no group and keeps what it had.
        assert_eq!(
            registers(&collector.channels[1]),
            [0x3_0100, 0x42_0304, 0, 0, 0, 0x48_0000]
        );
        assert_eq!(
            registers(&collector.channels[0]),
            [0, 0, 0x40_0001, 0x40_0001, 0x40_0001, 0]
        );
    }

    #[test]
    fn restarts_counts_when_the_config_changes() {
        let mut collector = test_collector();
        collector.channels = vec![test_channel(1, 3, &test_config_space())];
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let sample = |collector: &mut HsxImcCollector| {
            runtime
                .block_on(collector.sample_config(Duration::from_millis(1), 0))
                .unwrap()
        };
        let cas = |counts: &UncoreCounts| count_values(counts)[0].3;

        collector
            .set_config(Some(config(dclk_and_cas_program())))
            .unwrap();
        sample(&mut collector);
        assert_eq!(cas(&sample(&mut collector)), 10);

        // An invalid config leaves the current one and its counts alone.
        let mut elsewhere = dclk_and_cas_program();
        elsewhere.groups[0].units = BTreeMap::from([(1, BTreeSet::from([2]))]);
        assert!(collector.set_config(Some(config(elsewhere))).is_err());
        assert_eq!(cas(&sample(&mut collector)), 15);

        // Setting a config, even the same one again, restarts its counts.
        collector
            .set_config(Some(config(dclk_and_cas_program())))
            .unwrap();
        let restarted = collector.config.as_ref().unwrap().counts();
        assert!(count_values(&restarted).iter().all(|count| count.3 == 0));
        assert!(
            restarted
                .running
                .iter()
                .all(|running| running.seconds == 0.0)
        );
        assert_eq!(cas(&sample(&mut collector)), 5);

        collector.set_config(None).unwrap();
        assert!(!collector.has_config());
        assert_eq!(
            runtime.block_on(collector.sample_config(Duration::from_millis(1), 0)),
            Err("no PMU config is set for HSX IMC".to_string())
        );
    }

    /// CAS reads on counter 0 and DCLK, on channel 3 of package 1.
    fn dclk_and_cas_program() -> crate::pmu_config::types::ProgrammedBoxType {
        config_program(
            BoxType::Imc,
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x40_0304),
                    config_counter(CounterSlot::Fixed, 0x40_0000),
                ],
                BTreeMap::from([(1, BTreeSet::from([3]))]),
            )],
        )
    }

    /// A channel's config space whose counter 0 holds 5 above bit 48 junk,
    /// DCLK 1000, and controls 1-3 a stale event.
    fn test_config_space() -> [u8; 0x100] {
        let mut config_space = [0_u8; 0x100];
        config_space[0xa0..0xa8].copy_from_slice(&((0xabcd_u64 << 48) | 5).to_le_bytes());
        config_space[0xd0..0xd8].copy_from_slice(&1_000_u64.to_le_bytes());
        for offset in [0xdc, 0xe0, 0xe4] {
            config_space[offset..offset + 4].copy_from_slice(&0x40_0001_u32.to_le_bytes());
        }
        config_space
    }

    fn test_channel(package_id: u32, id: u16, config_space: &[u8]) -> HsxImcChannel {
        HsxImcChannel {
            id,
            scope: HsxUncoreScope { package_id },
            unit: HsxUncoreUnit::from_config_space(config_space),
        }
    }

    fn test_collector() -> HsxImcCollector {
        HsxImcCollector {
            channels: Vec::new(),
            config: None,
            next_group: 0,
        }
    }

    fn test_scope() -> HsxUncoreScope {
        HsxUncoreScope { package_id: 0 }
    }
}
