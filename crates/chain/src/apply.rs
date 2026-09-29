//! Main-block application (xdagj `setMain` / `applyBlock` / `unSetMain`).
//!
//! Legacy rules reproduce xdagj 0.8.4 exactly, including:
//! * the `-1` / `0` / fee return codes of `applyBlock` (not processed /
//!   rejected / applied);
//! * the nonce rules of account transactions;
//! * balances stored as C-units and re-read through the lossy conversion;
//! * an arithmetic overflow aborting the rest of the main block's
//!   application (xdagj throws out of `setMain`, leaving what was done).
//!
//! Unapplying uses the undo journal, so it is exact by construction.

use std::sync::Arc;

use xdag_storage::Table;
use xdag_types::{Address, Block, FieldType, HashLow, LinkTarget, Nano};

use crate::chain::{Chain, ChainEvent};
use crate::evm_api::{EvmAccount, EvmChanges, EvmEnv, EvmStateAccess};
use crate::fees;
use crate::keys;
use crate::overlay::UndoLog;
use crate::preverify::{VerifiedPayload, VerifiedTx};
use crate::records::{flags, AccountRecord, Balance, Direction, HistoryEntry, TxLocation, TxStatus};
use crate::{ChainError, Result};

/// Result of `applyBlock` for one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applied {
    /// xdagj `-1`: already processed, or not processable yet (nonce gap).
    Skip,
    /// Processed; fee to hand to the referencing block (0 when rejected).
    Gas(Nano),
}

/// Aborts the main block application (xdagj exception inside `setMain`).
#[derive(Debug)]
struct Abort;

struct Ctx {
    legacy: bool,
    height: u64,
    main_time: u64,
    coinbase: Option<Address>,
    main_hash: [u8; 32],
    evm_gas_used: u64,
    evm_seq: u32,
}

struct Frame {
    h: HashLow,
    block: Arc<Block>,
    links: Vec<HashLow>,
    next: usize,
    gas: Nano,
    is_main: bool,
    /// link currently being applied (child frame on top of this one)
    waiting: Option<HashLow>,
}

pub(crate) fn subject_addr(a: &Address) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&a.0);
    v
}

pub(crate) fn subject_block(h: &HashLow) -> Vec<u8> {
    let mut v = vec![2u8];
    v.extend_from_slice(&h.0);
    v
}

impl Chain {
    /// xdagj `setMain`. Returns the new height.
    pub(crate) fn set_main(&mut self, h: &HashLow) -> Result<u64> {
        let p = self.params.clone();
        let height = self.meta.nmain + 1;
        let block = self.block(h)?.ok_or_else(|| ChainError::Corrupt("main candidate has no raw block".into()))?;
        let legacy = !p.is_nova_time(block.time);
        let mut ctx = Ctx { legacy, height, main_time: block.time, coinbase: block.coinbase, main_hash: block.hash().0, evm_gas_used: 0, evm_seq: 0 };
        self.history_seq = 0;
        self.ov.begin_journal();

        let reward = fees::reward(height, &p);
        let mut st = self.state(h)?;
        st.height = height;
        st.flags |= flags::MAIN;
        st.amount = st.amount.checked_add(reward.0 as i64).ok_or(ChainError::Overflow)?;
        self.put_state(h, &st)?;
        self.ov.put(Table::MainHeight, keys::height(height), h.0.to_vec())?;
        self.meta.nmain = height;
        self.mark_meta_dirty();

        let res = self.apply_block(&mut ctx, h, block.clone());
        let mut fee_total = Nano::ZERO;
        match res {
            Ok(Applied::Skip) => {}
            Ok(Applied::Gas(fee)) => {
                let mut st = self.state(h)?;
                st.amount = st.amount.checked_add(fee.0 as i64).ok_or(ChainError::Overflow)?;
                st.fee = fee;
                st.ref_ = Some(*h);
                self.put_state(h, &st)?;
                fee_total = fee;
                let seed_source = match crate::pow::RxSchedule::seed_source_height(&p, height) {
                    Some(sh) => self.main_at(sh)?,
                    None => None,
                };
                self.rx.on_set_main(&p, height, block.time, seed_source);
            }
            Err(Abort) => {
                tracing::warn!(height, "main block application aborted on overflow (xdagj-compatible)");
            }
        }
        self.record(
            &subject_block(h),
            HistoryEntry {
                tx: h.0.to_vec(),
                direction: Direction::Earning,
                amount: Nano(reward.0 + fee_total.0),
                counterparty: None,
                time: block.time,
                main_height: height,
                status: TxStatus::Applied,
                remark: vec![],
            },
        )?;

        let entries = self.ov.end_journal();
        let log = UndoLog { legacy, entries };
        self.ov.put(Table::Journal, keys::height(height), log.encode())?;
        self.events.push(ChainEvent::MainSet { height, block: *h });
        Ok(height)
    }

    /// xdagj `unSetMain`, via the undo journal.
    pub(crate) fn unset_main(&mut self, h: &HashLow, height: u64) -> Result<()> {
        let raw =
            self.ov.get(Table::Journal, &keys::height(height))?.ok_or_else(|| ChainError::Corrupt(format!("missing journal for height {height}")))?;
        let log = UndoLog::decode(&raw)?;
        self.ov.undo(log)?;
        self.ov.delete(Table::Journal, keys::height(height))?;
        self.meta.nmain = height - 1;
        let params = self.params.clone();
        self.rx.on_unset_main(&params, height);
        self.mark_meta_dirty();
        self.events.push(ChainEvent::MainUnset { height, block: *h });
        Ok(())
    }

    /// Iterative xdagj `applyBlock(flag, block)`.
    fn apply_block(&mut self, ctx: &mut Ctx, root: &HashLow, root_block: Arc<Block>) -> std::result::Result<Applied, Abort> {
        let r = self.apply_block_inner(ctx, root, root_block);
        match r {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("storage error while applying main block: {e}");
                Err(Abort)
            }
        }
    }

    fn apply_block_inner(&mut self, ctx: &mut Ctx, root: &HashLow, root_block: Arc<Block>) -> Result<std::result::Result<Applied, Abort>> {
        // enter root
        let mut stack: Vec<Frame> = vec![];
        match self.enter(root, root_block, true)? {
            Some(f) => stack.push(f),
            None => return Ok(Ok(Applied::Skip)),
        }
        let mut last: Option<Applied> = None;
        loop {
            let top = stack.len() - 1;
            // consume the result of a finished child
            if let Some(res) = last.take() {
                let child = stack[top].waiting.take().unwrap();
                if let Applied::Gas(g) = res {
                    match stack[top].gas.checked_add(g) {
                        Some(v) => stack[top].gas = v,
                        None => return Ok(Err(Abort)),
                    }
                    let mut cst = self.state(&child)?;
                    cst.ref_ = Some(stack[top].h);
                    self.put_state(&child, &cst)?;
                }
            }
            // descend into the next unapplied link
            let mut descended = false;
            while stack[top].next < stack[top].links.len() {
                let l = stack[top].links[stack[top].next];
                stack[top].next += 1;
                let st = self.state(&l)?;
                if st.flags & flags::MAIN_REF != 0 {
                    continue;
                }
                let Some(b) = self.block(&l)? else {
                    continue; // snapshot block without raw data: nothing to apply
                };
                // xdagj resets the child's fee before applying it
                let mut cst = st;
                cst.fee = Nano::ZERO;
                self.put_state(&l, &cst)?;
                stack[top].waiting = Some(l);
                match self.enter(&l, b, false)? {
                    Some(f) => {
                        stack.push(f);
                        descended = true;
                        break;
                    }
                    None => {
                        last = Some(Applied::Skip);
                        break;
                    }
                }
            }
            if descended || last.is_some() {
                continue;
            }
            // all links handled: process the block's own transaction
            let frame = stack.pop().unwrap();
            let res = match self.finish(ctx, &frame)? {
                Ok(r) => r,
                Err(Abort) => return Ok(Err(Abort)),
            };
            if stack.is_empty() {
                return Ok(Ok(res));
            }
            last = Some(res);
        }
    }

    /// Start applying a block: returns None if it was already processed.
    fn enter(&mut self, h: &HashLow, block: Arc<Block>, is_main: bool) -> Result<Option<Frame>> {
        let mut st = self.state(h)?;
        if st.flags & flags::MAIN_REF != 0 {
            return Ok(None);
        }
        st.flags |= flags::MAIN_REF;
        self.put_state(h, &st)?;
        let links: Vec<HashLow> = block.block_links().collect();
        Ok(Some(Frame { h: *h, block, links, next: 0, gas: Nano::ZERO, is_main, waiting: None }))
    }

    /// Second half of `applyBlock`: inputs/outputs of the block itself.
    fn finish(&mut self, ctx: &mut Ctx, f: &Frame) -> Result<std::result::Result<Applied, Abort>> {
        let block = f.block.clone();
        if block.links().next().is_none() && block.ext_root.is_none() {
            let mut st = self.state(&f.h)?;
            st.flags |= flags::APPLIED;
            self.put_state(&f.h, &st)?;
            return Ok(Ok(Applied::Gas(Nano::ZERO)));
        }
        let own = if ctx.legacy { self.own_tx_legacy(ctx, f)? } else { self.own_tx_nova(ctx, f)? };
        let own = match own {
            Ok(o) => o,
            Err(Abort) => return Ok(Err(Abort)),
        };
        match own {
            OwnResult::Skip => Ok(Ok(Applied::Skip)),
            OwnResult::Rejected => Ok(Ok(Applied::Gas(Nano::ZERO))),
            OwnResult::Applied(block_gas) => {
                let mut extra_gas = Nano::ZERO;
                if !ctx.legacy && block.ext_root.is_some() {
                    extra_gas = self.execute_payload(ctx, &f.h, &block)?;
                }
                let mut st = self.state(&f.h)?;
                st.flags |= flags::APPLIED;
                let is_tx = block.is_tx();
                let ret = if !f.is_main && is_tx {
                    st.fee = block_gas;
                    block_gas
                } else if !f.is_main {
                    let g = f.gas.checked_add(extra_gas).unwrap_or(f.gas);
                    st.fee = g;
                    g
                } else if f.gas.is_zero() && !block_gas.is_zero() {
                    block_gas
                } else {
                    f.gas.checked_add(extra_gas).unwrap_or(f.gas)
                };
                self.put_state(&f.h, &st)?;
                Ok(Ok(Applied::Gas(ret)))
            }
        }
    }
}

enum OwnResult {
    Skip,
    Rejected,
    Applied(Nano),
}

impl Chain {
    fn nonce_processed(&mut self, a: &Address) -> Result<u64> {
        let mut r = self.account(a)?.unwrap_or_default();
        r.nonce += 1;
        self.put_account(a, &r)?;
        Ok(r.nonce)
    }

    /// Legacy `applyBlock` body for one block.
    fn own_tx_legacy(&mut self, ctx: &mut Ctx, f: &Frame) -> Result<std::result::Result<OwnResult, Abort>> {
        let p = self.params.clone();
        let block = f.block.clone();
        let h = f.h;
        let mut sum_in: i64 = 0;
        let mut sum_out: i64 = 0;
        let nano = |l: &xdag_types::Link| l.amount.to_nano_legacy().map(|n| n.0 as i64).unwrap_or(0);
        for link in block.links() {
            let amt = nano(link);
            match (link.kind, link.target) {
                (FieldType::Input, LinkTarget::Address(a)) => {
                    let acct = self.account(&a)?.unwrap_or_default();
                    let balance = acct.balance_nano_legacy()?;
                    let executed = acct.nonce;
                    let bn = block.tx_nonce.unwrap_or(0);
                    if bn > executed + 1 {
                        return Ok(Ok(OwnResult::Skip));
                    }
                    if bn <= executed {
                        return Ok(Ok(OwnResult::Skip));
                    }
                    if (balance.0 as i64) < amt {
                        let n = self.nonce_processed(&a)?;
                        self.tx_done(ctx, &h, &block, TxStatus::Rejected, Some(a), n)?;
                        return Ok(Ok(OwnResult::Rejected));
                    }
                    sum_in = match sum_in.checked_add(amt) {
                        Some(v) => v,
                        None => return Ok(Err(Abort)),
                    };
                }
                (FieldType::In, LinkTarget::Block(r)) => {
                    let rst = self.state(&r)?;
                    if rst.amount < amt {
                        self.tx_done(ctx, &h, &block, TxStatus::Rejected, None, 0)?;
                        return Ok(Ok(OwnResult::Rejected));
                    }
                    sum_in = match sum_in.checked_add(amt) {
                        Some(v) => v,
                        None => return Ok(Err(Abort)),
                    };
                }
                _ => {
                    sum_out = match sum_out.checked_add(amt) {
                        Some(v) => v,
                        None => return Ok(Err(Abort)),
                    };
                }
            }
        }
        let own_amount = self.state(&h)?.amount;
        let lhs = match own_amount.checked_add(sum_in) {
            Some(v) => v,
            None => return Ok(Err(Abort)),
        };
        if lhs < sum_out || own_amount < 0 || sum_in != sum_out {
            let mut who = None;
            let mut n = 0;
            if let Some(first) = block.inputs.first() {
                if let (FieldType::Input, LinkTarget::Address(a)) = (first.kind, first.target) {
                    n = self.nonce_processed(&a)?;
                    who = Some(a);
                }
            }
            if block.is_tx() {
                self.tx_done(ctx, &h, &block, TxStatus::Rejected, who, n)?;
            }
            return Ok(Ok(OwnResult::Rejected));
        }

        let limit = if block.is_tx() { fees::output_limit(&block, &p).unwrap_or(Nano::ZERO) } else { Nano::ZERO };
        let mut block_gas = Nano::ZERO;
        let mut sender: Option<Address> = None;
        let mut sender_nonce = 0;
        for link in block.links() {
            let amt = nano(link);
            match (link.kind, link.target) {
                (FieldType::In, LinkTarget::Block(r)) => {
                    let mut rst = self.state(&r)?;
                    rst.amount -= amt; // may go negative with duplicate IN links, as in xdagj
                    self.put_state(&r, &rst)?;
                    self.hist_block(ctx, &r, &h, &block, Direction::Input, Nano(amt as u64), TxStatus::Applied)?;
                }
                (FieldType::Input, LinkTarget::Address(a)) => {
                    self.legacy_add(&a, -(amt as i128))?;
                    sender_nonce = self.nonce_processed(&a)?;
                    sender = Some(a);
                    self.hist_addr(ctx, &a, &h, &block, Direction::Input, Nano(amt as u64), TxStatus::Applied, None)?;
                }
                (FieldType::Output, LinkTarget::Address(a)) => {
                    let credit = (amt as i128) - limit.0 as i128;
                    self.legacy_add(&a, credit)?;
                    block_gas = match block_gas.checked_add(limit) {
                        Some(v) => v,
                        None => return Ok(Err(Abort)),
                    };
                    self.hist_addr(ctx, &a, &h, &block, Direction::Output, Nano(credit.max(0) as u64), TxStatus::Applied, sender)?;
                }
                _ => {}
            }
        }
        if block.is_tx() {
            self.tx_done(ctx, &h, &block, TxStatus::Applied, sender, sender_nonce)?;
        }
        Ok(Ok(OwnResult::Applied(block_gas)))
    }

    /// Add (or subtract) nano to a legacy account, reproducing xdagj's
    /// `addAmount`/`subtractAmount` storage round-trip.
    fn legacy_add(&mut self, a: &Address, delta: i128) -> Result<()> {
        let mut r = self.account(a)?.unwrap_or_default();
        match r.balance {
            Balance::Legacy(_) => {
                let cur = r.balance_nano_legacy()?.0 as i128;
                let new = (cur + delta).max(0) as u64;
                r.balance = Balance::Legacy(Nano(new).to_camount_legacy().0);
            }
            Balance::Wei(w) => {
                let d = delta * xdag_types::amount::WEI_PER_NANO as i128;
                r.balance = Balance::Wei(((w as i128) + d).max(0) as u128);
            }
        }
        self.put_account(a, &r)
    }

    fn wei_balance(&mut self, a: &Address) -> Result<(AccountRecord, u128)> {
        let r = self.account(a)?.unwrap_or_default();
        let w = r.balance_wei()?;
        Ok((r, w))
    }

    fn set_wei(&mut self, a: &Address, mut r: AccountRecord, w: u128) -> Result<()> {
        r.balance = Balance::Wei(w);
        self.put_account(a, &r)
    }

    /// Nova `applyBlock` body: exact arithmetic, fee-on-failure, aggregated
    /// block-balance spends.
    fn own_tx_nova(&mut self, ctx: &mut Ctx, f: &Frame) -> Result<std::result::Result<OwnResult, Abort>> {
        let p = self.params.clone();
        let block = f.block.clone();
        let h = f.h;
        let wei = xdag_types::amount::WEI_PER_NANO;
        let nano = |l: &xdag_types::Link| l.amount.to_nano_legacy().map(|n| n.0).unwrap_or(0);
        let mut sum_in: u64 = 0;
        let mut sum_out: u64 = 0;
        for l in block.links() {
            let a = nano(l);
            if matches!(l.kind, FieldType::In | FieldType::Input) {
                sum_in = sum_in.saturating_add(a);
            } else {
                sum_out = sum_out.saturating_add(a);
            }
        }
        if !block.is_tx() {
            // link / batch / main block: only zero-amount references
            if sum_in != 0 || sum_out != 0 {
                return Ok(Ok(OwnResult::Rejected));
            }
            return Ok(Ok(OwnResult::Applied(Nano::ZERO)));
        }
        let limit = fees::output_limit(&block, &p).unwrap_or(Nano::ZERO);
        let fee_total = limit.0.saturating_mul(block.outputs.iter().filter(|o| o.kind == FieldType::Output).count() as u64);

        if block.is_account_tx() {
            let (a, _) = block.account_input().unwrap();
            let (acct, bal) = self.wei_balance(&a)?;
            let bn = block.tx_nonce.unwrap_or(0);
            if bn != acct.nonce + 1 {
                return Ok(Ok(OwnResult::Skip));
            }
            let spend = sum_in as u128 * wei;
            if sum_in != sum_out || bal < spend {
                // fee-on-failure: charge the output fees if affordable
                let fee_w = fee_total as u128 * wei;
                if bal >= fee_w {
                    let mut r = acct.clone();
                    r.nonce += 1;
                    self.set_wei(&a, r.clone(), bal - fee_w)?;
                    self.hist_addr(ctx, &a, &h, &block, Direction::Fee, Nano(fee_total), TxStatus::Failed, None)?;
                    self.tx_done(ctx, &h, &block, TxStatus::Failed, Some(a), r.nonce)?;
                    return Ok(Ok(OwnResult::Applied(Nano(fee_total))));
                }
                self.tx_done(ctx, &h, &block, TxStatus::Rejected, Some(a), acct.nonce)?;
                return Ok(Ok(OwnResult::Rejected));
            }
            let mut r = acct.clone();
            r.nonce += 1;
            self.set_wei(&a, r.clone(), bal - spend)?;
            self.hist_addr(ctx, &a, &h, &block, Direction::Input, Nano(sum_in), TxStatus::Applied, None)?;
            let mut gas = 0u64;
            for o in block.outputs.iter().filter(|o| o.kind == FieldType::Output) {
                let to = o.address().unwrap();
                let credit = nano(o) - limit.0;
                let (tr, tb) = self.wei_balance(&to)?;
                self.set_wei(&to, tr, tb + credit as u128 * wei)?;
                gas += limit.0;
                self.hist_addr(ctx, &to, &h, &block, Direction::Output, Nano(credit), TxStatus::Applied, Some(a))?;
            }
            self.tx_done(ctx, &h, &block, TxStatus::Applied, Some(a), r.nonce)?;
            return Ok(Ok(OwnResult::Applied(Nano(gas))));
        }

        // main transaction: spend block balances (aggregated per source block)
        if sum_in != sum_out {
            self.tx_done(ctx, &h, &block, TxStatus::Rejected, None, 0)?;
            return Ok(Ok(OwnResult::Rejected));
        }
        let mut per_source: Vec<(HashLow, u64)> = vec![];
        for l in block.inputs.iter().filter(|l| l.kind == FieldType::In) {
            let r = l.block().unwrap();
            match per_source.iter_mut().find(|(x, _)| *x == r) {
                Some(e) => e.1 += nano(l),
                None => per_source.push((r, nano(l))),
            }
        }
        for (r, need) in &per_source {
            if self.state(r)?.amount < *need as i64 {
                self.tx_done(ctx, &h, &block, TxStatus::Rejected, None, 0)?;
                return Ok(Ok(OwnResult::Rejected));
            }
        }
        for (r, need) in &per_source {
            let mut rst = self.state(r)?;
            rst.amount -= *need as i64;
            self.put_state(r, &rst)?;
            self.hist_block(ctx, r, &h, &block, Direction::Input, Nano(*need), TxStatus::Applied)?;
        }
        let mut gas = 0u64;
        for o in block.outputs.iter().filter(|o| o.kind == FieldType::Output) {
            let to = o.address().unwrap();
            let credit = nano(o) - limit.0;
            let (tr, tb) = self.wei_balance(&to)?;
            self.set_wei(&to, tr, tb + credit as u128 * wei)?;
            gas += limit.0;
            self.hist_addr(ctx, &to, &h, &block, Direction::Output, Nano(credit), TxStatus::Applied, None)?;
        }
        self.tx_done(ctx, &h, &block, TxStatus::Applied, None, 0)?;
        Ok(Ok(OwnResult::Applied(Nano(gas))))
    }

    fn payload_of(&mut self, h: &HashLow, block: &Block) -> Result<Option<VerifiedPayload>> {
        if let Some(pl) = self.payload_cache.get(h) {
            return Ok(Some(pl.clone()));
        }
        let Some(raw) = self.ov.get(Table::Payload, &h.0)? else { return Ok(None) };
        let pre = crate::preverify::preverify(block, Some(Arc::new(raw)), &self.params, self.evm.as_deref())
            .map_err(|e| ChainError::Corrupt(format!("stored payload no longer verifies: {e}")))?;
        let pl = pre.payload;
        if let Some(pl) = &pl {
            self.payload_cache.put(*h, pl.clone());
        }
        Ok(pl)
    }

    /// Execute a Nova payload; returns the native fees collected (nano).
    fn execute_payload(&mut self, ctx: &mut Ctx, h: &HashLow, block: &Block) -> Result<Nano> {
        let Some(pl) = self.payload_of(h, block)? else {
            return Err(ChainError::Corrupt("batch block without payload".into()));
        };
        let nova = self.params.nova.clone().unwrap();
        let wei = xdag_types::amount::WEI_PER_NANO;
        let mut fees = 0u64;
        for (i, tx) in pl.txs.iter().enumerate() {
            match tx {
                VerifiedTx::Native { tx, sender, hash } => {
                    let (acct, bal) = self.wei_balance(sender)?;
                    if tx.nonce != acct.nonce + 1 {
                        continue; // stale or future nonce: not executable here
                    }
                    let need = (tx.amount.0 as u128 + tx.fee.0 as u128) * wei;
                    let fee_w = tx.fee.0 as u128 * wei;
                    let mut r = acct.clone();
                    let status = if bal >= need {
                        r.nonce += 1;
                        self.set_wei(sender, r.clone(), bal - need)?;
                        let (tr, tb) = self.wei_balance(&tx.to)?;
                        self.set_wei(&tx.to, tr, tb + tx.amount.0 as u128 * wei)?;
                        TxStatus::Applied
                    } else if bal >= fee_w {
                        r.nonce += 1;
                        self.set_wei(sender, r.clone(), bal - fee_w)?;
                        TxStatus::Failed
                    } else {
                        continue;
                    };
                    fees = fees.saturating_add(tx.fee.0);
                    let loc = TxLocation { block: *h, index: i as u32, main_height: ctx.height, status, sender: *sender, fee: tx.fee, gas_used: 0 };
                    self.ov.put(Table::TxIndex, hash.to_vec(), loc.encode())?;
                    let entry = |dir, amount, cp: Option<&Address>| HistoryEntry {
                        tx: hash.to_vec(),
                        direction: dir,
                        amount,
                        counterparty: cp.map(|a| a.0.to_vec()),
                        time: block.time,
                        main_height: ctx.height,
                        status,
                        remark: tx.remark.clone(),
                    };
                    let debit = if status == TxStatus::Applied { Nano(tx.amount.0 + tx.fee.0) } else { tx.fee };
                    self.record(&subject_addr(sender), entry(Direction::Input, debit, Some(&tx.to)))?;
                    if status == TxStatus::Applied {
                        self.record(&subject_addr(&tx.to), entry(Direction::Output, tx.amount, Some(sender)))?;
                    }
                    self.events.push(ChainEvent::TxDone { tx: hash.to_vec(), sender: Some(*sender), nonce: r.nonce });
                }
                VerifiedTx::Evm { raw, info } => {
                    let Some(evm) = self.evm.clone() else { continue };
                    if ctx.evm_gas_used.saturating_add(info.gas_limit) > nova.main_gas_limit {
                        continue; // main-block gas cap reached: skipped without effect (pools retry it later)
                    }
                    let env = EvmEnv {
                        chain_id: nova.chain_id,
                        height: ctx.height,
                        timestamp: xdag_types::time::xdag_to_ms(ctx.main_time) / 1000,
                        coinbase: ctx.coinbase.unwrap_or(Address::ZERO),
                        prevrandao: ctx.main_hash,
                        gas_limit: nova.batch_gas_limit,
                        min_gas_price: nova.min_gas_price,
                    };
                    let result = {
                        let mut view = StateView { chain: self };
                        evm.execute(&mut view, &env, raw)
                    };
                    let Ok(res) = result else { continue };
                    ctx.evm_gas_used += res.gas_used;
                    self.apply_evm_changes(&res.changes)?;
                    let status = if res.success { TxStatus::Applied } else { TxStatus::Failed };
                    let loc = TxLocation {
                        block: *h,
                        index: i as u32,
                        main_height: ctx.height,
                        status,
                        sender: info.sender,
                        fee: Nano::from_wei_floor(res.fee),
                        gas_used: res.gas_used,
                    };
                    self.ov.put(Table::TxIndex, info.hash.to_vec(), loc.encode())?;
                    self.ov.put(Table::Receipt, info.hash.to_vec(), crate::query::encode_receipt(&res, ctx.height))?;
                    let mut k = ctx.height.to_be_bytes().to_vec();
                    k.extend_from_slice(&ctx.evm_seq.to_be_bytes());
                    ctx.evm_seq += 1;
                    self.ov.put(Table::EvmTxs, k, info.hash.to_vec())?;
                    let debit = Nano::from_wei_floor(if res.success { info.value } else { 0 } + res.fee);
                    self.record(
                        &subject_addr(&info.sender),
                        HistoryEntry {
                            tx: info.hash.to_vec(),
                            direction: Direction::Input,
                            amount: debit,
                            counterparty: info.to.map(|a| a.0.to_vec()),
                            time: block.time,
                            main_height: ctx.height,
                            status,
                            remark: vec![],
                        },
                    )?;
                    if let (Some(to), true) = (info.to, res.success && info.value > 0) {
                        self.record(
                            &subject_addr(&to),
                            HistoryEntry {
                                tx: info.hash.to_vec(),
                                direction: Direction::Output,
                                amount: Nano::from_wei_floor(info.value),
                                counterparty: Some(info.sender.0.to_vec()),
                                time: block.time,
                                main_height: ctx.height,
                                status,
                                remark: vec![],
                            },
                        )?;
                    }
                    self.events.push(ChainEvent::TxDone { tx: info.hash.to_vec(), sender: Some(info.sender), nonce: info.nonce + 1 });
                }
            }
        }
        Ok(Nano(fees))
    }

    fn apply_evm_changes(&mut self, c: &EvmChanges) -> Result<()> {
        for (hash, code) in &c.codes {
            self.ov.put(Table::Code, hash.to_vec(), code.clone())?;
        }
        for a in &c.cleared_storage {
            let rows = self.ov.db().scan_prefix(Table::Storage, &a.0, usize::MAX)?;
            for (k, _) in rows {
                self.ov.delete(Table::Storage, k)?;
            }
        }
        for (a, slot, v) in &c.storage {
            let k = keys::storage_slot(a, slot);
            if *v == [0u8; 32] {
                self.ov.delete(Table::Storage, k)?;
            } else {
                self.ov.put(Table::Storage, k, v.to_vec())?;
            }
        }
        for (a, acct) in &c.accounts {
            match acct {
                Some(e) => {
                    let mut r = self.account(a)?.unwrap_or_default();
                    r.balance = Balance::Wei(e.balance);
                    r.nonce = e.nonce;
                    r.code_hash = e.code_hash;
                    self.put_account(a, &r)?;
                }
                None => {
                    self.ov.delete(Table::Account, a.0.to_vec())?;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // history
    // ------------------------------------------------------------------

    pub(crate) fn record(&mut self, subject: &[u8], e: HistoryEntry) -> Result<()> {
        if !self.opts.index_history {
            return Ok(());
        }
        self.history_seq += 1;
        let k = keys::history(subject, e.main_height, self.history_seq);
        self.ov.put(Table::History, k, e.encode())
    }

    #[allow(clippy::too_many_arguments)]
    fn hist_addr(
        &mut self,
        ctx: &Ctx,
        a: &Address,
        tx: &HashLow,
        block: &Block,
        dir: Direction,
        amount: Nano,
        status: TxStatus,
        cp: Option<Address>,
    ) -> Result<()> {
        self.record(
            &subject_addr(a),
            HistoryEntry {
                tx: tx.0.to_vec(),
                direction: dir,
                amount,
                counterparty: cp.map(|c| c.0.to_vec()),
                time: block.time,
                main_height: ctx.height,
                status,
                remark: block.remark.map(|r| r.to_vec()).unwrap_or_default(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn hist_block(&mut self, ctx: &Ctx, b: &HashLow, tx: &HashLow, block: &Block, dir: Direction, amount: Nano, status: TxStatus) -> Result<()> {
        self.record(
            &subject_block(b),
            HistoryEntry {
                tx: tx.0.to_vec(),
                direction: dir,
                amount,
                counterparty: None,
                time: block.time,
                main_height: ctx.height,
                status,
                remark: block.remark.map(|r| r.to_vec()).unwrap_or_default(),
            },
        )
    }

    /// Record the execution status of a transaction block.
    fn tx_done(&mut self, ctx: &Ctx, h: &HashLow, block: &Block, status: TxStatus, sender: Option<Address>, nonce: u64) -> Result<()> {
        let mut st = self.state(h)?;
        st.status = status as u8;
        self.put_state(h, &st)?;
        let fee = fees::tx_fee(block, &self.params).unwrap_or(Nano::ZERO);
        let loc = TxLocation { block: *h, index: 0, main_height: ctx.height, status, sender: sender.unwrap_or(Address::ZERO), fee, gas_used: 0 };
        self.ov.put(Table::TxIndex, h.0.to_vec(), loc.encode())?;
        if status != TxStatus::Applied {
            if let Some(a) = sender {
                if let Some((_, amt)) = block.account_input() {
                    let amount = amt.to_nano_legacy().unwrap_or(Nano::ZERO);
                    self.hist_addr(ctx, &a, h, block, Direction::Input, amount, status, None)?;
                }
            }
        }
        self.events.push(ChainEvent::TxDone { tx: h.0.to_vec(), sender, nonce });
        Ok(())
    }
}

/// EVM view over the chain state during application.
pub(crate) struct StateView<'a> {
    pub chain: &'a mut Chain,
}

impl EvmStateAccess for StateView<'_> {
    fn account(&mut self, a: &Address) -> std::result::Result<Option<EvmAccount>, String> {
        let r = self.chain.account(a).map_err(|e| e.to_string())?;
        match r {
            None => Ok(None),
            Some(r) => Ok(Some(EvmAccount { balance: r.balance_wei().map_err(|e| e.to_string())?, nonce: r.nonce, code_hash: r.code_hash })),
        }
    }

    fn code(&mut self, code_hash: &[u8; 32]) -> std::result::Result<Vec<u8>, String> {
        Ok(self.chain.ov.get(Table::Code, code_hash).map_err(|e| e.to_string())?.unwrap_or_default())
    }

    fn storage(&mut self, a: &Address, slot: &[u8; 32]) -> std::result::Result<[u8; 32], String> {
        let v = self.chain.ov.get(Table::Storage, &keys::storage_slot(a, slot)).map_err(|e| e.to_string())?;
        let mut out = [0u8; 32];
        if let Some(v) = v {
            out.copy_from_slice(&v[..32]);
        }
        Ok(out)
    }

    fn block_hash(&mut self, height: u64) -> std::result::Result<[u8; 32], String> {
        let h = self.chain.main_at(height).map_err(|e| e.to_string())?;
        match h {
            Some(h) => Ok(self.chain.info(&h).map_err(|e| e.to_string())?.map(|i| i.hash.0).unwrap_or([0u8; 32])),
            None => Ok([0u8; 32]),
        }
    }
}
