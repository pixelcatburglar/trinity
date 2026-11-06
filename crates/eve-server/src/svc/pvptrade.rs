//! Player-to-player trade (pvptradesvc.py, pvptradewnd.py), and the CEO
//! trade in which one side of a war surrenders.
//!
//! Two pilots docked in the same station open a trade: trademgr.InitiateTrade
//! makes a trade container (a real item in the station, owned by EVE System,
//! so neither hangar lists it) and hands both a bound session on it. What a
//! pilot offers moves into the container, still theirs; any change to either
//! offer clears both acceptances. When both accept, what each offered goes
//! to the other's hangar, the money offered changes wallets, and for a CEO
//! trade the war ends. Either side can abort, and everything goes home.
//!
//! The trades under way live in the world (`WorldState::trades`); a
//! container left over from a restart is emptied back into its owners'
//! hangars when the server starts ([`recover`]).

use eve_marshal::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{error, info};

use crate::items::{self, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError};
use crate::svc::bound;
use crate::svc::inventory::{self, Container, FLAG_HANGAR};
use crate::svc::membership::{db_failed, message, Refused};
use crate::svc::rows::key_val;
use crate::svc::wallet::{self, Entry};
use crate::world::World;

const SESSION: &str = "tradeSession";
const TYPE_CARGO_CONTAINER: i64 = 23;
const OWNER_SYSTEM: i64 = 1;
/// The item name that marks a trade container, for [`recover`].
const CONTAINER_NAME: &str = "Trade";
/// market_refTypes: Player Trading.
const REF_PLAYER_TRADING: i32 = 1;

/// One trade under way.
#[derive(Debug, Clone)]
pub struct Trade {
    /// Serialises offer changes, inventory withdrawals, acceptance and abort.
    pub gate: Arc<tokio::sync::Mutex<()>>,
    pub container: i64,
    pub station: i64,
    pub traders: [i64; 2],
    pub accepted: [bool; 2],
    pub money: [f64; 2],
    /// For a CEO trade: the war it ends.
    pub war: Option<i64>,
}

impl Trade {
    fn side(&self, char_id: i64) -> Option<usize> {
        self.traders.iter().position(|t| *t == char_id)
    }
}

/// Why a change to a trade did not happen: a refusal, or a failed change.
enum Failed {
    Refused(Outcome),
    Db(StoreError),
}

impl From<StoreError> for Failed {
    fn from(e: StoreError) -> Self {
        Failed::Db(e)
    }
}

impl From<Outcome> for Failed {
    fn from(refusal: Outcome) -> Self {
        Failed::Refused(refusal)
    }
}

impl Failed {
    /// The refusal to answer with; a failed change while doing `what`.
    fn refusal(self, what: &str) -> Outcome {
        match self {
            Failed::Refused(refusal) => refusal,
            Failed::Db(e) => db_failed(what, e),
        }
    }
}

/// trademgr.InitiateTrade(charID, warID=None).
pub async fn trademgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "InitiateTrade" {
        return None;
    }
    let war = call.arg_i64(1);
    Some(initiate(ctx, call.arg_id(0), war).await.map_or_else(|refusal| refusal, Outcome::Value))
}

async fn initiate(ctx: &Ctx<'_>, other: i64, war: Option<i64>) -> Refused<Value> {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(message(if war.is_some() { "MustBeInStationToNegotiate" } else { "OtherCharNotAtStation" }));
    };
    let other_name = ctx.store.read(|db| name_of(db, other));
    if other == me {
        return Err(Outcome::custom("You cannot trade with yourself."));
    }
    {
        let w = ctx.world.lock();
        if w.station_of(other) != Some(station) {
            return Err(Outcome::UserError {
                msg: "OtherCharNotAtStation".into(),
                args: vec![("name".into(), Value::text(&other_name))],
            });
        }
        if w.trades.values().any(|t| t.side(me).is_some() || t.side(other).is_some()) {
            return Err(Outcome::custom("One of you is already trading."));
        }
    }
    if let Some(war) = war {
        ctx.store.read(|db| may_surrender(db, war, me, other))?;
    }

    let result = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let container = txn
            .create(NewItem { type_id: TYPE_CARGO_CONTAINER, owner_id: OWNER_SYSTEM, location_id: station, flag: 0, quantity: 1, singleton: true })?;
        if let Some(row) = txn.db().table("entity").get_id(container).map(|e| e.id) {
            txn.tx().update("entity", row, &[("itemName", store::Value::Text(CONTAINER_NAME.into()))])?;
        }
        Ok::<_, StoreError>(container)
    });
    let container = result.map_err(|e| db_failed("opening the trade", e))?;
    let trade = Trade { gate: Arc::new(tokio::sync::Mutex::new(())), container, station, traders: [me, other], accepted: [false; 2], money: [0.0; 2], war };
    let session = bound::object_ref(ctx.node_id, SESSION, &[container]);
    let registered = {
        let mut w = ctx.world.lock();
        // Both participants may initiate at once; the first to get here opens
        // the trade.
        if w.station_of(me)!=Some(station) || w.station_of(other)!=Some(station) || w.trades.values().any(|t|t.side(me).is_some() || t.side(other).is_some()) { false } else {
            w.trades.insert(container, trade);
            let event = Value::Tuple(vec![Value::bytes("Initiate"), Value::Int(me), session.clone()]);
            w.notify(&w.clients_of(other), "OnTrade", event);
            true
        }
    };
    if !registered {
        ctx.store.write(|tx| match tx.table("entity").get_id(container).map(|e| e.id) {
            Some(row) => tx.delete("entity", row),
            None => Ok(()),
        }).map_err(|e|db_failed("closing the unused trade",e))?;
        return Err(Outcome::custom("One of you has left the station or is already trading."));
    }
    info!("{me} opened a {}trade with {other} ({container})", if war.is_some() { "surrender " } else { "" });
    Ok(session)
}

/// A CEO trade: both are the active CEOs (an alliance's: its executor's) of
/// the two sides of a war that has not ended.
fn may_surrender(db: &Db, war: i64, me: i64, other: i64) -> Refused<()> {
    let Some((declarer, against)) = crate::svc::war::sides(db, war) else {
        return Err(message("CanNotSurrenderAsWarIsOver"));
    };
    let mut leaders = Vec::new();
    for side in [declarer, against] {
        // An alliance's side is led by its executor corporation's CEO.
        let corp = db.table("alliances").get_id(side).and_then(|a| a.int("executorCorpID")).unwrap_or(side);
        match db.table("corporation").get_id(corp).and_then(|co| co.int("ceoID")) {
            Some(ceo) => leaders.push(ceo),
            None => return Err(message("CanNotSurrenderToAllianceInTurmoil")),
        }
    }
    if !(leaders.contains(&me) && leaders.contains(&other)) {
        return Err(message("MustBeActiveCEOToEndWar"));
    }
    Ok(())
}

fn name_of(db: &Db, char_id: i64) -> String {
    db.table("character_").get_id(char_id).and_then(|c| c.text("characterName")).unwrap_or_default().to_string()
}

/// The bound trade session.
pub async fn session(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let container = bound::parse(&call.service).and_then(|b| b.params.first().copied()).unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let trade = ctx.world.lock().trades.get(&container).cloned();
    let Some(trade) = trade.filter(|t| t.side(me).is_some() && ctx.session.station_id == Some(t.station)) else {
        return Some(Outcome::custom("That trade is over."));
    };
    let _guard=trade.gate.lock().await;
    let Some(trade)=ctx.world.lock().trades.get(&container).cloned() else {return Some(Outcome::custom("That trade is over."))};
    let ids = |v: Option<&Value>| -> Vec<i64> { v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect() };
    let result = match call.method.as_str() {
        "List" => Ok(key_val(vec![
            ("tradeContainerID", Value::Int(container)),
            ("traders", Value::List(trade.traders.iter().map(|t| Value::Int(*t)).collect())),
            ("items", inventory::list(ctx, Container::Item(container)).await),
            ("money", Value::List(trade.money.iter().map(|m| Value::Double(*m)).collect())),
        ])),
        "IsCEOTrade" => Ok(Value::Bool(trade.war.is_some())),
        "Add" => offer(ctx, &trade, &[call.arg_id(0)], call.arg_i64(1)).await,
        "MultiAdd" => offer(ctx, &trade, &ids(call.args.first()), None).await,
        "MultiMerge" | "StackAll" => Ok(Value::None),
        "OfferMoney" => offer_money(ctx, &trade, call.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0)).await,
        "ToggleAccept" => accept(ctx, container, call.arg_i64(0).unwrap_or(0) != 0).await,
        "Abort" => abort_locked(ctx.store, ctx.world, container).await.map(|_| Value::None),
        _ => return Some(Outcome::Unknown(format!("{SESSION}::{}", call.method))),
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// Add/MultiAdd: the pilot's own items from the hangar go into the trade.
async fn offer(ctx: &Ctx<'_>, trade: &Trade, ids: &[i64], quantity: Option<i64>) -> Refused<Value> {
    if quantity.is_some_and(|q|q<=0 || q>i32::MAX as i64) {return Err(Outcome::custom("Choose a positive item quantity."))}
    let me = ctx.session.char_id.unwrap_or(0);
    let events = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        for &id in ids {
            let Some(item) = txn.item(id)? else { continue };
            let in_hangar = item.location_id == trade.station && item.flag == i64::from(FLAG_HANGAR);
            if item.owner_id != me || !in_hangar || Some(item.id) == ctx.session.ship_id {
                continue;
            }
            match offer_split_quantity(quantity, item.quantity, item.singleton)? {
                Some(q) => {
                    txn.set_quantity(id, item.quantity - q)?;
                    let new = NewItem { type_id: item.type_id, owner_id: me, location_id: trade.container, flag: 0, quantity: q, singleton: false };
                    txn.create(new)?;
                }
                None => txn.move_to(id, trade.container, 0)?,
            }
        }
        Ok::<_, Failed>(txn.events())
    }).map_err(|e| e.refusal("offering the items"))?;
    tell_items(ctx.world, trade, events);
    reset(ctx.world, trade.container);
    Ok(Value::None)
}

fn offer_split_quantity(requested: Option<i64>, available: i64, singleton: bool) -> Refused<Option<i64>> {
    let available = if singleton { 1 } else { available };
    let quantity = requested.unwrap_or(available);
    if quantity <= 0 || quantity > available || quantity > i32::MAX as i64 {
        return Err(Outcome::custom("That quantity is not available to trade."));
    }
    Ok((quantity < available).then_some(quantity))
}

async fn offer_money(ctx: &Ctx<'_>, trade: &Trade, amount: f64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(side) = trade.side(me) else { return Ok(Value::None) };
    let have = ctx.store.read(|db| db.table("character_").get_id(me).and_then(|c| c.float("balance"))).unwrap_or(0.0);
    if !amount.is_finite() || amount < 0.0 {
        return Err(Outcome::custom("That is not an amount of money."));
    }
    if amount > have {
        return Err(Outcome::not_enough_money(amount, have));
    }
    let mut w = ctx.world.lock();
    let Some(t) = w.trades.get_mut(&trade.container) else { return Ok(Value::None) };
    t.money[side] = amount;
    t.accepted = [false; 2];
    let money = Value::List(t.money.iter().map(|m| Value::Double(*m)).collect());
    let event = Value::Tuple(vec![Value::bytes("MoneyOffer"), Value::Int(trade.container), money]);
    let traders = t.traders;
    let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    w.notify(&clients, "OnTrade", event);
    Ok(Value::None)
}

/// ToggleAccept(state); both accepting completes the trade.
async fn accept(ctx: &Ctx<'_>, container: i64, state: bool) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let both = {
        let mut w = ctx.world.lock();
        let Some(t) = w.trades.get_mut(&container) else { return Ok(Value::None) };
        let Some(side) = t.side(me) else { return Ok(Value::None) };
        t.accepted[side] = state;
        let (accepted, traders) = (t.accepted, t.traders);
        let states = Value::List(accepted.iter().map(|a| Value::Int(i64::from(*a))).collect());
        let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
        w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("StateToggle"), Value::Int(container), states]));
        accepted == [true, true]
    };
    if both {
        complete(ctx, container).await?;
    }
    Ok(Value::None)
}

/// Each side's offer goes to the other's hangar; the money changes hands;
/// a surrender ends its war.
async fn complete(ctx: &Ctx<'_>, container: i64) -> Refused<()> {
    let Some(trade) = ctx.world.lock().trades.get(&container).cloned() else { return Ok(()) };
    let result = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let surrender=if let Some(war)=trade.war {
            may_surrender(txn.db(),war,trade.traders[0],trade.traders[1])?;
            Some(crate::svc::war::prepare_surrender(txn.tx(),war)?)
        } else {None};
        let inside: Vec<(i64, i64)> = txn
            .db()
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(container)])
            .filter_map(|r| Some((r.int("itemID")?, r.int("ownerID")?)))
            .collect();
        for (item, owner) in inside {
            if !trade.traders.contains(&owner) {return Err(Outcome::custom("The trade contains an item belonging to somebody else.").into())}
            let to = if owner == trade.traders[0] { trade.traders[1] } else { trade.traders[0] };
            for (child, child_owner) in contents(txn.db(), item) {
                if child_owner!=owner {return Err(Outcome::custom("An offered container holds somebody else's property.").into())}
                txn.set_owner(child,to)?;
            }
            txn.move_to(item, trade.station, i64::from(FLAG_HANGAR))?;
            txn.set_owner(item, to)?;
        }
        let mut balances = Vec::new();
        for (from, to, amount) in [(0, 1, trade.money[0]), (1, 0, trade.money[1])] {
            if amount <= 0.0 {
                continue;
            }
            let (payer, payee) = (trade.traders[from], trade.traders[to]);
            let entry = Entry { ref_type: REF_PLAYER_TRADING, payer, payee, arg_id1: "", reason: "" };
            let paid = wallet::post(txn.tx(), payer, -amount, &entry).map_err(crate::svc::membership::transfer_refused)?;
            let got = wallet::post(txn.tx(), payee, amount, &entry).map_err(crate::svc::membership::transfer_refused)?;
            balances.extend([(payer, paid), (payee, got)]);
        }
        txn.destroy(container)?;
        Ok::<_, Failed>((txn.events(), balances,surrender))
    }).map_err(|e| e.refusal("completing the trade"));
    let (events, balances,surrender) = match result {
        Ok(v) => v,
        Err(refusal) => {
            // Leave the trade open, unaccepted, for the two to sort out.
            reset(ctx.world, container);
            return Err(refusal);
        }
    };
    tell_items(ctx.world, &trade, events);
    {
        let mut w = ctx.world.lock();
        w.trades.remove(&container);
        for (owner, balance) in balances {
            let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
            w.notify(&w.clients_of(owner), "OnAccountChange", cash);
        }
        let clients: Vec<i64> = trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect();
        w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("TradeComplete"), Value::Int(container)]));
    }
    info!("trade {container} between {} and {} completed", trade.traders[0], trade.traders[1]);
    if let (Some(war),Some(event)) = (trade.war,surrender) {
        ctx.store.read(|db| crate::svc::war::announce_surrender(ctx.world, db, war,event));
    }
    Ok(())
}

/// Everything inside `item`, however deep, with its owner, by itemID.
fn contents(db: &Db, item: i64) -> Vec<(i64, i64)> {
    let entity = db.table("entity");
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut parents = vec![item];
    while let Some(parent) = parents.pop() {
        for e in entity.find(&["locationID"], &[store::Value::Int(parent)]) {
            let (Some(id), Some(owner)) = (e.int("itemID"), e.int("ownerID")) else { continue };
            if seen.insert(id) {
                found.push((id, owner));
                parents.push(id);
            }
        }
    }
    found.sort_unstable();
    found
}

/// Abort: everything goes back to its owner's hangar.
pub async fn abort(store: &Store, world: &World, container: i64) -> Refused<()> {
    let Some(trade)=world.lock().trades.get(&container).cloned() else {return Ok(())};
    let _guard=trade.gate.lock().await;
    abort_locked(store,world,container).await
}

async fn abort_locked(store: &Store, world: &World, container: i64) -> Refused<()> {
    let Some(trade) = world.lock().trades.get(&container).cloned() else { return Ok(()) };
    let result = store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let inside: Vec<i64> = txn
            .db()
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(container)])
            .filter_map(|r| r.int("itemID"))
            .collect();
        for item in inside {
            txn.move_to(item, trade.station, i64::from(FLAG_HANGAR))?;
        }
        txn.destroy(container)?;
        Ok::<_, StoreError>(txn.events())
    });
    let events = result.map_err(|e| db_failed("returning the offers", e))?;
    world.lock().trades.remove(&container);
    tell_items(world, &trade, events);
    let w = world.lock();
    let clients: Vec<i64> = trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("Cancel"), Value::Int(container)]));
    info!("trade {container} aborted");
    Ok(())
}

/// Inventory withdrawals must use the same gate as offer and acceptance RPCs.
pub async fn inventory_guard(world:&World,char_id:i64)->Option<tokio::sync::OwnedMutexGuard<()>> {
    let gate=world.lock().trades.values().find(|t|t.side(char_id).is_some()).map(|t|Arc::clone(&t.gate));
    match gate {Some(gate)=>Some(gate.lock_owned().await),None=>None}
}

/// Both acceptances clear when either offer changes.
fn reset(world: &World, container: i64) {
    let mut w = world.lock();
    let Some(t) = w.trades.get_mut(&container) else { return };
    t.accepted = [false; 2];
    let traders = t.traders;
    let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    let states = Value::List(vec![Value::Int(0), Value::Int(0)]);
    w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("StateToggle"), Value::Int(container), states]));
}

/// Item changes go to both traders: each watches the other's offer.
fn tell_items(world: &World, trade: &Trade, events: Vec<Value>) {
    let clients: Vec<i64> = {
        let w = world.lock();
        trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect()
    };
    for client in clients {
        items::notify(world, client, events.clone());
    }
}

/// Whether `item` is the caller's own offer in a trade it is part of: it
/// may take it back out (moves.rs).
pub fn my_offer(ctx: &Ctx<'_>, owner: i64, location: i64) -> bool {
    let me = ctx.session.char_id.unwrap_or(0);
    owner == me && ctx.world.lock().trades.get(&location).is_some_and(|t| t.side(me).is_some())
}

/// Something left a trade container (taken back out): the acceptances clear.
pub fn offers_changed(world: &World, origins: &[i64]) {
    let changed: Vec<i64> = {
        let w = world.lock();
        origins.iter().copied().filter(|o| w.trades.contains_key(o)).collect()
    };
    for container in changed {
        reset(world, container);
    }
}

/// Trades a pilot is part of end when they leave the station.
pub async fn left_station(store: &Store, world: &World, char_id: i64) {
    let mine: Vec<i64> = world.lock().trades.values().filter(|t| t.side(char_id).is_some()).map(|t| t.container).collect();
    for container in mine {
        if let Err(Outcome::UserError { msg, .. }) = abort(store, world, container).await {
            error!("aborting trade {container}: {msg}");
        }
    }
}

/// At startup: trade containers left over from before are emptied back into
/// their owners' hangars, and go.
pub fn recover(store: &Store) {
    let recovered = store.write(|tx| {
        let boxes: Vec<(store::RowId, i64, store::Value)> = tx
            .table("entity")
            .find(&["typeID"], &[store::Value::Int(TYPE_CARGO_CONTAINER)])
            .filter(|e| e.int("ownerID") == Some(OWNER_SYSTEM) && e.text("itemName") == Some(CONTAINER_NAME))
            .filter_map(|e| Some((e.id, e.int("itemID")?, e.value("locationID").clone())))
            .collect();
        for (_, id, location) in &boxes {
            let inside: Vec<store::RowId> = tx.table("entity").find(&["locationID"], &[store::Value::Int(*id)]).map(|e| e.id).collect();
            for row in inside {
                tx.update("entity", row, &[("locationID", location.clone()), ("flag", store::Value::Int(i64::from(FLAG_HANGAR)))])?;
            }
        }
        for (row, _, _) in &boxes {
            tx.delete("entity", *row)?;
        }
        Ok::<_, StoreError>(boxes.len())
    });
    match recovered {
        Ok(0) => {}
        Ok(n) => info!("{n} trade container(s) from before were emptied back into hangars"),
        Err(e) => error!("recovering trade containers: {}", crate::logging::chain(&e)),
    }
}
