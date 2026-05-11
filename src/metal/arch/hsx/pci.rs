use crate::arch::IntelServerCpuModel;
use crate::metal::pci::{PciBus, PciDevice, PciDeviceSpec, PciLocation};

pub const HA_UNITS: [(u8, u8); 2] = [(18, 1), (18, 5)];
pub const IRP_DEVICE: u8 = 5;
pub const IRP_FUNCTION: u8 = 6;

const BROADWELL_IRP_DEVICE_ID: u16 = 0x6f39;
const BROADWELL_UBOX_DEVICE_ID: u16 = 0x6f1e;
const HASWELL_IRP_DEVICE_ID: u16 = 0x2f39;
const HASWELL_UBOX_DEVICE_ID: u16 = 0x2f1e;
const UBOX_GID_OFFSET: u64 = 0x54;
const UBOX_LNID_OFFSET: u64 = 0x40;

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

pub fn irp_locations(model: IntelServerCpuModel) -> Result<Vec<IrpSocketLocation>, String> {
    let irp_device_id = irp_device_id(model)
        .ok_or_else(|| format!("HSX/BDX IRP collection is not supported for {model:?}"))?;
    let irp_locations = crate::metal::pci::find_intel_devices_matching_device_id(irp_device_id)?;
    let bus_map = ubox_package_bus_map(model)?;
    let mut locations = Vec::new();

    for bus_scope in bus_map {
        let Some(location) = irp_locations.iter().copied().find(|location| {
            location.group == bus_scope.bus.group && location.bus == bus_scope.bus.bus
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
            package_id: bus_scope.package_id,
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

pub fn imc_socket_buses(
    imc_channels: &[PciDeviceSpec],
    socket_count: usize,
) -> Result<Vec<ImcSocketBus>, String> {
    let locations = crate::metal::pci::find_intel_devices_matching_any_spec(imc_channels)?;
    let mut buses = Vec::<PciBus>::new();

    for location in locations {
        let bus = PciBus {
            bus: location.bus,
            group: location.group,
        };

        if !buses.contains(&bus) {
            buses.push(bus);
        }
    }

    if buses.len() < socket_count {
        return Err(format!(
            "discovered {} HSX IMC buses for {socket_count} CPU packages",
            buses.len()
        ));
    }

    Ok(buses
        .into_iter()
        .take(socket_count)
        .enumerate()
        .map(|(socket_index, bus)| ImcSocketBus { bus, socket_index })
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImcSocketBus {
    pub bus: PciBus,
    pub socket_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrpSocketLocation {
    pub location: PciLocation,
    pub package_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UboxBusScope {
    bus: PciBus,
    package_id: u32,
}

fn ubox_package_bus_map(model: IntelServerCpuModel) -> Result<Vec<UboxBusScope>, String> {
    let ubox_device_id = match model {
        IntelServerCpuModel::HaswellXeon => HASWELL_UBOX_DEVICE_ID,
        IntelServerCpuModel::BroadwellXeon => BROADWELL_UBOX_DEVICE_ID,
        _ => {
            return Err(format!(
                "HSX/BDX UBox mapping is not supported for {model:?}"
            ));
        }
    };

    let ubox_locations = crate::metal::pci::find_intel_devices_matching_device_id(ubox_device_id)?;
    let mut bus_map = Vec::new();

    for location in ubox_locations {
        let device = PciDevice::open_readonly(location)?;
        let local_node_id = device.read_u32(UBOX_LNID_OFFSET)? & 0x7;
        let node_mapping = device.read_u32(UBOX_GID_OFFSET)?;
        let package_id = package_id_from_node_mapping(local_node_id, node_mapping).ok_or_else(|| {
            format!(
                "failed to map UBox local node id {local_node_id} through node mapping 0x{node_mapping:x} at {location}"
            )
        })?;

        bus_map.push(UboxBusScope {
            bus: PciBus {
                bus: location.bus,
                group: location.group,
            },
            package_id,
        });
    }

    bus_map.sort_by_key(|bus_scope| bus_scope.package_id);
    bus_map.dedup_by_key(|bus_scope| bus_scope.package_id);
    Ok(bus_map)
}

fn package_id_from_node_mapping(local_node_id: u32, node_mapping: u32) -> Option<u32> {
    (0..8).find(|package_id| ((node_mapping >> (package_id * 3)) & 0x7) == local_node_id)
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

    #[test]
    fn maps_local_node_id_to_package_id() {
        assert_eq!(package_id_from_node_mapping(0, 0b010_001_000), Some(0));
        assert_eq!(package_id_from_node_mapping(1, 0b010_001_000), Some(1));
        assert_eq!(package_id_from_node_mapping(2, 0b010_001_000), Some(2));
        assert_eq!(package_id_from_node_mapping(3, 0b010_001_000), None);
    }
}
