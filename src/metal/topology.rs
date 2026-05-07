use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
use nix::unistd::Pid;

const CPUID_EXTENDED_TOPOLOGY: u32 = 0x0b;
const CPUID_EXTENDED_TOPOLOGY_V2: u32 = 0x1f;
const TOPOLOGY_TYPE_CORE: u32 = 2;
const TOPOLOGY_TYPE_DIE: u32 = 5;
const TOPOLOGY_TYPE_DIE_GROUP: u32 = 6;
const TOPOLOGY_TYPE_INVALID: u32 = 0;
const TOPOLOGY_TYPE_MODULE: u32 = 3;
const TOPOLOGY_TYPE_SMT: u32 = 1;
const TOPOLOGY_TYPE_TILE: u32 = 4;

struct ScopedThreadAffinity {
    affinity: CpuSet,
}

impl ScopedThreadAffinity {
    fn current() -> Result<Self, String> {
        Ok(Self {
            affinity: sched_getaffinity(Pid::from_raw(0))
                .map_err(|error| format!("failed to read CPU affinity: {error}"))?,
        })
    }
}

impl Drop for ScopedThreadAffinity {
    fn drop(&mut self) {
        let _ = sched_setaffinity(Pid::from_raw(0), &self.affinity);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuTopology {
    pub cpu: u32,
    pub levels: Vec<TopologyLevel>,
    pub x2apic_id: u32,
}

impl CpuTopology {
    pub fn level_id(&self, kind: TopologyLevelKind) -> Option<u32> {
        self.levels
            .iter()
            .find(|level| level.kind == kind)
            .map(|level| level.id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopologyLevel {
    pub id: u32,
    pub kind: TopologyLevelKind,
    pub shift: u32,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum TopologyLevelKind {
    Smt,
    Core,
    Module,
    Tile,
    Die,
    DieGroup,
    Package,
    Unknown(u32),
}

impl TopologyLevelKind {
    fn from_raw(level_type: u32) -> Self {
        match level_type {
            TOPOLOGY_TYPE_SMT => Self::Smt,
            TOPOLOGY_TYPE_CORE => Self::Core,
            TOPOLOGY_TYPE_MODULE => Self::Module,
            TOPOLOGY_TYPE_TILE => Self::Tile,
            TOPOLOGY_TYPE_DIE => Self::Die,
            TOPOLOGY_TYPE_DIE_GROUP => Self::DieGroup,
            _ => Self::Unknown(level_type),
        }
    }
}

fn pin_to_cpu(cpu: u32) -> Result<(), String> {
    let cpu = usize::try_from(cpu).map_err(|error| format!("CPU id is invalid: {error}"))?;
    let mut affinity = CpuSet::new();
    affinity
        .set(cpu)
        .map_err(|error| format!("failed to build CPU affinity set: {error}"))?;
    sched_setaffinity(Pid::from_raw(0), &affinity)
        .map_err(|error| format!("failed to set CPU affinity: {error}"))
}

pub fn cpu_topologies() -> Result<Vec<CpuTopology>, String> {
    let _original_affinity = ScopedThreadAffinity::current()?;
    let mut topologies = Vec::new();

    for cpu in available_cpu_candidates()? {
        if pin_to_cpu(cpu).is_err() {
            continue;
        }

        topologies.push(current_cpu_topology(cpu)?);
    }

    if topologies.is_empty() {
        return Err("failed to discover CPU topology for any CPU".to_string());
    }

    Ok(topologies)
}

fn available_cpu_candidates() -> Result<Vec<u32>, String> {
    let mut cpus = Vec::new();

    for cpu in 0..CpuSet::count() {
        cpus.push(u32::try_from(cpu).map_err(|error| format!("CPU id is invalid: {error}"))?);
    }

    if cpus.is_empty() {
        return Err("system does not expose any CPU affinity slots".to_string());
    }

    Ok(cpus)
}

fn current_cpu_topology(cpu: u32) -> Result<CpuTopology, String> {
    cpu_topology_from_topology_leaf(cpu, topology_leaf)
}

fn cpu_topology_from_topology_leaf(
    cpu: u32,
    topology_leaf: impl FnOnce() -> Option<u32>,
) -> Result<CpuTopology, String> {
    let mut levels = Vec::new();
    let mut package_shift = None;
    let mut previous_shift = None;
    let mut x2apic_id = None;
    let leaf = topology_leaf()
        .ok_or_else(|| "failed to read CPU topology from CPUID extended topology".to_string())?;

    for level_number in 0.. {
        let level = cpuid_level(leaf, level_number);
        let level_shift = level.eax & 0x1f;
        let level_type = (level.ecx >> 8) & 0xff;

        if level_type == TOPOLOGY_TYPE_INVALID {
            break;
        }

        let low_shift = previous_shift.unwrap_or(0);
        levels.push(TopologyLevel {
            id: topology_level_id(level.edx, low_shift, level_shift),
            kind: TopologyLevelKind::from_raw(level_type),
            shift: level_shift,
        });

        previous_shift = Some(level_shift);
        x2apic_id = Some(level.edx);
        package_shift = Some(level_shift);
    }

    match (x2apic_id, package_shift) {
        (Some(x2apic_id), Some(package_shift)) => {
            let package_id = x2apic_id >> package_shift;
            levels.push(TopologyLevel {
                id: package_id,
                kind: TopologyLevelKind::Package,
                shift: package_shift,
            });

            Ok(CpuTopology {
                cpu,
                levels,
                x2apic_id,
            })
        }
        _ => Err("failed to read CPU topology from CPUID extended topology".to_string()),
    }
}

fn topology_level_id(x2apic_id: u32, low_shift: u32, high_shift: u32) -> u32 {
    let level_bits = high_shift.saturating_sub(low_shift);

    if level_bits == 0 {
        return 0;
    }

    (x2apic_id >> low_shift) & ((1_u32 << level_bits) - 1)
}

fn cpuid_level(leaf: u32, level_number: u32) -> raw_cpuid::CpuIdResult {
    #[cfg(test)]
    if leaf == tests::CPUID_TEST_TOPOLOGY || leaf == tests::CPUID_TEST_MULTI_DIE {
        return tests::test_topology_level(leaf, level_number);
    }

    raw_cpuid::cpuid!(leaf, level_number)
}

fn topology_leaf() -> Option<u32> {
    let maximum_leaf = raw_cpuid::cpuid!(0).eax;

    if maximum_leaf >= CPUID_EXTENDED_TOPOLOGY_V2 {
        Some(CPUID_EXTENDED_TOPOLOGY_V2)
    } else if maximum_leaf >= CPUID_EXTENDED_TOPOLOGY {
        Some(CPUID_EXTENDED_TOPOLOGY)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_cpu_candidates() {
        assert!(!available_cpu_candidates().unwrap().is_empty());
    }

    #[test]
    fn tolerates_unknown_non_zero_topology_level_types() {
        let topology = cpu_topology_from_topology_leaf(0, || Some(CPUID_TEST_TOPOLOGY)).unwrap();

        assert_eq!(topology.x2apic_id, 0x100);
        assert_eq!(topology.levels[0].kind, TopologyLevelKind::Smt);
        assert_eq!(topology.levels[0].id, 0);
        assert_eq!(topology.levels[1].kind, TopologyLevelKind::Core);
        assert_eq!(topology.levels[1].id, 0);
        assert_eq!(topology.levels[2].kind, TopologyLevelKind::Unknown(7));
        assert_eq!(topology.levels[2].id, 0);
        assert_eq!(topology.level_id(TopologyLevelKind::Package), Some(1));
    }

    #[test]
    fn computes_die_id_within_package() {
        let topology = cpu_topology_from_topology_leaf(0, || Some(CPUID_TEST_MULTI_DIE)).unwrap();

        assert_eq!(topology.level_id(TopologyLevelKind::Die), Some(3));
        assert_eq!(topology.level_id(TopologyLevelKind::Package), Some(1));
        assert_eq!(topology.levels[2].kind, TopologyLevelKind::Die);
        assert_eq!(topology.levels[2].id, 3);
    }

    pub const CPUID_TEST_TOPOLOGY: u32 = u32::MAX;
    pub const CPUID_TEST_MULTI_DIE: u32 = u32::MAX - 1;

    pub fn test_topology_level(leaf: u32, level_number: u32) -> raw_cpuid::CpuIdResult {
        if leaf == CPUID_TEST_MULTI_DIE {
            return match level_number {
                0 => cpuid_result(1, 1, 0x100, 0x1c0),
                1 => cpuid_result(6, 1, 0x200, 0x1c0),
                2 => cpuid_result(8, 1, 0x500, 0x1c0),
                _ => cpuid_result(0, 0, 0, 0),
            };
        }

        match level_number {
            0 => cpuid_result(1, 1, 0x100, 0x100),
            1 => cpuid_result(7, 1, 0x200, 0x100),
            2 => cpuid_result(8, 1, 0x700, 0x100),
            _ => cpuid_result(0, 0, 0, 0),
        }
    }

    fn cpuid_result(eax: u32, ebx: u32, ecx: u32, edx: u32) -> raw_cpuid::CpuIdResult {
        raw_cpuid::CpuIdResult { eax, ebx, ecx, edx }
    }
}
