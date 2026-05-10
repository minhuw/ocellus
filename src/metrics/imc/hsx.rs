use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE};
use crate::metrics::uncore::hsx::{
    self, HsxUncoreScope, HsxUncoreSpec, HsxUncoreUnit, average_u64, events_per_second,
    scale_to_enabled,
};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

const HA_EVENT_GROUPS: [HsxHaEventGroup; 1] = [HsxHaEventGroup {
    events: [
        HsxHaEventSpec::new(HsxHaEventKind::LocalRead, 0x01, 0x01),
        HsxHaEventSpec::new(HsxHaEventKind::RemoteRead, 0x01, 0x02),
        HsxHaEventSpec::new(HsxHaEventKind::LocalWrite, 0x01, 0x04),
        HsxHaEventSpec::new(HsxHaEventKind::RemoteWrite, 0x01, 0x08),
    ],
}];

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
    pub ha_local_read_bytes_per_second: f64,
    pub ha_local_read_ratio: f64,
    pub ha_local_write_bytes_per_second: f64,
    pub ha_local_write_ratio: f64,
    pub ha_remote_read_bytes_per_second: f64,
    pub ha_remote_write_bytes_per_second: f64,
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
        ha_measurements: &BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>,
        imc_measurements: &BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
    ) -> Result<Self, String> {
        let activate = required_measurement(imc_measurements, HsxImcEventKind::Activate)?;
        let ha_local_read = required_ha_measurement(ha_measurements, HsxHaEventKind::LocalRead)?;
        let ha_remote_read = required_ha_measurement(ha_measurements, HsxHaEventKind::RemoteRead)?;
        let ha_local_write = required_ha_measurement(ha_measurements, HsxHaEventKind::LocalWrite)?;
        let ha_remote_write =
            required_ha_measurement(ha_measurements, HsxHaEventKind::RemoteWrite)?;
        let page_miss_precharge =
            required_measurement(imc_measurements, HsxImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(imc_measurements, HsxImcEventKind::ReadCas)?;
        let read_queue = required_measurement(imc_measurements, HsxImcEventKind::RpqNonEmpty)?;
        let write_cas = required_measurement(imc_measurements, HsxImcEventKind::WriteCas)?;
        let write_queue = required_measurement(imc_measurements, HsxImcEventKind::WpqNonEmpty)?;
        let write_queue_full = required_measurement(imc_measurements, HsxImcEventKind::WpqFull)?;
        let ha_read_count = scaled_ha_count(ha_local_read) + scaled_ha_count(ha_remote_read);
        let ha_write_count = scaled_ha_count(ha_local_write) + scaled_ha_count(ha_remote_write);

        Ok(Self {
            activate_commands_per_second: event_rate(activate),
            frequency_hz: frequency_hz(read_cas),
            ha_local_read_bytes_per_second: ha_bytes_per_second(ha_local_read),
            ha_local_read_ratio: hsx::ratio(scaled_ha_count(ha_local_read), ha_read_count),
            ha_local_write_bytes_per_second: ha_bytes_per_second(ha_local_write),
            ha_local_write_ratio: hsx::ratio(scaled_ha_count(ha_local_write), ha_write_count),
            ha_remote_read_bytes_per_second: ha_bytes_per_second(ha_remote_read),
            ha_remote_write_bytes_per_second: ha_bytes_per_second(ha_remote_write),
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
        mut ha_measurements: BTreeMap<
            HsxUncoreScope,
            BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>,
        >,
        imc_measurements: BTreeMap<
            HsxUncoreScope,
            BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
        >,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(imc_measurements.len());

        for (scope, scope_measurements) in imc_measurements {
            let ha_scope_measurements = ha_measurements
                .remove(&scope)
                .ok_or_else(|| format!("HSX HA measurements for {scope:?} are missing"))?;
            scopes.push(HsxImcMetricsByScope::from_measurements(
                scope,
                &ha_scope_measurements,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct HsxImcCollector {
    channels: Vec<HsxImcChannel>,
    ha_units: Vec<HsxHaUnit>,
    next_group: usize,
}

impl HsxImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        Ok(Self {
            channels: discover_channels(model)?,
            ha_units: discover_ha_units(model)?,
            next_group: 0,
        })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HsxImcMetrics, String> {
        if interval.is_zero() {
            return Err("HSX IMC measure interval must be non-zero".to_string());
        }

        let mut ha_measurements = HsxHaMeasurementAccumulator::new();
        let mut imc_measurements = HsxImcMeasurementAccumulator::new();

        for slice in self.schedule(interval) {
            match slice.group {
                HsxMeasurementGroup::Ha(group) => {
                    program_ha_units(&self.ha_units, group)?;

                    let started_at = Instant::now();
                    unfreeze_ha_units(&self.ha_units)?;
                    tokio::time::sleep(slice.duration).await;
                    freeze_ha_units(&self.ha_units)?;

                    read_ha_units(
                        &self.ha_units,
                        HsxHaMeasurement {
                            enabled: interval,
                            group,
                            running: started_at.elapsed(),
                        },
                        &mut ha_measurements,
                    )?;
                }
                HsxMeasurementGroup::Imc(group) => {
                    program_channels(&self.channels, group)?;

                    let started_at = Instant::now();
                    unfreeze_channels(&self.channels)?;
                    tokio::time::sleep(slice.duration).await;
                    freeze_channels(&self.channels)?;

                    read_channels(
                        &self.channels,
                        HsxImcMeasurement {
                            enabled: interval,
                            group,
                            running: started_at.elapsed(),
                        },
                        &mut imc_measurements,
                    )?;
                }
            }
        }

        self.rotate_group();

        HsxImcMetrics::from_measurements(
            ha_measurements.into_measurements(),
            imc_measurements.into_measurements(),
        )
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % measurement_groups().len();
    }

    fn schedule(&self, interval: Duration) -> Vec<HsxImcMeasurementSlice> {
        let groups = measurement_groups();
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
    ha_local_read_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    ha_local_read_ratio: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    ha_local_write_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    ha_local_write_ratio: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    ha_remote_read_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
    ha_remote_write_bytes_per_second: Family<HsxImcScopeLabels, Gauge<f64, AtomicU64>>,
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
            ha_local_read_bytes_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_local_read_ratio: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_local_write_bytes_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_local_write_ratio: Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_remote_read_bytes_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_remote_write_bytes_per_second:
                Family::<HsxImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
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
        registry.register(
            "ocellus_haswell_ha_local_read_bytes_per_second",
            "Interval-derived Haswell HA local read request bandwidth in bytes per second",
            metrics.ha_local_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_read_ratio",
            "Haswell HA local read request ratio",
            metrics.ha_local_read_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_write_bytes_per_second",
            "Interval-derived Haswell HA local write request bandwidth in bytes per second",
            metrics.ha_local_write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_local_write_ratio",
            "Haswell HA local write request ratio",
            metrics.ha_local_write_ratio.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_remote_read_bytes_per_second",
            "Interval-derived Haswell HA remote read request bandwidth in bytes per second",
            metrics.ha_remote_read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_haswell_ha_remote_write_bytes_per_second",
            "Interval-derived Haswell HA remote write request bandwidth in bytes per second",
            metrics.ha_remote_write_bytes_per_second.clone(),
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
            self.ha_local_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.ha_local_read_bytes_per_second);
            self.ha_local_read_ratio
                .get_or_create(&labels)
                .set(scope.ha_local_read_ratio);
            self.ha_local_write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.ha_local_write_bytes_per_second);
            self.ha_local_write_ratio
                .get_or_create(&labels)
                .set(scope.ha_local_write_ratio);
            self.ha_remote_read_bytes_per_second
                .get_or_create(&labels)
                .set(scope.ha_remote_read_bytes_per_second);
            self.ha_remote_write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.ha_remote_write_bytes_per_second);
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
    scope: HsxUncoreScope,
    unit: HsxUncoreUnit,
}

impl HsxImcChannel {
    fn new(location: metal::pci::PciLocation, scope: HsxUncoreScope) -> Result<Self, String> {
        Ok(Self {
            scope,
            unit: HsxUncoreUnit::new(location)?,
        })
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

#[derive(Debug)]
struct HsxHaUnit {
    scope: HsxUncoreScope,
    unit: HsxUncoreUnit,
}

impl HsxHaUnit {
    fn new(location: metal::pci::PciLocation, scope: HsxUncoreScope) -> Result<Self, String> {
        Ok(Self {
            scope,
            unit: HsxUncoreUnit::new(location)?,
        })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HsxImcEventGroup {
    events: [HsxImcEventSpec; hsx::COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HsxMeasurementGroup {
    Ha(HsxHaEventGroup),
    Imc(HsxImcEventGroup),
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
    group: HsxMeasurementGroup,
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
        for channel in spec.imc_channels {
            if let Ok(location) =
                metal::pci::find_intel_device_matching_spec_on_bus(*channel, bus_scope.bus)
            {
                channels.push(HsxImcChannel::new(location, bus_scope.scope)?);
            }
        }
    }

    if channels.is_empty() {
        return Err(format!("failed to discover any {} IMC channels", spec.name));
    }

    Ok(channels)
}

fn discover_ha_units(model: IntelServerCpuModel) -> Result<Vec<HsxHaUnit>, String> {
    let spec = HsxUncoreSpec::from_model(model)
        .ok_or_else(|| format!("HSX HA collection is not supported for {model:?}"))?;
    let bus_scopes = hsx::bus_scopes(spec)?;
    let mut units = Vec::with_capacity(bus_scopes.len() * metal::arch::hsx::pci::HA_UNITS.len());

    for bus_scope in bus_scopes {
        for (device, function) in metal::arch::hsx::pci::HA_UNITS {
            if let Ok(location) =
                metal::pci::find_intel_device_at_address_on_bus(device, function, bus_scope.bus)
            {
                units.push(HsxHaUnit::new(location, bus_scope.scope)?);
            }
        }
    }

    if units.is_empty() {
        return Err(format!("failed to discover any {} HA units", spec.name));
    }

    Ok(units)
}

fn program_ha_units(units: &[HsxHaUnit], group: HsxHaEventGroup) -> Result<(), String> {
    for unit in units {
        unit.unit.freeze_and_reset()?;
    }

    for unit in units {
        unit.program(group)?;
    }

    Ok(())
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

fn read_ha_units(
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

fn freeze_ha_units(units: &[HsxHaUnit]) -> Result<(), String> {
    for unit in units {
        unit.unit.freeze()?;
    }

    Ok(())
}

fn freeze_channels(channels: &[HsxImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unit.freeze()?;
    }

    Ok(())
}

fn unfreeze_ha_units(units: &[HsxHaUnit]) -> Result<(), String> {
    for unit in units {
        unit.unit.unfreeze()?;
    }

    Ok(())
}

fn unfreeze_channels(channels: &[HsxImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unit.unfreeze()?;
    }

    Ok(())
}

fn required_ha_measurement(
    measurements: &BTreeMap<HsxHaEventKind, HsxHaEventMeasurement>,
    kind: HsxHaEventKind,
) -> Result<&HsxHaEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("HSX HA measurement {kind:?} is missing"))
}

fn required_measurement(
    measurements: &BTreeMap<HsxImcEventKind, HsxImcEventMeasurement>,
    kind: HsxImcEventKind,
) -> Result<&HsxImcEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("HSX IMC measurement {kind:?} is missing"))
}

fn ha_bytes_per_second(measurement: &HsxHaEventMeasurement) -> f64 {
    events_per_second(scaled_ha_count(measurement), measurement.enabled) * BYTES_PER_CACHE_LINE
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

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn measurement_groups() -> Vec<HsxMeasurementGroup> {
    HA_EVENT_GROUPS
        .into_iter()
        .map(HsxMeasurementGroup::Ha)
        .chain(IMC_EVENT_GROUPS.into_iter().map(HsxMeasurementGroup::Imc))
        .collect()
}

fn queue_cycle_ratio(measurement: &HsxImcEventMeasurement) -> f64 {
    hsx::ratio(measurement.value, measurement.ticks)
}

fn scaled_count(measurement: &HsxImcEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

fn scaled_ha_count(measurement: &HsxHaEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_hsx_imc_metrics() {
        let scope = test_scope();
        let metrics = HsxImcMetrics::from_measurements(
            BTreeMap::from([(
                scope,
                BTreeMap::from([
                    ha_measurement(HsxHaEventKind::LocalRead, 100, 100),
                    ha_measurement(HsxHaEventKind::RemoteRead, 300, 100),
                    ha_measurement(HsxHaEventKind::LocalWrite, 200, 100),
                    ha_measurement(HsxHaEventKind::RemoteWrite, 200, 100),
                ]),
            )]),
            BTreeMap::from([(
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
            )]),
        )
        .unwrap();

        let imc = metrics.scopes[0];

        assert_eq!(imc.ha_local_read_bytes_per_second, 64_000.0);
        assert_eq!(imc.ha_remote_read_bytes_per_second, 192_000.0);
        assert_eq!(imc.ha_local_read_ratio, 0.25);
        assert_eq!(imc.ha_local_write_ratio, 0.5);
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

        assert_eq!(slices.len(), 3);
        assert_eq!(slices[0].group, HsxMeasurementGroup::Ha(HA_EVENT_GROUPS[0]));
        assert_eq!(
            slices[1].group,
            HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[0])
        );
        assert_eq!(
            slices[2].group,
            HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[1])
        );
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                HsxMeasurementGroup::Ha(HA_EVENT_GROUPS[0]),
                HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[0]),
                HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[1]),
            ]
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![
                HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[0]),
                HsxMeasurementGroup::Imc(IMC_EVENT_GROUPS[1]),
                HsxMeasurementGroup::Ha(HA_EVENT_GROUPS[0]),
            ]
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

    fn ha_measurement(
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

    fn slice_groups(slices: Vec<HsxImcMeasurementSlice>) -> Vec<HsxMeasurementGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }

    fn test_collector() -> HsxImcCollector {
        HsxImcCollector {
            channels: Vec::new(),
            ha_units: Vec::new(),
            next_group: 0,
        }
    }

    fn test_scope() -> HsxUncoreScope {
        HsxUncoreScope { package_id: 0 }
    }
}
