//! Game master tools: `slash` (the chat window's /commands, lscchannel.py:541,
//! and the GM menu's "TR me here!"), and the dungeon tools' services, which
//! have no dungeons to work on.
//!
//! The client sends slash commands only from a session with ROLE_GML or
//! ROLE_LEGIONEER (lscchannel.py ROLE_SLASH), and the server holds to the
//! same. The commands:
//! - /create <type> [quantity]: into the hangar, or the hold in space;
//! - /giveisk <amount>;
//! - /giveskill [me|charID|\"name\"] <skill|all> [level]: trained, straight into the character;
//! - /setstanding <ownerID> <standing>: an NPC's standing toward you;
//! - /spawn: a group of the system's rats, around your ship;
//! - /tr me <station | system | celestial | character>;
//! - /bots on|off|status: the bot population; /bots event <betrayal |
//!   merger | collapse | coalition | raid>, /bots war <a> | <b>, /bots
//!   density <region> <factor>, /bots follow <name>: steering it;
//! - /help.
//! A type or a skill is its ID or its name.

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::Vec3;
use crate::items::{ItemTxn, NewItem};
use crate::sde;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{StoreError, Value as V};
use crate::svc::wallet::{self, Entry};
use crate::world::Move;

const ROLE_GML: i64 = 8;
const ROLE_LEGIONEER: i64 = 262_144;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;
const FLAG_SKILL: i64 = 7;
const FLAG_SKILL_IN_TRAINING: i64 = 61;
const CATEGORY_SKILL: i32 = 16;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;
/// refAgentMiscellaneous, for money a GM conjures.
const REF_GM: i32 = 25;
const OWNER_SYSTEM: i64 = 1;

/// Why a command's store write was refused: the store's error, or the
/// command's own reason, said to the GM as it is.
struct Refused(String);

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Self {
        Refused(e.to_string())
    }
}

/// Every command [`slash`] answers, for the client's autocompletion.
const COMMANDS: &[&str] = &["bots", "create", "createitem", "giveisk", "givelp", "givemoney", "giveskill", "help", "setstanding", "spawn", "tr"];

const HELP: &str = "/create <type> [qty], /giveisk <amount>, /givelp <amount> [corpID],/giveskill <skill> [level], /setstanding <ownerID> <standing>, /spawn, /tr me <place or pilot>, /bots on|off|status|event <kind>|war <a> | <b>|density <region> <x>|follow <name>";

pub async fn slash(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "SlashCmd" {
        return None;
    }
    if ctx.session.role & (ROLE_GML | ROLE_LEGIONEER) == 0 {
        return Some(Outcome::custom("You are not allowed to use slash commands."));
    }
    let line = call.arg_text(0).unwrap_or_default();
    let mut words = line.trim().trim_start_matches('/').split_whitespace();
    let command = words.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = words.collect();
    let me = ctx.session.char_id.unwrap_or(0);
    let result = match command.as_str() {
        // The chat window's slash service first asks with a bare "/" for the
        // command list, and reads it only from a SlashError's reason, as
        // "...: ['name', ...]" (svc_slash.py InitStuff). Any other answer
        // fails it, and the command typed is never sent.
        "" => {
            let names = COMMANDS.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
            let reason = format!("Commands: [{names}]");
            return Some(Outcome::UserError { msg: "SlashError".into(), args: vec![("reason".into(), Value::bytes(reason))] });
        }
        "help" => Ok(HELP.to_string()),
        "create" | "createitem" => create(ctx, &rest).await,
        "giveisk" | "givemoney" => give_isk(ctx, &rest).await,
        "givelp" => give_lp(ctx, &rest).await,
        "giveskill" => give_skill(ctx, &rest).await,
        "setstanding" => set_standing(ctx, &rest).await,
        "spawn" => spawn(ctx),
        "tr" => teleport(ctx, &rest).await,
        "bots" => match rest.first().copied() {
            Some("on") => {
                crate::bots::set_running(true);
                Ok("The bots are running.".into())
            }
            Some("off") => {
                crate::bots::set_running(false);
                Ok("The bots are paused.".into())
            }
            Some("status") | None => Ok(crate::bots::status()),
            Some(_) => crate::bots::request(&rest),
        },
        other => Err(format!("Unknown command /{other}. {HELP}")),
    };
    info!("{me} /{command} {}: {}", rest.join(" "), match &result { Ok(r) => r.as_str(), Err(e) => e.as_str() });
    Some(match result {
        Ok(reply) => {
            // The chat window only logs a command's result; show it on screen.
            let notify = Value::Dict(vec![(Value::bytes("notify"), Value::text(reply.clone()))]);
            let w = ctx.world.lock();
            w.notify(&[ctx.client_id], "OnRemoteMessage", Value::Tuple(vec![Value::bytes("CustomNotify"), notify]));
            Outcome::Value(Value::text(reply))
        }
        Err(why) => Outcome::custom(why),
    })
}

/// A type by ID, or by its name (exactly, else the first that starts so):
/// the ID first, then an exact name (case aside), then the shortest name.
/// Only types whose group is known count, and only types in `category` if
/// one is given.
fn find_type(words: &[&str], category: Option<i32>) -> Option<(i64, String)> {
    let key = words.join(" ");
    // The database lowered ASCII letters only (its locale is C), and the
    // key's own % and _ were wildcards in its LIKE.
    let lower = key.to_ascii_lowercase();
    let prefix = format!("{lower}%");
    let groups = sde::table("invGroups");
    sde::table("invTypes")
        .rows()
        .filter_map(|t| {
            let id = t.int("typeID")?;
            let group = groups.get("groupID", t.int("groupID")?)?;
            if category.is_some_and(|c| group.int("categoryID") != Some(c as i64)) {
                return None;
            }
            let name = t.text("typeName");
            let by_id = id.to_string() == key;
            let exact = name.map(|n| n.to_ascii_lowercase() == lower);
            if !(by_id || exact == Some(true) || name.is_some_and(|n| like(&n.to_ascii_lowercase(), &prefix))) {
                return None;
            }
            // ORDER BY ... DESC puts NULL (no name) first.
            let exact_rank = match exact {
                None => 0,
                Some(true) => 1,
                Some(false) => 2,
            };
            let length = name.map(|n| n.chars().count());
            Some(((!by_id, exact_rank, length.is_none(), length.unwrap_or(0)), id, name))
        })
        .min_by_key(|(rank, _, _)| *rank)
        .map(|(_, id, name)| (id, name.unwrap_or_default().to_string()))
}

/// SQL's LIKE: `%` matches any run of characters, `_` any one, and `\`
/// takes the next character as it is.
fn like(text: &str, pattern: &str) -> bool {
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => match chars.next() {
                Some(c) => Token::Char(c),
                // The database refuses a pattern ending in its escape.
                None => return false,
            },
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.chars().collect();
    // Which lengths of the text the pattern so far can match.
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    for token in tokens {
        let mut next = vec![false; text.len() + 1];
        match token {
            Token::Any => {
                let mut reached = false;
                for (n, m) in matched.iter().enumerate() {
                    reached |= *m;
                    next[n] = reached;
                }
            }
            Token::One => (0..text.len()).for_each(|n| next[n + 1] = matched[n]),
            Token::Char(c) => (0..text.len()).for_each(|n| next[n + 1] = matched[n] && text[n] == c),
        }
        matched = next;
    }
    matched[text.len()]
}

/// The trailing number of a command, if there is one, and the words before.
fn trailing_number<'a>(words: &'a [&'a str]) -> (&'a [&'a str], Option<f64>) {
    match words.split_last() {
        Some((last, before)) if !before.is_empty() => match last.replace(',', "").parse::<f64>() {
            Ok(n) => (before, Some(n)),
            Err(_) => (words, None),
        },
        _ => (words, None),
    }
}

async fn create(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (name, quantity) = trailing_number(words);
    let quantity = quantity.unwrap_or(1.0).max(1.0) as i64;
    let (type_id, type_name) = find_type(name, None).ok_or("No such type.")?;
    let (location, flag) = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => (station, FLAG_HANGAR),
        (None, Some(ship)) => (ship, FLAG_CARGO),
        _ => return Err("You have nowhere to put it.".into()),
    };
    let events = ctx
        .store
        .write(|tx| {
            let mut items = ItemTxn::new(tx);
            items.create(NewItem { type_id, owner_id: me, location_id: location, flag, quantity, singleton: false })?;
            Ok::<_, StoreError>(items.events())
        })
        .map_err(|e| e.to_string())?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    Ok(format!("Created {quantity} x {type_name}"))
}

async fn give_isk(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let amount: f64 = words.first().and_then(|w| w.replace(',', "").parse().ok()).ok_or("How much?")?;
    let entry = Entry { ref_type: REF_GM, payer: OWNER_SYSTEM, payee: me, arg_id1: "", reason: "GM" };
    let balance = ctx
        .store
        .write(|tx| wallet::post(tx, me, amount, &entry).map_err(|_| Refused("The wallet would go below zero.".into())))
        .map_err(|r| r.0)?;
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
    w.notify(&w.clients_of(me), "OnAccountChange", cash);
    Ok(format!("Your balance is now {balance:.2} ISK"))
}

/// `/givelp <amount> [corporationID]`: loyalty points with a corporation, by
/// default the owner of the station you are docked in.
async fn give_lp(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    let amount: i64 = words.first().and_then(|w| w.replace(',', "").parse().ok()).ok_or("How many?")?;
    let corp = match words.get(1) {
        Some(w) => w.parse::<i32>().map_err(|_| "Which corporation (an ID)?")?,
        None => {
            let station = ctx.session.station_id.ok_or("Dock, or name a corporation ID.")?;
            let owner = ctx.store.read(|db| db.table("staStations").get_id(station).and_then(|r| r.int("corporationID")));
            owner.ok_or("This station has no owner.")? as i32
        }
    };
    let total = ctx.store.write(|tx| crate::svc::lp::add_lp(tx, me, corp, amount).map_err(Refused)).map_err(|r| r.0)?;
    Ok(format!("You now have {total} LP with corporation {corp}."))
}

/// `/giveskill [who] <skill|all> [level]`, as the client's GM tools send it:
/// the info window's "give required skills" (infosvc.py DoGiveSkills,
/// `/giveskill me <typeID> <level>`), `/super` (`/giveskill me all 5`) and
/// the trainer (`/giveskill <charID> ...`). Who is `me` (the default), a
/// character ID or a quoted name; the skill a type ID or name, or `all`
/// published skills; the level 5 unless given. A skill is only ever raised,
/// so giving a ship's requirements never lowers what is already trained.
/// The skill list shows new skills after the next login.
async fn give_skill(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let (target, target_name, words) = skill_target(ctx, words)?;
    let (name, level) = trailing_number(words);
    if name.is_empty() {
        return Err("Which skill? /giveskill [me|characterID|\"name\"] <skill|all> [level]".into());
    }
    let level = level.unwrap_or(5.0).clamp(0.0, 5.0) as i64;
    let types: Vec<(i64, String)> = if matches!(name, [all] if all.eq_ignore_ascii_case("all")) {
        let sde = sde::get();
        sde::table("invTypes")
            .rows()
            .filter(|t| t.bool("published") == Some(true))
            .filter_map(|t| Some((t.int("typeID")?, t.text("typeName").unwrap_or("").to_string())))
            .filter(|(id, _)| sde.group_and_category(*id).map(|(_, c)| c) == Some(i64::from(CATEGORY_SKILL)))
            .collect()
    } else {
        vec![find_type(name, Some(CATEGORY_SKILL)).ok_or("No such skill.")?]
    };
    let now = crate::bulkdata::filetime_now();
    let (events, raised) = ctx
        .store
        .write(|tx| {
            let mut items = ItemTxn::new(tx);
            let mut raised = 0;
            for (type_id, _) in &types {
                raised += usize::from(raise_skill(&mut items, target, *type_id, level, now)?);
            }
            Ok::<_, StoreError>((items.events(), raised))
        })
        .map_err(|e| e.to_string())?;
    {
        let w = ctx.world.lock();
        let clients = w.clients_of(target);
        for event in events {
            w.notify(&clients, "OnItemChange", event);
        }
    }
    let whose = if target == ctx.session.char_id.unwrap_or(0) { String::new() } else { format!("{target_name}'s ") };
    Ok(match types.as_slice() {
        [(_, skill)] if raised == 0 => format!("{whose}{skill} is already at level {level} or above"),
        [(_, skill)] => format!("{whose}{skill} is now at level {level}; log in again to see it in the skill list"),
        _ => format!("{raised} of {whose}{} skills raised to level {level}; log in again to see them in the skill list", types.len()),
    })
}

/// Who a `/giveskill` is for, their name, and the words after them: `me`, a
/// character's ID or a quoted name first, else the caller.
fn skill_target<'a>(ctx: &Ctx<'_>, words: &'a [&'a str]) -> Result<(i64, String, &'a [&'a str]), String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let name_of = |id: i64| ctx.store.read(|db| db.table("character_").get_id(id).and_then(|c| c.text("characterName").map(str::to_string)));
    let Some(first) = words.first() else { return Err("Usage: /giveskill [me|characterID|\"name\"] <skill|all> [level]".into()) };
    if first.eq_ignore_ascii_case("me") {
        return Ok((me, String::new(), &words[1..]));
    }
    if first.starts_with('"') {
        // The name runs to the word that closes its quote.
        let close = words
            .iter()
            .enumerate()
            .position(|(i, w)| if i == 0 { w.len() > 1 && w.ends_with('"') } else { w.ends_with('"') })
            .ok_or("Close the pilot's name with a quote.")?;
        let wanted = words[..=close].join(" ").trim_matches('"').to_ascii_lowercase();
        let found = ctx.store.read(|db| {
            db.table("character_").rows().find(|c| c.text("characterName").is_some_and(|n| n.to_ascii_lowercase() == wanted)).and_then(|c| Some((c.int("characterID")?, c.text("characterName")?.to_string())))
        });
        let (id, name) = found.ok_or("No such pilot.")?;
        return Ok((id, name, &words[close + 1..]));
    }
    // A number that names a character, with a skill after it; else the
    // number is the skill (`/giveskill 3300 5`).
    if words.len() > 1 {
        if let Some((id, name)) = first.parse::<i64>().ok().and_then(|id| Some((id, name_of(id)?))) {
            return Ok((id, name, &words[1..]));
        }
    }
    Ok((me, String::new(), words))
}

/// Raise `char_id`'s skill of `type_id` to `level` (made if they lack it),
/// with the points that level needs: false if it was already there.
fn raise_skill(items: &mut ItemTxn<'_, '_>, char_id: i64, type_id: i64, level: i64, now: i64) -> Result<bool, StoreError> {
    let existing = items
        .db()
        .table("entity")
        .find(&["locationID"], &[V::Int(char_id)])
        .find(|r| r.int("typeID") == Some(type_id) && matches!(r.int("flag"), Some(FLAG_SKILL | FLAG_SKILL_IN_TRAINING)))
        .and_then(|r| r.int("itemID"));
    let attr = |items: &ItemTxn<'_, '_>, id: i64, a: i32| {
        items.db().table("entity_attributes").get(&[V::Int(id), V::Int(i64::from(a))]).and_then(|r| r.float("valueFloat").or_else(|| r.int("valueInt").map(|v| v as f64)))
    };
    let (have_level, have_points) = existing.map_or((None, 0.0), |id| (attr(items, id, ATTR_SKILL_LEVEL), attr(items, id, ATTR_SKILL_POINTS).unwrap_or(0.0)));
    if have_level.is_some_and(|l| l.round() as i64 >= level) {
        return Ok(false);
    }
    let id = match existing {
        Some(id) => id,
        None => items.create(NewItem { type_id, owner_id: char_id, location_id: char_id, flag: FLAG_SKILL, quantity: 1, singleton: true })?,
    };
    let stc = sde::get().type_attribute(type_id, ATTR_SKILL_TIME_CONSTANT).unwrap_or(1.0) * 250.0;
    let points = crate::losses::points_at(stc, level).round().max(have_points) as i64;
    for (attr, value) in [(ATTR_SKILL_LEVEL, level), (ATTR_SKILL_POINTS, points)] {
        let sets = [("itemID", V::Int(id)), ("attributeID", V::Int(attr as i64)), ("valueInt", V::Int(value)), ("valueFloat", V::Null)];
        items.tx().upsert("entity_attributes", &["itemID", "attributeID"], &sets, &[("valueInt", V::Int(value)), ("valueFloat", V::Null)])?;
    }
    crate::svc::skills::history::record(items.tx(), char_id, type_id, crate::svc::skills::history::GM_GAVE_SKILL, now, points as f64)?;
    Ok(true)
}

async fn set_standing(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let from: i64 = words.first().and_then(|w| w.parse().ok()).ok_or("Whose standing?")?;
    let standing: f64 = words.get(1).and_then(|w| w.parse().ok()).ok_or("What standing?")?;
    let standing = standing.clamp(-10.0, 10.0);
    let sets = [("characterID", V::Int(me)), ("fromID", V::Int(from)), ("standing", V::Float(standing))];
    let saved = ctx.store.write(|tx| tx.upsert("chrNPCStandings", &["characterID", "fromID"], &sets, &[("standing", V::Float(standing))]));
    if saved.is_err() {
        return Err("The standing could not be saved.".into());
    }
    let w = ctx.world.lock();
    w.notify(&w.clients_of(me), "OnStandingSet", Value::Tuple(vec![Value::Int(from), Value::Int(me), Value::Double(standing)]));
    Ok(format!("{from}'s standing toward you is now {standing:.2}"))
}

fn spawn(ctx: &Ctx<'_>) -> Result<String, String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Err("You have to be in space.".into());
    };
    let mut w = ctx.world.lock();
    let park = w.parks.get_mut(&system).ok_or("There is no park here.")?;
    let at = park.ships.get(&ship_id).map(|s| s.pos).ok_or("Your ship is not here.")?;
    let security = park.wildlife.security;
    let rats = crate::npc::spawn_group(park, ship_id, at + Vec3::new(15_000.0, 0.0, 15_000.0), security, crate::combat::now_ms());
    let entries: Vec<Value> = rats.iter().flat_map(|id| crate::svc::space::add_ball_entries(park, *id)).collect();
    crate::svc::space::broadcast(&w, system, None, entries);
    Ok(format!("{} rat(s) spawned", rats.len()))
}

/// /tr me <where>: a station docks you there; a system, a celestial in one or
/// another pilot's ship puts you in space beside it (from space only).
async fn teleport(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let target = match words {
        ["me", rest @ ..] if !rest.is_empty() => rest.join(" "),
        _ => return Err("Usage: /tr me <station, system, celestial or pilot>".into()),
    };
    // An ID as its text, or a name as the database lowered it (ASCII
    // letters only: its locale is C).
    let by_id = |id: Option<i64>| id.is_some_and(|id| id.to_string() == target);
    let lower = target.to_ascii_lowercase();
    let by_name = |name: Option<&str>| name.is_some_and(|n| n.to_ascii_lowercase() == lower);
    let me_in_space = ctx.session.solar_system_id.is_some();
    // The celestial (or station, or system) by that ID, else the first by
    // that name, in a system that exists.
    let place = ctx.store.read(|db| {
        let systems = db.table("mapSolarSystems");
        let in_a_system = |d: &crate::store::Row| {
            let system = d.int("solarSystemID").or(d.int("itemID"))?;
            systems.get_id(system).map(|s| (system, s.int("constellationID"), s.int("regionID")))
        };
        let candidates = db.table("mapDenormalize").rows().filter(|d| by_id(d.int("itemID")) || by_name(d.text("itemName")));
        let mut first = None;
        for d in candidates {
            let Some(system) = in_a_system(&d) else { continue };
            let found = (d.int("itemID"), d.int("groupID"), system, (d.float("x"), d.float("y"), d.float("z")), d.float("radius"), d.text("itemName").map(str::to_string));
            if by_id(d.int("itemID")) {
                first = Some(found);
                break;
            }
            first.get_or_insert(found);
        }
        first
    });
    let pilot = || ctx.store.read(|db| db.table("character_").rows().find(|c| by_id(c.int("characterID")) || by_name(c.text("characterName"))).and_then(|c| c.int("characterID")));
    let (station, system, constellation, region, pos, name) = if let Some((item, group, (system, constellation, region), (x, y, z), radius, name)) = place {
        let item = item.unwrap_or(0);
        let at = Vec3::new(x.unwrap_or(0.0), y.unwrap_or(0.0), z.unwrap_or(0.0));
        let radius = radius.unwrap_or(0.0);
        let station = (group == Some(15)).then_some(item);
        // Off a celestial's surface; to a system's sun, an AU out.
        let pos = if item == system { Vec3::new(1.5e11, 0.0, 0.0) } else { at + Vec3::new(radius + 10_000.0, 0.0, 0.0) };
        (station, system, constellation.unwrap_or(0), region.unwrap_or(0), pos, name.unwrap_or_default())
    } else if let Some(other) = pilot() {
        let presence = ctx.world.lock().presence_of(other).ok_or("That pilot is not online.")?;
        let r = ctx
            .store
            .read(|db| db.table("mapSolarSystems").get_id(presence.solar_system_id).map(|s| (s.int("constellationID"), s.int("regionID"))))
            .ok_or("That pilot is nowhere.")?;
        let pos = {
            let w = ctx.world.lock();
            w.parks
                .get(&presence.solar_system_id)
                .and_then(|p| p.ships.values().find(|s| s.pilot.char_id == other))
                .map_or(Vec3::new(1.5e11, 0.0, 0.0), |s| s.pos + Vec3::new(2_000.0, 0.0, 0.0))
        };
        let station = presence.station_id;
        (station, presence.solar_system_id, r.0.unwrap_or(0), r.1.unwrap_or(0), pos, target.clone())
    } else {
        return Err(format!("Nowhere called {target}."));
    };
    if station.is_none() && !me_in_space {
        return Err("Undock first: from a station, /tr only goes to other stations.".into());
    }
    let w = ctx.world.lock();
    w.push_move(ctx.client_id, Move::Teleport { station_id: station, solar_system_id: system, constellation_id: constellation, region_id: region, pos: (pos.x, pos.y, pos.z) });
    Ok(format!("On your way to {name}"))
}

/// `keeper` and `dungeon`: the dungeon editor (there is no dungeon data to
/// edit) and acceleration gates, which fling a ship on to the next room of
/// a deadspace complex (complexes).
pub async fn dungeon_tools(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(match call.method.as_str() {
        "GetLevelEditor" => Outcome::Value(Value::None),
        "ActivateAccelerationGate" => activate_gate(ctx, call.arg_id(0)),
        "DEGetDungeons" | "DEGetFactions" | "DEGetRooms" => Outcome::Value(Value::List(Vec::new())),
        "EditObject" | "CopyObject" | "RemoveObject" => Outcome::custom("There are no dungeons to edit."),
        _ => return None,
    })
}

fn activate_gate(ctx: &Ctx<'_>, gate: i64) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Outcome::custom("You are not in space.") };
    let mut w = ctx.world.lock();
    let landing = match w.parks.get(&system) {
        Some(park) => crate::complexes::gate_landing(park, ship_id, gate),
        None => Err("You are not in space.".into()),
    };
    match landing {
        Ok(at) => {
            let tick = w.parks.get(&system).map_or(0, |p| p.tick);
            crate::svc::space::broadcast(&w, system, None, vec![crate::svc::space::special_fx(tick, gate, Some(ship_id), "effects.WarpGateEffect")]);
            crate::svc::space::order(&mut w, system, ship_id, crate::ballpark::Command::Warp { target: at, range: 0.0 });
            info!("ship {ship_id} through acceleration gate {gate}");
            Outcome::Value(Value::None)
        }
        Err(why) => Outcome::custom(why),
    }
}

/// `benchmark`: SaveSession(session, dbconn), a client's performance dump
/// (benchmarkSvc.py), noted and dropped.
pub async fn benchmark(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "SaveSession" {
        return None;
    }
    info!("benchmark session from {:?} not stored", ctx.session.char_id);
    Some(Outcome::Value(Value::None))
}
