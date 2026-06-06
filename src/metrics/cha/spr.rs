use std::collections::{BTreeMap, btree_map::Entry};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metal;
use crate::metal::msr::Msr;
use crate::metrics::cha::{
    CHA_COUNTER_COUNT, ChaCacheState, ChaEventKind, ChaEventMeasurement, ChaHaRequestLocality,
    ChaHaRequestMetrics, ChaLlcLookupMetrics, ChaLookupOperation, ChaMultiplexMode,
    ChaRequestOperation, ChaRequestQueueMetrics, ChaRequestSource, ChaScopeMetrics,
    ChaSfEvictionMetrics, ChaTransactionLabel, ChaTransactionMetrics, ChaTransactionResult,
    ChaTransactionResultMetrics, bytes_per_second, event_rate, linux_uncore_unit_ids,
    llc_victim_metrics, pci_location_for_cpu, required_measurement, scale_measurement_value,
};
use crate::metrics::common::topology_label;
use crate::metrics::uncore::skx::{
    SKX_UNCORE_COUNTER_WIDTH, UncoreScope, frequency_hz, mask_counter, measurement_round_count,
    queue_residency_seconds, ratio, uncore_leaders,
};

const SPR_CHA_COUNT_DEVICE_ID: u16 = 0x325b;
const SPR_CHA_COUNT_LOW_OFFSET: u64 = 0x9c;
const SPR_CHA_COUNT_HIGH_OFFSET: u64 = 0xa0;
const SPR_EMR_CHA_NAME: &str = "SPR/EMR";
const SPR_CHA_CLOCK_EVENT: u8 = 0x01;
const SPR_MAX_CHA_COUNT: usize = 128;
const SPR_MSR_UNC_CBO_CONFIG: u64 = 0x2ffe;
const SPR_UNIT_COUNTER_RESET_BIT: u64 = 1 << 9;
const SPR_UNIT_CONTROL_RESET_BIT: u64 = 1 << 8;
const SPR_UNIT_FREEZE_BIT: u64 = 1 << 0;

const fn spr_cha_counter_offset(cha_id: usize, counter_index: usize) -> u64 {
    0x2008 + 0x10 * cha_id as u64 + counter_index as u64
}

const fn spr_cha_control_offset(cha_id: usize, counter_index: usize) -> u64 {
    0x2002 + 0x10 * cha_id as u64 + counter_index as u64
}

const fn spr_cha_filter_offset(cha_id: usize) -> u64 {
    0x200e + 0x10 * cha_id as u64
}

const fn spr_cha_unit_control_offset(cha_id: usize) -> u64 {
    0x2000 + 0x10 * cha_id as u64
}

fn spr_cha_freeze_and_reset(msr: &Msr, cha_id: usize) -> Result<(), String> {
    msr.write(
        spr_cha_unit_control_offset(cha_id),
        SPR_UNIT_FREEZE_BIT | SPR_UNIT_CONTROL_RESET_BIT,
    )?;
    msr.write(
        spr_cha_unit_control_offset(cha_id),
        SPR_UNIT_FREEZE_BIT | SPR_UNIT_COUNTER_RESET_BIT,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaEventSpec {
    event: u8,
    kind: SprChaEventKind,
    umask: u8,
    umask_ext: u32,
}

impl SprChaEventSpec {
    const fn new(kind: SprChaEventKind, event: u8, umask: u8, umask_ext: u32) -> Self {
        Self {
            event,
            kind,
            umask,
            umask_ext,
        }
    }

    const fn clockticks(kind: ChaEventKind, event: u8) -> Self {
        Self::new(SprChaEventKind::Exported(kind), event, 0x00, 0)
    }

    const fn transaction_clockticks(
        transaction: SprChaTransaction,
        counter_kind: SprChaCounterKind,
        event: u8,
    ) -> Self {
        Self::new(
            SprChaEventKind::TransactionClockticks(transaction, counter_kind),
            event,
            0x00,
            0,
        )
    }

    const fn unused() -> Self {
        Self::new(SprChaEventKind::Unused, 0x00, 0x00, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaEventGroup {
    events: [SprChaEventSpec; CHA_COUNTER_COUNT],
}

impl SprChaEventGroup {
    const fn frequency() -> Self {
        Self {
            events: [
                SprChaEventSpec::clockticks(ChaEventKind::EvictionClockticks, SPR_CHA_CLOCK_EVENT),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
            ],
        }
    }

    const fn ha_requests() -> Self {
        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Local,
                        ChaRequestOperation::Read,
                    )),
                    0x50,
                    0x01,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Remote,
                        ChaRequestOperation::Read,
                    )),
                    0x50,
                    0x02,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Local,
                        ChaRequestOperation::Write,
                    )),
                    0x50,
                    0x04,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Remote,
                        ChaRequestOperation::Write,
                    )),
                    0x50,
                    0x08,
                    0,
                ),
            ],
        }
    }

    const fn llc_lookup(state: ChaCacheState) -> Self {
        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        state,
                        ChaLookupOperation::Read,
                    )),
                    0x34,
                    spr_cha_llc_lookup_state_umask(state),
                    0x1bc1,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        state,
                        ChaLookupOperation::Rfo,
                    )),
                    0x34,
                    spr_cha_llc_lookup_state_umask(state),
                    0x1bc8,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        state,
                        ChaLookupOperation::RemoteSnoop,
                    )),
                    0x34,
                    spr_cha_llc_lookup_state_umask(state),
                    0x1c19,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        state,
                        ChaLookupOperation::Any,
                    )),
                    0x34,
                    spr_cha_llc_lookup_state_umask(state),
                    0x20,
                ),
            ],
        }
    }

    const fn request_queue(source: ChaRequestSource) -> Self {
        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::RequestQueueOccupancy(source)),
                    0x36,
                    spr_cha_request_source_umask(source),
                    0xc001ff,
                ),
                SprChaEventSpec::clockticks(
                    ChaEventKind::RequestQueueClockticks(source),
                    SPR_CHA_CLOCK_EVENT,
                ),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
            ],
        }
    }

    const fn sf_evictions() -> Self {
        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::SfEviction(ChaCacheState::All)),
                    0x35,
                    0x02,
                    0,
                ),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
            ],
        }
    }

    const fn llc_victims() -> Self {
        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::All)),
                    0x37,
                    0x0f,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::M)),
                    0x37,
                    0x01,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::E)),
                    0x37,
                    0x02,
                    0,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::S)),
                    0x37,
                    0x04,
                    0,
                ),
            ],
        }
    }

    const fn transaction(transaction: SprChaTransaction, counter_kind: SprChaCounterKind) -> Self {
        let tor = transaction.tor_spec(counter_kind);

        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::TransactionOccupancy(transaction, counter_kind),
                    0x36,
                    tor.umask,
                    tor.umask_ext,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::TransactionInsert(transaction, counter_kind),
                    0x35,
                    tor.umask,
                    tor.umask_ext,
                ),
                SprChaEventSpec::transaction_clockticks(
                    transaction,
                    counter_kind,
                    SPR_CHA_CLOCK_EVENT,
                ),
                SprChaEventSpec::unused(),
            ],
        }
    }

    const fn aggregate_transaction(transaction: SprChaTransaction) -> Self {
        let tor = transaction.aggregate_tor_spec();

        Self {
            events: [
                SprChaEventSpec::new(
                    SprChaEventKind::TransactionOccupancy(transaction, SprChaCounterKind::All),
                    0x36,
                    tor.umask,
                    tor.umask_ext,
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::TransactionInsert(transaction, SprChaCounterKind::All),
                    0x35,
                    tor.umask,
                    tor.umask_ext,
                ),
                SprChaEventSpec::transaction_clockticks(
                    transaction,
                    SprChaCounterKind::All,
                    SPR_CHA_CLOCK_EVENT,
                ),
                SprChaEventSpec::unused(),
            ],
        }
    }
}

const fn spr_cha_llc_lookup_state_umask(state: ChaCacheState) -> u8 {
    match state {
        ChaCacheState::I => 0x01,
        ChaCacheState::SfS => 0x02,
        ChaCacheState::SfE => 0x04,
        ChaCacheState::SfH => 0x08,
        ChaCacheState::S => 0x10,
        ChaCacheState::E => 0x20,
        ChaCacheState::M => 0x40,
        ChaCacheState::F => 0x80,
        ChaCacheState::All | ChaCacheState::SfM => 0x00,
    }
}

const fn spr_cha_request_source_umask(source: ChaRequestSource) -> u8 {
    match source {
        ChaRequestSource::Ia => 0x01,
        ChaRequestSource::Io => 0x04,
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum SprChaTransaction {
    IaClFlush,
    IaDrd,
    IaItoM,
    IaRfo,
    IaSpecItoM,
    IaWbMtoI,
    IoClFlush,
    IoItoM,
    IoItoMCacheNear,
    IoPciRdCur,
    IoWbMtoI,
}

impl SprChaTransaction {
    const fn label(self) -> ChaTransactionLabel {
        match self {
            Self::IaClFlush => ChaTransactionLabel::new("ia_clflush"),
            Self::IaDrd => ChaTransactionLabel::new("ia_drd"),
            Self::IaItoM => ChaTransactionLabel::new("ia_itom"),
            Self::IaRfo => ChaTransactionLabel::new("ia_rfo"),
            Self::IaSpecItoM => ChaTransactionLabel::new("ia_specitom"),
            Self::IaWbMtoI => ChaTransactionLabel::new("ia_wbmtoi"),
            Self::IoClFlush => ChaTransactionLabel::new("io_clflush"),
            Self::IoItoM => ChaTransactionLabel::new("io_itom"),
            Self::IoItoMCacheNear => ChaTransactionLabel::new("io_itomcachenear"),
            Self::IoPciRdCur => ChaTransactionLabel::new("io_pcirdcur"),
            Self::IoWbMtoI => ChaTransactionLabel::new("io_wbmtoi"),
        }
    }

    const fn tor_spec(self, counter_kind: SprChaCounterKind) -> SprChaTorSpec {
        match (self, counter_kind) {
            (Self::IaClFlush, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::ClFlush,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IaDrd, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::Drd,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IaItoM, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::ItoM,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IaRfo, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::Rfo,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IaSpecItoM, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::SpecItoM,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IaWbMtoI, result) => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::WbMtoI,
                SprChaTorSource::Ia,
                result,
            )),
            (Self::IoClFlush, result) => SprChaTorSpec::io(SprChaTorUmaskExt::new(
                SprChaTorRequest::ClFlush,
                SprChaTorSource::Io,
                result,
            )),
            (Self::IoPciRdCur, result) => SprChaTorSpec::io(SprChaTorUmaskExt::new(
                SprChaTorRequest::PciRdCur,
                SprChaTorSource::Io,
                result,
            )),
            (Self::IoItoM, result) => SprChaTorSpec::io(SprChaTorUmaskExt::new(
                SprChaTorRequest::ItoM,
                SprChaTorSource::Io,
                result,
            )),
            (Self::IoItoMCacheNear, result) => SprChaTorSpec::io(SprChaTorUmaskExt::new(
                SprChaTorRequest::ItoMCacheNear,
                SprChaTorSource::Io,
                result,
            )),
            (Self::IoWbMtoI, result) => SprChaTorSpec::io(SprChaTorUmaskExt::new(
                SprChaTorRequest::WbMtoI,
                SprChaTorSource::Io,
                result,
            )),
        }
    }

    const fn aggregate_tor_spec(self) -> SprChaTorSpec {
        match self {
            Self::IaSpecItoM => SprChaTorSpec::ia(SprChaTorUmaskExt::new(
                SprChaTorRequest::SpecItoM,
                SprChaTorSource::Ia,
                SprChaCounterKind::All,
            )),
            _ => self.tor_spec(SprChaCounterKind::All),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprChaTransactionResultMode {
    Aggregate,
    DirectHitMiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaTransactionSpec {
    kind: SprChaTransaction,
    result_mode: SprChaTransactionResultMode,
}

impl SprChaTransactionSpec {
    const fn aggregate(kind: SprChaTransaction) -> Self {
        Self {
            kind,
            result_mode: SprChaTransactionResultMode::Aggregate,
        }
    }

    const fn direct_hit_miss(kind: SprChaTransaction) -> Self {
        Self {
            kind,
            result_mode: SprChaTransactionResultMode::DirectHitMiss,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaTorSpec {
    umask: u8,
    umask_ext: u32,
}

impl SprChaTorSpec {
    const fn ia(umask_ext: SprChaTorUmaskExt) -> Self {
        Self {
            umask: 0x01,
            umask_ext: umask_ext.value(),
        }
    }

    const fn io(umask_ext: SprChaTorUmaskExt) -> Self {
        Self {
            umask: 0x04,
            umask_ext: umask_ext.value(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaTorUmaskExt {
    ddr: bool,
    hbm: bool,
    hit: bool,
    isoc: bool,
    loc: bool,
    match_opc: bool,
    miss: bool,
    mmcfg: bool,
    mmio: bool,
    nc: bool,
    nm: bool,
    not_nm: bool,
    opc: u16,
    pmm: bool,
    premorph_opc: bool,
    rem: bool,
}

impl SprChaTorUmaskExt {
    const fn new(
        request: SprChaTorRequest,
        source: SprChaTorSource,
        result: SprChaCounterKind,
    ) -> Self {
        Self {
            ddr: true,
            hbm: true,
            hit: result.hit(),
            isoc: false,
            loc: true,
            match_opc: true,
            miss: result.miss(),
            mmcfg: true,
            mmio: true,
            nc: false,
            nm: true,
            not_nm: true,
            opc: request.opcode(source),
            pmm: true,
            premorph_opc: source.premorph_opcode(),
            rem: true,
        }
    }

    const fn value(self) -> u32 {
        self.shifted_value(57, self.isoc)
            | self.shifted_value(56, self.nc)
            | self.shifted_value(55, self.not_nm)
            | self.shifted_value(54, self.nm)
            | (((self.opc as u32) & 0x07ff) << (43 - 32))
            | self.shifted_value(42, self.premorph_opc)
            | self.shifted_value(41, self.match_opc)
            | self.shifted_value(40, self.loc)
            | self.shifted_value(39, self.rem)
            | self.shifted_value(38, self.mmio)
            | self.shifted_value(37, self.mmcfg)
            | self.shifted_value(36, self.hbm)
            | self.shifted_value(35, self.pmm)
            | self.shifted_value(34, self.ddr)
            | self.shifted_value(33, self.miss)
            | self.shifted_value(32, self.hit)
    }

    const fn shifted_value(self, bit: u8, enabled: bool) -> u32 {
        if enabled { 1 << (bit - 32) } else { 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprChaTorRequest {
    ClFlush,
    Drd,
    ItoM,
    ItoMCacheNear,
    PciRdCur,
    Rfo,
    SpecItoM,
    WbMtoI,
}

impl SprChaTorRequest {
    const fn opcode(self, source: SprChaTorSource) -> u16 {
        match (self, source) {
            (Self::ClFlush, SprChaTorSource::Ia) => 0x118,
            (Self::ClFlush, SprChaTorSource::Io) => 0x118,
            (Self::Drd, SprChaTorSource::Ia) => 0x102,
            (Self::Drd, SprChaTorSource::Io) => 0x102,
            (Self::ItoM, SprChaTorSource::Ia) => 0x188,
            (Self::ItoM, SprChaTorSource::Io) => 0x188,
            (Self::ItoMCacheNear, SprChaTorSource::Ia) => 0x1a8,
            (Self::ItoMCacheNear, SprChaTorSource::Io) => 0x1a8,
            (Self::PciRdCur, SprChaTorSource::Ia) => 0x11e,
            (Self::PciRdCur, SprChaTorSource::Io) => 0x11e,
            (Self::Rfo, SprChaTorSource::Ia) => 0x100,
            (Self::Rfo, SprChaTorSource::Io) => 0x100,
            (Self::SpecItoM, SprChaTorSource::Ia) => 0x18a,
            (Self::SpecItoM, SprChaTorSource::Io) => 0x18a,
            (Self::WbMtoI, SprChaTorSource::Ia) => 0x184,
            (Self::WbMtoI, SprChaTorSource::Io) => 0x184,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprChaTorSource {
    Ia,
    Io,
}

impl SprChaTorSource {
    const fn premorph_opcode(self) -> bool {
        match self {
            Self::Ia => true,
            Self::Io => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum SprChaCounterKind {
    All,
    Hit,
    Miss,
}

impl SprChaCounterKind {
    const fn hit(self) -> bool {
        match self {
            Self::All => true,
            Self::Hit => true,
            Self::Miss => false,
        }
    }

    const fn miss(self) -> bool {
        match self {
            Self::All => true,
            Self::Hit => false,
            Self::Miss => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprChaEventKind {
    Exported(ChaEventKind),
    TransactionClockticks(SprChaTransaction, SprChaCounterKind),
    TransactionInsert(SprChaTransaction, SprChaCounterKind),
    TransactionOccupancy(SprChaTransaction, SprChaCounterKind),
    Unused,
}

impl SprChaEventKind {
    fn is_clockticks(self) -> bool {
        match self {
            Self::Exported(kind) => kind.is_clockticks(),
            Self::TransactionClockticks(_, _) => true,
            Self::TransactionInsert(_, _) | Self::TransactionOccupancy(_, _) | Self::Unused => {
                false
            }
        }
    }
}

const SPR_EMR_CHA_TRANSACTIONS: [SprChaTransactionSpec; 11] = [
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IoPciRdCur),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IoItoM),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IoItoMCacheNear),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IoWbMtoI),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IaDrd),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IaRfo),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IaItoM),
    SprChaTransactionSpec::aggregate(SprChaTransaction::IaSpecItoM),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IaClFlush),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IaWbMtoI),
    SprChaTransactionSpec::direct_hit_miss(SprChaTransaction::IoClFlush),
];

const SPR_EMR_CHA_EVENT_GROUPS: [SprChaEventGroup; 35] = [
    SprChaEventGroup::frequency(),
    SprChaEventGroup::ha_requests(),
    SprChaEventGroup::llc_lookup(ChaCacheState::SfS),
    SprChaEventGroup::llc_lookup(ChaCacheState::SfE),
    SprChaEventGroup::llc_lookup(ChaCacheState::SfH),
    SprChaEventGroup::llc_lookup(ChaCacheState::I),
    SprChaEventGroup::llc_lookup(ChaCacheState::S),
    SprChaEventGroup::llc_lookup(ChaCacheState::E),
    SprChaEventGroup::llc_lookup(ChaCacheState::M),
    SprChaEventGroup::llc_lookup(ChaCacheState::F),
    SprChaEventGroup::request_queue(ChaRequestSource::Ia),
    SprChaEventGroup::request_queue(ChaRequestSource::Io),
    SprChaEventGroup::sf_evictions(),
    SprChaEventGroup::llc_victims(),
    SprChaEventGroup::transaction(SprChaTransaction::IoPciRdCur, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IoPciRdCur, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IoItoM, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IoItoM, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IoItoMCacheNear, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IoItoMCacheNear, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IoWbMtoI, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IoWbMtoI, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IaDrd, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IaDrd, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IaRfo, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IaRfo, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IaItoM, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IaItoM, SprChaCounterKind::Miss),
    SprChaEventGroup::aggregate_transaction(SprChaTransaction::IaSpecItoM),
    SprChaEventGroup::transaction(SprChaTransaction::IaClFlush, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IaClFlush, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IaWbMtoI, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IaWbMtoI, SprChaCounterKind::Miss),
    SprChaEventGroup::transaction(SprChaTransaction::IoClFlush, SprChaCounterKind::Hit),
    SprChaEventGroup::transaction(SprChaTransaction::IoClFlush, SprChaCounterKind::Miss),
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct SprChaMetrics {
    pub(crate) ha_requests: Vec<ChaHaRequestMetrics>,
    pub(crate) llc_lookups: Vec<ChaLlcLookupMetrics>,
    pub(crate) llc_victims: Vec<crate::metrics::cha::ChaLlcVictimMetrics>,
    pub(crate) request_queues: Vec<ChaRequestQueueMetrics>,
    pub(crate) scopes: Vec<ChaScopeMetrics>,
    pub(crate) sf_evictions: Vec<ChaSfEvictionMetrics>,
    pub(crate) transaction_results: Vec<ChaTransactionResultMetrics>,
    pub(crate) transactions: Vec<ChaTransactionMetrics>,
}

impl SprChaMetrics {
    fn from_measurements(
        measurements: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut ha_requests = Vec::new();
        let mut llc_lookups = Vec::new();
        let mut llc_victims = Vec::new();
        let mut request_queues = Vec::new();
        let mut scopes = Vec::with_capacity(measurements.len());
        let mut sf_evictions = Vec::new();
        let mut transaction_results = Vec::new();
        let mut transactions = Vec::new();

        for (scope, scope_measurements) in measurements {
            let clockticks =
                required_measurement(&scope_measurements, ChaEventKind::EvictionClockticks)?;
            scopes.push(ChaScopeMetrics {
                frequency_hz: frequency_hz(clockticks.value, clockticks.running),
                scope,
            });

            ha_requests.push(ha_request_metrics(scope, &scope_measurements)?);
            llc_lookups.extend(spr_llc_lookup_metrics(scope, &scope_measurements)?);
            llc_victims.extend(spr_llc_victim_metrics(scope, &scope_measurements)?);
            request_queues.extend(request_queue_metrics(scope, &scope_measurements)?);
            sf_evictions.extend(spr_sf_eviction_metrics(scope, &scope_measurements)?);
            let transaction_scope_metrics = transaction_metrics(scope, &scope_measurements)?;
            transaction_results.extend(transaction_scope_metrics.results);
            transactions.extend(transaction_scope_metrics.totals);
        }

        Ok(Self {
            ha_requests,
            llc_lookups,
            llc_victims,
            request_queues,
            scopes,
            sf_evictions,
            transaction_results,
            transactions,
        })
    }
}

#[derive(Debug)]
struct SprChaTransactionScopeMetrics {
    results: Vec<ChaTransactionResultMetrics>,
    totals: Vec<ChaTransactionMetrics>,
}

#[derive(Debug)]
pub struct SprChaCollector {
    multiplex_mode: ChaMultiplexMode,
    next_group: usize,
    next_partition_offset: usize,
    packages: Vec<SprChaPackage>,
}

impl SprChaCollector {
    pub fn new() -> Result<Self, String> {
        let packages = discover_packages()?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages,
        })
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        if let Err(error) = self.validate_multiplex_mode(mode) {
            eprintln!("ocellus: disabling {SPR_EMR_CHA_NAME} CHA spatial multiplexing: {error}");
            self.multiplex_mode = ChaMultiplexMode::Temporal;
            return;
        }

        self.multiplex_mode = mode;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<SprChaMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{SPR_EMR_CHA_NAME} CHA measure interval must be non-zero"
            ));
        }

        let mut measurements = SprChaMeasurementAccumulator::new();
        let packages = &self.packages;

        let slices = self.schedule(interval);
        let measured_slice_count = slices.len();

        for slice in slices {
            program_packages(packages, slice)?;

            let started_at = Instant::now();
            unfreeze_packages(packages)?;
            tokio::time::sleep(slice.duration).await;
            freeze_packages(packages)?;

            read_packages(
                packages,
                interval,
                started_at.elapsed(),
                slice,
                &mut measurements,
            )?;
        }

        self.rotate_schedule(measured_slice_count);

        SprChaMetrics::from_measurements(measurements.into_measurements())
    }

    fn rotate_schedule(&mut self, measured_slice_count: usize) {
        self.next_group =
            (self.next_group + self.multiplex_mode.partitions()) % SPR_EMR_CHA_EVENT_GROUPS.len();
        self.next_partition_offset = self
            .next_partition_offset
            .wrapping_add(measured_slice_count);
    }

    fn schedule(&self, interval: Duration) -> Vec<SprChaMeasurementSlice> {
        let event_groups = &SPR_EMR_CHA_EVENT_GROUPS;
        let group_count = event_groups.len();
        let partitions = self.multiplex_mode.partitions();
        let slice_count_per_round = group_count.div_ceil(partitions);
        let round_count = measurement_round_count(interval, slice_count_per_round);
        let slice_count = slice_count_per_round * round_count;
        let slice_duration = interval.div_f64(slice_count as f64);
        let mut slices = Vec::with_capacity(slice_count);

        for round in 0..round_count {
            for slice_index in 0..slice_count_per_round {
                let first_group_offset = slice_index * partitions;
                let groups =
                    self.slice_groups(event_groups, first_group_offset, partitions, group_count);

                slices.push(SprChaMeasurementSlice {
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
        event_groups: &'static [SprChaEventGroup],
        first_group_offset: usize,
        partitions: usize,
        group_count: usize,
    ) -> [Option<SprChaEventGroup>; CHA_COUNTER_COUNT] {
        let mut groups = [None; CHA_COUNTER_COUNT];

        for (partition, group) in groups.iter_mut().enumerate().take(partitions) {
            let group_offset = first_group_offset + partition;
            if group_offset < group_count {
                let group_index = (self.next_group + group_offset) % group_count;
                *group = Some(event_groups[group_index]);
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

#[derive(Clone, Copy, Debug)]
struct SprChaUnitReading {
    counters: [u64; CHA_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct SprChaUnit {
    cpu: u32,
    id: usize,
}

impl SprChaUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(SPR_UNIT_FREEZE_BIT)
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        spr_cha_freeze_and_reset(&msr, self.id)
    }

    fn program(self, group: SprChaEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;

        msr.write(spr_cha_filter_offset(self.id), 0)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(
                spr_cha_control_offset(self.id, counter_index),
                counter_control(event),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<SprChaUnitReading, String> {
        Ok(SprChaUnitReading {
            counters: [
                self.read_counter(0).map(mask_spr_cha_counter)?,
                self.read_counter(1).map(mask_spr_cha_counter)?,
                self.read_counter(2).map(mask_spr_cha_counter)?,
                self.read_counter(3).map(mask_spr_cha_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(0)
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(spr_cha_counter_offset(self.id, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(spr_cha_unit_control_offset(self.id), value)
    }

    fn probe_writable(self) -> Result<(), String> {
        self.freeze_and_reset()?;
        self.unfreeze()
    }
}

#[derive(Debug)]
struct SprChaPackage {
    scope: UncoreScope,
    units: Vec<SprChaUnit>,
}

impl SprChaPackage {
    fn new(scope: UncoreScope, units: Vec<SprChaUnit>) -> Self {
        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct SprChaMeasurement {
    enabled: Duration,
    represented_unit_count: u64,
    running: Duration,
    unit_scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SprChaMeasurementSlice {
    duration: Duration,
    groups: [Option<SprChaEventGroup>; CHA_COUNTER_COUNT],
    partition_offset: usize,
    partition_width: usize,
}

#[derive(Debug, Default)]
struct SprChaMeasurementAccumulator {
    exported: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    transaction_clockticks:
        BTreeMap<(UncoreScope, SprChaTransaction, SprChaCounterKind), ChaEventMeasurement>,
    transaction_inserts:
        BTreeMap<(UncoreScope, SprChaTransaction, SprChaCounterKind), ChaEventMeasurement>,
    transaction_occupancy:
        BTreeMap<(UncoreScope, SprChaTransaction, SprChaCounterKind), ChaEventMeasurement>,
}

impl SprChaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: UncoreScope,
        kind: SprChaEventKind,
        value: u64,
        measurement: SprChaMeasurement,
    ) {
        if matches!(kind, SprChaEventKind::Unused) {
            return;
        }

        let scaled_value = if kind.is_clockticks() {
            value
        } else {
            (value as f64 * measurement.unit_scale) as u64
        };
        let event_measurement = ChaEventMeasurement {
            enabled: measurement.enabled,
            represented_unit_count: measurement.represented_unit_count,
            running: measurement.running,
            value: scaled_value,
        };

        match kind {
            SprChaEventKind::Exported(kind) => add_measurement(
                self.exported.entry(scope).or_default().entry(kind),
                event_measurement,
            ),
            SprChaEventKind::TransactionClockticks(transaction, counter_kind) => add_measurement(
                self.transaction_clockticks
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            SprChaEventKind::TransactionInsert(transaction, counter_kind) => add_measurement(
                self.transaction_inserts
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            SprChaEventKind::TransactionOccupancy(transaction, counter_kind) => add_measurement(
                self.transaction_occupancy
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            SprChaEventKind::Unused => unreachable!(),
        }
    }

    fn into_measurements(
        mut self,
    ) -> BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>> {
        for transaction in SPR_EMR_CHA_TRANSACTIONS {
            match transaction.result_mode {
                SprChaTransactionResultMode::Aggregate => {
                    self.export_counter_kind(
                        transaction.kind,
                        SprChaCounterKind::All,
                        ChaTransactionResult::All,
                    );
                }
                SprChaTransactionResultMode::DirectHitMiss => {
                    self.export_direct_hit_miss(transaction.kind)
                }
            }
        }

        self.exported
    }

    fn export_direct_hit_miss(&mut self, transaction: SprChaTransaction) {
        self.export_counter_kind(
            transaction,
            SprChaCounterKind::Hit,
            ChaTransactionResult::Hit,
        );
        self.export_counter_kind(
            transaction,
            SprChaCounterKind::Miss,
            ChaTransactionResult::Miss,
        );
    }

    fn export_counter_kind(
        &mut self,
        transaction: SprChaTransaction,
        counter_kind: SprChaCounterKind,
        result: ChaTransactionResult,
    ) {
        let mut scopes = Vec::new();
        for &(scope, event_transaction, event_counter_kind) in self.transaction_inserts.keys() {
            if event_transaction == transaction && event_counter_kind == counter_kind {
                scopes.push(scope);
            }
        }

        for scope in scopes {
            let Some(clockticks) = self
                .transaction_clockticks
                .get(&(scope, transaction, counter_kind))
                .copied()
            else {
                continue;
            };
            let Some(inserts) = self
                .transaction_inserts
                .get(&(scope, transaction, counter_kind))
                .copied()
            else {
                continue;
            };
            let Some(occupancy) = self
                .transaction_occupancy
                .get(&(scope, transaction, counter_kind))
                .copied()
            else {
                continue;
            };
            let measurements = self.exported.entry(scope).or_default();

            measurements.insert(
                ChaEventKind::TransactionClockticks(transaction.label(), result),
                clockticks,
            );
            measurements.insert(
                ChaEventKind::TransactionInsert(transaction.label(), result),
                inserts,
            );
            measurements.insert(
                ChaEventKind::TransactionOccupancy(transaction.label(), result),
                occupancy,
            );
        }
    }
}

pub struct SprChaPrometheusMetrics {
    frequency_hz: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    ha_request_bandwidth_bytes_per_second:
        Family<ChaHaRequestBandwidthLabels, Gauge<f64, AtomicU64>>,
    ha_request_local_ratio: Family<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>,
    llc_lookup_bytes_per_second: Family<ChaLlcLookupLabels, Gauge<f64, AtomicU64>>,
    llc_victims_per_second: Family<ChaStateLabels, Gauge<f64, AtomicU64>>,
    request_queue_occupancy_entries: Family<ChaRequestQueueLabels, Gauge<f64, AtomicU64>>,
    sf_eviction_bytes_per_second: Family<ChaSfEvictionLabels, Gauge<f64, AtomicU64>>,
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

impl std::fmt::Debug for SprChaPrometheusMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SprChaPrometheusMetrics").finish()
    }
}

impl SprChaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            frequency_hz: Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_request_bandwidth_bytes_per_second: Family::<
                ChaHaRequestBandwidthLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            ha_request_local_ratio:
                Family::<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>::default(),
            llc_lookup_bytes_per_second:
                Family::<ChaLlcLookupLabels, Gauge<f64, AtomicU64>>::default(),
            llc_victims_per_second: Family::<ChaStateLabels, Gauge<f64, AtomicU64>>::default(),
            request_queue_occupancy_entries:
                Family::<ChaRequestQueueLabels, Gauge<f64, AtomicU64>>::default(),
            sf_eviction_bytes_per_second:
                Family::<ChaSfEvictionLabels, Gauge<f64, AtomicU64>>::default(),
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
            "ocellus_cha_frequency_hz",
            "Interval-derived CHA clock frequency in hertz",
            metrics.frequency_hz.clone(),
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
            "ocellus_cha_request_queue_occupancy_entries",
            "Average CHA request queue occupancy in entries",
            metrics.request_queue_occupancy_entries.clone(),
        );
        registry.register(
            "ocellus_cha_sf_eviction_bytes_per_second",
            "Interval-derived CHA snoop filter eviction bandwidth in bytes per second",
            metrics.sf_eviction_bytes_per_second.clone(),
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

    pub fn update(&self, metrics: SprChaMetrics) {
        for scope in metrics.scopes {
            self.frequency_hz
                .get_or_create(&ChaScopeLabels::from_scope(scope.scope))
                .set(scope.frequency_hz);
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

        for metric in metrics.request_queues {
            self.request_queue_occupancy_entries
                .get_or_create(&ChaRequestQueueLabels::from_metric(metric))
                .set(metric.occupancy_entries);
        }

        for metric in metrics.llc_victims {
            self.llc_victims_per_second
                .get_or_create(&ChaStateLabels::from_llc_victim(metric))
                .set(metric.per_second);
        }

        for metric in metrics.sf_evictions {
            self.sf_eviction_bytes_per_second
                .get_or_create(&ChaSfEvictionLabels::from_metric(metric))
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
    fn from_llc_victim(metric: crate::metrics::cha::ChaLlcVictimMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
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
struct ChaSfEvictionLabels {
    die: String,
    die_group: String,
    package: String,
    state: String,
}

impl ChaSfEvictionLabels {
    fn from_metric(metric: ChaSfEvictionMetrics) -> Self {
        Self {
            die: topology_label(metric.scope.die_id),
            die_group: topology_label(metric.scope.die_group_id),
            package: metric.scope.package_id.to_string(),
            state: metric.state.label().to_string(),
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

fn discover_packages() -> Result<Vec<SprChaPackage>, String> {
    let mut packages = Vec::new();

    for leader in uncore_leaders()? {
        packages.push(SprChaPackage::new(
            leader.scope,
            discover_units(leader.cpu)?,
        ));
    }

    if packages.is_empty() {
        return Err(format!(
            "failed to discover any {SPR_EMR_CHA_NAME} CHA packages"
        ));
    }

    Ok(packages)
}

fn discover_units(cpu: u32) -> Result<Vec<SprChaUnit>, String> {
    let ids = spr_cha_ids(cpu)?;
    let msr = Msr::open_readonly(cpu)?;
    let mut units = Vec::new();

    for id in ids {
        if msr.read(spr_cha_unit_control_offset(id)).is_ok()
            && msr.read(spr_cha_counter_offset(id, 0)).is_ok()
            && msr.read(spr_cha_control_offset(id, 0)).is_ok()
            && msr.read(spr_cha_filter_offset(id)).is_ok()
        {
            let unit = SprChaUnit { cpu, id };
            if let Err(error) = unit.probe_writable() {
                eprintln!("ocellus: skipping {SPR_EMR_CHA_NAME} CHA {id} on CPU {cpu}: {error}");
                continue;
            }

            units.push(unit);
        }
    }

    if units.is_empty() {
        return Err(format!(
            "failed to discover any {SPR_EMR_CHA_NAME} CHA units on CPU {cpu}"
        ));
    }

    Ok(units)
}

fn spr_cha_ids(cpu: u32) -> Result<Vec<usize>, String> {
    match linux_uncore_unit_ids(&["uncore_cha_"], SPR_MAX_CHA_COUNT) {
        Ok(ids) => Ok(ids),
        Err(error) => {
            eprintln!("ocellus: falling back to {SPR_EMR_CHA_NAME} PCI CHA count: {error}");
            let count = spr_cha_count(cpu)?.min(SPR_MAX_CHA_COUNT);
            Ok((0..count).collect())
        }
    }
}

fn spr_cha_count(cpu: u32) -> Result<usize, String> {
    match spr_cha_count_from_msr(cpu).or_else(|_| spr_cha_count_from_pci(cpu)) {
        Ok(count) if count > 0 => Ok(count),
        Ok(_) | Err(_) => {
            let msr = Msr::open_readonly(cpu)?;
            let mut count = 0;

            for id in 0..SPR_MAX_CHA_COUNT {
                if msr.read(spr_cha_unit_control_offset(id)).is_ok() {
                    count += 1;
                } else if count > 0 {
                    break;
                }
            }

            if count == 0 {
                Err(format!("failed to discover {SPR_EMR_CHA_NAME} CHA count"))
            } else {
                Ok(count)
            }
        }
    }
}

fn spr_cha_count_from_msr(cpu: u32) -> Result<usize, String> {
    let count = Msr::open_readonly(cpu)?.read(SPR_MSR_UNC_CBO_CONFIG)? as usize;
    if count == 0 || count > SPR_MAX_CHA_COUNT {
        Err(format!(
            "{SPR_EMR_CHA_NAME} MSR 0x{SPR_MSR_UNC_CBO_CONFIG:x} reports invalid CHA count {count}"
        ))
    } else {
        Ok(count)
    }
}

fn spr_cha_count_from_pci(cpu: u32) -> Result<usize, String> {
    let locations = metal::pci::find_intel_devices_matching_device_id(SPR_CHA_COUNT_DEVICE_ID)?;
    let location = pci_location_for_cpu(cpu, &locations, &format!("{SPR_EMR_CHA_NAME} CHA count"))?;
    let device = metal::pci::PciDevice::open_readonly(location)?;
    let low = device.read_u32(SPR_CHA_COUNT_LOW_OFFSET)?;
    let high = device.read_u32(SPR_CHA_COUNT_HIGH_OFFSET)?;

    Ok((low.count_ones() + high.count_ones()) as usize)
}

fn program_packages(
    packages: &[SprChaPackage],
    slice: SprChaMeasurementSlice,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze_and_reset()?;
        }
    }

    for package in packages {
        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = spr_cha_partition(unit_index, slice, package.units.len());
            if let Some(group) = slice.groups[partition] {
                unit.program(group)?;
            }
        }
    }

    Ok(())
}

fn read_packages(
    packages: &[SprChaPackage],
    enabled: Duration,
    running: Duration,
    slice: SprChaMeasurementSlice,
    measurements: &mut SprChaMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let mut partition_counters = [[0_u64; CHA_COUNTER_COUNT]; CHA_COUNTER_COUNT];
        let mut partition_unit_counts = [0_u64; CHA_COUNTER_COUNT];

        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = spr_cha_partition(unit_index, slice, package.units.len());
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
                    SprChaMeasurement {
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

fn freeze_packages(packages: &[SprChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn unfreeze_packages(packages: &[SprChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn probe_writable_msrs(packages: &[SprChaPackage]) -> Result<(), String> {
    for package in packages {
        Msr::open(package.cpu())?;
    }

    Ok(())
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

fn transaction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<SprChaTransactionScopeMetrics, String> {
    let mut results = Vec::new();
    let mut totals = Vec::new();

    for transaction_spec in SPR_EMR_CHA_TRANSACTIONS {
        let transaction = transaction_spec.kind;
        match transaction_spec.result_mode {
            SprChaTransactionResultMode::Aggregate => {
                let aggregate = transaction_result_metrics(
                    scope,
                    measurements,
                    transaction,
                    ChaTransactionResult::All,
                )?;

                totals.push(ChaTransactionMetrics {
                    bandwidth_bytes_per_second: aggregate.bandwidth_bytes_per_second,
                    hit_rate: 0.0,
                    latency_seconds: aggregate.latency_seconds,
                    scope,
                    transaction: transaction.label(),
                });
                results.push(aggregate);
            }
            SprChaTransactionResultMode::DirectHitMiss => {
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
                    ChaEventKind::TransactionInsert(
                        transaction.label(),
                        ChaTransactionResult::Miss,
                    ),
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
        }
    }

    Ok(SprChaTransactionScopeMetrics { results, totals })
}

fn request_queue_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaRequestQueueMetrics>, String> {
    let mut metrics = Vec::new();

    for source in [ChaRequestSource::Ia, ChaRequestSource::Io] {
        let occupancy =
            required_measurement(measurements, ChaEventKind::RequestQueueOccupancy(source))?;
        let clockticks =
            required_measurement(measurements, ChaEventKind::RequestQueueClockticks(source))?;
        let occupancy_count = scale_measurement_value(occupancy);
        let clocktick_count = scale_measurement_value(clockticks);

        metrics.push(ChaRequestQueueMetrics {
            occupancy_entries: ratio(occupancy_count, clocktick_count),
            scope,
            source,
        });
    }

    Ok(metrics)
}

fn spr_llc_lookup_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaLlcLookupMetrics>, String> {
    let mut metrics = Vec::new();

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
        for operation in [
            ChaLookupOperation::Read,
            ChaLookupOperation::Rfo,
            ChaLookupOperation::RemoteSnoop,
            ChaLookupOperation::Any,
        ] {
            metrics.push(ChaLlcLookupMetrics {
                bytes_per_second: bytes_per_second(required_measurement(
                    measurements,
                    ChaEventKind::LlcLookup(state, operation),
                )?),
                operation,
                scope,
                state,
            });
        }
    }

    Ok(metrics)
}

fn spr_llc_victim_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<crate::metrics::cha::ChaLlcVictimMetrics>, String> {
    llc_victim_metrics(
        scope,
        measurements,
        &[
            ChaCacheState::All,
            ChaCacheState::M,
            ChaCacheState::E,
            ChaCacheState::S,
        ],
    )
}

fn spr_sf_eviction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaSfEvictionMetrics>, String> {
    Ok(vec![ChaSfEvictionMetrics {
        bytes_per_second: bytes_per_second(required_measurement(
            measurements,
            ChaEventKind::SfEviction(ChaCacheState::All),
        )?),
        scope,
        state: ChaCacheState::All,
    }])
}

fn transaction_result_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    transaction: SprChaTransaction,
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

fn counter_control(event: SprChaEventSpec) -> u64 {
    u64::from(event.event) | (u64::from(event.umask) << 8) | (u64::from(event.umask_ext) << 32)
}

fn add_measurement<K: Ord>(
    entry: Entry<'_, K, ChaEventMeasurement>,
    measurement: ChaEventMeasurement,
) {
    match entry {
        Entry::Vacant(entry) => {
            entry.insert(measurement);
        }
        Entry::Occupied(mut entry) => {
            entry.get_mut().add(
                measurement.value,
                measurement.running,
                measurement.represented_unit_count,
            );
        }
    }
}

fn spr_cha_partition(unit_index: usize, slice: SprChaMeasurementSlice, unit_count: usize) -> usize {
    let rotated_unit_index = (unit_index + slice.partition_offset) % unit_count;
    rotated_unit_index * slice.partition_width / unit_count
}

fn mask_spr_cha_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spr_umask_ext_events_without_legacy_counter_control_bits() {
        let umask_ext = SprChaTorUmaskExt::new(
            SprChaTorRequest::PciRdCur,
            SprChaTorSource::Io,
            SprChaCounterKind::Hit,
        )
        .value();
        let event = SprChaEventSpec::new(
            SprChaEventKind::TransactionInsert(
                SprChaTransaction::IoPciRdCur,
                SprChaCounterKind::Hit,
            ),
            0x35,
            0x04,
            umask_ext,
        );

        assert_eq!(
            counter_control(event),
            0x35 | (0x04 << 8) | (u64::from(umask_ext) << 32)
        );
    }

    #[test]
    fn builds_spr_transaction_umask_ext_from_bitfields() {
        assert_eq!(
            SprChaTorUmaskExt::new(
                SprChaTorRequest::Drd,
                SprChaTorSource::Ia,
                SprChaCounterKind::Hit
            )
            .value(),
            0xc817fd
        );
        assert_eq!(
            SprChaTorUmaskExt::new(
                SprChaTorRequest::Rfo,
                SprChaTorSource::Ia,
                SprChaCounterKind::Miss
            )
            .value(),
            0xc807fe
        );
        assert_eq!(
            SprChaTorUmaskExt::new(
                SprChaTorRequest::ItoM,
                SprChaTorSource::Io,
                SprChaCounterKind::Hit
            )
            .value(),
            0xcc43fd
        );
        assert_eq!(
            SprChaTorUmaskExt::new(
                SprChaTorRequest::ItoMCacheNear,
                SprChaTorSource::Io,
                SprChaCounterKind::Miss
            )
            .value(),
            0xcd43fe
        );
    }

    #[test]
    fn uses_documented_spr_transaction_umask_ext_values() {
        let cases = [
            (
                SprChaTransaction::IaClFlush,
                SprChaCounterKind::Hit,
                0xc8c7fd,
            ),
            (
                SprChaTransaction::IaClFlush,
                SprChaCounterKind::Miss,
                0xc8c7fe,
            ),
            (SprChaTransaction::IaDrd, SprChaCounterKind::Hit, 0xc817fd),
            (SprChaTransaction::IaDrd, SprChaCounterKind::Miss, 0xc817fe),
            (SprChaTransaction::IaItoM, SprChaCounterKind::Hit, 0xcc47fd),
            (SprChaTransaction::IaItoM, SprChaCounterKind::Miss, 0xcc47fe),
            (SprChaTransaction::IaRfo, SprChaCounterKind::Hit, 0xc807fd),
            (SprChaTransaction::IaRfo, SprChaCounterKind::Miss, 0xc807fe),
            (
                SprChaTransaction::IaSpecItoM,
                SprChaCounterKind::All,
                0xcc57ff,
            ),
            (
                SprChaTransaction::IaWbMtoI,
                SprChaCounterKind::Hit,
                0xcc27fd,
            ),
            (
                SprChaTransaction::IaWbMtoI,
                SprChaCounterKind::Miss,
                0xcc27fe,
            ),
            (
                SprChaTransaction::IoClFlush,
                SprChaCounterKind::Hit,
                0xc8c3fd,
            ),
            (
                SprChaTransaction::IoClFlush,
                SprChaCounterKind::Miss,
                0xc8c3fe,
            ),
            (
                SprChaTransaction::IoPciRdCur,
                SprChaCounterKind::Hit,
                0xc8f3fd,
            ),
            (
                SprChaTransaction::IoPciRdCur,
                SprChaCounterKind::Miss,
                0xc8f3fe,
            ),
            (SprChaTransaction::IoItoM, SprChaCounterKind::Hit, 0xcc43fd),
            (SprChaTransaction::IoItoM, SprChaCounterKind::Miss, 0xcc43fe),
            (
                SprChaTransaction::IoItoMCacheNear,
                SprChaCounterKind::Hit,
                0xcd43fd,
            ),
            (
                SprChaTransaction::IoItoMCacheNear,
                SprChaCounterKind::Miss,
                0xcd43fe,
            ),
            (
                SprChaTransaction::IoWbMtoI,
                SprChaCounterKind::Hit,
                0xcc23fd,
            ),
            (
                SprChaTransaction::IoWbMtoI,
                SprChaCounterKind::Miss,
                0xcc23fe,
            ),
        ];

        for (transaction, result, umask_ext) in cases {
            assert_eq!(transaction.tor_spec(result).umask_ext, umask_ext);
        }
    }

    #[test]
    fn uses_documented_spr_llc_victim_events() {
        let group = SprChaEventGroup::llc_victims();

        assert_eq!(
            group.events,
            [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::All)),
                    0x37,
                    0x0f,
                    0
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::M)),
                    0x37,
                    0x01,
                    0
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::E)),
                    0x37,
                    0x02,
                    0
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::S)),
                    0x37,
                    0x04,
                    0
                ),
            ]
        );
    }

    #[test]
    fn uses_documented_spr_llc_lookup_events() {
        let group = SprChaEventGroup::llc_lookup(ChaCacheState::F);

        assert_eq!(
            group.events,
            [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        ChaCacheState::F,
                        ChaLookupOperation::Read
                    )),
                    0x34,
                    0x80,
                    0x1bc1
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        ChaCacheState::F,
                        ChaLookupOperation::Rfo
                    )),
                    0x34,
                    0x80,
                    0x1bc8
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        ChaCacheState::F,
                        ChaLookupOperation::RemoteSnoop
                    )),
                    0x34,
                    0x80,
                    0x1c19
                ),
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::LlcLookup(
                        ChaCacheState::F,
                        ChaLookupOperation::Any
                    )),
                    0x34,
                    0x80,
                    0x20
                ),
            ]
        );
    }

    #[test]
    fn uses_documented_spr_sf_llc_eviction_event() {
        let group = SprChaEventGroup::sf_evictions();

        assert_eq!(
            group.events,
            [
                SprChaEventSpec::new(
                    SprChaEventKind::Exported(ChaEventKind::SfEviction(ChaCacheState::All)),
                    0x35,
                    0x02,
                    0
                ),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
                SprChaEventSpec::unused(),
            ]
        );
    }

    #[test]
    fn uses_counter_zero_for_spr_cha_tor_occupancy() {
        let group =
            SprChaEventGroup::transaction(SprChaTransaction::IoPciRdCur, SprChaCounterKind::Hit);

        assert_eq!(group.events[0].event, 0x36);
        assert_eq!(group.events[1].event, 0x35);
    }

    #[test]
    fn partitions_cha_units_spatially() {
        let slice = SprChaMeasurementSlice {
            duration: Duration::from_millis(1),
            groups: [None; CHA_COUNTER_COUNT],
            partition_offset: 1,
            partition_width: 4,
        };

        assert_eq!(spr_cha_partition(0, slice, 8), 0);
        assert_eq!(spr_cha_partition(1, slice, 8), 1);
        assert_eq!(spr_cha_partition(6, slice, 8), 3);
        assert_eq!(spr_cha_partition(7, slice, 8), 0);
    }
}
