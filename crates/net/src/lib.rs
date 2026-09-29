//! XDAG P2P networking.
//!
//! Wire-compatible with xdagj (frames, handshake, block and sync messages) so
//! the node can join an existing network, but **without a whitelist**:
//!
//! * anyone may connect (bounded by per-IP / per-subnet / total limits);
//! * peers are found through configured seeds and peer exchange
//!   (`GET_PEERS`/`PEERS`, an extension xdagj simply ignores);
//! * every peer is rate limited and scored; misbehaving IPs are banned;
//! * requests are bounded (xdagj served `BLOCKS_REQUEST` over any time range,
//!   a trivial denial of service once the network is open);
//! * statistics sent by peers are never trusted for consensus or for deciding
//!   that synchronisation is finished.

pub mod frame;
pub mod handshake;
pub mod message;
pub mod peer;
pub mod ratelimit;
pub mod sync;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use xdag_types::HashLow;

pub use handshake::Identity;
pub use message::{Message, Stats};

pub type ConnId = u64;

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub network: u8,
    pub network_version: i16,
    pub listen: SocketAddr,
    /// Port advertised in handshakes (defaults to the listen port).
    pub advertise_port: Option<u16>,
    pub seeds: Vec<String>,
    pub client_id: String,
    pub node_tag: String,
    pub generate_block: bool,
    pub max_inbound: usize,
    pub max_outbound: usize,
    pub max_inbound_per_ip: usize,
    pub max_inbound_per_subnet: usize,
    pub max_frame_body: usize,
    pub max_packet: usize,
    pub handshake_expiry_ms: i64,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    pub ban_duration: Duration,
    /// TTL used when broadcasting our own blocks (xdagj default 5).
    pub ttl: i32,
    /// Allow private/loopback addresses from peer exchange (devnets).
    pub allow_private: bool,
    /// Optional local deny list (IPs); there is no allow list.
    pub deny: Vec<IpAddr>,
    pub peers_file: Option<PathBuf>,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            network: 0,
            network_version: 0,
            listen: "0.0.0.0:8001".parse().unwrap(),
            advertise_port: None,
            seeds: vec![],
            client_id: format!("xdag-rs/v{}", env!("CARGO_PKG_VERSION")),
            node_tag: "xdag-rs".into(),
            generate_block: false,
            max_inbound: 128,
            max_outbound: 16,
            max_inbound_per_ip: 4,
            max_inbound_per_subnet: 16,
            max_frame_body: 128 * 1024,
            max_packet: 16 * 1024 * 1024,
            handshake_expiry_ms: 5 * 60 * 1000,
            handshake_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(180),
            ban_duration: Duration::from_secs(3600),
            ttl: 5,
            allow_private: false,
            deny: vec![],
            peers_file: None,
        }
    }
}

/// What the network layer needs from the node. Implementations must not block
/// for long (they are called from async tasks); heavy work should be queued.
pub trait ChainHandle: Send + Sync + 'static {
    fn stats(&self) -> Stats;
    fn latest_main(&self) -> i64;
    /// Time of the newest main block (sync windows start after it).
    fn last_main_time(&self) -> u64;
    fn is_synced(&self) -> bool;
    /// A block arrived. `sync` is true for requested/SYNC_BLOCK deliveries.
    fn on_block(&self, raw: Vec<u8>, payload: Option<Vec<u8>>, from: ConnId, ttl: i32, sync: bool);
    fn on_txs(&self, txs: Vec<(u8, Vec<u8>)>, from: ConnId);
    /// A Nova payload arrived (answer to GET_PAYLOAD or sent after a SYNC_BLOCK).
    fn on_payload(&self, h: HashLow, payload: Vec<u8>, from: ConnId);
    /// Raw block, xdagj execution state and Nova payload.
    fn get_block(&self, h: &HashLow) -> Option<(Vec<u8>, u8, Option<Vec<u8>>)>;
    fn blocks_in_range(&self, start: u64, end: u64, limit: usize) -> Vec<(Vec<u8>, u8)>;
    fn sums(&self, start: u64, end: u64) -> Option<[u8; 256]>;
    fn get_payload(&self, h: &HashLow) -> Option<Vec<u8>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub conn: ConnId,
    pub peer_id: String,
    pub addr: String,
    pub listen_port: u16,
    pub inbound: bool,
    pub client_id: String,
    pub node_tag: String,
    pub nova: bool,
    pub latest_block: i64,
    pub score: i64,
    pub connected_secs: u64,
}

pub(crate) struct PeerHandle {
    pub conn: ConnId,
    pub peer_id: String,
    pub addr: SocketAddr,
    pub listen_port: u16,
    pub inbound: bool,
    pub client_id: String,
    pub node_tag: String,
    pub nova: bool,
    pub latest_block: AtomicI64,
    pub score: AtomicI64,
    pub connected: Instant,
    pub tx: mpsc::Sender<Message>,
    pub closed: AtomicBool,
}

impl PeerHandle {
    pub fn send(&self, m: Message) -> bool {
        if m.is_extension() && !self.nova {
            return false;
        }
        match self.tx.try_send(m) {
            Ok(()) => true,
            Err(_) => {
                // xdagj disconnects with MESSAGE_QUEUE_FULL; we drop the message
                // and let the idle/score logic handle a stuck peer.
                false
            }
        }
    }
}

#[derive(Default)]
struct AddrEntry {
    last_attempt: Option<Instant>,
    failures: u32,
    last_success: Option<Instant>,
}

pub(crate) struct NetInner {
    pub cfg: NetConfig,
    pub id: Identity,
    pub chain: Arc<dyn ChainHandle>,
    pub peers: RwLock<HashMap<ConnId, Arc<PeerHandle>>>,
    next_conn: AtomicU64,
    addrs: Mutex<HashMap<SocketAddr, AddrEntry>>,
    bans: Mutex<HashMap<IpAddr, Instant>>,
    inbound_ips: Mutex<HashMap<IpAddr, usize>>,
    dialing: Mutex<HashSet<SocketAddr>>,
    pub pending_sums: Mutex<HashMap<i64, oneshot::Sender<Vec<u8>>>>,
    pub pending_ranges: Mutex<HashMap<i64, oneshot::Sender<()>>>,
    /// Recently relayed blocks (avoid echoing the same block around).
    seen: Mutex<(HashSet<HashLow>, VecDeque<HashLow>)>,
    pub shutdown: AtomicBool,
}

impl NetInner {
    pub fn advertise_port(&self) -> u16 {
        self.cfg.advertise_port.unwrap_or(self.cfg.listen.port())
    }

    pub fn is_banned(&self, ip: &IpAddr) -> bool {
        if self.cfg.deny.contains(ip) {
            return true;
        }
        let mut b = self.bans.lock();
        match b.get(ip) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                b.remove(ip);
                false
            }
            None => false,
        }
    }

    pub fn ban(&self, ip: IpAddr, why: &str) {
        tracing::warn!(%ip, "banning peer: {why}");
        self.bans.lock().insert(ip, Instant::now() + self.cfg.ban_duration);
        let victims: Vec<_> = self.peers.read().values().filter(|p| p.addr.ip() == ip).cloned().collect();
        for p in victims {
            p.send(Message::Disconnect(message::Reason::BadPeer as u8));
            p.closed.store(true, Ordering::SeqCst);
        }
    }

    /// Adjust a peer's score; peers far below zero are banned.
    pub fn score(&self, conn: ConnId, delta: i64, why: &str) {
        let p = self.peers.read().get(&conn).cloned();
        if let Some(p) = p {
            let s = p.score.fetch_add(delta, Ordering::SeqCst) + delta;
            if s < -100 {
                self.ban(p.addr.ip(), why);
            }
        }
    }

    pub fn mark_seen(&self, h: HashLow) -> bool {
        let mut g = self.seen.lock();
        if g.0.contains(&h) {
            return false;
        }
        g.0.insert(h);
        g.1.push_back(h);
        while g.1.len() > 100_000 {
            if let Some(old) = g.1.pop_front() {
                g.0.remove(&old);
            }
        }
        true
    }

    pub fn register(&self, p: Arc<PeerHandle>) -> Result<(), message::Reason> {
        let mut peers = self.peers.write();
        if p.peer_id == self.id.peer_id {
            return Err(message::Reason::DuplicatedPeerId); // connected to ourselves
        }
        if peers.values().any(|x| x.peer_id == p.peer_id) {
            return Err(message::Reason::DuplicatedPeerId);
        }
        let (inb, outb) = peers.values().fold((0, 0), |(i, o), x| if x.inbound { (i + 1, o) } else { (i, o + 1) });
        if p.inbound && inb >= self.cfg.max_inbound {
            return Err(message::Reason::TooManyPeers);
        }
        if !p.inbound && outb >= self.cfg.max_outbound * 2 {
            return Err(message::Reason::TooManyPeers);
        }
        peers.insert(p.conn, p.clone());
        drop(peers);
        let listen = SocketAddr::new(p.addr.ip(), p.listen_port);
        let mut a = self.addrs.lock();
        let e = a.entry(listen).or_default();
        e.last_success = Some(Instant::now());
        e.failures = 0;
        Ok(())
    }

    pub fn unregister(&self, conn: ConnId) {
        if let Some(p) = self.peers.write().remove(&conn) {
            if p.inbound {
                let mut m = self.inbound_ips.lock();
                if let Some(c) = m.get_mut(&p.addr.ip()) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        m.remove(&p.addr.ip());
                    }
                }
            }
            tracing::debug!(peer = %p.peer_id, addr = %p.addr, "peer disconnected");
        }
    }

    pub fn add_addr(&self, a: SocketAddr) {
        if a.port() == 0 {
            return;
        }
        if !self.cfg.allow_private && !is_public(&a.ip()) {
            return;
        }
        let mut m = self.addrs.lock();
        if m.len() < 10_000 {
            m.entry(a).or_default();
        }
    }

    pub fn known_addrs(&self, n: usize) -> Vec<(String, u16)> {
        let peers = self.peers.read();
        let mut v: Vec<(String, u16)> =
            peers.values().filter(|p| self.cfg.allow_private || is_public(&p.addr.ip())).map(|p| (p.addr.ip().to_string(), p.listen_port)).collect();
        v.truncate(n);
        v
    }

    pub fn random_peer(&self, nova_only: bool) -> Option<Arc<PeerHandle>> {
        use rand::seq::IteratorRandom;
        self.peers.read().values().filter(|p| !p.closed.load(Ordering::SeqCst) && (!nova_only || p.nova)).choose(&mut rand::thread_rng()).cloned()
    }

    pub fn all_peers(&self) -> Vec<Arc<PeerHandle>> {
        self.peers.read().values().cloned().collect()
    }
}

pub fn is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()),
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00),
    }
}

fn subnet_key(ip: &IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets()[..3].to_vec(),
        IpAddr::V6(v6) => v6.octets()[..6].to_vec(),
    }
}

/// Handle used by the node to drive the network.
#[derive(Clone)]
pub struct NetHandle {
    pub(crate) inner: Arc<NetInner>,
}

impl NetHandle {
    pub fn peer_id(&self) -> &str {
        &self.inner.id.peer_id
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        self.inner
            .all_peers()
            .iter()
            .map(|p| PeerInfo {
                conn: p.conn,
                peer_id: p.peer_id.clone(),
                addr: p.addr.to_string(),
                listen_port: p.listen_port,
                inbound: p.inbound,
                client_id: p.client_id.clone(),
                node_tag: p.node_tag.clone(),
                nova: p.nova,
                latest_block: p.latest_block.load(Ordering::Relaxed),
                score: p.score.load(Ordering::Relaxed),
                connected_secs: p.connected.elapsed().as_secs(),
            })
            .collect()
    }

    /// Relay a block to every peer except `except` (payload only to Nova peers).
    pub fn broadcast_block(&self, raw: &[u8], payload: Option<&[u8]>, ttl: i32, except: Option<ConnId>) {
        if ttl <= 0 {
            return;
        }
        if let Ok(b) = xdag_types::Block::parse(raw) {
            if !self.inner.mark_seen(b.hashlow()) && except.is_some() {
                return;
            }
        }
        for p in self.inner.all_peers() {
            if Some(p.conn) == except {
                continue;
            }
            match payload {
                Some(pl) if p.nova => {
                    p.send(Message::NewBlockExt { block: raw.to_vec(), ttl, payload: pl.to_vec() });
                }
                _ => {
                    p.send(Message::NewBlock { block: raw.to_vec(), ttl });
                }
            }
        }
    }

    /// Ask for a missing block (from `prefer` if given, else from every peer).
    pub fn request_block(&self, h: HashLow, prefer: Option<ConnId>, sync: bool) {
        let stats = self.inner.chain.stats();
        let body = message::XdagBody::with_hashlow(&h, stats);
        let msg = if sync { Message::SyncBlockRequest(body) } else { Message::BlockRequest(body) };
        let peers = self.inner.all_peers();
        if let Some(c) = prefer {
            if let Some(p) = peers.iter().find(|p| p.conn == c) {
                p.send(msg);
                return;
            }
        }
        for p in peers {
            p.send(msg.clone());
        }
    }

    pub fn request_payload(&self, h: HashLow, prefer: Option<ConnId>) {
        let peers = self.inner.all_peers();
        let target = prefer.and_then(|c| peers.iter().find(|p| p.conn == c && p.nova).cloned());
        match target {
            Some(p) => {
                p.send(Message::GetPayload(h));
            }
            None => {
                for p in peers.iter().filter(|p| p.nova) {
                    p.send(Message::GetPayload(h));
                }
            }
        }
    }

    pub fn broadcast_txs(&self, txs: Vec<(u8, Vec<u8>)>, except: Option<ConnId>) {
        if txs.is_empty() {
            return;
        }
        for p in self.inner.all_peers() {
            if Some(p.conn) != except {
                p.send(Message::NewTxs(txs.clone()));
            }
        }
    }

    pub fn penalize(&self, conn: ConnId, delta: i64, why: &str) {
        self.inner.score(conn, -delta.abs(), why);
    }

    pub fn connect(&self, addr: SocketAddr) {
        let inner = self.inner.clone();
        tokio::spawn(async move { dial(inner, addr).await });
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Start listening, dialing seeds and discovery. Returns immediately.
pub async fn start(cfg: NetConfig, id: Identity, chain: Arc<dyn ChainHandle>) -> std::io::Result<NetHandle> {
    let listener = TcpListener::bind(cfg.listen).await?;
    tracing::info!(listen = %cfg.listen, peer_id = %id.peer_id, "p2p listening (open network, no whitelist)");
    let inner = Arc::new(NetInner {
        cfg: cfg.clone(),
        id,
        chain,
        peers: RwLock::new(HashMap::new()),
        next_conn: AtomicU64::new(1),
        addrs: Mutex::new(HashMap::new()),
        bans: Mutex::new(HashMap::new()),
        inbound_ips: Mutex::new(HashMap::new()),
        dialing: Mutex::new(HashSet::new()),
        pending_sums: Mutex::new(HashMap::new()),
        pending_ranges: Mutex::new(HashMap::new()),
        seen: Mutex::new((HashSet::new(), VecDeque::new())),
        shutdown: AtomicBool::new(false),
    });
    load_peers(&inner);
    for s in &cfg.seeds {
        match tokio::net::lookup_host(s.as_str()).await {
            Ok(addrs) => {
                for a in addrs {
                    inner.addrs.lock().entry(a).or_default();
                }
            }
            Err(e) => tracing::warn!(seed = %s, "cannot resolve seed: {e}"),
        }
    }
    // accept loop
    {
        let inner = inner.clone();
        tokio::spawn(async move {
            loop {
                if inner.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept().await {
                    Ok((stream, addr)) => accept(inner.clone(), stream, addr),
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }
    // dialer + discovery + persistence
    {
        let inner = inner.clone();
        tokio::spawn(async move {
            let mut tick: u64 = 0;
            loop {
                if inner.shutdown.load(Ordering::SeqCst) {
                    save_peers(&inner);
                    break;
                }
                dial_more(&inner);
                if tick % 30 == 5 {
                    if let Some(p) = inner.random_peer(true) {
                        p.send(Message::GetPeers);
                    }
                }
                if tick % 60 == 30 {
                    save_peers(&inner);
                }
                tick += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
    // synchronisation
    {
        let inner = inner.clone();
        tokio::spawn(async move { sync::run(inner).await });
    }
    Ok(NetHandle { inner })
}

fn accept(inner: Arc<NetInner>, stream: TcpStream, addr: SocketAddr) {
    let ip = addr.ip();
    if inner.is_banned(&ip) {
        return;
    }
    {
        let mut m = inner.inbound_ips.lock();
        let per_ip = m.get(&ip).copied().unwrap_or(0);
        if per_ip >= inner.cfg.max_inbound_per_ip {
            return;
        }
        let key = subnet_key(&ip);
        let per_subnet: usize = m.iter().filter(|(k, _)| subnet_key(k) == key).map(|(_, v)| *v).sum();
        if per_subnet >= inner.cfg.max_inbound_per_subnet {
            return;
        }
        let inbound = inner.peers.read().values().filter(|p| p.inbound).count();
        if inbound >= inner.cfg.max_inbound {
            return;
        }
        *m.entry(ip).or_insert(0) += 1;
    }
    let conn = inner.next_conn.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let r = peer::run(inner.clone(), stream, addr, true, conn).await;
        if let Err(e) = r {
            tracing::debug!(%addr, "inbound connection ended: {e}");
        }
        // make sure the per-IP counter is released even if the handshake failed
        let registered = inner.peers.read().contains_key(&conn);
        if registered {
            inner.unregister(conn);
        } else {
            let mut m = inner.inbound_ips.lock();
            if let Some(c) = m.get_mut(&ip) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&ip);
                }
            }
        }
    });
}

async fn dial(inner: Arc<NetInner>, addr: SocketAddr) {
    if inner.is_banned(&addr.ip()) || !inner.dialing.lock().insert(addr) {
        return;
    }
    {
        let mut a = inner.addrs.lock();
        let e = a.entry(addr).or_default();
        e.last_attempt = Some(Instant::now());
    }
    let res = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await;
    match res {
        Ok(Ok(stream)) => {
            let conn = inner.next_conn.fetch_add(1, Ordering::SeqCst);
            inner.dialing.lock().remove(&addr);
            if let Err(e) = peer::run(inner.clone(), stream, addr, false, conn).await {
                tracing::debug!(%addr, "outbound connection ended: {e}");
            }
            inner.unregister(conn);
        }
        _ => {
            inner.dialing.lock().remove(&addr);
            let mut a = inner.addrs.lock();
            if let Some(e) = a.get_mut(&addr) {
                e.failures += 1;
            }
        }
    }
}

fn dial_more(inner: &Arc<NetInner>) {
    let (outbound, connected): (usize, HashSet<SocketAddr>) = {
        let peers = inner.peers.read();
        (peers.values().filter(|p| !p.inbound).count(), peers.values().map(|p| SocketAddr::new(p.addr.ip(), p.listen_port)).collect())
    };
    let dialing = inner.dialing.lock().len();
    if outbound + dialing >= inner.cfg.max_outbound {
        return;
    }
    let want = inner.cfg.max_outbound - outbound - dialing;
    let now = Instant::now();
    let mut candidates: Vec<SocketAddr> = inner
        .addrs
        .lock()
        .iter()
        .filter(|(a, e)| {
            !connected.contains(a)
                && e.failures < 10
                && e.last_attempt.map(|t| now.duration_since(t) > Duration::from_secs(30 * (1 + e.failures as u64))).unwrap_or(true)
        })
        .map(|(a, _)| *a)
        .collect();
    use rand::seq::SliceRandom;
    candidates.shuffle(&mut rand::thread_rng());
    for a in candidates.into_iter().take(want) {
        if a.port() == inner.advertise_port() && (a.ip().is_loopback() || a.ip().is_unspecified()) {
            continue;
        }
        let i = inner.clone();
        tokio::spawn(async move { dial(i, a).await });
    }
}

fn load_peers(inner: &NetInner) {
    let Some(p) = &inner.cfg.peers_file else { return };
    if let Ok(s) = std::fs::read_to_string(p) {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&s) {
            for a in list {
                if let Ok(sa) = a.parse::<SocketAddr>() {
                    inner.addrs.lock().entry(sa).or_default();
                }
            }
        }
    }
}

fn save_peers(inner: &NetInner) {
    let Some(p) = &inner.cfg.peers_file else { return };
    let list: Vec<String> =
        inner.addrs.lock().iter().filter(|(_, e)| e.last_success.is_some() || e.failures < 3).map(|(a, _)| a.to_string()).take(2000).collect();
    if let Ok(s) = serde_json::to_string(&list) {
        let _ = std::fs::write(p, s);
    }
}
