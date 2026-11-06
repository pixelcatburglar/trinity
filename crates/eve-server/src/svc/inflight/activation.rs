//! Funded effect startup: protect identity/mobility while staging its inventory
//! debit, then publish exactly once after commit. The owned task finishes even
//! if its requesting connection is cancelled.

use super::{Active, Ctx, Outcome};
use crate::items::ItemTxn;
use crate::store::{Store, StoreError, Value};
use crate::world::World;

struct Reservation {
    world: World,
    system: i64,
    ship: i64,
    pilot: i64,
    client: i64,
    capacitor: f64,
    capacitor_before: crate::combat::Charged,
    capacitor_reserved: crate::combat::Charged,
    published: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut world = self.world.lock();
        if let Some(ship) = world.parks.get_mut(&self.system).and_then(|park| park.ships.get_mut(&self.ship)) {
            if ship.pilot.char_id == self.pilot && ship.client_id == self.client {
                if !self.published && self.capacitor > 0.0 {
                    if let Some(fight) = &mut ship.fight {
                        // Reading/recharging a curve does not mutate it. Restore
                        // an untouched curve exactly; otherwise preserve intervening
                        // combat/refits and refund onto their current curve.
                        if fight.vitals.capacitor == self.capacitor_reserved {
                            fight.vitals.capacitor = self.capacitor_before;
                        } else {
                            fight.vitals.capacitor.add(crate::combat::now_ms(),self.capacitor);
                        }
                    }
                }
                ship.identity_reserved = false;
            }
        }
        if self.capacitor > 0.0 { notify_capacitor(&world,self.system,self.ship,self.pilot,self.client); }
    }
}

fn refused(outcome: Outcome) -> String {
    match outcome {
        Outcome::UserError { args, .. } => args.iter().find(|(key, _)| key == "error").and_then(|(_,value)| value.as_string())
            .unwrap_or_else(|| "That module cannot be activated now.".into()),
        _ => "That module cannot be activated now.".into(),
    }
}

fn database(error: StoreError) -> String {
    tracing::error!("funded module activation: {}",crate::logging::chain(&error));
    "The module could not be activated.".into()
}

/// Why the store write was given up: what the pilot is told.
struct Refusal(String);

impl From<StoreError> for Refusal {
    fn from(error: StoreError) -> Self {
        Refusal(database(error))
    }
}

impl From<String> for Refusal {
    fn from(why: String) -> Self {
        Refusal(why)
    }
}

impl From<&str> for Refusal {
    fn from(why: &str) -> Self {
        Refusal(why.into())
    }
}

pub(super) async fn funded(ctx: &Ctx<'_>, system: i64, ship: i64, pilot: i64, module: Active, fuel_type: i64, quantity: i64) -> Result<Active,String> {
    let world = ctx.world.clone();
    let store = ctx.store.clone();
    let client = ctx.client_id;
    tokio::spawn(async move { run(&store,world,system,ship,pilot,client,module,fuel_type,quantity) })
        .await.map_err(|error| {
            tracing::error!("funded activation task failed: {error}");
            "The module activation could not be completed.".to_string()
        })?
}

#[allow(clippy::too_many_arguments)]
fn run(store: &Store, world: World, system: i64, ship: i64, pilot: i64, client: i64, mut module: Active, fuel_type: i64, quantity: i64) -> Result<Active,String> {
    let (events, mut reservation, module) = store.write(|tx| -> Result<_, Refusal> {
        let mut txn = ItemTxn::new(tx);
        let current = txn.db().table("character_").get_id(pilot).map(|c| {
            c.int("shipID") == Some(ship) && c.int("solarSystemID") == Some(system) && c.int("stationID").unwrap_or(0) == 0
        });
        match current {
            None => return Err("That character is no longer available.".into()),
            Some(false) => return Err("Your ship or location changed. Try again.".into()),
            Some(true) => {}
        }
        let hull = txn.item(ship)?.ok_or("Your ship no longer exists.")?;
        if hull.owner_id != pilot || hull.location_id != system || hull.category_id != 6 {
            return Err("You no longer control that ship.".into());
        }
        let fitted = txn.item(module.module_id)?.ok_or("That module no longer exists.")?;
        if fitted.owner_id != pilot || fitted.location_id != ship || fitted.category_id != 7 || !fitted.singleton
            || !(crate::dogma::FLAG_SLOT_FIRST..=crate::dogma::FLAG_SLOT_LAST).contains(&fitted.flag) {
            return Err("That module is no longer fitted to your ship.".into());
        }
        let online = txn.db().table("entity_attributes").get(&[Value::Int(module.module_id),Value::Int(2)])
            .map(|a| super::stored_value(&a).unwrap_or(0.0));
        if !online.is_some_and(|value| value!=0.0) { return Err("That module is offline.".into()); }
        let reservation = {
            let mut live = world.lock();
            super::validate_activation(&live,system,ship,pilot,client,&module,false).map_err(refused)?;
            let hull = live.parks.get_mut(&system).unwrap().ships.get_mut(&ship).unwrap();
            let now=crate::combat::now_ms();
            let capacitor_before=hull.fight.as_ref().unwrap().vitals.capacitor;
            if module.cap_need > 0.0 && !hull.fight.as_mut().unwrap().vitals.capacitor.spend(now,module.cap_need) {
                return Err("Your capacitor is too low to run that module.".into());
            }
            module.first_cycle_prepaid=module.cap_need>0.0;
            let capacitor_reserved=hull.fight.as_ref().unwrap().vitals.capacitor;
            hull.identity_reserved = true;
            Reservation { world:world.clone(),system,ship,pilot,client,capacitor:module.cap_need,capacitor_before,capacitor_reserved,published:false }
        };
        let fuel = crate::svc::jumps::FuelCost { cargo:ship,type_id:fuel_type,quantity,flag:Some(5) };
        fuel.debit(&mut txn)?;
        // Damage continues during the transaction, but removal and entry into
        // warp defer until the reservation resolves. Refusal here rolls fuel back.
        super::validate_activation(&world.lock(),system,ship,pilot,client,&module,true).map_err(refused)?;
        Ok((txn.events(), reservation, module))
    }).map_err(|Refusal(why)| why)?;
    // No await/cancellation point between committed payment and publication.
    // The reservation guarantees identity even if live damage changed while
    // the payment was made; that damage is retained and ordinary death
    // processing follows.
    let started = {
        let mut live = world.lock();
        for event in events { live.notify(&[client],"OnItemChange",event); }
        let started=super::publish_activation(&mut live,system,ship,pilot,client,module);
        let mut out=crate::heartbeat::Out::default();
        crate::fight::funded_first_cycle(live.parks.get_mut(&system).unwrap(),ship,started.module_id,started.started_ms,&mut out);
        let jobs=out.deliver(&live);
        live.pending_jobs.extend(jobs);
        live.parks[&system].ships[&ship].fight.as_ref().unwrap().module(started.module_id).cloned().unwrap_or(started)
    };
    reservation.published=true;
    drop(reservation);
    Ok(started)
}

fn notify_capacitor(world: &crate::world::WorldState, system:i64,ship:i64,pilot:i64,client:i64) {
    let Some(fight)=world.parks.get(&system).and_then(|p|p.ships.get(&ship)).and_then(|s|s.fight.as_ref()) else{return;};
    let now=crate::combat::now_ms();
    let amount=fight.vitals.capacitor.at(now);
    world.notify(&[client],"OnModuleAttributeChanges",eve_marshal::Value::Tuple(vec![eve_marshal::Value::List(vec![eve_marshal::Value::Tuple(vec![
        eve_marshal::Value::bytes("OnModuleAttributeChange"),eve_marshal::Value::Int(pilot),eve_marshal::Value::Int(ship),
        eve_marshal::Value::Int(crate::combat::attr::CHARGE as i64),eve_marshal::Value::Long(crate::combat::filetime(now)),
        eve_marshal::Value::Double(amount),eve_marshal::Value::Double(amount),
    ])])]));
}
