//! Capital ships' jumps: cynosural fields, jump drives and titans' bridges
//! (gang.py, menusvc.py "Jump to member", "Bridge to member", "Jump through").
//!
//! - A pilot lights a cynosural field with a Cynosural Field Generator: it
//!   burns Liquid Ozone from the hold, holds the ship where it is for its
//!   ten-minute cycle, cannot be lit in high-security space, and puts a field
//!   (a beacon) beside the ship. Everyone in the pilot's gang hears of it
//!   (OnJumpBeaconChange) and so may jump to it.
//! - A ship with a jump drive (canJump) in space jumps to a gang member's
//!   field (beyonce.BeaconJump) if it is within the drive's range in light
//!   years, burning isotopes from its hold for each light year and most of its
//!   capacitor, and comes out beside the field.
//! - A titan with a Jump Portal Generator opens a bridge to a gang member's
//!   field (beyonce.BridgeToMember): Strontium Clathrates from its hold, a
//!   minute's portal (OnBridgeModeChange to the gang); gang members near it
//!   jump through (beyonce.JumpThroughGang), the titan paying drive fuel by mass.

use std::collections::HashSet;

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::Vec3;
use crate::beacons::{self, Beacon, Kind};
use crate::dogma::Loadout;
use crate::items::ItemTxn;
use crate::services::Ctx;
use crate::store::{self, Db, StoreError};
use crate::world::{World, WorldState};

pub(crate) mod portal;

pub const ATTR_CONSUMPTION_TYPE: i32 = 713;
pub const ATTR_CONSUMPTION_QUANTITY: i32 = 714;
const ATTR_CAN_JUMP: i32 = 861;
const ATTR_JUMP_FUEL_TYPE: i32 = 866;
const ATTR_JUMP_RANGE: i32 = 867;
const ATTR_JUMP_FUEL_PER_LY: i32 = 868;
const ATTR_JUMP_CAPACITOR_NEED: i32 = 898;
const ATTR_PORTAL_MASS_FACTOR: i32 = 1001;
const ATTR_CAPACITOR_CAPACITY: i32 = 482;
const FLAG_CARGO: i32 = 5;
const GROUP_JUMP_PORTAL_GENERATOR: i64 = 590;
const METERS_PER_LIGHT_YEAR: f64 = 9.460_730_472_580_8e15;
/// High-security space takes no cynosural field.
const HIGH_SEC: f64 = 0.45;
/// How close to a titan a ship must be to jump through its bridge.
pub const BRIDGE_RANGE: f64 = 5_000.0;

pub type Refused<T> = Result<T, String>;

/// A quote, not a payment. The connection consumes this after the jump-out
/// delay, in the same transaction that moves the ship and pilot.
pub struct PreparedJump {
    pub at: Vec3,
    pub fuel: FuelCost,
    pub capacitor: f64,
    pub clients: Vec<i64>,
}

pub struct FuelCost {
    pub cargo: i64,
    pub type_id: i64,
    pub quantity: i64,
    /// Ship fuel is in flagCargo; jump-array fuel is in its structure stores.
    pub flag: Option<i32>,
}

impl FuelCost {
    /// The fuel's stacks in the hold, by itemID: (itemID, quantity).
    fn stacks(&self, db: &Db) -> Vec<(i64, i64)> {
        let mut stacks: Vec<(i64, i64)> = db
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(self.cargo)])
            .filter(|e| e.int("typeID") == Some(self.type_id) && self.flag.is_none_or(|f| e.int("flag") == Some(f.into())))
            .filter_map(|e| Some((e.int("itemID")?, e.int("quantity").filter(|q| *q > 0)?)))
            .collect();
        stacks.sort_unstable();
        stacks
    }

    pub async fn check(&self, ctx: &Ctx<'_>) -> Refused<()> {
        let have: i64 = ctx.store.read(|db| self.stacks(db)).iter().map(|(_, quantity)| quantity).sum();
        if have<self.quantity { return Err(format!("That jump needs {} units of fuel.",self.quantity)); }
        Ok(())
    }

    /// The caller owns the arrival transaction. Refusal or a later failed
    /// arrival write rolls these item changes back together.
    pub fn debit(&self, txn: &mut ItemTxn<'_, '_>) -> Refused<()> {
        if self.type_id<=0 || self.quantity<0 { return Err("The jump fuel requirement is invalid.".into()); }
        let stacks=self.stacks(txn.db());
        let have:i64=stacks.iter().map(|(_,quantity)|quantity).sum();
        if have<self.quantity { return Err(format!("That jump needs {} units of fuel; only {have} remain.",self.quantity)); }
        let mut left=self.quantity;
        for (id,quantity) in stacks {
            if left==0 { break; }
            let take=left.min(quantity);
            txn.set_quantity(id,quantity-take).map_err(payment_error)?;
            left-=take;
        }
        Ok(())
    }
}

fn payment_error(error: StoreError) -> String {
    tracing::error!("jump fuel: {}",crate::logging::chain(&error));
    "The jump fuel could not be paid.".into()
}

/// The clients of everyone in a pilot's gang (the pilot included).
pub fn gang_clients(w: &WorldState, char_id: i64) -> Vec<i64> {
    match w.gangs.of(char_id).and_then(|g| w.gangs.get(g)) {
        Some(g) => g.members.iter().flat_map(|m| w.clients_of(*m)).collect(),
        None => w.clients_of(char_id),
    }
}

/// A cynosural field lit: the generator just started on `ship_id`. Refuses
/// in high-security space.
pub fn may_light_cyno(security: f64) -> Refused<()> {
    if security >= HIGH_SEC {
        return Err("Cynosural fields cannot be lit in high-security space.".into());
    }
    Ok(())
}

/// Puts a pilot's cynosural field beside their ship and tells their gang.
pub fn cyno_lit(w: &mut WorldState, system: i64, char_id: i64, ship_id: i64, generator_type: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(pos) = park.ships.get(&ship_id).map(|s| s.pos) else { return };
    let id = crate::npc::next_id();
    let (field_type, name) = if generator_type == 28646 {
        (beacons::TYPE_COVERT_CYNOSURAL_FIELD, "Covert Cynosural Field")
    } else { (beacons::TYPE_CYNOSURAL_FIELD, "Cynosural Field") };
    let field = Beacon::new(id, field_type, Some(char_id), name, pos + Vec3::new(0.0, 0.0, 1_500.0), 1.0, Kind::Cyno { char_id, ship_id });
    let entries = beacons::add(park, field);
    crate::svc::space::broadcast(w, system, None, entries);
    let clients = gang_clients(w, char_id);
    w.notify(&clients, "OnJumpBeaconChange", Value::Tuple(vec![Value::Int(char_id), Value::Int(system), Value::Int(id), Value::Bool(true)]));
    info!("{char_id} lit a cynosural field ({id}) in {system}");
}

/// A pilot's cynosural field went out (its generator stopped, or the ship
/// left or died): gone from space, and their gang told.
pub fn cyno_out(w: &mut WorldState, system: i64, char_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let ids: Vec<i64> = park.beacons.values().filter(|b| matches!(b.kind, Kind::Cyno { char_id: c, .. } if c == char_id)).map(|b| b.id).collect();
    if ids.is_empty() {
        return;
    }
    let entries = beacons::remove(park, &ids);
    crate::svc::space::broadcast(w, system, None, entries);
    let clients = gang_clients(w, char_id);
    for id in ids {
        w.notify(&clients, "OnJumpBeaconChange", Value::Tuple(vec![Value::Int(char_id), Value::Int(system), Value::Int(id), Value::Bool(false)]));
    }
    info!("{char_id}'s cynosural field in {system} went out");
}

/// Cynosural fields whose ships are gone (destroyed, docked, jumped) go out.
pub fn orphaned_cynos(w: &mut WorldState) {
    let gone: Vec<(i64, i64)> = w
        .parks
        .iter()
        .flat_map(|(system, park)| {
            park.beacons.values().filter_map(move |b| match b.kind {
                Kind::Cyno { char_id, ship_id } => {
                    let burning = park.ships.get(&ship_id).and_then(|s| s.fight.as_ref()).is_some_and(|f| f.modules.iter().any(|m| m.action == crate::combat::Action::Cyno));
                    (!burning).then_some((*system, char_id))
                }
                _ => None,
            })
        })
        .collect();
    for (system, char_id) in gone {
        cyno_out(w, system, char_id);
    }
}

/// Trinity's map consumer unpacks (moduleCnt, structureCnt), including fixed
/// beacons whose system has no connected clients and therefore no ballpark.
pub async fn beacon_count(ctx: &Ctx<'_>) -> Value {
    let fixed = ctx.store.read(|db| {
        let mut counts = std::collections::BTreeMap::<i64, i64>::new();
        for type_id in crate::sde::get().types_in_group(838) {
            for e in db.table("entity").find(&["typeID"], &[store::Value::Int((*type_id).into())]) {
                let Some(system) = e.int("locationID") else { continue };
                if online(db, e.value("itemID")) && e.int("quantity").is_some_and(|q| q > 0)
                    && db.table("mapSolarSystems").get_id(system).is_some() {
                    *counts.entry(system).or_default() += 1;
                }
            }
        }
        counts
    });
    beacon_counts(ctx.world, fixed)
}

/// Whether a fixed beacon's structure is online (state 4 or 5).
fn online(db: &Db, item: &store::Value) -> bool {
    db.table("posStructures").get(std::slice::from_ref(item)).is_some_and(|p| matches!(p.int("state"), Some(4 | 5)))
}

fn beacon_counts(world: &World, fixed: impl IntoIterator<Item=(i64,i64)>) -> Value {
    let mut counts = std::collections::BTreeMap::<i64,(i64,i64)>::new();
    for (system,n) in fixed { if n>0 { counts.entry(system).or_default().1 += n; } }
    let w = world.lock();
    for (system,park) in &w.parks {
        let n=park.beacons.values().filter(|b| b.type_id==beacons::TYPE_CYNOSURAL_FIELD && matches!(b.kind,Kind::Cyno{..})).count() as i64;
        if n>0 { counts.entry(*system).or_default().0=n; }
    }
    Value::Dict(counts.into_iter().map(|(id,(modules,structures))| (Value::Int(id),Value::Tuple(vec![Value::Int(modules),Value::Int(structures)]))).collect())
}

/// Light years between two systems.
pub async fn light_years(ctx: &Ctx<'_>, a: i64, b: i64) -> Option<f64> {
    ctx.store.read(|db| Some((system_at(db, a)? - system_at(db, b)?).len() / METERS_PER_LIGHT_YEAR))
}

/// Where a solar system is on the map.
fn system_at(db: &Db, id: i64) -> Option<Vec3> {
    let r = db.table("mapSolarSystems").get_id(id)?;
    Some(Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0)))
}

/// Where a field is: its system's park must hold it, lit by `char_id`.
pub fn field_at(w: &WorldState, system: i64, beacon: i64, char_id: i64) -> Option<Vec3> {
    let b = w.parks.get(&system)?.beacons.get(&beacon)?;
    matches!(b.kind, Kind::Cyno { char_id: c, .. } if c == char_id).then_some(b.pos)
}

/// Fixed beacons need not have a loaded ballpark. Resolve their current owner,
/// location and operating state from the same records used by capital navigation.
async fn alliance_field_at(ctx: &Ctx<'_>, system: i64, beacon: i64) -> Refused<Vec3> {
    let alliance = ctx.session.alliance_id.filter(|id| *id > 0)
        .ok_or("You must belong to the beacon's alliance.")?;
    ctx.store.read(|db| {
        let e = db.table("entity").get_id(beacon).filter(|e| {
            e.int("locationID") == Some(system)
                && e.int("typeID").is_some_and(|t| crate::sde::get().types_in_group(838).contains(&(t as i32)))
                && online(db, e.value("itemID"))
                && e.int("ownerID").and_then(|o| db.table("corporation").get_id(o)).is_some_and(|c| c.int("allianceID") == Some(alliance))
        })?;
        Some(Vec3::new(e.float("x").unwrap_or(0.0), e.float("y").unwrap_or(0.0), e.float("z").unwrap_or(0.0)))
    })
    .ok_or_else(|| "That alliance cynosural beacon is not available.".into())
}

fn may_jump(ctx: &Ctx<'_>, ship_id: i64, system: i64) -> Refused<()> {
    if ctx.session.station_id.is_some() { return Err("You must be in space to jump.".into()); }
    let w = ctx.world.lock();
    let ship = w.parks.get(&system).and_then(|p| p.ships.get(&ship_id))
        .ok_or("Your ship is no longer in space.")?;
    if ship.identity_reserved || !ship.is_pilot() || ship.pilot.char_id != ctx.session.char_id.unwrap_or(0)
        || ship.client_id != ctx.client_id {
        return Err("You no longer control that ship or it is changing state.".into());
    }
    if ship.warping() { return Err("You cannot jump while in warp.".into()); }
    if ship.cloaked { return Err("You cannot jump while cloaked.".into()); }
    Ok(())
}

/// What a jump drive offers: (range in light years, fuel type, fuel a light
/// year, capacitor needed as a share of capacity, capacitor capacity).
pub struct Drive {
    pub range: f64,
    pub fuel: i64,
    pub fuel_per_ly: f64,
    pub capacitor_share: f64,
    pub capacitor: f64,
}

/// The ship's jump drive, with its pilot's skills applied; None if it has none.
pub async fn drive(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Option<Drive> {
    let loadout = ctx.store.read(|db| Loadout::load(db, ctx.dogma, char_id, Some(ship_id), &HashSet::new())).ok()?;
    let i = loadout.find(ship_id)?;
    let v = |a: i32| loadout.value(i, a);
    if v(ATTR_CAN_JUMP) <= 0.0 && v(ATTR_JUMP_RANGE) <= 0.0 {
        return None;
    }
    Some(Drive {
        range: v(ATTR_JUMP_RANGE),
        fuel: v(ATTR_JUMP_FUEL_TYPE) as i64,
        fuel_per_ly: v(ATTR_JUMP_FUEL_PER_LY),
        capacitor_share: v(ATTR_JUMP_CAPACITOR_NEED),
        capacitor: v(ATTR_CAPACITOR_CAPACITY),
    })
}

/// Checks a jump to a gang member's field and quotes it: in the gang,
/// the field lit, within range, fuel in the hold and capacitor to spare.
/// Returns where the jump lands.
pub async fn prepare_beacon_jump(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, member: i64, beacon: i64, to: i64) -> Refused<PreparedJump> {
    if from == to {
        return Err("The field is in this system.".into());
    }
    let at = jump_field(&ctx.world.lock(), char_id, ship_id, from, member, beacon, to)?;
    let mut quote = prepare_drive_jump(ctx, char_id, ship_id, from, to, at).await?;
    // Loading skills/fuel yields; a departed gang member or extinguished field
    // must not authorize a jump from an earlier snapshot.
    may_jump(ctx,ship_id,from)?;
    quote.at = landing(jump_field(&ctx.world.lock(), char_id, ship_id, from, member, beacon, to)?);
    Ok(quote)
}

fn jump_field(w: &WorldState, char_id:i64, ship_id:i64, from:i64, member:i64, beacon:i64, to:i64) -> Refused<Vec3> {
    if w.gangs.of(char_id).is_none() || w.gangs.of(char_id)!=w.gangs.of(member) {
        return Err("You can only jump to a cynosural field lit by a member of your gang.".into());
    }
    let at=field_at(w,to,beacon,member).ok_or("That cynosural field has gone out.")?;
    let field=&w.parks[&to].beacons[&beacon];
    let ship=w.parks.get(&from).and_then(|p|p.ships.get(&ship_id)).ok_or("Your ship is no longer in space.")?;
    if field.type_id==beacons::TYPE_COVERT_CYNOSURAL_FIELD && ship.pilot.group_id!=898 {
        return Err("Only Black Ops jump drives can lock onto a covert cynosural field.".into());
    }
    Ok(at)
}

pub async fn prepare_alliance_jump(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, beacon: i64, to: i64) -> Refused<PreparedJump> {
    let at = alliance_field_at(ctx, to, beacon).await?;
    prepare_drive_jump(ctx, char_id, ship_id, from, to, at).await
}

async fn prepare_drive_jump(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, to: i64, at: Vec3) -> Refused<PreparedJump> {
    may_jump(ctx, ship_id, from)?;
    if from == to { return Err("The field is in this system.".into()); }
    let Some(d) = drive(ctx, char_id, ship_id).await else { return Err("Your ship has no jump drive.".into()) };
    let ly = light_years(ctx, from, to).await.ok_or("Those systems could not be found.")?;
    if ly > d.range {
        return Err(format!("The field is {ly:.2} light years away; your jump drive reaches {:.2}.", d.range));
    }
    let need = d.capacitor * d.capacitor_share;
    let now = crate::combat::now_ms();
    let enough = {
        let w = ctx.world.lock();
        w.parks.get(&from).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref())
            .is_some_and(|f| f.vitals.capacitor.at(now)>=need)
    };
    if !enough {
        return Err(format!("A jump needs {:.0}% of your capacitor.", d.capacitor_share * 100.0));
    }
    let fuel=FuelCost { cargo:ship_id,type_id:d.fuel,quantity:(d.fuel_per_ly*ly).ceil() as i64,flag:Some(FLAG_CARGO) };
    fuel.check(ctx).await?;
    Ok(PreparedJump {at:landing(at),fuel,capacitor:need,clients:vec![ctx.client_id]})
}

/// Somewhere near a field.
pub fn landing(at: Vec3) -> Vec3 {
    let dir = crate::combat::Rng::seeded(crate::combat::now_ms() as u64).in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
    at + dir * 4_000.0
}

/// BridgeToMember(charID, beaconID, solarsystemID): a titan opens a portal
/// to a gang member's field.
pub async fn open_bridge(ctx: &Ctx<'_>, member: i64, beacon: i64, to: i64) -> Refused<()> {
    let (Some(me), Some(ship_id), Some(from)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Err("You must be in space to open a bridge.".into());
    };
    let together = {
        let w = ctx.world.lock();
        let g = w.gangs.of(me);
        g.is_some() && g == w.gangs.of(member)
    };
    if !together {
        return Err("You can only bridge to a field lit by a member of your gang.".into());
    }
    if field_at(&ctx.world.lock(), to, beacon, member).is_none() {
        return Err("That cynosural field has gone out.".into());
    }
    open_bridge_to(ctx, me, ship_id, from, Some(member), beacon, to).await
}

pub async fn open_bridge_structure(ctx: &Ctx<'_>, beacon: i64, to: i64) -> Refused<()> {
    let (Some(me), Some(ship_id), Some(from)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Err("You must be in space to open a bridge.".into());
    };
    alliance_field_at(ctx, to, beacon).await?;
    open_bridge_to(ctx, me, ship_id, from, None, beacon, to).await
}

async fn open_bridge_to(ctx: &Ctx<'_>, me: i64, ship_id: i64, from: i64, member: Option<i64>, beacon: i64, to: i64) -> Refused<()> {
    may_jump(ctx, ship_id, from)?;
    if from == to { return Err("The field is in this system.".into()); }
    portal::open(ctx, me, ship_id, from, member, beacon, to).await
}

/// Bridges whose minute is up close.
pub(crate) fn portal_is_live(w:&WorldState,ship:i64,bridge:&crate::world::Bridge)->bool {
    portal::bridge_live(w,ship,bridge)
}

/// Expired portals and portals whose live source or member field disappeared.
pub fn close_bridges(w: &mut WorldState, now_ms: i64) {
    w.portal_cycles.retain(|_,cycle|cycle.until_ms>now_ms);
    let over: Vec<i64> = w.bridges.iter().filter(|(ship, b)| b.until_ms <= now_ms
        || !portal::bridge_live(w, **ship, b)).map(|(id, _)| *id).collect();
    for ship in over {
        let Some(b) = w.bridges.remove(&ship) else { continue };
        let (clients,event) = if b.member.is_some() { (gang_clients(w,b.titan_pilot),"OnBridgeModeChange") }
            else { (w.in_space(b.system),"OnAllianceBridgeModeChange") };
        w.notify(&clients, event, Value::Tuple(vec![Value::Int(ship), Value::Int(b.to_system), Value::Int(b.beacon), Value::Bool(false)]));
    }
}

/// Checks a jump through a titan's bridge and quotes the titan's payment.
/// Returns where it lands.
pub async fn prepare_jump_through(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, titan_pilot: i64, titan: i64, alliance: bool) -> Refused<(i64, PreparedJump)> {
    may_jump(ctx, ship_id, from)?;
    let (bridge, mass, gap, titan_cargo) = {
        let w = ctx.world.lock();
        let Some(b) = w.bridges.get(&titan).cloned() else { return Err("That titan has no bridge open.".into()) };
        if b.until_ms <= crate::combat::now_ms() { return Err("That jump portal has closed.".into()); }
        if !portal::bridge_live(&w,titan,&b) { return Err("That jump portal is no longer operational.".into()); }
        let park = w.parks.get(&from);
        if b.titan_pilot != titan_pilot { return Err("That is not the pilot of this titan.".into()); }
        if alliance {
            let owner = park.and_then(|p|p.ships.get(&titan)).and_then(|s|s.pilot.alliance_id).filter(|id|*id>0);
            if b.member.is_some() || owner.is_none() || owner != ctx.session.alliance_id {
                return Err("Only the titan's alliance may use this bridge.".into());
            }
        } else {
            let g = w.gangs.of(char_id);
            if g.is_none() || g != w.gangs.of(titan_pilot) {
                return Err("Only the titan's gang may use its bridge.".into());
            }
        }
        let gap = park.and_then(|p| p.surface_distance(ship_id, titan)).unwrap_or(f64::MAX);
        let mass = park.and_then(|p| p.ships.get(&ship_id)).map_or(0.0, |s| s.stats.mass);
        (b, mass, gap, titan)
    };
    if bridge.system != from {
        return Err("That titan is not in this system.".into());
    }
    if gap > BRIDGE_RANGE {
        return Err(format!("You must be within {:.0} km of the titan to jump through its bridge.", BRIDGE_RANGE / 1_000.0));
    }
    if bridge.harmonics>0 {
        let loadout=ctx.store.read(|db|Loadout::load(db,ctx.dogma,char_id,Some(ship_id),&HashSet::new())).map_err(payment_error)?;
        let index=loadout.find(ship_id).ok_or("Your ship could not be read.")?;
        if loadout.value(index,1253)<bridge.harmonics as f64 {
            return Err("Your ship does not have the jump harmonics required by that covert portal.".into());
        }
    }
    let generator_online = ctx.store.read(|db| {
        let fitted = db.table("entity").get_id(bridge.generator_id).is_some_and(|e| {
            e.int("ownerID") == Some(titan_pilot) && e.int("locationID") == Some(titan) && e.int("flag").is_some_and(|f| (11..=35).contains(&f))
                && e.int("singleton") == Some(1) && e.int("quantity") == Some(1)
        });
        fitted && db.table("entity_attributes").get(&[store::Value::Int(bridge.generator_id), store::Value::Int(2)])
            .is_some_and(|a| a.float("valueFloat").or(a.float("valueInt")).unwrap_or(0.0) != 0.0)
    });
    if !generator_online {
        return Err("The portal generator is no longer online and fitted to that ship.".into());
    }
    let at = match bridge.member {
        Some(member) => field_at(&ctx.world.lock(), bridge.to_system, bridge.beacon, member)
            .ok_or("The field at the far end has gone out.")?,
        None => alliance_field_at(ctx, bridge.to_system, bridge.beacon).await?,
    };
    let ly = light_years(ctx, from, bridge.to_system).await.ok_or("Those systems could not be found.")?;
    let drive = drive(ctx,titan_pilot,titan).await.ok_or("The titan's jump drive is unavailable.")?;
    let quantity = (mass * bridge.mass_factor * drive.fuel_per_ly * ly).ceil().max(1.0) as i64;
    let titan_clients = ctx.world.lock().clients_of(titan_pilot);
    let fuel=FuelCost { cargo:titan_cargo,type_id:drive.fuel,quantity,flag:Some(FLAG_CARGO) };
    fuel.check(ctx).await.map_err(|why|format!("The titan cannot power your jump: {why}"))?;
    may_jump(ctx,ship_id,from)?;
    {
        let w=ctx.world.lock();
        if w.bridges.get(&titan)!=Some(&bridge) || bridge.until_ms<=crate::combat::now_ms() || !portal::bridge_live(&w,titan,&bridge) {
            return Err("The jump portal changed while preparing your jump.".into());
        }
        let park=w.parks.get(&from).ok_or("Your ship is no longer in space.")?;
        if park.surface_distance(ship_id,titan).unwrap_or(f64::MAX)>BRIDGE_RANGE {
            return Err("You moved out of range of the jump portal.".into());
        }
        if alliance {
            if park.ships.get(&titan).and_then(|s|s.pilot.alliance_id).filter(|id|*id>0)!=ctx.session.alliance_id {
                return Err("The titan is no longer in your alliance.".into());
            }
        } else if w.gangs.of(char_id).is_none() || w.gangs.of(char_id)!=w.gangs.of(titan_pilot) {
            return Err("You are no longer in the titan's gang.".into());
        }
    }
    Ok((bridge.to_system,PreparedJump {at:landing(at),fuel,capacitor:0.0,clients:titan_clients}))
}
