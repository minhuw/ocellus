use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::msr::Msr;
use crate::metrics::pcu::{PcuClockMetrics, PcuScope, PcuScopeLabels, register_frequency};
use crate::metrics::uncore::skx::{UncoreScope, uncore_leaders, wrapping_delta};

const COUNTER_WIDTH: u32 = 48;
const PCU_COUNTER_BASE: u64 = 0x717;
const PCU_CONTROL_BASE: u64 = 0x711;
const PCU_BOX_CONTROL: u64 = 0x710;
const UNIT_COUNTER_RESET_BIT: u64 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u64 = 1 << 0;
const UNIT_FREEZE_BIT: u64 = 1 << 8;

const UNIT_FREEZE: u64 = UNIT_FREEZE_BIT;
const UNIT_FREEZE_AND_RESET: u64 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT;
const UNIT_UNFREEZE: u64 = 0;

#[derive(Clone, Debug, serde::Serialize)]
pub struct IcxPcuMetrics {
    pub clocks: Vec<PcuClockMetrics<PcuScope>>,
}

impl IcxPcuMetrics {
    fn from_readings(readings: Vec<IcxPcuReading>) -> Self {
        Self {
            clocks: readings
                .into_iter()
                .map(|reading| PcuClockMetrics {
                    frequency_hz: event_rate(reading.ticks, reading.running),
                    scope: PcuScope::from_uncore_scope(reading.scope),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct IcxPcuCollector {
    packages: Vec<IcxPcuPackage>,
}

impl IcxPcuCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        if !matches!(model, IntelServerCpuModel::IceLakeXeon) {
            return Err(format!(
                "Ice Lake PCU collection is not supported for {model:?}"
            ));
        }

        let packages = discover_packages()?;
        probe_writable_msrs(&packages)?;

        Ok(Self { packages })
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxPcuMetrics, String> {
        if interval.is_zero() {
            return Err("Ice Lake PCU measure interval must be non-zero".to_string());
        }

        program_packages(&self.packages)?;

        let started_at = Instant::now();
        unfreeze_packages(&self.packages)?;
        tokio::time::sleep(interval).await;
        freeze_packages(&self.packages)?;

        let readings = read_packages(&self.packages, started_at.elapsed())?;

        Ok(IcxPcuMetrics::from_readings(readings))
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxPcuUnit {
    cpu: u32,
}

impl IcxPcuUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE)
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(UNIT_FREEZE_AND_RESET)
    }

    fn program(self) -> Result<(), String> {
        Msr::open(self.cpu)?.write(PCU_CONTROL_BASE, counter_control(0x00))
    }

    fn read(self) -> Result<u64, String> {
        Ok(mask_counter(
            Msr::open_readonly(self.cpu)?.read(PCU_COUNTER_BASE)?,
        ))
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(UNIT_UNFREEZE)
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(PCU_BOX_CONTROL, value)
    }

    fn probe_writable(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        msr.write(PCU_BOX_CONTROL, UNIT_FREEZE)?;
        msr.write(PCU_CONTROL_BASE, 0)?;
        Ok(())
    }
}

#[derive(Debug)]
struct IcxPcuPackage {
    scope: UncoreScope,
    unit: IcxPcuUnit,
}

#[derive(Clone, Copy, Debug)]
struct IcxPcuReading {
    running: Duration,
    scope: UncoreScope,
    ticks: u64,
}

#[derive(Debug)]
pub struct IcxPcuPrometheusMetrics {
    frequency: Family<PcuScopeLabels, Gauge<f64, AtomicU64>>,
}

impl IcxPcuPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        Self {
            frequency: register_frequency(registry, "Interval-derived PCU PCLK frequency in hertz"),
        }
    }

    pub fn update(&self, metrics: IcxPcuMetrics) {
        for metric in metrics.clocks {
            self.frequency
                .get_or_create(&PcuScopeLabels::new(metric.scope))
                .set(metric.frequency_hz);
        }
    }
}

fn counter_control(event: u8) -> u64 {
    u64::from(event) | (1 << 17) | (1 << 20) | (1 << 22)
}

fn discover_packages() -> Result<Vec<IcxPcuPackage>, String> {
    let packages = uncore_leaders()?
        .into_iter()
        .map(|leader| IcxPcuPackage {
            scope: leader.scope,
            unit: IcxPcuUnit { cpu: leader.cpu },
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        return Err("failed to discover any Ice Lake PCU packages".to_string());
    }

    Ok(packages)
}

fn event_rate(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();
    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

fn freeze_packages(packages: &[IcxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze()?;
    }
    Ok(())
}

fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << COUNTER_WIDTH) - 1)
}

fn probe_writable_msrs(packages: &[IcxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.probe_writable()?;
    }
    Ok(())
}

fn program_packages(packages: &[IcxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.freeze_and_reset()?;
        package.unit.program()?;
    }
    Ok(())
}

fn read_packages(
    packages: &[IcxPcuPackage],
    running: Duration,
) -> Result<Vec<IcxPcuReading>, String> {
    packages
        .iter()
        .map(|package| {
            Ok(IcxPcuReading {
                running,
                scope: package.scope,
                ticks: wrapping_delta(0, package.unit.read()?, COUNTER_WIDTH),
            })
        })
        .collect()
}

fn unfreeze_packages(packages: &[IcxPcuPackage]) -> Result<(), String> {
    for package in packages {
        package.unit.unfreeze()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_control_does_not_use_freeze_enable_bit() {
        assert_eq!(UNIT_FREEZE, UNIT_FREEZE_BIT);
        assert_eq!(
            UNIT_FREEZE_AND_RESET,
            UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT
        );
        assert_eq!(UNIT_UNFREEZE, 0);
    }

    #[test]
    fn programs_only_clockticks() {
        assert_eq!(counter_control(0x00) & 0xff, 0x00);
    }

    #[test]
    fn converts_ticks_to_frequency() {
        let metrics = IcxPcuMetrics::from_readings(vec![IcxPcuReading {
            running: Duration::from_secs(2),
            scope: UncoreScope {
                die_group_id: 0,
                die_id: 0,
                package_id: 0,
            },
            ticks: 200,
        }]);

        assert_eq!(metrics.clocks[0].frequency_hz, 100.0);
    }
}
