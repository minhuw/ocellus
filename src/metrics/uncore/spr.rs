use crate::metal;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};
use crate::metrics::uncore::skx::UncoreScope;

pub const UNCORE_DISCOVERY_DVSEC_ID_PMON: u16 = 1;
pub const UNCORE_EXT_CAP_ID_DISCOVERY: u16 = 0x23;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncoreDiscoveryVsec {
    pub address: u32,
    pub cap_id: u16,
    pub cap_next: u16,
    pub entry_id: u16,
    pub tbir: u8,
}

impl UncoreDiscoveryVsec {
    pub fn from_words(first: u64, second: u64) -> Self {
        Self {
            address: ((second >> 35) & ((1_u64 << 29) - 1)) as u32,
            cap_id: (first & 0xffff) as u16,
            cap_next: ((first >> 20) & 0x0fff) as u16,
            entry_id: (second & 0xffff) as u16,
            tbir: ((second >> 32) & 0x07) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncoreGlobalDiscovery {
    pub max_units: u16,
    pub stride: u8,
}

impl UncoreGlobalDiscovery {
    pub fn from_words(words: [u64; 3]) -> Self {
        Self {
            max_units: ((words[0] >> 16) & 0x03ff) as u16,
            stride: ((words[0] >> 8) & 0xff) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncoreBoxDiscovery {
    pub access_type: u8,
    pub bit_width: u8,
    pub box_control: u64,
    pub box_id: u16,
    pub box_type: u16,
    pub counter_offset: u8,
    pub control_offset: u8,
    pub num_registers: u8,
}

impl UncoreBoxDiscovery {
    pub fn from_words(words: [u64; 3]) -> Self {
        Self {
            access_type: ((words[0] >> 62) & 0x03) as u8,
            bit_width: ((words[0] >> 16) & 0xff) as u8,
            box_control: words[1],
            box_id: ((words[2] >> 16) & 0xffff) as u16,
            box_type: (words[2] & 0xffff) as u16,
            counter_offset: ((words[0] >> 24) & 0xff) as u8,
            control_offset: ((words[0] >> 8) & 0xff) as u8,
            num_registers: (words[0] & 0xff) as u8,
        }
    }

    pub fn is_valid(self) -> bool {
        self.num_registers != 0 && self.box_control != 0
    }
}

#[derive(Clone, Debug)]
pub struct UncoreDiscoverySocketBoxes {
    pub boxes: Vec<UncoreBoxDiscovery>,
    pub scope: UncoreScope,
}

pub fn discover_uncore_boxes(box_type: u16) -> Result<Vec<UncoreDiscoverySocketBoxes>, String> {
    let topologies = metal::topology::cpu_topologies()?;
    let mut sockets = Vec::new();
    for discovered_device in metal::pci::find_intel_devices()? {
        let Ok(device) = metal::pci::PciDevice::open_readonly(discovered_device.location) else {
            continue;
        };

        let mut offset = 0x100;
        loop {
            let Ok(first_word) = device.read_u64(offset) else {
                break;
            };
            if first_word == 0 {
                break;
            }
            let Ok(second_word) = device.read_u64(offset + 8) else {
                break;
            };
            let vsec = UncoreDiscoveryVsec::from_words(first_word, second_word);

            if vsec.cap_id == UNCORE_EXT_CAP_ID_DISCOVERY
                && vsec.entry_id == UNCORE_DISCOVERY_DVSEC_ID_PMON
            {
                let bar_offset = 0x10 + u64::from(vsec.tbir) * 4;
                let bar = discovery_bar(&device, bar_offset)?;
                if bar != 0 {
                    let scope = pci_device_scope(discovered_device.location, &topologies)?;
                    sockets.push(UncoreDiscoverySocketBoxes {
                        boxes: discover_uncore_boxes_from_bar(bar, box_type)?,
                        scope,
                    });
                }
            }

            let next_offset = u64::from(vsec.cap_next & !0x03);
            if next_offset == 0 || next_offset == offset {
                break;
            }
            offset = next_offset;
        }
    }

    Ok(sockets)
}

pub fn decode_discovery_bar(low: u32, high: u32) -> u64 {
    (u64::from(low) | (u64::from(high) << 32)) & !0xfff
}

fn pci_device_scope(
    location: metal::pci::PciLocation,
    topologies: &[CpuTopology],
) -> Result<UncoreScope, String> {
    let local_cpus = metal::pci::local_cpus(location)?;
    scope_from_local_cpus(&local_cpus, topologies).ok_or_else(|| {
        format!("failed to map PCI device {location} local CPUs to a CPU topology scope")
    })
}

fn scope_from_local_cpus(local_cpus: &[u32], topologies: &[CpuTopology]) -> Option<UncoreScope> {
    for cpu in local_cpus {
        if let Some(topology) = topologies.iter().find(|topology| topology.cpu == *cpu) {
            return uncore_scope_from_topology(topology).ok();
        }
    }

    None
}

fn uncore_scope_from_topology(topology: &CpuTopology) -> Result<UncoreScope, String> {
    Ok(UncoreScope {
        die_group_id: topology.level_id(TopologyLevelKind::DieGroup).unwrap_or(0),
        die_id: topology.level_id(TopologyLevelKind::Die).unwrap_or(0),
        package_id: topology
            .level_id(TopologyLevelKind::Package)
            .ok_or_else(|| "CPU topology is missing package level".to_string())?,
    })
}

fn discovery_bar(device: &metal::pci::PciDevice, offset: u64) -> Result<u64, String> {
    let low = device.read_u32(offset)?;
    let high = if low & 0x04 != 0 {
        device.read_u32(offset + 4)?
    } else {
        0
    };

    Ok(decode_discovery_bar(low, high))
}

fn discover_uncore_boxes_from_bar(
    bar: u64,
    box_type: u16,
) -> Result<Vec<UncoreBoxDiscovery>, String> {
    let mmio = metal::mmio::Mmio::open(bar)?;
    let global = UncoreGlobalDiscovery::from_words(read_discovery_words(&mmio, 0)?);
    let stride = u64::from(global.stride) * 8;
    let mut boxes = Vec::new();

    for unit_index in 0..global.max_units {
        let words = read_discovery_words(&mmio, u64::from(unit_index + 1) * stride)?;
        if words[0] == 0 && words[1] == 0 {
            continue;
        }

        let box_pmu = UncoreBoxDiscovery::from_words(words);
        if box_pmu.is_valid()
            && box_pmu.box_type == box_type
            && box_pmu.bit_width <= 64
            && box_pmu.num_registers >= 4
        {
            boxes.push(box_pmu);
        }
    }

    Ok(boxes)
}

fn read_discovery_words(mmio: &metal::mmio::Mmio, offset: u64) -> Result<[u64; 3], String> {
    Ok([
        mmio.read_u64(offset)?,
        mmio.read_u64(offset + 8)?,
        mmio.read_u64(offset + 16)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_uncore_discovery_vsec() {
        let first = u64::from(UNCORE_EXT_CAP_ID_DISCOVERY) | (0x120_u64 << 20);
        let second = UNCORE_DISCOVERY_DVSEC_ID_PMON as u64 | (5_u64 << 32) | (0x12345_u64 << 35);

        let vsec = UncoreDiscoveryVsec::from_words(first, second);

        assert_eq!(vsec.cap_id, UNCORE_EXT_CAP_ID_DISCOVERY);
        assert_eq!(vsec.cap_next, 0x120);
        assert_eq!(vsec.entry_id, UNCORE_DISCOVERY_DVSEC_ID_PMON);
        assert_eq!(vsec.tbir, 5);
        assert_eq!(vsec.address, 0x12345);
    }

    #[test]
    fn decodes_uncore_discovery_box() {
        let words = [
            4_u64 | (0x20_u64 << 8) | (48_u64 << 16) | (0x08_u64 << 24) | (1_u64 << 62),
            0x22800,
            6 | (10_u64 << 16),
        ];

        let box_pmu = UncoreBoxDiscovery::from_words(words);

        assert_eq!(box_pmu.num_registers, 4);
        assert_eq!(box_pmu.control_offset, 0x20);
        assert_eq!(box_pmu.bit_width, 48);
        assert_eq!(box_pmu.counter_offset, 0x08);
        assert_eq!(box_pmu.access_type, 1);
        assert_eq!(box_pmu.box_control, 0x22800);
        assert_eq!(box_pmu.box_type, 6);
        assert_eq!(box_pmu.box_id, 10);
    }

    #[test]
    fn decodes_64_bit_discovery_bar() {
        assert_eq!(decode_discovery_bar(0xf000_0004, 0x1234), 0x1234_f000_0000);
        assert_eq!(decode_discovery_bar(0xf000_0000, 0), 0xf000_0000);
    }
}
