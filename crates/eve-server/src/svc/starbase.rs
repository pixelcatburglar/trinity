//! Starbases: launching structures from a cargo hold, anchoring them, putting
//! them online and offline, unanchoring and scooping them; the control
//! tower's fuel; the posMgr calls of the tower window (moonmining.py); and
//! sovereignty, which claiming towers win for their alliance. The park side
//! of a structure is pos.rs.
//!
//! A control tower anchors near a moon, one to a moon; everything else
//! anchors within reach of an online tower of the same owner and draws on
//! its CPU and power grid. An online tower burns its fuel every
//! posControlTowerPeriod: the online resources in full, power and CPU fuel
//! in proportion to what its structures draw, a quarter less where its
//! alliance holds sovereignty (moonmining.ShowFuel). Short of online fuel it
//! goes offline, and its structures with it.
//!
//! In a system no NPC faction holds, the alliance with the most claiming,
//! online towers holds sovereignty, once their claims have stood for
//! `RMR_SOV_CLAIM_DELAY` (the claim tab says up to six days); a tie leaves it
//! where it was.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{debug, error, info};

use crate::ballpark::Vec3;
use crate::items::{self, ItemTxn, NewItem};
use crate::pos::{self, state, Structure};
use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db, Store, StoreError, Tx};
use crate::svc::membership::{access_denied, message, Refused};
use crate::svc::moonmining;
use crate::svc::rows::rowset;
use crate::svc::space::{self, entry};
use crate::world::World;

/// dgmEffects: onlineForStructures, anchorLiftForStructures.
const EFFECT_ONLINE: i64 = 901;
const EFFECT_UNANCHOR: i64 = 1023;
const ATTR_POWER: i32 = 30;
const ATTR_CPU: i32 = 50;
const ATTR_POWER_OUTPUT: i32 = 11;
const ATTR_CPU_OUTPUT: i32 = 48;
const ATTR_HARVESTER_CPU_BONUS: i32 = 756;
const ATTR_MAX_STRUCTURE_DISTANCE: i32 = 650;
const ATTR_TOWER_PERIOD: i32 = 722;
const ATTR_ANCHORING_SECURITY_MAX: i32 = 1032;
/// invControlTowerResourcePurposes.
const PURPOSE_ONLINE: i64 = 1;
const PURPOSE_POWER: i64 = 2;
const PURPOSE_CPU: i64 = 3;
const PURPOSE_REINFORCE: i64 = 4;
/// eveConstants corpRoleStarbaseConfig.
const CORP_ROLE_STARBASE_CONFIG: i64 = 1 << 53;
const GROUP_MOON: i64 = 8;
const FLAG_CARGO: i64 = 5;
/// eveConstants maxConfigureDistance, with slack for the two simulations.
const CONFIGURE_RANGE: f64 = 5_000.0 * 1.25;
/// How far from the ship a structure may be anchored (pwn.CheckAnchoringPosition).
const ANCHOR_REACH: f64 = 30_000.0;
/// How close to a moon's surface a control tower must anchor.
const MOON_RANGE: f64 = 100_000.0;
/// No anchoring at or above this security ('clean spacelanes').
const CONCORD_ANCHOR_LIMIT: f64 = 0.8;
/// Room left around a launched structure.
const LAUNCH_CLEARANCE: f64 = 500.0;
/// Fuel burnt where the owner's alliance holds sovereignty.
const SOVEREIGN_FUEL: f64 = 0.75;
const TICKS_PER_MS: i64 = 10_000;
const TICKS_PER_SECOND: i64 = 10_000_000;
const DEFAULT_PERIOD_MS: i64 = 3_600_000;

/// The states in which a structure draws on its tower (onlining,
/// reinforced, online, operating).
fn drawing_state(state: Option<i64>) -> bool {
    matches!(state, Some(2..=5))
}

// ---- launching -------------------------------------------------------------------------

/// ship.Drop(itemIDs, ownerID): structures in the hold go into space, owned
/// by the pilot or their corporation (menusvc LaunchForSelf/LaunchForCorp).
/// None if none of the items is a structure (drones are launched elsewhere).
pub async fn launch(ctx: &Ctx<'_>, ids: &[i64], owner: Option<i64>) -> Option<Outcome> {
    let structures: Vec<i64> = {
        let sde = crate::sde::get();
        let launchable: Vec<i32> = [sde.types_in_category(pos::CATEGORY_STRUCTURE as i64), sde.types_in_group(crate::svc::outposts::GROUP_CONSTRUCTION_PLATFORM)].concat();
        ctx.store.read(|db| {
            let entity = db.table("entity");
            let mut found: Vec<i64> = ids
                .iter()
                .filter(|id| entity.get_id(**id).and_then(|e| e.int("typeID")).is_some_and(|t| launchable.contains(&(t as i32))))
                .copied()
                .collect();
            found.sort_unstable();
            found.dedup();
            found
        })
    };
    if structures.is_empty() {
        return None;
    }
    Some(launch_structures(ctx, ids.iter().copied().filter(|i| structures.contains(i)).collect(), owner).await)
}

async fn launch_structures(ctx: &Ctx<'_>, ids: Vec<i64>, owner: Option<i64>) -> Outcome {
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Outcome::custom("Structures can only be launched in space.");
    };
    let owner = owner.unwrap_or(char_id);
    if owner != char_id && Some(owner) != ctx.session.corp_id {
        return Outcome::custom("You can launch structures for yourself or your corporation.");
    }
    let Some((ship_pos, ship_radius)) = ctx.world.lock().parks.get(&system).and_then(|p| p.ball(ship_id)) else {
        return Outcome::custom("You are not in space.");
    };
    let mut rng = crate::combat::Rng::seeded(crate::combat::now_ms() as u64 ^ ship_id as u64);
    let result: Result<(Vec<(i64, i64, Vec3)>, Vec<Value>), StoreError> = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let mut launched = Vec::new();
        for id in ids {
            let Some(item) = txn.item(id)? else { continue };
            if item.owner_id != char_id || item.location_id != ship_id || item.flag != FLAG_CARGO {
                continue;
            }
            let structure = if item.quantity > 1 {
                txn.set_quantity(id, item.quantity - 1)?;
                txn.create(NewItem { type_id: item.type_id, owner_id: owner, location_id: system, flag: 0, quantity: 1, singleton: true })?
            } else {
                txn.move_to(id, system, 0)?;
                txn.set_singleton(id, true)?;
                if owner != char_id {
                    txn.set_owner(id, owner)?;
                }
                id
            };
            let radius = crate::sde::table("invTypes").get("typeID", item.type_id).and_then(|t| t.float("radius")).unwrap_or(100.0);
            let pos = ship_pos + rng.in_sphere(1.0).unit() * (ship_radius + radius + LAUNCH_CLEARANCE);
            txn.set_position(structure, pos.tuple())?;
            txn.tx().insert_or_ignore("posStructures", &[("itemID", store::Value::Int(structure))])?;
            launched.push((structure, item.type_id, pos));
        }
        Ok((launched, txn.events()))
    });
    let (launched, events) = match result {
        Ok(v) => v,
        Err(e) => {
            error!("launching structures: {}", crate::logging::chain(&e));
            return Outcome::custom("The structure could not be launched.");
        }
    };
    items::notify(ctx.world, ctx.client_id, events);
    // Read back through the loader, for the names, owners and delays.
    let fresh = match ctx.store.read(|db| pos::load(db, system)) {
        Ok(all) => all,
        Err(e) => {
            error!("reading launched structures: {}", crate::logging::chain(&e));
            Vec::new()
        }
    };
    let mut world = ctx.world.lock();
    let mut new_ids = Vec::new();
    if let Some(park) = world.parks.get_mut(&system) {
        let mut adds = Vec::new();
        for s in fresh.into_iter().filter(|s| launched.iter().any(|(id, _, _)| *id == s.id)) {
            adds.push(pos::add_entry(park.tick, &s));
            new_ids.push(Value::Int(s.id));
            info!("launched {} ({}) for {owner} in {system}", s.name, s.id);
            park.structures.insert(s.id, s);
        }
        space::broadcast(&world, system, None, adds);
    }
    Outcome::Value(Value::List(new_ids))
}

// ---- anchoring, onlining, unanchoring --------------------------------------------------

/// The caller's view of one structure: it, and how far the caller's ship is
/// from it and from a point.
struct Seen {
    s: Structure,
    system: i64,
    ship_pos: Vec3,
    distance: f64,
}

fn seen(ctx: &Ctx<'_>, id: i64) -> Option<Seen> {
    let (system, ship_id) = (ctx.session.solar_system_id?, ctx.session.ship_id?);
    let world = ctx.world.lock();
    let park = world.parks.get(&system)?;
    let s = park.structures.get(&id)?.clone();
    let ship_pos = park.ships.get(&ship_id)?.pos;
    let distance = park.surface_distance(ship_id, id)?;
    Some(Seen { s, system, ship_pos, distance })
}

/// posMgr.AnchorStructure(itemID, (x, y, z)).
async fn anchor(ctx: &Ctx<'_>, id: i64, at: Vec3) -> Refused<Value> {
    let Some(Seen { s, system, ship_pos, distance }) = seen(ctx, id) else {
        return Err(Outcome::custom("That structure is not here."));
    };
    let now = crate::bulkdata::filetime_now();
    if !s.is_free(now) {
        return Err(message_with("AlreadyUnanchoring", &[("group", Value::text(&s.name))]));
    }
    if !allowed(ctx, &s, ctx.store.read(|db| deploy_flag(db, s.tower.unwrap_or(s.id), "deployAnchor"))) || distance > CONFIGURE_RANGE {
        return Err(access_denied("you may not anchor that structure from here"));
    }
    let reach = (at - ship_pos).len();
    if reach > ANCHOR_REACH {
        return Err(message_with(
            "CantAnchorTooDistant",
            &[("structure", Value::text(&s.name)), ("limit", Value::Int(ANCHOR_REACH as i64)), ("actual", Value::Int(reach as i64))],
        ));
    }
    let security = ctx.store.read(|db| system_security(db, system));
    if security >= CONCORD_ANCHOR_LIMIT {
        return Err(message("CantAnchorConcordRestriction"));
    }
    if let Some(max) = type_attributes(&[s.type_id], &[ATTR_ANCHORING_SECURITY_MAX]).get(&(s.type_id, ATTR_ANCHORING_SECURITY_MAX)) {
        if security > *max {
            return Err(Outcome::custom(format!(
                "CONCORD forbids anchoring the {} in systems above {max:.1} security.",
                s.name
            )));
        }
    }

    let (tower, moon) = if s.is_tower() {
        (None, Some(moon_for(ctx, system, &s, at).await?))
    } else {
        (Some(tower_for(ctx, system, &s, at, now).await?), None)
    };
    let stamp = pos::timer_start(now, s.delays[0]);
    let saved = ctx.store.write(|tx| {
        set_structure(
            tx,
            id,
            &[
                ("state", store::Value::Int(state::ANCHORED.into())),
                ("stateTimestamp", store::Value::Int(stamp)),
                ("controlTowerID", tower.map_or(store::Value::Null, store::Value::Int)),
                ("moonID", moon.map_or(store::Value::Null, store::Value::Int)),
            ],
        )?;
        if let Some(row) = tx.table("entity").get_id(id).map(|e| e.id) {
            tx.update("entity", row, &[("x", store::Value::Float(at.x)), ("y", store::Value::Float(at.y)), ("z", store::Value::Float(at.z))])?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = saved {
        error!("anchoring {id}: {}", crate::logging::chain(&e));
        return Err(Outcome::custom("The structure could not be anchored."));
    }
    update(ctx.world, system, id, |s, tick| {
        s.state = state::ANCHORED;
        s.stamp = Some(stamp);
        s.tower = tower;
        s.pos = at;
        let mut out = vec![entry(tick, "SetBallPosition", vec![Value::Int(id), Value::Double(at.x), Value::Double(at.y), Value::Double(at.z)])];
        out.extend(pos::free_entries(tick, id, false));
        out
    });
    info!("anchoring {} ({id}) in {system}{}", s.name, tower.map(|t| format!(" at tower {t}")).unwrap_or_default());
    Ok(Value::None)
}

/// The moon a control tower anchors at: the nearest, which no other tower
/// holds.
async fn moon_for(ctx: &Ctx<'_>, system: i64, s: &Structure, at: Vec3) -> Refused<i64> {
    let (moon, taken) = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system).ok_or_else(|| Outcome::custom("You are not in space."))?;
        let moon = park
            .celestials
            .iter()
            .filter(|c| c.group_id == GROUP_MOON)
            .map(|c| (c, (c.pos - at).len() - c.radius))
            .filter(|(_, d)| *d <= MOON_RANGE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(c, _)| (c.id, c.name.clone()));
        (moon, park.structures.values().filter(|o| o.is_tower() && o.id != s.id && o.fixed()).map(|o| o.id).collect::<Vec<_>>())
    };
    let Some((moon_id, moon_name)) = moon else { return Err(message("CantAnchorNoMoonNearby")) };
    let occupied = ctx.store.read(|db| {
        let structures = db.table("posStructures");
        taken.iter().any(|t| structures.get_id(*t).is_some_and(|p| p.int("moonID") == Some(moon_id)))
    });
    if occupied {
        return Err(message_with("CantAnchorMoonAlreadyHasControlTower", &[("tower", Value::text(&s.name)), ("moon", Value::text(moon_name))]));
    }
    Ok(moon_id)
}

/// The online tower of the same owner whose reach covers `at`.
async fn tower_for(ctx: &Ctx<'_>, system: i64, s: &Structure, at: Vec3, now: i64) -> Refused<i64> {
    let towers: Vec<Structure> = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system).ok_or_else(|| Outcome::custom("You are not in space."))?;
        park.structures.values().filter(|t| t.is_tower() && t.fixed()).cloned().collect()
    };
    let types: Vec<i64> = towers.iter().map(|t| t.type_id).collect();
    let reach = type_attributes(&types, &[ATTR_MAX_STRUCTURE_DISTANCE]);
    let near = towers.iter().find(|t| (t.pos - at).len() <= reach.get(&(t.type_id, ATTR_MAX_STRUCTURE_DISTANCE)).copied().unwrap_or(0.0));
    let Some(tower) = near else { return Err(message_with("CantAnchorRequireTower", &[("item", Value::text(&s.name))])) };
    if tower.owner_id != s.owner_id {
        return Err(message_with("CantAnchorStructureOwnerBad", &[("what", Value::text(&s.name)), ("whom", Value::text(owner_name(ctx, tower.owner_id).await))]));
    }
    if !tower.fully_online(now) {
        return Err(message("CantTowerNotOnline"));
    }
    Ok(tower.id)
}

/// dogmaIM Activate/Deactivate of a structure's onlineForStructures and
/// anchorLiftForStructures; None for anything that is not a structure.
pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let id = call.arg_id(0);
    let effect = call.arg_i64(1)?;
    if !matches!(call.method.as_str(), "Activate" | "Deactivate") || ![EFFECT_ONLINE, EFFECT_UNANCHOR].contains(&effect) {
        return None;
    }
    if seen(ctx, id).is_none() {
        return None;
    }
    let result = match (call.method.as_str(), effect) {
        ("Activate", EFFECT_ONLINE) => online(ctx, id).await,
        ("Deactivate", EFFECT_ONLINE) => offline(ctx, id).await,
        ("Activate", EFFECT_UNANCHOR) => unanchor(ctx, id).await,
        _ => Ok(Value::None),
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

async fn online(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let Seen { s, system, distance, .. } = seen(ctx, id).ok_or_else(|| Outcome::custom("That structure is not here."))?;
    not_reinforced(ctx, &s)?;
    let now = crate::bulkdata::filetime_now();
    if !s.fully_anchored(now) {
        return Err(message_with("CantDoWhileNotAnchored", &[("item", Value::text(&s.name))]));
    }
    if !allowed(ctx, &s, ctx.store.read(|db| deploy_flag(db, s.tower.unwrap_or(s.id), "deployOnline"))) || distance > CONFIGURE_RANGE {
        return Err(access_denied("you may not put that structure online from here"));
    }
    if s.is_tower() {
        let fuel = ctx.store.read(|db| Fuel::of(db, &s, system));
        let burn = fuel.burn(0.0, 0.0);
        if let Some((type_id, need)) = burn.iter().find(|(t, need)| fuel.held.get(t).copied().unwrap_or(0) < *need) {
            return Err(message_with(
                "CantOnlineTowerLacksResources",
                &[("tower", Value::text(&s.name)), ("resource", Value::text(format!("{need} x {}", type_name(*type_id))))],
            ));
        }
        // The first period's fuel goes as it comes online.
        if let Err(e) = ctx.store.write(|tx| consume(tx, id, &burn, now)) {
            error!("fuelling tower {id}: {}", crate::logging::chain(&e));
            return Err(Outcome::custom("The tower could not be fuelled."));
        }
    } else {
        draw_from_tower(ctx, system, &s, now).await?;
    }
    let stamp = pos::timer_start(now, s.delays[1]);
    set_state(ctx, system, id, state::ONLINING, Some(stamp)).await?;
    info!("onlining {} ({id}) in {system}", s.name);
    Ok(Value::None)
}

/// A structure coming online needs its tower online, with the CPU and power
/// to spare and fuel for them.
async fn draw_from_tower(ctx: &Ctx<'_>, system: i64, s: &Structure, now: i64) -> Refused<()> {
    let (tower, drawing) = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system).ok_or_else(|| Outcome::custom("You are not in space."))?;
        let tower = s.tower.and_then(|t| park.structures.get(&t)).cloned();
        let drawing: Vec<i64> = park
            .structures
            .values()
            .filter(|o| o.tower == s.tower && o.id != s.id && o.drawing())
            .map(|o| o.type_id)
            .collect();
        (tower, drawing)
    };
    let Some(tower) = tower else { return Err(message_with("CantOnlineRequireTower", &[("item", Value::text(&s.name))])) };
    if !tower.fully_online(now) {
        return Err(message("CantTowerNotOnline"));
    }
    let mut types = drawing.clone();
    types.extend([s.type_id, tower.type_id]);
    let a = type_attributes(&types, &[ATTR_CPU, ATTR_POWER, ATTR_CPU_OUTPUT, ATTR_POWER_OUTPUT]);
    let cpu = cpu_factors(tower.type_id, &types);
    let get = |t: i64, attr: i32| a.get(&(t, attr)).copied().unwrap_or(0.0) * if attr == ATTR_CPU { cpu(t) } else { 1.0 };
    let used = |attr: i32| drawing.iter().map(|t| get(*t, attr)).sum::<f64>();
    let fuel = ctx.store.read(|db| Fuel::of(db, &tower, system));
    for (attr, output, purpose, lacks, what) in [
        (ATTR_CPU, ATTR_CPU_OUTPUT, PURPOSE_CPU, "CantOnlineTowerLacksCpuResources", "CPU"),
        (ATTR_POWER, ATTR_POWER_OUTPUT, PURPOSE_POWER, "CantOnlineTowerLacksPowerResources", "power grid"),
    ] {
        let need = get(s.type_id, attr);
        if need <= 0.0 {
            continue;
        }
        if used(attr) + need > get(tower.type_id, output) {
            return Err(Outcome::custom(format!("The {} does not have the {what} to spare for the {}.", tower.name, s.name)));
        }
        if !fuel.has_any(purpose) {
            return Err(message_with(lacks, &[("what", Value::text(&s.name)), ("tower", Value::text(&tower.name))]));
        }
    }
    Ok(())
}

/// Offline; a tower going offline takes its structures with it.
async fn offline(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let Seen { s, system, distance, .. } = seen(ctx, id).ok_or_else(|| Outcome::custom("That structure is not here."))?;
    not_reinforced(ctx, &s)?;
    if !s.drawing() {
        return Ok(Value::None);
    }
    if !allowed(ctx, &s, ctx.store.read(|db| deploy_flag(db, s.tower.unwrap_or(s.id), "deployOffline"))) || distance > CONFIGURE_RANGE {
        return Err(access_denied("you may not take that structure offline from here"));
    }
    take_offline(ctx.store, ctx.world, system, id).await;
    info!("took {} ({id}) offline in {system}", s.name);
    Ok(Value::None)
}

async fn unanchor(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let Seen { s, system, distance, .. } = seen(ctx, id).ok_or_else(|| Outcome::custom("That structure is not here."))?;
    not_reinforced(ctx, &s)?;
    let now = crate::bulkdata::filetime_now();
    if !s.fully_anchored(now) {
        return Err(message_with("CantDoWhileNotAnchored", &[("item", Value::text(&s.name))]));
    }
    if !allowed(ctx, &s, ctx.store.read(|db| deploy_flag(db, s.tower.unwrap_or(s.id), "deployUnanchor"))) || distance > CONFIGURE_RANGE {
        return Err(access_denied("you may not unanchor that structure from here"));
    }
    if s.is_tower() {
        let anchored_here = ctx.world.lock().parks.get(&system).is_some_and(|p| p.structures.values().any(|o| o.tower == Some(id) && o.fixed()));
        if anchored_here {
            return Err(message_with("CantUnanchorTowerStructuresPresent", &[("tower", Value::text(&s.name))]));
        }
    }
    let holding = ctx.store.read(|db| db.table("entity").find(&["locationID"], &[store::Value::Int(id)]).next().is_some());
    if holding {
        return Err(message_with("CantUnanchorContainerNotEmpty", &[("structure", Value::text(&s.name))]));
    }
    let stamp = pos::timer_start(now, s.delays[2]);
    set_state(ctx, system, id, state::UNANCHORED, Some(stamp)).await?;
    info!("unanchoring {} ({id}) in {system}", s.name);
    Ok(Value::None)
}

/// ship.Scoop(itemID) of a structure: into the cargo hold, once it floats
/// free. None if the item is not a structure.
pub async fn scoop(ctx: &Ctx<'_>, id: i64) -> Option<Outcome> {
    let Seen { s, system, distance, .. } = seen(ctx, id)?;
    Some(scoop_structure(ctx, s, system, distance).await.map_or_else(|refusal| refusal, Outcome::Value))
}

async fn scoop_structure(ctx: &Ctx<'_>, s: Structure, system: i64, distance: f64) -> Refused<Value> {
    let (char_id, ship_id) = (ctx.session.char_id.unwrap_or(0), ctx.session.ship_id.unwrap_or(0));
    let now = crate::bulkdata::filetime_now();
    if s.state != state::UNANCHORED {
        return Err(message_with("CanOnlyScoopIfUnanchored", &[("group", Value::text(&s.name))]));
    }
    if !s.is_free(now) {
        return Err(message_with("CantScoopAnchoring", &[("group", Value::text(&s.name))]));
    }
    if s.owner_id != char_id && Some(s.owner_id) != ctx.session.corp_id {
        return Err(message_with("CantScoopNotInCorp", &[("corp", Value::text(owner_name(ctx, s.owner_id).await))]));
    }
    if distance > crate::loot::TRANSFER_RANGE {
        return Err(Outcome::custom(format!("The {} is {distance:.0} m away; get within {:.0} m to scoop it.", s.name, crate::loot::TRANSFER_RANGE)));
    }
    let types = crate::sde::table("invTypes");
    let (free, volume) = ctx.store.read(|db| {
        let entity = db.table("entity");
        // No hold at all for a ship that is gone or of a type the data does
        // not know; the cargo is summed in the order it comes, and what has
        // no volume takes no room.
        let capacity = entity.get_id(ship_id).and_then(|r| r.int("typeID")).and_then(|t| types.get("typeID", t)).map(|t| t.float("capacity").unwrap_or(0.0));
        let used: f64 = entity
            .find(&["locationID", "flag"], &[store::Value::Int(ship_id), store::Value::Int(FLAG_CARGO)])
            .filter_map(|r| Some(types.get("typeID", r.int("typeID")?)?.float("volume")? * r.int("quantity")? as f64))
            .sum();
        let volume = types.get("typeID", s.type_id).and_then(|t| t.float("volume"));
        (capacity.map_or(0.0, |c| c - used), volume.unwrap_or(0.0))
    });
    if volume > free + 1e-6 {
        return Err(Outcome::custom(format!("Not enough room: the {} needs {volume:.0} m3 and only {free:.0} m3 is free.", s.name)));
    }
    let result = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        txn.move_to(s.id, ship_id, FLAG_CARGO)?;
        if s.owner_id != char_id {
            txn.set_owner(s.id, char_id)?;
        }
        txn.tx().delete_where("posStructures", &["itemID"], &[store::Value::Int(s.id)])?;
        Ok::<_, StoreError>(txn.events())
    });
    let events = match result {
        Ok(events) => events,
        Err(e) => {
            error!("scooping {}: {}", s.id, crate::logging::chain(&e));
            return Err(Outcome::custom("The structure could not be scooped."));
        }
    };
    items::notify(ctx.world, ctx.client_id, events);
    let mut world = ctx.world.lock();
    if let Some(park) = world.parks.get_mut(&system) {
        park.structures.remove(&s.id);
        let gone = entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(s.id)])]);
        space::broadcast(&world, system, None, vec![gone]);
    }
    info!("{char_id} scooped {} ({})", s.name, s.id);
    Ok(Value::None)
}

// ---- state changes ---------------------------------------------------------------------

/// Saves a structure's new state and shows it to the system.
async fn set_state(ctx: &Ctx<'_>, system: i64, id: i64, new: i16, stamp: Option<i64>) -> Refused<()> {
    let saved = ctx.store.write(|tx| {
        set_structure(tx, id, &[("state", store::Value::Int(new.into())), ("stateTimestamp", stamp.map_or(store::Value::Null, store::Value::Int))])
    });
    if let Err(e) = saved {
        error!("saving the state of structure {id}: {}", crate::logging::chain(&e));
        return Err(Outcome::custom("The structure could not do that just now."));
    }
    update(ctx.world, system, id, |s, _| {
        s.state = new;
        s.stamp = stamp;
        Vec::new()
    });
    Ok(())
}

/// Sets columns of the posStructures row of `id` (nothing if there is none).
fn set_structure(tx: &mut Tx<'_>, id: i64, sets: store::Sets) -> Result<(), StoreError> {
    match tx.table("posStructures").get_id(id).map(|p| p.id) {
        Some(row) => tx.update("posStructures", row, sets),
        None => Ok(()),
    }
}

/// Changes a structure in its park and broadcasts `f`'s destiny entries,
/// then its new slim item.
fn update(world: &World, system: i64, id: i64, f: impl FnOnce(&mut Structure, i64) -> Vec<Value>) {
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    let tick = park.tick;
    let Some(s) = park.structures.get_mut(&id) else { return };
    let mut entries = f(s, tick);
    entries.push(pos::slim_change(tick, s));
    space::broadcast(&w, system, None, entries);
}

/// dgmAttributeTypes posPlayerControlStructure: a structure a pilot can man.
const ATTR_PLAYER_CONTROL: i32 = 1167;
/// eveConstants corpRoleStarbaseCaretaker.
const CORP_ROLE_STARBASE_CARETAKER: i64 = 1 << 58;

/// posMgr.AssumeStructureControl(itemID) / RelinquishStructureControl(itemID)
/// (playerOwned.py:355-393): a member of the owning corporation with a
/// starbase role (or a director) takes over one of its online weapons while
/// on grid with it; one pilot at a time, unless the last one has left.
/// The controller is shown to everyone through the slim item.
fn control(ctx: &Ctx<'_>, id: i64, assume: bool) -> Refused<Value> {
    let (Some(system), Some(ship_id), Some(me)) = (ctx.session.solar_system_id, ctx.session.ship_id, ctx.session.char_id) else {
        return Err(Outcome::custom("You must be in space."));
    };
    let mut w = ctx.world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return Err(Outcome::custom("That structure is not here.")) };
    let Some(s) = park.structures.get(&id) else { return Err(Outcome::custom("That structure is not here.")) };
    if !assume {
        if s.controller != Some(me) {
            return Err(Outcome::custom("You are not in control of that structure."));
        }
    } else {
        if crate::sde::get().type_attribute(s.type_id, ATTR_PLAYER_CONTROL).unwrap_or(0.0) == 0.0 {
            return Err(Outcome::custom("That structure cannot be controlled."));
        }
        if ctx.session.corp_id != Some(s.corp_id) {
            return Err(access_denied("only the owning corporation may control its structures"));
        }
        let role = [CORP_ROLE_STARBASE_CARETAKER, CORP_ROLE_STARBASE_CONFIG, CORP_ROLE_DIRECTOR].iter().any(|r| ctx.session.has_corp_role(*r));
        if !role {
            return Err(access_denied("controlling starbase structures needs a starbase role"));
        }
        if !matches!(s.state, pos::state::ONLINE | pos::state::OPERATING) {
            return Err(Outcome::custom("That structure must be online to be controlled."));
        }
        if park.surface_distance(ship_id, id).is_none_or(|d| d > space::GRID_RANGE) {
            return Err(Outcome::custom("You are too far away to control that structure."));
        }
        // Someone else still flying here keeps it.
        let held = s.controller.filter(|c| *c != me && park.ships.values().any(|ship| ship.pilot.char_id == *c && ship.is_pilot()));
        if let Some(other) = held {
            return Err(Outcome::custom(format!("That structure is already controlled by another pilot ({other}).")));
        }
    }
    let tick = park.tick;
    let Some(s) = park.structures.get_mut(&id) else { return Err(Outcome::custom("That structure is not here.")) };
    let want = assume.then_some(me);
    if s.controller == want {
        return Ok(Value::None);
    }
    s.controller = want;
    let entry = pos::slim_change(tick, s);
    space::broadcast(&w, system, None, vec![entry]);
    info!("{me} {} structure {id}", if assume { "took control of" } else { "released" });
    Ok(Value::None)
}

/// A structure, and if it is a tower everything drawing on it, goes offline.
pub async fn take_offline(store: &Store, world: &World, system: i64, id: i64) {
    let offline = store.write(|tx| {
        let rows: Vec<(store::RowId, i64)> = tx
            .table("posStructures")
            .rows()
            .filter(|p| (p.int("itemID") == Some(id) || p.int("controlTowerID") == Some(id)) && drawing_state(p.int("state")))
            .filter_map(|p| Some((p.id, p.int("itemID")?)))
            .collect();
        for (row, _) in &rows {
            tx.update("posStructures", *row, &[("state", store::Value::Int(state::ANCHORED.into())), ("stateTimestamp", store::Value::Null)])?;
        }
        Ok::<_, StoreError>(rows.into_iter().map(|(_, id)| id).collect::<Vec<i64>>())
    });
    let ids: Vec<i64> = match offline {
        Ok(ids) => ids,
        Err(e) => {
            error!("taking structure {id} offline: {}", crate::logging::chain(&e));
            return;
        }
    };
    for id in ids {
        update(world, system, id, |s, _| {
            s.state = state::ANCHORED;
            s.stamp = None;
            Vec::new()
        });
    }
}

/// Nothing changes around a reinforced tower (CantPerformTowerReinforced).
fn not_reinforced(ctx: &Ctx<'_>, s: &Structure) -> Refused<()> {
    let tower = match s.tower {
        Some(t) => ctx.world.lock().parks.get(&ctx.session.solar_system_id.unwrap_or(0)).and_then(|p| p.structures.get(&t)).cloned(),
        None => Some(s.clone()),
    };
    match tower {
        Some(t) if t.invulnerable() => Err(message_with("CantPerformTowerReinforced", &[("tower", Value::text(&t.name))])),
        _ => Ok(()),
    }
}

// ---- combat ----------------------------------------------------------------------------

/// What a structure has left after a fight, so it outlives a restart.
pub async fn save_damage(store: &Store, id: i64, shield: f64, armor: f64, hull: f64) {
    let saved = store.write(|tx| {
        set_structure(
            tx,
            id,
            &[("shieldCharge", store::Value::Float(shield)), ("armorDamage", store::Value::Float(armor)), ("hullDamage", store::Value::Float(hull))],
        )
    });
    if let Err(e) = saved {
        error!("saving the damage of structure {id}: {}", crate::logging::chain(&e));
    }
}

/// A tower fell below a quarter shield. Its strontium buys whole periods of
/// reinforcement, all burnt now; until they run out it takes no damage and
/// nothing around it changes. Without strontium it stays open.
pub async fn reinforce(world: &World, store: &Store, system: i64, tower_id: i64) {
    let Some((tower, fuel)) = store.read(|db| {
        let tower = tower_as_loaded(db, system, tower_id).ok().flatten()?;
        let fuel = Fuel::of(db, &tower, system);
        Some((tower, fuel))
    }) else {
        return;
    };
    let per_period: Vec<(i64, i64)> = fuel
        .needs
        .iter()
        .filter(|(_, purpose, _)| *purpose == PURPOSE_REINFORCE)
        .map(|&(type_id, _, quantity)| (type_id, ((quantity as f64 * fuel.factor).ceil() as i64).max(1)))
        .collect();
    let periods = per_period.iter().map(|(t, q)| fuel.held.get(t).copied().unwrap_or(0) / q).min().unwrap_or(0);
    if periods == 0 {
        info!("tower {tower_id} in {system} is below a quarter shield with no strontium to reinforce it");
        return;
    }
    let period = type_attributes(&[tower.type_id], &[ATTR_TOWER_PERIOD])
        .get(&(tower.type_id, ATTR_TOWER_PERIOD))
        .map_or(DEFAULT_PERIOD_MS, |ms| *ms as i64)
        * TICKS_PER_MS;
    let until = crate::bulkdata::filetime_now() + periods * period;
    let burn: Vec<(i64, i64)> = per_period.iter().map(|(t, q)| (*t, q * periods)).collect();
    let saved = store.write(|tx| {
        take(tx, tower_id, &burn)?;
        set_structure(tx, tower_id, &[("state", store::Value::Int(state::REINFORCED.into())), ("stateTimestamp", store::Value::Int(until))])
    });
    if let Err(e) = saved {
        error!("reinforcing tower {tower_id}: {}", crate::logging::chain(&e));
        return;
    }
    update(world, system, tower_id, |s, _| {
        s.state = state::REINFORCED;
        s.stamp = Some(until);
        Vec::new()
    });
    info!("tower {tower_id} in {system} is reinforced for {periods} period(s)");
}

/// A structure was destroyed: it goes with what it held; a tower's
/// structures go offline and come loose from it.
pub async fn lost(store: &Store, id: i64, tower: bool) {
    let result = store.write(|tx| {
        let held: Vec<store::RowId> = tx.table("entity").find(&["locationID"], &[store::Value::Int(id)]).map(|e| e.id).collect();
        for row in held {
            tx.delete("entity", row)?;
        }
        if let Some(row) = tx.table("entity").get_id(id).map(|e| e.id) {
            tx.delete("entity", row)?;
        }
        tx.delete_where("posStructures", &["itemID"], &[store::Value::Int(id)])?;
        if tower {
            let loose: Vec<(store::RowId, bool)> = tx
                .table("posStructures")
                .rows()
                .filter(|p| p.int("controlTowerID") == Some(id))
                .map(|p| (p.id, drawing_state(p.int("state"))))
                .collect();
            for (row, drawing) in loose {
                let mut sets = vec![("controlTowerID", store::Value::Null)];
                if drawing {
                    sets.push(("state", store::Value::Int(state::ANCHORED.into())));
                    sets.push(("stateTimestamp", store::Value::Null));
                }
                tx.update("posStructures", row, &sets)?;
            }
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = result {
        error!("clearing away structure {id}: {}", crate::logging::chain(&e));
    }
}

// ---- who may, and helpers -----------------------------------------------------------------

/// Whether the caller may act on `s` under a tower deploy setting: 0 needs
/// the Starbase Config role in the owning corporation, 1 any of its members,
/// 2 any member of its alliance. Owners may always.
pub(crate) fn allowed(ctx: &Ctx<'_>, s: &Structure, deploy: i16) -> bool {
    if ctx.session.char_id == Some(s.owner_id) {
        return true;
    }
    let member = ctx.session.corp_id == Some(s.corp_id);
    match deploy {
        2 => member || (s.alliance_id.is_some() && ctx.session.alliance_id == s.alliance_id),
        1 => member,
        _ => member && (ctx.session.has_corp_role(CORP_ROLE_STARBASE_CONFIG) || ctx.session.has_corp_role(CORP_ROLE_DIRECTOR)),
    }
}

fn deploy_flag(db: &Db, tower: i64, column: &str) -> i16 {
    db.table("posStructures").get_id(tower).and_then(|p| p.int(column)).unwrap_or(0) as i16
}

/// Base attribute values: (typeID, attributeID) -> value.
fn type_attributes(types: &[i64], attrs: &[i32]) -> HashMap<(i64, i32), f64> {
    let sde = crate::sde::get();
    types
        .iter()
        .flat_map(|t| sde.type_attributes(*t).iter().filter(|(a, _)| attrs.contains(a)).map(move |(a, v)| ((*t, *a), *v)))
        .collect()
}

fn system_security(db: &Db, system: i64) -> f64 {
    db.table("mapSolarSystems").get_id(system).and_then(|s| s.float("security")).unwrap_or(0.0)
}

/// A corporation's name, else a character's, else ''.
async fn owner_name(ctx: &Ctx<'_>, owner: i64) -> String {
    ctx.store.read(|db| {
        db.table("corporation")
            .get_id(owner)
            .and_then(|c| c.text("corporationName"))
            .or_else(|| db.table("character_").get_id(owner).and_then(|c| c.text("characterName")))
            .unwrap_or_default()
            .to_string()
    })
}

fn type_name(type_id: i64) -> String {
    crate::sde::get().type_(type_id).map(|t| t.name.to_string()).unwrap_or_default()
}

fn message_with(key: &str, args: &[(&str, Value)]) -> Outcome {
    Outcome::UserError { msg: key.into(), args: args.iter().map(|(k, v)| ((*k).into(), v.clone())).collect() }
}

// ---- fuel ------------------------------------------------------------------------------

/// What a tower burns and what it holds.
struct Fuel {
    /// (resource, purpose, quantity per period), the ones this system asks
    /// for: faction charters only in that faction's high-security space.
    needs: Vec<(i64, i64, i64)>,
    held: HashMap<i64, i64>,
    /// 0.75 where the owner's alliance holds sovereignty, else 1.
    factor: f64,
}

impl Fuel {
    fn of(db: &Db, tower: &Structure, system: i64) -> Fuel {
        let here = db.table("mapSolarSystems").get_id(system);
        let needs = here.map_or_else(Vec::new, |s| {
            let sde = crate::sde::get();
            let security = s.float("security");
            // The system's faction, else its constellation's, else its region's.
            let faction = s
                .int("factionID")
                .or_else(|| sde.table("mapConstellations").get("constellationID", s.int("constellationID")?)?.int("factionID"))
                .or_else(|| sde.table("mapRegions").get("regionID", s.int("regionID")?)?.int("factionID"));
            sde.table("invControlTowerResources")
                .find("controlTowerTypeID", tower.type_id)
                .filter(|r| r.float("minSecurityLevel").is_none_or(|min| security.is_some_and(|s| s >= min)))
                .filter(|r| r.int("factionID").is_none_or(|f| faction == Some(f)))
                .map(|r| (r.int("resourceTypeID").unwrap_or(0), r.int("purpose").unwrap_or(0), r.int("quantity").unwrap_or(0)))
                .collect()
        });
        let mut held: HashMap<i64, i64> = HashMap::new();
        for e in db.table("entity").find(&["locationID"], &[store::Value::Int(tower.id)]) {
            *held.entry(e.int("typeID").unwrap_or(0)).or_insert(0) += e.int("quantity").unwrap_or(0);
        }
        let sovereign = here.is_some_and(|s| tower.alliance_id.is_some() && s.int("allianceID") == tower.alliance_id);
        Fuel { needs, held, factor: if sovereign { SOVEREIGN_FUEL } else { 1.0 } }
    }

    /// One period's burn, for power and CPU loads (fractions of the tower's
    /// output in use).
    fn burn(&self, power_load: f64, cpu_load: f64) -> Vec<(i64, i64)> {
        self.needs
            .iter()
            .filter_map(|&(type_id, purpose, quantity)| {
                let load = match purpose {
                    PURPOSE_ONLINE => 1.0,
                    PURPOSE_POWER => power_load,
                    PURPOSE_CPU => cpu_load,
                    _ => return None,
                };
                let amount = (quantity as f64 * self.factor * load).ceil() as i64;
                (amount > 0).then_some((type_id, amount))
            })
            .collect()
    }

    fn has_any(&self, purpose: i64) -> bool {
        self.needs.iter().any(|(t, p, _)| *p == purpose && self.held.get(t).copied().unwrap_or(0) > 0)
    }
}

/// Takes `burn` out of a tower's bay and records the period as paid for.
fn consume(tx: &mut Tx<'_>, tower: i64, burn: &[(i64, i64)], now: i64) -> Result<(), StoreError> {
    take(tx, tower, burn)?;
    set_structure(tx, tower, &[("lastFuel", store::Value::Int(now))])
}

/// Takes `burn` out of a tower's bay, smallest stacks first.
fn take(tx: &mut Tx<'_>, tower: i64, burn: &[(i64, i64)]) -> Result<(), StoreError> {
    for &(type_id, mut amount) in burn {
        let mut stacks: Vec<(store::RowId, i64)> = tx
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(tower)])
            .filter(|e| e.int("typeID") == Some(type_id))
            .map(|e| (e.id, e.int("quantity").unwrap_or(0)))
            .collect();
        stacks.sort_by_key(|(_, quantity)| *quantity);
        for (row, quantity) in stacks {
            if amount == 0 {
                break;
            }
            let take = amount.min(quantity);
            if take == quantity {
                tx.delete("entity", row)?;
            } else {
                tx.update("entity", row, &[("quantity", store::Value::Int(quantity - take))])?;
            }
            amount -= take;
        }
    }
    Ok(())
}

/// Every online tower whose period is up burns another period's fuel, or
/// goes offline without it. Run by the heartbeat.
pub async fn burn_fuel(world: &World, store: &Store) {
    let now = crate::bulkdata::filetime_now();
    let sde = crate::sde::get();
    let towers: Vec<(i64, i64, Option<i64>, i64)> = store.read(|db| {
        let types = sde.types_in_group(pos::GROUP_CONTROL_TOWER as i64);
        let entity = db.table("entity");
        db.table("posStructures")
            .rows()
            .filter(|p| drawing_state(p.int("state")))
            .filter_map(|p| {
                let e = entity.get_id(p.int("itemID")?)?;
                let type_id = e.int("typeID").filter(|t| types.contains(&(*t as i32)))?;
                Some((p.int("itemID")?, e.int("locationID").unwrap_or(0), p.int("lastFuel"), type_id))
            })
            .collect()
    });
    for (tower_id, system, last, type_id) in towers {
        let period = sde.type_attribute(type_id, ATTR_TOWER_PERIOD).unwrap_or(DEFAULT_PERIOD_MS as f64) as i64 * TICKS_PER_MS;
        let last = last.unwrap_or(now);
        if now < last + period {
            continue;
        }
        let Some((fuel, (power, cpu))) = store.read(|db| {
            let tower = tower_as_loaded(db, system, tower_id).ok().flatten()?;
            Some((Fuel::of(db, &tower, system), tower_load(db, &tower)))
        }) else {
            continue;
        };
        let burn = fuel.burn(power, cpu);
        let short = burn.iter().find(|(t, need)| fuel.held.get(t).copied().unwrap_or(0) < *need).copied();
        match short {
            Some((type_id, need)) => {
                info!("tower {tower_id} in {system} goes offline: it needs {need} of type {type_id}");
                take_offline(store, world, system, tower_id).await;
            }
            None => match store.write(|tx| consume(tx, tower_id, &burn, last + period)) {
                Ok(()) => moonmining::cycle(store, tower_id).await,
                Err(e) => error!("fuelling tower {tower_id}: {}", crate::logging::chain(&e)),
            },
        }
    }
}

/// A tower as the park loader reads it (owner, alliance, type).
fn tower_as_loaded(db: &Db, system: i64, tower: i64) -> Result<Option<Structure>, StoreError> {
    Ok(pos::load(db, system)?.into_iter().find(|s| s.id == tower))
}

/// The fractions of a tower's power grid and CPU its online structures use.
fn tower_load(db: &Db, tower: &Structure) -> (f64, f64) {
    let entity = db.table("entity");
    let drawing: Vec<i64> = db
        .table("posStructures")
        .rows()
        .filter(|p| p.int("controlTowerID") == Some(tower.id) && drawing_state(p.int("state")))
        .filter_map(|p| entity.get_id(p.int("itemID")?)?.int("typeID"))
        .collect();
    let mut types = drawing.clone();
    types.push(tower.type_id);
    let a = type_attributes(&types, &[ATTR_CPU, ATTR_POWER, ATTR_CPU_OUTPUT, ATTR_POWER_OUTPUT]);
    let cpu = cpu_factors(tower.type_id, &types);
    let get = |t: i64, attr: i32| a.get(&(t, attr)).copied().unwrap_or(0.0) * if attr == ATTR_CPU { cpu(t) } else { 1.0 };
    let load = |attr: i32, output: i32| {
        let out = get(tower.type_id, output);
        if out <= 0.0 { 0.0 } else { drawing.iter().map(|t| get(*t, attr)).sum::<f64>() / out }
    };
    (load(ATTR_POWER, ATTR_POWER_OUTPUT), load(ATTR_CPU, ATTR_CPU_OUTPUT))
}

/// What each type's CPU need is multiplied by at this tower: its
/// controlTowerMoonHarvesterCPUBonus (a percentage) for moon harvesters, 1
/// for everything else.
fn cpu_factors(tower_type: i64, types: &[i64]) -> impl Fn(i64) -> f64 {
    let bonus = type_attributes(&[tower_type], &[ATTR_HARVESTER_CPU_BONUS]).get(&(tower_type, ATTR_HARVESTER_CPU_BONUS)).copied().unwrap_or(0.0);
    let sde = crate::sde::get();
    let harvesters: Vec<i64> =
        types.iter().copied().filter(|t| sde.type_(*t).is_some_and(|ty| ty.group_id as i64 == moonmining::GROUP_MOON_MINING)).collect();
    move |t| if harvesters.contains(&t) { 1.0 + bonus / 100.0 } else { 1.0 }
}

// ---- sovereignty -----------------------------------------------------------------------

/// Counts claiming towers and moves sovereignty where it has changed hands.
/// Run by the heartbeat.
pub async fn sovereignty(store: &Store, claim_delay_secs: i64) {
    let now = crate::bulkdata::filetime_now();
    // A claim starts standing when its tower is online, and lapses when it
    // is not, or no longer claims.
    let claimed = store.write(|tx| {
        let changes: Vec<(store::RowId, store::Value)> = tx
            .table("posStructures")
            .rows()
            .filter_map(|p| {
                let (claim, drawing, since) = (p.int("claim"), drawing_state(p.int("state")), p.int("claimSince"));
                if claim == Some(1) && drawing && since.is_none() {
                    Some((p.id, store::Value::Int(now)))
                } else if (claim == Some(0) || !drawing) && since.is_some() {
                    Some((p.id, store::Value::Null))
                } else {
                    None
                }
            })
            .collect();
        for (row, since) in changes {
            tx.update("posStructures", row, &[("claimSince", since)])?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = claimed {
        error!("updating claims: {}", crate::logging::chain(&e));
        return;
    }
    let standing = now - claim_delay_secs * TICKS_PER_SECOND;
    // Standing claims by system and alliance (a corporation's, or its
    // owning character's corporation's), and who holds each system now.
    let (claims, holders) = store.read(|db| {
        let entity = db.table("entity");
        let systems = db.table("mapSolarSystems");
        let corporations = db.table("corporation");
        let characters = db.table("character_");
        let mut claims: Vec<((i64, Option<i64>), i64)> = Vec::new();
        for p in db.table("posStructures").rows() {
            if p.int("claim") != Some(1) || !p.int("claimSince").is_some_and(|t| t <= standing) {
                continue;
            }
            let Some(e) = p.int("itemID").and_then(|id| entity.get_id(id)) else { continue };
            let Some(system) = e.int("locationID").and_then(|l| systems.get_id(l)) else { continue };
            if !system.value("factionID").is_null() {
                continue;
            }
            let owner = e.int("ownerID");
            let alliance = owner
                .and_then(|o| corporations.get_id(o))
                .and_then(|co| co.int("allianceID"))
                .or_else(|| {
                    let corp = owner.and_then(|o| characters.get_id(o))?.int("corporationID")?;
                    corporations.get_id(corp)?.int("allianceID")
                })
                .filter(|a| *a != 0);
            let key = (system.int("solarSystemID").unwrap_or(0), alliance);
            match claims.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => claims.push((key, 1)),
            }
        }
        let holders: HashMap<i64, i64> =
            systems.rows().filter_map(|s| Some((s.int("solarSystemID")?, s.int("allianceID")?))).collect();
        (claims, holders)
    });
    let mut by_system: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for ((system, alliance), count) in claims {
        if let Some(alliance) = alliance {
            by_system.entry(system).or_default().push((alliance, count));
        }
    }
    let mut systems: Vec<i64> = by_system.keys().chain(holders.keys()).copied().collect();
    systems.sort_unstable();
    systems.dedup();
    let changes: Vec<(i64, Option<i64>, Option<i64>)> = systems
        .into_iter()
        .filter_map(|system| {
            let holder = holders.get(&system).copied();
            let winner = sovereign(by_system.get(&system).map_or(&[][..], |v| v), holder);
            (winner != holder).then_some((system, holder, winner))
        })
        .collect();
    // Many at once (null-sec settled at the start) are summed up, not listed.
    let many = changes.len() > SOV_CHANGES_LISTED;
    let mut moved = 0;
    for (system, holder, winner) in &changes {
        let saved = store.write(|tx| match tx.table("mapSolarSystems").get_id(*system).map(|s| s.id) {
            Some(row) => tx.update("mapSolarSystems", row, &[("allianceID", winner.map_or(store::Value::Null, store::Value::Int))]),
            None => Ok(()),
        });
        match saved {
            Ok(_) if many => debug!("sovereignty of {system} passes from {holder:?} to {winner:?}"),
            Ok(_) => info!("sovereignty of {system} passes from {holder:?} to {winner:?}"),
            Err(e) => {
                error!("moving sovereignty of {system}: {}", crate::logging::chain(&e));
                continue;
            }
        }
        moved += 1;
    }
    if many {
        info!("sovereignty of {moved} systems changed hands");
    }
}

/// More changes of sovereignty than this in one pass are logged as a count.
const SOV_CHANGES_LISTED: usize = 10;

/// The alliance with strictly the most claims; on a tie the holder keeps it
/// if it is among the leaders, else nobody holds it.
fn sovereign(claims: &[(i64, i64)], holder: Option<i64>) -> Option<i64> {
    let most = claims.iter().map(|(_, n)| *n).max()?;
    let leaders: Vec<i64> = claims.iter().filter(|(_, n)| *n == most).map(|(a, _)| *a).collect();
    match leaders.as_slice() {
        [only] => Some(*only),
        _ => holder.filter(|h| leaders.contains(h)),
    }
}

// ---- posMgr ----------------------------------------------------------------------------

/// posMgr, bound per solar system (moniker.GetPOSMgr); also called unbound
/// for the fuel table.
pub async fn pos_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let tower = call.arg_id(0);
    let flag = |i: usize| call.args.get(i).map(|v| v.as_i64().unwrap_or(0) != 0).unwrap_or(false);
    let result = match call.method.as_str() {
        "GetControlTowers" => Ok(super::bridges::towers(ctx).await),
        "GetJumpArrays" => Ok(super::bridges::list(ctx).await),
        "InstallJumpBridgeLink" => return Some(super::bridges::install(ctx, call.arg_id(0), call.arg_id(1), call.arg_id(2)).await),
        "UninstallJumpBridgeLink" => return Some(super::bridges::uninstall(ctx, call.arg_id(0)).await),
        "GetControlTowerFuelRequirements" => {
            let header = ["controlTowerTypeID", "resourceTypeID", "purpose", "quantity", "minSecurityLevel", "factionID"];
            let mut resources: Vec<_> = crate::sde::table("invControlTowerResources").rows().collect();
            // ORDER BY 1, 3, 2, NULLs last.
            resources.sort_by_key(|r| {
                (r.int("controlTowerTypeID"), r.int("purpose").is_none(), r.int("purpose"), r.int("resourceTypeID"))
            });
            Ok(rowset(&header, resources.iter().map(|r| Value::List(r.line(&header))).collect()))
        }
        "AnchorStructure" => {
            let at = call.args.get(1).and_then(|v| v.seq()).map(|p| p.iter().filter_map(|c| c.as_f64()).collect::<Vec<_>>());
            match at.as_deref() {
                Some([x, y, z]) => anchor(ctx, tower, Vec3::new(*x, *y, *z)).await,
                _ => Err(Outcome::custom("That is not a position.")),
            }
        }
        "GetMoonForTower" => Ok(moonmining::moon_for_tower(ctx, tower).await),
        "GetTowerClaim" => Ok(Value::Bool(setting(ctx, tower, "claim").await.and_then(|v| v.as_i64()) == Some(1))),
        "SetTowerClaim" => {
            let claim = flag(1);
            configure(ctx, tower, &[("claim", store::Value::Int(i64::from(claim))), ("claimSince", store::Value::Null)]).await.map(|v| {
                info!("tower {tower} {} sovereignty", if claim { "claims" } else { "no longer claims" });
                v
            })
        }
        "GetTowerSentrySettings" => {
            const COLUMNS: [&str; 6] = ["sentryStanding", "sentryStatus", "sentryStandingDrop", "sentryStatusDrop", "sentryAggression", "sentryWar"];
            let r = ctx.store.read(|db| db.table("posStructures").get_id(tower).map(|p| p.line(&COLUMNS)));
            let cell = |i: usize| r.as_ref().map_or(Value::None, |r| r[i].clone());
            let truth = |i: usize| Value::Bool(cell(i).as_i64() == Some(1));
            Ok(Value::Tuple(vec![cell(0), cell(1), truth(2), truth(3), truth(4), truth(5)]))
        }
        "SetTowerSentrySettings" => {
            let opt = |i: usize| call.args.get(i).and_then(|v| v.as_f64());
            let sentry = crate::guard::Sentry {
                standing: opt(1),
                status: opt(2),
                standing_drop: flag(3),
                status_drop: flag(4),
                aggression: flag(5),
                war: flag(6),
            };
            let system = ctx.session.solar_system_id.unwrap_or(0);
            if let Some(s) = ctx.world.lock().parks.get_mut(&system).and_then(|p| p.structures.get_mut(&tower)) {
                s.sentry = sentry;
            }
            let number = |v: Option<f64>| v.map_or(store::Value::Null, store::Value::Float);
            configure(
                ctx,
                tower,
                &[
                    ("sentryStanding", number(opt(1))),
                    ("sentryStatus", number(opt(2))),
                    ("sentryStandingDrop", store::Value::Int(i64::from(flag(3)))),
                    ("sentryStatusDrop", store::Value::Int(i64::from(flag(4)))),
                    ("sentryAggression", store::Value::Int(i64::from(flag(5)))),
                    ("sentryWar", store::Value::Int(i64::from(flag(6)))),
                ],
            )
            .await
        }
        // SetTowerPassword(towerID, password or False to keep it, allowCorp, allowAlliance)
        "SetTowerPassword" => {
            let password = call.arg_text(1);
            let (corp, alliance) = (flag(2), flag(3));
            let mut sets = vec![("allowCorp", store::Value::Int(i64::from(corp))), ("allowAlliance", store::Value::Int(i64::from(alliance)))];
            if let Some(p) = &password {
                sets.push(("password", store::Value::Text(p.as_str().into())));
            }
            let saved = configure(ctx, tower, &sets).await;
            if saved.is_ok() {
                // The field retunes on the next pulse (pos::step).
                let system = ctx.session.solar_system_id.unwrap_or(0);
                if let Some(s) = ctx.world.lock().parks.get_mut(&system).and_then(|p| p.structures.get_mut(&tower)) {
                    if let Some(p) = &password {
                        s.harmonic = pos::harmonic(p);
                    }
                    s.allow_corp = corp;
                    s.allow_alliance = alliance;
                }
            }
            saved
        }
        // SetShipPassword(password): the ship's harmonic, until it leaves space.
        "SetShipPassword" => ship_password(ctx, &call.arg_text(0).unwrap_or_default()),
        "AssumeStructureControl" => control(ctx, call.arg_id(0), true),
        "RelinquishStructureControl" => control(ctx, call.arg_id(0), false),
        "GetStarbasePermissions" => Ok(permissions(ctx, tower).await),
        "SetStarbasePermissions" => set_permissions(ctx, tower, call).await,
        "GetMoonProcessInfoForTower" => Ok(moonmining::process_info(ctx, tower).await),
        "LinkResourceForTower" => match may_configure(ctx, tower) {
            Ok(()) => moonmining::link(ctx, tower, call.args.get(1)).await,
            Err(refusal) => Err(refusal),
        },
        "ChangeStructureProvisionType" => match may_configure(ctx, tower) {
            Ok(()) => moonmining::change_provision(ctx, tower, call.arg_id(1), call.arg_id(2)).await,
            Err(refusal) => Err(refusal),
        },
        // The client's shift-click debug cycle: production runs on the tower's
        // fuel period only.
        "RunMoonProcessCycleforTower" => Ok(Value::None),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// A ship takes the harmonic of a password, unless it sits inside a force
/// field, which would tear it apart.
fn ship_password(ctx: &Ctx<'_>, password: &str) -> Refused<Value> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Err(Outcome::custom("You are not in space."));
    };
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return Err(Outcome::custom("You are not in space.")) };
    let Some(pos) = park.ships.get(&ship_id).map(|s| s.pos) else { return Err(Outcome::custom("You are not in space.")) };
    let inside = park.structures.values().any(|s| s.field.as_ref().is_some_and(|f| (pos - s.pos).len() < f.radius));
    if inside {
        return Err(message("CantChangeShipHarmonicWithinForceField"));
    }
    let tick = park.tick;
    let Some(ship) = park.ships.get_mut(&ship_id) else { return Err(Outcome::custom("You are not in space.")) };
    ship.harmonic = pos::harmonic(password);
    let key = (ship.harmonic, ship.pilot.corp_id, ship.pilot.alliance_id.unwrap_or(-1));
    let set = pos::harmonic_entry(tick, ship_id, key, false);
    space::broadcast(&world, system, None, vec![set]);
    Ok(Value::None)
}

/// One column of a structure's settings row.
async fn setting(ctx: &Ctx<'_>, id: i64, column: &str) -> Option<Value> {
    ctx.store.read(|db| db.table("posStructures").get_id(id).map(|p| p.cell(column)))
}

/// Whether the caller may configure the tower (Starbase Config, or owner).
fn may_configure(ctx: &Ctx<'_>, tower: i64) -> Refused<()> {
    let Some(Seen { s, .. }) = seen(ctx, tower) else { return Err(Outcome::custom("That tower is not here.")) };
    not_reinforced(ctx, &s)?;
    if !allowed(ctx, &s, 0) {
        return Err(message("CantConfigureCorpStarbaseRoleLacking"));
    }
    Ok(())
}

/// Changes a tower's settings, for those allowed to configure it.
async fn configure(ctx: &Ctx<'_>, tower: i64, sets: store::Sets<'_>) -> Refused<Value> {
    let Some(Seen { s, .. }) = seen(ctx, tower) else { return Err(Outcome::custom("That tower is not here.")) };
    not_reinforced(ctx, &s)?;
    if !allowed(ctx, &s, 0) {
        return Err(message("CantConfigureCorpStarbaseRoleLacking"));
    }
    match ctx.store.write(|tx| set_structure(tx, tower, sets)) {
        Ok(_) => Ok(Value::None),
        Err(e) => {
            error!("configuring tower {tower}: {}", crate::logging::chain(&e));
            Err(Outcome::custom("The tower could not be configured."))
        }
    }
}

const USAGE_HEADER: [&str; 4] = ["structureID", "viewput", "viewputtake", "use"];

/// (deployFlags, usageFlagsList): who may anchor, unanchor, online and
/// offline around the tower; and per structure, who may look, take and use.
async fn permissions(ctx: &Ctx<'_>, tower: i64) -> Value {
    let (flags, lines) = ctx.store.read(|db| {
        let structures = db.table("posStructures");
        let r = structures.get_id(tower);
        let flags: Vec<i64> =
            ["deployAnchor", "deployUnanchor", "deployOnline", "deployOffline"].iter().map(|c| r.and_then(|r| r.int(c)).unwrap_or(0)).collect();
        let mut usage: Vec<_> = structures.rows().filter(|p| p.int("controlTowerID") == Some(tower)).collect();
        usage.sort_by_key(|p| p.int("itemID"));
        let lines: Vec<Value> = usage.iter().map(|p| Value::List(p.line(&["itemID", "useViewPut", "useViewPutTake", "useUse"]))).collect();
        (flags, lines)
    });
    let deploy = Value::Dict(
        ["anchor", "unanchor", "online", "offline"].iter().zip(flags).map(|(k, flag)| (Value::bytes(*k), Value::Int(flag))).collect(),
    );
    let usage = rowset(&USAGE_HEADER, lines);
    Value::Tuple(vec![deploy, usage])
}

async fn set_permissions(ctx: &Ctx<'_>, tower: i64, call: &Call) -> Refused<Value> {
    let deploy = |key: &str| -> i16 {
        match call.args.get(1) {
            Some(Value::Dict(d)) => d.iter().find(|(k, _)| *k == Value::bytes(key)).and_then(|(_, v)| v.as_i64()).unwrap_or(0) as i16,
            _ => 0,
        }
    };
    let flag = |key: &str| store::Value::Int(deploy(key).into());
    configure(
        ctx,
        tower,
        &[("deployAnchor", flag("anchor")), ("deployUnanchor", flag("unanchor")), ("deployOnline", flag("online")), ("deployOffline", flag("offline"))],
    )
    .await?;
    // usageFlagsList: a util.Rowset of [structureID, viewput, viewputtake, use].
    let lines = match call.args.get(2) {
        Some(Value::Instance { state, .. }) => match state.as_ref() {
            Value::Dict(d) => d.iter().find(|(k, _)| *k == Value::bytes("lines")).and_then(|(_, v)| v.seq()).map(|l| l.to_vec()),
            _ => None,
        },
        _ => None,
    };
    let saved = ctx.store.write(|tx| {
        for l in lines.unwrap_or_default() {
            let Some(l) = l.seq() else { continue };
            let at = |i: usize| l.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
            // Only the tower's own structures.
            let Some(row) = tx.table("posStructures").get_id(at(0)).filter(|p| p.int("controlTowerID") == Some(tower)).map(|p| p.id) else { continue };
            let usage = |i: usize| store::Value::Int((at(i) as i16).into());
            tx.update("posStructures", row, &[("useViewPut", usage(1)), ("useViewPutTake", usage(2)), ("useUse", usage(3))])?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = saved {
        error!("statement failed: {}", crate::logging::chain(&e));
    }
    Ok(Value::None)
}

// ---- reach -----------------------------------------------------------------------------

/// What a pilot does with a structure's contents, each governed by one of
/// the tower's usage settings (moonmining.StructureAccess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Look inside and put things in: "viewput".
    ViewPut,
    /// Take things out as well: "viewputtake".
    Take,
    /// Use what it does: "use".
    Use,
}

/// Whether `id` is a structure here, and if so whether the caller's ship is
/// close enough and the structure's usage setting lets it (an owner always
/// may). None for anything that is not a structure.
pub async fn may(ctx: &Ctx<'_>, id: i64, access: Access) -> Option<bool> {
    let Seen { s, distance, .. } = seen(ctx, id)?;
    if distance > crate::loot::TRANSFER_RANGE * 1.25 {
        return Some(false);
    }
    // Putting a reaction into a reactor is using it.
    let access = if access == Access::ViewPut && s.group_id == moonmining::GROUP_REACTOR { Access::Use } else { access };
    let column = match access {
        Access::ViewPut => "useViewPut",
        Access::Take => "useViewPutTake",
        Access::Use => "useUse",
    };
    Some(allowed(ctx, &s, ctx.store.read(|db| deploy_flag(db, s.id, column))))
}
