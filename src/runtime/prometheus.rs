use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::metrics::MetricsRegistry;
use crate::runtime::sampler::SamplerReader;

const OPENMETRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

#[derive(Debug)]
struct PrometheusExporter {
    sampler: SamplerReader,
    registry: Registry,
    metrics: MetricsRegistry,
}

impl PrometheusExporter {
    fn new(sampler: SamplerReader) -> Self {
        let mut registry = Registry::default();
        let metrics = MetricsRegistry::register(&mut registry, sampler.metadata().processor);

        Self {
            sampler,
            registry,
            metrics,
        }
    }

    async fn update_scrape_metrics(&self) {
        self.metrics.update(self.sampler.latest_state().await);
    }

    async fn render_metrics(&self) -> Result<String, std::fmt::Error> {
        self.update_scrape_metrics().await;

        let mut metrics = String::new();
        encode(&mut metrics, &self.registry)?;
        Ok(metrics)
    }
}

#[derive(Debug)]
struct AppState {
    exporter: PrometheusExporter,
}

impl AppState {
    fn new(sampler: SamplerReader) -> Self {
        Self {
            exporter: PrometheusExporter::new(sampler),
        }
    }
}

pub async fn run(
    listen: SocketAddr,
    sampler: SamplerReader,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("failed to bind {listen}: {error}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| format!("failed to read listener address: {error}"))?;
    let state = Arc::new(AppState::new(sampler));
    let app = Router::new()
        .route("/", get(healthz))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .with_state(state);

    eprintln!(
        "ocellus {}: serving Prometheus metrics on http://{local_addr}/metrics",
        env!("CARGO_PKG_VERSION")
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await
        .map_err(|error| format!("server failed: {error}"))
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    match state.exporter.render_metrics().await {
        Ok(metrics) => (
            StatusCode::OK,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static(OPENMETRICS_CONTENT_TYPE),
            )],
            metrics,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("failed to render metrics: {error}\n"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                processor: crate::metrics::ProcessorMetadata {
                    invariant_tsc_supported: true,
                },
            },
            crate::metrics::MetricsState::default(),
        );
        let exporter = PrometheusExporter::new(sampler);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_info gauge"));
        assert!(metrics.contains("ocellus_info"));
        assert!(metrics.contains("# TYPE processor_info gauge"));
        assert!(metrics.contains("processor_info"));
        assert!(!metrics.contains("ocellus_up"));
        assert!(!metrics.contains("ocellus_tsc_supported"));
        assert!(metrics.contains("processor_info{invariant_tsc_supported=\"true\"}"));
        assert!(!metrics.contains("ocellus_info{invariant_tsc_supported"));
        assert!(!metrics.contains("ocellus_invariant_tsc_supported"));
        assert!(!metrics.contains("ocellus_tsc_frequency_hz"));
        assert!(!metrics.contains("ocellus_tsc_sample_cycles"));
    }

    #[test]
    fn renders_tsc_metric_after_first_sample() {
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                processor: crate::metrics::ProcessorMetadata {
                    invariant_tsc_supported: true,
                },
            },
            crate::metrics::MetricsState {
                version: env!("CARGO_PKG_VERSION").to_string(),
                tsc: Some(crate::metrics::tsc::TscMetrics {
                    frequency_hz: 2_400_000_000.0,
                }),
            },
        );
        let exporter = PrometheusExporter::new(sampler);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_tsc_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_tsc_frequency_hz"));
        assert!(metrics.contains("2400000000"));
    }
}
