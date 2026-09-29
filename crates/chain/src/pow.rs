//! Proof of work: difficulty of a block and the RandomX seed schedule.
//!
//! Candidate main blocks (time = last tick of an epoch) after the RandomX fork
//! are scored by `RandomX(key, sha256(raw[0..480]) ‖ raw[480..512])`; all other
//! blocks by their sha256d hash. Blocks with inputs count as difficulty 1
//! (xdagj rule). The RandomX key changes every `seed_epoch_blocks` main blocks
//! and is derived from the main chain exactly like xdagj's `RandomX` class.

use xdag_storage::{Reader, Writer};
use xdag_types::difficulty::hash_difficulty;
use xdag_types::hash::sha256;
use xdag_types::{Block, HashLow, NetworkParams};

/// Pluggable RandomX implementation (the real one lives in `xdag-randomx`).
pub trait PowEngine: Send + Sync {
    /// RandomX hash of `input` under `key`; `None` if RandomX is unavailable.
    fn randomx(&self, key: &[u8; 32], input: &[u8; 64]) -> Option<[u8; 32]>;
    /// Hint that `key` will be needed soon (pre-initialise caches).
    fn prepare(&self, _key: &[u8; 32]) {}
}

/// PoW engine without RandomX: only usable before the RandomX fork (devnet
/// and tests). Candidate blocks after the fork fall back to sha256d scoring,
/// which is what xdagj does when no seed is loaded.
pub struct NoRandomX;

impl PowEngine for NoRandomX {
    fn randomx(&self, _key: &[u8; 32], _input: &[u8; 64]) -> Option<[u8; 32]> {
        None
    }
}

/// The 64-byte RandomX input of a candidate block.
pub fn randomx_input(raw: &[u8; 512]) -> [u8; 64] {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(&sha256(&raw[..480]));
    data[32..].copy_from_slice(&raw[480..]);
    data
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    pub height: u64,
    /// First epoch that uses this seed.
    pub switch_epoch: u64,
    pub key: [u8; 32],
}

/// RandomX fork state derived from the main chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RxSchedule {
    /// Epoch after which candidate blocks use RandomX (None = not forked).
    pub fork_epoch: Option<u64>,
    /// Seeds in height order. Only the last two matter for new blocks.
    pub seeds: Vec<Seed>,
}

impl RxSchedule {
    pub fn is_fork(&self, epoch: u64) -> bool {
        matches!(self.fork_epoch, Some(f) if epoch > f)
    }

    /// Key for a block of `epoch` (xdagj `randomXBlockHash` memory selection).
    pub fn key_for_epoch(&self, epoch: u64) -> Option<[u8; 32]> {
        let latest = self.seeds.last()?;
        if epoch >= latest.switch_epoch {
            return Some(latest.key);
        }
        if self.seeds.len() >= 2 {
            return Some(self.seeds[self.seeds.len() - 2].key);
        }
        None
    }

    /// Height whose main block provides the seed introduced at `height`, if any.
    pub fn seed_source_height(p: &NetworkParams, height: u64) -> Option<u64> {
        let rx = &p.randomx;
        (height >= rx.fork_height && height & (rx.seed_epoch_blocks - 1) == 0).then(|| height - rx.seed_lag)
    }

    /// Called when a main block at `height` (with time `time`) is set.
    /// `seed_source` is the main block at `seed_source_height(height)`.
    pub fn on_set_main(&mut self, p: &NetworkParams, height: u64, time: u64, seed_source: Option<HashLow>) {
        let rx = &p.randomx;
        if height < rx.fork_height {
            return;
        }
        let epoch = p.epochs().epoch(time);
        if height == rx.fork_height {
            self.fork_epoch = Some(epoch + rx.seed_lag);
        }
        if height & (rx.seed_epoch_blocks - 1) == 0 {
            if let Some(h) = seed_source {
                let mut key = [0u8; 32];
                key[..24].copy_from_slice(&h.0);
                self.seeds.retain(|s| s.height < height);
                self.seeds.push(Seed { height, switch_epoch: epoch + rx.seed_lag + 1, key });
                // keep a bounded history (old seeds are only needed for deep reorgs)
                if self.seeds.len() > 8 {
                    self.seeds.remove(0);
                }
            }
        }
    }

    pub fn on_unset_main(&mut self, p: &NetworkParams, height: u64) {
        let rx = &p.randomx;
        if height < rx.fork_height {
            return;
        }
        if height == rx.fork_height {
            // xdagj sets the fork time to -1 here (every epoch counts as forked);
            // reverting to "not forked" is the consistent choice.
            self.fork_epoch = None;
        }
        if height & (rx.seed_epoch_blocks - 1) == 0 {
            self.seeds.retain(|s| s.height != height);
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(1);
        match self.fork_epoch {
            Some(e) => {
                w.u8(1).u64(e);
            }
            None => {
                w.u8(0);
            }
        }
        w.u32(self.seeds.len() as u32);
        for s in &self.seeds {
            w.u64(s.height).u64(s.switch_epoch).fixed(&s.key);
        }
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let mut r = Reader::new(b);
        if r.u8().ok()? != 1 {
            return None;
        }
        let fork_epoch = if r.u8().ok()? == 1 { Some(r.u64().ok()?) } else { None };
        let n = r.u32().ok()? as usize;
        let mut seeds = Vec::with_capacity(n);
        for _ in 0..n {
            seeds.push(Seed { height: r.u64().ok()?, switch_epoch: r.u64().ok()?, key: r.fixed::<32>().ok()? });
        }
        Some(RxSchedule { fork_epoch, seeds })
    }
}

/// xdagj `calculateCurrentBlockDiff` for a new block.
///
/// Legacy rules score *every* input-less block by its sha256d hash, even after
/// the RandomX fork where only end-of-epoch candidates are RandomX-mined. A
/// mid-epoch link block whose hash was ground with GPUs/ASICs can therefore
/// outweigh all RandomX candidates and take the epoch's main block (and its
/// reward). Only the node whitelist kept outsiders from exploiting this.
/// Under Nova rules only main-block candidates carry difficulty.
pub fn own_difficulty(block: &Block, p: &NetworkParams, rx: &RxSchedule, engine: &dyn PowEngine) -> u128 {
    let ep = p.epochs();
    let candidate = ep.is_end_of_epoch(block.time) && block.nonce.is_some() && block.inputs.is_empty();
    if p.is_nova_time(block.time) && !candidate {
        return 0;
    }
    if !block.inputs.is_empty() {
        return 1;
    }
    let epoch = ep.epoch(block.time);
    if rx.is_fork(epoch) && ep.is_end_of_epoch(block.time) {
        if let Some(key) = rx.key_for_epoch(epoch) {
            if let Some(h) = engine.randomx(&key, &randomx_input(block.raw())) {
                return hash_difficulty(&h);
            }
        }
    }
    hash_difficulty(&block.hash().0)
}

/// Leading zero bits of the 96-bit difficulty word (Nova anti-spam PoW).
pub fn pow_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut v: u128 = 0;
    for i in (20..32).rev() {
        v = (v << 8) | hash[i] as u128;
    }
    v.leading_zeros().saturating_sub(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_follows_xdagj() {
        let mut p = NetworkParams::devnet();
        p.randomx.fork_height = 8;
        p.randomx.seed_epoch_blocks = 4;
        p.randomx.seed_lag = 2;
        let mut s = RxSchedule::default();
        let hl = |h: u64| RxSchedule::seed_source_height(&p, h).map(|x| HashLow([x as u8; 24]));
        s.on_set_main(&p, 7, 7 << 16, hl(7));
        assert!(!s.is_fork(100));
        s.on_set_main(&p, 8, 8 << 16, hl(8));
        assert_eq!(s.fork_epoch, Some(10));
        assert!(s.is_fork(11) && !s.is_fork(10));
        assert_eq!(s.seeds.len(), 1);
        assert_eq!(s.seeds[0].switch_epoch, 11);
        assert_eq!(s.key_for_epoch(10), None);
        assert_eq!(s.key_for_epoch(11).unwrap()[0], 6);
        s.on_set_main(&p, 12, 12 << 16, hl(12));
        assert_eq!(s.key_for_epoch(14).unwrap()[0], 6); // before switch → previous
        assert_eq!(s.key_for_epoch(15).unwrap()[0], 10);
        s.on_unset_main(&p, 12);
        assert_eq!(s.seeds.len(), 1);
        let enc = s.encode();
        assert_eq!(RxSchedule::decode(&enc).unwrap(), s);
    }

    #[test]
    fn zero_bits() {
        let mut h = [0u8; 32];
        h[31] = 0x80;
        assert_eq!(pow_zero_bits(&h), 0);
        h[31] = 0x00;
        h[30] = 0x01;
        assert_eq!(pow_zero_bits(&h), 15);
    }
}
