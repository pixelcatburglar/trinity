pub mod agent_missions;
pub mod agents;
pub mod alert;
pub mod alliance;
pub mod bookmark;
pub mod boosters;
pub mod bound;
pub mod bridges;
pub mod charcreation;
pub mod charsheet;
pub mod chat;
pub mod clones;
pub mod corphangar;
pub mod jumps;
pub mod outposts;
pub mod scan;
pub mod config;
pub mod contracts;
pub mod corp;
pub mod dogma;
pub mod embark;
pub mod factory;
pub mod fitting;
pub mod fleet;
pub mod gang;
pub mod gm;
pub mod inflight;
pub mod insurance;
pub mod inventory;
pub mod language;
pub mod login;
pub mod machonet;
pub mod lookup;
pub mod medical;
pub mod map;
pub mod market;
pub mod maintenance;
pub mod membership;
pub mod moonmining;
pub mod missions;
pub mod moves;
pub mod notes;
pub mod onlinestatus;
pub mod pvptrade;
pub mod petition;
pub mod ram;
pub mod repair;
pub mod reprocessing;
pub mod recruitment;
pub mod rows;
pub mod skills;
pub mod space;
pub mod starbase;
pub mod standing;
pub mod station;
pub mod trade;
pub mod votes;
pub mod lp;
pub mod trinity;
pub mod tutorial;
pub mod war;
pub mod wallet;

use crate::services::{handler, Registry};

/// Every service the server answers. A name listed more than once is served by
/// several modules, tried in the order given here.
///
/// Calls that change the session (`character::SelectCharacterID`, `ship::Undock`,
/// `beyonce::Dock` / `StargateJump` / movement) never get here: `conn` serves them.
pub fn registry() -> Registry {
    let mut r = Registry::default();
    r.add("objectCaching", handler!(login::object_caching))
        .add("authentication", handler!(login::authentication))
        .add("alert", handler!(alert::alert))
        .add("config", handler!(config::config))
        .add("languageSvc", handler!(language::language_svc))
        .add("machoNet", handler!(machonet::macho_net))
        // character
        .add("character", handler!(login::character))
        .add("character", handler!(charcreation::character))
        .add("character", handler!(charsheet::character))
        .add("character", handler!(notes::character))
        .add("character", handler!(trinity::character))
        .add("contractMgr", handler!(contracts::contract_mgr))
        .add("LPSvc", handler!(lp::lp_svc))
        .add("voiceMgr", handler!(trinity::voice_mgr))
        .add("gangSvc", handler!(fleet::gang_svc))
        .add_bound("gangSvcObjectHandler", handler!(fleet::object_handler))
        .add("charmgr", handler!(charsheet::charmgr))
        .add_bound("skillMgr", handler!(skills::skill_mgr))
        .add_bound("jumpCloneSvc", handler!(clones::jump_clone_svc))
        .add("standing2", handler!(standing::standing))
        .add("standing2", handler!(trinity::standing2))
        // station and map
        .add("station", handler!(station::station))
        .add("stationSvc", handler!(station::station_svc))
        .add("map", handler!(map::map))
        .add("map", handler!(station::map))
        .add("tutorialSvc", handler!(tutorial::tutorial_svc))
        .add("tutorialSvc", handler!(trinity::tutorial_svc))
        // items and ships
        .add_bound("invbroker", handler!(inventory::invbroker))
        .add("invcontainer", handler!(inventory::invcontainer))
        .add_bound("dogmaIM", handler!(starbase::dogma_im))
        .add_bound("dogmaIM", handler!(inflight::dogma_im))
        .add_bound("dogmaIM", handler!(dogma::dogma_im))
        .add("factory", handler!(factory::factory))
        .add("ramProxy", handler!(ram::ram_proxy))
        .add_bound("reprocessingSvc", handler!(reprocessing::reprocessing_svc))
        .add_bound("repairSvc", handler!(repair::repair_svc))
        .add_bound("ship", handler!(space::ship))
        .add_bound("insuranceSvc", handler!(insurance::insurance_svc))
        .add_bound("beyonce", handler!(space::beyonce))
        .add_bound("posMgr", handler!(starbase::pos_mgr))
        .add_bound("entity", handler!(crate::drones::entity))
        .add_bound("aggressionMgr", handler!(crate::concord::aggression_mgr))
        // money and market
        .add("account", handler!(wallet::account))
        .add("billMgr", handler!(wallet::bill_mgr))
        .add("marketProxy", handler!(market::market_proxy))
        .add("marketProxy", handler!(wallet::market_proxy))
        .add("trademgr", handler!(pvptrade::trademgr))
        .add_bound("tradeSession", handler!(pvptrade::session))
        // corporations and alliances
        .add("corpmgr", handler!(corp::corpmgr))
        .add("corporationSvc", handler!(corp::corporation_svc))
        .add("corporationSvc", handler!(recruitment::corporation_svc))
        .add_bound("corpRegistry", handler!(recruitment::corp_registry))
        .add_bound("corpRegistry", handler!(membership::corp_registry))
        .add_bound("corpRegistry", handler!(alliance::corp_registry))
        .add_bound("corpRegistry", handler!(war::corp_registry))
        .add_bound("corpRegistry", handler!(votes::corp_registry))
        .add_bound("corpRegistry", handler!(corp::registry))
        // Before corp::station_mgr, which answers every method it is given.
        .add_bound("corpStationMgr", handler!(trinity::corp_station_mgr))
        .add_bound("corpStationMgr", handler!(corp::station_mgr))
        .add_bound("corpMembers", handler!(corp::members_provider))
        .add_bound(corp::views::OFFICES_PROVIDER, handler!(corp::offices_provider))
        .add_bound("allianceRegistry", handler!(alliance::alliance_registry))
        .add_bound("warRegistry", handler!(war::war_registry))
        // social
        .add("LSC", handler!(chat::lsc))
        .add("LSC", handler!(trinity::lsc))
        .add("onlineStatus", handler!(onlinestatus::online_status))
        .add("bookmark", handler!(bookmark::bookmark))
        .add("voucher", handler!(bookmark::voucher))
        .add("petitioner", handler!(petition::petitioner))
        .add("lookupSvc", handler!(lookup::lookup_svc))
        .add("gangsta", handler!(gang::gangsta))
        // agents and missions
        .add_bound("agentMgr", handler!(agents::agent_mgr))
        .add_bound("missionMgr", handler!(missions::mission_mgr))
        // game masters
        .add("slash", handler!(gm::slash))
        .add("keeper", handler!(gm::dungeon_tools))
        .add("dungeon", handler!(gm::dungeon_tools))
        .add("benchmark", handler!(gm::benchmark));
    r
}
