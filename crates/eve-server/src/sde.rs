//! The game's static data, served from memory.
//!
//! The tables here never change while the server runs, so they are compiled
//! into the binary from `db/static/*.sql.gz` (`tools/trinity_static.py`
//! refreshes them), read once at startup, and looked up without a round trip
//! to the database. PostgreSQL does not hold them: only the tables the game
//! changes as it runs are imported there (`db/*.sql.gz`).
//!
//! Every table is a [`StaticTable`] by its dump name ([`table`]); the few the
//! hot paths need are also kept typed ([`Sde::type_`], dogma's tables).

pub(crate) mod copy;
pub mod market;
mod table;

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use tracing::info;

pub use table::{ColType, Row, StaticTable, Value};

/// Each table compiled in, by name.
macro_rules! dumps {
    ($($name:literal),* $(,)?) => {
        const DUMPS: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../db/static/", $name, ".sql.gz")))),*
        ];
    };
}

dumps![
    "TL2MaterialsForTypeWithActivity", "agtAgentTypes", "agtAgents", "agtConfig", "agtOfferDetails", "billTypes",
    "bloodlineTypes", "cacheLocations", "cacheOwners", "chrAccessories", "chrAncestries", "chrAttributes",
    "chrBLAccessories", "chrBLBackgrounds", "chrBLBeards", "chrBLCostumes", "chrBLDecos", "chrBLEyebrows", "chrBLEyes",
    "chrBLHairs", "chrBLLights", "chrBLLipsticks", "chrBLMakeups", "chrBLSkins", "chrBackgrounds", "chrBeards",
    "chrBloodlines", "chrCareerSkills", "chrCareerSpecialities", "chrCareerSpecialitySkills", "chrCareers", "chrCostumes",
    "chrDecos", "chrDepartments", "chrEyebrows", "chrEyes", "chrFactions", "chrFields", "chrHairs", "chrLights",
    "chrLipsticks", "chrMakeups", "chrRaceSkills", "chrRaces", "chrSchoolAgents", "chrSchools", "chrSkins",
    "chrSpecialities", "crpActivities", "crpNPCCorporationDivisions", "crpNPCCorporationResearchFields",
    "crpNPCCorporationTrades", "crpNPCCorporations", "crpNPCDivisions", "crpNPCTickerNames", "dgmAttributeTypes",
    "dgmAttributes", "dgmEffects", "dgmTypeAttribs", "dgmTypeAttributes", "dgmTypeEffects", "dgmTypeModifiers",
    "eveCategories", "eveConstants", "eveFlags", "eveGraphics", "eveMessages", "eveRoles", "eveStaticLocations",
    "eveStatistics", "eveUnits", "evemailMimeType", "factionRaces", "invBlueprintTypes", "invCategories",
    "invContrabandTypes", "invControlTowerResourcePurposes", "invControlTowerResources", "invFlags", "invGroups",
    "invMarketGroups", "invMetaGroups", "invMetaTypes", "invOreReprocessing", "invShipTypes", "invTypeReactions",
    "invTypes", "languages", "mapCelestialStatistics", "mapConstellationJumps", "mapConstellations", "mapJumps",
    "mapLandmarks", "mapRegionJumps", "mapRegions", "mapSolarSystemJumps", "mapUniverse", "market_keyMap",
    "market_refTypes", "npcStandings", "ramALTypeCategory", "ramALTypeGroup", "ramALTypes", "ramActivities",
    "ramAssemblyLineTypeDetailPerCategory", "ramAssemblyLineTypeDetailPerGroup", "ramAssemblyLineTypes",
    "ramCompletedStatuses", "spawnBounds", "spawnGroupEntries", "spawnGroups", "spawns", "staOperationServices",
    "staOperations", "staServices", "staStationTypes", "tutorial_criteria", "tutorial_page_criteria", "tutorial_pages",
    "tutorials", "tutorials_criterias",
];

/// Whether `name` is one of the tables served from here rather than from
/// PostgreSQL.
pub fn is_static(name: &str) -> bool {
    DUMPS.iter().any(|(n, _)| *n == name)
}

#[derive(Debug, Clone)]
pub struct Type {
    pub group_id: i32,
    pub name: Box<str>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub category_id: i32,
    pub name: Box<str>,
}

#[derive(Debug, Clone)]
pub struct AttributeType {
    pub default_value: Option<f64>,
    pub stackable: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Effect {
    pub name: Box<str>,
    pub category: i16,
}

/// One dgmTypeModifiers row: what a type's effect does to an attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeModifier {
    pub type_id: i32,
    pub effect_name: Box<str>,
    pub effect_category: i16,
    pub func: Box<str>,
    pub domain: Box<str>,
    pub modified_attribute_id: i32,
    pub modifying_attribute_id: i32,
    pub operation: i16,
    pub group_id: Option<i32>,
    pub skill_type_id: Option<i32>,
    pub off_while_active: bool,
}

/// The tables as read, by name.
pub struct Tables(HashMap<&'static str, StaticTable>);

impl Tables {
    /// A table by name. Panics on a name that is not compiled in: the names
    /// are written in the code, so that is a bug for the tests to catch.
    pub fn table(&self, name: &str) -> &StaticTable {
        self.0.get(name).unwrap_or_else(|| panic!("{name} is not a static table"))
    }

    fn table_mut(&mut self, name: &str) -> &mut StaticTable {
        self.0.get_mut(name).unwrap_or_else(|| panic!("{name} is not a static table"))
    }
}

pub struct Sde {
    tables: Tables,
    types: HashMap<i32, Type>,
    groups: HashMap<i32, Group>,
    /// Each group's types and each category's, ascending.
    types_by_group: HashMap<i32, Vec<i32>>,
    types_by_category: HashMap<i32, Vec<i32>>,
    attribute_types: HashMap<i32, AttributeType>,
    effects: HashMap<i32, Effect>,
    /// dgmTypeAttributes, `coalesce(valueFloat, valueInt)`, rows with
    /// neither left out; in dump order within a type.
    type_attributes: HashMap<i32, Vec<(i32, f64)>>,
    type_effects: HashMap<i32, Vec<i32>>,
    modifiers: Vec<TypeModifier>,
    /// Indices into `modifiers`, by type and by effect name, in dump order.
    modifiers_by_type: HashMap<i32, Vec<usize>>,
    modifiers_by_effect: HashMap<Box<str>, Vec<usize>>,
}

static SDE: OnceLock<Sde> = OnceLock::new();

/// The static data. The server reads it at startup ([`init`]); anything
/// that runs without that (a test) reads it on first use, without the
/// startup price fix-ups.
pub fn get() -> &'static Sde {
    SDE.get_or_init(|| {
        let tables = read_tables().unwrap_or_else(|e| panic!("the static data compiled into the server is unreadable: {e}"));
        Sde::index(tables).unwrap_or_else(|e| panic!("the static data compiled into the server is inconsistent: {e}"))
    })
}

/// A static table by name. Panics on a name that is not compiled in: the
/// names are written in the code, so that is a bug for the tests to catch.
pub fn table(name: &str) -> &'static StaticTable {
    get().table(name)
}

/// Read the compiled-in tables, as they are before [`init`] prices what the
/// data leaves unpriced (a fresh world's NPC market is stocked from these).
pub fn read() -> Result<Tables, String> {
    read_tables()
}

/// Make `tables` the static data, pricing what the data leaves unpriced from
/// the NPC market in `store`. Done once, before anything else reads the
/// static data or the server takes clients.
pub fn init(mut tables: Tables, store: &crate::store::Store) -> Result<&'static Sde, String> {
    if let Some(sde) = SDE.get() {
        return Ok(sde);
    }
    let started = Instant::now();
    prices::fill(store, &mut tables);
    let sde = Sde::index(tables)?;
    let sde = SDE.get_or_init(|| sde);
    info!(
        "static data: {} tables, {} rows, read in {:.0?}",
        sde.tables.0.len(),
        sde.tables.0.values().map(StaticTable::len).sum::<usize>(),
        started.elapsed()
    );
    Ok(sde)
}

/// Every compiled-in table, read in parallel.
fn read_tables() -> Result<Tables, String> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(DUMPS.len());
    let chunks: Vec<&[(&'static str, &'static [u8])]> = DUMPS.chunks(DUMPS.len().div_ceil(threads)).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| scope.spawn(move || chunk.iter().map(|(n, gz)| StaticTable::read(n, gz).map(|t| (*n, t))).collect::<Result<Vec<_>, _>>()))
            .collect();
        let mut tables = HashMap::new();
        for h in handles {
            tables.extend(h.join().map_err(|_| "a static table reader panicked".to_string())??);
        }
        Ok(Tables(tables))
    })
}

/// The attributes invTypes keeps as columns (attributeCategory 9).
pub const TYPE_COLUMN_ATTRIBUTES: &[(i32, &str)] = &[(4, "mass"), (38, "capacity"), (161, "volume"), (162, "radius"), (195, "raceID")];

fn key(id: i64) -> Option<i32> {
    i32::try_from(id).ok()
}

fn int32(r: &Row, column: &str) -> Result<i32, String> {
    let v = r.int(column).ok_or_else(|| format!("{}.{column} is NULL", r.table().name))?;
    i32::try_from(v).map_err(|_| format!("{}.{column} {v} does not fit an integer", r.table().name))
}

fn int16(r: &Row, column: &str) -> Result<i16, String> {
    let v = r.int(column).unwrap_or(0);
    i16::try_from(v).map_err(|_| format!("{}.{column} {v} is out of range", r.table().name))
}

impl Sde {
    /// The typed tables and lookups, from the tables read.
    fn index(tables: Tables) -> Result<Sde, String> {
        let t = |name: &str| tables.0.get(name).ok_or_else(|| format!("{name} is not compiled in"));

        let mut types = HashMap::new();
        for r in t("invTypes")?.rows() {
            // invTypes."groupID" may be NULL; joining invGroups on it, as the
            // queries this serves did, drops such a type.
            let Some(group_id) = r.int("groupID") else { continue };
            types.insert(int32(&r, "typeID")?, Type { group_id: group_id as i32, name: r.text("typeName").unwrap_or_default().into() });
        }
        let mut groups = HashMap::new();
        for r in t("invGroups")?.rows() {
            let Some(category_id) = r.int("categoryID") else { continue };
            groups.insert(int32(&r, "groupID")?, Group { category_id: category_id as i32, name: r.text("groupName").unwrap_or_default().into() });
        }
        let mut types_by_group: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut types_by_category: HashMap<i32, Vec<i32>> = HashMap::new();
        for (id, ty) in &types {
            types_by_group.entry(ty.group_id).or_default().push(*id);
            if let Some(g) = groups.get(&ty.group_id) {
                types_by_category.entry(g.category_id).or_default().push(*id);
            }
        }
        types_by_group.values_mut().chain(types_by_category.values_mut()).for_each(|v| v.sort_unstable());

        let mut attribute_types = HashMap::new();
        for r in t("dgmAttributeTypes")?.rows() {
            attribute_types.insert(
                int32(&r, "attributeID")?,
                AttributeType { default_value: r.float("defaultValue"), stackable: r.bool("stackable") },
            );
        }
        let mut effects = HashMap::new();
        for r in t("dgmEffects")?.rows() {
            effects.insert(
                int32(&r, "effectID")?,
                Effect { name: r.text("effectName").unwrap_or_default().into(), category: int16(&r, "effectCategory")? },
            );
        }
        let mut type_attributes: HashMap<i32, Vec<(i32, f64)>> = HashMap::new();
        for r in t("dgmTypeAttributes")?.rows() {
            if let Some(v) = r.float("valueFloat").or_else(|| r.float("valueInt")) {
                type_attributes.entry(int32(&r, "typeID")?).or_default().push((int32(&r, "attributeID")?, v));
            }
        }
        // Mass, capacity, volume, radius and race are attributes too, kept in
        // invTypes' columns rather than dgmTypeAttributes (attributeCategory
        // 9): an item's cargo hold is its capacity attribute, which cargo
        // expanders modify.
        for r in t("invTypes")?.rows() {
            let Some(type_id) = r.int("typeID") else { continue };
            let attrs = type_attributes.entry(type_id as i32).or_default();
            for (attr, column) in TYPE_COLUMN_ATTRIBUTES {
                let value = if *column == "raceID" { r.int(column).map(|v| v as f64) } else { r.float(column) };
                if let Some(v) = value {
                    if !attrs.iter().any(|(a, _)| a == attr) {
                        attrs.push((*attr, v));
                    }
                }
            }
        }
        let mut type_effects: HashMap<i32, Vec<i32>> = HashMap::new();
        for r in t("dgmTypeEffects")?.rows() {
            type_effects.entry(int32(&r, "typeID")?).or_default().push(int32(&r, "effectID")?);
        }
        let mut modifiers = Vec::new();
        for r in t("dgmTypeModifiers")?.rows() {
            let text = |c: &str| r.text(c).map(Into::into).ok_or_else(|| format!("dgmTypeModifiers.{c} is NULL"));
            modifiers.push(TypeModifier {
                type_id: int32(&r, "typeID")?,
                effect_name: text("effectName")?,
                effect_category: int16(&r, "effectCategory")?,
                func: text("func")?,
                domain: text("domain")?,
                modified_attribute_id: int32(&r, "modifiedAttributeID")?,
                modifying_attribute_id: int32(&r, "modifyingAttributeID")?,
                operation: int16(&r, "operation")?,
                group_id: r.int("groupID").map(|v| v as i32),
                skill_type_id: r.int("skillTypeID").map(|v| v as i32),
                off_while_active: r.bool("offWhileActive").unwrap_or(false),
            });
        }
        let mut modifiers_by_type: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut modifiers_by_effect: HashMap<Box<str>, Vec<usize>> = HashMap::new();
        for (i, m) in modifiers.iter().enumerate() {
            modifiers_by_type.entry(m.type_id).or_default().push(i);
            modifiers_by_effect.entry(m.effect_name.clone()).or_default().push(i);
        }

        Ok(Sde {
            tables,
            types,
            groups,
            types_by_group,
            types_by_category,
            attribute_types,
            effects,
            type_attributes,
            type_effects,
            modifiers,
            modifiers_by_type,
            modifiers_by_effect,
        })
    }

    /// A static table by name; see [`table`].
    pub fn table(&self, name: &str) -> &StaticTable {
        self.tables.table(name)
    }

    /// The tables as read, for code written against [`Tables`].
    pub fn tables(&self) -> &Tables {
        &self.tables
    }

    pub fn type_(&self, type_id: i64) -> Option<&Type> {
        self.types.get(&key(type_id)?)
    }

    pub fn group(&self, group_id: i64) -> Option<&Group> {
        self.groups.get(&key(group_id)?)
    }

    /// A type's group and category, if both are known (what joining
    /// invTypes and invGroups on it gave).
    pub fn group_and_category(&self, type_id: i64) -> Option<(i64, i64)> {
        let t = self.type_(type_id)?;
        let g = self.groups.get(&t.group_id)?;
        Some((t.group_id as i64, g.category_id as i64))
    }

    /// The types of a group, ascending: for `"typeID" = ANY($n)` where a
    /// query joined invTypes to filter by group.
    pub fn types_in_group(&self, group_id: i64) -> &[i32] {
        key(group_id).and_then(|k| self.types_by_group.get(&k)).map_or(&[], Vec::as_slice)
    }

    /// The types of a category, ascending (types whose group is unknown are
    /// in none).
    pub fn types_in_category(&self, category_id: i64) -> &[i32] {
        key(category_id).and_then(|k| self.types_by_category.get(&k)).map_or(&[], Vec::as_slice)
    }

    pub fn attribute_types(&self) -> impl Iterator<Item = (i32, &AttributeType)> {
        self.attribute_types.iter().map(|(id, a)| (*id, a))
    }

    pub fn effect(&self, effect_id: i32) -> Option<&Effect> {
        self.effects.get(&effect_id)
    }

    /// A type's own attribute values, before any item's stored values.
    pub fn type_attributes(&self, type_id: i64) -> &[(i32, f64)] {
        key(type_id).and_then(|k| self.type_attributes.get(&k)).map_or(&[], Vec::as_slice)
    }

    /// One attribute of a type, `coalesce(valueFloat, valueInt)`.
    pub fn type_attribute(&self, type_id: i64, attribute_id: i32) -> Option<f64> {
        self.type_attributes(type_id).iter().find(|(a, _)| *a == attribute_id).map(|(_, v)| *v)
    }

    pub fn type_effects(&self, type_id: i64) -> &[i32] {
        key(type_id).and_then(|k| self.type_effects.get(&k)).map_or(&[], Vec::as_slice)
    }

    /// A type's modifiers, in dump order.
    pub fn type_modifiers(&self, type_id: i64) -> impl Iterator<Item = &TypeModifier> {
        let ids = key(type_id).and_then(|k| self.modifiers_by_type.get(&k)).map_or(&[][..], Vec::as_slice);
        ids.iter().map(|i| &self.modifiers[*i])
    }

    /// Every type's modifiers for one effect, in dump order.
    pub fn effect_modifiers(&self, effect_name: &str) -> impl Iterator<Item = &TypeModifier> {
        let ids = self.modifiers_by_effect.get(effect_name).map_or(&[][..], Vec::as_slice);
        ids.iter().map(|i| &self.modifiers[*i])
    }
}

/// Prices the data leaves at nothing, which insurance and clones are priced
/// by: a ship at what the NPC market pays for it, a clone a quarter of an
/// ISK for every skill point it keeps (the Alpha clone stays free). These
/// were once UPDATEs of invTypes made on every start.
mod prices {
    use super::*;

    const CATEGORY_SHIP: i64 = 6;
    const GROUP_CLONE: i64 = 23;
    const TYPE_ALPHA_CLONE: i64 = 164;
    const ATTR_SKILL_POINTS_SAVED: i64 = 419;

    /// The NPC market's best bid for each type it buys.
    pub(super) fn fill(store: &crate::store::Store, tables: &mut Tables) {
        let bids = store.read(|db| {
            let mut bids: HashMap<i64, f64> = HashMap::new();
            for o in db.table("market_orders").rows() {
                if o.int("bid") != Some(1) || o.int("charID").is_none_or(|c| c >= 100_000_000) {
                    continue;
                }
                let (Some(ty), Some(price)) = (o.int("typeID"), o.float("price")) else { continue };
                let best = bids.entry(ty).or_insert(price);
                *best = best.max(price);
            }
            bids
        });
        apply(tables, &bids);
    }

    pub(super) fn apply(tables: &mut Tables, bids: &HashMap<i64, f64>) {
        let ship_groups: std::collections::HashSet<i64> = tables
            .table("invGroups")
            .rows()
            .filter(|g| g.int("categoryID") == Some(CATEGORY_SHIP))
            .filter_map(|g| g.int("groupID"))
            .collect();
        let unpriced = |r: &Row| r.float("basePrice").unwrap_or(0.0) == 0.0;
        let is_ship = |r: &Row| r.int("groupID").is_some_and(|g| ship_groups.contains(&g));
        // Ships are priced only while some marketable ship is still unpriced,
        // as the UPDATE this replaces was.
        let ships_unpriced = tables.table("invTypes").rows().any(|r| is_ship(&r) && unpriced(&r) && r.int("marketGroupID").is_some());
        let clone_points: HashMap<i64, f64> = tables
            .table("dgmTypeAttributes")
            .rows()
            .filter(|a| a.int("attributeID") == Some(ATTR_SKILL_POINTS_SAVED))
            .filter_map(|a| Some((a.int("typeID")?, a.float("valueFloat").or_else(|| a.float("valueInt"))?)))
            .collect();
        tables.table_mut("invTypes").update("basePrice", |r| {
            if !unpriced(&r) {
                return None;
            }
            let id = r.int("typeID")?;
            if ships_unpriced && is_ship(&r) {
                if let Some(bid) = bids.get(&id) {
                    return Some(Value::Float(*bid));
                }
            }
            if r.int("groupID") == Some(GROUP_CLONE) && id != TYPE_ALPHA_CLONE {
                return clone_points.get(&id).map(|p| Value::Float((p * 0.25).round()));
            }
            None
        });
    }
}
