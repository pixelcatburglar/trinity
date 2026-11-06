//! `repairSvc`: the station repair shop (base_repairshop.py), bound per station,
//! and repackaging from the item menu (menusvc.py UnasembleItems), unbound.
//!
//! Quotes include a ship's hull, armour, fitted modules and drones. Repair
//! prices remain zero until the original server's pricing formula is known.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{error, info};

use crate::items::invitem_cells;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError};
use crate::svc::bound;
use crate::svc::inventory::item_change;
use crate::svc::rows::key_val;

const ATTR_DAMAGE: i32 = 3;
const ATTR_HP: i32 = 9;
const ATTR_ARMOR_DAMAGE: i32 = 266;
const ATTR_ARMOR_HP: i32 = 265;
/// Column position of `singleton` in an item row (const.ixSingleton).
const IX_SINGLETON: i64 = 6;

pub async fn repair_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    if matches!(call.method.as_str(), "GetDamageReports" | "RepairItems" | "UnasembleItem" | "UnasembleItems")
        && (ctx.session.station_id.is_none() || station.is_some_and(|id| Some(id) != ctx.session.station_id))
    {
        return Some(Outcome::custom("Dock at this station to use its repair service."));
    }
    let value = match call.method.as_str() {
        "GetDamageReports" => damage_reports(ctx, &ids(call.args.first())).await,
        "RepairItems" => return Some(repair(ctx, &ids(call.args.first())).await),
        // UnasembleItem(itemID), on the station's repair shop.
        "UnasembleItem" => {
            let station = station.or(ctx.session.station_id)?;
            return Some(repackage(ctx, station, &[call.arg_id(0)]).await);
        }
        // UnasembleItems({stationID: [itemID]}), from the item menu.
        "UnasembleItems" => {
            let Some(Value::Dict(by_station)) = call.args.first() else { return Some(Outcome::Value(Value::None)) };
            for (station, items) in by_station {
                let outcome = repackage(ctx, station.as_i64().unwrap_or(0), &ids(Some(items))).await;
                if !matches!(outcome, Outcome::Value(_)) {
                    return Some(outcome);
                }
            }
            Value::None
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn ids(v: Option<&Value>) -> Vec<i64> {
    v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64())
        .filter(|id| *id > 0 && i32::try_from(*id).is_ok()).collect()
}

/// Whether `item` is at the station, or fitted to a ship docked there.
/// Ownership must hold for both the item and its containing ship.
fn local(db: &Db, item: &Row, me: i64, station: i64) -> bool {
    if item.int("ownerID") != Some(me) {
        return false;
    }
    let location = item.int("locationID");
    location == Some(station)
        || location.and_then(|l| db.table("entity").get_id(l)).is_some_and(|ship| {
            ship.int("ownerID") == Some(me)
                && ship.int("locationID") == Some(station)
                && ship.int("typeID").is_some_and(|t| ship_types().contains(&(t as i32)))
        })
}

/// The types of the Ship category.
fn ship_types() -> &'static [i32] {
    crate::sde::get().types_in_category(6)
}

/// One attribute's dgmTypeAttribs value (stored as text) for every type
/// that has it.
fn type_attribs(attribute: i32) -> HashMap<i64, f64> {
    crate::sde::table("dgmTypeAttribs")
        .find("attributeID", attribute as i64)
        .filter_map(|r| Some((r.int("typeID")?, r.text("value")?.trim().parse::<f64>().ok()?)))
        .collect()
}

/// An item's stored attribute, 0 if it has none.
fn stored(db: &Db, item: i64, attribute: i32) -> f64 {
    db.table("entity_attributes")
        .get(&[store::Value::Int(item), store::Value::Int(attribute.into())])
        .and_then(|a| a.float("valueFloat").or(a.float("valueInt")))
        .unwrap_or(0.0)
}

/// {itemID: KeyVal(quote=[KeyVal(itemID, typeID, damage, maxHealth,
/// costToRepairOneUnitOfDamage)])} for the caller's items
/// (base_repairshop.py:176-187).
async fn damage_reports(ctx: &Ctx<'_>, item_ids: &[i64]) -> Value {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else { return Value::Dict(Vec::new()) };
    let hp = type_attribs(ATTR_HP);
    let armor = type_attribs(ATTR_ARMOR_HP);
    let reports = ctx.store.read(|db| {
        let entity = db.table("entity");
        let mut requested: Vec<i64> =
            item_ids.iter().copied().filter(|id| entity.get_id(*id).is_some_and(|e| local(db, &e, me, station))).collect();
        requested.sort_unstable();
        requested.dedup();
        let mut reports: std::collections::BTreeMap<i64, Vec<Value>> = std::collections::BTreeMap::new();
        for id in requested {
            // The item, and what is fitted to it or in its drone bay.
            let fitted = entity
                .find(&["locationID"], &[store::Value::Int(id)])
                .filter(|e| e.int("itemID") != Some(id) && matches!(e.int("flag"), Some(11..=35 | 92..=99 | 87)));
            let mut parts: Vec<Row> = entity.get_id(id).into_iter().chain(fitted).filter(|e| local(db, e, me, station)).collect();
            parts.sort_by_key(|e| e.int("itemID"));
            for e in parts {
                let item_id = e.int("itemID").unwrap_or(0);
                let type_id = e.int("typeID");
                let max = |by_type: &HashMap<i64, f64>| type_id.and_then(|t| by_type.get(&t)).copied().unwrap_or(0.0);
                let quote = key_val(vec![
                    ("itemID", Value::Int(item_id)),
                    ("typeID", Value::int_or_none(type_id)),
                    ("damage", Value::Double(stored(db, item_id, ATTR_DAMAGE) + stored(db, item_id, ATTR_ARMOR_DAMAGE))),
                    ("maxHealth", Value::Double(max(&hp) + max(&armor))),
                    ("costToRepairOneUnitOfDamage", Value::Double(0.0)),
                ]);
                reports.entry(id).or_default().push(quote);
            }
        }
        reports
    });
    Value::Dict(reports.into_iter().map(|(id, quotes)| (Value::Int(id), key_val(vec![("quote", Value::List(quotes))]))).collect())
}

async fn repair(ctx: &Ctx<'_>, item_ids: &[i64]) -> Outcome {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Outcome::custom("Dock at a station before repairing items.");
    };
    let result = ctx.store.write(|tx| {
        let db = tx.db();
        let entity = db.table("entity");
        let mut allowed: Vec<i64> =
            item_ids.iter().copied().filter(|id| entity.get_id(*id).is_some_and(|e| local(db, &e, me, station))).collect();
        allowed.sort_unstable();
        allowed.dedup();
        if item_ids.iter().any(|id| !allowed.contains(id)) { return Ok(None); }
        // Hull (3) and armour (266) damage, with the value each had.
        let mut damage = Vec::new();
        for id in &allowed {
            for a in db.table("entity_attributes").find(&["itemID"], &[store::Value::Int(*id)]) {
                if matches!(a.int("attributeID"), Some(3 | 266)) {
                    damage.push((a.id, *id, a.int("attributeID").unwrap_or(0), a.float("valueFloat").or(a.float("valueInt")).unwrap_or(0.0)));
                }
            }
        }
        let mut changed = Vec::with_capacity(damage.len());
        for (row, item, attribute, value) in damage {
            tx.delete("entity_attributes", row)?;
            changed.push((item, attribute, value));
        }
        Ok::<_, StoreError>(Some(changed))
    });
    match result {
        Ok(Some(changed)) => {
            let now = crate::bulkdata::filetime_now();
            let changes = changed.into_iter().map(|(item, attribute, value)| Value::Tuple(vec![
                Value::bytes("OnModuleAttributeChange"), Value::Int(me), Value::Int(item),
                Value::Int(attribute), Value::Long(now), Value::Int(0), Value::Double(value),
            ])).collect();
            ctx.world.lock().notify(&[ctx.client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
            Outcome::Value(Value::None)
        }
        Ok(None) => Outcome::custom("You can only repair your items at this station."),
        Err(error) => {
            error!("station repair: {}", crate::logging::chain(&error));
            Outcome::custom("Your items could not be repaired.")
        }
    }
}

/// Repackage assembled items the caller owns in `station`: never the active
/// ship, and only once they are empty.
async fn repackage(ctx: &Ctx<'_>, station: i64, item_ids: &[i64]) -> Outcome {
    let Some(me) = ctx.session.char_id else { return Outcome::Value(Value::None) };
    if ctx.session.station_id != Some(station) {
        return Outcome::custom("Dock at this station before repackaging items.");
    }
    let mut ids = item_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let active_ship = ctx.session.ship_id;

    let (damaged, holding) = ctx.store.read(|db| {
        let damaged = ids.iter().any(|id| [3, 266].iter().any(|a| stored(db, *id, *a) > 0.0));
        let holding = ids.iter().any(|id| db.table("entity").find(&["locationID"], &[store::Value::Int(*id)]).next().is_some());
        (damaged, holding)
    });
    if damaged {
        return Outcome::custom("Repair damaged items before repackaging them.");
    }
    if holding {
        return Outcome::custom("Remove everything from an item before repackaging it.");
    }
    let repackaged = ctx.store.write(|tx| {
        let mut done = Vec::new();
        for id in &ids {
            let Some(row) = tx.table("entity").get_id(*id).filter(|e| {
                e.int("ownerID") == Some(me) && e.int("locationID") == Some(station) && e.int("singleton") == Some(1) && active_ship != Some(*id)
            }) else {
                continue;
            };
            let row = row.id;
            tx.update("entity", row, &[("singleton", store::Value::Int(0))])?;
            done.push(row);
        }
        let entity = tx.table("entity");
        let rows: Vec<Vec<Value>> = done.iter().filter_map(|row| entity.row(*row)).filter_map(|r| invitem_cells(&r)).collect();
        Ok::<_, StoreError>((done.len(), rows))
    });
    let (count, rows) = repackaged.unwrap_or_else(|e| {
        error!("repackaging: {}", crate::logging::chain(&e));
        (0, Vec::new())
    });
    if count == 0 {
        return Outcome::Value(Value::None);
    }

    let world = ctx.world.lock();
    for cells in rows {
        let event = item_change(cells, vec![(IX_SINGLETON, Value::Int(1))]);
        world.notify(&[ctx.client_id], "OnItemChange", event);
    }
    info!("repackaged {count} item(s) at station {station}");
    Outcome::Value(Value::None)
}
