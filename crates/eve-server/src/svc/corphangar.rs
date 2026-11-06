//! Corporation hangars: an office's seven divisions (flags 4 and 116 to
//! 121), named by the directors (corpRegistry.UpdateDivisionNames).
//!
//! Members of the corporation that rents an office reach it when docked in
//! its station. What goes in becomes the corporation's; what comes out
//! becomes the taker's. Looking into a division needs its Hangar Query role,
//! taking from it its Hangar Take role; directors may do anything.

use eve_marshal::Value;

use crate::services::Ctx;
use crate::store::{self, Db, StoreError};
use crate::session::CORP_ROLE_DIRECTOR;

pub const FLAG_HANGAR: i64 = 4;
/// Divisions 2 to 7.
const FLAG_CORP_SAG2: i64 = 116;
const FLAG_CORP_SAG7: i64 = 121;
/// corpRoleHangarCanTake1 and corpRoleHangarCanQuery1; the rest follow.
const ROLE_HANGAR_TAKE_1: i64 = 8_192;
const ROLE_HANGAR_QUERY_1: i64 = 1_048_576;

/// Which division (1 to 7) an office flag is.
pub fn division(flag: i64) -> Option<u32> {
    match flag {
        FLAG_HANGAR => Some(1),
        FLAG_CORP_SAG2..=FLAG_CORP_SAG7 => Some((flag - FLAG_CORP_SAG2 + 2) as u32),
        _ => None,
    }
}

/// The flags of the divisions.
pub fn flags() -> impl Iterator<Item = i64> {
    std::iter::once(FLAG_HANGAR).chain(FLAG_CORP_SAG2..=FLAG_CORP_SAG7)
}

/// The corporation and station of an office, if `id` is one.
pub fn office(db: &Db, id: i64) -> Option<(i64, i64)> {
    let r = db.table("crpOffices").find_one(&["itemID"], &[store::Value::Int(id)])?;
    Some((r.int("corporationID")?, r.int("stationID")?))
}

/// Whether the caller is a member of the office's corporation, docked there.
pub fn mine(ctx: &Ctx<'_>, office: (i64, i64)) -> bool {
    ctx.session.corp_id == Some(office.0) && ctx.session.station_id == Some(office.1)
}

pub(crate) fn roles_at(ctx: &Ctx<'_>, station: Option<i64>) -> i64 {
    let s = ctx.session;
    s.roles_at_all | match station.filter(|id| *id > 0) {
        Some(id) if Some(id) == s.hq_id => s.roles_at_hq,
        Some(id) if Some(id) == s.base_id => s.roles_at_base,
        _ => s.roles_at_other,
    }
}

pub fn may_take(ctx: &Ctx<'_>, flag: i64) -> bool {
    may_take_at(ctx, flag, ctx.session.station_id)
}

pub(crate) fn may_take_at(ctx: &Ctx<'_>, flag: i64, station: Option<i64>) -> bool {
    let r = roles_at(ctx, station);
    r & CORP_ROLE_DIRECTOR != 0 || division(flag).is_some_and(|d| r & (ROLE_HANGAR_TAKE_1 << (d - 1)) != 0)
}

pub fn may_query(ctx: &Ctx<'_>, flag: i64) -> bool {
    may_query_at(ctx, flag, ctx.session.station_id)
}

pub(crate) fn may_query_at(ctx: &Ctx<'_>, flag: i64, station: Option<i64>) -> bool {
    let r = roles_at(ctx, station);
    r & CORP_ROLE_DIRECTOR != 0 || division(flag).is_some_and(|d| r & (ROLE_HANGAR_QUERY_1 << (d - 1)) != 0)
}

/// The flags of an office the caller may look into.
pub fn visible_flags(ctx: &Ctx<'_>) -> Vec<i64> {
    flags().filter(|f| may_query(ctx, *f)).collect()
}

/// corpRegistry.UpdateDivisionNames(d1, ..., d7): directors name the divisions.
pub async fn update_division_names(ctx: &Ctx<'_>, args: &[Value]) -> Result<Value, String> {
    let corp = ctx.session.corp_id.ok_or("No corporation.")?;
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err("Only directors may rename the divisions.".into());
    }
    let names: Vec<Option<String>> = (0..7).map(|i| args.get(i).and_then(|v| v.as_string()).map(|s| s.chars().take(50).collect())).collect();
    let header = ["division1", "division2", "division3", "division4", "division5", "division6", "division7"];
    let saved = ctx.store.write(|tx| -> Result<_, StoreError> {
        let Some(row) = tx.table("corporation").get_id(corp) else { return Ok(None) };
        let (id, old) = (row.id, header.map(|c| row.text(c).unwrap_or_default().to_string()));
        // A name left out keeps the division's name.
        let sets: Vec<(&str, store::Value)> =
            header.iter().zip(&names).filter_map(|(c, n)| n.as_deref().map(|n| (*c, store::Value::Text(n.into())))).collect();
        tx.update("corporation", id, &sets)?;
        Ok(Some(old))
    });
    let old = match saved {
        Ok(old) => old,
        Err(e) => {
            tracing::error!("statement failed: {e}");
            return Err("The division names could not be saved.".into());
        }
    };
    let before: Vec<Value> = (0..7).map(|i| Value::text(old.as_ref().map(|o| o[i].clone()).unwrap_or_default())).collect();
    let after: Vec<Value> = (0..7).map(|i| names[i].clone().map(Value::text).unwrap_or_else(|| before[i].clone())).collect();
    let change = crate::svc::rows::change(&header, Some(&before), Some(&after));
    let w = ctx.world.lock();
    w.notify(&w.corp_members(corp), "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp), change]));
    tracing::info!("corporation {corp}'s divisions renamed");
    Ok(Value::None)
}

/// corpRegistry.UpdateCorporation(description, url, taxRate): directors set
/// the corporation's description, web page and the tax it takes.
pub async fn update_corporation(ctx: &Ctx<'_>, args: &[Value]) -> Result<Value, String> {
    let corp = ctx.session.corp_id.ok_or("No corporation.")?;
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err("Only directors may change the corporation's details.".into());
    }
    let description = args.first().and_then(|v| v.as_string()).unwrap_or_default();
    let url = args.get(1).and_then(|v| v.as_string()).unwrap_or_default();
    let tax = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
    let saved = ctx.store.write(|tx| -> Result<_, StoreError> {
        let Some(row) = tx.table("corporation").get_id(corp) else { return Ok(None) };
        let (id, old) = (row.id, (row.text("description").unwrap_or_default().to_string(), row.text("url").unwrap_or_default().to_string(), row.float("taxRate")));
        tx.update(
            "corporation",
            id,
            &[("description", store::Value::Text(description.as_str().into())), ("url", store::Value::Text(url.as_str().into())), ("taxRate", store::Value::Float(tax))],
        )?;
        Ok(Some(old))
    });
    let old = match saved {
        Ok(old) => old,
        Err(e) => {
            tracing::error!("statement failed: {e}");
            return Err("The corporation's details could not be saved.".into());
        }
    };
    let header = ["description", "url", "taxRate"];
    let before = vec![
        Value::text(old.as_ref().map(|o| o.0.clone()).unwrap_or_default()),
        Value::text(old.as_ref().map(|o| o.1.clone()).unwrap_or_default()),
        Value::Double(old.as_ref().and_then(|o| o.2).unwrap_or(0.0)),
    ];
    let after = vec![Value::text(description), Value::text(url), Value::Double(tax)];
    let change = crate::svc::rows::change(&header, Some(&before), Some(&after));
    let w = ctx.world.lock();
    w.notify(&w.corp_members(corp), "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp), change]));
    Ok(Value::None)
}
