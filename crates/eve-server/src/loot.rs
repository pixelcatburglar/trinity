//! What is left floating after a fight: cargo containers holding a rat's loot
//! or what survived of a player's ship.
//!
//! A can is a real item in the solar system (with its position in the entity
//! row), so it outlives a restart; its contents are ordinary items inside it,
//! opened and emptied through the usual inventory calls. Anyone close enough
//! may take from it, and what they take becomes theirs. A can goes when it is
//! empty or two hours after it was dropped.
//!
//! Rat loot is drawn from the market's modules and charges, priced to the
//! rat's bounty.

use std::sync::OnceLock;

use eve_marshal::Value;
use tracing::{error, info};

use crate::ballpark::{Park, Vec3};
use crate::combat::Rng;
use crate::heartbeat::Out;
use crate::items::{ItemTxn, NewItem};
use crate::store::{Db, Store, StoreError};
use crate::svc::space::{self, entry};
use crate::world::World;

pub const TYPE_CARGO_CONTAINER: i64 = 23;
const GROUP_CARGO_CONTAINER: i32 = 12;
/// How close a ship must be to take from a can (const.maxCargoContainerTransferDistance).
pub const TRANSFER_RANGE: f64 = 2_500.0;
const LIFETIME_MS: i64 = 2 * 3_600_000;
const CATEGORY_MODULE: i32 = 7;
const CATEGORY_CHARGE: i32 = 8;

#[derive(Debug, Clone)]
pub struct Can {
    pub id: i64,
    pub type_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    pub owner_id: i64,
    pub expires_ms: i64,
}

/// What goes into a new can: something new, or an item that already exists.
#[derive(Debug, Clone)]
pub enum Stash {
    New { type_id: i64, quantity: i64 },
    Existing(i64),
}

pub fn slim(can: &Can) -> Value {
    space::slim(vec![
        ("itemID", Value::Int(can.id)),
        ("typeID", Value::Int(can.type_id)),
        ("ownerID", Value::Int(can.owner_id)),
    ])
}

// ---- loot tables -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LootType {
    type_id: i64,
    price: f64,
    charge: bool,
    portion: i64,
}

static TABLE: OnceLock<Vec<LootType>> = OnceLock::new();

/// Reads what rats can drop, once. The static data is in memory; the
/// data handle and the error stay for the caller.
pub fn ensure_table(_db: &Db) -> Result<(), StoreError> {
    if TABLE.get().is_some() {
        return Ok(());
    }
    let sde = crate::sde::get();
    let mut table = sde
        .table("invTypes")
        .rows()
        .filter_map(|r| {
            let type_id = r.int("typeID")?;
            let (_, category) = sde.group_and_category(type_id)?;
            let price = r.float("basePrice").filter(|p| *p > 0.0)?;
            if ![CATEGORY_MODULE, CATEGORY_CHARGE].contains(&(category as i32)) || r.int("marketGroupID").is_none() {
                return None;
            }
            Some(LootType {
                type_id,
                price,
                charge: category as i32 == CATEGORY_CHARGE,
                portion: r.int("portionSize").unwrap_or(1).max(1),
            })
        })
        .collect::<Vec<_>>();
    // Cheapest first; equal prices stay in the static data's order.
    table.sort_by(|a, b| a.price.total_cmp(&b.price));
    info!("loot table: {} types", table.len());
    let _ = TABLE.set(table);
    Ok(())
}

/// What a rat worth `bounty` drops: some modules and charges worth about
/// what it was, or nothing at all.
pub fn roll(bounty: f64, count: (usize, usize), rng: &mut Rng) -> Vec<Stash> {
    let Some(table) = TABLE.get() else { return Vec::new() };
    if rng.unit() < 0.3 {
        return Vec::new();
    }
    let (lo, hi) = if count.1 > 0 { (count.0.max(1), count.1.max(count.0.max(1))) } else { (1, 2) };
    let n = lo + rng.below(hi - lo + 1);
    let affordable = |t: &&LootType| {
        if t.charge { t.price * 100.0 <= bounty.max(1_000.0) } else { t.price <= bounty.max(1_000.0) * 1.5 }
    };
    let pool: Vec<&LootType> = table.iter().filter(affordable).collect();
    if pool.is_empty() {
        return Vec::new();
    }
    (0..n)
        .map(|_| {
            // Cheaper things are commoner: the better of two picks from the bottom.
            let t = pool[rng.below(pool.len()).min(rng.below(pool.len()))];
            let quantity = if t.charge { t.portion * (20 + rng.below(81) as i64) } else { 1 };
            Stash::New { type_id: t.type_id, quantity }
        })
        .collect()
}

// ---- cans in the park --------------------------------------------------------------------

/// The cans already floating in a system, for a new park.
pub fn load_cans(db: &Db, solar_system_id: i64, now: i64) -> Result<Vec<Can>, StoreError> {
    let containers = crate::sde::get().types_in_group(GROUP_CARGO_CONTAINER as i64);
    let types = crate::sde::table("invTypes");
    Ok(db
        .table("entity")
        .find(&["locationID"], &[crate::store::Value::Int(solar_system_id)])
        .filter(|r| r.int("typeID").is_some_and(|t| containers.contains(&(t as i32))))
        .filter_map(|r| {
            let type_id = r.int("typeID")?;
            let t = types.get("typeID", type_id)?;
            Some(Can {
                id: r.int("itemID").unwrap_or(0),
                type_id,
                name: r
                    .text("itemName")
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| t.text("typeName").unwrap_or_default())
                    .to_string(),
                pos: Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0)),
                radius: t.float("radius").unwrap_or(10.0),
                owner_id: r.int("ownerID").unwrap_or(0),
                expires_ms: now + LIFETIME_MS,
            })
        })
        .collect())
}

/// Cans past their time go, and their contents with them.
pub fn expire(park: &mut Park, now: i64, out: &mut Out) {
    let old: Vec<i64> = park.cans.values().filter(|c| c.expires_ms <= now).map(|c| c.id).collect();
    for id in old {
        park.cans.remove(&id);
        out.destiny(park.solar_system_id, entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
        out.jobs.push(crate::heartbeat::Job::Scrap { can_id: id });
    }
}

/// Whether `ship_id` is close enough to `can_id` to take from it.
pub fn in_reach(world: &World, solar_system_id: i64, ship_id: i64, can_id: i64) -> bool {
    let w = world.lock();
    let Some(park) = w.parks.get(&solar_system_id) else { return false };
    let (Some(can), Some(ship)) = (park.cans.get(&can_id), park.ships.get(&ship_id)) else { return false };
    (can.pos - ship.pos).len() - can.radius - ship.stats.radius <= TRANSFER_RANGE * 1.25
}

pub fn is_can(world: &World, solar_system_id: i64, item_id: i64) -> bool {
    world.lock().parks.get(&solar_system_id).is_some_and(|p| p.cans.contains_key(&item_id))
}

// ---- dropping and clearing -------------------------------------------------------------------

/// Puts a can at `pos` holding `stash`, owned by `owner_id`, and shows it to
/// everyone in the system.
pub async fn drop_can(store: &Store, world: &World, solar_system_id: i64, pos: Vec3, owner_id: i64, stash: Vec<Stash>) {
    if stash.is_empty() {
        return;
    }
    let result = store.write(|tx| -> Result<_, StoreError> {
        let mut items = ItemTxn::new(tx);
        let can = stage_can(&mut items, solar_system_id, pos, owner_id, &stash)?.expect("nonempty stash");
        Ok((can, items.events()))
    });
    let (can, events) = match result {
        Ok(v) => v,
        Err(e) => {
            error!("dropping a can in {solar_system_id}: {}", crate::logging::chain(&e));
            return;
        }
    };
    {
        let w = world.lock();
        for client in w.clients_of(owner_id) {
            for event in &events { w.notify(&[client], "OnItemChange", event.clone()); }
        }
    }
    show_can(world, solar_system_id, can);
}

/// Stage surviving cargo with its container in the caller's transaction, so
/// a ship cannot disappear while its cargo is left loose in the solar system.
pub(crate) fn stage_can(items: &mut ItemTxn<'_, '_>, solar_system_id: i64, pos: Vec3, owner_id: i64, stash: &[Stash]) -> Result<Option<Can>, StoreError> {
    if stash.is_empty() { return Ok(None); }
    let can = items
            .create(NewItem {
                type_id: TYPE_CARGO_CONTAINER,
                owner_id,
                location_id: solar_system_id,
                flag: 0,
                quantity: 1,
                singleton: true,
            })?;
        items.set_position(can, pos.tuple())?;
        for s in stash {
            match *s {
                Stash::New { type_id, quantity } => {
                    let singleton = false;
                    items.create(NewItem { type_id, owner_id, location_id: can, flag: 0, quantity, singleton })?;
                }
                Stash::Existing(id) => items.move_to(id, can, 0)?,
            }
        }
        let name: String = crate::sde::get().type_(TYPE_CARGO_CONTAINER).map(|t| t.name.to_string()).unwrap_or_default();
    Ok(Some(Can { id: can, type_id: TYPE_CARGO_CONTAINER, name, pos, radius: 14.0, owner_id, expires_ms: crate::combat::now_ms() + LIFETIME_MS }))
}

pub(crate) fn show_can(world: &World, solar_system_id: i64, can: Can) {
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&solar_system_id) else { return };
    let can_id = can.id;
    let blob = crate::destiny::encode(crate::destiny::stamp(park.tick), &[crate::destiny::Ball::celestial(can.id, can.radius, can.pos.tuple(), &can.name, false)]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![slim(&can)]), Value::Dict(Vec::new())]);
    let add = entry(park.tick, "AddBalls", vec![chunk]);
    park.cans.insert(can_id, can);
    space::broadcast(&w, solar_system_id, None, vec![add]);
    info!("can {can_id} dropped in {solar_system_id}");
}

/// After items left some containers: any of them that is a can in space and
/// now empty goes.
pub async fn clear_emptied(store: &Store, world: &World, solar_system_id: i64, origins: &[i64]) {
    let cans: Vec<i64> = origins.iter().copied().filter(|id| is_can(world, solar_system_id, *id)).collect();
    for can in cans {
        // Still empty when it goes: nothing can be put in between.
        let removed = store.write(|tx| {
            let entity = tx.table("entity");
            if entity.find_one(&["locationID"], &[crate::store::Value::Int(can)]).is_some() {
                return Ok(false);
            }
            if let Some(row) = entity.get_id(can).map(|r| r.id) {
                tx.delete("entity", row)?;
            }
            Ok::<_, StoreError>(true)
        });
        match removed {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                error!("removing empty can {can}: {}", crate::logging::chain(&e));
                continue;
            }
        }
        let mut w = world.lock();
        if let Some(park) = w.parks.get_mut(&solar_system_id) {
            park.cans.remove(&can);
            let gone = entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(can)])]);
            space::broadcast(&w, solar_system_id, None, vec![gone]);
        }
        info!("can {can} emptied and gone");
    }
}

/// An expired can and everything in it.
pub async fn scrap(store: &Store, can_id: i64) {
    let result = store.write(|tx| ItemTxn::new(tx).destroy_tree(can_id, None, &[]));
    if let Err(e) = result {
        error!("scrapping can {can_id}: {}", crate::logging::chain(&e));
    }
}
