use std::collections::{BTreeSet, HashMap};

use eve_marshal::Value;

use crate::sde;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db};
use crate::svc::rows::{header_value, index_rowset, rowset};

const LOCATION_UNIVERSE: i64 = 9;

pub async fn config(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let ids = || id_args(call);
    let arg = |i: usize| call.arg_i64(i);
    let flag = |i: usize| arg(i).unwrap_or(0) != 0;

    let value = match call.method.as_str() {
        "CacheFileGetRegions" => map_objects(ctx, LOCATION_UNIVERSE, true, false, false, false).await,
        "CacheFileGetConstellations" => map_objects(ctx, call.arg_id(1), false, true, false, false).await,
        "CacheFileGetSystems" => map_objects(ctx, call.arg_id(1), false, false, true, false).await,
        "CacheFileGetLandmarks" => landmarks(),
        "GetMapOffices" => map_offices(ctx, call.arg_id(0)).await,
        "GetOwnersEx" | "GetMultiOwnersEx" => owners(ctx, &ids()).await,
        "GetLocationsEx" | "GetMultiLocationsEx" => locations(ctx, &ids()).await,
        "GetCorpTickerNamesEx" | "GetMultiCorpTickerNamesEx" => {
            keyed_live(ctx, "corporation", CORP_TICKER_COLUMNS, &ids()).await
        }
        "GetAllianceShortNamesEx" | "GetMultiAllianceShortNamesEx" => {
            keyed_live(ctx, "alliance_shortnames", ALLIANCE_SHORTNAME_COLUMNS, &ids()).await
        }
        "GetInvTypesEx" | "GetMultiInvTypesEx" => {
            keyed_static("invTypes", "typeID", INV_TYPES, &ids())
        }
        "GetInvGroupsEx" | "GetMultiInvGroupsEx" => {
            keyed_static("invGroups", "groupID", INV_GROUPS, &ids())
        }
        "GetInvCategoriesEx" | "GetMultiInvCategoriesEx" => {
            keyed_static("invCategories", "categoryID", INV_CATEGORIES, &ids())
        }
        "GetInvMetaGroupsEx" | "GetMultiInvMetaGroupsEx" => {
            keyed_static("invMetaGroups", "metaGroupID", INV_METAGROUPS, &ids())
        }
        "GetBlueprintTypeEx" | "GetMultiBlueprintTypeEx" => {
            keyed_static("invBlueprintTypes", "blueprintTypeID", BPTYPES, &ids())
        }
        "GetGraphicsEx" | "GetMultiGraphicsEx" => {
            keyed_static("eveGraphics", "graphicID", GRAPHICS, &ids())
        }
        "GetAssemblyLineTypeEx" | "GetMultiAssemblyLineTypeEx" => {
            keyed_static("ramAssemblyLineTypes", "assemblyLineTypeID", RAMALTYPES, &ids())
        }

        "GetMapObjects" => {
            map_objects(
                ctx,
                arg(0).unwrap_or(LOCATION_UNIVERSE),
                flag(1),
                flag(2),
                flag(3),
                flag(4),
            )
            .await
        }
        "GetMapConnections" => {
            map_connections(ctx, arg(0).unwrap_or(LOCATION_UNIVERSE), flag(1), flag(2), flag(3))
                .await
        }
        "GetMap" => map_of_system(ctx, arg(0).unwrap_or(0)).await,
        "GetMapLandmarks" => landmarks(),
        "SetMapLandmarks" => Value::None,
        "GetCelestialStatistic" => celestial_statistic(arg(0).unwrap_or(0)),
        "GetUnits" => units(),
        "GetStationSolarSystemsByOwner" => {
            station_systems_by_owner(ctx, arg(0).unwrap_or(0)).await
        }

        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn id_args(call: &Call) -> Vec<i32> {
    let mut out = Vec::new();
    let mut push = |v: &Value| {
        if let Some(n) = v.as_i64() {
            if let Ok(n) = i32::try_from(n) {
                out.push(n);
            }
        }
    };
    match call.args.first() {
        Some(Value::List(v)) | Some(Value::Tuple(v)) => v.iter().for_each(&mut push),
        Some(v) => push(v),
        None => {}
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A rowset as the client's cached lookups take it: (header, lines).
fn keyed(header: &[&str], lines: Vec<Value>) -> Value {
    Value::Tuple(vec![header_value(header), Value::List(lines)])
}

/// A live table's rows by its primary key (the first of `columns`), in key
/// order, as a `SELECT` of `columns` `WHERE key = ANY(ids)` found them
/// through the primary key's index.
async fn keyed_live(ctx: &Ctx<'_>, table: &str, columns: &[&str], ids: &[i32]) -> Value {
    let lines = ctx.store.read(|db| {
        let table = db.table(table);
        ids.iter().filter_map(|id| table.get_id(i64::from(*id))).map(|r| Value::List(r.line(columns))).collect()
    });
    keyed(columns, lines)
}

/// A static table's rows by key, as the database gave a `SELECT` of `columns`
/// `WHERE key = ANY(ids)`: in key order, as the primary key's index found
/// them (the dumps are in key order too).
fn keyed_static(table: &str, key: &str, columns: &[&str], ids: &[i32]) -> Value {
    let table = sde::table(table);
    let lines = ids.iter().flat_map(|id| table.find(key, *id as i64)).map(|r| Value::List(r.line(columns))).collect();
    keyed(columns, lines)
}

/// Each ID's owner name and type, from the static owners, characters (a
/// character's type is its bloodline's when it has none of its own),
/// corporations or alliances; an ID none of them names is left out.
async fn owners(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let bloodlines = sde::table("bloodlineTypes");
    let lines = ctx.store.read(|db| {
        let mut lines = Vec::new();
        for &id in ids {
            let id = i64::from(id);
            let o = db.table("eveStaticOwners").get_id(id);
            let c = db.table("character_").get_id(id);
            let co = db.table("corporation").get_id(id);
            let a = db.table("alliances").get_id(id);
            let name = [o.map(|o| o.cell("ownerName")), c.map(|c| c.cell("characterName")), co.map(|co| co.cell("corporationName")), a.map(|a| a.cell("allianceName"))]
                .into_iter()
                .flatten()
                .find(|v| *v != Value::None);
            let Some(name) = name else { continue };
            let own_type = o.and_then(|o| o.int("typeID")).filter(|t| *t != 0).or_else(|| c.and_then(|c| c.int("typeID")).filter(|t| *t != 0));
            // One row per matching bloodline row, as the join gave.
            let bloodline: Vec<Option<i64>> = match c.and_then(|c| c.int("bloodlineID")) {
                Some(b) => bloodlines.find("bloodlineID", b).map(|r| r.int("typeID")).collect(),
                None => Vec::new(),
            };
            let bloodline = if bloodline.is_empty() { vec![None] } else { bloodline };
            for bt in bloodline {
                let Some(type_id) = own_type.or(bt).or(co.map(|_| 2)).or(a.map(|_| 16159)) else { continue };
                lines.push(Value::List(vec![Value::Int(id), name.clone(), Value::Int(type_id)]));
            }
        }
        lines
    });
    keyed(HEADER, lines)
}

/// Each ID's name and position, from the static locations, the map or the
/// items. Location 0 answers with an empty name: the selected-item panel
/// asks for it (gang.CanJumpThrough returns False for any non-bridging ship).
async fn locations(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const HEADER: &[&str] = &["locationID", "locationName", "x", "y", "z"];
    let statics = sde::table("eveStaticLocations");
    let lines = ctx.store.read(|db| {
        let mut lines = Vec::new();
        for &id in ids {
            let id = i64::from(id);
            let d = db.table("mapDenormalize").get_id(id);
            let e = db.table("entity").get_id(id);
            let found: Vec<sde::Row> = statics.find("locationID", id).collect();
            let found: Vec<Option<sde::Row>> = if found.is_empty() { vec![None] } else { found.into_iter().map(Some).collect() };
            for l in found {
                let name = l.and_then(|l| l.text("locationName")).or_else(|| d.and_then(|d| d.text("itemName"))).or_else(|| e.and_then(|e| e.text("itemName")));
                let name = match name {
                    Some(name) => name,
                    None if id == 0 => "",
                    None => continue,
                };
                let at = |c: &str| Value::Double(l.and_then(|l| l.float(c)).or_else(|| d.and_then(|d| d.float(c))).or_else(|| e.and_then(|e| e.float(c))).unwrap_or(0.0));
                lines.push(Value::List(vec![Value::Int(id), Value::text(name), at("x"), at("y"), at("z")]));
            }
        }
        lines
    });
    // What is only in space (rats, asteroids, cans, beacons, sites) has no
    // row; the client still looks it up before warping to it (menusvc.py
    // WarpToItem names it in "Warping to ..."), and fails without one.
    let found: BTreeSet<i64> = lines.iter().filter_map(|l| match l { Value::List(c) => c.first().and_then(|v| v.as_i64()), _ => None }).collect();
    let missing: Vec<i64> = ids.iter().map(|&id| i64::from(id)).filter(|id| *id != 0 && !found.contains(id)).collect();
    let mut lines = lines;
    if !missing.is_empty() {
        let w = ctx.world.lock();
        let here = ctx.session.solar_system_id.and_then(|s| w.parks.get(&s));
        for id in missing {
            let live = here.and_then(|p| live_location(p, id)).or_else(|| w.parks.values().find_map(|p| live_location(p, id)));
            if let Some((name, at)) = live {
                lines.push(Value::List(vec![Value::Int(id), Value::text(name), Value::Double(at.x), Value::Double(at.y), Value::Double(at.z)]));
            }
        }
    }
    keyed(HEADER, lines)
}

/// The name and position of a transient object in `park`, by ID.
fn live_location(park: &crate::ballpark::Park, id: i64) -> Option<(String, crate::ballpark::Vec3)> {
    let type_name = |t: i64| sde::table("invTypes").get("typeID", t).and_then(|r| r.text("typeName").map(str::to_string)).unwrap_or_default();
    let named = |name: &str, type_id: i64| if name.is_empty() { type_name(type_id) } else { name.to_string() };
    if let Some(s) = park.ships.get(&id) {
        return Some((type_name(s.pilot.type_id), s.pos));
    }
    if let Some(r) = park.rocks.get(&id) {
        return Some((named(&r.name, r.type_id), r.pos));
    }
    if let Some(c) = park.cans.get(&id) {
        return Some((named(&c.name, c.type_id), c.pos));
    }
    if let Some(b) = park.beacons.get(&id) {
        return Some((named(&b.name, b.type_id), b.pos));
    }
    if let Some(s) = park.sites.get(&id) {
        return Some((s.name.clone(), s.pos));
    }
    park.structures.get(&id).map(|s| (named(&s.name, s.type_id), s.pos))
}

const CORP_TICKER_COLUMNS: &[&str] = &["corporationID", "tickerName", "shape1", "shape2", "shape3", "color1", "color2", "color3"];

const ALLIANCE_SHORTNAME_COLUMNS: &[&str] = &["allianceID", "shortName"];

const INV_TYPES: &[&str] = &[
    "typeID", "groupID", "typeName", "description", "graphicID", "radius", "mass",
    "volume", "capacity", "portionSize", "raceID", "basePrice", "published",
    "marketGroupID", "chanceOfDuplicating",
];

const INV_GROUPS: &[&str] = &[
    "groupID", "categoryID", "groupName", "description", "graphicID", "useBasePrice",
    "allowManufacture", "allowRecycler", "anchored", "anchorable", "fittableNonSingleton",
];

const INV_CATEGORIES: &[&str] = &["categoryID", "categoryName", "description", "graphicID"];

const INV_METAGROUPS: &[&str] = &["metaGroupID", "metaGroupName", "description", "graphicID"];

const BPTYPES: &[&str] = &[
    "blueprintTypeID", "parentBlueprintTypeID", "productTypeID", "productionTime", "techLevel",
];

const GRAPHICS: &[&str] = &[
    "graphicID", "url3D", "urlWeb", "description", "published", "obsolete", "icon",
    "urlSound", "explosionID",
];

const RAMALTYPES: &[&str] = &[
    "assemblyLineTypeID", "assemblyLineTypeName", "description", "baseTimeMultiplier",
    "baseMaterialMultiplier", "volume", "activityID", "minCostPerHour",
];

const MAP_OBJECT_HEADER: &[&str] = &[
    "itemID", "itemName", "typeID", "groupID", "locationID", "orbitID", "x", "y", "z",
    "factionID", "radius", "security",
];

/// The regions (all of them, from the universe), as map objects of type 3
/// in the universe.
fn region_objects(id: i32) -> impl Iterator<Item = Value> {
    sde::table("mapRegions")
        .rows()
        .filter(move |r| id == 9 || r.int("regionID") == Some(id as i64))
        .map(|r| {
            let mut l = r.line(&["regionID", "regionName"]);
            l.extend([Value::Int(3), Value::Int(3), Value::Int(LOCATION_UNIVERSE), Value::None]);
            l.extend(r.line(&["x", "y", "z", "factionID", "radius"]));
            l.push(Value::None);
            Value::List(l)
        })
}

/// A region's constellations (or all of them, from the universe), as map
/// objects of type 4 in their region.
fn constellation_objects(id: i32) -> impl Iterator<Item = Value> {
    sde::table("mapConstellations")
        .rows()
        .filter(move |r| id == 9 || r.int("regionID") == Some(id as i64))
        .map(|r| {
            let mut l = r.line(&["constellationID", "constellationName"]);
            l.extend([Value::Int(4), Value::Int(4)]);
            l.extend(r.line(&["regionID"]));
            l.push(Value::None);
            l.extend(r.line(&["x", "y", "z", "factionID", "radius"]));
            l.push(Value::None);
            Value::List(l)
        })
}

/// A constellation's systems (or all of them, from the universe), as map
/// objects of type 5 in their constellation.
fn system_objects(db: &Db, id: i64) -> Vec<Value> {
    db.table("mapSolarSystems")
        .rows()
        .filter(|r| id == 9 || r.int("constellationID") == Some(id))
        .map(|r| {
            let mut l = r.line(&["solarSystemID", "solarSystemName"]);
            l.extend([Value::Int(5), Value::Int(5), r.cell("constellationID"), Value::None]);
            l.extend(r.line(&["x", "y", "z", "factionID", "radius", "security"]));
            Value::List(l)
        })
        .collect()
}

/// A system's celestials, as map objects in their system.
fn celestial_objects(db: &Db, id: i64) -> Vec<Value> {
    if id == 9 {
        return Vec::new();
    }
    db.table("mapDenormalize")
        .find(&["solarSystemID"], &[store::Value::Int(id)])
        .map(|r| {
            let mut l = r.line(&["itemID", "itemName", "typeID", "groupID", "solarSystemID", "orbitID", "x", "y", "z"]);
            l.push(Value::None);
            l.extend(r.line(&["radius", "security"]));
            Value::List(l)
        })
        .collect()
}

async fn map_objects(
    ctx: &Ctx<'_>,
    id: i64,
    regions: bool,
    constellations: bool,
    systems: bool,
    celestials: bool,
) -> Value {
    let id32 = i32::try_from(id).unwrap_or(0);
    // Regions and constellations are static data; they came first, ahead of
    // the systems and celestials the store holds.
    let mut found = Vec::new();
    if regions {
        found.extend(region_objects(id32));
    }
    if constellations {
        found.extend(constellation_objects(id32));
    }
    if systems || celestials {
        let id = i64::from(id32);
        ctx.store.read(|db| {
            if systems {
                found.extend(system_objects(db, id));
            }
            if celestials {
                found.extend(celestial_objects(db, id));
            }
        });
    }
    rowset(MAP_OBJECT_HEADER, found)
}

/// A connection: (ctype 0, fromRegionID, fromConstellationID,
/// fromSolarSystemID, stargateID, celestialID, toSolarSystemID,
/// toConstellationID, toRegionID), the parts a jump does not have None.
fn connection(parts: [Value; 8]) -> Value {
    let mut l = vec![Value::Int(0)];
    l.extend(parts);
    Value::Tuple(l)
}

/// A key sorting as `ORDER BY` sorts the column ascending: NULLs last.
fn nulls_last(v: Option<i64>) -> (bool, Option<i64>) {
    (v.is_none(), v)
}

async fn map_connections(ctx: &Ctx<'_>, id: i64, reg: bool, con: bool, sol: bool) -> Value {
    let id32 = i32::try_from(id).unwrap_or(0);
    let from = |r: &sde::Row, column: &str| id32 == 9 || r.int(column) == Some(id32 as i64);
    let mut found = Vec::new();
    if reg {
        for r in sde::table("mapRegionJumps").rows().filter(|r| from(r, "fromRegionID")) {
            let [from_region, to_region] = [r.cell("fromRegionID"), r.cell("toRegionID")];
            found.push(connection([from_region, Value::None, Value::None, Value::None, Value::None, Value::None, Value::None, to_region]));
        }
    }
    if con {
        for r in sde::table("mapConstellationJumps").rows().filter(|r| from(r, "fromConstellationID")) {
            let [a, b, c, d] = ["fromRegionID", "fromConstellationID", "toConstellationID", "toRegionID"].map(|c| r.cell(c));
            found.push(connection([a, b, Value::None, Value::None, Value::None, Value::None, c, d]));
        }
    }
    if sol {
        // The system jumps asked for, one per pair of systems and sorted by
        // them, each with the first gate pair the map places in those two
        // systems (its gate and the gate it leads to).
        let mut jumps: Vec<sde::Row> = sde::table("mapSolarSystemJumps").rows().filter(|r| from(r, "fromSolarSystemID")).collect();
        jumps.sort_by_key(|r| (nulls_last(r.int("fromSolarSystemID")), nulls_last(r.int("toSolarSystemID"))));
        jumps.dedup_by_key(|r| (r.int("fromSolarSystemID"), r.int("toSolarSystemID")));
        let gates: HashMap<(i64, i64), (i64, i64)> = ctx.store.read(|db| {
            let map = db.table("mapDenormalize");
            let system = |item: i64| map.get_id(item).and_then(|d| d.int("solarSystemID"));
            let mut gates = HashMap::new();
            for g in sde::table("mapJumps").rows() {
                let (Some(gate), Some(destination)) = (g.int("stargateID"), g.int("celestialID")) else { continue };
                if let (Some(a), Some(b)) = (system(gate), system(destination)) {
                    gates.entry((a, b)).or_insert((gate, destination));
                }
            }
            gates
        });
        let int = |v: Option<i64>| v.map_or(Value::None, Value::Int);
        for j in jumps {
            let [from_region, from_constellation, from_system, to_system, to_constellation, to_region] =
                ["fromRegionID", "fromConstellationID", "fromSolarSystemID", "toSolarSystemID", "toConstellationID", "toRegionID"].map(|c| j.int(c));
            let gate = from_system.zip(to_system).and_then(|pair| gates.get(&pair));
            found.push(Value::Tuple(vec![
                Value::Int(0),
                int(from_region),
                int(from_constellation),
                int(from_system),
                int(gate.map(|g| g.0)),
                int(gate.map(|g| g.1)),
                int(to_system),
                int(to_constellation),
                int(to_region),
            ]));
        }
    }
    Value::List(found)
}

async fn map_of_system(ctx: &Ctx<'_>, solar_system_id: i64) -> Value {
    const HEADER: &[&str] = &["itemID", "typeID", "locationID", "x", "y", "z", "itemName"];
    let id = i64::from(i32::try_from(solar_system_id).unwrap_or(0));
    let lines = ctx.store.read(|db| {
        let mut lines = Vec::new();
        if let Some(s) = db.table("mapSolarSystems").get_id(id) {
            let mut l = vec![s.cell("solarSystemID"), Value::Int(5), s.cell("solarSystemID")];
            l.extend(s.line(&["x", "y", "z", "solarSystemName"]));
            lines.push(Value::List(l));
        }
        lines.extend(
            db.table("mapDenormalize")
                .find(&["solarSystemID"], &[store::Value::Int(id)])
                .map(|d| Value::List(d.line(&["itemID", "typeID", "solarSystemID", "x", "y", "z", "itemName"]))),
        );
        lines
    });
    rowset(HEADER, lines)
}

fn landmarks() -> Value {
    const HEADER: &[&str] = &[
        "landmarkID", "landmarkName", "description", "locationID", "x", "y", "z", "radius",
        "graphicID", "importance", "url2d",
    ];
    let mut found: Vec<sde::Row> = sde::table("mapLandmarks").rows().collect();
    found.sort_by_key(|r| r.int("landmarkID"));
    rowset(HEADER, found.iter().map(|r| Value::List(r.line(HEADER))).collect())
}

fn celestial_statistic(item_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "celestialID", "temperature", "spectralClass", "luminosity", "age", "life",
        "orbitRadius", "eccentricity", "massDust", "massGas", "fragmented", "density",
        "surfaceGravity", "escapeVelocity", "orbitPeriod", "rotationRate", "locked",
        "pressure", "radius", "mass",
    ];
    let id32 = i32::try_from(item_id).unwrap_or(0);
    let found = sde::table("mapCelestialStatistics").find("celestialID", id32 as i64);
    rowset(HEADER, found.map(|r| Value::List(r.line(HEADER))).collect())
}

fn units() -> Value {
    const HEADER: &[&str] = &["unitID", "unitName", "displayName", "description"];
    let mut found: Vec<sde::Row> = sde::table("eveUnits").rows().collect();
    found.sort_by_key(|r| r.int("unitID"));
    index_rowset(
        HEADER,
        "unitID",
        found
            .iter()
            .map(|r| {
                let l = r.line(HEADER);
                (l.first().cloned().unwrap_or(Value::None), Value::List(l))
            })
            .collect(),
    )
}

async fn station_systems_by_owner(ctx: &Ctx<'_>, owner_id: i64) -> Value {
    const HEADER: &[&str] = &["solarSystemID"];
    let owner = i64::from(i32::try_from(owner_id).unwrap_or(0));
    // A station's owner is its item's, or its corporation's when it has no
    // item.
    let systems: BTreeSet<(bool, Option<i64>)> = ctx.store.read(|db| {
        let entity = db.table("entity");
        db.table("staStations")
            .rows()
            .filter(|st| {
                let item_owner = entity.get(std::slice::from_ref(st.value("stationID"))).and_then(|e| e.int("ownerID"));
                item_owner.or(st.int("corporationID")) == Some(owner)
            })
            .map(|st| nulls_last(st.int("solarSystemID")))
            .collect()
    });
    rowset(HEADER, systems.into_iter().map(|(_, s)| Value::List(vec![s.map_or(Value::None, Value::Int)])).collect())
}

/// The offices in a system's stations: (stationID, corporationID), sorted.
async fn map_offices(ctx: &Ctx<'_>, solar_system_id: i64) -> Value {
    let id = i64::from(i32::try_from(solar_system_id).unwrap_or(0));
    let mut offices: Vec<(i64, i64)> = ctx.store.read(|db| {
        let offices = db.table("crpOffices");
        db.table("staStations")
            .find(&["solarSystemID"], &[store::Value::Int(id)])
            .flat_map(|s| offices.find(&["stationID"], std::slice::from_ref(s.value("stationID"))))
            .map(|o| (o.int("stationID").unwrap_or(0), o.int("corporationID").unwrap_or(0)))
            .collect()
    });
    offices.sort_unstable();
    rowset(&["stationID", "corporationID"], offices.into_iter().map(|(s, c)| Value::List(vec![Value::Int(s), Value::Int(c)])).collect())
}
