pub mod icx;
pub mod skx;
pub mod spr;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum IioMetrics {
    Icx(icx::IcxIioMetrics),
    Skx(skx::SkxIioMetrics),
    Spr(spr::SprIioMetrics),
}

#[derive(Debug)]
pub enum IioCollector {
    Icx(icx::IcxIioCollector),
    Skx(skx::SkxIioCollector),
    Spr(spr::SprIioCollector),
}

impl IioCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxIioCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxIioCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::SapphireRapids => {
                spr::SprIioCollector::new(architecture).map(Self::Spr)
            }
            model => Err(format!("IIO collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(
                IntelServerCpuModel::SkylakeXeon
                    | IntelServerCpuModel::IceLakeXeon
                    | IntelServerCpuModel::SapphireRapids
            )
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IioMetrics, String> {
        match self {
            Self::Icx(collector) => collector.sample(interval).await.map(IioMetrics::Icx),
            Self::Skx(collector) => collector.sample(interval).await.map(IioMetrics::Skx),
            Self::Spr(collector) => collector.sample(interval).await.map(IioMetrics::Spr),
        }
    }
}

#[derive(Debug)]
pub struct IioTask {
    collector: IioCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl IioTask {
    pub fn new(
        collector: IioCollector,
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
                Ok(iio) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Iio(Box::new(
                            iio,
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
pub enum IioPrometheusMetrics {
    Icx(icx::IcxIioPrometheusMetrics),
    Skx(skx::SkxIioPrometheusMetrics),
    Spr(spr::SprIioPrometheusMetrics),
}

impl IioPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::IceLakeXeon) => {
                Some(Self::Icx(icx::IcxIioPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::SkxIioPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SapphireRapids) => {
                Some(Self::Spr(spr::SprIioPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: IioMetrics) {
        match (self, metrics) {
            (Self::Icx(prometheus), IioMetrics::Icx(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), IioMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Spr(prometheus), IioMetrics::Spr(metrics)) => prometheus.update(metrics),
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched IIO Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_known_iio_architectures() {
        assert!(!IioCollector::is_supported(&test_architecture(0x4f)));
        assert!(IioCollector::is_supported(&test_architecture(0x55)));
        assert!(IioCollector::is_supported(&test_architecture(0x6a)));
        assert!(!IioCollector::is_supported(&test_architecture(0x6c)));
        assert!(IioCollector::is_supported(&test_architecture(0x8f)));
        assert!(!IioCollector::is_supported(&test_architecture(0xcf)));
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
