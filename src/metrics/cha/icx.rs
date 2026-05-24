use std::collections::{BTreeMap, btree_map::Entry};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::arch::{Architecture, IntelServerCpuModel};
use crate::metal;
use crate::metal::msr::Msr;
use crate::metrics::cha::{
    CHA_COUNTER_COUNT, ChaCacheState, ChaEventKind, ChaEventMeasurement, ChaHaRequestLocality,
    ChaHaRequestMetrics, ChaMultiplexMode, ChaRequestOperation, ChaRequestQueueMetrics,
    ChaRequestSource, ChaScopeMetrics, ChaSfEvictionMetrics, ChaTransactionLabel,
    ChaTransactionMetrics, ChaTransactionResult, ChaTransactionResultMetrics, bytes_per_second,
    event_rate, llc_victim_metrics, required_measurement, scale_measurement_value,
};
use crate::metrics::uncore::skx::{
    SKX_UNCORE_COUNTER_WIDTH, UncoreScope, frequency_hz, mask_counter, measurement_round_count,
    queue_residency_seconds, ratio, uncore_leaders,
};

const COUNTER_ENABLE_BIT: u64 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u64 = 1 << 20;
const COUNTER_RESET_BIT: u64 = 1 << 17;
const ICX_CAPID_DEVICE_ID: u16 = 0x345b;
const ICX_CAPID6_OFFSET: u64 = 0x9c;
const ICX_CAPID7_OFFSET: u64 = 0xa0;
const ICX_CAPID_BITMAP_WIDTH: usize = 40;
const ICX_MAX_CHA_COUNT: usize = 42;
const ICX_UNIT_FREEZE: u64 = 0x10100;
const ICX_UNIT_FREEZE_AND_RESET: u64 = 0x10103;
const ICX_UNIT_UNFREEZE: u64 = 0x10000;

const ICX_CHA_MSR_PMON_BOX_CTL: [u64; ICX_MAX_CHA_COUNT] = [
    0x0e00, 0x0e0e, 0x0e1c, 0x0e2a, 0x0e38, 0x0e46, 0x0e54, 0x0e62, 0x0e70, 0x0e7e, 0x0e8c, 0x0e9a,
    0x0ea8, 0x0eb6, 0x0ec4, 0x0ed2, 0x0ee0, 0x0eee, 0x0f0a, 0x0f18, 0x0f26, 0x0f34, 0x0f42, 0x0f50,
    0x0f5e, 0x0f6c, 0x0f7a, 0x0f88, 0x0f96, 0x0fa4, 0x0fb2, 0x0fc0, 0x0fce, 0x0fdc, 0x0b60, 0x0b6e,
    0x0b7c, 0x0b8a, 0x0b98, 0x0ba6, 0x0bb4, 0x0bc2,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcxChaArchitecture {
    Icx,
}

impl IcxChaArchitecture {
    pub(crate) const fn model(self) -> IntelServerCpuModel {
        match self {
            Self::Icx => IntelServerCpuModel::IceLakeXeon,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Icx => "Ice Lake-SP",
        }
    }

    const fn clock_event(self) -> u8 {
        match self {
            Self::Icx => 0x00,
        }
    }

    const fn counter_offset(self, cha_id: usize, counter_index: usize) -> u64 {
        match self {
            Self::Icx => ICX_CHA_MSR_PMON_BOX_CTL[cha_id] + 8 + counter_index as u64,
        }
    }

    const fn control_offset(self, cha_id: usize, counter_index: usize) -> u64 {
        match self {
            Self::Icx => ICX_CHA_MSR_PMON_BOX_CTL[cha_id] + 1 + counter_index as u64,
        }
    }

    const fn filter_offset(self, cha_id: usize) -> u64 {
        match self {
            Self::Icx => ICX_CHA_MSR_PMON_BOX_CTL[cha_id] + 5,
        }
    }

    const fn unit_control_offset(self, cha_id: usize) -> u64 {
        match self {
            Self::Icx => ICX_CHA_MSR_PMON_BOX_CTL[cha_id],
        }
    }

    const fn unit_freeze(self) -> u64 {
        match self {
            Self::Icx => ICX_UNIT_FREEZE,
        }
    }

    const fn unit_unfreeze(self) -> u64 {
        match self {
            Self::Icx => ICX_UNIT_UNFREEZE,
        }
    }

    fn freeze_and_reset(self, msr: &Msr, cha_id: usize) -> Result<(), String> {
        match self {
            Self::Icx => msr.write(self.unit_control_offset(cha_id), ICX_UNIT_FREEZE_AND_RESET),
        }
    }

    fn event_groups(self) -> &'static [IcxChaEventGroup] {
        match self {
            Self::Icx => &ICX_CHA_EVENT_GROUPS,
        }
    }

    fn supported_transactions(self) -> &'static [IcxChaTransactionSpec] {
        match self {
            Self::Icx => &ICX_CHA_TRANSACTIONS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaEventSpec {
    event: u8,
    kind: IcxChaEventKind,
    umask: u8,
    umask_ext: u32,
}

impl IcxChaEventSpec {
    const fn new(kind: IcxChaEventKind, event: u8, umask: u8, umask_ext: u32) -> Self {
        Self {
            event,
            kind,
            umask,
            umask_ext,
        }
    }

    const fn clockticks(kind: ChaEventKind, event: u8) -> Self {
        Self::new(IcxChaEventKind::Exported(kind), event, 0x00, 0)
    }

    const fn transaction_clockticks(
        transaction: IcxChaTransaction,
        counter_kind: IcxChaCounterKind,
        event: u8,
    ) -> Self {
        Self::new(
            IcxChaEventKind::TransactionClockticks(transaction, counter_kind),
            event,
            0x00,
            0,
        )
    }

    const fn unused() -> Self {
        Self::new(IcxChaEventKind::Unused, 0x00, 0x00, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaEventGroup {
    events: [IcxChaEventSpec; CHA_COUNTER_COUNT],
}

impl IcxChaEventGroup {
    const fn frequency(architecture: IcxChaArchitecture) -> Self {
        Self {
            events: [
                IcxChaEventSpec::clockticks(
                    ChaEventKind::EvictionClockticks,
                    architecture.clock_event(),
                ),
                IcxChaEventSpec::unused(),
                IcxChaEventSpec::unused(),
                IcxChaEventSpec::unused(),
            ],
        }
    }

    const fn ha_requests() -> Self {
        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Local,
                        ChaRequestOperation::Read,
                    )),
                    0x50,
                    0x01,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Remote,
                        ChaRequestOperation::Read,
                    )),
                    0x50,
                    0x02,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::HaRequest(
                        ChaHaRequestLocality::Local,
                        ChaRequestOperation::Write,
                    )),
                    0x50,
                    0x04,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::HaRequest(
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

    const fn request_queue(architecture: IcxChaArchitecture, source: ChaRequestSource) -> Self {
        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::RequestQueueOccupancy(source)),
                    0x36,
                    icx_cha_request_source_umask(source),
                    0xc001ff,
                ),
                IcxChaEventSpec::clockticks(
                    ChaEventKind::RequestQueueClockticks(source),
                    architecture.clock_event(),
                ),
                IcxChaEventSpec::unused(),
                IcxChaEventSpec::unused(),
            ],
        }
    }

    const fn sf_evictions() -> Self {
        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::SfEviction(ChaCacheState::M)),
                    0x3d,
                    0x01,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::SfEviction(ChaCacheState::E)),
                    0x3d,
                    0x02,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::SfEviction(ChaCacheState::S)),
                    0x3d,
                    0x04,
                    0,
                ),
                IcxChaEventSpec::unused(),
            ],
        }
    }

    const fn llc_victims() -> Self {
        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::All)),
                    0x37,
                    0x0f,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::M)),
                    0x37,
                    0x01,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::E)),
                    0x37,
                    0x02,
                    0,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::S)),
                    0x37,
                    0x04,
                    0,
                ),
            ],
        }
    }

    const fn transaction(
        architecture: IcxChaArchitecture,
        transaction: IcxChaTransaction,
        counter_kind: IcxChaCounterKind,
    ) -> Self {
        let tor = transaction.tor_spec(counter_kind);

        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::TransactionOccupancy(transaction, counter_kind),
                    0x36,
                    tor.umask,
                    tor.umask_ext,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::TransactionInsert(transaction, counter_kind),
                    0x35,
                    tor.umask,
                    tor.umask_ext,
                ),
                IcxChaEventSpec::transaction_clockticks(
                    transaction,
                    counter_kind,
                    architecture.clock_event(),
                ),
                IcxChaEventSpec::unused(),
            ],
        }
    }

    const fn aggregate_transaction(
        architecture: IcxChaArchitecture,
        transaction: IcxChaTransaction,
    ) -> Self {
        let tor = transaction.aggregate_tor_spec();

        Self {
            events: [
                IcxChaEventSpec::new(
                    IcxChaEventKind::TransactionOccupancy(transaction, IcxChaCounterKind::All),
                    0x36,
                    tor.umask,
                    tor.umask_ext,
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::TransactionInsert(transaction, IcxChaCounterKind::All),
                    0x35,
                    tor.umask,
                    tor.umask_ext,
                ),
                IcxChaEventSpec::transaction_clockticks(
                    transaction,
                    IcxChaCounterKind::All,
                    architecture.clock_event(),
                ),
                IcxChaEventSpec::unused(),
            ],
        }
    }
}

const fn icx_cha_request_source_umask(source: ChaRequestSource) -> u8 {
    match source {
        ChaRequestSource::Ia => 0x01,
        ChaRequestSource::Io => 0x04,
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum IcxChaTransaction {
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

impl IcxChaTransaction {
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

    const fn tor_spec(self, counter_kind: IcxChaCounterKind) -> IcxChaTorSpec {
        match (self, counter_kind) {
            (Self::IaClFlush, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ClFlush,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IaDrd, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Drd,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IaItoM, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IaRfo, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Rfo,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IaSpecItoM, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::SpecItoM,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IaWbMtoI, result) => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::WbMtoI,
                IcxChaTorSource::Ia,
                result,
            )),
            (Self::IoClFlush, result) => IcxChaTorSpec::io(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ClFlush,
                IcxChaTorSource::Io,
                result,
            )),
            (Self::IoPciRdCur, result) => IcxChaTorSpec::io(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::PciRdCur,
                IcxChaTorSource::Io,
                result,
            )),
            (Self::IoItoM, result) => IcxChaTorSpec::io(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Io,
                result,
            )),
            (Self::IoItoMCacheNear, result) => IcxChaTorSpec::io(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoMCacheNear,
                IcxChaTorSource::Io,
                result,
            )),
            (Self::IoWbMtoI, result) => IcxChaTorSpec::io(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::WbMtoI,
                IcxChaTorSource::Io,
                result,
            )),
        }
    }

    const fn aggregate_tor_spec(self) -> IcxChaTorSpec {
        match self {
            Self::IaSpecItoM => IcxChaTorSpec::ia(IcxChaTorUmaskExt::new(
                IcxChaTorRequest::SpecItoM,
                IcxChaTorSource::Ia,
                IcxChaCounterKind::All,
            )),
            _ => self.tor_spec(IcxChaCounterKind::All),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcxChaTransactionResultMode {
    Aggregate,
    DirectHitMiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaTransactionSpec {
    kind: IcxChaTransaction,
    result_mode: IcxChaTransactionResultMode,
}

impl IcxChaTransactionSpec {
    const fn aggregate(kind: IcxChaTransaction) -> Self {
        Self {
            kind,
            result_mode: IcxChaTransactionResultMode::Aggregate,
        }
    }

    const fn direct_hit_miss(kind: IcxChaTransaction) -> Self {
        Self {
            kind,
            result_mode: IcxChaTransactionResultMode::DirectHitMiss,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaTorSpec {
    umask: u8,
    umask_ext: u32,
}

impl IcxChaTorSpec {
    const fn ia(umask_ext: IcxChaTorUmaskExt) -> Self {
        Self {
            umask: 0x01,
            umask_ext: umask_ext.value(),
        }
    }

    const fn io(umask_ext: IcxChaTorUmaskExt) -> Self {
        Self {
            umask: 0x04,
            umask_ext: umask_ext.value(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaTorUmaskExt {
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

impl IcxChaTorUmaskExt {
    const fn new(
        request: IcxChaTorRequest,
        source: IcxChaTorSource,
        result: IcxChaCounterKind,
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
enum IcxChaTorRequest {
    ClFlush,
    Drd,
    ItoM,
    ItoMCacheNear,
    PciRdCur,
    Rfo,
    SpecItoM,
    WbMtoI,
}

impl IcxChaTorRequest {
    const fn opcode(self, source: IcxChaTorSource) -> u16 {
        match (self, source) {
            (Self::ClFlush, IcxChaTorSource::Ia) => 0x118,
            (Self::ClFlush, IcxChaTorSource::Io) => 0x118,
            (Self::Drd, IcxChaTorSource::Ia) => 0x102,
            (Self::Drd, IcxChaTorSource::Io) => 0x102,
            (Self::ItoM, IcxChaTorSource::Ia) => 0x188,
            (Self::ItoM, IcxChaTorSource::Io) => 0x188,
            (Self::ItoMCacheNear, IcxChaTorSource::Ia) => 0x1a8,
            (Self::ItoMCacheNear, IcxChaTorSource::Io) => 0x1a8,
            (Self::PciRdCur, IcxChaTorSource::Ia) => 0x11e,
            (Self::PciRdCur, IcxChaTorSource::Io) => 0x11e,
            (Self::Rfo, IcxChaTorSource::Ia) => 0x100,
            (Self::Rfo, IcxChaTorSource::Io) => 0x100,
            (Self::SpecItoM, IcxChaTorSource::Ia) => 0x18a,
            (Self::SpecItoM, IcxChaTorSource::Io) => 0x18a,
            (Self::WbMtoI, IcxChaTorSource::Ia) => 0x184,
            (Self::WbMtoI, IcxChaTorSource::Io) => 0x184,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcxChaTorSource {
    Ia,
    Io,
}

impl IcxChaTorSource {
    const fn premorph_opcode(self) -> bool {
        match self {
            Self::Ia => true,
            Self::Io => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum IcxChaCounterKind {
    All,
    Hit,
    Miss,
}

impl IcxChaCounterKind {
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
enum IcxChaEventKind {
    Exported(ChaEventKind),
    TransactionClockticks(IcxChaTransaction, IcxChaCounterKind),
    TransactionInsert(IcxChaTransaction, IcxChaCounterKind),
    TransactionOccupancy(IcxChaTransaction, IcxChaCounterKind),
    Unused,
}

impl IcxChaEventKind {
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

const ICX_CHA_TRANSACTIONS: [IcxChaTransactionSpec; 11] = [
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IoPciRdCur),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IoItoM),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IoItoMCacheNear),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IoWbMtoI),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IaDrd),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IaRfo),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IaItoM),
    IcxChaTransactionSpec::aggregate(IcxChaTransaction::IaSpecItoM),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IaClFlush),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IaWbMtoI),
    IcxChaTransactionSpec::direct_hit_miss(IcxChaTransaction::IoClFlush),
];

const ICX_CHA_EVENT_GROUPS: [IcxChaEventGroup; 27] = [
    IcxChaEventGroup::frequency(IcxChaArchitecture::Icx),
    IcxChaEventGroup::ha_requests(),
    IcxChaEventGroup::request_queue(IcxChaArchitecture::Icx, ChaRequestSource::Ia),
    IcxChaEventGroup::request_queue(IcxChaArchitecture::Icx, ChaRequestSource::Io),
    IcxChaEventGroup::sf_evictions(),
    IcxChaEventGroup::llc_victims(),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoPciRdCur,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoPciRdCur,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoItoM,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoItoM,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoItoMCacheNear,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoItoMCacheNear,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoWbMtoI,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoWbMtoI,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaDrd,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaDrd,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaRfo,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaRfo,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaItoM,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaItoM,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::aggregate_transaction(IcxChaArchitecture::Icx, IcxChaTransaction::IaSpecItoM),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaClFlush,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaClFlush,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaWbMtoI,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IaWbMtoI,
        IcxChaCounterKind::Miss,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoClFlush,
        IcxChaCounterKind::Hit,
    ),
    IcxChaEventGroup::transaction(
        IcxChaArchitecture::Icx,
        IcxChaTransaction::IoClFlush,
        IcxChaCounterKind::Miss,
    ),
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct IcxChaMetrics {
    pub(crate) ha_requests: Vec<ChaHaRequestMetrics>,
    pub(crate) llc_victims: Vec<crate::metrics::cha::ChaLlcVictimMetrics>,
    pub(crate) request_queues: Vec<ChaRequestQueueMetrics>,
    pub(crate) scopes: Vec<ChaScopeMetrics>,
    pub(crate) sf_evictions: Vec<ChaSfEvictionMetrics>,
    pub(crate) transaction_results: Vec<ChaTransactionResultMetrics>,
    pub(crate) transactions: Vec<ChaTransactionMetrics>,
}

impl IcxChaMetrics {
    fn from_measurements(
        architecture: IcxChaArchitecture,
        measurements: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    ) -> Result<Self, String> {
        let mut ha_requests = Vec::new();
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
            llc_victims.extend(icx_llc_victim_metrics(scope, &scope_measurements)?);
            request_queues.extend(request_queue_metrics(scope, &scope_measurements)?);
            sf_evictions.extend(sf_eviction_metrics(scope, &scope_measurements)?);
            let transaction_scope_metrics =
                transaction_metrics(architecture, scope, &scope_measurements)?;
            transaction_results.extend(transaction_scope_metrics.results);
            transactions.extend(transaction_scope_metrics.totals);
        }

        Ok(Self {
            ha_requests,
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
struct IcxChaTransactionScopeMetrics {
    results: Vec<ChaTransactionResultMetrics>,
    totals: Vec<ChaTransactionMetrics>,
}

#[derive(Debug)]
pub struct IcxChaCollector {
    architecture: IcxChaArchitecture,
    multiplex_mode: ChaMultiplexMode,
    next_group: usize,
    next_partition_offset: usize,
    packages: Vec<IcxChaPackage>,
}

impl IcxChaCollector {
    pub fn new(architecture: &Architecture) -> Result<Self, String> {
        let model = architecture.intel_server_model();
        let architecture = IcxChaArchitecture::Icx;
        if model != architecture.model() {
            return Err(format!(
                "{} CHA collection is not supported for {model:?}",
                architecture.name()
            ));
        }

        let packages = discover_packages(architecture)?;
        probe_writable_msrs(&packages)?;

        Ok(Self {
            architecture,
            multiplex_mode: ChaMultiplexMode::default(),
            next_group: 0,
            next_partition_offset: 0,
            packages,
        })
    }

    pub fn set_multiplex_mode(&mut self, mode: ChaMultiplexMode) {
        if let Err(error) = self.validate_multiplex_mode(mode) {
            eprintln!(
                "ocellus: disabling {} CHA spatial multiplexing: {error}",
                self.architecture.name()
            );
            self.multiplex_mode = ChaMultiplexMode::Temporal;
            return;
        }

        self.multiplex_mode = mode;
    }

    pub async fn sample(&mut self, interval: Duration) -> Result<IcxChaMetrics, String> {
        if interval.is_zero() {
            return Err(format!(
                "{} CHA measure interval must be non-zero",
                self.architecture.name()
            ));
        }

        let mut measurements = IcxChaMeasurementAccumulator::new();
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

        IcxChaMetrics::from_measurements(
            self.architecture,
            measurements.into_measurements(self.architecture),
        )
    }

    fn rotate_schedule(&mut self, measured_slice_count: usize) {
        self.next_group = (self.next_group + self.multiplex_mode.partitions())
            % self.architecture.event_groups().len();
        self.next_partition_offset = self
            .next_partition_offset
            .wrapping_add(measured_slice_count);
    }

    fn schedule(&self, interval: Duration) -> Vec<IcxChaMeasurementSlice> {
        let event_groups = self.architecture.event_groups();
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

                slices.push(IcxChaMeasurementSlice {
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
        event_groups: &'static [IcxChaEventGroup],
        first_group_offset: usize,
        partitions: usize,
        group_count: usize,
    ) -> [Option<IcxChaEventGroup>; CHA_COUNTER_COUNT] {
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
struct IcxChaUnitReading {
    counters: [u64; CHA_COUNTER_COUNT],
}

#[derive(Clone, Copy, Debug)]
struct IcxChaUnit {
    architecture: IcxChaArchitecture,
    cpu: u32,
    id: usize,
}

impl IcxChaUnit {
    fn freeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_freeze())
    }

    fn freeze_and_reset(self) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;
        self.architecture.freeze_and_reset(&msr, self.id)
    }

    fn program(self, group: IcxChaEventGroup) -> Result<(), String> {
        let msr = Msr::open(self.cpu)?;

        msr.write(self.architecture.filter_offset(self.id), 0)?;
        for (counter_index, event) in group.events.into_iter().enumerate() {
            msr.write(
                self.architecture.control_offset(self.id, counter_index),
                counter_control(self.architecture, event),
            )?;
        }

        Ok(())
    }

    fn read(self) -> Result<IcxChaUnitReading, String> {
        Ok(IcxChaUnitReading {
            counters: [
                self.read_counter(0).map(mask_icx_cha_counter)?,
                self.read_counter(1).map(mask_icx_cha_counter)?,
                self.read_counter(2).map(mask_icx_cha_counter)?,
                self.read_counter(3).map(mask_icx_cha_counter)?,
            ],
        })
    }

    fn unfreeze(self) -> Result<(), String> {
        self.write_unit_control(self.architecture.unit_unfreeze())
    }

    fn read_counter(self, counter_index: usize) -> Result<u64, String> {
        Msr::open_readonly(self.cpu)?.read(self.architecture.counter_offset(self.id, counter_index))
    }

    fn write_unit_control(self, value: u64) -> Result<(), String> {
        Msr::open(self.cpu)?.write(self.architecture.unit_control_offset(self.id), value)
    }

    fn probe_writable(self) -> Result<(), String> {
        self.freeze_and_reset()?;
        self.unfreeze()
    }
}

#[derive(Debug)]
struct IcxChaPackage {
    scope: UncoreScope,
    units: Vec<IcxChaUnit>,
}

impl IcxChaPackage {
    fn new(scope: UncoreScope, units: Vec<IcxChaUnit>) -> Self {
        Self { scope, units }
    }

    fn cpu(&self) -> u32 {
        self.units.first().map(|unit| unit.cpu).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct IcxChaMeasurement {
    enabled: Duration,
    represented_unit_count: u64,
    running: Duration,
    unit_scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcxChaMeasurementSlice {
    duration: Duration,
    groups: [Option<IcxChaEventGroup>; CHA_COUNTER_COUNT],
    partition_offset: usize,
    partition_width: usize,
}

#[derive(Debug, Default)]
struct IcxChaMeasurementAccumulator {
    exported: BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>>,
    transaction_clockticks:
        BTreeMap<(UncoreScope, IcxChaTransaction, IcxChaCounterKind), ChaEventMeasurement>,
    transaction_inserts:
        BTreeMap<(UncoreScope, IcxChaTransaction, IcxChaCounterKind), ChaEventMeasurement>,
    transaction_occupancy:
        BTreeMap<(UncoreScope, IcxChaTransaction, IcxChaCounterKind), ChaEventMeasurement>,
}

impl IcxChaMeasurementAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        scope: UncoreScope,
        kind: IcxChaEventKind,
        value: u64,
        measurement: IcxChaMeasurement,
    ) {
        if matches!(kind, IcxChaEventKind::Unused) {
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
            IcxChaEventKind::Exported(kind) => add_measurement(
                self.exported.entry(scope).or_default().entry(kind),
                event_measurement,
            ),
            IcxChaEventKind::TransactionClockticks(transaction, counter_kind) => add_measurement(
                self.transaction_clockticks
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            IcxChaEventKind::TransactionInsert(transaction, counter_kind) => add_measurement(
                self.transaction_inserts
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            IcxChaEventKind::TransactionOccupancy(transaction, counter_kind) => add_measurement(
                self.transaction_occupancy
                    .entry((scope, transaction, counter_kind)),
                event_measurement,
            ),
            IcxChaEventKind::Unused => unreachable!(),
        }
    }

    fn into_measurements(
        mut self,
        architecture: IcxChaArchitecture,
    ) -> BTreeMap<UncoreScope, BTreeMap<ChaEventKind, ChaEventMeasurement>> {
        for transaction in architecture.supported_transactions() {
            match transaction.result_mode {
                IcxChaTransactionResultMode::Aggregate => {
                    self.export_counter_kind(
                        transaction.kind,
                        IcxChaCounterKind::All,
                        ChaTransactionResult::All,
                    );
                }
                IcxChaTransactionResultMode::DirectHitMiss => {
                    self.export_direct_hit_miss(transaction.kind)
                }
            }
        }

        self.exported
    }

    fn export_direct_hit_miss(&mut self, transaction: IcxChaTransaction) {
        self.export_counter_kind(
            transaction,
            IcxChaCounterKind::Hit,
            ChaTransactionResult::Hit,
        );
        self.export_counter_kind(
            transaction,
            IcxChaCounterKind::Miss,
            ChaTransactionResult::Miss,
        );
    }

    fn export_counter_kind(
        &mut self,
        transaction: IcxChaTransaction,
        counter_kind: IcxChaCounterKind,
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

pub struct IcxChaPrometheusMetrics {
    frequency_hz: Family<ChaScopeLabels, Gauge<f64, AtomicU64>>,
    ha_request_bandwidth_bytes_per_second:
        Family<ChaHaRequestBandwidthLabels, Gauge<f64, AtomicU64>>,
    ha_request_local_ratio: Family<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>,
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

impl std::fmt::Debug for IcxChaPrometheusMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("IcxChaPrometheusMetrics").finish()
    }
}

impl IcxChaPrometheusMetrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            frequency_hz: Family::<ChaScopeLabels, Gauge<f64, AtomicU64>>::default(),
            ha_request_bandwidth_bytes_per_second: Family::<
                ChaHaRequestBandwidthLabels,
                Gauge<f64, AtomicU64>,
            >::default(),
            ha_request_local_ratio:
                Family::<ChaHaRequestRatioLabels, Gauge<f64, AtomicU64>>::default(),
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

    pub fn update(&self, metrics: IcxChaMetrics) {
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
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
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
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
            operation: operation.label().to_string(),
            package: scope.package_id.to_string(),
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
            die: scope.die_id.to_string(),
            die_group: scope.die_group_id.to_string(),
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
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
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
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
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
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
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
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
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
            die: metric.scope.die_id.to_string(),
            die_group: metric.scope.die_group_id.to_string(),
            package: metric.scope.package_id.to_string(),
            result: metric.result.label().to_string(),
            transaction: metric.transaction.as_str().to_string(),
        }
    }
}

fn discover_packages(architecture: IcxChaArchitecture) -> Result<Vec<IcxChaPackage>, String> {
    let mut packages = Vec::new();

    for leader in uncore_leaders()? {
        packages.push(IcxChaPackage::new(
            leader.scope,
            discover_units(architecture, leader.cpu)?,
        ));
    }

    if packages.is_empty() {
        return Err(format!(
            "failed to discover any {} CHA packages",
            architecture.name()
        ));
    }

    Ok(packages)
}

fn discover_units(architecture: IcxChaArchitecture, cpu: u32) -> Result<Vec<IcxChaUnit>, String> {
    let cha_ids = icx_cha_ids()?;
    let msr = Msr::open_readonly(cpu)?;
    let mut units = Vec::new();

    for id in cha_ids {
        if msr.read(architecture.unit_control_offset(id)).is_ok()
            && msr.read(architecture.counter_offset(id, 0)).is_ok()
            && msr.read(architecture.control_offset(id, 0)).is_ok()
            && msr.read(architecture.filter_offset(id)).is_ok()
        {
            let unit = IcxChaUnit {
                architecture,
                cpu,
                id,
            };
            if let Err(error) = unit.probe_writable() {
                eprintln!(
                    "ocellus: skipping {} CHA {id} on CPU {cpu}: {error}",
                    architecture.name()
                );
                continue;
            }

            units.push(unit);
        }
    }

    if units.is_empty() {
        return Err(format!(
            "failed to discover any {} CHA units on CPU {cpu}",
            architecture.name()
        ));
    }

    Ok(units)
}

fn icx_cha_ids() -> Result<Vec<usize>, String> {
    let locations = metal::pci::find_intel_devices_matching_device_id(ICX_CAPID_DEVICE_ID)?;
    let location = *locations
        .first()
        .ok_or_else(|| "failed to find Ice Lake-SP CAPID PCI device".to_string())?;
    let device = metal::pci::PciDevice::open_readonly(location)?;
    let capid6 = device.read_u32(ICX_CAPID6_OFFSET)?;
    let capid7 = device.read_u32(ICX_CAPID7_OFFSET)?;

    icx_cha_ids_from_capid(capid6, capid7)
}

fn icx_cha_ids_from_capid(capid6: u32, capid7: u32) -> Result<Vec<usize>, String> {
    let bitmap = u64::from(capid6) | ((u64::from(capid7) & 0xff) << 32);
    let max_count = ICX_CAPID_BITMAP_WIDTH.min(ICX_CHA_MSR_PMON_BOX_CTL.len());
    let ids: Vec<_> = (0..max_count)
        .filter(|id| ((bitmap >> id) & 1) != 0)
        .collect();

    if ids.is_empty() {
        Err("Ice Lake-SP CAPID reports zero available CHAs".to_string())
    } else {
        Ok(ids)
    }
}

fn program_packages(
    packages: &[IcxChaPackage],
    slice: IcxChaMeasurementSlice,
) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze_and_reset()?;
        }
    }

    for package in packages {
        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = icx_cha_partition(unit_index, slice, package.units.len());
            if let Some(group) = slice.groups[partition] {
                unit.program(group)?;
            }
        }
    }

    Ok(())
}

fn read_packages(
    packages: &[IcxChaPackage],
    enabled: Duration,
    running: Duration,
    slice: IcxChaMeasurementSlice,
    measurements: &mut IcxChaMeasurementAccumulator,
) -> Result<(), String> {
    for package in packages {
        let mut partition_counters = [[0_u64; CHA_COUNTER_COUNT]; CHA_COUNTER_COUNT];
        let mut partition_unit_counts = [0_u64; CHA_COUNTER_COUNT];

        for (unit_index, unit) in package.units.iter().enumerate() {
            let partition = icx_cha_partition(unit_index, slice, package.units.len());
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
                    IcxChaMeasurement {
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

fn freeze_packages(packages: &[IcxChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.freeze()?;
        }
    }

    Ok(())
}

fn unfreeze_packages(packages: &[IcxChaPackage]) -> Result<(), String> {
    for package in packages {
        for unit in &package.units {
            unit.unfreeze()?;
        }
    }

    Ok(())
}

fn probe_writable_msrs(packages: &[IcxChaPackage]) -> Result<(), String> {
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
    architecture: IcxChaArchitecture,
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<IcxChaTransactionScopeMetrics, String> {
    let mut results = Vec::new();
    let mut totals = Vec::new();

    for transaction_spec in architecture.supported_transactions() {
        let transaction = transaction_spec.kind;
        match transaction_spec.result_mode {
            IcxChaTransactionResultMode::Aggregate => {
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
            IcxChaTransactionResultMode::DirectHitMiss => {
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

    Ok(IcxChaTransactionScopeMetrics { results, totals })
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

fn sf_eviction_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
) -> Result<Vec<ChaSfEvictionMetrics>, String> {
    let mut metrics = Vec::new();

    for state in [
        crate::metrics::cha::ChaCacheState::M,
        crate::metrics::cha::ChaCacheState::E,
        crate::metrics::cha::ChaCacheState::S,
    ] {
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

fn icx_llc_victim_metrics(
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

fn transaction_result_metrics(
    scope: UncoreScope,
    measurements: &BTreeMap<ChaEventKind, ChaEventMeasurement>,
    transaction: IcxChaTransaction,
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

fn counter_control(architecture: IcxChaArchitecture, event: IcxChaEventSpec) -> u64 {
    match architecture {
        IcxChaArchitecture::Icx => {
            event_control_bits(event.event, event.umask) | (u64::from(event.umask_ext) << 32)
        }
    }
}

fn event_control_bits(event: u8, umask: u8) -> u64 {
    u64::from(event)
        | (u64::from(umask) << 8)
        | COUNTER_RESET_BIT
        | COUNTER_OVERFLOW_ENABLE_BIT
        | COUNTER_ENABLE_BIT
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

fn icx_cha_partition(unit_index: usize, slice: IcxChaMeasurementSlice, unit_count: usize) -> usize {
    let rotated_unit_index = (unit_index + slice.partition_offset) % unit_count;
    rotated_unit_index * slice.partition_width / unit_count
}

fn mask_icx_cha_counter(counter: u64) -> u64 {
    mask_counter(counter, SKX_UNCORE_COUNTER_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_icx_umask_ext_events_with_legacy_counter_control_bits() {
        let umask_ext = IcxChaTorUmaskExt::new(
            IcxChaTorRequest::PciRdCur,
            IcxChaTorSource::Io,
            IcxChaCounterKind::Hit,
        )
        .value();
        let event = IcxChaEventSpec::new(
            IcxChaEventKind::TransactionInsert(
                IcxChaTransaction::IoPciRdCur,
                IcxChaCounterKind::Hit,
            ),
            0x35,
            0x04,
            umask_ext,
        );

        assert_eq!(
            counter_control(IcxChaArchitecture::Icx, event),
            event_control_bits(0x35, 0x04) | (u64::from(umask_ext) << 32)
        );
    }

    #[test]
    fn builds_icx_io_itom_umask_ext_from_bitfields() {
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Ia,
                IcxChaCounterKind::Hit
            )
            .value(),
            0xcc47fd
        );
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Ia,
                IcxChaCounterKind::Miss
            )
            .value(),
            0xcc47fe
        );
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Io,
                IcxChaCounterKind::Hit
            )
            .value(),
            0xcc43fd
        );
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::ItoM,
                IcxChaTorSource::Io,
                IcxChaCounterKind::Miss
            )
            .value(),
            0xcc43fe
        );
    }

    #[test]
    fn builds_icx_io_rfo_umask_ext_from_bitfields() {
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Rfo,
                IcxChaTorSource::Io,
                IcxChaCounterKind::Hit
            )
            .value(),
            0xc803fd
        );
        assert_eq!(
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Rfo,
                IcxChaTorSource::Io,
                IcxChaCounterKind::Miss
            )
            .value(),
            0xc803fe
        );
    }

    #[test]
    fn uses_documented_icx_transaction_umask_ext_values() {
        let cases = [
            (
                IcxChaTransaction::IaClFlush,
                IcxChaCounterKind::Hit,
                0xc8c7fd,
            ),
            (
                IcxChaTransaction::IaClFlush,
                IcxChaCounterKind::Miss,
                0xc8c7fe,
            ),
            (IcxChaTransaction::IaDrd, IcxChaCounterKind::Hit, 0xc817fd),
            (IcxChaTransaction::IaDrd, IcxChaCounterKind::Miss, 0xc817fe),
            (IcxChaTransaction::IaItoM, IcxChaCounterKind::Hit, 0xcc47fd),
            (IcxChaTransaction::IaItoM, IcxChaCounterKind::Miss, 0xcc47fe),
            (IcxChaTransaction::IaRfo, IcxChaCounterKind::Hit, 0xc807fd),
            (IcxChaTransaction::IaRfo, IcxChaCounterKind::Miss, 0xc807fe),
            (
                IcxChaTransaction::IaSpecItoM,
                IcxChaCounterKind::All,
                0xcc57ff,
            ),
            (
                IcxChaTransaction::IaWbMtoI,
                IcxChaCounterKind::Hit,
                0xcc27fd,
            ),
            (
                IcxChaTransaction::IaWbMtoI,
                IcxChaCounterKind::Miss,
                0xcc27fe,
            ),
            (
                IcxChaTransaction::IoClFlush,
                IcxChaCounterKind::Hit,
                0xc8c3fd,
            ),
            (
                IcxChaTransaction::IoClFlush,
                IcxChaCounterKind::Miss,
                0xc8c3fe,
            ),
            (
                IcxChaTransaction::IoPciRdCur,
                IcxChaCounterKind::Hit,
                0xc8f3fd,
            ),
            (
                IcxChaTransaction::IoPciRdCur,
                IcxChaCounterKind::Miss,
                0xc8f3fe,
            ),
            (IcxChaTransaction::IoItoM, IcxChaCounterKind::Hit, 0xcc43fd),
            (IcxChaTransaction::IoItoM, IcxChaCounterKind::Miss, 0xcc43fe),
            (
                IcxChaTransaction::IoItoMCacheNear,
                IcxChaCounterKind::Hit,
                0xcd43fd,
            ),
            (
                IcxChaTransaction::IoItoMCacheNear,
                IcxChaCounterKind::Miss,
                0xcd43fe,
            ),
            (
                IcxChaTransaction::IoWbMtoI,
                IcxChaCounterKind::Hit,
                0xcc23fd,
            ),
            (
                IcxChaTransaction::IoWbMtoI,
                IcxChaCounterKind::Miss,
                0xcc23fe,
            ),
        ];

        for (transaction, result, umask_ext) in cases {
            assert_eq!(transaction.tor_spec(result).umask_ext, umask_ext);
        }
    }

    #[test]
    fn uses_documented_icx_llc_victim_events() {
        let group = IcxChaEventGroup::llc_victims();

        assert_eq!(
            group.events,
            [
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::All)),
                    0x37,
                    0x0f,
                    0
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::M)),
                    0x37,
                    0x01,
                    0
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::E)),
                    0x37,
                    0x02,
                    0
                ),
                IcxChaEventSpec::new(
                    IcxChaEventKind::Exported(ChaEventKind::LlcVictim(ChaCacheState::S)),
                    0x37,
                    0x04,
                    0
                ),
            ]
        );
    }

    #[test]
    fn discovers_icx_cha_ids_from_capid_bitmap() {
        assert_eq!(
            icx_cha_ids_from_capid(0x0000_0f0f, 0).unwrap(),
            vec![0, 1, 2, 3, 8, 9, 10, 11]
        );
        assert_eq!(
            icx_cha_ids_from_capid(0, 0xff).unwrap(),
            vec![32, 33, 34, 35, 36, 37, 38, 39]
        );
        assert!(icx_cha_ids_from_capid(0, 0).is_err());
    }

    #[test]
    fn icx_ia_drd_uses_direct_hit_events() {
        let group = IcxChaEventGroup::transaction(
            IcxChaArchitecture::Icx,
            IcxChaTransaction::IaDrd,
            IcxChaCounterKind::Hit,
        );

        assert_eq!(group.events[0].event, 0x36);
        assert_eq!(group.events[0].umask, 0x01);
        assert_eq!(
            group.events[0].umask_ext,
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Drd,
                IcxChaTorSource::Ia,
                IcxChaCounterKind::Hit
            )
            .value()
        );
        assert_eq!(group.events[1].event, 0x35);
        assert_eq!(group.events[1].umask, 0x01);
        assert_eq!(
            group.events[1].umask_ext,
            IcxChaTorUmaskExt::new(
                IcxChaTorRequest::Drd,
                IcxChaTorSource::Ia,
                IcxChaCounterKind::Hit
            )
            .value()
        );
    }

    #[test]
    fn exports_direct_hit_and_miss_for_icx_transactions() {
        let mut accumulator = IcxChaMeasurementAccumulator::new();
        let scope = UncoreScope {
            die_group_id: 0,
            die_id: 0,
            package_id: 0,
        };
        let measurement = IcxChaMeasurement {
            enabled: Duration::from_millis(100),
            represented_unit_count: 4,
            running: Duration::from_millis(100),
            unit_scale: 1.0,
        };

        accumulator.add(
            scope,
            IcxChaEventKind::TransactionInsert(IcxChaTransaction::IaDrd, IcxChaCounterKind::Hit),
            30,
            measurement,
        );
        accumulator.add(
            scope,
            IcxChaEventKind::TransactionOccupancy(IcxChaTransaction::IaDrd, IcxChaCounterKind::Hit),
            70,
            measurement,
        );
        accumulator.add(
            scope,
            IcxChaEventKind::TransactionClockticks(
                IcxChaTransaction::IaDrd,
                IcxChaCounterKind::Hit,
            ),
            100,
            measurement,
        );
        accumulator.add(
            scope,
            IcxChaEventKind::TransactionInsert(IcxChaTransaction::IaDrd, IcxChaCounterKind::Miss),
            10,
            measurement,
        );
        accumulator.add(
            scope,
            IcxChaEventKind::TransactionOccupancy(
                IcxChaTransaction::IaDrd,
                IcxChaCounterKind::Miss,
            ),
            20,
            measurement,
        );
        accumulator.add(
            scope,
            IcxChaEventKind::TransactionClockticks(
                IcxChaTransaction::IaDrd,
                IcxChaCounterKind::Miss,
            ),
            100,
            measurement,
        );

        let measurements = accumulator.into_measurements(IcxChaArchitecture::Icx);
        let scope_measurements = measurements.get(&scope).unwrap();
        assert_eq!(
            scope_measurements
                .get(&ChaEventKind::TransactionInsert(
                    IcxChaTransaction::IaDrd.label(),
                    ChaTransactionResult::Hit,
                ))
                .unwrap()
                .value,
            30
        );
        assert_eq!(
            scope_measurements
                .get(&ChaEventKind::TransactionOccupancy(
                    IcxChaTransaction::IaDrd.label(),
                    ChaTransactionResult::Hit,
                ))
                .unwrap()
                .value,
            70
        );
        assert_eq!(
            scope_measurements
                .get(&ChaEventKind::TransactionInsert(
                    IcxChaTransaction::IaDrd.label(),
                    ChaTransactionResult::Miss,
                ))
                .unwrap()
                .value,
            10
        );
        assert_eq!(
            scope_measurements
                .get(&ChaEventKind::TransactionClockticks(
                    IcxChaTransaction::IaDrd.label(),
                    ChaTransactionResult::Hit,
                ))
                .unwrap()
                .value,
            100
        );
    }

    #[test]
    fn partitions_cha_units_spatially() {
        let slice = IcxChaMeasurementSlice {
            duration: Duration::from_millis(1),
            groups: [None; CHA_COUNTER_COUNT],
            partition_offset: 1,
            partition_width: 4,
        };

        assert_eq!(icx_cha_partition(0, slice, 8), 0);
        assert_eq!(icx_cha_partition(1, slice, 8), 1);
        assert_eq!(icx_cha_partition(6, slice, 8), 3);
        assert_eq!(icx_cha_partition(7, slice, 8), 0);
    }
}
