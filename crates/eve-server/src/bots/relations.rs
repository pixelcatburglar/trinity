//! How the bots' sides (alliances, or corporations outside one) come to feel
//! about players, and what they do about it.
//!
//! Every bot a player kills adds to the *heat* its side holds against them,
//! and the bot itself bears a grudge. Heat cools by the day. As it climbs:
//! - the bot that died mails its killer;
//! - the side sets its standing to the player at -10 (`ownerStandings`, so
//!   its towers' guns take it in), and its bots attack them on sight;
//! - it puts a bounty on them (a real one, paid from its executor's wallet);
//! - and, if the player is in a player corporation, declares war on it.
//!
//! Pirates (and fighters of hostile sides) within a few jumps come hunting
//! a player they hold a grudge against.
//!
//! Killing a side's war enemies earns its *favor* instead: it sets its
//! standing to the player at +5 and offers a mercenary contract, paying for
//! every enemy ship they kill from then on; more favor makes them blue (+10),
//! never attacked by its bots, and brings an invitation to join. Bot
//! corporations take any player who applies and is not hostile to them.
//!
//! The other way round, bots in NPC corporations apply to player
//! corporations; a corporation that accepts one gets a member who works near
//! its headquarters, pays its tax, and answers when its towers are attacked.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use eve_marshal::Value;
use tracing::{error, info};

use super::behaviour::{self, Activity, Atlas, Bot, Role};
use super::politics::Politics;
use super::presence::Identity;
use crate::ballpark::Pilot;
use crate::combat::Rng;
use crate::store::{Db, Sets, Store, StoreError, Tx, Value as Field};

pub const HOSTILE: u32 = 1;
pub const BOUNTY: u32 = 2;
pub const WAR: u32 = 4;
pub const CONTRACT: u32 = 8;
pub const BLUE: u32 = 16;
pub const INVITED: u32 = 32;
pub const THREATENED: u32 = 64;

/// Heat at which a side turns on a player, bounties them, and declares war.
const HOSTILE_AT: f32 = 3.0;
const BOUNTY_AT: f32 = 6.0;
const WAR_AT: f32 = 10.0;
/// Favor at which a side hires a player, and asks them to join.
const CONTRACT_AT: f32 = 2.0;
const INVITE_AT: f32 = 6.0;
/// What heat and favor keep of themselves each day of the bots' clock.
const HEAT_KEPT: f32 = 0.85;
const FAVOR_KEPT: f32 = 0.95;
/// How long a bot's personal grudge lasts, on the bots' clock.
const GRUDGE_SECS: u32 = 2 * 86_400;
/// The most bots hunting one player at once, and how far they come from.
const HUNTERS: usize = 5;
const HUNT_JUMPS: i64 = 4;
/// How long a hunt lasts before the hunter gives up, on the bots' clock.
const HUNT_SECS: u32 = 30 * 60;
const BOUNTY_PER_HEAT: f64 = 2_500_000.0;
const WAR_COST: f64 = 2_000_000.0;
const WAR_DAYS: i64 = 7;
/// Mercenary pay for a frigate; bigger hulls pay by the square of their class.
const MERC_PAY: f64 = 750_000.0;
/// A player corporation gets an application from a bot about this often,
/// per politics tick, while it has fewer than `PENDING_MAX` waiting.
const APPLY_CHANCE: f64 = 1.0 / 36.0;
const PENDING_MAX: i64 = 3;
const SECOND_FT: i64 = 10_000_000;
const REF_PLAYER_DONATION: i32 = 10;

/// eveConstants crpApplication*.
const APPLIED_BY_CHARACTER: i16 = 0;
const ACCEPTED_BY_CHARACTER: i16 = 2;
const REJECTED_BY_CORPORATION: i16 = 4;
const ACCEPTED_BY_CORPORATION: i16 = 6;
const APPLICATION_HEADER: &[&str] = &["corporationID", "characterID", "applicationText", "applicationDateTime", "status"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rel {
    pub heat: f32,
    pub favor: f32,
    pub flags: u32,
}

/// A bot killed by a player.
#[derive(Debug, Clone, Copy)]
pub struct Kill {
    pub victim: u32,
    pub killer: i32,
    pub pod: bool,
    /// The lost hull's class (`ships::weight_class`).
    pub class: u8,
    pub system: i32,
}

/// What the relations call for that needs the database.
#[derive(Debug, Clone, PartialEq)]
pub enum Deed {
    Standing { side: i32, player: i32, value: f64 },
    Bounty { side: i32, player: i32, amount: f64 },
    War { side: i32, player: i32 },
    Pay { side: i32, player: i32, amount: f64, class: u8 },
    Mail { from: Sender, player: i32, kind: Letter },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sender {
    /// A bot, by character.
    Pilot(i32),
    /// A side's leader: its executor corporation's CEO.
    Leader(i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Letter {
    Revenge { system: i32 },
    Hostile { side: i32 },
    Bounty { side: i32, amount: f64 },
    War { side: i32 },
    Contract { side: i32, enemy: i32 },
    Invite { side: i32 },
}

pub struct Relations {
    pub map: HashMap<(i32, i32), Rel>,
    dirty: HashSet<(i32, i32)>,
    /// Bots with a grudge: bot index -> (player, until).
    pub grudges: HashMap<u32, (i32, u32)>,
    /// Bots out hunting: bot index -> (player, until).
    pub hunting: HashMap<u32, (i32, u32)>,
    pub deeds: Vec<Deed>,
    pub news: Vec<super::sov::News>,
    next_cooling: u32,
    changed: bool,
    /// When each player last counted for firing on a side's tower.
    tower_shots: HashMap<(i32, i32), u32>,
}

/// What the bots' brains look up, in space: whom each side attacks on sight
/// and whom it leaves alone, and which bots hunt which player.
#[derive(Default)]
struct Stances {
    hostile: HashSet<(i64, i64)>,
    friendly: HashSet<(i64, i64)>,
    grudge: HashMap<i64, i64>,
}

static STANCES: RwLock<Option<Stances>> = RwLock::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    Hostile,
    Neutral,
    Friendly,
}

/// How a bot (by its pilot) stands to another pilot.
pub fn stance(bot: &Pilot, other: i64) -> Stance {
    let Ok(guard) = STANCES.read() else { return Stance::Neutral };
    let Some(s) = guard.as_ref() else { return Stance::Neutral };
    let side = crate::svc::war::side(bot);
    if s.grudge.get(&bot.char_id) == Some(&other) || s.hostile.contains(&(side, other)) {
        Stance::Hostile
    } else if s.friendly.contains(&(side, other)) {
        Stance::Friendly
    } else {
        Stance::Neutral
    }
}

impl Relations {
    pub fn new() -> Relations {
        Relations { map: HashMap::new(), dirty: HashSet::new(), grudges: HashMap::new(), hunting: HashMap::new(), deeds: Vec::new(), news: Vec::new(), next_cooling: 86_400, changed: true, tower_shots: HashMap::new() }
    }

    fn rel(&mut self, side: i32, player: i32) -> &mut Rel {
        self.dirty.insert((side, player));
        self.changed = true;
        self.map.entry((side, player)).or_default()
    }

    pub fn get(&self, side: i32, player: i32) -> Rel {
        self.map.get(&(side, player)).cloned().unwrap_or_default()
    }

    /// A bot of `side` (None: an NPC corporation's) killed by a player;
    /// `enemies` are the sides at war with it.
    pub fn killed(&mut self, k: Kill, victim_char: i32, side: Option<i32>, enemies: &[i32], clock: u32) {
        self.grudges.insert(k.victim, (k.killer, clock + GRUDGE_SECS));
        self.changed = true;
        if let Some(side) = side {
            let heat = if k.pod { 2.0 } else { 1.0 + k.class as f32 * 0.25 };
            let r = self.rel(side, k.killer);
            r.heat += heat;
            let (heat, flags) = (r.heat, r.flags);
            if flags & THREATENED == 0 {
                self.rel(side, k.killer).flags |= THREATENED;
                self.deeds.push(Deed::Mail { from: Sender::Pilot(victim_char), player: k.killer, kind: Letter::Revenge { system: k.system } });
            }
            self.escalate(side, k.killer, heat, flags);
        }
        for &enemy in enemies {
            let gain = 1.0 + k.class as f32 * 0.25;
            let before = self.get(enemy, k.killer);
            if before.flags & HOSTILE != 0 {
                continue;
            }
            if before.flags & CONTRACT != 0 {
                self.deeds.push(Deed::Pay { side: enemy, player: k.killer, amount: MERC_PAY * (1.0 + k.class as f64).powi(2), class: k.class });
            }
            let r = self.rel(enemy, k.killer);
            r.favor += gain;
            let (favor, flags) = (r.favor, r.flags);
            if favor >= CONTRACT_AT && flags & CONTRACT == 0 {
                self.rel(enemy, k.killer).flags |= CONTRACT;
                self.deeds.push(Deed::Standing { side: enemy, player: k.killer, value: 5.0 });
                self.deeds.push(Deed::Mail { from: Sender::Leader(enemy), player: k.killer, kind: Letter::Contract { side: enemy, enemy: side.unwrap_or(0) } });
            }
            if favor >= INVITE_AT && flags & INVITED == 0 {
                self.rel(enemy, k.killer).flags |= INVITED | BLUE;
                self.deeds.push(Deed::Standing { side: enemy, player: k.killer, value: 10.0 });
                self.deeds.push(Deed::Mail { from: Sender::Leader(enemy), player: k.killer, kind: Letter::Invite { side: enemy } });
            }
        }
    }

    /// A player fired on one of a side's towers (counted once in ten
    /// minutes of the bots' clock).
    pub fn tower_attacked(&mut self, side: i32, player: i32, clock: u32) {
        if self.tower_shots.get(&(side, player)).is_some_and(|t| clock < t + 600) {
            return;
        }
        self.tower_shots.insert((side, player), clock);
        let r = self.rel(side, player);
        r.heat += 1.5;
        let (heat, flags) = (r.heat, r.flags);
        self.escalate(side, player, heat, flags);
    }

    fn escalate(&mut self, side: i32, player: i32, heat: f32, flags: u32) {
        let mut set = 0;
        if heat >= HOSTILE_AT && flags & HOSTILE == 0 {
            set |= HOSTILE;
            self.deeds.push(Deed::Standing { side, player, value: -10.0 });
            self.deeds.push(Deed::Mail { from: Sender::Leader(side), player, kind: Letter::Hostile { side } });
        }
        if heat >= BOUNTY_AT && flags & BOUNTY == 0 {
            set |= BOUNTY;
            let amount = (heat as f64 * BOUNTY_PER_HEAT).round();
            self.deeds.push(Deed::Bounty { side, player, amount });
            self.deeds.push(Deed::Mail { from: Sender::Leader(side), player, kind: Letter::Bounty { side, amount } });
        }
        if heat >= WAR_AT && flags & WAR == 0 {
            set |= WAR;
            self.deeds.push(Deed::War { side, player });
        }
        if set != 0 {
            let r = self.rel(side, player);
            // Hostile undoes being blue.
            r.flags = (r.flags | set) & !(BLUE | CONTRACT | INVITED);
        }
    }

    /// Once a day of the bots' clock: heat and favor cool, and a side that
    /// has cooled off forgives (its standing back to nothing).
    pub fn cool(&mut self, clock: u32) {
        if clock < self.next_cooling {
            return;
        }
        self.next_cooling = clock + 86_400;
        let keys: Vec<(i32, i32)> = self.map.keys().copied().collect();
        for key in keys {
            let r = self.map.get_mut(&key).expect("listed");
            r.heat *= HEAT_KEPT;
            r.favor *= FAVOR_KEPT;
            if r.heat < 1.0 && r.flags & (HOSTILE | THREATENED | WAR | BOUNTY) != 0 {
                if r.flags & HOSTILE != 0 {
                    self.deeds.push(Deed::Standing { side: key.0, player: key.1, value: 0.0 });
                }
                r.flags &= !(HOSTILE | THREATENED | WAR | BOUNTY);
            }
            self.dirty.insert(key);
        }
        self.map.retain(|_, r| r.heat >= 0.05 || r.favor >= 0.05 || r.flags != 0);
        self.grudges.retain(|_, (_, until)| *until > clock);
        self.tower_shots.retain(|_, t| clock < *t + 600);
        self.changed = true;
    }

    /// Bots go after players they hold a grudge against, or whose side
    /// is hostile to them, when they are near: the bots set off (their
    /// indexes, with the seconds to their next step, are returned).
    #[allow(clippy::too_many_arguments)]
    pub fn hunt(
        &mut self,
        atlas: &Atlas,
        bots: &mut [Bot],
        ids: &[Identity],
        sides: impl Fn(&Identity) -> Option<i32>,
        armed: impl Fn(&Identity) -> bool,
        players: &[(i32, i32)],
        clock: u32,
        rng: &mut Rng,
    ) -> Vec<(u32, u32)> {
        self.hunting.retain(|i, (p, until)| *until > clock && bots[*i as usize].online && players.iter().any(|(c, _)| c == p));
        let mut sent = Vec::new();
        for &(player, system) in players {
            if atlas.systems.get(&system).is_none_or(|s| s.security >= 0.45) {
                continue;
            }
            let hostile: HashSet<i32> = self.map.iter().filter(|((_, p), r)| *p == player && r.flags & HOSTILE != 0).map(|((s, _), _)| *s).collect();
            let grudged: HashSet<u32> = self.grudges.iter().filter(|(_, (p, _))| *p == player).map(|(i, _)| *i).collect();
            if hostile.is_empty() && grudged.is_empty() {
                continue;
            }
            let mut have = self.hunting.values().filter(|(p, _)| *p == player).count();
            if have >= HUNTERS {
                continue;
            }
            let near: HashMap<i32, i64> = crate::gates::reach_on(atlas.graph, system as i64, HUNT_JUMPS).into_iter().map(|(s, n)| (s as i32, n)).collect();
            let mut candidates: Vec<(i64, u32)> = (0..bots.len() as u32)
                .filter(|i| !self.hunting.contains_key(i))
                .filter_map(|i| {
                    let (b, id) = (&bots[i as usize], &ids[i as usize]);
                    let fighter = matches!(b.role, Role::Pirate | Role::Ratter | Role::Explorer);
                    let free = b.online && b.pod.is_none() && !matches!(b.activity, Activity::Sieging | Activity::Escorting);
                    let minded = grudged.contains(&i) || sides(id).is_some_and(|s| hostile.contains(&s));
                    (fighter && free && minded && armed(id)).then(|| near.get(&b.system).map(|d| (*d, i))).flatten()
                })
                .collect();
            candidates.sort_unstable();
            for (_, i) in candidates.into_iter().take(HUNTERS - have) {
                let b = &mut bots[i as usize];
                let wait = if b.system == system && b.station == 0 {
                    b.activity = Activity::Camping;
                    30
                } else {
                    behaviour::go(b, atlas, system, 0, rng)
                };
                self.hunting.insert(i, (player, clock + HUNT_SECS));
                sent.push((i, wait));
                have += 1;
            }
        }
        if !sent.is_empty() {
            self.changed = true;
        }
        sent
    }

    /// The stances space reads, brought up to date if anything changed.
    pub fn publish(&mut self, bots: &[Bot]) {
        if !self.changed {
            return;
        }
        self.changed = false;
        let mut s = Stances::default();
        for ((side, player), r) in &self.map {
            if r.flags & HOSTILE != 0 {
                s.hostile.insert((*side as i64, *player as i64));
            } else if r.flags & (BLUE | CONTRACT) != 0 {
                s.friendly.insert((*side as i64, *player as i64));
            }
        }
        for (i, (player, _)) in self.grudges.iter().chain(self.hunting.iter()) {
            s.grudge.insert(bots[*i as usize].id as i64, *player as i64);
        }
        if let Ok(mut g) = STANCES.write() {
            *g = Some(s);
        }
    }

    /// Players the bots have feelings about, for the dashboard: (side,
    /// player, heat, favor, flags), warmest first.
    pub fn standings(&self) -> Vec<(i32, i32, f32, f32, u32)> {
        let mut v: Vec<(i32, i32, f32, f32, u32)> = self.map.iter().map(|((s, p), r)| (*s, *p, r.heat, r.favor, r.flags)).collect();
        v.sort_by(|a, b| (b.2 + b.3).total_cmp(&(a.2 + a.3)));
        v.truncate(100);
        v
    }
}

// ---- the database ------------------------------------------------------------------------------

pub fn load(db: &Db) -> Relations {
    let mut r = Relations::new();
    for row in db.table("botRelations").rows() {
        let rel = Rel { heat: row.float("heat").unwrap_or(0.0) as f32, favor: row.float("favor").unwrap_or(0.0) as f32, flags: row.int("flags").unwrap_or(0) as u32 };
        r.map.insert((row.int("sideID").unwrap_or(0) as i32, row.int("charID").unwrap_or(0) as i32), rel);
    }
    r
}

pub async fn flush(r: &mut Relations, store: &Store) -> Result<(), StoreError> {
    if r.dirty.is_empty() {
        return Ok(());
    }
    let keys: Vec<(i32, i32)> = r.dirty.drain().collect();
    let (kept, gone): (Vec<(i32, i32)>, Vec<(i32, i32)>) = keys.into_iter().partition(|k| r.map.contains_key(k));
    let now = crate::bulkdata::filetime_now();
    let key = |k: &(i32, i32)| [("sideID", Field::Int(i64::from(k.0))), ("charID", Field::Int(i64::from(k.1)))];
    if !kept.is_empty() {
        store.write(|tx| -> Result<(), StoreError> {
            for k in &kept {
                let rel = &r.map[k];
                let values = [
                    ("heat", Field::Float(f64::from(rel.heat))),
                    ("favor", Field::Float(f64::from(rel.favor))),
                    ("flags", Field::Int(i64::from(rel.flags as i32))),
                    ("updatedAt", Field::Int(now)),
                ];
                let row: Vec<(&str, Field)> = key(k).into_iter().chain(values.iter().cloned()).collect();
                tx.upsert("botRelations", &["sideID", "charID"], &row, &values)?;
            }
            Ok(())
        })?;
    }
    if !gone.is_empty() {
        store.write(|tx| -> Result<(), StoreError> {
            for k in &gone {
                tx.delete_where("botRelations", &["sideID", "charID"], &key(k).map(|(_, v)| v))?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// A side's executor corporation (the side itself, for a corporation) and
/// that corporation's CEO.
fn leader(db: &Db, side: i32) -> Option<(i32, i32)> {
    let corp = db.table("alliances").get_id(i64::from(side)).and_then(|a| a.int("executorCorpID")).unwrap_or(i64::from(side));
    let r = db.table("corporation").get_id(corp)?;
    Some((r.int("corporationID")? as i32, r.int("ceoID")? as i32))
}

fn owner_name(db: &Db, id: i32) -> String {
    let id64 = i64::from(id);
    [("alliances", "allianceName"), ("corporation", "corporationName"), ("character_", "characterName"), ("mapSolarSystems", "solarSystemName")]
        .iter()
        .find_map(|(table, column)| db.table(table).get_id(id64).and_then(|r| r.text(column).map(String::from)))
        .unwrap_or_else(|| id.to_string())
}

fn isk(amount: f64) -> String {
    let whole = amount.round() as i64;
    let digits = whole.to_string();
    let mut out = String::new();
    for (n, c) in digits.chars().enumerate() {
        if n > 0 && (digits.len() - n) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} ISK")
}

/// Carries out what the relations called for: standings, bounties, wars,
/// mercenary pay and mail.
pub async fn act(r: &mut Relations, store: &Store, world: &crate::world::World) {
    if r.deeds.is_empty() {
        return;
    }
    let deeds = std::mem::take(&mut r.deeds);
    let mut standings_changed = false;
    for deed in deeds {
        let done = match deed {
            Deed::Standing { side, player, value } => {
                standings_changed = true;
                set_standing(store, side, player, value).await
            }
            Deed::Bounty { side, player, amount } => bounty(store, side, player, amount).await,
            Deed::War { side, player } => match declare(store, side, player).await {
                Ok(Some((against, ceo))) => {
                    let (who, them) = store.read(|db| (owner_name(db, side), owner_name(db, against)));
                    r.news.push((crate::bulkdata::filetime_now(), format!("{who} declared war on {them}")));
                    mail(store, world, Sender::Leader(side), ceo, Letter::War { side }).await;
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            },
            Deed::Pay { side, player, amount, .. } => {
                pay(store, world, side, player, amount).await;
                Ok(())
            }
            Deed::Mail { from, player, kind } => {
                mail(store, world, from, player, kind).await;
                Ok(())
            }
        };
        if let Err(e) = done {
            error!("bots: relations: {}", crate::logging::chain(&e));
        }
    }
    if standings_changed {
        store.read(crate::guard::refresh_standings);
    }
}

async fn set_standing(store: &Store, side: i32, player: i32, value: f64) -> Result<(), StoreError> {
    let key = [Field::Int(i64::from(side)), Field::Int(i64::from(player))];
    store.write(|tx| -> Result<(), StoreError> {
        if value == 0.0 {
            tx.delete_where("ownerStandings", &["fromID", "toID"], &key)?;
        } else {
            let reason = if value < 0.0 { "Killed our pilots" } else { "Fought our enemies" };
            let set = [("standing", Field::Float(value)), ("reason", Field::Text(reason.into())), ("setAt", Field::Int(crate::bulkdata::filetime_now()))];
            let row: Vec<(&str, Field)> = [("fromID", key[0].clone()), ("toID", key[1].clone())].into_iter().chain(set.iter().cloned()).collect();
            tx.upsert("ownerStandings", &["fromID", "toID"], &row, &set)?;
        }
        Ok(())
    })?;
    info!("bots: {side}'s standing to {player} is now {value}");
    Ok(())
}

/// A bounty from the side's leader, paid out of its executor's wallet (as
/// much as it has, up to `amount`).
async fn bounty(store: &Store, side: i32, player: i32, amount: f64) -> Result<(), StoreError> {
    if !amount.is_finite() || amount<100_000.0 {return Ok(())}
    let Some((exec, ceo)) = store.read(|db| leader(db, side)) else { return Ok(()) };
    let paid = store.write(|tx| -> Result<f64, StoreError> {
        let Some(target) = tx.table("character_").get_id(i64::from(player)).map(|c| (c.id, c.float("bounty").unwrap_or(0.0))) else { return Ok(0.0) };
        let Some((wallet, balance)) = tx.table("corporation").get_id(i64::from(exec)).map(|c| (c.id, c.float("balance").unwrap_or(0.0))) else { return Ok(0.0) };
        if balance < 100_000.0 {
            return Ok(0.0);
        }
        let paid = balance.min(amount);
        tx.update("corporation", wallet, &[("balance", Field::Float(balance - paid))])?;
        let now = crate::bulkdata::filetime_now();
        let on = [Field::Int(i64::from(player)), Field::Int(i64::from(ceo))];
        let placed = tx.table("bounties").get(&on).map(|b| b.float("bounty").unwrap_or(0.0));
        let row = [("characterID", on[0].clone()), ("ownerID", on[1].clone()), ("bounty", Field::Float(paid)), ("placedAt", Field::Int(now))];
        let update = [("bounty", Field::Float(placed.unwrap_or(0.0) + paid)), ("placedAt", Field::Int(now))];
        tx.upsert("bounties", &["characterID", "ownerID"], &row, &update)?;
        tx.update("character_", target.0, &[("bounty", Field::Float(target.1 + paid))])?;
        Ok(paid)
    })?;
    if paid < 100_000.0 {
        return Ok(());
    }
    info!("bots: {side} put {paid} ISK on {player}'s head");
    Ok(())
}

/// The NPC corporations (crpNPCCorporations), which are no player's.
fn npc_corporations() -> HashSet<i64> {
    crate::sde::table("crpNPCCorporations").rows().filter_map(|n| n.int("corporationID")).collect()
}

/// War on the player's corporation (or its alliance), if it is a player
/// one and the side is not at war with it already. Returns who it is
/// against and their leader's CEO.
async fn declare(store: &Store, side: i32, player: i32) -> Result<Option<(i32, i32)>, StoreError> {
    let npc = npc_corporations();
    let declared = store.write(|tx| -> Result<Option<(i32, i32)>, StoreError> {
        let db = tx.db();
        let corp = db
            .table("character_")
            .get_id(i64::from(player))
            .and_then(|c| db.table("corporation").get_id(c.int("corporationID")?))
            .filter(|co| co.int("corporationID").is_some_and(|id| !npc.contains(&id) && db.table("botGroups").get_id(id).is_none()));
        let Some(co) = corp else { return Ok(None) };
        let corp_id = co.int("corporationID").unwrap_or(0);
        let against = co.int("allianceID").filter(|a| *a != 0).unwrap_or(corp_id) as i32;
        let ceo = co.int("ceoID").unwrap_or(0) as i32;
        let wars = db.table("crpWars");
        let between = |x: i32, y: i32| {
            wars.find(&["declaredByID"], &[Field::Int(i64::from(x))]).any(|w| w.int("retracted").is_none() && w.int("againstID") == Some(i64::from(y)))
        };
        if between(side, against) || between(against, side) {
            return Ok(None);
        }
        let Some((exec, _)) = leader(db, side) else { return Ok(None) };
        let Some((wallet, balance)) = db.table("corporation").get_id(i64::from(exec)).map(|c| (c.id, c.float("balance").unwrap_or(0.0))) else { return Ok(None) };
        if balance < WAR_COST {
            return Ok(None);
        }
        tx.update("corporation", wallet, &[("balance", Field::Float(balance - WAR_COST))])?;
        let now = crate::bulkdata::filetime_now();
        let id = tx.nextval("dyn_id_seq")?;
        tx.insert(
            "crpWars",
            &[
                ("warID", Field::Int(id)),
                ("declaredByID", Field::Int(i64::from(side))),
                ("againstID", Field::Int(i64::from(against))),
                ("timeDeclared", Field::Int(now)),
                ("timeFinished", Field::Int(now + WAR_DAYS * 86_400 * SECOND_FT)),
                ("mutual", Field::Int(0)),
            ],
        )?;
        Ok(Some((against, ceo)))
    })?;
    let Some((against, ceo)) = declared else { return Ok(None) };
    store.read(crate::svc::war::refresh);
    info!("bots: {side} declared war on {against} over {player}");
    Ok(Some((against, ceo)))
}

/// Mercenary pay from the side's executor to the player, if it can afford it.
async fn pay(store: &Store, world: &crate::world::World, side: i32, player: i32, amount: f64) {
    use crate::svc::wallet::{move_cash, Payment, Transfer};
    let Some((exec, _)) = store.read(|db| leader(db, side)) else { return };
    let payment = Payment { payer: exec as i64, payee: Some(player as i64), amount, ref_type: REF_PLAYER_DONATION, arg_id1: "", reason: "Mercenary contract" };
    match move_cash(store, payment).await {
        Ok(()) => {
            info!("bots: {side} paid {player} {amount} ISK for a kill");
            tell_balance(store, world, player);
        }
        Err(Transfer::NotEnough { .. }) | Err(Transfer::NoSuchOwner) => {}
        Err(Transfer::Db(e)) => error!("bots: mercenary pay: {}", crate::logging::chain(&e)),
    }
}

/// A player's wallet changed: their client hears its new balance.
pub fn tell_balance(store: &Store, world: &crate::world::World, player: i32) {
    let balance = store.read(|db| db.table("character_").get_id(i64::from(player)).and_then(|r| r.float("balance")));
    if let Some(balance) = balance {
        let w = world.lock();
        let event = Value::Tuple(vec![Value::bytes("cash"), Value::Int(player as i64), Value::Double(balance)]);
        w.notify(&w.clients_of(player as i64), "OnAccountChange", event);
    }
}

/// An EVE mail to a player.
pub async fn mail(store: &Store, world: &crate::world::World, from: Sender, player: i32, kind: Letter) {
    let sender = match from {
        Sender::Pilot(c) => c,
        Sender::Leader(side) => match store.read(|db| leader(db, side)) {
            Some((_, ceo)) => ceo,
            None => return,
        },
    };
    let (subject, body) = store.read(|db| {
        let name = owner_name(db, player);
        match kind {
            Letter::Revenge { system } => {
                let place = owner_name(db, system);
                (
                    "You'll regret that".to_string(),
                    format!("{name},\n\nThat was my ship you blew up in {place}. I know your name now, and so do my friends. Watch the gates.\n"),
                )
            }
            Letter::Hostile { side } => {
                let who = owner_name(db, side);
                ("Hostile".to_string(), format!("{name},\n\nYou have killed enough of our pilots. As of today you are hostile to {who}: our ships will engage you on sight and our towers will fire on you.\n"))
            }
            Letter::Bounty { side, amount } => {
                let who = owner_name(db, side);
                ("A price on your head".to_string(), format!("{name},\n\n{who} has placed a bounty of {} on you. Every pilot in New Eden can collect it.\n", isk(amount)))
            }
            Letter::War { side } => {
                let who = owner_name(db, side);
                (
                    format!("War declared by {who}"),
                    format!("{name},\n\nOne of your members has been killing our pilots, and you have done nothing about it. {who} has declared war on you. It is in force from tomorrow and lasts a week.\n"),
                )
            }
            Letter::Contract { side, enemy } => {
                let (who, them) = (owner_name(db, side), owner_name(db, enemy));
                (
                    "A contract".to_string(),
                    format!(
                        "{name},\n\nWe have noticed what you have been doing to {them}. {who} will pay you for every ship of theirs you destroy from now on: {} for a frigate, more for bigger hulls, paid straight to your wallet. Our standing towards you is now good.\n",
                        isk(MERC_PAY)
                    ),
                )
            }
            Letter::Invite { side } => {
                let who = owner_name(db, side);
                ("An invitation".to_string(), format!("{name},\n\nYou have done more for {who} than most of our members. If you want to fly with us, apply to any of our corporations; you will be accepted.\n"))
            }
        }
    });
    let body = body.replace('\n', "<br>");
    if let Err(e) = store.write(|tx| crate::svc::chat::deliver(tx, world, sender as i64, &[player as i64], &subject, &body)) {
        error!("bots: mailing {player}: {}", crate::logging::chain(&e));
    }
}

// ---- corporations ------------------------------------------------------------------------------

/// Applications to bot corporations: accepted unless the applicant is
/// hostile to the corporation's side, rejected if so.
pub async fn answer_applications(r: &Relations, store: &Store, world: &crate::world::World, p: &Politics) -> Result<(), StoreError> {
    // (corporation, character, its text, when)
    let applied: Vec<(i32, i32, String, i64)> = store.read(|db| {
        let groups = db.table("botGroups");
        db.table("crpApplications")
            .rows()
            .filter(|a| a.int("status") == Some(i64::from(APPLIED_BY_CHARACTER)) && a.int("deleted") == Some(0))
            .filter(|a| a.int("corporationID").is_some_and(|c| groups.get_id(c).is_some()))
            .map(|a| {
                let text = a.text("applicationText").unwrap_or_default().to_string();
                (a.int("corporationID").unwrap_or(0) as i32, a.int("characterID").unwrap_or(0) as i32, text, a.int("applicationDateTime").unwrap_or(0))
            })
            .collect()
    });
    for (corp, char_id, text, when) in applied {
        let side = p.side_of(corp).unwrap_or(corp);
        let hostile = r.get(side, char_id).flags & HOSTILE != 0;
        let (status, note) = if hostile { (REJECTED_BY_CORPORATION, "We do not take pilots who kill ours.") } else { (ACCEPTED_BY_CORPORATION, "Welcome aboard. Accept to join us.") };
        store.write(|tx| set_application(tx, corp, char_id, &[("status", Field::Int(i64::from(status))), ("corporationText", Field::Text(note.into()))]))?;
        let line = |s: i16| vec![Value::Int(corp as i64), Value::Int(char_id as i64), Value::text(text.as_str()), Value::Long(when), Value::Int(s as i64)];
        let change = crate::svc::rows::change(APPLICATION_HEADER, Some(&line(APPLIED_BY_CHARACTER)), Some(&line(status)));
        let w = world.lock();
        w.notify(&w.clients_of(char_id as i64), "OnCorporationApplicationChanged", Value::Tuple(vec![Value::Int(char_id as i64), Value::Int(corp as i64), change]));
        info!("bots: corporation {corp} {} {char_id}'s application", if hostile { "turned down" } else { "accepted" });
    }
    Ok(())
}

/// Sets columns of a character's application to a corporation, if there is one.
fn set_application(tx: &mut Tx<'_>, corp: i32, char_id: i32, sets: Sets) -> Result<(), StoreError> {
    let Some(row) = tx.table("crpApplications").get(&[Field::Int(i64::from(corp)), Field::Int(i64::from(char_id))]).map(|a| a.id) else { return Ok(()) };
    tx.update("crpApplications", row, sets)
}

const PITCHES: [&str; 6] = [
    "Experienced pilot looking for a corporation that actually flies together.",
    "I mine, I haul, I pay my taxes. Looking for a home.",
    "Combat pilot, a few years in. Want to be useful to someone.",
    "Saw your corporation around. Would like to join if you'll have me.",
    "Solo too long. Looking for people to fly with.",
    "Industrialist with my own ships. Happy to work near your headquarters.",
];

/// Why a bot's move into a corporation did not happen.
enum Join {
    /// The corporation would not have it: nothing changes.
    Refused,
    Failed(StoreError),
}

impl From<StoreError> for Join {
    fn from(e: StoreError) -> Join {
        Join::Failed(e)
    }
}

/// Bots in NPC corporations apply to player corporations now and then;
/// those accepted join (changing corporation as a player would), move near
/// its headquarters, and work for it. Bots a corporation let go are noticed.
/// Returns the bots whose corporation changed.
#[allow(clippy::too_many_arguments)]
pub async fn recruit(
    store: &Store,
    world: &crate::world::World,
    atlas: &Atlas,
    p: &Politics,
    bots: &mut [Bot],
    ids: &mut [Identity],
    by_char: &HashMap<i64, u32>,
    rng: &mut Rng,
) -> Result<Vec<u32>, StoreError> {
    let mut moved = Vec::new();
    // Applications accepted by the corporation: the bot accepts too.
    // (corporation, character, its text, when, headquarters, alliance)
    let accepted: Vec<(i32, i32, String, i64, i64, i64)> = store.read(|db| {
        let (bot_rows, corps) = (db.table("bots"), db.table("corporation"));
        db.table("crpApplications")
            .rows()
            .filter(|a| a.int("status") == Some(i64::from(ACCEPTED_BY_CORPORATION)) && a.int("deleted") == Some(0))
            .filter(|a| a.int("characterID").is_some_and(|c| bot_rows.get_id(c).is_some()))
            .filter_map(|a| {
                let co = corps.get_id(a.int("corporationID")?)?;
                let text = a.text("applicationText").unwrap_or_default().to_string();
                Some((
                    a.int("corporationID").unwrap_or(0) as i32,
                    a.int("characterID").unwrap_or(0) as i32,
                    text,
                    a.int("applicationDateTime").unwrap_or(0),
                    co.int("stationID").unwrap_or(0),
                    co.int("allianceID").unwrap_or(0),
                ))
            })
            .collect()
    });
    for (corp, char_id, text, when, hq, alliance) in accepted {
        let Some(&i) = by_char.get(&(char_id as i64)) else { continue };
        let now = crate::bulkdata::filetime_now();
        let joined = store.write(|tx| -> Result<i64, Join> {
            let left = crate::svc::membership::change_corporation(tx, char_id as i64, corp as i64, 0, now).map_err(|_| Join::Refused)?;
            set_application(tx, corp, char_id, &[("status", Field::Int(i64::from(ACCEPTED_BY_CHARACTER)))])?;
            Ok(left)
        });
        let left = match joined {
            Ok(left) => left,
            Err(Join::Refused) => continue,
            Err(Join::Failed(e)) => return Err(e),
        };
        crate::svc::membership::member_moved(world, char_id as i64, left, corp as i64);
        let line = |s: i16| vec![Value::Int(corp as i64), Value::Int(char_id as i64), Value::text(text.as_str()), Value::Long(when), Value::Int(s as i64)];
        let change = crate::svc::rows::change(APPLICATION_HEADER, Some(&line(ACCEPTED_BY_CORPORATION)), Some(&line(ACCEPTED_BY_CHARACTER)));
        {
            let w = world.lock();
            w.notify(&w.corp_members(corp as i64), "OnCorporationApplicationChanged", Value::Tuple(vec![Value::Int(char_id as i64), Value::Int(corp as i64), change]));
        }
        ids[i as usize].corp = corp;
        ids[i as usize].alliance = Some(alliance as i32).filter(|a| *a != 0);
        // Home near the headquarters, if it can get there.
        let hq = hq as i32;
        let b = &mut bots[i as usize];
        if atlas.station_system.contains_key(&hq) && atlas.route(atlas.system_of(b.home), atlas.system_of(hq)).is_some() {
            b.home = hq;
            b.turf = 0;
        }
        moved.push(i);
        info!("bots: {} joined player corporation {corp}", ids[i as usize].name);
    }
    // Bots their corporation let go (or that are somewhere else now).
    let watched: Vec<i32> = ids.iter().zip(bots.iter()).filter(|(id, _)| !p.corps.contains_key(&id.corp) && id.corp >= 2_000_000).map(|(_, b)| b.id).collect();
    if !watched.is_empty() {
        // (character, corporation, alliance)
        let now_in: Vec<(i64, i32, i32)> = store.read(|db| {
            let (characters, corps) = (db.table("character_"), db.table("corporation"));
            watched
                .iter()
                .filter_map(|c| {
                    let corp = characters.get_id(i64::from(*c))?.int("corporationID")?;
                    let co = corps.get_id(corp)?;
                    Some((i64::from(*c), corp as i32, co.int("allianceID").unwrap_or(0) as i32))
                })
                .collect()
        });
        for (char_id, corp, alliance) in now_in {
            let Some(&i) = by_char.get(&char_id) else { continue };
            let alliance = Some(alliance).filter(|a| *a != 0);
            if ids[i as usize].corp != corp {
                ids[i as usize].corp = corp;
                ids[i as usize].alliance = alliance;
                moved.push(i);
            } else {
                ids[i as usize].alliance = alliance;
            }
        }
    }
    // New applications.
    let npc = npc_corporations();
    // (corporation, its headquarters' region, applications from bots waiting)
    let corps: Vec<(i32, i32, i64)> = store.read(|db| {
        let (stations, systems, groups, bot_rows) = (db.table("staStations"), db.table("mapSolarSystems"), db.table("botGroups"), db.table("bots"));
        let applications = db.table("crpApplications");
        db.table("corporation")
            .rows()
            .filter(|co| co.int("deleted") == Some(0) && co.int("memberCount").is_some_and(|n| n > 0))
            .filter_map(|co| {
                let id = co.int("corporationID")?;
                if npc.contains(&id) || groups.get_id(id).is_some() {
                    return None;
                }
                let system = stations.get_id(co.int("stationID")?)?.int("solarSystemID")?;
                let region = systems.get_id(system)?.int("regionID").unwrap_or(0);
                let pending = applications
                    .find(&["corporationID"], &[Field::Int(id)])
                    .filter(|a| matches!(a.int("status"), Some(0 | 1 | 5 | 6)) && a.int("deleted") == Some(0))
                    .filter(|a| a.int("characterID").is_some_and(|c| bot_rows.get_id(c).is_some()))
                    .count() as i64;
                Some((id as i32, region as i32, pending))
            })
            .collect()
    });
    for (corp, region, pending) in corps {
        if pending >= PENDING_MAX || rng.unit() >= APPLY_CHANCE {
            continue;
        }
        // A bot of an NPC corporation, living in the region if one does.
        let free: Vec<u32> = (0..bots.len() as u32).filter(|i| ids[*i as usize].corp < 2_000_000 && !p.corps.contains_key(&ids[*i as usize].corp)).collect();
        if free.is_empty() {
            break;
        }
        let local: Vec<u32> = free.iter().copied().filter(|i| atlas.region_of(atlas.system_of(bots[*i as usize].home)) == region).take(500).collect();
        let pool = if local.is_empty() { &free } else { &local };
        let i = pool[rng.below(pool.len())];
        let pitch = PITCHES[rng.below(PITCHES.len())];
        let now = crate::bulkdata::filetime_now();
        store.write(|tx| {
            let fresh = [("applicationText", Field::Text(pitch.into())), ("applicationDateTime", Field::Int(now)), ("status", Field::Int(i64::from(APPLIED_BY_CHARACTER)))];
            let row: Vec<(&str, Field)> = [("corporationID", Field::Int(i64::from(corp))), ("characterID", Field::Int(i64::from(bots[i as usize].id)))]
                .into_iter()
                .chain(fresh.iter().cloned())
                .chain([("deleted", Field::Int(0))])
                .collect();
            let update: Vec<(&str, Field)> = fresh.iter().cloned().chain([("corporationText", Field::Text("".into())), ("deleted", Field::Int(0))]).collect();
            tx.upsert("crpApplications", &["corporationID", "characterID"], &row, &update)
        })?;
        let line = vec![Value::Int(corp as i64), Value::Int(bots[i as usize].id as i64), Value::text(pitch), Value::Long(now), Value::Int(APPLIED_BY_CHARACTER as i64)];
        let change = crate::svc::rows::change(APPLICATION_HEADER, None, Some(&line));
        let w = world.lock();
        w.notify(&w.corp_members(corp as i64), "OnCorporationApplicationChanged", Value::Tuple(vec![Value::Int(bots[i as usize].id as i64), Value::Int(corp as i64), change]));
        info!("bots: {} applied to join corporation {corp}", ids[i as usize].name);
    }
    Ok(moved)
}
