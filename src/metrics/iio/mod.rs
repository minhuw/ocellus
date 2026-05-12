pub mod skx;

use prometheus_client::registry::Registry;

use crate::arch::IntelServerCpuModel;
use crate::metrics::InfoMetadata;

pub use skx::{IioCollector, IioMetrics, IioTask};

#[derive(Debug)]
pub enum IioPrometheusMetrics {
    Skx(skx::IioPrometheusMetrics),
}

impl IioPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::IioPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: IioMetrics) {
        match self {
            Self::Skx(prometheus) => prometheus.update(metrics),
        }
    }
}
