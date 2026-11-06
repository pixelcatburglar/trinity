use std::cmp::Ordering;
use std::collections::BTreeMap;

use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row};
use crate::svc::rows::{row, rowset_with_columns};

pub async fn market_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if matches!(call.method.as_str(), "StartupCheck" | "GetOrders" | "GetCharOrders" | "GetCorporationOrders"
        | "GetStationAsks" | "GetSystemAsks" | "GetRegionBest" | "PlaceCharOrder" | "CancelCharOrder" | "ModifyCharOrder") {
        let result = crate::svc::trade::expire(ctx).await;
        if !matches!(result, Outcome::Value(_)) { return Some(result); }
    }
    let arg0 = || call.arg_id(0);

    let out = match call.method.as_str() {
        "StartupCheck" => {
            if ctx.session.char_id.is_none() || ctx.session.region_id.is_none() {
                Outcome::custom("Select a character in a region before opening the market.")
            } else {
                Outcome::Value(Value::None)
            }
        }
        "GetMarketGroups" => Outcome::Value(market_groups()),

        "GetOrders" => cached_orders(orders_for_type(ctx, arg0()).await),

        "GetOldPriceHistory" => {
            Outcome::Value(history_rows(Vec::new()))
        }
        "GetNewPriceHistory" => Outcome::Value(price_history(ctx, arg0()).await),

        "GetStationAsks" => Outcome::Value(best_asks(ctx, Scope::Station).await),
        "GetSystemAsks" => Outcome::Value(best_asks(ctx, Scope::SolarSystem).await),
        "GetRegionBest" => Outcome::Value(best_asks(ctx, Scope::Region).await),

        "GetCharOrders" => Outcome::Value(char_orders(ctx).await),
        "GetCorporationOrders" => Outcome::Value(corporation_orders(ctx).await),

        "PlaceCharOrder" | "CancelCharOrder" | "ModifyCharOrder" => {
            return crate::svc::trade::market_write(call, ctx).await;
        }
        "GetCharSkillLimits" => {
            Outcome::Unknown("marketProxy::GetCharSkillLimits has no client call site".into())
        }

        _ => return None,
    };
    Some(out)
}

fn cached_orders(value: Value) -> Outcome {
    let table=eve_marshal::StringTable::default();
    let bytes=match eve_marshal::save(&value,&table) {
        Ok(bytes)=>bytes,
        Err(error)=>{error!("serializing market orders: {error}");return Outcome::custom("Market orders could not be loaded.");}
    };
    // The original marketQuote compares the cached result's checksum before
    // replacing its local order list. Raw RPC results leave that version at 0.
    let (mut a,mut b)=(1u32,0u32);
    for byte in &bytes {a=(a+u32::from(*byte))%65521;b=(b+a)%65521;}
    Outcome::Value(Value::Instance {
        class:Box::new(Value::bytes("objectCaching.CachedMethodCallResult")),
        state:Box::new(Value::Tuple(vec![
            Value::Dict(vec![(Value::bytes("versionCheck"),Value::bytes("always")),
                (Value::bytes("sessionInfo"),Value::bytes("regionid"))]),
            Value::Bytes(bytes),
            Value::List(vec![Value::Long(crate::bulkdata::filetime_now()),Value::Int(((b<<16)|a) as i32 as i64)]),
        ])),
    })
}

const GROUP_HEADER: &[&str] = &[
    "marketGroupID",
    "parentGroupID",
    "marketGroupName",
    "description",
    "graphicID",
    "hasTypes",
    "types",
];

/// The market groups by parent, each with every type in it or in the groups
/// below it (twelve levels deep at most), by type ID.
fn market_groups() -> Value {
    let groups = crate::sde::table("invMarketGroups");
    let types = crate::sde::table("invTypes");

    let mut types_by_group: BTreeMap<i32, Vec<Value>> = BTreeMap::new();
    for root in groups.rows() {
        let Some(root_id) = root.int("marketGroupID") else { continue };
        let mut nodes = vec![root_id];
        let mut frontier = vec![root_id];
        for _ in 0..12 {
            frontier = frontier
                .iter()
                .flat_map(|n| groups.find("parentGroupID", *n))
                .filter_map(|g| g.int("marketGroupID"))
                .collect();
            nodes.extend(&frontier);
        }
        let mut ids: Vec<i64> = nodes.iter().flat_map(|n| types.find("marketGroupID", *n)).filter_map(|t| t.int("typeID")).collect();
        if ids.is_empty() {
            continue;
        }
        ids.sort_unstable();
        types_by_group.entry(root_id as i32).or_default().extend(ids.into_iter().map(Value::Int));
    }

    let mut rows: Vec<_> = groups.rows().collect();
    rows.sort_by_key(|r| r.int("marketGroupID"));

    let mut buckets: BTreeMap<Option<i32>, Vec<Value>> = BTreeMap::new();
    buckets.entry(None).or_default();
    for r in &rows {
        let group_id = r.int("marketGroupID").unwrap_or(0) as i32;
        let parent = r.int("parentGroupID").map(|p| p as i32);
        let line = Value::List(vec![
            Value::Int(group_id as i64),
            parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None),
            r.cell("marketGroupName"),
            r.text("description").map(|d| Value::Bytes(d.as_bytes().to_vec())).unwrap_or(Value::None),
            r.cell("graphicID"),
            Value::Int(r.int("hasTypes").unwrap_or(0)),
            Value::List(types_by_group.remove(&group_id).unwrap_or_default()),
        ]);
        buckets.entry(parent).or_default().push(line);
        buckets.entry(Some(group_id)).or_default();
    }

    Value::Dict(
        buckets
            .into_iter()
            .map(|(parent, lines)| {
                let key = parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None);
                (key, rowset_with_columns(GROUP_HEADER, lines))
            })
            .collect(),
    )
}

pub const ORDER_HEADER: &[&str] = &[
    "orderID",
    "typeID",
    "charID",
    "regionID",
    "stationID",
    "range",
    "bid",
    "price",
    "volEntered",
    "volRemaining",
    "issued",
    "orderState",
    "minVolume",
    "contraband",
    "accountKey",
    "duration",
    "isCorp",
    "solarSystemID",
    "escrow",
    "jumps",
    "constellationID",
];

/// The `market_orders` columns behind [`ORDER_HEADER`], up to its last
/// (`constellationID`, from the order's solar system). `accountID` is sent as
/// `accountKey`.
const ORDER_COLUMNS: &[&str] = &[
    "orderID",
    "typeID",
    "charID",
    "regionID",
    "stationID",
    "range",
    "bid",
    "price",
    "volEntered",
    "volRemaining",
    "issued",
    "orderState",
    "minVolume",
    "contraband",
    "accountID",
    "duration",
    "isCorp",
    "solarSystemID",
    "escrow",
    "jumps",
];

/// A `market_orders` row as the client's order line, with its solar
/// system's constellation (None for a system the map does not hold).
pub fn order_line(db: &Db, r: &Row) -> Vec<Value> {
    let mut line = r.line(ORDER_COLUMNS);
    let system = db.table("mapSolarSystems").get(std::slice::from_ref(r.value("solarSystemID")));
    line.push(system.map_or(Value::None, |s| s.cell("constellationID")));
    line
}

/// Cheapest first, then oldest.
fn by_price_then_issued(a: &Row, b: &Row) -> Ordering {
    let price = |r: &Row| r.float("price").unwrap_or(0.0);
    price(a).total_cmp(&price(b)).then(a.int("issued").cmp(&b.int("issued")))
}

/// Newest first.
fn by_issued_descending(a: &Row, b: &Row) -> Ordering {
    b.int("issued").cmp(&a.int("issued"))
}

fn open(r: &Row) -> bool {
    r.int("orderState") == Some(0)
}

fn order_rows(lines: Vec<Value>) -> Value {
    use eve_marshal::dbrow::{dbtype,DbRow,DbRowDescriptor};
    let descriptor=std::sync::Arc::new(DbRowDescriptor::new(ORDER_HEADER.iter().map(|name| {
        let kind=match *name {"price"|"escrow"=>dbtype::R8,"issued"=>dbtype::FILETIME,_=>dbtype::I4};
        (name.as_bytes().to_vec(),kind)
    }).collect()));
    let rows=lines.into_iter().map(|line| {
        let Value::List(values)=line else {unreachable!("order line")};
        Value::DbRow(Box::new(DbRow{descriptor:descriptor.clone(),values}))
    }).collect();
    // GetBestMatchableBid appends these rows to dbutil.RowList, which rejects
    // util.Row. Native rows also provide __keys__, mutable cells and columns.
    Value::reduce("dbutil.RowList",vec![Value::Tuple(vec![descriptor.to_value(),Value::List(rows)])])
}

async fn orders_for_type(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let empty =
        || Value::Tuple(vec![order_rows(Vec::new()), order_rows(Vec::new())]);
    let Some(region_id) = ctx.session.region_id else { return empty() };

    let (asks, bids) = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db
            .table("market_orders")
            .find(&["typeID", "regionID"], &[store::Value::Int(type_id), store::Value::Int(region_id)])
            .filter(|r| open(r) && r.int("volRemaining").is_some_and(|v| v > 0))
            .collect();
        rows.sort_by(by_price_then_issued);
        let (mut asks, mut bids) = (Vec::new(), Vec::new());
        for r in &rows {
            let line = Value::List(order_line(db, r));
            if r.int("bid") == Some(0) { asks.push(line) } else { bids.push(line) }
        }
        (asks, bids)
    });
    Value::Tuple(vec![order_rows(asks), order_rows(bids)])
}

async fn char_orders(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return order_rows(Vec::new()) };
    order_rows(ctx.store.read(|db| {
        let mut rows: Vec<Row> = db.table("market_orders").find(&["charID"], &[store::Value::Int(char_id)]).filter(open).collect();
        rows.sort_by(by_issued_descending);
        rows.iter().map(|r| Value::List(order_line(db, r))).collect()
    }))
}

async fn corporation_orders(ctx: &Ctx<'_>) -> Value {
    let Some(corp_id) = ctx.session.corp_id else { return order_rows(Vec::new()) };
    order_rows(ctx.store.read(|db| {
        let mut rows: Vec<Row> = db
            .table("market_orders")
            .find(&["walletOwnerID"], &[store::Value::Int(corp_id)])
            .filter(|r| r.int("isCorp") == Some(1) && open(r) && r.int("charID").is_some_and(|c| c >= 100_000_000))
            .collect();
        rows.sort_by(by_issued_descending);
        rows.iter().map(|r| Value::List(order_line(db, r))).collect()
    }))
}

enum Scope {
    Station,
    SolarSystem,
    Region,
}

async fn best_asks(ctx: &Ctx<'_>, scope: Scope) -> Value {
    let (column, scope_id) = match scope {
        Scope::Station => ("stationID", ctx.session.station_id),
        Scope::SolarSystem => ("solarSystemID", ctx.session.solar_system_id2),
        Scope::Region => ("regionID", ctx.session.region_id),
    };
    let Some(scope_id) = scope_id else { return Value::Dict(Vec::new()) };

    // The best ask of each type: the cheapest, then the oldest.
    ctx.store.read(|db| {
        let mut best: BTreeMap<i64, Row> = BTreeMap::new();
        let asks = db
            .table("market_orders")
            .find(&[column], &[store::Value::Int(scope_id)])
            .filter(|r| r.int("bid") == Some(0) && open(r) && r.int("volRemaining").is_some_and(|v| v > 0));
        for r in asks {
            let type_id = r.int("typeID").unwrap_or(0);
            if best.get(&type_id).is_none_or(|b| by_price_then_issued(&r, b) == Ordering::Less) {
                best.insert(type_id, r);
            }
        }
        Value::Dict(best.iter().map(|(type_id, r)| (Value::Int(*type_id), row(ORDER_HEADER, order_line(db, r)))).collect())
    })
}

fn history_rows(lines: Vec<Vec<Value>>) -> Value {
    use eve_marshal::dbrow::{dbtype, DbRow, DbRowDescriptor};
    let descriptor = std::sync::Arc::new(DbRowDescriptor::of(&[
        ("historyDate", dbtype::FILETIME), ("lowPrice", dbtype::R8),
        ("highPrice", dbtype::R8), ("avgPrice", dbtype::R8),
        ("volume", dbtype::I8), ("orders", dbtype::I4),
    ]));
    let rows = lines.into_iter().map(|values| Value::DbRow(Box::new(DbRow {
        descriptor: descriptor.clone(), values,
    }))).collect();
    // marketsvc.GetPriceHistory constructs a RowList from new.header and
    // appends blue.DBRows; a util.Rowset or bare tuple is not compatible.
    Value::reduce("dbutil.RowList", vec![Value::Tuple(vec![descriptor.to_value(), Value::List(rows)])])
}

async fn price_history(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let Some(region) = ctx.session.region_id else { return history_rows(vec![]) };
    // The primary key (regionID, typeID, historyDate) keeps them by date.
    history_rows(ctx.store.read(|db| {
        db.table("market_history")
            .find(&["regionID", "typeID"], &[store::Value::Int(region), store::Value::Int(type_id)])
            .map(|r| r.line(&["historyDate", "lowPrice", "highPrice", "avgPrice", "volume", "orders"]))
            .collect()
    }))
}
