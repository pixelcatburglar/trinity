//! Fitting: modules into slots, charges into modules, and modules on- and
//! offline, with the ship's numbers recomputed by the dogma engine.
//!
//! A module goes into the slot it asks for or, with flagAutoFit, the first free
//! one of its kind; the ship must have that many slots and, for turrets and
//! launchers, a free hardpoint. A fitted module goes online if the ship has the
//! CPU and power grid left for it. Taking a module out takes it offline and
//! brings its charge along.
//!
//! The client hears item moves through OnItemChange; what that does to the
//! ship's and modules' attributes goes out as OnModuleAttributeChanges, and a
//! module going on- or offline as OnGodmaShipEffect (godma.py:777, :800).

use std::collections::{BTreeMap, HashMap, HashSet};

use eve_marshal::Value;
use tracing::{error, info};

use crate::dogma::{attr, category, effect, Catalog, Loadout, State};
use crate::items::{Item, ItemTxn, NewItem};
use crate::services::{Ctx, Outcome};
use crate::store::{self, Db, StoreError, Tx};
use crate::svc::moves::{refuse, MoveError};

const FLAG_AUTO_FIT: i64 = 0;
pub const EFFECT_ONLINE: i64 = 16;
/// Module attributes naming the charge groups it takes, and its charge size.
const CHARGE_GROUPS: [i32; 5] = [604, 605, 606, 609, 610];
const CHARGE_SIZE: i32 = 128;

/// Each kind of slot: the module effect that asks for it, its first flag and
/// the ship attribute that counts them.
const SLOTS: [(i32, i64, i32); 4] = [
    (effect::LO_POWER, 11, attr::LOW_SLOTS),
    (effect::MED_POWER, 19, attr::MED_SLOTS),
    (effect::HI_POWER, 27, attr::HI_SLOTS),
    (effect::RIG_SLOT, 92, attr::RIG_SLOTS),
];

/// Every computed attribute of a loadout, by item.
pub type Snapshot = HashMap<i64, HashMap<i32, f64>>;

pub fn snapshot(db: &Db, catalog: &Catalog, char_id: i64, ship_id: Option<i64>) -> Result<Snapshot, StoreError> {
    Ok(Loadout::load(db, catalog, char_id, ship_id, &HashSet::new())?.snapshot())
}

pub fn is_slot(flag: i64) -> bool {
    (11..=35).contains(&flag) || (92..=99).contains(&flag)
}

pub fn is_fitting_flag(flag: i64) -> bool {
    flag == FLAG_AUTO_FIT || is_slot(flag)
}

/// Put `item` (a module, or up to `quantity` of a charge) into `ship` at `flag`.
pub fn fit(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, ship: &Item, item: &Item, flag: i64, quantity: Option<i64>) -> Result<(), MoveError> {
    // Reloading works anywhere; changing modules needs a station.
    if ctx.session.station_id.is_none() && item.category_id != category::CHARGE {
        return refuse("Ships can only be refitted while docked.");
    }
    if Some(ship.id) != ctx.session.ship_id {
        return refuse("You can only fit your active ship.");
    }
    if (92..=99).contains(&item.flag) {
        return refuse("Installed rigs cannot be moved. Destroy the rig to remove it.");
    }
    check_skills(txn.db(), ctx.session.char_id.unwrap_or(0), item.type_id)?;
    match item.category_id {
        category::CHARGE => load_charge(ctx, txn, ship, item, flag, quantity.unwrap_or(item.quantity)),
        category::MODULE => fit_module(ctx, txn, ship, item, flag),
        _ => refuse("Only modules and charges can be fitted."),
    }
}

/// Trinity has six prerequisite slots (the last three were added after RMR).
pub(crate) fn check_skills(db: &Db, char_id: i64, type_id: i64) -> Result<(), MoveError> {
    let required = required_skill_levels(db, type_id)?;
    let types: Vec<i32> = required.keys().copied().collect();
    let trained = crate::svc::skills::levels(db, char_id, &types)?;
    for (skill, level) in required {
        if trained.get(&skill).copied().unwrap_or(0) < level {
            return refuse(format!("You need skill {skill} trained to level {level} to fit this item."));
        }
    }
    Ok(())
}

/// The static data is in memory; the database handle and the error stay for
/// the callers written against the query this once was.
pub fn required_skill_levels(_db: &Db, type_id: i64) -> Result<BTreeMap<i32, i64>, StoreError> {
    let Ok(type_id) = i32::try_from(type_id) else { return Ok(BTreeMap::new()) };
    // dgmTypeAttribs keeps its values as text; read as `value::float8`, and
    // made whole as `::integer` does, rounding halves to even.
    let attribs = crate::sde::table("dgmTypeAttribs");
    let value = |attribute: i64| {
        attribs
            .find("typeID", type_id as i64)
            .find(|r| r.int("attributeID") == Some(attribute))
            .and_then(|r| r.text("value")?.trim().parse::<f64>().ok())
    };
    let mut required: BTreeMap<i32, i64> = BTreeMap::new();
    for (skill, level) in [(182, 277), (183, 278), (184, 279), (1285, 1286), (1289, 1287), (1290, 1288)] {
        let Some(skill) = value(skill).filter(|v| *v > 0.0) else { continue };
        let skill = skill.round_ties_even() as i32;
        let level = value(level).map_or(1, |v| v.round_ties_even() as i32 as i64);
        required.entry(skill).and_modify(|old| *old = (*old).max(level)).or_insert(level);
    }
    Ok(required)
}

fn fit_module(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, ship: &Item, module: &Item, wanted: i64) -> Result<(), MoveError> {
    let char_id = ctx.session.char_id.unwrap_or(0);
    let loadout = Loadout::load(txn.db(), ctx.dogma, char_id, Some(ship.id), &HashSet::new())?;
    let Some(ship_index) = loadout.find(ship.id) else { return refuse("Your ship could not be read.") };

    let effects: HashSet<i32> = crate::sde::get().type_effects(module.type_id).iter().copied().collect();
    let Some(&(_, first, count_attr)) = SLOTS.iter().find(|(e, _, _)| effects.contains(e)) else {
        return refuse("That module has no slot to go in.");
    };
    let slots = loadout.value(ship_index, count_attr).max(0.0) as i64;
    let taken: HashSet<i64> = loadout
        .pieces
        .iter()
        .filter(|p| p.category_id == category::MODULE)
        .map(|p| p.flag)
        .collect();
    let range = first..first + slots;
    let flag = if wanted == FLAG_AUTO_FIT {
        match range.clone().find(|f| !taken.contains(f)) {
            Some(f) => f,
            None => return refuse("There is no free slot of that kind."),
        }
    } else if !range.contains(&wanted) {
        return refuse("That module does not fit in that slot.");
    } else {
        wanted
    };
    // Dropped on a fitted slot: the module there goes back to the hangar.
    let occupant = loadout.pieces.iter().find(|p| p.category_id == category::MODULE && p.flag == flag);
    if effects.contains(&effect::RIG_SLOT) {
        if occupant.is_some() {
            return refuse("Destroy the installed rig before fitting a replacement.");
        }
        let cost = type_attribute(module.type_id, attr::UPGRADE_COST);
        let room = loadout.value(ship_index, attr::UPGRADE_CAPACITY) - loadout.value(ship_index, attr::UPGRADE_LOAD);
        if cost > room + 1e-6 {
            return refuse("Your ship does not have enough calibration for that rig.");
        }
    }
    let loadout = match occupant.map(|p| p.item_id) {
        Some(occupant) => {
            let station = ctx.session.station_id.unwrap_or(0);
            let Some(old) = txn.item(occupant)? else { return refuse("Your ship could not be read.") };
            unfit(txn, &old, station, crate::svc::inventory::FLAG_HANGAR.into())?;
            txn.move_to(old.id, station, crate::svc::inventory::FLAG_HANGAR.into())?;
            Loadout::load(txn.db(), ctx.dogma, char_id, Some(ship.id), &HashSet::new())?
        }
        None => loadout,
    };
    let ship_index = ship_index_of(&loadout, ship.id);
    for (fitted, left, what) in [
        (effect::TURRET_FITTED, attr::TURRET_SLOTS_LEFT, "turret"),
        (effect::LAUNCHER_FITTED, attr::LAUNCHER_SLOTS_LEFT, "launcher"),
    ] {
        if effects.contains(&fitted) && loadout.find(module.id).is_none() && loadout.value(ship_index, left) < 1.0 {
            return refuse(format!("There is no free {what} hardpoint."));
        }
    }

    let module_id = if module.quantity > 1 {
        txn.set_quantity(module.id, module.quantity - 1)?;
        let new = NewItem {
            type_id: module.type_id,
            owner_id: module.owner_id,
            location_id: ship.id,
            flag,
            quantity: 1,
            singleton: true,
        };
        txn.create(new)?
    } else {
        txn.move_to(module.id, ship.id, flag)?;
        txn.set_singleton(module.id, true)?;
        module.id
    };

    // Online straight away, if the ship can carry it.
    let loadout = Loadout::load(txn.db(), ctx.dogma, char_id, Some(ship.id), &HashSet::new())?;
    if let Some(i) = loadout.find(module_id) {
        if can_online(&loadout, ship_index_of(&loadout, ship.id), i) {
            set_online_attr(txn.tx(), module_id, true)?;
        }
    }
    info!("fitted module {module_id} (type {}) in slot {flag}", module.type_id);
    Ok(())
}

fn ship_index_of(loadout: &Loadout<'_>, ship_id: i64) -> usize {
    loadout.find(ship_id).unwrap_or(0)
}

/// Whether module `i` fits in the CPU and power grid the ship has left.
fn can_online(loadout: &Loadout<'_>, ship: usize, i: usize) -> bool {
    let hp = loadout.value(i, crate::combat::attr::HP);
    if hp > 0.0 && loadout.value(i, crate::combat::attr::DAMAGE) >= hp {
        return false;
    }
    let cpu_left = loadout.value(ship, attr::CPU_OUTPUT) - loadout.value(ship, attr::CPU_LOAD);
    let power_left = loadout.value(ship, attr::POWER_OUTPUT) - loadout.value(ship, attr::POWER_LOAD);
    let (cpu, power) = (loadout.value(i, attr::CPU), loadout.value(i, attr::POWER));
    let online_now = loadout.pieces[i].state != State::Offline;
    online_now || (cpu <= cpu_left + 1e-6 && power <= power_left + 1e-6)
}

fn set_online_attr(tx: &mut Tx<'_>, item_id: i64, on: bool) -> Result<(), StoreError> {
    let (item, attribute) = (store::Value::Int(item_id), store::Value::Int(i64::from(attr::IS_ONLINE)));
    if on {
        let sets = [("itemID", item), ("attributeID", attribute), ("valueInt", store::Value::Int(1)), ("valueFloat", store::Value::Null)];
        tx.upsert("entity_attributes", &["itemID", "attributeID"], &sets, &sets[2..])?;
    } else {
        tx.delete_where("entity_attributes", &["itemID", "attributeID"], &[item, attribute])?;
    }
    Ok(())
}

/// Load up to `wanted` of a charge into the module at `flag`, as far as the
/// module holds.
fn load_charge(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, ship: &Item, charge: &Item, flag: i64, wanted: i64) -> Result<(), MoveError> {
    if wanted <= 0 || charge.quantity <= 0 {
        return refuse("Load a positive quantity of charges.");
    }
    let char_id = ctx.session.char_id.unwrap_or(0);
    let loadout = Loadout::load(txn.db(), ctx.dogma, char_id, Some(ship.id), &HashSet::new())?;
    let Some(module) = loadout.pieces.iter().position(|p| p.category_id == category::MODULE && p.flag == flag) else {
        return refuse("There is no module in that slot to load.");
    };
    if let Some(system) = ctx.session.solar_system_id {
        let world = ctx.world.lock();
        let running = world.parks.get(&system).and_then(|park| park.ships.get(&ship.id))
            .and_then(|ship| ship.fight.as_ref()).is_some_and(|fight| fight.module(loadout.pieces[module].item_id).is_some());
        if running { return refuse("Deactivate the module before loading charges."); }
    }
    let groups: Vec<i64> = CHARGE_GROUPS
        .iter()
        .map(|a| loadout.value(module, *a) as i64)
        .filter(|g| *g > 0)
        .collect();
    if !charge_group(charge.type_id).is_some_and(|g| groups.contains(&g)) {
        return refuse("That charge does not fit that module.");
    }
    let size = loadout.value(module, CHARGE_SIZE);
    if size > 0.0 {
        let charge_size = type_attribute(charge.type_id, CHARGE_SIZE);
        if (charge_size - size).abs() > 1e-6 {
            return refuse("That charge is the wrong size for that module.");
        }
    }
    let loaded = loadout.pieces.iter().find(|p| p.category_id == category::CHARGE && p.flag == flag);
    if let Some(loaded) = loaded {
        if loaded.type_id != charge.type_id {
            return refuse("Unload the current charges first.");
        }
    }
    let capacity = type_capacity(loadout.pieces[module].type_id);
    let room = if charge.volume > 0.0 { (capacity / charge.volume).floor() as i64 } else { charge.quantity };
    let have = match loaded {
        Some(l) => txn.item(l.item_id)?.map_or(0, |i| i.quantity),
        None => 0,
    };
    let quantity = wanted.clamp(1, charge.quantity).min(room - have);
    if quantity <= 0 {
        return refuse("That module is already full.");
    }
    match loaded {
        Some(l) => txn.set_quantity(l.item_id, have + quantity)?,
        None if quantity == charge.quantity => txn.move_to(charge.id, ship.id, flag)?,
        None => {
            let new = NewItem { type_id: charge.type_id, owner_id: charge.owner_id, location_id: ship.id, flag, quantity, singleton: false };
            txn.create(new)?;
        }
    }
    if !(loaded.is_none() && quantity == charge.quantity) {
        txn.set_quantity(charge.id, charge.quantity - quantity)?;
    }
    Ok(())
}

/// A charge's group; None for a type the static data does not know, which
/// fits nothing.
fn charge_group(type_id: i64) -> Option<i64> {
    crate::sde::get().type_(type_id).map(|t| t.group_id as i64)
}

fn type_capacity(type_id: i64) -> f64 {
    crate::sde::table("invTypes").get("typeID", type_id).and_then(|r| r.float("capacity")).unwrap_or(0.0)
}

fn type_attribute(type_id: i64, attribute: i32) -> f64 {
    crate::sde::get().type_attribute(type_id, attribute).unwrap_or(0.0)
}

/// A module leaving its slot goes offline and takes its charge with it.
pub fn unfit(txn: &mut ItemTxn<'_, '_>, module: &Item, location: i64, flag: i64) -> Result<(), MoveError> {
    if (92..=99).contains(&module.flag) {
        return refuse("Installed rigs cannot be removed intact. Destroy the rig to remove it.");
    }
    set_online_attr(txn.tx(), module.id, false)?;
    unload(txn, module.location_id, module.flag, location, flag)
}

/// Move the charges in `ship`'s slot `slot` to `location` at `flag`.
pub fn unload(txn: &mut ItemTxn<'_, '_>, ship_id: i64, slot: i64, location: i64, flag: i64) -> Result<(), MoveError> {
    let charge_types = crate::sde::get().types_in_category(category::CHARGE);
    let charges: Vec<i64> = txn
        .db()
        .table("entity")
        .find(&["locationID", "flag"], &[store::Value::Int(ship_id), store::Value::Int(slot)])
        .filter(|e| e.int("typeID").and_then(|t| i32::try_from(t).ok()).is_some_and(|t| charge_types.binary_search(&t).is_ok()))
        .filter_map(|e| e.int("itemID"))
        .collect();
    for id in charges {
        txn.move_to(id, location, flag)?;
    }
    Ok(())
}

/// OnModuleAttributeChanges for what differs between two snapshots, for items
/// in both (new items are read whole by the client).
pub fn attribute_changes(owner: i64, before: &Snapshot, after: &Snapshot) -> Vec<Value> {
    let now = crate::bulkdata::filetime_now();
    let mut out = Vec::new();
    for (item, attrs) in after {
        let Some(old) = before.get(item) else { continue };
        for (a, v) in attrs {
            let was = old.get(a).copied().unwrap_or(0.0);
            if (v - was).abs() > 1e-9 {
                out.push(Value::Tuple(vec![
                    Value::bytes("OnModuleAttributeChange"),
                    Value::Int(owner),
                    Value::Int(*item),
                    Value::Int(*a as i64),
                    Value::Long(now),
                    number(*v),
                    number(was),
                ]));
            }
        }
    }
    out
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        Value::Int(v as i64)
    } else {
        Value::Double(v)
    }
}

/// Tell the client what changed between `before` and the loadout now.
pub async fn push_attribute_changes(ctx: &Ctx<'_>, before: &Snapshot) {
    let Some(char_id) = ctx.session.char_id else { return };
    match ctx.store.read(|db| snapshot(db, ctx.dogma, char_id, ctx.session.ship_id)) {
        Ok(after) => {
            let changes = attribute_changes(char_id, before, &after);
            if !changes.is_empty() {
                ctx.world.lock().notify(&[ctx.client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
            }
        }
        Err(e) => error!("recomputing attributes: {}", crate::logging::chain(&e)),
    }
}

/// dogmaIM Activate/Deactivate(itemID, "online"): put a fitted module on- or
/// offline.
pub async fn set_online(ctx: &Ctx<'_>, item_id: i64, on: bool) -> Outcome {
    let (Some(char_id), Some(ship_id)) = (ctx.session.char_id, ctx.session.ship_id) else {
        return Outcome::custom("You have no ship.");
    };
    if !on {
        if let Some(system) = ctx.session.solar_system_id {
            let world = ctx.world.lock();
            if world.parks.get(&system).and_then(|park| park.ships.get(&ship_id))
                .and_then(|ship| ship.fight.as_ref()).is_some_and(|fight| fight.module(item_id).is_some())
            {
                return Outcome::custom("Deactivate the module before putting it offline.");
            }
        }
    }
    let result: Result<Option<Snapshot>, MoveError> = ctx.store.write(|tx| {
        let loadout = Loadout::load(tx.db(), ctx.dogma, char_id, Some(ship_id), &HashSet::new())?;
        let Some(i) = loadout.find(item_id).filter(|i| loadout.pieces[*i].category_id == category::MODULE) else {
            return refuse("That module is not fitted to your ship.");
        };
        let online = loadout.pieces[i].state != State::Offline;
        if online == on {
            return Ok(None);
        }
        if on {
            check_skills(tx.db(), char_id, loadout.pieces[i].type_id)?;
        }
        if on && !can_online(&loadout, ship_index_of(&loadout, ship_id), i) {
            return refuse("Your ship does not have the CPU or power grid left to online that module.");
        }
        let before = loadout.snapshot();
        set_online_attr(tx, item_id, on)?;
        Ok(Some(before))
    });
    match result {
        Ok(Some(before)) => {
            push_attribute_changes(ctx, &before).await;
            ctx.world.lock().notify(&[ctx.client_id], "OnGodmaShipEffect", online_effect(item_id, char_id, ship_id, on));
            info!("module {item_id} {}", if on { "online" } else { "offline" });
            Outcome::Value(Value::None)
        }
        Ok(None) => Outcome::Value(Value::None),
        Err(MoveError::Refused(why)) => Outcome::custom(why),
        Err(MoveError::Reactivation{item,delay})=>super::moves::reactivation_error(item,delay),
        Err(MoveError::Db(e)) => {
            error!("online toggle for {item_id}: {}", crate::logging::chain(&e));
            Outcome::custom("The module could not be changed.")
        }
    }
}

/// OnGodmaShipEffect(itemID, effectID, time, start, active, environment,
/// startTime, duration, repeat, randomSeed, error) for the online effect.
fn online_effect(item_id: i64, char_id: i64, ship_id: i64, on: bool) -> Value {
    let now = crate::bulkdata::filetime_now();
    let environment = Value::Tuple(vec![
        Value::Int(item_id),
        Value::Int(char_id),
        Value::Int(ship_id),
        Value::None,
        Value::None,
        Value::List(Vec::new()),
        Value::Int(EFFECT_ONLINE),
    ]);
    Value::Tuple(vec![
        Value::Int(item_id),
        Value::Int(EFFECT_ONLINE),
        Value::Long(now),
        Value::Int(i64::from(on)),
        Value::Int(i64::from(on)),
        environment,
        Value::Long(now),
        Value::Int(-1),
        Value::Int(-1),
        Value::None,
        Value::None,
    ])
}
