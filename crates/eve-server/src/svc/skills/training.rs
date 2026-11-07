//! Skill training in real time. A pilot trains one skill at a time; it
//! moves to the in-training flag, and `skillTraining` keeps when it started,
//! the points it had then, and when its next level is due. Points accrue
//! along that line, so progress agrees with the end time the client was
//! told, and training goes on while the pilot is offline or the server is
//! down. When the end time passes the skill gains its level and leaves
//! training (`OnSkillTrained`).
//!
//! The rate is the client's (godma `GetSPM`): the pilot's attributes as
//! dogma computes them, so implants, boosters and learning skills count.
//! The client works out progress as `target - minutes left * rate`, so the
//! end time must follow the same rate; when the pilot's attributes change
//! mid-training the rest of the level is re-timed at the new rate.
//!
//! A skillbook trained from a hangar or cargo is first injected: one book
//! becomes the pilot's level 0 skill.

use std::collections::HashSet;
use std::time::Duration;

use eve_marshal::Value;
use tracing::error;

use super::{history, own_attr, sp_for_level, ATTR_PRIMARY, ATTR_SECONDARY, ATTR_SKILL_LEVEL, ATTR_SKILL_POINTS, ATTR_SKILL_TIME_CONSTANT, CATEGORY_SKILL, FLAG_SKILL, FLAG_SKILL_IN_TRAINING};
use crate::dogma::{Catalog, Loadout};
use crate::items::{ItemTxn, NewItem};
use crate::services::{Ctx, Outcome};
use crate::store::{Db, Store, StoreError, Tx, Value as Cell};
use crate::world::World;

/// One minute as FILETIME ticks.
const MINUTE: i64 = 600_000_000;
/// One second as FILETIME ticks.
const SECOND: i64 = 10_000_000;
/// How often finished skills are looked for.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// How many checks pass between looks at whether a pilot's rate changed.
const RETIME_EVERY: u32 = 5;

const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;

/// The pilot's attributes, as `ATTR_*` IDs (charisma .. willpower).
const CHARACTER_ATTRIBUTES: [i32; 5] = [164, 165, 166, 167, 168];

/// A skill's attribute: its own value, else its type's, else the default.
fn skill_attr(db: &Db, skill_id: i64, type_id: i64, attribute_id: i32) -> f64 {
    own_attr(db, skill_id, attribute_id)
        .or_else(|| crate::sde::get().type_attribute(type_id, attribute_id))
        .or_else(|| crate::sde::table("dgmAttributeTypes").get("attributeID", attribute_id as i64)?.float("defaultValue"))
        .unwrap_or(0.0)
}

/// A pilot's stored attribute (intelligence, memory, ...): their own value,
/// else their bloodline's. Only a fallback when dogma has none.
fn stored_character_attr(db: &Db, char_id: i64, attribute_id: i32) -> f64 {
    if let Some(v) = own_attr(db, char_id, attribute_id) {
        return v;
    }
    let column = match attribute_id {
        164 => "charisma",
        165 => "intelligence",
        166 => "memory",
        167 => "perception",
        168 => "willpower",
        _ => return 0.0,
    };
    db.table("character_")
        .get_id(char_id)
        .and_then(|c| c.int("bloodlineID"))
        .and_then(|b| crate::sde::table("chrBloodlines").get("bloodlineID", b))
        .and_then(|b| b.int(column))
        .map_or(0.0, |v| v as f64)
}

/// The pilot's attributes as the client is sent them: dogma's values for
/// the character with their skills, implants and boosters applied.
fn character_attrs(db: &Db, catalog: &Catalog, char_id: i64) -> [f64; 5] {
    let loadout = match Loadout::load(db, catalog, char_id, None, &HashSet::new()) {
        Ok(l) => Some(l),
        Err(e) => {
            error!("dogma loadout for skill training: {}", crate::logging::chain(&e));
            None
        }
    };
    CHARACTER_ATTRIBUTES.map(|a| {
        loadout
            .as_ref()
            .and_then(|l| Some(l.value(l.character()?, a)))
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or_else(|| stored_character_attr(db, char_id, a))
    })
}

/// Skill points a minute: the primary attribute and half the secondary
/// (godma `GetSPM`).
fn rate(db: &Db, catalog: &Catalog, char_id: i64, skill_id: i64, type_id: i64) -> f64 {
    let attrs = character_attrs(db, catalog, char_id);
    let of = |a: i32| CHARACTER_ATTRIBUTES.iter().position(|x| *x == a).map_or(0.0, |i| attrs[i]);
    let mut spm = 0.0;
    let primary = skill_attr(db, skill_id, type_id, ATTR_PRIMARY) as i32;
    let secondary = skill_attr(db, skill_id, type_id, ATTR_SECONDARY) as i32;
    if primary != 0 {
        spm += of(primary);
    }
    if secondary != 0 {
        spm += of(secondary) / 2.0;
    }
    spm
}

/// When the points from `points` to `target` are done at `spm` a minute,
/// starting `now`.
fn due_at(now: i64, points: f64, target: f64, spm: f64) -> i64 {
    now + (((target - points).max(0.0) / spm) * MINUTE as f64).ceil() as i64
}

/// The points the skill's next level needs; None at level 5.
fn next_level_points(db: &Db, skill_id: i64, type_id: i64) -> Option<f64> {
    let stc = skill_attr(db, skill_id, type_id, ATTR_SKILL_TIME_CONSTANT);
    sp_for_level(stc * 250.0, skill_attr(db, skill_id, type_id, ATTR_SKILL_LEVEL))
}

/// The skill `char_id` is training, with when it started, its points then,
/// and when it is due.
struct Training {
    row: crate::store::RowId,
    skill_id: i64,
    started_at: i64,
    start_points: f64,
    ends_at: i64,
}

fn training_of(db: &Db, char_id: i64) -> Option<Training> {
    let r = db.table("skillTraining").get_id(char_id)?;
    Some(Training {
        row: r.id,
        skill_id: r.int("skillID")?,
        started_at: r.int("startedAt")?,
        start_points: r.float("startPoints")?,
        ends_at: r.int("endsAt")?,
    })
}

impl Training {
    /// The points along the training line at `now`, toward `target`.
    fn points_at(&self, target: f64, now: i64) -> f64 {
        let span = (self.ends_at - self.started_at).max(1) as f64;
        let done = ((now - self.started_at) as f64 / span).clamp(0.0, 1.0);
        self.start_points + (target - self.start_points) * done
    }
}

/// When `char_id`'s training of `skill_id` ends, if they are training it.
pub fn end_of(db: &Db, char_id: i64, skill_id: i64) -> Option<i64> {
    training_of(db, char_id).filter(|t| t.skill_id == skill_id).map(|t| t.ends_at)
}

fn set_attr(tx: &mut Tx<'_>, item_id: i64, attribute_id: i32, value: i64) -> Result<(), StoreError> {
    let key = [Cell::Int(item_id), Cell::Int(i64::from(attribute_id))];
    match tx.table("entity_attributes").get(&key).map(|r| r.id) {
        Some(row) => tx.update("entity_attributes", row, &[("valueInt", Cell::Int(value)), ("valueFloat", Cell::Null)]),
        None => tx
            .insert("entity_attributes", &[("itemID", key[0].clone()), ("attributeID", key[1].clone()), ("valueInt", Cell::Int(value)), ("valueFloat", Cell::Null)])
            .map(|_| ()),
    }
}

/// A changed attribute of a pilot or their skill, as the client is told of it.
pub struct Changed {
    pub char_id: i64,
    /// The skill, or the pilot themselves.
    pub item_id: i64,
    pub attribute_id: i32,
    pub old: f64,
    pub new: f64,
}

/// Stop what `char_id` is training, crediting the points earned so far: the
/// skill stopped, if any.
fn halt(items: &mut ItemTxn<'_, '_>, char_id: i64, now: i64, changed: &mut Vec<Changed>) -> Result<Option<i64>, StoreError> {
    let Some(t) = training_of(items.db(), char_id) else {
        // A skill left in training without a record (older data) just stops.
        let left: Vec<i64> = items
            .db()
            .table("entity")
            .find(&["ownerID"], &[Cell::Int(char_id)])
            .filter(|e| e.int("flag") == Some(i64::from(FLAG_SKILL_IN_TRAINING)))
            .filter_map(|e| e.int("itemID"))
            .collect();
        for id in &left {
            items.move_to(*id, char_id, i64::from(FLAG_SKILL))?;
        }
        return Ok(left.first().copied());
    };
    if let Some(type_id) = items.db().table("entity").get_id(t.skill_id).and_then(|e| e.int("typeID")) {
        let target = next_level_points(items.db(), t.skill_id, type_id).unwrap_or(t.start_points);
        let old = skill_attr(items.db(), t.skill_id, type_id, ATTR_SKILL_POINTS);
        let new = t.points_at(target, now).floor().max(old);
        set_attr(items.tx(), t.skill_id, ATTR_SKILL_POINTS, new as i64)?;
        history::record(items.tx(), char_id, type_id, history::TRAINING_CANCELLED, now, new)?;
        changed.push(Changed { char_id, item_id: t.skill_id, attribute_id: ATTR_SKILL_POINTS, old, new });
        items.move_to(t.skill_id, char_id, i64::from(FLAG_SKILL))?;
    }
    items.tx().delete("skillTraining", t.row)?;
    Ok(Some(t.skill_id))
}

/// Start `char_id` training `skill_id` now: when it will be done, or None if
/// it cannot be trained (not one of their skills, level 5, no attributes to
/// learn with).
fn begin(items: &mut ItemTxn<'_, '_>, catalog: &Catalog, char_id: i64, skill_id: i64, now: i64) -> Result<Option<i64>, StoreError> {
    let db = items.db();
    let Some(e) = db.table("entity").get_id(skill_id) else { return Ok(None) };
    let Some(type_id) = e.int("typeID") else { return Ok(None) };
    if e.int("ownerID") != Some(char_id) || crate::sde::get().group_and_category(type_id).map(|(_, c)| c) != Some(i64::from(CATEGORY_SKILL)) {
        return Ok(None);
    }
    // Only a skill the pilot has learnt; a book still in a hangar is
    // injected first (`inject`).
    if e.int("locationID") != Some(char_id) || !matches!(e.int("flag"), Some(f) if f == i64::from(FLAG_SKILL) || f == i64::from(FLAG_SKILL_IN_TRAINING)) {
        return Ok(None);
    }
    let Some(target) = next_level_points(db, skill_id, type_id) else { return Ok(None) };
    let spm = rate(db, catalog, char_id, skill_id, type_id);
    if !(spm.is_finite() && spm > 0.0) {
        return Ok(None);
    }
    let points = skill_attr(db, skill_id, type_id, ATTR_SKILL_POINTS);
    let ends_at = due_at(now, points, target, spm);
    items.tx().insert(
        "skillTraining",
        &[
            ("characterID", Cell::Int(char_id)),
            ("skillID", Cell::Int(skill_id)),
            ("startedAt", Cell::Int(now)),
            ("startPoints", Cell::Float(points)),
            ("endsAt", Cell::Int(ends_at)),
        ],
    )?;
    items.move_to(skill_id, char_id, i64::from(FLAG_SKILL_IN_TRAINING))?;
    history::record(items.tx(), char_id, type_id, history::TRAINING_STARTED, now, points)?;
    Ok(Some(ends_at))
}

/// The skill `char_id` already knows of `type_id`, if any.
fn known(db: &Db, char_id: i64, type_id: i64) -> Option<i64> {
    db.table("entity")
        .find(&["locationID"], &[Cell::Int(char_id)])
        .filter(|e| e.int("typeID") == Some(type_id))
        .filter(|e| matches!(e.int("flag"), Some(f) if f == i64::from(FLAG_SKILL) || f == i64::from(FLAG_SKILL_IN_TRAINING)))
        .find_map(|e| e.int("itemID"))
}

/// Make one book of the stack `book_id` (in reach, in a hangar or cargo)
/// `char_id`'s level 0 skill: its item ID.
fn inject(ctx: &Ctx<'_>, items: &mut ItemTxn<'_, '_>, char_id: i64, book_id: i64) -> Result<i64, Undone> {
    let Some(book) = items.item(book_id)? else { return Err(Undone::Refused("That skillbook no longer exists.".into())) };
    if book.owner_id != char_id || book.category_id != i64::from(CATEGORY_SKILL) {
        return Err(Undone::Refused("That is not your skillbook.".into()));
    }
    if !matches!(book.flag, FLAG_HANGAR | FLAG_CARGO) || !crate::svc::moves::reachable(ctx, items, &book)? {
        return Err(Undone::Refused("The skillbook must be in your hangar or cargo hold here.".into()));
    }
    if book.quantity < 1 {
        return Err(Undone::Refused("That skillbook stack is empty.".into()));
    }
    let name = |t: i64| crate::sde::get().type_(t).map_or_else(|| t.to_string(), |t| t.name.to_string());
    if known(items.db(), char_id, book.type_id).is_some() {
        return Err(Undone::Refused(format!("You already know {}.", name(book.type_id))));
    }
    let required = crate::svc::fitting::required_skill_levels(items.db(), book.type_id)?;
    let types: Vec<i32> = required.keys().copied().collect();
    let trained = super::levels(items.db(), char_id, &types)?;
    for (skill, level) in &required {
        if trained.get(skill).copied().unwrap_or(0) < *level {
            return Err(Undone::Refused(format!("You need {} at level {level} before you can learn {}.", name(i64::from(*skill)), name(book.type_id))));
        }
    }
    let skill_id = if book.quantity > 1 {
        items.set_quantity(book_id, book.quantity - 1)?;
        items.create(NewItem { type_id: book.type_id, owner_id: char_id, location_id: char_id, flag: i64::from(FLAG_SKILL), quantity: 1, singleton: true })?
    } else {
        items.move_to(book_id, char_id, i64::from(FLAG_SKILL))?;
        items.set_singleton(book_id, true)?;
        book_id
    };
    set_attr(items.tx(), skill_id, ATTR_SKILL_LEVEL, 0)?;
    set_attr(items.tx(), skill_id, ATTR_SKILL_POINTS, 0)?;
    Ok(skill_id)
}

/// Re-time `char_id`'s training at the rate their attributes give now: the
/// points earned so far are kept and the rest of the level goes at the new
/// rate. The training and its new start points and end time, if the end
/// moved; nothing is written.
fn retimed(db: &Db, catalog: &Catalog, char_id: i64, now: i64) -> Option<(Training, f64, i64)> {
    let t = training_of(db, char_id)?;
    if t.ends_at <= now {
        return None;
    }
    let type_id = db.table("entity").get_id(t.skill_id).and_then(|e| e.int("typeID"))?;
    let target = next_level_points(db, t.skill_id, type_id)?;
    let spm = rate(db, catalog, char_id, t.skill_id, type_id);
    if !(spm.is_finite() && spm > 0.0) {
        return None;
    }
    let points = t.points_at(target, now);
    let ends_at = due_at(now, points, target, spm);
    ((ends_at - t.ends_at).abs() >= SECOND).then_some((t, points, ends_at))
}

/// Re-time `char_id`'s training (see `retimed`): the skill and its new end
/// time, if that moved.
fn retime(tx: &mut Tx<'_>, catalog: &Catalog, char_id: i64, now: i64) -> Result<Option<(i64, i64)>, StoreError> {
    let Some((t, points, ends_at)) = retimed(tx.db(), catalog, char_id, now) else { return Ok(None) };
    tx.update("skillTraining", t.row, &[("startedAt", Cell::Int(now)), ("startPoints", Cell::Float(points)), ("endsAt", Cell::Int(ends_at))])?;
    Ok(Some((t.skill_id, ends_at)))
}

/// Re-time every pilot's training whose attributes changed since it was
/// timed (implants, boosters, learning skills, clone jumps), and tell them
/// the new end time.
fn retime_all(store: &Store, world: &World, catalog: &Catalog) {
    let now = crate::bulkdata::filetime_now();
    let moved: Vec<i64> = store.read(|db| {
        let training: Vec<i64> = db.table("skillTraining").rows().filter_map(|r| r.int("characterID")).collect();
        training.into_iter().filter(|c| retimed(db, catalog, *c, now).is_some()).collect()
    });
    for char_id in moved {
        match store.write(|tx| retime(tx, catalog, char_id, now)) {
            Ok(Some((skill_id, eta))) => {
                tracing::info!("{char_id}'s training of {skill_id} re-timed for a changed rate");
                let w = world.lock();
                w.notify(&w.clients_of(char_id), "OnSkillStartTraining", Value::Tuple(vec![Value::Int(skill_id), Value::Long(eta)]));
            }
            Ok(None) => {}
            Err(e) => error!("re-timing skill training: {}", crate::logging::chain(&e)),
        }
    }
}

fn attribute_changes<'a>(changed: impl Iterator<Item = &'a Changed>, now: i64) -> Value {
    let changes = changed
        .map(|c| {
            Value::Tuple(vec![
                Value::bytes("OnModuleAttributeChange"),
                Value::Int(c.char_id),
                Value::Int(c.item_id),
                Value::Int(i64::from(c.attribute_id)),
                Value::Long(now),
                Value::Double(c.new),
                Value::Double(c.old),
            ])
        })
        .collect();
    Value::Tuple(vec![Value::List(changes)])
}

/// Tell `char_id`'s clients what changed.
fn tell(world: &World, char_id: i64, events: Vec<Value>, changed: &[Changed], now: i64, also: Vec<(&'static str, Value)>) {
    let w = world.lock();
    let clients = w.clients_of(char_id);
    for event in events {
        w.notify(&clients, "OnItemChange", event);
    }
    if changed.iter().any(|c| c.char_id == char_id) {
        w.notify(&clients, "OnModuleAttributeChanges", attribute_changes(changed.iter().filter(|c| c.char_id == char_id), now));
    }
    for (event, args) in also {
        w.notify(&clients, event, args);
    }
}

/// Why a start undoes itself: the pilot is told why, the skill cannot be
/// trained, or the store refused a change.
enum Undone {
    Refused(String),
    NotTrainable,
    Store(StoreError),
}

impl From<StoreError> for Undone {
    fn from(e: StoreError) -> Self {
        Undone::Store(e)
    }
}

/// skillMgr.CharStartTrainingSkill: train `item_id`, stopping what the
/// pilot trained before (its points so far are kept). A skillbook in a
/// hangar or cargo (the hangar's "Train" menu) is injected first.
pub async fn start(ctx: &Ctx<'_>, item_id: i64) -> Outcome {
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("Select a character first.") };
    let _inventory = crate::svc::pvptrade::inventory_guard(ctx.world, char_id).await;
    let now = crate::bulkdata::filetime_now();
    let mut changed = Vec::new();
    let result = ctx.store.write(|tx| {
        let mut items = ItemTxn::new(tx);
        let learnt = items.db().table("entity").get_id(item_id).is_some_and(|e| e.int("locationID") == Some(char_id));
        let injected = (!learnt).then(|| inject(ctx, &mut items, char_id, item_id)).transpose()?;
        let skill_id = injected.unwrap_or(item_id);
        let halted = halt(&mut items, char_id, now, &mut changed)?;
        let Some(eta) = begin(&mut items, ctx.dogma, char_id, skill_id, now)? else {
            // Not trainable: what trained before stays as it was.
            return Err(Undone::NotTrainable);
        };
        let injected_type = injected.and_then(|id| items.db().table("entity").get_id(id)).and_then(|e| e.int("typeID"));
        Ok((items.events(), skill_id, injected_type, halted.filter(|h| *h != skill_id), eta))
    });
    match result {
        Ok((events, skill_id, injected_type, halted, eta)) => {
            if let Some(type_id) = injected_type {
                let mut w = ctx.world.lock();
                for client in w.clients_of(char_id) {
                    w.skills_unseen.entry(client).or_default().insert((char_id, type_id));
                }
            }
            let mut also = Vec::new();
            if let Some(h) = halted {
                also.push(("OnSkillTrainingStopped", Value::Tuple(vec![Value::Int(h), Value::Int(1)])));
            }
            also.push(("OnSkillStartTraining", Value::Tuple(vec![Value::Int(skill_id), Value::Long(eta)])));
            tell(ctx.world, char_id, events, &changed, now, also);
            Outcome::Value(Value::None)
        }
        Err(Undone::Refused(why)) => Outcome::custom(why),
        Err(Undone::NotTrainable) => Outcome::Value(Value::None),
        Err(Undone::Store(e)) => {
            error!("starting skill training: {}", crate::logging::chain(&e));
            Outcome::custom("The skill could not be trained.")
        }
    }
}

/// skillMgr.CharStopTrainingSkill: stop, keeping the points earned so far.
pub async fn stop(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let now = crate::bulkdata::filetime_now();
    let mut changed = Vec::new();
    let result = ctx.store.write(|tx| {
        if training_of(tx.db(), char_id).is_some_and(|t| t.skill_id != skill_id) {
            return Ok(None);
        }
        let mut items = ItemTxn::new(tx);
        let halted = halt(&mut items, char_id, now, &mut changed)?;
        Ok::<_, StoreError>(halted.map(|h| (items.events(), h)))
    });
    match result {
        Ok(Some((events, halted))) => {
            let stopped = ("OnSkillTrainingStopped", Value::Tuple(vec![Value::Int(halted), Value::Int(0)]));
            tell(ctx.world, char_id, events, &changed, now, vec![stopped]);
        }
        Ok(None) => {}
        Err(e) => error!("stopping skill training: {}", crate::logging::chain(&e)),
    }
    Value::None
}

/// Give every skill whose training is due its level, and tell its pilot
/// (and, when a learning skill raised their attributes, those too).
pub fn complete_due(store: &Store, world: &World, catalog: &Catalog) {
    let now = crate::bulkdata::filetime_now();
    let due: Vec<i64> = store.read(|db| {
        db.table("skillTraining").rows().filter(|r| r.int("endsAt").is_some_and(|e| e <= now)).filter_map(|r| r.int("characterID")).collect()
    });
    if due.is_empty() {
        return;
    }
    let mut changed = Vec::new();
    let result = store.write(|tx| {
        let mut done = Vec::new();
        for char_id in &due {
            let Some(t) = training_of(tx.db(), *char_id) else { continue };
            let attrs_before = character_attrs(tx.db(), catalog, *char_id);
            let mut items = ItemTxn::new(tx);
            let skill = items.db().table("entity").get_id(t.skill_id).filter(|e| e.int("ownerID") == Some(*char_id)).and_then(|e| e.int("typeID"));
            if let Some(type_id) = skill {
                let db = items.db();
                let (level, points) = (skill_attr(db, t.skill_id, type_id, ATTR_SKILL_LEVEL), skill_attr(db, t.skill_id, type_id, ATTR_SKILL_POINTS));
                if let Some(target) = next_level_points(db, t.skill_id, type_id) {
                    let target = target.max(points);
                    set_attr(items.tx(), t.skill_id, ATTR_SKILL_POINTS, target.ceil() as i64)?;
                    set_attr(items.tx(), t.skill_id, ATTR_SKILL_LEVEL, level as i64 + 1)?;
                    history::record(items.tx(), *char_id, type_id, history::TRAINING_COMPLETE, t.ends_at, target.ceil())?;
                    changed.push(Changed { char_id: *char_id, item_id: t.skill_id, attribute_id: ATTR_SKILL_POINTS, old: points, new: target.ceil() });
                    changed.push(Changed { char_id: *char_id, item_id: t.skill_id, attribute_id: ATTR_SKILL_LEVEL, old: level, new: level + 1.0 });
                }
                items.move_to(t.skill_id, *char_id, i64::from(FLAG_SKILL))?;
            }
            let events = items.events();
            tx.delete("skillTraining", t.row)?;
            let attrs_after = character_attrs(tx.db(), catalog, *char_id);
            for ((a, old), new) in CHARACTER_ATTRIBUTES.iter().zip(attrs_before).zip(attrs_after) {
                if (new - old).abs() > 1e-9 {
                    changed.push(Changed { char_id: *char_id, item_id: *char_id, attribute_id: *a, old, new });
                }
            }
            done.push((*char_id, t.skill_id, events));
        }
        Ok::<_, StoreError>(done)
    });
    match result {
        Ok(done) => {
            for (char_id, skill_id, events) in done {
                tracing::info!("{char_id} trained skill {skill_id}");
                // OnSkillTrained renews the client's skill list.
                {
                    let mut w = world.lock();
                    for client in w.clients_of(char_id) {
                        if let Some(s) = w.skills_unseen.get_mut(&client) {
                            s.retain(|(c, _)| *c != char_id);
                        }
                    }
                }
                tell(world, char_id, events, &changed, now, vec![("OnSkillTrained", Value::Tuple(vec![Value::Int(skill_id)]))]);
            }
        }
        Err(e) => error!("completing skill training: {}", crate::logging::chain(&e)),
    }
}

/// Look for finished skills every second, and for pilots whose rate
/// changed every few, for as long as the server runs.
pub async fn run(server: std::sync::Arc<crate::conn::Server>) {
    let mut every = tokio::time::interval(CHECK_EVERY);
    let mut ticks = 0u32;
    loop {
        every.tick().await;
        complete_due(&server.store, &server.world, &server.dogma);
        ticks = ticks.wrapping_add(1);
        if ticks % RETIME_EVERY == 0 {
            retime_all(&server.store, &server.world, &server.dogma);
        }
    }
}

/// At start: mend skills and their training.
///  - A learnt skill left a stackable item (trained straight from a hangar
///    before books were injected) becomes a single skill; any extra books
///    in its stack go back to the pilot's station hangar.
///  - A skill left in training without a record (data from before training
///    was timed) trains on from now.
///  - Training timed at another rate (before the rate counted implants and
///    learning skills) is re-timed from now at the pilot's rate.
pub fn ensure(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    let now = crate::bulkdata::filetime_now();
    let catalog = Catalog::new();
    let stacked: Vec<(i64, i64, i64, i64)> = tx
        .table("entity")
        .rows()
        .filter(|e| matches!(e.int("flag"), Some(f) if f == i64::from(FLAG_SKILL) || f == i64::from(FLAG_SKILL_IN_TRAINING)))
        .filter(|e| e.int("locationID").is_some() && e.int("locationID") == e.int("ownerID"))
        .filter(|e| e.int("singleton") != Some(1))
        .filter_map(|e| Some((e.int("itemID")?, e.int("typeID")?, e.int("ownerID")?, e.int("quantity").unwrap_or(1))))
        .filter(|(_, t, _, _)| crate::sde::get().group_and_category(*t).map(|(_, c)| c) == Some(i64::from(CATEGORY_SKILL)))
        .collect();
    for (skill_id, type_id, char_id, quantity) in stacked {
        let station = tx.table("character_").get_id(char_id).and_then(|c| c.int("stationID")).filter(|s| *s > 0);
        let mut items = ItemTxn::new(tx);
        items.set_singleton(skill_id, true)?;
        if quantity > 1 {
            items.set_quantity(skill_id, 1)?;
            if let Some(station) = station {
                items.create(NewItem { type_id, owner_id: char_id, location_id: station, flag: FLAG_HANGAR, quantity: quantity - 1, singleton: false })?;
            }
        }
        tracing::info!("skill {skill_id} of {char_id} made a single item (was a stack of {quantity})");
    }
    let untimed: Vec<(i64, i64)> = tx
        .table("entity")
        .rows()
        .filter(|e| e.int("flag") == Some(i64::from(FLAG_SKILL_IN_TRAINING)))
        .filter_map(|e| Some((e.int("ownerID")?, e.int("itemID")?)))
        .filter(|(char_id, _)| tx.table("skillTraining").get_id(*char_id).is_none())
        .collect();
    for (char_id, skill_id) in untimed {
        let mut items = ItemTxn::new(tx);
        items.move_to(skill_id, char_id, i64::from(FLAG_SKILL))?;
        begin(&mut items, &catalog, char_id, skill_id, now)?;
    }
    let training: Vec<i64> = tx.table("skillTraining").rows().filter_map(|r| r.int("characterID")).collect();
    for char_id in training {
        if let Some((skill_id, eta)) = retime(tx, &catalog, char_id, now)? {
            tracing::info!("{char_id}'s training of {skill_id} re-timed to end at {eta}");
        }
    }
    Ok(())
}
