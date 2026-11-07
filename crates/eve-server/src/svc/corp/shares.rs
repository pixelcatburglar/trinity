//! Corporation shares and dividends: who holds how many of a corporation's
//! shares (`crpCharShares`; a holder is a character or a corporation),
//! handing shares on, and paying a dividend.
//!
//! corpRegistry.GetShareholders / MoveCompanyShares / MovePrivateShares
//! (bco_shares.py:33-40, wallet.py:1625-1660, 1807-1813) and PayoutDividend
//! (base_corporation.py:670, corp_ui_home.py:106-131).

use eve_marshal::Value;
use tracing::info;

use crate::services::{Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db};
use crate::svc::membership::{access_denied, db_failed, message, Refused, Undone};
use crate::svc::rows::{change, index_rowset, rowset};
use crate::svc::wallet::{self, Entry, Transfer};

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

/// market_refTypes / eveConstants refCorporationDividendPayment.
const REF_DIVIDEND: i32 = 38;

/// GetSharesByShareholder's columns: the client keys them on corporationID
/// and fills OnShareChange rows from them (bco_shares.py:22-30, 52-60).
pub const HOLDING_HEADER: &[&str] = &["corporationID", "shares"];
const SHAREHOLDER_HEADER: &[&str] = &["shareholderID", "corporationID", "shares"];

fn held(db: &Db, holder: i64, corp: i64) -> i64 {
    db.table("crpCharShares").get(&[int(holder), int(corp)]).and_then(|r| r.int("shares")).unwrap_or(0)
}

/// GetSharesByShareholder(corpShares): the shares the caller (or, with
/// corpShares, the caller's corporation) holds, in any corporation.
pub async fn shares_by_shareholder(ctx: &Ctx<'_>, corp_shares: bool) -> Value {
    let holder = if corp_shares { ctx.session.corp_id } else { ctx.session.char_id };
    let Some(holder) = holder else { return rowset(HOLDING_HEADER, Vec::new()) };
    let mut lines: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
        db.table("crpCharShares")
            .find(&["characterID"], &[int(holder)])
            .filter(|r| r.int("shares").unwrap_or(0) > 0)
            .filter_map(|r| Some((r.int("corporationID")?, r.line(HOLDING_HEADER))))
            .collect()
    });
    lines.sort_by_key(|(id, _)| *id);
    rowset(HOLDING_HEADER, lines.into_iter().map(|(_, l)| Value::List(l)).collect())
}

/// GetShareholders(corporationID): everyone holding the caller's
/// corporation's shares, keyed on shareholderID.
pub async fn shareholders(ctx: &Ctx<'_>, corp: i64) -> Outcome {
    if ctx.session.corp_id != Some(corp) {
        return access_denied("only its members may see a corporation's shareholders");
    }
    let mut lines: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
        db.table("crpCharShares")
            .find(&["corporationID"], &[int(corp)])
            .filter(|r| r.int("shares").unwrap_or(0) > 0)
            .filter_map(|r| Some((r.int("characterID")?, vec![r.cell("characterID"), r.cell("corporationID"), r.cell("shares")])))
            .collect()
    });
    lines.sort_by_key(|(id, _)| *id);
    Outcome::Value(index_rowset(SHAREHOLDER_HEADER, "shareholderID", lines.into_iter().map(|(id, l)| (Value::Int(id), Value::List(l))).collect()))
}

/// Who may hold shares: a character, or a corporation that still exists.
fn is_owner(db: &Db, id: i64) -> bool {
    db.table("character_").get_id(id).is_some()
        || db.table("corporation").get_id(id).is_some_and(|c| c.int("deleted") == Some(0))
}

fn may_direct(db: &Db, corp: i64, me: i64) -> bool {
    let ceo = db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"));
    let own = db.table("chrCorporationRoles").get_id(me).and_then(|r| r.int("rolesAtAll")).unwrap_or(0);
    ceo == Some(me) || own & CORP_ROLE_DIRECTOR != 0
}

/// One holding before and after a move: (holder, corporation, old, new).
type Holding = (i64, i64, i64, i64);

/// MoveCompanyShares(corporationID, toShareholderID, numberOfShares) moves
/// shares the caller's corporation holds (CEO or directors);
/// MovePrivateShares the caller's own.
pub async fn move_shares(ctx: &Ctx<'_>, args: &[Value], company: bool) -> Refused<Value> {
    let corp = args.first().and_then(Value::as_i64).unwrap_or(0);
    let to = args.get(1).and_then(Value::as_i64).unwrap_or(0);
    let n = args.get(2).and_then(Value::as_i64).unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let holder = if company { ctx.session.corp_id.unwrap_or(0) } else { me };
    if n <= 0 || n > i64::from(i32::MAX) {
        return Err(Outcome::custom("Give at least one share."));
    }
    if to == holder {
        return Err(Outcome::custom("They already hold those shares."));
    }
    let moved: Vec<Holding> = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let db = tx.db();
            if company && !may_direct(db, holder, me) {
                return Err(access_denied("only the CEO and directors may give away the corporation's shares").into());
            }
            if !is_owner(db, to) {
                return Err(message("GiveSharesSelectOwner").into());
            }
            let (from_old, to_old) = (held(db, holder, corp), held(db, to, corp));
            if from_old < n {
                return Err(Outcome::custom(format!("Only {from_old} of those shares are held there.")).into());
            }
            let to_new = to_old.checked_add(n).filter(|t| *t <= i64::from(i32::MAX)).ok_or_else(|| Outcome::custom("That holding would be too large."))?;
            let from_row = tx.table("crpCharShares").get(&[int(holder), int(corp)]).map(|r| r.id);
            if let Some(row) = from_row {
                if from_old == n {
                    tx.delete("crpCharShares", row)?;
                } else {
                    tx.update("crpCharShares", row, &[("shares", int(from_old - n))])?;
                }
            }
            tx.upsert(
                "crpCharShares",
                &["characterID", "corporationID"],
                &[("characterID", int(to)), ("corporationID", int(corp)), ("shares", int(to_new))],
                &[("shares", int(to_new))],
            )?;
            Ok(vec![(holder, corp, from_old, from_old - n), (to, corp, to_old, to_new)])
        })
        .map_err(|Undone(o)| o)?;
    info!("{me} moved {n} shares of {corp} from {holder} to {to}");
    notify(ctx, &moved);
    Ok(Value::None)
}

/// OnShareChange(shareholderID, corporationID, change) to the holders, the
/// members of a holding corporation and the members of the corporation the
/// shares are in (bco_shares.py:42-71, wallet.py:212-218).
fn notify(ctx: &Ctx<'_>, holdings: &[Holding]) {
    let w = ctx.world.lock();
    for &(holder, corp, old, new) in holdings {
        let line = |n: i64| (n > 0).then(|| vec![Value::Int(corp), Value::Int(n)]);
        let (before, after) = (line(old), line(new));
        if before == after {
            continue;
        }
        let diff = change(HOLDING_HEADER, before.as_deref(), after.as_deref());
        let mut clients = w.clients_of(holder);
        clients.extend(w.corp_members(holder));
        clients.extend(w.corp_members(corp));
        clients.sort_unstable();
        clients.dedup();
        w.notify(&clients, "OnShareChange", Value::Tuple(vec![Value::Int(holder), Value::Int(corp), diff]));
    }
}

/// PayoutDividend(payShareholders, payoutAmount): the CEO pays `amount`
/// out of the corporation's wallet, to its shareholders in proportion to
/// their shares (the corporation's own holding takes no part), or else in
/// equal parts to its members. Parts are whole cents; what does not divide
/// stays in the wallet.
pub async fn payout_dividend(ctx: &Ctx<'_>, args: &[Value]) -> Refused<Value> {
    let to_shareholders = args.first().and_then(Value::as_i64).unwrap_or(0) != 0;
    let amount = args.get(1).and_then(Value::as_f64).unwrap_or(0.0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    if !amount.is_finite() || amount < 1.0 {
        return Err(Outcome::custom("Pay out at least 1 ISK."));
    }
    let paid: Vec<(i64, f64)> = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let db = tx.db();
            if !crate::svc::membership::is_player_corp(db, corp)? {
                return Err(access_denied("only player corporations pay dividends").into());
            }
            if db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID")) != Some(me) {
                return Err(message("OnlyCEOCanPayoutDividends").into());
            }
            let weights: Vec<(i64, i64)> = if to_shareholders {
                db.table("crpCharShares")
                    .find(&["corporationID"], &[int(corp)])
                    .filter_map(|r| Some((r.int("characterID")?, r.int("shares").unwrap_or(0))))
                    .filter(|(holder, n)| *holder != corp && *n > 0)
                    .collect()
            } else {
                db.table("character_").find(&["corporationID"], &[int(corp)]).filter_map(|c| Some((c.int("characterID")?, 1))).collect()
            };
            let total: i64 = weights.iter().map(|(_, n)| n).sum();
            if total == 0 {
                return Err(Outcome::custom(if to_shareholders { "Nobody else holds the corporation's shares." } else { "The corporation has no members." }).into());
            }
            let cents = (amount * 100.0).floor() as i128;
            let mut weights = weights;
            weights.sort_unstable();
            let parts: Vec<(i64, f64)> = weights
                .into_iter()
                .map(|(owner, n)| (owner, (cents * i128::from(n) / i128::from(total)) as f64 / 100.0))
                .filter(|(_, part)| *part > 0.0)
                .collect();
            let reason = "";
            for &(owner, part) in &parts {
                let entry = Entry { ref_type: REF_DIVIDEND, payer: corp, payee: owner, arg_id1: "", reason };
                wallet::post(tx, corp, -part, &entry).map_err(|e| match e {
                    Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
                    Transfer::NoSuchOwner => Outcome::custom("The corporation has no wallet."),
                    Transfer::Db(e) => db_failed("paying the dividend", e),
                })?;
                wallet::post(tx, owner, part, &entry).map_err(|e| match e {
                    Transfer::Db(e) => db_failed("paying the dividend", e),
                    _ => Outcome::custom("A dividend recipient has no wallet."),
                })?;
            }
            Ok(parts)
        })
        .map_err(|Undone(o)| o)?;
    let total: f64 = paid.iter().map(|(_, p)| p).sum();
    info!("corporation {corp} paid {total:.2} ISK in dividends to {} recipients", paid.len());
    wallet::notify_balance(ctx, corp).await;
    for (owner, _) in &paid {
        wallet::notify_balance(ctx, *owner).await;
    }
    Ok(Value::None)
}

/// GetMemberIDsWithMoreThanAvgShares(): the members holding more of the
/// corporation's shares than its shareholding members do on average.
pub async fn members_above_average(ctx: &Ctx<'_>, corp: i64) -> Value {
    let ids = ctx.store.read(|db| {
        let members: Vec<(i64, i64)> = db
            .table("crpCharShares")
            .find(&["corporationID"], &[int(corp)])
            .filter_map(|r| Some((r.int("characterID")?, r.int("shares").unwrap_or(0))))
            .filter(|(holder, n)| *n > 0 && db.table("character_").get_id(*holder).and_then(|c| c.int("corporationID")) == Some(corp))
            .collect();
        if members.is_empty() {
            return Vec::new();
        }
        let total: i64 = members.iter().map(|(_, n)| n).sum();
        let mut ids: Vec<i64> = members.iter().filter(|(_, n)| n * members.len() as i64 > total).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    });
    Value::List(ids.into_iter().map(Value::Int).collect())
}
