//! Write overlay with an undo journal.
//!
//! All changes made while importing one block (including any main blocks it
//! causes to be applied) accumulate here and are committed as one atomic
//! database transaction. While a main block is being applied, the first old
//! value of every touched key in a journaled table is recorded; unapplying the
//! main block replays those values in reverse, which restores the state
//! *exactly* — no hand-written inverse logic (xdagj's `unApplyBlock` had
//! several asymmetries with `applyBlock`).

use std::collections::{HashMap, HashSet};
use xdag_storage::{Db, Table, WriteBatch};
use xdag_storage::{Reader, Writer};

use crate::ChainError;

pub fn is_journaled(t: Table) -> bool {
    matches!(
        t,
        Table::BlockState
            | Table::Account
            | Table::Storage
            | Table::Code
            | Table::MainHeight
            | Table::History
            | Table::TxIndex
            | Table::Receipt
            | Table::EvmTxs
    )
}

#[derive(Default)]
struct Journal {
    entries: Vec<(Table, Vec<u8>, Option<Vec<u8>>)>,
    seen: HashSet<(Table, Vec<u8>)>,
}

pub struct Overlay {
    db: Db,
    writes: HashMap<(Table, Vec<u8>), Option<Vec<u8>>>,
    order: Vec<(Table, Vec<u8>)>,
    journal: Option<Journal>,
}

/// Encoded undo log of one main block.
pub struct UndoLog {
    pub legacy: bool,
    pub entries: Vec<(Table, Vec<u8>, Option<Vec<u8>>)>,
}

impl UndoLog {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(1).bool(self.legacy).u32(self.entries.len() as u32);
        for (t, k, v) in &self.entries {
            w.u8(*t as u8).bytes(k);
            match v {
                Some(v) => {
                    w.u8(1).bytes(v);
                }
                None => {
                    w.u8(0);
                }
            }
        }
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = |_| ChainError::Corrupt("journal".into());
        let mut r = Reader::new(b);
        if r.u8().map_err(e)? != 1 {
            return Err(ChainError::Corrupt("journal version".into()));
        }
        let legacy = r.bool().map_err(e)?;
        let n = r.u32().map_err(e)? as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Table::from_u8(r.u8().map_err(e)?).ok_or_else(|| ChainError::Corrupt("journal table".into()))?;
            let k = r.bytes().map_err(e)?;
            let v = if r.u8().map_err(e)? == 1 { Some(r.bytes().map_err(e)?) } else { None };
            entries.push((t, k, v));
        }
        Ok(UndoLog { legacy, entries })
    }
}

impl Overlay {
    pub fn new(db: Db) -> Self {
        Overlay { db, writes: HashMap::new(), order: Vec::new(), journal: None }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn get(&self, t: Table, k: &[u8]) -> Result<Option<Vec<u8>>, ChainError> {
        if let Some(v) = self.writes.get(&(t, k.to_vec())) {
            return Ok(v.clone());
        }
        Ok(self.db.get(t, k)?)
    }

    fn record_old(&mut self, t: Table, k: &[u8]) -> Result<(), ChainError> {
        if self.journal.is_none() || !is_journaled(t) {
            return Ok(());
        }
        let key = (t, k.to_vec());
        if self.journal.as_ref().unwrap().seen.contains(&key) {
            return Ok(());
        }
        let old = self.get(t, k)?;
        let j = self.journal.as_mut().unwrap();
        j.seen.insert(key);
        j.entries.push((t, k.to_vec(), old));
        Ok(())
    }

    pub fn put(&mut self, t: Table, k: Vec<u8>, v: Vec<u8>) -> Result<(), ChainError> {
        self.record_old(t, &k)?;
        let key = (t, k);
        if !self.writes.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.writes.insert(key, Some(v));
        Ok(())
    }

    pub fn delete(&mut self, t: Table, k: Vec<u8>) -> Result<(), ChainError> {
        self.record_old(t, &k)?;
        let key = (t, k);
        if !self.writes.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.writes.insert(key, None);
        Ok(())
    }

    pub fn begin_journal(&mut self) {
        debug_assert!(self.journal.is_none(), "nested journal");
        self.journal = Some(Journal::default());
    }

    pub fn journaling(&self) -> bool {
        self.journal.is_some()
    }

    pub fn end_journal(&mut self) -> Vec<(Table, Vec<u8>, Option<Vec<u8>>)> {
        self.journal.take().map(|j| j.entries).unwrap_or_default()
    }

    /// Replay an undo log (newest change first).
    pub fn undo(&mut self, log: UndoLog) -> Result<(), ChainError> {
        debug_assert!(self.journal.is_none());
        for (t, k, old) in log.entries.into_iter().rev() {
            match old {
                Some(v) => self.put(t, k, v)?,
                None => {
                    if log.legacy && t == Table::Account {
                        // xdagj never deletes an address once it was credited:
                        // `addressIsExist` stays true after a rollback. Keep an
                        // empty record so INPUT validation agrees with xdagj.
                        self.put(t, k, crate::records::AccountRecord::default().encode())?
                    } else {
                        self.delete(t, k)?
                    }
                }
            }
        }
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        !self.writes.is_empty()
    }

    pub fn pending_len(&self) -> usize {
        self.writes.len()
    }

    pub fn commit(&mut self, durable: bool) -> Result<(), ChainError> {
        debug_assert!(self.journal.is_none(), "commit while journaling");
        if self.writes.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::new();
        let mut order = std::mem::take(&mut self.order);
        // group by table: fewer table (re)opens in the write transaction
        order.sort_by_key(|(t, _)| *t as u8);
        for key in order {
            if let Some(v) = self.writes.remove(&key) {
                match v {
                    Some(v) => batch.put(key.0, key.1, v),
                    None => batch.delete(key.0, key.1),
                }
            }
        }
        self.writes.clear();
        self.db.write(batch, durable)?;
        Ok(())
    }

    pub fn discard(&mut self) {
        self.writes.clear();
        self.order.clear();
        self.journal = None;
    }
}
