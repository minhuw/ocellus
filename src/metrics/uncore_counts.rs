//! Raw counts of PMU configs, absent until a config is first set.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::pmu_config::types::{BoxType, PmuConfig, UncoreCounts};

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct CountLabels {
    box_type: &'static str,
    package: String,
    unit: String,
    group: String,
    slot: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct RunningLabels {
    box_type: &'static str,
    package: String,
    group: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct BoxTypeLabels {
    box_type: &'static str,
}

/// `config_id`, `box_type`, `package`, `group`, `slot` and `units`, then the
/// counter's event labels, which differ between box types.
type EventInfoLabels = Vec<(String, String)>;

#[derive(Debug)]
pub struct UncoreCountPrometheusMetrics {
    config_id: Family<BoxTypeLabels, Gauge>,
    config_start_timestamp_seconds: Family<BoxTypeLabels, Gauge<f64, AtomicU64>>,
    count: Family<CountLabels, Counter>,
    event_info: Family<EventInfoLabels, Gauge>,
    running_seconds: Family<RunningLabels, Counter<f64, AtomicU64>>,
    schedule_first_group: Family<BoxTypeLabels, Gauge>,
    /// The series of each box type's config, so that a config change
    /// removes exactly them.
    exported: Mutex<BTreeMap<BoxType, ExportedConfig>>,
}

#[derive(Debug)]
struct ExportedConfig {
    config_id: u64,
    started_at: SystemTime,
    counts: HashSet<CountLabels>,
    running: HashSet<RunningLabels>,
    event_info: Vec<EventInfoLabels>,
}

impl UncoreCountPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            config_id: Family::default(),
            config_start_timestamp_seconds: Family::default(),
            count: Family::default(),
            event_info: Family::default(),
            running_seconds: Family::default(),
            schedule_first_group: Family::default(),
            exported: Mutex::default(),
        };

        registry.register(
            "ocellus_uncore_count",
            "Raw count of a PMU config counter since the config started; ocellus_uncore_event_info says what it counts",
            metrics.count.clone(),
        );
        registry.register(
            "ocellus_uncore_running_seconds",
            "Time a PMU config group has counted since the config started",
            metrics.running_seconds.clone(),
        );
        registry.register(
            "ocellus_uncore_config_id",
            "Hash of the box type's PMU config programming; deltas are valid only between samples with equal config_id and config start",
            metrics.config_id.clone(),
        );
        registry.register(
            "ocellus_uncore_config_start_timestamp_seconds",
            "Unix time the box type's PMU config was set and its counts started from 0",
            metrics.config_start_timestamp_seconds.clone(),
        );
        registry.register(
            "ocellus_uncore_schedule_first_group",
            "Temporal PMU config group that counted first in the box type's latest sample; slice p of each round ran group (first + p) mod the group count",
            metrics.schedule_first_group.clone(),
        );
        registry.register(
            "ocellus_uncore_event_info",
            "What each counter of a PMU config counts, per package and group",
            metrics.event_info.clone(),
        );

        metrics
    }

    /// Sets a box type's counts. A sample of another config than the one
    /// exported first removes the exported series, so no counter goes
    /// backwards.
    pub fn update(&self, counts: UncoreCounts) {
        let config = &counts.config;
        let box_type = config.program.box_type;
        let mut exported = self.exported.lock().unwrap();
        if exported.get(&box_type).is_some_and(|series| {
            (series.config_id, series.started_at) != (config.config_id, config.started_at)
        }) {
            self.remove_series(box_type, exported.remove(&box_type).unwrap());
        }
        let series = exported
            .entry(box_type)
            .or_insert_with(|| self.export_config(config));
        if let Some(group) = counts.first_group {
            self.schedule_first_group
                .get_or_create(&BoxTypeLabels {
                    box_type: box_type.label(),
                })
                .set(i64::from(group));
        }

        for count in &counts.counts {
            let labels = CountLabels {
                box_type: box_type.label(),
                package: count.package.to_string(),
                unit: count.unit.label(),
                group: count.group.to_string(),
                slot: count.slot.label(),
            };
            let counter = self.count.get_or_create(&labels);
            let value = counter.get();
            if count.value > value {
                counter.inc_by(count.value - value);
            }
            series.counts.insert(labels);
        }
        for running in &counts.running {
            let labels = RunningLabels {
                box_type: box_type.label(),
                package: running.package.to_string(),
                group: running.group.to_string(),
            };
            let counter = self.running_seconds.get_or_create(&labels);
            let delta = running.seconds - counter.get();
            if delta > 0.0 {
                counter.inc_by(delta);
            }
            series.running.insert(labels);
        }
    }

    /// Removes every series of a box type's config.
    pub fn remove(&self, box_type: BoxType) {
        if let Some(series) = self.exported.lock().unwrap().remove(&box_type) {
            self.remove_series(box_type, series);
        }
    }

    fn export_config(&self, config: &PmuConfig) -> ExportedConfig {
        let labels = BoxTypeLabels {
            box_type: config.program.box_type.label(),
        };
        self.config_id
            .get_or_create(&labels)
            .set(i64::try_from(config.config_id).expect("config ids are below 2^53"));
        self.config_start_timestamp_seconds
            .get_or_create(&labels)
            .set(
                config
                    .started_at
                    .duration_since(UNIX_EPOCH)
                    .expect("PMU configs start after the Unix epoch")
                    .as_secs_f64(),
            );
        let event_info = event_info_labels(config);
        for labels in &event_info {
            self.event_info.get_or_create(labels).set(1);
        }

        ExportedConfig {
            config_id: config.config_id,
            started_at: config.started_at,
            counts: HashSet::new(),
            running: HashSet::new(),
            event_info,
        }
    }

    fn remove_series(&self, box_type: BoxType, series: ExportedConfig) {
        let labels = BoxTypeLabels {
            box_type: box_type.label(),
        };
        self.config_id.remove(&labels);
        self.config_start_timestamp_seconds.remove(&labels);
        self.schedule_first_group.remove(&labels);
        for labels in &series.counts {
            self.count.remove(labels);
        }
        for labels in &series.running {
            self.running_seconds.remove(labels);
        }
        for labels in &series.event_info {
            self.event_info.remove(labels);
        }
    }
}

/// One label set per package, group and slot of the config.
fn event_info_labels(config: &PmuConfig) -> Vec<EventInfoLabels> {
    let config_id = config.config_id.to_string();
    let box_type = config.program.box_type.label();
    let mut labels = Vec::new();
    for group in &config.program.groups {
        for (package, units) in &group.units {
            for counter in &group.counters {
                let mut counter_labels = vec![
                    ("config_id".to_string(), config_id.clone()),
                    ("box_type".to_string(), box_type.to_string()),
                    ("package".to_string(), package.to_string()),
                    ("group".to_string(), group.index.to_string()),
                    ("slot".to_string(), counter.slot.label()),
                    ("units".to_string(), unit_ranges(units)),
                ];
                counter_labels.extend(counter.labels.clone());
                labels.push(counter_labels);
            }
        }
    }

    labels
}

/// Unit ids as ranges, for example "0-12,14".
fn unit_ranges(units: &BTreeSet<u16>) -> String {
    let mut ranges = Vec::<(u16, u16)>::new();
    for &unit in units {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == unit => *end = unit,
            _ => ranges.push((unit, unit)),
        }
    }

    ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_units_as_ranges() {
        let ranges = |units: &[u16]| unit_ranges(&units.iter().copied().collect());

        assert_eq!(ranges(&[3]), "3");
        assert_eq!(
            ranges(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14]),
            "0-12,14"
        );
        assert_eq!(ranges(&[0, 2, 3, 5]), "0,2-3,5");
    }
}
