//! Cloaking devices (effects.Cloak, effects.Uncloak).
//!
//! A cloaked ship leaves everyone's view but its pilot's (its ball carries
//! the cloak flag for them), cannot be locked, and breaks the locks already on
//! it; NPCs and bots lose sight of it. It cannot be cloaked within
//! `DECLOAK_RANGE` of anything, and anything coming that close (a ship, a
//! structure, a celestial) drops the cloak. While cloaked no other module can
//! be started; only a covert ops cloak lets the ship warp. When the device
//! stops, the ship is seen again.

use tracing::info;

use crate::ballpark::Park;
use crate::combat::Action;
use crate::heartbeat::Out;
use crate::svc::space;
use crate::world::WorldState;

pub mod durability;

/// Absolute server timestamps survive profile reloads and reconnects.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct History {
    #[serde(default)]
    pub targeting_at: i64,
    #[serde(default)]
    pub module_ready: std::collections::BTreeMap<i64,i64>,
}

impl History {
    /// Merge observations without extending them relative to the current time.
    pub fn merge(&mut self,other:History,now:i64) {
        self.prune(now);
        let mut other=other;other.prune(now);
        self.targeting_at=self.targeting_at.max(other.targeting_at);
        for (id,deadline) in other.module_ready {
            let current=self.module_ready.entry(id).or_default();*current=(*current).max(deadline);
        }
    }
    pub fn prune(&mut self,now:i64) {
        // FILETIME is the client's timestamp representation. Reject corrupt
        // values which cannot be represented instead of creating endless waits.
        let valid=|deadline:i64|deadline>now && deadline.checked_add(11_644_473_600_000).and_then(|v|v.checked_mul(10_000)).is_some();
        if !valid(self.targeting_at) {self.targeting_at=0;}
        self.module_ready.retain(|id,deadline|*id>0 && *id<=i32::MAX as i64 && valid(*deadline));
    }
}

pub fn reactivation_remaining(world:&WorldState,module:i64,now:i64)->Option<i64> {
    world.parks.values().flat_map(|p|p.ships.values()).filter_map(|s|s.fight.as_ref())
        .filter_map(|f|f.cloak_history.module_ready.get(&module)).copied().filter(|ready|*ready>now).max().map(|ready|ready-now)
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedState {
    #[serde(default)]
    pub history: History,
    #[serde(default)]
    pub active: Option<SavedCloak>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedCloak {
    pub module_id: i64,
    pub type_id: i64,
    pub effect_id: i64,
    pub started_ms: i64,
}

pub fn snapshot(ship:&crate::ballpark::Ship)->SavedState {
    let Some(fight)=&ship.fight else{return SavedState::default();};
    let mut history=fight.cloak_history.clone();history.prune(crate::combat::now_ms());
    // Reactivating modules cannot be removed from a fitting; its 25 slots
    // bound legitimate history. Keep corrupt in-memory data from growing a
    // checkpoint beyond the durable envelope's limit.
    history.module_ready=history.module_ready.into_iter().take(25).collect();
    SavedState {history,active:fight.modules.iter()
        .find(|m|matches!(m.action,Action::Cloak {..}) && m.continuous())
        .map(|m|SavedCloak {module_id:m.module_id,type_id:m.type_id,effect_id:m.effect_id,started_ms:m.started_ms})}
}

/// Nothing may be this close to a ship that cloaks, or stays cloaked.
pub const DECLOAK_RANGE: f64 = 2_000.0;

/// What is too close to a ship to cloak: the name of the nearest thing.
fn crowded(park: &Park, ship_id: i64) -> Option<String> {
    let me = park.ships.get(&ship_id)?;
    let near = |pos: crate::ballpark::Vec3, radius: f64| (pos - me.pos).len() - radius - me.stats.radius <= DECLOAK_RANGE;
    if let Some(s) = park.ships.values().find(|s| s.id != ship_id && !s.cloaked && near(s.pos, s.stats.radius)) {
        return Some(s.pilot.name.clone());
    }
    if park.structures.values().any(|s| near(s.pos, s.radius)) {
        return Some("a structure".into());
    }
    if let Some(c) = park.celestials.iter().find(|c| near(c.pos, c.radius)) {
        return Some(if c.name.is_empty() { "a celestial".into() } else { c.name.clone() });
    }
    park.cans.values().find(|c| near(c.pos, c.radius)).map(|_| "a container".into())
}

/// Whether the ship may cloak where it is.
pub fn may_cloak(w: &WorldState, system: i64, ship_id: i64) -> Result<(), String> {
    let Some(park) = w.parks.get(&system) else { return Err("You are not in space.".into()) };
    let Some(ship)=park.ships.get(&ship_id) else {return Err("You are not in space.".into());};
    if ship.cloaked {return Err("Your ship is already cloaked.".into());}
    if ship.fight.as_ref().is_some_and(|f|f.locked().next().is_some()) {
        return Err("You cannot cloak while you have targets locked.".into());
    }
    if park.ships.values().any(|s|s.id!=ship_id && s.fight.as_ref().is_some_and(|f|f.is_locked(ship_id))) {
        return Err("You cannot cloak while another ship has you locked.".into());
    }
    match crowded(park, ship_id) {
        Some(what) => Err(format!("You cannot cloak within {:.0} km of {what}.", DECLOAK_RANGE / 1_000.0)),
        None => Ok(()),
    }
}

/// The ship just cloaked: out of others' sight, locks on it broken.
pub fn cloaked(w: &mut WorldState, system: i64, ship_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    ship.cloaked = true;
    let owner = ship.client_id;
    let tick = park.tick;
    let stop_warp=ship.warping() && !ship.in_warp()
        && !ship.fight.as_ref().is_some_and(|f|f.modules.iter().any(|m|matches!(m.action,Action::Cloak {..}) && m.warp_safe));
    // Pending locks are cancelled as the cloak engages; established locks
    // were rejected by may_cloak under this same publication lock.
    let own_targets=ship.fight.as_mut().map(|f|std::mem::take(&mut f.locks).into_keys().collect::<Vec<_>>()).unwrap_or_default();
    if stop_warp {park.command(ship_id,crate::ballpark::Command::Stop);}
    // Whoever had it locked loses it.
    let lockers: Vec<i64> = park.ships.values().filter(|s| s.fight.as_ref().is_some_and(|f| f.locks.contains_key(&ship_id))).map(|s| s.id).collect();
    let mut out = Out::default();
    if stop_warp {out.destiny(system,space::entry(tick,"Stop",vec![eve_marshal::Value::Int(ship_id)]));}
    for target in own_targets {
        out.note(owner,"OnTarget",eve_marshal::Value::Tuple(vec![eve_marshal::Value::bytes("lost"),eve_marshal::Value::Int(target),eve_marshal::Value::None]));
        crate::fight::tell_target_lost(park,ship_id,target,&mut out);
    }
    for l in lockers {
        if let Some(f) = park.ships.get_mut(&l).and_then(|s| s.fight.as_mut()) {
            f.locks.remove(&ship_id);
        }
        if let Some(other)=park.ships.get(&l) {out.note(other.client_id,"OnTarget",eve_marshal::Value::Tuple(vec![eve_marshal::Value::bytes("lost"),eve_marshal::Value::Int(ship_id),eve_marshal::Value::None]));}
        crate::fight::tell_target_lost(park, l, ship_id, &mut out);
        if let Some(brain) = park.ships.get_mut(&l).and_then(|s| s.npc.as_mut()).filter(|b| b.target == Some(ship_id)) {
            brain.target = None;
        }
    }
    let fx = space::special_fx(tick, ship_id, None, "effects.Cloak");
    // Original destiny.dll: CloakBall(int id, byte kind, optional range).
    // The owner's existing ball must change its native flag too.
    space::broadcast(w,system,Some(owner),vec![space::entry(tick,"CloakBall",vec![eve_marshal::Value::Int(ship_id),eve_marshal::Value::Int(1)])]);
    space::broadcast(w, system, None, vec![fx]);
    let others: Vec<i64> = w.in_space(system).into_iter().filter(|c| *c != owner).collect();
    for c in others {
        space::broadcast(w, system, Some(c), space::remove_ball_entries(tick, ship_id));
        w.with_hidden(c,|hidden| {hidden.insert(ship_id);});
    }
    let jobs=out.deliver(w);w.pending_jobs.extend(jobs);
    info!("ship {ship_id} cloaked in {system}");
}

/// The ship's cloak dropped: seen again by everyone.
pub fn uncloaked(w: &mut WorldState, system: i64, ship_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    if !ship.cloaked {
        return;
    }
    // A queued old stop must never reveal a newly activated cloak.
    if ship.fight.as_ref().is_some_and(|f|f.modules.iter().any(|m|matches!(m.action,Action::Cloak {..}))) {return;}
    ship.cloaked = false;
    let owner = ship.client_id;
    let entries = space::add_ball_entries(park, ship_id);
    let fx = space::special_fx(park.tick, ship_id, None, "effects.Uncloak");
    let native=space::entry(park.tick,"UncloakBall",vec![eve_marshal::Value::Int(ship_id)]);
    let others: Vec<i64> = w.in_space(system).into_iter().filter(|c| *c != owner).collect();
    for c in others {
        space::broadcast(w, system, Some(c), entries.clone());
    }
    space::broadcast(w,system,Some(owner),vec![native]);
    space::broadcast(w, system, None, vec![fx]);
    info!("ship {ship_id} uncloaked in {system}");
}

/// Cloaked ships something has come too close to lose their cloak (the
/// device stops, and the heartbeat shows the ship again).
pub fn proximity(park: &mut Park, now: i64, out: &mut Out) {
    let crowded_ships: Vec<(i64, i64)> = park
        .ships
        .values()
        .filter(|s| s.cloaked)
        .filter(|s| crowded(park, s.id).is_some())
        .filter_map(|s| s.fight.as_ref()?.modules.iter().find(|m| matches!(m.action, Action::Cloak { .. })).map(|m| (s.id, m.module_id)))
        .collect();
    for (ship, module) in crowded_ships {
        let note = crate::fight::notify("Your cloak drops: something came too close.");
        crate::fight::stop(park, ship, module, now, out, Some(note));
    }
}

/// Whether a ship is cloaked with a device that lets it warp.
pub fn warp_safe(park: &Park, ship_id: i64) -> Option<bool> {
    let ship = park.ships.get(&ship_id).filter(|s| s.cloaked)?;
    Some(ship.fight.as_ref().is_some_and(|f| f.modules.iter().any(|m| matches!(m.action, Action::Cloak { .. }) && m.warp_safe)))
}
