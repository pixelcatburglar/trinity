//! A confirmed player hull loss is an obligation, not a best-effort callback.
//! Persist the original attribution before settlement; commit destruction,
//! insurance/clone consequences and the kill record together.
use super::*;

/// `pendingShipLosses` is in the store's schema.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

pub fn blocked_in_memory(world:&crate::world::World,ship:i64)->bool {
    world.lock().pending_ship_losses.contains_key(&ship)
}
pub fn blocked_character_in_memory(world:&crate::world::World,pilot:i64)->bool {
    world.lock().pending_ship_losses.values().any(|loss|loss.char_id==pilot && !loss.unpiloted)
}

pub fn valid(loss:&Loss)->bool {
    let id=|n:i64|n>0 && n<=i32::MAX as i64;
    id(loss.ship_id) && id(loss.char_id) && id(loss.ship_type) && id(loss.system) && loss.pos.is_finite()
        && loss.arrival_custody.as_ref().is_none_or(|arrival|id(arrival.system) && arrival.at.is_finite())
        && loss.capsule==(loss.ship_type==TYPE_CAPSULE)
        && loss.final_blow.is_none_or(|n|i32::try_from(n).is_ok())
        && loss.attackers.iter().all(|a|i32::try_from(a.who).is_ok() && i32::try_from(a.ship_type).is_ok()
            && i32::try_from(a.weapon_type).is_ok() && a.damage.is_finite() && a.damage>=0.0)
}

fn decode(raw:&str,ship:i64,pilot:i64,system:i64,ty:i64)->Option<Loss> {
    if raw.len()>1_048_576{return None;}
    let loss:Loss=serde_json::from_str(raw).ok()?;
    (valid(&loss) && (loss.ship_id,loss.char_id,loss.system,loss.ship_type)==(ship,pilot,system,ty)).then_some(loss)
}

/// Called in the write that holds the hull's custody. This also supports
/// checkpointing an observed lethal threshold before profile reload.
pub fn stage(tx:&mut Tx<'_>,loss:&Loss)->Result<bool,StoreError> {
    if !valid(loss){return Ok(false);}
    let Ok(raw)=serde_json::to_string(loss) else{return Ok(false);};
    if raw.len()>1_048_576{return Ok(false);}
    let ships=crate::sde::get().types_in_category(6);
    let hull=tx.table("entity").get_id(loss.ship_id).filter(|e| e.int("ownerID")==Some(loss.char_id)
        && e.int("locationID")==Some(loss.custody_system()) && e.int("typeID")==Some(loss.ship_type)
        && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0)
        && ships.contains(&(loss.ship_type as i32)));
    if hull.is_none(){return Ok(false);}
    let occupied=!loss.unpiloted && aboard(tx.db(),loss.char_id,loss.ship_id,loss.custody_system());
    // The indexed system remains the encounter's attribution. Explicit
    // destination custody is part of the same immutable serialized record.
    tx.insert_or_ignore("pendingShipLosses",&[("shipID",store::Value::Int(loss.ship_id)),("characterID",store::Value::Int(loss.char_id)),
        ("solarSystemID",store::Value::Int(loss.system)),("shipTypeID",store::Value::Int(loss.ship_type)),("occupied",store::Value::Bool(occupied)),
        ("createdAt",store::Value::Int(combat::now_ms())),("loss",store::Value::Text(raw.into()))])?;
    Ok(true)
}

fn capture(store:&Store,loss:&Loss)->Result<Option<Loss>,StoreError> {
    store.write(|tx| {
        if !stage(tx,loss)? {return Ok(None);}
        let Some(row)=tx.table("pendingShipLosses").get_id(loss.ship_id) else {return Ok(None);};
        let mut recorded=decode(row.text("loss").unwrap_or_default(),loss.ship_id,row.int("characterID").unwrap_or(0),
            row.int("solarSystemID").unwrap_or(0),row.int("shipTypeID").unwrap_or(0));
        if row.bool("occupied")!=Some(true) {if let Some(loss)=recorded.as_mut(){loss.unpiloted=true;}}
        Ok(recorded)
    })
}

pub async fn handle(server:&Server,store:&Store,loss:Loss) {
    if !valid(&loss){error!(ship=loss.ship_id,"invalid confirmed hull loss; refusing destructive settlement");return;}
    // Keep the first observed loss. Later retries cannot replace attribution
    // with a newly loaded inactive hull profile or a stale connection ID.
    let loss=server.world.lock().pending_ship_losses.entry(loss.ship_id).or_insert(loss).clone();
    let recorded=match capture(store,&loss) {
        Ok(Some(mut recorded))=>{
            recorded.client_id=server.world.lock().clients_of(recorded.char_id).first().copied().unwrap_or(0);
            recorded
        }
        Ok(None)=>{
            if store.read(|db|completed(db,&loss)) {server.world.lock().pending_ship_losses.remove(&loss.ship_id);}
            return;
        }
        Err(error)=>{error!(ship=loss.ship_id,"saving confirmed space loss: {}",crate::logging::chain(&error));return;}
    };
    if recorded.capsule && !recorded.unpiloted {podded(server,store,&recorded).await;}
    else {ship_lost(server,store,&recorded).await;}
    if store.read(|db|completed(db,&recorded)) {server.world.lock().pending_ship_losses.remove(&recorded.ship_id);}
}

fn completed(db:&Db,loss:&Loss)->bool {
    db.table("entity").get_id(loss.ship_id).is_none()
        && db.table("killLog").rows().any(|k|k.int("victimItemID")==Some(loss.ship_id) && k.int("victimCharID")==Some(loss.char_id))
}

pub async fn retry(server:&Server,store:&Store) {
    let pending:Vec<_>=server.world.lock().pending_ship_losses.values().cloned().collect();
    let ids:std::collections::BTreeSet<_>=pending.iter().map(|l|l.ship_id).collect();
    for loss in pending {handle(server,store,loss).await;}
    let cursor=i64::from(server.world.lock().pending_ship_loss_cursor);
    // The next page of recorded losses after the cursor, by hull.
    let rows:Vec<(i64,i64,i64,i64,String)>=store.read(|db| {
        let mut rows:Vec<_>=db.table("pendingShipLosses").rows().filter_map(|r| {
            let ship=r.int("shipID").filter(|ship|*ship>cursor)?;
            Some((ship,r.int("characterID").unwrap_or(0),r.int("solarSystemID").unwrap_or(0),r.int("shipTypeID").unwrap_or(0),
                r.text("loss").unwrap_or_default().to_string()))
        }).collect();
        rows.sort_by_key(|row|row.0);
        rows.truncate(64);
        rows
    });
    // Include invalid rows and memory duplicates in scan progress. A complete
    // page of quarantined losses must not monopolize every recovery attempt.
    server.world.lock().pending_ship_loss_cursor=rows.last().map(|row|row.0 as i32).unwrap_or(0);
    for (ship,pilot,system,ty,raw) in rows {
        if ids.contains(&ship){continue;}
        if let Some(loss)=decode(&raw,ship,pilot,system,ty) {
            handle(server,store,loss).await;
        }else{warn!(ship,"invalid durable space loss remains quarantined");}
    }
}
