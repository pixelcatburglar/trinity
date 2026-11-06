//! What losing a ship costs, done once the heartbeat has decided it happened.
//!
//! A ship goes with half of what was in and on it; the other half floats in a
//! can where it died. Its insurance pays out. The pilot ejects in a capsule.
//! A capsule lost costs the implants and, beyond what the clone keeps, skill
//! points from the pilot's biggest skill; the pilot wakes in a fresh Alpha
//! clone at its home station (see `svc::medical`). Every loss is written to
//! the kill log and mailed, in the old killmail form, to the victim and
//! whoever landed the final blow.

use std::collections::BTreeSet;

use eve_marshal::Value;
use tracing::{error, info, warn};

use crate::ballpark::{Ship, Vec3};
use crate::combat::{self, attr};
use crate::conn::Server;
use crate::items::{self, ItemTxn, NewItem};
use crate::loot::{self, Stash};
use crate::store::{self, Db, Store, StoreError, Tx};
use crate::svc::medical::{GROUP_CLONE, TYPE_CLONE_ALPHA};
use crate::svc::{inflight, space};
use crate::world::Move;

mod docked;
pub mod pending;
pub mod journal;
pub use docked::ArrivalDisposition;

const TYPE_CAPSULE: i64 = 670;
const FLAG_HANGAR: i64 = 4;
const FLAG_IMPLANT: i64 = 89;
const FLAG_SKILL: i64 = 7;
const FLAG_SKILL_IN_TRAINING: i64 = 61;
/// A ship in space sits in its solar system with no flag.
const FLAG_IN_SPACE: i64 = 0;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;
/// market_refTypes: "Insurance".
const REF_INSURANCE: i32 = 19;
const OWNER_SYSTEM: i64 = 1;
/// Of what was in a destroyed ship, this share survives into the can.
const SURVIVING_SHARE: f64 = 0.5;

/// Who hit a ship that died, and with what.
#[derive(Debug, Clone,serde::Serialize,serde::Deserialize)]
pub struct Attacker {
    /// A character, or for an NPC minus its type.
    pub who: i64,
    pub ship_type: i64,
    pub weapon_type: i64,
    pub damage: f64,
}

/// Accepted travel moved custody after the source encounter. Keep that
/// placement separate from the encounter recorded in a kill report.
#[derive(Debug, Clone, Copy, PartialEq,serde::Serialize,serde::Deserialize)]
pub struct SpaceArrivalCustody {
    pub system: i64,
    pub at: Vec3,
}

#[derive(Debug, Clone,serde::Serialize,serde::Deserialize)]
pub struct Loss {
    #[serde(skip)]
    pub client_id: i64,
    pub char_id: i64,
    pub ship_id: i64,
    pub ship_type: i64,
    pub system: i64,
    pub pos: Vec3,
    /// A confirmed loss after an accepted space arrival belongs to the
    /// destination inventory. Attribution still describes the source fight.
    #[serde(default)]
    pub arrival_custody: Option<SpaceArrivalCustody>,
    pub capsule: bool,
    /// An abandoned hull retains its owner but has no occupant to eject or pod.
    pub unpiloted: bool,
    /// Who landed the final blow, as in [`Attacker::who`].
    pub final_blow: Option<i64>,
    pub attackers: Vec<Attacker>,
    /// For a bot: the fittings and cargo destroyed with it, (type, quantity).
    pub items: Vec<(i64, i64)>,
}

impl Loss {
    pub fn custody_system(&self) -> i64 {
        self.arrival_custody.as_ref().map_or(self.system, |arrival| arrival.system)
    }

    pub fn custody_position(&self) -> Vec3 {
        self.arrival_custody.as_ref().map_or(self.pos, |arrival| arrival.at)
    }

    pub fn of(ship: &Ship, system: i64, capsule: bool) -> Loss {
        let fight = ship.fight.as_ref();
        let attackers = fight
            .map(|f| {
                f.damage_log
                    .iter()
                    .map(|(who, (ship_type, weapon_type, damage))| Attacker { who: *who, ship_type: *ship_type, weapon_type: *weapon_type, damage: *damage })
                    .collect()
            })
            .unwrap_or_default();
        let final_blow = fight.and_then(|f| f.last_attacker).map(|(who, _)| who);
        Loss {
            client_id: ship.client_id,
            char_id: ship.pilot.char_id,
            ship_id: ship.id,
            ship_type: ship.pilot.type_id,
            system,
            pos: ship.pos,
            arrival_custody: None,
            capsule,
            unpiloted: ship.unpiloted,
            final_blow,
            attackers,
            items: Vec::new(),
        }
    }
}

pub async fn handle(server: &Server, store: &Store, loss: Loss) {
    journal::handle(server,store,loss).await;
}

/// A bot's ship destroyed: the kill log and the killmail, as for a player.
/// The rest (a new ship, waking at home) is the bot simulation's.
pub async fn bot_lost(server: &Server, store: &Store, loss: Loss) {
    let report = store.read(|db| report_text(db, &loss));
    let mut destroyed = Vec::new();
    for (t, n) in &loss.items {
        let name = type_name(*t);
        destroyed.push(if *n > 1 { format!("{name}, Qty: {n}") } else { name });
    }
    let items = format!("<items>{}</items>", loss.items.iter().map(|(ty, quantity)| format!(r#"<i t="{ty}" f="0" d="0" x="{quantity}"/>"#)).collect::<String>());
    record(server, store, &loss, report, destroyed, items).await;
}

/// `killLog."killBlob"` and `killLogParticipants."securityStatus"` are in the
/// store's schema.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// Why a write that settles custody kept nothing: the store refused a
/// change, or custody was not as the settlement needs it (the reason the
/// caller is given).
enum Refused {
    Store(StoreError),
    Because(String),
}

impl From<StoreError> for Refused {
    fn from(error: StoreError) -> Self {
        Refused::Store(error)
    }
}

impl From<&str> for Refused {
    fn from(reason: &str) -> Self {
        Refused::Because(reason.into())
    }
}

impl From<String> for Refused {
    fn from(reason: String) -> Self {
        Refused::Because(reason)
    }
}

impl Refused {
    /// The reason, with a store's refusal put as `failed` puts it.
    fn reason(self, failed: fn(StoreError) -> String) -> String {
        match self {
            Refused::Store(error) => failed(error),
            Refused::Because(reason) => reason,
        }
    }
}

/// Whether the character sits in `ship_id` in space in `system`.
fn aboard(db: &Db, char_id: i64, ship_id: i64, system: i64) -> bool {
    db.table("character_").get_id(char_id).is_some_and(|c| {
        c.int("shipID") == Some(ship_id) && c.int("solarSystemID") == Some(system) && c.int("stationID").unwrap_or(0) == 0
    })
}

/// Set columns of the row of `table` whose key is `id` (nothing if there is
/// none, as an UPDATE matching no row did).
fn update_id(tx: &mut Tx<'_>, table: &str, id: i64, sets: store::Sets) -> Result<(), StoreError> {
    match tx.table(table).get_id(id).map(|r| r.id) {
        Some(row) => tx.update(table, row, sets),
        None => Ok(()),
    }
}

#[derive(Debug)]
struct LostItem { id: i64, type_id: i64, flag: i64, quantity: i64, parent: i64 }

/// Everything in and on the lost hull at any depth (for a capsule, what is in
/// the pilot's head), never the pilot.
fn item_snapshot(db: &Db, loss: &Loss) -> Vec<LostItem> {
    let pod = loss.capsule && !loss.unpiloted;
    let root = if pod { loss.char_id } else { loss.ship_id };
    let entity = db.table("entity");
    let lost = |e: store::Row| LostItem {
        id: e.int("itemID").unwrap_or(0),
        type_id: e.int("typeID").unwrap_or(0),
        flag: e.int("flag").unwrap_or(0),
        quantity: e.int("quantity").unwrap_or(0),
        parent: e.int("locationID").unwrap_or(0),
    };
    let mut out: Vec<LostItem> = entity
        .find(&["locationID"], &[store::Value::Int(root)])
        .filter(|e| e.int("itemID") != Some(loss.char_id) && (!pod || e.int("flag") == Some(FLAG_IMPLANT)))
        .map(lost)
        .collect();
    let mut seen: std::collections::HashSet<i64> = out.iter().map(|i| i.id).collect();
    let mut next = 0;
    while next < out.len() {
        let parent = out[next].id;
        next += 1;
        for e in entity.find(&["locationID"], &[store::Value::Int(parent)]) {
            let id = e.int("itemID").unwrap_or(0);
            if id != loss.char_id && seen.insert(id) {
                out.push(lost(e));
            }
        }
    }
    out
}

fn item_blob(db: &Db, before: &[LostItem]) -> String {
    let ids = before.iter().map(|i| i.id).collect::<Vec<_>>();
    let entity = db.table("entity");
    let survivors = ids.iter().filter_map(|id| Some((*id, entity.get_id(*id)?.int("quantity")?))).collect::<std::collections::BTreeMap<_,_>>();
    fn xml(item: &LostItem, all: &[LostItem], survivors: &std::collections::BTreeMap<i64,i64>, depth: usize) -> String {
        if depth > 32 { return String::new(); }
        let dropped = survivors.get(&item.id).copied().unwrap_or(0).clamp(0,item.quantity.max(0));
        let children = all.iter().filter(|i| i.parent == item.id).map(|i| xml(i,all,survivors,depth+1)).collect::<String>();
        format!(r#"<i t="{}" f="{}" d="{}" x="{}">{children}</i>"#,item.type_id,item.flag,dropped,item.quantity.max(0)-dropped)
    }
    format!("<items>{}</items>",before.iter().filter(|i| !ids.contains(&i.parent)).map(|i|xml(i,before,&survivors,0)).collect::<String>())
}

// ---- the ship -----------------------------------------------------------------------------

/// A destroyed fitting or container takes its loaded charges and cargo with
/// it. ItemTxn::destroy intentionally operates on one item, so loss handling
/// revalidates each descendant before recording its notification.
fn destroy_tree(txn: &mut ItemTxn<'_, '_>, id: i64, parent:i64, pilot: i64) -> Result<(), StoreError> {
    txn.destroy_tree(id,Some(parent),&[pilot])
}

/// Returns what was destroyed, by type name, for the killmail.
async fn ship_lost(server: &Server, store: &Store, loss: &Loss) -> Option<(Vec<String>, String)> {
    let (char_id, ship_id) = (loss.char_id, loss.ship_id);
    let mut rng = combat::Rng::seeded(combat::now_ms() as u64 ^ ship_id as u64);
    let result = store.write(|tx| -> Result<_, StoreError> {
        let mut txn = ItemTxn::new(tx);
        // Do not redirect an owner who already left this ship before the
        // delayed destruction callback reached the store.
        let occupied = !loss.unpiloted && aboard(txn.db(), char_id, ship_id, loss.custody_system());
        // A repeated heartbeat callback must not create another capsule,
        // charge the same loss twice, or pay its insurance again.
        let Some(hull)=txn.item(ship_id)? else {return Ok(None);};
        if hull.owner_id!=char_id || hull.type_id!=loss.ship_type || hull.location_id!=loss.custody_system()
            || hull.flag!=FLAG_IN_SPACE || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {return Ok(None);}
        let before = item_snapshot(txn.db(), loss);
        // An item of a type the static data does not know is left out, as
        // joining on it did.
        let contents: Vec<(i64, String)> = txn
            .db()
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(ship_id)])
            .filter(|e| e.int("itemID") != Some(char_id))
            .filter_map(|e| Some((e.int("itemID")?, known_type_name(e.int("typeID")?)?)))
            .collect();
        let mut survivors = Vec::new();
        let mut destroyed = Vec::new();
        for (id, name) in contents {
            if rng.unit() < SURVIVING_SHARE {
                survivors.push(id);
            } else {
                destroy_tree(&mut txn, id, ship_id, char_id)?;
                destroyed.push(name);
            }
        }
        // Save the can and its complete contents in the same transaction as
        // destruction, so a failed can creation preserves the original ship.
        let stash: Vec<_> = survivors.into_iter().map(Stash::Existing).collect();
        let can = loot::stage_can(&mut txn, loss.custody_system(), loss.custody_position(), char_id, &stash)?;
        txn.destroy(ship_id)?;
        crate::svc::clones::ship_destroyed(txn.tx(), ship_id)?;
        let capsule = if occupied {
            let capsule = txn
                .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: loss.custody_system(), flag: FLAG_IN_SPACE, quantity: 1, singleton: true })?;
            txn.set_position(capsule,loss.custody_position().tuple())?;
            update_id(txn.tx(), "character_", char_id, &[("shipID", store::Value::Int(capsule))])?;
            // Pilot inventory location follows the same system/station
            // convention as normal travel; character_.shipID is the cockpit.
            let at = loss.custody_position();
            update_id(txn.tx(), "entity", char_id, &[
                ("locationID", store::Value::Int(loss.custody_system())),
                ("x", store::Value::Float(at.x)),
                ("y", store::Value::Float(at.y)),
                ("z", store::Value::Float(at.z)),
            ])?;
            Some(capsule)
        } else { None };
        let insurance = claim_insurance(txn.tx(), ship_id, crate::bulkdata::filetime_now())?;
        let blob = item_blob(txn.db(), &before);
        let report=report_text(txn.db(),loss);
        let record=stage_record(txn.tx(),loss,report,destroyed.clone(),blob.clone())?;
        Ok(Some((capsule, txn.events(), can, destroyed, insurance, blob,record)))
    });
    let (capsule_id, events, can, destroyed, insurance, blob,record) = match result {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => {
            error!("losing ship {ship_id} of {char_id}: {}", crate::logging::chain(&e));
            return None;
        }
    };
    if loss.unpiloted {
        let clients=server.world.lock().clients_of(char_id);
        for client in clients { items::notify(&server.world,client,events.clone()); }
    } else { items::notify(&server.world, loss.client_id, events); }
    if let Some(payout) = insurance { notify_insurance(server, loss, payout); }
    if let Some(can) = can { loot::show_can(&server.world, loss.custody_system(), can); }
    deliver_record(server,store,record).await;

    let Some(capsule_id)=capsule_id else {
        info!("unoccupied ship {ship_id} of {char_id} destroyed");
        return Some((destroyed,blob));
    };

    if let Err(error)=publish_capsule(server,store,loss,capsule_id).await {
        error!("publishing replacement capsule {capsule_id}: {}",crate::logging::chain(&error));
    }
    info!("{char_id} lost ship {ship_id}; now in capsule {capsule_id}");
    Some((destroyed, blob))
}

/// Settlement is already committed. Read the character again before loading
/// and publishing, since reconnect/boarding may have won during notification
/// delivery. An existing live capsule owns its motion and connection identity.
async fn publish_capsule(server:&Server,store:&Store,loss:&Loss,capsule_id:i64)->Result<(),StoreError> {
    // The capsule is put in space while the store is held, so no boarding
    // can come between the check and the publication.
    store.read(|db| {
        if !aboard(db,loss.char_id,capsule_id,loss.custody_system()) {return Ok(());}
        let capsule=db.table("entity").get_id(capsule_id).filter(|e| e.int("ownerID")==Some(loss.char_id)
            && e.int("locationID")==Some(loss.custody_system()) && e.int("typeID")==Some(TYPE_CAPSULE) && e.int("flag")==Some(FLAG_IN_SPACE)
            && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s| s!=0));
        let Some(row)=capsule else{return Ok(());};
        let Some(profile)=inflight::profile(db,&server.dogma,loss.char_id,capsule_id,&BTreeSet::new(),combat::now_ms())? else{return Ok(());};
        let pos=Vec3::new(row.float("x").unwrap_or(0.0),row.float("y").unwrap_or(0.0),row.float("z").unwrap_or(0.0));
        let mut w=server.world.lock();
        if w.parks.get(&loss.custody_system()).is_some_and(|p|p.ships.contains_key(&capsule_id)){return Ok(());}
        // Never send an old completion to a new connection. A reconnect loads
        // its authoritative capsule itself.
        let client=if w.clients_of(loss.char_id).contains(&loss.client_id){loss.client_id}else{0};
        let mut capsule=Ship::new(capsule_id,client,profile.stats,profile.pilot,pos,Vec3::ZERO);capsule.fight=Some(profile.fighter);
        if let Some(park)=w.parks.get_mut(&loss.custody_system()) {
            park.advance(crate::destiny::tick_now());park.ships.insert(capsule_id,capsule);
            let entries=space::add_ball_entries(park,capsule_id);space::broadcast(&w,loss.custody_system(),None,entries);
        }
        if client!=0 {w.push_move(client,Move::ShipLost {ship_id:capsule_id});}
        Ok(())
    })
}

struct InsurancePayout { owner: i64, payout: f64, balance: f64 }

/// Consuming the policy and crediting its wallet are part of the ship's loss
/// transaction.
fn claim_insurance(tx: &mut Tx<'_>, ship: i64, now: i64) -> Result<Option<InsurancePayout>, StoreError> {
    use crate::svc::wallet::{post, Entry, Transfer};
    let Some(r) = tx.table("insuranceContracts").find_one(&["shipID"], &[store::Value::Int(ship)]) else { return Ok(None); };
    let (contract, payout, owner) = (r.id, r.float("payout").unwrap_or(0.0), r.int("ownerID").unwrap_or(0));
    let (start, end) = (r.int("startDate").unwrap_or(0), r.int("endDate").unwrap_or(0));
    if end <= now {
        tx.delete("insuranceContracts", contract)?;
        return Ok(None);
    }
    if start > now || !payout.is_finite() || payout <= 0.0 { return Ok(None); }
    let arg = ship.to_string();
    let entry = Entry { ref_type: REF_INSURANCE.into(), payer: OWNER_SYSTEM, payee: owner, arg_id1: &arg, reason: "" };
    let balance = match post(tx, owner, payout, &entry) {
        Ok(balance) => balance,
        Err(Transfer::Db(e)) => return Err(e),
        Err(_) => { warn!("insurance for {ship}: no payable wallet for {owner}"); return Ok(None); }
    };
    tx.delete("insuranceContracts", contract)?;
    Ok(Some(InsurancePayout { owner, payout, balance }))
}
fn notify_insurance(server: &Server, loss: &Loss, paid: InsurancePayout) {
    let InsurancePayout { owner, payout, balance } = paid;
    info!("insurance paid {payout} ISK to {owner} for ship {}", loss.ship_id);
    let w = server.world.lock();
    let mut clients = w.clients_of(owner);
    clients.extend(w.corp_members(owner));
    w.notify(&clients, "OnAccountChange", Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]));
    let text = format!("Your insurance has paid out {} ISK for the loss of your ship.", crate::heartbeat::fmt_isk(payout));
    let (key, args) = crate::fight::notify(&text);
    w.notify(&clients, "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
}

// ---- the capsule --------------------------------------------------------------------------

/// Where a podded pilot wakes: the station chosen, else where they last
/// docked, else their corporation's; with its system, constellation and
/// region. None if that is no station.
fn wake_station(db: &Db, char_id: i64, chosen: Option<i64>) -> Option<(i64, i64, i64, i64)> {
    let c = db.table("character_").get_id(char_id)?;
    let station = chosen
        .or_else(|| c.int("stationID").filter(|s| *s != 0))
        .or_else(|| db.table("corporation").get_id(c.int("corporationID")?)?.int("stationID"))?;
    let s = db.table("staStations").get_id(station)?;
    Some((station, s.int("solarSystemID").unwrap_or(0), s.int("constellationID").unwrap_or(0), s.int("regionID").unwrap_or(0)))
}

async fn podded(server: &Server, store: &Store, loss: &Loss) -> Option<(Vec<String>, String)> {
    let (char_id, capsule_id) = (loss.char_id, loss.ship_id);
    // Err(None): the loss no longer applies, and nothing it did is kept.
    let result = store.write(|tx| -> Result<_, Option<StoreError>> {
        let mut txn = ItemTxn::new(tx);
        if !aboard(txn.db(),char_id,capsule_id,loss.custody_system()) {return Err(None);}
        let Some(hull)=txn.item(capsule_id)? else {return Err(None);};
        if hull.owner_id!=char_id || hull.type_id!=TYPE_CAPSULE || hull.location_id!=loss.custody_system()
            || hull.flag!=FLAG_IN_SPACE || hull.quantity!=1 || !hull.singleton {return Err(None);}
        let before = item_snapshot(txn.db(), loss);
        let mut bounty_balance = None;
        if let Some(killer) = loss.final_blow.filter(|id| *id >= 100_000_000 && *id != char_id) {
            if pay_player_bounty(txn.tx(), char_id, killer)? > 0.0 {
                let balance = txn.db().table("character_").get_id(killer).and_then(|c| c.float("balance")).unwrap_or(0.0);
                bounty_balance = Some((killer, balance));
            }
        }
        txn.destroy_tree(capsule_id,Some(loss.custody_system()),&[char_id])?;
        let implants = lose_implants(&mut txn, char_id)?;
        let skill_loss = lose_skill_points(&mut txn, char_id)?;
        // The clone's station, else where they last docked, else their corporation's.
        let chosen = crate::svc::medical::home_station(txn.db(), char_id)?;
        crate::svc::medical::replace_clone(txn.tx(), char_id, TYPE_CLONE_ALPHA, chosen)?;
        let Some((station, system, constellation, region)) = wake_station(txn.db(), char_id, chosen) else { return Err(None) };
        let pod = txn
            .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true })?;
        update_id(txn.tx(), "character_", char_id, &[
            ("shipID", store::Value::Int(pod)),
            ("stationID", store::Value::Int(station)),
            ("solarSystemID", store::Value::Int(system)),
            ("constellationID", store::Value::Int(constellation)),
            ("regionID", store::Value::Int(region)),
        ])?;
        update_id(txn.tx(), "entity", char_id, &[("locationID", store::Value::Int(station))])?;
        let blob = item_blob(txn.db(), &before);
        let report=report_text(txn.db(),loss);
        let record=stage_record(txn.tx(),loss,report,implants.clone(),blob.clone())?;
        let events = txn.events();
        let m = Move::Podded {
            station_id: station,
            solar_system_id: system,
            constellation_id: constellation,
            region_id: region,
            ship_id: Some(pod),
        };
        Ok((m, events, implants, skill_loss, bounty_balance, blob,record))
    });
    match result {
        Ok((m, events, implants, skill_loss, bounty_balance, blob,record)) => {
            info!("{char_id} podded; waking at {m:?}");
            items::notify(&server.world, loss.client_id, events);
            deliver_record(server,store,record).await;
            let w = server.world.lock();
            if let Some((killer, balance)) = bounty_balance {
                w.notify(&w.clients_of(killer), "OnAccountChange", Value::Tuple(vec![Value::bytes("cash"), Value::Int(killer), Value::Double(balance)]));
            }
            if let Some((skill, lost)) = skill_loss {
                let text = format!(
                    "Your clone could not hold everything you knew: you have lost {} skill points of {skill}. \
                     Upgrade your clone at a station's medical bay to keep more.",
                    crate::heartbeat::fmt_isk(lost)
                );
                let (key, args) = crate::fight::notify(&text);
                w.notify(&[loss.client_id], "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
            }
            w.push_move(loss.client_id, m);
            Some((implants, blob))
        }
        Err(None) => {
            error!("{char_id} podded with no station to wake in");
            None
        }
        Err(Some(e)) => {
            error!("podding {char_id}: {}", crate::logging::chain(&e));
            None
        }
    }
}

/// A player's bounty is claimed on capsule destruction, in the same transaction
/// as the loss. Clearing both the aggregate and contributions prevents repeats.
pub(crate) fn pay_player_bounty(tx: &mut Tx<'_>, victim: i64, killer: i64) -> Result<f64, StoreError> {
    use crate::svc::wallet::{post, Entry, Transfer};
    if victim == killer || killer < 100_000_000 { return Ok(0.0); }
    let people = tx.table("character_");
    if people.get_id(killer).is_none() { return Ok(0.0); }
    let bounty = people.get_id(victim).and_then(|r| r.float("bounty")).unwrap_or(0.0);
    if !bounty.is_finite() || bounty <= 0.0 { return Ok(0.0); }
    let arg = victim.to_string();
    let entry = Entry {ref_type:17,payer:1,payee:killer,arg_id1:&arg,reason:"Player bounty prize"};
    match post(tx,killer,bounty,&entry) {
        Ok(_) => {},
        Err(Transfer::Db(e)) => return Err(e),
        Err(_) => return Ok(0.0),
    }
    update_id(tx, "character_", victim, &[("bounty", store::Value::Float(0.0))])?;
    tx.delete_where("bounties", &["characterID"], &[store::Value::Int(victim)])?;
    Ok(bounty)
}

/// Destroys the implants in the pilot's head; returns their names.
fn lose_implants(txn: &mut ItemTxn<'_, '_>, char_id: i64) -> Result<Vec<String>, StoreError> {
    let implants: Vec<(i64, i64)> = txn
        .db()
        .table("entity")
        .find(&["locationID", "flag"], &[store::Value::Int(char_id), store::Value::Int(FLAG_IMPLANT)])
        .filter_map(|e| Some((e.int("itemID")?, e.int("typeID")?)))
        .collect();
    let mut names = Vec::new();
    for (id, type_id) in implants {
        // An implant of a type the static data does not know stays, as
        // joining on it left it out.
        let Some(name) = known_type_name(type_id) else { continue };
        txn.destroy_tree(id,Some(char_id),&[char_id])?;
        names.push(name);
    }
    Ok(names)
}

/// Skill points a skill of `rank * 250` time constant has at `level`.
pub(crate) fn points_at(time_constant: f64, level: i64) -> f64 {
    if level <= 0 { 0.0 } else { time_constant * 2f64.powf(2.5 * (level - 1) as f64) }
}

/// The level `points` reach.
fn level_of(time_constant: f64, points: f64) -> i64 {
    (1..=5).rev().find(|l| points + 1e-6 >= points_at(time_constant, *l)).unwrap_or(0)
}

/// What the clone keeps; anything over comes off the biggest skill. Returns
/// (skill, points lost) if anything was.
fn lose_skill_points(txn: &mut ItemTxn<'_, '_>, char_id: i64) -> Result<Option<(String, f64)>, StoreError> {
    let sde = crate::sde::get();
    let db = txn.db();
    let entity = db.table("entity");
    // The best clone the pilot owns that says what it keeps.
    let clones = sde.types_in_group(GROUP_CLONE as i64);
    let saved: Option<f64> = entity
        .find(&["ownerID"], &[store::Value::Int(char_id)])
        .filter_map(|e| e.int("typeID"))
        .filter(|t| clones.contains(&(*t as i32)))
        .collect::<BTreeSet<i64>>()
        .into_iter()
        .filter_map(|t| sde.type_attribute(t, attr::SKILL_POINTS_SAVED))
        .max_by(f64::total_cmp);
    let saved = saved.or_else(|| sde.type_attribute(TYPE_CLONE_ALPHA, attr::SKILL_POINTS_SAVED)).unwrap_or(f64::MAX);
    let attributes = db.table("entity_attributes");
    let skills: Vec<(i64, i64, String, f64, f64)> = entity
        .find(&["locationID"], &[store::Value::Int(char_id)])
        .filter(|e| matches!(e.int("flag"), Some(FLAG_SKILL | FLAG_SKILL_IN_TRAINING)))
        .filter_map(|e| {
            let id = e.int("itemID")?;
            let type_id = e.int("typeID")?;
            let name = known_type_name(type_id)?;
            let stc = sde.type_attribute(type_id, ATTR_SKILL_TIME_CONSTANT).unwrap_or(250.0);
            let points = attributes
                .get(&[store::Value::Int(id), store::Value::Int(ATTR_SKILL_POINTS.into())])
                .and_then(|a| a.float("valueFloat").or_else(|| a.float("valueInt")))
                .unwrap_or(0.0);
            Some((id, type_id, name, points, stc))
        })
        .collect();
    let total: f64 = skills.iter().map(|s| s.3).sum();
    if total <= saved {
        return Ok(None);
    }
    let Some(biggest) = skills.iter().max_by(|a, b| a.3.total_cmp(&b.3)) else { return Ok(None) };
    let (id, type_id, name, points, stc) = biggest.clone();
    let lost = (total - saved).min(points);
    let left = points - lost;
    for (a, value) in [(ATTR_SKILL_POINTS, left), (ATTR_SKILL_LEVEL, level_of(stc, left) as f64)] {
        txn.tx().upsert(
            "entity_attributes",
            &["itemID", "attributeID"],
            &[
                ("itemID", store::Value::Int(id)),
                ("attributeID", store::Value::Int(a.into())),
                ("valueInt", store::Value::Null),
                ("valueFloat", store::Value::Float(value)),
            ],
            &[("valueInt", store::Value::Null), ("valueFloat", store::Value::Float(value))],
        )?;
    }
    crate::svc::skills::history::record(txn.tx(), char_id, type_id, crate::svc::skills::history::CLONE_PENALTY, crate::bulkdata::filetime_now(), left)?;
    info!("{char_id} lost {lost} skill points of {name} (clone keeps {saved}, had {total})");
    Ok(Some((name, lost)))
}

// ---- the kill report ------------------------------------------------------------------------

struct Named {
    name: String,
    corp_id: i64,
    corp: String,
    alliance_id: i64,
    alliance: String,
    security: f64,
}

fn who(db: &Db, char_id: i64) -> Named {
    let Some(c) = db.table("character_").get_id(char_id) else {
        return Named { name: format!("character {char_id}"), corp_id: 0, corp: String::new(), alliance_id: 0, alliance: String::new(), security: 0.0 };
    };
    let corp = c.int("corporationID").and_then(|id| db.table("corporation").get_id(id));
    let alliance = corp.and_then(|co| db.table("alliances").get_id(co.int("allianceID")?));
    Named {
        name: c.text("characterName").unwrap_or_default().to_string(),
        corp_id: c.int("corporationID").unwrap_or(0),
        corp: corp.and_then(|co| co.text("corporationName")).unwrap_or_default().to_string(),
        alliance_id: corp.and_then(|co| co.int("allianceID")).unwrap_or(0),
        alliance: alliance.and_then(|al| al.text("allianceName")).unwrap_or_default().to_string(),
        security: c.float("securityRating").unwrap_or(0.0),
    }
}

/// A type's name, if the static data knows the type.
fn known_type_name(type_id: i64) -> Option<String> {
    crate::sde::get().type_(type_id).map(|t| t.name.to_string())
}

fn type_name(type_id: i64) -> String {
    known_type_name(type_id).unwrap_or_else(|| format!("type {type_id}"))
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "NONE" } else { s }
}

/// The killmail, up to its "Destroyed items" list (which only the loss knows).
fn report_text(db: &Db, loss: &Loss) -> String {
    let victim = who(db, loss.char_id);
    let (system, security) = db
        .table("mapSolarSystems")
        .get_id(loss.system)
        .map_or((String::new(), 0.0), |r| (r.text("solarSystemName").unwrap_or_default().to_string(), r.float("security").unwrap_or(0.0)));
    let mut text = format!(
        "{}\n\nVictim: {}\nAlliance: {}\nCorp: {}\nDestroyed: {}\nSystem: {}\nSecurity: {:.1}\n\nInvolved parties:\n",
        killmail_time(),
        victim.name,
        or_none(&victim.alliance),
        victim.corp,
        type_name(loss.ship_type),
        system,
        security
    );
    let mut attackers = loss.attackers.clone();
    attackers.sort_by(|a, b| b.damage.total_cmp(&a.damage));
    for a in &attackers {
        let final_blow = if Some(a.who) == loss.final_blow { " (laid the final blow)" } else { "" };
        if a.who > 0 {
            let p = who(db, a.who);
            text.push_str(&format!(
                "\nName: {}{final_blow}\nSecurity: {:.1}\nAlliance: {}\nCorp: {}\nShip: {}\nWeapon: {}\n",
                p.name,
                p.security,
                or_none(&p.alliance),
                p.corp,
                type_name(a.ship_type),
                type_name(a.weapon_type)
            ));
        } else {
            text.push_str(&format!("\nName: {}{final_blow}\n", type_name(-a.who)));
        }
    }
    text
}
fn killmail_time() -> String {
    // FILETIME to a calendar date, without a date library: days since 1601.
    let secs = crate::bulkdata::filetime_now() / 10_000_000 - 11_644_473_600;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}.{m:02}.{d:02} {:02}:{:02}", rest / 3_600, rest % 3_600 / 60)
}

/// Howard Hinnant's days-to-civil, for 1970-based day numbers.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

struct RecordedLoss { subject: String, text: String, recipients: Vec<i64> }

/// Callers that settle custody can include the kill record in that transaction.
fn stage_record(tx: &mut Tx<'_>, loss: &Loss, mut text: String, destroyed: Vec<String>, items: String)
    -> Result<RecordedLoss,StoreError> {
    text.push_str("\nDestroyed items:\n\n");
    for item in &destroyed {
        text.push_str(item);
        text.push('\n');
    }
    let victim = who(tx.db(), loss.char_id);
    let total: f64 = loss.attackers.iter().map(|a| a.damage).sum();
    let ship_name = type_name(loss.ship_type);
    let subject = format!("Kill: {} ({ship_name})", victim.name);
        let final_char = loss.final_blow.filter(|w| *w > 0).unwrap_or(0);
        let kill_id = tx.nextval("killLog_killID_seq")?;
        tx.insert(
            "killLog",
            &[
                ("killID", store::Value::Int(kill_id)),
                ("killTime", store::Value::Int(crate::bulkdata::filetime_now())),
                ("victimCharID", store::Value::Int(loss.char_id)),
                ("victimCorpID", store::Value::Int(victim.corp_id)),
                ("victimAllianceID", store::Value::Int(victim.alliance_id)),
                ("victimShipTypeID", store::Value::Int(loss.ship_type)),
                ("victimItemID", store::Value::Int(loss.ship_id)),
                ("solarSystemID", store::Value::Int(loss.system)),
                ("locationID", store::Value::Int(loss.system)),
                ("finalBlowID", store::Value::Int(loss.final_blow.unwrap_or(0))),
                ("totalDamage", store::Value::Float(total)),
                ("isPodKill", store::Value::Int(i64::from(loss.capsule && !loss.unpiloted))),
                ("killBlob", store::Value::Text(items.into())),
            ],
        )?;
        for a in loss.attackers.iter().filter(|a| a.who != 0) {
            let p = who(tx.db(), a.who);
            tx.insert(
                "killLogParticipants",
                &[
                    ("killID", store::Value::Int(kill_id)),
                    ("ownerID", store::Value::Int(a.who)),
                    ("corporationID", store::Value::Int(p.corp_id)),
                    ("allianceID", store::Value::Int(p.alliance_id)),
                    ("shipTypeID", store::Value::Int(a.ship_type)),
                    ("weaponTypeID", store::Value::Int(a.weapon_type)),
                    ("damageDone", store::Value::Float(a.damage)),
                    ("finalBlow", store::Value::Int(i64::from(Some(a.who) == loss.final_blow))),
                    ("securityStatus", if a.who > 0 { store::Value::Float(p.security) } else { store::Value::Null }),
                ],
            )?;
        }
    tracing::debug!("staged kill {kill_id}: {} lost a {ship_name}", victim.name);
    let mut recipients = vec![loss.char_id];
    if final_char != 0 && final_char != loss.char_id {
        recipients.push(final_char);
    }
    Ok(RecordedLoss {subject,text,recipients})
}

async fn deliver_record(server:&Server,store:&Store,record:RecordedLoss) {
    for to in record.recipients {
        mail(server,store,to,&record.subject,&record.text).await;
    }
}

/// The kill log rows, and the killmail to the victim and the final blow.
async fn record(server: &Server, store: &Store, loss: &Loss, text: String, destroyed: Vec<String>, items: String) {
    match store.write(|tx| stage_record(tx,loss,text,destroyed,items)) {
        Ok(record)=>deliver_record(server,store,record).await,
        Err(e)=>error!("logging the loss of {}: {}",loss.ship_id,crate::logging::chain(&e)),
    }
}

/// An EVE mail from the system to `to`.
async fn mail(server: &Server, store: &Store, to: i64, subject: &str, body: &str) {
    let body = body.replace('\n', "<br>");
    if let Err(e) = store.write(|tx| crate::svc::chat::deliver(tx, &server.world, OWNER_SYSTEM, &[to], subject, &body)) {
        error!("mailing {to}: {}", crate::logging::chain(&e));
    }
}
