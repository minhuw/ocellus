pub mod hsx;
pub mod icx;
pub mod skx;
pub mod snb;
pub mod spr;

use std::collections::BTreeMap;
use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::pci;
use crate::metrics::common::BYTES_PER_CACHE_LINE;
use crate::metrics::uncore::skx::{UncoreScope, events_per_second, scale_to_enabled};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

pub const CHA_COUNTER_COUNT: usize = 4;
const LINUX_UNCORE_EVENT_SOURCE_ROOT: &str = "/sys/bus/event_source/devices";

pub(crate) fn linux_uncore_unit_ids(
    prefixes: &[&str],
    max_count: usize,
) -> Result<Vec<usize>, String> {
    let mut ids = Vec::new();

    for entry in std::fs::read_dir(LINUX_UNCORE_EVENT_SOURCE_ROOT).map_err(|error| {
        format!(
            "failed to read Linux uncore PMU devices from {LINUX_UNCORE_EVENT_SOURCE_ROOT}: {error}"
        )
    })? {
        let entry =
            entry.map_err(|error| format!("failed to read Linux uncore PMU device: {error}"))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };

        if let Some(id) =
            linux_uncore_unit_id_from_event_source_name(file_name, prefixes, max_count)
        {
            ids.push(id);
        }
    }

    ids.sort_unstable();
    ids.dedup();

    if ids.is_empty() {
        return Err(format!(
            "Linux uncore PMU exposes no units matching {:?}",
            prefixes
        ));
    }

    Ok(ids)
}

pub(crate) fn linux_uncore_unit_id_from_event_source_name(
    name: &str,
    prefixes: &[&str],
    max_count: usize,
) -> Option<usize> {
    for prefix in prefixes {
        let Some(id) = name.strip_prefix(prefix) else {
            continue;
        };
        let Ok(id) = id.parse::<usize>() else {
            continue;
        };

        if id < max_count {
            return Some(id);
        }
    }

    None
}

pub(crate) fn pci_location_for_cpu(
    cpu: u32,
    locations: &[pci::PciLocation],
    device_name: &str,
) -> Result<pci::PciLocation, String> {
    pci_location_for_cpu_with_local_cpus(cpu, locations, device_name, pci::local_cpus)
}

pub(crate) fn pci_location_for_cpu_with_local_cpus(
    cpu: u32,
    locations: &[pci::PciLocation],
    device_name: &str,
    mut local_cpus: impl FnMut(pci::PciLocation) -> Result<Vec<u32>, String>,
) -> Result<pci::PciLocation, String> {
    match locations {
        [] => Err(format!("failed to find {device_name} PCI device")),
        [location] => Ok(*location),
        _ => locations
            .iter()
            .copied()
            .find(|location| {
                local_cpus(*location).is_ok_and(|local_cpus| local_cpus.contains(&cpu))
            })
            .ok_or_else(|| format!("failed to map CPU {cpu} to a {device_name} PCI device")),
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaCacheState {
    All,
    E,
    F,
    I,
    M,
    S,
    SfE,
    SfM,
    SfS,
}

impl ChaCacheState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::E => "e",
            Self::F => "f",
            Self::I => "i",
            Self::M => "m",
            Self::S => "s",
            Self::SfE => "sf_e",
            Self::SfM => "sf_m",
            Self::SfS => "sf_s",
        }
    }

    pub(crate) const fn filter0_bits(self) -> u16 {
        match self {
            Self::All => 0x00,
            Self::I => 0x01,
            Self::SfS => 0x02,
            Self::SfE => 0x04,
            Self::SfM => 0x08,
            Self::S => 0x10,
            Self::E => 0x20,
            Self::M => 0x40,
            Self::F => 0x80,
        }
    }

    #[cfg(test)]
    pub(crate) const fn llc_lookup_any_state_bits() -> u16 {
        0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaLookupOperation {
    Any,
    Read,
    RemoteSnoop,
    Write,
}

impl ChaLookupOperation {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Read => "read",
            Self::RemoteSnoop => "remote_snoop",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaHaRequestLocality {
    Local,
    Remote,
}

impl ChaHaRequestLocality {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaNoCreditDirection {
    Read,
    Write,
}

impl ChaNoCreditDirection {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaRequestOperation {
    Read,
    Write,
}

impl ChaRequestOperation {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaRequestSource {
    Ia,
    Io,
}

impl ChaRequestSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ia => "ia",
            Self::Io => "io",
        }
    }

    pub(crate) const fn all_umask(self) -> u8 {
        match self {
            Self::Ia => 0x31,
            Self::Io => 0x34,
        }
    }

    pub(crate) const fn result_umask(self, result: ChaTransactionResult) -> u8 {
        match (self, result) {
            (Self::Ia, ChaTransactionResult::All) => 0x31,
            (Self::Io, ChaTransactionResult::All) => 0x34,
            (Self::Ia, ChaTransactionResult::Hit) => 0x11,
            (Self::Io, ChaTransactionResult::Hit) => 0x14,
            (Self::Ia, ChaTransactionResult::Miss) => 0x21,
            (Self::Io, ChaTransactionResult::Miss) => 0x24,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaRxcQueue {
    Irq,
    Prq,
}

impl ChaRxcQueue {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Irq => "irq",
            Self::Prq => "prq",
        }
    }

    pub(crate) const fn umask(self) -> u8 {
        match self {
            Self::Irq => 0x01,
            Self::Prq => 0x10,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaTransactionResult {
    All,
    Hit,
    Miss,
}

impl ChaTransactionResult {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChaTransactionLabel(&'static str);

impl ChaTransactionLabel {
    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }

    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

impl serde::Serialize for ChaTransactionLabel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChaEventKind {
    EvictionClockticks,
    EvictionInsert,
    EvictionOccupancy,
    HaRequest(ChaHaRequestLocality, ChaRequestOperation),
    LlcLookup(ChaCacheState, ChaLookupOperation),
    LlcVictim(ChaCacheState),
    NoCredits(ChaNoCreditDirection),
    NoCreditsClockticks,
    RequestQueueClockticks(ChaRequestSource),
    RequestQueueInsert(ChaRequestSource),
    RequestQueueOccupancy(ChaRequestSource),
    RxcClockticks(ChaRxcQueue),
    RxcInsert(ChaRxcQueue),
    RxcOccupancy(ChaRxcQueue),
    SfEviction(ChaCacheState),
    TransactionClockticks(ChaTransactionLabel, ChaTransactionResult),
    TransactionInsert(ChaTransactionLabel, ChaTransactionResult),
    TransactionOccupancy(ChaTransactionLabel, ChaTransactionResult),
    Unused,
}

impl ChaEventKind {
    pub(crate) fn is_clockticks(self) -> bool {
        matches!(
            self,
            Self::EvictionClockticks
                | Self::NoCreditsClockticks
                | Self::RequestQueueClockticks(_)
                | Self::RxcClockticks(_)
                | Self::TransactionClockticks(_, _)
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChaEventMeasurement {
    pub(crate) enabled: Duration,
    pub(crate) running: Duration,
    pub(crate) represented_unit_count: u64,
    pub(crate) value: u64,
}

impl ChaEventMeasurement {
    pub(crate) fn add(&mut self, value: u64, running: Duration, represented_unit_count: u64) {
        self.running += running;
        self.represented_unit_count = represented_unit_count;
        self.value += value;
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaEvictionMetrics {
    pub bandwidth_bytes_per_second: f64,
    pub latency_seconds: f64,
    pub occupancy_entries: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaHaRequestMetrics {
    pub local_read_bytes_per_second: f64,
    pub local_read_ratio: f64,
    pub local_write_bytes_per_second: f64,
    pub local_write_ratio: f64,
    pub remote_read_bytes_per_second: f64,
    pub remote_write_bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaLlcLookupMetrics {
    pub bytes_per_second: f64,
    pub operation: ChaLookupOperation,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub state: ChaCacheState,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaLlcVictimMetrics {
    pub per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub state: ChaCacheState,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaNoCreditMetrics {
    pub direction: ChaNoCreditDirection,
    pub ratio: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaRequestQueueMetrics {
    pub occupancy_entries: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub source: ChaRequestSource,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaRxcMetrics {
    pub inserts_per_second: f64,
    pub latency_seconds: f64,
    pub occupancy_entries: f64,
    pub queue: ChaRxcQueue,
    #[serde(flatten)]
    pub scope: UncoreScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaScopeMetrics {
    pub frequency_hz: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaSfEvictionMetrics {
    pub bytes_per_second: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub state: ChaCacheState,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaTransactionMetrics {
    pub bandwidth_bytes_per_second: f64,
    pub hit_rate: f64,
    pub latency_seconds: f64,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub transaction: ChaTransactionLabel,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ChaTransactionResultMetrics {
    pub bandwidth_bytes_per_second: f64,
    pub inserts_per_second: f64,
    pub latency_seconds: f64,
    pub occupancy_entries: f64,
    pub result: ChaTransactionResult,
    #[serde(flatten)]
    pub scope: UncoreScope,
    pub transaction: ChaTransactionLabel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChaMultiplexMode {
    #[default]
    Temporal,
    Spatial {
        partitions: usize,
    },
}

impl ChaMultiplexMode {
    pub const fn spatial(partitions: usize) -> Self {
        Self::Spatial { partitions }
    }

    pub(crate) const fn partitions(self) -> usize {
        match self {
            Self::Temporal => 1,
            Self::Spatial { partitions } => partitions,
        }
    }
}

pub(crate) fn bytes_per_second(measurement: &ChaEventMeasurement) -> f64 {
    event_rate(measurement) * BYTES_PER_CACHE_LINE
}

pub(crate) fn event_rate(measurement: &ChaEventMeasurement) -> f64 {
    events_per_second(scale_measurement_value(measurement), measurement.enabled)
}

pub(crate) fn scale_measurement_value(measurement: &ChaEventMeasurement) -> u64 {
    scale_to_enabled(measurement.value, measurement.enabled, measurement.running)
}

pub(crate) fn llc_victim_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    states: &[ChaCacheState],
) -> Result<Vec<ChaLlcVictimMetrics>, String> {
    let mut metrics = Vec::new();

    for &state in states {
        metrics.push(ChaLlcVictimMetrics {
            per_second: event_rate(required_measurement(
                measurements,
                ChaEventKind::LlcVictim(state),
            )?),
            scope,
            state,
        });
    }

    Ok(metrics)
}

pub(crate) fn required_measurement(
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    kind: ChaEventKind,
) -> Result<&ChaEventMeasurement, String> {
    measurements
        .get(&kind)
        .ok_or_else(|| format!("CHA measurement {kind:?} is missing"))
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum ChaMetrics {
    Hsx(hsx::HsxChaMetrics),
    Icx(icx::IcxChaMetrics),
    Ivb(snb::SnbChaMetrics),
    Skx(skx::SkxChaMetrics),
    Snb(snb::SnbChaMetrics),
    Emr(spr::SprChaMetrics),
    Spr(spr::SprChaMetrics),
}

#[derive(Debug)]
pub enum ChaCollector {
    Hsx(hsx::HsxChaCollector),
    Icx(icx::IcxChaCollector),
    Ivb(snb::SnbChaCollector),
    Skx(skx::SkxChaCollector),
    Snb(snb::SnbChaCollector),
    Emr(spr::SprChaCollector),
    Spr(spr::SprChaCollector),
}

impl ChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SandyBridgeEp => {
                snb::SnbChaCollector::new(architecture).map(Self::Snb)
            }
            IntelServerCpuModel::IvyTown => snb::SnbChaCollector::new(architecture).map(Self::Ivb),
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxChaCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxChaCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxChaCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::SapphireRapids => spr::SprChaCollector::new().map(Self::Spr),
            IntelServerCpuModel::EmeraldRapids => spr::SprChaCollector::new().map(Self::Emr),
            model => Err(format!("CHA collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            IntelServerCpuModel::from_family_model(architecture.family, architecture.model),
            Some(
                IntelServerCpuModel::SandyBridgeEp
                    | IntelServerCpuModel::IvyTown
                    | IntelServerCpuModel::HaswellXeon
                    | IntelServerCpuModel::BroadwellXeon
                    | IntelServerCpuModel::IceLakeXeon
                    | IntelServerCpuModel::SkylakeXeon
                    | IntelServerCpuModel::SapphireRapids
                    | IntelServerCpuModel::EmeraldRapids
            )
        )
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        match self {
            Self::Hsx(collector) => collector.set_multiplex_mode(mode),
            Self::Icx(collector) => collector.set_multiplex_mode(mode),
            Self::Ivb(collector) => collector.set_multiplex_mode(mode),
            Self::Skx(collector) => collector.set_multiplex_mode(mode),
            Self::Snb(collector) => collector.set_multiplex_mode(mode),
            Self::Emr(collector) => collector.set_multiplex_mode(mode),
            Self::Spr(collector) => collector.set_multiplex_mode(mode),
        }
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<ChaMetrics, String> {
        match self {
            Self::Hsx(collector) => collector.sample(interval).await.map(ChaMetrics::Hsx),
            Self::Icx(collector) => collector.sample(interval).await.map(ChaMetrics::Icx),
            Self::Ivb(collector) => collector.sample(interval).await.map(ChaMetrics::Ivb),
            Self::Skx(collector) => collector.sample(interval).await.map(ChaMetrics::Skx),
            Self::Snb(collector) => collector.sample(interval).await.map(ChaMetrics::Snb),
            Self::Emr(collector) => collector.sample(interval).await.map(ChaMetrics::Emr),
            Self::Spr(collector) => collector.sample(interval).await.map(ChaMetrics::Spr),
        }
    }
}

#[derive(Debug)]
pub struct ChaTask {
    collector: ChaCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl ChaTask {
    pub fn new(
        collector: ChaCollector,
        interval: Duration,
        events: mpsc::Sender<MetricEvent>,
    ) -> Self {
        Self {
            collector,
            events,
            interval,
        }
    }

    pub async fn run(mut self) {
        loop {
            match self.collector.sample(self.interval).await {
                Ok(cha) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Cha(Box::new(
                            cha,
                        )))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = self.events.send(MetricEvent::Failure(error)).await;
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum ChaPrometheusMetrics {
    Hsx(hsx::HsxChaPrometheusMetrics),
    Icx(icx::IcxChaPrometheusMetrics),
    Ivb(snb::SnbChaPrometheusMetrics),
    Skx(skx::SkxChaPrometheusMetrics),
    Snb(snb::SnbChaPrometheusMetrics),
    Emr(spr::SprChaPrometheusMetrics),
    Spr(spr::SprChaPrometheusMetrics),
}

impl ChaPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SandyBridgeEp) => {
                Some(Self::Snb(snb::SnbChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IvyTown) => {
                Some(Self::Ivb(snb::SnbChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Hsx(hsx::HsxChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IceLakeXeon) => {
                Some(Self::Icx(icx::IcxChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::SkxChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SapphireRapids) => {
                Some(Self::Spr(spr::SprChaPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::EmeraldRapids) => {
                Some(Self::Emr(spr::SprChaPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: ChaMetrics) {
        match (self, metrics) {
            (Self::Hsx(prometheus), ChaMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Icx(prometheus), ChaMetrics::Icx(metrics)) => prometheus.update(metrics),
            (Self::Ivb(prometheus), ChaMetrics::Ivb(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), ChaMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Snb(prometheus), ChaMetrics::Snb(metrics)) => prometheus.update(metrics),
            (Self::Emr(prometheus), ChaMetrics::Emr(metrics)) => prometheus.update(metrics),
            (Self::Spr(prometheus), ChaMetrics::Spr(metrics)) => prometheus.update(metrics),
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched CHA Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_all_cha_architectures() {
        assert!(ChaCollector::is_supported(&test_architecture(0x2d)));
        assert!(ChaCollector::is_supported(&test_architecture(0x3e)));
        assert!(ChaCollector::is_supported(&test_architecture(0x3f)));
        assert!(ChaCollector::is_supported(&test_architecture(0x4f)));
        assert!(ChaCollector::is_supported(&test_architecture(0x55)));
        assert!(ChaCollector::is_supported(&test_architecture(0x6a)));
        assert!(!ChaCollector::is_supported(&test_architecture(0x6c)));
        assert!(ChaCollector::is_supported(&test_architecture(0x8f)));
        assert!(ChaCollector::is_supported(&test_architecture(0xcf)));
    }

    #[test]
    fn parses_linux_uncore_cha_event_source_names() {
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cha_0", &["uncore_cha_"], 28),
            Some(0)
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cha_27", &["uncore_cha_"], 28),
            Some(27)
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cha_28", &["uncore_cha_"], 28),
            None
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_iio_0", &["uncore_cha_"], 28),
            None
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cha", &["uncore_cha_"], 28),
            None
        );
    }

    #[test]
    fn parses_linux_uncore_cbox_event_source_names() {
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cbox_0", &["uncore_cbox_"], 32),
            Some(0)
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cbox_31", &["uncore_cbox_"], 32),
            Some(31)
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cbox_32", &["uncore_cbox_"], 32),
            None
        );
        assert_eq!(
            linux_uncore_unit_id_from_event_source_name("uncore_cha_0", &["uncore_cbox_"], 32),
            None
        );
    }

    #[test]
    fn maps_cpu_to_socket_local_pci_location() {
        let locations = [
            crate::metal::pci::PciLocation {
                bus: 0,
                device: 0x1e,
                function: 3,
                group: 0,
            },
            crate::metal::pci::PciLocation {
                bus: 1,
                device: 0x1e,
                function: 3,
                group: 0,
            },
        ];

        assert_eq!(
            pci_location_for_cpu_with_local_cpus(12, &locations, "test CAPID", |location| {
                Ok(match location.bus {
                    0 => vec![0, 2, 4, 6],
                    1 => vec![1, 3, 12, 14],
                    _ => Vec::new(),
                })
            })
            .unwrap(),
            locations[1]
        );
        assert_eq!(
            pci_location_for_cpu_with_local_cpus(0, &[], "test CAPID", |_| Ok(Vec::new()))
                .unwrap_err(),
            "failed to find test CAPID PCI device"
        );
        assert_eq!(
            pci_location_for_cpu_with_local_cpus(99, &locations, "test CAPID", |_| {
                Ok(Vec::new())
            })
            .unwrap_err(),
            "failed to map CPU 99 to a test CAPID PCI device"
        );
    }

    fn test_architecture(model: u8) -> Architecture {
        Architecture {
            brand: "test".to_string(),
            family: 6,
            features: crate::arch::ArchitectureFeatures::default(),
            model,
            vendor: "GenuineIntel".to_string(),
        }
    }
}
