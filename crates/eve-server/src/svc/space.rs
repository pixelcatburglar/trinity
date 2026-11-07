//! `beyonce`, the remote ballpark: what the client sees in space and how its
//! ship moves (michelle.py, docs: destiny/ballpark/michelle).
//!
//! Every solar system with a ship in it has a [`Park`] in the shared world. A
//! movement command updates the park and goes out as a destiny opcode to every
//! client in the system, which then runs the same motion locally. A client that
//! (re)builds its ballpark gets a SetState of the whole park.

use std::collections::HashSet;

use eve_marshal::Value;
use tracing::{debug, warn};

use crate::ballpark::{Celestial, Command, Park, Ship, Vec3};
use crate::destiny::{self, flags, mode, Ball};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db};
use crate::svc::rows::{key_val, rowset};
use crate::world::WorldState;

const TYPE_SOLAR_SYSTEM: i64 = 5;
const OWNER_SYSTEM: i64 = 1;
const GROUP_STARGATE: i64 = 10;
const GROUP_STATION: i64 = 15;
/// Sun, planet, moon, asteroid belt, stargate, station.
const CELESTIAL_GROUPS: [i32; 6] = [6, 7, 8, 9, 10, 15];
/// WarpToStuff without minRange (menusvc's default warp-to distance).
const DEFAULT_WARP_RANGE: f64 = 15_000.0;
/// Trinity const.minWarpDistance / const.maxWarpEndDistance.
const MIN_WARP_DISTANCE: f64 = 150_000.0;
const MAX_WARP_RANGE: f64 = 100_000.0;
/// The menu's "approach" with no range (menusvc.py:1598 `range or 50`).
const DEFAULT_FOLLOW_RANGE: f64 = 50.0;
/// How far from their own ship a pilot sees other ships: their grid. The
/// client's ballpark (and so its overview) holds only these. A ship comes into
/// view inside GRID_RANGE and leaves it beyond GRID_RANGE_OUT, so one on the
/// edge does not flicker in and out.
pub const GRID_RANGE: f64 = 300_000.0;
const GRID_RANGE_OUT: f64 = 330_000.0;

/// `ship`: Undock is served by `conn` (it changes the session); nothing else
/// on the ship object is implemented yet.
/// The `ship` service. Undock and Board change the session, so the connection
/// handles them.
pub async fn ship(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if let Some(result) = crate::svc::maintenance::handle(call, ctx).await { return Some(result); }
    match call.method.as_str() {
        "AssembleShip" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids = ids.iter().filter_map(|v| v.as_i64()).collect();
            Some(crate::svc::moves::assemble_ship(ctx, ids).await)
        }
        // Drop(itemIDs, ownerID=None, ignoreWarning=False): launch structures or
        // drones, one per ID listed (a stack of five is listed five times).
        "Drop" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids: Vec<i64> = ids.iter().filter_map(|v| v.as_i64()).collect();
            let owner = call.args.get(1).and_then(|v| v.as_i64());
            if let Some(outcome) = crate::svc::starbase::launch(ctx, &ids, owner).await {
                return Some(outcome);
            }
            Some(crate::drones::launch(ctx, ids).await)
        }
        // Scoop(itemID, password=None): a structure floating free.
        "Scoop" => crate::svc::starbase::scoop(ctx, call.arg_id(0)).await,
        "ScoopDrone" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids: Vec<i64> = ids.iter().filter_map(|v| v.as_i64()).collect();
            Some(crate::drones::scoop(ctx, ids).await)
        }
        _ => None,
    }
}

pub async fn beyonce(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // LoadFormations accepts a tuple; no formations exist.
        "GetFormations" => {
            push_set_state(ctx).await;
            Value::Tuple(Vec::new())
        }
        // michelle.RequestReset: the client lost sync and wants the world again.
        "UpdateStateRequest" => {
            push_set_state(ctx).await;
            Value::None
        }
        "GetGangState" => key_val(vec![("targetTags", Value::Dict(Vec::new()))]),
        // This node admits jumps immediately; there are no queued systems.
        "GetJumpQueueStatus" => Value::Dict(Vec::new()),
        "GetGangLocationAndInfo" => crate::svc::fleet::location_info(&ctx.world.lock(), ctx.session.char_id.unwrap_or(0)),
        "GangRegroup" => return Some(crate::svc::fleet::regroup(&mut ctx.world.lock(), ctx.session.char_id.unwrap_or(0))
            .map_or_else(|e| e, Outcome::Value)),
        "ConeScan" | "GetScans" | "CombineScans" | "GiveProbe" | "DestroyProbe" => return crate::svc::scan::beyonce(call, ctx).await,
        // GotoBookmark(bookmarkID): the address book's Approach (addressbook.py:1087-1091).
        "GotoBookmark" => return Some(goto_bookmark(ctx, call.arg_id(0)).await),
        // BridgeToMember(charID, beaconID, solarsystemID): a titan's portal.
        "BridgeToMember" => {
            return Some(match crate::svc::jumps::open_bridge(ctx, call.arg_id(0), call.arg_id(1), call.arg_id(2)).await {
                Ok(()) => Outcome::Value(Value::None),
                Err(why) => Outcome::custom(why),
            })
        }
        "BridgeToStructure" => {
            return Some(match crate::svc::jumps::open_bridge_structure(ctx, call.arg_id(0), call.arg_id(1)).await {
                Ok(()) => Outcome::Value(Value::None),
                Err(why) => Outcome::custom(why),
            })
        }
        "GetRelativity" => relativity(ctx, call),
        method if is_movement(method) => return Some(movement(ctx, call).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub fn is_movement(method: &str) -> bool {
    matches!(
        method,
        "SetSpeedFraction" | "GotoDirection" | "GotoPoint" | "Stop" | "FollowBall" | "Orbit" | "WarpToStuff" | "WarpToTutorial"
    )
}

// ---- the park ---------------------------------------------------------------

/// Makes sure `solar_system_id` has a park, loading its celestials if not.
pub async fn ensure_park(ctx: &Ctx<'_>, solar_system_id: i64) {
    if ctx.world.lock().parks.contains_key(&solar_system_id) {
        return;
    }
    let celestials = ctx.store.read(|db| load_celestials(db, solar_system_id));
    let launched_drones = crate::drones::load(ctx, solar_system_id).await.unwrap_or_else(|e| {
        warn!("drones in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
        Vec::new()
    });
    let empty_ships = super::embark::load_empty(ctx, solar_system_id).await.unwrap_or_else(|e| {
        warn!("empty ships in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
        Vec::new()
    });
    let now = crate::combat::now_ms();
    let (cans, structures, wildlife, complexes, faction) = ctx.store.read(|db| {
        if let Err(e) = crate::loot::ensure_table(db) {
            warn!("no loot table: {}", crate::logging::chain(&e));
        }
        let cans = crate::loot::load_cans(db, solar_system_id, now).unwrap_or_else(|e| {
            warn!("cans in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
            Vec::new()
        });
        let structures = crate::pos::load(db, solar_system_id).unwrap_or_else(|e| {
            warn!("structures in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
            Vec::new()
        });
        let wildlife = match crate::npc::load(db, solar_system_id) {
            Ok(w) => w,
            Err(e) => {
                warn!("system {solar_system_id} gets no belt life: {}", crate::logging::chain(&e));
                Default::default()
            }
        };
        let complexes = crate::complexes::load(db, solar_system_id);
        let faction = db
            .table("corporation")
            .get_id(wildlife.corp_id)
            .and_then(|r| r.text("corporationName"))
            .map(|n| n.split_whitespace().next().unwrap_or("Pirate").trim_end_matches("'s").to_string())
            .unwrap_or_else(|| "Pirate".into());
        (cans, structures, wildlife, complexes, faction)
    });
    let created = {
    let mut world = ctx.world.lock();
    if let std::collections::hash_map::Entry::Vacant(entry) = world.parks.entry(solar_system_id) {
        let mut park = Park::new(solar_system_id, celestials, destiny::tick_now());
        park.wildlife = std::sync::Arc::new(wildlife);
        crate::npc::populate(&mut park);
        park.ships.extend(empty_ships.into_iter().map(|ship| (ship.id, ship)));
        park.cans = cans.into_iter().map(|c| (c.id, c)).collect();
        park.structures = structures.into_iter().map(|s| (s.id, s)).collect();
        crate::complexes::place(&mut park, complexes, &faction);
        entry.insert(park);
        true
    } else {false}
    };
    if created {
        if let Err(e)=crate::drones::install(ctx,solar_system_id,launched_drones).await {
            warn!("restoring drones in {solar_system_id}: {}",crate::logging::chain(&e));
        }
    }
}

fn load_celestials(db: &Db, solar_system_id: i64) -> Vec<Celestial> {
    let map = db.table("mapDenormalize");
    let mut rows: Vec<_> = map
        .find(&["solarSystemID"], &[store::Value::Int(solar_system_id)])
        .filter(|d| d.int("groupID").is_some_and(|g| CELESTIAL_GROUPS.contains(&(g as i32))))
        .collect();
    rows.sort_by_key(|d| d.int("itemID"));
    // Where each stargate leads (mapJumps), and the system of the gate at
    // the other end.
    let jumps = crate::sde::table("mapJumps");
    rows.iter()
        .map(|r| {
            let gate = r.int("itemID").and_then(|id| jumps.get("stargateID", id)?.int("celestialID"));
            Celestial {
                id: r.int("itemID").unwrap_or(0),
                type_id: r.int("typeID").unwrap_or(0),
                group_id: r.int("groupID").unwrap_or(0),
                name: r.text("itemName").unwrap_or_default().to_string(),
                pos: Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0)),
                radius: r.float("radius").unwrap_or(0.0),
                jump: gate.and_then(|gate| Some((map.get_id(gate)?.int("solarSystemID")?, gate))),
            }
        })
        .collect()
}

// ---- what the client is sent --------------------------------------------------

pub fn entry(tick: i64, opcode: &str, args: Vec<Value>) -> Value {
    Value::Tuple(vec![
        Value::Int(destiny::stamp(tick) as i64),
        Value::Tuple(vec![Value::bytes(opcode), Value::Tuple(args)]),
    ])
}

/// DoDestinyUpdate(state, waitForBubble=0) to every client in the system, or
/// only to `only`.
pub fn broadcast(world: &WorldState, solar_system_id: i64, only: Option<i64>, entries: Vec<Value>) {
    if entries.is_empty() {
        return;
    }
    let targets = match only {
        Some(c) => vec![c],
        None => world.in_space(solar_system_id),
    };
    send(world, solar_system_id, &targets, &entries);
}

/// DoDestinyUpdate to each of `clients`, less what concerns ships off its grid.
pub fn send(world: &WorldState, solar_system_id: i64, clients: &[i64], entries: &[Value]) {
    if entries.is_empty() {
        return;
    }
    let park = world.parks.get(&solar_system_id);
    for &client in clients {
        let ego = park.and_then(|p| ego_of(p, client));
        let Some(mine) = world.with_hidden(client, |hidden| {
            entries.iter().filter_map(|e| sift(e, park, ego, hidden)).collect::<Vec<_>>()
        }) else {
            continue;
        };
        if !mine.is_empty() {
            let (event,args)=destiny_notification(mine);
            world.notify(&[client],event,args);
        }
    }
}

/// michelle.DoDestinyUpdate checks that every action has the same stamp.
/// Keep causal order while splitting at stamp boundaries; the plural envelope
/// is explicitly supported by DoDestinyUpdates for these ordered sub-batches.
pub(crate) fn destiny_notification(entries:Vec<Value>) -> (&'static str,Value) {
    let mut batches:Vec<Vec<Value>>=Vec::new();
    let mut previous:Option<Value>=None;
    for entry in entries {
        let stamp=entry.seq().and_then(|e|e.first()).cloned();
        if batches.is_empty() || stamp!=previous { batches.push(Vec::new()); previous=stamp; }
        batches.last_mut().unwrap().push(entry);
    }
    let envelope=|batch|Value::Tuple(vec![Value::List(batch),Value::Int(0)]);
    if batches.len()==1 {
        ("DoDestinyUpdate",envelope(batches.pop().unwrap()))
    } else {
        ("DoDestinyUpdates",Value::Tuple(vec![Value::List(batches.into_iter().map(envelope).collect())]))
    }
}

/// The ship `client_id` flies in `park`.
fn ego_of(park: &Park, client_id: i64) -> Option<&Ship> {
    park.ships.values().find(|s| s.client_id == client_id && s.is_player())
}

/// Whether `ship` is on `ego`'s grid; `shown` if the client has it already.
fn on_grid(ego: &Ship, ship: &Ship, shown: bool) -> bool {
    let reach = if shown { GRID_RANGE_OUT } else { GRID_RANGE };
    ship.id == ego.id || (ship.pos - ego.pos).len_sq() <= reach * reach
}

/// The ships a SetState for `ego` leaves out: those off its grid, and other
/// pilots' cloaked ones.
pub fn hidden_from(park: &Park, ego: i64) -> HashSet<i64> {
    let Some(me) = park.ships.get(&ego) else { return HashSet::new() };
    park.ships
        .values()
        .filter(|s| s.id != ego && (s.cloaked || !on_grid(me, s, false)))
        .map(|s| s.id)
        .collect()
}

/// A destiny entry's opcode and arguments.
fn parts(e: &Value) -> Option<(&[u8], &[Value])> {
    let call = e.seq()?.get(1)?.seq()?;
    Some((call.first()?.as_bytes()?, call.get(1)?.seq()?))
}

/// A slim item's itemID.
fn slim_id(slim: &Value) -> Option<i64> {
    match slim {
        Value::Instance { state, .. } => state.get_str("itemID")?.as_i64(),
        _ => None,
    }
}

fn references_hidden(opcode:&[u8],args:&[Value],hidden:&HashSet<i64>) -> bool {
    let hidden_arg=|i|args.get(i).and_then(Value::as_i64).is_some_and(|id|hidden.contains(&id));
    hidden_arg(0) || match opcode {
        b"OnSpecialFX"=>hidden_arg(3),
        b"FollowBall" | b"Orbit"=>hidden_arg(1),
        _=>false,
    }
}

fn course_target(ship:&Ship) -> Option<i64> {
    match ship.command {Command::Follow {target,..}|Command::Orbit {target,..}=>Some(target),_=>None}
}

/// Extrapolate the visible ship's current motion without disclosing or
/// referencing a target ball that the observer does not have.
fn untargeted_course(ship:&Ship,tick:i64) -> Value {
    let direction=ship.vel.unit();
    if direction==Vec3::ZERO {entry(tick,"Stop",vec![Value::Int(ship.id)])}
    else {entry(tick,"GotoDirection",vec![Value::Int(ship.id),Value::Double(direction.x),Value::Double(direction.y),Value::Double(direction.z)])}
}

fn course_for_view(ship:&Ship,tick:i64,hidden:&HashSet<i64>) -> Vec<Value> {
    if course_target(ship).is_some_and(|id|hidden.contains(&id)) {vec![untargeted_course(ship,tick)]}
    else {snapshot_course_entries(ship,tick)}
}

/// One destiny entry as a client with `hidden` ships is to get it, if at all.
/// An AddBalls for a single ship decides whether the client has it; a
/// RemoveBalls loses the ships it never had; anything else about a hidden ship
/// is dropped.
fn sift(e: &Value, park: Option<&Park>, ego: Option<&Ship>, hidden: &mut HashSet<i64>) -> Option<Value> {
    let Some((opcode, args)) = parts(e) else { return Some(e.clone()) };
    match opcode {
        b"SetState" => Some(e.clone()),
        b"AddBalls" => {
            let slims = args.first().and_then(Value::seq).and_then(|c| c.get(1)).and_then(Value::seq).unwrap_or(&[]);
            let ship = match slims {
                [one] => slim_id(one).and_then(|id| park?.ships.get(&id)),
                _ => None,
            };
            match (ship, ego) {
                (Some(s), Some(me)) if s.id != me.id => {
                    if !s.cloaked && on_grid(me, s, false) {
                        hidden.remove(&s.id);
                        Some(e.clone())
                    } else {
                        hidden.insert(s.id);
                        None
                    }
                }
                _ => Some(e.clone()),
            }
        }
        b"RemoveBalls" => {
            let Some(ids) = args.first().and_then(Value::seq) else { return Some(e.clone()) };
            let kept: Vec<Value> =
                ids.iter().filter(|v| !v.as_i64().is_some_and(|id| hidden.remove(&id))).cloned().collect();
            if kept.is_empty() {
                None
            } else if kept.len() == ids.len() {
                Some(e.clone())
            } else {
                let stamp = e.seq().and_then(|t| t.first()).cloned().unwrap_or(Value::Int(0));
                Some(Value::Tuple(vec![
                    stamp,
                    Value::Tuple(vec![Value::bytes("RemoveBalls"), Value::Tuple(vec![Value::List(kept)])]),
                ]))
            }
        }
        b"FollowBall" | b"Orbit" if !args.first().and_then(Value::as_i64).is_some_and(|id|hidden.contains(&id))
            && args.get(1).and_then(Value::as_i64).is_some_and(|id|hidden.contains(&id)) => {
            let ship=park?.ships.get(&args.first()?.as_i64()?)?;
            let tick=e.seq()?.first()?.as_i64()?;
            Some(untargeted_course(ship,tick))
        }
        _ if references_hidden(opcode,args,hidden) => None,
        _ => Some(e.clone()),
    }
}

/// Brings every in-space client's view up to date with where the ships are:
/// AddBalls for those that came onto its grid, RemoveBalls for those that
/// left it. Cloaked ships are `cloak`'s business.
pub fn refresh_views(world: &WorldState) {
    for park in world.parks.values() {
        if park.players().next().is_none() {
            continue;
        }
        for client in world.in_space(park.solar_system_id) {
            let Some(me) = ego_of(park, client) else { continue };
            let entries=world.with_hidden(client,|hidden|refresh_view_entries(park,me,hidden)).unwrap_or_default();
            if !entries.is_empty() {
                let (event,args)=destiny_notification(entries);
                world.notify(&[client],event,args);
            }
        }
    }
}

fn refresh_view_entries(park:&Park,me:&Ship,hidden:&mut HashSet<i64>) -> Vec<Value> {
    let mut gone=Vec::new();
    let mut arrived=Vec::new();
    hidden.retain(|id|park.ships.contains_key(id));
    for ship in park.ships.values().filter(|s|s.id!=me.id && !s.cloaked) {
        let shown=!hidden.contains(&ship.id);
        match (shown,on_grid(me,ship,shown)) {
            (false,true)=>{hidden.remove(&ship.id);arrived.push(ship.id);}
            (true,false)=>{hidden.insert(ship.id);gone.push(Value::Int(ship.id));}
            _=>{}
        }
    }
    let mut entries=Vec::new();
    if !gone.is_empty() {entries.push(entry(park.tick,"RemoveBalls",vec![Value::List(gone)]));}
    entries.extend(add_many_ball_entries(park,&arrived).into_iter().filter_map(|e|sift(&e,Some(park),Some(me),hidden)));
    // A follower can remain visible while its target leaves or returns.
    // Restore it after all incoming targets have been added, even though the
    // follower itself did not cross the grid boundary.
    for ship in park.ships.values().filter(|s|!hidden.contains(&s.id) && !arrived.contains(&s.id)) {
        if course_target(ship).is_some_and(|id|hidden.contains(&id) || arrived.contains(&id)) {
            entries.extend(course_for_view(ship,park.tick,hidden));
        }
    }
    entries
}

fn ship_ball(ship: &Ship) -> Ball {
    let warp = ship.warp_snapshot();
    Ball {
        id: ship.id,
        mode: if warp.is_some() { mode::WARP } else { mode::STOP },
        warp,
        radius: ship.stats.radius,
        pos: ship.pos.tuple(),
        flags: if ship.massive() { flags::SHIP } else { flags::SHIP_IN_WARP },
        mass: ship.stats.mass,
        corp_id: ship.pilot.corp_id as i32,
        alliance_id: ship.pilot.alliance_id.unwrap_or(-1) as i32,
        harmonic: ship.harmonic,
        max_velocity: ship.stats.max_velocity,
        velocity: ship.vel.tuple(),
        agility: ship.stats.agility,
        speed_fraction: ship.speed_fraction,
        name: ship.pilot.name.clone(),
        cloaked: ship.cloaked,
    }
}

pub fn slim(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("foo.SlimItem")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// groupID/categoryID are left out on purpose: the client derives them from typeID.
pub(crate) fn ship_slim(ship: &Ship) -> Value {
    let p = &ship.pilot;
    // An NPC belongs to its corporation and has no pilot to show.
    if ship.is_npc() {
        return slim(vec![
            ("itemID", Value::Int(ship.id)),
            ("typeID", Value::Int(p.type_id)),
            ("ownerID", Value::Int(p.corp_id)),
            ("corpID", Value::Int(p.corp_id)),
        ]);
    }
    // A drone belongs to its pilot but is not flown by one.
    if ship.drone.is_some() {
        return slim(vec![
            ("itemID", Value::Int(ship.id)),
            ("typeID", Value::Int(p.type_id)),
            ("ownerID", Value::Int(p.char_id)),
            ("corpID", Value::Int(p.corp_id)),
        ]);
    }
    slim(vec![
        ("itemID", Value::Int(ship.id)),
        ("typeID", Value::Int(p.type_id)),
        ("ownerID", Value::Int(p.char_id)),
        // uix.GetSlimItemName and michelle.AddBalls distinguish an empty hull
        // by charID=None; ownership survives the pilot leaving it.
        ("charID", if ship.unpiloted { Value::None } else { Value::Int(p.char_id) }),
        ("corpID", Value::Int(p.corp_id)),
        ("allianceID", Value::int_or_none(p.alliance_id)),
        // consider.py rounds it; it must be a number.
        ("securityStatus", Value::Double(p.security)),
        ("bounty", Value::Double(p.bounty)),
    ])
}

fn celestial_slim(c: &Celestial, owners: &[(i64, i64)]) -> Value {
    let owner = owners.iter().find(|(id, _)| *id == c.id).map_or(OWNER_SYSTEM, |(_, o)| *o);
    let mut fields = vec![
        ("itemID", Value::Int(c.id)),
        ("typeID", Value::Int(c.type_id)),
        ("ownerID", Value::Int(owner)),
    ];
    if let Some((system, gate)) = c.jump {
        let jump = crate::svc::rows::row(
            &["locationID", "toCelestialID"],
            vec![Value::Int(system), Value::Int(gate)],
        );
        fields.push(("jumps", Value::List(vec![jump])));
    }
    slim(fields)
}

/// [shield (fraction, tau), armor, structure], as the ship is now.
fn health(ship: &Ship) -> Value {
    match &ship.fight {
        Some(f) => crate::fight::damage_state(&f.vitals, crate::combat::now_ms()),
        None => Value::List(vec![
            Value::Tuple(vec![Value::Double(1.0), Value::Double(1.0)]),
            Value::Double(1.0),
            Value::Double(1.0),
        ]),
    }
}

/// The destiny opcode for a ship's current command.
pub fn command_entry(ship: &Ship, tick: i64) -> Value {
    let id = Value::Int(ship.id);
    let point = |p: &Vec3| vec![id.clone(), Value::Double(p.x), Value::Double(p.y), Value::Double(p.z)];
    match &ship.command {
        Command::Stop => entry(tick, "Stop", vec![id]),
        Command::Direction(d) => entry(tick, "GotoDirection", point(d)),
        Command::Point(p) => entry(tick, "GotoPoint", point(p)),
        Command::Follow { target, range } => {
            entry(tick, "FollowBall", vec![id, Value::Int(*target), Value::Double(*range)])
        }
        Command::Orbit { target, range } => {
            entry(tick, "Orbit", vec![id, Value::Int(*target), Value::Double(*range)])
        }
        // WarpTo(ballID, x, y, z, range, warpFactor): without the factor
        // destiny warps everyone at 2 AU/s.
        Command::Warp { target, range } => {
            let mut args = point(target);
            args.push(Value::Double(*range));
            args.push(Value::Int(ship.stats.warp_factor));
            entry(tick, "WarpTo", args)
        }
    }
}

/// effects.Warping for a ship that has just been given a warp: after WarpTo,
/// so destiny already has the ball in warp mode. The client's effect draws the
/// tunnel and the "Warp Drive Active" indication for its own ship, and stops
/// itself once the ball leaves warp (warp.py ShakeCameraAndDoStuff).
pub fn warp_fx(ship: &Ship, tick: i64) -> Option<Value> {
    ship.warping().then(|| special_fx(tick, ship.id, None, "effects.Warping"))
}

/// The opcodes that put a ship back on its current course, for a client that
/// has just been given the ship standing still.
pub fn course_entries(ship: &Ship, tick: i64) -> Vec<Value> {
    if ship.command == Command::Stop {
        return Vec::new();
    }
    let speed = entry(tick, "SetSpeedFraction", vec![Value::Int(ship.id), Value::Double(ship.speed_fraction)]);
    vec![command_entry(ship, tick), speed]
}

/// WARP snapshots already restore the entire native course, including its
/// entry tick. WarpTo here would overwrite that tick with the alignment
/// sentinel -1 and restart a ship that is already in flight.
fn snapshot_course_entries(ship: &Ship, tick: i64) -> Vec<Value> {
    if ship.warp_snapshot().is_some() { Vec::new() } else { course_entries(ship, tick) }
}

/// SetState for `ego`: the whole park, then every moving ship's course.
pub fn set_state_entries(
    park: &Park,
    ego: i64,
    hidden: &HashSet<i64>,
    station_owners: &[(i64, i64)],
    char_gangs: &[(i64, i64)],
    alliance_bridges: &[(i64, i64, i64)],
) -> Vec<Value> {
    const DRONE_HEADER: &[&str] =
        &["droneID", "ownerID", "controllerID", "activityState", "typeID", "controllerOwnerID", "targetID"];
    let mut balls: Vec<Ball> = park
        .celestials
        .iter()
        .map(|c| Ball::celestial(c.id, c.radius, c.pos.tuple(), &c.name, c.massive()))
        .collect();
    let mut slims: Vec<Value> = park.celestials.iter().map(|c| celestial_slim(c, station_owners)).collect();
    for rock in park.rocks.values() {
        balls.push(Ball::celestial(rock.id, rock.radius, rock.pos.tuple(), &rock.name, true));
        slims.push(crate::npc::rock_slim(rock));
    }
    for can in park.cans.values() {
        balls.push(Ball::celestial(can.id, can.radius, can.pos.tuple(), &can.name, false));
        slims.push(crate::loot::slim(can));
    }
    for site in park.sites.values() {
        balls.push(site.ball());
        slims.push(site.slim());
    }
    for b in park.beacons.values().filter(|b| !b.hidden) {
        balls.push(b.ball());
        slims.push(b.slim());
    }
    let mut damage = Vec::new();
    let now_ms = crate::combat::now_ms();
    for s in park.structures.values() {
        balls.push(s.ball());
        slims.push(s.slim());
        damage.push((Value::Int(s.id), crate::fight::damage_state(&s.vitals, now_ms)));
        if let Some(f) = &s.field {
            balls.push(f.ball(s));
            slims.push(f.slim(s));
        }
    }
    // Cloaked ships are seen only by their pilots; ships off the grid not at all.
    for ship in park.ships.values().filter(|s| (!s.cloaked || s.id == ego) && !hidden.contains(&s.id)) {
        balls.push(ship_ball(ship));
        slims.push(ship_slim(ship));
        damage.push((Value::Int(ship.id), health(ship)));
    }
    let bag = key_val(vec![
        ("state", Value::Bytes(destiny::encode(destiny::stamp(park.tick), &balls))),
        ("ego", Value::Int(ego)),
        ("slims", Value::List(slims)),
        (
            "solItem",
            slim(vec![
                ("itemID", Value::Int(park.solar_system_id)),
                ("typeID", Value::Int(TYPE_SOLAR_SYSTEM)),
                ("ownerID", Value::Int(OWNER_SYSTEM)),
            ]),
        ),
        ("aggressors", park.watch.aggressors()),
        ("droneState", rowset(DRONE_HEADER, crate::drones::state_lines(park))),
        ("gangCorps", Value::List(Vec::new())),
        (
            "charGangs",
            Value::List(char_gangs.iter().map(|(c, g)| Value::Tuple(vec![Value::Int(*c), Value::Int(*g)])).collect()),
        ),
        ("damageState", Value::Dict(damage)),
        ("effectStates", Value::List(park.ships.values()
            .filter(|s|(!s.cloaked || s.id==ego) && !hidden.contains(&s.id))
            .flat_map(|s|module_effect_entries(s,park.tick))
            .filter_map(|e| {
                let (op,args)=parts(&e)?;
                (!references_hidden(op,args,hidden)).then(||Value::Tuple(args.to_vec()))
            }).collect())),
        // michelle.py restores these when entering a park or resetting it.
        ("allianceBridges", Value::List(alliance_bridges.iter().map(|(ship,system,beacon)|
            Value::Tuple(vec![Value::Int(*ship),Value::Int(*system),Value::Int(*beacon)])).collect())),
    ]);
    let mut entries = vec![entry(park.tick, "SetState", vec![bag])];
    for ship in park.ships.values().filter(|s| (!s.cloaked || s.id==ego) && !hidden.contains(&s.id)) {
        entries.extend(course_for_view(ship,park.tick,hidden));
    }
    // Reset mid-warp: the pilot's tunnel goes on.
    entries.extend(park.ships.get(&ego).and_then(|s| warp_fx(s, park.tick)));
    entries
}

/// AddBalls for one ship joining the park, then its course.
pub fn add_ball_entries(park: &Park, ship_id: i64) -> Vec<Value> {
    let Some(ship) = park.ships.get(&ship_id) else { return Vec::new() };
    let blob = destiny::encode(destiny::stamp(park.tick), &[ship_ball(ship)]);
    let chunk = Value::Tuple(vec![
        Value::Bytes(blob),
        Value::List(vec![ship_slim(ship)]),
        Value::Dict(vec![(Value::Int(ship.id), health(ship))]),
    ]);
    let mut entries = vec![entry(park.tick, "AddBalls", vec![chunk])];
    entries.extend(snapshot_course_entries(ship, park.tick));
    entries.extend(module_effect_entries(ship,park.tick));
    entries.extend(warp_fx(ship, park.tick));
    entries
}

/// Reuse the ordinary module effect wire arguments so a new observer or a
/// reset restores the original target and cycle start, not a fresh cycle.
fn module_effect_entries(ship:&Ship,tick:i64) -> Vec<Value> {
    ship.fight.iter().flat_map(|f|&f.modules).filter(|m|!m.guid.is_empty())
        .map(|m|crate::fight::module_fx(tick,ship.id,m,true)).collect()
}

/// Add every newly visible ball before restoring any course: FollowBall and
/// Orbit may refer to another ship that entered the grid in the same pulse.
fn add_many_ball_entries(park:&Park,ships:&[i64]) -> Vec<Value> {
    let mut additions=Vec::new();
    let mut updates=Vec::new();
    for id in ships {
        let mut entries=add_ball_entries(park,*id).into_iter();
        additions.extend(entries.next());
        updates.extend(entries);
    }
    additions.extend(updates);
    additions
}

pub fn remove_ball_entries(tick: i64, ship_id: i64) -> Vec<Value> {
    vec![entry(tick, "RemoveBalls", vec![Value::List(vec![Value::Int(ship_id)])])]
}

/// OnSpecialFX(shipID, moduleID, moduleTypeID, targetID, otherTypeID, area, guid,
/// isOffensive, start, active) as a destiny entry (spaceMgr.py:120).
pub fn special_fx(tick: i64, ship_id: i64, target_id: Option<i64>, guid: &str) -> Value {
    entry(
        tick,
        "OnSpecialFX",
        vec![
            Value::Int(ship_id),
            Value::None,
            Value::None,
            Value::int_or_none(target_id),
            Value::None,
            Value::List(Vec::new()),
            Value::bytes(guid),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
        ],
    )
}

/// Stations' owning corporations, for their slims.
pub async fn station_owners(ctx: &Ctx<'_>, solar_system_id: i64) -> Vec<(i64, i64)> {
    ctx.store.read(|db| owners_in(db, solar_system_id))
}

fn owners_in(db: &Db, solar_system_id: i64) -> Vec<(i64, i64)> {
    db.table("staStations")
        .find(&["solarSystemID"], &[store::Value::Int(solar_system_id)])
        .filter_map(|r| Some((r.int("stationID")?, r.int("corporationID")?)))
        .collect()
}

/// Puts a ship into its system's park and shows it to everyone else there.
pub fn add_ship(world: &mut WorldState, system: i64, ship: Ship) {
    let ship_id = ship.id;
    let owner = ship.client_id;
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    park.ships.insert(ship_id, ship);
    let entries = add_ball_entries(park, ship_id);
    let others: Vec<i64> = world.in_space(system).into_iter().filter(|c| *c != owner).collect();
    send(world, system, &others, &entries);
}

/// Gives a ship in a park a new course and tells the system (a warp opens
/// the throttle first, as destiny wants).
pub fn order(world: &mut WorldState, system: i64, ship_id: i64, command: Command) {
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    if !park.ships.get(&ship_id).is_some_and(|ship| !ship.identity_reserved && !ship.in_warp()
        && crate::ballpark::valid_command(ship_id,ship.pos,&command)) {
        return;
    }
    let tick = park.tick;
    let mut entries = Vec::new();
    if matches!(command, Command::Warp { .. }) && park.ships[&ship_id].speed_fraction < 1.0 {
        park.set_speed_fraction(ship_id, 1.0);
        entries.push(entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(1.0)]));
    }
    park.command(ship_id, command);
    entries.insert(0, command_entry(&park.ships[&ship_id], tick));
    entries.extend(warp_fx(&park.ships[&ship_id], tick));
    broadcast(world, system, None, entries);
}

/// Takes a ship out of its system's park, out of everyone's view and out of
/// every fight it was in; returns it.
pub fn remove_ship(world: &mut WorldState, system: i64, ship_id: i64) -> Option<Ship> {
    world.maintenance.remove(&ship_id);
    let park = world.parks.get_mut(&system)?;
    park.advance(destiny::tick_now());
    let mut out = crate::heartbeat::Out::default();
    let now = crate::combat::now_ms();
    crate::fight::stand_down(park, ship_id, now, &mut out);
    let ship = crate::fight::detach(park, ship_id, now, &mut out)?;
    let entries = remove_ball_entries(park.tick, ship_id);
    let jobs=out.deliver(world);
    world.pending_jobs.extend(jobs);
    let others: Vec<i64> = world.in_space(system).into_iter().filter(|c| *c != ship.client_id).collect();
    send(world, system, &others, &entries);
    Some(ship)
}

/// Queues a SetState for the caller's own ship.
pub(crate) async fn push_set_state(ctx: &Ctx<'_>) {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return;
    };
    ensure_park(ctx, system).await;
    let owners = station_owners(ctx, system).await;
    let mut world = ctx.world.lock();
    crate::drones::reconcile(&mut world);
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    if !park.ships.contains_key(&ship_id) {
        warn!("SetState for ship {ship_id}, which is not in the park of {system}");
    }
    // Just out of a gate: the pilot sees the arrival too, once their park is up.
    let arrival = park.ships.get_mut(&ship_id).and_then(|s| s.arrived_through.take());
    let pilots: Vec<i64> = park.ships.values().map(|s| s.pilot.char_id).filter(|c| *c != 0).collect();
    let char_gangs = world.gangs.memberships(pilots);
    let Some(park) = world.parks.get(&system) else { return };
    let hidden = hidden_from(park, ship_id);
    let alliance_bridges: Vec<_> = world.bridges.iter().filter(|(id,b)| b.system==system && b.member.is_none()
        && b.until_ms>crate::combat::now_ms() && park.ships.contains_key(id) && !hidden.contains(id))
        .map(|(id,b)|(*id,b.to_system,b.beacon)).collect();
    let mut entries = set_state_entries(park, ship_id, &hidden, &owners, &char_gangs, &alliance_bridges);
    let shown = park.ships.len() - hidden.len();
    world.with_hidden(ctx.client_id, |h| *h = hidden);
    if let Some(gate) = arrival {
        entries.push(special_fx(park.tick, ship_id, Some(gate), "effects.JumpIn"));
    }
    debug!("SetState: {} ball(s), ego {ship_id}", park.celestials.len() + shown);
    broadcast(&world, system, Some(ctx.client_id), entries);
    world.sees(ctx.client_id, system);
}

/// A capacitor-limited warp can choose a shorter exit at entry. Rebuild the
/// native state for every client that already has this park, preserving each
/// observer's visibility and the original entry stamp/distance. Called only
/// after releasing the simulation lock.
pub fn resync_after_warp_limit(db: &Db, world: &crate::world::World, system: i64) {
    let owners = owners_in(db, system);
    resync_warp_views(&world.lock(),system,&owners);
}

fn resync_warp_views(world: &WorldState, system: i64, owners: &[(i64,i64)]) {
    let Some(park)=world.parks.get(&system) else { return };
    let char_gangs=world.gangs.memberships(park.ships.values().filter(|s|s.is_pilot()).map(|s|s.pilot.char_id));
    let now=crate::combat::now_ms();
    for client in world.in_space(system) {
        let Some(ego)=ego_of(park,client) else { continue };
        let hidden=hidden_from(park,ego.id);
        let bridges=world.bridges.iter().filter(|(id,b)|b.system==system && b.member.is_none()
            && b.until_ms>now && park.ships.contains_key(id) && !hidden.contains(id))
            .map(|(id,b)|(*id,b.to_system,b.beacon)).collect::<Vec<_>>();
        let entries=set_state_entries(park,ego.id,&hidden,owners,&char_gangs,&bridges);
        world.with_hidden(client,|h|*h=hidden);
        send(world,system,&[client],&entries);
    }
}

/// (diff, x, y, z, serverTick, tTime, rTime): where the server has the ship, and
/// how far that is from where the client has it (michelle.GetRelativity).
fn relativity(ctx: &Ctx<'_>, call: &Call) -> Value {
    let client_pos = Vec3::new(
        call.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
        call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
    );
    let tick = destiny::tick_now();
    let server_pos = ctx
        .session
        .solar_system_id
        .zip(ctx.session.ship_id)
        .and_then(|(system, ship)| {
            let mut world = ctx.world.lock();
            let park = world.parks.get_mut(&system)?;
            park.advance(tick);
            park.ships.get(&ship).map(|s| s.pos)
        })
        .unwrap_or(client_pos);
    Value::Tuple(vec![
        Value::Double((server_pos - client_pos).len()),
        Value::Double(server_pos.x),
        Value::Double(server_pos.y),
        Value::Double(server_pos.z),
        Value::Int(destiny::stamp(tick) as i64),
        Value::Long(crate::bulkdata::filetime_now()),
        Value::Long(crate::bulkdata::filetime_now()),
    ])
}

// ---- movement -------------------------------------------------------------------

enum Order {
    Command(Command),
    SpeedFraction(f64),
}

/// A movement call on the caller's ship: update the park, tell the system.
async fn movement(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    // The neocom's tutorial button calls WarpToTutorial() (neocom.py:1329-1333).
    if call.method == "WarpToTutorial" || (call.method == "WarpToStuff" && call.arg_text(0).as_deref() == Some("tutorial")) {
        return warp_to_tutorial(ctx, call, system, ship_id);
    }
    let Some(order) = parse_order(ctx, call).await else {
        return Outcome::custom("Invalid movement command or unavailable destination.");
    };
    if call.method == "WarpToStuff" {
        let fleet = match call.kwarg("gang") {
            None | Some(Value::Bool(false)) | Some(Value::Int(0)) => false,
            Some(Value::Bool(true)) | Some(Value::Int(1)) => true,
            _ => return Outcome::custom("Invalid fleet warp flag."),
        };
        if fleet { return gang_warp(ctx, system, ship_id, order, call); }
    }
    steer(ctx, system, ship_id, order, &call.method)
}

fn gang_warp(ctx: &Ctx<'_>, system: i64, ship_id: i64, requested: Order, call: &Call) -> Outcome {
    let Some(me) = ctx.session.char_id else { return Outcome::custom("No character selected."); };
    let Order::Command(Command::Warp { target, range }) = requested else { return Outcome::custom("Invalid fleet warp destination."); };
    let mut world = ctx.world.lock();
    let Some(gang) = world.gangs.of(me).and_then(|id|world.gangs.get(id)) else {
        return Outcome::custom("You are not in a fleet.");
    };
    let members = match &gang.fleet {
        Some(fleet) => match fleet.warp_members(me) { Ok(members) => members, Err(error) => return error },
        None if gang.leader == Some(me) => gang.members.clone(),
        None => return Outcome::custom("Only the gang leader can order a gang warp."),
    };
    let Some(park) = world.parks.get_mut(&system) else { return Outcome::custom("Your ship is not in space."); };
    park.advance(destiny::tick_now());
    let Some(leader) = park.ships.get(&ship_id).filter(|s|s.pilot.char_id==me && s.client_id==ctx.client_id && s.is_player()) else {
        return Outcome::UserError{msg:"ShipNotYours".into(),args:vec![]};
    };
    if leader.identity_reserved { return Outcome::custom("Your ship transfer is still in progress."); }
    if leader.in_warp() { return Outcome::Value(Value::None); }
    let end_range = call.kwarg("minRange").and_then(finite_number).unwrap_or(DEFAULT_WARP_RANGE);
    let radius = (range - leader.stats.radius - end_range).max(0.0);
    let courses:Vec<_> = park.ships.values().filter(|s| s.is_pilot() && members.contains(&s.pilot.char_id)
        && on_grid(leader,s,false) && !s.identity_reserved && !s.in_warp()
        && !s.fight.as_ref().is_some_and(|f|f.afflictions.scrambled>=1.0)
        && (!s.cloaked || crate::cloak::warp_safe(park,s.id)!=Some(false)))
        .filter_map(|ship|course_to(ship,target,radius,end_range).map(|course|(ship.id,course))).collect();
    if courses.is_empty() { return Outcome::custom("No fleet ships here can warp to that destination."); }
    for (id, course) in courses { order(&mut world,system,id,course); }
    Outcome::Value(Value::None)
}

/// Warps the caller's ship to `range` off something at `at` of `radius`
/// (an agent's location, say).
pub fn warp_to_point(ctx: &Ctx<'_>, at: Vec3, radius: f64, range: f64, why: &str) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    let Some(command) = warp_course(ctx, at, radius, range) else {
        return Outcome::custom("The destination is unavailable or too close to warp to.");
    };
    steer(ctx, system, ship_id, Order::Command(command), why)
}

/// The client's tutorial warps a new pilot to their personal tutorial site
/// (tutorialsvc.py WarpToTutorial): opened a short warp away, with a few
/// weak rats to practise on, the first time they ask.
fn warp_to_tutorial(ctx: &Ctx<'_>, call: &Call, system: i64, ship_id: i64) -> Outcome {
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("Select a character first.") };
    let range = match call.kwarg("minRange") {
        Some(v) => match finite_number(v) {
            Some(r) => r,
            None => return Outcome::custom("Invalid movement command or unavailable destination."),
        },
        None => DEFAULT_WARP_RANGE,
    };
    let (at, radius) = {
        let mut world = ctx.world.lock();
        let Some(park) = world.parks.get_mut(&system) else { return Outcome::custom("Your ship is not in space.") };
        let Some(near) = park.ships.get(&ship_id).map(|s| s.pos) else { return Outcome::custom("Your ship is not in space.") };
        let (at, radius, entries) = crate::sites::tutorial(park, char_id, near, crate::combat::now_ms());
        if !entries.is_empty() {
            broadcast(&world, system, None, entries);
        }
        (at, radius)
    };
    warp_to_point(ctx, at, radius, range, "WarpToStuff")
}

/// GotoBookmark(bookmarkID): fly straight at one of the pilot's bookmarks in
/// this system, as found for a warp to it.
async fn goto_bookmark(ctx: &Ctx<'_>, bookmark: i64) -> Outcome {
    match warp_target(ctx, "bookmark", bookmark).await {
        Some((at, _)) => approach_point(ctx, at, "GotoBookmark"),
        None => Outcome::custom("That bookmark is not in this solar system."),
    }
}

/// Flies the caller's ship straight to `at`.
pub fn approach_point(ctx: &Ctx<'_>, at: Vec3, why: &str) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    steer(ctx, system, ship_id, Order::Command(Command::Point(at)), why)
}

/// Puts a movement order to a ship: the park follows it, and the system is told.
fn steer(ctx: &Ctx<'_>, system: i64, ship_id: i64, order: Order, method: &str) -> Outcome {
    let mut world = ctx.world.lock();
    if let Order::Command(Command::Follow { target, .. } | Command::Orbit { target, .. }) = &order {
        if world.with_hidden(ctx.client_id, |hidden| hidden.contains(target)).unwrap_or(false) {
            return Outcome::custom("That target is not visible in your ballpark.");
        }
    }
    let Some(park) = world.parks.get_mut(&system) else {
        return Outcome::Unknown(format!("beyonce::{method}: no ballpark for {system}"));
    };
    park.advance(destiny::tick_now());
    let tick = park.tick;
    match park.ships.get(&ship_id) {
        None => return Outcome::Unknown(format!("beyonce::{method}: ship {ship_id} is not in space")),
        Some(ship) if Some(ship.pilot.char_id) != ctx.session.char_id || ship.client_id != ctx.client_id || !ship.is_player() =>
            return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() },
        Some(ship) if ship.identity_reserved => return Outcome::custom("Your ship transfer is still in progress."),
        // destiny ignores course changes mid-warp; so does the server.
        Some(ship) if ship.in_warp() => return Outcome::Value(Value::None),
        Some(ship)
            if matches!(order, Order::Command(Command::Warp { .. }))
                && ship.fight.as_ref().is_some_and(|f| f.afflictions.scrambled >= 1.0) =>
        {
            return Outcome::UserError { msg: "WarpScrambled".into(), args: Vec::new() };
        }
        Some(ship) if matches!(order, Order::Command(Command::Warp { .. })) && ship.cloaked && crate::cloak::warp_safe(park, ship_id) == Some(false) => {
            return Outcome::custom("Your cloaking device does not let you warp; deactivate it first.");
        }
        Some(_) => {}
    }
    if !valid_order(park, ship_id, &order) {
        return Outcome::custom("Invalid movement command or unavailable destination.");
    }
    let (entries, what) = match order {
        Order::SpeedFraction(f) => {
            park.set_speed_fraction(ship_id, f);
            let speed = entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(f)]);
            (vec![speed], format!("speed {f:.2}"))
        }
        Order::Command(command) => {
            let what = format!("{command:?}");
            let mut entries = Vec::new();
            // destiny aligns at the throttle's speed and only warps above
            // 75% of the top one, so a warp opens it.
            if matches!(command, Command::Warp { .. }) && park.ships[&ship_id].speed_fraction < 1.0 {
                park.set_speed_fraction(ship_id, 1.0);
                entries.push(entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(1.0)]));
            }
            park.command(ship_id, command);
            entries.insert(0, command_entry(&park.ships[&ship_id], tick));
            entries.extend(warp_fx(&park.ships[&ship_id], tick));
            (entries, what)
        }
    };
    debug!("ship {ship_id}: {what}");
    broadcast(&world, system, None, entries);
    Outcome::Value(Value::None)
}

async fn parse_order(ctx: &Ctx<'_>, call: &Call) -> Option<Order> {
    let f = |i: usize| call.args.get(i).and_then(finite_number);
    let vec3 = || Some(Vec3::new(f(0)?, f(1)?, f(2)?));
    let range = |default: f64| match call.args.get(1) { Some(v) => finite_number(v), None => Some(default) };
    Some(match call.method.as_str() {
        "SetSpeedFraction" => Order::SpeedFraction(f(0)?.clamp(0.0, 1.0)),
        "GotoDirection" => Order::Command(Command::Direction(vec3()?)),
        "GotoPoint" => Order::Command(Command::Point(vec3()?)),
        "Stop" => Order::Command(Command::Stop),
        "FollowBall" => Order::Command(Command::Follow {
            target: call.arg_i64(0)?,
            range: range(DEFAULT_FOLLOW_RANGE)?,
        }),
        "Orbit" => Order::Command(Command::Orbit { target: call.arg_i64(0)?, range: range(0.0)? }),
        "WarpToStuff" => {
            let kind = call.arg_text(0)?;
            let id = call.arg_i64(1)?;
            let range = match call.kwarg("minRange") { Some(v) => finite_number(v)?, None => DEFAULT_WARP_RANGE };
            let (at, radius) = warp_target(ctx, &kind, id).await?;
            Order::Command(warp_course(ctx, at, radius, range)?)
        }
        _ => return None,
    })
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite())
}

fn valid_order(park: &Park, ship_id: i64, order: &Order) -> bool {
    let Some(ship) = park.ships.get(&ship_id) else { return false; };
    match order {
        Order::SpeedFraction(f) => f.is_finite() && (0.0..=1.0).contains(f),
        Order::Command(command) => {
            if !crate::ballpark::valid_command(ship_id, ship.pos, command) { return false; }
            match command {
                Command::Follow { target, .. } | Command::Orbit { target, .. } => {
                    if let Some(other) = park.ships.get(target) {
                        !other.cloaked && !other.in_warp() && on_grid(ship, other, true)
                    } else {
                        // Celestials are always in the client's ballpark.
                        park.ball(*target).is_some()
                    }
                }
                _ => true,
            }
        }
    }
}

/// What WarpToStuff(kind, id) goes to, as its centre and radius: an item in
/// the system, a bookmark, or another character's ship.
async fn warp_target(ctx: &Ctx<'_>, kind: &str, id: i64) -> Option<(Vec3, f64)> {
    let system = ctx.session.solar_system_id?;
    // IDs beyond an integer name nothing.
    i32::try_from(id).ok()?;
    i32::try_from(system).ok()?;
    let point = |r: &store::Row, [x, y, z]: [&str; 3]| Some(Vec3::new(r.float(x)?, r.float(y)?, r.float(z)?));
    match kind {
        "item" => {
            if let Some(ball) = ctx.world.lock().parks.get(&system).and_then(|p| {
                if p.ships.get(&id).is_some_and(|s| s.cloaked || s.in_warp()) || p.beacons.get(&id).is_some_and(|b|b.hidden) { None } else { p.ball(id) }
            }) {
                return Some(ball);
            }
            ctx.store.read(|db| {
                let r = db.table("mapDenormalize").get_id(id).filter(|r| r.int("solarSystemID") == Some(system))?;
                Some((point(&r, ["x", "y", "z"])?, r.float("radius").unwrap_or(0.0)))
            })
        }
        "bookmark" => {
            // A celestial in this system warps to the celestial; a coordinate
            // bookmark (itemID = locationID = the system) to its own x/y/z.
            // A system, constellation or region is never a warp target --
            // its mapDenormalize x/y/z are galactic, and older bookmarks of
            // one were filed under the system the pilot was in.
            let owner = ctx.session.char_id?;
            ctx.store.read(|db| {
                let b = db
                    .table("bookmarks")
                    .get_id(id)
                    .filter(|b| b.int("ownerID") == Some(owner) && b.int("locationID") == Some(system))?;
                let d = b.int("itemID").and_then(|item| db.table("mapDenormalize").get_id(item));
                let galactic = |g: i64| matches!(g, 3..=5);
                if let Some(d) = &d {
                    let kept = match d.int("groupID") {
                        Some(g) if !galactic(g) => d.int("solarSystemID") == Some(system),
                        Some(_) => b.int("itemID") == b.int("locationID") && b.float("x") != d.float("x"),
                        None => false,
                    };
                    if !kept {
                        return None;
                    }
                }
                // A bookmark of a celestial carries the celestial's position.
                let celestial = d.filter(|d| !d.int("groupID").is_some_and(galactic)).and_then(|d| Some((point(&d, ["x", "y", "z"])?, d.float("radius").unwrap_or(0.0))));
                match celestial {
                    Some(found) => Some(found),
                    None => Some((point(&b, ["x", "y", "z"])?, 0.0)),
                }
            })
        }
        "char" => {
            let world = ctx.world.lock();
            let gang = world.gangs.of(ctx.session.char_id?)?;
            if world.gangs.of(id) != Some(gang) { return None; }
            let park = world.parks.get(&system)?;
            park.ships.values().find(|s| s.pilot.char_id == id && s.is_pilot() && !s.cloaked && !s.in_warp()).map(|s| (s.pos, s.stats.radius))
        }
        "scan" => crate::svc::scan::result_at(ctx.world, ctx.session.char_id?, system, id),
        _ => {
            warn!("WarpToStuff to a {kind:?} is not supported");
            None
        }
    }
}

/// Keep the target and stopping range separate: converting the range to a
/// closer point can turn a valid short warp into Destiny's GotoPoint fallback.
fn warp_course(ctx: &Ctx<'_>, at: Vec3, radius: f64, range: f64) -> Option<Command> {
    let world = ctx.world.lock();
    let park = world.parks.get(&ctx.session.solar_system_id?)?;
    let ship = park.ships.get(&ctx.session.ship_id?)?;
    course_to(ship, at, radius, range)
}

fn course_to(ship: &Ship, at: Vec3, radius: f64, range: f64) -> Option<Command> {
    if !at.is_finite() || !radius.is_finite() || radius < 0.0 || !range.is_finite() || !(0.0..=MAX_WARP_RANGE).contains(&range)
        || (ship.pos - at).len() - radius - ship.stats.radius <= MIN_WARP_DISTANCE { return None; }
    let command = Command::Warp { target: at, range: radius + ship.stats.radius + range };
    crate::ballpark::valid_command(ship.id, ship.pos, &command).then_some(command)
}

// ---- jumps --------------------------------------------------------------------------

pub struct Jump {
    pub solar_system_id: i64,
    pub constellation_id: i64,
    pub region_id: i64,
    pub arrival: Vec3,
}

pub async fn resolve_jump(ctx: &Ctx<'_>, solar_system_id: i64, from_gate: i64, to_gate: i64) -> Option<Jump> {
    // Only a gate pair mapJumps joins.
    crate::sde::table("mapJumps").get("stargateID", from_gate).filter(|j| j.int("celestialID") == Some(to_gate))?;
    ctx.store.read(|db| {
        let map = db.table("mapDenormalize");
        map.get_id(from_gate).filter(|g| g.int("solarSystemID") == Some(solar_system_id) && g.int("groupID") == Some(GROUP_STARGATE))?;
        let d = map.get_id(to_gate)?;
        let gate = Vec3::new(d.float("x")?, d.float("y")?, d.float("z")?);
        Some(Jump {
            solar_system_id: d.int("solarSystemID")?,
            constellation_id: d.int("constellationID")?,
            region_id: d.int("regionID")?,
            arrival: arrival_point(gate, d.float("radius").unwrap_or(0.0)),
        })
    })
}

const ARRIVAL_CLEARANCE: f64 = 1500.0;

/// Where a ship comes out of a gate: just off its surface, on the sun's side.
pub fn arrival_point(gate: Vec3, gate_radius: f64) -> Vec3 {
    let offset = gate_radius + ARRIVAL_CLEARANCE;
    if gate.len() < offset {
        return gate + Vec3::new(offset, 0.0, 0.0);
    }
    gate - gate.unit() * offset
}

const UNDOCK_ALONG_MIN_COS: f64 = 0.25;

/// The undock point and push of a station: its type's dock entry, heading out
/// along the dock orientation.
///
/// Some types' orientation runs along the hull or back into it (type 56 has
/// its entry out along +Z and its orientation down -Y), which slides the
/// undocking ship into the station; those leave straight out from the centre
/// through the entry instead.
pub async fn undock_point(ctx: &Ctx<'_>, station_id: i64) -> Option<(Vec3, Vec3)> {
    let (station, station_type) = ctx.store.read(|db| {
        let s = db.table("staStations").get_id(station_id)?;
        let d = db.table("mapDenormalize").get_id(station_id)?;
        Some((Vec3::new(d.float("x").unwrap_or(0.0), d.float("y").unwrap_or(0.0), d.float("z").unwrap_or(0.0)), s.int("stationTypeID")))
    })?;
    // A station type the static data does not know docks at the centre.
    let st = station_type.and_then(|t| crate::sde::table("staStationTypes").get("stationTypeID", t));
    let v = |[x, y, z]: [&str; 3]| {
        let c = |col: &str| st.and_then(|s| s.float(col)).unwrap_or(0.0);
        Vec3::new(c(x), c(y), c(z))
    };
    let entry = v(["dockEntryX", "dockEntryY", "dockEntryZ"]);
    Some((station + entry, undock_heading(entry, v(["dockOrientationX", "dockOrientationY", "dockOrientationZ"]))))
}

/// See `undock_point`: the orientation if it leads away from the station,
/// within ~75 degrees of straight out (the sound types are within 70).
fn undock_heading(entry: Vec3, orientation: Vec3) -> Vec3 {
    let (out, along) = (entry.unit(), orientation.unit());
    if along != Vec3::ZERO && (out == Vec3::ZERO || out.dot(along) > UNDOCK_ALONG_MIN_COS) {
        along
    } else if out != Vec3::ZERO {
        out
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    }
}

/// Is `id` a station in the park?
pub fn is_station(park: &Park, id: i64) -> bool {
    park.celestial(id).is_some_and(|c| c.group_id == GROUP_STATION)
}
