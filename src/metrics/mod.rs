pub mod cha;
pub(crate) mod common;
pub mod iio;
pub mod imc;
mod info;
pub mod irp;
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
    Rapl(Box<rapl::RaplMetrics>),
    Tsc(Box<tsc::TscMetrics>),
}

#[derive(Debug)]
pub struct MetricsRegistry {
    cha: cha::ChaPrometheusMetrics,
    iio: iio::IioPrometheusMetrics,
    imc: imc::ImcPrometheusMetrics,
    irp: irp::IrpPrometheusMetrics,
    rapl: rapl::RaplPrometheusMetrics,
    tsc: tsc::TscPrometheusMetrics,
}

impl MetricsRegistry {
    pub fn register(registry: &mut Registry, metadata: InfoMetadata) -> Self {
        info::register(registry, metadata.clone());

        Self {
            cha: cha::ChaPrometheusMetrics::register(registry, &metadata),
            iio: iio::IioPrometheusMetrics::register(registry),
            imc: imc::ImcPrometheusMetrics::register(registry),
            irp: irp::IrpPrometheusMetrics::register(registry, &metadata),
            rapl: rapl::RaplPrometheusMetrics::register(registry),
            tsc: tsc::TscPrometheusMetrics::register(registry),
        }
    }

    pub fn update(&self, update: MetricUpdate) {
        match update {
            MetricUpdate::Cha(cha) => self.cha.update(*cha),
            MetricUpdate::Iio(iio) => self.iio.update(*iio),
            MetricUpdate::Imc(imc) => self.imc.update(*imc),
            MetricUpdate::Irp(irp) => self.irp.update(*irp),
            MetricUpdate::Rapl(rapl) => self.rapl.update(*rapl),
            MetricUpdate::Tsc(tsc) => self.tsc.update(*tsc),
        }
    }

    #[cfg(test)]
    pub fn update_state(&self, state: MetricsState) {
        if let Some(cha) = state.cha {
            self.cha.update(cha);
        }
        if let Some(iio) = state.iio {
            self.iio.update(iio);
        }
        if let Some(imc) = state.imc {
            self.imc.update(imc);
        }
        if let Some(irp) = state.irp {
            self.irp.update(irp);
        }
        if let Some(rapl) = state.rapl {
            self.rapl.update(rapl);
        }
        if let Some(tsc) = state.tsc {
            self.tsc.update(tsc);
        }
    }
}
