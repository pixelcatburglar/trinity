//! `jumpCloneSvc`, bound as (locationID, groupID): jump clones
//! (clonejumpsvc.py, medical.py, charactersheet.py).
//!
//! A jump clone is a body waiting somewhere, with its own implants (items
//! located on the clone, flag implant). A pilot docked in a station with a
//! clone bay installs one there (`InstallCloneInStation`, for a fee and as
//! many as Infomorph Psychology allows); a ship with a clone vat bay fitted
//! carries them for others (`OfferShipCloneInstallation`, accepted by the
//! pilot asked). `CloneJump` swaps bodies: the pilot's current body, with its
//! implants, stays behind as a jump clone in the station they leave, and they
//! wake in the clone they jump to, with its implants, in a fresh capsule.
//! It can be done once a day.
//!
//! Choices the client leaves to the server (the rules of the time):
//! - NPC stations take clones from pilots their owner corporation stands at
//!   `game.jumpCloneMinStanding` or better (8.0); stations of the pilot's own
//!   corporation or alliance take them from anyone;
//! - one clone per pilot per station;
//! - a clone left behind replaces one already at the station only when the
//!   pilot agrees (JumpCheckWillLoseExistingClone[AndImplants]).

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, StoreError, Tx};
use crate::svc::rows::{key_val, rowset};
use crate::world::Move;

const CLONE_HEADER: &[&str] = &["jumpCloneID", "locationID"];
const IMPLANT_HEADER: &[&str] = &["jumpCloneID", "typeID"];
const SHIP_CLONE_HEADER: &[&str] = &["jumpCloneID", "ownerID", "locationID"];

/// What installing a clone costs.
pub const PRICE: f64 = 900_000.0;
/// One jump a day.
const COOLDOWN_FT: i64 = 24 * 3_600 * 10_000_000;
const FLAG_IMPLANT: i64 = 89;
const FLAG_HANGAR: i64 = 4;
const SKILL_INFOMORPH_PSYCHOLOGY: i32 = 24242;
const SERVICE_CLONING: i32 = 512;
const SERVICE_JUMP_CLONE: i32 = 8_388_608;
pub const CLONE_VAT_BAYS: [i32; 2] = [23735, 24547];
const TYPE_CAPSULE: i64 = 670;
/// market_refTypes: "Jump Clone Installation Fee" is not in the table of the
/// time; the clone fee goes as "Clone Activation".
const REF_CLONE: i32 = 8;
/// Fitted modules sit in the low, medium and high slots.
const SLOT_FLAGS: (i64, i64) = (11, 34);

type Refused<T> = Result<T, Outcome>;

fn refuse<T>(text: &str) -> Refused<T> {
    Err(Outcome::custom(text))
}

fn db_err(e: StoreError) -> Outcome {
    tracing::error!("jump clones: {}", crate::logging::chain(&e));
    Outcome::custom("The clone bay could not do that just now.")
}

/// A refusal inside a store write: returned from it, it undoes what the
/// write did.
struct Abort(Outcome);

impl From<Outcome> for Abort {
    fn from(o: Outcome) -> Self {
        Abort(o)
    }
}

impl From<StoreError> for Abort {
    fn from(e: StoreError) -> Self {
        Abort(db_err(e))
    }
}

fn abort<T>(text: &str) -> Result<T, Abort> {
    Err(Abort(Outcome::custom(text)))
}

pub async fn jump_clone_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let Some(me) = ctx.session.char_id else {
        return Some(Outcome::custom("Select a character first."));
    };
    let result = match call.method.as_str() {
        "GetCloneState" => clone_state(ctx, me).await,
        "GetShipCloneState" => Ok(clones_at(ctx, ctx.session.ship_id).await),
        "GetStationCloneState" => Ok(clones_at(ctx, ctx.session.station_id).await),
        "GetPriceForClone" => Ok(Value::Double(PRICE)),
        "InstallCloneInStation" => install(ctx, me).await,
        "DestroyInstalledClone" => destroy(ctx, me, call.arg_id(0)).await,
        "CloneJump" => jump(ctx, me, call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await,
        "OfferShipCloneInstallation" => offer(ctx, me, call.arg_id(0)).await,
        "AcceptShipCloneInstallation" => accept(ctx, me).await,
        "CancelShipCloneInstallation" => {
            cancel(ctx, me);
            Ok(Value::None)
        }
        _ => return None,
    };
    Some(result.map_or_else(|o| o, Outcome::Value))
}

/// clonejumpsvc.GetCloneState reads named members, including its cooldown.
async fn clone_state(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let (clones, implants, last) = ctx.store.read(|db| {
        let mut mine: Vec<_> = db.table("chrJumpClones").find(&["ownerID"], &[store::Value::Int(me)]).collect();
        mine.sort_by_key(|j| j.int("jumpCloneID"));
        let clones = mine.iter().map(|j| Value::List(j.line(CLONE_HEADER))).collect();
        let entity = db.table("entity");
        let mut implants: Vec<(Option<i64>, Option<i64>, Vec<Value>)> = Vec::new();
        for j in &mine {
            let Some(clone) = j.int("jumpCloneID") else { continue };
            for e in entity.find(&["locationID", "flag"], &[store::Value::Int(clone), store::Value::Int(FLAG_IMPLANT)]) {
                implants.push((Some(clone), e.int("typeID"), vec![j.cell("jumpCloneID"), e.cell("typeID")]));
            }
        }
        // ORDER BY 1, 2: NULLs last.
        implants.sort_by_key(|(clone, ty, _)| (clone.is_none(), *clone, ty.is_none(), *ty));
        let implants = implants.into_iter().map(|(_, _, line)| Value::List(line)).collect();
        let last = db.table("character_").get_id(me).and_then(|r| r.int("lastCloneJump")).unwrap_or(0);
        (clones, implants, last)
    });
    Ok(key_val(vec![
        ("clones", rowset(CLONE_HEADER, clones)),
        ("implants", rowset(IMPLANT_HEADER, implants)),
        ("timeLastJump", Value::Long(last)),
    ]))
}

/// Everyone's clones at a station or in a ship.
async fn clones_at(ctx: &Ctx<'_>, location: Option<i64>) -> Value {
    let Some(location) = location else {
        return rowset(SHIP_CLONE_HEADER, Vec::new());
    };
    let lines = ctx.store.read(|db| {
        let mut here: Vec<_> = db.table("chrJumpClones").find(&["locationID"], &[store::Value::Int(location)]).collect();
        here.sort_by_key(|j| j.int("jumpCloneID"));
        here.iter().map(|j| Value::List(j.line(SHIP_CLONE_HEADER))).collect()
    });
    rowset(SHIP_CLONE_HEADER, lines)
}

/// How many clones a pilot may keep: a level of Infomorph Psychology each.
fn allowed(db: &Db, me: i64) -> Result<i64, StoreError> {
    Ok(crate::svc::skills::levels(db, me, &[SKILL_INFOMORPH_PSYCHOLOGY])?
        .get(&SKILL_INFOMORPH_PSYCHOLOGY)
        .copied()
        .unwrap_or(0))
}

fn count(db: &Db, me: i64) -> i64 {
    db.table("chrJumpClones").find(&["ownerID"], &[store::Value::Int(me)]).count() as i64
}

/// The pilot, still where the session has them and in the ship it has them
/// in: when they last clone jumped.
fn pilot_here(db: &Db, ctx: &Ctx<'_>, me: i64) -> Refused<i64> {
    let Some(pilot) = db.table("character_").get_id(me) else {
        return refuse("That character no longer exists.");
    };
    if pilot.int("stationID").unwrap_or(0) != ctx.session.station_id.unwrap_or(0)
        || pilot.int("shipID").filter(|id| *id > 0) != ctx.session.ship_id
    {
        return refuse("Your location or active ship has changed. Refresh the clone bay.");
    }
    Ok(pilot.int("lastCloneJump").unwrap_or(0))
}

/// The clone `me` has at `location`, if any.
fn clone_of(db: &Db, me: i64, location: i64) -> Option<i64> {
    db.table("chrJumpClones")
        .find(&["ownerID"], &[store::Value::Int(me)])
        .find(|j| j.int("locationID") == Some(location))
        .and_then(|j| j.int("jumpCloneID"))
}

fn invalidated(ctx: &Ctx<'_>, owner: i64, location: i64, station: bool) {
    let w = ctx.world.lock();
    w.notify(
        &w.clients_of(owner),
        "OnJumpCloneCacheInvalidated",
        Value::Tuple(Vec::new()),
    );
    let event = if station {
        "OnStationJumpCloneCacheInvalidated"
    } else {
        "OnShipJumpCloneCacheInvalidated"
    };
    let here: Vec<i64> = w
        .players()
        .iter()
        .filter(|p| p.station_id == Some(location) || p.solar_system_id == location)
        .map(|p| p.char_id)
        .flat_map(|c| w.clients_of(c))
        .collect();
    w.notify(
        &here,
        event,
        Value::Tuple(vec![Value::Int(location), Value::Int(owner)]),
    );
}

async fn install(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some(station) = ctx.session.station_id else {
        return refuse("You must be docked to install a clone.");
    };
    let (operations, masks) = crate::svc::station::service_masks();
    ctx.store
        .write(|tx| -> Result<(), Abort> {
            pilot_here(tx.db(), ctx, me)?;
            let Some(s) = tx.table("staStations").get_id(station) else {
                return abort("This station has no clone bay.");
            };
            let owner = s.int("corporationID");
            let services = s
                .int("operationID")
                .and_then(|op| operations.iter().position(|o| i64::from(*o) == op))
                .map_or(0, |i| i64::from(masks[i]));
            let standing = owner
                .and_then(|corp| tx.table("chrNPCStandings").get(&[store::Value::Int(me), store::Value::Int(corp)]))
                .and_then(|n| n.float("standing"))
                .unwrap_or(0.0);
            let owner_alliance =
                owner.and_then(|corp| tx.table("corporation").get_id(corp)).and_then(|co| co.int("allianceID")).unwrap_or(0);
            let owner = owner.unwrap_or(0);
            let ours = Some(owner) == ctx.session.corp_id
                || (owner_alliance != 0 && Some(owner_alliance) == ctx.session.alliance_id);
            if services & (SERVICE_CLONING | SERVICE_JUMP_CLONE) as i64 == 0 {
                return abort("This station has no clone bay.");
            }
            let need = crate::config::jump_clone_min_standing();
            if !ours && standing < need {
                return abort(&format!("The station's owners install clones only for pilots they stand at {need:.1} or better towards; they stand at {standing:.1} towards you."));
            }
            if clone_of(tx.db(), me, station).is_some() {
                return abort("You already have a clone at this station.");
            }
            let (have, may) = (count(tx.db(), me), allowed(tx.db(), me)?);
            if have >= may {
                return abort(&format!(
                    "You have {have} jump clone(s), as many as your Infomorph Psychology allows."
                ));
            }
            pay(tx, me, PRICE)?;
            tx.insert(
                "chrJumpClones",
                &[
                    ("ownerID", store::Value::Int(me)),
                    ("locationID", store::Value::Int(station)),
                    ("createdAt", store::Value::Int(crate::bulkdata::filetime_now())),
                ],
            )?;
            Ok(())
        })
        .map_err(|Abort(o)| o)?;
    super::wallet::notify_balance(ctx, me).await;
    info!("{me} installed a jump clone at station {station}");
    invalidated(ctx, me, station, true);
    Ok(Value::None)
}

fn pay(tx: &mut Tx<'_>, me: i64, amount: f64) -> Refused<()> {
    use crate::svc::wallet::{post, Entry, Transfer};
    let entry = Entry {
        payer: me,
        payee: 0,
        ref_type: REF_CLONE,
        arg_id1: "",
        reason: "Jump clone installation",
    };
    match post(tx, me, -amount, &entry) {
        Ok(_) => Ok(()),
        Err(Transfer::NotEnough { need, have }) => Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => refuse("No wallet."),
        Err(Transfer::Db(e)) => Err(db_err(e)),
    }
}

/// The items at `locations`, gone with their stored attributes.
fn remove_items_at(tx: &mut Tx<'_>, locations: &[i64]) -> Result<(), StoreError> {
    for location in locations {
        let items: Vec<(store::RowId, Option<i64>)> = tx
            .table("entity")
            .find(&["locationID"], &[store::Value::Int(*location)])
            .map(|e| (e.id, e.int("itemID")))
            .collect();
        for (row, item) in items {
            tx.delete("entity", row)?;
            if let Some(item) = item {
                tx.delete_where("entity_attributes", &["itemID"], &[store::Value::Int(item)])?;
            }
        }
    }
    Ok(())
}

/// A clone and its implants, gone.
fn remove(tx: &mut Tx<'_>, clone: i64) -> Result<(), StoreError> {
    remove_items_at(tx, &[clone])?;
    tx.delete_where("chrJumpClones", &["jumpCloneID"], &[store::Value::Int(clone)])?;
    Ok(())
}

/// Jump clones carried in a ship, gone with it.
pub fn ship_destroyed(tx: &mut Tx<'_>, ship: i64) -> Result<u64, StoreError> {
    let clones: Vec<i64> = tx
        .table("chrJumpClones")
        .find(&["locationID"], &[store::Value::Int(ship)])
        .filter_map(|j| j.int("jumpCloneID"))
        .collect();
    remove_items_at(tx, &clones)?;
    Ok(tx.delete_where("chrJumpClones", &["locationID"], &[store::Value::Int(ship)])? as u64)
}

/// DestroyInstalledClone(cloneID): one's own clone where one is, or anyone's
/// in the ship one flies.
async fn destroy(ctx: &Ctx<'_>, me: i64, clone: i64) -> Refused<Value> {
    let row = ctx.store.read(|db| {
        db.table("chrJumpClones").get_id(clone).map(|r| (r.int("ownerID").unwrap_or(0), r.int("locationID").unwrap_or(0)))
    });
    let Some((owner, location)) = row else {
        return refuse("That clone no longer exists.");
    };
    let in_my_ship = ctx
        .store
        .write(|tx| -> Result<bool, Abort> {
            let in_my_ship = ctx.session.ship_id == Some(location)
                && tx.table("entity").get_id(location).is_some_and(|e| e.int("ownerID") == Some(me))
                && tx.table("character_").get_id(me).is_some_and(|c| c.int("shipID") == Some(location));
            if owner != me && !in_my_ship {
                return abort("You can only destroy your own clone, or clones in your current ship.");
            }
            let still = tx
                .table("chrJumpClones")
                .get_id(clone)
                .is_some_and(|j| j.int("ownerID") == Some(owner) && j.int("locationID") == Some(location));
            if !still {
                return abort("That clone no longer exists.");
            }
            remove(tx, clone)?;
            Ok(in_my_ship)
        })
        .map_err(|Abort(o)| o)?;
    info!("{me} destroyed jump clone {clone} of {owner} at {location}");
    invalidated(ctx, owner, location, !in_my_ship);
    Ok(Value::None)
}

/// Where a clone jump lands: (station, system, constellation, region, at).
type Landing = (Option<i64>, i64, i64, i64, crate::ballpark::Vec3);

/// CloneJump(destLocationID, force).
async fn jump(ctx: &Ctx<'_>, me: i64, dest: i64, force: bool) -> Refused<Value> {
    let Some(here) = ctx.session.station_id else {
        return refuse("You must be docked to clone jump.");
    };
    if dest == here {
        return refuse("You are already at that clone.");
    }
    let now = crate::bulkdata::filetime_now();
    let ((station, system, constellation, region, _), capsule, live) = ctx
        .store
        .write(|tx| -> Result<(Landing, i64, Option<crate::ballpark::Ship>), Abort> {
            let last = pilot_here(tx.db(), ctx, me)?;
            let next = last.saturating_add(COOLDOWN_FT);
            if next > now {
                return abort(&format!("Your body has not recovered from your last clone jump: you can jump again in {:.1} hours.",(next-now) as f64/36_000_000_000.0));
            }
            let Some(target) = clone_of(tx.db(), me, dest) else {
                return abort("You have no clone there.");
            };
            let landing = landing(tx.db(), ctx, dest, force)?;
            let (station, system, constellation, region, at) = landing;
            let still = tx
                .table("chrJumpClones")
                .get_id(target)
                .is_some_and(|j| j.int("ownerID") == Some(me) && j.int("locationID") == Some(dest));
            if !still {
                return abort("That clone is no longer available.");
            }
            if let Some(old) = clone_of(tx.db(), me, here) {
                if !force {
                    let implants = tx.table("entity").find(&["locationID"], &[store::Value::Int(old)]).count();
                    let msg = if implants > 0 {
                        "JumpCheckWillLoseExistingCloneAndImplants"
                    } else {
                        "JumpCheckWillLoseExistingClone"
                    };
                    return Err(Abort(Outcome::UserError {
                        msg: msg.into(),
                        args: vec![],
                    }));
                }
                remove(tx, old)?;
            }
            let left = tx.insert(
                "chrJumpClones",
                &[("ownerID", store::Value::Int(me)), ("locationID", store::Value::Int(here)), ("createdAt", store::Value::Int(now))],
            )?;
            let left = tx.table("chrJumpClones").row(left).and_then(|r| r.int("jumpCloneID")).unwrap_or(0);
            // The body left behind keeps its implants; the one woken in
            // brings its own.
            move_implants(tx, me, me, left)?;
            move_implants(tx, me, target, me)?;
            tx.delete_where("chrJumpClones", &["jumpCloneID"], &[store::Value::Int(target)])?;
            let location = station.unwrap_or(system);
            let flag = if station.is_some() { FLAG_HANGAR } else { 0 };
            let capsule = tx.nextval("dyn_id_seq")?;
            tx.insert(
                "entity",
                &[
                    ("itemID", store::Value::Int(capsule)),
                    ("itemName", store::Value::Text("".into())),
                    ("typeID", store::Value::Int(TYPE_CAPSULE)),
                    ("ownerID", store::Value::Int(me)),
                    ("locationID", store::Value::Int(location)),
                    ("flag", store::Value::Int(flag)),
                    ("contraband", store::Value::Int(0)),
                    ("singleton", store::Value::Int(1)),
                    ("quantity", store::Value::Int(1)),
                    ("x", store::Value::Float(at.x)),
                    ("y", store::Value::Float(at.y)),
                    ("z", store::Value::Float(at.z)),
                ],
            )?;
            if let Some(pilot) = tx.table("character_").get_id(me).map(|r| r.id) {
                tx.update(
                    "character_",
                    pilot,
                    &[
                        ("stationID", store::Value::Int(station.unwrap_or(0))),
                        ("solarSystemID", store::Value::Int(system)),
                        ("constellationID", store::Value::Int(constellation)),
                        ("regionID", store::Value::Int(region)),
                        ("shipID", store::Value::Int(capsule)),
                        ("lastCloneJump", store::Value::Int(now)),
                    ],
                )?;
            }
            if let Some(body) = tx.table("entity").get_id(me).map(|r| r.id) {
                tx.update("entity", body, &[("locationID", store::Value::Int(location))])?;
            }
            let live = if station.is_none() {
                let profile = crate::svc::inflight::profile(
                    tx.db(),
                    ctx.dogma,
                    me,
                    capsule,
                    &Default::default(),
                    crate::combat::now_ms(),
                )?
                .ok_or_else(|| Outcome::custom("The destination capsule could not be prepared."))?;
                let mut ship = crate::ballpark::Ship::new(
                    capsule,
                    ctx.client_id,
                    profile.stats,
                    profile.pilot,
                    at,
                    crate::ballpark::Vec3::ZERO,
                );
                ship.fight = Some(profile.fighter);
                Some(ship)
            } else {
                None
            };
            Ok((landing, capsule, live))
        })
        .map_err(|Abort(o)| o)?;
    if live.is_some() {
        crate::svc::space::ensure_park(ctx, system).await;
    }
    {
        let mut world = ctx.world.lock();
        if let Some(ship) = live {
            crate::svc::space::add_ship(&mut world, system, ship);
        }
        world.push_move(
            ctx.client_id,
            Move::Relocate {
                station_id: station,
                solar_system_id: system,
                constellation_id: constellation,
                region_id: region,
                ship_id: capsule,
            },
        );
    }
    info!("{me} clone jumped from station {here} to {dest}, in capsule {capsule}");
    invalidated(ctx, me, here, true);
    Ok(Value::None)
}

/// Where a jump to the clone at `dest` lands: the station, or the ship
/// carrying it (which needs a clone vat bay, and in space must be able to
/// receive the jump now: the capsule wakes beside it).
fn landing(db: &Db, ctx: &Ctx<'_>, dest: i64, force: bool) -> Result<Landing, Abort> {
    if let Some(place) = db.table("staStations").get_id(dest) {
        return Ok((
            place.int("stationID"),
            place.int("solarSystemID").unwrap_or(0),
            place.int("constellationID").unwrap_or(0),
            place.int("regionID").unwrap_or(0),
            crate::ballpark::Vec3::ZERO,
        ));
    }
    let ships = crate::sde::get().types_in_category(6);
    let place = db.table("entity").get_id(dest).filter(|e| e.int("typeID").is_some_and(|t| ships.contains(&(t as i32)))).and_then(|e| {
        let location = e.int("locationID");
        let st = location.and_then(|l| db.table("staStations").get_id(l));
        let m = st.and_then(|st| st.int("solarSystemID")).or(location).and_then(|s| db.table("mapSolarSystems").get_id(s))?;
        Some((st.and_then(|st| st.int("stationID")), m.int("solarSystemID"), m.int("constellationID"), m.int("regionID")))
    });
    let Some((station, system, constellation, region)) = place else {
        return abort("The ship carrying your clone is no longer available.");
    };
    if !has_module(db, dest, &CLONE_VAT_BAYS)? {
        return abort("The ship carrying your clone has no clone vat bay.");
    }
    if !force {
        return Err(Abort(Outcome::UserError {
            msg: "JumpCheckIntoShip".into(),
            args: vec![],
        }));
    }
    let station = station.filter(|id| *id > 0);
    let system = system.unwrap_or(0);
    let at = if station.is_none() {
        let mut world = ctx.world.lock();
        let Some(park) = world.parks.get_mut(&system) else {
            return abort("The ship carrying your clone is not available in space.");
        };
        park.advance(crate::destiny::tick_now());
        let Some(carrier) = park
            .ships
            .get(&dest)
            .filter(|ship| ship.is_pilot() && !ship.warping() && !ship.cloaked)
        else {
            return abort("The ship carrying your clone cannot receive a clone jump now.");
        };
        carrier.pos + crate::ballpark::Vec3::new(carrier.stats.radius + 2_500.0, 0.0, 0.0)
    } else {
        crate::ballpark::Vec3::ZERO
    };
    Ok((station, system, constellation.unwrap_or(0), region.unwrap_or(0), at))
}

/// Moves `owner`'s implants at `from` to `to`.
fn move_implants(tx: &mut Tx<'_>, owner: i64, from: i64, to: i64) -> Result<(), StoreError> {
    let rows: Vec<store::RowId> = tx
        .table("entity")
        .find(&["locationID", "flag", "ownerID"], &[store::Value::Int(from), store::Value::Int(FLAG_IMPLANT), store::Value::Int(owner)])
        .map(|e| e.id)
        .collect();
    for row in rows {
        tx.update("entity", row, &[("locationID", store::Value::Int(to))])?;
    }
    Ok(())
}

/// Whether a ship has one of `types` fitted.
pub fn has_module(db: &Db, ship: i64, types: &[i32]) -> Result<bool, StoreError> {
    Ok(db.table("entity").find(&["locationID"], &[store::Value::Int(ship)]).any(|e| {
        e.int("typeID").is_some_and(|t| types.contains(&(t as i32)))
            && e.int("flag").is_some_and(|f| (SLOT_FLAGS.0..=SLOT_FLAGS.1).contains(&f))
    }))
}

fn offer_args(offerer: i64, target: i64, ship: i64) -> Value {
    Value::Tuple(vec![Value::Tuple(vec![
        Value::Int(offerer),
        Value::Int(target),
        Value::Int(ship),
        Value::None,
    ])])
}

/// OfferShipCloneInstallation(charID): a pilot whose ship has a clone vat
/// bay offers another pilot in the same system a clone in it.
async fn offer(ctx: &Ctx<'_>, me: i64, target: i64) -> Refused<Value> {
    let Some(ship) = ctx.session.ship_id else {
        return refuse("You are not in a ship.");
    };
    if !ctx.store.read(|db| has_module(db, ship, &CLONE_VAT_BAYS)).map_err(db_err)? {
        return refuse("Your ship has no clone vat bay.");
    }
    let mut w = ctx.world.lock();
    let Some(there) = w.presence_of(target) else {
        return refuse("That pilot is not online.");
    };
    if Some(there.solar_system_id) != ctx.session.solar_system_id2.or(ctx.session.solar_system_id) {
        return refuse("That pilot is not in this system.");
    }
    w.clone_offers.insert(target, (me, ship));
    let clients = w.clients_of(target);
    w.notify(
        &clients,
        "OnShipJumpCloneInstallationOffered",
        offer_args(me, target, ship),
    );
    info!("{me} offered {target} a clone in ship {ship}");
    Ok(Value::None)
}

/// AcceptShipCloneInstallation(): the pilot asked takes the clone, and pays.
async fn accept(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some((offerer, ship)) = ctx.world.lock().clone_offers.remove(&me) else {
        return refuse("Nobody is offering you a clone.");
    };
    let done = ctx.store.write(|tx| -> Result<(), Abort> {
        pilot_here(tx.db(), ctx, me)?;
        // The ship, still the offering pilot's own and still flown by them.
        let carrier = tx
            .table("entity")
            .get_id(ship)
            .filter(|e| e.int("ownerID") == Some(offerer))
            .filter(|_| tx.table("character_").get_id(offerer).is_some_and(|c| c.int("shipID") == Some(ship)))
            .map(|e| {
                let location = e.int("locationID");
                location.and_then(|l| tx.table("staStations").get_id(l)).and_then(|st| st.int("solarSystemID")).or(location)
            });
        let Some(system) = carrier else { return abort("The offering pilot no longer controls that ship."); };
        if system.is_none() || system != ctx.session.solar_system_id2.or(ctx.session.solar_system_id) || !has_module(tx.db(), ship, &CLONE_VAT_BAYS)? {
            return abort("The offered clone bay is no longer available in this system.");
        }
        let online=ctx.world.lock().presence_of(offerer).is_some_and(|p|Some(p.solar_system_id)==ctx.session.solar_system_id2.or(ctx.session.solar_system_id));
        if !online {return abort("The offering pilot is no longer available.");}
        let (have, may) = (count(tx.db(), me), allowed(tx.db(), me)?);
        if have >= may {
            return abort(&format!("You have {have} jump clone(s), as many as your Infomorph Psychology allows."));
        }
        if clone_of(tx.db(), me, ship).is_some() {
            return abort("You already have a clone in that ship.");
        }
        pay(tx, me, PRICE)?;
        tx.insert(
            "chrJumpClones",
            &[
                ("ownerID", store::Value::Int(me)),
                ("locationID", store::Value::Int(ship)),
                ("createdAt", store::Value::Int(crate::bulkdata::filetime_now())),
            ],
        )?;
        Ok(())
    });
    let done = match done {
        Ok(()) => {
            super::wallet::notify_balance(ctx, me).await;
            Ok(())
        }
        Err(Abort(why)) => Err(why),
    };
    let w = ctx.world.lock();
    let both: Vec<i64> = w
        .clients_of(me)
        .into_iter()
        .chain(w.clients_of(offerer))
        .collect();
    match done {
        Ok(()) => {
            w.notify(
                &both,
                "OnShipJumpCloneInstallationDone",
                offer_args(offerer, me, ship),
            );
            w.notify(
                &w.clients_of(offerer),
                "OnShipJumpCloneCacheInvalidated",
                Value::Tuple(vec![Value::Int(ship), Value::Int(me)]),
            );
            w.notify(
                &w.clients_of(me),
                "OnJumpCloneCacheInvalidated",
                Value::Tuple(Vec::new()),
            );
            info!("{me} has a jump clone in {offerer}'s ship {ship}");
            Ok(Value::None)
        }
        Err(why) => {
            w.notify(
                &w.clients_of(offerer),
                "OnShipJumpCloneInstallationCanceled",
                offer_args(offerer, me, ship),
            );
            Err(why)
        }
    }
}

/// CancelShipCloneInstallation(): whichever side calls it, the offer is off.
fn cancel(ctx: &Ctx<'_>, me: i64) {
    let mut w = ctx.world.lock();
    let mine: Vec<(i64, i64, i64)> = w
        .clone_offers
        .iter()
        .filter(|(t, (o, _))| **t == me || *o == me)
        .map(|(t, (o, s))| (*o, *t, *s))
        .collect();
    for (offerer, target, ship) in mine {
        w.clone_offers.remove(&target);
        let other = if offerer == me { target } else { offerer };
        let clients = w.clients_of(other);
        w.notify(
            &clients,
            "OnShipJumpCloneInstallationCanceled",
            offer_args(offerer, target, ship),
        );
    }
}
