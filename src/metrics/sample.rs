use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SampleLabels {
    collector: String,
}

#[derive(Debug)]
pub struct SamplePrometheusMetrics {
    samples: Family<SampleLabels, Counter>,
    window_end_timestamp_seconds: Family<SampleLabels, Gauge<f64, AtomicU64>>,
    window_start_timestamp_seconds: Family<SampleLabels, Gauge<f64, AtomicU64>>,
}

impl SamplePrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            samples: Family::<SampleLabels, Counter>::default(),
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
    use std::time::Duration;

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
    fn never_moves_the_sequence_backwards() {
        let mut registry = Registry::default();
        let metrics = SamplePrometheusMetrics::register(&mut registry);

        metrics.update("imc", 5, window(100));
        metrics.update("imc", 4, window(99));

        assert_eq!(metrics.sequence("imc"), 5);
    }
}
