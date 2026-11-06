//! The attribute engine: what a character's ship, modules, charges, skills and
//! implants add up to.
//!
//! A [`Loadout`] holds the items and their base attributes (type values, then
//! the item's own stored values); [`Loadout::value`] applies every modifier
//! aimed at an attribute, computing the modifying values the same way, so
//! chains resolve themselves (a skill's level scales its bonus, which then
//! modifies the ship). Modifiers come from dgmTypeModifiers, plus the few that
//! 2.10 hard-coded: an online module loads the ship's CPU and power grid, and a
//! fitted turret or launcher takes a hardpoint.
//!
//! Operations apply in dogma's order (assign, multiply, divide, add, subtract,
//! post-multiply, post-divide, post-percent, post-assign). Multiplications
//! from modules and charges on an attribute that does not stack are stacking
//! penalised: the n-th strongest counts for exp(-(n/2.67)^2) of itself.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::RwLock;

use crate::store::{self, Db, StoreError};

pub mod attr {
    pub const IS_ONLINE: i32 = 2;
    pub const POWER_OUTPUT: i32 = 11;
    pub const LOW_SLOTS: i32 = 12;
    pub const MED_SLOTS: i32 = 13;
    pub const HI_SLOTS: i32 = 14;
    pub const POWER_LOAD: i32 = 15;
    pub const POWER: i32 = 30;
    pub const CPU_OUTPUT: i32 = 48;
    pub const CPU_LOAD: i32 = 49;
    pub const CPU: i32 = 50;
    pub const LAUNCHER_SLOTS_LEFT: i32 = 101;
    pub const TURRET_SLOTS_LEFT: i32 = 102;
    pub const RIG_SLOTS: i32 = 1137;
    pub const UPGRADE_CAPACITY: i32 = 1132;
    pub const UPGRADE_LOAD: i32 = 1152;
    pub const UPGRADE_COST: i32 = 1153;
    pub const UPGRADE_SLOTS_LEFT: i32 = 1154;
    pub const REQUIRED_SKILLS: [i32; 6] = [182, 183, 184, 1285, 1289, 1290];
}

pub mod effect {
    pub const LAUNCHER_FITTED: i32 = 40;
    pub const TURRET_FITTED: i32 = 42;
    pub const HI_POWER: i32 = 12;
    pub const MED_POWER: i32 = 13;
    pub const LO_POWER: i32 = 11;
    pub const RIG_SLOT: i32 = 2663;
}

pub mod category {
    pub const SHIP: i64 = 6;
    pub const MODULE: i64 = 7;
    pub const CHARGE: i64 = 8;
    pub const SKILL: i64 = 16;
    pub const IMPLANT: i64 = 20;
    pub const SUBSYSTEM: i64 = 32;
}

const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
const FLAG_BOOSTER: i32 = 88;
const FLAG_IMPLANT: i32 = 89;
pub const FLAG_SLOT_FIRST: i64 = 11;
pub const FLAG_SLOT_LAST: i64 = 35;
pub const FLAG_RIG_FIRST: i64 = 92;
pub const FLAG_RIG_LAST: i64 = 99;

/// Effect categories, as dgmTypeModifiers stores them.
const PASSIVE: i16 = 0;
const ACTIVE: i16 = 1;
const ONLINE: i16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Offline,
    Online,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Character,
    Ship,
    /// Fitted in a slot of the ship.
    Module,
    /// Loaded in a module; `module` is the module's index.
    Charge { module: usize },
    Skill,
    Implant,
    /// One of the character's drones in space: its skills reach it, its
    /// ship's fitting does not.
    Drone,
}

#[derive(Debug, Clone)]
pub struct Piece {
    pub item_id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub category_id: i64,
    pub flag: i64,
    pub role: Role,
    pub state: State,
    base: HashMap<i32, f64>,
    effects: HashSet<i32>,
}

impl Piece {
    pub fn has_effect(&self, effect_id: i32) -> bool {
        self.effects.contains(&effect_id)
    }

    fn required_skills(&self) -> impl Iterator<Item = i64> + '_ {
        attr::REQUIRED_SKILLS.iter().filter_map(|a| self.base.get(a)).map(|v| *v as i64).filter(|v| *v > 0)
    }

    /// Every attribute this piece has a value for before modification.
    fn base_attributes(&self) -> impl Iterator<Item = i32> + '_ {
        self.base.keys().copied()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Modifier {
    type_id: i64,
    category: i16,
    func: Func,
    domain: Domain,
    modified: i32,
    modifying: i32,
    operation: i16,
    group_id: Option<i64>,
    skill_type_id: Option<i64>,
    off_while_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Item,
    LocationGroup,
    LocationRequiredSkill,
    OwnerRequiredSkill,
    Location,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Domain {
    Ship,
    Char,
    Itself,
    Other,
    Target,
}

/// One modifier aimed at one (piece, attribute).
#[derive(Debug, Clone, Copy)]
struct Incoming {
    source: usize,
    modifying: i32,
    operation: i16,
    /// A literal value instead of an attribute of the source.
    fixed: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct AttributeType {
    default: f64,
    stackable: bool,
}

/// What dogma keeps between loadouts: attribute defaults from the static
/// data, and the fleet bonuses currently in force.
pub struct Catalog {
    attributes: HashMap<i32, AttributeType>,
    fleet_bonuses: RwLock<HashMap<i64, BTreeMap<i32, f64>>>,
}

impl Catalog {
    pub fn new() -> Self {
        let attributes = crate::sde::get()
            .attribute_types()
            .map(|(id, a)| {
                (id, AttributeType { default: a.default_value.unwrap_or(0.0), stackable: a.stackable.unwrap_or(true) })
            })
            .collect();
        Catalog { attributes, fleet_bonuses: RwLock::new(HashMap::new()) }
    }

    pub fn fleet_bonuses(&self, char_id: i64) -> BTreeMap<i32, f64> {
        self.fleet_bonuses.read().expect("fleet bonuses lock").get(&char_id).cloned().unwrap_or_default()
    }

    /// Replace the current fleet state atomically, returning each changed
    /// pilot's previous bonuses so live ships can be refitted and notified.
    pub fn replace_fleet_bonuses(&self, next: HashMap<i64, BTreeMap<i32, f64>>) -> Vec<(i64, BTreeMap<i32, f64>)> {
        let mut current = self.fleet_bonuses.write().expect("fleet bonuses lock");
        let ids = current.keys().chain(next.keys()).copied().collect::<HashSet<_>>();
        let changed = ids.into_iter().filter_map(|id| {
            let before = current.get(&id).cloned().unwrap_or_default();
            let after = next.get(&id).cloned().unwrap_or_default();
            (before != after).then_some((id, before))
        }).collect();
        *current = next;
        changed
    }

    /// Keep the live ship and catalog consistent if a refit failed. The next
    /// fleet pulse compares against this state and retries the pending change.
    pub fn restore_fleet_bonuses(&self, char_id: i64, previous: BTreeMap<i32, f64>) {
        let mut current = self.fleet_bonuses.write().expect("fleet bonuses lock");
        if previous.is_empty() { current.remove(&char_id); }
        else { current.insert(char_id, previous); }
    }
}

/// Original effects 234/235/494 supply the Warp Drive Operation level
/// multiplier and ship warp-capacitor percentage. The imported modifier
/// table maps them for skill3455/implant13241, but omits rig26066 despite
/// its identical original effect494. A type with one of these effects and no
/// modifiers of its own for it borrows that effect's modifiers from the types
/// that have them (each distinct one once); no persistent rows are added.
const BORROWED_EFFECTS: [i32; 3] = [234, 235, 494];

/// A type's modifiers: its own from dgmTypeModifiers, then any it borrows.
fn type_modifiers(sde: &crate::sde::Sde, type_id: i64) -> Vec<Modifier> {
    let mut out: Vec<Modifier> = sde.type_modifiers(type_id).filter_map(|m| modifier(type_id, m.effect_category, m)).collect();
    let mut borrowed: Vec<Modifier> = Vec::new();
    for effect_id in sde.type_effects(type_id).iter().filter(|e| BORROWED_EFFECTS.contains(e)) {
        let Some(effect) = sde.effect(*effect_id) else { continue };
        if sde.type_modifiers(type_id).any(|m| m.effect_name == effect.name) {
            continue;
        }
        for m in sde.effect_modifiers(&effect.name).filter_map(|m| modifier(type_id, effect.category, m)) {
            if !borrowed.contains(&m) {
                borrowed.push(m);
            }
        }
    }
    out.extend(borrowed);
    out
}

/// A modifier row as dogma applies it, for `type_id`; None for a func or
/// domain the engine does not model.
fn modifier(type_id: i64, category: i16, m: &crate::sde::TypeModifier) -> Option<Modifier> {
    let func = match &*m.func {
        "ItemModifier" => Func::Item,
        "LocationGroupModifier" => Func::LocationGroup,
        "LocationRequiredSkillModifier" => Func::LocationRequiredSkill,
        "OwnerRequiredSkillModifier" => Func::OwnerRequiredSkill,
        "LocationModifier" => Func::Location,
        _ => return None,
    };
    let domain = match &*m.domain {
        "shipID" => Domain::Ship,
        "charID" => Domain::Char,
        "itemID" => Domain::Itself,
        "otherID" => Domain::Other,
        "targetID" | "target" => Domain::Target,
        _ => return None,
    };
    Some(Modifier {
        type_id,
        category,
        func,
        domain,
        modified: m.modified_attribute_id,
        modifying: m.modifying_attribute_id,
        operation: m.operation,
        group_id: m.group_id.map(i64::from),
        skill_type_id: m.skill_type_id.map(i64::from),
        off_while_active: m.off_while_active,
    })
}

pub struct Loadout<'c> {
    catalog: &'c Catalog,
    pub pieces: Vec<Piece>,
    modifiers: Vec<Modifier>,
    incoming: HashMap<(usize, i32), Vec<Incoming>>,
    memo: RefCell<HashMap<(usize, i32), f64>>,
    busy: RefCell<HashSet<(usize, i32)>>,
}

impl<'c> Loadout<'c> {
    /// The character, its skills and implants, and (if given) its ship with
    /// whatever is fitted. A module is online when its isOnline attribute says
    /// so; `active` lists modules whose active effects are running.
    pub fn load(
        db: &Db,
        catalog: &'c Catalog,
        char_id: i64,
        ship_id: Option<i64>,
        active: &HashSet<i64>,
    ) -> Result<Loadout<'c>, StoreError> {
        Self::load_with_drones(db, catalog, char_id, ship_id, active, &[])
    }

    /// As [`Loadout::load`], with some of the character's drones as well.
    pub fn load_with_drones(
        db: &Db,
        catalog: &'c Catalog,
        char_id: i64,
        ship_id: Option<i64>,
        active: &HashSet<i64>,
        drones: &[i64],
    ) -> Result<Loadout<'c>, StoreError> {
        let sde = crate::sde::get();
        let entity = db.table("entity");
        let ship = ship_id.unwrap_or(0);
        // The character and ship themselves, the character's skills,
        // boosters and implants, what is fitted to the ship, and the drones,
        // by itemID.
        let mut candidates: Vec<store::Row> = Vec::new();
        candidates.extend(entity.get_id(char_id));
        candidates.extend(entity.get_id(ship));
        let on_character = [FLAG_SKILL, FLAG_SKILL_IN_TRAINING, FLAG_BOOSTER, FLAG_IMPLANT].map(i64::from);
        candidates.extend(entity.find(&["locationID"], &[store::Value::Int(char_id)]).filter(|e| e.int("flag").is_some_and(|f| on_character.contains(&f))));
        let fitted = |f: i64| (FLAG_SLOT_FIRST..=FLAG_SLOT_LAST).contains(&f) || (FLAG_RIG_FIRST..=FLAG_RIG_LAST).contains(&f);
        candidates.extend(entity.find(&["locationID"], &[store::Value::Int(ship)]).filter(|e| e.int("flag").is_some_and(fitted)));
        candidates.extend(drones.iter().filter_map(|d| entity.get_id(*d)));
        let found: BTreeMap<i64, store::Row> = candidates.into_iter().filter_map(|r| Some((r.int("itemID")?, r))).collect();
        // A booster counts only while the character has it running.
        let now = crate::bulkdata::filetime_now();
        let boosters = db.table("character_boosters");
        let running = |e: &store::Row| {
            boosters.get_id(e.int("itemID").unwrap_or(0)).is_some_and(|b| {
                b.int("character") == Some(char_id)
                    && b.int("type_id") == e.int("typeID")
                    && e.int("ownerID") == Some(char_id)
                    && e.int("locationID") == Some(char_id)
                    && e.int("quantity") == Some(1)
                    && e.int("singleton") == Some(1)
                    && b.int("expires").is_some_and(|t| t > now)
            })
        };
        let rows: Vec<store::Row> = found.into_values().filter(|e| e.int("flag") != Some(i64::from(FLAG_BOOSTER)) || running(e)).collect();
        let mut pieces = Vec::with_capacity(rows.len());
        for r in &rows {
            let item_id = r.int("itemID").unwrap_or(0);
            let type_id = r.int("typeID").unwrap_or(0);
            // An item of a type the static data does not know (or whose
            // group it does not know) is left out, as joining on it did.
            let Some((group_id, category_id)) = sde.group_and_category(type_id) else { continue };
            let flag = r.int("flag").unwrap_or(0);
            let role = if item_id == char_id {
                Role::Character
            } else if Some(item_id) == ship_id {
                Role::Ship
            } else if category_id == category::SKILL {
                Role::Skill
            } else if drones.contains(&item_id) {
                Role::Drone
            } else if r.int("locationID") == Some(char_id) {
                Role::Implant
            } else if category_id == category::CHARGE {
                Role::Charge { module: usize::MAX }
            } else {
                Role::Module
            };
            pieces.push(Piece {
                item_id,
                type_id,
                group_id,
                category_id,
                flag,
                role,
                state: State::Offline,
                base: HashMap::new(),
                effects: HashSet::new(),
            });
        }
        // A charge sits in its module's slot.
        for i in 0..pieces.len() {
            if let Role::Charge { .. } = pieces[i].role {
                let flag = pieces[i].flag;
                if let Some(m) = pieces.iter().position(|p| p.role == Role::Module && p.flag == flag) {
                    pieces[i].role = Role::Charge { module: m };
                }
            }
        }

        for p in pieces.iter_mut() {
            p.base.extend(sde.type_attributes(p.type_id).iter().copied());
        }
        // Each item's own stored values override its type's.
        let attributes = db.table("entity_attributes");
        for p in pieces.iter_mut() {
            for r in attributes.find(&["itemID"], &[store::Value::Int(p.item_id)]) {
                let value = r.float("valueFloat").or_else(|| r.float("valueInt"));
                let (Some(attribute), Some(v)) = (r.int("attributeID"), value) else { continue };
                p.base.insert(attribute as i32, v);
            }
        }

        for p in pieces.iter_mut() {
            p.effects = sde.type_effects(p.type_id).iter().copied().collect();
            p.state = match p.role {
                Role::Module if active.contains(&p.item_id) => State::Active,
                Role::Module if p.base.get(&attr::IS_ONLINE).copied().unwrap_or(0.0) != 0.0 => State::Online,
                Role::Module => State::Offline,
                _ => State::Online,
            };
        }

        let mut type_ids: Vec<i64> = pieces.iter().map(|p| p.type_id).collect();
        type_ids.sort_unstable();
        type_ids.dedup();
        let modifiers = type_ids.into_iter().flat_map(|t| type_modifiers(sde, t)).collect();

        let mut loadout = Loadout {
            catalog,
            pieces,
            modifiers,
            incoming: HashMap::new(),
            memo: RefCell::new(HashMap::new()),
            busy: RefCell::new(HashSet::new()),
        };
        loadout.apply_fleet_bonuses(&catalog.fleet_bonuses(char_id));
        Ok(loadout)
    }

    /// Fleet skills are external skill modifiers, exempt from module stacking
    /// penalties. Rewiring permits callers to reconstruct the previous state.
    pub fn apply_fleet_bonuses(&mut self, bonuses: &BTreeMap<i32, f64>) {
        self.wire();
        self.memo.borrow_mut().clear();
        self.busy.borrow_mut().clear();
        let Some(source) = self.character().or_else(|| self.ship()) else { return };
        for (i, piece) in self.pieces.iter().enumerate() {
            for (&attribute, &percent) in bonuses {
                let applies = if attribute == 77 { piece.role == Role::Module && piece.base.contains_key(&attribute) }
                    else { piece.role == Role::Ship };
                if applies {
                    self.incoming.entry((i, attribute)).or_default().push(Incoming { source, modifying: 0, operation: 6, fixed: Some(percent) });
                }
            }
        }
    }

    fn index_of(&self, role: Role) -> Option<usize> {
        self.pieces.iter().position(|p| p.role == role)
    }

    pub fn find(&self, item_id: i64) -> Option<usize> {
        self.pieces.iter().position(|p| p.item_id == item_id)
    }

    pub fn ship(&self) -> Option<usize> {
        self.index_of(Role::Ship)
    }

    pub fn character(&self) -> Option<usize> {
        self.index_of(Role::Character)
    }

    /// The charge loaded in module `i`.
    pub fn charge_of(&self, i: usize) -> Option<usize> {
        self.pieces.iter().position(|p| p.role == Role::Charge { module: i })
    }

    /// Whether piece `i`'s type (or the item itself) has attribute `a`, rather
    /// than falling back to the attribute's default.
    pub fn has(&self, i: usize, a: i32) -> bool {
        self.pieces[i].base.contains_key(&a)
    }

    /// Whether a modifier of `category` from piece `i` applies right now.
    fn applies(&self, i: usize, m: &Modifier) -> bool {
        let p = &self.pieces[i];
        match p.role {
            Role::Module => match m.category {
                PASSIVE => true,
                ONLINE => match p.state {
                    State::Online => true,
                    State::Active => !m.off_while_active,
                    State::Offline => false,
                },
                ACTIVE => p.state == State::Active,
                _ => false,
            },
            // A loaded charge counts; its active effects only while its module runs.
            Role::Charge { module } if module != usize::MAX => {
                m.category != ACTIVE || self.pieces[module].state == State::Active
            }
            Role::Charge { .. } => false,
            _ => m.category == PASSIVE || m.category == ONLINE,
        }
    }

    /// Resolve every modifier to the (piece, attribute) pairs it touches.
    fn wire(&mut self) {
        let mut incoming: HashMap<(usize, i32), Vec<Incoming>> = HashMap::new();
        let ship = self.index_of(Role::Ship);
        let character = self.index_of(Role::Character);
        let on_ship: Vec<usize> = self
            .pieces
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.role, Role::Module | Role::Charge { .. }))
            .map(|(i, _)| i)
            .collect();
        // What the character's skills reach by owner: the fitting and its drones.
        let owned: Vec<usize> = self
            .pieces
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.role, Role::Module | Role::Charge { .. } | Role::Drone))
            .map(|(i, _)| i)
            .collect();

        for (i, p) in self.pieces.iter().enumerate() {
            if p.group_id == 303 {
                // Type IDs changed meaning across releases. The imported
                // later modifier table contains unrelated learning/ROF
                // effects and duplicate bonuses for original boosters.
                if let Some((effect,target,modified,modifying))=crate::svc::boosters::modifier(p.type_id).filter(|_|p.flag==88 && p.role==Role::Implant) {
                    if p.has_effect(effect) {
                        use crate::svc::boosters::Target;
                        let targets=match target {
                            Target::Ship=>ship.into_iter().collect::<Vec<_>>(),
                            Target::Gunnery=>on_ship.iter().copied().filter(|&t|self.pieces[t].required_skills().any(|s|s==3300)).collect(),
                            Target::Missile=>owned.iter().copied().filter(|&t|self.pieces[t].required_skills().any(|s|s==3319)).collect(),
                        };
                        for t in targets {incoming.entry((t,modified)).or_default().push(Incoming {source:i,modifying,operation:6,fixed:None});}
                    }
                }
                continue;
            }
            for m in self.modifiers.iter().filter(|m| m.type_id == p.type_id) {
                if !self.applies(i, m) {
                    continue;
                }
                let domain = match m.domain {
                    Domain::Ship => ship,
                    Domain::Char => character,
                    Domain::Itself => Some(i),
                    Domain::Other => match p.role {
                        Role::Charge { module } if module != usize::MAX => Some(module),
                        Role::Module => self.pieces.iter().position(|c| c.role == Role::Charge { module: i }),
                        _ => None,
                    },
                    Domain::Target => None,
                };
                let Some(domain) = domain else { continue };
                let located: &[usize] = if Some(domain) == ship { &on_ship } else { &[] };
                let targets: Vec<usize> = match m.func {
                    Func::Item => vec![domain],
                    Func::Location => located.to_vec(),
                    Func::LocationGroup => {
                        located.iter().copied().filter(|&t| Some(self.pieces[t].group_id) == m.group_id).collect()
                    }
                    Func::LocationRequiredSkill | Func::OwnerRequiredSkill => {
                        let skill = m.skill_type_id.unwrap_or(-1);
                        let scope: &[usize] = if m.func == Func::OwnerRequiredSkill { &owned } else { located };
                        scope.iter().copied().filter(|&t| self.pieces[t].required_skills().any(|s| s == skill)).collect()
                    }
                };
                for t in targets {
                    incoming.entry((t, m.modified)).or_default().push(Incoming {
                        source: i,
                        modifying: m.modifying,
                        operation: m.operation,
                        fixed: None,
                    });
                }
            }
            // Original 45017 skill effect848 applies effective attribute619
            // (effect850 already scales it by trained level) as a percentage
            // to cloak modules' sensor recalibration time. The imported later
            // modifier table omits848; do not duplicate an explicit mapping.
            if p.has_effect(848) && !self.modifiers.iter().any(|m|m.type_id==p.type_id && m.modified==560 && m.modifying==619) {
                for &t in &on_ship {
                    if self.pieces[t].required_skills().any(|skill|skill==11579) {
                        incoming.entry((t,560)).or_default().push(Incoming {source:i,modifying:619,operation:6,fixed:None});
                    }
                }
            }
        }

        // What 2.10 hard-coded: fitting load and hardpoints.
        if let Some(ship) = ship {
            for (i, p) in self.pieces.iter().enumerate().filter(|(_, p)| p.role == Role::Module) {
                if p.state != State::Offline {
                    for (load, need) in [(attr::CPU_LOAD, attr::CPU), (attr::POWER_LOAD, attr::POWER)] {
                        incoming.entry((ship, load)).or_default().push(Incoming { source: i, modifying: need, operation: 2, fixed: None });
                    }
                }
                for (fitted, left) in [(effect::TURRET_FITTED, attr::TURRET_SLOTS_LEFT), (effect::LAUNCHER_FITTED, attr::LAUNCHER_SLOTS_LEFT)] {
                    if p.has_effect(fitted) {
                        incoming.entry((ship, left)).or_default().push(Incoming { source: i, modifying: 0, operation: 3, fixed: Some(1.0) });
                    }
                }
                if p.has_effect(effect::RIG_SLOT) {
                    incoming.entry((ship, attr::UPGRADE_LOAD)).or_default()
                        .push(Incoming { source: i, modifying: attr::UPGRADE_COST, operation: 2, fixed: None });
                    incoming.entry((ship, attr::UPGRADE_SLOTS_LEFT)).or_default()
                        .push(Incoming { source: i, modifying: 0, operation: 3, fixed: Some(1.0) });
                }
            }
        }
        self.incoming = incoming;
    }

    fn base(&self, i: usize, a: i32) -> f64 {
        self.pieces[i]
            .base
            .get(&a)
            .copied()
            .unwrap_or_else(|| self.catalog.attributes.get(&a).map_or(0.0, |t| t.default))
    }

    /// The value of attribute `a` on piece `i` with every modifier applied.
    pub fn value(&self, i: usize, a: i32) -> f64 {
        if let Some(v) = self.memo.borrow().get(&(i, a)) {
            return *v;
        }
        // A modifier loop (none should exist) falls back to the base value.
        if !self.busy.borrow_mut().insert((i, a)) {
            return self.base(i, a);
        }
        let v = self.compute(i, a);
        self.busy.borrow_mut().remove(&(i, a));
        self.memo.borrow_mut().insert((i, a), v);
        v
    }

    fn compute(&self, i: usize, a: i32) -> f64 {
        let mut value = self.base(i, a);
        let Some(mods) = self.incoming.get(&(i, a)) else { return value };
        let stackable = self.catalog.attributes.get(&a).map_or(true, |t| t.stackable);
        let mut by_op: HashMap<i16, Vec<(f64, bool)>> = HashMap::new();
        for m in mods {
            let v = m.fixed.unwrap_or_else(|| self.value(m.source, m.modifying));
            let exempt = matches!(
                self.pieces[m.source].category_id,
                category::SHIP | category::CHARGE | category::SKILL | category::IMPLANT | category::SUBSYSTEM
            ) || self.pieces[m.source].role == Role::Character;
            by_op.entry(m.operation).or_default().push((v, !stackable && !exempt));
        }
        for op in [-1, 0, 1, 2, 3, 4, 5, 6, 7] {
            let Some(vals) = by_op.get(&op) else { continue };
            match op {
                -1 | 7 => value = vals.last().map_or(value, |(v, _)| *v),
                2 => value += vals.iter().map(|(v, _)| v).sum::<f64>(),
                3 => value -= vals.iter().map(|(v, _)| v).sum::<f64>(),
                _ => {
                    let factor = |v: f64| match op {
                        0 | 4 => v,
                        1 | 5 => {
                            if v == 0.0 {
                                1.0
                            } else {
                                1.0 / v
                            }
                        }
                        _ => 1.0 + v / 100.0,
                    };
                    let free: f64 = vals.iter().filter(|(_, p)| !p).map(|(v, _)| factor(*v)).product();
                    let penalised: Vec<f64> = vals.iter().filter(|(_, p)| *p).map(|(v, _)| factor(*v)).collect();
                    value *= free * stacking_penalised(&penalised);
                }
            }
        }
        value
    }

    /// Every attribute piece `i` has, computed.
    pub fn attributes(&self, i: usize) -> HashMap<i32, f64> {
        let mut ids: HashSet<i32> = self.pieces[i].base_attributes().collect();
        ids.extend(self.incoming.keys().filter(|(p, _)| *p == i).map(|(_, a)| *a));
        ids.into_iter().map(|a| (a, self.value(i, a))).collect()
    }

    /// Every piece's computed attributes, by item ID.
    pub fn snapshot(&self) -> HashMap<i64, HashMap<i32, f64>> {
        (0..self.pieces.len()).map(|i| (self.pieces[i].item_id, self.attributes(i))).collect()
    }
}

/// Multiply `factors` together, the n-th furthest from 1 (counted separately
/// for bonuses and penalties) weighted by exp(-(n/2.67)^2).
pub fn stacking_penalised(factors: &[f64]) -> f64 {
    let mut total = 1.0;
    for side in [true, false] {
        let mut group: Vec<f64> = factors.iter().copied().filter(|f| (*f > 1.0) == side && *f != 1.0).collect();
        group.sort_by(|a, b| (b - 1.0).abs().total_cmp(&(a - 1.0).abs()));
        for (n, f) in group.iter().enumerate() {
            total *= 1.0 + (f - 1.0) * (-(n as f64 / 2.67).powi(2)).exp();
        }
    }
    total
}
