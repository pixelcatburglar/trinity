//! Revalidate departure permissions inside the arrival/payment write.
//! Endpoint state is read in that write. Dynamic fleet/field authorization is
//! accepted under the world lock immediately before it commits.
use super::*;
use crate::{
    services::Ctx,
    world::{Bridge, World, WorldState},
};
use crate::store::Db;

pub(super) mod reserved;
pub(super) mod transition;
mod completion;

pub(super) struct Departure<'a> {
    catalog: &'a crate::dogma::Catalog,
    request: JumpRequest,
    ship: Ship,
    from: i64,
    to: i64,
    bridge: Option<Bridge>,
}

impl<'a> Departure<'a> {
    pub(super) fn capture(
        ctx: &Ctx<'a>,
        w: &WorldState,
        request: JumpRequest,
        to: i64,
    ) -> std::result::Result<Self, String> {
        let from = ctx.session.solar_system_id.ok_or("You are not in space.")?;
        let ship = w
            .parks
            .get(&from)
            .and_then(|p| p.ships.get(&ctx.session.ship_id.unwrap_or(0)))
            .filter(|s| {
                s.is_pilot()
                    && s.client_id == ctx.client_id
                    && s.pilot.char_id == ctx.session.char_id.unwrap_or(0)
                    && !s.identity_reserved
                    && !s.warping()
                    && !s.cloaked
            })
            .ok_or("You no longer control that ship here.")?
            .clone();
        let bridge = match request {
            JumpRequest::Portal { ship, .. } => w.bridges.get(&ship).cloned(),
            _ => None,
        };
        let result = Self {
            catalog: ctx.dogma,
            request,
            ship,
            from,
            to,
            bridge,
        };
        result.live(w)?;
        Ok(result)
    }

    fn live(&self, w: &WorldState) -> std::result::Result<(), String> {
        let pilot = self.ship.pilot.char_id;
        // Identity reservation keeps combat and ordinary motion running. Use
        // the current hull for late range checks, not its capture-time position.
        let traveler=w.parks.get(&self.from).and_then(|p|p.ships.get(&self.ship.id))
            .filter(|s|s.pilot.char_id==pilot && s.client_id==self.ship.client_id).unwrap_or(&self.ship);
        match self.request {
            JumpRequest::Beacon { member, beacon } => {
                if w.gangs.of(pilot).is_none() || w.gangs.of(pilot) != w.gangs.of(member) {
                    return Err("The cynosural field's pilot is no longer in your gang.".into());
                }
                if crate::svc::jumps::field_at(w, self.to, beacon, member).is_none() {
                    return Err("The cynosural field has gone out.".into());
                }
                if w.parks[&self.to].beacons[&beacon].type_id
                    == crate::beacons::TYPE_COVERT_CYNOSURAL_FIELD
                    && traveler.pilot.group_id != 898
                {
                    return Err("Only Black Ops jump drives can use that covert field.".into());
                }
            }
            JumpRequest::Portal {
                pilot: host,
                ship: portal,
                alliance,
            } => {
                let b = self.bridge.as_ref().ok_or("The jump portal has closed.")?;
                if w.bridges.get(&portal) != Some(b)
                    || b.system != self.from
                    || b.to_system != self.to
                    || b.titan_pilot != host
                    || b.until_ms <= crate::combat::now_ms()
                    || !crate::svc::jumps::portal_is_live(w, portal, b)
                {
                    return Err("The jump portal changed before your departure.".into());
                }
                let source = &w.parks[&self.from].ships[&portal];
                if (traveler.pos - source.pos).len() - traveler.stats.radius - source.stats.radius
                    > crate::svc::jumps::BRIDGE_RANGE
                {
                    return Err("The jump portal moved out of range.".into());
                }
                if alliance {
                    if b.member.is_some()
                        || traveler.pilot.alliance_id.filter(|id| *id > 0).is_none()
                        || traveler.pilot.alliance_id != source.pilot.alliance_id
                    {
                        return Err("You are no longer in the jump portal's alliance.".into());
                    }
                } else if w.gangs.of(pilot).is_none() || w.gangs.of(pilot) != w.gangs.of(host) {
                    return Err("You are no longer in the jump portal's gang.".into());
                }
            }
            JumpRequest::Structure { local, .. } => {
                let source = w
                    .parks
                    .get(&self.from)
                    .and_then(|p| p.structures.get(&local))
                    .ok_or("The jump array is no longer here.")?;
                if !matches!(source.state, 4 | 5)
                    || (traveler.pos - source.pos).len() - traveler.stats.radius - source.radius
                        > crate::svc::jumps::BRIDGE_RANGE
                {
                    return Err("The jump array is no longer available within range.".into());
                }
            }
            JumpRequest::Alliance { .. } => {}
        }
        Ok(())
    }

    /// The endpoints' durable state, read in the arrival/payment write.
    pub(super) fn check_endpoints(&self, db: &Db) -> std::result::Result<(), String> {
        let pilot = self.ship.pilot.char_id;
        let corp = corporation_of(db, pilot).ok_or_else(unsaved)?;
        match self.request {
            JumpRequest::Alliance { beacon } => self.fixed_beacon(db, corp, beacon)?,
            JumpRequest::Portal {
                pilot: host,
                ship: portal,
                alliance,
            } => {
                let host_corp=corporation_of(db,host).ok_or("The jump portal's pilot is unavailable.")?;
                let in_place = db.table("entity").get_id(portal).is_some_and(|e| {
                    e.int("ownerID") == Some(host)
                        && e.int("locationID") == Some(self.from)
                        && e.int("flag").unwrap_or(0) == 0
                        && e.int("quantity").is_some_and(|q| q > 0)
                });
                if !in_place {
                    return Err("The jump portal's ship has moved.".into());
                }
                let generator = self
                    .bridge
                    .as_ref()
                    .ok_or("The jump portal closed.")?
                    .generator_id;
                let fitted=db.table("entity").get_id(generator).is_some_and(|e|e.int("ownerID")==Some(host) && e.int("locationID")==Some(portal)
                    && e.int("flag").is_some_and(|f|(11..=35).contains(&f)) && e.int("quantity").is_some_and(|q|q>0)
                    && e.int("singleton").is_some_and(|s|s!=0))
                    && db.table("entity_attributes").get(&[StoreValue::Int(generator),StoreValue::Int(2)])
                        .is_some_and(|a|a.float("valueFloat").or_else(||a.float("valueInt")).unwrap_or(0.0)!=0.0);
                if !fitted {
                    return Err("The jump portal generator is no longer fitted and online.".into());
                }
                let harmonics = self.bridge.as_ref().unwrap().harmonics;
                if harmonics > 0 {
                    let loadout = crate::dogma::Loadout::load(
                        db,
                        self.catalog,
                        self.ship.pilot.char_id,
                        Some(self.ship.id),
                        &Default::default(),
                    )
                    .map_err(travel_failed)?;
                    let index = loadout
                        .find(self.ship.id)
                        .ok_or("Your ship could not be read.")?;
                    if loadout.value(index, 1253) < harmonics as f64 {
                        return Err(
                            "Your ship's jump harmonics no longer permit this portal.".into()
                        );
                    }
                }
                if alliance {
                    let members = affiliations(db, &[corp, host_corp]);
                    let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
                    if own.is_none() || own != members.get(&host_corp).copied().flatten() {
                        return Err("The jump portal's alliance changed.".into());
                    }
                }
                if let Some(b) = self.bridge.as_ref().filter(|b| b.member.is_none()) {
                    self.fixed_beacon(db, corp, b.beacon)?;
                }
            }
            JumpRequest::Structure { local, remote } => {
                let link=db.table("jumpBridgeLinks").get_id(local).is_some_and(|l|l.int("destinationID")==Some(remote));
                if !link {
                    return Err("The jump array link has changed.".into());
                }
                let mut ids = vec![local, remote];
                ids.sort_unstable();
                ids.dedup();
                let rows: Vec<_> = ids.iter().filter_map(|id| db.table("entity").get_id(*id)).collect();
                if rows.len() != 2
                    || rows.iter().any(|r| {
                        // An endpoint of a type the static data does not
                        // know is no jump array (joining on it left it out).
                        r.int("typeID").and_then(|t| crate::sde::get().type_(t)).map(|t| t.group_id) != Some(707)
                            || r.int("quantity").is_none_or(|q| q <= 0)
                            || r.int("locationID")
                                != Some(if r.int("itemID") == Some(local) {
                                    self.from
                                } else {
                                    self.to
                                })
                    })
                {
                    return Err("The jump array endpoints have changed.".into());
                }
                let states: Vec<_> = ids.iter().filter_map(|id| db.table("posStructures").get_id(*id)).collect();
                if states.len() != 2 || states.iter().any(|r| !matches!(r.int("state"), Some(4 | 5)))
                {
                    return Err("Both jump arrays must remain online.".into());
                }
                let owner = rows
                    .iter()
                    .find(|r| r.int("itemID") == Some(local))
                    .unwrap()
                    .int("ownerID");
                if rows.iter().any(|r| r.int("ownerID") != owner) {
                    return Err(
                        "The linked jump arrays no longer belong to the same corporation.".into(),
                    );
                }
                let owner = owner.unwrap_or(0);
                if corp != owner {
                    let members = affiliations(db, &[corp, owner]);
                    let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
                    if own.is_none() || own != members.get(&owner).copied().flatten() {
                        return Err("The jump array's alliance changed.".into());
                    }
                }
            }
            JumpRequest::Beacon { .. } => {}
        }
        Ok(())
    }

    fn fixed_beacon(
        &self,
        db: &Db,
        corp: i64,
        beacon: i64,
    ) -> std::result::Result<(), String> {
        let beacons = crate::sde::get().types_in_group(838);
        let owner = db
            .table("entity")
            .get_id(beacon)
            .filter(|e| {
                e.int("locationID") == Some(self.to)
                    && e.int("typeID").is_some_and(|t| beacons.contains(&(t as i32)))
                    && e.int("quantity").is_some_and(|q| q > 0)
            })
            .ok_or("The alliance beacon has moved or disappeared.")?
            .int("ownerID")
            .unwrap_or(0);
        let online = db.table("posStructures").get_id(beacon).is_some_and(|r| matches!(r.int("state"), Some(4 | 5)));
        if !online {
            return Err("The alliance beacon is no longer online.".into());
        }
        let members = affiliations(db, &[corp, owner]);
        let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
        if own.is_none() || own != members.get(&owner).copied().flatten() {
            return Err("The beacon no longer belongs to your alliance.".into());
        }
        Ok(())
    }
}

fn corporation_of(db: &Db, pilot: i64) -> Option<i64> {
    db.table("character_").get_id(pilot).and_then(|c| c.int("corporationID"))
}

/// What a lookup that found no row reported: the save failed.
fn unsaved() -> String {
    "Your ship's location could not be saved. Please try again.".into()
}

/// Each existing corporation's alliance.
fn affiliations(
    db: &Db,
    corps: &[i64],
) -> std::collections::BTreeMap<i64, Option<i64>> {
    corps
        .iter()
        .filter_map(|id| db.table("corporation").get_id(*id).map(|co| (*id, co.int("allianceID"))))
        .collect()
}
