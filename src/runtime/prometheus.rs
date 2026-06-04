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
                cha_supported: true,
                iio_supported: true,
                imc_supported: true,
                interconnect_supported: false,
                irp_supported: true,
                pcu_supported: true,
                rdt_supported: true,
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

    fn haswell_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) CPU E5-2699 v4 @ 2.20GHz".to_string();
        metadata.processor.model = 0x4f;
        metadata
    }

    fn sandy_bridge_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) CPU E5-2690 0 @ 2.90GHz".to_string();
        metadata.processor.model = 0x2d;
        metadata
    }

    fn ivy_bridge_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) CPU E5-2697 v2 @ 2.70GHz".to_string();
        metadata.processor.model = 0x3e;
        metadata
    }

    fn skylake_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) Gold 6154 CPU @ 3.00GHz".to_string();
        metadata.processor.model = 0x55;
        metadata
    }

    fn ice_lake_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) Platinum 8380 CPU @ 2.30GHz".to_string();
        metadata.processor.model = 0x6a;
        metadata
    }

    fn sapphire_rapids_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) Platinum 8480+ CPU @ 2.00GHz".to_string();
        metadata.processor.model = 0x8f;
        metadata
    }

    fn emerald_rapids_info_metadata() -> crate::metrics::InfoMetadata {
        let mut metadata = info_metadata();
        metadata.processor.brand = "Intel(R) Xeon(R) Platinum 8592+ CPU @ 1.90GHz".to_string();
        metadata.processor.model = 0xcf;
        metadata
    }

    fn unsupported_info_metadata() -> crate::metrics::InfoMetadata {
        crate::metrics::InfoMetadata {
            collectors: crate::metrics::CollectorMetadata {
                cha_supported: false,
                iio_supported: false,
                imc_supported: false,
                interconnect_supported: false,
                irp_supported: false,
                pcu_supported: false,
                rdt_supported: false,
            },
            processor: crate::metrics::ProcessorMetadata {
                brand: "unsupported".to_string(),
                family: 6,
                invariant_tsc_supported: false,
                model: 0,
                package_rapl_supported: false,
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
        assert!(metrics.contains("interconnect_supported=\"false\""));
        assert!(metrics.contains("pcu_supported=\"true\""));
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
    fn skips_unsupported_optional_metric_families() {
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: unsupported_info_metadata(),
            },
            crate::metrics::MetricsState::default(),
        );
        let exporter = PrometheusExporter::new(sampler);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("cha_supported=\"false\""));
        assert!(!metrics.contains("ocellus_cha_"));
        assert!(!metrics.contains("ocellus_iio_"));
        assert!(!metrics.contains("ocellus_imc_"));
        assert!(!metrics.contains("ocellus_interconnect_"));
        assert!(!metrics.contains("ocellus_irp_"));
        assert!(!metrics.contains("ocellus_pcu_"));
        assert!(!metrics.contains("ocellus_rapl_"));
    }

    #[test]
    fn renders_tsc_metric_after_first_sample() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
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
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: Some(crate::metrics::rapl::RaplMetrics {
                domains: vec![crate::metrics::rapl::RaplDomainMetrics {
                    domain: crate::metrics::rapl::RaplDomainKind::Package,
                    energy_joules_total: 42.0,
                    power_watts: 21.0,
                    scope: crate::metrics::rapl::RaplScope {
                        die_group_id: None,
                        die_id: None,
                        package_id: 0,
                    },
                }],
            }),
            rdt: None,
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
        assert!(metrics.contains("die_group=\"unknown\""));
        assert!(metrics.contains("die=\"unknown\""));
        assert!(metrics.contains("domain=\"package\""));
        assert!(metrics.contains("package=\"0\""));
        assert!(metrics.contains("# TYPE ocellus_rapl_power_watts gauge"));
        assert!(metrics.contains("ocellus_rapl_power_watts"));
    }

    #[test]
    fn renders_rdt_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: Some(crate::metrics::rdt::RdtMetrics {
                scopes: vec![crate::metrics::rdt::RdtScopeMetrics {
                    l3_occupancy_bytes: Some(4096.0),
                    local_memory_bandwidth_bytes_per_second: Some(2048.0),
                    remote_memory_bandwidth_bytes_per_second: Some(1024.0),
                    scope: crate::metrics::rdt::RdtScope {
                        core_id: 1,
                        cpu: 2,
                        die_group_id: None,
                        die_id: None,
                        package_id: 0,
                    },
                    total_memory_bandwidth_bytes_per_second: Some(3072.0),
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

        assert!(metrics.contains("# TYPE ocellus_rdt_l3_occupancy_bytes gauge"));
        assert!(metrics.contains("ocellus_rdt_l3_occupancy_bytes"));
        assert!(metrics.contains("# TYPE ocellus_rdt_memory_bandwidth_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_rdt_memory_bandwidth_bytes_per_second"));
        assert!(metrics.contains("core=\"1\""));
        assert!(metrics.contains("cpu=\"2\""));
        assert!(metrics.contains("die_group=\"unknown\""));
        assert!(metrics.contains("die=\"unknown\""));
        assert!(metrics.contains("traffic=\"total\""));
        assert!(metrics.contains("traffic=\"local\""));
        assert!(metrics.contains("traffic=\"remote\""));
    }

    #[test]
    fn renders_qpi_interconnect_metrics() {
        assert_renders_interconnect_metrics(
            haswell_info_metadata(),
            crate::metrics::interconnect::InterconnectMetrics::Bdx(hsx_interconnect_metrics()),
            true,
        );
    }

    #[test]
    fn renders_upi_interconnect_metrics() {
        assert_renders_interconnect_metrics(
            skylake_info_metadata(),
            crate::metrics::interconnect::InterconnectMetrics::Skx(skx_interconnect_metrics()),
            false,
        );
    }

    fn assert_renders_interconnect_metrics(
        mut metadata: crate::metrics::InfoMetadata,
        interconnect: crate::metrics::interconnect::InterconnectMetrics,
        has_queues: bool,
    ) {
        metadata.collectors.interconnect_supported = true;
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: Some(interconnect),
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("interconnect_supported=\"true\""));
        assert!(metrics.contains("# TYPE ocellus_interconnect_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_interconnect_data_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_interconnect_flits_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_interconnect_power_state_ratio gauge"));
        assert!(metrics.contains("ocellus_interconnect_frequency_hz"));
        assert!(metrics.contains("ocellus_interconnect_data_bytes_per_second"));
        assert!(metrics.contains("ocellus_interconnect_flits_per_second"));
        assert!(metrics.contains("ocellus_interconnect_power_state_ratio"));
        assert!(metrics.contains("direction=\"rx\""));
        assert!(metrics.contains("direction=\"tx\""));
        assert!(metrics.contains("traffic=\"data\""));
        assert!(metrics.contains("traffic=\"non_data\""));
        assert!(metrics.contains("state=\"l1\""));

        if has_queues {
            assert!(metrics.contains("# TYPE ocellus_interconnect_queue_inserts_per_second gauge"));
            assert!(metrics.contains("# TYPE ocellus_interconnect_queue_latency_seconds gauge"));
            assert!(metrics.contains("# TYPE ocellus_interconnect_queue_occupancy_flits gauge"));
        } else {
            assert!(!metrics.contains("ocellus_interconnect_queue_inserts_per_second"));
            assert!(!metrics.contains("ocellus_interconnect_queue_latency_seconds"));
            assert!(!metrics.contains("ocellus_interconnect_queue_occupancy_flits"));
        }
    }

    fn hsx_interconnect_metrics() -> crate::metrics::interconnect::hsx::HsxInterconnectMetrics {
        let scope = interconnect_scope();
        crate::metrics::interconnect::hsx::HsxInterconnectMetrics {
            links: vec![crate::metrics::interconnect::InterconnectLinkMetrics {
                frequency_hz: 8_000_000_000.0,
                scope,
            }],
            power_states: vec![
                crate::metrics::interconnect::InterconnectPowerStateMetrics {
                    direction: None,
                    ratio: 0.125,
                    scope,
                    state: crate::metrics::interconnect::InterconnectPowerState::L1,
                },
                crate::metrics::interconnect::InterconnectPowerStateMetrics {
                    direction: Some(crate::metrics::interconnect::InterconnectDirection::Tx),
                    ratio: 0.25,
                    scope,
                    state: crate::metrics::interconnect::InterconnectPowerState::L0p,
                },
            ],
            queues: vec![crate::metrics::interconnect::InterconnectQueueMetrics {
                direction: crate::metrics::interconnect::InterconnectDirection::Rx,
                inserts_per_second: 100.0,
                latency_seconds: 0.000001,
                occupancy_flits: 2.0,
                scope,
            }],
            traffic: interconnect_traffic(scope),
        }
    }

    fn skx_interconnect_metrics() -> crate::metrics::interconnect::skx::SkxInterconnectMetrics {
        let scope = interconnect_scope();
        crate::metrics::interconnect::skx::SkxInterconnectMetrics {
            links: vec![crate::metrics::interconnect::InterconnectLinkMetrics {
                frequency_hz: 10_400_000_000.0,
                scope,
            }],
            power_states: vec![
                crate::metrics::interconnect::InterconnectPowerStateMetrics {
                    direction: None,
                    ratio: 0.125,
                    scope,
                    state: crate::metrics::interconnect::InterconnectPowerState::L1,
                },
            ],
            traffic: interconnect_traffic(scope),
        }
    }

    fn interconnect_scope() -> crate::metrics::interconnect::InterconnectScope {
        crate::metrics::interconnect::InterconnectScope {
            die_group_id: None,
            die_id: None,
            link_id: 1,
            package_id: 0,
        }
    }

    fn interconnect_traffic(
        scope: crate::metrics::interconnect::InterconnectScope,
    ) -> Vec<crate::metrics::interconnect::InterconnectTrafficMetrics> {
        vec![
            crate::metrics::interconnect::InterconnectTrafficMetrics {
                bytes_per_second: Some(1024.0),
                direction: crate::metrics::interconnect::InterconnectDirection::Tx,
                flits_per_second: 128.0,
                scope,
                traffic: crate::metrics::interconnect::InterconnectTrafficClass::Data,
            },
            crate::metrics::interconnect::InterconnectTrafficMetrics {
                bytes_per_second: None,
                direction: crate::metrics::interconnect::InterconnectDirection::Rx,
                flits_per_second: 64.0,
                scope,
                traffic: crate::metrics::interconnect::InterconnectTrafficClass::NonData,
            },
        ]
    }

    #[test]
    fn renders_imc_metrics() {
        assert_renders_server_imc_metrics(
            info_metadata(),
            crate::metrics::imc::ImcMetrics::Skx(crate::metrics::imc::skx::ImcMetrics {
                scopes: vec![crate::metrics::imc::skx::ImcScopeMetrics {
                    activate_commands_per_second: 128.0,
                    frequency_hz: 1_000_000_000.0,
                    page_miss_precharge_commands_per_second: 512.0,
                    read_cas_commands_per_second: 1024.0,
                    read_bytes_per_second: 1024.0,
                    rpq_residency_seconds: 0.000001,
                    rpq_occupancy_entries: 0.5,
                    scope: crate::metrics::imc::skx::ImcScope {
                        die_group_id: None,
                        die_id: None,
                        package_id: 0,
                    },
                    write_cas_commands_per_second: 2048.0,
                    write_bytes_per_second: 2048.0,
                    wpq_residency_seconds: 0.000002,
                    wpq_occupancy_entries: 0.75,
                }],
            }),
        );
    }

    #[test]
    fn renders_ice_lake_imc_metrics() {
        assert_renders_server_imc_metrics(
            ice_lake_info_metadata(),
            crate::metrics::imc::ImcMetrics::Icx(icx_imc_metrics()),
        );
    }

    #[test]
    fn renders_sandy_bridge_imc_metrics() {
        assert_renders_snb_imc_metrics(
            sandy_bridge_info_metadata(),
            crate::metrics::imc::ImcMetrics::Snb(snb_imc_metrics()),
        );
    }

    #[test]
    fn renders_ivy_bridge_imc_metrics() {
        assert_renders_snb_imc_metrics(
            ivy_bridge_info_metadata(),
            crate::metrics::imc::ImcMetrics::Ivb(snb_imc_metrics()),
        );
    }

    #[test]
    fn renders_sapphire_rapids_imc_metrics() {
        assert_renders_server_imc_metrics(
            sapphire_rapids_info_metadata(),
            crate::metrics::imc::ImcMetrics::Spr(spr_imc_metrics()),
        );
    }

    #[test]
    fn renders_emerald_rapids_imc_metrics() {
        assert_renders_server_imc_metrics(
            emerald_rapids_info_metadata(),
            crate::metrics::imc::ImcMetrics::Emr(spr_imc_metrics()),
        );
    }

    fn assert_renders_server_imc_metrics(
        metadata: crate::metrics::InfoMetadata,
        imc: crate::metrics::imc::ImcMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: Some(imc),
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
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

    fn assert_renders_snb_imc_metrics(
        metadata: crate::metrics::InfoMetadata,
        imc: crate::metrics::imc::ImcMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: Some(imc),
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_imc_activate_commands_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_frequency_hz gauge"));
        assert!(
            metrics.contains("# TYPE ocellus_imc_page_miss_precharge_commands_per_second gauge")
        );
        assert!(metrics.contains("# TYPE ocellus_imc_read_cas_commands_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_read_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_rpq_non_empty_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_write_cas_commands_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_write_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_wpq_full_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_imc_wpq_non_empty_ratio gauge"));
        assert!(metrics.contains("package=\"0\""));
        assert!(!metrics.contains("ocellus_imc_rpq_residency_seconds"));
        assert!(!metrics.contains("ocellus_imc_rpq_occupancy_entries"));
    }

    fn icx_imc_metrics() -> crate::metrics::imc::icx::IcxImcMetrics {
        crate::metrics::imc::icx::IcxImcMetrics {
            scopes: vec![crate::metrics::imc::icx::IcxImcScopeMetrics {
                activate_commands_per_second: 128.0,
                frequency_hz: 1_000_000_000.0,
                page_miss_precharge_commands_per_second: 512.0,
                read_cas_commands_per_second: 1024.0,
                read_bytes_per_second: 1024.0,
                rpq_residency_seconds: 0.000001,
                rpq_occupancy_entries: 0.5,
                scope: crate::metrics::imc::icx::IcxImcScope {
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
                write_cas_commands_per_second: 2048.0,
                write_bytes_per_second: 2048.0,
                wpq_residency_seconds: 0.000002,
                wpq_occupancy_entries: 0.75,
            }],
        }
    }

    fn snb_imc_metrics() -> crate::metrics::imc::snb::SnbImcMetrics {
        crate::metrics::imc::snb::SnbImcMetrics {
            scopes: vec![crate::metrics::imc::snb::SnbImcScopeMetrics {
                activate_commands_per_second: 128.0,
                frequency_hz: 1_000_000_000.0,
                page_miss_precharge_commands_per_second: 512.0,
                read_cas_commands_per_second: 1024.0,
                read_bytes_per_second: 1024.0,
                rpq_non_empty_ratio: 0.25,
                scope: crate::metrics::imc::snb::SnbImcScope { package_id: 0 },
                write_cas_commands_per_second: 2048.0,
                write_bytes_per_second: 2048.0,
                wpq_full_ratio: 0.5,
                wpq_non_empty_ratio: 0.75,
            }],
        }
    }

    fn spr_imc_metrics() -> crate::metrics::imc::spr::SprImcMetrics {
        crate::metrics::imc::spr::SprImcMetrics {
            scopes: vec![crate::metrics::imc::spr::SprImcScopeMetrics {
                activate_commands_per_second: 128.0,
                frequency_hz: 1_000_000_000.0,
                page_miss_precharge_commands_per_second: 512.0,
                read_cas_commands_per_second: 1024.0,
                read_bytes_per_second: 1024.0,
                rpq_residency_seconds: 0.000001,
                rpq_occupancy_entries: 0.5,
                scope: crate::metrics::imc::spr::SprImcScope {
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
                write_cas_commands_per_second: 2048.0,
                write_bytes_per_second: 2048.0,
                wpq_residency_seconds: 0.000002,
                wpq_occupancy_entries: 0.75,
            }],
        }
    }

    #[test]
    fn renders_iio_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: Some(crate::metrics::iio::IioMetrics::Skx(
                crate::metrics::iio::skx::SkxIioMetrics {
                    ports: vec![crate::metrics::iio::skx::IioPciePortMetrics {
                        port_id: 1,
                        read_bytes_per_second: 1024.0,
                        scope: crate::metrics::uncore::skx::UncoreScope {
                            die_group_id: None,
                            die_id: None,
                            package_id: 0,
                        },
                        stack: crate::metrics::uncore::skx::SkxIioStack::Pcie1,
                        write_bytes_per_second: 2048.0,
                    }],
                    scopes: vec![crate::metrics::iio::skx::IioScopeMetrics {
                        completion_inserts_per_second: 1.0,
                        completion_latency_seconds: 0.000002,
                        completion_occupancy_entries: 2.0,
                        frequency_hz: 1_000_000_000.0,
                        l1_misses_per_second: 4.0,
                        l2_misses_per_second: 5.0,
                        l3_misses_per_second: 6.0,
                        scope: crate::metrics::uncore::skx::UncoreScope {
                            die_group_id: None,
                            die_id: None,
                            package_id: 0,
                        },
                        stack: crate::metrics::uncore::skx::SkxIioStack::Pcie1,
                        tlb_hits_per_second: 10.0,
                        tlb_misses_per_second: 11.0,
                        vtd_accesses_per_second: 12.0,
                        vtd_latency_seconds: 0.000001,
                        vtd_occupancy_entries: 13.0,
                    }],
                },
            )),
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
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

        assert!(metrics.contains("# TYPE ocellus_iio_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_iio_frequency_hz"));
        assert!(metrics.contains("# TYPE ocellus_iio_tlb_misses_per_second gauge"));
        assert!(metrics.contains("ocellus_iio_tlb_misses_per_second"));
        assert!(metrics.contains("# TYPE ocellus_iio_vtd_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_iio_vtd_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_iio_completion_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_iio_completion_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_iio_pcie_read_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_iio_pcie_read_bytes_per_second"));
        assert!(metrics.contains("stack=\"pcie1\""));
        assert!(metrics.contains("port=\"1\""));
    }

    #[test]
    fn renders_sapphire_rapids_iio_metrics() {
        assert_renders_spr_iio_metrics(
            sapphire_rapids_info_metadata(),
            crate::metrics::iio::IioMetrics::Spr(spr_iio_metrics()),
        );
    }

    #[test]
    fn renders_emerald_rapids_iio_metrics() {
        assert_renders_spr_iio_metrics(
            emerald_rapids_info_metadata(),
            crate::metrics::iio::IioMetrics::Emr(spr_iio_metrics()),
        );
    }

    fn assert_renders_spr_iio_metrics(
        metadata: crate::metrics::InfoMetadata,
        iio: crate::metrics::iio::IioMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: Some(iio),
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_iio_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_inbound_read_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_inbound_write_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_pcie_read_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_pcie_write_bytes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_completion_inserts_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_completion_latency_seconds gauge"));
        assert!(metrics.contains("# TYPE ocellus_iio_completion_occupancy_entries gauge"));
        assert!(metrics.contains("stack=\"m2iosf10\""));
    }

    fn spr_iio_metrics() -> crate::metrics::iio::spr::SprIioMetrics {
        crate::metrics::iio::spr::SprIioMetrics {
            ports: vec![crate::metrics::iio::spr::SprIioPciePortMetrics {
                port_id: 1,
                read_bytes_per_second: 1024.0,
                scope: crate::metrics::uncore::skx::UncoreScope {
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
                stack: crate::metrics::iio::spr::SPR_IIO_STACKS[10],
                write_bytes_per_second: 2048.0,
            }],
            scopes: vec![crate::metrics::iio::spr::SprIioScopeMetrics {
                completion_inserts_per_second: 3.0,
                completion_latency_seconds: 0.000004,
                completion_occupancy_entries: 5.0,
                frequency_hz: 1_000_000_000.0,
                scope: crate::metrics::uncore::skx::UncoreScope {
                    die_group_id: None,
                    die_id: None,
                    package_id: 0,
                },
                stack: crate::metrics::iio::spr::SPR_IIO_STACKS[10],
                inbound_read_bytes_per_second: 1024.0,
                inbound_reads_per_second: 2.0,
                inbound_write_bytes_per_second: 2048.0,
                inbound_writes_per_second: 4.0,
            }],
        }
    }

    #[test]
    fn renders_irp_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: Some(crate::metrics::irp::IrpMetrics::Skx(
                crate::metrics::irp::skx::SkxIrpMetrics {
                    scopes: vec![crate::metrics::irp::skx::IrpScopeMetrics {
                        clflush_bytes_per_second: 3.0,
                        core_read_bytes_per_second: 8.0,
                        demand_read_bytes_per_second: 9.0,
                        faf_occupancy_entries: 2.0,
                        pcie_inbound_reads_per_second: 13.0,
                        frequency_hz: 1_000_000_000.0,
                        io_write_conflict_ratio: 0.25,
                        pci_dca_hint_bytes_per_second: 10.0,
                        pci_itom_bytes_per_second: 4.0,
                        pcie_read_current_bytes_per_second: 5.0,
                        read_for_ownership_bytes_per_second: 6.0,
                        scope: crate::metrics::uncore::skx::UncoreScope {
                            die_group_id: None,
                            die_id: None,
                            package_id: 0,
                        },
                        stack: crate::metrics::uncore::skx::SkxIioStack::Pcie1,
                        total_irp_occupancy_entries: 11.0,
                        wbmtoi_bytes_per_second: 7.0,
                        pcie_inbound_writes_per_second: 12.0,
                        pcie_inbound_write_latency_seconds: 0.000001,
                    }],
                },
            )),
            pcu: None,
            rapl: None,
            rdt: None,
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

        assert!(metrics.contains("# TYPE ocellus_irp_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_irp_frequency_hz"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_inbound_write_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_irp_pcie_inbound_write_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_read_current_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_irp_pcie_read_current_bytes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_inbound_writes_per_second gauge"));
        assert!(metrics.contains("ocellus_irp_pcie_inbound_writes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_inbound_reads_per_second gauge"));
        assert!(metrics.contains("ocellus_irp_pcie_inbound_reads_per_second"));
        assert!(metrics.contains("# TYPE ocellus_irp_io_write_conflict_ratio gauge"));
        assert!(metrics.contains("ocellus_irp_io_write_conflict_ratio"));
        assert!(metrics.contains("stack=\"pcie1\""));
    }

    #[test]
    fn renders_emerald_rapids_irp_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: Some(crate::metrics::irp::IrpMetrics::Emr(
                crate::metrics::irp::spr::SprIrpMetrics {
                    scopes: vec![crate::metrics::irp::spr::SprIrpScopeMetrics {
                        all_hit_m_snoop_responses_per_second: 1.0,
                        faf_full_ratio: 0.25,
                        faf_occupancy_entries: 2.0,
                        pcie_inbound_reads_per_second: 3.0,
                        frequency_hz: 1_000_000_000.0,
                        io_write_conflict_ratio: 0.5,
                        scope: crate::metrics::irp::spr::SprUncoreScope {
                            die_group_id: None,
                            die_id: None,
                            package_id: 0,
                        },
                        stack: crate::metrics::irp::spr::SprIrpStack::new(10, "m2iosf10"),
                        total_irp_occupancy_entries: 4.0,
                        pcie_inbound_writes_per_second: 5.0,
                        pcie_inbound_write_latency_seconds: 0.000001,
                    }],
                },
            )),
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: emerald_rapids_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_irp_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_irp_pcie_inbound_writes_per_second"));
        assert!(metrics.contains("ocellus_irp_all_hit_m_snoop_responses_per_second"));
        assert!(metrics.contains("stack=\"m2iosf10\""));
    }

    #[test]
    fn renders_sandy_bridge_pcu_metrics() {
        assert_renders_snb_pcu_metrics(
            sandy_bridge_info_metadata(),
            crate::metrics::pcu::PcuMetrics::Snb(snb_pcu_metrics()),
        );
    }

    #[test]
    fn renders_ivy_bridge_pcu_metrics() {
        assert_renders_snb_pcu_metrics(
            ivy_bridge_info_metadata(),
            crate::metrics::pcu::PcuMetrics::Ivb(snb_pcu_metrics()),
        );
    }

    #[test]
    fn renders_broadwell_pcu_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: Some(crate::metrics::pcu::PcuMetrics::Bdx(hsx_pcu_metrics())),
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: haswell_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_core_c_state_average_cores gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_limit_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_transition_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_memory_phase_shedding_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_package_c_state_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_thermal_throttle_ratio gauge"));
        assert!(metrics.contains("reason=\"power\""));
        assert!(metrics.contains("source=\"vr_hot\""));
        assert!(metrics.contains("package=\"0\""));
    }

    #[test]
    fn renders_skylake_pcu_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: Some(crate::metrics::pcu::PcuMetrics::Skx(skx_pcu_metrics())),
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: skylake_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_core_c_state_average_cores gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_limit_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_transition_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_memory_phase_shedding_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_package_c_state_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_thermal_throttle_ratio gauge"));
        assert!(metrics.contains("reason=\"io_p\""));
        assert!(metrics.contains("source=\"vr_hot\""));
        assert!(metrics.contains("die_group=\"unknown\""));
        assert!(metrics.contains("die=\"unknown\""));
        assert!(!metrics.contains("reason=\"os\""));
    }

    #[test]
    fn renders_ice_lake_pcu_metrics() {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: Some(crate::metrics::pcu::PcuMetrics::Icx(
                crate::metrics::pcu::icx::IcxPcuMetrics {
                    clocks: vec![crate::metrics::pcu::PcuClockMetrics {
                        frequency_hz: 1_000_000_000.0,
                        scope: crate::metrics::pcu::PcuScope {
                            die_group_id: None,
                            die_id: None,
                            package_id: 0,
                        },
                    }],
                },
            )),
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: ice_lake_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_hz gauge"));
        assert!(metrics.contains("die_group=\"unknown\""));
        assert!(metrics.contains("die=\"unknown\""));
        assert!(!metrics.contains("ocellus_pcu_core_c_state_average_cores"));
        assert!(!metrics.contains("ocellus_pcu_frequency_limit_ratio"));
    }

    #[test]
    fn renders_sapphire_rapids_pcu_metrics() {
        assert_renders_spr_pcu_metrics(
            sapphire_rapids_info_metadata(),
            crate::metrics::pcu::PcuMetrics::Spr(spr_pcu_metrics()),
        );
    }

    #[test]
    fn renders_emerald_rapids_pcu_metrics() {
        assert_renders_spr_pcu_metrics(
            emerald_rapids_info_metadata(),
            crate::metrics::pcu::PcuMetrics::Emr(spr_pcu_metrics()),
        );
    }

    fn assert_renders_snb_pcu_metrics(
        metadata: crate::metrics::InfoMetadata,
        pcu: crate::metrics::pcu::PcuMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: Some(pcu),
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_core_c_state_average_cores gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_limit_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_package_c_state_ratio gauge"));
        assert!(metrics.contains("reason=\"current\""));
        assert!(metrics.contains("reason=\"perf_p\""));
        assert!(metrics.contains("c_state=\"c6\""));
        assert!(metrics.contains("package=\"0\""));
    }

    fn assert_renders_spr_pcu_metrics(
        metadata: crate::metrics::InfoMetadata,
        pcu: crate::metrics::pcu::PcuMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: Some(pcu),
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_pcu_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_pcu_core_c_state_average_cores gauge"));
        assert!(metrics.contains("c_state=\"c0\""));
        assert!(metrics.contains("c_state=\"c6\""));
        assert!(!metrics.contains("ocellus_pcu_frequency_limit_ratio"));
        assert!(!metrics.contains("ocellus_pcu_package_c_state_ratio"));
        assert!(!metrics.contains("c_state=\"c3\""));
    }

    fn snb_pcu_metrics() -> crate::metrics::pcu::snb::SnbPcuMetrics {
        let scope = crate::metrics::pcu::PcuPackageScope { package_id: 0 };
        crate::metrics::pcu::snb::SnbPcuMetrics {
            clocks: vec![crate::metrics::pcu::PcuClockMetrics {
                frequency_hz: 800_000_000.0,
                scope,
            }],
            core_c_states: vec![crate::metrics::pcu::PcuCoreCStateMetrics {
                average_cores: 2.0,
                c_state: crate::metrics::pcu::PcuCoreCState::C6,
                scope,
            }],
            frequency_limits: vec![
                crate::metrics::pcu::PcuFrequencyLimitMetrics {
                    ratio: 0.25,
                    reason: crate::metrics::pcu::PcuFrequencyLimitReason::Current,
                    scope,
                },
                crate::metrics::pcu::PcuFrequencyLimitMetrics {
                    ratio: 0.5,
                    reason: crate::metrics::pcu::PcuFrequencyLimitReason::PerfP,
                    scope,
                },
            ],
            frequency_transition: Vec::new(),
            memory_phase_shedding: Vec::new(),
            package_c_states: vec![crate::metrics::pcu::PcuPackageCStateMetrics {
                c_state: crate::metrics::pcu::PcuCoreCState::C6,
                ratio: 0.75,
                scope,
            }],
            thermal_throttles: Vec::new(),
        }
    }

    fn hsx_pcu_metrics() -> crate::metrics::pcu::hsx::HsxPcuMetrics {
        let scope = crate::metrics::pcu::PcuPackageScope { package_id: 0 };
        crate::metrics::pcu::hsx::HsxPcuMetrics {
            clocks: vec![crate::metrics::pcu::PcuClockMetrics {
                frequency_hz: 800_000_000.0,
                scope,
            }],
            core_c_states: vec![crate::metrics::pcu::PcuCoreCStateMetrics {
                average_cores: 2.0,
                c_state: crate::metrics::pcu::PcuCoreCState::C0,
                scope,
            }],
            frequency_limits: vec![crate::metrics::pcu::PcuFrequencyLimitMetrics {
                ratio: 0.25,
                reason: crate::metrics::pcu::PcuFrequencyLimitReason::Power,
                scope,
            }],
            frequency_transition: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.1,
                scope,
                source: "frequency_transition",
            }],
            memory_phase_shedding: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.2,
                scope,
                source: "memory_phase_shedding",
            }],
            package_c_states: vec![crate::metrics::pcu::PcuPackageCStateMetrics {
                c_state: crate::metrics::pcu::PcuCoreCState::C6,
                ratio: 0.75,
                scope,
            }],
            thermal_throttles: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.3,
                scope,
                source: crate::metrics::pcu::PcuThermalThrottleSource::VrHot,
            }],
        }
    }

    fn skx_pcu_metrics() -> crate::metrics::pcu::skx::SkxPcuMetrics {
        let scope = crate::metrics::pcu::PcuScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };
        crate::metrics::pcu::skx::SkxPcuMetrics {
            clocks: vec![crate::metrics::pcu::PcuClockMetrics {
                frequency_hz: 1_000_000_000.0,
                scope,
            }],
            core_c_states: vec![crate::metrics::pcu::PcuCoreCStateMetrics {
                average_cores: 2.0,
                c_state: crate::metrics::pcu::PcuCoreCState::C0,
                scope,
            }],
            frequency_limits: vec![crate::metrics::pcu::PcuFrequencyLimitMetrics {
                ratio: 0.25,
                reason: crate::metrics::pcu::PcuFrequencyLimitReason::IoP,
                scope,
            }],
            frequency_transition: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.1,
                scope,
                source: "frequency_transition",
            }],
            memory_phase_shedding: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.2,
                scope,
                source: "memory_phase_shedding",
            }],
            package_c_states: vec![crate::metrics::pcu::PcuPackageCStateMetrics {
                c_state: crate::metrics::pcu::PcuCoreCState::C6,
                ratio: 0.75,
                scope,
            }],
            thermal_throttles: vec![crate::metrics::pcu::PcuCycleRatioMetrics {
                ratio: 0.3,
                scope,
                source: crate::metrics::pcu::PcuThermalThrottleSource::VrHot,
            }],
        }
    }

    fn spr_pcu_metrics() -> crate::metrics::pcu::spr::SprPcuMetrics {
        let scope = crate::metrics::pcu::PcuScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };
        crate::metrics::pcu::spr::SprPcuMetrics {
            clocks: vec![crate::metrics::pcu::PcuClockMetrics {
                frequency_hz: 1_000_000_000.0,
                scope,
            }],
            core_c_states: vec![
                crate::metrics::pcu::PcuCoreCStateMetrics {
                    average_cores: 1.0,
                    c_state: crate::metrics::pcu::PcuCoreCState::C0,
                    scope,
                },
                crate::metrics::pcu::PcuCoreCStateMetrics {
                    average_cores: 2.0,
                    c_state: crate::metrics::pcu::PcuCoreCState::C6,
                    scope,
                },
            ],
        }
    }

    #[test]
    fn renders_sandy_bridge_irp_metrics() {
        assert_renders_snb_irp_metrics(
            sandy_bridge_info_metadata(),
            crate::metrics::irp::IrpMetrics::Snb(snb_irp_metrics()),
        );
    }

    #[test]
    fn renders_ivy_bridge_irp_metrics() {
        assert_renders_snb_irp_metrics(
            ivy_bridge_info_metadata(),
            crate::metrics::irp::IrpMetrics::Ivb(snb_irp_metrics()),
        );
    }

    fn assert_renders_snb_irp_metrics(
        metadata: crate::metrics::InfoMetadata,
        irp: crate::metrics::irp::IrpMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: Some(irp),
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_irp_frequency_hz gauge"));
        assert!(metrics.contains("# TYPE ocellus_irp_io_write_conflict_ratio gauge"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_inbound_reads_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_irp_pcie_inbound_writes_per_second gauge"));
        assert!(metrics.contains("# TYPE ocellus_irp_total_occupancy_entries gauge"));
        assert!(metrics.contains("package=\"0\""));
        assert!(!metrics.contains("ocellus_irp_clflush_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_core_read_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_demand_read_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_faf_occupancy_entries"));
        assert!(!metrics.contains("ocellus_irp_pci_dca_hint_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_pci_itom_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_pcie_inbound_write_latency_seconds"));
        assert!(!metrics.contains("ocellus_irp_pcie_read_current_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_read_for_ownership_bytes_per_second"));
        assert!(!metrics.contains("ocellus_irp_wbmtoi_bytes_per_second"));
    }

    fn snb_irp_metrics() -> crate::metrics::irp::snb::SnbIrpMetrics {
        crate::metrics::irp::snb::SnbIrpMetrics {
            scopes: vec![crate::metrics::irp::snb::SnbIrpScopeMetrics {
                frequency_hz: 1_000_000_000.0,
                io_write_conflict_ratio: 0.25,
                pcie_inbound_reads_per_second: 13.0,
                pcie_inbound_writes_per_second: 12.0,
                scope: crate::metrics::irp::snb::SnbIrpScope { package_id: 0 },
                total_irp_occupancy_entries: 11.0,
            }],
        }
    }

    #[test]
    fn renders_cha_metrics() {
        let scope = crate::metrics::uncore::skx::UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };
        let state = crate::metrics::MetricsState {
            cha: Some(crate::metrics::cha::ChaMetrics::Skx(
                crate::metrics::cha::skx::SkxChaMetrics {
                    evictions: vec![crate::metrics::cha::ChaEvictionMetrics {
                        bandwidth_bytes_per_second: 10.0,
                        latency_seconds: 0.000003,
                        occupancy_entries: 6.0,
                        scope,
                    }],
                    ha_requests: vec![crate::metrics::cha::ChaHaRequestMetrics {
                        local_read_bytes_per_second: 11.0,
                        local_read_ratio: 0.75,
                        local_write_bytes_per_second: 12.0,
                        local_write_ratio: 0.5,
                        remote_read_bytes_per_second: 13.0,
                        remote_write_bytes_per_second: 14.0,
                        scope,
                    }],
                    llc_lookups: vec![crate::metrics::cha::ChaLlcLookupMetrics {
                        bytes_per_second: 1.0,
                        operation: crate::metrics::cha::ChaLookupOperation::Read,
                        scope,
                        state: crate::metrics::cha::ChaCacheState::M,
                    }],
                    llc_victims: vec![crate::metrics::cha::ChaLlcVictimMetrics {
                        per_second: 2.0,
                        scope,
                        state: crate::metrics::cha::ChaCacheState::E,
                    }],
                    no_credits: vec![crate::metrics::cha::ChaNoCreditMetrics {
                        direction: crate::metrics::cha::ChaNoCreditDirection::Read,
                        ratio: 0.25,
                        scope,
                    }],
                    request_queues: vec![crate::metrics::cha::ChaRequestQueueMetrics {
                        occupancy_entries: 7.0,
                        scope,
                        source: crate::metrics::cha::ChaRequestSource::Ia,
                    }],
                    rxc: vec![crate::metrics::cha::ChaRxcMetrics {
                        inserts_per_second: 4.0,
                        latency_seconds: 0.000001,
                        occupancy_entries: 5.0,
                        queue: crate::metrics::cha::ChaRxcQueue::Irq,
                        scope,
                    }],
                    scopes: vec![crate::metrics::cha::ChaScopeMetrics {
                        frequency_hz: 1_000_000_000.0,
                        scope,
                    }],
                    sf_evictions: vec![crate::metrics::cha::ChaSfEvictionMetrics {
                        bytes_per_second: 3.0,
                        scope,
                        state: crate::metrics::cha::ChaCacheState::S,
                    }],
                    transaction_results: vec![crate::metrics::cha::ChaTransactionResultMetrics {
                        bandwidth_bytes_per_second: 15.0,
                        inserts_per_second: 4.0,
                        latency_seconds: 0.000001,
                        occupancy_entries: 5.0,
                        result: crate::metrics::cha::ChaTransactionResult::Miss,
                        scope,
                        transaction: crate::metrics::cha::ChaTransactionLabel::new("ia_drd"),
                    }],
                    transactions: vec![crate::metrics::cha::ChaTransactionMetrics {
                        bandwidth_bytes_per_second: 16.0,
                        hit_rate: 0.9,
                        latency_seconds: 0.0,
                        scope,
                        transaction: crate::metrics::cha::ChaTransactionLabel::new("ia_drd"),
                    }],
                },
            )),
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
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

        assert!(metrics.contains("# TYPE ocellus_cha_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_cha_frequency_hz"));
        assert!(metrics.contains("# TYPE ocellus_cha_eviction_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_cha_eviction_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_cha_ha_request_local_ratio gauge"));
        assert!(metrics.contains("ocellus_cha_ha_request_local_ratio"));
        assert!(metrics.contains("# TYPE ocellus_cha_llc_lookup_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_cha_llc_lookup_bytes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_cha_llc_victims_per_second gauge"));
        assert!(metrics.contains("ocellus_cha_llc_victims_per_second"));
        assert!(metrics.contains("# TYPE ocellus_cha_no_credit_ratio gauge"));
        assert!(metrics.contains("ocellus_cha_no_credit_ratio"));
        assert!(metrics.contains("# TYPE ocellus_cha_request_queue_occupancy_entries gauge"));
        assert!(metrics.contains("ocellus_cha_request_queue_occupancy_entries"));
        assert!(metrics.contains("# TYPE ocellus_cha_rxc_inserts_per_second gauge"));
        assert!(metrics.contains("ocellus_cha_rxc_inserts_per_second"));
        assert!(metrics.contains("# TYPE ocellus_cha_rxc_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_cha_rxc_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_cha_rxc_occupancy_entries gauge"));
        assert!(metrics.contains("ocellus_cha_rxc_occupancy_entries"));
        assert!(metrics.contains("# TYPE ocellus_cha_sf_eviction_bytes_per_second gauge"));
        assert!(metrics.contains("ocellus_cha_sf_eviction_bytes_per_second"));
        assert!(metrics.contains("# TYPE ocellus_cha_transaction_result_latency_seconds gauge"));
        assert!(metrics.contains("ocellus_cha_transaction_result_latency_seconds"));
        assert!(metrics.contains("# TYPE ocellus_cha_transaction_hit_rate gauge"));
        assert!(metrics.contains("ocellus_cha_transaction_hit_rate"));
        assert!(metrics.contains("direction=\"read\""));
        assert!(metrics.contains("locality=\"local\""));
        assert!(metrics.contains("operation=\"read\""));
        assert!(metrics.contains("queue=\"irq\""));
        assert!(metrics.contains("source=\"ia\""));
        assert!(metrics.contains("state=\"m\""));
        assert!(metrics.contains("transaction=\"ia_drd\""));
    }

    #[test]
    fn renders_hsx_cha_metrics() {
        let scope = crate::metrics::uncore::skx::UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };
        let state = crate::metrics::MetricsState {
            cha: Some(crate::metrics::cha::ChaMetrics::Hsx(
                crate::metrics::cha::hsx::HsxChaMetrics {
                    llc_lookups: vec![crate::metrics::cha::ChaLlcLookupMetrics {
                        bytes_per_second: 1.0,
                        operation: crate::metrics::cha::ChaLookupOperation::Any,
                        scope,
                        state: crate::metrics::cha::ChaCacheState::F,
                    }],
                    llc_victims: vec![crate::metrics::cha::ChaLlcVictimMetrics {
                        per_second: 2.0,
                        scope,
                        state: crate::metrics::cha::ChaCacheState::M,
                    }],
                    scopes: vec![crate::metrics::cha::ChaScopeMetrics {
                        frequency_hz: 1_000_000_000.0,
                        scope,
                    }],
                    transaction_results: vec![crate::metrics::cha::ChaTransactionResultMetrics {
                        bandwidth_bytes_per_second: 3.0,
                        inserts_per_second: 4.0,
                        latency_seconds: 0.000001,
                        occupancy_entries: 5.0,
                        result: crate::metrics::cha::ChaTransactionResult::Hit,
                        scope,
                        transaction: crate::metrics::cha::ChaTransactionLabel::new("pcie_rfo"),
                    }],
                    transactions: vec![crate::metrics::cha::ChaTransactionMetrics {
                        bandwidth_bytes_per_second: 6.0,
                        hit_rate: 0.75,
                        latency_seconds: 0.000002,
                        scope,
                        transaction: crate::metrics::cha::ChaTransactionLabel::new("pcie_rfo"),
                    }],
                },
            )),
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: haswell_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_cha_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_cha_llc_lookup_bytes_per_second"));
        assert!(metrics.contains("ocellus_cha_llc_victims_per_second"));
        assert!(metrics.contains("ocellus_cha_transaction_result_inserts_per_second"));
        assert!(metrics.contains("ocellus_cha_transaction_hit_rate"));
        assert!(metrics.contains("operation=\"any\""));
        assert!(metrics.contains("result=\"hit\""));
        assert!(metrics.contains("state=\"f\""));
        assert!(metrics.contains("transaction=\"pcie_rfo\""));
    }

    #[test]
    fn renders_sandy_bridge_cha_metrics() {
        assert_renders_snb_cha_metrics(
            sandy_bridge_info_metadata(),
            crate::metrics::cha::ChaMetrics::Snb(snb_cha_metrics(
                crate::metrics::cha::ChaLookupOperation::Read,
            )),
        );
    }

    #[test]
    fn renders_ivy_bridge_cha_metrics() {
        assert_renders_snb_cha_metrics(
            ivy_bridge_info_metadata(),
            crate::metrics::cha::ChaMetrics::Ivb(snb_cha_metrics(
                crate::metrics::cha::ChaLookupOperation::Any,
            )),
        );
    }

    #[test]
    fn renders_ice_lake_cha_metrics() {
        assert_renders_server_cha_metrics(
            ice_lake_info_metadata(),
            crate::metrics::cha::ChaMetrics::Icx(ice_lake_cha_metrics()),
            "io_pcirdcur",
        );
    }

    #[test]
    fn renders_sapphire_rapids_cha_metrics() {
        assert_renders_server_cha_metrics(
            sapphire_rapids_info_metadata(),
            crate::metrics::cha::ChaMetrics::Spr(sapphire_rapids_cha_metrics()),
            "io_pcirdcur",
        );
    }

    #[test]
    fn renders_emerald_rapids_cha_metrics() {
        assert_renders_server_cha_metrics(
            emerald_rapids_info_metadata(),
            crate::metrics::cha::ChaMetrics::Emr(sapphire_rapids_cha_metrics()),
            "io_pcirdcur",
        );
    }

    fn assert_renders_server_cha_metrics(
        metadata: crate::metrics::InfoMetadata,
        cha: crate::metrics::cha::ChaMetrics,
        transaction: &str,
    ) {
        let state = crate::metrics::MetricsState {
            cha: Some(cha),
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_cha_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_cha_ha_request_bandwidth_bytes_per_second"));
        assert!(metrics.contains("ocellus_cha_ha_request_local_ratio"));
        assert!(metrics.contains("ocellus_cha_transaction_result_inserts_per_second"));
        assert!(metrics.contains("ocellus_cha_transaction_result_occupancy_entries"));
        assert!(metrics.contains("ocellus_cha_transaction_hit_rate"));
        assert!(metrics.contains("locality=\"local\""));
        assert!(metrics.contains("operation=\"read\""));
        assert!(metrics.contains(&format!("transaction=\"{transaction}\"")));
    }

    fn assert_renders_snb_cha_metrics(
        metadata: crate::metrics::InfoMetadata,
        cha: crate::metrics::cha::ChaMetrics,
    ) {
        let state = crate::metrics::MetricsState {
            cha: Some(cha),
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: metadata,
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_cha_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_cha_llc_lookup_bytes_per_second"));
        assert!(metrics.contains("ocellus_cha_llc_victims_per_second"));
        assert!(metrics.contains("ocellus_cha_transaction_result_inserts_per_second"));
        assert!(metrics.contains("ocellus_cha_transaction_hit_rate"));
        assert!(metrics.contains("operation=\""));
        assert!(metrics.contains("result=\"hit\""));
        assert!(metrics.contains("state=\"m\""));
        assert!(metrics.contains("transaction=\"ia_drd\""));
    }

    fn ice_lake_cha_metrics() -> crate::metrics::cha::icx::IcxChaMetrics {
        let metrics = server_cha_metric_fields();

        crate::metrics::cha::icx::IcxChaMetrics {
            ha_requests: metrics.0,
            llc_lookups: metrics.1,
            llc_victims: Vec::new(),
            request_queues: metrics.2,
            scopes: metrics.3,
            sf_evictions: metrics.4,
            transaction_results: metrics.5,
            transactions: metrics.6,
        }
    }

    fn sapphire_rapids_cha_metrics() -> crate::metrics::cha::spr::SprChaMetrics {
        let metrics = server_cha_metric_fields();

        crate::metrics::cha::spr::SprChaMetrics {
            ha_requests: metrics.0,
            llc_victims: Vec::new(),
            request_queues: metrics.2,
            scopes: metrics.3,
            sf_evictions: metrics.4,
            transaction_results: metrics.5,
            transactions: metrics.6,
        }
    }

    fn snb_cha_metrics(
        lookup_operation: crate::metrics::cha::ChaLookupOperation,
    ) -> crate::metrics::cha::snb::SnbChaMetrics {
        let scope = crate::metrics::uncore::skx::UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };

        crate::metrics::cha::snb::SnbChaMetrics {
            llc_lookups: vec![crate::metrics::cha::ChaLlcLookupMetrics {
                bytes_per_second: 1.0,
                operation: lookup_operation,
                scope,
                state: crate::metrics::cha::ChaCacheState::M,
            }],
            llc_victims: vec![crate::metrics::cha::ChaLlcVictimMetrics {
                per_second: 2.0,
                scope,
                state: crate::metrics::cha::ChaCacheState::M,
            }],
            scopes: vec![crate::metrics::cha::ChaScopeMetrics {
                frequency_hz: 1_000_000_000.0,
                scope,
            }],
            transaction_results: vec![crate::metrics::cha::ChaTransactionResultMetrics {
                bandwidth_bytes_per_second: 3.0,
                inserts_per_second: 4.0,
                latency_seconds: 0.000001,
                occupancy_entries: 5.0,
                result: crate::metrics::cha::ChaTransactionResult::Hit,
                scope,
                transaction: crate::metrics::cha::ChaTransactionLabel::new("ia_drd"),
            }],
            transactions: vec![crate::metrics::cha::ChaTransactionMetrics {
                bandwidth_bytes_per_second: 6.0,
                hit_rate: 0.75,
                latency_seconds: 0.000002,
                scope,
                transaction: crate::metrics::cha::ChaTransactionLabel::new("ia_drd"),
            }],
        }
    }

    fn server_cha_metric_fields() -> (
        Vec<crate::metrics::cha::ChaHaRequestMetrics>,
        Vec<crate::metrics::cha::ChaLlcLookupMetrics>,
        Vec<crate::metrics::cha::ChaRequestQueueMetrics>,
        Vec<crate::metrics::cha::ChaScopeMetrics>,
        Vec<crate::metrics::cha::ChaSfEvictionMetrics>,
        Vec<crate::metrics::cha::ChaTransactionResultMetrics>,
        Vec<crate::metrics::cha::ChaTransactionMetrics>,
    ) {
        let scope = crate::metrics::uncore::skx::UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        };

        (
            vec![crate::metrics::cha::ChaHaRequestMetrics {
                local_read_bytes_per_second: 1.0,
                local_read_ratio: 0.5,
                local_write_bytes_per_second: 2.0,
                local_write_ratio: 0.75,
                remote_read_bytes_per_second: 3.0,
                remote_write_bytes_per_second: 4.0,
                scope,
            }],
            vec![crate::metrics::cha::ChaLlcLookupMetrics {
                bytes_per_second: 14.0,
                operation: crate::metrics::cha::ChaLookupOperation::Read,
                scope,
                state: crate::metrics::cha::ChaCacheState::M,
            }],
            vec![crate::metrics::cha::ChaRequestQueueMetrics {
                occupancy_entries: 11.0,
                scope,
                source: crate::metrics::cha::ChaRequestSource::Ia,
            }],
            vec![crate::metrics::cha::ChaScopeMetrics {
                frequency_hz: 1_000_000_000.0,
                scope,
            }],
            vec![crate::metrics::cha::ChaSfEvictionMetrics {
                bytes_per_second: 12.0,
                scope,
                state: crate::metrics::cha::ChaCacheState::M,
            }],
            vec![
                crate::metrics::cha::ChaTransactionResultMetrics {
                    bandwidth_bytes_per_second: 5.0,
                    inserts_per_second: 6.0,
                    latency_seconds: 0.000001,
                    occupancy_entries: 7.0,
                    result: crate::metrics::cha::ChaTransactionResult::Hit,
                    scope,
                    transaction: crate::metrics::cha::ChaTransactionLabel::new("io_pcirdcur"),
                },
                crate::metrics::cha::ChaTransactionResultMetrics {
                    bandwidth_bytes_per_second: 8.0,
                    inserts_per_second: 9.0,
                    latency_seconds: 0.000002,
                    occupancy_entries: 10.0,
                    result: crate::metrics::cha::ChaTransactionResult::Miss,
                    scope,
                    transaction: crate::metrics::cha::ChaTransactionLabel::new("io_pcirdcur"),
                },
            ],
            vec![crate::metrics::cha::ChaTransactionMetrics {
                bandwidth_bytes_per_second: 13.0,
                hit_rate: 0.4,
                latency_seconds: 0.0000015,
                scope,
                transaction: crate::metrics::cha::ChaTransactionLabel::new("io_pcirdcur"),
            }],
        )
    }

    #[test]
    fn renders_hsx_ha_and_imc_metrics() {
        let scope = crate::metrics::uncore::hsx::HsxUncoreScope { package_id: 0 };
        let state = crate::metrics::MetricsState {
            cha: None,
            iio: None,
            imc: Some(crate::metrics::imc::ImcMetrics::Hsx(
                crate::metrics::imc::hsx::HsxImcMetrics {
                    scopes: vec![crate::metrics::imc::hsx::HsxImcMetricsByScope {
                        activate_commands_per_second: 5.0,
                        frequency_hz: 1_000_000_000.0,
                        ha_local_read_bytes_per_second: 1.0,
                        ha_local_read_ratio: 0.75,
                        ha_local_write_bytes_per_second: 2.0,
                        ha_local_write_ratio: 0.5,
                        ha_remote_read_bytes_per_second: 3.0,
                        ha_remote_write_bytes_per_second: 4.0,
                        page_miss_precharge_commands_per_second: 6.0,
                        read_cas_commands_per_second: 7.0,
                        read_bytes_per_second: 8.0,
                        rpq_non_empty_ratio: 0.2,
                        scope,
                        write_cas_commands_per_second: 9.0,
                        write_bytes_per_second: 10.0,
                        wpq_full_ratio: 0.5,
                        wpq_non_empty_ratio: 0.6,
                    }],
                },
            )),
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
        };
        let sampler = SamplerReader::new_for_test(
            crate::runtime::sampler::SamplerMetadata {
                measure_interval: std::time::Duration::from_millis(1),
                info: haswell_info_metadata(),
            },
            state.clone(),
        );
        let exporter = PrometheusExporter::new(sampler);
        exporter.update_state(state);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let metrics = runtime.block_on(exporter.render_metrics()).unwrap();

        assert!(metrics.contains("# TYPE ocellus_haswell_imc_frequency_hz gauge"));
        assert!(metrics.contains("ocellus_haswell_imc_read_bytes_per_second"));
        assert!(metrics.contains("ocellus_haswell_imc_rpq_non_empty_ratio"));
        assert!(metrics.contains("ocellus_haswell_imc_wpq_full_ratio"));
        assert!(metrics.contains("ocellus_haswell_imc_wpq_non_empty_ratio"));
        assert!(metrics.contains("ocellus_haswell_ha_local_read_bytes_per_second"));
        assert!(metrics.contains("ocellus_haswell_ha_local_read_ratio"));
        assert!(metrics.contains("package=\"0\""));
    }
}
