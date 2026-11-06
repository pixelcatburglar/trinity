//! dogmaIM in space: locking targets and running modules (targetMgr.py,
//! godma.py), and what a ship brings to a fight, as the dogma engine computes
//! it from the ship, its fitting and its pilot's skills.
//!
//! A lock takes 40000 / (scanResolution * asinh(signature)^2) seconds, so
//! AddTarget answers "pending" and the heartbeat sends OnTarget("add") when it
//! completes. Activate builds the module's cycle from its computed attributes
//! and those of its charge; the heartbeat runs it from there.

use std::collections::{BTreeSet, HashSet};

use eve_marshal::Value;
use tracing::{debug, error, info};

use crate::ballpark::{Pilot, Ship, ShipStats};
use crate::combat::{self, attr, Action, Active, Ammo, Charged, Fighter, Gun, Lock, Vitals, REPEAT_FOREVER};
use crate::dogma::{category, Catalog, Loadout, State};
use crate::fight;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::space::{self, entry};

mod activation;

/// Effect categories (dgmEffects.effectCategory).
const TARGETED: i16 = 2;
/// Effects whose cycle is hard-coded here; any other active effect is
/// [`Action::Passive`]: dogma applies its modifiers while it runs.
mod effect {
    pub const SHIELD_BOOSTING: i64 = 4;
    pub const SPEED_BOOST: i64 = 7;
    pub const TARGET_ATTACK: i64 = 10;
    pub const MINING: i64 = 17;
    pub const STRUCTURE_REPAIR: i64 = 26;
    pub const ARMOR_REPAIR: i64 = 27;
    pub const PROJECTILE_FIRED: i64 = 34;
    pub const POWER_BOOSTER: i64 = 48;
    pub const MINING_LASER: i64 = 67;
    pub const USE_MISSILES: i64 = 101;
    pub const SPEED_BOOST_MASS_ADDITION: i64 = 1253;
    pub const SPEED_BOOST_MASS_SIG_RAD: i64 = 1254;
    pub const SPEED_BOOST_MASS_SIG_VELOCITY: i64 = 1417;
    pub const SHIELD_TRANSFER: i64 = 18;
    pub const MODIFY_TARGET_SPEED: i64 = 29;
    pub const ENERGY_TRANSFER: i64 = 31;
    pub const ENERGY_VAMPIRE: i64 = 32;
    pub const ENERGY_DESTABILIZATION: i64 = 36;
    pub const WARP_SCRAMBLE: i64 = 39;
    pub const DECREASE_TARGET_SPEED: i64 = 586;
    pub const TARGET_ARMOR_REPAIR: i64 = 592;
    pub const DISRUPT: i64 = 609;
    pub const DAMPEN: i64 = 837;
    pub const DISRUPT_TEST: i64 = 1274;
    pub const DAMPEN_TEST: i64 = 1354;
    pub const JAM: i64 = 1358;
    pub const TARGET_PAINT_2: i64 = 1507;
    pub const TARGET_PAINT: i64 = 1549;
    pub const ENERGY_DESTABILIZATION_NEW: i64 = 2303;
    pub const ENERGY_NOSFERATU: i64 = 2304;
    pub const SHIP_SCAN: i64 = 46;
    pub const CARGO_SCAN: i64 = 47;
    pub const SURVEY_SCAN: i64 = 81;
    pub const CLOAKING: i64 = 607;
    pub const CLOAKING_WARP_SAFE: i64 = 980;
    pub const SYSTEM_SCAN: i64 = 1150;
    pub const CYNOSURAL_GENERATION: i64 = 1729;
    pub const CYNOSURAL_GENERATION_45017: i64 = 2857;
    pub const EMP_WAVE_GRID: i64 = 2071;
    pub const JUMP_PORTAL_GENERATION: i64 = 2152;

    pub const PROPULSION: [i32; 4] =
        [SPEED_BOOST as i32, SPEED_BOOST_MASS_ADDITION as i32, SPEED_BOOST_MASS_SIG_RAD as i32, SPEED_BOOST_MASS_SIG_VELOCITY as i32];
}
/// Frequency and mining crystals last; everything else is used up.
const REUSABLE_CHARGE_GROUPS: [i64; 2] = [86, 482];

// ---- what a ship is ----------------------------------------------------------------------

/// A ship as it flies and fights.
pub struct Profile {
    pub stats: ShipStats,
    pub pilot: Pilot,
    pub fighter: Fighter,
}

/// Builds `ship_id`'s profile for `char_id`, with the effects of the
/// `active` modules applied. Its hit points come from what is stored on the
/// ship: armour and structure damage, and the shield and capacitor as they
/// were when it was last in space (full if never).
pub fn profile(
    db: &Db,
    catalog: &Catalog,
    char_id: i64,
    ship_id: i64,
    active: &BTreeSet<i64>,
    now: i64,
) -> Result<Option<Profile>, StoreError> {
    let running: HashSet<i64> = active.iter().copied().collect();
    let loadout = Loadout::load(db, catalog, char_id, Some(ship_id), &running)?;
    let (Some(ship), character) = (loadout.ship(), loadout.character()) else { return Ok(None) };
    let Some(hull) = db.table("entity").get_id(ship_id) else { return Ok(None) };
    let pilot_row = db.table("character_").get_id(char_id);
    let corporation = pilot_row.and_then(|c| c.int("corporationID")).and_then(|c| db.table("corporation").get_id(c));
    // A hull of a type the static data does not know has no profile, as
    // joining on it found none.
    let type_id = hull.int("typeID").unwrap_or(0);
    let Some(t) = crate::sde::table("invTypes").get("typeID", type_id) else { return Ok(None) };
    let stored: Vec<(i32, f64)> = db
        .table("entity_attributes")
        .find(&["itemID"], &[store::Value::Int(ship_id)])
        .filter_map(|a| {
            let id = a.int("attributeID")? as i32;
            [attr::SHIELD_CHARGE, attr::CHARGE].contains(&id).then_some(())?;
            Some((id, stored_value(&a)?))
        })
        .collect();

    let v = |a: i32| loadout.value(ship, a);
    let mut mass = t.float("mass").unwrap_or(0.0);
    let mut max_velocity = v(attr::MAX_VELOCITY);
    let mut signature = v(attr::SIGNATURE_RADIUS);
    // Afterburners and microwarpdrives: thrust over mass, as 2.10 hard-coded it.
    for (i, p) in loadout.pieces.iter().enumerate() {
        if p.state != State::Active || !effect::PROPULSION.iter().any(|e| p.has_effect(*e)) {
            continue;
        }
        mass += loadout.value(i, attr::MASS_ADDITION);
        let thrust = loadout.value(i, attr::SPEED_BOOST_FACTOR);
        let factor = loadout.value(i, attr::SPEED_FACTOR) / 100.0;
        if mass > 0.0 {
            max_velocity *= 1.0 + factor * thrust / mass;
        }
        signature *= 1.0 + loadout.value(i, attr::SIGNATURE_RADIUS_BONUS) / 100.0;
    }
    let mut stats = ShipStats::new(t.float("radius").unwrap_or(0.0), mass, v(attr::AGILITY), max_velocity, v(600));
    stats.warp_capacitor_need = v(crate::warp_energy::WARP_CAPACITOR_NEED);
    let pilot = Pilot {
        type_id,
        group_id: t.int("groupID").unwrap_or_default(),
        name: hull
            .text("itemName")
            .filter(|n| !n.is_empty())
            .map_or_else(|| t.text("typeName").unwrap_or_default().to_string(), str::to_string),
        char_id,
        corp_id: pilot_row.and_then(|c| c.int("corporationID")).unwrap_or(0),
        alliance_id: corporation.and_then(|c| c.int("allianceID")).filter(|a| *a != 0),
        security: pilot_row.and_then(|c| c.float("securityRating")).unwrap_or(0.0),
        bounty: pilot_row.and_then(|c| c.float("bounty")).unwrap_or(0.0),
    };

    let mut shield = Charged::full(v(attr::SHIELD_CAPACITY), v(attr::SHIELD_RECHARGE_RATE), now);
    let mut capacitor = Charged::full(v(attr::CAPACITOR_CAPACITY), v(attr::RECHARGE_RATE), now);
    for (a, value) in stored {
        match a {
            attr::SHIELD_CHARGE => shield.set(now, value),
            attr::CHARGE => capacitor.set(now, value),
            _ => {}
        }
    }
    let locks_ship = v(attr::MAX_LOCKED_TARGETS);
    let locks_skill = character.map_or(locks_ship, |c| loadout.value(c, attr::MAX_LOCKED_TARGETS));
    let max_locked = if locks_skill > 0.0 { locks_ship.min(locks_skill) } else { locks_ship };
    let vitals = Vitals {
        shield,
        armor_max: v(attr::ARMOR_HP),
        armor_damage: v(attr::ARMOR_DAMAGE).max(0.0),
        hull_max: v(attr::HP),
        hull_damage: v(attr::DAMAGE).max(0.0),
        capacitor,
        resonance: [attr::SHIELD_RESONANCE.map(v), attr::ARMOR_RESONANCE.map(v), attr::HULL_RESONANCE.map(v)],
    };
    let mut fighter = Fighter::new(
        vitals,
        signature.max(1.0),
        v(attr::SCAN_RESOLUTION),
        v(attr::MAX_TARGET_RANGE),
        max_locked.max(1.0) as usize,
    );
    fighter.applied = active.clone();
    fighter.top_speed = stats.max_velocity;
    fighter.sensors = attr::SENSOR_STRENGTHS.map(v);
    Ok(Some(Profile { stats, pilot, fighter }))
}

/// A stored attribute's value (`coalesce("valueFloat", "valueInt"::float8)`).
fn stored_value(a: &Row) -> Option<f64> {
    a.float("valueFloat").or_else(|| a.int("valueInt").map(|v| v as f64))
}

/// A ship entering space: its profile, made into a park ship.
pub async fn ship_for_space(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Option<Ship> {
    let now = combat::now_ms();
    let (p, position, history) = ctx.store.read(|db| {
        let p = match profile(db, ctx.dogma, char_id, ship_id, &BTreeSet::new(), now) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => {
                error!("loading ship {ship_id}: {}", crate::logging::chain(&e));
                return None;
            }
        };
        let row = db.table("entity").get_id(ship_id).filter(|e| e.int("ownerID") == Some(char_id))?;
        let position = crate::ballpark::Vec3::new(row.float("x").unwrap_or(0.0), row.float("y").unwrap_or(0.0), row.float("z").unwrap_or(0.0));
        match crate::cloak::durability::load_history(db, char_id, ship_id, now) {
            Ok(history) => Some((p, position, history)),
            Err(error) => {
                error!("loading ship {ship_id} cooldowns: {}", crate::logging::chain(&error));
                None
            }
        }
    })?;
    let mut ship = Ship::new(ship_id, ctx.client_id, p.stats, p.pilot, position, Default::default());
    ship.fight = Some(p.fighter);
    ship.fight.as_mut().unwrap().cloak_history = history;
    Some(ship)
}

/// Restore a saved continuous cloak only while the same fitted, online item
/// still supports its original effect. No activation charge or FX is replayed;
/// native SetState and ItemGetInfo carry the restored state to the client.
pub async fn restore_cloak(ctx:&Ctx<'_>,ship:&mut Ship,mut saved:crate::cloak::SavedState)->Result<(),StoreError> {
    saved.history.prune(combat::now_ms());
    if !saved.history.module_ready.is_empty() {
        let fitted:HashSet<i64>=ctx.store.read(|db| db.table("entity").find(&["ownerID","locationID"],&[store::Value::Int(ship.pilot.char_id),store::Value::Int(ship.id)])
            .filter(|e| e.int("flag").is_some_and(|f| (11..=35).contains(&f)) && e.int("singleton")!=Some(0) && e.int("quantity").is_some_and(|q| q>0))
            .filter_map(|e| e.int("itemID")).collect());
        saved.history.module_ready.retain(|id,_|fitted.contains(id));
    }
    if let Some(fight)=&mut ship.fight {fight.cloak_history.merge(saved.history,combat::now_ms());}
    let Some(saved)=saved.active else{return Ok(());};
    if saved.module_id<=0 || saved.module_id>i32::MAX as i64 || saved.type_id<=0 || saved.type_id>i32::MAX as i64
        || saved.started_ms<0 || saved.started_ms>combat::now_ms() {return Ok(());}
    // The module, still fitted and online.
    let fitted=ctx.store.read(|db| {
        let e=db.table("entity").get_id(saved.module_id)?;
        let fitted=e.int("ownerID")==Some(ship.pilot.char_id) && e.int("locationID")==Some(ship.id)
            && e.int("flag").is_some_and(|f| (11..=35).contains(&f)) && e.int("singleton")!=Some(0) && e.int("quantity").is_some_and(|q| q>0);
        let attribute=db.table("entity_attributes").get(&[store::Value::Int(saved.module_id),store::Value::Int(2)])?;
        (fitted && stored_value(&attribute).unwrap_or(0.0)!=0.0).then(|| e.int("typeID")).flatten()
    });
    if fitted!=Some(saved.type_id) {return Ok(());}
    let name=match saved.effect_id {effect::CLOAKING=>"cloaking",effect::CLOAKING_WARP_SAFE=>"cloakingWarpSafe",_=>return Ok(())};
    let Some(effect)=effect_by_name(ctx,name).await else{return Ok(());};
    if effect.id!=saved.effect_id || (ship.in_warp() && !effect.warp_safe) {return Ok(());}
    let planned={
        let loadout=ctx.store.read(|db| Loadout::load(db,ctx.dogma,ship.pilot.char_id,Some(ship.id),&HashSet::new()))?;
        plan(&loadout,saved.module_id,&effect,None,0)
    };
    let Ok(planned)=planned else{return Ok(());};
    let Ok(mut module)=finish(ctx,planned).await else{return Ok(());};
    if !module.continuous() || !matches!(module.action,Action::Cloak {..}) {return Ok(());}
    module.started_ms=saved.started_ms;module.next_ms=i64::MAX;
    let active=BTreeSet::from([module.module_id]);
    let Some(updated)=ctx.store.read(|db| profile(db,ctx.dogma,ship.pilot.char_id,ship.id,&active,combat::now_ms()))? else{return Ok(());};
    apply_profile(ship,updated,combat::now_ms());
    if let Some(fight)=&mut ship.fight {fight.modules.push(module);ship.cloaked=true;}
    Ok(())
}

/// Moves a profile's numbers onto a ship already in space, keeping its hit
/// points (as fractions where a maximum changed), locks and modules.
pub fn apply_profile(ship: &mut Ship, p: Profile, now: i64) {
    ship.stats = p.stats;
    ship.pilot = p.pilot;
    let Some(fight) = ship.fight.as_mut() else {
        ship.fight = Some(p.fighter);
        return;
    };
    let new = p.fighter;
    fight.vitals.shield.rescale(now, new.vitals.shield.max, new.vitals.shield.recharge_ms);
    fight.vitals.capacitor.rescale(now, new.vitals.capacitor.max, new.vitals.capacitor.recharge_ms);
    fight.vitals.armor_max = new.vitals.armor_max;
    fight.vitals.hull_max = new.vitals.hull_max;
    fight.vitals.resonance = new.vitals.resonance;
    fight.signature = new.signature;
    fight.scan_resolution = new.scan_resolution;
    fight.max_target_range = new.max_target_range;
    fight.max_locked = new.max_locked;
    fight.applied = new.applied;
    fight.top_speed = new.top_speed;
    fight.sensors = new.sensors;
    // Webs still hold: the next pulse slows it again.
    ship.stats.max_velocity = new.top_speed * fight.afflictions.speed;
}

/// Stores what a ship leaving space has left: damage always; the shield and
/// capacitor only while it stays out there (docked, they refill).
/// Fallible form for callers that commit vitals with a location transition.
pub fn write_vitals(tx: &mut Tx<'_>, ship_id: i64, fighter: &Fighter, docked: bool) -> Result<(), StoreError> {
    write_vitals_at(tx,ship_id,fighter,docked,combat::now_ms())
}

pub(crate) fn write_vitals_at(tx: &mut Tx<'_>, ship_id: i64, fighter: &Fighter, docked: bool, now: i64) -> Result<(), StoreError> {
    let v = &fighter.vitals;
    let mut set = vec![(attr::ARMOR_DAMAGE, v.armor_damage), (attr::DAMAGE, v.hull_damage)];
    let mut clear = Vec::new();
    if docked {
        clear.extend([attr::SHIELD_CHARGE, attr::CHARGE]);
    } else {
        set.extend([(attr::SHIELD_CHARGE, v.shield.at(now)), (attr::CHARGE, v.capacitor.at(now))]);
    }
    let key = |a: i32| [store::Value::Int(ship_id), store::Value::Int(i64::from(a))];
    for (a, value) in set {
        if value <= 0.0 && matches!(a, attr::ARMOR_DAMAGE | attr::DAMAGE) {
            tx.delete_where("entity_attributes", &["itemID", "attributeID"], &key(a))?;
        } else {
            let [item, attribute] = key(a);
            tx.upsert(
                "entity_attributes",
                &["itemID", "attributeID"],
                &[("itemID", item), ("attributeID", attribute), ("valueInt", store::Value::Null), ("valueFloat", store::Value::Float(value))],
                &[("valueInt", store::Value::Null), ("valueFloat", store::Value::Float(value))],
            )?;
        }
    }
    for a in clear {
        tx.delete_where("entity_attributes", &["itemID", "attributeID"], &key(a))?;
    }
    Ok(())
}

// ---- targeting --------------------------------------------------------------------------

pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let outcome = match call.method.as_str() {
        "AddTarget" => add_target(ctx, call.arg_id(0)),
        "RemoveTarget" => remove_target(ctx, call.arg_id(0)),
        "ClearTargets" => clear_targets(ctx),
        "GetTargets" => Outcome::Value(Value::List(with_fight(ctx, |_, f, _| f.locked().map(Value::Int).collect()).unwrap_or_default())),
        "GetTargeters" => Outcome::Value(Value::List(targeters(ctx).into_iter().map(Value::Int).collect())),
        "Activate" => activate(ctx, call).await,
        "Deactivate" => deactivate(ctx, call).await,
        _ => return None,
    };
    Some(outcome)
}

/// Runs `f` on the caller's ship in space, under the world lock.
fn with_fight<T>(ctx: &Ctx<'_>, f: impl FnOnce(&mut crate::ballpark::Park, &mut Fighter, i64) -> T) -> Option<T> {
    let (system, ship_id) = (ctx.session.solar_system_id?, ctx.session.ship_id?);
    let mut world = ctx.world.lock();
    let park = world.parks.get_mut(&system)?;
    let mut fighter = park.ships.get_mut(&ship_id)?.fight.take()?;
    let result = f(park, &mut fighter, ship_id);
    if let Some(ship) = park.ships.get_mut(&ship_id) {
        ship.fight = Some(fighter);
    }
    Some(result)
}

fn user_error(msg: &str, args: Vec<(&str, Value)>) -> Outcome {
    Outcome::UserError { msg: msg.into(), args: args.into_iter().map(|(k, v)| (k.into(), v)).collect() }
}

fn add_target(ctx: &Ctx<'_>, target: i64) -> Outcome {
    let now = combat::now_ms();
    let result = with_fight(ctx, |park, fight, me| -> Result<Value, Outcome> {
        let locked: Vec<Value> = fight.locked().map(Value::Int).collect();
        if fight.locks.contains_key(&target) {
            return Ok(Value::Tuple(vec![Value::Int(i64::from(!fight.is_locked(target))), Value::List(locked)]));
        }
        if target == me {
            return Err(Outcome::custom("You cannot target yourself."));
        }
        if park.ships.get(&me).is_some_and(|s| s.cloaked) {
            return Err(user_error("DeniedTargetingCloaked",vec![]));
        }
        if now < fight.cloak_history.targeting_at {
            return Err(user_error("DeniedTargetAfterCloak",vec![]));
        }
        let name = park
            .ships
            .get(&target)
            .filter(|s| !s.cloaked)
            .map(|s| s.pilot.name.clone())
            .or_else(|| park.rocks.get(&target).map(|r| r.name.clone()))
            .or_else(|| park.structures.get(&target).map(|s| s.name.clone()))
            .or_else(|| park.celestial(target).map(|c| c.name.clone()));
        let Some(name) = name else {
            return Err(user_error("TargetCantValidate", Vec::new()));
        };
        if park.ships.get(&me).is_some_and(|s| s.warping()) {
            return Err(Outcome::custom("You cannot lock targets while warping."));
        }
        if fight.afflictions.jammed {
            return Err(Outcome::custom("Your targeting systems are being jammed."));
        }
        if fight.locks.len() >= fight.max_locked {
            return Err(user_error("TargetingSystemsAlreadyFullyUtilized", vec![("limit", Value::Int(fight.max_locked as i64))]));
        }
        let distance = park.surface_distance(me, target).unwrap_or(f64::MAX);
        if distance > fight.lock_range() {
            return Err(user_error("TargetOutOfRangeFar", vec![("targetname", Value::text(name))]));
        }
        let signature = match park.ships.get(&target) {
            Some(s) => s.fight.as_ref().map_or(100.0, |f| f.sig()),
            None => park.ball(target).map_or(100.0, |(_, r)| r),
        };
        let seconds = combat::lock_seconds(fight.scan_res(), signature);
        fight.locks.insert(target, Lock::Pending(now + (seconds * 1_000.0) as i64));
        debug!("locking {target} ({name}) from {distance:.0} m: {seconds:.1}s");
        Ok(Value::Tuple(vec![Value::Int(1), Value::List(locked)]))
    });
    match result {
        Some(Ok(v)) => Outcome::Value(v),
        Some(Err(refusal)) => refusal,
        None => Outcome::custom("You are not in space."),
    }
}

fn remove_target(ctx: &Ctx<'_>, target: i64) -> Outcome {
    unlock(ctx, Some(target));
    Outcome::Value(Value::None)
}

fn clear_targets(ctx: &Ctx<'_>) -> Outcome {
    unlock(ctx, None);
    Outcome::Value(Value::None)
}

/// Drops one lock, or all of them, and tells both sides.
fn unlock(ctx: &Ctx<'_>, only: Option<i64>) {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return };
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return };
    let Some(fight) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) else { return };
    let dropped: Vec<(i64, Lock)> = match only {
        Some(t) => fight.locks.remove(&t).map(|l| (t, l)).into_iter().collect(),
        None => std::mem::take(&mut fight.locks).into_iter().collect(),
    };
    let mut out = crate::heartbeat::Out::default();
    for (t, lock) in &dropped {
        if *lock == Lock::Locked {
            fight::tell_target_lost(park, ship_id, *t, &mut out);
        }
    }
    let event = match only {
        Some(t) => Value::Tuple(vec![Value::bytes("lost"), Value::Int(t), Value::None]),
        None => Value::Tuple(vec![Value::bytes("clear")]),
    };
    out.note(ctx.client_id, "OnTarget", event);
    out.deliver(&world);
}

/// Ships that have the caller locked.
fn targeters(ctx: &Ctx<'_>) -> Vec<i64> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Vec::new() };
    let world = ctx.world.lock();
    let Some(park) = world.parks.get(&system) else { return Vec::new() };
    park.ships
        .values()
        .filter(|s| s.fight.as_ref().is_some_and(|f| f.is_locked(ship_id)))
        .map(|s| s.id)
        .collect()
}

// ---- modules ------------------------------------------------------------------------------

/// What dgmEffects says about an effect.
struct EffectRow {
    id: i64,
    name: String,
    category: i16,
    guid: String,
    offensive: bool,
    warp_safe: bool,
    duration_attr: Option<i32>,
    discharge_attr: Option<i32>,
    range_attr: Option<i32>,
    falloff_attr: Option<i32>,
}

/// The first effect of that name, in the static data's order (one name is
/// used twice). The static data is in memory; the context stays for the
/// callers written against the query this once was.
async fn effect_by_name(_ctx: &Ctx<'_>, name: &str) -> Option<EffectRow> {
    let r = crate::sde::table("dgmEffects").rows().find(|r| r.text("effectName") == Some(name))?;
    let attribute = |c: &str| r.int(c).map(|v| v as i32);
    Some(EffectRow {
        id: r.int("effectID").unwrap_or_default(),
        name: name.to_string(),
        category: r.int("effectCategory").unwrap_or(0) as i16,
        guid: r.text("guid").unwrap_or_default().to_string(),
        offensive: r.int("isOffensive").unwrap_or(0) != 0,
        duration_attr: attribute("durationAttributeID"),
        discharge_attr: attribute("dischargeAttributeID"),
        range_attr: attribute("rangeAttributeID"),
        falloff_attr: attribute("falloffAttributeID"),
        warp_safe: r.int("isWarpSafe").unwrap_or(0) != 0,
    })
}

fn effect_name(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_string())
}

/// Activate(itemID, effectName, target, repeat).
async fn activate(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let item_id = call.arg_id(0);
    let Some(name) = effect_name(call.args.get(1)) else {
        return Outcome::custom("Which effect should that module run?");
    };
    if name == "online" {
        return crate::svc::fitting::set_online(ctx, item_id, true).await;
    }
    let target = call.args.get(2).and_then(|v| v.as_i64());
    let repeat = call.args.get(3).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id)
    else {
        return Outcome::custom("Modules can only be activated in space.");
    };
    let Some(effect) = effect_by_name(ctx, &name).await else {
        return Outcome::Unknown(format!("dogmaIM::Activate of unknown effect {name:?}"));
    };

    let running: HashSet<i64> = {
        let world = ctx.world.lock();
        let ship = world.parks.get(&system).and_then(|p| p.ships.get(&ship_id));
        if let Some(ship) = ship {
            if let Err(error) = check_warp_activation(ship, effect.warp_safe) {
                return Outcome::custom(error);
            }
        }
        if ship.is_some_and(|s| s.cloaked) && effect.id != effect::CLOAKING && effect.id != effect::CLOAKING_WARP_SAFE {
            return Outcome::custom("You cannot activate modules while cloaked.");
        }
        match ship.and_then(|s| s.fight.as_ref()) {
            Some(f) if f.module(item_id).is_some() => return Outcome::Value(Value::None),
            Some(f) => f.modules.iter().map(|m| m.module_id).collect(),
            None => return Outcome::custom("Your ship is not in space."),
        }
    };
    let loadout = match ctx.store.read(|db| Loadout::load(db, ctx.dogma, char_id, Some(ship_id), &running)) {
        Ok(l) => l,
        Err(e) => {
            error!("activating {item_id}: {}", crate::logging::chain(&e));
            return Outcome::custom("The module could not be read.");
        }
    };
    let planned = plan(&loadout, item_id, &effect, target, repeat);
    drop(loadout);
    // What some modules need before they start: where they may run, fuel.
    let planned = match planned {
        Ok(p) => special_start(ctx, system, ship_id, p).await,
        Err(why) => Err(why),
    };
    let fuel = planned.as_ref().ok().and_then(|plan| plan.fuel);
    let module = match planned {
        Ok(p) => finish(ctx, p).await,
        Err(why) => Err(why),
    };
    let module = match module {
        Ok(m) => m,
        Err(why) => {
            info!("activate {item_id} ({name}) refused: {why}");
            return Outcome::custom(why);
        }
    };

    let refits = module.action == Action::Passive || matches!(module.action,Action::Cloak {..});
    let started = if let Some((fuel_type, quantity)) = fuel {
        match activation::funded(ctx, system, ship_id, char_id, module, fuel_type, quantity).await {
            Ok(started) => started,
            Err(error) => return Outcome::custom(error),
        }
    } else {
        let mut world = ctx.world.lock();
        if let Err(error) = validate_activation(&world, system, ship_id, char_id, ctx.client_id, &module, false) { return error; }
        publish_activation(&mut world, system, ship_id, char_id, ctx.client_id, module)
    };
    info!(
        "activated {} ({item_id}) {} on {:?}, {} ms cycle",
        started.name, effect.name, started.target, started.duration_ms
    );
    match started.action {
        Action::Scan => crate::svc::scan::scan_started(ctx, system, ship_id, item_id, started.duration_ms).await,
        _ => {}
    }
    if refits {
        ctx.store.read(|db| refit(db, ctx.dogma, ctx.world, ctx.client_id, char_id, ship_id, system));
    }
    Outcome::Value(Value::None)
}

fn validate_activation(world: &crate::world::WorldState, system: i64, ship_id: i64, char_id: i64, client_id: i64, module: &Active, reserved: bool) -> Result<(), Outcome> {
    let Some(park) = world.parks.get(&system) else { return Err(Outcome::custom("Your ship is not in space.")); };
    let Some(ship) = park.ships.get(&ship_id) else { return Err(Outcome::custom("Your ship is not in space.")); };
    if ship.pilot.char_id != char_id || ship.client_id != client_id || ship.unpiloted || ship.identity_reserved != reserved {
        return Err(Outcome::custom("Your ship is changing pilots or is no longer under your control."));
    }
    check_warp_activation(ship, module.warp_safe).map_err(Outcome::custom)?;
    if module.disallow_warp_alignment && ship.warping() {
        return Err(Outcome::custom("That module cannot be activated while aligning to warp."));
    }
    if ship.cloaked && !matches!(module.action,Action::Cloak {..}) {
        return Err(Outcome::custom("You cannot activate modules while cloaked."));
    }
    let Some(fighter) = &ship.fight else { return Err(Outcome::custom("Your ship is not in space.")); };
    if matches!(module.action,Action::Cloak {..}) {
        crate::cloak::may_cloak(world,system,ship_id).map_err(Outcome::custom)?;
        if fighter.cloak_history.module_ready.get(&module.module_id).is_some_and(|ready| *ready>combat::now_ms()) {
            return Err(user_error("ModuleReactivationDelayed",vec![
                ("item",Value::Int(module.type_id)),("delay",Value::Double((fighter.cloak_history.module_ready[&module.module_id]-combat::now_ms()).max(0) as f64/1000.0))]));
        }
    }
    if fighter.vitals.hull_damage >= fighter.vitals.hull_max || fighter.module(module.module_id).is_some() {
        return Err(Outcome::custom("That module cannot be started on this ship now."));
    }
    check_target(park,ship_id,module).map_err(Outcome::custom)?;
    if let Some(target) = module.target {
        if !fighter.is_locked(target) {
            return Err(user_error("TargetNotLocked",vec![("modulename",Value::text(module.name.clone())),("targetname",Value::Int(target))]));
        }
    }
    Ok(())
}

/// Caller either validates under this lock or holds a live identity reservation
/// across its fuel transaction. No fallible work follows a committed debit.
fn publish_activation(world: &mut crate::world::WorldState, system: i64, ship_id: i64, char_id: i64, client_id: i64, mut module: Active) -> Active {
    let now = combat::now_ms();
    let park = world.parks.get_mut(&system).expect("validated activation park");
    let tick = park.tick;
    let ship = park.ships.get_mut(&ship_id).expect("validated activation ship");
    module.started_ms = now;
    module.next_ms = if module.continuous() {i64::MAX} else {now};
    ship.fight.as_mut().expect("validated activation fighter").modules.push(module.clone());
    world.notify(&[client_id],"OnGodmaShipEffect",fight::effect_event(char_id,ship_id,&module,true,now));
    space::broadcast(world,system,None,vec![fight::module_fx(tick,ship_id,&module,true)]);
    if module.action == Action::Cyno { crate::svc::jumps::cyno_lit(world,system,char_id,ship_id,module.type_id); }
    if matches!(module.action,Action::Cloak {..}) { crate::cloak::cloaked(world,system,ship_id); }
    module
}

fn check_warp_activation(ship: &Ship, warp_safe: bool) -> Result<(), &'static str> {
    if ship.in_warp() && !warp_safe {
        Err("That module cannot be activated while in warp.")
    } else {
        Ok(())
    }
}

/// Whether the module's target makes sense for what it does.
fn check_target(park: &crate::ballpark::Park, ship_id: i64, m: &Active) -> Result<(), String> {
    let Some(t) = m.target else { return Ok(()) };
    match m.action {
        Action::Turret { .. } | Action::Missile { .. } => {
            if !park.ships.get(&t).is_some_and(|s| s.fight.is_some()) && !park.structures.contains_key(&t) {
                return Err("That is not something you can shoot.".into());
            }
        }
        Action::Mining { range, .. } => {
            if !park.rocks.contains_key(&t) {
                return Err("You can only mine asteroids.".into());
            }
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > range {
                return Err(format!("The asteroid is {distance:.0} m away; your {} reaches {range:.0} m.", m.name));
            }
        }
        ref a if a.projected() => {
            if !park.ships.get(&t).is_some_and(|s| s.fight.is_some()) {
                return Err("That only works on ships.".into());
            }
            let (optimal, falloff) = m.reach.unwrap_or((0.0, 0.0));
            let reach = optimal + 2.0 * falloff;
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > reach {
                return Err(format!("The target is {distance:.0} m away; your {} reaches {reach:.0} m.", m.name));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A module's cycle as far as its attributes say; [`finish`] fills in what
/// needs the database.
struct Plan {
    module: Active,
    /// The charge it burns: (item, type, per cycle).
    burns: Option<(i64, i64, i64)>,
    /// A weapon with built-in ammunition of this type, whose damage is still
    /// to be read (times the damage multiplier).
    built_in: Option<(i64, f64)>,
    /// Fuel it burns from the hold to start: (type, quantity).
    fuel: Option<(i64, i64)>,
}

/// The cycle a module runs, from its computed attributes and its charge's.
fn plan(loadout: &Loadout<'_>, item_id: i64, effect: &EffectRow, target: Option<i64>, repeat: i64) -> Result<Plan, String> {
    let Some(i) = loadout.find(item_id).filter(|i| loadout.pieces[*i].category_id == category::MODULE) else {
        return Err("That module is not fitted to your ship.".into());
    };
    let piece = &loadout.pieces[i];
    if piece.state == State::Offline {
        return Err("That module is offline.".into());
    }
    let hp = loadout.value(i, attr::HP);
    if hp > 0.0 && loadout.value(i, attr::DAMAGE) >= hp {
        return Err("That module is burned out. Repair it before activating it.".into());
    }
    if !piece.has_effect(effect.id as i32) {
        return Err("That module cannot do that.".into());
    }
    let v = |a: i32| loadout.value(i, a);
    let continuous_cloak = effect.duration_attr.is_none() && matches!(effect.id,effect::CLOAKING|effect::CLOAKING_WARP_SAFE);
    let duration = effect.duration_attr.map_or(-1.0, v);
    if duration <= 0.0 && !continuous_cloak {
        return Err("That module has no cycle to run.".into());
    }
    if continuous_cloak {
        if loadout.pieces.iter().filter(|p|p.role==crate::dogma::Role::Module && p.group_id==330).count()>1 {
            return Err("You cannot cloak with more than one cloaking device fitted.".into());
        }
        if loadout.ship().is_some_and(|ship|loadout.value(ship,1163)==0.0) {
            return Err("An active module is preventing your ship from cloaking.".into());
        }
    }
    let charge = loadout.charge_of(i);
    let burns = |per_cycle: i64| {
        charge
            .map(|c| &loadout.pieces[c])
            .filter(|p| !REUSABLE_CHARGE_GROUPS.contains(&p.group_id))
            .map(|p| (p.item_id, p.type_id, per_cycle))
    };

    let mut burned = None;
    let mut built_in = None;
    let action = match effect.id {
        effect::TARGET_ATTACK | effect::PROJECTILE_FIRED => {
            let multiplier = v(attr::DAMAGE_MULTIPLIER);
            let base = match charge {
                Some(c) => attr::DAMAGE_TYPES.map(|d| loadout.value(c, d)),
                None if loadout.has(i, attr::AMMO_LOADED) => {
                    built_in = Some((v(attr::AMMO_LOADED) as i64, multiplier));
                    [0.0; 4]
                }
                None => return Err("Load some ammunition first.".into()),
            };
            let per_cycle = if loadout.has(i, attr::CHARGE_RATE) { v(attr::CHARGE_RATE).max(1.0) as i64 } else { 1 };
            burned = burns(per_cycle);
            Action::Turret {
                damage: base.map(|d| d * multiplier),
                gun: Gun {
                    optimal: v(attr::MAX_RANGE),
                    falloff: v(attr::FALLOFF),
                    tracking: v(attr::TRACKING_SPEED),
                    optimal_sig: v(attr::OPTIMAL_SIG_RADIUS),
                },
            }
        }
        // A probe launcher fires probes, not missiles.
        effect::USE_MISSILES if charge.is_some_and(|c| matches!(loadout.pieces[c].group_id, 479 | 492)) => {
            burned = burns(1);
            Action::Scan
        }
        effect::SYSTEM_SCAN => return Err("Load probes and activate the launcher to send one out.".into()),
        effect::SHIP_SCAN | effect::CARGO_SCAN | effect::SURVEY_SCAN => Action::Scan,
        effect::USE_MISSILES => {
            let Some(c) = charge else { return Err("Load some missiles first.".into()) };
            let m = |a: i32| loadout.value(c, a);
            burned = burns(1);
            let flight_ms = m(attr::EXPLOSION_DELAY);
            Action::Missile {
                damage: attr::DAMAGE_TYPES.map(m),
                flight_ms: flight_ms as i64,
                range: m(attr::MAX_VELOCITY) * flight_ms / 1_000.0,
                cloud: m(attr::AOE_CLOUD_SIZE),
                aoe_velocity: m(attr::AOE_VELOCITY),
            }
        }
        effect::SHIELD_BOOSTING => Action::ShieldBoost(v(attr::SHIELD_BONUS)),
        effect::ARMOR_REPAIR => Action::ArmorRepair(v(attr::ARMOR_DAMAGE_AMOUNT)),
        effect::STRUCTURE_REPAIR => Action::HullRepair(v(attr::STRUCTURE_DAMAGE_AMOUNT)),
        effect::POWER_BOOSTER => {
            let Some(c) = charge else { return Err("Load some capacitor booster charges first.".into()) };
            burned = burns(1);
            Action::CapBoost(loadout.value(c, attr::CAPACITOR_BONUS))
        }
        effect::MINING | effect::MINING_LASER => Action::Mining { amount: v(attr::MINING_AMOUNT), range: v(attr::MAX_RANGE) },
        effect::ENERGY_DESTABILIZATION | effect::ENERGY_DESTABILIZATION_NEW => {
            Action::Neutralize(v(attr::ENERGY_DESTABILIZATION_AMOUNT))
        }
        effect::ENERGY_VAMPIRE | effect::ENERGY_NOSFERATU => Action::Leech(v(attr::POWER_TRANSFER_AMOUNT)),
        effect::ENERGY_TRANSFER => Action::GiveCapacitor(v(attr::POWER_TRANSFER_AMOUNT)),
        effect::SHIELD_TRANSFER => Action::GiveShield(v(attr::SHIELD_BONUS)),
        effect::TARGET_ARMOR_REPAIR => Action::GiveArmor(v(attr::ARMOR_DAMAGE_AMOUNT)),
        effect::MODIFY_TARGET_SPEED | effect::DECREASE_TARGET_SPEED => {
            Action::Web((1.0 + v(attr::SPEED_FACTOR) / 100.0).clamp(0.01, 1.0))
        }
        effect::WARP_SCRAMBLE => Action::Scramble(v(attr::WARP_SCRAMBLE_STRENGTH).max(1.0)),
        effect::JAM => Action::Jam(attr::JAMMER_STRENGTHS.map(v)),
        effect::DAMPEN | effect::DAMPEN_TEST => Action::Dampen {
            range: multiplier_or_one(v(attr::MAX_TARGET_RANGE_MULTIPLIER)),
            resolution: multiplier_or_one(v(attr::SCAN_RESOLUTION_MULTIPLIER)),
        },
        effect::DISRUPT | effect::DISRUPT_TEST => Action::Disrupt {
            range: multiplier_or_one(v(attr::MAX_RANGE_MULTIPLIER)),
            tracking: multiplier_or_one(v(attr::TRACKING_SPEED_MULTIPLIER)),
        },
        effect::TARGET_PAINT | effect::TARGET_PAINT_2 => Action::Paint(1.0 + v(attr::SIGNATURE_RADIUS_BONUS) / 100.0),
        effect::CYNOSURAL_GENERATION | effect::CYNOSURAL_GENERATION_45017 => Action::Cyno,
        effect::JUMP_PORTAL_GENERATION => Action::Portal,
        effect::EMP_WAVE_GRID => Action::Doomsday { damage: attr::DAMAGE_TYPES.map(v), range: DOOMSDAY_RANGE },
        effect::CLOAKING | effect::CLOAKING_WARP_SAFE => Action::Cloak {
            warp_safe: effect.warp_safe,
            targeting_delay_ms: v(560).max(0.0) as i64,
            reactivation_delay_ms: v(669).max(0.0) as i64,
        },
        _ => Action::Passive,
    };
    // Generators and superweapons burn fuel from the hold to start.
    let fuel = matches!(action, Action::Cyno | Action::Portal | Action::Doomsday { .. })
        .then(|| (v(crate::svc::jumps::ATTR_CONSUMPTION_TYPE) as i64, v(crate::svc::jumps::ATTR_CONSUMPTION_QUANTITY) as i64))
        .filter(|(t, q)| *t > 0 && *q > 0);
    // They run one cycle at a time.
    let repeat = if continuous_cloak || matches!(action, Action::Cyno | Action::Portal | Action::Doomsday { .. } | Action::Scan) { 0 } else { repeat };
    // A launcher's useMissiles is an activation effect: the target belongs to
    // its charge's missileLaunching, and the client sends it for that
    // (shipmodulebutton.ActivateEffect).
    let targeted = effect.category == TARGETED || matches!(action, Action::Missile { .. });
    if targeted && target.is_none() {
        return Err("That module needs a target.".into());
    }
    let reach =action.projected().then(|| (effect.range_attr.map_or(0.0, v), effect.falloff_attr.map_or(0.0, v)));
    if action.projected() && effect.category != TARGETED {
        return Err("That module works on a target.".into());
    }
    // A missile's look lives on the charge's launching effect.
    let guid = match (effect.id, charge) {
        (effect::USE_MISSILES, Some(_)) => "effects.MissileDeployment".to_string(),
        _ => effect.guid.clone(),
    };
    let module = Active {
        module_id: item_id,
        type_id: piece.type_id,
        effect_id: effect.id,
        guid,
        offensive: effect.offensive,
        warp_safe: effect.warp_safe,
        disallow_warp_alignment: v(1245) != 0.0,
        action,
        target: if targeted { target } else { None },
        ammo: None,
        duration_ms: duration as i64,
        cap_need: effect.discharge_attr.map_or(0.0, v).max(0.0),
        first_cycle_prepaid: false,
        started_ms: 0,
        next_ms: 0,
        cycles: 0,
        repeat: repeat.min(REPEAT_FOREVER),
        stopping: false,
        name: String::new(),
        reach,
        landed: true,
    };
    Ok(Plan { module, burns: burned, built_in, fuel })
}

/// How far a doomsday reaches: the whole grid.
const DOOMSDAY_RANGE: f64 = 250_000.0;

/// Checks that some modules need before they start, and the fuel they burn.
async fn special_start(ctx: &Ctx<'_>, system: i64, ship_id: i64, plan: Plan) -> Result<Plan, String> {
    let security = ctx.store.read(|db| db.table("mapSolarSystems").get_id(system).and_then(|r| r.float("security"))).unwrap_or(0.0);
    match plan.module.action {
        Action::Cyno => crate::svc::jumps::may_light_cyno(security)?,
        Action::Portal => return Err("Open a jump portal from the gang menu: Bridge to member.".into()),
        Action::Cloak { .. } => crate::cloak::may_cloak(&ctx.world.lock(), system, ship_id)?,
        Action::Scan => crate::svc::scan::may_scan(ctx, ship_id).await?,
        _ => {}
    }
    Ok(plan)
}

/// The rest of a module's cycle: its name, its charges left, built-in damage.
async fn finish(ctx: &Ctx<'_>, plan: Plan) -> Result<Active, String> {
    let mut module = plan.module;
    module.name = type_name(module.type_id);
    if let Some((item_id, type_id, per_cycle)) = plan.burns {
        let quantity = ctx.store.read(|db| db.table("entity").get_id(item_id).and_then(|r| r.int("quantity"))).unwrap_or(0);
        if quantity < per_cycle {
            return Err("That module is out of charges.".into());
        }
        module.ammo = Some(Ammo { item_id, type_id, quantity, per_cycle });
    }
    if let (Some((type_id, multiplier)), Action::Turret { damage, .. }) = (plan.built_in, &mut module.action) {
        *damage = type_damage(type_id).map(|d| d * multiplier);
    }
    Ok(module)
}

/// A multiplier attribute, where 0 (absent) means no change.
fn multiplier_or_one(v: f64) -> f64 {
    if v > 0.0 { v } else { 1.0 }
}

fn type_name(type_id: i64) -> String {
    crate::sde::get().type_(type_id).map_or_else(|| format!("type {type_id}"), |t| t.name.to_string())
}

/// A charge type's damage, for a weapon with built-in ammunition.
fn type_damage(type_id: i64) -> combat::Damage {
    let mut damage = [0.0; 4];
    for (a, value) in crate::sde::get().type_attributes(type_id) {
        if let Some(at) = attr::DAMAGE_TYPES.iter().position(|d| d == a) {
            damage[at] = *value;
        }
    }
    damage
}

/// Timed effects stop at their cycle boundary; continuous effects stop now.
async fn deactivate(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let item_id = call.arg_id(0);
    if effect_name(call.args.get(1)).as_deref() == Some("online") {
        return crate::svc::fitting::set_online(ctx, item_id, false).await;
    }
    if let (Some(system),Some(ship_id),Some(pilot))=(ctx.session.solar_system_id,ctx.session.ship_id,ctx.session.char_id) {
        let continuous={let mut world=ctx.world.lock();
        let continuous=world.parks.get(&system).and_then(|p|p.ships.get(&ship_id))
            .filter(|s|s.pilot.char_id==pilot && s.client_id==ctx.client_id && !s.identity_reserved)
            .and_then(|s|s.fight.as_ref()).and_then(|f|f.module(item_id)).is_some_and(Active::continuous);
        if continuous {
            let mut out=crate::heartbeat::Out::default();
            fight::stop(world.parks.get_mut(&system).unwrap(),ship_id,item_id,combat::now_ms(),&mut out,None);
            let jobs=out.deliver(&world);world.pending_jobs.extend(jobs);
            crate::cloak::uncloaked(&mut world,system,ship_id);
        }
        continuous};
        if continuous {
            ctx.store.read(|db| refit(db,ctx.dogma,ctx.world,ctx.client_id,pilot,ship_id,system));
            return Outcome::Value(Value::None);
        }
    }
    let found = with_fight(ctx, |_, fight, _| match fight.module_mut(item_id) {
        Some(m) => {
            m.stopping = true;
            true
        }
        None => false,
    });
    if found != Some(true) {
        debug!("deactivate {item_id}: not running");
    }
    Outcome::Value(Value::None)
}

/// Recomputes a ship's attributes for the modules now running, moves the new
/// numbers into the park, and tells the pilot what changed.
/// A fleet booster or command chain changed. Rebuild the same loadout with the
/// old external bonuses to send attribute deltas, then update the live fighter.
pub fn fleet_refit(
    db: &Db,
    catalog: &Catalog,
    world: &crate::world::World,
    char_id: i64,
    previous: &std::collections::BTreeMap<i32, f64>,
) -> Result<(), StoreError> {
    let ship = {
        let w = world.lock();
        w.parks.iter().find_map(|(system, park)| park.ships.values().find(|s| s.pilot.char_id == char_id && s.is_player())
            .map(|s| (*system, s.id, s.client_id, s.fight.as_ref().map(|f| f.applied.clone()).unwrap_or_default())))
    };
    let Some((system, ship_id, client_id, active)) = ship else { return Ok(()) };
    let active_hash = active.iter().copied().collect();
    let mut before = Loadout::load(db, catalog, char_id, Some(ship_id), &active_hash)?;
    let after = before.snapshot();
    before.apply_fleet_bonuses(previous);
    let before = before.snapshot();
    let Some(profile) = profile(db, catalog, char_id, ship_id, &active, combat::now_ms())? else { return Ok(()) };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return Ok(()) };
    let tick = park.tick;
    let Some(ship) = park.ships.get_mut(&ship_id).filter(|s| s.pilot.char_id == char_id) else { return Ok(()) };
    let old_speed = ship.stats.max_velocity;
    apply_profile(ship, profile, combat::now_ms());
    // Running mining cycles already hold a computed yield. Keep it in sync
    // with the loadout without resetting their cycle clocks.
    let now_bonus = catalog.fleet_bonuses(char_id);
    let yield_ratio = (1.0 + now_bonus.get(&77).copied().unwrap_or(0.0) / 100.0)
        / (1.0 + previous.get(&77).copied().unwrap_or(0.0) / 100.0);
    if let Some(fight) = &mut ship.fight {
        for module in &mut fight.modules {
            if let Action::Mining { amount, .. } = &mut module.action { *amount *= yield_ratio; }
        }
    }
    let speed = ship.stats.max_velocity;
    if (speed - old_speed).abs() > 1e-6 {
        space::broadcast(&w, system, None, vec![entry(tick, "SetMaxSpeed", vec![Value::Int(ship_id), Value::Double(speed)])]);
    }
    let changes = super::fitting::attribute_changes(char_id, &before, &after);
    if !changes.is_empty() { w.notify(&[client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)])); }
    Ok(())
}

pub fn refit(
    db: &Db,
    catalog: &Catalog,
    world: &crate::world::World,
    client_id: i64,
    char_id: i64,
    ship_id: i64,
    system: i64,
) {
    let (before_set, now_set): (BTreeSet<i64>, BTreeSet<i64>) = {
        let w = world.lock();
        let Some(fight) = w.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()) else { return };
        let passive = fight.modules.iter().filter(|m| m.action == Action::Passive || matches!(m.action,Action::Cloak {..})).map(|m| m.module_id).collect();
        (fight.applied.clone(), passive)
    };
    if before_set == now_set {
        return;
    }
    let as_hash = |s: &BTreeSet<i64>| s.iter().copied().collect::<HashSet<i64>>();
    let result = (|| {
        let before = Loadout::load(db, catalog, char_id, Some(ship_id), &as_hash(&before_set))?.snapshot();
        let after = Loadout::load(db, catalog, char_id, Some(ship_id), &as_hash(&now_set))?.snapshot();
        let p = profile(db, catalog, char_id, ship_id, &now_set, combat::now_ms())?;
        Ok::<_, StoreError>((before, after, p))
    })();
    let (before, after, p) = match result {
        Ok((b, a, Some(p))) => (b, a, p),
        Ok(_) => return,
        Err(e) => {
            error!("recomputing ship {ship_id}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    let tick = park.tick;
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    let old_speed = ship.stats.max_velocity;
    let old_mass = ship.stats.mass;
    apply_profile(ship, p, combat::now_ms());
    let mut entries = Vec::new();
    if (ship.stats.max_velocity - old_speed).abs() > 1e-6 {
        entries.push(entry(tick, "SetMaxSpeed", vec![Value::Int(ship_id), Value::Double(ship.stats.max_velocity)]));
    }
    if (ship.stats.mass - old_mass).abs() > 1e-6 {
        entries.push(entry(tick, "SetBallMass", vec![Value::Int(ship_id), Value::Double(ship.stats.mass)]));
    }
    debug!("ship {ship_id} refitted: {:.0} m/s, {:.0} kg", ship.stats.max_velocity, ship.stats.mass);
    space::broadcast(&w, system, None, entries);
    let changes = crate::svc::fitting::attribute_changes(char_id, &before, &after);
    if !changes.is_empty() {
        w.notify(&[client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
    }
}
