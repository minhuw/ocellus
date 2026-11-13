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
    ConfigurableCollector, EventProfile, InfoMetadata, MetricEvent, MetricUpdate,
    run_configurable_collector,
};
use crate::pmu_config::types::{PmuConfig, UncoreCounts};
use crate::runtime::pmu_control::ConfigRequests;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum ImcMetrics {
    Emr(spr::SprImcMetrics),
    Hsx(hsx::HsxImcMetrics),
    Icx(icx::IcxImcMetrics),
    Ivb(snb::SnbImcMetrics),
    Skx(skx::ImcMetrics),
    Snb(snb::SnbImcMetrics),
    Spr(spr::SprImcMetrics),
}

#[derive(Debug)]
pub enum ImcCollector {
    Emr(spr::SprImcCollector),
    Hsx(hsx::HsxImcCollector),
    Icx(icx::IcxImcCollector),
    Ivb(snb::SnbImcCollector),
    Skx(skx::SkxImcCollector),
    Snb(snb::SnbImcCollector),
    Spr(spr::SprImcCollector),
}

impl ImcCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SandyBridgeEp => {
                snb::SnbImcCollector::new(architecture).map(Self::Snb)
            }
            IntelServerCpuModel::IvyTown => snb::SnbImcCollector::new(architecture).map(Self::Ivb),
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxImcCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxImcCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxImcCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::SapphireRapids => spr::SprImcCollector::new().map(Self::Spr),
            IntelServerCpuModel::EmeraldRapids => spr::SprImcCollector::new().map(Self::Emr),
            model => Err(format!("IMC collection is not supported for {model:?}")),
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

    /// Only Skylake-SP has more than the default profile;
    /// `EventProfile::validate` rejects the rest at startup.
    pub fn set_event_profile(&mut self, profile: EventProfile) -> Result<(), String> {
        match self {
            Self::Skx(collector) => {
                collector.set_event_profile(profile);
                Ok(())
            }
            _ if profile == EventProfile::Default => Ok(()),
            _ => Err(format!(
                "IMC event profile {} is only supported on Skylake-SP",
                profile.label()
            )),
        }
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<ImcMetrics, String> {
        match self {
            Self::Emr(collector) => collector.sample(interval).await.map(ImcMetrics::Emr),
            Self::Hsx(collector) => collector.sample(interval).await.map(ImcMetrics::Hsx),
            Self::Icx(collector) => collector.sample(interval).await.map(ImcMetrics::Icx),
            Self::Ivb(collector) => collector.sample(interval).await.map(ImcMetrics::Ivb),
            Self::Skx(collector) => collector.sample(interval).await.map(ImcMetrics::Skx),
            Self::Snb(collector) => collector.sample(interval).await.map(ImcMetrics::Snb),
            Self::Spr(collector) => collector.sample(interval).await.map(ImcMetrics::Spr),
        }
    }
}

impl ConfigurableCollector for ImcCollector {
    async fn sample_update(&mut self, interval: Duration) -> Result<MetricUpdate, String> {
        self.sample(interval)
            .await
            .map(|imc| MetricUpdate::Imc(Box::new(imc)))
    }

    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        match self {
            Self::Hsx(collector) => collector.config_units(),
            Self::Skx(collector) => collector.config_units(),
            Self::Emr(_) | Self::Icx(_) | Self::Ivb(_) | Self::Snb(_) | Self::Spr(_) => {
                BTreeMap::new()
            }
        }
    }

    fn has_config(&self) -> bool {
        match self {
            Self::Hsx(collector) => collector.has_config(),
            Self::Skx(collector) => collector.has_config(),
            Self::Emr(_) | Self::Icx(_) | Self::Ivb(_) | Self::Snb(_) | Self::Spr(_) => false,
        }
    }

    fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        match self {
            Self::Hsx(collector) => collector.set_config(config),
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
            Self::Hsx(collector) => collector.sample_config(interval).await,
            Self::Skx(collector) => collector.sample_config(interval).await,
            collector => Err(format!(
                "PMU configs are not supported on {}",
                collector.architecture()
            )),
        }
    }
}

impl ImcCollector {
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
pub struct ImcTask {
    collector: ImcCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
    requests: ConfigRequests,
}

impl ImcTask {
    pub fn new(
        collector: ImcCollector,
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
pub enum ImcPrometheusMetrics {
    Emr(spr::SprImcPrometheusMetrics),
    Hsx(hsx::HsxImcPrometheusMetrics),
    Icx(icx::IcxImcPrometheusMetrics),
    Ivb(snb::SnbImcPrometheusMetrics),
    Skx(skx::ImcPrometheusMetrics),
    Snb(snb::SnbImcPrometheusMetrics),
    Spr(spr::SprImcPrometheusMetrics),
}

impl ImcPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SandyBridgeEp) => {
                Some(Self::Snb(snb::SnbImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IvyTown) => {
                Some(Self::Ivb(snb::SnbImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Hsx(hsx::HsxImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IceLakeXeon) => {
                Some(Self::Icx(icx::IcxImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::ImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SapphireRapids) => {
                Some(Self::Spr(spr::SprImcPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::EmeraldRapids) => {
                Some(Self::Emr(spr::SprImcPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: ImcMetrics) {
        match (self, metrics) {
            (Self::Emr(prometheus), ImcMetrics::Emr(metrics)) => prometheus.update(metrics),
            (Self::Hsx(prometheus), ImcMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Icx(prometheus), ImcMetrics::Icx(metrics)) => prometheus.update(metrics),
            (Self::Ivb(prometheus), ImcMetrics::Ivb(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), ImcMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Snb(prometheus), ImcMetrics::Snb(metrics)) => prometheus.update(metrics),
            (Self::Spr(prometheus), ImcMetrics::Spr(metrics)) => prometheus.update(metrics),
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
        assert!(ImcCollector::is_supported(&test_architecture(0x2d)));
        assert!(ImcCollector::is_supported(&test_architecture(0x3e)));
        assert!(ImcCollector::is_supported(&test_architecture(0x3f)));
        assert!(ImcCollector::is_supported(&test_architecture(0x4f)));
        assert!(ImcCollector::is_supported(&test_architecture(0x55)));
        assert!(ImcCollector::is_supported(&test_architecture(0x6a)));
        assert!(!ImcCollector::is_supported(&test_architecture(0x6c)));
        assert!(ImcCollector::is_supported(&test_architecture(0x8f)));
        assert!(ImcCollector::is_supported(&test_architecture(0xcf)));
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
