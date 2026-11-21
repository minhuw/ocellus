//! Sandy Bridge-EP and Ivy Bridge-EP encodings of PMU config requests for
//! the CBo (`cha`) and IMC boxes. The models lay out the CBo filters
//! differently: Sandy Bridge-EP has one filter register, Ivy Bridge-EP a
//! FILTER0 and a FILTER1 like Haswell-EP.
//!
//! [SNB] is Intel's Xeon E5-2600 uncore performance monitoring guide
//! (327043-001), [IVB] the Xeon E5-2600 v2 and E7 v2 one (329468-001); the
//! JSON is the perfmon file of each model that `counters` comes from.

mod counters;

use crate::metrics::cha::ChaCacheState;
use crate::metrics::cha::snb::snb_llc_lookup_state_bits;
use crate::pmu_config::spec::{
    Alias, BoxSpec, Encoding, EventRequest, Filter0Request, Filter1Request, GroupProgram,
    GroupRequest, Number, Param, Token, check_counter, check_event_count, event_encoding, join,
};
use crate::pmu_config::types::{BoxType, ChaFilters, CounterSlot, EventLabels, ProgrammedCounter};

/// Cn_MSR_PMON_CTL ([SNB] Table 2-10, [IVB] Table 2-14): ev_sel [7:0],
/// umask [15:8], tid_en 19, en 22, and bits 21:20 "Reserved; SW must write
/// to 0", which configs leave 0. MC_CHy_PCI_PMON_CTL ([SNB] Table 2-61,
/// [IVB] Table 2-75) has no tid_en and the same enable bit; configs leave
/// its overflow enable (bit 20 on Ivy Bridge-EP, reserved on Sandy
/// Bridge-EP) 0.
const COUNTER_TID_ENABLE: u64 = 1 << 19;
const COUNTER_ENABLE: u64 = 1 << 22;
/// MC_CHy_PCI_PMON_FIXED_CTL.en ([SNB] Table 2-62, [IVB] Table 2-76).
const FIXED_COUNTER_ENABLE: u64 = 1 << 22;

const OPCODE_MAX: u64 = 0x1ff;

const LLC_LOOKUP_EVENT: u8 = 0x34;
const TOR_INSERTS_EVENT: u8 = 0x35;
const TOR_OCCUPANCY_EVENT: u8 = 0x36;
const LLC_VICTIMS_EVENT: u8 = 0x37;
/// Umask bits of TOR_INSERTS and TOR_OCCUPANCY that match the filter opcode
/// and nid ([SNB] Tables 2-30, 2-31 and [IVB] Tables 2-33, 2-34, "Filter
/// Dep"), and the NID bit of LLC_LOOKUP and LLC_VICTIMS ([SNB] Tables 2-16,
/// 2-17; [IVB] Tables 2-19, 2-20).
const TOR_OPCODE_UMASK: u8 = 0x01;
const NID_UMASK: u8 = 0x40;

/// What sets the configs of the two models apart.
struct Model {
    name: &'static str,
    cbo: BoxSpec,
    imc: BoxSpec,
    filters: FilterLayout,
    /// IDI opcodes by name, from the model's guide.
    opcodes: &'static [(&'static str, u16)],
    tor_inserts_umasks: &'static [u8],
    tor_occupancy_umasks: &'static [u8],
}

/// Where a model's CBo filter fields sit, as masks in their register. A
/// request's filter0 fields (tid, state) are in Cn_MSR_PMON_BOX_FILTER0 on
/// both models. Its filter1 fields (nid, opcode, nc, isoc) are in
/// Cn_MSR_PMON_BOX_FILTER1 on Ivy Bridge-EP, but on Sandy Bridge-EP in the
/// one Cn_MSR_PMON_BOX_FILTER, which sits where FILTER0 does.
struct FilterLayout {
    tid: u64,
    state_shift: u32,
    state: u64,
    nid_shift: u32,
    nid: u64,
    opcode_shift: u32,
    opcode: u64,
    /// 0 where the model has no such bit.
    nc: u64,
    isoc: u64,
    /// Whether filter1's fields have a register of their own.
    filter1_register: bool,
}

impl FilterLayout {
    const fn filter0_mask(&self) -> u64 {
        self.state | self.tid
    }

    const fn filter1_mask(&self) -> u64 {
        self.isoc | self.nc | self.opcode | self.nid
    }

    /// The registers that hold a group's filter0 and filter1 fields.
    fn registers(&self, filter0: u64, filter1: u64) -> ChaFilters {
        if self.filter1_register {
            ChaFilters { filter0, filter1 }
        } else {
            ChaFilters {
                filter0: filter0 | filter1,
                filter1: 0,
            }
        }
    }
}

const SANDY_BRIDGE: Model = Model {
    name: SNB_MODEL,
    cbo: BoxSpec {
        model: SNB_MODEL,
        name: "CBo",
        general_counters: 4,
        event_counters: &counters::SNB_CBO,
        aliases: SNB_CBO_ALIASES,
    },
    imc: BoxSpec {
        model: SNB_MODEL,
        name: "IMC",
        general_counters: 4,
        event_counters: &counters::SNB_IMC,
        aliases: SNB_IMC_ALIASES,
    },
    // Cn_MSR_PMON_BOX_FILTER ([SNB] Table 2-12): tid [4:0], nid [17:10],
    // state [22:18], opc [31:23]; bits 9:5 are reserved. Linux's
    // snbep_cbox_filter_mask has the same fields.
    filters: FilterLayout {
        tid: 0x1f,
        state_shift: 18,
        state: 0x1f << 18,
        nid_shift: 10,
        nid: 0xff << 10,
        opcode_shift: 23,
        opcode: OPCODE_MAX << 23,
        nc: 0,
        isoc: 0,
        filter1_register: false,
    },
    opcodes: &SNB_OPCODES,
    tor_inserts_umasks: SNB_TOR_INSERTS_UMASKS,
    tor_occupancy_umasks: SNB_TOR_OCCUPANCY_UMASKS,
};

const IVY_BRIDGE: Model = Model {
    name: IVB_MODEL,
    cbo: BoxSpec {
        model: IVB_MODEL,
        name: "CBo",
        general_counters: 4,
        event_counters: &counters::IVB_CBO,
        aliases: IVB_CBO_ALIASES,
    },
    imc: BoxSpec {
        model: IVB_MODEL,
        name: "IMC",
        general_counters: 4,
        event_counters: &counters::IVB_IMC,
        aliases: IVB_IMC_ALIASES,
    },
    // Cn_MSR_PMON_BOX_FILTER0 ([IVB] Table 2-16): tid [4:0], state [22:17];
    // bits 16:5 are reserved. Cn_MSR_PMON_BOX_FILTER1 (Table 2-17): nid
    // [15:0], opc [28:20], nc 30, isoc 31; bit 29 is reserved. Linux's
    // ivbep_cbox_filter_mask also has a link field at filter0 bits 8:5 and
    // a c6 bit at filter1 bit 29, which the guide reserves and configs leave
    // 0.
    filters: FilterLayout {
        tid: 0x1f,
        state_shift: 17,
        state: 0x3f << 17,
        nid_shift: 0,
        nid: 0xffff,
        opcode_shift: 20,
        opcode: OPCODE_MAX << 20,
        nc: 1 << 30,
        isoc: 1 << 31,
        filter1_register: true,
    },
    opcodes: &IVB_OPCODES,
    tor_inserts_umasks: IVB_TOR_INSERTS_UMASKS,
    tor_occupancy_umasks: IVB_TOR_OCCUPANCY_UMASKS,
};

const SNB_MODEL: &str = "Sandy Bridge-EP";
const IVB_MODEL: &str = "Ivy Bridge-EP";

/// The TOR_INSERTS umasks of [SNB] Table 2-30 and the JSON: OPCODE (0x01),
/// MISS_OPCODE (0x03), EVICTION (0x04), MISS_ALL (0x0a) and WB (0x10), each
/// plain or with NID (0x40), and NID_ALL (0x48), whose plain ALL (0x08)
/// neither lists.
const SNB_TOR_INSERTS_UMASKS: &[u8] = &[
    0x01, 0x03, 0x04, 0x0a, 0x10, 0x41, 0x43, 0x44, 0x48, 0x4a, 0x50,
];
/// The TOR_OCCUPANCY umasks of [SNB] Table 2-31 and the JSON: OPCODE,
/// MISS_OPCODE, EVICTION, ALL (0x08) and MISS_ALL, each plain or with NID;
/// no WB.
const SNB_TOR_OCCUPANCY_UMASKS: &[u8] =
    &[0x01, 0x03, 0x04, 0x08, 0x0a, 0x41, 0x43, 0x44, 0x48, 0x4a];
/// The TOR_INSERTS umasks of [IVB] Table 2-33 and the JSON, those of
/// Haswell-EP: one base subevent, OPCODE, EVICTION, ALL or WB; MISS only with
/// OPCODE or ALL; and at most one of LOCAL (0x20), NID (0x40) or REMOTE
/// (0x80), which neither lists with every base.
const IVB_TOR_INSERTS_UMASKS: &[u8] = &[
    0x01, 0x03, 0x04, 0x08, 0x10, 0x21, 0x23, 0x28, 0x2a, 0x41, 0x43, 0x44, 0x48, 0x4a, 0x50, 0x81,
    0x83, 0x88, 0x8a,
];
/// The TOR_OCCUPANCY umasks of [IVB] Table 2-34 and the JSON: those of
/// TOR_INSERTS and MISS_ALL (0x0a).
const IVB_TOR_OCCUPANCY_UMASKS: &[u8] = &[
    0x01, 0x03, 0x04, 0x08, 0x0a, 0x10, 0x21, 0x23, 0x28, 0x2a, 0x41, 0x43, 0x44, 0x48, 0x4a, 0x50,
    0x81, 0x83, 0x88, 0x8a,
];

// Encodings from the guides and the JSON, which agree; the comments name the
// events and tables.

/// UNC_C_TOR_INSERTS.* and UNC_C_TOR_OCCUPANCY.*: the base subevent, then
/// where the request's memory is homed. Not every pair is valid; see each
/// model's umask lists.
const TOR_KINDS: Param = Param {
    field: "kind",
    values: &[
        ("opcode", 0x01),
        ("miss_opcode", 0x03),
        ("eviction", 0x04),
        ("all", 0x08),
        ("miss_all", 0x0a),
        ("wb", 0x10),
    ],
};
/// Sandy Bridge-EP tells homes apart only by the filter nid.
const SNB_TOR_HOMES: Param = Param {
    field: "home",
    values: &[("any", 0x00), ("nid", NID_UMASK)],
};
const IVB_TOR_HOMES: Param = Param {
    field: "home",
    values: &[
        ("any", 0x00),
        ("local", 0x20),
        ("remote", 0x80),
        ("nid", NID_UMASK),
    ],
};
/// UNC_C_LLC_LOOKUP.* ([SNB] Table 2-16)
const SNB_LLC_LOOKUP_OPS: Param = Param {
    field: "op",
    values: &[
        ("data_read", 0x03),
        ("write", 0x05),
        ("remote_snoop", 0x09),
        ("nid", 0x41),
    ],
};
/// UNC_C_LLC_LOOKUP.* ([IVB] Table 2-19), which adds ANY.
const IVB_LLC_LOOKUP_OPS: Param = Param {
    field: "op",
    values: &[
        ("data_read", 0x03),
        ("write", 0x05),
        ("remote_snoop", 0x09),
        ("any", 0x11),
        ("nid", 0x41),
    ],
};
/// UNC_C_LLC_VICTIMS.* ([SNB] Table 2-17, [IVB] Table 2-20)
const LLC_VICTIM_STATES: Param = Param {
    field: "state",
    values: &[("m", 0x01), ("e", 0x02), ("s", 0x04), ("miss", 0x08)],
};

const SNB_CBO_ALIASES: &[Alias] = &[
    // UNC_C_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    Alias::new(
        "tor_inserts",
        TOR_INSERTS_EVENT,
        0x00,
        &[TOR_KINDS, SNB_TOR_HOMES],
    ),
    Alias::new(
        "tor_occupancy",
        TOR_OCCUPANCY_EVENT,
        0x00,
        &[TOR_KINDS, SNB_TOR_HOMES],
    ),
    Alias::new("llc_lookup", LLC_LOOKUP_EVENT, 0x00, &[SNB_LLC_LOOKUP_OPS]),
    Alias::new("llc_victims", LLC_VICTIMS_EVENT, 0x00, &[LLC_VICTIM_STATES]),
];

const IVB_CBO_ALIASES: &[Alias] = &[
    // UNC_C_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    Alias::new(
        "tor_inserts",
        TOR_INSERTS_EVENT,
        0x00,
        &[TOR_KINDS, IVB_TOR_HOMES],
    ),
    Alias::new(
        "tor_occupancy",
        TOR_OCCUPANCY_EVENT,
        0x00,
        &[TOR_KINDS, IVB_TOR_HOMES],
    ),
    Alias::new("llc_lookup", LLC_LOOKUP_EVENT, 0x00, &[IVB_LLC_LOOKUP_OPS]),
    Alias::new("llc_victims", LLC_VICTIMS_EVENT, 0x00, &[LLC_VICTIM_STATES]),
];

/// UNC_M_CAS_COUNT.* ([SNB] Table 2-66, [IVB] Table 2-80)
const CAS_COUNT_OPS: Param = Param {
    field: "op",
    values: &[("rd", 0x03), ("wr", 0x0c), ("all", 0x0f)],
};

// The RPQ and WPQ events of [SNB] Table 2-64 and the JSON.
const SNB_IMC_ALIASES: &[Alias] = &[
    Alias::new("cas_count", 0x04, 0x00, &[CAS_COUNT_OPS]),
    // UNC_M_RPQ_INSERTS
    Alias::new("rpq_inserts", 0x10, 0x00, &[]),
    // UNC_M_RPQ_CYCLES_NE
    Alias::new("rpq_cycles_ne", 0x11, 0x00, &[]),
    // UNC_M_RPQ_CYCLES_FULL
    Alias::new("rpq_cycles_full", 0x12, 0x00, &[]),
    // UNC_M_WPQ_INSERTS
    Alias::new("wpq_inserts", 0x20, 0x00, &[]),
    // UNC_M_WPQ_CYCLES_NE
    Alias::new("wpq_cycles_ne", 0x21, 0x00, &[]),
    // UNC_M_WPQ_CYCLES_FULL
    Alias::new("wpq_cycles_full", 0x22, 0x00, &[]),
    // UNC_M_RPQ_OCCUPANCY
    Alias::new("rpq_occupancy", 0x80, 0x00, &[]),
    // UNC_M_WPQ_OCCUPANCY
    Alias::new("wpq_occupancy", 0x81, 0x00, &[]),
];

// [IVB] section 2.5.6 and the JSON list neither RPQ_CYCLES_FULL (0x12) nor
// the occupancy events 0x80 and 0x81.
const IVB_IMC_ALIASES: &[Alias] = &[
    Alias::new("cas_count", 0x04, 0x00, &[CAS_COUNT_OPS]),
    // UNC_M_RPQ_INSERTS
    Alias::new("rpq_inserts", 0x10, 0x00, &[]),
    // UNC_M_RPQ_CYCLES_NE
    Alias::new("rpq_cycles_ne", 0x11, 0x00, &[]),
    // UNC_M_WPQ_INSERTS
    Alias::new("wpq_inserts", 0x20, 0x00, &[]),
    // UNC_M_WPQ_CYCLES_NE
    Alias::new("wpq_cycles_ne", 0x21, 0x00, &[]),
    // UNC_M_WPQ_CYCLES_FULL
    Alias::new("wpq_cycles_full", 0x22, 0x00, &[]),
];

/// The filter state bits by name ([SNB] Table 2-12, [IVB] Table 2-16: I
/// 0x01, S 0x02, E 0x04, M 0x08, F 0x10), which the default CBo collector
/// programs too. Ivy Bridge-EP's sixth bit (0x20, M' in the JSON) takes a
/// raw filter0.
const FILTER0_STATES: [(&str, ChaCacheState); 5] = [
    ("I", ChaCacheState::I),
    ("S", ChaCacheState::S),
    ("E", ChaCacheState::E),
    ("M", ChaCacheState::M),
    ("F", ChaCacheState::F),
];

/// IDI opcodes from [SNB] Table 2-13, "Opcode Match by IDI Packet Type for
/// Cn_MSR_PMON_BOX_FILTER.opc".
const SNB_OPCODES: [(&str, u16); 19] = [
    ("RFO", 0x180),
    ("CRd", 0x181),
    ("DRd", 0x182),
    ("PRd", 0x187),
    ("WCiLF", 0x18c),
    ("WCiL", 0x18d),
    ("PrefRFO", 0x190),
    ("PrefCode", 0x191),
    ("PrefData", 0x192),
    ("PCIWiLF", 0x194),
    ("PCIPRd", 0x195),
    ("PCIItoM", 0x19c),
    ("PCIRdCur", 0x19e),
    ("WbMtoI", 0x1c4),
    ("WbMtoE", 0x1c5),
    ("ItoM", 0x1c8),
    ("PCINSRd", 0x1e4),
    ("PCINSWr", 0x1e5),
    ("PCINSWrF", 0x1e6),
];

/// IDI opcodes from [IVB] Table 2-18, without PCIWrUpdate (0x19d) and PCIRMW
/// (0x19e), which it marks as the prior generation's.
const IVB_OPCODES: [(&str, u16); 19] = [
    ("RFO", 0x180),
    ("CRd", 0x181),
    ("DRd", 0x182),
    ("PRd", 0x187),
    ("WCiLF", 0x18c),
    ("WCiL", 0x18d),
    ("PrefRFO", 0x190),
    ("PrefCode", 0x191),
    ("PrefData", 0x192),
    ("PCIWiL", 0x193),
    ("PCIWiLF", 0x194),
    ("PCIItoM", 0x19c),
    ("PCIRdCur", 0x19e),
    ("WbMtoI", 0x1c4),
    ("WbMtoE", 0x1c5),
    ("ItoM", 0x1c8),
    ("PCINSRd", 0x1e4),
    ("PCINSWr", 0x1e5),
    ("PCINSWrF", 0x1e6),
];

/// A CBo group's filter registers, and which match fields the request sets.
/// A raw register sets a field when its bits of the field are not 0.
#[derive(Clone, Copy)]
struct CboFilters {
    registers: ChaFilters,
    tid: bool,
    opcode: bool,
    nid: bool,
}

pub(super) fn program_sandy_bridge_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    program_group(&SANDY_BRIDGE, box_type, group, path)
}

pub(super) fn program_ivy_bridge_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    program_group(&IVY_BRIDGE, box_type, group, path)
}

fn program_group(
    model: &Model,
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    let snb_box = match box_type {
        BoxType::Cha => &model.cbo,
        BoxType::Imc => &model.imc,
        BoxType::Iio | BoxType::Irp | BoxType::Ha => {
            return Err(format!(
                "{path}: PMU configs do not support {} on {}",
                box_type.label(),
                model.name
            ));
        }
    };
    check_event_count(snb_box, group, path)?;
    let filters = if box_type == BoxType::Cha {
        Some(cbo_filters(model, group, path)?)
    } else {
        for (field, given) in [
            ("filter0", group.filter0.is_some()),
            ("filter1", group.filter1.is_some()),
        ] {
            if given {
                return Err(format!("{path}.{field}: only CBo groups take filters"));
            }
        }
        None
    };
    if group.fixed && box_type != BoxType::Imc {
        return Err(format!(
            "{path}.fixed: only IMC boxes have a fixed counter on {}",
            model.name
        ));
    }

    let mut counters = Vec::new();
    let mut tid_used = false;
    let mut opcode_used = false;
    for (slot, event) in group.events.iter().enumerate() {
        let Some(event) = event else {
            continue;
        };
        let event_path = format!("{path}.events[{slot}]");
        let encoding = encoding(model, snb_box, event, &event_path)?;
        check_counter(snb_box, &encoding, slot, &event_path)?;
        if let Some(filters) = filters {
            check_cbo_event(model, &encoding, filters, &event_path)?;
            opcode_used |= is_tor(encoding.event) && encoding.umask & TOR_OPCODE_UMASK != 0;
        }
        if event.tid_en {
            match filters {
                None => return Err(format!("{event_path}.tid_en: only CBo events take tid_en")),
                Some(filters) if !filters.tid => {
                    return Err(format!(
                        "{event_path}.tid_en: the group's filter0 sets no tid"
                    ));
                }
                Some(_) => tid_used = true,
            }
        }
        counters.push(ProgrammedCounter {
            slot: CounterSlot::General(slot as u8),
            control: control(&encoding, event.tid_en),
            labels: labels(model, &encoding, event.tid_en, filters),
        });
    }
    if let Some(filters) = filters {
        if filters.tid && !tid_used {
            return Err(format!(
                "{path}.filter0: sets a tid, but no event sets tid_en"
            ));
        }
        // The opcode matches TOR events with an opcode umask; nothing else
        // counts by it ([SNB] Table 2-12, [IVB] Table 2-17).
        if filters.opcode && !opcode_used {
            return Err(format!(
                "{path}.filter1: sets an opcode, but no TOR event has an opcode umask (0x01)"
            ));
        }
    }
    if group.fixed {
        // The default IMC collector's DCLK control without the reset bit.
        counters.push(ProgrammedCounter {
            slot: CounterSlot::Fixed,
            control: FIXED_COUNTER_ENABLE,
            labels: EventLabels::from([("alias".to_string(), "dclk".to_string())]),
        });
    }

    Ok(GroupProgram {
        counters,
        filters: filters.map(|filters| filters.registers),
    })
}

fn encoding(
    model: &Model,
    snb_box: &BoxSpec,
    event: &EventRequest,
    path: &str,
) -> Result<Encoding, String> {
    for (field, given) in [
        ("ch_mask", event.ch_mask.is_some()),
        ("fc_mask", event.fc_mask.is_some()),
    ] {
        if given {
            return Err(format!("{path}.{field}: not supported on {}", model.name));
        }
    }

    event_encoding(snb_box, event, path)
}

fn is_tor(event: u8) -> bool {
    event == TOR_INSERTS_EVENT || event == TOR_OCCUPANCY_EVENT
}

fn check_cbo_event(
    model: &Model,
    encoding: &Encoding,
    filters: CboFilters,
    path: &str,
) -> Result<(), String> {
    let (name, umasks) = match encoding.event {
        TOR_INSERTS_EVENT => ("TOR_INSERTS", model.tor_inserts_umasks),
        TOR_OCCUPANCY_EVENT => ("TOR_OCCUPANCY", model.tor_occupancy_umasks),
        LLC_LOOKUP_EVENT => ("LLC_LOOKUP", &[][..]),
        LLC_VICTIMS_EVENT => ("LLC_VICTIMS", &[][..]),
        _ => return Ok(()),
    };
    if is_tor(encoding.event) {
        if !umasks.contains(&encoding.umask) {
            return Err(format!(
                "{path}: {} {name} (event {:#04x}) has no umask {:#04x} (umasks: {})",
                model.name,
                encoding.event,
                encoding.umask,
                join(umasks.iter().map(|umask| format!("{umask:#04x}")))
            ));
        }
        // "Subevents that require an opcode or NID match require the
        // Cn_MSR_PMON_BOX_FILTER.{opc, nid} field to be set." (TOR_INSERTS
        // and TOR_OCCUPANCY of both guides)
        if encoding.umask & TOR_OPCODE_UMASK != 0 && !filters.opcode {
            return Err(format!(
                "{path}: umask {:#04x} of {name} (event {:#04x}) needs a filter1 opcode",
                encoding.umask, encoding.event
            ));
        }
    }
    if encoding.event == LLC_LOOKUP_EVENT {
        // "One must ALWAYS set filter mask bit 0 and select a state or
        // states to match. Otherwise, the event will count nothing." (LLC_LOOKUP
        // of both guides)
        if encoding.umask & 0x01 == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without umask bit 0"
            ));
        }
        if filters.registers.filter0 & model.filters.state == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without filter0 states"
            ));
        }
    }
    // The NID subevents match the target NID "programmed in
    // Cn_MSR_PMON_BOX_FILTER.nid".
    if encoding.umask & NID_UMASK != 0 && !filters.nid {
        return Err(format!(
            "{path}: umask {:#04x} of {name} (event {:#04x}) needs a filter1 nid",
            encoding.umask, encoding.event
        ));
    }

    Ok(())
}

fn cbo_filters(model: &Model, group: &GroupRequest, path: &str) -> Result<CboFilters, String> {
    let (filter0, tid) = filter0(&model.filters, group.filter0.as_ref(), path)?;
    let (filter1, opcode, nid) = filter1(model, group.filter1.as_ref(), path)?;

    Ok(CboFilters {
        registers: model.filters.registers(filter0, filter1),
        tid,
        opcode,
        nid,
    })
}

/// The bits of filter0's fields, by default 0, and whether they set a tid.
fn filter0(
    layout: &FilterLayout,
    request: Option<&Filter0Request>,
    path: &str,
) -> Result<(u64, bool), String> {
    let Some(request) = request else {
        return Ok((0, false));
    };
    let path = format!("{path}.filter0");
    if let Some(raw) = &request.raw {
        if request.states.is_some() || request.tid.is_some() {
            return Err(format!("{path}: raw excludes states and tid"));
        }
        let value = masked_filter(raw, layout.filter0_mask(), &format!("{path}.raw"))?;
        return Ok((value, value & layout.tid != 0));
    }
    if request.states.is_none() && request.tid.is_none() {
        return Err(format!("{path}: needs states, tid or raw"));
    }

    let mut value = 0;
    if let Some(states) = &request.states {
        if states.is_empty() {
            return Err(format!("{path}.states: must not be empty"));
        }
        for (index, name) in states.iter().enumerate() {
            let state = FILTER0_STATES
                .iter()
                .find(|(state, _)| state == name)
                .map(|(_, state)| *state)
                .ok_or_else(|| {
                    format!(
                        "{path}.states[{index}]: unknown state \"{name}\" (states: {})",
                        join(FILTER0_STATES.iter().map(|(state, _)| state))
                    )
                })?;
            value |= u64::from(snb_llc_lookup_state_bits(state)) << layout.state_shift;
        }
    }
    if let Some(tid) = &request.tid {
        if tid.0 > layout.tid {
            return Err(format!(
                "{path}.tid: must be at most {:#04x}, got {:#x}",
                layout.tid, tid.0
            ));
        }
        value |= tid.0;
    }

    Ok((value, request.tid.is_some()))
}

/// The bits of filter1's fields, by default 0, and whether they set an
/// opcode and a nid.
fn filter1(
    model: &Model,
    request: Option<&Filter1Request>,
    path: &str,
) -> Result<(u64, bool, bool), String> {
    let Some(request) = request else {
        return Ok((0, false, false));
    };
    let layout = &model.filters;
    let path = format!("{path}.filter1");
    for (field, unsupported) in [
        ("locality", request.locality.is_some()),
        ("nc", request.nc && layout.nc == 0),
        ("isoc", request.isoc && layout.isoc == 0),
    ] {
        if unsupported {
            return Err(format!("{path}.{field}: not supported on {}", model.name));
        }
    }
    if let Some(raw) = &request.raw {
        if request.opcodes.is_some() || request.nid.is_some() || request.nc || request.isoc {
            return Err(format!("{path}: raw excludes opcodes, nid, nc and isoc"));
        }
        let value = masked_filter(raw, layout.filter1_mask(), &format!("{path}.raw"))?;
        return Ok((value, value & layout.opcode != 0, value & layout.nid != 0));
    }

    let mut value = 0;
    if let Some(tokens) = &request.opcodes {
        if tokens.len() != 1 {
            return Err(format!(
                "{path}.opcodes: takes 1 opcode, got {}",
                tokens.len()
            ));
        }
        let opcode = opcode(model, &tokens[0], &format!("{path}.opcodes[0]"))?;
        value |= u64::from(opcode) << layout.opcode_shift;
    }
    if let Some(nid) = &request.nid {
        let max = layout.nid >> layout.nid_shift;
        if nid.0 > max {
            return Err(format!(
                "{path}.nid: must be at most {max:#x}, got {:#x}",
                nid.0
            ));
        }
        value |= nid.0 << layout.nid_shift;
    }
    if request.nc {
        value |= layout.nc;
    }
    if request.isoc {
        value |= layout.isoc;
    }

    Ok((value, request.opcodes.is_some(), request.nid.is_some()))
}

fn opcode(model: &Model, token: &Token, path: &str) -> Result<u16, String> {
    match token {
        Token::Integer(value) if *value <= OPCODE_MAX => Ok(*value as u16),
        Token::Integer(value) => Err(format!("{path}: opcodes are 9 bits, got {value:#x}")),
        Token::Name(name) => model
            .opcodes
            .iter()
            .find(|(opcode, _)| opcode == name)
            .map(|(_, value)| *value)
            .ok_or_else(|| {
                format!(
                    "{path}: unknown {} opcode \"{name}\" (opcodes: {})",
                    model.name,
                    join(model.opcodes.iter().map(|(opcode, _)| opcode))
                )
            }),
    }
}

fn masked_filter(value: &Number, mask: u64, path: &str) -> Result<u64, String> {
    if value.0 & !mask != 0 {
        return Err(format!(
            "{path}: must fit the mask {mask:#x}, got {:#x}",
            value.0
        ));
    }

    Ok(value.0)
}

fn control(encoding: &Encoding, tid_en: bool) -> u64 {
    let tid_enable = if tid_en { COUNTER_TID_ENABLE } else { 0 };

    u64::from(encoding.event) | (u64::from(encoding.umask) << 8) | tid_enable | COUNTER_ENABLE
}

/// The event's labels, with the CBo filter registers the model has: filter0
/// only on Sandy Bridge-EP.
fn labels(
    model: &Model,
    encoding: &Encoding,
    tid_en: bool,
    filters: Option<CboFilters>,
) -> EventLabels {
    let mut labels = EventLabels::from([
        ("alias".to_string(), encoding.alias.to_string()),
        ("event".to_string(), format!("{:#04x}", encoding.event)),
        ("umask".to_string(), format!("{:#04x}", encoding.umask)),
    ]);
    if let Some(filters) = filters {
        let registers = filters.registers;
        labels.insert("filter0".to_string(), format!("{:#x}", registers.filter0));
        if model.filters.filter1_register {
            labels.insert("filter1".to_string(), format!("{:#x}", registers.filter1));
        }
    }
    if tid_en {
        labels.insert("tid_en".to_string(), "1".to_string());
    }

    labels
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::*;
    use crate::arch::IntelServerCpuModel;
    use crate::pmu_config::spec::{ID_LIMIT, PmuConfigRequest, ProgrammedConfig, validate};
    use crate::pmu_config::types::{PmuTopology, ProgrammedGroup};

    const SNB: IntelServerCpuModel = IntelServerCpuModel::SandyBridgeEp;
    const IVB: IntelServerCpuModel = IntelServerCpuModel::IvyTown;

    fn topology() -> PmuTopology {
        let packages =
            |count: u16| BTreeMap::from([(0, (0..count).collect()), (1, (0..count).collect())]);

        PmuTopology {
            units: BTreeMap::from([
                (BoxType::Cha, packages(8)),
                (BoxType::Iio, packages(1)),
                (BoxType::Irp, packages(1)),
                (BoxType::Imc, packages(4)),
                (BoxType::Ha, packages(1)),
            ]),
        }
    }

    fn configs(
        model: IntelServerCpuModel,
        request: Value,
    ) -> Result<Vec<ProgrammedConfig>, String> {
        let request: PmuConfigRequest =
            serde_json::from_value(request).map_err(|error| error.to_string())?;
        validate(request, &topology(), model)
    }

    fn pmu(box_type: &str, group: Value) -> Value {
        json!({"pmus": {box_type: {"groups": [group]}}})
    }

    fn program(model: IntelServerCpuModel, group: Value) -> Result<ProgrammedGroup, String> {
        let mut configs = configs(model, pmu("cha", group))?;
        Ok(configs.remove(0).program.groups.remove(0))
    }

    fn name(model: IntelServerCpuModel) -> &'static str {
        match model {
            SNB => SNB_MODEL,
            IVB => IVB_MODEL,
            model => panic!("{model:?} is not Sandy or Ivy Bridge-EP"),
        }
    }

    fn controls(group: &ProgrammedGroup) -> Vec<(CounterSlot, u64)> {
        group
            .counters
            .iter()
            .map(|counter| (counter.slot, counter.control))
            .collect()
    }

    fn labels(pairs: &[(&str, &str)]) -> EventLabels {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn filters(filter0: u64, filter1: u64) -> Option<ChaFilters> {
        Some(ChaFilters { filter0, filter1 })
    }

    /// The PCIe ItoM TOR inserts of the default collector's `PciItoM` group:
    /// the PCIe thread id 0x1e ([SNB] Table 2-12 note) and opcode 0x1c8 land
    /// in the one filter register on Sandy Bridge-EP and in FILTER0 and
    /// FILTER1 on Ivy Bridge-EP.
    #[test]
    fn programs_pcie_itom_tor_inserts_in_each_filter_layout() {
        let request = json!({
            "filter0": {"tid": "0x1e"},
            "filter1": {"opcodes": ["ItoM"]},
            "events": [{"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true}]
        });

        let snb = program(SNB, request.clone()).unwrap();
        assert_eq!(snb.filters, filters(0xe400_001e, 0));
        assert_eq!(controls(&snb), [(CounterSlot::General(0), 0x48_0135)]);
        assert_eq!(
            snb.counters[0].labels,
            labels(&[
                ("alias", "tor_inserts"),
                ("event", "0x35"),
                ("umask", "0x01"),
                ("filter0", "0xe400001e"),
                ("tid_en", "1"),
            ])
        );

        let ivb = program(IVB, request).unwrap();
        assert_eq!(ivb.filters, filters(0x1e, 0x1c80_0000));
        assert_eq!(controls(&ivb), [(CounterSlot::General(0), 0x48_0135)]);
        assert_eq!(
            ivb.counters[0].labels,
            labels(&[
                ("alias", "tor_inserts"),
                ("event", "0x35"),
                ("umask", "0x01"),
                ("filter0", "0x1e"),
                ("filter1", "0x1c800000"),
                ("tid_en", "1"),
            ])
        );
    }

    #[test]
    fn encodes_events_exactly() {
        // (request, Sandy Bridge-EP filters, Ivy Bridge-EP filters, controls)
        let cases = [
            (
                json!({
                    "filter1": {"opcodes": ["DRd"]},
                    "events": [{"alias": "tor_occupancy", "kind": "miss_opcode", "home": "any"}]
                }),
                (0xc100_0000, 0),
                (0, 0x1820_0000),
                vec![(CounterSlot::General(0), 0x40_0336)],
            ),
            (
                json!({
                    "filter0": {"states": ["M"]},
                    "events": [
                        {"alias": "llc_lookup", "op": "data_read"},
                        {"alias": "llc_lookup", "op": "write"}
                    ]
                }),
                (0x20_0000, 0),
                (0x10_0000, 0),
                vec![
                    (CounterSlot::General(0), 0x40_0334),
                    (CounterSlot::General(1), 0x40_0534),
                ],
            ),
            (
                json!({"events": [{"alias": "llc_victims", "state": "m"}, {"alias": "llc_victims", "state": "miss"}]}),
                (0, 0),
                (0, 0),
                vec![
                    (CounterSlot::General(0), 0x40_0137),
                    (CounterSlot::General(1), 0x40_0837),
                ],
            ),
            (
                json!({"events": [null, null, null, {"alias": "clockticks"}]}),
                (0, 0),
                (0, 0),
                vec![(CounterSlot::General(3), 0x40_0000)],
            ),
        ];

        for (request, snb, ivb, expected) in cases {
            for (model, (filter0, filter1)) in [(SNB, snb), (IVB, ivb)] {
                let group = program(model, request.clone()).unwrap();

                assert_eq!(
                    group.filters,
                    filters(filter0, filter1),
                    "{model:?} {request}"
                );
                assert_eq!(controls(&group), expected, "{model:?} {request}");
            }
        }
        let clockticks = program(SNB, json!({"events": [{"alias": "clockticks"}]})).unwrap();
        assert_eq!(
            clockticks.counters[0].labels,
            labels(&[
                ("alias", "clockticks"),
                ("event", "0x00"),
                ("umask", "0x00"),
                ("filter0", "0x0"),
            ])
        );
    }

    #[test]
    fn encodes_every_alias() {
        let both = &[SNB, IVB][..];
        // A group with the filters every event below needs.
        let group = |event: Value| {
            json!({
                "filter0": {"states": ["I"]},
                "filter1": {"opcodes": ["DRd"], "nid": 1},
                "events": [event, {"alias": "tor_inserts", "kind": "opcode", "home": "any"}]
            })
        };
        let cases = [
            (both, json!({"alias": "clockticks"}), 0x00, 0x00),
            (
                both,
                json!({"alias": "tor_inserts", "kind": "opcode", "home": "nid"}),
                0x35,
                0x41,
            ),
            (
                both,
                json!({"alias": "tor_inserts", "kind": "miss_opcode", "home": "any"}),
                0x35,
                0x03,
            ),
            (
                both,
                json!({"alias": "tor_inserts", "kind": "eviction", "home": "nid"}),
                0x35,
                0x44,
            ),
            (
                both,
                json!({"alias": "tor_inserts", "kind": "wb", "home": "any"}),
                0x35,
                0x10,
            ),
            (
                &[SNB],
                json!({"alias": "tor_inserts", "kind": "miss_all", "home": "nid"}),
                0x35,
                0x4a,
            ),
            (
                &[IVB],
                json!({"alias": "tor_inserts", "kind": "all", "home": "local"}),
                0x35,
                0x28,
            ),
            (
                &[IVB],
                json!({"alias": "tor_inserts", "kind": "miss_opcode", "home": "remote"}),
                0x35,
                0x83,
            ),
            (
                both,
                json!({"alias": "tor_occupancy", "kind": "all", "home": "any"}),
                0x36,
                0x08,
            ),
            (
                both,
                json!({"alias": "tor_occupancy", "kind": "miss_all", "home": "nid"}),
                0x36,
                0x4a,
            ),
            (
                &[IVB],
                json!({"alias": "tor_occupancy", "kind": "wb", "home": "nid"}),
                0x36,
                0x50,
            ),
            (
                &[IVB],
                json!({"alias": "tor_occupancy", "kind": "miss_all", "home": "remote"}),
                0x36,
                0x8a,
            ),
            (
                both,
                json!({"alias": "llc_lookup", "op": "remote_snoop"}),
                0x34,
                0x09,
            ),
            (
                both,
                json!({"alias": "llc_lookup", "op": "nid"}),
                0x34,
                0x41,
            ),
            (
                &[IVB],
                json!({"alias": "llc_lookup", "op": "any"}),
                0x34,
                0x11,
            ),
            (
                both,
                json!({"alias": "llc_victims", "state": "e"}),
                0x37,
                0x02,
            ),
            (
                both,
                json!({"alias": "llc_victims", "state": "s"}),
                0x37,
                0x04,
            ),
        ];

        for (models, event, code, umask) in cases {
            for &model in models {
                let group = program(model, group(event.clone())).unwrap();

                assert_eq!(
                    group.counters[0].control,
                    code | (umask << 8) | COUNTER_ENABLE,
                    "{model:?} {event}"
                );
            }
        }
    }

    #[test]
    fn programs_imc_cas_counts_and_dclk() {
        for model in [SNB, IVB] {
            let group = configs(
                model,
                pmu(
                    "imc",
                    json!({
                        "events": [
                            {"alias": "cas_count", "op": "rd"},
                            null,
                            {"alias": "cas_count", "op": "wr"},
                            {"alias": "cas_count", "op": "all"}
                        ],
                        "fixed": true
                    }),
                ),
            )
            .unwrap()
            .remove(0)
            .program
            .groups
            .remove(0);

            assert_eq!(group.filters, None);
            assert_eq!(
                controls(&group),
                [
                    (CounterSlot::General(0), 0x40_0304),
                    (CounterSlot::General(2), 0x40_0c04),
                    (CounterSlot::General(3), 0x40_0f04),
                    // The default collector's DCLK control
                    // FIXED_COUNTER_RESET_AND_ENABLE (0x480000) without the
                    // reset bit (1 << 19).
                    (CounterSlot::Fixed, 0x40_0000),
                ],
                "{model:?}"
            );
            assert_eq!(
                group.counters[0].labels,
                labels(&[("alias", "cas_count"), ("event", "0x04"), ("umask", "0x03")])
            );
            assert_eq!(group.counters[3].labels, labels(&[("alias", "dclk")]));
        }
    }

    #[test]
    fn encodes_every_imc_alias() {
        let both = &[SNB, IVB][..];
        let cases = [
            (both, "rpq_inserts", 0x10),
            (both, "rpq_cycles_ne", 0x11),
            (&[SNB], "rpq_cycles_full", 0x12),
            (both, "wpq_inserts", 0x20),
            (both, "wpq_cycles_ne", 0x21),
            (both, "wpq_cycles_full", 0x22),
            (&[SNB], "rpq_occupancy", 0x80),
            (&[SNB], "wpq_occupancy", 0x81),
        ];

        for (models, alias, code) in cases {
            for &model in models {
                let mut configs =
                    configs(model, pmu("imc", json!({"events": [{"alias": alias}]}))).unwrap();
                let group = configs.remove(0).program.groups.remove(0);

                assert_eq!(
                    controls(&group),
                    [(CounterSlot::General(0), code | COUNTER_ENABLE)],
                    "{model:?} {alias}"
                );
            }
        }
        let imc = |model| configs(model, pmu("imc", json!({"fixed": true})));
        assert!(imc(SNB).is_ok() && imc(IVB).is_ok());
    }

    #[test]
    fn encodes_cbo_filters_in_each_layout() {
        let tor_tid =
            json!({"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true});
        let tor_nid_tid =
            json!({"alias": "tor_inserts", "kind": "opcode", "home": "nid", "tid_en": true});
        let clockticks = json!({"alias": "clockticks"});
        // (request, Sandy Bridge-EP filters, Ivy Bridge-EP filters)
        let cases = [
            (json!({"events": [clockticks.clone()]}), (0, 0), (0, 0)),
            (
                json!({"filter0": {"states": ["I", "S", "E", "M", "F"]}, "events": [clockticks.clone()]}),
                (0x7c_0000, 0),
                (0x3e_0000, 0),
            ),
            (
                json!({
                    "filter0": {"states": ["M"], "tid": 30},
                    "events": [{"alias": "llc_lookup", "op": "data_read", "tid_en": true}]
                }),
                (0x20_001e, 0),
                (0x10_001e, 0),
            ),
            (
                json!({"filter0": {"tid": 0}, "events": [{"alias": "clockticks", "tid_en": true}]}),
                (0, 0),
                (0, 0),
            ),
            (
                json!({
                    "filter0": {"tid": "0x1f"},
                    "filter1": {"opcodes": ["0x19e"]},
                    "events": [tor_tid.clone()]
                }),
                (0xcf00_001f, 0),
                (0x1f, 0x19e0_0000),
            ),
            (
                json!({"filter1": {"nid": 1}, "events": [clockticks.clone()]}),
                (0x400, 0),
                (0, 0x1),
            ),
            (
                json!({"filter1": {"opcodes": ["PCIRdCur"]}, "events": [{"event": "0x36", "umask": "0x01"}]}),
                (0xcf00_0000, 0),
                (0, 0x19e0_0000),
            ),
            (
                json!({"filter1": {}, "events": [clockticks.clone()]}),
                (0, 0),
                (0, 0),
            ),
        ];

        for (request, snb, ivb) in cases {
            for (model, (filter0, filter1)) in [(SNB, snb), (IVB, ivb)] {
                let group = program(model, request.clone()).unwrap();

                assert_eq!(
                    group.filters,
                    filters(filter0, filter1),
                    "{model:?} {request}"
                );
            }
        }

        let model_cases = [
            // The one Sandy Bridge-EP register: raw filter0 holds tid and
            // state, raw filter1 nid and opcode, each in place.
            (
                SNB,
                json!({
                    "filter0": {"raw": "0x7c001f"},
                    "filter1": {"raw": "0xff83fc00"},
                    "events": [tor_nid_tid.clone()]
                }),
                (0xffff_fc1f, 0),
            ),
            (
                SNB,
                json!({"filter1": {"nid": "0xff"}, "events": [clockticks.clone()]}),
                (0x3_fc00, 0),
            ),
            (
                SNB,
                json!({"filter1": {"opcodes": ["PCIPRd"]}, "events": [
                    {"alias": "tor_occupancy", "kind": "opcode", "home": "any"}
                ]}),
                (0xca80_0000, 0),
            ),
            (
                IVB,
                json!({
                    "filter0": {"raw": "0x7e001f"},
                    "filter1": {"raw": "0xdff0ffff"},
                    "events": [tor_nid_tid.clone()]
                }),
                (0x7e_001f, 0xdff0_ffff),
            ),
            (
                IVB,
                json!({"filter1": {"nid": "0xffff", "nc": true, "isoc": true}, "events": [clockticks.clone()]}),
                (0, 0xc000_ffff),
            ),
            (
                IVB,
                json!({"filter1": {"opcodes": ["PCIWiL"], "nc": true}, "events": [
                    {"alias": "tor_inserts", "kind": "opcode", "home": "any"}
                ]}),
                (0, 0x5930_0000),
            ),
            // The undocumented sixth state bit, M' in the JSON.
            (
                IVB,
                json!({"filter0": {"raw": "0x400000"}, "events": [clockticks.clone()]}),
                (0x40_0000, 0),
            ),
        ];
        for (model, request, (filter0, filter1)) in model_cases {
            let group = program(model, request.clone()).unwrap();

            assert_eq!(
                group.filters,
                filters(filter0, filter1),
                "{model:?} {request}"
            );
        }
    }

    /// Every listed TOR umask is one base subevent, MISS only with OPCODE or
    /// ALL, and at most one home, NID the only one on Sandy Bridge-EP.
    #[test]
    fn tor_umasks_follow_the_subevent_rule() {
        for (model, homes) in [(SANDY_BRIDGE, 0x40), (IVY_BRIDGE, 0xe0)] {
            for umask in model
                .tor_inserts_umasks
                .iter()
                .chain(model.tor_occupancy_umasks)
            {
                let base = umask & 0x1d;
                assert_eq!(base.count_ones(), 1, "{} {umask:#04x}", model.name);
                assert!(
                    umask & 0x02 == 0 || base == 0x01 || base == 0x08,
                    "{} {umask:#04x}",
                    model.name
                );
                assert!(
                    (umask & 0xe0).count_ones() <= 1,
                    "{} {umask:#04x}",
                    model.name
                );
                assert_eq!(umask & 0xe0 & !homes, 0, "{} {umask:#04x}", model.name);
            }
        }
        assert_eq!(SNB_TOR_INSERTS_UMASKS.len(), 11);
        assert_eq!(SNB_TOR_OCCUPANCY_UMASKS.len(), 10);
        assert_eq!(IVB_TOR_INSERTS_UMASKS.len(), 19);
        assert_eq!(IVB_TOR_OCCUPANCY_UMASKS.len(), 20);
        let only_in = |umasks: &[u8], other: &[u8]| {
            umasks
                .iter()
                .copied()
                .filter(|umask| !other.contains(umask))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            only_in(SNB_TOR_INSERTS_UMASKS, SNB_TOR_OCCUPANCY_UMASKS),
            [0x10, 0x50]
        );
        assert_eq!(
            only_in(SNB_TOR_OCCUPANCY_UMASKS, SNB_TOR_INSERTS_UMASKS),
            [0x08]
        );
        assert_eq!(
            only_in(IVB_TOR_OCCUPANCY_UMASKS, IVB_TOR_INSERTS_UMASKS),
            [0x0a]
        );
        assert!(only_in(IVB_TOR_INSERTS_UMASKS, IVB_TOR_OCCUPANCY_UMASKS).is_empty());
    }

    #[test]
    fn accepts_and_rejects_tor_umasks() {
        // filter1 sets an opcode and a nid, which every listed umask accepts.
        let tor = |model, event: &str, umask: u8| {
            program(
                model,
                json!({
                    "filter1": {"opcodes": ["DRd"], "nid": 1},
                    "events": [
                        {"event": event, "umask": umask},
                        {"alias": "tor_inserts", "kind": "opcode", "home": "any"}
                    ]
                }),
            )
        };

        for (model, spec) in [(SNB, SANDY_BRIDGE), (IVB, IVY_BRIDGE)] {
            for umask in 0..=u8::MAX {
                for (event, umasks) in [
                    ("0x35", spec.tor_inserts_umasks),
                    ("0x36", spec.tor_occupancy_umasks),
                ] {
                    assert_eq!(
                        tor(model, event, umask).is_ok(),
                        umasks.contains(&umask),
                        "{model:?} {event} {umask:#04x}"
                    );
                }
            }
        }
        // The two layouts differ where the tables do.
        for (event, umask, snb, ivb) in [
            ("0x35", 0x08, false, true),
            ("0x35", 0x0a, true, false),
            ("0x36", 0x10, false, true),
            ("0x35", 0x21, false, true),
            ("0x36", 0x8a, false, true),
        ] {
            assert_eq!(tor(SNB, event, umask).is_ok(), snb, "{event} {umask:#04x}");
            assert_eq!(tor(IVB, event, umask).is_ok(), ivb, "{event} {umask:#04x}");
        }
    }

    #[test]
    fn rejects_invalid_requests_on_both_models() {
        let cha_event = |event: Value| pmu("cha", json!({"events": [event]}));
        let cha_filters = |filters: Value| {
            let mut group = json!({"events": [{"alias": "clockticks"}]});
            group
                .as_object_mut()
                .unwrap()
                .extend(filters.as_object().unwrap().clone());
            pmu("cha", group)
        };
        let tor_opcode = json!({"alias": "tor_inserts", "kind": "opcode", "home": "any"});

        for model in [SNB, IVB] {
            let name = name(model);
            let cases = [
                (
                    pmu("iio", json!({"events": [{"event": 0}]})),
                    format!("pmus.iio.groups[0]: PMU configs do not support iio on {name}"),
                ),
                (
                    pmu("irp", json!({"events": [{"event": 0}]})),
                    format!("pmus.irp.groups[0]: PMU configs do not support irp on {name}"),
                ),
                (
                    pmu("ha", json!({"events": [{"event": 0}]})),
                    format!("pmus.ha.groups[0]: PMU configs do not support ha on {name}"),
                ),
                (
                    pmu(
                        "cha",
                        json!({"events": vec![json!({"alias": "clockticks"}); 5]}),
                    ),
                    format!(
                        "pmus.cha.groups[0].events: {name} CBo boxes have 4 general counters, got 5 events"
                    ),
                ),
                (
                    cha_event(json!({"event": "0x38"})),
                    format!("pmus.cha.groups[0].events[0]: Intel lists no {name} CBo event 0x38"),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter1": {"opcodes": ["ItoM"]},
                            "events": [tor_opcode.clone(), {"alias": "tor_occupancy", "kind": "opcode", "home": "any"}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[1]: event 0x36 counts only on counters 0".to_string(),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter0": {"states": ["M"]},
                            "events": [null, null, {"alias": "llc_lookup", "op": "write"}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[2]: event 0x34 counts only on counters 0, 1".to_string(),
                ),
                (
                    cha_event(json!({"event": "0x1b", "umask": "0x01"})),
                    "pmus.cha.groups[0].events[0]: event 0x1b counts only on counters 2, 3".to_string(),
                ),
                (
                    cha_event(tor_opcode.clone()),
                    "pmus.cha.groups[0].events[0]: umask 0x01 of TOR_INSERTS (event 0x35) needs a filter1 opcode".to_string(),
                ),
                (
                    cha_event(
                        json!({"alias": "tor_occupancy", "kind": "miss_opcode", "home": "any"}),
                    ),
                    "pmus.cha.groups[0].events[0]: umask 0x03 of TOR_OCCUPANCY (event 0x36) needs a filter1 opcode".to_string(),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter1": {"opcodes": ["ItoM"]},
                            "events": [{"alias": "tor_inserts", "kind": "opcode", "home": "nid"}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[0]: umask 0x41 of TOR_INSERTS (event 0x35) needs a filter1 nid".to_string(),
                ),
                (
                    cha_event(json!({"alias": "tor_inserts", "kind": "eviction", "home": "nid"})),
                    "pmus.cha.groups[0].events[0]: umask 0x44 of TOR_INSERTS (event 0x35) needs a filter1 nid".to_string(),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter0": {"states": ["I"]},
                            "events": [{"alias": "llc_lookup", "op": "nid"}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[0]: umask 0x41 of LLC_LOOKUP (event 0x34) needs a filter1 nid".to_string(),
                ),
                (
                    cha_event(json!({"event": "0x37", "umask": "0x40"})),
                    "pmus.cha.groups[0].events[0]: umask 0x40 of LLC_VICTIMS (event 0x37) needs a filter1 nid".to_string(),
                ),
                (
                    cha_filters(json!({"filter1": {"opcodes": ["ItoM"]}})),
                    "pmus.cha.groups[0].filter1: sets an opcode, but no TOR event has an opcode umask (0x01)".to_string(),
                ),
                (
                    cha_event(json!({"alias": "llc_lookup", "op": "data_read"})),
                    "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without filter0 states".to_string(),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter0": {"tid": 1},
                            "events": [{"alias": "llc_lookup", "op": "data_read", "tid_en": true}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without filter0 states".to_string(),
                ),
                (
                    pmu(
                        "cha",
                        json!({
                            "filter0": {"states": ["M"]},
                            "events": [{"event": "0x34", "umask": "0x02"}]
                        }),
                    ),
                    "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without umask bit 0".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"tid": "0x1e"}})),
                    "pmus.cha.groups[0].filter0: sets a tid, but no event sets tid_en".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"raw": "0x1e"}})),
                    "pmus.cha.groups[0].filter0: sets a tid, but no event sets tid_en".to_string(),
                ),
                (
                    cha_event(json!({"alias": "clockticks", "tid_en": true})),
                    "pmus.cha.groups[0].events[0].tid_en: the group's filter0 sets no tid".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"tid": "0x20"}})),
                    "pmus.cha.groups[0].filter0.tid: must be at most 0x1f, got 0x20".to_string(),
                ),
                (
                    cha_filters(json!({"filter1": {"opcodes": ["ItoM", "RFO"]}})),
                    "pmus.cha.groups[0].filter1.opcodes: takes 1 opcode, got 2".to_string(),
                ),
                (
                    cha_filters(json!({"filter1": {"opcodes": []}})),
                    "pmus.cha.groups[0].filter1.opcodes: takes 1 opcode, got 0".to_string(),
                ),
                (
                    cha_filters(json!({"filter1": {"opcodes": ["0x200"]}})),
                    "pmus.cha.groups[0].filter1.opcodes[0]: opcodes are 9 bits, got 0x200".to_string(),
                ),
                (
                    cha_filters(json!({"filter1": {"opcodes": ["RdCur"]}})),
                    format!(
                        "pmus.cha.groups[0].filter1.opcodes[0]: unknown {name} opcode \"RdCur\" (opcodes: RFO, CRd, DRd,"
                    ),
                ),
                (
                    cha_filters(json!({"filter1": {"locality": "local"}})),
                    format!("pmus.cha.groups[0].filter1.locality: not supported on {name}"),
                ),
                (
                    cha_filters(json!({"filter1": {"raw": 0, "nid": 1}})),
                    "pmus.cha.groups[0].filter1: raw excludes opcodes, nid, nc and isoc".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"raw": 0, "tid": 1}})),
                    "pmus.cha.groups[0].filter0: raw excludes states and tid".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {}})),
                    "pmus.cha.groups[0].filter0: needs states, tid or raw".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"states": []}})),
                    "pmus.cha.groups[0].filter0.states: must not be empty".to_string(),
                ),
                (
                    cha_filters(json!({"filter0": {"states": ["SF_E"]}})),
                    "pmus.cha.groups[0].filter0.states[0]: unknown state \"SF_E\" (states: I, S, E, M, F)".to_string(),
                ),
                (
                    pmu("cha", json!({"fixed": true})),
                    format!(
                        "pmus.cha.groups[0].fixed: only IMC boxes have a fixed counter on {name}"
                    ),
                ),
                (
                    pmu(
                        "imc",
                        json!({"events": vec![json!({"alias": "rpq_inserts"}); 5]}),
                    ),
                    format!(
                        "pmus.imc.groups[0].events: {name} IMC boxes have 4 general counters, got 5 events"
                    ),
                ),
                (
                    pmu("imc", json!({"events": [{"event": "0x03"}]})),
                    format!("pmus.imc.groups[0].events[0]: Intel lists no {name} IMC event 0x03"),
                ),
                (
                    pmu("imc", json!({"events": [{"event": "0xff"}]})),
                    format!("pmus.imc.groups[0].events[0]: Intel lists no {name} IMC event 0xff"),
                ),
                (
                    pmu(
                        "imc",
                        json!({"filter0": {"states": ["M"]}, "events": [{"alias": "rpq_inserts"}]}),
                    ),
                    "pmus.imc.groups[0].filter0: only CBo groups take filters".to_string(),
                ),
                (
                    pmu("imc", json!({"filter1": {}, "fixed": true})),
                    "pmus.imc.groups[0].filter1: only CBo groups take filters".to_string(),
                ),
                (
                    pmu(
                        "imc",
                        json!({"events": [{"alias": "cas_count", "op": "rd", "tid_en": true}]}),
                    ),
                    "pmus.imc.groups[0].events[0].tid_en: only CBo events take tid_en".to_string(),
                ),
                (
                    pmu(
                        "imc",
                        json!({"events": [{"alias": "cas_count", "op": "rd", "ch_mask": 1}]}),
                    ),
                    format!("pmus.imc.groups[0].events[0].ch_mask: not supported on {name}"),
                ),
                (
                    pmu(
                        "imc",
                        json!({"events": [{"alias": "cas_count", "op": "rd_reg"}]}),
                    ),
                    "pmus.imc.groups[0].events[0].op: cas_count op must be one of rd, wr, all, got \"rd_reg\"".to_string(),
                ),
                (
                    pmu("imc", json!({"events": [{"alias": "cas_count"}]})),
                    "pmus.imc.groups[0].events[0]: cas_count needs a op (rd, wr, all)".to_string(),
                ),
                (
                    pmu("imc", json!({"events": [{"alias": "tor_inserts"}]})),
                    format!(
                        "pmus.imc.groups[0].events[0].alias: unknown {name} IMC alias \"tor_inserts\" (aliases: cas_count, rpq_inserts, rpq_cycles_ne,"
                    ),
                ),
                (
                    cha_event(json!({"alias": "clockticks", "ch_mask": 1})),
                    format!("pmus.cha.groups[0].events[0].ch_mask: not supported on {name}"),
                ),
                (
                    cha_event(json!({"alias": "tor_inserts", "source": "io", "result": "all"})),
                    "pmus.cha.groups[0].events[0].source: tor_inserts takes no source".to_string(),
                ),
                (
                    cha_event(json!({"alias": "sf_eviction", "state": "m"})),
                    format!(
                        "pmus.cha.groups[0].events[0].alias: unknown {name} CBo alias \"sf_eviction\" (aliases: clockticks, tor_inserts, tor_occupancy, llc_lookup, llc_victims)"
                    ),
                ),
                (
                    cha_event(json!({"alias": "llc_victims", "state": "f"})),
                    "pmus.cha.groups[0].events[0].state: llc_victims state must be one of m, e, s, miss, got \"f\"".to_string(),
                ),
            ];

            for (request, expected) in cases {
                let error = configs(model, request.clone()).unwrap_err();
                assert!(
                    error.starts_with(&expected),
                    "{model:?} {request}\nexpected: {expected}\n     got: {error}"
                );
            }
        }
    }

    #[test]
    fn rejects_what_one_model_lacks() {
        let cha_event = |event: Value| pmu("cha", json!({"events": [event]}));
        let cha_filters = |filters: Value| {
            let mut group = json!({"events": [{"alias": "clockticks"}]});
            group
                .as_object_mut()
                .unwrap()
                .extend(filters.as_object().unwrap().clone());
            pmu("cha", group)
        };
        let tor_all = json!({"alias": "tor_inserts", "kind": "eviction", "home": "any"});
        let cases = [
            (
                SNB,
                cha_filters(json!({"filter1": {"nc": true}})),
                "pmus.cha.groups[0].filter1.nc: not supported on Sandy Bridge-EP",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"isoc": true}})),
                "pmus.cha.groups[0].filter1.isoc: not supported on Sandy Bridge-EP",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"nid": "0x100"}})),
                "pmus.cha.groups[0].filter1.nid: must be at most 0xff, got 0x100",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"raw": "0x1"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xff83fc00, got 0x1",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"raw": "0x200"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xff83fc00, got 0x200",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"raw": "0x100000000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xff83fc00, got 0x100000000",
            ),
            (
                SNB,
                cha_filters(json!({"filter0": {"raw": "0x400"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0x7c001f, got 0x400",
            ),
            (
                SNB,
                cha_filters(json!({"filter0": {"raw": "0x20"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0x7c001f, got 0x20",
            ),
            (
                SNB,
                pmu(
                    "cha",
                    json!({"filter1": {"raw": "0xe4000000"}, "events": [tor_all.clone()]}),
                ),
                "pmus.cha.groups[0].filter1: sets an opcode, but no TOR event has an opcode umask (0x01)",
            ),
            (
                SNB,
                cha_filters(json!({"filter1": {"opcodes": ["PCIWiL"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: unknown Sandy Bridge-EP opcode \"PCIWiL\"",
            ),
            (
                SNB,
                cha_event(json!({"alias": "tor_inserts", "kind": "all", "home": "local"})),
                "pmus.cha.groups[0].events[0].home: tor_inserts home must be one of any, nid, got \"local\"",
            ),
            (
                SNB,
                cha_event(json!({"alias": "tor_inserts", "kind": "opcode"})),
                "pmus.cha.groups[0].events[0]: tor_inserts needs a home (any, nid)",
            ),
            (
                SNB,
                cha_event(json!({"alias": "tor_inserts", "kind": "all", "home": "any"})),
                "pmus.cha.groups[0].events[0]: Sandy Bridge-EP TOR_INSERTS (event 0x35) has no umask 0x08 (umasks: 0x01, 0x03, 0x04, 0x0a,",
            ),
            (
                SNB,
                cha_event(json!({"alias": "tor_occupancy", "kind": "wb", "home": "any"})),
                "pmus.cha.groups[0].events[0]: Sandy Bridge-EP TOR_OCCUPANCY (event 0x36) has no umask 0x10",
            ),
            (
                SNB,
                pmu(
                    "cha",
                    json!({"filter0": {"states": ["I"]}, "events": [{"alias": "llc_lookup", "op": "any"}]}),
                ),
                "pmus.cha.groups[0].events[0].op: llc_lookup op must be one of data_read, write, remote_snoop, nid, got \"any\"",
            ),
            (
                SNB,
                cha_event(json!({"event": "0x3c"})),
                "pmus.cha.groups[0].events[0]: Intel lists no Sandy Bridge-EP CBo event 0x3c",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"nid": "0x10000"}})),
                "pmus.cha.groups[0].filter1.nid: must be at most 0xffff, got 0x10000",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"raw": "0x20000000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x20000000",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"raw": "0x10000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x10000",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"raw": "0x100000000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x100000000",
            ),
            (
                IVB,
                cha_filters(json!({"filter0": {"raw": "0x20"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0x7e001f, got 0x20",
            ),
            (
                IVB,
                cha_filters(json!({"filter0": {"raw": "0x800000"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0x7e001f, got 0x800000",
            ),
            (
                IVB,
                pmu(
                    "cha",
                    json!({"filter1": {"raw": "0x1c800000"}, "events": [tor_all.clone()]}),
                ),
                "pmus.cha.groups[0].filter1: sets an opcode, but no TOR event has an opcode umask (0x01)",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"opcodes": ["PCIPRd"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: unknown Ivy Bridge-EP opcode \"PCIPRd\"",
            ),
            (
                IVB,
                cha_filters(json!({"filter1": {"opcodes": ["PCIWrUpdate"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: unknown Ivy Bridge-EP opcode \"PCIWrUpdate\"",
            ),
            (
                IVB,
                cha_event(json!({"alias": "tor_inserts", "kind": "opcode"})),
                "pmus.cha.groups[0].events[0]: tor_inserts needs a home (any, local, remote, nid)",
            ),
            (
                IVB,
                cha_event(json!({"alias": "tor_inserts", "kind": "miss_all", "home": "any"})),
                "pmus.cha.groups[0].events[0]: Ivy Bridge-EP TOR_INSERTS (event 0x35) has no umask 0x0a",
            ),
            (
                IVB,
                cha_event(json!({"alias": "tor_inserts", "kind": "eviction", "home": "local"})),
                "pmus.cha.groups[0].events[0]: Ivy Bridge-EP TOR_INSERTS (event 0x35) has no umask 0x24",
            ),
            (
                IVB,
                cha_event(json!({"event": "0x21"})),
                "pmus.cha.groups[0].events[0]: Intel lists no Ivy Bridge-EP CBo event 0x21",
            ),
            (
                IVB,
                pmu("imc", json!({"events": [{"alias": "rpq_cycles_full"}]})),
                "pmus.imc.groups[0].events[0].alias: unknown Ivy Bridge-EP IMC alias \"rpq_cycles_full\" (aliases: cas_count, rpq_inserts, rpq_cycles_ne, wpq_inserts, wpq_cycles_ne, wpq_cycles_full)",
            ),
            (
                IVB,
                pmu("imc", json!({"events": [{"alias": "wpq_occupancy"}]})),
                "pmus.imc.groups[0].events[0].alias: unknown Ivy Bridge-EP IMC alias \"wpq_occupancy\"",
            ),
            (
                IVB,
                pmu("imc", json!({"events": [{"event": "0x80"}]})),
                "pmus.imc.groups[0].events[0]: Intel lists no Ivy Bridge-EP IMC event 0x80",
            ),
            (
                SNB,
                pmu("imc", json!({"events": [{"event": "0x90"}]})),
                "pmus.imc.groups[0].events[0]: Intel lists no Sandy Bridge-EP IMC event 0x90",
            ),
        ];

        for (model, request, expected) in cases {
            let error = configs(model, request.clone()).unwrap_err();
            assert!(
                error.starts_with(expected),
                "{model:?} {request}\nexpected: {expected}\n     got: {error}"
            );
        }
    }

    #[test]
    fn config_id_is_stable_and_tracks_the_filter_layout() {
        let pcie_itom = json!({"cha": {"groups": [{
            "filter0": {"tid": "0x1e"},
            "filter1": {"opcodes": ["ItoM"]},
            "events": [
                {"alias": "tor_occupancy", "kind": "opcode", "home": "any", "tid_en": true},
                {"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true},
                {"alias": "clockticks"}
            ]
        }]}});
        let clockticks = json!({"cha": {"groups": [{"events": [{"alias": "clockticks"}]}]}});
        let config_id = |model, pmus: &Value| {
            let configs = configs(model, json!({"pmus": pmus})).unwrap();
            assert_eq!(configs.len(), 1);
            configs[0].config_id
        };

        let snb = config_id(SNB, &pcie_itom);
        let ivb = config_id(IVB, &pcie_itom);

        // The ids these configs had when they were introduced.
        assert_eq!(snb, 4_131_553_321_324_391);
        assert_eq!(ivb, 8_150_143_705_323_103);
        assert!(snb < ID_LIMIT && ivb < ID_LIMIT);
        // The same request fills different registers on the two models.
        assert_ne!(snb, ivb);
        // The id hashes the programming, not the model: groups without
        // filters program both models alike.
        assert_eq!(config_id(SNB, &clockticks), config_id(IVB, &clockticks));

        let imc = json!({"imc": {"groups": [{
            "events": [{"alias": "cas_count", "op": "rd"}, {"alias": "cas_count", "op": "wr"}],
            "fixed": true
        }]}});
        assert_eq!(config_id(SNB, &imc), 4_950_938_686_470_922);
        assert_eq!(config_id(IVB, &imc), config_id(SNB, &imc));
        // The same counters on another box type get another id.
        let raw = |box_type: &str| json!({box_type: {"groups": [{"events": [{"event": "0x00"}]}]}});
        assert_ne!(config_id(SNB, &raw("imc")), config_id(SNB, &raw("cha")));
    }

    #[test]
    fn tables_are_sorted_and_aliases_name_listed_events() {
        for snb_box in [
            &SANDY_BRIDGE.cbo,
            &SANDY_BRIDGE.imc,
            &IVY_BRIDGE.cbo,
            &IVY_BRIDGE.imc,
        ] {
            let name = format!("{} {}", snb_box.model, snb_box.name);
            assert!(
                snb_box
                    .event_counters
                    .windows(2)
                    .all(|pair| pair[0].0 < pair[1].0),
                "{name}"
            );
            assert!(
                snb_box.event_counters.iter().all(
                    |(_, counters)| *counters != 0 && counters >> snb_box.general_counters == 0
                ),
                "{name}"
            );
            for (index, alias) in snb_box.aliases.iter().enumerate() {
                assert!(
                    snb_box
                        .event_counters
                        .binary_search_by_key(&alias.event, |(event, _)| *event)
                        .is_ok(),
                    "{name} {}",
                    alias.name
                );
                assert!(
                    snb_box.aliases[..index]
                        .iter()
                        .all(|earlier| earlier.name != alias.name),
                    "{name} {}",
                    alias.name
                );
            }
        }
        for model in [SANDY_BRIDGE, IVY_BRIDGE] {
            for (index, (name, value)) in model.opcodes.iter().enumerate() {
                assert!(u64::from(*value) <= OPCODE_MAX && *value >= 0x180, "{name}");
                assert!(
                    model.opcodes[..index]
                        .iter()
                        .all(|(earlier, earlier_value)| earlier != name && earlier_value != value),
                    "{} {name}",
                    model.name
                );
            }
            let layout = &model.filters;
            if !layout.filter1_register {
                assert_eq!(layout.filter0_mask() & layout.filter1_mask(), 0);
            }
            assert_eq!(layout.state >> layout.state_shift & 0x1f, 0x1f);
            assert_eq!(layout.opcode >> layout.opcode_shift, OPCODE_MAX);
        }
        // Linux's snbep_cbox_filter_mask(0xf): tid, nid, state and opc.
        assert_eq!(SANDY_BRIDGE.filters.filter0_mask(), 0x007c_001f);
        assert_eq!(SANDY_BRIDGE.filters.filter1_mask(), 0xff83_fc00);
        assert_eq!(
            SANDY_BRIDGE.filters.filter0_mask() | SANDY_BRIDGE.filters.filter1_mask(),
            0xffff_fc1f
        );
        assert_eq!(IVY_BRIDGE.filters.filter0_mask(), 0x007e_001f);
        assert_eq!(IVY_BRIDGE.filters.filter1_mask(), 0xdff0_ffff);
    }
}
