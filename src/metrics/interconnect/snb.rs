use std::time::Duration;

use crate::arch::Architecture;
use crate::metrics::interconnect::{
    InterconnectLinkMetrics, InterconnectPowerStateMetrics, InterconnectQueueMetrics,
    InterconnectTrafficMetrics,
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnbInterconnectMetrics {
    pub links: Vec<InterconnectLinkMetrics>,
    pub power_states: Vec<InterconnectPowerStateMetrics>,
    pub queues: Vec<InterconnectQueueMetrics>,
    pub traffic: Vec<InterconnectTrafficMetrics>,
}

impl From<super::qpi_common::QpiMetrics> for SnbInterconnectMetrics {
    fn from(metrics: super::qpi_common::QpiMetrics) -> Self {
        Self {
            links: metrics.links,
            power_states: metrics.power_states,
            queues: metrics.queues,
            traffic: metrics.traffic,
        }
    }
}

#[derive(Debug)]
pub struct SnbInterconnectCollector {
    inner: super::qpi_common::QpiCollector,
}

impl SnbInterconnectCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        super::qpi_common::QpiCollector::new(architecture).map(|inner| Self { inner })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SnbInterconnectMetrics, String> {
        self.inner.sample(interval).await.map(Into::into)
    }
}
