use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

const INTEL_VENDOR_ID: u16 = 0x8086;
const PCI_CONFIG_ROOT: &str = "/proc/bus/pci";
const PCI_VENDOR_DEVICE_OFFSET: u64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciBus {
    pub bus: u8,
    pub group: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciDeviceSpec {
    pub device: u8,
    pub device_id: u16,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciLocation {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub group: u16,
}

#[derive(Debug)]
pub struct PciDevice {
    file: File,
    location: PciLocation,
}

impl PciDevice {
    pub fn open(location: PciLocation) -> Result<Self, String> {
        Self::open_with_options(location, true)
    }

    pub fn read_u32(&self, offset: u64) -> io::Result<u32> {
        let mut bytes = [0_u8; 4];
        self.file.read_exact_at(&mut bytes, offset)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn read_u64(&self, offset: u64) -> io::Result<u64> {
        let mut bytes = [0_u8; 8];
        self.file.read_exact_at(&mut bytes, offset)?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub fn read_u64_required(&self, offset: u64) -> Result<u64, String> {
        self.read_u64(offset)
            .map_err(|error| format!("failed to read PCI {self} offset 0x{offset:x}: {error}"))
    }

    pub fn write_u32(&self, offset: u64, value: u32) -> io::Result<()> {
        self.file.write_all_at(&value.to_le_bytes(), offset)
    }

    pub fn write_u32_required(&self, offset: u64, value: u32) -> Result<(), String> {
        self.write_u32(offset, value).map_err(|error| {
            format!("failed to write PCI {self} offset 0x{offset:x} value 0x{value:x}: {error}")
        })
    }

    pub fn open_readonly(location: PciLocation) -> Result<Self, String> {
        Self::open_with_options(location, false)
    }

    fn open_with_options(location: PciLocation, write: bool) -> Result<Self, String> {
        let path = pci_config_path(location);
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;

        Ok(Self { file, location })
    }
}

impl fmt::Display for PciDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.location)
    }
}

impl fmt::Display for PciLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04x}:{:02x}:{:02x}.{}",
            self.group, self.bus, self.device, self.function
        )
    }
}

pub fn find_intel_device_on_bus(spec: PciDeviceSpec, bus: PciBus) -> Result<PciLocation, String> {
    let location = PciLocation {
        bus: bus.bus,
        device: spec.device,
        function: spec.function,
        group: bus.group,
    };

    if is_matching_intel_device(location, spec.device_id) {
        Ok(location)
    } else {
        Err(format!(
            "failed to find Intel PCI device {spec:?} on bus {bus}"
        ))
    }
}

pub fn find_intel_devices(spec: PciDeviceSpec) -> Result<Vec<PciLocation>, String> {
    let mut locations = Vec::new();

    for entry in std::fs::read_dir(PCI_CONFIG_ROOT)
        .map_err(|error| format!("failed to read {PCI_CONFIG_ROOT}: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("failed to read {PCI_CONFIG_ROOT} entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to read PCI entry type: {error}"))?
            .is_dir()
        {
            continue;
        }

        let Some(bus) = parse_pci_bus_dir(&entry.file_name()) else {
            continue;
        };
        let location = PciLocation {
            bus: bus.bus,
            device: spec.device,
            function: spec.function,
            group: bus.group,
        };

        if is_matching_intel_device(location, spec.device_id) {
            locations.push(location);
        }
    }

    locations.sort_by_key(|location| (location.group, location.bus));
    Ok(locations)
}

impl fmt::Display for PciBus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04x}:{:02x}", self.group, self.bus)
    }
}

fn is_matching_intel_device(location: PciLocation, expected_device_id: u16) -> bool {
    let Ok(device) = PciDevice::open_readonly(location) else {
        return false;
    };
    let Ok(vendor_device) = device.read_u32(PCI_VENDOR_DEVICE_OFFSET) else {
        return false;
    };

    let vendor_id = (vendor_device & 0xffff) as u16;
    let device_id = (vendor_device >> 16) as u16;

    vendor_id == INTEL_VENDOR_ID && device_id == expected_device_id
}

fn parse_pci_bus_dir(name: &std::ffi::OsStr) -> Option<PciBus> {
    let name = name.to_str()?;
    let (group, bus) = match name.split_once(':') {
        Some((group, bus)) => (u16::from_str_radix(group, 16).ok()?, bus),
        None => (0, name),
    };

    Some(PciBus {
        bus: u8::from_str_radix(bus, 16).ok()?,
        group,
    })
}

fn pci_config_path(location: PciLocation) -> PathBuf {
    let mut path = PathBuf::from(PCI_CONFIG_ROOT);

    if location.group == 0 {
        path.push(format!("{:02x}", location.bus));
    } else {
        path.push(format!("{:04x}:{:02x}", location.group, location.bus));
    }
    path.push(format!("{:02x}.{:x}", location.device, location.function));

    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_zero_group_pci_path() {
        assert_eq!(
            pci_config_path(PciLocation {
                bus: 0x7f,
                device: 0x0a,
                function: 2,
                group: 0,
            }),
            PathBuf::from("/proc/bus/pci/7f/0a.2")
        );
    }

    #[test]
    fn builds_non_zero_group_pci_path() {
        assert_eq!(
            pci_config_path(PciLocation {
                bus: 0xff,
                device: 0x0d,
                function: 6,
                group: 1,
            }),
            PathBuf::from("/proc/bus/pci/0001:ff/0d.6")
        );
    }

    #[test]
    fn parses_pci_bus_directory_names() {
        assert_eq!(
            parse_pci_bus_dir(std::ffi::OsStr::new("7f")),
            Some(PciBus {
                bus: 0x7f,
                group: 0,
            })
        );
        assert_eq!(
            parse_pci_bus_dir(std::ffi::OsStr::new("0001:ff")),
            Some(PciBus {
                bus: 0xff,
                group: 1,
            })
        );
        assert_eq!(parse_pci_bus_dir(std::ffi::OsStr::new("devices")), None);
    }
}
