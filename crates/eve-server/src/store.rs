//! The live game data, held in memory and kept on disk by the server itself.
//!
//! Every table the game changes as it runs (items, characters, wallets, the
//! market, ...) lives in a [`Store`]: rows with ordered indexes, the
//! constraints and triggers PostgreSQL enforced, and transactions that apply
//! wholly or not at all. One write transaction runs at a time, which is what
//! the row and advisory locks of the SQL it replaces were for; reads run
//! alongside each other. Committed transactions go to a write-ahead log on
//! disk, with periodic snapshots (see `wal`).
//!
//! The tables are defined in `store/schema.json`.

mod load;
mod schema;
mod table;
mod tx;
mod wal;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use tracing::info;

pub use crate::sde::Value;
pub use load::read_dump;
pub use schema::ColType;
pub use table::{Row, RowId};
pub use tx::{Db, Sets, StoreError, Tx, CHECK_VIOLATION, DATA_EXCEPTION, FOREIGN_KEY_VIOLATION, NOT_NULL_VIOLATION, UNIQUE_VIOLATION};

/// The live tables' definitions, read once.
pub fn schema() -> Arc<schema::Schema> {
    static SCHEMA: OnceLock<Arc<schema::Schema>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| Arc::new(schema::Schema::parse(include_str!("store/schema.json")).unwrap_or_else(|e| panic!("the store's schema is invalid: {e}"))))
        .clone()
}

/// Every live table, empty, and every sequence before its first value.
pub fn empty() -> Db {
    Db::empty(schema())
}

/// Every live table filled from `dumps` (PostgreSQL dumps' text: the
/// `db/*.sql.gz` a fresh world starts from, or a `pg_dump` of a database
/// moving into the store).
pub fn from_dumps(dumps: &[String]) -> Result<Db, String> {
    let mut db = empty();
    load::load(&mut db, dumps)?;
    Ok(db)
}

struct Disk {
    dir: Arc<PathBuf>,
    log: wal::Log,
    generation: AtomicU64,
    /// Held while a snapshot is written, so one is written at a time.
    checkpointing: Mutex<()>,
}

pub struct Store {
    db: RwLock<Db>,
    disk: Option<Disk>,
}

thread_local! {
    /// Whether this thread is inside a read or write of a store, which a
    /// nested read or write would deadlock on.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

struct Inside;

impl Inside {
    fn enter() -> Inside {
        INSIDE.with(|i| {
            assert!(!i.get(), "a store read or write inside another: finish the first before starting the next");
            i.set(true);
        });
        // A write may lock the world while it holds the store, so the store
        // must not be waited for while holding the world.
        assert!(!crate::world::world_held(), "the store entered while holding the world lock: read or write the store first, then lock the world");
        Inside
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE.with(|i| i.set(false));
    }
}

impl Store {
    /// A store kept only in memory (for tests).
    pub fn in_memory(db: Db) -> Store {
        Store { db: RwLock::new(db), disk: None }
    }

    /// The store kept in `dir`: what is there, or (when `dir` holds nothing
    /// yet) the world `fresh` makes, saved there first.
    pub fn open(dir: &Path, fresh: impl FnOnce() -> Result<Db, String>) -> Result<Store, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let started = Instant::now();
        let (db, generation) = match wal::recover(dir, empty())? {
            Some((db, last, replayed)) => {
                info!("store: read from {} (generation {last}, {replayed} transactions replayed) in {:.0?}", dir.display(), started.elapsed());
                // Go on appending to the newest log; the next snapshot
                // compacts it away.
                (db, last)
            }
            None => {
                let db = fresh()?;
                wal::write_snapshot(dir, 0, &db)?;
                info!("store: new data saved to {} in {:.0?}", dir.display(), started.elapsed());
                (db, 0)
            }
        };
        let dir = Arc::new(dir.to_path_buf());
        let log = wal::Log::open(dir.clone(), generation)?;
        Ok(Store { db: RwLock::new(db), disk: Some(Disk { dir, log, generation: AtomicU64::new(generation), checkpointing: Mutex::new(()) }) })
    }

    /// The data, for a store that is done with (a fresh world being made).
    pub fn into_db(self) -> Db {
        self.db.into_inner().unwrap_or_else(|p| p.into_inner())
    }

    /// Read the data. Readers run alongside each other, never alongside a
    /// write.
    pub fn read<T>(&self, f: impl FnOnce(&Db) -> T) -> T {
        let _inside = Inside::enter();
        let db = self.db.read().unwrap_or_else(|p| p.into_inner());
        f(&db)
    }

    /// Change the data: `f`'s changes all apply if it returns Ok, and none
    /// does if it returns Err (or panics). One write runs at a time.
    pub fn write<T, E: From<StoreError>>(&self, f: impl FnOnce(&mut Tx) -> Result<T, E>) -> Result<T, E> {
        let _inside = Inside::enter();
        let mut db = self.db.write().unwrap_or_else(|p| p.into_inner());
        let mut tx = Tx::new(&mut db);
        let out = f(&mut tx)?;
        if let Some(disk) = &self.disk {
            if !tx.log.is_empty() || tx.sequences_moved {
                // Queued while the store is still locked, so the log holds
                // transactions in the order they committed.
                disk.log.commit(wal::encode_commit(tx.db(), &tx.log, tx.sequences_moved));
            }
        }
        tx.commit();
        Ok(out)
    }

    /// Wait until everything committed so far is on disk.
    pub fn sync(&self) {
        if let Some(disk) = &self.disk {
            disk.log.sync();
        }
    }

    /// Write a snapshot of the data and drop the log it makes unnecessary.
    pub fn checkpoint(&self) -> Result<(), String> {
        let Some(disk) = &self.disk else { return Ok(()) };
        let _one = disk.checkpointing.lock().unwrap_or_else(|p| p.into_inner());
        let started = Instant::now();
        let (copy, generation) = {
            let _inside = Inside::enter();
            let db = self.db.write().unwrap_or_else(|p| p.into_inner());
            let generation = disk.generation.fetch_add(1, Ordering::SeqCst) + 1;
            disk.log.rotate(generation);
            // Tables are shared until written: this copy costs nothing now,
            // and writes made while the snapshot is written copy only the
            // tables they touch.
            (db.clone(), generation)
        };
        wal::write_snapshot(&disk.dir, generation, &copy)?;
        wal::prune(&disk.dir, generation);
        info!("store: snapshot {generation} written in {:.0?}", started.elapsed());
        Ok(())
    }
}
