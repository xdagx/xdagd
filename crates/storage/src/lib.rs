//! Storage for the XDAG node.
//!
//! Design goals (each addresses a failure of the xdagj storage layer):
//!
//! * **Explicit, versioned encodings.** Every record is encoded by hand with a
//!   leading version byte; nothing depends on the in-memory shape of a type.
//!   xdagj persisted Java objects with Kryo's field serializer, so any change to
//!   `BlockInfo` made old databases unreadable and every upgrade had to wipe the
//!   store and restart from a balance snapshot, losing history.
//! * **In-place migrations.** The database records its schema version; on open,
//!   pending migrations run in order inside write transactions. Opening a
//!   database written by a *newer* node is refused instead of corrupting it.
//! * **Atomic multi-table commits.** A block import (and any main-chain
//!   changes it triggers) is written as one transaction.

pub mod codec;

use parking_lot::Mutex;
use redb::{Database, Durability, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

pub use codec::{Reader, Writer};

/// Current schema version written by this build.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Db(String),
    #[error("database schema version {found} is newer than supported {supported}; refusing to open")]
    TooNew { found: u32, supported: u32 },
    #[error("corrupt record in {table}: {msg}")]
    Corrupt { table: &'static str, msg: String },
    #[error("migration {0} failed: {1}")]
    Migration(u32, String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

macro_rules! db_err {
    ($($t:ty),*) => {$(
        impl From<$t> for StorageError {
            fn from(e: $t) -> Self { StorageError::Db(e.to_string()) }
        }
    )*};
}
db_err!(redb::Error, redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

/// Logical tables. Keys and values are opaque bytes; the chain crate owns the
/// encodings (see `codec`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Table {
    /// Single-key metadata: schema version, chain status, …
    Meta = 0,
    /// hashlow(24) → raw 512-byte block
    BlockRaw = 1,
    /// hashlow(24) → block metadata record
    BlockInfo = 2,
    /// height(u64 BE) → hashlow of the main block at that height
    MainHeight = 3,
    /// epoch(u64 BE) ‖ hashlow → () — time index used by sync and explorers
    TimeIndex = 4,
    /// sums key → 4096-byte sums array (xdagj/C sync protocol)
    Sums = 5,
    /// address(20) → account record
    Account = 6,
    /// code hash(32) → EVM bytecode
    Code = 7,
    /// address(20) ‖ slot(32) → value(32)
    Storage = 8,
    /// height(u64 BE) → undo log of the state changes made by that main block
    Journal = 9,
    /// address(20) or hashlow(24) ‖ height(u64 BE) ‖ seq(u32 BE) → history entry
    History = 10,
    /// tx hash(32) → transaction location/status
    TxIndex = 11,
    /// hashlow(24) → Nova extension payload
    Payload = 12,
    /// hashlow(24) → () — blocks nobody references yet (DAG tips)
    NoRef = 13,
    /// hashlow(24) → () — blocks signed by one of our keys
    Ours = 14,
    /// Pre-snapshot archive: hashlow(24) → raw block imported from old nodes
    Archive = 15,
    /// Receipts of EVM transactions: tx hash(32) → receipt record
    Receipt = 16,
    /// hashlow(24) → execution state of a block (flags MAIN/MAIN_REF/APPLIED,
    /// balance, fee, ref, height). Journaled; DAG metadata lives in BlockInfo.
    BlockState = 17,
    /// Pre-snapshot archive index: address(20) or hashlow(24) ‖ time ‖ hashlow → entry
    ArchiveHistory = 18,
    /// main height(u64 BE) ‖ seq(u32 BE) → EVM tx hash (Ethereum block view)
    EvmTxs = 19,
}

pub const ALL_TABLES: &[Table] = &[
    Table::Meta,
    Table::BlockRaw,
    Table::BlockInfo,
    Table::MainHeight,
    Table::TimeIndex,
    Table::Sums,
    Table::Account,
    Table::Code,
    Table::Storage,
    Table::Journal,
    Table::History,
    Table::TxIndex,
    Table::Payload,
    Table::NoRef,
    Table::Ours,
    Table::Archive,
    Table::Receipt,
    Table::BlockState,
    Table::ArchiveHistory,
    Table::EvmTxs,
];

impl Table {
    pub fn name(self) -> &'static str {
        match self {
            Table::Meta => "meta",
            Table::BlockRaw => "block_raw",
            Table::BlockInfo => "block_info",
            Table::MainHeight => "main_height",
            Table::TimeIndex => "time_index",
            Table::Sums => "sums",
            Table::Account => "account",
            Table::Code => "code",
            Table::Storage => "storage",
            Table::Journal => "journal",
            Table::History => "history",
            Table::TxIndex => "tx_index",
            Table::Payload => "payload",
            Table::NoRef => "noref",
            Table::Ours => "ours",
            Table::Archive => "archive",
            Table::Receipt => "receipt",
            Table::BlockState => "block_state",
            Table::ArchiveHistory => "archive_history",
            Table::EvmTxs => "evm_txs",
        }
    }

    pub fn from_u8(v: u8) -> Option<Table> {
        ALL_TABLES.iter().copied().find(|t| *t as u8 == v)
    }

    fn def(self) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
        TableDefinition::new(self.name())
    }
}

/// A set of writes applied atomically.
#[derive(Default, Debug, Clone)]
pub struct WriteBatch {
    pub ops: Vec<(Table, Vec<u8>, Option<Vec<u8>>)>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&mut self, t: Table, k: impl Into<Vec<u8>>, v: impl Into<Vec<u8>>) {
        self.ops.push((t, k.into(), Some(v.into())));
    }
    pub fn delete(&mut self, t: Table, k: impl Into<Vec<u8>>) {
        self.ops.push((t, k.into(), None));
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// A migration from `from` to `from + 1`.
pub struct Migration {
    pub from: u32,
    pub description: &'static str,
    pub run: fn(&Db) -> std::result::Result<(), String>,
}

#[derive(Clone)]
pub struct Db {
    inner: Arc<Database>,
    write_lock: Arc<Mutex<()>>,
}

const SCHEMA_KEY: &[u8] = b"schema_version";

impl Db {
    /// Open (or create) a database and bring it to [`SCHEMA_VERSION`].
    pub fn open(path: &Path, migrations: &[Migration]) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::Db(e.to_string()))?;
        }
        let database = Database::builder().set_cache_size(256 << 20).create(path)?;
        let db = Db { inner: Arc::new(database), write_lock: Arc::new(Mutex::new(())) };
        // make sure every table exists so readers never fail on a missing table
        {
            let wtx = db.inner.begin_write()?;
            for t in ALL_TABLES {
                wtx.open_table(t.def())?;
            }
            wtx.commit()?;
        }
        db.migrate(migrations)?;
        Ok(db)
    }

    pub fn open_temporary() -> Result<(Db, TempDirGuard)> {
        let dir =
            std::env::temp_dir().join(format!("xdag-db-{}-{}", std::process::id(), TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).map_err(|e| StorageError::Db(e.to_string()))?;
        let db = Db::open(&dir.join("chain.redb"), &[])?;
        Ok((db, TempDirGuard(dir)))
    }

    pub fn schema_version(&self) -> Result<Option<u32>> {
        Ok(self.get(Table::Meta, SCHEMA_KEY)?.map(|v| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&v[..4]);
            u32::from_be_bytes(b)
        }))
    }

    fn set_schema_version(&self, v: u32) -> Result<()> {
        let mut b = WriteBatch::new();
        b.put(Table::Meta, SCHEMA_KEY, v.to_be_bytes().to_vec());
        self.write(b, true)
    }

    fn migrate(&self, migrations: &[Migration]) -> Result<()> {
        match self.schema_version()? {
            None => {
                self.set_schema_version(SCHEMA_VERSION)?;
            }
            Some(v) if v > SCHEMA_VERSION => {
                return Err(StorageError::TooNew { found: v, supported: SCHEMA_VERSION });
            }
            Some(mut v) => {
                while v < SCHEMA_VERSION {
                    let m = migrations.iter().find(|m| m.from == v).ok_or_else(|| StorageError::Migration(v, "no migration registered".into()))?;
                    tracing::info!(from = v, to = v + 1, "running storage migration: {}", m.description);
                    (m.run)(self).map_err(|e| StorageError::Migration(v, e))?;
                    v += 1;
                    self.set_schema_version(v)?;
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, t: Table, k: &[u8]) -> Result<Option<Vec<u8>>> {
        let rtx = self.inner.begin_read()?;
        let table = rtx.open_table(t.def())?;
        Ok(table.get(k)?.map(|g| g.value().to_vec()))
    }

    pub fn contains(&self, t: Table, k: &[u8]) -> Result<bool> {
        let rtx = self.inner.begin_read()?;
        let table = rtx.open_table(t.def())?;
        Ok(table.get(k)?.is_some())
    }

    /// Apply a batch atomically. `durable = false` trades crash-durability of
    /// the most recent commits for throughput (redb `Durability::Eventual`
    /// still never corrupts the file).
    pub fn write(&self, batch: WriteBatch, durable: bool) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let _g = self.write_lock.lock();
        let mut wtx = self.inner.begin_write()?;
        wtx.set_durability(if durable { Durability::Immediate } else { Durability::Eventual });
        {
            let mut current: Option<(Table, redb::Table<&[u8], &[u8]>)> = None;
            for (t, k, v) in &batch.ops {
                if current.as_ref().map(|c| c.0) != Some(*t) {
                    drop(current.take()); // release the previous table before opening the next
                    current = Some((*t, wtx.open_table(t.def())?));
                }
                let table = &mut current.as_mut().unwrap().1;
                match v {
                    Some(v) => {
                        table.insert(k.as_slice(), v.as_slice())?;
                    }
                    None => {
                        table.remove(k.as_slice())?;
                    }
                }
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// Entries whose key starts with `prefix`, in key order.
    pub fn scan_prefix(&self, t: Table, prefix: &[u8], limit: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let end = prefix_end(prefix);
        let upper = match &end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        self.scan_bounds(t, Bound::Included(prefix), upper, limit, false)
    }

    /// Entries in `[start, end)` (end `None` = unbounded), optionally newest-first.
    pub fn scan_range(&self, t: Table, start: &[u8], end: Option<&[u8]>, limit: usize, reverse: bool) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let upper = match end {
            Some(e) => Bound::Excluded(e),
            None => Bound::Unbounded,
        };
        self.scan_bounds(t, Bound::Included(start), upper, limit, reverse)
    }

    fn scan_bounds(&self, t: Table, lower: Bound<&[u8]>, upper: Bound<&[u8]>, limit: usize, reverse: bool) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let rtx = self.inner.begin_read()?;
        let table = rtx.open_table(t.def())?;
        let range = table.range::<&[u8]>((lower, upper))?;
        let mut out = Vec::new();
        let mut push = |item: std::result::Result<(redb::AccessGuard<&[u8]>, redb::AccessGuard<&[u8]>), redb::StorageError>| -> Result<bool> {
            let (k, v) = item?;
            out.push((k.value().to_vec(), v.value().to_vec()));
            Ok(out.len() < limit)
        };
        if reverse {
            for item in range.rev() {
                if !push(item)? {
                    break;
                }
            }
        } else {
            for item in range {
                if !push(item)? {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Number of entries (O(n); for diagnostics and tests).
    pub fn count(&self, t: Table) -> Result<u64> {
        let rtx = self.inner.begin_read()?;
        let table = rtx.open_table(t.def())?;
        Ok(table.len()?)
    }

    /// Iterate all entries of a table in chunks (used by migrations/export).
    pub fn for_each(&self, t: Table, mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        let rtx = self.inner.begin_read()?;
        let table = rtx.open_table(t.def())?;
        for item in table.iter()? {
            let (k, v) = item?;
            if !f(k.value(), v.value()) {
                break;
            }
        }
        Ok(())
    }
}

/// Smallest key greater than every key with this prefix.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last == 0xff {
            end.pop();
        } else {
            *last += 1;
            return Some(end);
        }
    }
    None
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Removes a temporary database directory on drop.
pub struct TempDirGuard(pub std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_write_read_scan() {
        let (db, _g) = Db::open_temporary().unwrap();
        let mut b = WriteBatch::new();
        for i in 0u8..10 {
            b.put(Table::History, vec![1, i], vec![i]);
        }
        b.put(Table::History, vec![2, 0], vec![42]);
        db.write(b, false).unwrap();
        assert_eq!(db.get(Table::History, &[1, 3]).unwrap(), Some(vec![3]));
        let s = db.scan_prefix(Table::History, &[1], 100).unwrap();
        assert_eq!(s.len(), 10);
        let r = db.scan_range(Table::History, &[1, 5], Some(&[2]), 3, true).unwrap();
        assert_eq!(r.iter().map(|(k, _)| k[1]).collect::<Vec<_>>(), vec![9, 8, 7]);
        let mut d = WriteBatch::new();
        d.delete(Table::History, vec![1, 3]);
        db.write(d, true).unwrap();
        assert_eq!(db.get(Table::History, &[1, 3]).unwrap(), None);
    }

    #[test]
    fn migrations_run_in_order_and_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.redb");
        {
            let db = Db::open(&path, &[]).unwrap();
            // pretend it was written by schema 0
            let mut b = WriteBatch::new();
            b.put(Table::Meta, SCHEMA_KEY, 0u32.to_be_bytes().to_vec());
            db.write(b, true).unwrap();
        }
        fn m0(db: &Db) -> std::result::Result<(), String> {
            let mut b = WriteBatch::new();
            b.put(Table::Meta, b"migrated".to_vec(), vec![1]);
            db.write(b, true).map_err(|e| e.to_string())
        }
        {
            let db = Db::open(&path, &[Migration { from: 0, description: "test", run: m0 }]).unwrap();
            assert_eq!(db.get(Table::Meta, b"migrated").unwrap(), Some(vec![1]));
            assert_eq!(db.schema_version().unwrap(), Some(SCHEMA_VERSION));
            let mut b = WriteBatch::new();
            b.put(Table::Meta, SCHEMA_KEY, (SCHEMA_VERSION + 1).to_be_bytes().to_vec());
            db.write(b, true).unwrap();
        }
        assert!(matches!(Db::open(&path, &[]), Err(StorageError::TooNew { .. })));
    }

    #[test]
    fn prefix_end_works() {
        assert_eq!(prefix_end(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(prefix_end(&[1, 0xff]), Some(vec![2]));
        assert_eq!(prefix_end(&[0xff]), None);
    }
}
