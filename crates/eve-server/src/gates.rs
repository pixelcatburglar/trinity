//! The star gate network, read once: how many jumps lie between systems.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use crate::sde;
use crate::store::Db;

static GATES: OnceLock<HashMap<i64, Vec<i64>>> = OnceLock::new();

/// Each system's neighbours across a gate.
pub type Graph = HashMap<i64, Vec<i64>>;

/// The gates are static data now, so the live data these were once read
/// from goes unused; the callers still pass it.
pub fn graph(_db: &Db) -> Option<&'static Graph> {
    Some(gates())
}

/// Each system's neighbours, in the order the data lists the jumps.
fn gates() -> &'static HashMap<i64, Vec<i64>> {
    GATES.get_or_init(|| {
        let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
        for r in sde::table("mapSolarSystemJumps").rows() {
            map.entry(r.int("fromSolarSystemID").unwrap_or(0)).or_default().push(r.int("toSolarSystemID").unwrap_or(0));
        }
        map
    })
}

/// The jumps from one system to another, if no more than `limit`.
pub fn jumps(_db: &Db, from: i64, to: i64, limit: i64) -> Option<i64> {
    hops(gates(), from, to, limit)
}

/// Every system within `limit` jumps of `from` (itself included), with its
/// distance, nearest first.
pub fn within(_db: &Db, from: i64, limit: i64) -> Vec<(i64, i64)> {
    reach(gates(), from, limit)
}

/// The systems to jump through from `from` to `to`, `to` last and `from`
/// left out; None if no gates lead there.
pub fn route(gates: &Graph, from: i64, to: i64) -> Option<Vec<i64>> {
    if from == to {
        return Some(Vec::new());
    }
    let mut came_from: HashMap<i64, i64> = HashMap::from([(from, from)]);
    let mut queue = VecDeque::from([from]);
    while let Some(at) = queue.pop_front() {
        for next in gates.get(&at).into_iter().flatten() {
            if came_from.contains_key(next) {
                continue;
            }
            came_from.insert(*next, at);
            if *next == to {
                let mut path = vec![to];
                let mut step = at;
                while step != from {
                    path.push(step);
                    step = came_from[&step];
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(*next);
        }
    }
    None
}

/// Every system within `limit` jumps, on a graph already loaded.
pub fn reach_on(gates: &Graph, from: i64, limit: i64) -> Vec<(i64, i64)> {
    reach(gates, from, limit)
}

/// Breadth-first, out to `limit` jumps.
fn hops(gates: &HashMap<i64, Vec<i64>>, from: i64, to: i64, limit: i64) -> Option<i64> {
    reach(gates, from, limit).into_iter().find(|(s, _)| *s == to).map(|(_, n)| n)
}

fn reach(gates: &HashMap<i64, Vec<i64>>, from: i64, limit: i64) -> Vec<(i64, i64)> {
    let mut seen = HashSet::from([from]);
    let mut queue = VecDeque::from([(from, 0)]);
    let mut out = Vec::new();
    while let Some((at, n)) = queue.pop_front() {
        out.push((at, n));
        if n == limit {
            continue;
        }
        for next in gates.get(&at).into_iter().flatten() {
            if seen.insert(*next) {
                queue.push_back((*next, n + 1));
            }
        }
    }
    out
}
