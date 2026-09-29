//! RPC responses over a simulated chain (shape compatibility with xdagj and
//! basic Ethereum semantics).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use xdag_chain::testkit::{nova_params, Sim};
use xdag_rpc::{dispatch, Backend, Status};
use xdag_storage::Db;
use xdag_types::nova::NovaTx;
use xdag_types::{Address, HashLow, KeyPair, Nano, NetworkParams};

struct Mock {
    sim: Mutex<Sim>,
    db: Db,
    params: NetworkParams,
}

impl Backend for Mock {
    fn params(&self) -> &NetworkParams {
        &self.params
    }
    fn db(&self) -> &Db {
        &self.db
    }
    fn status(&self) -> Status {
        let s = self.sim.lock();
        Status { nmain: s.chain.nmain(), synced: true, ..Default::default() }
    }
    fn submit_block(&self, _raw: Vec<u8>) -> Result<HashLow, String> {
        Err("read-only mock".into())
    }
    fn submit_nova_tx(&self, _tx: NovaTx) -> Result<[u8; 32], String> {
        Err("read-only mock".into())
    }
    fn evm_call(&self, _: Address, _: Option<Address>, _: Vec<u8>, _: u128, _: Option<u64>) -> Result<xdag_chain::EvmExecResult, String> {
        Err("no evm".into())
    }
    fn peers(&self) -> Vec<xdag_net::PeerInfo> {
        vec![]
    }
    fn coinbase(&self) -> Option<Address> {
        Some(self.sim.lock().miner.address())
    }
    fn next_nonce(&self, _a: &Address, _evm: bool) -> u64 {
        1
    }
    fn wallet_transfer(&self, _: Option<Address>, _: Address, _: Nano, _: &str, _: &str) -> Result<Vec<String>, String> {
        Err("no wallet".into())
    }
    fn client_version(&self) -> String {
        "test".into()
    }
    fn pending(&self) -> Vec<([u8; 32], Address, u64, bool)> {
        vec![]
    }
}

fn setup() -> (Arc<Mock>, KeyPair, Vec<HashLow>) {
    let p = nova_params();
    let mut s = Sim::new(p.clone());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    let pay = s.payout(mains[0], alice.address(), Nano::from_xdag(7));
    assert!(s.import(&pay).is_imported());
    s.mine(&[pay.hashlow()]);
    s.mine_n(2);
    let db = s.chain.db().clone();
    (Arc::new(Mock { sim: Mutex::new(s), db, params: p }), alice, mains)
}

fn call(b: &dyn Backend, m: &str, p: Value) -> Value {
    dispatch(b, m, &p).unwrap_or_else(|e| panic!("{m}: {} {}", e.code, e.message))
}

#[test]
fn xdag_methods_have_xdagj_shapes() {
    let (b, alice, mains) = setup();
    let n: u64 = call(&*b, "xdag_blockNumber", json!([])).as_str().unwrap().parse().unwrap();
    assert!(n >= 5);
    assert_eq!(call(&*b, "xdag_getBalance", json!([alice.address().to_base58()])), json!("6.900000000"));
    assert_eq!(call(&*b, "xdag_netType", json!([])), json!("devnet"));
    let blk = call(&*b, "xdag_getBlockByNumber", json!(["1", "1"]));
    assert_eq!(blk["state"], "Main");
    assert_eq!(blk["type"], "Main");
    assert_eq!(blk["height"], 1);
    assert_eq!(blk["address"], mains[0].to_legacy_address());
    assert!(blk["refs"].as_array().unwrap().iter().any(|r| r["direction"] == 2));
    // the payout spent 7 XDAG from main block #1
    assert_eq!(blk["balance"], "1017.000000000");
    let by_hash = call(&*b, "xdag_getBlockByHash", json!([mains[0].to_legacy_address(), "0"]));
    assert_eq!(by_hash["hash"], blk["hash"]);
    let acct = call(&*b, "xdag_getBlockByHash", json!([alice.address().to_base58(), "1"]));
    assert_eq!(acct["type"], "Wallet");
    assert_eq!(acct["transactions"].as_array().unwrap().len(), 1);
    let hist = call(&*b, "xdag_getHistory", json!([alice.address().to_base58()]));
    assert_eq!(hist[0]["status"], "applied");
    assert_eq!(hist[0]["amount"], "6.900000000");
    let reward = call(&*b, "xdag_getRewardByNumber", json!(["1"]));
    assert_eq!(reward, "1024.000000000");
    // xdagj returns strings (not errors) from sendRawTransaction
    let r = call(&*b, "xdag_sendRawTransaction", json!(["00"]));
    assert!(r.as_str().unwrap().starts_with("INVALID_BLOCK"));
}

#[test]
fn eth_methods() {
    let (b, alice, _) = setup();
    assert_eq!(call(&*b, "eth_chainId", json!([])), json!("0x7866"));
    assert_eq!(call(&*b, "net_version", json!([])), json!("30822"));
    let bal = call(&*b, "eth_getBalance", json!([alice.address().to_hex(), "latest"]));
    assert_eq!(bal, json!(format!("{:#x}", 6_900_000_000u128 * 1_000_000_000)));
    let blk = call(&*b, "eth_getBlockByNumber", json!(["0x1", false]));
    assert_eq!(blk["number"], "0x1");
    assert_eq!(blk["transactions"], json!([]));
    let blk2 = call(&*b, "eth_getBlockByNumber", json!(["0x2", false]));
    assert_eq!(blk2["parentHash"], blk["hash"]);
    let by_hash = call(&*b, "eth_getBlockByHash", json!([blk["hash"], false]));
    assert_eq!(by_hash["number"], "0x1");
    assert_eq!(call(&*b, "eth_getCode", json!([alice.address().to_hex(), "latest"])), json!("0x"));
    assert!(dispatch(&*b, "eth_nonexistent", &json!([])).is_err());
    let logs = call(&*b, "eth_getLogs", json!([{"fromBlock": "0x1", "toBlock": "latest"}]));
    assert_eq!(logs, json!([]));
}

#[test]
fn history_pages_do_not_skip_entries_of_the_same_main_block() {
    let p = nova_params();
    let mut s = Sim::new(p.clone());
    let mains = s.mine_n(3);
    let alice = KeyPair::random();
    // five payouts to alice, all executed by the same main block
    let pays: Vec<_> = (0..5u64)
        .map(|i| {
            let mut t = xdag_types::BlockTemplate::new(p.header_field(), s.t(100 + i));
            let amt = Nano::from_xdag(1 + i).to_camount_legacy();
            t.links.push((xdag_types::FieldType::In, xdag_types::LinkTarget::Block(mains[0]), amt));
            t.links.push((xdag_types::FieldType::Output, xdag_types::LinkTarget::Address(alice.address()), amt));
            t.sign_out = Some(s.miner.clone());
            t.include_out_pubkey = true;
            let b = xdag_chain::builder::seal_template(&p, t).unwrap();
            assert!(s.import(&b).is_imported());
            b.hashlow()
        })
        .collect();
    s.mine(&pays);
    s.mine_n(2);
    let db = s.chain.db().clone();
    let b = Arc::new(Mock { sim: Mutex::new(s), db, params: p });

    let addr = alice.address().to_base58();
    let all = call(&*b, "xdag_getHistory", json!([addr, null, 100]));
    assert_eq!(all.as_array().unwrap().len(), 5);
    let mut seen = vec![];
    let mut cursor = Value::Null;
    loop {
        let page = call(&*b, "xdag_getHistory", json!([addr, cursor, 2]));
        let page = page.as_array().unwrap().clone();
        if page.is_empty() {
            break;
        }
        cursor = page.last().unwrap()["cursor"].clone();
        seen.extend(page.into_iter().map(|e| e["cursor"].as_str().unwrap().to_string()));
    }
    let want: Vec<String> = all.as_array().unwrap().iter().map(|e| e["cursor"].as_str().unwrap().to_string()).collect();
    assert_eq!(seen, want, "cursor paging returns every entry exactly once, newest first");
}
