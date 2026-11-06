mod offices;
pub(crate) use offices::{ensure_schema as ensure_office_schema,ensure_slots_locked as ensure_office_slots_locked};

use eve_marshal::Value;
use tracing::error;

use crate::items::{invitem_lines, INVITEM_COLUMNS};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, StoreError};
use crate::svc::bound;
use crate::svc::inventory::FLAG_CORP_MARKET;
use crate::svc::rows::{index_rowset, row, rowset};

pub async fn registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(corp_registry(ctx, &call.method, &call.args).await)
}

pub async fn station_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or(ctx.session.station_id)
        .unwrap_or(0);
    if let Some(done) = crate::svc::outposts::station_mgr(ctx, &call.method, &call.args, station).await {
        return Some(done.map_or_else(Outcome::custom, Outcome::Value));
    }
    Some(corp_station_mgr(ctx, &call.method, &call.args, Some(station)).await)
}

pub async fn corpmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetCorporations" | "GetPublicInfo" => corporation_row(ctx, arg0).await,
        "GetCorporationIDForCharacter" => corporation_id_for_character(ctx, arg0).await,
        "GetCorporationWallet" => {
            return Some(Outcome::Unknown(
                "corpmgr::GetCorporationWallet needs a bound corp wallet object".into(),
            ))
        }
        "GetCorporationStations" => {
            let corp = ctx.session.corp_id.unwrap_or(0);
            let mut stations: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
                db.table("staStations")
                    .find(&["corporationID"], &[store::Value::Int(corp)])
                    .map(|r| (r.int("stationID").unwrap_or(0), r.line(&["stationID", "solarSystemID", "stationTypeID"])))
                    .collect()
            });
            stations.sort_by_key(|(id, _)| *id);
            rowset(&["itemID", "locationID", "typeID"], stations.into_iter().map(|(_, l)| Value::List(l)).collect())
        }
        "GetItemsRented" => return Some(match items_rented(ctx).await {
            Ok(value)=>Outcome::Value(value),Err(error)=>{error!("corporation rentals: {}",crate::logging::chain(&error));Outcome::custom("The corporation's rentals could not be read.")}
        }),
        "SearchAssets" => search_assets(ctx,call).await,
        // (corpID, which): the stations holding the corporation's assets of a kind.
        "GetAssetInventory" => {
            let which = call.arg_text(1).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_locations(ctx, corp, &which).await,
                None => rowset(&["locationID"], Vec::new()),
            }
        }
        // (corpID, locationID, which): those assets at one station.
        "GetAssetInventoryForLocation" => {
            let which = call.arg_text(2).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_items(ctx, corp, call.arg_id(1), &which).await,
                None => rowset(&INVITEM_COLUMNS, Vec::new()),
            }
        }
        // (memberID, fromDate, toDate, rowsPerPage): item and role audit trails.
        // Neither is recorded yet.
        "AuditMember" => Value::Tuple(vec![
            rowset(&["eventDateTime", "eventTypeID", "actorID", "itemID", "typeID", "quantity", "locationID"], Vec::new()),
            rowset(&["changeTime", "characterID", "issuerID", "oldRoles", "newRoles", "grantable"], Vec::new()),
        ]),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The corporation ID if it is the caller's own; assets are not public.
fn own_corp(ctx: &Ctx<'_>, corp_id: i64) -> Option<i64> {
    (ctx.session.corp_id == Some(corp_id)).then_some(corp_id)
}

/// The corporation asset and wallet views share these rental records.
async fn items_rented(ctx: &Ctx<'_>) -> Result<Value,StoreError> {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let header = ["typeID", "rentedFromID", "startDate", "rentPeriodInDays", "flag", "periodCost", "balanceDueDate"];
    // Earlier builds created offices without rentalInfo. Preserve existing
    // history while linking those offices to their stations' rental slots.
    let mut stations:Vec<i64>=ctx.store.read(|db|db.table("crpOffices").find(&["corporationID"],&[store::Value::Int(corp)])
        .filter(|o|o.value("rentalSlot").is_null()).filter_map(|o|o.int("stationID")).collect());
    stations.sort_unstable();stations.dedup();
    if !stations.is_empty() {
        ctx.store.write(|tx|{
            for station in stations {
                if tx.table("staStations").get_id(station).is_some() {
                    ensure_office_slots_locked(tx,station)?;
                }
            }
            Ok::<_,StoreError>(())
        })?;
    }
    let mut rented:Vec<((i64,i64),Vec<Value>)>=ctx.store.read(|db|db.table("rentalInfo").find(&["renterID"],&[store::Value::Int(corp)])
        .filter(|r|r.int("renterID").is_some_and(|renter|renter>0))
        .map(|r|((r.int("stationID").unwrap_or(0),r.int("slotNumber").unwrap_or(0)),
            r.line(&["typeID","stationID","startDate","rentPeriodInDays","slotNumber","periodCost","balanceDueDate"]))).collect());
    rented.sort_by_key(|(key,_)|*key);
    Ok(rowset(&header,rented.into_iter().map(|(_,l)|Value::List(l)).collect()))
}

/// One kind of corporation asset.
#[derive(Clone, Copy)]
enum AssetScope {
    /// In the corporation's offices.
    Offices,
    /// In space, outside the corporation market deliveries.
    Property,
    Deliveries,
}

fn asset_scope(which: &str) -> Option<AssetScope> {
    Some(match which {
        "offices" => AssetScope::Offices,
        "property" => AssetScope::Property,
        "deliveries" => AssetScope::Deliveries,
        _ => return None, // "junk", "jobs": nothing tracks these yet
    })
}

/// The corporation's items of one kind of asset.
fn scoped_assets<'d>(db: &'d Db, corp: i64, scope: AssetScope) -> impl Iterator<Item = store::Row<'d>> + 'd {
    let offices = db.table("crpOffices");
    let systems = db.table("mapSolarSystems");
    db.table("entity").find(&["ownerID"], &[store::Value::Int(corp)]).filter(move |e| {
        let location = e.value("locationID");
        let flag = e.int("flag");
        match scope {
            AssetScope::Offices => offices.find(&["itemID"], std::slice::from_ref(location)).any(|o| o.int("corporationID") == Some(corp)),
            AssetScope::Property => flag != Some(i64::from(FLAG_CORP_MARKET)) && systems.get(std::slice::from_ref(location)).is_some(),
            AssetScope::Deliveries => flag == Some(i64::from(FLAG_CORP_MARKET)),
        }
    })
}

/// The distinct places `items` are, ascending: an item in one of the
/// corporation's offices is at the office's station.
fn asset_location_rowset<'d>(db: &'d Db, corp: i64, items: impl Iterator<Item = store::Row<'d>>) -> Value {
    let offices = db.table("crpOffices");
    let mut locations = std::collections::BTreeSet::new();
    for e in items {
        let location = e.value("locationID");
        let stations: Vec<i64> = offices
            .find(&["itemID"], std::slice::from_ref(location))
            .filter(|o| o.int("corporationID") == Some(corp))
            .filter_map(|o| o.int("stationID"))
            .collect();
        if stations.is_empty() {
            locations.extend(location.int());
        } else {
            locations.extend(stations);
        }
    }
    rowset(&["locationID"], locations.into_iter().map(|l| Value::List(vec![Value::Int(l)])).collect())
}

async fn search_assets(ctx:&Ctx<'_>,call:&Call)->Value {
    let empty=||rowset(&["locationID"],vec![]);
    let Some(corp)=ctx.session.corp_id else {return empty()};
    let Some(scope)=asset_scope(&call.arg_text(0).unwrap_or_default()) else {return empty()};
    let criterion=|i|call.arg_i64(i).and_then(|n|i32::try_from(n).ok()).filter(|n|*n>0);
    let category=criterion(1);let group=criterion(2);let ty=criterion(3);let quantity=criterion(4).unwrap_or(0);
    if category.is_none() && group.is_none() && ty.is_none() {return empty()}
    let types=searched_types(category,group,ty);
    ctx.store.read(|db|asset_location_rowset(db,corp,scoped_assets(db,corp,scope).filter(|e|
        e.int("typeID").is_some_and(|t|types.contains(&(t as i32))) && e.int("quantity").is_some_and(|q|q>=i64::from(quantity)))))
}

/// The types an asset search's category, group and type criteria (at least
/// one given) let through; a type whose group is unknown matches none.
fn searched_types(category:Option<i32>,group:Option<i32>,ty:Option<i32>)->Vec<i32> {
    let sde=crate::sde::get();
    let candidates:Vec<i32>=match (ty,group,category) {
        (Some(t),_,_)=>vec![t],
        (None,Some(g),_)=>sde.types_in_group(g as i64).to_vec(),
        (None,None,Some(c))=>sde.types_in_category(c as i64).to_vec(),
        (None,None,None)=>Vec::new(),
    };
    candidates.into_iter().filter(|t|{
        let Some((g,c))=sde.group_and_category(*t as i64) else {return false};
        category.is_none_or(|x|x as i64==c) && group.is_none_or(|x|x as i64==g)
    }).collect()
}

async fn asset_locations(ctx: &Ctx<'_>, corp: i64, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(&["locationID"], Vec::new()) };
    // Office contents sit in the office item; report the station it is in.
    ctx.store.read(|db| asset_location_rowset(db, corp, scoped_assets(db, corp, scope)))
}

async fn asset_items(ctx: &Ctx<'_>, corp: i64, station_id: i64, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(&INVITEM_COLUMNS, Vec::new()) };
    let lines = ctx.store.read(|db| {
        let offices = db.table("crpOffices");
        let mut items: Vec<store::Row> = scoped_assets(db, corp, scope)
            .filter(|e| {
                let location = e.int("locationID");
                location == Some(station_id)
                    || offices
                        .find(&["itemID"], std::slice::from_ref(e.value("locationID")))
                        .any(|o| o.int("corporationID") == Some(corp) && o.int("stationID") == Some(station_id))
            })
            .collect();
        items.sort_by_key(|e| e.int("itemID"));
        invitem_lines(items)
    });
    rowset(&INVITEM_COLUMNS, lines)
}

pub async fn corporation_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetNPCDivisions" => npc_divisions(),
        "GetEmploymentRecord" => employment_record(ctx, arg0).await,
        "GetCorpInfo" => corp_market_info(),
        "GetFactionInfo" => faction_info(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn corp_registry(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg0 = args.first().and_then(|v| v.as_i64());
    let corp = ctx.session.corp_id.unwrap_or(0);

    let out = match method {
        "GetCorporation" => corporation_row(ctx, corp).await,
        "GetEveOwners" => eve_owners(ctx, corp).await,
        "GetMember" => member(ctx, arg0.unwrap_or(0)).await,
        "GetRoles" => {
            let roles = roles();
            rowset(ROLE_HEADER, roles.iter().map(role_line).collect())
        }
        "GetRoleGroups" => role_groups(),
        "GetSharesByShareholder" => shares_by_shareholder(ctx, arg0.unwrap_or(0) != 0).await,
        "GetInfoWindowDataForChar" => info_window_data_for_char(ctx, arg0.unwrap_or(0)).await,
        "GetTitles" => index_rowset(TITLE_HEADER, "titleID", Vec::new()),
        // The stations holding items locked down by a corporation vote.
        "GetLockedItemLocations" => crate::svc::votes::locked_locations(ctx).await,
        "GetMemberIDsWithMoreThanAvgShares" => Value::List(Vec::new()),

        "GetMembers" => members_rowset(ctx, corp).await,

        _ => return Outcome::Unknown(format!("corpRegistry::{method}")),
    };
    Outcome::Value(out)
}

const CORP_HEADER: &[&str] = &[
    "corporationID", "corporationName", "description", "tickerName", "url", "taxRate",
    "minimumJoinStanding", "corporationType", "hasPlayerPersonnelManager",
    "sendCharTerminationMessage", "creatorID", "ceoID", "stationID", "raceID", "allianceID",
    "shares", "memberCount", "memberLimit", "allowedMemberRaceIDs", "graphicID", "shape1",
    "shape2", "shape3", "color1", "color2", "color3", "typeface", "division1", "division2",
    "division3", "division4", "division5", "division6", "division7", "balance", "deleted",
    "walletDivision1", "walletDivision2", "walletDivision3", "walletDivision4",
    "walletDivision5", "walletDivision6", "walletDivision7",
];

/// A corporation row as the client's corporation line (`CORP_HEADER`).
/// NPC corporations have no founder (creatorID 0, which no owner record
/// matches); their CEO stands in, so the info window shows "CEO and Founder".
/// No wallet division names are kept.
fn corp_line(r: &store::Row) -> Vec<Value> {
    let mut line = r.line(&CORP_HEADER[..36]);
    if r.int("creatorID") == Some(0) {
        line[10] = r.cell("ceoID");
    }
    line.resize(CORP_HEADER.len(), Value::None);
    line
}

async fn corporation_row(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    ctx.store.read(|db| match db.table("corporation").get_id(corp_id) {
        Some(r) => row(CORP_HEADER, corp_line(&r)),
        None => Value::None,
    })
}

async fn eve_owners(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let lines = ctx.store.read(|db| {
        db.table("character_")
            .find(&["corporationID"], &[store::Value::Int(corp_id)])
            .map(|r| Value::List(r.line(&["characterID", "characterName", "typeID"])))
            .collect()
    });
    rowset(HEADER, lines)
}

const MEMBER_HEADER: &[&str] = &[
    "characterID", "corporationID", "title", "startDateTime", "roles", "grantableRoles",
    "rolesAtHQ", "grantableRolesAtHQ", "rolesAtBase", "grantableRolesAtBase", "rolesAtOther",
    "grantableRolesAtOther", "baseID", "titleMask", "divisionID", "squadronID",
];

async fn member(ctx: &Ctx<'_>, char_id: i64) -> Value {
    match member_lines(ctx, MemberScope::One(char_id)).await.pop() {
        Some((_, line)) => row(MEMBER_HEADER, line),
        None => Value::None,
    }
}

enum MemberScope {
    One(i64),
    Corporation(i64),
}

/// Member rows (character ID, line), in character ID order.
async fn member_lines(ctx: &Ctx<'_>, scope: MemberScope) -> Vec<(i64, Vec<Value>)> {
    ctx.store.read(|db| {
        let characters = db.table("character_");
        let members: Vec<store::Row> = match scope {
            MemberScope::One(id) => characters.get_id(id).into_iter().collect(),
            MemberScope::Corporation(id) => characters.find(&["corporationID"], &[store::Value::Int(id)]).collect(),
        };
        let mut lines: Vec<(i64, Vec<Value>)> = members
            .into_iter()
            .filter_map(|c| {
                let corp = db.table("corporation").get(std::slice::from_ref(c.value("corporationID")))?;
                let roles = db.table("chrCorporationRoles").get(std::slice::from_ref(c.value("characterID")));
                let mask = |column: &str| Value::Long(roles.and_then(|r| r.int(column)).unwrap_or(0));
                let mut line = c.line(&["characterID", "corporationID", "title", "corporationDateTime"]);
                line.extend(
                    [
                        "rolesAtAll", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
                        "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
                    ]
                    .map(mask),
                );
                // baseID, titleMask, divisionID, squadronID.
                line.extend([corp.cell("stationID"), Value::Long(0), Value::Int(1), Value::Int(1)]);
                Some((c.int("characterID").unwrap_or(0), line))
            })
            .collect();
        lines.sort_by_key(|(id, _)| *id);
        lines
    })
}

/// Bound object name of a corporation's member list.
const MEMBERS_PROVIDER: &str = "corpMembers";

/// GetMembers: a util.SparseRowset over a server-side provider, which the
/// client pages through with Fetch and FetchByKey (rowset.py).
async fn members_rowset(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    let count = member_lines(ctx, MemberScope::Corporation(corp_id)).await.len() as i64;
    let provider = bound::object_ref_with(
        ctx.node_id,
        MEMBERS_PROVIDER,
        &[corp_id],
        vec![("realRowCount", Value::Int(count)), ("header", crate::svc::rows::header_value(MEMBER_HEADER))],
    );
    Value::Instance {
        class: Box::new(Value::bytes("util.SparseRowset")),
        state: Box::new(Value::Tuple(vec![crate::svc::rows::header_value(MEMBER_HEADER), provider, Value::Int(count)])),
    }
}

/// The member list provider: Fetch(start, count) gives [(pk, line)],
/// FetchByKey(pks) gives [(pk, position, line)].
pub async fn members_provider(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let corp_id = bound::parse(&call.service).and_then(|b| b.params.first().copied()).unwrap_or(0);
    if !matches!(call.method.as_str(), "Fetch" | "FetchByKey") {
        return None;
    }
    if ctx.session.corp_id != Some(corp_id) {
        return Some(Outcome::custom("That is not your corporation's member list."));
    }
    let lines = member_lines(ctx, MemberScope::Corporation(corp_id)).await;
    let value = if call.method == "Fetch" {
        let start = call.arg_i64(0).unwrap_or(0).max(0) as usize;
        let size = call.arg_i64(1).unwrap_or(0).max(0) as usize;
        lines
            .into_iter()
            .skip(start)
            .take(size)
            .map(|(pk, line)| Value::Tuple(vec![Value::Int(pk), Value::List(line)]))
            .collect()
    } else {
        let wanted: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect();
        lines
            .into_iter()
            .enumerate()
            .filter(|(_, (pk, _))| wanted.contains(pk))
            .map(|(i, (pk, line))| Value::Tuple(vec![Value::Int(pk), Value::Int(i as i64), Value::List(line)]))
            .collect()
    };
    Some(Outcome::Value(Value::List(value)))
}

const ROLE_HEADER: &[&str] = &["roleID", "roleName", "shortDescription", "description"];

struct CorpRole {
    id: i64,
    name: String,
    short: String,
    long: String,
}

fn role_line(r: &CorpRole) -> Value {
    Value::List(vec![
        Value::Long(r.id),
        Value::Bytes(r.name.clone().into_bytes()),
        Value::Bytes(r.short.clone().into_bytes()),
        Value::Bytes(r.long.clone().into_bytes()),
    ])
}

fn role_text(name: &str) -> Option<(String, String)> {
    let divisional = |prefix: &str| -> Option<u32> {
        let d = name.strip_prefix(prefix)?;
        match d.parse::<u32>() {
            Ok(n) if (1..=7).contains(&n) && d.len() == 1 => Some(n),
            _ => None,
        }
    };
    if let Some(n) = divisional("corpRoleHangarCanTake") {
        return Some((
            format!("Hangar Take Division {n}"),
            format!("May take items out of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleHangarCanQuery") {
        return Some((
            format!("Hangar Query Division {n}"),
            format!("May look at the contents of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleAccountCanTake") {
        return Some((
            format!("Account Take Division {n}"),
            format!("May withdraw funds from corporation account division {n}"),
        ));
    }
    let general = match name {
        "corpRoleDirector" => ("Director", "Has every role in the corporation"),
        "corpRolePersonnelManager" => {
            ("Personnel Manager", "May accept and reject applications to the corporation")
        }
        "corpRoleAccountant" => ("Accountant", "May view the corporation wallet and journal"),
        "corpRoleSecurityOfficer" => {
            ("Security Officer", "May set corporation standings and declare war")
        }
        "corpRoleFactoryManager" => {
            ("Factory Manager", "May rent and operate the corporation's factory slots")
        }
        "corpRoleStationManager" => {
            ("Station Manager", "May manage the corporation's stations")
        }
        "corpRoleCanRentOffice" => ("Rent Office", "May rent offices for the corporation"),
        "corpRoleAuditor" => ("Auditor", "May read the corporation's member audit log"),
        _ => return None,
    };
    Some((general.0.to_string(), general.1.to_string()))
}

/// The corpRole constants (all but the 0xFFFFFFFF mask), by value.
fn roles() -> Vec<CorpRole> {
    let mut constants: Vec<(String, i64)> = crate::sde::table("eveConstants")
        .rows()
        .filter_map(|r| Some((r.text("constantID")?, r.int("constantValue")?)))
        .filter(|(name, value)| name.starts_with("corpRole") && *value != 4294967295)
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    constants.sort_by_key(|(_, value)| *value);
    let mut out = Vec::new();
    for (name, id) in constants {
        if let Some((short, long)) = role_text(&name) {
            out.push(CorpRole { id, name, short, long });
        }
    }
    out
}

const ROLE_GROUP_HEADER: &[&str] =
    &["roleGroupID", "roleGroupName", "roleMask", "isDivisional", "appliesTo", "appliesToGrantable"];

fn role_groups() -> Value {
    let roles = roles();
    let mask_of = |pred: &dyn Fn(&str) -> bool| -> i64 {
        roles.iter().filter(|r| pred(&r.name)).fold(0i64, |acc, r| acc | r.id)
    };
    let general = mask_of(&|n: &str| {
        !n.starts_with("corpRoleHangarCan") && !n.starts_with("corpRoleAccountCan")
    });
    let hangar = mask_of(&|n: &str| n.starts_with("corpRoleHangarCan"));
    let account = mask_of(&|n: &str| n.starts_with("corpRoleAccountCan"));

    let groups: &[(i64, &str, i64, i64, &str, &str)] = &[
        (0, "General Roles", general, 0, "roles", "grantableRoles"),
        (1, "Accounting Roles", account, 0, "roles", "grantableRoles"),
        (2, "Hangar Roles at HQ", hangar, 1, "rolesAtHQ", "grantableRolesAtHQ"),
        (4, "Hangar Roles at Base", hangar, 1, "rolesAtBase", "grantableRolesAtBase"),
        (6, "Hangar Roles at Other", hangar, 1, "rolesAtOther", "grantableRolesAtOther"),
    ];
    let lines = groups
        .iter()
        .map(|(id, name, mask, divisional, applies, grantable)| {
            Value::List(vec![
                Value::Int(*id),
                Value::Bytes(name.as_bytes().to_vec()),
                Value::Long(*mask),
                Value::Int(*divisional),
                Value::Bytes(applies.as_bytes().to_vec()),
                Value::Bytes(grantable.as_bytes().to_vec()),
            ])
        })
        .collect();
    rowset(ROLE_GROUP_HEADER, lines)
}

async fn shares_by_shareholder(ctx: &Ctx<'_>, corp_shares: bool) -> Value {
    const HEADER: &[&str] = &["corporationID", "shares"];
    if corp_shares {
        return rowset(HEADER, Vec::new());
    }
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let lines = ctx.store.read(|db| {
        db.table("crpCharShares")
            .find(&["characterID"], &[store::Value::Int(char_id)])
            .map(|r| Value::List(r.line(&["corporationID", "shares"])))
            .collect()
    });
    rowset(HEADER, lines)
}

async fn info_window_data_for_char(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "corpID", "allianceID", "title", "title1", "title2", "title3", "title4", "title5",
        "title6", "title7", "title8",
    ];
    ctx.store.read(|db| {
        let Some(c) = db.table("character_").get_id(char_id) else { return Value::None };
        let Some(corp) = db.table("corporation").get(std::slice::from_ref(c.value("corporationID"))) else { return Value::None };
        let mut line = vec![c.cell("corporationID"), corp.cell("allianceID"), c.cell("title")];
        line.resize(HEADER.len(), Value::None);
        row(HEADER, line)
    })
}

async fn corporation_id_for_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    ctx.store.read(|db| db.table("character_").get_id(char_id).map_or(Value::None, |c| c.cell("corporationID")))
}

fn npc_divisions() -> Value {
    const HEADER: &[&str] = &["divisionID", "divisionName", "description", "leaderType"];
    let mut divisions: Vec<_> = crate::sde::table("crpNPCDivisions").rows().collect();
    divisions.sort_by_key(|r| r.int("divisionID"));
    rowset(HEADER, divisions.iter().map(|r| Value::List(r.line(HEADER))).collect())
}

async fn employment_record(ctx: &Ctx<'_>, char_id: i64) -> Value {
    // `deleted` is whether the corporation has closed: the info window marks
    // it so (infosvc.py GetEmploymentHistorySubContent).
    const HEADER: &[&str] = &["corporationID", "startDate", "deleted"];
    let mut lines: Vec<(i64, Vec<Value>)> = ctx.store.read(|db| {
        let corporations = db.table("corporation");
        db.table("chrEmployment")
            .find(&["characterID"], &[store::Value::Int(char_id)])
            .filter(|e| e.int("deleted") == Some(0))
            .map(|e| {
                let deleted = corporations.get(std::slice::from_ref(e.value("corporationID"))).and_then(|c| c.int("deleted")).unwrap_or(0);
                (e.int("startDate").unwrap_or(0), vec![e.cell("corporationID"), e.cell("startDate"), Value::Int(deleted)])
            })
            .collect()
    });
    lines.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
    rowset(HEADER, lines.into_iter().map(|(_, l)| Value::List(l)).collect())
}

fn corp_market_info() -> Value {
    const HEADER: &[&str] = &[
        "typeID", "sellPrice", "sellQuantity", "sellDateTime", "sellStationID", "buyPrice",
        "buyQuantity", "buyDateTime", "buyStationID",
    ];
    rowset(HEADER, Vec::new())
}

async fn faction_info(ctx: &Ctx<'_>) -> Value {
    let by_corp = static_pairs("crpNPCCorporations", "corporationID", "factionID");
    let regions = static_lists("mapRegions", "factionID", "regionID");
    let constellations = static_lists("mapConstellations", "factionID", "constellationID");
    let solar_systems = ctx.store.read(|db| {
        int_lists(db.table("mapSolarSystems").rows().filter_map(|r| Some((r.int("factionID")?, r.cell("solarSystemID")))))
    });
    let races = static_lists("factionRaces", "factionID", "raceID");
    let station_count = static_pairs("chrFactions", "factionID", "stationCount");
    let system_count = static_pairs("chrFactions", "factionID", "stationSystemCount");

    const NPC_HEADER: &[&str] = &["corporationID", "size", "extent"];
    let npc_info = crate::sde::table("crpNPCCorporations")
        .rows()
        .map(|r| (r.cell("corporationID"), row(NPC_HEADER, r.line(NPC_HEADER))))
        .collect();

    Value::Tuple(vec![
        Value::Dict(by_corp),
        Value::Dict(regions),
        Value::Dict(constellations),
        Value::Dict(solar_systems),
        Value::Dict(races),
        Value::Dict(station_count),
        Value::Dict(system_count),
        Value::Dict(npc_info),
    ])
}

/// A static table's `key: value` pairs, in dump order, leaving out rows
/// where either is NULL.
fn static_pairs(table: &str, key: &str, value: &str) -> Vec<(Value, Value)> {
    crate::sde::table(table)
        .rows()
        .filter(|r| !r.value(key).is_null() && !r.value(value).is_null())
        .map(|r| (r.cell(key), r.cell(value)))
        .collect()
}

/// A static table's values listed under each (non-NULL) integer key, keys
/// in the order they first appear in the dump, as [`int_lists`] gives them.
fn static_lists(table: &str, key: &str, value: &str) -> Vec<(Value, Value)> {
    let mut keys: Vec<i64> = Vec::new();
    let mut vals: Vec<Vec<Value>> = Vec::new();
    for r in crate::sde::table(table).rows() {
        let Some(k) = r.int(key) else { continue };
        let v = r.cell(value);
        match keys.iter().position(|x| *x == k) {
            Some(i) => vals[i].push(v),
            None => {
                keys.push(k);
                vals.push(vec![v]);
            }
        }
    }
    keys.into_iter().zip(vals).map(|(k, v)| (Value::Int(k), Value::List(v))).collect()
}

/// Values listed under each key, keys in the order they first appear.
fn int_lists(pairs: impl Iterator<Item = (i64, Value)>) -> Vec<(Value, Value)> {
    let mut keys: Vec<i64> = Vec::new();
    let mut vals: Vec<Vec<Value>> = Vec::new();
    for (k, v) in pairs {
        match keys.iter().position(|x| *x == k) {
            Some(i) => vals[i].push(v),
            None => {
                keys.push(k);
                vals.push(vec![v]);
            }
        }
    }
    keys.into_iter().zip(vals).map(|(k, v)| (Value::Int(k), Value::List(v))).collect()
}

const TITLE_HEADER: &[&str] = &[
    "titleID", "titleName", "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

async fn corp_station_mgr(
    ctx: &Ctx<'_>,
    method: &str,
    args: &[Value],
    station: Option<i64>,
) -> Outcome {
    let station = station.or(ctx.session.station_id).unwrap_or(0);
    let arg0 = args.first().and_then(|v| v.as_i64());

    let out = match method {
        "GetCorporateStationInfo" => corporate_station_info(ctx, station).await,

        "GetQuoteForRentingAnOffice" => return offices::quote(ctx,station,args).await,

        "DoesPlayersCorpHaveJunkAtStation" => Value::Int(0),
        "GetQuoteForGettingCorpJunkBack" => Value::Double(0.0),

        "DoStandingCheckForStationService" => Value::None,

        "GetPotentialHomeStations" => potential_home_stations(ctx, station).await,

        "AreAnyFactorySlotsAvailableForRenting"
        | "AreAnyResearchFacilitySlotsAvailableForRenting" => Value::Int(0),
        "GetQuoteForRentingAFactorySlot" | "GetQuoteForRentingAResearchFacilitySlot" => {
            Value::Double(0.0)
        }

        "SetCloneTypeID" => return crate::svc::medical::set_clone_type(ctx, arg0.unwrap_or(0)).await,
        "SetHomeStation" => return crate::svc::medical::set_home_station(ctx, arg0.unwrap_or(0)).await,
        "RentOffice" => return offices::rent(ctx,station,args).await,

        _ => return Outcome::Unknown(format!("corpStationMgr::{method}")),
    };
    Outcome::Value(out)
}

async fn corporate_station_info(ctx: &Ctx<'_>, station: i64) -> Value {
    const OFFICE_HEADER: &[&str] = &["corporationID", "itemID", "officeFolderID"];

    let (offices, corp_lines) = ctx.store.read(|db| {
        let station_offices: Vec<store::Row> = db.table("crpOffices").find(&["stationID"], &[store::Value::Int(station)]).collect();
        let offices: Vec<Value> = station_offices.iter().map(|o| Value::List(o.line(OFFICE_HEADER))).collect();
        // The corporations based here and those with an office here.
        let corp_lines: Vec<Vec<Value>> = db
            .table("corporation")
            .rows()
            .filter(|c| {
                c.int("stationID") == Some(station)
                    || station_offices.iter().any(|o| o.int("corporationID") == c.int("corporationID"))
            })
            .map(|c| corp_line(&c))
            .collect();
        (offices, corp_lines)
    });

    // The owners the client shows: each corporation, its founder and CEO.
    let mut ids: Vec<i64> = Vec::new();
    for line in &corp_lines {
        for i in [0usize, 10, 11] {
            if let Some(id) = line[i].as_i64() {
                if id != 0 && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    let eveowners = ctx.store.read(|db| eve_owners_by_id(db, &ids));
    let corporations = rowset(CORP_HEADER, corp_lines.into_iter().map(Value::List).collect());

    Value::Tuple(vec![eveowners, corporations, rowset(OFFICE_HEADER, offices)])
}

/// Each ID's owner name and type, from the static owners, characters (a
/// character's type is its bloodline's when it has none of its own) or
/// corporations; an ID none of them names is left out.
fn eve_owners_by_id(db: &Db, ids: &[i64]) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let bloodline_types = crate::sde::table("bloodlineTypes");
    let mut lines = Vec::new();
    for &id in ids {
        let o = db.table("eveStaticOwners").get_id(id);
        let c = db.table("character_").get_id(id);
        let co = db.table("corporation").get_id(id);
        let name = [o.map(|o| o.cell("ownerName")), c.map(|c| c.cell("characterName")), co.map(|co| co.cell("corporationName"))]
            .into_iter()
            .flatten()
            .find(|v| *v != Value::None);
        let Some(name) = name else { continue };
        let own_type = o.and_then(|o| o.int("typeID")).filter(|t| *t != 0).or_else(|| c.and_then(|c| c.int("typeID")).filter(|t| *t != 0));
        // One row per matching bloodline type, as the join gave.
        let bloodline: Vec<Option<i64>> = match c.and_then(|c| c.int("bloodlineID")) {
            Some(b) => bloodline_types.find("bloodlineID", b).filter_map(|r| r.int("typeID")).map(Some).collect(),
            None => Vec::new(),
        };
        let bloodline = if bloodline.is_empty() { vec![None] } else { bloodline };
        for bt in bloodline {
            let Some(type_id) = own_type.or(bt).or(co.map(|_| 2)) else { continue };
            lines.push(Value::List(vec![Value::Int(id), name.clone(), Value::Int(type_id)]));
        }
    }
    rowset(HEADER, lines)
}

async fn potential_home_stations(ctx: &Ctx<'_>, station: i64) -> Value {
    const HEADER: &[&str] = &["stationID", "typeID"];
    let lines = ctx.store.read(|db| {
        db.table("staStations").get_id(station).map(|s| Value::List(s.line(&["stationID", "stationTypeID"]))).into_iter().collect()
    });
    rowset(HEADER, lines)
}
