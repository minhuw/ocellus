use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

mod arch;
mod metal;
mod metrics;
mod runtime;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 8080));
const DEFAULT_LOCAL_OUTPUT: &str = "ocellus-metrics.jsonl";
const DEFAULT_MEASURE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ChaMultiplexModeConfig {
    Temporal,
    Spatial,
}

impl ChaMultiplexModeConfig {
    fn to_metric_mode(self, partitions: usize) -> metrics::cha::ChaMultiplexMode {
        match self {
            Self::Temporal => metrics::cha::ChaMultiplexMode::Temporal,
            Self::Spatial => metrics::cha::ChaMultiplexMode::spatial(partitions),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ChaRequestLocalityConfig {
    All,
    Local,
}

impl ChaRequestLocalityConfig {
    fn to_metric_locality(self) -> metrics::cha::ChaRequestLocality {
        match self {
            Self::All => metrics::cha::ChaRequestLocality::All,
            Self::Local => metrics::cha::ChaRequestLocality::Local,
        }
    }
}

#[derive(Clone, Debug, Parser)]
#[command(version, about, long_about = None)]
struct Config {
    #[arg(
        long,
        help = "Run as Prometheus pull endpoint instead of local JSON writer"
    )]
    daemon: bool,

    #[arg(long, default_value_t = DEFAULT_LISTEN, help = "Daemon listen address")]
    listen: SocketAddr,

    #[arg(
        long,
        default_value = DEFAULT_LOCAL_OUTPUT,
        help = "Local JSON output path"
    )]
    output: PathBuf,

    #[arg(
        long,
        default_value_t = DEFAULT_MEASURE_INTERVAL.as_millis() as u64,
        value_parser = clap::value_parser!(u64).range(1..),
        help = "Measurement interval in milliseconds"
    )]
    measure_interval_ms: u64,

    #[arg(
        long,
        value_enum,
        default_value_t = ChaMultiplexModeConfig::Spatial,
        help = "CHA multiplexing mode"
    )]
    cha_multiplex_mode: ChaMultiplexModeConfig,

    #[arg(
        long,
        default_value_t = 4,
        value_parser = clap::value_parser!(u8).range(1..=4),
        help = "CHA spatial multiplex partitions"
    )]
    cha_spatial_partitions: u8,

    #[arg(
        long,
        value_enum,
        default_value_t = ChaRequestLocalityConfig::All,
        help = "CHA requester locality to count (Skylake-SP only)"
    )]
    cha_request_locality: ChaRequestLocalityConfig,
}

impl Config {
    fn measure_interval(&self) -> Duration {
        Duration::from_millis(self.measure_interval_ms)
    }

    fn runtime_mode(&self) -> runtime::RuntimeMode {
        if self.daemon {
            runtime::RuntimeMode::Daemon {
                listen: self.listen,
            }
        } else {
            runtime::RuntimeMode::Local {
                output: self.output.clone(),
            }
        }
    }

    fn cha_multiplex_mode(&self) -> metrics::cha::ChaMultiplexMode {
        self.cha_multiplex_mode
            .to_metric_mode(usize::from(self.cha_spatial_partitions))
    }

    fn cha_request_locality(&self) -> metrics::cha::ChaRequestLocality {
        self.cha_request_locality.to_metric_locality()
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    run(Config::parse()).await
}

async fn run(config: Config) -> Result<(), String> {
    let architecture = arch::Architecture::detect()?;
    metrics::tsc::preflight_permissions(&architecture)?;

    let measure_interval = config.measure_interval();
    let sampler = runtime::sampler::spawn(
        measure_interval,
        architecture,
        config.cha_multiplex_mode(),
        config.cha_request_locality(),
    );

    runtime::run(config.runtime_mode(), sampler).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_spatial_cha_multiplexing() {
        let config = Config::parse_from(["ocellus"]);

        assert_eq!(
            config.cha_multiplex_mode(),
            metrics::cha::ChaMultiplexMode::spatial(4)
        );
    }

    #[test]
    fn defaults_to_all_cha_request_locality() {
        let config = Config::parse_from(["ocellus"]);

        assert_eq!(
            config.cha_request_locality(),
            metrics::cha::ChaRequestLocality::All
        );
    }

    #[test]
    fn parses_local_cha_request_locality() {
        let config = Config::parse_from(["ocellus", "--cha-request-locality", "local"]);

        assert_eq!(
            config.cha_request_locality(),
            metrics::cha::ChaRequestLocality::Local
        );
    }
}
