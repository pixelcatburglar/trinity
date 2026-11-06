//! Original 45017 combat boosters. Only formulas established by the original
//! type/effect catalogue are admitted; unsupported boosters are never consumed.

use std::collections::BTreeMap;
use eve_marshal::Value;
use crate::{items::{ItemTxn, NewItem}, services::{Ctx, Outcome}};
use crate::store::{self, Store, StoreError, Tx};
use super::{fitting, moves::{self, MoveError}};

mod refresh;

pub const FLAG: i64 = 88;
pub const EXPIRY: i32 = 1088;
const DURATION: i32 = 330;
const SLOT: i32 = 1087;
const BIOLOGY: i32 = 3405;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target { Ship, Gunnery, Missile }

/// Original effect names encode the percentage operation and target scope.
/// Effect2432 is also on Energy Management and the original capacitor rigs.
/// No later type-ID modifier rows are used for any booster.
pub fn modifier(type_id:i64) -> Option<(i32, Target, i32, i32)> {
    Some(match type_id {
        28672 => (1472, Target::Missile, 654, 848),
        28678 => (290, Target::Gunnery, 54, 294),
        28680 => (2432, Target::Ship, 482, 1079),
        28682 => (223, Target::Ship, 37, 315),
        28684 => (298, Target::Gunnery, 158, 349),
        _ => return None,
    })
}

/// `character_boosters` and `character_booster_refresh` are part of the
/// store's schema.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

fn expiry(start:i64, duration:f64, biology_bonus:f64, level:i64)->Option<i64> {
    let duration=duration*(1.0+biology_bonus*level.clamp(0,5) as f64/100.0);
    if !duration.is_finite() || duration<=0.0 {return None;}
    let ticks=duration*10_000.0;
    if !ticks.is_finite() || ticks>=i64::MAX as f64 {return None;}
    start.checked_add(ticks.round() as i64).filter(|end|*end>start)
}

fn dirty(tx:&mut Tx<'_>,character:i64)->Result<(),StoreError> {
    tx.insert_or_ignore("character_booster_refresh",&[("character",store::Value::Int(character))])?;
    Ok(())
}

/// The character's boosters due by `now`, by item.
fn due(db:&store::Db,character:i64,now:i64)->Vec<i64> {
    let mut items:Vec<i64>=db.table("character_boosters").find(&["character"],&[store::Value::Int(character)])
        .filter(|b|b.int("expires").is_some_and(|e|e<=now)).filter_map(|b|b.int("item")).collect();
    items.sort_unstable();
    items
}

/// Run inside the transaction that checked the character still exists.
/// Each item is rechecked, so expiry never deletes an item that was moved,
/// lost, or transferred since its booster was recorded.
fn expire_locked(tx:&mut ItemTxn<'_,'_>,character:i64,now:i64)->Result<bool,StoreError> {
    let rows=due(tx.db(),character,now);
    for &id in &rows {
        let item=tx.item(id)?;
        // A newer deadline/generation is checked too.
        let due=tx.db().table("character_boosters").get_id(id)
            .filter(|b|b.int("character")==Some(character) && b.int("expires").is_some_and(|e|e<=now))
            .map(|b|(b.id,b.int("type_id")));
        let Some((row,type_id))=due else {continue};
        if item.as_ref().is_some_and(|item|item.owner_id==character && item.location_id==character
            && item.flag==FLAG && Some(item.type_id)==type_id && item.quantity==1 && item.singleton) {
            tx.destroy(id)?;
        } else {
            tx.tx().delete("character_boosters",row)?;
        }
    }
    if !rows.is_empty() {dirty(tx.tx(),character)?;}
    Ok(!rows.is_empty())
}

pub async fn consume(ctx:&Ctx<'_>,args:&[Value])->Outcome {
    let [arg]=args else {return Outcome::custom("Choose one booster to consume.");};
    let Some(id)=arg.as_i64().filter(|id|*id>0 && *id<=i32::MAX as i64) else {return Outcome::custom("Invalid booster item.");};
    let Some(character)=ctx.session.char_id else {return Outcome::custom("Select a character first.");};
    let _inventory=super::pvptrade::inventory_guard(ctx.world,character).await;
    let result:Result<_,Error>=ctx.store.write(|t| {
        let mut tx=ItemTxn::new(t);
        if !super::login::lock_character_lifecycle(tx.db(),character)? {return Err(Error::Refused("That character no longer exists."));}
        let row=tx.db().table("character_").get_id(character).map(|r|(r.int("shipID"),r.int("stationID"),r.int("solarSystemID")))
            .ok_or(Error::Refused("That character no longer exists."))?;
        if crate::losses::pending::blocked_character(tx.db(),character)?
            || crate::losses::journal::blocked_character_in_memory(ctx.world,character)
            || crate::travel::blocked_character_in_memory(ctx.world,character) {
            return Err(Error::Refused("Your ship loss or station arrival must finish before consuming a booster."));
        }
        if row.0.filter(|id|*id>0)!=ctx.session.ship_id
            || row.1.filter(|id|*id>0)!=ctx.session.station_id.filter(|id|*id>0)
            || row.2.filter(|id|*id>0)!=ctx.session.solar_system_id2 {
            return Err(Error::Refused("Your location changed. Try again after the session change."));
        }
        let Some(item)=tx.item(id)? else {return Err(Error::Refused("That booster no longer exists."));};
        if item.owner_id!=character || item.category_id!=20 || item.quantity<1 {return Err(Error::Refused("That is not your booster."));}
        let sde=crate::sde::get();
        let Some(typ)=sde.type_(item.type_id) else {return Err(Error::Refused("That item is not a booster."));};
        if typ.group_id!=303 {return Err(Error::Refused("That item is not a booster."));}
        let Some((effect,..))=modifier(item.type_id) else {return Err(Error::Refused("The original effects of this booster are not implemented yet. It has not been consumed."));};
        let effects=sde.type_effects(item.type_id);
        if effects.len()!=1 || effects[0]!=effect {return Err(Error::Refused("This booster's effect data does not match the original supported formula."));}
        super::maintenance::pending_mutation(ctx,&mut tx,&item)?;
        if !matches!(item.flag,0|4|5) || !moves::reachable(ctx,&mut tx,&item)? {return Err(Error::Refused("The booster must be in accessible cargo or your hangar."));}
        fitting::check_skills(tx.db(),character,item.type_id)?;
        let attrs=sde.type_attributes(item.type_id).iter().filter(|(a,_)|[DURATION,SLOT].contains(a)).copied().collect::<BTreeMap<_,_>>();
        let slot=attrs.get(&SLOT).copied().filter(|v|v.is_finite() && v.fract()==0.0 && *v>0.0 && *v<=i32::MAX as f64).ok_or(Error::Refused("The booster has no valid slot."))? as i64;
        let biology=super::skills::levels(tx.db(),character,&[BIOLOGY])?.get(&BIOLOGY).copied().unwrap_or(0);
        let bonus=sde.type_attribute(BIOLOGY as i64,66).filter(|_|sde.type_effects(BIOLOGY as i64).contains(&2866)).ok_or(Error::Refused("The original Biology duration data is missing."))?;
        let now=crate::bulkdata::filetime_now();
        let expires=expiry(now,*attrs.get(&DURATION).unwrap_or(&0.0),bonus,biology).ok_or(Error::Refused("The booster has no valid duration."))?;
        expire_locked(&mut tx,character,now)?;
        // A booster of a type the static data does not know does not hold the
        // slot, as joining on it left it out.
        let existing=tx.db().table("character_boosters").find(&["character","slot"],&[store::Value::Int(character),store::Value::Int(slot)]).next()
            .and_then(|b|b.int("type_id")).and_then(|t|sde.type_(t));
        if let Some(existing)=existing {
            return Err(Error::Occupied(typ.name.to_string(),existing.name.to_string()));
        }
        let consumed=if item.quantity>1 {
            tx.set_quantity(id,item.quantity-1)?;
            tx.create(NewItem{type_id:item.type_id,owner_id:character,location_id:character,flag:FLAG,quantity:1,singleton:true})?
        } else {tx.move_to(id,character,FLAG)?;tx.set_singleton(id,true)?;id};
        tx.tx().insert("character_boosters",&[("item",store::Value::Int(consumed)),("character",store::Value::Int(character)),("type_id",store::Value::Int(item.type_id)),
            ("slot",store::Value::Int(slot)),("started",store::Value::Int(now)),("expires",store::Value::Int(expires))])?;
        tx.tx().upsert("entity_attributes",&["itemID","attributeID"],
            &[("itemID",store::Value::Int(consumed)),("attributeID",store::Value::Int(i64::from(EXPIRY))),("valueInt",store::Value::Int(expires)),("valueFloat",store::Value::Null)],
            &[("valueInt",store::Value::Int(expires)),("valueFloat",store::Value::Null)])?;
        dirty(tx.tx(),character)?;
        if crate::losses::journal::blocked_character_in_memory(ctx.world,character)
            || crate::travel::blocked_character_in_memory(ctx.world,character) {
            return Err(Error::Refused("Your ship loss must finish before consuming a booster."));
        }
        let origins=tx.origins();
        Ok((tx.events(),origins))
    });
    match result {
        Ok((events,origins))=>{
            crate::items::notify(ctx.world,ctx.client_id,events);
            super::pvptrade::offers_changed(ctx.world,&origins);
            // The accepted inventory mutation is durable even if this refresh
            // fails. Its durable marker lets the heartbeat retry safely.
            if let Err(error)=refresh::character(ctx.store,ctx.dogma,ctx.world,character).await {tracing::warn!("refreshing consumed booster: {error}");}
            Outcome::Value(Value::None)
        }
        Err(Error::Occupied(first,second))=>Outcome::UserError{msg:"OnlyOneBoosterActive".into(),args:vec![("typeName".into(),Value::text(first)),("typeName2".into(),Value::text(second))]},
        Err(Error::Refused(why))=>Outcome::custom(why),
        Err(Error::Move(MoveError::Refused(why)))=>Outcome::custom(why),
        Err(Error::Move(MoveError::Reactivation{item,delay}))=>moves::reactivation_error(item,delay),
        Err(Error::Db(error))|Err(Error::Move(MoveError::Db(error)))=>{tracing::error!("consuming booster: {}",crate::logging::chain(&error));Outcome::custom("The booster could not be consumed.")}
    }
}

enum Error { Db(StoreError), Move(MoveError), Refused(&'static str), Occupied(String,String) }
impl From<StoreError> for Error {fn from(e:StoreError)->Self{Self::Db(e)}}
impl From<MoveError> for Error {fn from(e:MoveError)->Self{Self::Move(e)}}

pub async fn pulse(server:&crate::conn::Server,store:&Store) {
    if let Err(error)=expire(store,&server.world).await {tracing::warn!("expiring boosters: {error}");}
    let pending:Vec<i64>=store.read(|db|{
        let mut characters:Vec<i64>=db.table("character_booster_refresh").rows().filter_map(|r|r.int("character")).collect();
        characters.sort_unstable();characters.truncate(256);characters
    });
    for character in pending {
        let _inventory=super::pvptrade::inventory_guard(&server.world,character).await;
        if let Err(error)=refresh::character(store,&server.dogma,&server.world,character).await {tracing::warn!("refreshing expired booster for {character}: {error}");}
    }
}

pub async fn expire(store:&Store,world:&crate::world::World)->Result<(),StoreError> {
    let now=crate::bulkdata::filetime_now();
    let characters:Vec<i64>=store.read(|db|{
        let due:std::collections::BTreeSet<i64>=db.table("character_boosters").rows()
            .filter(|b|b.int("expires").is_some_and(|e|e<=now)).filter_map(|b|b.int("character")).collect();
        due.into_iter().take(256).collect()
    });
    for character in characters {
        let _inventory=super::pvptrade::inventory_guard(world,character).await;
        let events=store.write(|t|{
            let mut tx=ItemTxn::new(t);
            if !super::login::lock_character_lifecycle(tx.db(),character)? {return Ok(None);}
            expire_locked(&mut tx,character,now)?;
            Ok::<_,StoreError>(Some(tx.events()))
        })?;
        let Some(events)=events else {continue};
        let w=world.lock();let clients=w.clients_of(character);
        for event in events {w.notify(&clients,"OnItemChange",event);}
    }
    Ok(())
}
