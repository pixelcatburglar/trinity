//! Corporation jump arrays. Wire shapes come from moonmining.py and
//! menusvc.dis::JumpPortalBridgeMenu/BridgePortals/JumpThroughPortal.
use super::jumps;
use crate::{
    ballpark::Vec3,
    services::{Ctx, Outcome},
    session::CORP_ROLE_DIRECTOR,
    store::{self, Db, StoreError, Tx},
};
use eve_marshal::Value;

const GROUP_ARRAY: i64 = 707;
const STARBASE_CONFIG: i64 = 1 << 53;

/// The links are kept in `jumpBridgeLinks`, which the store's schema defines.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// A type's value of one attribute in dgmTypeAttribs, which keeps its
/// values as text (every one of them a number, as `value::float8` read it).
fn type_attrib(type_id: i64, attribute: i64) -> Option<f64> {
    crate::sde::table("dgmTypeAttribs")
        .find("typeID", type_id)
        .find(|r| r.int("attributeID") == Some(attribute))
        .and_then(|r| r.text("value")?.trim().parse().ok())
}

/// The corporation's structures of `types` that are starbase structures in a
/// solar system, by system and then item.
fn structures<'d>(db: &'d Db, corp: i64, types: &[i32]) -> Vec<store::Row<'d>> {
    let mut rows: Vec<store::Row<'d>> = db
        .table("entity")
        .find(&["ownerID"], &[store::Value::Int(corp)])
        .filter(|e| e.int("typeID").is_some_and(|t| types.contains(&(t as i32))))
        .filter(|e| e.int("itemID").is_some_and(|id| db.table("posStructures").get_id(id).is_some()))
        .filter(|e| e.int("locationID").is_some_and(|s| db.table("mapSolarSystems").get_id(s).is_some()))
        .collect();
    rows.sort_by_key(|e| (e.int("locationID"), e.int("itemID")));
    rows
}

pub async fn towers(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    ctx.store.read(|db| {
        Value::List(
            structures(db, corp, crate::sde::get().types_in_group(365))
                .iter()
                .map(|row| Value::Tuple(row.line(&["typeID", "itemID", "locationID"])))
                .collect(),
        )
    })
}

/// A jump array: its system, its ID, and the (system, ID, type) of the
/// array it is linked to.
struct Array {
    system: i64,
    id: i64,
    link: Option<(i64, i64, i64)>,
}

fn arrays(db: &Db, corp: i64) -> Vec<Array> {
    structures(db, corp, crate::sde::get().types_in_group(GROUP_ARRAY))
        .iter()
        .map(|e| {
            let id = e.int("itemID").unwrap_or(0);
            let link = db
                .table("jumpBridgeLinks")
                .get_id(id)
                .and_then(|l| db.table("entity").get_id(l.int("destinationID")?))
                .and_then(|remote| Some((remote.int("locationID")?, remote.int("itemID")?, remote.int("typeID")?)));
            Array { system: e.int("locationID").unwrap_or(0), id, link }
        })
        .collect()
}

pub async fn list(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    let arrays = ctx.store.read(|db| arrays(db, corp));
    Value::List(
        arrays
            .iter()
            .map(|array| {
                let link = match array.link {
                    Some((system, structure, type_id)) => vec![(
                        Value::Int(array.id),
                        Value::Tuple(vec![
                            Value::Int(system),
                            Value::Int(structure),
                            Value::Int(type_id),
                        ]),
                    )],
                    None => Vec::new(),
                };
                Value::Tuple(vec![Value::Int(array.system), Value::Dict(link)])
            })
            .collect(),
    )
}

pub async fn linkable(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    let arrays = ctx.store.read(|db| arrays(db, corp));
    Value::List(
        arrays
            .iter()
            .filter(|array| array.link.is_none())
            .map(|array| Value::Tuple(vec![Value::Int(array.system), Value::Int(array.id)]))
            .collect(),
    )
}

fn may_configure(ctx: &Ctx<'_>) -> Result<(i64, i64), String> {
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) && !ctx.session.has_corp_role(STARBASE_CONFIG)
    {
        return Err("You need the Starbase Configuration role.".into());
    }
    let corp = ctx.session.corp_id.ok_or("You have no corporation.")?;
    let system = ctx
        .session
        .solar_system_id
        .ok_or("You must be in space to configure a jump bridge.")?;
    Ok((corp, system))
}

/// Why a bridge change was refused, inside its store write: what the pilot
/// is told.
struct Refused(String);

impl From<StoreError> for Refused {
    fn from(error: StoreError) -> Self {
        Refused(db_error(error))
    }
}

impl From<&str> for Refused {
    fn from(reason: &str) -> Self {
        Refused(reason.into())
    }
}

pub async fn install(ctx: &Ctx<'_>, local: i64, remote_system: i64, remote: i64) -> Outcome {
    let result = install_inner(ctx, local, remote_system, remote).await;
    match result {
        Ok(()) => {
            refresh(ctx, local, remote).await;
            Outcome::Value(Value::None)
        }
        Err(reason) => Outcome::custom(reason),
    }
}

async fn install_inner(
    ctx: &Ctx<'_>,
    local: i64,
    remote_system: i64,
    remote: i64,
) -> Result<(), String> {
    let (corp, system) = may_configure(ctx)?;
    if local == remote || system == remote_system {
        return Err("A jump bridge must connect different systems.".into());
    }
    // Read before the write (it reads the store itself); only refused, if
    // the systems are not found, where the checks below come to it.
    let distance = jumps::light_years(ctx, system, remote_system).await;
    ctx.store
        .write(|tx| -> Result<(), Refused> {
            let db = tx.db();
            let types = crate::sde::get().types_in_group(GROUP_ARRAY);
            // (owner, system, state, type) of an array.
            let array = |id: i64| {
                let e = db.table("entity").get_id(id).filter(|e| e.int("typeID").is_some_and(|t| types.contains(&(t as i32))))?;
                let p = db.table("posStructures").get_id(id)?;
                Some((e.int("ownerID"), e.int("locationID"), p.int("state"), e.int("typeID")))
            };
            let a = array(local).ok_or("The local jump array does not exist.")?;
            let b = array(remote).ok_or("The destination jump array does not exist.")?;
            if a.0 != Some(corp) || b.0 != Some(corp) {
                return Err("Both jump arrays must belong to your corporation.".into());
            }
            if a.1 != Some(system) || b.1 != Some(remote_system) {
                return Err("Those jump arrays are not in the specified systems.".into());
            }
            if [a, b].iter().any(|row| !matches!(row.2, Some(1 | 4 | 5))) {
                return Err("Both jump arrays must be anchored before linking them.".into());
            }
            let distance = distance.ok_or("Those systems could not be found.")?;
            let range = |row: &(Option<i64>, Option<i64>, Option<i64>, Option<i64>)| row.3.and_then(|t| type_attrib(t, 867)).unwrap_or(0.0);
            if distance > range(&a).min(range(&b)) {
                return Err("The destination is beyond the jump arrays' range.".into());
            }
            let links = db.table("jumpBridgeLinks");
            let linked = [local, remote].iter().any(|id| {
                links.get_id(*id).is_some() || links.find_one(&["destinationID"], &[store::Value::Int(*id)]).is_some()
            });
            if linked {
                return Err("Unlink the existing bridge before making another link.".into());
            }
            for (from, to) in [(local, remote), (remote, local)] {
                tx.insert(
                    "jumpBridgeLinks",
                    &[("sourceID", store::Value::Int(from)), ("destinationID", store::Value::Int(to))],
                )?;
            }
            Ok(())
        })
        .map_err(|Refused(reason)| reason)
}

pub async fn uninstall(ctx: &Ctx<'_>, local: i64) -> Outcome {
    let result: Result<i64, String> = may_configure(ctx).and_then(|(corp, system)| {
        ctx.store
            .write(|tx| -> Result<i64, Refused> {
                let db = tx.db();
                let local_row = db
                    .table("entity")
                    .get_id(local)
                    .filter(|e| e.int("ownerID") == Some(corp) && e.int("locationID") == Some(system))
                    .filter(|_| db.table("posStructures").get_id(local).is_some());
                if local_row.is_none() {
                    return Err("That is not your corporation's local jump array.".into());
                }
                let links = db.table("jumpBridgeLinks");
                let mut ids: Vec<store::RowId> = links.get_id(local).map(|l| l.id).into_iter().collect();
                ids.extend(links.find(&["destinationID"], &[store::Value::Int(local)]).map(|l| l.id));
                ids.dedup();
                let peers: Vec<i64> = ids.iter().filter_map(|id| links.row(*id)?.int("destinationID")).collect();
                for id in ids {
                    tx.delete("jumpBridgeLinks", id)?;
                }
                Ok(peers.into_iter().find(|id| *id != local).unwrap_or(0))
            })
            .map_err(|Refused(reason)| reason)
    });
    match result {
        Ok(remote) => {
            refresh(ctx, local, remote).await;
            Outcome::Value(Value::None)
        }
        Err(reason) => Outcome::custom(reason),
    }
}

async fn refresh(ctx: &Ctx<'_>, local: i64, remote: i64) {
    // Each structure's (ID, system) and the (ID, system, type) it links to.
    let rows: Vec<((i64, i64), Option<(i64, i64, i64)>)> = ctx.store.read(|db| {
        let entity = db.table("entity");
        let ids = if local == remote { vec![local] } else { vec![local, remote] };
        ids.into_iter()
            .filter_map(|id| {
                let e = entity.get_id(id)?;
                let other = db
                    .table("jumpBridgeLinks")
                    .get_id(id)
                    .and_then(|l| entity.get_id(l.int("destinationID")?))
                    .and_then(|o| Some((o.int("itemID")?, o.int("locationID")?, o.int("typeID")?)));
                Some(((e.int("itemID")?, e.int("locationID")?), other))
            })
            .collect()
    });
    let mut world = ctx.world.lock();
    for ((id, system), other) in rows {
        let entry = world.parks.get_mut(&system).and_then(|park| {
            let structure = park.structures.get_mut(&id)?;
            structure.bridge = other;
            Some(crate::pos::slim_change(park.tick, structure))
        });
        if let Some(entry) = entry {
            super::space::broadcast(&world, system, None, vec![entry]);
        }
    }
}

fn db_error(error: StoreError) -> String {
    tracing::error!("jump bridge: {}", crate::logging::chain(&error));
    "The jump bridge operation could not be completed.".into()
}

/// Validate both endpoints and quote the departure array's fuel. Arrival
/// commits the debit together with the pilot and ship's destination.
pub async fn prepare(ctx: &Ctx<'_>, local: i64, remote: i64, to: i64) -> Result<jumps::PreparedJump, String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Err("You are not in space.".into());
    };
    let mass = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system).ok_or("You are not in space.")?;
        let ship = park.ships.get(&ship_id).ok_or("You are not in space.")?;
        if ship.warping() || ship.cloaked {
            return Err("You cannot jump while cloaked or warping.".into());
        }
        let array = park
            .structures
            .get(&local)
            .ok_or("The jump array is not here.")?;
        if (ship.pos - array.pos).len() - ship.stats.radius - array.radius > jumps::BRIDGE_RANGE {
            return Err("Move within 5 km of the jump array.".into());
        }
        if ship.pilot.corp_id != array.corp_id
            && (ship.pilot.alliance_id.is_none() || ship.pilot.alliance_id != array.alliance_id)
        {
            return Err(
                "You are not a member of this jump array's corporation or alliance.".into(),
            );
        }
        ship.stats.mass
    };
    // The far array's position and type, and both arrays' states.
    let link = ctx.store.read(|db| {
        let entity = db.table("entity");
        let structures = db.table("posStructures");
        db.table("jumpBridgeLinks").get_id(local).filter(|l| l.int("destinationID") == Some(remote))?;
        entity.get_id(local).filter(|a| a.int("locationID") == Some(system))?;
        let b = entity.get_id(remote).filter(|b| b.int("locationID") == Some(to))?;
        let (p, q) = (structures.get_id(local)?, structures.get_id(remote)?);
        Some((
            Vec3::new(b.float("x").unwrap_or(0.0), b.float("y").unwrap_or(0.0), b.float("z").unwrap_or(0.0)),
            b.int("typeID").unwrap_or(0),
            p.int("state"),
            q.int("state"),
        ))
    });
    let (destination, array_type, near, far) = link.ok_or("That jump bridge link no longer exists.")?;
    if !matches!(near, Some(4 | 5)) || !matches!(far, Some(4 | 5)) {
        return Err("Both jump arrays must be online.".into());
    }
    let attr = |id: i64| type_attrib(array_type, id).unwrap_or(0.0);
    let fuel = attr(866) as i32;
    let distance = jumps::light_years(ctx, system, to)
        .await
        .ok_or("Those systems could not be found.")?;
    // menusvc's tooltip gives these four factors. The original server's
    // rounding expression is not in the dump; ceiling follows this server's
    // capital-drive payment rule and remains a historical-parity limitation.
    let quantity = (distance * attr(868) * attr(1001) * mass).ceil().max(1.0) as i64;
    if fuel <= 0 {
        return Err("The jump array's fuel type is missing.".into());
    }
    let fuel=jumps::FuelCost {cargo:local,type_id:i64::from(fuel),quantity,flag:None};
    fuel.check(ctx).await?;
    Ok(jumps::PreparedJump {at:jumps::landing(destination),fuel,capacitor:0.0,clients:vec![ctx.client_id]})
}
