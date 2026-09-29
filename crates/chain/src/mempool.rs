//! Pool of pending Nova transactions (compact native transfers and EVM
//! transactions) waiting to be packed into batch blocks.
//!
//! Legacy 512-byte transaction blocks do not live here: they are DAG blocks
//! and wait in the no-ref set until a link or main block references them.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use xdag_types::nova::NovaTx;
use xdag_types::Address;

use crate::preverify::VerifiedTx;

#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub tx: VerifiedTx,
    pub raw: NovaTx,
    /// Price per unit used for ordering (wei per gas for EVM, nano fee for native).
    pub priority: u128,
    pub added: Instant,
    /// Already packed into a batch block that is not executed yet. Later
    /// nonces of the same sender may be packed right after it.
    pub included: Option<Instant>,
}

/// How long an included transaction may wait for execution before it is
/// offered again (its batch block may have been orphaned).
pub const INCLUDED_RETRY: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("transaction already known")]
    Known,
    #[error("nonce too low (next expected {0})")]
    NonceTooLow(u64),
    #[error("nonce too far in the future")]
    NonceGap,
    #[error("insufficient balance for value + fee")]
    Insufficient,
    #[error("pool is full")]
    Full,
    #[error("replacement underpriced")]
    Underpriced,
}

pub struct TxPool {
    by_hash: HashMap<[u8; 32], PoolEntry>,
    by_sender: HashMap<Address, BTreeMap<u64, [u8; 32]>>,
    pub max_size: usize,
    pub max_per_sender: usize,
    pub max_nonce_gap: u64,
    pub ttl: Duration,
}

/// Nonce the next transaction of a kind must carry, given the executed count.
pub fn next_nonce(tx: &VerifiedTx, executed: u64) -> u64 {
    match tx {
        VerifiedTx::Native { .. } => executed + 1,
        VerifiedTx::Evm { .. } => executed,
    }
}

fn tx_nonce(tx: &VerifiedTx) -> u64 {
    match tx {
        VerifiedTx::Native { tx, .. } => tx.nonce,
        VerifiedTx::Evm { info, .. } => info.nonce,
    }
}

fn priority(tx: &VerifiedTx) -> u128 {
    match tx {
        VerifiedTx::Native { tx, .. } => tx.fee.0 as u128,
        VerifiedTx::Evm { info, .. } => info.max_gas_price,
    }
}

/// Upper bound of what the sender pays, in wei.
pub fn max_cost_wei(tx: &VerifiedTx) -> u128 {
    match tx {
        VerifiedTx::Native { tx, .. } => (tx.amount.0 as u128 + tx.fee.0 as u128) * xdag_types::amount::WEI_PER_NANO,
        VerifiedTx::Evm { info, .. } => info.value + info.gas_limit as u128 * info.max_gas_price,
    }
}

impl Default for TxPool {
    fn default() -> Self {
        TxPool {
            by_hash: HashMap::new(),
            by_sender: HashMap::new(),
            max_size: 200_000,
            max_per_sender: 1024,
            max_nonce_gap: 1024,
            ttl: Duration::from_secs(3 * 3600),
        }
    }
}

impl TxPool {
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    pub fn contains(&self, h: &[u8; 32]) -> bool {
        self.by_hash.contains_key(h)
    }

    pub fn get(&self, h: &[u8; 32]) -> Option<&PoolEntry> {
        self.by_hash.get(h)
    }

    /// Admit a verified transaction. `executed` is the sender's executed
    /// transaction count and `balance_wei` its balance in state.
    pub fn add(&mut self, tx: VerifiedTx, raw: NovaTx, executed: u64, balance_wei: u128) -> Result<(), PoolError> {
        let hash = tx.hash();
        if self.by_hash.contains_key(&hash) {
            return Err(PoolError::Known);
        }
        let sender = tx.sender();
        let nonce = tx_nonce(&tx);
        let first = next_nonce(&tx, executed);
        if nonce < first {
            return Err(PoolError::NonceTooLow(first));
        }
        if nonce > first + self.max_nonce_gap {
            return Err(PoolError::NonceGap);
        }
        // cumulative cost of this sender's queued transactions must be covered
        let queued: u128 =
            self.by_sender.get(&sender).map(|m| m.values().filter_map(|h| self.by_hash.get(h)).map(|e| max_cost_wei(&e.tx)).sum()).unwrap_or(0);
        if queued + max_cost_wei(&tx) > balance_wei {
            return Err(PoolError::Insufficient);
        }
        if let Some(existing) = self.by_sender.get(&sender).and_then(|m| m.get(&nonce)).copied() {
            // replacement: require a 10% higher priority
            let old = self.by_hash.get(&existing).map(|e| e.priority).unwrap_or(0);
            if priority(&tx) * 10 < old * 11 {
                return Err(PoolError::Underpriced);
            }
            self.remove(&existing);
        }
        if self.by_hash.len() >= self.max_size {
            return Err(PoolError::Full);
        }
        if self.by_sender.get(&sender).map(|m| m.len()).unwrap_or(0) >= self.max_per_sender {
            return Err(PoolError::Full);
        }
        let entry = PoolEntry { priority: priority(&tx), tx, raw, added: Instant::now(), included: None };
        self.by_sender.entry(sender).or_default().insert(nonce, hash);
        self.by_hash.insert(hash, entry);
        Ok(())
    }

    pub fn remove(&mut self, h: &[u8; 32]) -> Option<PoolEntry> {
        let e = self.by_hash.remove(h)?;
        let sender = e.tx.sender();
        if let Some(m) = self.by_sender.get_mut(&sender) {
            m.retain(|_, v| v != h);
            if m.is_empty() {
                self.by_sender.remove(&sender);
            }
        }
        Some(e)
    }

    /// Drop transactions whose nonce is below the executed count, and old ones.
    pub fn prune(&mut self, mut executed: impl FnMut(&Address) -> u64) {
        let now = Instant::now();
        let mut drop = vec![];
        for (sender, m) in &self.by_sender {
            let ex = executed(sender);
            for (nonce, h) in m {
                if let Some(e) = self.by_hash.get(h) {
                    if *nonce < next_nonce(&e.tx, ex) || now.duration_since(e.added) > self.ttl {
                        drop.push(*h);
                    }
                }
            }
        }
        for h in drop {
            self.remove(&h);
        }
    }

    /// Mark transactions as packed into a batch block.
    pub fn mark_included(&mut self, hashes: &[[u8; 32]]) {
        let now = Instant::now();
        for h in hashes {
            if let Some(e) = self.by_hash.get_mut(h) {
                e.included = Some(now);
            }
        }
    }

    /// (hash, sender, nonce, in flight) of every queued transaction.
    pub fn list(&self) -> Vec<([u8; 32], Address, u64, bool)> {
        let mut v: Vec<_> = self.by_hash.iter().map(|(h, e)| (*h, e.tx.sender(), tx_nonce(&e.tx), e.included.is_some())).collect();
        v.sort_by_key(|x| (x.1, x.2));
        v
    }

    /// Highest queued nonce of a sender (for "pending nonce" RPC queries).
    pub fn highest_nonce(&self, a: &Address) -> Option<u64> {
        self.by_sender.get(a).and_then(|m| m.keys().next_back().copied())
    }

    /// Pick executable transactions: per sender a gap-free nonce run starting at
    /// the next expected nonce; senders interleaved by priority.
    pub fn select(&self, max_txs: usize, max_bytes: usize, max_gas: u64, mut executed: impl FnMut(&Address) -> u64) -> Vec<(NovaTx, [u8; 32])> {
        // candidate queues
        let now = Instant::now();
        let mut queues: Vec<Vec<&PoolEntry>> = vec![];
        for (sender, m) in &self.by_sender {
            let ex = executed(sender);
            let mut q = vec![];
            let mut expect: Option<u64> = None;
            for (nonce, h) in m {
                let Some(e) = self.by_hash.get(h) else { continue };
                let want = expect.unwrap_or_else(|| next_nonce(&e.tx, ex));
                if *nonce < want {
                    continue;
                }
                if *nonce != want {
                    break;
                }
                expect = Some(want + 1);
                match e.included {
                    Some(t) if now.duration_since(t) < INCLUDED_RETRY => continue, // in flight
                    _ => q.push(e),
                }
            }
            if !q.is_empty() {
                q.reverse(); // pop from the end = lowest nonce first
                queues.push(q);
            }
        }
        let mut out = vec![];
        let mut bytes = 8usize;
        let mut gas = 0u64;
        loop {
            // pick the queue whose head has the highest priority
            let best = queues.iter().enumerate().filter(|(_, q)| !q.is_empty()).max_by_key(|(_, q)| q.last().unwrap().priority).map(|(i, _)| i);
            let Some(i) = best else { break };
            let e = queues[i].pop().unwrap();
            let size = 5 + e.raw.encode_inner().len();
            let g = match &e.tx {
                VerifiedTx::Evm { info, .. } => info.gas_limit,
                _ => 0,
            };
            if out.len() >= max_txs || bytes + size > max_bytes {
                break;
            }
            if gas + g > max_gas {
                queues[i].clear(); // later nonces of this sender cannot go either
                continue;
            }
            bytes += size;
            gas += g;
            out.push((e.raw.clone(), e.tx.hash()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdag_types::nova::NativeTransfer;
    use xdag_types::{KeyPair, Nano};

    fn native(k: &KeyPair, nonce: u64, fee: u64) -> (VerifiedTx, NovaTx) {
        let t = NativeTransfer::new_signed(k, 1, nonce, Address::ZERO, Nano(10), Nano(fee), b"").unwrap();
        (VerifiedTx::Native { hash: t.tx_hash(), sender: k.address(), tx: t.clone() }, NovaTx::Native(t))
    }

    #[test]
    fn selects_gap_free_runs_by_priority() {
        let a = KeyPair::random();
        let b = KeyPair::random();
        let mut pool = TxPool::default();
        for n in 1..=3 {
            let (v, r) = native(&a, n, 5);
            pool.add(v, r, 0, u128::MAX).unwrap();
        }
        let (v, r) = native(&b, 2, 100); // gap: b's next is 1
        pool.add(v, r, 0, u128::MAX).unwrap();
        let (v, r) = native(&b, 1, 50);
        pool.add(v, r, 0, u128::MAX).unwrap();
        let sel = pool.select(10, 1 << 20, u64::MAX, |_| 0);
        assert_eq!(sel.len(), 5);
        // b's run first (priority 50 > 5), then b's 2 (100), then a's
        let senders: Vec<u64> = sel
            .iter()
            .map(|(t, _)| match t {
                NovaTx::Native(n) => n.fee.0,
                _ => 0,
            })
            .collect();
        // in-flight transactions are skipped but do not block later nonces
        let a_hashes: Vec<[u8; 32]> = sel.iter().filter(|(t, _)| matches!(t, NovaTx::Native(n) if n.fee.0 == 5)).map(|(_, h)| *h).take(1).collect();
        pool.mark_included(&a_hashes);
        let again = pool.select(10, 1 << 20, u64::MAX, |_| 0);
        assert_eq!(again.len(), 4);
        assert_eq!(senders, vec![50, 100, 5, 5, 5]);
        assert_eq!(pool.add(native(&a, 1, 6).0, native(&a, 1, 6).1, 1, u128::MAX), Err(PoolError::NonceTooLow(2)));
        pool.prune(|x| if *x == a.address() { 2 } else { 0 });
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn balance_and_replacement_rules() {
        let a = KeyPair::random();
        let mut pool = TxPool::default();
        let (v, r) = native(&a, 1, 100);
        assert_eq!(pool.add(v.clone(), r.clone(), 0, 10), Err(PoolError::Insufficient));
        pool.add(v, r, 0, u128::MAX).unwrap();
        let (v2, r2) = native(&a, 1, 100);
        // same content → same hash → known; a different fee is a replacement
        assert_eq!(pool.add(v2, r2, 0, u128::MAX), Err(PoolError::Known));
        let (v3, r3) = native(&a, 1, 105);
        assert_eq!(pool.add(v3, r3, 0, u128::MAX), Err(PoolError::Underpriced));
        let (v4, r4) = native(&a, 1, 120);
        pool.add(v4, r4, 0, u128::MAX).unwrap();
        assert_eq!(pool.len(), 1);
    }
}
