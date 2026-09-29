//! Block production: per-epoch main-block candidates (mined by pools or the
//! built-in miner), Nova batch blocks for pending transactions, link blocks
//! for unreferenced blocks, and pool reward payouts.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xdag_chain::query;
use xdag_pool::rewards::{payout_block, payout_node, sweep_solo, RewardManager, RewardPolicy};
use xdag_pool::{Coordinator, PowKind};
use xdag_types::{Address, HashLow, Nano};

use crate::node::Node;

pub fn reward_policy(node: &Node) -> RewardPolicy {
    RewardPolicy {
        fund: Address::parse(&node.params.fund_address).unwrap_or(Address::ZERO),
        fund_percent: node.cfg.mining.fund_percent,
        node_percent: node.cfg.mining.node_percent,
        delay_epochs: 16,
        node_tag: node.cfg.node_tag.clone(),
    }
}

fn new_task(node: &Node, coord: &Coordinator, epoch: u64) -> Option<HashLow> {
    let now = xdag_types::time::now_xdag();
    let (cand, pow, pretop) = {
        let mut chain = node.chain.lock();
        let cand = match chain.main_candidate(&node.key, node.cfg.node_tag.as_bytes(), now) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("cannot build candidate: {e}");
                return None;
            }
        };
        let rx = chain.rx_schedule();
        let pow = match (rx.is_fork(epoch), rx.key_for_epoch(epoch), &node.rx_engine) {
            (true, Some(key), Some(_)) => PowKind::RandomX { key },
            _ => PowKind::Sha256d,
        };
        let pretop = chain.pretop_for(epoch).ok().flatten();
        (cand, pow, pretop)
    };
    coord.new_task(&cand, epoch, pow);
    pretop
}

pub fn run(node: Arc<Node>, coord: Arc<Coordinator>) {
    let ep = node.params.epochs();
    let mut cur_epoch: Option<u64> = None;
    let mut cur_pretop: Option<HashLow> = None;
    let mut last_tick = Instant::now();
    let mut last_batch = Instant::now();
    let mut last_link = Instant::now();
    let mut rewards = RewardManager::default();
    let policy = reward_policy(&node);
    let batch_interval = Duration::from_millis(node.cfg.mining.batch_interval_ms.max(100));
    loop {
        if node.shutdown.load(Ordering::SeqCst) {
            break;
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            let confirmed = node.chain.lock().tick();
            if let Err(e) = confirmed {
                tracing::error!("chain tick failed: {e}");
            }
            let db = node.db.clone();
            node.txpool.lock().prune(|a| query::account(&db, a).ok().flatten().map(|r| r.nonce).unwrap_or(0));
        }
        if !node.cfg.mining.generate_blocks || !node.is_synced() {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let now = xdag_types::time::now_xdag();
        let epoch = ep.epoch(now);
        if Some(epoch) != cur_epoch {
            if let (Some(b), Some(prev)) = (coord.finish(), cur_epoch) {
                let out = node.import_local(b.raw().to_vec(), None);
                if out.is_imported() {
                    tracing::debug!(block = %b.hashlow(), "candidate submitted");
                    rewards.record(&b, prev);
                } else {
                    tracing::debug!(?out, "candidate not imported");
                }
            }
            pay_rewards(&node, &policy, &mut rewards, epoch);
            cur_pretop = new_task(&node, &coord, epoch);
            cur_epoch = Some(epoch);
        } else {
            // a better block of an earlier epoch appeared: rebuild on top of it
            let pretop = node.chain.lock().pretop_for(epoch).ok().flatten();
            if pretop != cur_pretop {
                cur_pretop = new_task(&node, &coord, epoch);
            }
        }

        if node.params.is_nova_epoch(epoch) && last_batch.elapsed() >= batch_interval {
            last_batch = Instant::now();
            produce_batch(&node);
        }
        if last_link.elapsed() >= Duration::from_secs(2) {
            last_link = Instant::now();
            let norefs = query::noref_blocks(&node.db, 64).map(|v| v.len()).unwrap_or(0);
            if norefs >= 8 {
                let lb = node.chain.lock().link_block(&node.key, xdag_types::time::now_xdag());
                if let Ok(Some(b)) = lb {
                    node.import_local(b.raw().to_vec(), None);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pack pending Nova transactions into batch blocks.
pub fn produce_batch(node: &Node) -> usize {
    let Some(nova) = node.params.nova.clone() else { return 0 };
    let db = node.db.clone();
    let mut produced = 0;
    loop {
        let selected = node.txpool.lock().select(nova.max_payload_txs, nova.max_payload_bytes, nova.batch_gas_limit, |a| {
            query::account(&db, a).ok().flatten().map(|r| r.nonce).unwrap_or(0)
        });
        if selected.is_empty() {
            break;
        }
        let (txs, hashes): (Vec<_>, Vec<_>) = selected.into_iter().unzip();
        let n = txs.len();
        let built = node.chain.lock().batch_block(&node.key, &txs, xdag_types::time::now_xdag());
        match built {
            Ok((b, payload)) => {
                let out = node.import_local(b.raw().to_vec(), Some(payload.to_vec()));
                if !out.is_imported() {
                    tracing::warn!(?out, "batch block rejected");
                    break;
                }
                // in flight: later nonces of the same senders can follow immediately
                node.txpool.lock().mark_included(&hashes);
                produced += n;
            }
            Err(e) => {
                tracing::warn!("cannot build batch: {e}");
                break;
            }
        }
        if n < nova.max_payload_txs {
            break;
        }
    }
    produced
}

fn pay_rewards(node: &Node, policy: &RewardPolicy, rewards: &mut RewardManager, epoch: u64) {
    for m in rewards.due(epoch, policy.delay_epochs) {
        let Ok(Some(v)) = query::block_view(&node.db, &m.block) else { continue };
        if v.state.flags & xdag_chain::flags::MAIN == 0 || v.state.amount <= 0 {
            continue;
        }
        let balance = Nano(v.state.amount as u64);
        let now = xdag_types::time::now_xdag();
        let ep = node.params.epochs();
        let time = if ep.is_end_of_epoch(now) { now - 1 } else { now };
        if m.solo {
            if let Some(tx) = sweep_solo(&node.params, &node.key, &m, balance, time) {
                if node.import_local(tx.raw().to_vec(), None).is_imported() {
                    tracing::info!(block = %m.block, amount = %balance, "swept solo mining reward");
                }
            }
            continue;
        }
        if let Some((tx, node_share)) = payout_block(&node.params, policy, &node.key, &m, balance, time) {
            let out = node.import_local(tx.raw().to_vec(), None);
            if out.is_imported() {
                tracing::info!(block = %m.block, pool = %m.pool, "paid pool reward");
                rewards.node_batch.push((m.block, node_share));
            }
        }
        if rewards.node_batch.len() >= 10 {
            let batch: Vec<_> = rewards.node_batch.drain(..10).collect();
            if let Some(tx) = payout_node(&node.params, policy, &node.key, &batch, time) {
                node.import_local(tx.raw().to_vec(), None);
            }
        }
    }
}
