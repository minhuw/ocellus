//! PMU config requests and their validation into the programming of each
//! box type.

mod skx;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::arch::IntelServerCpuModel;
use crate::pmu_config::types::{
    Aggregate, BoxType, ChaFilters, CounterSlot, Grouping, PmuTopology, ProgrammedBoxType,
    ProgrammedCounter, ProgrammedGroup,
};

/// Config ids stay below 2^53, exact as sample values.
const ID_LIMIT: u64 = 1 << 53;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// A request to set the PMU configs of some box types, as rhea-worker puts it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmuConfigRequest {
    pmus: PmuRequests,
}

impl PmuConfigRequest {
    /// The box types the request programs.
    pub fn box_types(&self) -> BTreeSet<BoxType> {
        let pmus = &self.pmus;
        [
            (BoxType::Cha, pmus.cha.is_some()),
            (BoxType::Iio, pmus.iio.is_some()),
            (BoxType::Irp, pmus.irp.is_some()),
            (BoxType::Imc, pmus.imc.is_some()),
        ]
        .into_iter()
        .filter_map(|(box_type, named)| named.then_some(box_type))
        .collect()
    }
}

/// The validated programming of one box type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgrammedConfig {
    /// Hash of `program`, below 2^53: equal programming of a box type gives
    /// an equal id, whatever other box types the request programs.
    pub config_id: u64,
    pub program: ProgrammedBoxType,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PmuRequests {
    cha: Option<BoxRequest>,
    iio: Option<BoxRequest>,
    irp: Option<BoxRequest>,
    imc: Option<BoxRequest>,
}

impl PmuRequests {
    fn into_box_requests(self) -> impl Iterator<Item = (BoxType, BoxRequest)> {
        [
            (BoxType::Cha, self.cha),
            (BoxType::Iio, self.iio),
            (BoxType::Irp, self.irp),
            (BoxType::Imc, self.imc),
        ]
        .into_iter()
        .filter_map(|(box_type, request)| Some((box_type, request?)))
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxRequest {
    #[serde(default)]
    grouping: GroupingRequest,
    #[serde(default)]
    aggregate: AggregateRequest,
    /// Default: every package of the host.
    packages: Option<Vec<Number>>,
    groups: Vec<GroupRequest>,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum GroupingRequest {
    #[default]
    Fixed,
    Spatial,
    Temporal,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum AggregateRequest {
    #[default]
    Sum,
    Box,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupRequest {
    /// Unit ids in every selected package; default: all units.
    units: Option<Vec<Number>>,
    filter0: Option<Filter0Request>,
    filter1: Option<Filter1Request>,
    /// Entry `i` programs general counter `i`; `null` leaves it unused.
    #[serde(default)]
    events: Vec<Option<EventRequest>>,
    /// Enables the box's fixed counter.
    #[serde(default)]
    fixed: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter0Request {
    states: Option<Vec<String>>,
    raw: Option<Number>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter1Request {
    /// Opcode names or raw opcode values.
    opcodes: Option<Vec<Token>>,
    locality: Option<LocalityRequest>,
    raw: Option<Number>,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum LocalityRequest {
    #[default]
    All,
    Local,
    Remote,
}

/// An event by alias and alias parameters, or by raw register fields.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventRequest {
    alias: Option<String>,
    event: Option<Number>,
    umask: Option<Number>,
    ch_mask: Option<Number>,
    fc_mask: Option<Number>,
    source: Option<String>,
    result: Option<String>,
    op: Option<String>,
    state: Option<String>,
}

impl EventRequest {
    /// The alias parameters by field name.
    fn params(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("source", self.source.as_deref()),
            ("result", self.result.as_deref()),
            ("op", self.op.as_deref()),
            ("state", self.state.as_deref()),
        ]
    }
}

/// A JSON integer or "0x" hex string.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(try_from = "Token")]
struct Number(u64);

impl TryFrom<Token> for Number {
    type Error = String;

    fn try_from(token: Token) -> Result<Self, String> {
        match token {
            Token::Integer(value) => Ok(Self(value)),
            Token::Name(name) => Err(format!(
                "expected an integer or a \"0x\" hex string, got \"{name}\""
            )),
        }
    }
}

/// A JSON integer or string; a string that starts with "0x" is a hex integer.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Integer(u64),
    Name(String),
}

impl<'de> serde::Deserialize<'de> for Token {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TokenVisitor)
    }
}

struct TokenVisitor;

impl serde::de::Visitor<'_> for TokenVisitor {
    type Value = Token;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an integer or a string")
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Token, E> {
        Ok(Token::Integer(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Token, E> {
        u64::try_from(value)
            .map(Token::Integer)
            .map_err(|_| E::custom(format!("expected a non-negative integer, got {value}")))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Token, E> {
        let Some(digits) = value.strip_prefix("0x") else {
            return Ok(Token::Name(value.to_string()));
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(E::custom(format!("invalid hex integer \"{value}\"")));
        }
        u64::from_str_radix(digits, 16)
            .map(Token::Integer)
            .map_err(|_| E::custom(format!("hex integer \"{value}\" does not fit in 64 bits")))
    }
}

/// The counters and filters of one group, as an architecture programs them.
struct GroupProgram {
    counters: Vec<ProgrammedCounter>,
    filters: Option<ChaFilters>,
}

/// Programs one group of a box type; errors name the group's JSON path.
type ProgramGroup = fn(BoxType, &GroupRequest, &str) -> Result<GroupProgram, String>;

pub fn validate(
    request: PmuConfigRequest,
    topology: &PmuTopology,
    model: IntelServerCpuModel,
) -> Result<Vec<ProgrammedConfig>, String> {
    let program_group: ProgramGroup = match model {
        IntelServerCpuModel::SkylakeXeon => skx::program_group,
        _ => return Err(format!("PMU configs are not supported on {model:?}")),
    };

    let mut configs = Vec::new();
    for (box_type, box_request) in request.pmus.into_box_requests() {
        let program = program_box_type(box_type, box_request, topology, program_group)?;
        configs.push(ProgrammedConfig {
            config_id: config_id(&program),
            program,
        });
    }
    if configs.is_empty() {
        return Err("pmus: must program at least one PMU".to_string());
    }

    Ok(configs)
}

fn program_box_type(
    box_type: BoxType,
    request: BoxRequest,
    topology: &PmuTopology,
    program_group: ProgramGroup,
) -> Result<ProgrammedBoxType, String> {
    let path = format!("pmus.{}", box_type.label());
    let host_units = topology
        .units
        .get(&box_type)
        .filter(|packages| !packages.is_empty())
        .ok_or_else(|| format!("{path}: this host has no {} boxes", box_type.label()))?;
    let packages = select_packages(&path, request.packages.as_deref(), host_units)?;
    let grouping = match request.grouping {
        GroupingRequest::Fixed => Grouping::Fixed,
        GroupingRequest::Spatial => Grouping::Spatial,
        GroupingRequest::Temporal => Grouping::Temporal,
    };
    let group_count = request.groups.len();
    match grouping {
        Grouping::Fixed if group_count != 1 => {
            return Err(format!(
                "{path}.groups: fixed grouping takes exactly 1 group, got {group_count}"
            ));
        }
        Grouping::Spatial if group_count < 2 => {
            return Err(format!(
                "{path}.groups: spatial grouping takes at least 2 groups, got {group_count}"
            ));
        }
        Grouping::Temporal if group_count < 2 => {
            return Err(format!(
                "{path}.groups: temporal grouping takes at least 2 groups, got {group_count}"
            ));
        }
        _ => {}
    }

    let mut groups = Vec::with_capacity(group_count);
    for (index, group) in request.groups.iter().enumerate() {
        let group_path = format!("{path}.groups[{index}]");
        let index = u8::try_from(index)
            .map_err(|_| format!("{path}.groups: takes at most 256 groups, got {group_count}"))?;
        let units = resolve_units(
            &group_path,
            group.units.as_deref(),
            grouping,
            &packages,
            host_units,
        )?;
        let program = program_group(box_type, group, &group_path)?;
        if program.counters.is_empty() {
            return Err(format!("{group_path}: programs no counter"));
        }
        groups.push(ProgrammedGroup {
            index,
            counters: program.counters,
            filters: program.filters,
            units,
        });
    }
    check_shared_units(&path, grouping, &groups)?;

    Ok(ProgrammedBoxType {
        box_type,
        grouping,
        aggregate: match request.aggregate {
            AggregateRequest::Sum => Aggregate::Sum,
            AggregateRequest::Box => Aggregate::Box,
        },
        groups,
    })
}

fn select_packages(
    path: &str,
    requested: Option<&[Number]>,
    host_units: &BTreeMap<u16, BTreeSet<u16>>,
) -> Result<BTreeSet<u16>, String> {
    let Some(requested) = requested else {
        return Ok(host_units.keys().copied().collect());
    };
    if requested.is_empty() {
        return Err(format!("{path}.packages: must not be empty"));
    }

    let mut packages = BTreeSet::new();
    for (index, package) in requested.iter().enumerate() {
        let package_path = format!("{path}.packages[{index}]");
        let id = u16::try_from(package.0)
            .ok()
            .filter(|id| host_units.contains_key(id))
            .ok_or_else(|| {
                format!(
                    "{package_path}: package {} does not exist (packages: {})",
                    package.0,
                    join(host_units.keys())
                )
            })?;
        if !packages.insert(id) {
            return Err(format!("{package_path}: package {id} is listed twice"));
        }
    }

    Ok(packages)
}

fn resolve_units(
    path: &str,
    requested: Option<&[Number]>,
    grouping: Grouping,
    packages: &BTreeSet<u16>,
    host_units: &BTreeMap<u16, BTreeSet<u16>>,
) -> Result<BTreeMap<u16, BTreeSet<u16>>, String> {
    let Some(requested) = requested else {
        if grouping == Grouping::Spatial {
            return Err(format!(
                "{path}.units: spatial groups must list their units"
            ));
        }
        return Ok(packages
            .iter()
            .map(|package| (*package, host_units[package].clone()))
            .collect());
    };
    if requested.is_empty() {
        return Err(format!("{path}.units: must not be empty"));
    }

    let mut units = BTreeSet::new();
    for (index, unit) in requested.iter().enumerate() {
        let unit_path = format!("{path}.units[{index}]");
        let id = u16::try_from(unit.0).ok();
        if let Some(package) = packages
            .iter()
            .find(|package| !id.is_some_and(|id| host_units[*package].contains(&id)))
        {
            return Err(format!(
                "{unit_path}: unit {} does not exist in package {package} (units: {})",
                unit.0,
                join(&host_units[package])
            ));
        }
        let id = id.expect("units that exist in a package fit in u16");
        if !units.insert(id) {
            return Err(format!("{unit_path}: unit {id} is listed twice"));
        }
    }

    Ok(packages
        .iter()
        .map(|package| (*package, units.clone()))
        .collect())
}

fn check_shared_units(
    path: &str,
    grouping: Grouping,
    groups: &[ProgrammedGroup],
) -> Result<(), String> {
    match grouping {
        Grouping::Fixed => {}
        Grouping::Spatial => {
            for (later, group) in groups.iter().enumerate() {
                for earlier in &groups[..later] {
                    for (package, units) in &group.units {
                        if let Some(unit) = units.intersection(&earlier.units[package]).next() {
                            return Err(format!(
                                "{path}.groups[{later}].units: unit {unit} of package {package} is also in groups[{}]",
                                earlier.index
                            ));
                        }
                    }
                }
            }
        }
        Grouping::Temporal => {
            if let Some(group) = groups.iter().find(|group| group.units != groups[0].units) {
                return Err(format!(
                    "{path}.groups[{}].units: temporal groups must run on the units of groups[0]",
                    group.index
                ));
            }
        }
    }

    Ok(())
}

/// FNV-1a 64 over a canonical serialization of a box type's programming,
/// below 2^53.
fn config_id(program: &ProgrammedBoxType) -> u64 {
    let mut bytes = Vec::new();
    put_len(&mut bytes, program.box_type.label().len());
    bytes.extend(program.box_type.label().as_bytes());
    bytes.push(match program.grouping {
        Grouping::Fixed => 0,
        Grouping::Spatial => 1,
        Grouping::Temporal => 2,
    });
    bytes.push(match program.aggregate {
        Aggregate::Box => 0,
        Aggregate::Sum => 1,
    });
    put_len(&mut bytes, program.groups.len());
    for group in &program.groups {
        bytes.push(group.index);
        put_len(&mut bytes, group.units.len());
        for (package, units) in &group.units {
            bytes.extend(package.to_le_bytes());
            put_len(&mut bytes, units.len());
            for unit in units {
                bytes.extend(unit.to_le_bytes());
            }
        }
        match group.filters {
            None => bytes.push(0),
            Some(filters) => {
                bytes.push(1);
                bytes.extend(filters.filter0.to_le_bytes());
                bytes.extend(filters.filter1.to_le_bytes());
            }
        }
        put_len(&mut bytes, group.counters.len());
        for counter in &group.counters {
            match counter.slot {
                CounterSlot::General(index) => bytes.extend([0, index]),
                CounterSlot::Fixed => bytes.push(1),
            }
            bytes.extend(counter.control.to_le_bytes());
        }
    }

    let hash = bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    hash & (ID_LIMIT - 1)
}

fn put_len(bytes: &mut Vec<u8>, len: usize) {
    bytes.extend(
        u32::try_from(len)
            .expect("lengths fit in u32")
            .to_le_bytes(),
    );
}

fn join<T: fmt::Display>(values: impl IntoIterator<Item = T>) -> String {
    values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn topology() -> PmuTopology {
        let packages =
            |count: u16| BTreeMap::from([(0, (0..count).collect()), (1, (0..count).collect())]);

        PmuTopology {
            units: BTreeMap::from([
                (BoxType::Cha, packages(4)),
                (BoxType::Iio, packages(6)),
                (BoxType::Irp, packages(6)),
                (BoxType::Imc, packages(6)),
            ]),
        }
    }

    fn request(pmus: Value) -> Value {
        json!({"pmus": pmus})
    }

    fn pmu(box_type: &str, group: Value) -> Value {
        request(json!({box_type: {"groups": [group]}}))
    }

    fn clockticks() -> Value {
        json!({"events": [{"alias": "clockticks"}]})
    }

    fn parse(value: Value) -> Result<PmuConfigRequest, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn configs(value: Value) -> Result<Vec<ProgrammedConfig>, String> {
        validate(parse(value)?, &topology(), IntelServerCpuModel::SkylakeXeon)
    }

    fn validate_json(value: Value) -> Result<Vec<ProgrammedBoxType>, String> {
        Ok(configs(value)?
            .into_iter()
            .map(|config| config.program)
            .collect())
    }

    fn controls(group: &ProgrammedGroup) -> Vec<(CounterSlot, u64)> {
        group
            .counters
            .iter()
            .map(|counter| (counter.slot, counter.control))
            .collect()
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn programs_the_documented_request() {
        let configs = configs(json!({
            "pmus": {
                "cha": {
                    "grouping": "fixed",
                    "aggregate": "sum",
                    "packages": [0],
                    "groups": [{
                        "units": [0, 1, 2],
                        "filter0": {"states": ["M", "E", "SF_E"]},
                        "filter1": {"opcodes": ["ItoM"], "locality": "all"},
                        "events": [
                            {"alias": "tor_inserts", "source": "io", "result": "miss"},
                            {"event": "0x35", "umask": "0x34"},
                            null
                        ]
                    }]
                },
                "iio": {"groups": [{"units": [2], "events": [
                    {"alias": "comp_buf_inserts"},
                    {"event": "0xc2", "umask": "0x03", "ch_mask": "0x0f", "fc_mask": "0x04"}
                ]}]},
                "irp": {"groups": [{"events": [{"alias": "faf_inserts"}, {"alias": "faf_occupancy"}]}]},
                "imc": {"groups": [{"events": [
                    {"alias": "cas_count", "op": "rd"},
                    {"alias": "cas_count", "op": "wr"}
                ], "fixed": true}]}
            }
        }))
        .unwrap();

        assert!(configs.iter().all(|config| config.config_id < ID_LIMIT));
        let programs = configs
            .into_iter()
            .map(|config| config.program)
            .collect::<Vec<_>>();
        let [cha, iio, irp, imc] = programs.as_slice() else {
            panic!("expected four box types, got {programs:?}");
        };
        assert_eq!(cha.box_type, BoxType::Cha);
        assert_eq!(cha.grouping, Grouping::Fixed);
        assert_eq!(cha.aggregate, Aggregate::Sum);
        let group = &cha.groups[0];
        assert_eq!(group.index, 0);
        assert_eq!(
            group.units,
            BTreeMap::from([(0, BTreeSet::from([0, 1, 2]))])
        );
        assert_eq!(
            group.filters,
            Some(ChaFilters {
                filter0: (0x40 | 0x20 | 0x04) << 17,
                filter1: 0x49033,
            })
        );
        assert_eq!(
            controls(group),
            [
                (CounterSlot::General(0), 0x50_2435),
                (CounterSlot::General(1), 0x50_3435)
            ]
        );
        assert_eq!(
            group.counters[0].labels,
            labels(&[
                ("alias", "tor_inserts"),
                ("event", "0x35"),
                ("umask", "0x24"),
                ("filter0", "0xc80000"),
                ("filter1", "0x49033"),
            ])
        );
        assert_eq!(group.counters[1].labels["alias"], "raw");

        let iio_group = &iio.groups[0];
        assert_eq!(
            iio_group.units,
            BTreeMap::from([(0, BTreeSet::from([2])), (1, BTreeSet::from([2]))])
        );
        assert_eq!(iio_group.filters, None);
        assert_eq!(
            controls(iio_group),
            [
                (CounterSlot::General(0), 0x40f0_0050_03c2),
                (CounterSlot::General(1), 0x40f0_0050_03c2)
            ]
        );
        assert_eq!(
            iio_group.counters[0].labels,
            labels(&[
                ("alias", "comp_buf_inserts"),
                ("event", "0xc2"),
                ("umask", "0x03"),
                ("ch_mask", "0x0f"),
                ("fc_mask", "0x04"),
            ])
        );

        assert_eq!(
            controls(&irp.groups[0]),
            [
                (CounterSlot::General(0), 0x50_0018),
                (CounterSlot::General(1), 0x50_0019)
            ]
        );
        assert_eq!(irp.groups[0].units[&1], BTreeSet::from([0, 1, 2, 3, 4, 5]));

        assert_eq!(
            controls(&imc.groups[0]),
            [
                (CounterSlot::General(0), 0x50_0304),
                (CounterSlot::General(1), 0x50_0c04),
                (CounterSlot::Fixed, 0x40_0000)
            ]
        );
        assert_eq!(
            imc.groups[0].counters[2].labels,
            labels(&[("alias", "dclk")])
        );
    }

    /// The CHA collector's `ChaEventGroup::tor(ChaTorOpcode::ItoM)`, the
    /// mem-io-itom profile (src/metrics/cha/skx.rs:460-489, 673-674):
    /// filter0 `ChaFilter0::none()` = 0 (:463); filter1
    /// `ChaFilter1::total_opcode(0x248)` (:464, :494) with the default
    /// locality, `value()` (:153-174) = 0x49033, also Intel's FILTER_VALUE of
    /// UNC_CHA_TOR_INSERTS.IO_MISS_ITOM; counters
    /// `pmon::counter_control(0x35, umask, true)` (:786) with umasks 0x34,
    /// 0x24, 0x31, 0x21 (cha/mod.rs:247-257) = 0x523435, 0x522435, 0x523135,
    /// 0x522135, here without the reset bit (1 << 17).
    #[test]
    fn matches_the_cha_collector_itom_tor_group() {
        let programs = validate_json(pmu(
            "cha",
            json!({
                "filter1": {"opcodes": ["ItoM"]},
                "events": [
                    {"alias": "tor_inserts", "source": "io", "result": "all"},
                    {"alias": "tor_inserts", "source": "io", "result": "miss"},
                    {"alias": "tor_inserts", "source": "ia", "result": "all"},
                    {"alias": "tor_inserts", "source": "ia", "result": "miss"}
                ]
            }),
        ))
        .unwrap();
        let group = &programs[0].groups[0];

        assert_eq!(
            group.filters,
            Some(ChaFilters {
                filter0: 0,
                filter1: 0x49033,
            })
        );
        assert_eq!(
            controls(group),
            [
                (CounterSlot::General(0), 0x52_3435 & !(1 << 17)),
                (CounterSlot::General(1), 0x52_2435 & !(1 << 17)),
                (CounterSlot::General(2), 0x52_3135 & !(1 << 17)),
                (CounterSlot::General(3), 0x52_2135 & !(1 << 17)),
            ]
        );
    }

    #[test]
    fn fills_in_defaults() {
        let programs = validate_json(pmu("cha", clockticks())).unwrap();
        let cha = &programs[0];

        assert_eq!(cha.grouping, Grouping::Fixed);
        assert_eq!(cha.aggregate, Aggregate::Sum);
        assert_eq!(
            cha.groups[0].units,
            BTreeMap::from([
                (0, BTreeSet::from([0, 1, 2, 3])),
                (1, BTreeSet::from([0, 1, 2, 3]))
            ])
        );
        // The collector's ChaFilter1::total_all_opcodes() (cha/skx.rs:2508).
        assert_eq!(
            cha.groups[0].filters,
            Some(ChaFilters {
                filter0: 0,
                filter1: 0x3b,
            })
        );
        assert_eq!(
            cha.groups[0].counters[0].labels,
            labels(&[
                ("alias", "clockticks"),
                ("event", "0x00"),
                ("umask", "0x00"),
                ("filter0", "0x0"),
                ("filter1", "0x3b"),
            ])
        );
    }

    #[test]
    fn encodes_cha_filters() {
        let cases = [
            (
                json!({"filter0": {"states": ["M", "E", "SF_E"]}}),
                0xc8_0000,
                0x3b,
            ),
            (
                json!({"filter0": {"states": ["I", "SF_S", "SF_E", "SF_H", "S", "E", "M", "F"]}}),
                0x1fe_0000,
                0x3b,
            ),
            (json!({"filter0": {"raw": "0x123"}}), 0x123, 0x3b),
            (json!({"filter1": {}}), 0, 0x3b),
            (json!({"filter1": {"locality": "local"}}), 0, 0x3a),
            (json!({"filter1": {"locality": "remote"}}), 0, 0x39),
            (json!({"filter1": {"opcodes": ["ItoM"]}}), 0, 0x4_9033),
            (
                json!({"filter1": {"opcodes": ["ItoM", "RdCur"], "locality": "local"}}),
                0,
                0x10f4_9032,
            ),
            (json!({"filter1": {"opcodes": [0]}}), 0, 0x33),
            (json!({"filter1": {"opcodes": ["0x180"]}}), 0, 0x3_0033),
            (json!({"filter1": {"raw": "0x40433"}}), 0, 0x4_0433),
        ];

        for (filters, filter0, filter1) in cases {
            let mut group = clockticks();
            group
                .as_object_mut()
                .unwrap()
                .extend(filters.as_object().unwrap().clone());
            let programs = validate_json(pmu("cha", group)).unwrap();

            assert_eq!(
                programs[0].groups[0].filters,
                Some(ChaFilters { filter0, filter1 }),
                "{filters}"
            );
        }
    }

    #[test]
    fn encodes_iio_masks() {
        let programs = validate_json(pmu(
            "iio",
            json!({"events": [
                {"alias": "data_req_of_cpu", "op": "mem_read"},
                {"alias": "data_req_of_cpu", "op": "mem_write", "ch_mask": "0x01", "fc_mask": 4},
                {"alias": "comp_buf_occupancy"},
                {"event": "0xc0", "umask": "0x04"}
            ]}),
        ))
        .unwrap();
        let group = &programs[0].groups[0];
        let masks = |counter: &ProgrammedCounter| {
            (
                counter.labels["umask"].clone(),
                counter.labels["ch_mask"].clone(),
                counter.labels["fc_mask"].clone(),
            )
        };

        assert_eq!(
            group.counters.iter().map(masks).collect::<Vec<_>>(),
            [
                ("0x04".to_string(), "0x0f".to_string(), "0x07".to_string()),
                ("0x01".to_string(), "0x01".to_string(), "0x04".to_string()),
                ("0x0f".to_string(), "0x00".to_string(), "0x04".to_string()),
                ("0x04".to_string(), "0x00".to_string(), "0x00".to_string()),
            ]
        );
        assert_eq!(
            controls(group),
            [
                (CounterSlot::General(0), 0x70f0_0050_0483),
                (CounterSlot::General(1), 0x4010_0050_0183),
                (CounterSlot::General(2), 0x4000_0050_0fd5),
                (CounterSlot::General(3), 0x50_04c0),
            ]
        );
    }

    #[test]
    fn programs_spatial_and_temporal_groups() {
        let programs = validate_json(request(json!({
            "cha": {"grouping": "spatial", "aggregate": "box", "groups": [
                {"units": [0, 1], "events": [{"alias": "clockticks"}]},
                {"units": [2, 3], "events": [{"alias": "sf_eviction", "state": "m"}]}
            ]},
            "irp": {"grouping": "temporal", "packages": [1], "groups": [
                {"events": [{"alias": "coherent_ops", "op": "pcirdcur"}]},
                {"units": [0, 1, 2, 3, 4, 5], "events": [null, {"alias": "clockticks"}]}
            ]}
        })))
        .unwrap();
        let [cha, irp] = programs.as_slice() else {
            panic!("expected two box types, got {programs:?}");
        };

        assert_eq!(cha.grouping, Grouping::Spatial);
        assert_eq!(cha.aggregate, Aggregate::Box);
        assert_eq!(cha.groups[1].index, 1);
        assert_eq!(cha.groups[1].units[&1], BTreeSet::from([2, 3]));
        assert_eq!(irp.grouping, Grouping::Temporal);
        assert_eq!(irp.groups[0].units, irp.groups[1].units);
        assert_eq!(irp.groups[0].units.keys().collect::<Vec<_>>(), [&1]);
        assert_eq!(
            controls(&irp.groups[1]),
            [(CounterSlot::General(1), 0x50_0001)]
        );
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
        let iio_event = |event: Value| pmu("iio", json!({"events": [event]}));
        let cases = [
            (request(json!({})), "pmus: must program at least one PMU"),
            (
                request(json!({"cha": {"groups": []}})),
                "pmus.cha.groups: fixed grouping takes exactly 1 group, got 0",
            ),
            (
                request(json!({"cha": {"groups": [clockticks(), clockticks()]}})),
                "pmus.cha.groups: fixed grouping takes exactly 1 group, got 2",
            ),
            (
                request(json!({"cha": {"grouping": "spatial", "groups": [
                    {"units": [0], "events": [{"alias": "clockticks"}]}
                ]}})),
                "pmus.cha.groups: spatial grouping takes at least 2 groups, got 1",
            ),
            (
                request(json!({"cha": {"grouping": "spatial", "groups": [
                    clockticks(),
                    {"units": [0], "events": [{"alias": "clockticks"}]}
                ]}})),
                "pmus.cha.groups[0].units: spatial groups must list their units",
            ),
            (
                request(json!({"cha": {"grouping": "spatial", "groups": [
                    {"units": [0, 1], "events": [{"alias": "clockticks"}]},
                    {"units": [2, 1], "events": [{"alias": "clockticks"}]}
                ]}})),
                "pmus.cha.groups[1].units: unit 1 of package 0 is also in groups[0]",
            ),
            (
                request(json!({"irp": {"grouping": "temporal", "groups": [clockticks()]}})),
                "pmus.irp.groups: temporal grouping takes at least 2 groups, got 1",
            ),
            (
                request(json!({"irp": {"grouping": "temporal", "groups": [
                    clockticks(),
                    {"units": [0], "events": [{"alias": "clockticks"}]}
                ]}})),
                "pmus.irp.groups[1].units: temporal groups must run on the units of groups[0]",
            ),
            (
                request(json!({"cha": {"packages": [2], "groups": [clockticks()]}})),
                "pmus.cha.packages[0]: package 2 does not exist (packages: 0, 1)",
            ),
            (
                request(json!({"cha": {"packages": [0, 0], "groups": [clockticks()]}})),
                "pmus.cha.packages[1]: package 0 is listed twice",
            ),
            (
                request(json!({"cha": {"packages": [], "groups": [clockticks()]}})),
                "pmus.cha.packages: must not be empty",
            ),
            (
                pmu(
                    "cha",
                    json!({"units": [4], "events": [{"alias": "clockticks"}]}),
                ),
                "pmus.cha.groups[0].units[0]: unit 4 does not exist in package 0 (units: 0, 1, 2, 3)",
            ),
            (
                pmu(
                    "cha",
                    json!({"units": [1, 1], "events": [{"alias": "clockticks"}]}),
                ),
                "pmus.cha.groups[0].units[1]: unit 1 is listed twice",
            ),
            (
                pmu(
                    "cha",
                    json!({"units": [], "events": [{"alias": "clockticks"}]}),
                ),
                "pmus.cha.groups[0].units: must not be empty",
            ),
            (
                pmu("cha", json!({"events": [null]})),
                "pmus.cha.groups[0]: programs no counter",
            ),
            (
                pmu("imc", json!({})),
                "pmus.imc.groups[0]: programs no counter",
            ),
            (
                pmu(
                    "cha",
                    json!({"events": vec![json!({"alias": "clockticks"}); 5]}),
                ),
                "pmus.cha.groups[0].events: Skylake-SP CHA boxes have 4 general counters, got 5 events",
            ),
            (
                pmu(
                    "irp",
                    json!({"events": [null, null, {"alias": "clockticks"}]}),
                ),
                "pmus.irp.groups[0].events: Skylake-SP IRP boxes have 2 general counters, got 3 events",
            ),
            (
                cha_event(json!({"event": "0xff"})),
                "pmus.cha.groups[0].events[0]: Intel lists no Skylake-SP CHA event 0xff",
            ),
            (
                pmu("irp", json!({"events": [{"event": "0x20"}]})),
                "pmus.irp.groups[0].events[0]: Intel lists no Skylake-SP IRP event 0x20",
            ),
            (
                pmu(
                    "cha",
                    json!({"events": [
                        {"alias": "clockticks"},
                        {"event": "0x36", "umask": "0x34"}
                    ]}),
                ),
                "pmus.cha.groups[0].events[1]: event 0x36 counts only on counters 0",
            ),
            (
                pmu(
                    "cha",
                    json!({"events": [
                        null,
                        {"alias": "tor_occupancy", "source": "io", "result": "all"}
                    ]}),
                ),
                "pmus.cha.groups[0].events[1]: event 0x36 counts only on counters 0",
            ),
            (
                iio_event(json!({"alias": "comp_buf_occupancy"})),
                "pmus.iio.groups[0].events[0]: event 0xd5 counts only on counters 2, 3",
            ),
            (
                pmu(
                    "iio",
                    json!({"events": [null, null, {"alias": "data_req_of_cpu", "op": "msg"}]}),
                ),
                "pmus.iio.groups[0].events[2]: event 0x83 counts only on counters 0, 1",
            ),
            (
                pmu(
                    "iio",
                    json!({"filter0": {"states": ["M"]}, "events": [{"alias": "clockticks"}]}),
                ),
                "pmus.iio.groups[0].filter0: only CHA groups take filters",
            ),
            (
                pmu("imc", json!({"filter1": {}, "fixed": true})),
                "pmus.imc.groups[0].filter1: only CHA groups take filters",
            ),
            (
                pmu("cha", json!({"fixed": true})),
                "pmus.cha.groups[0].fixed: only IMC boxes have a fixed counter on Skylake-SP",
            ),
            (
                cha_event(json!({"alias": "cas_count", "op": "rd"})),
                "pmus.cha.groups[0].events[0].alias: unknown Skylake-SP CHA alias \"cas_count\" (aliases: clockticks, tor_inserts, tor_occupancy, llc_lookup, llc_victims, sf_eviction)",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "source": "io"})),
                "pmus.cha.groups[0].events[0]: tor_inserts needs a result (all, hit, miss)",
            ),
            (
                cha_event(json!({"alias": "tor_inserts", "source": "pcie", "result": "all"})),
                "pmus.cha.groups[0].events[0].source: tor_inserts source must be one of ia, io, all, got \"pcie\"",
            ),
            (
                cha_event(json!({"alias": "clockticks", "op": "any"})),
                "pmus.cha.groups[0].events[0].op: clockticks takes no op",
            ),
            (
                cha_event(json!({"alias": "clockticks", "umask": 0})),
                "pmus.cha.groups[0].events[0].umask: alias clockticks sets its own umask",
            ),
            (
                cha_event(json!({"alias": "clockticks", "event": 0})),
                "pmus.cha.groups[0].events[0]: takes an alias or an event, not both",
            ),
            (
                cha_event(json!({})),
                "pmus.cha.groups[0].events[0]: needs an alias or an event",
            ),
            (
                cha_event(json!({"event": "0x35", "umask": "0x34", "source": "io"})),
                "pmus.cha.groups[0].events[0].source: only aliases take a source",
            ),
            (
                cha_event(json!({"alias": "clockticks", "ch_mask": 1})),
                "pmus.cha.groups[0].events[0].ch_mask: only IIO events take a ch_mask",
            ),
            (
                iio_event(json!({"alias": "clockticks", "fc_mask": 8})),
                "pmus.iio.groups[0].events[0].fc_mask: must be at most 0x07, got 0x8",
            ),
            (
                iio_event(json!({"alias": "clockticks", "ch_mask": "0x100"})),
                "pmus.iio.groups[0].events[0].ch_mask: must be at most 0xff, got 0x100",
            ),
            (
                cha_event(json!({"event": 256})),
                "pmus.cha.groups[0].events[0].event: must be at most 0xff, got 0x100",
            ),
            (
                cha_event(json!({"event": 0, "umask": "0x1ff"})),
                "pmus.cha.groups[0].events[0].umask: must be at most 0xff, got 0x1ff",
            ),
            (
                cha_filters(json!({"filter0": {"states": ["SF_M"]}})),
                "pmus.cha.groups[0].filter0.states[0]: unknown state \"SF_M\" (states: I, SF_S, SF_E, SF_H, S, E, M, F)",
            ),
            (
                cha_filters(json!({"filter0": {"states": []}})),
                "pmus.cha.groups[0].filter0.states: must not be empty",
            ),
            (
                cha_filters(json!({"filter0": {"states": ["M"], "raw": 0}})),
                "pmus.cha.groups[0].filter0: takes states or raw, not both",
            ),
            (
                cha_filters(json!({"filter0": {}})),
                "pmus.cha.groups[0].filter0: needs states or raw",
            ),
            (
                cha_filters(json!({"filter0": {"raw": "0x100000000"}})),
                "pmus.cha.groups[0].filter0.raw: filter registers are 32 bits, got 0x100000000",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["ItoM", "RFO", "DRd"]}})),
                "pmus.cha.groups[0].filter1.opcodes: takes 1 or 2 opcodes, got 3",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": []}})),
                "pmus.cha.groups[0].filter1.opcodes: takes 1 or 2 opcodes, got 0",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["ItoM", "PCIRdCur"]}})),
                "pmus.cha.groups[0].filter1.opcodes[1]: unknown Skylake-SP opcode \"PCIRdCur\" (opcodes: RFO, CRd, DRd,",
            ),
            (
                cha_filters(json!({"filter1": {"opcodes": ["0x400"]}})),
                "pmus.cha.groups[0].filter1.opcodes[0]: opcodes are 10 bits, got 0x400",
            ),
            (
                cha_filters(json!({"filter1": {"raw": 0, "locality": "local"}})),
                "pmus.cha.groups[0].filter1: raw excludes opcodes and locality",
            ),
            (
                cha_filters(json!({"filter1": {"raw": "0x100000000"}})),
                "pmus.cha.groups[0].filter1.raw: filter registers are 32 bits, got 0x100000000",
            ),
            (
                cha_event(json!({"alias": "llc_lookup", "op": "any"})),
                "pmus.cha.groups[0].events[0]: LLC_LOOKUP (event 0x34) counts nothing without filter0 states",
            ),
            (
                pmu(
                    "cha",
                    json!({
                        "filter0": {"raw": "0x1"},
                        "events": [{"alias": "llc_lookup", "op": "any"}]
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
        ];

        for (request, expected) in cases {
            let error = validate_json(request.clone()).unwrap_err();
            assert!(
                error.starts_with(expected),
                "{request}\nexpected: {expected}\n     got: {error}"
            );
        }
    }

    #[test]
    fn rejects_box_types_the_host_lacks() {
        let mut topology = topology();
        topology.units.remove(&BoxType::Imc);
        let request = parse(pmu("imc", json!({"fixed": true}))).unwrap();

        assert_eq!(
            validate(request, &topology, IntelServerCpuModel::SkylakeXeon).unwrap_err(),
            "pmus.imc: this host has no imc boxes"
        );
    }

    #[test]
    fn rejects_other_models() {
        let request = parse(pmu("cha", clockticks())).unwrap();

        assert_eq!(
            validate(request, &topology(), IntelServerCpuModel::IceLakeXeon).unwrap_err(),
            "PMU configs are not supported on IceLakeXeon"
        );
    }

    #[test]
    fn parses_numbers_and_rejects_unknown_fields() {
        let with_package = |package: Value| {
            let request =
                request(json!({"cha": {"packages": [package], "groups": [clockticks()]}}));
            parse(request).map(|request| request.pmus.cha.unwrap().packages.unwrap()[0].0)
        };
        assert_eq!(with_package(json!("0x11")), Ok(17));
        assert_eq!(with_package(json!("0xFf")), Ok(255));

        let cases = [
            (
                json!("17"),
                "expected an integer or a \"0x\" hex string, got \"17\"",
            ),
            (json!(-1), "expected a non-negative integer, got -1"),
            (json!(1.5), "invalid type: floating point `1.5`"),
            (json!("0xzz"), "invalid hex integer \"0xzz\""),
            (json!("0x+1"), "invalid hex integer \"0x+1\""),
            (json!("0x"), "invalid hex integer \"0x\""),
            (
                json!("0x10000000000000000"),
                "hex integer \"0x10000000000000000\" does not fit in 64 bits",
            ),
        ];
        for (package, expected) in cases {
            let error = with_package(package.clone()).unwrap_err();
            assert!(error.contains(expected), "{package}: {error}");
        }

        let with_top_level = |field: &str| {
            let mut request = pmu("cha", clockticks());
            request[field] = json!(1);
            request
        };
        let cases = [
            (with_top_level("lease"), "unknown field `lease`"),
            (with_top_level("window_id"), "unknown field `window_id`"),
            (
                request(json!({"upi": {"groups": []}})),
                "unknown field `upi`",
            ),
            (
                request(json!({"cha": {"groups": [], "partitions": 2}})),
                "unknown field `partitions`",
            ),
            (
                request(json!({"cha": {"grouping": "rotating", "groups": []}})),
                "unknown variant `rotating`",
            ),
            (
                pmu("cha", json!({"events": [], "counters": 4})),
                "unknown field `counters`",
            ),
            (
                pmu("cha", json!({"filter1": {"opcode": "ItoM"}})),
                "unknown field `opcode`",
            ),
            (
                pmu("cha", json!({"filter1": {"locality": "home"}})),
                "unknown variant `home`",
            ),
            (
                pmu(
                    "cha",
                    json!({"events": [{"alias": "clockticks", "counter": 0}]}),
                ),
                "unknown field `counter`",
            ),
        ];
        for (request, expected) in cases {
            let error = parse(request.clone()).unwrap_err();
            assert!(error.contains(expected), "{request}: {error}");
        }
    }

    #[test]
    fn names_the_box_types_a_request_programs() {
        let request = parse(request(
            json!({"cha": {"groups": []}, "imc": {"groups": []}}),
        ));

        assert_eq!(
            request.unwrap().box_types(),
            BTreeSet::from([BoxType::Cha, BoxType::Imc])
        );
    }

    #[test]
    fn config_id_is_per_box_type_and_tracks_its_programming() {
        let cha =
            |umask: &str| json!({"groups": [{"events": [{"event": "0x35", "umask": umask}]}]});
        let imc = || json!({"groups": [{"fixed": true}]});
        let config_id = |pmus: Value, box_type: BoxType| {
            configs(request(pmus))
                .unwrap()
                .into_iter()
                .find(|config| config.program.box_type == box_type)
                .unwrap()
                .config_id
        };
        let base = config_id(json!({"cha": cha("0x34")}), BoxType::Cha);

        assert!(base < ID_LIMIT);
        assert_eq!(config_id(json!({"cha": cha("0x34")}), BoxType::Cha), base);
        // Other box types in the request leave a box type's id alone.
        let both = json!({"cha": cha("0x34"), "imc": imc()});
        assert_eq!(config_id(both.clone(), BoxType::Cha), base);
        assert_eq!(
            config_id(both, BoxType::Imc),
            config_id(json!({"imc": imc()}), BoxType::Imc)
        );
        // The same programming by alias.
        assert_eq!(
            config_id(
                json!({"cha": {"groups": [{"events": [
                    {"alias": "tor_inserts", "source": "io", "result": "all"}
                ]}]}}),
                BoxType::Cha
            ),
            base
        );

        assert_ne!(config_id(json!({"cha": cha("0x31")}), BoxType::Cha), base);
        let mut units = cha("0x34");
        units["groups"][0]["units"] = json!([0]);
        assert_ne!(config_id(json!({"cha": units}), BoxType::Cha), base);
        let mut aggregate = cha("0x34");
        aggregate["aggregate"] = json!("box");
        assert_ne!(config_id(json!({"cha": aggregate}), BoxType::Cha), base);
    }
}
