pub mod hsx;
pub mod icx;
pub mod skx;
pub mod snb;
pub mod spr;

use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

pub use hsx::HsxIrpMetrics;
pub use icx::IcxIrpMetrics;
pub use skx::SkxIrpMetrics;
pub use snb::SnbIrpMetrics;
pub use spr::SprIrpMetrics;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum IrpMetrics {
    Emr(SprIrpMetrics),
    Hsx(HsxIrpMetrics),
    Icx(IcxIrpMetrics),
    Ivb(SnbIrpMetrics),
    Skx(SkxIrpMetrics),
    Snb(SnbIrpMetrics),
    Spr(SprIrpMetrics),
}

#[derive(Debug)]
pub enum IrpCollector {
    Emr(spr::SprIrpCollector),
    Hsx(hsx::HsxIrpCollector),
    Icx(icx::IcxIrpCollector),
    Ivb(snb::SnbIrpCollector),
    Skx(skx::SkxIrpCollector),
    Snb(snb::SnbIrpCollector),
    Spr(spr::SprIrpCollector),
}

impl IrpCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SandyBridgeEp => {
                snb::SnbIrpCollector::new(architecture).map(Self::Snb)
            }
            IntelServerCpuModel::IvyTown => snb::SnbIrpCollector::new(architecture).map(Self::Ivb),
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxIrpCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxIrpCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxIrpCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::SapphireRapids => spr::SprIrpCollector::new().map(Self::Spr),
            IntelServerCpuModel::EmeraldRapids => spr::SprIrpCollector::new().map(Self::Emr),
            model => Err(format!("IRP collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(
                IntelServerCpuModel::SandyBridgeEp
                    | IntelServerCpuModel::IvyTown
                    | IntelServerCpuModel::HaswellXeon
                    | IntelServerCpuModel::BroadwellXeon
                    | IntelServerCpuModel::SkylakeXeon
                    | IntelServerCpuModel::IceLakeXeon
                    | IntelServerCpuModel::SapphireRapids
                    | IntelServerCpuModel::EmeraldRapids
            )
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IrpMetrics, String> {
        match self {
            Self::Emr(collector) => collector.sample(interval).await.map(IrpMetrics::Emr),
            Self::Hsx(collector) => collector.sample(interval).await.map(IrpMetrics::Hsx),
            Self::Icx(collector) => collector.sample(interval).await.map(IrpMetrics::Icx),
            Self::Ivb(collector) => collector.sample(interval).await.map(IrpMetrics::Ivb),
            Self::Skx(collector) => collector.sample(interval).await.map(IrpMetrics::Skx),
            Self::Snb(collector) => collector.sample(interval).await.map(IrpMetrics::Snb),
            Self::Spr(collector) => collector.sample(interval).await.map(IrpMetrics::Spr),
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
    Emr(spr::SprIrpPrometheusMetrics),
    Hsx(hsx::HsxIrpPrometheusMetrics),
    Icx(icx::IcxIrpPrometheusMetrics),
    Ivb(snb::SnbIrpPrometheusMetrics),
    Skx(skx::SkxIrpPrometheusMetrics),
    Snb(snb::SnbIrpPrometheusMetrics),
    Spr(spr::SprIrpPrometheusMetrics),
}

impl IrpPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SandyBridgeEp) => {
                Some(Self::Snb(snb::SnbIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IvyTown) => {
                Some(Self::Ivb(snb::SnbIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Hsx(hsx::HsxIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IceLakeXeon) => {
                Some(Self::Icx(icx::IcxIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::SkxIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SapphireRapids) => {
                Some(Self::Spr(spr::SprIrpPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::EmeraldRapids) => {
                Some(Self::Emr(spr::SprIrpPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: IrpMetrics) {
        match (self, metrics) {
            (Self::Emr(prometheus), IrpMetrics::Emr(metrics)) => prometheus.update(metrics),
            (Self::Hsx(prometheus), IrpMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Icx(prometheus), IrpMetrics::Icx(metrics)) => prometheus.update(metrics),
            (Self::Ivb(prometheus), IrpMetrics::Ivb(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), IrpMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Snb(prometheus), IrpMetrics::Snb(metrics)) => prometheus.update(metrics),
            (Self::Spr(prometheus), IrpMetrics::Spr(metrics)) => prometheus.update(metrics),
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched IRP Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_known_irp_architectures() {
        assert!(IrpCollector::is_supported(&test_architecture(0x2d)));
        assert!(IrpCollector::is_supported(&test_architecture(0x3e)));
        assert!(IrpCollector::is_supported(&test_architecture(0x3f)));
        assert!(IrpCollector::is_supported(&test_architecture(0x4f)));
        assert!(IrpCollector::is_supported(&test_architecture(0x55)));
        assert!(IrpCollector::is_supported(&test_architecture(0x6a)));
        assert!(!IrpCollector::is_supported(&test_architecture(0x6c)));
        assert!(IrpCollector::is_supported(&test_architecture(0x8f)));
        assert!(IrpCollector::is_supported(&test_architecture(0xcf)));
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
