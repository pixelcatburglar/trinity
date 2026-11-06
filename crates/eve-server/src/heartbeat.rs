//! The server's pulse: four times a second, every solar system with a player
//! in space moves on (see `fight`). The pulse only decides, under the world
//! lock; what it decided that needs the store is done after the lock is
//! released: charges used, ore mined, bounties paid, ships and capsules
//! lost.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use eve_marshal::Value;
use tracing::{error, info, warn};

use crate::ballpark::Vec3;
use crate::combat;
use crate::conn::Server;
use crate::items::{self, ItemTxn, NewItem};
use crate::store::{Store, StoreError, Value as StoreValue};
use crate::svc::{inflight, space};
use crate::world::{World, WorldState};

const PULSE_MS: u64 = 250;
/// Pulses between looks at slow corporate business (alliance applications
/// coming due): once a minute.
const AFFAIRS_EVERY: u64 = 60_000 / PULSE_MS;
const FLAG_CARGO: i64 = 5;
/// market_refTypes: "Bounty Prize".
const REF_BOUNTY_PRIZE: i32 = 17;
const OWNER_CONCORD: i64 = 1000125;

/// What a pulse decided, to be told or done once the world lock is released.
#[derive(Default)]
pub struct Out {
    /// Destiny entries for everyone in a system, in order.
    destiny: Vec<(i64, Value)>,
    notes: Vec<(i64, &'static str, Value)>,
    pub jobs: Vec<Job>,
    /// Ships whose hit points changed this pulse.
    hurt: BTreeSet<i64>,
    /// Ships whose capacitor changed this pulse.
    drained: BTreeSet<i64>,
}

impl Out {
    pub fn destiny(&mut self, system: i64, entry: Value) {
        self.destiny.push((system, entry));
    }

    pub fn note(&mut self, client_id: i64, event: &'static str, args: Value) {
        self.notes.push((client_id, event, args));
    }

    pub fn hurt(&mut self, ship_id: i64) {
        self.hurt.insert(ship_id);
    }

    pub fn drained(&mut self, ship_id: i64) {
        self.drained.insert(ship_id);
    }

    /// The hurt and drained marks for ships `here` says belong to the caller.
    pub fn take_marks(&mut self, here: impl Fn(i64) -> bool) -> (BTreeSet<i64>, BTreeSet<i64>) {
        let (hurt, rest): (BTreeSet<i64>, BTreeSet<i64>) = std::mem::take(&mut self.hurt).into_iter().partition(|id| here(*id));
        self.hurt = rest;
        let (drained, rest): (BTreeSet<i64>, BTreeSet<i64>) =
            std::mem::take(&mut self.drained).into_iter().partition(|id| here(*id));
        self.drained = rest;
        (hurt, drained)
    }

    /// Sends what was decided; returns the jobs left to do.
    pub fn deliver(self, world: &WorldState) -> Vec<Job> {
        let mut by_system: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        for (system, e) in self.destiny {
            by_system.entry(system).or_default().push(e);
        }
        for (system, entries) in by_system {
            space::broadcast(world, system, None, entries);
        }
        for (client, event, args) in self.notes {
            world.notify(&[client], event, args);
        }
        self.jobs
    }
}

/// Work a pulse leaves for after the lock.
#[derive(Debug, Clone)]
pub enum Job {
    NpcKill { system:i64 },
    /// A charge stack burned down to `quantity`.
    Ammo { client_id: i64, charge_id: i64, quantity: i64 },
    /// Ore into `ship_id`'s hold, mined by `module_id` on `miner_id` (the ship, or a drone).
    Ore { client_id: i64, char_id: i64, ship_id: i64, miner_id: i64, module_id: i64, system: i64, type_id: i64, quantity: i64 },
    /// A rat's bounty, and what killing it does for security status: (share
    /// of the way to 10, highest it raises it to).
    Bounty { client_id: Option<i64>, char_id: i64, amount: f64, npc_type: i64, security: (f64, f64) },
    /// A module that changes attributes stopped.
    Refit { client_id: i64, char_id: i64, ship_id: i64, system: i64 },
    /// A player's ship (or capsule) was destroyed.
    Lost(Box<crate::losses::Loss>),
    /// A bot's ship was destroyed.
    BotLost(Box<crate::losses::Loss>),
    /// Something was killed with loot on it.
    Loot { system: i64, pos: Vec3, owner_id: i64, stash: Vec<crate::loot::Stash> },
    /// A can's time ran out.
    Scrap { can_id: i64 },
    /// A drone flew home: back into its ship's drone bay.
    DroneHome { client_id: i64, drone_id: i64, ship_id: i64 },
    /// A drone was destroyed.
    DroneLost { client_id: Option<i64>, drone_id: i64, system:i64, owner:i64, generation:Option<i64> },
    /// The last rat of a mission site died.
    SiteCleared { mission_id: i64, char_id: i64 },
    /// A crime, or a kill, changed a pilot's security status by this much.
    Security { char_id: i64, delta: f64 },
    /// A starbase structure's health after a fight.
    StructureDamage { id: i64, shield: f64, armor: f64, hull: f64 },
    /// A tower fell below a quarter shield and may go reinforced.
    Reinforce { system: i64, tower: i64 },
    /// A battery fired a round from its bay.
    StructureAmmo { item_id: i64, quantity: i64 },
    /// A starbase structure was destroyed.
    StructureLost { id: i64, tower: bool },
    /// A starbase structure finished a timed step.
    Structure { id: i64, state: i16, stamp: Option<i64>, tower: Option<i64> },
    /// A cloaking device stopped: the ship is seen again.
    Uncloak { system: i64, ship_id: i64 },
    /// Available power or a new scramble changed the predicted warp at entry.
    WarpResync { system: i64 },
    /// The last room of a deadspace complex was cleared.
    ComplexCleared { complex_id: i64 },
}

pub async fn run(server: Arc<Server>) {
    let (queued,waiting)=tokio::sync::mpsc::unbounded_channel();
    // Join two scoped futures: dropping run cancels both; the jobs a pulse
    // leaves never hold up the simulation and there is only one persistence
    // consumer.
    tokio::join!(simulation(&server,queued),persistence(&server,waiting));
}

async fn simulation(server:&Server,queued:tokio::sync::mpsc::UnboundedSender<Job>) {
    info!("heartbeat every {PULSE_MS} ms");
    let mut interval = tokio::time::interval(Duration::from_millis(PULSE_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        for job in pulse(&server.world, combat::now_ms()) {
            match job {
                // Decloaking changes the live ballpark, not the database.
                Job::Uncloak{system,ship_id}=>crate::cloak::uncloaked(&mut server.world.lock(),system,ship_id),
                job=>if queued.send(job).is_err() {return;},
            }
        }
    }
}

async fn persistence(server:&Server,mut waiting:tokio::sync::mpsc::UnboundedReceiver<Job>) {
    let store=&server.store;
    let mut interval=tokio::time::interval(Duration::from_millis(PULSE_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pulses=0u64;
    loop {
        interval.tick().await;
        pulses=pulses.wrapping_add(1);
        // Bound each batch so a continuously producing simulation cannot
        // starve checkpoints, journal repair, or other periodic work.
        for _ in 0..128 {
            let job=match waiting.try_recv() {
                Ok(job)=>job,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)=>break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)=>return,
            };
            perform(server, job).await;
        }
        store.read(|db| crate::svc::fleet::update_bonuses(&server.world, db, &server.dogma));
        if pulses%4==0 {crate::svc::boosters::pulse(server,store).await;}
        if pulses % 20 == 1 {
            crate::drones::checkpoint(&server.world, store).await;
            crate::space_state::checkpoint(&server.world, store).await;
            crate::svc::maintenance::retry_repairs(&server.world, store).await;
            crate::losses::pending::retry(server, store).await;
            crate::losses::journal::retry(server, store).await;
            crate::travel::retry(server, store).await;
            crate::svc::jumps::portal::retry(server, store).await;
            crate::conn::logout::retry(server,store).await;
            if let Err(e) = store.read(|db| crate::svc::fleet::refresh_skills(&server.world, db)) {
                warn!("refreshing fleet skills: {e}");
            }
        }
        if pulses % AFFAIRS_EVERY == 1 {
            let session = crate::session::Session::default();
            let ctx = crate::services::Ctx { store, dogma: &server.dogma,
                bulk: &server.bulk, world: &server.world, node_id: server.node_id, client_id: 0, session: &session };
            crate::svc::trade::expire(&ctx).await;
            crate::svc::alliance::take_effect(&server.world, store, server.alliance_join_delay_secs).await;
            crate::svc::alliance::bill_alliances(store).await;
            crate::svc::war::bill_wars(&server.world, store).await;
            crate::svc::starbase::burn_fuel(&server.world, store).await;
            crate::svc::starbase::sovereignty(store, server.sov_claim_delay_secs).await;
            crate::svc::votes::close_votes(&server.world, store).await;
            crate::complexes::seed(store).await;
            crate::svc::outposts::finish(&server.world, store).await;
            store.read(|db| crate::guard::reload(&server.world, db));
            store.read(crate::guard::refresh_standings);
        }
    }
}

/// One pulse of every occupied system.
fn pulse(world: &World, now: i64) -> Vec<Job> {
    let mut w = world.lock();
    crate::drones::reconcile(&mut w);
    let mut out = Out::default();
    let tick = crate::destiny::tick_now();
    for park in w.parks.values_mut() {
        crate::pos::step(park, &mut out);
        if park.players().next().is_none() {
            continue;
        }
        park.advance(tick);
        warp_changes(park, &mut out);
        crate::cloak::proximity(park, now, &mut out);
        crate::complexes::step(park, now, &mut out);
        crate::fight::step(park, now, &mut out);
        for e in crate::beacons::expire(park, now) {
            out.destiny(park.solar_system_id, e);
        }
    }
    crate::svc::jumps::orphaned_cynos(&mut w);
    crate::svc::jumps::close_bridges(&mut w, now);
    crate::svc::fleet::pulse(&mut w, now);
    let mut jobs = std::mem::take(&mut w.pending_jobs);
    jobs.extend(out.deliver(&w));
    // The loss is handled after the lock is released. Quarantine a confirmed
    // death while still holding the simulation lock, even if its job has not
    // started, so a reconnecting pilot cannot reload the destroyed hull.
    for job in &jobs {
        if let Job::Lost(loss)=job {
            w.pending_ship_losses.entry(loss.ship_id).or_insert_with(||(**loss).clone());
        }
    }
    crate::svc::space::refresh_views(&w);
    jobs
}

fn warp_changes(park: &mut crate::ballpark::Park, out: &mut Out) {
    let mut resync = false;
    for (ship_id, change) in std::mem::take(&mut park.warp_events) {
        let Some(ship) = park.ships.get(&ship_id) else { continue; };
        match change {
            crate::ballpark::WarpChange::Entered { cost, limited } => {
                if cost > 0.0 { out.drained(ship_id); }
                if limited {
                    resync = true;
                    if ship.is_player() {
                        out.note(ship.client_id,"OnRemoteMessage",Value::Tuple(vec![Value::bytes("WarpingWithAvailablePower"),Value::Dict(vec![])]));
                    }
                }
            }
            crate::ballpark::WarpChange::Stopped { scrambled } => {
                resync = true;
                if ship.is_player() {
                    let (key,args) = if scrambled { ("WarpScrambled",Value::Dict(vec![])) } else {
                        ("CustomNotify",Value::Dict(vec![(Value::bytes("notify"),Value::text("Your ship cannot enter warp with its current power or destination."))]))
                    };
                    out.note(ship.client_id,"OnRemoteMessage",Value::Tuple(vec![Value::bytes(key),args]));
                }
            }
        }
    }
    if resync { out.jobs.push(Job::WarpResync { system:park.solar_system_id }); }
}

async fn perform(server: &Server, job: Job) {
    let store = &server.store;
    match job {
        Job::NpcKill { system } => {
            if let Err(e) = crate::svc::map::record_activity(store,system,2).await { warn!("recording NPC kill: {e}"); }
        }
        Job::Ammo { client_id, charge_id, quantity } => ammo(server, client_id, charge_id, quantity).await,
        Job::Ore { client_id, char_id, ship_id, miner_id, module_id, system, type_id, quantity } => {
            ore(server, client_id, char_id, (ship_id, miner_id, module_id), system, type_id, quantity).await
        }
        Job::Bounty { client_id, char_id, amount, npc_type, security } => {
            bounty(server, client_id, char_id, amount, npc_type).await;
            raise_security(store, char_id, security).await;
        }
        Job::Refit { client_id, char_id, ship_id, system } => {
            store.read(|db| inflight::refit(db, &server.dogma, &server.world, client_id, char_id, ship_id, system))
        }
        Job::Lost(loss) => crate::losses::handle(server, store, *loss).await,
        Job::BotLost(loss) => crate::losses::bot_lost(server, store, *loss).await,
        Job::Loot { system, pos, owner_id, stash } => crate::loot::drop_can(store, &server.world, system, pos, owner_id, stash).await,
        Job::Scrap { can_id } => crate::loot::scrap(store, can_id).await,
        Job::DroneHome { client_id, drone_id, ship_id } => crate::drones::home(server, store, client_id, drone_id, ship_id).await,
        Job::DroneLost { client_id, drone_id, system, owner, generation } => crate::drones::lost(server, store, client_id, drone_id, system, owner, generation).await,
        Job::Security { char_id, delta } => change_security(store, char_id, delta).await,
        Job::SiteCleared { mission_id, char_id } => crate::svc::agent_missions::site_cleared(&server.world, store, mission_id, char_id).await,
        Job::Structure { id, state, stamp, tower } => save_structure(store, id, state, stamp, tower).await,
        Job::StructureDamage { id, shield, armor, hull } => crate::svc::starbase::save_damage(store, id, shield, armor, hull).await,
        Job::Reinforce { system, tower } => crate::svc::starbase::reinforce(&server.world, store, system, tower).await,
        Job::StructureLost { id, tower } => crate::svc::starbase::lost(store, id, tower).await,
        Job::StructureAmmo { item_id, quantity } => crate::guard::save_ammo(store, item_id, quantity).await,
        Job::Uncloak { system, ship_id } => crate::cloak::uncloaked(&mut server.world.lock(), system, ship_id),
        Job::WarpResync { system } => store.read(|db| space::resync_after_warp_limit(db, &server.world, system)),
        Job::ComplexCleared { complex_id } => crate::complexes::cleared(store, complex_id).await,
    }
}

async fn ammo(server: &Server, client_id: i64, charge_id: i64, quantity: i64) {
    let result = server.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        txn.set_quantity(charge_id, quantity)?;
        Ok::<_, StoreError>(txn.events())
    });
    match result {
        Ok(events) => items::notify(&server.world, client_id, events),
        Err(e) => error!("using charge {charge_id}: {}", crate::logging::chain(&e)),
    }
}

/// Mined ore goes into the cargo hold, as much as fits; a full hold stops
/// the laser.
#[allow(clippy::too_many_arguments)]
async fn ore(server: &Server, client_id: i64, char_id: i64, (ship_id, miner_id, module_id): (i64, i64, i64), system: i64, type_id: i64, quantity: i64) {
    let types = crate::sde::table("invTypes");
    // The hull's type and what its hold has, each content's type and
    // quantity in itemID order.
    let hold = server.store.read(|db| {
        let entity = db.table("entity");
        let hull_type = entity.get_id(ship_id)?.int("typeID").unwrap_or(0);
        let mut contents: Vec<(i64, i64, i64)> = entity
            .find(&["locationID", "flag"], &[StoreValue::Int(ship_id), StoreValue::Int(FLAG_CARGO)])
            .map(|e| (e.int("itemID").unwrap_or(0), e.int("typeID").unwrap_or(0), e.int("quantity").unwrap_or(0)))
            .collect();
        contents.sort_unstable_by_key(|(id, _, _)| *id);
        Some((hull_type, contents))
    });
    let Some((hull_type, contents)) = hold else {
        error!("mining {quantity} of {type_id} into {ship_id}: the ship is gone");
        return;
    };
    // A hull of a type the static data does not know holds nothing;
    // contents of a type it does not know take no room.
    let capacity = types.get("typeID", hull_type).map(|t| t.float("capacity").unwrap_or(0.0));
    let used: f64 = contents
        .iter()
        .filter_map(|(_, t, q)| Some(*q as f64 * types.get("typeID", *t)?.float("volume").unwrap_or(0.0)))
        .sum();
    let room = capacity.map_or(0.0, |c| c - used);
    let volume = types.get("typeID", type_id).map_or(1.0, |t| t.float("volume").unwrap_or(1.0));
    let fits = ((room / volume.max(1e-6)).floor() as i64).clamp(0, quantity);
    let result = if fits == 0 {
        Ok(Vec::new())
    } else {
        server.store.write(|tx| {
            let mut txn = ItemTxn::new(tx);
            txn.stack_onto(NewItem { type_id, owner_id: char_id, location_id: ship_id, flag: FLAG_CARGO, quantity: fits, singleton: false })?;
            Ok::<_, StoreError>(txn.events())
        })
    };
    match result {
        Ok(events) => {
            items::notify(&server.world, client_id, events);
            if fits < quantity {
                let mut w = server.world.lock();
                let mut out = Out::default();
                if let Some(park) = w.parks.get_mut(&system) {
                    let message = crate::fight::notify("Your cargo hold is full; the mining laser shuts down.");
                    crate::fight::stop(park, miner_id, module_id, combat::now_ms(), &mut out, Some(message));
                }
                out.deliver(&w);
            }
        }
        Err(e) => error!("mining {quantity} of {type_id} into {ship_id}: {}", crate::logging::chain(&e)),
    }
}

/// A payment the wallet refused, as the store write gives it up.
struct Unpaid(crate::svc::wallet::Transfer);

impl From<StoreError> for Unpaid {
    fn from(e: StoreError) -> Self {
        Unpaid(crate::svc::wallet::Transfer::Db(e))
    }
}

async fn bounty(server: &Server, client_id: Option<i64>, char_id: i64, amount: f64, npc_type: i64) {
    use crate::svc::wallet::{post, Entry, Transfer};
    let arg = npc_type.to_string();
    let result = server
        .store
        .write(|tx| {
            let entry = Entry { ref_type: REF_BOUNTY_PRIZE, payer: OWNER_CONCORD, payee: char_id, arg_id1: &arg, reason: "" };
            post(tx, char_id, amount, &entry).map_err(Unpaid)
        })
        .map_err(|Unpaid(t)| t);
    match result {
        Ok(balance) => {
            info!("bounty of {amount} ISK to {char_id} for a {npc_type}");
            if let Some(client) = client_id {
                let w = server.world.lock();
                let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(char_id), Value::Double(balance)]);
                w.notify(&[client], "OnAccountChange", cash);
                let text = format!("A bounty of {} ISK has been paid into your wallet.", fmt_isk(amount));
                let (key, args) = crate::fight::notify(&text);
                w.notify(&[client], "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
            }
        }
        Err(Transfer::Db(e)) => error!("paying a bounty to {char_id}: {}", crate::logging::chain(&e)),
        Err(_) => warn!("bounty for {char_id}: no wallet"),
    }
}

/// A pirate killed: security status rises by a share of the way to 10, but
/// no killing of that rat raises it past its maximum.
async fn raise_security(store: &Store, char_id: i64, (bonus, max_gain): (f64, f64)) {
    if bonus <= 0.0 {
        return;
    }
    let result = store.write(|tx| {
        let Some(c) = tx.table("character_").get_id(char_id) else { return Ok(()) };
        let (row, rating) = (c.id, c.float("securityRating").unwrap_or(0.0));
        if rating < max_gain {
            tx.update("character_", row, &[("securityRating", StoreValue::Float(max_gain.min(rating + (10.0 - rating) * bonus)))])?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = result {
        error!("raising the security status of {char_id}: {}", crate::logging::chain(&e));
    }
}

async fn save_structure(store: &Store, id: i64, state: i16, stamp: Option<i64>, tower: Option<i64>) {
    let result = store.write(|tx| {
        let Some(row) = tx.table("posStructures").get_id(id).map(|r| r.id) else { return Ok(()) };
        let or_null = |v: Option<i64>| v.map_or(StoreValue::Null, StoreValue::Int);
        tx.update("posStructures", row, &[("state", StoreValue::Int(i64::from(state))), ("stateTimestamp", or_null(stamp)), ("controlTowerID", or_null(tower))])
    });
    if let Err(e) = result {
        error!("saving structure {id}: {}", crate::logging::chain(&e));
    }
}

async fn change_security(store: &Store, char_id: i64, delta: f64) {
    let result = store.write(|tx| {
        let Some(c) = tx.table("character_").get_id(char_id) else { return Ok(None) };
        let (row, rating) = (c.id, c.float("securityRating").unwrap_or(0.0));
        let rating = (rating + delta).min(10.0).max(-10.0);
        tx.update("character_", row, &[("securityRating", StoreValue::Float(rating))])?;
        Ok::<_, StoreError>(Some(rating))
    });
    match result {
        Ok(Some(rating)) => info!("security status of {char_id} {delta:+.3}, now {rating:.2}"),
        Ok(None) => {}
        Err(e) => error!("changing the security status of {char_id}: {}", crate::logging::chain(&e)),
    }
}

pub fn fmt_isk(amount: f64) -> String {
    let whole = amount.round() as i64;
    let digits = whole.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
