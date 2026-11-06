use eve_marshal::Value;
use tracing::{error, warn};

use crate::items::invitem_cells;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, StoreError};
use crate::svc::rows::rowset;

const BOOKMARK_HEADER: &[&str] = &[
    "bookmarkID", "ownerID", "itemID", "typeID", "flag", "memo", "created", "x", "y", "z",
    "locationID", "note",
];

/// What a voucher keeps of its bookmark.
const VOUCHER_COLUMNS: &[&str] = &["itemID", "typeID", "flag", "created", "x", "y", "z", "locationID", "note"];

/// The longest note the client's bookmark windows take (addressbook.py).
const NOTE_MAX: usize = 6000;

fn clip_note(note: Option<String>) -> Option<String> {
    note.map(|n| n.chars().take(NOTE_MAX).collect())
}

pub async fn bookmark(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetBookmarks" => get_bookmarks(ctx).await,
        // BookmarkLocation(itemID, ownerID, memo, note).
        "BookmarkLocation" => {
            let note = clip_note(call.arg_text(3)).unwrap_or_default();
            bookmark_location(ctx, arg(0).unwrap_or(0), call.arg_text(2), note).await
        }
        // UpdateBookmark(bookmarkID, ownerID, memo, note=note).
        "UpdateBookmark" => {
            let note = clip_note(call.kwarg("note").and_then(|v| v.as_string()).or_else(|| call.arg_text(3)));
            update_bookmark(ctx, arg(0).unwrap_or(0), call.arg_text(2), note).await
        }
        "DeleteBookmarks" => delete_bookmarks(ctx, call.args.first()).await,

        // CreateBookmarkVoucher(bookmarkID, memo) -> the voucher, which the
        // container it was dropped on then takes (container.py:457).
        "CreateBookmarkVoucher" => match make_vouchers(ctx, &[(arg(0).unwrap_or(0), call.arg_text(1))]).await {
            Ok(mut made) => made.pop().unwrap_or(Value::None),
            Err(refusal) => return Some(refusal),
        },
        // CreateBookmarkVouchers([(bookmarkID, memo) or (bookmarkID,)]).
        "CreateBookmarkVouchers" => {
            let wanted: Vec<(i64, Option<String>)> = call
                .args
                .first()
                .and_then(|v| v.seq())
                .unwrap_or(&[])
                .iter()
                .filter_map(|p| {
                    let p = p.seq()?;
                    Some((p.first()?.as_i64()?, p.get(1).and_then(|m| m.as_string())))
                })
                .collect();
            match make_vouchers(ctx, &wanted).await {
                Ok(made) => Value::List(made),
                Err(refusal) => return Some(refusal),
            }
        }
        // AddBookmarkFromVoucher(voucherID): the voucher's bookmark, copied
        // into the caller's places; the voucher stays.
        "AddBookmarkFromVoucher" => from_voucher(ctx, arg(0).unwrap_or(0)).await,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// typeBookmark, of groupVoucher.
const TYPE_BOOKMARK: i64 = 51;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;

/// Bookmarks made into vouchers: in the station hangar when docked, else in
/// the ship's hold. Each answer carries its voucherID.
async fn make_vouchers(ctx: &Ctx<'_>, wanted: &[(i64, Option<String>)]) -> Result<Vec<Value>, Outcome> {
    let Some(me) = ctx.session.char_id else { return Err(Outcome::custom("No character.")) };
    let (location, flag) = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => (station, FLAG_HANGAR),
        (None, Some(ship)) => (ship, FLAG_CARGO),
        _ => return Err(Outcome::custom("You have nowhere to keep a voucher.")),
    };
    let made = ctx.store.write(|tx| -> Result<_, StoreError> {
        let mut txn = crate::items::ItemTxn::new(tx);
        let mut out = Vec::new();
        for (bookmark_id, memo) in wanted {
            let bookmark = txn.db().table("bookmarks").get_id(*bookmark_id).filter(|b| b.int("ownerID") == Some(me)).map(|b| {
                let kept: Vec<(&str, store::Value)> = VOUCHER_COLUMNS.iter().map(|c| (*c, b.value(c).clone())).collect();
                (kept, b.text("memo").unwrap_or_default().to_string())
            });
            let Some((kept, own_memo)) = bookmark else {
                out.push(Value::None);
                continue;
            };
            let id = txn.create(crate::items::NewItem { type_id: TYPE_BOOKMARK, owner_id: me, location_id: location, flag, quantity: 1, singleton: true })?;
            let memo: String = memo.clone().unwrap_or(own_memo);
            let mut sets = vec![("voucherID", store::Value::Int(id)), ("memo", store::Value::Text(memo.into()))];
            sets.extend(kept);
            txn.tx().insert("bookmarkVouchers", &sets)?;
            out.push(crate::svc::rows::key_val(vec![("voucherID", Value::Int(id))]));
        }
        Ok((out, txn.events()))
    });
    match made {
        Ok((out, events)) => {
            crate::items::notify(ctx.world, ctx.client_id, events);
            let count = out.iter().filter(|v| !matches!(v, Value::None)).count();
            tracing::info!("{me} made {count} bookmark voucher(s)");
            Ok(out)
        }
        Err(e) => {
            error!("making bookmark vouchers: {}", crate::logging::chain(&e));
            Err(Outcome::custom("The voucher could not be made."))
        }
    }
}

async fn from_voucher(ctx: &Ctx<'_>, voucher_id: i64) -> Value {
    let Some(me) = ctx.session.char_id else { return Value::None };
    let copied = ctx.store.write(|tx| -> Result<_, StoreError> {
        let owned = tx.table("entity").get_id(voucher_id).is_some_and(|e| e.int("ownerID") == Some(me));
        let Some(v) = tx.table("bookmarkVouchers").get_id(voucher_id).filter(|_| owned) else { return Ok(None) };
        let mut sets: Vec<(&str, store::Value)> = VOUCHER_COLUMNS.iter().map(|c| (*c, v.value(c).clone())).collect();
        sets.push(("memo", v.value("memo").clone()));
        sets.push(("ownerID", store::Value::Int(me)));
        let row = tx.insert("bookmarks", &sets)?;
        Ok(tx.table("bookmarks").row(row).map(|b| b.line(BOOKMARK_HEADER)))
    });
    match copied {
        Ok(Some(line)) => crate::svc::rows::row(BOOKMARK_HEADER, line),
        Ok(None) => Value::None,
        Err(e) => {
            error!("AddBookmarkFromVoucher({voucher_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

/// `voucher`: GetObject(voucherID), the voucher.Bookmark the client keeps
/// (voucherCache.py): its item row as header and data, and the memo it
/// shows as its name.
pub async fn voucher(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "GetObject" {
        return None;
    }
    let id = call.arg_id(0);
    let found = ctx.store.read(|db| {
        let item = db.table("entity").get_id(id).and_then(|r| invitem_cells(&r))?;
        let memo = db.table("bookmarkVouchers").get_id(id).and_then(|r| r.text("memo").map(str::to_string));
        Some((item, memo))
    });
    let Some((item, memo)) = found else {
        return Some(Outcome::Value(Value::None));
    };
    let memo = memo.unwrap_or_else(|| "Bookmark".into());
    let header = Value::List(crate::svc::inventory::INVITEM_HEADER.iter().map(|h| Value::bytes(*h)).collect());
    let state = Value::Dict(vec![
        (Value::bytes("header"), header),
        (Value::bytes("data"), Value::List(item)),
        (Value::bytes("description"), Value::text(memo)),
    ]);
    Some(Outcome::Value(Value::Instance { class: Box::new(Value::bytes("voucher.Bookmark")), state: Box::new(state) }))
}

async fn get_bookmarks(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset(BOOKMARK_HEADER, Vec::new()) };
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<_> = db.table("bookmarks").find(&["ownerID"], &[store::Value::Int(char_id)]).collect();
        rows.sort_by_key(|r| r.int("bookmarkID"));
        rows.iter().map(|r| Value::List(r.line(BOOKMARK_HEADER))).collect()
    });
    rowset(BOOKMARK_HEADER, lines)
}

const GROUP_REGION: i64 = 3;
const GROUP_CONSTELLATION: i64 = 4;
const GROUP_SOLAR_SYSTEM: i64 = 5;
const LOCATION_UNIVERSE: i64 = 9;
const TYPE_SOLAR_SYSTEM: i64 = 5;

/// Where `item_id` is now in the caller's system: their own ship (also for
/// the system itself, the spot they are at), or another ball they can see.
fn ball_position(ctx: &Ctx<'_>, item_id: i64) -> Option<(i64, crate::ballpark::Vec3)> {
    let system = ctx.session.solar_system_id?;
    let ship = ctx.session.ship_id?;
    let target = if item_id == system { ship } else { item_id };
    let mut world = ctx.world.lock();
    let park = world.parks.get_mut(&system)?;
    park.advance(crate::destiny::tick_now());
    if target != ship
        && (park.ships.get(&target).is_some_and(|s| s.cloaked) || park.beacons.get(&target).is_some_and(|b| b.hidden))
    {
        return None;
    }
    let (at, _) = park.ball(target)?;
    Some((system, at))
}

async fn bookmark_location(ctx: &Ctx<'_>, item_id: i64, memo: Option<String>, note: String) -> Value {
    let failed = || {
        Value::Tuple(vec![
            Value::Int(0),
            Value::Int(item_id),
            Value::None,
            Value::None,
            Value::None,
            Value::None,
            Value::None,
        ])
    };
    let Some(char_id) = ctx.session.char_id else { return failed() };

    let found = ctx.store.read(|db| {
        db.table("mapDenormalize").get_id(item_id).map(|r| {
            let col = |c: &str| r.int(c);
            let at = (r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0));
            (col("typeID").unwrap_or(0), col("solarSystemID"), at, col("groupID"), col("constellationID"), col("regionID"))
        })
    });
    let Some((type_id, system, at, group, constellation, region)) = found else {
        // Not a celestial: "Save Location" in space (the client sends the
        // ship or the system, addressbook.BookmarkCurrentLocation) or a
        // ball in the park. It becomes a coordinate bookmark -- itemID and
        // locationID the system, typeSolarSystem -- since the client takes
        // a bookmark whose itemID is still a ball to be that ball's current
        // position (menusvc CelestialMenu).
        let Some((system, at)) = ball_position(ctx, item_id) else {
            warn!("BookmarkLocation({item_id}): neither a celestial nor in the caller's ballpark");
            return failed();
        };
        let memo = memo.unwrap_or_default();
        let created = crate::bulkdata::filetime_now();
        let sets = [
            ("ownerID", store::Value::Int(char_id)),
            ("itemID", store::Value::Int(system)),
            ("typeID", store::Value::Int(TYPE_SOLAR_SYSTEM)),
            ("flag", store::Value::Null),
            ("memo", store::Value::Text(memo.into())),
            ("created", store::Value::Int(created)),
            ("x", store::Value::Float(at.x)),
            ("y", store::Value::Float(at.y)),
            ("z", store::Value::Float(at.z)),
            ("locationID", store::Value::Int(system)),
            ("note", store::Value::Text(note.into())),
        ];
        return match insert_bookmark(ctx, &sets) {
            Ok(id) => Value::Tuple(vec![
                id,
                Value::Int(system),
                Value::Int(TYPE_SOLAR_SYSTEM),
                Value::Double(at.x),
                Value::Double(at.y),
                Value::Double(at.z),
                Value::Int(system),
            ]),
            Err(e) => {
                error!("BookmarkLocation({item_id}) insert: {err}", err = crate::logging::chain(&e));
                failed()
            }
        };
    };

    // A system, constellation or region is filed under its parent with no
    // position: the client offers "Warp to" for any bookmark whose locationID
    // is the current system and warps to its x/y/z (menusvc CelestialMenu),
    // so a system filed under a system would warp to galactic coordinates.
    let (location_id, at) = match group {
        Some(GROUP_SOLAR_SYSTEM) => (constellation, None),
        Some(GROUP_CONSTELLATION) => (region, None),
        Some(GROUP_REGION) => (Some(LOCATION_UNIVERSE), None),
        _ => (system, Some(at)),
    };
    let location_id = location_id.or(ctx.session.solar_system_id2).unwrap_or(0);
    let (x, y, z) = (at.map(|p| p.0), at.map(|p| p.1), at.map(|p| p.2));
    let coord = |v: Option<f64>| v.map(Value::Double).unwrap_or(Value::None);
    let stored = |v: Option<f64>| v.map_or(store::Value::Null, store::Value::Float);

    let memo = memo.unwrap_or_default();
    let created = crate::bulkdata::filetime_now();
    let sets = [
        ("ownerID", store::Value::Int(char_id)),
        ("itemID", store::Value::Int(item_id)),
        ("typeID", store::Value::Int(type_id)),
        ("flag", store::Value::Null),
        ("memo", store::Value::Text(memo.into())),
        ("created", store::Value::Int(created)),
        ("x", stored(x)),
        ("y", stored(y)),
        ("z", stored(z)),
        ("locationID", store::Value::Int(location_id)),
        ("note", store::Value::Text(note.into())),
    ];
    match insert_bookmark(ctx, &sets) {
        Ok(id) => Value::Tuple(vec![
            id,
            Value::Int(item_id),
            Value::Int(type_id),
            coord(x),
            coord(y),
            coord(z),
            Value::Int(location_id),
        ]),
        Err(e) => {
            error!("BookmarkLocation({item_id}) insert: {err}", err = crate::logging::chain(&e));
            failed()
        }
    }
}

/// A new bookmark; its bookmarkID as the client is sent it.
fn insert_bookmark(ctx: &Ctx<'_>, sets: store::Sets) -> Result<Value, StoreError> {
    ctx.store.write(|tx| {
        let row = tx.insert("bookmarks", sets)?;
        Ok(tx.table("bookmarks").row(row).map_or(Value::None, |b| b.cell("bookmarkID")))
    })
}

async fn update_bookmark(ctx: &Ctx<'_>, bookmark_id: i64, memo: Option<String>, note: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    if memo.is_none() && note.is_none() {
        return Value::None;
    }
    let mut sets = Vec::new();
    if let Some(memo) = memo {
        sets.push(("memo", store::Value::Text(memo.into())));
    }
    if let Some(note) = note {
        sets.push(("note", store::Value::Text(note.into())));
    }
    let updated = ctx.store.write(|tx| {
        match tx.table("bookmarks").get_id(bookmark_id).filter(|b| b.int("ownerID") == Some(char_id)).map(|b| b.id) {
            Some(row) => tx.update("bookmarks", row, &sets),
            None => Ok(()),
        }
    });
    if let Err(e) = updated {
        error!("UpdateBookmark({bookmark_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn delete_bookmarks(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let ids: Vec<i64> = arg.and_then(|v| v.seq()).map(|s| s.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default();
    if ids.is_empty() {
        return Value::None;
    }
    let deleted = ctx.store.write(|tx| {
        for id in &ids {
            if let Some(row) = tx.table("bookmarks").get_id(*id).filter(|b| b.int("ownerID") == Some(char_id)).map(|b| b.id) {
                tx.delete("bookmarks", row)?;
            }
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = deleted {
        error!("DeleteBookmarks({} ids): {err}", ids.len(), err = crate::logging::chain(&e));
    }
    Value::None
}
