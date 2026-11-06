//! The world the server starts from: on its first start, a fresh world made
//! from the dumps compiled into it; on every start, the fix-ups that keep
//! older data in line with the code.

use std::collections::BTreeMap;

use crate::sde::Tables;
use crate::store::{self, Db, Store, StoreError, Tx, Value};

/// Each live table's starting rows, compiled in (`db/*.sql.gz`).
macro_rules! dumps {
    ($($name:literal),* $(,)?) => {
        const DUMPS: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../db/", $name, ".sql.gz")))),*
        ];
    };
}

dumps![
    "00_shared", "account", "accountBalance", "agtCharMissions", "agtMissionGroups", "agtMissionTypes",
    "agtMissions", "agtOfferGroups", "agtOfferRequired", "agtOfferReward", "agtOffers", "allianceApplications",
    "allianceMembers", "allianceRelationships", "alliance_shortnames", "alliances", "alscLog", "billsReceivable",
    "bookmarkVouchers", "bookmarks", "botGroups", "botRegionDensity", "botRelations", "botStock", "bots",
    "bounties", "channelChars", "channelMods", "channelTypes", "channels", "channelsStatic", "characterStatic",
    "character_", "chrAgentLoyalty", "chrAgentResearch", "chrCorporationRoles", "chrEmployment", "chrJumpClones",
    "chrKillRights", "chrMissionState", "chrNPCStandings", "chrNotes", "chrOffers", "chrOwnerNote",
    "chrStandings", "corporation", "corporationStatic", "courierMissions", "crpApplications", "crpCharShares",
    "crpItemLocks", "crpOffices", "crpSanctionedActions", "crpTitles", "crpVoteCases", "crpVoteOptions",
    "crpVotes", "crpWars", "deadspaceComplexes", "droneState", "entity", "entityStatic", "entity_attributes",
    "eveEncyclTypes", "eveNames", "eveStaticOwners", "eveStaticOwnersStatic", "evemail", "evemailDetails",
    "insuranceContracts", "invBlueprints", "invOreInfo", "invOreYield", "killLog", "killLogParticipants",
    "mapDenormalize", "mapSolarSystems", "market_history", "market_journal", "market_orders",
    "market_transactions", "moonScans", "outpostConstruction", "ownerStandings", "petitionCategories",
    "petitionCategoryProperties", "petitionLog", "petitionMessages", "petitionProperties", "petitionQueues",
    "petitions", "posLinks", "posStructures", "ramAssemblyLineStationCostLogs", "ramAssemblyLineStations",
    "ramAssemblyLines", "ramInstallationTypeDefaultContents", "ramJobs", "rentalInfo", "secureContainers",
    "staStations", "standingTransactions", "stationServiceRules"
];

/// A fresh world: the live tables as the dumps hold them, with the NPC
/// market stocked from the static data.
pub fn fresh(t: &Tables) -> Result<Db, String> {
    let mut texts = Vec::with_capacity(DUMPS.len());
    for (name, gz) in DUMPS {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(*gz), &mut text).map_err(|e| format!("{name}: {e}"))?;
        texts.push(crate::sde::copy::unix_lines(text));
    }
    let store = Store::in_memory(store::from_dumps(&texts)?);
    store.write(|tx| crate::sde::market::seed_fresh(tx, t)).map_err(|e| format!("stocking the NPC market: {e}"))?;
    Ok(store.into_db())
}

/// Bring the data in line with the code, at every start: one transaction.
pub fn ensure(store: &Store, t: &Tables) -> Result<(), StoreError> {
    store.write(|tx| {
        let timed = |what: &str, started: std::time::Instant| tracing::debug!("start-up: {what} in {:.0?}", started.elapsed());
        let s = std::time::Instant::now();
        backfill_character_origins(tx)?;
        timed("character origins", s);
        let s = std::time::Instant::now();
        crate::static_entities::ensure(tx)?;
        timed("static identities", s);
        let s = std::time::Instant::now();
        crate::sde::market::stock_starter_stations(tx, t)?;
        timed("starter stations", s);
        let s = std::time::Instant::now();
        crate::svc::recruitment::ensure(tx)?;
        crate::svc::contracts::ensure(tx)?;
        crate::svc::trade::ensure(tx)?;
        crate::svc::petition::ensure(tx)?;
        crate::svc::bridges::ensure(tx)?;
        crate::svc::chat::ensure(tx)?;
        crate::losses::ensure(tx)?;
        crate::travel::ensure(tx)?;
        crate::svc::jumps::portal::ensure(tx)?;
        crate::losses::pending::ensure(tx)?;
        crate::losses::journal::ensure(tx)?;
        crate::svc::map::ensure(tx)?;
        crate::svc::tutorial::ensure(tx)?;
        crate::drones::ensure(tx)?;
        crate::space_state::ensure(tx)?;
        crate::cloak::durability::ensure(tx)?;
        crate::svc::maintenance::ensure(tx)?;
        crate::svc::boosters::ensure(tx)?;
        crate::svc::outposts::ensure_access_locks(tx)?;
        crate::svc::outposts::ensure_management_schema(tx)?;
        crate::svc::corp::ensure_office_schema(tx)?;
        crate::conn::logout::ensure(tx)?;
        crate::svc::skills::ensure(tx)?;
        timed("module fix-ups", s);
        Ok(())
    })
}

/// Characters made before `character_` had the columns: the school their
/// corporation runs, that school's first career and the career's first
/// speciality, and (for players) the bloodline's first ancestry.
fn backfill_character_origins(tx: &mut Tx) -> Result<(), StoreError> {
    let first = |table: &str, from: &str, to: &str, smallest: bool| -> BTreeMap<i64, i64> {
        let mut map: BTreeMap<i64, i64> = BTreeMap::new();
        for r in crate::sde::table(table).rows() {
            let (Some(k), Some(v)) = (r.int(from), r.int(to)) else { continue };
            map.entry(k).and_modify(|e| if smallest && v < *e { *e = v }).or_insert(v);
        }
        map
    };
    let schools = first("chrSchools", "corporationID", "schoolID", false);
    let careers = first("chrCareers", "schoolID", "careerID", true);
    let specialities = first("chrCareerSpecialities", "careerID", "specialityID", true);
    let ancestries = first("chrAncestries", "bloodlineID", "ancestryID", true);
    let rows: Vec<store::RowId> = tx.table("character_").rows().map(|r| r.id).collect();
    for id in rows {
        let Some(c) = tx.table("character_").row(id) else { continue };
        let mut sets: Vec<(&str, Value)> = Vec::new();
        let mut school = c.int("schoolID");
        if school.is_none() {
            if let Some(s) = c.int("corporationID").and_then(|corp| schools.get(&corp)) {
                school = Some(*s);
                sets.push(("schoolID", Value::Int(*s)));
            }
        }
        let mut career = c.int("careerID");
        if career.is_none() {
            if let Some(k) = school.and_then(|s| careers.get(&s)) {
                career = Some(*k);
                sets.push(("careerID", Value::Int(*k)));
            }
        }
        if c.int("careerSpecialityID").is_none() {
            if let Some(p) = career.and_then(|k| specialities.get(&k)) {
                sets.push(("careerSpecialityID", Value::Int(*p)));
            }
        }
        if c.int("ancestryID").is_none() && c.int("accountID").is_some() {
            if let Some(a) = c.int("bloodlineID").and_then(|b| ancestries.get(&b)) {
                sets.push(("ancestryID", Value::Int(*a)));
            }
        }
        if !sets.is_empty() {
            tx.update("character_", id, &sets)?;
        }
    }
    Ok(())
}
