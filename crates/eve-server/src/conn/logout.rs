//! Disconnected ships stay live until they can leave space with their final
//! snapshot saved. Portal sources additionally remain through the accepted
//! generator cycle.
use crate::{ballpark::Ship,combat,conn::Server,world::World};
use crate::store::{Db,Store,StoreError,Tx,Value};

/// A ship already taken out of space whose final snapshot was refused: it is
/// held, and the snapshot retried, rather than rewound into space.
#[derive(Clone,Debug)]
pub(crate) struct Pending {system:i64,ship:Ship,tick:i64}

pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

pub fn blocked(db:&Db,ship:i64)->Result<bool,StoreError> {
    Ok(db.table("pendingShipLogouts").find_one(&["shipID"],&[Value::Int(ship)]).is_some())
}
pub fn blocked_character(db:&Db,pilot:i64)->Result<bool,StoreError> {
    Ok(db.table("pendingShipLogouts").find_one(&["characterID"],&[Value::Int(pilot)]).is_some())
}
pub fn blocked_character_in_memory(world:&World,pilot:i64)->bool {
    world.lock().pending_logouts.values().any(|p|p.ship.pilot.char_id==pilot)
}

/// The persistence worker resumes attempts begun by disconnect handlers.
pub async fn retry(server:&Server,store:&Store) {
    let ids:std::collections::BTreeSet<_>={let w=server.world.lock();w.retained_logouts.keys().chain(w.pending_logouts.keys()).copied().collect()};
    for id in ids {
        if let Err(error)=retry_one(&server.world,store,id).await {
            tracing::warn!(ship=id,"saving retained logout: {}",crate::logging::chain(&error));
        }
    }
}

/// Why an attempt's write is undone.
enum Undone {
    Store(StoreError),
    /// The final snapshot was refused; what the attempt wrote goes with it.
    Refused,
}

impl From<StoreError> for Undone {
    fn from(error:StoreError)->Undone {Undone::Store(error)}
}

pub(crate) async fn retry_one(world:&World,store:&Store,id:i64)->Result<(),StoreError> {
    let missing={let w=world.lock();w.retained_logouts.get(&id).copied().filter(|_|!w.pending_logouts.contains_key(&id)
        && !w.parks.values().any(|park|park.ships.contains_key(&id)))};
    if let Some(identity)=missing {
        let remains=store.read(|db| {
            db.table("entity").get_id(id).is_some_and(|e|e.int("ownerID")==Some(identity.0))
                && db.table("character_").get_id(identity.0).is_some_and(|c|c.int("shipID")==Some(id) && c.int("stationID").unwrap_or(0)==0)
        });
        if !remains {let mut w=world.lock();if w.retained_logouts.get(&id)==Some(&identity) && !w.pending_logouts.contains_key(&id) {w.retained_logouts.remove(&id);}}
        return Ok(());
    }
    let (pilot,client,system,pending)={
        let mut w=world.lock();
        if let Some(p)=w.pending_logouts.get(&id) {(p.ship.pilot.char_id,p.ship.client_id,p.system,Some(p.clone()))}
        else {
            let Some(&(pilot,client))=w.retained_logouts.get(&id) else {return Ok(());};
            let live=w.parks.iter().find_map(|(system,park)|park.ships.get(&id).map(|s|(*system,s)));
            let Some((system,ship))=live else {return Ok(());};
            if !ship.is_player() || ship.pilot.char_id!=pilot || ship.client_id!=client {
                w.retained_logouts.remove(&id);return Ok(());
            }
            if !w.clients_of(pilot).is_empty() || ship.identity_reserved || crate::svc::jumps::portal::active_until_locked(&w,id).is_some() {return Ok(());}
            (pilot,client,system,None)
        }
    };
    let saved=store.write(|tx| {
        if pending.is_some() && tx.table("entity").get_id(id).is_none() {
            // Loss settlement has removed the held hull since: there is no
            // snapshot left to save.
            let mut w=world.lock();
            if w.pending_logouts.remove(&id).is_some() && w.retained_logouts.get(&id)==Some(&(pilot,client)) {w.retained_logouts.remove(&id);}
            return Ok(());
        }
        if !crate::space_state::lock_current(tx.db(),pilot,id,system)? {return Ok(());}
        if crate::losses::pending::blocked_except_logout(tx.db(),id)? {return Ok(());}
        let mut w=world.lock();
        let p=if let Some(p)=pending {p}else{
            if !ready(&w,id,pilot,client,system) {return Ok(());}
            let tick=w.parks[&system].tick;
            let Some(ship)=crate::svc::space::remove_ship(&mut w,system,id) else {return Ok(());};
            // Taken out of space, the ship is held until its snapshot is
            // saved, whatever becomes of this attempt.
            let p=Pending{system,ship,tick};w.pending_logouts.insert(id,p.clone());p
        };
        if !crate::space_state::write_logout(tx,p.system,&p.ship,p.tick)? {return Err(Undone::Refused);}
        tx.delete_where("pendingShipLogouts",&["shipID"],&[Value::Int(id)])?;
        w.pending_logouts.remove(&id);w.retained_logouts.remove(&id);
        Ok(())
    });
    match saved {
        Ok(())|Err(Undone::Refused)=>Ok(()),
        Err(Undone::Store(error))=>Err(error),
    }
}

fn ready(w:&crate::world::WorldState,id:i64,pilot:i64,client:i64,system:i64)->bool {
    w.clients_of(pilot).is_empty() && !w.pending_ship_losses.contains_key(&id)
        && crate::svc::jumps::portal::active_until_locked(w,id).is_none()
        && w.parks.get(&system).and_then(|park|park.ships.get(&id)).is_some_and(|ship|
            ship.is_player() && ship.pilot.char_id==pilot && ship.client_id==client && !ship.identity_reserved
            && ship.fight.as_ref().is_none_or(|fight|!fight.modules.iter().any(|m|m.action==combat::Action::Cyno)))
}
