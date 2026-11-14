//! Types shared by config validation (`spec`), the periodic collectors that
//! run a config, and its export.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

/// Uncore PMU types a config can program. The periodic collector of the same
/// name counts the config while it is set.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum BoxType {
    Cha,
    Iio,
    Irp,
    Imc,
    /// Home agent; Haswell-EP and Broadwell-EP only.
    Ha,
}

impl BoxType {
    /// The `box_type` label, also the name of the periodic collector.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cha => "cha",
            Self::Iio => "iio",
            Self::Irp => "irp",
            Self::Imc => "imc",
            Self::Ha => "ha",
        }
    }
}

/// How the groups of one box type share its boxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum Grouping {
    /// One group on every selected box for as long as the config is set.
    Fixed,
    /// Groups count at the same time, each on its own disjoint set of units.
    Spatial,
    /// Groups take turns on every selected box within every sample, from the
    /// group [`temporal_first_group`] gives.
    Temporal,
}

/// Which group each sample of temporal groups starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum TemporalStart {
    /// The sample of index `cycle` starts at group `cycle` mod n, one group
    /// later than the sample before, so any n consecutive samples give every
    /// group every slice position once. The default.
    Precession,
    /// Cycles split into blocks of n from cycle 0; the sample `cycle` of
    /// block k = `cycle` / n starts at group (`cycle` + r_k) mod n, where r_k
    /// = SplitMix64(k) mod n. Within a block every group counts at every
    /// slice position once, and across blocks a fixed instant moves between
    /// positions pseudo-randomly.
    Random,
}

impl TemporalStart {
    fn is_precession(&self) -> bool {
        *self == Self::Precession
    }
}

/// What one exported count series covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum Aggregate {
    /// One series per box: `unit` is the box id.
    Box,
    /// One series per package and group, summed over the group's units:
    /// `unit` is "sum".
    Sum,
}

/// A counter of a box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum CounterSlot {
    /// General-purpose counter `n`.
    General(u8),
    /// The box's fixed counter (IMC DCLK on SKX).
    Fixed,
}

impl CounterSlot {
    /// The `slot` label: the counter index, or "fixed".
    pub fn label(self) -> String {
        match self {
            Self::General(index) => index.to_string(),
            Self::Fixed => "fixed".to_string(),
        }
    }
}

/// What a counter counts, exported as labels of `ocellus_uncore_event_info`
/// (for example `alias`, `event`, `umask`, `ch_mask`, `fc_mask`). Values are
/// hex strings for register fields.
pub type EventLabels = BTreeMap<String, String>;

/// One counter, resolved to its control register value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProgrammedCounter {
    pub slot: CounterSlot,
    /// Control register value with the enable bit set and the reset bit
    /// clear; the collector adds the reset bit when it programs the counter.
    pub control: u64,
    pub labels: EventLabels,
}

/// Box-level filter registers of a group (SKX CHA filter0 and filter1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ChaFilters {
    pub filter0: u64,
    pub filter1: u64,
}

/// One event group of a box type.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProgrammedGroup {
    /// Index of the group in the request; the `group` label.
    pub index: u8,
    /// Programmed counters in slot order; unused slots are absent.
    pub counters: Vec<ProgrammedCounter>,
    /// Filter registers, present only for box types that have them.
    pub filters: Option<ChaFilters>,
    /// Unit ids the group runs on, per package.
    pub units: BTreeMap<u16, BTreeSet<u16>>,
}

/// The programming of one box type in a config.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProgrammedBoxType {
    pub box_type: BoxType,
    pub grouping: Grouping,
    /// Precession for fixed and spatial groups. The JSON state leaves out
    /// precession, as it did before the field.
    #[serde(skip_serializing_if = "TemporalStart::is_precession")]
    pub temporal_start: TemporalStart,
    pub aggregate: Aggregate,
    pub groups: Vec<ProgrammedGroup>,
}

impl ProgrammedBoxType {
    /// The `group` of the temporal group that counts first in the sample of
    /// index `cycle`; `None` for fixed and spatial groups, which count at once.
    pub fn first_group(&self, cycle: u64) -> Option<u8> {
        (self.grouping == Grouping::Temporal).then(|| {
            self.groups[temporal_first_group(self.temporal_start, cycle, self.groups.len())].index
        })
    }
}

/// The position of the group that counts first in the sample of index
/// `cycle` (`metrics::sample::cycle_index`), of `group_count` temporal
/// groups that start as `start` says.
pub fn temporal_first_group(start: TemporalStart, cycle: u64, group_count: usize) -> usize {
    let n = group_count as u64;
    let offset = match start {
        TemporalStart::Precession => 0,
        TemporalStart::Random => splitmix64(cycle / n) % n,
    };

    ((cycle % n + offset) % n) as usize
}

/// The SplitMix64 finalizer.
fn splitmix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The boxes of this host a config can program: for each box type and
/// package, the unit ids (CHA id, IIO/IRP stack, IMC channel).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PmuTopology {
    pub units: BTreeMap<BoxType, BTreeMap<u16, BTreeSet<u16>>>,
}

/// A box type's programming as its periodic collector runs it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PmuConfig {
    /// Hash of `program`, below 2^53: equal programming gives an equal id.
    pub config_id: u64,
    /// When the counts restarted from 0.
    pub started_at: SystemTime,
    pub program: ProgrammedBoxType,
}

/// The unit a count series covers: one box, or the sum over a group's units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum CountUnit {
    Box(u16),
    Sum,
}

impl CountUnit {
    /// The `unit` label: the box id, or "sum".
    pub fn label(self) -> String {
        match self {
            Self::Box(id) => id.to_string(),
            Self::Sum => "sum".to_string(),
        }
    }
}

/// A counter's count since its config started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct UnitCount {
    pub package: u16,
    pub unit: CountUnit,
    pub group: u8,
    pub slot: CounterSlot,
    pub value: u64,
}

/// How long a group has counted since its config started.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct GroupRunning {
    pub package: u16,
    pub group: u8,
    pub seconds: f64,
}

/// One sample of a config: raw counts cumulative since it started.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct UncoreCounts {
    pub config: PmuConfig,
    pub counts: Vec<UnitCount>,
    pub running: Vec<GroupRunning>,
    /// The `group` that counted first in this sample, for temporal groups;
    /// the collector loop sets it from the sample's cycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_group: Option<u8>,
}

/// Accumulates the per-slice counts and running time of one config.
#[derive(Debug)]
pub struct CountAccumulator {
    config: PmuConfig,
    /// Keyed by package, unit, group and slot.
    counts: BTreeMap<(u16, u16, u8, CounterSlot), u64>,
    /// Keyed by package and group.
    running: BTreeMap<(u16, u8), Duration>,
}

impl CountAccumulator {
    /// Starts every counter and group of `config` at 0.
    pub fn new(config: PmuConfig) -> Self {
        let mut counts = BTreeMap::new();
        let mut running = BTreeMap::new();
        for group in &config.program.groups {
            for (&package, units) in &group.units {
                running.insert((package, group.index), Duration::ZERO);
                for &unit in units {
                    for counter in &group.counters {
                        counts.insert((package, unit, group.index, counter.slot), 0);
                    }
                }
            }
        }

        Self {
            config,
            counts,
            running,
        }
    }

    pub fn config(&self) -> &PmuConfig {
        &self.config
    }

    /// Adds one slice's count of a counter. Errs for a counter the config
    /// does not program.
    pub fn add_count(
        &mut self,
        package: u16,
        unit: u16,
        group: u8,
        slot: CounterSlot,
        count: u64,
    ) -> Result<(), String> {
        let total = self
            .counts
            .get_mut(&(package, unit, group, slot))
            .ok_or_else(|| {
                format!(
                    "{} config {} does not program package {package} unit {unit} group {group} slot {}",
                    self.config.program.box_type.label(),
                    self.config.config_id,
                    slot.label()
                )
            })?;
        *total += count;
        Ok(())
    }

    /// Adds one slice's running time of a group. Errs for a group the config
    /// does not run in `package`.
    pub fn add_running(
        &mut self,
        package: u16,
        group: u8,
        running: Duration,
    ) -> Result<(), String> {
        let total = self.running.get_mut(&(package, group)).ok_or_else(|| {
            format!(
                "{} config {} does not run group {group} in package {package}",
                self.config.program.box_type.label(),
                self.config.config_id
            )
        })?;
        *total += running;
        Ok(())
    }

    /// The counts so far, per box or summed over each group's units as the
    /// config's aggregate says.
    pub fn counts(&self) -> UncoreCounts {
        let mut counts = BTreeMap::<(u16, CountUnit, u8, CounterSlot), u64>::new();
        for (&(package, unit, group, slot), &value) in &self.counts {
            let unit = match self.config.program.aggregate {
                Aggregate::Box => CountUnit::Box(unit),
                Aggregate::Sum => CountUnit::Sum,
            };
            *counts.entry((package, unit, group, slot)).or_default() += value;
        }

        UncoreCounts {
            config: self.config.clone(),
            counts: counts
                .into_iter()
                .map(|((package, unit, group, slot), value)| UnitCount {
                    package,
                    unit,
                    group,
                    slot,
                    value,
                })
                .collect(),
            running: self
                .running
                .iter()
                .map(|(&(package, group), running)| GroupRunning {
                    package,
                    group,
                    seconds: running.as_secs_f64(),
                })
                .collect(),
            first_group: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(aggregate: Aggregate) -> PmuConfig {
        let counter = |slot| ProgrammedCounter {
            slot,
            control: 0x40_0035,
            labels: EventLabels::new(),
        };
        PmuConfig {
            config_id: 7,
            started_at: SystemTime::UNIX_EPOCH,
            program: ProgrammedBoxType {
                box_type: BoxType::Cha,
                grouping: Grouping::Fixed,
                temporal_start: TemporalStart::Precession,
                aggregate,
                groups: vec![ProgrammedGroup {
                    index: 0,
                    counters: vec![
                        counter(CounterSlot::General(0)),
                        counter(CounterSlot::General(2)),
                    ],
                    filters: None,
                    units: BTreeMap::from([(0, BTreeSet::from([3, 5])), (1, BTreeSet::from([3]))]),
                }],
            },
        }
    }

    fn value(counts: &UncoreCounts, package: u16, unit: CountUnit, slot: CounterSlot) -> u64 {
        counts
            .counts
            .iter()
            .find(|count| count.package == package && count.unit == unit && count.slot == slot)
            .unwrap()
            .value
    }

    #[test]
    fn sums_slices_per_box_or_per_package() {
        for aggregate in [Aggregate::Box, Aggregate::Sum] {
            let mut accumulator = CountAccumulator::new(config(aggregate));
            for _ in 0..2 {
                accumulator
                    .add_count(0, 3, 0, CounterSlot::General(0), 10)
                    .unwrap();
                accumulator
                    .add_count(0, 5, 0, CounterSlot::General(0), 1)
                    .unwrap();
                accumulator
                    .add_running(0, 0, Duration::from_millis(500))
                    .unwrap();
            }
            let counts = accumulator.counts();

            match aggregate {
                Aggregate::Box => {
                    assert_eq!(counts.counts.len(), 6);
                    assert_eq!(
                        value(&counts, 0, CountUnit::Box(3), CounterSlot::General(0)),
                        20
                    );
                    assert_eq!(
                        value(&counts, 0, CountUnit::Box(5), CounterSlot::General(0)),
                        2
                    );
                }
                Aggregate::Sum => {
                    assert_eq!(counts.counts.len(), 4);
                    assert_eq!(
                        value(&counts, 0, CountUnit::Sum, CounterSlot::General(0)),
                        22
                    );
                    assert_eq!(
                        value(&counts, 1, CountUnit::Sum, CounterSlot::General(2)),
                        0
                    );
                }
            }
            assert_eq!(counts.running.len(), 2);
            assert_eq!(counts.running[0].seconds, 1.0);
            assert_eq!(counts.running[1].seconds, 0.0);
        }
    }

    #[test]
    fn mixes_block_indices_with_the_splitmix64_finalizer() {
        // The first outputs of SplitMix64 seeded with 0.
        let mut state = 0_u64;
        for expected in [
            0xe220_a839_7b1d_cdaf,
            0x6e78_9e6a_a1b9_65f4,
            0x06c4_5d18_8009_454f,
        ] {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            assert_eq!(splitmix64(state), expected);
        }
    }

    #[test]
    fn starts_temporal_groups_at_the_same_group_for_the_same_cycle() {
        for (cycle, group_count, precession, random) in [
            (0, 3, 0, 0),
            (3, 3, 0, 1),
            (1_763_000_005, 3, 1, 1),
            (13, 8, 5, 2),
            (11, 11, 0, 2),
            (u64::MAX, 11, 4, 1),
            (1_763_000_067, 12, 3, 0),
            (1_763_000_068, 12, 4, 1),
            (5, 1, 0, 0),
        ] {
            for (start, expected) in [
                (TemporalStart::Precession, precession),
                (TemporalStart::Random, random),
            ] {
                assert_eq!(
                    temporal_first_group(start, cycle, group_count),
                    expected,
                    "{start:?}, {group_count} groups, cycle {cycle}"
                );
            }
        }

        let mut program = config(Aggregate::Box).program;
        program.groups = (0..3)
            .map(|index| ProgrammedGroup {
                index,
                ..program.groups[0].clone()
            })
            .collect();
        for (grouping, start, expected) in [
            (Grouping::Fixed, TemporalStart::Precession, None),
            (Grouping::Spatial, TemporalStart::Precession, None),
            (Grouping::Temporal, TemporalStart::Precession, Some(0)),
            (Grouping::Temporal, TemporalStart::Random, Some(1)),
        ] {
            program.grouping = grouping;
            program.temporal_start = start;
            assert_eq!(program.first_group(3), expected, "{grouping:?}, {start:?}");
        }
    }

    #[test]
    fn leaves_the_default_temporal_start_out_of_the_json_state() {
        let mut program = config(Aggregate::Box).program;
        program.grouping = Grouping::Temporal;
        let json = serde_json::to_value(&program).unwrap();
        assert!(json.get("temporal_start").is_none(), "{json}");

        program.temporal_start = TemporalStart::Random;
        assert_eq!(
            serde_json::to_value(&program).unwrap()["temporal_start"],
            "Random"
        );
    }

    #[test]
    fn rejects_counters_the_config_does_not_program() {
        let mut accumulator = CountAccumulator::new(config(Aggregate::Sum));

        assert!(
            accumulator
                .add_count(0, 4, 0, CounterSlot::General(0), 1)
                .is_err()
        );
        assert!(
            accumulator
                .add_count(0, 3, 0, CounterSlot::General(1), 1)
                .is_err()
        );
        assert!(
            accumulator
                .add_count(0, 3, 1, CounterSlot::General(0), 1)
                .is_err()
        );
        assert!(
            accumulator
                .add_running(2, 0, Duration::from_secs(1))
                .is_err()
        );
    }
}
