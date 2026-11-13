//! Skylake-SP and Cascade Lake encodings of PMU config requests.

mod counters;

use crate::metal::arch::skx::pmon;
use crate::metrics::cha::ChaCacheState;
use crate::metrics::cha::skx::{
    CHA_FILTER0_STATE_SHIFT, CHA_FILTER1_ALL_OPCODE_BIT, CHA_FILTER1_LOCAL_BIT,
    CHA_FILTER1_NEAR_MEMORY_BIT, CHA_FILTER1_NOT_NEAR_MEMORY_BIT, CHA_FILTER1_OPCODE0_SHIFT,
    CHA_FILTER1_OPCODE1_SHIFT, CHA_FILTER1_REMOTE_BIT,
};
use crate::pmu_config::spec::{
    EventRequest, Filter0Request, Filter1Request, GroupProgram, GroupRequest, LocalityRequest,
    Number, Token, join,
};
use crate::pmu_config::types::{BoxType, ChaFilters, CounterSlot, EventLabels, ProgrammedCounter};

const FILTER0_STATE_MASK: u64 = 0x3ff << CHA_FILTER0_STATE_SHIFT;
const FILTER_REGISTER_MAX: u64 = 0xffff_ffff;
const IDI_OPCODE_MAX: u64 = 0x3ff;
const IIO_FC_MASK_MAX: u8 = 0x07;
const LLC_LOOKUP_EVENT: u8 = 0x34;

/// A box type as Skylake-SP implements it.
struct SkxBox {
    /// Box name in messages.
    name: &'static str,
    general_counters: usize,
    /// Event code → mask of the general counters it counts on.
    event_counters: &'static [(u8, u8)],
    aliases: &'static [Alias],
}

const CHA: SkxBox = SkxBox {
    name: "CHA",
    general_counters: 4,
    event_counters: &counters::CHA,
    aliases: CHA_ALIASES,
};

const IIO: SkxBox = SkxBox {
    name: "IIO",
    general_counters: 4,
    event_counters: &counters::IIO,
    aliases: IIO_ALIASES,
};

const IRP: SkxBox = SkxBox {
    name: "IRP",
    general_counters: 2,
    event_counters: &counters::IRP,
    aliases: IRP_ALIASES,
};

const IMC: SkxBox = SkxBox {
    name: "IMC",
    general_counters: 4,
    event_counters: &counters::IMC,
    aliases: IMC_ALIASES,
};

/// An event by name. Its umask is `umask` or'd with the bits of each
/// parameter's value.
struct Alias {
    name: &'static str,
    event: u8,
    umask: u8,
    params: &'static [Param],
    /// IIO defaults, which the request may override.
    ch_mask: u8,
    fc_mask: u8,
}

impl Alias {
    const fn new(name: &'static str, event: u8, umask: u8, params: &'static [Param]) -> Self {
        Self {
            name,
            event,
            umask,
            params,
            ch_mask: 0,
            fc_mask: 0,
        }
    }
}

/// An alias parameter: its request field and the umask bits of its values.
struct Param {
    field: &'static str,
    values: &'static [(&'static str, u8)],
}

// Encodings from Intel perfmon SKX/events/skylakex_uncore*.json (see
// `counters`); the comments name the events.

/// UNC_CHA_TOR_INSERTS.* and UNC_CHA_TOR_OCCUPANCY.*: IRQ (0x01) for IA,
/// PRQ (0x04) for IO; all is ALL_IO_IA for inserts and ALL_FROM_LOC, which
/// adds EVICT (0x02), for occupancy.
const TOR_INSERT_SOURCES: Param = Param {
    field: "source",
    values: &[("ia", 0x01), ("io", 0x04), ("all", 0x05)],
};
const TOR_OCCUPANCY_SOURCES: Param = Param {
    field: "source",
    values: &[("ia", 0x01), ("io", 0x04), ("all", 0x07)],
};
const TOR_RESULTS: Param = Param {
    field: "result",
    values: &[("all", 0x30), ("hit", 0x10), ("miss", 0x20)],
};
/// UNC_CHA_LLC_LOOKUP.*
const LLC_LOOKUP_OPS: Param = Param {
    field: "op",
    values: &[
        ("any", 0x11),
        ("data_read", 0x03),
        ("write", 0x05),
        ("remote_snoop", 0x09),
        ("local", 0x31),
        ("remote", 0x91),
    ],
};
/// UNC_CHA_LLC_VICTIMS.*
const LLC_VICTIM_STATES: Param = Param {
    field: "state",
    values: &[
        ("total_m", 0x01),
        ("total_e", 0x02),
        ("total_s", 0x04),
        ("total_f", 0x08),
        ("local_m", 0x21),
        ("local_e", 0x22),
        ("local_s", 0x24),
        ("local_f", 0x28),
        ("local_all", 0x2f),
        ("remote_m", 0x81),
        ("remote_e", 0x82),
        ("remote_s", 0x84),
        ("remote_f", 0x88),
        ("remote_all", 0x8f),
    ],
};
/// UNC_CHA_SF_EVICTION.*_STATE
const SF_EVICTION_STATES: Param = Param {
    field: "state",
    values: &[("m", 0x01), ("e", 0x02), ("s", 0x04)],
};

const CHA_ALIASES: &[Alias] = &[
    // UNC_CHA_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    Alias::new(
        "tor_inserts",
        0x35,
        0x00,
        &[TOR_INSERT_SOURCES, TOR_RESULTS],
    ),
    Alias::new(
        "tor_occupancy",
        0x36,
        0x00,
        &[TOR_OCCUPANCY_SOURCES, TOR_RESULTS],
    ),
    Alias::new("llc_lookup", LLC_LOOKUP_EVENT, 0x00, &[LLC_LOOKUP_OPS]),
    Alias::new("llc_victims", 0x37, 0x00, &[LLC_VICTIM_STATES]),
    Alias::new("sf_eviction", 0x3d, 0x00, &[SF_EVICTION_STATES]),
];

/// PCIe partitions 0-3 of a stack, as UNC_IIO_COMP_BUF_INSERTS.CMPD.ALL_PARTS
/// selects them. Intel lists DATA_REQ_*_CPU per partition only, and its IO
/// bandwidth metrics sum PART0-PART3.
const IIO_ALL_PARTS: u8 = 0x0f;
/// UNC_IIO_DATA_REQ_OF_CPU.*.PART*
const DATA_REQ_OF_CPU_OPS: Param = Param {
    field: "op",
    values: &[
        ("mem_write", 0x01),
        ("peer_write", 0x02),
        ("mem_read", 0x04),
        ("peer_read", 0x08),
        ("atomic", 0x10),
        ("atomiccmp", 0x20),
        ("msg", 0x40),
    ],
};
/// UNC_IIO_DATA_REQ_BY_CPU.*.PART*
const DATA_REQ_BY_CPU_OPS: Param = Param {
    field: "op",
    values: &[
        ("mem_write", 0x01),
        ("peer_write", 0x02),
        ("mem_read", 0x04),
        ("peer_read", 0x08),
        ("cfg_write", 0x10),
        ("io_write", 0x20),
        ("cfg_read", 0x40),
        ("io_read", 0x80),
    ],
};

const IIO_ALIASES: &[Alias] = &[
    // UNC_IIO_CLOCKTICKS
    Alias::new("clockticks", 0x01, 0x00, &[]),
    // UNC_IIO_COMP_BUF_INSERTS.CMPD.ALL_PARTS
    Alias {
        ch_mask: IIO_ALL_PARTS,
        fc_mask: 0x04,
        ..Alias::new("comp_buf_inserts", 0xc2, 0x03, &[])
    },
    // UNC_IIO_COMP_BUF_OCCUPANCY.CMPD.ALL_PARTS
    Alias {
        fc_mask: 0x04,
        ..Alias::new("comp_buf_occupancy", 0xd5, 0x0f, &[])
    },
    Alias {
        ch_mask: IIO_ALL_PARTS,
        fc_mask: 0x07,
        ..Alias::new("data_req_of_cpu", 0x83, 0x00, &[DATA_REQ_OF_CPU_OPS])
    },
    Alias {
        ch_mask: IIO_ALL_PARTS,
        fc_mask: 0x07,
        ..Alias::new("data_req_by_cpu", 0xc0, 0x00, &[DATA_REQ_BY_CPU_OPS])
    },
];

/// UNC_I_COHERENT_OPS.*
const COHERENT_OPS: Param = Param {
    field: "op",
    values: &[
        ("pcirdcur", 0x01),
        ("crd", 0x02),
        ("drd", 0x04),
        ("rfo", 0x08),
        ("pcitom", 0x10),
        ("pcidcahint", 0x20),
        ("wbmtoi", 0x40),
        ("clflush", 0x80),
    ],
};

const IRP_ALIASES: &[Alias] = &[
    // UNC_I_CLOCKTICKS
    Alias::new("clockticks", 0x01, 0x00, &[]),
    // UNC_I_FAF_INSERTS
    Alias::new("faf_inserts", 0x18, 0x00, &[]),
    // UNC_I_FAF_OCCUPANCY
    Alias::new("faf_occupancy", 0x19, 0x00, &[]),
    Alias::new("coherent_ops", 0x10, 0x00, &[COHERENT_OPS]),
];

/// UNC_M_CAS_COUNT.RD, .WR and .ALL
const CAS_COUNT_OPS: Param = Param {
    field: "op",
    values: &[("rd", 0x03), ("wr", 0x0c), ("all", 0x0f)],
};

const IMC_ALIASES: &[Alias] = &[
    Alias::new("cas_count", 0x04, 0x00, &[CAS_COUNT_OPS]),
    // UNC_M_RPQ_INSERTS
    Alias::new("rpq_inserts", 0x10, 0x00, &[]),
    // UNC_M_RPQ_OCCUPANCY
    Alias::new("rpq_occupancy", 0x80, 0x00, &[]),
    // UNC_M_WPQ_INSERTS
    Alias::new("wpq_inserts", 0x20, 0x00, &[]),
    // UNC_M_WPQ_OCCUPANCY
    Alias::new("wpq_occupancy", 0x81, 0x00, &[]),
];

/// Cn_MSR_PMON_BOX_FILTER0.state bits by name, after the Skylake-SP uncore
/// guide's FILTER0 field definitions (table 2-54).
const FILTER0_STATES: [(&str, ChaCacheState); 8] = [
    ("I", ChaCacheState::I),
    ("SF_S", ChaCacheState::SfS),
    ("SF_E", ChaCacheState::SfE),
    ("SF_H", ChaCacheState::SfH),
    ("S", ChaCacheState::S),
    ("E", ChaCacheState::E),
    ("M", ChaCacheState::M),
    ("F", ChaCacheState::F),
];

/// IDI opcodes of the IRQ, from the Skylake-SP uncore guide's table 3-1,
/// "Opcode Match by IDI Packet Type (relevant to IRQ) for
/// Cn_MSR_PMON_BOX_FILTER.opc". Opcodes of the other queues are raw values.
const IDI_OPCODES: [(&str, u16); 23] = [
    ("RFO", 0x200),
    ("CRd", 0x201),
    ("DRd", 0x202),
    ("PRd", 0x207),
    ("WCiLF", 0x20c),
    ("WCiL", 0x20d),
    ("UCRdF", 0x20e),
    ("WiL", 0x20f),
    ("RdCur", 0x21e),
    ("WbPushHint", 0x243),
    ("WbMtoI", 0x244),
    ("WbMtoE", 0x245),
    ("WbEFtoI", 0x246),
    ("WbEFtoE", 0x247),
    ("ItoM", 0x248),
    ("LlcPrefRFO", 0x258),
    ("LlcPrefCode", 0x259),
    ("LlcPrefData", 0x25a),
    ("IntLog", 0x279),
    ("IntPhy", 0x27a),
    ("IntPriUp", 0x27b),
    ("SplitLock", 0x27e),
    ("Lock", 0x27f),
];

/// The register fields of one counter's event.
struct Encoding {
    alias: &'static str,
    event: u8,
    umask: u8,
    ch_mask: u8,
    fc_mask: u8,
}

pub(super) fn program_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    let skx_box = match box_type {
        BoxType::Cha => &CHA,
        BoxType::Iio => &IIO,
        BoxType::Irp => &IRP,
        BoxType::Imc => &IMC,
        BoxType::Ha => return Err(format!("{path}: Skylake-SP has no ha boxes")),
    };
    if group.events.len() > skx_box.general_counters {
        return Err(format!(
            "{path}.events: Skylake-SP {} boxes have {} general counters, got {} events",
            skx_box.name,
            skx_box.general_counters,
            group.events.len()
        ));
    }
    let filters = if box_type == BoxType::Cha {
        Some(ChaFilters {
            filter0: filter0(group.filter0.as_ref(), path)?,
            filter1: filter1(group.filter1.as_ref(), path)?,
        })
    } else {
        for (field, given) in [
            ("filter0", group.filter0.is_some()),
            ("filter1", group.filter1.is_some()),
        ] {
            if given {
                return Err(format!("{path}.{field}: only CHA groups take filters"));
            }
        }
        None
    };
    if group.fixed && box_type != BoxType::Imc {
        return Err(format!(
            "{path}.fixed: only IMC boxes have a fixed counter on Skylake-SP"
        ));
    }

    let mut counters = Vec::new();
    for (slot, event) in group.events.iter().enumerate() {
        let Some(event) = event else {
            continue;
        };
        let event_path = format!("{path}.events[{slot}]");
        let encoding = encoding(skx_box, box_type, event, &event_path)?;
        check_counter(skx_box, &encoding, slot, &event_path)?;
        if let Some(filters) = filters {
            check_cha_filters(&encoding, filters, &event_path)?;
        }
        counters.push(ProgrammedCounter {
            slot: CounterSlot::General(slot as u8),
            control: control(box_type, &encoding),
            labels: labels(box_type, &encoding, filters),
        });
    }
    if group.fixed {
        counters.push(ProgrammedCounter {
            slot: CounterSlot::Fixed,
            control: u64::from(pmon::FIXED_COUNTER_ENABLE),
            labels: EventLabels::from([("alias".to_string(), "dclk".to_string())]),
        });
    }

    Ok(GroupProgram { counters, filters })
}

fn encoding(
    skx_box: &SkxBox,
    box_type: BoxType,
    event: &EventRequest,
    path: &str,
) -> Result<Encoding, String> {
    if box_type != BoxType::Iio {
        for (field, given) in [
            ("ch_mask", event.ch_mask.is_some()),
            ("fc_mask", event.fc_mask.is_some()),
        ] {
            if given {
                return Err(format!("{path}.{field}: only IIO events take a {field}"));
            }
        }
    }

    let mut encoding = match (&event.alias, &event.event) {
        (Some(alias), None) => alias_encoding(skx_box, alias, event, path)?,
        (None, Some(code)) => raw_encoding(code, event, path)?,
        (Some(_), Some(_)) => {
            return Err(format!("{path}: takes an alias or an event, not both"));
        }
        (None, None) => return Err(format!("{path}: needs an alias or an event")),
    };
    if let Some(ch_mask) = &event.ch_mask {
        encoding.ch_mask = register_field(ch_mask, u8::MAX, &format!("{path}.ch_mask"))?;
    }
    if let Some(fc_mask) = &event.fc_mask {
        encoding.fc_mask = register_field(fc_mask, IIO_FC_MASK_MAX, &format!("{path}.fc_mask"))?;
    }

    Ok(encoding)
}

fn alias_encoding(
    skx_box: &SkxBox,
    name: &str,
    event: &EventRequest,
    path: &str,
) -> Result<Encoding, String> {
    let alias = skx_box
        .aliases
        .iter()
        .find(|alias| alias.name == name)
        .ok_or_else(|| {
            format!(
                "{path}.alias: unknown Skylake-SP {} alias \"{name}\" (aliases: {})",
                skx_box.name,
                join(skx_box.aliases.iter().map(|alias| alias.name))
            )
        })?;
    if event.umask.is_some() {
        return Err(format!("{path}.umask: alias {name} sets its own umask"));
    }

    let mut umask = alias.umask;
    for (field, value) in event.params() {
        let param = alias.params.iter().find(|param| param.field == field);
        match (param, value) {
            (None, None) => {}
            (None, Some(_)) => return Err(format!("{path}.{field}: {name} takes no {field}")),
            (Some(param), None) => {
                return Err(format!(
                    "{path}: {name} needs a {field} ({})",
                    join(param.values.iter().map(|(value, _)| value))
                ));
            }
            (Some(param), Some(value)) => {
                umask |= param
                    .values
                    .iter()
                    .find(|(name, _)| *name == value)
                    .map(|(_, bits)| *bits)
                    .ok_or_else(|| {
                        format!(
                            "{path}.{field}: {name} {field} must be one of {}, got \"{value}\"",
                            join(param.values.iter().map(|(value, _)| value))
                        )
                    })?;
            }
        }
    }

    Ok(Encoding {
        alias: alias.name,
        event: alias.event,
        umask,
        ch_mask: alias.ch_mask,
        fc_mask: alias.fc_mask,
    })
}

fn raw_encoding(code: &Number, event: &EventRequest, path: &str) -> Result<Encoding, String> {
    if let Some((field, _)) = event
        .params()
        .into_iter()
        .find(|(_, value)| value.is_some())
    {
        return Err(format!("{path}.{field}: only aliases take a {field}"));
    }
    let umask = match &event.umask {
        Some(umask) => register_field(umask, u8::MAX, &format!("{path}.umask"))?,
        None => 0,
    };

    Ok(Encoding {
        alias: "raw",
        event: register_field(code, u8::MAX, &format!("{path}.event"))?,
        umask,
        ch_mask: 0,
        fc_mask: 0,
    })
}

fn register_field(value: &Number, max: u8, path: &str) -> Result<u8, String> {
    u8::try_from(value.0)
        .ok()
        .filter(|field| *field <= max)
        .ok_or_else(|| format!("{path}: must be at most {max:#04x}, got {:#x}", value.0))
}

fn check_counter(
    skx_box: &SkxBox,
    encoding: &Encoding,
    slot: usize,
    path: &str,
) -> Result<(), String> {
    let counters = skx_box
        .event_counters
        .binary_search_by_key(&encoding.event, |(event, _)| *event)
        .map(|index| skx_box.event_counters[index].1)
        .map_err(|_| {
            format!(
                "{path}: Intel lists no Skylake-SP {} event {:#04x}",
                skx_box.name, encoding.event
            )
        })?;
    if counters & (1 << slot) == 0 {
        return Err(format!(
            "{path}: event {:#04x} counts only on counters {}",
            encoding.event,
            join((0..8).filter(|counter| counters & (1 << counter) != 0))
        ));
    }

    Ok(())
}

fn check_cha_filters(encoding: &Encoding, filters: ChaFilters, path: &str) -> Result<(), String> {
    // LLC_LOOKUP: "One must ALWAYS set umask bit 0 and select a state or
    // states to match. Otherwise, the event will count nothing." (Skylake-SP
    // uncore guide, CHA event list)
    if encoding.event == LLC_LOOKUP_EVENT {
        if encoding.umask & 0x01 == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without umask bit 0"
            ));
        }
        if filters.filter0 & FILTER0_STATE_MASK == 0 {
            return Err(format!(
                "{path}: LLC_LOOKUP (event 0x34) counts nothing without filter0 states"
            ));
        }
    }

    Ok(())
}

fn filter0(request: Option<&Filter0Request>, path: &str) -> Result<u64, String> {
    let Some(request) = request else {
        return Ok(0);
    };
    let path = format!("{path}.filter0");
    let states = match (&request.states, &request.raw) {
        (None, Some(raw)) => return raw_filter(raw, &format!("{path}.raw")),
        (Some(states), None) => states,
        (Some(_), Some(_)) => return Err(format!("{path}: takes states or raw, not both")),
        (None, None) => return Err(format!("{path}: needs states or raw")),
    };
    if states.is_empty() {
        return Err(format!("{path}.states: must not be empty"));
    }

    let mut bits = 0;
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
        bits |= state.filter0_bits();
    }

    Ok(u64::from(bits) << CHA_FILTER0_STATE_SHIFT)
}

/// The filter1 value, by default the one the CHA collector programs: both
/// localities, all opcodes, near and not-near memory.
fn filter1(request: Option<&Filter1Request>, path: &str) -> Result<u64, String> {
    let Some(request) = request else {
        return Ok(filter1_value(LocalityRequest::All, &[]));
    };
    let path = format!("{path}.filter1");
    if let Some(raw) = &request.raw {
        if request.opcodes.is_some() || request.locality.is_some() {
            return Err(format!("{path}: raw excludes opcodes and locality"));
        }
        return raw_filter(raw, &format!("{path}.raw"));
    }

    let mut opcodes = Vec::new();
    if let Some(tokens) = &request.opcodes {
        if tokens.is_empty() || tokens.len() > 2 {
            return Err(format!(
                "{path}.opcodes: takes 1 or 2 opcodes, got {}",
                tokens.len()
            ));
        }
        for (index, token) in tokens.iter().enumerate() {
            opcodes.push(opcode(token, &format!("{path}.opcodes[{index}]"))?);
        }
    }

    Ok(filter1_value(
        request.locality.unwrap_or_default(),
        &opcodes,
    ))
}

fn filter1_value(locality: LocalityRequest, opcodes: &[u16]) -> u64 {
    let locality = match locality {
        LocalityRequest::All => CHA_FILTER1_REMOTE_BIT | CHA_FILTER1_LOCAL_BIT,
        LocalityRequest::Local => CHA_FILTER1_LOCAL_BIT,
        LocalityRequest::Remote => CHA_FILTER1_REMOTE_BIT,
    };
    let all_opcodes = if opcodes.is_empty() {
        CHA_FILTER1_ALL_OPCODE_BIT
    } else {
        0
    };
    let opcode0 = opcodes.first().copied().unwrap_or(0);
    let opcode1 = opcodes.get(1).copied().unwrap_or(0);

    u64::from(
        locality
            | all_opcodes
            | CHA_FILTER1_NEAR_MEMORY_BIT
            | CHA_FILTER1_NOT_NEAR_MEMORY_BIT
            | (u32::from(opcode0) << CHA_FILTER1_OPCODE0_SHIFT)
            | (u32::from(opcode1) << CHA_FILTER1_OPCODE1_SHIFT),
    )
}

fn opcode(token: &Token, path: &str) -> Result<u16, String> {
    match token {
        Token::Integer(value) if *value <= IDI_OPCODE_MAX => Ok(*value as u16),
        Token::Integer(value) => Err(format!("{path}: opcodes are 10 bits, got {value:#x}")),
        Token::Name(name) => IDI_OPCODES
            .iter()
            .find(|(opcode, _)| opcode == name)
            .map(|(_, value)| *value)
            .ok_or_else(|| {
                format!(
                    "{path}: unknown Skylake-SP opcode \"{name}\" (opcodes: {})",
                    join(IDI_OPCODES.iter().map(|(opcode, _)| opcode))
                )
            }),
    }
}

fn raw_filter(value: &Number, path: &str) -> Result<u64, String> {
    if value.0 > FILTER_REGISTER_MAX {
        return Err(format!(
            "{path}: filter registers are 32 bits, got {:#x}",
            value.0
        ));
    }

    Ok(value.0)
}

fn control(box_type: BoxType, encoding: &Encoding) -> u64 {
    if box_type == BoxType::Iio {
        pmon::iio_counter_control_without_reset(
            encoding.event,
            encoding.umask,
            encoding.ch_mask,
            encoding.fc_mask,
            true,
        )
    } else {
        u64::from(pmon::counter_control_without_reset(
            encoding.event,
            encoding.umask,
            true,
        ))
    }
}

fn labels(box_type: BoxType, encoding: &Encoding, filters: Option<ChaFilters>) -> EventLabels {
    let mut labels = EventLabels::from([
        ("alias".to_string(), encoding.alias.to_string()),
        ("event".to_string(), format!("{:#04x}", encoding.event)),
        ("umask".to_string(), format!("{:#04x}", encoding.umask)),
    ]);
    if box_type == BoxType::Iio {
        labels.insert("ch_mask".to_string(), format!("{:#04x}", encoding.ch_mask));
        labels.insert("fc_mask".to_string(), format!("{:#04x}", encoding.fc_mask));
    }
    if let Some(filters) = filters {
        labels.insert("filter0".to_string(), format!("{:#x}", filters.filter0));
        labels.insert("filter1".to_string(), format!("{:#x}", filters.filter1));
    }

    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_tables_are_sorted_and_fit_the_boxes() {
        for skx_box in [CHA, IIO, IRP, IMC] {
            assert!(
                skx_box
                    .event_counters
                    .windows(2)
                    .all(|pair| pair[0].0 < pair[1].0),
                "{}",
                skx_box.name
            );
            assert!(
                skx_box.event_counters.iter().all(
                    |(_, counters)| *counters != 0 && counters >> skx_box.general_counters == 0
                ),
                "{}",
                skx_box.name
            );
        }
    }

    #[test]
    fn aliases_name_listed_events_and_distinct_values() {
        for skx_box in [CHA, IIO, IRP, IMC] {
            for (index, alias) in skx_box.aliases.iter().enumerate() {
                assert!(
                    skx_box
                        .event_counters
                        .binary_search_by_key(&alias.event, |(event, _)| *event)
                        .is_ok(),
                    "{} {}",
                    skx_box.name,
                    alias.name
                );
                assert!(
                    skx_box.aliases[..index]
                        .iter()
                        .all(|earlier| earlier.name != alias.name),
                    "{} {}",
                    skx_box.name,
                    alias.name
                );
            }
        }
        for (index, (name, value)) in IDI_OPCODES.iter().enumerate() {
            assert!(*value <= IDI_OPCODE_MAX as u16, "{name}");
            assert!(
                IDI_OPCODES[..index]
                    .iter()
                    .all(|(earlier, earlier_value)| earlier != name && earlier_value != value),
                "{name}"
            );
        }
    }
}
