//! Read-only corpRegistry views: the corporation's offices and stations,
//! member tracking, the member search, items locked by a vote, and rented
//! factory and research slots.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db};
use crate::svc::bound;
use crate::svc::corp::titles;
use crate::svc::rows::{header_value, index_rowset, rowset};

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

/// FILETIME ticks in an hour and a day.
const HOUR: i64 = 36_000_000_000;
const DAY: i64 = 24 * HOUR;

fn station_type(db: &Db, station: i64) -> Value {
    Value::int_or_none(db.table("staStations").get_id(station).and_then(|s| s.int("stationTypeID")))
}

// ---- offices --------------------------------------------------------------------------

/// The office list's columns. `typeID` is the station's type: both of its
/// readers open the station's menu with it (corp_ui_home.py:330,342).
const OFFICE_HEADER: &[&str] = &["officeID", "stationID", "typeID", "officeFolderID", "corporationID"];

/// Bound object name of a corporation's office list.
pub const OFFICES_PROVIDER: &str = "corpOffices";

/// The corporation's offices, keyed (and ordered) by officeID.
fn office_lines(db: &Db, corp: i64) -> Vec<(i64, Vec<Value>)> {
    let mut lines: Vec<(i64, Vec<Value>)> = db
        .table("crpOffices")
        .find(&["corporationID"], &[int(corp)])
        .filter_map(|o| {
            let office = o.int("itemID")?;
            let station = o.int("stationID")?;
            Some((office, vec![Value::Int(office), Value::Int(station), station_type(db, station), o.cell("officeFolderID"), Value::Int(corp)]))
        })
        .collect();
    lines.sort_by_key(|(id, _)| *id);
    lines
}

/// GetOffices(): a util.SparseRowset over [`offices_provider`]; the client
/// Fetches it and looks offices up with SelectByUniqueColumnValues
/// (invCache.py:423, manufacturing.py:2275, corp_ui_votes.py:505).
pub async fn offices(ctx: &Ctx<'_>) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let count = ctx.store.read(|db| office_lines(db, corp).len()) as i64;
    let provider = bound::object_ref_with(
        ctx.node_id,
        OFFICES_PROVIDER,
        &[corp],
        vec![("realRowCount", Value::Int(count)), ("header", header_value(OFFICE_HEADER))],
    );
    Value::Instance {
        class: Box::new(Value::bytes("util.SparseRowset")),
        state: Box::new(Value::Tuple(vec![header_value(OFFICE_HEADER), provider, Value::Int(count)])),
    }
}

/// The office list provider (util.SparseRowsetProvider, rowset.py:620):
/// Fetch(start, count) -> [(pk, line)]; FetchByKey(pks) and
/// SelectByUniqueColumnValues(column, values) -> [(pk, position, line)].
pub async fn offices_provider(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if !matches!(call.method.as_str(), "Fetch" | "FetchByKey" | "SelectByUniqueColumnValues") {
        return None;
    }
    let corp = bound::parse(&call.service).and_then(|b| b.params.first().copied()).unwrap_or(0);
    if ctx.session.corp_id != Some(corp) {
        return Some(Outcome::custom("That is not your corporation's office list."));
    }
    let lines = ctx.store.read(|db| office_lines(db, corp));
    let indexed = |(i, (pk, line)): (usize, (i64, Vec<Value>))| Value::Tuple(vec![Value::Int(pk), Value::Int(i as i64), Value::List(line)]);
    let value: Vec<Value> = match call.method.as_str() {
        "Fetch" => {
            let start = call.arg_i64(0).unwrap_or(0).max(0) as usize;
            let size = call.arg_i64(1).unwrap_or(0).max(0) as usize;
            lines.into_iter().skip(start).take(size).map(|(pk, line)| Value::Tuple(vec![Value::Int(pk), Value::List(line)])).collect()
        }
        "FetchByKey" => {
            let wanted: Vec<i64> = call.args.first().and_then(Value::seq).unwrap_or(&[]).iter().filter_map(Value::as_i64).collect();
            lines.into_iter().enumerate().filter(|(_, (pk, _))| wanted.contains(pk)).map(indexed).collect()
        }
        _ => {
            let column = call.arg_text(0).unwrap_or_default();
            let Some(at) = OFFICE_HEADER.iter().position(|h| *h == column) else {
                return Some(Outcome::custom("The office list has no such column."));
            };
            let wanted: Vec<i64> = call.args.get(1).and_then(Value::seq).unwrap_or(&[]).iter().filter_map(Value::as_i64).collect();
            // Each value matches its first row only (rowset.py:720-733).
            let mut left = wanted;
            let mut out = Vec::new();
            for (i, (pk, line)) in lines.into_iter().enumerate() {
                if left.is_empty() {
                    break;
                }
                if let Some(found) = line.get(at).and_then(Value::as_i64).and_then(|v| left.iter().position(|w| *w == v)) {
                    left.remove(found);
                    out.push(indexed((i, (pk, line))));
                }
            }
            out
        }
    };
    Some(Outcome::Value(Value::List(value)))
}

// ---- stations -------------------------------------------------------------------------

/// GetStations(): the stations the corporation owns, with their types
/// (corp_ui_home.py:362-369).
pub async fn stations(ctx: &Ctx<'_>) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let mut lines: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
        db.table("staStations")
            .find(&["corporationID"], &[int(corp)])
            .filter_map(|s| Some((s.int("stationID")?, s.line(&["stationID", "stationTypeID", "solarSystemID"]))))
            .collect()
    });
    lines.sort_by_key(|(id, _)| *id);
    rowset(&["stationID", "typeID", "solarSystemID"], lines.into_iter().map(|(_, l)| Value::List(l)).collect())
}

// ---- member tracking ------------------------------------------------------------------

const TRACKING_SIMPLE: &[&str] =
    &["characterID", "title", "baseID", "startDateTime", "roles", "grantableRoles", "lastOnline"];
const TRACKING_FULL: &[&str] = &[
    "characterID", "title", "baseID", "startDateTime", "roles", "grantableRoles", "lastOnline",
    "logonDateTime", "logoffDateTime", "shipTypeID", "locationID",
];

/// GetMemberTrackingInfo() (directors) / GetMemberTrackingInfoSimple()
/// (base_corporation.py:757-761). `lastOnline` is hours since the member was
/// last online, negative while online (corp_ui_member_tracking.py:44-54).
///
/// The server keeps no logon time: `logonDateTime` is None, and
/// `logoffDateTime` is the last disconnect (`character_.lastOnline`).
pub async fn member_tracking(ctx: &Ctx<'_>, full: bool) -> Outcome {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let online: std::collections::HashMap<i64, crate::world::Presence> =
        ctx.world.lock().players().into_iter().map(|p| (p.char_id, p)).collect();
    let lines = ctx.store.read(|db| -> Result<Vec<(i64, Vec<Value>)>, Outcome> {
        if full {
            let ceo = db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"));
            let own = db.table("chrCorporationRoles").get_id(me).and_then(|r| r.int("rolesAtAll")).unwrap_or(0);
            if ceo != Some(me) && own & CORP_ROLE_DIRECTOR == 0 {
                return Err(crate::svc::membership::access_denied("only the CEO and directors may see full member tracking"));
            }
        }
        let mut lines: Vec<(i64, Vec<Value>)> = db
            .table("character_")
            .find(&["corporationID"], &[int(corp)])
            .filter_map(|c| {
                let id = c.int("characterID")?;
                let roles = db.table("chrCorporationRoles").get_id(id);
                let mask = |column: &str| Value::Long(roles.and_then(|r| r.int(column)).unwrap_or(0));
                let presence = online.get(&id);
                let last_logoff = c.int("lastOnline").filter(|t| *t > 0);
                let hours = match (presence, last_logoff) {
                    (Some(_), _) => -1,
                    (None, Some(t)) => (now - t).max(0) / HOUR,
                    // Never recorded: longer ago than the client's last band.
                    (None, None) => 24 * 365,
                };
                let mut line = vec![
                    Value::Int(id),
                    c.cell("title"),
                    c.cell("baseID"),
                    c.cell("corporationDateTime"),
                    mask("rolesAtAll"),
                    mask("grantableRoles"),
                    Value::Int(hours),
                ];
                if full {
                    let location = match presence {
                        Some(p) => p.station_id.unwrap_or(p.solar_system_id),
                        None => c.int("stationID").filter(|s| *s > 0).or(c.int("solarSystemID")).unwrap_or(0),
                    };
                    let ship_type = c.int("shipID").and_then(|s| db.table("entity").get_id(s)).and_then(|e| e.int("typeID"));
                    line.extend([
                        Value::None,
                        last_logoff.map_or(Value::None, Value::Long),
                        Value::int_or_none(ship_type),
                        Value::int_or_none(Some(location).filter(|l| *l > 0)),
                    ]);
                }
                Some((id, line))
            })
            .collect();
        lines.sort_by_key(|(id, _)| *id);
        Ok(lines)
    });
    match lines {
        Ok(lines) => Outcome::Value(rowset(
            if full { TRACKING_FULL } else { TRACKING_SIMPLE },
            lines.into_iter().map(|(_, l)| Value::List(l)).collect(),
        )),
        Err(refused) => refused,
    }
}

/// Note the time a character's connection closed, for member tracking.
pub fn record_logoff(store: &crate::store::Store, char_id: i64) -> Result<(), store::StoreError> {
    let now = crate::bulkdata::filetime_now();
    store.write(|tx| match tx.table("character_").get_id(char_id).map(|c| c.id) {
        Some(row) => tx.update("character_", row, &[("lastOnline", int(now))]),
        None => Ok(()),
    })
}

// ---- member search --------------------------------------------------------------------

/// corp_ui_member_find.py:1-16.
const OR: i64 = 1;
const AND: i64 = 2;
const EQUAL: i64 = 1;
const GREATER: i64 = 2;
const GREATER_OR_EQUAL: i64 = 3;
const LESS: i64 = 4;
const LESS_OR_EQUAL: i64 = 5;
const NOT_EQUAL: i64 = 6;
const HAS_BIT: i64 = 7;
const NOT_HAS_BIT: i64 = 8;
const STR_CONTAINS: i64 = 9;
const STR_LIKE: i64 = 10;
const STR_STARTS_WITH: i64 = 11;
const STR_ENDS_WITH: i64 = 12;
const STR_IS: i64 = 13;

const ROLE_PROPERTIES: [&str; 8] = [
    "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// One term of a search: [property, operator, value], preceded by its join
/// operator after the first (corp_ui_member_find.py:254-262).
struct Term {
    join: Option<i64>,
    property: String,
    operator: i64,
    value: Value,
}

fn parse_terms(query: Option<&Value>) -> Option<Vec<Term>> {
    let terms = query?.seq()?;
    if terms.len() > 50 {
        return None;
    }
    terms
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let t = t.seq()?;
            let (join, rest) = match (i, t.len()) {
                (0, 3) => (None, t),
                (_, 4) => (Some(t[0].as_i64().filter(|j| matches!(*j, OR | AND))?), &t[1..]),
                _ => return None,
            };
            Some(Term { join, property: rest[0].as_string()?, operator: rest[1].as_i64()?, value: rest[2].clone() })
        })
        .collect()
}

/// A member, as the search sees them.
struct Candidate {
    id: i64,
    name: String,
    base: Option<i64>,
    start: i64,
    title_mask: i64,
    /// In `ROLE_PROPERTIES` order.
    roles: [i64; 8],
}

fn compare(a: i64, op: i64, b: i64) -> Option<bool> {
    Some(match op {
        EQUAL => a == b,
        NOT_EQUAL => a != b,
        GREATER => a > b,
        GREATER_OR_EQUAL => a >= b,
        LESS => a < b,
        LESS_OR_EQUAL => a <= b,
        _ => return None,
    })
}

/// A SQL LIKE pattern (`%` any run, `_` one character), ignoring case.
fn like(text: &str, pattern: &str) -> bool {
    fn go(t: &[char], p: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('%', rest)) => (0..=t.len()).any(|i| go(&t[i..], rest)),
            Some(('_', rest)) => !t.is_empty() && go(&t[1..], rest),
            Some((c, rest)) => t.first() == Some(c) && go(&t[1..], rest),
        }
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    go(&t, &p)
}

fn matches(c: &Candidate, term: &Term) -> Option<bool> {
    let property = term.property.as_str();
    if let Some(at) = ROLE_PROPERTIES.iter().position(|p| *p == property) {
        let bits = term.value.as_i64()?;
        let has = c.roles[at] & bits == bits;
        return match term.operator {
            HAS_BIT => Some(has),
            NOT_HAS_BIT => Some(!has),
            _ => None,
        };
    }
    match property {
        "titleMask" => {
            let bits = term.value.as_i64()?;
            let has = c.title_mask & bits == bits;
            match term.operator {
                HAS_BIT => Some(has),
                NOT_HAS_BIT => Some(!has),
                _ => None,
            }
        }
        "baseID" => {
            let want = term.value.as_i64();
            match term.operator {
                EQUAL => Some(c.base == want),
                NOT_EQUAL => Some(c.base != want),
                _ => None,
            }
        }
        // The date picker gives midnight of a day (datepicker.py:60-64):
        // equal means joined that day.
        "startDateTime" => {
            let day = term.value.as_i64()?;
            let joined = c.start - c.start.rem_euclid(DAY);
            let day = day - day.rem_euclid(DAY);
            compare(joined, term.operator, day)
        }
        // Searched by name (an edit control, corp_ui_member_find.py:137).
        "characterID" => {
            let want = term.value.as_string()?.to_lowercase();
            let name = c.name.to_lowercase();
            Some(match term.operator {
                STR_CONTAINS => name.contains(&want),
                STR_LIKE => like(&name, &want),
                STR_STARTS_WITH => name.starts_with(&want),
                STR_ENDS_WITH => name.ends_with(&want),
                STR_IS => name == want,
                _ => return None,
            })
        }
        _ => None,
    }
}

/// GetMemberIDsByQuery(query, includeImplied, searchTitles): the members
/// matching the terms, joined left to right (corp_ui_member_find.py:252-266).
/// With `searchTitles` a member's roles include their titles'; with
/// `includeImplied` a director (or the CEO) counts as holding every role.
pub async fn member_ids_by_query(ctx: &Ctx<'_>, args: &[Value]) -> Outcome {
    let Some(terms) = parse_terms(args.first()) else {
        return Outcome::custom("That member search could not be understood.");
    };
    let implied = args.get(1).and_then(Value::as_i64).unwrap_or(0) != 0;
    let with_titles = args.get(2).and_then(Value::as_i64).unwrap_or(0) != 0;
    let corp = ctx.session.corp_id.unwrap_or(0);
    let found = ctx.store.read(|db| -> Option<Vec<i64>> {
        let ceo = db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"));
        let mut ids = Vec::new();
        for c in db.table("character_").find(&["corporationID"], &[int(corp)]) {
            let Some(id) = c.int("characterID") else { continue };
            let own = db.table("chrCorporationRoles").get_id(id);
            let mut roles = [
                "rolesAtAll", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
                "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
            ]
            .map(|col| own.and_then(|r| r.int(col)).unwrap_or(0));
            let title_mask = c.int("titleMask").unwrap_or(0) & titles::ALL_TITLES;
            if with_titles {
                for (r, t) in roles.iter_mut().zip(titles::granted(db, corp, title_mask)) {
                    *r |= t;
                }
            }
            if implied && (ceo == Some(id) || roles[0] & CORP_ROLE_DIRECTOR != 0) {
                roles = [-1; 8];
            }
            let candidate = Candidate {
                id,
                name: c.text("characterName").unwrap_or_default().to_owned(),
                base: c.int("baseID"),
                start: c.int("corporationDateTime").unwrap_or(0),
                title_mask,
                roles,
            };
            let mut result: Option<bool> = None;
            for term in &terms {
                let hit = matches(&candidate, term)?;
                result = Some(match (result, term.join) {
                    (None, _) => hit,
                    (Some(r), Some(OR)) => r || hit,
                    (Some(r), _) => r && hit,
                });
            }
            if result == Some(true) {
                ids.push(candidate.id);
            }
        }
        ids.sort_unstable();
        Some(ids)
    });
    match found {
        Some(ids) => Outcome::Value(Value::List(ids.into_iter().map(Value::Int).collect())),
        None => Outcome::custom("That member search uses a property or operator this list does not support."),
    }
}

// ---- locked items and rentals ---------------------------------------------------------

const LOCKED_HEADER: &[&str] = &["itemID", "typeID", "ownerID", "locationID"];

/// GetLockedItemsByLocation(locationID): the corporation's items locked by a
/// vote there, an IndexRowset keyed on itemID (bco_itemLocking.py:48-56,
/// corp_ui_accounts.py:349-365).
pub async fn locked_items_by_location(ctx: &Ctx<'_>, location: i64) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let mut items: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
        db.table("crpItemLocks")
            .find(&["corporationID"], &[int(corp)])
            .filter(|r| r.int("locationID") == Some(location))
            .filter_map(|r| Some((r.int("itemID")?, vec![r.cell("itemID"), r.cell("typeID"), Value::Int(corp), r.cell("locationID")])))
            .collect()
    });
    items.sort_by_key(|(id, _)| *id);
    index_rowset(LOCKED_HEADER, "itemID", items.into_iter().map(|(id, l)| (Value::Int(id), Value::List(l))).collect())
}

/// GetRentalDetailsPlayer() / GetRentalDetailsCorp(): rented factory and
/// research slots (wallet.py:1199-1212). This server rents none
/// (AreAnyFactorySlotsAvailableForRenting is 0), so there are none to list.
pub fn rental_details() -> Value {
    Value::List(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Candidate {
        Candidate { id: 7, name: "Ada Lovelace".into(), base: Some(60000004), start: 5 * DAY + 3 * HOUR, title_mask: 0b101, roles: [0b11, 0, 0, 0, 0, 0, 0, 0] }
    }

    fn term(property: &str, operator: i64, value: Value) -> Term {
        Term { join: None, property: property.into(), operator, value }
    }

    #[test]
    fn roles_titles_and_names_match() {
        let c = candidate();
        assert_eq!(matches(&c, &term("roles", HAS_BIT, Value::Int(0b10))), Some(true));
        assert_eq!(matches(&c, &term("roles", NOT_HAS_BIT, Value::Int(0b100))), Some(true));
        assert_eq!(matches(&c, &term("titleMask", HAS_BIT, Value::Int(0b100))), Some(true));
        assert_eq!(matches(&c, &term("characterID", STR_STARTS_WITH, Value::bytes("ada"))), Some(true));
        assert_eq!(matches(&c, &term("characterID", STR_LIKE, Value::bytes("a%lace"))), Some(true));
        assert_eq!(matches(&c, &term("characterID", STR_IS, Value::bytes("ada"))), Some(false));
        assert_eq!(matches(&c, &term("baseID", NOT_EQUAL, Value::Int(60000004))), Some(false));
    }

    #[test]
    fn dates_compare_by_day() {
        let c = candidate();
        assert_eq!(matches(&c, &term("startDateTime", EQUAL, Value::Long(5 * DAY))), Some(true));
        assert_eq!(matches(&c, &term("startDateTime", GREATER, Value::Long(4 * DAY))), Some(true));
        assert_eq!(matches(&c, &term("startDateTime", LESS, Value::Long(5 * DAY))), Some(false));
    }

    #[test]
    fn unsupported_terms_are_refused() {
        let c = candidate();
        assert_eq!(matches(&c, &term("roles", EQUAL, Value::Int(1))), None);
        assert_eq!(matches(&c, &term("shipID", EQUAL, Value::Int(1))), None);
    }

    #[test]
    fn queries_parse_with_join_operators() {
        let q = Value::List(vec![
            Value::List(vec![Value::bytes("roles"), Value::Int(HAS_BIT), Value::Int(1)]),
            Value::List(vec![Value::Int(OR), Value::bytes("titleMask"), Value::Int(HAS_BIT), Value::Int(4)]),
        ]);
        let terms = parse_terms(Some(&q)).unwrap();
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[1].join, Some(OR));
        // A later term needs its join operator.
        let bad = Value::List(vec![q.seq().unwrap()[0].clone(), q.seq().unwrap()[0].clone()]);
        assert!(parse_terms(Some(&bad)).is_none());
    }
}
