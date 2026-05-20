pub mod cha;
pub(crate) mod common;
pub mod iio;
pub mod imc;
mod info;
pub mod irp;
pub mod pcu;
pub mod rapl;
pub mod tsc;
pub mod uncore;

use prometheus_client::registry::Registry;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct MetricsState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cha: Option<cha::ChaMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iio: Option<iio::IioMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imc: Option<imc::ImcMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub irp: Option<irp::IrpMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcu: Option<pcu::PcuMetrics>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rapl: Option<rapl::RaplMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsc: Option<tsc::TscMetrics>,
}

impl MetricsState {
    pub fn apply(&mut self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => self.cha = Some(*cha),
            MetricUpdate::Iio(iio) => self.iio = Some(*iio),
            MetricUpdate::Imc(imc) => self.imc = Some(*imc),
            MetricUpdate::Irp(irp) => self.irp = Some(*irp),
            MetricUpdate::Pcu(pcu) => self.pcu = Some(*pcu),
            MetricUpdate::Rapl(rapl) => self.rapl = Some(*rapl),
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
            irp: None,
            pcu: None,
            rapl: None,
            tsc: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CollectorMetadata {
    pub cha_supported: bool,
    pub iio_supported: bool,
    pub imc_supported: bool,
    pub irp_supported: bool,
    pub pcu_supported: bool,
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
    Update(Box<MetricUpdate>),
}

#[derive(Clone, Debug)]
pub enum MetricUpdate {
    Cha(Box<cha::ChaMetrics>),
    Iio(Box<iio::IioMetrics>),
    Imc(Box<imc::ImcMetrics>),
    Irp(Box<irp::IrpMetrics>),
    Pcu(Box<pcu::PcuMetrics>),
    Rapl(Box<rapl::RaplMetrics>),
    Tsc(Box<tsc::TscMetrics>),
}

#[derive(Debug)]
pub struct MetricsRegistry {
    cha: Option<cha::ChaPrometheusMetrics>,
    iio: Option<iio::IioPrometheusMetrics>,
    imc: Option<imc::ImcPrometheusMetrics>,
    irp: Option<irp::IrpPrometheusMetrics>,
    pcu: Option<pcu::PcuPrometheusMetrics>,
    rapl: Option<rapl::RaplPrometheusMetrics>,
    tsc: tsc::TscPrometheusMetrics,
}

impl MetricsRegistry {
    pub fn register(registry: &mut Registry, metadata: InfoMetadata) -> Self {
        info::register(registry, metadata.clone());

        Self {
            cha: cha::ChaPrometheusMetrics::register(registry, &metadata),
            iio: iio::IioPrometheusMetrics::register(registry, &metadata),
            imc: imc::ImcPrometheusMetrics::register(registry, &metadata),
            irp: irp::IrpPrometheusMetrics::register(registry, &metadata),
            pcu: pcu::PcuPrometheusMetrics::register(registry, &metadata),
            rapl: rapl::RaplPrometheusMetrics::register(registry, &metadata),
            tsc: tsc::TscPrometheusMetrics::register(registry),
        }
    }

    pub fn update(&self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => expect_registered(&self.cha, "CHA").update(*cha),
            MetricUpdate::Iio(iio) => expect_registered(&self.iio, "IIO").update(*iio),
            MetricUpdate::Imc(imc) => expect_registered(&self.imc, "IMC").update(*imc),
            MetricUpdate::Irp(irp) => expect_registered(&self.irp, "IRP").update(*irp),
            MetricUpdate::Pcu(pcu) => expect_registered(&self.pcu, "PCU").update(*pcu),
            MetricUpdate::Rapl(rapl) => expect_registered(&self.rapl, "RAPL").update(*rapl),
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
        if let Some(irp) = state.irp {
            expect_registered(&self.irp, "IRP").update(irp);
        }
        if let Some(pcu) = state.pcu {
            expect_registered(&self.pcu, "PCU").update(pcu);
        }
        if let Some(rapl) = state.rapl {
            expect_registered(&self.rapl, "RAPL").update(rapl);
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
