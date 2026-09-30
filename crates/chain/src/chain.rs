//! Block import and main-chain selection (port of xdagj `BlockchainImpl`).

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use lru::LruCache;
use xdag_storage::{Db, Table};
use xdag_types::crypto::pubkey_address;
use xdag_types::{Address, Block, Difficulty, FieldType, HashLow, LinkTarget, NetworkParams, PublicKey, U256};

use crate::evm_api::EvmEngine;
use crate::fees;
use crate::keys;
use crate::overlay::Overlay;
use crate::pow::{self, PowEngine, RxSchedule};
use crate::preverify::{PreVerified, VerifiedPayload};
use crate::records::{flags, AccountRecord, BlockInfo, BlockState, ChainMeta, SnapshotKey};
use crate::{ChainError, Result};

pub trait Clock: Send + Sync {
    fn now_xdag(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_xdag(&self) -> u64 {
        xdag_types::time::now_xdag()
    }
}

/// Deterministic clock for tests and simulations.
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(t: u64) -> Arc<Self> {
        Arc::new(ManualClock(AtomicU64::new(t)))
    }
    pub fn set(&self, t: u64) {
        self.0.store(t, Ordering::SeqCst)
    }
    pub fn advance(&self, d: u64) {
        self.0.fetch_add(d, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_xdag(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Freshly broadcast block (NEW_BLOCK).
    Gossip,
    /// Block fetched during synchronisation or as a missing parent.
    Sync,
    /// Produced by this node.
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    ImportedBest,
    ImportedNotBest,
    Exist,
    /// Already held as an unsaved main-block candidate.
    InMem,
    NoParent(HashLow),
    /// Not valid *yet*: a legacy rule depends on state (the INPUT address must
    /// exist). xdagj drops such a block and re-requests it later when another
    /// block references it; callers should retry after new main blocks.
    Deferred(String),
    Invalid(String),
    Error(String),
}

impl ImportOutcome {
    pub fn is_imported(&self) -> bool {
        matches!(self, ImportOutcome::ImportedBest | ImportOutcome::ImportedNotBest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainEvent {
    NewTop {
        top: HashLow,
        diff: Difficulty,
    },
    MainSet {
        height: u64,
        block: HashLow,
    },
    MainUnset {
        height: u64,
        block: HashLow,
    },
    Stored {
        block: HashLow,
    },
    /// A transaction (tx block hashlow or Nova tx hash) left the pending state.
    TxDone {
        tx: Vec<u8>,
        sender: Option<Address>,
        nonce: u64,
    },
}

#[derive(Clone, Debug)]
pub struct ChainOptions {
    /// xdagj MAX_ALLOWED_EXTRA.
    pub max_extra: usize,
    /// xdagj MAX_ORPHAN_SIZE (policy limit on unreferenced account txs from gossip).
    pub max_orphan_account_txs: usize,
    /// Keys of this node (blocks signed by them get the OURS flag).
    pub our_keys: Vec<PublicKey>,
    /// Reject blocks from the future (disabled in deterministic tests).
    pub check_future: bool,
    pub cache_blocks: usize,
    /// Record per-address history (on by default; this is what xdagj lost on
    /// every upgrade).
    pub index_history: bool,
}

impl Default for ChainOptions {
    fn default() -> Self {
        ChainOptions {
            max_extra: 65536,
            max_orphan_account_txs: 3750,
            our_keys: vec![],
            check_future: true,
            cache_blocks: 65536,
            index_history: true,
        }
    }
}

pub(crate) struct ExtraEntry {
    pub block: Arc<Block>,
    pub info: BlockInfo,
    pub payload: Option<VerifiedPayload>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RemoveAction {
    Normal,
    Extra,
    Reuse,
}

pub struct Chain {
    pub(crate) params: Arc<NetworkParams>,
    pub(crate) opts: ChainOptions,
    pub(crate) ov: Overlay,
    pub(crate) meta: ChainMeta,
    pub(crate) rx: RxSchedule,
    pub(crate) pow: Arc<dyn PowEngine>,
    pub(crate) evm: Option<Arc<dyn EvmEngine>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) extra: HashMap<HashLow, ExtraEntry>,
    extra_order: VecDeque<HashLow>,
    pending: Option<(HashLow, BlockInfo)>,
    info_cache: LruCache<HashLow, BlockInfo>,
    block_cache: LruCache<HashLow, Arc<Block>>,
    pub(crate) payload_cache: LruCache<HashLow, VerifiedPayload>,
    /// First block with an earlier epoch on a block's max-diff-link chain
    /// (memoised: turns the same-epoch walk of `calculateBlockDiff` into O(1)).
    epoch_exit: LruCache<HashLow, Option<(u64, Difficulty)>>,
    pub(crate) events: Vec<ChainEvent>,
    pub(crate) history_seq: u32,
    /// Unreferenced account transactions (for the orphan-pool policy limit).
    noref_account_txs: usize,
    meta_dirty: bool,
}

impl Chain {
    pub fn open(
        db: Db,
        params: NetworkParams,
        opts: ChainOptions,
        pow: Arc<dyn PowEngine>,
        evm: Option<Arc<dyn EvmEngine>>,
        clock: Arc<dyn Clock>,
    ) -> Result<Chain> {
        if db.get(Table::Meta, keys::META_SNAPSHOT_IMPORT)?.is_some() {
            return Err(ChainError::Invalid(
                "a snapshot import into this database did not finish: delete the database file and repeat the import".into(),
            ));
        }
        let meta = match db.get(Table::Meta, keys::META_CHAIN)? {
            Some(b) => ChainMeta::decode(&b)?,
            None => ChainMeta::default(),
        };
        let rx = match db.get(Table::Meta, keys::META_RX)? {
            Some(b) => RxSchedule::decode(&b).ok_or_else(|| ChainError::Corrupt("randomx schedule".into()))?,
            None => RxSchedule::default(),
        };
        let cache = NonZeroUsize::new(opts.cache_blocks.max(16)).unwrap();
        let noref_account_txs = 0;
        let mut c = Chain {
            params: Arc::new(params),
            opts,
            ov: Overlay::new(db),
            meta,
            rx,
            pow,
            evm,
            clock,
            extra: HashMap::new(),
            extra_order: VecDeque::new(),
            pending: None,
            info_cache: LruCache::new(cache),
            block_cache: LruCache::new(cache),
            payload_cache: LruCache::new(NonZeroUsize::new(1024).unwrap()),
            epoch_exit: LruCache::new(cache),
            events: vec![],
            history_seq: 0,
            noref_account_txs,
            meta_dirty: false,
        };
        c.noref_account_txs = c.count_noref_account_txs()?;
        c.recover_top()?;
        Ok(c)
    }

    /// Unsaved main-block candidates live only in memory (as in xdagj), so
    /// after a restart the persisted top may be gone. Like xdagj, fall back to
    /// the last main block — and also clear stale MAIN_CHAIN marks above it,
    /// which would otherwise make a later fork search stop at a block that is
    /// not on the current chain and unwind everything below it.
    fn recover_top(&mut self) -> Result<()> {
        let Some(top) = self.meta.top else { return Ok(()) };
        if self.info(&top)?.is_some() {
            return Ok(());
        }
        let ep = self.params.epochs();
        let (new_top, from_epoch) = match self.main_at(self.meta.nmain)? {
            Some(m) => {
                let i = self.info(&m)?.ok_or_else(|| ChainError::Corrupt("main block without info".into()))?;
                (Some((m, i.difficulty)), ep.epoch(i.time) + 1)
            }
            None => (None, 0),
        };
        let stale = crate::query::blocks_in_epochs(self.ov.db(), from_epoch, u64::MAX, usize::MAX)?;
        let mut cleared = 0;
        for h in stale {
            if let Some(mut i) = self.info(&h)? {
                if i.flags & flags::MAIN_CHAIN != 0 {
                    i.flags &= !flags::MAIN_CHAIN;
                    self.put_info(&h, i)?;
                    cleared += 1;
                }
            }
        }
        match new_top {
            Some((m, d)) => {
                self.meta.top = Some(m);
                self.meta.top_diff = d;
            }
            None => {
                self.meta.top = None;
                self.meta.top_diff = U256::ZERO;
            }
        }
        tracing::info!(top = ?self.meta.top, cleared, "recovered chain top after restart");
        self.mark_meta_dirty();
        self.commit(true)
    }

    /// An unsaved main-block candidate held in memory, with its payload.
    pub fn extra_block(&self, h: &HashLow) -> Option<(Arc<Block>, Option<Vec<u8>>)> {
        self.extra.get(h).map(|e| (e.block.clone(), e.payload.as_ref().map(|p| p.raw.to_vec())))
    }

    fn count_noref_account_txs(&mut self) -> Result<usize> {
        let norefs = self.ov.db().scan_prefix(Table::NoRef, &[], usize::MAX)?;
        let mut n = 0;
        for (k, _) in norefs {
            if let Some(h) = HashLow::from_slice(&k) {
                if let Some(b) = self.block(&h)? {
                    if b.is_account_tx() {
                        n += 1;
                    }
                }
            }
        }
        Ok(n)
    }

    pub fn params(&self) -> &Arc<NetworkParams> {
        &self.params
    }

    pub fn db(&self) -> &Db {
        self.ov.db()
    }

    pub fn meta(&self) -> &ChainMeta {
        &self.meta
    }

    pub fn nmain(&self) -> u64 {
        self.meta.nmain
    }

    pub fn top(&self) -> Option<HashLow> {
        self.meta.top
    }

    pub fn rx_schedule(&self) -> &RxSchedule {
        &self.rx
    }

    pub fn evm(&self) -> Option<&Arc<dyn EvmEngine>> {
        self.evm.as_ref()
    }

    pub fn now(&self) -> u64 {
        self.clock.now_xdag()
    }

    pub fn set_our_keys(&mut self, keys: Vec<PublicKey>) {
        self.opts.our_keys = keys;
    }

    pub fn take_events(&mut self) -> Vec<ChainEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn extra_count(&self) -> usize {
        self.extra.len()
    }

    // ------------------------------------------------------------------
    // record access
    // ------------------------------------------------------------------

    pub fn info(&mut self, h: &HashLow) -> Result<Option<BlockInfo>> {
        if let Some((ph, pi)) = &self.pending {
            if ph == h {
                return Ok(Some(pi.clone()));
            }
        }
        if let Some(e) = self.extra.get(h) {
            return Ok(Some(e.info.clone()));
        }
        if let Some(i) = self.info_cache.get(h) {
            return Ok(Some(i.clone()));
        }
        match self.ov.get(Table::BlockInfo, &h.0)? {
            Some(b) => {
                let i = BlockInfo::decode(&b)?;
                self.info_cache.put(*h, i.clone());
                Ok(Some(i))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn put_info(&mut self, h: &HashLow, info: BlockInfo) -> Result<()> {
        if let Some((ph, pi)) = &mut self.pending {
            if ph == h {
                *pi = info;
                return Ok(());
            }
        }
        if let Some(e) = self.extra.get_mut(h) {
            e.info = info;
            return Ok(());
        }
        self.ov.put(Table::BlockInfo, h.0.to_vec(), info.encode())?;
        self.info_cache.put(*h, info);
        Ok(())
    }

    pub fn state(&mut self, h: &HashLow) -> Result<BlockState> {
        match self.ov.get(Table::BlockState, &h.0)? {
            Some(b) => BlockState::decode(&b),
            None => Ok(BlockState::default()),
        }
    }

    pub(crate) fn put_state(&mut self, h: &HashLow, st: &BlockState) -> Result<()> {
        self.ov.put(Table::BlockState, h.0.to_vec(), st.encode())
    }

    /// Parsed raw block (None for unknown and snapshot-only blocks).
    pub fn block(&mut self, h: &HashLow) -> Result<Option<Arc<Block>>> {
        if let Some(e) = self.extra.get(h) {
            return Ok(Some(e.block.clone()));
        }
        if let Some(b) = self.block_cache.get(h) {
            return Ok(Some(b.clone()));
        }
        match self.ov.get(Table::BlockRaw, &h.0)? {
            Some(raw) => {
                let b = Arc::new(Block::parse(&raw).map_err(|e| ChainError::Corrupt(format!("stored block: {e}")))?);
                self.block_cache.put(*h, b.clone());
                Ok(Some(b))
            }
            None => Ok(None),
        }
    }

    pub fn is_stored(&mut self, h: &HashLow) -> Result<bool> {
        if self.info_cache.contains(h) {
            return Ok(true);
        }
        Ok(self.ov.get(Table::BlockInfo, &h.0)?.is_some())
    }

    pub fn has_block(&mut self, h: &HashLow) -> Result<bool> {
        Ok(self.extra.contains_key(h) || self.is_stored(h)?)
    }

    pub fn account(&mut self, a: &Address) -> Result<Option<AccountRecord>> {
        match self.ov.get(Table::Account, &a.0)? {
            Some(b) => Ok(Some(AccountRecord::decode(&b)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn put_account(&mut self, a: &Address, r: &AccountRecord) -> Result<()> {
        self.ov.put(Table::Account, a.0.to_vec(), r.encode())
    }

    pub fn main_at(&mut self, height: u64) -> Result<Option<HashLow>> {
        if height == 0 || height > self.meta.nmain {
            return Ok(None);
        }
        Ok(self.ov.get(Table::MainHeight, &keys::height(height))?.and_then(|v| HashLow::from_slice(&v)))
    }

    pub(crate) fn mark_meta_dirty(&mut self) {
        self.meta_dirty = true;
    }

    fn write_meta(&mut self) -> Result<()> {
        if self.meta_dirty {
            self.ov.put(Table::Meta, keys::META_CHAIN.to_vec(), self.meta.encode())?;
            self.ov.put(Table::Meta, keys::META_RX.to_vec(), self.rx.encode())?;
            self.meta_dirty = false;
        }
        Ok(())
    }

    /// Flush pending writes (also called after snapshot import and by tests).
    pub fn commit(&mut self, durable: bool) -> Result<()> {
        self.write_meta()?;
        self.ov.commit(durable)
    }

    fn reset_after_error(&mut self) {
        self.ov.discard();
        self.pending = None;
        self.info_cache.clear();
        self.block_cache.clear();
        self.epoch_exit.clear();
        if let Ok(Some(b)) = self.ov.db().get(Table::Meta, keys::META_CHAIN) {
            if let Ok(m) = ChainMeta::decode(&b) {
                self.meta = m;
            }
        }
        if let Ok(Some(b)) = self.ov.db().get(Table::Meta, keys::META_RX) {
            if let Some(r) = RxSchedule::decode(&b) {
                self.rx = r;
            }
        }
    }

    // ------------------------------------------------------------------
    // import
    // ------------------------------------------------------------------

    /// Import a pre-verified block (xdagj `tryToConnect`) and commit.
    pub fn import(&mut self, block: Arc<Block>, pre: PreVerified, src: Source) -> ImportOutcome {
        let out = self.import_uncommitted(block, pre, src);
        if out.is_imported() {
            if let Err(e) = self.commit(false) {
                tracing::error!("commit failed: {e}");
                self.reset_after_error();
                return ImportOutcome::Error(e.to_string());
            }
        }
        out
    }

    /// Import without committing, so a batch of imports shares one database
    /// transaction (call [`Chain::commit`] afterwards). Rejections never leave
    /// partial writes behind: every validation happens before any mutation.
    pub fn import_uncommitted(&mut self, block: Arc<Block>, pre: PreVerified, src: Source) -> ImportOutcome {
        match self.try_to_connect(&block, pre, src) {
            Ok(o) => o,
            Err(ChainError::Invalid(m)) => {
                self.pending = None;
                ImportOutcome::Invalid(m)
            }
            Err(e) => {
                tracing::error!("import of {:?} failed: {e}", block.hashlow());
                self.reset_after_error();
                ImportOutcome::Error(e.to_string())
            }
        }
    }

    fn try_to_connect(&mut self, block: &Arc<Block>, pre: PreVerified, src: Source) -> Result<ImportOutcome> {
        let p = self.params.clone();
        let ep = p.epochs();
        let hl = block.hashlow();
        let inv = |m: &str| Ok(ImportOutcome::Invalid(m.to_string()));

        // --- validation (no mutation) -----------------------------------
        if (block.type_word & 0xf) as u8 != p.header_type {
            return inv("block type error: not a block of this network");
        }
        let now = self.clock.now_xdag();
        if (self.opts.check_future && block.time > now + p.max_future_drift()) || block.time < p.era {
            return inv("block time is illegal");
        }
        if src == Source::Gossip && block.is_account_tx() && self.noref_account_txs >= self.opts.max_orphan_account_txs {
            return inv("orphan block pool is full");
        }
        if self.extra.contains_key(&hl) {
            return Ok(ImportOutcome::InMem);
        }
        if self.is_stored(&hl)? {
            return Ok(ImportOutcome::Exist);
        }
        let nova = p.is_nova_time(block.time);
        let mut is_extra = ep.is_end_of_epoch(block.time) && block.nonce.is_some();

        let tx_fee = match fees::tx_fee(block, &p) {
            Ok(f) => f,
            Err(_) => return inv("transaction fee overflow"),
        };
        if block.is_tx() && tx_fee.is_zero() {
            return inv("there is a problem with the transaction fee of this transaction block");
        }

        let mut input_count = 0;
        for link in block.links() {
            let amt = link.amount.to_nano_legacy().map_err(|_| ChainError::Invalid("amount overflow".into()))?;
            match link.target {
                LinkTarget::Block(r) => {
                    if link.kind == FieldType::Out && !amt.is_zero() {
                        return inv("address's amount isn't zero");
                    }
                    let Some(ri) = self.info(&r)? else {
                        return Ok(ImportOutcome::NoParent(r));
                    };
                    if ri.time >= block.time {
                        return inv("ref block's time >= block's time");
                    }
                    if link.kind == FieldType::In && amt < tx_fee {
                        return inv("ref block's balance < fee");
                    }
                }
                LinkTarget::Address(a) => {
                    if link.kind == FieldType::Input {
                        input_count += 1;
                        if input_count > 1 {
                            return inv("the quantity of the input must be exactly one");
                        }
                        // Nova drops this state-dependent validity rule; the
                        // anti-spam PoW and fee-on-failure replace it.
                        if !nova && self.account(&a)?.is_none() {
                            return Ok(ImportOutcome::Deferred("address isn't exist".into()));
                        }
                    }
                    if matches!(link.kind, FieldType::Input | FieldType::Output) {
                        if tx_fee.is_zero() {
                            return inv("when constructing a block, the fee entered is illegal");
                        }
                        let limit = match fees::output_limit(block, &p) {
                            Ok(l) => l,
                            Err(_) => return inv("transaction without outputs"),
                        };
                        if limit.is_zero() {
                            return inv("when constructing a block, the fee entered is illegal");
                        }
                        if link.kind == FieldType::Input && amt < tx_fee {
                            return inv("ref input amount < gas");
                        } else if link.kind == FieldType::Output && amt < limit {
                            return inv("ref output amount < gas");
                        }
                    }
                }
            }
            if !amt.is_zero() {
                is_extra = false;
            }
        }

        if block.is_account_tx() {
            if block.tx_nonce.is_none() {
                return inv("account transaction block must have nonce");
            }
        } else if block.tx_nonce.is_some() {
            return inv("only account transactions carry a nonce");
        }

        if !self.can_use_input(block, &pre.keys)? {
            return inv("block's input can't be used");
        }

        if nova {
            let nova_p = p.nova.as_ref().unwrap();
            let candidate = ep.is_end_of_epoch(block.time) && block.nonce.is_some();
            if !candidate && pow::pow_zero_bits(&block.hash().0) < nova_p.min_link_pow_bits {
                return inv("insufficient anti-spam proof of work");
            }
            if block.ext_root.is_some() && pre.payload.is_none() {
                return inv("missing verified payload");
            }
            if block.ext_root.is_some() && block.is_tx() {
                return inv("transaction blocks cannot carry a payload");
            }
            // amounts must add up without overflow (xdagj threw mid-application)
            let mut sum_in: u64 = 0;
            let mut sum_out: u64 = 0;
            for l in block.links() {
                let a = l.amount.to_nano_legacy().unwrap().0;
                let r = if matches!(l.kind, FieldType::In | FieldType::Input) { &mut sum_in } else { &mut sum_out };
                *r = r.checked_add(a).filter(|v| *v <= i64::MAX as u64).ok_or(ChainError::Invalid("amount sum overflow".into()))?;
            }
        }

        // --- mutation ------------------------------------------------------
        // (xdagj defines a REMARK flag but never sets it; neither do we)
        let mut dflags = if is_extra { flags::EXTRA } else { 0 };
        let action = if is_extra { RemoveAction::Extra } else { RemoveAction::Normal };
        let refs: Vec<HashLow> = block.block_links().collect();
        for r in refs {
            self.remove_orphan(&r, action)?;
        }

        self.check_new_main()?;

        if self.opts.our_keys.iter().any(|k| block.outsig_signed_by(k)) {
            dflags |= flags::OURS;
        }

        let own = pow::own_difficulty(block, &p, &self.rx, &*self.pow);
        let (diff, max_link) = self.calc_diff(block, own)?;

        self.process_extra()?;

        let payload_txs = pre.payload.as_ref().map(|pl| pl.txs.len() as u32).unwrap_or(0);
        let info = BlockInfo {
            hash: block.hash(),
            time: block.time,
            type_word: block.type_word,
            flags: dflags,
            difficulty: diff,
            max_diff_link: max_link,
            remark: block.remark,
            snapshot: None,
            ext_root: if nova { block.ext_root } else { None },
            payload_txs,
        };
        self.pending = Some((hl, info));

        let mut outcome = ImportOutcome::ImportedNotBest;
        let mut best = diff > self.meta.top_diff;
        let mut anc = None;
        if best {
            anc = self.find_ancestor(&hl)?;
            if self.forks_below_checkpoint(anc)? {
                tracing::warn!(block = ?hl, "heavier chain forks below the snapshot checkpoint: not adopted");
                best = false;
            }
        }
        if best {
            self.unwind_main(anc)?;
            self.update_new_chain(&hl)?;
            self.meta.top = Some(hl);
            self.meta.top_diff = diff;
            self.mark_meta_dirty();
            self.events.push(ChainEvent::NewTop { top: hl, diff });
            outcome = ImportOutcome::ImportedBest;
        }

        let (_, info) = self.pending.take().unwrap();
        if is_extra {
            self.extra.insert(hl, ExtraEntry { block: block.clone(), info, payload: pre.payload });
            self.extra_order.push_back(hl);
        } else {
            self.save_block(block, info, pre.payload, true)?;
        }
        Ok(outcome)
    }

    /// xdagj `canUseInput`.
    fn can_use_input(&mut self, block: &Block, keys: &[PublicKey]) -> Result<bool> {
        for input in &block.inputs {
            let ok = match input.target {
                LinkTarget::Address(a) => keys.iter().any(|k| pubkey_address(k) == a),
                LinkTarget::Block(r) => {
                    let info = self.info(&r)?.ok_or_else(|| ChainError::Corrupt("input block vanished".into()))?;
                    match &info.snapshot {
                        Some(SnapshotKey::PublicKey(pk)) => keys.iter().any(|k| k.serialize() == *pk),
                        Some(SnapshotKey::RawBlock(raw)) => match Block::parse(&raw[..]) {
                            Ok(b) => keys.iter().any(|k| b.outsig_signed_by(k)),
                            Err(_) => false,
                        },
                        None => match self.block(&r)? {
                            Some(b) => keys.iter().any(|k| b.outsig_signed_by(k)),
                            None => false,
                        },
                    }
                }
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// xdagj `calculateBlockDiff`: cumulative difficulty and max-diff link.
    fn calc_diff(&mut self, block: &Block, own: u128) -> Result<(Difficulty, Option<HashLow>)> {
        let ep = self.params.epochs();
        let cu = U256::from(own);
        let bepoch = ep.epoch(block.time);
        let mut max = cu;
        let mut max_link = None;
        let links: Vec<HashLow> = block.block_links().collect();
        for r in links {
            let Some(ri) = self.info(&r)? else { break };
            let cur = if ep.epoch(ri.time) < bepoch {
                ri.difficulty + cu
            } else {
                let mut cur = ri.difficulty;
                if let Some((exit_epoch, exit_diff)) = self.epoch_exit_of(&r, &ri)? {
                    if exit_epoch < bepoch && exit_diff + cu > cur {
                        cur = exit_diff + cu;
                    }
                }
                cur
            };
            if cur > max {
                max = cur;
                max_link = Some(r);
            }
        }
        Ok((max, max_link))
    }

    /// First block on `h`'s max-diff-link chain (starting at `h`) whose epoch
    /// differs from `h`'s epoch.
    fn epoch_exit_of(&mut self, h: &HashLow, info: &BlockInfo) -> Result<Option<(u64, Difficulty)>> {
        if let Some(v) = self.epoch_exit.get(h) {
            return Ok(*v);
        }
        let ep = self.params.epochs();
        let e = ep.epoch(info.time);
        let res = match info.max_diff_link {
            None => None,
            Some(m) => match self.info(&m)? {
                None => None,
                Some(mi) => {
                    if ep.epoch(mi.time) != e {
                        Some((ep.epoch(mi.time), mi.difficulty))
                    } else {
                        self.epoch_exit_of(&m, &mi)?
                    }
                }
            },
        };
        self.epoch_exit.put(*h, res);
        Ok(res)
    }

    /// xdagj `removeOrphan`, iterative.
    pub(crate) fn remove_orphan(&mut self, h: &HashLow, action: RemoveAction) -> Result<()> {
        let mut work = vec![(*h, action)];
        while let Some((h, action)) = work.pop() {
            let Some(mut info) = self.info(&h)? else { continue };
            if info.snapshot.is_some() {
                continue;
            }
            if info.flags & flags::REF != 0 || (action == RemoveAction::Extra && info.flags & flags::EXTRA == 0) {
                continue;
            }
            if info.flags & flags::EXTRA != 0 {
                let Some(entry) = self.extra.remove(&h) else { continue };
                if action == RemoveAction::Reuse {
                    continue; // evicted: dropped without saving
                }
                info.flags &= !flags::EXTRA;
                info.flags |= flags::REF;
                let links: Vec<HashLow> = entry.block.block_links().collect();
                self.save_block(&entry.block, info, entry.payload, false)?;
                for r in links.into_iter().rev() {
                    work.push((r, RemoveAction::Normal));
                }
            } else {
                self.remove_noref(&h)?;
                info.flags |= flags::REF;
                self.put_info(&h, info)?;
            }
        }
        Ok(())
    }

    fn remove_noref(&mut self, h: &HashLow) -> Result<()> {
        if self.ov.get(Table::NoRef, &h.0)?.is_some() {
            self.ov.delete(Table::NoRef, h.0.to_vec())?;
            if let Some(b) = self.block(h)? {
                if b.is_account_tx() {
                    self.noref_account_txs = self.noref_account_txs.saturating_sub(1);
                }
            }
        }
        Ok(())
    }

    /// xdagj `processExtraBlock`: evict the oldest candidate beyond the cap.
    fn process_extra(&mut self) -> Result<()> {
        while self.extra.len() > self.opts.max_extra {
            let Some(old) = self.extra_order.pop_front() else { break };
            if self.extra.contains_key(&old) {
                self.remove_orphan(&old, RemoveAction::Reuse)?;
            }
        }
        if self.extra_order.len() > 2 * self.opts.max_extra.max(16) {
            let extra = &self.extra;
            self.extra_order.retain(|h| extra.contains_key(h));
        }
        Ok(())
    }

    pub(crate) fn save_block(&mut self, block: &Arc<Block>, info: BlockInfo, payload: Option<VerifiedPayload>, new_orphan: bool) -> Result<()> {
        let hl = block.hashlow();
        let ep = self.params.epochs();
        self.ov.put(Table::BlockRaw, hl.0.to_vec(), block.raw().to_vec())?;
        self.ov.put(Table::TimeIndex, keys::time_index(ep.epoch(block.time), &hl), vec![])?;
        if info.flags & flags::OURS != 0 {
            self.ov.put(Table::Ours, hl.0.to_vec(), vec![])?;
        }
        if let Some(pl) = &payload {
            self.ov.put(Table::Payload, hl.0.to_vec(), pl.raw.to_vec())?;
            self.payload_cache.put(hl, pl.clone());
        }
        crate::sums::add_block(&mut self.ov, block.time, block.sum())?;
        if new_orphan && info.flags & flags::REF == 0 {
            self.ov.put(Table::NoRef, hl.0.to_vec(), vec![])?;
            if block.is_account_tx() {
                self.noref_account_txs += 1;
            }
        }
        self.put_info(&hl, info)?;
        self.block_cache.put(hl, block.clone());
        self.meta.nblocks += 1;
        self.mark_meta_dirty();
        self.events.push(ChainEvent::Stored { block: hl });
        Ok(())
    }

    // ------------------------------------------------------------------
    // main chain selection
    // ------------------------------------------------------------------

    fn max_link_diff(&mut self, info: &BlockInfo) -> Result<Option<Difficulty>> {
        match info.max_diff_link {
            None => Ok(None),
            Some(m) => Ok(self.info(&m)?.map(|i| i.difficulty)),
        }
    }

    /// xdagj `findAncestor` (fork variant: flags are set by `update_new_chain`).
    fn find_ancestor(&mut self, start: &HashLow) -> Result<Option<HashLow>> {
        let ep = self.params.epochs();
        let mut cur = Some(*start);
        let mut b0: Option<(HashLow, u64)> = None;
        while let Some(h) = cur {
            let Some(info) = self.info(&h)? else {
                cur = None;
                break;
            };
            if info.flags & flags::MAIN_CHAIN != 0 {
                break;
            }
            let tmp_diff = self.max_link_diff(&info)?;
            let e = ep.epoch(info.time);
            let cond = match tmp_diff {
                None => true,
                Some(d) => info.difficulty > d,
            };
            if cond && b0.is_none_or(|(_, e0)| e0 > e) {
                b0 = Some((h, e));
            }
            cur = if tmp_diff.is_some() { info.max_diff_link } else { None };
        }
        if let (Some(br), Some((b0h, b0e))) = (cur, b0) {
            if br != b0h {
                if let Some(bi) = self.info(&br)? {
                    if ep.epoch(bi.time) == b0e {
                        cur = bi.max_diff_link;
                    }
                }
            }
        }
        Ok(cur)
    }

    /// Main blocks up to the snapshot height are final on a node bootstrapped
    /// from a snapshot (their undo journals are not part of it).
    fn forks_below_checkpoint(&mut self, anc: Option<HashLow>) -> Result<bool> {
        if self.meta.snapshot_height == 0 {
            return Ok(false);
        }
        let Some(a) = anc else { return Ok(true) };
        let st = self.state(&a)?;
        Ok(st.flags & flags::MAIN != 0 && st.height < self.meta.snapshot_height)
    }

    /// xdagj `unWindMain`.
    fn unwind_main(&mut self, anc: Option<HashLow>) -> Result<()> {
        let mut cur = self.meta.top;
        while let Some(h) = cur {
            if Some(h) == anc {
                break;
            }
            let Some(mut info) = self.info(&h)? else { break };
            if info.snapshot.is_some() {
                break; // snapshot blocks have no raw data; xdagj's walk ends here too
            }
            let st = self.state(&h)?;
            if self.meta.snapshot_height > 0 && st.flags & flags::MAIN != 0 && st.height <= self.meta.snapshot_height {
                break; // snapshot checkpoint (see forks_below_checkpoint)
            }
            info.flags &= !flags::MAIN_CHAIN;
            let next = info.max_diff_link;
            self.put_info(&h, info)?;
            if self.state(&h)?.flags & flags::MAIN != 0 {
                self.unset_main_to(&h)?;
            }
            cur = next;
        }
        Ok(())
    }

    /// xdagj `updateNewChain`.
    fn update_new_chain(&mut self, start: &HashLow) -> Result<()> {
        let ep = self.params.epochs();
        let mut cur = Some(*start);
        let mut b0: Option<u64> = None;
        let mut marked = vec![];
        while let Some(h) = cur {
            let Some(mut info) = self.info(&h)? else { break };
            if info.flags & flags::MAIN_CHAIN != 0 {
                break;
            }
            let tmp_diff = self.max_link_diff(&info)?;
            let e = ep.epoch(info.time);
            let cond = match tmp_diff {
                None => true,
                Some(d) => info.difficulty > d,
            };
            let next = if tmp_diff.is_some() { info.max_diff_link } else { None };
            if cond && b0.is_none_or(|e0| e0 > e) {
                info.flags |= flags::MAIN_CHAIN;
                self.put_info(&h, info)?;
                b0 = Some(e);
                marked.push(h);
            }
            cur = next;
        }
        // Newly marked main-chain blocks (except the newest) reference their
        // links as if they had been saved (xdagj removes them from the orphan
        // pool here, which sets REF on still-unreferenced links).
        if marked.len() > 1 {
            for h in marked.iter().skip(1) {
                if let Some(b) = self.block(h)? {
                    let links: Vec<HashLow> = b.block_links().collect();
                    for l in links {
                        if let Some(li) = self.info(&l)? {
                            if li.flags & flags::REF == 0 {
                                self.remove_orphan(&l, RemoveAction::Normal)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// xdagj `checkNewMain`: confirm the oldest unconfirmed main-chain block
    /// once it is referenced, has a successor and is 2 s old.
    pub fn check_new_main(&mut self) -> Result<Option<u64>> {
        let now = self.clock.now_xdag();
        let mut p: Option<(HashLow, BlockInfo)> = None;
        let mut i = 0;
        let mut cur = self.meta.top;
        let mut guard = 0u64;
        while let Some(h) = cur {
            let Some(info) = self.info(&h)? else { break };
            if self.state(&h)?.flags & flags::MAIN != 0 {
                break;
            }
            if info.flags & flags::MAIN_CHAIN != 0 {
                i += 1;
                cur = info.max_diff_link;
                p = Some((h, info));
            } else {
                cur = info.max_diff_link;
            }
            guard += 1;
            if guard > 10_000_000 {
                return Err(ChainError::Corrupt("max-diff-link cycle".into()));
            }
        }
        if let Some((h, info)) = p {
            if info.flags & flags::REF != 0 && i > 1 && now >= info.time + 2 * 1024 {
                let height = self.set_main(&h)?;
                return Ok(Some(height));
            }
        }
        Ok(None)
    }

    /// Periodic maintenance (xdagj `checkState`, every 1024 ms): confirm main
    /// blocks and persist.
    pub fn tick(&mut self) -> Result<Vec<u64>> {
        let mut confirmed = vec![];
        while let Some(h) = self.check_new_main()? {
            confirmed.push(h);
        }
        self.commit(false)?;
        Ok(confirmed)
    }

    /// Unset main blocks from the tip down to (and including) `h`, preserving
    /// the LIFO order the undo journal needs.
    fn unset_main_to(&mut self, h: &HashLow) -> Result<()> {
        let target = self.state(h)?.height;
        if target == 0 {
            return Ok(());
        }
        while self.meta.nmain >= target {
            let height = self.meta.nmain;
            let Some(b) = self.main_at(height)? else {
                return Err(ChainError::Corrupt(format!("no main block at height {height}")));
            };
            self.unset_main(&b, height)?;
        }
        Ok(())
    }
}
