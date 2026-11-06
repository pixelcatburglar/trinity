//! Trinity contracts: escrowed goods and money, with all writes committed together.
//! Wire columns and method signatures follow client contracts.py and journal.py.

use super::{
    rows::{key_val, row, rowset},
    wallet::{post, Entry, Transfer},
};
use crate::{
    items::{Item, ItemTxn, NewItem},
    services::{Call, Ctx, Outcome},
    store::{self, Db, Row, StoreError, Tx},
};
use eve_marshal::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

const DAY: i64 = 864_000_000_000;
const CONTRACT_MANAGER: i64 = 72_057_594_037_927_936;
const DELIVERIES_ROLES: i64 = 256 | 4_503_599_627_370_496 | 18_014_398_509_481_984 | 1;
const SYSTEM: i64 = 1;
const HEADER: &[&str] = &[
    "contractID",
    "type",
    "issuerID",
    "issuerCorpID",
    "forCorp",
    "availability",
    "assigneeID",
    "acceptorID",
    "dateIssued",
    "dateExpired",
    "dateAccepted",
    "numDays",
    "dateCompleted",
    "startStationID",
    "startSolarSystemID",
    "startRegionID",
    "endStationID",
    "endSolarSystemID",
    "endRegionID",
    "price",
    "reward",
    "collateral",
    "title",
    "description",
    "volume",
    "status",
    "issuerAllianceID",
    "issuerWalletKey",
    "acceptorWalletKey",
];
const ITEM_HEADER: &[&str] = &[
    "itemID",
    "typeID",
    "quantity",
    "inCrate",
    "parentID",
    "copy",
    "materialLevel",
    "productivityLevel",
    "bpRuns",
];
const BID_HEADER: &[&str] = &["bidderID", "amount", "dateBid"];

/// The contract tables are defined by the store's schema; there is nothing
/// left to bring in line at start-up.
pub fn ensure(_tx: &mut Tx<'_>) -> std::result::Result<(), StoreError> {
    Ok(())
}

fn int(n: i64) -> store::Value {
    store::Value::Int(n)
}

/// A contract row, read inside the transaction that acts on it.
struct Contract {
    id: i64,
    kind: i64,
    issuer_id: i64,
    issuer_corp_id: i64,
    for_corp: bool,
    availability: i64,
    assignee_id: i64,
    acceptor_id: i64,
    date_expired: i64,
    date_accepted: i64,
    num_days: i64,
    start_station_id: i64,
    end_station_id: i64,
    price: f64,
    reward: f64,
    collateral: f64,
    status: i64,
    acceptor_corp: bool,
    crate_id: i64,
    source_location: i64,
    source_flag: i64,
    deleted: bool,
    deposit: f64,
    sales_tax: f64,
    tax_rate: f64,
    tax_cap: f64,
}
impl Contract {
    fn read(r: &Row) -> Self {
        let number = |c: &str| r.int(c).unwrap_or(0);
        let amount = |c: &str| r.float(c).unwrap_or(0.0);
        let yes = |c: &str| r.bool(c).unwrap_or(false);
        Self {
            id: number("contractID"),
            kind: number("type"),
            issuer_id: number("issuerID"),
            issuer_corp_id: number("issuerCorpID"),
            for_corp: yes("forCorp"),
            availability: number("availability"),
            assignee_id: number("assigneeID"),
            acceptor_id: number("acceptorID"),
            date_expired: number("dateExpired"),
            date_accepted: number("dateAccepted"),
            num_days: number("numDays"),
            start_station_id: number("startStationID"),
            end_station_id: number("endStationID"),
            price: amount("price"),
            reward: amount("reward"),
            collateral: amount("collateral"),
            status: number("status"),
            acceptor_corp: yes("acceptorCorp"),
            crate_id: number("crateID"),
            source_location: number("sourceLocation"),
            source_flag: number("sourceFlag"),
            deleted: yes("deleted"),
            deposit: amount("deposit"),
            sales_tax: amount("salesTax"),
            tax_rate: amount("taxRate"),
            tax_cap: amount("taxCap"),
        }
    }
    /// The contract with this ID, deleted or not.
    fn get(db: &Db, id: i64) -> Option<Self> {
        db.table("contracts").get_id(id).map(|r| Self::read(&r))
    }
    fn issuer(&self) -> i64 {
        if self.for_corp {
            self.issuer_corp_id
        } else {
            self.issuer_id
        }
    }
}

/// A contract's bids, oldest first.
fn bids_of(db: &Db, contract: i64) -> impl Iterator<Item = Row<'_>> + '_ {
    db.table("contract_bids").find(&["contractID"], &[int(contract)])
}
fn has_bid(db: &Db, contract: i64, who: i64) -> bool {
    bids_of(db, contract).any(|b| b.int("bidderID") == Some(who))
}
/// Whether `who` should look at the contract: an auction it was outbid on,
/// one rejected, expired or overdue, unless it dismissed the notification.
fn attention(db: &Db, c: &Contract, who: i64, now: i64) -> bool {
    let involved = c.issuer() == who || c.acceptor_id == who || has_bid(db, c.id, who);
    let outbid = c.kind == 2
        && c.status == 0
        && has_bid(db, c.id, who)
        && bids_of(db, c.id).last().is_some_and(|b| b.int("bidderID") != Some(who));
    let due = outbid
        || c.status == 6
        || ([0, 2, 3].contains(&c.status) && c.date_expired <= now)
        || (c.status == 1 && c.date_accepted + c.num_days * DAY <= now);
    let dismissed = db.table("contract_notifications").get(&[int(c.id), int(who)]).is_some();
    !c.deleted && involved && due && !dismissed
}

async fn counters(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let me = owner(ctx, false)?;
    let corp = ctx.session.corp_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    ctx.store.read(|db| -> Result<Value> {
        let characters = db.table("character_");
        // Each contract the pilot or its corporation is party to, and whether
        // it is assigned within the corporation.
        let rs: Vec<(Contract, bool)> = db
            .table("contracts")
            .rows()
            .map(|r| Contract::read(&r))
            .filter(|c| {
                !c.deleted
                    && (c.issuer_id == me
                        || c.issuer_corp_id == corp
                        || [me, corp].contains(&c.assignee_id)
                        || [me, corp].contains(&c.acceptor_id))
            })
            .map(|c| {
                let internal = corp >= 100_000_000
                    && (c.assignee_id == corp
                        || characters.get_id(c.assignee_id).is_some_and(|a| a.int("corporationID") == Some(corp)));
                (c, internal)
            })
            .collect();
        let outstanding = |who: i64| {
            rs.iter()
                .filter(|(c, _)| c.issuer() == who && c.status == 0)
                .count() as i64
        };
        let assigned = |who: i64| {
            rs.iter()
                .filter(|(c, _)| c.assignee_id == who && c.status == 0 && c.date_expired > now)
                .count() as i64
        };
        let progress = |who: i64| {
            rs.iter()
                .filter(|(c, _)| c.acceptor_id == who && c.status == 1)
                .count() as i64
        };
        let mut attention = [0i64; 2];
        let mut bidding = [0i64; 2];
        let contracts: Vec<Contract> = db.table("contracts").rows().map(|r| Contract::read(&r)).collect();
        for (i, who) in [me, corp].iter().enumerate() {
            attention[i] = contracts.iter().filter(|c| self::attention(db, c, *who, now)).count() as i64;
            bidding[i] = contracts
                .iter()
                .filter(|c| !c.deleted && c.status == 0 && c.date_expired > now && has_bid(db, c.id, *who))
                .count() as i64;
        }
        let non_corp = rs
            .iter()
            .filter(|(c, internal)| !c.for_corp && c.issuer_id == me && !internal && c.status == 0)
            .count() as i64;
        let for_corp = rs
            .iter()
            .filter(|(c, internal)| c.for_corp && c.issuer_id == me && !internal && c.status == 0)
            .count() as i64;
        let values = match call.method.as_str() {
            "NumRequiringAttention" => vec![("n", attention[0]), ("ncorp", attention[1])],
            "NumOutstandingContracts" => vec![
                ("myCharTotal", outstanding(me)),
                ("myCorpTotal", outstanding(corp)),
                ("nonCorpForMyChar", non_corp),
                ("nonCorpForMyCorp", for_corp),
            ],
            _ => vec![
                ("numOutstandingContracts", outstanding(corp)),
                ("numOutstandingContractsNonCorp", non_corp),
                ("numOutstandingContractsForCorp", for_corp),
                ("numRequiresAttention", attention[0]),
                ("numRequiresAttentionCorp", attention[1]),
                ("numAssignedTo", assigned(me)),
                ("numAssignedToCorp", assigned(corp)),
                ("numBiddingOn", bidding[0]),
                ("numBiddingOnCorp", bidding[1]),
                ("numInProgress", progress(me)),
                ("numInProgressCorp", progress(corp)),
            ],
        };
        Ok(key_val(
            values
                .into_iter()
                .map(|(k, v)| (k, Value::Int(v)))
                .collect(),
        ))
    })
}

/// The office a corporation rents at a station.
fn office(db: &Db, station: i64, corp: i64) -> Option<i64> {
    db.table("crpOffices")
        .find(&["stationID"], &[int(station)])
        .find(|o| o.int("corporationID") == Some(corp))
        .and_then(|o| o.int("itemID"))
}

async fn inventory(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let station = call.arg_id(0);
    let container = call.method != "GetItemsInStation";
    let corp = flag(call, "forCorp", if container { 2 } else { 1 });
    let who = owner(ctx, corp)?;
    let deliveries = corp && ctx.session.corp_role() & DELIVERIES_ROLES != 0;
    ctx.store.read(|db| -> Result<Value> {
        let entity = db.table("entity");
        let mut location = station;
        if corp {
            location = office(db, station, who).unwrap_or(station);
        }
        if container {
            let id = call.arg_id(1);
            let r = entity.get_id(id);
            if !r.is_some_and(|r| {
                let (owner, at, flag) = (r.int("ownerID"), r.int("locationID"), r.int("flag").unwrap_or(0));
                owner == Some(who)
                    && (at == Some(location) || (deliveries && at == Some(station) && flag == 62))
                    && (!corp || super::corphangar::may_query(ctx, flag) || (deliveries && flag == 62))
            }) {
                return refuse("That container is not in your hangar at this station.");
            }
            location = id;
        }
        if call.method == "HasFittedCharges" {
            let charges = crate::sde::get().types_in_category(8);
            let r = entity.find(&["locationID"], &[int(location)]).find(|e| {
                e.int("typeID").is_some_and(|t| charges.contains(&(t as i32))) && e.int("flag").is_some_and(|f| (11..=34).contains(&f))
            });
            return Ok(r
                .and_then(|r| r.int("typeID"))
                .map(Value::Int)
                .unwrap_or(Value::None));
        }
        let mut rs: Vec<Row> = entity.find(&["ownerID", "locationID"], &[int(who), int(location)]).collect();
        if deliveries && !container {
            rs.extend(entity.find(&["ownerID", "locationID", "flag"], &[int(who), int(station), int(62)]));
        }
        rs.sort_by_key(|r| r.int("itemID"));
        rs.dedup_by_key(|r| r.id);
        let rs: Vec<Row> = rs
            .into_iter()
            .filter(|r| {
                let flag = r.int("flag").unwrap_or(0);
                container
                    || (!corp && flag == 4)
                    || (corp && super::corphangar::may_query(ctx, flag))
                    || (deliveries && flag == 62)
            })
            .collect();
        Ok(rowset(&crate::items::INVITEM_COLUMNS, crate::items::invitem_lines(rs)))
    })
}

async fn write(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let actor = owner(ctx, false)?;
    let (result, changes, fx) = ctx.store.write(|tx| -> Result<_> {
        if tx.table("character_").get_id(actor).is_none() {
            return refuse("That character no longer exists.");
        }
        let mut tx = ItemTxn::new(tx);
        let mut fx = Effects::default();
        let result = match call.method.as_str() {
            "CreateContract" => create(call, ctx, &mut tx, &mut fx)?,
            "SplitStack" => split(call, ctx, &mut tx)?,
            "DeleteNotification" => {
                let who = owner(ctx, flag(call, "forCorp", 1))?;
                let id = call.arg_id(0);
                if Contract::get(tx.db(), id).is_some() {
                    tx.tx().insert_or_ignore("contract_notifications", &[("contractID", int(id)), ("ownerID", int(who))])?;
                }
                Value::None
            }
            _ => {
                let r = Contract::get(tx.db(), call.arg_id(0))
                    .filter(|c| !c.deleted)
                    .ok_or_else(|| Failure::Text("That contract no longer exists.".into()))?;
                fx.owners.insert(r.issuer());
                match call.method.as_str() {
                    "AcceptContract" => accept(call, ctx, &mut tx, &mut fx, &r)?,
                    "PlaceBid" => bid(call, ctx, &mut tx, &mut fx, &r)?,
                    "FinishAuction" => finish_auction(call, ctx, &mut tx, &mut fx, &r)?,
                    _ => complete(call, ctx, &mut tx, &mut fx, &r)?,
                }
                Value::Bool(true)
            }
        };
        Ok((result, tx.events(), fx))
    })?;
    let w = ctx.world.lock();
    let mut clients = vec![ctx.client_id];
    for who in &fx.owners {
        clients.extend(w.clients_of(*who));
        clients.extend(w.corp_members(*who));
    }
    clients.sort_unstable();
    clients.dedup();
    for change in changes {
        w.notify(&clients, "OnItemChange", change);
    }
    for (who, balance) in fx.balances {
        let mut clients = w.clients_of(who);
        clients.extend(w.corp_members(who));
        w.notify(
            &clients,
            "OnAccountChange",
            Value::Tuple(vec![
                Value::bytes("cash"),
                Value::Int(who),
                Value::Double(balance),
            ]),
        );
    }
    if let Some((event, id, station)) = fx.event {
        let location = key_val(vec![
            ("locationID", Value::Int(station)),
            ("groupID", Value::Int(15)),
        ]);
        w.notify(
            &clients,
            event,
            Value::Tuple(vec![Value::Int(id), location]),
        );
    }
    Ok(result)
}

#[derive(Debug)]
struct NewContract {
    kind: i64,
    availability: i64,
    assignee: i64,
    expiry: i64,
    days: i64,
    start: i64,
    end: i64,
    price: f64,
    reward: f64,
    collateral: f64,
    title: String,
    description: String,
    items: Vec<(i64, i64)>,
    requests: Vec<(i64, i64)>,
    flag: i64,
    corp: bool,
}
fn pairs(value: Option<&Value>) -> Result<Vec<(i64, i64)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value
        .seq()
        .ok_or_else(|| Failure::Text("Invalid item list.".into()))?;
    if entries.len() > 100 {
        return refuse("A contract can contain at most 100 items.");
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for v in entries {
        let p = v
            .seq()
            .ok_or_else(|| Failure::Text("Invalid item list.".into()))?;
        let id = p.first().and_then(Value::as_i64).unwrap_or(0);
        let qty = p.get(1).and_then(Value::as_i64).unwrap_or(0);
        if p.len() != 2
            || id <= 0
            || id > i32::MAX as i64
            || qty <= 0
            || qty > i32::MAX as i64
            || !seen.insert(id)
        {
            return refuse("Contract items must have distinct IDs and positive quantities.");
        }
        out.push((id, qty));
    }
    out.sort_unstable();
    Ok(out)
}
impl NewContract {
    fn parse(call: &Call) -> Result<Self> {
        for index in [0, 1, 2, 4, 5, 6] {
            if !call
                .arg_i64(index)
                .is_some_and(|n| (0..=i32::MAX as i64).contains(&n))
            {
                return refuse("Invalid contract identifier or duration.");
            }
        }
        let n = Self {
            kind: call.arg_id(0),
            availability: call.arg_id(1),
            assignee: call.arg_id(2),
            expiry: call.arg_id(3),
            days: call.arg_id(4),
            start: call.arg_id(5),
            end: call.arg_id(6),
            price: call.args.get(7).and_then(Value::as_f64).unwrap_or(f64::NAN),
            reward: call.args.get(8).and_then(Value::as_f64).unwrap_or(f64::NAN),
            collateral: call.args.get(9).and_then(Value::as_f64).unwrap_or(f64::NAN),
            title: call.arg_text(10).unwrap_or_default(),
            description: call.arg_text(11).unwrap_or_default(),
            items: pairs(arg(call, "itemList", 12))?,
            flag: arg(call, "flag", 13).and_then(Value::as_i64).unwrap_or(4),
            requests: pairs(arg(call, "requestItemTypeList", 14))?,
            corp: flag(call, "forCorp", 15),
        };
        if !(1..=5).contains(&n.kind)
            || ![0, 1].contains(&n.availability)
            || n.assignee < 0
            || (n.availability == 1) != (n.assignee > 0)
        {
            return refuse("Invalid contract type or availability.");
        }
        if ![1440, 4320, 10080, 20160].contains(&n.expiry)
            || !(0..=365).contains(&n.days)
            || ([3, 4, 5].contains(&n.kind) && n.days == 0)
        {
            return refuse("Invalid contract expiry or duration.");
        }
        if [n.price, n.reward, n.collateral]
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1e12).contains(v))
        {
            return refuse("Contract amounts must be between 0 and one trillion ISK.");
        }
        if n.title.chars().count() > 50 || n.description.chars().count() > 4000 {
            return refuse("The contract title or description is too long.");
        }
        if n.kind != 1 && !n.requests.is_empty() {
            return refuse("Only item exchanges can request items.");
        }
        if ([2, 3].contains(&n.kind) || (n.kind == 4 && n.reward == 0.0)) && n.items.is_empty() {
            return refuse("Select at least one item for this contract.");
        }
        if n.kind == 2 && (n.reward > 0.0 || (n.collateral > 0.0 && n.collateral < n.price)) {
            return refuse("The auction buyout must cover its starting price.");
        }
        if n.kind == 5 && (!n.items.is_empty() || n.price != 0.0 || n.start != 0) {
            return refuse("Freeform contracts may contain a reward and collateral, but no items.");
        }
        Ok(n)
    }
}

fn station(db: &Db, id: i64) -> Result<(i64, i64)> {
    let r = db
        .table("staStations")
        .get_id(id)
        .ok_or_else(|| Failure::Text("Select a valid station.".into()))?;
    Ok((r.int("solarSystemID").unwrap_or(0), r.int("regionID").unwrap_or(0)))
}
fn source(
    db: &Db,
    ctx: &Ctx<'_>,
    station: i64,
    who: i64,
    corp: bool,
    flag: i64,
) -> Result<i64> {
    if !corp {
        if flag != 4 {
            return refuse("Items must be in your personal hangar.");
        }
        return Ok(station);
    }
    if flag == 62 {
        if ctx.session.corp_role() & DELIVERIES_ROLES == 0 {
            return refuse("You cannot take items from corporation deliveries.");
        }
        return Ok(station);
    }
    if !super::corphangar::may_take(ctx, flag) {
        return refuse("You cannot take items from that corporation division.");
    }
    office(db, station, who)
        .ok_or_else(|| Failure::Text("Your corporation has no office at that station.".into()))
}
fn portion(tx: &mut ItemTxn<'_, '_>, item: &Item, qty: i64) -> Result<i64> {
    if qty < 1 || qty > item.quantity || (item.singleton && qty != item.quantity) {
        return refuse("The selected quantity is no longer available.");
    }
    if qty == item.quantity {
        return Ok(item.id);
    }
    tx.set_quantity(item.id, item.quantity - qty)?;
    Ok(tx.create(NewItem {
        type_id: item.type_id,
        owner_id: item.owner_id,
        location_id: item.location_id,
        flag: item.flag,
        quantity: qty,
        singleton: false,
    })?)
}

/// Set columns of a contract (nothing if it is gone, as an UPDATE matching
/// no row did).
fn set(tx: &mut Tx<'_>, id: i64, sets: store::Sets) -> std::result::Result<(), StoreError> {
    match tx.table("contracts").get_id(id).map(|r| r.id) {
        Some(row) => tx.update("contracts", row, sets),
        None => Ok(()),
    }
}

fn create(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
) -> Result<Value> {
    let n = NewContract::parse(call)?;
    let who = owner(ctx, n.corp)?;
    // One write runs at a time, so this count cannot be raced past the cap.
    let count = tx
        .db()
        .table("contracts")
        .rows()
        .map(|r| Contract::read(&r))
        .filter(|c| !c.deleted && c.status == 0 && c.issuer() == who)
        .count();
    if count >= 500 {
        return refuse("You have reached the outstanding contract limit.");
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let skills = super::skills::levels(tx.db(), me, &[25235, 25233, 3446, 16622])?;
    let corp = ctx.session.corp_id.unwrap_or(0);
    let characters = tx.db().table("character_");
    let in_corp = |id: i64| characters.get_id(id).is_some_and(|a| a.int("corporationID") == Some(corp));
    let internal = n.assignee > 0 && corp >= 100_000_000 && (n.assignee == corp || in_corp(n.assignee));
    if !internal {
        let active = tx
            .db()
            .table("contracts")
            .rows()
            .map(|r| Contract::read(&r))
            .filter(|c| {
                !c.deleted
                    && c.status == 0
                    && c.issuer_id == me
                    && c.for_corp == n.corp
                    && !(c.assignee_id == corp && corp >= 100_000_000)
                    && !(corp >= 100_000_000 && in_corp(c.assignee_id))
            })
            .count() as i64;
        let level = skills[&if n.corp { 25233 } else { 25235 }].clamp(0, 5);
        let limit = if n.corp {
            10 + 10 * level
        } else {
            1 + 4 * level
        };
        if active >= limit {
            return refuse("Train Contracting or Corporation Contracting to create more outstanding contracts.");
        }
    }
    let fees = fees(
        n.kind,
        n.availability,
        n.price.max(n.reward),
        n.expiry,
        skills[&3446],
        skills[&16622],
    );
    let (start_system, start_region) = if n.kind == 5 {
        (
            ctx.session.solar_system_id2.unwrap_or(0),
            ctx.session.region_id.unwrap_or(0),
        )
    } else {
        station(tx.db(), n.start)?
    };
    let (end_system, end_region) = if n.end > 0 {
        station(tx.db(), n.end)?
    } else {
        (0, 0)
    };
    if n.kind == 3 && n.end == 0 {
        return refuse("Choose a delivery station.");
    }
    if n.assignee > 0 {
        let db = tx.db();
        let exists = db.table("eveStaticOwners").get_id(n.assignee).is_some()
            || db.table("character_").get_id(n.assignee).is_some()
            || db.table("corporation").get_id(n.assignee).is_some()
            || db.table("alliances").get_id(n.assignee).is_some();
        if !exists {
            return refuse("That assignee does not exist.");
        }
    }
    let source = if n.kind == 5 {
        0
    } else {
        source(tx.db(), ctx, n.start, who, n.corp, n.flag)?
    };
    let now = crate::bulkdata::filetime_now();
    let expires = now + n.expiry * 600_000_000;
    let text = |s: &str| store::Value::Text(s.into());
    let row = tx.tx().insert(
        "contracts",
        &[
            ("type", int(n.kind)),
            ("issuerID", int(ctx.session.char_id.unwrap_or(0))),
            ("issuerCorpID", int(ctx.session.corp_id.unwrap_or(0))),
            ("forCorp", store::Value::Bool(n.corp)),
            ("availability", int(n.availability)),
            ("assigneeID", int(n.assignee)),
            ("dateIssued", int(now)),
            ("dateExpired", int(expires)),
            ("numDays", int(n.days)),
            ("startStationID", int(n.start)),
            ("startSolarSystemID", int(start_system)),
            ("startRegionID", int(start_region)),
            ("endStationID", int(n.end)),
            ("endSolarSystemID", int(end_system)),
            ("endRegionID", int(end_region)),
            ("price", store::Value::Float(n.price)),
            ("reward", store::Value::Float(n.reward)),
            ("collateral", store::Value::Float(n.collateral)),
            ("title", text(&n.title)),
            ("description", text(&n.description)),
            ("issuerAllianceID", int(ctx.session.alliance_id.unwrap_or(0))),
            ("sourceLocation", int(source)),
            ("sourceFlag", int(n.flag)),
        ],
    )?;
    let id = tx.db().table("contracts").row(row).and_then(|r| r.int("contractID")).unwrap_or(0);
    let mut volume = 0.0;
    for (item_id, qty) in n.items {
        let item = tx
            .item(item_id)?
            .ok_or_else(|| Failure::Text("An offered item no longer exists.".into()))?;
        if item.owner_id != who
            || item.location_id != source
            || item.flag != n.flag
            || ctx.session.ship_id == Some(item_id)
        {
            return refuse("An offered item is not available in this hangar.");
        }
        let item_id = portion(tx, &item, qty)?;
        volume += item.volume * qty as f64;
        snapshot(tx.tx(), id, item_id, 0)?;
        let children = descendants(tx.db(), item_id);
        for child in children {
            let item = tx
                .item(child)?
                .ok_or_else(|| Failure::Text("An item in that container is missing.".into()))?;
            if item.owner_id != who {
                return refuse("That container holds goods belonging to another owner.");
            }
            snapshot(tx.tx(), id, child, item.location_id)?;
            tx.set_owner(child, SYSTEM)?;
        }
        tx.set_owner(item_id, SYSTEM)?;
        tx.move_to(item_id, id, 4)?;
    }
    if n.kind == 3 && volume > 120_000.0 {
        return refuse("Courier packages cannot exceed 120,000 cubic metres.");
    }
    for (ty, qty) in n.requests {
        if crate::sde::table("invTypes").get("typeID", ty).is_none() {
            return refuse("A requested item type does not exist.");
        }
        tx.tx().insert(
            "contract_items",
            &[
                ("contractID", int(id)),
                ("itemID", int(0)),
                ("typeID", int(ty)),
                ("quantity", int(qty)),
                ("inCrate", store::Value::Bool(false)),
            ],
        )?;
    }
    set(tx.tx(), id, &[("volume", store::Value::Float(volume))])?;
    money(tx, fx, who, -n.reward, id, 83)?;
    money(tx, fx, who, -fees.broker, id, 72)?;
    money(tx, fx, who, -fees.deposit, id, 74)?;
    set(
        tx.tx(),
        id,
        &[
            ("deposit", store::Value::Float(fees.deposit)),
            ("salesTax", store::Value::Float(fees.tax)),
            ("taxRate", store::Value::Float(fees.rate)),
            ("taxCap", store::Value::Float(fees.cap)),
        ],
    )?;
    fx.owners.insert(who);
    if n.assignee > 0 {
        fx.owners.insert(n.assignee);
        fx.event = Some(("OnContractAssigned", id, n.start));
    }
    Ok(Value::Int(id))
}

struct Fees {
    broker: f64,
    deposit: f64,
    tax: f64,
    rate: f64,
    cap: f64,
}
// contractscommon.CalcContractFees, including its truncation before the duration multiplier.
fn fees(kind: i64, private: i64, base: f64, minutes: i64, broker: i64, accounting: i64) -> Fees {
    if private != 0 {
        return Fees {
            broker: 10_000.0,
            deposit: 0.0,
            tax: 0.0,
            rate: 0.0,
            cap: 0.0,
        };
    }
    let multiplier = if kind == 3 { 0.1 } else { 1.0 };
    let rate = (0.01 - accounting.clamp(0, 5) as f64 * 0.001) * multiplier;
    let cap = 100_000_000.0 * (1.0 - accounting.clamp(0, 5) as f64 * 0.1);
    let broker_rate = (0.004 - broker.clamp(0, 5) as f64 * 0.0002) * multiplier;
    let broker_cap = 10_000_000.0 * (1.0 - broker.clamp(0, 5) as f64 * 0.05);
    Fees {
        broker: (base * broker_rate).clamp(10_000.0, broker_cap).floor()
            * (1.0 + (minutes / 1440 - 1) as f64 * 0.05),
        deposit: (base * 0.01 * multiplier)
            .clamp(if kind == 3 { 10_000.0 } else { 100_000.0 }, 10_000_000.0)
            .floor(),
        tax: if kind == 3 {
            0.0
        } else {
            (base * rate).clamp(10_000.0, cap).floor()
        },
        rate: if kind == 3 { 0.0 } else { rate },
        cap,
    }
}
fn settle_fees(
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    r: &Contract,
    auction: Option<f64>,
) -> Result<()> {
    let id = r.id;
    let tax = if let Some(amount) = auction {
        if r.tax_rate > 0.0 {
            (amount * r.tax_rate)
                .clamp(10_000.0, r.tax_cap)
                .floor()
        } else {
            0.0
        }
    } else {
        r.sales_tax
    };
    money(tx, fx, r.issuer(), r.deposit, id, 82)?;
    money(tx, fx, r.issuer(), -tax, id, 73)?;
    set(
        tx.tx(),
        id,
        &[("deposit", store::Value::Float(0.0)), ("salesTax", store::Value::Float(0.0))],
    )?;
    Ok(())
}

/// Record an item as the contract holds it, with what its blueprint row says
/// (nothing if the item is gone).
fn snapshot(tx: &mut Tx<'_>, contract: i64, item: i64, parent: i64) -> Result<()> {
    let Some(e) = tx.table("entity").get_id(item) else { return Ok(()) };
    let blueprint = tx.table("invBlueprints").get_id(item);
    let bp = |c: &str| blueprint.map_or(store::Value::Null, |b| b.value(c).clone());
    let sets = [
        ("contractID", int(contract)),
        ("itemID", e.value("itemID").clone()),
        ("typeID", e.value("typeID").clone()),
        ("quantity", e.value("quantity").clone()),
        ("inCrate", store::Value::Bool(true)),
        ("parentID", int(parent)),
        ("copy", bp("copy")),
        ("materialLevel", bp("materialLevel")),
        ("productivityLevel", bp("productivityLevel")),
        ("bpRuns", bp("licensedProductionRunsRemaining")),
    ];
    tx.insert("contract_items", &sets)?;
    Ok(())
}
/// Everything inside `item`, however deep, by itemID.
fn descendants(db: &Db, item: i64) -> Vec<i64> {
    let entity = db.table("entity");
    let mut found = BTreeSet::new();
    let mut pending = vec![item];
    while let Some(at) = pending.pop() {
        for e in entity.find(&["locationID"], &[int(at)]) {
            let Some(id) = e.int("itemID") else { continue };
            if found.insert(id) {
                pending.push(id);
            }
        }
    }
    found.into_iter().collect()
}
/// A contract's crated items, by itemID.
fn crated(db: &Db, contract: i64) -> Vec<Row<'_>> {
    let mut rs: Vec<Row> = db
        .table("contract_items")
        .find(&["contractID"], &[int(contract)])
        .filter(|r| r.bool("inCrate") == Some(true))
        .collect();
    rs.sort_by_key(|r| r.int("itemID"));
    rs
}
fn give_goods(
    tx: &mut ItemTxn<'_, '_>,
    id: i64,
    who: i64,
    location: i64,
    flag: i64,
) -> Result<()> {
    let rs: Vec<(i64, i64)> = crated(tx.db(), id)
        .iter()
        .map(|r| (r.int("itemID").unwrap_or(0), r.int("parentID").unwrap_or(0)))
        .collect();
    for (item, parent) in rs {
        tx.set_owner(item, who)?;
        if parent == 0 {
            tx.move_to(item, location, flag)?;
        }
    }
    Ok(())
}
fn split(call: &Call, ctx: &Ctx<'_>, tx: &mut ItemTxn<'_, '_>) -> Result<Value> {
    let corp = flag(call, "forCorp", 3);
    let who = owner(ctx, corp)?;
    let flag = call.arg_i64(4).unwrap_or(4);
    let location = source(tx.db(), ctx, call.arg_id(0), who, corp, flag)?;
    let item = tx
        .item(call.arg_id(1))?
        .ok_or_else(|| Failure::Text("That item no longer exists.".into()))?;
    if item.owner_id != who
        || item.location_id != location
        || item.flag != flag
        || item.singleton
        || call.arg_id(2) >= item.quantity
    {
        return refuse("That stack cannot be split.");
    }
    Ok(Value::Int(portion(tx, &item, call.arg_id(2))?))
}

fn may_accept(ctx: &Ctx<'_>, r: &Contract, who: i64) -> Result<()> {
    if r.status != 0 || r.date_expired <= crate::bulkdata::filetime_now()
    {
        return refuse("This contract is no longer available.");
    }
    if r.issuer() == who {
        return refuse("You cannot accept your own contract.");
    }
    let assigned = r.assignee_id;
    if r.availability != 0
        && assigned != who
        && Some(assigned) != ctx.session.corp_id
        && Some(assigned) != ctx.session.alliance_id
    {
        return refuse("This private contract is assigned to somebody else.");
    }
    Ok(())
}
fn status(tx: &mut ItemTxn<'_, '_>, id: i64, state: i64) -> Result<()> {
    let completed = if state >= 4 {
        crate::bulkdata::filetime_now()
    } else {
        0
    };
    set(tx.tx(), id, &[("status", int(state)), ("dateCompleted", int(completed))])?;
    Ok(())
}
fn accept(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    r: &Contract,
) -> Result<()> {
    let corp = flag(call, "forCorp", 1);
    let who = owner(ctx, corp)?;
    may_accept(ctx, r, who)?;
    let id = r.id;
    let seller = r.issuer();
    let kind = r.kind;
    let start = r.start_station_id;
    if kind == 2 {
        return refuse("Place a bid to accept an auction.");
    }
    if kind == 1 {
        let mut requests: Vec<(i64, i64)> = tx
            .db()
            .table("contract_items")
            .find(&["contractID"], &[int(id)])
            .filter(|r| r.bool("inCrate") == Some(false))
            .map(|r| (r.int("typeID").unwrap_or(0), r.int("quantity").unwrap_or(0)))
            .collect();
        requests.sort_by_key(|(ty, _)| *ty);
        if corp && !requests.is_empty() && ctx.session.corp_role() & DELIVERIES_ROLES == 0 {
            return refuse("You cannot take the requested items from corporation deliveries.");
        }
        for (ty, quantity) in requests {
            let mut remaining = quantity;
            // Requested goods come from the accepting owner's station hangar/deliveries.
            let mut rs: Vec<i64> = tx
                .db()
                .table("entity")
                .find(&["ownerID", "locationID", "flag"], &[int(who), int(start), int(delivery(corp))])
                .filter(|e| e.int("typeID") == Some(ty) && e.int("singleton") == Some(0))
                .filter_map(|e| e.int("itemID"))
                .collect();
            rs.sort_unstable();
            for item_id in rs {
                if remaining == 0 {
                    break;
                }
                let item = tx.item(item_id)?.ok_or_else(|| {
                    Failure::Text("A requested item is no longer available.".into())
                })?;
                let qty = remaining.min(item.quantity);
                let item_id = portion(tx, &item, qty)?;
                tx.set_owner(item_id, seller)?;
                tx.move_to(item_id, start, delivery(r.for_corp))?;
                remaining -= qty;
            }
            if remaining > 0 {
                return refuse(
                    "The requested goods are not available in your hangar at the pickup station.",
                );
            }
        }
    }
    let price = r.price;
    let collateral = r.collateral;
    if kind == 1 || kind == 4 {
        money(tx, fx, who, -price, id, 71)?;
        money(tx, fx, seller, price, id, 71)?;
    }
    if [3, 4, 5].contains(&kind) {
        money(tx, fx, who, -collateral, id, 65)?;
    }
    if kind == 1 || kind == 4 {
        money(tx, fx, who, r.reward, id, 68)?;
    }
    if kind == 1 {
        settle_fees(tx, fx, r, None)?;
    }
    if kind == 3 {
        let crate_id = tx.create(NewItem {
            type_id: super::missions::TYPE_PLASTIC_WRAP,
            owner_id: who,
            location_id: start,
            flag: delivery(corp),
            quantity: 1,
            singleton: true,
        })?;
        give_goods(tx, id, who, crate_id, 5)?;
        set(tx.tx(), id, &[("crateID", int(crate_id))])?;
    } else if kind != 5 {
        give_goods(tx, id, who, start, delivery(corp))?;
    }
    set(
        tx.tx(),
        id,
        &[
            ("acceptorID", int(who)),
            ("acceptorCorp", store::Value::Bool(corp)),
            ("dateAccepted", int(crate::bulkdata::filetime_now())),
        ],
    )?;
    status(tx, id, if kind == 1 { 4 } else { 1 })?;
    fx.owners.insert(who);
    fx.event = Some(("OnContractAccepted", id, start));
    Ok(())
}

fn next_bid(price: f64, current: Option<f64>, buyout: f64) -> f64 {
    let bid = price.max(current.unwrap_or(0.0) + (0.1 * price).floor().max(1000.0));
    if buyout > 0.0 {
        bid.min(buyout)
    } else {
        bid
    }
}
fn bid(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    r: &Contract,
) -> Result<()> {
    let corp = flag(call, "forCorp", 2);
    let who = owner(ctx, corp)?;
    may_accept(ctx, r, who)?;
    if r.kind != 2 {
        return refuse("Only auctions accept bids.");
    }
    let id = r.id;
    let amount = call.args.get(1).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let previous = bids_of(tx.db(), id)
        .last()
        .map(|b| (b.int("bidderID").unwrap_or(0), b.float("amount").unwrap_or(0.0)));
    let buyout = r.collateral;
    let minimum = next_bid(r.price, previous.map(|(_, amount)| amount), buyout);
    if !amount.is_finite() || amount < minimum || amount > 1e12 || (buyout > 0.0 && amount > buyout)
    {
        return refuse("The bid is below the minimum or above the buyout price. Refresh the contract and try again.");
    }
    if let Some((old_owner, old_amount)) = previous {
        money(tx, fx, old_owner, old_amount, id, 64)?;
        fx.owners.insert(old_owner);
        fx.event = Some(("OnContractOutbid", id, r.start_station_id));
    }
    money(tx, fx, who, -amount, id, 63)?;
    let now = crate::bulkdata::filetime_now();
    tx.tx().insert(
        "contract_bids",
        &[
            ("contractID", int(id)),
            ("bidderID", int(who)),
            ("amount", store::Value::Float(amount)),
            ("dateBid", int(now)),
            ("forCorp", store::Value::Bool(corp)),
        ],
    )?;
    tx.tx().delete_where("contract_notifications", &["contractID"], &[int(id)])?;
    if buyout > 0.0 && amount >= buyout {
        set(tx.tx(), id, &[("dateExpired", int(now))])?;
        fx.event = Some(("OnContractBuyout", id, r.start_station_id));
    }
    Ok(())
}
fn finish_auction(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    r: &Contract,
) -> Result<()> {
    let id = r.id;
    let state = r.status;
    if r.kind != 2
        || ![0, 2, 3].contains(&state)
        || r.date_expired > crate::bulkdata::filetime_now()
    {
        return refuse("This auction is not ready to settle.");
    }
    let (winner, amount, for_corp) = bids_of(tx.db(), id)
        .last()
        .map(|b| (b.int("bidderID").unwrap_or(0), b.float("amount").unwrap_or(0.0), b.bool("forCorp").unwrap_or(false)))
        .ok_or_else(|| Failure::Text("This auction has no bids; the issuer may cancel it to reclaim the items.".into()))?;
    let is_issuer = flag(call, "isIssuer", 1);
    if is_issuer {
        if owner(ctx, r.for_corp)? != r.issuer() || state == 2 {
            return refuse("You cannot collect this auction's money.");
        }
        money(tx, fx, r.issuer(), amount, id, 67)?;
        settle_fees(tx, fx, r, Some(amount))?;
    } else {
        if owner(ctx, for_corp)? != winner || state == 3 {
            return refuse("Only the winning bidder can collect these items.");
        }
        give_goods(
            tx,
            id,
            winner,
            r.start_station_id,
            delivery(for_corp),
        )?;
    }
    set(
        tx.tx(),
        id,
        &[
            ("acceptorID", int(winner)),
            ("acceptorCorp", store::Value::Bool(for_corp)),
            ("dateAccepted", int(crate::bulkdata::filetime_now())),
        ],
    )?;
    status(
        tx,
        id,
        if state != 0 {
            4
        } else if is_issuer {
            2
        } else {
            3
        },
    )?;
    fx.owners.insert(winner);
    fx.event = Some(("OnContractCompleted", id, r.start_station_id));
    Ok(())
}

fn complete(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    r: &Contract,
) -> Result<()> {
    let id = r.id;
    let seller = r.issuer();
    let state = r.status;
    let kind = r.kind;
    let me = ctx.session.char_id.unwrap_or(0);
    let is_seller = if r.for_corp {
        ctx.session.corp_id == Some(seller) && owner(ctx, true).is_ok()
    } else {
        me == seller
    };
    let buyer = r.acceptor_id;
    let is_buyer = if r.acceptor_corp {
        ctx.session.corp_id == Some(buyer) && owner(ctx, true).is_ok()
    } else {
        me == buyer
    };
    let deleting = call.method == "DeleteContract";
    let requested = if deleting { 5 } else { call.arg_id(1) };
    let start = r.start_station_id;
    if deleting || requested == 6 {
        if ![0, 6].contains(&state) {
            return refuse("An accepted or completed contract cannot be cancelled.");
        }
        if deleting && !is_seller {
            return refuse("Only the issuer may delete this contract.");
        }
        if requested == 6 {
            let assignee = r.assignee_id;
            if state != 0
                || !(assignee == me
                    || (ctx.session.corp_id == Some(assignee) && owner(ctx, true).is_ok()))
            {
                return refuse(
                    "Only the assigned character or corporation may reject this contract.",
                );
            }
        }
        if bids_of(tx.db(), id).next().is_some() {
            return refuse("An auction with bids cannot be cancelled or rejected.");
        }
        if state == 0 {
            give_goods(
                tx,
                id,
                seller,
                r.source_location,
                r.source_flag,
            )?;
            money(tx, fx, seller, r.reward, id, 66)?;
        }
        status(tx, id, requested)?;
        if deleting {
            set(tx.tx(), id, &[("deleted", store::Value::Bool(true))])?;
        }
        fx.event = Some(("OnContractRejected", id, start));
        return Ok(());
    }
    if ![1, 3].contains(&state) || ![3, 4, 5].contains(&kind) || !(is_seller || is_buyer) {
        return refuse("You cannot complete or fail this contract.");
    }
    if requested == 7 {
        let overdue = r.date_accepted + r.num_days * DAY
            <= crate::bulkdata::filetime_now();
        if is_seller && !is_buyer && !overdue && !(kind == 5 && state == 3) {
            return refuse("The contractor still has time to complete this contract.");
        }
        if kind != 4 {
            money(tx, fx, seller, r.reward, id, 66)?;
        }
        money(tx, fx, seller, r.collateral, id, 70)?;
        status(tx, id, 7)?;
        fx.event = Some(("OnContractFailed", id, start));
        return Ok(());
    }
    if requested != 4 {
        return refuse("Invalid contract completion status.");
    }
    if kind == 5 && is_buyer && !is_seller {
        if state != 1 {
            return refuse("The issuer must confirm that the work is complete.");
        }
        status(tx, id, 3)?;
        fx.event = Some(("OnContractCompleted", id, start));
        return Ok(());
    }
    if kind != 5 && !is_buyer {
        return refuse("Only the accepting contractor can deliver these goods.");
    }
    // The crated items: (itemID, parentID, quantity), by itemID.
    let rs: Vec<(i64, i64, i64)> = crated(tx.db(), id)
        .iter()
        .map(|r| (r.int("itemID").unwrap_or(0), r.int("parentID").unwrap_or(0), r.int("quantity").unwrap_or(0)))
        .collect();
    if kind == 3 {
        let crate_id = r.crate_id;
        let dest = r.end_station_id;
        let package = tx.item(crate_id)?.ok_or_else(|| {
            Failure::Text("The courier package is missing or has been destroyed.".into())
        })?;
        let in_ship = ctx.session.station_id == Some(dest)
            && ctx.session.ship_id == Some(package.location_id)
            && package.flag == 5;
        if package.owner_id != buyer || (package.location_id != dest && !in_ship) {
            return refuse("Deliver the courier package to the destination station first.");
        }
        // Every original item and its quantity must still be sealed in the package.
        let expected: HashSet<i64> = rs.iter().map(|(item, _, _)| *item).collect();
        let contents: HashSet<i64> = descendants(tx.db(), crate_id).into_iter().collect();
        if contents != expected {
            return refuse("The courier package has been opened or damaged.");
        }
        for (item_id, parent, quantity) in &rs {
            let item = tx.item(*item_id)?.ok_or_else(|| {
                Failure::Text("The courier package has been opened or damaged.".into())
            })?;
            let expected = if *parent == 0 { crate_id } else { *parent };
            if item.owner_id != buyer
                || item.location_id != expected
                || item.quantity != *quantity
            {
                return refuse("The courier package has been opened or damaged.");
            }
        }
        give_goods(tx, id, seller, dest, delivery(r.for_corp))?;
        tx.destroy(crate_id)?;
    }
    if kind == 4 {
        let expected: HashSet<i64> = rs.iter().map(|(item, _, _)| *item).collect();
        for (root, _, _) in rs.iter().filter(|(_, parent, _)| *parent == 0) {
            let contents = descendants(tx.db(), *root);
            if contents.iter().any(|item| !expected.contains(item)) {
                return refuse(
                    "Remove your additional cargo and fittings before returning the loaned items.",
                );
            }
        }
        for (item_id, parent, promised) in &rs {
            let item = tx
                .item(*item_id)?
                .ok_or_else(|| Failure::Text("A loaned item has not been returned.".into()))?;
            if item.owner_id != buyer
                || item.quantity < *promised
                || item.location_id != if *parent == 0 { start } else { *parent }
                || ctx.session.ship_id == Some(item.id)
            {
                return refuse("Return the loaned items to the original station hangar first.");
            }
            if item.quantity > *promised {
                let remainder = portion(tx, &item, item.quantity - promised)?;
                tx.move_to(remainder, start, delivery(r.acceptor_corp))?;
            }
        }
        give_goods(
            tx,
            id,
            seller,
            r.source_location,
            r.source_flag,
        )?;
    }
    if kind == 4 {
        money(tx, fx, buyer, -r.reward, id, 71)?;
        money(tx, fx, seller, r.reward, id, 71)?;
    } else {
        money(tx, fx, buyer, r.reward, id, 68)?;
    }
    money(tx, fx, buyer, r.collateral, id, 69)?;
    settle_fees(tx, fx, r, None)?;
    status(tx, id, 4)?;
    fx.owners.insert(buyer);
    fx.event = Some(("OnContractCompleted", id, start));
    Ok(())
}

#[derive(Debug)]
enum Failure {
    Text(String),
    Db(StoreError),
    Money { need: f64, have: f64 },
}
impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Self::Db(e)
    }
}
impl From<Transfer> for Failure {
    fn from(e: Transfer) -> Self {
        match e {
            Transfer::Db(e) => Self::Db(e),
            Transfer::NotEnough { need, have } => Self::Money { need, have },
            Transfer::NoSuchOwner => Self::Text("That wallet no longer exists.".into()),
        }
    }
}
type Result<T> = std::result::Result<T, Failure>;
fn refuse<T>(text: &str) -> Result<T> {
    Err(Failure::Text(text.into()))
}
fn owner(ctx: &Ctx<'_>, corp: bool) -> Result<i64> {
    if corp
        && ctx.session.corp_role() & (CONTRACT_MANAGER | crate::session::CORP_ROLE_DIRECTOR) == 0
    {
        return refuse("You need the Contract Manager role to act for your corporation.");
    }
    if corp {
        ctx.session.corp_id
    } else {
        ctx.session.char_id
    }
    .ok_or_else(|| Failure::Text("Select a character first.".into()))
}
fn arg<'a>(call: &'a Call, name: &str, index: usize) -> Option<&'a Value> {
    call.kwarg(name).or(call.args.get(index))
}
fn flag(call: &Call, name: &str, index: usize) -> bool {
    arg(call, name, index).and_then(Value::as_i64).unwrap_or(0) != 0
}
fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    match v {
        Value::Instance { state, .. } => field(state, name),
        _ => v.get_str(name),
    }
}
fn delivery(corp: bool) -> i64 {
    if corp {
        62
    } else {
        4
    }
}

#[derive(Default)]
struct Effects {
    balances: HashMap<i64, f64>,
    owners: HashSet<i64>,
    event: Option<(&'static str, i64, i64)>,
}
fn money(
    tx: &mut ItemTxn<'_, '_>,
    fx: &mut Effects,
    who: i64,
    delta: f64,
    contract: i64,
    reference: i32,
) -> Result<()> {
    if delta == 0.0 {
        return Ok(());
    }
    let id = contract.to_string();
    let entry = Entry {
        ref_type: reference,
        payer: if delta < 0.0 { who } else { 0 },
        payee: if delta > 0.0 { who } else { 0 },
        arg_id1: &id,
        reason: "Contract",
    };
    fx.balances
        .insert(who, post(tx.tx(), who, delta, &entry)?);
    fx.owners.insert(who);
    Ok(())
}

pub async fn contract_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "GetContract" => detail(ctx, call.arg_id(0)).await,
        "GetContractList"
        | "GetContractListForOwner"
        | "GetMyExpiredContractList"
        | "GetMyCurrentContractList"
        | "GetMyBids" => list(call, ctx).await,
        "NumRequiringAttention" | "NumOutstandingContracts" | "CollectMyPageInfo" => {
            counters(call, ctx).await
        }
        "GetItemsInStation" | "GetItemsInContainer" | "HasFittedCharges" => {
            inventory(call, ctx).await
        }
        "CreateContract" | "AcceptContract" | "DeleteContract" | "CompleteContract"
        | "PlaceBid" | "FinishAuction" | "DeleteNotification" | "SplitStack" => {
            write(call, ctx).await
        }
        _ => return None,
    };
    Some(match result {
        Ok(v) => Outcome::Value(v),
        Err(Failure::Text(t)) => Outcome::custom(t),
        Err(Failure::Money { need, have }) => Outcome::not_enough_money(need, have),
        Err(Failure::Db(e)) => {
            tracing::error!("contract {}: {}", call.method, crate::logging::chain(&e));
            Outcome::custom("The contract could not be processed. Please try again.")
        }
    })
}

/// Whether the caller may see the contract: a public one, or one it (or its
/// corporation or alliance) is party to.
fn visible(ctx: &Ctx<'_>, c: &Contract) -> bool {
    let me = ctx.session.char_id.unwrap_or(-1);
    let corp = ctx.session.corp_id.unwrap_or(-1);
    let alliance = ctx.session.alliance_id.unwrap_or(-1);
    c.availability == 0
        || [me, corp, alliance].contains(&c.assignee_id)
        || c.issuer_id == me
        || (c.for_corp && c.issuer_corp_id == corp)
        || [me, corp].contains(&c.acceptor_id)
}
fn item_rows(db: &Db, id: i64) -> Value {
    let mut rs: Vec<Row> = db.table("contract_items").find(&["contractID"], &[int(id)]).collect();
    // Crated items first, each kind in the order it was recorded.
    rs.sort_by_key(|r| (r.bool("inCrate") != Some(true), r.int("id")));
    rowset(ITEM_HEADER, rs.iter().map(|r| Value::List(r.line(ITEM_HEADER))).collect())
}
fn bid_rows(db: &Db, id: i64) -> Value {
    let rs: Vec<Row> = bids_of(db, id).collect();
    rowset(BID_HEADER, rs.iter().rev().map(|r| Value::List(r.line(BID_HEADER))).collect())
}
async fn detail(ctx: &Ctx<'_>, id: i64) -> Result<Value> {
    Ok(ctx.store.read(|db| {
        let Some(r) = db.table("contracts").get_id(id) else {
            return Value::None;
        };
        let c = Contract::read(&r);
        if c.deleted || !visible(ctx, &c) {
            return Value::None;
        }
        key_val(vec![
            ("contract", row(HEADER, r.line(HEADER))),
            ("items", item_rows(db, id)),
            ("bids", bid_rows(db, id)),
        ])
    }))
}

/// A condition `list` puts on contracts.
type Condition<'a> = Box<dyn Fn(&Db, &Row, &Contract) -> bool + 'a>;

async fn list(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let me = owner(ctx, false)?;
    let mut q: Vec<Condition> = vec![Box::new(|_, _, c| !c.deleted && visible(ctx, c))];
    let now = crate::bulkdata::filetime_now();
    match call.method.as_str() {
        "GetContractList" => {
            q.push(Box::new(move |_, _, c| c.status == 0 && c.date_expired > now));
            if let Some(filter) = call.args.first() {
                for (name, column) in [
                    ("type", "type"),
                    ("assigneeID", "assigneeID"),
                    ("stationID", "startStationID"),
                    ("solarSystemID", "startSolarSystemID"),
                    ("regionID", "startRegionID"),
                ] {
                    if let Some(n) = field(filter, name).and_then(Value::as_i64) {
                        if n > 0 || name == "assigneeID" {
                            q.push(Box::new(move |_, r, _| r.int(column) == Some(n)));
                        }
                    }
                }
                for (name, at_least) in [("priceMin", true), ("priceMax", false)] {
                    if let Some(n) = field(filter, name)
                        .and_then(Value::as_f64)
                        .filter(|n| n.is_finite() && *n > 0.0)
                    {
                        q.push(Box::new(move |_, _, c| if at_least { c.price >= n } else { c.price <= n }));
                    }
                }
                if let Some(s) = field(filter, "string")
                    .and_then(Value::as_string)
                    .filter(|s| !s.is_empty())
                {
                    let s = s.to_lowercase();
                    q.push(Box::new(move |_, r, _| {
                        let text = format!("{} {}", r.text("title").unwrap_or(""), r.text("description").unwrap_or(""));
                        text.to_lowercase().contains(&s)
                    }));
                }
                for (name, exclude) in [("issuedByIDs", false), ("notIssuedByIDs", true)] {
                    if let Some(s) = field(filter, name).and_then(Value::as_string) {
                        let ids: Vec<i64> =
                            s.split(',').filter_map(|s| s.trim().parse::<i32>().ok()).map(i64::from).collect();
                        if !ids.is_empty() {
                            q.push(Box::new(move |_, _, c| ids.contains(&c.issuer()) != exclude));
                        }
                    }
                }
                // An item of the type, group or category asked for, among the
                // types the static data knows with their group (what joining
                // invTypes and invGroups kept), as the types it may be.
                for name in ["itemTypeID", "itemGroupID", "itemCategoryID"] {
                    if let Some(n) = field(filter, name)
                        .and_then(Value::as_i64)
                        .filter(|n| *n > 0)
                    {
                        let sde = crate::sde::get();
                        let types: Vec<i32> = match name {
                            "itemTypeID" => sde.group_and_category(n).map(|_| vec![n as i32]).unwrap_or_default(),
                            "itemGroupID" => sde.types_in_group(n).iter().copied().filter(|t| sde.group_and_category(*t as i64).is_some()).collect(),
                            _ => sde.types_in_category(n).to_vec(),
                        };
                        q.push(Box::new(move |db, _, c| {
                            db.table("contract_items")
                                .find(&["contractID"], &[int(c.id)])
                                .any(|i| i.int("typeID").is_some_and(|t| types.contains(&(t as i32))))
                        }));
                    }
                }
            }
        }
        "GetContractListForOwner" => {
            let who = call.arg_id(0);
            if flag(call, "isAccepted", 3) {
                q.push(Box::new(move |_, _, c| c.acceptor_id == who));
            } else {
                q.push(Box::new(move |_, _, c| c.issuer() == who));
            }
            if let Some(s) = call.arg_i64(1).filter(|s| *s >= 0) {
                q.push(Box::new(move |_, _, c| c.status == s));
            }
            if let Some(t) = call.arg_i64(2).filter(|t| *t > 0) {
                q.push(Box::new(move |_, _, c| c.kind == t));
            }
        }
        method => {
            let who = owner(
                ctx,
                flag(
                    call,
                    "forCorp",
                    if method == "GetMyCurrentContractList" {
                        1
                    } else {
                        0
                    },
                ),
            )?;
            match method {
                "GetMyBids" => q.push(Box::new(move |db, _, c| has_bid(db, c.id, who))),
                "GetMyCurrentContractList" => {
                    q.push(Box::new(|_, _, c| [0, 1, 2, 3].contains(&c.status)));
                    if flag(call, "isAccepted", 0) {
                        q.push(Box::new(move |_, _, c| c.acceptor_id == who));
                    } else {
                        q.push(Box::new(move |_, _, c| c.issuer() == who));
                    }
                }
                _ => q.push(Box::new(move |db, _, c| attention(db, c, who, now))),
            }
        }
    }
    if let Some(start) = call.kwarg("startContractID").and_then(Value::as_i64) {
        q.push(Box::new(move |_, _, c| c.id <= start));
    }
    let limit = call
        .kwarg("num")
        .and_then(Value::as_i64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let _ = me;
    Ok(ctx.store.read(|db| {
        let mut contracts: Vec<(i64, Row)> = db
            .table("contracts")
            .rows()
            .filter_map(|r| {
                let c = Contract::read(&r);
                q.iter().all(|f| f(db, &r, &c)).then_some((c.id, r))
            })
            .collect();
        contracts.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
        contracts.truncate(limit);
        let mut items = vec![];
        let mut bids = vec![];
        for (id, _) in &contracts {
            items.push((Value::Int(*id), item_rows(db, *id)));
            bids.push((Value::Int(*id), bid_rows(db, *id)));
        }
        key_val(vec![
            ("contracts", rowset(HEADER, contracts.iter().map(|(_, r)| Value::List(r.line(HEADER))).collect())),
            ("items", Value::Dict(items)),
            ("bids", Value::Dict(bids)),
        ])
    }))
}
