use std::time::Duration;

use crate::arch::Architecture;
use crate::metrics::interconnect::{
    InterconnectLinkMetrics, InterconnectPowerStateMetrics, InterconnectTrafficMetrics,
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct IcxInterconnectMetrics {
    pub links: Vec<InterconnectLinkMetrics>,
    pub power_states: Vec<InterconnectPowerStateMetrics>,
    pub traffic: Vec<InterconnectTrafficMetrics>,
}

impl From<super::upi_common::UpiMetrics> for IcxInterconnectMetrics {
    fn from(metrics: super::upi_common::UpiMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            traffic: metrics.traffic,
        }
    }
}

#[derive(Debug)]
pub struct IcxInterconnectCollector {
    inner: super::upi_common::UpiCollector,
}

impl IcxInterconnectCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        super::upi_common::UpiCollector::new(architecture).map(|inner| Self { inner })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxInterconnectMetrics, String> {
        self.inner.sample(interval).await.map(Into::into)
    }
}
