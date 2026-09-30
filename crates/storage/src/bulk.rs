//! Loading many rows at once.
//!
//! A copy-on-write B-tree rewrites every page a transaction touches. Rows that
//! arrive in random key order (blocks are keyed by their hash) touch nearly
//! every leaf in every batch, so loading millions of them batch by batch
//! writes a table many times over. [`BulkLoader`] sorts the rows first — in
//! memory up to a limit, beyond it as sorted runs in temporary files that are
//! then merged — and inserts them table by table in ascending key order, so
//! that each page is written once.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::{Db, Result, StorageError, Table, WriteBatch};

/// Rows written to the database per transaction, in bytes.
const BATCH_BYTES: usize = 32 << 20;

fn io(e: std::io::Error) -> StorageError {
    StorageError::Db(format!("bulk load: {e}"))
}

pub struct BulkLoader {
    dir: PathBuf,
    /// The rows of the current run, back to back:
    /// table(1) key length(u32) key value length(u32) value.
    buf: Vec<u8>,
    /// Offset of each row in `buf`, in arrival order.
    index: Vec<u32>,
    limit: usize,
    runs: Vec<PathBuf>,
}

/// One row inside a run buffer.
fn row(buf: &[u8], off: u32) -> (u8, &[u8], &[u8]) {
    let o = off as usize;
    let klen = u32::from_le_bytes(buf[o + 1..o + 5].try_into().unwrap()) as usize;
    let v = o + 5 + klen;
    let vlen = u32::from_le_bytes(buf[v..v + 4].try_into().unwrap()) as usize;
    (buf[o], &buf[o + 5..v], &buf[v + 4..v + 4 + vlen])
}

impl BulkLoader {
    /// `dir` holds the temporary files (created, and removed when the loader
    /// is dropped); `memory` bounds the rows held in memory.
    pub fn new(dir: &Path, memory: usize) -> Result<Self> {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).map_err(io)?;
        Ok(BulkLoader { dir: dir.to_path_buf(), buf: Vec::new(), index: Vec::new(), limit: memory.clamp(1 << 16, 1 << 31), runs: Vec::new() })
    }

    /// Queue a row. Of several rows for one key, the last one put is kept.
    pub fn put(&mut self, t: Table, k: &[u8], v: &[u8]) -> Result<()> {
        let size = 9 + k.len() + v.len();
        if size > self.limit {
            return Err(StorageError::Db("bulk load: row larger than the memory limit".into()));
        }
        if self.buf.len() + size > self.limit {
            self.spill()?;
        }
        self.index.push(self.buf.len() as u32);
        self.buf.push(t as u8);
        self.buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(k);
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v);
        Ok(())
    }

    /// Sort the current run by (table, key); rows of one key keep their order.
    fn sort(&mut self) {
        let buf = &self.buf;
        self.index.sort_by(|a, b| {
            let (ta, ka, _) = row(buf, *a);
            let (tb, kb, _) = row(buf, *b);
            (ta, ka).cmp(&(tb, kb))
        });
    }

    /// Write the current run to a temporary file.
    fn spill(&mut self) -> Result<()> {
        if self.index.is_empty() {
            return Ok(());
        }
        self.sort();
        let path = self.dir.join(format!("run-{}", self.runs.len()));
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&path).map_err(io)?);
        for off in &self.index {
            let o = *off as usize;
            let (_, k, v) = row(&self.buf, *off);
            w.write_all(&self.buf[o..o + 9 + k.len() + v.len()]).map_err(io)?;
        }
        w.flush().map_err(io)?;
        self.runs.push(path);
        self.buf.clear();
        self.index.clear();
        Ok(())
    }

    /// Insert every queued row in key order. Returns the number of rows.
    pub fn finish(mut self, db: &Db) -> Result<u64> {
        let mut out = Sink { db, batch: WriteBatch::new(), bytes: 0, rows: 0 };
        if self.runs.is_empty() {
            self.sort();
            for off in &self.index {
                let (t, k, v) = row(&self.buf, *off);
                out.put(t, k.to_vec(), v.to_vec())?;
            }
            return out.finish();
        }
        self.spill()?;
        self.buf = Vec::new();
        // merge the runs; of equal keys the one from the earlier run goes
        // first, so that the row put last is the one that stays
        let mut runs = self.runs.iter().map(|p| Run::open(p)).collect::<Result<Vec<_>>>()?;
        let mut heap = BinaryHeap::new();
        for (i, r) in runs.iter_mut().enumerate() {
            if let Some((t, k, v)) = r.next()? {
                heap.push(Reverse((t, k, i, v)));
            }
        }
        while let Some(Reverse((t, k, i, v))) = heap.pop() {
            out.put(t, k, v)?;
            if let Some((t, k, v)) = runs[i].next()? {
                heap.push(Reverse((t, k, i, v)));
            }
        }
        out.finish()
    }
}

impl Drop for BulkLoader {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run(BufReader<File>);

impl Run {
    fn open(p: &Path) -> Result<Run> {
        Ok(Run(BufReader::with_capacity(1 << 20, File::open(p).map_err(io)?)))
    }

    fn next(&mut self) -> Result<Option<(u8, Vec<u8>, Vec<u8>)>> {
        let mut head = [0u8; 5];
        match self.0.read_exact(&mut head[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(io(e)),
        }
        self.0.read_exact(&mut head[1..]).map_err(io)?;
        let mut k = vec![0u8; u32::from_le_bytes(head[1..].try_into().unwrap()) as usize];
        self.0.read_exact(&mut k).map_err(io)?;
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).map_err(io)?;
        let mut v = vec![0u8; u32::from_le_bytes(len) as usize];
        self.0.read_exact(&mut v).map_err(io)?;
        Ok(Some((head[0], k, v)))
    }
}

/// Writes sorted rows in transactions of bounded size.
struct Sink<'a> {
    db: &'a Db,
    batch: WriteBatch,
    bytes: usize,
    rows: u64,
}

impl Sink<'_> {
    fn put(&mut self, t: u8, k: Vec<u8>, v: Vec<u8>) -> Result<()> {
        let t = Table::from_u8(t).ok_or_else(|| StorageError::Db("bulk load: unknown table".into()))?;
        self.bytes += k.len() + v.len();
        self.rows += 1;
        self.batch.put(t, k, v);
        if self.bytes >= BATCH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.bytes = 0;
        self.db.write(std::mem::take(&mut self.batch), false)
    }

    fn finish(mut self) -> Result<u64> {
        self.flush()?;
        Ok(self.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(memory: usize) {
        let (db, guard) = Db::open_temporary().unwrap();
        let dir = guard.0.join("spool");
        let mut l = BulkLoader::new(&dir, memory).unwrap();
        // keys in a scattered order, two tables interleaved, some keys twice
        let n = 5000u32;
        for i in 0..n {
            let k = (i * 7919 % n).to_be_bytes();
            l.put(Table::History, &k, &i.to_le_bytes()).unwrap();
            l.put(Table::TxIndex, &k, &[1u8; 40]).unwrap();
        }
        for i in 0..100u32 {
            l.put(Table::History, &i.to_be_bytes(), b"last").unwrap();
        }
        assert_eq!(l.finish(&db).unwrap(), 2 * n as u64 + 100);
        assert!(!dir.exists(), "temporary files are removed");
        assert_eq!(db.count(Table::History).unwrap(), n as u64);
        assert_eq!(db.count(Table::TxIndex).unwrap(), n as u64);
        assert_eq!(db.get(Table::History, &7u32.to_be_bytes()).unwrap().unwrap(), b"last");
        let k = 4000u32;
        let i = (0..n).find(|i| i * 7919 % n == k).unwrap();
        assert_eq!(db.get(Table::History, &k.to_be_bytes()).unwrap().unwrap(), i.to_le_bytes());
    }

    #[test]
    fn rows_are_loaded_whether_or_not_they_fit_in_memory() {
        load(1 << 30);
        // many runs on disk (the minimum run size is 64 KiB)
        load(1);
    }
}
