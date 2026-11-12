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
    CollectorMetadata, ConfigurableCollector, EventProfile, InfoMetadata, MetricEvent,
    MetricsState, SampledUpdate, SamplerUpdate,
};
use crate::pmu_config::types::BoxType;
use crate::runtime::pmu_control::PmuControl;

const EVENT_CHANNEL_CAPACITY: usize = 64;
const UPDATE_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct SamplerMetadata {
    pub measure_interval: Duration,
    pub info: InfoMetadata,
}

#[derive(Clone, Debug)]
pub struct SamplerReader {
    control: PmuControl,
    metadata: SamplerMetadata,
    latest: Arc<RwLock<MetricsState>>,
    updates: broadcast::Sender<SamplerUpdate>,
}

impl SamplerReader {
    /// Sets the PMU configs of the CHA, IIO, IRP and IMC collectors.
    pub fn pmu_control(&self) -> PmuControl {
        self.control.clone()
    }

    pub async fn latest_state(&self) -> MetricsState {
        self.latest.read().await.clone()
    }

    pub fn metadata(&self) -> SamplerMetadata {
        self.metadata.clone()
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<SamplerUpdate> {
        self.updates.subscribe()
    }

    #[cfg(test)]
    pub fn new_for_test(metadata: SamplerMetadata, latest_state: MetricsState) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);

        Self::from_parts(
            PmuControl::new(crate::arch::IntelServerCpuModel::SkylakeXeon),
            metadata,
            Arc::new(RwLock::new(latest_state)),
            updates,
        )
    }

    /// A sampler of one fake collector of `box_type`.
    #[cfg(test)]
    pub fn spawn_for_test(
        metadata: SamplerMetadata,
        box_type: BoxType,
        collector: crate::metrics::testing::FakeCollector,
    ) -> Self {
        let latest = Arc::new(RwLock::new(MetricsState::default()));
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (update_tx, _) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);
        let mut control = PmuControl::new(crate::arch::IntelServerCpuModel::SkylakeXeon);
        let requests = control.register(box_type, collector.config_units());
        tokio::spawn(crate::metrics::run_configurable_collector(
            collector,
            metadata.measure_interval,
            requests,
            event_tx,
        ));
        tokio::spawn(aggregate_events(
            event_rx,
            latest.clone(),
            update_tx.clone(),
        ));

        Self::from_parts(control, metadata, latest, update_tx)
    }

    #[cfg(test)]
    pub fn send_update_for_test(&self, update: SamplerUpdate) {
        let _ = self.updates.send(update);
    }

    fn from_parts(
        control: PmuControl,
        metadata: SamplerMetadata,
        latest: Arc<RwLock<MetricsState>>,
        updates: broadcast::Sender<SamplerUpdate>,
    ) -> Self {
        Self {
            control,
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
    let mut control = PmuControl::new(architecture.intel_server_model());

    spawn_cha_collector(&architecture, config, &event_tx, &mut control);
    spawn_rapl_collector(&architecture, measure_interval, &event_tx);
    spawn_imc_collector(&architecture, config, &event_tx, &mut control);
    spawn_iio_collector(&architecture, measure_interval, &event_tx, &mut control);
    spawn_irp_collector(&architecture, measure_interval, &event_tx, &mut control);
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
        reader: SamplerReader::from_parts(control, metadata, latest, update_tx),
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
    control: &mut PmuControl,
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
            let requests = control.register(BoxType::Cha, cha.config_units());
            spawn_collector(
                "cha",
                ChaTask::new(cha, measure_interval, events.clone(), requests).run(),
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
    control: &mut PmuControl,
) {
    match IioCollector::new(architecture) {
        Ok(iio) => {
            eprintln!("ocellus: starting IIO collector");
            let requests = control.register(BoxType::Iio, iio.config_units());
            spawn_collector(
                "iio",
                IioTask::new(iio, measure_interval, events.clone(), requests).run(),
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
    control: &mut PmuControl,
) {
    let measure_interval = config.measure_interval;

    match ImcCollector::new(architecture).and_then(|mut imc| {
        imc.set_event_profile(config.event_profile)?;
        Ok(imc)
    }) {
        Ok(imc) => {
            eprintln!("ocellus: starting IMC collector");
            let requests = control.register(BoxType::Imc, imc.config_units());
            spawn_collector(
                "imc",
                ImcTask::new(imc, measure_interval, events.clone(), requests).run(),
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
    control: &mut PmuControl,
) {
    match IrpCollector::new(architecture) {
        Ok(irp) => {
            eprintln!("ocellus: starting IRP collector");
            let requests = control.register(BoxType::Irp, irp.config_units());
            spawn_collector(
                "irp",
                IrpTask::new(irp, measure_interval, events.clone(), requests).run(),
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
            spawn_isolated_collector(
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

/// Spawns a collector whose errors stop only itself: they reach the sampler
/// as `MetricEvent::Stopped` rather than `MetricEvent::Failure`.
fn spawn_isolated_collector(
    name: &'static str,
    collector: impl Future<Output = Result<(), String>> + Send + 'static,
    events: mpsc::Sender<MetricEvent>,
) {
    tokio::spawn(async move {
        let error = match tokio::spawn(collector).await {
            // The event channel closed, so the sampler is already gone.
            Ok(Ok(())) => return,
            Ok(Err(error)) => error,
            Err(error) => format!("{name} collector task failed: {error}"),
        };
        let _ = events
            .send(MetricEvent::Stopped {
                collector: name,
                error,
            })
            .await;
    });
}

async fn aggregate_events(
    mut events: mpsc::Receiver<MetricEvent>,
    latest: Arc<RwLock<MetricsState>>,
    updates: broadcast::Sender<SamplerUpdate>,
) -> Result<(), String> {
    let mut state = MetricsState::default();
    let mut sequences = BTreeMap::<&'static str, u64>::new();

    while let Some(event) = events.recv().await {
        match event {
            MetricEvent::Failure(error) => return Err(error),
            MetricEvent::Stopped { collector, error } => {
                eprintln!(
                    "ocellus: {collector} collector stopped: {error}; the other collectors continue"
                );
                state.clear(collector);
                *latest.write().await = state.clone();
                let _ = updates.send(SamplerUpdate::Stopped(collector));
            }
            MetricEvent::ConfigChanged(box_type) => {
                state.config_changed(box_type);
                *latest.write().await = state.clone();
                let _ = updates.send(SamplerUpdate::ConfigChanged(box_type));
            }
            MetricEvent::Update(update, window) => {
                let update = *update;
                let sequence = sequences.entry(update.collector()).or_default();
                *sequence += 1;
                let sequence = *sequence;
                state.apply(update.clone());
                *latest.write().await = state.clone();
                let _ = updates.send(SamplerUpdate::Sample(SampledUpdate {
                    update,
                    sequence,
                    window,
                }));
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
                let SamplerUpdate::Sample(update) = update_rx.recv().await.unwrap() else {
                    panic!("expected a sample");
                };
                assert!(update.window.start <= update.window.end);
                received.push((update.update.collector(), update.sequence));
            }
            assert!(latest.read().await.tsc.is_some());
            received
        });

        assert_eq!(received, [("tsc", 1), ("rdt", 1), ("tsc", 2)]);
    }

    #[test]
    fn keeps_sampling_after_an_isolated_collector_stops() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (event_tx, event_rx) = mpsc::channel(8);
            let (update_tx, mut update_rx) = broadcast::channel(8);
            let latest = Arc::new(RwLock::new(MetricsState::default()));
            let aggregate = tokio::spawn(aggregate_events(event_rx, latest.clone(), update_tx));

            event_tx.send(rdt_event()).await.unwrap();
            event_tx
                .send(MetricEvent::Stopped {
                    collector: "rdt",
                    error: "monitor group disappeared".to_string(),
                })
                .await
                .unwrap();
            event_tx.send(tsc_event()).await.unwrap();

            assert!(matches!(
                update_rx.recv().await.unwrap(),
                SamplerUpdate::Sample(SampledUpdate { sequence: 1, .. })
            ));
            assert!(matches!(
                update_rx.recv().await.unwrap(),
                SamplerUpdate::Stopped("rdt")
            ));
            let SamplerUpdate::Sample(update) = update_rx.recv().await.unwrap() else {
                panic!("expected a sample");
            };
            assert_eq!(update.update.collector(), "tsc");
            assert!(!aggregate.is_finished());
            let state = latest.read().await;
            assert!(state.rdt.is_none());
            assert!(state.tsc.is_some());
        });
    }

    #[test]
    fn keeps_config_counts_until_the_config_changes() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (event_tx, event_rx) = mpsc::channel(8);
            let (update_tx, mut update_rx) = broadcast::channel(8);
            let latest = Arc::new(RwLock::new(MetricsState::default()));
            tokio::spawn(aggregate_events(event_rx, latest.clone(), update_tx));
            let counts = crate::metrics::testing::uncore_counts(
                crate::metrics::testing::pmu_config(BoxType::Irp, 7, 1_000),
                42,
            );

            event_tx.send(tsc_event()).await.unwrap();
            event_tx
                .send(MetricEvent::ConfigChanged(BoxType::Irp))
                .await
                .unwrap();
            event_tx
                .send(MetricEvent::update(
                    MetricUpdate::Uncore(Box::new(counts.clone())),
                    SystemTime::now(),
                ))
                .await
                .unwrap();
            for _ in 0..2 {
                update_rx.recv().await.unwrap();
            }
            let SamplerUpdate::Sample(update) = update_rx.recv().await.unwrap() else {
                panic!("expected a sample");
            };
            assert_eq!((update.update.collector(), update.sequence), ("irp", 1));
            assert_eq!(latest.read().await.uncore[&BoxType::Irp], counts);

            event_tx
                .send(MetricEvent::ConfigChanged(BoxType::Irp))
                .await
                .unwrap();
            assert!(matches!(
                update_rx.recv().await.unwrap(),
                SamplerUpdate::ConfigChanged(BoxType::Irp)
            ));
            let state = latest.read().await;
            assert!(state.uncore.is_empty());
            assert!(state.tsc.is_some());
        });
    }

    #[test]
    fn reports_isolated_collector_errors_as_stopped() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (event_tx, mut event_rx) = mpsc::channel(8);
            spawn_isolated_collector("rdt", async { Err("boom".to_string()) }, event_tx.clone());

            let event = event_rx.recv().await.unwrap();
            assert!(matches!(
                event,
                MetricEvent::Stopped { collector: "rdt", ref error } if error == "boom"
            ));

            spawn_isolated_collector("rdt", async { Ok(()) }, event_tx);
            // A clean exit means the sampler is gone: no event, then the
            // channel closes.
            let next =
                tokio::time::timeout(std::time::Duration::from_millis(200), event_rx.recv()).await;
            assert!(!matches!(next, Ok(Some(_))));
        });
    }
}
