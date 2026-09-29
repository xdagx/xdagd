//! Snapshot export/import: a node bootstrapped from a snapshot must reach the
//! same state as the node that exported it, never apply a pre-snapshot block
//! a second time and never reorganise below the snapshot.

mod common;

use std::sync::Arc;

use common::*;
use xdag_chain::records::{flags, AccountRecord};
use xdag_chain::{snapshot, ImportOutcome};
use xdag_storage::Table;
use xdag_types::nova::{NativeTransfer, NovaTx};
use xdag_types::{Address, HashLow, KeyPair, Nano};

/// Fresh node bootstrapped from `a`'s snapshot (reopened, as `xdagd` does).
fn bootstrap(a: &mut Sim, horizon_epochs: u64) -> Sim {
    let bytes = snapshot::export_with_horizon(&mut a.chain, Vec::new(), horizon_epochs).unwrap();
    let (db, guard) = xdag_storage::Db::open_temporary().unwrap();
    let mut b = Sim::with_db(a.params.clone(), db.clone(), guard);
    let h = snapshot::import(&mut b.chain, &mut &bytes[..]).unwrap();
    assert_eq!(h.nmain, a.chain.nmain());
    b.chain = xdag_chain::Chain::open(
        db,
        b.params.clone(),
        xdag_chain::ChainOptions { check_future: false, ..Default::default() },
        Arc::new(xdag_chain::pow::NoRandomX),
        None,
        b.clock.clone(),
    )
    .unwrap();
    b.clock.set(a.now());
    b.epoch = a.epoch;
    b
}

/// Import blocks produced by `a` into `b` (fetching missing parents from `a`,
/// as the node does on `NoParent`) and let `b` confirm main blocks.
fn follow(a: &mut Sim, b: &mut Sim, blocks: &[HashLow]) {
    for h in blocks {
        let mut todo = vec![*h];
        while let Some(h) = todo.last().copied() {
            let blk = a.chain.block(&h).unwrap().expect("block known to the exporter");
            match b.import(&blk) {
                ImportOutcome::NoParent(p) => todo.push(p),
                out => {
                    assert!(out.is_imported(), "{out:?}");
                    todo.pop();
                }
            }
        }
    }
    b.clock.set(a.now());
    b.epoch = a.epoch;
    b.chain.tick().unwrap();
}

fn accounts(s: &Sim) -> Vec<(Vec<u8>, String, u64)> {
    let mut v = vec![];
    s.chain
        .db()
        .for_each(Table::Account, |k, val| {
            let r = AccountRecord::decode(val).unwrap();
            v.push((k.to_vec(), format!("{:?}", r.balance), r.nonce));
            true
        })
        .unwrap();
    v
}

fn main_states(a: &mut Sim, b: &mut Sim) {
    assert_eq!(a.chain.nmain(), b.chain.nmain());
    for h in 1..=a.chain.nmain() {
        let ha = a.chain.main_at(h).unwrap().unwrap();
        assert_eq!(Some(ha), b.chain.main_at(h).unwrap(), "main block {h}");
        assert_eq!(a.chain.state(&ha).unwrap(), b.chain.state(&ha).unwrap(), "state of main block {h}");
    }
}

#[test]
fn snapshot_node_continues_exactly_like_the_exporter() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(2);
    // alice → bob, still unprocessed when the snapshot is taken, and older
    // than the horizon by then
    let tx = a.transfer(&alice, bob.address(), Nano::from_xdag(10), 1, 200);
    assert!(a.import(&tx).is_imported());
    a.mine_n(6);

    let mut b = bootstrap(&mut a, 3);
    assert_eq!(b.chain.nmain(), a.chain.nmain());
    assert_eq!(accounts(&a), accounts(&b));
    assert!(b.chain.block(&tx.hashlow()).unwrap().is_some(), "pending block carried in full");
    assert!(b.chain.block(&pay.hashlow()).unwrap().is_none(), "old processed zero-balance block left out");
    let last_main = b.chain.main_at(b.chain.nmain()).unwrap().unwrap();
    assert!(b.chain.block(&last_main).unwrap().is_some(), "recent main blocks carried in full");
    assert!(b.chain.block(&mains[1]).unwrap().is_none(), "old main blocks kept as snapshot blocks");

    let m = a.mine(&[tx.hashlow()]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    assert!(b.chain.nmain() > 11);
    main_states(&mut a, &mut b);
    assert_eq!(accounts(&a), accounts(&b));
    assert_eq!(b.balance(&bob.address()), Nano(Nano::from_xdag(10).0 - a.params.min_gas.0));
    let st = b.chain.state(&tx.hashlow()).unwrap();
    assert!(st.flags & flags::APPLIED != 0);

    // and the snapshot node can itself produce a snapshot for a third node
    let mut c = bootstrap(&mut b, 3);
    let more = a.mine_n(2);
    follow(&mut a, &mut b, &more);
    follow(&mut a, &mut c, &more);
    main_states(&mut a, &mut c);
    assert_eq!(accounts(&a), accounts(&c));
}

#[test]
fn a_late_pre_horizon_block_is_not_applied_twice() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(8);
    let mut b = bootstrap(&mut a, 3);
    assert!(b.chain.block(&pay.hashlow()).unwrap().is_none());

    // a new block references the old payout again: b has to fetch it
    let m = a.mine(&[pay.hashlow()]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[pay.hashlow()]);
    let st = b.chain.state(&pay.hashlow()).unwrap();
    assert!(st.flags & flags::MAIN_REF != 0, "recorded as processed before the snapshot");
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    main_states(&mut a, &mut b);
    assert_eq!(accounts(&a), accounts(&b));
    assert_eq!(b.balance(&alice.address()), a.balance(&alice.address()));
}

#[test]
fn snapshot_node_never_reorganises_below_the_snapshot() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(8);
    let mut b = bootstrap(&mut a, 3);
    let (top, nmain) = (b.chain.top(), b.chain.nmain());
    // a far heavier candidate forking from main block #2, and one that is
    // not connected to the main chain at all
    let fork = b.candidate_at(b.epoch, Some(mains[1]), &KeyPair::random(), &[], 12);
    assert!(matches!(b.import(&fork), ImportOutcome::ImportedNotBest));
    let lone = b.candidate_at(b.epoch, None, &KeyPair::random(), &[], 12);
    assert!(matches!(b.import(&lone), ImportOutcome::ImportedNotBest));
    assert_eq!((b.chain.top(), b.chain.nmain()), (top, nmain));
    // the real chain continues normally
    let next = a.mine_n(3);
    follow(&mut a, &mut b, &next);
    main_states(&mut a, &mut b);
}

#[test]
fn nova_batch_pending_at_snapshot_time_executes_after_import() {
    let mut a = Sim::new(nova_params());
    a.mine_n(3);
    let alice = KeyPair::random();
    let bob: Address = KeyPair::random().address();
    a.genesis_alloc(&[(alice.address(), 50 * 10u128.pow(18))]);
    let chain_id = a.params.nova.as_ref().unwrap().chain_id;
    let fee = a.params.nova.as_ref().unwrap().min_native_fee;
    let t1 = NativeTransfer::new_signed(&alice, chain_id, 1, bob, Nano::from_xdag(3), fee, b"x").unwrap();
    let t2 = NativeTransfer::new_signed(&alice, chain_id, 2, bob, Nano::from_xdag(4), fee, b"").unwrap();
    let now = a.t(20);
    let (batch, payload) = a.chain.batch_block(&a.miner.clone(), &[NovaTx::Native(t1), NovaTx::Native(t2)], now).unwrap();
    assert!(a.import_with(&batch, Some(payload)).is_imported());

    let mut b = bootstrap(&mut a, 3);
    assert_eq!(accounts(&a), accounts(&b));
    let m = a.mine(&[batch.hashlow()]);
    let rest = a.mine_n(2);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    main_states(&mut a, &mut b);
    assert_eq!(accounts(&a), accounts(&b));
    assert_eq!(b.balance(&bob), Nano::from_xdag(7));
}

#[test]
fn randomx_schedule_is_rebuilt_from_the_main_chain() {
    let mut p = legacy_params();
    p.randomx.fork_height = 8;
    p.randomx.seed_epoch_blocks = 4;
    p.randomx.seed_lag = 2;
    let mut a = Sim::new(p);
    a.mine_n(23);
    let want = a.chain.rx_schedule().clone();
    assert!(want.fork_epoch.is_some() && want.seeds.len() >= 2);
    let same = |x: &xdag_chain::pow::RxSchedule| x.fork_epoch == want.fork_epoch && x.seeds.iter().rev().take(2).eq(want.seeds.iter().rev().take(2));

    // a snapshot carrying the schedule
    let b = bootstrap(&mut a, 3);
    assert!(same(b.chain.rx_schedule()));

    // a snapshot without it (as written by the xdagj exporter)
    let bytes = snapshot::export_with_horizon(&mut a.chain, Vec::new(), 3).unwrap();
    let at = 4 + 1 + 1 + 8 + 24 + 32 + 8;
    let n = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    let mut stripped = bytes[..at].to_vec();
    stripped.extend_from_slice(&0u64.to_le_bytes());
    stripped.extend_from_slice(&bytes[at + 8 + n..]);
    let mut c = Sim::new(a.params.clone());
    snapshot::import(&mut c.chain, &mut &stripped[..]).unwrap();
    assert!(same(c.chain.rx_schedule()));

    // and one whose schedule contradicts its main chain is refused
    let mut wrong = want.clone();
    wrong.fork_epoch = wrong.fork_epoch.map(|e| e + 1);
    let enc = wrong.encode();
    let mut bad = bytes[..at].to_vec();
    bad.extend_from_slice(&(enc.len() as u64).to_le_bytes());
    bad.extend_from_slice(&enc);
    bad.extend_from_slice(&bytes[at + 8 + n..]);
    let mut d = Sim::new(a.params.clone());
    assert!(snapshot::import(&mut d.chain, &mut &bad[..]).is_err());
}
