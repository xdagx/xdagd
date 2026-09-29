//! `xdagd bench` — throughput of the full import + execution path on this
//! machine, comparing Nova batch blocks with xdagj-style one-transaction-per-
//! block legacy transactions (both executed by this implementation).

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use rayon::prelude::*;
use xdag_chain::testkit::{nova_params, Sim};
use xdag_chain::{preverify, Source};
use xdag_types::nova::{NativeTransfer, NovaTx};
use xdag_types::{Block, BlockTemplate, CAmount, FieldType, HashLow, KeyPair, LinkTarget, Nano};

fn rate(n: usize, secs: f64) -> String {
    format!("{:>10.0} tx/s", n as f64 / secs.max(1e-9))
}

pub fn run(txs: usize, senders: usize, legacy: bool) -> Result<()> {
    println!("xdagd bench — {} CPU threads (rayon), release build recommended", rayon::current_num_threads());
    println!();
    nova(txs, senders)?;
    if legacy {
        println!();
        legacy_bench(txs.min(20_000), senders)?;
    }
    Ok(())
}

fn nova(txs: usize, senders: usize) -> Result<()> {
    let mut params = nova_params();
    if let Some(n) = params.nova.as_mut() {
        n.min_link_pow_bits = 8;
    }
    let mut s = Sim::with_evm(params.clone(), Arc::new(xdag_evm::RevmEngine::new()));
    s.mine_n(3);
    let chain_id = params.nova.as_ref().unwrap().chain_id;
    let fee = params.nova.as_ref().unwrap().min_native_fee;
    let keys: Vec<KeyPair> = (0..senders).map(|_| KeyPair::random()).collect();
    s.genesis_alloc(&keys.iter().map(|k| (k.address(), 1_000_000u128 * 1_000_000_000_000_000_000)).collect::<Vec<_>>());
    let per = txs.div_ceil(senders);
    println!("[Nova] {txs} native transfers from {senders} senders ({per} each), batches of up to 8192");

    // signing is the wallets' job: not timed
    let t0 = Instant::now();
    let signed: Vec<NovaTx> = (1..=per as u64)
        .flat_map(|nonce| keys.iter().map(move |k| (nonce, k)))
        .take(txs)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|(nonce, k)| NovaTx::Native(NativeTransfer::new_signed(k, chain_id, *nonce, keys_target(k), Nano(1), fee, b"").unwrap()))
        .collect();
    println!("  signed in {:.2}s (client side, not part of the measurement)", t0.elapsed().as_secs_f64());

    // A: admission (signature recovery + stateless checks), parallel
    let t = Instant::now();
    let ok = signed.par_iter().filter(|t| preverify::verify_payload_tx(t, &params, None).is_ok()).count();
    let a = t.elapsed().as_secs_f64();
    anyhow::ensure!(ok == signed.len(), "verification failed");
    println!("  A admission (verify signatures):    {}   ({:.3}s)", rate(ok, a), a);

    // B: batch blocks → import → main blocks → execution
    let t = Instant::now();
    let mut batches: Vec<HashLow> = vec![];
    for chunk in signed.chunks(8192) {
        let (b, payload) = s.chain.batch_block(&s.miner.clone(), chunk, s.t(100 + batches.len() as u64)).map_err(|e| anyhow!("{e}"))?;
        let pv = preverify(&b, Some(payload), &params, s.evm.as_deref()).map_err(|e| anyhow!(e))?;
        let out = s.chain.import(Arc::new(b.clone()), pv, Source::Local);
        anyhow::ensure!(out.is_imported(), "batch import: {out:?}");
        batches.push(b.hashlow());
    }
    let imported = t.elapsed().as_secs_f64();
    for group in batches.chunks(9) {
        s.mine(group);
    }
    s.mine_n(2);
    let total = t.elapsed().as_secs_f64();
    let applied = count_applied(&s, &signed);
    anyhow::ensure!(applied == signed.len(), "only {applied}/{} applied", signed.len());
    println!("  B block build + verify + import:    {}   ({:.3}s, {} batch blocks)", rate(signed.len(), imported), imported, batches.len());
    println!("  B end-to-end incl. execution:       {}   ({:.3}s)", rate(signed.len(), total), total);
    Ok(())
}

fn keys_target(k: &KeyPair) -> xdag_types::Address {
    // deterministic distinct receiver per sender
    let mut a = k.address();
    a.0[0] ^= 0xff;
    a
}

fn count_applied(s: &Sim, txs: &[NovaTx]) -> usize {
    txs.iter()
        .filter(|t| {
            let h = match t {
                NovaTx::Native(n) => n.tx_hash(),
                NovaTx::Evm(_) => return false,
            };
            matches!(xdag_chain::query::tx_location(s.chain.db(), &h), Ok(Some(l)) if l.status == xdag_chain::TxStatus::Applied)
        })
        .count()
}

/// xdagj model: every transfer is its own 512-byte DAG block that must be
/// referenced by link blocks and finally a main block. Run under Nova rules
/// (legacy rules let a lucky, disconnected link block outweigh a young test
/// chain and unwind it — see docs), so only the block structure differs.
fn legacy_bench(txs: usize, senders: usize) -> Result<()> {
    let params = nova_params();
    let mut s = Sim::new(params.clone());
    s.mine_n(3);
    let keys: Vec<KeyPair> = (0..senders).map(|_| KeyPair::random()).collect();
    s.genesis_alloc(&keys.iter().map(|k| (k.address(), 1_000_000u128 * 1_000_000_000_000_000_000)).collect::<Vec<_>>());
    let per = txs.div_ceil(senders);
    println!("[block-per-transaction model, as in xdagj] {txs} transfers, one 512-byte block each");
    let base = s.t(1000);
    let t0 = Instant::now();
    let blocks: Vec<Block> = (1..=per as u64)
        .flat_map(|nonce| keys.iter().enumerate().map(move |(i, k)| (nonce, i, k)))
        .take(txs)
        .collect::<Vec<_>>()
        .par_iter()
        .enumerate()
        .map(|(j, (nonce, _, k))| {
            let mut t = BlockTemplate::new(params.header_field(), base + j as u64);
            t.tx_nonce = Some(*nonce);
            let amt = Nano::from_milli(200).to_camount_legacy();
            t.links.push((FieldType::Input, LinkTarget::Address(k.address()), amt));
            t.links.push((FieldType::Output, LinkTarget::Address(keys_target(k)), amt));
            t.sign_out = Some((*k).clone());
            t.include_out_pubkey = true;
            xdag_chain::builder::seal_template(&params, t).unwrap()
        })
        .collect();
    println!("  signed in {:.2}s (client side, not part of the measurement)", t0.elapsed().as_secs_f64());
    let t = Instant::now();
    let pre: Vec<_> = blocks.par_iter().map(|b| preverify(b, None, &params, None)).collect();
    let mut hs = vec![];
    for (b, pv) in blocks.iter().zip(pre) {
        // Source::Sync: bypass the 3750-orphan gossip cap (measures processing speed)
        let out = s.chain.import_uncommitted(Arc::new(b.clone()), pv.map_err(|e| anyhow!(e))?, Source::Sync);
        anyhow::ensure!(out.is_imported(), "tx import: {out:?}");
        hs.push(b.hashlow());
        if hs.len() % 4096 == 0 {
            s.chain.commit(false).map_err(|e| anyhow!("{e}"))?;
        }
    }
    s.chain.commit(false).map_err(|e| anyhow!("{e}"))?;
    let imported = t.elapsed().as_secs_f64();
    // aggregate with link blocks (12 references each + PoW nonce), level by level
    let mut level = hs;
    let mut time = base + txs as u64 + 10;
    while level.len() > 9 {
        let mut next = vec![];
        for chunk in level.chunks(12) {
            let mut lt = BlockTemplate::new(params.header_field(), time);
            time += 1;
            for h in chunk {
                lt.links.push((FieldType::Out, LinkTarget::Block(*h), CAmount::ZERO));
            }
            lt.sign_out = Some(s.miner.clone());
            let lb = xdag_chain::builder::seal_template(&params, lt).map_err(|e| anyhow!("{e}"))?;
            anyhow::ensure!(s.import(&lb).is_imported(), "link import");
            next.push(lb.hashlow());
        }
        level = next;
    }
    s.mine(&level);
    s.mine_n(2);
    let total = t.elapsed().as_secs_f64();
    let applied = blocks
        .iter()
        .filter(|b| matches!(xdag_chain::query::tx_location(s.chain.db(), &b.hashlow().0), Ok(Some(l)) if l.status == xdag_chain::TxStatus::Applied))
        .count();
    anyhow::ensure!(applied == blocks.len(), "only {applied}/{} applied", blocks.len());
    println!("  verify + import tx blocks:          {}   ({:.3}s)", rate(blocks.len(), imported), imported);
    println!("  end-to-end incl. link blocks + exec:{}   ({:.3}s)", rate(blocks.len(), total), total);
    Ok(())
}
