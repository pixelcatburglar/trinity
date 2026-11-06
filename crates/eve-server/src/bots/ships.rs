//! What bots fly: a ladder of hull classes for each role, climbed as a
//! bot's wallet grows, and the modules a hull of each class is fitted with.
//!
//! - Fighters (ratters and pirates) go frigate, destroyer, cruiser,
//!   battlecruiser, battleship, and the richest on to a capital: ratters a
//!   carrier, pirates a dreadnought; explorers stop at cruisers; miners go
//!   frigate, mining barge, exhumer; haulers climb the industrials by hold
//!   size; traders fly shuttles, then industrials.
//! - A bot moves up a rung when it can buy the next hull several times
//!   over (`upgrade`), and after a loss buys the hull it lost again if it
//!   can, or the best it can afford (`rebuy`). Hulls come off the market
//!   the bots supply (`economy::buy_hull`).
//! - A fitting is real module types, the same for the same bot and hull:
//!   guns of its race and size in the turret slots, a propulsion module and
//!   a tank in the mids and lows, mining lasers for miners, and ammunition.
//!   It is what a dead bot's wreck holds and its killmail lists.

use std::collections::HashMap;

use super::behaviour::Role;
use crate::combat::Rng;
use crate::sde::Row;

const ATTR_LOW_SLOTS: i32 = 12;
const ATTR_MED_SLOTS: i32 = 13;
const ATTR_HI_SLOTS: i32 = 14;
const ATTR_CAPACITY: i32 = 38;
const ATTR_TURRET_SLOTS: i32 = 102;
const ATTR_CHARGE_SIZE: i32 = 128;
const ATTR_META_LEVEL: i32 = 633;

pub const GROUP_FRIGATE: i32 = 25;
pub const GROUP_CRUISER: i32 = 26;
pub const GROUP_BATTLESHIP: i32 = 27;
pub const GROUP_INDUSTRIAL: i32 = 28;
pub const GROUP_SHUTTLE: i32 = 31;
pub const GROUP_BATTLECRUISER: i32 = 419;
pub const GROUP_DESTROYER: i32 = 420;
pub const GROUP_BARGE: i32 = 463;
pub const GROUP_EXHUMER: i32 = 543;
pub const GROUP_DREADNOUGHT: i32 = 485;
pub const GROUP_CARRIER: i32 = 547;
const HULL_GROUPS: [i32; 11] = [
    GROUP_FRIGATE, GROUP_CRUISER, GROUP_BATTLESHIP, GROUP_INDUSTRIAL, GROUP_SHUTTLE, GROUP_BATTLECRUISER, GROUP_DESTROYER, GROUP_BARGE, GROUP_EXHUMER,
    GROUP_DREADNOUGHT, GROUP_CARRIER,
];

const RACE_CALDARI: i64 = 1;
const RACE_MINMATAR: i64 = 2;
const RACE_AMARR: i64 = 4;

// Module groups.
const ENERGY_WEAPON: i32 = 53;
const PROJECTILE_WEAPON: i32 = 55;
const HYBRID_WEAPON: i32 = 74;
const MINING_LASER: i32 = 54;
const STRIP_MINER: i32 = 464;
const PROJECTILE_AMMO: i32 = 83;
const HYBRID_CHARGE: i32 = 85;
const FREQUENCY_CRYSTAL: i32 = 86;
const SHIELD_EXTENDER: i32 = 38;
const SHIELD_BOOSTER: i32 = 40;
const AFTERBURNER: i32 = 46;
const DAMAGE_CONTROL: i32 = 60;
const ARMOR_REPAIRER: i32 = 62;
const ARMOR_PLATE: i32 = 329;
const GYROSTABILIZER: i32 = 59;
const HEAT_SINK: i32 = 205;
const MAGNETIC_STABILIZER: i32 = 302;
const MODULE_GROUPS: [i32; 16] = [
    ENERGY_WEAPON, PROJECTILE_WEAPON, HYBRID_WEAPON, MINING_LASER, STRIP_MINER, PROJECTILE_AMMO, HYBRID_CHARGE, FREQUENCY_CRYSTAL,
    SHIELD_EXTENDER, SHIELD_BOOSTER, AFTERBURNER, DAMAGE_CONTROL, ARMOR_REPAIRER, ARMOR_PLATE, GYROSTABILIZER, HEAT_SINK,
];

/// A hull a bot can fly.
#[derive(Debug, Clone)]
pub struct Hull {
    pub type_id: i32,
    pub group: i32,
    pub race: i64,
    /// What it costs on the market, before the bots' own supply moves it.
    pub price: f64,
    /// Its hold, m3.
    pub capacity: f64,
    pub slots: (u8, u8, u8),
    pub turrets: u8,
}

/// A module or charge a fitting may hold.
#[derive(Debug, Clone)]
struct Module {
    type_id: i32,
    group: i32,
    /// Turret and charge size (1 small to 4 capital); 0 for the rest.
    size: i32,
    meta: i32,
    name: String,
}

pub struct Shipyard {
    pub hulls: HashMap<i32, Hull>,
    /// Hulls by (group, race), cheapest first.
    by_group_race: HashMap<(i32, i64), Vec<i32>>,
    modules: Vec<Module>,
}

/// The rungs of a role's ladder: the hull groups of each tier.
pub fn ladder(role: Role) -> &'static [i32] {
    match role {
        Role::Ratter => &[GROUP_FRIGATE, GROUP_DESTROYER, GROUP_CRUISER, GROUP_BATTLECRUISER, GROUP_BATTLESHIP, GROUP_CARRIER],
        Role::Pirate => &[GROUP_FRIGATE, GROUP_DESTROYER, GROUP_CRUISER, GROUP_BATTLECRUISER, GROUP_BATTLESHIP, GROUP_DREADNOUGHT],
        Role::Explorer => &[GROUP_FRIGATE, GROUP_DESTROYER, GROUP_CRUISER],
        Role::Miner => &[GROUP_FRIGATE, GROUP_BARGE, GROUP_EXHUMER],
        // Industrials, small to big: the tier picks a third of them by hold.
        Role::Hauler => &[GROUP_INDUSTRIAL, GROUP_INDUSTRIAL, GROUP_INDUSTRIAL],
        Role::Trader => &[GROUP_SHUTTLE, GROUP_INDUSTRIAL],
    }
}

/// The class a hull's group stands for in fights: frigate 0 to capital 5.
pub fn weight_class(group: i32) -> u8 {
    match group {
        GROUP_DESTROYER => 1,
        GROUP_CRUISER | GROUP_BARGE | GROUP_EXHUMER | GROUP_INDUSTRIAL => 2,
        GROUP_BATTLECRUISER => 3,
        GROUP_BATTLESHIP => 4,
        GROUP_DREADNOUGHT | GROUP_CARRIER => 5,
        _ => 0,
    }
}

/// Every hull type the ladders draw on.
pub fn hull_types() -> Vec<i32> {
    hull_rows().iter().filter_map(|t| t.int("typeID")).map(|t| t as i32).collect()
}

/// A type's largest value of an attribute as an integer (`valueInt`, else
/// `valueFloat` rounded as SQL's `::int` rounds), 0 without one.
fn int_attribute(type_id: i64, attribute: i32) -> i64 {
    crate::sde::table("dgmTypeAttributes")
        .find("typeID", type_id)
        .filter(|a| a.int("attributeID") == Some(attribute as i64))
        .filter_map(|a| a.int("valueInt").or_else(|| a.float("valueFloat").map(|v| v.round_ties_even() as i64)))
        .max()
        .unwrap_or(0)
}

/// The marketable types of a race in the hull groups, by type ID.
fn hull_rows() -> Vec<Row<'static>> {
    types_of(&HULL_GROUPS, |t| t.int("marketGroupID").is_some() && t.int("raceID").is_some())
}

/// The invTypes rows of `groups` that `keep` lets through, by type ID.
fn types_of(groups: &[i32], keep: impl Fn(&Row) -> bool) -> Vec<Row<'static>> {
    let types = crate::sde::table("invTypes");
    let mut rows: Vec<Row<'static>> = groups.iter().flat_map(|g| types.find("groupID", *g as i64)).filter(|t| keep(t)).collect();
    rows.sort_by_key(|t| t.int("typeID"));
    rows
}

impl Shipyard {
    /// Every hull of the ladders, with its market price (from `prices`, the
    /// NPC asks), hold and slots; and the modules fittings draw on.
    pub fn load(prices: &HashMap<i32, f64>) -> Shipyard {
        let sde = crate::sde::get();
        let mut hulls = HashMap::new();
        let mut by_group_race: HashMap<(i32, i64), Vec<i32>> = HashMap::new();
        for t in hull_rows() {
            let type_id = t.int("typeID").unwrap_or(0) as i32;
            let Some(&price) = prices.get(&type_id) else { continue };
            // The type's largest value of an attribute, 0 without one.
            let attribute = |id: i32| {
                sde.type_attributes(type_id as i64).iter().filter(|(a, _)| *a == id).map(|(_, v)| *v).reduce(f64::max).unwrap_or(0.0)
            };
            let capacity = Some(attribute(ATTR_CAPACITY)).filter(|c| *c > 0.0).unwrap_or_else(|| t.float("capacity").unwrap_or(0.0));
            let slot = |id: i32| attribute(id).clamp(0.0, 8.0) as u8;
            let hull = Hull {
                type_id,
                group: t.int("groupID").unwrap_or(0) as i32,
                race: t.int("raceID").unwrap_or(0),
                price,
                capacity,
                slots: (slot(ATTR_HI_SLOTS), slot(ATTR_MED_SLOTS), slot(ATTR_LOW_SLOTS)),
                turrets: slot(ATTR_TURRET_SLOTS),
            };
            by_group_race.entry((hull.group, hull.race)).or_default().push(type_id);
            hulls.insert(type_id, hull);
        }
        for list in by_group_race.values_mut() {
            list.sort_by(|a, b| hulls[a].price.total_cmp(&hulls[b].price).then(a.cmp(b)));
        }
        let groups = MODULE_GROUPS.iter().copied().chain([MAGNETIC_STABILIZER]).collect::<Vec<i32>>();
        let modules = types_of(&groups, |t| t.int("marketGroupID").is_some() && t.int("published") == Some(1))
            .iter()
            .map(|t| {
                let type_id = t.int("typeID").unwrap_or(0);
                Module {
                    type_id: type_id as i32,
                    group: t.int("groupID").unwrap_or(0) as i32,
                    name: t.text("typeName").unwrap_or_default().to_string(),
                    size: int_attribute(type_id, ATTR_CHARGE_SIZE) as i32,
                    meta: int_attribute(type_id, ATTR_META_LEVEL) as i32,
                }
            })
            .collect();
        Shipyard { hulls, by_group_race, modules }
    }

    /// The hull of a role's tier for a race, chosen by `seed` among those of
    /// the right class (another race's when its own has none).
    pub fn hull_for(&self, role: Role, race: i64, tier: u8, seed: u64) -> Option<i32> {
        let rungs = ladder(role);
        let tier = (tier as usize).min(rungs.len() - 1);
        let group = rungs[tier];
        let list = self
            .by_group_race
            .get(&(group, race))
            .or_else(|| self.by_group_race.iter().filter(|((g, _), _)| *g == group).min_by_key(|((_, r), _)| *r).map(|(_, l)| l))?;
        if role == Role::Hauler {
            // Industrials by hold: tier 0 the smallest third, 2 the biggest.
            let mut by_hold = list.clone();
            by_hold.sort_by(|a, b| self.hulls[a].capacity.total_cmp(&self.hulls[b].capacity));
            let third = by_hold.len().div_ceil(3).max(1);
            let part: Vec<i32> = by_hold.chunks(third).nth(tier).or(by_hold.chunks(third).last()).map(|c| c.to_vec()).unwrap_or_default();
            return part.get((seed % part.len().max(1) as u64) as usize).copied();
        }
        list.get((seed % list.len() as u64) as usize).copied()
    }

    /// Where a hull stands on a role's ladder.
    pub fn tier_of(&self, role: Role, hull: i32) -> u8 {
        let Some(h) = self.hulls.get(&hull) else { return 0 };
        if role == Role::Hauler {
            let mut same: Vec<&Hull> = self.hulls.values().filter(|o| o.group == GROUP_INDUSTRIAL && o.race == h.race).collect();
            same.sort_by(|a, b| a.capacity.total_cmp(&b.capacity));
            // The same thirds `hull_for` picks from.
            let at = same.iter().position(|o| o.type_id == hull).unwrap_or(0);
            let third = same.len().div_ceil(3).max(1);
            return (at / third).min(2) as u8;
        }
        ladder(role).iter().position(|g| *g == h.group).unwrap_or(0) as u8
    }

    pub fn price(&self, hull: i32) -> f64 {
        self.hulls.get(&hull).map_or(0.0, |h| h.price)
    }

    pub fn race_of(&self, hull: i32) -> i64 {
        self.hulls.get(&hull).map_or(RACE_AMARR, |h| h.race)
    }

    pub fn capacity(&self, hull: i32) -> f64 {
        self.hulls.get(&hull).map_or(0.0, |h| h.capacity)
    }

    pub fn group_of(&self, hull: i32) -> i32 {
        self.hulls.get(&hull).map_or(0, |h| h.group)
    }

    /// All the hulls any ladder holds: the types space must know.
    pub fn all(&self) -> impl Iterator<Item = i32> + '_ {
        self.hulls.keys().copied()
    }

    /// What a bot's hull carries: (type, quantity) of its modules, and the
    /// ammunition its guns use. Richer bots fit better (named) modules where
    /// there are any.
    pub fn fitting(&self, hull: i32, seed: u64, rich: bool) -> Vec<(i32, i64)> {
        let Some(h) = self.hulls.get(&hull) else { return Vec::new() };
        let mut rng = Rng::seeded(seed ^ (hull as u64).wrapping_mul(0x9e37_79b9));
        let size = match weight_class(h.group) {
            0 | 1 => 1,
            2 | 3 => 2,
            4 => 3,
            _ => 4,
        };
        let pick = |rng: &mut Rng, group: i32, size: Option<i32>, words: &[&str]| -> Option<i32> {
            let fits = |m: &&Module| m.group == group && size.is_none_or(|s| m.size == s) && (words.is_empty() || words.iter().any(|w| m.name.contains(w)));
            let mut list: Vec<&Module> = self.modules.iter().filter(fits).collect();
            if list.is_empty() && !words.is_empty() {
                list = self.modules.iter().filter(|m| m.group == group && size.is_none_or(|s| m.size == s)).collect();
            }
            let best = if rich { list.iter().map(|m| m.meta).max().unwrap_or(0) } else { 0 };
            let tier: Vec<&&Module> = list.iter().filter(|m| m.meta == best).collect();
            let chosen = if tier.is_empty() { list.first().copied() } else { Some(*tier[rng.below(tier.len())]) };
            chosen.map(|m| m.type_id)
        };
        let words: &[&str] = match size {
            1 => &["Small", "1MN", "Micro"],
            2 => &["Medium", "10MN"],
            _ => &["Large", "100MN"],
        };
        let mut out: Vec<(i32, i64)> = Vec::new();
        let mut add = |t: Option<i32>, n: i64| {
            if let Some(t) = t.filter(|_| n > 0) {
                match out.iter_mut().find(|(o, _)| *o == t) {
                    Some(e) => e.1 += n,
                    None => out.push((t, n)),
                }
            }
        };
        let (hi, med, low) = h.slots;
        match h.group {
            GROUP_BARGE | GROUP_EXHUMER => add(pick(&mut rng, STRIP_MINER, None, &[]), hi.max(1) as i64),
            GROUP_INDUSTRIAL | GROUP_SHUTTLE => {}
            _ => {
                let (gun, ammo) = match h.race {
                    RACE_AMARR => (ENERGY_WEAPON, FREQUENCY_CRYSTAL),
                    RACE_MINMATAR => (PROJECTILE_WEAPON, PROJECTILE_AMMO),
                    _ => (HYBRID_WEAPON, HYBRID_CHARGE),
                };
                let guns = h.turrets.min(hi).max(1) as i64;
                add(pick(&mut rng, gun, Some(size), &[]), guns);
                let rounds = if ammo == FREQUENCY_CRYSTAL { guns } else { guns * 200 };
                add(pick(&mut rng, ammo, Some(size), &[]), rounds);
                // Miners in frigates mine with what highs are left.
                if hi as i64 > guns && rng.unit() < 0.3 {
                    add(pick(&mut rng, MINING_LASER, None, &[]), 1);
                }
            }
        }
        if med > 0 {
            add(pick(&mut rng, AFTERBURNER, None, words), 1);
            let shield = matches!(h.race, RACE_CALDARI | RACE_MINMATAR);
            if shield && med > 1 {
                add(pick(&mut rng, SHIELD_EXTENDER, None, words), (med as i64 - 2).max(1));
                add(pick(&mut rng, SHIELD_BOOSTER, None, words), 1);
            }
        }
        if low > 0 {
            match h.group {
                GROUP_INDUSTRIAL | GROUP_SHUTTLE | GROUP_BARGE | GROUP_EXHUMER => add(pick(&mut rng, DAMAGE_CONTROL, None, &[]), 1),
                _ => {
                    let armor = matches!(h.race, RACE_AMARR) || !matches!(h.race, RACE_CALDARI | RACE_MINMATAR) && rng.unit() < 0.7;
                    if armor {
                        add(pick(&mut rng, ARMOR_REPAIRER, None, words), 1);
                        add(pick(&mut rng, ARMOR_PLATE, None, words), (low as i64 - 1).min(2));
                    } else {
                        let boost = match h.race {
                            RACE_MINMATAR => GYROSTABILIZER,
                            RACE_AMARR => HEAT_SINK,
                            _ => MAGNETIC_STABILIZER,
                        };
                        add(pick(&mut rng, boost, None, &[]), (low as i64).min(2));
                        add(pick(&mut rng, DAMAGE_CONTROL, None, &[]), 1);
                    }
                }
            }
        }
        out
    }

    /// How much harder a fitting hits than a plain one: named guns do a
    /// little more.
    pub fn punch(&self, fitting: &[(i32, i64)]) -> f64 {
        let best = fitting
            .iter()
            .filter_map(|(t, _)| self.modules.iter().find(|m| m.type_id == *t && matches!(m.group, ENERGY_WEAPON | PROJECTILE_WEAPON | HYBRID_WEAPON)))
            .map(|m| m.meta)
            .max()
            .unwrap_or(0);
        1.0 + 0.05 * best as f64
    }
}

/// The tier a bot moves up to, if its wallet allows: when it holds the next
/// hull's price `CUSHION` times over.
pub const CUSHION: f64 = 6.0;
