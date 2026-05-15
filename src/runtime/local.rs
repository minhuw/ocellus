use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

use crate::metrics::MetricsState;
use crate::runtime::sampler::SamplerReader;

const SCHEMA_VERSION: u32 = 3;

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
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
            rapl: None,
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
    fn encodes_rapl_domain_sample() {
        let state = MetricsState {
            cha: None,
            iio: None,
            imc: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            irp: None,
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
}
