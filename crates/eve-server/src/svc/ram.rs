//! `ramProxy`: the Science & Industry window (manufacturing.py): the assembly
//! lines on offer, installing jobs on them, and delivering what they make.
//!
//! A job runs on one assembly line of a station. It is quoted first
//! (InstallJob with quoteOnly=1) and installed once the pilot accepts the
//! price (quoteOnly=0, authorizedCost). The blueprint leaves the hangar for
//! the job (flagFactoryBlueprint) and comes back when the job is delivered or
//! cancelled; materials are taken at install and lost on cancelling.
//!
//! What a job needs is the blueprint's own rows in
//! TL2MaterialsForTypeWithActivity for the activity: skills (category 16, the
//! quantity being the level), materials used up whole (damagePerJob 1, with
//! waste when manufacturing), and tools, each run wearing out damagePerJob of
//! one.
//!
//! The rules, from the client where it shows them (infosvc.py:1064,
//! manufacturing.py GetSkillLimits) and from the game of the time where not:
//! - waste: wasteFactor / (1 + ME), or wasteFactor x (1 - ME) below 0; and
//!   Production Efficiency's 25% less 5% a level on top;
//! - manufacturing time: productionTime less PE / (1 + PE) of the
//!   productivityModifier, then 4% less a level of Industry;
//! - research: researchProductivityTime (PE) or researchMaterialTime (ME) a
//!   run, 5% less a level of Research or Metallurgy;
//! - copying: researchCopyTime x licensed runs / maxProductionLimit a copy,
//!   5% less a level of Science (my choice; the client says nothing);
//! - slots: 1 + Mass Production + Advanced Mass Production manufacturing jobs,
//!   1 + Laboratory Operation + Advanced Laboratory Operation others;
//! - reach: Supply Chain Management (manufacturing) and Scientific Networking
//!   (the rest): docked there, the system, 5, 10, 20 jumps, the region;
//! - price: the line's install cost and its cost per hour, by started hour,
//!   paid to the line's owner.
//!
//! Jobs for a corporation (forCorp) are installed by its directors or factory
//! managers: the blueprint and materials come from its office's divisions
//! (those the installer may take from), its wallet pays, and what is made
//! (and the blueprint) goes back to the division chosen for output.
//!
//! Starbase assembly arrays and laboratories, anchored and online, carry
//! lines of their own (`STARBASE_LINES`; the database has none for them):
//! jobs there take blueprints and materials from the structure's hold and
//! deliver into it, and are installed from space in the same system.
//!
//! Not served: invention (there are no activity 8 rows), standing discounts,
//! and changing a line's prices.

use eve_marshal::Value;
use tracing::info;

use crate::items::{Item, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::inventory::FLAG_FACTORY_BLUEPRINT;
use crate::svc::membership::{db_failed, message, Refused, Undone};
use crate::svc::rows::{key_val, row, rowset};
use crate::svc::wallet::{self, Entry, Transfer};

const ACTIVITY_MANUFACTURING: i64 = 1;
const ACTIVITY_TIME: i64 = 3;
const ACTIVITY_MATERIAL: i64 = 4;
const ACTIVITY_COPYING: i64 = 5;

const CATEGORY_BLUEPRINT: i64 = 9;
const CATEGORY_SKILL: i64 = 16;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;

const STATUS_DELIVERED: i16 = 1;
const STATUS_ABORTED: i16 = 2;
/// corpRoleFactoryManager.
const CORP_ROLE_FACTORY_MANAGER: i64 = 1_024;

/// The lines each starbase array or laboratory carries: (structure type,
/// [(assembly line type, how many)]).
const STARBASE_LINES: [(i64, &[(i64, i64)]); 16] = [
    (13780, &[(24, 6)]),
    (16220, &[(23, 6)]),
    (24574, &[(17, 1)]),
    (24653, &[(18, 1)]),
    (24654, &[(19, 1)]),
    (24655, &[(20, 1)]),
    (24656, &[(21, 1)]),
    (24657, &[(22, 1)]),
    (24575, &[(10, 1)]),
    (24610, &[(10, 1)]),
    (24658, &[(25, 10)]),
    (24659, &[(26, 6)]),
    (24660, &[(27, 10)]),
    (16216, &[(28, 3), (29, 3), (30, 3)]),
    (24567, &[(13, 5)]),
    (25305, &[(37, 2)]),
];

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

fn float(v: f64) -> store::Value {
    store::Value::Float(v)
}

/// Brings starbase lines into line with the arrays and laboratories online
/// now: lines for those new, none for those gone or offline.
fn sync_starbase_lines(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    let types: Vec<i64> = STARBASE_LINES.iter().map(|(t, _)| *t).collect();
    // (itemID, typeID, ownerID, solar system, region) of each online one.
    let online: Vec<[i64; 5]> = {
        let db = tx.db();
        let (structures, systems) = (db.table("posStructures"), db.table("mapSolarSystems"));
        types
            .iter()
            .flat_map(|t| db.table("entity").find(&["typeID"], &[int(*t)]))
            .filter_map(|e| {
                let id = e.int("itemID")?;
                let state = structures.get_id(id)?.int("state");
                if !matches!(state, Some(4 | 5)) {
                    return None;
                }
                let system = e.int("locationID")?;
                let region = systems.get_id(system)?.int("regionID").unwrap_or(0);
                Some([id, e.int("typeID").unwrap_or(0), e.int("ownerID").unwrap_or(0), system, region])
            })
            .collect()
    };
    let ids: Vec<i64> = online.iter().map(|r| r[0]).collect();
    // Gone or offline: their lines go (jobs running on them are kept).
    let gone: Vec<store::RowId> = tx
        .table("ramAssemblyLineStations")
        .rows()
        .filter(|l| l.int("stationTypeID").is_some_and(|t| types.contains(&t)) && !l.int("stationID").is_some_and(|s| ids.contains(&s)))
        .map(|l| l.id)
        .collect();
    for id in gone {
        tx.delete("ramAssemblyLineStations", id)?;
    }
    for &[id, type_id, owner, system, region] in &online {
        if tx.table("ramAssemblyLineStations").find(&["stationID"], &[int(id)]).next().is_some() {
            continue;
        }
        let Some((_, lines)) = STARBASE_LINES.iter().find(|(t, _)| *t == type_id) else { continue };
        for &(line_type, quantity) in lines.iter() {
            tx.insert(
                "ramAssemblyLineStations",
                &[
                    ("stationID", int(id)),
                    ("assemblyLineTypeID", int(line_type)),
                    ("quantity", int(quantity)),
                    ("stationTypeID", int(type_id)),
                    ("ownerID", int(owner)),
                    ("solarSystemID", int(system)),
                    ("regionID", int(region)),
                ],
            )?;
            let existing = tx
                .table("ramAssemblyLines")
                .rows()
                .filter(|l| l.int("containerID") == Some(id) && l.int("assemblyLineTypeID") == Some(line_type))
                .count() as i64;
            // A line type the data does not know makes no lines.
            let Some(known) = crate::sde::table("ramAssemblyLineTypes").get("assemblyLineTypeID", line_type) else { continue };
            let activity = known.int("activityID").map_or(store::Value::Null, int);
            for _ in existing..quantity {
                let line_id = tx.nextval("dyn_id_seq")?;
                tx.insert(
                    "ramAssemblyLines",
                    &[
                        ("assemblyLineID", int(line_id)),
                        ("assemblyLineTypeID", int(line_type)),
                        ("containerID", int(id)),
                        ("nextFreeTime", int(crate::bulkdata::filetime_now())),
                        ("UIGroupingID", int(0)),
                        ("costInstall", float(0.0)),
                        ("costPerHour", float(0.0)),
                        ("restrictionMask", int(0)),
                        ("discountPerGoodStandingPoint", float(0.0)),
                        ("surchargePerBadStandingPoint", float(0.0)),
                        ("minimumStanding", float(-10.0)),
                        ("minimumCharSecurity", float(-10.0)),
                        ("minimumCorpSecurity", float(-10.0)),
                        ("maximumCharSecurity", float(10.0)),
                        ("maximumCorpSecurity", float(10.0)),
                        ("ownerID", int(owner)),
                        ("activityID", activity.clone()),
                    ],
                )?;
            }
        }
        tracing::info!("starbase structure {id} now carries its assembly lines");
    }
    Ok(())
}

const SKILL_INDUSTRY: i32 = 3380;
const SKILL_MASS_PRODUCTION: i32 = 3387;
const SKILL_PRODUCTION_EFFICIENCY: i32 = 3388;
const SKILL_SCIENCE: i32 = 3402;
const SKILL_RESEARCH: i32 = 3403;
const SKILL_LABORATORY_OPERATION: i32 = 3406;
const SKILL_METALLURGY: i32 = 3409;
const SKILL_SUPPLY_CHAIN_MANAGEMENT: i32 = 24268;
const SKILL_SCIENTIFIC_NETWORKING: i32 = 24270;
const SKILL_ADVANCED_LABORATORY_OPERATION: i32 = 24624;
const SKILL_ADVANCED_MASS_PRODUCTION: i32 = 24625;

/// The journal's refType for a job's fee: Manufacturing (56) to Copying (60).
const REF_JOB_BASE: i32 = 55;
/// const.ramMaxCopyRuns and ramMaxProductionTimeInDays.
const MAX_COPIES: i64 = 20;
const MAX_DAYS: f64 = 30.0;
/// Jumps of reach per level of Supply Chain Management or Scientific
/// Networking; -1 is the station one is docked in (GetSkillLimits).
const REACH: [i64; 6] = [-1, 0, 5, 10, 20, 50];

const SECOND: i64 = 10_000_000;

/// manufacturing.py GetAssemblyLinesScrolllist reads these off each line, and
/// GetRegionDetails appends an activityID column itself.
const LINE_HEADER: &[&str] = &[
    "containerID", "containerTypeID", "containerLocationID", "assemblyLineTypeID", "quantity",
    "ownerID",
];

/// One assembly line, as GetAssemblyLineDetailsScrolllist and
/// ShowAssemblyLineDetails read it.
const ASSEMBLY_LINE_HEADER: &[&str] = &[
    "assemblyLineID", "assemblyLineTypeID", "containerID", "nextFreeTime", "costInstall", "costPerHour",
    "restrictionMask", "discountPerGoodStandingPoint", "surchargePerBadStandingPoint", "minimumStanding",
    "minimumCharSecurity", "minimumCorpSecurity", "maximumCharSecurity", "maximumCorpSecurity", "ownerID",
    "activityID",
];

const JOB_HEADER: &[&str] = &[
    "jobID", "assemblyLineID", "installerID", "creator", "installedItemID", "installedItemTypeID",
    "installedItemOwnerID", "installedItemQuantity", "installedItemProductivityLevel",
    "installedItemMaterialLevel", "installedItemLicensedProductionRunsRemaining", "containerID",
    "containerTypeID", "containerLocationID", "installedInSolarSystemID", "activityID",
    "outputTypeID", "outputFlag", "outputLocationID", "runs", "licensedProductionRuns",
    "installTime", "beginProductionTime", "endProductionTime", "completed", "completedStatus",
    "charAddedCost",
];

/// The quote window's fields (QuotePanel.LoadQuote): times in seconds, the
/// start as a FILETIME, and the bill of materials.
const QUOTE_HEADER: &[&str] = &[
    "maxJobStartTime", "productionTime", "cost", "installCost", "usageCost", "materialMultiplier",
    "charMaterialMultiplier", "timeMultiplier", "charTimeMultiplier", "bom",
];

pub async fn ram_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "AssemblyLinesSelect" => Ok(assembly_lines(ctx, call.arg_text(0).as_deref())),
        "AssemblyLinesGet" => Ok(lines_at(ctx, call.arg_id(0))),
        "GetJobs2" => Ok(jobs(ctx, call)),
        "InstallJob" => install(ctx, call).await,
        "CompleteJob" => complete(ctx, call.arg_id(1), call.arg_i64(2).unwrap_or(0) != 0).await,
        "UpdateAssemblyLineConfigurations" => Err(message("RamConfigAssemblyLinesAccessDenied")),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// AssemblyLinesSelect(typeFlag): 'region' is every line in the caller's region;
/// 'char', 'corp' and 'alliance' are the lines that owner holds
/// (manufacturing.py:938-942).
fn assembly_lines(ctx: &Ctx<'_>, type_flag: Option<&str>) -> Value {
    if let Err(e) = ctx.store.write(sync_starbase_lines) {
        tracing::error!("starbase assembly lines: {}", crate::logging::chain(&e));
    }
    let s = ctx.session;
    let (column, id) = match type_flag {
        Some("char") => ("ownerID", s.char_id),
        Some("corp") => ("ownerID", s.corp_id),
        Some("alliance") => ("ownerID", s.alliance_id),
        _ => ("regionID", s.region_id),
    };
    let Some(id) = id else { return rowset(LINE_HEADER, Vec::new()) };
    const COLUMNS: &[&str] = &["stationID", "stationTypeID", "solarSystemID", "assemblyLineTypeID", "quantity", "ownerID"];
    ctx.store.read(|db| {
        let mut found: Vec<Row> = db.table("ramAssemblyLineStations").rows().filter(|l| l.int(column) == Some(id)).collect();
        found.sort_by_key(|l| (l.int("stationID"), l.int("assemblyLineTypeID")));
        rowset(LINE_HEADER, found.iter().map(|l| Value::List(l.line(COLUMNS))).collect())
    })
}

/// AssemblyLinesGet(containerID): the station's lines, each with when it is
/// next free.
fn lines_at(ctx: &Ctx<'_>, container: i64) -> Value {
    const COLUMNS: &[&str] = &[
        "assemblyLineID", "assemblyLineTypeID", "containerID", "nextFreeTime", "costInstall", "costPerHour", "restrictionMask",
        "discountPerGoodStandingPoint", "surchargePerBadStandingPoint", "minimumStanding", "minimumCharSecurity",
        "minimumCorpSecurity", "maximumCharSecurity", "maximumCorpSecurity", "ownerID",
    ];
    // Each line with its type's activity; a line of a type the data does not
    // know is not offered.
    let types = crate::sde::table("ramAssemblyLineTypes");
    let lines = ctx.store.read(|db| {
        let mut found: Vec<Row> = db.table("ramAssemblyLines").rows().filter(|l| l.int("containerID") == Some(container)).collect();
        found.sort_by_key(|l| l.int("assemblyLineID"));
        found
            .iter()
            .filter_map(|r| {
                let t = types.get("assemblyLineTypeID", r.int("assemblyLineTypeID")?)?;
                let mut line = r.line(COLUMNS);
                line.push(t.cell("activityID"));
                Some(Value::List(line))
            })
            .collect()
    });
    rowset(ASSEMBLY_LINE_HEADER, lines)
}

/// GetJobs2(ownerID, completed, fromDate, toDate): the jobs of the caller or its
/// corporation, delivered or not; the dates (FILETIME, or None) bound installTime.
fn jobs(ctx: &Ctx<'_>, call: &Call) -> Value {
    let owner = call.arg_i64(0);
    let s = ctx.session;
    if owner.is_none() || (owner != s.char_id && owner != s.corp_id) {
        return rowset(JOB_HEADER, Vec::new());
    }
    let completed = i64::from(call.arg_i64(1).unwrap_or(0) != 0);
    let from = call.arg_i64(2);
    let to = call.arg_i64(3);
    ctx.store.read(|db| {
        let mut found: Vec<Row> = db
            .table("ramJobs")
            .find(&["installedItemOwnerID", "completed"], &[int(owner.unwrap_or(0)), int(completed)])
            .filter(|j| {
                let installed = j.int("installTime").unwrap_or(0);
                from.is_none_or(|f| installed >= f) && to.is_none_or(|t| installed <= t)
            })
            .collect();
        found.sort_by_key(|j| std::cmp::Reverse(j.int("installTime")));
        rowset(JOB_HEADER, found.iter().map(|j| Value::List(j.line(JOB_HEADER))).collect())
    })
}

// ---- installing --------------------------------------------------------------------------

struct Line {
    id: i64,
    type_id: i64,
    station: i64,
    station_type: i64,
    system: i64,
    region: i64,
    owner: i64,
    activity: i64,
    time_multiplier: f64,
    material_multiplier: f64,
    cost_install: f64,
    cost_per_hour: f64,
    next_free: i64,
    min_security: f64,
    max_security: f64,
    /// A starbase array or laboratory, not a station.
    structure: bool,
}

fn line(db: &Db, id: i64) -> Refused<Line> {
    let l = db.table("ramAssemblyLines").get_id(id).ok_or_else(|| message("RamPleasePickAnInstalltion"))?;
    // A line of a type the data does not know is no line at all.
    let t = l
        .int("assemblyLineTypeID")
        .and_then(|t| crate::sde::table("ramAssemblyLineTypes").get("assemblyLineTypeID", t))
        .ok_or_else(|| message("RamPleasePickAnInstalltion"))?;
    // The station it is in, or else (a starbase structure's) its first
    // station row.
    let container = l.int("containerID");
    let s = container.and_then(|c| db.table("staStations").get_id(c));
    let ls = container.and_then(|c| db.table("ramAssemblyLineStations").find(&["stationID"], &[int(c)]).next());
    let place = |column: &str| s.and_then(|s| s.int(column)).or_else(|| ls.and_then(|ls| ls.int(column))).unwrap_or(0);
    let f = |column: &str, default: f64| l.float(column).unwrap_or(default);
    Ok(Line {
        id: l.int("assemblyLineID").unwrap_or(0),
        type_id: l.int("assemblyLineTypeID").unwrap_or(0),
        station: container.unwrap_or(0),
        station_type: place("stationTypeID"),
        system: place("solarSystemID"),
        region: place("regionID"),
        owner: l.int("ownerID").unwrap_or(0),
        activity: t.int("activityID").unwrap_or(0),
        time_multiplier: t.float("baseTimeMultiplier").unwrap_or(0.0),
        material_multiplier: t.float("baseMaterialMultiplier").unwrap_or(0.0),
        cost_install: f("costInstall", 0.0),
        cost_per_hour: f("costPerHour", 0.0),
        next_free: l.int("nextFreeTime").unwrap_or(0),
        min_security: f("minimumCharSecurity", -10.0),
        max_security: f("maximumCharSecurity", 10.0),
        structure: s.is_none(),
    })
}

/// A blueprint item with its type's figures and its own research.
struct Blueprint {
    item: Item,
    type_name: String,
    product: i64,
    production_time: f64,
    research_productivity_time: f64,
    research_material_time: f64,
    research_copy_time: f64,
    productivity_modifier: f64,
    waste_factor: f64,
    max_production_limit: i64,
    copy: bool,
    material_level: i64,
    productivity_level: i64,
    runs_left: i64,
}

fn blueprint(txn: &mut ItemTxn<'_, '_>, item_id: i64) -> Refused<Blueprint> {
    let item = txn
        .item(item_id)
        .map_err(|e| db_failed("reading the blueprint", e))?
        .ok_or_else(|| message("RamPleasePickAnItemToInstall"))?;
    if item.category_id != CATEGORY_BLUEPRINT {
        return Err(message("RamActivityRequiresABlueprint"));
    }
    // The type's figures, for a blueprint type the data knows (and has as a type).
    let sde = crate::sde::get();
    let type_name = sde.table("invTypes").get("typeID", item.type_id).map(|t| t.text("typeName").unwrap_or_default().to_string());
    let (Some(b), Some(type_name)) = (sde.table("invBlueprintTypes").get("blueprintTypeID", item.type_id), type_name) else {
        return Err(message("RamActivityRequiresABlueprint"));
    };
    let r = txn.db().table("invBlueprints").get_id(item_id);
    // An item without research of its own is an original at level 0 with unlimited runs.
    let own = |column: &str, default: i64| r.as_ref().map_or(default, |r| r.int(column).unwrap_or(default));
    let i = |c| b.int(c).unwrap_or(0);
    let f = |c| b.float(c).unwrap_or(0.0);
    Ok(Blueprint {
        type_name,
        product: i("productTypeID"),
        production_time: f("productionTime"),
        research_productivity_time: f("researchProductivityTime"),
        research_material_time: f("researchMaterialTime"),
        research_copy_time: f("researchCopyTime"),
        productivity_modifier: f("productivityModifier"),
        waste_factor: f("wasteFactor"),
        max_production_limit: i("maxProductionLimit").max(1),
        copy: own("copy", 0) != 0,
        material_level: own("materialLevel", 0),
        productivity_level: own("productivityLevel", 0),
        runs_left: own("licensedProductionRunsRemaining", -1),
        item,
    })
}

/// One row of what a job needs.
#[derive(Debug, Clone, PartialEq)]
struct Need {
    type_id: i64,
    quantity: i64,
    damage: f64,
    skill: bool,
}

/// The blueprint's rows for the activity, by required type; a required type
/// the data does not know is left out.
fn needs(blueprint_type: i64, activity: i64) -> Vec<Need> {
    let sde = crate::sde::get();
    let mut needs: Vec<(i64, Need)> = sde
        .table("TL2MaterialsForTypeWithActivity")
        .find("typeID", blueprint_type as i32 as i64)
        .filter(|m| m.int("activity") == Some(activity as i32 as i64) && m.int("quantity").is_some_and(|q| q > 0))
        .filter_map(|m| {
            let required = m.int("requiredTypeID")?;
            let (_, category) = sde.group_and_category(required)?;
            let need = Need {
                type_id: required,
                quantity: m.int("quantity").unwrap_or(0),
                damage: m.float("damagePerJob").unwrap_or(1.0),
                skill: category == CATEGORY_SKILL,
            };
            Some((required, need))
        })
        .collect();
    needs.sort_by_key(|(required, _)| *required);
    needs.into_iter().map(|(_, n)| n).collect()
}

/// What a job will take, how long and what it costs.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    seconds: f64,
    begin: i64,
    end: i64,
    install_cost: f64,
    usage_cost: f64,
    material_multiplier: f64,
    char_material_multiplier: f64,
    time_multiplier: f64,
    char_time_multiplier: f64,
    /// (type, quantity) without waste, and the waste on top.
    raw: Vec<(i64, i64)>,
    waste: Vec<(i64, i64)>,
    tools: Vec<Need>,
    skills: Vec<(i64, i64)>,
    output_type: i64,
}

impl Plan {
    fn cost(&self) -> f64 {
        self.install_cost + self.usage_cost
    }

    /// Everything used up whole: the materials and their waste, summed.
    fn consumed(&self) -> Vec<(i64, i64)> {
        let mut total: Vec<(i64, i64)> = self.raw.clone();
        for (t, q) in &self.waste {
            match total.iter_mut().find(|(rt, _)| rt == t) {
                Some(e) => e.1 += q,
                None => total.push((*t, *q)),
            }
        }
        total
    }
}

/// The waste a blueprint's material level leaves (0.1 is 10%).
fn waste(waste_factor: f64, material_level: i64) -> f64 {
    let base = waste_factor / 100.0;
    if material_level >= 0 { base / (1.0 + material_level as f64) } else { base * (1.0 - material_level as f64) }
}

/// One manufacturing run's time at a productivity level (infosvc.py:1067).
fn run_time(production_time: f64, productivity_modifier: f64, productivity_level: i64) -> f64 {
    let pl = productivity_level as f64;
    if productivity_level >= 0 {
        production_time - pl / (1.0 + pl) * productivity_modifier
    } else {
        production_time - (pl - 1.0) * productivity_modifier
    }
}

/// The multipliers a line puts on what it makes: per group, else per
/// category; None if the line cannot make it.
fn line_detail(line_type: i64, subject: i64) -> Option<(f64, f64)> {
    let sde = crate::sde::get();
    let (group, category) = sde.group_and_category(subject as i32 as i64)?;
    let line_type = line_type as i32 as i64;
    // The group's row wins whenever there is one, even with a multiplier
    // missing (which makes it no use, as before).
    let detail = sde
        .table("ramAssemblyLineTypeDetailPerGroup")
        .find("assemblyLineTypeID", line_type)
        .find(|d| d.int("groupID") == Some(group))
        .or_else(|| sde.table("ramAssemblyLineTypeDetailPerCategory").find("assemblyLineTypeID", line_type).find(|d| d.int("categoryID") == Some(category)))?;
    Some((detail.float("timeMultiplier")?, detail.float("materialMultiplier")?))
}

/// InstallJob(installationLocationData, installedItemLocationData,
/// bomLocationData, flagOutput, runs, activityID, licensedProductionRuns,
/// forCorp, 'blah', quoteOnly=, authorizedCost=, installedItem=,
/// maxJobStartTime=) -> (quote, the blueprint's itemID if it was split off a
/// stack). The locations are [invLocation, path, ids] (manufacturing.py:1538).
async fn install(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let path_ids = |i: usize| call.args.get(i).and_then(|v| v.seq()).and_then(|p| p.get(2)).and_then(|v| v.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let line_id = path_ids(0).get(1).and_then(|v| v.as_i64()).ok_or_else(|| message("RamPleasePickAnInstalltion"))?;
    let item_id = path_ids(1).first().and_then(|v| v.as_i64()).ok_or_else(|| message("RamPleasePickAnItemToInstall"))?;
    // bomLocationData: [invLocation, [[locationID, ownerID, flag]], []], or [[flagNone]] for none.
    let bom_flag = call
        .args
        .get(2)
        .and_then(|v| v.seq())
        .and_then(|b| b.get(1))
        .and_then(|v| v.seq())
        .and_then(|p| p.first())
        .and_then(|v| v.seq())
        .and_then(|step| if step.len() >= 3 { step.get(2) } else { None })
        .and_then(|v| v.as_i64());
    let output_flag = call.arg_i64(3).filter(|f| *f != 0).unwrap_or(FLAG_HANGAR);
    let runs = call.arg_i64(4).unwrap_or(1).max(1);
    let activity = call.arg_i64(5).unwrap_or(0);
    let licensed_runs = call.arg_i64(6).unwrap_or(0);
    let for_corp = call.args.get(7).is_some_and(|v| v.as_i64().unwrap_or(0) != 0 || *v == Value::Bool(true));
    let quote_only = call.kwarg("quoteOnly").and_then(|v| v.as_i64()).unwrap_or(1) != 0;
    let authorized = call.kwarg("authorizedCost").and_then(|v| v.as_f64());
    let latest_start = call.kwarg("maxJobStartTime").and_then(|v| v.as_i64());
    // A corporation job: its wallet and its hangars.
    let roles = ctx.session.corp_role();
    let who = if for_corp {
        let Some(corp) = ctx.session.corp_id else { return Err(Outcome::custom("You are not in a corporation.")) };
        if roles & (crate::session::CORP_ROLE_DIRECTOR | CORP_ROLE_FACTORY_MANAGER) == 0 {
            return Err(Outcome::custom("Installing jobs for your corporation needs the Factory Manager role."));
        }
        corp
    } else {
        me
    };
    if ctx.session.ship_id.is_none() {
        return Err(message("RamNoShip"));
    }

    let (reply, installed) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            // The installer's job slots span every assembly line; the store runs one
            // writer at a time, so the count below holds until this commits.
            if !crate::svc::login::lock_character_lifecycle(txn.db(), me).map_err(|e| db_failed("locking the installer", e))? {
                return Err(Outcome::custom("That character no longer exists.").into());
            }
            let line = line(txn.db(), line_id)?;
            if line.activity != activity || ![ACTIVITY_MANUFACTURING, ACTIVITY_TIME, ACTIVITY_MATERIAL, ACTIVITY_COPYING].contains(&activity) {
                return Err(message("RamActivityInvalid").into());
            }
            within_reach(ctx, txn.db(), &line)?;
            let security = security_of(txn.db(), me);
            if security < line.min_security {
                return Err(message("RamAccessDeniedSecStatusTooLow").into());
            }
            if security > line.max_security {
                return Err(message("RamAccessDeniedSecStatusTooHigh").into());
            }

            let mut bp = blueprint(&mut txn, item_id)?;
            if bp.item.flag == i64::from(FLAG_FACTORY_BLUEPRINT)
                || txn.db().table("ramJobs").rows().any(|j| j.int("installedItemID") == Some(item_id) && j.int("completed") == Some(0))
            {
                return Err(Outcome::custom("That blueprint is already installed in an industry job.").into());
            }
            if bp.item.owner_id != who {
                return Err(message("RamCannotInstallItemForAnother").into());
            }
            // The corporation's office at the station, for its materials and output.
            let office = if for_corp {
                txn.db()
                    .table("crpOffices")
                    .find(&["corporationID"], &[int(who)])
                    .find(|o| o.int("stationID") == Some(line.station))
                    .and_then(|o| o.int("itemID"))
            } else {
                None
            };
            let in_office = office.is_some_and(|o| bp.item.location_id == o && crate::svc::corphangar::may_take_at(ctx, bp.item.flag, Some(line.station)));
            if !in_office && !at_station(ctx, &mut txn, &bp.item, line.station)? {
                return Err(message_with("RamInstalledItemWrongLocation", &[("location", Value::text(station_name(txn.db(), line.station)))]).into());
            }
            match activity {
                ACTIVITY_COPYING if bp.copy => return Err(message("RamCannotCopyABlueprintCopy").into()),
                ACTIVITY_TIME | ACTIVITY_MATERIAL if bp.copy => return Err(message("RamCannotResearchABlueprintCopy").into()),
                ACTIVITY_MANUFACTURING if bp.copy && runs > bp.runs_left => return Err(message("RamTooManyProductionRuns").into()),
                ACTIVITY_COPYING if runs > MAX_COPIES => return Err(Outcome::custom(format!("At most {MAX_COPIES} copies can be made in one job.")).into()),
                _ => {}
            }
            if activity == ACTIVITY_MANUFACTURING && bp.product == 0 {
                return Err(message("RamNoKnownOutputType").into());
            }
            let subject = if activity == ACTIVITY_MANUFACTURING { bp.product } else { bp.item.type_id };
            let (detail_time, detail_material) =
                line_detail(line.type_id, subject).ok_or_else(|| message("RamBadEndProductForActivity"))?;

            // A blueprint from a stack is installed alone.
            let mut split_off = Value::None;
            if bp.item.quantity > 1 {
                txn.set_quantity(bp.item.id, bp.item.quantity - 1).map_err(|e| db_failed("splitting the stack", e))?;
                let one = NewItem {
                    type_id: bp.item.type_id,
                    owner_id: bp.item.owner_id,
                    location_id: bp.item.location_id,
                    flag: bp.item.flag,
                    quantity: 1,
                    singleton: true,
                };
                let id = txn.create(one).map_err(|e| db_failed("splitting the stack", e))?;
                split_off = Value::Int(id);
                bp = blueprint(&mut txn, id)?;
            }

            let need = needs(bp.item.type_id, activity);
            let skill_types: Vec<i32> = need
                .iter()
                .filter(|n| n.skill)
                .map(|n| n.type_id as i32)
                .chain([
                    SKILL_INDUSTRY, SKILL_MASS_PRODUCTION, SKILL_PRODUCTION_EFFICIENCY, SKILL_SCIENCE, SKILL_RESEARCH,
                    SKILL_LABORATORY_OPERATION, SKILL_METALLURGY, SKILL_ADVANCED_LABORATORY_OPERATION, SKILL_ADVANCED_MASS_PRODUCTION,
                ])
                .collect();
            let levels = crate::svc::skills::levels(txn.db(), me, &skill_types).map_err(|e| db_failed("reading skills", e))?;
            let level = |s: i32| levels.get(&s).copied().unwrap_or(0);
            for n in need.iter().filter(|n| n.skill) {
                if level(n.type_id as i32) < n.quantity {
                    let name = type_name(n.type_id);
                    return Err(message_with("RamNeedSkillForJob", &[("skillLevel", Value::Int(n.quantity)), ("item", Value::text(name))]).into());
                }
            }
            slots_free(txn.db(), me, activity, &level)?;

            let now = crate::bulkdata::filetime_now();
            let plan = plan(&line, &bp, &need, activity, runs, licensed_runs, (detail_time, detail_material), &level, now);
            if plan.seconds > MAX_DAYS * 86_400.0 {
                return Err(message_with("RamProductionTimeExceedsLimits", &[("productionTime", Value::Double(plan.seconds / 3600.0)), ("limit", Value::Double(MAX_DAYS * 24.0))]).into());
            }

            if quote_only {
                // The split-off blueprint is kept: the install refers to it.
                return Ok((Value::Tuple(vec![quote(&plan), split_off]), None));
            }
            if latest_start.is_some_and(|t| plan.begin > t) {
                return Err(message("RamCannotGuaranteeStartTime").into());
            }
            if authorized.is_some_and(|a| plan.cost() > a + 0.01) {
                return Err(Outcome::custom("The price has gone up since your quote. Please ask for a new one.").into());
            }

            // The materials, from the hangar or the ship's hold at the station; a
            // corporation's office division; or a starbase structure's hold.
            let source = match bom_flag {
                _ if line.structure => Some((line.station, bom_flag.unwrap_or(0))),
                Some(f) if office.is_some() && crate::svc::corphangar::division(f).is_some() => {
                    if !crate::svc::corphangar::may_take_at(ctx, f, Some(line.station)) {
                        return Err(Outcome::custom("You may not take materials from that division.").into());
                    }
                    office.map(|o| (o, f))
                }
                Some(FLAG_CARGO) if ctx.session.station_id == Some(line.station) => ctx.session.ship_id.map(|s| (s, FLAG_CARGO)),
                Some(FLAG_HANGAR) => Some((line.station, FLAG_HANGAR)),
                _ => None,
            };
            let consumed = plan.consumed();
            if !consumed.is_empty() || !plan.tools.is_empty() {
                let Some((location, flag)) = source else { return Err(Outcome::custom("Please select an input location for the job.").into()) };
                let mut rng = crate::combat::Rng::seeded(now as u64 ^ (bp.item.id as u64) << 20);
                let mut take: Vec<(i64, i64, i64)> = consumed.iter().map(|(t, q)| (*t, *q, *q)).collect();
                for tool in &plan.tools {
                    let worn = tool.damage * runs as f64;
                    let used = worn.floor() as i64 + i64::from(rng.unit() < worn.fract());
                    take.push((tool.type_id, tool.quantity.max(used), used));
                }
                for (t, have_at_least, used) in take {
                    use_up(&mut txn, who, location, flag, t, have_at_least, used)?;
                }
            }

            // The fee, from the pilot (or the corporation) to the line's owner.
            let reason = format!("{} job, {} run(s)", activity_name(activity), runs);
            let entry = Entry { ref_type: REF_JOB_BASE + activity as i32, payer: who, payee: line.owner, arg_id1: "", reason: &reason };
            let balance = match wallet::post(txn.tx(), who, -plan.cost(), &entry) {
                Ok(b) => b,
                Err(Transfer::NotEnough { need, have }) => return Err(Outcome::not_enough_money(need, have).into()),
                Err(Transfer::NoSuchOwner) => return Err(Outcome::custom("Your wallet could not be found.").into()),
                Err(Transfer::Db(e)) => return Err(db_failed("paying for the job", e).into()),
            };
            if plan.cost() > 0.0 {
                if let Err(Transfer::Db(e)) = wallet::post(txn.tx(), line.owner, plan.cost(), &entry) {
                    return Err(db_failed("paying the line's owner", e).into());
                }
            }

            // Where the output (and the blueprint) goes: the structure's hold, the
            // corporation's division, or the pilot's hangar.
            let (output_location, output_flag) = match office {
                _ if line.structure => (line.station, 0),
                Some(o) if crate::svc::corphangar::division(output_flag).is_some() => (o, output_flag),
                Some(o) => (o, FLAG_HANGAR),
                None => (line.station, output_flag),
            };
            txn.set_singleton(bp.item.id, true).map_err(|e| db_failed("installing the blueprint", e))?;
            txn.move_to(bp.item.id, line.station, i64::from(FLAG_FACTORY_BLUEPRINT)).map_err(|e| db_failed("installing the blueprint", e))?;
            let tx = txn.tx();
            // ramJobs."jobID" is an identity column: its next value, as the column's default gave it.
            let job_id = tx.nextval("ramJobs_jobID_seq")?;
            let job = [
                ("jobID", int(job_id)),
                ("assemblyLineID", int(line.id)),
                ("installerID", int(me)),
                ("creator", int(me)),
                ("installedItemID", int(bp.item.id)),
                ("installedItemTypeID", int(bp.item.type_id)),
                ("installedItemOwnerID", int(who)),
                ("installedItemQuantity", int(1)),
                ("installedItemProductivityLevel", int(bp.productivity_level)),
                ("installedItemMaterialLevel", int(bp.material_level)),
                ("installedItemLicensedProductionRunsRemaining", int(bp.runs_left)),
                ("containerID", int(line.station)),
                ("containerTypeID", int(line.station_type)),
                ("containerLocationID", int(line.system)),
                ("installedInSolarSystemID", int(line.system)),
                ("activityID", int(activity)),
                ("outputTypeID", int(plan.output_type)),
                ("outputFlag", int(output_flag)),
                ("outputLocationID", int(output_location)),
                ("runs", int(runs)),
                ("licensedProductionRuns", int(licensed_runs)),
                ("installTime", int(now)),
                ("beginProductionTime", int(plan.begin)),
                ("endProductionTime", int(plan.end)),
            ];
            tx.insert("ramJobs", &job).map_err(|e| db_failed("recording the job", e))?;
            if let Some(row) = tx.table("ramAssemblyLines").get_id(line.id).map(|l| l.id) {
                tx.update("ramAssemblyLines", row, &[("nextFreeTime", int(plan.end))]).map_err(|e| db_failed("booking the assembly line", e))?;
            }
            let done = format!(
                "{me} installed job {job_id}: {} x{runs} of {} on line {} at {}, {:.0} s, {:.2} ISK",
                activity_name(activity), bp.type_name, line.id, line.station, plan.seconds, plan.cost()
            );
            Ok((Value::Tuple(vec![quote(&plan), split_off]), Some((txn.events(), balance, done))))
        })
        .map_err(|Undone(o)| o)?;
    let Some((events, balance, done)) = installed else { return Ok(reply) };

    crate::items::notify(ctx.world, ctx.client_id, events);
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(who), Value::Double(balance)]);
    w.notify(&w.clients_of(me), "OnAccountChange", cash);
    info!("{done}");
    Ok(reply)
}

/// The time, the price and the bill of materials of a job.
#[allow(clippy::too_many_arguments)]
fn plan(
    line: &Line,
    bp: &Blueprint,
    need: &[Need],
    activity: i64,
    runs: i64,
    licensed_runs: i64,
    (detail_time, detail_material): (f64, f64),
    level: &(dyn Fn(i32) -> i64 + Sync),
    now: i64,
) -> Plan {
    let line_time = line.time_multiplier * detail_time;
    let line_material = line.material_multiplier * detail_material;
    let less = |skill: i32, per_level: f64| 1.0 - per_level * level(skill) as f64;
    let (base_seconds, time_multiplier, char_time_multiplier) = match activity {
        ACTIVITY_MANUFACTURING => {
            let run = run_time(bp.production_time, bp.productivity_modifier, bp.productivity_level);
            (run, line_time * run / bp.production_time.max(1.0), less(SKILL_INDUSTRY, 0.04))
        }
        ACTIVITY_TIME => (bp.research_productivity_time, line_time, less(SKILL_RESEARCH, 0.05)),
        ACTIVITY_MATERIAL => (bp.research_material_time, line_time, less(SKILL_METALLURGY, 0.05)),
        _ => {
            let share = licensed_runs.clamp(1, bp.max_production_limit) as f64 / bp.max_production_limit as f64;
            (bp.research_copy_time * share, line_time, less(SKILL_SCIENCE, 0.05))
        }
    };
    let per_run = if activity == ACTIVITY_MANUFACTURING { bp.production_time } else { base_seconds };
    let seconds = per_run * time_multiplier * char_time_multiplier * runs as f64;

    let manufacturing = activity == ACTIVITY_MANUFACTURING;
    let material_multiplier = if manufacturing { line_material * (1.0 + waste(bp.waste_factor, bp.material_level)) } else { line_material };
    let char_material_multiplier = if manufacturing { 1.25 - 0.05 * level(SKILL_PRODUCTION_EFFICIENCY) as f64 } else { 1.0 };
    let (mut raw, mut waste_rows, mut tools, mut skills) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for n in need {
        if n.skill {
            skills.push((n.type_id, n.quantity));
        } else if n.damage >= 1.0 {
            let base = (n.quantity as f64 * line_material).round() as i64 * runs;
            let total = (n.quantity as f64 * material_multiplier * char_material_multiplier).round() as i64 * runs;
            raw.push((n.type_id, base));
            if total > base {
                waste_rows.push((n.type_id, total - base));
            }
        } else if n.damage > 0.0 {
            tools.push(n.clone());
        }
    }

    let begin = now.max(line.next_free);
    let hours = (seconds / 3600.0).ceil();
    Plan {
        seconds,
        begin,
        end: begin + (seconds * SECOND as f64) as i64,
        install_cost: line.cost_install,
        usage_cost: line.cost_per_hour * hours,
        material_multiplier,
        char_material_multiplier,
        time_multiplier,
        char_time_multiplier,
        raw,
        waste: waste_rows,
        tools,
        skills,
        output_type: if manufacturing { bp.product } else { bp.item.type_id },
    }
}

fn quote(p: &Plan) -> Value {
    let material = |(t, q): &(i64, i64)| key_val(vec![("requiredTypeID", Value::Int(*t)), ("quantity", Value::Int(*q))]);
    let extras = p
        .tools
        .iter()
        .map(|n| {
            key_val(vec![
                ("requiredTypeID", Value::Int(n.type_id)),
                ("quantity", Value::Int(n.quantity)),
                ("damagePerJob", Value::Double(n.damage)),
                ("isSkillCheck", Value::Int(0)),
            ])
        })
        .collect();
    let bom = key_val(vec![
        ("rawMaterials", Value::List(p.raw.iter().map(material).collect())),
        ("extras", Value::List(extras)),
        ("skills", Value::Dict(p.skills.iter().map(|(t, l)| (Value::Int(*t), Value::Int(*l))).collect())),
        ("wasteMaterials", Value::List(p.waste.iter().map(material).collect())),
    ]);
    row(
        QUOTE_HEADER,
        vec![
            Value::Long(p.begin),
            Value::Double(p.seconds),
            Value::Double(p.cost()),
            Value::Double(p.install_cost),
            Value::Double(p.usage_cost),
            Value::Double(p.material_multiplier),
            Value::Double(p.char_material_multiplier),
            Value::Double(p.time_multiplier),
            Value::Double(p.char_time_multiplier),
            bom,
        ],
    )
}

/// Whether the blueprint is in the station's hangar, or in the hold of a
/// ship there.
fn at_station(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, item: &Item, station: i64) -> Refused<bool> {
    if item.location_id == station {
        // A station's hangar; or anywhere in a starbase structure's hold.
        let structure = txn.db().table("posStructures").get_id(station).is_some();
        return Ok(item.flag == FLAG_HANGAR || structure);
    }
    if item.flag != FLAG_CARGO {
        return Ok(false);
    }
    let ship = txn.item(item.location_id).map_err(|e| db_failed("reading the ship", e))?;
    Ok(ship.is_some_and(|s| s.location_id == station && Some(s.owner_id) == ctx.session.char_id))
}

/// Takes `used` units of `t` from the stacks at a location (assembled ships
/// do not count), after checking `have_at_least` are there.
fn use_up(txn: &mut ItemTxn<'_, '_>, owner: i64, location: i64, flag: i64, t: i64, have_at_least: i64, mut used: i64) -> Refused<()> {
    let mut stacks: Vec<(i64, i64)> = txn
        .db()
        .table("entity")
        .find(&["locationID", "flag", "ownerID"], &[int(location), int(flag), int(owner)])
        .filter(|e| e.int("typeID") == Some(t) && e.int("singleton") == Some(0))
        .map(|e| (e.int("itemID").unwrap_or(0), e.int("quantity").unwrap_or(0)))
        .collect();
    stacks.sort_by_key(|(id, _)| *id);
    if stacks.iter().map(|(_, q)| q).sum::<i64>() < have_at_least {
        return Err(message_with("RamNeedMoreForJob", &[("item", Value::text(type_name(t)))]));
    }
    for (id, q) in stacks {
        if used <= 0 {
            break;
        }
        let take = q.min(used);
        txn.set_quantity(id, q - take).map_err(|e| db_failed("using the materials", e))?;
        used -= take;
    }
    Ok(())
}

/// Jobs a character may have running: manufacturing, or the research kinds
/// together.
fn slots_free(db: &Db, me: i64, activity: i64, level: &(dyn Fn(i32) -> i64 + Sync)) -> Refused<()> {
    let manufacturing = activity == ACTIVITY_MANUFACTURING;
    let allowed = if manufacturing {
        1 + level(SKILL_MASS_PRODUCTION) + level(SKILL_ADVANCED_MASS_PRODUCTION)
    } else {
        1 + level(SKILL_LABORATORY_OPERATION) + level(SKILL_ADVANCED_LABORATORY_OPERATION)
    };
    let running = db
        .table("ramJobs")
        .find(&["installerID", "completed"], &[int(me), int(0)])
        .filter(|j| j.int("activityID").is_some_and(|a| (a == ACTIVITY_MANUFACTURING) == manufacturing))
        .count();
    if running as i64 >= allowed {
        let kind = if manufacturing { "manufacturing" } else { "research" };
        return Err(Outcome::custom(format!("Your skills allow {allowed} {kind} job(s) at a time, and you have {running} running.")));
    }
    Ok(())
}

/// Whether the caller may use a line where they are: their reach skill, in
/// jumps, from where they are.
fn within_reach(ctx: &Ctx<'_>, db: &Db, line: &Line) -> Refused<()> {
    if ctx.session.station_id == Some(line.station) {
        return Ok(());
    }
    // A starbase's lines are worked from its system.
    if line.structure {
        return if ctx.session.solar_system_id2 == Some(line.system) { Ok(()) } else { Err(Outcome::custom("You must be in the starbase's system to install a job there.")) };
    }
    if ctx.session.region_id != Some(line.region) {
        return Err(message("RamRangeLimitationRegion"));
    }
    let skill = if line.activity == ACTIVITY_MANUFACTURING { SKILL_SUPPLY_CHAIN_MANAGEMENT } else { SKILL_SCIENTIFIC_NETWORKING };
    let me = ctx.session.char_id.unwrap_or(0);
    let level = crate::svc::skills::levels(db, me, &[skill]).map_err(|e| db_failed("reading skills", e))?;
    let reach = REACH[level.get(&skill).copied().unwrap_or(0).clamp(0, 5) as usize];
    let here = ctx.session.solar_system_id2.unwrap_or(0);
    let ok = match reach {
        -1 => false,
        50 => true,
        r => crate::gates::jumps(db, here, line.system, r).is_some(),
    };
    if ok {
        Ok(())
    } else if reach == -1 {
        Err(message_with("RamRangeLimitationJumpsNoSkill", &[("jumps", Value::text("some"))]))
    } else {
        Err(message_with("RamRangeLimitation", &[("range", Value::text(format!("jobs within {reach} jumps")))]))
    }
}

// ---- delivering --------------------------------------------------------------------------

/// CompleteJob(installationLocationData, jobID, cancel): a finished job's
/// output and its blueprint go to the owner's hangar at the station; a
/// cancelled one gives the blueprint back alone.
async fn complete(ctx: &Ctx<'_>, job_id: i64, cancel: bool) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (events, activity, type_name) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let r = txn.db().table("ramJobs").get_id(job_id).ok_or_else(|| message("RamCompletionNoSuchJob"))?;
            let i = |c: &str| r.int(c).unwrap_or(0);
            let (owner, item_id, activity, output, station) = (i("installedItemOwnerID"), i("installedItemID"), i("activityID"), i("outputTypeID"), i("outputLocationID"));
            let (runs, licensed_runs, end, completed) = (i("runs"), i("licensedProductionRuns"), i("endProductionTime"), i("completed"));
            let (out_flag, container, job_row) = (i("outputFlag"), i("containerID"), r.id);
            let roles = ctx.session.corp_role();
            let corp_job = Some(owner) == ctx.session.corp_id && roles & (crate::session::CORP_ROLE_DIRECTOR | CORP_ROLE_FACTORY_MANAGER) != 0;
            if owner != me && !corp_job {
                return Err(message("RamCompletionAccessDenied").into());
            }
            if completed != 0 {
                return Err(message("RamCompletionJobCompleted").into());
            }
            if !cancel && crate::bulkdata::filetime_now() < end {
                return Err(message("RamCompletionInProduction").into());
            }
            let bp = blueprint(&mut txn, item_id)?;
            if bp.item.owner_id != owner || bp.item.location_id != container || bp.item.flag != i64::from(FLAG_FACTORY_BLUEPRINT) {
                return Err(Outcome::custom("The installed blueprint is no longer held by this job.").into());
            }
            txn.move_to(item_id, station, out_flag).map_err(|e| db_failed("returning the blueprint", e))?;
            if !cancel {
                deliver(&mut txn, &bp, activity, output, owner, station, out_flag, runs, licensed_runs).map_err(|e| db_failed("delivering the job", e))?;
            }
            let status = if cancel { STATUS_ABORTED } else { STATUS_DELIVERED };
            txn.tx()
                .update("ramJobs", job_row, &[("completed", int(1)), ("completedStatus", int(status.into()))])
                .map_err(|e| db_failed("closing the job", e))?;
            Ok((txn.events(), activity, bp.type_name))
        })
        .map_err(|Undone(o)| o)?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    info!("{me} {} job {job_id} ({} of {type_name})", if cancel { "cancelled" } else { "delivered" }, activity_name(activity));
    Ok(Value::None)
}

/// What a finished job makes: the products, the research on the blueprint,
/// or the copies.
#[allow(clippy::too_many_arguments)]
fn deliver(
    txn: &mut ItemTxn<'_, '_>,
    bp: &Blueprint,
    activity: i64,
    output: i64,
    owner: i64,
    station: i64,
    out_flag: i64,
    runs: i64,
    licensed_runs: i64,
) -> Result<(), StoreError> {
    let item_id = bp.item.id;
    match activity {
        ACTIVITY_MANUFACTURING => {
            let portion = crate::sde::table("invTypes")
                .get("typeID", output as i32 as i64)
                .and_then(|t| t.int("portionSize"))
                .unwrap_or(1)
                .max(1);
            let made = NewItem { type_id: output, owner_id: owner, location_id: station, flag: out_flag, quantity: portion * runs, singleton: false };
            txn.create(made)?;
            if bp.copy {
                let left = bp.runs_left - runs;
                if left <= 0 {
                    txn.tx().delete_where("invBlueprints", &["itemID"], &[int(item_id)])?;
                    txn.destroy(item_id)?;
                } else if let Some(row) = txn.db().table("invBlueprints").get_id(item_id).map(|b| b.id) {
                    txn.tx().update("invBlueprints", row, &[("licensedProductionRunsRemaining", int(left))])?;
                }
            }
        }
        ACTIVITY_TIME => research(txn.tx(), item_id, "productivityLevel", runs)?,
        ACTIVITY_MATERIAL => research(txn.tx(), item_id, "materialLevel", runs)?,
        _ => {
            for _ in 0..runs {
                let copy = NewItem { type_id: output, owner_id: owner, location_id: station, flag: out_flag, quantity: 1, singleton: true };
                let id = txn.create(copy)?;
                txn.tx().insert(
                    "invBlueprints",
                    &[
                        ("itemID", int(id)),
                        ("copy", int(1)),
                        ("materialLevel", int(bp.material_level)),
                        ("productivityLevel", int(bp.productivity_level)),
                        ("licensedProductionRunsRemaining", int(licensed_runs.max(1))),
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Research adds `runs` to the blueprint's own level, from 0 when it has
/// none yet.
fn research(tx: &mut Tx<'_>, item_id: i64, column: &str, runs: i64) -> Result<(), StoreError> {
    match tx.table("invBlueprints").get_id(item_id).map(|b| (b.id, b.int(column).unwrap_or(0))) {
        Some((row, level)) => tx.update("invBlueprints", row, &[(column, int(level + runs))]),
        None => tx.insert("invBlueprints", &[("itemID", int(item_id)), (column, int(runs))]).map(|_| ()),
    }
}

// ---- shared ------------------------------------------------------------------------------

fn activity_name(activity: i64) -> &'static str {
    match activity {
        ACTIVITY_MANUFACTURING => "Manufacturing",
        ACTIVITY_TIME => "Time productivity research",
        ACTIVITY_MATERIAL => "Material research",
        ACTIVITY_COPYING => "Copying",
        _ => "Unknown",
    }
}

fn security_of(db: &Db, char_id: i64) -> f64 {
    db.table("character_").get_id(char_id).and_then(|r| r.float("securityRating")).unwrap_or(0.0)
}

fn type_name(type_id: i64) -> String {
    let t = crate::sde::table("invTypes").get("typeID", type_id as i32 as i64);
    t.and_then(|t| t.text("typeName")).unwrap_or_default().to_string()
}

fn station_name(db: &Db, station: i64) -> String {
    db.table("staStations").get_id(station).and_then(|r| r.text("stationName")).unwrap_or_default().to_string()
}

fn message_with(key: &str, args: &[(&str, Value)]) -> Outcome {
    Outcome::UserError { msg: key.into(), args: args.iter().map(|(k, v)| ((*k).into(), v.clone())).collect() }
}
