//! Portable state snapshots ("XSNP" format, version 2).
//!
//! A snapshot is a complete copy of what the exporting node knows: every
//! account, every block (with its raw data whenever the node has it), the
//! main-chain index and the execution records. It is how an xdagj node's state
//! is carried over (written by `tools/xdagj-exporter`) and how state moves
//! between implementations. Loading a snapshot never deletes anything.
//!
//! ```text
//! "XSNP" u8:version=2 u8:network u64:nmain hashlow:top [32]:top_diff(BE)
//! u64:len randomx_schedule             (len 0: derived by the importer)
//! u64:n  { [20]:address u8:kind(0=C-units u64, 1=wei u128) balance u64:nonce
//!          opt[32]:code_hash }
//! u64:n  { hashlow [32]:hash(0=unknown) u64:time u8:flags u8:status u64:height
//!          [32]:diff(BE) opt hashlow:max_diff_link i64:amount u64:fee
//!          opt[32]:remark u8:kind data opt hashlow:ref }
//! u64:n  { u64:height hashlow }
//! u8:n   { u8:table u64:n { u32:len key u32:len value } }
//! ```
//!
//! Integers are little-endian, `opt x` is a presence byte followed by `x`.
//!
//! Block `kind`: 3 = the block's 512 raw bytes, 4 = raw bytes + u64 length +
//! Nova payload. Blocks whose data the exporter does not have (xdagj keeps
//! only metadata for blocks it inherited from its own snapshot) are carried
//! as 0 = no key (unspendable), 1 = compressed public key (33 bytes) or
//! 2 = raw block kept as key material only (512 bytes).
//!
//! `status` is the execution result of the block's own transaction
//! ([`TxStatus`]), or [`STATUS_UNKNOWN`] to let the importer derive it from
//! the flags. The trailing tables are raw copies of execution records
//! (contract code and storage, history, transaction index, receipts).
//!
//! Nothing is pruned, deliberately. Whether a block has been executed is
//! consensus state, and XDAG puts no lower bound on a block's timestamp or on
//! how old a referenced block may be: a node that forgot old executed blocks
//! could not tell a block that is sent again from a new one, and would execute
//! it twice or not at all. Peers also compare per-range block checksums when
//! they synchronise, which only match between nodes holding the same blocks.
//!
//! Main blocks up to `nmain` are final on the importing node: it never adopts
//! a chain that forks below them (their undo journals are not carried).

use std::io::{Read, Write};
use std::sync::Arc;

use xdag_storage::{BulkLoader, Db, Table};
use xdag_types::{Address, BlockHash, Difficulty, HashLow, Nano, Network, U256};

use crate::chain::Chain;
use crate::keys;
use crate::pow::{RxSchedule, Seed};
use crate::records::{flags, AccountRecord, Balance, BlockInfo, BlockState, ChainMeta, SnapshotKey, TxStatus};
use crate::{ChainError, Result};

pub const MAGIC: &[u8; 4] = b"XSNP";
pub const VERSION: u8 = 2;

/// Block status value meaning "derive it from the flags" (xdagj exports).
pub const STATUS_UNKNOWN: u8 = 0xff;

/// Execution-record tables carried verbatim.
pub const RECORD_TABLES: [Table; 6] = [Table::Code, Table::Storage, Table::History, Table::TxIndex, Table::Receipt, Table::EvmTxs];

/// Rows an import sorts in memory before it goes through temporary files.
const LOADER_MEMORY: usize = 64 << 20;

const MAX_ROW_KEY: usize = 1 << 10;
const MAX_ROW_VALUE: usize = 16 << 20;

/// Key material / data of a snapshot block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotData {
    None,
    Key(SnapshotKey),
    /// A regular block (with its Nova payload, if any).
    Full {
        raw: Box<[u8; 512]>,
        payload: Option<Vec<u8>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotBlock {
    pub hashlow: HashLow,
    pub hash: BlockHash,
    pub time: u64,
    /// xdagj combined flags.
    pub flags: u8,
    /// [`TxStatus`] of the block's own transaction, or [`STATUS_UNKNOWN`].
    pub status: u8,
    pub height: u64,
    pub difficulty: Difficulty,
    pub max_diff_link: Option<HashLow>,
    pub amount: i64,
    pub fee: Nano,
    pub remark: Option<[u8; 32]>,
    pub data: SnapshotData,
    /// Main block that processed this block (execution state), if any.
    pub ref_: Option<HashLow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub network: u8,
    pub nmain: u64,
    pub top: HashLow,
    pub top_diff: Difficulty,
    /// RandomX fork state. `None`: the importer derives it from the main chain.
    pub rx: Option<RxSchedule>,
}

fn io(e: std::io::Error) -> ChainError {
    ChainError::Other(format!("snapshot i/o: {e}"))
}

fn order() -> ChainError {
    ChainError::Other("snapshot sections written out of order".into())
}

/// Streaming writer: header, then the account, block and main-index sections
/// (each announced with [`SnapshotWriter::section`]), then the record tables.
pub struct SnapshotWriter<T: Write> {
    out: T,
    /// 1 accounts, 2 blocks, 3 main index, 4 record tables.
    stage: u8,
    /// Entries the current section still expects.
    open: u64,
    /// Record tables still expected.
    tables: u8,
}

impl<T: Write> SnapshotWriter<T> {
    pub fn new(out: T, h: &SnapshotHeader) -> Result<Self> {
        let mut w = SnapshotWriter { out, stage: 0, open: 0, tables: 0 };
        w.b(MAGIC)?;
        w.u8(VERSION)?;
        w.u8(h.network)?;
        w.u64(h.nmain)?;
        w.b(&h.top.0)?;
        w.b(&h.top_diff.to_be_bytes::<32>())?;
        let rx = h.rx.as_ref().map(|r| r.encode()).unwrap_or_default();
        w.u64(rx.len() as u64)?;
        w.b(&rx)?;
        Ok(w)
    }

    fn b(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b).map_err(io)
    }
    fn u8(&mut self, v: u8) -> Result<()> {
        self.b(&[v])
    }
    fn u64(&mut self, v: u64) -> Result<()> {
        self.b(&v.to_le_bytes())
    }
    fn opt(&mut self, v: Option<&[u8]>) -> Result<()> {
        match v {
            Some(v) => {
                self.u8(1)?;
                self.b(v)
            }
            None => self.u8(0),
        }
    }
    fn entry(&mut self, stage: u8) -> Result<()> {
        if self.stage != stage || self.open == 0 {
            return Err(order());
        }
        self.open -= 1;
        Ok(())
    }

    /// Start the next of the three entry sections with its number of entries.
    pub fn section(&mut self, n: u64) -> Result<()> {
        if self.open != 0 || self.stage >= 3 {
            return Err(order());
        }
        self.stage += 1;
        self.open = n;
        self.u64(n)
    }

    pub fn account(&mut self, a: &Address, r: &AccountRecord) -> Result<()> {
        self.entry(1)?;
        self.b(&a.0)?;
        match r.balance {
            Balance::Legacy(c) => {
                self.u8(0)?;
                self.u64(c)?;
            }
            Balance::Wei(v) => {
                self.u8(1)?;
                self.b(&v.to_le_bytes())?;
            }
        }
        self.u64(r.nonce)?;
        self.opt(r.code_hash.as_ref().map(|h| &h[..]))
    }

    pub fn block(&mut self, b: &SnapshotBlock) -> Result<()> {
        self.entry(2)?;
        self.b(&b.hashlow.0)?;
        self.b(&b.hash.0)?;
        self.u64(b.time)?;
        self.u8(b.flags)?;
        self.u8(b.status)?;
        self.u64(b.height)?;
        self.b(&b.difficulty.to_be_bytes::<32>())?;
        self.opt(b.max_diff_link.as_ref().map(|h| &h.0[..]))?;
        self.b(&b.amount.to_le_bytes())?;
        self.u64(b.fee.0)?;
        self.opt(b.remark.as_ref().map(|r| &r[..]))?;
        match &b.data {
            SnapshotData::None => self.u8(0)?,
            SnapshotData::Key(SnapshotKey::PublicKey(k)) => {
                self.u8(1)?;
                self.b(k)?;
            }
            SnapshotData::Key(SnapshotKey::RawBlock(r)) => {
                self.u8(2)?;
                self.b(&r[..])?;
            }
            SnapshotData::Full { raw, payload } => {
                self.u8(if payload.is_some() { 4 } else { 3 })?;
                self.b(&raw[..])?;
                if let Some(pl) = payload {
                    self.u64(pl.len() as u64)?;
                    self.b(pl)?;
                }
            }
        }
        self.opt(b.ref_.as_ref().map(|h| &h.0[..]))
    }

    pub fn main(&mut self, height: u64, h: &HashLow) -> Result<()> {
        self.entry(3)?;
        self.u64(height)?;
        self.b(&h.0)
    }

    /// After the main index: the number of record tables that follow.
    pub fn tables(&mut self, n: u8) -> Result<()> {
        if self.stage != 3 || self.open != 0 {
            return Err(order());
        }
        self.stage = 4;
        self.tables = n;
        self.u8(n)
    }

    pub fn table(&mut self, t: Table, rows: u64) -> Result<()> {
        if self.stage != 4 || self.open != 0 || self.tables == 0 {
            return Err(order());
        }
        self.tables -= 1;
        self.open = rows;
        self.u8(t as u8)?;
        self.u64(rows)
    }

    pub fn row(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        self.entry(4)?;
        self.b(&(k.len() as u32).to_le_bytes())?;
        self.b(k)?;
        self.b(&(v.len() as u32).to_le_bytes())?;
        self.b(v)
    }

    pub fn finish(mut self) -> Result<T> {
        if self.stage != 4 || self.open != 0 || self.tables != 0 {
            return Err(order());
        }
        self.out.flush().map_err(io)?;
        Ok(self.out)
    }

    pub fn accounts(&mut self, accts: &[(Address, AccountRecord)]) -> Result<()> {
        self.section(accts.len() as u64)?;
        accts.iter().try_for_each(|(a, r)| self.account(a, r))
    }

    pub fn blocks(&mut self, blocks: &[SnapshotBlock]) -> Result<()> {
        self.section(blocks.len() as u64)?;
        blocks.iter().try_for_each(|b| self.block(b))
    }

    /// The main index, followed by no record tables (as xdagj exports have).
    pub fn main_index(mut self, idx: &[(u64, HashLow)]) -> Result<T> {
        self.section(idx.len() as u64)?;
        idx.iter().try_for_each(|(h, hl)| self.main(*h, hl))?;
        self.tables(0)?;
        self.finish()
    }
}

/// Streaming reader, the counterpart of [`SnapshotWriter`].
pub struct SnapshotReader<T: Read> {
    input: T,
    max_payload: usize,
}

impl<T: Read> SnapshotReader<T> {
    /// Read the header. `max_payload` bounds the Nova payload of a block.
    pub fn new(input: T, max_payload: usize) -> Result<(Self, SnapshotHeader)> {
        let mut r = SnapshotReader { input, max_payload };
        if &r.arr::<4>()? != MAGIC {
            return Err(ChainError::Invalid("not an XSNP snapshot".into()));
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(ChainError::Invalid(format!(
                "snapshot format {version} is not supported (this version reads format {VERSION}): export it again with a matching exporter"
            )));
        }
        let network = r.u8()?;
        let nmain = r.u64()?;
        let top = HashLow(r.arr::<24>()?);
        let top_diff = U256::from_be_bytes(r.arr::<32>()?);
        let rxlen = r.u64()? as usize;
        if rxlen > 1 << 20 {
            return Err(ChainError::Invalid("bad randomx schedule".into()));
        }
        let rx = match rxlen {
            0 => None,
            n => Some(RxSchedule::decode(&r.vec(n)?).ok_or_else(|| ChainError::Invalid("bad randomx schedule".into()))?),
        };
        Ok((r, SnapshotHeader { network, nmain, top, top_diff, rx }))
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut b = [0u8; N];
        self.input.read_exact(&mut b).map_err(io)?;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr::<8>()?))
    }
    fn vec(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.input.read_exact(&mut v).map_err(io)?;
        Ok(v)
    }
    fn opt<const N: usize>(&mut self) -> Result<Option<[u8; N]>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.arr::<N>()?)),
            k => Err(ChainError::Invalid(format!("bad presence byte {k}"))),
        }
    }

    /// Number of entries of the next section.
    pub fn section(&mut self) -> Result<u64> {
        self.u64()
    }

    pub fn account(&mut self) -> Result<(Address, AccountRecord)> {
        let a = Address(self.arr::<20>()?);
        let balance = match self.u8()? {
            0 => Balance::Legacy(self.u64()?),
            1 => Balance::Wei(u128::from_le_bytes(self.arr::<16>()?)),
            k => return Err(ChainError::Invalid(format!("bad balance kind {k}"))),
        };
        let nonce = self.u64()?;
        let code_hash = self.opt::<32>()?;
        Ok((a, AccountRecord { balance, nonce, pending_nonce: 0, code_hash }))
    }

    pub fn block(&mut self) -> Result<SnapshotBlock> {
        let hashlow = HashLow(self.arr::<24>()?);
        let hash = BlockHash(self.arr::<32>()?);
        let time = self.u64()?;
        let flags = self.u8()?;
        let status = self.u8()?;
        if status > TxStatus::Failed as u8 && status != STATUS_UNKNOWN {
            return Err(ChainError::Invalid(format!("bad block status {status}")));
        }
        let height = self.u64()?;
        let difficulty = U256::from_be_bytes(self.arr::<32>()?);
        let max_diff_link = self.opt::<24>()?.map(HashLow);
        let amount = i64::from_le_bytes(self.arr::<8>()?);
        let fee = Nano(self.u64()?);
        let remark = self.opt::<32>()?;
        let data = match self.u8()? {
            0 => SnapshotData::None,
            1 => SnapshotData::Key(SnapshotKey::PublicKey(self.arr::<33>()?)),
            2 => SnapshotData::Key(SnapshotKey::RawBlock(Box::new(self.arr::<512>()?))),
            3 => SnapshotData::Full { raw: Box::new(self.arr::<512>()?), payload: None },
            4 => {
                let raw = Box::new(self.arr::<512>()?);
                let len = self.u64()? as usize;
                if len > self.max_payload {
                    return Err(ChainError::Invalid("snapshot payload too large".into()));
                }
                SnapshotData::Full { raw, payload: Some(self.vec(len)?) }
            }
            k => return Err(ChainError::Invalid(format!("bad snapshot block kind {k}"))),
        };
        let ref_ = self.opt::<24>()?.map(HashLow);
        Ok(SnapshotBlock { hashlow, hash, time, flags, status, height, difficulty, max_diff_link, amount, fee, remark, data, ref_ })
    }

    pub fn main(&mut self) -> Result<(u64, HashLow)> {
        Ok((self.u64()?, HashLow(self.arr::<24>()?)))
    }

    /// Number of record tables after the main index.
    pub fn tables(&mut self) -> Result<u8> {
        self.u8()
    }

    pub fn table(&mut self) -> Result<(Table, u64)> {
        let id = self.u8()?;
        let t = Table::from_u8(id)
            .filter(|t| RECORD_TABLES.contains(t))
            .ok_or_else(|| ChainError::Invalid(format!("unexpected table {id} in snapshot")))?;
        Ok((t, self.u64()?))
    }

    pub fn row(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        let klen = u32::from_le_bytes(self.arr::<4>()?) as usize;
        if klen > MAX_ROW_KEY {
            return Err(ChainError::Invalid("snapshot record key too large".into()));
        }
        let k = self.vec(klen)?;
        let vlen = u32::from_le_bytes(self.arr::<4>()?) as usize;
        if vlen > MAX_ROW_VALUE {
            return Err(ChainError::Invalid("snapshot record too large".into()));
        }
        Ok((k, self.vec(vlen)?))
    }

    /// The snapshot must end here.
    pub fn finish(mut self) -> Result<()> {
        let mut trailing = [0u8; 1];
        match self.input.read(&mut trailing).map_err(io)? {
            0 => Ok(()),
            _ => Err(ChainError::Invalid("trailing data after snapshot".into())),
        }
    }
}

/// Load a snapshot into an empty chain database (archive tables may already
/// hold imported history; nothing is ever deleted).
///
/// A snapshot that is not for this network or database is refused before
/// anything is written. If the import fails later, the database stays marked
/// as incomplete and `Chain::open` refuses it: it has to be deleted.
pub fn import<T: Read>(chain: &mut Chain, input: &mut T) -> Result<SnapshotHeader> {
    if chain.meta().nmain > 0 || chain.meta().top.is_some() || chain.meta().nblocks > 0 {
        return Err(ChainError::Invalid("the chain database is not empty".into()));
    }
    let params = chain.params().clone();
    let ep = params.epochs();
    let max_payload = params.nova.as_ref().map(|n| n.max_payload_bytes).unwrap_or(0);
    let (mut r, header) = SnapshotReader::new(input, max_payload)?;
    if Network::from_id(header.network) != Some(params.network) {
        return Err(ChainError::Invalid("snapshot is for another network".into()));
    }

    // The import is written in several commits. Mark the database first, so
    // that a failed or interrupted import can never be opened as a chain.
    chain.ov.put(Table::Meta, keys::META_SNAPSHOT_IMPORT.to_vec(), vec![1])?;
    chain.commit(true)?;

    // Blocks are keyed by their hash and arrive in any order: the rows are
    // sorted before they are inserted (see `BulkLoader`).
    let mut rows = BulkLoader::new(&chain.db().path().with_extension("import"), LOADER_MEMORY)?;

    for _ in 0..r.section()? {
        let (a, rec) = r.account()?;
        rows.put(Table::Account, &a.0, &rec.encode())?;
    }

    let mut stored = 0u64;
    // highest main block that comes without its data: the exporter inherited
    // it from a snapshot of its own
    let mut inherited_main: Option<u64> = None;
    for i in 0..r.section()? {
        let blk = r.block()?;
        let (hashlow, fl) = (blk.hashlow, blk.flags);
        if fl & flags::MAIN != 0 && !matches!(blk.data, SnapshotData::Full { .. }) {
            inherited_main = inherited_main.max(Some(blk.height));
        }
        let mut info = BlockInfo {
            hash: blk.hash,
            time: blk.time,
            type_word: 0,
            // our-block marks belong to the exporting node's wallet
            flags: fl & !flags::APPLY_MASK & !flags::EXTRA & !flags::OURS,
            difficulty: blk.difficulty,
            max_diff_link: blk.max_diff_link,
            remark: blk.remark,
            snapshot: None,
            ext_root: None,
            payload_txs: 0,
        };
        let mut status = if blk.status == STATUS_UNKNOWN { TxStatus::Pending as u8 } else { blk.status };
        match blk.data {
            SnapshotData::Full { mut raw, payload } => {
                let parse = |raw: &[u8; 512]| xdag_types::Block::parse(&raw[..]).map_err(|e| ChainError::Invalid(format!("snapshot block: {e}")));
                let mut b = parse(&raw)?;
                if b.hashlow() != hashlow && raw[..8] != [0u8; 8] {
                    // stored with a transport header, which is not part of the block's identity
                    raw[..8].fill(0);
                    b = parse(&raw)?;
                }
                if b.hashlow() != hashlow || (blk.hash != BlockHash([0u8; 32]) && b.hash() != blk.hash) || b.time != blk.time {
                    return Err(ChainError::Invalid(format!("snapshot block {hashlow:?} does not match its data")));
                }
                info.hash = b.hash();
                let payload = payload.map(Arc::new);
                let pre = crate::preverify::preverify(&b, payload.clone(), &params, chain.evm().map(|e| &**e))
                    .map_err(|e| ChainError::Invalid(format!("snapshot block {hashlow:?}: {e}")))?;
                info.type_word = b.type_word;
                if let Some(pl) = &pre.payload {
                    info.ext_root = b.ext_root;
                    info.payload_txs = pl.txs.len() as u32;
                    rows.put(Table::Payload, &hashlow.0, payload.as_deref().map(|p| &p[..]).unwrap_or_default())?;
                }
                // a processed transaction was applied, rejected (it then has
                // the main block that judged it) or skipped over a nonce gap
                if blk.status == STATUS_UNKNOWN && b.is_tx() && fl & flags::MAIN_REF != 0 {
                    status = if fl & flags::APPLIED != 0 {
                        TxStatus::Applied
                    } else if blk.ref_.is_some() {
                        TxStatus::Rejected
                    } else {
                        TxStatus::Pending
                    } as u8;
                }
                rows.put(Table::BlockRaw, &hashlow.0, &raw[..])?;
                rows.put(Table::TimeIndex, &keys::time_index(ep.epoch(blk.time), &hashlow), &[])?;
                crate::sums::add_block(&mut chain.ov, blk.time, b.sum())?;
                if fl & flags::REF == 0 {
                    rows.put(Table::NoRef, &hashlow.0, &[])?;
                }
                stored += 1;
            }
            SnapshotData::Key(k) => info.snapshot = Some(k),
            // unknown key: unspendable, but still a valid reference target
            SnapshotData::None => info.snapshot = Some(SnapshotKey::PublicKey([0u8; 33])),
        }
        rows.put(Table::BlockInfo, &hashlow.0, &info.encode())?;
        let st = BlockState { flags: fl & flags::APPLY_MASK, amount: blk.amount, fee: blk.fee, ref_: blk.ref_, height: blk.height, status };
        // blocks no main block has touched have no execution state
        if st != BlockState::default() {
            rows.put(Table::BlockState, &hashlow.0, &st.encode())?;
        }
        if i % 100_000 == 99_999 {
            // the per-range block checksums accumulate in the overlay
            chain.commit(false)?;
        }
    }

    for _ in 0..r.section()? {
        let (h, hl) = r.main()?;
        rows.put(Table::MainHeight, &keys::height(h), &hl.0)?;
    }

    for _ in 0..r.tables()? {
        let (t, n) = r.table()?;
        for _ in 0..n {
            let (k, v) = r.row()?;
            rows.put(t, &k, &v)?;
        }
    }
    r.finish()?;
    chain.commit(false)?;
    rows.finish(chain.db())?;

    let (nmain, top, top_diff) = (header.nmain, header.top, header.top_diff);
    chain.meta = ChainMeta { nmain, top: Some(top), top_diff, nblocks: chain.meta.nblocks + stored, snapshot_height: nmain };
    chain.mark_meta_dirty();
    chain.commit(false)?;
    let rebuilt = rebuild_rx(chain, inherited_main)?;
    let rx = match header.rx {
        None => rebuilt,
        Some(sr) if same_effect(&sr, &rebuilt) => sr,
        Some(_) => return Err(ChainError::Invalid("snapshot RandomX schedule does not match its main chain".into())),
    };
    chain.rx = rx.clone();
    chain.mark_meta_dirty();
    chain.ov.delete(Table::Meta, keys::META_SNAPSHOT_IMPORT.to_vec())?;
    chain.commit(true)?;
    Ok(SnapshotHeader { network: header.network, nmain, top, top_diff, rx: Some(rx) })
}

/// The RandomX state new blocks depend on (fork epoch and the last two seeds),
/// as replaying every main block would leave it.
///
/// A chain taken over from an xdagj node that was itself started from a
/// snapshot (`inherited` is the highest main block it has no data for, its
/// snapshot height) lacks the fork block. Such a node counts the fork from the
/// last seed height at or below its snapshot height and scores older
/// candidates by sha256d (`randomXLoadingSnapshotJ`); the line is drawn in the
/// same place here.
fn rebuild_rx(chain: &mut Chain, inherited: Option<u64>) -> Result<RxSchedule> {
    let p = chain.params().clone();
    let rx = &p.randomx;
    let ep = p.epochs();
    let nmain = chain.meta().nmain;
    let mut s = RxSchedule::default();
    if nmain < rx.fork_height {
        return Ok(s);
    }
    let missing = |h: u64| ChainError::Invalid(format!("snapshot lacks main block {h} needed for the RandomX schedule"));
    let base = match inherited {
        Some(h) if h > rx.fork_height => h - h % rx.seed_epoch_blocks,
        _ => rx.fork_height,
    };
    let (_, fork_time) = main_time(chain, base)?.ok_or_else(|| missing(base))?;
    s.fork_epoch = Some(ep.epoch(fork_time) + rx.seed_lag);
    let last = nmain & !(rx.seed_epoch_blocks - 1);
    for h in [last.checked_sub(rx.seed_epoch_blocks), Some(last)].into_iter().flatten() {
        if h < rx.fork_height {
            continue;
        }
        let (_, t) = main_time(chain, h)?.ok_or_else(|| missing(h))?;
        let (src, _) = main_time(chain, h - rx.seed_lag)?.ok_or_else(|| missing(h - rx.seed_lag))?;
        let mut key = [0u8; 32];
        key[..24].copy_from_slice(&src.0);
        s.seeds.push(Seed { height: h, switch_epoch: ep.epoch(t) + rx.seed_lag + 1, key });
    }
    Ok(s)
}

fn main_time(chain: &mut Chain, h: u64) -> Result<Option<(HashLow, u64)>> {
    let Some(hl) = chain.main_at(h)? else { return Ok(None) };
    Ok(chain.info(&hl)?.map(|i| (hl, i.time)))
}

/// Same fork epoch and same two newest seeds.
fn same_effect(a: &RxSchedule, b: &RxSchedule) -> bool {
    let tail = |s: &RxSchedule| s.seeds.iter().rev().take(2).cloned().collect::<Vec<_>>();
    a.fork_epoch == b.fork_epoch && tail(a) == tail(b)
}

/// `Db::for_each` with a fallible body.
fn each(db: &Db, t: Table, mut f: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
    let mut failed = None;
    db.for_each(t, |k, v| match f(k, v) {
        Ok(()) => true,
        Err(e) => {
            failed = Some(e);
            false
        }
    })?;
    failed.map_or(Ok(()), Err)
}

/// Export everything the node knows (streamed straight from the database).
pub fn export<T: Write>(chain: &mut Chain, out: T) -> Result<T> {
    chain.commit(true)?;
    let db = chain.db().clone();
    let meta = chain.meta().clone();
    // an unsaved in-memory candidate cannot be exported: fall back to the
    // last main block (the importing node re-selects its top from there)
    let (top, top_diff) = match meta.top {
        Some(t) if db.contains(Table::BlockInfo, &t.0)? => (t, meta.top_diff),
        _ => match chain.main_at(meta.nmain)? {
            Some(m) => (m, chain.info(&m)?.map(|i| i.difficulty).unwrap_or_default()),
            None => (HashLow::ZERO, U256::ZERO),
        },
    };
    let header = SnapshotHeader { network: chain.params().network.id(), nmain: meta.nmain, top, top_diff, rx: Some(chain.rx_schedule().clone()) };
    let mut w = SnapshotWriter::new(out, &header)?;
    let bad_key = |t: &str| ChainError::Corrupt(format!("{t} key"));

    w.section(db.count(Table::Account)?)?;
    each(&db, Table::Account, |k, v| {
        let a = Address::from_slice(k).ok_or_else(|| bad_key("account"))?;
        w.account(&a, &AccountRecord::decode(v)?)
    })?;

    w.section(db.count(Table::BlockInfo)?)?;
    each(&db, Table::BlockInfo, |k, v| {
        let h = HashLow::from_slice(k).ok_or_else(|| bad_key("block"))?;
        let info = BlockInfo::decode(v)?;
        let s = match db.get(Table::BlockState, k)? {
            Some(b) => BlockState::decode(&b)?,
            None => BlockState::default(),
        };
        let data = match &info.snapshot {
            Some(key) => SnapshotData::Key(key.clone()),
            None => {
                let raw = db.get(Table::BlockRaw, k)?.ok_or_else(|| ChainError::Corrupt(format!("block {h:?} has no data")))?;
                let raw: [u8; 512] = raw.try_into().map_err(|_| ChainError::Corrupt(format!("block {h:?} has a bad size")))?;
                let payload = if info.ext_root.is_some() { db.get(Table::Payload, k)? } else { None };
                SnapshotData::Full { raw: Box::new(raw), payload }
            }
        };
        w.block(&SnapshotBlock {
            hashlow: h,
            hash: info.hash,
            time: info.time,
            flags: info.flags | s.flags,
            status: s.status,
            height: s.height,
            difficulty: info.difficulty,
            max_diff_link: info.max_diff_link,
            amount: s.amount,
            fee: s.fee,
            remark: info.remark,
            data,
            ref_: s.ref_,
        })
    })?;

    w.section(db.count(Table::MainHeight)?)?;
    each(&db, Table::MainHeight, |k, v| {
        let height = u64::from_be_bytes(k.try_into().map_err(|_| bad_key("main height"))?);
        w.main(height, &HashLow::from_slice(v).ok_or_else(|| bad_key("main height"))?)
    })?;

    w.tables(RECORD_TABLES.len() as u8)?;
    for t in RECORD_TABLES {
        w.table(t, db.count(t)?)?;
        each(&db, t, |k, v| w.row(k, v))?;
    }
    w.finish()
}

/// What a snapshot file contains (`xdagd snapshot info`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotSummary {
    pub header: SnapshotHeader,
    pub accounts: u64,
    /// Running hash over the account entries in file order:
    /// `d = sha256(d ‖ address ‖ kind ‖ balance ‖ nonce ‖ code hash)`, from 32 zero bytes.
    pub accounts_digest: [u8; 32],
    /// Accounts are in ascending address order (as both exporters write
    /// them): equal account sets then give equal digests.
    pub accounts_sorted: bool,
    /// Sum of legacy balances, converted to nano the way xdagj displays them.
    pub legacy_nano: u128,
    /// Sum of Nova balances.
    pub wei: u128,
    pub blocks: u64,
    pub blocks_with_data: u64,
    /// Sum of block balances (nano).
    pub block_nano: i128,
    pub mains: u64,
    pub records: Vec<(Table, u64)>,
}

/// Read a snapshot through without importing it.
pub fn describe<T: Read>(input: T) -> Result<SnapshotSummary> {
    let (mut r, header) = SnapshotReader::new(input, MAX_ROW_VALUE)?;
    let mut s = SnapshotSummary { header, accounts_sorted: true, ..Default::default() };
    s.accounts = r.section()?;
    let mut last: Option<Address> = None;
    for _ in 0..s.accounts {
        let (a, rec) = r.account()?;
        let mut e = s.accounts_digest.to_vec();
        e.extend_from_slice(&a.0);
        match rec.balance {
            Balance::Legacy(c) => {
                e.push(0);
                e.extend_from_slice(&c.to_le_bytes());
                let nano = xdag_types::CAmount(c).to_nano_legacy().map_err(|_| ChainError::Invalid(format!("balance of {a:?} out of range")))?;
                s.legacy_nano += nano.0 as u128;
            }
            Balance::Wei(w) => {
                e.push(1);
                e.extend_from_slice(&w.to_le_bytes());
                s.wei = s.wei.checked_add(w).ok_or(ChainError::Overflow)?;
            }
        }
        e.extend_from_slice(&rec.nonce.to_le_bytes());
        e.extend_from_slice(&rec.code_hash.unwrap_or([0u8; 32]));
        s.accounts_digest = xdag_types::hash::sha256(&e);
        s.accounts_sorted &= last.is_none_or(|l| l.0 < a.0);
        last = Some(a);
    }
    s.blocks = r.section()?;
    for _ in 0..s.blocks {
        let b = r.block()?;
        s.blocks_with_data += matches!(b.data, SnapshotData::Full { .. }) as u64;
        s.block_nano += b.amount as i128;
    }
    s.mains = r.section()?;
    for _ in 0..s.mains {
        r.main()?;
    }
    for _ in 0..r.tables()? {
        let (t, rows) = r.table()?;
        for _ in 0..rows {
            r.row()?;
        }
        s.records.push((t, rows));
    }
    r.finish()?;
    Ok(s)
}

/// Result of comparing two snapshots (`xdagd snapshot diff`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotDiff {
    /// Human-readable findings, most important first.
    pub lines: Vec<String>,
    /// Differences in consensus state: accounts, block balances, execution
    /// flags, fees, heights, difficulties and the main-chain index.
    pub state_differences: u64,
    /// Blocks only one side knows (each node holds a few unconfirmed blocks
    /// the other has not seen or has not kept).
    pub only_in_first: u64,
    pub only_in_second: u64,
}

/// What two nodes must agree on about a block.
struct BlockMeta {
    time: u64,
    flags: u8,
    status: u8,
    height: u64,
    difficulty: Difficulty,
    max_diff_link: Option<HashLow>,
    amount: i64,
    fee: Nano,
    ref_: Option<HashLow>,
    has_data: bool,
}

/// Marks of the exporting node that are not consensus state.
const LOCAL_FLAGS: u8 = flags::OURS | flags::EXTRA | flags::REMARK;

impl BlockMeta {
    fn of(b: &SnapshotBlock) -> BlockMeta {
        BlockMeta {
            time: b.time,
            flags: b.flags,
            status: b.status,
            height: b.height,
            difficulty: b.difficulty,
            max_diff_link: b.max_diff_link,
            amount: b.amount,
            fee: b.fee,
            ref_: b.ref_,
            has_data: matches!(b.data, SnapshotData::Full { .. }),
        }
    }

    /// Everything that is compared, condensed (the status apart: an exporter
    /// may not know it).
    fn digest(&self) -> [u8; 7] {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.time, self.flags & !LOCAL_FLAGS, self.height, self.difficulty, self.max_diff_link, self.amount, self.fee.0, self.ref_, self.has_data)
            .hash(&mut h);
        let mut d = [0u8; 7];
        d.copy_from_slice(&h.finish().to_le_bytes()[..7]);
        d
    }

    fn same_status(a: u8, b: u8) -> bool {
        a == b || a == STATUS_UNKNOWN || b == STATUS_UNKNOWN
    }

    fn differences(&self, o: &BlockMeta) -> Vec<String> {
        let mut what = vec![];
        if self.time != o.time {
            what.push(format!("time {:x}/{:x}", self.time, o.time));
        }
        if (self.flags & !LOCAL_FLAGS) != (o.flags & !LOCAL_FLAGS) {
            what.push(format!("flags {:02x}/{:02x}", self.flags & !LOCAL_FLAGS, o.flags & !LOCAL_FLAGS));
        }
        if !Self::same_status(self.status, o.status) {
            what.push(format!("status {}/{}", self.status, o.status));
        }
        if self.height != o.height {
            what.push(format!("height {}/{}", self.height, o.height));
        }
        if self.difficulty != o.difficulty {
            what.push(format!("difficulty {:x}/{:x}", self.difficulty, o.difficulty));
        }
        if self.max_diff_link != o.max_diff_link {
            what.push(format!("max-diff link {:?}/{:?}", self.max_diff_link, o.max_diff_link));
        }
        if self.amount != o.amount {
            what.push(format!("balance {}/{} nano", self.amount, o.amount));
        }
        if self.fee != o.fee {
            what.push(format!("fee {}/{} nano", self.fee.0, o.fee.0));
        }
        if self.ref_ != o.ref_ {
            what.push(format!("ref {:?}/{:?}", self.ref_, o.ref_));
        }
        if self.has_data != o.has_data {
            what.push(format!("block data present {}/{}", self.has_data, o.has_data));
        }
        what
    }
}

/// Compare the consensus state in two snapshots taken at the same point of
/// the same chain, e.g. one exported from an xdagj node and one from an xdagd
/// node that followed it. Node-local marks (our-block flag) are ignored.
///
/// The second snapshot is streamed against 32 bytes per block kept of the
/// first, which is read once more to describe the blocks that differ.
pub fn diff<A: Read, B: Read>(mut first: impl FnMut() -> Result<A>, second: B) -> Result<SnapshotDiff> {
    const SHOWN: usize = 8;
    let (mut ra, ha) = SnapshotReader::new(first()?, MAX_ROW_VALUE)?;
    let mut accounts = std::collections::BTreeMap::new();
    for _ in 0..ra.section()? {
        let (a, rec) = ra.account()?;
        // legacy balances compare by the value xdagj shows for them
        accounts.insert(a, (rec.balance_wei()?, rec.nonce, rec.code_hash));
    }
    let count_a = (accounts.len(), ra.section()?);
    let mut blocks: Vec<(HashLow, [u8; 7], u8)> = Vec::with_capacity(count_a.1 as usize);
    for _ in 0..count_a.1 {
        let b = ra.block()?;
        let m = BlockMeta::of(&b);
        blocks.push((b.hashlow, m.digest(), m.status));
    }
    blocks.sort_unstable_by_key(|e| e.0);
    let mut mains: Vec<(u64, HashLow)> = (0..ra.section()?).map(|_| ra.main()).collect::<Result<_>>()?;
    mains.sort_unstable();
    drop(ra);

    let (mut rb, hb) = SnapshotReader::new(second, MAX_ROW_VALUE)?;
    if ha.network != hb.network {
        return Err(ChainError::Invalid("the snapshots are of different networks".into()));
    }
    let mut d = SnapshotDiff::default();
    d.lines.push(format!("main height: {} / {}", ha.nmain, hb.nmain));
    if ha.nmain != hb.nmain {
        d.lines.push("  (different heights: states are only comparable at the same main height)".into());
    }
    let note = |d: &mut SnapshotDiff, shown: &mut usize, line: String| {
        d.state_differences += 1;
        *shown += 1;
        if *shown <= SHOWN {
            d.lines.push(line);
        }
    };

    let (mut shown, count_b) = (0, rb.section()?);
    for _ in 0..count_b {
        let (addr, rec) = rb.account()?;
        let vb = (rec.balance_wei()?, rec.nonce, rec.code_hash);
        match accounts.remove(&addr) {
            None => note(&mut d, &mut shown, format!("account {addr}: only in the second")),
            Some(va) if va != vb => {
                note(&mut d, &mut shown, format!("account {addr}: balance {} wei nonce {} / balance {} wei nonce {}", va.0, va.1, vb.0, vb.1))
            }
            _ => {}
        }
    }
    for addr in accounts.keys() {
        note(&mut d, &mut shown, format!("account {addr}: only in the first"));
    }
    d.lines.push(format!("accounts: {} / {count_b}, {shown} differ", count_a.0));

    let count_b = rb.section()?;
    let mut seen = vec![false; blocks.len()];
    let (mut differing, mut common) = (0usize, 0u64);
    let mut detail: std::collections::HashMap<HashLow, BlockMeta> = Default::default();
    for _ in 0..count_b {
        let b = rb.block()?;
        let Ok(i) = blocks.binary_search_by_key(&b.hashlow, |e| e.0) else {
            d.only_in_second += 1;
            continue;
        };
        seen[i] = true;
        common += 1;
        let m = BlockMeta::of(&b);
        if blocks[i].1 != m.digest() || !BlockMeta::same_status(blocks[i].2, m.status) {
            differing += 1;
            if detail.len() < SHOWN {
                detail.insert(b.hashlow, m);
            }
        }
    }
    d.only_in_first = seen.iter().filter(|s| !**s).count() as u64;

    let (mut shown, mut common_mains, mains_b) = (0, 0usize, rb.section()?);
    for _ in 0..mains_b {
        let (h, hl) = rb.main()?;
        if let Ok(i) = mains.binary_search_by_key(&h, |e| e.0) {
            common_mains += 1;
            if mains[i].1 != hl {
                note(&mut d, &mut shown, format!("main block {h}: {} / {}", mains[i].1.to_legacy_address(), hl.to_legacy_address()));
            }
        }
    }
    d.lines.push(format!("main-chain index: {} / {mains_b} entries, {common_mains} in common, {shown} differ", mains.len()));
    drop(rb);

    // the blocks that differ, described from both sides
    d.state_differences += differing as u64;
    if !detail.is_empty() {
        let (mut ra, _) = SnapshotReader::new(first()?, MAX_ROW_VALUE)?;
        for _ in 0..ra.section()? {
            ra.account()?;
        }
        for _ in 0..ra.section()? {
            let b = ra.block()?;
            if let Some(mb) = detail.get(&b.hashlow) {
                d.lines.push(format!("block {}: {}", b.hashlow.to_legacy_address(), BlockMeta::of(&b).differences(mb).join(", ")));
            }
        }
    }
    d.lines.push(format!(
        "blocks: {} / {count_b}, {common} in common of which {differing} differ; {} only in the first, {} only in the second",
        count_a.1, d.only_in_first, d.only_in_second
    ));
    Ok(d)
}
