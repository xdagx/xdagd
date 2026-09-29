//! Smart contracts end to end: EVM transactions inside Nova batch blocks,
//! executed when the main block is applied, rolled back on reorg.

use std::sync::Arc;

use xdag_chain::records::TxStatus;
use xdag_chain::testkit::*;
use xdag_evm::{tx, RevmEngine};
use xdag_types::nova::NovaTx;
use xdag_types::{KeyPair, Nano};

const WEI: u128 = 1_000_000_000_000_000_000;

fn counter_initcode() -> Vec<u8> {
    let mut runtime = vec![0x60, 0x01, 0x36, 0x14, 0x60, 0x13, 0x57, 0x60, 0x00, 0x54, 0x60, 0x01, 0x01, 0x60, 0x00, 0x55, 0x00];
    while runtime.len() < 0x13 {
        runtime.push(0x00);
    }
    runtime.extend_from_slice(&[0x5b, 0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    let mut init = vec![0x60, runtime.len() as u8, 0x80, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x00, 0xf3, 0x00];
    init.extend_from_slice(&runtime);
    init
}

fn sim() -> (Sim, u64) {
    let s = Sim::with_evm(nova_params(), Arc::new(RevmEngine::new()));
    let cid = s.params.nova.as_ref().unwrap().chain_id;
    (s, cid)
}

fn run_batch(s: &mut Sim, txs: Vec<Vec<u8>>) -> xdag_types::HashLow {
    let entries: Vec<NovaTx> = txs.into_iter().map(NovaTx::Evm).collect();
    let (b, payload) = s.chain.batch_block(&s.miner.clone(), &entries, s.t(100)).unwrap();
    let o = s.import_with(&b, Some(payload));
    assert!(o.is_imported(), "{o:?}");
    b.hashlow()
}

#[test]
fn deploy_call_and_receipts() {
    let (mut s, cid) = sim();
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let pay = s.payout(mains[0], alice.evm_address(), Nano::from_xdag(100));
    assert!(s.import(&pay).is_imported());
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    let bal = xdag_chain::query::balance_wei(s.chain.db(), &alice.evm_address()).unwrap();
    assert_eq!(bal, (100u128 * 1_000_000_000 - 100_000_000) * 1_000_000_000);

    let deploy = tx::sign_legacy(&alice, cid, 0, 1_000_000_000, 300_000, None, 0, &counter_initcode());
    let deploy_hash = tx::decode(&deploy).unwrap().hash;
    let b1 = run_batch(&mut s, vec![deploy]);
    s.mine(&[b1]);
    s.mine_n(2);
    let loc = xdag_chain::query::tx_location(s.chain.db(), &deploy_hash).unwrap().expect("deploy indexed");
    assert_eq!(loc.status, TxStatus::Applied);
    let rc = xdag_chain::query::receipt(s.chain.db(), &deploy_hash).unwrap().unwrap();
    assert!(rc.success);
    let contract = rc.contract_address.unwrap();
    let acct = xdag_chain::query::account(s.chain.db(), &contract).unwrap().unwrap();
    assert!(acct.code_hash.is_some());

    let calls: Vec<Vec<u8>> =
        (1..=2).map(|n| tx::sign_eip1559(&alice, cid, n, 1_000_000_000, 2_000_000_000, 100_000, Some(contract), 0, b"")).collect();
    let b2 = run_batch(&mut s, calls);
    s.mine(&[b2]);
    s.mine_n(2);
    let slot = xdag_chain::query::storage_at(s.chain.db(), &contract, &[0u8; 32]).unwrap();
    assert_eq!(slot[31], 2);
    assert_eq!(xdag_chain::query::account(s.chain.db(), &alice.evm_address()).unwrap().unwrap().nonce, 3);
    // fees went to the coinbase of the applying main block (the miner)
    let miner_wei = xdag_chain::query::balance_wei(s.chain.db(), &s.miner.address()).unwrap();
    assert!(miner_wei > 0);
    // alice paid exactly value(0) + fees
    let spent = bal - xdag_chain::query::balance_wei(s.chain.db(), &alice.evm_address()).unwrap();
    assert_eq!(spent, miner_wei);
    let _ = WEI;
}

#[test]
fn evm_state_is_rolled_back_on_reorg() {
    let (mut s, cid) = sim();
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let pay = s.payout(mains[0], alice.evm_address(), Nano::from_xdag(100));
    s.import(&pay);
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    let deploy = tx::sign_legacy(&alice, cid, 0, 1_000_000_000, 300_000, None, 0, &counter_initcode());
    let deploy_hash = tx::decode(&deploy).unwrap().hash;
    let b1 = run_batch(&mut s, vec![deploy]);
    s.mine(&[b1]);
    s.mine_n(2);
    let contract = xdag_chain::query::receipt(s.chain.db(), &deploy_hash).unwrap().unwrap().contract_address.unwrap();
    let base_top = s.chain.top().unwrap();
    let before = s.state_fingerprint();

    // branch A: increments the counter
    let call = tx::sign_eip1559(&alice, cid, 1, 1_000_000_000, 1_000_000_000, 100_000, Some(contract), 0, b"");
    let b2 = run_batch(&mut s, vec![call]);
    let fork = s.epoch;
    let a1 = s.candidate_at(fork, Some(base_top), &s.miner.clone(), &[b2], 0);
    let a2 = s.candidate_at(fork + 1, Some(a1.hashlow()), &s.miner.clone(), &[], 0);
    let a3 = s.candidate_at(fork + 2, Some(a2.hashlow()), &s.miner.clone(), &[], 0);
    for b in [&a1, &a2, &a3] {
        assert!(s.import(b).is_imported());
    }
    s.clock.set(s.epochs().start_of_epoch(fork + 10));
    s.chain.tick().unwrap();
    assert_eq!(xdag_chain::query::storage_at(s.chain.db(), &contract, &[0u8; 32]).unwrap()[31], 1);

    // branch B: heavier, without the call
    let other = KeyPair::random();
    let mut prev = base_top;
    for i in 0..3 {
        let b = s.candidate_at(fork + i, Some(prev), &other, &[], 16);
        prev = b.hashlow();
        assert!(s.import(&b).is_imported());
    }
    s.chain.tick().unwrap();
    assert_eq!(s.chain.top(), Some(prev));
    assert_eq!(xdag_chain::query::storage_at(s.chain.db(), &contract, &[0u8; 32]).unwrap()[31], 0, "increment undone");
    assert_eq!(xdag_chain::query::account(s.chain.db(), &alice.evm_address()).unwrap().unwrap().nonce, 1);
    // everything that existed before the fork is byte-identical
    let after = s.state_fingerprint();
    for (k, v) in &before {
        let now = after.iter().find(|(ka, _)| ka == k).map(|(_, v)| v);
        if k[0] == xdag_storage::Table::MainHeight as u8 {
            continue; // heights above the fork now point at branch B
        }
        if k[0] == xdag_storage::Table::BlockState as u8 || k[0] == xdag_storage::Table::Account as u8 {
            // accounts touched only by branch B blocks (the other miner) may differ
            if now.is_none() {
                panic!("entry {} vanished", hex::encode(k));
            }
            continue;
        }
        assert_eq!(now, Some(v), "entry {} differs", hex::encode(k));
    }
    let alice_rec = xdag_chain::query::account(s.chain.db(), &alice.evm_address()).unwrap().unwrap();
    let alice_before = before
        .iter()
        .find(|(k, _)| k[0] == xdag_storage::Table::Account as u8 && k[1..] == alice.evm_address().0)
        .map(|(_, v)| xdag_chain::records::AccountRecord::decode(v).unwrap())
        .unwrap();
    assert_eq!(alice_rec, alice_before, "alice's account restored exactly");
}
