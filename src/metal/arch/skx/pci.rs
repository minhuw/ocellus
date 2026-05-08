use crate::metal::pci::{PciBus, PciDeviceSpec};

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

pub fn socket_buses(socket_count: usize) -> Result<Vec<SocketBus>, String> {
    let locations = crate::metal::pci::find_intel_devices(IMC_CHANNELS[0])?;

    if locations.len() < socket_count {
        return Err(format!(
            "discovered {} SKX IMC buses for {socket_count} CPU packages",
            locations.len()
        ));
    }

    locations
        .into_iter()
        .take(socket_count)
        .enumerate()
        .map(|(socket_id, location)| {
            Ok(SocketBus {
                bus: PciBus {
                    bus: location.bus,
                    group: location.group,
                },
                socket_id: u32::try_from(socket_id)
                    .map_err(|error| format!("SKX socket id is invalid: {error}"))?,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketBus {
    pub bus: PciBus,
    pub socket_id: u32,
}
