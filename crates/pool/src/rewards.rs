//! Reward distribution for pool-mined main blocks (xdagj `PoolAwardManager`).
//!
//! 16 epochs after one of our candidates became a main block, its balance is
//! split: `fund%` to the community fund, `node%` kept by the node (paid to the
//! node address in batches of 10 blocks) and the rest to the pool address
//! embedded in the winning nonce (last 20 bytes).

use std::collections::VecDeque;

use xdag_types::block::BlockTemplate;
use xdag_types::{Address, Block, CAmount, FieldType, HashLow, KeyPair, LinkTarget, Nano, NetworkParams};

#[derive(Clone, Debug)]
pub struct RewardPolicy {
    pub fund: Address,
    /// Percent of the block balance for the fund (xdagj default 5).
    pub fund_percent: f64,
    /// Percent kept by the node (xdagj default 5).
    pub node_percent: f64,
    pub delay_epochs: u64,
    pub node_tag: String,
}

#[derive(Clone, Debug)]
pub struct MinedBlock {
    pub epoch: u64,
    pub block: HashLow,
    pub pool: Address,
    /// Mined by the node itself (nonce address = coinbase): the whole balance
    /// is swept to the node address. xdagj skipped these blocks, leaving solo
    /// rewards stranded in block balances.
    pub solo: bool,
}

#[derive(Default)]
pub struct RewardManager {
    pub pending: VecDeque<MinedBlock>,
    /// (block, node share) waiting to be paid in one batch.
    pub node_batch: Vec<(HashLow, Nano)>,
}

fn pct(amount: Nano, percent: f64) -> Nano {
    // exact integer arithmetic with 1e-6 precision on the percentage
    let ppm = (percent * 10_000.0).round() as u128; // parts per million of 100%
    Nano(((amount.0 as u128) * ppm / 1_000_000) as u64)
}

impl RewardManager {
    /// Remember a candidate we produced whose nonce came from a pool.
    pub fn record(&mut self, b: &Block, epoch: u64) {
        let (Some(pool), Some(cb)) = (b.nonce_address(), b.coinbase) else { return };
        self.pending.push_back(MinedBlock { epoch, block: b.hashlow(), pool, solo: pool == cb });
    }

    pub fn due(&mut self, now_epoch: u64, delay: u64) -> Vec<MinedBlock> {
        let mut out = vec![];
        while let Some(m) = self.pending.front() {
            if m.epoch + delay <= now_epoch {
                out.push(self.pending.pop_front().unwrap());
            } else {
                break;
            }
        }
        out
    }
}

/// Build the fund + pool payment for one main block (balance `balance`).
/// Returns the transaction and the node's share, or None if the policy
/// cannot be satisfied (same conditions as xdagj).
pub fn payout_block(p: &NetworkParams, policy: &RewardPolicy, key: &KeyPair, mined: &MinedBlock, balance: Nano, time: u64) -> Option<(Block, Nano)> {
    let min = p.min_gas;
    let fund = pct(balance, policy.fund_percent);
    let node = pct(balance, policy.node_percent);
    let pool = balance.checked_sub(fund)?.checked_sub(node)?;
    let send = balance.checked_sub(node)?;
    if policy.fund_percent + policy.node_percent >= 100.0 || fund < min || pool < min || send.0 < 2 * min.0 {
        return None;
    }
    let mut t = BlockTemplate::new(p.header_field(), time);
    t.header_fee = min.0; // xdagj passes MIN_GAS as the extra fee of payouts
    t.links.push((FieldType::In, LinkTarget::Block(mined.block), send.to_camount_legacy()));
    t.links.push((FieldType::Output, LinkTarget::Address(policy.fund), fund.to_camount_legacy()));
    t.links.push((FieldType::Output, LinkTarget::Address(mined.pool), pool.to_camount_legacy()));
    t.sign_out = Some(key.clone());
    t.include_out_pubkey = true;
    let b = xdag_chain::builder::seal_template(p, t).ok()?;
    Some((b, node))
}

/// Sweep a solo-mined block balance to the node address.
pub fn sweep_solo(p: &NetworkParams, key: &KeyPair, mined: &MinedBlock, balance: Nano, time: u64) -> Option<Block> {
    if balance.0 < 2 * p.min_gas.0 {
        return None;
    }
    let mut t = BlockTemplate::new(p.header_field(), time);
    t.links.push((FieldType::In, LinkTarget::Block(mined.block), balance.to_camount_legacy()));
    t.links.push((FieldType::Output, LinkTarget::Address(key.address()), balance.to_camount_legacy()));
    t.sign_out = Some(key.clone());
    t.include_out_pubkey = true;
    xdag_chain::builder::seal_template(p, t).ok()
}

/// Pay accumulated node shares (up to 10 input blocks per transaction).
pub fn payout_node(p: &NetworkParams, policy: &RewardPolicy, key: &KeyPair, batch: &[(HashLow, Nano)], time: u64) -> Option<Block> {
    if batch.is_empty() {
        return None;
    }
    let total: u64 = batch.iter().map(|(_, n)| n.0).sum();
    let mut t = BlockTemplate::new(p.header_field(), time);
    for (h, n) in batch.iter().take(10) {
        t.links.push((FieldType::In, LinkTarget::Block(*h), n.to_camount_legacy()));
    }
    t.links.push((FieldType::Output, LinkTarget::Address(key.address()), Nano(total).to_camount_legacy()));
    let mut remark = [0u8; 32];
    let r = format!("Pay to {}", policy.node_tag);
    let n = r.len().min(32);
    remark[..n].copy_from_slice(&r.as_bytes()[..n]);
    t.remark = Some(remark);
    t.sign_out = Some(key.clone());
    t.include_out_pubkey = true;
    xdag_chain::builder::seal_template(p, t).ok()
}

#[allow(dead_code)]
fn _c(_: CAmount) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_are_exact() {
        assert_eq!(pct(Nano::from_xdag(64), 5.0), Nano(3_200_000_000));
        assert_eq!(pct(Nano(1_000_000_007), 5.0), Nano(50_000_000));
    }
}
