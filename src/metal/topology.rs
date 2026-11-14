use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
use nix::unistd::Pid;

const CPUID_EXTENDED_TOPOLOGY: u32 = 0x0b;
const CPUID_EXTENDED_TOPOLOGY_V2: u32 = 0x1f;
const ONLINE_CPUS_PATH: &str = "/sys/devices/system/cpu/online";
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

/// Reads the topology of every online CPU. Fails if any online CPU cannot be
/// pinned, so that no caller works from a partial CPU list.
pub fn cpu_topologies() -> Result<Vec<CpuTopology>, String> {
    let _original_affinity = ScopedThreadAffinity::current()?;

    cpu_topologies_from_online_cpus(&online_cpus()?, pin_to_cpu, current_cpu_topology)
}

fn cpu_topologies_from_online_cpus(
    online_cpus: &[u32],
    mut pin_to_cpu: impl FnMut(u32) -> Result<(), String>,
    mut cpu_topology: impl FnMut(u32) -> Result<CpuTopology, String>,
) -> Result<Vec<CpuTopology>, String> {
    let mut topologies = Vec::new();
    let mut unpinnable_cpus = Vec::new();
    let mut first_pin_error = None;

    for &cpu in online_cpus {
        if let Err(error) = pin_to_cpu(cpu) {
            unpinnable_cpus.push(cpu);
            first_pin_error.get_or_insert(error);
            continue;
        }

        topologies.push(cpu_topology(cpu)?);
    }

    if let Some(error) = first_pin_error {
        return Err(format!(
            "cannot pin to online CPUs {} ({error}); the cgroup cpuset of the ocellus process likely excludes them, and ocellus must run where every online CPU is allowed",
            format_cpu_list(&unpinnable_cpus)
        ));
    }

    Ok(topologies)
}

fn online_cpus() -> Result<Vec<u32>, String> {
    let cpulist = std::fs::read_to_string(ONLINE_CPUS_PATH)
        .map_err(|error| format!("failed to read {ONLINE_CPUS_PATH}: {error}"))?;

    online_cpus_from_cpulist(&cpulist)
}

fn online_cpus_from_cpulist(cpulist: &str) -> Result<Vec<u32>, String> {
    let cpus = parse_cpu_list(cpulist).map_err(|error| {
        format!("failed to parse {ONLINE_CPUS_PATH} value {cpulist:?}: {error}")
    })?;

    if cpus.is_empty() {
        return Err(format!("{ONLINE_CPUS_PATH} lists no CPUs"));
    }

    Ok(cpus)
}

pub fn parse_cpu_list(cpulist: &str) -> Result<Vec<u32>, String> {
    let mut cpus = Vec::new();

    for item in cpulist.trim().split(',').filter(|item| !item.is_empty()) {
        match item.split_once('-') {
            Some((start, end)) => {
                let start = parse_cpu_id(start)?;
                let end = parse_cpu_id(end)?;
                if end < start {
                    return Err(format!("invalid descending CPU range {item}"));
                }
                cpus.extend(start..=end);
            }
            None => cpus.push(parse_cpu_id(item)?),
        }
    }

    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

pub fn format_cpu_list(cpus: &[u32]) -> String {
    if cpus.is_empty() {
        return String::new();
    }

    let mut cpus = cpus.to_vec();
    cpus.sort_unstable();
    cpus.dedup();

    let mut ranges = Vec::new();
    let mut start = cpus[0];
    let mut previous = cpus[0];
    for cpu in cpus.into_iter().skip(1) {
        if cpu == previous + 1 {
            previous = cpu;
            continue;
        }

        ranges.push(format_cpu_range(start, previous));
        start = cpu;
        previous = cpu;
    }
    ranges.push(format_cpu_range(start, previous));

    ranges.join(",")
}

fn format_cpu_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn parse_cpu_id(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|error| format!("invalid CPU id {value:?}: {error}"))
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
    fn parses_online_cpus() {
        assert_eq!(
            online_cpus_from_cpulist("0-3,6-7\n"),
            Ok(vec![0, 1, 2, 3, 6, 7])
        );
        assert!(online_cpus_from_cpulist("\n").is_err());
    }

    #[test]
    fn reads_topology_of_every_online_cpu() {
        let topologies =
            cpu_topologies_from_online_cpus(&[0, 1, 2, 3], |_| Ok(()), test_cpu_topology).unwrap();

        assert_eq!(
            topologies
                .iter()
                .map(|topology| topology.cpu)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn fails_on_online_cpu_that_cannot_be_pinned() {
        let error = cpu_topologies_from_online_cpus(
            &[0, 1, 2, 3],
            |cpu| match cpu {
                2 => Err("failed to set CPU affinity: EINVAL: Invalid argument".to_string()),
                _ => Ok(()),
            },
            test_cpu_topology,
        )
        .unwrap_err();

        assert_eq!(
            error,
            "cannot pin to online CPUs 2 (failed to set CPU affinity: EINVAL: Invalid argument); the cgroup cpuset of the ocellus process likely excludes them, and ocellus must run where every online CPU is allowed"
        );
    }

    #[test]
    fn names_every_online_cpu_that_cannot_be_pinned() {
        let error = cpu_topologies_from_online_cpus(
            &[0, 1, 2, 3, 4, 5, 6, 7],
            |cpu| match cpu {
                4..=7 => Ok(()),
                _ => Err("failed to set CPU affinity: EINVAL: Invalid argument".to_string()),
            },
            test_cpu_topology,
        )
        .unwrap_err();

        assert!(
            error.starts_with("cannot pin to online CPUs 0-3 ("),
            "{error}"
        );
    }

    #[test]
    fn skips_offline_cpus() {
        let online_cpus = online_cpus_from_cpulist("0-3,6-7\n").unwrap();
        let topologies = cpu_topologies_from_online_cpus(
            &online_cpus,
            |cpu| match cpu {
                4 | 5 => Err("failed to set CPU affinity: EINVAL: Invalid argument".to_string()),
                _ => Ok(()),
            },
            test_cpu_topology,
        )
        .unwrap();

        assert_eq!(
            topologies
                .iter()
                .map(|topology| topology.cpu)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 6, 7]
        );
    }

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0-2,4,6-7\n"), Ok(vec![0, 1, 2, 4, 6, 7]));
        assert_eq!(parse_cpu_list("4,2,2,3\n"), Ok(vec![2, 3, 4]));
        assert_eq!(parse_cpu_list("\n"), Ok(vec![]));
        assert!(parse_cpu_list("3-1").is_err());
    }

    #[test]
    fn formats_cpu_lists() {
        assert_eq!(format_cpu_list(&[]), "");
        assert_eq!(format_cpu_list(&[0, 1, 2, 4, 6, 7]), "0-2,4,6-7");
        assert_eq!(format_cpu_list(&[4, 2, 3, 2]), "2-4");
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

    fn test_cpu_topology(cpu: u32) -> Result<CpuTopology, String> {
        cpu_topology_from_topology_leaf(cpu, || Some(CPUID_TEST_TOPOLOGY))
    }

    fn cpuid_result(eax: u32, ebx: u32, ecx: u32, edx: u32) -> raw_cpuid::CpuIdResult {
        raw_cpuid::CpuIdResult { eax, ebx, ecx, edx }
    }
}
