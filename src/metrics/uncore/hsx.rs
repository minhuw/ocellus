use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::arch::IntelServerCpuModel;
use crate::metal;
use crate::metal::arch::skx::pmon;
use crate::metal::pci::PciDevice;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::common::DEFAULT_MAX_SLICE;
use crate::pmu_config::types::{
    BoxType, CountAccumulator, CounterSlot, Grouping, ProgrammedBoxType, ProgrammedGroup,
    UncoreCounts, temporal_first_group,
};

pub const COUNTER_COUNT: usize = 4;
pub const COUNTER_WIDTH: u32 = 48;
pub const CTL_OFFSETS: [u64; COUNTER_COUNT] = [0xd8, 0xdc, 0xe0, 0xe4];
pub const CTR_OFFSETS: [u64; COUNTER_COUNT] = [0xa0, 0xa8, 0xb0, 0xb8];
pub const DCLK_CTL_OFFSET: u64 = 0xf0;
pub const DCLK_CTR_OFFSET: u64 = 0xd0;
pub const UNIT_CTL_OFFSET: u64 = 0xf4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct HsxUncoreScope {
    pub package_id: u32,
}

impl HsxUncoreScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HsxUncoreSpec {
    pub imc_channels: &'static [metal::pci::PciDeviceSpec],
    pub name: &'static str,
}

impl HsxUncoreSpec {
    pub fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::HaswellXeon => Some(Self {
                imc_channels: &metal::arch::hsx::pci::HASWELL_IMC_CHANNELS,
                name: "Haswell",
            }),
            IntelServerCpuModel::BroadwellXeon => Some(Self {
                imc_channels: &metal::arch::hsx::pci::BROADWELL_IMC_CHANNELS,
                name: "Broadwell",
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct HsxUncoreUnit {
    device: PciDevice,
}

impl HsxUncoreUnit {
    pub fn new(location: metal::pci::PciLocation) -> Result<Self, String> {
        Ok(Self {
            device: PciDevice::open(location)?,
        })
    }

    pub fn freeze(&self) -> Result<(), String> {
        self.device.write_u32(UNIT_CTL_OFFSET, pmon::UNIT_FREEZE)
    }

    pub fn freeze_and_reset(&self) -> Result<(), String> {
        self.device
            .write_u32(UNIT_CTL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)
    }

    pub fn program_counter(
        &self,
        counter_index: usize,
        event: u8,
        umask: u8,
    ) -> Result<(), String> {
        self.device.write_u32(
            CTL_OFFSETS[counter_index],
            pmon::counter_control(event, umask, true),
        )
    }

    pub fn read_dword(&self, offset: u64) -> Result<u32, String> {
        self.device.read_u32(offset)
    }

    pub fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
        self.device
            .read_u64(CTR_OFFSETS[counter_index])
            .map(mask_counter)
    }

    pub fn read_fixed_counter(&self) -> Result<u64, String> {
        self.device.read_u64(DCLK_CTR_OFFSET).map(mask_counter)
    }

    pub fn reset_and_enable_fixed_counter(&self) -> Result<(), String> {
        self.device
            .write_u32(DCLK_CTL_OFFSET, pmon::FIXED_COUNTER_RESET_AND_ENABLE)
    }

    pub fn unfreeze(&self) -> Result<(), String> {
        self.device.write_u32(UNIT_CTL_OFFSET, pmon::UNIT_UNFREEZE)
    }

    pub fn write(&self, offset: u64, value: u32) -> Result<(), String> {
        self.device.write_u32(offset, value)
    }

    /// A unit whose config space is a temporary file holding `config_space`.
    #[cfg(test)]
    pub(crate) fn from_config_space(config_space: &[u8]) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ocellus-hsx-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, config_space).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        Self {
            device: PciDevice::from_file(
                file,
                metal::pci::PciLocation {
                    bus: 0,
                    device: 0,
                    function: 0,
                    group: 0,
                },
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HsxSocketBusScope {
    pub bus: metal::pci::PciBus,
    pub scope: HsxUncoreScope,
}

pub fn average_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut count = 0;
    let mut sum = 0_u64;

    for value in values {
        count += 1;
        sum += value;
    }

    if count == 0 { 0 } else { sum / count }
}

pub fn bus_scopes(spec: HsxUncoreSpec) -> Result<Vec<HsxSocketBusScope>, String> {
    let scopes = package_scopes()?;
    let socket_buses = metal::arch::hsx::pci::imc_socket_buses(spec.imc_channels, scopes.len())?;

    if socket_buses.len() != scopes.len() {
        return Err(format!(
            "discovered {} {} IMC buses for {} CPU packages",
            socket_buses.len(),
            spec.name,
            scopes.len()
        ));
    }

    socket_buses
        .into_iter()
        .map(|socket_bus| {
            let scope = scopes
                .get(socket_bus.socket_index)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "failed to map {} IMC socket index {} to a CPUID package",
                        spec.name, socket_bus.socket_index
                    )
                })?;

            Ok(HsxSocketBusScope {
                bus: socket_bus.bus,
                scope,
            })
        })
        .collect()
}

pub fn package_scopes() -> Result<Vec<HsxUncoreScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(HsxUncoreScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any Haswell/Broadwell uncore scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
}

pub fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

pub fn frequency_hz(ticks: u64, duration: Duration) -> f64 {
    events_per_second(ticks, duration)
}

pub fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << COUNTER_WIDTH) - 1)
}

/// The rounds that split `interval` into slices of at most
/// `DEFAULT_MAX_SLICE`, each round one slice per group.
pub fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

pub fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

/// The units of `table` that `find` locates, each with its index in `table`
/// as its id, so an absent unit shifts no other unit's id.
pub fn present_units<T: Copy>(
    table: &[T],
    mut find: impl FnMut(T) -> Option<metal::pci::PciLocation>,
) -> Vec<(u16, metal::pci::PciLocation)> {
    table
        .iter()
        .enumerate()
        .filter_map(|(id, &unit)| Some((id as u16, find(unit)?)))
        .collect()
}

// PMU config helpers shared by the HSX IMC and HA collectors.

/// A box type of `HsxUncoreUnit`s that PMU configs program.
#[derive(Clone, Copy, Debug)]
pub struct HsxConfigBox {
    pub box_type: BoxType,
    /// The box type in errors.
    pub name: &'static str,
    /// One unit of the box type in errors.
    pub unit: &'static str,
    /// Whether the units have the DCLK fixed counter.
    pub fixed: bool,
}

impl HsxConfigBox {
    /// Errs for a program the units in `units` cannot count.
    pub fn check_config(
        self,
        program: &ProgrammedBoxType,
        units: &BTreeMap<u16, BTreeSet<u16>>,
    ) -> Result<(), String> {
        let (name, unit_name) = (self.name, self.unit);
        if program.box_type != self.box_type {
            return Err(format!(
                "Haswell-EP/Broadwell-EP {name} cannot count a {} config",
                program.box_type.label()
            ));
        }

        for group in &program.groups {
            if group.filters.is_some() {
                return Err(format!(
                    "{name} config group {} has filters, which {name} {unit_name}s do not have",
                    group.index
                ));
            }
            for (package, group_units) in &group.units {
                let host_units = units.get(package).ok_or_else(|| {
                    format!(
                        "{name} config group {} runs on package {package}, which has no {name} {unit_name}s on this host",
                        group.index
                    )
                })?;
                if let Some(unit) = group_units.difference(host_units).next() {
                    return Err(format!(
                        "{name} config group {} runs on {unit_name} {unit} of package {package}, which this host does not have",
                        group.index
                    ));
                }
            }
            self.unit_writes(group)?;
        }

        Ok(())
    }

    /// The (offset, value) writes that program one unit for `group`: freeze
    /// and reset the unit, then reset and enable each counter of the group,
    /// and write 0 to the controls of the others, DCLK included.
    pub fn unit_writes(self, group: &ProgrammedGroup) -> Result<Vec<(u64, u32)>, String> {
        let mut controls = CTL_OFFSETS
            .into_iter()
            .chain(self.fixed.then_some(DCLK_CTL_OFFSET))
            .map(|offset| (offset, 0))
            .collect::<BTreeMap<u64, u32>>();

        for counter in &group.counters {
            let (offset, _) = self
                .slot_offsets(counter.slot)
                .map_err(|error| format!("{} config group {}: {error}", self.name, group.index))?;
            let control = u32::try_from(counter.control).map_err(|_| {
                format!(
                    "{} config group {} slot {} control 0x{:x} does not fit in 32 bits",
                    self.name,
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

        Ok([(UNIT_CTL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)]
            .into_iter()
            .chain(controls)
            .collect())
    }

    /// Samples the config of `accumulator` for `interval` on `units`, each
    /// with its package and unit id, and returns its counts so far; `cycle`
    /// sets the group temporal groups start from. Units in none of the
    /// config's groups stay untouched.
    pub async fn sample_config(
        self,
        units: &[((u16, u16), &HsxUncoreUnit)],
        accumulator: &mut CountAccumulator,
        interval: Duration,
        cycle: u64,
    ) -> Result<UncoreCounts, String> {
        if interval.is_zero() {
            return Err(format!(
                "HSX {} measure interval must be non-zero",
                self.name
            ));
        }

        let ids = units.iter().map(|&(id, _)| id).collect::<Vec<_>>();
        let program = accumulator.config().program.clone();

        for slice in config_schedule(&program, interval, cycle) {
            let targets = slice_units(&ids, &program, &slice)?;
            for &(unit, group) in &targets {
                for (offset, value) in self.unit_writes(&program.groups[group])? {
                    units[unit].1.write(offset, value)?;
                }
            }

            let started_at = Instant::now();
            for &(unit, _) in &targets {
                units[unit].1.unfreeze()?;
            }
            tokio::time::sleep(slice.duration).await;
            for &(unit, _) in &targets {
                units[unit].1.freeze()?;
            }
            let running = started_at.elapsed();

            let mut readings = Vec::with_capacity(targets.len());
            for &(unit, group) in &targets {
                let group = &program.groups[group];
                let (package, id) = ids[unit];
                readings.push(ConfigReading {
                    package,
                    unit: id,
                    group: group.index,
                    counters: self.read_counters(units[unit].1, group)?,
                });
            }
            fold_readings(accumulator, &readings, running)?;
        }

        Ok(accumulator.counts())
    }

    /// The raw values of `group`'s counters on `unit`.
    fn read_counters(
        self,
        unit: &HsxUncoreUnit,
        group: &ProgrammedGroup,
    ) -> Result<Vec<(CounterSlot, u64)>, String> {
        group
            .counters
            .iter()
            .map(|counter| {
                let (_, offset) = self.slot_offsets(counter.slot)?;
                let value = read_counter_dwords(offset, |offset| unit.read_dword(offset))?;
                Ok((counter.slot, value))
            })
            .collect()
    }

    /// The control and counter offsets of a counter slot.
    fn slot_offsets(self, slot: CounterSlot) -> Result<(u64, u64), String> {
        let offsets = match slot {
            CounterSlot::General(index) => CTL_OFFSETS
                .get(usize::from(index))
                .zip(CTR_OFFSETS.get(usize::from(index)))
                .map(|(&control, &counter)| (control, counter)),
            CounterSlot::Fixed => self.fixed.then_some((DCLK_CTL_OFFSET, DCLK_CTR_OFFSET)),
        };

        offsets.ok_or_else(|| {
            format!(
                "Haswell-EP/Broadwell-EP {} {}s have no counter slot {}",
                self.name,
                self.unit,
                slot.label()
            )
        })
    }
}

/// One slice of a config sample: its groups count together.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigSlice {
    pub duration: Duration,
    /// Positions in the config's groups.
    pub groups: Vec<usize>,
}

/// The raw counter values of one unit and group in a slice.
#[derive(Debug)]
pub struct ConfigReading {
    pub package: u16,
    pub unit: u16,
    pub group: u8,
    pub counters: Vec<(CounterSlot, u64)>,
}

/// The unit ids of each package, from the package and unit id of each unit.
pub fn unit_map(ids: impl IntoIterator<Item = (u16, u16)>) -> BTreeMap<u16, BTreeSet<u16>> {
    let mut units = BTreeMap::<u16, BTreeSet<u16>>::new();
    for (package, unit) in ids {
        units.entry(package).or_default().insert(unit);
    }

    units
}

/// Fixed and spatial groups count for the whole interval in one slice;
/// temporal groups take turns in equal slices, every round in group order
/// from the group `temporal_first_group` gives for `cycle`, wrapping around.
pub fn config_schedule(
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

/// The units `slice` programs, as (position in `ids`, position in the
/// config's groups). `ids` holds each unit's package and unit id.
pub fn slice_units(
    ids: &[(u16, u16)],
    program: &ProgrammedBoxType,
    slice: &ConfigSlice,
) -> Result<Vec<(usize, usize)>, String> {
    let mut units = Vec::new();

    for &group in &slice.groups {
        for (&package, group_units) in &program.groups[group].units {
            for &unit in group_units {
                let position = ids
                    .iter()
                    .position(|&id| id == (package, unit))
                    .ok_or_else(|| {
                        format!(
                            "this host has no {} unit {unit} in package {package}",
                            program.box_type.label()
                        )
                    })?;
                units.push((position, group));
            }
        }
    }

    Ok(units)
}

/// Reads the counter at `offset` from its two dwords: high, low, high, and
/// again until the high dword holds.
pub fn read_counter_dwords(
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
pub fn fold_readings(
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

#[cfg(test)]
mod tests {
    use super::config_fixtures::*;
    use super::*;
    use crate::pmu_config::types::{Aggregate, CountUnit};

    #[test]
    fn selects_model_specific_imc_device_ids() {
        let haswell = HsxUncoreSpec::from_model(IntelServerCpuModel::HaswellXeon).unwrap();
        let broadwell = HsxUncoreSpec::from_model(IntelServerCpuModel::BroadwellXeon).unwrap();

        assert_eq!(
            haswell.imc_channels,
            &metal::arch::hsx::pci::HASWELL_IMC_CHANNELS
        );
        assert_eq!(
            broadwell.imc_channels,
            &metal::arch::hsx::pci::BROADWELL_IMC_CHANNELS
        );
        assert_ne!(haswell.imc_channels[0], broadwell.imc_channels[0]);
    }

    #[test]
    fn schedules_config_slices() {
        let units = |units: &[u16]| BTreeMap::from([(1, units.iter().copied().collect())]);
        let general = || vec![config_counter(CounterSlot::General(0), 0x40_0304)];
        let ids = two_package_ids();

        let fixed = config_program(
            BoxType::Imc,
            Grouping::Fixed,
            vec![config_group(0, general(), units(&[0, 7]))],
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
            slice_units(&ids, &fixed, &slices[0]).unwrap(),
            vec![(8, 0), (15, 0)]
        );

        let spatial = config_program(
            BoxType::Imc,
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
            slice_units(&ids, &spatial, &slices[0]).unwrap(),
            vec![(8, 0), (9, 0), (10, 1)]
        );

        let temporal = config_program(
            BoxType::Imc,
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
            slice_units(&ids, &temporal, &slices[1]).unwrap(),
            vec![(13, 1)]
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

    // The HSX IMC and HA collectors share this schedule.
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
                BoxType::Imc,
                Grouping::Fixed,
                vec![config_group(
                    0,
                    vec![
                        config_counter(CounterSlot::General(0), 0x40_0304),
                        config_counter(CounterSlot::Fixed, 0x40_0000),
                    ],
                    BTreeMap::from([(0, BTreeSet::from([1, 6])), (1, BTreeSet::from([1]))]),
                )],
            );
            program.aggregate = aggregate;
            let mut accumulator = CountAccumulator::new(config(program));
            let readings = [
                config_reading(0, 1, 0, [10, 1_000]),
                config_reading(0, 6, 0, [(0xffff << 48) | 20, (0x1 << 48) | 1_000]),
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
                        (0, CountUnit::Box(6), CounterSlot::General(0), 40),
                        (0, CountUnit::Box(6), CounterSlot::Fixed, 2_000),
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
        let general = || vec![config_counter(CounterSlot::General(0), 0x40_0304)];
        let units = || BTreeMap::from([(0, BTreeSet::from([0]))]);
        let mut accumulator = CountAccumulator::new(config(config_program(
            BoxType::Ha,
            Grouping::Temporal,
            vec![
                config_group(0, general(), units()),
                config_group(1, general(), units()),
            ],
        )));

        for _ in 0..2 {
            for (group, value, milliseconds) in [(0, 1, 250), (1, 2, 125)] {
                let reading = ConfigReading {
                    package: 0,
                    unit: 0,
                    group,
                    counters: vec![(CounterSlot::General(0), value)],
                };
                fold_readings(
                    &mut accumulator,
                    &[reading],
                    Duration::from_millis(milliseconds),
                )
                .unwrap();
            }
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
    fn keeps_the_table_index_of_present_units() {
        let present = present_units(&[10, 11, 12, 13], |unit| {
            (unit != 11).then_some(metal::pci::PciLocation {
                bus: 0x7f,
                device: unit,
                function: 0,
                group: 0,
            })
        });

        assert_eq!(
            present
                .iter()
                .map(|(id, location)| (*id, location.device))
                .collect::<Vec<_>>(),
            vec![(0, 10), (2, 12), (3, 13)]
        );
        assert_eq!(
            unit_map([(1, 3), (0, 2), (1, 0)]),
            BTreeMap::from([(0, BTreeSet::from([2])), (1, BTreeSet::from([0, 3]))])
        );
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

    /// Two packages of eight channels each.
    fn two_package_ids() -> Vec<(u16, u16)> {
        (0..2)
            .flat_map(|package| (0..8).map(move |unit| (package, unit)))
            .collect()
    }
}

/// Configs and counts for the tests of the HSX collectors.
#[cfg(test)]
pub(crate) mod config_fixtures {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::SystemTime;

    use crate::pmu_config::types::{
        Aggregate, BoxType, CountUnit, CounterSlot, EventLabels, Grouping, PmuConfig,
        ProgrammedBoxType, ProgrammedCounter, ProgrammedGroup, UncoreCounts,
    };

    pub(crate) fn config_counter(slot: CounterSlot, control: u64) -> ProgrammedCounter {
        ProgrammedCounter {
            slot,
            control,
            labels: EventLabels::new(),
        }
    }

    pub(crate) fn config_group(
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

    /// A program of per-box counts.
    pub(crate) fn config_program(
        box_type: BoxType,
        grouping: Grouping,
        groups: Vec<ProgrammedGroup>,
    ) -> ProgrammedBoxType {
        ProgrammedBoxType {
            box_type,
            grouping,
            aggregate: Aggregate::Box,
            groups,
        }
    }

    pub(crate) fn config(program: ProgrammedBoxType) -> PmuConfig {
        PmuConfig {
            config_id: 1,
            started_at: SystemTime::UNIX_EPOCH,
            program,
        }
    }

    pub(crate) fn count_values(counts: &UncoreCounts) -> Vec<(u16, CountUnit, CounterSlot, u64)> {
        counts
            .counts
            .iter()
            .map(|count| (count.package, count.unit, count.slot, count.value))
            .collect()
    }
}
