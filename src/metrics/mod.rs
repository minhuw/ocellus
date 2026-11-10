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
pub mod tsc;
pub mod uncore;

use prometheus_client::registry::Registry;
use serde::Serialize;

use crate::arch::{Architecture, IntelServerCpuModel};
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
        }
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
        }
    }
}

/// What the sampler broadcasts to exporters.
#[derive(Clone, Debug)]
pub enum SamplerUpdate {
    Sample(SampledUpdate),
    Stopped(&'static str),
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

    #[cfg(test)]
    pub fn sample_sequence(&self, collector: &str) -> u64 {
        self.samples.sequence(collector)
    }

    fn update_collector(&self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => expect_registered(&self.cha, "CHA").update(*cha),
            MetricUpdate::Iio(iio) => expect_registered(&self.iio, "IIO").update(*iio),
            MetricUpdate::Imc(imc) => expect_registered(&self.imc, "IMC").update(*imc),
            MetricUpdate::Interconnect(interconnect) => {
                expect_registered(&self.interconnect, "interconnect").update(*interconnect)
            }
            MetricUpdate::Irp(irp) => expect_registered(&self.irp, "IRP").update(*irp),
            MetricUpdate::Pcu(pcu) => expect_registered(&self.pcu, "PCU").update(*pcu),
            MetricUpdate::Rapl(rapl) => expect_registered(&self.rapl, "RAPL").update(*rapl),
            MetricUpdate::Rdt(rdt) => expect_registered(&self.rdt, "RDT").update(*rdt),
            MetricUpdate::Tsc(tsc) => self.tsc.update(*tsc),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
