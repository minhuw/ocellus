pub mod hsx;
pub mod skx;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{MetricEvent, MetricUpdate};

pub use hsx::HsxImcMetrics;
pub use skx::ImcMetrics as SkxImcMetrics;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum ImcMetrics {
    Hsx(HsxImcMetrics),
    Skx(SkxImcMetrics),
}

#[derive(Debug)]
pub enum ImcCollector {
    Hsx(hsx::HsxImcCollector),
    Skx(skx::SkxImcCollector),
}

impl ImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxImcCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxImcCollector::new(architecture).map(Self::Skx)
            }
            model => Err(format!("IMC collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            architecture.intel_server_model(),
            IntelServerCpuModel::HaswellXeon
                | IntelServerCpuModel::BroadwellXeon
                | IntelServerCpuModel::SkylakeXeon
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<ImcMetrics, String> {
        match self {
            Self::Hsx(collector) => collector.sample(interval).await.map(ImcMetrics::Hsx),
            Self::Skx(collector) => collector.sample(interval).await.map(ImcMetrics::Skx),
        }
    }
}

#[derive(Debug)]
pub struct ImcTask {
    collector: ImcCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl ImcTask {
    pub fn new(
        collector: ImcCollector,
        interval: Duration,
        events: mpsc::Sender<MetricEvent>,
    ) -> Self {
        Self {
            collector,
            events,
            interval,
        }
    }

    pub async fn run(mut self) {
        loop {
            match self.collector.sample(self.interval).await {
                Ok(imc) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Imc(Box::new(
                            imc,
                        )))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = self.events.send(MetricEvent::Failure(error)).await;
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ImcPrometheusMetrics {
    hsx: hsx::HsxImcPrometheusMetrics,
    skx: skx::ImcPrometheusMetrics,
}

impl ImcPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        Self {
            hsx: hsx::HsxImcPrometheusMetrics::register(registry),
            skx: skx::ImcPrometheusMetrics::register(registry),
        }
    }

    pub fn update(&self, metrics: ImcMetrics) {
        match metrics {
            ImcMetrics::Hsx(metrics) => self.hsx.update(metrics),
            ImcMetrics::Skx(metrics) => self.skx.update(metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_all_imc_architectures() {
        assert!(ImcCollector::is_supported(&test_architecture(0x3f)));
        assert!(ImcCollector::is_supported(&test_architecture(0x4f)));
        assert!(ImcCollector::is_supported(&test_architecture(0x55)));
        assert!(!ImcCollector::is_supported(&test_architecture(0xcf)));
    }

    fn test_architecture(model: u8) -> Architecture {
        Architecture {
            brand: "test".to_string(),
            family: 6,
            features: crate::arch::ArchitectureFeatures::default(),
            model,
            vendor: "GenuineIntel".to_string(),
        }
    }
}
