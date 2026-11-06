//! Original quote/RentOffice contract with one custody and payment transaction.
use eve_marshal::Value;
use crate::store::{self,Db,StoreError};
use crate::{items::{ItemTxn,NewItem},services::{Ctx,Outcome}};
use crate::svc::wallet::{self,Entry,Transfer,REF_OFFICE_RENTAL_FEE};

mod slots;
pub(crate) use slots::{ensure_schema,ensure_slots_locked};

struct Rental { corporation:i64, cost:f64, slots:i64 }
enum Error { Db(StoreError), Refused(Outcome) }
impl From<Outcome> for Error {fn from(value:Outcome)->Self{Self::Refused(value)}}
impl From<StoreError> for Error {fn from(error:StoreError)->Self{Self::Db(error)}}
impl Error {
    fn outcome(self)->Outcome {match self {
        Self::Refused(out)=>out,
        Self::Db(error)=>{tracing::error!("office rental transaction: {}",crate::logging::chain(&error));denied("The office rental could not be completed. Check your corporation's offices before retrying.")}
    }}
}
type Result<T> = std::result::Result<T,Error>;

fn message(name:&str)->Outcome {Outcome::UserError{msg:name.into(),args:vec![]}}
fn denied(reason:&str)->Outcome {Outcome::UserError{msg:"RentingOfficeRequestDenied".into(),args:vec![("reason".into(),Value::text(reason))]}}

/// Everything a quote or rental is decided by, read inside the store
/// operation that acts on it.
fn context(ctx:&Ctx<'_>,db:&Db,station:i64,quote:bool)->Result<Rental> {
    let pilot=ctx.session.char_id.filter(|id|*id>0 && *id<=i32::MAX as i64)
        .ok_or_else(||denied("Select a character first."))?;
    if !(1..=i32::MAX as i64).contains(&station) || ctx.session.station_id!=Some(station) {
        return Err(denied("You must be docked at the station whose office you are renting.").into());
    }
    let Some(character)=db.table("character_").get_id(pilot) else {return Err(denied("That character no longer exists.").into());};
    let corporation=character.int("corporationID").filter(|corp|*corp>=100_000_000)
        .ok_or_else(||denied("Only player corporations can rent offices."))?;
    if ctx.session.corp_id!=Some(corporation) || character.int("stationID")!=Some(station) {
        return Err(denied("Your corporation or location has changed. Try again after the session change.").into());
    }
    if crate::losses::pending::blocked_character(db,pilot)?
        || crate::losses::journal::blocked_character_in_memory(ctx.world,pilot)
        || crate::travel::blocked_character_in_memory(ctx.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {
        return Err(denied("Your ship loss or station arrival must finish first.").into());
    }
    let place=db.table("staStations").get_id(station)
        .ok_or_else(||denied("That station no longer exists."))?;
    let cost=place.int("officeRentalCost").filter(|cost|*cost>=0).ok_or_else(||denied("That station has no valid office rental fee."))? as f64;
    let station_type=place.int("stationTypeID").ok_or_else(||denied("That station has no office configuration."))?;
    let slots=crate::sde::table("staStationTypes").get("stationTypeID",station_type)
        .and_then(|r|r.int("officeSlots")).map_or(0,|slots|slots.max(0));
    let corp=db.table("corporation").get_id(corporation)
        .ok_or_else(||denied("Your corporation no longer exists."))?;
    if corp.int("deleted")!=Some(0) || !corp.float("balance").is_some_and(f64::is_finite) {return Err(denied("Your corporation wallet is unavailable.").into());}
    let roles=db.table("chrCorporationRoles").get_id(pilot).and_then(|r|r.int("rolesAtAll")).unwrap_or(0);
    if corp.int("ceoID")!=Some(pilot) && roles & crate::session::CORP_ROLE_DIRECTOR==0 {
        return Err((if quote {message("RentingOfficeQuotesOnlyGivenToActiveCEOsOrEquivale")} else {denied("Only the current CEO or a director may rent a corporation office.")}).into());
    }
    Ok(Rental{corporation,cost,slots})
}

pub(super) async fn quote(ctx:&Ctx<'_>,station:i64,args:&[Value])->Outcome {
    if !args.is_empty() {return denied("An office quote takes no arguments.");}
    let result=ctx.store.read(|db|context(ctx,db,station,true));
    result.map_or_else(Error::outcome,|rental|Outcome::Value(Value::Double(rental.cost)))
}

pub(super) async fn rent(ctx:&Ctx<'_>,station:i64,args:&[Value])->Outcome {
    let [value]=args else {return denied("Provide the quoted office rental fee.");};
    let Some(offered)=value.as_f64().filter(|cost|cost.is_finite() && *cost>=0.0) else {return denied("The offered office rental fee must be a finite, nonnegative number.");};
    let result=ctx.store.write(|tx|->Result<_> {
        let rental=context(ctx,tx.db(),station,false)?;
        if offered<rental.cost {return Err(Outcome::UserError{msg:"RentingAnOfficeCostsMore".into(),args:vec![("amount".into(),Value::Double(rental.cost))]}.into());}
        let offices:Vec<Option<i64>>=tx.table("crpOffices").find(&["stationID"],&[store::Value::Int(station)]).map(|o|o.int("corporationID")).collect();
        if offices.contains(&Some(rental.corporation)) {
            return Err(message("RentingYouHaveAnOfficeHere").into());
        }
        if offices.len() as i64>=rental.slots {return Err(message("NoOfficesAreAvailableForRenting").into());}
        ensure_slots_locked(tx,station)?;
        let db=tx.db();
        let slot=db.table("rentalInfo").find(&["stationID"],&[store::Value::Int(station)])
            .filter(|r|r.int("typeID")==Some(26) && r.int("renterID")==Some(0) && r.int("publiclyAvailable")!=Some(0))
            .filter(|r|r.int("slotNumber").is_some_and(|n|(1..=rental.slots).contains(&n)))
            .find(|r|db.table("crpOffices").find_one(&["stationID","rentalSlot"],&[store::Value::Int(station),r.value("slotNumber").clone()]).is_none())
            .map(|r|(r.id,r.int("slotNumber").unwrap_or(0))).ok_or_else(||message("NoOfficesAreAvailableForRenting"))?;
        let mut items=ItemTxn::new(tx);
        let office=items.create(NewItem{type_id:27,owner_id:rental.corporation,location_id:station,flag:0,quantity:1,singleton:true})?;
        let tx=items.tx();
        let folder=tx.nextval("office_folder_seq")?;
        tx.insert("crpOffices",&[("corporationID",store::Value::Int(rental.corporation)),("stationID",store::Value::Int(station)),("itemID",store::Value::Int(office)),
            ("typeID",store::Value::Int(27)),("officeFolderID",store::Value::Int(folder)),("rentalSlot",store::Value::Int(slot.1))])?;
        let start=crate::bulkdata::filetime_now();
        let due=start+30*24*60*60*10_000_000_i64;
        tx.update("rentalInfo",slot.0,&[("renterID",store::Value::Int(rental.corporation)),("startDate",store::Value::Int(start)),("rentPeriodInDays",store::Value::Int(30)),
            ("periodCost",store::Value::Float(rental.cost)),("balanceDueDate",store::Value::Int(due)),("billID",store::Value::Int(0)),("discontinue",store::Value::Int(0))])?;
        let entry=Entry{ref_type:REF_OFFICE_RENTAL_FEE,payer:rental.corporation,payee:0,arg_id1:"",reason:""};
        wallet::post(tx,rental.corporation,-rental.cost,&entry).map_err(|error|match error {
            Transfer::NotEnough{need,have}=>Outcome::not_enough_money(need,have).into(),
            Transfer::NoSuchOwner=>denied("Your corporation has no wallet.").into(),Transfer::Db(error)=>Error::Db(error),
        })?;
        Ok((rental,office,folder))
    });
    match result {
        Ok((rental,office,folder))=>{
            {
                // The original three-argument event has no station ID and
                // updates the recipient's current-station office cache.
                let w=ctx.world.lock();let clients=w.players().into_iter()
                    .filter(|p|p.docked && p.station_id==Some(station))
                    .flat_map(|p|w.clients_of(p.char_id)).collect::<Vec<_>>();
                w.notify(&clients,"OnOfficeRentalChanged",Value::Tuple(vec![Value::Int(rental.corporation),Value::Int(office),Value::Int(folder)]));
            }
            wallet::notify_balance(ctx,rental.corporation).await;
            tracing::info!(corporation=rental.corporation,station,office,cost=rental.cost,"corporation rented office");
            Outcome::Value(Value::Int(office))
        }
        Err(why)=>why.outcome(),
    }
}
