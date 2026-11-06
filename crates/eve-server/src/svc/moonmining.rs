//! Starbase production (moonmining.py's Production tab): moon harvesting
//! arrays mine the moon a tower stands at, silos hold what they are given,
//! and reactors turn what their silos hold into something else.
//!
//! Each harvester mines one of its moon's materials and each silo holds one
//! type, both chosen with ChangeStructureProvisionType; LinkResourceForTower
//! links what a structure puts out to the structure that takes it in
//! (`posLinks`). A reactor runs the reaction whose item sits in it. Every
//! time the tower pays for another period of fuel, its online structures
//! run one cycle: harvesters first, then reactors.
//!
//! The database has no record of what each moon holds, so it is drawn from
//! the moon's ID: up to three materials, the rarer ones less often, the
//! rarest only where security is at or below zero.

use std::collections::HashMap;
use std::sync::OnceLock;

use eve_marshal::Value;
use tracing::{error, info};

use crate::items::{ItemTxn, NewItem};
use crate::pos::state;
use crate::services::{Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError};
use crate::svc::membership::Refused;

pub const GROUP_MOON_MINING: i64 = 416;
pub const GROUP_SILO: i64 = 404;
pub const GROUP_REACTOR: i64 = 438;
const GROUP_MOON_MATERIALS: i64 = 427;
const ATTR_CARGO_GROUP: i32 = 629;
const ATTR_HARVESTER_QUALITY: i32 = 710;
const ATTR_MOON_MINING_AMOUNT: i32 = 726;
const ATTR_REACTION_GROUP: [i32; 2] = [842, 843];
const ATTR_SILO_CAPACITY_BONUS: i32 = 757;
/// Moon material tiers by base price, with how often each turns up.
const TIERS: [(f64, u32); 5] = [(2.0, 45), (8.0, 25), (16.0, 15), (64.0, 10), (256.0, 5)];
/// How many materials a moon holds, with how often: none, one, two, three.
const COUNTS: [u32; 4] = [30, 40, 20, 10];

static MATERIALS: OnceLock<Vec<(i64, f64)>> = OnceLock::new();

/// The moon materials and their base prices, by type, read once; a material
/// the data leaves unpriced is left out.
pub fn materials() -> &'static [(i64, f64)] {
    MATERIALS.get_or_init(|| {
        let sde = crate::sde::get();
        let types = sde.table("invTypes");
        sde.types_in_group(GROUP_MOON_MATERIALS)
            .iter()
            .filter_map(|t| Some((*t as i64, types.get("typeID", *t as i64)?.float("basePrice")?)))
            .collect()
    })
}

/// What a moon holds: (material, batches per cycle).
pub fn moon_materials(moon_id: i64, security: f64, materials: &[(i64, f64)]) -> Vec<(i64, i64)> {
    let mut rng = crate::combat::Rng::seeded(moon_id as u64 ^ 0x6d6f_6f6e);
    let pick = |rng: &mut crate::combat::Rng, weights: &[u32]| {
        let total: u32 = weights.iter().sum();
        let mut roll = rng.below(total as usize) as u32;
        weights.iter().position(|w| {
            let hit = roll < *w;
            roll = roll.saturating_sub(*w);
            hit
        })
    };
    let count = pick(&mut rng, &COUNTS).unwrap_or(0);
    let mut out: Vec<(i64, i64)> = Vec::new();
    for _ in 0..count {
        let Some(mut tier) = pick(&mut rng, &TIERS.map(|(_, w)| w)) else { continue };
        if tier == TIERS.len() - 1 && security > 0.0 {
            tier -= 1;
        }
        let in_tier: Vec<i64> = materials.iter().filter(|(_, p)| (*p - TIERS[tier].0).abs() < 1e-6).map(|(t, _)| *t).collect();
        if in_tier.is_empty() {
            continue;
        }
        let t = in_tier[rng.below(in_tier.len())];
        if !out.iter().any(|(o, _)| *o == t) {
            out.push((t, 1));
        }
    }
    out
}

/// The moon a tower stands at and its system's security.
fn tower_moon(db: &Db, tower: i64) -> Option<(i64, f64)> {
    let p = db.table("posStructures").get_id(tower)?;
    let e = db.table("entity").get_id(tower)?;
    let security = e
        .int("locationID")
        .and_then(|s| db.table("mapSolarSystems").get_id(s))
        .and_then(|s| s.float("security"))
        .unwrap_or(0.0);
    Some((p.int("moonID")?, security))
}

/// GetMoonForTower(towerID): (moonID, [(typeID, batches)]); the materials
/// only once the tower is online and can scan the moon.
pub async fn moon_for_tower(ctx: &Ctx<'_>, tower: i64) -> Value {
    let (place, online) = ctx.store.read(|db| {
        let online = db
            .table("posStructures")
            .get_id(tower)
            .is_some_and(|p| matches!(p.int("state"), Some(3..=5)));
        (tower_moon(db, tower), online)
    });
    let Some((moon, security)) = place else { return Value::None };
    if !online {
        return Value::Tuple(vec![Value::Int(moon), Value::None]);
    }
    let held = moon_materials(moon, security, materials());
    let list = held.iter().map(|(t, q)| Value::Tuple(vec![Value::Int(*t), Value::Int(*q)])).collect();
    Value::Tuple(vec![Value::Int(moon), Value::List(list)])
}

/// A production structure of a tower, as the database has it.
#[derive(Debug, Clone)]
struct Producer {
    id: i64,
    group_id: i64,
    type_id: i64,
    owner_id: i64,
    online: bool,
    provision: Option<i64>,
}

/// The tower's harvesters, silos and reactors, by item ID.
fn producers(db: &Db, tower: i64) -> Vec<Producer> {
    let sde = crate::sde::get();
    let types: Vec<i32> = [GROUP_MOON_MINING, GROUP_SILO, GROUP_REACTOR].iter().flat_map(|g| sde.types_in_group(*g)).copied().collect();
    let entity = db.table("entity");
    let mut out: Vec<Producer> = db
        .table("posStructures")
        .rows()
        .filter(|p| p.int("controlTowerID") == Some(tower))
        .filter_map(|p| {
            let id = p.int("itemID")?;
            let e = entity.get_id(id)?;
            let type_id = e.int("typeID").unwrap_or(0);
            if !types.iter().any(|t| i64::from(*t) == type_id) {
                return None;
            }
            Some(Producer {
                id,
                group_id: sde.type_(type_id).map_or(0, |t| t.group_id as i64),
                type_id,
                owner_id: e.int("ownerID").unwrap_or(0),
                online: matches!(p.int("state").map(|s| s as i16), Some(state::ONLINING | state::REINFORCED | state::ONLINE | state::OPERATING)),
                provision: p.int("provisionType"),
            })
        })
        .collect();
    out.sort_by_key(|p| p.id);
    out
}

/// The reaction item sitting in a reactor, of a group the reactor runs
/// (reactionGroup1/2): (itemID, reaction typeID).
fn reaction_in(db: &Db, reactor: i64) -> Option<(i64, i64)> {
    let entity = db.table("entity");
    let reactor_type = entity.get_id(reactor)?.int("typeID")?;
    let sde = crate::sde::get();
    let reactions = sde.table("invTypeReactions");
    let runs: Vec<i32> = type_attribute_all(reactor_type, &ATTR_REACTION_GROUP)
        .into_iter()
        .flat_map(|g| sde.types_in_group(g))
        .filter(|t| reactions.get("reactionTypeID", **t as i64).is_some())
        .copied()
        .collect();
    entity
        .find(&["locationID"], &[store::Value::Int(reactor)])
        .filter_map(|e| Some((e.int("itemID")?, e.int("typeID")?)))
        .filter(|(_, t)| runs.iter().any(|r| i64::from(*r) == *t))
        .min_by_key(|(id, _)| *id)
}

/// A silo's capacity with its online tower's controlTowerSiloCapacityBonus
/// (a PostPercent on capacity for the silo group); None for anything not a
/// silo.
pub fn silo_capacity(db: &Db, silo: i64) -> Result<Option<f64>, StoreError> {
    let sde = crate::sde::get();
    let entity = db.table("entity");
    let Some(silo_type) = entity
        .get_id(silo)
        .and_then(|e| e.int("typeID"))
        .filter(|t| sde.types_in_group(GROUP_SILO).iter().any(|s| i64::from(*s) == *t))
    else {
        return Ok(None);
    };
    let structures = db.table("posStructures");
    let tower_type = structures
        .get_id(silo)
        .and_then(|p| p.int("controlTowerID"))
        .and_then(|t| structures.get_id(t))
        .filter(|tp| matches!(tp.int("state"), Some(2..=5)))
        .and_then(|tp| entity.get_id(tp.int("itemID")?))
        .and_then(|te| te.int("typeID"));
    let capacity = sde.table("invTypes").get("typeID", silo_type).and_then(|t| t.float("capacity"));
    let bonus = tower_type.and_then(|tower| sde.type_attribute(tower, ATTR_SILO_CAPACITY_BONUS)).unwrap_or(0.0);
    Ok(capacity.map(|c| c * (1.0 + bonus / 100.0)))
}

/// Why `container` will not take an item of `type_id` put in by hand, if it
/// is a silo or reactor that won't: a silo holds only its cargo group, and
/// only the type it is set to (or, unset, the one it already holds); a
/// reactor holds one reaction, of a group it runs.
pub fn refuses(db: &Db, container: i64, item: i64, type_id: i64) -> Result<Option<&'static str>, StoreError> {
    let entity = db.table("entity");
    let Some(holder) = entity.get_id(container) else { return Ok(None) };
    let sde = crate::sde::get();
    let holder_type = holder.int("typeID").unwrap_or(0);
    let provision = db.table("posStructures").get_id(container).and_then(|p| p.int("provisionType"));
    // A container of a type the data does not know is no silo or reactor.
    let Some(group) = sde.type_(holder_type).map(|t| t.group_id as i64) else { return Ok(None) };
    let item_group = sde.type_(type_id).map_or(0, |t| t.group_id as i64);
    let inside = move || entity.find(&["locationID"], &[store::Value::Int(container)]).filter(move |e| e.int("itemID") != Some(item));
    match group {
        GROUP_SILO => {
            let cargo = type_attribute(&[holder_type], ATTR_CARGO_GROUP).get(&holder_type).map(|g| *g as i64);
            if cargo.is_some_and(|g| g != item_group) {
                return Ok(Some("That silo cannot hold that."));
            }
            match provision {
                Some(p) if p != type_id => Ok(Some("That silo is set to hold something else.")),
                Some(_) => Ok(None),
                None if inside().any(|e| e.int("typeID") != Some(type_id)) => Ok(Some("That silo already holds something else.")),
                None => Ok(None),
            }
        }
        GROUP_REACTOR => {
            let runs = type_attribute_all(holder_type, &ATTR_REACTION_GROUP);
            let reaction = sde.table("invTypeReactions").get("reactionTypeID", type_id).is_some();
            if !reaction || !runs.contains(&item_group) {
                return Ok(Some("That reactor cannot run that."));
            }
            if inside().next().is_some() {
                return Ok(Some("That reactor already holds a reaction."));
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Every value a type has among `attrs`, as integers (rounded half to even,
/// as PostgreSQL casts them).
fn type_attribute_all(type_id: i64, attrs: &[i32]) -> Vec<i64> {
    crate::sde::get().type_attributes(type_id).iter().filter(|(a, _)| attrs.contains(a)).map(|(_, v)| v.round_ties_even() as i64).collect()
}

/// A reaction's (inputs, outputs): (typeID, batches).
fn reaction_rows(reaction: i64) -> (Vec<(i64, i64)>, Vec<(i64, i64)>) {
    let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
    for r in crate::sde::table("invTypeReactions").find("reactionTypeID", reaction) {
        let row = (r.int("typeID").unwrap_or(0), r.int("quantity").unwrap_or(0));
        if r.int("input") == Some(1) { inputs.push(row) } else { outputs.push(row) }
    }
    (inputs, outputs)
}

/// The links out of `sources`: (sourceID, typeID, destinationID), in the
/// order they were made.
fn links(db: &Db, sources: &[i64]) -> Vec<(i64, i64, i64)> {
    db.table("posLinks")
        .rows()
        .filter_map(|r| Some((r.int("sourceID")?, r.int("typeID")?, r.int("destinationID")?)))
        .filter(|(s, _, _)| sources.contains(s))
        .collect()
}

fn type_attribute(types: &[i64], attr: i32) -> HashMap<i64, f64> {
    let sde = crate::sde::get();
    types.iter().filter_map(|t| Some((*t, sde.type_attribute(*t, attr)?))).collect()
}

/// GetMoonProcessInfoForTower(towerID): for each harvester, silo and reactor,
/// (itemID, active, reaction, incoming [(sourceID, typeID)], demands,
/// supplies), demands and supplies as [(typeID, quantity)], -1 for "any".
/// A reactor's are left for the client to read from its reaction.
pub async fn process_info(ctx: &Ctx<'_>, tower: i64) -> Value {
    ctx.store.read(|db| {
        let all = producers(db, tower);
        let ids: Vec<i64> = all.iter().map(|p| p.id).collect();
        let links = links(db, &ids);
        let quality = type_attribute(&all.iter().map(|p| p.type_id).collect::<Vec<_>>(), ATTR_HARVESTER_QUALITY);
        let mut out = Vec::new();
        for p in &all {
            let incoming: Vec<Value> = links
                .iter()
                .filter(|(_, _, d)| *d == p.id)
                .map(|(s, t, _)| Value::Tuple(vec![Value::Int(*s), Value::Int(*t)]))
                .collect();
            let pair = |t: i64, q: i64| Value::List(vec![Value::Tuple(vec![Value::Int(t), Value::Int(q)])]);
            let none = || Value::List(Vec::new());
            let (reaction, demands, supplies) = match p.group_id {
                GROUP_MOON_MINING => {
                    let supplies = p.provision.map_or_else(none, |t| pair(t, quality.get(&p.type_id).copied().unwrap_or(1.0) as i64));
                    (Value::None, none(), supplies)
                }
                GROUP_SILO => match p.provision {
                    Some(t) => (Value::None, pair(t, -1), pair(t, -1)),
                    None => (Value::None, none(), none()),
                },
                _ => {
                    let reaction = reaction_in(db, p.id);
                    (reaction.map_or(Value::None, |(i, t)| Value::Tuple(vec![Value::Int(i), Value::Int(t)])), Value::None, Value::None)
                }
            };
            out.push(Value::Tuple(vec![Value::Int(p.id), Value::Bool(p.online), reaction, Value::List(incoming), demands, supplies]));
        }
        Value::List(out)
    })
}

/// ChangeStructureProvisionType(towerID, itemID, typeID): what a harvester
/// mines (one of its moon's materials) or a silo holds (of its cargo group,
/// if it has one). Links of the old type go.
pub async fn change_provision(ctx: &Ctx<'_>, tower: i64, item: i64, type_id: i64) -> Refused<Value> {
    let (all, place) = ctx.store.read(|db| (producers(db, tower), tower_moon(db, tower)));
    let Some(p) = all.iter().find(|p| p.id == item) else { return Err(Outcome::custom("That structure is not part of this tower.")) };
    match p.group_id {
        GROUP_MOON_MINING => {
            let (moon, security) = place.ok_or_else(|| Outcome::custom("The tower stands at no moon."))?;
            if !moon_materials(moon, security, materials()).iter().any(|(t, _)| *t == type_id) {
                return Err(Outcome::custom("This moon does not hold that material."));
            }
        }
        GROUP_SILO => {
            let wanted = type_attribute(&[p.type_id], ATTR_CARGO_GROUP).get(&p.type_id).map(|g| *g as i64);
            if let Some(group) = wanted {
                if !crate::sde::get().type_(type_id).is_some_and(|t| t.group_id as i64 == group) {
                    return Err(Outcome::custom("That silo cannot hold that."));
                }
            }
        }
        _ => return Err(Outcome::custom("Only harvesters and silos are set to a type.")),
    }
    let saved = ctx.store.write(|tx| {
        if let Some(row) = tx.table("posStructures").get_id(item).map(|r| r.id) {
            tx.update("posStructures", row, &[("provisionType", store::Value::Int(type_id))])?;
        }
        let stale: Vec<store::RowId> = tx
            .table("posLinks")
            .rows()
            .filter(|l| (l.int("sourceID") == Some(item) || l.int("destinationID") == Some(item)) && l.int("typeID") != Some(type_id))
            .map(|l| l.id)
            .collect();
        for row in stale {
            tx.delete("posLinks", row)?;
        }
        Ok::<_, StoreError>(())
    });
    saved.map_err(|e| crate::svc::membership::db_failed("setting the type", e))?;
    info!("structure {item} at tower {tower} now handles type {type_id}");
    Ok(Value::None)
}

/// LinkResourceForTower(towerID, [((typeID, sourceID), destinationID)]):
/// the tower's links, all at once. Links that make no sense are dropped;
/// the answer is whether all of them were kept.
pub async fn link(ctx: &Ctx<'_>, tower: i64, wanted: Option<&Value>) -> Refused<Value> {
    let (all, reactions) = ctx.store.read(|db| {
        let all = producers(db, tower);
        let mut reactions: HashMap<i64, (Vec<(i64, i64)>, Vec<(i64, i64)>)> = HashMap::new();
        for p in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
            if let Some((_, reaction)) = reaction_in(db, p.id) {
                reactions.insert(p.id, reaction_rows(reaction));
            }
        }
        (all, reactions)
    });
    let by_id: HashMap<i64, &Producer> = all.iter().map(|p| (p.id, p)).collect();
    let supplies = |p: &Producer, t: i64| match p.group_id {
        GROUP_REACTOR => reactions.get(&p.id).is_some_and(|(_, out)| out.iter().any(|(o, _)| *o == t)),
        _ => p.provision == Some(t),
    };
    let demands = |p: &Producer, t: i64| match p.group_id {
        GROUP_REACTOR => reactions.get(&p.id).is_some_and(|(inp, _)| inp.iter().any(|(i, _)| *i == t)),
        GROUP_SILO => p.provision == Some(t),
        _ => false,
    };
    let mut kept = Vec::new();
    let mut all_kept = true;
    for l in wanted.and_then(|v| v.seq()).unwrap_or(&[]) {
        let parts = l.seq().unwrap_or(&[]);
        let key = parts.first().and_then(|k| k.seq()).unwrap_or(&[]);
        let (t, source, dest) = (
            key.first().and_then(|v| v.as_i64()),
            key.get(1).and_then(|v| v.as_i64()),
            parts.get(1).and_then(|v| v.as_i64()),
        );
        let (Some(t), Some(s), Some(d)) = (t, source, dest) else { all_kept = false; continue };
        match (by_id.get(&s), by_id.get(&d)) {
            (Some(src), Some(dst)) if s != d && supplies(src, t) && demands(dst, t) => kept.push((s, t, d)),
            _ => all_kept = false,
        }
    }
    let saved = ctx.store.write(|tx| {
        for p in &all {
            tx.delete_where("posLinks", &["sourceID"], &[store::Value::Int(p.id)])?;
        }
        for (s, t, d) in &kept {
            let sets = [("sourceID", store::Value::Int(*s)), ("typeID", store::Value::Int(*t)), ("destinationID", store::Value::Int(*d))];
            tx.upsert("posLinks", &["sourceID", "typeID"], &sets, &sets[2..])?;
        }
        Ok::<_, StoreError>(())
    });
    saved.map_err(|e| crate::svc::membership::db_failed("linking the structures", e))?;
    info!("tower {tower}: {} production link(s)", kept.len());
    Ok(Value::Bool(all_kept))
}

/// One production cycle at a tower, when it has paid for a period of fuel.
pub async fn cycle(store: &Store, tower: i64) {
    if let Err(e) = store.write(|tx| run(&mut ItemTxn::new(tx), tower)) {
        error!("production at tower {tower}: {}", crate::logging::chain(&e));
    }
}

fn run(txn: &mut ItemTxn<'_, '_>, tower: i64) -> Result<(), StoreError> {
    let db = txn.db();
    let all: Vec<Producer> = producers(db, tower).into_iter().filter(|p| p.online).collect();
    if all.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = all.iter().map(|p| p.id).collect();
    let links = links(db, &ids);
    let by_id: HashMap<i64, Producer> = all.iter().map(|p| (p.id, p.clone())).collect();
    let moon = match tower_moon(db, tower) {
        Some((moon, security)) => moon_materials(moon, security, materials()),
        None => Vec::new(),
    };
    let mut reactions = HashMap::new();
    for p in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
        if let Some((_, reaction)) = reaction_in(db, p.id) {
            reactions.insert(p.id, reaction_rows(reaction));
        }
    }
    let mut types: Vec<i64> = moon.iter().map(|(t, _)| *t).collect();
    types.extend(reactions.values().flat_map(|(i, o)| i.iter().chain(o).map(|(t, _)| *t)));
    let amount = type_attribute(&types, ATTR_MOON_MINING_AMOUNT);
    let units = |t: i64, batches: i64| batches * amount.get(&t).copied().unwrap_or(1.0).max(1.0) as i64;
    let quality = type_attribute(&all.iter().map(|p| p.type_id).collect::<Vec<_>>(), ATTR_HARVESTER_QUALITY);
    let to = |s: i64, t: i64| links.iter().find(|(src, ty, _)| *src == s && *ty == t).map(|(_, _, d)| *d).filter(|d| by_id.contains_key(d));
    let from = |d: i64, t: i64| links.iter().find(|(_, ty, dst)| *dst == d && *ty == t).map(|(s, _, _)| *s).filter(|s| by_id.contains_key(s));

    // Harvesters fill their silos.
    for h in all.iter().filter(|p| p.group_id == GROUP_MOON_MINING) {
        let Some(t) = h.provision else { continue };
        let Some(&(_, batches)) = moon.iter().find(|(m, _)| *m == t) else { continue };
        let Some(silo) = to(h.id, t) else { continue };
        let take = batches.min(quality.get(&h.type_id).copied().unwrap_or(1.0) as i64);
        put(txn, &by_id[&silo], t, units(t, take))?;
    }
    // Reactors run when every input is there and every output has room.
    'reactors: for r in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
        let Some((inputs, outputs)) = reactions.get(&r.id) else { continue };
        let mut sources = Vec::new();
        for &(t, q) in inputs {
            let Some(silo) = from(r.id, t) else { continue 'reactors };
            if held(txn, silo, t) < units(t, q) {
                continue 'reactors;
            }
            sources.push((silo, t, units(t, q)));
        }
        let mut sinks = Vec::new();
        for &(t, q) in outputs {
            let Some(silo) = to(r.id, t) else { continue 'reactors };
            if room(txn, &by_id[&silo], t)? < units(t, q) {
                continue 'reactors;
            }
            sinks.push((silo, t, units(t, q)));
        }
        for (silo, t, n) in sources {
            take_from(txn, silo, t, n)?;
        }
        for (silo, t, n) in sinks {
            put(txn, &by_id[&silo], t, n)?;
        }
    }
    Ok(())
}

/// How much of `t` a silo holds.
fn held(txn: &ItemTxn<'_, '_>, silo: i64, t: i64) -> i64 {
    txn.db()
        .table("entity")
        .find(&["locationID"], &[store::Value::Int(silo)])
        .filter(|e| e.int("typeID") == Some(t))
        .filter_map(|e| e.int("quantity"))
        .sum()
}

/// How many more units of `t` a silo can take.
fn room(txn: &ItemTxn<'_, '_>, silo: &Producer, t: i64) -> Result<i64, StoreError> {
    let capacity = silo_capacity(txn.db(), silo.id)?.unwrap_or(0.0);
    let types = crate::sde::table("invTypes");
    let volume_of = |t: i64| types.get("typeID", t).map(|r| r.float("volume"));
    // Summed in the order the rows come, as sum() did; a type the data does
    // not know, or whose volume it leaves out, takes no room.
    let used = txn
        .db()
        .table("entity")
        .find(&["locationID"], &[store::Value::Int(silo.id)])
        .filter_map(|r| Some(volume_of(r.int("typeID")?)?? * r.int("quantity")? as f64))
        .sum::<f64>();
    // GREATEST ignores a NULL volume; a type the data does not know counts as 1 m3.
    let volume = volume_of(t).map_or(1.0, |v| v.map_or(0.0001, |v| v.max(0.0001)));
    Ok(((capacity - used) / volume).floor().max(0.0) as i64)
}

/// Adds up to `n` of `t` to a silo, as much as fits, on the stack already there.
fn put(txn: &mut ItemTxn<'_, '_>, silo: &Producer, t: i64, n: i64) -> Result<(), StoreError> {
    if silo.group_id != GROUP_SILO || silo.provision != Some(t) {
        return Ok(());
    }
    let n = n.min(room(txn, silo, t)?);
    if n <= 0 {
        return Ok(());
    }
    let stack = txn
        .db()
        .table("entity")
        .find(&["locationID"], &[store::Value::Int(silo.id)])
        .find(|e| e.int("typeID") == Some(t))
        .map(|e| (e.int("itemID").unwrap_or(0), e.int("quantity").unwrap_or(0)));
    match stack {
        Some((id, quantity)) => txn.set_quantity(id, quantity + n),
        None => txn
            .create(NewItem { type_id: t, owner_id: silo.owner_id, location_id: silo.id, flag: 0, quantity: n, singleton: false })
            .map(|_| ()),
    }
}

/// Takes `n` of `t` out of a silo, smallest stacks first.
fn take_from(txn: &mut ItemTxn<'_, '_>, silo: i64, t: i64, mut n: i64) -> Result<(), StoreError> {
    let mut stacks: Vec<(i64, i64)> = txn
        .db()
        .table("entity")
        .find(&["locationID"], &[store::Value::Int(silo)])
        .filter(|e| e.int("typeID") == Some(t))
        .map(|e| (e.int("itemID").unwrap_or(0), e.int("quantity").unwrap_or(0)))
        .collect();
    stacks.sort_by_key(|(_, q)| *q);
    for (id, q) in stacks {
        if n == 0 {
            break;
        }
        let take = n.min(q);
        if take == q { txn.destroy(id)? } else { txn.set_quantity(id, q - take)? }
        n -= take;
    }
    Ok(())
}
