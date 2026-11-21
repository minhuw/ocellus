use crate::arch::IntelServerCpuModel;
use crate::metal::pci::{PciBus, PciDeviceSpec, PciLocation};
use crate::metal::topology::TopologyLevelKind;
use crate::metal::ubox::{self, UboxNodeMap, UboxSpec};

pub const HA_UNITS: [(u8, u8); 2] = [(18, 1), (18, 5)];
pub const IRP_DEVICE: u8 = 5;
pub const IRP_FUNCTION: u8 = 6;

const BROADWELL_IRP_DEVICE_ID: u16 = 0x6f39;
const HASWELL_IRP_DEVICE_ID: u16 = 0x2f39;

pub const HASWELL_IMC_CHANNELS: [PciDeviceSpec; 8] = [
    PciDeviceSpec {
        device: 20,
        function: 0,
        device_id: 0x2fb0,
    },
    PciDeviceSpec {
        device: 20,
        function: 1,
        device_id: 0x2fb1,
    },
    PciDeviceSpec {
        device: 21,
        function: 0,
        device_id: 0x2fb4,
    },
    PciDeviceSpec {
        device: 21,
        function: 1,
        device_id: 0x2fb5,
    },
    PciDeviceSpec {
        device: 23,
        function: 0,
        device_id: 0x2fd0,
    },
    PciDeviceSpec {
        device: 23,
        function: 1,
        device_id: 0x2fd1,
    },
    PciDeviceSpec {
        device: 24,
        function: 0,
        device_id: 0x2fd4,
    },
    PciDeviceSpec {
        device: 24,
        function: 1,
        device_id: 0x2fd5,
    },
];

pub const BROADWELL_IMC_CHANNELS: [PciDeviceSpec; 8] = [
    PciDeviceSpec {
        device: 20,
        function: 0,
        device_id: 0x6fb0,
    },
    PciDeviceSpec {
        device: 20,
        function: 1,
        device_id: 0x6fb1,
    },
    PciDeviceSpec {
        device: 21,
        function: 0,
        device_id: 0x6fb4,
    },
    PciDeviceSpec {
        device: 21,
        function: 1,
        device_id: 0x6fb5,
    },
    PciDeviceSpec {
        device: 23,
        function: 0,
        device_id: 0x6fd0,
    },
    PciDeviceSpec {
        device: 23,
        function: 1,
        device_id: 0x6fd1,
    },
    PciDeviceSpec {
        device: 24,
        function: 0,
        device_id: 0x6fd4,
    },
    PciDeviceSpec {
        device: 24,
        function: 1,
        device_id: 0x6fd5,
    },
];

pub fn irp_device_id(model: IntelServerCpuModel) -> Option<u16> {
    match model {
        IntelServerCpuModel::HaswellXeon => Some(HASWELL_IRP_DEVICE_ID),
        IntelServerCpuModel::BroadwellXeon => Some(BROADWELL_IRP_DEVICE_ID),
        _ => None,
    }
}

fn ubox_spec(model: IntelServerCpuModel) -> Option<UboxSpec> {
    match model {
        IntelServerCpuModel::HaswellXeon => Some(ubox::HASWELL_UBOX),
        IntelServerCpuModel::BroadwellXeon => Some(ubox::BROADWELL_UBOX),
        _ => None,
    }
}

pub fn irp_locations(model: IntelServerCpuModel) -> Result<Vec<IrpSocketLocation>, String> {
    let irp_device_id = irp_device_id(model)
        .ok_or_else(|| format!("HSX/BDX IRP collection is not supported for {model:?}"))?;
    let ubox_spec = ubox_spec(model)
        .ok_or_else(|| format!("HSX/BDX UBox mapping is not supported for {model:?}"))?;
    let irp_locations = crate::metal::pci::find_intel_devices_matching_device_id(irp_device_id)?;
    let node_map = UboxNodeMap::discover(ubox_spec, &cpu_package_ids()?)?;
    let mut locations = Vec::new();

    for ubox in node_map.uboxes() {
        let Some(location) = irp_locations.iter().copied().find(|location| {
            location.group == ubox.location.group && location.bus == ubox.location.bus
        }) else {
            continue;
        };

        if location.device != IRP_DEVICE || location.function != IRP_FUNCTION {
            return Err(format!(
                "discovered IRP device id 0x{irp_device_id:x} at unexpected PCI address {location}; expected D{IRP_DEVICE}:F{IRP_FUNCTION}"
            ));
        }

        locations.push(IrpSocketLocation {
            location,
            package_id: ubox.package_id,
        });
    }

    if locations.is_empty() {
        return Err(format!(
            "failed to discover any HSX/BDX IRP devices with device id 0x{irp_device_id:x}"
        ));
    }

    locations.sort_by_key(|location| location.package_id);
    Ok(locations)
}

/// The bus of each package's IMC channels, by package.
pub fn imc_socket_buses(
    imc_channels: &[PciDeviceSpec],
    node_map: &UboxNodeMap,
) -> Result<Vec<ImcSocketBus>, String> {
    let locations = crate::metal::pci::find_intel_devices_matching_any_spec(imc_channels)?;
    let socket_devices = node_map.socket_devices(&locations, "IMC")?;

    Ok(socket_devices
        .into_iter()
        .map(|(package_id, location)| ImcSocketBus {
            bus: PciBus {
                bus: location.bus,
                group: location.group,
            },
            package_id,
        })
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImcSocketBus {
    pub bus: PciBus,
    pub package_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrpSocketLocation {
    pub location: PciLocation,
    pub package_id: u32,
}

fn cpu_package_ids() -> Result<Vec<u32>, String> {
    crate::metal::topology::cpu_topologies()?
        .iter()
        .map(|topology| {
            topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_model_specific_irp_device_ids() {
        assert_eq!(
            irp_device_id(IntelServerCpuModel::HaswellXeon),
            Some(HASWELL_IRP_DEVICE_ID)
        );
        assert_eq!(
            irp_device_id(IntelServerCpuModel::BroadwellXeon),
            Some(BROADWELL_IRP_DEVICE_ID)
        );
        assert_eq!(irp_device_id(IntelServerCpuModel::SkylakeXeon), None);
    }
}
