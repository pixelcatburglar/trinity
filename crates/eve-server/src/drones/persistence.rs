//! Durable launched-drone state. Inventory remains authoritative: an item
//! transition invalidates its launch generation, and snapshots never create one.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub(super) struct Identity { pub(super) generation: i64, revision: i64 }

#[derive(Clone, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    motion: Option<crate::ballpark::Motion>,
    position: [f64; 3],
    velocity: [f64; 3],
    controller: Option<i64>,
    controller_owner: i64,
    assignment: Option<Assignment>,
    order: Order,
    activity: i64,
    mined: bool,
    settings: Settings,
    control_range: f64,
}

impl State {
    fn initial(owner: i64, controller: Option<i64>, pos: Vec3, settings: Settings, range: f64) -> Self {
        Self { motion:None, position: [pos.x,pos.y,pos.z], velocity:[0.0;3], controller, controller_owner:owner,
            assignment:None, order:Order::Idle, activity:activity::IDLE, mined:false, settings, control_range:range }
    }
    fn capture(ship: &Ship,tick:i64) -> Self {
        let d=ship.drone.as_ref().unwrap();
        Self { motion:d.restored_motion.clone().or_else(||Some(ship.motion(tick))), position:[ship.pos.x,ship.pos.y,ship.pos.z], velocity:[ship.vel.x,ship.vel.y,ship.vel.z],
            controller:d.controller, controller_owner:d.controller_owner, assignment:d.assignment,
            order:d.order.clone(), activity:d.activity, mined:d.mined, settings:d.settings, control_range:d.control_range }
    }
    fn valid(&self) -> bool {
        Vec3::new(self.position[0],self.position[1],self.position[2]).is_finite()
            && Vec3::new(self.velocity[0],self.velocity[1],self.velocity[2]).is_finite()
            && self.control_range.is_finite() && self.control_range>=0.0
    }
}

/// The launch records, their generations and the trigger that drops a
/// record when its item moves are defined in the store's schema.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(),StoreError> {Ok(())}

pub(super) fn begin(tx:&mut Tx<'_>,id:i64,owner:i64,system:i64,controller:Option<i64>,pos:Vec3,
    settings:Settings,range:f64) -> Result<Identity,StoreError> {
    let state=serde_json::to_string(&State::initial(owner,controller,pos,settings,range)).unwrap();
    let record=[("owner_id",store::Value::Int(owner)),("system_id",store::Value::Int(system)),("state",store::Value::Text(state.into()))];
    // A relaunch of a recorded drone starts a new generation.
    let row=match tx.table("launched_drone_state").get_id(id).map(|r|r.id) {
        Some(row)=>{
            let generation=tx.nextval("launched_drone_generation")?;
            let mut sets=vec![("generation",store::Value::Int(generation)),("revision",store::Value::Int(0))];
            sets.extend(record);
            tx.update("launched_drone_state",row,&sets)?;
            row
        }
        None=>{
            let mut sets=vec![("drone_id",store::Value::Int(id))];
            sets.extend(record);
            tx.insert("launched_drone_state",&sets)?
        }
    };
    let generation=tx.table("launched_drone_state").row(row).and_then(|r|r.int("generation")).unwrap_or(0);
    Ok(Identity{generation,revision:0})
}

#[derive(Clone)]
struct Snapshot { system:i64, tick:i64, ship:Ship, identity:Identity }

fn capture(world:&World, ids:Option<&[i64]>) -> Vec<Snapshot> {
    let mut w=world.lock(); let mut snapshots=Vec::new();
    for (system,park) in &mut w.parks {
        for ship in park.ships.values_mut() {
            if ids.is_some_and(|ids|!ids.contains(&ship.id)) || !ship.pos.is_finite() || !ship.vel.is_finite() {continue}
            let Some(d)=ship.drone.as_mut().filter(|d|!d.pending_bay) else {continue};
            let Some(identity)=d.persistence.as_mut() else {continue};
            identity.revision=identity.revision.saturating_add(1);
            let identity=*identity;
            snapshots.push(Snapshot{system:*system,tick:park.tick,ship:ship.clone(),identity});
        }
    }
    snapshots
}

async fn save(store:&Store,world:&World,snapshot:Snapshot) -> Result<bool,StoreError> {
    let id=snapshot.ship.id;
    let owner=snapshot.ship.drone.as_ref().unwrap().owner;
    store.write(|tx| {
        // The inventory item must still be this owner's drone in space here.
        let custody=tx.table("entity").get_id(id).is_some_and(|e|e.int("ownerID")==Some(owner)
            && e.int("locationID")==Some(snapshot.system) && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)
            && e.int("typeID").is_some_and(|t|crate::sde::get().types_in_category(18).contains(&(t as i32))));
        if !custody {return Ok(false)}
        let live=world.lock().parks.get(&snapshot.system).and_then(|p|p.ships.get(&id))
            .and_then(|s|s.drone.as_ref()).is_some_and(|d|!d.pending_bay && d.owner==owner
                && d.persistence.is_some_and(|p|p.generation==snapshot.identity.generation));
        if !live {return Ok(false)}
        let state=serde_json::to_string(&State::capture(&snapshot.ship,snapshot.tick)).unwrap();
        let Some(record)=tx.table("launched_drone_state").get_id(id).filter(|r|r.int("generation")==Some(snapshot.identity.generation)
            && r.int("revision").is_some_and(|r|r<snapshot.identity.revision) && r.int("owner_id")==Some(owner)
            && r.int("system_id")==Some(snapshot.system)).map(|r|r.id) else {return Ok(false)};
        tx.update("launched_drone_state",record,&[("revision",store::Value::Int(snapshot.identity.revision)),("state",store::Value::Text(state.into()))])?;
        if let Some(item)=tx.table("entity").get_id(id).map(|r|r.id) {
            tx.update("entity",item,&[("x",store::Value::Float(snapshot.ship.pos.x)),("y",store::Value::Float(snapshot.ship.pos.y)),
                ("z",store::Value::Float(snapshot.ship.pos.z))])?;
        }
        if let Some(f)=&snapshot.ship.fight {crate::svc::inflight::write_vitals(tx,id,f,false)?;}
        Ok(true)
    })
}

pub async fn checkpoint(world:&World,store:&Store) {
    for snapshot in capture(world,None) {
        if let Err(e)=save(store,world,snapshot).await {warn!("saving launched drone: {}",crate::logging::chain(&e));}
    }
}

pub(super) async fn ordered(ctx:&Ctx<'_>,ids:&[i64]) {
    for snapshot in capture(ctx.world,Some(ids)) {
        if let Err(e)=save(ctx.store,ctx.world,snapshot).await {warn!("saving drone order: {e}");}
    }
}

pub(super) fn vitals(db:&Db,ids:&[i64]) -> HashMap<i64,Vec<(i32,f64)>> {
    let attributes=db.table("entity_attributes");
    let mut out=HashMap::<i64,Vec<(i32,f64)>>::new();
    for id in ids {
        if out.contains_key(id) {continue}
        for attribute in [attr::SHIELD_CHARGE,attr::CHARGE,attr::ARMOR_DAMAGE,attr::DAMAGE] {
            let Some(row)=attributes.get(&[store::Value::Int(*id),store::Value::Int(attribute.into())]) else {continue};
            if let Some(v)=row.float("valueFloat").or_else(||row.float("valueInt")).filter(|v|v.is_finite()) {out.entry(*id).or_default().push((attribute,v));}
        }
    }
    out
}

pub(super) fn apply_vitals(ship:&mut Ship,values:Option<&Vec<(i32,f64)>>,now:i64) {
    let Some(f)=ship.fight.as_mut() else {return};
    for (a,v) in values.into_iter().flatten() {
        match *a {
            attr::SHIELD_CHARGE=>f.vitals.shield.set(now,*v), attr::CHARGE=>f.vitals.capacitor.set(now,*v),
            attr::ARMOR_DAMAGE=>f.vitals.armor_damage=v.max(0.0), attr::DAMAGE=>f.vitals.hull_damage=v.max(0.0), _=>{}
        }
    }
}

/// Existing drones from databases predating this table keep their inventory
/// position and damage. Their unrecorded orders cannot be inferred.
pub async fn load(ctx:&Ctx<'_>,system:i64) -> Result<Vec<Ship>,StoreError> {
    let rows=ctx.store.read(|db| {
        let drones=crate::sde::get().types_in_category(18);
        let (characters,corporations)=(db.table("character_"),db.table("corporation"));
        let mut rows:Vec<(i64,i64,i64,i64,Option<i64>)>=db.table("entity").find(&["locationID","flag"],&[store::Value::Int(system),store::Value::Int(0)])
            .filter(|e|e.int("quantity")==Some(1) && e.int("singleton")==Some(1) && e.int("typeID").is_some_and(|t|drones.contains(&(t as i32))))
            .filter_map(|e|{
                let owner=e.int("ownerID")?;
                let corp=characters.get_id(owner).and_then(|c|c.int("corporationID"));
                let alliance=corp.and_then(|c|corporations.get_id(c)).and_then(|co|co.int("allianceID"));
                Some((e.int("itemID")?,e.int("typeID")?,owner,corp.unwrap_or(0),alliance))
            }).collect();
        rows.sort_by_key(|r|r.0);
        rows
    });
    let type_ids=rows.iter().map(|r|r.1).collect::<Vec<_>>();
    let types=ctx.store.read(|db|npc::load_types(db,&type_ids))?;
    let mut out=Vec::new(); let now=combat::now_ms();let mut rng=combat::Rng::seeded(now as u64);
    for (id,type_id,owner,corp_id,alliance_id) in rows {
        let Some(ty)=types.get(&type_id) else {continue};
        let (settings,loadout)=ctx.store.read(|db|{
            let settings=Settings::load(db,owner);
            let owner_ship=db.table("character_").get_id(owner).and_then(|c|c.int("shipID"))
                .filter(|ship|db.table("entity").get_id(*ship).is_some_and(|e|e.int("ownerID")==Some(owner)));
            crate::dogma::Loadout::load_with_drones(db,ctx.dogma,owner,owner_ship,&Default::default(),&[id]).map(|loadout|(settings,loadout))
        })?;
        let attributes=loadout.find(id).map(|i|loadout.attributes(i));let ty=ty.with_attributes(attributes.as_ref());
        let range=loadout.character().map_or(20_000.0,|c|loadout.value(c,458).max(0.0));
        let ship=ctx.store.write(|tx|->Result<Option<Ship>,StoreError> {
            let Some(item)=tx.table("entity").get_id(id).filter(|e|e.int("ownerID")==Some(owner) && e.int("locationID")==Some(system)
                && e.int("typeID")==Some(type_id) && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)) else {return Ok(None)};
            let pos=Vec3::new(item.float("x").unwrap_or(0.0),item.float("y").unwrap_or(0.0),item.float("z").unwrap_or(0.0));
            if !pos.is_finite() {return Ok(None)}
            let record=tx.table("launched_drone_state").get_id(id).filter(|r|r.int("owner_id")==Some(owner) && r.int("system_id")==Some(system));
            let recorded=match record {
                Some(row)=>{
                    match row.text("state").and_then(|state|serde_json::from_str::<State>(state).ok()).filter(State::valid) {
                        Some(state)=>Some((Identity{generation:row.int("generation").unwrap_or(0),revision:row.int("revision").unwrap_or(0)},state)),
                        None=>{
                            warn!(drone=id,"invalid durable drone state; preserving inventory position and vitals");
                            None
                        },
                    }
                },
                None=>None,
            };
            let state=recorded.as_ref().map_or_else(||State::initial(owner,None,pos,settings,range),|(_,state)|state.clone());
            let stored=vitals(tx.db(),&[id]);
            let pilot=Pilot{type_id,group_id:ty.group_id,name:ty.name.clone(),char_id:owner,corp_id,alliance_id,security:0.0,bounty:0.0};
            let mut ship=make(&ty,id,pilot,0,state.controller.unwrap_or(0),pos,now,&mut rng);
            ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);ship.command=Command::Stop;
            apply_vitals(&mut ship,stored.get(&id),now);
            if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {return Ok(None)}
            // A drone without a valid record starts a new generation.
            let identity=match recorded {
                Some((identity,_))=>identity,
                None=>begin(tx,id,owner,system,None,pos,settings,range)?,
            };
            let d=ship.drone.as_mut().unwrap();d.persistence=Some(identity);d.controller=state.controller;d.controller_owner=state.controller_owner;
            d.controller_client=0;d.assignment=state.assignment;d.order=state.order;d.activity=state.activity;d.mined=state.mined;
            d.restored_motion=state.motion;
            d.settings=settings;d.control_range=range;d.next_think_ms=0;
            Ok(Some(ship))
        })?;
        out.extend(ship);
    }
    Ok(out)
}

/// Revalidate the inventory immediately before making loaded drones live. A
/// scoop, loss, ownership transfer or relaunch during profile loading wins.
pub async fn install(ctx:&Ctx<'_>,system:i64,ships:Vec<Ship>) -> Result<(),StoreError> {
    if ships.is_empty() {return Ok(())}
    // The drones go live while the store is held, so an inventory change
    // either came first and is seen here, or comes after and sees them live.
    ctx.store.read(|db| {
        let mut ids:Vec<i64>=ships.iter().map(|s|s.id).collect();
        ids.sort_unstable();ids.dedup();
        let (entity,records)=(db.table("entity"),db.table("launched_drone_state"));
        let mut ships:HashMap<i64,Ship>=ships.into_iter().map(|s|(s.id,s)).collect();
        let stored=vitals(db,&ids);
        let now=combat::now_ms();let mut valid=Vec::new();
        for row in ids.iter().filter_map(|id|entity.get_id(*id)).filter(|e|e.int("locationID")==Some(system) && e.int("flag")==Some(0)
            && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)) {
            let Some(id)=row.int("itemID") else {continue};
            let Some(mut ship)=ships.remove(&id) else {continue};
            let d=ship.drone.as_mut().unwrap();
            let Some(record)=records.get_id(id).filter(|r|d.persistence.is_some_and(|p|Some(p.generation)==r.int("generation"))) else {continue};
            if Some(d.owner)!=row.int("ownerID") || Some(ship.pilot.type_id)!=row.int("typeID") {continue}
            let Some(Ok(state))=record.text("state").map(serde_json::from_str::<State>) else {continue};
            if !state.valid() {continue}
            ship.pos=Vec3::new(row.float("x").unwrap_or(0.0),row.float("y").unwrap_or(0.0),row.float("z").unwrap_or(0.0));
            if !ship.pos.is_finite() {continue}
            ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);
            d.persistence=Some(Identity{generation:record.int("generation").unwrap_or(0),revision:record.int("revision").unwrap_or(0)});
            d.client_id=0;d.controller_client=0;ship.client_id=0;
            d.controller=state.controller;d.controller_owner=state.controller_owner;d.assignment=state.assignment;
            d.order=state.order;d.activity=state.activity;d.mined=state.mined;
            d.restored_motion=(state.position==[ship.pos.x,ship.pos.y,ship.pos.z]).then_some(state.motion).flatten();
            apply_vitals(&mut ship,stored.get(&id),now);
            if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {continue}
            valid.push(ship);
        }
        let mut w=ctx.world.lock();let mut out=Out::default();let mut inserted=Vec::new();
        if let Some(park)=w.parks.get_mut(&system) {
            for ship in valid {
                let id=ship.id;
                if let std::collections::btree_map::Entry::Vacant(entry)=park.ships.entry(id) {
                    entry.insert(ship);
                    inserted.push(id);
                }
            }
        }
        reconcile_into(&mut w,&mut out);
        if let Some(park)=w.parks.get_mut(&system) {
            for id in inserted {
                // These balls have not been published yet; their first payload
                // carries the restored phase and needs no separate reset.
                if let Some(d)=park.ships.get_mut(&id).and_then(|s|s.drone.as_mut()) {d.motion_resync=false;}
                for e in space::add_ball_entries(park,id) {out.destiny(system,e);}
            }
        }
        out.deliver(&w);
    });
    Ok(())
}

/// Sessions are ephemeral. Bind to current piloted hulls, reject capsules,
/// empty hulls and revoked fleet delegation, and never reuse a saved client ID.
pub fn reconcile(world:&mut crate::world::WorldState) {
    let mut out=Out::default();
    reconcile_into(world,&mut out);
    out.deliver(world);
}

fn reconcile_into(world:&mut crate::world::WorldState,out:&mut Out) {
    for park in world.parks.values_mut() {
        let pilots:HashMap<i64,(i64,i64)>=park.ships.values().filter(|s|s.is_pilot() && s.pilot.group_id!=29)
            .map(|s|(s.pilot.char_id,(s.id,s.client_id))).collect();
        for ship in park.ships.values_mut() {
            let Some(d)=ship.drone.as_mut().filter(|d|!d.pending_bay) else {continue};
            let old=(d.client_id,d.controller,d.controller_owner,d.controller_client);
            d.client_id=pilots.get(&d.owner).map_or(0,|p|p.1);ship.client_id=d.client_id;
            let delegated=d.controller_owner!=d.owner && ship.pilot.group_id==GROUP_FIGHTER
                && world.gangs.of(d.owner).is_some() && world.gangs.of(d.owner)==world.gangs.of(d.controller_owner);
            if d.controller_owner!=d.owner && !delegated {
                d.restored_motion=None;d.order=Order::Idle;d.activity=activity::IDLE;
            }
            let selected=if delegated {pilots.get(&d.controller_owner).copied()} else {None}.or_else(||pilots.get(&d.owner).copied());
            let owner=selected.and_then(|(id,_)|pilots.iter().find(|(_,p)|p.0==id).map(|(owner,_)|*owner));
            d.controller=selected.map(|p|p.0);d.controller_client=selected.map_or(0,|p|p.1);d.controller_owner=owner.unwrap_or(d.owner);
            if d.owner==OWNER_SYSTEM {d.controller=None;d.controller_client=0;d.client_id=0;ship.client_id=0;}
            if d.assignment.is_some_and(|a|!pilots.get(&a.pilot()).is_some_and(|p|p.0==a.ship())
                || world.gangs.of(d.owner).is_none() || world.gangs.of(d.owner)!=world.gangs.of(a.pilot())) {clear_assignment(ship.id,d,out);}
            if old!=(d.client_id,d.controller,d.controller_owner,d.controller_client) {
                d.next_think_ms=0;state_note(out,ship.id,d,ship.pilot.type_id,Some(old.3));
            }
            let recovered=d.controller.and_then(|_|d.restored_motion.take());
            if let Some(motion)=recovered {
                if ship.restore_motion(&motion,park.tick) {ship.drone.as_mut().unwrap().motion_resync=true;}
            }
        }
    }
}
