//! A lethal hit during an accepted arrival still destroys the source hull.
//! Settle the loss before publishing the new station session. Normal ship
//! losses leave a capsule at the accepted station; pod losses use medical home.
use super::*;
use crate::svc::medical;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ArrivalDisposition {
    pub ship_id:i64,
    pub station_id:i64,
    pub solar_system_id:i64,
    pub constellation_id:i64,
    pub region_id:i64,
}

fn failed(error:StoreError)->String {
    error!("settling lethal station arrival: {}",crate::logging::chain(&error));
    "The ship's loss could not yet be settled.".into()
}

/// What a settlement leaves to publish once it has committed.
struct Settled {
    events:Vec<Value>,
    insurance:Option<InsurancePayout>,
    can:Option<loot::Can>,
    bounty_balance:Option<(i64,f64)>,
    skill_loss:Option<(String,f64)>,
    record:RecordedLoss,
}

pub(super) async fn settle_loss(server:&Server,store:&Store,station:i64,loss:&Loss)
    ->Result<ArrivalDisposition,String> {
    let pilot=loss.char_id;
    // Where the pilot ends up, and what is left to publish (None when the
    // loss was settled before).
    let (disposition,settled)=store.write(|tx| -> Result<(ArrivalDisposition,Option<Settled>),Refused> {
        let mut txn=ItemTxn::new(tx);
        let row=txn.db().table("character_").get_id(pilot).ok_or("The arriving character no longer exists.")?;
        let current=ArrivalDisposition {ship_id:row.int("shipID").unwrap_or(0),station_id:row.int("stationID").unwrap_or(0),
            solar_system_id:row.int("solarSystemID").unwrap_or(0),constellation_id:row.int("constellationID").unwrap_or(0),region_id:row.int("regionID").unwrap_or(0)};
        let hull=txn.item(loss.ship_id)?;
        if hull.is_none() {
            // A retry after a completed settlement must not create another
            // capsule, duplicate the kill record, or pay twice.
            let done=txn.db().table("killLog").rows().any(|k|k.int("victimItemID")==Some(loss.ship_id) && k.int("victimCharID")==Some(pilot));
            if done && current.ship_id!=loss.ship_id && current.ship_id>0 && current.station_id>=0 {
                if let Some(replacement)=txn.item(current.ship_id)? {
                    let location=if current.station_id>0 {current.station_id}else{current.solar_system_id};
                    let flag=if current.station_id>0 {FLAG_HANGAR}else{FLAG_IN_SPACE};
                    if replacement.category_id==6 && replacement.owner_id==pilot && replacement.location_id==location
                        && replacement.flag==flag && replacement.quantity==1 && replacement.singleton {
                        // A boarding or undock after the settlement is newer
                        // authoritative state; recovery must not require the
                        // original capsule.
                        let place=if current.station_id>0 {station_place(txn.db(),current.station_id)}else{
                            txn.db().table("mapSolarSystems").get_id(current.solar_system_id)
                                .map(|r|(r.int("solarSystemID").unwrap_or(0),r.int("constellationID").unwrap_or(0),r.int("regionID").unwrap_or(0)))
                        };
                        if place==Some((current.solar_system_id,current.constellation_id,current.region_id)) {return Ok((current,None));}
                    }
                }
            }
            return Err("The arriving hull disappeared without a settled loss.".into());
        }
        let hull=hull.unwrap();
        if current.ship_id!=loss.ship_id || current.station_id!=station || hull.owner_id!=pilot || hull.location_id!=station
            || hull.type_id!=loss.ship_type || hull.flag!=FLAG_HANGAR || !hull.singleton || hull.quantity!=1 || hull.category_id!=6 {
            return Err("The arriving ship's custody changed before loss settlement.".into());
        }
        let accepted=station_place(txn.db(),station).ok_or("The arrival station no longer exists.")?;
        if current.solar_system_id!=accepted.0 || current.constellation_id!=accepted.1 || current.region_id!=accepted.2 {
            return Err("The arriving station does not match the character's location.".into());
        }
        let before=item_snapshot(txn.db(),loss);
        let report=report_text(txn.db(),loss);
        let mut insurance=None;
        let mut can=None;
        let mut bounty_balance=None;
        let mut skill_loss=None;
        let (destination,destroyed)=if loss.capsule && !loss.unpiloted {
            let chosen=medical::home_station(txn.db(),pilot)?;
            let destination=chosen.unwrap_or(station);
            if station_place(txn.db(),destination).is_none() {return Err("The medical clone has no valid home station.".into());}
            if let Some(killer)=loss.final_blow.filter(|id|*id>=100_000_000 && *id!=pilot) {
                if pay_player_bounty(txn.tx(),pilot,killer)?>0.0 {
                    let balance=txn.db().table("character_").get_id(killer).and_then(|c|c.float("balance")).unwrap_or(0.0);
                    bounty_balance=Some((killer,balance));
                }
            }
            // A capsule should have no cargo, but malformed descendants must not
            // become orphaned items when the hull is deleted.
            txn.destroy_tree(loss.ship_id,Some(station),&[pilot])?;
            let implants=lose_implants(&mut txn,pilot)?;
            skill_loss=lose_skill_points(&mut txn,pilot)?;
            medical::replace_clone(txn.tx(),pilot,TYPE_CLONE_ALPHA,chosen)?;
            (destination,implants)
        }else{
            // Contents of a type the static data does not know are left out, as
            // joining on it did.
            let mut contents:Vec<(i64,String)>=txn.db().table("entity").find(&["locationID"],&[store::Value::Int(loss.ship_id)])
                .filter(|e|e.int("itemID")!=Some(pilot))
                .filter_map(|e|Some((e.int("itemID")?,known_type_name(e.int("typeID")?)?))).collect();
            contents.sort_by_key(|(id,_)|*id);
            let mut rng=combat::Rng::seeded(combat::now_ms() as u64^loss.ship_id as u64);
            let mut survivors=Vec::new();let mut destroyed=Vec::new();
            for (id,name) in contents {
                if rng.unit()<SURVIVING_SHARE {survivors.push(Stash::Existing(id));}
                else {destroy_tree(&mut txn,id,loss.ship_id,pilot)?;destroyed.push(name);}
            }
            can=loot::stage_can(&mut txn,loss.system,loss.pos,pilot,&survivors)?;
            txn.destroy(loss.ship_id)?;
            crate::svc::clones::ship_destroyed(txn.tx(),loss.ship_id)?;
            insurance=claim_insurance(txn.tx(),loss.ship_id,crate::bulkdata::filetime_now())?;
            (station,destroyed)
        };
        let (system,constellation,region)=station_place(txn.db(),destination).ok_or("The destination station disappeared.")?;
        let capsule=txn.create(NewItem {type_id:TYPE_CAPSULE,owner_id:pilot,location_id:destination,flag:FLAG_HANGAR,quantity:1,singleton:true})?;
        update_id(txn.tx(),"character_",pilot,&[("shipID",store::Value::Int(capsule)),("stationID",store::Value::Int(destination)),
            ("solarSystemID",store::Value::Int(system)),("constellationID",store::Value::Int(constellation)),("regionID",store::Value::Int(region))])?;
        update_id(txn.tx(),"entity",pilot,&[("locationID",store::Value::Int(destination)),("x",store::Value::Float(0.0)),
            ("y",store::Value::Float(0.0)),("z",store::Value::Float(0.0))])?;
        let blob=item_blob(txn.db(),&before);
        let marker=txn.db().table("pendingStationArrivals").get_id(loss.ship_id).filter(|p|p.int("characterID")==Some(pilot)).map(|p|p.id);
        if let Some(marker)=marker {txn.tx().delete("pendingStationArrivals",marker)?;}
        let record=stage_record(txn.tx(),loss,report,destroyed,blob)?;
        let disposition=ArrivalDisposition {ship_id:capsule,station_id:destination,solar_system_id:system,constellation_id:constellation,region_id:region};
        Ok((disposition,Some(Settled {events:txn.events(),insurance,can,bounty_balance,skill_loss,record})))
    }).map_err(|refused|refused.reason(failed))?;
    let Some(Settled {events,insurance,can,bounty_balance,skill_loss,record})=settled else {return Ok(disposition);};
    items::notify(&server.world,loss.client_id,events);
    if let Some(payout)=insurance {notify_insurance(server,loss,payout);}
    if let Some(can)=can {loot::show_can(&server.world,loss.system,can);}
    {
        let w=server.world.lock();
        if let Some((killer,balance))=bounty_balance {
            w.notify(&w.clients_of(killer),"OnAccountChange",Value::Tuple(vec![Value::bytes("cash"),Value::Int(killer),Value::Double(balance)]));
        }
        if let Some((skill,lost))=skill_loss {
            let text=format!("Your clone could not hold everything you knew: you have lost {} skill points of {skill}. Upgrade your clone at a station's medical bay to keep more.",crate::heartbeat::fmt_isk(lost));
            let (key,args)=crate::fight::notify(&text);
            w.notify(&[loss.client_id],"OnRemoteMessage",Value::Tuple(vec![Value::bytes(key),args]));
        }
    }
    deliver_record(server,store,record).await;
    Ok(disposition)
}

fn station_place(db:&Db,station:i64)->Option<(i64,i64,i64)> {
    db.table("staStations").get_id(station)
        .map(|r|(r.int("solarSystemID").unwrap_or(0),r.int("constellationID").unwrap_or(0),r.int("regionID").unwrap_or(0)))
}
