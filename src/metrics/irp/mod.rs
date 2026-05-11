pub mod skx;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{MetricEvent, MetricUpdate};

pub use skx::SkxIrpMetrics;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum IrpMetrics {
    Skx(SkxIrpMetrics),
}

#[derive(Debug)]
pub enum IrpCollector {
    Skx(skx::SkxIrpCollector),
}

impl IrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxIrpCollector::new(architecture).map(Self::Skx)
            }
            model => Err(format!("IRP collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            architecture.intel_server_model(),
            IntelServerCpuModel::SkylakeXeon
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IrpMetrics, String> {
        match self {
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
pub struct IrpPrometheusMetrics {
    skx: skx::SkxIrpPrometheusMetrics,
}

impl IrpPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        Self {
            skx: skx::SkxIrpPrometheusMetrics::register(registry),
        }
    }

    pub fn update(&self, metrics: IrpMetrics) {
        match metrics {
            IrpMetrics::Skx(metrics) => self.skx.update(metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_only_skylake_irp() {
        assert!(!IrpCollector::is_supported(&test_architecture(0x3f)));
        assert!(!IrpCollector::is_supported(&test_architecture(0x4f)));
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
