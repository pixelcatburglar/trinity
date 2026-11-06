//! Player outposts (menusvc.py "Assemble Platform", "Access Resources",
//! "Build"; base_stationmanagement.py).
//!
//! An outpost platform, assembled in a hangar (invbroker
//! AssembleConstructionPlatform), is launched for a corporation like a
//! starbase structure. In space, `Build` on it (from its owner corporation's
//! directors) starts construction, if the corporation's alliance holds the
//! system's sovereignty, the system has no station yet, and the platform is
//! near a planet. Construction takes `BUILD_HOURS` (scaled like starbases);
//! then the platform becomes a real station of its race's outpost type and
//! services, owned by the corporation, which anyone the station allows may
//! dock at.
//!
//! The corporation that owns a station runs it from the station management
//! window (corpStationMgr): its name and description, docking and office
//! costs, the reprocessing take, who may use each service, and the clones
//! installed there.

mod management;
pub use management::ensure as ensure_management_schema;

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::Ctx;
use crate::store::{self, Db, Store, StoreError, Tx};

pub const GROUP_CONSTRUCTION_PLATFORM: i64 = 307;
const GROUP_PLANET: i64 = 7;
const GROUP_STATION: i64 = 15;
const BUILD_HOURS: f64 = 24.0;
/// A platform must be this close to a planet to be built.
const PLANET_RANGE: f64 = 1.0e10;
const ROLE_DIRECTOR: i64 = crate::session::CORP_ROLE_DIRECTOR;

/// The outpost each platform becomes: (platform, station type, operation).
const OUTPOSTS: [(i64, i64, i64); 5] = [(10257, 21645, 50), (10258, 21646, 51), (10260, 21644, 49), (19758, 21642, 48), (22313, 21646, 51)];

pub type Refused<T> = Result<T, String>;

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

fn float(v: f64) -> store::Value {
    store::Value::Float(v)
}

fn text(v: &str) -> store::Value {
    store::Value::Text(v.into())
}

/// AssembleConstructionPlatform / AssembleCargoContainer(itemID, ...): the
/// packaged item in a hangar becomes one ready to use.
pub async fn assemble(ctx: &Ctx<'_>, item_id: i64) -> Refused<()> {
    let me = ctx.session.char_id.ok_or("No character.")?;
    let result = ctx.store.write(|tx| {
        let mut txn = crate::items::ItemTxn::new(tx);
        let Some(item) = txn.item(item_id)? else { return Ok(Err("That item no longer exists.")) };
        if item.owner_id != me && Some(item.owner_id) != ctx.session.corp_id {
            return Ok(Err("That is not yours."));
        }
        if item.quantity > 1 {
            txn.set_quantity(item_id, item.quantity - 1)?;
            txn.create(crate::items::NewItem { type_id: item.type_id, owner_id: item.owner_id, location_id: item.location_id, flag: item.flag, quantity: 1, singleton: true })?;
        } else {
            txn.set_singleton(item_id, true)?;
        }
        Ok::<_, StoreError>(Ok(txn.events()))
    });
    match result {
        Ok(Ok(events)) => {
            crate::items::notify(ctx.world, ctx.client_id, events);
            Ok(())
        }
        Ok(Err(why)) => Err(why.into()),
        Err(e) => {
            error!("assembling {item_id}: {}", crate::logging::chain(&e));
            Err("It could not be assembled.".into())
        }
    }
}

/// What `Build` needs to know of a platform in space.
struct Site {
    type_id: i64,
    owner: i64,
    /// The alliance holding the system's sovereignty.
    sov: Option<i64>,
    owner_alliance: i64,
    stations: usize,
    /// The distance to the nearest planet, None without one.
    planet_gap: Option<f64>,
    building: bool,
}

fn site(db: &Db, platform: i64, system: i64) -> Option<Site> {
    let e = db.table("entity").get_id(platform).filter(|e| e.int("locationID") == Some(system))?;
    let s = db.table("mapSolarSystems").get_id(system)?;
    let owner = e.int("ownerID").unwrap_or(0);
    let (x, y, z) = (e.float("x")?, e.float("y")?, e.float("z")?);
    let planet_gap = db
        .table("mapDenormalize")
        .find(&["solarSystemID"], &[int(system)])
        .filter(|d| d.int("groupID") == Some(GROUP_PLANET))
        .filter_map(|d| Some(((d.float("x")? - x).powi(2) + (d.float("y")? - y).powi(2) + (d.float("z")? - z).powi(2)).sqrt()))
        .min_by(f64::total_cmp);
    Some(Site {
        type_id: e.int("typeID").unwrap_or(0),
        owner,
        sov: s.int("allianceID"),
        owner_alliance: db.table("corporation").get_id(owner).and_then(|co| co.int("allianceID")).unwrap_or(0),
        stations: db.table("staStations").find(&["solarSystemID"], &[int(system)]).count(),
        planet_gap,
        building: db.table("outpostConstruction").get_id(platform).is_some(),
    })
}

/// `Build` on a platform in space: construction starts.
pub async fn build(ctx: &Ctx<'_>, platform: i64) -> Refused<()> {
    let (Some(me), Some(system)) = (ctx.session.char_id, ctx.session.solar_system_id) else { return Err("You must be in space by the platform.".into()) };
    let site = ctx.store.read(|db| site(db, platform, system)).ok_or("That platform is not here.")?;
    let (type_id, owner) = (site.type_id, site.owner);
    // Something of a type the data does not know is no platform at all.
    let group = crate::sde::get().type_(type_id).ok_or("That platform is not here.")?.group_id as i64;
    let (sov, owner_alliance, stations) = (site.sov, site.owner_alliance, site.stations);
    let planet_gap = site.planet_gap.unwrap_or(f64::MAX);
    if group != GROUP_CONSTRUCTION_PLATFORM {
        return Err("Only an outpost platform can be built into an outpost.".into());
    }
    if site.building {
        return Err("That outpost is already being built.".into());
    }
    if Some(owner) != ctx.session.corp_id || !ctx.session.has_corp_role(ROLE_DIRECTOR) {
        return Err("Only a director of the platform's corporation can build it.".into());
    }
    if owner_alliance == 0 || sov != Some(owner_alliance) {
        return Err("Your alliance must hold sovereignty in this system to build an outpost here.".into());
    }
    if stations > 0 {
        return Err("This system already has a station.".into());
    }
    if planet_gap > PLANET_RANGE {
        return Err("An outpost must be built near a planet.".into());
    }
    if !OUTPOSTS.iter().any(|(p, _, _)| *p == type_id) {
        return Err("That platform has no outpost design.".into());
    }
    let now = crate::bulkdata::filetime_now();
    let hours = BUILD_HOURS / crate::pos::speedup().max(0.001);
    let done = now + (hours * 3_600.0 * 10_000_000.0) as i64;
    ctx.store
        .write(|tx| {
            tx.insert(
                "outpostConstruction",
                &[
                    ("platformID", int(platform)),
                    ("ownerID", int(owner)),
                    ("solarSystemID", int(system)),
                    ("startedAt", int(now)),
                    ("doneAt", int(done)),
                    ("builderID", int(me)),
                ],
            )
        })
        .map_err(|e| {
            error!("starting outpost {platform}: {}", crate::logging::chain(&e));
            "The construction could not start.".to_string()
        })?;
    info!("{me} started building outpost platform {platform} in {system}, done in {hours:.1} h");
    Ok(())
}

/// A finished construction, and where its station goes.
struct Due {
    platform: i64,
    owner: i64,
    system: i64,
    type_id: i64,
    pos: (f64, f64, f64),
    constellation: i64,
    region: i64,
    security: f64,
    name: String,
}

/// Outposts whose construction is done become stations. Run by the heartbeat.
pub async fn finish(world: &crate::world::World, store: &Store) {
    let now = crate::bulkdata::filetime_now();
    let due: Vec<Due> = store.read(|db| {
        db.table("outpostConstruction")
            .rows()
            .filter(|o| o.int("doneAt").is_some_and(|d| d <= now))
            .filter_map(|o| {
                let platform = o.int("platformID")?;
                let owner = o.int("ownerID").unwrap_or(0);
                let system = o.int("solarSystemID").unwrap_or(0);
                let e = db.table("entity").get_id(platform)?;
                let s = db.table("mapSolarSystems").get_id(system)?;
                let corp_name = db.table("corporation").get_id(owner).and_then(|co| co.text("corporationName")).unwrap_or_default();
                Some(Due {
                    platform,
                    owner,
                    system,
                    type_id: e.int("typeID").unwrap_or(0),
                    pos: (e.float("x").unwrap_or(0.0), e.float("y").unwrap_or(0.0), e.float("z").unwrap_or(0.0)),
                    constellation: s.int("constellationID").unwrap_or(0),
                    region: s.int("regionID").unwrap_or(0),
                    security: s.float("security").unwrap_or(0.0),
                    name: format!("{} - {corp_name} Outpost", s.text("solarSystemName").unwrap_or_default()),
                })
            })
            .collect()
    });
    for d in due {
        let Some(&(_, station_type, operation)) = OUTPOSTS.iter().find(|(p, _, _)| *p == d.type_id) else { continue };
        let made = store.write(|tx| make_station(tx, &d, station_type, operation));
        match made {
            Ok(station) => {
                info!("outpost {} ({station}) built for corporation {}", d.name, d.owner);
                show(world, store, d.system, d.platform, station);
            }
            Err(e) => error!("building outpost from platform {}: {}", d.platform, crate::logging::chain(&e)),
        }
    }
}

/// The station, its celestial and name, in place of the platform.
fn make_station(tx: &mut Tx<'_>, d: &Due, station_type: i64, operation: i64) -> Result<i64, StoreError> {
    let (x, y, z) = d.pos;
    let taken = tx.table("mapDenormalize").rows().filter_map(|m| m.int("itemID")).filter(|id| (61_000_000..62_000_000).contains(id)).max();
    let station = taken.unwrap_or(61_000_000).max(61_000_000) + 1;
    let place = [("solarSystemID", int(d.system)), ("constellationID", int(d.constellation)), ("regionID", int(d.region)), ("x", float(x)), ("y", float(y)), ("z", float(z))];
    let mut sets = vec![
        ("stationID", int(station)),
        ("security", int((d.security * 1000.0) as i64)),
        ("dockingCostPerVolume", float(0.0)),
        ("maxShipVolumeDockable", int(50_000_000)),
        ("officeRentalCost", int(10_000)),
        ("operationID", int(operation)),
        ("stationTypeID", int(station_type)),
        ("corporationID", int(d.owner)),
        ("stationName", text(&d.name)),
        ("reprocessingEfficiency", float(0.5)),
        ("reprocessingStationsTake", float(0.05)),
        ("reprocessingHangarFlag", int(4)),
    ];
    sets.extend(place.iter().cloned());
    tx.insert("staStations", &sets)?;
    // The station's celestial is made only for a station type the data knows.
    if let Some(t) = crate::sde::table("invTypes").get("typeID", station_type) {
        let radius = t.float("radius").unwrap_or(15000.0);
        let mut sets = vec![
            ("itemID", int(station)),
            ("typeID", int(station_type)),
            ("groupID", int(GROUP_STATION)),
            ("radius", float(radius)),
            ("itemName", text(&d.name)),
            ("security", float(d.security)),
        ];
        sets.extend(place.iter().cloned());
        tx.insert("mapDenormalize", &sets)?;
    }
    tx.insert_or_ignore(
        "eveNames",
        &[("itemID", int(station)), ("itemName", text(&d.name)), ("categoryID", int(3)), ("groupID", int(GROUP_STATION)), ("typeID", int(station_type))],
    )?;
    tx.delete_where("outpostConstruction", &["platformID"], &[int(d.platform)])?;
    tx.delete_where("posStructures", &["itemID"], &[int(d.platform)])?;
    tx.delete_where("entity", &["itemID"], &[int(d.platform)])?;
    tx.delete_where("entity", &["locationID"], &[int(d.platform)])?;
    Ok(station)
}

/// The platform goes from the system's park and the station appears.
fn show(world: &crate::world::World, store: &Store, system: i64, platform: i64, station: i64) {
    let celestial = store.read(|db| {
        let r = db.table("mapDenormalize").get_id(station)?;
        Some(crate::ballpark::Celestial {
            id: station,
            type_id: r.int("typeID").unwrap_or(0),
            group_id: GROUP_STATION,
            name: r.text("itemName").unwrap_or_default().to_string(),
            pos: crate::ballpark::Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0)),
            radius: r.float("radius").unwrap_or(15_000.0),
            jump: None,
        })
    });
    let Some(celestial) = celestial else { return };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    let mut entries = Vec::new();
    if park.structures.remove(&platform).is_some() {
        entries.push(crate::svc::space::entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(platform)])]));
    }
    let ball = crate::destiny::Ball::celestial(celestial.id, celestial.radius, celestial.pos.tuple(), &celestial.name, true);
    let blob = crate::destiny::encode(crate::destiny::stamp(park.tick), &[ball]);
    let slim = crate::svc::space::slim(vec![("itemID", Value::Int(celestial.id)), ("typeID", Value::Int(celestial.type_id)), ("name", Value::text(&celestial.name))]);
    entries.push(crate::svc::space::entry(park.tick, "AddBalls", vec![Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![slim]), Value::Dict(Vec::new())])]));
    park.celestials.push(celestial);
    crate::svc::space::broadcast(&w, system, None, entries);
}

// ---- station management ---------------------------------------------------------------

fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// The corpStationMgr calls of the station management window; None for others.
pub async fn station_mgr(ctx: &Ctx<'_>, method: &str, args: &[Value], station: i64) -> Option<Result<Value, String>> {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    Some(match method {
        "GetStationServiceIdentifiers" => {
            let mut services: Vec<_> = crate::sde::table("staServices").rows().collect();
            services.sort_by_key(|s| s.int("serviceID"));
            let lines = services.iter().map(|s| Value::List(vec![s.cell("serviceID"), s.cell("serviceName"), Value::Int(0)])).collect();
            Ok(crate::svc::rows::rowset(&["serviceID", "serviceName", "serviceNameID"], lines))
        }
        "GetStationServiceAccessRule" | "GetStationManagementServiceCostModifiers" => management::read(ctx,station,method,args).await,
        "GetStationDetails" => management::details(ctx,station,args).await,
        "GetRentableItems" => management::rentables(ctx,station,args).await,
        "GetOwnerIDsOfClonesAtStation" => management::read(ctx,station,method,args).await,
        "RevokeCloneContractsAtStation" => {
            if args.len()!=2 || !arg(0).is_some_and(|n|(1..=i32::MAX as i64).contains(&n))
                || !arg(1).is_some_and(|n|n==-1 || (1..=i32::MAX as i64).contains(&n)) {
                return Some(Err("Choose a valid corporation and clone owner, or -1 for every owner in that corporation.".into()));
            }
            let (corp,char_id)=(arg(0).unwrap_or(-1),arg(1).unwrap_or(-1));
            management::revoke_clones(ctx,station,corp,char_id).map(|gone|{info!(station,gone,"station clone contracts revoked");Value::None})
        }
        "UpdateStationManagementSettings" => update(ctx,station,args).await.map(|_|Value::None),
        _ => return None,
    })
}

/// Source-shaped settings are validated and committed atomically.
async fn update(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<()> {
    management::update(ctx,station,args).await
}

/// Whether a station's owners let a pilot use a service (docking included),
/// by the rules they set: standing towards them and security status.
pub fn may_use(db: &Db, station: i64, service: i64, char_id: i64) -> bool {
    match may_use_checked(db,station,service,char_id) {
        Ok(allowed)=>allowed,
        Err(error)=>{error!("checking station {station} service access: {}",crate::logging::chain(&error));false}
    }
}

pub fn may_use_checked(db:&Db,station:i64,service:i64,char_id:i64)->Result<bool,StoreError> {
    let Some(owner)=db.table("staStations").get_id(station).and_then(|s|s.int("corporationID")) else { return Ok(false) };
    let Some(character)=db.table("character_").get_id(char_id) else { return Ok(false) };
    let corporation=character.int("corporationID");
    if corporation==Some(owner) {
        return Ok(true);
    }
    let rule=db.table("stationServiceRules").get(&[int(station),int(service)]);
    let rule_value=|column:&str,default:f64|rule.and_then(|r|r.float(column)).unwrap_or(default);
    let (min_standing, min_sec, max_sec) = (rule_value("minimumStanding",-10.0), rule_value("minimumCharSecurity",-10.0), rule_value("maximumCharSecurity",10.0));
    // The lower of the owners' standings towards the pilot and the pilot's
    // corporation; none set is 0.
    let standings=db.table("ownerStandings");
    let standing=[Some(char_id),corporation].into_iter().flatten()
        .filter_map(|to|standings.get(&[int(owner),int(to)])).filter_map(|o|o.float("standing")).min_by(f64::total_cmp).unwrap_or(0.0);
    let sec = character.float("securityRating").unwrap_or(0.0);
    Ok([standing,sec,min_standing,min_sec,max_sec].into_iter().all(f64::is_finite)
        && standing >= min_standing && sec >= min_sec && sec <= max_sec)
}

/// The store runs one writer at a time, so a standing or rule cannot change
/// while a check runs: nothing guards the station's standings any more.
pub fn ensure_access_locks(_tx:&mut Tx<'_>)->Result<(),StoreError>{
    Ok(())
}

/// As [`may_use_checked`], inside the caller's transaction (an arrival
/// deciding whether the pilot may dock): what it reads cannot change before
/// that transaction commits.
pub fn may_use_locked(db:&Db,station:i64,service:i64,char_id:i64)->Result<bool,StoreError> {
    may_use_checked(db,station,service,char_id)
}
