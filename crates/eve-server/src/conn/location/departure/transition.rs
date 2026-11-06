//! Gate, station and administrative transitions share the paid jump's live
//! identity reservation. Only a committed move removes a hull from combat.
use super::*;
use super::reserved::{Mobility,Reservation,CommitState};
use crate::{combat,conn::Server,items::ItemTxn,session::Session};

#[derive(Clone,Copy,Debug)]
pub(in crate::conn::location) struct Place {
    pub station:Option<i64>,pub system:i64,pub constellation:i64,pub region:i64,pub at:Vec3,
}

#[derive(Clone,Copy)]
pub(in crate::conn::location) enum Request {
    Gate {from_gate:i64,to_gate:i64,to:Place},
    Dock {station:i64,towed:bool},
    Admin {to:Place},
}

#[derive(Debug)]
pub(in crate::conn::location) struct Arrived {
    pub place:Place,
    pub ship_id:i64,
    pub docking:Option<(Vec3,Vec3,i64)>,
}

#[derive(Debug)]
pub(in crate::conn::location) enum Failure {Refused(String),Pending(String)}
impl From<String> for Failure {fn from(reason:String)->Self {Self::Refused(reason)}}
impl From<&str> for Failure {fn from(reason:&str)->Self {Self::Refused(reason.into())}}
impl From<StoreError> for Failure {fn from(error:StoreError)->Self {Self::Refused(travel_failed(error))}}
impl std::fmt::Display for Failure {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {match self {Self::Refused(s)|Self::Pending(s)=>f.write_str(s)}}
}

fn validate_live(w:&WorldState,reservation:&Reservation,request:Request,from:i64,ship:i64)->std::result::Result<(),String> {
    reservation.current(w,true)?;
    let park=&w.parks[&from];
    match request {
        Request::Gate {from_gate,..}=>{
            if !park.surface_distance(ship,from_gate).is_some_and(|d|d.is_finite() && d<=JUMP_RANGE*RANGE_SLACK) {
                return Err("Your ship is no longer within range of that stargate.".into());
            }
        }
        Request::Dock {station,towed}=>{
            if !space::is_station(park,station) {return Err("That station is not in this solar system.".into());}
            if !towed && !park.surface_distance(ship,station).is_some_and(|d|d.is_finite() && d<=DOCK_RANGE) {
                return Err("Your ship is no longer within docking range.".into());
            }
        }
        Request::Admin {..}=>{},
    }
    Ok(())
}

/// The connection calls this in an owned task through publication.
pub(in crate::conn::location) async fn run(server:&Server,session:&Session,client:i64,request:Request)->std::result::Result<Arrived,Failure> {
    let ctx=crate::services::Ctx {bulk:&server.bulk,dogma:&server.dogma,session,store:&server.store,
        node_id:server.node_id,world:&server.world,client_id:client};
    let pilot=session.char_id.ok_or("You have no active character.")?;
    let ship=session.ship_id.ok_or("You have no active ship.")?;
    let from=session.solar_system_id2.or(session.solar_system_id).ok_or("Your current system is unknown.")?;
    let source_station=session.station_id;
    if !matches!(request,Request::Admin{..}) && source_station.is_some() {return Err("You must be in space.".into());}
    match request {
        Request::Dock {towed:true,..} if session.role&(8|16|262144)==0=>return Err("Only a game master may request a remote tow.".into()),
        Request::Admin {..} if session.role&(8|262144)==0=>return Err("You are not allowed to relocate ships.".into()),
        Request::Admin {to} if source_station.is_some() && to.station.is_none()=>return Err("Undock before relocating into space.".into()),
        _=>{},
    }
    let (mut reservation,place,profile,snapshot,marker,space_marker)=server.store.write(|tx| -> std::result::Result<_,Failure> {
        let mut txn=ItemTxn::new(tx);
        let Some(character)=txn.db().table("character_").get_id(pilot) else {
            return Err("Your character is no longer available.".into());
        };
        if character.int("shipID")!=Some(ship) || character.int("solarSystemID")!=Some(from)
            || character.int("stationID").unwrap_or(0)!=source_station.unwrap_or(0) {
            return Err("Your active ship or location changed before departure.".into());
        }
        if crate::losses::pending::blocked_character(txn.db(),pilot).map_err(travel_failed)?
            || crate::travel::blocked_character_in_memory(&server.world,pilot)
            || crate::svc::jumps::portal::blocked_character_in_memory(&server.world,pilot) {return Err("Your earlier travel, portal activation or hull loss is still pending.".into());}
        let hull=txn.item(ship).map_err(travel_failed)?.ok_or("Your ship no longer exists.")?;
        if hull.owner_id!=pilot || hull.location_id!=source_station.unwrap_or(from) || hull.flag!=if source_station.is_some(){4}else{0}
            || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {
            return Err("Your active ship is no longer available here.".into());
        }
        let reservation=if source_station.is_none() {
            let mut w=server.world.lock();
            w.parks.get_mut(&from).ok_or("Your ship is no longer in space.")?.advance(destiny::tick_now());
            let mobility=match request {Request::Gate{..}=>Mobility::Jump,Request::Dock{..}=>Mobility::Dock,Request::Admin{..}=>Mobility::Admin};
            Some(Reservation::reserve(&ctx,&mut w,from,mobility,0.0)?)
        }else{None};
        if let Some(r)=&reservation {validate_live(&server.world.lock(),r,request,from,ship)?;}
        let db=txn.db();
        let place=match request {
            Request::Gate {from_gate,to_gate,to}=>{
                // The gate pair must be one mapJumps joins (static data).
                crate::sde::table("mapJumps").get("stargateID",from_gate).filter(|j|j.int("celestialID")==Some(to_gate)).ok_or("That stargate does not lead there.")?;
                let gates=db.table("mapDenormalize");
                gates.get_id(from_gate).filter(|s|s.int("solarSystemID")==Some(from) && s.int("groupID")==Some(10))
                    .ok_or("That stargate does not lead there.")?;
                let d=gates.get_id(to_gate).filter(|d|d.int("groupID")==Some(10)).ok_or("That stargate does not lead there.")?;
                if d.int("solarSystemID")!=Some(to.system) {return Err("That gate's destination changed.".into());}
                let coordinate=|column:&str|d.float(column).unwrap_or(0.0);
                Place {station:None,system:to.system,constellation:d.int("constellationID").unwrap_or(0),region:d.int("regionID").unwrap_or(0),
                    at:space::arrival_point(Vec3::new(coordinate("x"),coordinate("y"),coordinate("z")),coordinate("radius"))}
            }
            Request::Dock {station,..}=>{
                let row=db.table("staStations").get_id(station).ok_or("That station no longer exists.")?;
                if row.int("solarSystemID")!=Some(from) {return Err("That station is not in this solar system.".into());}
                if !crate::svc::outposts::may_use_locked(db,station,2097152,pilot).map_err(travel_failed)? {return Err("The station's owners do not allow you to dock here.".into());}
                Place {station:Some(station),system:from,constellation:row.int("constellationID").unwrap_or(0),region:row.int("regionID").unwrap_or(0),at:Vec3::ZERO}
            }
            Request::Admin {to}=>{
                if !to.at.is_finite() {return Err("The relocation position is invalid.".into());}
                let row=db.table("mapSolarSystems").get_id(to.system).ok_or("The destination system does not exist.")?;
                if let Some(station)=to.station {
                    if !db.table("staStations").get_id(station).is_some_and(|s|s.int("solarSystemID")==Some(to.system)) {
                        return Err("The station is not in that destination system.".into());
                    }
                }
                Place {constellation:row.int("constellationID").unwrap_or(0),region:row.int("regionID").unwrap_or(0),..to}
            }
        };
        if place.station.is_none() && !server.world.lock().parks.contains_key(&place.system) {return Err("The destination system is not ready.".into());}
        let profile=inflight::profile(db,&server.dogma,pilot,ship,&Default::default(),combat::now_ms()).map_err(travel_failed)?
            .ok_or("Your ship could not be prepared for arrival.")?;
        let snapshot=if let Some(r)=&reservation {r.current(&server.world.lock(),true)?.clone()}else{
            let saved=db.table("entity").get_id(ship).map(|e|[e.float("x"),e.float("y"),e.float("z")].map(|v|v.unwrap_or(0.0))).ok_or_else(unsaved)?;
            let mut loaded=Ship::new(ship,client,profile.stats,profile.pilot.clone(),Vec3::new(saved[0],saved[1],saved[2]),Vec3::ZERO);
            loaded.fight=Some(profile.fighter.clone());loaded
        };
        let source=super::completion::source(db,from,source_station,snapshot.pos)?;
        let destination=place.station.unwrap_or(place.system);
        let tx=txn.tx();
        let row=tx.table("character_").get_id(pilot).map(|c|c.id).ok_or_else(unsaved)?;
        tx.update("character_",row,&[("stationID",StoreValue::Int(place.station.unwrap_or(0))),("solarSystemID",StoreValue::Int(place.system)),
            ("constellationID",StoreValue::Int(place.constellation)),("regionID",StoreValue::Int(place.region))]).map_err(travel_failed)?;
        update_by_id(tx,"entity",ship,&[("locationID",StoreValue::Int(destination)),("flag",StoreValue::Int(if place.station.is_some(){4}else{0})),
            ("x",StoreValue::Float(place.at.x)),("y",StoreValue::Float(place.at.y)),("z",StoreValue::Float(place.at.z))]).map_err(travel_failed)?;
        update_by_id(tx,"entity",pilot,&[("locationID",StoreValue::Int(destination))]).map_err(travel_failed)?;
        if let Some(f)=&snapshot.fight {inflight::write_vitals(tx,ship,f,place.station.is_some()).map_err(travel_failed)?;}
        crate::cloak::durability::save_history(tx,&snapshot,combat::now_ms()).map_err(travel_failed)?;
        let marker=if let Some(station)=place.station {
            Some(crate::losses::pending::begin(tx,pilot,ship,station,from).map_err(travel_failed)?)
        }else{None};
        let space_marker=if place.station.is_none() {
            Some(crate::travel::begin(tx,pilot,ship,source,super::completion::destination(place)).map_err(travel_failed)?)
        }else{None};
        if let Some(r)=&reservation {validate_live(&server.world.lock(),r,request,from,ship)?;}
        Ok((reservation,place,profile,snapshot,marker,space_marker))
    })?;
    if let Some(r)=reservation.as_mut() {r.commit_state=CommitState::Accepted;}
    let (latest,docking,configuration)={
        let mut w=server.world.lock();
        if let Some(r)=&reservation {r.current(&w,false).map_err(Failure::Pending)?;}
        let docking=match request {Request::Dock{station,..}=>{
            let park=&w.parks[&from];Some((park.ships[&ship].pos,park.ball(station).map(|v|v.0).unwrap_or(snapshot.pos),station))
        },_=>None};
        let configuration=w.maintenance.get(&ship).copied();
        let latest=if reservation.is_some() {space::remove_ship(&mut w,from,ship)
            .ok_or_else(||Failure::Pending("Your committed transition lost its ship and needs reconciliation.".into()))?}else{snapshot};
        (latest,docking,configuration)
    };
    if let Some(marker)=marker {
        let mut latest=latest;
        let lethal=latest.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6);
        if !lethal {inflight::apply_profile(&mut latest,profile,combat::now_ms());}
        let final_state=crate::losses::pending::finish(server,&server.store,marker,latest).await.map_err(Failure::Pending)?;
        drop(reservation);
        return Ok(Arrived {place:Place {station:Some(final_state.station_id),system:final_state.solar_system_id,
            constellation:final_state.constellation_id,region:final_state.region_id,at:Vec3::ZERO},ship_id:final_state.ship_id,
            docking:if final_state.ship_id==ship {docking}else{None}});
    }
    let through=if let Request::Gate{to_gate,..}=request {Some(to_gate)}else{None};
    let arrived=super::completion::finish_space(server,space_marker.unwrap(),latest,profile,through).await?;
    if let Some(r)=reservation.as_mut(){r.system=arrived.place.system;}
    if arrived.ship_id==ship && arrived.place.station.is_none() {
        let mut w=server.world.lock();
        if matches!(request,Request::Admin{..}) && from==place.system {
            if let Some(config)=configuration {w.maintenance.insert(ship,config);}
        }
        if let Some(gate)=through {
            let tick=w.parks[&place.system].tick;let flash=space::special_fx(tick,ship,Some(gate),"effects.JumpIn");
            for other in w.in_space(place.system).into_iter().filter(|c|*c!=client) {space::broadcast(&w,place.system,Some(other),vec![flash.clone()]);}
        }
    }
    drop(reservation);
    Ok(arrived)
}
