//! Legacy (xdagj-compatible) consensus scenarios.

mod common;

use common::*;
use xdag_chain::records::{flags, TxStatus};
use xdag_chain::ImportOutcome;
use xdag_types::{KeyPair, Nano};

#[test]
fn main_chain_grows_and_rewards_accrue() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(6);
    // the newest candidate is the top; confirmations lag by one
    assert_eq!(s.chain.nmain(), 5);
    let reward = xdag_chain::fees::reward(1, &s.params);
    assert_eq!(reward, Nano::from_xdag(1024));
    for (i, h) in mains.iter().take(5).enumerate() {
        let st = s.chain.state(h).unwrap();
        assert_eq!(st.height, i as u64 + 1);
        assert!(st.flags & flags::MAIN != 0);
        assert_eq!(st.amount, reward.0 as i64);
        assert_eq!(st.ref_, Some(*h));
    }
    assert_eq!(s.chain.top(), Some(mains[5]));
}

#[test]
fn payout_and_account_transfer() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let bob = KeyPair::random();

    // pool pays 100 XDAG from main block #1 to alice
    let pay = s.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(s.import(&pay).is_imported());
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    let fee = s.params.min_gas; // 0.1 XDAG per output
    assert_eq!(s.balance(&alice.address()), Nano(Nano::from_xdag(100).0 - fee.0));
    assert_eq!(s.block_amount(&mains[0]), Nano::from_xdag(1024 - 100).0 as i64);

    // alice → bob, nonce 1
    let tx = s.transfer(&alice, bob.address(), Nano::from_xdag(10), 1, 200);
    assert!(s.import(&tx).is_imported());
    s.mine(&[tx.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.balance(&bob.address()), Nano(Nano::from_xdag(10).0 - fee.0));
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 1);
    let loc = xdag_chain::query::tx_location(s.chain.db(), &tx.hashlow().0).unwrap().unwrap();
    assert_eq!(loc.status, TxStatus::Applied);

    // history recorded at execution time
    let hist = xdag_chain::query::address_history(s.chain.db(), &bob.address(), None, 10).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].status, TxStatus::Applied);
}

#[test]
fn insufficient_balance_consumes_nonce_without_transfer() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let pay = s.payout(mains[0], alice.address(), Nano::from_xdag(5));
    s.import(&pay);
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    let before = s.balance(&alice.address());
    let tx = s.transfer(&alice, KeyPair::random().address(), Nano::from_xdag(50), 1, 300);
    assert!(s.import(&tx).is_imported());
    s.mine(&[tx.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.balance(&alice.address()), before);
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 1, "xdagj consumes the nonce of a rejected tx");
    let loc = xdag_chain::query::tx_location(s.chain.db(), &tx.hashlow().0).unwrap().unwrap();
    assert_eq!(loc.status, TxStatus::Rejected);
}

#[test]
fn validation_rules_match_xdagj() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    // INPUT from an address that never received funds
    let tx = s.transfer(&alice, KeyPair::random().address(), Nano::from_xdag(1), 1, 10);
    assert!(matches!(s.import(&tx), ImportOutcome::Deferred(m) if m.contains("isn't exist")));
    // spending a main block with the wrong key
    let mut bad = s.payout(mains[0], alice.address(), Nano::from_xdag(1));
    let thief = KeyPair::random();
    {
        let mut t = xdag_types::BlockTemplate::new(s.params.header_field(), s.t(20));
        for l in bad.links() {
            t.links.push((l.kind, l.target, l.amount));
        }
        t.sign_out = Some(thief.clone());
        t.include_out_pubkey = true;
        bad = t.build().unwrap();
    }
    assert!(matches!(s.import(&bad), ImportOutcome::Invalid(m) if m.contains("can't be used")));
    // output below the per-output fee
    let small = s.payout(mains[0], alice.address(), Nano(1000));
    assert!(matches!(s.import(&small), ImportOutcome::Invalid(_)));
    // unknown parent
    let orphan_ref = s.payout(xdag_types::HashLow([7u8; 24]), alice.address(), Nano::from_xdag(1));
    assert!(matches!(s.import(&orphan_ref), ImportOutcome::NoParent(_)));
    // duplicate import
    let ok = s.payout(mains[1], alice.address(), Nano::from_xdag(1));
    assert!(s.import(&ok).is_imported());
    assert_eq!(s.import(&ok), ImportOutcome::Exist);
}

#[test]
fn nonce_gap_waits_and_stale_nonce_is_ignored() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    let pay = s.payout(mains[0], alice.address(), Nano::from_xdag(100));
    s.import(&pay);
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    // nonce 2 before nonce 1: not processed (xdagj -1), stays MAIN_REF without ref
    let tx2 = s.transfer(&alice, bob.address(), Nano::from_xdag(1), 2, 10);
    s.import(&tx2);
    s.mine(&[tx2.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 0);
    let st = s.chain.state(&tx2.hashlow()).unwrap();
    assert!(st.flags & flags::MAIN_REF != 0 && st.flags & flags::APPLIED == 0 && st.ref_.is_none());
    let tx1 = s.transfer(&alice, bob.address(), Nano::from_xdag(1), 1, 20);
    s.import(&tx1);
    s.mine(&[tx1.hashlow()]);
    s.mine_n(2);
    assert_eq!(s.account(&alice.address()).unwrap().nonce, 1);
}

#[test]
fn reorg_undoes_and_redoes_state_exactly() {
    // Node X sees chain A (with a payout), then a heavier chain B without it,
    // then A extended past B. Node Y only ever sees the final chain A.
    // Their states must be byte-identical: rollback via the undo journal is exact.
    let mut x = Sim::new(legacy_params());
    let base = x.mine_n(3);
    let alice = KeyPair::random();
    let pay = x.payout(base[0], alice.address(), Nano::from_xdag(100));
    assert!(x.import(&pay).is_imported());
    let fork_epoch = x.epoch;
    let miner_a = x.miner.clone();
    let miner_b = KeyPair::random();

    // chain A: 3 blocks, low difficulty
    let mut a_blocks = vec![];
    let mut prev = base[2];
    for i in 0..3 {
        let links: Vec<_> = if i == 0 { vec![pay.hashlow()] } else { vec![] };
        let b = x.candidate_at(fork_epoch + i, Some(prev), &miner_a, &links, 0);
        prev = b.hashlow();
        a_blocks.push(b);
    }
    // chain B: 3 blocks, much heavier
    let mut b_blocks = vec![];
    let mut prev_b = base[2];
    for i in 0..3 {
        let b = x.candidate_at(fork_epoch + i, Some(prev_b), &miner_b, &[], 14);
        prev_b = b.hashlow();
        b_blocks.push(b);
    }
    // A extension: 3 more heavy blocks on top of A
    let mut a_ext = vec![];
    for i in 3..6 {
        let b = x.candidate_at(fork_epoch + i, Some(prev), &miner_a, &[], 18);
        prev = b.hashlow();
        a_ext.push(b);
    }
    let far = x.epochs().start_of_epoch(fork_epoch + 20);

    for b in &a_blocks {
        assert!(x.import(b).is_imported());
    }
    x.clock.set(far);
    x.chain.tick().unwrap();
    assert_eq!(x.balance(&alice.address()).0, Nano::from_xdag(100).0 - x.params.min_gas.0);

    for b in &b_blocks {
        assert!(x.import(b).is_imported());
    }
    x.chain.tick().unwrap();
    assert_eq!(x.chain.top(), Some(b_blocks[2].hashlow()), "heavier chain B must win");
    assert_eq!(x.balance(&alice.address()), Nano::ZERO, "payout on the losing branch is undone");
    assert_eq!(x.block_amount(&base[0]), Nano::from_xdag(1024).0 as i64, "spent block balance restored");

    for b in &a_ext {
        let o = x.import(b);
        assert!(o.is_imported(), "{o:?}");
    }
    x.chain.tick().unwrap();
    assert_eq!(x.chain.top(), Some(a_ext[2].hashlow()));
    assert_eq!(x.balance(&alice.address()).0, Nano::from_xdag(100).0 - x.params.min_gas.0);

    // Node Y: same base, never sees B
    let mut y = Sim::new(legacy_params());
    y.clock.set(x.clock_now());
    let mut all: Vec<xdag_types::Block> = vec![];
    for h in &base {
        all.push((*x.chain.block(h).unwrap().unwrap()).clone());
    }
    all.push(pay.clone());
    all.extend(a_blocks.iter().cloned());
    all.extend(a_ext.iter().cloned());
    for b in &all {
        let o = y.import(b);
        assert!(o.is_imported(), "{o:?}");
    }
    y.chain.tick().unwrap();
    assert_eq!(y.chain.top(), x.chain.top());
    assert_eq!(y.chain.nmain(), x.chain.nmain());
    // B's blocks exist in X but not in Y: compare only states of Y's blocks and all accounts
    let fx = x.state_fingerprint();
    let fy = y.state_fingerprint();
    for (k, v) in &fy {
        let other = fx.iter().find(|(kx, _)| kx == k).map(|(_, v)| v);
        assert_eq!(other, Some(v), "state entry {} differs", hex::encode(k));
    }
    let accounts_x: Vec<_> = fx.iter().filter(|(k, _)| k[0] == xdag_storage::Table::Account as u8).collect();
    let accounts_y: Vec<_> = fy.iter().filter(|(k, _)| k[0] == xdag_storage::Table::Account as u8).collect();
    assert_eq!(accounts_x, accounts_y);
}

#[test]
fn import_order_does_not_change_the_result() {
    // Build a DAG once, then import it in several orders.
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let pay = s.payout(mains[0], alice.address(), Nano::from_xdag(100));
    s.import(&pay);
    s.mine(&[pay.hashlow()]);
    let tx = s.transfer(&alice, KeyPair::random().address(), Nano::from_xdag(1), 1, 50);
    s.mine_n(1);
    s.import(&tx);
    s.mine(&[tx.hashlow()]);
    s.mine_n(3);
    let expected_top = s.chain.top();
    let expected_nmain = s.chain.nmain();
    let expected_alice = s.balance(&alice.address());

    // collect all stored + extra blocks via the time index
    let db = s.chain.db().clone();
    let mut hashes = xdag_chain::query::blocks_in_epochs(&db, 0, u64::MAX, 10_000).unwrap();
    if let Some(t) = expected_top {
        if !hashes.contains(&t) {
            hashes.push(t);
        }
    }
    let mut blocks = vec![];
    for h in &hashes {
        if let Some(b) = s.chain.block(h).unwrap() {
            blocks.push((*b).clone());
        }
    }
    for seed in 0..4u64 {
        let mut order = blocks.clone();
        // deterministic shuffle
        let mut x = seed.wrapping_mul(0x9e3779b97f4a7c15) | 1;
        for i in (1..order.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            order.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let mut t = Sim::new(legacy_params());
        t.clock.set(s.clock.now_xdag_pub());
        let mut pending = order;
        let mut guard = 0;
        while !pending.is_empty() && guard < 100 {
            guard += 1;
            let mut next = vec![];
            for b in pending {
                match t.import(&b) {
                    ImportOutcome::NoParent(_) | ImportOutcome::Deferred(_) => next.push(b),
                    o => assert!(o.is_imported() || o == ImportOutcome::Exist || o == ImportOutcome::InMem, "{o:?}"),
                }
            }
            pending = next;
            t.chain.tick().unwrap();
        }
        t.chain.tick().unwrap();
        assert_eq!(t.chain.top(), expected_top, "order {seed}");
        assert_eq!(t.chain.nmain(), expected_nmain, "order {seed}");
        assert_eq!(t.balance(&alice.address()), expected_alice, "order {seed}");
    }
}

impl ClockNow for Sim {
    fn clock_now(&self) -> u64 {
        use xdag_chain::Clock;
        self.clock.now_xdag()
    }
}
trait ClockNow {
    fn clock_now(&self) -> u64;
}

trait ClockExt {
    fn now_xdag_pub(&self) -> u64;
}
impl ClockExt for std::sync::Arc<xdag_chain::ManualClock> {
    fn now_xdag_pub(&self) -> u64 {
        use xdag_chain::Clock;
        self.now_xdag()
    }
}

#[test]
fn restart_recovers_top_when_candidate_was_in_memory() {
    let (db, guard) = xdag_storage::Db::open_temporary().unwrap();
    let mut s = Sim::with_db(legacy_params(), db.clone(), guard);
    s.mine_n(5);
    let nmain = s.chain.nmain();
    let top = s.chain.top().unwrap();
    assert_eq!(s.chain.extra_count(), 1, "the newest candidate is only in memory");
    let clock = s.clock.clone();
    // "restart": reopen the chain on the same database
    let reopened = xdag_chain::Chain::open(
        db,
        s.params.clone(),
        xdag_chain::ChainOptions { check_future: false, ..Default::default() },
        std::sync::Arc::new(xdag_chain::pow::NoRandomX),
        None,
        clock,
    )
    .unwrap();
    assert_ne!(reopened.top(), Some(top));
    assert_eq!(reopened.nmain(), nmain);
    s.chain = reopened;
    // mining continues on top of the last main block
    s.mine_n(3);
    assert!(s.chain.nmain() > nmain, "chain advances after restart");
}

#[test]
fn multi_output_payouts_are_applied() {
    let mut s = Sim::new(legacy_params());
    let mains = s.mine_n(3);
    let keys: Vec<KeyPair> = (0..20).map(|_| KeyPair::random()).collect();
    let mut pays = vec![];
    for chunk in keys.chunks(9) {
        let per = Nano::from_xdag(1);
        let mut t = xdag_types::BlockTemplate::new(s.params.header_field(), s.t(50 + pays.len() as u64));
        t.links.push((xdag_types::FieldType::In, xdag_types::LinkTarget::Block(mains[0]), Nano(per.0 * chunk.len() as u64).to_camount_legacy()));
        for k in chunk {
            t.links.push((xdag_types::FieldType::Output, xdag_types::LinkTarget::Address(k.address()), per.to_camount_legacy()));
        }
        t.sign_out = Some(s.miner.clone());
        t.include_out_pubkey = true;
        let b = t.build().unwrap();
        let o = s.import(&b);
        assert!(o.is_imported(), "{o:?}");
        pays.push(b.hashlow());
    }
    let before = s.chain.nmain();
    for g in pays.chunks(9) {
        let top_before = s.chain.top();
        s.mine(g);
        eprintln!("mined: top changed = {}", s.chain.top() != top_before);
    }
    s.mine_n(2);
    assert!(s.chain.nmain() > before, "nmain stuck at {}", s.chain.nmain());
    assert!(s.balance(&keys[0].address()).0 > 0);
}

/// A block linked to nothing, whose sha256d hash was ground past the
/// chain's total difficulty, becomes the top under legacy rules; the fork
/// search finds no common ancestor and every main block is unwound.
fn disconnected_heavy_block(s: &Sim, min_zero_bits: u32) -> xdag_types::Block {
    let mut t = xdag_types::BlockTemplate::new(s.params.header_field(), s.t(5000));
    t.sign_out = Some(KeyPair::random());
    t.mining_nonce = Some([0u8; 32]);
    let b = t.build().unwrap();
    let mut raw = *b.raw();
    let mut ctr = 0u64;
    loop {
        let h = xdag_types::hash::sha256d(&raw);
        if xdag_chain::pow::pow_zero_bits(&h) >= min_zero_bits {
            return xdag_types::Block::parse(&raw).unwrap();
        }
        ctr += 1;
        raw[480..488].copy_from_slice(&ctr.to_le_bytes());
    }
}

#[test]
fn legacy_rules_let_a_disconnected_ground_block_unwind_the_whole_chain() {
    let mut s = Sim::new(legacy_params());
    s.mine_n(6);
    assert_eq!(s.chain.nmain(), 5);
    let evil = disconnected_heavy_block(&s, 12);
    assert!(evil.links().next().is_none());
    assert!(s.import(&evil).is_imported());
    assert_eq!(s.chain.top(), Some(evil.hashlow()));
    assert_eq!(s.chain.nmain(), 0, "every main block was unset");
}

#[test]
fn nova_rules_ignore_a_disconnected_ground_block() {
    let mut s = Sim::new(nova_params());
    s.mine_n(6);
    let before = (s.chain.top(), s.chain.nmain());
    let evil = disconnected_heavy_block(&s, 12);
    assert!(s.import(&evil).is_imported());
    assert_eq!((s.chain.top(), s.chain.nmain()), before);
}
