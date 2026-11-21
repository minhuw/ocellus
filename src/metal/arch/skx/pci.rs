use crate::metal::pci::{PciBus, PciDeviceSpec};
use crate::metal::ubox::UboxNodeMap;

pub const IMC_CHANNELS: [PciDeviceSpec; 6] = [
    PciDeviceSpec {
        device: 10,
        function: 2,
        device_id: 0x2042,
    },
    PciDeviceSpec {
        device: 10,
        function: 6,
        device_id: 0x2046,
    },
    PciDeviceSpec {
        device: 11,
        function: 2,
        device_id: 0x204a,
    },
    PciDeviceSpec {
        device: 12,
        function: 2,
        device_id: 0x2042,
    },
    PciDeviceSpec {
        device: 12,
        function: 6,
        device_id: 0x2046,
    },
    PciDeviceSpec {
        device: 13,
        function: 2,
        device_id: 0x204a,
    },
];

/// The bus of each package's IMC channels, by package.
pub fn imc_socket_buses(node_map: &UboxNodeMap) -> Result<Vec<ImcSocketBus>, String> {
    let locations = crate::metal::pci::find_intel_devices_matching_spec(IMC_CHANNELS[0])?;
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
