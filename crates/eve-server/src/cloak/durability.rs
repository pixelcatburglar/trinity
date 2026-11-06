//! Preserve observed cooldown deadlines independently of a ship's trajectory.
//!
//! Trinity exposes delay durations and refusal messages, but the supplied
//! client does not establish whether the original server reset them on docking.
//! Keeping an already observed deadline is this server's continuity policy;
//! this is not a new original dogma attribute or a reconstructed timer formula.
use super::History;
use crate::store::{Db, Row, StoreError, Tx, Value};
use crate::{ballpark::Ship, items::Item};

/// The `cloakCooldowns` table and the entity trigger that drops a binding
/// whose custody changed are part of the store's schema.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

fn of(types:&[i32],ty:Option<i64>)->bool {
    ty.is_some_and(|ty|types.iter().any(|t|i64::from(*t)==ty))
}

/// One assembled, unstacked item (`quantity=1 AND singleton<>0`).
fn assembled(e:&Row)->bool {
    e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0)
}

fn fitted(e:&Row)->bool {
    e.int("flag").is_some_and(|f|(11..=35).contains(&f))
}

/// Recheck the character's hull and fitted modules before writing their
/// deadlines. Only observed timestamps are saved; repeated saves never
/// restart a timer.
pub fn save_history(tx:&mut Tx<'_>,ship:&Ship,now:i64)->Result<(),StoreError> {
    let Some(fighter)=&ship.fight else{return Ok(());};
    let mut history=fighter.cloak_history.clone();history.prune(now);
    let owner=ship.pilot.char_id;
    let hull=ship.id;
    let ty=ship.pilot.type_id;
    let sde=crate::sde::get();
    let entity=tx.table("entity");
    let hull_held=entity.get_id(hull).is_some_and(|e|e.int("ownerID")==Some(owner) && e.int("typeID")==Some(ty)
        && assembled(&e) && of(sde.types_in_category(6),e.int("typeID")));
    if !hull_held {return Ok(());}
    let expired:Vec<_>=tx.table("cloakCooldowns").find(&["shipID"],&[Value::Int(hull)])
        .filter(|c|c.int("targetingAt").is_some_and(|t|t<=now) && c.int("readyAt").is_some_and(|r|r<=now))
        .map(|c|c.id).collect();
    for id in expired {tx.delete("cloakCooldowns",id)?;}
    if history.targeting_at>now {
        save(tx,hull,hull,owner,ty,ty,history.targeting_at,0)?;
    }
    // The deadlines are keyed by item, so these come in itemID order.
    let entity=tx.table("entity");
    let modules:Vec<(i64,i64)>=history.module_ready.keys().take(25)
        .filter_map(|id|entity.get_id(*id))
        .filter(|e|e.int("ownerID")==Some(owner) && e.int("locationID")==Some(hull) && assembled(e) && fitted(e)
            && of(sde.types_in_group(330),e.int("typeID")))
        .filter_map(|e|Some((e.int("itemID")?,e.int("typeID")?))).collect();
    for (id,module_type) in modules {
        save(tx,id,hull,owner,module_type,ty,0,history.module_ready[&id])?;
    }
    Ok(())
}

/// Write one deadline. A row still bound to the same hull, owner and types
/// keeps the later of its stored deadline (when that is a plausible time)
/// and the new one; any other binding is replaced.
#[allow(clippy::too_many_arguments)]
fn save(tx:&mut Tx<'_>,item:i64,ship:i64,owner:i64,ty:i64,ship_type:i64,targeting:i64,ready:i64)->Result<(),StoreError> {
    let latest=(i64::MAX/10_000)-11_644_473_600_000;
    let kept=|stored:Option<i64>,new:i64|stored.filter(|v|(0..=latest).contains(v)).unwrap_or(0).max(new);
    let existing=tx.table("cloakCooldowns").get_id(item).map(|c|{
        let same=c.int("shipID")==Some(ship) && c.int("ownerID")==Some(owner) && c.int("typeID")==Some(ty) && c.int("shipTypeID")==Some(ship_type);
        let targeting=if same {kept(c.int("targetingAt"),targeting)} else {targeting};
        let ready=if same {kept(c.int("readyAt"),ready)} else {ready};
        (c.id,targeting,ready)
    });
    match existing {
        Some((row,targeting,ready))=>tx.update("cloakCooldowns",row,&[
            ("shipID",Value::Int(ship)),("ownerID",Value::Int(owner)),("typeID",Value::Int(ty)),("shipTypeID",Value::Int(ship_type)),
            ("targetingAt",Value::Int(targeting)),("readyAt",Value::Int(ready))]),
        None=>tx.insert("cloakCooldowns",&[
            ("itemID",Value::Int(item)),("shipID",Value::Int(ship)),("ownerID",Value::Int(owner)),("typeID",Value::Int(ty)),
            ("shipTypeID",Value::Int(ship_type)),("targetingAt",Value::Int(targeting)),("readyAt",Value::Int(ready))]).map(|_|()),
    }
}

/// Reads deadlines only. It cannot restore active effects or native movement.
/// Each row must still describe the same assembled hull and fitted module.
pub fn load_history(db:&Db,owner:i64,ship:i64,now:i64)->Result<History,StoreError> {
    // The hull's own row is the hull, a ship type; the others are fitted
    // cloaking devices (group 330).
    let sde=crate::sde::get();
    let entity=db.table("entity");
    let mut rows:Vec<(i64,i64,i64)>=db.table("cloakCooldowns").find(&["shipID"],&[Value::Int(ship)])
        .filter(|c|c.int("ownerID")==Some(owner))
        .filter(|c|{
            let Some(h)=c.int("shipID").and_then(|id|entity.get_id(id)) else{return false;};
            let Some(e)=c.int("itemID").and_then(|id|entity.get_id(id)) else{return false;};
            h.int("ownerID")==c.int("ownerID") && h.int("typeID")==c.int("shipTypeID")
                && e.int("ownerID")==c.int("ownerID") && e.int("typeID")==c.int("typeID")
                && assembled(&h) && of(sde.types_in_category(6),h.int("typeID")) && assembled(&e)
                && (e.int("itemID")==h.int("itemID")
                    || (e.int("locationID")==h.int("itemID") && fitted(&e) && of(sde.types_in_group(330),e.int("typeID"))))
        })
        .filter_map(|c|Some((c.int("itemID")?,c.int("targetingAt")?,c.int("readyAt")?))).collect();
    rows.sort_by_key(|r|r.0);
    let mut history=History::default();
    for (id,targeting,ready) in rows {
        if id==ship {history.targeting_at=targeting;}
        else if history.module_ready.len()<25 {history.module_ready.insert(id,ready);}
    }
    history.prune(now);
    Ok(history)
}

/// Use after ItemTxn::item has re-read the source module. The supplied
/// custody predicates and current hull binding are rechecked here.
pub fn module_reactivation_remaining(db:&Db,item:&Item,now:i64)->Result<Option<i64>,StoreError> {
    if item.category_id!=7 || !item.singleton || item.quantity!=1 || !(11..=35).contains(&item.flag) {
        return Ok(None);
    }
    let history=load_history(db,item.owner_id,item.location_id,now)?;
    let current=db.table("entity").get_id(item.id).is_some_and(|e|e.int("typeID")==Some(item.type_id) && e.int("ownerID")==Some(item.owner_id)
        && e.int("locationID")==Some(item.location_id) && e.int("flag")==Some(item.flag) && assembled(&e));
    Ok(current.then(|| history.module_ready.get(&item.id).copied()).flatten().map(|ready|ready-now))
}
