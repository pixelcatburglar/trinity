//! `lookupSvc`: the name searches behind the people/places pickers (uix.py __Search).
//!
//! The client reads results by column name through uix.GetAttr (`characterID`,
//! `ownerName`, ...) and in places by position (`each[1]` is the ID), so every
//! result is `[<x>Name, <x>ID, typeID]`.

use eve_marshal::Value;

use crate::sde;
use crate::services::{Call, Ctx, Outcome};
use crate::store::Db;

const TYPE_CORPORATION: i32 = 2;
const TYPE_FACTION: i32 = 30;
const TYPE_ALLIANCE: i32 = 16159;
/// cfg.IsNPC: IDs at or above this belong to players.
const FIRST_PLAYER_ID: i32 = 100_000_000;
const MAX_RESULTS: i64 = 500;

pub async fn lookup_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let search = call.arg_text(0).unwrap_or_default();
    let exact = call.arg_i64(1).unwrap_or(0) == 1;
    let value = match call.method.as_str() {
        "LookupOwners" => owners(ctx, &search, exact, false).await,
        "LookupPCOwners" | "LookupNoneNPCAccountOwners" => owners(ctx, &search, exact, true).await,
        "LookupCharacters" => {
            let header = ["characterName", "characterID", "typeID"];
            find(ctx, &header, &search, exact, |db, hit| {
                db.table("character_")
                    .rows()
                    .filter(|r| !r.value("accountID").is_null() && hit(r.text("characterName")))
                    .map(|r| r.line(&header))
                    .collect()
            })
            .await
        }
        "LookupCorporations" => corporations(ctx, &search, exact, false).await,
        "LookupAlliances" | "LookupAllianceShortNames" => {
            let column = if call.method == "LookupAlliances" { "allianceName" } else { "shortName" };
            find(ctx, &["allianceName", "allianceID", "typeID", "shortName"], &search, exact, |db, hit| {
                db.table("alliances")
                    .rows()
                    .filter(|r| r.int("deleted") == Some(0) && hit(r.text(column)))
                    .map(|r| vec![r.cell("allianceName"), r.cell("allianceID"), Value::Int(TYPE_ALLIANCE as i64), r.cell("shortName")])
                    .collect()
            })
            .await
        }
        "LookupPlayerCorporations" => corporations(ctx, &search, exact, true).await,
        "LookupCorporationTickers" => {
            find(ctx, &["corporationName", "corporationID", "typeID", "tickerName"], &search, exact, |db, hit| {
                db.table("corporation")
                    .rows()
                    .filter(|r| hit(r.text("tickerName")))
                    .map(|r| vec![r.cell("corporationName"), r.cell("corporationID"), Value::Int(TYPE_CORPORATION as i64), r.cell("tickerName")])
                    .collect()
            })
            .await
        }
        "LookupCorporationsOrAlliances" => {
            find(ctx, &["ownerName", "ownerID", "typeID"], &search, exact, |db, hit| {
                let corporations = db.table("corporation").rows().map(|r| (r, "corporationName", "corporationID", TYPE_CORPORATION));
                let alliances = db.table("alliances").rows().map(|r| (r, "allianceName", "allianceID", TYPE_ALLIANCE));
                corporations
                    .chain(alliances)
                    .filter(|(r, name, ..)| hit(r.text(name)))
                    .map(|(r, name, id, type_id)| vec![r.cell(name), r.cell(id), Value::Int(type_id as i64)])
                    .collect()
            })
            .await
        }
        "LookupStations" => {
            find(ctx, &["stationName", "stationID", "typeID"], &search, exact, |db, hit| {
                db.table("staStations")
                    .rows()
                    .filter(|r| hit(r.text("stationName")))
                    .map(|r| r.line(&["stationName", "stationID", "stationTypeID"]))
                    .collect()
            })
            .await
        }
        "LookupFactions" => factions(&search, exact),
        "LookupLocationsByGroup" => {
            // (groupID, searchStr): the arguments are the other way round here.
            let group = call.arg_i64(0).and_then(|g| i32::try_from(g).ok()).unwrap_or(0);
            let search = call.arg_text(1).unwrap_or_default();
            let header = ["itemName", "itemID", "typeID"];
            find(ctx, &header, &search, false, |db, hit| {
                db.table("mapDenormalize")
                    .rows()
                    .filter(|r| r.int("groupID") == Some(i64::from(group)) && hit(r.text("itemName")))
                    .map(|r| r.line(&header))
                    .collect()
            })
            .await
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn owners(ctx: &Ctx<'_>, search: &str, exact: bool, players_only: bool) -> Value {
    let header = ["ownerName", "ownerID", "typeID"];
    find(ctx, &header, search, exact, |db, hit| {
        db.table("eveStaticOwners")
            .rows()
            .filter(|r| (!players_only || r.int("ownerID").is_some_and(|id| id >= FIRST_PLAYER_ID as i64)) && hit(r.text("ownerName")))
            .map(|r| r.line(&header))
            .collect()
    })
    .await
}

async fn corporations(ctx: &Ctx<'_>, search: &str, exact: bool, players_only: bool) -> Value {
    find(ctx, &["corporationName", "corporationID", "typeID"], search, exact, |db, hit| {
        db.table("corporation")
            .rows()
            .filter(|r| (!players_only || r.int("corporationID").is_some_and(|id| id >= FIRST_PLAYER_ID as i64)) && hit(r.text("corporationName")))
            .map(|r| vec![r.cell("corporationName"), r.cell("corporationID"), Value::Int(TYPE_CORPORATION as i64)])
            .collect()
    })
    .await
}

/// The factions, which are static data, matched as `find` matches.
fn factions(search: &str, exact: bool) -> Value {
    const HEADER: &[&str] = &["factionName", "factionID", "typeID"];
    if search.is_empty() {
        return crate::svc::rows::rowset(HEADER, Vec::new());
    }
    let mut found: Vec<sde::Row> =
        sde::table("chrFactions").rows().filter(|r| r.text("factionName").is_some_and(|name| matches(name, search, exact))).collect();
    found.sort_by_key(|r| r.text("factionName"));
    found.truncate(MAX_RESULTS as usize);
    let lines = found
        .iter()
        .map(|r| {
            let mut line = r.line(&["factionName", "factionID"]);
            line.push(Value::Int(TYPE_FACTION as i64));
            Value::List(line)
        })
        .collect();
    crate::svc::rows::rowset(HEADER, lines)
}

/// Whether `text` matches `search` case-insensitively: equality when
/// `exact`, otherwise "contains". Letters fold as the database's lower() and
/// ILIKE folded them (ASCII only: its locale was C), and the search is
/// literal text, wildcards and all.
fn matches(text: &str, search: &str, exact: bool) -> bool {
    let text = text.to_ascii_lowercase();
    let search = search.to_ascii_lowercase();
    if exact { text == search } else { text.contains(&search) }
}

/// The lines `rows` gives for the rows whose searched column (a NULL never
/// matches) passes `hit`: the first [`MAX_RESULTS`] in the order of their
/// first column, the name.
async fn find(
    ctx: &Ctx<'_>,
    header: &[&str],
    search: &str,
    exact: bool,
    rows: impl FnOnce(&Db, &dyn Fn(Option<&str>) -> bool) -> Vec<Vec<Value>>,
) -> Value {
    if search.is_empty() {
        return crate::svc::rows::rowset(header, Vec::new());
    }
    let hit = |text: Option<&str>| text.is_some_and(|t| matches(t, search, exact));
    let lines = ctx.store.read(|db| {
        let mut found: Vec<(Option<String>, Vec<Value>)> = rows(db, &hit).into_iter().map(|line| (line[0].as_string(), line)).collect();
        // ORDER BY sorts NULLs last.
        found.sort_by(|(a, _), (b, _)| (a.is_none(), a).cmp(&(b.is_none(), b)));
        found.into_iter().take(MAX_RESULTS as usize).map(|(_, line)| Value::List(line)).collect()
    });
    crate::svc::rows::rowset(header, lines)
}
