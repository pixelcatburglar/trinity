//! Alliances: the registry clients bind per alliance (`allianceRegistry`),
//! the corporation's side of founding and joining one (`corpRegistry`), and
//! wars.
//!
//! A corporation's CEO founds an alliance or applies to one; the executor
//! corporation's directors accept or reject; an accepted application takes
//! effect a day later ([`take_effect`], from the heartbeat). Member
//! corporations each back one member as executor, and the most backed one
//! leads. A corporation leaves by quitting or by being kicked out.
//!
//! The executor's directors also set the alliance's relationships and run
//! its wars (svc/war.rs). Every fortnight CONCORD bills the alliance for its
//! member corporations ([`bill_alliances`]); the executor's accountants pay
//! alliance bills from the executor corporation's wallet.

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::session::{CORP_ROLE_ACCOUNTANT, CORP_ROLE_DIRECTOR};
use crate::store::{self, Db, Row, Sets, Store, StoreError, Tx};
use crate::svc::bound;
use crate::svc::membership::{access_denied, db_failed, free_tickers, message, name_error, ticker_ok, transfer_refused, Refused};
use crate::svc::rows::{change, index_rowset, rowset};
use crate::svc::wallet::{self, Entry, Side};
use crate::svc::war;
use crate::world::World;

const ALLIANCE: &str = "allianceRegistry";

/// eveConstants allianceApplication*.
const APPLICATION_NEW: i16 = 1;
const APPLICATION_ACCEPTED: i16 = 2;
const APPLICATION_EFFECTIVE: i16 = 3;
const APPLICATION_REJECTED: i16 = 4;
/// eveConstants allianceCreationCost, paid by the founding corporation.
const CREATION_COST: f64 = 1_000_000_000.0;
/// market_refTypes: Alliance Registration Fee.
const REF_ALLIANCE_REGISTRATION_FEE: i32 = 48;
const TICKS_PER_SECOND: i64 = 10_000_000;
/// eveConstants allianceMembershipCost: owed per member corporation each
/// period.
const MEMBERSHIP_COST: f64 = 2_000_000.0;
pub const MAINTENANCE_PERIOD: i64 = 14 * war::DAY;
/// billTypes: Alliance Maintainance Bill.
const BILL_TYPE_ALLIANCE_MAINTENANCE: i32 = 5;
/// eveConstants allianceRelationship*: NAP, Friend, Competitor, Enemy.
const RELATIONSHIPS: std::ops::RangeInclusive<i64> = 1..=4;

/// The `alliances` columns the client reads.
const ALLIANCE_HEADER: &[&str] = &[
    "allianceID", "allianceName", "shortName", "description", "url", "executorCorpID",
    "creatorCorpID", "creatorCharID", "dictatorial", "startDate", "memberCount", "deleted",
];

/// The `allianceMembers` columns.
const MEMBER_HEADER: &[&str] = &["allianceID", "corporationID", "chosenExecutorID", "startDate"];

/// The `allianceApplications` columns.
const APPLICATION_HEADER: &[&str] =
    &["allianceID", "corporationID", "applicationText", "applicationDateTime", "state"];

/// The `allianceRelationships` columns.
const RELATIONSHIP_HEADER: &[&str] = &["fromID", "toID", "relationship"];

// ---- allianceRegistry ------------------------------------------------------------------

pub async fn alliance_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    // Bound as (allianceID, isMaster); unbound calls name the alliance themselves.
    let subject = bound::parse(&call.service).and_then(|b| b.params.first().copied()).or(ctx.session.alliance_id);
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let result = match call.method.as_str() {
        "GetAlliance" => {
            let id = call.arg_i64(0).or(subject).filter(|id| *id != 0);
            let Some(id) = id else {
                return Some(Outcome::Unknown("allianceRegistry::GetAlliance with no alliance".into()));
            };
            alliance_row(ctx, id).ok_or_else(|| message("AllianceNotFound"))
        }
        "GetRankedAlliances" => Ok(ctx.store.read(|db| {
            let mut alliances: Vec<Row> = db.table("alliances").rows().filter(|a| a.int("deleted") == Some(0)).collect();
            alliances.sort_by(|a, b| b.int("memberCount").cmp(&a.int("memberCount")).then(a.int("allianceID").cmp(&b.int("allianceID"))));
            rowset(ALLIANCE_HEADER, alliances.iter().map(|a| Value::List(a.line(ALLIANCE_HEADER))).collect())
        })),
        "GetMembers" => Ok(ctx.store.read(|db| {
            keyed(MEMBER_HEADER, db.table("allianceMembers").find(&["allianceID"], &[id(subject.unwrap_or(0))]), 1)
        })),
        "GetAllianceMembers" => {
            let alliance_id = call.arg_i64(0).or(subject).unwrap_or(0);
            Ok(ctx.store.read(|db| {
                let mut members: Vec<Row> = db.table("allianceMembers").find(&["allianceID"], &[id(alliance_id)]).collect();
                members.sort_by_key(|m| m.int("corporationID"));
                rowset(MEMBER_HEADER, members.iter().map(|m| Value::List(m.line(MEMBER_HEADER))).collect())
            }))
        }
        "GetApplications" => Ok(ctx.store.read(|db| {
            let applications = db.table("allianceApplications").find(&["allianceID"], &[id(subject.unwrap_or(0))]);
            keyed(APPLICATION_HEADER, applications.filter(|a| a.int("deleted") == Some(0)), 1)
        })),
        "UpdateApplication" => {
            let state = call.arg_i64(2).unwrap_or(-1) as i16;
            answer_application(ctx, subject.unwrap_or(0), call.arg_id(0), &text_arg(1), state).await
        }
        "DeclareExecutorSupport" => declare_support(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "DeleteMember" => delete_member(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "UpdateAlliance" => update_alliance(ctx, subject.unwrap_or(0), &text_arg(0), &text_arg(1)).await,
        "GetRelationships" => Ok(ctx.store.read(|db| {
            keyed(RELATIONSHIP_HEADER, db.table("allianceRelationships").find(&["fromID"], &[id(subject.unwrap_or(0))]), 1)
        })),
        "SetRelationship" => set_relationship(ctx, subject.unwrap_or(0), call.arg_i64(0).unwrap_or(0), call.arg_id(1)).await,
        "DeleteRelationship" => delete_relationship(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "DeclareWarAgainst" => declare_war(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "RetractWar" => retract_war(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "ChangeMutualWarFlag" => {
            mutual_war(ctx, subject.unwrap_or(0), call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await
        }
        "GetBills" => Ok(wallet::bills(ctx, subject, Side::Payable).await),
        "GetBillsReceivable" => Ok(wallet::bills(ctx, subject, Side::Receivable).await),
        "GetBillBalance" => bill_balance(ctx, subject.unwrap_or(0), call.arg_id(0)),
        "PayBill" => pay_bill(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        method => return Some(Outcome::Unknown(format!("{ALLIANCE}::{method}"))),
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

fn alliance_row(ctx: &Ctx<'_>, alliance_id: i64) -> Option<Value> {
    ctx.store.read(|db| alliance_line(db, alliance_id)).map(|line| crate::svc::rows::row(ALLIANCE_HEADER, line))
}

/// UpdateApplication(corporationID, text, state): the executor's directors
/// accept or reject. An acceptance starts the clock on the membership.
async fn answer_application(ctx: &Ctx<'_>, alliance_id: i64, corp_id: i64, note: &str, state: i16) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    let Some(old) = ctx.store.read(|db| application(db, alliance_id, corp_id)) else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    if application_state(&old) != APPLICATION_NEW || ![APPLICATION_ACCEPTED, APPLICATION_REJECTED].contains(&state) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    let when = if state == APPLICATION_ACCEPTED { Value::Long(crate::bulkdata::filetime_now()) } else { old[3].clone() };
    write(ctx.store, |tx| {
        let sets = [("applicationText", text(note)), ("state", id(state.into())), ("applicationDateTime", id(when.as_i64().unwrap_or(0)))];
        update_where(tx, "allianceApplications", &["allianceID", "corporationID"], &[id(alliance_id), id(corp_id)], &sets)
            .map_err(|e| db_failed("answering the application", e))?;
        Ok(())
    })?;
    let new = vec![old[0].clone(), old[1].clone(), Value::text(note), when, Value::Int(state.into())];
    info!("alliance {alliance_id} answered corporation {corp_id}'s application with state {state}");
    let corps = with(ctx.store.read(|db| member_corps(db, alliance_id)), corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), Some(&new))));
    Ok(Value::None)
}

/// DeclareExecutorSupport(corporationID): the caller's corporation backs
/// another member to lead. The most backed member leads; a tie keeps the
/// executor there is.
async fn declare_support(ctx: &Ctx<'_>, alliance_id: i64, choice: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if ctx.session.alliance_id != Some(alliance_id) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors of a member corporation may back an executor"));
    }
    let (members, alliance) = ctx.store.read(|db| (member_corps(db, alliance_id), alliance_line(db, alliance_id)));
    if !members.contains(&choice) {
        return Err(Outcome::custom("That corporation is not a member of the alliance."));
    }
    let alliance = alliance.ok_or_else(|| message("AllianceNotFound"))?;
    if alliance[8].as_i64() == Some(1) && alliance[5].as_i64() != Some(choice) {
        return Err(message("CanNotChooseExecutorAsDictatorial"));
    }

    let (old, new, elected) = write(ctx.store, |tx| {
        let old = member_line(tx.db(), corp_id).ok_or_else(|| Outcome::custom("Your corporation is not a member."))?;
        update_where(tx, "allianceMembers", &["corporationID"], &[id(corp_id)], &[("chosenExecutorID", id(choice))])
            .map_err(|e| db_failed("recording the choice", e))?;
        let new = member_line(tx.db(), corp_id).unwrap_or_default();
        let elected = elect(tx, alliance_id)?;
        Ok((old, new, elected))
    })?;

    info!("corporation {corp_id} backs {choice} to lead alliance {alliance_id}");
    tell(ctx.world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, Some(&old), Some(&new))));
    if let Some((was, now)) = elected {
        info!("corporation {now} now leads alliance {alliance_id} (was {was})");
        tell_executor(ctx.world, &members, alliance_id, was, now);
    }
    Ok(Value::None)
}

/// DeleteMember(corporationID): a member's directors take it out, or the
/// executor's directors kick it out. The executor itself cannot leave while
/// others remain; the last member leaving closes the alliance.
async fn delete_member(ctx: &Ctx<'_>, alliance_id: i64, corp_id: i64) -> Refused<Value> {
    let own_corp = ctx.session.corp_id == Some(corp_id);
    if ctx.session.alliance_id != Some(alliance_id) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may take a corporation out of the alliance"));
    }
    let (executor, members) = ctx.store.read(|db| (executor_of(db, alliance_id), member_corps(db, alliance_id)));
    if !own_corp && ctx.session.corp_id != executor {
        return Err(access_denied("only the executor corporation may kick members out"));
    }
    if !members.contains(&corp_id) {
        return Err(Outcome::custom("That corporation is not a member of the alliance."));
    }
    if executor == Some(corp_id) && members.len() > 1 {
        return Err(Outcome::custom(
            "The executor corporation cannot leave while other corporations remain. Back another member as executor first.",
        ));
    }

    let old = write(ctx.store, |tx| {
        let old = member_line(tx.db(), corp_id).unwrap_or_default();
        let (alliance, corp) = (id(alliance_id), id(corp_id));
        leave(tx, &alliance, &corp).map_err(|e| db_failed("leaving the alliance", e))?;
        // The count before this member left decides whether it was the last.
        if let Some((row, count, deleted)) =
            tx.table("alliances").get_id(alliance_id).map(|a| (a.id, a.int("memberCount").unwrap_or(0), a.int("deleted").unwrap_or(0)))
        {
            let sets = [("memberCount", id((count - 1).max(0))), ("deleted", id(if count <= 1 { 1 } else { deleted }))];
            tx.update("alliances", row, &sets).map_err(|e| db_failed("counting members", e))?;
        }
        Ok(old)
    })?;

    if members.len() == 1 {
        info!("corporation {corp_id} left alliance {alliance_id}, which is closed");
    } else if own_corp {
        info!("corporation {corp_id} quit alliance {alliance_id}");
    } else {
        info!("corporation {corp_id} was kicked out of alliance {alliance_id}");
    }
    tell(ctx.world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, Some(&old), None)));
    tell_corporation_alliance(ctx.world, corp_id, Some(alliance_id), None);
    Ok(Value::None)
}

/// The corporation's membership, its application, the backing it had and
/// its alliance go.
fn leave(tx: &mut Tx<'_>, alliance: &store::Value, corp: &store::Value) -> Result<(), StoreError> {
    tx.delete_where("allianceMembers", &["corporationID", "allianceID"], &[corp.clone(), alliance.clone()])?;
    tx.delete_where("allianceApplications", &["corporationID", "allianceID"], &[corp.clone(), alliance.clone()])?;
    update_where(tx, "allianceMembers", &["allianceID", "chosenExecutorID"], &[alliance.clone(), corp.clone()], &[("chosenExecutorID", store::Value::Null)])?;
    update_where(tx, "corporation", &["corporationID", "allianceID"], &[corp.clone(), alliance.clone()], &[("allianceID", id(0))])
}

/// UpdateAlliance(description, url), by the executor's directors.
async fn update_alliance(ctx: &Ctx<'_>, alliance_id: i64, description: &str, url: &str) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    let old = ctx.store.read(|db| alliance_line(db, alliance_id)).ok_or_else(|| message("AllianceNotFound"))?;
    write(ctx.store, |tx| {
        update_where(tx, "alliances", &["allianceID"], &[id(alliance_id)], &[("description", text(description)), ("url", text(url))])
            .map_err(|e| db_failed("updating the alliance", e))?;
        Ok(())
    })?;
    let mut new = old.clone();
    new[3] = Value::text(description);
    new[4] = Value::text(url);
    let members = ctx.store.read(|db| member_corps(db, alliance_id));
    let event = Value::Tuple(vec![Value::Int(alliance_id), change(ALLIANCE_HEADER, Some(&old), Some(&new))]);
    tell(ctx.world, &members, "OnAllianceChanged", event);
    Ok(Value::None)
}

// ---- corpRegistry: the corporation's side ----------------------------------------------

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let result = match call.method.as_str() {
        "GetSuggestedAllianceShortNames" => {
            let names = free_tickers(ctx, "alliances", "shortName", &text_arg(0)).await;
            Ok(rowset(&["shortName"], names.into_iter().map(|t| Value::List(vec![Value::text(t)])).collect()))
        }
        "CreateAlliance" => create_alliance(ctx, &text_arg(0), &text_arg(1), &text_arg(2), &text_arg(3)).await,
        "ApplyToJoinAlliance" => apply(ctx, call.arg_id(0), &text_arg(1)).await,
        "GetAllianceApplications" => Ok(ctx.store.read(|db| {
            let applications = db.table("allianceApplications").find(&["corporationID"], &[id(ctx.session.corp_id.unwrap_or(0))]);
            keyed(APPLICATION_HEADER, applications.filter(|a| a.int("deleted") == Some(0)), 0)
        })),
        "DeleteAllianceApplication" => withdraw(ctx, call.arg_id(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// CreateAlliance(name, shortName, description, url): the CEO's corporation
/// founds it, pays for it from its wallet, and is its first executor.
async fn create_alliance(ctx: &Ctx<'_>, name: &str, short_name: &str, description: &str, url: &str) -> Refused<Value> {
    let (name, short_name) = (name.trim(), short_name.trim());
    let char_id = ctx.session.char_id.unwrap_or(0);
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let corp = ctx.store.read(|db| corp_line(db, corp_id))?;
    if corp.ceo != char_id {
        return Err(access_denied("only the CEO can found an alliance"));
    }
    if corp.alliance_id != 0 {
        return Err(message("AllianceCreateFailCorpInAlliance"));
    }
    if let Some(key) = name_error("Alliance", name) {
        return Err(message(&key));
    }
    if !ticker_ok(short_name) {
        return Err(message("AllianceShortNameInvalid"));
    }

    let alliance_id = write(ctx.store, |tx| {
        if taken(tx.db(), "allianceName", name) {
            return Err(message("AllianceNameInvalidTaken").into());
        }
        if taken(tx.db(), "shortName", short_name) {
            return Err(message("AllianceShortNameInvalidTaken").into());
        }
        let entry = Entry { ref_type: REF_ALLIANCE_REGISTRATION_FEE, payer: corp_id, payee: 0, arg_id1: "", reason: "" };
        wallet::post(tx, corp_id, -CREATION_COST, &entry).map_err(transfer_refused)?;

        let now = crate::bulkdata::filetime_now();
        let row = tx
            .insert(
                "alliances",
                &[
                    ("allianceName", text(name)),
                    ("shortName", text(short_name)),
                    ("description", text(description)),
                    ("url", text(url)),
                    ("executorCorpID", id(corp_id)),
                    ("creatorCorpID", id(corp_id)),
                    ("creatorCharID", id(char_id)),
                    ("startDate", id(now)),
                    ("memberCount", id(1)),
                ],
            )
            .map_err(|e| db_failed("inserting the alliance", e))?;
        let alliance_id = tx.table("alliances").row(row).and_then(|a| a.int("allianceID")).unwrap_or(0);
        // alliance_shortnames follows by trigger (sync_alliance_shortnames).
        join(tx, alliance_id, corp_id, now)?;
        tx.delete_where("allianceApplications", &["corporationID"], &[id(corp_id)])
            .map_err(|e| db_failed("clearing old applications", e))?;
        Ok(alliance_id)
    })?;

    info!("corporation {corp_id} founded alliance {name:?} <{short_name}> ({alliance_id})");
    tell_corporation_alliance(ctx.world, corp_id, None, Some(alliance_id));
    Ok(Value::Int(alliance_id))
}

/// Whether an alliance already has `value` in `column`, ignoring case.
fn taken(db: &Db, column: &str, value: &str) -> bool {
    let value = value.to_lowercase();
    db.table("alliances").rows().any(|a| a.text(column).is_some_and(|v| v.to_lowercase() == value))
}

/// ApplyToJoinAlliance(allianceID, text), by the CEO or a director.
async fn apply(ctx: &Ctx<'_>, alliance_id: i64, note: &str) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may apply to join an alliance"));
    }
    if ctx.store.read(|db| corp_line(db, corp_id))?.alliance_id != 0 {
        return Err(message("AllianceJoinFailCorpInAlliance"));
    }
    let alliance = ctx.store.read(|db| alliance_line(db, alliance_id));
    if alliance.as_ref().is_none_or(|a| a[11].as_i64() != Some(0)) {
        return Err(message("AllianceNotFound"));
    }
    if ctx.store.read(|db| application(db, alliance_id, corp_id)).is_some() {
        return Err(message("CantApplyTwiceToAlliance"));
    }
    let now = crate::bulkdata::filetime_now();
    write(ctx.store, |tx| {
        let filed = [("applicationText", text(note)), ("applicationDateTime", id(now)), ("state", id(APPLICATION_NEW.into())), ("deleted", id(0))];
        let mut sets = vec![("allianceID", id(alliance_id)), ("corporationID", id(corp_id))];
        sets.extend(filed.iter().cloned());
        tx.upsert("allianceApplications", &["allianceID", "corporationID"], &sets, &filed)
            .map_err(|e| db_failed("filing the application", e))?;
        Ok(())
    })?;
    let new = vec![Value::Int(alliance_id), Value::Int(corp_id), Value::text(note), Value::Long(now), Value::Int(APPLICATION_NEW.into())];
    info!("corporation {corp_id} applied to join alliance {alliance_id}");
    let corps = with(ctx.store.read(|db| member_corps(db, alliance_id)), corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, None, Some(&new))));
    Ok(Value::None)
}

/// DeleteAllianceApplication(allianceID): the corporation withdraws (or
/// clears away a rejected) application.
async fn withdraw(ctx: &Ctx<'_>, alliance_id: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may withdraw an alliance application"));
    }
    let Some(old) = ctx.store.read(|db| application(db, alliance_id, corp_id)) else { return Ok(Value::None) };
    if application_state(&old) == APPLICATION_EFFECTIVE {
        return Err(Outcome::custom("That application has taken effect; leave the alliance instead."));
    }
    write(ctx.store, |tx| {
        tx.delete_where("allianceApplications", &["allianceID", "corporationID"], &[id(alliance_id), id(corp_id)])
            .map_err(|e| db_failed("withdrawing the application", e))?;
        Ok(())
    })?;
    let corps = with(ctx.store.read(|db| member_corps(db, alliance_id)), corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), None)));
    Ok(Value::None)
}

// ---- memberships taking effect ---------------------------------------------------------

/// Accepted applications older than `delay_secs` become memberships. Run by
/// the heartbeat.
pub async fn take_effect(world: &World, store: &Store, delay_secs: i64) {
    let due = crate::bulkdata::filetime_now() - delay_secs * TICKS_PER_SECOND;
    let accepted: Vec<Vec<Value>> = store.read(|db| {
        db.table("allianceApplications")
            .rows()
            .filter(|a| {
                a.int("deleted") == Some(0)
                    && a.int("state") == Some(APPLICATION_ACCEPTED.into())
                    && a.int("applicationDateTime").is_some_and(|t| t <= due)
            })
            .map(|a| a.line(APPLICATION_HEADER))
            .collect()
    });
    for old in accepted {
        let (alliance_id, corp_id) = (old[0].as_i64().unwrap_or(0), old[1].as_i64().unwrap_or(0));
        match admit(store, alliance_id, corp_id).await {
            Ok(Some(member)) => {
                info!("corporation {corp_id} joined alliance {alliance_id}");
                let mut new = old.clone();
                new[4] = Value::Int(APPLICATION_EFFECTIVE.into());
                let members = store.read(|db| member_corps(db, alliance_id));
                tell(world, &members, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), Some(&new))));
                tell(world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, None, Some(&member))));
                tell_corporation_alliance(world, corp_id, None, Some(alliance_id));
            }
            Ok(None) => info!("alliance application of {corp_id} to {alliance_id} lapsed: the corporation or the alliance moved on"),
            Err(_) => {}
        }
    }
}

/// Makes one accepted application a membership; None (and the application
/// dropped) if the corporation joined elsewhere or the alliance closed.
async fn admit(store: &Store, alliance_id: i64, corp_id: i64) -> Refused<Option<Vec<Value>>> {
    write(store, |tx| {
        let db = tx.db();
        let free = db.table("corporation").get_id(corp_id).is_some_and(|c| c.int("allianceID") == Some(0))
            && db.table("alliances").get_id(alliance_id).is_some_and(|a| a.int("deleted") == Some(0));
        let key = [id(alliance_id), id(corp_id)];
        if !free {
            tx.delete_where("allianceApplications", &["allianceID", "corporationID"], &key)
                .map_err(|e| db_failed("dropping the application", e))?;
            return Ok(None);
        }
        join(tx, alliance_id, corp_id, crate::bulkdata::filetime_now())?;
        update_where(tx, "allianceApplications", &["allianceID", "corporationID"], &key, &[("state", id(APPLICATION_EFFECTIVE.into()))])
            .map_err(|e| db_failed("closing the application", e))?;
        if let Some((row, count)) = tx.table("alliances").get_id(alliance_id).map(|a| (a.id, a.int("memberCount").unwrap_or(0))) {
            tx.update("alliances", row, &[("memberCount", id(count + 1))]).map_err(|e| db_failed("counting members", e))?;
        }
        Ok(member_line(tx.db(), corp_id))
    })
}

/// The corporation becomes a member, backing the executor there is (itself,
/// for a founder), and its wars become the alliance's.
fn join(tx: &mut Tx<'_>, alliance_id: i64, corp_id: i64, now: i64) -> Refused<()> {
    war::adopt(tx, corp_id, alliance_id)?;
    if let Some(executor) = tx.table("alliances").get_id(alliance_id).map(|a| a.value("executorCorpID").clone()) {
        tx.insert(
            "allianceMembers",
            &[("allianceID", id(alliance_id)), ("corporationID", id(corp_id)), ("chosenExecutorID", executor), ("startDate", id(now))],
        )
        .map_err(|e| db_failed("adding the member", e))?;
    }
    update_where(tx, "corporation", &["corporationID"], &[id(corp_id)], &[("allianceID", id(alliance_id))])
        .map_err(|e| db_failed("recording the alliance", e))?;
    Ok(())
}

/// Counts the members' backing and moves the lead if another member has
/// more of it than the executor; returns (old, new) when it moves.
fn elect(tx: &mut Tx<'_>, alliance_id: i64) -> Refused<Option<(i64, i64)>> {
    let db = tx.db();
    let Some(alliance) = db.table("alliances").get_id(alliance_id) else { return Ok(None) };
    let mut members: Vec<Row> = db.table("allianceMembers").find(&["allianceID"], &[id(alliance_id)]).collect();
    members.sort_by_key(|m| (m.int("startDate"), m.int("corporationID")));
    let backing: Vec<(i64, i64)> = members
        .iter()
        .map(|m| {
            let corp = m.int("corporationID").unwrap_or(0);
            (corp, members.iter().filter(|b| b.int("chosenExecutorID") == Some(corp)).count() as i64)
        })
        .collect();
    let Some(executor) = alliance.int("executorCorpID").filter(|_| !members.is_empty()) else { return Ok(None) };
    let Some(winner) = most_backed(&backing, executor) else { return Ok(None) };
    if winner == executor {
        return Ok(None);
    }
    update_where(tx, "alliances", &["allianceID"], &[id(alliance_id)], &[("executorCorpID", id(winner))])
        .map_err(|e| db_failed("changing the executor", e))?;
    Ok(Some((executor, winner)))
}

/// The member with strictly the most backing, else the executor; `backing`
/// is (corporation, members backing it), oldest member first.
fn most_backed(backing: &[(i64, i64)], executor: i64) -> Option<i64> {
    let held = backing.iter().find(|(c, _)| *c == executor).map(|(_, n)| *n);
    let (_, (best, votes)) = backing.iter().copied().enumerate().max_by(|(i, a), (j, b)| a.1.cmp(&b.1).then(j.cmp(i)))?;
    match held {
        Some(n) if n >= votes => Some(executor),
        _ => Some(best),
    }
}

// ---- writes ----------------------------------------------------------------------------

/// A refusal inside a write, which undoes it; the store's own refusals
/// become the registry's apology.
struct Undo(Outcome);

impl From<Outcome> for Undo {
    fn from(refusal: Outcome) -> Self {
        Undo(refusal)
    }
}

impl From<StoreError> for Undo {
    fn from(e: StoreError) -> Self {
        Undo(db_failed("changing the alliance", e))
    }
}

/// One write transaction: all of `f`'s changes, or none when it refuses.
fn write<T>(store: &Store, f: impl FnOnce(&mut Tx<'_>) -> Result<T, Undo>) -> Refused<T> {
    store.write(f).map_err(|Undo(refusal)| refusal)
}

/// `UPDATE table SET sets WHERE columns = values`.
fn update_where(tx: &mut Tx<'_>, table: &str, columns: &[&str], values: &[store::Value], sets: Sets) -> Result<(), StoreError> {
    let rows: Vec<store::RowId> = tx.table(table).find(columns, values).map(|r| r.id).collect();
    for row in rows {
        tx.update(table, row, sets)?;
    }
    Ok(())
}

fn id(value: i64) -> store::Value {
    store::Value::Int(value)
}

fn text(value: &str) -> store::Value {
    store::Value::Text(value.into())
}

// ---- reads and notifications -----------------------------------------------------------

struct CorpLine {
    ceo: i64,
    alliance_id: i64,
}

fn corp_line(db: &Db, corp_id: i64) -> Refused<CorpLine> {
    let r = db.table("corporation").get_id(corp_id).ok_or_else(|| Outcome::custom("Your corporation could not be found."))?;
    Ok(CorpLine { ceo: r.int("ceoID").unwrap_or(0), alliance_id: r.int("allianceID").unwrap_or(0) })
}

fn alliance_line(db: &Db, alliance_id: i64) -> Option<Vec<Value>> {
    db.table("alliances").get_id(alliance_id).map(|a| a.line(ALLIANCE_HEADER))
}

fn member_line(db: &Db, corp_id: i64) -> Option<Vec<Value>> {
    db.table("allianceMembers").get_id(corp_id).map(|m| m.line(MEMBER_HEADER))
}

fn application(db: &Db, alliance_id: i64, corp_id: i64) -> Option<Vec<Value>> {
    db.table("allianceApplications")
        .get(&[id(alliance_id), id(corp_id)])
        .filter(|a| a.int("deleted") == Some(0))
        .map(|a| a.line(APPLICATION_HEADER))
}

fn application_state(line: &[Value]) -> i16 {
    line.get(4).and_then(|v| v.as_i64()).unwrap_or(-1) as i16
}

fn member_corps(db: &Db, alliance_id: i64) -> Vec<i64> {
    db.table("allianceMembers").find(&["allianceID"], &[id(alliance_id)]).filter_map(|m| m.int("corporationID")).collect()
}

fn executor_of(db: &Db, alliance_id: i64) -> Option<i64> {
    alliance_line(db, alliance_id).and_then(|a| a[5].as_i64())
}

/// The executor corporation, if the caller is one of its directors.
fn require_executor_director(ctx: &Ctx<'_>, alliance_id: i64) -> Refused<i64> {
    require_executor_role(ctx, alliance_id, CORP_ROLE_DIRECTOR, "directors")
}

fn require_executor_role(ctx: &Ctx<'_>, alliance_id: i64, role: i64, who: &str) -> Refused<i64> {
    let executor = ctx.store.read(|db| executor_of(db, alliance_id));
    let holds = ctx.session.has_corp_role(role) || ctx.session.has_corp_role(CORP_ROLE_DIRECTOR);
    match executor {
        Some(e) if ctx.session.alliance_id == Some(alliance_id) && ctx.session.corp_id == Some(e) && holds => Ok(e),
        _ => Err(access_denied(&format!("only {who} of the executor corporation may do that"))),
    }
}

/// `rows` as an IndexRowset keyed on the column at `key`.
fn keyed<'a>(header: &[&str], rows: impl Iterator<Item = Row<'a>>, key: usize) -> Value {
    let items = rows
        .map(|r| {
            let l = r.line(header);
            (l[key].clone(), Value::List(l))
        })
        .collect();
    index_rowset(header, header[key], items)
}

fn with(mut corps: Vec<i64>, corp_id: i64) -> Vec<i64> {
    if !corps.contains(&corp_id) {
        corps.push(corp_id);
    }
    corps
}

fn application_event(alliance_id: i64, corp_id: i64, change: Value) -> Value {
    Value::Tuple(vec![Value::Int(alliance_id), Value::Int(corp_id), change])
}

fn member_event(alliance_id: i64, corp_id: i64, change: Value) -> Value {
    Value::Tuple(vec![Value::Int(alliance_id), Value::Int(corp_id), change])
}

/// Queues `event` for every online member of the corporations.
fn tell(world: &World, corps: &[i64], event: &'static str, args: Value) {
    let w = world.lock();
    let clients: Vec<i64> = corps.iter().flat_map(|c| w.corp_members(*c)).collect();
    w.notify(&clients, event, args);
}

fn tell_executor(world: &World, members: &[i64], alliance_id: i64, was: i64, now: i64) {
    let mut c = Vec::new();
    c.push((Value::bytes("executorCorpID"), Value::Tuple(vec![Value::Int(was), Value::Int(now)])));
    tell(world, members, "OnAllianceChanged", Value::Tuple(vec![Value::Int(alliance_id), Value::Dict(c)]));
}

/// A corporation's alliance changed: its cached corporation row, then every
/// member's session.
fn tell_corporation_alliance(world: &World, corp_id: i64, was: Option<i64>, now: Option<i64>) {
    let c = Value::Dict(vec![(Value::bytes("allianceID"), Value::Tuple(vec![Value::int_or_none(was), Value::int_or_none(now)]))]);
    tell(world, &[corp_id], "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp_id), c]));
    world.lock().reaffiliate(&[corp_id], &[]);
}

// ---- relationships, wars and bills ----------------------------------------------------

/// SetRelationship(relationship, toID): NAP, friend, competitor or enemy,
/// toward a corporation or another alliance.
async fn set_relationship(ctx: &Ctx<'_>, alliance_id: i64, relationship: i64, to_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    if !RELATIONSHIPS.contains(&relationship) {
        return Err(Outcome::custom("That is not a relationship."));
    }
    let known = ctx.store.read(|db| {
        db.table("corporation").get_id(to_id).is_some()
            || db.table("alliances").get_id(to_id).is_some_and(|a| a.int("deleted") == Some(0))
    });
    if !known || to_id == alliance_id {
        return Err(Outcome::custom("Relationships are with other corporations and alliances."));
    }
    let old = ctx.store.read(|db| relationship_line(db, alliance_id, to_id));
    write(ctx.store, |tx| {
        tx.upsert(
            "allianceRelationships",
            &["fromID", "toID"],
            &[("fromID", id(alliance_id)), ("toID", id(to_id)), ("relationship", id(relationship))],
            &[("relationship", id(relationship))],
        )
        .map_err(|e| db_failed("setting the relationship", e))?;
        Ok(())
    })?;
    let new = vec![Value::Int(alliance_id), Value::Int(to_id), Value::Int(relationship)];
    tell_relationship(ctx, alliance_id, to_id, change(RELATIONSHIP_HEADER, old.as_deref(), Some(&new)))
}

async fn delete_relationship(ctx: &Ctx<'_>, alliance_id: i64, to_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    let Some(old) = ctx.store.read(|db| relationship_line(db, alliance_id, to_id)) else { return Ok(Value::None) };
    write(ctx.store, |tx| {
        tx.delete_where("allianceRelationships", &["fromID", "toID"], &[id(alliance_id), id(to_id)])
            .map_err(|e| db_failed("deleting the relationship", e))?;
        Ok(())
    })?;
    tell_relationship(ctx, alliance_id, to_id, change(RELATIONSHIP_HEADER, Some(&old), None))
}

fn relationship_line(db: &Db, alliance_id: i64, to_id: i64) -> Option<Vec<Value>> {
    db.table("allianceRelationships").get(&[id(alliance_id), id(to_id)]).map(|r| r.line(RELATIONSHIP_HEADER))
}

/// OnAllianceRelationshipChanged(allianceID, toID, change), to the members.
fn tell_relationship(ctx: &Ctx<'_>, alliance_id: i64, to_id: i64, change: Value) -> Refused<Value> {
    let members = ctx.store.read(|db| member_corps(db, alliance_id));
    let event = Value::Tuple(vec![Value::Int(alliance_id), Value::Int(to_id), change]);
    tell(ctx.world, &members, "OnAllianceRelationshipChanged", event);
    Ok(Value::None)
}

/// DeclareWarAgainst(ownerID): the executor pays for the first week.
async fn declare_war(ctx: &Ctx<'_>, alliance_id: i64, against: i64) -> Refused<Value> {
    let executor = require_executor_director(ctx, alliance_id)?;
    if ctx.store.read(|db| member_corps(db, alliance_id)).contains(&against) {
        return Err(Outcome::custom("You cannot declare war on a member of your own alliance."));
    }
    war::declare(ctx, alliance_id, executor, against).await
}

async fn retract_war(ctx: &Ctx<'_>, alliance_id: i64, war_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    war::retract(ctx, alliance_id, war_id).await
}

async fn mutual_war(ctx: &Ctx<'_>, alliance_id: i64, war_id: i64, mutual: bool) -> Refused<Value> {
    require_executor_director(ctx, alliance_id)?;
    war::set_mutual(ctx, alliance_id, war_id, mutual).await
}

/// GetBillBalance(billID): what is still owed on one of the alliance's bills.
fn bill_balance(ctx: &Ctx<'_>, alliance_id: i64, bill_id: i64) -> Refused<Value> {
    let owed = ctx.store.read(|db| {
        db.table("billsReceivable")
            .get_id(bill_id)
            .filter(|b| b.int("debtorID") == Some(alliance_id) && b.int("paid") == Some(0))
            .and_then(|b| b.float("amount"))
    });
    Ok(Value::Double(owed.unwrap_or(0.0)))
}

/// PayBill(billID): the executor's accountants pay the alliance's bill from
/// the executor corporation's wallet.
async fn pay_bill(ctx: &Ctx<'_>, alliance_id: i64, bill_id: i64) -> Refused<Value> {
    let executor = require_executor_role(ctx, alliance_id, CORP_ROLE_ACCOUNTANT, "accountants")?;
    match wallet::settle_bill(ctx, bill_id, alliance_id, executor).await {
        Outcome::Value(v) => Ok(v),
        refusal => Err(refusal),
    }
}

/// Bills every alliance for its members, once a fortnight from its founding;
/// the founding fee covers the first fortnight. Run by the heartbeat.
pub async fn bill_alliances(store: &Store) {
    let now = crate::bulkdata::filetime_now();
    let alliances: Vec<(i64, i64, i64)> = store.read(|db| {
        db.table("alliances")
            .rows()
            .filter(|a| a.int("deleted") == Some(0))
            .map(|a| (a.int("allianceID").unwrap_or(0), a.int("startDate").unwrap_or(now), a.int("memberCount").unwrap_or(0)))
            .collect()
    });
    for (alliance_id, founded, members) in alliances {
        let first = founded + MAINTENANCE_PERIOD;
        let amount = MEMBERSHIP_COST * members.max(1) as f64;
        let billed = store.write(|tx| -> Result<bool, StoreError> {
            Ok(match war::next_period(tx.db(), BILL_TYPE_ALLIANCE_MAINTENANCE, alliance_id, first, now) {
                war::Period::Due { until } => war::issue_bill(tx, BILL_TYPE_ALLIANCE_MAINTENANCE, alliance_id, amount, until, alliance_id).is_some(),
                _ => false,
            })
        });
        if billed == Ok(true) {
            info!("alliance {alliance_id} billed {amount} ISK for {members} member corporation(s)");
        }
    }
}
