//! Drones: launched from a ship's drone bay into space (ship.Drop), told what
//! to do through the solar system's `entity` object (menusvc.py: CmdEngage,
//! CmdMine, CmdReturnHome, CmdReturnBay...), and scooped back (ScoopDrone or
//! by flying home).
//!
//! In space a drone is its own item in the solar system and a ship in the
//! park, with a [`Drone`] instead of a pilot. It fights with the guns its type
//! has, as rats do, and mines into its controller's cargo hold. Its pilot
//! hears what it is doing as OnDroneStateChange, with the activity states of
//! drone.py (idle, fighting, mining, returning...).

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{debug, error, info, warn};

use crate::ballpark::{Command, Park, Pilot, Ship, ShipStats, Vec3};
use crate::combat::{self, attr, Action, Active, Lock, REPEAT_FOREVER};
use crate::conn::Server;
use crate::heartbeat::{Job, Out};
use crate::items::{self, ItemTxn, NewItem};
use crate::npc;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError, Tx};
use crate::svc::space;
use crate::world::World;

mod persistence;
pub use persistence::{checkpoint, ensure, install, load, reconcile};

const FLAG_DRONE_BAY: i64 = 87;
const CATEGORY_DRONE: i64 = 18;
const OWNER_SYSTEM: i64 = 1;
const GROUP_FIGHTER: i64 = 549;
const GROUP_COMBAT_DRONE: i64 = 100;
const EFFECT_MINING: i64 = 17;
/// A drone this close to its ship is back in the bay.
const DOCK_RANGE: f64 = 1_500.0;
/// How far from its ship it waits.
const IDLE_ORBIT: f64 = 1_000.0;
/// menusvc.py's drone scoop distance, separate from cargo-container transfer.
const SCOOP_RANGE: f64 = 1_500.0;
/// How far beyond its ship's hull a drone comes out.
const LAUNCH_CLEARANCE: f64 = 150.0;
const THINK_MS: i64 = 1_000;
const ATTR_AGGRESSIVE: i32 = 1275;
const ATTR_FIGHTER_FOLLOW: i32 = 1283;
const ATTR_FOCUS_FIRE: i32 = 1297;
const SETTING_ATTRIBUTES: [i32; 3] = [ATTR_AGGRESSIVE, ATTR_FIGHTER_FOLLOW, ATTR_FOCUS_FIRE];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Settings {
    aggressive: bool,
    fighter_follow: bool,
    focus_fire: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { aggressive: true, fighter_follow: true, focus_fire: false }
    }
}

impl Settings {
    fn set(&mut self, attribute: i32, value: bool) {
        match attribute {
            ATTR_AGGRESSIVE => self.aggressive = value,
            ATTR_FIGHTER_FOLLOW => self.fighter_follow = value,
            ATTR_FOCUS_FIRE => self.focus_fire = value,
            _ => {}
        }
    }

    fn load(db: &Db, owner: i64) -> Self {
        let mut settings = Self::default();
        let attributes = db.table("entity_attributes");
        let own: std::collections::HashMap<i32, f64> = SETTING_ATTRIBUTES
            .iter()
            .filter_map(|a| {
                let r = attributes.get(&[store::Value::Int(owner), store::Value::Int((*a).into())])?;
                Some((*a, r.float("valueInt").or_else(|| r.float("valueFloat"))?))
            })
            .collect();
        // The character's own value, else the attribute's default, else on;
        // an attribute the static data does not know is left as it is.
        let types = crate::sde::table("dgmAttributeTypes");
        for attribute in SETTING_ATTRIBUTES {
            let Some(a) = types.get("attributeID", attribute as i64) else { continue };
            let value = own.get(&attribute).copied().or_else(|| a.float("defaultValue")).unwrap_or(1.0);
            settings.set(attribute, value != 0.0);
        }
        settings
    }
}

fn setting_changes(value: Option<&Value>) -> Option<Vec<(i32, bool)>> {
    let Value::Dict(values) = value? else { return None };
    values.iter().map(|(key, value)| {
        let key = i32::try_from(key.as_i64()?).ok()?;
        if !SETTING_ATTRIBUTES.contains(&key) { return None; }
        match value.as_i64()? {
            0 => Some((key, false)),
            1 => Some((key, true)),
            _ => None,
        }
    }).collect()
}

/// The client sends these at login and whenever its drone settings change.
/// Keep them on the character so subsequently launched drones inherit them.
pub async fn change_settings(ctx: &Ctx<'_>, value: Option<&Value>) -> Outcome {
    let Some(owner) = ctx.session.char_id else { return Outcome::custom("Select a character first.") };
    let Some(changes) = setting_changes(value) else { return Outcome::custom("Invalid drone settings.") };
    let result = ctx.store.write(|tx| {
        for (attribute, enabled) in &changes {
            let value = store::Value::Int(i64::from(*enabled));
            tx.upsert(
                "entity_attributes",
                &["itemID", "attributeID"],
                &[
                    ("itemID", store::Value::Int(owner)),
                    ("attributeID", store::Value::Int((*attribute).into())),
                    ("valueInt", value.clone()),
                    ("valueFloat", store::Value::Null),
                ],
                &[("valueInt", value), ("valueFloat", store::Value::Null)],
            )?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(error) = result {
        error!("saving drone settings: {}", crate::logging::chain(&error));
        return Outcome::custom("Your drone settings could not be saved.");
    }
    for park in ctx.world.lock().parks.values_mut() {
        for ship in park.ships.values_mut() {
            if let Some(drone) = ship.drone.as_mut().filter(|drone| drone.owner == owner) {
                for (attribute, enabled) in &changes {
                    drone.settings.set(*attribute, *enabled);
                }
                drone.next_think_ms = 0;
            }
        }
    }
    Outcome::Value(Value::None)
}

/// drone.py's activity states (eveConstants entity*).
pub mod activity {
    pub const IDLE: i64 = 0;
    pub const COMBAT: i64 = 1;
    pub const MINING: i64 = 2;
    pub const DEPARTING: i64 = 4;
    pub const DEPARTING_2: i64 = 5;
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Order {
    Idle,
    Engage(i64),
    Mine { target: i64, repeat: bool },
    /// Back to the ship, to wait there.
    ReturnHome,
    /// Back into the drone bay.
    ReturnBay,
}

#[derive(Debug, Clone)]
pub struct Drone {
    persistence: Option<persistence::Identity>,
    restored_motion: Option<crate::ballpark::Motion>,
    motion_resync: bool,
    pub owner: i64,
    pub client_id: i64,
    /// The ship it works for, while that ship is here.
    pub controller: Option<i64>,
    controller_owner: i64,
    controller_client: i64,
    assignment: Option<Assignment>,
    pending_bay: bool,
    pub order: Order,
    pub activity: i64,
    weapons: Vec<Active>,
    miner: Option<Active>,
    /// Whether the miner has run since the last Mine order.
    mined: bool,
    orbit: f64,
    next_think_ms: i64,
    settings: Settings,
    control_range: f64,
}

impl Drone {
    pub(crate) fn generation(&self) -> Option<i64> {self.persistence.map(|p|p.generation)}
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum Assignment { Assist { pilot: i64, ship: i64 }, Guard { pilot: i64, ship: i64 } }
impl Assignment {
    fn ship(self) -> i64 { match self { Self::Assist {ship,..} | Self::Guard {ship,..} => ship } }
    fn pilot(self) -> i64 { match self { Self::Assist {pilot,..} | Self::Guard {pilot,..} => pilot } }
    fn name(self) -> &'static str { match self { Self::Assist {..} => "assist", Self::Guard {..} => "guard" } }
}

/// Rows for SetState's droneState rowset:
/// droneID, ownerID, controllerID, activityState, typeID, controllerOwnerID, targetID.
pub fn state_lines(park: &Park) -> Vec<Value> {
    park.ships
        .values()
        .filter_map(|s| s.drone.as_ref().filter(|d| d.owner != OWNER_SYSTEM).map(|d| (s, d)))
        .map(|(s, d)| Value::List(state_line(s.id, d, s.pilot.type_id)))
        .collect()
}

fn state_line(id: i64, d: &Drone, type_id: i64) -> Vec<Value> {
    vec![
        Value::Int(id),
        Value::Int(d.owner),
        Value::int_or_none(d.controller),
        Value::Int(d.activity),
        Value::Int(type_id),
        Value::Int(d.controller_owner),
        Value::int_or_none(match d.order { Order::Engage(target) | Order::Mine {target,..} => Some(target), _ => None }),
    ]
}

fn state_note(out: &mut Out, id: i64, d: &Drone, type_id: i64, previous_client: Option<i64>) {
    let mut clients = vec![d.client_id,d.controller_client];
    clients.extend(previous_client);
    clients.sort_unstable(); clients.dedup();
    for client in clients.into_iter().filter(|c| *c > 0) {
        out.note(client,"OnDroneStateChange",Value::Tuple(state_line(id,d,type_id)));
    }
}

fn clear_assignment(id: i64, drone: &mut Drone, out: &mut Out) {
    if drone.assignment.take().is_some() {
        out.note(drone.client_id,"OnDroneActivityChange",Value::Tuple(vec![Value::Int(id),Value::None,Value::None]));
    }
}

// ---- launching -----------------------------------------------------------------------------

/// ship.Drop(itemIDs): one drone into space per ID listed. Returns the
/// launched drones' IDs.
pub async fn launch(ctx: &Ctx<'_>, ids: Vec<i64>) -> Outcome {
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Outcome::custom("Drones can only be launched in space.");
    };
    let (ship_pos, ship_radius, out_already) = {
        let world = ctx.world.lock();
        let Some(park) = world.parks.get(&system) else { return Outcome::custom("You are not in space.") };
        let Some(ship) = park.ships.get(&ship_id) else { return Outcome::custom("You are not in space.") };
        if !ship.is_pilot() || ship.pilot.char_id!=char_id || ship.pilot.group_id==29 {
            return Outcome::custom("You must be piloting a ship to launch drones.");
        }
        let out = park.ships.values().filter(|s| s.drone.as_ref().is_some_and(|d| d.owner == char_id)).count();
        (ship.pos, ship.stats.radius, out)
    };
    let (limit, control_range) = match drone_limits(ctx, char_id, ship_id).await {
        Ok(l) => l,
        Err(e) => {
            error!("launching drones: {}", crate::logging::chain(&e));
            return Outcome::custom("Your drones could not be launched.");
        }
    };
    if limit == 0 {
        return Outcome::custom("You need the Drones skill to launch drones.");
    }
    let room = limit.saturating_sub(out_already);
    if room == 0 {
        return Outcome::custom(format!("You can control at most {limit} drones."));
    }
    let wanted: Vec<i64> = ids.into_iter().take(room).collect();

    let settings = ctx.store.read(|db| Settings::load(db, char_id));

    let mut rng = combat::Rng::seeded(combat::now_ms() as u64 ^ ship_id as u64);
    // Err(None) refuses the launch, undoing what was staged.
    let result = ctx.store.write(|tx| -> Result<(Vec<(i64, i64, Vec3, persistence::Identity)>, Vec<Value>), Option<StoreError>> {
        let mut txn = ItemTxn::new(tx);
        if !txn.item(ship_id)?.is_some_and(|s|s.owner_id==char_id && s.location_id==system) {return Err(None)}
        let mut launched = Vec::new();
        for id in wanted {
            let Some(item) = txn.item(id)? else { continue };
            if item.owner_id != char_id || item.location_id != ship_id || item.flag != FLAG_DRONE_BAY || item.category_id != CATEGORY_DRONE {
                continue;
            }
            let drone = if item.quantity > 1 {
                txn.set_quantity(id, item.quantity - 1)?;
                txn.create(NewItem { type_id: item.type_id, owner_id: char_id, location_id: system, flag: 0, quantity: 1, singleton: true })?
            } else {
                txn.move_to(id, system, 0)?;
                txn.set_singleton(id, true)?;
                id
            };
            // Clear of the hull: destiny would shove a drone launched inside it.
            let pos = ship_pos + rng.in_sphere(1.0).unit() * (ship_radius + LAUNCH_CLEARANCE);
            txn.set_position(drone, pos.tuple())?;
            let identity=persistence::begin(txn.tx(),drone,char_id,system,Some(ship_id),pos,settings,control_range)?;
            launched.push((drone, item.type_id, pos, identity));
        }
        if launched.is_empty() {
            return Err(None);
        }
        if !ctx.world.lock().parks.get(&system).and_then(|p|p.ships.get(&ship_id))
            .is_some_and(|s|s.is_pilot() && s.pilot.char_id==char_id && s.pilot.group_id!=29) {return Err(None)}
        Ok((launched, txn.events()))
    });
    let (launched, events) = match result {
        Ok(v) => v,
        Err(None) => return Outcome::custom("Those drones are not in your drone bay."),
        Err(Some(e)) => {
            error!("launching drones: {}", crate::logging::chain(&e));
            return Outcome::custom("Your drones could not be launched.");
        }
    };
    items::notify(ctx.world, ctx.client_id, events);

    let types: Vec<i64> = launched.iter().map(|(_, t, _, _)| *t).collect();
    let types = match ctx.store.read(|db| npc::load_types(db, &types)) {
        Ok(t) => t,
        Err(e) => {
            error!("reading drone types: {}", crate::logging::chain(&e));
            HashMap::new()
        }
    };
    // The pilot's drone skills, through dogma.
    let drone_ids: Vec<i64> = launched.iter().map(|(id, _, _, _)| *id).collect();
    let skilled: HashMap<i64, HashMap<i32, f64>> =
        match ctx.store.read(|db| crate::dogma::Loadout::load_with_drones(db, ctx.dogma, char_id, Some(ship_id), &Default::default(), &drone_ids)) {
            Ok(l) => drone_ids.iter().filter_map(|id| Some((*id, l.attributes(l.find(*id)?)))).collect(),
            Err(e) => {
                error!("applying drone skills: {}", crate::logging::chain(&e));
                HashMap::new()
            }
        };
    let stored=ctx.store.read(|db|persistence::vitals(db,&drone_ids));
    let now = combat::now_ms();
    let mut prepared = Vec::new();
    let mut ids = Vec::new();
    for (id, type_id, pos, identity) in launched {
        let Some(t) = types.get(&type_id) else { continue };
        let t = &t.with_attributes(skilled.get(&id));
        let pilot = Pilot {
            type_id,
            group_id: t.group_id,
            name: t.name.clone(),
            char_id,
            corp_id: ctx.session.corp_id.unwrap_or(0),
            alliance_id: ctx.session.alliance_id,
            security: 0.0,
            bounty: 0.0,
        };
        let mut drone = make(t, id, pilot, ctx.client_id, ship_id, pos, now, &mut rng);
        persistence::apply_vitals(&mut drone,stored.get(&id),now);
        if let Some(state) = drone.drone.as_mut() {
            state.persistence=Some(identity);
            state.settings = settings;
            state.control_range = control_range;
        }
        prepared.push(drone);
        ids.push(id);
    }
    if let Err(e)=persistence::install(ctx,system,prepared).await {
        error!("making launched drones visible: {}",crate::logging::chain(&e));
        return Outcome::custom("Your launched drones could not be restored to space.");
    }
    ids.retain(|id|ctx.world.lock().parks.get(&system).is_some_and(|p|p.ships.contains_key(id)));
    info!("launched {} drone(s) from {ship_id}", ids.len());
    Outcome::Value(Value::List(ids.into_iter().map(Value::Int).collect()))
}

/// How many drones the pilot can run: the character's maxActiveDrones, which
/// the Drones skill raises.
async fn drone_limits(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Result<(usize, f64), StoreError> {
    let loadout = ctx.store.read(|db| crate::dogma::Loadout::load(db, ctx.dogma, char_id, Some(ship_id), &Default::default()))?;
    Ok(loadout.character().map_or((0, 20_000.0), |character| (
        loadout.value(character, attr::MAX_ACTIVE_DRONES).max(0.0) as usize,
        loadout.value(character, 458).max(0.0),
    )))
}

#[allow(clippy::too_many_arguments)]
fn make(t: &npc::NpcType, id: i64, pilot: Pilot, client_id: i64, controller: i64, pos: Vec3, now: i64, rng: &mut combat::Rng) -> Ship {
    let a = |x: i32| t.get(x);
    let top = a(attr::MAX_VELOCITY).max(a(attr::ENTITY_CRUISE_SPEED)).max(100.0);
    let agility = if a(attr::AGILITY) > 0.0 { a(attr::AGILITY) } else { 1.0 };
    let mass = if t.mass > 0.0 { t.mass } else { 3_000.0 };
    let stats = ShipStats::new(t.radius.max(5.0), mass, agility, top, 1.0);
    let mut fighter = npc::entity_fighter(t, 60_000.0, 1, top, now);
    if fighter.vitals.hull_max <= 1.0 {
        fighter.vitals.hull_max = 50.0;
    }
    let miner = (a(attr::MINING_AMOUNT) > 0.0).then(|| {
        let action = Action::Mining { amount: a(attr::MINING_AMOUNT), range: a(attr::MAX_RANGE).max(1_000.0) };
        let mut m = npc::entity_module(t, id, EFFECT_MINING, "effects.Mining", action, a(attr::DURATION));
        m.offensive = false;
        m
    });
    let orbit = if a(attr::ORBIT_RANGE) > 0.0 { a(attr::ORBIT_RANGE) } else { a(attr::ENTITY_FLY_RANGE).max(500.0) };
    let drone = Drone {
        persistence: None,
        restored_motion: None,
        motion_resync: false,
        owner: pilot.char_id,
        client_id,
        controller: Some(controller),
        controller_owner: pilot.char_id,
        controller_client: client_id,
        assignment: None,
        pending_bay: false,
        order: Order::Idle,
        activity: activity::IDLE,
        weapons: npc::weapons(t, &HashMap::new(), id, rng),
        miner,
        mined: false,
        orbit,
        next_think_ms: now,
        settings: Settings::default(),
        control_range: 20_000.0,
    };
    let mut ship = Ship::new(id, client_id, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.drone = Some(drone);
    ship.command = Command::Orbit { target: controller, range: IDLE_ORBIT };
    ship
}

// ---- orders ----------------------------------------------------------------------------------

/// The solar system's `entity` object: drone orders. Each returns the drones
/// it could not order, as {droneID: (message, args)}.
pub async fn entity(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    reconcile(&mut ctx.world.lock());
    let ids: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect();
    let target = call.arg_i64(1);
    match call.method.as_str() {
        "CmdAbandonDrone" => return Some(Outcome::Value(abandon(ctx, &ids).await)),
        "CmdRelinquishControl" => {
            let value=relinquish(ctx,&ids);persistence::ordered(ctx,&ids).await;return Some(Outcome::Value(value));
        },
        "CmdDelegateControl" | "CmdAssist" | "CmdGuard" => {
            let value=assign(ctx,&ids,target.unwrap_or(0),&call.method).await;
            persistence::ordered(ctx,&ids).await;return Some(Outcome::Value(value));
        },
        _ => {}
    }
    let order = match call.method.as_str() {
        "CmdEngage" => Order::Engage(target?),
        "CmdMine" => Order::Mine { target: target?, repeat: false },
        "CmdMineRepeatedly" => Order::Mine { target: target?, repeat: true },
        "CmdReturnHome" => Order::ReturnHome,
        "CmdReturnBay" => Order::ReturnBay,
        "CmdUnanchor" => {
            return Some(Outcome::custom("Your drones cannot do that."));
        }
        _ => return None,
    };
    let value=command(ctx,&ids,order);
    persistence::ordered(ctx,&ids).await;
    Some(Outcome::Value(value))
}

fn command(ctx: &Ctx<'_>, ids: &[i64], order: Order) -> Value {
    let (Some(char_id), Some(system),Some(ship_id)) = (ctx.session.char_id, ctx.session.solar_system_id,ctx.session.ship_id) else {
        return Value::Dict(Vec::new());
    };
    let mut failed = Vec::new();
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return Value::Dict(Vec::new()) };
    if !park.ships.get(&ship_id).is_some_and(|s| s.pilot.char_id == char_id && s.is_pilot() && s.pilot.group_id != 29) {
        return failures(ids.iter().map(|id| (*id,"You are not piloting that ship.")).collect());
    }
    let mut out = Out::default();
    let target_ok = match &order {
        Order::Engage(t) => park.ships.get(t).is_some_and(|s| s.fight.is_some() && s.pilot.char_id != char_id),
        Order::Mine { target, .. } => park.rocks.contains_key(target),
        _ => true,
    };
    for id in ids {
        let Some(ship) = park.ships.get_mut(id).filter(|s| s.drone.is_some()) else {
            failed.push((*id, "That drone is not in space."));
            continue;
        };
        let drone = ship.drone.as_mut().unwrap();
        let recall = matches!(order,Order::ReturnHome|Order::ReturnBay) && drone.owner == char_id;
        let controlled = drone.controller == Some(ship_id) && drone.controller_owner == char_id;
        if drone.pending_bay || (!recall && !controlled) || (order == Order::ReturnBay && drone.owner != char_id) {
            failed.push((*id,"You do not control that drone.")); continue;
        }
        if !target_ok {
            failed.push((*id, "Your drones cannot do that to that target."));
            continue;
        }
        if matches!(order, Order::Mine { .. }) && drone.miner.is_none() {
            failed.push((*id, "That drone cannot mine."));
            continue;
        }
        drone.order = order.clone();
        drone.mined = false;
        drone.next_think_ms = 0;
        let previous_client = drone.controller_client;
        if recall {
            drone.controller = Some(ship_id);
            drone.controller_owner = char_id;
            drone.controller_client = ctx.client_id;
        }
        clear_assignment(*id,drone,&mut out);
        state_note(&mut out,*id,drone,ship.pilot.type_id,Some(previous_client));
    }
    debug!("drones {ids:?}: {order:?}");
    out.deliver(&world);
    failures(failed)
}

fn failures(failed: Vec<(i64,&str)>) -> Value {
    Value::Dict(
        failed
            .into_iter()
            .map(|(id, why)| {
                let args = Value::Dict(vec![(Value::bytes("error"), Value::text(why))]);
                (Value::Int(id), Value::Tuple(vec![Value::bytes("CustomError"), args]))
            })
            .collect(),
    )
}

async fn assign(ctx: &Ctx<'_>,ids: &[i64],target: i64,method: &str) -> Value {
    let Some(me) = ctx.session.char_id else { return failures(ids.iter().map(|id|(*id,"Select a character first.")).collect()) };
    let system = ctx.session.solar_system_id.unwrap_or(0);
    let target_ship = {
        let w=ctx.world.lock();
        if w.gangs.of(me).is_none() || w.gangs.of(me)!=w.gangs.of(target) || me==target { None }
        else { w.parks.get(&system).and_then(|p|p.ships.values().find(|s|s.pilot.char_id==target && s.is_pilot() && s.pilot.group_id!=29)).map(|s|s.id) }
    };
    let Some(target_ship) = target_ship else { return failures(ids.iter().map(|id|(*id,"Choose a fleet member piloting a ship in this system.")).collect()) };
    let limit = match drone_limits(ctx,target,target_ship).await { Ok((n,_))=>n, Err(e)=>{ error!("delegating drones: {e}");return failures(ids.iter().map(|id|(*id,"Could not read the pilot's drone skills.")).collect()) } };
    let mut w=ctx.world.lock();
    if w.gangs.of(me).is_none() || w.gangs.of(me)!=w.gangs.of(target) { return failures(ids.iter().map(|id|(*id,"That pilot is no longer in your fleet.")).collect()) }
    let Some(park)=w.parks.get_mut(&system) else {return failures(ids.iter().map(|id|(*id,"You are not in space.")).collect())};
    let Some(recipient)=park.ships.get(&target_ship).filter(|s|s.pilot.char_id==target && s.is_pilot() && s.pilot.group_id!=29) else {return failures(ids.iter().map(|id|(*id,"That pilot is not in space.")).collect())};
    let recipient_client=recipient.client_id;
    let mut controlled=park.ships.values().filter(|s|s.drone.as_ref().is_some_and(|d|d.controller==Some(target_ship))).count();
    let mut failed=Vec::new(); let mut out=Out::default();
    for id in ids {
        let Some(ship)=park.ships.get_mut(id).filter(|s|s.drone.is_some()) else {failed.push((*id,"That drone is not in space."));continue};
        let d=ship.drone.as_mut().unwrap();
        if d.owner!=me || d.controller!=ctx.session.ship_id || d.controller_owner!=me || d.pending_bay {failed.push((*id,"You must own and control that drone."));continue}
        if (method=="CmdDelegateControl" && ship.pilot.group_id!=GROUP_FIGHTER) || (method!="CmdDelegateControl" && ship.pilot.group_id!=GROUP_COMBAT_DRONE) {failed.push((*id,"That drone cannot perform this assignment."));continue}
        if method=="CmdDelegateControl" && controlled>=limit {failed.push((*id,"That pilot cannot control more drones."));continue}
        clear_assignment(*id,d,&mut out);
        d.order=Order::Idle; d.next_think_ms=0;
        if method=="CmdDelegateControl" {
            d.controller=Some(target_ship); d.controller_owner=target; d.controller_client=recipient_client; controlled+=1;
        } else {
            let assignment=if method=="CmdAssist" {Assignment::Assist{pilot:target,ship:target_ship}} else {Assignment::Guard{pilot:target,ship:target_ship}};
            d.assignment=Some(assignment);
            out.note(d.client_id,"OnDroneActivityChange",Value::Tuple(vec![Value::Int(*id),Value::Int(target),Value::bytes(assignment.name())]));
        }
        state_note(&mut out,*id,d,ship.pilot.type_id,None);
    }
    out.deliver(&w); failures(failed)
}

fn relinquish(ctx: &Ctx<'_>, ids: &[i64]) -> Value {
    let mut w=ctx.world.lock(); let mut failed=Vec::new(); let mut out=Out::default();
    let Some(park)=ctx.session.solar_system_id.and_then(|s|w.parks.get_mut(&s)) else {return failures(ids.iter().map(|id|(*id,"You are not in space.")).collect())};
    for id in ids {
        let Some(drone)=park.ships.get(id).and_then(|s|s.drone.as_ref()) else {failed.push((*id,"That drone is not in space."));continue};
        if drone.controller_owner!=ctx.session.char_id.unwrap_or(0) || drone.controller!=ctx.session.ship_id || drone.owner==ctx.session.char_id.unwrap_or(0) || drone.pending_bay {failed.push((*id,"You do not control a delegated drone here."));continue}
        let owner=drone.owner;
        let destination=park.ships.values().find(|s|s.pilot.char_id==owner && s.is_pilot() && s.pilot.group_id!=29).map(|s|(s.id,s.client_id));
        let ship=park.ships.get_mut(id).unwrap(); let d=ship.drone.as_mut().unwrap();
        let previous=d.controller_client;
        d.controller=destination.map(|(id,_)|id); d.controller_owner=owner; d.controller_client=destination.map_or(0,|(_,client)|client);
        d.order=Order::Idle; d.next_think_ms=0;
        clear_assignment(*id,d,&mut out);
        state_note(&mut out,*id,d,ship.pilot.type_id,Some(previous));
    }
    out.deliver(&w); failures(failed)
}

async fn abandon(ctx: &Ctx<'_>, ids: &[i64]) -> Value {
    let owner=ctx.session.char_id.unwrap_or(0); let system=ctx.session.solar_system_id.unwrap_or(0);
    let mut failed=Vec::new(); let mut selected=Vec::new();
    {
        let mut w=ctx.world.lock();
        if let Some(park)=w.parks.get_mut(&system) {
            for id in ids {
                if let Some(d)=park.ships.get_mut(id).and_then(|s|s.drone.as_mut()).filter(|d|d.owner==owner && !d.pending_bay) {d.pending_bay=true; selected.push(*id)}
                else {failed.push((*id,"You do not own that drone."))}
            }
        } else {return failures(ids.iter().map(|id|(*id,"You are not in space.")).collect())}
    }
    let result=ctx.store.write(|tx| {
        let mut txn=ItemTxn::new(tx);
        let mut abandoned=Vec::new();
        for id in &selected {
            if txn.item(*id)?.is_some_and(|i|i.owner_id==owner && i.location_id==system) {
                let live={ctx.world.lock().parks.get(&system).and_then(|p|p.ships.get(id)).cloned()};
                let Some(ship)=live.filter(|s|s.drone.as_ref().is_some_and(|d|d.owner==owner && d.pending_bay)) else {continue};
                txn.set_position(*id,ship.pos.tuple())?;
                if let Some(f)=&ship.fight {crate::svc::inflight::write_vitals(txn.tx(),*id,f,false)?;}
                txn.set_owner(*id,OWNER_SYSTEM)?;
                let identity=persistence::begin(txn.tx(),*id,OWNER_SYSTEM,system,None,ship.pos,Settings::default(),0.0)?;
                abandoned.push((*id,identity));
            }
        }
        Ok::<_,StoreError>((abandoned,txn.events()))
    });
    let (abandoned,events)=match result {Ok(v)=>v,Err(e)=>{error!("abandoning drones: {e}");(vec![],vec![])}};
    items::notify(ctx.world,ctx.client_id,events);
    let mut w=ctx.world.lock(); let mut out=Out::default();
    if let Some(park)=w.parks.get_mut(&system) {
        for id in selected {
            if let Some(ship)=park.ships.get_mut(&id).filter(|s|s.drone.is_some()) {
                let d=ship.drone.as_mut().unwrap(); d.pending_bay=false;
                let Some((_,identity))=abandoned.iter().find(|(item,_)|*item==id) else {failed.push((id,"The drone could not be abandoned."));continue};
                d.persistence=Some(*identity);
                d.restored_motion=None;
                let previous=d.controller_client;
                clear_assignment(id,d,&mut out);
                d.owner=OWNER_SYSTEM; d.controller=None; d.controller_owner=OWNER_SYSTEM; d.controller_client=0; d.order=Order::Idle; d.next_think_ms=0;
                ship.pilot.char_id=OWNER_SYSTEM; ship.pilot.corp_id=0; ship.pilot.alliance_id=None;
                state_note(&mut out,id,d,ship.pilot.type_id,Some(previous)); d.client_id=0;
                out.destiny(system,space::entry(park.tick,"OnSlimItemChange",vec![Value::Int(id),space::ship_slim(ship)]));
            }
        }
    }
    out.deliver(&w); failures(failed)
}

// ---- thinking ---------------------------------------------------------------------------------

pub fn think(park: &mut Park, now: i64, out: &mut Out) {
    let mut resync=false;
    for ship in park.ships.values_mut() {
        if let Some(d)=ship.drone.as_mut() {resync |= std::mem::take(&mut d.motion_resync);}
    }
    if resync {out.jobs.push(Job::WarpResync{system:park.solar_system_id});}
    let due: Vec<i64> = park
        .ships
        .values()
        .filter(|s| s.drone.as_ref().is_some_and(|d| d.next_think_ms <= now))
        .map(|s| s.id)
        .collect();
    for id in due {
        think_one(park, id, now, out);
    }
}

fn think_one(park: &mut Park, id: i64, now: i64, out: &mut Out) {
    let Some(mut drone) = park.ships.get(&id).and_then(|s| s.drone.clone()) else { return };
    if drone.pending_bay { return; }
    // A recovered course waits for current control authorization. It cannot
    // attack, mine or move under an obsolete session while its pilot logs in.
    if drone.restored_motion.is_some() && drone.controller.is_none() {return}
    let controller = drone.controller.filter(|c| park.ships.get(c).is_some_and(|s|
        s.is_pilot() && s.pilot.char_id==drone.controller_owner && s.pilot.group_id!=29)).or_else(|| {
        (drone.owner != OWNER_SYSTEM).then(|| park.ships.values().find(|s|s.pilot.char_id==drone.owner && s.is_pilot() && s.pilot.group_id!=29).map(|s|s.id)).flatten()
    });
    let controller_meta=controller.and_then(|id|park.ships.get(&id)).map(|s|(s.pilot.char_id,s.client_id));
    let assignment=controller.and(drone.assignment).filter(|a|park.ships.get(&a.ship()).is_some_and(|s|s.pilot.char_id==a.pilot() && s.is_pilot() && s.pilot.group_id!=29));
    // Defensive targeting below must use the newly validated controller,
    // rather than the hull it controlled before an eject or boarding.
    drone.controller=controller;
    drone.controller_owner=controller_meta.map_or(drone.owner,|(owner,_)|owner);
    let mut order = drone.order.clone();
    // An order with nothing left to do becomes waiting.
    let gone = match &order {
        Order::Engage(t) => !park.ships.contains_key(t),
        Order::Mine { target, .. } => !park.rocks.contains_key(target),
        Order::ReturnHome | Order::ReturnBay => controller.is_none(),
        Order::Idle => false,
    };
    if gone {
        order = Order::Idle;
    }
    if controller.is_none() { order=Order::Idle; }
    if order == Order::Idle && controller.is_some() {
        if let Some(target)=assignment.and_then(|a|assignment_target(park,id,&drone,a)) { order=Order::Engage(target); }
    }
    if order == Order::Idle && controller.is_some() && assignment.is_none() && drone.settings.aggressive && !drone.weapons.is_empty() {
        if let Some(target) = defensive_target(park, id, &drone) {
            order = Order::Engage(target);
        }
    }
    // Only fighters can pursue a target through warp, and only when the pilot
    // enabled attack-and-follow. Ordinary drones return to their controller.
    if let Order::Engage(target) = order {
        if park.ships.get(&target).is_some_and(|ship| ship.warping()) {
            let is_fighter = park.ships.get(&id).is_some_and(|ship| ship.pilot.group_id == 549);
            if !is_fighter || !drone.settings.fighter_follow {
                order = Order::Idle;
            }
        }
    }
    let running = |park: &Park, module: i64| {
        park.ships.get(&id).and_then(|s| s.fight.as_ref()).is_some_and(|f| f.module(module).is_some())
    };
    if let Order::Mine { repeat: false, .. } = order {
        let miner = drone.miner.as_ref().map(|m| m.module_id);
        if drone.mined && miner.is_some_and(|m| !running(park, m)) {
            order = Order::ReturnBay;
        }
    }

    let (activity, mut course, work): (i64, Option<Command>, Option<i64>) = match &order {
        Order::Idle => (activity::IDLE, assignment.map(Assignment::ship).or(controller).map(|c| Command::Orbit { target: c, range: IDLE_ORBIT }), None),
        Order::Engage(t) => (activity::COMBAT, Some(Command::Orbit { target: *t, range: drone.orbit }), Some(*t)),
        Order::Mine { target, .. } => (activity::MINING, Some(Command::Orbit { target: *target, range: drone.orbit.min(500.0) }), Some(*target)),
        Order::ReturnHome => (activity::DEPARTING_2, controller.map(|c| Command::Follow { target: c, range: IDLE_ORBIT }), None),
        Order::ReturnBay => (activity::DEPARTING, controller.map(|c| Command::Follow { target: c, range: 0.0 }), None),
    };
    if let Order::Engage(target) = order {
        if let Some(ship) = park.ships.get(&target).filter(|ship| ship.warping()) {
            if let Command::Warp { target, .. } = ship.command {
                course = Some(Command::Warp { target, range: drone.orbit });
            }
        }
    }

    // Home: into the bay.
    if order == Order::ReturnBay {
        if let Some(c) = controller {
            if park.surface_distance(id, c).is_some_and(|d| d <= DOCK_RANGE) {
                dock(park, id, c, now, out);
                return;
            }
        }
    }
    if order == Order::ReturnHome {
        if let Some(c) = controller {
            if park.surface_distance(id, c).is_some_and(|d| d <= IDLE_ORBIT * 1.5) {
                order = Order::Idle;
            }
        }
    }

    // Stop what it should no longer do.
    let keep: Vec<i64> = match work {
        Some(_) => drone.weapons.iter().map(|w| w.module_id).chain(drone.miner.iter().map(|m| m.module_id)).collect(),
        None => Vec::new(),
    };
    let stale: Vec<i64> = park
        .ships
        .get(&id)
        .and_then(|s| s.fight.as_ref())
        .map(|f| f.modules.iter().filter(|m| !keep.contains(&m.module_id) || m.target != work).map(|m| m.module_id).collect())
        .unwrap_or_default();
    for m in stale {
        crate::fight::stop(park, id, m, now, out, None);
    }

    let tick = park.tick;
    let system = park.solar_system_id;
    let signature = work.and_then(|t| park.ships.get(&t)).and_then(|s| s.fight.as_ref()).map_or(100.0, |f| f.sig());
    let Some(ship) = park.ships.get_mut(&id) else { return };
    let Some(fight) = ship.fight.as_mut() else { return };
    match work {
        Some(t) => {
            fight.locks.retain(|l, _| *l == t);
            if !fight.locks.contains_key(&t) {
                let ms = (combat::lock_seconds(fight.scan_res(), signature) * 1_000.0) as i64;
                fight.locks.insert(t, Lock::Pending(now + ms));
            }
        }
        None => fight.locks.clear(),
    }
    let mut started = Vec::new();
    if let Some(t) = work.filter(|t| fight.is_locked(*t)) {
        let tools: Vec<Active> = match order {
            Order::Mine { repeat, .. } if !drone.mined => {
                drone.miner.iter().cloned().map(|mut m| {
                    m.repeat = if repeat { REPEAT_FOREVER } else { 0 };
                    m
                }).collect()
            }
            Order::Engage(_) => drone.weapons.clone(),
            _ => Vec::new(),
        };
        for mut m in tools {
            if fight.module(m.module_id).is_some() {
                continue;
            }
            m.target = Some(t);
            m.started_ms = now;
            m.next_ms = now;
            started.push(m.clone());
            fight.modules.push(m);
        }
    }
    let Some(d) = ship.drone.as_mut() else { return };
    if !started.is_empty() && matches!(order, Order::Mine { .. }) {
        d.mined = true;
    }
    let activity = if order == Order::Idle { activity::IDLE } else { activity };
    let changed = d.activity != activity || d.order != order || d.controller != controller;
    let previous_client=d.controller_client;
    d.order = order;
    d.activity = activity;
    d.controller = controller;
    d.controller_owner=controller_meta.map_or(d.owner,|(owner,_)|owner);
    d.controller_client=controller_meta.map_or(0,|(_,client)|client);
    if assignment.is_none() { clear_assignment(id,d,out); }
    d.next_think_ms = now + THINK_MS;
    if changed {
        state_note(out,id,d,ship.pilot.type_id,Some(previous_client));
    }
    for m in &started {
        out.destiny(system, crate::fight::module_fx(tick, id, m, true));
    }
    let course = course.unwrap_or(Command::Stop);
    if ship.command != course {
        park.command(id, course);
        park.set_speed_fraction(id, 1.0);
        if let Some(s) = park.ships.get(&id) {
            for e in space::course_entries(s, tick) {
                out.destiny(system, e);
            }
            if s.command == Command::Stop {
                out.destiny(system, space::command_entry(s, tick));
            }
        }
    }
}

fn assignment_target(park:&Park,id:i64,drone:&Drone,assignment:Assignment)->Option<i64> {
    match assignment {
        Assignment::Guard{ship,..}=>{
            let mut guarding=drone.clone(); guarding.controller=Some(ship);
            defensive_target(park,id,&guarding)
        }
        Assignment::Assist{ship,..}=>park.ships.get(&ship)?.fight.as_ref()?.modules.iter()
            .filter(|m|m.offensive).filter_map(|m|m.target)
            .find(|target|park.ships.get(target).is_some_and(|s|s.fight.is_some() && !s.cloaked && s.pilot.char_id!=drone.owner)
                && drone.controller.and_then(|c|park.ships.get(&c)).zip(park.ships.get(target))
                    .is_some_and(|(c,t)|(c.pos-t.pos).len()<=drone.control_range)),
    }
}

/// Aggressive drones retaliate against actual offensive modules aimed at
/// their controller or its drones. They never attack merely locked targets.
fn defensive_target(park: &Park, id: i64, drone: &Drone) -> Option<i64> {
    let controller = drone.controller?;
    let own = park.ships.get(&id)?;
    let threats: Vec<&Ship> = park.ships.values().filter(|ship| {
        !ship.cloaked && !ship.warping() && ship.id != id && ship.pilot.char_id != drone.owner && ship.pilot.char_id != drone.controller_owner
            && ship.fight.as_ref().is_some_and(|fight| fight.modules.iter().any(|module| {
                module.offensive && module.target.is_some_and(|target| {
                    target == controller || park.ships.get(&target).and_then(|ship| ship.drone.as_ref())
                        .is_some_and(|other| other.owner == drone.owner)
                })
            }))
            && park.ships.get(&controller).is_some_and(|controller| (ship.pos - controller.pos).len() <= drone.control_range)
    }).collect();
    if drone.settings.focus_fire {
        for ally in park.ships.values().filter_map(|ship| ship.drone.as_ref()).filter(|ally| ally.owner == drone.owner) {
            if let Order::Engage(target) = ally.order {
                if threats.iter().any(|ship| ship.id == target) {
                    return Some(target);
                }
            }
        }
    }
    threats.into_iter().min_by(|a, b| {
        (a.pos - own.pos).len_sq().total_cmp(&(b.pos - own.pos).len_sq()).then(a.id.cmp(&b.id))
    }).map(|ship| ship.id)
}

/// Into the bay: out of space, and the item back where it came from.
fn dock(park: &mut Park, id: i64, controller: i64, now: i64, out: &mut Out) {
    let Some(drone) = park.ships.get_mut(&id).and_then(|s|s.drone.as_mut()) else { return };
    if drone.pending_bay { return; }
    drone.pending_bay=true; drone.next_think_ms=now+THINK_MS;
    let client_id=drone.client_id;
    out.jobs.push(Job::DroneHome { client_id, drone_id: id, ship_id: controller });
}

/// ship.ScoopDrone(ids): drones close to the ship go straight into the bay.
pub async fn scoop(ctx: &Ctx<'_>, ids: Vec<i64>) -> Outcome {
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Outcome::custom("You are not in space.");
    };
    let mut scooped = Vec::new();
    {
        let mut world = ctx.world.lock();
        let Some(park) = world.parks.get_mut(&system) else { return Outcome::custom("You are not in space.") };
        if !park.ships.get(&ship_id).is_some_and(|s|s.pilot.char_id==char_id && s.is_player()) {return Outcome::custom("You are not piloting that ship.")}
        for id in ids {
            let mine = park.ships.get(&id).and_then(|s| s.drone.as_ref()).is_some_and(|d| !d.pending_bay && (d.owner == char_id || (d.owner==OWNER_SYSTEM && d.controller.is_none())));
            let near = park.surface_distance(id, ship_id).is_some_and(|d| d <= SCOOP_RANGE);
            if mine && near {
                park.ships.get_mut(&id).unwrap().drone.as_mut().unwrap().pending_bay=true;
                scooped.push(id);
            }
        }
    }
    if scooped.is_empty() {
        return Outcome::custom("There is no recoverable drone close enough to scoop.");
    }
    let mut success=false;
    for id in scooped {
        success |= into_bay(ctx.store,ctx.dogma,ctx.world,ctx.client_id,id,ship_id).await;
    }
    if success {Outcome::Value(Value::None)} else {Outcome::custom("The drones could not be moved into your drone bay.")}
}

pub async fn home(server: &Server, store: &Store, client_id: i64, drone_id: i64, ship_id: i64) {
    into_bay(store,&server.dogma,&server.world,client_id,drone_id,ship_id).await;
}

fn release_pending(world:&World, ids:&[i64]) {
    for park in world.lock().parks.values_mut() {
        for id in ids { if let Some(d)=park.ships.get_mut(id).and_then(|s|s.drone.as_mut()) {d.pending_bay=false;} }
    }
}

fn bay_ready(world:&crate::world::WorldState,system:i64,owner:i64,drone_id:i64,ship_id:i64) -> bool {
    let Some(park)=world.parks.get(&system) else {return false};
    let Some(ship)=park.ships.get(&ship_id).filter(|s|s.is_pilot() && s.pilot.char_id==owner) else {return false};
    let Some(drone)=park.ships.get(&drone_id).and_then(|s|s.drone.as_ref()) else {return false};
    drone.pending_bay && (drone.owner==owner || (drone.owner==OWNER_SYSTEM && drone.controller.is_none()))
        && park.surface_distance(drone_id,ship.id).is_some_and(|distance|distance<=SCOOP_RANGE)
}

async fn into_bay(store: &Store,catalog:&crate::dogma::Catalog, world: &World, client_id: i64, drone_id: i64, ship_id: i64) -> bool {
    let live={let w=world.lock();w.parks.iter().find_map(|(system,p)| {
        let ship=p.ships.get(&ship_id)?; let drone=p.ships.get(&drone_id)?.drone.as_ref()?;
        (ship.is_pilot() && drone.pending_bay && (drone.owner==ship.pilot.char_id || (drone.owner==OWNER_SYSTEM && drone.controller.is_none()))
            && p.surface_distance(drone_id,ship_id).is_some_and(|d|d<=SCOOP_RANGE)).then_some((*system,ship.pilot.char_id))
    })};
    let Some((system,owner))=live else {release_pending(world,&[drone_id]);return false};
    // Err(None) refuses the return, undoing what was staged.
    let result = store.write(|tx| -> Result<Vec<Value>, Option<StoreError>> {
        let mut txn = ItemTxn::new(tx);
        let Some(ship)=txn.item(ship_id)? else {return Err(None)};
        let Some(drone)=txn.item(drone_id)? else {return Err(None)};
        if ship.owner_id!=owner || ship.location_id!=system || drone.category_id!=CATEGORY_DRONE || drone.location_id!=system
            || (drone.owner_id!=owner && drone.owner_id!=OWNER_SYSTEM) {return Err(None)}
        let loadout=crate::dogma::Loadout::load(txn.db(),catalog,owner,Some(ship_id),&Default::default())?;
        let capacity=loadout.ship().map_or(0.0,|ship|loadout.value(ship,283));
        // Drones of a type the static data does not know take no room, as
        // joining on it left them out.
        let used:f64=txn.db().table("entity").find(&["locationID","flag"],&[store::Value::Int(ship_id),store::Value::Int(FLAG_DRONE_BAY)])
            .filter_map(|r|{
                let volume=crate::sde::table("invTypes").get("typeID",r.int("typeID")?)?.float("volume")?;
                Some(volume*r.int("quantity")? as f64)
            }).sum();
        if used+drone.volume*drone.quantity as f64>capacity+1e-6 {return Err(None)}
        let live={world.lock().parks.get(&system).and_then(|p|p.ships.get(&drone_id)).cloned()};
        if let Some(f)=live.as_ref().and_then(|s|s.fight.as_ref()) {crate::svc::inflight::write_vitals(txn.tx(),drone_id,f,false)?;}
        txn.set_owner(drone_id,owner)?;
        txn.move_to(drone_id, ship_id, FLAG_DRONE_BAY)?;
        // Heartbeat motion and destruction can have changed the park since
        // the live snapshot above. Recheck the live reservation and
        // destination before committing; refusing undoes the staged changes.
        if !bay_ready(&world.lock(),system,owner,drone_id,ship_id) {return Err(None)}
        Ok(txn.events())
    });
    match result {
        Ok(events) => {
            let mut w=world.lock(); let mut out=Out::default();
            if let Some(park)=w.parks.get_mut(&system) {
                if crate::fight::detach(park,drone_id,combat::now_ms(),&mut out).is_some() {
                    out.destiny(system,space::entry(park.tick,"RemoveBalls",vec![Value::List(vec![Value::Int(drone_id)])]));
                }
            }
            out.deliver(&w); drop(w);
            items::notify(world, client_id, events);
            debug!("drone {drone_id} back in {ship_id}");
            true
        }
        Err(e) => {
            if let Some(e)=e {error!("returning drone {drone_id} to {ship_id}: {}",crate::logging::chain(&e));}
            release_pending(world,&[drone_id]);false
        }
    }
}

async fn destroy_lost(store:&Store,system:i64,owner:i64,drone_id:i64,generation:Option<i64>)
    -> Result<Option<Vec<Value>>,StoreError> {
    store.write(|tx| {
        let mut txn=ItemTxn::new(tx);
        let Some(item)=txn.item(drone_id)? else {return Ok(None)};
        if item.owner_id!=owner || item.location_id!=system || item.flag!=0 || item.category_id!=CATEGORY_DRONE {return Ok(None)}
        let current=txn.db().table("launched_drone_state").get_id(drone_id)
            .filter(|r|r.int("owner_id")==Some(owner) && r.int("system_id")==Some(system)).and_then(|r|r.int("generation"));
        if current!=generation {return Ok(None)}
        txn.destroy(drone_id)?;
        Ok(Some(txn.events()))
    })
}

pub async fn lost(server: &Server, store: &Store, client_id: Option<i64>, drone_id: i64,system:i64,owner:i64,generation:Option<i64>) {
    let result=destroy_lost(store,system,owner,drone_id,generation).await;
    match result {
        Ok(Some(events)) => {
            if let Some(client) = client_id {
                items::notify(&server.world, client, events);
            }
        }
        Ok(None)=>{},
        Err(e) => warn!("destroying drone {drone_id}: {}", crate::logging::chain(&e)),
    }
}
