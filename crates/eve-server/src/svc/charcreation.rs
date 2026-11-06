use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use eve_marshal::Value;
use tracing::{debug, error, info, warn};

use crate::sde::{self, Row};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{Db, Store, StoreError, Tx, Value as Cell};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::rows::rowset;

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetCharCreationInfo" => cached(&CREATION_INFO, CREATION, "GetCharCreationInfo"),
        // Trinity's creator merges this over GetCharCreationInfo: its careers
        // and specialities, and the skills (with levels) each grants.
        "GetCharNewExtraCreationInfo" => {
            cached(&EXTRA_CREATION_INFO, EXTRA_CREATION, "GetCharNewExtraCreationInfo")
        }
        "GetAppearanceInfo" => cached(&APPEARANCE_INFO, APPEARANCE, "GetAppearanceInfo"),
        "ValidateName" => validate_name(call, ctx),
        "ValidateNameEx" => validate_name_ex(call, ctx),
        "GetCharacterAppearanceList" => {
            let ids: Vec<i32> = call
                .args
                .first()
                .and_then(|v| v.seq())
                .map(|s| {
                    s.iter()
                        .filter_map(|v| v.as_i64())
                        .filter_map(|v| i32::try_from(v).ok())
                        .collect()
                })
                .unwrap_or_default();
            character_appearance_list(ctx, &ids)
        }
        "CreateCharacter" => return Some(create_character(call, ctx).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

struct Catalogue {
    key: &'static str,
    source: Option<Source>,
    header: &'static [&'static str],
    /// Sent as a list of packed blue.DBRows rather than a util.Rowset: for a
    /// catalogue the client reads through a row's `__header__` (the
    /// descriptor's (name, type) pairs), which only a DBRow has.
    packed: bool,
}

const fn c(key: &'static str, header: &'static [&'static str], source: Source) -> Catalogue {
    Catalogue { key, source: Some(source), header, packed: false }
}

const fn packed(key: &'static str, header: &'static [&'static str], source: Source) -> Catalogue {
    Catalogue { key, source: Some(source), header, packed: true }
}

const fn empty(key: &'static str, header: &'static [&'static str]) -> Catalogue {
    Catalogue { key, source: None, header, packed: false }
}

/// The static table a catalogue's lines come from, and which of its rows
/// and columns they carry.
#[derive(Clone, Copy)]
struct Source {
    table: &'static str,
    /// The line's values, one per header name; when not given, the columns
    /// the header names, as stored.
    columns: Option<&'static [Col]>,
    /// Only the looks a player may pick (`npc = 0`); the rest are NPCs'.
    players_only: bool,
    /// Sorted by these integer columns; otherwise in the table's own order,
    /// which the client has always been sent.
    order_by: &'static [&'static str],
}

const fn from(table: &'static str) -> Source {
    Source { table, columns: None, players_only: false, order_by: &[] }
}

impl Source {
    const fn columns(self, columns: &'static [Col]) -> Source {
        Source { columns: Some(columns), ..self }
    }

    const fn order_by(self, order_by: &'static [&'static str]) -> Source {
        Source { order_by, ..self }
    }

    const fn players_only(self) -> Source {
        Source { players_only: true, ..self }
    }

    fn lines(&self, header: &[&str]) -> Vec<Value> {
        let mut rows: Vec<Row> = sde::table(self.table)
            .rows()
            .filter(|r| !self.players_only || r.int("npc") == Some(0))
            .collect();
        rows.sort_by(|a, b| {
            self.order_by
                .iter()
                .map(|c| nulls_last(a.int(c), b.int(c)))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        rows.iter()
            .map(|r| match self.columns {
                Some(columns) => Value::List(columns.iter().map(|c| c.cell(r)).collect()),
                None => Value::List(r.line(header)),
            })
            .collect()
    }
}

/// How one value of a catalogue line is read from its row.
#[derive(Clone, Copy)]
enum Col {
    /// The column as stored.
    Plain(&'static str),
    /// A text column, NULL sent as an empty string.
    OrEmpty(&'static str),
    /// A number the table keeps as text, sent as an integer (blank as None).
    Number(&'static str),
}

use Col::{Number, OrEmpty, Plain};

impl Col {
    fn cell(self, r: &Row) -> Value {
        match self {
            Plain(c) => r.cell(c),
            OrEmpty(c) => Value::text(r.text(c).unwrap_or("")),
            Number(c) => number(r, c).map_or(Value::None, |v| Value::Int(v.into())),
        }
    }
}

/// A number kept in a text column (chrAncestries."typeID2"), blank as None.
fn number(r: &Row, column: &str) -> Option<i32> {
    r.text(column).map(|s| s.trim_matches(' ')).filter(|s| !s.is_empty()).and_then(|s| s.parse().ok())
}

/// Ascending, NULLs last, as the database sorts.
fn nulls_last(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
    }
}

const CREATION: &[Catalogue] = &[
    c(
        "races",
        &["raceID", "raceName", "description", "skillTypeID1", "typeID", "typeQuantity", "graphicID", "shortDescription", "_raceName"],
        from("chrRaces").order_by(&["raceID"]).columns(&[
            Plain("raceID"), Plain("raceName"), Plain("description"), Plain("skillTypeID1"), Plain("typeID"),
            Plain("typeQuantity"), Plain("graphicID"), OrEmpty("shortDescription"), Plain("raceName"),
        ]),
    ),
    c(
        "bloodlines",
        &[
            "bloodlineID", "bloodlineName", "raceID", "description", "maleDescription",
            "femaleDescription", "shipTypeID", "corporationID", "perception", "willpower",
            "charisma", "memory", "intelligence", "bonusTypeID", "skillTypeID1", "skillTypeID2",
            "graphicID", "shortDescription", "shortMaleDescription", "shortFemaleDescription",
            "_bloodlineName",
        ],
        from("chrBloodlines").order_by(&["bloodlineID"]).columns(&[
            Plain("bloodlineID"), Plain("bloodlineName"), Plain("raceID"), Plain("description"), Plain("maleDescription"),
            Plain("femaleDescription"), Plain("shipTypeID"), Plain("corporationID"), Plain("perception"), Plain("willpower"),
            Plain("charisma"), Plain("memory"), Plain("intelligence"), Plain("bonusTypeID"), Plain("skillTypeID1"),
            Plain("skillTypeID2"), Plain("graphicID"), OrEmpty("shortDescription"), OrEmpty("shortMaleDescription"),
            OrEmpty("shortFemaleDescription"), Plain("bloodlineName"),
        ]),
    ),
    // nextgencharcreation_base.py GetAttributesFromStep walks an ancestry's
    // `__header__` to find its attribute bonuses by name.
    packed(
        "ancestries",
        &[
            "ancestryID", "ancestryName", "bloodlineID", "description", "postPerception",
            "postWillpower", "postCharisma", "postMemory", "postIntelligence", "perception",
            "willpower", "charisma", "memory", "intelligence", "skillTypeID1", "skillTypeID2",
            "typeID", "typeQuantity", "typeID2", "typeQuantity2", "graphicID", "shortDescription",
        ],
        from("chrAncestries").order_by(&["ancestryID"]).columns(&[
            Plain("ancestryID"), Plain("ancestryName"), Plain("bloodlineID"), Plain("description"),
            Plain("postPerception"), Plain("postWillpower"), Plain("postCharisma"), Plain("postMemory"), Plain("postIntelligence"),
            Plain("perception"), Plain("willpower"), Plain("charisma"), Plain("memory"), Plain("intelligence"),
            Plain("skillTypeID1"), Plain("skillTypeID2"), Plain("typeID"), Plain("typeQuantity"),
            Number("typeID2"), Plain("typeQuantity2"),
            Plain("graphicID"), OrEmpty("shortDescription"),
        ]),
    ),
    c(
        "attributes",
        &["attributeID", "attributeName", "description", "graphicID", "shortDescription", "_attributeName"],
        from("chrAttributes").order_by(&["attributeID"]).columns(&[
            Plain("attributeID"), Plain("attributeName"), Plain("description"), Plain("graphicID"), OrEmpty("shortDescription"),
            Plain("attributeName"),
        ]),
    ),
    c(
        "schools",
        &[
            "raceID", "schoolID", "schoolName", "description", "graphicID", "corporationID",
            "agentID", "newAgentID", "careerID",
        ],
        from("chrSchools").order_by(&["schoolID"]),
    ),
    c(
        "departments",
        &[
            "schoolID", "departmentID", "departmentName", "description", "skillTypeID1",
            "skillTypeID2", "skillTypeID3", "graphicID",
        ],
        from("chrDepartments").order_by(&["departmentID"]),
    ),
    c(
        "fields",
        &[
            "departmentID", "fieldID", "fieldName", "description", "skillTypeID1", "skillTypeID2",
            "typeID", "typeQuantity", "graphicID",
        ],
        from("chrFields").order_by(&["fieldID"]),
    ),
    c(
        "specialities",
        &[
            "fieldID", "specialityID", "specialityName", "description", "skillTypeID1",
            "skillTypeID2", "typeID", "typeQuantity", "graphicID",
        ],
        from("chrSpecialities").order_by(&["specialityID"]),
    ),
    c("skins", BL_HEADER_SKIN, from("chrBLSkins").players_only()),
    c("eyes", BL_HEADER_EYES, from("chrBLEyes").players_only()),
    c("eyebrows", BL_HEADER_EYEBROWS, from("chrBLEyebrows").players_only()),
    c("makeups", BL_HEADER_MAKEUP, from("chrBLMakeups").players_only()),
    c("lipsticks", BL_HEADER_LIPSTICK, from("chrBLLipsticks").players_only()),
    c("decos", BL_HEADER_DECO, from("chrBLDecos").players_only()),
    c("hairs", BL_HEADER_HAIR, from("chrBLHairs").players_only()),
    c("beards", BL_HEADER_BEARD, from("chrBLBeards").players_only()),
    c("accessories", BL_HEADER_ACCESSORY, from("chrBLAccessories").players_only()),
    c("costumes", BL_HEADER_COSTUME, from("chrBLCostumes").players_only()),
    c("lights", &["lightID", "lightName"], from("chrBLLights")),
    c("backgrounds", &["backgroundID", "backgroundName"], from("chrBLBackgrounds")),
    empty("benefits", &["benefitID", "benefitName", "description"]),
    empty("handicaps", &["handicapID", "handicapName", "description"]),
];

const BL_HEADER_SKIN: &[&str] = &["bloodlineID", "gender", "skinID"];
const BL_HEADER_EYES: &[&str] = &["bloodlineID", "gender", "eyesID"];
const BL_HEADER_EYEBROWS: &[&str] = &["bloodlineID", "gender", "eyebrowsID"];
const BL_HEADER_MAKEUP: &[&str] = &["bloodlineID", "gender", "makeupID"];
const BL_HEADER_LIPSTICK: &[&str] = &["bloodlineID", "gender", "lipstickID"];
const BL_HEADER_DECO: &[&str] = &["bloodlineID", "gender", "decoID"];
const BL_HEADER_HAIR: &[&str] = &["bloodlineID", "gender", "hairID"];
const BL_HEADER_BEARD: &[&str] = &["bloodlineID", "gender", "beardID"];
const BL_HEADER_ACCESSORY: &[&str] = &["bloodlineID", "gender", "accessoryID"];
const BL_HEADER_COSTUME: &[&str] = &["bloodlineID", "gender", "costumeID"];

const APPEARANCE: &[Catalogue] = &[
    c("skins", &["skinID", "skinName"], from("chrSkins")),
    c("eyes", &["eyesID", "eyesName"], from("chrEyes")),
    c("eyebrows", &["eyebrowsID", "eyebrowsName"], from("chrEyebrows")),
    c("makeups", &["makeupID", "makeupName"], from("chrMakeups")),
    c("lipsticks", &["lipstickID", "lipstickName"], from("chrLipsticks")),
    c("decos", &["decoID", "decoName"], from("chrDecos")),
    c("hairs", &["hairID", "hairName"], from("chrHairs")),
    c("beards", &["beardID", "beardName"], from("chrBeards")),
    c("accessories", &["accessoryID", "accessoryName"], from("chrAccessories")),
    c("costumes", &["costumeID", "costumeName"], from("chrCostumes")),
    c("lights", &["lightID", "lightName"], from("chrLights")),
    c("backgrounds", &["backgroundID", "backgroundName"], from("chrBackgrounds")),
];

/// `specialities` here replaces the old field-based one in the merged info.
const EXTRA_CREATION: &[Catalogue] = &[
    c(
        "careers",
        &["raceID", "careerID", "careerName", "description", "shortDescription", "graphicID", "schoolID", "_careerName"],
        from("chrCareers").order_by(&["careerID"]).columns(&[
            Plain("raceID"), Plain("careerID"), Plain("careerName"), Plain("description"), OrEmpty("shortDescription"),
            Plain("graphicID"), Plain("schoolID"), Plain("careerName"),
        ]),
    ),
    c(
        "specialities",
        &["careerID", "specialityID", "specialityName", "description", "shortDescription", "graphicID", "departmentID", "_specialityName"],
        from("chrCareerSpecialities").order_by(&["specialityID"]).columns(&[
            Plain("careerID"), Plain("specialityID"), Plain("specialityName"), Plain("description"), OrEmpty("shortDescription"),
            Plain("graphicID"), Plain("departmentID"), Plain("specialityName"),
        ]),
    ),
    c(
        "careerskills",
        &["careerID", "skillTypeID", "levels"],
        from("chrCareerSkills").order_by(&["careerID", "skillTypeID"]),
    ),
    c(
        "specialityskills",
        &["specialityID", "skillTypeID", "levels"],
        from("chrCareerSpecialitySkills").order_by(&["specialityID", "skillTypeID"]),
    ),
    c(
        "raceskills",
        &["raceID", "skillTypeID", "levels"],
        from("chrRaceSkills").order_by(&["raceID", "skillTypeID"]),
    ),
];

static CREATION_INFO: OnceLock<Value> = OnceLock::new();
static EXTRA_CREATION_INFO: OnceLock<Value> = OnceLock::new();
static APPEARANCE_INFO: OnceLock<Value> = OnceLock::new();

fn cached(cell: &'static OnceLock<Value>, specs: &'static [Catalogue], what: &str) -> Value {
    cell.get_or_init(|| build(specs, what)).clone()
}

fn build(specs: &[Catalogue], what: &str) -> Value {
    let mut pairs = Vec::with_capacity(specs.len());
    let mut total = 0usize;
    for spec in specs {
        let value = match &spec.source {
            None => rowset(spec.header, Vec::new()),
            Some(source) => {
                let lines = source.lines(spec.header);
                total += lines.len();
                if spec.packed {
                    packed_rows(spec.header, lines)
                } else {
                    rowset(spec.header, lines)
                }
            }
        };
        pairs.push((Value::bytes(spec.key), value));
    }
    debug!("{what}: built {} catalogues, {total} rows", specs.len());
    Value::Dict(pairs)
}

/// Query lines as packed DBRows sharing one descriptor.
fn packed_rows(header: &[&str], lines: Vec<Value>) -> Value {
    let names: Vec<Value> = header.iter().map(|h| Value::bytes(*h)).collect();
    let descriptor = std::sync::Arc::new(crate::bulkdata::descriptor_for(&names, &lines));
    Value::List(
        lines
            .into_iter()
            .map(|line| {
                Value::DbRow(Box::new(eve_marshal::DbRow {
                    descriptor: std::sync::Arc::clone(&descriptor),
                    values: line.seq().map(<[Value]>::to_vec).unwrap_or_default(),
                }))
            })
            .collect(),
    )
}

fn validate_name(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(name) = call.args.first().and_then(as_text) else {
        warn!("ValidateName: no name argument");
        return Value::Int(0);
    };
    if !name_shape_ok(&name) {
        debug!("ValidateName({name:?}) -> rejected (shape)");
        return Value::Int(0);
    }
    if ctx.store.read(|db| name_taken(db, &name)) {
        debug!("ValidateName({name:?}) -> rejected (taken)");
        Value::Int(0)
    } else {
        debug!("ValidateName({name:?}) -> ok");
        Value::Int(1)
    }
}

/// `ValidateNameEx`: 1 if the name may be taken, else the code the client
/// turns into its message (nextgencharcreation_step_step.py): -1 too short,
/// -2 too long, -5 illegal characters, -6 more than one space, -101 taken.
fn validate_name_ex(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(name) = call.args.first().and_then(as_text) else {
        return Value::Int(-5);
    };
    let code = match name_shape_error(&name) {
        Some("CharNameInvalidMinLength") => -1,
        Some("CharNameInvalidMaxLength") => -2,
        Some("CharNameInvalidMaxSpaces") => -6,
        Some(_) => -5,
        None if ctx.store.read(|db| name_taken(db, &name)) => -101,
        None => 1,
    };
    debug!("ValidateNameEx({name:?}) -> {code}");
    Value::Int(code)
}

/// Whether a character already has this name, letter case aside.
fn name_taken(db: &Db, name: &str) -> bool {
    let name = name.to_lowercase();
    db.table("character_").rows().any(|r| r.text("characterName").is_some_and(|n| n.to_lowercase() == name))
}

pub(crate) fn name_shape_ok(name: &str) -> bool {
    name_shape_error(name).is_none()
}

fn name_shape_error(name: &str) -> Option<&'static str> {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() < 4 {
        return Some("CharNameInvalidMinLength");
    }
    if chars.len() > 24 {
        return Some("CharNameInvalidMaxLength");
    }
    if chars.iter().filter(|c| **c == ' ').count() > 1 {
        return Some("CharNameInvalidMaxSpaces");
    }
    if !chars.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '\'')) {
        return Some("CharNameInvalidSomeChar");
    }
    let (Some(first), Some(last)) = (chars.first(), chars.last()) else {
        return Some("CharNameInvalid");
    };
    if !first.is_ascii_alphanumeric() {
        return Some("CharNameInvalidFirstChar");
    }
    if !last.is_ascii_alphanumeric() {
        return Some("CharNameInvalidLastChar");
    }
    None
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::Bytes(b) | Value::Buffer(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Unicode(s) => Some(s.clone()),
        _ => None,
    }
}

enum Refusal {
    Message(&'static str),
    Custom(String),
}

impl Refusal {
    fn into_outcome(self) -> Outcome {
        match self {
            Refusal::Message(msg) => Outcome::UserError { msg: msg.into(), args: Vec::new() },
            Refusal::Custom(text) => Outcome::custom(text),
        }
    }
}

fn db_failed(what: &str, e: StoreError) -> Refusal {
    error!("CreateCharacter: {what}: {err}", err = crate::logging::chain(&e));
    Refusal::Custom("Character creation failed on the server. Please try again.".into())
}

impl From<StoreError> for Refusal {
    fn from(e: StoreError) -> Self {
        db_failed("store write", e)
    }
}

async fn create_character(call: &Call, ctx: &Ctx<'_>) -> Outcome {
    let Some(account_id) = ctx.session.user_id.and_then(|v| i32::try_from(v).ok()) else {
        return Refusal::Custom("Not logged in.".into()).into_outcome();
    };

    let Some(name) = call.args.first().and_then(as_text) else {
        warn!("CreateCharacter: argument 1 (name) is missing or not a string");
        return Refusal::Message("CharNameInvalid").into_outcome();
    };
    // (name, bloodlineID, genderID, ancestryID, schoolID, departmentID,
    //  fieldID, specialityID, int, cha, per, mem, wil, appearance). Trinity's
    // creator sends None for the four school-path IDs and names its choice
    // as careerID= and careerSpecialityID= instead.
    let required: Option<Vec<i32>> = [1, 2, 3, 8, 9, 10, 11, 12].iter().map(|&i| arg_i32(call, i)).collect();
    let Some(ids) = required else {
        warn!("CreateCharacter({name:?}): bloodline, gender, ancestry and attributes are not all ints");
        return Refusal::Custom("Malformed character creation request.".into()).into_outcome();
    };
    let Some(Value::Dict(appearance)) = call.args.get(13) else {
        warn!("CreateCharacter({name:?}): argument 14 (appearance) is not a dict");
        return Refusal::Custom("Malformed character creation request.".into()).into_outcome();
    };
    let kw = |k: &str| call.kwarg(k).and_then(|v| v.as_i64()).and_then(|v| i32::try_from(v).ok());
    let career = match (kw("careerID"), kw("careerSpecialityID")) {
        (Some(c), Some(s)) => Some(Career { career_id: c, speciality_id: s }),
        (None, None) => None,
        _ => {
            warn!("CreateCharacter({name:?}): a career without a speciality, or the reverse");
            return Refusal::Custom("Choose a career and a speciality.".into()).into_outcome();
        }
    };
    let school_path = (arg_i32(call, 4), arg_i32(call, 5), arg_i32(call, 6), arg_i32(call, 7));
    let path = match (career, school_path) {
        (Some(c), _) => Path::Career(c),
        (None, (Some(school_id), Some(department_id), Some(field_id), Some(speciality_id))) => {
            Path::School { school_id, department_id, field_id, speciality_id }
        }
        _ => {
            warn!("CreateCharacter({name:?}): neither a career nor a school path");
            return Refusal::Custom("Malformed character creation request.".into()).into_outcome();
        }
    };

    let req = CreateRequest {
        name,
        bloodline_id: ids[0],
        gender: ids[1],
        ancestry_id: ids[2],
        path,
        spent: Spent {
            intelligence: ids[3],
            charisma: ids[4],
            perception: ids[5],
            memory: ids[6],
            willpower: ids[7],
        },
        appearance,
        account_id,
    };

    match insert_character(&req, ctx.store).await {
        Ok(char_id) => {
            info!("CreateCharacter({:?}) -> {char_id}", req.name);
            Outcome::Value(Value::Int(char_id as i64))
        }
        Err(refusal) => refusal.into_outcome(),
    }
}

struct CreateRequest<'a> {
    name: String,
    bloodline_id: i32,
    gender: i32,
    ancestry_id: i32,
    path: Path,
    spent: Spent,
    appearance: &'a [(Value, Value)],
    account_id: i32,
}

#[derive(Clone, Copy)]
struct Career {
    career_id: i32,
    speciality_id: i32,
}

/// How the character came by its school, skills and starting kit.
#[derive(Clone, Copy)]
enum Path {
    /// Trinity's creator: a career (which names the school) and a speciality.
    Career(Career),
    /// The 2006 creator: school, department, field and speciality.
    School { school_id: i32, department_id: i32, field_id: i32, speciality_id: i32 },
}

struct Spent {
    intelligence: i32,
    charisma: i32,
    perception: i32,
    memory: i32,
    willpower: i32,
}

impl Spent {
    fn all(&self) -> [i32; 5] {
        [self.intelligence, self.charisma, self.perception, self.memory, self.willpower]
    }
}

const MAX_POINTS_PER_ATTRIBUTE: i32 = 3;
const POINTS_TO_ASSIGN: i32 = 5;
const MAX_ATTRIBUTE_VALUE: i32 = 20;
const MAX_CHARACTERS_PER_ACCOUNT: i64 = 3;

const FLAG_NONE: i32 = 0;
const FLAG_CARGO: i32 = 5;
const FLAG_SKILL: i32 = 7;

const DGM_CHARISMA: i32 = 164;
const DGM_INTELLIGENCE: i32 = 165;
const DGM_MEMORY: i32 = 166;
const DGM_PERCEPTION: i32 = 167;
const DGM_WILLPOWER: i32 = 168;
const DGM_SKILL_TIME_CONSTANT: i32 = 275;
const DGM_SKILL_POINTS: i32 = 276;
const DGM_SKILL_LEVEL: i32 = 280;

/// One appearance choice: `<stem>ID` in the client's dict, checked against
/// the static table that lists what may be picked.
struct Slot {
    stem: &'static str,
    required: bool,
    table: &'static str,
    /// Listed per bloodline and gender (with the NPC-only looks left out),
    /// rather than open to everyone.
    keyed: bool,
}

const fn slot(stem: &'static str, required: bool, table: &'static str) -> Slot {
    Slot { stem, required, table, keyed: true }
}

const SLOTS: &[Slot] = &[
    slot("accessory", false, "chrBLAccessories"),
    slot("beard", false, "chrBLBeards"),
    slot("costume", true, "chrBLCostumes"),
    slot("deco", false, "chrBLDecos"),
    slot("eyebrows", true, "chrBLEyebrows"),
    slot("eyes", true, "chrBLEyes"),
    slot("hair", true, "chrBLHairs"),
    slot("lipstick", false, "chrBLLipsticks"),
    slot("makeup", false, "chrBLMakeups"),
    slot("skin", true, "chrBLSkins"),
    Slot { stem: "background", required: true, keyed: false, table: "chrBLBackgrounds" },
    Slot { stem: "light", required: true, keyed: false, table: "chrBLLights" },
];

impl Slot {
    /// Whether choice `id` is open to a character of this bloodline and gender.
    fn allows(&self, id: i32, bloodline_id: i32, gender: i32) -> bool {
        sde::table(self.table).find(&format!("{}ID", self.stem), id.into()).any(|r| self.fits(&r, bloodline_id, gender))
    }

    /// Whether a row of the slot's table is one this bloodline and gender may pick.
    fn fits(&self, r: &Row, bloodline_id: i32, gender: i32) -> bool {
        !self.keyed
            || (r.int("bloodlineID") == Some(bloodline_id.into())
                && r.int("gender") == Some(gender.into())
                && r.int("npc") == Some(0))
    }
}

const ROTATIONS: &[&str] = &[
    "headRotation1", "headRotation2", "headRotation3",
    "eyeRotation1", "eyeRotation2", "eyeRotation3",
    "camPos1", "camPos2", "camPos3",
];

const MORPHS: &[&str] = &[
    "morph1e", "morph1n", "morph1s", "morph1w",
    "morph2e", "morph2n", "morph2s", "morph2w",
    "morph3e", "morph3n", "morph3s", "morph3w",
    "morph4e", "morph4n", "morph4s", "morph4w",
];

async fn insert_character(req: &CreateRequest<'_>, store: &Store) -> Result<i32, Refusal> {
    let name = &req.name;
    if let Some(msg) = name_shape_error(name) {
        info!("CreateCharacter({name:?}) -> {msg}");
        return Err(Refusal::Message(msg));
    }
    // One write: the three-character limit and the case-insensitive name
    // check hold for everything it makes.
    let (char_id, src) = store.write(|tx| create(tx, req))?;
    info!(
        "CreateCharacter({name:?}): id {char_id}, corp {}, station {}, {} skills, ship {}",
        src.corporation_id,
        src.station_id,
        src.skills.len(),
        src.ship_type_id
    );
    Ok(char_id)
}

/// Everything a new character is made of, in the creation's write: its
/// item, character row, names, attributes, roles, employment, skills, ship
/// and starting kit. Returns its ID and what it was made from.
fn create(tx: &mut Tx<'_>, req: &CreateRequest<'_>) -> Result<(i32, Sources), Refusal> {
    let name = &req.name;
    if tx.table("account").get_id(i64::from(req.account_id)).is_none() {
        return Err(Refusal::Custom("That account no longer exists.".into()));
    }

    if name_taken(tx.db(), name) {
        info!("CreateCharacter({name:?}) -> CharNameInvalidTaken");
        return Err(Refusal::Message("CharNameInvalidTaken"));
    }

    let used = tx.table("character_").find(&["accountID"], &[Cell::Int(i64::from(req.account_id))]).count() as i64;
    if used >= MAX_CHARACTERS_PER_ACCOUNT {
        info!("CreateCharacter({name:?}): account {} already has {used}", req.account_id);
        return Err(Refusal::Custom(format!(
            "This account already has {used} characters, which is the maximum."
        )));
    }

    let src = load_sources(tx.db(), req)?;
    validate_points(&req.spent, &src)?;
    let appearance = read_appearance(req)?;

    let now = crate::bulkdata::filetime_now();
    let char_id = next_id(tx)?;

    insert_entity(
        tx,
        Entity {
            item_id: char_id,
            name,
            type_id: src.type_id,
            owner_id: char_id,
            location_id: src.station_id,
            flag: FLAG_NONE,
            singleton: 1,
            quantity: 1,
        },
    )?;

    let character = insert_character_row(tx, req, &src, &appearance, char_id, now)?;
    // How the character was made, for the character sheet.
    let (career_id, speciality_id) = match req.path {
        Path::Career(c) => (Some(c.career_id), Some(c.speciality_id)),
        Path::School { .. } => (None, None),
    };
    let opt = |v: Option<i32>| v.map_or(Cell::Null, |v| Cell::Int(v.into()));
    tx.update(
        "character_",
        character,
        &[
            ("ancestryID", Cell::Int(req.ancestry_id.into())),
            ("schoolID", Cell::Int(src.school_id.into())),
            ("careerID", opt(career_id)),
            ("careerSpecialityID", opt(speciality_id)),
        ],
    )
    .map_err(|e| db_failed("character_ creation choices", e))?;

    tx.insert(
        "eveStaticOwners",
        &[("ownerID", Cell::Int(char_id.into())), ("ownerName", Cell::Text(name.as_str().into())), ("typeID", Cell::Int(src.type_id.into()))],
    )
    .map_err(|e| db_failed("eveStaticOwners insert", e))?;
    tx.insert(
        "eveNames",
        &[
            ("itemID", Cell::Int(char_id.into())),
            ("itemName", Cell::Text(name.as_str().into())),
            ("categoryID", Cell::Int(1)),
            ("groupID", Cell::Int(1)),
            ("typeID", Cell::Int(src.type_id.into())),
        ],
    )
    .map_err(|e| db_failed("eveNames insert", e))?;

    for (attribute_id, value) in [
        (DGM_INTELLIGENCE, src.base.intelligence + req.spent.intelligence + src.post.intelligence),
        (DGM_CHARISMA, src.base.charisma + req.spent.charisma + src.post.charisma),
        (DGM_PERCEPTION, src.base.perception + req.spent.perception + src.post.perception),
        (DGM_MEMORY, src.base.memory + req.spent.memory + src.post.memory),
        (DGM_WILLPOWER, src.base.willpower + req.spent.willpower + src.post.willpower),
    ] {
        insert_attribute(tx, char_id, attribute_id, value)?;
    }

    tx.insert(
        "chrCorporationRoles",
        &[
            ("characterID", Cell::Int(char_id.into())),
            ("corprole", Cell::Int(0)),
            ("rolesAtAll", Cell::Int(0)),
            ("rolesAtBase", Cell::Int(0)),
            ("rolesAtHQ", Cell::Int(0)),
            ("rolesAtOther", Cell::Int(0)),
        ],
    )
    .map_err(|e| db_failed("chrCorporationRoles insert", e))?;

    tx.insert(
        "chrEmployment",
        &[
            ("characterID", Cell::Int(char_id.into())),
            ("corporationID", Cell::Int(src.corporation_id.into())),
            ("startDate", Cell::Int(now)),
            ("deleted", Cell::Int(0)),
        ],
    )
    .map_err(|e| db_failed("chrEmployment insert", e))?;

    for (type_id, level) in &src.skills {
        let item_id = next_id(tx)?;
        insert_entity(
            tx,
            Entity {
                item_id,
                name: "",
                type_id: *type_id,
                owner_id: char_id,
                location_id: char_id,
                flag: FLAG_SKILL,
                singleton: 1,
                quantity: 1,
            },
        )?;
        insert_attribute(tx, item_id, DGM_SKILL_LEVEL, *level)?;
        insert_attribute(tx, item_id, DGM_SKILL_POINTS, skill_points(*type_id, *level))?;
    }

    let ship_id = next_id(tx)?;
    insert_entity(
        tx,
        Entity {
            item_id: ship_id,
            name: "",
            type_id: src.ship_type_id,
            owner_id: char_id,
            location_id: src.station_id,
            flag: FLAG_HANGAR,
            singleton: 1,
            quantity: 1,
        },
    )?;

    if let (Some(type_id), Some(quantity)) = (src.charge_type_id, src.charge_quantity) {
        let charge_id = next_id(tx)?;
        insert_entity(
            tx,
            Entity {
                item_id: charge_id,
                name: "",
                type_id,
                owner_id: char_id,
                location_id: ship_id,
                flag: FLAG_CARGO,
                singleton: 0,
                quantity,
            },
        )?;
    }

    tx.update("character_", character, &[("shipID", Cell::Int(ship_id.into()))])
        .map_err(|e| db_failed("character_.shipID update", e))?;

    for (type_id, quantity) in &src.equipment {
        let item_id = next_id(tx)?;
        insert_entity(
            tx,
            Entity {
                item_id,
                name: "",
                type_id: *type_id,
                owner_id: char_id,
                location_id: src.station_id,
                flag: FLAG_HANGAR,
                singleton: 0,
                quantity: *quantity,
            },
        )?;
    }
    Ok((char_id, src))
}

fn next_id(tx: &mut Tx<'_>) -> Result<i32, Refusal> {
    let id = tx.nextval("dyn_id_seq").map_err(|e| db_failed("nextval(dyn_id_seq)", e))?;
    // dyn_id_seq stays below 2^31: the client's item IDs are integers.
    Ok(id as i32)
}

struct Entity<'a> {
    item_id: i32,
    name: &'a str,
    type_id: i32,
    owner_id: i32,
    location_id: i32,
    flag: i32,
    singleton: i32,
    quantity: i32,
}

fn insert_entity(tx: &mut Tx<'_>, e: Entity<'_>) -> Result<(), Refusal> {
    tx.insert(
        "entity",
        &[
            ("itemID", Cell::Int(e.item_id.into())),
            ("itemName", Cell::Text(e.name.into())),
            ("typeID", Cell::Int(e.type_id.into())),
            ("ownerID", Cell::Int(e.owner_id.into())),
            ("locationID", Cell::Int(e.location_id.into())),
            ("flag", Cell::Int(e.flag.into())),
            ("contraband", Cell::Int(0)),
            ("singleton", Cell::Int(e.singleton.into())),
            ("quantity", Cell::Int(e.quantity.into())),
            ("x", Cell::Float(0.0)),
            ("y", Cell::Float(0.0)),
            ("z", Cell::Float(0.0)),
            ("customInfo", Cell::Null),
        ],
    )
    .map_err(|err| db_failed("entity insert", err))?;
    Ok(())
}

fn insert_attribute(tx: &mut Tx<'_>, item_id: i32, attribute_id: i32, value: i32) -> Result<(), Refusal> {
    tx.insert(
        "entity_attributes",
        &[
            ("itemID", Cell::Int(item_id.into())),
            ("attributeID", Cell::Int(attribute_id.into())),
            ("valueInt", Cell::Int(value.into())),
            ("valueFloat", Cell::Null),
        ],
    )
    .map_err(|e| db_failed("entity_attributes insert", e))?;
    Ok(())
}

fn skill_points(type_id: i32, level: i32) -> i32 {
    if level <= 0 {
        return 0;
    }
    // The stored integer first, as the skill's rank always was read; the
    // typed lookup (sde::type_attribute) prefers the float.
    let rank = sde::table("dgmTypeAttributes")
        .find("typeID", type_id.into())
        .find(|r| r.int("attributeID") == Some(DGM_SKILL_TIME_CONSTANT.into()))
        .and_then(|r| r.float("valueInt").or_else(|| r.float("valueFloat")));
    let rank = rank.unwrap_or_else(|| {
        warn!("CreateCharacter: skill type {type_id} has no skillTimeConstant, rank 1");
        1.0
    });
    (rank * 250.0 * 2f64.powf(2.5 * (level as f64 - 1.0))).round() as i32
}

#[derive(Default)]
struct Attrs {
    intelligence: i32,
    charisma: i32,
    perception: i32,
    memory: i32,
    willpower: i32,
}

struct Sources {
    type_id: i32,
    race_id: i32,
    ship_type_id: i32,
    school_id: i32,
    corporation_id: i32,
    station_id: i32,
    solar_system_id: i32,
    constellation_id: i32,
    region_id: i32,
    base: Attrs,
    post: Attrs,
    charge_type_id: Option<i32>,
    charge_quantity: Option<i32>,
    skills: Vec<(i32, i32)>,
    equipment: Vec<(i32, i32)>,
}

fn load_sources(db: &Db, req: &CreateRequest<'_>) -> Result<Sources, Refusal> {
    if req.gender != 0 && req.gender != 1 {
        return Err(Refusal::Custom("Unknown gender.".into()));
    }

    // A bloodline of the four playable races, with its character type and
    // its race (whose skill and starting charge it brings).
    let bloodline = sde::table("chrBloodlines")
        .find("bloodlineID", req.bloodline_id.into())
        .find(|b| matches!(b.int("raceID"), Some(1 | 2 | 4 | 8)))
        .and_then(|b| {
            let t = sde::table("bloodlineTypes").get("bloodlineID", req.bloodline_id.into())?;
            let r = sde::table("chrRaces").get("raceID", b.int("raceID")?)?;
            Some((b, t, r))
        });
    let Some((b, t, r)) = bloodline else {
        return Err(Refusal::Custom("Unknown bloodline.".into()));
    };
    let race_id = required(&b, "raceID")?;
    let type_id = required(&t, "typeID")?;
    let ship_type_id = required(&b, "shipTypeID")?;
    let base = Attrs {
        intelligence: required(&b, "intelligence")?,
        charisma: required(&b, "charisma")?,
        perception: required(&b, "perception")?,
        memory: required(&b, "memory")?,
        willpower: required(&b, "willpower")?,
    };
    let mut skills: Vec<i32> = Vec::new();
    push_skill(&mut skills, int(&r, "skillTypeID1"));
    push_skill(&mut skills, int(&b, "skillTypeID1"));
    push_skill(&mut skills, int(&b, "skillTypeID2"));
    let charge_type_id = int(&r, "typeID");
    let charge_quantity = int(&r, "typeQuantity");

    let Some(a) = child("chrAncestries", "ancestryID", req.ancestry_id, "bloodlineID", req.bloodline_id) else {
        return Err(Refusal::Custom("That ancestry does not belong to that bloodline.".into()));
    };
    let post = Attrs {
        intelligence: int(&a, "postIntelligence").unwrap_or(0),
        charisma: int(&a, "postCharisma").unwrap_or(0),
        perception: int(&a, "postPerception").unwrap_or(0),
        memory: int(&a, "postMemory").unwrap_or(0),
        willpower: int(&a, "postWillpower").unwrap_or(0),
    };
    let ancestry_skills = [int(&a, "skillTypeID1"), int(&a, "skillTypeID2")];
    let ancestry_items = [
        (int(&a, "typeID"), int(&a, "typeQuantity")),
        (number(&a, "typeID2"), int(&a, "typeQuantity2")),
    ];

    let (school_id, department_id, field_id, speciality_id) = match req.path {
        Path::School { school_id, department_id, field_id, speciality_id } => {
            (school_id, department_id, field_id, speciality_id)
        }
        Path::Career(career) => {
            let base = Sources {
                type_id,
                race_id,
                ship_type_id,
                school_id: 0,
                corporation_id: 0,
                station_id: 0,
                solar_system_id: 0,
                constellation_id: 0,
                region_id: 0,
                base,
                post,
                charge_type_id,
                charge_quantity,
                skills: Vec::new(),
                equipment: Vec::new(),
            };
            return career_sources(db, base, career, ancestry_items);
        }
    };
    for s in ancestry_skills {
        push_skill(&mut skills, s);
    }

    let (corporation_id, station_id, solar_system_id, constellation_id, region_id) =
        school_station(db, school_id, race_id)?;
    let Some(row) = child("chrDepartments", "departmentID", department_id, "schoolID", school_id) else {
        return Err(Refusal::Custom("That department does not belong to that school.".into()));
    };
    push_skill(&mut skills, int(&row, "skillTypeID1"));
    push_skill(&mut skills, int(&row, "skillTypeID2"));
    push_skill(&mut skills, int(&row, "skillTypeID3"));

    let mut equipment: Vec<(i32, i32)> = Vec::new();
    let Some(row) = child("chrFields", "fieldID", field_id, "departmentID", department_id) else {
        return Err(Refusal::Custom("That field does not belong to that department.".into()));
    };
    push_skill(&mut skills, int(&row, "skillTypeID1"));
    push_skill(&mut skills, int(&row, "skillTypeID2"));
    push_equipment(&mut equipment, int(&row, "typeID"), int(&row, "typeQuantity"));

    let Some(row) = child("chrSpecialities", "specialityID", speciality_id, "fieldID", field_id) else {
        return Err(Refusal::Custom("That speciality does not belong to that field.".into()));
    };
    push_skill(&mut skills, int(&row, "skillTypeID1"));
    push_skill(&mut skills, int(&row, "skillTypeID2"));
    push_equipment(&mut equipment, int(&row, "typeID"), int(&row, "typeQuantity"));

    Ok(Sources {
        type_id,
        race_id,
        ship_type_id,
        school_id,
        corporation_id,
        station_id,
        solar_system_id,
        constellation_id,
        region_id,
        base,
        post,
        charge_type_id,
        charge_quantity,
        skills: group_skills(&skills),
        equipment,
    })
}

/// The school's corporation and its first station in the corporation's home
/// system: where a new character starts.
fn school_station(db: &Db, school_id: i32, race_id: i32) -> Result<(i32, i32, i32, i32, i32), Refusal> {
    let home = child("chrSchools", "schoolID", school_id, "raceID", race_id).and_then(|s| {
        let corporation_id = int(&s, "corporationID")?;
        let corporation = sde::table("crpNPCCorporations").get("corporationID", corporation_id.into())?;
        Some((corporation_id, int(&corporation, "solarSystemID")?))
    });
    // The corporation's station in that system with the lowest ID.
    let row = home.and_then(|(corporation_id, solar_system_id)| {
        db.table("staStations")
            .find(&["solarSystemID"], &[Cell::Int(solar_system_id.into())])
            .filter(|r| r.int("corporationID") == Some(corporation_id.into()))
            .min_by_key(|r| r.int("stationID"))
            .map(|r| {
                let id = |column: &str| r.int(column).map_or(0, |v| v as i32);
                (corporation_id, id("stationID"), id("solarSystemID"), id("constellationID"), id("regionID"))
            })
    });
    row.ok_or_else(|| Refusal::Custom("That school does not belong to that race.".into()))
}

/// Trinity's path: the career names the school; skills are the race's,
/// the career's and the speciality's, their levels added where they meet
/// (as the creator shows them); the kit is the ancestry's two items.
fn career_sources(
    db: &Db,
    mut src: Sources,
    career: Career,
    ancestry_items: [(Option<i32>, Option<i32>); 2],
) -> Result<Sources, Refusal> {
    let school_id = child("chrCareers", "careerID", career.career_id, "raceID", src.race_id)
        .and_then(|r| int(&r, "schoolID"));
    let Some(school_id) = school_id else {
        return Err(Refusal::Custom("That career does not belong to that race.".into()));
    };
    if child("chrCareerSpecialities", "specialityID", career.speciality_id, "careerID", career.career_id).is_none() {
        return Err(Refusal::Custom("That speciality does not belong to that career.".into()));
    }
    let (corporation_id, station_id, solar_system_id, constellation_id, region_id) =
        school_station(db, school_id, src.race_id)?;

    let mut levels: BTreeMap<i32, i64> = BTreeMap::new();
    for (table, column, key) in [
        ("chrRaceSkills", "raceID", src.race_id),
        ("chrCareerSkills", "careerID", career.career_id),
        ("chrCareerSpecialitySkills", "specialityID", career.speciality_id),
    ] {
        for r in sde::table(table).find(column, key.into()) {
            if let (Some(skill), Some(level)) = (int(&r, "skillTypeID"), r.int("levels").filter(|l| *l > 0)) {
                *levels.entry(skill).or_default() += level;
            }
        }
    }
    if levels.is_empty() {
        return Err(Refusal::Custom(
            "The server has no career skill data; load Trinity's static data (tools/trinity_static.py).".into(),
        ));
    }
    src.skills = levels.into_iter().map(|(skill, level)| (skill, level.min(5) as i32)).collect();
    for (type_id, quantity) in ancestry_items {
        push_equipment(&mut src.equipment, type_id, quantity);
    }
    src.corporation_id = corporation_id;
    src.station_id = station_id;
    src.solar_system_id = solar_system_id;
    src.constellation_id = constellation_id;
    src.region_id = region_id;
    src.school_id = school_id;
    Ok(src)
}

/// An `integer` column of a static row.
fn int(r: &Row, column: &str) -> Option<i32> {
    r.int(column).map(|v| v as i32)
}

/// An `integer` column creation cannot do without; NULL is broken data.
fn required(r: &Row, column: &str) -> Result<i32, Refusal> {
    int(r, column).ok_or_else(|| {
        error!("CreateCharacter: {}.{column} is NULL", r.table().name);
        Refusal::Custom("Character creation failed on the server. Please try again.".into())
    })
}

/// The row of `table` keyed `id` (by `key`), if it belongs to `parent_id`
/// (by `parent`): a department of the chosen school, say.
fn child(table: &str, key: &str, id: i32, parent: &str, parent_id: i32) -> Option<Row<'static>> {
    sde::table(table).find(key, id.into()).find(|r| r.int(parent) == Some(parent_id.into()))
}

fn push_skill(into: &mut Vec<i32>, id: Option<i32>) {
    if let Some(id) = id.filter(|v| *v != 0) {
        into.push(id);
    }
}

fn push_equipment(into: &mut Vec<(i32, i32)>, type_id: Option<i32>, quantity: Option<i32>) {
    if let Some(type_id) = type_id.filter(|v| *v != 0) {
        into.push((type_id, quantity.unwrap_or(1).max(1)));
    }
}

fn group_skills(all: &[i32]) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = Vec::new();
    for id in all {
        match out.iter_mut().find(|(t, _)| t == id) {
            Some((_, level)) => *level += 1,
            None => out.push((*id, 1)),
        }
    }
    for (id, level) in out.iter_mut() {
        if *level > 5 {
            warn!("CreateCharacter: skill {id} granted {level} times, clamped to 5");
            *level = 5;
        }
    }
    out
}

fn validate_points(spent: &Spent, src: &Sources) -> Result<(), Refusal> {
    let values = spent.all();
    if values.iter().any(|v| !(0..=MAX_POINTS_PER_ATTRIBUTE).contains(v)) {
        return Err(Refusal::Custom(format!(
            "No attribute may take more than {MAX_POINTS_PER_ATTRIBUTE} points."
        )));
    }
    if values.iter().sum::<i32>() > POINTS_TO_ASSIGN {
        return Err(Refusal::Custom(format!(
            "Only {POINTS_TO_ASSIGN} attribute points may be assigned."
        )));
    }
    let bases = [
        src.base.intelligence,
        src.base.charisma,
        src.base.perception,
        src.base.memory,
        src.base.willpower,
    ];
    if bases.iter().zip(values).any(|(b, v)| b + v > MAX_ATTRIBUTE_VALUE) {
        return Err(Refusal::Custom(format!(
            "No attribute may exceed {MAX_ATTRIBUTE_VALUE}."
        )));
    }
    Ok(())
}

struct Appearance {
    ids: Vec<Option<i32>>,
    rotations: Vec<f64>,
    morphs: Vec<Option<f64>>,
}

fn read_appearance(req: &CreateRequest<'_>) -> Result<Appearance, Refusal> {
    let mut ids = Vec::with_capacity(SLOTS.len());
    for s in SLOTS {
        let key = format!("{}ID", s.stem);
        let value = dict_get(req.appearance, &key);
        let id = match value {
            None | Some(Value::None) => None,
            Some(v) => match v.as_i64().and_then(|v| i32::try_from(v).ok()) {
                Some(id) => Some(id),
                None => {
                    return Err(Refusal::Custom(format!("Appearance value {key} is not an id.")))
                }
            },
        };
        match id {
            None if s.required => {
                return Err(Refusal::Custom(format!("Appearance is missing {key}.")));
            }
            None => {}
            Some(id) => {
                if !s.allows(id, req.bloodline_id, req.gender) {
                    return Err(Refusal::Custom(format!(
                        "Appearance value {key} = {id} is not available to this bloodline."
                    )));
                }
            }
        }
        ids.push(id);
    }

    let mut rotations = Vec::with_capacity(ROTATIONS.len());
    for key in ROTATIONS {
        let Some(v) = dict_get(req.appearance, key).and_then(Value::as_f64) else {
            return Err(Refusal::Custom(format!("Appearance is missing {key}.")));
        };
        rotations.push(v);
    }

    let mut morphs = Vec::with_capacity(MORPHS.len());
    for key in MORPHS {
        morphs.push(dict_get(req.appearance, key).and_then(Value::as_f64));
    }

    Ok(Appearance { ids, rotations, morphs })
}

/// The `character_` row; returns its row.
fn insert_character_row(
    tx: &mut Tx<'_>,
    req: &CreateRequest<'_>,
    src: &Sources,
    a: &Appearance,
    char_id: i32,
    now: i64,
) -> Result<crate::store::RowId, Refusal> {
    let int = |v: i32| Cell::Int(v.into());
    let empty = || Cell::Text("".into());
    let mut sets: Vec<(&str, Cell)> = vec![
        ("characterID", int(char_id)),
        ("characterName", Cell::Text(req.name.as_str().into())),
        ("accountID", int(req.account_id)),
        ("title", empty()),
        ("description", empty()),
        ("bounty", Cell::Float(0.0)),
        ("balance", Cell::Float(crate::config::starting_isk(req.account_id.into()))),
        ("securityRating", Cell::Float(0.0)),
        ("petitionMessage", empty()),
        ("logonMinutes", Cell::Int(0)),
        ("corporationID", int(src.corporation_id)),
        ("corporationDateTime", Cell::Int(now)),
        ("startDateTime", Cell::Int(now)),
        ("createDateTime", Cell::Int(now)),
        ("typeID", int(src.type_id)),
        ("raceID", int(src.race_id)),
        ("bloodlineID", int(req.bloodline_id)),
        ("gender", int(req.gender)),
        ("stationID", int(src.station_id)),
        ("solarSystemID", int(src.solar_system_id)),
        ("constellationID", int(src.constellation_id)),
        ("regionID", int(src.region_id)),
    ];
    let ids = ["accessoryID", "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID", "lipstickID", "makeupID", "skinID", "backgroundID", "lightID"];
    for (column, id) in ids.into_iter().zip(&a.ids) {
        sets.push((column, id.map_or(Cell::Null, int)));
    }
    for (column, v) in ROTATIONS.iter().zip(&a.rotations) {
        sets.push((*column, Cell::Float(*v)));
    }
    for (column, v) in MORPHS.iter().zip(&a.morphs) {
        sets.push((*column, v.map_or(Cell::Null, Cell::Float)));
    }
    tx.insert("character_", &sets).map_err(|e| db_failed("character_ insert", e))
}

fn arg_i32(call: &Call, i: usize) -> Option<i32> {
    call.arg_i64(i).and_then(|v| i32::try_from(v).ok())
}

fn dict_get<'a>(d: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    d.iter()
        .find(|(k, _)| match k {
            Value::Bytes(b) | Value::Buffer(b) => b.as_slice() == key.as_bytes(),
            Value::Unicode(s) => s == key,
            _ => false,
        })
        .map(|(_, v)| v)
}

const APPEARANCE_HEADER: &[&str] = &[
    "gender", "accessoryID", "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID",
    "lipstickID", "makeupID", "skinID", "backgroundID", "lightID", "headRotation1",
    "headRotation2", "headRotation3", "eyeRotation1", "eyeRotation2", "eyeRotation3", "camPos1",
    "camPos2", "camPos3", "morph1e", "morph1n", "morph1s", "morph1w", "morph2e", "morph2n",
    "morph2s", "morph2w", "morph3e", "morph3n", "morph3s", "morph3w", "morph4e", "morph4n",
    "morph4s", "morph4w",
];

fn character_appearance_list(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    if ids.is_empty() {
        return Value::List(Vec::new());
    }
    ctx.store.read(|db| {
        let characters = db.table("character_");
        let out = ids
            .iter()
            .map(|id| match characters.get_id(i64::from(*id)) {
                Some(r) => crate::svc::rows::rowset(APPEARANCE_HEADER, vec![Value::List(r.line(APPEARANCE_HEADER))]),
                None => Value::Dict(Vec::new()),
            })
            .collect();
        Value::List(out)
    })
}
