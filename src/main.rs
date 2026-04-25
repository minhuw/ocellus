use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

mod metal;
mod metrics;
mod runtime;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 8080));
const DEFAULT_LOCAL_OUTPUT: &str = "ocellus-metrics.jsonl";
const DEFAULT_MEASURE_INTERVAL: Duration = Duration::from_secs(1);

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
}

#[tokio::main]
async fn main() -> Result<(), String> {
    run(Config::parse()).await
}

async fn run(config: Config) -> Result<(), String> {
    metrics::tsc::preflight_permissions()?;

    let measure_interval = config.measure_interval();
    let sampler = runtime::sampler::spawn(measure_interval);

    runtime::run(config.runtime_mode(), sampler).await
}
