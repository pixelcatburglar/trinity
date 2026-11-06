//! Agent missions, made up by the server: the client draws the conversation
//! and the journal, but every word and every mission comes from here
//! (agents.py __Interact, journal.py ShowAgentTab); agtMissions holds none.
//!
//! What an agent offers follows its division:
//! - courier (Distribution, Storage and the offices): carry a crate from the
//!   agent's station to one a few jumps off; it is handed over on accepting,
//!   and the mission is completed docked at the drop-off;
//! - mining (Mining, Astrosurveying): bring ore to the agent's station;
//! - trade (Manufacturing, Production, R&D): bring goods to the agent's station;
//! - encounter (Security and the like): clear the pirates at a site near the
//!   agent (`sites`), then report back from anywhere.
//!
//! The conversation is DoAction(actionID) with the actions below. Level 1
//! agents talk to anyone; higher ones want an effective standing (the best of
//! agent, corporation and faction) of 1, 3, 5 and 7, and none works for a
//! pilot one of them puts at -2 or below. Completing raises the agent's
//! standing and its corporation's, by the client's own formula
//! (standingsvc.py OnStandingsModified); quitting lowers the agent's.
//! Offers last a day, accepted missions a week; finishing within the bonus
//! time pays a bonus on top. The amounts are this server's choice.

use eve_marshal::Value;
use tracing::{error, info};

use crate::ballpark::Vec3;
use crate::combat::Rng;
use crate::items::{ItemTxn, NewItem};
use crate::services::{Ctx, Outcome};
use crate::store::{Db, Row, Store, StoreError, Tx, Value as V};
use crate::svc::rows::key_val;
use crate::svc::wallet::{self, Entry, Transfer};
use crate::world::World;

const KIND_COURIER: i16 = 1;
const KIND_MINING: i16 = 2;
const KIND_TRADE: i16 = 3;
const KIND_ENCOUNTER: i16 = 4;

const OFFERED: i16 = 0;
const ACCEPTED: i16 = 2;
const CLEARED: i16 = 3;
const COMPLETED: i16 = 4;
const DECLINED: i16 = 5;
const QUIT: i16 = 6;
const EXPIRED: i16 = 7;

/// The conversation's choices (the action IDs are this server's own).
const ASK: i64 = 1;
const ACCEPT: i64 = 2;
const DECLINE: i64 = 3;
const COMPLETE: i64 = 4;
const QUIT_MISSION: i64 = 5;

/// Agent types that hand out missions: basic, tutorial, CONCORD, storyline
/// and event agents.
const MISSION_AGENTS: [i64; 5] = [2, 3, 5, 6, 8];
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;
const TYPE_SOLAR_SYSTEM: i64 = 5;
const CATEGORY_COMMODITY: i32 = 17;

const REF_AGENT_MISSION_REWARD: i32 = 33;
const REF_AGENT_MISSION_TIME_BONUS: i32 = 34;

const SECOND: i64 = 10_000_000;
const HOUR: i64 = 3_600 * SECOND;
const OFFER_LIFETIME: i64 = 24 * HOUR;
const MISSION_LIFETIME: i64 = 7 * 24 * HOUR;

/// By level (1 to 5): the base reward, the cargo or ore volume asked for, how
/// strong an encounter's pirates are (as the security of a system with such
/// belt rats), and the standing a pilot needs.
const REWARD: [f64; 5] = [60_000.0, 220_000.0, 650_000.0, 1_600_000.0, 5_000_000.0];
const VOLUME: [f64; 5] = [5.0, 40.0, 200.0, 1_000.0, 5_000.0];
const ORE_VOLUME: [f64; 5] = [1_000.0, 3_000.0, 8_000.0, 20_000.0, 50_000.0];
const ENCOUNTER_SECURITY: [f64; 5] = [0.9, 0.6, 0.3, -0.2, -0.8];
const STANDING_NEEDED: [f64; 5] = [-10.0, 1.0, 3.0, 5.0, 7.0];
/// The ores each level asks for, commonest first.
const ORES: [&[i64]; 5] = [&[1230, 1228], &[1230, 1228, 1224], &[1224, 18, 1227], &[1227, 20, 1226], &[20, 1226, 1231, 21]];

const COURIER_NAMES: [&str; 5] = ["Urgent Delivery", "Sealed Documents", "Parcel Run", "Supply Drop", "Special Delivery"];
const MINING_NAMES: [&str; 3] = ["Ore Requisition", "Raw Materials", "Mineral Shortage"];
const TRADE_NAMES: [&str; 3] = ["Supply Order", "Procurement", "Shopping List"];
const ENCOUNTER_NAMES: [&str; 5] = ["Pirate Incursion", "Clear the Pocket", "Hunters Hunted", "Smuggler Camp", "Rogue Patrol"];

// ---- the agent and the mission -----------------------------------------------------------

struct Agent {
    id: i64,
    name: String,
    division: i64,
    corp: i64,
    faction: Option<i64>,
    station: i64,
    station_name: String,
    system: i64,
    level: usize,
    quality: i64,
    agent_type: i64,
}

/// An agent, if it is one and its station is known: its name is on its
/// character row, its faction its corporation's.
fn agent(db: &Db, id: i64) -> Option<Agent> {
    let a = crate::sde::table("agtAgents").get("agentID", (id as i32).into())?;
    let station = a.int("stationID")?;
    let s = db.table("staStations").get_id(station)?;
    let name = db.table("character_").get_id(id).and_then(|c| c.text("characterName")).unwrap_or_default();
    let corp = a.int("corporationID");
    Some(Agent {
        id: a.int("agentID")?,
        name: name.to_string(),
        division: a.int("divisionID").unwrap_or(0),
        corp: corp.unwrap_or(0),
        faction: corp.and_then(|c| crate::sde::table("crpNPCCorporations").get("corporationID", c)).and_then(|n| n.int("factionID")),
        station,
        station_name: s.text("stationName").unwrap_or_default().to_string(),
        system: s.int("solarSystemID").unwrap_or(0),
        level: a.int("level").unwrap_or(1).clamp(1, 5) as usize,
        quality: a.int("quality").unwrap_or(0),
        agent_type: a.int("agentTypeID").unwrap_or(2),
    })
}

#[derive(Debug, Clone)]
struct Mission {
    id: i64,
    agent_id: i64,
    kind: i16,
    state: i16,
    name: String,
    type_id: i64,
    quantity: i64,
    cargo_id: Option<i64>,
    destination: i64,
    site: Option<Vec3>,
    reward: f64,
    bonus: f64,
    bonus_seconds: i64,
    accepted_at: Option<i64>,
    expires_at: i64,
}

fn mission_of(r: &Row) -> Mission {
    let site = match (r.float("siteX"), r.float("siteY"), r.float("siteZ")) {
        (Some(x), Some(y), Some(z)) => Some(Vec3::new(x, y, z)),
        _ => None,
    };
    Mission {
        id: r.int("missionID").unwrap_or(0),
        agent_id: r.int("agentID").unwrap_or(0),
        kind: r.int("kind").unwrap_or(0) as i16,
        state: r.int("state").unwrap_or(0) as i16,
        name: r.text("name").unwrap_or_default().to_string(),
        type_id: r.int("typeID").unwrap_or(0),
        quantity: r.int("quantity").unwrap_or(0),
        cargo_id: r.int("cargoID"),
        destination: r.int("destinationID").unwrap_or(0),
        site,
        reward: r.float("reward").unwrap_or(0.0),
        bonus: r.float("bonus").unwrap_or(0.0),
        bonus_seconds: r.int("bonusSeconds").unwrap_or(0),
        accepted_at: r.int("acceptedAt"),
        expires_at: r.int("expiresAt").unwrap_or(0),
    }
}

/// Whether a mission is still open: offered, accepted or cleared.
fn is_open(r: &Row) -> bool {
    matches!(r.int("state"), Some(0 | 2 | 3))
}

/// The character's open missions (one agent's, or all); those past their
/// time lapse on the way.
fn open_missions(tx: &mut Tx<'_>, char_id: i64, agent_id: Option<i64>) -> Vec<Mission> {
    let now = crate::bulkdata::filetime_now();
    let lapsed: Vec<_> = tx
        .table("agtCharMissions")
        .find(&["characterID"], &[V::Int(char_id)])
        .filter(|r| is_open(r) && r.int("expiresAt").is_some_and(|e| e < now))
        .map(|r| r.id)
        .collect();
    for row in lapsed {
        if let Err(e) = tx.update("agtCharMissions", row, &[("state", V::Int(EXPIRED.into())), ("closedAt", V::Int(now))]) {
            error!("lapsing missions of {char_id}: {}", crate::logging::chain(&e));
            break;
        }
    }
    let mut open: Vec<Mission> = tx
        .table("agtCharMissions")
        .find(&["characterID"], &[V::Int(char_id)])
        .filter(|r| is_open(r) && agent_id.is_none_or(|a| r.int("agentID") == Some(a)))
        .map(|r| mission_of(&r))
        .collect();
    open.sort_by_key(|m| m.id);
    open
}

/// [`open_missions`] in a write of its own.
fn open_missions_of(store: &Store, char_id: i64, agent_id: Option<i64>) -> Vec<Mission> {
    store.write(|tx| Ok::<_, StoreError>(open_missions(tx, char_id, agent_id))).unwrap_or_default()
}

fn kind_of(division: i64) -> i16 {
    match division {
        14 | 5 => KIND_MINING,
        12 | 16 | 18 => KIND_TRADE,
        6 | 9 | 10 | 19 | 21 => KIND_ENCOUNTER,
        _ => KIND_COURIER,
    }
}

fn kind_name(kind: i16) -> &'static str {
    match kind {
        KIND_COURIER => "Courier",
        KIND_MINING => "Mining",
        KIND_TRADE => "Trade",
        _ => "Encounter",
    }
}

// ---- the conversation ----------------------------------------------------------------------

/// DoAction(actionID) -> ((agentSays, [(actionID, text)]), oob).
pub async fn conversation(ctx: &Ctx<'_>, agent_id: i64, action: Option<i64>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(agent) = ctx.store.read(|db| agent(db, agent_id)) else {
        return said("I have nothing to say to you.".into(), Vec::new());
    };
    let open = open_missions_of(ctx.store, me, Some(agent_id)).into_iter().next();
    let result = match (action, open) {
        (Some(ASK), None) => ask(ctx, &agent).await,
        (Some(ACCEPT), Some(m)) if m.state == OFFERED => accept(ctx, &agent, m).await,
        (Some(DECLINE), Some(m)) if m.state == OFFERED => close(ctx, &agent, &m, DECLINED, "declined").await.map(|_| {
            said("Very well. Perhaps another time.".into(), Vec::new())
        }),
        (Some(COMPLETE), Some(m)) if m.state != OFFERED => complete(ctx, &agent, m).await,
        (Some(QUIT_MISSION), Some(m)) if m.state != OFFERED => quit(ctx, &agent, &m).await,
        (_, Some(m)) => Ok(status(ctx, &agent, &m).await),
        (_, None) => Ok(said(
            format!("Hello, {}. What can I do for you?", html(&ctx.session.char_name)),
            vec![(ASK, "Do you have any work for me?")],
        )),
    };
    result.unwrap_or_else(|why| said(why, Vec::new()))
}

fn said(says: String, choices: Vec<(i64, &str)>) -> Value {
    let choices = choices.into_iter().map(|(id, t)| Value::Tuple(vec![Value::Int(id), Value::text(t)])).collect();
    Value::Tuple(vec![Value::Tuple(vec![Value::text(says), Value::List(choices)]), Value::Dict(Vec::new())])
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn ask(ctx: &Ctx<'_>, agent: &Agent) -> Result<Value, String> {
    if !MISSION_AGENTS.contains(&agent.agent_type) {
        return Err("I don't hand out missions. Perhaps one of my colleagues can help you.".into());
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let standings = ctx.store.read(|db| standings_toward(db, me, agent));
    if standings.iter().any(|s| *s <= -2.0) {
        return Err("I won't work with you. Mend your relations with us first.".into());
    }
    let best = standings.iter().copied().fold(0.0, f64::max);
    let needed = STANDING_NEEDED[agent.level - 1];
    if best < needed {
        return Err(format!(
            "I'm afraid I can't trust you with my work yet. You'll need an effective standing of {needed:.1} with me, my corporation or its faction; yours is {best:.2}."
        ));
    }
    let mission = offer(ctx, agent).await.ok_or_else(|| "I have nothing for you right now. Check back with me later.".to_string())?;
    notify_change(ctx.world, me, "offered", agent.id);
    Ok(briefing(ctx, agent, &mission).await)
}

/// The offer, with what it pays and what the pilot may answer.
async fn briefing(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Value {
    let mut says = format!("<b>{}</b><br><br>{}", html(&m.name), ctx.store.read(|db| objective(db, agent, m)));
    says.push_str(&format!("<br><br>{}", rewards(agent, m)));
    said(says, vec![(ACCEPT, "Accept"), (DECLINE, "Decline")])
}

fn rewards(agent: &Agent, m: &Mission) -> String {
    format!(
        "Reward: {} ISK and {} loyalty points with my corporation<br>Bonus: {} ISK if completed within {}.",
        isk(m.reward),
        mission_lp(agent),
        isk(m.bonus),
        interval(m.bonus_seconds)
    )
}

/// Loyalty points a mission pays, by agent level (1 to 5), spent in the LP
/// store of the agent's corporation (svc::lp). The amounts are this server's.
const LP_BY_LEVEL: [i64; 6] = [0, 100, 250, 600, 1_500, 3_500];

fn mission_lp(agent: &Agent) -> i64 {
    LP_BY_LEVEL[agent.level.min(LP_BY_LEVEL.len() - 1)]
}

async fn status(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Value {
    if m.state == OFFERED {
        return briefing(ctx, agent, m).await;
    }
    let mut says = format!("<b>{}</b><br><br>{}", html(&m.name), ctx.store.read(|db| objective(db, agent, m)));
    if m.state == CLEARED {
        says.push_str("<br><br>I hear the pirates are gone. Good work.");
    }
    said(says, vec![(COMPLETE, "I have completed the mission."), (QUIT_MISSION, "I want to quit this mission.")])
}

/// What the pilot has to do, in the agent's words.
fn objective(db: &Db, agent: &Agent, m: &Mission) -> String {
    let what = html(&type_name(m.type_id));
    match m.kind {
        KIND_COURIER => {
            let (station, system) = station_names(db, m.destination);
            format!(
                "I need {} unit(s) of {what} taken from here, {}, to {} in {}. You'll find the cargo in your hangar here once you accept.",
                m.quantity,
                html(&agent.station_name),
                html(&station),
                html(&system)
            )
        }
        KIND_MINING => format!("Bring me {} unit(s) of {what}, here at {}.", m.quantity, html(&agent.station_name)),
        KIND_TRADE => format!("I'm short of {} unit(s) of {what}. Get them to me here at {}.", m.quantity, html(&agent.station_name)),
        _ => format!(
            "A band of pirates has set up in {}. Clear them out; the location is in your journal. Report back to me when it's done.",
            html(&system_name(db, m.destination))
        ),
    }
}

// ---- making an offer -----------------------------------------------------------------------

async fn offer(ctx: &Ctx<'_>, agent: &Agent) -> Option<Mission> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let mut rng = Rng::seeded(now as u64 ^ (me as u64) << 16 ^ agent.id as u64);
    let level = agent.level;
    let quality = 1.0 + agent.quality as f64 / 50.0;
    let base = REWARD[level - 1] * quality;
    let kind = kind_of(agent.division);
    let pick = |rng: &mut Rng, names: &[&'static str]| names[rng.below(names.len())];
    let (name, type_id, quantity, destination, site, reward, bonus, bonus_hours) = match kind {
        KIND_COURIER => {
            let reach = (level as i64 + 1).min(6);
            let (systems, stations) = ctx.store.read(|db| {
                let systems: Vec<(i64, i64)> = crate::gates::within(db, agent.system, reach);
                let stations: Vec<(i64, i64)> = db
                    .table("staStations")
                    .rows()
                    .filter(|r| r.int("solarSystemID").is_some_and(|s| systems.iter().any(|(id, _)| *id == s)))
                    .filter(|r| r.int("stationID").is_some_and(|s| s != agent.station))
                    .filter_map(|r| Some((r.int("stationID")?, r.int("solarSystemID")?)))
                    .collect();
                (systems, stations)
            });
            let &(station, system) = stations.get(rng.below(stations.len().max(1)))?;
            let jumps = systems.iter().find(|(s, _)| *s == system).map_or(0, |(_, n)| *n);
            let (cargo, volume, _) = commodity(&mut rng, 0.1, 10.0)?;
            let quantity = (VOLUME[level - 1] * rng.range(0.5, 1.0) / volume).ceil().max(1.0) as i64;
            let reward = base * (0.6 + 0.15 * jumps as f64);
            (pick(&mut rng, &COURIER_NAMES), cargo, quantity, station, None, reward, reward * 0.5, 1.0 + jumps as f64 * 0.25)
        }
        KIND_MINING => {
            let ores = ORES[level - 1];
            let ore = ores[rng.below(ores.len()).min(rng.below(ores.len()))];
            let volume = type_volume(ore).max(0.01);
            let quantity = (ORE_VOLUME[level - 1] * rng.range(0.6, 1.0) / volume).round().max(1.0) as i64;
            (pick(&mut rng, &MINING_NAMES), ore, quantity, agent.station, None, base, base * 0.3, 4.0)
        }
        KIND_TRADE => {
            let (goods, _, price) = commodity(&mut rng, 0.01, 50.0)?;
            let budget = base * 0.5;
            let quantity = (budget / price.max(1.0)).round().clamp(1.0, 100_000.0) as i64;
            let reward = base * 0.5 + price * quantity as f64 * 1.15;
            (pick(&mut rng, &TRADE_NAMES), goods, quantity, agent.station, None, reward, base * 0.2, 4.0)
        }
        _ => {
            let systems = ctx.store.read(|db| crate::gates::within(db, agent.system, level as i64 - 1));
            let &(system, _) = systems.get(rng.below(systems.len().max(1)))?;
            let at = ctx.store.read(|db| site_position(db, system, &mut rng));
            let reward = base * 1.3;
            (pick(&mut rng, &ENCOUNTER_NAMES), 0, 0, system, Some(at), reward, reward * 0.4, 2.0)
        }
    };
    let (bonus_seconds, expires) = ((bonus_hours * 3_600.0) as i32, now + OFFER_LIFETIME);
    let coordinate = |c: fn(&Vec3) -> f64| site.as_ref().map_or(V::Null, |s| V::Float(c(s)));
    let sets = [
        ("characterID", V::Int(me)),
        ("agentID", V::Int(agent.id)),
        ("kind", V::Int(kind.into())),
        ("state", V::Int(0)),
        ("name", V::Text(name.into())),
        ("typeID", if type_id != 0 { V::Int(type_id) } else { V::Null }),
        ("quantity", V::Int(quantity)),
        ("destinationID", V::Int(destination)),
        ("siteX", coordinate(|s| s.x)),
        ("siteY", coordinate(|s| s.y)),
        ("siteZ", coordinate(|s| s.z)),
        ("reward", V::Float(reward.round())),
        ("bonus", V::Float(bonus.round())),
        ("bonusSeconds", V::Int(bonus_seconds.into())),
        ("offeredAt", V::Int(now)),
        ("expiresAt", V::Int(expires)),
    ];
    // missionID is an identity column the store's schema gives no default:
    // drawn from its sequence here.
    let id = ctx
        .store
        .write(|tx| {
            let id = tx.nextval("agtCharMissions_missionID_seq")?;
            let mut sets = sets.to_vec();
            sets.push(("missionID", V::Int(id)));
            tx.insert("agtCharMissions", &sets)?;
            Ok::<_, StoreError>(id)
        })
        .map_err(|e| error!("offering {me} a mission of agent {}: {}", agent.id, crate::logging::chain(&e)))
        .ok()?;
    info!("agent {} offered {me} mission {id}: {} ({name})", agent.id, kind_name(kind));
    Some(Mission {
        id,
        agent_id: agent.id,
        kind,
        state: OFFERED,
        name: name.into(),
        type_id,
        quantity,
        cargo_id: None,
        destination,
        site,
        reward: reward.round(),
        bonus: bonus.round(),
        bonus_seconds: bonus_seconds as i64,
        accepted_at: None,
        expires_at: expires,
    })
}

/// A commodity of one unit's volume within bounds: (typeID, volume, base price).
/// Marketable and priced, by type ID.
fn commodity(rng: &mut Rng, min_volume: f64, max_volume: f64) -> Option<(i64, f64, f64)> {
    let types = crate::sde::table("invTypes");
    let rows: Vec<crate::sde::Row> = crate::sde::get()
        .types_in_category(CATEGORY_COMMODITY.into())
        .iter()
        .filter_map(|t| types.get("typeID", (*t).into()))
        .filter(|t| {
            t.int("marketGroupID").is_some()
                && t.float("basePrice").is_some_and(|p| p > 0.0)
                && t.float("volume").is_some_and(|v| (min_volume..=max_volume).contains(&v))
        })
        .collect();
    let r = rows.get(rng.below(rows.len().max(1)))?;
    Some((r.int("typeID")?, r.float("volume").unwrap_or(1.0), r.float("basePrice").unwrap_or(1.0)))
}

/// Somewhere quiet in a system: a few thousand km off one of its planets,
/// or an AU from the sun if it has none.
fn site_position(db: &Db, system: i64, rng: &mut Rng) -> Vec3 {
    let planets: Vec<(Vec3, f64)> = db
        .table("mapDenormalize")
        .find(&["solarSystemID"], &[V::Int(system)])
        .filter(|r| r.int("groupID") == Some(7))
        .filter_map(|r| Some((Vec3::new(r.float("x")?, r.float("y")?, r.float("z")?), r.float("radius").unwrap_or(0.0))))
        .collect();
    let (centre, radius) = planets.get(rng.below(planets.len().max(1))).copied().unwrap_or((Vec3::ZERO, 1.5e11));
    let out = rng.in_sphere(1.0);
    let dir = if out.len() > 1e-6 { out.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
    centre + dir * (radius + rng.range(3.0e6, 9.0e6))
}

// ---- accepting, completing, quitting -------------------------------------------------------

async fn accept(ctx: &Ctx<'_>, agent: &Agent, m: Mission) -> Result<Value, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let mut cargo = None;
    if m.kind == KIND_COURIER {
        if ctx.session.station_id != Some(agent.station) {
            return Err(format!("Come and see me at {} to pick up the cargo.", html(&agent.station_name)));
        }
        let crate_ = NewItem { type_id: m.type_id, owner_id: me, location_id: agent.station, flag: FLAG_HANGAR, quantity: m.quantity, singleton: false };
        let (id, events) = ctx
            .store
            .write(|tx| {
                let mut items = ItemTxn::new(tx);
                let id = items.create(crate_)?;
                Ok::<_, StoreError>((id, items.events()))
            })
            .map_err(|e| e.to_string())?;
        cargo = Some(id);
        crate::items::notify(ctx.world, ctx.client_id, events);
    }
    let sets = [
        ("state", V::Int(ACCEPTED.into())),
        ("acceptedAt", V::Int(now)),
        ("expiresAt", V::Int(now + MISSION_LIFETIME)),
        ("cargoID", cargo.map_or(V::Null, V::Int)),
    ];
    let saved = ctx.store.write(|tx| {
        let offered = tx.table("agtCharMissions").get_id(m.id).filter(|r| r.int("state") == Some(OFFERED.into())).map(|r| r.id);
        match offered {
            Some(row) => tx.update("agtCharMissions", row, &sets),
            None => Ok(()),
        }
    });
    if let Err(e) = saved {
        error!("accepting mission {}: {}", m.id, crate::logging::chain(&e));
    }
    info!("{me} accepted mission {} from agent {}", m.id, agent.id);
    notify_change(ctx.world, me, "accepted", agent.id);
    let accepted = Mission { state: ACCEPTED, accepted_at: Some(now), cargo_id: cargo, ..m };
    let mut says = format!("Excellent. {}", ctx.store.read(|db| objective(db, agent, &accepted)));
    says.push_str("<br><br>Come back to me when you're done.");
    Ok(said(says, Vec::new()))
}

/// Why completing a mission stopped, as the agent says it.
struct Refused(String);

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Self {
        Refused(e.to_string())
    }
}

async fn complete(ctx: &Ctx<'_>, agent: &Agent, m: Mission) -> Result<Value, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let in_time = m.accepted_at.is_some_and(|a| now - a <= m.bonus_seconds * SECOND);
    let earned = mission_lp(agent);
    let (balance, events) = ctx.store.write(|tx| complete_in(tx, ctx, agent, &m, now, in_time, earned)).map_err(|r| r.0)?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    let gain = 0.01 * (1.0 + agent.level as f64);
    modify_standing(ctx.store, ctx.world, me, agent.id, gain);
    modify_standing(ctx.store, ctx.world, me, agent.corp, gain / 2.0);
    close_site(ctx.world, m.id);
    {
        let w = ctx.world.lock();
        let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
        w.notify(&w.clients_of(me), "OnAccountChange", cash);
    }
    notify_change(ctx.world, me, "completed", agent.id);
    info!("{me} completed mission {} for agent {}{}", m.id, agent.id, if in_time { " in time for the bonus" } else { "" });
    let paid = m.reward + if in_time { m.bonus } else { 0.0 };
    Ok(said(
        format!(
            "Thank you, {}. I've transferred {} ISK to your account, and {} loyalty points with my corporation.",
            html(&ctx.session.char_name),
            isk(paid),
            earned
        ),
        Vec::new(),
    ))
}

/// Completing, in one transaction: what the mission asked for taken, the
/// reward and loyalty points paid, the mission closed. The wallet's balance
/// and the item events.
fn complete_in(tx: &mut Tx<'_>, ctx: &Ctx<'_>, agent: &Agent, m: &Mission, now: i64, in_time: bool, earned: i64) -> Result<(f64, Vec<Value>), Refused> {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut txn = ItemTxn::new(tx);
    match m.kind {
        KIND_COURIER => {
            if ctx.session.station_id != Some(m.destination) {
                let (station, _) = station_names(txn.db(), m.destination);
                return Err(Refused(format!("The cargo has to reach {} first.", html(&station))));
            }
            let item = match m.cargo_id {
                Some(id) => txn.item(id)?,
                None => None,
            };
            let delivered = match &item {
                Some(i) if i.location_id == m.destination && i.owner_id == me => true,
                Some(i) if Some(i.location_id) == ctx.session.ship_id => true,
                _ => false,
            };
            if !delivered || item.as_ref().is_some_and(|i| i.quantity < m.quantity) {
                return Err(Refused("I don't see the cargo there. Bring all of it to the drop-off and talk to me again.".into()));
            }
            txn.destroy(m.cargo_id.unwrap_or(0))?;
        }
        KIND_MINING | KIND_TRADE => {
            if ctx.session.station_id != Some(agent.station) {
                return Err(Refused(format!("Bring them to me at {}.", html(&agent.station_name))));
            }
            let mut left = m.quantity;
            let sources = [(agent.station, FLAG_HANGAR), (ctx.session.ship_id.unwrap_or(0), FLAG_CARGO)];
            let mut stacks = Vec::new();
            for (location, flag) in sources {
                let mut here: Vec<(i64, i64)> = txn
                    .db()
                    .table("entity")
                    .find(&["locationID", "flag", "ownerID"], &[V::Int(location), V::Int(flag), V::Int(me)])
                    .filter(|r| r.int("typeID") == Some(m.type_id) && r.int("singleton") == Some(0))
                    .map(|r| (r.int("itemID").unwrap_or(0), r.int("quantity").unwrap_or(0)))
                    .collect();
                here.sort_by_key(|(id, _)| *id);
                stacks.extend(here);
            }
            if stacks.iter().map(|(_, q)| q).sum::<i64>() < m.quantity {
                return Err(Refused(format!("You don't have {} unit(s) of {} here yet.", m.quantity, html(&type_name(m.type_id)))));
            }
            for (id, q) in stacks {
                if left == 0 {
                    break;
                }
                let take = q.min(left);
                txn.set_quantity(id, q - take)?;
                left -= take;
            }
        }
        _ if m.state != CLEARED => return Err(Refused("Those pirates are still out there.".into())),
        _ => {}
    }

    let reason = format!("Mission: {}", m.name);
    let mut balance = 0.0;
    for (amount, ref_type) in [(m.reward, REF_AGENT_MISSION_REWARD), (if in_time { m.bonus } else { 0.0 }, REF_AGENT_MISSION_TIME_BONUS)] {
        if amount <= 0.0 {
            continue;
        }
        let entry = Entry { ref_type, payer: agent.id, payee: me, arg_id1: "", reason: &reason };
        balance = match wallet::post(txn.tx(), me, amount, &entry) {
            Ok(b) => b,
            Err(Transfer::Db(e)) => return Err(Refused(format!("The reward could not be paid: {e}"))),
            Err(_) => return Err(Refused("Your wallet could not be found.".into())),
        };
    }
    if earned > 0 {
        crate::svc::lp::add_lp(txn.tx(), me as i32, agent.corp as i32, earned)
            .map_err(|e| Refused(format!("The loyalty points could not be paid: {e}")))?;
    }
    if let Some(row) = txn.db().table("agtCharMissions").get_id(m.id).map(|r| r.id) {
        txn.tx().update("agtCharMissions", row, &[("state", V::Int(COMPLETED.into())), ("closedAt", V::Int(now))])?;
    }
    Ok((balance, txn.events()))
}

async fn quit(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Result<Value, String> {
    close(ctx, agent, m, QUIT, "quit").await?;
    let me = ctx.session.char_id.unwrap_or(0);
    modify_standing(ctx.store, ctx.world, me, agent.id, -0.02 * agent.level as f64);
    close_site(ctx.world, m.id);
    Ok(said("That's disappointing. I'll remember this.".into(), Vec::new()))
}

async fn close(ctx: &Ctx<'_>, agent: &Agent, m: &Mission, state: i16, what: &'static str) -> Result<(), String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let saved = ctx.store.write(|tx| match tx.table("agtCharMissions").get_id(m.id).map(|r| r.id) {
        Some(row) => tx.update("agtCharMissions", row, &[("state", V::Int(state.into())), ("closedAt", V::Int(now))]),
        None => Ok(()),
    });
    if let Err(e) = saved {
        error!("closing mission {}: {}", m.id, crate::logging::chain(&e));
    }
    info!("{me} {what} mission {} of agent {}", m.id, agent.id);
    notify_change(ctx.world, me, what, agent.id);
    Ok(())
}

/// An encounter's site is shut when its mission is over.
fn close_site(world: &World, mission_id: i64) {
    let mut w = world.lock();
    let systems: Vec<i64> = w.parks.iter().filter(|(_, p)| p.sites.values().any(|s| s.mission_id == mission_id)).map(|(id, _)| *id).collect();
    for system in systems {
        let entries = w.parks.get_mut(&system).map(|p| crate::sites::close(p, mission_id)).unwrap_or_default();
        crate::svc::space::broadcast(&w, system, None, entries);
    }
}

/// Heartbeat job: an encounter's last pirate is dead.
pub async fn site_cleared(world: &World, store: &Store, mission_id: i64, char_id: i64) {
    let cleared = store.write(|tx| {
        let accepted = tx
            .table("agtCharMissions")
            .get_id(mission_id)
            .filter(|r| r.int("characterID") == Some(char_id) && r.int("state") == Some(ACCEPTED.into()))
            .map(|r| (r.id, r.int("agentID")));
        let Some((row, agent)) = accepted else { return Ok(None) };
        tx.update("agtCharMissions", row, &[("state", V::Int(CLEARED.into()))])?;
        Ok::<_, StoreError>(Some(agent))
    });
    match cleared {
        Ok(Some(agent)) => {
            let agent = agent.unwrap_or(0);
            info!("mission {mission_id} of {char_id}: the site is cleared");
            notify_change(world, char_id, "objective", agent);
            let w = world.lock();
            let note = Value::Dict(vec![(Value::bytes("info"), Value::text("The pirates are gone. Report back to your agent."))]);
            w.notify(&w.clients_of(char_id), "OnRemoteMessage", Value::Tuple(vec![Value::bytes("CustomInfo"), note]));
        }
        Ok(None) => {}
        Err(e) => error!("recording the clearing of mission {mission_id}: {}", crate::logging::chain(&e)),
    }
}

// ---- standings -------------------------------------------------------------------------------

/// The pilot's standing from the agent, its corporation and its faction.
fn standings_toward(db: &Db, me: i64, agent: &Agent) -> Vec<f64> {
    let ids: Vec<i64> = [Some(agent.id), Some(agent.corp), agent.faction].into_iter().flatten().collect();
    let mut found: Vec<f64> = db
        .table("chrNPCStandings")
        .find(&["characterID"], &[V::Int(me)])
        .filter(|r| r.int("fromID").is_some_and(|from| ids.contains(&from)))
        .filter_map(|r| r.float("standing"))
        .collect();
    found.resize(ids.len(), 0.0);
    found
}

/// A standing moved by `m` the way the client moves it (standingsvc.py
/// OnStandingsModified): a gain closes that share of the gap to 10, a loss
/// that share of the way down to -10.
pub fn modified(standing: f64, m: f64) -> f64 {
    let s = standing / 10.0;
    let new = if m > 0.0 { 1.0 - (1.0 - s) * (1.0 - m) } else { s + (1.0 + s) * m };
    (new * 10.0).clamp(-10.0, 10.0)
}

fn modify_standing(store: &Store, world: &World, me: i64, from: i64, m: f64) {
    let saved = store.write(|tx| {
        let old = tx.table("chrNPCStandings").get(&[V::Int(me), V::Int(from)]).and_then(|r| r.float("standing")).unwrap_or(0.0);
        let new = modified(old, m);
        let sets = [("characterID", V::Int(me)), ("fromID", V::Int(from)), ("standing", V::Float(new))];
        tx.upsert("chrNPCStandings", &["characterID", "fromID"], &sets, &[("standing", V::Float(new))])?;
        Ok::<_, StoreError>(new)
    });
    let new = match saved {
        Ok(new) => new,
        Err(e) => {
            error!("saving the standing of {from} toward {me}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let w = world.lock();
    w.notify(&w.clients_of(me), "OnStandingSet", Value::Tuple(vec![Value::Int(from), Value::Int(me), Value::Double(new)]));
}

fn notify_change(world: &World, char_id: i64, what: &str, agent_id: i64) {
    let w = world.lock();
    w.notify(&w.clients_of(char_id), "OnAgentMissionChange", Value::Tuple(vec![Value::text(what), Value::Int(agent_id)]));
}

// ---- the journal -----------------------------------------------------------------------------

/// GetMyJournalDetails() -> (missions, offers, research): each mission as
/// (missionState, important, missionType, missionName, agentID,
/// expirationTime, bookmarks) (journal.py:241).
pub async fn journal(ctx: &Ctx<'_>, agent_id: Option<i64>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut missions = Vec::new();
    for m in open_missions_of(ctx.store, me, agent_id) {
        let state = if m.state == OFFERED { 0 } else { 2 };
        let bookmarks = ctx.store.read(|db| bookmarks(db, &m));
        missions.push(Value::Tuple(vec![
            Value::Int(state),
            Value::Int(0),
            Value::text(kind_name(m.kind)),
            Value::text(&m.name),
            Value::Int(m.agent_id),
            Value::Long(m.expires_at),
            Value::List(bookmarks),
        ]));
    }
    Value::Tuple(vec![Value::List(missions), Value::List(Vec::new()), Value::List(Vec::new())])
}

/// The places a mission sends the pilot, as the address book merges them
/// into its own (addressbook.py GetMapBookmarks).
fn bookmarks(db: &Db, m: &Mission) -> Vec<Value> {
    let bookmark = |location_type: &str, hint: String, item: i64, type_id: i64, system: i64, at: Vec3| {
        key_val(vec![
            ("agentID", Value::Int(m.agent_id)),
            ("locationType", Value::text(location_type)),
            ("locationNumber", Value::Int(m.id)),
            ("hint", Value::text(hint)),
            ("itemID", Value::Int(item)),
            ("typeID", Value::Int(type_id)),
            ("locationID", Value::Int(system)),
            ("x", Value::Double(at.x)),
            ("y", Value::Double(at.y)),
            ("z", Value::Double(at.z)),
        ])
    };
    match (m.kind, m.site) {
        (KIND_ENCOUNTER, Some(at)) => {
            let name = system_name(db, m.destination);
            vec![bookmark("dungeon", format!("Objective ({name})"), m.destination, TYPE_SOLAR_SYSTEM, m.destination, at)]
        }
        (KIND_COURIER, _) => {
            let Some(r) = db.table("staStations").get_id(m.destination) else { return Vec::new() };
            let at = Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0));
            let hint = format!("Drop-off ({})", r.text("stationName").unwrap_or_default());
            vec![bookmark("dropoff", hint, m.destination, r.int("stationTypeID").unwrap_or(0), r.int("solarSystemID").unwrap_or(0), at)]
        }
        _ => Vec::new(),
    }
}

/// GetMissionJournalInfo(): the mission's page, or None without one.
pub async fn mission_page(ctx: &Ctx<'_>, agent_id: i64) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(agent) = ctx.store.read(|db| agent(db, agent_id)) else { return Value::None };
    let Some(m) = open_missions_of(ctx.store, me, Some(agent_id)).into_iter().next() else { return Value::None };
    let state = match m.state {
        OFFERED => "Offered",
        CLEARED => "Objective complete; report to the agent",
        _ => "Accepted",
    };
    let page = format!(
        "<HTML><BODY><H3>{}</H3><TABLE><TR><TD>Agent:</TD><TD>{}</TD></TR><TR><TD>Type:</TD><TD>{}</TD></TR><TR><TD>State:</TD><TD>{}</TD></TR></TABLE><BR>{}<BR><BR>{}</BODY></HTML>",
        html(&m.name),
        html(&agent.name),
        kind_name(m.kind),
        state,
        ctx.store.read(|db| objective(db, &agent, &m)),
        rewards(&agent, &m)
    );
    Value::text(page)
}

// ---- going there -----------------------------------------------------------------------------

/// WarpToLocation(locationType, locationNumber, warpRange, gang), and with
/// `approach` GotoLocation(locationType, locationNumber): to a mission's
/// place in the pilot's own system. An encounter's site opens as they set out.
pub async fn go_to(ctx: &Ctx<'_>, number: i64, range: f64, approach: bool) -> Outcome {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(system) = ctx.session.solar_system_id else { return Outcome::custom("You have to be in space for that.") };
    let mission = ctx.store.read(|db| {
        db.table("agtCharMissions")
            .get_id(number)
            .filter(|r| r.int("characterID") == Some(me) && matches!(r.int("state"), Some(2 | 3)))
            .map(|r| mission_of(&r))
    });
    let Some(m) = mission else {
        return Outcome::custom("That mission is not yours, or is over.");
    };
    let (at, radius) = match (m.kind, m.site) {
        (KIND_ENCOUNTER, Some(at)) => {
            if m.destination != system {
                return Outcome::custom("That location is in another solar system. Set your destination there first.");
            }
            if m.state == ACCEPTED {
                open_site(ctx, &m, system, at).await;
            }
            (at, 0.0)
        }
        _ => {
            let place = ctx.store.read(|db| {
                db.table("mapDenormalize").get_id(m.destination).map(|r| {
                    let at = Vec3::new(r.float("x").unwrap_or(0.0), r.float("y").unwrap_or(0.0), r.float("z").unwrap_or(0.0));
                    (at, r.float("radius").unwrap_or(0.0), r.int("solarSystemID"))
                })
            });
            let Some((at, radius, in_system)) = place else { return Outcome::custom("That station is gone.") };
            if in_system != Some(system) {
                return Outcome::custom("That location is in another solar system. Set your destination there first.");
            }
            (at, radius)
        }
    };
    if approach {
        crate::svc::space::approach_point(ctx, at, "GotoLocation")
    } else {
        crate::svc::space::warp_to_point(ctx, at, radius, range, "WarpToLocation")
    }
}

async fn open_site(ctx: &Ctx<'_>, m: &Mission, system: i64, at: Vec3) {
    let level = ctx.store.read(|db| agent(db, m.agent_id)).map_or(1, |a| a.level);
    let mut w = ctx.world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    if park.sites.values().any(|s| s.mission_id == m.id) {
        return;
    }
    let site = crate::sites::Site {
        id: crate::npc::next_id(),
        mission_id: m.id,
        char_id: ctx.session.char_id.unwrap_or(0),
        name: m.name.clone(),
        pos: at,
        rats: Vec::new(),
    };
    let entries = crate::sites::open(park, site, ENCOUNTER_SECURITY[level - 1], crate::combat::now_ms());
    crate::svc::space::broadcast(&w, system, None, entries);
}

// ---- names and amounts -----------------------------------------------------------------------

fn static_type(type_id: i64) -> Option<crate::sde::Row<'static>> {
    crate::sde::table("invTypes").get("typeID", (type_id as i32).into())
}

fn type_name(type_id: i64) -> String {
    static_type(type_id).and_then(|r| r.text("typeName")).unwrap_or_default().to_string()
}

fn type_volume(type_id: i64) -> f64 {
    static_type(type_id).and_then(|r| r.float("volume")).unwrap_or(1.0)
}

fn system_name(db: &Db, system: i64) -> String {
    db.table("mapSolarSystems").get_id(system).and_then(|r| r.text("solarSystemName")).unwrap_or_default().to_string()
}

/// A station's name and its system's.
fn station_names(db: &Db, station: i64) -> (String, String) {
    let s = db.table("staStations").get_id(station);
    let m = s.and_then(|s| db.table("mapSolarSystems").get_id(s.int("solarSystemID")?));
    match (s, m) {
        (Some(s), Some(m)) => (s.text("stationName").unwrap_or_default().to_string(), m.text("solarSystemName").unwrap_or_default().to_string()),
        _ => (String::new(), String::new()),
    }
}

fn isk(amount: f64) -> String {
    let whole = amount.round() as i64;
    let digits = whole.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if whole < 0 { format!("-{out}") } else { out }
}

fn interval(seconds: i64) -> String {
    let (h, m) = (seconds / 3_600, seconds % 3_600 / 60);
    match (h, m) {
        (0, m) => format!("{m} minutes"),
        (h, 0) => format!("{h} hour(s)"),
        (h, m) => format!("{h} hour(s) {m} minutes"),
    }
}
