pub mod hsx;
pub mod icx;
pub mod skx;
pub mod snb;
pub mod spr;

use std::time::Duration;

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::mpsc;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metrics::uncore::skx::UncoreScope;
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterconnectDirection {
    Rx,
    Tx,
}

impl InterconnectDirection {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Rx => "rx",
            Self::Tx => "tx",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterconnectTrafficClass {
    Data,
    NonData,
}

impl InterconnectTrafficClass {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::NonData => "non_data",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterconnectPowerState {
    L0p,
    L1,
}

impl InterconnectPowerState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::L0p => "l0p",
            Self::L1 => "l1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct InterconnectScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub link_id: u32,
    pub package_id: u32,
}

impl InterconnectScope {
    pub(crate) const fn new(scope: UncoreScope, link_id: u32) -> Self {
        Self {
            die_group_id: scope.die_group_id,
            die_id: scope.die_id,
            link_id,
            package_id: scope.package_id,
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct InterconnectLinkMetrics {
    pub frequency_hz: f64,
    #[serde(flatten)]
    pub scope: InterconnectScope,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct InterconnectPowerStateMetrics {
    pub direction: Option<InterconnectDirection>,
    pub ratio: f64,
    #[serde(flatten)]
    pub scope: InterconnectScope,
    pub state: InterconnectPowerState,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct InterconnectTrafficMetrics {
    pub bytes_per_second: Option<f64>,
    pub direction: InterconnectDirection,
    pub flits_per_second: f64,
    #[serde(flatten)]
    pub scope: InterconnectScope,
    pub traffic: InterconnectTrafficClass,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct InterconnectQueueMetrics {
    pub direction: InterconnectDirection,
    pub inserts_per_second: f64,
    pub latency_seconds: f64,
    pub occupancy_flits: f64,
    #[serde(flatten)]
    pub scope: InterconnectScope,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum InterconnectMetrics {
    Bdx(hsx::HsxInterconnectMetrics),
    Emr(spr::SprInterconnectMetrics),
    Hsx(hsx::HsxInterconnectMetrics),
    Icx(icx::IcxInterconnectMetrics),
    Ivb(snb::SnbInterconnectMetrics),
    Skx(skx::SkxInterconnectMetrics),
    Snb(snb::SnbInterconnectMetrics),
    Spr(spr::SprInterconnectMetrics),
}

#[derive(Debug)]
pub enum InterconnectCollector {
    Bdx(hsx::HsxInterconnectCollector),
    Emr(spr::SprInterconnectCollector),
    Hsx(hsx::HsxInterconnectCollector),
    Icx(icx::IcxInterconnectCollector),
    Ivb(snb::SnbInterconnectCollector),
    Skx(skx::SkxInterconnectCollector),
    Snb(snb::SnbInterconnectCollector),
    Spr(spr::SprInterconnectCollector),
}

impl InterconnectCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SandyBridgeEp => {
                snb::SnbInterconnectCollector::new(architecture).map(Self::Snb)
            }
            IntelServerCpuModel::IvyTown => {
                snb::SnbInterconnectCollector::new(architecture).map(Self::Ivb)
            }
            IntelServerCpuModel::HaswellXeon => {
                hsx::HsxInterconnectCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxInterconnectCollector::new(architecture).map(Self::Bdx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxInterconnectCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxInterconnectCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SapphireRapids => {
                spr::SprInterconnectCollector::new(architecture).map(Self::Spr)
            }
            IntelServerCpuModel::EmeraldRapids => {
                spr::SprInterconnectCollector::new(architecture).map(Self::Emr)
            }
            model => Err(format!(
                "interconnect collection is not supported for {model:?}"
            )),
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
                    | IntelServerCpuModel::SkylakeXeon
                    | IntelServerCpuModel::IceLakeXeon
                    | IntelServerCpuModel::SapphireRapids
                    | IntelServerCpuModel::EmeraldRapids
            )
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<InterconnectMetrics, String> {
        match self {
            Self::Bdx(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Bdx),
            Self::Emr(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Emr),
            Self::Hsx(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Hsx),
            Self::Icx(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Icx),
            Self::Ivb(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Ivb),
            Self::Skx(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Skx),
            Self::Snb(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Snb),
            Self::Spr(collector) => collector
                .sample(interval)
                .await
                .map(InterconnectMetrics::Spr),
        }
    }
}

#[derive(Debug)]
pub struct InterconnectTask {
    collector: InterconnectCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl InterconnectTask {
    pub fn new(
        collector: InterconnectCollector,
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
                Ok(interconnect) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Interconnect(
                            Box::new(interconnect),
                        ))))
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
pub enum InterconnectPrometheusMetrics {
    Bdx(InterconnectPrometheusFamilies),
    Emr(InterconnectPrometheusFamilies),
    Hsx(InterconnectPrometheusFamilies),
    Icx(InterconnectPrometheusFamilies),
    Ivb(InterconnectPrometheusFamilies),
    Skx(InterconnectPrometheusFamilies),
    Snb(InterconnectPrometheusFamilies),
    Spr(InterconnectPrometheusFamilies),
}

impl InterconnectPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        let mut families = || InterconnectPrometheusFamilies::register(registry);

        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SandyBridgeEp) => Some(Self::Snb(families())),
            Some(IntelServerCpuModel::IvyTown) => Some(Self::Ivb(families())),
            Some(IntelServerCpuModel::HaswellXeon) => Some(Self::Hsx(families())),
            Some(IntelServerCpuModel::BroadwellXeon) => Some(Self::Bdx(families())),
            Some(IntelServerCpuModel::SkylakeXeon) => Some(Self::Skx(families())),
            Some(IntelServerCpuModel::IceLakeXeon) => Some(Self::Icx(families())),
            Some(IntelServerCpuModel::SapphireRapids) => Some(Self::Spr(families())),
            Some(IntelServerCpuModel::EmeraldRapids) => Some(Self::Emr(families())),
            _ => None,
        }
    }

    pub fn update(&self, metrics: InterconnectMetrics) {
        match (self, metrics) {
            (Self::Bdx(prometheus), InterconnectMetrics::Bdx(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Emr(prometheus), InterconnectMetrics::Emr(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Hsx(prometheus), InterconnectMetrics::Hsx(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Icx(prometheus), InterconnectMetrics::Icx(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Ivb(prometheus), InterconnectMetrics::Ivb(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Skx(prometheus), InterconnectMetrics::Skx(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Snb(prometheus), InterconnectMetrics::Snb(metrics)) => {
                prometheus.update(metrics)
            }
            (Self::Spr(prometheus), InterconnectMetrics::Spr(metrics)) => {
                prometheus.update(metrics)
            }
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched interconnect Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
        }
    }
}

#[derive(Debug)]
pub struct InterconnectPrometheusFamilies {
    data_bytes_per_second:
        Family<InterconnectDirectionLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    flits_per_second: Family<InterconnectTrafficLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    frequency_hz: Family<InterconnectScopeLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    power_state_ratio:
        Family<InterconnectPowerStateLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    queue_inserts_per_second:
        Family<InterconnectDirectionLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    queue_latency_seconds:
        Family<InterconnectDirectionLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    queue_occupancy_flits:
        Family<InterconnectDirectionLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
}

impl InterconnectPrometheusFamilies {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            data_bytes_per_second: Family::default(),
            flits_per_second: Family::default(),
            frequency_hz: Family::default(),
            power_state_ratio: Family::default(),
            queue_inserts_per_second: Family::default(),
            queue_latency_seconds: Family::default(),
            queue_occupancy_flits: Family::default(),
        };

        registry.register(
            "ocellus_interconnect_data_bytes_per_second",
            "Interval-derived QPI/UPI payload data bandwidth in bytes per second",
            metrics.data_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_interconnect_flits_per_second",
            "Interval-derived QPI/UPI flits per second by direction and traffic class",
            metrics.flits_per_second.clone(),
        );
        registry.register(
            "ocellus_interconnect_frequency_hz",
            "Interval-derived QPI/UPI link-layer clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_interconnect_power_state_ratio",
            "Ratio of QPI/UPI link-layer cycles spent in the power state",
            metrics.power_state_ratio.clone(),
        );
        registry.register(
            "ocellus_interconnect_queue_inserts_per_second",
            "Interval-derived QPI link-layer queue inserts per second",
            metrics.queue_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_interconnect_queue_latency_seconds",
            "Interval-derived QPI link-layer queue residency latency in seconds",
            metrics.queue_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_interconnect_queue_occupancy_flits",
            "Average QPI link-layer queue occupancy in flits",
            metrics.queue_occupancy_flits.clone(),
        );

        metrics
    }

    pub fn update<M>(&self, metrics: M)
    where
        M: Into<InterconnectMetricsView>,
    {
        let metrics = metrics.into();

        for link in metrics.links {
            let scope_labels = InterconnectScopeLabels::new(link.scope);
            self.frequency_hz
                .get_or_create(&scope_labels)
                .set(link.frequency_hz);
        }

        for traffic in metrics.traffic {
            if let Some(bytes_per_second) = traffic.bytes_per_second {
                self.data_bytes_per_second
                    .get_or_create(&InterconnectDirectionLabels::new(
                        traffic.scope,
                        traffic.direction,
                    ))
                    .set(bytes_per_second);
            }
            self.flits_per_second
                .get_or_create(&InterconnectTrafficLabels::new(
                    traffic.scope,
                    traffic.direction,
                    traffic.traffic,
                ))
                .set(traffic.flits_per_second);
        }

        for power_state in metrics.power_states {
            self.power_state_ratio
                .get_or_create(&InterconnectPowerStateLabels::new(
                    power_state.scope,
                    power_state.direction,
                    power_state.state,
                ))
                .set(power_state.ratio);
        }

        for queue in metrics.queues {
            let labels = InterconnectDirectionLabels::new(queue.scope, queue.direction);
            self.queue_inserts_per_second
                .get_or_create(&labels)
                .set(queue.inserts_per_second);
            self.queue_latency_seconds
                .get_or_create(&labels)
                .set(queue.latency_seconds);
            self.queue_occupancy_flits
                .get_or_create(&labels)
                .set(queue.occupancy_flits);
        }
    }
}

#[derive(Debug)]
pub struct InterconnectMetricsView {
    links: Vec<InterconnectLinkMetrics>,
    power_states: Vec<InterconnectPowerStateMetrics>,
    queues: Vec<InterconnectQueueMetrics>,
    traffic: Vec<InterconnectTrafficMetrics>,
}

impl From<hsx::HsxInterconnectMetrics> for InterconnectMetricsView {
    fn from(metrics: hsx::HsxInterconnectMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: metrics.queues,
            traffic: metrics.traffic,
        }
    }
}

impl From<snb::SnbInterconnectMetrics> for InterconnectMetricsView {
    fn from(metrics: snb::SnbInterconnectMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: metrics.queues,
            traffic: metrics.traffic,
        }
    }
}

impl From<icx::IcxInterconnectMetrics> for InterconnectMetricsView {
    fn from(metrics: icx::IcxInterconnectMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: Vec::new(),
            traffic: metrics.traffic,
        }
    }
}

impl From<skx::SkxInterconnectMetrics> for InterconnectMetricsView {
    fn from(metrics: skx::SkxInterconnectMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: Vec::new(),
            traffic: metrics.traffic,
        }
    }
}

impl From<spr::SprInterconnectMetrics> for InterconnectMetricsView {
    fn from(metrics: spr::SprInterconnectMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: Vec::new(),
            traffic: metrics.traffic,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct InterconnectDirectionLabels {
    die: String,
    die_group: String,
    direction: String,
    link: String,
    package: String,
}

impl InterconnectDirectionLabels {
    fn new(scope: InterconnectScope, direction: InterconnectDirection) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            direction: direction.label().to_string(),
            link: scope.link_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct InterconnectPowerStateLabels {
    die: String,
    die_group: String,
    direction: String,
    link: String,
    package: String,
    state: String,
}

impl InterconnectPowerStateLabels {
    fn new(
        scope: InterconnectScope,
        direction: Option<InterconnectDirection>,
        state: InterconnectPowerState,
    ) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            direction: direction
                .map_or("link", InterconnectDirection::label)
                .to_string(),
            link: scope.link_id.to_string(),
            package: scope.package_id.to_string(),
            state: state.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct InterconnectScopeLabels {
    die: String,
    die_group: String,
    link: String,
    package: String,
}

impl InterconnectScopeLabels {
    fn new(scope: InterconnectScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            link: scope.link_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct InterconnectTrafficLabels {
    die: String,
    die_group: String,
    direction: String,
    link: String,
    package: String,
    traffic: String,
}

impl InterconnectTrafficLabels {
    fn new(
        scope: InterconnectScope,
        direction: InterconnectDirection,
        traffic: InterconnectTrafficClass,
    ) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            direction: direction.label().to_string(),
            link: scope.link_id.to_string(),
            package: scope.package_id.to_string(),
            traffic: traffic.label().to_string(),
        }
    }
}

pub(crate) fn data_bytes_per_second(flits: u64, bytes_per_flit: f64, duration: Duration) -> f64 {
    events_per_second(flits, duration) * bytes_per_flit
}

pub(crate) fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

pub(crate) fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_mainline_interconnect_architectures() {
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x2d
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x3e
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x3f
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x4f
        )));
        assert!(!InterconnectCollector::is_supported(&test_architecture(
            0x56
        )));
        assert!(!InterconnectCollector::is_supported(&test_architecture(
            0x57
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x55
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x6a
        )));
        assert!(!InterconnectCollector::is_supported(&test_architecture(
            0x6c
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0x8f
        )));
        assert!(InterconnectCollector::is_supported(&test_architecture(
            0xcf
        )));
    }

    #[test]
    fn computes_data_bandwidth_from_data_flits() {
        assert_eq!(
            data_bytes_per_second(900, 64.0 / 9.0, Duration::from_secs(1)),
            6_400.0
        );
        assert_eq!(
            data_bytes_per_second(800, 8.0, Duration::from_secs(2)),
            3_200.0
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

mod qpi_common {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use crate::arch::{Architecture, IntelServerCpuModel};
    use crate::metal;
    use crate::metal::pci::{PciBus, PciDevice};
    use crate::metal::topology::{CpuTopology, TopologyLevelKind};
    use crate::metrics::interconnect::{
        InterconnectDirection, InterconnectLinkMetrics, InterconnectPowerState,
        InterconnectPowerStateMetrics, InterconnectQueueMetrics, InterconnectScope,
        InterconnectTrafficClass, InterconnectTrafficMetrics, data_bytes_per_second,
        events_per_second, ratio,
    };
    use crate::metrics::uncore::skx::UncoreScope;

    const COUNTER_COUNT: usize = 4;
    const COUNTER_ENABLE_BIT: u32 = 1 << 22;
    const COUNTER_EVENT_EXT_BIT: u32 = 1 << 21;
    const COUNTER_RESET_BIT: u32 = 1 << 17;
    const COUNTER_WIDTH: u32 = 48;
    const QPI_COUNTER_OFFSETS: [u64; COUNTER_COUNT] = [0xa0, 0xa8, 0xb0, 0xb8];
    const QPI_CONTROL_OFFSETS: [u64; COUNTER_COUNT] = [0xd8, 0xdc, 0xe0, 0xe4];
    const QPI_DATA_BYTES_PER_FLIT: f64 = 8.0;
    const QPI_UNIT_CONTROL_OFFSET: u64 = 0xf4;
    const UBOX_GID_OFFSET: u64 = 0x54;
    const UBOX_LOCAL_NODE_ID_OFFSET: u64 = 0x40;
    const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
    const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
    const UNIT_FREEZE_BIT: u32 = 1 << 8;
    const UNIT_FREEZE_ENABLE_BIT: u32 = 1 << 16;

    const COMMON_QPI_EVENT_GROUPS: [QpiEventGroup; 3] = [
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::Clockticks, 0x14, 0x00, false),
                QpiEventSpec::new(QpiEventKind::TxDataFlits, 0x00, 0x02, false),
                QpiEventSpec::new(QpiEventKind::TxNonDataFlits, 0x00, 0x04, false),
                QpiEventSpec::new(QpiEventKind::L1PowerCycles, 0x12, 0x00, false),
            ],
        },
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::TxQueueInserts, 0x04, 0x00, false),
                QpiEventSpec::new(QpiEventKind::TxQueueOccupancy, 0x07, 0x00, false),
                QpiEventSpec::new(QpiEventKind::RxQueueInserts, 0x08, 0x00, false),
                QpiEventSpec::new(QpiEventKind::RxQueueOccupancy, 0x0b, 0x00, false),
            ],
        },
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::RxL0pPowerCycles, 0x10, 0x00, false),
                QpiEventSpec::new(QpiEventKind::TxL0pPowerCycles, 0x0d, 0x00, false),
                QpiEventSpec::unused(),
                QpiEventSpec::unused(),
            ],
        },
    ];

    const HSX_BDX_QPI_EVENT_GROUPS: [QpiEventGroup; 6] = [
        COMMON_QPI_EVENT_GROUPS[0],
        COMMON_QPI_EVENT_GROUPS[1],
        COMMON_QPI_EVENT_GROUPS[2],
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::RxDataFlits, 0x02, 0x08, true),
                QpiEventSpec::new(QpiEventKind::RxDataFlits, 0x03, 0x04, true),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x02, 0x10, true),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x03, 0x08, true),
            ],
        },
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x02, 0x01, true),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x02, 0x06, true),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x03, 0x01, true),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x03, 0x02, true),
            ],
        },
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x03, 0x10, true),
                QpiEventSpec::unused(),
                QpiEventSpec::unused(),
                QpiEventSpec::unused(),
            ],
        },
    ];

    const SNB_IVB_QPI_EVENT_GROUPS: [QpiEventGroup; 7] = [
        COMMON_QPI_EVENT_GROUPS[0],
        COMMON_QPI_EVENT_GROUPS[1],
        COMMON_QPI_EVENT_GROUPS[2],
        QpiEventGroup {
            events: [
                QpiEventSpec::new(QpiEventKind::RxDataFlits, 0x01, 0x02, false),
                QpiEventSpec::new(QpiEventKind::RxNonDataFlits, 0x01, 0x04, false),
                QpiEventSpec::unused(),
                QpiEventSpec::unused(),
            ],
        },
        HSX_BDX_QPI_EVENT_GROUPS[3],
        HSX_BDX_QPI_EVENT_GROUPS[4],
        HSX_BDX_QPI_EVENT_GROUPS[5],
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum QpiArchitecture {
        Bdx,
        Hsx,
        Ivb,
        Snb,
    }

    impl QpiArchitecture {
        fn from_model(model: IntelServerCpuModel) -> Option<Self> {
            match model {
                IntelServerCpuModel::BroadwellXeon => Some(Self::Bdx),
                IntelServerCpuModel::HaswellXeon => Some(Self::Hsx),
                IntelServerCpuModel::IvyTown => Some(Self::Ivb),
                IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
                _ => None,
            }
        }

        const fn event_groups(self) -> &'static [QpiEventGroup] {
            match self {
                Self::Bdx | Self::Hsx => &HSX_BDX_QPI_EVENT_GROUPS,
                Self::Ivb | Self::Snb => &SNB_IVB_QPI_EVENT_GROUPS,
            }
        }

        const fn link_device_ids(self) -> &'static [(u16, u32)] {
            match self {
                Self::Bdx => &[(0x6f32, 0), (0x6f33, 1), (0x6f3a, 2)],
                Self::Hsx => &[(0x2f32, 0), (0x2f33, 1), (0x2f3a, 2)],
                Self::Ivb => &[(0x0e32, 0), (0x0e33, 1), (0x0e3a, 2)],
                Self::Snb => &[(0x3c41, 0), (0x3c42, 1)],
            }
        }

        const fn name(self) -> &'static str {
            match self {
                Self::Bdx => "Broadwell Xeon",
                Self::Hsx => "Haswell Xeon",
                Self::Ivb => "Ivy Bridge-EP",
                Self::Snb => "Sandy Bridge-EP",
            }
        }

        const fn ubox_device_id(self) -> u16 {
            match self {
                Self::Bdx => 0x6f1e,
                Self::Hsx => 0x2f1e,
                Self::Ivb => 0x0e1e,
                Self::Snb => 0x3ce0,
            }
        }

        const fn unit_freeze(self) -> u32 {
            UNIT_FREEZE_BIT | self.unit_freeze_enable()
        }

        const fn unit_freeze_and_reset(self) -> u32 {
            self.unit_freeze_enable()
                | UNIT_CONTROL_RESET_BIT
                | UNIT_COUNTER_RESET_BIT
                | UNIT_FREEZE_BIT
        }

        const fn unit_freeze_enable(self) -> u32 {
            match self {
                Self::Ivb => 0,
                Self::Bdx | Self::Hsx | Self::Snb => UNIT_FREEZE_ENABLE_BIT,
            }
        }

        const fn unit_unfreeze(self) -> u32 {
            self.unit_freeze_enable()
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum QpiEventKind {
        Clockticks,
        L1PowerCycles,
        RxDataFlits,
        RxL0pPowerCycles,
        RxNonDataFlits,
        RxQueueInserts,
        RxQueueOccupancy,
        TxDataFlits,
        TxL0pPowerCycles,
        TxNonDataFlits,
        TxQueueInserts,
        TxQueueOccupancy,
        Unused,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct QpiEventSpec {
        event: u8,
        ext: bool,
        kind: QpiEventKind,
        umask: u8,
    }

    impl QpiEventSpec {
        const fn new(kind: QpiEventKind, event: u8, umask: u8, ext: bool) -> Self {
            Self {
                event,
                ext,
                kind,
                umask,
            }
        }

        const fn unused() -> Self {
            Self::new(QpiEventKind::Unused, 0, 0, false)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct QpiEventGroup {
        events: [QpiEventSpec; COUNTER_COUNT],
    }

    #[derive(Clone, Debug, serde::Serialize)]
    pub struct QpiMetrics {
        pub links: Vec<InterconnectLinkMetrics>,
        pub power_states: Vec<InterconnectPowerStateMetrics>,
        pub queues: Vec<InterconnectQueueMetrics>,
        pub traffic: Vec<InterconnectTrafficMetrics>,
    }

    impl QpiMetrics {
        fn from_measurements(
            measurements: BTreeMap<InterconnectScope, BTreeMap<QpiEventKind, QpiEventMeasurement>>,
        ) -> Result<Self, String> {
            let mut links = Vec::with_capacity(measurements.len());
            let mut power_states = Vec::new();
            let mut queues = Vec::new();
            let mut traffic = Vec::new();

            for (scope, measurements) in measurements {
                let clockticks = required_measurement(&measurements, QpiEventKind::Clockticks)?;
                links.push(InterconnectLinkMetrics {
                    frequency_hz: frequency_hz(clockticks),
                    scope,
                });

                for (kind, direction, traffic_class, bytes_per_flit) in [
                    (
                        QpiEventKind::TxDataFlits,
                        InterconnectDirection::Tx,
                        InterconnectTrafficClass::Data,
                        Some(QPI_DATA_BYTES_PER_FLIT),
                    ),
                    (
                        QpiEventKind::TxNonDataFlits,
                        InterconnectDirection::Tx,
                        InterconnectTrafficClass::NonData,
                        None,
                    ),
                    (
                        QpiEventKind::RxDataFlits,
                        InterconnectDirection::Rx,
                        InterconnectTrafficClass::Data,
                        Some(QPI_DATA_BYTES_PER_FLIT),
                    ),
                    (
                        QpiEventKind::RxNonDataFlits,
                        InterconnectDirection::Rx,
                        InterconnectTrafficClass::NonData,
                        None,
                    ),
                ] {
                    if let Some(measurement) = measurements.get(&kind) {
                        traffic.push(traffic_metric(
                            scope,
                            direction,
                            traffic_class,
                            measurement,
                            bytes_per_flit,
                        ));
                    }
                }

                for (state, direction, kind) in [
                    (
                        InterconnectPowerState::L1,
                        None,
                        QpiEventKind::L1PowerCycles,
                    ),
                    (
                        InterconnectPowerState::L0p,
                        Some(InterconnectDirection::Rx),
                        QpiEventKind::RxL0pPowerCycles,
                    ),
                    (
                        InterconnectPowerState::L0p,
                        Some(InterconnectDirection::Tx),
                        QpiEventKind::TxL0pPowerCycles,
                    ),
                ] {
                    let measurement = required_measurement(&measurements, kind)?;
                    power_states.push(InterconnectPowerStateMetrics {
                        direction,
                        ratio: ratio(
                            scale_measurement_to_enabled(measurement),
                            scale_measurement_to_enabled(clockticks),
                        ),
                        scope,
                        state,
                    });
                }

                for (direction, inserts_kind, occupancy_kind) in [
                    (
                        InterconnectDirection::Rx,
                        QpiEventKind::RxQueueInserts,
                        QpiEventKind::RxQueueOccupancy,
                    ),
                    (
                        InterconnectDirection::Tx,
                        QpiEventKind::TxQueueInserts,
                        QpiEventKind::TxQueueOccupancy,
                    ),
                ] {
                    let inserts = required_measurement(&measurements, inserts_kind)?;
                    let occupancy = required_measurement(&measurements, occupancy_kind)?;
                    queues.push(InterconnectQueueMetrics {
                        direction,
                        inserts_per_second: event_rate(inserts),
                        latency_seconds: queue_latency_seconds(occupancy, inserts, clockticks),
                        occupancy_flits: ratio(
                            scale_measurement_to_enabled(occupancy),
                            scale_measurement_to_enabled(clockticks),
                        ),
                        scope,
                    });
                }
            }

            Ok(Self {
                links,
                power_states,
                queues,
                traffic,
            })
        }
    }

    #[derive(Debug)]
    pub struct QpiCollector {
        architecture: QpiArchitecture,
        links: Vec<QpiLink>,
        next_group: usize,
    }

    impl QpiCollector {
        pub fn new(architecture: &Architecture) -> Result<Self, String> {
            let model = architecture.intel_server_model();
            let architecture = QpiArchitecture::from_model(model)
                .ok_or_else(|| format!("QPI collection is not supported for {model:?}"))?;
            let links = discover_links(architecture)?;
            probe_writable_pci(&links)?;

            Ok(Self {
                architecture,
                links,
                next_group: 0,
            })
        }

        pub async fn sample(&mut self, interval: Duration) -> Result<QpiMetrics, String> {
            if interval.is_zero() {
                return Err(format!(
                    "{} QPI measure interval must be non-zero",
                    self.architecture.name()
                ));
            }

            let event_groups = self.architecture.event_groups();
            let slice_duration = interval.div_f64(event_groups.len() as f64);
            let mut measurements = QpiMeasurementAccumulator::new();

            for offset in 0..event_groups.len() {
                let group = event_groups[(self.next_group + offset) % event_groups.len()];
                program_links(&self.links, group)?;

                let started_at = Instant::now();
                unfreeze_links(&self.links)?;
                tokio::time::sleep(slice_duration).await;
                freeze_links(&self.links)?;

                read_links(
                    &self.links,
                    QpiMeasurement {
                        enabled: interval,
                        group,
                        running: started_at.elapsed(),
                    },
                    &mut measurements,
                )?;
            }

            self.next_group = (self.next_group + 1) % event_groups.len();

            QpiMetrics::from_measurements(measurements.into_measurements())
        }
    }

    #[derive(Debug)]
    struct QpiLink {
        scope: InterconnectScope,
        unit: QpiUnit,
    }

    #[derive(Debug)]
    struct QpiUnit {
        architecture: QpiArchitecture,
        device: PciDevice,
    }

    impl QpiUnit {
        fn new(
            architecture: QpiArchitecture,
            location: metal::pci::PciLocation,
        ) -> Result<Self, String> {
            Ok(Self {
                architecture,
                device: PciDevice::open(location)?,
            })
        }

        fn freeze(&self) -> Result<(), String> {
            self.device
                .write_u32(QPI_UNIT_CONTROL_OFFSET, self.architecture.unit_freeze())
        }

        fn freeze_and_reset(&self) -> Result<(), String> {
            self.device.write_u32(
                QPI_UNIT_CONTROL_OFFSET,
                self.architecture.unit_freeze_and_reset(),
            )
        }

        fn program(&self, group: QpiEventGroup) -> Result<(), String> {
            for (counter_index, event) in group.events.into_iter().enumerate() {
                self.device
                    .write_u32(QPI_CONTROL_OFFSETS[counter_index], counter_control(event))?;
            }
            Ok(())
        }

        fn read(&self) -> Result<QpiUnitReading, String> {
            Ok(QpiUnitReading {
                counters: [
                    self.read_counter(0)?,
                    self.read_counter(1)?,
                    self.read_counter(2)?,
                    self.read_counter(3)?,
                ],
            })
        }

        fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
            self.device
                .read_u64(QPI_COUNTER_OFFSETS[counter_index])
                .map(mask_counter)
        }

        fn unfreeze(&self) -> Result<(), String> {
            self.device
                .write_u32(QPI_UNIT_CONTROL_OFFSET, self.architecture.unit_unfreeze())
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct QpiUnitReading {
        counters: [u64; COUNTER_COUNT],
    }

    #[derive(Clone, Copy, Debug)]
    struct QpiEventMeasurement {
        enabled: Duration,
        running: Duration,
        value: u64,
    }

    impl QpiEventMeasurement {
        fn add(&mut self, value: u64, _enabled: Duration, running: Duration) {
            let value = scaled_value_to_enabled(self.value, self.enabled, self.running)
                + scaled_value_to_enabled(value, self.enabled, running);
            self.running = self.enabled;
            self.value = value;
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct QpiMeasurement {
        enabled: Duration,
        group: QpiEventGroup,
        running: Duration,
    }

    #[derive(Debug, Default)]
    struct QpiMeasurementAccumulator {
        measurements: BTreeMap<InterconnectScope, BTreeMap<QpiEventKind, QpiEventMeasurement>>,
    }

    impl QpiMeasurementAccumulator {
        fn new() -> Self {
            Self::default()
        }

        fn add(
            &mut self,
            scope: InterconnectScope,
            kind: QpiEventKind,
            value: u64,
            measurement: QpiMeasurement,
        ) {
            if kind == QpiEventKind::Unused {
                return;
            }

            self.measurements
                .entry(scope)
                .or_default()
                .entry(kind)
                .and_modify(|event_measurement| {
                    event_measurement.add(value, measurement.enabled, measurement.running)
                })
                .or_insert(QpiEventMeasurement {
                    enabled: measurement.enabled,
                    running: measurement.running,
                    value,
                });
        }

        fn into_measurements(
            self,
        ) -> BTreeMap<InterconnectScope, BTreeMap<QpiEventKind, QpiEventMeasurement>> {
            self.measurements
        }
    }

    fn counter_control(event: QpiEventSpec) -> u32 {
        if event.kind == QpiEventKind::Unused {
            return 0;
        }

        let event_ext = if event.ext { COUNTER_EVENT_EXT_BIT } else { 0 };
        u32::from(event.event)
            | (u32::from(event.umask) << 8)
            | event_ext
            | COUNTER_RESET_BIT
            | COUNTER_ENABLE_BIT
    }

    fn discover_links(architecture: QpiArchitecture) -> Result<Vec<QpiLink>, String> {
        let package_bus_scopes = package_bus_scopes(architecture)?;
        let mut links = Vec::new();

        for (device_id, link_id) in architecture.link_device_ids().iter().copied() {
            for location in metal::pci::find_intel_devices_matching_device_id(device_id)? {
                let scope = package_bus_scopes
                    .iter()
                    .find(|bus_scope| bus_scope.matches(location))
                    .map(|bus_scope| bus_scope.scope)
                    .ok_or_else(|| {
                        format!(
                            "failed to map {} QPI link {location} to a CPU package",
                            architecture.name()
                        )
                    })?;

                links.push(QpiLink {
                    scope: InterconnectScope::new(scope, link_id),
                    unit: QpiUnit::new(architecture, location)?,
                });
            }
        }

        links.sort_by_key(|link| {
            (
                link.scope.package_id,
                link.scope.die_group_id,
                link.scope.die_id,
                link.scope.link_id,
            )
        });

        if links.is_empty() {
            return Err(format!(
                "failed to discover any {} QPI links",
                architecture.name()
            ));
        }

        Ok(links)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct QpiBusScope {
        bus: PciBus,
        scope: UncoreScope,
    }

    impl QpiBusScope {
        fn matches(self, location: metal::pci::PciLocation) -> bool {
            self.bus.group == location.group && self.bus.bus == location.bus
        }
    }

    fn package_bus_scopes(architecture: QpiArchitecture) -> Result<Vec<QpiBusScope>, String> {
        let mut bus_scopes = Vec::new();

        for location in
            metal::pci::find_intel_devices_matching_device_id(architecture.ubox_device_id())?
        {
            let device = PciDevice::open_readonly(location)?;
            let local_node_id = device.read_u32(UBOX_LOCAL_NODE_ID_OFFSET)? & 0x7;
            let node_mapping = device.read_u32(UBOX_GID_OFFSET)?;
            let package_id = package_id_from_node_mapping(local_node_id, node_mapping).ok_or_else(|| {
            format!(
                "failed to map {} UBox local node id {local_node_id} through node mapping 0x{node_mapping:x} at {location}",
                architecture.name()
            )
        })?;

            bus_scopes.push(QpiBusScope {
                bus: PciBus {
                    bus: location.bus,
                    group: location.group,
                },
                scope: UncoreScope {
                    die_group_id: 0,
                    die_id: 0,
                    package_id,
                },
            });
        }

        if bus_scopes.is_empty() {
            let scopes = package_scopes()?;
            let mut link_buses = Vec::<PciBus>::new();
            for (device_id, _) in architecture.link_device_ids().iter().copied() {
                for location in metal::pci::find_intel_devices_matching_device_id(device_id)? {
                    let bus = PciBus {
                        bus: location.bus,
                        group: location.group,
                    };
                    if !link_buses.contains(&bus) {
                        link_buses.push(bus);
                    }
                }
            }
            link_buses.sort_by_key(|bus| (bus.group, bus.bus));

            for (index, bus) in link_buses.into_iter().enumerate() {
                let Some(scope) = scopes.get(index).copied() else {
                    break;
                };
                bus_scopes.push(QpiBusScope { bus, scope });
            }
        }

        bus_scopes.sort_by_key(|bus_scope| bus_scope.scope.package_id);
        bus_scopes.dedup_by_key(|bus_scope| bus_scope.scope.package_id);

        if bus_scopes.is_empty() {
            return Err(format!(
                "failed to discover any {} QPI package buses",
                architecture.name()
            ));
        }

        Ok(bus_scopes)
    }

    fn package_id_from_node_mapping(local_node_id: u32, node_mapping: u32) -> Option<u32> {
        (0..8).find(|package_id| ((node_mapping >> (package_id * 3)) & 0x7) == local_node_id)
    }

    fn package_scopes() -> Result<Vec<UncoreScope>, String> {
        let mut scopes = BTreeMap::new();

        for topology in metal::topology::cpu_topologies()? {
            scopes
                .entry(uncore_scope_from_topology(&topology)?)
                .or_insert(topology.cpu);
        }

        if scopes.is_empty() {
            return Err("failed to discover any QPI CPU package scopes".to_string());
        }

        Ok(scopes.into_keys().collect())
    }

    fn uncore_scope_from_topology(topology: &CpuTopology) -> Result<UncoreScope, String> {
        Ok(UncoreScope {
            die_group_id: topology.level_id(TopologyLevelKind::DieGroup).unwrap_or(0),
            die_id: topology.level_id(TopologyLevelKind::Die).unwrap_or(0),
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }

    fn event_rate(measurement: &QpiEventMeasurement) -> f64 {
        events_per_second(
            scale_measurement_to_enabled(measurement),
            measurement.enabled,
        )
    }

    fn frequency_hz(measurement: &QpiEventMeasurement) -> f64 {
        events_per_second(measurement.value, measurement.running)
    }

    fn freeze_links(links: &[QpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.freeze()?;
        }
        Ok(())
    }

    fn mask_counter(counter: u64) -> u64 {
        counter & ((1_u64 << COUNTER_WIDTH) - 1)
    }

    fn probe_writable_pci(links: &[QpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.freeze()?;
        }
        Ok(())
    }

    fn program_links(links: &[QpiLink], group: QpiEventGroup) -> Result<(), String> {
        for link in links {
            link.unit.freeze_and_reset()?;
        }
        for link in links {
            link.unit.program(group)?;
        }
        Ok(())
    }

    fn queue_latency_seconds(
        occupancy: &QpiEventMeasurement,
        inserts: &QpiEventMeasurement,
        clockticks: &QpiEventMeasurement,
    ) -> f64 {
        let occupancy = scale_measurement_to_enabled(occupancy);
        let enabled = inserts.enabled;
        let inserts = scale_measurement_to_enabled(inserts);
        let clockticks = scale_measurement_to_enabled(clockticks);
        if inserts == 0 || clockticks == 0 {
            return 0.0;
        }

        occupancy as f64 / inserts as f64 * enabled.as_secs_f64() / clockticks as f64
    }

    fn read_links(
        links: &[QpiLink],
        measurement: QpiMeasurement,
        measurements: &mut QpiMeasurementAccumulator,
    ) -> Result<(), String> {
        for link in links {
            let reading = link.unit.read()?;
            for counter_index in 0..COUNTER_COUNT {
                let event = measurement.group.events[counter_index];
                measurements.add(
                    link.scope,
                    event.kind,
                    reading.counters[counter_index],
                    measurement,
                );
            }
        }
        Ok(())
    }

    fn required_measurement(
        measurements: &BTreeMap<QpiEventKind, QpiEventMeasurement>,
        kind: QpiEventKind,
    ) -> Result<&QpiEventMeasurement, String> {
        measurements
            .get(&kind)
            .ok_or_else(|| format!("QPI measurement {kind:?} is missing"))
    }

    fn scale_measurement_to_enabled(measurement: &QpiEventMeasurement) -> u64 {
        scaled_value_to_enabled(measurement.value, measurement.enabled, measurement.running)
    }

    fn scaled_value_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
        if running.is_zero() {
            return 0;
        }

        (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
    }

    fn traffic_metric(
        scope: InterconnectScope,
        direction: InterconnectDirection,
        traffic: InterconnectTrafficClass,
        measurement: &QpiEventMeasurement,
        bytes_per_flit: Option<f64>,
    ) -> InterconnectTrafficMetrics {
        InterconnectTrafficMetrics {
            bytes_per_second: bytes_per_flit.map(|bytes_per_flit| {
                data_bytes_per_second(
                    scale_measurement_to_enabled(measurement),
                    bytes_per_flit,
                    measurement.enabled,
                )
            }),
            direction,
            flits_per_second: event_rate(measurement),
            scope,
            traffic,
        }
    }

    fn unfreeze_links(links: &[QpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.unfreeze()?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn selects_architecture_device_ids() {
            assert_eq!(
                QpiArchitecture::Snb.link_device_ids(),
                &[(0x3c41, 0), (0x3c42, 1)]
            );
            assert_eq!(
                QpiArchitecture::Ivb.link_device_ids(),
                &[(0x0e32, 0), (0x0e33, 1), (0x0e3a, 2)]
            );
            assert_eq!(
                QpiArchitecture::Hsx.link_device_ids(),
                &[(0x2f32, 0), (0x2f33, 1), (0x2f3a, 2)]
            );
            assert_eq!(
                QpiArchitecture::Bdx.link_device_ids(),
                &[(0x6f32, 0), (0x6f33, 1), (0x6f3a, 2)]
            );
        }

        #[test]
        fn sandy_and_ivy_use_g0_rx_group() {
            assert_eq!(QpiArchitecture::Snb.event_groups().len(), 7);
            assert_eq!(QpiArchitecture::Ivb.event_groups().len(), 7);
            assert_eq!(QpiArchitecture::Hsx.event_groups().len(), 6);
            assert_eq!(QpiArchitecture::Bdx.event_groups().len(), 6);
        }

        #[test]
        fn sandy_and_ivy_include_g0_and_extended_rx_flits() {
            for architecture in [QpiArchitecture::Snb, QpiArchitecture::Ivb] {
                assert!(
                    architecture
                        .event_groups()
                        .iter()
                        .flat_map(|group| group.events)
                        .any(|event| event.kind == QpiEventKind::RxDataFlits && !event.ext)
                );
                assert!(
                    architecture
                        .event_groups()
                        .iter()
                        .flat_map(|group| group.events)
                        .any(|event| event.kind == QpiEventKind::RxDataFlits && event.ext)
                );
            }
        }

        #[test]
        fn haswell_and_broadwell_rx_split_events_use_extended_select() {
            for event in HSX_BDX_QPI_EVENT_GROUPS[3..]
                .iter()
                .flat_map(|group| group.events)
                .filter(|event| event.kind != QpiEventKind::Unused)
            {
                assert!(event.ext);
            }
        }

        #[test]
        fn merged_split_events_sum_rates() {
            let mut measurement = QpiEventMeasurement {
                enabled: Duration::from_secs(4),
                running: Duration::from_secs(1),
                value: 10,
            };

            measurement.add(20, Duration::from_secs(4), Duration::from_secs(1));

            assert_eq!(event_rate(&measurement), 30.0);
        }

        #[test]
        fn ivy_box_control_drops_freeze_enable_bit() {
            assert_eq!(QpiArchitecture::Ivb.unit_freeze(), UNIT_FREEZE_BIT);
            assert_eq!(QpiArchitecture::Ivb.unit_unfreeze(), 0);
        }

        #[test]
        fn sandy_haswell_broadwell_box_control_keeps_freeze_enable_bit() {
            for architecture in [
                QpiArchitecture::Snb,
                QpiArchitecture::Hsx,
                QpiArchitecture::Bdx,
            ] {
                assert_eq!(
                    architecture.unit_freeze(),
                    UNIT_FREEZE_BIT | UNIT_FREEZE_ENABLE_BIT
                );
                assert_eq!(architecture.unit_unfreeze(), UNIT_FREEZE_ENABLE_BIT);
            }
        }

        #[test]
        fn encodes_counter_control_ext_bit_without_overflow_interrupts() {
            let clockticks = QpiEventSpec::new(QpiEventKind::Clockticks, 0x14, 0x02, false);
            let drs_data = QpiEventSpec::new(QpiEventKind::RxDataFlits, 0x02, 0x08, true);

            assert_eq!(
                counter_control(clockticks),
                0x14 | (0x02 << 8) | (1 << 17) | (1 << 22)
            );
            assert_eq!(
                counter_control(drs_data),
                0x02 | (0x08 << 8) | COUNTER_EVENT_EXT_BIT | (1 << 17) | (1 << 22)
            );
        }

        #[test]
        fn derives_link_power_and_queue_metrics() {
            let scope = InterconnectScope {
                die_group_id: 0,
                die_id: 0,
                link_id: 1,
                package_id: 0,
            };
            let mut measurements = BTreeMap::new();
            measurements.insert(
                scope,
                BTreeMap::from([
                    (QpiEventKind::Clockticks, measurement(1_000, 1)),
                    (QpiEventKind::TxDataFlits, measurement(200, 1)),
                    (QpiEventKind::TxNonDataFlits, measurement(100, 1)),
                    (QpiEventKind::RxDataFlits, measurement(300, 1)),
                    (QpiEventKind::RxNonDataFlits, measurement(150, 1)),
                    (QpiEventKind::L1PowerCycles, measurement(50, 1)),
                    (QpiEventKind::TxL0pPowerCycles, measurement(100, 1)),
                    (QpiEventKind::RxL0pPowerCycles, measurement(200, 1)),
                    (QpiEventKind::TxQueueInserts, measurement(25, 1)),
                    (QpiEventKind::TxQueueOccupancy, measurement(250, 1)),
                    (QpiEventKind::RxQueueInserts, measurement(50, 1)),
                    (QpiEventKind::RxQueueOccupancy, measurement(100, 1)),
                ]),
            );

            let metrics = QpiMetrics::from_measurements(measurements).unwrap();

            assert_eq!(metrics.links[0].frequency_hz, 1_000.0);
            assert_eq!(metrics.traffic[0].bytes_per_second, Some(1_600.0));
            assert_eq!(metrics.traffic[2].bytes_per_second, Some(2_400.0));
            assert_eq!(metrics.power_states.len(), 3);
            assert_eq!(metrics.queues.len(), 2);
            assert_eq!(metrics.queues[0].occupancy_flits, 0.1);
            assert_eq!(metrics.queues[0].latency_seconds, 0.002);
        }

        fn measurement(value: u64, seconds: u64) -> QpiEventMeasurement {
            QpiEventMeasurement {
                enabled: Duration::from_secs(seconds),
                running: Duration::from_secs(seconds),
                value,
            }
        }
    }
}

mod upi_common {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use crate::arch::{Architecture, IntelServerCpuModel};
    use crate::metal;
    use crate::metal::pci::{PciBus, PciDevice};
    use crate::metrics::interconnect::{
        InterconnectDirection, InterconnectLinkMetrics, InterconnectPowerState,
        InterconnectPowerStateMetrics, InterconnectScope, InterconnectTrafficClass,
        InterconnectTrafficMetrics, data_bytes_per_second, events_per_second, ratio,
    };
    use crate::metrics::uncore::skx::{UncoreLeader, UncoreScope, uncore_leaders};

    const COUNTER_COUNT: usize = 4;
    const COUNTER_ENABLE_BIT: u32 = 1 << 22;
    const COUNTER_RESET_BIT: u32 = 1 << 17;
    const COUNTER_WIDTH: u32 = 48;
    const GENERIC_UNIT_COUNTER_RESET_BIT: u32 = 1 << 9;
    const GENERIC_UNIT_CONTROL_RESET_BIT: u32 = 1 << 8;
    const GENERIC_UNIT_FREEZE_BIT: u32 = 1 << 0;
    const ICX_UPI_BOX_CONTROL_OFFSET: u64 = 0x318;
    const ICX_UPI_CONTROL_BASE: u64 = 0x350;
    const ICX_UPI_COUNTER_BASE: u64 = 0x320;
    const SKX_GID_OFFSET: u64 = 0xd4;
    const SKX_NODE_ID_OFFSET: u64 = 0xc0;
    const SKX_UPI_BOX_CONTROL_OFFSET: u64 = 0x378;
    const SKX_UPI_CONTROL_BASE: u64 = 0x350;
    const SKX_UPI_COUNTER_BASE: u64 = 0x318;
    const SPR_UPI_BOX_CONTROL_OFFSET: u64 = 0x318;
    const SPR_UPI_CONTROL_BASE: u64 = 0x350;
    const SPR_UPI_COUNTER_BASE: u64 = 0x320;
    const UPI_DATA_BYTES_PER_FLIT: f64 = 64.0 / 9.0;
    const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
    const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
    const UNIT_FREEZE_BIT: u32 = 1 << 8;

    const UPI_PUBLIC_EVENT_GROUPS: [UpiEventGroup; 3] = [
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::Clockticks, 0x01, 0x00),
                UpiEventSpec::new(UpiEventKind::TxDataFlits, 0x02, 0x0f),
                UpiEventSpec::new(UpiEventKind::RxDataFlits, 0x03, 0x0f),
                UpiEventSpec::new(UpiEventKind::L1PowerCycles, 0x21, 0x00),
            ],
        },
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::TxNonDataFlits, 0x02, 0x97),
                UpiEventSpec::new(UpiEventKind::RxNonDataFlits, 0x03, 0x97),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
            ],
        },
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::TxL0pPowerCycles, 0x27, 0x00),
                UpiEventSpec::new(UpiEventKind::RxL0pPowerCycles, 0x25, 0x00),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
            ],
        },
    ];

    const UPI_ICX_EVENT_GROUPS: [UpiEventGroup; 3] = [
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::Clockticks, 0x01, 0x00),
                UpiEventSpec::new(UpiEventKind::TxDataFlits, 0x02, 0x0f),
                UpiEventSpec::new(UpiEventKind::RxDataFlits, 0x03, 0x0f),
                UpiEventSpec::new(UpiEventKind::L1PowerCycles, 0x21, 0x00),
            ],
        },
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::TxNonDataFlits, 0x02, 0x97),
                UpiEventSpec::new(UpiEventKind::RxNonDataFlits, 0x03, 0x97),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
            ],
        },
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::TxL0pPowerCycles, 0x27, 0x00),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
            ],
        },
    ];

    const UPI_SPR_EVENT_GROUPS: [UpiEventGroup; 2] = [
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::Clockticks, 0x01, 0x00),
                UpiEventSpec::new(UpiEventKind::TxDataFlits, 0x02, 0x0f),
                UpiEventSpec::new(UpiEventKind::RxDataFlits, 0x03, 0x0f),
                UpiEventSpec::new(UpiEventKind::L1PowerCycles, 0x21, 0x00),
            ],
        },
        UpiEventGroup {
            events: [
                UpiEventSpec::new(UpiEventKind::TxNonDataFlits, 0x02, 0x97),
                UpiEventSpec::new(UpiEventKind::RxNonDataFlits, 0x03, 0x97),
                UpiEventSpec::unused(),
                UpiEventSpec::unused(),
            ],
        },
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum UpiArchitecture {
        Emr,
        Icx,
        Skx,
        Spr,
    }

    impl UpiArchitecture {
        fn from_model(model: IntelServerCpuModel) -> Option<Self> {
            match model {
                IntelServerCpuModel::EmeraldRapids => Some(Self::Emr),
                IntelServerCpuModel::IceLakeXeon => Some(Self::Icx),
                IntelServerCpuModel::SapphireRapids => Some(Self::Spr),
                IntelServerCpuModel::SkylakeXeon => Some(Self::Skx),
                _ => None,
            }
        }

        const fn box_control_base(self) -> u64 {
            match self {
                Self::Emr | Self::Spr => SPR_UPI_BOX_CONTROL_OFFSET,
                Self::Icx => ICX_UPI_BOX_CONTROL_OFFSET,
                Self::Skx => SKX_UPI_BOX_CONTROL_OFFSET,
            }
        }

        const fn control_base(self) -> u64 {
            match self {
                Self::Emr | Self::Spr => SPR_UPI_CONTROL_BASE,
                Self::Icx => ICX_UPI_CONTROL_BASE,
                Self::Skx => SKX_UPI_CONTROL_BASE,
            }
        }

        const fn control_stride(self) -> u64 {
            8
        }

        const fn counter_base(self) -> u64 {
            match self {
                Self::Emr | Self::Spr => SPR_UPI_COUNTER_BASE,
                Self::Icx => ICX_UPI_COUNTER_BASE,
                Self::Skx => SKX_UPI_COUNTER_BASE,
            }
        }

        const fn device_id(self) -> u16 {
            match self {
                Self::Emr | Self::Spr => 0x3241,
                Self::Icx => 0x3441,
                Self::Skx => 0x2058,
            }
        }

        const fn event_groups(self) -> &'static [UpiEventGroup] {
            match self {
                Self::Emr | Self::Spr => &UPI_SPR_EVENT_GROUPS,
                Self::Icx => &UPI_ICX_EVENT_GROUPS,
                Self::Skx => &UPI_PUBLIC_EVENT_GROUPS,
            }
        }

        const fn link_addresses(self) -> &'static [(u8, u8, u32)] {
            match self {
                Self::Emr | Self::Spr => &[(1, 1, 0), (2, 1, 1), (3, 1, 2), (4, 1, 3)],
                Self::Icx => &[(2, 1, 0), (3, 1, 1), (4, 1, 2)],
                Self::Skx => &[(14, 0, 0), (15, 0, 1), (16, 0, 2)],
            }
        }

        const fn name(self) -> &'static str {
            match self {
                Self::Emr => "Emerald Rapids",
                Self::Icx => "Ice Lake Xeon",
                Self::Skx => "Skylake/Cascade Lake",
                Self::Spr => "Sapphire Rapids",
            }
        }

        const fn unit_freeze(self) -> u32 {
            match self {
                Self::Emr | Self::Spr => GENERIC_UNIT_FREEZE_BIT,
                Self::Icx | Self::Skx => UNIT_FREEZE_BIT,
            }
        }

        const fn unit_freeze_and_reset(self) -> u32 {
            match self {
                Self::Emr | Self::Spr => {
                    GENERIC_UNIT_FREEZE_BIT
                        | GENERIC_UNIT_CONTROL_RESET_BIT
                        | GENERIC_UNIT_COUNTER_RESET_BIT
                }
                Self::Icx | Self::Skx => {
                    UNIT_FREEZE_BIT | UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT
                }
            }
        }

        const fn unit_unfreeze(self) -> u32 {
            0
        }

        const fn ubox_device_id(self) -> Option<u16> {
            match self {
                Self::Emr | Self::Spr => Some(0x3250),
                Self::Icx => Some(0x3450),
                Self::Skx => None,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum UpiEventKind {
        Clockticks,
        L1PowerCycles,
        RxDataFlits,
        RxL0pPowerCycles,
        RxNonDataFlits,
        TxDataFlits,
        TxL0pPowerCycles,
        TxNonDataFlits,
        Unused,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UpiEventSpec {
        event: u8,
        kind: UpiEventKind,
        umask: u8,
    }

    impl UpiEventSpec {
        const fn new(kind: UpiEventKind, event: u8, umask: u8) -> Self {
            Self { event, kind, umask }
        }

        const fn unused() -> Self {
            Self::new(UpiEventKind::Unused, 0, 0)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UpiEventGroup {
        events: [UpiEventSpec; COUNTER_COUNT],
    }

    #[derive(Clone, Debug, serde::Serialize)]
    pub struct UpiMetrics {
        pub links: Vec<InterconnectLinkMetrics>,
        pub power_states: Vec<InterconnectPowerStateMetrics>,
        pub traffic: Vec<InterconnectTrafficMetrics>,
    }

    impl UpiMetrics {
        fn from_measurements(
            measurements: BTreeMap<InterconnectScope, BTreeMap<UpiEventKind, UpiEventMeasurement>>,
        ) -> Result<Self, String> {
            let mut links = Vec::with_capacity(measurements.len());
            let mut power_states = Vec::new();
            let mut traffic = Vec::new();

            for (scope, measurements) in measurements {
                let clockticks = required_measurement(&measurements, UpiEventKind::Clockticks)?;
                let tx_data = required_measurement(&measurements, UpiEventKind::TxDataFlits)?;
                let tx_non_data =
                    required_measurement(&measurements, UpiEventKind::TxNonDataFlits)?;
                let rx_data = required_measurement(&measurements, UpiEventKind::RxDataFlits)?;
                let rx_non_data =
                    required_measurement(&measurements, UpiEventKind::RxNonDataFlits)?;

                links.push(InterconnectLinkMetrics {
                    frequency_hz: frequency_hz(clockticks),
                    scope,
                });
                traffic.push(traffic_metric(
                    scope,
                    InterconnectDirection::Tx,
                    InterconnectTrafficClass::Data,
                    tx_data,
                    Some(UPI_DATA_BYTES_PER_FLIT),
                ));
                traffic.push(traffic_metric(
                    scope,
                    InterconnectDirection::Tx,
                    InterconnectTrafficClass::NonData,
                    tx_non_data,
                    None,
                ));
                traffic.push(traffic_metric(
                    scope,
                    InterconnectDirection::Rx,
                    InterconnectTrafficClass::Data,
                    rx_data,
                    Some(UPI_DATA_BYTES_PER_FLIT),
                ));
                traffic.push(traffic_metric(
                    scope,
                    InterconnectDirection::Rx,
                    InterconnectTrafficClass::NonData,
                    rx_non_data,
                    None,
                ));

                let l1_cycles = required_measurement(&measurements, UpiEventKind::L1PowerCycles)?;
                power_states.push(InterconnectPowerStateMetrics {
                    direction: None,
                    ratio: ratio(
                        scale_measurement_to_enabled(l1_cycles),
                        scale_measurement_to_enabled(clockticks),
                    ),
                    scope,
                    state: InterconnectPowerState::L1,
                });

                for (direction, kind) in [
                    (InterconnectDirection::Rx, UpiEventKind::RxL0pPowerCycles),
                    (InterconnectDirection::Tx, UpiEventKind::TxL0pPowerCycles),
                ] {
                    if let Some(measurement) = measurements.get(&kind) {
                        power_states.push(InterconnectPowerStateMetrics {
                            direction: Some(direction),
                            ratio: ratio(
                                scale_measurement_to_enabled(measurement),
                                scale_measurement_to_enabled(clockticks),
                            ),
                            scope,
                            state: InterconnectPowerState::L0p,
                        });
                    }
                }
            }

            Ok(Self {
                links,
                power_states,
                traffic,
            })
        }
    }

    #[derive(Debug)]
    pub struct UpiCollector {
        architecture: UpiArchitecture,
        links: Vec<UpiLink>,
        next_group: usize,
    }

    impl UpiCollector {
        pub fn new(architecture: &Architecture) -> Result<Self, String> {
            let model = architecture.intel_server_model();
            let architecture = UpiArchitecture::from_model(model)
                .ok_or_else(|| format!("UPI collection is not supported for {model:?}"))?;
            let links = discover_links(architecture)?;
            probe_writable_pci(&links)?;

            Ok(Self {
                architecture,
                links,
                next_group: 0,
            })
        }

        pub async fn sample(&mut self, interval: Duration) -> Result<UpiMetrics, String> {
            if interval.is_zero() {
                return Err(format!(
                    "{} UPI measure interval must be non-zero",
                    self.architecture.name()
                ));
            }

            let mut measurements = UpiMeasurementAccumulator::new();
            let event_groups = self.architecture.event_groups();
            let group_count = event_groups.len();
            let slice_duration = interval.div_f64(group_count as f64);

            for group_index in 0..group_count {
                let rotated_index = (self.next_group + group_index) % group_count;
                let group = event_groups[rotated_index];
                program_links(&self.links, group)?;

                let started_at = Instant::now();
                unfreeze_links(&self.links)?;
                tokio::time::sleep(slice_duration).await;
                freeze_links(&self.links)?;

                read_links(
                    &self.links,
                    UpiMeasurement {
                        enabled: interval,
                        group,
                        running: started_at.elapsed(),
                    },
                    &mut measurements,
                )?;
            }

            self.rotate_group();

            UpiMetrics::from_measurements(measurements.into_measurements())
        }

        fn rotate_group(&mut self) {
            self.next_group = (self.next_group + 1) % self.architecture.event_groups().len();
        }
    }

    #[derive(Debug)]
    struct UpiLink {
        scope: InterconnectScope,
        unit: UpiUnit,
    }

    #[derive(Debug)]
    struct UpiUnit {
        architecture: UpiArchitecture,
        box_offset: u64,
        device: PciDevice,
    }

    impl UpiUnit {
        fn new(
            architecture: UpiArchitecture,
            location: metal::pci::PciLocation,
            box_offset: u64,
        ) -> Result<Self, String> {
            Ok(Self {
                architecture,
                box_offset,
                device: PciDevice::open(location)?,
            })
        }

        fn freeze(&self) -> Result<(), String> {
            self.device
                .write_u32(self.box_control_offset(), self.architecture.unit_freeze())
        }

        fn freeze_and_reset(&self) -> Result<(), String> {
            self.device.write_u32(
                self.box_control_offset(),
                self.architecture.unit_freeze_and_reset(),
            )
        }

        fn program(&self, group: UpiEventGroup) -> Result<(), String> {
            for (counter_index, event) in group.events.into_iter().enumerate() {
                self.device.write_u32(
                    self.control_offset(counter_index),
                    counter_control(self.architecture, event),
                )?;
            }
            Ok(())
        }

        fn read(&self) -> Result<UpiUnitReading, String> {
            Ok(UpiUnitReading {
                counters: [
                    self.read_counter(0)?,
                    self.read_counter(1)?,
                    self.read_counter(2)?,
                    self.read_counter(3)?,
                ],
            })
        }

        fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
            self.device
                .read_u64(self.counter_offset(counter_index))
                .map(mask_counter)
        }

        fn unfreeze(&self) -> Result<(), String> {
            self.device
                .write_u32(self.box_control_offset(), self.architecture.unit_unfreeze())
        }

        fn box_control_offset(&self) -> u64 {
            self.box_offset + self.architecture.box_control_base()
        }

        fn control_offset(&self, counter_index: usize) -> u64 {
            self.box_offset
                + self.architecture.control_base()
                + counter_index as u64 * self.architecture.control_stride()
        }

        fn counter_offset(&self, counter_index: usize) -> u64 {
            self.box_offset + self.architecture.counter_base() + counter_index as u64 * 8
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct UpiUnitReading {
        counters: [u64; COUNTER_COUNT],
    }

    #[derive(Clone, Copy, Debug)]
    struct UpiEventMeasurement {
        enabled: Duration,
        running: Duration,
        value: u64,
    }

    impl UpiEventMeasurement {
        fn add(&mut self, value: u64, enabled: Duration, running: Duration) {
            self.enabled += enabled;
            self.running += running;
            self.value += value;
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct UpiMeasurement {
        enabled: Duration,
        group: UpiEventGroup,
        running: Duration,
    }

    #[derive(Debug, Default)]
    struct UpiMeasurementAccumulator {
        measurements: BTreeMap<InterconnectScope, BTreeMap<UpiEventKind, UpiEventMeasurement>>,
    }

    impl UpiMeasurementAccumulator {
        fn new() -> Self {
            Self::default()
        }

        fn add(
            &mut self,
            scope: InterconnectScope,
            kind: UpiEventKind,
            value: u64,
            measurement: UpiMeasurement,
        ) {
            if kind == UpiEventKind::Unused {
                return;
            }

            self.measurements
                .entry(scope)
                .or_default()
                .entry(kind)
                .and_modify(|event_measurement| {
                    event_measurement.add(value, measurement.enabled, measurement.running)
                })
                .or_insert(UpiEventMeasurement {
                    enabled: measurement.enabled,
                    running: measurement.running,
                    value,
                });
        }

        fn into_measurements(
            self,
        ) -> BTreeMap<InterconnectScope, BTreeMap<UpiEventKind, UpiEventMeasurement>> {
            self.measurements
        }
    }

    fn counter_control(architecture: UpiArchitecture, event: UpiEventSpec) -> u32 {
        if event.kind == UpiEventKind::Unused {
            return 0;
        }

        let raw = u32::from(event.event) | (u32::from(event.umask) << 8);
        match architecture {
            UpiArchitecture::Emr | UpiArchitecture::Spr => raw,
            UpiArchitecture::Icx | UpiArchitecture::Skx => {
                raw | COUNTER_RESET_BIT | COUNTER_ENABLE_BIT
            }
        }
    }

    fn discover_links(architecture: UpiArchitecture) -> Result<Vec<UpiLink>, String> {
        let leaders = uncore_leaders()?;
        let bus_scopes = upi_bus_scopes(architecture, &leaders)?;
        let mut links = Vec::new();

        for (device, function, link_id) in architecture.link_addresses().iter().copied() {
            for bus_scope in &bus_scopes {
                let location = metal::pci::PciLocation {
                    bus: bus_scope.bus.bus,
                    device,
                    function,
                    group: bus_scope.bus.group,
                };
                let Ok(device) = PciDevice::open_readonly(location) else {
                    continue;
                };
                let Ok(vendor_device) = device.read_u32(0) else {
                    continue;
                };
                let vendor_id = vendor_device & 0xffff;
                let device_id = (vendor_device >> 16) as u16;
                if vendor_id != 0x8086 || device_id != architecture.device_id() {
                    continue;
                }

                links.push(UpiLink {
                    scope: InterconnectScope::new(bus_scope.scope, link_id),
                    unit: UpiUnit::new(architecture, location, 0)?,
                });
            }
        }

        links.sort_by_key(|link| {
            (
                link.scope.package_id,
                link.scope.die_group_id,
                link.scope.die_id,
                link.scope.link_id,
            )
        });

        if links.is_empty() {
            return Err(format!(
                "failed to discover any {} UPI links",
                architecture.name()
            ));
        }

        Ok(links)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UpiBusScope {
        bus: PciBus,
        scope: UncoreScope,
    }

    fn upi_bus_scopes(
        architecture: UpiArchitecture,
        leaders: &[UncoreLeader],
    ) -> Result<Vec<UpiBusScope>, String> {
        let mut bus_scopes = Vec::new();

        if let Some(ubox_device_id) = architecture.ubox_device_id() {
            for location in metal::pci::find_intel_devices_matching_device_id(ubox_device_id)? {
                let device = PciDevice::open_readonly(location)?;
                let local_node_id = device.read_u32(SKX_NODE_ID_OFFSET)? & 0x7;
                let node_mapping = device.read_u32(SKX_GID_OFFSET)?;
                let logical_id =
                logical_id_from_node_mapping(local_node_id, node_mapping).ok_or_else(|| {
                    format!(
                        "failed to map {} UBox local node id {local_node_id} through node mapping 0x{node_mapping:x} at {location}",
                        architecture.name()
                    )
                })?;
                let scope = leader_scope_for_logical_id(leaders, logical_id).ok_or_else(|| {
                    format!(
                        "failed to map {} UBox logical id {logical_id} to a CPU topology scope",
                        architecture.name()
                    )
                })?;

                bus_scopes.push(UpiBusScope {
                    bus: PciBus {
                        bus: location.bus,
                        group: location.group,
                    },
                    scope,
                });
            }
        }

        if bus_scopes.is_empty() {
            let mut upi_buses = Vec::<PciBus>::new();
            for location in
                metal::pci::find_intel_devices_matching_device_id(architecture.device_id())?
            {
                let bus = PciBus {
                    bus: location.bus,
                    group: location.group,
                };
                if !upi_buses.contains(&bus) {
                    upi_buses.push(bus);
                }
            }
            upi_buses.sort_by_key(|bus| (bus.group, bus.bus));

            for (index, bus) in upi_buses.into_iter().enumerate() {
                let Some(leader) = leaders.get(index) else {
                    break;
                };
                bus_scopes.push(UpiBusScope {
                    bus,
                    scope: leader.scope,
                });
            }
        }

        bus_scopes.sort_by_key(|bus_scope| {
            (
                bus_scope.scope.package_id,
                bus_scope.scope.die_group_id,
                bus_scope.scope.die_id,
                bus_scope.bus.group,
                bus_scope.bus.bus,
            )
        });
        bus_scopes.dedup_by_key(|bus_scope| {
            (
                bus_scope.scope.package_id,
                bus_scope.scope.die_group_id,
                bus_scope.scope.die_id,
            )
        });

        if bus_scopes.is_empty() {
            return Err(format!(
                "failed to discover any {} UPI package buses",
                architecture.name()
            ));
        }

        Ok(bus_scopes)
    }

    fn logical_id_from_node_mapping(local_node_id: u32, node_mapping: u32) -> Option<u32> {
        (0..8).find(|logical_id| ((node_mapping >> (logical_id * 3)) & 0x7) == local_node_id)
    }

    fn leader_scope_for_logical_id(
        leaders: &[UncoreLeader],
        logical_id: u32,
    ) -> Option<UncoreScope> {
        leaders
            .iter()
            .map(|leader| leader.scope)
            .find(|scope| scope.die_id == logical_id)
            .or_else(|| {
                leaders
                    .iter()
                    .map(|leader| leader.scope)
                    .find(|scope| scope.package_id == logical_id)
            })
    }

    fn event_rate(measurement: &UpiEventMeasurement) -> f64 {
        events_per_second(
            scale_measurement_to_enabled(measurement),
            measurement.enabled,
        )
    }

    fn frequency_hz(measurement: &UpiEventMeasurement) -> f64 {
        events_per_second(measurement.value, measurement.running)
    }

    fn freeze_links(links: &[UpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.freeze()?;
        }
        Ok(())
    }

    fn mask_counter(counter: u64) -> u64 {
        counter & ((1_u64 << COUNTER_WIDTH) - 1)
    }

    fn probe_writable_pci(links: &[UpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.freeze()?;
        }
        Ok(())
    }

    fn program_links(links: &[UpiLink], group: UpiEventGroup) -> Result<(), String> {
        for link in links {
            link.unit.freeze_and_reset()?;
        }
        for link in links {
            link.unit.program(group)?;
        }
        Ok(())
    }

    fn read_links(
        links: &[UpiLink],
        measurement: UpiMeasurement,
        measurements: &mut UpiMeasurementAccumulator,
    ) -> Result<(), String> {
        for link in links {
            let reading = link.unit.read()?;
            for counter_index in 0..COUNTER_COUNT {
                let event = measurement.group.events[counter_index];
                measurements.add(
                    link.scope,
                    event.kind,
                    reading.counters[counter_index],
                    measurement,
                );
            }
        }
        Ok(())
    }

    fn required_measurement(
        measurements: &BTreeMap<UpiEventKind, UpiEventMeasurement>,
        kind: UpiEventKind,
    ) -> Result<&UpiEventMeasurement, String> {
        measurements
            .get(&kind)
            .ok_or_else(|| format!("UPI measurement {kind:?} is missing"))
    }

    fn scale_measurement_to_enabled(measurement: &UpiEventMeasurement) -> u64 {
        if measurement.running.is_zero() {
            return 0;
        }

        (measurement.value as f64 * measurement.enabled.as_secs_f64()
            / measurement.running.as_secs_f64()) as u64
    }

    fn unfreeze_links(links: &[UpiLink]) -> Result<(), String> {
        for link in links {
            link.unit.unfreeze()?;
        }
        Ok(())
    }

    fn traffic_metric(
        scope: InterconnectScope,
        direction: InterconnectDirection,
        traffic: InterconnectTrafficClass,
        measurement: &UpiEventMeasurement,
        bytes_per_flit: Option<f64>,
    ) -> InterconnectTrafficMetrics {
        InterconnectTrafficMetrics {
            bytes_per_second: bytes_per_flit.map(|bytes_per_flit| {
                data_bytes_per_second(
                    scale_measurement_to_enabled(measurement),
                    bytes_per_flit,
                    measurement.enabled,
                )
            }),
            direction,
            flits_per_second: event_rate(measurement),
            scope,
            traffic,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn selects_architecture_register_maps() {
            assert_eq!(UpiArchitecture::Skx.device_id(), 0x2058);
            assert_eq!(UpiArchitecture::Skx.box_control_base(), 0x378);
            assert_eq!(UpiArchitecture::Icx.device_id(), 0x3441);
            assert_eq!(UpiArchitecture::Icx.box_control_base(), 0x318);
            assert_eq!(UpiArchitecture::Spr.device_id(), 0x3241);
            assert_eq!(UpiArchitecture::Emr.device_id(), 0x3241);
        }

        #[test]
        fn all_upi_architectures_use_offs8_control_stride() {
            for architecture in [
                UpiArchitecture::Skx,
                UpiArchitecture::Icx,
                UpiArchitecture::Spr,
                UpiArchitecture::Emr,
            ] {
                assert_eq!(architecture.control_stride(), 8);
            }
        }

        #[test]
        fn skx_public_events_include_rx_and_tx_l0p() {
            assert!(
                UpiArchitecture::Skx
                    .event_groups()
                    .iter()
                    .flat_map(|group| group.events)
                    .any(|event| event.kind == UpiEventKind::RxL0pPowerCycles)
            );
            assert!(
                UpiArchitecture::Skx
                    .event_groups()
                    .iter()
                    .flat_map(|group| group.events)
                    .any(|event| event.kind == UpiEventKind::TxL0pPowerCycles)
            );
        }

        #[test]
        fn icx_public_events_include_only_tx_l0p() {
            assert!(
                !UpiArchitecture::Icx
                    .event_groups()
                    .iter()
                    .flat_map(|group| group.events)
                    .any(|event| event.kind == UpiEventKind::RxL0pPowerCycles)
            );
            assert!(
                UpiArchitecture::Icx
                    .event_groups()
                    .iter()
                    .flat_map(|group| group.events)
                    .any(|event| event.kind == UpiEventKind::TxL0pPowerCycles)
            );
        }

        #[test]
        fn spr_and_emr_use_only_public_non_experimental_power_events() {
            for architecture in [UpiArchitecture::Spr, UpiArchitecture::Emr] {
                assert!(
                    !architecture
                        .event_groups()
                        .iter()
                        .flat_map(|group| group.events)
                        .any(|event| matches!(
                            event.kind,
                            UpiEventKind::RxL0pPowerCycles | UpiEventKind::TxL0pPowerCycles
                        ))
                );
            }
        }

        #[test]
        fn encodes_legacy_and_generic_counter_controls() {
            let event = UpiEventSpec::new(UpiEventKind::TxDataFlits, 0x02, 0x0f);
            assert_eq!(
                counter_control(UpiArchitecture::Skx, event),
                0x02 | (0x0f << 8) | COUNTER_RESET_BIT | COUNTER_ENABLE_BIT
            );
            assert_eq!(
                counter_control(UpiArchitecture::Spr, event),
                0x02 | (0x0f << 8)
            );
        }

        #[test]
        fn derives_upi_metrics() {
            let scope = InterconnectScope {
                die_group_id: 0,
                die_id: 0,
                link_id: 0,
                package_id: 0,
            };
            let mut measurements = BTreeMap::new();
            measurements.insert(
                scope,
                BTreeMap::from([
                    (UpiEventKind::Clockticks, measurement(1_000, 1)),
                    (UpiEventKind::TxDataFlits, measurement(900, 1)),
                    (UpiEventKind::TxNonDataFlits, measurement(100, 1)),
                    (UpiEventKind::RxDataFlits, measurement(450, 1)),
                    (UpiEventKind::RxNonDataFlits, measurement(50, 1)),
                    (UpiEventKind::L1PowerCycles, measurement(25, 1)),
                    (UpiEventKind::TxL0pPowerCycles, measurement(100, 1)),
                    (UpiEventKind::RxL0pPowerCycles, measurement(200, 1)),
                ]),
            );

            let metrics = UpiMetrics::from_measurements(measurements).unwrap();

            assert_eq!(metrics.links[0].frequency_hz, 1_000.0);
            assert_eq!(metrics.traffic[0].bytes_per_second, Some(6_400.0));
            assert_eq!(metrics.traffic[2].bytes_per_second, Some(3_200.0));
            assert_eq!(metrics.power_states.len(), 3);
        }

        fn measurement(value: u64, seconds: u64) -> UpiEventMeasurement {
            UpiEventMeasurement {
                enabled: Duration::from_secs(seconds),
                running: Duration::from_secs(seconds),
                value,
            }
        }
    }
}
