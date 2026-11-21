//! Ice Lake-SP, Sapphire Rapids and Emerald Rapids encodings of PMU config
//! requests.
//!
//! These CHAs select requests in the counter control's umask_ext field
//! instead of filter registers: no group takes `filter0` or `filter1`.

mod counters;

use crate::metal::arch::skx::pmon;
use crate::pmu_config::spec::{
    Alias, BoxSpec, Encoding, EventRequest, GroupProgram, GroupRequest, Param, Token,
    check_counter, check_event_count, event_encoding, join, raw_encoding,
};
use crate::pmu_config::types::{BoxType, CounterSlot, EventLabels, ProgrammedCounter};

const UMASK_EXT_SHIFT: u32 = 32;

/// How a model lays out its counter controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Ice Lake-SP: event, umask, overflow enable and enable bits as on
    /// Skylake-SP; the collector adds the counter's reset bit.
    Icx,
    /// Sapphire Rapids and Emerald Rapids: event and umask, no enable bit.
    /// Linux writes the event config as is (spr_uncore_msr_enable_event),
    /// and the collector resets counters through the box control.
    Spr,
}

/// One of the models this module encodes requests for.
struct Model {
    /// Model name in messages.
    name: &'static str,
    layout: Layout,
    cha: BoxSpec,
    cha_aliases: &'static [ChaAlias],
    /// The TOR opcodes the CHA aliases name.
    opcodes: &'static [Opcode],
    /// The largest CHA umask_ext: control bits 32-57 on Ice Lake-SP (Linux
    /// format umask_ext2), 32-63 on Sapphire Rapids (umask_ext5).
    umask_ext_max: u32,
    imc: BoxSpec,
    /// The `alias` label of the IMC fixed counter.
    imc_fixed_alias: &'static str,
}

const ICX: Model = Model {
    name: "Ice Lake-SP",
    layout: Layout::Icx,
    cha: BoxSpec {
        model: "Ice Lake-SP",
        name: "CHA",
        general_counters: 4,
        event_counters: &counters::ICX_CHA,
        aliases: &[],
    },
    cha_aliases: ICX_CHA_ALIASES,
    opcodes: ICX_OPCODES,
    umask_ext_max: (1 << 26) - 1,
    imc: BoxSpec {
        model: "Ice Lake-SP",
        name: "IMC",
        general_counters: 4,
        event_counters: &counters::ICX_IMC,
        aliases: ICX_IMC_ALIASES,
    },
    // UNC_M_HCLOCKTICKS, the one perfmon event of `Counter` "FIXED".
    imc_fixed_alias: "hclockticks",
};

const SPR: Model = spr("Sapphire Rapids");
const EMR: Model = spr("Emerald Rapids");

/// Sapphire Rapids and Emerald Rapids share their encodings and counters.
const fn spr(name: &'static str) -> Model {
    Model {
        name,
        layout: Layout::Spr,
        cha: BoxSpec {
            model: name,
            name: "CHA",
            general_counters: 4,
            event_counters: &counters::SPR_CHA,
            aliases: &[],
        },
        cha_aliases: SPR_CHA_ALIASES,
        opcodes: SPR_OPCODES,
        umask_ext_max: u32::MAX,
        imc: BoxSpec {
            model: name,
            name: "IMC",
            general_counters: 4,
            event_counters: &counters::SPR_IMC,
            aliases: SPR_IMC_ALIASES,
        },
        // Perfmon lists no fixed iMC event; Linux spr_uncore_imc programs
        // the fixed counter as on Ice Lake-SP.
        imc_fixed_alias: "fixed_clockticks",
    }
}

/// A CHA event by name. Its umask and umask_ext are `umask` and `umask_ext`
/// or'd with the bits of each parameter's value.
struct ChaAlias {
    name: &'static str,
    event: u8,
    umask: u8,
    umask_ext: u32,
    params: &'static [ChaParam],
    /// Whether the alias takes an `opcode` (the TOR events).
    tor: bool,
}

impl ChaAlias {
    const fn new(name: &'static str, event: u8, umask: u8, params: &'static [ChaParam]) -> Self {
        Self {
            name,
            event,
            umask,
            umask_ext: 0,
            params,
            tor: false,
        }
    }

    const fn tor(name: &'static str, event: u8) -> Self {
        Self {
            umask_ext: TOR_UMASK_EXT,
            tor: true,
            ..Self::new(name, event, 0x00, &[TOR_SOURCES, TOR_RESULTS])
        }
    }
}

/// A CHA alias parameter: its request field and the umask and umask_ext bits
/// of its values.
struct ChaParam {
    field: &'static str,
    values: &'static [(&'static str, u8, u32)],
}

/// The requests a TOR opcode comes from: IA requests enter the IRQ, IO
/// requests the PRQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TorSource {
    Ia,
    Io,
}

/// A TOR opcode by name.
struct Opcode {
    name: &'static str,
    source: TorSource,
    value: u16,
}

impl Opcode {
    const fn ia(name: &'static str, value: u16) -> Self {
        Self {
            name,
            source: TorSource::Ia,
            value,
        }
    }

    const fn io(name: &'static str, value: u16) -> Self {
        Self {
            name,
            source: TorSource::Io,
            value,
        }
    }
}

// Encodings from Intel perfmon {ICX,SPR,EMR}/events/*_uncore.json (see
// `counters`): aliases cover only events of the public files, never ones
// that only the experimental files list. The comments name the events.

/// The umask_ext bits all public UNC_CHA_TOR_INSERTS.* and
/// UNC_CHA_TOR_OCCUPANCY.* events set that select no subset: DDR, PMM, bit 4,
/// MMCFG, MMIO, local and remote targets (bits 2-8) and near and not near
/// memory (bits 22-23). The experimental single-bit events name them
/// (UNC_CHA_TOR_INSERTS.DDR = 0x4, .PMM = 0x8, .MMCFG = 0x20, .MMIO = 0x40,
/// .LOCAL_TGT = 0x80, .REMOTE_TGT = 0x100, .NEARMEM = 0x400000,
/// .NOT_NEARMEM = 0x800000).
const TOR_UMASK_EXT: u32 = 0x00c0_01fc;
/// UNC_CHA_TOR_INSERTS.MATCH_OPC: match the opcode in bits 11-21.
const TOR_MATCH_OPCODE: u32 = 1 << 9;
/// UNC_CHA_TOR_INSERTS.PREMORPH_OPC: match the opcode before morphing. Every
/// public IA opcode event sets it and no IO one does.
const TOR_PREMORPH_OPCODE: u32 = 1 << 10;
const TOR_OPCODE_SHIFT: u32 = 11;
const TOR_OPCODE_MAX: u64 = 0x7ff;

/// UNC_CHA_TOR_*.IA* and .IO*: IRQ (0x01) for IA, PRQ (0x04) for IO; all
/// counts both.
const TOR_SOURCES: ChaParam = ChaParam {
    field: "source",
    values: &[("ia", 0x01, 0), ("io", 0x04, 0), ("all", 0x05, 0)],
};
/// UNC_CHA_TOR_*.IA_HIT*, .IA_MISS*, .IO_HIT* and .IO_MISS*: umask_ext bit 0
/// for hits, bit 1 for misses.
const TOR_RESULTS: ChaParam = ChaParam {
    field: "result",
    values: &[("all", 0, 0x03), ("hit", 0, 0x01), ("miss", 0, 0x02)],
};
/// UNC_CHA_REQUESTS.*
const REQUEST_OPS: ChaParam = ChaParam {
    field: "op",
    values: &[
        ("reads", 0x03, 0),
        ("reads_local", 0x01, 0),
        ("reads_remote", 0x02, 0),
        ("writes", 0x0c, 0),
        ("writes_local", 0x04, 0),
        ("writes_remote", 0x08, 0),
        ("invitoe_local", 0x10, 0),
        ("invitoe_remote", 0x20, 0),
    ],
};
/// UNC_CHA_LLC_LOOKUP.DATA_READ, the one public LLC_LOOKUP event: the umask
/// selects cache states (all of them here), umask_ext the requests.
const ICX_LLC_LOOKUP_OPS: ChaParam = ChaParam {
    field: "op",
    values: &[("data_read", 0xff, 0x1bc1)],
};
/// UNC_CHA_SF_EVICTION.*_STATE
const SF_EVICTION_STATES: ChaParam = ChaParam {
    field: "state",
    values: &[("m", 0x01, 0), ("e", 0x02, 0), ("s", 0x04, 0)],
};

const ICX_CHA_ALIASES: &[ChaAlias] = &[
    // UNC_CHA_CLOCKTICKS
    ChaAlias::new("clockticks", 0x00, 0x00, &[]),
    ChaAlias::tor("tor_inserts", 0x35),
    ChaAlias::tor("tor_occupancy", 0x36),
    ChaAlias::new("llc_lookup", 0x34, 0x00, &[ICX_LLC_LOOKUP_OPS]),
    // UNC_CHA_LLC_VICTIMS.ALL, the one public LLC_VICTIMS event.
    ChaAlias::new("llc_victims", 0x37, 0x0f, &[]),
    ChaAlias::new("sf_eviction", 0x3d, 0x00, &[SF_EVICTION_STATES]),
    ChaAlias::new("requests", 0x50, 0x00, &[REQUEST_OPS]),
];

/// Sapphire Rapids and Emerald Rapids list LLC_LOOKUP, LLC_VICTIMS and
/// SF_EVICTION only as experimental events.
const SPR_CHA_ALIASES: &[ChaAlias] = &[
    // UNC_CHA_CLOCKTICKS
    ChaAlias::new("clockticks", 0x01, 0x00, &[]),
    ChaAlias::tor("tor_inserts", 0x35),
    ChaAlias::tor("tor_occupancy", 0x36),
    ChaAlias::new("requests", 0x50, 0x00, &[REQUEST_OPS]),
];

/// The opcodes of the public UNC_CHA_TOR_INSERTS.IA_* and .IO_* events,
/// named as in the event names.
const ICX_OPCODES: &[Opcode] = &[
    Opcode::ia("RFO", 0x100),
    Opcode::ia("CRD", 0x101),
    Opcode::ia("DRD", 0x102),
    Opcode::ia("FULL_STREAMING_WR", 0x10c),
    Opcode::ia("PARTIAL_STREAMING_WR", 0x10d),
    Opcode::ia("RFO_PREF", 0x110),
    Opcode::ia("CRD_PREF", 0x111),
    Opcode::ia("DRD_PREF", 0x112),
    Opcode::ia("CLFLUSH", 0x118),
    Opcode::ia("SPECITOM", 0x18a),
    Opcode::ia("LLCPREFRFO", 0x198),
    Opcode::ia("LLCPREFDATA", 0x19a),
    Opcode::io("RFO", 0x100),
    Opcode::io("PCIRDCUR", 0x11e),
    Opcode::io("ITOM", 0x188),
    Opcode::io("ITOMCACHENEAR", 0x1a8),
];

const SPR_OPCODES: &[Opcode] = &[
    Opcode::ia("RFO", 0x100),
    Opcode::ia("CRD", 0x101),
    Opcode::ia("DRD", 0x102),
    Opcode::ia("RFO_PREF", 0x110),
    Opcode::ia("CRD_PREF", 0x111),
    Opcode::ia("DRD_PREF", 0x112),
    Opcode::ia("CLFLUSH", 0x118),
    Opcode::ia("SPECITOM", 0x18a),
    Opcode::ia("LLCPREFRFO", 0x198),
    Opcode::ia("LLCPREFDATA", 0x19a),
    Opcode::io("RFO", 0x100),
    Opcode::io("CLFLUSH", 0x118),
    Opcode::io("PCIRDCUR", 0x11e),
    Opcode::io("WBMTOI", 0x184),
    Opcode::io("ITOM", 0x188),
    Opcode::io("ITOMCACHENEAR", 0x1a8),
];

const ICX_IMC_ALIASES: &[Alias] = &[
    // UNC_M_CLOCKTICKS
    Alias::new("clockticks", 0x00, 0x00, &[]),
    // UNC_M_CAS_COUNT.RD, .WR and .ALL
    Alias::new(
        "cas_count",
        0x04,
        0x00,
        &[Param {
            field: "op",
            values: &[("rd", 0x0f), ("wr", 0x30), ("all", 0x3f)],
        }],
    ),
    // UNC_M_RPQ_INSERTS.PCH*, UNC_M_WPQ_INSERTS.PCH*,
    // UNC_M_RPQ_OCCUPANCY_PCH* and UNC_M_WPQ_OCCUPANCY_PCH*: per pseudo
    // channel, the inserts by umask, the occupancies by event.
    Alias::new("rpq_inserts_pch0", 0x10, 0x01, &[]),
    Alias::new("rpq_inserts_pch1", 0x10, 0x02, &[]),
    Alias::new("wpq_inserts_pch0", 0x20, 0x01, &[]),
    Alias::new("wpq_inserts_pch1", 0x20, 0x02, &[]),
    Alias::new("rpq_occupancy_pch0", 0x80, 0x00, &[]),
    Alias::new("rpq_occupancy_pch1", 0x81, 0x00, &[]),
    Alias::new("wpq_occupancy_pch0", 0x82, 0x00, &[]),
    Alias::new("wpq_occupancy_pch1", 0x83, 0x00, &[]),
];

const SPR_IMC_ALIASES: &[Alias] = &[
    // UNC_M_CLOCKTICKS, at DCLK
    Alias::new("clockticks", 0x01, 0x01, &[]),
    // UNC_M_HCLOCKTICKS
    Alias::new("hclockticks", 0x01, 0x00, &[]),
    // UNC_M_CAS_COUNT.RD, .WR and .ALL
    Alias::new(
        "cas_count",
        0x05,
        0x00,
        &[Param {
            field: "op",
            values: &[("rd", 0xcf), ("wr", 0xf0), ("all", 0xff)],
        }],
    ),
    // UNC_M_RPQ_INSERTS.PCH*, UNC_M_WPQ_INSERTS.PCH*,
    // UNC_M_RPQ_OCCUPANCY_PCH* and UNC_M_WPQ_OCCUPANCY_PCH*: per pseudo
    // channel, the inserts by umask, the occupancies by event.
    Alias::new("rpq_inserts_pch0", 0x10, 0x01, &[]),
    Alias::new("rpq_inserts_pch1", 0x10, 0x02, &[]),
    Alias::new("wpq_inserts_pch0", 0x20, 0x01, &[]),
    Alias::new("wpq_inserts_pch1", 0x20, 0x02, &[]),
    Alias::new("rpq_occupancy_pch0", 0x80, 0x00, &[]),
    Alias::new("rpq_occupancy_pch1", 0x81, 0x00, &[]),
    Alias::new("wpq_occupancy_pch0", 0x82, 0x00, &[]),
    Alias::new("wpq_occupancy_pch1", 0x83, 0x00, &[]),
];

/// The register fields of one counter's event on these models.
struct IcxEncoding {
    base: Encoding,
    umask_ext: u32,
}

pub(super) fn program_icx_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    program_group(&ICX, box_type, group, path)
}

pub(super) fn program_spr_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    program_group(&SPR, box_type, group, path)
}

pub(super) fn program_emr_group(
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    program_group(&EMR, box_type, group, path)
}

fn program_group(
    model: &Model,
    box_type: BoxType,
    group: &GroupRequest,
    path: &str,
) -> Result<GroupProgram, String> {
    let box_spec = match box_type {
        BoxType::Cha => &model.cha,
        BoxType::Imc => &model.imc,
        BoxType::Ha => return Err(format!("{path}: {} has no ha boxes", model.name)),
        _ => {
            return Err(format!(
                "{path}: {} {} configs are not supported",
                model.name,
                box_type.label()
            ));
        }
    };
    check_event_count(box_spec, group, path)?;
    for (field, given) in [
        ("filter0", group.filter0.is_some()),
        ("filter1", group.filter1.is_some()),
    ] {
        if given {
            return Err(format!(
                "{path}.{field}: not supported on {}, whose CHA events select requests by umask_ext",
                model.name
            ));
        }
    }
    if group.fixed && box_type != BoxType::Imc {
        return Err(format!(
            "{path}.fixed: only IMC boxes have a fixed counter on {}",
            model.name
        ));
    }

    let mut counters = Vec::new();
    for (slot, event) in group.events.iter().enumerate() {
        let Some(event) = event else {
            continue;
        };
        let event_path = format!("{path}.events[{slot}]");
        let encoding = encoding(model, box_type, event, &event_path)?;
        check_counter(box_spec, &encoding.base, slot, &event_path)?;
        counters.push(ProgrammedCounter {
            slot: CounterSlot::General(slot as u8),
            control: control(model, box_type, &encoding),
            labels: labels(box_type, &encoding),
        });
    }
    if group.fixed {
        counters.push(ProgrammedCounter {
            slot: CounterSlot::Fixed,
            control: u64::from(pmon::FIXED_COUNTER_ENABLE),
            labels: EventLabels::from([("alias".to_string(), model.imc_fixed_alias.to_string())]),
        });
    }

    Ok(GroupProgram {
        counters,
        filters: None,
    })
}

fn encoding(
    model: &Model,
    box_type: BoxType,
    event: &EventRequest,
    path: &str,
) -> Result<IcxEncoding, String> {
    if event.tid_en {
        return Err(format!("{path}.tid_en: not supported on {}", model.name));
    }
    for (field, given) in [
        ("ch_mask", event.ch_mask.is_some()),
        ("fc_mask", event.fc_mask.is_some()),
    ] {
        if given {
            return Err(format!("{path}.{field}: only IIO events take a {field}"));
        }
    }
    if box_type == BoxType::Cha {
        return cha_encoding(model, event, path);
    }
    if event.umask_ext.is_some() {
        return Err(format!(
            "{path}.umask_ext: only CHA events take a umask_ext"
        ));
    }
    if event.opcode.is_some() {
        return Err(format!("{path}.opcode: only TOR aliases take an opcode"));
    }
    let box_spec = match box_type {
        BoxType::Imc => &model.imc,
        _ => unreachable!("program_group takes no other box types"),
    };

    Ok(IcxEncoding {
        base: event_encoding(box_spec, event, path)?,
        umask_ext: 0,
    })
}

/// The event of a CHA alias, or of raw register fields with an optional raw
/// umask_ext.
fn cha_encoding(model: &Model, event: &EventRequest, path: &str) -> Result<IcxEncoding, String> {
    match (&event.alias, &event.event) {
        (Some(alias), None) => cha_alias_encoding(model, alias, event, path),
        (None, Some(code)) => {
            if event.opcode.is_some() {
                return Err(format!("{path}.opcode: only TOR aliases take an opcode"));
            }
            let umask_ext = match &event.umask_ext {
                Some(value) => u32::try_from(value.0)
                    .ok()
                    .filter(|umask_ext| *umask_ext <= model.umask_ext_max)
                    .ok_or_else(|| {
                        format!(
                            "{path}.umask_ext: must be at most {:#x} on {}, got {:#x}",
                            model.umask_ext_max, model.name, value.0
                        )
                    })?,
                None => 0,
            };
            Ok(IcxEncoding {
                base: raw_encoding(code, event, path)?,
                umask_ext,
            })
        }
        (Some(_), Some(_)) => Err(format!("{path}: takes an alias or an event, not both")),
        (None, None) => Err(format!("{path}: needs an alias or an event")),
    }
}

fn cha_alias_encoding(
    model: &Model,
    name: &str,
    event: &EventRequest,
    path: &str,
) -> Result<IcxEncoding, String> {
    let alias = model
        .cha_aliases
        .iter()
        .find(|alias| alias.name == name)
        .ok_or_else(|| {
            format!(
                "{path}.alias: unknown {} CHA alias \"{name}\" (aliases: {})",
                model.name,
                join(model.cha_aliases.iter().map(|alias| alias.name))
            )
        })?;
    for (field, given) in [
        ("umask", event.umask.is_some()),
        ("umask_ext", event.umask_ext.is_some()),
    ] {
        if given {
            return Err(format!("{path}.{field}: alias {name} sets its own {field}"));
        }
    }

    let mut umask = alias.umask;
    let mut umask_ext = alias.umask_ext;
    for (field, value) in event.params() {
        let param = alias.params.iter().find(|param| param.field == field);
        match (param, value) {
            (None, None) => {}
            (None, Some(_)) => return Err(format!("{path}.{field}: {name} takes no {field}")),
            (Some(param), None) => {
                return Err(format!(
                    "{path}: {name} needs a {field} ({})",
                    join(param.values.iter().map(|(value, _, _)| value))
                ));
            }
            (Some(param), Some(value)) => {
                let (_, umask_bits, umask_ext_bits) = param
                    .values
                    .iter()
                    .find(|(name, _, _)| *name == value)
                    .ok_or_else(|| {
                        format!(
                            "{path}.{field}: {name} {field} must be one of {}, got \"{value}\"",
                            join(param.values.iter().map(|(value, _, _)| value))
                        )
                    })?;
                umask |= umask_bits;
                umask_ext |= umask_ext_bits;
            }
        }
    }
    if let Some(token) = &event.opcode {
        if !alias.tor {
            return Err(format!("{path}.opcode: {name} takes no opcode"));
        }
        let source = match event.source.as_deref() {
            Some("ia") => TorSource::Ia,
            Some("io") => TorSource::Io,
            _ => {
                return Err(format!(
                    "{path}.opcode: {name} matches an opcode only with source ia or io"
                ));
            }
        };
        let opcode = opcode(model, source, token, &format!("{path}.opcode"))?;
        umask_ext |= TOR_MATCH_OPCODE | (u32::from(opcode) << TOR_OPCODE_SHIFT);
        if source == TorSource::Ia {
            umask_ext |= TOR_PREMORPH_OPCODE;
        }
    }

    Ok(IcxEncoding {
        base: Encoding {
            alias: alias.name,
            event: alias.event,
            umask,
            ch_mask: 0,
            fc_mask: 0,
        },
        umask_ext,
    })
}

/// A named opcode of `source`'s requests, or a raw 11-bit opcode.
fn opcode(model: &Model, source: TorSource, token: &Token, path: &str) -> Result<u16, String> {
    let names = || {
        join(
            model
                .opcodes
                .iter()
                .filter(|opcode| opcode.source == source)
                .map(|opcode| opcode.name),
        )
    };
    match token {
        Token::Integer(value) if *value <= TOR_OPCODE_MAX => Ok(*value as u16),
        Token::Integer(value) => Err(format!("{path}: opcodes are 11 bits, got {value:#x}")),
        Token::Name(name) => model
            .opcodes
            .iter()
            .find(|opcode| opcode.source == source && opcode.name == name)
            .map(|opcode| opcode.value)
            .ok_or_else(|| {
                format!(
                    "{path}: unknown {} {source:?} opcode \"{name}\" (opcodes: {})",
                    model.name,
                    names()
                )
            }),
    }
}

fn control(model: &Model, box_type: BoxType, encoding: &IcxEncoding) -> u64 {
    let base = &encoding.base;
    let select = match model.layout {
        Layout::Icx => u64::from(pmon::counter_control_without_reset(
            base.event, base.umask, true,
        )),
        Layout::Spr => u64::from(base.event) | (u64::from(base.umask) << 8),
    };

    match box_type {
        BoxType::Cha => select | (u64::from(encoding.umask_ext) << UMASK_EXT_SHIFT),
        _ => select,
    }
}

fn labels(box_type: BoxType, encoding: &IcxEncoding) -> EventLabels {
    let base = &encoding.base;
    let mut labels = EventLabels::from([
        ("alias".to_string(), base.alias.to_string()),
        ("event".to_string(), format!("{:#04x}", base.event)),
        ("umask".to_string(), format!("{:#04x}", base.umask)),
    ]);
    if box_type == BoxType::Cha {
        labels.insert(
            "umask_ext".to_string(),
            format!("{:#x}", encoding.umask_ext),
        );
    }

    labels
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{Value, json};

    use super::*;
    use crate::arch::IntelServerCpuModel;
    use crate::pmu_config::spec::{PmuConfigRequest, ProgrammedConfig, validate};
    use crate::pmu_config::types::{PmuTopology, ProgrammedBoxType};

    const MODELS: [(IntelServerCpuModel, &Model); 3] = [
        (IntelServerCpuModel::IceLakeXeon, &ICX),
        (IntelServerCpuModel::SapphireRapids, &SPR),
        (IntelServerCpuModel::EmeraldRapids, &EMR),
    ];

    fn topology() -> PmuTopology {
        let packages =
            |count: u16| BTreeMap::from([(0, (0..count).collect()), (1, (0..count).collect())]);

        PmuTopology {
            units: BTreeMap::from([(BoxType::Cha, packages(4)), (BoxType::Imc, packages(8))]),
        }
    }

    fn configs(model: IntelServerCpuModel, pmus: Value) -> Result<Vec<ProgrammedConfig>, String> {
        let request: PmuConfigRequest =
            serde_json::from_value(json!({ "pmus": pmus })).map_err(|error| error.to_string())?;
        validate(request, &topology(), model)
    }

    fn program(model: IntelServerCpuModel, box_type: &str, group: Value) -> ProgrammedBoxType {
        let configs = configs(model, json!({box_type: {"groups": [group]}})).unwrap();
        assert_eq!(configs.len(), 1);
        configs.into_iter().next().unwrap().program
    }

    fn error(model: IntelServerCpuModel, box_type: &str, group: Value) -> String {
        configs(model, json!({box_type: {"groups": [group]}})).unwrap_err()
    }

    /// The control and labels of the one counter `event` programs on
    /// counter `slot` of a CHA.
    fn cha_counter(
        model: IntelServerCpuModel,
        slot: usize,
        event: Value,
    ) -> (u64, BTreeMap<String, String>) {
        let mut events = vec![Value::Null; slot];
        events.push(event);
        let program = program(model, "cha", json!({ "events": events }));
        let counter = &program.groups[0].counters[0];
        assert_eq!(counter.slot, CounterSlot::General(slot as u8));
        (counter.control, counter.labels.clone())
    }

    /// The control a model programs for an event, umask and umask_ext, as
    /// Intel perfmon lists them.
    fn perfmon_control(model: IntelServerCpuModel, event: u8, umask: u8, umask_ext: u32) -> u64 {
        let enable = match model {
            IntelServerCpuModel::IceLakeXeon => (1 << 22) | (1 << 20),
            _ => 0,
        };
        u64::from(event) | (u64::from(umask) << 8) | enable | (u64::from(umask_ext) << 32)
    }

    #[test]
    fn counter_tables_are_sorted_and_fit_the_boxes() {
        for (_, model) in MODELS {
            let boxes = [&model.cha, &model.imc];
            for box_spec in boxes {
                let counters = box_spec.event_counters;
                assert!(
                    counters.windows(2).all(|pair| pair[0].0 < pair[1].0),
                    "{} {}",
                    model.name,
                    box_spec.name
                );
                assert!(
                    counters.iter().all(|(_, counters)| *counters != 0
                        && counters >> box_spec.general_counters == 0),
                    "{} {}",
                    model.name,
                    box_spec.name
                );
            }
        }
    }

    #[test]
    fn aliases_name_listed_events_distinct_values_and_opcodes() {
        for (_, model) in MODELS {
            for (index, alias) in model.cha_aliases.iter().enumerate() {
                assert!(
                    model
                        .cha
                        .event_counters
                        .binary_search_by_key(&alias.event, |(event, _)| *event)
                        .is_ok(),
                    "{} {}",
                    model.name,
                    alias.name
                );
                assert!(
                    model.cha_aliases[..index]
                        .iter()
                        .all(|earlier| earlier.name != alias.name),
                    "{} {}",
                    model.name,
                    alias.name
                );
                assert!(alias.umask_ext <= model.umask_ext_max, "{}", alias.name);
            }
            for (index, opcode) in model.opcodes.iter().enumerate() {
                assert!(u64::from(opcode.value) <= TOR_OPCODE_MAX, "{}", opcode.name);
                assert!(
                    model.opcodes[..index]
                        .iter()
                        .all(|earlier| earlier.source != opcode.source
                            || (earlier.name != opcode.name && earlier.value != opcode.value)),
                    "{} {}",
                    model.name,
                    opcode.name
                );
            }
        }
    }

    #[test]
    fn programs_cha_events_as_perfmon_lists_them() {
        use IntelServerCpuModel::{EmeraldRapids, IceLakeXeon, SapphireRapids};

        // (model, slot, request, perfmon event, event, umask, umask_ext)
        let cases = [
            (
                IceLakeXeon,
                0,
                json!({"alias": "tor_inserts", "source": "ia", "result": "hit", "opcode": "DRD"}),
                "UNC_CHA_TOR_INSERTS.IA_HIT_DRD",
                0x35,
                0x01,
                0x00c8_17fd,
            ),
            (
                IceLakeXeon,
                3,
                json!({"alias": "tor_inserts", "source": "io", "result": "all", "opcode": "PCIRDCUR"}),
                "UNC_CHA_TOR_INSERTS.IO_PCIRDCUR",
                0x35,
                0x04,
                0x00c8_f3ff,
            ),
            (
                IceLakeXeon,
                1,
                json!({"alias": "tor_inserts", "source": "ia", "result": "miss"}),
                "UNC_CHA_TOR_INSERTS.IA_MISS",
                0x35,
                0x01,
                0x00c0_01fe,
            ),
            (
                IceLakeXeon,
                0,
                json!({"alias": "tor_occupancy", "source": "ia", "result": "miss", "opcode": "DRD"}),
                "UNC_CHA_TOR_OCCUPANCY.IA_MISS_DRD",
                0x36,
                0x01,
                0x00c8_17fe,
            ),
            (
                IceLakeXeon,
                2,
                json!({"alias": "llc_lookup", "op": "data_read"}),
                "UNC_CHA_LLC_LOOKUP.DATA_READ",
                0x34,
                0xff,
                0x1bc1,
            ),
            (
                IceLakeXeon,
                1,
                json!({"alias": "llc_victims"}),
                "UNC_CHA_LLC_VICTIMS.ALL",
                0x37,
                0x0f,
                0,
            ),
            (
                IceLakeXeon,
                2,
                json!({"alias": "sf_eviction", "state": "s"}),
                "UNC_CHA_SF_EVICTION.S_STATE",
                0x3d,
                0x04,
                0,
            ),
            (
                IceLakeXeon,
                3,
                json!({"alias": "clockticks"}),
                "UNC_CHA_CLOCKTICKS",
                0x00,
                0x00,
                0,
            ),
            (
                SapphireRapids,
                2,
                json!({"alias": "tor_inserts", "source": "io", "result": "miss", "opcode": "ITOM"}),
                "UNC_CHA_TOR_INSERTS.IO_MISS_ITOM",
                0x35,
                0x04,
                0x00cc_43fe,
            ),
            (
                SapphireRapids,
                1,
                json!({"alias": "tor_inserts", "source": "io", "result": "all", "opcode": "WBMTOI"}),
                "UNC_CHA_TOR_INSERTS.IO_WBMTOI",
                0x35,
                0x04,
                0x00cc_23ff,
            ),
            (
                SapphireRapids,
                0,
                json!({"alias": "tor_occupancy", "source": "ia", "result": "miss", "opcode": "DRD"}),
                "UNC_CHA_TOR_OCCUPANCY.IA_MISS_DRD",
                0x36,
                0x01,
                0x00c8_17fe,
            ),
            (
                SapphireRapids,
                3,
                json!({"alias": "requests", "op": "reads_local"}),
                "UNC_CHA_REQUESTS.READS_LOCAL",
                0x50,
                0x01,
                0,
            ),
            (
                SapphireRapids,
                0,
                json!({"alias": "clockticks"}),
                "UNC_CHA_CLOCKTICKS",
                0x01,
                0x00,
                0,
            ),
            (
                EmeraldRapids,
                3,
                json!({"alias": "tor_inserts", "source": "ia", "result": "hit", "opcode": "DRD"}),
                "UNC_CHA_TOR_INSERTS.IA_HIT_DRD",
                0x35,
                0x01,
                0x00c8_17fd,
            ),
            (
                EmeraldRapids,
                0,
                json!({"event": "0x35", "umask": 1, "umask_ext": "0x00c816fe"}),
                "UNC_CHA_TOR_INSERTS.IA_MISS_DRD_LOCAL",
                0x35,
                0x01,
                0x00c8_16fe,
            ),
        ];
        for (model, slot, request, name, event, umask, umask_ext) in cases {
            let (control, labels) = cha_counter(model, slot, request.clone());
            assert_eq!(
                control,
                perfmon_control(model, event, umask, umask_ext),
                "{model:?} {name}: {request}"
            );
            assert_eq!(labels["event"], format!("{event:#04x}"), "{name}");
            assert_eq!(labels["umask"], format!("{umask:#04x}"), "{name}");
            assert_eq!(labels["umask_ext"], format!("{umask_ext:#x}"), "{name}");
        }
    }

    #[test]
    fn labels_cha_counters_with_their_alias_and_umask_ext() {
        let (_, labels) = cha_counter(
            IntelServerCpuModel::SapphireRapids,
            0,
            json!({"alias": "tor_inserts", "source": "all", "result": "all"}),
        );
        assert_eq!(
            labels,
            BTreeMap::from(
                [
                    ("alias", "tor_inserts"),
                    ("event", "0x35"),
                    ("umask", "0x05"),
                    ("umask_ext", "0xc001ff"),
                ]
                .map(|(key, value)| (key.to_string(), value.to_string()))
            )
        );

        let (_, labels) = cha_counter(
            IntelServerCpuModel::IceLakeXeon,
            0,
            json!({"event": "0x50", "umask": "0x03"}),
        );
        assert_eq!(labels["alias"], "raw");
        assert_eq!(labels["umask_ext"], "0x0");
    }

    #[test]
    fn matches_raw_opcodes_before_morphing_only_for_ia() {
        for (source, umask, umask_ext) in [("ia", 0x01, 0x00cc_47ff), ("io", 0x04, 0x00cc_43ff)] {
            let (control, _) = cha_counter(
                IntelServerCpuModel::IceLakeXeon,
                0,
                json!({"alias": "tor_inserts", "source": source, "result": "all", "opcode": "0x188"}),
            );
            assert_eq!(
                control,
                perfmon_control(IntelServerCpuModel::IceLakeXeon, 0x35, umask, umask_ext),
                "{source}"
            );
        }
    }

    #[test]
    fn rejects_invalid_cha_requests() {
        use IntelServerCpuModel::{EmeraldRapids, IceLakeXeon, SapphireRapids};

        let tor = |extra: Value| {
            let mut event = json!({"alias": "tor_inserts", "source": "ia", "result": "all"});
            event
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"events": [event]})
        };
        let cases = [
            (
                IceLakeXeon,
                json!({"filter0": {"states": ["M"]}, "events": [{"alias": "clockticks"}]}),
                "pmus.cha.groups[0].filter0: not supported on Ice Lake-SP, whose CHA events select requests by umask_ext",
            ),
            (
                SapphireRapids,
                json!({"filter1": {"opcodes": ["ItoM"]}, "events": [{"alias": "clockticks"}]}),
                "pmus.cha.groups[0].filter1: not supported on Sapphire Rapids, whose CHA events select requests by umask_ext",
            ),
            (
                EmeraldRapids,
                json!({"fixed": true, "events": [{"alias": "clockticks"}]}),
                "pmus.cha.groups[0].fixed: only IMC boxes have a fixed counter on Emerald Rapids",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "clockticks"}, null, null, null, null]}),
                "pmus.cha.groups[0].events: Ice Lake-SP CHA boxes have 4 general counters, got 5 events",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "clockticks", "tid_en": true}]}),
                "pmus.cha.groups[0].events[0].tid_en: not supported on Ice Lake-SP",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"event": 1, "ch_mask": 1}]}),
                "pmus.cha.groups[0].events[0].ch_mask: only IIO events take a ch_mask",
            ),
            (
                SapphireRapids,
                json!({"events": [{"alias": "llc_lookup", "op": "data_read"}]}),
                "pmus.cha.groups[0].events[0].alias: unknown Sapphire Rapids CHA alias \"llc_lookup\" (aliases: clockticks, tor_inserts, tor_occupancy, requests)",
            ),
            (
                IceLakeXeon,
                tor(json!({"umask": 1})),
                "pmus.cha.groups[0].events[0].umask: alias tor_inserts sets its own umask",
            ),
            (
                IceLakeXeon,
                tor(json!({"umask_ext": 1})),
                "pmus.cha.groups[0].events[0].umask_ext: alias tor_inserts sets its own umask_ext",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "tor_inserts", "source": "ia"}]}),
                "pmus.cha.groups[0].events[0]: tor_inserts needs a result (all, hit, miss)",
            ),
            (
                IceLakeXeon,
                tor(json!({"result": "evict"})),
                "pmus.cha.groups[0].events[0].result: tor_inserts result must be one of all, hit, miss, got \"evict\"",
            ),
            (
                IceLakeXeon,
                tor(json!({"op": "any"})),
                "pmus.cha.groups[0].events[0].op: tor_inserts takes no op",
            ),
            (
                IceLakeXeon,
                tor(json!({"source": "all", "opcode": "DRD"})),
                "pmus.cha.groups[0].events[0].opcode: tor_inserts matches an opcode only with source ia or io",
            ),
            (
                IceLakeXeon,
                tor(json!({"opcode": "PCIRDCUR"})),
                "pmus.cha.groups[0].events[0].opcode: unknown Ice Lake-SP Ia opcode \"PCIRDCUR\" (opcodes: RFO, CRD, DRD, FULL_STREAMING_WR, PARTIAL_STREAMING_WR, RFO_PREF, CRD_PREF, DRD_PREF, CLFLUSH, SPECITOM, LLCPREFRFO, LLCPREFDATA)",
            ),
            (
                IceLakeXeon,
                tor(json!({"source": "io", "opcode": "WBMTOI"})),
                "pmus.cha.groups[0].events[0].opcode: unknown Ice Lake-SP Io opcode \"WBMTOI\" (opcodes: RFO, PCIRDCUR, ITOM, ITOMCACHENEAR)",
            ),
            (
                SapphireRapids,
                tor(json!({"opcode": "0x800"})),
                "pmus.cha.groups[0].events[0].opcode: opcodes are 11 bits, got 0x800",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "requests", "op": "reads", "opcode": "DRD"}]}),
                "pmus.cha.groups[0].events[0].opcode: requests takes no opcode",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"event": "0x35", "opcode": "DRD"}]}),
                "pmus.cha.groups[0].events[0].opcode: only TOR aliases take an opcode",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"event": "0x35", "umask_ext": "0x4000000"}]}),
                "pmus.cha.groups[0].events[0].umask_ext: must be at most 0x3ffffff on Ice Lake-SP, got 0x4000000",
            ),
            (
                SapphireRapids,
                json!({"events": [{"event": "0x35", "umask_ext": "0x100000000"}]}),
                "pmus.cha.groups[0].events[0].umask_ext: must be at most 0xffffffff on Sapphire Rapids, got 0x100000000",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"event": "0x35", "source": "ia"}]}),
                "pmus.cha.groups[0].events[0].source: only aliases take a source",
            ),
            (
                IceLakeXeon,
                json!({"events": [null, {"alias": "tor_occupancy", "source": "ia", "result": "all"}]}),
                "pmus.cha.groups[0].events[1]: event 0x36 counts only on counters 0",
            ),
            (
                SapphireRapids,
                json!({"events": [{"event": 0}]}),
                "pmus.cha.groups[0].events[0]: Intel lists no Sapphire Rapids CHA event 0x00",
            ),
        ];
        for (model, group, expected) in cases {
            assert_eq!(error(model, "cha", group.clone()), expected, "{group}");
        }
    }

    #[test]
    fn spr_and_emr_take_events_icx_lists_only_as_experimental() {
        // UNC_CHA_LLC_LOOKUP.I: experimental on all three, so raw only.
        for (model, _) in MODELS {
            let (control, _) = cha_counter(model, 0, json!({"event": "0x34", "umask": 1}));
            assert_eq!(control, perfmon_control(model, 0x34, 0x01, 0), "{model:?}");
        }
    }

    #[test]
    fn older_models_reject_umask_ext_and_opcode() {
        for model in [
            IntelServerCpuModel::SkylakeXeon,
            IntelServerCpuModel::HaswellXeon,
        ] {
            assert_eq!(
                error(
                    model,
                    "cha",
                    json!({"events": [{"event": "0x35", "umask_ext": 1}]})
                ),
                "pmus.cha.groups[0].events[0].umask_ext: only Ice Lake-SP, Sapphire Rapids and Emerald Rapids CHA events take a umask_ext",
                "{model:?}"
            );
            assert_eq!(
                error(
                    model,
                    "cha",
                    json!({"events": [{"alias": "tor_inserts", "opcode": "DRD"}]})
                ),
                "pmus.cha.groups[0].events[0].opcode: only Ice Lake-SP, Sapphire Rapids and Emerald Rapids TOR aliases take an opcode",
                "{model:?}"
            );
        }
    }

    #[test]
    fn programs_imc_events_as_perfmon_lists_them() {
        use IntelServerCpuModel::{EmeraldRapids, IceLakeXeon, SapphireRapids};

        // (model, request, perfmon event, event, umask)
        let cases = [
            (
                IceLakeXeon,
                json!({"alias": "cas_count", "op": "rd"}),
                "UNC_M_CAS_COUNT.RD",
                0x04,
                0x0f,
            ),
            (
                IceLakeXeon,
                json!({"alias": "cas_count", "op": "all"}),
                "UNC_M_CAS_COUNT.ALL",
                0x04,
                0x3f,
            ),
            (
                IceLakeXeon,
                json!({"alias": "clockticks"}),
                "UNC_M_CLOCKTICKS",
                0x00,
                0x00,
            ),
            (
                IceLakeXeon,
                json!({"alias": "rpq_occupancy_pch1"}),
                "UNC_M_RPQ_OCCUPANCY_PCH1",
                0x81,
                0x00,
            ),
            (
                SapphireRapids,
                json!({"alias": "cas_count", "op": "wr"}),
                "UNC_M_CAS_COUNT.WR",
                0x05,
                0xf0,
            ),
            (
                SapphireRapids,
                json!({"alias": "clockticks"}),
                "UNC_M_CLOCKTICKS",
                0x01,
                0x01,
            ),
            (
                EmeraldRapids,
                json!({"alias": "hclockticks"}),
                "UNC_M_HCLOCKTICKS",
                0x01,
                0x00,
            ),
            (
                EmeraldRapids,
                json!({"alias": "wpq_inserts_pch1"}),
                "UNC_M_WPQ_INSERTS.PCH1",
                0x20,
                0x02,
            ),
            (
                SapphireRapids,
                json!({"event": "0x05", "umask": "0xc1"}),
                "UNC_M_CAS_COUNT.RD_REG",
                0x05,
                0xc1,
            ),
        ];
        for (model, request, name, event, umask) in cases {
            for slot in 0..4 {
                let mut events = vec![Value::Null; slot];
                events.push(request.clone());
                let program = program(model, "imc", json!({ "events": events }));
                let counter = &program.groups[0].counters[0];
                assert_eq!(counter.slot, CounterSlot::General(slot as u8));
                assert_eq!(
                    counter.control,
                    perfmon_control(model, event, umask, 0),
                    "{model:?} {name}"
                );
                assert_eq!(counter.labels["event"], format!("{event:#04x}"), "{name}");
                assert_eq!(counter.labels["umask"], format!("{umask:#04x}"), "{name}");
                assert!(!counter.labels.contains_key("umask_ext"), "{name}");
            }
        }
    }

    #[test]
    fn programs_the_imc_fixed_counter() {
        for (model, alias) in [
            (IntelServerCpuModel::IceLakeXeon, "hclockticks"),
            (IntelServerCpuModel::SapphireRapids, "fixed_clockticks"),
            (IntelServerCpuModel::EmeraldRapids, "fixed_clockticks"),
        ] {
            let with_general = program(
                model,
                "imc",
                json!({"fixed": true, "events": [{"alias": "cas_count", "op": "rd"}]}),
            );
            let fixed = &with_general.groups[0].counters[1];
            assert_eq!(fixed.slot, CounterSlot::Fixed, "{model:?}");
            assert_eq!(fixed.control, 1 << 22, "{model:?}");
            assert_eq!(
                fixed.labels,
                BTreeMap::from([("alias".to_string(), alias.to_string())]),
                "{model:?}"
            );

            let fixed_only = program(model, "imc", json!({"fixed": true}));
            assert_eq!(fixed_only.groups[0].counters.len(), 1, "{model:?}");
        }
    }

    #[test]
    fn rejects_invalid_imc_requests() {
        use IntelServerCpuModel::{IceLakeXeon, SapphireRapids};

        let cases = [
            (
                IceLakeXeon,
                json!({"filter0": {"states": ["M"]}, "events": [{"alias": "clockticks"}]}),
                "pmus.imc.groups[0].filter0: not supported on Ice Lake-SP, whose CHA events select requests by umask_ext",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"event": 4, "umask_ext": 1}]}),
                "pmus.imc.groups[0].events[0].umask_ext: only CHA events take a umask_ext",
            ),
            (
                SapphireRapids,
                json!({"events": [{"alias": "cas_count", "op": "rd", "opcode": "DRD"}]}),
                "pmus.imc.groups[0].events[0].opcode: only TOR aliases take an opcode",
            ),
            (
                SapphireRapids,
                json!({"events": [{"alias": "cas_count", "op": "rd", "fc_mask": 1}]}),
                "pmus.imc.groups[0].events[0].fc_mask: only IIO events take a fc_mask",
            ),
            (
                SapphireRapids,
                json!({"events": [{"alias": "cas_count", "op": "underfill"}]}),
                "pmus.imc.groups[0].events[0].op: cas_count op must be one of rd, wr, all, got \"underfill\"",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "hclockticks"}]}),
                "pmus.imc.groups[0].events[0].alias: unknown Ice Lake-SP IMC alias \"hclockticks\" (aliases: clockticks, cas_count, rpq_inserts_pch0, rpq_inserts_pch1, wpq_inserts_pch0, wpq_inserts_pch1, rpq_occupancy_pch0, rpq_occupancy_pch1, wpq_occupancy_pch0, wpq_occupancy_pch1)",
            ),
            (
                SapphireRapids,
                json!({"events": [{"event": "0x04", "umask": "0x0f"}]}),
                "pmus.imc.groups[0].events[0]: Intel lists no Sapphire Rapids IMC event 0x04",
            ),
            (
                IceLakeXeon,
                json!({"events": [{"alias": "clockticks"}, null, null, null, {"alias": "clockticks"}]}),
                "pmus.imc.groups[0].events: Ice Lake-SP IMC boxes have 4 general counters, got 5 events",
            ),
        ];
        for (model, group, expected) in cases {
            assert_eq!(error(model, "imc", group.clone()), expected, "{group}");
        }
    }

    #[test]
    fn rejects_other_box_types_until_supported() {
        assert_eq!(
            error(IntelServerCpuModel::IceLakeXeon, "ha", json!({})),
            "pmus.ha: this host has no ha boxes"
        );
    }

    #[test]
    fn sapphire_and_emerald_rapids_program_alike() {
        let pmus = json!({"cha": {
            "grouping": "temporal",
            "groups": [
                {"events": [{"alias": "tor_occupancy", "source": "io", "result": "miss", "opcode": "ITOM"},
                            {"alias": "tor_inserts", "source": "io", "result": "miss", "opcode": "ITOM"},
                            {"alias": "clockticks"}]},
                {"events": [{"alias": "requests", "op": "writes"}]}
            ]
        }});
        let spr = configs(IntelServerCpuModel::SapphireRapids, pmus.clone()).unwrap();
        let emr = configs(IntelServerCpuModel::EmeraldRapids, pmus.clone()).unwrap();
        let icx = configs(IntelServerCpuModel::IceLakeXeon, pmus).unwrap();

        assert_eq!(spr, emr);
        assert_ne!(spr[0].config_id, icx[0].config_id);
        assert_eq!(
            icx[0].program.groups[1].units,
            BTreeMap::from([
                (0, BTreeSet::from([0, 1, 2, 3])),
                (1, BTreeSet::from([0, 1, 2, 3]))
            ])
        );
    }

    #[test]
    fn pins_the_config_ids_of_an_example() {
        let pmus = json!({"cha": {"groups": [{"events": [
            {"alias": "tor_inserts", "source": "io", "result": "miss", "opcode": "ITOM"},
            {"alias": "clockticks"}
        ]}]}});
        let ids = MODELS.map(|(model, _)| configs(model, pmus.clone()).unwrap()[0].config_id);
        assert_eq!(
            ids,
            [
                2_530_255_275_683_396,
                7_047_736_391_346_533,
                7_047_736_391_346_533
            ]
        );

        let pmus = json!({"imc": {
            "grouping": "temporal",
            "aggregate": "box",
            "groups": [
                {"fixed": true, "events": [{"alias": "cas_count", "op": "rd"}, {"alias": "cas_count", "op": "wr"}]},
                {"events": [{"alias": "rpq_occupancy_pch0"}, {"alias": "rpq_inserts_pch0"}]}
            ]
        }});
        let ids = MODELS.map(|(model, _)| configs(model, pmus.clone()).unwrap()[0].config_id);

        assert_eq!(
            ids,
            [
                4_677_831_721_322_206,
                8_486_092_047_021_842,
                8_486_092_047_021_842
            ]
        );
    }
}
