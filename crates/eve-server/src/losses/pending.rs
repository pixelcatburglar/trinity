//! A station arrival is provisional until its last live combat snapshot is
//! durably saved or its loss is settled. The marker commits with the move.
//! A restart with only that marker cannot infer the final damage; it stays
//! quarantined instead of inventing either survival or destruction.
use super::{ArrivalDisposition,Attacker,Loss,Refused};
use crate::{ballpark::{Ship,Vec3},combat,conn::Server,svc::inflight};
use crate::store::{self,Db,Store,StoreError,Tx};

#[derive(Debug,Clone,Copy)]
pub struct Marker {pub ship:i64,pub pilot:i64,pub station:i64,pub source:i64,pub generation:i64,pub ship_type:i64}

#[derive(Debug,Clone)]
pub struct Pending {marker:Marker,ship:Ship,busy:bool,claim:u64}

fn next_claim()->u64 {
    static CLAIM:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);
    CLAIM.fetch_add(1,std::sync::atomic::Ordering::Relaxed)
}
struct Busy {world:crate::world::World,marker:Marker,claim:u64}
impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(p)=self.world.lock().pending_station_arrivals.get_mut(&self.marker.ship)
            .filter(|p|p.marker.generation==self.marker.generation && p.claim==self.claim) {p.busy=false;}
    }
}

/// `pendingStationArrivals` and its generations are in the store's schema.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

pub fn blocked(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(blocked_except_travel(db,ship)? || crate::travel::blocked(db,ship)?)
}

pub(crate) fn blocked_except_travel(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(blocked_arrival_or_loss(db,ship)? || crate::svc::jumps::portal::blocked(db,ship)?)
}

pub(crate) fn blocked_except_portal(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(blocked_arrival_or_loss(db,ship)? || crate::travel::blocked(db,ship)?)
}

fn blocked_arrival_or_loss(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(blocked_arrival_or_loss_only(db,ship) || crate::conn::logout::blocked(db,ship)?)
}

pub(crate) fn blocked_except_logout(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(blocked_arrival_or_loss_only(db,ship) || crate::travel::blocked(db,ship)?
        || crate::svc::jumps::portal::blocked(db,ship)?)
}

fn blocked_arrival_or_loss_only(db:&Db,ship:i64)->bool {
    db.table("pendingStationArrivals").get_id(ship).is_some() || db.table("pendingShipLosses").get_id(ship).is_some()
}

pub fn blocked_character(db:&Db,pilot:i64)->Result<bool,StoreError> {
    let by=|table:&str| db.table(table).find(&["characterID"],&[store::Value::Int(pilot)]);
    Ok(by("pendingStationArrivals").next().is_some()
        || by("pendingShipLosses").any(|loss|loss.bool("occupied")==Some(true))
        || crate::travel::blocked_character(db,pilot)?
        || crate::conn::logout::blocked_character(db,pilot)?
        || crate::svc::jumps::portal::blocked_character(db,pilot)?)
}

/// Call in the arrival transaction after moving the current hull.
/// A duplicate unresolved arrival is a constraint failure, not a replacement.
pub fn begin(tx:&mut Tx<'_>,pilot:i64,ship:i64,station:i64,source:i64)->Result<Marker,StoreError> {
    let hull=tx.table("entity").get_id(ship).filter(|e|e.int("ownerID")==Some(pilot) && e.int("locationID")==Some(station)
        && e.int("flag")==Some(4) && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0));
    let Some(ship_type)=hull.map(|e|e.int("typeID").unwrap_or(0)) else {
        return Err(StoreError {code:store::DATA_EXCEPTION,message:format!("hull {ship} of {pilot} is not docked at {station} to arrive")});
    };
    let row=tx.insert("pendingStationArrivals",&[("shipID",store::Value::Int(ship)),("characterID",store::Value::Int(pilot)),
        ("stationID",store::Value::Int(station)),("sourceSystemID",store::Value::Int(source)),("shipTypeID",store::Value::Int(ship_type)),
        ("createdAt",store::Value::Int(combat::now_ms()))])?;
    let generation=tx.table("pendingStationArrivals").row(row).and_then(|r|r.int("generation")).unwrap_or(0);
    Ok(Marker {ship,pilot,station,source,generation,ship_type})
}

fn failed(error:StoreError)->String {
    tracing::warn!("final station snapshot is pending: {}",crate::logging::chain(&error));
    "The station arrival is awaiting its final combat state. Please try again shortly.".into()
}

/// Retains the detached final snapshot before any fallible store write.
/// The owned transition task awaits this; a failed caller must close its stale
/// session while the marker and heartbeat finish reconciliation.
pub async fn finish(server:&Server,store:&Store,marker:Marker,ship:Ship)->Result<ArrivalDisposition,String> {
    if ship.id!=marker.ship || ship.pilot.char_id!=marker.pilot || ship.pilot.type_id!=marker.ship_type {
        return Err("The final station snapshot does not match its arrival.".into());
    }
    let pending={
        let mut world=server.world.lock();
        if let Some(existing)=world.pending_station_arrivals.get_mut(&marker.ship) {
            if existing.marker.generation!=marker.generation || existing.busy {return Err("This station arrival is already being finalized.".into());}
            existing.busy=true;existing.claim=next_claim();existing.clone()
        }else{
            let pending=Pending {marker,ship,busy:true,claim:next_claim()};world.pending_station_arrivals.insert(marker.ship,pending.clone());pending
        }
    };
    let _busy=Busy {world:server.world.clone(),marker,claim:pending.claim};
    let result=attempt(server,store,&pending).await;
    complete_memory(server,&pending,result.is_ok());
    result
}

fn complete_memory(server:&Server,pending:&Pending,success:bool) {
    if !success{return;} // Only the RAII owner releases a failed attempt.
    let mut world=server.world.lock();
    if world.pending_station_arrivals.get(&pending.marker.ship).is_some_and(|p|p.marker.generation==pending.marker.generation && p.claim==pending.claim) {
        world.pending_station_arrivals.remove(&pending.marker.ship);
    }
}

/// Whether the arrival is as its marker left it: the pilot docked in the
/// hull at the station, and the marker of this generation still there.
fn locked(db:&Db,m:Marker)->Result<bool,String> {
    let Some(pilot)=db.table("character_").get_id(m.pilot) else {return Err("The arriving character is unavailable.".into());};
    if pilot.int("shipID")!=Some(m.ship) || pilot.int("stationID")!=Some(m.station) {return Ok(false);}
    let hull=db.table("entity").get_id(m.ship).filter(|e|e.int("ownerID")==Some(m.pilot) && e.int("locationID")==Some(m.station)
        && e.int("typeID")==Some(m.ship_type) && e.int("flag")==Some(4) && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0));
    if hull.is_none(){return Ok(false);}
    Ok(marked(db,m).is_some())
}

/// The marker of this arrival's generation.
fn marked(db:&Db,m:Marker)->Option<store::Row<'_>> {
    db.table("pendingStationArrivals").get_id(m.ship)
        .filter(|p|p.int("characterID")==Some(m.pilot) && p.int("stationID")==Some(m.station) && p.int("generation")==Some(m.generation))
}

fn disposition(db:&Db,pilot:i64)->Result<ArrivalDisposition,String> {
    let Some(c)=db.table("character_").get_id(pilot) else {
        return Err(failed(StoreError {code:store::DATA_EXCEPTION,message:format!("character {pilot} no longer exists")}));
    };
    Ok(ArrivalDisposition {ship_id:c.int("shipID").unwrap_or(0),station_id:c.int("stationID").unwrap_or(0),
        solar_system_id:c.int("solarSystemID").unwrap_or(0),constellation_id:c.int("constellationID").unwrap_or(0),region_id:c.int("regionID").unwrap_or(0)})
}

async fn attempt(server:&Server,store:&Store,p:&Pending)->Result<ArrivalDisposition,String> {
    let m=p.marker;
    let fighter=p.ship.fight.as_ref().ok_or("The final station snapshot has no combat state.")?;
    if !fighter.vitals.hull_max.is_finite() || fighter.vitals.hull_max<=0.0 || !fighter.vitals.hull_damage.is_finite() {
        return Err("The final station snapshot has invalid hull damage.".into());
    }
    let lethal=fighter.vitals.hull_damage>=fighter.vitals.hull_max-1e-6;
    if lethal {
        let mut loss=Loss::of(&p.ship,m.source,p.ship.pilot.type_id==670);
        let raw=serde_json::to_string(&StoredLoss::from(&loss)).map_err(|_|"The final loss could not be recorded.".to_string())?;
        if decode(&raw,m).is_none(){return Err("The final loss snapshot is invalid and remains quarantined.".into());}
        // The loss as recorded (the first one recorded stands), or None
        // when the arrival was already settled.
        let recorded=store.write(|tx| -> Result<Option<Loss>,Refused> {
            if locked(tx.db(),m)? {
                let Some(marker)=marked(tx.db(),m) else {return Err("The pending arrival changed before its final snapshot was saved.".into());};
                let (row,saved)=(marker.id,marker.text("loss").map(str::to_string));
                let saved=match saved {
                    Some(saved)=>saved,
                    None=>{tx.update("pendingStationArrivals",row,&[("loss",store::Value::Text(raw.as_str().into()))])?;raw.clone()}
                };
                Ok(Some(decode(&saved,m).ok_or("The recorded loss is invalid and remains quarantined.")?))
            }else{
                let done=tx.table("killLog").rows().any(|k|k.int("victimItemID")==Some(m.ship) && k.int("victimCharID")==Some(m.pilot));
                if !done{return Err("The pending arrival's custody changed before loss settlement.".into());}
                Ok(None)
            }
        }).map_err(|refused|refused.reason(failed))?;
        if let Some(recorded)=recorded {
            loss=recorded;
            loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
        }
        return super::docked::settle_loss(server,store,m.station,&loss).await;
    }
    // The arrival made final (where the pilot now is), or the loss it
    // recorded, to settle.
    let finished=store.write(|tx| -> Result<Result<ArrivalDisposition,Loss>,Refused> {
        if !locked(tx.db(),m)? {
            if blocked(tx.db(),m.ship)? {return Err("The pending arrival changed before its final snapshot was saved.".into());}
            // An arrival already finalized (finished again) is complete.
            return Ok(Ok(disposition(tx.db(),m.pilot)?));
        }
        let recorded=marked(tx.db(),m).and_then(|marker|marker.text("loss").map(str::to_string));
        if let Some(raw)=recorded {
            return Ok(Err(decode(&raw,m).ok_or("The recorded loss is invalid and remains quarantined.")?));
        }
        inflight::write_vitals(tx,m.ship,fighter,true)?;
        crate::cloak::durability::save_history(tx,&p.ship,combat::now_ms())?;
        let result=disposition(tx.db(),m.pilot)?;
        let marker=marked(tx.db(),m).map(|marker|marker.id);
        if let Some(row)=marker {tx.delete("pendingStationArrivals",row)?;}
        Ok(Ok(result))
    }).map_err(|refused|refused.reason(failed))?;
    match finished {
        Ok(result)=>Ok(result),
        Err(mut loss)=>{
            loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
            super::docked::settle_loss(server,store,m.station,&loss).await
        }
    }
}

pub async fn retry(server:&Server,store:&Store) {
    // The failed transition closes its old connection. Its successful caller,
    // or a later login, publishes the authoritative session. A retry may find
    // that the pilot has already boarded or undocked after a completed
    // settlement, so it must never push a captured relocation to that session.
    let pending:Vec<_>=server.world.lock().pending_station_arrivals.keys().copied().collect();
    for id in pending {
        let Some(p)=({let mut world=server.world.lock();world.pending_station_arrivals.get_mut(&id).filter(|p|!p.busy).map(|p|{p.busy=true;p.claim=next_claim();p.clone()})}) else {continue;};
        let _busy=Busy {world:server.world.clone(),marker:p.marker,claim:p.claim};
        let result=attempt(server,store,&p).await;complete_memory(server,&p,result.is_ok());
        if let Err(error)=result {tracing::warn!(ship=p.marker.ship,"station arrival remains pending: {error}");}
    }
    let cursor=i64::from(server.world.lock().pending_station_arrival_cursor);
    // The next page of arrivals with a recorded loss after the cursor, by hull.
    let rows:Vec<(Marker,String)>=store.read(|db| {
        let mut rows:Vec<_>=db.table("pendingStationArrivals").rows().filter_map(|r| {
            let ship=r.int("shipID").filter(|ship|*ship>cursor)?;
            let raw=r.text("loss")?.to_string();
            Some((Marker {ship,pilot:r.int("characterID").unwrap_or(0),station:r.int("stationID").unwrap_or(0),
                source:r.int("sourceSystemID").unwrap_or(0),ship_type:r.int("shipTypeID").unwrap_or(0),generation:r.int("generation").unwrap_or(0)},raw))
        }).collect();
        rows.sort_by_key(|(m,_)|m.ship);
        rows.truncate(64);
        rows
    });
    server.world.lock().pending_station_arrival_cursor=rows.last().map(|(m,_)|m.ship as i32).unwrap_or(0);
    for (m,raw) in rows {
        if server.world.lock().pending_station_arrivals.contains_key(&m.ship){continue;}
        let Some(mut loss)=decode(&raw,m) else {tracing::warn!(ship=m.ship,"invalid pending loss remains quarantined");continue;};
        loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
        if let Err(error)=super::docked::settle_loss(server,store,m.station,&loss).await {
            tracing::warn!(ship=m.ship,"durable station loss remains pending: {error}");
        }
    }
}

#[derive(serde::Serialize,serde::Deserialize)]
struct StoredAttacker {who:i64,ship_type:i64,weapon_type:i64,damage:f64}
#[derive(serde::Serialize,serde::Deserialize)]
struct StoredLoss {char_id:i64,ship_id:i64,ship_type:i64,system:i64,pos:Vec3,capsule:bool,unpiloted:bool,
    final_blow:Option<i64>,attackers:Vec<StoredAttacker>,items:Vec<(i64,i64)>}
impl From<&Loss> for StoredLoss {
    fn from(l:&Loss)->Self {Self {char_id:l.char_id,ship_id:l.ship_id,ship_type:l.ship_type,system:l.system,pos:l.pos,capsule:l.capsule,
        unpiloted:l.unpiloted,final_blow:l.final_blow,attackers:l.attackers.iter().map(|a|StoredAttacker {who:a.who,ship_type:a.ship_type,weapon_type:a.weapon_type,damage:a.damage}).collect(),items:l.items.clone()}}
}
fn decode(raw:&str,m:Marker)->Option<Loss> {
    if raw.len()>1_048_576{return None;}
    let l:StoredLoss=serde_json::from_str(raw).ok()?;
    if l.char_id!=m.pilot || l.ship_id!=m.ship || l.ship_type!=m.ship_type || l.system!=m.source || l.unpiloted
        || l.capsule!=(l.ship_type==670) || !l.pos.is_finite() || l.attackers.iter().any(|a|!a.damage.is_finite() || a.damage<0.0) {return None;}
    Some(Loss {client_id:0,char_id:l.char_id,ship_id:l.ship_id,ship_type:l.ship_type,system:l.system,pos:l.pos,arrival_custody:None,capsule:l.capsule,
        unpiloted:l.unpiloted,final_blow:l.final_blow,attackers:l.attackers.into_iter().map(|a|Attacker {who:a.who,ship_type:a.ship_type,weapon_type:a.weapon_type,damage:a.damage}).collect(),items:l.items})
}
