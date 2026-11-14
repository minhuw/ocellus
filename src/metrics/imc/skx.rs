use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::arch::skx::pmon;
use crate::metal::pci::PciDevice;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::EventProfile;
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE, topology_label};
use crate::pmu_config::types::{
    BoxType, CountAccumulator, CounterSlot, Grouping, PmuConfig, ProgrammedBoxType,
    ProgrammedGroup, UncoreCounts, temporal_first_group,
};

const IMC_COUNTER_WIDTH: u32 = 48;
const IMC_CTL_OFFSETS: [u64; 4] = [0xd8, 0xdc, 0xe0, 0xe4];
const IMC_CTR_OFFSETS: [u64; 4] = [0xa0, 0xa8, 0xb0, 0xb8];
const IMC_DCLK_CTL_OFFSET: u64 = 0xf0;
const IMC_DCLK_CTR_OFFSET: u64 = 0xd0;
const IMC_UNIT_CTL_OFFSET: u64 = 0xf4;

const SKX_IMC_QUEUE_GROUP: ImcEventGroup = ImcEventGroup {
    events: [
        ImcEventSpec::sum(ImcEventKind::ReadInsert, 0x10),
        ImcEventSpec::sum(ImcEventKind::WriteInsert, 0x20),
        ImcEventSpec::sum(ImcEventKind::ReadOccupancy, 0x80),
        ImcEventSpec::sum(ImcEventKind::WriteOccupancy, 0x81),
    ],
};

const SKX_IMC_COMMAND_GROUP: ImcEventGroup = ImcEventGroup {
    events: [
        ImcEventSpec::sum_with_umask(ImcEventKind::ReadCas, 0x04, 0x03),
        ImcEventSpec::sum_with_umask(ImcEventKind::WriteCas, 0x04, 0x0c),
        ImcEventSpec::sum_with_umask(ImcEventKind::Activate, 0x01, 0x0b),
        ImcEventSpec::sum_with_umask(ImcEventKind::PageMissPrecharge, 0x02, 0x01),
    ],
};

const SKX_IMC_EVENT_GROUPS: [ImcEventGroup; 2] = [SKX_IMC_QUEUE_GROUP, SKX_IMC_COMMAND_GROUP];

/// The mem-io profiles need only memory bandwidth, so the CAS group counts
/// for the whole interval, as peacock measured it.
const SKX_IMC_COMMAND_EVENT_GROUPS: [ImcEventGroup; 1] = [SKX_IMC_COMMAND_GROUP];

fn skx_imc_event_groups(profile: EventProfile) -> &'static [ImcEventGroup] {
    match profile {
        EventProfile::Default => &SKX_IMC_EVENT_GROUPS,
        EventProfile::MemIo2Hit | EventProfile::MemIoBasic | EventProfile::MemIoItom => {
            &SKX_IMC_COMMAND_EVENT_GROUPS
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ImcScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub die_group_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub die_id: Option<u32>,
    pub package_id: u32,
}

impl ImcScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            die_group_id: topology.level_id(TopologyLevelKind::DieGroup),
            die_id: topology.level_id(TopologyLevelKind::Die),
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ImcScopeMetrics {
    #[serde(flatten)]
    pub scope: ImcScope,
    pub activate_commands_per_second: f64,
    pub frequency_hz: f64,
    pub page_miss_precharge_commands_per_second: f64,
    pub read_cas_commands_per_second: f64,
    pub read_bytes_per_second: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpq_residency_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpq_occupancy_entries: Option<f64>,
    pub write_cas_commands_per_second: f64,
    pub write_bytes_per_second: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wpq_residency_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wpq_occupancy_entries: Option<f64>,
}

impl ImcScopeMetrics {
    /// Queue metrics come only from profiles that measure the queue group.
    fn from_measurements(
        scope: ImcScope,
        measurements: &BTreeMap<ImcEventKind, ImcEventMeasurement>,
        measures_queues: bool,
    ) -> Result<Self, String> {
        let queues = if measures_queues {
            Some(ImcQueueMeasurements {
                read_insert: required_measurement(measurements, ImcEventKind::ReadInsert)?,
                read_occupancy: required_measurement(measurements, ImcEventKind::ReadOccupancy)?,
                write_insert: required_measurement(measurements, ImcEventKind::WriteInsert)?,
                write_occupancy: required_measurement(measurements, ImcEventKind::WriteOccupancy)?,
            })
        } else {
            None
        };
        let activate = required_measurement(measurements, ImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(measurements, ImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(measurements, ImcEventKind::ReadCas)?;
        let write_cas = required_measurement(measurements, ImcEventKind::WriteCas)?;

        Ok(Self {
            scope,
            activate_commands_per_second: command_rate(activate),
            frequency_hz: frequency_hz(
                queues
                    .as_ref()
                    .map_or(read_cas, |queues| queues.read_insert),
            ),
            page_miss_precharge_commands_per_second: command_rate(page_miss_precharge),
            read_cas_commands_per_second: command_rate(read_cas),
            read_bytes_per_second: bytes_per_second(
                scale_to_enabled(read_cas.value, read_cas.enabled, read_cas.running),
                read_cas.enabled,
            ),
            rpq_residency_seconds: queues
                .as_ref()
                .map(|queues| queue_residency_seconds(queues.read_occupancy, queues.read_insert)),
            rpq_occupancy_entries: queues
                .as_ref()
                .map(|queues| ratio(queues.read_occupancy.value, queues.read_occupancy.ticks)),
            write_cas_commands_per_second: command_rate(write_cas),
            write_bytes_per_second: bytes_per_second(
                scale_to_enabled(write_cas.value, write_cas.enabled, write_cas.running),
                write_cas.enabled,
            ),
            wpq_residency_seconds: queues
                .as_ref()
                .map(|queues| queue_residency_seconds(queues.write_occupancy, queues.write_insert)),
            wpq_occupancy_entries: queues
                .as_ref()
                .map(|queues| ratio(queues.write_occupancy.value, queues.write_occupancy.ticks)),
        })
    }
}

struct ImcQueueMeasurements<'a> {
    read_insert: &'a ImcEventMeasurement,
    read_occupancy: &'a ImcEventMeasurement,
    write_insert: &'a ImcEventMeasurement,
    write_occupancy: &'a ImcEventMeasurement,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ImcMetrics {
    pub scopes: Vec<ImcScopeMetrics>,
}

impl ImcMetrics {
    fn from_measurements(
        groups: &[ImcEventGroup],
        measurements: BTreeMap<ImcScope, BTreeMap<ImcEventKind, ImcEventMeasurement>>,
    ) -> Result<Self, String> {
        let measures_queues = groups.contains(&SKX_IMC_QUEUE_GROUP);
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(ImcScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
                measures_queues,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SkxImcCollector {
    channels: Vec<ImcChannel>,
    config: Option<CountAccumulator>,
    groups: &'static [ImcEventGroup],
    next_group: usize,
}

impl SkxImcCollector {
    /// The IMC channels of each package that PMU configs can program.
    /// Unit id: the channel's index in its package, 0..=5 by (memory controller, channel).
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        let mut units = BTreeMap::<u16, BTreeSet<u16>>::new();
        for (package, unit) in channel_units(self.channels.iter().map(|channel| channel.scope)) {
            units.entry(package).or_default().insert(unit);
        }

        units
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        // The default sample reprograms every counter, DCLK included.
        let Some(config) = config else {
            self.config = None;
            return Ok(());
        };
        check_config(&config.program, &self.config_units())?;
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
            .ok_or_else(|| "no PMU config is set for Skylake-SP IMC".to_string())?;
        if interval.is_zero() {
            return Err("IMC measure interval must be non-zero".to_string());
        }

        let units = channel_units(self.channels.iter().map(|channel| channel.scope));
        let program = accumulator.config().program.clone();

        for slice in config_schedule(&program, interval, cycle) {
            let targets = slice_channels(&units, &program, &slice)?;
            for &(channel, group) in &targets {
                self.channels[channel].program_config(&program.groups[group])?;
            }

            let started_at = Instant::now();
            for &(channel, _) in &targets {
                self.channels[channel].unfreeze()?;
            }
            tokio::time::sleep(slice.duration).await;
            for &(channel, _) in &targets {
                self.channels[channel].freeze()?;
            }
            let running = started_at.elapsed();

            let mut readings = Vec::with_capacity(targets.len());
            for &(channel, group) in &targets {
                let group = &program.groups[group];
                let (package, unit) = units[channel];
                readings.push(ConfigReading {
                    package,
                    unit,
                    group: group.index,
                    counters: self.channels[channel].read_config(group)?,
                });
            }
            fold_readings(accumulator, &readings, running)?;
        }

        Ok(accumulator.counts())
    }
}

impl SkxImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        Ok(Self {
            channels: discover_channels(architecture.intel_server_model())?,
            config: None,
            groups: &SKX_IMC_EVENT_GROUPS,
            next_group: 0,
        })
    }

    pub fn set_event_profile(&mut self, profile: EventProfile) {
        self.groups = skx_imc_event_groups(profile);
        self.next_group = 0;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<ImcMetrics, String> {
        if interval.is_zero() {
            return Err("IMC measure interval must be non-zero".to_string());
        }

        let mut measurements = ImcMeasurementAccumulator::new();
        let channels = &self.channels;

        for slice in self.schedule(interval) {
            program_channels(channels, slice.group)?;

            let started_at = Instant::now();
            unfreeze_channels(channels)?;
            tokio::time::sleep(slice.duration).await;
            freeze_channels(channels)?;

            read_channels(
                channels,
                ImcMeasurement {
                    enabled: interval,
                    group: slice.group,
                    running: started_at.elapsed(),
                },
                &mut measurements,
            )?;
        }

        self.rotate_group();

        ImcMetrics::from_measurements(self.groups, measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % self.groups.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<ImcMeasurementSlice> {
        let group_count = self.groups.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(ImcMeasurementSlice {
                    duration: slice_duration,
                    group: self.groups[rotated_index],
                });
            }
        }

        slices
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImcMeasurementSlice {
    duration: Duration,
    group: ImcEventGroup,
}

#[derive(Debug)]
pub struct ImcPrometheusMetrics {
    activate_commands_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    page_miss_precharge_commands_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_cas_commands_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    read_bytes_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_residency_seconds: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    rpq_occupancy_entries: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_cas_commands_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    write_bytes_per_second: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_residency_seconds: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
    wpq_occupancy_entries: Family<ImcScopeLabels, Gauge<f64, AtomicU64>>,
}

impl ImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            activate_commands_per_second: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            frequency_hz: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            page_miss_precharge_commands_per_second:
                Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            read_cas_commands_per_second: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            read_bytes_per_second: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_residency_seconds: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            rpq_occupancy_entries: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            write_cas_commands_per_second: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(
            ),
            write_bytes_per_second: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_residency_seconds: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
            wpq_occupancy_entries: Family::<ImcScopeLabels, Gauge<f64, AtomicU64>>::default(),
        };

        registry.register(
            "ocellus_imc_activate_commands_per_second",
            "Interval-derived IMC activate commands per second",
            metrics.activate_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_frequency_hz",
            "Interval-derived IMC DCLK frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_imc_page_miss_precharge_commands_per_second",
            "Interval-derived IMC page-miss precharge commands per second",
            metrics.page_miss_precharge_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_read_cas_commands_per_second",
            "Interval-derived IMC read CAS commands per second",
            metrics.read_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_read_bytes_per_second",
            "Interval-derived IMC read bandwidth in bytes per second",
            metrics.read_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_rpq_residency_seconds",
            "Interval-derived IMC read pending queue residency in seconds",
            metrics.rpq_residency_seconds.clone(),
        );
        registry.register(
            "ocellus_imc_rpq_occupancy_entries",
            "Average IMC read pending queue occupancy in entries",
            metrics.rpq_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_imc_write_cas_commands_per_second",
            "Interval-derived IMC write CAS commands per second",
            metrics.write_cas_commands_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_write_bytes_per_second",
            "Interval-derived IMC write bandwidth in bytes per second",
            metrics.write_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_imc_wpq_residency_seconds",
            "Interval-derived IMC write pending queue residency in seconds",
            metrics.wpq_residency_seconds.clone(),
        );
        registry.register(
            "ocellus_imc_wpq_occupancy_entries",
            "Average IMC write pending queue occupancy in entries",
            metrics.wpq_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: ImcMetrics) {
        for scope in metrics.scopes {
            let labels = ImcScopeLabels::from_scope(scope.scope);

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
            if let Some(value) = scope.rpq_residency_seconds {
                self.rpq_residency_seconds.get_or_create(&labels).set(value);
            }
            if let Some(value) = scope.rpq_occupancy_entries {
                self.rpq_occupancy_entries.get_or_create(&labels).set(value);
            }
            self.write_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.write_cas_commands_per_second);
            self.write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.write_bytes_per_second);
            if let Some(value) = scope.wpq_residency_seconds {
                self.wpq_residency_seconds.get_or_create(&labels).set(value);
            }
            if let Some(value) = scope.wpq_occupancy_entries {
                self.wpq_occupancy_entries.get_or_create(&labels).set(value);
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ImcScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl ImcScopeLabels {
    fn from_scope(scope: ImcScope) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Debug)]
struct ImcChannel {
    device: PciDevice,
    scope: ImcScope,
}

impl ImcChannel {
    fn new(spec: ImcChannelSpec) -> Result<Self, String> {
        Ok(Self {
            device: PciDevice::open(spec.location)?,
            scope: spec.scope,
        })
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.device
            .write_u32(IMC_UNIT_CTL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)
    }

    fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IMC_UNIT_CTL_OFFSET, pmon::UNIT_FREEZE)
    }

    fn program(&self, group: ImcEventGroup) -> Result<(), String> {
        for (counter_index, event) in group.events.into_iter().enumerate() {
            self.device.write_u32(
                IMC_CTL_OFFSETS[counter_index],
                pmon::counter_control(event.event, event.umask, true),
            )?;
        }

        self.device
            .write_u32(IMC_DCLK_CTL_OFFSET, pmon::FIXED_COUNTER_RESET_AND_ENABLE)
    }

    fn read(&self) -> Result<ImcChannelReading, String> {
        Ok(ImcChannelReading {
            counters: [
                self.read_counter(0)?,
                self.read_counter(1)?,
                self.read_counter(2)?,
                self.read_counter(3)?,
            ],
            ticks: self
                .device
                .read_u64(IMC_DCLK_CTR_OFFSET)
                .map(mask_counter)?,
        })
    }

    fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
        self.device
            .read_u64(IMC_CTR_OFFSETS[counter_index])
            .map(mask_counter)
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32(IMC_UNIT_CTL_OFFSET, pmon::UNIT_UNFREEZE)
    }

    fn program_config(&self, group: &ProgrammedGroup) -> Result<(), String> {
        for (offset, value) in channel_writes(group)? {
            self.device.write_u32(offset, value)?;
        }

        Ok(())
    }

    /// The raw values of `group`'s counters.
    fn read_config(&self, group: &ProgrammedGroup) -> Result<Vec<(CounterSlot, u64)>, String> {
        group
            .counters
            .iter()
            .map(|counter| {
                let (_, offset) = slot_offsets(counter.slot)?;
                let value = read_counter_dwords(offset, |offset| self.device.read_u32(offset))?;
                Ok((counter.slot, value))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct ImcChannelReading {
    counters: [u64; 4],
    ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct ImcChannelSpec {
    location: metal::pci::PciLocation,
    scope: ImcScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImcEventGroup {
    events: [ImcEventSpec; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImcEventSpec {
    aggregate: ImcAggregate,
    event: u8,
    kind: ImcEventKind,
    umask: u8,
}

impl ImcEventSpec {
    const fn sum(kind: ImcEventKind, event: u8) -> Self {
        Self {
            aggregate: ImcAggregate::Sum,
            event,
            kind,
            umask: 0,
        }
    }

    const fn sum_with_umask(kind: ImcEventKind, event: u8, umask: u8) -> Self {
        Self {
            aggregate: ImcAggregate::Sum,
            event,
            kind,
            umask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImcAggregate {
    Sum,
}

impl ImcAggregate {
    fn aggregate(self, values: impl Iterator<Item = u64>) -> u64 {
        match self {
            Self::Sum => values.sum(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ImcEventKind {
    Activate,
    PageMissPrecharge,
    ReadCas,
    ReadInsert,
    ReadOccupancy,
    WriteCas,
    WriteInsert,
    WriteOccupancy,
}

#[derive(Clone, Copy, Debug)]
struct ImcEventMeasurement {
    enabled: Duration,
    running: Duration,
    ticks: u64,
    value: u64,
}

impl ImcEventMeasurement {
    fn add(&mut self, value: u64, ticks: u64, running: Duration) {
        self.running += running;
        self.ticks += ticks;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug)]
struct ImcMeasurement {
    enabled: Duration,
    group: ImcEventGroup,
    running: Duration,
}

#[derive(Debug, Default)]
struct ImcMeasurementAccumulator {
    measurements: BTreeMap<ImcScope, BTreeMap<ImcEventKind, ImcEventMeasurement>>,
}

impl ImcMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: ImcScope,
        kind: ImcEventKind,
        value: u64,
        ticks: u64,
        measurement: ImcMeasurement,
    ) {
        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(value, ticks, measurement.running)
            })
            .or_insert(ImcEventMeasurement {
                enabled: measurement.enabled,
                running: measurement.running,
                ticks,
                value,
            });
    }

    fn into_measurements(self) -> BTreeMap<ImcScope, BTreeMap<ImcEventKind, ImcEventMeasurement>> {
        self.measurements
    }
}

fn discover_channels(model: IntelServerCpuModel) -> Result<Vec<ImcChannel>, String> {
    let channel_specs = discover_channel_specs(model)?;
    let mut channels = Vec::with_capacity(channel_specs.len());

    for spec in channel_specs {
        channels.push(ImcChannel::new(spec)?);
    }

    Ok(channels)
}

fn discover_channel_specs(model: IntelServerCpuModel) -> Result<Vec<ImcChannelSpec>, String> {
    if !matches!(model, IntelServerCpuModel::SkylakeXeon) {
        return Err(format!("IMC collection is not supported for {model:?}"));
    }

    let bus_scopes = imc_bus_scopes()?;
    let mut channels =
        Vec::with_capacity(bus_scopes.len() * metal::arch::skx::pci::IMC_CHANNELS.len());

    for (bus, scope) in bus_scopes {
        for spec in metal::arch::skx::pci::IMC_CHANNELS {
            channels.push(ImcChannelSpec {
                location: metal::pci::find_intel_device_matching_spec_on_bus(spec, bus)?,
                scope,
            });
        }
    }

    Ok(channels)
}

fn imc_bus_scopes() -> Result<Vec<(metal::pci::PciBus, ImcScope)>, String> {
    let socket_scopes = imc_scopes()?;
    let socket_buses = metal::arch::skx::pci::imc_socket_buses(socket_scopes.len())?;

    if socket_buses.len() != socket_scopes.len() {
        return Err(format!(
            "discovered {} IMC buses for {} CPU packages",
            socket_buses.len(),
            socket_scopes.len()
        ));
    }

    socket_buses
        .into_iter()
        .map(|socket_bus| {
            let scope = socket_scopes
                .get(socket_bus.socket_index)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "failed to map SKX IMC socket index {} to a CPUID package",
                        socket_bus.socket_index
                    )
                })?;

            Ok((socket_bus.bus, scope))
        })
        .collect()
}

fn imc_scopes() -> Result<Vec<ImcScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(ImcScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any IMC scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
}

fn program_channels(channels: &[ImcChannel], group: ImcEventGroup) -> Result<(), String> {
    for channel in channels {
        channel.freeze_and_reset()?;
    }

    for channel in channels {
        channel.program(group)?;
    }

    Ok(())
}

fn read_channels(
    channels: &[ImcChannel],
    measurement: ImcMeasurement,
    measurements: &mut ImcMeasurementAccumulator,
) -> Result<(), String> {
    let mut readings = BTreeMap::<ImcScope, Vec<ImcChannelReading>>::new();

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
            let value = event.aggregate.aggregate(
                channel_readings
                    .iter()
                    .map(|reading| reading.counters[counter_index]),
            );

            measurements.add(scope, event.kind, value, ticks, measurement);
        }
    }

    Ok(())
}

fn freeze_channels(channels: &[ImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.freeze()?;
    }

    Ok(())
}

fn unfreeze_channels(channels: &[ImcChannel]) -> Result<(), String> {
    for channel in channels {
        channel.unfreeze()?;
    }

    Ok(())
}

/// One slice of a config sample: its groups count together.
#[derive(Debug, PartialEq, Eq)]
struct ConfigSlice {
    duration: Duration,
    /// Positions in the config's groups.
    groups: Vec<usize>,
}

/// The raw counter values of one channel and group in a slice.
#[derive(Debug)]
struct ConfigReading {
    package: u16,
    unit: u16,
    group: u8,
    counters: Vec<(CounterSlot, u64)>,
}

/// The package and unit id of each channel, in channel order.
fn channel_units(scopes: impl IntoIterator<Item = ImcScope>) -> Vec<(u16, u16)> {
    let mut next_units = BTreeMap::<u16, u16>::new();

    scopes
        .into_iter()
        .map(|scope| {
            let package = scope.package_id as u16;
            let next_unit = next_units.entry(package).or_default();
            let unit = *next_unit;
            *next_unit += 1;
            (package, unit)
        })
        .collect()
}

/// Errs for a program the IMC channels in `units` cannot count.
fn check_config(
    program: &ProgrammedBoxType,
    units: &BTreeMap<u16, BTreeSet<u16>>,
) -> Result<(), String> {
    if program.box_type != BoxType::Imc {
        return Err(format!(
            "Skylake-SP IMC cannot count a {} config",
            program.box_type.label()
        ));
    }

    for group in &program.groups {
        if group.filters.is_some() {
            return Err(format!(
                "IMC config group {} has filters, which IMC channels do not have",
                group.index
            ));
        }
        for (package, group_units) in &group.units {
            let host_units = units.get(package).ok_or_else(|| {
                format!(
                    "IMC config group {} runs on package {package}, which has no IMC channels on this host",
                    group.index
                )
            })?;
            if let Some(unit) = group_units.difference(host_units).next() {
                return Err(format!(
                    "IMC config group {} runs on channel {unit} of package {package}, which this host does not have",
                    group.index
                ));
            }
        }
        channel_writes(group)?;
    }

    Ok(())
}

/// The control and counter offsets of a counter slot.
fn slot_offsets(slot: CounterSlot) -> Result<(u64, u64), String> {
    match slot {
        CounterSlot::General(index) => IMC_CTL_OFFSETS
            .get(usize::from(index))
            .zip(IMC_CTR_OFFSETS.get(usize::from(index)))
            .map(|(&control, &counter)| (control, counter))
            .ok_or_else(|| format!("Skylake-SP IMC channels have no counter slot {index}")),
        CounterSlot::Fixed => Ok((IMC_DCLK_CTL_OFFSET, IMC_DCLK_CTR_OFFSET)),
    }
}

/// The (offset, value) writes that program one channel for `group`: freeze
/// and reset the channel, then reset and enable each counter of the group,
/// and write 0 to the controls of the others, DCLK included.
fn channel_writes(group: &ProgrammedGroup) -> Result<Vec<(u64, u32)>, String> {
    let mut controls = IMC_CTL_OFFSETS
        .into_iter()
        .chain([IMC_DCLK_CTL_OFFSET])
        .map(|offset| (offset, 0))
        .collect::<BTreeMap<u64, u32>>();

    for counter in &group.counters {
        let (offset, _) = slot_offsets(counter.slot)
            .map_err(|error| format!("IMC config group {}: {error}", group.index))?;
        let control = u32::try_from(counter.control).map_err(|_| {
            format!(
                "IMC config group {} slot {} control 0x{:x} does not fit in 32 bits",
                group.index,
                counter.slot.label(),
                counter.control
            )
        })?;
        let reset = match counter.slot {
            CounterSlot::General(_) => pmon::COUNTER_RESET,
            CounterSlot::Fixed => pmon::FIXED_COUNTER_RESET,
        };
        controls.insert(offset, control | reset);
    }

    Ok([(IMC_UNIT_CTL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)]
        .into_iter()
        .chain(controls)
        .collect())
}

/// Fixed and spatial groups count for the whole interval in one slice;
/// temporal groups take turns in equal slices, every round in group order
/// from the group `temporal_first_group` gives for `cycle`, wrapping around.
fn config_schedule(
    program: &ProgrammedBoxType,
    interval: Duration,
    cycle: u64,
) -> Vec<ConfigSlice> {
    let group_count = program.groups.len();

    match program.grouping {
        Grouping::Fixed | Grouping::Spatial => vec![ConfigSlice {
            duration: interval,
            groups: (0..group_count).collect(),
        }],
        Grouping::Temporal => {
            let round_count = measurement_round_count(interval, group_count);
            let duration = interval.div_f64((group_count * round_count) as f64);
            let start = temporal_first_group(cycle, group_count);

            (0..round_count)
                .flat_map(|_| 0..group_count)
                .map(|position| ConfigSlice {
                    duration,
                    groups: vec![(start + position) % group_count],
                })
                .collect()
        }
    }
}

/// The channels `slice` programs, as (position in `units`, position in the
/// config's groups). `units` holds each channel's package and unit id.
fn slice_channels(
    units: &[(u16, u16)],
    program: &ProgrammedBoxType,
    slice: &ConfigSlice,
) -> Result<Vec<(usize, usize)>, String> {
    let mut channels = Vec::new();

    for &group in &slice.groups {
        for (&package, group_units) in &program.groups[group].units {
            for &unit in group_units {
                let channel = units
                    .iter()
                    .position(|&channel_unit| channel_unit == (package, unit))
                    .ok_or_else(|| {
                        format!("this host has no IMC channel {unit} in package {package}")
                    })?;
                channels.push((channel, group));
            }
        }
    }

    Ok(channels)
}

/// Reads the counter at `offset` from its two dwords: high, low, high, and
/// again until the high dword holds.
fn read_counter_dwords(
    offset: u64,
    mut read_dword: impl FnMut(u64) -> Result<u32, String>,
) -> Result<u64, String> {
    let mut high = read_dword(offset + 4)?;

    loop {
        let low = read_dword(offset)?;
        let next_high = read_dword(offset + 4)?;
        if next_high == high {
            return Ok((u64::from(high) << 32) | u64::from(low));
        }
        high = next_high;
    }
}

/// Adds one slice's readings, masked to the counter width, and its running
/// time to each package and group it read.
fn fold_readings(
    accumulator: &mut CountAccumulator,
    readings: &[ConfigReading],
    running: Duration,
) -> Result<(), String> {
    let mut groups = BTreeSet::new();

    for reading in readings {
        for &(slot, value) in &reading.counters {
            accumulator.add_count(
                reading.package,
                reading.unit,
                reading.group,
                slot,
                mask_counter(value),
            )?;
        }
        groups.insert((reading.package, reading.group));
    }

    for (package, group) in groups {
        accumulator.add_running(package, group, running)?;
    }

    Ok(())
}

fn required_measurement(
    measurements: &BTreeMap<ImcEventKind, ImcEventMeasurement>,
    kind: ImcEventKind,
) -> Result<&ImcEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("IMC measurement {kind:?} is missing"))
}

fn average_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut count = 0;
    let mut sum = 0_u64;

    for value in values {
        count += 1;
        sum += value;
    }

    if count == 0 { 0 } else { sum / count }
}

fn bytes_per_second(cache_lines: u64, duration: Duration) -> f64 {
    events_per_second(cache_lines, duration) * BYTES_PER_CACHE_LINE
}

fn command_rate(measurement: &ImcEventMeasurement) -> f64 {
    events_per_second(
        scale_to_enabled(measurement.value, measurement.enabled, measurement.running),
        measurement.enabled,
    )
}

fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn frequency_hz(measurement: &ImcEventMeasurement) -> f64 {
    let elapsed = measurement.running.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        measurement.ticks as f64 / elapsed
    }
}

fn queue_residency_seconds(occupancy: &ImcEventMeasurement, insert: &ImcEventMeasurement) -> f64 {
    if occupancy.ticks == 0 || insert.value == 0 || insert.running.is_zero() {
        return 0.0;
    }

    let average_occupancy = ratio(occupancy.value, occupancy.ticks);
    let insert_rate = insert.value as f64 / insert.running.as_secs_f64();
    average_occupancy / insert_rate
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << IMC_COUNTER_WIDTH) - 1)
}

fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::pmu_config::types::{
        Aggregate, ChaFilters, CountUnit, EventLabels, ProgrammedCounter,
    };

    #[test]
    fn computes_multiplexed_metrics() {
        let scope = test_scope();
        let metrics = ImcMetrics::from_measurements(
            &SKX_IMC_EVENT_GROUPS,
            BTreeMap::from([(
                scope,
                BTreeMap::from([
                    measurement(ImcEventKind::ReadInsert, 200, 1_000, 100),
                    measurement(ImcEventKind::WriteInsert, 300, 1_000, 100),
                    measurement(ImcEventKind::ReadOccupancy, 400, 1_000, 100),
                    measurement(ImcEventKind::WriteOccupancy, 600, 1_000, 100),
                    measurement(ImcEventKind::ReadCas, 2_000, 1_000, 100),
                    measurement(ImcEventKind::WriteCas, 3_000, 1_000, 100),
                    measurement(ImcEventKind::Activate, 1_000, 1_000, 100),
                    measurement(ImcEventKind::PageMissPrecharge, 700, 1_000, 100),
                ]),
            )]),
        )
        .unwrap();

        let scope_metrics = metrics.scopes[0];

        assert_eq!(scope_metrics.activate_commands_per_second, 10_000.0);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(
            scope_metrics.page_miss_precharge_commands_per_second,
            7_000.0
        );
        assert_eq!(scope_metrics.read_cas_commands_per_second, 20_000.0);
        assert_eq!(scope_metrics.read_bytes_per_second, 1_280_000.0);
        assert_eq!(scope_metrics.write_cas_commands_per_second, 30_000.0);
        assert_eq!(scope_metrics.write_bytes_per_second, 1_920_000.0);
        assert_eq!(scope_metrics.rpq_occupancy_entries, Some(0.4));
        assert_eq!(scope_metrics.wpq_occupancy_entries, Some(0.6));
        assert_close(scope_metrics.rpq_residency_seconds.unwrap(), 0.0002);
        assert_close(scope_metrics.wpq_residency_seconds.unwrap(), 0.0002);
    }

    #[test]
    fn computes_residency_from_occupancy_dclk_and_insert_rate() {
        let occupancy = ImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(400),
            ticks: 400,
            value: 800,
        };
        let insert = ImcEventMeasurement {
            enabled: Duration::from_secs(1),
            running: Duration::from_millis(200),
            ticks: 200,
            value: 2_000_000,
        };

        assert_close(queue_residency_seconds(&occupancy, &insert), 0.0000002);
    }

    #[test]
    fn measures_only_cas_group_for_mem_io_profiles() {
        for profile in [
            EventProfile::MemIo2Hit,
            EventProfile::MemIoBasic,
            EventProfile::MemIoItom,
        ] {
            let mut collector = test_collector();
            collector.set_event_profile(profile);
            let slices = collector.schedule(Duration::from_secs(1));

            assert_eq!(slices.len(), 10);
            assert!(
                slices
                    .iter()
                    .all(|slice| slice.group == SKX_IMC_COMMAND_GROUP
                        && slice.duration == Duration::from_millis(100))
            );
        }
    }

    #[test]
    fn computes_bandwidth_without_queue_group() {
        let metrics = ImcMetrics::from_measurements(
            &SKX_IMC_COMMAND_EVENT_GROUPS,
            BTreeMap::from([(
                test_scope(),
                BTreeMap::from([
                    measurement(ImcEventKind::ReadCas, 100, 1_000, 100),
                    measurement(ImcEventKind::WriteCas, 50, 1_000, 100),
                    measurement(ImcEventKind::Activate, 10, 1_000, 100),
                    measurement(ImcEventKind::PageMissPrecharge, 5, 1_000, 100),
                ]),
            )]),
        )
        .unwrap();
        let scope_metrics = metrics.scopes[0];

        assert_eq!(scope_metrics.read_bytes_per_second, 64_000.0);
        assert_eq!(scope_metrics.write_bytes_per_second, 32_000.0);
        assert_eq!(scope_metrics.frequency_hz, 10_000.0);
        assert_eq!(scope_metrics.rpq_occupancy_entries, None);
        assert_eq!(scope_metrics.rpq_residency_seconds, None);
        assert_eq!(scope_metrics.wpq_occupancy_entries, None);
        assert_eq!(scope_metrics.wpq_residency_seconds, None);
    }

    #[test]
    fn omits_unmeasured_queue_metrics_from_exposition() {
        let mut registry = Registry::default();
        let prometheus = ImcPrometheusMetrics::register(&mut registry);
        let metrics = ImcMetrics::from_measurements(
            &SKX_IMC_COMMAND_EVENT_GROUPS,
            BTreeMap::from([(
                test_scope(),
                BTreeMap::from([
                    measurement(ImcEventKind::ReadCas, 100, 1_000, 100),
                    measurement(ImcEventKind::WriteCas, 50, 1_000, 100),
                    measurement(ImcEventKind::Activate, 10, 1_000, 100),
                    measurement(ImcEventKind::PageMissPrecharge, 5, 1_000, 100),
                ]),
            )]),
        )
        .unwrap();
        prometheus.update(metrics);
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();

        assert!(text.contains("ocellus_imc_read_bytes_per_second{"));
        assert!(!text.contains("ocellus_imc_rpq_occupancy_entries{"));
        assert!(!text.contains("ocellus_imc_wpq_residency_seconds{"));
    }

    #[test]
    fn schedules_short_interval_once_per_group() {
        let collector = test_collector();
        let slices = collector.schedule(Duration::from_millis(100));

        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].group, SKX_IMC_EVENT_GROUPS[0]);
        assert_eq!(slices[1].group, SKX_IMC_EVENT_GROUPS[1]);
        assert_eq!(slices[0].duration, Duration::from_millis(50));
    }

    #[test]
    fn schedules_long_interval_with_bounded_slices() {
        let collector = test_collector();
        let slices = collector.schedule(Duration::from_secs(1));

        assert_eq!(slices.len(), 10);
        assert_eq!(slices[0].group, SKX_IMC_EVENT_GROUPS[0]);
        assert_eq!(slices[1].group, SKX_IMC_EVENT_GROUPS[1]);
        assert_eq!(slices[8].group, SKX_IMC_EVENT_GROUPS[0]);
        assert_eq!(slices[9].group, SKX_IMC_EVENT_GROUPS[1]);
        assert_eq!(slices[0].duration, Duration::from_millis(100));
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![SKX_IMC_EVENT_GROUPS[0], SKX_IMC_EVENT_GROUPS[1]]
        );

        collector.rotate_group();
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            vec![SKX_IMC_EVENT_GROUPS[1], SKX_IMC_EVENT_GROUPS[0]]
        );
    }

    #[test]
    fn wraps_48_bit_counters() {
        assert_eq!(mask_counter((1_u64 << 50) | 7), 7);
    }

    #[test]
    fn scales_counts_to_enabled_time() {
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::from_millis(100)),
            1_000
        );
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::ZERO),
            0
        );
    }

    #[test]
    fn accumulates_repeated_multiplex_slices() {
        let mut accumulator = ImcMeasurementAccumulator::new();
        let scope = test_scope();

        accumulator.add(
            scope,
            ImcEventKind::ReadInsert,
            100,
            1_000,
            test_measurement(Duration::from_secs(1), Duration::from_millis(100)),
        );
        accumulator.add(
            scope,
            ImcEventKind::ReadInsert,
            200,
            2_000,
            test_measurement(Duration::from_secs(1), Duration::from_millis(200)),
        );

        let measurements = accumulator.into_measurements();
        let measurement = measurements
            .get(&scope)
            .unwrap()
            .get(&ImcEventKind::ReadInsert)
            .unwrap();

        assert_eq!(measurement.value, 300);
        assert_eq!(measurement.ticks, 3_000);
        assert_eq!(measurement.enabled, Duration::from_secs(1));
        assert_eq!(measurement.running, Duration::from_millis(300));
    }

    #[test]
    fn numbers_channels_within_each_package() {
        let scope = |package_id| ImcScope {
            die_group_id: None,
            die_id: None,
            package_id,
        };

        assert_eq!(
            channel_units([0, 0, 0, 1, 1].map(scope)),
            vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1)]
        );
    }

    #[test]
    fn programs_two_general_counters_and_dclk_on_channel_3() {
        let program = config_program(
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x50_0304),
                    config_counter(CounterSlot::General(3), 0x50_0c04),
                    config_counter(CounterSlot::Fixed, 0x40_0000),
                ],
                BTreeMap::from([(1, BTreeSet::from([3]))]),
            )],
        );
        let slices = config_schedule(&program, Duration::from_secs(1), 0);

        assert_eq!(
            slice_channels(&two_package_units(), &program, &slices[0]).unwrap(),
            vec![(9, 0)]
        );
        assert_eq!(
            channel_writes(&program.groups[0]).unwrap(),
            vec![
                (0xf4, 0x3_0103),
                (0xd8, 0x52_0304),
                (0xdc, 0),
                (0xe0, 0),
                (0xe4, 0x52_0c04),
                (0xf0, 0x48_0000),
            ]
        );
    }

    #[test]
    fn clears_dclk_control_of_groups_without_fixed() {
        let group = config_group(
            0,
            vec![config_counter(CounterSlot::General(1), 0x50_0304)],
            BTreeMap::from([(0, BTreeSet::from([0]))]),
        );

        assert_eq!(
            channel_writes(&group).unwrap(),
            vec![
                (0xf4, 0x3_0103),
                (0xd8, 0),
                (0xdc, 0x52_0304),
                (0xe0, 0),
                (0xe4, 0),
                (0xf0, 0),
            ]
        );
    }

    #[test]
    fn schedules_config_slices() {
        let units = |units: &[u16]| BTreeMap::from([(0, units.iter().copied().collect())]);
        let general = || vec![config_counter(CounterSlot::General(0), 0x50_0304)];

        let fixed = config_program(
            Grouping::Fixed,
            vec![config_group(0, general(), units(&[0, 4]))],
        );
        let slices = config_schedule(&fixed, Duration::from_secs(1), 0);
        assert_eq!(
            slices,
            vec![ConfigSlice {
                duration: Duration::from_secs(1),
                groups: vec![0],
            }]
        );
        assert_eq!(
            slice_channels(&two_package_units(), &fixed, &slices[0]).unwrap(),
            vec![(0, 0), (4, 0)]
        );

        let spatial = config_program(
            Grouping::Spatial,
            vec![
                config_group(0, general(), units(&[0, 1])),
                config_group(1, general(), units(&[2])),
            ],
        );
        let slices = config_schedule(&spatial, Duration::from_secs(1), 0);
        assert_eq!(
            slices,
            vec![ConfigSlice {
                duration: Duration::from_secs(1),
                groups: vec![0, 1],
            }]
        );
        assert_eq!(
            slice_channels(&two_package_units(), &spatial, &slices[0]).unwrap(),
            vec![(0, 0), (1, 0), (2, 1)]
        );

        let temporal = config_program(
            Grouping::Temporal,
            vec![
                config_group(0, general(), units(&[5])),
                config_group(1, general(), units(&[5])),
            ],
        );
        let slices = config_schedule(&temporal, Duration::from_secs(1), 0);
        assert_eq!(slices.len(), 10);
        for (position, slice) in slices.iter().enumerate() {
            assert_eq!(
                *slice,
                ConfigSlice {
                    duration: Duration::from_millis(100),
                    groups: vec![position % 2],
                }
            );
        }
        assert_eq!(
            slice_channels(&two_package_units(), &temporal, &slices[1]).unwrap(),
            vec![(5, 1)]
        );
        assert_eq!(
            config_schedule(&temporal, Duration::from_millis(100), 0),
            vec![
                ConfigSlice {
                    duration: Duration::from_millis(50),
                    groups: vec![0],
                },
                ConfigSlice {
                    duration: Duration::from_millis(50),
                    groups: vec![1],
                },
            ]
        );
    }

    fn scheduled_slices(
        program: &ProgrammedBoxType,
        interval: Duration,
        cycle: u64,
    ) -> Vec<crate::metrics::testing::ScheduledSlice> {
        config_schedule(program, interval, cycle)
            .into_iter()
            .map(|slice| (slice.duration, slice.groups))
            .collect()
    }

    #[test]
    fn starts_temporal_config_groups_by_block() {
        crate::metrics::testing::assert_block_schedule(BoxType::Imc, scheduled_slices);
    }

    #[test]
    fn reads_phase_locked_sources_without_bias_over_whole_blocks() {
        crate::metrics::testing::assert_unbiased_over_blocks(BoxType::Imc, scheduled_slices);
    }

    #[test]
    fn rereads_counter_when_high_dword_changes() {
        let read = |dwords: Vec<(u64, u32)>| {
            let mut dwords = dwords.into_iter();
            let value = read_counter_dwords(0xa8, |offset| {
                let (expected, dword) = dwords.next().unwrap();
                assert_eq!(offset, expected);
                Ok(dword)
            });
            assert!(dwords.next().is_none());
            value.unwrap()
        };

        assert_eq!(
            read(vec![(0xac, 0x7), (0xa8, 0x1234), (0xac, 0x7)]),
            0x7_0000_1234
        );
        assert_eq!(
            read(vec![
                (0xac, 0x7),
                (0xa8, 0xffff_fffe),
                (0xac, 0x8),
                (0xa8, 0x3),
                (0xac, 0x8),
            ]),
            0x8_0000_0003
        );
    }

    #[test]
    fn folds_config_readings_per_box_or_sum() {
        for aggregate in [Aggregate::Box, Aggregate::Sum] {
            let mut program = config_program(
                Grouping::Fixed,
                vec![config_group(
                    0,
                    vec![
                        config_counter(CounterSlot::General(0), 0x50_0304),
                        config_counter(CounterSlot::Fixed, 0x40_0000),
                    ],
                    BTreeMap::from([(0, BTreeSet::from([1, 2])), (1, BTreeSet::from([1]))]),
                )],
            );
            program.aggregate = aggregate;
            let mut accumulator = CountAccumulator::new(config(program));
            let readings = [
                config_reading(0, 1, 0, [10, 1_000]),
                config_reading(0, 2, 0, [(0xffff << 48) | 20, 1_000]),
                config_reading(1, 1, 0, [30, 2_000]),
            ];

            for _ in 0..2 {
                fold_readings(&mut accumulator, &readings, Duration::from_millis(500)).unwrap();
            }
            let counts = accumulator.counts();

            match aggregate {
                Aggregate::Box => assert_eq!(
                    count_values(&counts),
                    vec![
                        (0, CountUnit::Box(1), CounterSlot::General(0), 20),
                        (0, CountUnit::Box(1), CounterSlot::Fixed, 2_000),
                        (0, CountUnit::Box(2), CounterSlot::General(0), 40),
                        (0, CountUnit::Box(2), CounterSlot::Fixed, 2_000),
                        (1, CountUnit::Box(1), CounterSlot::General(0), 60),
                        (1, CountUnit::Box(1), CounterSlot::Fixed, 4_000),
                    ]
                ),
                Aggregate::Sum => assert_eq!(
                    count_values(&counts),
                    vec![
                        (0, CountUnit::Sum, CounterSlot::General(0), 60),
                        (0, CountUnit::Sum, CounterSlot::Fixed, 4_000),
                        (1, CountUnit::Sum, CounterSlot::General(0), 60),
                        (1, CountUnit::Sum, CounterSlot::Fixed, 4_000),
                    ]
                ),
            }
            assert_eq!(
                counts
                    .running
                    .iter()
                    .map(|running| (running.package, running.group, running.seconds))
                    .collect::<Vec<_>>(),
                vec![(0, 0, 1.0), (1, 0, 1.0)]
            );
        }
    }

    #[test]
    fn folds_running_time_per_temporal_group() {
        let general = || vec![config_counter(CounterSlot::General(0), 0x50_0304)];
        let units = || BTreeMap::from([(0, BTreeSet::from([0]))]);
        let mut accumulator = CountAccumulator::new(config(config_program(
            Grouping::Temporal,
            vec![
                config_group(0, general(), units()),
                config_group(1, general(), units()),
            ],
        )));

        for _ in 0..2 {
            fold_readings(
                &mut accumulator,
                &[ConfigReading {
                    package: 0,
                    unit: 0,
                    group: 0,
                    counters: vec![(CounterSlot::General(0), 1)],
                }],
                Duration::from_millis(250),
            )
            .unwrap();
            fold_readings(
                &mut accumulator,
                &[ConfigReading {
                    package: 0,
                    unit: 0,
                    group: 1,
                    counters: vec![(CounterSlot::General(0), 2)],
                }],
                Duration::from_millis(125),
            )
            .unwrap();
        }
        let counts = accumulator.counts();

        assert_eq!(
            counts
                .counts
                .iter()
                .map(|count| (count.group, count.value))
                .collect::<Vec<_>>(),
            vec![(0, 2), (1, 4)]
        );
        assert_eq!(
            counts
                .running
                .iter()
                .map(|running| (running.group, running.seconds))
                .collect::<Vec<_>>(),
            vec![(0, 0.5), (1, 0.25)]
        );
    }

    #[test]
    fn rejects_configs_imc_channels_cannot_count() {
        let host = BTreeMap::from([(0, (0..6).collect()), (1, (0..6).collect())]);
        let valid = || {
            config_program(
                Grouping::Fixed,
                vec![config_group(
                    0,
                    vec![
                        config_counter(CounterSlot::General(3), 0x50_0304),
                        config_counter(CounterSlot::Fixed, 0x40_0000),
                    ],
                    BTreeMap::from([(0, BTreeSet::from([0, 5])), (1, BTreeSet::from([2]))]),
                )],
            )
        };
        check_config(&valid(), &host).unwrap();

        let mut cha = valid();
        cha.box_type = BoxType::Cha;
        let mut package = valid();
        package.groups[0].units.insert(2, BTreeSet::from([0]));
        let mut channel = valid();
        channel.groups[0].units.insert(1, BTreeSet::from([2, 6]));
        let mut slot = valid();
        slot.groups[0]
            .counters
            .push(config_counter(CounterSlot::General(4), 0x50_0304));
        let mut filters = valid();
        filters.groups[0].filters = Some(ChaFilters {
            filter0: 0,
            filter1: 0,
        });

        for (program, error) in [
            (cha, "Skylake-SP IMC cannot count a cha config"),
            (
                package,
                "IMC config group 0 runs on package 2, which has no IMC channels on this host",
            ),
            (
                channel,
                "IMC config group 0 runs on channel 6 of package 1, which this host does not have",
            ),
            (
                slot,
                "IMC config group 0: Skylake-SP IMC channels have no counter slot 4",
            ),
            (
                filters,
                "IMC config group 0 has filters, which IMC channels do not have",
            ),
        ] {
            assert_eq!(check_config(&program, &host), Err(error.to_string()));
        }
    }

    #[test]
    fn sets_and_drops_configs() {
        let mut collector = test_collector();
        let program = config_program(
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![config_counter(CounterSlot::Fixed, 0x40_0000)],
                BTreeMap::from([(0, BTreeSet::from([0]))]),
            )],
        );

        assert!(collector.config_units().is_empty());
        assert_eq!(
            collector.set_config(Some(config(program))),
            Err(
                "IMC config group 0 runs on package 0, which has no IMC channels on this host"
                    .to_string()
            )
        );
        assert!(!collector.has_config());

        collector.config = Some(CountAccumulator::new(config(config_program(
            Grouping::Fixed,
            Vec::new(),
        ))));
        assert!(collector.has_config());
        collector.set_config(None).unwrap();
        assert!(!collector.has_config());
    }

    fn config_counter(slot: CounterSlot, control: u64) -> ProgrammedCounter {
        ProgrammedCounter {
            slot,
            control,
            labels: EventLabels::new(),
        }
    }

    fn config_group(
        index: u8,
        counters: Vec<ProgrammedCounter>,
        units: BTreeMap<u16, BTreeSet<u16>>,
    ) -> ProgrammedGroup {
        ProgrammedGroup {
            index,
            counters,
            filters: None,
            units,
        }
    }

    fn config_program(grouping: Grouping, groups: Vec<ProgrammedGroup>) -> ProgrammedBoxType {
        ProgrammedBoxType {
            box_type: BoxType::Imc,
            grouping,
            aggregate: Aggregate::Box,
            groups,
        }
    }

    fn config(program: ProgrammedBoxType) -> PmuConfig {
        PmuConfig {
            config_id: 1,
            started_at: SystemTime::UNIX_EPOCH,
            program,
        }
    }

    fn config_reading(
        package: u16,
        unit: u16,
        group: u8,
        [general, fixed]: [u64; 2],
    ) -> ConfigReading {
        ConfigReading {
            package,
            unit,
            group,
            counters: vec![
                (CounterSlot::General(0), general),
                (CounterSlot::Fixed, fixed),
            ],
        }
    }

    fn count_values(counts: &UncoreCounts) -> Vec<(u16, CountUnit, CounterSlot, u64)> {
        counts
            .counts
            .iter()
            .map(|count| (count.package, count.unit, count.slot, count.value))
            .collect()
    }

    /// Two packages of six channels each.
    fn two_package_units() -> Vec<(u16, u16)> {
        channel_units(
            [0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1].map(|package_id| ImcScope {
                die_group_id: None,
                die_id: None,
                package_id,
            }),
        )
    }

    fn measurement(
        kind: ImcEventKind,
        value: u64,
        ticks: u64,
        milliseconds: u64,
    ) -> (ImcEventKind, ImcEventMeasurement) {
        (
            kind,
            ImcEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                running: Duration::from_millis(milliseconds),
                ticks,
                value,
            },
        )
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    fn test_measurement(enabled: Duration, running: Duration) -> ImcMeasurement {
        ImcMeasurement {
            enabled,
            group: SKX_IMC_EVENT_GROUPS[0],
            running,
        }
    }

    fn slice_groups(slices: Vec<ImcMeasurementSlice>) -> Vec<ImcEventGroup> {
        slices.into_iter().map(|slice| slice.group).collect()
    }

    fn test_collector() -> SkxImcCollector {
        SkxImcCollector {
            channels: Vec::new(),
            config: None,
            groups: &SKX_IMC_EVENT_GROUPS,
            next_group: 0,
        }
    }

    fn test_scope() -> ImcScope {
        ImcScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }
}
