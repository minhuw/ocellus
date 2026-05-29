use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

use crate::metrics::MetricsState;
use crate::runtime::sampler::SamplerReader;

const SCHEMA_VERSION: u32 = 12;

#[derive(Debug, Serialize)]
struct LocalMetadata {
    schema_version: u32,
    ocellus_version: &'static str,
    measure_interval_ms: u64,
    started_at_unix_seconds: f64,
    invariant_tsc_supported: bool,
}

#[derive(Debug, Serialize)]
struct LocalSample {
    timestamp_unix_seconds: f64,
    #[serde(flatten)]
    state: MetricsState,
}

#[derive(Debug, Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
enum LocalRecord {
    Metadata(LocalMetadata),
    Sample(Box<LocalSample>),
}

impl LocalRecord {
    fn metadata(measure_interval: Duration, invariant_tsc_supported: bool) -> Self {
        Self::Metadata(LocalMetadata {
            schema_version: SCHEMA_VERSION,
            ocellus_version: env!("CARGO_PKG_VERSION"),
            measure_interval_ms: measure_interval.as_millis() as u64,
            started_at_unix_seconds: unix_time_seconds(SystemTime::now()),
            invariant_tsc_supported,
        })
    }

    fn sample(state: MetricsState) -> Self {
        Self::Sample(Box::new(LocalSample {
            timestamp_unix_seconds: unix_time_seconds(SystemTime::now()),
            state,
        }))
    }
}

fn unix_time_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_secs_f64()
}

pub async fn run(
    output_path: PathBuf,
    sampler: SamplerReader,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let metadata = sampler.metadata();
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_path)
        .await
        .map_err(|error| format!("failed to open {}: {error}", output_path.display()))?;
    eprintln!(
        "ocellus {}: appending local JSONL metrics to {} every {:.3}s",
        env!("CARGO_PKG_VERSION"),
        output_path.display(),
        metadata.measure_interval.as_secs_f64()
    );

    write_jsonl_record(
        &mut output,
        &LocalRecord::metadata(
            metadata.measure_interval,
            metadata.info.processor.invariant_tsc_supported,
        ),
    )
    .await?;

    let mut interval = tokio::time::interval(metadata.measure_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                write_jsonl_record(
                    &mut output,
                    &LocalRecord::sample(sampler.latest_state().await),
                )
                .await?;
            }
            _ = &mut shutdown => {
                output
                    .flush()
                    .await
                    .map_err(|error| format!("failed to flush JSONL output during shutdown: {error}"))?;
                return Ok(());
            }
        }
    }
}

async fn write_jsonl_record(output: &mut File, record: &LocalRecord) -> Result<(), String> {
    let line = serde_json::to_string(record)
        .map_err(|error| format!("failed to encode JSONL record: {error}"))?;

    output
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("failed to write JSONL record: {error}"))?;
    output
        .write_all(b"\n")
        .await
        .map_err(|error| format!("failed to write JSONL newline: {error}"))?;
    output
        .flush()
        .await
        .map_err(|error| format!("failed to flush JSONL output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_metadata_record() {
        let record = LocalRecord::metadata(Duration::from_millis(250), true);
        let json = serde_json::to_value(&record).unwrap();
        let object = json.as_object().unwrap();

        assert_eq!(object["record_type"], "metadata");
        assert_eq!(object["schema_version"], SCHEMA_VERSION);
        assert_eq!(object["measure_interval_ms"], 250);
        assert_eq!(object["invariant_tsc_supported"], true);
    }

    #[test]
    fn encodes_sample_record() {
        let state = MetricsState {
            cha: None,
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
        let record = LocalRecord::sample(state);
        let json = serde_json::to_value(&record).unwrap();
        let object = json.as_object().unwrap();

        assert_eq!(object["record_type"], "sample");
        assert!(object.contains_key("timestamp_unix_seconds"));
        assert!(object.contains_key("version"));
        assert!(!object.contains_key("invariant_tsc_supported"));
    }

    #[test]
    fn encodes_sandy_and_ivy_bridge_cha_architectures() {
        for (cha, expected) in [
            (
                crate::metrics::cha::ChaMetrics::Snb(empty_snb_cha_metrics()),
                "snb",
            ),
            (
                crate::metrics::cha::ChaMetrics::Ivb(empty_snb_cha_metrics()),
                "ivb",
            ),
        ] {
            let state = MetricsState {
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
            let record = LocalRecord::sample(state);
            let json = serde_json::to_value(&record).unwrap();

            assert_eq!(json["cha"]["architecture"], expected);
        }
    }

    #[test]
    fn encodes_sandy_and_ivy_bridge_imc_architectures() {
        for (imc, expected) in [
            (
                crate::metrics::imc::ImcMetrics::Snb(empty_snb_imc_metrics()),
                "snb",
            ),
            (
                crate::metrics::imc::ImcMetrics::Ivb(empty_snb_imc_metrics()),
                "ivb",
            ),
        ] {
            let state = MetricsState {
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
            let record = LocalRecord::sample(state);
            let json = serde_json::to_value(&record).unwrap();

            assert_eq!(json["imc"]["architecture"], expected);
        }
    }

    #[test]
    fn encodes_sandy_and_ivy_bridge_irp_architectures() {
        for (irp, expected) in [
            (
                crate::metrics::irp::IrpMetrics::Snb(empty_snb_irp_metrics()),
                "snb",
            ),
            (
                crate::metrics::irp::IrpMetrics::Ivb(empty_snb_irp_metrics()),
                "ivb",
            ),
        ] {
            let state = MetricsState {
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
            let record = LocalRecord::sample(state);
            let json = serde_json::to_value(&record).unwrap();

            assert_eq!(json["irp"]["architecture"], expected);
        }
    }

    #[test]
    fn encodes_pcu_architectures() {
        for (pcu, expected) in [
            (
                crate::metrics::pcu::PcuMetrics::Snb(empty_snb_pcu_metrics()),
                "snb",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Ivb(empty_snb_pcu_metrics()),
                "ivb",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Hsx(empty_hsx_pcu_metrics()),
                "hsx",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Bdx(empty_hsx_pcu_metrics()),
                "bdx",
            ),
            (
                crate::metrics::pcu::PcuMetrics::BdxDe(empty_hsx_pcu_metrics()),
                "bdx_de",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Icx(empty_icx_pcu_metrics()),
                "icx",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Skx(empty_skx_pcu_metrics()),
                "skx",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Spr(empty_spr_pcu_metrics()),
                "spr",
            ),
            (
                crate::metrics::pcu::PcuMetrics::Emr(empty_spr_pcu_metrics()),
                "emr",
            ),
        ] {
            let state = MetricsState {
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
            let record = LocalRecord::sample(state);
            let json = serde_json::to_value(&record).unwrap();

            assert_eq!(json["pcu"]["architecture"], expected);
        }
    }

    #[test]
    fn encodes_interconnect_architectures() {
        for (interconnect, expected) in [
            (
                crate::metrics::interconnect::InterconnectMetrics::Snb(
                    empty_snb_interconnect_metrics(),
                ),
                "snb",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Ivb(
                    empty_snb_interconnect_metrics(),
                ),
                "ivb",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Hsx(
                    empty_hsx_interconnect_metrics(),
                ),
                "hsx",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Bdx(
                    empty_hsx_interconnect_metrics(),
                ),
                "bdx",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Skx(
                    empty_skx_interconnect_metrics(),
                ),
                "skx",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Icx(
                    empty_icx_interconnect_metrics(),
                ),
                "icx",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Spr(
                    empty_spr_interconnect_metrics(),
                ),
                "spr",
            ),
            (
                crate::metrics::interconnect::InterconnectMetrics::Emr(
                    empty_spr_interconnect_metrics(),
                ),
                "emr",
            ),
        ] {
            let state = MetricsState {
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
            let record = LocalRecord::sample(state);
            let json = serde_json::to_value(&record).unwrap();

            assert_eq!(json["interconnect"]["architecture"], expected);
        }
    }

    #[test]
    fn encodes_rapl_domain_sample() {
        let state = MetricsState {
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            pcu: None,
            rapl: Some(crate::metrics::rapl::RaplMetrics {
                domains: vec![crate::metrics::rapl::RaplDomainMetrics {
                    domain: crate::metrics::rapl::RaplDomainKind::Dram,
                    energy_joules_total: 10.0,
                    power_watts: 5.0,
                    scope: crate::metrics::rapl::RaplScope {
                        die_group_id: 0,
                        die_id: 0,
                        package_id: 0,
                    },
                }],
            }),
            rdt: None,
            tsc: None,
        };
        let record = LocalRecord::sample(state);
        let json = serde_json::to_value(&record).unwrap();

        assert_eq!(json["rapl"]["domains"][0]["domain"], "dram");
        assert_eq!(json["rapl"]["domains"][0]["die_group_id"], 0);
        assert_eq!(json["rapl"]["domains"][0]["die_id"], 0);
        assert_eq!(json["rapl"]["domains"][0]["energy_joules_total"], 10.0);
        assert_eq!(json["rapl"]["domains"][0]["package_id"], 0);
        assert_eq!(json["rapl"]["domains"][0]["power_watts"], 5.0);
    }

    fn empty_snb_cha_metrics() -> crate::metrics::cha::snb::SnbChaMetrics {
        crate::metrics::cha::snb::SnbChaMetrics {
            llc_lookups: Vec::new(),
            llc_victims: Vec::new(),
            scopes: Vec::new(),
            transaction_results: Vec::new(),
            transactions: Vec::new(),
        }
    }

    fn empty_snb_imc_metrics() -> crate::metrics::imc::snb::SnbImcMetrics {
        crate::metrics::imc::snb::SnbImcMetrics { scopes: Vec::new() }
    }

    fn empty_snb_irp_metrics() -> crate::metrics::irp::snb::SnbIrpMetrics {
        crate::metrics::irp::snb::SnbIrpMetrics { scopes: Vec::new() }
    }

    fn empty_hsx_pcu_metrics() -> crate::metrics::pcu::hsx::HsxPcuMetrics {
        crate::metrics::pcu::hsx::HsxPcuMetrics {
            clocks: Vec::new(),
            core_c_states: Vec::new(),
            frequency_limits: Vec::new(),
            frequency_transition: Vec::new(),
            memory_phase_shedding: Vec::new(),
            package_c_states: Vec::new(),
            thermal_throttles: Vec::new(),
        }
    }

    fn empty_icx_pcu_metrics() -> crate::metrics::pcu::icx::IcxPcuMetrics {
        crate::metrics::pcu::icx::IcxPcuMetrics { clocks: Vec::new() }
    }

    fn empty_skx_pcu_metrics() -> crate::metrics::pcu::skx::SkxPcuMetrics {
        crate::metrics::pcu::skx::SkxPcuMetrics {
            clocks: Vec::new(),
            core_c_states: Vec::new(),
            frequency_limits: Vec::new(),
            frequency_transition: Vec::new(),
            memory_phase_shedding: Vec::new(),
            package_c_states: Vec::new(),
            thermal_throttles: Vec::new(),
        }
    }

    fn empty_snb_pcu_metrics() -> crate::metrics::pcu::snb::SnbPcuMetrics {
        crate::metrics::pcu::snb::SnbPcuMetrics {
            clocks: Vec::new(),
            core_c_states: Vec::new(),
            frequency_limits: Vec::new(),
            frequency_transition: Vec::new(),
            memory_phase_shedding: Vec::new(),
            package_c_states: Vec::new(),
            thermal_throttles: Vec::new(),
        }
    }

    fn empty_spr_pcu_metrics() -> crate::metrics::pcu::spr::SprPcuMetrics {
        crate::metrics::pcu::spr::SprPcuMetrics {
            clocks: Vec::new(),
            core_c_states: Vec::new(),
        }
    }

    fn empty_hsx_interconnect_metrics() -> crate::metrics::interconnect::hsx::HsxInterconnectMetrics
    {
        crate::metrics::interconnect::hsx::HsxInterconnectMetrics {
            links: Vec::new(),
            power_states: Vec::new(),
            queues: Vec::new(),
            traffic: Vec::new(),
        }
    }

    fn empty_icx_interconnect_metrics() -> crate::metrics::interconnect::icx::IcxInterconnectMetrics
    {
        crate::metrics::interconnect::icx::IcxInterconnectMetrics {
            links: Vec::new(),
            power_states: Vec::new(),
            traffic: Vec::new(),
        }
    }

    fn empty_skx_interconnect_metrics() -> crate::metrics::interconnect::skx::SkxInterconnectMetrics
    {
        crate::metrics::interconnect::skx::SkxInterconnectMetrics {
            links: Vec::new(),
            power_states: Vec::new(),
            traffic: Vec::new(),
        }
    }

    fn empty_snb_interconnect_metrics() -> crate::metrics::interconnect::snb::SnbInterconnectMetrics
    {
        crate::metrics::interconnect::snb::SnbInterconnectMetrics {
            links: Vec::new(),
            power_states: Vec::new(),
            queues: Vec::new(),
            traffic: Vec::new(),
        }
    }

    fn empty_spr_interconnect_metrics() -> crate::metrics::interconnect::spr::SprInterconnectMetrics
    {
        crate::metrics::interconnect::spr::SprInterconnectMetrics {
            links: Vec::new(),
            power_states: Vec::new(),
            traffic: Vec::new(),
        }
    }
}
