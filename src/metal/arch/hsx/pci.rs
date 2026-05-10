use crate::metal::pci::{PciBus, PciDeviceSpec};

pub const HA_UNITS: [(u8, u8); 2] = [(18, 1), (18, 5)];

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
