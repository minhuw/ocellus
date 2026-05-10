use std::collections::BTreeMap;
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
use crate::metrics::common::{BYTES_PER_CACHE_LINE, DEFAULT_MAX_SLICE};

const IMC_COUNTER_WIDTH: u32 = 48;
const IMC_CTL_OFFSETS: [u64; 4] = [0xd8, 0xdc, 0xe0, 0xe4];
const IMC_CTR_OFFSETS: [u64; 4] = [0xa0, 0xa8, 0xb0, 0xb8];
const IMC_DCLK_CTL_OFFSET: u64 = 0xf0;
const IMC_DCLK_CTR_OFFSET: u64 = 0xd0;
const IMC_UNIT_CTL_OFFSET: u64 = 0xf4;

const SKX_IMC_EVENT_GROUPS: [ImcEventGroup; 2] = [
    ImcEventGroup {
        events: [
            ImcEventSpec::sum(ImcEventKind::ReadInsert, 0x10),
            ImcEventSpec::sum(ImcEventKind::WriteInsert, 0x20),
            ImcEventSpec::sum(ImcEventKind::ReadOccupancy, 0x80),
            ImcEventSpec::sum(ImcEventKind::WriteOccupancy, 0x81),
        ],
    },
    ImcEventGroup {
        events: [
            ImcEventSpec::sum_with_umask(ImcEventKind::ReadCas, 0x04, 0x03),
            ImcEventSpec::sum_with_umask(ImcEventKind::WriteCas, 0x04, 0x0c),
            ImcEventSpec::sum_with_umask(ImcEventKind::Activate, 0x01, 0x0b),
            ImcEventSpec::sum_with_umask(ImcEventKind::PageMissPrecharge, 0x02, 0x01),
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ImcScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl ImcScope {
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

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ImcScopeMetrics {
    #[serde(flatten)]
    pub scope: ImcScope,
    pub activate_commands_per_second: f64,
    pub frequency_hz: f64,
    pub page_miss_precharge_commands_per_second: f64,
    pub read_cas_commands_per_second: f64,
    pub read_bytes_per_second: f64,
    pub rpq_residency_seconds: f64,
    pub rpq_occupancy_entries: f64,
    pub write_cas_commands_per_second: f64,
    pub write_bytes_per_second: f64,
    pub wpq_residency_seconds: f64,
    pub wpq_occupancy_entries: f64,
}

impl ImcScopeMetrics {
    fn from_measurements(
        scope: ImcScope,
        measurements: &BTreeMap<ImcEventKind, ImcEventMeasurement>,
    ) -> Result<Self, String> {
        let read_insert = required_measurement(measurements, ImcEventKind::ReadInsert)?;
        let write_insert = required_measurement(measurements, ImcEventKind::WriteInsert)?;
        let read_occupancy = required_measurement(measurements, ImcEventKind::ReadOccupancy)?;
        let write_occupancy = required_measurement(measurements, ImcEventKind::WriteOccupancy)?;
        let activate = required_measurement(measurements, ImcEventKind::Activate)?;
        let page_miss_precharge =
            required_measurement(measurements, ImcEventKind::PageMissPrecharge)?;
        let read_cas = required_measurement(measurements, ImcEventKind::ReadCas)?;
        let write_cas = required_measurement(measurements, ImcEventKind::WriteCas)?;

        Ok(Self {
            scope,
            activate_commands_per_second: command_rate(activate),
            frequency_hz: frequency_hz(read_insert),
            page_miss_precharge_commands_per_second: command_rate(page_miss_precharge),
            read_cas_commands_per_second: command_rate(read_cas),
            read_bytes_per_second: bytes_per_second(
                scale_to_enabled(read_cas.value, read_cas.enabled, read_cas.running),
                read_cas.enabled,
            ),
            rpq_residency_seconds: queue_residency_seconds(read_occupancy, read_insert),
            rpq_occupancy_entries: ratio(read_occupancy.value, read_occupancy.ticks),
            write_cas_commands_per_second: command_rate(write_cas),
            write_bytes_per_second: bytes_per_second(
                scale_to_enabled(write_cas.value, write_cas.enabled, write_cas.running),
                write_cas.enabled,
            ),
            wpq_residency_seconds: queue_residency_seconds(write_occupancy, write_insert),
            wpq_occupancy_entries: ratio(write_occupancy.value, write_occupancy.ticks),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ImcMetrics {
    pub scopes: Vec<ImcScopeMetrics>,
}

impl ImcMetrics {
    fn from_measurements(
        measurements: BTreeMap<ImcScope, BTreeMap<ImcEventKind, ImcEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut scopes = Vec::with_capacity(measurements.len());

        for (scope, scope_measurements) in measurements {
            scopes.push(ImcScopeMetrics::from_measurements(
                scope,
                &scope_measurements,
            )?);
        }

        Ok(Self { scopes })
    }
}

#[derive(Debug)]
pub struct SkxImcCollector {
    channels: Vec<ImcChannel>,
    next_group: usize,
}

impl SkxImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        Ok(Self {
            channels: discover_channels(architecture.intel_server_model())?,
            next_group: 0,
        })
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

        ImcMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_group(&mut self) {
        self.next_group = (self.next_group + 1) % SKX_IMC_EVENT_GROUPS.len();
    }

    fn schedule(&self, interval: Duration) -> Vec<ImcMeasurementSlice> {
        let group_count = SKX_IMC_EVENT_GROUPS.len();
        let round_count = measurement_round_count(interval, group_count);
        let slice_count = group_count * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for _ in 0..round_count {
            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                slices.push(ImcMeasurementSlice {
                    duration: slice_duration,
                    group: SKX_IMC_EVENT_GROUPS[rotated_index],
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
            self.rpq_residency_seconds
                .get_or_create(&labels)
                .set(scope.rpq_residency_seconds);
            self.rpq_occupancy_entries
                .get_or_create(&labels)
                .set(scope.rpq_occupancy_entries);
            self.write_cas_commands_per_second
                .get_or_create(&labels)
                .set(scope.write_cas_commands_per_second);
            self.write_bytes_per_second
                .get_or_create(&labels)
                .set(scope.write_bytes_per_second);
            self.wpq_residency_seconds
                .get_or_create(&labels)
                .set(scope.wpq_residency_seconds);
            self.wpq_occupancy_entries
                .get_or_create(&labels)
                .set(scope.wpq_occupancy_entries);
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
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
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
            ticks: self.device.read_u64(IMC_DCLK_CTR_OFFSET)?,
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
    if insert.ticks == 0 || insert.value == 0 {
        return 0.0;
    }

    let seconds_per_tick = insert.running.as_secs_f64() / insert.ticks as f64;
    seconds_per_tick * ratio(occupancy.value, insert.value)
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
    use super::*;

    #[test]
    fn computes_multiplexed_metrics() {
        let scope = test_scope();
        let metrics = ImcMetrics::from_measurements(BTreeMap::from([(
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
        )]))
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
        assert_eq!(scope_metrics.rpq_occupancy_entries, 0.4);
        assert_eq!(scope_metrics.wpq_occupancy_entries, 0.6);
        assert_eq!(scope_metrics.rpq_residency_seconds, 0.0002);
        assert_eq!(scope_metrics.wpq_residency_seconds, 0.0002);
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
            next_group: 0,
        }
    }

    fn test_scope() -> ImcScope {
        ImcScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        }
    }
}
