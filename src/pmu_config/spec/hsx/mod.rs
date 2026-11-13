//! Haswell-EP and Broadwell-EP encodings of PMU config requests. The two
//! models program their CBo (`cha`), HA and IMC boxes alike.
//!
//! [V4] is Intel's Xeon E5 and E7 v4 uncore performance monitoring guide
//! (334291-001US); the JSON is the perfmon file `counters` comes from.

mod counters;

use crate::metal::arch::skx::pmon;
use crate::metrics::cha::ChaCacheState;
use crate::metrics::cha::hsx::hsx_llc_lookup_state_bits;
use crate::pmu_config::spec::{
    Alias, BoxSpec, Encoding, EventRequest, Filter0Request, Filter1Request, GroupProgram,
    GroupRequest, Number, Param, Token, check_counter, check_event_count, event_encoding, join,
};
use crate::pmu_config::types::{BoxType, ChaFilters, CounterSlot, EventLabels, ProgrammedCounter};

const MODEL: &str = "Haswell-EP/Broadwell-EP";

/// Cn_MSR_PMON_CTL ([V4] Table 2-16): ev_sel [7:0], umask [15:8], tid_en 19,
/// en 22, and bits 21:20 "Reserved; SW must write to 0". The HA and IMC
/// controls (Tables 2-64, 2-115) ignore bit 19 and have an overflow enable at
/// bit 20, which configs leave 0 on every box.
const COUNTER_TID_ENABLE: u64 = 1 << 19;
const COUNTER_ENABLE: u64 = 1 << 22;

/// Cn_MSR_PMON_BOX_FILTER0 ([V4] Table 2-18): tid [5:0], state [23:17]; the
/// other bits are reserved.
const FILTER0_TID_MASK: u64 = 0x3f;
const FILTER0_STATE_SHIFT: u32 = 17;
const FILTER0_STATE_MASK: u64 = 0x7f << FILTER0_STATE_SHIFT;
const FILTER0_MASK: u64 = FILTER0_STATE_MASK | FILTER0_TID_MASK;
/// Cn_MSR_PMON_BOX_FILTER1 ([V4] Table 2-19): nid [15:0], opc [28:20], nc 30,
/// isoc 31; bit 29 and bits 19:16 are reserved.
const FILTER1_NID_MASK: u64 = 0xffff;
const FILTER1_OPCODE_SHIFT: u32 = 20;
const FILTER1_OPCODE_MASK: u64 = OPCODE_MAX << FILTER1_OPCODE_SHIFT;
const FILTER1_NC_BIT: u64 = 1 << 30;
const FILTER1_ISOC_BIT: u64 = 1 << 31;
const FILTER1_MASK: u64 =
    FILTER1_ISOC_BIT | FILTER1_NC_BIT | FILTER1_OPCODE_MASK | FILTER1_NID_MASK;
const OPCODE_MAX: u64 = 0x1ff;

const LLC_LOOKUP_EVENT: u8 = 0x34;
const TOR_INSERTS_EVENT: u8 = 0x35;
const TOR_OCCUPANCY_EVENT: u8 = 0x36;
const LLC_VICTIMS_EVENT: u8 = 0x37;
/// Umask bits of TOR_INSERTS and TOR_OCCUPANCY that match filter1.opc and
/// filter1.nid ([V4] Tables 2-40, 2-41, "Filter Dep"), and the NID bit of
/// LLC_LOOKUP and LLC_VICTIMS (Tables 2-21, 2-22).
const TOR_OPCODE_UMASK: u8 = 0x01;
const NID_UMASK: u8 = 0x40;

/// The TOR_INSERTS umasks of [V4] Table 2-40 and the JSON: one base subevent,
/// OPCODE (0x01), EVICTION (0x04), ALL (0x08) or WB (0x10); MISS (0x02) only
/// with OPCODE or ALL; and at most one of LOCAL (0x20), NID (0x40) or REMOTE
/// (0x80), which neither lists with every base.
const TOR_INSERTS_UMASKS: &[u8] = &[
    0x01, 0x03, 0x04, 0x08, 0x10, 0x21, 0x23, 0x28, 0x2a, 0x41, 0x43, 0x44, 0x48, 0x4a, 0x50, 0x81,
    0x83, 0x88, 0x8a,
];
/// The TOR_OCCUPANCY umasks of [V4] Table 2-41 and the JSON: those of
/// TOR_INSERTS and MISS_ALL (0x0a).
const TOR_OCCUPANCY_UMASKS: &[u8] = &[
    0x01, 0x03, 0x04, 0x08, 0x0a, 0x10, 0x21, 0x23, 0x28, 0x2a, 0x41, 0x43, 0x44, 0x48, 0x4a, 0x50,
    0x81, 0x83, 0x88, 0x8a,
];

const CBO: BoxSpec = BoxSpec {
    model: MODEL,
    name: "CBo",
    general_counters: 4,
    event_counters: &counters::CBO,
    aliases: CBO_ALIASES,
};

const HA: BoxSpec = BoxSpec {
    model: MODEL,
    name: "HA",
    general_counters: 4,
    event_counters: &counters::HA,
    aliases: HA_ALIASES,
};

const IMC: BoxSpec = BoxSpec {
    model: MODEL,
    name: "IMC",
    general_counters: 4,
    event_counters: &counters::IMC,
    aliases: IMC_ALIASES,
};

// Encodings from [V4] and the JSON, which agree; the comments name the
// events and [V4] tables.

/// UNC_C_TOR_INSERTS.* and UNC_C_TOR_OCCUPANCY.* (Tables 2-40, 2-41): the
/// base subevent, then where the request's memory is homed. Not every pair is
/// valid; see `TOR_INSERTS_UMASKS`.
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
const TOR_HOMES: Param = Param {
    field: "home",
    values: &[
        ("any", 0x00),
        ("local", 0x20),
        ("remote", 0x80),
        ("nid", NID_UMASK),
    ],
};
/// UNC_C_LLC_LOOKUP.* (Table 2-21)
const LLC_LOOKUP_OPS: Param = Param {
    field: "op",
    values: &[
        ("data_read", 0x03),
        ("write", 0x05),
        ("remote_snoop", 0x09),
        ("any", 0x11),
        ("read", 0x21),
        ("nid", 0x41),
    ],
};
/// UNC_C_LLC_VICTIMS.* (Table 2-22). The BDX JSON names 0x04 I_STATE but
/// describes it as "Lines in S State"; [V4] and the HSX JSON call it S_STATE.
const LLC_VICTIM_STATES: Param = Param {
    field: "state",
    values: &[
        ("m", 0x01),
        ("e", 0x02),
        ("s", 0x04),
        ("f", 0x08),
        ("miss", 0x10),
    ],
};
/// UNC_C_TxR_INSERTS.* (Table 2-43)
const TXR_INSERT_RINGS: Param = Param {
    field: "ring",
    values: &[
        ("ad_cache", 0x01),
        ("ak_cache", 0x02),
        ("bl_cache", 0x04),
        ("iv_cache", 0x08),
        ("ad_core", 0x10),
        ("ak_core", 0x20),
        ("bl_core", 0x40),
    ],
};
/// UNC_C_MISC.* (Table 2-23)
const MISC_OPS: Param = Param {
    field: "op",
    values: &[
        ("rspi_was_fse", 0x01),
        ("wc_aliasing", 0x02),
        ("started", 0x04),
        ("rfo_hit_s", 0x08),
        ("cvzero_prefetch_victim", 0x10),
        ("cvzero_prefetch_miss", 0x20),
    ],
};

const CBO_ALIASES: &[Alias] = &[
    // UNC_C_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    Alias::new(
        "tor_inserts",
        TOR_INSERTS_EVENT,
        0x00,
        &[TOR_KINDS, TOR_HOMES],
    ),
    Alias::new(
        "tor_occupancy",
        TOR_OCCUPANCY_EVENT,
        0x00,
        &[TOR_KINDS, TOR_HOMES],
    ),
    Alias::new("llc_lookup", LLC_LOOKUP_EVENT, 0x00, &[LLC_LOOKUP_OPS]),
    Alias::new("llc_victims", LLC_VICTIMS_EVENT, 0x00, &[LLC_VICTIM_STATES]),
    Alias::new("txr_inserts", 0x02, 0x00, &[TXR_INSERT_RINGS]),
    Alias::new("misc", 0x39, 0x00, &[MISC_OPS]),
];

/// UNC_H_REQUESTS.* (Table 2-83)
const REQUEST_OPS: Param = Param {
    field: "op",
    values: &[
        ("reads_local", 0x01),
        ("reads_remote", 0x02),
        ("writes_local", 0x04),
        ("writes_remote", 0x08),
        ("invitoe_local", 0x10),
        ("invitoe_remote", 0x20),
        ("reads", 0x03),
        ("writes", 0x0c),
    ],
};
/// UNC_H_SNOOP_RESP.* (Table 2-95)
const SNOOP_RESP_OPS: Param = Param {
    field: "op",
    values: &[
        ("rspi", 0x01),
        ("rsps", 0x02),
        ("rspifwd", 0x04),
        ("rspsfwd", 0x08),
        ("rsp_wb", 0x10),
        ("rsp_fwd_wb", 0x20),
        ("rspcnflct", 0x40),
    ],
};
/// UNC_H_SNP_RESP_RECV_LOCAL.* (Table 2-96), named as SNOOP_RESP's.
const SNP_RESP_RECV_LOCAL_OPS: Param = Param {
    field: "op",
    values: &[
        ("rspi", 0x01),
        ("rsps", 0x02),
        ("rspifwd", 0x04),
        ("rspsfwd", 0x08),
        ("rsp_wb", 0x10),
        ("rsp_fwd_wb", 0x20),
        ("rspcnflct", 0x40),
        ("other", 0x80),
    ],
};
/// UNC_H_OSB.* (Table 2-81)
const OSB_OPS: Param = Param {
    field: "op",
    values: &[
        ("reads_local", 0x02),
        ("invitoe_local", 0x04),
        ("remote", 0x08),
        ("cancelled", 0x10),
        ("reads_local_useful", 0x20),
        ("remote_useful", 0x40),
    ],
};

const HA_ALIASES: &[Alias] = &[
    // UNC_H_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    Alias::new("requests", 0x01, 0x00, &[REQUEST_OPS]),
    Alias::new("snoop_resp", 0x21, 0x00, &[SNOOP_RESP_OPS]),
    Alias::new(
        "snp_resp_recv_local",
        0x60,
        0x00,
        &[SNP_RESP_RECV_LOCAL_OPS],
    ),
    Alias::new("osb", 0x53, 0x00, &[OSB_OPS]),
];

/// UNC_M_CAS_COUNT.* (Table 2-120)
const CAS_COUNT_OPS: Param = Param {
    field: "op",
    values: &[
        ("rd", 0x03),
        ("rd_reg", 0x01),
        ("rd_underfill", 0x02),
        ("wr", 0x0c),
        ("all", 0x0f),
    ],
};

// Neither model has WPQ_INSERTS (0x20), nor the occupancy events 0x80/0x81.
const IMC_ALIASES: &[Alias] = &[
    Alias::new("cas_count", 0x04, 0x00, &[CAS_COUNT_OPS]),
    // UNC_M_RPQ_INSERTS
    Alias::new("rpq_inserts", 0x10, 0x00, &[]),
    // UNC_M_RPQ_CYCLES_NE
    Alias::new("rpq_cycles_ne", 0x11, 0x00, &[]),
    // UNC_M_WPQ_CYCLES_NE
    Alias::new("wpq_cycles_ne", 0x21, 0x00, &[]),
];

/// Cn_MSR_PMON_BOX_FILTER0.state bits by name, after [V4] Table 2-18
/// (I 0x01, S 0x02, E 0x04, M 0x08, F 0x10), which the default CBo collector
/// programs too. The undocumented D (0x20) and M' (0x40) take a raw filter0.
const FILTER0_STATES: [(&str, ChaCacheState); 5] = [
    ("I", ChaCacheState::I),
    ("S", ChaCacheState::S),
    ("E", ChaCacheState::E),
    ("M", ChaCacheState::M),
    ("F", ChaCacheState::F),
];

/// IDI opcodes from [V4] Table 2-20, "Opcode Match by IDI Packet Type for
/// Cn_MSR_PMON_BOX_FILTER.opc", and WiL, which only the Haswell-EP guide's
/// table lists but Linux perf and PCM also match on Broadwell-EP.
const OPCODES: [(&str, u16); 20] = [
    ("RFO", 0x180),
    ("CRd", 0x181),
    ("DRd", 0x182),
    ("PRd", 0x187),
    ("WCiLF", 0x18c),
    ("WCiL", 0x18d),
    ("WiL", 0x18f),
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

pub(super) fn program_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    let hsx_box = match box_type {
        BoxType::Cha => &CBO,
        BoxType::Ha => &HA,
        BoxType::Imc => &IMC,
        BoxType::Iio | BoxType::Irp => {
            return Err(format!(
                "{path}: PMU configs do not support {} on {MODEL}",
                box_type.label()
            ));
        }
    };
    check_event_count(hsx_box, group, path)?;
    let filters = if box_type == BoxType::Cha {
        Some(cbo_filters(group, path)?)
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
            "{path}.fixed: only IMC boxes have a fixed counter on {MODEL}"
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
        let encoding = encoding(event, hsx_box, &event_path)?;
        check_counter(hsx_box, &encoding, slot, &event_path)?;
        if let Some(filters) = filters {
            check_cbo_event(&encoding, filters, &event_path)?;
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
            labels: labels(&encoding, event.tid_en, filters),
        });
    }
    if let Some(filters) = filters {
        if filters.tid && !tid_used {
            return Err(format!(
                "{path}.filter0: sets a tid, but no event sets tid_en"
            ));
        }
        // [V4] Table 2-19: filter1.opc matches TOR events with an opcode
        // umask; nothing else counts by it.
        if filters.opcode && !opcode_used {
            return Err(format!(
                "{path}.filter1: sets an opcode, but no TOR event has an opcode umask (0x01)"
            ));
        }
    }
    if group.fixed {
        // The default IMC collector's DCLK control,
        // pmon::FIXED_COUNTER_RESET_AND_ENABLE, without the reset bit.
        counters.push(ProgrammedCounter {
            slot: CounterSlot::Fixed,
            control: u64::from(pmon::FIXED_COUNTER_ENABLE),
            labels: EventLabels::from([("alias".to_string(), "dclk".to_string())]),
        });
    }

    Ok(GroupProgram {
        counters,
        filters: filters.map(|filters| filters.registers),
    })
}

fn encoding(event: &EventRequest, hsx_box: &BoxSpec, path: &str) -> Result<Encoding, String> {
    for (field, given) in [
        ("ch_mask", event.ch_mask.is_some()),
        ("fc_mask", event.fc_mask.is_some()),
    ] {
        if given {
            return Err(format!("{path}.{field}: not supported on {MODEL}"));
        }
    }

    event_encoding(hsx_box, event, path)
}

fn is_tor(event: u8) -> bool {
    event == TOR_INSERTS_EVENT || event == TOR_OCCUPANCY_EVENT
}

fn check_cbo_event(encoding: &Encoding, filters: CboFilters, path: &str) -> Result<(), String> {
    let (name, umasks) = match encoding.event {
        TOR_INSERTS_EVENT => ("TOR_INSERTS", TOR_INSERTS_UMASKS),
        TOR_OCCUPANCY_EVENT => ("TOR_OCCUPANCY", TOR_OCCUPANCY_UMASKS),
        LLC_LOOKUP_EVENT => ("LLC_LOOKUP", &[][..]),
        LLC_VICTIMS_EVENT => ("LLC_VICTIMS", &[][..]),
        _ => return Ok(()),
    };
    if is_tor(encoding.event) {
        if !umasks.contains(&encoding.umask) {
            return Err(format!(
                "{path}: {name} (event {:#04x}) has no umask {:#04x} (umasks: {})",
                encoding.event,
                encoding.umask,
                join(umasks.iter().map(|umask| format!("{umask:#04x}")))
            ));
        }
        // "Subevents that require an opcode or NID match require the
        // Cn_MSR_PMON_BOX_FILTER.{opc, nid} field to be set." ([V4]
        // TOR_INSERTS and TOR_OCCUPANCY)
        if encoding.umask & TOR_OPCODE_UMASK != 0 && !filters.opcode {
            return Err(format!(
                "{path}: umask {:#04x} of {name} (event {:#04x}) needs a filter1 opcode",
                encoding.umask, encoding.event
            ));
        }
    }
    if encoding.event == LLC_LOOKUP_EVENT {
        // "One must ALWAYS set umask bit 0 and select a state or states to
        // match. Otherwise, the event will count nothing." ([V4] LLC_LOOKUP)
        if encoding.umask & 0x01 == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without umask bit 0"
            ));
        }
        if filters.registers.filter0 & FILTER0_STATE_MASK == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without filter0 states"
            ));
        }
    }
    // The NID subevents match "the Target NID ... programmed in
    // Cn_MSR_PMON_BOX_FILTER.nid" ([V4] Tables 2-21, 2-22, 2-40, 2-41).
    if encoding.umask & NID_UMASK != 0 && !filters.nid {
        return Err(format!(
            "{path}: umask {:#04x} of {name} (event {:#04x}) needs a filter1 nid",
            encoding.umask, encoding.event
        ));
    }

    Ok(())
}

fn cbo_filters(group: &GroupRequest, path: &str) -> Result<CboFilters, String> {
    let (filter0, tid) = filter0(group.filter0.as_ref(), path)?;
    let (filter1, opcode, nid) = filter1(group.filter1.as_ref(), path)?;

    Ok(CboFilters {
        registers: ChaFilters { filter0, filter1 },
        tid,
        opcode,
        nid,
    })
}

/// The filter0 value, by default 0, and whether it sets a tid.
fn filter0(request: Option<&Filter0Request>, path: &str) -> Result<(u64, bool), String> {
    let Some(request) = request else {
        return Ok((0, false));
    };
    let path = format!("{path}.filter0");
    if let Some(raw) = &request.raw {
        if request.states.is_some() || request.tid.is_some() {
            return Err(format!("{path}: raw excludes states and tid"));
        }
        let value = masked_filter(raw, FILTER0_MASK, &format!("{path}.raw"))?;
        return Ok((value, value & FILTER0_TID_MASK != 0));
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
            value |= u64::from(hsx_llc_lookup_state_bits(state)) << FILTER0_STATE_SHIFT;
        }
    }
    if let Some(tid) = &request.tid {
        if tid.0 > FILTER0_TID_MASK {
            return Err(format!(
                "{path}.tid: must be at most {FILTER0_TID_MASK:#04x}, got {:#x}",
                tid.0
            ));
        }
        value |= tid.0;
    }

    Ok((value, request.tid.is_some()))
}

/// The filter1 value, by default 0, and whether it sets an opcode and a nid.
fn filter1(request: Option<&Filter1Request>, path: &str) -> Result<(u64, bool, bool), String> {
    let Some(request) = request else {
        return Ok((0, false, false));
    };
    let path = format!("{path}.filter1");
    if request.locality.is_some() {
        return Err(format!("{path}.locality: not supported on {MODEL}"));
    }
    if let Some(raw) = &request.raw {
        if request.opcodes.is_some() || request.nid.is_some() || request.nc || request.isoc {
            return Err(format!("{path}: raw excludes opcodes, nid, nc and isoc"));
        }
        let value = masked_filter(raw, FILTER1_MASK, &format!("{path}.raw"))?;
        return Ok((
            value,
            value & FILTER1_OPCODE_MASK != 0,
            value & FILTER1_NID_MASK != 0,
        ));
    }

    let mut value = 0;
    if let Some(tokens) = &request.opcodes {
        if tokens.len() != 1 {
            return Err(format!(
                "{path}.opcodes: takes 1 opcode, got {}",
                tokens.len()
            ));
        }
        let opcode = opcode(&tokens[0], &format!("{path}.opcodes[0]"))?;
        value |= u64::from(opcode) << FILTER1_OPCODE_SHIFT;
    }
    if let Some(nid) = &request.nid {
        if nid.0 > FILTER1_NID_MASK {
            return Err(format!(
                "{path}.nid: must be at most {FILTER1_NID_MASK:#06x}, got {:#x}",
                nid.0
            ));
        }
        value |= nid.0;
    }
    if request.nc {
        value |= FILTER1_NC_BIT;
    }
    if request.isoc {
        value |= FILTER1_ISOC_BIT;
    }

    Ok((value, request.opcodes.is_some(), request.nid.is_some()))
}

fn opcode(token: &Token, path: &str) -> Result<u16, String> {
    match token {
        Token::Integer(value) if *value <= OPCODE_MAX => Ok(*value as u16),
        Token::Integer(value) => Err(format!("{path}: opcodes are 9 bits, got {value:#x}")),
        Token::Name(name) => OPCODES
            .iter()
            .find(|(opcode, _)| opcode == name)
            .map(|(_, value)| *value)
            .ok_or_else(|| {
                format!(
                    "{path}: unknown {MODEL} opcode \"{name}\" (opcodes: {})",
                    join(OPCODES.iter().map(|(opcode, _)| opcode))
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

fn labels(encoding: &Encoding, tid_en: bool, filters: Option<CboFilters>) -> EventLabels {
    let mut labels = EventLabels::from([
        ("alias".to_string(), encoding.alias.to_string()),
        ("event".to_string(), format!("{:#04x}", encoding.event)),
        ("umask".to_string(), format!("{:#04x}", encoding.umask)),
    ]);
    if let Some(filters) = filters {
        let registers = filters.registers;
        labels.insert("filter0".to_string(), format!("{:#x}", registers.filter0));
        labels.insert("filter1".to_string(), format!("{:#x}", registers.filter1));
    }
    if tid_en {
        labels.insert("tid_en".to_string(), "1".to_string());
    }

    labels
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{Value, json};

    use super::*;
    use crate::arch::IntelServerCpuModel;
    use crate::pmu_config::spec::{ID_LIMIT, PmuConfigRequest, ProgrammedConfig, validate};
    use crate::pmu_config::types::{PmuTopology, ProgrammedGroup};

    const MODELS: [IntelServerCpuModel; 2] = [
        IntelServerCpuModel::HaswellXeon,
        IntelServerCpuModel::BroadwellXeon,
    ];

    fn topology() -> PmuTopology {
        let packages =
            |count: u16| BTreeMap::from([(0, (0..count).collect()), (1, (0..count).collect())]);

        PmuTopology {
            units: BTreeMap::from([
                (BoxType::Cha, packages(4)),
                (BoxType::Iio, packages(1)),
                (BoxType::Irp, packages(1)),
                (BoxType::Imc, packages(8)),
                (BoxType::Ha, packages(2)),
            ]),
        }
    }

    /// The configs of `request` on Haswell-EP, which Broadwell-EP must equal.
    fn configs(request: Value) -> Result<Vec<ProgrammedConfig>, String> {
        let [haswell, broadwell] = MODELS.map(|model| {
            let request: PmuConfigRequest =
                serde_json::from_value(request.clone()).map_err(|error| error.to_string())?;
            validate(request, &topology(), model)
        });
        assert_eq!(haswell, broadwell, "{request}");

        haswell
    }

    fn pmu(box_type: &str, group: Value) -> Value {
        json!({"pmus": {box_type: {"groups": [group]}}})
    }

    fn program(box_type: &str, group: Value) -> Result<ProgrammedGroup, String> {
        let mut configs = configs(pmu(box_type, group))?;
        Ok(configs.remove(0).program.groups.remove(0))
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

    #[test]
    fn programs_io_itom_tor_inserts() {
        let group = program(
            "cha",
            json!({
                "filter0": {"tid": "0x3e"},
                "filter1": {"opcodes": ["ItoM"]},
                "events": [{"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true}]
            }),
        )
        .unwrap();

        assert_eq!(
            group.filters,
            Some(ChaFilters {
                filter0: 0x3e,
                filter1: 0x1c80_0000,
            })
        );
        assert_eq!(controls(&group), [(CounterSlot::General(0), 0x48_0135)]);
        assert_eq!(
            group.counters[0].labels,
            labels(&[
                ("alias", "tor_inserts"),
                ("event", "0x35"),
                ("umask", "0x01"),
                ("filter0", "0x3e"),
                ("filter1", "0x1c800000"),
                ("tid_en", "1"),
            ])
        );
    }

    #[test]
    fn encodes_events_exactly() {
        let cases = [
            (
                "cha",
                json!({
                    "filter1": {"opcodes": ["DRd"]},
                    "events": [{"alias": "tor_occupancy", "kind": "miss_opcode", "home": "any"}]
                }),
                Some(ChaFilters {
                    filter0: 0,
                    filter1: 0x1820_0000,
                }),
                vec![(CounterSlot::General(0), 0x40_0336)],
            ),
            (
                "cha",
                json!({
                    "filter0": {"states": ["M"]},
                    "events": [{"alias": "llc_lookup", "op": "any"}]
                }),
                Some(ChaFilters {
                    filter0: 0x10_0000,
                    filter1: 0,
                }),
                vec![(CounterSlot::General(0), 0x40_1134)],
            ),
            (
                "cha",
                json!({"events": [{"alias": "txr_inserts", "ring": "iv_cache"}]}),
                Some(ChaFilters {
                    filter0: 0,
                    filter1: 0,
                }),
                vec![(CounterSlot::General(0), 0x40_0802)],
            ),
            (
                "ha",
                json!({"events": [{"alias": "snoop_resp", "op": "rspi"}]}),
                None,
                vec![(CounterSlot::General(0), 0x40_0121)],
            ),
            (
                "imc",
                json!({
                    "events": [{"alias": "cas_count", "op": "rd"}, {"alias": "cas_count", "op": "wr"}],
                    "fixed": true
                }),
                None,
                vec![
                    (CounterSlot::General(0), 0x40_0304),
                    (CounterSlot::General(1), 0x40_0c04),
                    // The IMC collector's FIXED_COUNTER_RESET_AND_ENABLE
                    // (0x480000) without the reset bit (1 << 19).
                    (CounterSlot::Fixed, 0x40_0000),
                ],
            ),
        ];

        for (box_type, request, filters, expected) in cases {
            let group = program(box_type, request.clone()).unwrap();

            assert_eq!(group.filters, filters, "{request}");
            assert_eq!(controls(&group), expected, "{request}");
        }
        let imc = program("imc", json!({"fixed": true})).unwrap();
        assert_eq!(imc.counters[0].labels, labels(&[("alias", "dclk")]));
        let ha = program("ha", json!({"events": [{"alias": "clockticks"}]})).unwrap();
        assert_eq!(
            ha.counters[0].labels,
            labels(&[
                ("alias", "clockticks"),
                ("event", "0x00"),
                ("umask", "0x00")
            ])
        );
    }

    /// The CBo collector's PCIe ItoM TOR groups,
    /// `HsxChaEventGroup::transaction(PciItoM, 0x1c8, true, miss)`
    /// (src/metrics/cha/hsx.rs:76-77, 778-815): filter0
    /// `HsxChaFilter0::thread_filter(true)` = CBO_PCIE_REQUEST_TID 0x3e (:39,
    /// :839-844); filter1 `HsxChaFilter1::opcode(0x1c8)` = 0x1c800000
    /// (:862-868); counter 0 TOR_OCCUPANCY and counter 1 TOR_INSERTS with
    /// umask 0x01 (0x03 for misses) and the thread filter, counter 2
    /// clockticks, each `counter_control(event, umask, thread_filter)`
    /// (:1646-1648) = 0x5a0136, 0x5a0135 (0x5a0336, 0x5a0335) and 0x520000.
    /// Config mode leaves out the reset bit (17), as on SKX, and the reserved
    /// bit 20 the collector sets.
    #[test]
    fn matches_the_cbo_collector_pcie_itom_tor_groups() {
        const COLLECTOR_ONLY_BITS: u64 = (1 << 17) | (1 << 20);

        for (kind, occupancy, inserts) in [
            ("opcode", 0x5a_0136_u64, 0x5a_0135_u64),
            ("miss_opcode", 0x5a_0336, 0x5a_0335),
        ] {
            let group = program(
                "cha",
                json!({
                    "filter0": {"tid": "0x3e"},
                    "filter1": {"opcodes": ["ItoM"]},
                    "events": [
                        {"alias": "tor_occupancy", "kind": kind, "home": "any", "tid_en": true},
                        {"alias": "tor_inserts", "kind": kind, "home": "any", "tid_en": true},
                        {"alias": "clockticks"}
                    ]
                }),
            )
            .unwrap();

            assert_eq!(
                group.filters,
                Some(ChaFilters {
                    filter0: 0x3e,
                    filter1: 0x1c80_0000,
                })
            );
            assert_eq!(
                controls(&group),
                [
                    (CounterSlot::General(0), occupancy & !COLLECTOR_ONLY_BITS),
                    (CounterSlot::General(1), inserts & !COLLECTOR_ONLY_BITS),
                    (CounterSlot::General(2), 0x52_0000 & !COLLECTOR_ONLY_BITS),
                ]
            );
            assert!(!group.counters[2].labels.contains_key("tid_en"));
        }
    }

    #[test]
    fn encodes_every_alias() {
        let cases = [
            ("cha", json!({"alias": "clockticks"}), 0x00, 0x00),
            (
                "cha",
                json!({"alias": "tor_inserts", "kind": "eviction", "home": "any"}),
                0x35,
                0x04,
            ),
            (
                "cha",
                json!({"alias": "tor_inserts", "kind": "all", "home": "local"}),
                0x35,
                0x28,
            ),
            (
                "cha",
                json!({"alias": "tor_inserts", "kind": "miss_all", "home": "remote"}),
                0x35,
                0x8a,
            ),
            (
                "cha",
                json!({"alias": "tor_inserts", "kind": "wb", "home": "any"}),
                0x35,
                0x10,
            ),
            (
                "cha",
                json!({"alias": "tor_occupancy", "kind": "miss_all", "home": "any"}),
                0x36,
                0x0a,
            ),
            (
                "cha",
                json!({"alias": "llc_victims", "state": "s"}),
                0x37,
                0x04,
            ),
            (
                "cha",
                json!({"alias": "llc_victims", "state": "miss"}),
                0x37,
                0x10,
            ),
            (
                "cha",
                json!({"alias": "txr_inserts", "ring": "ad_cache"}),
                0x02,
                0x01,
            ),
            (
                "cha",
                json!({"alias": "txr_inserts", "ring": "ak_core"}),
                0x02,
                0x20,
            ),
            (
                "cha",
                json!({"alias": "txr_inserts", "ring": "bl_core"}),
                0x02,
                0x40,
            ),
            (
                "cha",
                json!({"alias": "misc", "op": "rspi_was_fse"}),
                0x39,
                0x01,
            ),
            (
                "cha",
                json!({"alias": "misc", "op": "rfo_hit_s"}),
                0x39,
                0x08,
            ),
            (
                "cha",
                json!({"alias": "misc", "op": "cvzero_prefetch_victim"}),
                0x39,
                0x10,
            ),
            (
                "cha",
                json!({"alias": "misc", "op": "cvzero_prefetch_miss"}),
                0x39,
                0x20,
            ),
            ("ha", json!({"alias": "clockticks"}), 0x00, 0x00),
            (
                "ha",
                json!({"alias": "requests", "op": "reads"}),
                0x01,
                0x03,
            ),
            (
                "ha",
                json!({"alias": "requests", "op": "writes"}),
                0x01,
                0x0c,
            ),
            (
                "ha",
                json!({"alias": "requests", "op": "invitoe_local"}),
                0x01,
                0x10,
            ),
            (
                "ha",
                json!({"alias": "requests", "op": "invitoe_remote"}),
                0x01,
                0x20,
            ),
            (
                "ha",
                json!({"alias": "snoop_resp", "op": "rspcnflct"}),
                0x21,
                0x40,
            ),
            (
                "ha",
                json!({"alias": "snp_resp_recv_local", "op": "other"}),
                0x60,
                0x80,
            ),
            (
                "ha",
                json!({"alias": "osb", "op": "invitoe_local"}),
                0x53,
                0x04,
            ),
            (
                "ha",
                json!({"alias": "osb", "op": "remote_useful"}),
                0x53,
                0x40,
            ),
            (
                "imc",
                json!({"alias": "cas_count", "op": "rd_reg"}),
                0x04,
                0x01,
            ),
            (
                "imc",
                json!({"alias": "cas_count", "op": "rd_underfill"}),
                0x04,
                0x02,
            ),
            (
                "imc",
                json!({"alias": "cas_count", "op": "all"}),
                0x04,
                0x0f,
            ),
            ("imc", json!({"alias": "rpq_inserts"}), 0x10, 0x00),
            ("imc", json!({"alias": "rpq_cycles_ne"}), 0x11, 0x00),
            ("imc", json!({"alias": "wpq_cycles_ne"}), 0x21, 0x00),
        ];

        for (box_type, event, code, umask) in cases {
            let group = program(box_type, json!({"events": [event.clone()]})).unwrap();

            assert_eq!(
                controls(&group),
                [(
                    CounterSlot::General(0),
                    code | (umask << 8) | COUNTER_ENABLE
                )],
                "{event}"
            );
        }
        // NID subevents with their filter1 nid.
        for (event, umask) in [
            (
                json!({"alias": "tor_inserts", "kind": "opcode", "home": "nid"}),
                0x41,
            ),
            (
                json!({"alias": "tor_occupancy", "kind": "eviction", "home": "nid"}),
                0x44,
            ),
            (json!({"alias": "llc_lookup", "op": "nid"}), 0x41),
        ] {
            let group = program(
                "cha",
                json!({
                    "filter0": {"states": ["I"]},
                    "filter1": {"opcodes": ["DRd"], "nid": 1},
                    "events": [event.clone(), {"alias": "tor_inserts", "kind": "opcode", "home": "any"}]
                }),
            )
            .unwrap();

            assert_eq!(group.counters[0].labels["umask"], format!("{umask:#04x}"));
        }
    }

    #[test]
    fn encodes_cbo_filters() {
        let tor_tid =
            json!({"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true});
        let cases = [
            (json!({"events": [{"alias": "clockticks"}]}), 0, 0),
            (
                json!({"filter0": {"states": ["I", "S", "E", "M", "F"]}, "events": [{"alias": "clockticks"}]}),
                0x3e_0000,
                0,
            ),
            (
                json!({
                    "filter0": {"states": ["M"], "tid": 62},
                    "events": [{"alias": "llc_lookup", "op": "any", "tid_en": true}]
                }),
                0x10_003e,
                0,
            ),
            (
                json!({
                    "filter0": {"tid": 0},
                    "events": [{"alias": "clockticks", "tid_en": true}]
                }),
                0,
                0,
            ),
            (
                json!({
                    "filter0": {"raw": "0xfe003f"},
                    "filter1": {"opcodes": ["0x19e"]},
                    "events": [tor_tid.clone()]
                }),
                0xfe_003f,
                0x19e0_0000,
            ),
            (
                json!({"filter0": {"raw": "0x200000"}, "events": [{"alias": "clockticks"}]}),
                0x20_0000,
                0,
            ),
            (
                json!({"filter1": {"opcodes": ["ItoM"]}, "events": [{"event": "0x36", "umask": "0x01"}]}),
                0,
                0x1c80_0000,
            ),
            (
                json!({"filter1": {"nid": "0xffff", "nc": true, "isoc": true}, "events": [{"alias": "clockticks"}]}),
                0,
                0xc000_ffff,
            ),
            (
                json!({"filter1": {"opcodes": ["WiL"], "nc": true}, "events": [
                    {"alias": "tor_inserts", "kind": "opcode", "home": "any"}
                ]}),
                0,
                0x58f0_0000,
            ),
            (
                json!({
                    "filter0": {"tid": "0x3e"},
                    "filter1": {"raw": "0xdff0ffff"},
                    "events": [tor_tid.clone()]
                }),
                0x3e,
                0xdff0_ffff,
            ),
            (
                json!({"filter1": {}, "events": [{"alias": "clockticks"}]}),
                0,
                0,
            ),
        ];

        for (request, filter0, filter1) in cases {
            let group = program("cha", request.clone()).unwrap();

            assert_eq!(
                group.filters,
                Some(ChaFilters { filter0, filter1 }),
                "{request}"
            );
        }
    }

    /// Every listed TOR umask is one base subevent, MISS only with OPCODE or
    /// ALL, and at most one home; both tables come from [V4] and the JSON.
    #[test]
    fn tor_umasks_follow_the_subevent_rule() {
        for umask in TOR_INSERTS_UMASKS.iter().chain(TOR_OCCUPANCY_UMASKS) {
            let base = umask & 0x1d;
            assert_eq!(base.count_ones(), 1, "{umask:#04x}");
            assert!(
                umask & 0x02 == 0 || base == 0x01 || base == 0x08,
                "{umask:#04x}"
            );
            assert!((umask & 0xe0).count_ones() <= 1, "{umask:#04x}");
        }
        assert_eq!(TOR_INSERTS_UMASKS.len(), 19);
        assert!(
            TOR_INSERTS_UMASKS
                .iter()
                .all(|umask| TOR_OCCUPANCY_UMASKS.contains(umask))
        );
        assert_eq!(
            TOR_OCCUPANCY_UMASKS
                .iter()
                .filter(|umask| !TOR_INSERTS_UMASKS.contains(umask))
                .collect::<Vec<_>>(),
            [&0x0a]
        );
    }

    #[test]
    fn accepts_and_rejects_tor_umasks() {
        // filter1 sets an opcode and a nid, which every listed umask accepts.
        let tor = |event: &str, umask: u8| {
            program(
                "cha",
                json!({
                    "filter1": {"opcodes": ["DRd"], "nid": 1},
                    "events": [
                        {"event": event, "umask": umask},
                        {"alias": "tor_inserts", "kind": "opcode", "home": "any"}
                    ]
                }),
            )
        };

        for umask in 0..=u8::MAX {
            for (event, umasks) in [("0x35", TOR_INSERTS_UMASKS), ("0x36", TOR_OCCUPANCY_UMASKS)] {
                assert_eq!(
                    tor(event, umask).is_ok(),
                    umasks.contains(&umask),
                    "{event} {umask:#04x}"
                );
            }
        }
        for umask in [
            0x00, 0x02, 0x05, 0x06, 0x09, 0x12, 0x24, 0x30, 0x61, 0xa1, 0xc1,
        ] {
            assert!(tor("0x35", umask).is_err(), "{umask:#04x}");
        }
        assert!(tor("0x35", 0x0a).is_err());
        assert!(tor("0x36", 0x0a).is_ok());
    }

    #[test]
    fn accepts_shared_filters_and_unfiltered_tor_events() {
        // The one-filtered-event-per-box note of [V4] is left to the first
        // hardware run (docs/bdx-step0-checks.md item 7).
        let group = program(
            "cha",
            json!({
                "filter0": {"states": ["M"], "tid": "0x3e"},
                "filter1": {"opcodes": ["ItoM"]},
                "events": [
                    {"alias": "tor_occupancy", "kind": "opcode", "home": "any", "tid_en": true},
                    {"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true},
                    {"alias": "tor_inserts", "kind": "miss_opcode", "home": "local"},
                    {"alias": "llc_lookup", "op": "any"}
                ]
            }),
        )
        .unwrap();
        assert_eq!(group.counters.len(), 4);
        assert_eq!(group.counters[2].labels["umask"], "0x23");
        assert!(!group.counters[2].labels.contains_key("tid_en"));

        for event in [
            json!({"alias": "tor_inserts", "kind": "eviction", "home": "any"}),
            json!({"alias": "tor_occupancy", "kind": "all", "home": "remote"}),
            json!({"event": "0x35", "umask": "0x10"}),
        ] {
            program("cha", json!({"events": [event.clone()]})).unwrap_or_else(|error| {
                panic!("{event}: {error}");
            });
        }
    }

    #[test]
    fn rejects_invalid_requests() {
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
        let cases = [
            (
                pmu("iio", json!({"events": [{"event": 0}]})),
                "pmus.iio.groups[0]: PMU configs do not support iio on Haswell-EP/Broadwell-EP",
            ),
            (
                pmu("irp", json!({"events": [{"event": 0}]})),
                "pmus.irp.groups[0]: PMU configs do not support irp on Haswell-EP/Broadwell-EP",
            ),
            (
                pmu(
                    "cha",
                    json!({"events": vec![json!({"alias": "clockticks"}); 5]}),
                ),
                "pmus.cha.groups[0].events: Haswell-EP/Broadwell-EP CBo boxes have 4 general counters, got 5 events",
            ),
            (
                cha_event(json!({"event": "0x38"})),
                "pmus.cha.groups[0].events[0]: Intel lists no Haswell-EP/Broadwell-EP CBo event 0x38",
            ),
            (
                pmu("imc", json!({"events": [{"event": "0x20"}]})),
                "pmus.imc.groups[0].events[0]: Intel lists no Haswell-EP/Broadwell-EP IMC event 0x20",
            ),
            (
                pmu("ha", json!({"events": [{"event": "0xff"}]})),
                "pmus.ha.groups[0].events[0]: Intel lists no Haswell-EP/Broadwell-EP HA event 0xff",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter1": {"opcodes": ["ItoM"]},
                        "events": [tor_opcode.clone(), {"alias": "tor_occupancy", "kind": "opcode", "home": "any"}]
                    }),
                ),
                "pmus.cha.groups[0].events[1]: event 0x36 counts only on counters 0",
            ),
            (
                pmu("cha", json!({"events": [null, null, {"event": "0x09"}]})),
                "pmus.cha.groups[0].events[2]: event 0x09 counts only on counters 0, 1",
            ),
            (
                pmu(
                    "ha",
                    json!({"events": [null, null, null, {"event": "0x61"}]}),
                ),
                "pmus.ha.groups[0].events[3]: event 0x61 counts only on counters 0, 1, 2",
            ),
            (
                cha_event(tor_opcode.clone()),
                "pmus.cha.groups[0].events[0]: umask 0x01 of TOR_INSERTS (event 0x35) needs a filter1 opcode",
            ),
            (
                cha_event(
                    json!({"alias": "tor_occupancy", "kind": "miss_opcode", "home": "local"}),
                ),
                "pmus.cha.groups[0].events[0]: umask 0x23 of TOR_OCCUPANCY (event 0x36) needs a filter1 opcode",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter1": {"opcodes": ["ItoM"]},
                        "events": [{"alias": "tor_inserts", "kind": "opcode", "home": "nid"}]
                    }),
                ),
                "pmus.cha.groups[0].events[0]: umask 0x41 of TOR_INSERTS (event 0x35) needs a filter1 nid",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "kind": "all", "home": "nid"})),
                "pmus.cha.groups[0].events[0]: umask 0x48 of TOR_INSERTS (event 0x35) needs a filter1 nid",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter0": {"states": ["I"]},
                        "events": [{"alias": "llc_lookup", "op": "nid"}]
                    }),
                ),
                "pmus.cha.groups[0].events[0]: umask 0x41 of LLC_LOOKUP (event 0x34) needs a filter1 nid",
            ),
            (
                cha_event(json!({"event": "0x37", "umask": "0x40"})),
                "pmus.cha.groups[0].events[0]: umask 0x40 of LLC_VICTIMS (event 0x37) needs a filter1 nid",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["ItoM"]}})),
                "pmus.cha.groups[0].filter1: sets an opcode, but no TOR event has an opcode umask (0x01)",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter1": {"raw": "0x1c800000"},
                        "events": [{"alias": "tor_inserts", "kind": "all", "home": "any"}]
                    }),
                ),
                "pmus.cha.groups[0].filter1: sets an opcode, but no TOR event has an opcode umask (0x01)",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "kind": "eviction", "home": "local"})),
                "pmus.cha.groups[0].events[0]: TOR_INSERTS (event 0x35) has no umask 0x24 (umasks: 0x01, 0x03,",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "kind": "miss_all", "home": "any"})),
                "pmus.cha.groups[0].events[0]: TOR_INSERTS (event 0x35) has no umask 0x0a",
            ),
            (
                cha_event(json!({"event": "0x36", "umask": "0x05"})),
                "pmus.cha.groups[0].events[0]: TOR_OCCUPANCY (event 0x36) has no umask 0x05",
            ),
            (
                cha_event(json!({"alias": "llc_lookup", "op": "any"})),
                "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without filter0 states",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter0": {"tid": 1},
                        "events": [{"alias": "llc_lookup", "op": "any", "tid_en": true}]
                    }),
                ),
                "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without filter0 states",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter0": {"states": ["M"]},
                        "events": [{"event": "0x34", "umask": "0x02"}]
                    }),
                ),
                "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without umask bit 0",
            ),
            (
                cha_filters(json!({"filter0": {"tid": "0x3e"}})),
                "pmus.cha.groups[0].filter0: sets a tid, but no event sets tid_en",
            ),
            (
                cha_filters(json!({"filter0": {"raw": "0x3e"}})),
                "pmus.cha.groups[0].filter0: sets a tid, but no event sets tid_en",
            ),
            (
                cha_event(json!({"alias": "clockticks", "tid_en": true})),
                "pmus.cha.groups[0].events[0].tid_en: the group's filter0 sets no tid",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter0": {"states": ["M"]},
                        "events": [{"alias": "llc_lookup", "op": "any", "tid_en": true}]
                    }),
                ),
                "pmus.cha.groups[0].events[0].tid_en: the group's filter0 sets no tid",
            ),
            (
                pmu(
                    "ha",
                    json!({"events": [{"alias": "clockticks", "tid_en": true}]}),
                ),
                "pmus.ha.groups[0].events[0].tid_en: only CBo events take tid_en",
            ),
            (
                cha_filters(json!({"filter0": {"tid": "0x40"}})),
                "pmus.cha.groups[0].filter0.tid: must be at most 0x3f, got 0x40",
            ),
            (
                cha_filters(json!({"filter0": {"raw": "0x40"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0xfe003f, got 0x40",
            ),
            (
                cha_filters(json!({"filter0": {"raw": "0x1000000"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0xfe003f, got 0x1000000",
            ),
            (
                cha_filters(json!({"filter0": {"raw": "0x10000"}})),
                "pmus.cha.groups[0].filter0.raw: must fit the mask 0xfe003f, got 0x10000",
            ),
            (
                cha_filters(json!({"filter1": {"raw": "0x20000000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x20000000",
            ),
            (
                cha_filters(json!({"filter1": {"raw": "0x10000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x10000",
            ),
            (
                cha_filters(json!({"filter1": {"raw": "0x100000000"}})),
                "pmus.cha.groups[0].filter1.raw: must fit the mask 0xdff0ffff, got 0x100000000",
            ),
            (
                cha_filters(json!({"filter1": {"nid": "0x10000"}})),
                "pmus.cha.groups[0].filter1.nid: must be at most 0xffff, got 0x10000",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["ItoM", "RFO"]}})),
                "pmus.cha.groups[0].filter1.opcodes: takes 1 opcode, got 2",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": []}})),
                "pmus.cha.groups[0].filter1.opcodes: takes 1 opcode, got 0",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["0x200"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: opcodes are 9 bits, got 0x200",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["RdCur"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: unknown Haswell-EP/Broadwell-EP opcode \"RdCur\" (opcodes: RFO, CRd, DRd,",
            ),
            (
                cha_filters(json!({"filter1": {"locality": "local"}})),
                "pmus.cha.groups[0].filter1.locality: not supported on Haswell-EP/Broadwell-EP",
            ),
            (
                cha_filters(json!({"filter1": {"raw": 0, "nc": true}})),
                "pmus.cha.groups[0].filter1: raw excludes opcodes, nid, nc and isoc",
            ),
            (
                cha_filters(json!({"filter0": {"raw": 0, "tid": 1}})),
                "pmus.cha.groups[0].filter0: raw excludes states and tid",
            ),
            (
                cha_filters(json!({"filter0": {}})),
                "pmus.cha.groups[0].filter0: needs states, tid or raw",
            ),
            (
                cha_filters(json!({"filter0": {"states": []}})),
                "pmus.cha.groups[0].filter0.states: must not be empty",
            ),
            (
                cha_filters(json!({"filter0": {"states": ["SF_E"]}})),
                "pmus.cha.groups[0].filter0.states[0]: unknown state \"SF_E\" (states: I, S, E, M, F)",
            ),
            (
                pmu(
                    "ha",
                    json!({"filter0": {"states": ["M"]}, "events": [{"alias": "clockticks"}]}),
                ),
                "pmus.ha.groups[0].filter0: only CBo groups take filters",
            ),
            (
                pmu("imc", json!({"filter1": {}, "fixed": true})),
                "pmus.imc.groups[0].filter1: only CBo groups take filters",
            ),
            (
                pmu("ha", json!({"fixed": true})),
                "pmus.ha.groups[0].fixed: only IMC boxes have a fixed counter on Haswell-EP/Broadwell-EP",
            ),
            (
                cha_event(json!({"alias": "clockticks", "ch_mask": 1})),
                "pmus.cha.groups[0].events[0].ch_mask: not supported on Haswell-EP/Broadwell-EP",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "source": "io", "result": "all"})),
                "pmus.cha.groups[0].events[0].source: tor_inserts takes no source",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "kind": "opcode"})),
                "pmus.cha.groups[0].events[0]: tor_inserts needs a home (any, local, remote, nid)",
            ),
            (
                cha_event(json!({"alias": "sf_eviction", "state": "m"})),
                "pmus.cha.groups[0].events[0].alias: unknown Haswell-EP/Broadwell-EP CBo alias \"sf_eviction\" (aliases: clockticks, tor_inserts, tor_occupancy, llc_lookup, llc_victims, txr_inserts, misc)",
            ),
            (
                pmu("imc", json!({"events": [{"alias": "wpq_inserts"}]})),
                "pmus.imc.groups[0].events[0].alias: unknown Haswell-EP/Broadwell-EP IMC alias \"wpq_inserts\"",
            ),
            (
                pmu(
                    "ha",
                    json!({"events": [{"alias": "snoop_resp", "op": "other"}]}),
                ),
                "pmus.ha.groups[0].events[0].op: snoop_resp op must be one of rspi, rsps,",
            ),
        ];

        for (request, expected) in cases {
            let error = configs(request.clone()).unwrap_err();
            assert!(
                error.starts_with(expected),
                "{request}\nexpected: {expected}\n     got: {error}"
            );
        }
    }

    #[test]
    fn names_the_ha_box_type() {
        let request: PmuConfigRequest =
            serde_json::from_value(json!({"pmus": {"ha": {"groups": []}, "imc": {"groups": []}}}))
                .unwrap();

        assert_eq!(
            request.box_types(),
            BTreeSet::from([BoxType::Imc, BoxType::Ha])
        );
    }

    #[test]
    fn config_id_is_stable_and_per_box_type() {
        let cha = |tid_en: bool| {
            json!({"groups": [{
                "filter0": {"tid": "0x3e"},
                "filter1": {"opcodes": ["ItoM"]},
                "events": [
                    {"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": true},
                    {"alias": "tor_inserts", "kind": "opcode", "home": "any", "tid_en": tid_en}
                ]
            }]})
        };
        let ha = || json!({"groups": [{"events": [{"alias": "snoop_resp", "op": "rspi"}]}]});
        let config_id = |pmus: Value, box_type: BoxType| {
            configs(json!({"pmus": pmus}))
                .unwrap()
                .into_iter()
                .find(|config| config.program.box_type == box_type)
                .unwrap()
                .config_id
        };
        let base = config_id(json!({"cha": cha(false)}), BoxType::Cha);

        assert!(base < ID_LIMIT);
        assert_eq!(config_id(json!({"cha": cha(false)}), BoxType::Cha), base);
        let all = json!({"cha": cha(false), "ha": ha(), "imc": {"groups": [{"fixed": true}]}});
        assert_eq!(config_id(all.clone(), BoxType::Cha), base);
        assert_eq!(
            config_id(all, BoxType::Ha),
            config_id(json!({"ha": ha()}), BoxType::Ha)
        );
        assert_ne!(config_id(json!({"cha": cha(true)}), BoxType::Cha), base);
        // The same programming on HA and IMC boxes differs by box type.
        let raw = || json!({"groups": [{"events": [{"event": "0x01", "umask": "0x03"}]}]});
        assert_ne!(
            config_id(json!({"ha": raw()}), BoxType::Ha),
            config_id(json!({"imc": raw()}), BoxType::Imc)
        );
    }

    #[test]
    fn tables_are_sorted_and_aliases_name_listed_events() {
        for hsx_box in [CBO, HA, IMC] {
            assert!(
                hsx_box
                    .event_counters
                    .windows(2)
                    .all(|pair| pair[0].0 < pair[1].0),
                "{}",
                hsx_box.name
            );
            assert!(
                hsx_box.event_counters.iter().all(
                    |(_, counters)| *counters != 0 && counters >> hsx_box.general_counters == 0
                ),
                "{}",
                hsx_box.name
            );
            for (index, alias) in hsx_box.aliases.iter().enumerate() {
                assert!(
                    hsx_box
                        .event_counters
                        .binary_search_by_key(&alias.event, |(event, _)| *event)
                        .is_ok(),
                    "{} {}",
                    hsx_box.name,
                    alias.name
                );
                assert!(
                    hsx_box.aliases[..index]
                        .iter()
                        .all(|earlier| earlier.name != alias.name),
                    "{} {}",
                    hsx_box.name,
                    alias.name
                );
            }
        }
        for (index, (name, value)) in OPCODES.iter().enumerate() {
            assert!(u64::from(*value) <= OPCODE_MAX && *value >= 0x180, "{name}");
            assert!(
                OPCODES[..index]
                    .iter()
                    .all(|(earlier, earlier_value)| earlier != name && earlier_value != value),
                "{name}"
            );
        }
        assert_eq!(FILTER0_MASK, 0x00fe_003f);
        assert_eq!(FILTER1_MASK, 0xdff0_ffff);
    }
}
