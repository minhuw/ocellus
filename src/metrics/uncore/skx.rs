use std::collections::BTreeMap;
use std::time::Duration;

use crate::metal;
use crate::metal::topology::{CpuTopology, TopologyLevelKind};

pub const BYTES_PER_CACHE_LINE: f64 = 64.0;
pub const DEFAULT_MAX_SLICE: Duration = Duration::from_millis(100);
pub const SKX_IIO_STACK_COUNT: usize = 6;
pub const SKX_UNCORE_COUNTER_WIDTH: u32 = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkxIioStack {
    CbdmaDmi,
    Pcie0,
    Pcie1,
    Pcie2,
    Mcp0,
    Mcp1,
}

impl SkxIioStack {
    pub const ALL: [Self; SKX_IIO_STACK_COUNT] = [
        Self::CbdmaDmi,
        Self::Pcie0,
        Self::Pcie1,
        Self::Pcie2,
        Self::Mcp0,
        Self::Mcp1,
    ];

    pub const fn id(self) -> usize {
        match self {
            Self::CbdmaDmi => 0,
            Self::Pcie0 => 1,
            Self::Pcie1 => 2,
            Self::Pcie2 => 3,
            Self::Mcp0 => 4,
            Self::Mcp1 => 5,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::CbdmaDmi => "cbdma_dmi",
            Self::Pcie0 => "pcie0",
            Self::Pcie1 => "pcie1",
            Self::Pcie2 => "pcie2",
            Self::Mcp0 => "mcp0",
            Self::Mcp1 => "mcp1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct UncoreScope {
    pub die_group_id: u32,
    pub die_id: u32,
    pub package_id: u32,
}

impl UncoreScope {
    fn from_topology(topology: &CpuTopology) -> Result<Self, String> {
        Ok(Self {
            die_group_id: topology.level_id(TopologyLevelKind::DieGroup).unwrap_or(0),
            die_id: topology.level_id(TopologyLevelKind::Die).unwrap_or(0),
            package_id: topology
                .level_id(TopologyLevelKind::Package)
                .ok_or_else(|| "CPU topology is missing package level".to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UncoreLeader {
    pub cpu: u32,
    pub scope: UncoreScope,
}

pub fn events_per_second(events: u64, duration: Duration) -> f64 {
    let elapsed = duration.as_secs_f64();

    if elapsed == 0.0 {
        0.0
    } else {
        events as f64 / elapsed
    }
}

pub fn frequency_hz(ticks: u64, duration: Duration) -> f64 {
    events_per_second(ticks, duration)
}

pub fn mask_counter(counter: u64, width: u32) -> u64 {
    counter & ((1_u64 << width) - 1)
}

pub fn measurement_round_count(interval: Duration, group_count: usize) -> usize {
    let interval_nanos = interval.as_nanos();
    let max_round_nanos = DEFAULT_MAX_SLICE.as_nanos() * group_count as u128;
    let rounds = interval_nanos.div_ceil(max_round_nanos).max(1);

    usize::try_from(rounds).unwrap_or(usize::MAX)
}

pub fn queue_residency_seconds(
    occupancy: u64,
    inserts: u64,
    ticks: u64,
    duration: Duration,
) -> f64 {
    if inserts == 0 || ticks == 0 {
        return 0.0;
    }

    let seconds_per_tick = duration.as_secs_f64() / ticks as f64;
    seconds_per_tick * occupancy as f64 / inserts as f64
}

pub fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub fn scale_to_enabled(value: u64, enabled: Duration, running: Duration) -> u64 {
    if running.is_zero() {
        return 0;
    }

    (value as f64 * enabled.as_secs_f64() / running.as_secs_f64()) as u64
}

pub fn uncore_leaders() -> Result<Vec<UncoreLeader>, String> {
    let mut leaders = BTreeMap::new();

    for topology in metal::topology::cpu_topologies()? {
        leaders
            .entry(UncoreScope::from_topology(&topology)?)
            .or_insert(topology.cpu);
    }

    if leaders.is_empty() {
        return Err("failed to discover any uncore scope leaders".to_string());
    }

    Ok(leaders
        .into_iter()
        .map(|(scope, cpu)| UncoreLeader { cpu, scope })
        .collect())
}

pub fn wrapping_delta(previous: u64, current: u64, width: u32) -> u64 {
    current.wrapping_sub(previous) & ((1_u64 << width) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_counter_width() {
        assert_eq!(mask_counter((1_u64 << 50) | 7, SKX_UNCORE_COUNTER_WIDTH), 7);
    }

    #[test]
    fn scales_to_enabled_time() {
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::from_millis(100)),
            1_000
        );
        assert_eq!(
            scale_to_enabled(100, Duration::from_secs(1), Duration::ZERO),
            0
        );
    }
}
