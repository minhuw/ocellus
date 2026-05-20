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
use crate::metrics::{InfoMetadata, MetricEvent, MetricUpdate};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PcuCoreCState {
    C0,
    C3,
    C6,
}

impl PcuCoreCState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::C0 => "c0",
            Self::C3 => "c3",
            Self::C6 => "c6",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PcuFrequencyLimitReason {
    Current,
    IoP,
    Os,
    PerfP,
    Power,
    Thermal,
}

impl PcuFrequencyLimitReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::IoP => "io_p",
            Self::Os => "os",
            Self::PerfP => "perf_p",
            Self::Power => "power",
            Self::Thermal => "thermal",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PcuThermalThrottleSource {
    ExternalProchot,
    InternalProchot,
    VrHot,
}

impl PcuThermalThrottleSource {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ExternalProchot => "external_prochot",
            Self::InternalProchot => "internal_prochot",
            Self::VrHot => "vr_hot",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct PcuScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl PcuScope {
    pub(crate) fn from_uncore_scope(scope: crate::metrics::uncore::skx::UncoreScope) -> Self {
        Self {
            die_group_id: scope.die_group_id,
            die_id: scope.die_id,
            package_id: scope.package_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct PcuPackageScope {
    pub package_id: u32,
}

impl PcuPackageScope {
    pub(crate) fn from_hsx_scope(scope: crate::metrics::uncore::hsx::HsxUncoreScope) -> Self {
        Self {
            package_id: scope.package_id,
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PcuClockMetrics<S> {
    pub frequency_hz: f64,
    #[serde(flatten)]
    pub scope: S,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PcuCoreCStateMetrics<S> {
    pub average_cores: f64,
    pub c_state: PcuCoreCState,
    #[serde(flatten)]
    pub scope: S,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PcuCycleRatioMetrics<S, K> {
    pub ratio: f64,
    #[serde(flatten)]
    pub scope: S,
    pub source: K,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PcuFrequencyLimitMetrics<S> {
    pub ratio: f64,
    pub reason: PcuFrequencyLimitReason,
    #[serde(flatten)]
    pub scope: S,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PcuPackageCStateMetrics<S> {
    pub c_state: PcuCoreCState,
    pub ratio: f64,
    #[serde(flatten)]
    pub scope: S,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "architecture")]
pub enum PcuMetrics {
    Bdx(hsx::HsxPcuMetrics),
    BdxDe(hsx::HsxPcuMetrics),
    Emr(spr::SprPcuMetrics),
    Hsx(hsx::HsxPcuMetrics),
    Icx(icx::IcxPcuMetrics),
    Ivb(snb::SnbPcuMetrics),
    Skx(skx::SkxPcuMetrics),
    Snb(snb::SnbPcuMetrics),
    Spr(spr::SprPcuMetrics),
}

#[derive(Debug)]
pub enum PcuCollector {
    Bdx(hsx::HsxPcuCollector),
    BdxDe(hsx::HsxPcuCollector),
    Emr(spr::SprPcuCollector),
    Hsx(hsx::HsxPcuCollector),
    Icx(icx::IcxPcuCollector),
    Ivb(snb::SnbPcuCollector),
    Skx(skx::SkxPcuCollector),
    Snb(snb::SnbPcuCollector),
    Spr(spr::SprPcuCollector),
}

impl PcuCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        match architecture.intel_server_model() {
            IntelServerCpuModel::SandyBridgeEp => {
                snb::SnbPcuCollector::new(architecture).map(Self::Snb)
            }
            IntelServerCpuModel::IvyTown => snb::SnbPcuCollector::new(architecture).map(Self::Ivb),
            IntelServerCpuModel::HaswellXeon => {
                hsx::HsxPcuCollector::new(architecture).map(Self::Hsx)
            }
            IntelServerCpuModel::BroadwellXeon => {
                hsx::HsxPcuCollector::new(architecture).map(Self::Bdx)
            }
            IntelServerCpuModel::BroadwellDe => {
                hsx::HsxPcuCollector::new(architecture).map(Self::BdxDe)
            }
            IntelServerCpuModel::IceLakeXeon => {
                icx::IcxPcuCollector::new(architecture).map(Self::Icx)
            }
            IntelServerCpuModel::SkylakeXeon => {
                skx::SkxPcuCollector::new(architecture).map(Self::Skx)
            }
            IntelServerCpuModel::SapphireRapids => spr::SprPcuCollector::new().map(Self::Spr),
            IntelServerCpuModel::EmeraldRapids => spr::SprPcuCollector::new().map(Self::Emr),
            model => Err(format!("PCU collection is not supported for {model:?}")),
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
                    | IntelServerCpuModel::BroadwellDe
                    | IntelServerCpuModel::SkylakeXeon
                    | IntelServerCpuModel::IceLakeXeon
                    | IntelServerCpuModel::SapphireRapids
                    | IntelServerCpuModel::EmeraldRapids
            )
        )
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<PcuMetrics, String> {
        match self {
            Self::Bdx(collector) => collector.sample(interval).await.map(PcuMetrics::Bdx),
            Self::BdxDe(collector) => collector.sample(interval).await.map(PcuMetrics::BdxDe),
            Self::Emr(collector) => collector.sample(interval).await.map(PcuMetrics::Emr),
            Self::Hsx(collector) => collector.sample(interval).await.map(PcuMetrics::Hsx),
            Self::Icx(collector) => collector.sample(interval).await.map(PcuMetrics::Icx),
            Self::Ivb(collector) => collector.sample(interval).await.map(PcuMetrics::Ivb),
            Self::Skx(collector) => collector.sample(interval).await.map(PcuMetrics::Skx),
            Self::Snb(collector) => collector.sample(interval).await.map(PcuMetrics::Snb),
            Self::Spr(collector) => collector.sample(interval).await.map(PcuMetrics::Spr),
        }
    }
}

#[derive(Debug)]
pub struct PcuTask {
    collector: PcuCollector,
    events: mpsc::Sender<MetricEvent>,
    interval: Duration,
}

impl PcuTask {
    pub fn new(
        collector: PcuCollector,
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
                Ok(pcu) => {
                    if self
                        .events
                        .send(MetricEvent::Update(Box::new(MetricUpdate::Pcu(Box::new(
                            pcu,
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
pub enum PcuPrometheusMetrics {
    Bdx(hsx::HsxPcuPrometheusMetrics),
    BdxDe(hsx::HsxPcuPrometheusMetrics),
    Emr(spr::SprPcuPrometheusMetrics),
    Hsx(hsx::HsxPcuPrometheusMetrics),
    Icx(icx::IcxPcuPrometheusMetrics),
    Ivb(snb::SnbPcuPrometheusMetrics),
    Skx(skx::SkxPcuPrometheusMetrics),
    Snb(snb::SnbPcuPrometheusMetrics),
    Spr(spr::SprPcuPrometheusMetrics),
}

impl PcuPrometheusMetrics {
    pub fn register(registry: &mut Registry, metadata: &InfoMetadata) -> Option<Self> {
        match IntelServerCpuModel::from_family_model(
            metadata.processor.family,
            metadata.processor.model,
        ) {
            Some(IntelServerCpuModel::SandyBridgeEp) => {
                Some(Self::Snb(snb::SnbPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::IvyTown) => {
                Some(Self::Ivb(snb::SnbPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::HaswellXeon) => {
                Some(Self::Hsx(hsx::HsxPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::BroadwellXeon) => {
                Some(Self::Bdx(hsx::HsxPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::BroadwellDe) => Some(Self::BdxDe(
                hsx::HsxPcuPrometheusMetrics::register(registry),
            )),
            Some(IntelServerCpuModel::IceLakeXeon) => {
                Some(Self::Icx(icx::IcxPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SkylakeXeon) => {
                Some(Self::Skx(skx::SkxPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::SapphireRapids) => {
                Some(Self::Spr(spr::SprPcuPrometheusMetrics::register(registry)))
            }
            Some(IntelServerCpuModel::EmeraldRapids) => {
                Some(Self::Emr(spr::SprPcuPrometheusMetrics::register(registry)))
            }
            _ => None,
        }
    }

    pub fn update(&self, metrics: PcuMetrics) {
        match (self, metrics) {
            (Self::Bdx(prometheus), PcuMetrics::Bdx(metrics)) => prometheus.update(metrics),
            (Self::BdxDe(prometheus), PcuMetrics::BdxDe(metrics)) => prometheus.update(metrics),
            (Self::Emr(prometheus), PcuMetrics::Emr(metrics)) => prometheus.update(metrics),
            (Self::Hsx(prometheus), PcuMetrics::Hsx(metrics)) => prometheus.update(metrics),
            (Self::Icx(prometheus), PcuMetrics::Icx(metrics)) => prometheus.update(metrics),
            (Self::Ivb(prometheus), PcuMetrics::Ivb(metrics)) => prometheus.update(metrics),
            (Self::Skx(prometheus), PcuMetrics::Skx(metrics)) => prometheus.update(metrics),
            (Self::Snb(prometheus), PcuMetrics::Snb(metrics)) => prometheus.update(metrics),
            (Self::Spr(prometheus), PcuMetrics::Spr(metrics)) => prometheus.update(metrics),
            (prometheus, metrics) => {
                debug_assert!(
                    false,
                    "mismatched PCU Prometheus updater {prometheus:?} for metrics {metrics:?}"
                );
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuPackageLabels {
    package: String,
}

impl PcuPackageLabels {
    pub(crate) fn new(scope: PcuPackageScope) -> Self {
        Self {
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl PcuScopeLabels {
    pub(crate) fn new(scope: PcuScope) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuPackageCStateLabels {
    c_state: String,
    package: String,
}

impl PcuPackageCStateLabels {
    pub(crate) fn new(scope: PcuPackageScope, c_state: PcuCoreCState) -> Self {
        Self {
            c_state: c_state.label().to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuScopeCStateLabels {
    c_state: String,
    die: String,
    die_group: String,
    package: String,
}

impl PcuScopeCStateLabels {
    pub(crate) fn new(scope: PcuScope, c_state: PcuCoreCState) -> Self {
        Self {
            c_state: c_state.label().to_string(),
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuPackageFrequencyLimitLabels {
    package: String,
    reason: String,
}

impl PcuPackageFrequencyLimitLabels {
    pub(crate) fn new(scope: PcuPackageScope, reason: PcuFrequencyLimitReason) -> Self {
        Self {
            package: scope.package_id.to_string(),
            reason: reason.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuScopeFrequencyLimitLabels {
    die: String,
    die_group: String,
    package: String,
    reason: String,
}

impl PcuScopeFrequencyLimitLabels {
    pub(crate) fn new(scope: PcuScope, reason: PcuFrequencyLimitReason) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
            reason: reason.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuPackageThermalThrottleLabels {
    package: String,
    source: String,
}

impl PcuPackageThermalThrottleLabels {
    pub(crate) fn new(scope: PcuPackageScope, source: PcuThermalThrottleSource) -> Self {
        Self {
            package: scope.package_id.to_string(),
            source: source.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
pub(crate) struct PcuScopeThermalThrottleLabels {
    die: String,
    die_group: String,
    package: String,
    source: String,
}

impl PcuScopeThermalThrottleLabels {
    pub(crate) fn new(scope: PcuScope, source: PcuThermalThrottleSource) -> Self {
        Self {
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            package: scope.package_id.to_string(),
            source: source.label().to_string(),
        }
    }
}

pub(crate) type PcuClockFamily<L> = Family<L, Gauge<f64, std::sync::atomic::AtomicU64>>;

pub(crate) fn register_frequency<L>(
    registry: &mut Registry,
    description: &'static str,
) -> PcuClockFamily<L>
where
    L: Clone
        + std::fmt::Debug
        + std::hash::Hash
        + Eq
        + Send
        + Sync
        + prometheus_client::encoding::EncodeLabelSet
        + 'static,
{
    let metric = Family::<L, Gauge<f64, std::sync::atomic::AtomicU64>>::default();
    registry.register("ocellus_pcu_frequency_hz", description, metric.clone());
    metric
}

pub(crate) fn cycle_ratio(value: u64, ticks: u64) -> f64 {
    if ticks == 0 {
        0.0
    } else {
        value as f64 / ticks as f64
    }
}

pub(crate) fn occupancy_average(value: u64, ticks: u64) -> f64 {
    cycle_ratio(value, ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_mainline_pcu_architectures() {
        assert!(PcuCollector::is_supported(&test_architecture(0x2d)));
        assert!(PcuCollector::is_supported(&test_architecture(0x3e)));
        assert!(PcuCollector::is_supported(&test_architecture(0x3f)));
        assert!(PcuCollector::is_supported(&test_architecture(0x4f)));
        assert!(PcuCollector::is_supported(&test_architecture(0x56)));
        assert!(PcuCollector::is_supported(&test_architecture(0x55)));
        assert!(!PcuCollector::is_supported(&test_architecture(0x57)));
        assert!(PcuCollector::is_supported(&test_architecture(0x6a)));
        assert!(!PcuCollector::is_supported(&test_architecture(0x6c)));
        assert!(PcuCollector::is_supported(&test_architecture(0x8f)));
        assert!(PcuCollector::is_supported(&test_architecture(0xcf)));
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
