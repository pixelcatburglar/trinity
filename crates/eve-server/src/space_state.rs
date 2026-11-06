//! Atomic player motion checkpoints. The live snapshot is taken inside the
//! store write that checks inventory, so no other write can carry a pre-jump
//! position across a move.
use crate::{
    ballpark::{Motion, Ship},
    combat, destiny,
    store::{Db, Store, StoreError, Tx, Value},
    svc::inflight,
    services::Ctx,
    world::World,
};
use tracing::{error, warn};

// Keep the motion fields at the top level so existing checkpoints and motion
// inspection tools remain compatible. Module custody is revalidated on restore.
#[derive(serde::Serialize,serde::Deserialize)]
struct Snapshot {
    #[serde(flatten)]
    motion:Motion,
    #[serde(default)]
    cloak:crate::cloak::SavedState,
}

/// `shipSpaceMotion` and the entity trigger that invalidates it are part of
/// the store's schema; there is nothing left to bring up to date.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// Whether the pilot still sits in that ship in space there. Checked in the
/// store write that checkpoints, so nothing can move the ship in between.
pub(crate) fn lock_current(db: &Db, pilot: i64, ship: i64, system: i64) -> Result<bool, StoreError> {
    let in_space = db.table("character_").get_id(pilot).is_some_and(|c| {
        c.int("shipID") == Some(ship) && c.int("solarSystemID") == Some(system) && c.int("stationID").unwrap_or(0) == 0
    });
    Ok(in_space
        && db.table("entity").get_id(ship).is_some_and(|e| {
            e.int("ownerID") == Some(pilot)
                && e.int("locationID") == Some(system)
                && e.int("flag").unwrap_or(0) == 0
                && e.int("quantity").is_some_and(|q| q > 0)
        }))
}

fn write(
    tx: &mut Tx<'_>,
    system: i64,
    ship: &Ship,
    tick: i64,
) -> Result<bool, StoreError> {
    if crate::losses::pending::blocked(tx.db(),ship.id)? {return Ok(false);}
    // The inactive profile loaded after restart can have a different hull
    // maximum. Preserve a confirmed lethal result, including attribution,
    // rather than interpreting its raw damage against that new maximum.
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max.is_finite() && f.vitals.hull_max>0.0
        && f.vitals.hull_damage.is_finite() && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return crate::losses::journal::stage(tx,&crate::losses::Loss::of(ship,system,ship.pilot.type_id==670));
    }
    write_snapshot(tx,system,ship,tick)
}

/// The travel journal has already validated its exact generation. Only that
/// owner may write through its own unresolved arrival marker.
pub(crate) fn write_arrival(tx:&mut Tx<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,StoreError> {
    if crate::losses::pending::blocked_except_travel(tx.db(),ship.id)?
        || ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0
            || !f.vitals.hull_damage.is_finite() || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {return Ok(false);}
    write_snapshot(tx,system,ship,tick)
}

/// Only the portal journal holding the exact receipt and custody may
/// checkpoint through its own marker. The source remains live during startup.
pub(crate) fn write_portal(tx:&mut Tx<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,StoreError> {
    if crate::losses::pending::blocked_except_portal(tx.db(),ship.id)?
        || ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0
            || !f.vitals.hull_damage.is_finite() || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {return Ok(false);}
    write_snapshot(tx,system,ship,tick)
}

pub(crate) fn write_logout(tx:&mut Tx<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,StoreError> {
    if crate::losses::pending::blocked_except_logout(tx.db(),ship.id)? {return Ok(false);}
    if ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0 || !f.vitals.hull_damage.is_finite()) {return Ok(false);}
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return crate::losses::journal::stage(tx,&crate::losses::Loss::of(ship,system,ship.pilot.type_id==670));
    }
    write_snapshot(tx,system,ship,tick)
}

fn write_snapshot(tx:&mut Tx<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,StoreError> {
    let motion = ship.motion(tick);
    // Validate before touching inventory. Corrupt physics must not become the
    // only durable location a character can load.
    if !ship.clone().restore_motion(&motion, tick) {
        return Ok(false);
    }
    let Ok(state) = serde_json::to_string(&Snapshot {motion,cloak:crate::cloak::snapshot(ship)}) else {
        return Ok(false);
    };
    let saved_at = combat::now_ms();
    if let Some(hull) = tx.table("entity").get_id(ship.id).map(|r| r.id) {
        tx.update("entity", hull, &[("x", Value::Float(ship.pos.x)), ("y", Value::Float(ship.pos.y)), ("z", Value::Float(ship.pos.z))])?;
    }
    if let Some(fighter) = &ship.fight {
        inflight::write_vitals_at(tx, ship.id, fighter, false, saved_at)?;
    }
    crate::cloak::durability::save_history(tx,ship,saved_at)?;
    let sets = [("characterID", Value::Int(ship.pilot.char_id)), ("solarSystemID", Value::Int(system)), ("tick", Value::Int(tick)),
        ("savedAt", Value::Int(saved_at)), ("state", Value::Text(state.into()))];
    let mut row = vec![("itemID", Value::Int(ship.id))];
    row.extend(sets.iter().cloned());
    tx.upsert("shipSpaceMotion", &["itemID"], &row, &sets)?;
    Ok(true)
}

/// Why a checkpoint's write was undone: refused (as a transaction ended
/// without COMMIT was), or failed.
enum Undone {
    Refused,
    Failed(StoreError),
}

impl From<StoreError> for Undone {
    fn from(e: StoreError) -> Self {
        Undone::Failed(e)
    }
}

fn saved(result: Result<(), Undone>) -> Result<bool, StoreError> {
    match result {
        Ok(()) => Ok(true),
        Err(Undone::Refused) => Ok(false),
        Err(Undone::Failed(e)) => Err(e),
    }
}

pub async fn checkpoint(world: &World, store: &Store) {
    let candidates: Vec<_> = {
        let world = world.lock();
        world
            .parks
            .iter()
            .flat_map(|(system, park)| {
                park.ships
                    .values()
                    .filter(|s| s.is_player() && s.client_id != 0 && !s.identity_reserved)
                    .map(|s| (*system, s.id, s.pilot.char_id, s.client_id))
            })
            .collect()
    };
    for (system, id, pilot, client) in candidates {
        if let Err(e) = checkpoint_one(world, store, system, id, pilot, client).await {
            error!("checkpointing ship {id}: {}", crate::logging::chain(&e));
        }
    }
}

pub(crate) async fn checkpoint_one(
    world: &World,
    store: &Store,
    system: i64,
    id: i64,
    pilot: i64,
    client: i64,
) -> Result<bool, StoreError> {
    saved(store.write(|tx| {
        if !lock_current(tx.db(), pilot, id, system)? {
            return Err(Undone::Refused);
        }
        let snapshot = {
            let world = world.lock();
            world.parks.get(&system).and_then(|park| {
                park.ships
                    .get(&id)
                    .filter(|s| {
                        s.is_player()
                            && !s.identity_reserved
                            && s.client_id == client
                            && s.pilot.char_id == pilot
                    })
                    .map(|ship| (ship.clone(), park.tick))
            })
        };
        let Some((ship, tick)) = snapshot else {
            return Err(Undone::Refused);
        };
        if !write(tx, system, &ship, tick)? {
            return Err(Undone::Refused);
        }
        Ok(())
    }))
}

/// Called only for a reconnect, not when undocking, boarding or jumping.
pub async fn restore(
    ctx: &Ctx<'_>,
    system: i64,
    ship: &mut Ship,
) -> Result<bool, StoreError> {
    let (ship_id, pilot) = (ship.id, ship.pilot.char_id);
    let row = ctx.store.read(|db| {
        let m = db.table("shipSpaceMotion").get_id(ship_id)?;
        if m.int("characterID") != Some(pilot) || m.int("solarSystemID") != Some(system) {
            return None;
        }
        let e = db.table("entity").get_id(ship_id)?;
        if e.int("ownerID") != Some(pilot) || e.int("locationID") != Some(system) || e.int("flag").unwrap_or(0) != 0 {
            return None;
        }
        let c = db.table("character_").get_id(pilot)?;
        if c.int("shipID") != Some(ship_id) || c.int("solarSystemID") != Some(system) || c.int("stationID").unwrap_or(0) != 0 {
            return None;
        }
        Some((m.text("state")?.to_string(), m.int("tick")?, m.int("savedAt")?))
    });
    let Some((raw, tick, saved_at)) = row else {
        return Ok(false);
    };
    let snapshot = if raw.len() <= 16_384 {
        serde_json::from_str::<Snapshot>(&raw).ok()
    } else {
        None
    };
    let now = combat::now_ms();
    let Some(snapshot) =
        snapshot.filter(|m| m.motion.tick == tick && saved_at >= 0 && saved_at <= now)
    else {
        warn!(
            ship = ship.id,
            "invalid durable motion; using stored position"
        );
        return Ok(false);
    };
    if !ship.restore_motion(&snapshot.motion, destiny::tick_now()) {
        warn!(
            ship = ship.id,
            "invalid durable trajectory; using stored position"
        );
        return Ok(false);
    }
    if let Some(fighter) = ship.fight.as_mut() {
        let stored: Vec<(i64, Option<f64>)> = ctx.store.read(|db| {
            db.table("entity_attributes")
                .find(&["itemID"], &[Value::Int(ship_id)])
                .filter(|r| matches!(r.int("attributeID"), Some(18 | 264)))
                .map(|r| (r.int("attributeID").unwrap_or(0), r.float("valueFloat").or(r.float("valueInt"))))
                .collect()
        });
        for (attribute, value) in stored {
            let Some(value)=value.filter(|v|v.is_finite()) else { continue; };
            match attribute as i32 {
                combat::attr::CHARGE=>fighter.vitals.capacitor.set(saved_at,value),
                combat::attr::SHIELD_CHARGE=>fighter.vitals.shield.set(saved_at,value),
                _=>{}
            }
        }
    }
    inflight::restore_cloak(ctx,ship,snapshot.cloak).await?;
    Ok(true)
}
