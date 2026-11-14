//! Test fixtures for PMU configs.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::metrics::{ConfigurableCollector, MetricUpdate};
use crate::pmu_config::types::{
    Aggregate, BoxType, ChaFilters, CountAccumulator, CountUnit, CounterSlot, EventLabels,
    GroupRunning, Grouping, PmuConfig, ProgrammedBoxType, ProgrammedCounter, ProgrammedGroup,
    TemporalStart, UncoreCounts, UnitCount,
};

/// A config of two counters in one group on units 0-2 and 4 of package 0,
/// started `started_at_ms` after the Unix epoch. CHA configs carry filters.
pub fn pmu_config(box_type: BoxType, config_id: u64, started_at_ms: u64) -> PmuConfig {
    let counter = |slot, umask: &str| {
        let mut labels = vec![("alias", "raw"), ("event", "0x35"), ("umask", umask)];
        if box_type == BoxType::Cha {
            labels.extend([("filter0", "0x0"), ("filter1", "0x3b")]);
        }
        ProgrammedCounter {
            slot,
            control: 0x40_0035,
            labels: labels
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    };

    PmuConfig {
        config_id,
        started_at: UNIX_EPOCH + Duration::from_millis(started_at_ms),
        program: ProgrammedBoxType {
            box_type,
            grouping: Grouping::Fixed,
            temporal_start: TemporalStart::Precession,
            aggregate: Aggregate::Sum,
            groups: vec![ProgrammedGroup {
                index: 0,
                counters: vec![
                    counter(CounterSlot::General(0), "0x34"),
                    counter(CounterSlot::General(1), "0x24"),
                ],
                filters: (box_type == BoxType::Cha).then_some(ChaFilters {
                    filter0: 0,
                    filter1: 0x3b,
                }),
                units: BTreeMap::from([(0, BTreeSet::from([0, 1, 2, 4]))]),
            }],
        },
    }
}

/// Counts `value` on counter 0 and `2 * value` on counter 1, over half a
/// second.
pub fn uncore_counts(config: PmuConfig, value: u64) -> UncoreCounts {
    UncoreCounts {
        config,
        counts: [(0, value), (1, 2 * value)]
            .into_iter()
            .map(|(slot, value)| UnitCount {
                package: 0,
                unit: CountUnit::Sum,
                group: 0,
                slot: CounterSlot::General(slot),
                value,
            })
            .collect(),
        running: vec![GroupRunning {
            package: 0,
            group: 0,
            seconds: 0.5,
        }],
        first_group: None,
    }
}

/// A slice of a config sample: its duration and the positions of its groups
/// in the config.
pub type ScheduledSlice = (Duration, Vec<usize>);

/// A per-box config of `group_count` groups of one counter, each on units 0
/// and 1 of packages 0 and 1.
fn schedule_program(
    box_type: BoxType,
    grouping: Grouping,
    temporal_start: TemporalStart,
    group_count: u8,
) -> ProgrammedBoxType {
    ProgrammedBoxType {
        box_type,
        grouping,
        temporal_start,
        aggregate: Aggregate::Box,
        groups: (0..group_count)
            .map(|index| ProgrammedGroup {
                index,
                counters: vec![ProgrammedCounter {
                    slot: CounterSlot::General(0),
                    control: 0x40_0035,
                    labels: EventLabels::new(),
                }],
                filters: None,
                units: (0..2)
                    .map(|package| (package, BTreeSet::from([0, 1])))
                    .collect(),
            })
            .collect(),
    }
}

/// Asserts the slices `schedule` gives for a cycle: fixed and spatial groups
/// count together for the whole interval, and temporal groups take turns in
/// rounds, each round from the config's `first_group`. Under precession any
/// n consecutive cycles give each group each slice position once; under the
/// random start every block of n cycles from a multiple of n does.
pub fn assert_temporal_schedule(
    box_type: BoxType,
    schedule: impl Fn(&ProgrammedBoxType, Duration, u64) -> Vec<ScheduledSlice>,
) {
    let second = Duration::from_secs(1);
    let program = |grouping, temporal_start, group_count| {
        schedule_program(box_type, grouping, temporal_start, group_count)
    };
    let starts = [TemporalStart::Precession, TemporalStart::Random];
    let slices = |dwell: Duration, groups: &[usize]| {
        groups
            .iter()
            .map(|&group| (dwell, vec![group]))
            .collect::<Vec<_>>()
    };

    for cycle in [0, 1, 7, u64::MAX] {
        assert_eq!(
            schedule(
                &program(Grouping::Fixed, TemporalStart::Precession, 1),
                second,
                cycle
            ),
            vec![(second, vec![0])]
        );
        assert_eq!(
            schedule(
                &program(Grouping::Spatial, TemporalStart::Precession, 3),
                second,
                cycle
            ),
            vec![(second, vec![0, 1, 2])]
        );
        for start in starts {
            assert_eq!(
                schedule(&program(Grouping::Temporal, start, 1), second, cycle),
                slices(Duration::from_millis(100), &[0; 10])
            );
        }
    }

    for (cycle, precession, random) in [
        (0, [0, 1, 2], [0, 1, 2]),
        (1, [1, 2, 0], [1, 2, 0]),
        (2, [2, 0, 1], [2, 0, 1]),
        (3, [0, 1, 2], [1, 2, 0]),
        (4, [1, 2, 0], [2, 0, 1]),
        (5, [2, 0, 1], [0, 1, 2]),
        (1_763_000_005, [1, 2, 0], [1, 2, 0]),
    ] {
        for (start, groups) in starts.into_iter().zip([precession, random]) {
            assert_eq!(
                schedule(
                    &program(Grouping::Temporal, start, 3),
                    Duration::from_millis(300),
                    cycle
                ),
                slices(Duration::from_millis(100), &groups),
                "{start:?}, cycle {cycle}"
            );
        }
    }

    // Two rounds of eight.
    for (cycle, precession, random) in [
        (5, [5, 6, 7, 0, 1, 2, 3, 4], [5, 6, 7, 0, 1, 2, 3, 4]),
        (13, [5, 6, 7, 0, 1, 2, 3, 4], [2, 3, 4, 5, 6, 7, 0, 1]),
    ] {
        for (start, groups) in starts.into_iter().zip([precession, random]) {
            assert_eq!(
                schedule(&program(Grouping::Temporal, start, 8), second, cycle),
                slices(Duration::from_micros(62_500), &groups.repeat(2)),
                "{start:?}, cycle {cycle}"
            );
        }
    }

    for (cycle, precession, random) in [
        (
            0,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        ),
        (
            10,
            [10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
            [10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        ),
        (
            11,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            [2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 1],
        ),
        (
            u64::MAX,
            [4, 5, 6, 7, 8, 9, 10, 0, 1, 2, 3],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0],
        ),
    ] {
        for (start, groups) in starts.into_iter().zip([precession, random]) {
            assert_eq!(
                schedule(&program(Grouping::Temporal, start, 11), second, cycle),
                slices(second.div_f64(11.0), &groups),
                "{start:?}, cycle {cycle}"
            );
        }
    }

    for group_count in [1, 2, 12, 14] {
        let n = usize::from(group_count);
        let first_block = 1_763_000_000 / n as u64;
        // The first cycles of the windows of n cycles the rule balances:
        // every cycle under precession, every multiple of n under the random
        // start.
        let precession_windows = (0..2 * n as u64)
            .chain(first_block * n as u64..first_block * n as u64 + 1_000)
            .collect::<Vec<_>>();
        let block_windows = std::iter::once(0)
            .chain(first_block..first_block + 1_000)
            .map(|block| block * n as u64)
            .collect::<Vec<_>>();
        for (start, windows) in starts.into_iter().zip([precession_windows, block_windows]) {
            let temporal = program(Grouping::Temporal, start, group_count);
            for window in windows {
                // The slice positions each group counted at in the window.
                let mut positions = vec![Vec::new(); n];
                for cycle in window..window + n as u64 {
                    let slices = schedule(&temporal, second, cycle);
                    assert_eq!(slices.len() % n, 0, "cycle {cycle}");
                    for (slot, (_, groups)) in slices.iter().enumerate() {
                        assert_eq!(groups, &slices[slot % n].1, "cycle {cycle}");
                    }
                    if start == TemporalStart::Precession {
                        assert_eq!(slices[0].1, [cycle as usize % n], "cycle {cycle}");
                    }
                    assert_eq!(
                        temporal.first_group(cycle),
                        Some(temporal.groups[slices[0].1[0]].index),
                        "{start:?}, cycle {cycle}"
                    );
                    for (position, (_, groups)) in slices[..n].iter().enumerate() {
                        positions[groups[0]].push(position);
                    }
                }
                for (group, mut positions) in positions.into_iter().enumerate() {
                    positions.sort_unstable();
                    assert_eq!(
                        positions,
                        (0..n).collect::<Vec<_>>(),
                        "{start:?}, {n} groups, window from {window}, group {group}"
                    );
                }
            }
        }
    }

    // A source at second 27 of every 120. Under precession group 0 meets it
    // at one slice position every time; the random start moves it around.
    for (start, position_count) in starts.into_iter().zip([1, 12]) {
        let twelve = program(Grouping::Temporal, start, 12);
        let positions = (0..100)
            .map(|period| {
                let cycle = 1_763_000_067 + 120 * period;
                schedule(&twelve, second, cycle)
                    .iter()
                    .position(|(_, groups)| groups == &[0])
                    .unwrap()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(positions.len(), position_count, "{start:?}: {positions:?}");
    }
}

/// The rate of a source that [`phase_locked_rates`] simulates outside its
/// burst, per second.
const BASE_RATE: f64 = 1e9;
/// The count a phase-locked source adds once every sample.
const BURST: u64 = 300_000_000;
/// Where in every sample the burst lands, as a fraction of the interval.
const BURST_PHASE: f64 = 0.37;

/// Each group's rate, its count over its running seconds summed over the
/// samples of `cycles`, of a source that counts `BASE_RATE` plus `BURST` at
/// `BURST_PHASE` of every sample, on the slices `schedule` gives.
fn phase_locked_rates(
    program: &ProgrammedBoxType,
    interval: Duration,
    cycles: impl IntoIterator<Item = u64>,
    schedule: &impl Fn(&ProgrammedBoxType, Duration, u64) -> Vec<ScheduledSlice>,
) -> BTreeMap<u8, f64> {
    let mut accumulator = CountAccumulator::new(PmuConfig {
        config_id: 1,
        started_at: UNIX_EPOCH,
        program: program.clone(),
    });
    let burst_at = interval.mul_f64(BURST_PHASE);

    for cycle in cycles {
        let mut start = Duration::ZERO;
        for (duration, positions) in schedule(program, interval, cycle) {
            let mut count = (BASE_RATE * duration.as_secs_f64()).round() as u64;
            if (start..start + duration).contains(&burst_at) {
                count += BURST;
            }
            for position in positions {
                let group = &program.groups[position];
                for (&package, units) in &group.units {
                    accumulator
                        .add_running(package, group.index, duration)
                        .unwrap();
                    for &unit in units {
                        for counter in &group.counters {
                            accumulator
                                .add_count(package, unit, group.index, counter.slot, count)
                                .unwrap();
                        }
                    }
                }
            }
            start += duration;
        }
    }

    let counts = accumulator.counts();
    let mut rates = BTreeMap::new();
    for count in &counts.counts {
        let running = counts
            .running
            .iter()
            .find(|running| (running.package, running.group) == (count.package, count.group))
            .unwrap();
        let rate = count.value as f64 / running.seconds;
        let previous = rates.insert(count.group, rate);
        assert!(
            previous.is_none_or(|previous| previous == rate),
            "the boxes of group {} disagree",
            count.group
        );
    }

    rates
}

/// Asserts that the temporal slices `schedule` gives read a source that
/// bursts at a fixed phase of every sample without bias: each of n groups
/// reads the base rate plus one burst per interval over n consecutive cycles
/// under precession, and over whole blocks of n cycles from a multiple of n
/// under the random start, where n samples of one cycle would give one group
/// all n bursts.
pub fn assert_unbiased_over_windows(
    box_type: BoxType,
    schedule: impl Fn(&ProgrammedBoxType, Duration, u64) -> Vec<ScheduledSlice>,
) {
    const FIRST_CYCLE: u64 = 1_763_000_000;
    let close = |rate: f64, expected: f64| (rate - expected).abs() <= expected * 1e-6;

    for start in [TemporalStart::Precession, TemporalStart::Random] {
        for (interval, group_count, windows) in [
            (Duration::from_secs(1), 3, 1),
            (Duration::from_secs(1), 8, 1),
            (Duration::from_secs(1), 11, 1),
            (Duration::from_secs(1), 12, 5),
            (Duration::from_millis(250), 3, 2),
        ] {
            let program = schedule_program(box_type, Grouping::Temporal, start, group_count);
            let n = u64::from(group_count);
            let burst_rate = BURST as f64 / interval.as_secs_f64();

            let first = match start {
                TemporalStart::Precession => FIRST_CYCLE,
                TemporalStart::Random => FIRST_CYCLE / n * n,
            };
            let rates =
                phase_locked_rates(&program, interval, first..first + windows * n, &schedule);
            assert_eq!(rates.len(), usize::from(group_count));
            for (group, rate) in rates {
                assert!(
                    close(rate, BASE_RATE + burst_rate),
                    "{start:?}, {group_count} groups over {interval:?}: group {group} reads {rate}"
                );
            }

            let rates = phase_locked_rates(
                &program,
                interval,
                std::iter::repeat_n(0, usize::from(group_count)),
                &schedule,
            );
            let (biased, unbiased): (Vec<f64>, Vec<f64>) = rates
                .into_values()
                .partition(|&rate| close(rate, BASE_RATE + n as f64 * burst_rate));
            assert_eq!(
                biased.len(),
                1,
                "{start:?}, {group_count} groups over {interval:?}"
            );
            assert!(
                unbiased.iter().all(|&rate| close(rate, BASE_RATE)),
                "{start:?}, {group_count} groups over {interval:?}: {unbiased:?}"
            );
        }
    }
}

/// A collector of two packages of units 0-5 that logs what the collector
/// loop does with it. A config's counts grow by 10 on each sample.
pub struct FakeCollector {
    log: mpsc::UnboundedSender<String>,
    default: MetricUpdate,
    set_config_error: Option<String>,
    config: Option<(PmuConfig, u64)>,
}

impl FakeCollector {
    /// A collector whose default samples are `default`, and its log.
    pub fn new(default: MetricUpdate) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (log, log_rx) = mpsc::unbounded_channel();
        let collector = Self {
            log,
            default,
            set_config_error: None,
            config: None,
        };

        (collector, log_rx)
    }

    /// Fails every `set_config` with `error`.
    pub fn failing(self, error: &str) -> Self {
        Self {
            set_config_error: Some(error.to_string()),
            ..self
        }
    }

    async fn sleep(&self, sample: &str, interval: Duration) {
        let _ = self.log.send(format!("{sample} started"));
        let mut cancelled =
            CancelledSample(Some((self.log.clone(), format!("{sample} cancelled"))));
        tokio::time::sleep(interval).await;
        cancelled.0 = None;
    }
}

/// Logs a sample that was dropped before it finished.
struct CancelledSample(Option<(mpsc::UnboundedSender<String>, String)>);

impl Drop for CancelledSample {
    fn drop(&mut self) {
        if let Some((log, line)) = self.0.take() {
            let _ = log.send(line);
        }
    }
}

impl ConfigurableCollector for FakeCollector {
    async fn sample_update(&mut self, interval: Duration) -> Result<MetricUpdate, String> {
        self.sleep("sample", interval).await;
        Ok(self.default.clone())
    }

    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        (0..2).map(|package| (package, (0..6).collect())).collect()
    }

    fn has_config(&self) -> bool {
        self.config.is_some()
    }

    fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        let _ = self.log.send(match &config {
            Some(config) => format!("set_config {}", config.config_id),
            None => "set_config none".to_string(),
        });
        if let Some(error) = &self.set_config_error {
            return Err(error.clone());
        }
        self.config = config.map(|config| (config, 0));
        Ok(())
    }

    async fn sample_config(
        &mut self,
        interval: Duration,
        _cycle: u64,
    ) -> Result<UncoreCounts, String> {
        self.sleep("config sample", interval).await;
        let (config, samples) = self
            .config
            .as_mut()
            .expect("the loop samples a config only while one is set");
        *samples += 1;
        Ok(uncore_counts(config.clone(), 10 * *samples))
    }
}
