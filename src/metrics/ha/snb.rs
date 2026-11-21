//! The home agents of Sandy Bridge-EP, one per package, and Ivy Bridge-EP,
//! HA0 and, on the dies that have it, HA1.
//!
//! [SNB] is Intel's Xeon E5-2600 uncore performance monitoring guide
//! (327043-001), [IVB] the Xeon E5-2600 v2 and E7 v2 one (329468-001).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::pci::{PciBus, PciDevice, PciDeviceSpec, PciLocation, PciVendorDevice};
use crate::metal::topology::TopologyLevelKind;
use crate::metal::ubox::{self, UboxNodeMap, UboxSpec};
use crate::metrics::common::BYTES_PER_CACHE_LINE;
use crate::metrics::ha::hsx::{HsxHaMetrics, HsxHaMetricsByScope};
use crate::metrics::uncore::hsx::{self, ConfigReading, HsxUncoreScope};
use crate::pmu_config::types::{
    BoxType, CountAccumulator, CounterSlot, PmuConfig, ProgrammedBoxType, ProgrammedGroup,
    UncoreCounts,
};

/// HA_PCI_PMON_BOX_CTL, HA_PCI_PMON_CTL{0-3} and HA_PCI_PMON_CTR{0-3}, each
/// counter 48 bits in two dwords ([SNB] Tables 2-33, 2-36; [IVB] Tables
/// 2-37, 2-41), where Linux's `snbep_uncore_ha` and `ivbep_uncore_ha`
/// program them.
const HA_UNIT_CTL_OFFSET: u64 = 0xf4;
const HA_CTL_OFFSETS: [u64; 4] = [0xd8, 0xdc, 0xe0, 0xe4];
const HA_CTR_OFFSETS: [u64; 4] = [0xa0, 0xa8, 0xb0, 0xb8];

// The unit and counter control bits of the Sandy/Ivy Bridge-EP IMC
// collector. [IVB] Tables 2-38 and 2-40 document each bit Ivy Bridge-EP
// writes; Table 2-38 asks for 1 in box bits 17:16, which stay 0 as in
// Linux's IVBEP_PMON_BOX_CTL_INT (board card c55). [SNB] Tables 2-34 and
// 2-35 reserve the resets (box bits 1:0, counter bit 17) and the overflow
// enable (bit 20), as its iMC tables do (c55); Linux writes the box resets
// on both models.
const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_FREEZE_ENABLE_BIT: u32 = 1 << 16;

/// The HA PMON function, D14:F1 ([SNB] Table 2-33), device id 0x3c46
/// (Linux `PCI_DEVICE_ID_INTEL_UNC_HA`).
const SNB_HA_UNITS: [PciDeviceSpec; 1] = [PciDeviceSpec {
    device: 14,
    function: 1,
    device_id: 0x3c46,
}];

/// HA0 at D14:F1 and HA1 at D28:F1 ([IVB] Table 2-37; its Table 1-3 puts
/// HA1 at D30:F1, where iMC1 channel 3 is), device ids 0x0e30 and 0x0e38
/// (Linux `ivbep_uncore_pci_ids`, HA boxes 0 and 1).
const IVB_HA_UNITS: [PciDeviceSpec; 2] = [
    PciDeviceSpec {
        device: 14,
        function: 1,
        device_id: 0x0e30,
    },
    PciDeviceSpec {
        device: 28,
        function: 1,
        device_id: 0x0e38,
    },
];

/// UNC_H_REQUESTS and the umasks of the Haswell-EP HA collector's group, in
/// counter order: local reads, remote reads, local writes and remote writes
/// ([IVB] Table 2-57).
const REQUESTS_EVENT: u8 = 0x01;
const IVB_REQUEST_UMASKS: [u8; 4] = [0x01, 0x02, 0x04, 0x08];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbHaArchitecture {
    Ivb,
    Snb,
}

impl SnbHaArchitecture {
    fn from_model(model: IntelServerCpuModel) -> Option<Self> {
        match model {
            IntelServerCpuModel::IvyTown => Some(Self::Ivb),
            IntelServerCpuModel::SandyBridgeEp => Some(Self::Snb),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ivb => "Ivy Bridge-EP",
            Self::Snb => "Sandy Bridge-EP",
        }
    }

    /// The model's home agents; a unit's id is its index.
    const fn units(self) -> &'static [PciDeviceSpec] {
        match self {
            Self::Ivb => &IVB_HA_UNITS,
            Self::Snb => &SNB_HA_UNITS,
        }
    }

    const fn ubox(self) -> UboxSpec {
        match self {
            Self::Ivb => ubox::IVY_BRIDGE_UBOX,
            Self::Snb => ubox::SANDY_BRIDGE_UBOX,
        }
    }

    /// The REQUESTS umasks of the default group. Sandy Bridge-EP has none:
    /// its REQUESTS counts all reads or all writes ([SNB] Table 2-48), not
    /// the local and remote ones the Haswell-EP metrics need.
    const fn request_umasks(self) -> Option<[u8; 4]> {
        match self {
            Self::Ivb => Some(IVB_REQUEST_UMASKS),
            Self::Snb => None,
        }
    }

    const fn unit_freeze(self) -> u32 {
        UNIT_FREEZE_BIT | self.unit_freeze_enable()
    }

    const fn unit_freeze_and_reset(self) -> u32 {
        self.unit_freeze_enable()
            | UNIT_CONTROL_RESET_BIT
            | UNIT_COUNTER_RESET_BIT
            | UNIT_FREEZE_BIT
    }

    const fn unit_freeze_enable(self) -> u32 {
        match self {
            Self::Ivb => 0,
            Self::Snb => UNIT_FREEZE_ENABLE_BIT,
        }
    }

    const fn unit_unfreeze(self) -> u32 {
        self.unit_freeze_enable()
    }
}

#[derive(Debug)]
pub struct SnbHaCollector {
    architecture: SnbHaArchitecture,
    units: Vec<SnbHaUnit>,
    config: Option<CountAccumulator>,
}

impl SnbHaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = SnbHaArchitecture::from_model(model).ok_or_else(|| {
            format!("Sandy/Ivy Bridge-EP HA collection is not supported for {model:?}")
        })?;

        Ok(Self {
            architecture,
            units: discover_units(architecture)?,
            config: None,
        })
    }

    /// Samples the default group for `interval`: on Ivy Bridge-EP the
    /// Haswell-EP HA collector's requests, with its metrics. Sandy Bridge-EP
    /// has no default group, so its sample only waits and has no scopes.
    pub async fn sample(&mut self, interval: Duration) -> Result<HsxHaMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} HA measure interval must be non-zero",
                self.architecture.name()
            ));
        }
        let Some(umasks) = self.architecture.request_umasks() else {
            tokio::time::sleep(interval).await;
            return Ok(HsxHaMetrics { scopes: Vec::new() });
        };

        let scopes = self
            .count_requests(umasks, interval)
            .await?
            .into_iter()
            .map(|(scope, counts)| counts.metrics(scope, interval))
            .collect();

        Ok(HsxHaMetrics { scopes })
    }

    /// The REQUESTS counts of each package's home agents over `interval`, in
    /// slices of at most `DEFAULT_MAX_SLICE`, each of which resets and
    /// programs every home agent.
    async fn count_requests(
        &self,
        umasks: [u8; 4],
        interval: Duration,
    ) -> Result<BTreeMap<HsxUncoreScope, RequestCounts>, String> {
        let slice_count = hsx::measurement_round_count(interval, 1);
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut counts = BTreeMap::<HsxUncoreScope, RequestCounts>::new();

        for _ in 0..slice_count {
            for unit in &self.units {
                unit.freeze_and_reset()?;
            }
            for unit in &self.units {
                unit.program_requests(umasks)?;
            }

            let started_at = Instant::now();
            for unit in &self.units {
                unit.unfreeze()?;
            }
            tokio::time::sleep(slice_duration).await;
            for unit in &self.units {
                unit.freeze()?;
            }
            let running = started_at.elapsed();

            let mut slice_values = BTreeMap::<HsxUncoreScope, [u64; 4]>::new();
            for unit in &self.units {
                let values = slice_values.entry(unit.scope).or_default();
                for (index, value) in values.iter_mut().enumerate() {
                    *value += unit.read_counter(index)?;
                }
            }
            for (scope, values) in slice_values {
                counts.entry(scope).or_default().add(values, running);
            }
        }

        Ok(counts)
    }
}

impl SnbHaCollector {
    /// The home agents of each package that PMU configs can program.
    /// Unit id: the home agent's index in the model's HA table, 0 on Sandy
    /// Bridge-EP and 0 (HA0) or 1 (HA1) on Ivy Bridge-EP.
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        hsx::unit_map(self.units.iter().map(SnbHaUnit::config_unit))
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// group for `None`. Counts restart from 0.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        // The default sample resets and programs every home agent on every
        // slice.
        let Some(config) = config else {
            self.config = None;
            return Ok(());
        };
        check_config(self.architecture, &config.program, &self.config_units())?;
        self.config = Some(CountAccumulator::new(config));

        Ok(())
    }

    /// Samples the set config for `interval`; `cycle` sets the group temporal
    /// groups start from. Its counts are cumulative since the config was set.
    /// Home agents in none of the config's groups stay untouched.
    pub async fn sample_config(
        &mut self,
        interval: Duration,
        cycle: u64,
    ) -> Result<UncoreCounts, String> {
        let name = self.architecture.name();
        let accumulator = self
            .config
            .as_mut()
            .ok_or_else(|| format!("no PMU config is set for {name} HA"))?;
        if interval.is_zero() {
            return Err(format!("{name} HA measure interval must be non-zero"));
        }
        let ids = self
            .units
            .iter()
            .map(SnbHaUnit::config_unit)
            .collect::<Vec<_>>();
        let program = accumulator.config().program.clone();

        for slice in hsx::config_schedule(&program, interval, cycle) {
            let targets = hsx::slice_units(&ids, &program, &slice)?;
            for &(unit, group) in &targets {
                for (offset, value) in
                    config_unit_writes(self.architecture, &program.groups[group])?
                {
                    self.units[unit].device.write_u32(offset, value)?;
                }
            }

            let started_at = Instant::now();
            for &(unit, _) in &targets {
                self.units[unit].unfreeze()?;
            }
            tokio::time::sleep(slice.duration).await;
            for &(unit, _) in &targets {
                self.units[unit].freeze()?;
            }
            let running = started_at.elapsed();

            let mut readings = Vec::with_capacity(targets.len());
            for &(unit, group) in &targets {
                let group = &program.groups[group];
                let (package, id) = ids[unit];
                readings.push(ConfigReading {
                    package,
                    unit: id,
                    group: group.index,
                    counters: self.units[unit].read_config_counters(group)?,
                });
            }
            hsx::fold_readings(accumulator, &readings, running)?;
        }

        Ok(accumulator.counts())
    }
}

/// One package's REQUESTS counts, in `IVB_REQUEST_UMASKS` order, and how
/// long they counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RequestCounts {
    values: [u64; 4],
    running: Duration,
}

impl RequestCounts {
    fn add(&mut self, values: [u64; 4], running: Duration) {
        for (total, value) in self.values.iter_mut().zip(values) {
            *total += value;
        }
        self.running += running;
    }

    /// The Haswell-EP HA metrics of the counts, scaled from their running
    /// time to `enabled`.
    fn metrics(self, scope: HsxUncoreScope, enabled: Duration) -> HsxHaMetricsByScope {
        let [local_read, remote_read, local_write, remote_write] = self
            .values
            .map(|value| hsx::scale_to_enabled(value, enabled, self.running));
        let bytes_per_second =
            |count| hsx::events_per_second(count, enabled) * BYTES_PER_CACHE_LINE;

        HsxHaMetricsByScope {
            local_read_bytes_per_second: bytes_per_second(local_read),
            local_read_ratio: hsx::ratio(local_read, local_read + remote_read),
            local_write_bytes_per_second: bytes_per_second(local_write),
            local_write_ratio: hsx::ratio(local_write, local_write + remote_write),
            remote_read_bytes_per_second: bytes_per_second(remote_read),
            remote_write_bytes_per_second: bytes_per_second(remote_write),
            scope,
        }
    }
}

#[derive(Debug)]
struct SnbHaUnit {
    architecture: SnbHaArchitecture,
    device: PciDevice,
    /// Index in the model's HA table, kept when another home agent is absent.
    id: u16,
    scope: HsxUncoreScope,
}

impl SnbHaUnit {
    fn new(architecture: SnbHaArchitecture, placement: SnbHaPlacement) -> Result<Self, String> {
        Ok(Self {
            architecture,
            device: PciDevice::open(placement.location)?,
            id: placement.id,
            scope: HsxUncoreScope {
                package_id: placement.package_id,
            },
        })
    }

    /// The home agent's package and unit id in PMU configs.
    fn config_unit(&self) -> (u16, u16) {
        (self.scope.package_id as u16, self.id)
    }

    fn freeze(&self) -> Result<(), String> {
        self.device
            .write_u32(HA_UNIT_CTL_OFFSET, self.architecture.unit_freeze())
    }

    fn freeze_and_reset(&self) -> Result<(), String> {
        self.device.write_u32(
            HA_UNIT_CTL_OFFSET,
            self.architecture.unit_freeze_and_reset(),
        )
    }

    fn unfreeze(&self) -> Result<(), String> {
        self.device
            .write_u32(HA_UNIT_CTL_OFFSET, self.architecture.unit_unfreeze())
    }

    fn program_requests(&self, umasks: [u8; 4]) -> Result<(), String> {
        for (offset, umask) in HA_CTL_OFFSETS.into_iter().zip(umasks) {
            self.device
                .write_u32(offset, counter_control(REQUESTS_EVENT, umask))?;
        }

        Ok(())
    }

    fn read_counter(&self, index: usize) -> Result<u64, String> {
        self.device
            .read_u64(HA_CTR_OFFSETS[index])
            .map(hsx::mask_counter)
    }

    /// The raw values of `group`'s counters.
    fn read_config_counters(
        &self,
        group: &ProgrammedGroup,
    ) -> Result<Vec<(CounterSlot, u64)>, String> {
        group
            .counters
            .iter()
            .map(|counter| {
                let (_, offset) = config_slot_offsets(counter.slot)?;
                let value =
                    hsx::read_counter_dwords(offset, |offset| self.device.read_u32(offset))?;
                Ok((counter.slot, value))
            })
            .collect()
    }
}

/// A home agent discovery found: its id, PCI location and package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnbHaPlacement {
    id: u16,
    location: PciLocation,
    package_id: u32,
}

fn discover_units(architecture: SnbHaArchitecture) -> Result<Vec<SnbHaUnit>, String> {
    let devices = ha_devices(
        architecture,
        metal::pci::find_intel_devices_matching_device_id,
    )?;
    let node_map = UboxNodeMap::discover(architecture.ubox(), &cpu_package_ids()?)?;

    place_units(architecture, &devices, &node_map)?
        .into_iter()
        .map(|placement| SnbHaUnit::new(architecture, placement))
        .collect()
}

/// The devices with a device id of the model's HA table, as `find` locates
/// them; a host without one has no home agent to count.
fn ha_devices(
    architecture: SnbHaArchitecture,
    mut find: impl FnMut(u16) -> Result<Vec<PciLocation>, String>,
) -> Result<Vec<PciVendorDevice>, String> {
    let mut devices = Vec::new();
    for spec in architecture.units() {
        devices.extend(
            find(spec.device_id)?
                .into_iter()
                .map(|location| PciVendorDevice {
                    device_id: spec.device_id,
                    location,
                }),
        );
    }

    if devices.is_empty() {
        return Err(format!(
            "failed to discover any {} HA devices with PCI device ids {}",
            architecture.name(),
            architecture
                .units()
                .iter()
                .map(|spec| format!("0x{:04x}", spec.device_id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(devices)
}

/// Places each of `devices` in the package `node_map` gives its bus. Each
/// must sit at its table address and hold an id no other device of its
/// package holds, and each package must have a home agent.
fn place_units(
    architecture: SnbHaArchitecture,
    devices: &[PciVendorDevice],
    node_map: &UboxNodeMap,
) -> Result<Vec<SnbHaPlacement>, String> {
    let name = architecture.name();
    let specs = architecture.units();
    let mut placements = Vec::<SnbHaPlacement>::with_capacity(devices.len());

    for device in devices {
        let location = device.location;
        let id = specs
            .iter()
            .position(|spec| spec.device_id == device.device_id)
            .ok_or_else(|| {
                format!(
                    "{location} (device id 0x{:04x}) is no {name} HA",
                    device.device_id
                )
            })?;
        let spec = specs[id];
        if (location.device, location.function) != (spec.device, spec.function) {
            return Err(format!(
                "{name} HA{id} device id 0x{:04x} is at {location}, not at device {} function {}",
                spec.device_id, spec.device, spec.function
            ));
        }
        let id = id as u16;
        let package_id = node_map.package_id(PciBus {
            bus: location.bus,
            group: location.group,
        })?;
        if let Some(other) = placements
            .iter()
            .find(|placement| (placement.package_id, placement.id) == (package_id, id))
        {
            return Err(format!(
                "{name} HA{id} devices {} and {location} both map to package {package_id}",
                other.location
            ));
        }

        placements.push(SnbHaPlacement {
            id,
            location,
            package_id,
        });
    }

    let packages = placements
        .iter()
        .map(|placement| placement.package_id)
        .collect::<BTreeSet<_>>();
    let ubox_packages = node_map
        .uboxes()
        .iter()
        .map(|ubox| ubox.package_id)
        .collect::<BTreeSet<_>>();
    if packages != ubox_packages {
        return Err(format!(
            "discovered {name} HA devices on packages {packages:?} for packages {ubox_packages:?}"
        ));
    }

    placements.sort_by_key(|placement| (placement.package_id, placement.id));
    Ok(placements)
}

fn cpu_package_ids() -> Result<Vec<u32>, String> {
    metal::topology::cpu_topologies()?
        .iter()
        .map(|topology| {
            topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())
        })
        .collect()
}

/// Errs for a program the home agents in `units` cannot count.
fn check_config(
    architecture: SnbHaArchitecture,
    program: &ProgrammedBoxType,
    units: &BTreeMap<u16, BTreeSet<u16>>,
) -> Result<(), String> {
    if program.box_type != BoxType::Ha {
        return Err(format!(
            "{} HA cannot count a {} config",
            architecture.name(),
            program.box_type.label()
        ));
    }

    for group in &program.groups {
        if group.filters.is_some() {
            return Err(format!(
                "HA config group {} has filters, which HA units do not have",
                group.index
            ));
        }
        for (package, group_units) in &group.units {
            let host_units = units.get(package).ok_or_else(|| {
                format!(
                    "HA config group {} runs on package {package}, which has no HA units on this host",
                    group.index
                )
            })?;
            if let Some(unit) = group_units.difference(host_units).next() {
                return Err(format!(
                    "HA config group {} runs on unit {unit} of package {package}, which this host does not have",
                    group.index
                ));
            }
        }
        config_unit_writes(architecture, group)?;
    }

    Ok(())
}

/// The (offset, value) writes that program one home agent for `group`:
/// freeze and reset it as the default group does, then reset and enable
/// each counter of the group, and write 0 to the controls of the others.
fn config_unit_writes(
    architecture: SnbHaArchitecture,
    group: &ProgrammedGroup,
) -> Result<Vec<(u64, u32)>, String> {
    let mut controls = HA_CTL_OFFSETS
        .into_iter()
        .map(|offset| (offset, 0))
        .collect::<BTreeMap<u64, u32>>();

    for counter in &group.counters {
        let (offset, _) = config_slot_offsets(counter.slot)
            .map_err(|error| format!("HA config group {}: {error}", group.index))?;
        let control = u32::try_from(counter.control).map_err(|_| {
            format!(
                "HA config group {} slot {} control 0x{:x} does not fit in 32 bits",
                group.index,
                counter.slot.label(),
                counter.control
            )
        })?;
        controls.insert(offset, control | COUNTER_RESET_BIT);
    }

    Ok([(HA_UNIT_CTL_OFFSET, architecture.unit_freeze_and_reset())]
        .into_iter()
        .chain(controls)
        .collect())
}

/// The control and counter offsets of a counter slot.
fn config_slot_offsets(slot: CounterSlot) -> Result<(u64, u64), String> {
    let offsets = match slot {
        CounterSlot::General(index) => HA_CTL_OFFSETS
            .get(usize::from(index))
            .zip(HA_CTR_OFFSETS.get(usize::from(index)))
            .map(|(&control, &counter)| (control, counter)),
        CounterSlot::Fixed => None,
    };

    offsets.ok_or_else(|| {
        format!(
            "Sandy/Ivy Bridge-EP HA units have no counter slot {}",
            slot.label()
        )
    })
}

/// The default group's controls, as the Haswell-EP HA collector writes them.
fn counter_control(event: u8, umask: u8) -> u32 {
    u32::from(event)
        | (u32::from(umask) << 8)
        | COUNTER_RESET_BIT
        | COUNTER_OVERFLOW_ENABLE_BIT
        | COUNTER_ENABLE_BIT
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::metal::ubox::tests::fake_node_map;
    use crate::metrics::ConfigurableCollector;
    use crate::metrics::uncore::hsx::config_fixtures::*;
    use crate::pmu_config::types::{ChaFilters, CountUnit, Grouping, PmuTopology};

    const IVB: SnbHaArchitecture = SnbHaArchitecture::Ivb;
    const SNB: SnbHaArchitecture = SnbHaArchitecture::Snb;

    #[test]
    fn uses_the_documented_devices_and_registers() {
        let addresses = |architecture: SnbHaArchitecture| {
            architecture
                .units()
                .iter()
                .map(|spec| (spec.device, spec.function, spec.device_id))
                .collect::<Vec<_>>()
        };

        assert_eq!(addresses(SNB), [(14, 1, 0x3c46)]);
        assert_eq!(addresses(IVB), [(14, 1, 0x0e30), (28, 1, 0x0e38)]);
        // The UBOXes of the Sandy/Ivy Bridge-EP IMC and IRP collectors.
        assert_eq!(SNB.ubox(), ubox::SANDY_BRIDGE_UBOX);
        assert_eq!(IVB.ubox(), ubox::IVY_BRIDGE_UBOX);
        for (spec, device_id) in [
            (ubox::SANDY_BRIDGE_UBOX, 0x3ce0),
            (ubox::IVY_BRIDGE_UBOX, 0x0e1e),
        ] {
            assert_eq!(
                (spec.device_id, spec.node_id_offset, spec.node_map_offset),
                (device_id, 0x40, 0x54)
            );
            assert_eq!(spec.bus_fill, ubox::BusFill::FromAbove);
        }
        assert_eq!(HA_UNIT_CTL_OFFSET, 0xf4);
        assert_eq!(HA_CTL_OFFSETS, [0xd8, 0xdc, 0xe0, 0xe4]);
        assert_eq!(HA_CTR_OFFSETS, [0xa0, 0xa8, 0xb0, 0xb8]);
    }

    #[test]
    fn encodes_snb_ivb_unit_and_counter_controls() {
        assert_eq!(SNB.unit_freeze(), 0x1_0100);
        assert_eq!(SNB.unit_freeze_and_reset(), 0x1_0103);
        assert_eq!(SNB.unit_unfreeze(), 0x1_0000);
        assert_eq!(IVB.unit_freeze(), 0x100);
        assert_eq!(IVB.unit_freeze_and_reset(), 0x103);
        assert_eq!(IVB.unit_unfreeze(), 0);
        assert_eq!(
            counter_control(0x01, 0x02),
            0x01 | (0x02 << 8) | (1 << 17) | (1 << 20) | (1 << 22)
        );
        // UNC_H_REQUESTS.{READS,WRITES}_{LOCAL,REMOTE} ([IVB] Table 2-57).
        assert_eq!(REQUESTS_EVENT, 0x01);
        assert_eq!(IVB.request_umasks(), Some([0x01, 0x02, 0x04, 0x08]));
        assert_eq!(SNB.request_umasks(), None);
    }

    fn ha(device_id: u16, bus: u8, device: u8) -> PciVendorDevice {
        PciVendorDevice {
            device_id,
            location: PciLocation {
                bus,
                device,
                function: 1,
                group: 0,
            },
        }
    }

    /// (package, unit id, bus) of each placement.
    fn placed(
        architecture: SnbHaArchitecture,
        devices: &[PciVendorDevice],
        node_map: &UboxNodeMap,
    ) -> Result<Vec<(u32, u16, u8)>, String> {
        Ok(place_units(architecture, devices, node_map)?
            .into_iter()
            .map(|placement| (placement.package_id, placement.id, placement.location.bus))
            .collect())
    }

    #[test]
    fn places_one_sandy_bridge_home_agent_per_socket() {
        // One socket, its UBOX and home agent on bus 0x3f.
        let map = fake_node_map(ubox::SANDY_BRIDGE_UBOX, &[0x3f], &[0], 0, &[0]).unwrap();
        assert_eq!(
            placed(SNB, &[ha(0x3c46, 0x3f, 14)], &map),
            Ok(vec![(0, 0, 0x3f)])
        );

        // Two sockets, node 0 on bus 0x3f and node 1 on 0x7f.
        let two = [ha(0x3c46, 0x7f, 14), ha(0x3c46, 0x3f, 14)];
        let map = fake_node_map(
            ubox::SANDY_BRIDGE_UBOX,
            &[0x3f, 0x7f],
            &[0, 1],
            0b001_000,
            &[0, 1],
        )
        .unwrap();
        assert_eq!(
            placed(SNB, &two, &map),
            Ok(vec![(0, 0, 0x3f), (1, 0, 0x7f)])
        );

        // Against bus order: the UBOX on bus 0x3f has node id 1, which the
        // node id map gives package 1.
        let map = fake_node_map(
            ubox::SANDY_BRIDGE_UBOX,
            &[0x3f, 0x7f],
            &[1, 0],
            0b001_000,
            &[0, 1],
        )
        .unwrap();
        assert_eq!(
            placed(SNB, &two, &map),
            Ok(vec![(0, 0, 0x7f), (1, 0, 0x3f)])
        );
    }

    #[test]
    fn places_two_ivy_bridge_home_agents_per_socket() {
        let map = fake_node_map(
            ubox::IVY_BRIDGE_UBOX,
            &[0x7f, 0xff],
            &[0, 1],
            0b001_000,
            &[0, 1],
        )
        .unwrap();
        let both = [
            ha(0x0e38, 0xff, 28),
            ha(0x0e30, 0x7f, 14),
            ha(0x0e30, 0xff, 14),
            ha(0x0e38, 0x7f, 28),
        ];
        assert_eq!(
            placed(IVB, &both, &map),
            Ok(vec![(0, 0, 0x7f), (0, 1, 0x7f), (1, 0, 0xff), (1, 1, 0xff),])
        );

        // Dies without HA1 keep HA0's id.
        let ha0 = [ha(0x0e30, 0x7f, 14), ha(0x0e30, 0xff, 14)];
        assert_eq!(
            placed(IVB, &ha0, &map),
            Ok(vec![(0, 0, 0x7f), (1, 0, 0xff)])
        );

        // One socket.
        let map = fake_node_map(ubox::IVY_BRIDGE_UBOX, &[0xff], &[0], 0, &[0]).unwrap();
        assert_eq!(
            placed(IVB, &[ha(0x0e38, 0xff, 28), ha(0x0e30, 0xff, 14)], &map),
            Ok(vec![(0, 0, 0xff), (0, 1, 0xff)])
        );
    }

    #[test]
    fn rejects_home_agents_it_cannot_place() {
        let map = fake_node_map(
            ubox::IVY_BRIDGE_UBOX,
            &[0x7f, 0xff],
            &[0, 1],
            0b001_000,
            &[0, 1],
        )
        .unwrap();
        let mut other_segment = ha(0x0e30, 0xff, 14);
        other_segment.location.group = 1;

        for (devices, error) in [
            (
                vec![
                    ha(0x0e30, 0x7f, 14),
                    ha(0x0e30, 0xff, 14),
                    ha(0x0e38, 0x7f, 30),
                ],
                "Ivy Bridge-EP HA1 device id 0x0e38 is at 0000:7f:1e.1, not at device 28 function 1",
            ),
            (
                vec![ha(0x0e30, 0x7f, 14)],
                "discovered Ivy Bridge-EP HA devices on packages {0} for packages {0, 1}",
            ),
            (
                // Bus 0x40 has no UBOX: the one above it, on 0x7f, maps it.
                vec![ha(0x0e30, 0x7f, 14), ha(0x0e30, 0x40, 14)],
                "Ivy Bridge-EP HA0 devices 0000:7f:0e.1 and 0000:40:0e.1 both map to package 0",
            ),
            (
                vec![ha(0x0e30, 0x7f, 14), other_segment],
                "no Ivy Bridge-EP UBOX maps PCI bus 0001:ff to a package",
            ),
            (
                vec![ha(0x3c46, 0x7f, 14)],
                "0000:7f:0e.1 (device id 0x3c46) is no Ivy Bridge-EP HA",
            ),
        ] {
            assert_eq!(placed(IVB, &devices, &map), Err(error.to_string()));
        }
    }

    #[test]
    fn needs_a_home_agent_device() {
        let mut asked = Vec::new();
        assert_eq!(
            ha_devices(IVB, |device_id| {
                asked.push(device_id);
                Ok(Vec::new())
            }),
            Err(
                "failed to discover any Ivy Bridge-EP HA devices with PCI device ids 0x0e30, 0x0e38"
                    .to_string()
            )
        );
        assert_eq!(asked, [0x0e30, 0x0e38]);
        assert_eq!(
            ha_devices(SNB, |_| Ok(Vec::new())),
            Err(
                "failed to discover any Sandy Bridge-EP HA devices with PCI device ids 0x3c46"
                    .to_string()
            )
        );
        assert_eq!(
            ha_devices(SNB, |_| Err("no /proc/bus/pci".to_string())),
            Err("no /proc/bus/pci".to_string())
        );

        let location = ha(0x0e38, 0xff, 28).location;
        assert_eq!(
            ha_devices(IVB, |device_id| Ok(if device_id == 0x0e38 {
                vec![location]
            } else {
                Vec::new()
            })),
            Ok(vec![ha(0x0e38, 0xff, 28)])
        );
    }

    /// A config space whose counters 0-3 hold 10, 30, 20 and 20, counter 1
    /// above bit 48 junk.
    fn request_config_space() -> [u8; 0x100] {
        let mut config_space = [0_u8; 0x100];
        for (offset, value) in [
            (0xa0, 10_u64),
            (0xa8, (0xabcd << 48) | 30),
            (0xb0, 20),
            (0xb8, 20),
        ] {
            config_space[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        config_space
    }

    fn registers(unit: &SnbHaUnit) -> [u32; 5] {
        [0xf4, 0xd8, 0xdc, 0xe0, 0xe4].map(|offset| unit.device.read_u32(offset).unwrap())
    }

    #[test]
    fn counts_the_requests_of_each_package_over_its_home_agents() {
        let mut collector = test_collector(IVB);
        collector.units = vec![
            test_unit(IVB, 0, 0, &request_config_space()),
            test_unit(IVB, 0, 1, &request_config_space()),
            test_unit(IVB, 1, 1, &request_config_space()),
        ];
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let counts = runtime
            .block_on(collector.count_requests(IVB_REQUEST_UMASKS, Duration::from_millis(1)))
            .unwrap();

        assert_eq!(
            counts.keys().copied().collect::<Vec<_>>(),
            [scope(0), scope(1)]
        );
        assert_eq!(counts[&scope(0)].values, [20, 60, 40, 40]);
        assert_eq!(counts[&scope(1)].values, [10, 30, 20, 20]);
        assert!(counts[&scope(0)].running >= Duration::from_millis(1));
        // Each home agent ends frozen with the REQUESTS controls.
        for unit in &collector.units {
            assert_eq!(
                registers(unit),
                [0x100, 0x52_0101, 0x52_0201, 0x52_0401, 0x52_0801]
            );
        }

        let metrics = runtime
            .block_on(collector.sample(Duration::from_millis(1)))
            .unwrap();
        assert_eq!(
            metrics
                .scopes
                .iter()
                .map(|scope| scope.scope)
                .collect::<Vec<_>>(),
            [scope(0), scope(1)]
        );
    }

    #[test]
    fn computes_the_haswell_ha_metrics_of_request_counts() {
        let counts = RequestCounts {
            values: [100, 300, 200, 200],
            running: Duration::from_millis(100),
        };

        // The numbers of the Haswell-EP collector's `computes_hsx_ha_metrics`.
        let metrics = counts.metrics(scope(1), Duration::from_millis(100));
        assert_eq!(metrics.local_read_bytes_per_second, 64_000.0);
        assert_eq!(metrics.remote_read_bytes_per_second, 192_000.0);
        assert_eq!(metrics.local_write_bytes_per_second, 128_000.0);
        assert_eq!(metrics.remote_write_bytes_per_second, 128_000.0);
        assert_eq!(metrics.local_read_ratio, 0.25);
        assert_eq!(metrics.local_write_ratio, 0.5);
        assert_eq!(metrics.scope, scope(1));

        // Counted for half the interval: scaled to all of it.
        let half = RequestCounts {
            running: Duration::from_millis(50),
            ..counts
        };
        let metrics = half.metrics(scope(1), Duration::from_millis(100));
        assert_eq!(metrics.local_read_bytes_per_second, 128_000.0);
        assert_eq!(metrics.local_read_ratio, 0.25);

        let mut total = RequestCounts::default();
        total.add([1, 2, 3, 4], Duration::from_millis(1));
        total.add([10, 20, 30, 40], Duration::from_millis(2));
        assert_eq!(
            total,
            RequestCounts {
                values: [11, 22, 33, 44],
                running: Duration::from_millis(3),
            }
        );
    }

    #[test]
    fn sandy_bridge_samples_no_default_group() {
        let mut collector = test_collector(SNB);
        collector.units = vec![test_unit(SNB, 0, 0, &request_config_space())];
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let metrics = runtime
            .block_on(collector.sample(Duration::from_millis(1)))
            .unwrap();

        assert!(metrics.scopes.is_empty());
        assert_eq!(registers(&collector.units[0]), [0; 5]);
        assert_eq!(collector.units[0].read_counter(0), Ok(10));
        assert_eq!(
            runtime
                .block_on(collector.sample(Duration::ZERO))
                .unwrap_err(),
            "Sandy Bridge-EP HA measure interval must be non-zero"
        );
    }

    /// The default group, as a config request of its REQUESTS aliases,
    /// validates to the controls the default sample programs, less the
    /// overflow enable (bit 20) it adds, and writes the same registers.
    #[test]
    fn config_mode_programs_the_ivy_bridge_default_group_alike() {
        let request: crate::pmu_config::spec::PmuConfigRequest =
            serde_json::from_value(json!({"pmus": {"ha": {"groups": [{"events": [
                {"alias": "requests", "op": "reads_local"},
                {"alias": "requests", "op": "reads_remote"},
                {"alias": "requests", "op": "writes_local"},
                {"alias": "requests", "op": "writes_remote"}
            ]}]}}}))
            .unwrap();
        let topology = PmuTopology {
            units: BTreeMap::from([(BoxType::Ha, BTreeMap::from([(0, BTreeSet::from([0, 1]))]))]),
        };
        let mut configs =
            crate::pmu_config::spec::validate(request, &topology, IntelServerCpuModel::IvyTown)
                .unwrap();
        let programmed = configs.remove(0).program.groups.remove(0);

        let mut expected = vec![(HA_UNIT_CTL_OFFSET, IVB.unit_freeze_and_reset())];
        expected.extend(HA_CTL_OFFSETS.into_iter().zip(IVB_REQUEST_UMASKS).map(
            |(offset, umask)| {
                (
                    offset,
                    counter_control(REQUESTS_EVENT, umask) & !COUNTER_OVERFLOW_ENABLE_BIT,
                )
            },
        ));
        assert_eq!(config_unit_writes(IVB, &programmed).unwrap(), expected);
    }

    #[test]
    fn programs_config_groups_on_each_model() {
        // Requests on counter 0 and TxR_AD.SNP on counter 3.
        let group = config_group(
            0,
            vec![
                config_counter(CounterSlot::General(0), 0x40_0301),
                config_counter(CounterSlot::General(3), 0x40_020f),
            ],
            BTreeMap::from([(1, BTreeSet::from([0]))]),
        );

        // Sandy Bridge-EP keeps the freeze enable bit 16 set.
        for (architecture, unit_control) in [(SNB, 0x1_0103), (IVB, 0x103)] {
            assert_eq!(
                config_unit_writes(architecture, &group).unwrap(),
                vec![
                    (0xf4, unit_control),
                    (0xd8, 0x42_0301),
                    (0xdc, 0),
                    (0xe0, 0),
                    (0xe4, 0x42_020f),
                ],
                "{architecture:?}"
            );
        }
    }

    #[test]
    fn numbers_home_agents_by_their_table_index() {
        let mut collector = test_collector(IVB);
        collector.units = [(0, 0), (0, 1), (1, 1)]
            .into_iter()
            .map(|(package, id)| test_unit(IVB, package, id, &[0; 0x100]))
            .collect();

        assert_eq!(
            collector.config_units(),
            BTreeMap::from([(0, BTreeSet::from([0, 1])), (1, BTreeSet::from([1]))])
        );
    }

    #[test]
    fn rejects_configs_home_agents_cannot_count() {
        let host = BTreeMap::from([(0, BTreeSet::from([0, 1])), (1, BTreeSet::from([0]))]);
        check_config(IVB, &snoop_program(), &host).unwrap();

        let mut cha = snoop_program();
        cha.box_type = BoxType::Cha;
        let mut package = snoop_program();
        package.groups[0].units.insert(2, BTreeSet::from([0]));
        let mut unit = snoop_program();
        unit.groups[0].units.insert(1, BTreeSet::from([1]));
        let mut fixed = snoop_program();
        fixed.groups[0]
            .counters
            .push(config_counter(CounterSlot::Fixed, 0x40_0000));
        let mut slot = snoop_program();
        slot.groups[0]
            .counters
            .push(config_counter(CounterSlot::General(4), 0x40_0121));
        let mut control = snoop_program();
        control.groups[0].counters[0].control = 1 << 32;
        let mut filters = snoop_program();
        filters.groups[0].filters = Some(ChaFilters {
            filter0: 0,
            filter1: 0,
        });

        for (program, error) in [
            (cha, "Ivy Bridge-EP HA cannot count a cha config"),
            (
                package,
                "HA config group 0 runs on package 2, which has no HA units on this host",
            ),
            (
                unit,
                "HA config group 0 runs on unit 1 of package 1, which this host does not have",
            ),
            (
                fixed,
                "HA config group 0: Sandy/Ivy Bridge-EP HA units have no counter slot fixed",
            ),
            (
                slot,
                "HA config group 0: Sandy/Ivy Bridge-EP HA units have no counter slot 4",
            ),
            (
                control,
                "HA config group 0 slot 0 control 0x100000000 does not fit in 32 bits",
            ),
            (
                filters,
                "HA config group 0 has filters, which HA units do not have",
            ),
        ] {
            assert_eq!(check_config(IVB, &program, &host), Err(error.to_string()));
        }
    }

    /// A config space whose counters 0, 1 and 3 hold 7, 9 and 11, counter 3
    /// above bit 48 junk, and whose control 2 holds a stale event.
    fn config_config_space() -> [u8; 0x100] {
        let mut config_space = [0_u8; 0x100];
        for (offset, value) in [(0xa0, 7_u64), (0xa8, 9), (0xb8, (0xff << 48) | 11)] {
            config_space[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        config_space[0xe0..0xe4].copy_from_slice(&0x40_0001_u32.to_le_bytes());
        config_space
    }

    #[test]
    fn counts_a_config_on_its_home_agents_and_restarts_when_it_changes() {
        // Sandy Bridge-EP's one home agent per package, Ivy Bridge-EP's two
        // in one package; the program runs on the second.
        for (architecture, ids, frozen) in [
            (SNB, [(1_u16, 0_u16), (0, 0)], 0x1_0100),
            (IVB, [(1, 1), (1, 0)], 0x100),
        ] {
            let target = ids[0];
            let mut collector = test_collector(architecture);
            collector.units = ids
                .into_iter()
                .rev()
                .map(|(package, id)| {
                    test_unit(architecture, package.into(), id, &config_config_space())
                })
                .collect();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let sample = |collector: &mut SnbHaCollector| {
                runtime
                    .block_on(collector.sample_config(Duration::from_millis(1), 0))
                    .unwrap()
            };
            let program = || requests_program(target);
            let expected = |factor: u64| {
                vec![
                    (
                        target.0,
                        CountUnit::Box(target.1),
                        CounterSlot::General(0),
                        7 * factor,
                    ),
                    (
                        target.0,
                        CountUnit::Box(target.1),
                        CounterSlot::General(1),
                        9 * factor,
                    ),
                    (
                        target.0,
                        CountUnit::Box(target.1),
                        CounterSlot::General(3),
                        11 * factor,
                    ),
                ]
            };

            collector.set_config(Some(config(program()))).unwrap();
            for factor in [1, 2] {
                let counts = sample(&mut collector);
                assert_eq!(count_values(&counts), expected(factor), "{architecture:?}");
                assert!(counts.running[0].seconds >= 0.001);
            }
            // The target ends frozen with the group's controls; the other
            // home agent is in no group and keeps what it had.
            assert_eq!(
                registers(&collector.units[1]),
                [frozen, 0x42_0301, 0x42_0c01, 0, 0x42_0000],
                "{architecture:?}"
            );
            assert_eq!(
                registers(&collector.units[0]),
                [0, 0, 0, 0x40_0001, 0],
                "{architecture:?}"
            );

            // An invalid config leaves the current one and its counts alone.
            let mut elsewhere = program();
            elsewhere.groups[0].units = BTreeMap::from([(2, BTreeSet::from([0]))]);
            assert!(collector.set_config(Some(config(elsewhere))).is_err());
            assert_eq!(count_values(&sample(&mut collector)), expected(3));

            // Setting a config, even the same one again, restarts its counts.
            collector.set_config(Some(config(program()))).unwrap();
            let restarted = collector.config.as_ref().unwrap().counts();
            assert!(count_values(&restarted).iter().all(|count| count.3 == 0));
            assert_eq!(count_values(&sample(&mut collector)), expected(1));

            collector.set_config(None).unwrap();
            assert!(!collector.has_config());
            assert_eq!(
                runtime.block_on(collector.sample_config(Duration::from_millis(1), 0)),
                Err(format!(
                    "no PMU config is set for {} HA",
                    architecture.name()
                ))
            );
        }
    }

    /// (package, unit, group, slot, count) of each count.
    fn group_counts(counts: &UncoreCounts) -> Vec<(u16, CountUnit, u8, CounterSlot, u64)> {
        counts
            .counts
            .iter()
            .map(|count| {
                (
                    count.package,
                    count.unit,
                    count.group,
                    count.slot,
                    count.value,
                )
            })
            .collect()
    }

    #[test]
    fn runs_spatial_and_temporal_configs_through_the_shared_schedule() {
        let reads = || vec![config_counter(CounterSlot::General(0), 0x40_0301)];
        let writes = || vec![config_counter(CounterSlot::General(1), 0x40_0c01)];
        let on = |id: u16| BTreeMap::from([(0, BTreeSet::from([id]))]);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let ivb_collector = || {
            let mut collector = test_collector(IVB);
            collector.units = vec![
                test_unit(IVB, 0, 0, &config_config_space()),
                test_unit(IVB, 0, 1, &config_config_space()),
            ];
            collector
        };

        // Spatial: one slice, HA0 counts group 0 and HA1 group 1.
        let mut collector = ivb_collector();
        collector
            .set_config(Some(config(config_program(
                BoxType::Ha,
                Grouping::Spatial,
                vec![
                    config_group(0, reads(), on(0)),
                    config_group(1, writes(), on(1)),
                ],
            ))))
            .unwrap();
        let counts = runtime
            .block_on(collector.sample_config(Duration::from_millis(1), 0))
            .unwrap();
        assert_eq!(
            group_counts(&counts),
            [
                (0, CountUnit::Box(0), 0, CounterSlot::General(0), 7),
                (0, CountUnit::Box(1), 1, CounterSlot::General(1), 9),
            ]
        );
        assert_eq!(registers(&collector.units[0])[1..3], [0x42_0301, 0]);
        assert_eq!(registers(&collector.units[1])[1..3], [0, 0x42_0c01]);

        // Temporal: both groups take turns on HA1, from the group the cycle
        // gives, and the last one stays programmed.
        for (cycle, last) in [(0, [0, 0x42_0c01]), (1, [0x42_0301, 0])] {
            let mut collector = ivb_collector();
            let program = config_program(
                BoxType::Ha,
                Grouping::Temporal,
                vec![
                    config_group(0, reads(), on(1)),
                    config_group(1, writes(), on(1)),
                ],
            );
            collector.set_config(Some(config(program))).unwrap();
            let counts = runtime
                .block_on(collector.sample_config(Duration::from_millis(2), cycle))
                .unwrap();

            assert_eq!(
                group_counts(&counts),
                [
                    (0, CountUnit::Box(1), 0, CounterSlot::General(0), 7),
                    (0, CountUnit::Box(1), 1, CounterSlot::General(1), 9),
                ],
                "cycle {cycle}"
            );
            assert!(counts.running.iter().all(|running| running.seconds > 0.0));
            assert_eq!(registers(&collector.units[1])[1..3], last, "cycle {cycle}");
            assert_eq!(registers(&collector.units[0]), [0, 0, 0, 0x40_0001, 0]);
        }
    }

    #[test]
    fn routes_configs_through_the_ha_dispatcher() {
        for architecture in [SNB, IVB] {
            let mut snb = test_collector(architecture);
            snb.units = vec![test_unit(architecture, 1, 0, &config_config_space())];
            let mut collector = match architecture {
                SnbHaArchitecture::Snb => super::super::HaCollector::Snb(snb),
                SnbHaArchitecture::Ivb => super::super::HaCollector::Ivb(snb),
            };

            assert_eq!(
                ConfigurableCollector::config_units(&collector),
                BTreeMap::from([(1, BTreeSet::from([0]))])
            );
            collector
                .set_config(Some(config(requests_program((1, 0)))))
                .unwrap();
            assert!(collector.has_config());
            collector.set_config(None).unwrap();
            assert!(!collector.has_config());
        }
    }

    /// Snoop responses RspI and RspS on Ivy Bridge-EP's HA1 of package 0.
    fn snoop_program() -> ProgrammedBoxType {
        config_program(
            BoxType::Ha,
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x40_0121),
                    config_counter(CounterSlot::General(1), 0x40_0221),
                ],
                BTreeMap::from([(0, BTreeSet::from([1]))]),
            )],
        )
    }

    /// REQUESTS.READS, REQUESTS.WRITES and CLOCKTICKS on counters 0, 1 and
    /// 3 of the home agent `(package, unit)`.
    fn requests_program((package, unit): (u16, u16)) -> ProgrammedBoxType {
        config_program(
            BoxType::Ha,
            Grouping::Fixed,
            vec![config_group(
                0,
                vec![
                    config_counter(CounterSlot::General(0), 0x40_0301),
                    config_counter(CounterSlot::General(1), 0x40_0c01),
                    config_counter(CounterSlot::General(3), 0x40_0000),
                ],
                BTreeMap::from([(package, BTreeSet::from([unit]))]),
            )],
        )
    }

    /// A home agent whose config space is a temporary file holding
    /// `config_space`.
    fn test_unit(
        architecture: SnbHaArchitecture,
        package_id: u32,
        id: u16,
        config_space: &[u8],
    ) -> SnbHaUnit {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ocellus-snb-ha-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, config_space).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        SnbHaUnit {
            architecture,
            device: PciDevice::from_file(
                file,
                PciLocation {
                    bus: 0,
                    device: 0,
                    function: 0,
                    group: 0,
                },
            ),
            id,
            scope: scope(package_id),
        }
    }

    fn test_collector(architecture: SnbHaArchitecture) -> SnbHaCollector {
        SnbHaCollector {
            architecture,
            units: Vec::new(),
            config: None,
        }
    }

    fn scope(package_id: u32) -> HsxUncoreScope {
        HsxUncoreScope { package_id }
    }
}
