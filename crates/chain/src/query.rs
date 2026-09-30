//! Read-side queries used by RPC, the pool and the CLI.
//!
//! Reads go straight to the database (committed state), so they never block
//! block import for long.

use xdag_storage::{Db, Reader, Table, Writer};
use xdag_types::{Address, Block, HashLow, Nano};

use crate::apply::{subject_addr, subject_block};
use crate::evm_api::{EvmExecResult, EvmLog};
use crate::keys;
use crate::records::{AccountRecord, BlockInfo, BlockState, ChainMeta, HistoryEntry, TxLocation};
use crate::{ChainError, Result};

/// Full view of a stored block.
#[derive(Clone, Debug)]
pub struct BlockView {
    pub hashlow: HashLow,
    pub info: BlockInfo,
    pub state: BlockState,
    pub block: Option<Block>,
    pub payload: Option<Vec<u8>>,
}

impl BlockView {
    /// xdagj-style combined flags.
    pub fn flags(&self) -> u8 {
        (self.info.flags & !crate::records::flags::APPLY_MASK) | (self.state.flags & crate::records::flags::APPLY_MASK)
    }
}

/// The persisted RandomX fork state (fork epoch and seeds).
pub fn rx_schedule(db: &Db) -> Result<crate::pow::RxSchedule> {
    match db.get(Table::Meta, keys::META_RX)? {
        Some(b) => crate::pow::RxSchedule::decode(&b).ok_or_else(|| ChainError::Corrupt("randomx schedule".into())),
        None => Ok(Default::default()),
    }
}

pub fn chain_meta(db: &Db) -> Result<ChainMeta> {
    Ok(match db.get(Table::Meta, keys::META_CHAIN)? {
        Some(b) => ChainMeta::decode(&b)?,
        None => ChainMeta::default(),
    })
}

pub fn block_view(db: &Db, h: &HashLow) -> Result<Option<BlockView>> {
    let Some(ib) = db.get(Table::BlockInfo, &h.0)? else { return Ok(None) };
    let info = BlockInfo::decode(&ib)?;
    let state = match db.get(Table::BlockState, &h.0)? {
        Some(b) => BlockState::decode(&b)?,
        None => BlockState::default(),
    };
    let block = match db.get(Table::BlockRaw, &h.0)? {
        Some(raw) => Some(Block::parse(&raw).map_err(|e| ChainError::Corrupt(e.to_string()))?),
        None => None,
    };
    let payload = db.get(Table::Payload, &h.0)?;
    Ok(Some(BlockView { hashlow: *h, info, state, block, payload }))
}

pub fn main_hashlow(db: &Db, height: u64) -> Result<Option<HashLow>> {
    let meta = chain_meta(db)?;
    if height == 0 || height > meta.nmain {
        return Ok(None);
    }
    Ok(db.get(Table::MainHeight, &keys::height(height))?.and_then(|v| HashLow::from_slice(&v)))
}

pub fn account(db: &Db, a: &Address) -> Result<Option<AccountRecord>> {
    Ok(match db.get(Table::Account, &a.0)? {
        Some(b) => Some(AccountRecord::decode(&b)?),
        None => None,
    })
}

/// Balance in nano as users see it (legacy records through the xdagj read
/// path; Nova records exact, sub-nano dust truncated).
pub fn balance(db: &Db, a: &Address) -> Result<Nano> {
    match account(db, a)? {
        Some(r) => r.balance_nano_legacy(),
        None => Ok(Nano::ZERO),
    }
}

pub fn balance_wei(db: &Db, a: &Address) -> Result<u128> {
    match account(db, a)? {
        Some(r) => r.balance_wei(),
        None => Ok(0),
    }
}

/// Newest-first history page of an address.
pub fn address_history(db: &Db, a: &Address, before_height: Option<u64>, limit: usize) -> Result<Vec<HistoryEntry>> {
    history(db, &subject_addr(a), before_height, limit)
}

pub fn block_history(db: &Db, h: &HashLow, before_height: Option<u64>, limit: usize) -> Result<Vec<HistoryEntry>> {
    history(db, &subject_block(h), before_height, limit)
}

fn history(db: &Db, subject: &[u8], before_height: Option<u64>, limit: usize) -> Result<Vec<HistoryEntry>> {
    let before = before_height.map(|height| HistoryCursor { height, seq: 0 });
    Ok(history_page(db, subject, before, limit)?.into_iter().map(|(_, e)| e).collect())
}

/// Position of a history entry (main-block height, sequence within that
/// main block's execution); pages continue strictly before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    pub height: u64,
    pub seq: u32,
}

impl std::fmt::Display for HistoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.height, self.seq)
    }
}

impl std::str::FromStr for HistoryCursor {
    type Err = ();
    /// `height.seq`, or a bare height (everything before that main block).
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        let (h, q) = s.split_once('.').unwrap_or((s, "0"));
        Ok(HistoryCursor { height: h.parse().map_err(|_| ())?, seq: q.parse().map_err(|_| ())? })
    }
}

/// Newest-first history of an address, with the cursor of each entry.
pub fn address_history_page(db: &Db, a: &Address, before: Option<HistoryCursor>, limit: usize) -> Result<Vec<(HistoryCursor, HistoryEntry)>> {
    history_page(db, &subject_addr(a), before, limit)
}

/// Newest-first history of a block, with the cursor of each entry.
pub fn block_history_page(db: &Db, h: &HashLow, before: Option<HistoryCursor>, limit: usize) -> Result<Vec<(HistoryCursor, HistoryEntry)>> {
    history_page(db, &subject_block(h), before, limit)
}

fn history_page(db: &Db, subject: &[u8], before: Option<HistoryCursor>, limit: usize) -> Result<Vec<(HistoryCursor, HistoryEntry)>> {
    let start = subject.to_vec();
    let end = match before {
        Some(c) => keys::history(subject, c.height, c.seq),
        None => xdag_storage::prefix_end(subject).unwrap_or_else(|| vec![0xff; subject.len() + 12]),
    };
    let rows = db.scan_range(Table::History, &start, Some(&end), limit, true)?;
    rows.iter()
        .map(|(k, v)| {
            let n = subject.len();
            let bad = || ChainError::Corrupt("history key".into());
            let height = u64::from_be_bytes(k.get(n..n + 8).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
            let seq = u32::from_be_bytes(k.get(n + 8..n + 12).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
            Ok((HistoryCursor { height, seq }, HistoryEntry::decode(v)?))
        })
        .collect()
}

pub fn tx_location(db: &Db, tx: &[u8]) -> Result<Option<TxLocation>> {
    Ok(match db.get(Table::TxIndex, tx)? {
        Some(b) => Some(TxLocation::decode(&b)?),
        None => None,
    })
}

pub fn main_blocks(db: &Db, count: usize) -> Result<Vec<(u64, HashLow)>> {
    let meta = chain_meta(db)?;
    let mut out = vec![];
    let mut h = meta.nmain;
    while h > 0 && out.len() < count {
        if let Some(hl) = main_hashlow(db, h)? {
            out.push((h, hl));
        }
        h -= 1;
    }
    Ok(out)
}

pub fn blocks_in_epochs(db: &Db, start_epoch: u64, end_epoch: u64, limit: usize) -> Result<Vec<HashLow>> {
    let rows = db.scan_range(Table::TimeIndex, &start_epoch.to_be_bytes(), Some(&end_epoch.to_be_bytes()), limit, false)?;
    Ok(rows.iter().filter_map(|(k, _)| HashLow::from_slice(&k[8..])).collect())
}

pub fn noref_blocks(db: &Db, limit: usize) -> Result<Vec<HashLow>> {
    Ok(db.scan_prefix(Table::NoRef, &[], limit)?.iter().filter_map(|(k, _)| HashLow::from_slice(k)).collect())
}

/// EVM transactions executed by the main block at `height`, in order.
pub fn evm_txs_at(db: &Db, height: u64) -> Result<Vec<[u8; 32]>> {
    let rows = db.scan_prefix(Table::EvmTxs, &height.to_be_bytes(), 100_000)?;
    Ok(rows.iter().filter_map(|(_, v)| v.as_slice().try_into().ok()).collect())
}

pub fn code(db: &Db, hash: &[u8; 32]) -> Result<Vec<u8>> {
    Ok(db.get(Table::Code, hash)?.unwrap_or_default())
}

pub fn storage_at(db: &Db, a: &Address, slot: &[u8; 32]) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    if let Some(v) = db.get(Table::Storage, &keys::storage_slot(a, slot))? {
        out.copy_from_slice(&v[..32]);
    }
    Ok(out)
}

/// Stored EVM receipt.
#[derive(Clone, Debug)]
pub struct Receipt {
    pub success: bool,
    pub gas_used: u64,
    pub fee: u128,
    pub height: u64,
    pub contract_address: Option<Address>,
    pub logs: Vec<EvmLog>,
    pub output: Vec<u8>,
}

pub fn encode_receipt(r: &EvmExecResult, height: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(1)
        .bool(r.success)
        .u64(r.gas_used)
        .u128(r.fee)
        .u64(height)
        .opt_fixed(r.contract_address.as_ref().map(|a| &a.0[..]))
        .bytes(&r.output)
        .u32(r.logs.len() as u32);
    for l in &r.logs {
        w.fixed(&l.address.0).u32(l.topics.len() as u32);
        for t in &l.topics {
            w.fixed(t);
        }
        w.bytes(&l.data);
    }
    w.finish()
}

pub fn receipt(db: &Db, tx: &[u8; 32]) -> Result<Option<Receipt>> {
    let Some(b) = db.get(Table::Receipt, tx)? else { return Ok(None) };
    let e = |_| ChainError::Corrupt("receipt".into());
    let mut r = Reader::new(&b);
    if r.u8().map_err(e)? != 1 {
        return Err(ChainError::Corrupt("receipt version".into()));
    }
    let success = r.bool().map_err(e)?;
    let gas_used = r.u64().map_err(e)?;
    let fee = r.u128().map_err(e)?;
    let height = r.u64().map_err(e)?;
    let contract_address = r.opt_fixed::<20>().map_err(e)?.map(Address);
    let output = r.bytes().map_err(e)?;
    let n = r.u32().map_err(e)? as usize;
    let mut logs = Vec::with_capacity(n);
    for _ in 0..n {
        let address = Address(r.fixed::<20>().map_err(e)?);
        let nt = r.u32().map_err(e)? as usize;
        let mut topics = Vec::with_capacity(nt);
        for _ in 0..nt {
            topics.push(r.fixed::<32>().map_err(e)?);
        }
        let data = r.bytes().map_err(e)?;
        logs.push(EvmLog { address, topics, data });
    }
    Ok(Some(Receipt { success, gas_used, fee, height, contract_address, logs, output }))
}

/// Read-only EVM state over committed data (eth_call, eth_estimateGas).
pub struct DbState<'a>(pub &'a Db);

impl crate::evm_api::EvmStateAccess for DbState<'_> {
    fn account(&mut self, a: &Address) -> std::result::Result<Option<crate::evm_api::EvmAccount>, String> {
        match account(self.0, a).map_err(|e| e.to_string())? {
            None => Ok(None),
            Some(r) => {
                Ok(Some(crate::evm_api::EvmAccount { balance: r.balance_wei().map_err(|e| e.to_string())?, nonce: r.nonce, code_hash: r.code_hash }))
            }
        }
    }
    fn code(&mut self, h: &[u8; 32]) -> std::result::Result<Vec<u8>, String> {
        code(self.0, h).map_err(|e| e.to_string())
    }
    fn storage(&mut self, a: &Address, slot: &[u8; 32]) -> std::result::Result<[u8; 32], String> {
        storage_at(self.0, a, slot).map_err(|e| e.to_string())
    }
    fn block_hash(&mut self, height: u64) -> std::result::Result<[u8; 32], String> {
        let Some(h) = main_hashlow(self.0, height).map_err(|e| e.to_string())? else { return Ok([0u8; 32]) };
        Ok(block_view(self.0, &h).map_err(|e| e.to_string())?.map(|v| v.info.hash.0).unwrap_or([0u8; 32]))
    }
}

/// Raw bytes of the `index`-th transaction of a batch block's payload.
pub fn payload_tx(db: &Db, block: &HashLow, index: u32) -> Result<Option<xdag_types::nova::NovaTx>> {
    let Some(p) = db.get(Table::Payload, &block.0)? else { return Ok(None) };
    let txs = xdag_types::nova::decode_payload(&p, usize::MAX).map_err(|e| ChainError::Corrupt(e.to_string()))?;
    Ok(txs.into_iter().nth(index as usize))
}
