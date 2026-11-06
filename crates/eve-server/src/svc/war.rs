//! Wars between player corporations and alliances (`crpWars`).
//!
//! An alliance's executor declares war, paying CONCORD up front for the first
//! week; after that the declarer is billed weekly ([`bill_wars`]), and an
//! unpaid bill ends the war. A war turns hostile a day after it is declared
//! and stops a day after it is retracted (cfg.IsWarInHostileState). The
//! defender may make it mutual, which stops the bills. A corporation joining
//! an alliance hands its wars to the alliance.
//!
//! CONCORD (concord.rs) runs inside the world lock and cannot ask the
//! database, so the wars in force are kept in memory ([`at_war`]), reloaded
//! by the heartbeat and after every change.

use std::collections::BTreeMap;
use std::sync::RwLock;

use eve_marshal::Value;
use tracing::{error, info};

use crate::ballpark::Pilot;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError, Tx};
use crate::svc::bound;
use crate::svc::membership::{db_failed, message, transfer_refused, Refused};
use crate::svc::rows::{change, index_rowset};
use crate::svc::wallet::{self, Entry};
use crate::world::World;

const WAR: &str = "warRegistry";

pub const DAY: i64 = 24 * 60 * 60 * 10_000_000;
pub const WEEK: i64 = 7 * DAY;
/// What declaring (and each week of) a war costs, before it grows with the
/// victim's other wars.
const WAR_BASE_COST: f64 = 2_000_000.0;
/// market_refTypes / eveConstants refWarFee.
const REF_WAR_FEE: i32 = 49;
/// billTypes: War Bill.
pub const BILL_TYPE_WAR: i32 = 4;
/// eveConstants ownerCONCORD: who war and alliance bills are owed to.
pub const OWNER_CONCORD: i64 = 1_000_125;

/// The `crpWars` columns the client is sent.
const WAR_HEADER: &[&str] = &[
    "warID", "declaredByID", "againstID", "timeDeclared", "timeFinished", "retracted", "mutual",
];

/// A refusal inside a store write, which undoes what the write did.
struct Undo(Outcome);

impl From<Outcome> for Undo {
    fn from(refusal: Outcome) -> Self {
        Undo(refusal)
    }
}

impl From<StoreError> for Undo {
    fn from(e: StoreError) -> Self {
        Undo(db_failed("changing the wars", e))
    }
}

/// warRegistry, bound as (allianceOrCorpID, isMaster).
pub async fn war_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let owner = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    let asked = call.arg_i64(0).or(owner).or(ctx.session.corp_id).unwrap_or(0);
    Some(match call.method.as_str() {
        "GetWars" => {
            let items = ctx.store.read(|db| {
                let wars = db.table("crpWars");
                let side = |column: &str| wars.find(&[column], &[store::Value::Int(asked)]);
                let by_id: BTreeMap<i64, Vec<Value>> =
                    side("declaredByID").chain(side("againstID")).map(|r| (r.int("warID").unwrap_or(0), r.line(WAR_HEADER))).collect();
                by_id.into_iter().map(|(id, line)| (Value::Int(id), Value::List(line))).collect()
            });
            Outcome::Value(index_rowset(WAR_HEADER, "warID", items))
        }
        "GetCostOfStartingAWarAgainst" => Outcome::Value(Value::Double(ctx.store.read(|db| cost_against(db, asked)))),
        method => Outcome::Unknown(format!("{WAR}::{method}")),
    })
}

/// corpRegistry RetractWar(warID) and ChangeMutualWarFlag(warID, mutual),
/// for a corporation outside any alliance; its CEO decides.
pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if !matches!(call.method.as_str(), "RetractWar" | "ChangeMutualWarFlag") {
        return None;
    }
    let result = async {
        let corp_id = ctx.session.corp_id.unwrap_or(0);
        let ceo = ctx.store.read(|db| db.table("corporation").get_id(corp_id).and_then(|r| r.int("ceoID")));
        if ctx.session.alliance_id.is_some() || ceo != ctx.session.char_id {
            return Err(message("MustBeActiveCEOToEndWar"));
        }
        if call.method == "RetractWar" {
            retract(ctx, corp_id, call.arg_id(0)).await
        } else {
            set_mutual(ctx, corp_id, call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await
        }
    }
    .await;
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// The base cost, once more for every other non-mutual war in force
/// against the victim.
fn cost_against(db: &Db, against: i64) -> f64 {
    let wars = db
        .table("crpWars")
        .find(&["againstID"], &[store::Value::Int(against)])
        .filter(|w| w.int("mutual") == Some(0) && w.value("retracted").is_null())
        .count() as i64;
    WAR_BASE_COST * (1 + wars) as f64
}

/// `declarer` (an alliance, or a corporation outside one) declares war on
/// `against`; `payer` is the corporation whose wallet pays the first week.
/// The caller has checked that the character may act for the declarer.
pub async fn declare(ctx: &Ctx<'_>, declarer: i64, payer: i64, against: i64) -> Refused<Value> {
    if against == declarer {
        return Err(Outcome::custom("You cannot declare war on yourself."));
    }
    let (is_alliance, corp_alliance) = ctx.store.read(|db| {
        let alliance = db.table("alliances").get_id(against).is_some_and(|a| a.int("deleted") == Some(0));
        (alliance, db.table("corporation").get_id(against).and_then(|c| c.int("allianceID")))
    });
    let npc = crate::sde::table("crpNPCCorporations").get("corporationID", against).is_some();
    if npc {
        return Err(message("CrpCantDeclareWarOnNPCCorp"));
    }
    if !is_alliance && corp_alliance.is_none() {
        return Err(Outcome::custom("There is no such corporation or alliance."));
    }
    if corp_alliance.is_some_and(|a| a != 0) {
        return Err(message("CrpCanNotSanctionWarAgainstOwner"));
    }

    let now = crate::bulkdata::filetime_now();
    let (war_id, cost) = ctx
        .store
        .write(|tx| {
            let already = tx
                .table("crpWars")
                .find(&["declaredByID"], &[store::Value::Int(declarer)])
                .any(|w| w.int("againstID") == Some(against) && w.int("retracted").is_none_or(|r| r > now - DAY));
            if already {
                return Err(Undo(Outcome::custom("You are already at war with them.")));
            }
            let cost = cost_against(tx.db(), against);
            let entry = Entry { ref_type: REF_WAR_FEE, payer, payee: OWNER_CONCORD, arg_id1: "", reason: "" };
            wallet::post(tx, payer, -cost, &entry).map_err(transfer_refused)?;
            wallet::post(tx, OWNER_CONCORD, cost, &entry).map_err(transfer_refused)?;
            let war_id = tx.nextval("dyn_id_seq").map_err(|e| db_failed("recording the war", e))?;
            tx.insert(
                "crpWars",
                &[
                    ("warID", store::Value::Int(war_id)),
                    ("declaredByID", store::Value::Int(declarer)),
                    ("againstID", store::Value::Int(against)),
                    ("timeDeclared", store::Value::Int(now)),
                    ("mutual", store::Value::Int(0)),
                ],
            )
            .map_err(|e| db_failed("recording the war", e))?;
            Ok((war_id, cost))
        })
        .map_err(|Undo(refusal)| refusal)?;

    info!("{declarer} declared war on {against} (war {war_id}) for {cost} ISK");
    ctx.store.read(|db| {
        let new = war_line(db, war_id).unwrap_or_default();
        tell_war(ctx.world, db, war_id, declarer, against, change(WAR_HEADER, None, Some(&new)));
        refresh(db);
    });
    Ok(Value::None)
}

/// Ends a war a day from `now`, as retracting it does.
fn end(tx: &mut Tx<'_>, war_id: i64, now: i64) -> Result<(), StoreError> {
    match tx.table("crpWars").get_id(war_id).map(|r| r.id) {
        Some(row) => tx.update("crpWars", row, &[("retracted", store::Value::Int(now)), ("timeFinished", store::Value::Int(now + DAY))]),
        None => Ok(()),
    }
}

/// The war's unpaid bill, gone.
fn delete_unpaid_bill(tx: &mut Tx<'_>, war_id: i64) -> Result<(), StoreError> {
    let unpaid: Vec<store::RowId> = tx
        .table("billsReceivable")
        .rows()
        .filter(|b| b.int("billTypeID") == Some(BILL_TYPE_WAR.into()) && b.int("externalID") == Some(war_id) && b.int("paid") == Some(0))
        .map(|b| b.id)
        .collect();
    for row in unpaid {
        tx.delete("billsReceivable", row)?;
    }
    Ok(())
}

/// RetractWar(warID), by the declarer: the war stops a day later, and its
/// unpaid bill is forgiven.
pub async fn retract(ctx: &Ctx<'_>, declarer: i64, war_id: i64) -> Refused<Value> {
    let old = ctx.store.read(|db| war_line(db, war_id)).ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[1].as_i64() != Some(declarer) {
        return Err(message("NotYourWarToEnd"));
    }
    if old[5] != Value::None {
        return Ok(Value::None);
    }
    let now = crate::bulkdata::filetime_now();
    ctx.store.write(|tx| end(tx, war_id, now)).map_err(|e| db_failed("retracting the war", e))?;
    forgive_bill(ctx.store, war_id);
    let mut new = old.clone();
    new[4] = Value::Long(now + DAY);
    new[5] = Value::Long(now);
    info!("{declarer} retracted war {war_id}");
    let against = old[2].as_i64().unwrap_or(0);
    ctx.store.read(|db| {
        tell_war(ctx.world, db, war_id, declarer, against, change(WAR_HEADER, Some(&old), Some(&new)));
        refresh(db);
    });
    Ok(Value::None)
}

/// A war surrendered in a CEO trade (svc/pvptrade.rs): it ends as a
/// retracted one does, a day later, and its unpaid bill is forgiven.
pub fn prepare_surrender(tx: &mut Tx<'_>, war_id: i64) -> Refused<(i64,i64,Value)> {
    let old = war_line(tx.db(), war_id).ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[5] != Value::None {
        return Err(message("CanNotSurrenderAsWarIsOver"));
    }
    let now = crate::bulkdata::filetime_now();
    end(tx, war_id, now).map_err(|e| db_failed("ending the war", e))?;
    delete_unpaid_bill(tx, war_id).map_err(|e|db_failed("forgiving the war bill",e))?;
    let mut new = old.clone();
    new[4] = Value::Long(now + DAY);
    new[5] = Value::Long(now);
    let (declarer, against) = (old[1].as_i64().unwrap_or(0), old[2].as_i64().unwrap_or(0));
    Ok((declarer,against,change(WAR_HEADER, Some(&old), Some(&new))))
}

pub fn announce_surrender(world:&World,db:&Db,war_id:i64,event:(i64,i64,Value)) {
    let (declarer,against,changes)=event;
    info!("war {war_id} of {declarer} on {against} ends in surrender");
    tell_war(world, db, war_id, declarer, against, changes);
    refresh(db);
}

/// The two sides of a war that has not ended: (declarer, against).
pub fn sides(db: &Db, war_id: i64) -> Option<(i64, i64)> {
    let w = war_line(db, war_id)?;
    if w[5] != Value::None {
        return None;
    }
    Some((w[1].as_i64()?, w[2].as_i64()?))
}

/// ChangeMutualWarFlag(warID, mutual), by the defender. A mutual war is not
/// billed.
pub async fn set_mutual(ctx: &Ctx<'_>, defender: i64, war_id: i64, mutual: bool) -> Refused<Value> {
    let old = ctx.store.read(|db| war_line(db, war_id)).ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[2].as_i64() != Some(defender) {
        return Err(message("NotYourWarToEnd"));
    }
    ctx.store
        .write(|tx| match tx.table("crpWars").get_id(war_id).map(|r| r.id) {
            Some(row) => tx.update("crpWars", row, &[("mutual", store::Value::Int(mutual.into()))]),
            None => Ok(()),
        })
        .map_err(|e| db_failed("changing the war", e))?;
    if mutual {
        forgive_bill(ctx.store, war_id);
    }
    let mut new = old.clone();
    new[6] = Value::Int(mutual.into());
    let declarer = old[1].as_i64().unwrap_or(0);
    ctx.store.read(|db| tell_war(ctx.world, db, war_id, declarer, defender, change(WAR_HEADER, Some(&old), Some(&new))));
    Ok(Value::None)
}

/// A corporation joining an alliance hands it its wars, on both sides.
pub fn adopt(tx: &mut Tx<'_>, corp_id: i64, alliance_id: i64) -> Refused<()> {
    let now = crate::bulkdata::filetime_now();
    for column in ["declaredByID", "againstID"] {
        let held: Vec<store::RowId> = tx
            .table("crpWars")
            .find(&[column], &[store::Value::Int(corp_id)])
            .filter(|w| w.int("retracted").is_none_or(|r| r > now - DAY))
            .map(|w| w.id)
            .collect();
        for row in held {
            tx.update("crpWars", row, &[(column, store::Value::Int(alliance_id))])
                .map_err(|e| db_failed("handing the wars to the alliance", e))?;
        }
    }
    Ok(())
}

fn war_line(db: &Db, war_id: i64) -> Option<Vec<Value>> {
    db.table("crpWars").get_id(war_id).map(|r| r.line(WAR_HEADER))
}

fn forgive_bill(store: &Store, war_id: i64) {
    if let Err(e) = store.write(|tx| delete_unpaid_bill(tx, war_id)) {
        error!("forgiving the bill of war {war_id}: {}", crate::logging::chain(&e));
    }
}

/// OnWarChanged(warID, declaredByID, againstID, change), to everyone on
/// either side.
fn tell_war(world: &World, db: &Db, war_id: i64, declarer: i64, against: i64, change: Value) {
    let mut corps = corps_of(db, declarer);
    corps.extend(corps_of(db, against));
    let w = world.lock();
    let clients: Vec<i64> = corps.iter().flat_map(|c| w.corp_members(*c)).collect();
    let event = Value::Tuple(vec![Value::Int(war_id), Value::Int(declarer), Value::Int(against), change]);
    w.notify(&clients, "OnWarChanged", event);
}

/// The corporations an owner stands for: an alliance's members, or itself.
pub fn corps_of(db: &Db, owner: i64) -> Vec<i64> {
    let members: Vec<i64> =
        db.table("allianceMembers").find(&["allianceID"], &[store::Value::Int(owner)]).filter_map(|r| r.int("corporationID")).collect();
    if members.is_empty() { vec![owner] } else { members }
}

// ---- weekly bills ----------------------------------------------------------------------

/// Bills every non-mutual war in force for the week starting now, and ends
/// any whose last bill went unpaid. Run by the heartbeat.
pub async fn bill_wars(world: &World, store: &Store) {
    let now = crate::bulkdata::filetime_now();
    let wars: Vec<Vec<Value>> = store.read(|db| {
        db.table("crpWars")
            .rows()
            .filter(|w| w.value("retracted").is_null() && w.int("mutual") == Some(0))
            .map(|w| w.line(WAR_HEADER))
            .collect()
    });
    for war in wars {
        let (war_id, declarer, against) = (war[0].as_i64().unwrap_or(0), war[1].as_i64().unwrap_or(0), war[2].as_i64().unwrap_or(0));
        let declared = war[3].as_i64().unwrap_or(now);
        match store.read(|db| next_period(db, BILL_TYPE_WAR, war_id, declared + WEEK, now)) {
            Period::Overdue => {
                if let Err(e) = store.write(|tx| end(tx, war_id, now)) {
                    error!("ending war {war_id}: {}", crate::logging::chain(&e));
                    continue;
                }
                info!("war {war_id} of {declarer} on {against} ends: its bill went unpaid");
                let mut new = war.clone();
                new[4] = Value::Long(now + DAY);
                new[5] = Value::Long(now);
                store.read(|db| tell_war(world, db, war_id, declarer, against, change(WAR_HEADER, Some(&war), Some(&new))));
            }
            Period::Due { until } => {
                let amount = store.read(|db| cost_against(db, against));
                let billed = store.write(|tx| {
                    let bill_id = issue_bill(tx, BILL_TYPE_WAR, declarer, amount, until, war_id);
                    if let (Some(bill_id), Some(row)) = (bill_id, tx.table("crpWars").get_id(war_id).map(|r| r.id)) {
                        let _ = tx.update("crpWars", row, &[("billID", store::Value::Int(bill_id))]);
                    }
                    Ok::<_, StoreError>(bill_id)
                });
                if let Ok(Some(_)) = billed {
                    info!("war {war_id}: {declarer} billed {amount} ISK for the week");
                }
            }
            Period::Covered => {}
        }
    }
    store.read(refresh);
}

/// Where a recurring bill stands.
pub enum Period {
    /// Paid for (or not started) up to now.
    Covered,
    /// A new period starts: bill it, due at `until`.
    Due { until: i64 },
    /// The last bill's due date passed unpaid.
    Overdue,
}

/// Recurring bills of `bill_type` for `external_id`, each due at the end of
/// the period it pays for; the first period starts at `first`. A new bill is
/// issued only once the last one is paid and its period over.
pub fn next_period(db: &Db, bill_type: i32, external_id: i64, first: i64, now: i64) -> Period {
    let last = db
        .table("billsReceivable")
        .rows()
        .filter(|b| b.int("billTypeID") == Some(bill_type.into()) && b.int("externalID") == Some(external_id))
        .max_by_key(|b| b.int("billID"));
    let period = |start: i64| if now >= start { Period::Due { until: start + period_of(bill_type) } } else { Period::Covered };
    match last {
        None => period(first),
        Some(r) => {
            let due = r.int("dueDateTime").unwrap_or(0);
            let paid = r.int("paid").unwrap_or(0) != 0;
            match (paid, now > due) {
                (false, true) => Period::Overdue,
                (true, _) => period(due),
                (false, false) => Period::Covered,
            }
        }
    }
}

fn period_of(bill_type: i32) -> i64 {
    if bill_type == BILL_TYPE_WAR { WEEK } else { crate::svc::alliance::MAINTENANCE_PERIOD }
}

/// A bill to CONCORD; returns its ID.
pub fn issue_bill(tx: &mut Tx<'_>, bill_type: i32, debtor: i64, amount: f64, due: i64, external_id: i64) -> Option<i64> {
    let bill = tx.insert(
        "billsReceivable",
        &[
            ("billTypeID", store::Value::Int(bill_type.into())),
            ("debtorID", store::Value::Int(debtor)),
            ("creditorID", store::Value::Int(OWNER_CONCORD)),
            ("amount", store::Value::Float(amount)),
            ("dueDateTime", store::Value::Int(due)),
            ("externalID", store::Value::Int(external_id)),
        ],
    );
    match bill {
        Ok(row) => tx.table("billsReceivable").row(row).and_then(|r| r.int("billID")),
        Err(e) => {
            error!("billing {debtor}: {}", crate::logging::chain(&e));
            None
        }
    }
}

// ---- hostility, for CONCORD -----------------------------------------------------------

/// A war in force: (declarer, against, declared, retracted), FILETIME.
type Span = (i64, i64, i64, Option<i64>);

static IN_FORCE: RwLock<Vec<Span>> = RwLock::new(Vec::new());

/// Reloads the wars in force.
pub fn refresh(db: &Db) {
    let since = crate::bulkdata::filetime_now() - DAY;
    let spans = db
        .table("crpWars")
        .rows()
        .filter(|w| w.int("retracted").is_none_or(|r| r > since))
        .map(|w| (w.int("declaredByID").unwrap_or(0), w.int("againstID").unwrap_or(0), w.int("timeDeclared").unwrap_or(0), w.int("retracted")))
        .collect();
    if let Ok(mut w) = IN_FORCE.write() {
        *w = spans;
    }
}

/// Whether two pilots' sides are at war in a hostile state, where each side
/// is the pilot's alliance, or corporation outside one.
pub fn at_war(a: &Pilot, b: &Pilot) -> bool {
    sides_at_war(side(a), side(b))
}

/// A pilot's side in a war: its alliance, or its corporation outside one.
pub fn side(p: &Pilot) -> i64 {
    p.alliance_id.filter(|a| *a != 0).unwrap_or(p.corp_id)
}

/// Whether two sides (alliances, or corporations outside one) are at war in
/// a hostile state.
pub fn sides_at_war(x: i64, y: i64) -> bool {
    let now = crate::bulkdata::filetime_now();
    IN_FORCE.read().is_ok_and(|wars| wars.iter().any(|w| hostile(*w, x, y, now)))
}

/// The sides `x` is at war with, in a hostile state.
pub fn enemies_of(x: i64) -> Vec<i64> {
    let now = crate::bulkdata::filetime_now();
    let Ok(wars) = IN_FORCE.read() else { return Vec::new() };
    let mut enemies: Vec<i64> = wars
        .iter()
        .filter_map(|w| {
            let y = if w.0 == x { w.1 } else if w.1 == x { w.0 } else { return None };
            hostile(*w, x, y, now).then_some(y)
        })
        .collect();
    enemies.sort_unstable();
    enemies.dedup();
    enemies
}

fn hostile((by, against, declared, retracted): Span, x: i64, y: i64, now: i64) -> bool {
    let pair = (by == x && against == y) || (by == y && against == x);
    pair && now - declared >= DAY && retracted.is_none_or(|r| now - r < DAY)
}
