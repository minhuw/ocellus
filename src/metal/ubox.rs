//! The CPU package of each uncore PCI bus, from the node-id registers of
//! each socket's UBOX, as Linux `snbep_pci2phy_map_init()` builds it
//! (arch/x86/events/intel/uncore_snbep.c).

use std::collections::{BTreeMap, BTreeSet};

use crate::metal::pci::{self, PciBus, PciDevice, PciLocation};

const NODE_ID_MASK: u32 = 0x7;
const NODE_MAP_ENTRIES: u32 = 8;

/// The UBOX device of a CPU family and its node-id registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UboxSpec {
    pub bus_fill: BusFill,
    pub device_id: u16,
    pub name: &'static str,
    /// CPUNODEID: the socket's node id in bits 2:0.
    pub node_id_offset: u64,
    /// GIDNIDMAP: the node id of package i in bits 3i+2:3i.
    pub node_map_offset: u64,
}

/// Which UBOX a bus without one belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusFill {
    /// The UBOX on the nearest bus at or above it (Linux `reverse`).
    FromAbove,
    /// The UBOX on the nearest bus at or below it.
    FromBelow,
}

/// Linux `snbep_uncore_pci_init()`: device 0x3ce0, SNBEP_CPUNODEID 0x40,
/// SNBEP_GIDNIDMAP 0x54, `reverse`.
pub const SANDY_BRIDGE_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromAbove,
    device_id: 0x3ce0,
    name: "Sandy Bridge-EP",
    node_id_offset: 0x40,
    node_map_offset: 0x54,
};

/// Linux `ivbep_uncore_pci_init()`: device 0x0e1e, the Sandy Bridge-EP
/// registers, `reverse`.
pub const IVY_BRIDGE_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromAbove,
    device_id: 0x0e1e,
    name: "Ivy Bridge-EP",
    node_id_offset: 0x40,
    node_map_offset: 0x54,
};

pub const HASWELL_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromAbove,
    device_id: 0x2f1e,
    name: "Haswell-EP",
    node_id_offset: 0x40,
    node_map_offset: 0x54,
};

pub const BROADWELL_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromAbove,
    device_id: 0x6f1e,
    name: "Broadwell-EP",
    node_id_offset: 0x40,
    node_map_offset: 0x54,
};

pub const SKYLAKE_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromBelow,
    device_id: 0x2014,
    name: "Skylake-SP",
    node_id_offset: 0xc0,
    node_map_offset: 0xd4,
};

pub const ICE_LAKE_UBOX: UboxSpec = UboxSpec {
    bus_fill: BusFill::FromAbove,
    device_id: 0x3450,
    name: "Ice Lake-SP",
    node_id_offset: 0xc0,
    node_map_offset: 0xd4,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UboxPackage {
    pub location: PciLocation,
    pub package_id: u32,
}

/// The package of each socket's UBOX, one UBOX per CPUID package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UboxNodeMap {
    spec: UboxSpec,
    uboxes: Vec<UboxPackage>,
}

impl UboxNodeMap {
    /// Reads every UBOX of `spec`, which must map one to one onto
    /// `package_ids`, the CPUID packages.
    pub fn discover(spec: UboxSpec, package_ids: &[u32]) -> Result<Self, String> {
        let locations = pci::find_intel_devices_matching_device_id(spec.device_id)?;

        Self::from_config_space(spec, &locations, package_ids, |location, offset| {
            PciDevice::open_readonly(location)?.read_u32(offset)
        })
    }

    fn from_config_space(
        spec: UboxSpec,
        locations: &[PciLocation],
        package_ids: &[u32],
        mut read_dword: impl FnMut(PciLocation, u64) -> Result<u32, String>,
    ) -> Result<Self, String> {
        let mut uboxes = Vec::with_capacity(locations.len());
        let mut locations_by_package = BTreeMap::new();

        for location in locations.iter().copied() {
            let node_id = read_dword(location, spec.node_id_offset)? & NODE_ID_MASK;
            let node_map = read_dword(location, spec.node_map_offset)?;
            let package_id = (0..NODE_MAP_ENTRIES)
                .find(|package_id| (node_map >> (package_id * 3)) & NODE_ID_MASK == node_id)
                .ok_or_else(|| {
                    format!(
                        "{} UBOX {location} node id {node_id} is not in its node id map 0x{node_map:06x}",
                        spec.name
                    )
                })?;

            if let Some(other) = locations_by_package.insert(package_id, location) {
                return Err(format!(
                    "{} UBOXes {other} and {location} both map to package {package_id}",
                    spec.name
                ));
            }
            uboxes.push(UboxPackage {
                location,
                package_id,
            });
        }

        let ubox_packages = locations_by_package.into_keys().collect::<BTreeSet<_>>();
        let cpu_packages = package_ids.iter().copied().collect::<BTreeSet<_>>();
        if ubox_packages != cpu_packages {
            return Err(format!(
                "{} UBOX node ids map to packages {ubox_packages:?}, but the CPUs are on packages {cpu_packages:?}",
                spec.name
            ));
        }

        uboxes.sort_by_key(|ubox| (ubox.location.group, ubox.location.bus));
        Ok(Self { spec, uboxes })
    }

    pub fn uboxes(&self) -> &[UboxPackage] {
        &self.uboxes
    }

    /// The package of `bus`: that of the UBOX on its bus, or else of the UBOX
    /// the family's bus fill gives it within its PCI segment.
    pub fn package_id(&self, bus: PciBus) -> Result<u32, String> {
        let candidates = self
            .uboxes
            .iter()
            .filter(|ubox| ubox.location.group == bus.group);
        let ubox = match self.spec.bus_fill {
            BusFill::FromAbove => candidates
                .filter(|ubox| ubox.location.bus >= bus.bus)
                .min_by_key(|ubox| ubox.location.bus),
            BusFill::FromBelow => candidates
                .filter(|ubox| ubox.location.bus <= bus.bus)
                .max_by_key(|ubox| ubox.location.bus),
        };

        ubox.map(|ubox| ubox.package_id)
            .ok_or_else(|| format!("no {} UBOX maps PCI bus {bus} to a package", self.spec.name))
    }

    /// The package of each distinct bus of `buses`; two buses may not share
    /// a package.
    pub fn bus_packages(
        &self,
        buses: impl IntoIterator<Item = PciBus>,
    ) -> Result<BTreeMap<u32, PciBus>, String> {
        let mut bus_packages = BTreeMap::new();

        for bus in buses {
            let package_id = self.package_id(bus)?;
            if let Some(other) = bus_packages.insert(package_id, bus)
                && other != bus
            {
                return Err(format!(
                    "{} PCI buses {other} and {bus} both map to package {package_id}",
                    self.spec.name
                ));
            }
        }

        Ok(bus_packages)
    }

    /// The package of each of `locations`, the `name` devices, which must be
    /// one per package.
    pub fn socket_devices(
        &self,
        locations: &[PciLocation],
        name: &str,
    ) -> Result<BTreeMap<u32, PciLocation>, String> {
        let mut devices = BTreeMap::new();

        for location in locations.iter().copied() {
            let package_id = self.package_id(PciBus {
                bus: location.bus,
                group: location.group,
            })?;
            if let Some(other) = devices.insert(package_id, location) {
                return Err(format!(
                    "{} {name} devices {other} and {location} both map to package {package_id}",
                    self.spec.name
                ));
            }
        }

        if devices.len() != self.uboxes.len() {
            return Err(format!(
                "discovered {} {name} devices on packages {:?} for packages {:?}",
                self.spec.name,
                devices.keys().collect::<Vec<_>>(),
                self.uboxes
                    .iter()
                    .map(|ubox| ubox.package_id)
                    .collect::<BTreeSet<_>>()
            ));
        }

        Ok(devices)
    }
}

/// The one scope of `scopes` on package `package_id`.
pub fn package_scope<S: Copy>(
    scopes: &[S],
    package_id: u32,
    scope_package_id: impl Fn(S) -> u32,
) -> Result<S, String> {
    let mut matching = scopes
        .iter()
        .copied()
        .filter(|scope| scope_package_id(*scope) == package_id);

    match (matching.next(), matching.next()) {
        (Some(scope), None) => Ok(scope),
        (None, _) => Err(format!("no CPU scope is on package {package_id}")),
        (Some(_), Some(_)) => Err(format!(
            "package {package_id} has more than one CPU scope, but its UBOX maps the package"
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The config space, by bus and offset, of UBOXes on `buses` of segment
    /// 0 with `node_ids` and the node id map `node_map`.
    fn fake_config_space(
        spec: UboxSpec,
        buses: &[u8],
        node_ids: &[u32],
        node_map: u32,
    ) -> (Vec<PciLocation>, BTreeMap<(u8, u64), u32>) {
        let locations = buses
            .iter()
            .map(|&bus| PciLocation {
                bus,
                device: 8,
                function: 2,
                group: 0,
            })
            .collect::<Vec<_>>();
        let mut config_space = BTreeMap::new();
        for (location, node_id) in locations.iter().zip(node_ids) {
            // Bits above the node id are not part of it.
            config_space.insert((location.bus, spec.node_id_offset), 0xabc0 | node_id);
            config_space.insert((location.bus, spec.node_map_offset), node_map);
        }

        (locations, config_space)
    }

    pub(crate) fn fake_node_map(
        spec: UboxSpec,
        buses: &[u8],
        node_ids: &[u32],
        node_map: u32,
        package_ids: &[u32],
    ) -> Result<UboxNodeMap, String> {
        let (locations, config_space) = fake_config_space(spec, buses, node_ids, node_map);

        UboxNodeMap::from_config_space(spec, &locations, package_ids, |location, offset| {
            config_space
                .get(&(location.bus, offset))
                .copied()
                .ok_or_else(|| format!("no fake dword at {location} offset 0x{offset:x}"))
        })
    }

    fn bus(bus: u8) -> PciBus {
        PciBus { bus, group: 0 }
    }

    #[test]
    fn maps_two_sockets_in_bus_order() {
        // Node 0 is package 0 and node 1 is package 1.
        let map = fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[0, 1], 0b001_000, &[0, 1]).unwrap();

        assert_eq!(map.package_id(bus(0x00)), Ok(0));
        assert_eq!(map.package_id(bus(0x3a)), Ok(0));
        assert_eq!(map.package_id(bus(0x80)), Ok(1));
        assert_eq!(map.package_id(bus(0xae)), Ok(1));
        assert_eq!(
            map.bus_packages([bus(0x3a), bus(0xae), bus(0x3a)]),
            Ok(BTreeMap::from([(0, bus(0x3a)), (1, bus(0xae))]))
        );
    }

    #[test]
    fn maps_two_sockets_against_bus_order() {
        // The UBOX on the lower bus has node id 1, which the node id map
        // gives package 1.
        let map = fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[1, 0], 0b001_000, &[0, 1]).unwrap();

        assert_eq!(map.package_id(bus(0x3a)), Ok(1));
        assert_eq!(map.package_id(bus(0xae)), Ok(0));
        assert_eq!(
            map.bus_packages([bus(0x3a), bus(0xae)]),
            Ok(BTreeMap::from([(0, bus(0xae)), (1, bus(0x3a))]))
        );

        // The same with the nodes swapped in the node id map instead.
        let map = fake_node_map(HASWELL_UBOX, &[0x7f, 0xff], &[0, 1], 0b000_001, &[0, 1]).unwrap();

        assert_eq!(map.package_id(bus(0x7f)), Ok(1));
        assert_eq!(map.package_id(bus(0xff)), Ok(0));
    }

    #[test]
    fn fills_buses_without_a_ubox_in_the_family_direction() {
        let map = fake_node_map(HASWELL_UBOX, &[0x7f, 0xff], &[0, 1], 0b001_000, &[0, 1]).unwrap();

        assert_eq!(map.package_id(bus(0x00)), Ok(0));
        assert_eq!(map.package_id(bus(0x80)), Ok(1));
        assert_eq!(
            map.package_id(PciBus {
                bus: 0x7f,
                group: 1
            }),
            Err("no Haswell-EP UBOX maps PCI bus 0001:7f to a package".to_string())
        );

        let map = fake_node_map(SKYLAKE_UBOX, &[0x16, 0x80], &[0, 1], 0b001_000, &[0, 1]).unwrap();

        assert_eq!(
            map.package_id(bus(0x00)),
            Err("no Skylake-SP UBOX maps PCI bus 0000:00 to a package".to_string())
        );
    }

    #[test]
    fn rejects_an_incomplete_node_map() {
        // Node id 2 is in no entry of the map.
        assert_eq!(
            fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[0, 2], 0b001_000, &[0, 1]),
            Err(
                "Skylake-SP UBOX 0000:80:08.2 node id 2 is not in its node id map 0x000008"
                    .to_string()
            )
        );
        // One UBOX for two CPU packages.
        assert_eq!(
            fake_node_map(SKYLAKE_UBOX, &[0x00], &[0], 0b001_000, &[0, 1]),
            Err(
                "Skylake-SP UBOX node ids map to packages {0}, but the CPUs are on packages {0, 1}"
                    .to_string()
            )
        );
        assert_eq!(
            fake_node_map(SKYLAKE_UBOX, &[], &[], 0, &[0]),
            Err(
                "Skylake-SP UBOX node ids map to packages {}, but the CPUs are on packages {0}"
                    .to_string()
            )
        );
    }

    #[test]
    fn rejects_an_inconsistent_node_map() {
        assert_eq!(
            fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[1, 1], 0b001_000, &[0, 1]),
            Err(
                "Skylake-SP UBOXes 0000:00:08.2 and 0000:80:08.2 both map to package 1".to_string()
            )
        );
        // Node 2 is package 2, but the CPUs are on packages 0 and 1.
        assert_eq!(
            fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[0, 2], 0b010_000_000, &[0, 1]),
            Err(
                "Skylake-SP UBOX node ids map to packages {0, 2}, but the CPUs are on packages {0, 1}"
                    .to_string()
            )
        );

        let map = fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[0, 1], 0b001_000, &[0, 1]).unwrap();
        assert_eq!(
            map.bus_packages([bus(0x17), bus(0x3a)]),
            Err("Skylake-SP PCI buses 0000:17 and 0000:3a both map to package 0".to_string())
        );
    }

    #[test]
    fn needs_one_socket_device_per_package() {
        let map = fake_node_map(SKYLAKE_UBOX, &[0x00, 0x80], &[1, 0], 0b001_000, &[0, 1]).unwrap();
        let imc = |bus, device| PciLocation {
            bus,
            device,
            function: 2,
            group: 0,
        };

        assert_eq!(
            map.socket_devices(&[imc(0x3a, 10), imc(0xae, 10)], "IMC"),
            Ok(BTreeMap::from([(0, imc(0xae, 10)), (1, imc(0x3a, 10))]))
        );
        assert_eq!(
            map.socket_devices(&[imc(0x3a, 10)], "IMC"),
            Err(
                "discovered Skylake-SP IMC devices on packages [1] for packages {0, 1}".to_string()
            )
        );
        assert_eq!(
            map.socket_devices(&[imc(0x3a, 10), imc(0x3a, 12), imc(0xae, 10)], "IMC"),
            Err(
                "Skylake-SP IMC devices 0000:3a:0a.2 and 0000:3a:0c.2 both map to package 1"
                    .to_string()
            )
        );
    }

    #[test]
    fn finds_the_one_scope_of_a_package() {
        let scopes = [(0_u32, 0_u32), (1, 0), (1, 1)];

        assert_eq!(package_scope(&scopes, 0, |scope| scope.0), Ok((0, 0)));
        assert_eq!(
            package_scope(&scopes, 1, |scope| scope.0),
            Err("package 1 has more than one CPU scope, but its UBOX maps the package".to_string())
        );
        assert_eq!(
            package_scope(&scopes, 2, |scope| scope.0),
            Err("no CPU scope is on package 2".to_string())
        );
    }
}
