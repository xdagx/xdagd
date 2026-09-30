//! Snapshot export/import: a node loaded from a snapshot must hold the same
//! state as the node that exported it, keep agreeing with it, never execute a
//! block twice and never reorganise below the snapshot.

mod common;

use std::sync::Arc;

use common::*;
use xdag_chain::records::{flags, AccountRecord, Balance, BlockInfo, BlockState, SnapshotKey};
use xdag_chain::snapshot::{self, SnapshotBlock, SnapshotData, SnapshotHeader, SnapshotReader, SnapshotWriter};
use xdag_chain::ImportOutcome;
use xdag_storage::{Db, Table};
use xdag_types::nova::{NativeTransfer, NovaTx};
use xdag_types::{Address, HashLow, KeyPair, Nano};

fn export(a: &mut Sim) -> Vec<u8> {
    snapshot::export(&mut a.chain, Vec::new()).unwrap()
}

fn reopen(db: Db, s: &Sim) -> xdag_chain::Result<xdag_chain::Chain> {
    xdag_chain::Chain::open(
        db,
        s.params.clone(),
        xdag_chain::ChainOptions { check_future: false, ..Default::default() },
        Arc::new(xdag_chain::pow::NoRandomX),
        None,
        s.clock.clone(),
    )
}

/// Fresh node loaded from a snapshot of `a` (reopened, as `xdagd` does).
fn node_from(a: &Sim, bytes: &[u8]) -> Sim {
    let (db, guard) = Db::open_temporary().unwrap();
    let mut b = Sim::with_db(a.params.clone(), db.clone(), guard);
    let h = snapshot::import(&mut b.chain, &mut &bytes[..]).unwrap();
    assert_eq!(h.nmain, a.chain.nmain());
    b.chain = reopen(db, &b).unwrap();
    b.clock.set(a.now());
    b.epoch = a.epoch;
    b
}

fn bootstrap(a: &mut Sim) -> Sim {
    let bytes = export(a);
    node_from(a, &bytes)
}

/// A snapshot taken apart, to rewrite it the way another exporter would.
struct Parsed {
    header: SnapshotHeader,
    accounts: Vec<(Address, AccountRecord)>,
    blocks: Vec<SnapshotBlock>,
    mains: Vec<(u64, HashLow)>,
}

fn parse(bytes: &[u8]) -> Parsed {
    let (mut r, header) = SnapshotReader::new(bytes, 1 << 20).unwrap();
    let accounts = (0..r.section().unwrap()).map(|_| r.account().unwrap()).collect();
    let blocks = (0..r.section().unwrap()).map(|_| r.block().unwrap()).collect();
    let mains = (0..r.section().unwrap()).map(|_| r.main().unwrap()).collect();
    Parsed { header, accounts, blocks, mains }
}

/// Written like an xdagj export: no execution-record tables.
fn write(p: &Parsed) -> Vec<u8> {
    let mut w = SnapshotWriter::new(Vec::new(), &p.header).unwrap();
    w.accounts(&p.accounts).unwrap();
    w.blocks(&p.blocks).unwrap();
    w.main_index(&p.mains).unwrap()
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

fn dump(s: &Sim, t: Table) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut v = vec![];
    s.chain
        .db()
        .for_each(t, |k, val| {
            v.push((k.to_vec(), val.to_vec()));
            true
        })
        .unwrap();
    v
}

fn accounts(s: &Sim) -> Vec<(Vec<u8>, Vec<u8>)> {
    dump(s, Table::Account)
}

fn main_states(a: &mut Sim, b: &mut Sim) {
    assert_eq!(a.chain.nmain(), b.chain.nmain());
    for h in 1..=a.chain.nmain() {
        let ha = a.chain.main_at(h).unwrap().unwrap();
        assert_eq!(Some(ha), b.chain.main_at(h).unwrap(), "main block {h}");
        assert_eq!(a.chain.state(&ha).unwrap(), b.chain.state(&ha).unwrap(), "state of main block {h}");
    }
}

/// A legacy account transfer with an explicit block time.
fn transfer_at(s: &Sim, from: &KeyPair, to: Address, amount: Nano, nonce: u64, time: u64) -> xdag_types::Block {
    let mut t = xdag_types::BlockTemplate::new(s.params.header_field(), time);
    t.tx_nonce = Some(nonce);
    t.links.push((xdag_types::FieldType::Input, xdag_types::LinkTarget::Address(from.address()), amount.to_camount_legacy()));
    t.links.push((xdag_types::FieldType::Output, xdag_types::LinkTarget::Address(to), amount.to_camount_legacy()));
    t.sign_out = Some(from.clone());
    t.include_out_pubkey = true;
    xdag_chain::builder::seal_template(&s.params, t).unwrap()
}

#[test]
fn a_snapshot_reproduces_the_node_and_both_continue_alike() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(2);
    // alice → bob, still unprocessed when the snapshot is taken
    let tx = a.transfer(&alice, bob.address(), Nano::from_xdag(10), 1, 200);
    assert!(a.import(&tx).is_imported());
    a.mine_n(6);

    let mut b = bootstrap(&mut a);
    // Every consensus and record table is identical: same blocks (so peers see
    // the same per-range checksums), same execution state, same history.
    for t in [
        Table::Account,
        Table::BlockInfo,
        Table::BlockState,
        Table::BlockRaw,
        Table::MainHeight,
        Table::TimeIndex,
        Table::Sums,
        Table::NoRef,
        Table::Payload,
        Table::History,
        Table::TxIndex,
        Table::Receipt,
        Table::EvmTxs,
        Table::Code,
        Table::Storage,
    ] {
        assert_eq!(dump(&a, t), dump(&b, t), "table {t:?}");
    }
    assert!(!dump(&b, Table::History).is_empty(), "history of executed transactions is carried");

    let m = a.mine(&[tx.hashlow()]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    assert!(b.chain.nmain() > 11);
    main_states(&mut a, &mut b);
    assert_eq!(a.state_fingerprint(), b.state_fingerprint());
    assert_eq!(b.balance(&bob.address()), Nano(Nano::from_xdag(10).0 - a.params.min_gas.0));
    assert!(b.chain.state(&tx.hashlow()).unwrap().flags & flags::APPLIED != 0);

    // and the snapshot node can itself produce a snapshot for a third node
    let mut c = bootstrap(&mut b);
    let more = a.mine_n(2);
    follow(&mut a, &mut b, &more);
    follow(&mut a, &mut c, &more);
    main_states(&mut a, &mut c);
    assert_eq!(a.state_fingerprint(), c.state_fingerprint());
}

#[test]
fn an_executed_block_is_not_executed_again() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(8);
    let mut b = bootstrap(&mut a);
    assert!(b.chain.state(&pay.hashlow()).unwrap().flags & flags::MAIN_REF != 0, "execution state is carried");

    // a new main block references the old payout again
    let m = a.mine(&[pay.hashlow()]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    main_states(&mut a, &mut b);
    assert_eq!(accounts(&a), accounts(&b));
    assert_eq!(b.balance(&alice.address()), Nano(Nano::from_xdag(100).0 - a.params.min_gas.0));
}

/// xdagj accepts any block time since the era, so a wallet whose clock is far
/// behind produces a brand-new block with an old timestamp. A node that was
/// loaded from a snapshot must execute it like every other node.
#[test]
fn a_new_block_with_an_old_timestamp_is_applied_like_on_the_exporter() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let bob = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    let old_time = a.t(300);
    a.mine_n(10);

    let mut b = bootstrap(&mut a);
    let tx = transfer_at(&a, &alice, bob.address(), Nano::from_xdag(10), 1, old_time);
    assert!(a.import(&tx).is_imported());
    let m = a.mine(&[tx.hashlow()]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    assert_eq!(a.balance(&bob.address()), Nano(Nano::from_xdag(10).0 - a.params.min_gas.0), "executed on the exporter");
    main_states(&mut a, &mut b);
    assert_eq!(accounts(&a), accounts(&b), "and identically on the snapshot node");
}

#[test]
fn snapshot_node_never_reorganises_below_the_snapshot() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(8);
    let mut b = bootstrap(&mut a);
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

    let mut b = bootstrap(&mut a);
    assert_eq!(accounts(&a), accounts(&b));
    let m = a.mine(&[batch.hashlow()]);
    let rest = a.mine_n(2);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    main_states(&mut a, &mut b);
    assert_eq!(a.state_fingerprint(), b.state_fingerprint());
    assert_eq!(b.balance(&bob), Nano::from_xdag(7));
}

/// An xdagj node keeps only metadata and key material for the blocks it
/// inherited from its own snapshot. They must stay valid link targets and
/// spendable, and must not get lost in a later export, even once empty.
#[test]
fn blocks_inherited_without_data_stay_usable() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(6);
    let alice = KeyPair::random();
    // main block #2 is paid out completely
    let drain = a.payout(mains[1], alice.address(), Nano::from_xdag(1024));
    assert!(a.import(&drain).is_imported());
    a.mine(&[drain.hashlow()]);
    a.mine_n(3);
    assert_eq!(a.block_amount(&mains[1]), 0);

    // as an xdagj node would export it: #1 known by its public key, #2 and #3
    // by the block kept as key material; no RandomX state, no statuses
    let mut p = parse(&export(&mut a));
    p.header.rx = None;
    let pubkey = a.miner.public().serialize();
    for blk in p.blocks.iter_mut() {
        let SnapshotData::Full { raw, .. } = &blk.data else { panic!("the exporter has every block's data") };
        let raw = raw.clone();
        if blk.hashlow == mains[0] {
            blk.data = SnapshotData::Key(SnapshotKey::PublicKey(pubkey));
        } else if blk.hashlow == mains[1] || blk.hashlow == mains[2] {
            blk.data = SnapshotData::Key(SnapshotKey::RawBlock(raw));
        }
        blk.status = snapshot::STATUS_UNKNOWN;
    }
    let mut b = node_from(&a, &write(&p));
    assert!(b.chain.block(&mains[0]).unwrap().is_none() && b.chain.block(&mains[1]).unwrap().is_none());
    assert_eq!(dump(&a, Table::BlockState), dump(&b, Table::BlockState), "statuses are derived from the flags");

    // spend from #1 and #3, and reference the empty #2 once more
    let bob = KeyPair::random();
    let carol = KeyPair::random();
    let pay1 = a.payout(mains[0], bob.address(), Nano::from_xdag(10));
    let pay3 = a.payout(mains[2], carol.address(), Nano::from_xdag(20));
    assert!(a.import(&pay1).is_imported() && a.import(&pay3).is_imported());
    let m = a.mine(&[pay1.hashlow(), pay3.hashlow(), mains[1]]);
    let rest = a.mine_n(3);
    follow(&mut a, &mut b, &[m]);
    follow(&mut a, &mut b, &rest);
    main_states(&mut a, &mut b);
    assert_eq!(a.state_fingerprint(), b.state_fingerprint());
    assert_eq!(b.balance(&bob.address()), Nano(Nano::from_xdag(10).0 - a.params.min_gas.0));
    assert_eq!(b.balance(&carol.address()), Nano(Nano::from_xdag(20).0 - a.params.min_gas.0));

    // a snapshot of that node still knows the blocks it has no data for
    let mut c = bootstrap(&mut b);
    assert!(c.chain.info(&mains[1]).unwrap().is_some(), "the empty, data-less block is still known");
    let more = a.mine(&[mains[1]]);
    let rest = a.mine_n(2);
    follow(&mut a, &mut c, &[more]);
    follow(&mut a, &mut c, &rest);
    main_states(&mut a, &mut c);
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
    let bytes = export(&mut a);
    let b = node_from(&a, &bytes);
    assert!(same(b.chain.rx_schedule()));

    // a snapshot without it (as written by the xdagj exporter)
    let mut parsed = parse(&bytes);
    parsed.header.rx = None;
    let c = node_from(&a, &write(&parsed));
    assert!(same(c.chain.rx_schedule()));

    // and one whose schedule contradicts its main chain is refused
    let mut wrong = want.clone();
    wrong.fork_epoch = wrong.fork_epoch.map(|e| e + 1);
    parsed.header.rx = Some(wrong);
    let mut d = Sim::new(a.params.clone());
    assert!(snapshot::import(&mut d.chain, &mut &write(&parsed)[..]).is_err());
}

#[test]
fn a_failed_import_cannot_be_opened_as_a_chain() {
    let mut a = Sim::new(legacy_params());
    a.mine_n(6);
    let bytes = export(&mut a);
    let (db, guard) = Db::open_temporary().unwrap();
    let mut b = Sim::with_db(a.params.clone(), db.clone(), guard);

    // a snapshot of another network, or in another format version, is
    // refused before anything is written
    let mut other = bytes.clone();
    other[5] ^= 1;
    assert!(snapshot::import(&mut b.chain, &mut &other[..]).is_err());
    let mut old = bytes.clone();
    old[4] = 1;
    let err = snapshot::import(&mut b.chain, &mut &old[..]).expect_err("old format").to_string();
    assert!(err.contains("format 1 is not supported"), "{err}");
    assert!(reopen(db.clone(), &b).is_ok());

    // a truncated file fails half-way: the database must not pass for a chain
    assert!(snapshot::import(&mut b.chain, &mut &bytes[..bytes.len() - 10]).is_err());
    let err = reopen(db.clone(), &b).err().expect("refused").to_string();
    assert!(err.contains("did not finish"), "{err}");
}

#[test]
fn import_fails_cleanly_when_a_randomx_seed_block_is_missing() {
    let mut p = legacy_params();
    p.randomx.fork_height = 8;
    p.randomx.seed_epoch_blocks = 4;
    p.randomx.seed_lag = 2;
    let mut a = Sim::new(p);
    a.mine_n(23);
    let seed_height = a.chain.nmain() & !3;

    // the main-chain index lacks the newest seed height
    let mut parsed = parse(&export(&mut a));
    parsed.header.rx = None;
    parsed.mains.retain(|(h, _)| *h != seed_height);

    let (db, guard) = Db::open_temporary().unwrap();
    let mut b = Sim::with_db(a.params.clone(), db.clone(), guard);
    let err = snapshot::import(&mut b.chain, &mut &write(&parsed)[..]).expect_err("import fails").to_string();
    assert!(err.contains("RandomX schedule"), "{err}");
    // no chain without its RandomX seeds: candidates would be scored by sha256d
    assert!(reopen(db, &b).is_err());
}

#[test]
fn the_writer_rejects_sections_out_of_order() {
    let h = SnapshotHeader::default();
    let mut w = SnapshotWriter::new(Vec::new(), &h).unwrap();
    assert!(w.account(&Address::ZERO, &AccountRecord::default()).is_err(), "no section announced");
    w.section(1).unwrap();
    assert!(w.section(0).is_err(), "previous section incomplete");
    w.account(&Address::ZERO, &AccountRecord::default()).unwrap();
    assert!(w.account(&Address::ZERO, &AccountRecord::default()).is_err(), "more entries than announced");
    assert!(w.tables(0).is_err(), "blocks and main index missing");
    w.section(0).unwrap();
    w.section(0).unwrap();
    w.tables(1).unwrap();
    assert!(w.finish().is_err(), "a table was announced but not written");
}

/// The byte layout `tools/xdagj-exporter` writes, field by field in the order
/// of its `run` and `writeBlock` methods (the tool itself needs a JVM and an
/// xdagj database, so it cannot run here). Keep the two in step.
#[test]
fn the_layout_written_by_the_xdagj_exporter_imports() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(2);
    let tx = a.transfer(&alice, KeyPair::random().address(), Nano::from_xdag(10), 1, 200);
    assert!(a.import(&tx).is_imported());
    a.mine(&[tx.hashlow()]);
    a.mine_n(3);
    a.chain.commit(true).unwrap();
    let db = a.chain.db().clone();
    let nmain = a.chain.nmain();
    // the top as the tool falls back to it: the last main block
    let top = a.chain.main_at(nmain).unwrap().unwrap();
    let top_diff = a.chain.info(&top).unwrap().unwrap().difficulty;
    fn opt(o: &mut Vec<u8>, v: Option<&[u8]>) {
        match v {
            Some(v) => {
                o.push(1);
                o.extend_from_slice(v);
            }
            None => o.push(0),
        }
    }

    let mut o: Vec<u8> = b"XSNP".to_vec();
    o.push(2); // FORMAT
    o.push(a.params.network.id());
    o.extend_from_slice(&nmain.to_le_bytes());
    o.extend_from_slice(&top.0);
    o.extend_from_slice(&top_diff.to_be_bytes::<32>());
    o.extend_from_slice(&0u64.to_le_bytes()); // no RandomX schedule
    let accts = dump(&a, Table::Account);
    o.extend_from_slice(&(accts.len() as u64).to_le_bytes());
    for (k, v) in &accts {
        let r = AccountRecord::decode(v).unwrap();
        let Balance::Legacy(c_units) = r.balance else { panic!("legacy chain") };
        o.extend_from_slice(k);
        o.push(0); // balance in C units
        o.extend_from_slice(&c_units.to_le_bytes());
        o.extend_from_slice(&r.nonce.to_le_bytes());
        o.push(0); // no contract code
    }
    let infos = dump(&a, Table::BlockInfo);
    o.extend_from_slice(&(infos.len() as u64).to_le_bytes());
    for (k, v) in &infos {
        let info = BlockInfo::decode(v).unwrap();
        let st = db.get(Table::BlockState, k).unwrap().map(|b| BlockState::decode(&b).unwrap()).unwrap_or_default();
        o.extend_from_slice(k);
        o.extend_from_slice(&[0u8; 32]); // hash: recomputed from the data
        o.extend_from_slice(&info.time.to_le_bytes());
        o.push(info.flags | st.flags);
        o.push(snapshot::STATUS_UNKNOWN);
        o.extend_from_slice(&st.height.to_le_bytes());
        o.extend_from_slice(&info.difficulty.to_be_bytes::<32>());
        opt(&mut o, info.max_diff_link.as_ref().map(|h| &h.0[..]));
        o.extend_from_slice(&st.amount.to_le_bytes());
        o.extend_from_slice(&st.fee.0.to_le_bytes());
        opt(&mut o, info.remark.as_ref().map(|r| &r[..]));
        o.push(3); // kind: block data
        o.extend_from_slice(&db.get(Table::BlockRaw, k).unwrap().unwrap());
        opt(&mut o, st.ref_.as_ref().map(|h| &h.0[..]));
    }
    let index = dump(&a, Table::MainHeight);
    o.extend_from_slice(&(index.len() as u64).to_le_bytes());
    for (k, v) in &index {
        o.extend_from_slice(&u64::from_be_bytes(k[..].try_into().unwrap()).to_le_bytes());
        o.extend_from_slice(v);
    }
    o.push(0); // no execution-record tables

    let mut b = node_from(&a, &o);
    for t in [Table::Account, Table::BlockInfo, Table::BlockState, Table::BlockRaw, Table::MainHeight, Table::TimeIndex, Table::Sums, Table::NoRef] {
        assert_eq!(dump(&a, t), dump(&b, t), "table {t:?}");
    }
    let next = a.mine_n(3);
    follow(&mut a, &mut b, &next);
    main_states(&mut a, &mut b);
    assert_eq!(a.state_fingerprint(), b.state_fingerprint());
}

#[test]
fn describing_a_snapshot_gives_counts_supply_and_a_stable_account_digest() {
    let mut a = Sim::new(legacy_params());
    let mains = a.mine_n(4);
    let alice = KeyPair::random();
    let pay = a.payout(mains[0], alice.address(), Nano::from_xdag(100));
    assert!(a.import(&pay).is_imported());
    a.mine(&[pay.hashlow()]);
    a.mine_n(3);
    let s1 = snapshot::describe(&export(&mut a)[..]).unwrap();
    assert_eq!(s1.accounts as usize, accounts(&a).len());
    assert_eq!(s1.blocks as usize, dump(&a, Table::BlockInfo).len());
    assert_eq!(s1.blocks_with_data, s1.blocks);
    assert_eq!(s1.mains, a.chain.nmain());
    assert!(s1.accounts_sorted);
    // nothing is created or lost: accounts + block balances = rewards paid so far
    let rewards = xdag_chain::fees::reward(1, &a.params).0 as i128 * a.chain.nmain() as i128;
    assert_eq!(s1.legacy_nano as i128 + s1.block_nano, rewards);

    // a node loaded from the snapshot exports the same thing
    let mut b = bootstrap(&mut a);
    assert_eq!(snapshot::describe(&export(&mut b)[..]).unwrap(), s1);

    // the digest follows the account state
    let tx = a.transfer(&alice, KeyPair::random().address(), Nano::from_xdag(1), 1, 100);
    assert!(a.import(&tx).is_imported());
    a.mine(&[tx.hashlow()]);
    a.mine_n(2);
    assert_ne!(snapshot::describe(&export(&mut a)[..]).unwrap().accounts_digest, s1.accounts_digest);
}
