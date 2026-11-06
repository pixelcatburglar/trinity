//! Skill training in real time. A pilot trains one skill at a time; it
//! moves to the in-training flag, and `skillTraining` keeps when it started,
//! the points it had then, and when its next level is due. Points accrue
//! along that line, so progress agrees with the end time the client was
//! told, and training goes on while the pilot is offline or the server is
//! down. When the end time passes the skill gains its level and leaves
//! training (`OnSkillTrained`).

use std::time::Duration;

use eve_marshal::Value;
use tracing::error;

use super::{history, own_attr, sp_for_level, ATTR_PRIMARY, ATTR_SECONDARY, ATTR_SKILL_LEVEL, ATTR_SKILL_POINTS, ATTR_SKILL_TIME_CONSTANT, CATEGORY_SKILL, FLAG_SKILL, FLAG_SKILL_IN_TRAINING};
use crate::items::ItemTxn;
use crate::services::Ctx;
use crate::store::{Db, Store, StoreError, Tx, Value as Cell};
use crate::world::World;

/// One minute as FILETIME ticks.
const MINUTE: i64 = 600_000_000;
/// How often finished skills are looked for.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// A skill's attribute: its own value, else its type's, else the default.
fn skill_attr(db: &Db, skill_id: i64, type_id: i64, attribute_id: i32) -> f64 {
    own_attr(db, skill_id, attribute_id)
        .or_else(|| crate::sde::get().type_attribute(type_id, attribute_id))
        .or_else(|| crate::sde::table("dgmAttributeTypes").get("attributeID", attribute_id as i64)?.float("defaultValue"))
        .unwrap_or(0.0)
}

/// A pilot's attribute (intelligence, memory, ...): their own value, else
/// their bloodline's.
fn character_attr(db: &Db, char_id: i64, attribute_id: i32) -> f64 {
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

/// Skill points a minute: the primary attribute and half the secondary.
fn rate(db: &Db, char_id: i64, skill_id: i64, type_id: i64) -> f64 {
    let mut spm = 0.0;
    let primary = skill_attr(db, skill_id, type_id, ATTR_PRIMARY) as i32;
    let secondary = skill_attr(db, skill_id, type_id, ATTR_SECONDARY) as i32;
    if primary != 0 {
        spm += character_attr(db, char_id, primary);
    }
    if secondary != 0 {
        spm += character_attr(db, char_id, secondary) / 2.0;
    }
    spm
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

/// A changed attribute of a pilot's skill, as the client is told of it.
pub struct Changed {
    pub char_id: i64,
    pub skill_id: i64,
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
        let span = (t.ends_at - t.started_at).max(1) as f64;
        let done = ((now - t.started_at) as f64 / span).clamp(0.0, 1.0);
        let old = skill_attr(items.db(), t.skill_id, type_id, ATTR_SKILL_POINTS);
        let new = (t.start_points + (target - t.start_points) * done).floor().max(old);
        set_attr(items.tx(), t.skill_id, ATTR_SKILL_POINTS, new as i64)?;
        history::record(items.tx(), char_id, type_id, history::TRAINING_CANCELLED, now, new)?;
        changed.push(Changed { char_id, skill_id: t.skill_id, attribute_id: ATTR_SKILL_POINTS, old, new });
        items.move_to(t.skill_id, char_id, i64::from(FLAG_SKILL))?;
    }
    items.tx().delete("skillTraining", t.row)?;
    Ok(Some(t.skill_id))
}

/// Start `char_id` training `skill_id` now: when it will be done, or None if
/// it cannot be trained (not theirs, level 5, no attributes to learn with).
fn begin(items: &mut ItemTxn<'_, '_>, char_id: i64, skill_id: i64, now: i64) -> Result<Option<i64>, StoreError> {
    let db = items.db();
    let Some(e) = db.table("entity").get_id(skill_id) else { return Ok(None) };
    let Some(type_id) = e.int("typeID") else { return Ok(None) };
    if e.int("ownerID") != Some(char_id) || crate::sde::get().group_and_category(type_id).map(|(_, c)| c) != Some(i64::from(CATEGORY_SKILL)) {
        return Ok(None);
    }
    let Some(target) = next_level_points(db, skill_id, type_id) else { return Ok(None) };
    let spm = rate(db, char_id, skill_id, type_id);
    if !(spm.is_finite() && spm > 0.0) {
        return Ok(None);
    }
    let points = skill_attr(db, skill_id, type_id, ATTR_SKILL_POINTS);
    let ends_at = now + (((target - points).max(0.0) / spm) * MINUTE as f64).ceil() as i64;
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

fn attribute_changes<'a>(changed: impl Iterator<Item = &'a Changed>, now: i64) -> Value {
    let changes = changed
        .map(|c| {
            Value::Tuple(vec![
                Value::bytes("OnModuleAttributeChange"),
                Value::Int(c.char_id),
                Value::Int(c.skill_id),
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

/// Why a start undoes itself: the skill cannot be trained, or the store
/// refused a change.
enum Undone {
    NotTrainable,
    Store(StoreError),
}

impl From<StoreError> for Undone {
    fn from(e: StoreError) -> Self {
        Undone::Store(e)
    }
}

/// skillMgr.CharStartTrainingSkill: train `skill_id`, stopping what the
/// pilot trained before (its points so far are kept).
pub async fn start(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let now = crate::bulkdata::filetime_now();
    let mut changed = Vec::new();
    let result = ctx.store.write(|tx| {
        let mut items = ItemTxn::new(tx);
        let halted = halt(&mut items, char_id, now, &mut changed)?;
        let Some(eta) = begin(&mut items, char_id, skill_id, now)? else {
            // Not trainable: what trained before stays as it was.
            return Err(Undone::NotTrainable);
        };
        Ok((items.events(), halted.filter(|h| *h != skill_id), eta))
    });
    match result {
        Ok((events, halted, eta)) => {
            let mut also = Vec::new();
            if let Some(h) = halted {
                also.push(("OnSkillTrainingStopped", Value::Tuple(vec![Value::Int(h), Value::Int(1)])));
            }
            also.push(("OnSkillStartTraining", Value::Tuple(vec![Value::Int(skill_id), Value::Long(eta)])));
            tell(ctx.world, char_id, events, &changed, now, also);
        }
        Err(Undone::NotTrainable) => {}
        Err(Undone::Store(e)) => error!("starting skill training: {}", crate::logging::chain(&e)),
    }
    Value::None
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

/// Give every skill whose training is due its level, and tell its pilot.
pub fn complete_due(store: &Store, world: &World) {
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
                    changed.push(Changed { char_id: *char_id, skill_id: t.skill_id, attribute_id: ATTR_SKILL_POINTS, old: points, new: target.ceil() });
                    changed.push(Changed { char_id: *char_id, skill_id: t.skill_id, attribute_id: ATTR_SKILL_LEVEL, old: level, new: level + 1.0 });
                }
                items.move_to(t.skill_id, *char_id, i64::from(FLAG_SKILL))?;
            }
            let events = items.events();
            tx.delete("skillTraining", t.row)?;
            done.push((*char_id, t.skill_id, events));
        }
        Ok::<_, StoreError>(done)
    });
    match result {
        Ok(done) => {
            for (char_id, skill_id, events) in done {
                tracing::info!("{char_id} trained skill {skill_id}");
                tell(world, char_id, events, &changed, now, vec![("OnSkillTrained", Value::Tuple(vec![Value::Int(skill_id)]))]);
            }
        }
        Err(e) => error!("completing skill training: {}", crate::logging::chain(&e)),
    }
}

/// Look for finished skills every second, for as long as the server runs.
pub async fn run(store: std::sync::Arc<Store>, world: World) {
    let mut every = tokio::time::interval(CHECK_EVERY);
    loop {
        every.tick().await;
        complete_due(&store, &world);
    }
}

/// At start: a skill left in training without a record (data from before
/// training was timed) trains on from now.
pub fn ensure(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    let now = crate::bulkdata::filetime_now();
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
        begin(&mut items, char_id, skill_id, now)?;
    }
    Ok(())
}
