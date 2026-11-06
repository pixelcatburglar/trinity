//! What lives in a solar system's asteroid belts: the asteroids, and the
//! pirates that wait among them for whoever comes to mine.
//!
//! A system's [`Wildlife`] is read from the store when its park is made:
//! which pirate faction holds it (from the system's or region's faction), the
//! rats of that faction by hull class, and the ores its security allows. Each
//! belt gets a field of asteroids at once; its rats spawn when a player comes
//! within [`SPAWN_RANGE`] and respawn some minutes after the last one dies.
//!
//! Rats are ordinary ships in the park with a [`Brain`]: they pick the nearest
//! player in range (or whoever shoots them), lock, orbit at their preferred
//! range and fire on the module cycles `fight` runs for everyone.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};

use eve_marshal::Value;
use tracing::{debug, info};

use crate::ballpark::{Command, Park, Pilot, Ship, ShipStats, Vec3};
use crate::combat::{self, attr, Action, Active, Charged, Damage, Fighter, Gun, Lock, Vitals, REPEAT_FOREVER};
use crate::heartbeat::Out;
use crate::store::{Db, StoreError};
use crate::svc::space;

/// Players this close to a belt wake its rats.
pub const SPAWN_RANGE: f64 = 150_000.0;
/// How long a cleared belt stays empty, ms.
const RESPAWN_MS: (f64, f64) = (240_000.0, 480_000.0);
/// Rats notice players this far off, at least.
const MIN_AGGRO_RANGE: f64 = 30_000.0;
/// A rat gives up on a target this far from its belt.
const LEASH: f64 = 120_000.0;
/// How far off a bot sees an enemy of its war worth going for.
const WAR_AGGRO_RANGE: f64 = 80_000.0;
const THINK_MS: i64 = 1_000;
const GROUP_BELT: i64 = 9;
const GROUP_CONCORD: i32 = 301;
const OWNER_SYSTEM: i64 = 1;

/// Transient objects (rats, asteroids) get IDs from here: above every item
/// the database will make for a long time, below destiny's 32-bit limit.
static NEXT_ID: AtomicI64 = AtomicI64::new(1_500_000_000);

pub fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Hull classes, smallest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Frigate,
    Destroyer,
    Cruiser,
    Battlecruiser,
    Battleship,
}

const CLASSES: [Class; 5] = [Class::Frigate, Class::Destroyer, Class::Cruiser, Class::Battlecruiser, Class::Battleship];

/// A pirate faction's belt rat groups (in [`CLASSES`] order) and the
/// corporation its rats belong to.
struct Pirates {
    name: &'static str,
    corp_id: i64,
    groups: [i32; 5],
}

const ANGELS: Pirates = Pirates { name: "Angel Cartel", corp_id: 1000124, groups: [550, 575, 551, 576, 552] };
const BLOOD_RAIDERS: Pirates = Pirates { name: "Blood Raiders", corp_id: 1000134, groups: [557, 577, 555, 578, 556] };
const GURISTAS: Pirates = Pirates { name: "Guristas", corp_id: 1000127, groups: [562, 579, 561, 580, 560] };
const SANSHA: Pirates = Pirates { name: "Sansha's Nation", corp_id: 1000162, groups: [567, 581, 566, 582, 565] };
const SERPENTIS: Pirates = Pirates { name: "Serpentis", corp_id: 1000135, groups: [572, 583, 571, 584, 570] };
const ALL_PIRATES: [&Pirates; 5] = [&ANGELS, &BLOOD_RAIDERS, &GURISTAS, &SANSHA, &SERPENTIS];

/// Who raids an empire's space; a pirate faction's own space is its own.
fn pirates_for(faction_id: Option<i64>, solar_system_id: i64) -> &'static Pirates {
    match faction_id {
        Some(500001 | 500010) => &GURISTAS,
        Some(500002 | 500011 | 500015) => &ANGELS,
        Some(500003 | 500007 | 500008 | 500012) => &BLOOD_RAIDERS,
        Some(500019) => &SANSHA,
        Some(500004 | 500009 | 500014 | 500020) => &SERPENTIS,
        _ => ALL_PIRATES[(solar_system_id % ALL_PIRATES.len() as i64) as usize],
    }
}

/// Ores by the lowest security they appear in: (typeID, security floor).
const ORES: [(i64, f64); 16] = [
    (1230, -1.0), // Veldspar
    (1228, -1.0), // Scordite
    (1224, 0.7),  // Pyroxeres
    (18, 0.5),    // Plagioclase
    (1227, 0.4),  // Omber
    (20, 0.4),    // Kernite
    (1226, 0.2),  // Jaspet
    (1231, 0.2),  // Hemorphite
    (21, 0.2),    // Hedbergite
    (1229, 0.0),  // Gneiss
    (1232, 0.0),  // Dark Ochre
    (19, 0.0),    // Spodumain
    (1225, 0.0),  // Crokite
    (1223, 0.0),  // Bistot
    (22, 0.0),    // Arkonor
    (11396, 0.0), // Mercoxit
];

#[derive(Debug, Clone)]
pub struct NpcType {
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub radius: f64,
    pub mass: f64,
    attrs: HashMap<i32, f64>,
}

impl NpcType {

    pub fn get(&self, a: i32) -> f64 {
        self.attrs.get(&a).copied().unwrap_or(0.0)
    }

    /// This type with some attributes as computed for one item (a drone with
    /// its pilot's skills applied).
    pub fn with_attributes(&self, computed: Option<&HashMap<i32, f64>>) -> NpcType {
        let mut t = self.clone();
        if let Some(c) = computed {
            t.attrs.extend(c.iter().map(|(a, v)| (*a, *v)));
        }
        t
    }

    fn bounty(&self) -> f64 {
        self.get(attr::ENTITY_KILL_BOUNTY)
    }

    /// Whether it carries a gun or a missile it can use.
    fn armed(&self, missiles: &HashMap<i64, NpcType>) -> bool {
        let gun = self.get(attr::DAMAGE_MULTIPLIER) > 0.0 && self.get(attr::SPEED) > 0.0;
        let missile = missiles.contains_key(&(self.get(attr::ENTITY_MISSILE_TYPE_ID) as i64));
        gun || missile
    }
}

#[derive(Debug, Clone)]
pub struct Ore {
    pub type_id: i64,
    pub name: String,
    pub volume: f64,
}

/// What a solar system's belts hold.
#[derive(Debug, Clone, Default)]
pub struct Wildlife {
    pub security: f64,
    pub corp_id: i64,
    /// Rat types by class, weakest (by bounty) first.
    rats: [Vec<NpcType>; 5],
    /// The missiles rats fire, by type.
    missiles: HashMap<i64, NpcType>,
    pub ores: Vec<Ore>,
    /// CONCORD's ships, for when someone commits a crime here.
    pub police: Vec<NpcType>,
}

/// An asteroid.
#[derive(Debug, Clone)]
pub struct Rock {
    pub id: i64,
    pub type_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    /// Units of ore left.
    pub quantity: i64,
    /// m3 per unit.
    pub volume: f64,
}

#[derive(Debug, Clone)]
pub struct Belt {
    pub id: i64,
    pub pos: Vec3,
    pub rats: Vec<i64>,
    /// When the rats may come (back); None until first visited.
    pub respawn_at: Option<i64>,
}

/// A missile in flight.
#[derive(Debug, Clone)]
pub struct Missile {
    pub attacker: combat_ref::Attacker,
    pub target: i64,
    pub impact_ms: i64,
    pub damage: Damage,
    pub cloud: f64,
    pub aoe_velocity: f64,
    /// False if the target is beyond the missile's flight range.
    pub reaches: bool,
    pub weapon: String,
}

/// Who fired, as it was at the time (it may be gone when the shot lands).
pub mod combat_ref {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Attacker {
        pub ship_id: i64,
        pub char_id: i64,
        /// The connection to tell, if a player fired.
        pub client_id: Option<i64>,
        /// For the kill report: what it flew and fired.
        pub ship_type: i64,
        pub weapon_type: i64,
    }
}

/// A periodic self-repair.
#[derive(Debug, Clone, Copy)]
struct Heal {
    amount: f64,
    every_ms: i64,
    next_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Brain {
    pub belt: i64,
    pub bounty: f64,
    pub target: Option<i64>,
    attack_range: f64,
    fly_range: f64,
    /// Orbit speed as a fraction of top speed.
    cruise: f64,
    next_think_ms: i64,
    /// The weapons it fires once locked, ready to be aimed.
    weapons: Vec<Active>,
    shield_boost: Option<Heal>,
    armor_repair: Option<Heal>,
    chasing: bool,
    /// What killing it does for a pilot's security status: a share of the
    /// way to 10, but not past the maximum.
    pub security_bonus: f64,
    pub security_max_gain: f64,
    /// How many stacks of loot it drops, at least and at most.
    pub loot_count: (usize, usize),
    /// CONCORD: here for one criminal, gone when done.
    pub police: Option<Police>,
    /// Fights only those who attack it (a bot minding its own business).
    passive: bool,
    /// What it goes back to when the fight is over, instead of its belt (a
    /// bot's errand).
    pub resume: Option<Command>,
}

#[derive(Debug, Clone, Copy)]
pub struct Police {
    pub criminal: i64,
    /// When it leaves if the criminal has gone.
    pub leave_ms: i64,
}

impl Brain {
    /// Someone shot it: shoot back, unless it is busy with someone else.
    pub fn provoked(&mut self, by: i64) {
        if self.target.is_none() {
            self.target = Some(by);
            self.next_think_ms = 0;
        }
    }

    fn aggro_range(&self) -> f64 {
        if self.passive {
            return 0.0;
        }
        (self.attack_range * 2.0).max(MIN_AGGRO_RANGE)
    }

    /// In a fight.
    pub fn engaged(&self) -> bool {
        self.target.is_some()
    }
}

/// How a bot fights: it shoots back at whoever shoots it, with `weapons`
/// from `attack_range`, orbiting at `fly_range`; unless `passive`, it also
/// goes for any player it comes across. After a fight it takes up `resume`.
pub fn bot_brain(weapons: Vec<Active>, attack_range: f64, fly_range: f64, passive: bool, resume: Command, now: i64) -> Brain {
    Brain {
        belt: 0,
        bounty: 0.0,
        target: None,
        attack_range,
        fly_range,
        cruise: 0.5,
        next_think_ms: now + THINK_MS,
        weapons,
        shield_boost: None,
        armor_repair: None,
        chasing: false,
        security_bonus: 0.0,
        security_max_gain: 0.0,
        loot_count: (0, 0),
        police: None,
        passive,
        resume: Some(resume),
    }
}

/// A turret for a ship without an entity's weapon attributes (a bot's
/// hull): `damage` per shot of each type, every `duration` ms.
pub fn bot_gun(t: &NpcType, ship_id: i64, damage: [f64; 4], gun: Gun, duration: f64) -> Active {
    entity_module(t, slot::gun(ship_id), EFFECT_TARGET_ATTACK, GUID_TURRET, Action::Turret { damage, gun }, duration)
}

// ---- loading ------------------------------------------------------------------------

pub fn load(db: &Db, solar_system_id: i64) -> Result<Wildlife, StoreError> {
    let system = db.table("mapSolarSystems").get_id(solar_system_id);
    // A system without a faction of its own takes its region's.
    let (security, faction) = system.map_or((1.0, None), |r| {
        let faction = r.int("factionID").or_else(|| {
            let region = r.int("regionID")?;
            crate::sde::table("mapRegions").get("regionID", region)?.int("factionID")
        });
        (r.float("security").unwrap_or(0.0), faction)
    });
    let pirates = pirates_for(faction, solar_system_id);

    let sde = crate::sde::get();
    let by_id: HashMap<i64, NpcType> = pirates
        .groups
        .iter()
        .flat_map(|g| sde.types_in_group(*g as i64))
        .filter_map(|id| npc_type(*id as i64))
        .map(|t| (t.type_id, t))
        .collect();

    let missile_ids: Vec<i32> = by_id
        .values()
        .map(|t| t.get(attr::ENTITY_MISSILE_TYPE_ID) as i32)
        .filter(|id| *id > 0)
        .collect();
    let missiles: HashMap<i64, NpcType> =
        missile_ids.iter().filter_map(|id| npc_type(*id as i64)).map(|t| (t.type_id, t)).collect();

    let mut rats: [Vec<NpcType>; 5] = Default::default();
    for t in by_id.into_values() {
        if t.bounty() <= 0.0 || t.get(attr::HP) <= 0.0 || !t.armed(&missiles) {
            continue;
        }
        if let Some(class) = pirates.groups.iter().position(|g| *g as i64 == t.group_id) {
            rats[class].push(t);
        }
    }
    for list in rats.iter_mut() {
        list.sort_by(|a, b| a.bounty().total_cmp(&b.bounty()).then(a.type_id.cmp(&b.type_id)));
    }

    let ore_ids: Vec<i32> = ORES.iter().filter(|(_, floor)| security >= *floor).map(|(id, _)| *id as i32).collect();
    // In the static data's order, as the query this replaced gave them.
    let ores = crate::sde::table("invTypes")
        .rows()
        .filter(|r| r.int("typeID").is_some_and(|id| ore_ids.contains(&(id as i32))))
        .map(|r| Ore {
            type_id: r.int("typeID").unwrap_or_default(),
            name: r.text("typeName").unwrap_or_default().to_string(),
            volume: r.float("volume").unwrap_or(1.0).max(0.01),
        })
        .collect::<Vec<_>>();

    let counts: Vec<usize> = rats.iter().map(Vec::len).collect();
    debug!(
        "system {solar_system_id}: security {security:.2}, {} rats {counts:?}, {} ores",
        pirates.name,
        ores.len()
    );
    let police: Vec<NpcType> = load_group(GROUP_CONCORD);
    Ok(Wildlife { security, corp_id: pirates.corp_id, rats, missiles, ores, police })
}

/// Every type in a group, with its attributes.
fn load_group(group_id: i32) -> Vec<NpcType> {
    let mut types: Vec<NpcType> =
        crate::sde::get().types_in_group(group_id as i64).iter().filter_map(|id| npc_type(*id as i64)).collect();
    types.sort_by_key(|t| t.type_id);
    types
}

/// Types with their attributes, for entities that are not belt rats (drones,
/// CONCORD). The static data is in memory; the store handle and the error
/// stay for the callers written against the query this once was.
pub fn load_types(_db: &Db, type_ids: &[i64]) -> Result<HashMap<i64, NpcType>, StoreError> {
    Ok(type_ids.iter().filter_map(|id| npc_type(*id)).map(|t| (t.type_id, t)).collect())
}

/// A type as the static data describes it, with its attributes; None for a
/// type it does not know.
fn npc_type(type_id: i64) -> Option<NpcType> {
    let r = crate::sde::table("invTypes").get("typeID", type_id)?;
    Some(NpcType {
        type_id,
        group_id: r.int("groupID")?,
        name: r.text("typeName").unwrap_or_default().to_string(),
        radius: r.float("radius").unwrap_or(0.0),
        mass: r.float("mass").unwrap_or(0.0),
        attrs: crate::sde::get().type_attributes(type_id).iter().copied().collect(),
    })
}

// ---- the belts ------------------------------------------------------------------------

/// Lays out the belts of a new park and fills them with asteroids.
pub fn populate(park: &mut Park) {
    let belts: Vec<(i64, Vec3)> =
        park.celestials.iter().filter(|c| c.group_id == GROUP_BELT).map(|c| (c.id, c.pos)).collect();
    let ores = park.wildlife.ores.clone();
    for (id, pos) in belts {
        park.belts.push(Belt { id, pos, rats: Vec::new(), respawn_at: None });
        if ores.is_empty() {
            continue;
        }
        let count = 8 + park.rng.below(8);
        for _ in 0..count {
            // The common ores are the common ones.
            let pick = park.rng.below(ores.len()).min(park.rng.below(ores.len()));
            let ore = &ores[pick];
            // Roughly 2,000 to 10,000 m3 of ore.
            let quantity = (park.rng.range(2_000.0, 10_000.0) / ore.volume).round().max(1.0) as i64;
            let offset = park.rng.in_sphere(25_000.0);
            let rock = Rock {
                id: next_id(),
                type_id: ore.type_id,
                name: ore.name.clone(),
                pos: pos + Vec3::new(offset.x, offset.y * 0.2, offset.z),
                radius: rock_radius(quantity as f64 * ore.volume),
                quantity,
                volume: ore.volume,
            };
            park.rocks.insert(rock.id, rock);
        }
    }
    info!(
        "system {}: {} belt(s), {} asteroid(s)",
        park.solar_system_id,
        park.belts.len(),
        park.rocks.len()
    );
}

/// An asteroid's size from the ore in it.
fn rock_radius(m3: f64) -> f64 {
    (800.0 + 40.0 * m3.cbrt()).min(12_000.0)
}

/// Asteroids are celestial-like; this is their slim.
pub fn rock_slim(rock: &Rock) -> Value {
    space::slim(vec![
        ("itemID", Value::Int(rock.id)),
        ("typeID", Value::Int(rock.type_id)),
        ("ownerID", Value::Int(OWNER_SYSTEM)),
    ])
}

// ---- thinking -------------------------------------------------------------------------

/// Spawns rats where players have come, and lets every rat think.
/// A pilot in the park, as those who might fight it see it.
#[derive(Debug, Clone, Copy)]
struct Seen {
    id: i64,
    pos: Vec3,
    player: bool,
    /// Its side in a war: its alliance, or its corporation outside one.
    side: i64,
    char_id: i64,
}

pub fn think(park: &mut Park, now: i64, out: &mut Out) {
    let seen: Vec<Seen> = park
        .ships
        .values()
        .filter(|s| s.is_pilot() && !s.warping() && !s.cloaked)
        .map(|s| Seen { id: s.id, pos: s.pos, player: s.is_player(), side: crate::svc::war::side(&s.pilot), char_id: s.pilot.char_id })
        .collect();
    let players: Vec<(i64, Vec3)> = seen.iter().filter(|s| s.player).map(|s| (s.id, s.pos)).collect();
    spawn_where_needed(park, now, &players, out);
    let due: Vec<i64> = park
        .ships
        .values()
        .filter(|s| !s.warping() && s.npc.as_ref().is_some_and(|b| b.police.is_none() && b.next_think_ms <= now))
        .map(|s| s.id)
        .collect();
    if due.is_empty() {
        return;
    }
    let enemies = enemies(park, &seen);
    for id in due {
        think_one(park, id, now, &seen, &enemies, out);
    }
}

/// The pairs of sides here at war with each other, both ways round: whom
/// armed bots fight on sight.
fn enemies(park: &Park, seen: &[Seen]) -> HashSet<(i64, i64)> {
    let mut found = HashSet::new();
    if !park.ships.values().any(|s| s.bot && s.npc.is_some()) {
        return found;
    }
    let mut sides: Vec<i64> = seen.iter().map(|s| s.side).collect();
    sides.sort_unstable();
    sides.dedup();
    for (i, a) in sides.iter().enumerate() {
        for b in &sides[i + 1..] {
            if crate::svc::war::sides_at_war(*a, *b) {
                found.insert((*a, *b));
                found.insert((*b, *a));
            }
        }
    }
    found
}

fn spawn_where_needed(park: &mut Park, now: i64, players: &[(i64, Vec3)], out: &mut Out) {
    for i in 0..park.belts.len() {
        let belt = &park.belts[i];
        if !belt.rats.is_empty() || belt.respawn_at.is_some_and(|t| t > now) {
            continue;
        }
        if players.iter().any(|(_, p)| (*p - belt.pos).len() < SPAWN_RANGE) {
            spawn(park, i, now, out);
        }
    }
}

/// How many of which class, by security: frigates in high-sec, bigger hulls
/// the lower it goes.
fn plan(security: f64, rng: &mut combat::Rng) -> Vec<Class> {
    let n = |rng: &mut combat::Rng, lo: usize, hi: usize| lo + rng.below(hi - lo + 1);
    let mut out = Vec::new();
    let mut add = |class: Class, count: usize| out.extend(std::iter::repeat(class).take(count));
    match security {
        s if s >= 0.7 => add(Class::Frigate, n(rng, 1, 3)),
        s if s >= 0.5 => {
            add(Class::Frigate, n(rng, 2, 3));
            add(Class::Destroyer, n(rng, 0, 1));
        }
        s if s > 0.0 => {
            add(Class::Cruiser, n(rng, 1, 2));
            add(Class::Destroyer, n(rng, 0, 2));
            add(Class::Frigate, n(rng, 1, 2));
        }
        s if s > -0.5 => {
            add(Class::Battlecruiser, n(rng, 0, 1));
            add(Class::Cruiser, n(rng, 2, 3));
            add(Class::Frigate, n(rng, 0, 2));
        }
        _ => {
            add(Class::Battleship, n(rng, 1, 2));
            add(Class::Cruiser, n(rng, 1, 3));
        }
    }
    out
}

/// Which third of a class's rats (by bounty) a system gets.
fn tier(security: f64, len: usize) -> std::ops::Range<usize> {
    let third = (len / 3).max(1);
    let start = match security {
        s if s >= 0.5 => 0,
        s if s > 0.0 => third,
        _ => len.saturating_sub(third),
    };
    start.min(len.saturating_sub(1))..(start + third).min(len)
}

fn spawn(park: &mut Park, belt_index: usize, now: i64, out: &mut Out) {
    let belt_id = park.belts[belt_index].id;
    let centre = park.belts[belt_index].pos;
    let security = park.wildlife.security;
    let ids = spawn_group(park, belt_id, centre, security, now);
    park.belts[belt_index].rats = ids.clone();
    if ids.is_empty() {
        // Nothing to spawn here; do not try again every pulse.
        park.belts[belt_index].respawn_at = Some(now + RESPAWN_MS.1 as i64);
        return;
    }
    info!("belt {belt_id}: {} rat(s) spawned", ids.len());
    for id in ids {
        for e in space::add_ball_entries(park, id) {
            out.destiny(park.solar_system_id, e);
        }
    }
}

/// A group of rats about `anchor` (a belt, or a mission site), as strong as
/// `security` calls for; returns their IDs. They are in the park but not yet
/// shown to anyone.
pub fn spawn_group(park: &mut Park, anchor: i64, centre: Vec3, security: f64, now: i64) -> Vec<i64> {
    let wildlife = std::sync::Arc::clone(&park.wildlife);
    let mut ids = Vec::new();
    for class in plan(security, &mut park.rng) {
        // A class this faction has none of falls back to the next smaller one.
        let index = CLASSES.iter().position(|c| *c == class).unwrap_or(0);
        let Some(list) = (0..=index).rev().map(|i| &wildlife.rats[i]).find(|l| !l.is_empty()) else { continue };
        let range = tier(security, list.len());
        let t = &list[range.start + park.rng.below(range.len())];
        let offset = park.rng.in_sphere(20_000.0);
        let rat = make_rat(t, &wildlife, anchor, centre + Vec3::new(offset.x, offset.y * 0.2, offset.z), now, &mut park.rng);
        ids.push(rat.id);
        park.ships.insert(rat.id, rat);
    }
    ids
}

fn make_rat(t: &NpcType, wildlife: &Wildlife, belt: i64, pos: Vec3, now: i64, rng: &mut combat::Rng) -> Ship {
    let id = next_id();
    let a = |x: i32| t.get(x);
    let cruise_speed = a(attr::ENTITY_CRUISE_SPEED).max(50.0);
    // Their listed top speed is a chase speed; keep it within reason.
    let top = a(attr::MAX_VELOCITY).clamp(cruise_speed, cruise_speed * 2.5);
    let agility = if a(attr::AGILITY) > 0.0 { a(attr::AGILITY) } else { 1.0 };
    let mass = if t.mass > 0.0 { t.mass } else { 1_000_000.0 };
    let stats = ShipStats::new(t.radius.max(20.0), mass, agility, top, 1.0);
    let pilot = Pilot {
        type_id: t.type_id,
        group_id: t.group_id,
        name: t.name.clone(),
        char_id: 0,
        corp_id: wildlife.corp_id,
        alliance_id: None,
        security: 0.0,
        bounty: 0.0,
    };
    let attack_range = a(attr::ENTITY_ATTACK_RANGE).max(a(attr::MAX_RANGE)).max(5_000.0);
    let fly_range = a(attr::ENTITY_FLY_RANGE).max(1_000.0);
    let fighter = entity_fighter(t, attack_range * 2.0, 1, top, now);
    let heal = |amount: i32, every: i32| {
        let (amount, every) = (a(amount), a(every) as i64);
        (amount > 0.0 && every > 0).then_some(Heal { amount, every_ms: every, next_ms: now })
    };
    let brain = Brain {
        belt,
        bounty: t.bounty(),
        target: None,
        attack_range,
        fly_range,
        cruise: (cruise_speed / top).clamp(0.1, 1.0),
        next_think_ms: now + rng.range(0.0, THINK_MS as f64) as i64,
        weapons: weapons(t, &wildlife.missiles, id, rng),
        shield_boost: heal(attr::ENTITY_SHIELD_BOOST_AMOUNT, attr::ENTITY_SHIELD_BOOST_DURATION),
        armor_repair: heal(attr::ENTITY_ARMOR_REPAIR_AMOUNT, attr::ENTITY_ARMOR_REPAIR_DURATION),
        chasing: false,
        security_bonus: a(attr::ENTITY_SECURITY_STATUS_KILL_BONUS),
        security_max_gain: a(attr::ENTITY_SECURITY_MAX_GAIN),
        loot_count: (a(attr::ENTITY_LOOT_COUNT_MIN) as usize, a(attr::ENTITY_LOOT_COUNT_MAX) as usize),
        police: None,
        passive: false,
        resume: None,
    };
    let mut ship = Ship::new(id, 0, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.npc = Some(brain);
    ship.command = Command::Orbit { target: belt, range: fly_range.max(5_000.0) };
    ship.speed_fraction = (cruise_speed / top).clamp(0.1, 1.0);
    ship
}

/// A CONCORD ship come for `criminal`: nothing it carries is fair, and
/// nothing a criminal has will stop it.
pub fn police_ship(t: &NpcType, criminal: i64, pos: Vec3, now: i64) -> Ship {
    const CONCORD: i64 = 1000125;
    let id = next_id();
    let top = t.get(attr::MAX_VELOCITY).max(1_000.0);
    let stats = ShipStats::new(t.radius.max(50.0), t.mass.max(1_000_000.0), 0.5, top, 5.0);
    let pilot = Pilot {
        type_id: t.type_id,
        group_id: t.group_id,
        name: t.name.clone(),
        char_id: 0,
        corp_id: CONCORD,
        alliance_id: None,
        security: 0.0,
        bounty: 0.0,
    };
    let mut fighter = entity_fighter(t, 250_000.0, 1, top, now);
    fighter.vitals.resonance = [[0.0; 4]; 3];
    fighter.vitals.hull_max = fighter.vitals.hull_max.max(1.0e6);
    fighter.scan_resolution = 5_000.0;
    fighter.sensors = [1_000.0; 4];
    let gun = Gun { optimal: 250_000.0, falloff: 50_000.0, tracking: 100.0, optimal_sig: 1.0 };
    let mut weapons = vec![entity_module(t, slot::gun(id), EFFECT_TARGET_ATTACK, GUID_TURRET, Action::Turret { damage: [0.0, 0.0, 2_500.0, 2_500.0], gun }, 2_000.0)];
    let mut scram = entity_module(t, slot::scrambler(id), EFFECT_WARP_SCRAMBLE_FOR_ENTITY, GUID_SCRAMBLE, Action::Scramble(100.0), 5_000.0);
    scram.reach = Some((250_000.0, 0.0));
    weapons.push(scram);
    let brain = Brain {
        belt: 0,
        bounty: 0.0,
        target: None,
        attack_range: 250_000.0,
        fly_range: 2_500.0,
        cruise: 1.0,
        next_think_ms: now,
        weapons,
        shield_boost: None,
        armor_repair: None,
        chasing: false,
        security_bonus: 0.0,
        security_max_gain: 0.0,
        loot_count: (0, 0),
        police: Some(Police { criminal, leave_ms: now + POLICE_LINGER_MS }),
        passive: false,
        resume: None,
    };
    let mut ship = Ship::new(id, 0, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.npc = Some(brain);
    ship
}

/// How long CONCORD stays once there is no one left to punish.
pub const POLICE_LINGER_MS: i64 = 30_000;

impl Brain {
    /// The weapons it fires once locked, ready to be aimed.
    pub fn arms(&self) -> &[Active] {
        &self.weapons
    }
}

fn resonance(t: &NpcType, a: i32) -> f64 {
    t.attrs.get(&a).copied().unwrap_or(1.0)
}

/// Hit points, resistances and sensors of an entity type (a rat, a drone, a
/// policeman), at full health.
pub fn entity_fighter(t: &NpcType, lock_range: f64, max_locked: usize, top_speed: f64, now: i64) -> Fighter {
    let a = |x: i32| t.get(x);
    let vitals = Vitals {
        shield: Charged::full(a(attr::SHIELD_CAPACITY), a(attr::SHIELD_RECHARGE_RATE), now),
        armor_max: a(attr::ARMOR_HP),
        armor_damage: 0.0,
        hull_max: a(attr::HP).max(1.0),
        hull_damage: 0.0,
        capacitor: Charged::full(a(attr::CAPACITOR_CAPACITY), a(attr::RECHARGE_RATE), now),
        resonance: [
            attr::SHIELD_RESONANCE.map(|r| resonance(t, r)),
            attr::ARMOR_RESONANCE.map(|r| resonance(t, r)),
            attr::HULL_RESONANCE.map(|r| resonance(t, r)),
        ],
    };
    let mut f = Fighter::new(vitals, a(attr::SIGNATURE_RADIUS).max(10.0), a(attr::SCAN_RESOLUTION).max(50.0), lock_range, max_locked);
    f.top_speed = top_speed;
    f.sensors = attr::SENSOR_STRENGTHS.map(a);
    f
}

/// Weapon IDs of an entity, which has no modules to tell its guns apart by.
pub mod slot {
    pub fn gun(ship: i64) -> i64 {
        ship
    }
    pub fn launcher(ship: i64) -> i64 {
        -ship
    }
    pub fn web(ship: i64) -> i64 {
        ship + 1_000_000_000_000
    }
    pub fn scrambler(ship: i64) -> i64 {
        ship + 2_000_000_000_000
    }
}

/// A module template for an entity's weapon.
pub fn entity_module(t: &NpcType, module_id: i64, effect_id: i64, guid: &str, action: Action, duration: f64) -> Active {
    Active {
        module_id,
        type_id: t.type_id,
        effect_id,
        guid: guid.to_string(),
        offensive: true,
        // These entity templates are weapons/webs/scramblers, all unsafe.
        warp_safe: false,
        disallow_warp_alignment: false,
        action,
        target: None,
        ammo: None,
        duration_ms: duration.max(500.0) as i64,
        cap_need: 0.0,
        first_cycle_prepaid: false,
        started_ms: 0,
        next_ms: 0,
        cycles: 0,
        repeat: REPEAT_FOREVER,
        stopping: false,
        name: t.name.clone(),
        reach: None,
        landed: true,
    }
}

/// An entity's guns and launchers (and, by chance, its web and scrambler), as
/// module templates aimed later.
pub fn weapons(t: &NpcType, missiles: &HashMap<i64, NpcType>, ship_id: i64, rng: &mut combat::Rng) -> Vec<Active> {
    let mut out = Vec::new();
    let template = |module_id: i64, effect_id: i64, guid: &str, action: Action, duration: f64| {
        entity_module(t, module_id, effect_id, guid, action, duration)
    };
    let multiplier = t.get(attr::DAMAGE_MULTIPLIER);
    if multiplier > 0.0 && t.get(attr::SPEED) > 0.0 {
        let damage = attr::DAMAGE_TYPES.map(|d| t.get(d) * multiplier);
        if damage.iter().sum::<f64>() > 0.0 {
            let gun = Gun {
                optimal: t.get(attr::MAX_RANGE),
                falloff: t.get(attr::FALLOFF),
                tracking: t.get(attr::TRACKING_SPEED),
                optimal_sig: t.get(attr::OPTIMAL_SIG_RADIUS),
            };
            out.push(template(slot::gun(ship_id), EFFECT_TARGET_ATTACK, GUID_TURRET, Action::Turret { damage, gun }, t.get(attr::SPEED)));
        }
    }
    let missile_type = t.get(attr::ENTITY_MISSILE_TYPE_ID) as i64;
    if let Some(m) = missiles.get(&missile_type) {
        let multiplier = if t.get(attr::MISSILE_DAMAGE_MULTIPLIER) > 0.0 { t.get(attr::MISSILE_DAMAGE_MULTIPLIER) } else { 1.0 };
        let damage = attr::DAMAGE_TYPES.map(|d| m.get(d) * multiplier);
        let scale = |a: i32| if t.get(a) > 0.0 { t.get(a) } else { 1.0 };
        let velocity = m.get(attr::MAX_VELOCITY) * scale(MISSILE_VELOCITY_MULTIPLIER);
        let flight_ms = m.get(attr::EXPLOSION_DELAY) * scale(MISSILE_FLIGHT_MULTIPLIER);
        let rate = t.get(attr::MISSILE_LAUNCH_DURATION);
        let rate = if rate >= 1_000.0 { rate } else { 10_000.0 };
        if damage.iter().sum::<f64>() > 0.0 && velocity > 0.0 {
            let action = Action::Missile {
                damage,
                flight_ms: flight_ms as i64,
                range: velocity * flight_ms / 1_000.0,
                cloud: m.get(attr::AOE_CLOUD_SIZE) * scale(MISSILE_CLOUD_MULTIPLIER),
                aoe_velocity: m.get(attr::AOE_VELOCITY) * scale(MISSILE_AOE_VELOCITY_MULTIPLIER),
            };
            let mut launcher = template(slot::launcher(ship_id), EFFECT_MISSILE_LAUNCHING, GUID_MISSILE, action, rate);
            launcher.name = m.name.clone();
            launcher.type_id = m.type_id;
            out.push(launcher);
        }
    }
    // Some carry a web or a scrambler; whether this one does is luck.
    let web_chance = t.get(attr::MODIFY_TARGET_SPEED_CHANCE);
    let web_range = t.get(attr::MODIFY_TARGET_SPEED_RANGE);
    if web_chance > 0.0 && web_range > 0.0 && rng.unit() < web_chance {
        let factor = if t.get(attr::SPEED_FACTOR) < 0.0 { 1.0 + t.get(attr::SPEED_FACTOR) / 100.0 } else { 0.5 };
        let duration = t.get(attr::MODIFY_TARGET_SPEED_DURATION).max(5_000.0);
        let mut web = template(slot::web(ship_id), EFFECT_MODIFY_TARGET_SPEED, GUID_WEB, Action::Web(factor.max(0.05)), duration);
        web.reach = Some((web_range, 0.0));
        out.push(web);
    }
    let scram_range = t.get(attr::WARP_SCRAMBLE_RANGE);
    if t.get(attr::ENTITY_WARP_SCRAMBLE_CHANCE) > 0.0 && scram_range > 0.0 && rng.unit() < t.get(attr::ENTITY_WARP_SCRAMBLE_CHANCE) {
        let mut scram = template(slot::scrambler(ship_id), EFFECT_WARP_SCRAMBLE_FOR_ENTITY, GUID_SCRAMBLE, Action::Scramble(1.0), 5_000.0);
        scram.reach = Some((scram_range, 0.0));
        out.push(scram);
    }
    out
}

const EFFECT_MODIFY_TARGET_SPEED: i64 = 575;
const EFFECT_WARP_SCRAMBLE_FOR_ENTITY: i64 = 563;
const GUID_WEB: &str = "effects.ModifyTargetSpeed";
const GUID_SCRAMBLE: &str = "effects.WarpScramble";

const EFFECT_TARGET_ATTACK: i64 = 10;
const EFFECT_MISSILE_LAUNCHING: i64 = 569;
const GUID_TURRET: &str = "effects.Laser";
const GUID_MISSILE: &str = "effects.MissileDeployment";
const MISSILE_VELOCITY_MULTIPLIER: i32 = 645;
const MISSILE_FLIGHT_MULTIPLIER: i32 = 646;
const MISSILE_CLOUD_MULTIPLIER: i32 = 858;
const MISSILE_AOE_VELOCITY_MULTIPLIER: i32 = 859;

fn think_one(park: &mut Park, id: i64, now: i64, seen: &[Seen], enemies: &HashSet<(i64, i64)>, out: &mut Out) {
    // A bot may be fighting a rat (a wingman's): where that rat is.
    let rat_target = park
        .ships
        .get(&id)
        .filter(|s| s.bot)
        .and_then(|s| s.npc.as_ref()?.target)
        .and_then(|t| park.ships.get(&t))
        .filter(|t| t.is_npc())
        .map(|t| t.pos);
    let Some(rat) = park.ships.get_mut(&id) else { return };
    // A bot also fights its war's enemies, wherever they come from, and
    // players its side holds to be hostile; and leaves its friends alone.
    let (is_bot, my_side) = (rat.bot, crate::svc::war::side(&rat.pilot));
    let me = if is_bot { Some(rat.pilot.clone()) } else { None };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    brain.next_think_ms = now + THINK_MS;

    // Patch up between fights, and during them.
    for (heal, armor) in [(&mut brain.shield_boost, false), (&mut brain.armor_repair, true)] {
        let Some(h) = heal.as_mut() else { continue };
        let hurt = if armor {
            fight.vitals.armor_damage > 0.25 * fight.vitals.armor_max
        } else {
            fight.vitals.shield.fraction(now) < 0.75
        };
        if hurt && now >= h.next_ms {
            if armor {
                fight.vitals.repair_armor(h.amount);
            } else {
                fight.vitals.shield.add(now, h.amount);
            }
            h.next_ms = now + h.every_ms;
            out.hurt(id);
        }
    }

    let pos = rat.pos;
    let belt_pos = park.belts.iter().find(|b| b.id == brain.belt).map_or(pos, |b| b.pos);
    let leash = brain.aggro_range().max(LEASH);

    // Is the current target still worth it?
    if let Some(t) = brain.target {
        let keep = seen
            .iter()
            .find(|s| s.id == t && (s.player || is_bot))
            .is_some_and(|s| (s.pos - pos).len() <= leash && (s.pos - belt_pos).len() <= LEASH * 1.5)
            || rat_target.is_some_and(|p| (p - pos).len() <= leash.max(WAR_AGGRO_RANGE * 2.0));
        if !keep {
            disengage(park, id, now, out);
            return;
        }
    }

    let Some(rat) = park.ships.get(&id) else { return };
    let Some(brain) = rat.npc.as_ref() else { return };
    let target = match brain.target {
        Some(t) => t,
        None => {
            let aggro = brain.aggro_range();
            let nearest = seen
                .iter()
                .filter(|s| s.id != id)
                .filter_map(|s| {
                    let stance = match (&me, s.player) {
                        (Some(me), true) => crate::bots::stance(me, s.char_id),
                        _ => crate::bots::Stance::Neutral,
                    };
                    let reach = if is_bot && enemies.contains(&(my_side, s.side)) {
                        WAR_AGGRO_RANGE.max(aggro)
                    } else if s.player && stance == crate::bots::Stance::Hostile {
                        WAR_AGGRO_RANGE.max(aggro)
                    } else if s.player && stance == crate::bots::Stance::Friendly {
                        return None;
                    } else if s.player {
                        aggro
                    } else {
                        return None;
                    };
                    let d = (s.pos - pos).len();
                    (d <= reach).then_some((s.id, d))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some((t, _)) = nearest else { return };
            t
        }
    };
    let signature = park.ships.get(&target).and_then(|s| s.fight.as_ref()).map_or(35.0, |f| f.signature);
    let distance = park.surface_distance(id, target).unwrap_or(f64::MAX);

    // Lock it.
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    brain.target = Some(target);
    // Aggro reaches further than sensors: a lock started out of range, or
    // while jammed, would be dropped by the next pulse and started over.
    let can_lock = distance <= fight.lock_range() && !fight.afflictions.jammed;
    if can_lock && !fight.locks.contains_key(&target) {
        fight.locks.clear();
        let lock_ms = (combat::lock_seconds(fight.scan_resolution, signature) * 1_000.0) as i64;
        fight.locks.insert(target, Lock::Pending(now + lock_ms));
        debug!("rat {id} locking {target} ({lock_ms} ms)");
    }

    // Close in at full speed, then orbit at its preferred range.
    let chase = distance > brain.attack_range;
    let orbit = Command::Orbit { target, range: brain.fly_range };
    let cruise = brain.cruise;
    let course_changed = rat.command != orbit || brain.chasing != chase;
    brain.chasing = chase;
    if course_changed {
        park.command(id, orbit);
        park.set_speed_fraction(id, if chase { 1.0 } else { cruise });
        let tick = park.tick;
        if let Some(rat) = park.ships.get(&id) {
            for e in space::course_entries(rat, tick) {
                out.destiny(park.solar_system_id, e);
            }
        }
    }

    // Once locked, open fire.
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    if !fight.is_locked(target) {
        return;
    }
    let mut started = Vec::new();
    for w in &brain.weapons {
        match fight.module_mut(w.module_id) {
            Some(running) => running.target = Some(target),
            None => {
                let mut m = w.clone();
                m.target = Some(target);
                m.started_ms = now;
                // Not everyone fires the instant they lock.
                m.next_ms = now + park.rng.range(0.0, m.duration_ms as f64 * 0.5) as i64;
                started.push(m.clone());
                fight.modules.push(m);
            }
        }
    }
    let tick = park.tick;
    for m in started {
        out.destiny(park.solar_system_id, crate::fight::module_fx(tick, id, &m, true));
    }
}

/// Stop fighting and go back to the belt.
fn disengage(park: &mut Park, id: i64, now: i64, out: &mut Out) {
    let modules: Vec<i64> = park
        .ships
        .get(&id)
        .and_then(|s| s.fight.as_ref())
        .map(|f| f.modules.iter().map(|m| m.module_id).collect())
        .unwrap_or_default();
    for m in modules {
        crate::fight::stop(park, id, m, now, out, None);
    }
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    let dropped: Vec<(i64, Lock)> = std::mem::take(&mut fight.locks).into_iter().collect();
    brain.target = None;
    brain.chasing = false;
    let home = brain.resume.clone().unwrap_or(Command::Orbit { target: brain.belt, range: brain.fly_range.max(5_000.0) });
    let cruise = if matches!(home, Command::Warp { .. }) { 1.0 } else { brain.cruise };
    for (t, lock) in dropped {
        if lock == Lock::Locked {
            crate::fight::tell_target_lost(park, id, t, out);
        }
    }
    park.command(id, home);
    park.set_speed_fraction(id, cruise);
    if let Some(rat) = park.ships.get(&id) {
        for e in space::course_entries(rat, park.tick) {
            out.destiny(park.solar_system_id, e);
        }
    }
    debug!("NPC {id} disengages");
}

/// A rat died: its belt starts the clock on a new spawn once it is empty.
pub fn killed(park: &mut Park, ship: &Ship, now: i64, out: &mut Out) {
    let Some(brain) = &ship.npc else { return };
    if let Some((mission_id, char_id)) = crate::sites::rat_killed(park, brain.belt, ship.id) {
        // A tutorial site belongs to no mission: clearing it is only practice.
        if mission_id != crate::sites::TUTORIAL {
            out.jobs.push(crate::heartbeat::Job::SiteCleared { mission_id, char_id });
        }
        return;
    }
    let killer = ship.fight.as_ref().and_then(|f| f.last_attacker).map(|(c, _)| c).filter(|c| *c > 0);
    if crate::complexes::rat_killed(park, brain.belt, ship.id, killer, out) {
        return;
    }
    let Some(belt) = park.belts.iter_mut().find(|b| b.id == brain.belt) else { return };
    belt.rats.retain(|r| *r != ship.id);
    if belt.rats.is_empty() {
        let wait = park.rng.range(RESPAWN_MS.0, RESPAWN_MS.1) as i64;
        belt.respawn_at = Some(now + wait);
        info!("belt {} cleared; rats return in {}s", belt.id, wait / 1_000);
    }
}
