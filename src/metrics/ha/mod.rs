pub mod hsx;

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
pub enum HaMetrics {
    Hsx(hsx::HsxHaMetrics),
}

/// The home agents; only Haswell-EP and Broadwell-EP have them as PMU boxes
/// of their own.
#[derive(Debug)]
pub enum HaCollector {
    Hsx(hsx::HsxHaCollector),
}

impl HaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxHaCollector::new(architecture).map(Self::Hsx)
            }
            model => Err(format!("HA collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon)
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<HaMetrics, String> {
        match self {
            Self::Hsx(collector) => collector.sample(interval).await.map(HaMetrics::Hsx),
        }
    }
}

impl ConfigurableCollector for HaCollector {
    async fn sample_update(&mut self, interval: Duration) -> Result<MetricUpdate, String> {
        self.sample(interval)
            .await
            .map(|ha| MetricUpdate::Ha(Box::new(ha)))
    }

    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        BTreeMap::new()
    }

    fn has_config(&self) -> bool {
        false
    }

    fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        match config {
            None => Ok(()),
            Some(_) => Err("PMU configs are not supported on home agents".to_string()),
        }
    }

    async fn sample_config(&mut self, _interval: Duration) -> Result<UncoreCounts, String> {
        Err("PMU configs are not supported on home agents".to_string())
    }
}

#[derive(Debug)]
pub struct HaTask {
    collector: HaCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
    requests: ConfigRequests,
}

impl HaTask {
    pub fn new(
        collector: HaCollector,
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
pub enum HaPrometheusMetrics {
    Hsx(hsx::HsxHaPrometheusMetrics),
}

impl HaPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Hsx(hsx::HsxHaPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: HaMetrics) {
        match (self, metrics) {
            (Self::Hsx(prometheus), HaMetrics::Hsx(metrics)) => prometheus.update(metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_haswell_ep_and_broadwell_ep_only() {
        assert!(HaCollector::is_supported(&test_architecture(0x3f)));
        assert!(HaCollector::is_supported(&test_architecture(0x4f)));
        for model in [0x2d, 0x3e, 0x55, 0x6a, 0x8f, 0xcf] {
            assert!(!HaCollector::is_supported(&test_architecture(model)));
        }
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
