//! Server-side destiny: what every ship in a solar system is doing and where it is.
//!
//! The client runs destiny.dll on the commands we send it, and it never hears
//! a position from us again until the next SetState or AddBalls: those, the
//! other players who see a ship, and every range check (docking, jumping,
//! locking, looting) read this simulation. The original 2.10 port now has a
//! bounded Trinity 45017 instruction comparison for ordinary steering,
//! collision, warp and orbit-target release (compatibility/destiny-45017).
//! Full native parity still requires client validation. Wherever it drifts,
//! two players see a ship in two places.
//!
//! One tick per second (const.simulationTimeStep). Each tick, as Evolve does it:
//!
//! 1. every ship's mode picks a thrust (a goal velocity divided by tau);
//! 2. every massive ship works out whether it will touch another massive ball
//!    during the tick, and adds a thrust that bounces it off;
//! 3. every ship is integrated exactly over the tick: velocity converges on
//!    tau * thrust with tau = mass * agility / friction; or, in warp, follows
//!    the warp's closed-form profile.
//!
//! Addresses in the notes are destiny.dll 2.10.3913.

use std::collections::BTreeMap;
use std::ops::{Add, Mul, Sub};
use std::sync::Arc;

use crate::combat::{Fighter, Rng};
use crate::npc::{Belt, Brain, Missile, Rock, Wildlife};

mod persistence;
mod candidates;
mod partition;
pub use persistence::Motion;

/// Ticks a park evolves at most per request; a park left alone longer than this
/// skips ahead instead (its ships have long since stopped).
const MAX_CATCH_UP: i64 = 3600;

/// Seconds per tick (Ballpark+0x60).
const DT: f64 = 1.0;
/// Ballpark.friction, "Friction coefficient of space": tau = inertia / friction.
const FRICTION: f64 = 1.0e6;
/// What a ball that is not free weighs, for collisions (0x1005a290).
const IMMOVABLE: f64 = 1.0e34;
/// How far ahead GotoDirection puts its point (0x1005a620).
const GOTO_DIRECTION_REACH: f64 = 1.0e17;
/// STOP bleeds off vertical velocity each tick: (vy - 0.07 vy) / 1.07.
const STOP_LEVELLING: f64 = 0.07;
const STOP_LEVELLING_DIV: f64 = 1.07;
/// Orbit: the reference axis turns by speed * 0.01 / radius radians a tick,
/// and wobbles at 0.7 of that around the ball's own ID.
const ORBIT_TURN: f64 = 0.01;
const ORBIT_WOBBLE: f64 = 0.7;
/// Orbit: how sharply the circle pulls, exp((R - d)^2 / -40000).
const ORBIT_PULL: f64 = -40_000.0;
/// Native 45017 orbit stores position + thrust * 10 AU as its GOTO goal.
const ORBIT_GOAL_SCALE: f64 = 1_495_978_707_000.0;
/// Collision thrust is scaled down by this (0x1005a268).
const BOUNCE: f64 = 0.85;
/// WarpTo closer than this (squared, 100 km) is a GotoPoint (0x10059548).
const MIN_WARP_DISTANCE_SQ: f64 = 1.0e10;
/// A warp starts once 1 - cos(heading, target) is below this and the speed
/// is over 0.75 of the maximum (squared: 0.5625).
const WARP_ALIGN: f64 = 0.01;
const WARP_ENTRY_SPEED_SQ: f64 = 0.5625;
/// Warp speed per point of warp factor: 0.1 AU/s (0x1005a598).
const WARP_FACTOR_SPEED: f64 = 14_959_787_070.0;
/// The warp's top speed is also limited to 1.5 times its distance per second.
const WARP_DISTANCE_SPEED: f64 = 1.5;
/// Warp factor of a warpSpeedMultiplier-1 ship: 3 AU/s.
const BASE_WARP_FACTOR: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }
    pub fn len(self) -> f64 {
        self.len_sq().sqrt()
    }
    pub fn len_sq(self) -> f64 {
        self.dot(self)
    }
    /// Coordinates must also leave room for the squared distances used by
    /// steering and collisions. Finite components alone can still overflow.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.len_sq().is_finite()
    }
    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - o.y * self.z, o.x * self.z - self.x * o.z, o.y * self.x - self.y * o.x)
    }
    /// The unit vector; a zero vector stays zero (destiny's Normalize, 0x100014e0).
    pub fn unit(self) -> Vec3 {
        let l = self.len();
        if l == 0.0 { self } else { self * (1.0 / l) }
    }
    pub fn tuple(self) -> (f64, f64, f64) {
        (self.x, self.y, self.z)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}
impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}
impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, k: f64) -> Vec3 {
        Vec3::new(self.x * k, self.y * k, self.z * k)
    }
}

const GROUP_STATION: i64 = 15;

/// A fixed object in the system: sun, planet, moon, belt, stargate, station.
#[derive(Debug, Clone)]
pub struct Celestial {
    pub id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    /// For a stargate: (destination solarSystemID, destination gate).
    pub jump: Option<(i64, i64)>,
}

impl Celestial {
    /// Whether ships bounce off it. Not a station: its undock point lies
    /// inside its radius, so a massive station would throw every ship that
    /// leaves it.
    pub fn massive(&self) -> bool {
        self.group_id != GROUP_STATION
    }
}

/// What a ship's type makes it do.
#[derive(Debug, Clone, Copy)]
pub struct ShipStats {
    pub radius: f64,
    pub mass: f64,
    pub agility: f64,
    pub max_velocity: f64,
    /// WarpTo's warp factor: tenths of an AU per second.
    pub warp_factor: i64,
    /// Effective dogma attribute 153: capacitor cost per kg per AU.
    pub warp_capacitor_need: f64,
}

impl ShipStats {
    pub fn new(radius: f64, mass: f64, agility: f64, max_velocity: f64, warp_multiplier: f64) -> Self {
        let multiplier = if warp_multiplier > 0.0 { warp_multiplier } else { 1.0 };
        ShipStats {
            radius,
            mass,
            agility,
            max_velocity,
            warp_factor: ((BASE_WARP_FACTOR * multiplier).round() as i64).max(1),
            warp_capacitor_need: 0.0,
        }
    }

    /// mass * agility: what thrust is divided by.
    fn inertia(&self) -> f64 {
        (self.mass * self.agility).max(f64::MIN_POSITIVE)
    }

}

/// Who flies a ship, for its SlimItem.
#[derive(Debug, Clone)]
pub struct Pilot {
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub char_id: i64,
    pub corp_id: i64,
    pub alliance_id: Option<i64>,
    pub security: f64,
    pub bounty: f64,
}

/// The movement command a ship is executing, as the client was told it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Command {
    Stop,
    Direction(Vec3),
    Point(Vec3),
    Follow { target: i64, range: f64 },
    Orbit { target: i64, range: f64 },
    /// Warp towards `target`, stopping `range` short of it.
    Warp { target: Vec3, range: f64 },
}

pub fn valid_command(ship_id: i64, pos: Vec3, command: &Command) -> bool {
    match command {
        Command::Stop => true,
        Command::Direction(v) => v.is_finite(),
        Command::Point(v) => v.is_finite() && (*v - pos).is_finite(),
        Command::Follow { target, range } | Command::Orbit { target, range } =>
            *target != ship_id && range.is_finite() && *range >= 0.0 && range * range < f64::MAX,
        Command::Warp { target, range } => target.is_finite() && (*target - pos).is_finite()
            && range.is_finite() && *range >= 0.0 && (*range as f32).is_finite()
            && (*range == 0.0 || *range < (*target - pos).len()),
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum WarpState {
    /// Turning and speeding up towards the warp point.
    Aligning,
    /// On the way: `start` is the tick the warp began, `distance` from where
    /// it began to `exit`.
    InWarp { exit: Vec3, distance: f64, start: i32 },
}

/// Native Trinity WARP mode payload (ReadBallFromStream 0x10031b54).
/// During alignment `target` is the destination and `start` is -1; after
/// entry it is the exit point and the original distance/time must survive
/// a client ballpark reset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarpSnapshot {
    pub target: Vec3,
    pub start: i32,
    pub distance: f64,
    pub range: f32,
    pub factor: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WarpChange {
    Entered { cost: f64, limited: bool },
    Stopped { scrambled: bool },
}

#[derive(Debug, Clone)]
pub struct Ship {
    pub id: i64,
    pub client_id: i64,
    /// A hull left in space without a pilot. Pilot metadata still records
    /// its property owner for inventory and loss reporting.
    pub unpiloted: bool,
    /// A transactional cockpit transfer is preparing to commit. Combat still
    /// changes the hull, but its identity and removal wait for that decision.
    pub identity_reserved: bool,
    pub stats: ShipStats,
    pub pilot: Pilot,
    pub pos: Vec3,
    pub vel: Vec3,
    pub speed_fraction: f64,
    pub command: Command,
    /// Where GOTO steers (the point, the far point of a direction, the warp
    /// point); ball+0xd0.
    goal: Vec3,
    /// The velocity a tick ago, which collisions start from (ball+0x90).
    prev_vel: Vec3,
    warp: Option<WarpState>,
    /// Hit points, locks and running modules; None for a ship that cannot fight.
    pub fight: Option<Fighter>,
    /// What an NPC thinks, or how a bot fights; None for a player's ship.
    pub npc: Option<Brain>,
    /// A player's drone: whose, and what it was told to do.
    pub drone: Option<crate::drones::Drone>,
    /// The shield harmonic its pilot set (SetShipPassword); -1 for none.
    pub harmonic: i64,
    /// The gate it just came out of, until its pilot's SetState shows the
    /// arrival.
    pub arrived_through: Option<i64>,
    /// Flown by the bot simulation: a pilot like a player's, with no client.
    pub bot: bool,
    /// A bot ship's fitting and cargo, (type, quantity): what its wreck holds
    /// and its killmail lists.
    pub bot_loot: Vec<(i64, i64)>,
    /// Cloaked: only its pilot sees it (`cloak`).
    pub cloaked: bool,
}

impl Ship {
    pub fn new(id: i64, client_id: i64, stats: ShipStats, pilot: Pilot, pos: Vec3, vel: Vec3) -> Self {
        Ship {
            id,
            client_id,
            unpiloted: false,
            identity_reserved: false,
            stats,
            pilot,
            pos,
            vel,
            speed_fraction: 1.0,
            command: Command::Stop,
            goal: pos,
            // A new ball starts with no previous velocity (0x1000cf90).
            prev_vel: Vec3::ZERO,
            warp: None,
            fight: None,
            npc: None,
            drone: None,
            harmonic: crate::destiny::NO_HARMONIC,
            arrived_through: None,
            bot: false,
            bot_loot: Vec::new(),
            cloaked: false,
        }
    }

    pub fn in_warp(&self) -> bool {
        matches!(self.warp, Some(WarpState::InWarp { .. }))
    }

    /// Aligning for or in warp.
    pub fn warping(&self) -> bool {
        self.warp.is_some()
    }

    /// Relocation changes ballparks rather than issuing a Destiny course.
    /// Discard private motion state so an old warp cannot resume after a
    /// jump or a GM teleport into another system.
    pub fn arrive(&mut self, at: Vec3) {
        self.pos = at;
        self.vel = Vec3::ZERO;
        self.prev_vel = Vec3::ZERO;
        self.goal = at;
        self.warp = None;
        self.command = Command::Stop;
    }

    pub fn warp_snapshot(&self) -> Option<WarpSnapshot> {
        let Command::Warp { range, .. } = self.command else { return None; };
        let (target, start, distance) = match self.warp? {
            WarpState::Aligning => (self.goal, -1, 0.0),
            WarpState::InWarp { exit, distance, start } => (exit, start, distance),
        };
        Some(WarpSnapshot { target, start, distance, range: range as f32, factor: self.stats.warp_factor as i32 })
    }

    /// A ship a connected player flies.
    pub fn is_player(&self) -> bool {
        self.is_pilot() && !self.bot
    }

    /// A ship with a character at the helm: a player's or a bot's (a bot's
    /// fights with an NPC's brain, but is flown by a character all the same).
    pub fn is_pilot(&self) -> bool {
        !self.unpiloted && (self.npc.is_none() || self.bot) && self.drone.is_none()
    }

    /// An NPC: a rat, CONCORD, a mission's enemies.
    pub fn is_npc(&self) -> bool {
        self.npc.is_some() && !self.bot
    }

    /// Native massive flag. Collision queries separately exclude cloaked targets.
    pub fn massive(&self) -> bool {
        !self.in_warp() && !self.cloaked
    }

    fn cruise(&self) -> f64 {
        self.speed_fraction * self.stats.max_velocity
    }
}

/// One solar system's moving objects.
#[derive(Debug, Clone)]
pub struct Park {
    pub solar_system_id: i64,
    /// The tick the ships' state is valid for.
    pub tick: i64,
    pub celestials: Vec<Celestial>,
    pub ships: BTreeMap<i64, Ship>,
    /// Asteroids, by ID.
    pub rocks: BTreeMap<i64, Rock>,
    /// Cargo containers floating here, by item ID.
    pub cans: BTreeMap<i64, crate::loot::Can>,
    /// Starbase structures, by item ID.
    pub structures: BTreeMap<i64, crate::pos::Structure>,
    /// Agents' mission sites, by beacon ID.
    pub sites: BTreeMap<i64, crate::sites::Site>,
    /// Cynosural fields, complex beacons and gates, scanner probes.
    pub beacons: BTreeMap<i64, crate::beacons::Beacon>,
    /// Deadspace complexes, as they are being played.
    pub complexes: BTreeMap<i64, crate::complexes::Complex>,
    /// Bots whose ships were destroyed here, for the bot simulation to
    /// collect.
    pub bot_losses: Vec<BotLoss>,
    /// Starbases under attack since the bots last looked: (tower, the
    /// attacking ship), for their owners to answer.
    pub tower_alarms: Vec<(i64, i64)>,
    /// Aggression and crime here.
    pub watch: crate::concord::Watch,
    pub belts: Vec<Belt>,
    /// Missiles on their way to a target.
    pub missiles: Vec<Missile>,
    /// What lives in this system's belts.
    pub wildlife: Arc<Wildlife>,
    pub rng: Rng,
    /// Physics can advance from an RPC as well as the heartbeat. Retain
    /// capacitor changes and corrected courses until the heartbeat reports them.
    pub warp_events: Vec<(i64, WarpChange)>,
    /// Persistent native-shaped collision neighborhood and hash-table history.
    partition: partition::Index,
}

/// A bot ship or capsule destroyed in space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotLoss {
    pub char_id: i64,
    /// The capsule its pilot ejected in; None when it was the capsule.
    pub pod: Option<i64>,
    /// The character who landed the final blow; 0 for none (or an NPC).
    pub killer: i64,
    /// What was lost.
    pub type_id: i64,
}

impl Park {
    pub fn new(solar_system_id: i64, celestials: Vec<Celestial>, tick: i64) -> Self {
        Park {
            solar_system_id,
            tick,
            celestials,
            ships: BTreeMap::new(),
            rocks: BTreeMap::new(),
            cans: BTreeMap::new(),
            structures: BTreeMap::new(),
            sites: BTreeMap::new(),
            beacons: BTreeMap::new(),
            complexes: BTreeMap::new(),
            bot_losses: Vec::new(),
            tower_alarms: Vec::new(),
            watch: Default::default(),
            belts: Vec::new(),
            missiles: Vec::new(),
            wildlife: Arc::new(Wildlife::default()),
            rng: Rng::seeded(solar_system_id as u64 ^ tick as u64),
            warp_events: Vec::new(),
            partition: Default::default(),
        }
    }

    pub fn celestial(&self, id: i64) -> Option<&Celestial> {
        self.celestials.iter().find(|c| c.id == id)
    }

    /// Position and radius of any ball in the park.
    pub fn ball(&self, id: i64) -> Option<(Vec3, f64)> {
        if let Some(s) = self.ships.get(&id) {
            return Some((s.pos, s.stats.radius));
        }
        if let Some(r) = self.rocks.get(&id) {
            return Some((r.pos, r.radius));
        }
        if let Some(c) = self.cans.get(&id) {
            return Some((c.pos, c.radius));
        }
        if let Some(s) = self.structures.get(&id) {
            return Some((s.pos, s.radius));
        }
        if let Some(s) = self.sites.get(&id) {
            return Some((s.pos, s.radius()));
        }
        if let Some(b) = self.beacons.get(&id) {
            return Some((b.pos, b.radius));
        }
        self.celestial(id).map(|c| (c.pos, c.radius))
    }

    /// Players' ships.
    pub fn players(&self) -> impl Iterator<Item = &Ship> {
        self.ships.values().filter(|s| s.is_player())
    }

    /// Surface-to-surface distance between two balls.
    pub fn surface_distance(&self, a: i64, b: i64) -> Option<f64> {
        let (pa, ra) = self.ball(a)?;
        let (pb, rb) = self.ball(b)?;
        Some(((pa - pb).len() - ra - rb).max(0.0))
    }

    /// Brings every ship forward to `tick`.
    pub fn advance(&mut self, tick: i64) {
        if tick <= self.tick {
            return;
        }
        self.tick = self.tick.max(tick - MAX_CATCH_UP);
        while self.tick < tick {
            self.step();
        }
    }

    /// Gives a ship a new command at the park's current tick, as destiny's
    /// FollowBall, Orbit, GotoPoint, GotoDirection, WarpTo and Stop take it.
    pub fn command(&mut self, ship_id: i64, command: Command) {
        let Some(ship) = self.ships.get_mut(&ship_id) else { return };
        // Destiny does not accept another course after entering warp. Keep
        // internal callers subject to the same rule as beyonce RPCs.
        if ship.identity_reserved || ship.in_warp() || !valid_command(ship_id, ship.pos, &command) {
            return;
        }
        let command = match command {
            // Stop on a stopped ship changes nothing (0x10017800).
            Command::Stop if ship.command == Command::Stop && ship.warp.is_none() => return,
            // Too close to warp: destiny goes there instead (0x10017bb0).
            Command::Warp { target, .. } if (target - ship.pos).len_sq() < MIN_WARP_DISTANCE_SQ => Command::Point(target),
            other => other,
        };
        ship.warp = None;
        match &command {
            Command::Point(p) => ship.goal = *p,
            Command::Direction(d) => ship.goal = ship.pos + d.unit() * GOTO_DIRECTION_REACH,
            Command::Warp { target, .. } => {
                ship.goal = *target;
                ship.warp = Some(WarpState::Aligning);
            }
            _ => {}
        }
        // A GOTO with the throttle at zero opens it (0x10017730).
        if matches!(command, Command::Point(_) | Command::Direction(_)) && ship.speed_fraction == 0.0 {
            ship.speed_fraction = 1.0;
        }
        ship.command = command;
    }

    pub fn set_speed_fraction(&mut self, ship_id: i64, fraction: f64) {
        if !fraction.is_finite() { return; }
        if let Some(ship) = self.ships.get_mut(&ship_id) {
            if ship.identity_reserved || ship.in_warp() { return; }
            ship.speed_fraction = fraction.clamp(0.0, 1.0);
        }
    }

    /// Seconds until `ship_id` is within `range` of `target`'s surface, if that
    /// happens within `limit` seconds on its current course.
    pub fn eta_within(&self, ship_id: i64, target: i64, range: f64, limit: i64) -> Option<i64> {
        let mut future = self.clone();
        for t in 0..=limit {
            if future.surface_distance(ship_id, target)? <= range {
                return Some(t);
            }
            future.step();
        }
        None
    }

    /// One tick of Ballpark::Evolve (0x100189c0).
    fn step(&mut self) {
        // The clock destiny reads during the tick: the stamp, as a C int.
        let now = crate::destiny::stamp(self.tick) as i32;
        self.sync_partitions();
        let thrust = self.thrusts(now);
        let bounce = self.bounces(&thrust);
        for ship in self.ships.values_mut() {
            let push = thrust.get(&ship.id).copied().unwrap_or(Vec3::ZERO) + bounce.get(&ship.id).copied().unwrap_or(Vec3::ZERO);
            move_ship(ship, push, now);
        }
        self.sync_partitions();
        self.tick += 1;
    }

    fn sync_partitions(&mut self) {
        let fixed=|id,pos,radius|partition::Ball {id,pos,vel:Vec3::ZERO,radius};
        // Nonmassive and cloaked occupants still affect native cell occupancy
        // and pointer-set growth; collision eligibility is checked by query.
        let mut balls:Vec<_>=self.ships.values().map(|s|partition::Ball {
            id:s.id,pos:s.pos,vel:s.vel,radius:s.stats.radius}).collect();
        balls.extend(self.rocks.values().map(|s|fixed(s.id,s.pos,s.radius)));
        balls.extend(self.celestials.iter().map(|s|fixed(s.id,s.pos,s.radius)));
        balls.extend(self.cans.values().map(|s|fixed(s.id,s.pos,s.radius)));
        balls.extend(self.structures.values().map(|s|fixed(s.id,s.pos,s.radius)));
        balls.extend(self.sites.values().map(|s|fixed(s.id,s.pos,s.radius())));
        balls.extend(self.beacons.values().map(|s|fixed(s.id,s.pos,s.radius)));
        for s in self.structures.values() {
            if let Some(f)=&s.field {balls.push(fixed(f.id,s.pos,f.radius));}
        }
        // Stable allocation order across storage categories; candidate order
        // itself remains the native hash/list traversal, not this ID order.
        balls.sort_unstable_by_key(|ball|ball.id);
        self.partition.sync(balls);
    }

    /// Step 1: each ship's thrust from its mode. Modes can change on the way:
    /// a lost target stops a follower, an aligned ship enters warp (and its
    /// followers let go of it).
    fn thrusts(&mut self, now: i32) -> BTreeMap<i64, Vec3> {
        // This step advances the park to tick+1. Use that time for capacitor
        // state, after module cycles run between the two Destiny ticks.
        let capacitor_ms = (self.tick + 1 - crate::combat::filetime(0) / 10_000_000) * 1_000;
        let targets: BTreeMap<i64, (Vec3, f64)> = self
            .ships
            .values()
            .filter(|s| !s.cloaked)
            .map(|s| (s.id, (s.pos, s.stats.radius)))
            .chain(self.rocks.values().map(|r| (r.id, (r.pos, r.radius))))
            .chain(self.cans.values().map(|c| (c.id, (c.pos, c.radius))))
            .chain(self.structures.values().map(|s| (s.id, (s.pos, s.radius))))
            .chain(self.sites.values().map(|s| (s.id, (s.pos, s.radius()))))
            .chain(self.beacons.values().map(|b| (b.id, (b.pos, b.radius))))
            .chain(self.celestials.iter().map(|c| (c.id, (c.pos, c.radius))))
            .collect();
        let mut thrust = BTreeMap::new();
        // Server mode updates use stable ID order. Native active-ball table
        // ordering is a separate remaining Evolve comparison. A warp changes
        // its followers immediately, so later updates see it in this tick.
        let ids: Vec<i64> = self.ships.keys().copied().collect();
        for id in ids {
            let Some(ship) = self.ships.get_mut(&id) else { continue };
            let mut warped = false;
            let push = match ship.command.clone() {
                Command::Stop => {
                    ship.vel.y = (ship.vel.y - ship.vel.y * STOP_LEVELLING) / STOP_LEVELLING_DIV;
                    Vec3::ZERO
                }
                Command::Point(_) | Command::Direction(_) => steer(ship, ship.goal),
                Command::Follow { target, range } => match targets.get(&target) {
                    Some(&(at, radius)) => {
                        ship.goal = follow_point(ship, at, radius + ship.stats.radius + range);
                        steer(ship, ship.goal)
                    }
                    None => lose_target(ship),
                },
                Command::Orbit { target, range } => match targets.get(&target) {
                    Some(&(at, radius)) => orbit(ship, at, radius + ship.stats.radius + range, now),
                    None => {
                        release_orbit(ship);
                        steer(ship, ship.goal)
                    }
                },
                Command::Warp { range, .. } => match ship.warp {
                    Some(WarpState::Aligning) if aligned(ship) => {
                        if let Some(change) = enter_warp(ship, range, now, capacitor_ms) {
                            self.warp_events.push((id, change));
                        }
                        warped = ship.in_warp();
                        Vec3::ZERO
                    }
                    Some(WarpState::Aligning) => steer(ship, ship.goal),
                    _ => Vec3::ZERO,
                },
            };
            thrust.insert(id, push);
            if warped {
                self.let_go_of(id);
            }
        }
        thrust
    }

    /// `warped` went into warp: whoever was following it lets go (0x10017e80).
    /// An orbit carries on in a straight line; anything else stops.
    fn let_go_of(&mut self, warped: i64) {
        for ship in self.ships.values_mut() {
            match ship.command {
                Command::Orbit { target, .. } if target == warped => {
                    release_orbit(ship);
                }
                Command::Follow { target, .. } if target == warped => ship.command = Command::Stop,
                _ => {}
            }
        }
    }

    /// Step 2: the extra thrust each massive ship needs this tick to bounce off
    /// whatever massive ball it is about to touch (0x10017a10, 0x10015b20).
    fn bounces(&self, thrust: &BTreeMap<i64, Vec3>) -> BTreeMap<i64, Vec3> {
        let mut bodies: Vec<Body> = self
            .ships
            .values()
            .filter(|s| s.massive())
            .map(|s| Body {
                id: s.id,
                pos: s.pos,
                vel: s.vel,
                thrust: thrust.get(&s.id).copied().unwrap_or(Vec3::ZERO),
                radius: s.stats.radius,
                inertia: s.stats.inertia(),
                mass: s.stats.mass,
                free: true,
                cloaked: s.cloaked,
                key: (s.harmonic, s.pilot.corp_id, s.pilot.alliance_id.unwrap_or(-1)),
                field: None,
            })
            .collect();
        let movers = bodies.len();
        let fixed = |id: i64, pos: Vec3, radius: f64| Body {
            id,
            pos,
            vel: Vec3::ZERO,
            thrust: Vec3::ZERO,
            radius,
            inertia: IMMOVABLE,
            mass: IMMOVABLE,
            free: false,
            cloaked: false,
            key: (-1, -1, -1),
            field: None,
        };
        bodies.extend(self.rocks.values().map(|r| fixed(r.id, r.pos, r.radius)));
        bodies.extend(self.celestials.iter().filter(|c| c.massive()).map(|c| fixed(c.id, c.pos, c.radius)));
        bodies.extend(self.structures.values().filter(|s| s.fixed()).map(|s| fixed(s.id, s.pos, s.radius)));
        for s in self.structures.values() {
            if let Some(f) = &s.field {
                bodies.push(Body { field: Some((f.harmonic, f.corp_id, f.alliance_id)), ..fixed(f.id, s.pos, f.radius) });
            }
        }

        let mut out = BTreeMap::new();
        let by_id:BTreeMap<_,_>=bodies.iter().map(|body|(body.id,body)).collect();
        for a in &bodies[..movers] {
            let prev_vel = self.ships[&a.id].prev_vel;
            let mut best: Option<(f64, Vec3)> = None;
            // Trinity 45017's query (RVA 0x29819) skips cloaked targets;
            // CloakBall also clears the mover's massive flag.
            let candidates=self.partition.candidates(a.id,|id|by_id.get(&id).is_some_and(|b|
                !b.cloaked && !b.field.is_some_and(|f|passes(f,a.key))));
            for id in candidates {
                let b=by_id[&id];
                let Some((toi, push)) = bounce(a, prev_vel, b) else { continue };
                let push = push * BOUNCE;
                // The latest contact of the tick wins; of two at the same
                // moment, the harder push.
                best = match best {
                    Some((t, p)) if toi < t || (toi == t && push.len_sq() <= p.len_sq()) => Some((t, p)),
                    _ => Some((toi, push)),
                };
            }
            if let Some((_, push)) = best {
                out.insert(a.id, push);
            }
        }
        out
    }
}

/// A ball as a collision sees it.
struct Body {
    id: i64,
    pos: Vec3,
    vel: Vec3,
    thrust: Vec3,
    radius: f64,
    /// mass * agility, or IMMOVABLE.
    inertia: f64,
    mass: f64,
    free: bool,
    cloaked: bool,
    /// Its (harmonic, corporation, alliance), for force fields to test.
    key: (i64, i64, i64),
    /// A force field's (harmonic, corporation, alliance); -1 where unset.
    field: Option<(i64, i64, i64)>,
}

/// Whether a ball keyed (harmonic, corporation, alliance) passes through a
/// force field: the master harmonic, or a matching harmonic, corporation or
/// alliance (destiny 2.10 0x10028d40).
pub fn passes((harmonic, corp, alliance): (i64, i64, i64), (h, c, a): (i64, i64, i64)) -> bool {
    h == -2 || (harmonic != -1 && harmonic == h) || (corp != -1 && corp == c) || (alliance != -1 && alliance == a)
}

/// Exact integration of v' = (tau * thrust - v) / tau over `dt` (0x1000dda0):
/// where a ball at `pos`, moving at `vel`, ends up, and how fast.
fn drift(pos: Vec3, vel: Vec3, thrust: Vec3, inertia: f64, dt: f64) -> (Vec3, Vec3) {
    if inertia >= IMMOVABLE {
        return (pos + vel * dt, vel);
    }
    let tau = inertia / FRICTION;
    let goal = thrust * tau;
    let k = (-dt / tau).exp();
    (pos + goal * dt + (vel - goal) * (tau * (1.0 - k)), goal + (vel - goal) * k)
}

/// Full thrust at a point, easing off over the last tick's worth of travel
/// (0x1000dc70).
fn steer(ship: &Ship, point: Vec3) -> Vec3 {
    let to = point - ship.pos;
    let dist_sq = to.len_sq();
    let cruise = ship.cruise();
    let near = DT * cruise + ship.stats.radius;
    let mut accel = FRICTION * cruise / ship.stats.inertia();
    if dist_sq < near * near {
        let k = dist_sq / (near * near);
        accel *= k * k;
    }
    to.unit() * accel
}

/// FOLLOW's goal: `reach` from the target's centre, on the line towards us.
fn follow_point(ship: &Ship, at: Vec3, reach: f64) -> Vec3 {
    let off = ship.pos - at;
    let dist = off.len();
    if dist == 0.0 {
        return at + Vec3::new(reach, 0.0, 0.0);
    }
    at + off * reach * (1.0 / dist)
}

/// The followed ball is gone: destiny stops the follower.
fn lose_target(ship: &mut Ship) -> Vec3 {
    ship.command = Command::Stop;
    ship.vel.y = (ship.vel.y - ship.vel.y * STOP_LEVELLING) / STOP_LEVELLING_DIV;
    Vec3::ZERO
}

/// RemoveBall and warp entry release interactive orbiters through
/// GotoDirection(current velocity), including GotoPoint's zero-throttle reset
/// (45017 RVAs 0x198c1–0x19903, 0x189ea–0x18a31, 0x18b31–0x18b38).
fn release_orbit(ship:&mut Ship) {
    let heading=ship.vel;
    ship.goal=ship.pos+heading.unit()*GOTO_DIRECTION_REACH;
    ship.command=Command::Direction(heading);
    if ship.speed_fraction==0.0 {ship.speed_fraction=1.0;}
}

/// ORBIT (Evolve, case 4): head for the tangent of the orbit circle, blended
/// with a sideways axis that drifts with time and the ship's ID, at full
/// thrust.
fn orbit(ship: &mut Ship, at: Vec3, reach: f64, now: i32) -> Vec3 {
    let cruise = ship.cruise();
    let accel = FRICTION * cruise / ship.stats.inertia();
    let to = at - ship.pos;
    let dist = to.len();
    let toward = to.unit();
    let turn = if reach != 0.0 { cruise * DT * ORBIT_TURN / reach } else { 0.0 };
    let theta = f64::from(now) * turn;
    let phi = theta * ORBIT_WOBBLE + f64::from(ship.id as i32);
    let axis = Vec3::new(theta.cos() * phi.cos(), phi.sin(), theta.sin() * phi.cos());
    let side = axis.cross(toward).unit();
    let gap = dist * dist - reach * reach;
    let mut heading = toward;
    if gap >= 0.0 && dist > 0.0 {
        heading = (toward * (gap / dist) + side * (gap.sqrt() * reach / dist)).unit();
    }
    let pull = ((reach - dist) * (reach - dist) / ORBIT_PULL).exp();
    let lean = side.dot(heading * -1.0);
    let room = (lean * lean - 1.0) * pull * pull + 1.0;
    let mut along = if room > 0.0 { room.sqrt() + pull * lean } else { pull * lean };
    if dist - reach < 0.0 {
        along = -along;
    }
    let thrust=(side * pull + heading * along) * accel;
    ship.goal=ship.pos+thrust*ORBIT_GOAL_SCALE;
    thrust
}

/// Lined up and fast enough to warp (Evolve, case 3).
fn aligned(ship: &Ship) -> bool {
    let to = (ship.goal - ship.pos).unit();
    let off = (1.0 - to.dot(ship.vel.unit())).abs();
    let v = ship.stats.max_velocity;
    off < WARP_ALIGN && ship.vel.len_sq() > v * v * WARP_ENTRY_SPEED_SQ
}

/// The warp begins (0x100186b0): it ends `range` short of the warp point, on
/// our side of it. destiny keeps the range as a float.
fn enter_warp(ship: &mut Ship, range: f64, now: i32, capacitor_ms: i64) -> Option<WarpChange> {
    // A cockpit/funded-activation transaction must publish its decision before
    // an already queued alignment can enter warp or spend capacitor.
    if ship.identity_reserved { return None; }
    if ship.fight.as_ref().is_some_and(|f| f.afflictions.scrambled >= 1.0
        || f.modules.iter().any(|m| matches!(m.action, crate::combat::Action::Cyno))) {
        ship.warp = None;
        ship.command = Command::Stop;
        return Some(WarpChange::Stopped { scrambled: true });
    }
    let range = f64::from(range as f32);
    // The ship can fly into the requested stopping range while aligning.
    // Starting there would produce a zero-length or backwards warp profile.
    if (ship.goal - ship.pos).len() <= range {
        ship.warp = None;
        ship.command = Command::Stop;
        return Some(WarpChange::Stopped { scrambled: false });
    }
    let mut exit = ship.goal + (ship.pos - ship.goal).unit() * range;
    let distance = (exit - ship.pos).len();
    let available = ship.fight.as_ref().map_or(0.0, |f|f.vitals.capacitor.at(capacitor_ms));
    let Some(quote) = crate::warp_energy::quote(distance,ship.stats.mass,ship.stats.warp_capacitor_need,available)
        .filter(|quote|quote.distance_m > 0.0) else {
        ship.warp = None;
        ship.command = Command::Stop;
        return Some(WarpChange::Stopped { scrambled: false });
    };
    if quote.limited { exit = ship.pos + (exit - ship.pos).unit() * quote.distance_m; }
    let top = (ship.stats.warp_factor as f64 * WARP_FACTOR_SPEED).min(quote.distance_m * WARP_DISTANCE_SPEED);
    // No server minimum distance is inferred here. Only reject a trip whose
    // endpoint cannot be represented, or whose native logarithm underflows.
    if exit == ship.pos || !exit.is_finite() || !(top / 3.0).is_finite() || top / 3.0 <= 0.0 {
        ship.warp = None;
        ship.command = Command::Stop;
        return Some(WarpChange::Stopped { scrambled: false });
    }
    if quote.cost > 0.0 {
        let paid = ship.fight.as_mut().is_some_and(|f| f.vitals.capacitor.spend(capacitor_ms,quote.cost));
        if !paid {
            ship.warp = None;
            ship.command = Command::Stop;
            return Some(WarpChange::Stopped { scrambled: false });
        }
    }
    ship.warp = Some(WarpState::InWarp { exit, distance: quote.distance_m, start: now });
    (quote.cost > 0.0 || quote.limited).then_some(WarpChange::Entered { cost: quote.cost, limited: quote.limited })
}

/// Step 3 for one ship: integrate it, or carry it along its warp.
fn move_ship(ship: &mut Ship, push: Vec3, now: i32) {
    ship.prev_vel = ship.vel;
    if let Some(WarpState::InWarp { exit, distance, start }) = ship.warp {
        warp_along(ship, exit, distance, f64::from(now.wrapping_sub(start)) * DT);
        return;
    }
    let (pos, vel) = drift(ship.pos, ship.vel, push, ship.stats.inertia(), DT);
    ship.pos = pos;
    ship.vel = vel;
}

/// Where a warp `t` seconds old has the ship (0x10017170): out along e^3t,
/// cruise, then in along e^-(t - slowing); it drops out when slower than the
/// ship's own top speed, still moving, and coasts to a stop.
fn warp_along(ship: &mut Ship, exit: Vec3, distance: f64, t: f64) {
    let top = (ship.stats.warp_factor as f64 * WARP_FACTOR_SPEED).min(distance * WARP_DISTANCE_SPEED);
    let cruising = (top / 3.0).ln() / 3.0;
    let slowing = (3.0 * distance + top * ((top / 3.0).ln() - 2.0)) / (3.0 * top);
    let back = (ship.pos - exit).unit();
    let (left, speed) = if t < cruising {
        let gone = (3.0 * t).exp();
        (distance - gone, 3.0 * gone)
    } else if t < slowing {
        (distance - top * (t - cruising + 1.0 / 3.0), top)
    } else {
        let x = ((3.0 * (distance + top * cruising) - (3.0 * top * t + 2.0 * top)) / top) / 3.0;
        let gone = (3.0 * distance - top * x.exp()) / 3.0;
        (distance - gone, (x.exp() * top / 3.0).abs())
    };
    ship.pos = exit + back * left;
    ship.vel = back * -speed;
    if t >= slowing && speed < ship.stats.max_velocity {
        ship.warp = None;
        ship.command = Command::Stop;
    }
}

/// Whether `a` touches `b` during the coming tick, and if so when (a fraction
/// of the tick) and the thrust that takes `a` off it (0x10015b20).
fn bounce(a: &Body, a_prev_vel: Vec3, b: &Body) -> Option<(f64, Vec3)> {
    let reach = a.radius + b.radius;
    let (a1, _) = drift(a.pos, a.vel, a.thrust, a.inertia, DT);
    let (b1, _) = drift(b.pos, b.vel, b.thrust, b.inertia, DT);
    let apart0 = a.pos - b.pos;
    let closing = (a1 - a.pos) - (b1 - b.pos);
    let dist_sq = apart0.len_sq();
    let clear = reach * reach <= dist_sq;
    let toi = match roots(closing.len_sq(), 2.0 * closing.dot(apart0), dist_sq - reach * reach) {
        Some((late, early)) => {
            let mut toi = -1.0;
            if (0.0..=1.0).contains(&late) {
                toi = late;
            }
            if (0.0..=1.0).contains(&early) && early < late {
                toi = early;
            }
            if !clear {
                0.0
            } else if toi < 0.0 {
                return None;
            } else {
                toi
            }
        }
        None if clear => return None,
        None => 0.0,
    };

    let push = if toi <= 0.0 {
        // Already overlapping: shove apart along the line between the
        // centres, a's share by weight.
        let normal = if dist_sq == 0.0 {
            Vec3::new(if a.id > b.id { 1.0 } else { -1.0 }, 0.0, 0.0)
        } else {
            (b.pos - a.pos).unit()
        };
        let share = b.inertia / (b.inertia + a.inertia) * (reach - dist_sq.sqrt());
        let own = normal * normal.dot(a.thrust);
        let speed = -(share / ((1.0 / (DT * FRICTION + a.inertia)) * DT * a.inertia));
        (normal * speed - a.vel) * (1.0 / DT) - own
    } else {
        // Contact during the tick: reflect off a fixed ball, or trade
        // momentum along the normal with a free one, then find the thrust
        // that ends the tick at the bounced velocity.
        let (pa, va) = drift(a.pos, a.vel, a.thrust, a.inertia, toi * DT);
        let (pb, vb) = drift(b.pos, b.vel, b.thrust, b.inertia, toi * DT);
        let normal = (pb - pa).unit();
        let van = normal.dot(va);
        let after = if b.free {
            let vbn = normal.dot(vb);
            let dv = (2.0 * b.mass * vbn + (a.mass * van - b.mass * van)) / (b.mass + a.mass) - van;
            va + normal * dv
        } else {
            va - normal * (2.0 * van)
        };
        let (_, v_end) = drift(pa, after, a.thrust, a.inertia, (1.0 - toi) * DT);
        let k = (-FRICTION * DT / a.inertia).exp();
        (a.thrust * (a.inertia * (k - 1.0)) - a_prev_vel * (FRICTION * k) + v_end * FRICTION) * (-1.0 / a.inertia / (k - 1.0))
    };
    Some((toi, push))
}

/// The roots of a t^2 + b t + c, larger first (0x1000e120).
fn roots(a: f64, b: f64, c: f64) -> Option<(f64, f64)> {
    if a == 0.0 {
        return None;
    }
    let disc = b * b - a * c * 4.0;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    Some(((s - b) * 0.5 / a, (-b - s) * 0.5 / a))
}
