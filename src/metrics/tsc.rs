use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::arch::Architecture;
use crate::metal;
use crate::metrics::{MetricEvent, MetricUpdate};

const IA32_TSC: u64 = 0x10;

#[derive(Debug)]
pub struct TscCollector {
    is_supported: bool,
    previous: Option<TscReading>,
}

#[derive(Debug)]
pub struct TscTask {
    collector: TscCollector,
    interval: Duration,
    events: mpsc::Sender<MetricEvent>,
}

#[derive(Debug)]
pub struct TscPrometheusMetrics {
    frequency_hz: Family<TscMetricLabels, Gauge<f64, AtomicU64>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct TscMetricLabels {}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct TscMetrics {
    pub frequency_hz: f64,
}

#[derive(Clone, Copy, Debug)]
struct TscReading {
    cycles: u64,
    at: Instant,
}

impl TscTask {
    pub fn new(
        collector: TscCollector,
        interval: Duration,
        events: mpsc::Sender<MetricEvent>,
    ) -> Self {
        Self {
            collector,
            interval,
            events,
        }
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            match self.collector.sample() {
                Ok(Some(tsc)) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Tsc(tsc))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = self.events.send(MetricEvent::Failure(error)).await;
                    return;
                }
            }
        }
    }
}

impl TscCollector {
    pub fn new() -> Self {
        Self::with_capabilities(tsc_supported())
    }

    pub fn sample(&mut self) -> Result<Option<TscMetrics>, String> {
        if !self.is_supported {
            return Ok(None);
        }

        let current = TscReading::now()?;
        let previous = match self.previous.replace(current) {
            Some(previous) => previous,
            None => return Ok(None),
        };

        Ok(TscMetrics::from_readings(previous, current))
    }

    pub(crate) fn with_capabilities(is_supported: bool) -> Self {
        Self {
            is_supported,
            previous: None,
        }
    }
}

impl TscPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let frequency_hz = Family::<TscMetricLabels, Gauge<f64, AtomicU64>>::default();

        registry.register(
            "ocellus_tsc_frequency_hz",
            "Estimated timestamp counter frequency from periodic samples",
            frequency_hz.clone(),
        );

        Self { frequency_hz }
    }

    pub fn update(&self, metrics: TscMetrics) {
        self.frequency_hz
            .get_or_create(&TscMetricLabels {})
            .set(metrics.frequency_hz);
    }
}

impl TscMetrics {
    fn from_readings(previous: TscReading, current: TscReading) -> Option<Self> {
        let elapsed = current
            .at
            .checked_duration_since(previous.at)?
            .as_secs_f64();

        if elapsed == 0.0 {
            return None;
        }

        Some(Self {
            frequency_hz: current.cycles.wrapping_sub(previous.cycles) as f64 / elapsed,
        })
    }
}

impl TscReading {
    fn now() -> Result<Self, String> {
        Ok(Self {
            cycles: read_tsc()?,
            at: Instant::now(),
        })
    }
}

pub fn preflight_permissions(architecture: &Architecture) -> Result<(), String> {
    if !architecture.features.tsc {
        return Ok(());
    }
    let msr = metal::msr::Msr::open_readonly(0)?;
    msr.read(IA32_TSC)?;

    Ok(())
}

fn read_tsc() -> Result<u64, String> {
    metal::msr::Msr::open_readonly(0)?.read(IA32_TSC)
}

fn tsc_supported() -> bool {
    metal::cpuid::has_tsc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_collector_has_no_tsc_sample() {
        let mut collector = TscCollector::with_capabilities(false);
        let tsc = collector.sample().unwrap();

        assert!(tsc.is_none());
    }

    #[test]
    fn computes_frequency_from_two_readings() {
        let previous = TscReading {
            cycles: 100,
            at: Instant::now(),
        };
        let current = TscReading {
            cycles: 300,
            at: previous.at + Duration::from_millis(100),
        };

        let metrics = TscMetrics::from_readings(previous, current).unwrap();

        assert_eq!(metrics.frequency_hz, 2000.0);
    }

    #[test]
    fn skips_frequency_when_elapsed_time_is_zero() {
        let reading = TscReading {
            cycles: 100,
            at: Instant::now(),
        };

        assert!(TscMetrics::from_readings(reading, reading).is_none());
    }
}
