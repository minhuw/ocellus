pub mod hsx;
pub mod skx;

use std::collections::BTreeMap;
use std::time::Duration;

use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::common::BYTES_PER_CACHE_LINE;
use crate::metrics::uncore::skx::{UncoreScope, events_per_second, scale_to_enabled};
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

pub const CHA_COUNTER_COUNT: usize = 4;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaCacheState {
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
    Hit,
    Miss,
}

impl ChaTransactionResult {
    pub(crate) fn label(self) -> &'static str {
        match self {
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
) -> Result<Vec<ChaLlcVictimMetrics>, String> {
    let mut metrics = Vec::new();

    for state in [
        ChaCacheState::M,
        ChaCacheState::E,
        ChaCacheState::S,
        ChaCacheState::F,
    ] {
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
    Skx(skx::SkxChaMetrics),
}

#[derive(Debug)]
pub enum ChaCollector {
    Hsx(hsx::HsxChaCollector),
    Skx(skx::SkxChaCollector),
}

impl ChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxChaCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxChaCollector::new(architecture).map(Self::Skx)
            }
            model => Err(format!("CHA collection is not supported for {model:?}")),
        }
    }

    pub fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            architecture.intel_server_model(),
            IntelServerCpuModel::HaswellXeon
                | IntelServerCpuModel::BroadwellXeon
                | IntelServerCpuModel::SkylakeXeon
        )
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        match self {
            Self::Hsx(collector) => collector.set_multiplex_mode(mode),
            Self::Skx(collector) => collector.set_multiplex_mode(mode),
        }
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<ChaMetrics, String> {
        match self {
            Self::Hsx(collector) => collector.sample(interval).await.map(ChaMetrics::Hsx),
            Self::Skx(collector) => collector.sample(interval).await.map(ChaMetrics::Skx),
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
    Skx(skx::SkxChaPrometheusMetrics),
}

impl ChaPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Self {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::HaswellXeon | IntelServerCpuModel::BroadwellXeon) => {
                Self::Hsx(hsx::HsxChaPrometheusMetrics::register(registry))
            }
            _ => Self::Skx(skx::SkxChaPrometheusMetrics::register(registry)),
        }
    }

    pub fn update(&self, metrics: ChaMetrics) {
        match (self, metrics) {
            (Self::Hsx(prometheus), ChaMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), ChaMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Hsx(_), ChaMetrics::Skx(_)) | (Self::Skx(_), ChaMetrics::Hsx(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_all_cha_architectures() {
        assert!(ChaCollector::is_supported(&test_architecture(0x3f)));
        assert!(ChaCollector::is_supported(&test_architecture(0x4f)));
        assert!(ChaCollector::is_supported(&test_architecture(0x55)));
        assert!(!ChaCollector::is_supported(&test_architecture(0xcf)));
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
