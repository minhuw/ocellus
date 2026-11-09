use std::net::SocketAddr;
use std::path::PathBuf;

use sampler::Sampler;
use tokio::signal::unix::{SignalKind, signal};
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
        result = shutdown_signal() => {
            let signal = result?;
            eprintln!("ocellus: received {signal}, shutting down");
            let _ = shutdown_tx.send(());
            runtime.await.map_err(|error| format!("runtime task failed: {error}"))?
        }
    }
}

/// Waits for SIGINT or SIGTERM. Returning from `run` drops the collectors,
/// which releases what they hold, such as Ocellus resctrl monitor groups;
/// systemd stops services with SIGTERM.
async fn shutdown_signal() -> Result<&'static str, String> {
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|error| format!("failed to listen for SIGTERM: {error}"))?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.map_err(|error| format!("failed to listen for SIGINT: {error}"))?;
            Ok("SIGINT")
        }
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_signal_returns_on_sigterm() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let signal = runtime.block_on(async {
            // An unhandled SIGTERM would kill the test process, so install a
            // listener before sending any.
            let _listener = signal(SignalKind::terminate()).unwrap();
            let shutdown = tokio::spawn(shutdown_signal());
            // The spawned listener only sees signals sent after it starts.
            for _ in 0..500 {
                if shutdown.is_finished() {
                    break;
                }
                let status = std::process::Command::new("kill")
                    .args(["-TERM", &std::process::id().to_string()])
                    .status()
                    .unwrap();
                assert!(status.success());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
                .await
                .expect("shutdown_signal ignored SIGTERM")
                .unwrap()
        });

        assert_eq!(signal, Ok("SIGTERM"));
    }
}
