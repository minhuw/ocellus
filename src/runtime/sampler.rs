use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::metrics::tsc::{TscCollector, TscTask};
use crate::metrics::{MetricEvent, MetricsState, ProcessorMetadata};

const EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct SamplerMetadata {
    pub measure_interval: Duration,
    pub processor: ProcessorMetadata,
}

#[derive(Clone, Debug)]
pub struct SamplerReader {
    metadata: SamplerMetadata,
    latest: Arc<RwLock<MetricsState>>,
}

impl SamplerReader {
    pub async fn latest_state(&self) -> MetricsState {
        self.latest.read().await.clone()
    }

    pub fn metadata(&self) -> SamplerMetadata {
        self.metadata
    }

    #[cfg(test)]
    pub fn new_for_test(metadata: SamplerMetadata, latest_state: MetricsState) -> Self {
        Self::from_parts(metadata, Arc::new(RwLock::new(latest_state)))
    }

    fn from_parts(metadata: SamplerMetadata, latest: Arc<RwLock<MetricsState>>) -> Self {
        Self { metadata, latest }
    }
}

#[derive(Debug)]
pub struct Sampler {
    reader: SamplerReader,
    task: JoinHandle<Result<(), String>>,
}

impl Sampler {
    pub fn reader(&self) -> SamplerReader {
        self.reader.clone()
    }

    pub async fn wait(self) -> Result<(), String> {
        match self.task.await {
            Ok(result) => result,
            Err(error) => Err(format!("sampler task failed: {error}")),
        }
    }
}

pub fn spawn(measure_interval: Duration) -> Sampler {
    let tsc = TscCollector::new();
    let metadata = SamplerMetadata {
        measure_interval,
        processor: ProcessorMetadata {
            invariant_tsc_supported: tsc.invariant_tsc_supported(),
        },
    };
    let latest = Arc::new(RwLock::new(MetricsState::default()));
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let task_latest = latest.clone();
    spawn_collector(
        "tsc",
        TscTask::new(tsc, measure_interval, event_tx.clone()).run(),
        event_tx,
    );
    let task = tokio::spawn(aggregate_events(event_rx, task_latest));

    Sampler {
        reader: SamplerReader::from_parts(metadata, latest),
        task,
    }
}

fn spawn_collector(
    name: &'static str,
    collector: impl Future<Output = ()> + Send + 'static,
    events: mpsc::Sender<MetricEvent>,
) {
    tokio::spawn(async move {
        let result = tokio::spawn(collector).await;
        let error = match result {
            Ok(()) => format!("{name} collector stopped unexpectedly"),
            Err(error) => format!("{name} collector task failed: {error}"),
        };
        let _ = events.send(MetricEvent::Failure(error)).await;
    });
}

async fn aggregate_events(
    mut events: mpsc::Receiver<MetricEvent>,
    latest: Arc<RwLock<MetricsState>>,
) -> Result<(), String> {
    let mut state = MetricsState::default();

    while let Some(event) = events.recv().await {
        match event {
            MetricEvent::Failure(error) => return Err(error),
            MetricEvent::Update(update) => {
                state.apply(update);
                *latest.write().await = state.clone();
            }
        }
    }

    Err("sampler event channel closed".to_string())
}
