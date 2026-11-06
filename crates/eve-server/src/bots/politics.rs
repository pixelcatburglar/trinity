//! Bot politics: the corporations and alliances bots belong to, the space
//! the big alliances call home, and the wars between them.
//!
//! It is formed once, the first time the bots run (`form`):
//! - about seven bots in ten join corporations of their own, a few big and
//!   many small, industrial or combat by who is in them;
//! - many of the bigger ones join alliances: null-sec alliances, each with a
//!   home region whose stations their members move to and whose belts they
//!   mine and rat in; pirate alliances, living in a low-sec region; and
//!   industrial ones, which stay where they are;
//! - neighbouring null-sec alliances, and pirates and their prey, go to war.
//!
//! After that it moves along (`tick`): wars run their course and new ones
//! start, and bots still in NPC corporations join bot ones. Where enemies
//! meet in space nobody is watching, they fight (`skirmish`), and the losers
//! wake at home.
//!
//! Everything is the real thing: corporations, alliances, members and wars
//! are rows the client reads as it would a player's. Bot wars are mutual, so
//! nobody is billed for them, and bots wage them only on each other.

use std::collections::{HashMap, HashSet, VecDeque};

use tracing::info;

use super::behaviour::{self, Atlas, Bot, Role};
use super::presence::Identity;
use crate::combat::Rng;
use crate::store::{RowId, Store, StoreError, Tx, Value};

const KIND_INDUSTRIAL_CORP: i16 = 1;
const KIND_COMBAT_CORP: i16 = 2;
pub(super) const KIND_NULL_ALLIANCE: i16 = 11;
pub(super) const KIND_PIRATE_ALLIANCE: i16 = 12;
const KIND_INDUSTRY_ALLIANCE: i16 = 13;

/// The share of bots that join bot corporations at the start.
const IN_CORPS: f64 = 0.7;
/// Roughly how many corporations make one alliance.
const CORPS_PER_ALLIANCE: usize = 30;
const NULL_ALLIANCES: usize = 25;
const PIRATE_ALLIANCES: usize = 15;
const HIGH_SEC: f32 = 0.45;

const MINUTE: u32 = 60;
const DAY: u32 = 86_400;
/// How long a war lasts, in days of the bots' clock.
const WAR_DAYS: (u32, u32) = (3, 14);
/// The share of bots still in NPC corporations that join a bot one each day.
const RECRUIT_PER_DAY: f64 = 0.01;
const TICKS_PER_DAY: f64 = (DAY / (10 * MINUTE)) as f64;
const SECOND_FT: i64 = 10_000_000;
const NEWS_KEPT: usize = 60;

pub struct Corp {
    pub kind: i16,
    pub alliance: Option<i32>,
    pub members: u32,
}

pub struct Alliance {
    pub kind: i16,
    pub region: Option<i32>,
}

pub struct War {
    pub id: i32,
    /// The two sides: alliances, or corporations outside one.
    pub a: i32,
    pub b: i32,
    /// When it ends, on the bots' clock.
    pub ends: u32,
    pub kills: u32,
}

pub struct Politics {
    pub corps: HashMap<i32, Corp>,
    pub alliances: HashMap<i32, Alliance>,
    pub wars: Vec<War>,
    /// Bots lost in fights since the last report, and since the start.
    pub kills: u64,
    pub lost: u64,
    /// The latest of what happened in null-sec, oldest first, for the dashboard.
    pub news: VecDeque<super::sov::News>,
    neighbours: HashMap<i32, HashSet<i32>>,
    /// Each alliance's executor corporation's wallet, as last read.
    pub treasury: HashMap<i32, f64>,
    /// Each alliance's prime time (UTC hour), and the hour it is now.
    pub primes: HashMap<i32, u8>,
    pub hour: f64,
    /// Fights nobody saw since the last save: (lost, its killer, system,
    /// the hull lost), for the kill reports.
    pub unseen_kills: Vec<(u32, u32, i32, i32)>,
    /// How fast the bots' clock runs.
    pub speed: f64,
}

impl Politics {
    pub fn tell(&mut self, news: Vec<super::sov::News>) {
        self.news.extend(news);
        while self.news.len() > NEWS_KEPT {
            self.news.pop_front();
        }
    }

    fn side(&self, corp: i32) -> Option<i32> {
        let c = self.corps.get(&corp)?;
        Some(c.alliance.unwrap_or(corp))
    }

    /// A bot corporation's side: its alliance, or itself outside one.
    pub fn side_of(&self, corp: i32) -> Option<i32> {
        self.side(corp)
    }

    /// An alliance's prime time: one of the bots' four, by its ID.
    pub fn prime_of(&self, alliance: i32) -> u8 {
        self.primes.get(&alliance).copied().unwrap_or_else(|| behaviour::PRIME_TIMES[(alliance.unsigned_abs() as usize / 7) % 4])
    }

    /// Whether it is an alliance's prime time, give or take three hours.
    pub fn in_prime(&self, alliance: i32) -> bool {
        let d = (self.hour - self.prime_of(alliance) as f64).rem_euclid(24.0);
        d.min(24.0 - d) <= 3.0
    }

    pub fn new(atlas: &Atlas) -> Politics {
        Politics {
            corps: HashMap::new(),
            alliances: HashMap::new(),
            wars: Vec::new(),
            kills: 0,
            lost: 0,
            news: VecDeque::new(),
            neighbours: region_neighbours(atlas),
            treasury: HashMap::new(),
            primes: HashMap::new(),
            hour: 0.0,
            unseen_kills: Vec::new(),
            speed: 1.0,
        }
    }

    /// The regions next to one, across a gate.
    pub fn neighbours_of(&self, region: i32) -> HashSet<i32> {
        self.neighbours.get(&region).cloned().unwrap_or_default()
    }

    pub fn summary(&self) -> String {
        let null = self.alliances.values().filter(|a| a.kind == KIND_NULL_ALLIANCE).count();
        format!(
            "{} bot corporations, {} alliances ({null} holding null-sec), {} wars, {} lost in fights",
            self.corps.len(),
            self.alliances.len(),
            self.wars.len(),
            self.kills
        )
    }
}

/// Regions next to each other across a gate.
fn region_neighbours(atlas: &Atlas) -> HashMap<i32, HashSet<i32>> {
    let mut out: HashMap<i32, HashSet<i32>> = HashMap::new();
    for s in atlas.systems.values() {
        for n in &s.neighbours {
            if let Some(other) = atlas.systems.get(n).filter(|o| o.region != s.region) {
                out.entry(s.region).or_default().insert(other.region);
            }
        }
    }
    out
}

fn filetime_now() -> i64 {
    crate::bulkdata::filetime_now()
}

/// What the database holds of bot politics, if it has been formed.
pub async fn load(store: &Store, atlas: &Atlas, clock: u32, speed: f64) -> Result<Politics, StoreError> {
    let mut p = Politics::new(atlas);
    p.speed = speed;
    let now = filetime_now();
    store.read(|db| {
        let corps = db.table("corporation");
        for g in db.table("botGroups").rows() {
            let (id, kind) = (g.int("ownerID").unwrap_or(0) as i32, g.int("kind").unwrap_or(0) as i16);
            if kind >= KIND_NULL_ALLIANCE {
                p.alliances.insert(id, Alliance { kind, region: g.int("regionID").map(|v| v as i32) });
            } else {
                let co = corps.get_id(i64::from(id));
                let alliance = co.and_then(|c| c.int("allianceID")).map(|v| v as i32).filter(|v| *v != 0);
                p.corps.insert(id, Corp { kind, alliance, members: co.and_then(|c| c.int("memberCount")).unwrap_or(0) as u32 });
            }
        }
        let groups = db.table("botGroups");
        for w in db.table("crpWars").rows() {
            if w.int("retracted").is_some() || !w.int("declaredByID").is_some_and(|d| groups.get_id(d).is_some()) {
                continue;
            }
            let left = ((w.int("timeFinished").unwrap_or(0) - now) as f64 / SECOND_FT as f64 * speed).max(0.0) as u32;
            let (id, a, b) = (w.int("warID").unwrap_or(0) as i32, w.int("declaredByID").unwrap_or(0) as i32, w.int("againstID").unwrap_or(0) as i32);
            p.wars.push(War { id, a, b, ends: clock + left, kills: 0 });
        }
    });
    // Wars made before bot wars were dated back (see `start_wars`).
    let since = now - crate::svc::war::DAY;
    let redated = store.write(|tx| {
        let rows: Vec<RowId> = {
            let groups = tx.table("botGroups");
            tx.table("crpWars")
                .rows()
                .filter(|w| w.int("timeDeclared").is_some_and(|t| t > since) && w.int("retracted").is_none() && w.int("mutual") == Some(1))
                .filter(|w| w.int("declaredByID").is_some_and(|d| groups.get_id(d).is_some()))
                .map(|w| w.id)
                .collect()
        };
        for row in &rows {
            tx.update("crpWars", *row, &[("timeDeclared", Value::Int(since))])?;
        }
        Ok::<_, StoreError>(rows.len())
    })?;
    if redated > 0 {
        store.read(crate::svc::war::refresh);
    }
    paint_logos(store).await?;
    Ok(p)
}

/// Gives bot corporations without one a logo: two or three shapes from the
/// corporation paint shop's (graphics 415 on, 163 of them) in its colours
/// (671 on, 15), picked from the corporation's ID so they stay the same.
async fn paint_logos(store: &Store) -> Result<(), StoreError> {
    let painted = store.write(|tx| {
        let rows: Vec<(RowId, i64)> = {
            let corps = tx.table("corporation");
            tx.table("botGroups")
                .rows()
                .filter_map(|g| corps.get_id(g.int("ownerID")?))
                .filter(|co| co.int("shape1").is_none())
                .map(|co| (co.id, co.int("corporationID").unwrap_or(0)))
                .collect()
        };
        for (row, id) in &rows {
            let id = *id;
            let shape3 = if id % 3 == 0 { Value::Null } else { Value::Int(415 + (id / 26569) % 163) };
            tx.update(
                "corporation",
                *row,
                &[
                    ("shape1", Value::Int(415 + id % 163)),
                    ("shape2", Value::Int(415 + (id / 163) % 163)),
                    ("shape3", shape3),
                    ("color1", Value::Int(671 + id % 15)),
                    ("color2", Value::Int(671 + (id / 15) % 15)),
                    ("color3", Value::Int(671 + (id / 225) % 15)),
                ],
            )?;
        }
        Ok::<_, StoreError>(rows.len())
    })?;
    if painted > 0 {
        info!("bots: {painted} corporation logos drawn");
    }
    Ok(())
}

// ---- forming ---------------------------------------------------------------------------------

/// A size from a long tail: mostly small, now and then very big.
fn tail(rng: &mut Rng, base: f64, power: f64, max: f64) -> usize {
    (base / rng.unit().max(1e-6).powf(power)).round().min(max) as usize
}

fn shuffle<T>(v: &mut [T], rng: &mut Rng) {
    for i in (1..v.len()).rev() {
        v.swap(i, rng.below(i + 1));
    }
}

const ADJECTIVES: [&str; 48] = [
    "Crimson", "Black", "Iron", "Silent", "Burning", "Frozen", "Hollow", "Golden", "Shattered", "Eternal", "Northern",
    "Rogue", "Steel", "Void", "Red", "Blue", "Dark", "Lost", "Final", "Savage", "Hidden", "Ancient", "Broken",
    "Deep", "Wild", "Grey", "Pale", "Storm", "Solar", "Lunar", "Atomic", "Quantum", "Free", "Outer", "Distant",
    "Brave", "Grim", "Swift", "Sacred", "Fallen", "Rising", "Twin", "Ghost", "Copper", "Obsidian", "Amber", "Vicious", "Sovereign",
];
const NOUNS: [&str; 48] = [
    "Vanguard", "Legion", "Horizon", "Fleet", "Wolves", "Ravens", "Serpents", "Hammer", "Anvil", "Shield", "Spear",
    "Dragons", "Phoenix", "Reapers", "Wardens", "Nomads", "Pioneers", "Drifters", "Miners", "Haulers", "Tribe",
    "Order", "Covenant", "Syndicate", "Cartel", "Brotherhood", "Company", "Guild", "Assembly", "Collective",
    "Frontier", "Outlaws", "Marauders", "Raiders", "Corsairs", "Sentinels", "Knights", "Hunters", "Prospectors",
    "Foundry", "Works", "Logistics", "Ventures", "Dynamics", "Armada", "Vipers", "Talons", "Crows",
];
const CORP_ENDINGS: [&str; 8] = ["", "", "", " Inc", " Corp", " Industries", " Holdings", " Enterprises"];
const ALLIANCE_ENDINGS: [&str; 8] = [" Alliance", " Coalition", " Federation", " Pact", " Union", " Accord", " Dominion", " Consortium"];

/// A name and ticker not yet taken (case aside). The ticker is the name's
/// initials (at least three letters, at most `ticker_len`), with letters
/// added or varied only to make it unique.
fn christen(rng: &mut Rng, endings: &[&str], ticker_len: usize, names: &mut HashSet<String>, tickers: &mut HashSet<String>) -> (String, String) {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let (adj, noun) = (ADJECTIVES[rng.below(ADJECTIVES.len())], NOUNS[rng.below(NOUNS.len())]);
        let mut name = format!("{adj} {noun}{}", endings[rng.below(endings.len())]);
        if attempt > 20 {
            name = format!("{name} {}", rng.below(1_000));
        }
        if names.contains(&name.to_lowercase()) {
            continue;
        }
        let initials: String = name.split_whitespace().filter_map(|w| w.chars().next()).filter(|c| c.is_ascii_uppercase()).take(ticker_len).collect();
        for t in 0..200 {
            let mut ticker = if t < 100 { initials.clone() } else { String::new() };
            let want = if t == 0 { 3 } else { (3 + t / 10).min(ticker_len) };
            while ticker.len() < want || (t > 0 && ticker == initials) {
                if ticker.len() >= ticker_len {
                    ticker.pop();
                }
                ticker.push((b'A' + rng.below(26) as u8) as char);
            }
            if tickers.insert(ticker.to_lowercase()) {
                names.insert(name.to_lowercase());
                return (name, ticker);
            }
        }
    }
}

fn combat(role: Role) -> bool {
    matches!(role, Role::Ratter | Role::Pirate | Role::Explorer)
}

/// A region's stations, or the nearest outside it if it has none.
fn stations_for(atlas: &Atlas, region: i32) -> Vec<i32> {
    let inside: Vec<i32> = atlas.systems.values().filter(|s| s.region == region).flat_map(|s| s.stations.iter().copied()).collect();
    if !inside.is_empty() {
        return inside;
    }
    let start = atlas.systems.iter().find(|(_, s)| s.region == region).map(|(id, _)| *id);
    start
        .and_then(|from| {
            crate::gates::reach_on(atlas.graph, from as i64, 10)
                .into_iter()
                .find_map(|(s, _)| atlas.systems.get(&(s as i32)).filter(|x| !x.stations.is_empty()).map(|x| x.stations.clone()))
        })
        .unwrap_or_default()
}

/// A region's systems with belts, of the given kind of space.
fn belts_in(atlas: &Atlas, region: i32, null: bool) -> Vec<i32> {
    let mut out: Vec<i32> = atlas
        .systems
        .iter()
        .filter(|(_, s)| s.region == region && s.belts > 0 && if null { s.security <= 0.0 } else { s.security < HIGH_SEC })
        .map(|(id, _)| *id)
        .collect();
    out.sort_unstable();
    out
}

/// Regions with at least `min` systems of a kind of space, most first.
fn regions_of(atlas: &Atlas, min: usize, keep: impl Fn(f32) -> bool) -> Vec<i32> {
    let mut count: HashMap<i32, usize> = HashMap::new();
    for s in atlas.systems.values().filter(|s| keep(s.security)) {
        *count.entry(s.region).or_default() += 1;
    }
    let mut out: Vec<(i32, usize)> = count.into_iter().filter(|(_, n)| *n >= min).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(r, _)| r).collect()
}

/// Forms bot politics from scratch: corporations, alliances, homes, wars.
#[allow(clippy::too_many_arguments)]
pub async fn form(store: &Store, atlas: &Atlas, bots: &mut [Bot], ids: &mut [Identity], rng: &mut Rng, clock: u32, speed: f64) -> Result<Politics, StoreError> {
    let started = std::time::Instant::now();
    let mut names: HashSet<String> = HashSet::new();
    let mut tickers: HashSet<String> = HashSet::new();
    store.read(|db| {
        for (table, name, ticker) in [("corporation", "corporationName", "tickerName"), ("alliances", "allianceName", "shortName")] {
            for r in db.table(table).rows() {
                names.extend(r.text(name).map(str::to_lowercase));
                tickers.extend(r.text(ticker).map(str::to_lowercase));
            }
        }
    });

    // Corporations, of members drawn at random.
    let mut joining: Vec<u32> = (0..bots.len() as u32).filter(|_| rng.unit() < IN_CORPS).collect();
    shuffle(&mut joining, rng);
    struct NewCorp {
        members: Vec<u32>,
        kind: i16,
        alliance: Option<usize>,
        name: String,
        ticker: String,
    }
    let mut corps: Vec<NewCorp> = Vec::new();
    let mut rest = &joining[..];
    while rest.len() >= 3 {
        let size = tail(rng, 4.0, 0.8, 2_500.0).clamp(3, rest.len());
        let (members, after) = rest.split_at(size);
        rest = after;
        let fighters = members.iter().filter(|i| combat(bots[**i as usize].role)).count();
        let kind = if fighters * 2 >= members.len() { KIND_COMBAT_CORP } else { KIND_INDUSTRIAL_CORP };
        let (name, ticker) = christen(rng, &CORP_ENDINGS, 4, &mut names, &mut tickers);
        corps.push(NewCorp { members: members.to_vec(), kind, alliance: None, name, ticker });
    }

    // Alliances: null-sec ones each with a region of its own, pirates in
    // low-sec, and the rest industrial.
    struct NewAlliance {
        kind: i16,
        region: Option<i32>,
        weight: f64,
        name: String,
        ticker: String,
    }
    let count = (corps.len() / CORPS_PER_ALLIANCE).max(3);
    let mut null_regions = regions_of(atlas, 20, |s| s <= 0.0);
    shuffle(&mut null_regions, rng);
    let mut low_regions = regions_of(atlas, 10, |s| s > 0.0 && s < HIGH_SEC);
    shuffle(&mut low_regions, rng);
    let mut alliances: Vec<NewAlliance> = Vec::new();
    for n in 0..count {
        let (kind, region) = if n < NULL_ALLIANCES.min(null_regions.len()) {
            (KIND_NULL_ALLIANCE, Some(null_regions[n]))
        } else if n < NULL_ALLIANCES.min(null_regions.len()) + PIRATE_ALLIANCES.min(low_regions.len()) {
            (KIND_PIRATE_ALLIANCE, Some(low_regions[n - NULL_ALLIANCES.min(null_regions.len())]))
        } else {
            (KIND_INDUSTRY_ALLIANCE, None)
        };
        let (name, ticker) = christen(rng, &ALLIANCE_ENDINGS, 5, &mut names, &mut tickers);
        alliances.push(NewAlliance { kind, region, weight: 1.0 / rng.unit().max(1e-6).powf(0.7), name, ticker });
    }
    let pick_alliance = |kind: i16, rng: &mut Rng| -> Option<usize> {
        let total: f64 = alliances.iter().filter(|a| a.kind == kind).map(|a| a.weight).sum();
        let mut x = rng.unit() * total;
        alliances.iter().enumerate().filter(|(_, a)| a.kind == kind).find(|(_, a)| {
            x -= a.weight;
            x <= 0.0
        }).map(|(i, _)| i)
    };
    for corp in corps.iter_mut().filter(|c| c.members.len() >= 8) {
        if rng.unit() >= 0.6 {
            continue;
        }
        let kind = match (corp.kind, rng.unit()) {
            (KIND_COMBAT_CORP, x) if x < 0.7 => KIND_NULL_ALLIANCE,
            (KIND_COMBAT_CORP, _) => KIND_PIRATE_ALLIANCE,
            (_, x) if x < 0.4 => KIND_NULL_ALLIANCE,
            _ => KIND_INDUSTRY_ALLIANCE,
        };
        corp.alliance = pick_alliance(kind, rng);
    }
    // An alliance nobody joined is not made.
    let used: HashSet<usize> = corps.iter().filter_map(|c| c.alliance).collect();

    // Homes: null-sec and pirate members move to their alliance's region.
    let mut moves: Vec<(u32, i32, i32)> = Vec::new();
    for corp in &corps {
        let Some(region) = corp.alliance.and_then(|a| alliances[a].region) else { continue };
        let null = alliances[corp.alliance.unwrap_or(0)].kind == KIND_NULL_ALLIANCE;
        let stations = stations_for(atlas, region);
        let belts = belts_in(atlas, region, null);
        if stations.is_empty() {
            continue;
        }
        for &i in &corp.members {
            let bot = &bots[i as usize];
            if bot.role == Role::Trader {
                continue;
            }
            let home = stations[rng.below(stations.len())];
            let turf = belts.get(rng.below(belts.len().max(1))).copied().unwrap_or(0);
            moves.push((i, home, turf));
        }
    }

    // Into the database, all at once.
    let now = filetime_now();
    let homes: HashMap<u32, (i32, i32)> = moves.iter().map(|(i, h, t)| (*i, (*h, *t))).collect();
    let hq = |c: &NewCorp| {
        let ceo = c.members[0];
        homes.get(&ceo).map_or(bots[ceo as usize].home, |h| h.0)
    };
    let mut used_sorted: Vec<usize> = used.iter().copied().collect();
    used_sorted.sort_unstable();
    let (corp_ids, alliance_id) = store.write(|tx| -> Result<(Vec<i32>, HashMap<usize, i32>), StoreError> {
        let mut new_ids = Vec::with_capacity(corps.len() + used.len());
        for _ in 0..corps.len() + used.len() {
            new_ids.push(tx.nextval("dyn_id_seq")? as i32);
        }
        let (corp_ids, rest_ids) = new_ids.split_at(corps.len());
        let mut alliance_id: HashMap<usize, i32> = HashMap::new();
        for (n, a) in used_sorted.iter().enumerate() {
            alliance_id.insert(*a, rest_ids[n]);
        }
        // The biggest member corporation leads each alliance.
        let mut executor: HashMap<usize, (usize, i32)> = HashMap::new();
        for (c, corp) in corps.iter().enumerate() {
            if let Some(a) = corp.alliance {
                let e = executor.entry(a).or_insert((0, corp_ids[c]));
                if corp.members.len() > e.0 {
                    *e = (corp.members.len(), corp_ids[c]);
                }
            }
        }
        for a in &used_sorted {
            let exec = executor[a].1;
            let corp = corp_ids.iter().position(|c| *c == exec).unwrap_or(0);
            let ceo = bots[corps[corp].members[0] as usize].id;
            let members = corps.iter().filter(|c| c.alliance == Some(*a)).count();
            // alliance_shortnames follows by trigger (sync_alliance_shortnames).
            tx.insert(
                "alliances",
                &[
                    ("allianceID", Value::Int(i64::from(alliance_id[a]))),
                    ("allianceName", Value::Text(alliances[*a].name.as_str().into())),
                    ("shortName", Value::Text(alliances[*a].ticker.as_str().into())),
                    ("description", Value::Text("".into())),
                    ("url", Value::Text("".into())),
                    ("executorCorpID", Value::Int(i64::from(exec))),
                    ("creatorCorpID", Value::Int(i64::from(exec))),
                    ("creatorCharID", Value::Int(i64::from(ceo))),
                    ("startDate", Value::Int(now)),
                    ("memberCount", Value::Int(members as i64)),
                ],
            )?;
        }

        let balances: Vec<f64> = corps.iter().map(|c| c.members.len() as f64 * 5.0e6 * rng.range(0.5, 2.0)).collect();
        // Member counts start at nothing: joining (`join_corps`) counts them in.
        for (c, corp) in corps.iter().enumerate() {
            let ceo = bots[corp.members[0] as usize].id;
            let Some(race) = tx.table("character_").get_id(i64::from(ceo)).map(|r| r.value("raceID").clone()) else { continue };
            tx.insert(
                "corporation",
                &[
                    ("corporationID", Value::Int(i64::from(corp_ids[c]))),
                    ("corporationName", Value::Text(corp.name.as_str().into())),
                    ("tickerName", Value::Text(corp.ticker.as_str().into())),
                    ("description", Value::Text("".into())),
                    ("url", Value::Text("".into())),
                    ("creatorID", Value::Int(i64::from(ceo))),
                    ("ceoID", Value::Int(i64::from(ceo))),
                    ("stationID", Value::Int(i64::from(hq(corp)))),
                    ("raceID", race),
                    ("allianceID", Value::Int(corp.alliance.map_or(0, |a| i64::from(alliance_id[&a])))),
                    ("memberCount", Value::Int(0)),
                    ("memberLimit", Value::Int(5000)),
                    ("balance", Value::Float(balances[c])),
                ],
            )?;
        }
        for (c, corp) in corps.iter().enumerate() {
            if let Some(a) = corp.alliance {
                tx.insert(
                    "allianceMembers",
                    &[
                        ("allianceID", Value::Int(i64::from(alliance_id[&a]))),
                        ("corporationID", Value::Int(i64::from(corp_ids[c]))),
                        ("chosenExecutorID", Value::Int(i64::from(executor[&a].1))),
                        ("startDate", Value::Int(now)),
                    ],
                )?;
            }
        }

        // The members leave their NPC corporations for the new ones.
        let (who, into): (Vec<i32>, Vec<i32>) = {
            let mut v = (Vec::new(), Vec::new());
            for (c, corp) in corps.iter().enumerate() {
                for &i in &corp.members {
                    v.0.push(bots[i as usize].id);
                    v.1.push(corp_ids[c]);
                }
            }
            v
        };
        join_corps(tx, &who, &into, now)?;

        for (c, corp) in corps.iter().enumerate() {
            tx.insert("botGroups", &[("ownerID", Value::Int(i64::from(corp_ids[c]))), ("kind", Value::Int(i64::from(corp.kind))), ("regionID", Value::Null)])?;
        }
        for a in &used_sorted {
            let region = alliances[*a].region.map_or(Value::Null, |r| Value::Int(i64::from(r)));
            tx.insert("botGroups", &[("ownerID", Value::Int(i64::from(alliance_id[a]))), ("kind", Value::Int(i64::from(alliances[*a].kind))), ("regionID", region)])?;
        }

        for (i, home, turf) in &moves {
            let id = i64::from(bots[*i as usize].id);
            if let Some(row) = tx.table("bots").get_id(id).map(|r| r.id) {
                tx.update("bots", row, &[("homeStationID", Value::Int(i64::from(*home))), ("turf", Value::Int(i64::from(*turf)))])?;
            }
            let station = tx.table("staStations").get_id(i64::from(*home)).map(|s| {
                [("stationID", s.value("stationID")), ("solarSystemID", s.value("solarSystemID")), ("constellationID", s.value("constellationID")), ("regionID", s.value("regionID"))]
                    .map(|(c, v)| (c, v.clone()))
            });
            let (Some(sets), Some(row)) = (station, tx.table("character_").get_id(id).map(|r| r.id)) else { continue };
            tx.update("character_", row, &sets)?;
        }
        Ok((corp_ids.to_vec(), alliance_id))
    })?;

    // And in memory.
    let mut p = Politics::new(atlas);
    p.speed = speed;
    for a in &used_sorted {
        p.alliances.insert(alliance_id[a], Alliance { kind: alliances[*a].kind, region: alliances[*a].region });
    }
    for (c, corp) in corps.iter().enumerate() {
        let alliance = corp.alliance.map(|a| alliance_id[&a]);
        p.corps.insert(corp_ids[c], Corp { kind: corp.kind, alliance, members: corp.members.len() as u32 });
        for &i in &corp.members {
            ids[i as usize].corp = corp_ids[c];
            ids[i as usize].alliance = alliance;
        }
    }
    for (i, home, turf) in moves {
        let bot = &mut bots[i as usize];
        bot.home = home;
        bot.turf = turf;
        if bot.station != 0 {
            bot.station = home;
            bot.system = atlas.station_system.get(&home).copied().unwrap_or(bot.system);
        }
    }
    start_wars(&mut p, store, rng, clock, speed).await?;
    info!("bots: politics formed in {:.1}s: {}", started.elapsed().as_secs_f64(), p.summary());
    paint_logos(store).await?;
    Ok(p)
}

/// Moves characters into corporations: member counts, the character rows
/// and their employment history.
fn join_corps(tx: &mut Tx<'_>, who: &[i32], into: &[i32], now: i64) -> Result<(), StoreError> {
    // Each corporation's members: less those leaving, plus those coming.
    let mut delta: HashMap<i64, i64> = HashMap::new();
    {
        let characters = tx.table("character_");
        for id in who {
            if let Some(corp) = characters.get_id(i64::from(*id)).and_then(|c| c.int("corporationID")) {
                *delta.entry(corp).or_default() -= 1;
            }
        }
    }
    for corp in into {
        *delta.entry(i64::from(*corp)).or_default() += 1;
    }
    for (corp, n) in delta {
        let Some(co) = tx.table("corporation").get_id(corp) else { continue };
        let (row, count) = (co.id, co.int("memberCount").unwrap_or(0));
        tx.update("corporation", row, &[("memberCount", Value::Int((count + n).max(0)))])?;
    }
    for (id, corp) in who.iter().zip(into) {
        if let Some(row) = tx.table("character_").get_id(i64::from(*id)).map(|r| r.id) {
            tx.update("character_", row, &[("corporationID", Value::Int(i64::from(*corp))), ("corporationDateTime", Value::Int(now))])?;
        }
    }
    for (id, corp) in who.iter().zip(into) {
        tx.insert(
            "chrEmployment",
            &[("characterID", Value::Int(i64::from(*id))), ("corporationID", Value::Int(i64::from(*corp))), ("startDate", Value::Int(now)), ("deleted", Value::Int(0))],
        )?;
    }
    Ok(())
}

/// Ends a war now (`retracted` and `timeFinished`).
pub(super) fn retract_war(tx: &mut Tx<'_>, war: i32, now: i64) -> Result<(), StoreError> {
    let Some(row) = tx.table("crpWars").get_id(i64::from(war)).map(|w| w.id) else { return Ok(()) };
    tx.update("crpWars", row, &[("retracted", Value::Int(now)), ("timeFinished", Value::Int(now))])
}

// ---- moving along ----------------------------------------------------------------------------

/// Every ten minutes of the bots' clock: wars end and start, and bots in NPC
/// corporations join bot ones.
#[allow(clippy::too_many_arguments)]
pub async fn tick(p: &mut Politics, store: &Store, bots: &[Bot], ids: &mut [Identity], rng: &mut Rng, clock: u32, speed: f64) -> Result<(), StoreError> {
    let now = filetime_now();
    let ended: Vec<War> = {
        let (gone, kept): (Vec<War>, Vec<War>) = std::mem::take(&mut p.wars).into_iter().partition(|w| w.ends <= clock);
        p.wars = kept;
        gone
    };
    for w in &ended {
        store.write(|tx| retract_war(tx, w.id, now))?;
        info!("bots: war {} of {} on {} is over, {} lost", w.id, w.a, w.b, w.kills);
    }
    let before = p.wars.len();
    start_wars(p, store, rng, clock, speed).await?;
    if !ended.is_empty() || p.wars.len() != before {
        store.read(crate::svc::war::refresh);
    }

    // Recruiting: some of the bots still in NPC corporations join.
    let free: Vec<usize> = (0..ids.len()).filter(|i| !p.corps.contains_key(&ids[*i].corp)).collect();
    let expected = free.len() as f64 * RECRUIT_PER_DAY / TICKS_PER_DAY;
    let n = expected.floor() as usize + usize::from(rng.unit() < expected.fract());
    if n > 0 && !p.corps.is_empty() {
        let corps: Vec<(i32, i16)> = p.corps.iter().map(|(id, c)| (*id, c.kind)).collect();
        let (mut who, mut into) = (Vec::new(), Vec::new());
        for _ in 0..n {
            let i = free[rng.below(free.len())];
            let want = if combat(bots[i].role) { KIND_COMBAT_CORP } else { KIND_INDUSTRIAL_CORP };
            let fitting: Vec<i32> = corps.iter().filter(|(_, k)| *k == want).map(|(id, _)| *id).collect();
            let Some(&corp) = fitting.get(rng.below(fitting.len().max(1))) else { continue };
            who.push(bots[i].id);
            into.push(corp);
            ids[i].corp = corp;
            ids[i].alliance = p.corps.get(&corp).and_then(|c| c.alliance);
            if let Some(c) = p.corps.get_mut(&corp) {
                c.members += 1;
            }
        }
        store.write(|tx| join_corps(tx, &who, &into, now))?;
    }
    Ok(())
}

/// Wars until each null-sec alliance has about one, and pirates their prey.
async fn start_wars(p: &mut Politics, store: &Store, rng: &mut Rng, clock: u32, speed: f64) -> Result<(), StoreError> {
    let null: Vec<(i32, Option<i32>)> = p.alliances.iter().filter(|(_, a)| a.kind == KIND_NULL_ALLIANCE).map(|(id, a)| (*id, a.region)).collect();
    let pirates: Vec<i32> = p.alliances.iter().filter(|(_, a)| a.kind == KIND_PIRATE_ALLIANCE).map(|(id, _)| *id).collect();
    let others: Vec<i32> = p.alliances.keys().copied().filter(|a| !pirates.contains(a)).collect();
    let target = null.len() / 2 + pirates.len() / 2;
    let at_war = |p: &Politics, a: i32, b: i32| p.wars.iter().any(|w| (w.a == a && w.b == b) || (w.a == b && w.b == a));
    let mut tries = 0;
    while p.wars.len() < target && tries < 200 {
        tries += 1;
        let (a, b) = if rng.unit() < 0.6 && null.len() >= 2 {
            // Null-sec: a neighbour, if there is one.
            let (a, region) = null[rng.below(null.len())];
            let next: Vec<i32> = null
                .iter()
                .filter(|(o, r)| *o != a && region.zip(*r).is_some_and(|(x, y)| p.neighbours.get(&x).is_some_and(|n| n.contains(&y))))
                .map(|(o, _)| *o)
                .collect();
            let b = next.get(rng.below(next.len().max(1))).copied().unwrap_or_else(|| null[rng.below(null.len())].0);
            (a, b)
        } else if !pirates.is_empty() && !others.is_empty() {
            (pirates[rng.below(pirates.len())], others[rng.below(others.len())])
        } else {
            break;
        };
        if a == b || at_war(p, a, b) {
            continue;
        }
        let days = WAR_DAYS.0 + rng.below((WAR_DAYS.1 - WAR_DAYS.0 + 1) as usize) as u32;
        war_between(p, store, a, b, days, clock, speed).await?;
    }
    Ok(())
}

/// A bot war between two sides, for `days` of the bots' clock. Returns its ID.
pub async fn war_between(p: &mut Politics, store: &Store, a: i32, b: i32, days: u32, clock: u32, speed: f64) -> Result<i32, StoreError> {
    let length = days * DAY;
    let now = filetime_now();
    let finished = now + (length as f64 / speed.max(0.001)) as i64 * SECOND_FT;
    // Dated a day back: bot wars are fought from the start, not after the
    // day's notice a declared war gives.
    let declared = now - crate::svc::war::DAY;
    let id = store.write(|tx| {
        let id = tx.nextval("dyn_id_seq")?;
        tx.insert(
            "crpWars",
            &[
                ("warID", Value::Int(id)),
                ("declaredByID", Value::Int(i64::from(a))),
                ("againstID", Value::Int(i64::from(b))),
                ("timeDeclared", Value::Int(declared)),
                ("timeFinished", Value::Int(finished)),
                ("mutual", Value::Int(1)),
            ],
        )?;
        Ok::<_, StoreError>(id as i32)
    })?;
    info!("bots: war {id}: {a} against {b}, for {days} days");
    p.wars.push(War { id, a, b, ends: clock + length, kills: 0 });
    Ok(id)
}

/// Every minute of the bots' clock: where enemies share a system nobody is
/// watching, they may fight; the losers wake at home. Returns those bots,
/// with the seconds until each one's next step.
pub fn skirmish(p: &mut Politics, atlas: &Atlas, bots: &mut [Bot], ids: &[Identity], watched: &HashSet<i32>, rng: &mut Rng) -> Vec<(u32, u32)> {
    if p.wars.is_empty() {
        return Vec::new();
    }
    let hostile: HashMap<(i32, i32), usize> = p.wars.iter().enumerate().map(|(n, w)| ((w.a.min(w.b), w.a.max(w.b)), n)).collect();
    let fighting: HashSet<i32> = p.wars.iter().flat_map(|w| [w.a, w.b]).collect();
    let mut present: HashMap<i32, HashMap<i32, Vec<u32>>> = HashMap::new();
    for (i, b) in bots.iter().enumerate() {
        if !b.online || b.station != 0 || watched.contains(&b.system) {
            continue;
        }
        let Some(side) = p.side(ids[i].corp).filter(|s| fighting.contains(s)) else { continue };
        present.entry(b.system).or_default().entry(side).or_default().push(i as u32);
    }
    let mut lost = Vec::new();
    for sides in present.values() {
        if sides.len() < 2 {
            continue;
        }
        let list: Vec<(&i32, &Vec<u32>)> = sides.iter().collect();
        for x in 0..list.len() {
            for y in x + 1..list.len() {
                let (a, ma) = list[x];
                let (b, mb) = list[y];
                let Some(&war) = hostile.get(&(*a.min(b), *a.max(b))) else { continue };
                let chance = (0.05 * ma.len().min(mb.len()) as f64).min(0.8);
                if rng.unit() >= chance {
                    continue;
                }
                // The smaller side is the likelier loser.
                let (loser, winner) = if rng.unit() < mb.len() as f64 / (ma.len() + mb.len()) as f64 { (ma, mb) } else { (mb, ma) };
                let n = 1 + rng.below(loser.len().min(3));
                for k in 0..n {
                    let i = loser[(k * 7919 + rng.below(loser.len())) % loser.len()];
                    let bot = &mut bots[i as usize];
                    if bot.station != 0 {
                        continue;
                    }
                    let killer = winner[rng.below(winner.len())];
                    p.unseen_kills.push((i, killer, bot.system, bot.hull));
                    lost.push((i, behaviour::reclone(bot, atlas, rng)));
                    p.wars[war].kills += 1;
                    p.kills += 1;
                    p.lost += 1;
                }
            }
        }
    }
    lost
}
