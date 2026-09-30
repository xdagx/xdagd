//! Persisted records of the chain and their explicit encodings.
//!
//! Every record starts with a version byte so that a future release can read
//! old data (or migrate it in place) instead of wiping the database.

use xdag_storage::{Reader, Writer};
use xdag_types::{Address, BlockHash, Difficulty, HashLow, Nano, U256};

use crate::ChainError;

/// DAG flags (xdagj `BI_*` values). The apply-related ones are stored in
/// [`BlockState`]; the DAG ones in [`BlockInfo`]. `combined_flags` rebuilds the
/// xdagj bitfield for RPC output.
pub mod flags {
    pub const MAIN: u8 = 0x01;
    pub const MAIN_CHAIN: u8 = 0x02;
    pub const APPLIED: u8 = 0x04;
    pub const MAIN_REF: u8 = 0x08;
    pub const REF: u8 = 0x10;
    pub const OURS: u8 = 0x20;
    pub const EXTRA: u8 = 0x40;
    pub const REMARK: u8 = 0x80;

    /// Flags owned by the execution state (journaled).
    pub const APPLY_MASK: u8 = MAIN | APPLIED | MAIN_REF;
}

fn corrupt(table: &'static str) -> impl Fn(xdag_storage::codec::DecodeError) -> ChainError {
    move |e| ChainError::Corrupt(format!("{table}: {e}"))
}

/// Pre-snapshot information that lets a block balance be spent after an
/// xdagj-style snapshot bootstrap (the raw block itself is not available).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotKey {
    /// 33-byte compressed public key that may spend the block.
    PublicKey([u8; 33]),
    /// The original raw block, used to verify its output signature.
    RawBlock(Box<[u8; 512]>),
}

/// Immutable-ish DAG metadata of a stored block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub hash: BlockHash,
    pub time: u64,
    pub type_word: u64,
    /// DAG flags: MAIN_CHAIN, REF, OURS, EXTRA, REMARK.
    pub flags: u8,
    /// Cumulative difficulty.
    pub difficulty: Difficulty,
    pub max_diff_link: Option<HashLow>,
    pub remark: Option<[u8; 32]>,
    /// Block came from a snapshot (no raw block, no links).
    pub snapshot: Option<SnapshotKey>,
    /// Nova payload root, if the block carries one.
    pub ext_root: Option<[u8; 32]>,
    /// Number of transactions in the payload (0 if none).
    pub payload_txs: u32,
}

impl BlockInfo {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION)
            .fixed(&self.hash.0)
            .u64(self.time)
            .u64(self.type_word)
            .u8(self.flags)
            .fixed(&self.difficulty.to_be_bytes::<32>())
            .opt_fixed(self.max_diff_link.as_ref().map(|h| &h.0[..]))
            .opt_fixed(self.remark.as_ref().map(|r| &r[..]))
            .opt_fixed(self.ext_root.as_ref().map(|r| &r[..]))
            .u32(self.payload_txs);
        match &self.snapshot {
            None => {
                w.u8(0);
            }
            Some(SnapshotKey::PublicKey(k)) => {
                w.u8(1).fixed(k);
            }
            Some(SnapshotKey::RawBlock(b)) => {
                w.u8(2).fixed(&b[..]);
            }
        }
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("block_info");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("block_info version {v}")));
        }
        let hash = BlockHash(r.fixed::<32>().map_err(&e)?);
        let time = r.u64().map_err(&e)?;
        let type_word = r.u64().map_err(&e)?;
        let flags = r.u8().map_err(&e)?;
        let difficulty = U256::from_be_bytes(r.fixed::<32>().map_err(&e)?);
        let max_diff_link = r.opt_fixed::<24>().map_err(&e)?.map(HashLow);
        let remark = r.opt_fixed::<32>().map_err(&e)?;
        let ext_root = r.opt_fixed::<32>().map_err(&e)?;
        let payload_txs = r.u32().map_err(&e)?;
        let snapshot = match r.u8().map_err(&e)? {
            0 => None,
            1 => Some(SnapshotKey::PublicKey(r.fixed::<33>().map_err(&e)?)),
            2 => Some(SnapshotKey::RawBlock(Box::new(r.fixed::<512>().map_err(&e)?))),
            x => return Err(ChainError::Corrupt(format!("block_info snapshot tag {x}"))),
        };
        Ok(BlockInfo { hash, time, type_word, flags, difficulty, max_diff_link, remark, snapshot, ext_root, payload_txs })
    }
}

/// Execution state of a block (journaled).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockState {
    /// MAIN / MAIN_REF / APPLIED.
    pub flags: u8,
    /// Block balance in nano (legacy block-balance model: main-block rewards).
    /// Signed like xdagj's `XAmount`: duplicate IN links could drive it
    /// negative under legacy rules.
    pub amount: i64,
    /// Fee collected (main blocks: total; tx blocks: their own fee).
    pub fee: Nano,
    /// Main block whose application processed this block.
    pub ref_: Option<HashLow>,
    /// Main-chain height (0 if not a main block).
    pub height: u64,
    /// Execution result of the block's own transaction (see [`TxStatus`]).
    pub status: u8,
}

impl BlockState {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION)
            .u8(self.flags)
            .i64(self.amount)
            .u64(self.fee.0)
            .opt_fixed(self.ref_.as_ref().map(|h| &h.0[..]))
            .u64(self.height)
            .u8(self.status);
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("block_state");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("block_state version {v}")));
        }
        Ok(BlockState {
            flags: r.u8().map_err(&e)?,
            amount: r.i64().map_err(&e)?,
            fee: Nano(r.u64().map_err(&e)?),
            ref_: r.opt_fixed::<24>().map_err(&e)?.map(HashLow),
            height: r.u64().map_err(&e)?,
            status: r.u8().map_err(&e)?,
        })
    }
}

/// Transaction execution outcome recorded in [`BlockState::status`] and the
/// history index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TxStatus {
    Pending = 0,
    Applied = 1,
    /// Processed but rejected (xdagj flags REF|MAIN_REF without APPLIED).
    Rejected = 2,
    /// Nova: executed, fee charged, value transfer failed / EVM reverted.
    Failed = 3,
}

impl TxStatus {
    pub fn from_u8(v: u8) -> TxStatus {
        match v {
            1 => TxStatus::Applied,
            2 => TxStatus::Rejected,
            3 => TxStatus::Failed,
            _ => TxStatus::Pending,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            TxStatus::Pending => "pending",
            TxStatus::Applied => "applied",
            TxStatus::Rejected => "rejected",
            TxStatus::Failed => "failed",
        }
    }
}

/// Balance representation of an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Balance {
    /// xdagj representation: C-units, converted through the lossy legacy path
    /// on every read/write.
    Legacy(u64),
    /// Nova representation: exact, 18 decimals.
    Wei(u128),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRecord {
    pub balance: Balance,
    /// Number of executed transactions (xdagj "executed nonce").
    pub nonce: u64,
    /// Highest nonce accepted into the pool/RPC (xdagj "tx quantity"); advisory.
    pub pending_nonce: u64,
    /// keccak256 of the EVM code (None for plain accounts).
    pub code_hash: Option<[u8; 32]>,
}

impl Default for AccountRecord {
    fn default() -> Self {
        AccountRecord { balance: Balance::Legacy(0), nonce: 0, pending_nonce: 0, code_hash: None }
    }
}

impl AccountRecord {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION);
        match self.balance {
            Balance::Legacy(c) => {
                w.u8(0).u64(c);
            }
            Balance::Wei(v) => {
                w.u8(1).u128(v);
            }
        }
        w.u64(self.nonce).u64(self.pending_nonce).opt_fixed(self.code_hash.as_ref().map(|h| &h[..]));
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("account");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("account version {v}")));
        }
        let balance = match r.u8().map_err(&e)? {
            0 => Balance::Legacy(r.u64().map_err(&e)?),
            1 => Balance::Wei(r.u128().map_err(&e)?),
            x => return Err(ChainError::Corrupt(format!("account balance tag {x}"))),
        };
        Ok(AccountRecord { balance, nonce: r.u64().map_err(&e)?, pending_nonce: r.u64().map_err(&e)?, code_hash: r.opt_fixed::<32>().map_err(&e)? })
    }

    /// Balance in nano as xdagj would read it (legacy: lossy conversion).
    pub fn balance_nano_legacy(&self) -> Result<Nano, ChainError> {
        match self.balance {
            Balance::Legacy(c) => xdag_types::CAmount(c).to_nano_legacy().map_err(|_| ChainError::Overflow),
            Balance::Wei(w) => Ok(Nano::from_wei_floor(w)),
        }
    }

    /// Exact balance in wei (Nova view). Legacy records convert through the
    /// xdagj read path, which is the value every xdagj node agrees on.
    pub fn balance_wei(&self) -> Result<u128, ChainError> {
        match self.balance {
            Balance::Legacy(_) => Ok(self.balance_nano_legacy()?.to_wei()),
            Balance::Wei(w) => Ok(w),
        }
    }
}

/// Chain-wide counters and the current top, persisted under `Meta`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainMeta {
    /// Number of main blocks (xdagj `nmain`).
    pub nmain: u64,
    pub top: Option<HashLow>,
    pub top_diff: Difficulty,
    /// Total stored blocks.
    pub nblocks: u64,
    /// Height of the snapshot the chain was bootstrapped from (0 = genesis sync).
    /// Main blocks up to it are final: their undo journals do not exist.
    pub snapshot_height: u64,
}

impl ChainMeta {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION)
            .u64(self.nmain)
            .opt_fixed(self.top.as_ref().map(|h| &h.0[..]))
            .fixed(&self.top_diff.to_be_bytes::<32>())
            .u64(self.nblocks)
            .u64(self.snapshot_height);
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("meta");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("meta version {v}")));
        }
        Ok(ChainMeta {
            nmain: r.u64().map_err(&e)?,
            top: r.opt_fixed::<24>().map_err(&e)?.map(HashLow),
            top_diff: U256::from_be_bytes(r.fixed::<32>().map_err(&e)?),
            nblocks: r.u64().map_err(&e)?,
            snapshot_height: r.u64().map_err(&e)?,
        })
    }
}

/// Direction of a history entry, matching xdagj's RPC `direction` codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// Funds left the account/block.
    Input = 0,
    /// Funds arrived.
    Output = 1,
    /// Mining reward / collected fees.
    Earning = 2,
    /// Fee paid (Nova failed transactions).
    Fee = 3,
}

/// One history entry for an address or block. Written when a transaction is
/// *executed* (xdagj wrote history when a block was merely received, so rejected
/// transactions showed up and reorgs were never reflected).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Transaction id: hashlow of the tx block, or the 32-byte hash of a Nova tx.
    pub tx: Vec<u8>,
    pub direction: Direction,
    /// Amount credited/debited (nano).
    pub amount: Nano,
    /// Counterparty (address or block), for display.
    pub counterparty: Option<Vec<u8>>,
    pub time: u64,
    pub main_height: u64,
    pub status: TxStatus,
    pub remark: Vec<u8>,
}

impl HistoryEntry {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION)
            .bytes(&self.tx)
            .u8(self.direction as u8)
            .u64(self.amount.0)
            .u8(self.counterparty.is_some() as u8)
            .bytes(self.counterparty.as_deref().unwrap_or(&[]))
            .u64(self.time)
            .u64(self.main_height)
            .u8(self.status as u8)
            .bytes(&self.remark);
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("history");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("history version {v}")));
        }
        let tx = r.bytes().map_err(&e)?;
        let direction = match r.u8().map_err(&e)? {
            0 => Direction::Input,
            1 => Direction::Output,
            2 => Direction::Earning,
            _ => Direction::Fee,
        };
        let amount = Nano(r.u64().map_err(&e)?);
        let has_cp = r.u8().map_err(&e)? != 0;
        let cp = r.bytes().map_err(&e)?;
        Ok(HistoryEntry {
            tx,
            direction,
            amount,
            counterparty: if has_cp { Some(cp) } else { None },
            time: r.u64().map_err(&e)?,
            main_height: r.u64().map_err(&e)?,
            status: TxStatus::from_u8(r.u8().map_err(&e)?),
            remark: r.bytes().map_err(&e)?,
        })
    }
}

/// Where a Nova transaction was executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxLocation {
    pub block: HashLow,
    pub index: u32,
    pub main_height: u64,
    pub status: TxStatus,
    pub sender: Address,
    pub fee: Nano,
    pub gas_used: u64,
}

impl TxLocation {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(Self::VERSION)
            .fixed(&self.block.0)
            .u32(self.index)
            .u64(self.main_height)
            .u8(self.status as u8)
            .fixed(&self.sender.0)
            .u64(self.fee.0)
            .u64(self.gas_used);
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ChainError> {
        let e = corrupt("tx_index");
        let mut r = Reader::new(b);
        let v = r.u8().map_err(&e)?;
        if v != Self::VERSION {
            return Err(ChainError::Corrupt(format!("tx_index version {v}")));
        }
        Ok(TxLocation {
            block: HashLow(r.fixed::<24>().map_err(&e)?),
            index: r.u32().map_err(&e)?,
            main_height: r.u64().map_err(&e)?,
            status: TxStatus::from_u8(r.u8().map_err(&e)?),
            sender: Address(r.fixed::<20>().map_err(&e)?),
            fee: Nano(r.u64().map_err(&e)?),
            gas_used: r.u64().map_err(&e)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let bi = BlockInfo {
            hash: BlockHash([3u8; 32]),
            time: 99,
            type_word: 0x51,
            flags: flags::REF | flags::MAIN_CHAIN,
            difficulty: U256::from(12345u64),
            max_diff_link: Some(HashLow([1u8; 24])),
            remark: None,
            snapshot: Some(SnapshotKey::PublicKey([2u8; 33])),
            ext_root: Some([5u8; 32]),
            payload_txs: 3,
        };
        assert_eq!(BlockInfo::decode(&bi.encode()).unwrap(), bi);
        let st = BlockState { flags: 9, amount: -5, fee: Nano(1), ref_: None, height: 7, status: 1 };
        assert_eq!(BlockState::decode(&st.encode()).unwrap(), st);
        let a = AccountRecord { balance: Balance::Wei(10), nonce: 3, pending_nonce: 4, code_hash: Some([9u8; 32]) };
        assert_eq!(AccountRecord::decode(&a.encode()).unwrap(), a);
        let m = ChainMeta { nmain: 5, top: Some(HashLow([7u8; 24])), top_diff: U256::from(77u8), nblocks: 9, snapshot_height: 4 };
        assert_eq!(ChainMeta::decode(&m.encode()).unwrap(), m);
        let h = HistoryEntry {
            tx: vec![1; 24],
            direction: Direction::Output,
            amount: Nano(3),
            counterparty: Some(vec![2; 20]),
            time: 1,
            main_height: 2,
            status: TxStatus::Applied,
            remark: b"x".to_vec(),
        };
        assert_eq!(HistoryEntry::decode(&h.encode()).unwrap(), h);
    }
}
