//! Buying and selling on the market: marketProxy PlaceCharOrder,
//! CancelCharOrder and ModifyCharOrder (marketsvc.py:236-250).
//!
//! An order first trades against what is already on the market at its price or
//! better; with a duration of 0 ("immediate") it must fill completely or nothing
//! happens, otherwise what is left is listed. Buyers pay the seller's price.
//! NPC orders (owned by an NPC corporation) never run out of money: their buy
//! orders hold no escrow, and what they buy leaves the game.
//!
//! A corporation order (useCorp) is placed by a member with the Trader or
//! Accountant role (or a director) on the corporation's behalf: the
//! corporation's wallet pays and is paid, what it sells comes out of its
//! office's divisions (as the member's roles allow), and what it buys is
//! delivered to its deliveries hangar at the station.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{error, info};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, StoreError, Tx, Value as V, DATA_EXCEPTION};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::market::ORDER_HEADER;
use crate::svc::rows::row;
use crate::svc::wallet::{post_account, Entry, Transfer};

const RANGE_STATION: i64 = -1;
const ORDER_OPEN: i32 = 0;
/// const.mktOrderExpired, also used for an order that has run out.
const ORDER_DONE: i32 = 2;
const ORDER_CANCELLED: i32 = 3;
/// wallet.py:1045: 0 is money in (a sale), 1 money out (a purchase).
const TRANSACTION_SALE: i32 = 0;
const TRANSACTION_PURCHASE: i32 = 1;

const REF_MARKET_TRANSACTION: i32 = 2;
const REF_MARKET_ESCROW: i32 = 42;
const REF_BROKER_FEE: i32 = 46;
const REF_TRANSACTION_TAX: i32 = 54;
/// const.marketCommissionPercentage, const.mktTransactionTax, const.mktMinimumFee.
const BROKER_FEE: f64 = 0.01;
const TRANSACTION_TAX: f64 = 0.01;
const MINIMUM_FEE: f64 = 100.0;
const SKILL_BROKER_RELATIONS: i32 = 3446;
const SKILL_ACCOUNTING: i32 = 16622;
const FIRST_PLAYER_ID: i64 = 100_000_000;
const CORP_ROLE_TRADER: i64 = 18_014_398_509_481_984;

/// Whose wallet an order uses: its pilot's, or for a corporation order the
/// pilot's corporation's.
fn wallet(o: &store::Row) -> i64 {
    o.int("walletOwnerID").filter(|w| *w != 0).or(o.int("charID")).unwrap_or(0)
}

/// Orders made before they recorded whose wallet they use: a corporation
/// order's is its pilot's corporation (the pilot's own if the pilot is gone),
/// anyone else's the pilot's.
pub fn ensure(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    let characters = tx.table("character_");
    let unset: Vec<(store::RowId, i64)> = tx
        .table("market_orders")
        .rows()
        .filter(|o| o.int("walletOwnerID") == Some(0))
        .map(|o| {
            let pilot = o.int("charID").unwrap_or(0);
            let owner = if o.int("isCorp") == Some(1) {
                characters.get_id(pilot).and_then(|c| c.int("corporationID")).unwrap_or(pilot)
            } else {
                pilot
            };
            (o.id, owner)
        })
        .collect();
    for (id, owner) in unset {
        tx.update("market_orders", id, &[("walletOwnerID", V::Int(owner))])?;
    }
    Ok(())
}

/// Why an order was not placed.
#[derive(Debug)]
enum Refusal {
    Text(String),
    Money { need: f64, have: f64 },
    Db(StoreError),
}

impl From<StoreError> for Refusal {
    fn from(e: StoreError) -> Self {
        Refusal::Db(e)
    }
}
impl From<Transfer> for Refusal {
    fn from(t: Transfer) -> Self {
        match t {
            Transfer::NotEnough { need, have } => Refusal::Money { need, have },
            Transfer::NoSuchOwner => Refusal::Text("That wallet does not exist.".into()),
            Transfer::Db(e) => Refusal::Db(e),
        }
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Text(why.into()))
}

/// What the client is told once a trade commits.
#[derive(Default)]
struct Effects {
    item_events: Vec<Value>,
    /// Owners whose wallets changed, with their new balances.
    balances: HashMap<(i64,i32), f64>,
    /// Orders that changed, with the reason the client is given.
    orders: Vec<(i64, &'static str)>,
    refresh: Vec<(i64, i64, bool)>,
}

pub async fn market_write(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let Some(me) = ctx.session.char_id else { return Some(Outcome::custom("Select a character first.")) };
    let arity=match call.method.as_str() {"PlaceCharOrder"=>11,"CancelCharOrder"=>2,"ModifyCharOrder"=>8,_=>return None};
    if call.args.len()!=arity || !call.kwargs.is_empty() {return Some(Outcome::custom("Invalid market request arguments."));}
    let result = match call.method.as_str() {
        // PlaceCharOrder(stationID, typeID, price, quantity, bid, range, itemID,
        //                minVolume, duration, useCorp, located)
        "PlaceCharOrder" => {
            let order = NewOrder {
                station: call.arg_id(0),
                type_id: call.arg_id(1),
                price: (call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0)*100.0).round()/100.0,
                quantity: call.arg_id(3),
                bid: call.arg_id(4) != 0,
                range: call.arg_i64(5).unwrap_or(RANGE_STATION),
                item_id: call.arg_i64(6),
                min_volume: call.arg_i64(7).unwrap_or(1),
                duration: call.arg_id(8),
                corp: call.arg_id(9) != 0,
                existing: None,
                account: if call.arg_id(9)!=0 {ctx.session.corp_account_key.unwrap_or(1000)} else {1000},
            };
            let s = ctx.session;
            if let Err(why) = access(ctx, order.station, if order.bid {16594} else {16598}, order.bid && order.duration==0,
                (order.bid && s.station_id!=Some(order.station)).then_some(order.range)).await {
                return Some(Outcome::custom(why));
            }
            let roles = (s.corp_id, s.roles_at_all | if Some(order.station)==s.hq_id {s.roles_at_hq}
                else if Some(order.station)==s.base_id {s.roles_at_base} else {s.roles_at_other});
            transact(ctx, |txn, fx| place(txn, fx, me, order, roles)).await
        }
        // CancelCharOrder(orderID, regionID)
        "CancelCharOrder" => {
            let order_id = call.arg_id(0);
            if let Err(why)=order_access(ctx,order_id).await {return Some(Outcome::custom(why));}
            let roles=(ctx.session.corp_id,ctx.session.corp_role());
            transact(ctx, |txn, fx| cancel(txn, fx, me, order_id,roles)).await
        }
        // ModifyCharOrder(orderID, newPrice, bid, stationID, solarSystemID, price, volRemaining, issued)
        "ModifyCharOrder" => {
            let order_id = call.arg_id(0);
            if let Err(why)=order_access(ctx,order_id).await {return Some(Outcome::custom(why));}
            if let Some(issued)=ctx.store.read(|db| db.table("market_orders").get_id(order_id).and_then(|o|o.int("issued"))) {
                if crate::bulkdata::filetime_now()-issued<3_000_000_000 {return Some(Outcome::custom("Please wait five minutes after placing or modifying an order before modifying it again."));}
            }
            let price = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let roles=(ctx.session.corp_id,ctx.session.corp_role());
            transact(ctx, |txn, fx| modify(txn, fx, me, order_id, price,roles)).await
        }
        _ => return None,
    };
    Some(result)
}

fn skill_distance(level: i64) -> i64 { [-1,0,5,10,20,50][level.clamp(0,5) as usize] }

async fn access(ctx: &Ctx<'_>, station: i64, skill: i32, immediate_buy: bool, visibility: Option<i64>) -> Result<(),String> {
    i32::try_from(station).map_err(|_|"Invalid station.")?;
    ctx.store.read(|db| -> Result<(),String> {
        let r=db.table("staStations").get_id(station).ok_or("Unknown station.")?;
        if ctx.session.region_id!=r.int("regionID") {return Err("You can only trade in your current region.".into());}
        if immediate_buy {return Ok(());}
        let skills=crate::svc::skills::levels(db,ctx.session.char_id.unwrap_or(0),&[skill,3447]).map_err(|_|"Trade skills could not be checked.")?;
        let distance=if ctx.session.station_id==Some(station) {-1} else {
            crate::gates::jumps(db,ctx.session.solar_system_id2.unwrap_or(0),r.int("solarSystemID").unwrap_or(0),50).unwrap_or(32767)
        };
        if distance>skill_distance(skills[&skill]) {return Err("Your trade skills do not allow trading at that distance.".into());}
        if visibility.is_some_and(|range|range>skill_distance(skills[&3447])) {return Err("Your Visibility skill does not allow that remote order range.".into());}
        Ok(())
    })
}

async fn order_access(ctx: &Ctx<'_>, id: i64) -> Result<(),String> {
    i32::try_from(id).map_err(|_|"Invalid order.")?;
    let station=ctx.store.read(|db| db.table("market_orders").get_id(id).and_then(|o|o.int("stationID"))).ok_or("Unknown order.")?;
    access(ctx,station,16595,false,None).await
}

async fn transact<F>(ctx: &Ctx<'_>, work: F) -> Outcome
where
    F: FnOnce(&mut ItemTxn<'_, '_>, &mut Effects) -> Result<(), Refusal>,
{
    let mut fx = Effects::default();
    let result: Result<(), Refusal> = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        work(&mut txn, &mut fx)?;
        fx.item_events = txn.events();
        Ok(())
    });
    match result {
        Ok(()) => {
            tell(ctx, fx).await;
            Outcome::Value(Value::None)
        }
        Err(Refusal::Text(why)) => {
            info!("market order refused: {why}");
            Outcome::custom(why)
        }
        Err(Refusal::Money { need, have }) => Outcome::not_enough_money(need, have),
        Err(Refusal::Db(e)) => {
            error!("market order: {}", crate::logging::chain(&e));
            Outcome::custom("The market could not process that order.")
        }
    }
}

/// An order as the client's order rows have it (`ORDER_HEADER`): its own
/// columns, then its system's constellation.
fn order_line(db: &Db, o: &store::Row) -> Vec<Value> {
    let mut line = o.line(&[
        "orderID", "typeID", "charID", "regionID", "stationID", "range", "bid", "price", "volEntered", "volRemaining", "issued",
        "orderState", "minVolume", "contraband", "accountID", "duration", "isCorp", "solarSystemID", "escrow", "jumps",
    ]);
    let system = o.int("solarSystemID").and_then(|s| db.table("mapSolarSystems").get_id(s));
    line.push(system.map_or(Value::None, |s| s.cell("constellationID")));
    line
}

/// Send item, wallet and order changes to whoever they concern.
async fn tell(ctx: &Ctx<'_>, fx: Effects) {
    // Each order's line, pilot, corporation flag and wallet owner.
    let (rows, corporations) = ctx.store.read(|db| {
        let orders = db.table("market_orders");
        let rows: HashMap<i64, (Vec<Value>, i64, bool, i64)> = fx
            .orders
            .iter()
            .map(|(id, _)| *id)
            .chain(fx.refresh.iter().map(|(_, id, _)| *id))
            .filter_map(|id| {
                let o = orders.get_id(id)?;
                Some((id, (order_line(db, &o), o.int("charID").unwrap_or(0), o.int("isCorp").unwrap_or(0) != 0, o.int("walletOwnerID").unwrap_or(0))))
            })
            .collect();
        let corporations: std::collections::HashSet<i64> =
            fx.balances.keys().map(|(id, _)| *id).filter(|id| db.table("corporation").get_id(*id).is_some()).collect();
        (rows, corporations)
    });
    let world = ctx.world.lock();
    for event in fx.item_events {
        let owner = event.seq().and_then(|args|args.first()).and_then(|item| {
            let Value::Instance {state,..}=item else {return None};
            let Value::Dict(fields)=state.as_ref() else {return None};
            fields.iter().find(|(k,_)|*k==Value::bytes("line"))?.1.seq()?.get(2)?.as_i64()
        });
        if let Some(owner)=owner {
            let mut clients=world.clients_of(owner);
            clients.extend(world.corp_members(owner));
            clients.sort_unstable(); clients.dedup();
            world.notify(&clients,"OnItemChange",event);
        }
    }
    for ((owner,key), balance) in fx.balances {
        let event = Value::Tuple(vec![Value::bytes(if key==1000 {"cash".into()} else {format!("cash{}",key-999)}), Value::Int(owner), Value::Double(balance)]);
        let clients=if corporations.contains(&owner) {world.corp_members(owner)} else {world.clients_of(owner)};
        world.notify(&clients, "OnAccountChange", event);
    }
    for (order_id, reason) in fx.orders {
        let Some((line, owner, corp, wallet_owner)) = rows.get(&order_id) else { continue };
        let event = Value::Tuple(vec![row(ORDER_HEADER, line.clone()), Value::bytes(reason), Value::Bool(*corp)]);
        let clients=if *corp {world.corp_members(*wallet_owner)} else {world.clients_of(*owner)};
        world.notify(&clients, "OnOwnOrderChanged", event);
    }
    for (actor,id,corp) in fx.refresh {
        if let Some((line,..))=rows.get(&id) {
            world.notify(&world.clients_of(actor),"OnOwnOrderChanged",Value::Tuple(vec![
                row(ORDER_HEADER,line.clone()),Value::bytes("Modify"),Value::Bool(corp)]));
        }
    }
}

struct NewOrder {
    station: i64,
    type_id: i64,
    price: f64,
    quantity: i64,
    bid: bool,
    range: i64,
    item_id: Option<i64>,
    min_volume: i64,
    duration: i64,
    corp: bool,
    existing: Option<i64>,
    account: i32,
}

/// An order on the other side of a trade.
struct Match {
    id: i64,
    /// Whose wallet and goods: a pilot's, or a corporation's.
    owner: i64,
    issuer: i64,
    account: i32,
    station: i64,
    price: f64,
    remaining: i64,
    escrow: f64,
    corp: bool,
    minimum: i64,
    range: i64,
    system: i64,
}

/// Where goods bought for an owner go: a corporation's deliveries, or a
/// pilot's hangar.
fn delivery_flag(corp: bool) -> i64 {
    if corp { crate::svc::inventory::FLAG_CORP_MARKET as i64 } else { FLAG_HANGAR as i64 }
}

struct Where {
    solar_system: i64,
    region: i64,
}

fn place(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, me: i64, order: NewOrder, ctx_roles: (Option<i64>, i64)) -> Result<(), Refusal> {
    let who = if order.corp {
        let (corp, roles) = ctx_roles;
        let Some(corp) = corp else { return refuse("You are not in a corporation.") };
        if roles & (crate::session::CORP_ROLE_DIRECTOR | crate::session::CORP_ROLE_ACCOUNTANT | CORP_ROLE_TRADER) == 0 {
            return refuse("Placing orders for your corporation needs the Trader or Accountant role.");
        }
        if !crate::svc::wallet::can_take(roles,order.account) {return refuse("You may not spend from that corporation wallet division.");}
        corp
    } else {
        me
    };
    if order.quantity <= 0 || order.quantity>i32::MAX as i64 || !order.price.is_finite() || order.price < 0.01 || order.price > 9_223_372_036_854.0 || !(order.price*order.quantity as f64).is_finite() {
        return refuse("Enter a quantity and a price.");
    }
    if order.min_volume<1 || order.min_volume>order.quantity || ![0,1,3,7,14,30,90].contains(&order.duration) || !(-1..=32767).contains(&order.range) {return refuse("Invalid minimum volume, duration or order range.")}
    if i32::try_from(order.station).is_err() || i32::try_from(order.type_id).is_err() {
        return refuse("Invalid station or item type.");
    }
    if order.item_id.is_some_and(|id|id<=0 || i32::try_from(id).is_err()) {return refuse("Invalid item ID.");}
    if !crate::sde::table("invTypes").find("typeID",order.type_id).any(|t|t.int("marketGroupID").is_some()) {
        return refuse("This item cannot be traded on the market.");
    }
    let station = txn.db().table("staStations").get_id(order.station).map(|s| (s.int("solarSystemID"), s.int("regionID")));
    let Some((Some(solar_system), Some(region))) = station else { return refuse("That station has no market.") };
    let at = Where { solar_system, region };
    if order.bid {
        buy(txn, fx, me, who, &order, &at)
    } else {
        sell(txn, fx, me, who, &order, &at, ctx_roles.1)
    }
}

/// The open orders with something left of a type in a region, on one side
/// of the market.
fn open_orders<'d>(db: &'d Db, type_id: i64, region: i64, bid: bool) -> impl Iterator<Item = store::Row<'d>> + 'd {
    db.table("market_orders")
        .find(&["typeID", "regionID", "bid"], &[V::Int(type_id), V::Int(region), V::Int(i64::from(bid))])
        .filter(|o| o.int("orderState") == Some(ORDER_OPEN.into()) && o.int("volRemaining").is_some_and(|v| v > 0))
}

fn buy(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, me: i64, who: i64, order: &NewOrder, at: &Where) -> Result<(), Refusal> {
    // Sell orders inside the buy order's range, cheapest first.
    let range = order.range.max(-1);
    let mut rows: Vec<(Match, i64)> = open_orders(txn.db(), order.type_id, at.region, false)
        .filter(|o| o.float("price").is_some_and(|p| p <= order.price + 0.005) && wallet(o) != who)
        .filter(|o| {
            range > 0
                || (range == -1 && o.int("stationID") == Some(order.station))
                || (range == 0 && o.int("solarSystemID") == Some(at.solar_system))
        })
        .map(|o| (as_match(&o), o.int("issued").unwrap_or(0)))
        .collect();
    rows.sort_by(|(a, a_issued), (b, b_issued)| a.price.total_cmp(&b.price).then(a_issued.cmp(b_issued)));
    let reachable:Option<std::collections::HashSet<i64>>=if order.range>0 && order.range<32767 {
        Some(crate::gates::within(txn.db(),at.solar_system,order.range).into_iter().map(|(id,_)|id).collect())
    } else {None};
    let fills = take(rows.into_iter().map(|(m, _)| m).filter(|m|reachable.as_ref().map_or(true,|systems|systems.contains(&m.system))), order.quantity);
    let filled: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && filled < order.quantity {
        return refuse("There is not enough for sale at that price within that range.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: who, payee: m.owner, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, -amount, &entry)?);
        if m.owner >= FIRST_PLAYER_ID {
            fx.balances.insert((m.owner,m.account), post_account(txn.tx(), m.owner, m.account, amount, &entry)?);
            pay_tax(txn, fx, m.owner, m.issuer, m.account, amount)?;
            record(txn.tx(), m.issuer, m.owner, m.account, who, order.type_id, *n, m.price, TRANSACTION_SALE, m.station, at.region)?;
        }
        record(txn.tx(), me, who, order.account, m.owner, order.type_id, *n, m.price, TRANSACTION_PURCHASE, m.station, at.region)?;
        let stack = NewItem { type_id: order.type_id, owner_id: who, location_id: m.station, flag: delivery_flag(order.corp), quantity: *n, singleton: false };
        txn.stack_onto(stack)?;
        history(txn.tx(), at.region, order.type_id, m.price, *n)?;
        fill(txn.tx(), fx, m, *n, None)?;
        fx.refresh.push((me,m.id,order.corp));
    }

    let left = order.quantity - filled;
    if left > 0 {
        let value = order.price * left as f64;
        let levels=crate::svc::skills::levels(txn.db(),me,&[16597])?;
        let escrow = (value * 0.75f64.powi(levels[&16597].clamp(0,5) as i32)*100.0).ceil()/100.0;
        if order.existing.is_none() { broker_fee(txn.tx(), fx, me, who, order.account, order.station, value)?; }
        let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: who, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, -escrow, &entry)?);
        let id = list(txn.tx(), me, who, order, at, left, escrow)?;
        fx.orders.push((id, if order.existing.is_some() { "Modify" } else { "Add" }));
    }
    info!("bought {filled} of type {} (listed {left})", order.type_id);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sell(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, me: i64, who: i64, order: &NewOrder, at: &Where, roles: i64) -> Result<(), Refusal> {
    let item = if order.existing.is_some() { None } else {
    let Some(item_id) = order.item_id else { return refuse("Choose the item to sell.") };
    let Some(item) = txn.item(item_id)? else { return refuse("That item no longer exists.") };
    if order.corp {
        // Out of the corporation's office there, from a division the seller may take from.
        let office = crate::svc::corphangar::office(txn.db(), item.location_id);
        let divisions = roles & crate::session::CORP_ROLE_DIRECTOR != 0 || crate::svc::corphangar::division(item.flag).is_some_and(|d| roles & (8_192 << (d - 1)) != 0);
        if item.owner_id != who || office != Some((who, order.station)) || !divisions {
            return refuse("Corporation items must be in a division of its office at that station that you may take from.");
        }
    } else if item.owner_id != me || item.location_id != order.station || item.flag != FLAG_HANGAR as i64 {
        return refuse("Items must be in your hangar at that station to be sold there.");
    }
    if item.type_id != order.type_id || order.quantity > item.quantity {
        return refuse("You do not have that many to sell.");
    }
    if item.singleton {
        return refuse("Repackage the item before selling it. Blueprint copies and assembled items cannot be sold on the market.");
    }
    if txn.db().table("entity").find(&["locationID"], &[V::Int(item_id)]).next().is_some() {
        return refuse("Empty the item before selling it.");
    }

        Some(item)
    };

    // Buy orders whose range reaches this station, best price first.
    let mut rows: Vec<(Match, i64)> = open_orders(txn.db(), order.type_id, at.region, true)
        .filter(|o| o.float("price").is_some_and(|p| p >= order.price - 0.005) && wallet(o) != who)
        .filter(|o| o.int("minVolume").unwrap_or(0).min(o.int("volRemaining").unwrap_or(0)) <= order.quantity)
        .filter(|o| {
            let range = o.int("range").unwrap_or(0);
            range > 0
                || (range == -1 && o.int("stationID") == Some(order.station))
                || (range == 0 && o.int("solarSystemID") == Some(at.solar_system))
        })
        .map(|o| (as_match(&o), o.int("issued").unwrap_or(0)))
        .collect();
    rows.sort_by(|(a, a_issued), (b, b_issued)| b.price.total_cmp(&a.price).then(a_issued.cmp(b_issued)));
    let mut eligible=Vec::new();
    let mut pending=order.quantity;
    for (m, _) in rows {
        if m.range>0 && m.range<32767 && crate::gates::jumps(txn.db(),at.solar_system,m.system,m.range).is_none() {continue}
        let quantity=pending.min(m.remaining);
        if quantity<m.minimum.min(m.remaining) || quantity==0 {continue;}
        let gap=(m.price*quantity as f64-m.escrow).max(0.0);
        if m.owner>=FIRST_PLAYER_ID && gap>0.0 {
            let db=txn.db();
            let balance=if m.account!=1000 {
                db.table("corporation_wallets").get(&[V::Int(m.owner),V::Int(m.account.into())]).and_then(|r|r.float("balance"))
            } else if m.corp {
                db.table("corporation").get_id(m.owner).and_then(|r|r.float("balance"))
            } else {
                db.table("character_").get_id(m.owner).and_then(|r|r.float("balance"))
            };
            if balance.is_none_or(|b|b<gap) {continue;}
        }
        pending-=quantity;
        eligible.push(m);
    }
    let fills = take(eligible.into_iter(), order.quantity);
    let sold: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && sold < order.quantity {
        return refuse("Nobody is buying that many at that price within reach.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        if m.owner>=FIRST_PLAYER_ID && amount>m.escrow {
            let entry=Entry{ref_type:REF_MARKET_TRANSACTION,payer:m.owner,payee:who,arg_id1:"",reason:""};
            fx.balances.insert((m.owner,m.account),post_account(txn.tx(),m.owner,m.account,-(amount-m.escrow),&entry)?);
        }
        // Paid out of the buyer's escrow, or by an NPC buyer.
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: m.owner, payee: who, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, amount, &entry)?);
        pay_tax(txn, fx, who, me, order.account, amount)?;
        record(txn.tx(), me, who, order.account, m.owner, order.type_id, *n, m.price, TRANSACTION_SALE, order.station, at.region)?;
        if m.owner >= FIRST_PLAYER_ID {
            record(txn.tx(), m.issuer, m.owner, m.account, who, order.type_id, *n, m.price, TRANSACTION_PURCHASE, order.station, at.region)?;
            let stack = NewItem { type_id: order.type_id, owner_id: m.owner, location_id: order.station, flag: delivery_flag(m.corp), quantity: *n, singleton: false };
            txn.stack_onto(stack)?;
        }
        history(txn.tx(), at.region, order.type_id, m.price, *n)?;
        fill(txn.tx(), fx, m, *n, Some(amount.min(m.escrow)))?;
        fx.refresh.push((me,m.id,order.corp));
    }

    let left = order.quantity - sold;
    if left > 0 {
        if order.existing.is_none() { broker_fee(txn.tx(), fx, me, who, order.account, order.station, order.price * left as f64)?; }
        let id = list(txn.tx(), me, who, order, at, left, 0.0)?;
        fx.orders.push((id, if order.existing.is_some() { "Modify" } else { "Add" }));
    }
    if let Some(item) = item { txn.set_quantity(item.id, item.quantity - order.quantity)?; }
    info!("sold {sold} of type {} (listed {left})", order.type_id);
    Ok(())
}

fn as_match(o: &store::Row) -> Match {
    Match {
        id: o.int("orderID").unwrap_or(0),
        owner: wallet(o),
        issuer: o.int("charID").unwrap_or(0),
        account: o.int("accountID").unwrap_or(0) as i32,
        station: o.int("stationID").unwrap_or(0),
        price: o.float("price").unwrap_or(0.0),
        remaining: o.int("volRemaining").unwrap_or(0),
        escrow: o.float("escrow").unwrap_or(0.0),
        corp: o.int("isCorp").is_some_and(|c| c != 0),
        minimum: o.int("minVolume").unwrap_or(1),
        range: o.int("range").unwrap_or(-1),
        system: o.int("solarSystemID").unwrap_or(0),
    }
}

/// Walk `orders` until `wanted` units are covered.
fn take(orders: impl Iterator<Item = Match>, wanted: i64) -> Vec<(Match, i64)> {
    let mut left = wanted;
    let mut out = Vec::new();
    for m in orders {
        if left == 0 {
            break;
        }
        let n = left.min(m.remaining);
        if n<m.minimum.min(m.remaining) {continue}
        left -= n;
        out.push((m, n));
    }
    out
}

/// Set columns of the order `id` (nothing if it is gone, as an UPDATE
/// matching no row did).
fn set_order(tx: &mut Tx<'_>, id: i64, sets: store::Sets) -> Result<(), StoreError> {
    match tx.table("market_orders").get_id(id).map(|o| o.id) {
        Some(row) => tx.update("market_orders", row, sets),
        None => Ok(()),
    }
}

/// Take `n` units off an order; a buy order also gives up `spent` escrow.
fn fill(tx: &mut Tx<'_>, fx: &mut Effects, m: &Match, n: i64, spent: Option<f64>) -> Result<(), Refusal> {
    if m.owner < FIRST_PLAYER_ID && tx.table("market_orders").get_id(m.id).and_then(|o| o.bool("starterSeed")) == Some(true) {
        return Ok(());
    }
    if m.owner>=FIRST_PLAYER_ID && spent.is_some_and(|s|s>m.escrow+0.005) {return refuse("That buy order no longer has enough money in escrow.")}
    let remaining = m.remaining - n;
    let escrow = spent.map_or(m.escrow, |s| (m.escrow - s).max(0.0));
    let state = if remaining == 0 { ORDER_DONE } else { ORDER_OPEN };
    set_order(tx, m.id, &[("volRemaining", V::Int(remaining)), ("orderState", V::Int(state.into())), ("escrow", V::Float(escrow))])?;
    if m.owner >= FIRST_PLAYER_ID {
        fx.orders.push((m.id, if remaining == 0 { "Expiry" } else { "Modify" }));
    }
    Ok(())
}

fn list(tx: &mut Tx<'_>, me: i64, who:i64, order: &NewOrder, at: &Where, quantity: i64, escrow: f64) -> Result<i64, Refusal> {
    let range = if order.bid { order.range } else { RANGE_STATION };
    if let Some(id) = order.existing {
        set_order(tx, id, &[("price", V::Float(order.price)), ("volRemaining", V::Int(quantity)), ("escrow", V::Float(escrow)),
            ("orderState", V::Int(0)), ("issued", V::Int(crate::bulkdata::filetime_now()))])?;
        return Ok(id);
    }
    let levels=crate::svc::skills::levels(tx.db(),me,&[3443,3444,16596,18580])?;
    let maximum=5+levels[&3443]*4+levels[&3444]*8+levels[&16596]*16+levels[&18580]*32;
    let count=tx.table("market_orders").find(&["charID"],&[V::Int(me)]).filter(|o|o.int("orderState")==Some(0)).count() as i64;
    if count>=maximum {return refuse("Your Trade skills do not allow any more open orders.");}

    let row = tx.insert(
        "market_orders",
        &[
            ("typeID", V::Int(order.type_id)),
            ("charID", V::Int(me)),
            ("regionID", V::Int(at.region)),
            ("stationID", V::Int(order.station)),
            ("range", V::Int(range)),
            ("bid", V::Int(i64::from(order.bid))),
            ("price", V::Float(order.price)),
            ("volEntered", V::Int(quantity)),
            ("volRemaining", V::Int(quantity)),
            ("issued", V::Int(crate::bulkdata::filetime_now())),
            ("orderState", V::Int(0)),
            ("minVolume", V::Int(order.min_volume)),
            ("contraband", V::Int(0)),
            ("accountID", V::Int(order.account.into())),
            ("duration", V::Int(order.duration)),
            ("isCorp", V::Int(i64::from(order.corp))),
            ("solarSystemID", V::Int(at.solar_system)),
            ("escrow", V::Float(escrow)),
            ("jumps", V::Int(1)),
            ("walletOwnerID", V::Int(who)),
        ],
    )?;
    Ok(tx.table("market_orders").row(row).and_then(|o| o.int("orderID")).unwrap_or(0))
}

/// The broker's commission for listing an order worth `value`, by the
/// lister's skill, from the wallet the order uses.
fn broker_fee(tx: &mut Tx<'_>, fx: &mut Effects, me: i64, who: i64, account: i32, station: i64, value: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(tx.db(), me, &[SKILL_BROKER_RELATIONS])?;
    let Some(s) = tx.table("staStations").get_id(station) else {
        return Err(Refusal::Db(StoreError { code: DATA_EXCEPTION, message: format!("broker fee: no station {station}") }));
    };
    // The pilot's standing with the station's owner and with its faction
    // (crpNPCCorporations), 0 where there is none.
    let owner = s.int("corporationID");
    let faction = owner.and_then(|c| crate::sde::table("crpNPCCorporations").get("corporationID", c)).and_then(|n| n.int("factionID"));
    let standings = tx.table("chrNPCStandings");
    let standing =
        |from: Option<i64>| from.and_then(|f| standings.get(&[V::Int(me), V::Int(f)])).and_then(|r| r.float("standing")).unwrap_or(0.0);
    let weighted=(0.7*standing(faction)+0.3*standing(owner))/10.0;
    let rate = BROKER_FEE * (1.0 - 0.05 * level[&SKILL_BROKER_RELATIONS].clamp(0,5) as f64) * 2.0f64.powf(-2.0*weighted);
    let fee = (value * rate).max(MINIMUM_FEE);
    let entry = Entry { ref_type: REF_BROKER_FEE, payer: who, payee: 0, arg_id1: "", reason: "" };
    fx.balances.insert((who,account), post_account(tx, who, account, -fee, &entry)?);
    Ok(())
}

/// Sales tax on `gross` earned by `seller`.
fn pay_tax(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, seller: i64, issuer: i64, account: i32, gross: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(txn.db(), issuer, &[SKILL_ACCOUNTING])?;
    let tax = gross * TRANSACTION_TAX * (1.0 - 0.1 * level[&SKILL_ACCOUNTING] as f64);
    if tax >= 0.01 {
        let entry = Entry { ref_type: REF_TRANSACTION_TAX, payer: seller, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert((seller,account), post_account(txn.tx(), seller, account, -tax, &entry)?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record(
    tx: &mut Tx<'_>,
    who: i64,
    wallet_owner: i64,
    account: i32,
    other: i64,
    type_id: i64,
    quantity: i64,
    price: f64,
    kind: i32,
    station: i64,
    region: i64,
) -> Result<(), Refusal> {
    tx.insert(
        "market_transactions",
        &[
            ("transactionDateTime", V::Int(crate::bulkdata::filetime_now())),
            ("typeID", V::Int(type_id)),
            ("quantity", V::Int(quantity)),
            ("price", V::Float(price)),
            ("transactionType", V::Int(kind.into())),
            ("clientID", V::Int(other)),
            ("regionID", V::Int(region)),
            ("stationID", V::Int(station)),
            ("characterID", V::Int(who)),
            ("isCorp", V::Int(i64::from(wallet_owner != who))),
            ("accountKey", V::Int(account.into())),
            ("corporationID", V::Int(if wallet_owner != who { wallet_owner } else { 0 })),
        ],
    )?;
    Ok(())
}

fn history(tx: &mut Tx<'_>, region: i64, type_id: i64, price: f64, quantity: i64) -> Result<(), Refusal> {
    let day = crate::bulkdata::filetime_now() / 864_000_000_000 * 864_000_000_000;
    let key = [V::Int(region), V::Int(type_id), V::Int(day)];
    let today = tx.table("market_history").get(&key).map(|h| {
        (h.id, h.float("lowPrice").unwrap_or(0.0), h.float("highPrice").unwrap_or(0.0), h.float("avgPrice").unwrap_or(0.0), h.int("volume").unwrap_or(0), h.int("orders").unwrap_or(0))
    });
    match today {
        Some((id, low, high, avg, volume, orders)) => tx.update(
            "market_history",
            id,
            &[
                ("lowPrice", V::Float(low.min(price))),
                ("highPrice", V::Float(high.max(price))),
                ("avgPrice", V::Float((avg * volume as f64 + price * quantity as f64) / (volume + quantity) as f64)),
                ("volume", V::Int(volume + quantity)),
                ("orders", V::Int(orders + 1)),
            ],
        )?,
        None => {
            tx.insert(
                "market_history",
                &[
                    ("regionID", key[0].clone()),
                    ("typeID", key[1].clone()),
                    ("historyDate", key[2].clone()),
                    ("lowPrice", V::Float(price)),
                    ("highPrice", V::Float(price)),
                    ("avgPrice", V::Float(price)),
                    ("volume", V::Int(quantity)),
                    ("orders", V::Int(1)),
                ],
            )?;
        }
    }
    Ok(())
}

/// Refund escrow and return unsold goods exactly once.
/// A player's open order whose duration has run out by `now` (FILETIME).
fn is_due(o: &crate::store::Row<'_>, now: i128) -> bool {
    o.int("orderState") == Some(0)
        && o.int("charID").is_some_and(|c| c >= FIRST_PLAYER_ID)
        && i128::from(o.int("issued").unwrap_or(0)) + i128::from(o.int("duration").unwrap_or(0)) * 864_000_000_000 <= now
}

fn expire_orders(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects) -> Result<(), Refusal> {
    let now = i128::from(crate::bulkdata::filetime_now());
    let mut due: Vec<(Match, bool, i64)> = txn
        .db()
        .table("market_orders")
        .rows()
        .filter(|o| is_due(o, now))
        .map(|o| (as_match(&o), o.int("bid").unwrap_or(0) != 0, o.int("typeID").unwrap_or(0)))
        .collect();
    due.sort_by_key(|(m, _, _)| m.id);
    for (order, bid, type_id) in due {
        close_order(txn, fx, &order, bid, type_id, ORDER_DONE)?;
    }
    Ok(())
}

/// When orders were last looked over for expiry (ms).
static LAST_EXPIRY: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// Looking means a scan of every order, the bots' too: orders run for days,
/// so every half minute is soon enough, and the market's calls stay quick.
const EXPIRY_EVERY_MS: i64 = 30_000;

/// Close the players' orders that have run their time, at most every
/// [`EXPIRY_EVERY_MS`]; the scan is a read, and only a due order takes a write.
pub async fn expire(ctx: &Ctx<'_>) -> Outcome {
    use std::sync::atomic::Ordering::Relaxed;
    let now = crate::combat::now_ms();
    let last = LAST_EXPIRY.load(Relaxed);
    if now - last < EXPIRY_EVERY_MS || LAST_EXPIRY.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return Outcome::Value(Value::None);
    }
    let filetime = i128::from(crate::bulkdata::filetime_now());
    if !ctx.store.read(|db| db.table("market_orders").rows().any(|o| is_due(&o, filetime))) {
        return Outcome::Value(Value::None);
    }
    transact(ctx, expire_orders).await
}

/// An open order of the caller's.
fn own_order(txn: &ItemTxn<'_, '_>, me: i64, order_id: i64,corp:Option<i64>) -> Result<(Match, bool, i64), Refusal> {
    if i32::try_from(order_id).is_err() { return refuse("Invalid order ID."); }
    let r = txn.db().table("market_orders").get_id(order_id).filter(|o| {
        (o.int("charID") == Some(me) || (o.int("isCorp") == Some(1) && corp.is_some() && o.int("walletOwnerID") == corp))
            && o.int("orderState") == Some(0)
    });
    let Some(r) = r else { return refuse("That order is no longer open.") };
    Ok((as_match(&r), r.int("bid").unwrap_or(0) != 0, r.int("typeID").unwrap_or(0)))
}

/// Cancel: a buy order's escrow comes back, a sell order's goods go back to
/// the hangar where it was listed.
fn cancel(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, me: i64, order_id: i64,roles:(Option<i64>,i64)) -> Result<(), Refusal> {
    let (order, bid, type_id) = own_order(txn, me, order_id,roles.0)?;
    authorize_order(&order,roles)?;
    close_order(txn, fx, &order, bid, type_id, ORDER_CANCELLED)
}

fn close_order(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, order: &Match, bid: bool, type_id: i64, state: i32) -> Result<(), Refusal> {
    let order_id = order.id;
    let who = order.owner;
    if bid {
        if order.escrow > 0.0 {
            let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: 0, payee: who, arg_id1: "", reason: "" };
            fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, order.escrow, &entry)?);
        }
    } else if order.remaining > 0 {
        let stack = NewItem { type_id, owner_id: who, location_id: order.station, flag: delivery_flag(order.corp), quantity: order.remaining, singleton: false };
        txn.stack_onto(stack)?;
    }
    set_order(txn.tx(), order_id, &[("orderState", V::Int(state.into())), ("escrow", V::Float(0.0))])?;
    fx.orders.push((order_id, "Expiry"));
    info!("cancelled order {order_id}");
    Ok(())
}

/// Reprice: a buy order's escrow follows its price.
fn modify(txn: &mut ItemTxn<'_, '_>, fx: &mut Effects, me: i64, order_id: i64, price: f64, roles: (Option<i64>,i64)) -> Result<(), Refusal> {
    if !price.is_finite() || price < 0.01 || price > 9_223_372_036_854.0 {
        return refuse("Enter a valid price.");
    }
    let (old, bid, type_id) = own_order(txn, me, order_id, roles.0)?;
    authorize_order(&old, roles)?;
    let r = txn.db().table("market_orders").get_id(order_id).map(|o| {
        let int = |c: &str| o.int(c).unwrap_or(0);
        (int("range"), int("minVolume"), int("duration"), int("solarSystemID"), int("regionID"))
    });
    let Some((range, min_volume, duration, solar_system, region)) = r else { return refuse("That order is no longer open.") };
    let order = NewOrder { station: old.station, type_id, price, quantity: old.remaining, bid,
        range, min_volume: min_volume.min(old.remaining),
        duration, corp: old.corp, item_id: None, existing: Some(order_id), account: old.account };
    let at = Where { solar_system, region };
    // Release the old reservation inside the same transaction. The replacement
    // immediately matches crossing orders and reserves only its remainder.
    if bid && old.escrow > 0.0 {
        let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: 0, payee: old.owner, arg_id1: "", reason: "" };
        fx.balances.insert((old.owner,old.account), post_account(txn.tx(), old.owner, old.account, old.escrow, &entry)?);
    }
    broker_fee(txn.tx(), fx, me, old.owner, old.account, old.station, ((price-old.price)*old.remaining as f64).max(0.0))?;
    set_order(txn.tx(), order_id, &[("orderState", V::Int(2)), ("volRemaining", V::Int(0)), ("escrow", V::Float(0.0)), ("price", V::Float(price))])?;
    fx.orders.push((order_id,"Expiry"));
    if bid { buy(txn, fx, old.issuer, old.owner, &order, &at)?; }
    else { sell(txn, fx, old.issuer, old.owner, &order, &at, roles.1)?; }
    // One final notification, rather than an expiry followed by a modification.
    if fx.orders.iter().filter(|(id,_)| *id==order_id).count()>1 {
        let first=fx.orders.iter().position(|(id,_)|*id==order_id).unwrap();
        fx.orders.remove(first);
    }
    Ok(())
}

fn authorize_order(order:&Match,roles:(Option<i64>,i64))->Result<(),Refusal> {
    if order.corp && !crate::svc::wallet::can_take(roles.1,order.account) {return refuse("You may not manage orders in that corporation wallet division.");}
    if order.corp && (roles.0!=Some(order.owner) || roles.1 & (crate::session::CORP_ROLE_DIRECTOR | crate::session::CORP_ROLE_ACCOUNTANT | CORP_ROLE_TRADER)==0) {return refuse("You no longer have permission to manage that corporation's order.")}
    Ok(())
}
