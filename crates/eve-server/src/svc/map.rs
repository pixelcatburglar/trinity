use std::collections::HashMap;

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError, Tx};
use crate::svc::rows::rowset;

pub async fn map(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetSolarSystemPseudoSecurities" => pseudo_securities(ctx).await,
        "GetStuckSystems" => stuck_systems(),
        "GetHistory" => history(ctx, call.arg_id(0), call.arg_i64(1).unwrap_or(1)).await,
        "GetSolarSystemVisits" => solar_system_visits(ctx).await,
        "GetStationCount" => station_count(ctx).await,
        "GetDeadspaceComplexMap" => deadspace_complex_map(ctx).await,
        "GetBeaconCount" => crate::svc::jumps::beacon_count(ctx).await,
        "GetAllianceBeacons" => alliance_beacons(ctx).await,
        "GetLinkableJumpArrays" => super::bridges::linkable(ctx).await,
        "GetMyExtraMapInfo" => my_extra_map_info(),
        "GetMyExtraMapInfoAgents" => my_extra_map_info_agents(ctx).await,
        "GetSolarSystemLoadRatios" => load_ratios(),

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn alliance_beacons(ctx: &Ctx<'_>) -> Value {
    let Some(alliance) = ctx.session.alliance_id.filter(|id| *id > 0) else {
        return Value::List(vec![]);
    };
    // Capital navigation unpacks (solarSystemID, structureID, structureTypeID).
    // The beacons are the Cynosural Generator Arrays (group 838).
    let beacons = crate::sde::get().types_in_group(838);
    let mut found: Vec<(i64, i64, Vec<Value>)> = ctx.store.read(|db| {
        let entity = db.table("entity");
        let structures = db.table("posStructures");
        let mut found = Vec::new();
        for corp in db.table("corporation").find(&["allianceID"], &[store::Value::Int(alliance)]) {
            let Some(owner) = corp.int("corporationID") else { continue };
            for e in entity.find(&["ownerID"], &[store::Value::Int(owner)]) {
                if !e.int("typeID").is_some_and(|t| beacons.contains(&(t as i32))) {
                    continue;
                }
                let online = e
                    .int("itemID")
                    .and_then(|id| structures.get_id(id))
                    .is_some_and(|p| matches!(p.int("state"), Some(4 | 5)));
                if online {
                    found.push((e.int("locationID").unwrap_or(0), e.int("itemID").unwrap_or(0), e.line(&["locationID", "itemID", "typeID"])));
                }
            }
        }
        found
    });
    found.sort_by_key(|(location, item, _)| (*location, *item));
    Value::List(found.into_iter().map(|(_, _, line)| Value::Tuple(line)).collect())
}

async fn pseudo_securities(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["solarSystemID", "security"];
    let lines = ctx.store.read(|db| {
        db.table("mapSolarSystems").rows().filter(|r| !r.value("security").is_null()).map(|r| Value::List(r.line(HEADER))).collect()
    });
    rowset(HEADER, lines)
}

fn stuck_systems() -> Value {
    Value::Dict(Vec::new())
}

/// The tables are defined by the store's schema; nothing to bring up to
/// date.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

pub async fn record_visit(store: &Store, char_id: i64, system: i64, arrived: bool) -> Result<(), StoreError> {
    let now = crate::bulkdata::filetime_now();
    store.write(|tx| {
        let key = [store::Value::Int(char_id), store::Value::Int(system)];
        match tx.table("chrSystemVisits").get(&key).map(|r| (r.id, r.int("visits").unwrap_or(0))) {
            Some((id, visits)) => tx.update(
                "chrSystemVisits",
                id,
                &[("visits", store::Value::Int(visits + i64::from(arrived))), ("lastDateTime", store::Value::Int(now))],
            ),
            None => tx
                .insert(
                    "chrSystemVisits",
                    &[("characterID", key[0].clone()), ("solarSystemID", key[1].clone()), ("lastDateTime", store::Value::Int(now))],
                )
                .map(drop),
        }
    })
}

/// kind 1 is a jump; kind 2 an NPC ship destroyed.
pub async fn record_activity(store: &Store, system: i64, kind: i16) -> Result<(), StoreError> {
    store.write(|tx| {
        tx.insert(
            "mapActivity",
            &[
                ("solarSystemID", store::Value::Int(system)),
                ("kind", store::Value::Int(kind.into())),
                ("eventTime", store::Value::Int(crate::bulkdata::filetime_now())),
            ],
        )
        .map(drop)
    })
}

/// Per-system sums, in the order each system first appears (as a GROUP BY
/// without ORDER BY gave them).
#[derive(Default)]
struct BySystem {
    order: Vec<i64>,
    sums: HashMap<i64, [i64; 3]>,
}

impl BySystem {
    fn add(&mut self, system: i64, values: [i64; 3]) {
        let sums = self.sums.entry(system).or_insert_with(|| {
            self.order.push(system);
            [0; 3]
        });
        for (sum, v) in sums.iter_mut().zip(values) {
            *sum += v;
        }
    }

    fn lines(&self) -> Vec<Value> {
        self.order
            .iter()
            .map(|s| Value::List(std::iter::once(*s).chain(self.sums[s]).map(Value::Int).collect()))
            .collect()
    }
}

async fn history(ctx: &Ctx<'_>, kind: i64, hours: i64) -> Value {
    let header = &["solarSystemID", "value1", "value2", "value3"];
    let since = crate::bulkdata::filetime_now() - hours.clamp(1, 24) * 36_000_000_000;
    // The recent mapActivity rows of one kind, by system.
    let activity = |db: &Db, kind: i64| -> Vec<i64> {
        db.table("mapActivity")
            .rows()
            .filter(|r| r.int("kind") == Some(kind) && r.int("eventTime").is_some_and(|t| t >= since))
            .filter_map(|r| r.int("solarSystemID"))
            .collect()
    };
    match kind {
        1 => {
            let lines = ctx.store.read(|db| {
                let mut jumps = BySystem::default();
                for system in activity(db, 1) {
                    jumps.add(system, [1, 0, 0]);
                }
                jumps.lines()
            });
            rowset(header, lines)
        }
        3 => {
            // Ships, NPCs and pods destroyed: a kill counts as a ship or a
            // pod, an NPC kill as both a ship and an NPC.
            let lines = ctx.store.read(|db| {
                let mut kills = BySystem::default();
                for r in db.table("killLog").rows().filter(|r| r.int("killTime").is_some_and(|t| t >= since)) {
                    let Some(system) = r.int("solarSystemID") else { continue };
                    let pod = r.int("isPodKill").unwrap_or(0) != 0;
                    kills.add(system, if pod { [0, 0, 1] } else { [1, 0, 0] });
                }
                for system in activity(db, 2) {
                    kills.add(system, [1, 1, 0]);
                }
                kills.lines()
            });
            rowset(header, lines)
        }
        _ => rowset(header, vec![]),
    }
}

async fn solar_system_visits(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["solarSystemID", "visits", "lastDateTime"];
    let char_id = ctx.session.char_id.unwrap_or(0);
    let lines = ctx.store.read(|db| {
        let mut visits: Vec<_> = db.table("chrSystemVisits").find(&["characterID"], &[store::Value::Int(char_id)]).collect();
        visits.sort_by_key(|r| r.int("solarSystemID"));
        visits.iter().map(|r| Value::List(r.line(HEADER))).collect()
    });
    rowset(HEADER, lines)
}

async fn station_count(ctx: &Ctx<'_>) -> Value {
    // Stations without a system count together, as GROUP BY put them.
    let counts = ctx.store.read(|db| {
        let mut order: Vec<Option<i64>> = Vec::new();
        let mut counts: HashMap<Option<i64>, i64> = HashMap::new();
        for r in db.table("staStations").rows() {
            let system = r.int("solarSystemID");
            *counts.entry(system).or_insert_with(|| {
                order.push(system);
                0
            }) += 1;
        }
        order.into_iter().map(|s| (s, counts[&s])).collect::<Vec<_>>()
    });
    Value::List(counts.into_iter().map(|(system, count)| Value::Tuple(vec![Value::int_or_none(system), Value::Int(count)])).collect())
}

async fn deadspace_complex_map(ctx: &Ctx<'_>) -> Value {
    let found = ctx.store.read(|db| crate::complexes::map(db));
    Value::List(
        found
            .into_iter()
            .map(|(system, id, level, name)| {
                Value::Tuple(vec![
                    Value::Int(system),
                    Value::Int(id),
                    Value::Int(level),
                    Value::text(name),
                ])
            })
            .collect(),
    )
}

fn my_extra_map_info() -> Value {
    Value::Tuple(vec![Value::None, Value::None])
}

async fn my_extra_map_info_agents(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["fromID", "rank"];
    let Some(char_id) = ctx.session.char_id else {
        return rowset(HEADER, Vec::new());
    };
    let lines = ctx.store.read(|db| {
        db.table("chrNPCStandings")
            .find(&["characterID"], &[store::Value::Int(char_id)])
            .map(|r| Value::List(vec![r.cell("fromID"), r.float("standing").map_or(Value::None, |s| Value::Double(s / 10.0))]))
            .collect()
    });
    rowset(HEADER, lines)
}

fn load_ratios() -> Value {
    rowset(&["solarSystemID", "loadRatio"], Vec::new())
}
