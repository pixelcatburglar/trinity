mod ballpark;
mod beacons;
mod bots;
mod cloak;
mod complexes;
mod sde;
mod store;
mod bulkdata;
mod combat;
mod concord;
mod config;
mod conn;
mod db;
mod destiny;
mod drones;
mod dogma;
mod exceptions;
mod fight;
mod gates;
mod guard;
mod heartbeat;
mod items;
mod logging;
mod losses;
mod loot;
mod npc;
mod pos;
mod services;
mod session;
mod sites;
mod startup;
mod static_entities;
mod space_state;
mod svc;
mod web;
mod warp_energy;
mod world;

mod travel;

use std::sync::Arc;
use std::time::Instant;

use eve_marshal::StringTable;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use config::Config;
use conn::{Conn, Server};

/// Bulk objects this close to the 1 MB frame cap are worth a warning.
const BULK_WARN_BYTES: usize = 900_000;

#[tokio::main]
async fn main() {
    // The config says how to log, so it is read before there is a log.
    let (config, note) = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = logging::init(&config.log_level, config.log_file.as_deref()) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    info!("{note}");
    if let Err(e) = run(config).await {
        error!("{}", logging::chain(&*e));
        std::process::exit(1);
    }
}

async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {

    let table = std::fs::read(&config.strings)
        .map(|raw| StringTable::parse(&raw))
        .map_err(|e| {
            format!("cannot read the shared string table at {}: {e}", config.strings.display())
        })?;
    info!("string table: {} entries from {}", table.len(), config.strings.display());

    // The static data as compiled in; what it leaves unpriced is priced from
    // the NPC market once the store is open.
    let tables = sde::read()?;
    let opened = Instant::now();
    let store = Arc::new(store::Store::open(&config.data, || match &config.import {
        Some(dump) => {
            info!("store: starting from the PostgreSQL dump {}", dump.display());
            store::from_dumps(&[store::read_dump(dump)?])
        }
        None => startup::fresh(&tables),
    })?);
    let accounts = store.read(|db| db.table("account").len());
    info!("store: {} ({accounts} accounts) open in {:.1}s", config.data.display(), opened.elapsed().as_secs_f64());
    let sde = sde::init(tables, &store)?;
    startup::ensure(&store, sde.tables()).map_err(|e| format!("bringing the data up to date: {e}"))?;
    let ids = db::sync_accounts(&store, &config.accounts).map_err(|e| format!("setting up the config file's accounts: {e}"))?;
    if !ids.is_empty() {
        info!("{} account(s) from the config file", ids.len());
    }
    config::set_rules(config::Rules {
        starting_isk: config.starting_isk,
        account_isk: ids.iter().zip(&config.accounts).filter_map(|(id, a)| Some((*id, a.starting_isk?))).collect(),
        auto_create_accounts: config.auto_create_accounts,
        jump_clone_min_standing: config.jump_clone_min_standing,
    });
    // Startup-only cleanup must finish before clients can create live trades.
    svc::pvptrade::recover(&store);

    let started = Instant::now();
    let bulk = bulkdata::BulkData::load(&store, &table, config.node_id, bulkdata::filetime_now())?;
    report_bulk(&bulk, started);

    let dogma = dogma::Catalog::new();

    let services = svc::registry();
    info!("{} services registered", services.len());

    let server = Arc::new(Server {
        table,
        store: Arc::clone(&store),
        node_id: config.node_id,
        bulk,
        dogma,
        services,
        world: world::World::default(),
        dump_dir: config.dump_dir.clone(),
        alliance_join_delay_secs: config.alliance_join_delay_secs,
        sov_claim_delay_secs: config.sov_claim_delay_secs,
    });
    pos::set_speedup(config.starbase_speedup);
    tokio::spawn(heartbeat::run(Arc::clone(&server)));
    bots::configure(config.bots_pirates_attack, config.bots_controls);
    tokio::spawn(bots::run(Arc::clone(&server), config.bots, config.bots_speed, config.bots_shown));
    tokio::spawn(web::run(Arc::clone(&server), config.web.clone()));
    let listener = TcpListener::bind(&config.bind).await?;
    info!("listening on {} as node {}", config.bind, config.node_id);

    tokio::spawn(checkpoints(Arc::clone(&store)));
    tokio::spawn(svc::skills::run_training(Arc::clone(&server)));

    let mut seq = 0i64;
    let mut stop = std::pin::pin!(shutdown());
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept failed: {err}", err = crate::logging::chain(&e));
                    continue;
                }
            },
            () = &mut stop => break,
        };
        let _ = stream.set_nodelay(true);
        seq += 1;
        tokio::spawn(Conn::new(stream, peer, Arc::clone(&server), seq).run());
    }
    info!("stopping: saving the data");
    let saving = Arc::clone(&store);
    tokio::task::spawn_blocking(move || {
        saving.sync();
        saving.checkpoint()
    })
    .await??;
    info!("data saved to {}", config.data.display());
    Ok(())
}

/// How often the data is snapshotted, so a start replays little of the log.
const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

async fn checkpoints(store: Arc<store::Store>) {
    let mut every = tokio::time::interval(CHECKPOINT_EVERY);
    every.tick().await;
    loop {
        every.tick().await;
        let store = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || store.checkpoint()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("snapshotting the data: {e}"),
            Err(e) => error!("snapshotting the data: {e}"),
        }
    }
}

/// Ctrl-C, or (in a container) SIGTERM.
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn report_bulk(bulk: &bulkdata::BulkData, started: Instant) {
    let mut bytes = 0usize;
    let mut empty = Vec::new();
    for spec in bulkdata::SPECS {
        let e = &bulk.entries[spec.key];
        bytes += e.pickle.len();
        if e.rows == 0 {
            empty.push(spec.key);
        }
        if e.pickle.len() > BULK_WARN_BYTES {
            warn!("bulk object {} is {} bytes, near the 1 MB frame cap", spec.key, e.pickle.len());
        }
    }
    info!(
        "bulk data: {} objects, {} rows, {:.1} MB on the wire, built in {:.1}s",
        bulk.entries.len(),
        bulk.entries.values().map(|e| e.rows).sum::<usize>(),
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    if !empty.is_empty() {
        debug!("{} bulk object(s) served with no rows: {}", empty.len(), empty.join(", "));
    }
}
