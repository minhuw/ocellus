pub mod hsx;
pub mod skx;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

pub use hsx::HsxIrpMetrics;
pub use skx::SkxIrpMetrics;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum IrpMetrics {
    Hsx(HsxIrpMetrics),
    Skx(SkxIrpMetrics),
}

#[derive(Debug)]
pub enum IrpCollector {
    Hsx(hsx::HsxIrpCollector),
    Skx(skx::SkxIrpCollector),
}

impl IrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxIrpCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxIrpCollector::new(architecture).map(Self::Skx)
            }
            model => Err(format!("IRP collection is not supported for {model:?}")),
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

    pub async fn sample(&mut self, interval: Duration) -> Result<IrpMetrics, String> {
        match self {
            Self::Hsx(collector) => collector.sample(interval).await.map(IrpMetrics::Hsx),
            Self::Skx(collector) => collector.sample(interval).await.map(IrpMetrics::Skx),
        }
    }
}

#[derive(Debug)]
pub struct IrpTask {
    collector: IrpCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl IrpTask {
    pub fn new(
        collector: IrpCollector,
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
                Ok(irp) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Irp(Box::new(
                            irp,
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
pub enum IrpPrometheusMetrics {
    Hsx(hsx::HsxIrpPrometheusMetrics),
    Skx(skx::SkxIrpPrometheusMetrics),
}

impl IrpPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Self {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Self::Hsx(hsx::HsxIrpPrometheusMetrics::register(registry))
            }
            _ => Self::Skx(skx::SkxIrpPrometheusMetrics::register(registry)),
        }
    }

    pub fn update(&self, metrics: IrpMetrics) {
        match (self, metrics) {
            (Self::Hsx(prometheus), IrpMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), IrpMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Hsx(_), IrpMetrics::Skx(_)) | (Self::Skx(_), IrpMetrics::Hsx(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_known_irp_architectures() {
        assert!(IrpCollector::is_supported(&test_architecture(0x3f)));
        assert!(IrpCollector::is_supported(&test_architecture(0x4f)));
        assert!(IrpCollector::is_supported(&test_architecture(0x55)));
        assert!(!IrpCollector::is_supported(&test_architecture(0xcf)));
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
