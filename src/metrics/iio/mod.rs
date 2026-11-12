pub mod icx;
pub mod skx;
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

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum IioMetrics {
    Emr(spr::SprIioMetrics),
    Icx(icx::IcxIioMetrics),
    Skx(skx::SkxIioMetrics),
    Spr(spr::SprIioMetrics),
}

#[derive(Debug)]
pub enum IioCollector {
    Emr(spr::SprIioCollector),
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
            IntelServerCpuModel::SapphireRapids => spr::SprIioCollector::new().map(Self::Spr),
            IntelServerCpuModel::EmeraldRapids => spr::SprIioCollector::new().map(Self::Emr),
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
                    | IntelServerCpuModel::EmeraldRapids
            )
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IioMetrics, String> {
        match self {
            Self::Emr(collector) => collector.sample(interval).await.map(IioMetrics::Emr),
            Self::Icx(collector) => collector.sample(interval).await.map(IioMetrics::Icx),
            Self::Skx(collector) => collector.sample(interval).await.map(IioMetrics::Skx),
            Self::Spr(collector) => collector.sample(interval).await.map(IioMetrics::Spr),
        }
    }
}

impl ConfigurableCollector for IioCollector {
    async fn sample_update(&mut self, interval: Duration) -> Result<MetricUpdate, String> {
        self.sample(interval)
            .await
            .map(|iio| MetricUpdate::Iio(Box::new(iio)))
    }

    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        match self {
            Self::Skx(collector) => collector.config_units(),
            Self::Emr(_) | Self::Icx(_) | Self::Spr(_) => BTreeMap::new(),
        }
    }

    fn has_config(&self) -> bool {
        match self {
            Self::Skx(collector) => collector.has_config(),
            Self::Emr(_) | Self::Icx(_) | Self::Spr(_) => false,
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

    async fn sample_config(&mut self, interval: Duration) -> Result<UncoreCounts, String> {
        match self {
            Self::Skx(collector) => collector.sample_config(interval).await,
            collector => Err(format!(
                "PMU configs are not supported on {}",
                collector.architecture()
            )),
        }
    }
}

impl IioCollector {
    /// The architecture's name, for errors.
    fn architecture(&self) -> &'static str {
        match self {
            Self::Emr(_) => "Emerald Rapids",
            Self::Icx(_) => "Ice Lake-SP",
            Self::Spr(_) => "Sapphire Rapids",
            Self::Skx(_) => "Skylake-SP",
        }
    }
}

#[derive(Debug)]
pub struct IioTask {
    collector: IioCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
    requests: ConfigRequests,
}

impl IioTask {
    pub fn new(
        collector: IioCollector,
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
pub enum IioPrometheusMetrics {
    Emr(spr::SprIioPrometheusMetrics),
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
            Some(IntelServerCpuModel::EmeraldRapids) => {
                Some(Self::Emr(spr::SprIioPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: IioMetrics) {
        match (self, metrics) {
            (Self::Emr(prometheus), IioMetrics::Emr(metrics)) => prometheus.update(metrics),
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
        assert!(IioCollector::is_supported(&test_architecture(0xcf)));
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
