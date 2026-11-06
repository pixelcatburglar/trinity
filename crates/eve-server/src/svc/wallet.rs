use eve_marshal::Value;
#[path="wallet_accounts.rs"]
mod accounts;
pub use accounts::{can_take,post_account,select_account};
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_ACCOUNTANT;
use crate::store::{self, Db, Row, Store, StoreError, Tx};
use crate::svc::rows::rowset;

const JOURNAL_BATCH: i64 = 25;

const REF_PLAYER_DONATION: i32 = 10;
const REF_CORPORATION_ACCOUNT_WITHDRAWAL: i32 = 37;
pub const REF_BOUNTY: i32 = 16;

const ACCOUNT_KEY_PERSONAL: i32 = 1000;

const REASON_MAX: usize = 40;

pub async fn account(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GiveCash" if call.kwarg("toAccountKey").is_some() => return Some(accounts::transfer_accounts(ctx,call,false).await),
        "GiveCash" => return Some(give_cash(call, ctx, false).await),
        "GiveCashFromCorpAccount" => return Some(accounts::transfer_accounts(ctx,call,true).await),
        "GetCashBalance" => {
            let corp = call.arg_id(0) != 0;
            let key=call.kwarg("accountKey").and_then(Value::as_i64).or_else(||call.arg_i64(1))
                .unwrap_or(ctx.session.corp_account_key.unwrap_or(1000) as i64) as i32;
            if corp && (!(1000..=1006).contains(&key) ||
                (ctx.session.corp_role() & (1|256|1024)==0 && !can_take(ctx.session.corp_role(),key))) {
                return Some(Outcome::custom("You do not have access to that corporation wallet."));
            }
            if corp && key!=1000 {
                let corp_id=ctx.session.corp_id.unwrap_or(0);
                let balance=ctx.store.read(|db|db.table("corporation_wallets").get(&[store::Value::Int(corp_id),store::Value::Int(i64::from(key))]).and_then(|r|r.float("balance"))).unwrap_or(0.0);
                Value::Double(balance)
            } else {cash_balance(ctx, corp).await}
        }
        "GetKeyMap" => key_map(),
        "GetRefTypes" => ref_types(),
        "GetJournal" => {
            if call.arg_id(3)!=0 && ctx.session.corp_role() & (1|256|1024)==0 {
                return Some(Outcome::custom("Corporation journals require an accounting role."));
            }
            journal(call, ctx).await
        },
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn bill_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetBillTypes" => bill_types(),
        "CharGetBills" => bills(ctx, ctx.session.char_id, Side::Payable).await,
        "CharGetBillsReceivable" => bills(ctx, ctx.session.char_id, Side::Receivable).await,
        "GetCorporationBills" => bills(ctx, ctx.session.corp_id, Side::Payable).await,
        "GetCorporationBillsReceivable" => bills(ctx, ctx.session.corp_id, Side::Receivable).await,
        "CharPayBill" => return Some(pay_bill(ctx, call.arg_id(0), false).await),
        "PayCorporationBill" => return Some(pay_bill(ctx, call.arg_id(0), true).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const BILL_TYPE_RENTAL: i64 = 2;
const BILL_TYPE_WAR: i64 = 4;
const BILL_TYPE_ALLIANCE_MAINTENANCE: i64 = 5;
/// market_refTypes: War Fee, Alliance Maintainance Fee.
const REF_WAR_FEE: i32 = 49;
const REF_ALLIANCE_MAINTENANCE_FEE: i32 = 50;
pub const REF_OFFICE_RENTAL_FEE: i32 = 13;
const REF_CORPORATION_PAYMENT: i32 = 11;

/// Pays one of the caller's (or, with the Accountant role, its corporation's)
/// open bills to the creditor (wallet.py PayBill).
async fn pay_bill(ctx: &Ctx<'_>, bill_id: i64, corp: bool) -> Outcome {
    let debtor = if corp {
        if !ctx.session.has_corp_role(CORP_ROLE_ACCOUNTANT) {
            return Outcome::UserError {
                msg: "CrpAccessDenied".into(),
                args: vec![("reason".into(), Value::bytes("you do not hold the Accountant role"))],
            };
        }
        ctx.session.corp_id
    } else {
        ctx.session.char_id
    };
    let Some(debtor) = debtor else { return Outcome::Value(Value::None) };
    settle_bill(ctx, bill_id, debtor, debtor).await
}

/// Pays `debtor`'s open bill `bill_id` to its creditor out of `payer`'s
/// wallet (the debtor's own, or an alliance's executor corporation's).
pub async fn settle_bill(ctx: &Ctx<'_>, bill_id: i64, debtor: i64, payer: i64) -> Outcome {
    let paid = ctx.store.write(|tx| -> Result<_, Refused> {
        let bill=tx.table("billsReceivable").get_id(bill_id)
            .filter(|b|b.int("debtorID")==Some(debtor) && b.int("paid")==Some(0))
            .map(|b|(b.id,b.int("creditorID"),b.float("amount"),b.int("billTypeID")));
        let Some((row,creditor,amount,bill_type))=bill else {return Ok(None)};
        let creditor=creditor.unwrap_or(0); let amount=amount.unwrap_or(f64::NAN);
        if !amount.is_finite() || amount<0.0 {return Err(Refused(Outcome::custom("This bill has an invalid amount.")))}
        let ref_type=match bill_type {Some(BILL_TYPE_RENTAL)=>REF_OFFICE_RENTAL_FEE,Some(BILL_TYPE_WAR)=>REF_WAR_FEE,Some(BILL_TYPE_ALLIANCE_MAINTENANCE)=>REF_ALLIANCE_MAINTENANCE_FEE,_=>REF_CORPORATION_PAYMENT};
        let bill_ref=bill_id.to_string();
        let entry=Entry {ref_type,payer,payee:creditor,arg_id1:&bill_ref,reason:""};
        let mut sides=vec![(payer,-amount),(creditor,amount)]; sides.sort_unstable_by_key(|(owner,_)|*owner);
        for (owner,delta) in sides {post(tx,owner,delta,&entry).map_err(|t|Refused(crate::svc::membership::transfer_refused(t)))?;}
        tx.update("billsReceivable",row,&[("paid",store::Value::Int(1))]).map_err(|e|Refused(crate::svc::membership::db_failed("marking the bill paid",e)))?;
        Ok(Some((creditor,amount)))
    });
    match paid {
        Ok(Some((creditor,amount))) => {
            info!("bill {bill_id}: {payer} paid {amount} ISK of {debtor}'s to {creditor}");
            notify_balance(ctx,payer).await; if creditor!=payer {notify_balance(ctx,creditor).await;}
            Outcome::Value(Value::None)
        }
        Ok(None) => Outcome::Value(Value::None),
        Err(Refused(refusal)) => refusal,
    }
}

/// Why a bill payment was refused (its changes undone).
struct Refused(Outcome);

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Self {
        Refused(crate::svc::membership::db_failed("paying the bill", e))
    }
}

/// The wallet-transaction part of the `marketProxy` service.
pub async fn market_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "CharGetNewTransactions" | "CharGetTransactions" | "CorpGetNewTransactions" | "CorpGetTransactions" =>
            Some(transactions(ctx, call).await),
        _ => None,
    }
}

async fn cash_balance(ctx: &Ctx<'_>, corp: bool) -> Value {
    let (table, owner) = if corp { ("corporation", ctx.session.corp_id) } else { ("character_", ctx.session.char_id) };
    let Some(owner) = owner else { return Value::Double(0.0) };
    Value::Double(ctx.store.read(|db| db.table(table).get_id(owner).and_then(|r| r.float("balance"))).unwrap_or(0.0))
}

fn key_map() -> Value {
    static_rowset("market_keyMap", &["accountKey", "accountType", "accountName", "description"])
}

fn ref_types() -> Value {
    static_rowset("market_refTypes", &["refTypeID", "refTypeText", "description"])
}

async fn journal(call: &Call, ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "refID", "transDate", "refTypeID", "ownerID1", "ownerID2", "argID1", "amount", "balance",
        "reason", "accountKey",
    ];

    let account_key = call.arg_i64(0).unwrap_or(1000);
    let from_date = call.arg_i64(1);
    let ref_type_id = call.arg_i64(2);
    let corp = call.arg_id(3) != 0;
    let from_ref_id = call.arg_i64(4);
    let reverse = call.arg_id(5) != 0;

    let owner = if corp { ctx.session.corp_id } else { ctx.session.char_id };
    let Some(owner) = owner else { return rowset(HEADER, Vec::new()) };

    let mut lines: Vec<Value> = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db
            .table("market_journal")
            .find(&["characterID", "accountKey"], &[store::Value::Int(owner), store::Value::Int(account_key)])
            .filter(|r| from_date.is_none_or(|d| r.int("transDate").is_some_and(|t| t < d)))
            .filter(|r| ref_type_id.is_none_or(|t| r.int("refTypeID") == Some(t)))
            .filter(|r| from_ref_id.is_none_or(|id| r.int("refID").is_some_and(|r| r < id)))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.int("refID")));
        rows.iter()
            .take(JOURNAL_BATCH as usize)
            .map(|r| {
                Value::List(vec![
                    r.cell("refID"),
                    r.cell("transDate"),
                    r.cell("refTypeID"),
                    r.cell("ownerID1"),
                    r.cell("ownerID2"),
                    text_id(r.text("argID1")),
                    Value::Double(r.float("amount").unwrap_or(0.0)),
                    Value::Double(r.float("balance").unwrap_or(0.0)),
                    r.cell("reason"),
                    r.cell("accountKey"),
                ])
            })
            .collect()
    });
    if !reverse {
        lines.reverse();
    }
    rowset(HEADER, lines)
}

fn bill_types() -> Value {
    static_rowset("billTypes", &["billTypeID", "billTypeName", "description"])
}

/// A static table's columns as a rowset, by its first (integer key) column.
fn static_rowset(table: &str, header: &[&str]) -> Value {
    let mut rows: Vec<_> = crate::sde::table(table).rows().collect();
    rows.sort_by_key(|r| r.int(header[0]));
    rowset(header, rows.iter().map(|r| Value::List(r.line(header))).collect())
}

#[derive(Clone, Copy)]
pub enum Side {
    Payable,
    Receivable,
}

/// The open bills `owner` owes (Payable) or is owed (Receivable).
pub async fn bills(ctx: &Ctx<'_>, owner: Option<i64>, side: Side) -> Value {
    const HEADER: &[&str] = &[
        "billID", "billTypeID", "debtorID", "creditorID", "amount", "dueDateTime", "interest",
        "externalID", "paid", "externalID2",
    ];
    let Some(owner) = owner else { return rowset(HEADER, Vec::new()) };

    let (by, other) = match side {
        Side::Payable => ("debtorID", "creditorID"),
        Side::Receivable => ("creditorID", "debtorID"),
    };
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db
            .table("billsReceivable")
            .find(&[by], &[store::Value::Int(owner)])
            .filter(|r| !r.value(other).is_null() && !r.value("billTypeID").is_null())
            .collect();
        rows.sort_by_key(|r| r.int("billID"));
        rows.iter()
            .filter(|r| r.int("paid").unwrap_or(0) == 0)
            .map(|r| {
                Value::List(vec![
                    r.cell("billID"),
                    r.cell("billTypeID"),
                    r.cell("debtorID"),
                    r.cell("creditorID"),
                    Value::Double(r.float("amount").unwrap_or(0.0)),
                    r.int("dueDateTime").map_or(Value::None, Value::Long),
                    Value::Double(r.float("interest").unwrap_or(0.0)),
                    Value::Int(r.int("externalID").unwrap_or(-1)),
                    Value::Int(0),
                    Value::Int(r.int("externalID2").unwrap_or(-1)),
                ])
            })
            .collect()
    });
    rowset(HEADER, lines)
}

async fn transactions(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    const HEADER: &[&str] = &[
        "transactionID", "transactionDateTime", "typeID", "quantity", "price", "transactionType",
        "clientID", "regionID", "stationID", "characterID", "corpTransaction", "accountKey",
    ];
    let corp = call.method.starts_with("Corp");
    let Some(me) = ctx.session.char_id else { return Outcome::custom("Select a character first.") };
    if corp && ctx.session.corp_role() & (crate::session::CORP_ROLE_DIRECTOR | CORP_ROLE_ACCOUNTANT | 1024) == 0 {
        return Outcome::custom("Corporation transactions require the Accountant or Junior Accountant role.");
    }
    let owner = if corp { ctx.session.corp_id.unwrap_or(0) } else { me };
    let offset = usize::from(!call.method.contains("New"));
    // A filter outside the range of the integer column it compares with is
    // no filter.
    let int = |i| call.arg_i64(i + offset).filter(|v| i32::try_from(*v).is_ok());
    let number = |i| call.args.get(i + offset).and_then(Value::as_f64).filter(|v| v.is_finite());
    let before = if offset == 1 { call.arg_i64(0).filter(|v| i32::try_from(*v).is_ok()) } else { None };
    let kind = int(0); let type_id = int(1); let client_id = int(2); let quantity = int(3);
    let from = call.arg_i64(4 + offset); let maximum = number(5); let minimum = number(6);
    let account = if corp { int(7) } else { None }; let actor = if corp { int(8) } else { None };
    let lines = ctx.store.read(|db| {
        let table = db.table("market_transactions");
        let mine: Box<dyn Iterator<Item = Row>> = if corp {
            Box::new(table.find(&["corporationID"], &[store::Value::Int(owner)]).filter(|r| r.int("isCorp") == Some(1)))
        } else {
            table.find(&["characterID"], &[store::Value::Int(owner)])
        };
        let is = |r: &Row, col: &str, want: Option<i64>| want.is_none_or(|w| r.int(col) == Some(w));
        let mut rows: Vec<Row> = mine
            .filter(|r| before.is_none_or(|b| r.int("transactionID").is_some_and(|id| id < b)))
            .filter(|r| is(r, "transactionType", kind) && is(r, "typeID", type_id) && is(r, "clientID", client_id))
            .filter(|r| quantity.is_none_or(|q| r.int("quantity").is_some_and(|n| n >= q)))
            .filter(|r| from.is_none_or(|f| r.int("transactionDateTime").is_some_and(|t| t >= f)))
            .filter(|r| maximum.is_none_or(|m| r.float("price").is_some_and(|p| p <= m)))
            .filter(|r| minimum.is_none_or(|m| r.float("price").is_some_and(|p| p >= m)))
            .filter(|r| is(r, "accountKey", account) && is(r, "characterID", actor))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.int("transactionID")));
        rows.iter()
            .take(25)
            .map(|r| {
                Value::List(r.line(&[
                    "transactionID", "transactionDateTime", "typeID", "quantity", "price", "transactionType",
                    "clientID", "regionID", "stationID", "characterID", "isCorp", "accountKey",
                ]))
            })
            .collect()
    });
    Outcome::Value(rowset(HEADER, lines))
}

async fn give_cash(call: &Call, ctx: &Ctx<'_>, from_corp: bool) -> Outcome {
    let to_owner = call.arg_id(0);
    let amount = match call.args.get(1) {
        Some(Value::Double(d)) => *d,
        Some(v) => v.as_i64().map(|i| i as f64).unwrap_or(0.0),
        None => 0.0,
    };
    let reason: String = call
        .args
        .get(2)
        .and_then(|v| v.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
        .chars()
        .take(REASON_MAX)
        .collect();

    let Some(char_id) = ctx.session.char_id else {
        return Outcome::Unknown("account::GiveCash without a character".into());
    };
    if !amount.is_finite() || amount <= 0.0 {
        return Outcome::Unknown(format!("account::GiveCash with amount {amount}"));
    }
    let payer = if from_corp {
        let Some(corp) = ctx.session.corp_id else {
            return Outcome::Unknown("account::GiveCashFromCorpAccount without a corporation".into());
        };
        if !ctx.session.has_corp_role(CORP_ROLE_ACCOUNTANT) {
            return Outcome::UserError {
                msg: "CrpAccessDenied".into(),
                args: vec![(
                    "reason".into(),
                    Value::bytes("you do not hold the Accountant role"),
                )],
            };
        }
        corp
    } else {
        char_id
    };

    let (ref_type, arg_id1) = if from_corp {
        (REF_CORPORATION_ACCOUNT_WITHDRAWAL, char_id.to_string())
    } else {
        (REF_PLAYER_DONATION, String::new())
    };
    let payment = Payment { payer, payee: Some(to_owner), amount, ref_type, arg_id1: &arg_id1, reason: &reason };
    match move_cash(ctx.store, payment).await {
        Ok(()) => {
            info!(
                "GiveCash{}: {payer} -> {to_owner}, {amount} ISK ({reason:?})",
                if from_corp { "FromCorpAccount" } else { "" }
            );
            notify_balance(ctx,payer).await;
            if to_owner!=payer {notify_balance(ctx,to_owner).await;}
            Outcome::Value(Value::None)
        }
        Err(Transfer::NotEnough { need, have }) => Outcome::not_enough_money(need, have),
        Err(Transfer::NoSuchOwner) => {
            Outcome::Unknown(format!("account::GiveCash to unknown owner {to_owner}"))
        }
        Err(Transfer::Db(e)) => {
            error!("GiveCash {payer} -> {to_owner}: {err}", err = crate::logging::chain(&e));
            Outcome::Unknown("account::GiveCash failed".into())
        }
    }
}

pub enum Transfer {
    NotEnough { need: f64, have: f64 },
    NoSuchOwner,
    Db(StoreError),
}

impl From<StoreError> for Transfer {
    fn from(e: StoreError) -> Self {
        Transfer::Db(e)
    }
}

pub(crate) async fn notify_balance(ctx:&Ctx<'_>,owner:i64) {
    let value=ctx.store.read(|db|balance(db,"character_",owner).or_else(||balance(db,"corporation",owner)));
    if let Some(balance)=value {let w=ctx.world.lock();let mut clients=w.clients_of(owner);clients.extend(w.corp_members(owner));w.notify(&clients,"OnAccountChange",Value::Tuple(vec![Value::bytes("cash"),Value::Int(owner),Value::Double(balance)]));}
}

/// One movement of ISK, journalled for both sides. `payee` None means the money
/// leaves the economy (a bounty, a fee).
pub struct Payment<'a> {
    pub payer: i64,
    pub payee: Option<i64>,
    pub amount: f64,
    pub ref_type: i32,
    pub arg_id1: &'a str,
    pub reason: &'a str,
}

pub async fn move_cash(store: &Store, p: Payment<'_>) -> Result<(), Transfer> {
    store.write(|tx| {
        let entry = Entry { ref_type: p.ref_type, payer: p.payer, payee: p.payee.unwrap_or(0), arg_id1: p.arg_id1, reason: p.reason };
        // Both sides post in ID order, so their journal lines do too.
        let mut sides: Vec<(i64, f64)> = std::iter::once((p.payer, -p.amount)).chain(p.payee.map(|id| (id, p.amount))).collect();
        sides.sort_unstable_by_key(|(id, _)| *id);
        for (owner, delta) in sides {
            post(tx, owner, delta, &entry)?;
        }
        Ok(())
    })
}

/// What a journal line records about the payment it belongs to.
pub struct Entry<'a> {
    pub ref_type: i32,
    pub payer: i64,
    /// 0 when the money leaves the economy or comes from nowhere.
    pub payee: i64,
    pub arg_id1: &'a str,
    pub reason: &'a str,
}

/// One side of a payment, inside the caller's transaction: add `delta` to
/// `owner`'s wallet (a character or a corporation) and journal it. Refuses to
/// go below zero. Returns the new balance.
pub fn post(tx: &mut Tx<'_>, owner: i64, delta: f64, e: &Entry<'_>) -> Result<f64, Transfer> {
    let Some(have) = wallet_balance(tx.db(), owner) else { return Err(Transfer::NoSuchOwner) };
    if delta < 0.0 && have + delta < 0.0 {
        return Err(Transfer::NotEnough { need: -delta, have });
    }
    let balance = have + delta;
    add_balance(tx, owner, balance)?;
    tx.insert(
        "market_journal",
        &[
            ("characterID", store::Value::Int(owner)),
            ("transDate", store::Value::Int(crate::bulkdata::filetime_now())),
            ("refTypeID", store::Value::Int(i64::from(e.ref_type))),
            ("ownerID1", store::Value::Int(e.payer)),
            ("ownerID2", store::Value::Int(e.payee)),
            ("argID1", store::Value::Text(e.arg_id1.into())),
            ("accountID", store::Value::Int(0)),
            ("amount", store::Value::Float(delta)),
            ("balance", store::Value::Float(balance)),
            ("reason", store::Value::Text(e.reason.into())),
            ("accountKey", store::Value::Int(i64::from(ACCOUNT_KEY_PERSONAL))),
        ],
    )?;
    Ok(balance)
}

/// An owner's balance in `table` (`character_` or `corporation`).
fn balance(db: &Db, table: &str, owner: i64) -> Option<f64> {
    db.table(table).get_id(owner).and_then(|r| r.float("balance"))
}

/// A wallet's balance: a corporation's, else a character's.
fn wallet_balance(db: &Db, owner: i64) -> Option<f64> {
    if db.table("corporation").get_id(owner).is_some() {
        return balance(db, "corporation", owner);
    }
    balance(db, "character_", owner)
}

/// Set the wallet's new balance: a corporation's if there is one, else a
/// character's.
fn add_balance(tx: &mut Tx<'_>, owner: i64, balance: f64) -> Result<(), StoreError> {
    for table in ["corporation", "character_"] {
        if let Some(row) = tx.table(table).get_id(owner).map(|r| r.id) {
            return tx.update(table, row, &[("balance", store::Value::Float(balance))]);
        }
    }
    Ok(())
}

fn text_id(v: Option<&str>) -> Value {
    match v.and_then(parse_i64) {
        Some(i) => Value::Int(i),
        None => Value::None,
    }
}

fn parse_i64(s: &str) -> Option<i64> {
    let s = s.trim();
    match s.parse::<i64>() {
        Ok(v) => Some(v),
        Err(_) => s.parse::<f64>().ok().map(|v| v as i64),
    }
}
