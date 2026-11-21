//! PMU config sampling shared by the Ice Lake-SP, Sapphire Rapids and
//! Emerald Rapids collectors, on the schedule and folding of the SKX IIO
//! collector.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::metrics::iio::skx::{config_schedule, config_targets, fold_config_slice};
use crate::pmu_config::types::{
    BoxType, CountAccumulator, CounterSlot, PmuConfig, ProgrammedGroup, UncoreCounts,
};

/// A box that counts one group of a config at a time.
pub(crate) trait ConfigBox {
    /// Freezes the box, resets its counters and controls, and programs
    /// `group`; counters the group leaves unused get control 0.
    fn program_group(&self, group: &ProgrammedGroup) -> Result<(), String>;
    fn freeze_box(&self) -> Result<(), String>;
    fn unfreeze_box(&self) -> Result<(), String>;
    /// The raw value of the counter in `slot`.
    fn read_slot(&self, slot: CounterSlot) -> Result<u64, String>;
}

impl<B: ConfigBox> ConfigBox for &B {
    fn program_group(&self, group: &ProgrammedGroup) -> Result<(), String> {
        (*self).program_group(group)
    }

    fn freeze_box(&self) -> Result<(), String> {
        (*self).freeze_box()
    }

    fn unfreeze_box(&self) -> Result<(), String> {
        (*self).unfreeze_box()
    }

    fn read_slot(&self, slot: CounterSlot) -> Result<u64, String> {
        (*self).read_slot(slot)
    }
}

/// Checks that `config` programs `box_type` on units of `host_units`, with no
/// filters, general counters below `counter_count`, and the fixed counter
/// only where the boxes have one (`fixed`).
pub(crate) fn check_config(
    config: &PmuConfig,
    box_type: BoxType,
    host_units: &BTreeMap<u16, BTreeSet<u16>>,
    counter_count: usize,
    fixed: bool,
) -> Result<(), String> {
    let label = box_type.label();
    if config.program.box_type != box_type {
        return Err(format!(
            "the {label} collector cannot run a config for {}",
            config.program.box_type.label()
        ));
    }

    for group in &config.program.groups {
        let path = format!("{label} config {} group {}", config.config_id, group.index);
        if group.filters.is_some() {
            return Err(format!("{path}: {label} groups take no filters"));
        }
        for counter in &group.counters {
            counter_index(counter.slot, counter_count, fixed)
                .map_err(|error| format!("{path}: {error}"))?;
        }
        for (package, units) in &group.units {
            let host = host_units
                .get(package)
                .ok_or_else(|| format!("{path}: package {package} has no {label} boxes"))?;
            if let Some(unit) = units.difference(host).next() {
                return Err(format!(
                    "{path}: unit {unit} does not exist in package {package}"
                ));
            }
        }
    }

    Ok(())
}

/// The index of general counter `slot` among `counter_count`, or `None` for
/// the fixed counter where the boxes have one (`fixed`).
pub(crate) fn counter_index(
    slot: CounterSlot,
    counter_count: usize,
    fixed: bool,
) -> Result<Option<usize>, String> {
    match slot {
        CounterSlot::General(index) if usize::from(index) < counter_count => {
            Ok(Some(usize::from(index)))
        }
        CounterSlot::Fixed if fixed => Ok(None),
        _ => Err(format!(
            "slot {} is not one of the {counter_count} general counters{}",
            slot.label(),
            if fixed { " or the fixed counter" } else { "" }
        )),
    }
}

/// The control of each of `counter_count` general counters for `group`, 0
/// for the counters it leaves unused, with `reset` added to the others.
pub(crate) fn general_controls(
    group: &ProgrammedGroup,
    counter_count: usize,
    reset: u64,
) -> Vec<u64> {
    (0..counter_count)
        .map(|index| {
            group
                .counters
                .iter()
                .find(|counter| counter.slot == CounterSlot::General(index as u8))
                .map_or(0, |counter| counter.control | reset)
        })
        .collect()
}

/// Samples the config of `accumulator` for `interval` and returns its counts
/// so far; `cycle` sets the group temporal groups start from. `find` gives
/// the box of a package and unit id. Boxes in none of the slice's groups stay
/// untouched.
pub(crate) async fn sample_config<B: ConfigBox>(
    accumulator: &mut CountAccumulator,
    interval: Duration,
    cycle: u64,
    find: impl Fn(u16, u16) -> Result<B, String>,
) -> Result<UncoreCounts, String> {
    let program = accumulator.config().program.clone();

    for slice in config_schedule(&program, interval, cycle) {
        let targets = config_targets(&program, &slice)
            .into_iter()
            .map(|target| Ok((find(target.package, target.unit)?, target)))
            .collect::<Result<Vec<_>, String>>()?;
        for (unit, target) in &targets {
            unit.program_group(target.group)?;
        }

        let started_at = Instant::now();
        for (unit, _) in &targets {
            unit.unfreeze_box()?;
        }
        tokio::time::sleep(slice.duration).await;
        for (unit, _) in &targets {
            unit.freeze_box()?;
        }
        let running = started_at.elapsed();

        let mut readings = Vec::new();
        for (unit, target) in &targets {
            for counter in &target.group.counters {
                readings.push(target.reading(counter.slot, unit.read_slot(counter.slot)?));
            }
        }
        fold_config_slice(accumulator, &slice, &readings, running)?;
    }

    Ok(accumulator.counts())
}

/// Fixtures for the config tests of the collectors that use this module.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::metrics::ConfigurableCollector;
    use crate::metrics::iio::skx::config_schedule;
    use crate::metrics::testing::{
        ScheduledSlice, assert_temporal_schedule, assert_unbiased_over_windows,
    };
    use crate::pmu_config::types::{
        Aggregate, EventLabels, Grouping, ProgrammedBoxType, ProgrammedCounter, TemporalStart,
    };

    /// A group of `index` on `units` of package `package`, with a counter of
    /// each control in `counters`.
    pub(crate) fn group(
        index: u8,
        package: u16,
        units: &[u16],
        counters: &[(CounterSlot, u64)],
    ) -> ProgrammedGroup {
        ProgrammedGroup {
            index,
            counters: counters
                .iter()
                .map(|&(slot, control)| ProgrammedCounter {
                    slot,
                    control,
                    labels: EventLabels::new(),
                })
                .collect(),
            filters: None,
            units: BTreeMap::from([(package, units.iter().copied().collect())]),
        }
    }

    /// A fixed config of `group` for `box_type`.
    pub(crate) fn config(box_type: BoxType, group: ProgrammedGroup) -> PmuConfig {
        PmuConfig {
            config_id: 1,
            started_at: UNIX_EPOCH,
            program: ProgrammedBoxType {
                box_type,
                grouping: Grouping::Fixed,
                temporal_start: TemporalStart::Precession,
                aggregate: Aggregate::Sum,
                groups: vec![group],
            },
        }
    }

    /// Asserts that config samples schedule temporal groups as every other
    /// config collector does.
    pub(crate) fn assert_config_schedule(box_type: BoxType) {
        let schedule = |program: &ProgrammedBoxType, interval: Duration, cycle: u64| {
            config_schedule(program, interval, cycle)
                .into_iter()
                .map(|slice| (slice.duration, slice.groups))
                .collect::<Vec<ScheduledSlice>>()
        };
        assert_temporal_schedule(box_type, schedule);
        assert_unbiased_over_windows(box_type, schedule);
    }

    /// Asserts that `collector`, whose boxes of `box_type` have
    /// `counter_count` general counters and a fixed counter if `fixed`,
    /// reports `units`, takes a config of all its counters on them and drops
    /// it for `None`, and rejects configs it cannot run without dropping the
    /// one it has.
    pub(crate) fn assert_runs_configs(
        collector: &mut impl ConfigurableCollector,
        box_type: BoxType,
        units: BTreeMap<u16, BTreeSet<u16>>,
        counter_count: usize,
        fixed: bool,
    ) {
        assert_eq!(collector.config_units(), units);
        let (&package, package_units) = units.last_key_value().unwrap();
        let unit = *package_units.last().unwrap();
        let mut slots = (0..counter_count as u8)
            .map(|index| (CounterSlot::General(index), 0x40_0001))
            .collect::<Vec<_>>();
        if fixed {
            slots.push((CounterSlot::Fixed, 0x40_0000));
        }

        assert!(!collector.has_config());
        collector
            .set_config(Some(config(box_type, group(0, package, &[unit], &slots))))
            .unwrap();
        assert!(collector.has_config());
        collector.set_config(None).unwrap();
        assert!(!collector.has_config());

        let label = box_type.label();
        let other = if box_type == BoxType::Cha {
            BoxType::Imc
        } else {
            BoxType::Cha
        };
        let general = [(CounterSlot::General(0), 0x40_0001)];
        let mut cases = vec![
            (
                config(other, group(0, package, &[unit], &general)),
                format!(
                    "the {label} collector cannot run a config for {}",
                    other.label()
                ),
            ),
            (
                config(box_type, group(0, package, &[unit + 1], &general)),
                format!(
                    "{label} config 1 group 0: unit {} does not exist in package {package}",
                    unit + 1
                ),
            ),
            (
                config(box_type, group(0, package + 1, &[unit], &general)),
                format!(
                    "{label} config 1 group 0: package {} has no {label} boxes",
                    package + 1
                ),
            ),
            (
                config(
                    box_type,
                    group(
                        0,
                        package,
                        &[unit],
                        &[(CounterSlot::General(counter_count as u8), 0x40_0001)],
                    ),
                ),
                format!(
                    "{label} config 1 group 0: slot {counter_count} is not one of the {counter_count} general counters{}",
                    if fixed { " or the fixed counter" } else { "" }
                ),
            ),
        ];
        if !fixed {
            cases.push((
                config(
                    box_type,
                    group(0, package, &[unit], &[(CounterSlot::Fixed, 0x40_0000)]),
                ),
                format!(
                    "{label} config 1 group 0: slot fixed is not one of the {counter_count} general counters"
                ),
            ));
        }

        collector
            .set_config(Some(config(box_type, group(0, package, &[unit], &general))))
            .unwrap();
        for (config, error) in cases {
            assert_eq!(collector.set_config(Some(config)), Err(error));
            assert!(collector.has_config());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::pmu_config::types::{
        Aggregate, CountUnit, EventLabels, Grouping, ProgrammedBoxType, ProgrammedCounter,
        TemporalStart,
    };

    fn group(index: u8, units: &[u16], slots: &[CounterSlot]) -> ProgrammedGroup {
        ProgrammedGroup {
            index,
            counters: slots
                .iter()
                .map(|&slot| ProgrammedCounter {
                    slot,
                    control: 0x40_0035,
                    labels: EventLabels::new(),
                })
                .collect(),
            filters: None,
            units: BTreeMap::from([(0, units.iter().copied().collect())]),
        }
    }

    fn config(grouping: Grouping, groups: Vec<ProgrammedGroup>) -> PmuConfig {
        PmuConfig {
            config_id: 7,
            started_at: UNIX_EPOCH,
            program: ProgrammedBoxType {
                box_type: BoxType::Cha,
                grouping,
                temporal_start: TemporalStart::Precession,
                aggregate: Aggregate::Box,
                groups,
            },
        }
    }

    /// Logs what the engine does to unit `id`; counter n reads `n + 10 *
    /// id`, with bits above the 48-bit width set.
    struct FakeBox<'a> {
        id: u16,
        log: &'a RefCell<Vec<String>>,
    }

    impl ConfigBox for FakeBox<'_> {
        fn program_group(&self, group: &ProgrammedGroup) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("program {} group {}", self.id, group.index));
            Ok(())
        }

        fn freeze_box(&self) -> Result<(), String> {
            self.log.borrow_mut().push(format!("freeze {}", self.id));
            Ok(())
        }

        fn unfreeze_box(&self) -> Result<(), String> {
            self.log.borrow_mut().push(format!("unfreeze {}", self.id));
            Ok(())
        }

        fn read_slot(&self, slot: CounterSlot) -> Result<u64, String> {
            let CounterSlot::General(index) = slot else {
                return Err("no fixed counter".to_string());
            };
            Ok((1 << 48) | (u64::from(index) + 10 * u64::from(self.id)))
        }
    }

    #[test]
    fn checks_box_type_filters_slots_and_units() {
        let host = BTreeMap::from([(0, BTreeSet::from([0, 1, 2]))]);
        let valid = config(
            Grouping::Fixed,
            vec![group(0, &[0, 2], &[CounterSlot::General(3)])],
        );
        assert_eq!(check_config(&valid, BoxType::Cha, &host, 4, false), Ok(()));
        assert_eq!(
            check_config(&valid, BoxType::Imc, &host, 4, true).unwrap_err(),
            "the imc collector cannot run a config for cha"
        );
        assert_eq!(
            check_config(&valid, BoxType::Cha, &host, 2, false).unwrap_err(),
            "cha config 7 group 0: slot 3 is not one of the 2 general counters"
        );

        let fixed = config(Grouping::Fixed, vec![group(0, &[0], &[CounterSlot::Fixed])]);
        assert_eq!(check_config(&fixed, BoxType::Cha, &host, 4, true), Ok(()));
        assert_eq!(
            check_config(&fixed, BoxType::Cha, &host, 4, false).unwrap_err(),
            "cha config 7 group 0: slot fixed is not one of the 4 general counters"
        );

        let absent = config(Grouping::Fixed, vec![group(0, &[3], &[])]);
        assert_eq!(
            check_config(&absent, BoxType::Cha, &host, 4, false).unwrap_err(),
            "cha config 7 group 0: unit 3 does not exist in package 0"
        );
        let mut other_package = absent.clone();
        other_package.program.groups[0].units = BTreeMap::from([(1, BTreeSet::from([0]))]);
        assert_eq!(
            check_config(&other_package, BoxType::Cha, &host, 4, false).unwrap_err(),
            "cha config 7 group 0: package 1 has no cha boxes"
        );
    }

    #[test]
    fn controls_reset_used_counters_and_zero_the_others() {
        let group = group(0, &[0], &[CounterSlot::General(1), CounterSlot::General(3)]);
        assert_eq!(
            general_controls(&group, 4, 1 << 17),
            [0, 0x42_0035, 0, 0x42_0035]
        );
        assert_eq!(general_controls(&group, 4, 0), [0, 0x40_0035, 0, 0x40_0035]);
    }

    #[test]
    fn samples_each_slice_on_the_boxes_of_its_groups() {
        let log = RefCell::new(Vec::new());
        let mut accumulator = CountAccumulator::new(config(
            Grouping::Temporal,
            vec![
                group(0, &[1, 2], &[CounterSlot::General(0)]),
                group(1, &[1, 2], &[CounterSlot::General(2)]),
            ],
        ));

        let counts = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(sample_config(
                &mut accumulator,
                Duration::from_millis(2),
                1,
                |_, id| Ok(FakeBox { id, log: &log }),
            ))
            .unwrap();

        // Cycle 1 of two precessing groups starts with group 1.
        assert_eq!(
            log.into_inner(),
            [
                "program 1 group 1",
                "program 2 group 1",
                "unfreeze 1",
                "unfreeze 2",
                "freeze 1",
                "freeze 2",
                "program 1 group 0",
                "program 2 group 0",
                "unfreeze 1",
                "unfreeze 2",
                "freeze 1",
                "freeze 2",
            ]
        );
        let values = counts
            .counts
            .iter()
            .map(|count| (count.unit, count.group, count.value))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                (CountUnit::Box(1), 0, 10),
                (CountUnit::Box(1), 1, 12),
                (CountUnit::Box(2), 0, 20),
                (CountUnit::Box(2), 1, 22),
            ]
        );
        let running = counts
            .running
            .iter()
            .map(|running| (running.group, running.seconds > 0.0))
            .collect::<Vec<_>>();
        assert_eq!(running, [(0, true), (1, true)]);
    }

    #[test]
    fn fails_on_a_box_it_cannot_find() {
        let mut accumulator = CountAccumulator::new(config(
            Grouping::Fixed,
            vec![group(0, &[1], &[CounterSlot::General(0)])],
        ));

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(sample_config(
                &mut accumulator,
                Duration::from_millis(1),
                0,
                |package, unit| -> Result<FakeBox<'_>, String> {
                    Err(format!("no unit {unit} in package {package}"))
                },
            ));

        assert_eq!(result.unwrap_err(), "no unit 1 in package 0");
    }
}
