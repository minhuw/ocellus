pub mod hsx;
pub mod skx;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

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
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(
                IntelServerCpuModel::HaswellXeon
                    | IntelServerCpuModel::BroadwellXeon
                    | IntelServerCpuModel::SkylakeXeon
            )
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
pub enum ImcPrometheusMetrics {
    Hsx(hsx::HsxImcPrometheusMetrics),
    Skx(skx::ImcPrometheusMetrics),
}

impl ImcPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Hsx(hsx::HsxImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::ImcPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: ImcMetrics) {
        match (self, metrics) {
            (Self::Hsx(prometheus), ImcMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), ImcMetrics::Skx(metrics)) => prometheus.update(metrics),
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched IMC Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
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
