use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::arch::Architecture;
use crate::metrics::rapl::{RaplCollector, RaplTask};
use crate::metrics::tsc::{TscCollector, TscTask};
use crate::metrics::{MetricEvent, MetricUpdate, MetricsState, ProcessorMetadata};

const EVENT_CHANNEL_CAPACITY: usize = 64;
const UPDATE_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct SamplerMetadata {
    pub measure_interval: Duration,
    pub processor: ProcessorMetadata,
}

#[derive(Clone, Debug)]
pub struct SamplerReader {
    metadata: SamplerMetadata,
    latest: Arc<RwLock<MetricsState>>,
    updates: broadcast::Sender<MetricUpdate>,
}

impl SamplerReader {
    pub async fn latest_state(&self) -> MetricsState {
        self.latest.read().await.clone()
    }

    pub fn metadata(&self) -> SamplerMetadata {
        self.metadata.clone()
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<MetricUpdate> {
        self.updates.subscribe()
    }

    #[cfg(test)]
    pub fn new_for_test(metadata: SamplerMetadata, latest_state: MetricsState) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);

        Self::from_parts(metadata, Arc::new(RwLock::new(latest_state)), updates)
    }

    fn from_parts(
        metadata: SamplerMetadata,
        latest: Arc<RwLock<MetricsState>>,
        updates: broadcast::Sender<MetricUpdate>,
    ) -> Self {
        Self {
            metadata,
            latest,
            updates,
        }
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

pub fn spawn(measure_interval: Duration, architecture: Architecture) -> Sampler {
    let metadata = sampler_metadata(measure_interval, &architecture);
    let latest = Arc::new(RwLock::new(MetricsState::default()));
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (update_tx, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);

    spawn_rapl_collector(&architecture, measure_interval, &event_tx);
    spawn_collector(
        "tsc",
        TscTask::new(TscCollector::new(), measure_interval, event_tx.clone()).run(),
        event_tx,
    );

    let task = tokio::spawn(aggregate_events(
        event_rx,
        latest.clone(),
        update_tx.clone(),
    ));

    Sampler {
        reader: SamplerReader::from_parts(metadata, latest, update_tx),
        task,
    }
}

fn sampler_metadata(measure_interval: Duration, architecture: &Architecture) -> SamplerMetadata {
    SamplerMetadata {
        measure_interval,
        processor: ProcessorMetadata {
            brand: architecture.brand.clone(),
            family: architecture.family,
            invariant_tsc_supported: architecture.features.invariant_tsc,
            model: architecture.model,
            package_rapl_supported: architecture.features.package_rapl,
            vendor: architecture.vendor.clone(),
        },
    }
}

fn spawn_rapl_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match RaplCollector::new(architecture) {
        Ok(rapl) => {
            eprintln!("ocellus: starting RAPL collector");
            spawn_collector(
                "rapl",
                RaplTask::new(rapl, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping RAPL collector: {error}");
        }
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
    updates: broadcast::Sender<MetricUpdate>,
) -> Result<(), String> {
    let mut state = MetricsState::default();

    while let Some(event) = events.recv().await {
        match event {
            MetricEvent::Failure(error) => return Err(error),
            MetricEvent::Update(update) => {
                state.apply(update.clone());
                *latest.write().await = state.clone();
                let _ = updates.send(update);
            }
        }
    }

    Err("sampler event channel closed".to_string())
}
