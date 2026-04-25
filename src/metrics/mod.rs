mod info;
pub mod tsc;

use prometheus_client::registry::Registry;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct MetricsState {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsc: Option<tsc::TscMetrics>,
}

impl MetricsState {
    pub fn apply(&mut self, update: MetricUpdate) {
        match update {
            MetricUpdate::Tsc(tsc) => self.tsc = Some(tsc),
        }
    }
}

impl Default for MetricsState {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            tsc: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessorMetadata {
    pub invariant_tsc_supported: bool,
}

#[derive(Clone, Debug)]
pub enum MetricEvent {
    Failure(String),
    Update(MetricUpdate),
}

#[derive(Clone, Debug)]
pub enum MetricUpdate {
    Tsc(tsc::TscMetrics),
}

#[derive(Debug)]
pub struct MetricsRegistry {
    tsc: tsc::TscPrometheusMetrics,
}

impl MetricsRegistry {
    pub fn register(registry: &mut Registry, processor: ProcessorMetadata) -> Self {
        info::register(registry, processor);

        Self {
            tsc: tsc::TscPrometheusMetrics::register(registry),
        }
    }

    pub fn update(&self, state: MetricsState) {
        self.tsc.update(state.tsc);
    }
}
