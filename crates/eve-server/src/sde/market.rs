//! What the static data says the NPC market stocks: the starter stations of
//! the schools, the marketable types and what they cost, and the NPC orders
//! made from them in the store.

use std::collections::{BTreeMap, HashMap, HashSet};

use tracing::info;

use super::Tables;
use crate::store::{self, StoreError, Tx, Value};

/// Each school with its corporation and that corporation's home system
/// (chrSchools joined to crpNPCCorporations): a school's starter station is
/// the corporation's lowest-numbered station in that system.
pub fn schools(t: &Tables) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let corps = t.table("crpNPCCorporations");
    let mut out = (Vec::new(), Vec::new(), Vec::new());
    for s in t.table("chrSchools").rows() {
        let (Some(school), Some(corp)) = (s.int("schoolID"), s.int("corporationID")) else { continue };
        let Some(system) = corps.get("corporationID", corp).and_then(|n| n.int("solarSystemID")) else { continue };
        out.0.push(school as i32);
        out.1.push(corp as i32);
        out.2.push(system as i32);
    }
    out
}

/// Every type in a market group, with its base price (None where the data
/// leaves it at nothing) and portion size.
pub fn marketable(t: &Tables) -> (Vec<i32>, Vec<Option<f64>>, Vec<Option<i32>>) {
    let mut out = (Vec::new(), Vec::new(), Vec::new());
    for r in t.table("invTypes").rows() {
        let Some(id) = r.int("typeID") else { continue };
        if r.int("marketGroupID").is_none() {
            continue;
        }
        out.0.push(id as i32);
        out.1.push(r.float("basePrice").filter(|p| *p != 0.0));
        out.2.push(r.int("portionSize").map(|p| p as i32));
    }
    out
}

/// The lowest-numbered station of each school's corporation in its home
/// system: the starter stations.
fn starter_stations(t: &Tables, db: &store::Db) -> Vec<i64> {
    let (schools, corps, systems) = schools(t);
    let stations = db.table("staStations");
    let mut out: BTreeMap<i32, i64> = BTreeMap::new();
    for ((school, corp), system) in schools.iter().zip(&corps).zip(&systems) {
        let first = stations
            .find(&["corporationID"], &[Value::Int(*corp as i64)])
            .filter(|s| s.int("solarSystemID") == Some(*system as i64))
            .filter_map(|s| s.int("stationID"))
            .min();
        if let Some(first) = first {
            let slot = out.entry(*school).or_insert(first);
            *slot = (*slot).min(first);
        }
    }
    out.into_values().collect()
}

/// A fresh world's NPC market: sell orders for every marketable type at one
/// hub station per region and at every school's starter station, and buy
/// orders beside them at half the price reaching the whole region, so
/// anything can be sold back (buying at the sell price and selling at half
/// leaves no loop to farm, reprocessing included).
///
/// The hub is the lowest-numbered station in the region's system with the
/// most stations. The price is the type's basePrice; a type without one is
/// priced at 110% of its reprocessing materials (two levels deep, so
/// components count), and anything still unpriced (ores) at 1000 ISK per
/// portion.
///
/// This was SQL run once when the database was first imported, so it does
/// nothing where there are NPC orders already.
pub fn seed_fresh(tx: &mut Tx, t: &Tables) -> Result<(), StoreError> {
    if tx.table("market_orders").rows().any(|o| o.int("charID").is_some_and(|c| c < 100_000_000)) {
        return Ok(());
    }
    // Each region's system with the most stations (the lower ID on a tie),
    // and that system's lowest-numbered station.
    let mut systems: BTreeMap<(i64, i64), (i64, i64)> = BTreeMap::new();
    for s in tx.table("staStations").rows() {
        let (Some(region), Some(system), Some(station)) = (s.int("regionID"), s.int("solarSystemID"), s.int("stationID")) else { continue };
        let e = systems.entry((region, system)).or_insert((0, station));
        e.0 += 1;
        e.1 = e.1.min(station);
    }
    let mut hubs: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
    for ((region, system), (count, first)) in &systems {
        let best = hubs.entry(*region).or_insert((*count, *system, *first));
        if *count > best.0 {
            *best = (*count, *system, *first);
        }
    }
    let mut outlets: Vec<i64> = hubs.values().map(|h| h.2).chain(starter_stations(t, tx.db())).collect();
    outlets.sort_unstable();
    outlets.dedup();
    let outlets: Vec<(i64, i64, i64, i64)> = outlets
        .into_iter()
        .filter_map(|id| {
            let s = tx.table("staStations").get_id(id)?;
            Some((id, s.int("corporationID")?, s.int("solarSystemID")?, s.int("regionID")?))
        })
        .collect();
    let (types, prices) = seed_prices(t);
    let issued = crate::bulkdata::filetime_now();
    let mut sold = 0;
    for side in [0, 1] {
        for (ty, price) in types.iter().zip(&prices) {
            // A buy order beside every sell order, at half its price.
            let price = if side == 0 { *price } else { cents((price * 0.5).max(0.01)) };
            for (station, corp, system, region) in &outlets {
                tx.insert(
                    "market_orders",
                    &[
                        ("typeID", Value::Int(*ty as i64)),
                        ("charID", Value::Int(*corp)),
                        ("walletOwnerID", Value::Int(*corp)),
                        ("regionID", Value::Int(*region)),
                        ("stationID", Value::Int(*station)),
                        ("range", Value::Int(if side == 0 { -1 } else { 32767 })),
                        ("bid", Value::Int(side)),
                        ("price", Value::Float(price)),
                        ("volEntered", Value::Int(1_000_000)),
                        ("volRemaining", Value::Int(1_000_000)),
                        ("issued", Value::Int(issued)),
                        ("orderState", Value::Int(0)),
                        ("minVolume", Value::Int(1)),
                        ("contraband", Value::Int(0)),
                        ("accountID", Value::Int(1000)),
                        ("duration", Value::Int(365)),
                        ("isCorp", Value::Int(1)),
                        ("solarSystemID", Value::Int(*system)),
                        ("escrow", Value::Float(0.0)),
                        ("jumps", Value::Int(1)),
                    ],
                )?;
                sold += 1;
            }
        }
    }
    info!("NPC market: {} sell orders and as many buy orders stocked in a fresh world", sold / 2);
    Ok(())
}

/// Every school's starter station keeps an NPC sell order for each
/// marketable type and a buy order at half its price (run at every start).
/// NPC orders already there count, and are marked, as its stock; player
/// orders and orders outside starter stations are not replaced.
pub fn stock_starter_stations(tx: &mut Tx, t: &Tables) -> Result<(), StoreError> {
    let stations = starter_stations(t, tx.db());
    // The lowest-numbered open NPC order of each (station, type, side) not
    // yet stock counts as its stock.
    let mut first: BTreeMap<(i64, i64, i64), (i64, store::RowId)> = BTreeMap::new();
    let mut stocked: HashSet<(i64, i64, i64)> = HashSet::new();
    for station in &stations {
        for o in tx.table("market_orders").find(&["stationID"], &[Value::Int(*station)]) {
            let key = (*station, o.int("typeID").unwrap_or(0), o.int("bid").unwrap_or(0));
            if o.bool("starterSeed") == Some(true) {
                stocked.insert(key);
            } else if o.int("charID").is_some_and(|c| c < 100_000_000) && o.int("orderState") == Some(0) {
                let id = o.int("orderID").unwrap_or(i64::MAX);
                let slot = first.entry(key).or_insert((id, o.id));
                if id < slot.0 {
                    *slot = (id, o.id);
                }
            }
        }
    }
    for (key, (_, row)) in first {
        if !stocked.contains(&key) {
            tx.update("market_orders", row, &[("starterSeed", Value::Bool(true))])?;
        }
    }
    // Each type at the NPC market's lowest sell price anywhere, else its
    // base price, else 1000 ISK per portion; never under 0.02.
    let mut lowest: HashMap<i64, f64> = HashMap::new();
    for o in tx.table("market_orders").rows() {
        if o.int("bid") != Some(0) || o.int("charID").is_none_or(|c| c >= 100_000_000) {
            continue;
        }
        let (Some(ty), Some(price)) = (o.int("typeID"), o.float("price")) else { continue };
        if price > 0.0 {
            let l = lowest.entry(ty).or_insert(price);
            *l = l.min(price);
        }
    }
    let (types, base, portions) = marketable(t);
    let issued = crate::bulkdata::filetime_now();
    // Stock already full, open and issued within the last month needs no
    // rewrite: the SQL this replaces re-issued every order on every start,
    // only so that none ever reached its 365-day expiry.
    const MONTH: i64 = 30 * 864_000_000_000;
    let mut fresh: HashSet<(i64, i64, i64)> = HashSet::new();
    for station in &stations {
        for o in tx.table("market_orders").find(&["stationID"], &[Value::Int(*station)]) {
            if o.bool("starterSeed") == Some(true)
                && o.int("volEntered") == Some(1_000_000_000)
                && o.int("volRemaining") == Some(1_000_000_000)
                && o.int("orderState") == Some(0)
                && o.int("duration") == Some(365)
                && o.int("walletOwnerID") == o.int("charID")
                && o.int("issued").is_some_and(|i| i > issued - MONTH)
            {
                fresh.insert((*station, o.int("typeID").unwrap_or(0), o.int("bid").unwrap_or(0)));
            }
        }
    }
    for station in stations {
        let Some(s) = tx.table("staStations").get_id(station) else { continue };
        let (corp, system, region) = (s.int("corporationID"), s.int("solarSystemID"), s.int("regionID"));
        let corp = corp.map_or(Value::Null, Value::Int);
        for ((ty, base), portion) in types.iter().zip(&base).zip(&portions) {
            let ty = *ty as i64;
            let price = lowest.get(&ty).copied().or(*base).unwrap_or_else(|| 1000.0 / (portion.unwrap_or(1).max(1) as f64)).max(0.02);
            for side in [0, 1] {
                if fresh.contains(&(station, ty, side)) {
                    continue;
                }
                let price = cents(if side == 1 { price * 0.5 } else { price });
                tx.upsert(
                    "market_orders",
                    &["stationID", "typeID", "bid"],
                    &[
                        ("typeID", Value::Int(ty)),
                        ("charID", corp.clone()),
                        ("walletOwnerID", corp.clone()),
                        ("regionID", region.map_or(Value::Null, Value::Int)),
                        ("stationID", Value::Int(station)),
                        ("range", Value::Int(-1)),
                        ("bid", Value::Int(side)),
                        ("price", Value::Float(price)),
                        ("volEntered", Value::Int(1_000_000_000)),
                        ("volRemaining", Value::Int(1_000_000_000)),
                        ("issued", Value::Int(issued)),
                        ("orderState", Value::Int(0)),
                        ("minVolume", Value::Int(1)),
                        ("contraband", Value::Int(0)),
                        ("accountID", Value::Int(1000)),
                        ("duration", Value::Int(365)),
                        ("isCorp", Value::Int(1)),
                        ("solarSystemID", system.map_or(Value::Null, Value::Int)),
                        ("escrow", Value::Float(0.0)),
                        ("jumps", Value::Int(0)),
                        ("starterSeed", Value::Bool(true)),
                    ],
                    &[
                        ("volEntered", Value::Int(1_000_000_000)),
                        ("volRemaining", Value::Int(1_000_000_000)),
                        ("orderState", Value::Int(0)),
                        ("issued", Value::Int(issued)),
                        ("duration", Value::Int(365)),
                        ("walletOwnerID", corp.clone()),
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Each marketable type's seed price, rounded to the cent as the SQL did
/// (`round(greatest(price, 0.01)::numeric, 2)`).
fn seed_prices(t: &Tables) -> (Vec<i32>, Vec<f64>) {
    let types = t.table("invTypes");
    let portion = |r: &super::Row| r.int("portionSize").unwrap_or(1).max(1) as f64;
    // Reprocessing materials per unit of each type that exists.
    let mut materials: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    for m in t.table("TL2MaterialsForTypeWithActivity").rows() {
        if m.float("activity") != Some(6.0) || m.float("damagePerJob") != Some(1.0) {
            continue;
        }
        let (Some(ty), Some(req), Some(qty)) = (m.int("typeID"), m.int("requiredTypeID"), m.float("quantity")) else { continue };
        let Some(owner) = types.get("typeID", ty) else { continue };
        materials.entry(ty).or_default().push((req, qty / portion(&owner)));
    }
    // 110% of the materials at `price`, or None if no material has one
    // (SQL's sum over no non-NULL products).
    let from_materials = |ty: i64, price: &dyn Fn(i64) -> Option<f64>| -> Option<f64> {
        let products: Vec<f64> = materials.get(&ty)?.iter().filter_map(|(req, per)| Some(per * price(*req)?)).collect();
        (!products.is_empty()).then(|| 1.1 * products.iter().sum::<f64>())
    };
    let base = |ty: i64| types.get("typeID", ty).and_then(|r| r.float("basePrice"));
    let price1: HashMap<i64, Option<f64>> = types
        .rows()
        .filter_map(|r| {
            let ty = r.int("typeID")?;
            Some((ty, base(ty).filter(|p| *p != 0.0).or_else(|| from_materials(ty, &base))))
        })
        .collect();
    let mut out = (Vec::new(), Vec::new());
    for r in types.rows() {
        let Some(ty) = r.int("typeID") else { continue };
        if r.int("marketGroupID").is_none() {
            continue;
        }
        let price = price1[&ty]
            .filter(|p| *p != 0.0)
            .or_else(|| from_materials(ty, &|req| price1.get(&req).copied().flatten()))
            .unwrap_or_else(|| 1000.0 / portion(&r));
        out.0.push(ty as i32);
        out.1.push(cents(price.max(0.01)));
    }
    out
}

/// `round(x::numeric, 2)`: PostgreSQL turns a float into a decimal of 15
/// significant digits, then rounds half away from zero.
fn cents(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // x = digits × 10^(exp − 14), digits being its 15 significant digits.
    let text = format!("{:.14e}", x.abs());
    let Some((mantissa, exp)) = text.split_once('e') else { return x };
    let (Ok(digits), Ok(exp)) = (mantissa.replace('.', "").parse::<i128>(), exp.parse::<i32>()) else { return x };
    // Hundredths of x: digits × 10^(exp − 12), rounded half away from zero.
    let shift = exp - 12;
    let hundredths = if shift >= 0 {
        digits * 10i128.pow(shift as u32)
    } else if -shift > 30 {
        0
    } else {
        let div = 10i128.pow((-shift) as u32);
        (digits + div / 2) / div
    };
    (hundredths as f64 / 100.0).copysign(x)
}
