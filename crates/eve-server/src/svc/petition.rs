//! `petitioner`: the help window's petitions (ui/shared/neocom/petition.py).
//!
//! A pilot files a petition in a category; it waits in its category's queue
//! until a petitionee (a session with ROLE_PETITIONEE) claims it. The two
//! then trade messages, each side hearing OnPetitionMessage as the other
//! writes; a petitionee may add comments, which the pilot never sees. The
//! petitionee closes it, hands it back, or escalates it to the queue the
//! category names. Every step is written to the petition's log.
//!
//! The categories, their extra fields and the queues are server data the
//! client only reads; db/petitionCategories.sql.gz and its neighbours hold a
//! set made for this server. Categories 1 and -5 open a web page in the
//! client and are left out; 4 and 5 carry the pilot's chat logs, and 7 no
//! combat log.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::ROLE_PETITIONEE;
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::membership::{db_failed, message, Refused};
use crate::svc::rows::{row, rowset};

/// const.maxPetitionsPerDay.
const MAX_PER_DAY: i64 = 8;
const DAY: i64 = 24 * 3600 * 10_000_000;
const TEXT_MAX: usize = 10_000;
/// const.petitionMaxChatLogSize and petitionMaxCombatLogSize.
const LOG_MAX: usize = 200_000;

/// The category group (1) every category belongs to unless it names
/// another.
pub fn ensure(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    tx.insert_or_ignore(
        "petitionCategoryGroups",
        &[("parentCategoryID", store::Value::Int(1)), ("displayName", store::Value::Text("Game support".into()))],
    )?;
    Ok(())
}

/// What the log records, as petitionLog.actionID, with the text it shows.
#[derive(Clone, Copy)]
enum Action {
    Created = 1,
    Claimed,
    Unclaimed,
    Escalated,
    Closed,
    Cancelled,
    Deleted,
    Message,
    Comment,
}

impl Action {
    fn text(self) -> &'static str {
        match self {
            Action::Created => "Petition created",
            Action::Claimed => "Petition claimed",
            Action::Unclaimed => "Petition unclaimed",
            Action::Escalated => "Petition escalated",
            Action::Closed => "Petition closed",
            Action::Cancelled => "Petition cancelled",
            Action::Deleted => "Petition deleted",
            Action::Message => "Message sent",
            Action::Comment => "Comment added",
        }
    }
}

/// The petition as the list entries and the view window read it
/// (PetitionField.Load and ShowPetition). touchDate is the last message, or
/// the filing; updated is 1 when the petitionee spoke last, which the queue
/// shows as "Waiting".
const PETITION_HEADER: &[&str] = &[
    "petitionID",
    "petitionerID",
    "categoryID",
    "queueID",
    "subject",
    "petition",
    "createDate",
    "touchDate",
    "claimed",
    "claimedBy",
    "closed",
    "closedAt",
    "deleted",
    "escalatesTo",
    "updated",
    "rating",
    "ratingComment",
    "ratingDateTime",
    "isOnline",
    "properties",
];
const MESSAGE_HEADER: &[&str] = &[
    "messageID",
    "petitionID",
    "senderID",
    "text",
    "sentDate",
    "comment",
    "read",
];
const LOG_HEADER: &[&str] = &[
    "logID",
    "petitionID",
    "logDateTime",
    "characterID",
    "eventText",
    "userName",
];

pub async fn petitioner(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let id = call.arg_id(0);
    let result = match call.method.as_str() {
        // CheckNewMessages reads .petitionID, .text and .messageID.
        "GetUnreadMessages" => {
            const HEADER: &[&str] = &["messageID", "petitionID", "text"];
            let lines = ctx.store.read(|db| {
                let messages = db.table("petitionMessages");
                let mut unread: Vec<Row> = db
                    .table("petitions")
                    .find(&["petitionerID"], &[store::Value::Int(me)])
                    .flat_map(|p| messages.find(&["petitionID"], std::slice::from_ref(p.value("petitionID"))))
                    .filter(|m| m.int("isPetitionee") == Some(1) && m.int("comment") == Some(0) && m.int("read") == Some(0))
                    .collect();
                unread.sort_by_key(|m| m.int("messageID"));
                unread.iter().map(|m| Value::List(m.line(HEADER))).collect()
            });
            Ok(rowset(HEADER, lines))
        }
        "MarkAsRead" => {
            let result = ctx.store.write(|tx| {
                let Some(m) = tx.table("petitionMessages").get_id(id) else { return Ok(()) };
                let party = m
                    .int("petitionID")
                    .and_then(|p| tx.table("petitions").get_id(p))
                    .is_some_and(|p| p.int("petitionerID") == Some(me) || p.int("claimedBy") == Some(me));
                if party {
                    tx.update("petitionMessages", m.id, &[("read", store::Value::Int(1))])?;
                }
                Ok::<_, StoreError>(())
            });
            logged(result);
            Ok(Value::None)
        }
        "GetMyPetitions" => Ok(petitions(ctx, |p| p.int("petitionerID") == Some(me) && p.int("deleted") == Some(0)).await),
        "GetMyPetitionsEx" => {
            let account = ctx.session.user_id.unwrap_or(-1);
            Ok(petitions(ctx, |p| (p.int("petitionerID") == Some(me) || p.int("accountID") == Some(account)) && p.int("deleted") == Some(0)).await)
        }
        "AddPetitionRating" | "UpdatePetitionRating" => rating(call,ctx).await,
        "GetClaimedPetitions" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, |p| p.int("claimedBy") == Some(me) && p.int("claimed") == Some(1) && p.int("closed") == Some(0)).await),
            Err(e) => Err(e),
        },
        "GetPetitionQueue" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, |p| {
                p.int("queueID") == Some(id) && p.int("claimed") == Some(0) && p.int("closed") == Some(0) && p.int("deleted") == Some(0)
            })
            .await),
            Err(e) => Err(e),
        },
        "GetPetition" => get_petition(ctx, id).await,
        "GetPetitionMessages" => messages(ctx, id).await,
        "GetPetitionLog" => log_of(ctx, id).await,
        "GetLog" => log_ids(ctx,id).await,
        "GetEvents" => Ok(rowset(&["eventID","eventText"],[Action::Created,Action::Claimed,Action::Unclaimed,Action::Escalated,Action::Closed,Action::Cancelled,Action::Deleted,Action::Message,Action::Comment].iter().map(|a|Value::List(vec![Value::Int(*a as i64),Value::text(a.text())])).collect())),
        "GetCategoryHierarchicalInfo" => hierarchy(ctx).await,
        "PropertyPopulationInfo" | "GetClientPickerInfo" => population(call,ctx).await,
        "GetCategories" => {
            const HEADER: &[&str] = &["categoryID", "displayName", "description", "escalatesTo"];
            Ok(rowset(HEADER, ctx.store.read(|db| ordered_lines(db, "petitionCategories", "categoryID", HEADER))))
        }
        "GetCategoryName" => {
            let name = ctx.store.read(|db| {
                db.table("petitionCategories").get_id(id).and_then(|c| c.text("displayName")).map(String::from)
            });
            Ok(Value::text(name.unwrap_or_default()))
        }
        "GetCategoryProperties" => {
            let lines = ctx.store.read(|db| {
                category_properties(db, id)
                    .iter()
                    .map(|p| {
                        // An empty description shows the field's name.
                        let description = match p.text("description") {
                            Some("") | None => p.cell("k"),
                            Some(_) => p.cell("description"),
                        };
                        Value::List(vec![p.cell("k"), p.cell("propertyID"), p.cell("k"), p.cell("required"), description, p.cell("inputType"), p.cell("inputInfo")])
                    })
                    .collect()
            });
            Ok(rowset(&["k","propertyID","propertyName","required","description","inputType","inputInfo"], lines))
        }
        "GetQueues" => {
            const HEADER: &[&str] = &["queueID", "queueName"];
            Ok(rowset(HEADER, ctx.store.read(|db| ordered_lines(db, "petitionQueues", "queueID", HEADER))))
        }
        "MayPetition" => Ok(Value::Int(may_petition(ctx, id, call.arg_i64(1)).await)),
        "CreatePetition" => create(ctx, call).await,
        "PetitionerChat" => chat(ctx, id, &call.arg_text(1).unwrap_or_default(), false, false).await,
        "PetitioneeChat" => {
            let comment = call.arg_i64(2).unwrap_or(0) != 0;
            chat(ctx, id, &call.arg_text(1).unwrap_or_default(), true, comment).await
        }
        "ClaimPetition" => claim(ctx, id).await,
        "UnClaimPetition" => unclaim(ctx, id, None).await,
        "EscalatePetition" => unclaim(ctx, id, Some(call.arg_i64(1))).await,
        "ClosePetition" => close(ctx, id).await,
        "CancelPetition" => withdraw(ctx, id, Action::Cancelled).await,
        "DeletePetition" => withdraw(ctx, id, Action::Deleted).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// A change whose failure the caller is not told about, only the log.
fn logged(result: Result<(), StoreError>) {
    if let Err(e) = result {
        error!("statement failed: {}", crate::logging::chain(&e));
    }
}

/// Every row of `table` ordered by its `key`, as `columns`.
fn ordered_lines(db: &Db, table: &str, key: &str, columns: &[&str]) -> Vec<Value> {
    let mut rows: Vec<Row> = db.table(table).rows().collect();
    rows.sort_by_key(|r| r.int(key));
    rows.iter().map(|r| Value::List(r.line(columns))).collect()
}

/// A category's extra fields, in their order.
fn category_properties(db: &Db, category: i64) -> Vec<Row<'_>> {
    let mut fields: Vec<Row> = db.table("petitionCategoryProperties").find(&["categoryID"], &[store::Value::Int(category)]).collect();
    fields.sort_by_key(|f| f.int("orderID"));
    fields
}

fn petitionee(ctx: &Ctx<'_>) -> Refused<()> {
    if ctx.session.role & ROLE_PETITIONEE == ROLE_PETITIONEE {
        Ok(())
    } else {
        Err(Outcome::custom("Only petitionees may do that."))
    }
}

async fn rating(call: &Call, ctx: &Ctx<'_>) -> Refused<Value> {
    let value = call.arg_id(1);
    let comment = call.arg_text(2).unwrap_or_default();
    if !(0..=100).contains(&value) || value % 10 != 0 || comment.chars().count() > 1000 {
        return Err(Outcome::custom(
            "Choose a rating from 0 to 100 and a comment of at most 1000 characters.",
        ));
    }
    let now = crate::bulkdata::filetime_now();
    let add = call.method == "AddPetitionRating";
    let (id, me) = (call.arg_id(0), ctx.session.char_id.unwrap_or(0));
    let rated = ctx.store.write(|tx| {
        // A new rating, or an edit within a week of the first.
        let Some(p) = tx.table("petitions").get_id(id).filter(|p| {
            let rated_at = p.int("ratingDateTime");
            p.int("petitionerID") == Some(me)
                && p.int("closed") == Some(1)
                && p.int("deleted") == Some(0)
                && if add { rated_at.is_none() } else { rated_at.is_some_and(|t| t > now - 7 * DAY) }
        }) else {
            return Ok(false);
        };
        let first = p.int("ratingDateTime").unwrap_or(now);
        tx.update(
            "petitions",
            p.id,
            &[("rating", store::Value::Int(value)), ("ratingComment", store::Value::Text(comment.as_str().into())), ("ratingDateTime", store::Value::Int(first))],
        )?;
        Ok::<_, StoreError>(true)
    }).map_err(|e|db_failed("saving the petition rating",e))?;
    if !rated {
        return Err(Outcome::custom("Only your own closed petitions can be rated; existing ratings can be edited for one week."));
    }
    Ok(Value::None)
}

async fn hierarchy(ctx: &Ctx<'_>) -> Refused<Value> {
    let (parents, children, descriptions) = ctx.store.read(|db| {
        let mut groups: Vec<Row> = db.table("petitionCategoryGroups").rows().collect();
        groups.sort_by_key(|r| r.int("parentCategoryID"));
        let mut categories: Vec<Row> = db.table("petitionCategories").rows().collect();
        categories.sort_by_key(|r| r.int("categoryID"));
        let parents: Vec<(Value, Value)> = groups
            .iter()
            .map(|r| {
                (
                    Value::Int(r.int("parentCategoryID").unwrap_or(0)),
                    Value::text(r.text("displayName").unwrap_or_default()),
                )
            })
            .collect();
        let mut children: std::collections::BTreeMap<i64, Vec<(Value, Value)>> =
            std::collections::BTreeMap::new();
        let mut descriptions = Vec::new();
        for r in categories {
            let id = Value::Int(r.int("categoryID").unwrap_or(0));
            children
                .entry(r.int("parentCategoryID").unwrap_or(1))
                .or_default()
                .push((id.clone(), Value::text(r.text("displayName").unwrap_or_default())));
            descriptions.push((id, Value::text(r.text("description").unwrap_or_default())));
        }
        (parents, children, descriptions)
    });
    Ok(Value::Tuple(vec![
        Value::Dict(parents),
        Value::Dict(
            children
                .into_iter()
                .map(|(id, entries)| (Value::Int(id), Value::Dict(entries)))
                .collect(),
        ),
        Value::Dict(descriptions),
    ]))
}

/// A petition's log entries in order, each as `entry` gives it with the
/// actor's account name (or '') after.
fn log_lines(db: &Db, id: i64, entry: impl Fn(&Row) -> Vec<Value>) -> Vec<Value> {
    let characters = db.table("character_");
    let accounts = db.table("account");
    let mut entries: Vec<Row> = db.table("petitionLog").find(&["petitionID"], &[store::Value::Int(id)]).collect();
    entries.sort_by_key(|l| l.int("logID"));
    entries
        .iter()
        .map(|l| {
            let account = l
                .int("actorID")
                .and_then(|actor| characters.get_id(actor))
                .and_then(|c| c.int("accountID"))
                .and_then(|a| accounts.get_id(a))
                .and_then(|a| a.text("accountName"));
            let mut line = entry(l);
            line.push(Value::text(account.unwrap_or("")));
            Value::List(line)
        })
        .collect()
}

async fn log_ids(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    parties(ctx, id).await?;
    let lines = ctx.store.read(|db| log_lines(db, id, |l| l.line(&["logID", "petitionID", "logDateTime", "actorID", "actionID"])));
    Ok(rowset(&["logID","petitionID","logDateTime","characterID","eventID","userName"], lines))
}

async fn population(call: &Call, ctx: &Ctx<'_>) -> Refused<Value> {
    let picker = call.method == "GetClientPickerInfo";
    let input = if picker {
        let property = call.arg_text(1).unwrap_or_default();
        let configured = ctx.store.read(|db| {
            db.table("petitionCategoryProperties")
                .rows()
                .filter(|p| p.text("k") == Some(property.as_str()) && p.text("inputType") == Some("Picker"))
                .min_by_key(|p| p.int("propertyID"))
                .and_then(|p| p.text("inputInfo"))
                .map(String::from)
        });
        configured.unwrap_or(property)
    } else {
        call.arg_text(0).unwrap_or_default()
    };
    let filter = if picker {
        call.arg_text(0).unwrap_or_default().to_lowercase()
    } else {
        String::new()
    };
    if picker && filter.chars().count() < 3 {
        return Ok(Value::List(vec![]));
    }
    let (table, id, name) = match input.to_lowercase().as_str() {
        "character" | "characters" | "pilot name" => ("character_", "characterID", "characterName"),
        "corporation" | "corporations" => ("corporation", "corporationID", "corporationName"),
        "solar system" | "solarsystem" | "solarsystems" => ("mapSolarSystems", "solarSystemID", "solarSystemName"),
        "station" | "stations" => ("staStations", "stationID", "stationName"),
        "type" | "types" | "item" | "items" => ("invTypes", "typeID", "typeName"),
        _ => return Ok(Value::List(vec![])),
    };
    // Choosing a character for a petition filed from character selection
    // offers only the account's own.
    let account = (!picker && table == "character_").then(|| ctx.session.user_id.unwrap_or(-1));
    let named = |found: Option<&str>| found.filter(|n| n.to_lowercase().contains(&filter)).map(String::from);
    let mut found: Vec<(String, Vec<Value>)> = if table == "invTypes" {
        crate::sde::table(table).rows().filter_map(|r| Some((named(r.text(name))?, r.line(&[id, name])))).collect()
    } else {
        ctx.store.read(|db| {
            db.table(table)
                .rows()
                .filter(|r| account.is_none_or(|a| r.int("accountID") == Some(a)))
                .filter_map(|r| Some((named(r.text(name))?, r.line(&[id, name]))))
                .collect()
        })
    };
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::List(
        found
            .into_iter()
            .take(25)
            .map(|(_, mut cells)| {
                if picker {
                    cells.swap(0, 1)
                }
                Value::Tuple(cells)
            })
            .collect(),
    ))
}

/// A petition's columns of PETITION_HEADER, up to the pilot's being online.
fn petition_line(db: &Db, p: &Row) -> Vec<Value> {
    let messages: Vec<Row> = db.table("petitionMessages").find(&["petitionID"], std::slice::from_ref(p.value("petitionID"))).collect();
    let touched = messages.iter().filter_map(|m| m.int("created")).max().or(p.int("createdAt"));
    // Who spoke last (comments aside): 1 for the petitionee.
    let updated = messages
        .iter()
        .filter(|m| m.int("comment") == Some(0))
        .max_by_key(|m| m.int("messageID"))
        .and_then(|m| m.int("isPetitionee"))
        .unwrap_or(0);
    let mut line = p.line(&["petitionID", "petitionerID", "categoryID", "queueID", "subject", "petition", "createdAt"]);
    line.push(touched.map_or(Value::None, Value::Long));
    line.extend(p.line(&["claimed", "claimedBy", "closed", "closedAt", "deleted", "escalatesTo"]));
    line.push(Value::Int(updated));
    line.extend(p.line(&["rating", "ratingComment", "ratingDateTime"]));
    line
}

/// Petitions matching `filter`, each with whether its pilot is online and
/// its extra fields.
async fn petitions(ctx: &Ctx<'_>, filter: impl Fn(&Row) -> bool) -> Value {
    let found: Vec<(Vec<Value>, i64, Option<Vec<(Value, Value)>>)> = ctx.store.read(|db| {
        let mut matching: Vec<Row> = db.table("petitions").rows().filter(|p| filter(p)).collect();
        matching.sort_by_key(|p| p.int("petitionID"));
        let properties = db.table("petitionProperties");
        matching
            .iter()
            .map(|p| {
                let fields: Vec<(Value, Value)> = properties
                    .find(&["petitionID"], std::slice::from_ref(p.value("petitionID")))
                    .map(|f| (Value::text(f.text("k").unwrap_or_default()), Value::text(f.text("v").unwrap_or_default())))
                    .collect();
                (petition_line(db, p), p.int("petitionerID").unwrap_or(0), (!fields.is_empty()).then_some(fields))
            })
            .collect()
    });
    let world = ctx.world.lock();
    let lines = found
        .into_iter()
        .map(|(mut l, petitioner, fields)| {
            l.push(Value::Int(i64::from(world.is_online(petitioner))));
            l.push(fields.map_or(Value::None, Value::Dict));
            Value::List(l)
        })
        .collect();
    rowset(PETITION_HEADER, lines)
}

/// A petition's petitioner and claimant, if the caller may see it: its
/// petitioner, or a petitionee.
async fn parties(ctx: &Ctx<'_>, id: i64) -> Refused<Parties> {
    let p = ctx
        .store
        .read(|db| {
            db.table("petitions").get_id(id).filter(|r| r.int("deleted") == Some(0)).map(|r| Parties {
                petitioner: r.int("petitionerID").unwrap_or(0),
                claimant: r.int("claimedBy").filter(|_| r.int("claimed") == Some(1)),
                closed: r.int("closed") == Some(1),
                escalates_to: r.int("escalatesTo"),
            })
        })
        .ok_or_else(|| Outcome::custom("That petition does not exist."))?;
    if Some(p.petitioner) != ctx.session.char_id && petitionee(ctx).is_err() {
        return Err(Outcome::custom("That is not your petition."));
    }
    Ok(p)
}

struct Parties {
    petitioner: i64,
    claimant: Option<i64>,
    closed: bool,
    escalates_to: Option<i64>,
}

async fn get_petition(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let found = petitions(ctx, |p| p.int("petitionID") == Some(id)).await;
    let line = match &found {
        Value::Instance { state, .. } => match state.as_ref() {
            Value::Dict(d) => d
                .iter()
                .find(|(k, _)| *k == Value::bytes("lines"))
                .and_then(|(_, v)| v.seq())
                .and_then(|l| l.first().cloned()),
            _ => None,
        },
        _ => None,
    };
    Ok(match line {
        Some(Value::List(l)) => row(PETITION_HEADER, l),
        _ => Value::None,
    })
}

/// The messages; comments only for petitionees.
async fn messages(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let comments = i64::from(petitionee(ctx).is_ok());
    let lines = ctx.store.read(|db| {
        let mut found: Vec<Row> = db
            .table("petitionMessages")
            .find(&["petitionID"], &[store::Value::Int(id)])
            .filter(|m| m.int("comment").is_some_and(|c| c <= comments))
            .collect();
        found.sort_by_key(|m| m.int("messageID"));
        found
            .iter()
            .map(|m| Value::List(m.line(&["messageID", "petitionID", "senderID", "text", "created", "comment", "read"])))
            .collect()
    });
    Ok(rowset(MESSAGE_HEADER, lines))
}

async fn log_of(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let lines = ctx.store.read(|db| {
        log_lines(db, id, |l| {
            let mut line = l.line(&["logID", "petitionID", "logDateTime", "actorID"]);
            line.push(Value::text(l.text("message").unwrap_or("")));
            line
        })
    });
    Ok(rowset(LOG_HEADER, lines))
}

/// MayPetition(categoryID, OocCharacterID): 1, or the client's refusals:
/// -1 one open petition per category, -2 too many today, -3 no character,
/// -4 not one of the account's characters.
async fn may_petition(ctx: &Ctx<'_>, category: i64, ooc: Option<i64>) -> i64 {
    let since = crate::bulkdata::filetime_now() - DAY;
    ctx.store.read(|db| {
        let Some(char_id) = petitioner_of(db, ctx, ooc) else {
            return if ctx.session.char_id.is_none() && ooc.is_none() {
                -3
            } else {
                -4
            };
        };
        let (open, today) = filed_by(db, char_id, category, since);
        if open > 0 {
            return -1;
        }
        if today >= MAX_PER_DAY {
            return -2;
        }
        1
    })
}

/// A pilot's open petitions in `category`, and how many it filed after
/// `since`.
fn filed_by(db: &Db, char_id: i64, category: i64, since: i64) -> (i64, i64) {
    let mut open = 0;
    let mut recent = 0;
    for p in db.table("petitions").find(&["petitionerID"], &[store::Value::Int(char_id)]) {
        if p.int("categoryID") == Some(category) && p.int("closed") == Some(0) && p.int("deleted") == Some(0) {
            open += 1;
        }
        if p.int("createdAt").is_some_and(|t| t > since) {
            recent += 1;
        }
    }
    (open, recent)
}

/// The character a petition is filed for: the session's, or (filed from
/// character selection) one of the account's.
fn petitioner_of(db: &Db, ctx: &Ctx<'_>, ooc: Option<i64>) -> Option<i64> {
    if let Some(char_id) = ctx.session.char_id {
        return Some(char_id);
    }
    let ooc = ooc?;
    let account = ctx.session.user_id.unwrap_or(0);
    db.table("character_").get_id(ooc).filter(|c| c.int("accountID") == Some(account)).map(|_| ooc)
}

/// CreatePetition(subject, petition, categoryID, properties, OocCharacterID,
/// combatLog=None, chatLog=None): files it in its category's first queue.
async fn create(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let category = call.arg_id(2);
    let Some(char_id) = ctx.store.read(|db| petitioner_of(db, ctx, call.arg_i64(4))) else {
        return Ok(Value::Bool(false));
    };
    let subject = call.arg_text(0).unwrap_or_default();
    let body = call.arg_text(1).unwrap_or_default();
    if subject.trim().is_empty()
        || subject.chars().count() > 200
        || body.trim().is_empty()
        || body.chars().count() > TEXT_MAX
    {
        return Err(Outcome::custom("Enter a subject of at most 200 characters and petition text of at most 10000 characters."));
    }
    let log = |name: &str, index: usize| {
        call.kwarg(name)
            .or(call.args.get(index))
            .and_then(Value::as_string)
            .map(|s| {
                s.chars()
                    .rev()
                    .take(LOG_MAX)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            })
    };
    let chat = log("chatLog", 5);
    let combat = log("combatLog", 6);
    let text_or_null = |s: &Option<String>| s.as_deref().map_or(store::Value::Null, |s| store::Value::Text(s.into()));
    let account = ctx.session.user_id.unwrap_or(0);
    // Every refusal comes before the first change, so a refused petition
    // leaves nothing behind.
    let filed = ctx.store.write(|tx| -> Result<Refused<Option<i64>>, StoreError> {
        let now = crate::bulkdata::filetime_now();
        let (open, today) = filed_by(tx.db(), char_id, category, now - DAY);
        if open > 0 || today >= MAX_PER_DAY {
            return Ok(Ok(None));
        }
        let definitions: Vec<(i64, String, bool)> = category_properties(tx.db(), category)
            .iter()
            .map(|r| (r.int("propertyID").unwrap_or(0), r.text("k").unwrap_or_default().to_string(), r.bool("required").unwrap_or(false)))
            .collect();
        let mut values: HashMap<String, String> = HashMap::new();
        if let Some(Value::Dict(fields)) = call.args.get(3) {
            for (k, v) in fields {
                if let (Some(k), Some(v)) = (
                    k.as_string(),
                    v.as_string().or_else(|| v.as_i64().map(|n| n.to_string())),
                ) {
                    values.insert(k, v);
                }
            }
        }
        if let Some(entries) = call.args.get(7).and_then(Value::seq) {
            for entry in entries {
                let Some(pair) = entry.seq().filter(|p| p.len() == 2) else {
                    return Ok(Err(Outcome::custom("Invalid petition property.")));
                };
                let id = pair[0].as_i64().unwrap_or(0);
                let Some(definition) = definitions.iter().find(|(property, _, _)| *property == id) else {
                    return Ok(Err(Outcome::custom("That field does not belong to this petition category.")));
                };
                let value = pair[1]
                    .as_string()
                    .or_else(|| pair[1].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                values.insert(definition.1.clone(), value);
            }
        }
        for (_, name, required) in &definitions {
            let value = values.get(name).map(String::as_str).unwrap_or("");
            if *required && (value.trim().is_empty() || value == "-1") {
                return Ok(Err(Outcome::custom(format!(
                    "Complete the required field: {name}."
                ))));
            }
            if value.chars().count() > 200 {
                return Ok(Err(Outcome::custom(format!("The {name} field is too long."))));
            }
        }
        let Some(escalates_to) = tx.table("petitionCategories").get_id(category).map(|c| c.value("escalatesTo").clone()) else {
            return Ok(Err(Outcome::custom("That petition category does not exist.")));
        };
        let queue = tx.table("petitionQueues").rows().filter_map(|q| q.int("queueID")).min();
        let id = tx.nextval("petitions_petitionID_seq")?;
        tx.insert(
            "petitions",
            &[
                ("petitionID", store::Value::Int(id)),
                ("petitionerID", store::Value::Int(char_id)),
                ("accountID", store::Value::Int(account)),
                ("categoryID", store::Value::Int(category)),
                ("queueID", queue.map_or(store::Value::Null, store::Value::Int)),
                ("subject", store::Value::Text(subject.as_str().into())),
                ("petition", store::Value::Text(body.as_str().into())),
                ("createdAt", store::Value::Int(now)),
                ("escalatesTo", escalates_to),
                ("combatLog", text_or_null(&combat)),
                ("chatLog", text_or_null(&chat)),
            ],
        )?;
        for (_, key, _) in definitions {
            if let Some(value) = values.get(&key) {
                tx.insert(
                    "petitionProperties",
                    &[("petitionID", store::Value::Int(id)), ("k", store::Value::Text(key.into())), ("v", store::Value::Text(value.as_str().into()))],
                )?;
            }
        }
        record(tx, id, char_id, Action::Created)?;
        Ok(Ok(Some(id)))
    });
    let Some(id) = filed.map_err(|e| db_failed("filing the petition", e))?? else {
        return Ok(Value::Bool(false));
    };
    info!("{char_id} filed petition {id} in category {category}: {subject:?}");
    let w = ctx.world.lock();
    w.notify(
        &w.with_role(ROLE_PETITIONEE),
        "OnNewPetition",
        Value::Tuple(vec![
            Value::Int(id),
            Value::Int(char_id),
            Value::text(subject),
            Value::Int(category),
        ]),
    );
    Ok(Value::Bool(true))
}

/// A message from the pilot, or from the petitionee who has it (a comment
/// stays among petitionees). Both sides hear it, the writer included, so an
/// open view reloads.
async fn chat(
    ctx: &Ctx<'_>,
    id: i64,
    text_in: &str,
    from_petitionee: bool,
    comment: bool,
) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if from_petitionee {
        petitionee(ctx)?;
    } else if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    if p.closed {
        return Err(message("PetitionClosed"));
    }
    let body: String = text_in.chars().take(TEXT_MAX).collect();
    let now = crate::bulkdata::filetime_now();
    let saved = ctx.store.write(|tx| {
        let message_id = tx.nextval("petitionMessages_messageID_seq")?;
        tx.insert(
            "petitionMessages",
            &[
                ("messageID", store::Value::Int(message_id)),
                ("petitionID", store::Value::Int(id)),
                ("senderID", store::Value::Int(me)),
                ("isPetitionee", store::Value::Int(i64::from(from_petitionee))),
                ("text", store::Value::Text(body.as_str().into())),
                ("created", store::Value::Int(now)),
                ("comment", store::Value::Int(i64::from(comment))),
            ],
        )?;
        record(
            tx,
            id,
            me,
            if comment {
                Action::Comment
            } else {
                Action::Message
            },
        )?;
        Ok::<_, StoreError>(message_id)
    });
    let message_id = saved.map_err(|e| {
        error!("query failed: {}", crate::logging::chain(&e));
        Outcome::custom("The message could not be saved.")
    })?;
    let mut to = vec![me];
    to.extend(p.claimant);
    if !comment {
        to.push(p.petitioner);
    }
    tell(
        ctx,
        &to,
        "OnPetitionMessage",
        vec![
            Value::Int(id),
            Value::Int(me),
            Value::text(&body),
            Value::Int(message_id),
        ],
    );
    Ok(Value::None)
}

/// ClaimPetition(petitionID): 1 if the caller now has it, 0 if someone else
/// got there first.
async fn claim(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let claimed = ctx.store.write(|tx| {
        let Some((row, petitioner)) = tx.table("petitions").get_id(id).and_then(|p| {
            let open = p.int("claimed") == Some(0) && p.int("closed") == Some(0) && p.int("deleted") == Some(0);
            // A petitionee cannot take their own petition.
            let petitioner = p.int("petitionerID").filter(|petitioner| *petitioner != me)?;
            open.then_some((p.id, petitioner))
        }) else {
            return Ok(None);
        };
        tx.update("petitions", row, &[("claimed", store::Value::Int(1)), ("claimedBy", store::Value::Int(me))])?;
        record(tx, id, me, Action::Claimed)?;
        Ok::<_, StoreError>(Some(petitioner))
    });
    let petitioner = match claimed {
        Ok(Some(petitioner)) => petitioner,
        Ok(None) => return Ok(Value::Int(0)),
        Err(e) => {
            error!("query failed: {}", crate::logging::chain(&e));
            return Ok(Value::Int(0));
        }
    };
    info!("{me} claimed petition {id}");
    tell(
        ctx,
        &[petitioner],
        "OnAssignPetition",
        vec![Value::Int(id), Value::Int(me)],
    );
    Ok(Value::Int(1))
}

/// UnClaimPetition(petitionID) puts it back in its queue;
/// EscalatePetition(petitionID, escalatesTo) into the queue above.
async fn unclaim(ctx: &Ctx<'_>, id: i64, escalate: Option<Option<i64>>) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.claimant != Some(me) {
        return Err(Outcome::custom("You have not claimed that petition."));
    }
    let queue = match escalate {
        None => None,
        Some(to) => Some(
            to.or(p.escalates_to)
                .ok_or_else(|| Outcome::custom("That petition cannot be escalated any further."))?,
        ),
    };
    logged(ctx.store.write(|tx| {
        if let Some(row) = tx.table("petitions").get_id(id).map(|r| r.id) {
            let mut sets = vec![("claimed", store::Value::Int(0)), ("claimedBy", store::Value::Null)];
            // Escalated, it moves up a queue and can go no higher there.
            if let Some(queue) = queue {
                sets.push(("queueID", store::Value::Int(queue)));
                sets.push(("escalatesTo", store::Value::Null));
            }
            tx.update("petitions", row, &sets)?;
        }
        record(
            tx,
            id,
            me,
            if queue.is_some() {
                Action::Escalated
            } else {
                Action::Unclaimed
            },
        )
    }));
    info!(
        "{me} {} petition {id}",
        if queue.is_some() {
            "escalated"
        } else {
            "unclaimed"
        }
    );
    tell(
        ctx,
        &[p.petitioner],
        "OnAssignPetition",
        vec![Value::Int(id), Value::None],
    );
    Ok(Value::None)
}

async fn close(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    let now = crate::bulkdata::filetime_now();
    logged(ctx.store.write(|tx| {
        if let Some(row) = tx.table("petitions").get_id(id).filter(|r| r.int("closed") == Some(0)).map(|r| r.id) {
            tx.update("petitions", row, &[("closed", store::Value::Int(1)), ("closedAt", store::Value::Int(now))])?;
        }
        record(tx, id, me, Action::Closed)
    }));
    info!("{me} closed petition {id}");
    tell(
        ctx,
        &[p.petitioner],
        "OnClosePetition",
        vec![Value::Int(id)],
    );
    Ok(Value::None)
}

/// The pilot takes back a petition nobody has claimed (CancelPetition), or
/// clears away a closed one (DeletePetition).
async fn withdraw(ctx: &Ctx<'_>, id: i64, how: Action) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    let now = crate::bulkdata::filetime_now();
    let withdrawn = ctx.store.write(|tx| {
        let Some(r) = tx.table("petitions").get_id(id) else { return Ok(false) };
        let (row, sets) = match how {
            Action::Cancelled if r.int("claimed") == Some(0) => {
                (r.id, vec![("deleted", store::Value::Int(1)), ("closed", store::Value::Int(1)), ("closedAt", store::Value::Int(now))])
            }
            Action::Cancelled => return Ok(false),
            _ if r.int("closed") == Some(1) => {
                (r.id, vec![("deleted", store::Value::Int(1)), ("closedAt", store::Value::Int(r.int("closedAt").unwrap_or(now)))])
            }
            _ => return Ok(false),
        };
        tx.update("petitions", row, &sets)?;
        record(tx, id, me, how)?;
        Ok::<_, StoreError>(true)
    });
    if !withdrawn.unwrap_or_else(|e| {
        error!("statement failed: {}", crate::logging::chain(&e));
        false
    }) {
        return Err(Outcome::custom(match how {
            Action::Cancelled => "A petition someone has claimed cannot be cancelled.",
            _ => "Only a closed petition can be deleted.",
        }));
    }
    info!("{me}: {} ({id})", how.text());
    Ok(Value::None)
}

fn record(tx: &mut Tx<'_>, id: i64, actor: i64, what: Action) -> Result<(), StoreError> {
    let log_id = tx.nextval("petitionLog_logID_seq")?;
    tx.insert(
        "petitionLog",
        &[
            ("logID", store::Value::Int(log_id)),
            ("petitionID", store::Value::Int(id)),
            ("logDateTime", store::Value::Int(crate::bulkdata::filetime_now())),
            ("actorID", store::Value::Int(actor)),
            ("actionID", store::Value::Int(what as i64)),
            ("message", store::Value::Text(what.text().into())),
        ],
    )?;
    Ok(())
}

fn tell(ctx: &Ctx<'_>, chars: &[i64], event: &'static str, args: Vec<Value>) {
    let w = ctx.world.lock();
    let mut clients: Vec<i64> = chars.iter().flat_map(|c| w.clients_of(*c)).collect();
    clients.sort_unstable();
    clients.dedup();
    w.notify(&clients, event, Value::Tuple(args));
}
