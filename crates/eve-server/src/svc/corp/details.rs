//! Changing a corporation's logo, its CEO-skill-based abilities and its
//! headquarters: corpRegistry.UpdateLogo / UpdateCorporationAbilities
//! (bco_corporations.py:178-182, corp_dlg_editcorpdetails.py:204-217,
//! 278-283) and corpStationMgr.MoveCorpHQHere (lobby.py:650-655).

use eve_marshal::Value;
use tracing::info;

use crate::services::{Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db};
use crate::svc::membership::{access_denied, db_failed, message, Refused, Undone, MEMBER_SKILLS};
use crate::svc::rows::change;
use crate::svc::wallet::{self, Entry, Transfer};

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

/// eveConstants corpLogoChangeCost and refCorporationLogoChangeCost.
const LOGO_CHANGE_COST: f64 = 100.0;
const REF_LOGO_CHANGE: i32 = 40;

const ATTR_IS_PLAYER_OWNABLE: i32 = 589;

const LOGO_COLUMNS:[&str; 7] = ["shape1", "shape2", "shape3", "color1", "color2", "color3", "typeface"];

/// The caller's player corporation, when they are its CEO or a director.
fn directed(ctx: &Ctx<'_>, db: &Db) -> Refused<i64> {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    if !crate::svc::membership::is_player_corp(db, corp)? {
        return Err(access_denied("NPC corporations cannot be changed"));
    }
    let ceo = db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"));
    let own = db.table("chrCorporationRoles").get_id(me).and_then(|r| r.int("rolesAtAll")).unwrap_or(0);
    if ceo != Some(me) && own & CORP_ROLE_DIRECTOR == 0 {
        return Err(message("OnlyCEOOrEquivCanEditCorp"));
    }
    Ok(corp)
}

/// OnCorporationChanged(corpID, change) to the members (as UpdateCorporation
/// sends, corphangar.rs).
fn tell(ctx: &Ctx<'_>, corp: i64, header: &[&str], before: &[Value], after: &[Value]) {
    let diff = change(header, Some(before), Some(after));
    if matches!(&diff, Value::Dict(d) if d.is_empty()) {
        return;
    }
    let w = ctx.world.lock();
    w.notify(&w.corp_members(corp), "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp), diff]));
}

/// UpdateLogo(shape1, shape2, shape3, color1, color2, color3, typeface): a
/// new logo, for corpLogoChangeCost from the corporation's wallet
/// (AskAcceptLogoChangeCost, corp_dlg_editcorpdetails.py:281).
pub async fn update_logo(ctx: &Ctx<'_>, args: &[Value]) -> Refused<Value> {
    let part = |i: usize| match args.get(i) {
        None | Some(Value::None) => Ok(store::Value::Null),
        Some(v) if i == 6 => v.as_string().map(|s| store::Value::Text(s.as_str().into())).ok_or(()),
        Some(v) => v.as_i64().filter(|n| *n >= 0 && *n <= i64::from(i32::MAX)).map(int).ok_or(()),
    };
    let new: Vec<store::Value> = (0..LOGO_COLUMNS.len()).map(part).collect::<Result<_, _>>().map_err(|_| Outcome::custom("That is not a logo."))?;
    let (corp, before, after) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let corp = directed(ctx, tx.db())?;
            let row = tx.table("corporation").get_id(corp).ok_or_else(|| Outcome::custom("Your corporation is unavailable."))?;
            let (id, before) = (row.id, row.line(&LOGO_COLUMNS));
            let entry = Entry { ref_type: REF_LOGO_CHANGE, payer: corp, payee: 0, arg_id1: "", reason: "" };
            wallet::post(tx, corp, -LOGO_CHANGE_COST, &entry).map_err(|e| match e {
                Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
                Transfer::NoSuchOwner => Outcome::custom("Your corporation has no wallet."),
                Transfer::Db(e) => db_failed("paying for the logo", e),
            })?;
            let sets: Vec<(&str, store::Value)> = LOGO_COLUMNS.iter().copied().zip(new).collect();
            tx.update("corporation", id, &sets)?;
            let after = tx.table("corporation").get_id(corp).map(|r| r.line(&LOGO_COLUMNS)).unwrap_or_default();
            Ok((corp, before, after))
        })
        .map_err(|Undone(o)| o)?;
    info!("corporation {corp} changed its logo");
    tell(ctx, corp, &LOGO_COLUMNS, &before, &after);
    wallet::notify_balance(ctx, corp).await;
    Ok(Value::None)
}

/// UpdateCorporationAbilities(): the member limit again from the CEO's
/// current skills, as founding set it. The client reads the corporation
/// again unless this answers None (corp_dlg_editcorpdetails.py:205).
pub async fn update_abilities(ctx: &Ctx<'_>) -> Refused<Value> {
    let (corp, before, after) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let corp = directed(ctx, tx.db())?;
            let row = tx.table("corporation").get_id(corp).ok_or_else(|| Outcome::custom("Your corporation is unavailable."))?;
            let (id, ceo, before) = (row.id, row.int("ceoID").unwrap_or(0), row.int("memberLimit").unwrap_or(0));
            let levels = crate::svc::skills::levels(tx.db(), ceo, &MEMBER_SKILLS.map(|(s, _)| s))?;
            let limit: i64 = MEMBER_SKILLS.iter().map(|(s, per)| levels.get(s).copied().unwrap_or(0) * per).sum();
            if limit != before {
                tx.update("corporation", id, &[("memberLimit", int(limit))])?;
            }
            Ok((corp, before, limit))
        })
        .map_err(|Undone(o)| o)?;
    if before != after {
        info!("corporation {corp} member limit {before} -> {after}");
    }
    tell(ctx, corp, &["memberLimit"], &[Value::Int(before)], &[Value::Int(after)]);
    Ok(Value::Bool(true))
}

/// MoveCorpHQHere(): the station the caller is in becomes the corporation's
/// headquarters. The CEO or a director decides; the corporation needs an
/// office there, and a player-ownable station is refused as the client does
/// (CanNotSetHQAtPlayerOwnedStation).
pub async fn move_hq_here(ctx: &Ctx<'_>, station: i64) -> Refused<Value> {
    if ctx.session.station_id != Some(station) {
        return Err(Outcome::custom("You must be docked in that station."));
    }
    let (corp, before) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let db = tx.db();
            let corp = directed(ctx, db)?;
            let station_type = db.table("staStations").get_id(station).and_then(|s| s.int("stationTypeID"));
            // dgmAttributeTypes isPlayerOwnable, read off the type as lobby.py:651 does.
            let ownable = station_type.and_then(|t| crate::sde::get().type_attribute(t, ATTR_IS_PLAYER_OWNABLE)).unwrap_or(0.0);
            if ownable != 0.0 {
                return Err(message("CanNotSetHQAtPlayerOwnedStation").into());
            }
            let has_office = db.table("crpOffices").find(&["stationID"], &[int(station)]).any(|o| o.int("corporationID") == Some(corp));
            if !has_office {
                return Err(Outcome::custom("Your corporation needs an office in this station to make it the headquarters.").into());
            }
            let row = tx.table("corporation").get_id(corp).ok_or_else(|| Outcome::custom("Your corporation is unavailable."))?;
            let (id, before) = (row.id, row.int("stationID").unwrap_or(0));
            if before == station {
                return Err(Outcome::custom("This station is already your corporation's headquarters.").into());
            }
            tx.update("corporation", id, &[("stationID", int(station))])?;
            Ok((corp, before))
        })
        .map_err(|Undone(o)| o)?;
    info!("corporation {corp} moved its headquarters from {before} to {station}");
    tell(ctx, corp, &["stationID"], &[Value::Int(before)], &[Value::Int(station)]);
    // Every member's session carries the HQ (hqID) and the roles that apply there.
    ctx.world.lock().reaffiliate(&[corp], &[]);
    Ok(Value::None)
}
