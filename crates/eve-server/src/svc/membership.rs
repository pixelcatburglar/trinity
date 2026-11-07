//! Player corporations: founding one, the application negotiation between a
//! character and a corporation, and leaving one. Whoever changes corporation
//! has their connection told to read its affiliation again
//! (`world::Move::Affiliation`), which sends the session change.

use std::collections::HashSet;

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::rows::{change, index_rowset, rowset};
use crate::svc::skills;
use crate::svc::wallet::{self, Entry, Transfer};

/// eveConstants crpApplication*: who last moved an application, and how.
const APPLIED_BY_CHARACTER: i16 = 0;
const RENEGOTIATED_BY_CHARACTER: i16 = 1;
const ACCEPTED_BY_CHARACTER: i16 = 2;
const REJECTED_BY_CHARACTER: i16 = 3;
const REJECTED_BY_CORPORATION: i16 = 4;
const RENEGOTIATED_BY_CORPORATION: i16 = 5;
const ACCEPTED_BY_CORPORATION: i16 = 6;

/// eveConstants corporationStartupCost.
const STARTUP_COST: f64 = 1_599_800.0;
/// market_refTypes: Corporation Registration Fee.
const REF_CORPORATION_REGISTRATION_FEE: i32 = 39;
const CORP_ROLE_PERSONNEL_MANAGER: i64 = 128;
/// Every corpRole* bit eveConstants holds (Director, then bits 7 to 55,
/// Starbase Config among them): what a CEO is given.
pub const ALL_ROLES: i64 = 0x00FF_FFFF_FFFF_FF81;
/// Shares issued to a corporation's founder.
const FOUNDER_SHARES: i64 = 1000;
/// typeCorporationManagement: needed to found a corporation.
const SKILL_CORPORATION_MANAGEMENT: i32 = 3363;
/// The skills that raise a CEO's member limit, and by how much per level
/// (Corporation Management, Megacorp Management, Empire Control, Sovereignty).
pub(crate) const MEMBER_SKILLS: [(i32, i64); 4] = [(3363, 10), (3731, 50), (3732, 200), (12241, 1000)];

const APPLICATION_HEADER: &[&str] =
    &["corporationID", "characterID", "applicationText", "applicationDateTime", "status"];

pub type Refused<T> = std::result::Result<T, Outcome>;

/// A refusal from inside a store write, which undoes the write.
pub(crate) struct Undone(pub Outcome);

impl From<Outcome> for Undone {
    fn from(o: Outcome) -> Self {
        Undone(o)
    }
}

impl From<StoreError> for Undone {
    fn from(e: StoreError) -> Self {
        Undone(db_failed("writing", e))
    }
}

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

fn text(v: &str) -> store::Value {
    store::Value::Text(v.into())
}

/// corpRegistry calls that change who belongs to which corporation.
pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let status = |i: usize| call.arg_i64(i).unwrap_or(-1) as i16;
    let result = match call.method.as_str() {
        "GetSuggestedTickerNames" => {
            let tickers = free_tickers(ctx, "corporation", "tickerName", &text_arg(0)).await;
            Ok(rowset(&["tickerName"], tickers.into_iter().map(|t| Value::List(vec![Value::text(t)])).collect()))
        }
        "AddCorporation" => add_corporation(ctx, call).await,
        "GetMyApplications" => Ok(my_applications(ctx)),
        "GetApplications" => Ok(corp_applications(ctx)),
        "InsertApplication" => insert_application(ctx, call.arg_id(0), &text_arg(1)).await,
        "UpdateApplication" => answer_offer(ctx, call.arg_id(0), &text_arg(1), status(2)).await,
        "UpdateApplicationOffer" => make_offer(ctx, call.arg_id(0), &text_arg(1), status(2)).await,
        "DeleteApplication" => delete_application(ctx, call.arg_id(0), call.arg_id(1)).await,
        "KickOutMember" => kick_out(ctx, call.arg_id(0)).await,
        "UpdateMember" => {
            let roles: Vec<Option<i64>> = (4..12).map(|i| call.arg_i64(i)).collect();
            update_member(ctx, call.arg_id(0), call.arg_text(1), &roles, call.arg_i64(13)).await
        }
        "UpdateMembers" => update_members(ctx, call.args.first()).await,
        "ExecuteActions" => execute_actions(ctx, call.args.first(), call.args.get(1)).await,
        "CanBeKickedOut" => Ok(Value::Bool(can_be_kicked_out(ctx, call.arg_id(0)))),
        "UpdateDivisionNames" => crate::svc::corphangar::update_division_names(ctx, &call.args).await.map_err(Outcome::custom),
        "UpdateCorporation" => crate::svc::corphangar::update_corporation(ctx, &call.args).await.map_err(Outcome::custom),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

// ---- founding --------------------------------------------------------------------------

/// AddCorporation(name, ticker, description, url, taxRate, shape1..3,
/// color1..3, typeface): founded at the station the character is in, which
/// becomes its HQ, with the founder as CEO.
async fn add_corporation(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    let Some(station_id) = ctx.session.station_id else { return Err(message("CanOnlyCreateCorpInStation")) };
    let name = call.arg_text(0).unwrap_or_default().trim().to_string();
    let ticker = call.arg_text(1).unwrap_or_default().trim().to_string();
    let description = call.arg_text(2).unwrap_or_default();
    let url = call.arg_text(3).unwrap_or_default();
    let tax_rate = call.args.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
    let opt_int = |i: usize| call.arg_i64(i).map_or(store::Value::Null, int);
    let typeface = call.arg_text(11);

    if let Some(key) = name_error("Corp", &name) {
        return Err(message(&key));
    }
    if !ticker_ok(&ticker) {
        return Err(message("CorpTickerNameInvalid"));
    }
    if ctx.store.read(|db| is_ceo(db, char_id))? {
        return Err(message("CEOCannotCreateCorporation"));
    }
    let levels = ctx
        .store
        .read(|db| skills::levels(db, char_id, &MEMBER_SKILLS.map(|(s, _)| s)))
        .map_err(|e| db_failed("reading corporation skills", e))?;
    if levels.get(&SKILL_CORPORATION_MANAGEMENT).copied().unwrap_or(0) < 1 {
        return Err(Outcome::UserError {
            msg: "PlayerCantCreateCorporation".into(),
            args: vec![("cost".into(), Value::Double(STARTUP_COST))],
        });
    }
    let member_limit: i64 = MEMBER_SKILLS.iter().map(|(s, per)| levels.get(s).copied().unwrap_or(0) * per).sum();

    let (balance, corp_id, left) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            if exists(tx.db(), "corporation", "corporationName", &name) {
                return Err(message("CorpNameInvalidTaken").into());
            }
            if exists(tx.db(), "corporation", "tickerName", &ticker) {
                return Err(message("CorpTickerNameInvalidTaken").into());
            }

            let entry = Entry {
                ref_type: REF_CORPORATION_REGISTRATION_FEE,
                payer: char_id,
                payee: 0,
                arg_id1: "",
                reason: "",
            };
            let balance = wallet::post(tx, char_id, -STARTUP_COST, &entry).map_err(transfer_refused)?;

            let now = crate::bulkdata::filetime_now();
            // The founder's race is the corporation's.
            let race = tx.table("character_").get_id(char_id).map(|c| c.value("raceID").clone()).ok_or_else(|| {
                db_failed("inserting the corporation", StoreError { code: store::DATA_EXCEPTION, message: format!("no character {char_id}") })
            })?;
            let row = tx
                .insert(
                    "corporation",
                    &[
                        ("corporationName", text(&name)),
                        ("tickerName", text(&ticker)),
                        ("description", text(&description)),
                        ("url", text(&url)),
                        ("taxRate", store::Value::Float(tax_rate)),
                        ("creatorID", int(char_id)),
                        ("ceoID", int(char_id)),
                        ("stationID", int(station_id)),
                        ("raceID", race),
                        ("memberLimit", int(member_limit)),
                        ("shape1", opt_int(5)),
                        ("shape2", opt_int(6)),
                        ("shape3", opt_int(7)),
                        ("color1", opt_int(8)),
                        ("color2", opt_int(9)),
                        ("color3", opt_int(10)),
                        ("typeface", typeface.as_deref().map_or(store::Value::Null, text)),
                    ],
                )
                .map_err(|e| db_failed("inserting the corporation", e))?;
            let corp_id = tx.table("corporation").row(row).and_then(|c| c.int("corporationID")).unwrap_or(0);
            tx.insert(
                "crpCharShares",
                &[("characterID", int(char_id)), ("corporationID", int(corp_id)), ("shares", int(FOUNDER_SHARES))],
            )
            .map_err(|e| db_failed("issuing the founder's shares", e))?;
            tx.update("corporation", row, &[("shares", int(FOUNDER_SHARES))]).map_err(|e| db_failed("recording the shares", e))?;
            let left = change_corporation(tx, char_id, corp_id, ALL_ROLES, now)?;
            Ok((balance, corp_id, left))
        })
        .map_err(|Undone(o)| o)?;
    member_moved(ctx.world, char_id, left, corp_id);

    info!("founded corporation {name:?} [{ticker}] ({corp_id}) at station {station_id}, member limit {member_limit}");
    let world = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(char_id), Value::Double(balance)]);
    world.notify(&world.clients_of(char_id), "OnAccountChange", cash);
    world.reaffiliate(&[], &[char_id]);
    Ok(Value::Int(corp_id))
}

// ---- applications ----------------------------------------------------------------------

/// The applications (not deleted) with `column` equal to `id`.
fn applications<'a>(db: &'a Db, column: &str, id: i64) -> impl Iterator<Item = Row<'a>> + 'a {
    db.table("crpApplications").find(&[column], &[int(id)]).filter(|r| r.int("deleted") == Some(0))
}

fn my_applications(ctx: &Ctx<'_>) -> Value {
    let char_id = ctx.session.char_id.unwrap_or(0);
    ctx.store.read(|db| {
        rowset(APPLICATION_HEADER, applications(db, "characterID", char_id).map(|r| Value::List(r.line(APPLICATION_HEADER))).collect())
    })
}

/// The applications to the caller's corporation, for its personnel managers.
fn corp_applications(ctx: &Ctx<'_>) -> Value {
    if !may_manage_personnel(ctx) {
        return index_rowset(APPLICATION_HEADER, "characterID", Vec::new());
    }
    let corp = ctx.session.corp_id.unwrap_or(0);
    let items = ctx.store.read(|db| {
        applications(db, "corporationID", corp)
            .map(|r| (Value::Int(r.int("characterID").unwrap_or(0)), Value::List(r.line(APPLICATION_HEADER))))
            .collect()
    });
    index_rowset(APPLICATION_HEADER, "characterID", items)
}

/// InsertApplication(corporationID, text): a character asks to join.
async fn insert_application(ctx: &Ctx<'_>, corp_id: i64, note: &str) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    if ctx.session.corp_id == Some(corp_id) {
        return Err(Outcome::custom("You are already a member of that corporation."));
    }
    if !ctx.store.read(|db| is_player_corp(db, corp_id))? {
        return Err(Outcome::custom("That corporation does not take applications."));
    }
    if ctx.store.read(|db| application(db, corp_id, char_id))?.is_some() {
        return Err(message("CrpApplicationAlreadyExists"));
    }
    let now = crate::bulkdata::filetime_now();
    ctx.store
        .write(|tx| {
            let filed = [
                ("applicationText", text(note)),
                ("applicationDateTime", int(now)),
                ("status", int(APPLIED_BY_CHARACTER.into())),
                ("corporationText", text("")),
                ("deleted", int(0)),
            ];
            let mut sets = vec![("corporationID", int(corp_id)), ("characterID", int(char_id))];
            sets.extend(filed.iter().cloned());
            tx.upsert("crpApplications", &["corporationID", "characterID"], &sets, &filed)
        })
        .map_err(|e| db_failed("filing the application", e))?;
    let new = application_line(corp_id, char_id, note, now, APPLIED_BY_CHARACTER);
    info!("{char_id} applied to join corporation {corp_id}");
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, None, Some(&new)));
    Ok(Value::None)
}

/// UpdateApplicationOffer(characterID, text, status): the corporation answers.
async fn make_offer(ctx: &Ctx<'_>, char_id: i64, note: &str, status: i16) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !may_manage_personnel(ctx) {
        return Err(access_denied("you do not hold the Personnel Manager role"));
    }
    let Some(old) = ctx.store.read(|db| application(db, corp_id, char_id))? else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    let allowed: &[i16] = match application_status(&old) {
        APPLIED_BY_CHARACTER | RENEGOTIATED_BY_CHARACTER => {
            &[REJECTED_BY_CORPORATION, RENEGOTIATED_BY_CORPORATION, ACCEPTED_BY_CORPORATION]
        }
        REJECTED_BY_CHARACTER => &[RENEGOTIATED_BY_CORPORATION],
        _ => &[],
    };
    if !allowed.contains(&status) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    let new = ctx.store.write(|tx| set_application(tx, &old, note, status).map_err(Undone)).map_err(|Undone(o)| o)?;
    info!("corporation {corp_id} answered {char_id}'s application with status {status}");
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
    Ok(Value::None)
}

/// UpdateApplication(corporationID, text, status): the character answers the
/// corporation; accepting its offer makes them a member.
async fn answer_offer(ctx: &Ctx<'_>, corp_id: i64, note: &str, status: i16) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    let Some(old) = ctx.store.read(|db| application(db, corp_id, char_id))? else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    let allowed: &[i16] = match application_status(&old) {
        REJECTED_BY_CORPORATION => &[RENEGOTIATED_BY_CHARACTER],
        RENEGOTIATED_BY_CORPORATION => &[RENEGOTIATED_BY_CHARACTER, REJECTED_BY_CHARACTER],
        ACCEPTED_BY_CORPORATION => &[ACCEPTED_BY_CHARACTER, REJECTED_BY_CHARACTER, RENEGOTIATED_BY_CHARACTER],
        _ => &[],
    };
    if !allowed.contains(&status) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    if status != ACCEPTED_BY_CHARACTER {
        let new = ctx.store.write(|tx| set_application(tx, &old, note, status).map_err(Undone)).map_err(|Undone(o)| o)?;
        tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
        return Ok(Value::None);
    }

    if ctx.session.station_id.is_none() {
        return Err(message("CrpCanNotChangeCorpInSpace"));
    }
    if ctx.store.read(|db| is_ceo(db, char_id))? {
        let names = ctx.store.read(|db| {
            let corps = db.table("corporation");
            let old = corps.get_id(db.table("character_").get_id(char_id)?.int("corporationID")?)?;
            let new = corps.get_id(corp_id)?;
            Some([old.text("corporationName"), new.text("corporationName")].map(|n| n.unwrap_or_default().to_string()))
        });
        let name = |i: usize| Value::text(names.as_ref().map(|n| n[i].clone()).unwrap_or_default());
        return Err(Outcome::UserError {
            msg: "CeoCanNotJoinACorp".into(),
            args: vec![("CEOsCorporation".into(), name(0)), ("otherCorporation".into(), name(1))],
        });
    }

    let (new, left) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let room = tx.table("corporation").get_id(corp_id).map(|c| (c.int("memberCount").unwrap_or(0), c.int("memberLimit").unwrap_or(0)));
            let (count, limit) = room.ok_or_else(|| {
                db_failed("reading the member limit", StoreError { code: store::DATA_EXCEPTION, message: format!("no corporation {corp_id}") })
            })?;
            if count >= limit {
                return Err(Outcome::UserError {
                    msg: "CorporationMembershipFull".into(),
                    args: vec![("limit".into(), Value::Int(limit)), ("count".into(), Value::Int(count))],
                }
                .into());
            }
            let new = set_application(tx, &old, note, status)?;
            let left = change_corporation(tx, char_id, corp_id, 0, crate::bulkdata::filetime_now())?;
            Ok((new, left))
        })
        .map_err(|Undone(o)| o)?;

    info!("{char_id} left corporation {left} and joined {corp_id}");
    member_moved(ctx.world, char_id, left, corp_id);
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

/// DeleteApplication(corporationID, characterID): withdrawn by the character,
/// or cleared away by the corporation.
async fn delete_application(ctx: &Ctx<'_>, corp_id: i64, char_id: i64) -> Refused<Value> {
    let own = ctx.session.char_id == Some(char_id);
    let corp_side = ctx.session.corp_id == Some(corp_id) && may_manage_personnel(ctx);
    if !own && !corp_side {
        return Err(access_denied("that application is not yours to delete"));
    }
    let Some(old) = ctx.store.read(|db| application(db, corp_id, char_id))? else { return Ok(Value::None) };
    ctx.store
        .write(|tx| tx.delete_where("crpApplications", &["corporationID", "characterID"], &[int(corp_id), int(char_id)]))
        .map_err(|e| db_failed("deleting the application", e))?;
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), None));
    Ok(Value::None)
}

/// OnCorporationApplicationChanged(applicantID, corporationID, change), to the
/// applicant and the corporation's members.
fn tell_application(ctx: &Ctx<'_>, corp_id: i64, char_id: i64, change: Value) {
    let world = ctx.world.lock();
    let mut clients = world.clients_of(char_id);
    clients.extend(world.corp_members(corp_id));
    clients.sort_unstable();
    clients.dedup();
    let event = Value::Tuple(vec![Value::Int(char_id), Value::Int(corp_id), change]);
    world.notify(&clients, "OnCorporationApplicationChanged", event);
}

fn application(db: &Db, corp_id: i64, char_id: i64) -> Refused<Option<Vec<Value>>> {
    let found = db.table("crpApplications").get(&[int(corp_id), int(char_id)]).filter(|r| r.int("deleted") == Some(0));
    Ok(found.map(|r| r.line(APPLICATION_HEADER)))
}

fn application_status(line: &[Value]) -> i16 {
    line.get(4).and_then(|v| v.as_i64()).unwrap_or(-1) as i16
}

fn application_line(corp_id: i64, char_id: i64, note: &str, when: i64, status: i16) -> Vec<Value> {
    vec![Value::Int(corp_id), Value::Int(char_id), Value::text(note), Value::Long(when), Value::Int(status.into())]
}

/// Moves an application to `status` with new text; returns its new line.
fn set_application(tx: &mut Tx<'_>, old: &[Value], note: &str, status: i16) -> Refused<Vec<Value>> {
    let id = |i: usize| old.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
    let (corp_id, char_id) = (id(0), id(1));
    if let Some(row) = tx.table("crpApplications").get(&[int(corp_id), int(char_id)]).map(|r| r.id) {
        tx.update("crpApplications", row, &[("applicationText", text(note)), ("status", int(status.into()))])
            .map_err(|e| db_failed("updating the application", e))?;
    }
    let when = old.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(application_line(corp_id, char_id, note, when, status))
}

// ---- leaving ---------------------------------------------------------------------------

/// KickOutMember(characterID): a member quits (kicking themselves), or a
/// director puts one out. They go back to the NPC corporation they last
/// belonged to.
pub async fn kick_out(ctx: &Ctx<'_>, char_id: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let own = ctx.session.char_id == Some(char_id);
    if !own && !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may kick members out"));
    }
    let member = ctx.store.read(|db| member_of(db, char_id));
    let Some((_, ceo, _)) = member.filter(|m| m.0 == Some(corp_id)) else { return Err(message("CrpCharNotInThisCorp")) };
    if ceo == Some(char_id) {
        return Err(access_denied("the CEO cannot leave the corporation"));
    }
    let in_space = if own { ctx.session.station_id.is_none() } else { ctx.world.lock().is_in_space(char_id) };
    if in_space {
        return Err(message(if own { "CrpCanNotChangeCorpInSpace" } else { "CrpCanNotKickMemberInSpace" }));
    }

    let home = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            // The NPC corporation they joined last.
            let home = tx
                .table("chrEmployment")
                .find(&["characterID"], &[int(char_id)])
                .filter(|e| e.int("corporationID").is_some_and(|c| npc_corporations().contains(&c)))
                .filter_map(|e| Some((e.int("startDate")?, e.int("corporationID")?)))
                .max_by_key(|(start, _)| *start)
                .map(|(_, corp)| corp)
                .ok_or_else(|| Outcome::custom("There is no NPC corporation for them to go back to."))?;
            change_corporation(tx, char_id, home, 0, crate::bulkdata::filetime_now())?;
            Ok(home)
        })
        .map_err(|Undone(o)| o)?;
    member_moved(ctx.world, char_id, corp_id, home);

    if own {
        info!("{char_id} quit corporation {corp_id} for {home}");
    } else {
        info!("{char_id} was kicked out of corporation {corp_id} into {home}");
    }
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

// ---- roles -----------------------------------------------------------------------------

/// The role columns UpdateMember takes, in its argument order after the
/// title, division and squadron (corp_dlg_edit_member.py).
const ROLE_COLUMNS: [&str; 8] = [
    "rolesAtAll", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// UpdateMember(charID, title, divisionID, squadronID, roles, grantableRoles,
/// rolesAtHQ, grantableRolesAtHQ, rolesAtBase, grantableRolesAtBase,
/// rolesAtOther, grantableRolesAtOther, baseID, titleMask); None leaves a
/// value as it is. The CEO and directors manage members; only the CEO makes
/// or manages directors, and nobody changes the CEO.
async fn update_member(ctx: &Ctx<'_>, char_id: i64, title: Option<String>, roles: &[Option<i64>], title_mask: Option<i64>) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let (_, ceo, held) = ctx
        .store
        .read(|db| member_of(db, char_id))
        .filter(|m| m.0 == Some(corp_id))
        .ok_or_else(|| message("CrpCharNotInThisCorp"))?;
    let (ceo, held) = (ceo.unwrap_or(0), held);
    let caller_is_ceo = me == ceo;
    if !caller_is_ceo && !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only the CEO and directors may change members"));
    }
    if char_id == ceo {
        return Err(access_denied("the CEO's roles cannot be changed"));
    }
    let director = |mask: i64| mask & CORP_ROLE_DIRECTOR == CORP_ROLE_DIRECTOR;
    if !caller_is_ceo && (director(held) || roles.iter().flatten().any(|m| director(*m))) {
        return Err(access_denied("only the CEO may appoint or manage directors"));
    }

    let title_mask = title_mask.map(crate::svc::corp::titles::checked_mask).transpose()?;
    if let Some(mask) = title_mask {
        let old = ctx
            .store
            .write(|tx| match tx.table("character_").get_id(char_id).map(|c| (c.id, c.int("titleMask").unwrap_or(0))) {
                Some((row, old)) => tx.update("character_", row, &[("titleMask", int(mask))]).map(|_| Some(old)),
                None => Ok(None),
            })
            .map_err(|e| db_failed("setting the titles", e))?;
        if let Some(old) = old.filter(|old| *old != mask) {
            let w = ctx.world.lock();
            let diff = Value::Dict(vec![(Value::bytes("titleMask"), Value::Tuple(vec![Value::Long(old), Value::Long(mask)]))]);
            w.notify(&w.corp_members(corp_id), "OnCorporationMemberChanged", Value::Tuple(vec![Value::Int(char_id), diff]));
        }
    }
    if let Some(title) = &title {
        ctx.store
            .write(|tx| match tx.table("character_").get_id(char_id).map(|c| c.id) {
                Some(row) => tx.update("character_", row, &[("title", text(title))]),
                None => Ok(()),
            })
            .map_err(|e| db_failed("setting the title", e))?;
    }
    if roles.iter().any(Option::is_some) {
        // A member without a roles row starts from none; what is not given stays.
        ctx.store
            .write(|tx| -> Result<_, Undone> {
                tx.insert_or_ignore("chrCorporationRoles", &[("characterID", int(char_id))])
                    .map_err(|e| db_failed("adding the roles", e))?;
                let set: Vec<(&str, store::Value)> =
                    ROLE_COLUMNS.iter().zip(roles).filter_map(|(c, r)| r.map(|r| (*c, int(r)))).collect();
                if let Some(row) = tx.table("chrCorporationRoles").get_id(char_id).map(|r| r.id) {
                    tx.update("chrCorporationRoles", row, &set).map_err(|e| db_failed("setting the roles", e))?;
                }
                Ok(())
            })
            .map_err(|Undone(o)| o)?;
        if roles.first().copied().flatten().is_some_and(director) {
            info!("{me} made {char_id} a director of {corp_id}");
        }
    }
    info!("{me} updated member {char_id} of corporation {corp_id}");
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

/// UpdateMembers(rows): a util.Rowset of [characterID, title, divisionID,
/// squadronID, roles, grantableRoles, ... titleMask], one UpdateMember each.
async fn update_members(ctx: &Ctx<'_>, rows: Option<&Value>) -> Refused<Value> {
    let lines = match rows {
        Some(Value::Instance { state, .. }) => match state.as_ref() {
            Value::Dict(d) => d.iter().find(|(k, _)| *k == Value::bytes("lines")).and_then(|(_, v)| v.seq()).map(|l| l.to_vec()),
            _ => None,
        },
        _ => None,
    };
    for l in lines.unwrap_or_default() {
        let Some(l) = l.seq() else { continue };
        let char_id = l.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let title = l.get(1).and_then(|v| v.as_string());
        let roles: Vec<Option<i64>> = (4..12).map(|i| l.get(i).and_then(|v| v.as_i64())).collect();
        update_member(ctx, char_id, title, &roles, l.get(13).and_then(|v| v.as_i64())).await?;
    }
    Ok(Value::None)
}

/// corp_ui_member_view_task_mgt.py / eveConstants CTV_* and CTPG_*.
const CTV_ADD: i64 = 1;
const CTV_REMOVE: i64 = 2;
const CTV_SET: i64 = 3;
const CTV_GIVE: i64 = 4;
const CTPG_CASH: i64 = 6;
const CTPG_SHARES: i64 = 7;
/// market_refTypes refCorporationAccountWithdrawal.
const REF_CORPORATION_ACCOUNT_WITHDRAWAL: i32 = 37;

/// The bitmask properties an action adds to or removes from, as
/// UpdateMember's role arguments are ordered (`ROLE_COLUMNS`).
const ACTION_ROLE_PROPERTIES: [&str; 8] = [
    "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// What one ExecuteActions action does to every target.
enum Action {
    /// Add (true) or remove the bits of one role column (`ROLE_COLUMNS` index).
    Role(usize, bool, i64),
    Titles(bool, i64),
    Base(Option<i64>),
    Cash(f64),
    Shares(i64),
}

fn parse_action(a: &Value) -> Option<Action> {
    let [verb, property, value] = a.seq()? else { return None };
    let verb = verb.as_i64()?;
    let add = verb == CTV_ADD;
    match (verb, property) {
        (CTV_ADD | CTV_REMOVE, p) => {
            let name = p.as_string()?;
            let bits = value.as_i64()?;
            if name == "titleMask" {
                return Some(Action::Titles(add, bits));
            }
            ACTION_ROLE_PROPERTIES.iter().position(|r| *r == name).map(|i| Action::Role(i, add, bits))
        }
        (CTV_SET, p) if p.as_string().as_deref() == Some("baseID") => Some(Action::Base(value.as_i64().filter(|b| *b > 0))),
        (CTV_GIVE, p) if p.as_i64() == Some(CTPG_CASH) => value.as_f64().filter(|v| v.is_finite() && *v > 0.0).map(Action::Cash),
        (CTV_GIVE, p) if p.as_i64() == Some(CTPG_SHARES) => value.as_i64().filter(|v| *v > 0).map(Action::Shares),
        _ => None,
    }
}

/// ExecuteActions(targetIDs, actions): the member task manager's actions,
/// [verb, property, value] each, applied to every target in turn
/// (corp_ui_member_view_task_mgt.py:150-159; bco_members.py:181-194 keeps
/// chat and mail invitations on the client). Roles and titles go through
/// UpdateMember, with its rules; cash comes from the corporation's wallet and
/// shares from the corporation's own holding.
async fn execute_actions(ctx: &Ctx<'_>, targets: Option<&Value>, actions: Option<&Value>) -> Refused<Value> {
    let targets: Vec<i64> = targets.and_then(Value::seq).unwrap_or(&[]).iter().filter_map(Value::as_i64).collect();
    let actions: Vec<Action> = actions
        .and_then(Value::seq)
        .unwrap_or(&[])
        .iter()
        .map(|a| parse_action(a).ok_or_else(|| Outcome::custom("One of those actions cannot be carried out.")))
        .collect::<Refused<_>>()?;
    if targets.is_empty() || actions.is_empty() {
        return Ok(Value::None);
    }
    if targets.len() > 1000 || actions.len() > 50 {
        return Err(Outcome::custom("Choose fewer members or actions."));
    }
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let ceo = ctx.store.read(|db| db.table("corporation").get_id(corp_id).and_then(|c| c.int("ceoID")));
    if ceo != Some(me) && !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only the CEO and directors may act on members"));
    }
    for &target in &targets {
        let current = ctx.store.read(|db| {
            let c = db.table("character_").get_id(target).filter(|c| c.int("corporationID") == Some(corp_id))?;
            let roles = db.table("chrCorporationRoles").get_id(target);
            Some((ROLE_COLUMNS.map(|col| roles.and_then(|r| r.int(col)).unwrap_or(0)), c.int("titleMask").unwrap_or(0)))
        });
        let Some((mut roles, mut titles)) = current else { return Err(message("CrpCharNotInThisCorp")) };
        let (before_roles, before_titles) = (roles, titles);
        for action in &actions {
            match *action {
                Action::Role(i, add, bits) => roles[i] = if add { roles[i] | bits } else { roles[i] & !bits },
                Action::Titles(add, bits) => titles = if add { titles | bits } else { titles & !bits },
                Action::Base(base) => set_base(ctx, corp_id, target, base)?,
                Action::Cash(amount) => {
                    let payment = crate::svc::wallet::Payment {
                        payer: corp_id,
                        payee: Some(target),
                        amount,
                        ref_type: REF_CORPORATION_ACCOUNT_WITHDRAWAL,
                        arg_id1: "",
                        reason: "",
                    };
                    crate::svc::wallet::move_cash(ctx.store, payment).await.map_err(transfer_refused)?;
                    wallet::notify_balance(ctx, corp_id).await;
                    wallet::notify_balance(ctx, target).await;
                }
                Action::Shares(n) => {
                    let args = [Value::Int(corp_id), Value::Int(target), Value::Int(n)];
                    crate::svc::corp::shares::move_shares(ctx, &args, true).await?;
                }
            }
        }
        if roles != before_roles || titles != before_titles {
            let changed: Vec<Option<i64>> = roles.iter().zip(before_roles).map(|(r, b)| (*r != b).then_some(*r)).collect();
            update_member(ctx, target, None, &changed, (titles != before_titles).then_some(titles)).await?;
        }
    }
    info!("{me} carried out {} action(s) on {} member(s) of {corp_id}", actions.len(), targets.len());
    Ok(Value::None)
}

/// A member's base: one of the corporation's office stations, or none.
fn set_base_checked(db: &Db, corp_id: i64, base: Option<i64>) -> bool {
    base.is_none_or(|station| db.table("crpOffices").find(&["stationID"], &[int(station)]).any(|o| o.int("corporationID") == Some(corp_id)))
}

fn set_base(ctx: &Ctx<'_>, corp_id: i64, char_id: i64, base: Option<i64>) -> Refused<()> {
    ctx.store
        .write(|tx| -> Result<(), Undone> {
            if !set_base_checked(tx.db(), corp_id, base) {
                return Err(Outcome::custom("A member's base must be a station where the corporation has an office.").into());
            }
            if let Some(row) = tx.table("character_").get_id(char_id).map(|c| c.id) {
                tx.update("character_", row, &[("baseID", base.map_or(store::Value::Null, int))])?;
            }
            Ok(())
        })
        .map_err(|Undone(o)| o)?;
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(())
}

fn can_be_kicked_out(ctx: &Ctx<'_>, char_id: i64) -> bool {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    ctx.store
        .read(|db| member_of(db, char_id))
        .is_some_and(|(corp, ceo, _)| corp == Some(corp_id) && ceo.is_some_and(|ceo| ceo != char_id))
}

/// A character's corporation, its CEO, and the character's roles (0 without
/// a roles row); None for a character without a corporation that exists.
fn member_of(db: &Db, char_id: i64) -> Option<(Option<i64>, Option<i64>, i64)> {
    let corp = db.table("character_").get_id(char_id)?.int("corporationID");
    let ceo = db.table("corporation").get_id(corp?)?.int("ceoID");
    let roles = db.table("chrCorporationRoles").get_id(char_id).and_then(|r| r.int("rolesAtAll")).unwrap_or(0);
    Some((corp, ceo, roles))
}

// ---- shared ----------------------------------------------------------------------------

/// Moves a character into `corp_id` with `roles`, inside the caller's
/// transaction: membership counts, the employment record and the roles.
/// Returns the corporation they left.
pub fn change_corporation(tx: &mut Tx<'_>, char_id: i64, corp_id: i64, roles: i64, now: i64) -> Refused<i64> {
    let (row, old_corp) = tx
        .table("character_")
        .get_id(char_id)
        .map(|c| (c.id, c.int("corporationID").unwrap_or(0)))
        .ok_or_else(|| db_failed("moving the character", StoreError { code: store::DATA_EXCEPTION, message: format!("no character {char_id}") }))?;
    tx.update("character_", row, &[("corporationID", int(corp_id)), ("corporationDateTime", int(now))])
        .map_err(|e| db_failed("moving the character", e))?;
    let counts = [(old_corp, -1), (corp_id, 1)];
    for (corp, delta) in counts {
        let Some((row, count)) = tx.table("corporation").get_id(corp).map(|c| (c.id, c.int("memberCount").unwrap_or(0))) else { continue };
        tx.update("corporation", row, &[("memberCount", int((count + delta).max(0)))]).map_err(|e| db_failed("counting members", e))?;
    }
    tx.insert(
        "chrEmployment",
        &[("characterID", int(char_id)), ("corporationID", int(corp_id)), ("startDate", int(now)), ("deleted", int(0))],
    )
    .map_err(|e| db_failed("recording the employment", e))?;
    let reset: Vec<(&str, store::Value)> = [
        ("corprole", 0),
        ("rolesAtAll", roles),
        ("rolesAtBase", 0),
        ("rolesAtHQ", 0),
        ("rolesAtOther", 0),
        ("grantableRoles", roles),
        ("grantableRolesAtHQ", 0),
        ("grantableRolesAtBase", 0),
        ("grantableRolesAtOther", 0),
    ]
    .into_iter()
    .map(|(c, v)| (c, int(v)))
    .collect();
    tx.upsert(
        "chrCorporationRoles",
        &["characterID"],
        &[("characterID", int(char_id)), ("rolesAtAll", int(roles)), ("grantableRoles", int(roles))],
        &reset,
    )
    .map_err(|e| db_failed("setting the roles", e))?;
    Ok(old_corp)
}

/// Tells both corporations' members, and the character, that they moved, so
/// open member lists follow (bco_members.py:170). Called after the commit.
pub fn member_moved(world: &crate::world::World, char_id: i64, old_corp: i64, new_corp: i64) {
    let w = world.lock();
    let mut clients: Vec<i64> = w.corp_members(old_corp);
    clients.extend(w.corp_members(new_corp));
    clients.extend(w.clients_of(char_id));
    clients.sort_unstable();
    clients.dedup();
    let change = Value::Dict(vec![(Value::bytes("corporationID"), Value::Tuple(vec![Value::Int(old_corp), Value::Int(new_corp)]))]);
    w.notify(&clients, "OnCorporationMemberChanged", Value::Tuple(vec![Value::Int(char_id), change]));
}

/// The NPC corporations (crpNPCCorporations), which no player runs.
fn npc_corporations() -> &'static HashSet<i64> {
    static IDS: std::sync::OnceLock<HashSet<i64>> = std::sync::OnceLock::new();
    IDS.get_or_init(|| crate::sde::table("crpNPCCorporations").rows().filter_map(|r| r.int("corporationID")).collect())
}

/// Whether the character is the CEO of a player corporation.
pub fn is_ceo(db: &Db, char_id: i64) -> Refused<bool> {
    let ceo_of = db.table("corporation").find(&["ceoID"], &[int(char_id)]);
    Ok(ceo_of.filter_map(|co| co.int("corporationID")).any(|id| !npc_corporations().contains(&id)))
}

pub fn is_player_corp(db: &Db, corp_id: i64) -> Refused<bool> {
    let corp = db.table("corporation").get_id(corp_id).filter(|co| co.int("deleted") == Some(0));
    Ok(corp.is_some() && !npc_corporations().contains(&corp_id))
}

/// Whether a row of `table` has `value` in `column`, ignoring case (as
/// SQL's `lower()` compares): a name in use.
pub fn exists(db: &Db, table: &str, column: &str, value: &str) -> bool {
    let value = value.to_lowercase();
    db.table(table).rows().any(|r| r.text(column).is_some_and(|t| t.to_lowercase() == value))
}

fn may_manage_personnel(ctx: &Ctx<'_>) -> bool {
    ctx.session.has_corp_role(CORP_ROLE_PERSONNEL_MANAGER) || ctx.session.has_corp_role(CORP_ROLE_DIRECTOR)
}

/// The message key for what is wrong with a corporation or alliance name
/// (`prefix` "Corp" or "Alliance": CorpNameInvalidMinLength and so on).
pub fn name_error(prefix: &str, name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let problem = if chars.len() < 4 {
        "MinLength"
    } else if chars.len() > 24 {
        "MaxLength"
    } else if !chars.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '.' | '\'')) {
        "SomeChar"
    } else if !chars[0].is_ascii_alphanumeric() {
        "FirstChar"
    } else if !chars[chars.len() - 1].is_ascii_alphanumeric() && chars[chars.len() - 1] != '.' {
        "LastChar"
    } else {
        return None;
    };
    Some(format!("{prefix}NameInvalid{problem}"))
}

/// A corporation ticker or alliance short name: one to five letters, digits,
/// dots, dashes or underscores.
pub fn ticker_ok(ticker: &str) -> bool {
    (1..=5).contains(&ticker.chars().count())
        && ticker.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Tickers made from a name: its initials, its first letters, then those
/// with a digit; the ones no row of `table` holds in `column` (the tickers
/// in use, compared in upper case).
pub async fn free_tickers(ctx: &Ctx<'_>, table: &str, column: &str, name: &str) -> Vec<String> {
    let candidates = ticker_candidates(name);
    let taken: HashSet<String> = ctx.store.read(|db| db.table(table).rows().filter_map(|r| r.text(column)).map(str::to_uppercase).collect());
    candidates.into_iter().filter(|c| !taken.contains(c)).take(8).collect()
}

fn ticker_candidates(name: &str) -> Vec<String> {
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect();
    let letters: String = words.concat();
    let initials: String = words.iter().filter_map(|w| w.chars().next()).take(5).collect();
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if ticker_ok(&t) && !out.contains(&t) {
            out.push(t);
        }
    };
    if !initials.is_empty() {
        add(initials.clone());
    }
    for n in (2..=5).rev() {
        add(letters.chars().take(n).collect());
    }
    let stem: String = initials.chars().take(4).collect();
    if !stem.is_empty() {
        for d in 1..=9 {
            add(format!("{stem}{d}"));
        }
    }
    out
}

pub fn message(key: &str) -> Outcome {
    Outcome::UserError { msg: key.into(), args: Vec::new() }
}

pub fn access_denied(reason: &str) -> Outcome {
    Outcome::UserError { msg: "CrpAccessDenied".into(), args: vec![("reason".into(), Value::text(reason))] }
}

pub fn db_failed(what: &str, e: StoreError) -> Outcome {
    error!("{what}: {}", crate::logging::chain(&e));
    Outcome::custom("The corporation registry could not do that just now.")
}

pub fn transfer_refused(t: Transfer) -> Outcome {
    match t {
        Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
        Transfer::NoSuchOwner => Outcome::custom("That wallet does not exist."),
        Transfer::Db(e) => db_failed("paying", e),
    }
}
