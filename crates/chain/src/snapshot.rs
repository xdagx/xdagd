//! Portable state snapshots ("XSNP" format).
//!
//! Used to bootstrap from an existing xdagj network (converted with
//! `tools/xdagj-exporter`) and for fast node bootstrap. Unlike xdagj's
//! snapshot mechanism, loading a snapshot never deletes anything: blocks and
//! history already present are kept, and pre-snapshot history can be imported
//! separately into the archive tables (see `archive`).
//!
//! ```text
//! "XSNP" u8:version u8:network u64:nmain hashlow:top [32]:top_diff(BE)
//! u64:horizon_time
//! bytes:randomx_schedule
//! u64:n  { [20]:address u8:kind(0=C-units,1=wei) u64|u128:balance u64:nonce }
//! u64:n  { hashlow [32]:hash(0=unknown) u64:time u8:flags u64:height [32]:diff
//!          opt hashlow:max_diff_link i64:amount u64:fee opt[32]:remark
//!          u8:kind data  opt hashlow:ref }
//! u64:n  { u64:height hashlow }
//! ```
//!
//! Block `kind`: 0 = no key (unspendable), 1 = compressed public key (33),
//! 2 = raw block kept only as key material (512), 3 = full block (512),
//! 4 = full block (512) + u64 length + Nova payload.
//!
//! Full blocks are regular DAG blocks on the importing node (they can be
//! served to peers, referenced and, if still pending, applied later). The
//! exporter carries in full every block newer than `horizon_time` and every
//! block not yet processed by a main block; older processed blocks are kept
//! only if they hold a balance (or are main blocks). A block older than the
//! horizon that reaches the importing node later was therefore processed
//! before the snapshot, and is recorded as such instead of being applied again.
//!
//! Main blocks up to `nmain` are final on the importing node: it never adopts
//! a chain that forks below them.

use std::io::{Read, Write};
use std::sync::Arc;

use xdag_storage::Table;
use xdag_types::{Address, BlockHash, Difficulty, HashLow, Nano, Network, U256};

use crate::chain::Chain;
use crate::keys;
use crate::pow::{RxSchedule, Seed};
use crate::records::{flags, AccountRecord, Balance, BlockInfo, BlockState, ChainMeta, SnapshotKey};
use crate::{ChainError, Result};

pub const MAGIC: &[u8; 4] = b"XSNP";

/// Recent history `export` carries in full, in epochs before the top main block.
pub const HORIZON_EPOCHS: u64 = 1024;

/// Key material / data of a snapshot block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotData {
    None,
    Key(SnapshotKey),
    /// A block kept as a regular block (with its Nova payload, if any).
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
    pub horizon_time: u64,
    pub top: HashLow,
    pub top_diff: Difficulty,
    pub rx: RxSchedule,
}

struct W<'a, T: Write>(&'a mut T);
impl<T: Write> W<'_, T> {
    fn b(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.0.write_all(b)
    }
    fn u8(&mut self, v: u8) -> std::io::Result<()> {
        self.b(&[v])
    }
    fn u64(&mut self, v: u64) -> std::io::Result<()> {
        self.b(&v.to_le_bytes())
    }
}

struct R<'a, T: Read>(&'a mut T);
impl<T: Read> R<'_, T> {
    fn arr<const N: usize>(&mut self) -> std::io::Result<[u8; N]> {
        let mut b = [0u8; N];
        self.0.read_exact(&mut b)?;
        Ok(b)
    }
    fn u8(&mut self) -> std::io::Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    fn u64(&mut self) -> std::io::Result<u64> {
        Ok(u64::from_le_bytes(self.arr::<8>()?))
    }
    fn vec(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.0.read_exact(&mut v)?;
        Ok(v)
    }
}

fn io(e: std::io::Error) -> ChainError {
    ChainError::Other(format!("snapshot i/o: {e}"))
}

pub struct SnapshotWriter<T: Write> {
    out: T,
}

impl<T: Write> SnapshotWriter<T> {
    pub fn new(mut out: T, h: &SnapshotHeader) -> Result<Self> {
        {
            let mut w = W(&mut out);
            w.b(MAGIC).map_err(io)?;
            w.u8(1).map_err(io)?;
            w.u8(h.network).map_err(io)?;
            w.u64(h.nmain).map_err(io)?;
            w.b(&h.top.0).map_err(io)?;
            w.b(&h.top_diff.to_be_bytes::<32>()).map_err(io)?;
            w.u64(h.horizon_time).map_err(io)?;
            let rx = h.rx.encode();
            w.u64(rx.len() as u64).map_err(io)?;
            w.b(&rx).map_err(io)?;
        }
        Ok(SnapshotWriter { out })
    }

    pub fn accounts(&mut self, accts: &[(Address, AccountRecord)]) -> Result<()> {
        let mut w = W(&mut self.out);
        w.u64(accts.len() as u64).map_err(io)?;
        for (a, r) in accts {
            w.b(&a.0).map_err(io)?;
            match r.balance {
                Balance::Legacy(c) => {
                    w.u8(0).map_err(io)?;
                    w.u64(c).map_err(io)?;
                }
                Balance::Wei(v) => {
                    w.u8(1).map_err(io)?;
                    w.b(&v.to_le_bytes()).map_err(io)?;
                }
            }
            w.u64(r.nonce).map_err(io)?;
        }
        Ok(())
    }

    pub fn blocks(&mut self, blocks: &[SnapshotBlock]) -> Result<()> {
        let mut w = W(&mut self.out);
        w.u64(blocks.len() as u64).map_err(io)?;
        for b in blocks {
            w.b(&b.hashlow.0).map_err(io)?;
            w.b(&b.hash.0).map_err(io)?;
            w.u64(b.time).map_err(io)?;
            w.u8(b.flags).map_err(io)?;
            w.u64(b.height).map_err(io)?;
            w.b(&b.difficulty.to_be_bytes::<32>()).map_err(io)?;
            match &b.max_diff_link {
                Some(h) => {
                    w.u8(1).map_err(io)?;
                    w.b(&h.0).map_err(io)?;
                }
                None => w.u8(0).map_err(io)?,
            }
            w.b(&b.amount.to_le_bytes()).map_err(io)?;
            w.u64(b.fee.0).map_err(io)?;
            match &b.remark {
                Some(r) => {
                    w.u8(1).map_err(io)?;
                    w.b(r).map_err(io)?;
                }
                None => w.u8(0).map_err(io)?,
            }
            match &b.data {
                SnapshotData::None => w.u8(0).map_err(io)?,
                SnapshotData::Key(SnapshotKey::PublicKey(k)) => {
                    w.u8(1).map_err(io)?;
                    w.b(k).map_err(io)?;
                }
                SnapshotData::Key(SnapshotKey::RawBlock(r)) => {
                    w.u8(2).map_err(io)?;
                    w.b(&r[..]).map_err(io)?;
                }
                SnapshotData::Full { raw, payload } => {
                    w.u8(if payload.is_some() { 4 } else { 3 }).map_err(io)?;
                    w.b(&raw[..]).map_err(io)?;
                    if let Some(pl) = payload {
                        w.u64(pl.len() as u64).map_err(io)?;
                        w.b(pl).map_err(io)?;
                    }
                }
            }
            match &b.ref_ {
                Some(h) => {
                    w.u8(1).map_err(io)?;
                    w.b(&h.0).map_err(io)?;
                }
                None => w.u8(0).map_err(io)?,
            }
        }
        Ok(())
    }

    pub fn main_index(mut self, idx: &[(u64, HashLow)]) -> Result<T> {
        let mut w = W(&mut self.out);
        w.u64(idx.len() as u64).map_err(io)?;
        for (h, hl) in idx {
            w.u64(*h).map_err(io)?;
            w.b(&hl.0).map_err(io)?;
        }
        Ok(self.out)
    }
}

/// Load a snapshot into an empty chain database (archive tables may already
/// hold imported history; nothing is ever deleted).
pub fn import<T: Read>(chain: &mut Chain, input: &mut T) -> Result<SnapshotHeader> {
    if chain.meta().nmain > 0 || chain.meta().top.is_some() {
        return Err(ChainError::Invalid("the chain database is not empty".into()));
    }
    let mut r = R(input);
    if &r.arr::<4>().map_err(io)? != MAGIC {
        return Err(ChainError::Invalid("not an XSNP snapshot".into()));
    }
    if r.u8().map_err(io)? != 1 {
        return Err(ChainError::Invalid("unsupported snapshot version".into()));
    }
    let network = r.u8().map_err(io)?;
    if Network::from_id(network) != Some(chain.params().network) {
        return Err(ChainError::Invalid("snapshot is for another network".into()));
    }
    let nmain = r.u64().map_err(io)?;
    let top = HashLow(r.arr::<24>().map_err(io)?);
    let top_diff = U256::from_be_bytes(r.arr::<32>().map_err(io)?);
    let horizon_time = r.u64().map_err(io)?;
    let rxlen = r.u64().map_err(io)? as usize;
    if rxlen > 1 << 20 {
        return Err(ChainError::Invalid("bad randomx schedule".into()));
    }
    // empty = let the importer derive it (the xdagj exporter does this)
    let snap_rx = match rxlen {
        0 => None,
        n => Some(RxSchedule::decode(&r.vec(n).map_err(io)?).ok_or_else(|| ChainError::Invalid("bad randomx schedule".into()))?),
    };

    let n = r.u64().map_err(io)?;
    for i in 0..n {
        let a = Address(r.arr::<20>().map_err(io)?);
        let balance = match r.u8().map_err(io)? {
            0 => Balance::Legacy(r.u64().map_err(io)?),
            1 => Balance::Wei(u128::from_le_bytes(r.arr::<16>().map_err(io)?)),
            k => return Err(ChainError::Invalid(format!("bad balance kind {k}"))),
        };
        let nonce = r.u64().map_err(io)?;
        chain.put_account(&a, &AccountRecord { balance, nonce, pending_nonce: 0, code_hash: None })?;
        if i % 100_000 == 99_999 {
            chain.commit(false)?;
        }
    }
    let params = chain.params().clone();
    let ep = params.epochs();
    let mut stored = 0u64;
    let n = r.u64().map_err(io)?;
    for i in 0..n {
        let hashlow = HashLow(r.arr::<24>().map_err(io)?);
        let hash = BlockHash(r.arr::<32>().map_err(io)?);
        let time = r.u64().map_err(io)?;
        let fl = r.u8().map_err(io)?;
        let height = r.u64().map_err(io)?;
        let difficulty = U256::from_be_bytes(r.arr::<32>().map_err(io)?);
        let max_diff_link = if r.u8().map_err(io)? == 1 { Some(HashLow(r.arr::<24>().map_err(io)?)) } else { None };
        let amount = i64::from_le_bytes(r.arr::<8>().map_err(io)?);
        let fee = Nano(r.u64().map_err(io)?);
        let remark = if r.u8().map_err(io)? == 1 { Some(r.arr::<32>().map_err(io)?) } else { None };
        let data = match r.u8().map_err(io)? {
            0 => SnapshotData::None,
            1 => SnapshotData::Key(SnapshotKey::PublicKey(r.arr::<33>().map_err(io)?)),
            2 => SnapshotData::Key(SnapshotKey::RawBlock(Box::new(r.arr::<512>().map_err(io)?))),
            3 => SnapshotData::Full { raw: Box::new(r.arr::<512>().map_err(io)?), payload: None },
            4 => {
                let raw = Box::new(r.arr::<512>().map_err(io)?);
                let len = r.u64().map_err(io)? as usize;
                let max = params.nova.as_ref().map(|n| n.max_payload_bytes).unwrap_or(0);
                if len > max {
                    return Err(ChainError::Invalid("snapshot payload too large".into()));
                }
                SnapshotData::Full { raw, payload: Some(r.vec(len).map_err(io)?) }
            }
            k => return Err(ChainError::Invalid(format!("bad snapshot block kind {k}"))),
        };
        let ref_ = if r.u8().map_err(io)? == 1 { Some(HashLow(r.arr::<24>().map_err(io)?)) } else { None };
        let mut info = BlockInfo {
            hash,
            time,
            type_word: 0,
            // our-block marks belong to the exporting node's wallet
            flags: fl & !flags::APPLY_MASK & !flags::EXTRA & !flags::OURS,
            difficulty,
            max_diff_link,
            remark,
            snapshot: None,
            ext_root: None,
            payload_txs: 0,
        };
        match data {
            SnapshotData::Full { mut raw, payload } => {
                let parse = |raw: &[u8; 512]| xdag_types::Block::parse(&raw[..]).map_err(|e| ChainError::Invalid(format!("snapshot block: {e}")));
                let mut b = parse(&raw)?;
                if b.hashlow() != hashlow && raw[..8] != [0u8; 8] {
                    // stored with a transport header, which is not part of the block's identity
                    raw[..8].fill(0);
                    b = parse(&raw)?;
                }
                if b.hashlow() != hashlow || (hash != BlockHash([0u8; 32]) && b.hash() != hash) || b.time != time {
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
                    chain.ov.put(Table::Payload, hashlow.0.to_vec(), payload.as_deref().cloned().unwrap_or_default())?;
                }
                chain.ov.put(Table::BlockRaw, hashlow.0.to_vec(), raw.to_vec())?;
                chain.ov.put(Table::TimeIndex, keys::time_index(ep.epoch(time), &hashlow), vec![])?;
                crate::sums::add_block(&mut chain.ov, time, b.sum())?;
                if fl & flags::REF == 0 {
                    chain.ov.put(Table::NoRef, hashlow.0.to_vec(), vec![])?;
                }
                stored += 1;
            }
            SnapshotData::Key(k) => info.snapshot = Some(k),
            // unknown key: unspendable, but still a valid reference target
            SnapshotData::None => info.snapshot = Some(SnapshotKey::PublicKey([0u8; 33])),
        }
        chain.put_info(&hashlow, info)?;
        let st = BlockState {
            flags: fl & flags::APPLY_MASK,
            amount,
            fee,
            ref_: ref_.or(if fl & flags::MAIN != 0 { Some(hashlow) } else { None }),
            height,
            status: 0,
        };
        chain.put_state(&hashlow, &st)?;
        if i % 100_000 == 99_999 {
            chain.commit(false)?;
        }
    }
    let n = r.u64().map_err(io)?;
    for _ in 0..n {
        let h = r.u64().map_err(io)?;
        let hl = HashLow(r.arr::<24>().map_err(io)?);
        chain.ov.put(Table::MainHeight, keys::height(h), hl.0.to_vec())?;
    }
    let mut trailing = [0u8; 1];
    if r.0.read(&mut trailing).map_err(io)? != 0 {
        return Err(ChainError::Invalid("trailing data after snapshot".into()));
    }
    chain.meta =
        ChainMeta { nmain, top: Some(top), top_diff, nblocks: chain.meta.nblocks + stored, snapshot_height: nmain, snapshot_time: horizon_time };
    chain.mark_meta_dirty();
    chain.commit(false)?;
    let rebuilt = rebuild_rx(chain)?;
    let rx = match snap_rx {
        None => rebuilt,
        Some(sr) if same_effect(&sr, &rebuilt) => sr,
        Some(_) => return Err(ChainError::Invalid("snapshot RandomX schedule does not match its main chain".into())),
    };
    chain.rx = rx.clone();
    chain.mark_meta_dirty();
    chain.commit(true)?;
    Ok(SnapshotHeader { network, nmain, horizon_time, top, top_diff, rx })
}

/// The RandomX state new blocks depend on (fork epoch and the last two seeds),
/// as replaying every main block would leave it. When the fork block itself is
/// not part of the history (nodes bootstrapped from an xdagj snapshot lack
/// it), the fork epoch comes from the first main block known after it, as
/// xdagj does after loading its snapshots: any past epoch is equivalent for
/// new blocks.
fn rebuild_rx(chain: &mut Chain) -> Result<RxSchedule> {
    let p = chain.params().clone();
    let rx = &p.randomx;
    let ep = p.epochs();
    let nmain = chain.meta().nmain;
    let mut s = RxSchedule::default();
    if nmain < rx.fork_height {
        return Ok(s);
    }
    let missing = |h: u64| ChainError::Invalid(format!("snapshot lacks main block {h} needed for the RandomX schedule"));
    let fork_time = match main_time(chain, rx.fork_height)? {
        Some((_, t)) => t,
        None => {
            let first = chain.db().scan_range(Table::MainHeight, &keys::height(rx.fork_height), None, 1, false)?;
            let (_, v) = first.into_iter().next().ok_or_else(|| missing(rx.fork_height))?;
            let hl = HashLow::from_slice(&v).ok_or_else(|| missing(rx.fork_height))?;
            chain.info(&hl)?.ok_or_else(|| missing(rx.fork_height))?.time
        }
    };
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

/// Export the current state with the default horizon (`HORIZON_EPOCHS`).
pub fn export<T: Write>(chain: &mut Chain, out: T) -> Result<T> {
    export_with_horizon(chain, out, HORIZON_EPOCHS)
}

/// Export accounts, main blocks, blocks holding a balance, every block not yet
/// processed and every block of the last `horizon_epochs` epochs.
pub fn export_with_horizon<T: Write>(chain: &mut Chain, out: T, horizon_epochs: u64) -> Result<T> {
    chain.commit(true)?;
    let db = chain.db().clone();
    let meta = chain.meta().clone();
    let ep = chain.params().epochs();
    let top_main_time = match chain.main_at(meta.nmain)? {
        Some(m) => chain.info(&m)?.map(|i| i.time).unwrap_or(0),
        None => 0,
    };
    let horizon_time = ep.start_of_epoch(ep.epoch(top_main_time).saturating_sub(horizon_epochs)).max(meta.snapshot_time);
    // an unsaved in-memory candidate cannot be exported: fall back to the
    // last main block (the importing node re-selects its top from there)
    let (top, top_diff) = match meta.top {
        Some(t) if db.contains(Table::BlockInfo, &t.0)? => (t, meta.top_diff),
        _ => match chain.main_at(meta.nmain)? {
            Some(m) => (m, chain.info(&m)?.map(|i| i.difficulty).unwrap_or_default()),
            None => (HashLow::ZERO, U256::ZERO),
        },
    };
    let header =
        SnapshotHeader { network: chain.params().network.id(), nmain: meta.nmain, horizon_time, top, top_diff, rx: chain.rx_schedule().clone() };
    let mut w = SnapshotWriter::new(out, &header)?;
    let mut accts = vec![];
    db.for_each(Table::Account, |k, v| {
        if let (Some(a), Ok(r)) = (Address::from_slice(k), AccountRecord::decode(v)) {
            accts.push((a, r));
        }
        true
    })?;
    w.accounts(&accts)?;
    drop(accts);

    let mut infos = vec![];
    db.for_each(Table::BlockInfo, |k, v| {
        if let (Some(h), Ok(i)) = (HashLow::from_slice(k), BlockInfo::decode(v)) {
            infos.push((h, i));
        }
        true
    })?;
    let mut blocks = vec![];
    for (h, info) in infos {
        let s = chain.state(&h)?;
        let keeps_value = s.amount != 0 || s.flags & flags::MAIN != 0;
        let data = match &info.snapshot {
            Some(k) if keeps_value => SnapshotData::Key(k.clone()),
            Some(_) => continue,
            None => {
                let Some(b) = chain.block(&h)? else { continue };
                if info.time >= horizon_time || s.flags & flags::MAIN_REF == 0 {
                    let payload = if info.ext_root.is_some() { db.get(Table::Payload, &h.0)? } else { None };
                    SnapshotData::Full { raw: Box::new(*b.raw()), payload }
                } else if keeps_value {
                    SnapshotData::Key(SnapshotKey::RawBlock(Box::new(*b.raw())))
                } else {
                    continue;
                }
            }
        };
        blocks.push(SnapshotBlock {
            hashlow: h,
            hash: info.hash,
            time: info.time,
            flags: info.flags | s.flags,
            height: s.height,
            difficulty: info.difficulty,
            max_diff_link: info.max_diff_link,
            amount: s.amount,
            fee: s.fee,
            remark: info.remark,
            data,
            ref_: s.ref_,
        });
    }
    w.blocks(&blocks)?;
    drop(blocks);
    let mut idx = vec![];
    for h in 1..=meta.nmain {
        if let Some(hl) = chain.main_at(h)? {
            idx.push((h, hl));
        }
    }
    w.main_index(&idx)
}
