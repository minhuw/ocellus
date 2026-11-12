use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// Wall-clock bounds of one collector sample.
///
/// Collectors that count over the sample interval (CHA, IMC, IIO, IRP, PCU,
/// interconnect) report the interval they counted over. Collectors that read
/// instantaneous values or deltas on a timer (RAPL, RDT, TSC) report the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleWindow {
    pub start: SystemTime,
    pub end: SystemTime,
}

impl SampleWindow {
    pub fn since(start: SystemTime) -> Self {
        Self {
            start,
            end: SystemTime::now(),
        }
    }
}

/// How long from `now` to the next whole multiple of `interval` since the
/// Unix epoch that is at least `interval / 2` away. Collectors sample or read
/// until then, so their samples start and end together on whole multiples of
/// the interval.
pub fn until_next_boundary(now: SystemTime, interval: Duration) -> Duration {
    let now = now
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_nanos();
    let interval = interval.as_nanos();
    let boundary = (now + interval / 2).div_ceil(interval) * interval;

    Duration::from_nanos(u64::try_from(boundary - now).expect("intervals fit in u64 nanoseconds"))
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SampleLabels {
    collector: String,
}

#[derive(Debug)]
pub struct SamplePrometheusMetrics {
    samples: Family<SampleLabels, Counter>,
    stopped: Family<SampleLabels, Gauge>,
    window_end_timestamp_seconds: Family<SampleLabels, Gauge<f64, AtomicU64>>,
    window_start_timestamp_seconds: Family<SampleLabels, Gauge<f64, AtomicU64>>,
}

impl SamplePrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            samples: Family::<SampleLabels, Counter>::default(),
            stopped: Family::<SampleLabels, Gauge>::default(),
            window_end_timestamp_seconds: Family::<SampleLabels, Gauge<f64, AtomicU64>>::default(),
            window_start_timestamp_seconds: Family::<SampleLabels, Gauge<f64, AtomicU64>>::default(
            ),
        };

        registry.register(
            "ocellus_samples",
            "Samples each collector has taken; a step of more than one between scrapes means a sample was not exported",
            metrics.samples.clone(),
        );
        registry.register(
            "ocellus_collector_stopped",
            "1 if the collector stopped after a runtime error and its metrics are no longer exported",
            metrics.stopped.clone(),
        );
        registry.register(
            "ocellus_sample_window_end_timestamp_seconds",
            "Unix time the collector's exported sample ended",
            metrics.window_end_timestamp_seconds.clone(),
        );
        registry.register(
            "ocellus_sample_window_start_timestamp_seconds",
            "Unix time the collector's exported sample started",
            metrics.window_start_timestamp_seconds.clone(),
        );

        metrics
    }

    pub fn update(&self, collector: &str, sequence: u64, window: SampleWindow) {
        let labels = SampleLabels {
            collector: collector.to_string(),
        };
        let samples = self.samples.get_or_create(&labels);
        let exported = samples.get();
        if sequence > exported {
            samples.inc_by(sequence - exported);
        }
        self.window_start_timestamp_seconds
            .get_or_create(&labels)
            .set(unix_seconds(window.start));
        self.window_end_timestamp_seconds
            .get_or_create(&labels)
            .set(unix_seconds(window.end));
    }

    pub fn stop(&self, collector: &str) {
        let labels = SampleLabels {
            collector: collector.to_string(),
        };
        self.stopped.get_or_create(&labels).set(1);
        self.window_start_timestamp_seconds.remove(&labels);
        self.window_end_timestamp_seconds.remove(&labels);
    }

    #[cfg(test)]
    pub fn sequence(&self, collector: &str) -> u64 {
        self.samples
            .get_or_create(&SampleLabels {
                collector: collector.to_string(),
            })
            .get()
    }
}

fn unix_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start_seconds: u64) -> SampleWindow {
        SampleWindow {
            start: UNIX_EPOCH + Duration::from_secs(start_seconds),
            end: UNIX_EPOCH + Duration::from_millis(start_seconds * 1_000 + 1_002),
        }
    }

    #[test]
    fn exports_sequence_and_window() {
        let mut registry = Registry::default();
        let metrics = SamplePrometheusMetrics::register(&mut registry);

        metrics.update("cha", 1, window(100));
        // A skipped sample shows as a step of two.
        metrics.update("cha", 3, window(102));
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();

        assert!(
            text.contains("ocellus_samples_total{collector=\"cha\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("ocellus_sample_window_start_timestamp_seconds{collector=\"cha\"} 102.0")
        );
        assert!(
            text.contains("ocellus_sample_window_end_timestamp_seconds{collector=\"cha\"} 103.002")
        );
    }

    #[test]
    fn ends_samples_on_the_next_boundary_at_least_half_an_interval_away() {
        let at = |milliseconds: u64| UNIX_EPOCH + Duration::from_millis(milliseconds);
        let second = Duration::from_secs(1);
        let cases = [
            (at(10_300), second, Duration::from_millis(700)),
            (at(10_500), second, Duration::from_millis(500)),
            (at(10_501), second, Duration::from_millis(1_499)),
            (at(11_000), second, second),
            // A sample that ended a little after a boundary.
            (at(11_003), second, Duration::from_millis(997)),
            (
                at(1_010),
                Duration::from_millis(250),
                Duration::from_millis(240),
            ),
            (
                at(1_130),
                Duration::from_millis(250),
                Duration::from_millis(370),
            ),
        ];

        for (now, interval, expected) in cases {
            assert_eq!(until_next_boundary(now, interval), expected, "{now:?}");
        }
        let now = at(1_700_000_000_123) + Duration::from_nanos(456);
        let until = until_next_boundary(now, second);
        assert_eq!(
            (now + until)
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos(),
            0
        );
    }

    #[test]
    fn never_moves_the_sequence_backwards() {
        let mut registry = Registry::default();
        let metrics = SamplePrometheusMetrics::register(&mut registry);

        metrics.update("imc", 5, window(100));
        metrics.update("imc", 4, window(99));

        assert_eq!(metrics.sequence("imc"), 5);
    }
}
