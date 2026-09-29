//! Real TCP connections between in-process nodes with a mock chain.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use xdag_net::{ChainHandle, ConnId, Identity, NetConfig, NetHandle, Stats};
use xdag_types::{HashLow, KeyPair};

#[derive(Default)]
struct MockChain {
    blocks: Mutex<HashMap<HashLow, Vec<u8>>>,
    received: Mutex<Vec<(HashLow, ConnId, i32)>>,
    net: Mutex<Option<NetHandle>>,
}

impl ChainHandle for MockChain {
    fn stats(&self) -> Stats {
        Stats::default()
    }
    fn latest_main(&self) -> i64 {
        0
    }
    fn last_main_time(&self) -> u64 {
        0
    }
    fn is_synced(&self) -> bool {
        true
    }
    fn on_block(&self, raw: Vec<u8>, _payload: Option<Vec<u8>>, from: ConnId, ttl: i32, _sync: bool) {
        let h = xdag_types::BlockHash::of_raw(&raw).hashlow();
        let new = self.blocks.lock().insert(h, raw.clone()).is_none();
        self.received.lock().push((h, from, ttl));
        if new {
            if let Some(n) = self.net.lock().clone() {
                n.broadcast_block(&raw, None, ttl, Some(from));
            }
        }
    }
    fn on_txs(&self, _txs: Vec<(u8, Vec<u8>)>, _from: ConnId) {}
    fn on_payload(&self, _h: HashLow, _p: Vec<u8>, _from: ConnId) {}
    fn get_block(&self, h: &HashLow) -> Option<(Vec<u8>, u8, Option<Vec<u8>>)> {
        self.blocks.lock().get(h).map(|b| (b.clone(), 0, None))
    }
    fn blocks_in_range(&self, _s: u64, _e: u64, _l: usize) -> Vec<(Vec<u8>, u8)> {
        vec![]
    }
    fn sums(&self, _s: u64, _e: u64) -> Option<[u8; 256]> {
        Some([0u8; 256])
    }
    fn get_payload(&self, _h: &HashLow) -> Option<Vec<u8>> {
        None
    }
}

async fn node(port: u16, seeds: Vec<String>) -> (NetHandle, Arc<MockChain>) {
    let chain = Arc::new(MockChain::default());
    let cfg = NetConfig {
        network: 2,
        listen: format!("127.0.0.1:{port}").parse().unwrap(),
        seeds,
        allow_private: true,
        max_outbound: 4,
        ..NetConfig::default()
    };
    let h = xdag_net::start(cfg, Identity::new(KeyPair::random()), chain.clone()).await.unwrap();
    *chain.net.lock() = Some(h.clone());
    (h, chain)
}

async fn wait_for(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    for _ in 0..secs * 10 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn a_block(tag: u8) -> Vec<u8> {
    let k = KeyPair::random();
    let mut t = xdag_types::BlockTemplate::new(xdag_types::FieldType::HeadTest, 0x16900000000 + tag as u64);
    t.sign_out = Some(k);
    t.build().unwrap().raw().to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_relay_discovery() {
    let base = 39000 + (std::process::id() % 1000) as u16 * 3;
    let (a, ca) = node(base, vec![]).await;
    let (b, cb) = node(base + 1, vec![format!("127.0.0.1:{base}")]).await;
    assert!(wait_for(|| a.peers().len() == 1 && b.peers().len() == 1, 10).await, "A-B handshake");
    // C only knows B; it must discover A through peer exchange
    let (c, cc) = node(base + 2, vec![format!("127.0.0.1:{}", base + 1)]).await;
    assert!(wait_for(|| c.peers().len() == 2, 40).await, "C discovers A via B: {:?}", c.peers());
    assert!(c.peers().iter().all(|p| p.nova));

    // relay: A → (B, C) with TTL
    let blk = a_block(1);
    let h = xdag_types::BlockHash::of_raw(&blk).hashlow();
    ca.blocks.lock().insert(h, blk.clone());
    a.broadcast_block(&blk, None, 5, None);
    assert!(wait_for(|| cb.blocks.lock().contains_key(&h) && cc.blocks.lock().contains_key(&h), 10).await);

    // request/response: C asks for a block only A has
    let blk2 = a_block(2);
    let h2 = xdag_types::BlockHash::of_raw(&blk2).hashlow();
    ca.blocks.lock().insert(h2, blk2);
    c.request_block(h2, None, true);
    assert!(wait_for(|| cc.blocks.lock().contains_key(&h2), 10).await);
    let _ = (&b, &cb);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_and_wrong_network_are_rejected() {
    use tokio::io::AsyncWriteExt;
    let base = 41000 + (std::process::id() % 1000) as u16 * 2;
    let (a, _ca) = node(base, vec![]).await;
    // raw garbage
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", base)).await.unwrap();
    let _ = s.write_all(&[0xde; 64]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.peers().len(), 0);
    // a node of another network
    let chain = Arc::new(MockChain::default());
    let cfg = NetConfig {
        network: 0,
        listen: format!("127.0.0.1:{}", base + 1).parse().unwrap(),
        seeds: vec![format!("127.0.0.1:{base}")],
        allow_private: true,
        ..NetConfig::default()
    };
    let other = xdag_net::start(cfg, Identity::new(KeyPair::random()), chain).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(other.peers().len(), 0);
    assert_eq!(a.peers().len(), 0);
}
