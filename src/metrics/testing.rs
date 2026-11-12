//! Test fixtures for PMU configs.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::metrics::{ConfigurableCollector, MetricUpdate};
use crate::pmu_config::types::{
    Aggregate, BoxType, ChaFilters, CountUnit, CounterSlot, GroupRunning, Grouping, PmuConfig,
    ProgrammedBoxType, ProgrammedCounter, ProgrammedGroup, UncoreCounts, UnitCount,
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

    async fn sample_config(&mut self, interval: Duration) -> Result<UncoreCounts, String> {
        self.sleep("config sample", interval).await;
        let (config, samples) = self
            .config
            .as_mut()
            .expect("the loop samples a config only while one is set");
        *samples += 1;
        Ok(uncore_counts(config.clone(), 10 * *samples))
    }
}
