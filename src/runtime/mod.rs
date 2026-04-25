use std::net::SocketAddr;
use std::path::PathBuf;

use sampler::Sampler;
use tokio::sync::oneshot;

pub mod local;
pub mod prometheus;
pub mod sampler;

#[derive(Clone, Debug)]
pub enum RuntimeMode {
    Daemon { listen: SocketAddr },
    Local { output: PathBuf },
}

pub async fn run(mode: RuntimeMode, sampler: Sampler) -> Result<(), String> {
    let reader = sampler.reader();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut runtime = match mode {
        RuntimeMode::Daemon { listen } => {
            tokio::spawn(prometheus::run(listen, reader, shutdown_rx))
        }
        RuntimeMode::Local { output } => tokio::spawn(local::run(output, reader, shutdown_rx)),
    };

    tokio::select! {
        result = &mut runtime => result.map_err(|error| format!("runtime task failed: {error}"))?,
        result = sampler.wait() => result.map_err(|error| format!("sampler failed: {error}")),
        result = tokio::signal::ctrl_c() => {
            result.map_err(|error| format!("failed to listen for Ctrl-C: {error}"))?;
            eprintln!("ocellus: received shutdown signal");
            let _ = shutdown_tx.send(());
            runtime.await.map_err(|error| format!("runtime task failed: {error}"))?
        }
    }
}
