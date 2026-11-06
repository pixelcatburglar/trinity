//! `insuranceSvc`, unbound and bound per station (base_insurance.py). Quotes are
//! computed by the client; the server keeps the contracts.

use eve_marshal::Value;

use crate::items::ItemTxn;
use crate::services::{Call, Ctx, Outcome};
use crate::session::{CORP_ROLE_ACCOUNTANT, CORP_ROLE_DIRECTOR};
use crate::store::{Db, StoreError, Value as V};
use crate::svc::rows::{row, rowset};
use crate::svc::wallet::{self, Entry, Transfer};

const CORP_ROLE_JUNIOR_ACCOUNTANT: i64 = 4_503_599_627_370_496;

const HEADER: &[&str] = &[
    "contractID",
    "shipID",
    "ownerID",
    "startDate",
    "endDate",
    "fraction",
    "amount",
    "payout",
    "isCorp",
];

pub async fn insurance_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // GetContracts(isCorp=0): the caller's contracts, or its corporation's.
        "GetContracts" => {
            let corp = call.arg_i64(0).unwrap_or(0) != 0;
            if corp
                && ctx.session.corp_role()
                    & (CORP_ROLE_ACCOUNTANT | CORP_ROLE_JUNIOR_ACCOUNTANT | CORP_ROLE_DIRECTOR)
                    == 0
            {
                return Some(insure_failed(
                    "You do not have access to corporation insurance.",
                ));
            }
            let owner = if corp {
                ctx.session.corp_id
            } else {
                ctx.session.char_id
            };
            let Some(owner) = owner else {
                return Some(Outcome::Value(rowset(HEADER, vec![])));
            };
            let now = crate::bulkdata::filetime_now();
            let lines = ctx.store.read(|db| {
                let mut current: Vec<_> = db
                    .table("insuranceContracts")
                    .find(&["ownerID"], &[V::Int(owner)])
                    .filter(|r| r.int("endDate").is_some_and(|end| end > now))
                    .collect();
                current.sort_by_key(|r| r.int("contractID"));
                current.iter().map(|r| Value::List(r.line(HEADER))).collect()
            });
            rowset(HEADER, lines)
        }
        // infosvc reads contract.ownerID/.fraction/.startDate/.endDate, or None.
        "GetContractForShip" => {
            let now = crate::bulkdata::filetime_now();
            let line = ctx.store.read(|db| {
                db.table("insuranceContracts")
                    .find(&["shipID"], &[V::Int(call.arg_id(0))])
                    .find(|r| r.int("endDate").is_some_and(|end| end > now))
                    .map(|r| r.line(HEADER))
            });
            match line {
                Some(line) => row(HEADER, line),
                None => Value::None,
            }
        }
        // InsureShip(shipID, amount, isCorp): the quote picked (base_insurance.py
        // GetQuoteForShip: 5% of the hull's base price buys 50% cover, each
        // further 5% another 10%).
        "InsureShip" => {
            let is_corp = call.arg_i64(2).unwrap_or(0) != 0;
            let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let void_old = call
                .kwargs
                .iter()
                .find(|(k, _)| k.as_bytes() == Some(b"voidOld"))
                .and_then(|(_, v)| v.as_i64())
                .or_else(|| call.arg_i64(3))
                .unwrap_or(0)
                != 0;
            return Some(insure(ctx, call.arg_id(0), amount, is_corp, void_old).await);
        }
        "UnInsureShip" => {
            return Some(
                uninsure(ctx, call.arg_id(0))
                    .await
                    .map_or_else(|e| e, |_| Outcome::Value(Value::None)),
            );
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const REF_INSURANCE: i32 = 19;
const CONTRACT_WEEKS: i64 = 12;

/// Match the client's repeated addition of five percent, allowing only floating
/// point rounding error. Always charge the canonical price for that level.
fn quote(amount: f64, base_price: f64) -> Option<(f64, f64)> {
    let step = base_price * 0.05;
    if !amount.is_finite() || !base_price.is_finite() || amount <= 0.0 || step <= 0.0 {
        return None;
    }
    (1..=6).find_map(|n| {
        let premium = step * n as f64;
        ((amount - premium).abs() <= premium.abs() * f64::EPSILON * 8.0)
            .then_some((premium, (4 + n) as f64 / 10.0))
    })
}

async fn insure(
    ctx: &Ctx<'_>,
    ship_id: i64,
    amount: f64,
    is_corp: bool,
    void_old: bool,
) -> Outcome {
    match purchase(ctx, ship_id, amount, is_corp, void_old).await {
        Ok(owner) => {
            wallet::notify_balance(ctx, owner).await;
            Outcome::Value(Value::None)
        }
        Err(error) => error,
    }
}

/// Why a purchase or a cancellation stopped, as the client is told it; a
/// store error is logged and told as the insurance not being updated.
struct Refusal(Outcome);

impl From<StoreError> for Refusal {
    fn from(error: StoreError) -> Self {
        Refusal(db_failed(error))
    }
}

fn refused(reason: &str) -> Refusal {
    Refusal(insure_failed(reason))
}

async fn purchase(
    ctx: &Ctx<'_>,
    ship_id: i64,
    amount: f64,
    is_corp: bool,
    void_old: bool,
) -> Result<i64, Outcome> {
    let (Some(char_id), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(insure_failed("Ships can only be insured in a station."));
    };
    if ship_id <= 0 || ship_id > i32::MAX as i64 {
        return Err(insure_failed("That is not a ship."));
    }
    if is_corp && !may_manage_corp(ctx) {
        return Err(insure_failed(
            "Only a corporation Accountant may insure its ships.",
        ));
    }
    let owner = if is_corp {
        ctx.session.corp_id.unwrap_or(0)
    } else {
        char_id
    };
    let (payout, fraction, premium) = ctx
        .store
        .write(|tx| {
            let mut tx = ItemTxn::new(tx);
            let ship = tx
                .item(ship_id)?
                .ok_or_else(|| refused("That ship no longer exists."))?;
            if ship.category_id != 6 || !ship.singleton {
                return Err(refused("That is not an assembled ship."));
            }
            if ship.owner_id != owner
                || !at_station(tx.db(), ship.location_id, ship.flag, owner, station, is_corp)
            {
                return Err(refused(
                    "The ship must be yours and in this station's hangar or corporation office.",
                ));
            }
            // The ship is of category 6, so its type is known.
            let base_price = crate::sde::table("invTypes")
                .get("typeID", ship.type_id)
                .and_then(|t| t.float("basePrice"))
                .unwrap_or(0.0);
            let (premium, fraction) = quote(amount, base_price)
                .ok_or_else(|| refused("That is not a premium on offer."))?;
            let now = crate::bulkdata::filetime_now();
            let old = tx
                .db()
                .table("insuranceContracts")
                .find_one(&["shipID"], &[V::Int(ship_id)])
                .map(|r| (r.id, r.int("ownerID").unwrap_or(0), r.int("endDate").unwrap_or(0)));
            if let Some((old, previous, end)) = old {
                if end > now {
                    if previous == owner {
                        return Err(Refusal(Outcome::UserError {
                            msg: "InsureShipFailedMultipleContracts".into(),
                            args: vec![],
                        }));
                    }
                    if !void_old {
                        return Err(Refusal(Outcome::UserError {
                            msg: "InsureShipFailedSingleContract".into(),
                            args: vec![(
                                "ownerName".into(),
                                Value::Tuple(vec![Value::Int(2), Value::Int(previous)]),
                            )],
                        }));
                    }
                }
                tx.tx().delete("insuranceContracts", old)?;
            }
            let arg = ship_id.to_string();
            let entry = Entry {
                payer: owner,
                payee: 0,
                ref_type: REF_INSURANCE,
                arg_id1: &arg,
                reason: "",
            };
            wallet::post(tx.tx(), owner, -premium, &entry).map_err(|e| {
                Refusal(match e {
                    Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
                    Transfer::NoSuchOwner => insure_failed("There is no wallet to pay from."),
                    Transfer::Db(e) => db_failed(e),
                })
            })?;
            let end = now + CONTRACT_WEEKS * 7 * 24 * 3_600 * 10_000_000;
            let payout = fraction * base_price;
            let contract = tx.tx().nextval("dyn_id_seq")?;
            tx.tx().insert(
                "insuranceContracts",
                &[
                    ("contractID", V::Int(contract)),
                    ("shipID", V::Int(ship_id)),
                    ("ownerID", V::Int(owner)),
                    ("startDate", V::Int(now)),
                    ("endDate", V::Int(end)),
                    ("fraction", V::Float(fraction)),
                    ("amount", V::Float(premium)),
                    ("payout", V::Float(payout)),
                    ("isCorp", V::Int(i64::from(is_corp))),
                ],
            )?;
            Ok((payout, fraction, premium))
        })
        .map_err(|r: Refusal| r.0)?;
    tracing::info!(
        "ship {ship_id} insured for {payout} ISK ({:.0}%) for {premium} ISK",
        fraction * 100.0
    );
    Ok(owner)
}

fn may_manage_corp(ctx: &Ctx<'_>) -> bool {
    ctx.session.corp_id.is_some()
        && ctx.session.corp_role() & (CORP_ROLE_ACCOUNTANT | CORP_ROLE_DIRECTOR) != 0
}

fn at_station(
    db: &Db,
    location: i64,
    flag: i64,
    owner: i64,
    station: i64,
    corp: bool,
) -> bool {
    if location == station {
        return if corp { flag == 62 } else { flag == 4 };
    }
    if !corp || super::corphangar::division(flag).is_none() {
        return false;
    }
    db.table("crpOffices")
        .find(&["itemID"], &[V::Int(location)])
        .any(|r| r.int("corporationID") == Some(owner) && r.int("stationID") == Some(station))
}

async fn uninsure(ctx: &Ctx<'_>, ship_id: i64) -> Result<(), Outcome> {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(insure_failed(
            "Insurance can only be cancelled in a station.",
        ));
    };
    if ship_id <= 0 || ship_id > i32::MAX as i64 {
        return Err(insure_failed("That is not a ship."));
    }
    ctx.store
        .write(|tx| {
            let mut tx = ItemTxn::new(tx);
            let ship = tx
                .item(ship_id)?
                .ok_or_else(|| refused("That ship no longer exists."))?;
            let Some((policy, owner, corp)) = tx
                .db()
                .table("insuranceContracts")
                .find_one(&["shipID"], &[V::Int(ship_id)])
                .map(|r| (r.id, r.int("ownerID").unwrap_or(0), r.int("isCorp").unwrap_or(0) != 0))
            else {
                return Ok(());
            };
            let permitted = if corp {
                ctx.session.corp_id == Some(owner) && may_manage_corp(ctx)
            } else {
                owner == me
            };
            if !permitted
                || ship.owner_id != owner
                || !at_station(tx.db(), ship.location_id, ship.flag, owner, station, corp)
            {
                return Err(refused("You cannot cancel insurance for that ship."));
            }
            tx.tx().delete("insuranceContracts", policy)?;
            Ok(())
        })
        .map_err(|r: Refusal| r.0)
}

fn db_failed(error: StoreError) -> Outcome {
    tracing::error!("insurance: {}", crate::logging::chain(&error));
    insure_failed("The insurance could not be updated.")
}

fn insure_failed(reason: &str) -> Outcome {
    Outcome::UserError {
        msg: "InsureShipFailed".into(),
        args: vec![("reason".into(), Value::text(reason))],
    }
}
