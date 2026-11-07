//! Starbase structures in a solar system: control towers and what is
//! anchored around them, as the park holds them.
//!
//! A structure is an item in the system (its position in the entity row)
//! with a `posStructures` row for its state. The states and their timers are
//! the client's (playerOwned.py): launched, a structure floats free and
//! unanchored; anchoring fixes it in place, then after the type's
//! anchoringDelay it is anchored; onlining takes onliningDelay; unanchoring
//! takes unanchoringDelay, after which it floats free again and can be
//! scooped. The client times each step from the slim item's posTimestamp,
//! so the server only has to agree on when a step began.
//!
//! `RMR_STARBASE_SPEEDUP` shortens those timers for testing by back-dating
//! the timestamp, which the client's timers then agree with.
//!
//! Structures can be shot (fight.rs). A tower whose shield falls below a
//! quarter goes reinforced if it has strontium (svc/starbase.rs reinforce):
//! invulnerable until the posTimestamp the strontium bought, then online
//! again. A structure whose hull gives out is destroyed.

use std::sync::OnceLock;

use eve_marshal::Value;

use crate::ballpark::{Park, Vec3};
use crate::combat::{attr, Charged, Vitals};
use crate::destiny::{flags, mode, Ball};
use crate::heartbeat::{Job, Out};
use crate::store::{self, Db, StoreError};
use crate::svc::space::{self, entry};

/// playerOwned.py STRUCTURE_*.
pub mod state {
    pub const UNANCHORED: i16 = 0;
    pub const ANCHORED: i16 = 1;
    pub const ONLINING: i16 = 2;
    pub const REINFORCED: i16 = 3;
    pub const ONLINE: i16 = 4;
    pub const OPERATING: i16 = 5;
}

pub const CATEGORY_STRUCTURE: i32 = 23;
pub const GROUP_CONTROL_TOWER: i64 = 365;
const TYPE_FORCE_FIELD: i64 = 16103;
const ATTR_SHIELD_RADIUS: i32 = 680;
const ATTR_ANCHORING_DELAY: i32 = 556;
const ATTR_ONLINING_DELAY: i32 = 677;
const ATTR_UNANCHORING_DELAY: i32 = 676;
const TICKS_PER_MS: i64 = 10_000;
/// What an anchored structure weighs to destiny: immovable.
const IMMOVABLE: f64 = 1.0e34;

static SPEEDUP: OnceLock<f64> = OnceLock::new();

/// How many times faster than the client's timers starbase steps run.
pub fn set_speedup(speedup: f64) {
    let _ = SPEEDUP.set(speedup.max(1.0));
}

pub fn speedup() -> f64 {
    SPEEDUP.get().copied().unwrap_or(1.0)
}

/// When a step of `delay` ticks that starts now should be stamped as having
/// started, so that it ends `delay / speedup` from now.
pub fn timer_start(now: i64, delay: i64) -> i64 {
    let speedup = SPEEDUP.get().copied().unwrap_or(1.0);
    now - delay + (delay as f64 / speedup) as i64
}

#[derive(Debug, Clone)]
pub struct Structure {
    pub id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub owner_id: i64,
    /// The owner's corporation (the owner itself, for a corporation).
    pub corp_id: i64,
    pub alliance_id: Option<i64>,
    pub pos: Vec3,
    pub radius: f64,
    pub state: i16,
    /// FILETIME the current timed step began; None once it is over.
    pub stamp: Option<i64>,
    /// For a structure other than a tower: the tower it is anchored to.
    pub tower: Option<i64>,
    /// anchoringDelay, onliningDelay, unanchoringDelay, in FILETIME ticks.
    pub delays: [i64; 3],
    pub vitals: Vitals,
    pub signature: f64,
    /// (character, ship) that last hurt it.
    pub last_attacker: Option<(i64, i64)>,
    /// Reinforcement was asked for since the shield last stood above a
    /// quarter (and found no strontium, if it is not reinforced).
    pub reinforce_asked: bool,
    /// For a tower: its force field's reach (shieldRadius), the harmonic its
    /// password makes, and whether its corporation and alliance pass.
    pub field_radius: f64,
    pub harmonic: i64,
    pub allow_corp: bool,
    pub allow_alliance: bool,
    /// The force field ball an online tower raises.
    pub field: Option<Field>,
    /// For a tower: whom its defences engage.
    pub sentry: crate::guard::Sentry,
    /// For a gun, missile or EW battery: what it is doing.
    pub guard: Option<crate::guard::Guard>,
    /// A jump array's linked endpoint: (structure, system, type).
    pub bridge: Option<(i64, i64, i64)>,
    /// The character manning it (posMgr.AssumeStructureControl), shown as
    /// the slim item's controllerID (playerOwned.py:357-410). Not saved: a
    /// restart leaves every structure unmanned.
    pub controller: Option<i64>,
}

/// A tower's force field: a ball of its own (type Force Field), centred on
/// the tower, solid to all but those it lets through.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub id: i64,
    pub radius: f64,
    pub harmonic: i64,
    pub corp_id: i64,
    pub alliance_id: i64,
}

impl Field {
    pub fn ball(&self, tower: &Structure) -> Ball {
        Ball {
            id: self.id,
            mode: mode::FIELD,
            warp: None,
            radius: self.radius,
            pos: tower.pos.tuple(),
            flags: flags::CELESTIAL,
            mass: IMMOVABLE,
            corp_id: self.corp_id as i32,
            alliance_id: self.alliance_id as i32,
            harmonic: self.harmonic,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: "Force Field".into(),
            cloaked: false,
        }
    }

    /// moonmining.GetForcefield finds it by group and owner.
    pub fn slim(&self, tower: &Structure) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(TYPE_FORCE_FIELD)),
            ("ownerID", Value::Int(tower.owner_id)),
            ("corpID", Value::Int(tower.corp_id)),
            ("allianceID", Value::int_or_none(tower.alliance_id)),
            ("controlTowerID", Value::Int(tower.id)),
        ])
    }
}

/// The harmonic a password makes: none for no password, else a hash that is
/// never the none (-1) or master (-2) value.
pub fn harmonic(password: &str) -> i64 {
    if password.is_empty() {
        return crate::destiny::NO_HARMONIC;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in password.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (h & 0x3fff_ffff_ffff_ffff) as i64
}

impl Structure {
    pub fn is_tower(&self) -> bool {
        self.group_id == GROUP_CONTROL_TOWER
    }

    fn delay(&self) -> i64 {
        match self.state {
            state::ANCHORED => self.delays[0],
            state::ONLINING => self.delays[1],
            state::UNANCHORED => self.delays[2],
            _ => 0,
        }
    }

    /// Whether the current step's timer is still running.
    pub fn busy(&self, now: i64) -> bool {
        self.stamp.is_some_and(|s| now < s + self.delay())
    }

    pub fn fully_anchored(&self, now: i64) -> bool {
        self.state == state::ANCHORED && !self.busy(now)
    }

    pub fn fully_online(&self, now: i64) -> bool {
        match self.state {
            state::ONLINING => !self.busy(now),
            state::REINFORCED | state::ONLINE | state::OPERATING => true,
            _ => false,
        }
    }

    /// Floating free: unanchored, and done unanchoring.
    pub fn is_free(&self, now: i64) -> bool {
        self.state == state::UNANCHORED && !self.busy(now)
    }

    /// The field it should have up now: an online tower's, letting through
    /// its password, and its corporation and alliance if it says so.
    fn wanted_field(&self, now: i64) -> Option<(i64, i64, i64)> {
        if !self.is_tower() || !self.fully_online(now) || self.field_radius <= 0.0 {
            return None;
        }
        let corp = if self.allow_corp { self.corp_id } else { -1 };
        let alliance = if self.allow_alliance { self.alliance_id.unwrap_or(-1) } else { -1 };
        Some((self.harmonic, corp, alliance))
    }

    /// Fixed in space: anything but a structure done unanchoring. Ships
    /// bounce off it, on both sides.
    pub fn fixed(&self) -> bool {
        !(self.state == state::UNANCHORED && self.stamp.is_none())
    }

    /// Reinforced: no damage gets through until its timer runs out.
    pub fn invulnerable(&self) -> bool {
        self.state == state::REINFORCED
    }

    pub fn destroyed(&self) -> bool {
        self.vitals.hull_max > 0.0 && self.vitals.hull_damage >= self.vitals.hull_max - 1e-6
    }

    /// Online or on its way there: drawing on its tower.
    pub fn drawing(&self) -> bool {
        matches!(self.state, state::ONLINING | state::REINFORCED | state::ONLINE | state::OPERATING)
    }

    pub fn slim(&self) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(self.type_id)),
            ("ownerID", Value::Int(self.owner_id)),
            ("corpID", Value::Int(self.corp_id)),
            ("allianceID", Value::int_or_none(self.alliance_id)),
            ("posState", Value::Int(self.state.into())),
            ("posTimestamp", self.stamp.map_or(Value::None, Value::Long)),
            ("controlTowerID", Value::int_or_none(self.tower)),
            ("incapacitated", Value::Int(0)),
            ("controllerID", Value::int_or_none(self.controller)),
            ("remoteStructureID", Value::int_or_none(self.bridge.map(|bridge| bridge.0))),
            ("remoteSystemID", Value::int_or_none(self.bridge.map(|bridge| bridge.1))),
        ])
    }

    /// Free and passable while it floats; fixed and solid once anchored.
    pub fn ball(&self) -> Ball {
        let free = !self.fixed();
        Ball {
            id: self.id,
            mode: mode::STOP,
            warp: None,
            radius: self.radius,
            pos: self.pos.tuple(),
            flags: if free { flags::IS_FREE | flags::IS_GLOBAL | flags::IS_INTERACTIVE } else { flags::CELESTIAL },
            mass: IMMOVABLE,
            corp_id: self.corp_id as i32,
            alliance_id: self.alliance_id.map_or(-1, |a| a as i32),
            harmonic: crate::destiny::NO_HARMONIC,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: self.name.clone(),
            cloaked: false,
        }
    }
}

/// OnSlimItemChange(itemID, slim), as a destiny entry (michelle.py:810).
pub fn slim_change(tick: i64, s: &Structure) -> Value {
    entry(tick, "OnSlimItemChange", vec![Value::Int(s.id), s.slim()])
}

/// SetBallFree(ballID, isFree) and SetBallMassive(ballID, isMassive): a
/// structure coming loose, or being fixed in place.
pub fn free_entries(tick: i64, id: i64, free: bool) -> Vec<Value> {
    vec![
        entry(tick, "SetBallFree", vec![Value::Int(id), Value::Int(free.into())]),
        entry(tick, "SetBallMassive", vec![Value::Int(id), Value::Int((!free).into())]),
    ]
}

/// AddBalls for one structure.
pub fn add_entry(tick: i64, s: &Structure) -> Value {
    let blob = crate::destiny::encode(crate::destiny::stamp(tick), &[s.ball()]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![s.slim()]), Value::Dict(Vec::new())]);
    entry(tick, "AddBalls", vec![chunk])
}

/// The structures in a system, for a new park.
pub fn load(db: &Db, solar_system_id: i64) -> Result<Vec<Structure>, StoreError> {
    let sde = crate::sde::get();
    let structure_types = sde.types_in_category(CATEGORY_STRUCTURE as i64);
    let corporations = db.table("corporation");
    let characters = db.table("character_");
    let entities = db.table("entity");
    let rows: Vec<store::Row> = entities
        .find(&["locationID"], &[store::Value::Int(solar_system_id)])
        .filter(|e| e.int("typeID").is_some_and(|t| structure_types.binary_search(&(t as i32)).is_ok()))
        .collect();
    let types: Vec<i32> = rows.iter().filter_map(|r| r.int("typeID")).map(|t| t as i32).collect();
    let delays = delays(&types);
    let health = health(&types);
    let now = crate::combat::now_ms();
    let mut structures: Vec<Structure> = rows
        .iter()
        .map(|e| {
            let type_id = e.int("typeID").unwrap_or(0);
            let known = sde.type_(type_id);
            let group_id = known.map_or(0, |t| t.group_id as i64);
            let owner_id = e.int("ownerID").unwrap_or(0);
            let p = db.table("posStructures").get_id(e.int("itemID").unwrap_or(0));
            // The owner is a corporation, or a pilot whose corporation counts.
            let co = corporations.get_id(owner_id);
            let ch = characters.get_id(owner_id);
            let cc = ch.and_then(|c| c.int("corporationID")).and_then(|c| corporations.get_id(c));
            let remote = db
                .table("jumpBridgeLinks")
                .get_id(e.int("itemID").unwrap_or(0))
                .and_then(|b| b.int("destinationID"))
                .and_then(|d| entities.get_id(d));
            let flag = |c: &str, default: i64| p.and_then(|p| p.int(c)).unwrap_or(default) == 1;
            Structure {
                id: e.int("itemID").unwrap_or(0),
                type_id,
                group_id,
                name: e.text("itemName").filter(|n| !n.is_empty()).map(str::to_string).or_else(|| known.map(|t| t.name.to_string())).unwrap_or_default(),
                owner_id,
                corp_id: co.and_then(|c| c.int("corporationID")).or_else(|| ch.and_then(|c| c.int("corporationID"))).unwrap_or(owner_id),
                alliance_id: co.and_then(|c| c.int("allianceID")).or_else(|| cc.and_then(|c| c.int("allianceID"))).filter(|a| *a != 0),
                pos: Vec3::new(e.float("x").unwrap_or(0.0), e.float("y").unwrap_or(0.0), e.float("z").unwrap_or(0.0)),
                radius: sde.table("invTypes").get("typeID", type_id).and_then(|t| t.float("radius")).unwrap_or(100.0),
                state: p.and_then(|p| p.int("state")).unwrap_or(0) as i16,
                stamp: p.and_then(|p| p.int("stateTimestamp")),
                tower: p.and_then(|p| p.int("controlTowerID")),
                delays: delays.get(&type_id).copied().unwrap_or_default(),
                vitals: {
                    let mut v = vitals(health.get(&type_id), now);
                    if let Some(charge) = p.and_then(|p| p.float("shieldCharge")) {
                        v.shield.set(now, charge);
                    }
                    v.armor_damage = p.and_then(|p| p.float("armorDamage")).unwrap_or(0.0);
                    v.hull_damage = p.and_then(|p| p.float("hullDamage")).unwrap_or(0.0);
                    v
                },
                signature: health.get(&type_id).and_then(|a| a.get(&attr::SIGNATURE_RADIUS)).copied().unwrap_or(0.0),
                last_attacker: None,
                reinforce_asked: false,
                field_radius: health.get(&type_id).and_then(|a| a.get(&ATTR_SHIELD_RADIUS)).copied().unwrap_or(0.0),
                harmonic: harmonic(p.and_then(|p| p.text("password")).unwrap_or_default()),
                allow_corp: flag("allowCorp", 1),
                allow_alliance: flag("allowAlliance", 0),
                field: None,
                controller: None,
                sentry: crate::guard::Sentry {
                    standing: p.and_then(|p| p.float("sentryStanding")),
                    status: p.and_then(|p| p.float("sentryStatus")),
                    standing_drop: flag("sentryStandingDrop", 0),
                    status_drop: flag("sentryStatusDrop", 0),
                    aggression: flag("sentryAggression", 0),
                    war: flag("sentryWar", 1),
                },
                guard: health.get(&type_id).and_then(|a| crate::guard::profile(group_id, a)),
                bridge: remote.and_then(|r| Some((r.int("itemID")?, r.int("locationID")?, r.int("typeID")?))),
            }
        })
        .collect();
    let batteries: Vec<i64> = structures.iter().filter(|s| s.guard.is_some()).map(|s| s.id).collect();
    if !batteries.is_empty() {
        let ammo = crate::guard::ammo(db, &batteries)?;
        for s in &mut structures {
            if let Some(g) = s.guard.as_mut() {
                g.ammo = ammo.get(&s.id).cloned();
            }
        }
    }
    Ok(structures)
}

/// anchoringDelay, onliningDelay and unanchoringDelay of each type, in ticks.
pub fn delays(types: &[i32]) -> std::collections::HashMap<i64, [i64; 3]> {
    let sde = crate::sde::get();
    let mut out: std::collections::HashMap<i64, [i64; 3]> = types.iter().map(|t| (i64::from(*t), [0; 3])).collect();
    for t in types {
        for &(a, v) in sde.type_attributes(i64::from(*t)) {
            let slot = match a {
                ATTR_ANCHORING_DELAY => 0,
                ATTR_ONLINING_DELAY => 1,
                ATTR_UNANCHORING_DELAY => 2,
                _ => continue,
            };
            out.entry(i64::from(*t)).or_default()[slot] = (v as i64) * TICKS_PER_MS;
        }
    }
    out
}

/// The attributes a structure fights with, by type.
pub fn health(types: &[i32]) -> std::collections::HashMap<i64, std::collections::HashMap<i32, f64>> {
    let mut wanted = vec![attr::HP, attr::SHIELD_CAPACITY, attr::ARMOR_HP, attr::SHIELD_RECHARGE_RATE, attr::SIGNATURE_RADIUS, ATTR_SHIELD_RADIUS];
    wanted.extend(crate::guard::profile_attributes());
    wanted.extend(attr::SHIELD_RESONANCE);
    wanted.extend(attr::ARMOR_RESONANCE);
    wanted.extend(attr::HULL_RESONANCE);
    let sde = crate::sde::get();
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<i32, f64>> = std::collections::HashMap::new();
    for t in types {
        for &(a, v) in sde.type_attributes(i64::from(*t)) {
            if wanted.contains(&a) {
                out.entry(i64::from(*t)).or_default().insert(a, v);
            }
        }
    }
    out
}

/// Full health from a type's attributes; resonances default to none.
pub fn vitals(a: Option<&std::collections::HashMap<i32, f64>>, now: i64) -> Vitals {
    let v = |id: i32, default: f64| a.and_then(|a| a.get(&id)).copied().unwrap_or(default);
    Vitals {
        shield: Charged::full(v(attr::SHIELD_CAPACITY, 0.0), v(attr::SHIELD_RECHARGE_RATE, 3_600_000.0), now),
        armor_max: v(attr::ARMOR_HP, 0.0),
        armor_damage: 0.0,
        hull_max: v(attr::HP, 1.0),
        hull_damage: 0.0,
        capacitor: Charged::full(0.0, 1.0, now),
        resonance: [
            attr::SHIELD_RESONANCE.map(|i| v(i, 1.0)),
            attr::ARMOR_RESONANCE.map(|i| v(i, 1.0)),
            attr::HULL_RESONANCE.map(|i| v(i, 1.0)),
        ],
    }
}

/// One pulse: steps whose timers ran out finish. Unanchoring sets the
/// structure loose; onlining brings it online.
pub fn step(park: &mut Park, out: &mut Out) {
    let now = crate::bulkdata::filetime_now();
    let now_ms = crate::combat::now_ms();
    let (tick, system) = (park.tick, park.solar_system_id);
    for s in park.structures.values_mut() {
        for e in reconcile_field(s, now, tick) {
            out.destiny(system, e);
        }
        // Recovered past a quarter shield: it may ask for reinforcement again.
        if s.reinforce_asked && !s.invulnerable() && s.vitals.damage_state(now_ms).0 >= 0.25 {
            s.reinforce_asked = false;
        }
        if s.stamp.is_none() || s.busy(now) {
            continue;
        }
        let loose = s.state == state::UNANCHORED;
        if matches!(s.state, state::ONLINING | state::REINFORCED) {
            s.state = state::ONLINE;
        }
        if loose {
            s.tower = None;
        }
        s.stamp = None;
        if loose {
            for e in free_entries(tick, s.id, true) {
                out.destiny(system, e);
            }
        }
        out.destiny(system, slim_change(tick, s));
        out.jobs.push(Job::Structure { id: s.id, state: s.state, stamp: None, tower: s.tower });
    }
}

/// Raises, lowers or retunes a tower's force field to match the tower;
/// returns the destiny entries that tell the system.
fn reconcile_field(s: &mut Structure, now: i64, tick: i64) -> Vec<Value> {
    let wanted = s.wanted_field(now);
    match (wanted, s.field.clone()) {
        (None, None) => Vec::new(),
        (None, Some(f)) => {
            s.field = None;
            vec![entry(tick, "RemoveBalls", vec![Value::List(vec![Value::Int(f.id)])])]
        }
        (Some((harmonic, corp_id, alliance_id)), None) => {
            let f = Field { id: crate::npc::next_id(), radius: s.field_radius, harmonic, corp_id, alliance_id };
            let blob = crate::destiny::encode(crate::destiny::stamp(tick), &[f.ball(s)]);
            let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![f.slim(s)]), Value::Dict(Vec::new())]);
            s.field = Some(f);
            vec![entry(tick, "AddBalls", vec![chunk])]
        }
        (Some(key), Some(f)) if key != (f.harmonic, f.corp_id, f.alliance_id) => {
            let (harmonic, corp_id, alliance_id) = key;
            s.field = Some(Field { harmonic, corp_id, alliance_id, ..f.clone() });
            vec![harmonic_entry(tick, f.id, key, true)]
        }
        _ => Vec::new(),
    }
}

/// SetBallHarmonic(ballID, harmonic, corporationID, allianceID, isField).
pub fn harmonic_entry(tick: i64, id: i64, (harmonic, corp, alliance): (i64, i64, i64), field: bool) -> Value {
    let args = vec![Value::Int(id), Value::Long(harmonic), Value::Int(corp), Value::Int(alliance), Value::Int(field.into())];
    entry(tick, "SetBallHarmonic", args)
}
