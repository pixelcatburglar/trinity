//! The web dashboard: a page, built in, and the JSON it reads, showing
//! who is playing, the wars under way, the alliances and the space they
//! hold, the busiest systems, the news from null-sec, the latest kills and
//! the killboard, the bots' market, their feelings about players, the
//! pilots being followed and politics' upheavals. `RMR_WEB` is where it
//! listens (empty: off).
//!
//! It answers GET, and closes each connection after its answer: a dashboard
//! polling every few seconds needs no more. The one thing it changes is the
//! bots (POST /api/control, with the request in `do`), and only when
//! `bots.dashboardControls` allows it.
//!
//! The page is TypeScript (web/src/app.ts), compiled to web/dist/app.js with
//! `npm run build` in crates/eve-server/web; the compiled file is part of the
//! source, so building the server needs no Node.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

use crate::conn::Server;
use crate::sde;
use crate::store::{Db, Value};

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/dist/app.js");

/// The most a request may be before it is turned away.
const MAX_REQUEST: usize = 8 * 1024;
/// FILETIME of 1970-01-01, in milliseconds since 1601.
const UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

static STARTED: OnceLock<Instant> = OnceLock::new();

pub async fn run(server: Arc<Server>, bind: String) {
    if bind.is_empty() {
        return;
    }
    STARTED.get_or_init(Instant::now);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            error!("web dashboard: cannot listen on {bind}: {e}");
            return;
        }
    };
    info!("web dashboard on http://{bind}/");
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), answer(stream, &server)).await {
                Ok(Err(e)) => debug!("web: {e}"),
                Err(_) => debug!("web: request timed out"),
                Ok(Ok(())) => {},
            }
        });
    }
}

async fn answer(mut stream: TcpStream, server: &Server) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return respond(&mut stream, 431, "text/plain", b"Request too large").await;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let path = target.split('?').next().unwrap_or("/");
    if method == "POST" && path == "/api/control" {
        let query = target.split_once('?').map_or("", |(_, q)| q);
        let asked = query.split('&').find_map(|kv| kv.strip_prefix("do=")).map(decode).unwrap_or_default();
        let (status, body) = if !crate::bots::controls_enabled() {
            (403, json!({ "error": "Dashboard controls are off (bots.dashboardControls in trinity.yml)." }))
        } else {
            let words: Vec<&str> = asked.split_whitespace().collect();
            match crate::bots::request(&words) {
                Ok(said) => (200, json!({ "ok": said })),
                Err(why) => (400, json!({ "error": why })),
            }
        };
        return respond(&mut stream, status, "application/json", body.to_string().as_bytes()).await;
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", b"Only GET").await;
    }
    match path {
        "/" | "/index.html" => respond(&mut stream, 200, "text/html; charset=utf-8", INDEX.as_bytes()).await,
        "/style.css" => respond(&mut stream, 200, "text/css; charset=utf-8", STYLE.as_bytes()).await,
        "/app.js" => respond(&mut stream, 200, "text/javascript; charset=utf-8", SCRIPT.as_bytes()).await,
        api if api.starts_with("/api/") => {
            let body = match api {
                "/api/summary" => Some(summary(server)),
                "/api/news" => Some(news(server)),
                "/api/sieges" => Some(sieges(server)),
                "/api/wars" => Some(wars(server)),
                "/api/players" => Some(players(server)),
                "/api/alliances" => Some(alliances(server)),
                "/api/corporations" => Some(corporations(server)),
                "/api/systems" => Some(systems(server)),
                "/api/map" => Some(map(server)),
                "/api/sov" => Some(sov(server)),
                "/api/kills" => Some(kills(server)),
                "/api/killboard" => Some(killboard(server)),
                "/api/market" => Some(server.world.lock().bot_panels.market.clone()),
                "/api/relations" => Some(server.world.lock().bot_panels.relations.clone()),
                "/api/followed" => Some(server.world.lock().bot_panels.followed.clone()),
                "/api/events" => Some(server.world.lock().bot_panels.events.clone()),
                "/api/pilots" => {
                    let query = target.split_once('?').map_or("", |(_, q)| q);
                    let q = query.split('&').find_map(|kv| kv.strip_prefix("q=")).map(decode).unwrap_or_default();
                    Some(pilots(server, &q))
                }
                _ => None,
            };
            match body {
                Some(v) => respond(&mut stream, 200, "application/json", v.to_string().as_bytes()).await,
                None => respond(&mut stream, 404, "application/json", br#"{"error":"no such thing"}"#).await,
            }
        }
        _ => respond(&mut stream, 404, "text/plain", b"Not found").await,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

/// A FILETIME as milliseconds since 1970, for the page's Date.
fn ms(filetime: i64) -> i64 {
    filetime / 10_000 - UNIX_EPOCH_MS
}

/// A number to one decimal as `round(x::numeric, 1)::float8` gave it: the
/// float as its 15 significant digits, rounded half away from zero.
fn round1(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let s = format!("{:.14e}", x.abs());
    let Some((mantissa, exp)) = s.split_once('e') else { return x };
    let (Ok(digits), Ok(exp)) = (mantissa.replace('.', "").parse::<u128>(), exp.parse::<i32>()) else { return x };
    // x * 10 = digits * 10^shift.
    let shift = exp - 13;
    let tenths = if shift >= 0 {
        return s.parse::<f64>().map_or(x, |v| v.copysign(x));
    } else if -shift > 30 {
        0
    } else {
        let unit = 10u128.pow((-shift) as u32);
        let (q, r) = (digits / unit, digits % unit);
        if r * 2 >= unit { q + 1 } else { q }
    };
    (tenths as f64 / 10.0).copysign(x)
}

/// Whether an owner is a bot alliance or corporation.
fn is_bots(db: &Db, owner: i64) -> bool {
    db.table("botGroups").get_id(owner).is_some()
}

/// An owner's name and ticker, as `COALESCE(alliance, corporation, '?')` and
/// `COALESCE(alliance ticker, corporation ticker, '')` of LEFT JOINs gave them.
fn side_names(db: &Db, owner: Option<i64>) -> (String, String) {
    let alliance = owner.and_then(|id| db.table("alliances").get_id(id));
    let corp = owner.and_then(|id| db.table("corporation").get_id(id));
    let name = alliance.and_then(|a| a.text("allianceName")).or_else(|| corp.and_then(|c| c.text("corporationName"))).unwrap_or("?");
    let ticker = alliance.and_then(|a| a.text("shortName")).or_else(|| corp.and_then(|c| c.text("tickerName"))).unwrap_or("");
    (name.to_string(), ticker.to_string())
}

fn summary(server: &Server) -> Json {
    let held = server.store.read(|db| db.table("mapSolarSystems").rows().filter(|s| s.int("allianceID").is_some()).count());
    let w = server.world.lock();
    let b = &w.bot_stats;
    json!({
        "players": w.players().len(),
        "bots": { "total": b.total, "online": b.online, "inSpace": b.in_space, "running": b.running },
        "corporations": b.corporations,
        "alliances": b.alliances,
        "wars": b.wars,
        "lostInFights": b.lost,
        "systemsHeld": held,
        "uptimeSeconds": STARTED.get().map_or(0, |s| s.elapsed().as_secs()),
    })
}

/// Sieges announced or under way, soonest first: where to go to see a battle.
fn sieges(server: &Server) -> Json {
    let mut list = server.world.lock().bot_stats.sieges.clone();
    list.sort_by_key(|s| s.starts);
    Json::Array(
        list.iter()
            .map(|s| {
                json!({
                    "attacker": s.attacker, "defender": s.defender, "system": s.system, "region": s.region,
                    "security": (s.security * 10.0).round() / 10.0, "starts": s.starts / 10_000 - UNIX_EPOCH_MS,
                    "fighting": s.fleets.is_some(),
                    "attackers": s.fleets.map(|f| f.0), "defenders": s.fleets.map(|f| f.1),
                })
            })
            .collect(),
    )
}

/// What the bot alliances did in null-sec lately, newest first.
fn news(server: &Server) -> Json {
    let news = server.world.lock().bot_stats.news.clone();
    Json::Array(news.iter().rev().map(|(at, what)| json!({ "time": at / 10_000 - UNIX_EPOCH_MS, "text": what })).collect())
}

/// The wars not retracted (or retracted later than now), latest declared
/// first.
fn wars(server: &Server) -> Json {
    let now = crate::bulkdata::filetime_now();
    let mut list: Vec<(i64, Json)> = server.store.read(|db| {
        let mut found: Vec<_> = db
            .table("crpWars")
            .rows()
            .filter(|w| w.int("retracted").is_none_or(|r| r > now))
            .collect();
        found.sort_by_key(|w| std::cmp::Reverse(w.int("timeDeclared")));
        found.truncate(200);
        found
            .iter()
            .map(|w| {
                let id = w.int("warID").unwrap_or(0);
                let declared_by = w.int("declaredByID");
                let (by, by_ticker) = side_names(db, declared_by);
                let (against, against_ticker) = side_names(db, w.int("againstID"));
                let item = json!({
                    "id": id,
                    "declaredBy": by,
                    "declaredByTicker": by_ticker,
                    "against": against,
                    "againstTicker": against_ticker,
                    "declared": w.int("timeDeclared").map(ms),
                    "ends": w.int("timeFinished").map(ms),
                    "mutual": w.int("mutual").map(|m| m == 1),
                    "bots": declared_by.is_some_and(|d| is_bots(db, d)),
                });
                (id, item)
            })
            .collect()
    });
    let losses = server.world.lock().bot_stats.war_losses.clone();
    for (id, item) in list.iter_mut() {
        item["losses"] = json!(losses.get(id).copied().unwrap_or(0));
    }
    Json::Array(list.into_iter().map(|(_, item)| item).collect())
}

/// A type's name, as a LEFT JOIN of invTypes gave it: null where there is
/// no type, or the data has none by that ID.
fn type_name(type_id: Option<i64>) -> Json {
    type_id.and_then(|id| sde::table("invTypes").get("typeID", id)).and_then(|t| t.text("typeName")).map_or(Json::Null, Json::from)
}

/// The people playing, not the bots: where each is and in what.
fn players(server: &Server) -> Json {
    let present = server.world.lock().players();
    let (mut list, names) = server.store.read(|db| {
        let characters = db.table("character_");
        let mut found: Vec<_> = present.iter().map(|p| p.char_id).collect::<std::collections::BTreeSet<_>>().into_iter().filter_map(|id| characters.get_id(id)).collect();
        found.sort_by(|a, b| a.text("characterName").cmp(&b.text("characterName")));
        let list: Vec<Json> = found
            .iter()
            .map(|c| {
                let corp = c.int("corporationID").and_then(|id| db.table("corporation").get_id(id));
                let alliance = corp.and_then(|co| co.int("allianceID")).and_then(|id| db.table("alliances").get_id(id));
                let ship = c.int("shipID").and_then(|id| db.table("entity").get_id(id)).and_then(|e| e.int("typeID"));
                json!({
                    "id": c.int("characterID"),
                    "name": c.text("characterName"),
                    "corporation": corp.and_then(|co| co.text("corporationName")),
                    "ticker": corp.and_then(|co| co.text("tickerName")),
                    "alliance": alliance.and_then(|a| a.text("allianceName")),
                    "security": c.float("securityRating").map(round1),
                    "ship": ship,
                })
            })
            .collect();
        let systems = db.table("mapSolarSystems");
        let names: HashMap<i64, (String, f64)> = present
            .iter()
            .filter_map(|p| systems.get_id(p.solar_system_id))
            .map(|s| (s.int("solarSystemID").unwrap_or(0), (s.text("solarSystemName").unwrap_or_default().to_string(), s.float("security").unwrap_or(0.0))))
            .collect();
        (list, names)
    });
    for item in list.iter_mut() {
        // The ship came as its type, named here from the static data.
        item["ship"] = type_name(item["ship"].as_i64());
        let id = item["id"].as_i64().unwrap_or(0);
        if let Some(p) = present.iter().find(|p| p.char_id == id) {
            let (system, security) = names.get(&p.solar_system_id).cloned().unwrap_or_default();
            item["system"] = json!(system);
            item["systemSecurity"] = json!((security * 10.0).round() / 10.0);
            item["docked"] = json!(p.docked);
        }
    }
    Json::Array(list)
}

/// The alliances standing, with their corporations and pilots, what kind
/// they are, where a bot one lives, and how many systems they hold; the
/// biggest first.
fn alliances(server: &Server) -> Json {
    server.store.read(|db| {
        let mut held: HashMap<i64, i64> = HashMap::new();
        for s in db.table("mapSolarSystems").rows() {
            if let Some(a) = s.int("allianceID") {
                *held.entry(a).or_default() += 1;
            }
        }
        let corporations = db.table("corporation");
        let mut list: Vec<(i64, Json)> = db
            .table("alliances")
            .rows()
            .filter(|a| a.int("deleted") == Some(0))
            .map(|a| {
                let id = a.int("allianceID").unwrap_or(0);
                let members: Vec<_> = corporations.find(&["allianceID"], &[Value::Int(id)]).filter(|co| co.int("deleted") == Some(0)).collect();
                let pilots: i64 = members.iter().filter_map(|co| co.int("memberCount")).sum();
                let group = db.table("botGroups").get_id(id);
                let kind = match group.and_then(|g| g.int("kind")) {
                    Some(11) => "null-sec",
                    Some(12) => "pirate",
                    Some(13) => "industrial",
                    _ => "players",
                };
                let region = group
                    .and_then(|g| g.int("regionID"))
                    .and_then(|r| sde::table("mapRegions").get("regionID", r))
                    .and_then(|r| r.text("regionName"));
                let item = json!({
                    "id": id,
                    "name": a.text("allianceName"),
                    "ticker": a.text("shortName"),
                    "corporations": members.len(),
                    "pilots": pilots,
                    "kind": kind,
                    "region": region,
                    "systems": held.get(&id).copied().unwrap_or(0),
                });
                (pilots, item)
            })
            .collect();
        list.sort_by_key(|(pilots, _)| std::cmp::Reverse(*pilots));
        list.truncate(60);
        Json::Array(list.into_iter().map(|(_, item)| item).collect())
    })
}

/// The biggest corporations of players and bots (the NPC corporations are
/// static data, and left out).
fn corporations(server: &Server) -> Json {
    let npc = sde::table("crpNPCCorporations");
    server.store.read(|db| {
        let mut found: Vec<_> = db
            .table("corporation")
            .rows()
            .filter(|co| co.int("deleted") == Some(0))
            .filter(|co| co.int("corporationID").is_none_or(|id| npc.get("corporationID", id).is_none()))
            .collect();
        found.sort_by_key(|co| std::cmp::Reverse(co.int("memberCount")));
        found.truncate(40);
        Json::Array(
            found
                .iter()
                .map(|co| {
                    let id = co.int("corporationID");
                    let alliance = co.int("allianceID").and_then(|a| db.table("alliances").get_id(a));
                    json!({
                        "id": id,
                        "name": co.text("corporationName"),
                        "ticker": co.text("tickerName"),
                        "members": co.int("memberCount"),
                        "alliance": alliance.and_then(|a| a.text("shortName")),
                        "bots": id.is_some_and(|id| is_bots(db, id)),
                    })
                })
                .collect(),
        )
    })
}

/// The busiest systems by pilots online: in space and docked, bots included.
fn systems(server: &Server) -> Json {
    let (space, docked) = server.world.lock().counts();
    let mut busiest: Vec<(i64, i64, i64)> = space.iter().map(|(s, n)| (*s, *n, docked.get(s).copied().unwrap_or(0))).collect();
    busiest.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then(a.0.cmp(&b.0)));
    busiest.truncate(30);
    let regions = sde::table("mapRegions");
    // Systems in a region the data lacks are not named, as the join with
    // the regions left them out.
    let names: HashMap<i64, (String, f64, String)> = server.store.read(|db| {
        let systems = db.table("mapSolarSystems");
        busiest
            .iter()
            .filter_map(|(id, _, _)| systems.get_id(*id))
            .filter_map(|s| {
                let region = regions.find("regionID", s.int("regionID")?).next()?;
                Some((
                    s.int("solarSystemID").unwrap_or(0),
                    (
                        s.text("solarSystemName").unwrap_or_default().to_string(),
                        s.float("security").map(round1).unwrap_or(0.0),
                        region.text("regionName").unwrap_or_default().to_string(),
                    ),
                ))
            })
            .collect()
    });
    Json::Array(
        busiest
            .iter()
            .map(|(id, in_space, in_station)| {
                let (name, security, region) = names.get(id).cloned().unwrap_or_default();
                json!({ "id": id, "name": name, "security": security, "region": region, "inSpace": in_space, "docked": in_station })
            })
            .collect(),
    )
}

/// Metres in a light year: the map's coordinates are sent in these.
const LIGHT_YEAR: f64 = 9.4607e15;

static MAP: OnceLock<Json> = OnceLock::new();

/// The star map for the sovereignty map, which never changes: regions as
/// [id, name], systems as [id, name, region, constellation, x, z, security]
/// (x and z in light years, seen from above), and each gate link once as
/// [from, to].
fn map(server: &Server) -> Json {
    if let Some(m) = MAP.get() {
        return m.clone();
    }
    // By name, unnamed regions last, as ORDER BY put them.
    let mut regions: Vec<sde::Row> = sde::table("mapRegions").rows().collect();
    regions.sort_by_key(|r| (r.text("regionName").is_none(), r.text("regionName")));
    let regions: Vec<Json> = regions
        .iter()
        .map(|r| json!([r.int("regionID"), r.text("regionName").unwrap_or_default()]))
        .collect();
    let ly = |v: Option<f64>| (v.unwrap_or(0.0) / LIGHT_YEAR * 100.0).round() / 100.0;
    let systems: Vec<Json> = server.store.read(|db| {
        db.table("mapSolarSystems")
            .rows()
            .map(|r| {
                json!([
                    r.int("solarSystemID").unwrap_or(0),
                    r.text("solarSystemName").unwrap_or_default(),
                    r.int("regionID").unwrap_or(0),
                    r.int("constellationID").unwrap_or(0),
                    ly(r.float("x")),
                    ly(r.float("z")),
                    (r.float("security").unwrap_or(0.0) * 100.0).round() / 100.0,
                ])
            })
            .collect()
    });
    let jumps: Vec<Json> = sde::table("mapSolarSystemJumps")
        .rows()
        .map(|r| (r.int("fromSolarSystemID"), r.int("toSolarSystemID")))
        .filter(|(from, to)| matches!((from, to), (Some(from), Some(to)) if from < to))
        .map(|(from, to)| json!([from, to]))
        .collect();
    let m = json!({ "regions": regions, "systems": systems, "jumps": jumps });
    MAP.get_or_init(|| m).clone()
}

/// Who holds what: [system, alliance] for every claimed system, and the
/// alliances holding any as [id, name, ticker].
fn sov(server: &Server) -> Json {
    server.store.read(|db| {
        let claimed: Vec<(i64, i64)> = db
            .table("mapSolarSystems")
            .rows()
            .filter_map(|s| Some((s.int("solarSystemID").unwrap_or(0), s.int("allianceID")?)))
            .collect();
        let holders: std::collections::HashSet<i64> = claimed.iter().map(|(_, a)| *a).collect();
        let held: Vec<Json> = claimed.iter().map(|(s, a)| json!([s, a])).collect();
        let alliances: Vec<Json> = db
            .table("alliances")
            .rows()
            .filter(|a| a.int("allianceID").is_some_and(|id| holders.contains(&id)))
            .map(|a| json!([a.int("allianceID").unwrap_or(0), a.text("allianceName").unwrap_or_default(), a.text("shortName").unwrap_or_default()]))
            .collect();
        json!({ "systems": held, "alliances": alliances })
    })
}

/// A query string's value: `+` for spaces, `%XX` for bytes.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bots whose names start with (or contain) `q`, for following one.
fn pilots(server: &Server, q: &str) -> Json {
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Json::Array(Vec::new());
    }
    let roster = std::sync::Arc::clone(&server.world.lock().bot_roster);
    let mut found: Vec<&crate::world::LocalBot> = roster.iter().filter(|b| b.name.to_lowercase().starts_with(&q)).take(15).collect();
    if found.len() < 15 {
        found.extend(roster.iter().filter(|b| !b.name.to_lowercase().starts_with(&q) && b.name.to_lowercase().contains(&q)).take(15 - found.len()));
    }
    Json::Array(found.iter().map(|b| json!({ "id": b.char_id, "name": b.name })).collect())
}

/// The last day's kills and losses by alliance (or corporation outside
/// one), bots' unseen fights included.
fn killboard(server: &Server) -> Json {
    let since = crate::bulkdata::filetime_now() - 24 * 3_600 * 10_000_000;
    // A side is an alliance, or the corporation where there is none (0).
    let side = |alliance: Option<i64>, corp: Option<i64>| alliance.filter(|a| *a != 0).or(corp);
    let (sides, total, by_type) = server.store.read(|db| {
        let recent: Vec<_> = db.table("killLog").rows().filter(|k| k.int("killTime").is_some_and(|t| t > since)).collect();
        // Each side in the order it first counts: kills, then losses.
        let mut order: Vec<Option<i64>> = Vec::new();
        let mut kills: HashMap<Option<i64>, i64> = HashMap::new();
        let participants = db.table("killLogParticipants");
        for k in &recent {
            let Some(id) = k.int("killID") else { continue };
            for p in participants.find(&["killID"], &[Value::Int(id)]).filter(|p| p.int("finalBlow") == Some(1)) {
                let s = side(p.int("allianceID"), p.int("corporationID"));
                if !order.contains(&s) {
                    order.push(s);
                }
                *kills.entry(s).or_default() += 1;
            }
        }
        let mut losses: HashMap<Option<i64>, i64> = HashMap::new();
        let mut by_type: Vec<(Option<i64>, i64)> = Vec::new();
        for k in &recent {
            let s = side(k.int("victimAllianceID"), k.int("victimCorpID"));
            if !order.contains(&s) {
                order.push(s);
            }
            *losses.entry(s).or_default() += 1;
            let ship = k.int("victimShipTypeID");
            match by_type.iter_mut().find(|(t, _)| *t == ship) {
                Some((_, n)) => *n += 1,
                None => by_type.push((ship, 1)),
            }
        }
        // A side of nobody (NULL) joins no counts.
        let count = |m: &HashMap<Option<i64>, i64>, s: Option<i64>| s.and_then(|s| m.get(&Some(s)).copied()).unwrap_or(0);
        let mut sides: Vec<(i64, i64, Json)> = order
            .into_iter()
            .map(|s| {
                let (name, ticker) = side_names(db, s);
                let (k, l) = (count(&kills, s), count(&losses, s));
                let item = json!({
                    "id": s,
                    "name": name,
                    "ticker": ticker,
                    "kills": k,
                    "losses": l,
                    "bots": s.is_some_and(|s| is_bots(db, s)),
                });
                (k, l, item)
            })
            .collect();
        sides.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        sides.truncate(30);
        (Json::Array(sides.into_iter().map(|(_, _, item)| item).collect()), recent.len(), by_type)
    });
    // The ships lost most, by name: the losses counted by type, which are
    // named from the static data (losses of a type it lacks left out, as
    // the join with invTypes left them).
    let mut lost: Vec<(Option<&str>, i64)> = Vec::new();
    for (ship, n) in by_type {
        let Some(ship) = ship.and_then(|id| sde::table("invTypes").get("typeID", id)) else { continue };
        let name = ship.text("typeName");
        match lost.iter_mut().find(|(other, _)| *other == name) {
            Some((_, count)) => *count += n,
            None => lost.push((name, n)),
        }
    }
    lost.sort_by(|a, b| b.1.cmp(&a.1));
    lost.truncate(10);
    let ships = Json::Array(lost.into_iter().map(|(ship, lost)| json!({ "ship": ship, "lost": lost })).collect());
    json!({ "total": total, "sides": sides, "ships": ships })
}

/// The latest kills.
fn kills(server: &Server) -> Json {
    let mut list: Vec<Json> = server.store.read(|db| {
        let mut found: Vec<_> = db.table("killLog").rows().collect();
        found.sort_by_key(|k| std::cmp::Reverse(k.int("killID")));
        found.truncate(30);
        let name = |id: Option<i64>| id.and_then(|id| db.table("character_").get_id(id)).and_then(|c| c.text("characterName"));
        found
            .iter()
            .map(|k| {
                json!({
                    "id": k.int("killID"),
                    "time": k.int("killTime").map(ms),
                    "victim": name(k.int("victimCharID")),
                    "victimCorporation": k.int("victimCorpID").and_then(|id| db.table("corporation").get_id(id)).and_then(|c| c.text("corporationName")),
                    "ship": k.int("victimShipTypeID"),
                    "system": k.int("solarSystemID").and_then(|id| db.table("mapSolarSystems").get_id(id)).and_then(|s| s.text("solarSystemName")),
                    "finalBlow": name(k.int("finalBlowID")),
                    "pod": k.int("isPodKill").map(|p| p == 1),
                })
            })
            .collect()
    });
    // The ship came as its type, named here from the static data.
    for item in list.iter_mut() {
        item["ship"] = type_name(item["ship"].as_i64());
    }
    Json::Array(list)
}
