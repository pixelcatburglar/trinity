//! Scanning (scanner.py, scansvc.py, moonscan.py, surveyscan.py, shipui.py).
//!
//! - **Directional scan** (beyonce.ConeScan): what lies in a cone from the
//!   ship, within a range: ships (not cloaked ones), structures, celestials,
//!   beacons.
//! - **Scanner probes**: a Scan Probe Launcher fires one into space where the
//!   ship is, for its lifetime (explosionDelay), covering a sphere of its scan
//!   range (in AU). beyonce.GetScans lists the pilot's probes and the last
//!   analysis; beyonce.CombineScans(probes, groupID) analyses with as many
//!   probes as the type works in (probesInGroup), for one group: ships of a
//!   group, stations, or cosmic signatures (hidden deadspace complexes).
//!   What lies within reach of every probe is found, its position known to
//!   within the probes' deviation; the analysis runs as the launcher's
//!   systemScan effect and the results come when it ends. A result can be
//!   warped to (WarpToStuff 'scan'). GiveProbe hands a probe to another pilot.
//! - **Survey probes** (Quest, Discovery, Gaze) fly for their lifetime and
//!   then survey every moon within their range of where they were launched:
//!   OnMoonScanComplete with what each moon holds (the same materials moon
//!   harvesters mine), kept in `moonScans`.
//! - **Ship, cargo and survey scanners**: at the end of a cycle,
//!   OnShipScanComplete (the target's capacitor and modules),
//!   OnCargoScanComplete (its hold) and OnSurveyScanComplete (asteroids in
//!   range and the ore in each).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::{Park, Vec3};
use crate::beacons::{self, Beacon, Kind};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Store, StoreError};
use crate::svc::rows::rowset;

const AU: f64 = 149_597_870_700.0;
const GROUP_SCANNER_PROBE: i64 = 479;
const GROUP_SURVEY_PROBE: i64 = 492;
const GROUP_PROBE_LAUNCHER: i64 = 481;
const GROUP_CARGO_SCANNER: i64 = 47;
const GROUP_SHIP_SCANNER: i64 = 48;
const GROUP_SURVEY_SCANNER: i64 = 49;
const GROUP_STATION: i64 = 15;
pub const GROUP_COSMIC_SIGNATURE: i64 = 502;
const TYPE_DEADSPACE_SIGNATURE: i64 = 19728;
const CATEGORY_SHIP: i64 = 6;
const ATTR_LIFETIME: i32 = 281;
const ATTR_SCAN_RANGE: i32 = 765;
const ATTR_PROBES_IN_GROUP: i32 = 794;
const ATTR_MIN_DEVIATION: i32 = 787;
const ATTR_MAX_DEVIATION: i32 = 788;
const ATTR_SURVEY_RANGE: i32 = 197;
const EFFECT_SYSTEM_SCAN: i64 = 1150;
/// How long an analysis takes.
const ANALYSIS_MS: i64 = 10_000;
const FLAG_CARGO: i64 = 5;

/// A pilot's probe out in space.
#[derive(Debug, Clone)]
pub struct Probe {
    pub owner: i64,
    pub type_id: i64,
    pub system: i64,
    pub pos: Vec3,
    pub until_ms: i64,
    /// AU.
    pub range: f64,
    pub group_count: i64,
    pub deviation: (f64, f64),
}

/// Something an analysis found.
#[derive(Debug, Clone)]
pub struct Found {
    pub id: i64,
    pub signal: f64,
    pub pos: Vec3,
    pub accuracy: f64,
    pub system: i64,
}

pub async fn beyonce(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id?;
    Some(match call.method.as_str() {
        "ConeScan" => cone_scan(ctx, call),
        "GetScans" => Outcome::Value(get_scans(ctx, me)),
        "CombineScans" => match combine(ctx, me, call.args.first(), call.arg_id(1)).await {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        },
        "GiveProbe" => match give(ctx, me, call.arg_id(0), call.arg_id(1)) {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        },
        // DestroyProbe(probeID): the scanner's menu (scanner.py:632-644).
        "DestroyProbe" => match destroy(ctx, me, call.arg_id(0)) {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        },
        _ => return None,
    })
}

/// ConeScan(angle, range, x, y, z): rows of (id, typeID, groupID).
fn cone_scan(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let f = |i: usize| call.args.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let (angle, range) = (f(0).clamp(0.01, std::f64::consts::TAU), f(1).max(0.0));
    let dir = Vec3::new(f(2), f(3), f(4));
    let dir = if dir.len() > 1e-9 { dir.unit() } else { Vec3::new(0.0, 0.0, 1.0) };
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Outcome::custom("You are not in space.") };
    let w = ctx.world.lock();
    let Some(park) = w.parks.get(&system) else { return Outcome::Value(rowset(&["id", "typeID", "groupID"], Vec::new())) };
    let Some(me) = park.ships.get(&ship_id).map(|s| s.pos) else { return Outcome::custom("You are not in space.") };
    let half = angle / 2.0;
    let seen = |pos: Vec3| {
        let d = pos - me;
        let len = d.len();
        len <= range && (len < 1.0 || angle >= std::f64::consts::TAU - 1e-6 || (d.unit().dot(dir)).clamp(-1.0, 1.0).acos() <= half)
    };
    let mut lines: Vec<Value> = Vec::new();
    let mut add = |id: i64, type_id: i64, group_id: i64| lines.push(Value::List(vec![Value::Int(id), Value::Int(type_id), Value::Int(group_id)]));
    for s in park.ships.values().filter(|s| s.id != ship_id && !s.cloaked && seen(s.pos)) {
        add(s.id, s.pilot.type_id, s.pilot.group_id);
    }
    for s in park.structures.values().filter(|s| seen(s.pos)) {
        add(s.id, s.type_id, s.group_id);
    }
    for c in park.celestials.iter().filter(|c| seen(c.pos)) {
        add(c.id, c.type_id, c.group_id);
    }
    for b in park.beacons.values().filter(|b| !b.hidden && seen(b.pos) && !matches!(b.kind, Kind::Probe { .. })) {
        add(b.id, b.type_id, beacon_group(b));
    }
    Outcome::Value(rowset(&["id", "typeID", "groupID"], lines))
}

fn beacon_group(b: &Beacon) -> i64 {
    if b.type_id==crate::beacons::TYPE_COVERT_CYNOSURAL_FIELD {return 897;}
    match b.kind {
        Kind::Gate { .. } => 366,
        _ => 310,
    }
}

/// GetScans: {'probes': {probeID: probe}, 'results': [(id, signal, pos, accuracy)]}.
fn get_scans(ctx: &Ctx<'_>, me: i64) -> Value {
    let system = ctx.session.solar_system_id.unwrap_or(0);
    let now = crate::combat::now_ms();
    let w = ctx.world.lock();
    let probes: Vec<(Value, Value)> = w
        .probes
        .iter()
        .filter(|(_, p)| p.owner == me && p.system == system && p.until_ms > now)
        .map(|(id, p)| {
            let kv = key_val(vec![
                ("probeID", Value::Int(*id)),
                ("typeID", Value::Int(p.type_id)),
                ("expiry", Value::Long(crate::combat::filetime(p.until_ms))),
                ("pos", point(p.pos)),
                ("scanRange", Value::Double(p.range)),
            ]);
            (Value::Int(*id), kv)
        })
        .collect();
    let results: Vec<Value> = w
        .scan_results
        .get(&me)
        .map(|list| {
            list.iter()
                .filter(|r| r.system == system)
                .map(|r| Value::Tuple(vec![Value::Int(r.id), Value::Double((r.signal * 10.0).round() / 10.0), point(r.pos), Value::Double(r.accuracy)]))
                .collect()
        })
        .unwrap_or_default();
    Value::Dict(vec![(Value::bytes("probes"), Value::Dict(probes)), (Value::bytes("results"), Value::List(results))])
}

fn point(p: Vec3) -> Value {
    Value::Tuple(vec![Value::Double(p.x), Value::Double(p.y), Value::Double(p.z)])
}

fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// GiveProbe(charID, probeID).
fn give(ctx: &Ctx<'_>, me: i64, to: i64, probe: i64) -> Result<(), String> {
    let mut w = ctx.world.lock();
    let Some(p) = w.probes.get_mut(&probe).filter(|p| p.owner == me) else { return Err("That is not your probe.".into()) };
    p.owner = to;
    info!("{me} gave probe {probe} to {to}");
    Ok(())
}

/// Survey probes destroyed before their survey, which then does not run.
fn destroyed() -> &'static std::sync::Mutex<std::collections::HashSet<i64>> {
    static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i64>>> = std::sync::OnceLock::new();
    IDS.get_or_init(Default::default)
}

/// A pilot destroys one of their probes: it leaves the scanner and space.
fn destroy(ctx: &Ctx<'_>, me: i64, probe: i64) -> Result<(), String> {
    let mut w = ctx.world.lock();
    let Some(p) = w.probes.get(&probe).filter(|p| p.owner == me) else { return Err("That is not your probe.".into()) };
    let system = p.system;
    let survey = p.type_id;
    w.probes.remove(&probe);
    if crate::sde::get().type_(survey).is_some_and(|t| i64::from(t.group_id) == GROUP_SURVEY_PROBE) {
        destroyed().lock().unwrap_or_else(|e| e.into_inner()).insert(probe);
    }
    if let Some(park) = w.parks.get_mut(&system) {
        let entries = beacons::remove(park, &[probe]);
        if !entries.is_empty() {
            crate::svc::space::broadcast(&w, system, None, entries);
        }
    }
    info!("{me} destroyed probe {probe}");
    Ok(())
}

/// The launcher fitted to a ship, for the analysis's effect.
async fn launcher(ctx: &Ctx<'_>, ship_id: i64) -> Option<i64> {
    let launchers = crate::sde::get().types_in_group(GROUP_PROBE_LAUNCHER);
    ctx.store.read(|db| {
        db.table("entity")
            .find(&["locationID"], &[store::Value::Int(ship_id)])
            .filter(|e| e.int("typeID").is_some_and(|t| launchers.contains(&(t as i32))))
            .find(|e| e.int("flag").is_some_and(|f| (11..=34).contains(&f)))
            .and_then(|e| e.int("itemID"))
    })
}

fn scan_event(char_id: i64, ship_id: i64, module: i64, start: bool, started: i64, now: i64) -> Value {
    let environment = Value::List(vec![Value::Int(module), Value::Int(char_id), Value::Int(ship_id), Value::None, Value::None, Value::List(Vec::new()), Value::Int(EFFECT_SYSTEM_SCAN)]);
    Value::Tuple(vec![
        Value::Int(module),
        Value::Int(EFFECT_SYSTEM_SCAN),
        Value::Long(crate::combat::filetime(now)),
        Value::Int(i64::from(start)),
        Value::Int(i64::from(start)),
        environment,
        Value::Long(crate::combat::filetime(started)),
        Value::Int(ANALYSIS_MS),
        Value::Int(0),
        Value::None,
        Value::None,
    ])
}

/// CombineScans(probeIDs, groupID): an analysis, its results due when it ends.
async fn combine(ctx: &Ctx<'_>, me: i64, ids: Option<&Value>, group: i64) -> Result<(), String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Err("You are not in space.".into()) };
    let ids: Vec<i64> = match ids {
        Some(Value::List(v)) | Some(Value::Tuple(v)) => v.iter().filter_map(|v| v.as_i64()).collect(),
        _ => Vec::new(),
    };
    let now = crate::combat::now_ms();
    let probes: Vec<Probe> = {
        let w = ctx.world.lock();
        ids.iter().filter_map(|id| w.probes.get(id)).filter(|p| p.owner == me && p.system == system && p.until_ms > now).cloned().collect()
    };
    if probes.is_empty() || probes.len() != ids.len() {
        return Err("Those probes are not yours, or no longer out.".into());
    }
    let need = probes[0].group_count.max(1) as usize;
    if probes.len() != need || probes.iter().any(|p| p.type_id != probes[0].type_id) {
        return Err(format!("This analysis takes {need} probes of the same type."));
    }
    let module = launcher(ctx, ship_id).await.ok_or("You have no probe launcher fitted.")?;
    let category_ship = crate::sde::get().group(group).map(|g| g.category_id as i64) == Some(CATEGORY_SHIP);
    let found = {
        let w = ctx.world.lock();
        match w.parks.get(&system) {
            Some(park) => analyse(park, &probes, group, category_ship, ship_id, now),
            None => Vec::new(),
        }
    };
    {
        let w = ctx.world.lock();
        w.notify(&[ctx.client_id], "OnGodmaShipEffect", scan_event(me, ship_id, module, true, now, now));
    }
    let (world, client) = (ctx.world.clone(), ctx.client_id);
    info!("{me} analyses {} probes for group {group}: {} found", probes.len(), found.len());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ANALYSIS_MS as u64)).await;
        let mut w = world.lock();
        w.scan_results.insert(me, found);
        w.notify(&[client], "OnGodmaShipEffect", scan_event(me, ship_id, module, false, now, crate::combat::now_ms()));
    });
    Ok(())
}

/// What lies within every probe's reach, of the group asked for.
fn analyse(park: &Park, probes: &[Probe], group: i64, ships: bool, me: i64, now: i64) -> Vec<Found> {
    let mut candidates: Vec<(i64, Vec3, f64)> = Vec::new();
    if ships {
        for s in park.ships.values().filter(|s| s.id != me && s.pilot.group_id == group && !s.cloaked) {
            candidates.push((s.id, s.pos, s.fight.as_ref().map_or(100.0, |f| f.sig())));
        }
    } else if group == GROUP_STATION {
        for c in park.celestials.iter().filter(|c| c.group_id == GROUP_STATION) {
            candidates.push((c.id, c.pos, 5_000.0));
        }
    } else if group == GROUP_COSMIC_SIGNATURE {
        for b in park.beacons.values().filter(|b| b.type_id == TYPE_DEADSPACE_SIGNATURE) {
            candidates.push((b.id, b.pos, 300.0));
        }
    }
    let mut rng = crate::combat::Rng::seeded(now as u64 ^ me as u64);
    let (min_dev, max_dev) = probes[0].deviation;
    candidates
        .into_iter()
        .filter(|(_, pos, _)| probes.iter().all(|p| (*pos - p.pos).len() <= p.range * AU))
        .map(|(id, pos, sig)| {
            // Further from the probes, weaker and vaguer.
            let nearest = probes.iter().map(|p| (pos - p.pos).len() / (p.range * AU).max(1.0)).fold(0.0, f64::max);
            let accuracy = min_dev + (max_dev - min_dev) * nearest.clamp(0.0, 1.0) * rng.unit();
            let off = rng.in_sphere(accuracy);
            let signal = (100.0 * (1.0 - nearest) * (sig / 400.0).sqrt().min(1.0)).clamp(1.0, 100.0);
            Found { id, signal, pos: pos + off, accuracy, system: park.solar_system_id }
        })
        .collect()
}

/// Where a scan result lies, for WarpToStuff('scan', resultID).
pub fn result_at(world: &crate::world::World, char_id: i64, system: i64, id: i64) -> Option<(Vec3, f64)> {
    let w = world.lock();
    w.scan_results.get(&char_id)?.iter().find(|r| r.id == id && r.system == system).map(|r| (r.pos, 0.0))
}

/// Before a scanning module starts: a probe launcher needs probes loaded;
/// scanners need nothing more.
pub async fn may_scan(_ctx: &Ctx<'_>, _ship_id: i64) -> Result<(), String> {
    Ok(())
}

/// A scanning module started: probes go out, scanners report at the end of
/// their cycle.
pub async fn scan_started(ctx: &Ctx<'_>, system: i64, ship_id: i64, module: i64, duration_ms: i64) {
    let Some(me) = ctx.session.char_id else { return };
    let Some((type_id, flag)) = ctx.store.read(|db| db.table("entity").get_id(module).map(|e| (e.int("typeID"), e.int("flag")))) else {
        return;
    };
    // A module of a type the data does not know is no scanner.
    let Some(module_type) = type_id.and_then(|t| crate::sde::get().type_(t)) else { return };
    let (group, flag) = (module_type.group_id as i64, flag.unwrap_or(0));
    match group {
        GROUP_PROBE_LAUNCHER => launch_probe(ctx, me, system, ship_id, flag).await,
        GROUP_SHIP_SCANNER | GROUP_CARGO_SCANNER | GROUP_SURVEY_SCANNER => {
            let target = {
                let w = ctx.world.lock();
                w.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()).and_then(|f| f.module(module)).and_then(|m| m.target)
            };
            let (world, store, client) = (ctx.world.clone(), Arc::clone(ctx.store), ctx.client_id);
            let range = survey_range(ctx, me, ship_id, module).await;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(duration_ms.max(1) as u64)).await;
                report(&world, &store, client, system, ship_id, group, target, range);
            });
        }
        _ => {}
    }
}

async fn survey_range(ctx: &Ctx<'_>, me: i64, ship_id: i64, module: i64) -> f64 {
    let active = std::collections::HashSet::new();
    let Ok(l) = ctx.store.read(|db| crate::dogma::Loadout::load(db, ctx.dogma, me, Some(ship_id), &active)) else { return 15_000.0 };
    l.find(module).map_or(15_000.0, |i| l.value(i, ATTR_SURVEY_RANGE).max(1_000.0))
}

/// A scanner's cycle ended: what it saw.
#[allow(clippy::too_many_arguments)]
fn report(world: &crate::world::World, store: &Store, client: i64, system: i64, ship_id: i64, group: i64, target: Option<i64>, range: f64) {
    match group {
        GROUP_SURVEY_SCANNER => {
            let w = world.lock();
            let Some(park) = w.parks.get(&system) else { return };
            let Some(me) = park.ships.get(&ship_id).map(|s| s.pos) else { return };
            let rocks: Vec<Value> = park
                .rocks
                .values()
                .filter(|r| (r.pos - me).len() <= range)
                .map(|r| Value::Tuple(vec![Value::Int(r.id), Value::Int(r.type_id), Value::Int(r.quantity)]))
                .collect();
            w.notify(&[client], "OnSurveyScanComplete", Value::Tuple(vec![Value::List(rocks)]));
        }
        GROUP_SHIP_SCANNER | GROUP_CARGO_SCANNER => {
            let Some(target) = target else { return };
            let (bot_loot, cap) = {
                let w = world.lock();
                let s = w.parks.get(&system).and_then(|p| p.ships.get(&target));
                let cap = s.and_then(|s| s.fight.as_ref()).map_or(0.0, |f| {
                    let now = crate::combat::now_ms();
                    100.0 * f.vitals.capacitor.at(now) / f.vitals.capacitor.max.max(1.0)
                });
                (s.filter(|s| s.bot || s.is_npc()).map(|s| s.bot_loot.clone()), cap)
            };
            let items: Vec<(i64, i64)> = match bot_loot {
                Some(loot) => loot,
                // What is in its hold (each type's total), or fitted (how
                // many of each type), in the order each type first appears.
                None => store.read(|db| {
                    let mut items: Vec<(i64, i64)> = Vec::new();
                    for e in db.table("entity").find(&["locationID"], &[store::Value::Int(target)]) {
                        let flag = e.int("flag").unwrap_or(0);
                        let (wanted, n) = if group == GROUP_CARGO_SCANNER {
                            (flag == FLAG_CARGO, e.int("quantity").unwrap_or(0))
                        } else {
                            ((11..=34).contains(&flag), 1)
                        };
                        if !wanted {
                            continue;
                        }
                        let type_id = e.int("typeID").unwrap_or(0);
                        match items.iter_mut().find(|(t, _)| *t == type_id) {
                            Some((_, total)) => *total += n,
                            None => items.push((type_id, n)),
                        }
                    }
                    items
                }),
            };
            let list = Value::List(items.iter().map(|(t, n)| Value::Tuple(vec![Value::Int(*t), Value::Int(*n)])).collect());
            let w = world.lock();
            if group == GROUP_CARGO_SCANNER {
                w.notify(&[client], "OnCargoScanComplete", Value::Tuple(vec![Value::Int(target), list]));
            } else {
                w.notify(&[client], "OnShipScanComplete", Value::Tuple(vec![Value::Int(target), Value::Double(cap), list]));
            }
        }
        _ => {}
    }
}

/// A probe out of the launcher (the charge was used by the cycle).
async fn launch_probe(ctx: &Ctx<'_>, me: i64, system: i64, ship_id: i64, flag: i64) {
    let sde = crate::sde::get();
    let probes: Vec<i32> = [GROUP_SCANNER_PROBE, GROUP_SURVEY_PROBE].iter().flat_map(|g| sde.types_in_group(*g)).copied().collect();
    let loaded = ctx.store.read(|db| {
        db.table("entity")
            .find(&["locationID", "flag"], &[store::Value::Int(ship_id), store::Value::Int(flag)])
            .find_map(|e| e.int("typeID").filter(|t| probes.contains(&(*t as i32))))
    });
    let Some(type_id) = loaded else {
        return;
    };
    let group = sde.type_(type_id).map_or(0, |t| t.group_id as i64);
    let attrs: HashMap<i32, f64> = sde.type_attributes(type_id).iter().copied().collect();
    let a = |id: i32| attrs.get(&id).copied().unwrap_or(0.0);
    let now = crate::combat::now_ms();
    let lifetime = a(ATTR_LIFETIME).max(60_000.0) as i64;
    let Some(pos) = ctx.world.lock().parks.get(&system).and_then(|p| p.ships.get(&ship_id)).map(|s| s.pos) else { return };
    let id = crate::npc::next_id();
    let probe = Probe {
        owner: me,
        type_id,
        system,
        pos,
        until_ms: now + lifetime,
        range: a(ATTR_SCAN_RANGE).max(0.1),
        group_count: a(ATTR_PROBES_IN_GROUP) as i64,
        deviation: (a(ATTR_MIN_DEVIATION), a(ATTR_MAX_DEVIATION).max(a(ATTR_MIN_DEVIATION))),
    };
    {
        let mut w = ctx.world.lock();
        let beacon = Beacon::new(id, type_id, Some(me), "Probe", pos + Vec3::new(0.0, 2_000.0, 0.0), 10.0, Kind::Probe { char_id: me, until_ms: probe.until_ms });
        if let Some(park) = w.parks.get_mut(&system) {
            let entries = beacons::add(park, beacon);
            crate::svc::space::broadcast(&w, system, None, entries);
        }
        w.probes.retain(|_, p| p.until_ms > now);
        w.probes.insert(id, probe.clone());
    }
    info!("{me} launched probe {id} (type {type_id}) in {system}, {} AU, for {} s", probe.range, lifetime / 1_000);
    if group == GROUP_SURVEY_PROBE {
        let (world, store, client) = (ctx.world.clone(), Arc::clone(ctx.store), ctx.client_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(lifetime as u64)).await;
            if destroyed().lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                return;
            }
            if let Err(e) = survey_moons(&world, &store, client, me, system, pos, probe.range * AU) {
                tracing::error!("moon survey: {}", crate::logging::chain(&e));
            }
        });
    }
}

/// A survey probe's flight is over: every moon within its range, and what
/// each holds.
fn survey_moons(world: &crate::world::World, store: &Store, client: i64, me: i64, system: i64, at: Vec3, range: f64) -> Result<(), StoreError> {
    // The system's security, and each of its moons with where it is.
    let (security, moons) = store.read(|db| {
        let security = db.table("mapSolarSystems").get_id(system).map_or(0.0, |s| s.float("security").unwrap_or(0.0));
        let moons: Vec<(i64, Vec3)> = db
            .table("mapDenormalize")
            .find(&["solarSystemID"], &[store::Value::Int(system)])
            .filter(|m| m.int("groupID") == Some(8))
            .map(|m| (m.int("itemID").unwrap_or(0), Vec3::new(m.float("x").unwrap_or(0.0), m.float("y").unwrap_or(0.0), m.float("z").unwrap_or(0.0))))
            .collect();
        (security, moons)
    });
    let materials = crate::svc::moonmining::materials();
    let now = crate::bulkdata::filetime_now();
    let mut surveyed = Vec::new();
    for (moon, pos) in moons {
        if (pos - at).len() > range {
            continue;
        }
        let held = crate::svc::moonmining::moon_materials(moon, security, materials);
        let products: Vec<Value> = held
            .iter()
            .map(|(t, q)| key_val(vec![("celestialID", Value::Int(moon)), ("typeID", Value::Int(*t)), ("quantity", Value::Int(*q * 100))]))
            .collect();
        surveyed.push(moon);
        if products.is_empty() {
            continue;
        }
        let w = world.lock();
        w.notify(&[client], "OnMoonScanComplete", Value::Tuple(vec![Value::List(products)]));
    }
    if !surveyed.is_empty() {
        store.write(|tx| {
            for moon in &surveyed {
                tx.upsert(
                    "moonScans",
                    &["charID", "moonID"],
                    &[("charID", store::Value::Int(me)), ("moonID", store::Value::Int(*moon)), ("scannedAt", store::Value::Int(now))],
                    &[("scannedAt", store::Value::Int(now))],
                )?;
            }
            Ok::<_, StoreError>(())
        })?;
    }
    info!("{me}'s survey probe in {system} surveyed {} moon(s)", surveyed.len());
    Ok(())
}
