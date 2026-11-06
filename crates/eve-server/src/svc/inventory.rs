use eve_marshal::Value;
use tracing::error;

use crate::items::{invitem_cells, invitem_lines};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row};
use crate::svc::bound;
use crate::svc::rows::{row, rowset};

const CONTAINER_WALLET: i64 = 10001;
const CONTAINER_GLOBAL: i64 = 10002;
const CONTAINER_SOLAR_SYSTEM: i64 = 10003;
pub const CONTAINER_HANGAR: i64 = 10004;
const CONTAINER_SCRAP_HEAP: i64 = 10005;
const CONTAINER_FACTORY: i64 = 10006;
const CONTAINER_BANK: i64 = 10007;
const CONTAINER_RECYCLER: i64 = 10008;
const CONTAINER_OFFICES: i64 = 10009;
const CONTAINER_STATION_CHARACTERS: i64 = 10010;
const CONTAINER_CHARACTER: i64 = 10011;
const CONTAINER_CORP_MARKET: i64 = 10012;

pub const FLAG_HANGAR: i32 = 4;
pub const FLAG_CORP_MARKET: i32 = 62;

const CATEGORY_BLUEPRINT: i64 = 9;

const STATION_ID_MIN: i64 = 60_000_000;
const STATION_ID_MAX: i64 = 64_000_000;

const UNLIMITED_CAPACITY: f64 = 9_000_000_000_000_000.0;

/// flagFactoryBlueprint: a blueprint installed in a job, out of every
/// listing until the job gives it back.
pub const FLAG_FACTORY_BLUEPRINT: i32 = 36;

/// What a blueprint item carries beyond the item row (GetSubContent reads
/// them off ListStationBlueprintItems); None for a blueprint never researched.
const BLUEPRINT_COLUMNS: &[&str] = &["copy", "materialLevel", "productivityLevel", "licensedProductionRunsRemaining"];

/// The client's item row columns.
pub const INVITEM_HEADER: &[&str] = &crate::items::INVITEM_COLUMNS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Special { container_id: i64, owner_id: Option<i64> },
    Item(i64),
}

const BROKER: &str = "invbroker";

const CONTAINER: &str = "invcontainer";

pub async fn invbroker(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if matches!(call.method.as_str(), "AssembleConstructionPlatform" | "AssembleCargoContainer") {
        return Some(match crate::svc::outposts::assemble(ctx, call.arg_id(0)).await {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        });
    }
    Some(broker_method(ctx, &call.method, &call.args))
}

pub async fn invcontainer(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let container = container_from_params(&bound::params_of(call));
    Some(container_method(ctx, container, call).await)
}

fn broker_method(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    match method {
        "GetInventory" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_SPECIAL, arg(0).unwrap_or(0), arg(1).unwrap_or(NO_OWNER)],
        )),
        "GetInventoryFromId" | "GetInventoryFromIdEx" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_ITEM, arg(0).unwrap_or(0), NO_OWNER],
        )),
        _ => Outcome::Unknown(format!("{BROKER}::{method}")),
    }
}

async fn container_method(ctx: &Ctx<'_>, container: Container, call: &Call) -> Outcome {
    if let Some(outcome) = crate::svc::moves::container_write(ctx, container, call).await {
        return outcome;
    }
    let (method, args) = (call.method.as_str(), &call.args);
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    // A starbase structure shows its contents only to those its settings let look.
    if let Container::Item(id) = container {
        use crate::svc::starbase::{may, Access};
        if method == "List" && may(ctx, id, Access::ViewPut).await == Some(false) && super::maintenance::view(ctx,id).await.is_none() {
            return Outcome::custom("You are not allowed to look inside that structure from here.");
        }
    }
    if let (Container::Item(id), "List") = (container, method) {
        if let Some(office) = ctx.store.read(|db| crate::svc::corphangar::office(db, id)) {
            if !crate::svc::corphangar::mine(ctx, office) {
                return Outcome::custom("That is not your corporation's office.");
            }
            return Outcome::Value(list(ctx,container).await);
        }
    }
    let out = match method {
        "List" => list(ctx, container).await,

        "ListStations" => {
            list_stations(ctx, arg(0).unwrap_or(0) != 0, arg(1).unwrap_or(0) != 0).await
        }
        "ListStationItems" => list_station_items(ctx, arg(0).unwrap_or(0), false, false).await,
        "ListStationBlueprintItems" => {
            list_station_items(ctx, arg(0).unwrap_or(0), true, arg(1).unwrap_or(0) != 0).await
        }

        "GetItem" => get_item(ctx, container).await,
        "GetCapacity" => get_capacity(ctx, container, arg(0)).await,
        // An outpost platform in space: construction starts.
        "Build" => {
            let Container::Item(id) = container else { return Outcome::custom("Only a platform can be built.") };
            return match crate::svc::outposts::build(ctx, id).await {
                Ok(()) => Outcome::Value(Value::None),
                Err(why) => Outcome::custom(why),
            };
        }

        _ => return Outcome::Unknown(format!("{CONTAINER}::{method} on {container:?}")),
    };
    Outcome::Value(out)
}

const KIND_SPECIAL: i64 = 0;
const KIND_ITEM: i64 = 1;
const NO_OWNER: i64 = -1;

fn container_from_params(params: &[i64]) -> Container {
    match (params.first().copied(), params.get(1).copied(), params.get(2).copied()) {
        (Some(KIND_ITEM), Some(item_id), _) => Container::Item(item_id),
        (Some(KIND_SPECIAL), Some(container_id), owner) => Container::Special {
            container_id,
            owner_id: owner.filter(|o| *o != NO_OWNER),
        },
        _ => Container::Special { container_id: CONTAINER_HANGAR, owner_id: None },
    }
}

/// Which items a container holds: those whose `columns` equal `values`, with
/// a flag perhaps excluded.
struct Scope {
    columns: &'static [&'static str],
    values: Vec<i64>,
    flag: FlagRule,
}

#[derive(Clone, Copy)]
enum FlagRule {
    Any,
    Not(i64),
}

impl Scope {
    fn rows<'d>(&self, db: &'d Db) -> impl Iterator<Item = Row<'d>> + 'd {
        let values: Vec<store::Value> = self.values.iter().map(|v| store::Value::Int(*v)).collect();
        let flag = self.flag;
        db.table("entity").find(self.columns, &values).filter(move |r| {
            let f = r.int("flag");
            match flag {
                FlagRule::Any => true,
                FlagRule::Not(x) => f.is_some_and(|f| f != x),
            }
        })
    }
}

fn contents_scope(ctx: &Ctx<'_>, container: Container) -> Option<Scope> {
    match container {
        Container::Item(item_id) => Some(Scope { columns: &["locationID"], values: vec![item_id], flag: FlagRule::Any }),
        Container::Special { container_id, owner_id } => match container_id {
            CONTAINER_HANGAR => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.char_id)?;
                Some(Scope {
                    columns: &["ownerID", "locationID"],
                    values: vec![owner, station],
                    flag: FlagRule::Not(i64::from(FLAG_FACTORY_BLUEPRINT)),
                })
            }
            CONTAINER_CORP_MARKET => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.corp_id)?;
                Some(Scope {
                    columns: &["ownerID", "locationID", "flag"],
                    values: vec![owner, station, i64::from(FLAG_CORP_MARKET)],
                    flag: FlagRule::Any,
                })
            }
            CONTAINER_GLOBAL => {
                let owner = ctx.session.char_id?;
                Some(Scope { columns: &["ownerID"], values: vec![owner], flag: FlagRule::Not(i64::from(FLAG_FACTORY_BLUEPRINT)) })
            }
            CONTAINER_CHARACTER => {
                let char_id = ctx.session.char_id?;
                Some(Scope { columns: &["locationID"], values: vec![char_id], flag: FlagRule::Any })
            }
            CONTAINER_WALLET
            | CONTAINER_SOLAR_SYSTEM
            | CONTAINER_SCRAP_HEAP
            | CONTAINER_FACTORY
            | CONTAINER_BANK
            | CONTAINER_RECYCLER
            | CONTAINER_OFFICES
            | CONTAINER_STATION_CHARACTERS => None,
            _ => None,
        },
    }
}

/// The items at `location` under `flag`, as listing rows.
fn items_at(ctx: &Ctx<'_>, location: i64, flag: i64) -> Vec<Value> {
    ctx.store.read(|db| invitem_lines(db.table("entity").find(&["locationID", "flag"], &[store::Value::Int(location), store::Value::Int(flag)])))
}

pub async fn list(ctx: &Ctx<'_>, container: Container) -> Value {
    if let Container::Item(id)=container {if ctx.store.read(|db|super::maintenance::is_array(db,id)).unwrap_or(false){
        if super::maintenance::view(ctx,id).await.is_none(){return rowset(INVITEM_HEADER,vec![]);}
        return rowset(INVITEM_HEADER,items_at(ctx,id,0));
    }}
    let ordinary=may_read(ctx,container).await;
    if !ordinary { if let Container::Item(id)=container {
        if Some(id)!=ctx.session.ship_id {
            if let Some((_,flag))=super::maintenance::view(ctx,id).await {
                return rowset(INVITEM_HEADER,items_at(ctx,id,flag));
            }
        }
    }}
    if !ordinary {
        return rowset(INVITEM_HEADER, Vec::new());
    }
    if let Container::Item(id) = container {
        if let Some((corp, _)) = ctx.store.read(|db| crate::svc::corphangar::office(db, id)) {
            let flags: Vec<i64> = crate::svc::corphangar::visible_flags(ctx).into_iter().map(i64::from).collect();
            let lines = ctx.store.read(|db| {
                invitem_lines(
                    db.table("entity")
                        .find(&["ownerID", "locationID"], &[store::Value::Int(corp), store::Value::Int(id)])
                        .filter(|r| r.int("flag").is_some_and(|f| flags.contains(&f))),
                )
            });
            return rowset(INVITEM_HEADER, lines);
        }
    }
    let Some(scope) = contents_scope(ctx, container) else {
        return rowset(INVITEM_HEADER, Vec::new());
    };
    let blueprint = i64::from(FLAG_FACTORY_BLUEPRINT);
    item_rows(ctx.store.read(|db| scope.rows(db).filter(|r| r.int("flag") != Some(blueprint)).filter_map(|r| invitem_cells(&r)).collect()))
}

/// A bound inventory reference is an address, not an access grant. In particular,
/// guessing another pilot's ship ID must not reveal its cargo or fitted modules.
async fn may_read(ctx: &Ctx<'_>, container: Container) -> bool {
    let Some(me) = ctx.session.char_id else { return false };
    let Container::Item(mut id) = container else {
        return match container {
            Container::Special { container_id:CONTAINER_HANGAR,owner_id } => ctx.session.station_id.is_some() && owner_id.unwrap_or(me)==me,
            Container::Special { container_id:CONTAINER_CORP_MARKET,owner_id } => ctx.session.station_id.is_some() && ctx.session.corp_id.is_some_and(|corp|owner_id.unwrap_or(corp)==corp),
            Container::Special { container_id:CONTAINER_GLOBAL|CONTAINER_CHARACTER,owner_id } => owner_id.unwrap_or(me)==me,
            _ => false,
        };
    };
    let mut own_chain = true;
    let mut corp_chain = true;
    let mut visited = std::collections::HashSet::new();
    for _ in 0..8 {
        if !visited.insert(id) { return false; }
        if let Some(office) = ctx.store.read(|db| crate::svc::corphangar::office(db,id)) {
            return crate::svc::corphangar::mine(ctx,office) && corp_chain;
        }
        if let Some(allowed) = crate::svc::starbase::may(ctx,id,crate::svc::starbase::Access::ViewPut).await {
            return allowed;
        }
        {
            let world = ctx.world.lock();
            if let Some(trade) = world.trades.get(&id) {
                return trade.traders.contains(&me) && ctx.session.station_id==Some(trade.station);
            }
        }
        if let (Some(system),Some(ship)) = (ctx.session.solar_system_id,ctx.session.ship_id) {
            if crate::loot::in_reach(ctx.world,system,ship,id) { return true; }
        }
        let row = ctx.store.read(|db| db.table("entity").get_id(id).map(|r| (r.int("ownerID"), r.int("locationID"), r.int("flag"))));
        let Some((Some(owner), Some(parent), Some(flag))) = row else { return false };
        if flag==i64::from(FLAG_FACTORY_BLUEPRINT) { return false; }
        own_chain &= owner==me;
        corp_chain &= Some(owner)==ctx.session.corp_id;
        if own_chain && (id==me || Some(id)==ctx.session.ship_id) { return true; }
        if let Some(office) = ctx.store.read(|db| crate::svc::corphangar::office(db,parent)) {
            return corp_chain && crate::svc::corphangar::mine(ctx,office) && crate::svc::corphangar::may_query(ctx,flag);
        }
        if ctx.session.station_id==Some(parent) { return own_chain; }
        id=parent;
    }
    false
}

pub async fn list_station_items(
    ctx: &Ctx<'_>,
    station_id: i64,
    blueprints_only: bool,
    for_corp: bool,
) -> Value {
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(INVITEM_HEADER, Vec::new()),
    };
    let divisions = for_corp.then(|| queryable_divisions(ctx, Some(station_id)));
    let blueprint_types = crate::sde::get().types_in_category(CATEGORY_BLUEPRINT);
    let header: Vec<&str> = INVITEM_HEADER.iter().chain(BLUEPRINT_COLUMNS).copied().collect();
    let lines = ctx.store.read(|db| {
        let entity = db.table("entity");
        // Items in the station itself or directly inside the owner's
        // containers there.
        let here = |location: Option<i64>| {
            location == Some(station_id)
                || location.and_then(|l| entity.get_id(l)).is_some_and(|c| c.int("locationID") == Some(station_id) && c.int("ownerID") == Some(owner))
        };
        let rows = entity.find(&["ownerID"], &[store::Value::Int(owner)]).filter(|r| {
            r.int("flag").is_some_and(|f| f != i64::from(FLAG_FACTORY_BLUEPRINT))
                && here(r.int("locationID"))
                && divisions.as_ref().is_none_or(|d| r.int("flag").is_some_and(|f| d.contains(&f)))
        });
        if !blueprints_only {
            return rows.filter_map(|r| invitem_cells(&r)).collect::<Vec<_>>();
        }
        let blueprints = db.table("invBlueprints");
        rows.filter(|r| r.int("typeID").is_some_and(|t| blueprint_types.iter().any(|b| i64::from(*b) == t)))
            .filter_map(|r| {
                let mut cells = invitem_cells(&r)?;
                let b = r.int("itemID").and_then(|id| blueprints.get_id(id));
                cells.extend(BLUEPRINT_COLUMNS.iter().map(|c| b.map_or(Value::None, |b| b.cell(c))));
                Some(cells)
            })
            .collect()
    });
    if !blueprints_only {
        return item_rows(lines);
    }
    rowset(&header, lines.into_iter().map(Value::List).collect())
}

pub async fn list_stations(ctx: &Ctx<'_>, blueprints_only: bool, for_corp: bool) -> Value {
    const HEADER: &[&str] = &["stationID", "itemCount"];
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(HEADER, Vec::new()),
    };
    let blueprint_types = crate::sde::get().types_in_category(CATEGORY_BLUEPRINT);
    // A corporation sees, per station, the divisions its roles there let it
    // query: those at its headquarters, its base, or anywhere else.
    let divisions = for_corp.then(|| {
        let hq = ctx.session.hq_id.unwrap_or(0);
        let base = ctx.session.base_id.unwrap_or(0);
        (hq, queryable_divisions(ctx, Some(hq)), base, queryable_divisions(ctx, Some(base)), queryable_divisions(ctx, None))
    });
    let counts = ctx.store.read(|db| {
        let mut counts: Vec<(i64, i64)> = Vec::new();
        for r in db.table("entity").find(&["ownerID"], &[store::Value::Int(owner)]) {
            let (Some(location), Some(flag)) = (r.int("locationID"), r.int("flag")) else { continue };
            if !(STATION_ID_MIN..STATION_ID_MAX).contains(&location) {
                continue;
            }
            if blueprints_only
                && (flag == 36 || !r.int("typeID").is_some_and(|t| blueprint_types.iter().any(|b| i64::from(*b) == t)))
            {
                continue;
            }
            if let Some((hq, at_hq, base, at_base, elsewhere)) = &divisions {
                let allowed = if location == *hq { at_hq } else if location == *base { at_base } else { elsewhere };
                if !allowed.contains(&flag) {
                    continue;
                }
            }
            match counts.iter_mut().find(|(l, _)| *l == location) {
                Some((_, n)) => *n += 1,
                None => counts.push((location, 1)),
            }
        }
        counts
    });
    rowset(HEADER, counts.into_iter().map(|(location, n)| Value::List(vec![Value::Int(location), Value::Int(n)])).collect())
}

/// The hangar divisions the caller's roles at `station` let it query.
fn queryable_divisions(ctx:&Ctx<'_>,station:Option<i64>)->Vec<i64> {
    crate::svc::corphangar::flags().filter(|flag|crate::svc::corphangar::may_query_at(ctx,*flag,station)).collect()
}

pub async fn get_item(ctx: &Ctx<'_>, container: Container) -> Value {
    let bay=match container{Container::Item(id)=>super::maintenance::view(ctx,id).await.is_some(),_=>false};
    if let Container::Item(id)=container {if !bay && ctx.store.read(|db|super::maintenance::is_array(db,id)).unwrap_or(false){return row(INVITEM_HEADER,vec![Value::None;INVITEM_HEADER.len()]);}}
    if !bay && !may_read(ctx,container).await { return row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]); }
    let item_id = match container {
        Container::Item(id) => id,
        Container::Special { .. } => match ctx.session.station_id {
            Some(s) => s,
            None => return row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
        },
    };
    match ctx.store.read(|db| db.table("entity").get_id(item_id).and_then(|r| invitem_cells(&r))) {
        Some(cells) => row(INVITEM_HEADER, cells),
        None => row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
    }
}

pub async fn get_capacity(ctx: &Ctx<'_>, container: Container, flag: Option<i64>) -> Value {
    const HEADER: &[&str] = &["capacity", "used"];
    if let Container::Item(id)=container {
        if let Some((capacity,bay_flag))=super::maintenance::view(ctx,id).await {
            if flag==Some(bay_flag) || (bay_flag==0 && flag.is_none()) {
                let used=ctx.store.read(|db|super::maintenance::used(db,id,bay_flag,None)).unwrap_or(0.0);
                return row(HEADER,vec![Value::Double(capacity),Value::Double(used)]);
            }
        }
        if ctx.store.read(|db|super::maintenance::is_array(db,id)).unwrap_or(false){return row(HEADER,vec![Value::Double(0.0),Value::Double(0.0)]);}
    }
    if !may_read(ctx,container).await { return row(HEADER,vec![Value::Double(0.0),Value::Double(0.0)]); }

    let capacity = match container {
        Container::Item(item_id) => {
            // A silo's tower may raise its capacity.
            let silo = match ctx.store.read(|db| crate::svc::moonmining::silo_capacity(db, item_id)) {
                Ok(silo) => silo,
                Err(e) => {
                    error!("invbroker GetCapacity silo: {err}", err = crate::logging::chain(&e));
                    None
                }
            };
            match silo {
                Some(capacity) => capacity,
                None => ctx
                    .store
                    .read(|db| db.table("entity").get_id(item_id).and_then(|r| r.int("typeID")))
                    .and_then(|type_id| crate::sde::table("invTypes").get("typeID", type_id))
                    .and_then(|t| t.float("capacity"))
                    .unwrap_or(0.0),
            }
        }
        Container::Special { .. } => UNLIMITED_CAPACITY,
    };

    let used = match contents_scope(ctx, container) {
        Some(scope) => {
            let mut visible: Option<Vec<i64>> = None;
            if let Container::Item(id) = container {
                if ctx.store.read(|db| crate::svc::corphangar::office(db,id)).is_some() {
                    visible = Some(crate::svc::corphangar::visible_flags(ctx).into_iter().map(i64::from).collect());
                }
            }
            // Items of a type the static data does not know take no room, as
            // joining on it left them out.
            let types = crate::sde::table("invTypes");
            ctx.store.read(|db| {
                scope
                    .rows(db)
                    .filter(|r| {
                        let f = r.int("flag");
                        visible.as_ref().is_none_or(|v| f.is_some_and(|f| v.contains(&f))) && flag.is_none_or(|want| f == Some(want))
                    })
                    .filter_map(|r| {
                        let type_id = r.int("typeID")?;
                        let t = types.get("typeID", type_id)?;
                        let group = t.int("groupID")? as i32;
                        Some(super::maintenance::unit_volume(type_id, group, r.int("singleton")? != 0, t.float("volume").unwrap_or(0.0)) * r.int("quantity")? as f64)
                    })
                    .sum()
            })
        }
        None => 0.0,
    };

    row(HEADER, vec![Value::Double(capacity), Value::Double(used)])
}

/// Item rows as a listing rowset.
fn item_rows(cells: Vec<Vec<Value>>) -> Value {
    rowset(INVITEM_HEADER, cells.into_iter().map(Value::List).collect())
}

/// Column positions in an item row (the client's const.ix*).
pub const IX_LOCATION_ID: i64 = 3;
pub const IX_QUANTITY: i64 = 7;

/// `OnItemChange(item, change)` arguments: the item as it is now and, keyed by
/// column position, the values it had before (invCache.py:111). An item whose
/// quantity is now 0 is gone; one whose old location is 0 is new.
pub fn item_change(cells: Vec<Value>, before: Vec<(i64, Value)>) -> Value {
    Value::Tuple(vec![
        row(INVITEM_HEADER, cells),
        Value::Dict(before.into_iter().map(|(ix, v)| (Value::Int(ix), v)).collect()),
    ])
}
