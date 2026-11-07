//! The original 45017 station-management argument and rental-row contracts.
use super::*;
use std::collections::HashSet;

const ROLE_STATION_MANAGER:i64=2048;
const OFFICE_FOLDER:i64=26;

/// Every station service the data knows, in its order.
fn service_ids()->Vec<i64> {
    crate::sde::table("staServices").rows().filter_map(|s|s.int("serviceID")).collect()
}

pub(super) fn failed(error:StoreError)->String {
    error!("station management: {}",crate::logging::chain(&error));
    "The station settings could not be read or saved.".into()
}

/// A refusal from inside a store write, which undoes the write.
struct Refusal(String);
impl From<String> for Refusal {fn from(why:String)->Self {Refusal(why)}}
impl From<&str> for Refusal {fn from(why:&str)->Self {Refusal(why.into())}}
impl From<StoreError> for Refusal {fn from(error:StoreError)->Self {Refusal(failed(error))}}

/// `dockingCostPerVolume` holds fractional ISK/m3, as the original UI
/// accepts: the store's schema already has it as double precision.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

/// Whether the caller may manage the station, by the data as it is now:
/// the caller's transaction sees nothing change before it commits.
pub(super) fn authorize(ctx:&Ctx<'_>,db:&Db,station:i64)->Refused<()> {
    let pilot=ctx.session.char_id.filter(|id|*id>0 && *id<=i32::MAX as i64).ok_or("Select a character first.")?;
    if !(1..=i32::MAX as i64).contains(&station) || ctx.session.station_id!=Some(station) {
        return Err("You must be docked at the station you are managing.".into());
    }
    if !crate::svc::login::lock_character_lifecycle(db,pilot).map_err(failed)? {return Err("Your character is no longer available.".into());}
    let character=db.table("character_").get_id(pilot).ok_or("Your character is no longer available.")?;
    let corp=character.int("corporationID").ok_or("Your corporation is unavailable.")?;
    if ctx.session.corp_id!=Some(corp) || character.int("stationID")!=Some(station) {
        return Err("Your corporation or docked station changed. Try again after the session change.".into());
    }
    if crate::losses::pending::blocked_character(db,pilot).map_err(failed)?
        || crate::losses::journal::blocked_character_in_memory(ctx.world,pilot)
        || crate::travel::blocked_character_in_memory(ctx.world,pilot)
        || crate::conn::logout::blocked_character_in_memory(ctx.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {
        return Err("Your pending arrival, travel or ship loss must finish before station management.".into());
    }
    let place=db.table("staStations").get_id(station).ok_or("That station no longer exists.")?;
    if place.int("corporationID")!=Some(corp) {return Err("Your corporation does not own this station.".into());}
    let ceo=db.table("corporation").get_id(corp).and_then(|r|r.int("ceoID"));
    let roles=crate::svc::corp::titles::effective_roles(db,pilot)[0];
    if ceo!=Some(pilot) && roles&(ROLE_STATION_MANAGER|ROLE_DIRECTOR)==0 {
        return Err("Only current station managers, directors or the CEO may manage the station.".into());
    }
    Ok(())
}

fn field<'a>(value:&'a Value,name:&str)->Option<&'a Value> {
    match value {Value::Instance{state,..}=>field(state,name),Value::Dict(values)=>values.iter().find(|(key,_)|key.as_string().as_deref()==Some(name)).map(|(_,value)|value),_=>None}
}
fn numeric(value:Option<&Value>,name:&str,min:f64,max:f64)->Refused<f64> {
    value.and_then(Value::as_f64).filter(|n|n.is_finite() && *n>=min && *n<=max)
        .ok_or_else(||format!("The station {name} is outside its allowed range."))
}
fn integer(value:Option<&Value>,name:&str,min:i64,max:i64)->Refused<i64> {
    value.and_then(Value::as_i64).filter(|n|*n>=min && *n<=max)
        .ok_or_else(||format!("The station {name} must be a valid integer."))
}
fn nullable_stamp(value:&Value)->Refused<Option<i64>> {
    if matches!(value,Value::None) {return Ok(None);}
    match value {Value::Int(stamp)|Value::Long(stamp) if *stamp>0=>Ok(Some(*stamp)),
        _=>Err("The capital timestamp must be an exact positive FILETIME or None.".into())}
}
fn stored_stamp(value:Option<String>)->Refused<Option<i64>> {
    value.filter(|v|!v.is_empty()).map(|v|v.parse::<i64>().ok().filter(|n|*n>0)
        .ok_or_else(||"The stored capital timestamp is invalid.".into())).transpose()
}

struct Rule {service:i64,standing:f64,char_min:f64,char_max:f64,corp_min:f64,corp_max:f64}
struct Modifier {service:i64,discount:f64,surcharge:f64}
struct Rental {number:i64,public:bool}
struct Settings {rules:Vec<Rule>,modifiers:Vec<Modifier>,rentals:Vec<Rental>,name:String,description:String,
    docking:f64,office:i64,take:f64,hangar:Option<i64>,capital:Option<i64>}

impl Settings {
    fn parse(args:&[Value])->Refused<Self> {
        let [rules,modifiers,rentals,name,description,docking,office,take,hangar,capital]=args else {
            return Err("Station management requires the original ten arguments.".into());
        };
        let name=name.as_string().filter(|s|!s.trim().is_empty() && s.chars().count()<=100).ok_or("The station name must contain 1 to 100 characters.")?;
        let description=description.as_string().filter(|s|s.chars().count()<=5000).ok_or("The station description may contain at most 5000 characters.")?;
        let docking=numeric(Some(docking),"docking cost",0.0,1000.0)?;
        let office=integer(Some(office),"office cost",0,i32::MAX as i64)?;
        let take=numeric(Some(take),"reprocessing fraction",0.0,1.0)?;
        let hangar=if matches!(hangar,Value::None) {None} else {
            let flag=integer(Some(hangar),"reprocessing hangar",0,i32::MAX as i64)?;
            if crate::svc::corphangar::division(flag).is_none(){return Err("Choose a corporation hangar division for reprocessing.".into());}
            Some(flag)
        };
        let capital=nullable_stamp(capital)?;
        let Value::Dict(rows)=rules else{return Err("Station access rules must be a dictionary.".into());};
        let mut checked_rules=Vec::new();let mut seen=HashSet::new();
        for (key,row) in rows {
            let service=integer(Some(key),"service ID",1,i32::MAX as i64)?;
            if !seen.insert(service) || field(row,"serviceID").is_some_and(|v|v.as_i64()!=Some(service)) {return Err("Station service rules have duplicate or mismatched identities.".into());}
            let get=|key|numeric(field(row,key),key,-10.0,10.0);
            let rule=Rule{service,standing:get("minimumStanding")?,char_min:get("minimumCharSecurity")?,char_max:get("maximumCharSecurity")?,corp_min:get("minimumCorpSecurity")?,corp_max:get("maximumCorpSecurity")?};
            if rule.char_min>rule.char_max || rule.corp_min>rule.corp_max {return Err("A station security minimum cannot exceed its maximum.".into());}
            checked_rules.push(rule);
        }
        let Some(rows)=modifiers.seq() else{return Err("Station cost modifiers must be a list.".into());};
        let mut checked_modifiers=Vec::new();seen.clear();
        for row in rows {
            let service=integer(field(row,"serviceID"),"service ID",1,i32::MAX as i64)?;
            if !seen.insert(service){return Err("Station cost modifiers repeat a service.".into());}
            checked_modifiers.push(Modifier{service,discount:numeric(field(row,"discountPerGoodStandingPoint"),"standing discount",0.0,10.0)?,surcharge:numeric(field(row,"surchargePerBadStandingPoint"),"standing surcharge",0.0,10.0)?});
        }
        let Some(rows)=rentals.seq() else{return Err("Station rentable changes must be a list.".into());};
        let mut checked_rentals=Vec::new();seen.clear();
        for row in rows {
            if integer(field(row,"typeID"),"rentable type",1,i32::MAX as i64)?!=OFFICE_FOLDER {return Err("Only original office-folder rental slots can be configured.".into());}
            let number=integer(field(row,"number"),"office slot",1,i32::MAX as i64)?;
            if !seen.insert(number){return Err("Station rentable changes repeat an office slot.".into());}
            let public=integer(field(row,"publiclyAvailable"),"office availability",0,1)?!=0;
            // All other echoed row fields are display data, never authority to
            // change a tenant, charge, bill, or lease date.
            checked_rentals.push(Rental{number,public});
        }
        Ok(Self{rules:checked_rules,modifiers:checked_modifiers,rentals:checked_rentals,name,description,docking,office,take,hangar,capital})
    }
}

pub(super) async fn update(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<()> {
    let settings=Settings::parse(args)?;
    ctx.store.write(|tx|->Result<(),Refusal> {
        authorize(ctx,tx.db(),station)?;
        let old=tx.table("staStations").get_id(station).ok_or("That station no longer exists.")?;
        let old_capital=stored_stamp(old.text("capitalStation").map(String::from))?;
        let (system,station_type)=(old.int("solarSystemID"),old.int("stationTypeID"));
        if settings.capital.is_some() && settings.capital!=old_capital {
            let eligible=(61_000_000..64_000_000).contains(&station)
                && system.and_then(|id|tx.table("mapSolarSystems").get_id(id)).is_some_and(|s|s.int("sovereigntyLevel").is_some_and(|level|level>=3));
            if !eligible{return Err("Only an outpost in a system with sovereignty level three can be designated a capital.".into());}
        }
        let known:HashSet<i64>=service_ids().into_iter().collect();
        if settings.rules.iter().map(|r|r.service).chain(settings.modifiers.iter().map(|m|m.service)).any(|id|!known.contains(&id)) {return Err("That station service does not exist.".into());}
        crate::svc::corp::ensure_office_slots_locked(tx,station)?;
        // Reconciliation retains historical leased rows, including slots outside
        // a subsequently reduced capacity. They remain readable, but cannot be
        // enabled as currently rentable slots by an echoed management row.
        let capacity=station_type.and_then(|t|crate::sde::table("staStationTypes").get("stationTypeID",t)).and_then(|t|t.int("officeSlots")).unwrap_or(0).max(0);
        for rental in &settings.rentals {
            if rental.number>capacity{return Err("That office rental slot lies outside the station's current capacity.".into());}
            let slot=tx.table("rentalInfo").get(&[int(station),int(rental.number)]).filter(|r|r.int("typeID")==Some(OFFICE_FOLDER)).map(|r|r.id);
            let Some(slot)=slot else{return Err("That office rental slot is no longer available for configuration.".into());};
            tx.update("rentalInfo",slot,&[("publiclyAvailable",int(rental.public.into()))])?;
        }
        let capital=settings.capital.map_or(store::Value::Null,|stamp|text(&stamp.to_string()));
        let row=tx.table("staStations").get_id(station).ok_or("That station no longer exists.")?.id;
        tx.update("staStations",row,&[("stationName",text(&settings.name)),("description",text(&settings.description)),("dockingCostPerVolume",float(settings.docking)),
            ("officeRentalCost",int(settings.office)),("reprocessingStationsTake",float(settings.take)),
            ("reprocessingHangarFlag",settings.hangar.map_or(store::Value::Null,int)),("capitalStation",capital)])?;
        for table in ["mapDenormalize","eveNames"] {
            if let Some(row)=tx.table(table).get_id(station).map(|r|r.id) {tx.update(table,row,&[("itemName",text(&settings.name))])?;}
        }
        for r in settings.rules {
            let values=[("minimumStanding",float(r.standing)),("minimumCharSecurity",float(r.char_min)),("maximumCharSecurity",float(r.char_max)),
                ("minimumCorpSecurity",float(r.corp_min)),("maximumCorpSecurity",float(r.corp_max))];
            let mut sets=vec![("stationID",int(station)),("serviceID",int(r.service))];sets.extend(values.iter().cloned());
            tx.upsert("stationServiceRules",&["stationID","serviceID"],&sets,&values)?;
        }
        for m in settings.modifiers {
            let values=[("discountPerGoodStandingPoint",float(m.discount)),("surchargePerBadStandingPoint",float(m.surcharge))];
            let mut sets=vec![("stationID",int(station)),("serviceID",int(m.service))];sets.extend(values.iter().cloned());
            tx.upsert("stationServiceRules",&["stationID","serviceID"],&sets,&values)?;
        }
        Ok(())
    }).map_err(|Refusal(why)|why)?;
    info!(station,"station management settings updated");Ok(())
}

pub(super) async fn rentables(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<Value> {
    if !args.is_empty(){return Err("GetRentableItems takes no arguments.".into());}
    ctx.store.write(|tx|->Result<Value,Refusal> {
        authorize(ctx,tx.db(),station)?;
        crate::svc::corp::ensure_office_slots_locked(tx,station)?;
        let mut rows:Vec<_>=tx.table("rentalInfo").find(&["stationID"],&[int(station)]).filter(|r|r.int("typeID")==Some(OFFICE_FOLDER)).collect();
        rows.sort_by_key(|r|r.int("slotNumber"));
        let positive=|r:&store::Row,column:&str|r.int(column).filter(|n|*n>0);
        Ok(Value::List(rows.iter().map(|row|key_val(vec![
            ("number",Value::Int(row.int("slotNumber").unwrap_or(0))),("typeID",Value::Int(row.int("typeID").unwrap_or(0))),
            ("rentedToID",positive(row,"renterID").map_or(Value::None,Value::Int)),
            ("publiclyAvailable",Value::Int(row.int("publiclyAvailable").unwrap_or(0))),
            ("startDate",positive(row,"startDate").map_or(Value::None,Value::Long)),
            ("rentPeriodInDays",Value::Int(row.int("rentPeriodInDays").unwrap_or(0))),
            ("periodCost",Value::Double(row.float("periodCost").unwrap_or(0.0))),
            ("balanceDueDate",positive(row,"balanceDueDate").map_or(Value::None,Value::Long)),
        ])).collect()))
    }).map_err(|Refusal(why)|why)
}

pub(super) async fn details(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<Value> {
    if args.len()!=1 || args[0].as_i64()!=Some(station) {return Err("Request the details of your bound station.".into());}
    ctx.store.read(|db| {
        authorize(ctx,db,station)?;
        let row=db.table("staStations").get_id(station).ok_or("That station no longer exists.")?;
        let capital=stored_stamp(row.text("capitalStation").map(String::from))?;
        Ok(key_val(vec![("stationID",Value::Int(station)),("stationName",Value::text(row.text("stationName").unwrap_or_default())),
            ("description",Value::text(row.text("description").unwrap_or_default())),("security",Value::Double(row.float("security").unwrap_or(0.0)/1000.0)),
            ("dockingCostPerVolume",Value::Double(row.float("dockingCostPerVolume").unwrap_or(0.0))),("officeRentalCost",Value::Int(row.int("officeRentalCost").unwrap_or(0))),
            ("maxShipVolumeDockable",Value::Double(row.float("maxShipVolumeDockable").unwrap_or(0.0))),("reprocessingStationsTake",Value::Double(row.float("reprocessingStationsTake").unwrap_or(0.05))),
            ("reprocessingHangarFlag",Value::int_or_none(row.int("reprocessingHangarFlag"))),("ownerID",Value::Int(row.int("corporationID").unwrap_or(0))),
            ("capitalStation",capital.map_or(Value::None,Value::Long))]))
    })
}

pub(super) async fn read(ctx:&Ctx<'_>,station:i64,method:&str,args:&[Value])->Refused<Value> {
    ctx.store.read(|db| {
    authorize(ctx,db,station)?;
    let rules=db.table("stationServiceRules");
    let value=match method {
        "GetStationServiceAccessRule"=>{
            if args.len()!=2 || args[0].as_i64()!=Some(station){return Err("Request a service rule for your bound station.".into());}
            let service=integer(args.get(1),"service ID",1,i32::MAX as i64)?;
            if !service_ids().contains(&service){return Err("That station service does not exist.".into());}
            let rule=rules.get(&[int(station),int(service)]);
            let get=|column:&str,default:f64|Value::Double(rule.and_then(|r|r.float(column)).unwrap_or(default));
            key_val(vec![("serviceID",Value::Int(service)),("minimumStanding",get("minimumStanding",-10.0)),
                ("minimumCharSecurity",get("minimumCharSecurity",-10.0)),("maximumCharSecurity",get("maximumCharSecurity",10.0)),
                ("minimumCorpSecurity",get("minimumCorpSecurity",-10.0)),("maximumCorpSecurity",get("maximumCorpSecurity",10.0))])
        }
        "GetStationManagementServiceCostModifiers"=>{
            if args.len()!=1 || args[0].as_i64()!=Some(station){return Err("Request cost modifiers for your bound station.".into());}
            let mut services=service_ids();services.sort_unstable();
            Value::List(services.into_iter().map(|service|{
                let rule=rules.get(&[int(station),int(service)]);
                let get=|column:&str|Value::Double(rule.and_then(|r|r.float(column)).unwrap_or(0.0));
                key_val(vec![("serviceID",Value::Int(service)),
                    ("discountPerGoodStandingPoint",get("discountPerGoodStandingPoint")),("surchargePerBadStandingPoint",get("surchargePerBadStandingPoint"))])
            }).collect())
        }
        "GetOwnerIDsOfClonesAtStation"=>{
            if args.len()!=1{return Err("Choose a corporation or all corporations to list clone owners.".into());}
            let corp=integer(args.first(),"corporation ID",-1,i32::MAX as i64)?;
            // Each (corporation, owner) once, in order; every owner is 0 when
            // all corporations are asked for.
            let owners:std::collections::BTreeSet<(i64,i64)>=db.table("chrJumpClones").find(&["locationID"],&[int(station)])
                .filter_map(|j|{
                    let owner=j.int("ownerID")?;
                    let c=db.table("character_").get_id(owner)?.int("corporationID")?;
                    (corp==-1 || c==corp).then_some((c,if corp==-1 {0} else {owner}))
                }).collect();
            Value::List(owners.into_iter().map(|(c,owner)|key_val(vec![("corporationID",Value::Int(c)),("characterID",Value::Int(owner))])).collect())
        }
        _=>return Err("Unknown station-management read.".into()),
    };
    Ok(value)
    })
}

/// RevokeCloneContractsAtStation: the jump clones at the station of the
/// corporation's members (or of one of them); how many went.
pub(super) fn revoke_clones(ctx:&Ctx<'_>,station:i64,corp:i64,char_id:i64)->Refused<usize> {
    ctx.store.write(|tx|->Result<usize,Refusal> {
        authorize(ctx,tx.db(),station)?;
        let gone:Vec<store::RowId>=tx.table("chrJumpClones").find(&["locationID"],&[int(station)])
            .filter(|j|j.int("ownerID").is_some_and(|owner|(char_id==-1 || owner==char_id)
                && tx.table("character_").get_id(owner).and_then(|c|c.int("corporationID"))==Some(corp)))
            .map(|j|j.id).collect();
        for id in &gone {tx.delete("chrJumpClones",*id)?;}
        Ok(gone.len())
    }).map_err(|Refusal(why)|why)
}
