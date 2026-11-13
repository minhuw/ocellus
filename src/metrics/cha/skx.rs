use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal::arch::skx::pmon;
use crate::metal::msr::Msr;
use crate::metal::pci;
use crate::metrics::EventProfile;
use crate::metrics::cha::{
    CHA_COUNTER_COUNT, ChaEventKind, ChaEventMeasurement, ChaRequestLocality, ChaTransactionLabel,
    LLC_LOOKUP_STATES, bytes_per_second, event_rate, linux_uncore_unit_ids,
    llc_lookup_metrics_for_states, llc_victim_metrics, pci_location_for_cpu, required_measurement,
    scale_measurement_value,
};
pub use crate::metrics::cha::{
    ChaCacheState, ChaEvictionMetrics, ChaHaRequestLocality, ChaHaRequestMetrics,
    ChaLlcLookupMetrics, ChaLlcVictimMetrics, ChaLookupOperation, ChaMultiplexMode,
    ChaNoCreditDirection, ChaNoCreditMetrics, ChaRequestOperation, ChaRequestQueueMetrics,
    ChaRequestSource, ChaRxcMetrics, ChaRxcQueue, ChaScopeMetrics, ChaSfEvictionMetrics,
    ChaTorInsertMetrics, ChaTorOpcode, ChaTorResult, ChaTransactionMetrics, ChaTransactionResult,
    ChaTransactionResultMetrics,
};
use crate::metrics::common::topology_label;
use crate::metrics::uncore::skx::{
    SKX_UNCORE_COUNTER_WIDTH, UncoreScope, frequency_hz, mask_counter, measurement_round_count,
    queue_residency_seconds, ratio, uncore_leaders,
};
use crate::pmu_config::types::{
    BoxType, ChaFilters, CountAccumulator, CounterSlot, Grouping, PmuConfig, ProgrammedBoxType,
    ProgrammedGroup, UncoreCounts,
};

const SKX_CHA_EVENT_GROUP_COUNT: usize = 37;
const SKX_CAPID6_OFFSET: u64 = 0x9c;
const SKX_CAPID_DEVICE_ID: u16 = 0x2083;
const SKX_CHA_CAPID6_MASK: u32 = (1_u32 << SKX_MAX_CHA_COUNT) - 1;
const SKX_MAX_CHA_COUNT: usize = 28;

const CHA_COUNTER_BASE: u64 = 0x0e08;
const CHA_CONTROL_BASE: u64 = 0x0e01;
const CHA_FILTER0_BASE: u64 = 0x0e05;
const CHA_FILTER1_BASE: u64 = 0x0e06;
const CHA_UNIT_CONTROL_BASE: u64 = 0x0e00;
const CHA_UNIT_STRIDE: u64 = 0x10;

pub(crate) const CHA_FILTER0_STATE_SHIFT: u32 = 17;
pub(crate) const CHA_FILTER1_REMOTE_BIT: u32 = 1 << 0;
pub(crate) const CHA_FILTER1_LOCAL_BIT: u32 = 1 << 1;
pub(crate) const CHA_FILTER1_ALL_OPCODE_BIT: u32 = 1 << 3;
pub(crate) const CHA_FILTER1_NEAR_MEMORY_BIT: u32 = 1 << 4;
pub(crate) const CHA_FILTER1_NOT_NEAR_MEMORY_BIT: u32 = 1 << 5;
pub(crate) const CHA_FILTER1_OPCODE0_SHIFT: u32 = 9;
pub(crate) const CHA_FILTER1_OPCODE1_SHIFT: u32 = 19;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum ChaTransactionKind {
    IaClFlush,
    IaDrd,
    IaItoM,
    IaRfo,
    IaWbMtoI,
    IoClFlush,
    IoItoM,
    IoItoMCacheNear,
    IoPciRdCur,
    IoWbMtoI,
}

impl ChaTransactionKind {
    const fn label(self) -> ChaTransactionLabel {
        match self {
            Self::IaClFlush => ChaTransactionLabel::new("ia_clflush"),
            Self::IaWbMtoI => ChaTransactionLabel::new("ia_wbmtoi"),
            Self::IaDrd => ChaTransactionLabel::new("ia_drd"),
            Self::IaItoM => ChaTransactionLabel::new("ia_itom"),
            Self::IaRfo => ChaTransactionLabel::new("ia_rfo"),
            Self::IoClFlush => ChaTransactionLabel::new("io_clflush"),
            Self::IoItoM => ChaTransactionLabel::new("io_itom"),
            Self::IoItoMCacheNear => ChaTransactionLabel::new("io_itomcachenear"),
            Self::IoPciRdCur => ChaTransactionLabel::new("io_pcirdcur"),
            Self::IoWbMtoI => ChaTransactionLabel::new("io_wbmtoi"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChaFilter0 {
    state: u16,
}

impl ChaFilter0 {
    const fn none() -> Self {
        Self { state: 0 }
    }

    const fn llc_lookup_state(state: ChaCacheState) -> Self {
        Self {
            state: state.filter0_bits(),
        }
    }

    #[cfg(test)]
    const fn llc_lookup_any_state() -> Self {
        Self {
            state: ChaCacheState::llc_lookup_any_state_bits(),
        }
    }

    const fn value(self) -> u32 {
        (self.state as u32) << CHA_FILTER0_STATE_SHIFT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChaFilter1 {
    local: bool,
    opcode0: u16,
    opcode1: u16,
    remote: bool,
}

impl ChaFilter1 {
    const fn total_all_opcodes() -> Self {
        Self {
            local: true,
            opcode0: 0,
            opcode1: 0,
            remote: true,
        }
    }

    const fn total_opcode(opcode0: u16) -> Self {
        Self {
            local: true,
            opcode0,
            opcode1: 0,
            remote: true,
        }
    }

    const fn with_locality(self, locality: ChaRequestLocality) -> Self {
        match locality {
            ChaRequestLocality::All => self,
            ChaRequestLocality::Local => Self {
                local: true,
                remote: false,
                ..self
            },
        }
    }

    const fn value(self) -> u32 {
        let remote = if self.remote {
            CHA_FILTER1_REMOTE_BIT
        } else {
            0
        };
        let local = if self.local { CHA_FILTER1_LOCAL_BIT } else { 0 };
        let all_opcodes = if self.opcode0 == 0 && self.opcode1 == 0 {
            CHA_FILTER1_ALL_OPCODE_BIT
        } else {
            0
        };

        remote
            | local
            | all_opcodes
            | CHA_FILTER1_NEAR_MEMORY_BIT
            | CHA_FILTER1_NOT_NEAR_MEMORY_BIT
            | ((self.opcode0 as u32) << CHA_FILTER1_OPCODE0_SHIFT)
            | ((self.opcode1 as u32) << CHA_FILTER1_OPCODE1_SHIFT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChaEventSpec {
    event: u8,
    kind: ChaEventKind,
    umask: u8,
}

impl ChaEventSpec {
    const fn sum(kind: ChaEventKind, event: u8, umask: u8) -> Self {
        Self { event, kind, umask }
    }

    const fn clockticks(kind: ChaEventKind) -> Self {
        Self::sum(kind, 0x00, 0x00)
    }

    const fn unused() -> Self {
        Self::sum(ChaEventKind::Unused, 0x00, 0x00)
    }
}

/// Stable identity of a CHA event group, exported as the `group` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChaGroupId {
    Eviction,
    HaRequests,
    LlcLookup(ChaCacheState),
    LlcVictims,
    LlcVictimsLocal,
    NoCredits,
    RequestQueue(ChaRequestSource),
    Rxc(ChaRxcQueue),
    SfEvictions,
    Tor(ChaTorOpcode),
    Transaction(ChaTransactionKind, ChaTransactionResult),
}

impl ChaGroupId {
    fn label(self) -> String {
        match self {
            Self::Eviction => "eviction".to_string(),
            Self::HaRequests => "ha_requests".to_string(),
            Self::LlcLookup(state) => format!("llc_lookup_{}", state.label()),
            Self::LlcVictims => "llc_victims".to_string(),
            Self::LlcVictimsLocal => "llc_victims_local".to_string(),
            Self::NoCredits => "no_credits".to_string(),
            Self::RequestQueue(source) => format!("request_queue_{}", source.label()),
            Self::Rxc(queue) => format!("rxc_{}", queue.label()),
            Self::SfEvictions => "sf_evictions".to_string(),
            Self::Tor(opcode) => format!("tor_{}", opcode.label()),
            Self::Transaction(transaction, result) => {
                format!("trx_{}_{}", transaction.label().as_str(), result.label())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChaEventGroup {
    events: [ChaEventSpec; CHA_COUNTER_COUNT],
    filter0: ChaFilter0,
    filter1: ChaFilter1,
    id: ChaGroupId,
}

impl ChaEventGroup {
    const fn eviction() -> Self {
        Self {
            id: ChaGroupId::Eviction,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(ChaEventKind::EvictionOccupancy, 0x36, 0x32),
                ChaEventSpec::sum(ChaEventKind::EvictionInsert, 0x35, 0x32),
                ChaEventSpec::clockticks(ChaEventKind::EvictionClockticks),
                ChaEventSpec::unused(),
            ],
        }
    }

    const fn ha_requests() -> Self {
        Self {
            id: ChaGroupId::HaRequests,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::HaRequest(ChaHaRequestLocality::Local, ChaRequestOperation::Read),
                    0x50,
                    0x01,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Remote,
                        ChaRequestOperation::Read,
                    ),
                    0x50,
                    0x02,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Local,
                        ChaRequestOperation::Write,
                    ),
                    0x50,
                    0x04,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Remote,
                        ChaRequestOperation::Write,
                    ),
                    0x50,
                    0x08,
                ),
            ],
        }
    }

    const fn llc_lookup(state: ChaCacheState) -> Self {
        Self {
            id: ChaGroupId::LlcLookup(state),
            filter0: ChaFilter0::llc_lookup_state(state),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Read),
                    0x34,
                    0x03,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Write),
                    0x34,
                    0x05,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::RemoteSnoop),
                    0x34,
                    0x09,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                    0x34,
                    0x11,
                ),
            ],
        }
    }

    const fn llc_victims() -> Self {
        Self {
            id: ChaGroupId::LlcVictims,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::M), 0x37, 0x01),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::E), 0x37, 0x02),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::S), 0x37, 0x04),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::F), 0x37, 0x08),
            ],
        }
    }

    /// LLC victims of local lines (LOCAL_M/E/S/F), as peacock counted them.
    const fn llc_victims_local() -> Self {
        Self {
            id: ChaGroupId::LlcVictimsLocal,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::M), 0x37, 0x21),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::E), 0x37, 0x22),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::S), 0x37, 0x24),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::F), 0x37, 0x28),
            ],
        }
    }

    const fn no_credits() -> Self {
        Self {
            id: ChaGroupId::NoCredits,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::NoCredits(ChaNoCreditDirection::Read),
                    0x58,
                    0x3f,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::NoCredits(ChaNoCreditDirection::Write),
                    0x5a,
                    0x3f,
                ),
                ChaEventSpec::clockticks(ChaEventKind::NoCreditsClockticks),
                ChaEventSpec::unused(),
            ],
        }
    }

    const fn request_queue(source: ChaRequestSource) -> Self {
        Self {
            id: ChaGroupId::RequestQueue(source),
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::RequestQueueOccupancy(source),
                    0x36,
                    source.all_umask(),
                ),
                ChaEventSpec::sum(
                    ChaEventKind::RequestQueueInsert(source),
                    0x35,
                    source.all_umask(),
                ),
                ChaEventSpec::clockticks(ChaEventKind::RequestQueueClockticks(source)),
                ChaEventSpec::unused(),
            ],
        }
    }

    const fn rxc(queue: ChaRxcQueue) -> Self {
        let umask = queue.umask();

        Self {
            id: ChaGroupId::Rxc(queue),
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(ChaEventKind::RxcOccupancy(queue), 0x11, umask),
                ChaEventSpec::sum(ChaEventKind::RxcInsert(queue), 0x13, umask),
                ChaEventSpec::clockticks(ChaEventKind::RxcClockticks(queue)),
                ChaEventSpec::unused(),
            ],
        }
    }

    const fn sf_evictions() -> Self {
        Self {
            id: ChaGroupId::SfEvictions,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_all_opcodes(),
            events: [
                ChaEventSpec::sum(ChaEventKind::SfEviction(ChaCacheState::M), 0x3d, 0x01),
                ChaEventSpec::sum(ChaEventKind::SfEviction(ChaCacheState::E), 0x3d, 0x02),
                ChaEventSpec::sum(ChaEventKind::SfEviction(ChaCacheState::S), 0x3d, 0x04),
                ChaEventSpec::unused(),
            ],
        }
    }

    const fn transaction(
        transaction: ChaTransactionKind,
        result: ChaTransactionResult,
        source: ChaRequestSource,
        opcode0: u16,
    ) -> Self {
        let id = ChaGroupId::Transaction(transaction, result);
        let umask = source.result_umask(result);
        let transaction = transaction.label();

        Self {
            id,
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_opcode(opcode0),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::TransactionOccupancy(transaction, result),
                    0x36,
                    umask,
                ),
                ChaEventSpec::sum(
                    ChaEventKind::TransactionInsert(transaction, result),
                    0x35,
                    umask,
                ),
                ChaEventSpec::clockticks(ChaEventKind::TransactionClockticks(transaction, result)),
                ChaEventSpec::unused(),
            ],
        }
    }

    /// TOR inserts of one opcode: IO and IA, all and misses, in one time slice.
    const fn tor(opcode: ChaTorOpcode) -> Self {
        Self {
            id: ChaGroupId::Tor(opcode),
            filter0: ChaFilter0::none(),
            filter1: ChaFilter1::total_opcode(skx_tor_opcode(opcode)),
            events: [
                ChaEventSpec::sum(
                    ChaEventKind::TorInsert(opcode, ChaRequestSource::Io, ChaTorResult::All),
                    0x35,
                    ChaRequestSource::Io.all_umask(),
                ),
                ChaEventSpec::sum(
                    ChaEventKind::TorInsert(opcode, ChaRequestSource::Io, ChaTorResult::Miss),
                    0x35,
                    ChaRequestSource::Io.result_umask(ChaTransactionResult::Miss),
                ),
                ChaEventSpec::sum(
                    ChaEventKind::TorInsert(opcode, ChaRequestSource::Ia, ChaTorResult::All),
                    0x35,
                    ChaRequestSource::Ia.all_umask(),
                ),
                ChaEventSpec::sum(
                    ChaEventKind::TorInsert(opcode, ChaRequestSource::Ia, ChaTorResult::Miss),
                    0x35,
                    ChaRequestSource::Ia.result_umask(ChaTransactionResult::Miss),
                ),
            ],
        }
    }
}

/// Skylake-SP IDI opcodes (uncore guide table 3-1) for the TOR insert groups.
const fn skx_tor_opcode(opcode: ChaTorOpcode) -> u16 {
    match opcode {
        ChaTorOpcode::ItoM => 0x248,
        ChaTorOpcode::RdCur => 0x21e,
        ChaTorOpcode::Rfo => 0x200,
        ChaTorOpcode::WbMtoI => 0x244,
    }
}

const CHA_TOR_OPCODES: [ChaTorOpcode; 4] = [
    ChaTorOpcode::ItoM,
    ChaTorOpcode::Rfo,
    ChaTorOpcode::RdCur,
    ChaTorOpcode::WbMtoI,
];

const CHA_TRANSACTIONS: [ChaTransactionKind; 10] = [
    ChaTransactionKind::IoPciRdCur,
    ChaTransactionKind::IoItoM,
    ChaTransactionKind::IoItoMCacheNear,
    ChaTransactionKind::IoWbMtoI,
    ChaTransactionKind::IaDrd,
    ChaTransactionKind::IaRfo,
    ChaTransactionKind::IaItoM,
    ChaTransactionKind::IaClFlush,
    ChaTransactionKind::IaWbMtoI,
    ChaTransactionKind::IoClFlush,
];

const SKX_CHA_EVENT_GROUPS: [ChaEventGroup; SKX_CHA_EVENT_GROUP_COUNT] = [
    ChaEventGroup::eviction(),
    ChaEventGroup::llc_lookup(ChaCacheState::SfS),
    ChaEventGroup::llc_lookup(ChaCacheState::SfE),
    ChaEventGroup::llc_lookup(ChaCacheState::SfH),
    ChaEventGroup::llc_lookup(ChaCacheState::I),
    ChaEventGroup::llc_lookup(ChaCacheState::S),
    ChaEventGroup::llc_lookup(ChaCacheState::E),
    ChaEventGroup::llc_lookup(ChaCacheState::M),
    ChaEventGroup::llc_lookup(ChaCacheState::F),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoPciRdCur,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x21e,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoPciRdCur,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x21e,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoM,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoM,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoWbMtoI,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x244,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoWbMtoI,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x244,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaWbMtoI,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x244,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaWbMtoI,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x244,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaClFlush,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x218,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaClFlush,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x218,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoClFlush,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x218,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoClFlush,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x218,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoMCacheNear,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x200,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoMCacheNear,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x200,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaDrd,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x202,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaDrd,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x202,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaItoM,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaItoM,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaRfo,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x200,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaRfo,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x200,
    ),
    ChaEventGroup::request_queue(ChaRequestSource::Ia),
    ChaEventGroup::request_queue(ChaRequestSource::Io),
    ChaEventGroup::rxc(ChaRxcQueue::Irq),
    ChaEventGroup::rxc(ChaRxcQueue::Prq),
    ChaEventGroup::llc_victims(),
    ChaEventGroup::no_credits(),
    ChaEventGroup::ha_requests(),
    ChaEventGroup::sf_evictions(),
];

/// mem-io two-type write hit (E1): one TOR insert group per opcode the
/// `ItoM_M`/`ItoM_SF_E` subtractions use, plus the M and SF_E lookups.
const SKX_MEM_IO_2HIT_CHA_EVENT_GROUPS: [ChaEventGroup; 6] = [
    ChaEventGroup::tor(ChaTorOpcode::ItoM),
    ChaEventGroup::tor(ChaTorOpcode::Rfo),
    ChaEventGroup::tor(ChaTorOpcode::RdCur),
    ChaEventGroup::tor(ChaTorOpcode::WbMtoI),
    ChaEventGroup::llc_lookup(ChaCacheState::M),
    ChaEventGroup::llc_lookup(ChaCacheState::SfE),
];

/// mem-io limited/special way experiments (E2, E3): ItoM hit rate only.
const SKX_MEM_IO_ITOM_CHA_EVENT_GROUPS: [ChaEventGroup; 1] =
    [ChaEventGroup::tor(ChaTorOpcode::ItoM)];

/// mem-io BASIC metric list (E6-E10): the twelve groups of peacock 3565bd2,
/// in its order, so each group gets the same time slice as the original.
const SKX_MEM_IO_BASIC_CHA_EVENT_GROUPS: [ChaEventGroup; 12] = [
    ChaEventGroup::eviction(),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoPciRdCur,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x21e,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoPciRdCur,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x21e,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoM,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoM,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x248,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoWbMtoI,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x244,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoMCacheNear,
        ChaTransactionResult::Hit,
        ChaRequestSource::Io,
        0x200,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IoItoMCacheNear,
        ChaTransactionResult::Miss,
        ChaRequestSource::Io,
        0x200,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaDrd,
        ChaTransactionResult::Hit,
        ChaRequestSource::Ia,
        0x202,
    ),
    ChaEventGroup::transaction(
        ChaTransactionKind::IaDrd,
        ChaTransactionResult::Miss,
        ChaRequestSource::Ia,
        0x202,
    ),
    ChaEventGroup::llc_victims_local(),
    ChaEventGroup::ha_requests(),
];

fn skx_cha_event_groups(profile: EventProfile) -> &'static [ChaEventGroup] {
    match profile {
        EventProfile::Default => &SKX_CHA_EVENT_GROUPS,
        EventProfile::MemIo2Hit => &SKX_MEM_IO_2HIT_CHA_EVENT_GROUPS,
        EventProfile::MemIoBasic => &SKX_MEM_IO_BASIC_CHA_EVENT_GROUPS,
        EventProfile::MemIoItom => &SKX_MEM_IO_ITOM_CHA_EVENT_GROUPS,
    }
}

pub(crate) fn event_group_count(profile: EventProfile) -> usize {
    skx_cha_event_groups(profile).len()
}

#[derive(Clone, Copy, Debug)]
struct ChaUnitReading {
    counters: [u64; CHA_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct ChaUnit {
    cpu: u32,
    id: usize,
}

impl ChaUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE))
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_FREEZE_AND_RESET))
    }

    fn program(self, group: ChaEventGroup, locality: ChaRequestLocality) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;

        msr.write(
            cha_filter0_offset(self.id),
            u64::from(group.filter0.value()),
        )?;
        msr.write(
            cha_filter1_offset(self.id),
            u64::from(programmed_filter1(group, locality)),
        )?;

        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(
                cha_control_offset(self.id, counter_index),
                u64::from(pmon::counter_control(event.event, event.umask, true)),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<ChaUnitReading, String> {
        Ok(ChaUnitReading {
            counters: [
                self.read_counter(0).map(mask_cha_counter)?,
                self.read_counter(1).map(mask_cha_counter)?,
                self.read_counter(2).map(mask_cha_counter)?,
                self.read_counter(3).map(mask_cha_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(u64::from(pmon::UNIT_UNFREEZE))
    }

    fn write(self, writes: &[(u64, u64)]) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        for &(address, value) in writes {
            msr.write(address, value)?;
        }

        Ok(())
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(cha_counter_offset(self.id, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(cha_unit_control_offset(self.id), value)
    }
}

#[derive(Debug)]
struct ChaPackage {
    scope: UncoreScope,
    units: Vec<ChaUnit>,
}

impl ChaPackage {
    fn new(scope: UncoreScope, units: Vec<ChaUnit>) -> Self {
        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChaMeasurementSlice {
    duration: Duration,
    groups: [Option<ChaEventGroup>; CHA_COUNTER_COUNT],
    partition_offset: usize,
    partition_width: usize,
}

#[derive(Clone, Copy, Debug)]
struct ChaMeasurement {
    enabled: Duration,
    represented_unit_count: u64,
    running: Duration,
    unit_scale: f64,
}

#[derive(Debug, Default)]
struct ChaMeasurementAccumulator {
    measurements: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
}

impl ChaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: UncoreScope,
        kind: ChaEventKind,
        value: u64,
        measurement: ChaMeasurement,
    ) {
        if kind == ChaEventKind::Unused {
            return;
        }

        let scaled_value = if kind.is_clockticks() {
            value
        } else {
            (value as f64 * measurement.unit_scale) as u64
        };

        self.measurements
            .entry(scope)
            .or_default()
            .entry(kind)
            .and_modify(|event_measurement| {
                event_measurement.add(
                    scaled_value,
                    measurement.running,
                    measurement.represented_unit_count,
                )
            })
            .or_insert(ChaEventMeasurement {
                enabled: measurement.enabled,
                represented_unit_count: measurement.represented_unit_count,
                running: measurement.running,
                value: scaled_value,
            });
    }

    fn into_measurements(
        self,
    ) -> BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>> {
        self.measurements
    }
}

#[derive(Debug)]
struct ChaTransactionScopeMetrics {
    results: Vec<ChaTransactionResultMetrics>,
    totals: Vec<ChaTransactionMetrics>,
}

/// How long one CHA event group counted during a sample.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChaEventGroupTiming {
    pub group: String,
    pub running_seconds: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SkxChaMetrics {
    pub event_groups: Vec<ChaEventGroupTiming>,
    pub evictions: Vec<ChaEvictionMetrics>,
    pub ha_requests: Vec<ChaHaRequestMetrics>,
    pub llc_lookups: Vec<ChaLlcLookupMetrics>,
    pub llc_victims: Vec<ChaLlcVictimMetrics>,
    pub no_credits: Vec<ChaNoCreditMetrics>,
    pub request_queues: Vec<ChaRequestQueueMetrics>,
    pub rxc: Vec<ChaRxcMetrics>,
    pub sample_window_seconds: f64,
    pub scopes: Vec<ChaScopeMetrics>,
    pub sf_evictions: Vec<ChaSfEvictionMetrics>,
    pub tor_inserts: Vec<ChaTorInsertMetrics>,
    pub transaction_results: Vec<ChaTransactionResultMetrics>,
    pub transactions: Vec<ChaTransactionMetrics>,
}

impl SkxChaMetrics {
    /// Derives the metrics of the event groups in `groups`. Metrics whose
    /// groups are not in the profile are omitted; a measurement missing from
    /// a group that is in the profile is still an error.
    fn from_measurements(
        groups: &[ChaEventGroup],
        measurements: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
        event_groups: Vec<ChaEventGroupTiming>,
        sample_window: Duration,
    ) -> Result<Self, String> {
        let measured = measured_event_kinds(groups);
        let lookup_states = LLC_LOOKUP_STATES
            .into_iter()
            .filter(|&state| {
                measured.contains(&ChaEventKind::LlcLookup(state, ChaLookupOperation::Any))
            })
            .collect::<Vec<_>>();
        let request_queue_sources = [ChaRequestSource::Ia, ChaRequestSource::Io]
            .into_iter()
            .filter(|&source| measured.contains(&ChaEventKind::RequestQueueOccupancy(source)))
            .collect::<Vec<_>>();
        let rxc_queues = [ChaRxcQueue::Irq, ChaRxcQueue::Prq]
            .into_iter()
            .filter(|&queue| measured.contains(&ChaEventKind::RxcOccupancy(queue)))
            .collect::<Vec<_>>();
        let tor_opcodes = CHA_TOR_OPCODES
            .into_iter()
            .filter(|&opcode| {
                measured.contains(&ChaEventKind::TorInsert(
                    opcode,
                    ChaRequestSource::Io,
                    ChaTorResult::All,
                ))
            })
            .collect::<Vec<_>>();

        let mut evictions = Vec::new();
        let mut ha_requests = Vec::new();
        let mut llc_lookups = Vec::new();
        let mut llc_victims = Vec::new();
        let mut no_credits = Vec::new();
        let mut request_queues = Vec::new();
        let mut rxc = Vec::new();
        let mut scopes = Vec::with_capacity(measurements.len());
        let mut sf_evictions = Vec::new();
        let mut tor_inserts = Vec::new();
        let mut transaction_results = Vec::new();
        let mut transactions = Vec::new();

        for (scope, scope_measurements) in measurements {
            if measured.contains(&ChaEventKind::EvictionClockticks) {
                let eviction_clockticks =
                    required_measurement(&scope_measurements, ChaEventKind::EvictionClockticks)?;
                scopes.push(ChaScopeMetrics {
                    frequency_hz: frequency_hz(
                        eviction_clockticks.value,
                        eviction_clockticks.running,
                    ),
                    scope,
                });
                evictions.push(eviction_metrics(scope, &scope_measurements)?);
            }
            if measured.contains(&ChaEventKind::HaRequest(
                ChaHaRequestLocality::Local,
                ChaRequestOperation::Read,
            )) {
                ha_requests.push(ha_request_metrics(scope, &scope_measurements)?);
            }
            llc_lookups.extend(llc_lookup_metrics_for_states(
                scope,
                &scope_measurements,
                &lookup_states,
            )?);
            if measured.contains(&ChaEventKind::LlcVictim(ChaCacheState::M)) {
                llc_victims.extend(llc_victim_metrics(
                    scope,
                    &scope_measurements,
                    &[
                        ChaCacheState::M,
                        ChaCacheState::E,
                        ChaCacheState::S,
                        ChaCacheState::F,
                    ],
                )?);
            }
            if measured.contains(&ChaEventKind::NoCreditsClockticks) {
                no_credits.extend(no_credit_metrics(scope, &scope_measurements)?);
            }
            request_queues.extend(request_queue_metrics(
                scope,
                &scope_measurements,
                &request_queue_sources,
            )?);
            rxc.extend(rxc_metrics(scope, &scope_measurements, &rxc_queues)?);
            if measured.contains(&ChaEventKind::SfEviction(ChaCacheState::M)) {
                sf_evictions.extend(sf_eviction_metrics(scope, &scope_measurements)?);
            }
            tor_inserts.extend(tor_insert_metrics(
                scope,
                &scope_measurements,
                &tor_opcodes,
            )?);

            let transaction_scope_metrics =
                transaction_metrics(scope, &scope_measurements, &measured)?;
            transaction_results.extend(transaction_scope_metrics.results);
            transactions.extend(transaction_scope_metrics.totals);
        }

        Ok(Self {
            event_groups,
            evictions,
            ha_requests,
            llc_lookups,
            llc_victims,
            no_credits,
            request_queues,
            rxc,
            sample_window_seconds: sample_window.as_secs_f64(),
            scopes,
            sf_evictions,
            tor_inserts,
            transaction_results,
            transactions,
        })
    }
}

fn measured_event_kinds(groups: &[ChaEventGroup]) -> BTreeSet<ChaEventKind> {
    groups
        .iter()
        .flat_map(|group| group.events)
        .map(|event| event.kind)
        .filter(|&kind| kind != ChaEventKind::Unused)
        .collect()
}

fn event_group_timings(
    groups: &[ChaEventGroup],
    group_running: &BTreeMap<ChaGroupId, Duration>,
) -> Vec<ChaEventGroupTiming> {
    groups
        .iter()
        .map(|group| ChaEventGroupTiming {
            group: group.id.label(),
            running_seconds: group_running
                .get(&group.id)
                .copied()
                .unwrap_or_default()
                .as_secs_f64(),
        })
        .collect()
}

#[derive(Debug)]
pub struct SkxChaCollector {
    /// The PMU config counted instead of `groups`, with its counts so far.
    config: Option<CountAccumulator>,
    groups: &'static [ChaEventGroup],
    multiplex_mode: ChaMultiplexMode,
    next_group: usize,
    next_partition_offset: usize,
    packages: Vec<ChaPackage>,
    request_locality: ChaRequestLocality,
}

impl SkxChaCollector {
    /// The CHA ids of each package that PMU configs can program.
    pub fn config_units(&self) -> BTreeMap<u16, BTreeSet<u16>> {
        self.packages
            .iter()
            .map(|package| {
                (
                    package.scope.package_id as u16,
                    package.units.iter().map(|unit| unit.id as u16).collect(),
                )
            })
            .collect()
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    /// Counts `config`'s groups from the next sample on, or the default
    /// groups for `None`. Counts restart from 0.
    ///
    /// The default samples need no reset: they program every CHA they read
    /// in every slice.
    pub fn set_config(&mut self, config: Option<PmuConfig>) -> Result<(), String> {
        let Some(config) = config else {
            self.config = None;
            return Ok(());
        };

        let program = &config.program;
        if program.box_type != BoxType::Cha {
            return Err(format!(
                "CHA collector cannot run {} configs",
                program.box_type.label()
            ));
        }
        let host_units = self.config_units();
        for group in &program.groups {
            for (package, units) in &group.units {
                let host = host_units.get(package).ok_or_else(|| {
                    format!(
                        "CHA config group {} runs in package {package}, which this host lacks",
                        group.index
                    )
                })?;
                if let Some(unit) = units.difference(host).next() {
                    return Err(format!(
                        "CHA config group {} runs on CHA {unit}, which package {package} lacks",
                        group.index
                    ));
                }
            }
            cha_config_filters(group)?;
            for counter in &group.counters {
                cha_config_counter_index(counter.slot)?;
            }
        }

        self.config = Some(CountAccumulator::new(config));
        Ok(())
    }

    /// Samples the set config for `interval`; `cycle` sets the group temporal
    /// groups start from. Its counts are cumulative since the config was set.
    pub async fn sample_config(
        &mut self,
        interval: Duration,
        cycle: u64,
    ) -> Result<UncoreCounts, String> {
        let accumulator = self
            .config
            .as_mut()
            .ok_or_else(|| "CHA collector has no PMU config to sample".to_string())?;
        if interval.is_zero() {
            return Err("CHA measure interval must be non-zero".to_string());
        }
        let program = accumulator.config().program.clone();

        for slice in cha_config_schedule(&program, interval, cycle) {
            let units = cha_config_units(&self.packages, &program, &slice)?;
            for unit in &units {
                unit.cha.write(&cha_config_unit_writes(
                    unit.cha.id,
                    &program.groups[unit.group],
                )?)?;
            }

            let started_at = Instant::now();
            for unit in &units {
                unit.cha.unfreeze()?;
            }
            tokio::time::sleep(slice.duration).await;
            for unit in &units {
                unit.cha.freeze()?;
            }
            let running = started_at.elapsed();

            let readings = units
                .iter()
                .map(|unit| unit.read(&program.groups[unit.group]))
                .collect::<Result<Vec<_>, String>>()?;
            add_cha_config_slice(accumulator, &program, &slice, &readings, running)?;
        }

        Ok(accumulator.counts())
    }
}

impl SkxChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let packages = discover_packages(architecture.intel_server_model())?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            config: None,
            groups: &SKX_CHA_EVENT_GROUPS,
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages,
            request_locality: ChaRequestLocality::default(),
        })
    }

    #[cfg(test)]
    fn is_supported(architecture: &Architecture) -> bool {
        matches!(
            architecture.intel_server_model(),
            IntelServerCpuModel::SkylakeXeon
        )
    }

    pub fn set_event_profile(&mut self, profile: EventProfile) {
        self.groups = skx_cha_event_groups(profile);
        self.next_group = 0;
    }

    pub fn set_request_locality(&mut self, locality: ChaRequestLocality) {
        self.request_locality = locality;
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        if let Err(error) = self.validate_multiplex_mode(mode) {
            eprintln!("ocellus: disabling CHA spatial multiplexing: {error}");
            self.multiplex_mode = ChaMultiplexMode::Temporal;
            return;
        }

        self.multiplex_mode = mode;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SkxChaMetrics, String> {
        if interval.is_zero() {
            return Err("CHA measure interval must be non-zero".to_string());
        }

        let mut measurements = ChaMeasurementAccumulator::new();
        let packages = &self.packages;

        let slices = self.schedule(interval);
        let measured_slice_count = slices.len();
        let mut group_running = BTreeMap::<ChaGroupId, Duration>::new();
        let window_started_at = Instant::now();

        for slice in slices {
            program_packages(packages, slice, self.request_locality)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;
            let running = started_at.elapsed();

            for group in slice.groups.into_iter().flatten() {
                *group_running.entry(group.id).or_default() += running;
            }
            read_packages(packages, interval, running, slice, &mut measurements)?;
        }

        let sample_window = window_started_at.elapsed();
        self.rotate_schedule(measured_slice_count);

        SkxChaMetrics::from_measurements(
            self.groups,
            measurements.into_measurements(),
            event_group_timings(self.groups, &group_running),
            sample_window,
        )
    }

    fn rotate_schedule(&mut self, measured_slice_count: usize) {
        self.next_group = (self.next_group + self.multiplex_mode.partitions()) % self.groups.len();
        self.next_partition_offset = self
            .next_partition_offset
            .wrapping_add(measured_slice_count);
    }

    #[cfg(test)]
    fn rotate_group(&mut self) {
        self.rotate_schedule(1);
    }

    fn schedule(&self, interval: Duration) -> Vec<ChaMeasurementSlice> {
        let group_count = self.groups.len();
        let partitions = self.multiplex_mode.partitions();
        let slice_count_per_round = group_count.div_ceil(partitions);
        let round_count = measurement_round_count(interval, slice_count_per_round);
        let slice_count = slice_count_per_round * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for round in 0..round_count {
            for slice_index in 0..slice_count_per_round {
                let first_group_offset = slice_index * partitions;
                let groups = self.slice_groups(first_group_offset, partitions, group_count);

                slices.push(ChaMeasurementSlice {
                    duration: slice_duration,
                    groups,
                    partition_offset: self.next_partition_offset
                        + (round * slice_count_per_round)
                        + slice_index,
                    partition_width: partitions,
                });
            }
        }

        slices
    }

    fn slice_groups(
        &self,
        first_group_offset: usize,
        partitions: usize,
        group_count: usize,
    ) -> [Option<ChaEventGroup>; CHA_COUNTER_COUNT] {
        let mut groups = [None; CHA_COUNTER_COUNT];

        for (partition, group) in groups.iter_mut().enumerate().take(partitions) {
            let group_offset = first_group_offset + partition;
            if group_offset < group_count {
                let group_index = (self.next_group + group_offset) % group_count;
                *group = Some(self.groups[group_index]);
            }
        }

        groups
    }

    fn validate_multiplex_mode(&self, mode: ChaMultiplexMode) -> Result<(), String> {
        let partitions = mode.partitions();

        if partitions == 0 || partitions > CHA_COUNTER_COUNT {
            return Err(format!(
                "CHA spatial partitions must be between 1 and {CHA_COUNTER_COUNT}"
            ));
        }

        for package in &self.packages {
            if partitions > package.units.len() {
                return Err(format!(
                    "CHA spatial partitions ({partitions}) exceed discovered CHA units ({}) for package {:?}",
                    package.units.len(),
                    package.scope
                ));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaHaRequestBandwidthLabels {
    die: String,
    die_group: String,
    locality: String,
    operation: String,
    package: String,
}

impl ChaHaRequestBandwidthLabels {
    fn new(
        scope: UncoreScope,
        locality: ChaHaRequestLocality,
        operation: ChaRequestOperation,
    ) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            locality: locality.label().to_string(),
            operation: operation.label().to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaHaRequestRatioLabels {
    die: String,
    die_group: String,
    operation: String,
    package: String,
}

impl ChaHaRequestRatioLabels {
    fn new(scope: UncoreScope, operation: ChaRequestOperation) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            operation: operation.label().to_string(),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaLlcLookupLabels {
    die: String,
    die_group: String,
    operation: String,
    package: String,
    state: String,
}

impl ChaLlcLookupLabels {
    fn from_metric(metric: ChaLlcLookupMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            operation: metric.operation.label().to_string(),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaNoCreditLabels {
    die: String,
    die_group: String,
    direction: String,
    package: String,
}

impl ChaNoCreditLabels {
    fn from_metric(metric: ChaNoCreditMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            direction: metric.direction.label().to_string(),
            package: metric.scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaRequestQueueLabels {
    die: String,
    die_group: String,
    package: String,
    source: String,
}

impl ChaRequestQueueLabels {
    fn from_metric(metric: ChaRequestQueueMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            source: metric.source.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaRxcLabels {
    die: String,
    die_group: String,
    package: String,
    queue: String,
}

impl ChaRxcLabels {
    fn from_metric(metric: ChaRxcMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            queue: metric.queue.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaScopeLabels {
    die: String,
    die_group: String,
    package: String,
}

impl ChaScopeLabels {
    fn from_scope(scope: UncoreScope) -> Self {
        Self {
            die: topology_label(scope.die_id),
            die_group: topology_label(scope.die_group_id),
            package: scope.package_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaStateLabels {
    die: String,
    die_group: String,
    package: String,
    state: String,
}

impl ChaStateLabels {
    fn from_llc_victim(metric: ChaLlcVictimMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
        }
    }

    fn from_sf_eviction(metric: ChaSfEvictionMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaEventGroupLabels {
    group: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaTorInsertLabels {
    die: String,
    die_group: String,
    opcode: String,
    package: String,
    result: String,
    source: String,
}

impl ChaTorInsertLabels {
    fn from_metric(metric: ChaTorInsertMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            opcode: metric.opcode.label().to_string(),
            package: metric.scope.package_id.to_string(),
            result: metric.result.label().to_string(),
            source: metric.source.label().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaTransactionLabels {
    die: String,
    die_group: String,
    package: String,
    transaction: String,
}

impl ChaTransactionLabels {
    fn from_metric(metric: ChaTransactionMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            transaction: metric.transaction.as_str().to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ChaTransactionResultLabels {
    die: String,
    die_group: String,
    package: String,
    result: String,
    transaction: String,
}

impl ChaTransactionResultLabels {
    fn from_metric(metric: ChaTransactionResultMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            result: metric.result.label().to_string(),
            transaction: metric.transaction.as_str().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct SkxChaPrometheusMetrics {
    eviction_bandwidth_bytes_per_second: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    eviction_latency_seconds: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    eviction_occupancy_entries: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    frequency_hz: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    group_running_seconds: Family<ChaEventGroupLabels, Gauge<f64, AtomicU64>>,
    ha_request_bandwidth_bytes_per_second:
        Family<ChaHaRequestBandwidthLabels, Gauge<f64, AtomicU64>>,
    ha_request_local_ratio: Family<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>,
    llc_lookup_bytes_per_second: Family<ChaLlcLookupLabels, Gauge<f64, AtomicU64>>,
    llc_victims_per_second: Family<ChaStateLabels, Gauge<f64, AtomicU64>>,
    no_credit_ratio: Family<ChaNoCreditLabels, Gauge<f64, AtomicU64>>,
    request_queue_occupancy_entries: Family<ChaRequestQueueLabels, Gauge<f64, AtomicU64>>,
    rxc_inserts_per_second: Family<ChaRxcLabels, Gauge<f64, AtomicU64>>,
    rxc_latency_seconds: Family<ChaRxcLabels, Gauge<f64, AtomicU64>>,
    rxc_occupancy_entries: Family<ChaRxcLabels, Gauge<f64, AtomicU64>>,
    sample_window_seconds: Gauge<f64, AtomicU64>,
    sf_eviction_bytes_per_second: Family<ChaStateLabels, Gauge<f64, AtomicU64>>,
    tor_inserts_per_second: Family<ChaTorInsertLabels, Gauge<f64, AtomicU64>>,
    transaction_bandwidth_bytes_per_second: Family<ChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_hit_rate: Family<ChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_latency_seconds: Family<ChaTransactionLabels, Gauge<f64, AtomicU64>>,
    transaction_result_bandwidth_bytes_per_second:
        Family<ChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_inserts_per_second:
        Family<ChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_latency_seconds: Family<ChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
    transaction_result_occupancy_entries: Family<ChaTransactionResultLabels, Gauge<f64, AtomicU64>>,
}

impl SkxChaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            eviction_bandwidth_bytes_per_second:
                Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            eviction_latency_seconds: Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            eviction_occupancy_entries: Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            frequency_hz: Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            group_running_seconds: Family::<ChaEventGroupLabels, Gauge<f64, AtomicU64>>::default(),
            ha_request_bandwidth_bytes_per_second: Family::<
                ChaHaRequestBandwidthLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            ha_request_local_ratio:
                Family::<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>::default(),
            llc_lookup_bytes_per_second:
                Family::<ChaLlcLookupLabels, Gauge<f64, AtomicU64>>::default(),
            llc_victims_per_second: Family::<ChaStateLabels, Gauge<f64, AtomicU64>>::default(),
            no_credit_ratio: Family::<ChaNoCreditLabels, Gauge<f64, AtomicU64>>::default(),
            request_queue_occupancy_entries:
                Family::<ChaRequestQueueLabels, Gauge<f64, AtomicU64>>::default(),
            rxc_inserts_per_second: Family::<ChaRxcLabels, Gauge<f64, AtomicU64>>::default(),
            rxc_latency_seconds: Family::<ChaRxcLabels, Gauge<f64, AtomicU64>>::default(),
            rxc_occupancy_entries: Family::<ChaRxcLabels, Gauge<f64, AtomicU64>>::default(),
            sample_window_seconds: Gauge::<f64, AtomicU64>::default(),
            sf_eviction_bytes_per_second: Family::<ChaStateLabels, Gauge<f64, AtomicU64>>::default(
            ),
            tor_inserts_per_second: Family::<ChaTorInsertLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_bandwidth_bytes_per_second: Family::<
                ChaTransactionLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_hit_rate: Family::<ChaTransactionLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_latency_seconds:
                Family::<ChaTransactionLabels, Gauge<f64, AtomicU64>>::default(),
            transaction_result_bandwidth_bytes_per_second: Family::<
                ChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_inserts_per_second: Family::<
                ChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_latency_seconds: Family::<
                ChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            transaction_result_occupancy_entries: Family::<
                ChaTransactionResultLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
        };

        registry.register(
            "ocellus_cha_eviction_bandwidth_bytes_per_second",
            "Interval-derived CHA eviction bandwidth in bytes per second",
            metrics.eviction_bandwidth_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_eviction_latency_seconds",
            "Interval-derived CHA eviction residency latency in seconds",
            metrics.eviction_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_eviction_occupancy_entries",
            "Average CHA eviction occupancy in entries",
            metrics.eviction_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_cha_frequency_hz",
            "Interval-derived CHA clock frequency in hertz",
            metrics.frequency_hz.clone(),
        );
        registry.register(
            "ocellus_cha_group_running_seconds",
            "Time each CHA event group counted during the last sample in seconds; divide by ocellus_cha_sample_window_seconds for its duty cycle",
            metrics.group_running_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_ha_request_bandwidth_bytes_per_second",
            "Interval-derived CHA HA request bandwidth in bytes per second",
            metrics.ha_request_bandwidth_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_ha_request_local_ratio",
            "Interval-derived CHA HA request local ratio",
            metrics.ha_request_local_ratio.clone(),
        );
        registry.register(
            "ocellus_cha_llc_lookup_bytes_per_second",
            "Interval-derived CHA LLC lookup bandwidth in bytes per second",
            metrics.llc_lookup_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_llc_victims_per_second",
            "Interval-derived CHA LLC victims per second",
            metrics.llc_victims_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_no_credit_ratio",
            "Average CHA no-credit cycles ratio",
            metrics.no_credit_ratio.clone(),
        );
        registry.register(
            "ocellus_cha_request_queue_occupancy_entries",
            "Average CHA request queue occupancy in entries",
            metrics.request_queue_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_cha_rxc_inserts_per_second",
            "Interval-derived CHA RxC queue inserts per second",
            metrics.rxc_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_rxc_latency_seconds",
            "Interval-derived CHA RxC queue residency latency in seconds",
            metrics.rxc_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_rxc_occupancy_entries",
            "Average CHA RxC queue occupancy in entries",
            metrics.rxc_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_cha_sample_window_seconds",
            "Wall time the last CHA sample took in seconds, including programming",
            metrics.sample_window_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_sf_eviction_bytes_per_second",
            "Interval-derived CHA snoop filter eviction bandwidth in bytes per second",
            metrics.sf_eviction_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_tor_inserts_per_second",
            "Interval-derived CHA TOR inserts per second by opcode; the four counts of one opcode share a time slice",
            metrics.tor_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_bandwidth_bytes_per_second",
            "Interval-derived CHA transaction bandwidth in bytes per second",
            metrics.transaction_bandwidth_bytes_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_hit_rate",
            "Interval-derived CHA transaction hit rate",
            metrics.transaction_hit_rate.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_latency_seconds",
            "Interval-derived CHA transaction latency in seconds",
            metrics.transaction_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_bandwidth_bytes_per_second",
            "Interval-derived CHA transaction result bandwidth in bytes per second",
            metrics
                .transaction_result_bandwidth_bytes_per_second
                .clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_inserts_per_second",
            "Interval-derived CHA transaction result inserts per second",
            metrics.transaction_result_inserts_per_second.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_latency_seconds",
            "Interval-derived CHA transaction result residency latency in seconds",
            metrics.transaction_result_latency_seconds.clone(),
        );
        registry.register(
            "ocellus_cha_transaction_result_occupancy_entries",
            "Average CHA transaction result occupancy in entries",
            metrics.transaction_result_occupancy_entries.clone(),
        );

        metrics
    }

    pub fn update(&self, metrics: SkxChaMetrics) {
        for timing in metrics.event_groups {
            self.group_running_seconds
                .get_or_create(&ChaEventGroupLabels {
                    group: timing.group,
                })
                .set(timing.running_seconds);
        }
        self.sample_window_seconds
            .set(metrics.sample_window_seconds);

        for metric in metrics.tor_inserts {
            self.tor_inserts_per_second
                .get_or_create(&ChaTorInsertLabels::from_metric(metric))
                .set(metric.inserts_per_second);
        }

        for scope in metrics.scopes {
            self.frequency_hz
                .get_or_create(&ChaScopeLabels::from_scope(scope.scope))
                .set(scope.frequency_hz);
        }

        for metric in metrics.evictions {
            let labels = ChaScopeLabels::from_scope(metric.scope);

            self.eviction_bandwidth_bytes_per_second
                .get_or_create(&labels)
                .set(metric.bandwidth_bytes_per_second);
            self.eviction_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
            self.eviction_occupancy_entries
                .get_or_create(&labels)
                .set(metric.occupancy_entries);
        }

        for metric in metrics.ha_requests {
            self.ha_request_bandwidth_bytes_per_second
                .get_or_create(&ChaHaRequestBandwidthLabels::new(
                    metric.scope,
                    ChaHaRequestLocality::Local,
                    ChaRequestOperation::Read,
                ))
                .set(metric.local_read_bytes_per_second);
            self.ha_request_bandwidth_bytes_per_second
                .get_or_create(&ChaHaRequestBandwidthLabels::new(
                    metric.scope,
                    ChaHaRequestLocality::Local,
                    ChaRequestOperation::Write,
                ))
                .set(metric.local_write_bytes_per_second);
            self.ha_request_bandwidth_bytes_per_second
                .get_or_create(&ChaHaRequestBandwidthLabels::new(
                    metric.scope,
                    ChaHaRequestLocality::Remote,
                    ChaRequestOperation::Read,
                ))
                .set(metric.remote_read_bytes_per_second);
            self.ha_request_bandwidth_bytes_per_second
                .get_or_create(&ChaHaRequestBandwidthLabels::new(
                    metric.scope,
                    ChaHaRequestLocality::Remote,
                    ChaRequestOperation::Write,
                ))
                .set(metric.remote_write_bytes_per_second);
            self.ha_request_local_ratio
                .get_or_create(&ChaHaRequestRatioLabels::new(
                    metric.scope,
                    ChaRequestOperation::Read,
                ))
                .set(metric.local_read_ratio);
            self.ha_request_local_ratio
                .get_or_create(&ChaHaRequestRatioLabels::new(
                    metric.scope,
                    ChaRequestOperation::Write,
                ))
                .set(metric.local_write_ratio);
        }

        for metric in metrics.llc_lookups {
            self.llc_lookup_bytes_per_second
                .get_or_create(&ChaLlcLookupLabels::from_metric(metric))
                .set(metric.bytes_per_second);
        }

        for metric in metrics.llc_victims {
            self.llc_victims_per_second
                .get_or_create(&ChaStateLabels::from_llc_victim(metric))
                .set(metric.per_second);
        }

        for metric in metrics.no_credits {
            self.no_credit_ratio
                .get_or_create(&ChaNoCreditLabels::from_metric(metric))
                .set(metric.ratio);
        }

        for metric in metrics.request_queues {
            self.request_queue_occupancy_entries
                .get_or_create(&ChaRequestQueueLabels::from_metric(metric))
                .set(metric.occupancy_entries);
        }

        for metric in metrics.rxc {
            let labels = ChaRxcLabels::from_metric(metric);

            self.rxc_inserts_per_second
                .get_or_create(&labels)
                .set(metric.inserts_per_second);
            self.rxc_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
            self.rxc_occupancy_entries
                .get_or_create(&labels)
                .set(metric.occupancy_entries);
        }

        for metric in metrics.sf_evictions {
            self.sf_eviction_bytes_per_second
                .get_or_create(&ChaStateLabels::from_sf_eviction(metric))
                .set(metric.bytes_per_second);
        }

        for metric in metrics.transaction_results {
            let labels = ChaTransactionResultLabels::from_metric(metric);

            self.transaction_result_bandwidth_bytes_per_second
                .get_or_create(&labels)
                .set(metric.bandwidth_bytes_per_second);
            self.transaction_result_inserts_per_second
                .get_or_create(&labels)
                .set(metric.inserts_per_second);
            self.transaction_result_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
            self.transaction_result_occupancy_entries
                .get_or_create(&labels)
                .set(metric.occupancy_entries);
        }

        for metric in metrics.transactions {
            let labels = ChaTransactionLabels::from_metric(metric);

            self.transaction_bandwidth_bytes_per_second
                .get_or_create(&labels)
                .set(metric.bandwidth_bytes_per_second);
            self.transaction_hit_rate
                .get_or_create(&labels)
                .set(metric.hit_rate);
            self.transaction_latency_seconds
                .get_or_create(&labels)
                .set(metric.latency_seconds);
        }
    }
}

fn discover_packages(model: IntelServerCpuModel) -> Result<Vec<ChaPackage>, String> {
    if !matches!(model, IntelServerCpuModel::SkylakeXeon) {
        return Err(format!("CHA collection is not supported for {model:?}"));
    }

    let mut packages = Vec::new();

    for leader in uncore_leaders()? {
        packages.push(ChaPackage::new(leader.scope, discover_units(leader.cpu)?));
    }

    if packages.is_empty() {
        return Err("failed to discover any CHA packages".to_string());
    }

    Ok(packages)
}

fn discover_units(cpu: u32) -> Result<Vec<ChaUnit>, String> {
    let msr = Msr::open_readonly(cpu)?;
    let mut units = Vec::new();

    for id in skx_cha_unit_ids(cpu)? {
        if msr.read(cha_unit_control_offset(id)).is_ok()
            && msr.read(cha_counter_offset(id, 0)).is_ok()
            && msr.read(cha_control_offset(id, 0)).is_ok()
            && msr.read(cha_filter0_offset(id)).is_ok()
            && msr.read(cha_filter1_offset(id)).is_ok()
        {
            units.push(ChaUnit { cpu, id });
        }
    }

    if units.is_empty() {
        return Err(format!("failed to discover any CHA units on CPU {cpu}"));
    }

    Ok(units)
}

fn skx_cha_unit_ids(cpu: u32) -> Result<Vec<usize>, String> {
    match skx_cha_unit_ids_from_linux_uncore_pmu() {
        Ok(ids) => Ok(ids),
        Err(error) => {
            eprintln!("ocellus: falling back to SKX CAPID6 for CHA discovery: {error}");
            match skx_cha_unit_ids_from_capid_pci(cpu) {
                Ok(ids) => Ok(ids),
                Err(error) => {
                    eprintln!(
                        "ocellus: falling back to MSR probing for SKX CHA discovery: {error}"
                    );
                    Ok(skx_cha_unit_ids_for_msr_probe())
                }
            }
        }
    }
}

fn skx_cha_unit_ids_from_linux_uncore_pmu() -> Result<Vec<usize>, String> {
    linux_uncore_unit_ids(&["uncore_cha_"], SKX_MAX_CHA_COUNT)
}

fn skx_cha_unit_ids_from_capid_pci(cpu: u32) -> Result<Vec<usize>, String> {
    let locations = pci::find_intel_devices_matching_device_id(SKX_CAPID_DEVICE_ID)?;
    let location = pci_location_for_cpu(cpu, &locations, "SKX CAPID")?;
    let device = pci::PciDevice::open_readonly(location)?;
    let capid6 = device.read_u32(SKX_CAPID6_OFFSET)?;

    skx_cha_unit_ids_from_capid6(capid6)
}

fn skx_cha_unit_ids_from_capid6(capid6: u32) -> Result<Vec<usize>, String> {
    let count = (capid6 & SKX_CHA_CAPID6_MASK).count_ones() as usize;
    if count == 0 {
        return Err("SKX CAPID6 reports zero available CHAs".to_string());
    }

    Ok((0..count).collect())
}

fn skx_cha_unit_ids_for_msr_probe() -> Vec<usize> {
    (0..SKX_MAX_CHA_COUNT).collect()
}

fn eviction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<ChaEvictionMetrics, String> {
    let clockticks = required_measurement(measurements, ChaEventKind::EvictionClockticks)?;
    let inserts = required_measurement(measurements, ChaEventKind::EvictionInsert)?;
    let occupancy = required_measurement(measurements, ChaEventKind::EvictionOccupancy)?;
    let clocktick_count = scale_measurement_value(clockticks);
    let insert_count = scale_measurement_value(inserts);
    let occupancy_count = scale_measurement_value(occupancy);

    Ok(ChaEvictionMetrics {
        bandwidth_bytes_per_second: bytes_per_second(inserts),
        latency_seconds: queue_residency_seconds(
            occupancy_count,
            insert_count,
            clocktick_count,
            inserts.enabled,
        ),
        occupancy_entries: ratio(occupancy_count, clocktick_count),
        scope,
    })
}

fn ha_request_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<ChaHaRequestMetrics, String> {
    let local_read = required_measurement(
        measurements,
        ChaEventKind::HaRequest(ChaHaRequestLocality::Local, ChaRequestOperation::Read),
    )?;
    let local_write = required_measurement(
        measurements,
        ChaEventKind::HaRequest(ChaHaRequestLocality::Local, ChaRequestOperation::Write),
    )?;
    let remote_read = required_measurement(
        measurements,
        ChaEventKind::HaRequest(ChaHaRequestLocality::Remote, ChaRequestOperation::Read),
    )?;
    let remote_write = required_measurement(
        measurements,
        ChaEventKind::HaRequest(ChaHaRequestLocality::Remote, ChaRequestOperation::Write),
    )?;
    let local_read_count = scale_measurement_value(local_read);
    let local_write_count = scale_measurement_value(local_write);
    let remote_read_count = scale_measurement_value(remote_read);
    let remote_write_count = scale_measurement_value(remote_write);

    Ok(ChaHaRequestMetrics {
        local_read_bytes_per_second: bytes_per_second(local_read),
        local_read_ratio: ratio(local_read_count, local_read_count + remote_read_count),
        local_write_bytes_per_second: bytes_per_second(local_write),
        local_write_ratio: ratio(local_write_count, local_write_count + remote_write_count),
        remote_read_bytes_per_second: bytes_per_second(remote_read),
        remote_write_bytes_per_second: bytes_per_second(remote_write),
        scope,
    })
}

fn no_credit_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaNoCreditMetrics>, String> {
    let clockticks = required_measurement(measurements, ChaEventKind::NoCreditsClockticks)?;
    let clocktick_count = scale_measurement_value(clockticks);
    let mut metrics = Vec::new();

    for direction in [ChaNoCreditDirection::Read, ChaNoCreditDirection::Write] {
        let no_credit_cycles = scale_measurement_value(required_measurement(
            measurements,
            ChaEventKind::NoCredits(direction),
        )?);

        metrics.push(ChaNoCreditMetrics {
            direction,
            ratio: ratio(
                no_credit_cycles,
                clocktick_count * clockticks.represented_unit_count,
            ),
            scope,
        });
    }

    Ok(metrics)
}

fn request_queue_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    sources: &[ChaRequestSource],
) -> Result<Vec<ChaRequestQueueMetrics>, String> {
    let mut metrics = Vec::new();

    for &source in sources {
        metrics.push(ChaRequestQueueMetrics {
            occupancy_entries: ratio(
                scale_measurement_value(required_measurement(
                    measurements,
                    ChaEventKind::RequestQueueOccupancy(source),
                )?),
                scale_measurement_value(required_measurement(
                    measurements,
                    ChaEventKind::RequestQueueClockticks(source),
                )?),
            ),
            scope,
            source,
        });
    }

    Ok(metrics)
}

fn rxc_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    queues: &[ChaRxcQueue],
) -> Result<Vec<ChaRxcMetrics>, String> {
    let mut metrics = Vec::new();

    for &queue in queues {
        let clockticks = required_measurement(measurements, ChaEventKind::RxcClockticks(queue))?;
        let inserts = required_measurement(measurements, ChaEventKind::RxcInsert(queue))?;
        let occupancy = required_measurement(measurements, ChaEventKind::RxcOccupancy(queue))?;
        let clocktick_count = scale_measurement_value(clockticks);
        let insert_count = scale_measurement_value(inserts);
        let occupancy_count = scale_measurement_value(occupancy);

        metrics.push(ChaRxcMetrics {
            inserts_per_second: event_rate(inserts),
            latency_seconds: queue_residency_seconds(
                occupancy_count,
                insert_count,
                clocktick_count,
                clockticks.enabled,
            ),
            occupancy_entries: ratio(occupancy_count, clocktick_count),
            queue,
            scope,
        });
    }

    Ok(metrics)
}

fn sf_eviction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaSfEvictionMetrics>, String> {
    let mut metrics = Vec::new();

    for state in [ChaCacheState::M, ChaCacheState::E, ChaCacheState::S] {
        metrics.push(ChaSfEvictionMetrics {
            bytes_per_second: bytes_per_second(required_measurement(
                measurements,
                ChaEventKind::SfEviction(state),
            )?),
            scope,
            state,
        });
    }

    Ok(metrics)
}

fn tor_insert_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    opcodes: &[ChaTorOpcode],
) -> Result<Vec<ChaTorInsertMetrics>, String> {
    let mut metrics = Vec::new();

    for &opcode in opcodes {
        for source in [ChaRequestSource::Io, ChaRequestSource::Ia] {
            for result in [ChaTorResult::All, ChaTorResult::Miss] {
                metrics.push(ChaTorInsertMetrics {
                    inserts_per_second: event_rate(required_measurement(
                        measurements,
                        ChaEventKind::TorInsert(opcode, source, result),
                    )?),
                    opcode,
                    result,
                    scope,
                    source,
                });
            }
        }
    }

    Ok(metrics)
}

/// Transaction results for every (transaction, result) group in `measured`,
/// and the hit/miss totals for transactions that measure both.
fn transaction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    measured: &BTreeSet<ChaEventKind>,
) -> Result<ChaTransactionScopeMetrics, String> {
    let mut results = Vec::new();
    let mut totals = Vec::new();

    for transaction in CHA_TRANSACTIONS {
        let measures = |result| {
            measured.contains(&ChaEventKind::TransactionInsert(
                transaction.label(),
                result,
            ))
        };
        match (
            measures(ChaTransactionResult::Hit),
            measures(ChaTransactionResult::Miss),
        ) {
            (true, true) => {}
            (false, false) => continue,
            (hit, _) => {
                let result = if hit {
                    ChaTransactionResult::Hit
                } else {
                    ChaTransactionResult::Miss
                };
                results.push(transaction_result_metrics(
                    scope,
                    measurements,
                    transaction,
                    result,
                )?);
                continue;
            }
        }

        let hit = transaction_result_metrics(
            scope,
            measurements,
            transaction,
            ChaTransactionResult::Hit,
        )?;
        let miss = transaction_result_metrics(
            scope,
            measurements,
            transaction,
            ChaTransactionResult::Miss,
        )?;
        let hit_inserts = required_measurement(
            measurements,
            ChaEventKind::TransactionInsert(transaction.label(), ChaTransactionResult::Hit),
        )?;
        let miss_inserts = required_measurement(
            measurements,
            ChaEventKind::TransactionInsert(transaction.label(), ChaTransactionResult::Miss),
        )?;
        let hit_insert_count = scale_measurement_value(hit_inserts) as f64;
        let miss_insert_count = scale_measurement_value(miss_inserts) as f64;

        let total_insert_count = hit_insert_count + miss_insert_count;

        totals.push(ChaTransactionMetrics {
            bandwidth_bytes_per_second: hit.bandwidth_bytes_per_second
                + miss.bandwidth_bytes_per_second,
            hit_rate: ratio(hit_insert_count as u64, total_insert_count as u64),
            latency_seconds: if total_insert_count == 0.0 {
                0.0
            } else {
                ((hit.latency_seconds * hit_insert_count)
                    + (miss.latency_seconds * miss_insert_count))
                    / total_insert_count
            },
            scope,
            transaction: transaction.label(),
        });
        results.push(hit);
        results.push(miss);
    }

    Ok(ChaTransactionScopeMetrics { results, totals })
}

fn transaction_result_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    transaction: ChaTransactionKind,
    result: ChaTransactionResult,
) -> Result<ChaTransactionResultMetrics, String> {
    let clockticks = required_measurement(
        measurements,
        ChaEventKind::TransactionClockticks(transaction.label(), result),
    )?;
    let inserts = required_measurement(
        measurements,
        ChaEventKind::TransactionInsert(transaction.label(), result),
    )?;
    let occupancy = required_measurement(
        measurements,
        ChaEventKind::TransactionOccupancy(transaction.label(), result),
    )?;
    let clocktick_count = scale_measurement_value(clockticks);
    let insert_count = scale_measurement_value(inserts);
    let occupancy_count = scale_measurement_value(occupancy);

    Ok(ChaTransactionResultMetrics {
        bandwidth_bytes_per_second: bytes_per_second(inserts),
        inserts_per_second: event_rate(inserts),
        latency_seconds: queue_residency_seconds(
            occupancy_count,
            insert_count,
            clocktick_count,
            clockticks.enabled,
        ),
        occupancy_entries: ratio(occupancy_count, clocktick_count),
        result,
        scope,
        transaction: transaction.label(),
    })
}

fn cha_control_offset(cha_id: usize, counter_index: usize) -> u64 {
    cha_unit_offset(CHA_CONTROL_BASE, cha_id) + counter_index as u64
}

fn cha_counter_offset(cha_id: usize, counter_index: usize) -> u64 {
    cha_unit_offset(CHA_COUNTER_BASE, cha_id) + counter_index as u64
}

fn cha_filter0_offset(cha_id: usize) -> u64 {
    cha_unit_offset(CHA_FILTER0_BASE, cha_id)
}

fn cha_filter1_offset(cha_id: usize) -> u64 {
    cha_unit_offset(CHA_FILTER1_BASE, cha_id)
}

fn cha_unit_control_offset(cha_id: usize) -> u64 {
    cha_unit_offset(CHA_UNIT_CONTROL_BASE, cha_id)
}

fn cha_unit_offset(base: u64, cha_id: usize) -> u64 {
    base + CHA_UNIT_STRIDE * cha_id as u64
}

fn mask_cha_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

fn probe_writable_msrs(packages: &[ChaPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
}

fn freeze_packages(packages: &[ChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

/// The filter1 value programmed for `group`. Locality applies to every group,
/// so one setting covers the TOR and LLC lookup events alike.
fn programmed_filter1(group: ChaEventGroup, locality: ChaRequestLocality) -> u32 {
    group.filter1.with_locality(locality).value()
}

fn program_packages(
    packages: &[ChaPackage],
    slice: ChaMeasurementSlice,
    locality: ChaRequestLocality,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze_and_reset()?;
        }
    }

    for package in packages {
        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = cha_partition(unit_index, slice, package.units.len());
            if let Some(group) = slice.groups[partition] {
                unit.program(group, locality)?;
            }
        }
    }

    Ok(())
}

fn read_packages(
    packages: &[ChaPackage],
    enabled: Duration,
    running: Duration,
    slice: ChaMeasurementSlice,
    measurements: &mut ChaMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let mut partition_counters = [[0_u64; CHA_COUNTER_COUNT]; CHA_COUNTER_COUNT];
        let mut partition_unit_counts = [0_u64; CHA_COUNTER_COUNT];

        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = cha_partition(unit_index, slice, package.units.len());
            if slice.groups[partition].is_none() {
                continue;
            }

            let reading = unit.read()?;
            partition_unit_counts[partition] += 1;

            for (counter, value) in partition_counters[partition]
                .iter_mut()
                .zip(reading.counters)
            {
                *counter += value;
            }
        }

        for (partition, group) in slice.groups.into_iter().enumerate() {
            let Some(group) = group else {
                continue;
            };

            let unit_count = partition_unit_counts[partition];
            if unit_count == 0 {
                continue;
            }

            let unit_scale = package.units.len() as f64 / unit_count as f64;

            for (counter_index, counter) in partition_counters[partition].into_iter().enumerate() {
                let event = group.events[counter_index];
                let value = if event.kind.is_clockticks() {
                    counter / unit_count
                } else {
                    counter
                };

                measurements.add(
                    package.scope,
                    event.kind,
                    value,
                    ChaMeasurement {
                        enabled,
                        represented_unit_count: package.units.len() as u64,
                        running,
                        unit_scale,
                    },
                );
            }
        }
    }

    Ok(())
}

fn cha_partition(unit_index: usize, slice: ChaMeasurementSlice, unit_count: usize) -> usize {
    let rotated_unit_index = (unit_index + slice.partition_offset) % unit_count;
    rotated_unit_index * slice.partition_width / unit_count
}

fn unfreeze_packages(packages: &[ChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

/// Config groups that count at the same time.
#[derive(Clone, Debug, PartialEq)]
struct ChaConfigSlice {
    duration: Duration,
    /// Positions in the config's groups.
    groups: Vec<usize>,
}

/// A CHA a config slice programs, with the position of its group.
#[derive(Clone, Copy, Debug)]
struct ChaConfigUnit {
    package: u16,
    cha: ChaUnit,
    group: usize,
}

impl ChaConfigUnit {
    fn read(self, group: &ProgrammedGroup) -> Result<ChaConfigReading, String> {
        let msr = Msr::open_readonly(self.cha.cpu)?;
        let counters = group
            .counters
            .iter()
            .map(|counter| {
                let index = cha_config_counter_index(counter.slot)?;
                Ok((
                    counter.slot,
                    msr.read(cha_counter_offset(self.cha.id, index))?,
                ))
            })
            .collect::<Result<_, String>>()?;

        Ok(ChaConfigReading {
            package: self.package,
            unit: self.cha.id as u16,
            group: group.index,
            counters,
        })
    }
}

/// The raw values of a CHA's programmed counters after a config slice.
#[derive(Clone, Debug, PartialEq)]
struct ChaConfigReading {
    package: u16,
    unit: u16,
    group: u8,
    counters: Vec<(CounterSlot, u64)>,
}

fn cha_config_counter_index(slot: CounterSlot) -> Result<usize, String> {
    match slot {
        CounterSlot::General(index) if usize::from(index) < CHA_COUNTER_COUNT => {
            Ok(usize::from(index))
        }
        _ => Err(format!(
            "Skylake-SP CHA boxes have no counter slot {}",
            slot.label()
        )),
    }
}

fn cha_config_filters(group: &ProgrammedGroup) -> Result<ChaFilters, String> {
    group
        .filters
        .ok_or_else(|| format!("CHA config group {} has no filters", group.index))
}

/// Fixed and spatial groups count for the whole interval. Temporal groups
/// take turns, one slice each per round, in index order from group `cycle`
/// mod their count, wrapping around.
fn cha_config_schedule(
    program: &ProgrammedBoxType,
    interval: Duration,
    cycle: u64,
) -> Vec<ChaConfigSlice> {
    let group_count = program.groups.len();

    match program.grouping {
        Grouping::Fixed | Grouping::Spatial => vec![ChaConfigSlice {
            duration: interval,
            groups: (0..group_count).collect(),
        }],
        Grouping::Temporal => {
            let round_count = measurement_round_count(interval, group_count);
            let duration = interval.div_f64((round_count * group_count) as f64);
            let start = (cycle % group_count as u64) as usize;

            (0..round_count)
                .flat_map(|_| 0..group_count)
                .map(|position| ChaConfigSlice {
                    duration,
                    groups: vec![(start + position) % group_count],
                })
                .collect()
        }
    }
}

/// The CHAs the groups of `slice` run on. Other CHAs stay untouched.
fn cha_config_units(
    packages: &[ChaPackage],
    program: &ProgrammedBoxType,
    slice: &ChaConfigSlice,
) -> Result<Vec<ChaConfigUnit>, String> {
    let mut units = Vec::new();

    for &group in &slice.groups {
        for (&package_id, unit_ids) in &program.groups[group].units {
            let package = packages
                .iter()
                .find(|package| package.scope.package_id as u16 == package_id)
                .ok_or_else(|| format!("CHA package {package_id} does not exist"))?;
            for &unit_id in unit_ids {
                let cha = package
                    .units
                    .iter()
                    .find(|unit| unit.id == usize::from(unit_id))
                    .copied()
                    .ok_or_else(|| {
                        format!("CHA {unit_id} does not exist in package {package_id}")
                    })?;
                units.push(ChaConfigUnit {
                    package: package_id,
                    cha,
                    group,
                });
            }
        }
    }

    Ok(units)
}

/// The MSR writes, as (address, value), that program `group` on CHA `cha_id`:
/// freeze and reset the unit, then set its filters and its counter controls
/// with the reset bit. Unused counters get 0.
fn cha_config_unit_writes(
    cha_id: usize,
    group: &ProgrammedGroup,
) -> Result<Vec<(u64, u64)>, String> {
    let filters = cha_config_filters(group)?;
    let mut controls = [0_u64; CHA_COUNTER_COUNT];
    for counter in &group.counters {
        controls[cha_config_counter_index(counter.slot)?] =
            counter.control | u64::from(pmon::COUNTER_RESET);
    }

    let mut writes = vec![
        (
            cha_unit_control_offset(cha_id),
            u64::from(pmon::UNIT_FREEZE_AND_RESET),
        ),
        (cha_filter0_offset(cha_id), filters.filter0),
        (cha_filter1_offset(cha_id), filters.filter1),
    ];
    writes.extend(
        controls
            .into_iter()
            .enumerate()
            .map(|(index, control)| (cha_control_offset(cha_id, index), control)),
    );

    Ok(writes)
}

/// Adds the readings of one slice, masked to the counter width, and gives
/// each group of the slice `running` in every package it runs in.
fn add_cha_config_slice(
    accumulator: &mut CountAccumulator,
    program: &ProgrammedBoxType,
    slice: &ChaConfigSlice,
    readings: &[ChaConfigReading],
    running: Duration,
) -> Result<(), String> {
    for &group in &slice.groups {
        let group = &program.groups[group];
        for &package in group.units.keys() {
            accumulator.add_running(package, group.index, running)?;
        }
    }
    for reading in readings {
        for &(slot, value) in &reading.counters {
            accumulator.add_count(
                reading.package,
                reading.unit,
                reading.group,
                slot,
                mask_cha_counter(value),
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmu_config::types::{Aggregate, CountUnit, EventLabels, ProgrammedCounter};

    #[test]
    fn exports_skx_llc_lookup_state_08_as_sf_h() {
        let labels: Vec<_> = SKX_CHA_EVENT_GROUPS
            .iter()
            .filter(|group| group.filter0.state == 0x08)
            .flat_map(|group| group.events)
            .filter_map(|event| match event.kind {
                ChaEventKind::LlcLookup(state, _) => Some(state.label()),
                _ => None,
            })
            .collect();

        assert_eq!(labels, ["sf_h"; 4]);
        assert!(
            LLC_LOOKUP_STATES
                .iter()
                .any(|state| state.filter0_bits() == 0x08 && state.label() == "sf_h")
        );
    }

    #[test]
    fn computes_cha_metrics() {
        let scope = test_scope();
        let metrics = SkxChaMetrics::from_measurements(
            &SKX_CHA_EVENT_GROUPS,
            BTreeMap::from([(scope, test_measurements())]),
            Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(metrics.scopes[0].frequency_hz, 10_000.0);
        assert_eq!(metrics.evictions[0].bandwidth_bytes_per_second, 128_000.0);
        assert_eq!(metrics.evictions[0].latency_seconds, 0.0002);
        assert_eq!(metrics.evictions[0].occupancy_entries, 0.4);
        assert_eq!(metrics.llc_lookups.len(), 32);
        assert_eq!(metrics.llc_lookups[0].bytes_per_second, 320_000.0);
        assert_eq!(metrics.llc_victims.len(), 4);
        assert_eq!(metrics.llc_victims[0].per_second, 400.0);
        assert_eq!(metrics.no_credits.len(), 2);
        assert_eq!(metrics.no_credits[0].ratio, 0.1);
        assert_eq!(
            no_credit_metrics(
                test_scope(),
                &BTreeMap::from([
                    (
                        ChaEventKind::NoCredits(ChaNoCreditDirection::Read),
                        ChaEventMeasurement {
                            enabled: Duration::from_millis(100),
                            represented_unit_count: 4,
                            running: Duration::from_millis(100),
                            value: 400,
                        },
                    ),
                    (
                        ChaEventKind::NoCredits(ChaNoCreditDirection::Write),
                        ChaEventMeasurement {
                            enabled: Duration::from_millis(100),
                            represented_unit_count: 4,
                            running: Duration::from_millis(100),
                            value: 800,
                        },
                    ),
                    (
                        ChaEventKind::NoCreditsClockticks,
                        ChaEventMeasurement {
                            enabled: Duration::from_millis(100),
                            represented_unit_count: 4,
                            running: Duration::from_millis(100),
                            value: 1_000,
                        },
                    ),
                ]),
            )
            .unwrap()[0]
                .ratio,
            0.1
        );
        assert_eq!(metrics.request_queues.len(), 2);
        assert_eq!(metrics.request_queues[0].occupancy_entries, 0.4);
        assert_eq!(metrics.rxc.len(), 2);
        assert_eq!(metrics.rxc[0].inserts_per_second, 2_000.0);
        assert_eq!(metrics.rxc[0].latency_seconds, 0.0002);
        assert_eq!(metrics.rxc[0].occupancy_entries, 0.4);
        assert_eq!(metrics.sf_evictions.len(), 3);
        assert_eq!(metrics.sf_evictions[0].bytes_per_second, 2_560.0);
        assert_eq!(
            metrics.transaction_results.len(),
            CHA_TRANSACTIONS.len() * 2
        );
        assert_eq!(metrics.transaction_results[0].inserts_per_second, 2_000.0);
        assert_eq!(metrics.transaction_results[0].latency_seconds, 0.0002);
        assert_eq!(metrics.transaction_results[0].occupancy_entries, 0.4);
        assert_eq!(metrics.transactions.len(), CHA_TRANSACTIONS.len());
        assert_eq!(
            metrics.transactions[0].bandwidth_bytes_per_second,
            192_000.0
        );
        assert!((metrics.transactions[0].hit_rate - (2.0 / 3.0)).abs() < 1e-9);
        assert!((metrics.transactions[0].latency_seconds - 0.0003).abs() < 1e-12);
    }

    #[test]
    fn encodes_skx_cha_filters() {
        assert_eq!(ChaFilter0::none().value(), 0);
        assert_eq!(ChaFilter0::llc_lookup_any_state().value(), 0xff << 17);
        assert_eq!(ChaFilter1::total_all_opcodes().value(), 0x3b);
        assert_eq!(ChaFilter1::total_opcode(0x202).value(), 0x40433);
    }

    #[test]
    fn uses_counting_eviction_tor_events() {
        let eviction_group = SKX_CHA_EVENT_GROUPS[0];

        assert_eq!(eviction_group.filter0, ChaFilter0::none());
        assert_eq!(eviction_group.filter1, ChaFilter1::total_all_opcodes());
        assert_eq!(
            eviction_group.events,
            [
                ChaEventSpec::sum(ChaEventKind::EvictionOccupancy, 0x36, 0x32),
                ChaEventSpec::sum(ChaEventKind::EvictionInsert, 0x35, 0x32),
                ChaEventSpec::clockticks(ChaEventKind::EvictionClockticks),
                ChaEventSpec::unused(),
            ]
        );
    }

    #[test]
    fn uses_documented_llc_victim_events() {
        let victim_group = SKX_CHA_EVENT_GROUPS[33];

        assert_eq!(
            victim_group.events,
            [
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::M), 0x37, 0x01),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::E), 0x37, 0x02),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::S), 0x37, 0x04),
                ChaEventSpec::sum(ChaEventKind::LlcVictim(ChaCacheState::F), 0x37, 0x08),
            ]
        );
    }

    #[test]
    fn uses_separate_ia_and_io_clflush_events() {
        assert_transaction_group(
            SKX_CHA_EVENT_GROUPS[17],
            ChaTransactionKind::IaClFlush,
            ChaTransactionResult::Hit,
            0x11,
        );
        assert_transaction_group(
            SKX_CHA_EVENT_GROUPS[18],
            ChaTransactionKind::IaClFlush,
            ChaTransactionResult::Miss,
            0x21,
        );
        assert_transaction_group(
            SKX_CHA_EVENT_GROUPS[19],
            ChaTransactionKind::IoClFlush,
            ChaTransactionResult::Hit,
            0x14,
        );
        assert_transaction_group(
            SKX_CHA_EVENT_GROUPS[20],
            ChaTransactionKind::IoClFlush,
            ChaTransactionResult::Miss,
            0x24,
        );
    }

    #[test]
    fn uses_documented_skx_transaction_filters() {
        let cases = [
            (
                9,
                ChaTransactionKind::IoPciRdCur,
                ChaTransactionResult::Hit,
                ChaRequestSource::Io,
                0x21e,
            ),
            (
                10,
                ChaTransactionKind::IoPciRdCur,
                ChaTransactionResult::Miss,
                ChaRequestSource::Io,
                0x21e,
            ),
            (
                11,
                ChaTransactionKind::IoItoM,
                ChaTransactionResult::Hit,
                ChaRequestSource::Io,
                0x248,
            ),
            (
                12,
                ChaTransactionKind::IoItoM,
                ChaTransactionResult::Miss,
                ChaRequestSource::Io,
                0x248,
            ),
            (
                13,
                ChaTransactionKind::IoWbMtoI,
                ChaTransactionResult::Hit,
                ChaRequestSource::Io,
                0x244,
            ),
            (
                14,
                ChaTransactionKind::IoWbMtoI,
                ChaTransactionResult::Miss,
                ChaRequestSource::Io,
                0x244,
            ),
            (
                15,
                ChaTransactionKind::IaWbMtoI,
                ChaTransactionResult::Hit,
                ChaRequestSource::Ia,
                0x244,
            ),
            (
                16,
                ChaTransactionKind::IaWbMtoI,
                ChaTransactionResult::Miss,
                ChaRequestSource::Ia,
                0x244,
            ),
            (
                17,
                ChaTransactionKind::IaClFlush,
                ChaTransactionResult::Hit,
                ChaRequestSource::Ia,
                0x218,
            ),
            (
                18,
                ChaTransactionKind::IaClFlush,
                ChaTransactionResult::Miss,
                ChaRequestSource::Ia,
                0x218,
            ),
            (
                19,
                ChaTransactionKind::IoClFlush,
                ChaTransactionResult::Hit,
                ChaRequestSource::Io,
                0x218,
            ),
            (
                20,
                ChaTransactionKind::IoClFlush,
                ChaTransactionResult::Miss,
                ChaRequestSource::Io,
                0x218,
            ),
            (
                21,
                ChaTransactionKind::IoItoMCacheNear,
                ChaTransactionResult::Hit,
                ChaRequestSource::Io,
                0x200,
            ),
            (
                22,
                ChaTransactionKind::IoItoMCacheNear,
                ChaTransactionResult::Miss,
                ChaRequestSource::Io,
                0x200,
            ),
            (
                23,
                ChaTransactionKind::IaDrd,
                ChaTransactionResult::Hit,
                ChaRequestSource::Ia,
                0x202,
            ),
            (
                24,
                ChaTransactionKind::IaDrd,
                ChaTransactionResult::Miss,
                ChaRequestSource::Ia,
                0x202,
            ),
            (
                25,
                ChaTransactionKind::IaItoM,
                ChaTransactionResult::Hit,
                ChaRequestSource::Ia,
                0x248,
            ),
            (
                26,
                ChaTransactionKind::IaItoM,
                ChaTransactionResult::Miss,
                ChaRequestSource::Ia,
                0x248,
            ),
            (
                27,
                ChaTransactionKind::IaRfo,
                ChaTransactionResult::Hit,
                ChaRequestSource::Ia,
                0x200,
            ),
            (
                28,
                ChaTransactionKind::IaRfo,
                ChaTransactionResult::Miss,
                ChaRequestSource::Ia,
                0x200,
            ),
        ];

        for (index, transaction, result, source, opcode) in cases {
            let group = SKX_CHA_EVENT_GROUPS[index];
            assert_eq!(group.filter1, ChaFilter1::total_opcode(opcode));
            assert_transaction_group(group, transaction, result, source.result_umask(result));
        }
    }

    #[test]
    fn uses_skx_cha_group_count() {
        assert_eq!(SKX_CHA_EVENT_GROUPS.len(), SKX_CHA_EVENT_GROUP_COUNT);
    }

    const PROFILES: [EventProfile; 4] = [
        EventProfile::Default,
        EventProfile::MemIo2Hit,
        EventProfile::MemIoBasic,
        EventProfile::MemIoItom,
    ];

    #[test]
    fn sizes_event_profiles() {
        assert_eq!(event_group_count(EventProfile::Default), 37);
        assert_eq!(event_group_count(EventProfile::MemIo2Hit), 6);
        assert_eq!(event_group_count(EventProfile::MemIoBasic), 12);
        assert_eq!(event_group_count(EventProfile::MemIoItom), 1);
    }

    #[test]
    fn gives_every_group_of_a_profile_a_unique_id() {
        for profile in PROFILES {
            let groups = skx_cha_event_groups(profile);
            let ids = groups.iter().map(|group| group.id).collect::<BTreeSet<_>>();

            assert_eq!(ids.len(), groups.len(), "{profile:?}");
        }
    }

    #[test]
    fn orders_mem_io_groups_like_the_spec() {
        let ids = |profile| {
            skx_cha_event_groups(profile)
                .iter()
                .map(|group| group.id.label())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(EventProfile::MemIo2Hit),
            [
                "tor_itom",
                "tor_rfo",
                "tor_rdcur",
                "tor_wbmtoi",
                "llc_lookup_m",
                "llc_lookup_sf_e",
            ]
        );
        assert_eq!(ids(EventProfile::MemIoItom), ["tor_itom"]);
        // peacock 3565bd2 order: Eviction, PCIe Read Hit/Miss, Full Write
        // Hit/Miss, Writeback, Partial Write Hit/Miss, DRD Hit/Miss, LLC
        // Victims, HA Request.
        assert_eq!(
            ids(EventProfile::MemIoBasic),
            [
                "eviction",
                "trx_io_pcirdcur_hit",
                "trx_io_pcirdcur_miss",
                "trx_io_itom_hit",
                "trx_io_itom_miss",
                "trx_io_wbmtoi_hit",
                "trx_io_itomcachenear_hit",
                "trx_io_itomcachenear_miss",
                "trx_ia_drd_hit",
                "trx_ia_drd_miss",
                "llc_victims_local",
                "ha_requests",
            ]
        );
    }

    #[test]
    fn counts_local_llc_victims_in_mem_io_basic() {
        let victims = skx_cha_event_groups(EventProfile::MemIoBasic)
            .iter()
            .find(|group| group.id == ChaGroupId::LlcVictimsLocal)
            .unwrap();

        // peacock 3565bd2 "LLC Victims": UNC_CHA_LLC_VICTIMS.LOCAL_{M,E,S,F}.
        assert_eq!(
            victims.events.map(|event| (event.event, event.umask)),
            [(0x37, 0x21), (0x37, 0x22), (0x37, 0x24), (0x37, 0x28)]
        );
        assert_eq!(victims.filter1, ChaFilter1::total_all_opcodes());
    }

    #[test]
    fn encodes_tor_insert_groups() {
        for (opcode, value) in [
            (ChaTorOpcode::ItoM, 0x248),
            (ChaTorOpcode::Rfo, 0x200),
            (ChaTorOpcode::RdCur, 0x21e),
            (ChaTorOpcode::WbMtoI, 0x244),
        ] {
            let group = ChaEventGroup::tor(opcode);

            assert_eq!(group.filter0, ChaFilter0::none());
            assert_eq!(group.filter1, ChaFilter1::total_opcode(value));
            assert_eq!(
                group.events.map(|event| (event.event, event.umask)),
                [(0x35, 0x34), (0x35, 0x24), (0x35, 0x31), (0x35, 0x21)]
            );
        }

        // peacock programs TOR groups with filter1 (opc << 9) | 0x32.
        assert_eq!(
            programmed_filter1(
                ChaEventGroup::tor(ChaTorOpcode::ItoM),
                ChaRequestLocality::Local
            ),
            (0x248 << 9) | 0x32
        );
    }

    #[test]
    fn schedules_every_mem_io_group_equally_within_one_second() {
        for (profile, rounds) in [
            (EventProfile::MemIo2Hit, 2),
            (EventProfile::MemIoBasic, 1),
            (EventProfile::MemIoItom, 10),
        ] {
            let mut collector = test_collector();
            collector.set_event_profile(profile);
            let groups = skx_cha_event_groups(profile);
            let slices = collector.schedule(Duration::from_secs(1));
            let slice_duration = Duration::from_secs(1).div_f64(slices.len() as f64);

            assert_eq!(slices.len(), groups.len() * rounds, "{profile:?}");
            assert!(slices.iter().all(|slice| slice.duration == slice_duration));
            let scheduled = slice_groups(slices);
            for group in groups {
                assert_eq!(
                    scheduled
                        .iter()
                        .filter(|slice| slice.id == group.id)
                        .count(),
                    rounds,
                    "{profile:?} {:?}",
                    group.id
                );
            }
        }
    }

    #[test]
    fn computes_only_mem_io_2hit_metrics() {
        let groups = skx_cha_event_groups(EventProfile::MemIo2Hit);
        let metrics = SkxChaMetrics::from_measurements(
            groups,
            BTreeMap::from([(test_scope(), measurements_for(groups))]),
            event_group_timings(groups, &BTreeMap::new()),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(metrics.tor_inserts.len(), 4 * 2 * 2);
        assert!(
            metrics
                .tor_inserts
                .iter()
                .all(|metric| metric.inserts_per_second == 1_000.0)
        );
        assert_eq!(metrics.llc_lookups.len(), 2 * 4);
        assert!(
            metrics
                .llc_lookups
                .iter()
                .all(|metric| matches!(metric.state, ChaCacheState::M | ChaCacheState::SfE))
        );
        assert_eq!(metrics.event_groups.len(), 6);
        assert!(metrics.scopes.is_empty());
        assert!(metrics.evictions.is_empty());
        assert!(metrics.ha_requests.is_empty());
        assert!(metrics.llc_victims.is_empty());
        assert!(metrics.no_credits.is_empty());
        assert!(metrics.request_queues.is_empty());
        assert!(metrics.rxc.is_empty());
        assert!(metrics.sf_evictions.is_empty());
        assert!(metrics.transaction_results.is_empty());
        assert!(metrics.transactions.is_empty());
    }

    #[test]
    fn computes_mem_io_basic_metrics() {
        let groups = skx_cha_event_groups(EventProfile::MemIoBasic);
        let metrics = SkxChaMetrics::from_measurements(
            groups,
            BTreeMap::from([(test_scope(), measurements_for(groups))]),
            Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(metrics.scopes.len(), 1);
        assert_eq!(metrics.evictions.len(), 1);
        assert_eq!(metrics.ha_requests.len(), 1);
        assert_eq!(metrics.llc_victims.len(), 4);
        // Writeback measures only its hit group, so it has no total.
        assert_eq!(metrics.transaction_results.len(), 9);
        assert_eq!(
            metrics
                .transactions
                .iter()
                .map(|metric| metric.transaction.as_str())
                .collect::<Vec<_>>(),
            ["io_pcirdcur", "io_itom", "io_itomcachenear", "ia_drd"]
        );
        assert!(metrics.tor_inserts.is_empty());
        assert!(metrics.llc_lookups.is_empty());
        assert!(metrics.no_credits.is_empty());
        assert!(metrics.request_queues.is_empty());
        assert!(metrics.rxc.is_empty());
        assert!(metrics.sf_evictions.is_empty());
    }

    #[test]
    fn rejects_missing_measurement_of_a_profile_group() {
        let groups = skx_cha_event_groups(EventProfile::MemIoItom);
        let mut measurements = measurements_for(groups);
        measurements.remove(&ChaEventKind::TorInsert(
            ChaTorOpcode::ItoM,
            ChaRequestSource::Io,
            ChaTorResult::Miss,
        ));

        assert!(
            SkxChaMetrics::from_measurements(
                groups,
                BTreeMap::from([(test_scope(), measurements)]),
                Vec::new(),
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    #[test]
    fn reports_running_time_of_every_profile_group() {
        let timings = event_group_timings(
            skx_cha_event_groups(EventProfile::MemIo2Hit),
            &BTreeMap::from([(
                ChaGroupId::Tor(ChaTorOpcode::ItoM),
                Duration::from_millis(160),
            )]),
        );

        assert_eq!(timings.len(), 6);
        assert_eq!(timings[0].group, "tor_itom");
        assert_eq!(timings[0].running_seconds, 0.16);
        assert!(
            timings[1..]
                .iter()
                .all(|timing| timing.running_seconds == 0.0)
        );
    }

    #[test]
    fn filters_local_and_remote_requests_by_default() {
        let itom = ChaEventGroup::transaction(
            ChaTransactionKind::IoItoM,
            ChaTransactionResult::Miss,
            ChaRequestSource::Io,
            0x248,
        );
        let lookup = ChaEventGroup::llc_lookup(ChaCacheState::M);

        // 0x49033 is the Filter1 value perfmon lists for UNC_CHA_TOR_INSERTS.IO_MISS_ITOM.
        assert_eq!(programmed_filter1(itom, ChaRequestLocality::All), 0x49033);
        assert_eq!(programmed_filter1(lookup, ChaRequestLocality::All), 0x3b);
        assert_eq!(test_collector().request_locality, ChaRequestLocality::All);
    }

    #[test]
    fn filters_only_local_requests_when_requested() {
        let itom = ChaEventGroup::transaction(
            ChaTransactionKind::IoItoM,
            ChaTransactionResult::Miss,
            ChaRequestSource::Io,
            0x248,
        );
        let lookup = ChaEventGroup::llc_lookup(ChaCacheState::M);

        assert_eq!(programmed_filter1(itom, ChaRequestLocality::Local), 0x49032);
        assert_eq!(programmed_filter1(lookup, ChaRequestLocality::Local), 0x3a);
    }

    #[test]
    fn local_request_locality_clears_only_the_remote_bit_of_every_group() {
        for group in SKX_CHA_EVENT_GROUPS {
            let all = programmed_filter1(group, ChaRequestLocality::All);
            let local = programmed_filter1(group, ChaRequestLocality::Local);

            assert_ne!(all & CHA_FILTER1_REMOTE_BIT, 0);
            assert_eq!(local, all & !CHA_FILTER1_REMOTE_BIT);
            assert_ne!(local & CHA_FILTER1_LOCAL_BIT, 0);
        }
    }

    #[test]
    fn stores_requested_locality() {
        let mut collector = test_collector();

        collector.set_request_locality(ChaRequestLocality::Local);

        assert_eq!(collector.request_locality, ChaRequestLocality::Local);
    }

    #[test]
    fn uses_full_skx_cha_address_map() {
        assert_eq!(cha_unit_control_offset(0), 0x0e00);
        assert_eq!(cha_control_offset(0, 0), 0x0e01);
        assert_eq!(cha_filter0_offset(0), 0x0e05);
        assert_eq!(cha_filter1_offset(0), 0x0e06);
        assert_eq!(cha_counter_offset(0, 0), 0x0e08);

        assert_eq!(cha_unit_control_offset(27), 0x0fb0);
        assert_eq!(cha_control_offset(27, 3), 0x0fb4);
        assert_eq!(cha_counter_offset(27, 3), 0x0fbb);
    }

    #[test]
    fn decodes_skx_cha_count_from_capid6() {
        assert_eq!(
            skx_cha_unit_ids_from_capid6(0x000f).unwrap(),
            (0..4).collect::<Vec<usize>>()
        );
        assert_eq!(
            skx_cha_unit_ids_from_capid6(0xf0f0).unwrap(),
            (0..8).collect::<Vec<usize>>()
        );
        assert_eq!(skx_cha_unit_ids_from_capid6(0xf000_0001).unwrap(), vec![0]);
        assert_eq!(
            skx_cha_unit_ids_from_capid6(0x03d2_f4f4).unwrap(),
            (0..16).collect::<Vec<usize>>()
        );
        assert_eq!(
            skx_cha_unit_ids_from_capid6(0x02e9_eb74).unwrap(),
            (0..16).collect::<Vec<usize>>()
        );
        assert!(skx_cha_unit_ids_from_capid6(0).is_err());
    }

    #[test]
    fn falls_back_to_full_skx_cha_msr_probe_range() {
        assert_eq!(
            skx_cha_unit_ids_for_msr_probe(),
            (0..SKX_MAX_CHA_COUNT).collect::<Vec<usize>>()
        );
    }

    #[test]
    fn rotates_starting_event_group() {
        let mut collector = test_collector();

        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            SKX_CHA_EVENT_GROUPS.to_vec()
        );

        collector.rotate_group();
        let mut expected = SKX_CHA_EVENT_GROUPS.to_vec();
        expected.rotate_left(1);
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            expected
        );
    }

    #[test]
    fn spatial_multiplexing_groups_events_deterministically() {
        let mut collector = test_collector();
        collector.set_multiplex_mode(ChaMultiplexMode::spatial(4));

        let slices = collector.schedule(Duration::from_millis(100));
        assert_eq!(slices.len(), SKX_CHA_EVENT_GROUP_COUNT.div_ceil(4));
        assert_eq!(
            slice_group_partitions(slices)[0],
            SKX_CHA_EVENT_GROUPS[0..4].to_vec()
        );

        collector.rotate_group();
        let rotated_groups = slice_group_partitions(collector.schedule(Duration::from_millis(100)));
        let flattened_groups: Vec<ChaEventGroup> =
            rotated_groups.iter().flatten().copied().collect();
        let mut expected = SKX_CHA_EVENT_GROUPS.to_vec();
        expected.rotate_left(4);
        assert_eq!(rotated_groups[0], SKX_CHA_EVENT_GROUPS[4..8].to_vec());
        assert_eq!(flattened_groups, expected);
    }

    #[test]
    fn spatial_multiplexing_uses_contiguous_partition_offsets() {
        let mut collector = test_collector_with_units(SKX_MAX_CHA_COUNT);
        collector.set_multiplex_mode(ChaMultiplexMode::spatial(4));

        let slices = collector.schedule(Duration::from_secs(2));
        let partition_offsets: Vec<usize> =
            slices.iter().map(|slice| slice.partition_offset).collect();

        assert_eq!(partition_offsets, (0..slices.len()).collect::<Vec<usize>>());
    }

    #[test]
    fn spatial_multiplexing_advances_sampled_units_after_full_sample() {
        let mut collector = test_collector_with_units(SKX_MAX_CHA_COUNT);
        collector.set_multiplex_mode(ChaMultiplexMode::spatial(4));
        let group = SKX_CHA_EVENT_GROUPS[0];
        let mut sampled_units = Vec::new();

        for _ in 0..3 {
            let slices = collector.schedule(Duration::from_millis(100));
            sampled_units.push(sampled_units_for_group(&slices, group, SKX_MAX_CHA_COUNT));
            collector.rotate_schedule(slices.len());
        }

        assert_ne!(sampled_units[1], sampled_units[2]);
    }

    #[test]
    fn spatial_multiplexing_falls_back_when_partitions_exceed_cha_units() {
        let mut collector = test_collector_with_units(3);
        collector.set_multiplex_mode(ChaMultiplexMode::spatial(4));

        assert_eq!(collector.multiplex_mode, ChaMultiplexMode::Temporal);
        assert_eq!(
            slice_groups(collector.schedule(Duration::from_millis(100))),
            SKX_CHA_EVENT_GROUPS.to_vec()
        );
    }

    #[test]
    fn supports_only_skylake_xeon_uncore_spec() {
        assert!(SkxChaCollector::is_supported(&test_architecture(0x55)));
        assert!(!SkxChaCollector::is_supported(&test_architecture(0xcf)));
    }

    #[test]
    fn programs_a_config_group_on_one_cha() {
        let mut group = config_group(0, &[5], &[0, 2]);
        group.counters[0].control = 0x40_0035;
        group.counters[1].control = 0x40_0f36;

        assert_eq!(
            cha_config_unit_writes(5, &group).unwrap(),
            vec![
                (0x0e50, 0x3_0103),
                (0x0e55, 0x1fe_0000),
                (0x0e56, 0x3b),
                (0x0e51, 0x42_0035),
                (0x0e52, 0),
                (0x0e53, 0x42_0f36),
                (0x0e54, 0),
            ]
        );
    }

    #[test]
    fn schedules_config_slices_by_grouping() {
        let slice = |milliseconds, groups: &[usize]| ChaConfigSlice {
            duration: Duration::from_millis(milliseconds),
            groups: groups.to_vec(),
        };
        let fixed = config_program(
            Grouping::Fixed,
            Aggregate::Sum,
            vec![config_group(0, &[0, 1], &[0])],
        );
        let spatial = config_program(
            Grouping::Spatial,
            Aggregate::Sum,
            vec![
                config_group(0, &[0, 1], &[0]),
                config_group(1, &[2], &[0]),
                config_group(2, &[3], &[0]),
            ],
        );
        let temporal = config_program(
            Grouping::Temporal,
            Aggregate::Sum,
            (0..3)
                .map(|index| config_group(index, &[0, 1], &[0]))
                .collect(),
        );

        assert_eq!(
            cha_config_schedule(&fixed, Duration::from_secs(1), 0),
            vec![slice(1_000, &[0])]
        );
        assert_eq!(
            cha_config_schedule(&spatial, Duration::from_secs(1), 0),
            vec![slice(1_000, &[0, 1, 2])]
        );
        assert_eq!(
            cha_config_schedule(&temporal, Duration::from_millis(300), 0),
            vec![slice(100, &[0]), slice(100, &[1]), slice(100, &[2])]
        );
        assert_eq!(
            cha_config_schedule(&temporal, Duration::from_millis(600), 0),
            vec![
                slice(100, &[0]),
                slice(100, &[1]),
                slice(100, &[2]),
                slice(100, &[0]),
                slice(100, &[1]),
                slice(100, &[2]),
            ]
        );
    }

    fn scheduled_slices(
        program: &ProgrammedBoxType,
        interval: Duration,
        cycle: u64,
    ) -> Vec<crate::metrics::testing::ScheduledSlice> {
        cha_config_schedule(program, interval, cycle)
            .into_iter()
            .map(|slice| (slice.duration, slice.groups))
            .collect()
    }

    #[test]
    fn precesses_temporal_config_groups_by_cycle() {
        crate::metrics::testing::assert_precessing_schedule(BoxType::Cha, scheduled_slices);
    }

    #[test]
    fn reads_phase_locked_sources_without_bias_over_a_cycle_of_samples() {
        crate::metrics::testing::assert_unbiased_by_precession(BoxType::Cha, scheduled_slices);
    }

    #[test]
    fn programs_only_the_chas_of_a_slices_groups() {
        let packages = vec![
            ChaPackage::new(
                test_scope(),
                (0..6).map(|id| ChaUnit { cpu: 3, id }).collect(),
            ),
            ChaPackage::new(
                UncoreScope {
                    package_id: 1,
                    ..test_scope()
                },
                (0..6).map(|id| ChaUnit { cpu: 30, id }).collect(),
            ),
        ];
        let mut program = config_program(
            Grouping::Spatial,
            Aggregate::Sum,
            vec![config_group(0, &[1, 2], &[0]), config_group(1, &[4], &[0])],
        );
        program.groups[1].units.insert(1, BTreeSet::from([4]));
        let units = |groups: &[usize]| {
            let slice = ChaConfigSlice {
                duration: Duration::from_secs(1),
                groups: groups.to_vec(),
            };
            cha_config_units(&packages, &program, &slice)
                .unwrap()
                .into_iter()
                .map(|unit| (unit.package, unit.cha.cpu, unit.cha.id, unit.group))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            units(&[0, 1]),
            vec![(0, 3, 1, 0), (0, 3, 2, 0), (0, 3, 4, 1), (1, 30, 4, 1)]
        );
        assert_eq!(units(&[1]), vec![(0, 3, 4, 1), (1, 30, 4, 1)]);
    }

    #[test]
    fn folds_masked_config_readings_per_box_or_summed() {
        for aggregate in [Aggregate::Box, Aggregate::Sum] {
            let program = config_program(
                Grouping::Spatial,
                aggregate,
                vec![
                    config_group(0, &[1, 2], &[0, 2]),
                    config_group(1, &[4], &[1]),
                ],
            );
            let mut accumulator = CountAccumulator::new(test_config(program.clone()));
            let slice = ChaConfigSlice {
                duration: Duration::from_millis(100),
                groups: vec![0, 1],
            };
            let readings = [
                config_reading(1, 0, &[(0, 10), (2, (1 << 48) | 7)]),
                config_reading(2, 0, &[(0, 5), (2, 1)]),
                config_reading(4, 1, &[(1, 3)]),
            ];
            let mut counts = Vec::new();
            for _ in 0..2 {
                add_cha_config_slice(
                    &mut accumulator,
                    &program,
                    &slice,
                    &readings,
                    Duration::from_millis(100),
                )
                .unwrap();
                counts.push(accumulator.counts());
            }

            let values = |counts: &UncoreCounts| {
                counts
                    .counts
                    .iter()
                    .map(|count| (count.unit, count.group, count.slot, count.value))
                    .collect::<Vec<_>>()
            };
            let general = CounterSlot::General;
            match aggregate {
                Aggregate::Box => {
                    assert_eq!(
                        values(&counts[0]),
                        vec![
                            (CountUnit::Box(1), 0, general(0), 10),
                            (CountUnit::Box(1), 0, general(2), 7),
                            (CountUnit::Box(2), 0, general(0), 5),
                            (CountUnit::Box(2), 0, general(2), 1),
                            (CountUnit::Box(4), 1, general(1), 3),
                        ]
                    );
                    assert_eq!(
                        values(&counts[1]),
                        vec![
                            (CountUnit::Box(1), 0, general(0), 20),
                            (CountUnit::Box(1), 0, general(2), 14),
                            (CountUnit::Box(2), 0, general(0), 10),
                            (CountUnit::Box(2), 0, general(2), 2),
                            (CountUnit::Box(4), 1, general(1), 6),
                        ]
                    );
                }
                Aggregate::Sum => {
                    assert_eq!(
                        values(&counts[0]),
                        vec![
                            (CountUnit::Sum, 0, general(0), 15),
                            (CountUnit::Sum, 0, general(2), 8),
                            (CountUnit::Sum, 1, general(1), 3),
                        ]
                    );
                    assert_eq!(
                        values(&counts[1]),
                        vec![
                            (CountUnit::Sum, 0, general(0), 30),
                            (CountUnit::Sum, 0, general(2), 16),
                            (CountUnit::Sum, 1, general(1), 6),
                        ]
                    );
                }
            }
            assert_eq!(running_seconds(&counts[1]), vec![(0, 0, 0.2), (0, 1, 0.2)]);
        }
    }

    #[test]
    fn gives_temporal_groups_the_running_time_of_their_own_slices() {
        let mut program = config_program(
            Grouping::Temporal,
            Aggregate::Sum,
            vec![config_group(0, &[1], &[0]), config_group(1, &[1], &[0])],
        );
        for group in &mut program.groups {
            group.units.insert(1, BTreeSet::from([1]));
        }
        let mut accumulator = CountAccumulator::new(test_config(program.clone()));

        for (group, milliseconds) in [(0, 30), (1, 70), (0, 30)] {
            let slice = ChaConfigSlice {
                duration: Duration::from_millis(milliseconds),
                groups: vec![group],
            };
            add_cha_config_slice(
                &mut accumulator,
                &program,
                &slice,
                &[],
                Duration::from_millis(milliseconds),
            )
            .unwrap();
        }

        assert_eq!(
            running_seconds(&accumulator.counts()),
            vec![(0, 0, 0.06), (0, 1, 0.07), (1, 0, 0.06), (1, 1, 0.07)]
        );
    }

    #[test]
    fn reports_discovered_chas_as_config_units() {
        let mut collector = test_collector_with_units(3);
        collector.packages.push(ChaPackage::new(
            UncoreScope {
                package_id: 1,
                ..test_scope()
            },
            vec![ChaUnit { cpu: 30, id: 0 }, ChaUnit { cpu: 30, id: 2 }],
        ));

        assert_eq!(
            collector.config_units(),
            BTreeMap::from([(0, BTreeSet::from([0, 1, 2])), (1, BTreeSet::from([0, 2]))])
        );
    }

    #[test]
    fn rejects_configs_the_cha_collector_cannot_run() {
        let mut collector = test_collector_with_units(6);
        let valid = || {
            config_program(
                Grouping::Fixed,
                Aggregate::Sum,
                vec![config_group(0, &[1, 2], &[0])],
            )
        };
        let mut iio = valid();
        iio.box_type = BoxType::Iio;
        let mut other_package = valid();
        other_package.groups[0].units = BTreeMap::from([(1, BTreeSet::from([1]))]);
        let mut other_unit = valid();
        other_unit.groups[0].units = BTreeMap::from([(0, BTreeSet::from([2, 6]))]);
        let mut fixed_slot = valid();
        fixed_slot.groups[0].counters[0].slot = CounterSlot::Fixed;
        let mut fifth_slot = valid();
        fifth_slot.groups[0].counters[0].slot = CounterSlot::General(4);
        let mut no_filters = valid();
        no_filters.groups[0].filters = None;

        for (program, error) in [
            (iio, "cannot run iio configs"),
            (other_package, "package 1, which this host lacks"),
            (other_unit, "CHA 6, which package 0 lacks"),
            (fixed_slot, "no counter slot fixed"),
            (fifth_slot, "no counter slot 4"),
            (no_filters, "has no filters"),
        ] {
            let result = collector.set_config(Some(test_config(program)));
            assert!(
                result
                    .as_ref()
                    .is_err_and(|message| message.contains(error)),
                "{result:?}"
            );
            assert!(!collector.has_config());
        }
        collector.set_config(Some(test_config(valid()))).unwrap();
        assert!(collector.has_config());
    }

    #[test]
    fn setting_a_config_restarts_counts_from_zero() {
        let mut collector = test_collector_with_units(6);
        let first = config_program(
            Grouping::Fixed,
            Aggregate::Box,
            vec![config_group(0, &[1], &[0])],
        );
        let second = config_program(
            Grouping::Fixed,
            Aggregate::Box,
            vec![config_group(0, &[1], &[3])],
        );
        let count_one_slice =
            |collector: &mut SkxChaCollector, program: &ProgrammedBoxType, slot: u8| {
                let slice = cha_config_schedule(program, Duration::from_secs(1), 0).remove(0);
                add_cha_config_slice(
                    collector.config.as_mut().unwrap(),
                    program,
                    &slice,
                    &[config_reading(1, 0, &[(slot, 9)])],
                    Duration::from_secs(1),
                )
                .unwrap();
            };
        let counts = |collector: &SkxChaCollector| {
            let counts = collector.config.as_ref().unwrap().counts();
            (
                counts.config.program.groups[0].counters[0].slot,
                counts.counts[0].value,
                counts.running[0].seconds,
            )
        };

        collector
            .set_config(Some(test_config(first.clone())))
            .unwrap();
        count_one_slice(&mut collector, &first, 0);
        assert_eq!(counts(&collector), (CounterSlot::General(0), 9, 1.0));

        collector
            .set_config(Some(test_config(second.clone())))
            .unwrap();
        assert_eq!(counts(&collector), (CounterSlot::General(3), 0, 0.0));
        count_one_slice(&mut collector, &second, 3);
        assert_eq!(counts(&collector), (CounterSlot::General(3), 9, 1.0));

        collector.set_config(Some(test_config(second))).unwrap();
        assert_eq!(counts(&collector), (CounterSlot::General(3), 0, 0.0));

        collector.set_config(None).unwrap();
        assert!(!collector.has_config());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(collector.sample_config(Duration::from_secs(1), 0))
                .is_err()
        );
    }

    fn config_group(index: u8, units: &[u16], slots: &[u8]) -> ProgrammedGroup {
        ProgrammedGroup {
            index,
            counters: slots
                .iter()
                .map(|&slot| ProgrammedCounter {
                    slot: CounterSlot::General(slot),
                    control: 0x40_0035,
                    labels: EventLabels::new(),
                })
                .collect(),
            filters: Some(ChaFilters {
                filter0: 0x1fe_0000,
                filter1: 0x3b,
            }),
            units: BTreeMap::from([(0, units.iter().copied().collect())]),
        }
    }

    fn config_program(
        grouping: Grouping,
        aggregate: Aggregate,
        groups: Vec<ProgrammedGroup>,
    ) -> ProgrammedBoxType {
        ProgrammedBoxType {
            box_type: BoxType::Cha,
            grouping,
            aggregate,
            groups,
        }
    }

    fn test_config(program: ProgrammedBoxType) -> PmuConfig {
        PmuConfig {
            config_id: 7,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            program,
        }
    }

    fn config_reading(unit: u16, group: u8, counters: &[(u8, u64)]) -> ChaConfigReading {
        ChaConfigReading {
            package: 0,
            unit,
            group,
            counters: counters
                .iter()
                .map(|&(slot, value)| (CounterSlot::General(slot), value))
                .collect(),
        }
    }

    fn running_seconds(counts: &UncoreCounts) -> Vec<(u16, u8, f64)> {
        counts
            .running
            .iter()
            .map(|running| (running.package, running.group, running.seconds))
            .collect()
    }

    fn measurement(
        kind: ChaEventKind,
        value: u64,
        milliseconds: u64,
    ) -> (ChaEventKind, ChaEventMeasurement) {
        (
            kind,
            ChaEventMeasurement {
                enabled: Duration::from_millis(milliseconds),
                represented_unit_count: 1,
                running: Duration::from_millis(milliseconds),
                value,
            },
        )
    }

    fn measurements_for(groups: &[ChaEventGroup]) -> BTreeMap<ChaEventKind, ChaEventMeasurement> {
        measured_event_kinds(groups)
            .into_iter()
            .map(|kind| measurement(kind, 100, 100))
            .collect()
    }

    fn slice_groups(slices: Vec<ChaMeasurementSlice>) -> Vec<ChaEventGroup> {
        slices
            .into_iter()
            .map(|slice| slice.groups[0].expect("slice should have primary group"))
            .collect()
    }

    fn slice_group_partitions(slices: Vec<ChaMeasurementSlice>) -> Vec<Vec<ChaEventGroup>> {
        slices
            .into_iter()
            .map(|slice| slice.groups.into_iter().flatten().collect())
            .collect()
    }

    fn sampled_units_for_group(
        slices: &[ChaMeasurementSlice],
        group: ChaEventGroup,
        unit_count: usize,
    ) -> Vec<usize> {
        let slice = slices
            .iter()
            .find(|slice| slice.groups.contains(&Some(group)))
            .expect("group should be scheduled");
        let partition = slice
            .groups
            .iter()
            .position(|candidate| *candidate == Some(group))
            .expect("group should be assigned to a partition");

        (0..unit_count)
            .filter(|unit_index| cha_partition(*unit_index, *slice, unit_count) == partition)
            .collect()
    }

    fn test_architecture(model: u8) -> Architecture {
        Architecture {
            brand: "test".to_string(),
            family: 6,
            features: crate::arch::ArchitectureFeatures::default(),
            model,
            vendor: "GenuineIntel".to_string(),
        }
    }

    fn assert_transaction_group(
        group: ChaEventGroup,
        transaction: ChaTransactionKind,
        result: ChaTransactionResult,
        umask: u8,
    ) {
        let transaction = transaction.label();

        assert_eq!(
            group.events,
            [
                ChaEventSpec::sum(
                    ChaEventKind::TransactionOccupancy(transaction, result),
                    0x36,
                    umask
                ),
                ChaEventSpec::sum(
                    ChaEventKind::TransactionInsert(transaction, result),
                    0x35,
                    umask
                ),
                ChaEventSpec::clockticks(ChaEventKind::TransactionClockticks(transaction, result)),
                ChaEventSpec::unused(),
            ]
        );
    }

    fn test_collector() -> SkxChaCollector {
        test_collector_with_units(CHA_COUNTER_COUNT)
    }

    fn test_collector_with_units(unit_count: usize) -> SkxChaCollector {
        SkxChaCollector {
            config: None,
            groups: &SKX_CHA_EVENT_GROUPS,
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages: vec![ChaPackage::new(
                test_scope(),
                (0..unit_count).map(|id| ChaUnit { cpu: 0, id }).collect(),
            )],
            request_locality: ChaRequestLocality::default(),
        }
    }

    fn test_measurements() -> BTreeMap<ChaEventKind, ChaEventMeasurement> {
        let mut measurements = BTreeMap::from([
            measurement(ChaEventKind::EvictionClockticks, 1_000, 100),
            measurement(ChaEventKind::EvictionInsert, 200, 100),
            measurement(ChaEventKind::EvictionOccupancy, 400, 100),
            measurement(
                ChaEventKind::HaRequest(ChaHaRequestLocality::Local, ChaRequestOperation::Read),
                100,
                100,
            ),
            measurement(
                ChaEventKind::HaRequest(ChaHaRequestLocality::Remote, ChaRequestOperation::Read),
                50,
                100,
            ),
            measurement(
                ChaEventKind::HaRequest(ChaHaRequestLocality::Local, ChaRequestOperation::Write),
                80,
                100,
            ),
            measurement(
                ChaEventKind::HaRequest(ChaHaRequestLocality::Remote, ChaRequestOperation::Write),
                20,
                100,
            ),
            measurement(
                ChaEventKind::NoCredits(ChaNoCreditDirection::Read),
                100,
                100,
            ),
            measurement(
                ChaEventKind::NoCredits(ChaNoCreditDirection::Write),
                200,
                100,
            ),
            measurement(ChaEventKind::NoCreditsClockticks, 1_000, 100),
            measurement(
                ChaEventKind::RequestQueueClockticks(ChaRequestSource::Ia),
                1_000,
                100,
            ),
            measurement(
                ChaEventKind::RequestQueueClockticks(ChaRequestSource::Io),
                1_000,
                100,
            ),
            measurement(
                ChaEventKind::RequestQueueInsert(ChaRequestSource::Ia),
                200,
                100,
            ),
            measurement(
                ChaEventKind::RequestQueueInsert(ChaRequestSource::Io),
                100,
                100,
            ),
            measurement(
                ChaEventKind::RequestQueueOccupancy(ChaRequestSource::Ia),
                400,
                100,
            ),
            measurement(
                ChaEventKind::RequestQueueOccupancy(ChaRequestSource::Io),
                500,
                100,
            ),
        ]);

        for state in [
            ChaCacheState::SfS,
            ChaCacheState::SfE,
            ChaCacheState::SfH,
            ChaCacheState::I,
            ChaCacheState::S,
            ChaCacheState::E,
            ChaCacheState::M,
            ChaCacheState::F,
        ] {
            measurements.extend([
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Read),
                    500,
                    100,
                ),
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Write),
                    200,
                    100,
                ),
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::RemoteSnoop),
                    100,
                    100,
                ),
                measurement(
                    ChaEventKind::LlcLookup(state, ChaLookupOperation::Any),
                    800,
                    100,
                ),
            ]);
        }

        for (state, value) in [
            (ChaCacheState::M, 40),
            (ChaCacheState::E, 20),
            (ChaCacheState::S, 30),
            (ChaCacheState::F, 10),
        ] {
            measurements.insert(
                ChaEventKind::LlcVictim(state),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value,
                },
            );
        }

        for (state, value) in [
            (ChaCacheState::M, 4),
            (ChaCacheState::E, 2),
            (ChaCacheState::S, 3),
        ] {
            measurements.insert(
                ChaEventKind::SfEviction(state),
                ChaEventMeasurement {
                    enabled: Duration::from_millis(100),
                    represented_unit_count: 1,
                    running: Duration::from_millis(100),
                    value,
                },
            );
        }

        for transaction in CHA_TRANSACTIONS {
            let transaction = transaction.label();

            measurements.extend([
                measurement(
                    ChaEventKind::TransactionClockticks(transaction, ChaTransactionResult::Hit),
                    1_000,
                    100,
                ),
                measurement(
                    ChaEventKind::TransactionClockticks(transaction, ChaTransactionResult::Miss),
                    1_000,
                    100,
                ),
                measurement(
                    ChaEventKind::TransactionInsert(transaction, ChaTransactionResult::Hit),
                    200,
                    100,
                ),
                measurement(
                    ChaEventKind::TransactionInsert(transaction, ChaTransactionResult::Miss),
                    100,
                    100,
                ),
                measurement(
                    ChaEventKind::TransactionOccupancy(transaction, ChaTransactionResult::Hit),
                    400,
                    100,
                ),
                measurement(
                    ChaEventKind::TransactionOccupancy(transaction, ChaTransactionResult::Miss),
                    500,
                    100,
                ),
            ]);
        }

        for queue in [ChaRxcQueue::Irq, ChaRxcQueue::Prq] {
            measurements.extend([
                measurement(ChaEventKind::RxcClockticks(queue), 1_000, 100),
                measurement(ChaEventKind::RxcInsert(queue), 200, 100),
                measurement(ChaEventKind::RxcOccupancy(queue), 400, 100),
            ]);
        }

        measurements
    }

    fn test_scope() -> UncoreScope {
        UncoreScope {
            die_group_id: None,
            die_id: None,
            package_id: 0,
        }
    }
}
