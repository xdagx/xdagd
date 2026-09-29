//! Block production: main-block candidates, link blocks and Nova batch blocks
//! (xdagj `createMainBlock` / `createLinkBlock`, plus batches).

use std::sync::Arc;

use rand::RngCore;
use xdag_types::block::{BlockTemplate, MAX_LINKS};
use xdag_types::nova::{encode_payload, payload_root, NovaTx};
use xdag_types::{Block, CAmount, FieldType, HashLow, KeyPair, LinkTarget};

use crate::chain::Chain;
use crate::fees;
use crate::pow::pow_zero_bits;
use crate::records::flags;
use crate::Result;

#[derive(Clone, Debug)]
struct Orphan {
    h: HashLow,
    time: u64,
    /// ordering key: account/main transactions by fee first, links last
    rank: (u8, std::cmp::Reverse<u64>, u64),
}

impl Chain {
    /// Best block of an epoch earlier than `epoch` (xdagj "pretop").
    pub fn pretop_for(&mut self, epoch: u64) -> Result<Option<HashLow>> {
        let ep = self.params.epochs();
        let mut cur = self.meta.top;
        while let Some(h) = cur {
            let Some(info) = self.info(&h)? else { return Ok(None) };
            if ep.epoch(info.time) < epoch {
                return Ok(Some(h));
            }
            cur = info.max_diff_link;
        }
        Ok(None)
    }

    /// Unreferenced blocks usable as links of a block with time `cutoff`,
    /// transactions first (highest fee, lowest nonce), then plain blocks.
    fn orphans(&mut self, cutoff: u64, max: usize) -> Result<Vec<HashLow>> {
        let hs = crate::query::noref_blocks(self.db(), 100_000)?;
        let mut v = vec![];
        for h in hs {
            if self.extra.contains_key(&h) {
                continue;
            }
            let Some(info) = self.info(&h)? else { continue };
            if info.time >= cutoff || info.flags & flags::REF != 0 {
                continue;
            }
            let Some(b) = self.block(&h)? else { continue };
            let rank = if b.is_tx() {
                let fee = fees::tx_fee(&b, &self.params).map(|f| f.0).unwrap_or(0);
                (0u8, std::cmp::Reverse(fee), b.tx_nonce.unwrap_or(0))
            } else {
                (1u8, std::cmp::Reverse(0), info.time)
            };
            v.push(Orphan { h, time: info.time, rank });
        }
        v.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.time.cmp(&b.time)).then(a.h.cmp(&b.h)));
        // keep per-account nonce order: a later nonce never precedes an earlier one
        // (sorting by (fee desc, nonce asc) already achieves this for equal fees;
        // unequal fees across nonces of one sender are rare and only delay execution)
        Ok(v.into_iter().take(max).map(|o| o.h).collect())
    }

    /// Candidate main block for the current epoch; the caller installs the
    /// best nonce found by miners (field 15) before importing it.
    pub fn main_candidate(&mut self, key: &KeyPair, remark: &[u8], now: u64) -> Result<Block> {
        let ep = self.params.epochs();
        let time = ep.end_of_epoch(now);
        let mut t = BlockTemplate::new(self.params.header_field(), time);
        if let Some(pre) = self.pretop_for(ep.epoch(time))? {
            t.links.push((FieldType::Out, LinkTarget::Block(pre), CAmount::ZERO));
        }
        t.links.push((FieldType::Coinbase, LinkTarget::Address(key.address()), CAmount::ZERO));
        let has_remark = !remark.is_empty();
        let used = 1 + t.links.len() + has_remark as usize + 2;
        for h in self.orphans(time, MAX_LINKS.saturating_sub(used))? {
            t.links.push((FieldType::Out, LinkTarget::Block(h), CAmount::ZERO));
        }
        if has_remark {
            let mut r = [0u8; 32];
            let n = remark.len().min(32);
            r[..n].copy_from_slice(&remark[..n]);
            t.remark = Some(r);
        }
        t.sign_out = Some(key.clone());
        let mut nonce = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut nonce[..12]);
        nonce[12..].copy_from_slice(&key.address().0);
        t.mining_nonce = Some(nonce);
        t.build().map_err(|e| crate::ChainError::Other(e.to_string()))
    }

    /// Link block referencing unreferenced blocks (xdagj `createLinkBlock`):
    /// timestamped just after its newest link so it stays in that epoch.
    pub fn link_block(&mut self, key: &KeyPair, now: u64) -> Result<Option<Block>> {
        let nova = self.params.is_nova_time(now);
        let max = MAX_LINKS - 2 - usize::from(nova);
        let links = self.orphans(now, max)?;
        if links.is_empty() {
            return Ok(None);
        }
        let mut newest = 0;
        for h in &links {
            if let Some(i) = self.info(h)? {
                newest = newest.max(i.time);
            }
        }
        let time = (newest + 1).min(now);
        let ep = self.params.epochs();
        let time = if ep.is_end_of_epoch(time) { time - 1 } else { time };
        let mut t = BlockTemplate::new(self.params.header_field(), time);
        for h in links {
            t.links.push((FieldType::Out, LinkTarget::Block(h), CAmount::ZERO));
        }
        t.sign_out = Some(key.clone());
        self.seal(t).map(Some)
    }

    /// Nova batch block carrying `txs` in its payload.
    pub fn batch_block(&mut self, key: &KeyPair, txs: &[NovaTx], now: u64) -> Result<(Block, Arc<Vec<u8>>)> {
        let payload = Arc::new(encode_payload(txs));
        let ep = self.params.epochs();
        let time = if ep.is_end_of_epoch(now) { now - 1 } else { now };
        let mut t = BlockTemplate::new(self.params.header_field(), time);
        // also help the DAG along: reference a few orphans (and keep it connected)
        let mut links = self.orphans(time, 8)?;
        if links.is_empty() {
            if let Some(top) = self.meta.top {
                if self.info(&top)?.map(|i| i.time < time).unwrap_or(false) {
                    links.push(top);
                }
            }
        }
        for h in links {
            t.links.push((FieldType::Out, LinkTarget::Block(h), CAmount::ZERO));
        }
        t.ext_root = Some(payload_root(&payload));
        t.sign_out = Some(key.clone());
        let b = self.seal(t)?;
        Ok((b, payload))
    }

    /// Sign and, under Nova, grind the field-15 nonce until the block meets
    /// the anti-spam proof of work.
    pub fn seal(&self, t: BlockTemplate) -> Result<Block> {
        seal_template(&self.params, t)
    }
}

/// See [`Chain::seal`]. Also used by wallets building transaction blocks.
pub fn seal_template(p: &xdag_types::NetworkParams, mut t: BlockTemplate) -> Result<Block> {
    let nova_bits = match &p.nova {
        Some(n) if p.is_nova_time(t.time) => Some(n.min_link_pow_bits),
        _ => None,
    };
    let Some(bits) = nova_bits else {
        return t.build().map_err(|e| crate::ChainError::Other(e.to_string()));
    };
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    t.mining_nonce = Some(nonce);
    let signed = t.build().map_err(|e| crate::ChainError::Other(e.to_string()))?;
    // the nonce is excluded from every signature digest: grind without re-signing
    let mut raw = *signed.raw();
    let mut ctr: u64 = u64::from_le_bytes(nonce[..8].try_into().unwrap());
    loop {
        let h = xdag_types::hash::sha256d(&raw);
        if pow_zero_bits(&h) >= bits {
            return Block::parse(&raw).map_err(|e| crate::ChainError::Other(e.to_string()));
        }
        ctr = ctr.wrapping_add(1);
        raw[MAX_LINKS * 32..MAX_LINKS * 32 + 8].copy_from_slice(&ctr.to_le_bytes());
    }
}
