use std::collections::BTreeMap;
use std::time::Duration;

use crate::arch::IntelServerCpuModel;
use crate::metal;
use crate::metal::arch::skx::pmon;
use crate::metal::pci::PciDevice;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};

pub const COUNTER_COUNT: usize = 4;
pub const COUNTER_WIDTH: u32 = 48;
pub const CTL_OFFSETS: [u64; COUNTER_COUNT] = [0xd8, 0xdc, 0xe0, 0xe4];
pub const CTR_OFFSETS: [u64; COUNTER_COUNT] = [0xa0, 0xa8, 0xb0, 0xb8];
pub const DCLK_CTL_OFFSET: u64 = 0xf0;
pub const DCLK_CTR_OFFSET: u64 = 0xd0;
pub const UNIT_CTL_OFFSET: u64 = 0xf4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct HsxUncoreScope {
    pub package_id: u32,
}

impl HsxUncoreScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HsxUncoreSpec {
    pub imc_channels: &'static [metal::pci::PciDeviceSpec],
    pub name: &'static str,
}

impl HsxUncoreSpec {
    pub fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::HaswellXeon => Some(Self {
                imc_channels: &metal::arch::hsx::pci::HASWELL_IMC_CHANNELS,
                name: "Haswell",
            }),
            IntelServerCpuModel::BroadwellXeon => Some(Self {
                imc_channels: &metal::arch::hsx::pci::BROADWELL_IMC_CHANNELS,
                name: "Broadwell",
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct HsxUncoreUnit {
    device: PciDevice,
}

impl HsxUncoreUnit {
    pub fn new(location: metal::pci::PciLocation) -> Result<Self, String> {
        Ok(Self {
            device: PciDevice::open(location)?,
        })
    }

    pub fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32_required(UNIT_CTL_OFFSET, pmon::UNIT_FREEZE)
    }

    pub fn freeze_and_reset(&self) -> Result<(), String> {
        self.device
            .write_u32_required(UNIT_CTL_OFFSET, pmon::UNIT_FREEZE_AND_RESET)
    }

    pub fn program_counter(
        &self,
        counter_index: usize,
        event: u8,
        umask: u8,
    ) -> Result<(), String> {
        self.device.write_u32_required(
            CTL_OFFSETS[counter_index],
            pmon::counter_control(event, umask, true),
        )
    }

    pub fn read_counter(&self, counter_index: usize) -> Result<u64, String> {
        self.device
            .read_u64_required(CTR_OFFSETS[counter_index])
            .map(mask_counter)
    }

    pub fn read_fixed_counter(&self) -> Result<u64, String> {
        self.device
            .read_u64_required(DCLK_CTR_OFFSET)
            .map(mask_counter)
    }

    pub fn reset_and_enable_fixed_counter(&self) -> Result<(), String> {
        self.device
            .write_u32_required(DCLK_CTL_OFFSET, pmon::FIXED_COUNTER_RESET_AND_ENABLE)
    }

    pub fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32_required(UNIT_CTL_OFFSET, pmon::UNIT_UNFREEZE)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HsxSocketBusScope {
    pub bus: metal::pci::PciBus,
    pub scope: HsxUncoreScope,
}

pub fn average_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut count = 0;
    let mut sum = 0_u64;

    for value in values {
        count += 1;
        sum += value;
    }

    if count == 0 { 0 } else { sum / count }
}

pub fn bus_scopes(spec: HsxUncoreSpec) -> Result<Vec<HsxSocketBusScope>, String> {
    let scopes = package_scopes()?;
    let socket_buses = metal::arch::hsx::pci::imc_socket_buses(spec.imc_channels, scopes.len())?;

    if socket_buses.len() != scopes.len() {
        return Err(format!(
            "discovered {} {} IMC buses for {} CPU packages",
            socket_buses.len(),
            spec.name,
            scopes.len()
        ));
    }

    socket_buses
        .into_iter()
        .map(|socket_bus| {
            let scope = scopes
                .get(socket_bus.socket_index)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "failed to map {} IMC socket index {} to a CPUID package",
                        spec.name, socket_bus.socket_index
                    )
                })?;

            Ok(HsxSocketBusScope {
                bus: socket_bus.bus,
                scope,
            })
        })
        .collect()
}

pub fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

pub fn frequency_hz(ticks: u64, duration: Duration) -> f64 {
    events_per_second(ticks, duration)
}

pub fn mask_counter(counter: u64) -> u64 {
    counter & ((1_u64 << COUNTER_WIDTH) - 1)
}

pub fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

fn package_scopes() -> Result<Vec<HsxUncoreScope>, String> {
    let mut scopes = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        scopes
            .entry(HsxUncoreScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if scopes.is_empty() {
        return Err("failed to discover any Haswell/Broadwell uncore scopes".to_string());
    }

    Ok(scopes.into_keys().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_model_specific_imc_device_ids() {
        let haswell = HsxUncoreSpec::from_model(IntelServerCpuModel::HaswellXeon).unwrap();
        let broadwell = HsxUncoreSpec::from_model(IntelServerCpuModel::BroadwellXeon).unwrap();

        assert_eq!(
            haswell.imc_channels,
            &metal::arch::hsx::pci::HASWELL_IMC_CHANNELS
        );
        assert_eq!(
            broadwell.imc_channels,
            &metal::arch::hsx::pci::BROADWELL_IMC_CHANNELS
        );
        assert_ne!(haswell.imc_channels[0], broadwell.imc_channels[0]);
    }
}
