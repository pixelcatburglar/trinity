//! Corporation adverts, as consumed by Trinity's bco_recruitment.py.
use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::{rows, wallet};

const DAY: i64 = 864_000_000_000;
const HEADER: &[&str] = &[
    "adID",
    "corporationID",
    "allianceID",
    "stationID",
    "regionID",
    "raceMask",
    "typeMask",
    "skillPoints",
    "description",
    "createDateTime",
    "expiryDateTime",
];

/// An advert as the client lists it, with its corporation's alliance and its
/// station's region; None when either is missing (an advert is listed only
/// with both).
fn ad_line(db: &Db, ad: &Row) -> Option<Vec<Value>> {
    let corp = db.table("corporation").get_id(ad.int("corporationID")?)?;
    let station = db.table("staStations").get_id(ad.int("stationID")?)?;
    Some(vec![
        ad.cell("adID"),
        ad.cell("corporationID"),
        corp.cell("allianceID"),
        ad.cell("stationID"),
        station.cell("regionID"),
        ad.cell("raceMask"),
        ad.cell("typeMask"),
        ad.cell("skillPoints"),
        ad.cell("description"),
        ad.cell("createDateTime"),
        ad.cell("expiryDateTime"),
    ])
}

/// The advert `id` if it is the corporation's.
fn own_ad(db: &Db, corp: i64, id: i64) -> Option<Row<'_>> {
    db.table("crpRecruitmentAds").get_id(id).filter(|a| a.int("corporationID") == Some(corp))
}

/// A corporation's adverts, in the order they were placed (adID).
fn corporation_ads(db: &Db, corp: i64) -> Value {
    let mut ads: Vec<Row> = db.table("crpRecruitmentAds").find(&["corporationID"], &[store::Value::Int(corp)]).collect();
    ads.sort_by_key(|a| a.int("adID"));
    rows::rowset(HEADER, ads.iter().filter_map(|a| ad_line(db, a)).map(Value::List).collect())
}

/// The tables are in the store's schema. The Trinity static export lacks the
/// advert type catalogue: imported original rows are served verbatim; no
/// later-era or invented categories are seeded.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

fn number(call: &Call, i: usize) -> Option<i64> {
    call.arg_i64(i)
        .or_else(|| call.arg_text(i)?.trim().parse().ok())
}

fn failure(e: StoreError) -> Outcome {
    tracing::error!("recruitment: {}", crate::logging::chain(&e));
    Outcome::custom("The recruitment advert could not be saved. Please try again.")
}

/// Why an advert change was not made, undoing whatever it had done.
enum Failed {
    Refused(Outcome),
    Store(StoreError),
}

impl From<StoreError> for Failed {
    fn from(e: StoreError) -> Self {
        Failed::Store(e)
    }
}

impl From<Outcome> for Failed {
    fn from(o: Outcome) -> Self {
        Failed::Refused(o)
    }
}

pub async fn corporation_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "GetRecruitmentAdTypes" => ctx.store.read(|db| {
            const COLUMNS: &[&str] = &["typeID", "typeMask", "typeName", "description", "groupID", "groupName"];
            let mut types: Vec<Row> = db.table("crpRecruitmentAdTypes").rows().collect();
            types.sort_by_key(|t| (t.int("groupID"), t.int("typeID")));
            rows::rowset(COLUMNS, types.iter().map(|t| Value::List(t.line(COLUMNS))).collect())
        }),
        "GetRecruitmentAdsForCorporation" => {
            let corp = call.arg_i64(0).or(ctx.session.corp_id).unwrap_or(0);
            ctx.store.read(|db| corporation_ads(db, corp))
        }
        "GetRecruitmentAdsByCriteria" => {
            let region = number(call, 0).unwrap_or(0);
            let skill = number(call, 1).unwrap_or(i64::MAX).max(0);
            let types = number(call, 2).unwrap_or(0).max(0);
            let races = number(call, 3).unwrap_or(0).max(0);
            let alliance = number(call, 4).unwrap_or(0) != 0;
            let min = number(call, 5).unwrap_or(0).max(0);
            let max = number(call, 6).unwrap_or(0).max(0);
            let now = crate::bulkdata::filetime_now();
            let wants = |mask: i64, wanted: i64| wanted == 0 || mask == 0 || mask & wanted != 0;
            ctx.store.read(|db| {
                let (corps, stations) = (db.table("corporation"), db.table("staStations"));
                let mut found: Vec<(i64, i64, Vec<Value>)> = db
                    .table("crpRecruitmentAds")
                    .rows()
                    .filter(|a| a.int("expiryDateTime").is_some_and(|e| e > now))
                    .filter(|a| a.int("skillPoints").is_some_and(|s| s <= skill))
                    .filter(|a| wants(a.int("typeMask").unwrap_or(0), types) && wants(a.int("raceMask").unwrap_or(0), races))
                    .filter(|a| {
                        let Some(c) = a.int("corporationID").and_then(|id| corps.get_id(id)) else { return false };
                        let Some(s) = a.int("stationID").and_then(|id| stations.get_id(id)) else { return false };
                        let members = c.int("memberCount").unwrap_or(0);
                        (region <= 0 || s.int("regionID") == Some(region))
                            && (!alliance || c.int("allianceID").unwrap_or(0) != 0)
                            && members >= min
                            && (max == 0 || members <= max)
                    })
                    .filter_map(|a| Some((a.int("createDateTime")?, a.int("adID")?, ad_line(db, &a)?)))
                    .collect();
                found.sort_by(|x, y| (y.0, y.1).cmp(&(x.0, x.1)));
                found.truncate(1000);
                rows::rowset(HEADER, found.into_iter().map(|(_, _, line)| Value::List(line)).collect())
            })
        }
        _ => return None,
    };
    Some(Outcome::Value(result))
}

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method == "GetRecruitmentAdsForCorporation" {
        let corp = ctx.session.corp_id.unwrap_or(0);
        return Some(Outcome::Value(ctx.store.read(|db| corporation_ads(db, corp))));
    }
    if !matches!(
        call.method.as_str(),
        "CreateRecruitmentAd" | "UpdateRecruitmentAd" | "DeleteRecruitmentAd"
    ) {
        return None;
    }
    Some(write_ad(call, ctx).await.unwrap_or_else(|e| e))
}

fn validate(races: i64, types: i64, skill: i64, description: &str) -> Result<(), Outcome> {
    if races < 0
        || races & !15 != 0
        || types < 0
        || !(0..=200_000_000).contains(&skill)
        || description.chars().count() > 1000
    {
        return Err(Outcome::custom(
            "Invalid recruitment criteria or description.",
        ));
    }
    Ok(())
}

async fn write_ad(call: &Call, ctx: &Ctx<'_>) -> Result<Outcome, Outcome> {
    let corp = ctx
        .session
        .corp_id
        .ok_or_else(|| Outcome::custom("Join a corporation first."))?;
    let character = ctx
        .session
        .char_id
        .ok_or_else(|| Outcome::custom("Select a character first."))?;
    if !ctx.session.has_corp_role(128)
        && !ctx
            .session
            .has_corp_role(crate::session::CORP_ROLE_DIRECTOR)
    {
        return Err(Outcome::custom(
            "Only personnel managers may manage recruitment adverts.",
        ));
    }
    let create = call.method == "CreateRecruitmentAd";
    let delete = call.method == "DeleteRecruitmentAd";
    let written = ctx.store.write(|tx| -> Result<_, Failed> {
        // Verify membership in the same transaction as the change.
        let member = tx.table("corporation").get_id(corp).is_some()
            && corp >= 100_000_000
            && tx.table("character_").get_id(character).is_some_and(|p| p.int("corporationID") == Some(corp));
        if !member {
            return Err(Outcome::custom(
                "Only members of player corporations may manage adverts.",
            )
            .into());
        }
        let mut id = number(call, 0).unwrap_or(0);
        let old = if create {
            None
        } else {
            let line = own_ad(tx.db(), corp, id).and_then(|a| ad_line(tx.db(), &a));
            Some(line.ok_or_else(|| Outcome::custom("That advert does not belong to your corporation."))?)
        };
        if delete {
            if let Some(row) = own_ad(tx.db(), corp, id).map(|a| a.id) {
                tx.delete("crpRecruitmentAds", row)?;
            }
        } else {
            let races = number(call, 2).unwrap_or(-1);
            let types = number(call, if create { 3 } else { 1 }).unwrap_or(-1);
            let skill = number(call, if create { 5 } else { 3 }).unwrap_or(-1);
            let description = call
                .arg_text(if create { 6 } else { 4 })
                .unwrap_or_default();
            validate(races, types, skill, &description)?;
            let masks = [
                ("raceMask", store::Value::Int(races)),
                ("typeMask", store::Value::Int(types)),
                ("skillPoints", store::Value::Int(skill)),
                ("description", store::Value::Text(description.as_str().into())),
            ];
            if create {
                let days = number(call, 0).unwrap_or(0);
                if !(1..=30).contains(&days) {
                    return Err(Outcome::custom("Adverts must run for 1 to 30 days.").into());
                }
                let station = number(call, 1).unwrap_or(0);
                let office = tx
                    .table("crpOffices")
                    .find(&["corporationID"], &[store::Value::Int(corp)])
                    .any(|o| o.int("stationID") == Some(station));
                if !office {
                    return Err(Outcome::custom("Your corporation must rent an office at this station.").into());
                }
                let now = crate::bulkdata::filetime_now();
                let expires = now + days * DAY;
                let mut sets = vec![
                    ("corporationID", store::Value::Int(corp)),
                    ("stationID", store::Value::Int(station)),
                    ("createDateTime", store::Value::Int(now)),
                    ("expiryDateTime", store::Value::Int(expires)),
                ];
                sets.extend(masks);
                let row = tx.insert("crpRecruitmentAds", &sets)?;
                id = tx.table("crpRecruitmentAds").row(row).and_then(|a| a.int("adID")).unwrap_or(0);
                let arg = id.to_string();
                let entry = wallet::Entry {
                    ref_type: 86,
                    payer: corp,
                    payee: 0,
                    arg_id1: &arg,
                    reason: "Corporation recruitment advert",
                };
                wallet::post(
                    tx,
                    corp,
                    -(500_000.0 + 250_000.0 * days as f64),
                    &entry,
                )
                .map_err(|e| match e {
                    wallet::Transfer::NotEnough { need, have } => Failed::Refused(Outcome::not_enough_money(need, have)),
                    wallet::Transfer::Db(e) => Failed::Store(e),
                    wallet::Transfer::NoSuchOwner => Failed::Refused(Outcome::custom("Corporation wallet not found.")),
                })?;
            } else if let Some(row) = tx.table("crpRecruitmentAds").get_id(id).map(|a| a.id) {
                tx.update("crpRecruitmentAds", row, &masks)?;
            }
        }
        let new = if delete {
            None
        } else {
            let line = tx.table("crpRecruitmentAds").get_id(id).and_then(|a| ad_line(tx.db(), &a));
            Some(line.ok_or_else(|| failure(StoreError { code: store::DATA_EXCEPTION, message: format!("advert {id} cannot be listed") }))?)
        };
        Ok((id, old, new))
    });
    let (id, old, new) = written.map_err(|e| match e {
        Failed::Refused(o) => o,
        Failed::Store(e) => failure(e),
    })?;
    if create {
        wallet::notify_balance(ctx, corp).await;
    }
    let world = ctx.world.lock();
    world.notify(
        &world.corp_members(corp),
        "OnCorporationRecruitmentAdChanged",
        Value::Tuple(vec![
            Value::Int(corp),
            Value::Int(id),
            rows::change(HEADER, old.as_deref(), new.as_deref()),
        ]),
    );
    Ok(Outcome::Value(if create {
        Value::Int(id)
    } else {
        Value::None
    }))
}
