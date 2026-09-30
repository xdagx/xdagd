//! Real TCP connections between in-process nodes with a mock chain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Times of further blocks this node claims to have (sums only).
    history: Mutex<Vec<u64>>,
    last_main_time: AtomicU64,
    syncing: AtomicBool,
    range_requests: Mutex<Vec<(u64, u64)>>,
    closed: AtomicBool,
    backlog: AtomicU64,
}

fn block_time(raw: &[u8]) -> u64 {
    u64::from_le_bytes(raw[16..24].try_into().unwrap())
}

impl ChainHandle for MockChain {
    fn stats(&self) -> Stats {
        Stats::default()
    }
    fn latest_main(&self) -> i64 {
        0
    }
    fn last_main_time(&self) -> u64 {
        self.last_main_time.load(Ordering::SeqCst)
    }
    fn is_synced(&self) -> bool {
        !self.syncing.load(Ordering::SeqCst)
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
    fn blocks_in_range(&self, s: u64, e: u64, _l: usize) -> Vec<(Vec<u8>, u8)> {
        self.range_requests.lock().push((s, e));
        self.blocks.lock().values().filter(|b| (s..e).contains(&block_time(b))).map(|b| (b.clone(), 0)).collect()
    }
    fn sums(&self, s: u64, e: u64) -> Option<[u8; 256]> {
        let sub = (e - s) / 16;
        let mut out = [0u8; 256];
        let blocks = self.blocks.lock();
        let history = self.history.lock();
        for t in blocks.values().map(|b| block_time(b)).chain(history.iter().copied()).filter(|t| (s..e).contains(t)) {
            let i = ((t - s) / sub) as usize;
            let sum = u64::from_le_bytes(out[i * 16..i * 16 + 8].try_into().unwrap()).wrapping_add(t);
            let size = u64::from_le_bytes(out[i * 16 + 8..i * 16 + 16].try_into().unwrap()) + 512;
            out[i * 16..i * 16 + 8].copy_from_slice(&sum.to_le_bytes());
            out[i * 16 + 8..i * 16 + 16].copy_from_slice(&size.to_le_bytes());
        }
        Some(out)
    }
    fn get_payload(&self, _h: &HashLow) -> Option<Vec<u8>> {
        None
    }
    fn open_network(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }
    fn import_backlog(&self) -> usize {
        self.backlog.load(Ordering::SeqCst) as usize
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
    block_at(0x16900000000 + tag as u64)
}

fn block_at(time: u64) -> Vec<u8> {
    let k = KeyPair::random();
    let mut t = xdag_types::BlockTemplate::new(xdag_types::FieldType::HeadTest, time);
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

/// A node bootstrapped from a pruned snapshot has none of the old blocks its
/// peer still serves. It must still find the blocks after its last main block
/// instead of spending every sync round on the old ranges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_reaches_recent_blocks_despite_a_long_history_on_the_peer() {
    let base_port = 43000 + (std::process::id() % 1000) as u16 * 2;
    let era = 0x16900000000u64;
    // the peer: 600 old blocks, one per 2^24 ticks (about 114 days), and one new block
    let (_a, ca) = node(base_port, vec![]).await;
    *ca.history.lock() = (0..600u64).map(|k| era + (k << 24)).collect();
    let last_main = era + (600 << 24);
    let new_block = block_at(last_main + (3 << 20) + 7);
    let h = xdag_types::BlockHash::of_raw(&new_block).hashlow();
    ca.blocks.lock().insert(h, new_block);

    // the snapshot node: no old blocks at all, main chain ends at `last_main`
    let chain = Arc::new(MockChain::default());
    chain.last_main_time.store(last_main, Ordering::SeqCst);
    chain.syncing.store(true, Ordering::SeqCst);
    let cfg = NetConfig {
        network: 2,
        listen: format!("127.0.0.1:{}", base_port + 1).parse().unwrap(),
        seeds: vec![format!("127.0.0.1:{base_port}")],
        allow_private: true,
        max_outbound: 4,
        ..NetConfig::default()
    };
    // while its import queue is long, the node does not fetch more
    chain.backlog.store(1_000_000, Ordering::SeqCst);
    let b = xdag_net::start(cfg, Identity::new(KeyPair::random()), chain.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(ca.range_requests.lock().is_empty(), "no range is requested while the import backlog is large");
    chain.backlog.store(0, Ordering::SeqCst);

    assert!(wait_for(|| chain.blocks.lock().contains_key(&h), 20).await, "the new block is fetched by range sync");
    let asked = ca.range_requests.lock().clone();
    assert!(!asked.is_empty() && asked.iter().all(|(s, _)| s + (1 << 20) > last_main), "only windows after the last main block: {asked:?}");
    let _ = b;
}

/// While the chain reports the network as closed (xdagj rules in force), the
/// node talks to its seeds and allow-listed addresses only, in both
/// directions, and never bans those: they are the operator's choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_network_only_talks_to_configured_peers() {
    use tokio::io::AsyncReadExt;
    let port = 45000 + (std::process::id() % 1000) as u16 * 3;
    let chain = Arc::new(MockChain::default());
    chain.closed.store(true, Ordering::SeqCst);
    let cfg = NetConfig {
        network: 2,
        listen: format!("127.0.0.1:{port}").parse().unwrap(),
        allow: vec!["127.0.0.1".parse().unwrap()],
        allow_private: true,
        ..NetConfig::default()
    };
    let a = xdag_net::start(cfg, Identity::new(KeyPair::random()), chain.clone()).await.unwrap();

    // bytes the listener sends to a fresh connection coming from `src`
    async fn greeting(src: &str, port: u16) -> usize {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind(format!("{src}:0").parse().unwrap()).unwrap();
        let mut s = sock.connect(format!("127.0.0.1:{port}").parse().unwrap()).await.unwrap();
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf)).await.ok().and_then(|r| r.ok()).unwrap_or(0)
    }
    assert!(greeting("127.0.0.1", port).await > 0, "an allowed address is greeted with the handshake");
    assert_eq!(greeting("127.0.0.2", port).await, 0, "a stranger is dropped without a word");

    // outbound: unknown addresses are not dialled either
    let stranger = tokio::net::TcpListener::bind(("127.0.0.3", port + 1)).await.unwrap();
    a.connect(format!("127.0.0.3:{}", port + 1).parse().unwrap());
    assert!(tokio::time::timeout(Duration::from_millis(800), stranger.accept()).await.is_err(), "not dialled while closed");

    // a configured peer is not banned, whatever its score
    let (b, _cb) = node(port + 2, vec![format!("127.0.0.1:{port}")]).await;
    assert!(wait_for(|| a.peers().len() == 1, 10).await, "the allowed peer connects");
    a.penalize(a.peers()[0].conn, 1000, "test");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.peers().len(), 1, "still connected");
    assert!(greeting("127.0.0.1", port).await > 0, "and its address is still accepted");

    // once the network opens (Nova rules in force) strangers are welcome
    chain.closed.store(false, Ordering::SeqCst);
    assert!(greeting("127.0.0.2", port).await > 0);
    a.connect(format!("127.0.0.3:{}", port + 1).parse().unwrap());
    assert!(tokio::time::timeout(Duration::from_secs(3), stranger.accept()).await.is_ok());
    let _ = b;
}

/// A time window can hold more blocks than the send queue has room for: the
/// peer that asked for it must still get every one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_range_is_delivered_completely() {
    let base_port = 47000 + (std::process::id() % 1000) as u16 * 2;
    let last_main = 0x16900000000u64;
    let n = 20_000usize;
    let (_a, ca) = node(base_port, vec![]).await;
    {
        let mut blocks = ca.blocks.lock();
        for i in 0..n {
            let raw = block_at(last_main + 1000 + (i as u64 % 5000));
            blocks.insert(xdag_types::BlockHash::of_raw(&raw).hashlow(), raw);
        }
    }
    let chain = Arc::new(MockChain::default());
    chain.last_main_time.store(last_main, Ordering::SeqCst);
    chain.syncing.store(true, Ordering::SeqCst);
    let cfg = NetConfig {
        network: 2,
        listen: format!("127.0.0.1:{}", base_port + 1).parse().unwrap(),
        seeds: vec![format!("127.0.0.1:{base_port}")],
        allow_private: true,
        max_outbound: 4,
        ..NetConfig::default()
    };
    let b = xdag_net::start(cfg, Identity::new(KeyPair::random()), chain.clone()).await.unwrap();
    let complete = wait_for(|| chain.blocks.lock().len() == n, 60).await;
    assert!(complete, "received {} of {n} blocks", chain.blocks.lock().len());
    let _ = b;
}
