use std::collections::{BTreeMap, HashSet};

use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{Db, Row, RowId};
use crate::svc::rows::{index_rowset, row};

const SERVICE: &str = "dogmaIM";

const EFFECT_ONLINE: i64 = 16;
const ATTR_IS_ONLINE: i32 = 2;
const ATTR_QUANTITY: i32 = 805;

const CHARGED: [(i32, i32, i32); 2] = [(18, 55, 482), (264, 479, 263)];

const ALLOWED_CATEGORIES: [i32; 7] = [11, 23, 6, 7, 20, 16, 8];
const GROUP_CHARACTER: i32 = 1;

pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(ctx, &call.method, &call.args).await)
}

async fn method_call(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    match method {
        "GetAttributeTypes" => Outcome::Value(attribute_types()),

        "GetSkillPrerequisites" => Outcome::Value(Value::List(Vec::new())),
        "GetRequiredSkillLevels" => match ctx.store.read(|db| crate::svc::fitting::required_skill_levels(db,
            args.first().and_then(Value::as_i64).unwrap_or(0))) {
            Ok(levels) => Outcome::Value(Value::Dict(levels.into_iter()
                .map(|(skill, level)| (Value::Int(i64::from(skill)), Value::Int(level))).collect())),
            Err(error) => {
                error!("reading required skills: {}", crate::logging::chain(&error));
                Outcome::custom("The required skills could not be read.")
            }
        },

        "ShipGetInfo" => Outcome::Value(ship_info(ctx).await),
        "CharGetInfo" => Outcome::Value(char_info(ctx).await),
        "ItemGetInfo" => Outcome::Value(item_info(ctx, args.first()).await),
        "ChangeDroneSettings" => crate::drones::change_settings(ctx, args.first()).await,

        "CheckSendLocationInfo" => Outcome::Value(Value::None),

        // Targeting and module activation are `inflight`'s.
        "LogAttribute" => Outcome::Value(Value::List(Vec::new())),

        _ => Outcome::Unknown(format!("{SERVICE}::{method}")),
    }
}

fn attribute_types() -> Value {
    const HEADER: &[&str] = &[
        "attributeID", "attributeName", "attributeCategory", "description", "maxAttributeID",
        "attributeIdx", "graphicID", "chargeRechargeTimeID", "defaultValue", "published",
        "displayName", "unitID", "stackable", "highIsGood",
    ];
    let mut rows: Vec<_> = crate::sde::table("dgmAttributeTypes").rows().collect();
    rows.sort_by_key(|r| r.int("attributeID"));
    index_rowset(
        HEADER,
        "attributeName",
        rows.iter()
            .map(|r| {
                let l = r.line(HEADER);
                (l.get(1).cloned().unwrap_or(Value::None), Value::List(l))
            })
            .collect(),
    )
}

const INVITEM_HEADER: &[&str] = &[
    "itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton", "quantity",
    "groupID", "categoryID", "customInfo",
];

const INFO_HEADER: &[&str] = &["itemID", "invItem", "attributes", "activeEffects", "time"];

/// Whether an item is shown: anything but an implanted booster's slot
/// (flag 88), and there only a booster still running in its own pilot.
fn shown(db: &Db, e: &Row, now: i64) -> bool {
    if e.int("flag") != Some(88) {
        return true;
    }
    db.table("character_boosters").get_id(e.int("itemID").unwrap_or(0)).is_some_and(|b| {
        b.int("character") == e.int("ownerID")
            && b.int("character") == e.int("locationID")
            && b.int("type_id") == e.int("typeID")
            && e.int("quantity") == Some(1)
            && e.int("singleton") == Some(1)
            && b.int("expires").is_some_and(|expires| expires > now)
    })
}

struct Item {
    item_id: i64,
    type_id: i64,
    owner_id: i64,
    location_id: i64,
    group_id: i32,
    category_id: i32,
    booster_expiry: Option<i64>,
    line: Vec<Value>,
}

/// Stacked ammunition can be addressed by its ship, slot and type instead of
/// the inventory entity ID (godma.py::IsSubLocation/UpdateItem).
#[derive(Debug, PartialEq, Eq)]
enum ItemKey {
    Entity(i32),
    Sublocation { location: i32, flag: i32, type_id: i32 },
}

impl ItemKey {
    fn parse(value: &Value) -> Option<Self> {
        let positive = |v: &Value| i32::try_from(v.as_i64()?).ok().filter(|id| *id > 0);
        if let Some(id) = positive(value) {
            return Some(Self::Entity(id));
        }
        let [location, flag, type_id] = value.seq()? else { return None };
        let flag = positive(flag)?;
        if !crate::svc::fitting::is_slot(i64::from(flag)) {
            return None;
        }
        Some(Self::Sublocation { location: positive(location)?, flag, type_id: positive(type_id)? })
    }

    fn value(&self) -> Value {
        match *self {
            Self::Entity(id) => Value::Int(i64::from(id)),
            Self::Sublocation { location, flag, type_id } => Value::Tuple(
                [location, flag, type_id].into_iter().map(|v| Value::Int(i64::from(v))).collect(),
            ),
        }
    }
}

async fn ship_info(ctx: &Ctx<'_>) -> Value {
    let (Some(ship_id), Some(char_id)) = (ctx.session.ship_id, ctx.session.char_id) else {
        return Value::Dict(Vec::new());
    };
    // The ship, its pilot, and what is fitted to it (slots and rigs).
    info_dict(ctx, |db| {
        let entity = db.table("entity");
        let mut ids: Vec<RowId> = [ship_id, char_id].iter().filter_map(|id| entity.get_id(*id)).map(|r| r.id).collect();
        ids.extend(
            entity
                .find(&["locationID"], &[crate::store::Value::Int(ship_id)])
                .filter(|e| e.int("flag").is_some_and(|f| (11..=35).contains(&f) || (92..=99).contains(&f)))
                .map(|r| r.id),
        );
        ids
    })
    .await
}

async fn char_info(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::Dict(Vec::new()) };
    // The character, its skills, implants and boosters.
    info_dict(ctx, |db| {
        let entity = db.table("entity");
        let mut ids: Vec<RowId> = entity.get_id(char_id).map(|r| r.id).into_iter().collect();
        ids.extend(
            entity
                .find(&["locationID"], &[crate::store::Value::Int(char_id)])
                .filter(|e| matches!(e.int("flag"), Some(7 | 61 | 88 | 89)))
                .map(|r| r.id),
        );
        ids
    })
    .await
}

async fn item_info(ctx: &Ctx<'_>, key: Option<&Value>) -> Value {
    let Some(key) = key.and_then(ItemKey::parse) else { return Value::None };
    let items = match key {
        ItemKey::Entity(id) => load_items(ctx, |db| {
            let now = crate::bulkdata::filetime_now();
            db.table("entity").get_id(i64::from(id)).filter(|e| shown(db, e, now)).map(|r| r.id).into_iter().collect()
        }),
        // Only charges (category 8) are addressed this way.
        ItemKey::Sublocation { type_id, .. }
            if crate::sde::get().group_and_category(i64::from(type_id)).is_none_or(|(_, category)| category != 8) =>
        {
            return Value::None
        }
        // The first stack of that charge in that slot.
        ItemKey::Sublocation { location, flag, type_id } => load_items(ctx, |db| {
            let now = crate::bulkdata::filetime_now();
            let v = |n: i32| crate::store::Value::Int(i64::from(n));
            db.table("entity")
                .find(&["locationID", "flag"], &[v(location), v(flag)])
                .filter(|e| e.int("typeID") == Some(i64::from(type_id)) && e.int("singleton") == Some(0) && e.int("quantity").is_some_and(|q| q > 0))
                .filter(|e| shown(db, e, now))
                .min_by_key(|e| e.int("itemID"))
                .map(|e| e.id)
                .into_iter()
                .collect()
        }),
    };
    let Some(item) = items.into_iter().next() else { return Value::None };
    let attrs = computed_attributes(ctx, &[item.item_id]).await;
    let now = crate::bulkdata::filetime_now();
    let running = running_modules(ctx);
    info_row_for_key(&item, &key, &attrs, now, running.iter().find(|module| module.module_id == item.item_id))
}

async fn info_dict(ctx: &Ctx<'_>, pick: impl FnOnce(&Db) -> Vec<RowId>) -> Value {
    let items = load_items(ctx, |db| {
        let now = crate::bulkdata::filetime_now();
        let entity = db.table("entity");
        // Each once, in the table's order.
        let mut ids = pick(db);
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter().filter(|id| entity.row(*id).is_some_and(|e| shown(db, &e, now))).collect()
    });
    let ids: Vec<i64> = items.iter().map(|i| i.item_id).collect();
    let attrs = computed_attributes(ctx, &ids).await;
    let now = crate::bulkdata::filetime_now();
    let running = running_modules(ctx);
    Value::Dict(
        items
            .iter()
            .map(|item| (Value::Int(item.item_id), info_row_for_key(item, &ItemKey::Entity(item.item_id as i32),
                &attrs, now, running.iter().find(|module| module.module_id == item.item_id))))
            .collect(),
    )
}

fn info_row_for_key(item: &Item, key: &ItemKey, attrs: &BTreeMap<i64, BTreeMap<i32, f64>>, now: i64, running: Option<&crate::combat::Active>) -> Value {
    let own = attrs.get(&item.item_id);
    let mut inv_item = item.line.clone();
    let mut attributes = attribute_dict(item, own);
    if let (Some(expiry),Value::Dict(pairs))=(item.booster_expiry,&mut attributes) {
        pairs.retain(|(id,_)|*id!=Value::Int(i64::from(crate::svc::boosters::EXPIRY)));
        pairs.push((Value::Int(i64::from(crate::svc::boosters::EXPIRY)),Value::Long(expiry)));
    }
    if matches!(key, ItemKey::Sublocation { .. }) {
        if let Some(id) = inv_item.first_mut() {
            *id = key.value();
        }
        if let Value::Dict(pairs) = &mut attributes {
            pairs.retain(|(id, _)| *id != Value::Int(i64::from(ATTR_QUANTITY)));
            pairs.push((Value::Int(i64::from(ATTR_QUANTITY)), item.line.get(7).cloned().unwrap_or(Value::Int(0))));
        }
    }
    row(
        INFO_HEADER,
        vec![
            key.value(),
            row(INVITEM_HEADER, inv_item),
            attributes,
            active_effects(item, own, running),
            Value::Long(now),
        ],
    )
}

/// The entity rows `pick` chooses as items, leaving out what the client is
/// not shown.
fn load_items(ctx: &Ctx<'_>, pick: impl FnOnce(&Db) -> Vec<RowId>) -> Vec<Item> {
    ctx.store.read(|db| {
        let entity = db.table("entity");
        let boosters = db.table("character_boosters");
        pick(db)
            .into_iter()
            .filter_map(|id| entity.row(id))
            .filter_map(|r| {
                let item_id = r.int("itemID")?;
                let type_id = r.int("typeID")?;
                // An item of a type the static data does not know (or whose
                // group it does not know) is left out, as joining on it did.
                let (group_id, category_id) = crate::sde::get().group_and_category(type_id)?;
                let line = crate::items::invitem_line(&r)?;
                let item = Item {
                    item_id,
                    type_id,
                    owner_id: r.int("ownerID").unwrap_or(0),
                    location_id: r.int("locationID").unwrap_or(0),
                    group_id: group_id as i32,
                    category_id: category_id as i32,
                    booster_expiry: boosters.get_id(item_id).and_then(|b| b.int("expires")),
                    line,
                };
                let flag = r.int("flag").unwrap_or(0);
                if flag == 5 || flag == 87 {
                    return None;
                }
                if !ALLOWED_CATEGORIES.contains(&item.category_id)
                    && item.group_id != GROUP_CHARACTER
                {
                    return None;
                }
                let _ = item.type_id;
                Some(item)
            })
            .collect()
    })
}

/// Attributes as the dogma engine computes them for the character's loadout
/// (character, skills, implants, ship and what is fitted); anything outside it
/// gets its stored values.
async fn computed_attributes(ctx: &Ctx<'_>, item_ids: &[i64]) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    let mut out = BTreeMap::new();
    let mut rest = Vec::new();
    match ctx.session.char_id {
        Some(char_id) => {
            let active = active_modules(ctx);
            match ctx.store.read(|db| crate::dogma::Loadout::load(db, ctx.dogma, char_id, ctx.session.ship_id, &active)) {
                Ok(loadout) => {
                    for id in item_ids {
                        match loadout.find(*id) {
                            Some(i) => {
                                out.insert(*id, loadout.attributes(i).into_iter().collect());
                            }
                            None => rest.push(*id),
                        }
                    }
                }
                Err(e) => {
                    error!("dogma loadout: {}", crate::logging::chain(&e));
                    rest.extend_from_slice(item_ids);
                }
            }
        }
        None => rest.extend_from_slice(item_ids),
    }
    if !rest.is_empty() {
        out.extend(attributes_for(ctx, &rest));
    }
    live_vitals(ctx, &mut out);
    out
}

fn active_modules(ctx: &Ctx<'_>) -> HashSet<i64> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return HashSet::new();
    };
    ctx.world.lock().parks.get(&system)
        .and_then(|park| park.ships.get(&ship_id))
        .and_then(|ship| ship.fight.as_ref())
        .map(|fight| fight.applied.iter().copied().collect())
        .unwrap_or_default()
}

fn running_modules(ctx: &Ctx<'_>) -> Vec<crate::combat::Active> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Vec::new();
    };
    ctx.world.lock().parks.get(&system)
        .and_then(|park| park.ships.get(&ship_id))
        .and_then(|ship| ship.fight.as_ref())
        .map(|fight| fight.modules.clone()).unwrap_or_default()
}

/// In space, the ship's hit points and capacitor are the fight's, not the
/// database's.
fn live_vitals(ctx: &Ctx<'_>, attrs: &mut BTreeMap<i64, BTreeMap<i32, f64>>) {
    use crate::combat::attr;
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return };
    let Some(own) = attrs.get_mut(&ship_id) else { return };
    let world = ctx.world.lock();
    let Some(fight) = world.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()) else {
        return;
    };
    let now = crate::combat::now_ms();
    own.insert(attr::SHIELD_CHARGE, fight.vitals.shield.at(now));
    own.insert(attr::CHARGE, fight.vitals.capacitor.at(now));
    own.insert(attr::ARMOR_DAMAGE, fight.vitals.armor_damage);
    own.insert(attr::DAMAGE, fight.vitals.hull_damage);
}

fn attributes_for(
    ctx: &Ctx<'_>,
    item_ids: &[i64],
) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    // Each item's own values and, where it has none, its type's
    // (dgmTypeAttribs, the numeric ones); only attributes the static data
    // knows.
    let known = |attr: i32| crate::sde::table("dgmAttributeTypes").get("attributeID", attr as i64).is_some();
    let mut out: BTreeMap<i64, BTreeMap<i32, f64>> = BTreeMap::new();
    let mut types: BTreeMap<i64, i64> = BTreeMap::new();
    ctx.store.read(|db| {
        let entity = db.table("entity");
        let attributes = db.table("entity_attributes");
        for id in item_ids {
            let Some(e) = entity.get_id(*id) else { continue };
            types.insert(*id, e.int("typeID").unwrap_or(0));
            for a in attributes.find(&["itemID"], &[crate::store::Value::Int(*id)]) {
                let (Some(attr), Some(v)) = (a.int("attributeID"), a.float("valueFloat").or_else(|| a.float("valueInt"))) else { continue };
                let attr = attr as i32;
                if known(attr) {
                    out.entry(*id).or_default().insert(attr, v);
                }
            }
        }
    });
    for (item, type_id) in types {
        for ta in crate::sde::table("dgmTypeAttribs").find("typeID", type_id) {
            let (Some(attr), Some(v)) = (ta.int("attributeID"), ta.text("value").and_then(numeric)) else { continue };
            let attr = attr as i32;
            if known(attr) && !out.get(&item).is_some_and(|own| own.contains_key(&attr)) {
                out.entry(item).or_default().insert(attr, v);
            }
        }
        // The attributes invTypes keeps as columns (a hold's capacity): the
        // client reads a live item's from what it is sent, with 0 for none.
        let sde = crate::sde::get();
        for (attr, _) in crate::sde::TYPE_COLUMN_ATTRIBUTES {
            if let Some(v) = sde.type_attribute(type_id, *attr) {
                out.entry(item).or_default().entry(*attr).or_insert(v);
            }
        }
    }
    out
}

/// A dgmTypeAttribs value as a number, if it reads as one: what matching
/// `^\s*-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?\s*$` and casting to float8 gave.
fn numeric(value: &str) -> Option<f64> {
    let s = value.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'));
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    if b.first() == Some(&b'-') {
        i += 1;
    }
    if !digits(&mut i) {
        return None;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return None;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse().ok()
}

fn attribute_dict(item: &Item, own: Option<&BTreeMap<i32, f64>>) -> Value {
    let mut attrs: BTreeMap<i32, f64> = own.cloned().unwrap_or_default();
    for (value_attr, tau_attr, cap_attr) in CHARGED {
        if attrs.contains_key(&value_attr) {
            continue;
        }
        let (Some(&tau), Some(&cap)) = (attrs.get(&tau_attr), attrs.get(&cap_attr)) else {
            continue;
        };
        if tau != 0.0 && cap != 0.0 {
            attrs.insert(value_attr, cap);
        }
    }
    let _ = item;
    Value::Dict(
        attrs.into_iter().map(|(id, v)| (Value::Int(id as i64),
            if id==crate::svc::boosters::EXPIRY {Value::Long(v as i64)} else {number(v)})).collect(),
    )
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Value::Int(v as i64)
    } else {
        Value::Double(v)
    }
}

fn active_effects(item: &Item, own: Option<&BTreeMap<i32, f64>>, running: Option<&crate::combat::Active>) -> Value {
    let online = own.and_then(|a| a.get(&ATTR_IS_ONLINE)).copied().unwrap_or(0.0);
    let mut effects = Vec::new();
    let env = vec![
        Value::Int(item.item_id),
        Value::Int(item.owner_id),
        Value::Int(item.location_id),
        Value::None,
        Value::None,
        Value::List(Vec::new()),
        Value::Int(EFFECT_ONLINE),
    ];
    let mut d = env;
    d.push(Value::Long(crate::bulkdata::filetime_now()));
    d.push(Value::Int(0));
    d.push(Value::Int(0));
    d.push(Value::None);
    if online != 0.0 {
        effects.push((Value::Int(EFFECT_ONLINE), Value::List(d)));
    }
    if let Some(module) = running {
        effects.push((Value::Int(module.effect_id), Value::List(vec![
            Value::Int(item.item_id), Value::Int(item.owner_id), Value::Int(item.location_id),
            Value::int_or_none(module.target), Value::int_or_none(module.ammo.map(|ammo| ammo.item_id)),
            Value::List(Vec::new()), Value::Int(module.effect_id),
            Value::Long(crate::combat::filetime(module.started_ms)), Value::Int(module.duration_ms),
            Value::Int(module.repeat), Value::None,
        ])));
    }
    Value::Dict(effects)
}
