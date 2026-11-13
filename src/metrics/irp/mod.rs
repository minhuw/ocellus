pub mod hsx;
pub mod icx;
pub mod skx;
pub mod snb;
pub mod spr;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::{
    ConfigurableCollector, InfoMetadata, MetricEvent, MetricUpdate, run_configurable_collector,
};
use crate::pmu_config::types::{PmuConfig, UncoreCounts};
use crate::runtime::pmu_control::ConfigRequests;

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

impl ConfigurableCollector for IrpCollector {
    async fn sample_update(&mut self, interval: Duration) -> Result<MetricUpdate, String> {
        self.sample(interval)
            .await
            .map(|irp| MetricUpdate::Irp(Box::new(irp)))
    }

    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        match self {
            Self::Skx(collector) => collector.config_units(),
            Self::Emr(_)
            | Self::Hsx(_)
            | Self::Icx(_)
            | Self::Ivb(_)
            | Self::Snb(_)
            | Self::Spr(_) => BTreeMap::new(),
        }
    }

    fn has_config(&self) -> bool {
        match self {
            Self::Skx(collector) => collector.has_config(),
            Self::Emr(_)
            | Self::Hsx(_)
            | Self::Icx(_)
            | Self::Ivb(_)
            | Self::Snb(_)
            | Self::Spr(_) => false,
        }
    }

    fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        match self {
            Self::Skx(collector) => collector.set_config(config),
            _ if config.is_none() => Ok(()),
            collector => Err(format!(
                "PMU configs are not supported on {}",
                collector.architecture()
            )),
        }
    }

    async fn sample_config(
        &mut self,
        interval: Duration,
        cycle: u64,
    ) -> Result<UncoreCounts, String> {
        match self {
            Self::Skx(collector) => collector.sample_config(interval, cycle).await,
            collector => Err(format!(
                "PMU configs are not supported on {}",
                collector.architecture()
            )),
        }
    }
}

impl IrpCollector {
    /// The architecture's name, for errors.
    fn architecture(&self) -> &'static str {
        match self {
            Self::Emr(_) => "Emerald Rapids",
            Self::Hsx(_) => "Haswell-EP and Broadwell-EP",
            Self::Icx(_) => "Ice Lake-SP",
            Self::Ivb(_) => "Ivy Bridge-EP",
            Self::Snb(_) => "Sandy Bridge-EP",
            Self::Spr(_) => "Sapphire Rapids",
            Self::Skx(_) => "Skylake-SP",
        }
    }
}

#[derive(Debug)]
pub struct IrpTask {
    collector: IrpCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
    requests: ConfigRequests,
}

impl IrpTask {
    pub fn new(
        collector: IrpCollector,
        interval: Duration,
        events: mpsc::Sender<MetricEvent>,
        requests: ConfigRequests,
    ) -> Self {
        Self {
            collector,
            events,
            interval,
            requests,
        }
    }

    pub async fn run(self) {
        run_configurable_collector(self.collector, self.interval, self.requests, self.events).await;
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
