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
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, oneshot};

use crate::metrics::MetricsRegistry;
use crate::runtime::sampler::SamplerReader;

const OPENMETRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

#[derive(Debug)]
struct PrometheusExporter {
    metrics: Arc<MetricsRegistry>,
    registry: Mutex<Registry>,
}

impl PrometheusExporter {
    fn new(sampler: SamplerReader) -> Self {
        let mut registry = Registry::default();
        let metrics = Arc::new(MetricsRegistry::register(
            &mut registry,
            sampler.metadata().info,
        ));

        Self {
            metrics,
            registry: Mutex::new(registry),
        }
    }

    #[cfg(test)]
    fn update_state(&self, state: crate::metrics::MetricsState) {
        self.metrics.update_state(state);
    }

    fn spawn_updater(&self, sampler: SamplerReader) {
        let metrics = self.metrics.clone();
        let mut updates = sampler.subscribe_updates();

        tokio::spawn(async move {
            loop {
                match updates.recv().await {
                    Ok(update) => metrics.update(update),
                    Err(RecvError::Lagged(skipped)) => {
                        eprintln!("ocellus: Prometheus metrics updater skipped {skipped} updates");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        });
    }

    async fn render_metrics(&self) -> Result<String, std::fmt::Error> {
        let mut metrics = String::new();
        let registry = self.registry.lock().await;
        encode(&mut metrics, &registry)?;
        Ok(metrics)
    }
}

#[derive(Debug)]
struct AppState {
    exporter: PrometheusExporter,
}

impl AppState {
    fn new(sampler: SamplerReader) -> Self {
        let exporter = PrometheusExporter::new(sampler.clone());
        exporter.spawn_updater(sampler);

        Self { exporter }
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

    fn info_metadata() -> crate::metrics::InfoMetadata {
        crate::metrics::InfoMetadata {
            collectors: crate::metrics::CollectorMetadata {
                imc_supported: true,
            },
            processor: crate::metrics::ProcessorMetadata {
                brand: "Intel(R) Xeon(R) Gold 6252 CPU @ 2.10GHz".to_string(),
                family: 6,
                invariant_tsc_supported: true,
                model: 85,
                package_rapl_supported: true,
                vendor: "GenuineIntel".to_string(),
            },
        }
    }

    #[test]
    fn renders_prometheus_text() {
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: info_metadata(),
            },
            crate::metrics::MetricsState::default(),
        );
        let state = crate::metrics::MetricsState::default();
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_info gauge"));
        assert!(metrics.contains("ocellus_info"));
        assert!(metrics.contains("# TYPE ocellus_collector_info gauge"));
        assert!(metrics.contains("ocellus_collector_info"));
        assert!(metrics.contains("# TYPE processor_info gauge"));
        assert!(metrics.contains("processor_info"));
        assert!(!metrics.contains("ocellus_up"));
        assert!(!metrics.contains("ocellus_tsc_supported"));
        assert!(metrics.contains("imc_supported=\"true\""));
        assert!(
            !metrics
                .lines()
                .any(|line| line.starts_with("processor_info{") && line.contains("imc_supported"))
        );
        assert!(metrics.contains("invariant_tsc_supported=\"true\""));
        assert!(metrics.contains("package_rapl_supported=\"true\""));
        assert!(metrics.contains("vendor=\"GenuineIntel\""));
        assert!(!metrics.contains("ocellus_info{invariant_tsc_supported"));
        assert!(!metrics.contains("ocellus_invariant_tsc_supported"));
        assert!(!metrics.contains("ocellus_tsc_frequency_hz"));
        assert!(!metrics.contains("ocellus_tsc_sample_cycles"));
    }

    #[test]
    fn renders_tsc_metric_after_first_sample() {
        let state = crate::metrics::MetricsState {
            imc: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            rapl: None,
            tsc: Some(crate::metrics::tsc::TscMetrics {
                frequency_hz: 2_400_000_000.0,
            }),
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_tsc_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_tsc_frequency_hz"));
        assert!(metrics.contains("2400000000"));
    }

    #[test]
    fn renders_rapl_domain_metrics() {
        let state = crate::metrics::MetricsState {
            imc: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            rapl: Some(crate::metrics::rapl::RaplMetrics {
                domains: vec![crate::metrics::rapl::RaplDomainMetrics {
                    domain: crate::metrics::rapl::RaplDomainKind::Package,
                    energy_joules_total: 42.0,
                    power_watts: 21.0,
                    scope: crate::metrics::rapl::RaplScope {
                        die_group_id: 0,
                        die_id: 0,
                        package_id: 0,
                    },
                }],
            }),
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_rapl_energy_joules counter"));
        assert!(metrics.contains("ocellus_rapl_energy_joules_total"));
        assert!(metrics.contains("die_group=\"0\""));
        assert!(metrics.contains("die=\"0\""));
        assert!(metrics.contains("domain=\"package\""));
        assert!(metrics.contains("package=\"0\""));
        assert!(metrics.contains("# TYPE ocellus_rapl_power_watts gauge"));
        assert!(metrics.contains("ocellus_rapl_power_watts"));
    }

    #[test]
    fn renders_imc_metrics() {
        let state = crate::metrics::MetricsState {
            imc: Some(crate::metrics::imc::skx::ImcMetrics {
                scopes: vec![crate::metrics::imc::skx::ImcScopeMetrics {
                    activate_commands_per_second: 128.0,
                    frequency_hz: 1_000_000_000.0,
                    page_miss_precharge_commands_per_second: 512.0,
                    read_cas_commands_per_second: 1024.0,
                    read_bytes_per_second: 1024.0,
                    rpq_residency_seconds: 0.000001,
                    rpq_occupancy_entries: 0.5,
                    scope: crate::metrics::imc::skx::ImcScope {
                        die_group_id: 0,
                        die_id: 0,
                        package_id: 0,
                    },
                    write_cas_commands_per_second: 2048.0,
                    write_bytes_per_second: 2048.0,
                    wpq_residency_seconds: 0.000002,
                    wpq_occupancy_entries: 0.75,
                }],
            }),
            version: env!("CARGO_PKG_VERSION").to_string(),
            rapl: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_imc_activate_commands_per_second gauge"));
        assert!(metrics.contains("ocellus_imc_activate_commands_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_imc_frequency_hz"));
        assert!(
            metrics.contains("# TYPE ocellus_imc_page_miss_precharge_commands_per_second gauge")
        );
        assert!(metrics.contains("ocellus_imc_page_miss_precharge_commands_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_read_cas_commands_per_second gauge"));
        assert!(metrics.contains("ocellus_imc_read_cas_commands_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_read_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_imc_read_bytes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_rpq_residency_seconds gauge"));
        assert!(metrics.contains("ocellus_imc_rpq_residency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_imc_rpq_occupancy_entries gauge"));
        assert!(metrics.contains("ocellus_imc_rpq_occupancy_entries"));
        assert!(metrics.contains("# TYPE ocellus_imc_write_cas_commands_per_second gauge"));
        assert!(metrics.contains("ocellus_imc_write_cas_commands_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_write_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_imc_write_bytes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_imc_wpq_residency_seconds gauge"));
        assert!(metrics.contains("ocellus_imc_wpq_residency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_imc_wpq_occupancy_entries gauge"));
        assert!(metrics.contains("ocellus_imc_wpq_occupancy_entries"));
    }
}
