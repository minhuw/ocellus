//! Runtime PMU configs. A box type's config is set through the periodic
//! collector that programs the box type's PMU, one change at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::arch::IntelServerCpuModel;
use crate::pmu_config::types::{BoxType, PmuConfig, PmuTopology};

/// The periodic collectors that can run PMU configs, and the config each
/// runs.
#[derive(Clone, Debug)]
pub struct PmuControl {
    model: IntelServerCpuModel,
    topology: PmuTopology,
    collectors: BTreeMap<BoxType, mpsc::UnboundedSender<ConfigRequest>>,
    /// The config of each configured box type. Held for a whole change, so
    /// that changes run one at a time.
    configs: Arc<Mutex<BTreeMap<BoxType, PmuConfig>>>,
}

impl PmuControl {
    pub fn new(model: IntelServerCpuModel) -> Self {
        Self {
            model,
            topology: PmuTopology::default(),
            collectors: BTreeMap::new(),
            configs: Arc::default(),
        }
    }

    /// Registers the periodic collector of `box_type`, which can program
    /// `units` and must handle every request it receives.
    pub fn register(
        &mut self,
        box_type: BoxType,
        units: BTreeMap<u16, BTreeSet<u16>>,
    ) -> ConfigRequests {
        let (sender, receiver) = mpsc::unbounded_channel();
        let previous = self.collectors.insert(box_type, sender);
        assert!(
            previous.is_none(),
            "registered two {} collectors",
            box_type.label()
        );
        if !units.is_empty() {
            self.topology.units.insert(box_type, units);
        }

        ConfigRequests { box_type, receiver }
    }

    pub fn model(&self) -> IntelServerCpuModel {
        self.model
    }

    /// The boxes the registered collectors can program.
    pub fn topology(&self) -> &PmuTopology {
        &self.topology
    }

    pub fn is_running(&self, box_type: BoxType) -> bool {
        self.collectors
            .get(&box_type)
            .is_some_and(|collector| !collector.is_closed())
    }

    /// The config of each configured box type.
    pub async fn configs(&self) -> BTreeMap<BoxType, PmuConfig> {
        self.configs.lock().await.clone()
    }

    /// Sets `configs` and returns the other configured box types to their
    /// default groups, one box type after the other. Returns once every
    /// collector has switched, or on the first that fails; the error names
    /// the box types that switched before it. The change runs to the end
    /// even if the caller stops waiting for it.
    pub async fn set(&self, configs: Vec<PmuConfig>) -> Result<(), String> {
        let control = self.clone();
        tokio::spawn(async move { control.apply(configs).await })
            .await
            .map_err(|error| format!("PMU config change failed: {error}"))?
    }

    async fn apply(&self, configs: Vec<PmuConfig>) -> Result<(), String> {
        let mut current = self.configs.lock().await;
        let mut changes = current
            .keys()
            .map(|box_type| (*box_type, None))
            .collect::<BTreeMap<_, _>>();
        for config in configs {
            changes.insert(config.program.box_type, Some(config));
        }

        let mut switched = Vec::new();
        for (box_type, config) in changes {
            let label = box_type.label();
            if let Err(error) = self.switch(box_type, config.clone()).await {
                let switched = if switched.is_empty() {
                    "none".to_string()
                } else {
                    switched.join(", ")
                };
                return Err(format!(
                    "{label}: {error} (box types already switched: {switched})"
                ));
            }
            match config {
                Some(config) => {
                    eprintln!(
                        "ocellus: {label} collector counts PMU config {}",
                        config.config_id
                    );
                    current.insert(box_type, config);
                }
                None => {
                    eprintln!("ocellus: {label} collector counts its default groups");
                    current.remove(&box_type);
                }
            }
            switched.push(label);
        }

        Ok(())
    }

    async fn switch(&self, box_type: BoxType, config: Option<PmuConfig>) -> Result<(), String> {
        let label = box_type.label();
        let collector = self
            .collectors
            .get(&box_type)
            .ok_or_else(|| format!("no {label} collector runs"))?;
        let (ack, acked) = oneshot::channel();
        collector
            .send(ConfigRequest { config, ack })
            .map_err(|_| format!("the {label} collector exited"))?;
        acked
            .await
            .map_err(|_| format!("the {label} collector exited"))?
    }
}

/// The config requests to one periodic collector.
#[derive(Debug)]
pub struct ConfigRequests {
    box_type: BoxType,
    receiver: mpsc::UnboundedReceiver<ConfigRequest>,
}

impl ConfigRequests {
    pub fn box_type(&self) -> BoxType {
        self.box_type
    }

    /// Waits for the next request; never returns once the `PmuControl` is
    /// gone.
    pub async fn next(&mut self) -> ConfigRequest {
        match self.receiver.recv().await {
            Some(request) => request,
            None => std::future::pending().await,
        }
    }
}

/// Switches a collector to `config`, or to its default groups for `None`.
#[derive(Debug)]
pub struct ConfigRequest {
    pub config: Option<PmuConfig>,
    /// The result of the switch; the collector sends it once it no longer
    /// counts the previous config.
    pub ack: oneshot::Sender<Result<(), String>>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::metrics::testing::pmu_config;

    /// Acks every request with `result`, after `delay`, and logs it.
    fn spawn_collector(
        control: &mut PmuControl,
        box_type: BoxType,
        delay: Duration,
        result: Result<(), String>,
        log: mpsc::UnboundedSender<(BoxType, Option<u64>)>,
    ) {
        let mut requests = control.register(box_type, BTreeMap::from([(0, BTreeSet::from([0]))]));
        tokio::spawn(async move {
            loop {
                let ConfigRequest { config, ack } = requests.next().await;
                tokio::time::sleep(delay).await;
                log.send((box_type, config.map(|config| config.config_id)))
                    .unwrap();
                ack.send(result.clone()).unwrap();
            }
        });
    }

    fn config_ids(configs: &BTreeMap<BoxType, PmuConfig>) -> Vec<(BoxType, u64)> {
        configs
            .iter()
            .map(|(box_type, config)| (*box_type, config.config_id))
            .collect()
    }

    #[test]
    fn builds_the_topology_from_the_registered_collectors() {
        let mut control = PmuControl::new(IntelServerCpuModel::SkylakeXeon);
        let units = BTreeMap::from([(0, BTreeSet::from([0, 1])), (1, BTreeSet::from([0, 1]))]);
        drop(control.register(BoxType::Cha, units.clone()));
        drop(control.register(BoxType::Imc, BTreeMap::new()));

        assert_eq!(
            control.topology(),
            &PmuTopology {
                units: BTreeMap::from([(BoxType::Cha, units)])
            }
        );
        assert!(!control.is_running(BoxType::Cha));
        assert!(!control.is_running(BoxType::Iio));
    }

    #[test]
    fn sets_named_box_types_and_clears_the_others() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let mut control = PmuControl::new(IntelServerCpuModel::SkylakeXeon);
            let (log_tx, mut log) = mpsc::unbounded_channel();
            for box_type in [BoxType::Cha, BoxType::Iio, BoxType::Imc] {
                spawn_collector(
                    &mut control,
                    box_type,
                    Duration::ZERO,
                    Ok(()),
                    log_tx.clone(),
                );
            }
            assert!(control.is_running(BoxType::Cha));

            control
                .set(vec![
                    pmu_config(BoxType::Cha, 1, 0),
                    pmu_config(BoxType::Iio, 2, 0),
                ])
                .await
                .unwrap();
            assert_eq!(
                config_ids(&control.configs().await),
                [(BoxType::Cha, 1), (BoxType::Iio, 2)]
            );
            assert_eq!(log.try_recv(), Ok((BoxType::Cha, Some(1))));
            assert_eq!(log.try_recv(), Ok((BoxType::Iio, Some(2))));

            control
                .set(vec![pmu_config(BoxType::Imc, 3, 0)])
                .await
                .unwrap();
            assert_eq!(config_ids(&control.configs().await), [(BoxType::Imc, 3)]);
            assert_eq!(log.try_recv(), Ok((BoxType::Cha, None)));
            assert_eq!(log.try_recv(), Ok((BoxType::Iio, None)));
            assert_eq!(log.try_recv(), Ok((BoxType::Imc, Some(3))));

            control.set(Vec::new()).await.unwrap();
            assert!(control.configs().await.is_empty());
            assert_eq!(log.try_recv(), Ok((BoxType::Imc, None)));
            assert!(log.try_recv().is_err());
        });
    }

    #[test]
    fn stops_at_the_first_collector_that_fails() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let mut control = PmuControl::new(IntelServerCpuModel::SkylakeXeon);
            let (log_tx, mut log) = mpsc::unbounded_channel();
            spawn_collector(
                &mut control,
                BoxType::Cha,
                Duration::ZERO,
                Ok(()),
                log_tx.clone(),
            );
            spawn_collector(
                &mut control,
                BoxType::Irp,
                Duration::ZERO,
                Err("boom".to_string()),
                log_tx.clone(),
            );
            spawn_collector(&mut control, BoxType::Imc, Duration::ZERO, Ok(()), log_tx);

            let error = control
                .set(vec![
                    pmu_config(BoxType::Cha, 1, 0),
                    pmu_config(BoxType::Irp, 2, 0),
                    pmu_config(BoxType::Imc, 3, 0),
                ])
                .await
                .unwrap_err();

            assert_eq!(error, "irp: boom (box types already switched: cha)");
            assert_eq!(config_ids(&control.configs().await), [(BoxType::Cha, 1)]);
            assert_eq!(log.try_recv(), Ok((BoxType::Cha, Some(1))));
            assert_eq!(log.try_recv(), Ok((BoxType::Irp, Some(2))));
            assert!(log.try_recv().is_err());

            let error = control
                .set(vec![pmu_config(BoxType::Iio, 4, 0)])
                .await
                .unwrap_err();
            assert_eq!(
                error,
                "iio: no iio collector runs (box types already switched: cha)"
            );
        });
    }

    #[test]
    fn finishes_a_change_its_caller_stopped_waiting_for() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            let mut control = PmuControl::new(IntelServerCpuModel::SkylakeXeon);
            let (log_tx, _log) = mpsc::unbounded_channel();
            spawn_collector(
                &mut control,
                BoxType::Cha,
                Duration::from_millis(100),
                Ok(()),
                log_tx,
            );

            let cancelled = tokio::time::timeout(
                Duration::from_millis(10),
                control.set(vec![pmu_config(BoxType::Cha, 1, 0)]),
            )
            .await;
            assert!(cancelled.is_err());

            // Waits for the change to finish.
            assert_eq!(config_ids(&control.configs().await), [(BoxType::Cha, 1)]);
        });
    }
}
