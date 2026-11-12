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
}

impl BoxType {
    /// The `box_type` label, also the name of the periodic collector.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cha => "cha",
            Self::Iio => "iio",
            Self::Irp => "irp",
            Self::Imc => "imc",
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
    /// Groups take turns on every selected box, switching at every sample.
    Temporal,
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
    pub aggregate: Aggregate,
    pub groups: Vec<ProgrammedGroup>,
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
