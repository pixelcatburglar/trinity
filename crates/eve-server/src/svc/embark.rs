//! Capsule ejection and boarding abandoned ships. Containment, ownership and
//! the active ship change in one transaction; a reserved live hull cannot be
//! boarded by two pilots or destroyed halfway through the transfer.

use std::collections::BTreeSet;
use tracing::warn;

use crate::ballpark::{Ship, Vec3};
use crate::items::{ItemTxn, NewItem};
use crate::services::Ctx;
use crate::store::{self, Db, StoreError};
use super::{inflight, moves::{refuse, MoveError}, space};

const CAPSULE: i64 = 670;
/// Trinity const.maxBoardingDistance.
const BOARD_RANGE: f64 = 6550.0;

pub fn can_fly(db: &Db, pilot: i64, type_id: i64) -> Result<(), MoveError> {
    let required = super::fitting::required_skill_levels(db, type_id)?;
    let types: Vec<_> = required.keys().copied().collect();
    let trained = super::skills::levels(db, pilot, &types)?;
    for (skill, level) in required {
        if trained.get(&skill).copied().unwrap_or(0) < level {
            return refuse(format!("You need skill {skill} trained to level {level} to pilot this ship."));
        }
    }
    Ok(())
}

/// An empty hull keeps its property owner but receives no former pilot's
/// skills or implants. Its pilot field is only ownership metadata.
pub(crate) fn empty_profile(db: &Db, ctx: &Ctx<'_>, hull: &Ship) -> Result<Ship, MoveError> {
    let Some(profile) = inflight::profile(db, ctx.dogma, 0, hull.id, &BTreeSet::new(), crate::combat::now_ms())? else {
        return refuse("The abandoned ship could not be loaded.");
    };
    let mut empty = hull.clone();
    let owner = hull.pilot.clone();
    inflight::apply_profile(&mut empty, profile, crate::combat::now_ms());
    empty.pilot.char_id = owner.char_id;
    empty.pilot.corp_id = owner.corp_id;
    empty.pilot.alliance_id = owner.alliance_id;
    empty.pilot.security = owner.security;
    empty.pilot.bounty = owner.bounty;
    empty.client_id = 0;
    empty.unpiloted = true;
    empty.cloaked = false;
    empty.arrive(hull.pos);
    if let Some(f) = &mut empty.fight { f.modules.clear(); f.locks.clear(); f.applied.clear(); }
    Ok(empty)
}

/// Reconstruct only ships that are nobody's active hull. Offline characters'
/// active ships remain owned by their login path, never become boardable.
pub async fn load_empty(ctx: &Ctx<'_>, system: i64) -> Result<Vec<Ship>, StoreError> {
    let ships = crate::sde::get().types_in_category(6);
    ctx.store.read(|db| {
        let characters = db.table("character_");
        let corporations = db.table("corporation");
        let mut empty = Vec::new();
        for e in db.table("entity").find(&["locationID", "flag"], &[store::Value::Int(system), store::Value::Int(0)]) {
            let (Some(id), Some(owner), Some(type_id)) = (e.int("itemID"), e.int("ownerID"), e.int("typeID")) else { continue };
            if e.int("singleton") != Some(1) || !ships.contains(&(type_id as i32)) { continue; }
            if characters.find(&["shipID"], &[store::Value::Int(id)]).next().is_some() { continue; }
            let pos = Vec3::new(e.float("x").unwrap_or(0.0), e.float("y").unwrap_or(0.0), e.float("z").unwrap_or(0.0));
            if !pos.is_finite() { continue; }
            let Some(mut p) = inflight::profile(db, ctx.dogma, 0, id, &BTreeSet::new(), crate::combat::now_ms())? else { continue; };
            let corp = characters.get_id(owner).and_then(|c| c.int("corporationID"));
            p.pilot.char_id = owner;
            p.pilot.corp_id = corp.unwrap_or(0);
            p.pilot.alliance_id = corp.and_then(|corp| corporations.get_id(corp)).and_then(|co| co.int("allianceID"));
            let mut ship = Ship::new(id, 0, p.stats, p.pilot, pos, Vec3::ZERO);
            ship.fight = Some(p.fighter);
            ship.unpiloted = true;
            empty.push(ship);
        }
        Ok(empty)
    })
}

fn ready_in(ctx: &Ctx<'_>, w: &crate::world::WorldState, system: i64, current: i64, target: Option<i64>, reserved: bool) -> Result<(), MoveError> {
    let Some(park) = w.parks.get(&system) else { return refuse("Your ship is not in space."); };
    let Some(ship) = park.ships.get(&current).filter(|s| s.is_player() && s.client_id == ctx.client_id && Some(s.pilot.char_id) == ctx.session.char_id) else {
        return refuse("You are not piloting that ship.");
    };
    if ship.warping() || ship.cloaked { return refuse("You cannot change ships while warping or cloaked."); }
    if ship.identity_reserved != reserved { return refuse("That ship is already changing pilots."); }
    if crate::svc::jumps::portal::active_until_locked(w,current).is_some() {
        return refuse("Your jump portal must finish its active cycle before you can change ships.");
    }
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return refuse("Your ship has been destroyed.");
    }
    if ship.fight.as_ref().is_some_and(|f| f.modules.iter().any(|m| matches!(m.action, crate::combat::Action::Cyno))) {
        return refuse("Deactivate your cynosural field before ejecting.");
    }
    if let Some(target) = target {
        let Some(hull) = park.ships.get(&target).filter(|s| s.unpiloted && !s.warping() && !s.cloaked) else {
            return refuse("You can only board an abandoned ship.");
        };
        if hull.identity_reserved != reserved { return refuse("That ship is already changing pilots."); }
        if park.surface_distance(current, target).map_or(true,|d| !d.is_finite() || d > BOARD_RANGE) {
            return refuse(format!("Get within {BOARD_RANGE} metres to board that ship."));
        }
        if hull.fight.as_ref().is_some_and(|f| f.vitals.hull_damage >= f.vitals.hull_max-1e-6) {
            return refuse("That ship is too damaged to board.");
        }
    } else if ship.pilot.type_id == CAPSULE {
        return refuse("You cannot eject from your capsule.");
    }
    Ok(())
}

fn ready(ctx: &Ctx<'_>, system: i64, current: i64, target: Option<i64>) -> Result<(), MoveError> {
    ready_in(ctx,&ctx.world.lock(),system,current,target,false)
}

/// Change the cockpit profile without rewinding damage, charge or position
/// accumulated while the transfer was prepared.
pub(crate) fn with_profile(live: &Ship, prepared: &Ship) -> Ship {
    let mut ship=live.clone();
    if let Some(fighter)=&prepared.fight {
        inflight::apply_profile(&mut ship,inflight::Profile {stats:prepared.stats,pilot:prepared.pilot.clone(),fighter:fighter.clone()},crate::combat::now_ms());
    }
    ship.client_id=prepared.client_id;ship.unpiloted=prepared.unpiloted;ship.cloaked=false;
    ship.arrive(live.pos);
    if let Some(f)=&mut ship.fight {f.modules.clear();f.locks.clear();f.applied.clear();}
    ship
}

/// None ejects into a newly created capsule. Some boards an empty hull, leaving
/// the current hull behind (an emptied capsule is removed).
pub async fn switch(ctx: &Ctx<'_>, target: Option<i64>) -> Result<i64, MoveError> {
    let (Some(pilot), Some(current), Some(system), None) =
        (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id, ctx.session.station_id) else {
            return refuse("You must be in space to change ships this way.");
        };
    if target == Some(current) { return Ok(current); }
    ready(ctx, system, current, target)?;
    let mut recipients = vec![ctx.client_id];
    // Whether this transfer reserved the live hulls, which a failure must
    // release (and only then: another transfer may hold them).
    let mut reserved = false;
    let done = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        if !super::login::lock_character_lifecycle(txn.db(), pilot)? {
            return refuse("That character is no longer available.");
        }
        let state = txn.db().table("character_").get_id(pilot).filter(|c| {
            c.int("shipID") == Some(current) && c.int("stationID") == Some(0) && c.int("solarSystemID") == Some(system)
        });
        if state.is_none() { return refuse("Your ship or location changed. Try again."); }
        let mut ids = vec![current];
        ids.extend(target);
        ids.sort_unstable();
        for id in ids { if txn.item(id)?.is_none() { return refuse("That ship no longer exists."); } }
        let old = txn.item(current)?.unwrap();
        if old.owner_id != pilot || old.location_id != system { return refuse("You do not control that ship."); }
        if let Some(target) = target {
            let hull = txn.item(target)?.unwrap();
            if hull.category_id != 6 || !hull.singleton || hull.flag != 0 || hull.location_id != system {
                return refuse("You can only board an assembled ship here in space.");
            }
            if txn.db().table("character_").find(&["shipID"], &[store::Value::Int(target)]).next().is_some() {
                return refuse("That ship already has a pilot.");
            }
            can_fly(txn.db(), pilot, hull.type_id)?;
            recipients.extend(ctx.world.lock().clients_of(hull.owner_id));
        }
        // Revalidate against the live ships, then reserve both live hulls in
        // one short world-lock section.
        ready(ctx, system, current, target)?;
        let (old_live, target_live) = {
            let mut w = ctx.world.lock();
            ready_in(ctx,&w,system,current,target,false)?;
            let Some(park) = w.parks.get_mut(&system) else { return refuse("The ballpark changed."); };
            let old=park.ships.get_mut(&current).unwrap();old.identity_reserved=true;
            let old=old.clone();
            let target=target.map(|id| {let hull=park.ships.get_mut(&id).unwrap();hull.identity_reserved=true;hull.clone()});
            (old,target)
        };
        reserved = true;
        if let Some(f) = &old_live.fight { inflight::write_vitals(txn.tx(), current, f, false)?; }
        txn.set_position(current, old_live.pos.tuple())?;
        let next = match target_live.as_ref() {
            Some(hull) => {
                if let Some(f) = &hull.fight { inflight::write_vitals(txn.tx(), hull.id, f, false)?; }
                txn.set_position(hull.id, hull.pos.tuple())?;
                // All carried property follows the hull; each child is
                // reread so only what is still inside is handed over.
                let mut parents = vec![hull.id];
                let mut seen = BTreeSet::new();
                while let Some(parent) = parents.pop() {
                    if !seen.insert(parent) { continue; }
                    txn.set_owner(parent, pilot)?;
                    let mut children: Vec<i64> = txn.db().table("entity").find(&["locationID"], &[store::Value::Int(parent)]).filter_map(|e| e.int("itemID")).collect();
                    children.sort_unstable();
                    for id in children {
                        if id == pilot { continue; }
                        if txn.item(id)?.is_some_and(|i| i.location_id==parent) { parents.push(id); }
                    }
                }
                hull.id
            }
            None => {
                let id = txn.create(NewItem { type_id: CAPSULE, owner_id: pilot, location_id: system, flag: 0, quantity: 1, singleton: true })?;
                txn.set_position(id, old_live.pos.tuple())?;
                id
            }
        };
        let mut empty = if old.type_id == CAPSULE {
            txn.destroy(current)?;
            None
        } else {
            Some(empty_profile(txn.db(), ctx, &old_live)?)
        };
        if let Some(empty) = &mut empty {
            if let Some(f) = &empty.fight { inflight::write_vitals(txn.tx(), current, f, false)?; }
        }
        set_ship(txn.tx(), pilot, next)?;
        // Character inventory location follows the station/system convention
        // used by docking, jumps and clone transfers; shipID is the cockpit.
        if let Some(row) = txn.db().table("entity").get_id(pilot).map(|e| e.id) {
            txn.tx().update("entity", row, &[("locationID", store::Value::Int(system))])?;
        }
        let Some(profile) = inflight::profile(txn.db(), ctx.dogma, pilot, next, &BTreeSet::new(), crate::combat::now_ms())? else {
            return refuse("The new ship profile could not be loaded.");
        };
        let pos = target_live.as_ref().map_or(old_live.pos, |s|s.pos);
        let mut ship = Ship::new(next, ctx.client_id, profile.stats, profile.pilot, pos, Vec3::ZERO);
        ship.fight = Some(profile.fighter);
        if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max) {
            return refuse("That ship is too damaged for you to board.");
        }
        // Freeze the final durable snapshot as late as possible. The hulls
        // stayed visible and combat continued while the transfer was prepared.
        let (latest_old,latest_target)={
            let mut w=ctx.world.lock();
            if let Some(park)=w.parks.get_mut(&system) {park.advance(crate::destiny::tick_now());}
            ready_in(ctx,&w,system,current,target,true)?;
            let park=&w.parks[&system];
            (park.ships[&current].clone(),target.map(|id|park.ships[&id].clone()))
        };
        if let Some(prepared)=&empty {empty=Some(with_profile(&latest_old,prepared));}
        if let Some(latest)=&latest_target {ship=with_profile(latest,&ship);} else {ship.arrive(latest_old.pos);}
        if let Some(hull)=&empty {
            txn.set_position(current,hull.pos.tuple())?;
            if let Some(f)=&hull.fight {inflight::write_vitals(txn.tx(),current,f,false)?;}
        }
        txn.set_position(next,ship.pos.tuple())?;
        if let Some(f)=&ship.fight {inflight::write_vitals(txn.tx(),next,f,false)?;}
        // A lethal hit or range change while the final writes were made
        // aborts the transfer; the next heartbeat processes the actual
        // destruction.
        ready_in(ctx,&ctx.world.lock(),system,current,target,true)?;
        Ok::<_, MoveError>((ship, empty, old_live, txn.events()))
    });
    match done {
        Ok((mut ship, mut empty, old_live, events)) => {
            let next = ship.id;
            let mut w = ctx.world.lock();
            // Keep damage, charge and position changes that happened during
            // the transaction; a failed request never removes live balls.
            if let Some(park)=w.parks.get_mut(&system) {
                park.advance(crate::destiny::tick_now());
                if let Some(live)=park.ships.get_mut(&current) {live.identity_reserved=false;}
                if let Some(id)=target {if let Some(live)=park.ships.get_mut(&id) {live.identity_reserved=false;}}
            }
            let live_old=w.parks.get(&system).and_then(|p|p.ships.get(&current)).cloned().unwrap_or(old_live);
            if let Some(prepared)=empty.take() {
                let mut live=with_profile(&live_old,&prepared);live.identity_reserved=false;
                empty=Some(live);
            }
            if let Some(id)=target {
                if let Some(live)=w.parks.get(&system).and_then(|p|p.ships.get(&id)) {
                    let mut live=with_profile(live,&ship);live.identity_reserved=false;
                    ship=live;
                }
            } else {ship.arrive(live_old.pos);}
            // Complete normal departure bookkeeping after the durable transfer.
            space::remove_ship(&mut w, system, current);
            if let Some(id)=target {space::remove_ship(&mut w,system,id);}
            if let Some(empty) = empty { space::add_ship(&mut w, system, empty); }
            space::add_ship(&mut w, system, ship);
            recipients.sort_unstable(); recipients.dedup();
            for event in events { w.notify(&recipients, "OnItemChange", event); }
            Ok(next)
        }
        Err(error) => {
            if reserved {
                let mut w = ctx.world.lock();
                if let Some(park)=w.parks.get_mut(&system) {
                    if let Some(ship)=park.ships.get_mut(&current) {ship.identity_reserved=false;}
                    if let Some(id)=target {if let Some(ship)=park.ships.get_mut(&id) {ship.identity_reserved=false;}}
                }
            }
            Err(error)
        }
    }
}

/// The character's active ship is now `ship`.
fn set_ship(tx: &mut store::Tx<'_>, pilot: i64, ship: i64) -> Result<(), StoreError> {
    match tx.table("character_").get_id(pilot).map(|c| c.id) {
        Some(row) => tx.update("character_", row, &[("shipID", store::Value::Int(ship))]),
        None => Ok(()),
    }
}

/// Station Board and Evacuate use the same ownership and skill checks, without
/// publishing an abandoned hull into space. An emptied capsule is consumed.
pub async fn station_switch(ctx: &Ctx<'_>, target: Option<i64>) -> Result<i64, MoveError> {
    let (Some(pilot), Some(current), Some(station)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.station_id) else {
        return refuse("You must be docked to use the station hangar.");
    };
    if target == Some(current) { return Ok(current); }
    let switched=ctx.store.write(|tx| {
        let mut txn=ItemTxn::new(tx);
        if !super::login::lock_character_lifecycle(txn.db(),pilot)? { return refuse("That character is no longer available."); }
        if txn.db().table("character_").get_id(pilot).filter(|c|c.int("shipID")==Some(current) && c.int("stationID")==Some(station)).is_none() {
            return refuse("Your ship or station changed. Try again.");
        }
        let mut ids=vec![current];ids.extend(target);ids.sort_unstable();
        for id in ids { if txn.item(id)?.is_none() { return refuse("That ship no longer exists."); } }
        let old=txn.item(current)?.unwrap();
        if old.owner_id!=pilot || old.location_id!=station || old.flag!=4 || !old.singleton || old.category_id!=6 {
            return refuse("You do not control that ship in this station.");
        }
        if target.is_none() && old.type_id==CAPSULE { return Ok(None); }
        let next=if let Some(target)=target {
            let hull=txn.item(target)?.unwrap();
            if hull.owner_id!=pilot || hull.location_id!=station || hull.flag!=4 || !hull.singleton || hull.category_id!=6 {
                return refuse("You can only board your assembled ships in this station.");
            }
            if txn.db().table("character_").find(&["shipID"],&[store::Value::Int(target)]).any(|c|c.int("characterID")!=Some(pilot)) {
                return refuse("That ship already has a pilot.");
            }
            can_fly(txn.db(),pilot,hull.type_id)?;
            target
        } else {
            txn.create(NewItem {type_id:CAPSULE,owner_id:pilot,location_id:station,flag:4,quantity:1,singleton:true})?
        };
        if old.type_id==CAPSULE { txn.destroy(current)?; }
        set_ship(txn.tx(),pilot,next)?;
        Ok::<_, MoveError>(Some((next,txn.events())))
    })?;
    let Some((next,events))=switched else { return Ok(current) };
    crate::items::notify(ctx.world,ctx.client_id,events);
    Ok(next)
}

pub fn error_text(error: MoveError) -> String {
    match error {
        MoveError::Refused(reason) => reason,
        MoveError::Reactivation{delay,..}=>format!("The module is still recovering for {:.1} seconds.",delay as f64/1000.0),
        MoveError::Db(error) => { warn!("changing ship: {}", crate::logging::chain(&error)); "Your ship could not be changed.".into() }
    }
}
