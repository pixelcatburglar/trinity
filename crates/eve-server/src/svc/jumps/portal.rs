//! Opening a titan portal has one accepted-start point: the final live check
//! after staging payment, at the end of the store write that pays. A later
//! loss of the destination closes that accepted portal; it can never make an
//! unusable destination available to travelers.

use super::*;
use crate::combat::{self, Charged};
use crate::dogma::{Catalog, State};
use crate::store::Store;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
mod journal;
pub use journal::{Pending, ensure, blocked, blocked_character, blocked_in_memory, blocked_character_in_memory, retry};

#[derive(Clone)]
struct Request {
    world: World,
    pilot: i64,
    client: i64,
    ship: i64,
    from: i64,
    member: Option<i64>,
    beacon: i64,
    to: i64,
    alliance: Option<i64>,
    bonuses: BTreeMap<i32, f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Quote {
    generator_id: i64,
    generator_type: i64,
    fuel_type: i64,
    fuel: i64,
    capacitor: f64,
    duration: i64,
    mass_factor: f64,
    range: f64,
    harmonics: i64,
}

struct Reservation {
    world: World,
    system: i64,
    ship: i64,
    pilot: i64,
    client: i64,
    capacitor: f64,
    before: Charged,
    reserved: Charged,
    state: CommitState,
    receipt: Option<journal::Receipt>,
}
#[derive(PartialEq)] enum CommitState {Prepared, Accepted}

impl Drop for Reservation {
    fn drop(&mut self) {
        // An accepted start is the journal's to finish; only a refused one,
        // whose payment the store write undid, is released here.
        if self.state != CommitState::Prepared {return;}
        let mut w = self.world.lock();
        if let Some(receipt)=&self.receipt {w.pending_portals.remove(&receipt.generation);}
        w.pending_bridges.remove(&self.ship);
        if let Some(ship) = w.parks.get_mut(&self.system).and_then(|p| p.ships.get_mut(&self.ship)) {
            if ship.pilot.char_id == self.pilot && ship.client_id == self.client {
                if let Some(f) = &mut ship.fight {
                    if self.capacitor > 0.0 {
                        if f.vitals.capacitor == self.reserved { f.vitals.capacitor = self.before; }
                        else { f.vitals.capacitor.add(combat::now_ms(), self.capacitor); }
                    }
                }
                ship.identity_reserved = false;
            }
        }
        let Some(f) = w.parks.get(&self.system).and_then(|p| p.ships.get(&self.ship)).and_then(|s| s.fight.as_ref()) else { return; };
        let now = combat::now_ms();
        let amount = f.vitals.capacitor.at(now);
        w.notify(&[self.client], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(vec![Value::Tuple(vec![
            Value::bytes("OnModuleAttributeChange"), Value::Int(self.pilot), Value::Int(self.ship),
            Value::Int(combat::attr::CHARGE as i64), Value::Long(combat::filetime(now)), Value::Double(amount), Value::Double(amount),
        ])])]));
    }
}

/// The exact remaining accepted cycle, without inventing a logout grace.
pub fn active_until_locked(w:&WorldState,ship:i64)->Option<i64> {
    w.portal_cycles.get(&ship).filter(|b|b.until_ms>combat::now_ms() && w.parks.get(&b.system).and_then(|p|p.ships.get(&ship))
        .is_some_and(|s|s.is_pilot() && s.pilot.char_id==b.titan_pilot && s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage<f.vitals.hull_max-1e-6)))
        .map(|b|b.until_ms)
}

fn database(error: StoreError) -> String {
    tracing::error!("opening jump portal: {}", crate::logging::chain(&error));
    "The jump portal could not be opened.".into()
}

/// A refusal inside a portal's store write, which undoes what it did.
struct Refusal(String);

impl From<String> for Refusal {
    fn from(why: String) -> Self {
        Refusal(why)
    }
}

impl From<&str> for Refusal {
    fn from(why: &str) -> Self {
        Refusal(why.into())
    }
}

impl From<StoreError> for Refusal {
    fn from(error: StoreError) -> Self {
        Refusal(database(error))
    }
}

/// Whether the fixed beacon `beacon` stands online in `to`, owned by a
/// corporation of `alliance`, and `corporation` is in that alliance too.
fn beacon_available(db: &Db, beacon: i64, to: i64, alliance: i64, corporation: i64) -> bool {
    let in_alliance = |corp: i64| db.table("corporation").get_id(corp).is_some_and(|c| c.int("allianceID") == Some(alliance));
    db.table("entity").get_id(beacon).is_some_and(|e| {
        e.int("locationID") == Some(to)
            && e.int("typeID").is_some_and(|t| crate::sde::get().types_in_group(838).contains(&(t as i32)))
            && db.table("posStructures").get_id(beacon).is_some_and(|p| matches!(p.int("state"), Some(4 | 5)))
            && e.int("ownerID").is_some_and(in_alliance)
    }) && in_alliance(corporation)
}

pub(super) async fn open(ctx: &Ctx<'_>, pilot: i64, ship: i64, from: i64, member: Option<i64>, beacon: i64, to: i64) -> Refused<()> {
    let request = Request { world:ctx.world.clone(), pilot, client:ctx.client_id,
        ship, from, member, beacon, to, alliance:ctx.session.alliance_id,
        bonuses:ctx.dogma.fleet_bonuses(pilot) };
    // Dropping the caller cannot cancel the finalization of a payment that
    // has been accepted. Before payment, every refusal releases RAII state.
    tokio::spawn(run(request, ctx.store.clone())).await.map_err(|error| {
        tracing::error!("portal startup task: {error}");
        "The jump portal request could not be completed.".to_string()
    })?
}

fn source(w: &WorldState, r: &Request, reserved: bool) -> Refused<()> {
    let hull = w.parks.get(&r.from).and_then(|p| p.ships.get(&r.ship)).ok_or("Your ship is no longer in space.")?;
    if !hull.is_pilot() || hull.client_id != r.client || hull.pilot.char_id != r.pilot || hull.identity_reserved != reserved {
        return Err("You no longer control that ship or it is changing state.".into());
    }
    if hull.warping() || hull.cloaked { return Err("You cannot open a portal while warping or cloaked.".into()); }
    if hull.fight.as_ref().is_none_or(|f| f.vitals.hull_max <= 0.0 || f.vitals.hull_damage >= f.vitals.hull_max) {
        return Err("Your ship cannot open a portal now.".into());
    }
    if w.pending_bridges.contains(&r.ship) != reserved || w.bridges.get(&r.ship).is_some_and(|b| b.until_ms > combat::now_ms()
        && !(reserved && w.pending_portals.get(&b.generation).is_some_and(|p|p.owns(r.ship,r.pilot,r.client)))) {
        return Err("Your jump portal is already open or being opened.".into());
    }
    if !reserved && active_until_locked(w,r.ship).is_some() {return Err("Your portal generator's accepted cycle is still active.".into());}
    Ok(())
}

fn destination(w: &WorldState, r: &Request, harmonics:i64) -> Refused<()> {
    if let Some(member) = r.member {
        let gang = w.gangs.of(r.pilot);
        if gang.is_none() || gang != w.gangs.of(member) { return Err("The field's pilot is no longer in your gang.".into()); }
        if field_at(w, r.to, r.beacon, member).is_none() { return Err("That cynosural field has gone out.".into()); }
        if w.parks[&r.to].beacons[&r.beacon].type_id == 28650 && harmonics < 2 {
            return Err("A standard portal cannot lock onto a covert cynosural field.".into());
        }
    } else {
        // The paying store write checks the fixed beacon's actual owner and
        // state. Also reject a live source whose alliance changed during
        // preparation.
        let hull = w.parks.get(&r.from).and_then(|p| p.ships.get(&r.ship)).ok_or("Your ship is no longer in space.")?;
        if r.alliance.filter(|id| *id > 0) != hull.pilot.alliance_id.filter(|id| *id > 0) {
            return Err("Your alliance changed while opening the portal.".into());
        }
    }
    Ok(())
}

/// Dynamic invalidation is checked on every heartbeat and before portal use.
pub(super) fn bridge_live(w: &WorldState, ship: i64, b: &crate::world::Bridge) -> bool {
    let Some(hull) = w.parks.get(&b.system).and_then(|p| p.ships.get(&ship)) else { return false; };
    if !hull.is_pilot() || hull.pilot.char_id != b.titan_pilot || hull.warping() || hull.cloaked
        || hull.fight.as_ref().is_none_or(|f| f.vitals.hull_max <= 0.0 || f.vitals.hull_damage >= f.vitals.hull_max) { return false; }
    match b.member {
        Some(member) => w.gangs.of(b.titan_pilot).is_some() && w.gangs.of(b.titan_pilot) == w.gangs.of(member)
            && field_at(w, b.to_system, b.beacon, member).is_some()
            && w.parks[&b.to_system].beacons.get(&b.beacon).is_some_and(|field|field.type_id != 28650 || b.harmonics >= 2),
        None => true, // Fixed-beacon authorization is freshly checked for every transit.
    }
}

async fn run(r: Request, store: Arc<Store>) -> Refused<()> {
    let (receipt, events, _owner) = store.write(|tx| -> Result<_, Refusal> {
    let mut txn = ItemTxn::new(tx);
    if txn.db().table("character_").get_id(r.pilot).is_none() {
        return Err("That character is no longer available.".into());
    }
    if crate::losses::pending::blocked_character(txn.db(),r.pilot)?
        || crate::losses::journal::blocked_character_in_memory(&r.world,r.pilot)
        || crate::travel::blocked_character_in_memory(&r.world,r.pilot)
        || crate::conn::logout::blocked_character_in_memory(&r.world,r.pilot)
        || blocked_character_in_memory(&r.world,r.pilot) {
        return Err("Your ship has an unresolved state transition or loss.".into());
    }
    let corporation = txn.db().table("character_").get_id(r.pilot)
        .filter(|c| c.int("shipID")==Some(r.ship) && c.int("solarSystemID")==Some(r.from) && c.int("stationID").unwrap_or(0)==0)
        .map(|c| c.int("corporationID").unwrap_or(0))
        .ok_or("Your ship or location changed. Try again.")?;
    let hull = txn.item(r.ship)?.ok_or("That ship no longer exists.")?;
    if hull.owner_id != r.pilot || hull.location_id != r.from || hull.flag != 0 || hull.quantity != 1 || !hull.singleton || hull.category_id != 6 {
        return Err("You no longer control that ship here in space.".into());
    }
    if !journal::retire_ready(txn.tx(),&r.world,r.ship)? {return Err("Your jump portal is already open or being opened.".into());}
    // Every possible generator, fitted and whole; their effective attributes
    // are loaded in this same write, so no earlier loadout grants custody.
    let generator_types = crate::sde::get().types_in_group(GROUP_JUMP_PORTAL_GENERATOR);
    let mut ids: Vec<i64> = txn.db().table("entity").find(&["locationID"], &[store::Value::Int(r.ship)])
        .filter(|e| e.int("ownerID")==Some(r.pilot) && e.int("typeID").is_some_and(|t| generator_types.contains(&(t as i32)))
            && e.int("flag").is_some_and(|f| (11..=35).contains(&f)) && e.int("singleton")==Some(1) && e.int("quantity")==Some(1))
        .filter_map(|e| e.int("itemID"))
        .collect();
    ids.sort_unstable();
    if ids.is_empty() { return Err("You have no Jump Portal Generator fitted.".into()); }
    let catalog = Catalog::new();
    catalog.replace_fleet_bonuses(HashMap::from([(r.pilot, r.bonuses.clone())]));
    let active = r.world.lock().parks.get(&r.from).and_then(|p|p.ships.get(&r.ship)).and_then(|s|s.fight.as_ref())
        .map(|f|f.applied.iter().copied().collect()).unwrap_or_default();
    let quote = {
    let loadout = Loadout::load(txn.db(), &catalog, r.pilot, Some(r.ship), &active)?;
    let ship_index = loadout.find(r.ship).ok_or("Your ship could not be read.")?;
    if loadout.value(ship_index, ATTR_CAN_JUMP) <= 0.0 && loadout.value(ship_index, ATTR_JUMP_RANGE) <= 0.0 {
        return Err("Your ship cannot open a jump portal.".into());
    }
    let module = loadout.pieces.iter().position(|p| ids.contains(&p.item_id) && p.state != State::Offline)
        .ok_or("You have no online Jump Portal Generator fitted.")?;
    if loadout.value(module, combat::attr::HP) > 0.0 && loadout.value(module, combat::attr::DAMAGE) >= loadout.value(module, combat::attr::HP) {
        return Err("Repair that portal generator before activating it.".into());
    }
    let value = |attr| loadout.value(module, attr);
    let allowed_group=value(1298) as i64;
    if allowed_group>0 && allowed_group!=loadout.pieces[ship_index].group_id {
        return Err("That portal generator cannot operate on this class of ship.".into());
    }
    Quote { generator_id:loadout.pieces[module].item_id, generator_type:loadout.pieces[module].type_id, fuel_type:value(ATTR_CONSUMPTION_TYPE) as i64, fuel:value(ATTR_CONSUMPTION_QUANTITY) as i64,
        // Original 45017 effect2152 points to jumpPortalDuration1002.
        capacitor:value(6), duration:value(1002) as i64,
        mass_factor:value(ATTR_PORTAL_MASS_FACTOR), range:loadout.value(ship_index, ATTR_JUMP_RANGE),
        harmonics:if value(1253)>=2.0 {2}else{0} }
    };
    if quote.fuel < 0 || (quote.fuel>0 && quote.fuel_type <= 0) || quote.fuel > i32::MAX as i64 || quote.duration <= 0
        || quote.duration > i32::MAX as i64 || !quote.capacitor.is_finite() || quote.capacitor < 0.0
        || !quote.mass_factor.is_finite() || quote.mass_factor <= 0.0 || !quote.range.is_finite() || quote.range <= 0.0 {
        return Err("That portal generator has invalid operating attributes.".into());
    }
    let ly = (system_at(txn.db(), r.from).ok_or("The source system was not found.")? - system_at(txn.db(), r.to).ok_or("The destination system was not found.")?).len()/METERS_PER_LIGHT_YEAR;
    if !ly.is_finite() || ly > quote.range { return Err(format!("That field is outside your portal's {:.2} light-year range.",quote.range).into()); }
    if r.member.is_none() {
        let alliance = r.alliance.filter(|a|*a>0).ok_or("You must belong to the beacon's alliance.")?;
        // Owner, system, operational state, and both corporations' alliances
        // are read in the same write that pays. No cached session grant.
        if !beacon_available(txn.db(), r.beacon, r.to, alliance, corporation) { return Err("That alliance beacon is no longer available to your corporation.".into()); }
    }
    let mut reservation = {
        let mut w = r.world.lock();
        source(&w,&r,false)?; destination(&w,&r,quote.harmonics)?;
        let hull = w.parks.get_mut(&r.from).unwrap().ships.get_mut(&r.ship).unwrap();
        let capacitor = &mut hull.fight.as_mut().unwrap().vitals.capacitor;
        let before = *capacitor;
        if !capacitor.spend(combat::now_ms(),quote.capacitor) { return Err("Your capacitor is too low to open a portal.".into()); }
        let reserved = *capacitor;
        hull.identity_reserved = true;
        w.pending_bridges.insert(r.ship);
        Reservation { world:r.world.clone(),system:r.from,ship:r.ship,pilot:r.pilot,client:r.client,
            capacitor:quote.capacitor,before,reserved,state:CommitState::Prepared,receipt:None }
    };
    if quote.fuel>0 { FuelCost { cargo:r.ship,type_id:quote.fuel_type,quantity:quote.fuel,flag:Some(FLAG_CARGO) }.debit(&mut txn)?; }
    let events=txn.events();
    let charge={let w=r.world.lock();source(&w,&r,true)?;destination(&w,&r,quote.harmonics)?;
        w.parks[&r.from].ships[&r.ship].fight.as_ref().unwrap().vitals.capacitor.at(combat::now_ms())};
    if !charge.is_finite() || charge<0.0 {return Err("Your capacitor state is invalid.".into());}
    // Persist the already-reserved live charge with fuel and the receipt. The
    // marker still quarantines unknown later combat; finalization writes fresh
    // current vitals, never replays this captured charge into the live ship.
    let charged=[("valueInt",store::Value::Null),("valueFloat",store::Value::Float(charge))];
    txn.tx().upsert("entity_attributes",&["itemID","attributeID"],
        &[("itemID",store::Value::Int(r.ship)),("attributeID",store::Value::Int(18)),charged[0].clone(),charged[1].clone()],&charged)?;
    let receipt=journal::begin(txn.tx(),&r,&quote,hull.type_id)?;
    reservation.receipt=Some(receipt.clone());
    {
        let mut w = r.world.lock();
        source(&w,&r,true)?; destination(&w,&r,quote.harmonics)?;
        w.pending_portals.insert(receipt.generation,Pending::owned(receipt.clone(),r.client,reservation.before,reservation.reserved));
    } // Accepted-start point. Later dynamic revocations close this accepted portal.
    let owner=journal::owner_guard(&r.world,receipt.generation);
    reservation.state=CommitState::Accepted;
    Ok((receipt,events,owner))
    }).map_err(|Refusal(why)| why)?;
    journal::complete_owned(&r,&store,&receipt,events).await
}

fn announce(w:&WorldState,r:&Request,open:bool) {
    let (clients,event)=if r.member.is_some() { (gang_clients(w,r.pilot),"OnBridgeModeChange") }
        else { (w.in_space(r.from),"OnAllianceBridgeModeChange") };
    w.notify(&clients,event,Value::Tuple(vec![Value::Int(r.ship),Value::Int(r.to),Value::Int(r.beacon),Value::Bool(open)]));
}
