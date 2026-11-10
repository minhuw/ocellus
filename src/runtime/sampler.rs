use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::arch::Architecture;
use crate::metrics::cha::{ChaCollector, ChaMultiplexMode, ChaRequestLocality, ChaTask};
use crate::metrics::iio::{IioCollector, IioTask};
use crate::metrics::imc::{ImcCollector, ImcTask};
use crate::metrics::interconnect::{InterconnectCollector, InterconnectTask};
use crate::metrics::irp::{IrpCollector, IrpTask};
use crate::metrics::pcu::{PcuCollector, PcuTask};
use crate::metrics::rapl::{RaplCollector, RaplTask};
use crate::metrics::rdt::{RdtCollector, RdtTask};
use crate::metrics::tsc::{TscCollector, TscTask};
use crate::metrics::{
    CollectorMetadata, EventProfile, InfoMetadata, MetricEvent, MetricsState, SampledUpdate,
};

const EVENT_CHANNEL_CAPACITY: usize = 64;
const UPDATE_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct SamplerMetadata {
    pub measure_interval: Duration,
    pub info: InfoMetadata,
}

#[derive(Clone, Debug)]
pub struct SamplerReader {
    metadata: SamplerMetadata,
    latest: Arc<RwLock<MetricsState>>,
    updates: broadcast::Sender<SampledUpdate>,
}

impl SamplerReader {
    pub async fn latest_state(&self) -> MetricsState {
        self.latest.read().await.clone()
    }

    pub fn metadata(&self) -> SamplerMetadata {
        self.metadata.clone()
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<SampledUpdate> {
        self.updates.subscribe()
    }

    #[cfg(test)]
    pub fn new_for_test(metadata: SamplerMetadata, latest_state: MetricsState) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);

        Self::from_parts(metadata, Arc::new(RwLock::new(latest_state)), updates)
    }

    #[cfg(test)]
    pub fn send_update_for_test(&self, update: SampledUpdate) {
        let _ = self.updates.send(update);
    }

    fn from_parts(
        metadata: SamplerMetadata,
        latest: Arc<RwLock<MetricsState>>,
        updates: broadcast::Sender<SampledUpdate>,
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

#[derive(Clone, Copy, Debug)]
pub struct SamplerConfig {
    pub measure_interval: Duration,
    pub event_profile: EventProfile,
    pub cha_multiplex_mode: ChaMultiplexMode,
    pub cha_request_locality: ChaRequestLocality,
}

pub fn spawn(config: SamplerConfig, architecture: Architecture) -> Sampler {
    let measure_interval = config.measure_interval;
    let metadata = sampler_metadata(config, &architecture);
    let latest = Arc::new(RwLock::new(MetricsState::default()));
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (update_tx, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);

    spawn_cha_collector(&architecture, config, &event_tx);
    spawn_rapl_collector(&architecture, measure_interval, &event_tx);
    spawn_imc_collector(&architecture, config, &event_tx);
    spawn_iio_collector(&architecture, measure_interval, &event_tx);
    spawn_irp_collector(&architecture, measure_interval, &event_tx);
    spawn_pcu_collector(&architecture, measure_interval, &event_tx);
    spawn_interconnect_collector(&architecture, measure_interval, &event_tx);
    spawn_rdt_collector(&architecture, measure_interval, &event_tx);
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

fn sampler_metadata(config: SamplerConfig, architecture: &Architecture) -> SamplerMetadata {
    SamplerMetadata {
        measure_interval: config.measure_interval,
        info: InfoMetadata {
            collectors: CollectorMetadata {
                event_profile: config.event_profile,
                cha_group_count: ChaCollector::event_group_count(
                    architecture,
                    config.event_profile,
                ),
                cha_multiplex_mode: config.cha_multiplex_mode,
                cha_request_locality: ChaCollector::effective_request_locality(
                    architecture,
                    config.cha_request_locality,
                ),
                cha_supported: ChaCollector::is_supported(architecture),
                iio_supported: IioCollector::is_supported(architecture),
                imc_supported: ImcCollector::is_supported(architecture),
                interconnect_supported: InterconnectCollector::is_supported(architecture),
                irp_supported: IrpCollector::is_supported(architecture),
                pcu_supported: PcuCollector::is_supported(architecture),
                rdt_supported: RdtCollector::is_supported(architecture),
            },
            processor: crate::metrics::ProcessorMetadata {
                brand: architecture.brand.clone(),
                family: architecture.family,
                invariant_tsc_supported: architecture.features.invariant_tsc,
                model: architecture.model,
                package_rapl_supported: architecture.features.package_rapl,
                vendor: architecture.vendor.clone(),
            },
        },
    }
}

fn spawn_cha_collector(
    architecture: &Architecture,
    config: SamplerConfig,
    events: &mpsc::Sender<MetricEvent>,
) {
    let measure_interval = config.measure_interval;

    match ChaCollector::new(architecture).and_then(|mut cha| {
        cha.set_event_profile(config.event_profile)?;
        cha.set_multiplex_mode(config.cha_multiplex_mode);
        cha.set_request_locality(config.cha_request_locality);
        Ok(cha)
    }) {
        Ok(cha) => {
            eprintln!("ocellus: starting CHA collector");
            spawn_collector(
                "cha",
                ChaTask::new(cha, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping CHA collector: {error}");
        }
    }
}

fn spawn_iio_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match IioCollector::new(architecture) {
        Ok(iio) => {
            eprintln!("ocellus: starting IIO collector");
            spawn_collector(
                "iio",
                IioTask::new(iio, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping IIO collector: {error}");
        }
    }
}

fn spawn_imc_collector(
    architecture: &Architecture,
    config: SamplerConfig,
    events: &mpsc::Sender<MetricEvent>,
) {
    let measure_interval = config.measure_interval;

    match ImcCollector::new(architecture).and_then(|mut imc| {
        imc.set_event_profile(config.event_profile)?;
        Ok(imc)
    }) {
        Ok(imc) => {
            eprintln!("ocellus: starting IMC collector");
            spawn_collector(
                "imc",
                ImcTask::new(imc, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping IMC collector: {error}");
        }
    }
}

fn spawn_irp_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match IrpCollector::new(architecture) {
        Ok(irp) => {
            eprintln!("ocellus: starting IRP collector");
            spawn_collector(
                "irp",
                IrpTask::new(irp, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping IRP collector: {error}");
        }
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

fn spawn_pcu_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match PcuCollector::new(architecture) {
        Ok(pcu) => {
            eprintln!("ocellus: starting PCU collector");
            spawn_collector(
                "pcu",
                PcuTask::new(pcu, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping PCU collector: {error}");
        }
    }
}

fn spawn_interconnect_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match InterconnectCollector::new(architecture) {
        Ok(interconnect) => {
            eprintln!("ocellus: starting interconnect collector");
            spawn_collector(
                "interconnect",
                InterconnectTask::new(interconnect, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping interconnect collector: {error}");
        }
    }
}

fn spawn_rdt_collector(
    architecture: &Architecture,
    measure_interval: Duration,
    events: &mpsc::Sender<MetricEvent>,
) {
    match RdtCollector::new(architecture) {
        Ok(rdt) => {
            eprintln!("ocellus: starting RDT collector");
            spawn_collector(
                "rdt",
                RdtTask::new(rdt, measure_interval, events.clone()).run(),
                events.clone(),
            );
        }
        Err(error) => {
            eprintln!("ocellus: skipping RDT collector: {error}");
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
    updates: broadcast::Sender<SampledUpdate>,
) -> Result<(), String> {
    let mut state = MetricsState::default();
    let mut sequences = BTreeMap::<&'static str, u64>::new();

    while let Some(event) = events.recv().await {
        match event {
            MetricEvent::Failure(error) => return Err(error),
            MetricEvent::Update(update, window) => {
                let update = *update;
                let sequence = sequences.entry(update.collector()).or_default();
                *sequence += 1;
                let sequence = *sequence;
                state.apply(update.clone());
                *latest.write().await = state.clone();
                let _ = updates.send(SampledUpdate {
                    update,
                    sequence,
                    window,
                });
            }
        }
    }

    Err("sampler event channel closed".to_string())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::metrics::MetricUpdate;

    fn tsc_event() -> MetricEvent {
        MetricEvent::update(
            MetricUpdate::Tsc(Box::new(crate::metrics::tsc::TscMetrics {
                frequency_hz: 1.0,
            })),
            SystemTime::now(),
        )
    }

    fn rdt_event() -> MetricEvent {
        MetricEvent::update(
            MetricUpdate::Rdt(Box::new(crate::metrics::rdt::RdtMetrics {
                scopes: Vec::new(),
            })),
            SystemTime::now(),
        )
    }

    #[test]
    fn numbers_samples_per_collector() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let received = runtime.block_on(async {
            let (event_tx, event_rx) = mpsc::channel(8);
            let (update_tx, mut update_rx) = broadcast::channel(8);
            let latest = Arc::new(RwLock::new(MetricsState::default()));
            tokio::spawn(aggregate_events(event_rx, latest.clone(), update_tx));

            for event in [tsc_event(), rdt_event(), tsc_event()] {
                event_tx.send(event).await.unwrap();
            }
            let mut received = Vec::new();
            for _ in 0..3 {
                let update = update_rx.recv().await.unwrap();
                assert!(update.window.start <= update.window.end);
                received.push((update.update.collector(), update.sequence));
            }
            assert!(latest.read().await.tsc.is_some());
            received
        });

        assert_eq!(received, [("tsc", 1), ("rdt", 1), ("tsc", 2)]);
    }
}
