//! Portal payment is a durable operation, not a ship relocation. A receipt
//! never replaces a live ship with an earlier snapshot. Unknown final combat
//! state after process loss remains quarantined; no offline timer is invented.
use super::*;
use crate::{conn::Server,losses::Loss,store::Tx};

#[derive(Debug,Clone,PartialEq,serde::Serialize,serde::Deserialize)]
struct Intent {
    pilot:i64,ship:i64,ship_type:i64,system:i64,member:Option<i64>,beacon:i64,to:i64,alliance:Option<i64>,
    quote:Quote,started:i64,expires:i64,boot:String,
}
#[derive(Debug,Clone,PartialEq)]
pub(super) struct Receipt {pub generation:i64,intent:Intent}
#[derive(Debug,Clone)]
pub struct Pending {receipt:Receipt,client:i64,before:Charged,reserved:Charged,loss:Option<Loss>,busy:u64}
impl Pending {
    pub(super) fn owns(&self,ship:i64,pilot:i64,client:i64)->bool {self.receipt.intent.ship==ship && self.receipt.intent.pilot==pilot && self.client==client}
    pub(super) fn owned(receipt:Receipt,client:i64,before:Charged,reserved:Charged)->Self {
        Self {receipt,client,before,reserved,loss:None,busy:claim()}
    }
}
#[derive(Debug,Clone,Copy,PartialEq)] enum Resolution {Accepted,Ready,Closed,Superseded,Unknown}
struct Locked {resolution:Resolution,custody:bool,module:bool,loss:Option<Loss>}
fn claim()->u64 {static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)}
fn boot()->&'static str {
    static BOOT:std::sync::OnceLock<String>=std::sync::OnceLock::new();
    BOOT.get_or_init(||format!("{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()))
}
fn valid(r:&Receipt)->bool {
    let i=&r.intent;let id=|v:i64|v>0 && v<=i32::MAX as i64;
    r.generation>0 && [i.pilot,i.ship,i.ship_type,i.system,i.beacon,i.to,i.quote.generator_id,i.quote.generator_type].into_iter().all(id)
        && i.member.is_none_or(id) && i.alliance.is_none_or(id) && i.system!=i.to && i.started>0
        && i.expires.checked_sub(i.started)==Some(i.quote.duration) && i.quote.duration>0 && i.quote.duration<=i32::MAX as i64
        && i.quote.capacitor.is_finite() && i.quote.capacitor>=0.0 && i.quote.fuel>=0 && i.quote.fuel<=i32::MAX as i64
        && (i.quote.fuel==0 || id(i.quote.fuel_type)) && i.quote.range.is_finite() && i.quote.range>0.0
        && i.quote.mass_factor.is_finite() && i.quote.mass_factor>0.0 && [0,2].contains(&i.quote.harmonics) && i.boot.len()<=128
}
fn loss_valid(loss:&Loss,r:&Receipt)->bool {
    let i=&r.intent;crate::losses::journal::valid(loss) && !loss.unpiloted && loss.arrival_custody.is_none()
        && (loss.char_id,loss.ship_id,loss.ship_type,loss.system)==(i.pilot,i.ship,i.ship_type,i.system)
}
fn decode(row:&store::Row)->Option<Receipt> {
    let raw=row.text("intent")?;if raw.len()>16384{return None;}
    let receipt=Receipt {generation:row.int("generation")?,intent:serde_json::from_str(raw).ok()?};
    (valid(&receipt) && Some(receipt.intent.pilot)==row.int("characterID")
        && Some(receipt.intent.ship)==row.int("shipID")).then_some(receipt)
}
/// `portalReceipts` is part of the store's schema; there is nothing left to
/// bring up to date.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {Ok(())}
/// The receipts not yet paid out (state 0) that hold `column` = `id`.
fn unresolved(db:&Db,column:&str,id:i64)->bool {
    db.table("portalReceipts").find(&[column],&[store::Value::Int(id)]).any(|r|r.int("state")==Some(0))
}
pub fn blocked(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(unresolved(db,"shipID",ship))
}
pub fn blocked_character(db:&Db,pilot:i64)->Result<bool,StoreError> {
    Ok(unresolved(db,"characterID",pilot))
}
pub fn blocked_in_memory(world:&World,ship:i64)->bool {world.lock().pending_portals.values().any(|p|p.receipt.intent.ship==ship)}
pub fn blocked_character_in_memory(world:&World,pilot:i64)->bool {world.lock().pending_portals.values().any(|p|p.receipt.intent.pilot==pilot)}

pub(super) fn begin(tx:&mut Tx<'_>,r:&Request,quote:&Quote,ship_type:i64)->Refused<Receipt> {
    let started=combat::now_ms();let intent=Intent {pilot:r.pilot,ship:r.ship,ship_type,system:r.from,member:r.member,beacon:r.beacon,to:r.to,
        alliance:r.alliance,quote:quote.clone(),started,expires:started.checked_add(quote.duration).ok_or("Invalid portal duration.")?,boot:boot().into()};
    let raw=serde_json::to_string(&intent).map_err(|_|"Invalid portal receipt.")?;
    let row=tx.insert("portalReceipts",&[("characterID",store::Value::Int(r.pilot)),("shipID",store::Value::Int(r.ship)),("intent",store::Value::Text(raw.into()))]).map_err(database)?;
    let generation=tx.table("portalReceipts").row(row).and_then(|row|row.int("generation")).unwrap_or(0);
    let receipt=Receipt {generation,intent};if !valid(&receipt){return Err("Invalid portal receipt.".into());}Ok(receipt)
}

/// The caller already holds the character and hull custody. A runtime
/// closure may precede its asynchronous durable closure; retire only that row.
pub(super) fn retire_ready(tx:&mut Tx<'_>,world:&World,ship:i64)->Refused<bool> {
    let rows:Vec<(i64,Option<i64>)>=tx.table("portalReceipts").find(&["shipID"],&[store::Value::Int(ship)])
        .filter(|r|matches!(r.int("state"),Some(0|1))).map(|r|(r.int("generation").unwrap_or(0),r.int("state"))).collect();
    for (generation,state) in rows {
        let live={let w=world.lock();w.pending_portals.contains_key(&generation) || w.bridges.get(&ship).is_some_and(|b|
            b.generation==generation && b.until_ms>combat::now_ms() && bridge_live(&w,ship,b))};
        if state==Some(0) || live {return Ok(false);}
        close(tx,generation)?;
    }Ok(true)
}
fn close(tx:&mut Tx<'_>,generation:i64)->Refused<()> {
    let Some(row)=tx.table("portalReceipts").get_id(generation).filter(|r|matches!(r.int("state"),Some(0|1))).map(|r|r.id) else {return Ok(());};
    tx.update("portalReceipts",row,&[("state",store::Value::Int(2)),("completedAt",store::Value::Int(combat::now_ms()))]).map_err(database)
}

fn locked(db:&Db,r:&Receipt)->Refused<Locked> {
    if !valid(r){return Err("Invalid portal receipt remains quarantined.".into());}
    let i=&r.intent;
    let entity=db.table("entity");
    let custody=db.table("character_").get_id(i.pilot).is_some_and(|p|p.int("shipID")==Some(i.ship) && p.int("solarSystemID")==Some(i.system) && p.int("stationID").unwrap_or(0)==0)
        && entity.get_id(i.ship).is_some_and(|h|h.int("typeID")==Some(i.ship_type) && h.int("ownerID")==Some(i.pilot)
            && h.int("locationID")==Some(i.system) && h.int("flag")==Some(0) && h.int("quantity")==Some(1) && h.int("singleton").is_some_and(|v|v!=0));
    let online=db.table("entity_attributes").get(&[store::Value::Int(i.quote.generator_id),store::Value::Int(2)])
        .is_some_and(|a|a.float("valueInt").or(a.float("valueFloat")).unwrap_or(0.0)>0.0);
    let module=entity.get_id(i.quote.generator_id).is_some_and(|m|m.int("typeID")==Some(i.quote.generator_type)
        && m.int("ownerID")==Some(i.pilot) && m.int("locationID")==Some(i.ship)
        && m.int("flag").is_some_and(|v|(11..=35).contains(&v)) && m.int("quantity")==Some(1) && m.int("singleton").is_some_and(|v|v!=0))
        && online;
    let receipts=db.table("portalReceipts");
    let newer=receipts.rows().any(|n|n.int("generation").is_some_and(|g|g>r.generation)
        && (n.int("shipID")==Some(i.ship) || n.int("characterID")==Some(i.pilot)));
    // The receipt is written in the same store write that pays, so a missing
    // one was never accepted here: it stays quarantined.
    let Some(row)=receipts.get_id(r.generation) else {
        return Ok(Locked {resolution:Resolution::Unknown,custody,module,loss:None});
    };
    if decode(&row).as_ref()!=Some(r){return Err("Portal receipt identity mismatch remains quarantined.".into());}
    let loss=match row.text("loss") {
        Some(raw)=>{let loss:Loss=if raw.len()<=1048576 {serde_json::from_str(raw).map_err(|_|"Corrupt portal loss remains quarantined.")?}else{return Err("Oversized portal loss remains quarantined.".into());};
            if !loss_valid(&loss,r){return Err("Invalid portal loss remains quarantined.".into());}Some(loss)},None=>None};
    let resolution=match row.int("state") {Some(0) if !newer=>Resolution::Accepted,Some(1) if !newer=>Resolution::Ready,Some(2) if newer=>Resolution::Superseded,Some(2)=>Resolution::Closed,_=>Resolution::Unknown};
    Ok(Locked {resolution,custody,module,loss})
}
fn request(r:&Receipt,world:&World,client:i64)->Request {let i=&r.intent;Request {world:world.clone(),pilot:i.pilot,client,ship:i.ship,from:i.system,
    member:i.member,beacon:i.beacon,to:i.to,alliance:i.alliance,bonuses:BTreeMap::new()}}
fn bridge(r:&Receipt)->crate::world::Bridge {let i=&r.intent;crate::world::Bridge {generation:r.generation,started_ms:i.started,titan_pilot:i.pilot,
    generator_id:i.quote.generator_id,system:i.system,to_system:i.to,beacon:i.beacon,member:i.member,until_ms:i.expires,mass_factor:i.quote.mass_factor,harmonics:i.quote.harmonics}}
fn fixed(db:&Db,i:&Intent)->bool {
    if i.member.is_some(){return true;}
    db.table("character_").get_id(i.pilot).and_then(|c|c.int("corporationID"))
        .is_some_and(|corporation|beacon_available(db,i.beacon,i.to,i.alliance.unwrap_or(0),corporation))
}
fn remember_loss(world:&World,r:&Receipt,loss:Loss) {
    let mut w=world.lock();if let Some(p)=w.pending_portals.get_mut(&r.generation).filter(|p|p.receipt==*r) {if p.loss.is_none(){p.loss=Some(loss);}}
    // The portal marker already quarantines this source. Do not expose the
    // loss to independent settlement before the portal closure commits.
}
fn handoff_loss(world:&World,loss:Loss) {world.lock().pending_ship_losses.entry(loss.ship_id).or_insert(loss);}
fn settled_source(db:&Db,r:&Receipt)->bool {
    let i=&r.intent;
    db.table("entity").get_id(i.ship).is_none()
        && db.table("killLog").rows().any(|k|k.int("victimItemID")==Some(i.ship) && k.int("victimCharID")==Some(i.pilot)
            && k.int("victimShipTypeID")==Some(i.ship_type) && k.int("solarSystemID")==Some(i.system))
}
fn forget(world:&World,r:&Receipt) {let mut w=world.lock();if w.pending_portals.get(&r.generation).is_some_and(|p|p.receipt==*r) {w.pending_portals.remove(&r.generation);}}
fn release(world:&World,r:&Receipt,refund:bool,closed:bool) {
    let mut w=world.lock();let Some(p)=w.pending_portals.remove(&r.generation) else{return;};
    if p.receipt!=*r {w.pending_portals.insert(r.generation,p);return;}
    let i=&r.intent;
    // Historical completion is never authority over a newer same-pilot,
    // same-client reservation. Retire only our own memory obligation.
    if w.pending_portals.values().any(|p|p.receipt.intent.ship==i.ship || p.receipt.intent.pilot==i.pilot)
        || w.bridges.get(&i.ship).is_some_and(|b|b.generation>r.generation)
        || w.portal_cycles.get(&i.ship).is_some_and(|b|b.generation>r.generation) {return;}
    w.pending_bridges.remove(&i.ship);
    if closed && w.bridges.get(&i.ship).is_some_and(|b|b.generation==r.generation) {
        w.bridges.remove(&i.ship);announce(&w,&request(r,world,p.client),false);
    }
    // A revoked route does not prove the module's accepted timed cycle ended.
    // This lifecycle is separate from traveler access and keeps original906
    // restrictions until the already-recorded deadline, with no renewed grace.
    if !refund && i.expires>combat::now_ms() && w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship))
        .is_some_and(|s|s.pilot.char_id==i.pilot && s.client_id==p.client && !s.unpiloted && s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage<f.vitals.hull_max-1e-6)) {
        if w.portal_cycles.get(&i.ship).is_none_or(|cycle|cycle.generation<=r.generation){w.portal_cycles.insert(i.ship,bridge(r));}
    }
    let mut amount=None;
    if let Some(s)=w.parks.get_mut(&i.system).and_then(|p|p.ships.get_mut(&i.ship)).filter(|s|s.pilot.char_id==i.pilot && s.client_id==p.client && s.identity_reserved) {
        if let Some(f)=&mut s.fight {if refund {if f.vitals.capacitor==p.reserved {f.vitals.capacitor=p.before;}else{f.vitals.capacitor.add(combat::now_ms(),i.quote.capacitor);}}
            amount=Some(f.vitals.capacitor.at(combat::now_ms()));}s.identity_reserved=false;
    }
    if let Some(amount)=amount {w.notify(&[p.client],"OnModuleAttributeChanges",Value::Tuple(vec![Value::List(vec![Value::Tuple(vec![
        Value::bytes("OnModuleAttributeChange"),Value::Int(i.pilot),Value::Int(i.ship),Value::Int(combat::attr::CHARGE as i64),
        Value::Long(combat::filetime(combat::now_ms())),Value::Double(amount),Value::Double(amount)])]) ]));}
}
pub(super) struct Busy {world:World,generation:i64,claim:u64}
impl Drop for Busy {fn drop(&mut self){if let Some(p)=self.world.lock().pending_portals.get_mut(&self.generation).filter(|p|p.busy==self.claim){p.busy=0;}}}
pub(super) fn owner_guard(world:&World,generation:i64)->Busy {Busy {world:world.clone(),generation,claim:world.lock().pending_portals[&generation].busy}}

/// How the first write of an attempt ended.
enum Step {
    /// Resolved without further writes.
    Done(Resolution),
    /// A lethal source's loss was retained; the second write stages it.
    Loss,
    /// The source's fresh checkpoint was written, and the receipt marked
    /// ready (`target`) or closed.
    Checkpointed {target:bool},
}

/// Resolve only, never replay payment. The source remains live: each failed
/// final write is retried from current combat state, not a retained healthy copy.
async fn attempt(world:&World,store:&Store,pending:&Pending)->Refused<Resolution> {
    let r=&pending.receipt;let i=&r.intent;let req=request(r,world,pending.client);
    let step=store.write(|tx|->Result<Step,Refusal> {
        let state=locked(tx.db(),r)?;
        match state.resolution {Resolution::Superseded=>return Ok(Step::Done(Resolution::Superseded)),Resolution::Unknown=>return Err("Portal payment outcome remains unknown.".into()),_=>{}}
        if state.resolution==Resolution::Closed && (state.loss.is_some() || !state.custody) {
            if state.custody {if let Some(loss)=state.loss {handoff_loss(world,loss);}}return Ok(Step::Done(Resolution::Closed));
        }
        if !state.custody {
            if settled_source(tx.db(),r) {close(tx,r.generation)?;return Ok(Step::Done(Resolution::Closed));}
            return Err("Portal source custody changed; outcome remains quarantined.".into());
        }
        let live={let w=world.lock();w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship).filter(|s|
            s.pilot.char_id==i.pilot && s.pilot.type_id==i.ship_type && !s.unpiloted && s.client_id==pending.client && s.identity_reserved).map(|s|(s.clone(),p.tick)))};
        let loss=state.loss.or_else(||pending.loss.clone()).or_else(||live.as_ref().and_then(|(s,_)|s.fight.as_ref().filter(|f|
            f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6).map(|_|Loss::of(s,i.system,s.pilot.type_id==670))));
        if let Some(loss)=loss {
            if !loss_valid(&loss,r){return Err("Invalid portal source loss.".into());}remember_loss(world,r,loss.clone());
            // Retain known lethality first, so failure of the subsequent loss stage
            // cannot turn a restart into an invented healthy source.
            let raw=serde_json::to_string(&loss).map_err(|_|"Could not encode portal source loss.")?;
            if let Some(row)=tx.table("portalReceipts").get_id(r.generation) {
                let (row,kept)=(row.id,row.value("loss").clone());
                let loss=if kept.is_null() {store::Value::Text(raw.into())}else{kept};
                tx.update("portalReceipts",row,&[("loss",loss),("state",store::Value::Int(0))])?;
            }
            return Ok(Step::Loss);
        }
        let Some((ship,tick))=live else {return if state.resolution==Resolution::Closed {Ok(Step::Done(Resolution::Closed))}else{Err("The last portal source combat state is unknown; it remains quarantined.".into())};};
        let target=state.resolution!=Resolution::Closed && state.module && fixed(tx.db(),i) && i.boot==boot() && i.expires>combat::now_ms()
            && {let w=world.lock();source(&w,&req,true).is_ok() && destination(&w,&req,i.quote.harmonics).is_ok()};
        // Even a Ready checkpoint can have newer live combat. Save the fresh
        // same-reservation snapshot, never a retained healthy copy; this
        // writes the existing charge, without performing another debit.
        if !crate::space_state::write_portal(tx,i.system,&ship,tick)? {return Err("Portal source checkpoint remains quarantined.".into());}
        if target {
            if let Some(row)=tx.table("portalReceipts").get_id(r.generation).filter(|row|row.int("state")==Some(0)).map(|row|row.id) {
                tx.update("portalReceipts",row,&[("state",store::Value::Int(1)),("completedAt",store::Value::Int(combat::now_ms()))])?;
            }
        }
        else {close(tx,r.generation)?;}
        Ok(Step::Checkpointed {target})
    }).map_err(|Refusal(why)|why)?;
    let target=match step {
        Step::Done(resolution)=>return Ok(resolution),
        Step::Loss=>{
            let (resolution,loss)=store.write(|tx|->Result<_,Refusal> {
                let l=locked(tx.db(),r)?;
                if l.resolution==Resolution::Closed{return Ok((Resolution::Closed,None));}
                let loss=l.loss.ok_or("Portal loss is not durable.")?;
                if !l.custody && settled_source(tx.db(),r) {close(tx,r.generation)?;return Ok((Resolution::Closed,None));}
                if !l.custody || !crate::losses::journal::stage(tx,&loss)? {return Err("Portal loss custody could not be confirmed.".into());}
                close(tx,r.generation)?;Ok((Resolution::Closed,Some(loss)))
            }).map_err(|Refusal(why)|why)?;
            if let Some(loss)=loss {handoff_loss(world,loss);}
            return Ok(resolution);
        }
        Step::Checkpointed {target}=>target,
    };
    let lethal={let w=world.lock();w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship))
        .filter(|s|s.pilot.char_id==i.pilot && s.client_id==pending.client && s.identity_reserved)
        .filter(|s|s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6))
        .map(|s|Loss::of(s,i.system,s.pilot.type_id==670))};
    if let Some(loss)=lethal {remember_loss(world,r,loss);return Err("Portal source became lethal during its checkpoint; retaining its loss.".into());}
    if !target {return Ok(Resolution::Closed);}
    // No store access between the final runtime validation and publication.
    let mut w=world.lock();
    if !w.pending_portals.get(&r.generation).is_some_and(|p|p.receipt==*r) {return Err("Portal generation was superseded.".into());}
    if source(&w,&req,true).is_err() || destination(&w,&req,i.quote.harmonics).is_err() || i.expires<=combat::now_ms() {
        if let Some(s)=w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship)).filter(|s|s.pilot.char_id==i.pilot && s.client_id==pending.client) {
            if s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
                let loss=Loss::of(s,i.system,s.pilot.type_id==670);drop(w);remember_loss(world,r,loss);
            }
        }
        return Err("Accepted portal source or destination changed; closing its paid cycle.".into());
    }
    if w.bridges.get(&i.ship).is_some_and(|b|b.generation!=r.generation){return Err("A newer portal owns this ship.".into());}
    if !w.bridges.contains_key(&i.ship) {
        w.bridges.insert(i.ship,bridge(r));announce(&w,&req,true);let tick=w.parks[&i.system].tick;
        crate::svc::space::broadcast(&w,i.system,None,vec![crate::svc::space::special_fx(tick,i.ship,None,"effects.JumpPortal")]);
    }Ok(Resolution::Ready)
}

/// Finishes an accepted start: its item changes are told, then it is
/// resolved (opened, or closed) from the live source, retrying until it is.
pub(super) async fn complete_owned(r:&Request,store:&Store,receipt:&Receipt,events:Vec<Value>)->Refused<()> {
    {let w=r.world.lock();for e in &events {w.notify(&[r.client],"OnItemChange",e.clone());}}
    loop {
        let result:Refused<()>=async {
            let pending=r.world.lock().pending_portals.get(&receipt.generation).cloned().ok_or("Portal operation ownership was lost.")?;
            match attempt(&r.world,store,&pending).await? {
                outcome @ (Resolution::Ready|Resolution::Closed)=>{release(&r.world,receipt,false,outcome==Resolution::Closed);Ok(())},
                Resolution::Superseded=>{forget(&r.world,receipt);Ok(())},
                _=>Err("Accepted portal finalization remains unresolved.".into()),
            }
        }.await;
        match result {Ok(())=>return Ok(()),Err(error)=>tracing::warn!(generation=receipt.generation,"{error}")}
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// What a recovery scan needs of a receipt not yet closed.
struct Unclosed {generation:i64,receipt:Option<Receipt>,state:Option<i64>,loss:Option<String>}

pub async fn retry(server:&Server,store:&Store) {
    let mut pending:Vec<_>=server.world.lock().pending_portals.keys().copied().collect();pending.sort_unstable();
    for generation in pending {
        let Some(p)=({let mut w=server.world.lock();w.pending_portals.get_mut(&generation).filter(|p|p.busy==0).map(|p|{p.busy=claim();p.clone()})}) else {continue;};
        let _busy=Busy {world:server.world.clone(),generation:p.receipt.generation,claim:p.busy};
        match attempt(&server.world,store,&p).await {
            Ok(outcome @ (Resolution::Ready|Resolution::Closed))=>release(&server.world,&p.receipt,false,outcome==Resolution::Closed),
            Ok(Resolution::Superseded)=>forget(&server.world,&p.receipt),Ok(_)=>{},Err(e)=>tracing::warn!(generation=p.receipt.generation,"portal retry: {e}")}
    }
    let cursor=server.world.lock().portal_retry_cursor;
    // Those after the cursor first, then from the start; 64 at a time.
    let rows:Vec<Unclosed>=store.read(|db| {
        let mut rows:Vec<Unclosed>=db.table("portalReceipts").rows().filter(|r|matches!(r.int("state"),Some(0|1)))
            .map(|r|Unclosed {generation:r.int("generation").unwrap_or(0),receipt:decode(&r),state:r.int("state"),loss:r.text("loss").map(str::to_string)})
            .collect();
        rows.sort_by_key(|r|(r.generation<=cursor,r.generation));
        rows.truncate(64);
        rows
    });
    for row in rows {
        let generation=row.generation;server.world.lock().portal_retry_cursor=generation;
        if server.world.lock().pending_portals.contains_key(&generation){continue;}
        let Some(r)=row.receipt else {tracing::warn!(generation,"invalid portal receipt remains quarantined");continue;};
        if row.state==Some(0) {
            let Some(raw)=row.loss else {
                // A generic loss may already have committed before this
                // receipt retained its loss payload. Only exact destruction
                // proof can retire such a marker; surviving/unknown hulls
                // remain quarantined and are never reconstructed here.
                let result=store.write(|tx|->Result<(),Refusal> {let state=locked(tx.db(),&r)?;
                    if state.resolution==Resolution::Accepted && !state.custody && settled_source(tx.db(),&r) {close(tx,generation)?;}
                    Ok(())});
                if let Err(Refusal(error))=result {tracing::warn!(generation,"portal completion proof: {error}");}continue;
            };
            let Some(loss)= (if raw.len()<=1048576 {serde_json::from_str::<Loss>(&raw).ok()}else{None}).filter(|l|loss_valid(l,&r)) else{continue;};
            let mut p=Pending::owned(r.clone(),0,Charged::full(0.0,0.0,0),Charged::full(0.0,0.0,0));p.loss=Some(loss);p.busy=0;
            server.world.lock().pending_portals.entry(generation).or_insert(p);continue;
        }
        // A completed payment does not authorize recreating an offline ship or
        // a transient cyno ID from a prior process. Never renew its deadline.
        let live={let w=server.world.lock();w.bridges.get(&r.intent.ship).is_some_and(|b|b.generation==generation && b.until_ms>combat::now_ms() && bridge_live(&w,r.intent.ship,b))};
        if !live {
            let result=store.write(|tx|->Result<(),Refusal> {let l=locked(tx.db(),&r)?;
                if l.resolution==Resolution::Ready {close(tx,generation)?;}Ok(())});
            if let Err(Refusal(error))=result {tracing::warn!(generation,"portal closure: {error}");}
        }
    }
}
