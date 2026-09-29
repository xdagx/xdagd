//! Nova rules: batch payloads, exact amounts, fee-on-failure, anti-spam PoW.

mod common;

use std::sync::Arc;

use common::*;
use xdag_chain::records::TxStatus;
use xdag_chain::ImportOutcome;
use xdag_types::nova::{NativeTransfer, NovaTx};
use xdag_types::{BlockTemplate, CAmount, FieldType, KeyPair, LinkTarget, Nano};

fn funded(s: &mut Sim, who: &KeyPair, xdag: u64) {
    let mains = s.chain.meta().nmain;
    let src = s.chain.main_at(mains.max(1)).unwrap().unwrap();
    let pay = s.payout(src, who.address(), Nano::from_xdag(xdag));
    assert!(s.import(&pay).is_imported(), "payout import");
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
}

fn chain_id(s: &Sim) -> u64 {
    s.params.nova.as_ref().unwrap().chain_id
}

#[test]
fn batch_of_native_transfers_executes_exactly() {
    let mut s = Sim::new(nova_params());
    s.mine_n(3);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    let carol = KeyPair::random();
    funded(&mut s, &alice, 100);
    let start = s.chain.meta().nmain;
    let alice_before = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    assert_eq!(alice_before, (Nano::from_xdag(100).0 - s.params.min_gas.0) as u128 * 1_000_000_000);

    let fee = Nano::from_milli(1);
    let t1 = NativeTransfer::new_signed(&alice, chain_id(&s), 1, bob.address(), Nano(1_234_567_891), fee, b"one").unwrap();
    let t2 = NativeTransfer::new_signed(&alice, chain_id(&s), 2, carol.address(), Nano(7), fee, b"").unwrap();
    // wrong nonce: skipped, not failed
    let t3 = NativeTransfer::new_signed(&alice, chain_id(&s), 9, carol.address(), Nano(7), fee, b"").unwrap();
    let now = s.t(500);
    let (batch, payload) =
        s.chain.batch_block(&s.miner.clone(), &[NovaTx::Native(t1.clone()), NovaTx::Native(t2.clone()), NovaTx::Native(t3.clone())], now).unwrap();
    // without its payload the block is not acceptable
    assert!(matches!(s.import(&batch), ImportOutcome::Invalid(_)));
    let o = s.import_with(&batch, Some(payload.clone()));
    assert!(o.is_imported(), "{o:?}");
    s.mine(&[batch.hashlow()]);
    s.mine_n(2);
    assert!(s.chain.meta().nmain > start);

    let wei = 1_000_000_000u128;
    assert_eq!(xdag_chain::query::balance_wei(s.chain.db(), &bob.address()).unwrap(), 1_234_567_891 * wei);
    assert_eq!(xdag_chain::query::balance_wei(s.chain.db(), &carol.address()).unwrap(), 7 * wei);
    let alice_after = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    assert_eq!(alice_before - alice_after, (1_234_567_891 + 7 + 2 * fee.0 as u128) * wei);
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 2);
    let l1 = xdag_chain::query::tx_location(s.chain.db(), &t1.tx_hash()).unwrap().unwrap();
    assert_eq!(l1.status, TxStatus::Applied);
    assert!(xdag_chain::query::tx_location(s.chain.db(), &t3.tx_hash()).unwrap().is_none());
    let hist = xdag_chain::query::address_history(s.chain.db(), &bob.address(), None, 10).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].remark, b"one");
}

#[test]
fn failed_transfer_pays_fee_and_consumes_nonce() {
    let mut s = Sim::new(nova_params());
    s.mine_n(3);
    let alice = KeyPair::random();
    funded(&mut s, &alice, 1);
    let before = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    let fee = Nano::from_milli(2);
    let t = NativeTransfer::new_signed(&alice, chain_id(&s), 1, KeyPair::random().address(), Nano::from_xdag(50), fee, b"").unwrap();
    let (batch, payload) = s.chain.batch_block(&s.miner.clone(), &[NovaTx::Native(t.clone())], s.t(10)).unwrap();
    assert!(s.import_with(&batch, Some(payload)).is_imported());
    s.mine(&[batch.hashlow()]);
    s.mine_n(2);
    let after = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    assert_eq!(before - after, fee.0 as u128 * 1_000_000_000);
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 1);
    let loc = xdag_chain::query::tx_location(s.chain.db(), &t.tx_hash()).unwrap().unwrap();
    assert_eq!(loc.status, TxStatus::Failed);
}

#[test]
fn legacy_tx_blocks_still_work_and_pay_fee_on_failure() {
    let mut s = Sim::new(nova_params());
    s.mine_n(3);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    funded(&mut s, &alice, 10);
    // spend more than the balance: output fee (0.1 XDAG) is charged
    let before = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    let tx = s.transfer(&alice, bob.address(), Nano::from_xdag(100), 1, 10);
    assert!(s.import(&tx).is_imported());
    s.mine(&[tx.hashlow()]);
    s.mine_n(2);
    let after = xdag_chain::query::balance_wei(s.chain.db(), &alice.address()).unwrap();
    assert_eq!(before - after, s.params.min_gas.0 as u128 * 1_000_000_000);
    let loc = xdag_chain::query::tx_location(s.chain.db(), &tx.hashlow().0).unwrap().unwrap();
    assert_eq!(loc.status, TxStatus::Failed);
    // a valid one afterwards (nonce 2)
    let tx2 = s.transfer(&alice, bob.address(), Nano::from_xdag(5), 2, 20);
    assert!(s.import(&tx2).is_imported());
    s.mine(&[tx2.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.balance(&bob.address()), Nano(Nano::from_xdag(5).0 - s.params.min_gas.0));
}

#[test]
fn anti_spam_pow_and_payload_integrity() {
    let mut s = Sim::new(nova_params());
    let mains = s.mine_n(3);
    // a link block without proof of work
    let mut t = BlockTemplate::new(s.params.header_field(), s.t(5));
    t.links.push((FieldType::Out, LinkTarget::Block(mains[2]), CAmount::ZERO));
    t.sign_out = Some(s.miner.clone());
    let mut raw_block = None;
    for i in 0..64u8 {
        t.remark = Some([i; 32]);
        let b = t.build().unwrap();
        if xdag_chain::pow::pow_zero_bits(&b.hash().0) < 4 {
            raw_block = Some(b);
            break;
        }
    }
    let unsealed = raw_block.unwrap();
    assert!(matches!(s.import(&unsealed), ImportOutcome::Invalid(m) if m.contains("proof of work")));
    // the sealed version is accepted
    let sealed = xdag_chain::builder::seal_template(&s.params, t).unwrap();
    assert!(s.import(&sealed).is_imported());

    // payload tampering
    let alice = KeyPair::random();
    let tx = NativeTransfer::new_signed(&alice, chain_id(&s), 1, alice.address(), Nano(1), Nano::from_milli(1), b"").unwrap();
    let (batch, payload) = s.chain.batch_block(&s.miner.clone(), &[NovaTx::Native(tx)], s.t(30)).unwrap();
    let mut bad = (*payload).clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert!(matches!(s.import_with(&batch, Some(Arc::new(bad))), ImportOutcome::Invalid(m) if m.contains("root")));
    // wrong chain id inside the payload
    let foreign = NativeTransfer::new_signed(&alice, 1, 1, alice.address(), Nano(1), Nano::from_milli(1), b"").unwrap();
    let (b2, p2) = s.chain.batch_block(&s.miner.clone(), &[NovaTx::Native(foreign)], s.t(40)).unwrap();
    assert!(matches!(s.import_with(&b2, Some(p2)), ImportOutcome::Invalid(m) if m.contains("chain id")));
}

#[test]
fn duplicate_in_links_cannot_double_spend_a_block_balance() {
    // xdagj bug: two IN links to the same block each pass the balance check,
    // then both are subtracted and the block balance goes negative (inflation).
    let mut s = Sim::new(nova_params());
    let mains = s.mine_n(3);
    let bal = s.block_amount(&mains[0]);
    assert!(bal > 0);
    let half_plus = Nano((bal as u64) / 2 + Nano::from_xdag(10).0);
    let thief_to = KeyPair::random().address();
    let mut t = BlockTemplate::new(s.params.header_field(), s.t(50));
    t.links.push((FieldType::In, LinkTarget::Block(mains[0]), half_plus.to_camount_legacy()));
    t.links.push((FieldType::In, LinkTarget::Block(mains[0]), half_plus.to_camount_legacy()));
    t.links.push((FieldType::Output, LinkTarget::Address(thief_to), Nano(half_plus.0 * 2).to_camount_legacy()));
    t.sign_out = Some(s.miner.clone());
    t.include_out_pubkey = true;
    let b = xdag_chain::builder::seal_template(&s.params, t).unwrap();
    assert!(s.import(&b).is_imported());
    s.mine(&[b.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.balance(&thief_to), Nano::ZERO);
    assert_eq!(s.block_amount(&mains[0]), bal);
    let loc = xdag_chain::query::tx_location(s.chain.db(), &b.hashlow().0).unwrap().unwrap();
    assert_eq!(loc.status, TxStatus::Rejected);
}

#[test]
fn legacy_rules_allow_the_duplicate_in_double_spend() {
    // Documents the xdagj behaviour we must stay compatible with before Nova.
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let bal = s.block_amount(&mains[0]);
    let half_plus = Nano((bal as u64) / 2 + Nano::from_xdag(10).0);
    let to = KeyPair::random().address();
    let mut t = BlockTemplate::new(s.params.header_field(), s.t(50));
    t.links.push((FieldType::In, LinkTarget::Block(mains[0]), half_plus.to_camount_legacy()));
    t.links.push((FieldType::In, LinkTarget::Block(mains[0]), half_plus.to_camount_legacy()));
    t.links.push((FieldType::Output, LinkTarget::Address(to), Nano(half_plus.0 * 2).to_camount_legacy()));
    t.sign_out = Some(s.miner.clone());
    t.include_out_pubkey = true;
    let b = t.build().unwrap();
    assert!(s.import(&b).is_imported());
    s.mine(&[b.hashlow()]);
    s.mine_n(2);
    assert!(s.block_amount(&mains[0]) < 0, "xdagj lets the block balance go negative");
}

/// A mid-epoch block whose sha256d hash was ground (cheap on GPUs) competing
/// with RandomX-mined candidates for the main block of an epoch.
fn ground_link_block(s: &Sim, parent: xdag_types::HashLow, bits: u32) -> xdag_types::Block {
    let attacker = KeyPair::random();
    let mut t = BlockTemplate::new(s.params.header_field(), s.t(1000));
    t.links.push((FieldType::Out, LinkTarget::Block(parent), CAmount::ZERO));
    t.sign_out = Some(attacker);
    t.mining_nonce = Some([0u8; 32]);
    let b = t.build().unwrap();
    let mut raw = *b.raw();
    let mut ctr = 0u64;
    loop {
        let h = xdag_types::hash::sha256d(&raw);
        if xdag_chain::pow::pow_zero_bits(&h) >= bits {
            return xdag_types::Block::parse(&raw).unwrap();
        }
        ctr += 1;
        raw[480..488].copy_from_slice(&ctr.to_le_bytes());
    }
}

#[test]
fn legacy_rules_let_a_ground_link_block_take_the_main_block() {
    let mut s = Sim::new(legacy_params());
    s.mine_n(3);
    let top = s.chain.top().unwrap();
    let evil = ground_link_block(&s, top, 16);
    assert!(s.import(&evil).is_imported());
    assert_eq!(s.chain.top(), Some(evil.hashlow()), "sha256d grinding outweighs the candidates");
    s.mine_n(3);
    let st = s.chain.state(&evil.hashlow()).unwrap();
    assert!(st.flags & xdag_chain::flags::MAIN != 0, "the ground block became a main block and took the reward");
}

#[test]
fn nova_rules_ignore_hash_grinding_of_non_candidates() {
    let mut s = Sim::new(nova_params());
    s.mine_n(3);
    let top = s.chain.top().unwrap();
    let evil = ground_link_block(&s, top, 16);
    assert!(s.import(&evil).is_imported());
    assert_eq!(s.chain.top(), Some(top), "a non-candidate block adds no difficulty");
    s.mine_n(3);
    let st = s.chain.state(&evil.hashlow()).unwrap();
    assert!(st.flags & xdag_chain::flags::MAIN == 0);
}

#[test]
fn history_records_the_credited_amount() {
    let mut s = Sim::new(nova_params());
    s.mine_n(3);
    let alice = KeyPair::random();
    funded(&mut s, &alice, 10);
    let bal = s.balance(&alice.address());
    let hist = xdag_chain::query::address_history(s.chain.db(), &alice.address(), None, 10).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].amount, bal, "history shows what was credited");
}
