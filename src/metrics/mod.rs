pub mod cha;
pub(crate) mod common;
pub mod iio;
pub mod imc;
mod info;
pub mod interconnect;
pub mod irp;
pub mod pcu;
pub mod rapl;
pub mod rdt;
pub mod sample;
#[cfg(test)]
pub(crate) mod testing;
pub mod tsc;
pub mod uncore;
mod uncore_counts;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use prometheus_client::registry::Registry;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::pmu_config::types::{BoxType, PmuConfig, UncoreCounts};
use crate::runtime::pmu_control::{ConfigRequest, ConfigRequests};
pub use sample::SampleWindow;

/// Which fixed set of CHA and IMC event groups the collectors measure.
///
/// `Default` measures every group Ocellus knows. The `MemIo*` profiles
/// measure only the groups one mem-io experiment needs, so each group gets a
/// larger share of the sample interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EventProfile {
    #[default]
    Default,
    MemIo2Hit,
    MemIoBasic,
    MemIoItom,
}

impl EventProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::MemIo2Hit => "mem-io-2hit",
            Self::MemIoBasic => "mem-io-basic",
            Self::MemIoItom => "mem-io-itom",
        }
    }

    /// Rejects profiles the architecture or multiplexing mode cannot honor.
    ///
    /// The mem-io profiles reproduce measurements whose event groups were
    /// time-multiplexed across all CHAs, so they require temporal mode rather
    /// than silently measuring different CHA subsets.
    pub fn validate(
        self,
        architecture: &Architecture,
        cha_multiplex_mode: cha::ChaMultiplexMode,
    ) -> Result<(), String> {
        if self == Self::Default {
            return Ok(());
        }

        if IntelServerCpuModel::from_family_model(architecture.family, architecture.model)
            != Some(IntelServerCpuModel::SkylakeXeon)
        {
            return Err(format!(
                "--event-profile {} is only supported on Skylake-SP and Cascade Lake (CPU model 0x55), not model {:#x}",
                self.label(),
                architecture.model
            ));
        }

        if cha_multiplex_mode != cha::ChaMultiplexMode::Temporal {
            return Err(format!(
                "--event-profile {} requires --cha-multiplex-mode temporal",
                self.label()
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricsState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cha: Option<cha::ChaMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iio: Option<iio::IioMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imc: Option<imc::ImcMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interconnect: Option<interconnect::InterconnectMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub irp: Option<irp::IrpMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcu: Option<pcu::PcuMetrics>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rapl: Option<rapl::RaplMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rdt: Option<rdt::RdtMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsc: Option<tsc::TscMetrics>,
    /// The latest counts of each box type that runs a PMU config.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub uncore: BTreeMap<BoxType, UncoreCounts>,
}

impl MetricsState {
    /// Forgets a stopped collector's last metrics.
    pub fn clear(&mut self, collector: &str) {
        match collector {
            "cha" => self.cha = None,
            "iio" => self.iio = None,
            "imc" => self.imc = None,
            "interconnect" => self.interconnect = None,
            "irp" => self.irp = None,
            "pcu" => self.pcu = None,
            "rapl" => self.rapl = None,
            "rdt" => self.rdt = None,
            "tsc" => self.tsc = None,
            _ => {}
        }
    }

    pub fn apply(&mut self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => self.cha = Some(*cha),
            MetricUpdate::Iio(iio) => self.iio = Some(*iio),
            MetricUpdate::Imc(imc) => self.imc = Some(*imc),
            MetricUpdate::Interconnect(interconnect) => self.interconnect = Some(*interconnect),
            MetricUpdate::Irp(irp) => self.irp = Some(*irp),
            MetricUpdate::Pcu(pcu) => self.pcu = Some(*pcu),
            MetricUpdate::Rapl(rapl) => self.rapl = Some(*rapl),
            MetricUpdate::Rdt(rdt) => self.rdt = Some(*rdt),
            MetricUpdate::Tsc(tsc) => self.tsc = Some(*tsc),
            MetricUpdate::Uncore(counts) => {
                self.uncore.insert(counts.config.program.box_type, *counts);
            }
        }
    }

    /// Forgets a box type's metrics when its PMU config changes: its default
    /// metrics when a config replaces them, its counts when the config
    /// changes or goes.
    pub fn config_changed(&mut self, box_type: BoxType) {
        self.clear(box_type.label());
        self.uncore.remove(&box_type);
    }
}

impl Default for MetricsState {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            cha: None,
            iio: None,
            imc: None,
            interconnect: None,
            irp: None,
            pcu: None,
            rapl: None,
            rdt: None,
            tsc: None,
            uncore: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CollectorMetadata {
    pub event_profile: EventProfile,
    pub cha_group_count: Option<usize>,
    pub cha_multiplex_mode: cha::ChaMultiplexMode,
    pub cha_request_locality: cha::ChaRequestLocality,
    pub cha_supported: bool,
    pub iio_supported: bool,
    pub imc_supported: bool,
    pub interconnect_supported: bool,
    pub irp_supported: bool,
    pub pcu_supported: bool,
    pub rdt_supported: bool,
}

#[derive(Clone, Debug)]
pub struct InfoMetadata {
    pub collectors: CollectorMetadata,
    pub processor: ProcessorMetadata,
}

#[derive(Clone, Debug)]
pub struct ProcessorMetadata {
    pub brand: String,
    pub family: u8,
    pub invariant_tsc_supported: bool,
    pub model: u8,
    pub package_rapl_supported: bool,
    pub vendor: String,
}

#[derive(Clone, Debug)]
pub enum MetricEvent {
    Failure(String),
    /// A collector stopped on its own error; the other collectors continue.
    Stopped {
        collector: &'static str,
        error: String,
    },
    /// A box type's collector switched to another PMU config, or to its
    /// default groups.
    ConfigChanged(BoxType),
    Update(Box<MetricUpdate>, SampleWindow),
}

impl MetricEvent {
    /// An update for the sample that started at `started_at` and ends now.
    pub fn update(update: MetricUpdate, started_at: std::time::SystemTime) -> Self {
        Self::Update(Box::new(update), SampleWindow::since(started_at))
    }
}

#[derive(Clone, Debug)]
pub enum MetricUpdate {
    Cha(Box<cha::ChaMetrics>),
    Iio(Box<iio::IioMetrics>),
    Imc(Box<imc::ImcMetrics>),
    Interconnect(Box<interconnect::InterconnectMetrics>),
    Irp(Box<irp::IrpMetrics>),
    Pcu(Box<pcu::PcuMetrics>),
    Rapl(Box<rapl::RaplMetrics>),
    Rdt(Box<rdt::RdtMetrics>),
    Tsc(Box<tsc::TscMetrics>),
    /// A sample of a PMU config, from the collector of its box type.
    Uncore(Box<UncoreCounts>),
}

impl MetricUpdate {
    pub fn collector(&self) -> &'static str {
        match self {
            Self::Cha(_) => "cha",
            Self::Iio(_) => "iio",
            Self::Imc(_) => "imc",
            Self::Interconnect(_) => "interconnect",
            Self::Irp(_) => "irp",
            Self::Pcu(_) => "pcu",
            Self::Rapl(_) => "rapl",
            Self::Rdt(_) => "rdt",
            Self::Tsc(_) => "tsc",
            Self::Uncore(counts) => counts.config.program.box_type.label(),
        }
    }
}

/// What the sampler broadcasts to exporters.
#[derive(Clone, Debug)]
pub enum SamplerUpdate {
    Sample(SampledUpdate),
    Stopped(&'static str),
    ConfigChanged(BoxType),
}

/// One collector update with the window it covers and its sequence number,
/// counted per collector from 1.
#[derive(Clone, Debug)]
pub struct SampledUpdate {
    pub update: MetricUpdate,
    pub sequence: u64,
    pub window: SampleWindow,
}

#[derive(Debug)]
pub struct MetricsRegistry {
    cha: Option<cha::ChaPrometheusMetrics>,
    iio: Option<iio::IioPrometheusMetrics>,
    imc: Option<imc::ImcPrometheusMetrics>,
    interconnect: Option<interconnect::InterconnectPrometheusMetrics>,
    irp: Option<irp::IrpPrometheusMetrics>,
    pcu: Option<pcu::PcuPrometheusMetrics>,
    rapl: Option<rapl::RaplPrometheusMetrics>,
    rdt: Option<rdt::RdtPrometheusMetrics>,
    samples: sample::SamplePrometheusMetrics,
    tsc: tsc::TscPrometheusMetrics,
    uncore: uncore_counts::UncoreCountPrometheusMetrics,
    /// Box types whose last sample counted a PMU config: their default
    /// metrics stopped updating, so /metrics leaves them out.
    configured: Mutex<BTreeSet<BoxType>>,
}

impl MetricsRegistry {
    pub fn register(registry: &mut Registry, metadata: InfoMetadata) -> Self {
        info::register(registry, metadata.clone());

        Self {
            cha: cha::ChaPrometheusMetrics::register(registry, &metadata),
            iio: iio::IioPrometheusMetrics::register(registry, &metadata),
            imc: imc::ImcPrometheusMetrics::register(registry, &metadata),
            interconnect: interconnect::InterconnectPrometheusMetrics::register(
                registry, &metadata,
            ),
            irp: irp::IrpPrometheusMetrics::register(registry, &metadata),
            pcu: pcu::PcuPrometheusMetrics::register(registry, &metadata),
            rapl: rapl::RaplPrometheusMetrics::register(registry, &metadata),
            rdt: rdt::RdtPrometheusMetrics::register(registry, &metadata),
            samples: sample::SamplePrometheusMetrics::register(registry),
            tsc: tsc::TscPrometheusMetrics::register(registry),
            uncore: uncore_counts::UncoreCountPrometheusMetrics::register(registry),
            configured: Mutex::default(),
        }
    }

    /// Sets the collector's metrics together with its sample sequence and
    /// window. Callers hold the registry lock that scrapes take, so a scrape
    /// sees all of one sample or none of it.
    pub fn update(&self, sampled: SampledUpdate) {
        self.samples
            .update(sampled.update.collector(), sampled.sequence, sampled.window);
        self.update_collector(sampled.update);
    }

    /// Removes a stopped collector's series so its last values do not look
    /// current, and marks it stopped. Only collectors spawned isolated stop
    /// without failing the sampler; RDT is the one that does.
    pub fn stop(&self, collector: &str) {
        if collector == "rdt"
            && let Some(rdt) = &self.rdt
        {
            rdt.clear();
        }
        self.samples.stop(collector);
    }

    /// Removes the series of a box type's previous PMU config, and leaves out
    /// its default metrics until it samples its default groups again.
    pub fn config_changed(&self, box_type: BoxType) {
        self.uncore.remove(box_type);
        self.configured.lock().unwrap().insert(box_type);
    }

    /// Drops the default metrics of configured box types from rendered
    /// metrics. A box type's default families are those named
    /// `ocellus_<box type>_`.
    pub fn without_configured_defaults(&self, metrics: String) -> String {
        let prefixes = self
            .configured
            .lock()
            .unwrap()
            .iter()
            .map(|box_type| format!("ocellus_{}_", box_type.label()))
            .collect::<Vec<_>>();
        if prefixes.is_empty() {
            return metrics;
        }

        metrics
            .split_inclusive('\n')
            .filter(|line| {
                let name = ["# HELP ", "# TYPE ", "# UNIT "]
                    .iter()
                    .find_map(|comment| line.strip_prefix(comment))
                    .unwrap_or(line);
                !prefixes.iter().any(|prefix| name.starts_with(prefix))
            })
            .collect()
    }

    #[cfg(test)]
    pub fn sample_sequence(&self, collector: &str) -> u64 {
        self.samples.sequence(collector)
    }

    fn update_collector(&self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => {
                self.default_sampled(BoxType::Cha);
                expect_registered(&self.cha, "CHA").update(*cha)
            }
            MetricUpdate::Iio(iio) => {
                self.default_sampled(BoxType::Iio);
                expect_registered(&self.iio, "IIO").update(*iio)
            }
            MetricUpdate::Imc(imc) => {
                self.default_sampled(BoxType::Imc);
                expect_registered(&self.imc, "IMC").update(*imc)
            }
            MetricUpdate::Interconnect(interconnect) => {
                expect_registered(&self.interconnect, "interconnect").update(*interconnect)
            }
            MetricUpdate::Irp(irp) => {
                self.default_sampled(BoxType::Irp);
                expect_registered(&self.irp, "IRP").update(*irp)
            }
            MetricUpdate::Pcu(pcu) => expect_registered(&self.pcu, "PCU").update(*pcu),
            MetricUpdate::Rapl(rapl) => expect_registered(&self.rapl, "RAPL").update(*rapl),
            MetricUpdate::Rdt(rdt) => expect_registered(&self.rdt, "RDT").update(*rdt),
            MetricUpdate::Tsc(tsc) => self.tsc.update(*tsc),
            MetricUpdate::Uncore(counts) => {
                self.configured
                    .lock()
                    .unwrap()
                    .insert(counts.config.program.box_type);
                self.uncore.update(*counts);
            }
        }
    }

    /// A box type sampled its default groups, so it runs no PMU config.
    fn default_sampled(&self, box_type: BoxType) {
        self.uncore.remove(box_type);
        self.configured.lock().unwrap().remove(&box_type);
    }

    #[cfg(test)]
    pub fn update_state(&self, state: MetricsState) {
        if let Some(cha) = state.cha {
            expect_registered(&self.cha, "CHA").update(cha);
        }
        if let Some(iio) = state.iio {
            expect_registered(&self.iio, "IIO").update(iio);
        }
        if let Some(imc) = state.imc {
            expect_registered(&self.imc, "IMC").update(imc);
        }
        if let Some(interconnect) = state.interconnect {
            expect_registered(&self.interconnect, "interconnect").update(interconnect);
        }
        if let Some(irp) = state.irp {
            expect_registered(&self.irp, "IRP").update(irp);
        }
        if let Some(pcu) = state.pcu {
            expect_registered(&self.pcu, "PCU").update(pcu);
        }
        if let Some(rapl) = state.rapl {
            expect_registered(&self.rapl, "RAPL").update(rapl);
        }
        if let Some(rdt) = state.rdt {
            expect_registered(&self.rdt, "RDT").update(rdt);
        }
        if let Some(tsc) = state.tsc {
            self.tsc.update(tsc);
        }
    }
}

fn expect_registered<'a, T>(prometheus: &'a Option<T>, name: &str) -> &'a T {
    prometheus
        .as_ref()
        .unwrap_or_else(|| panic!("received {name} update without registered {name} metrics"))
}

/// A periodic collector of an uncore box type, whose PMU a config can
/// program.
pub(crate) trait ConfigurableCollector: Send {
    fn sample_update(
        &mut self,
        interval: Duration,
    ) -> impl Future<Output = Result<MetricUpdate, String>> + Send;

    /// The unit ids of each package that configs can program; empty if the
    /// collector takes no configs.
    fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>>;

    fn has_config(&self) -> bool;

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String>;

    /// Samples the set config for `interval`. Its counts are cumulative since
    /// the config was set.
    fn sample_config(
        &mut self,
        interval: Duration,
    ) -> impl Future<Output = Result<UncoreCounts, String>> + Send;
}

/// Samples `collector` back to back: its config while one is set, its
/// default groups otherwise. A config request cancels the sample in flight
/// at its next await; samples touch registers only between awaits, never
/// across one.
pub(crate) async fn run_configurable_collector<C: ConfigurableCollector>(
    mut collector: C,
    interval: Duration,
    mut requests: ConfigRequests,
    events: mpsc::Sender<MetricEvent>,
) {
    let box_type = requests.box_type();

    loop {
        let started_at = SystemTime::now();
        let sample = tokio::select! {
            biased;
            request = requests.next() => {
                let ConfigRequest { config, ack } = request;
                let result = collector.set_config(config);
                if result.is_ok()
                    && events
                        .send(MetricEvent::ConfigChanged(box_type))
                        .await
                        .is_err()
                {
                    return;
                }
                let _ = ack.send(result);
                continue;
            }
            sample = sample_configurable(&mut collector, interval) => sample,
        };

        match sample {
            Ok(update) => {
                if events
                    .send(MetricEvent::update(update, started_at))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = events.send(MetricEvent::Failure(error)).await;
                return;
            }
        }
    }
}

async fn sample_configurable<C: ConfigurableCollector>(
    collector: &mut C,
    interval: Duration,
) -> Result<MetricUpdate, String> {
    if collector.has_config() {
        let counts = collector.sample_config(interval).await?;
        Ok(MetricUpdate::Uncore(Box::new(counts)))
    } else {
        collector.sample_update(interval).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::runtime::pmu_control::PmuControl;

    const MEM_IO_PROFILES: [EventProfile; 3] = [
        EventProfile::MemIo2Hit,
        EventProfile::MemIoBasic,
        EventProfile::MemIoItom,
    ];

    fn architecture(model: u8) -> Architecture {
        Architecture {
            brand: "test".to_string(),
            family: 6,
            features: crate::arch::ArchitectureFeatures::default(),
            model,
            vendor: "GenuineIntel".to_string(),
        }
    }

    #[test]
    fn accepts_default_profile_everywhere() {
        for model in [0x2d, 0x4f, 0x55, 0x6a, 0x8f] {
            assert!(
                EventProfile::Default
                    .validate(&architecture(model), cha::ChaMultiplexMode::spatial(4))
                    .is_ok()
            );
        }
    }

    #[test]
    fn accepts_mem_io_profiles_on_skylake_sp_with_temporal_multiplexing() {
        for profile in MEM_IO_PROFILES {
            assert!(
                profile
                    .validate(&architecture(0x55), cha::ChaMultiplexMode::Temporal)
                    .is_ok()
            );
        }
    }

    #[test]
    fn rejects_mem_io_profiles_off_skylake_sp() {
        for profile in MEM_IO_PROFILES {
            for model in [0x4f, 0x6a, 0x8f] {
                assert!(
                    profile
                        .validate(&architecture(model), cha::ChaMultiplexMode::Temporal)
                        .is_err()
                );
            }
        }
    }

    fn spawn_fake_collector(
        collector: testing::FakeCollector,
        interval: Duration,
    ) -> (PmuControl, mpsc::Receiver<MetricEvent>) {
        let mut control = PmuControl::new(IntelServerCpuModel::SkylakeXeon);
        let requests = control.register(BoxType::Cha, collector.config_units());
        let (events_tx, events_rx) = mpsc::channel(8);
        tokio::spawn(run_configurable_collector(
            collector, interval, requests, events_tx,
        ));

        (control, events_rx)
    }

    fn tsc_update() -> MetricUpdate {
        MetricUpdate::Tsc(Box::new(tsc::TscMetrics { frequency_hz: 1.0 }))
    }

    #[test]
    fn sends_a_sample_after_each_interval() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (collector, _log) = testing::FakeCollector::new(tsc_update());
            let (_control, mut events) = spawn_fake_collector(collector, Duration::from_millis(1));

            for _ in 0..2 {
                let Some(MetricEvent::Update(update, window)) = events.recv().await else {
                    panic!("expected a sample");
                };
                assert_eq!(update.collector(), "tsc");
                assert!(window.start <= window.end);
            }
        });
    }

    #[test]
    fn a_config_request_cancels_the_sample_in_flight_and_switches_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (collector, mut log) = testing::FakeCollector::new(tsc_update());
            // Far longer than the test: only a cancelled sample lets it end.
            let (control, mut events) = spawn_fake_collector(collector, Duration::from_secs(3600));
            assert_eq!(log.recv().await.unwrap(), "sample started");

            let setting = Instant::now();
            let set = tokio::spawn({
                let control = control.clone();
                async move {
                    control
                        .set(vec![testing::pmu_config(BoxType::Cha, 7, 0)])
                        .await
                }
            });
            assert!(matches!(
                events.recv().await,
                Some(MetricEvent::ConfigChanged(BoxType::Cha))
            ));
            assert_eq!(set.await.unwrap(), Ok(()));
            assert!(setting.elapsed() < Duration::from_secs(1));
            assert_eq!(log.recv().await.unwrap(), "sample cancelled");
            assert_eq!(log.recv().await.unwrap(), "set_config 7");
            assert_eq!(log.recv().await.unwrap(), "config sample started");

            control.set(Vec::new()).await.unwrap();
            assert!(matches!(
                events.recv().await,
                Some(MetricEvent::ConfigChanged(BoxType::Cha))
            ));
            assert_eq!(log.recv().await.unwrap(), "config sample cancelled");
            assert_eq!(log.recv().await.unwrap(), "set_config none");
            assert_eq!(log.recv().await.unwrap(), "sample started");
        });
    }

    #[test]
    fn samples_the_config_once_set() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (collector, _log) = testing::FakeCollector::new(tsc_update());
            let (control, mut events) = spawn_fake_collector(collector, Duration::from_millis(1));
            let config = testing::pmu_config(BoxType::Cha, 7, 0);

            control.set(vec![config.clone()]).await.unwrap();
            loop {
                match events.recv().await.unwrap() {
                    MetricEvent::Update(update, _) if update.collector() == "tsc" => {}
                    MetricEvent::ConfigChanged(BoxType::Cha) => break,
                    event => panic!("unexpected event {event:?}"),
                }
            }
            for value in [10, 20] {
                let Some(MetricEvent::Update(update, _)) = events.recv().await else {
                    panic!("expected a sample");
                };
                let MetricUpdate::Uncore(counts) = *update else {
                    panic!("expected a config sample, got {update:?}");
                };
                assert_eq!(*counts, testing::uncore_counts(config.clone(), value));
            }
        });
    }

    #[test]
    fn keeps_sampling_its_default_groups_when_a_config_fails() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let (collector, mut log) = testing::FakeCollector::new(tsc_update());
            let collector = collector.failing("no such box");
            let (control, mut events) = spawn_fake_collector(collector, Duration::from_millis(1));

            let error = control
                .set(vec![testing::pmu_config(BoxType::Cha, 7, 0)])
                .await
                .unwrap_err();
            assert_eq!(error, "cha: no such box (box types already switched: none)");
            assert!(control.configs().await.is_empty());
            for _ in 0..3 {
                let Some(MetricEvent::Update(update, _)) = events.recv().await else {
                    panic!("expected a default sample");
                };
                assert_eq!(update.collector(), "tsc");
            }
            while let Ok(line) = log.try_recv() {
                assert!(!line.starts_with("config sample"), "{line}");
            }
        });
    }

    #[test]
    fn rejects_mem_io_profiles_with_spatial_multiplexing() {
        for profile in MEM_IO_PROFILES {
            let error = profile
                .validate(&architecture(0x55), cha::ChaMultiplexMode::spatial(4))
                .unwrap_err();

            assert!(error.contains("--cha-multiplex-mode temporal"), "{error}");
        }
    }
}
