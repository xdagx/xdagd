//! One peer connection: handshake, then message dispatch with rate limits.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use rand::RngCore;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};
use xdag_types::HashLow;

use crate::frame::{packetize, Assembler, FrameCodec};
use crate::handshake::{self, HandshakeError, SECRET_LEN};
use crate::message::{self, Handshake, Message, Reason, XdagBody, CAP_FULL_NODE, CAP_LIGHT_NODE, CAP_NOVA};
use crate::ratelimit::PeerLimits;
use crate::{ConnId, NetInner, PeerHandle};

/// xdagj REQUEST_BLOCKS_MAX_TIME: the largest range a BLOCKS_REQUEST may cover.
pub const MAX_RANGE: i64 = 1 << 20;
/// Cap on blocks served for one range request.
pub const MAX_RANGE_BLOCKS: usize = 50_000;

fn now_ms() -> i64 {
    xdag_types::time::now_ms() as i64
}

fn capabilities() -> Vec<String> {
    vec![CAP_FULL_NODE.into(), CAP_LIGHT_NODE.into(), CAP_NOVA.into()]
}

fn my_handshake(inner: &NetInner, secret: &[u8]) -> Handshake {
    handshake::build(
        &inner.id,
        inner.cfg.network,
        inner.cfg.network_version,
        inner.advertise_port(),
        &inner.cfg.client_id,
        capabilities(),
        inner.chain.latest_main(),
        secret,
        now_ms(),
        inner.cfg.generate_block,
        &inner.cfg.node_tag,
    )
}

pub(crate) async fn run(inner: Arc<NetInner>, stream: TcpStream, addr: SocketAddr, inbound: bool, conn: ConnId) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let max_body = inner.cfg.max_frame_body;
    let max_packet = inner.cfg.max_packet;
    let (rd, wr) = stream.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec { max_body });
    let mut writer = FramedWrite::new(wr, FrameCodec { max_body });
    let (tx, mut rx) = mpsc::channel::<Message>(8192);

    let wtask = tokio::spawn(async move {
        let mut packet_id: i32 = 0;
        while let Some(m) = rx.recv().await {
            packet_id = packet_id.wrapping_add(1);
            let frames = match packetize(m.code(), packet_id, &m.encode(), max_body, max_packet) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("cannot encode message 0x{:02x}: {e}", m.code());
                    continue;
                }
            };
            for f in frames {
                if writer.send(f).await.is_err() {
                    return;
                }
            }
            if matches!(m, Message::Disconnect(_)) {
                return;
            }
        }
    });

    let mut asm = Assembler::new(max_packet);
    let res = session(&inner, &mut reader, &mut asm, &tx, addr, inbound, conn).await;
    // let the writer flush a pending DISCONNECT
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), wtask).await;
    res
}

async fn next_message(
    reader: &mut FramedRead<tokio::net::tcp::OwnedReadHalf, FrameCodec>,
    asm: &mut Assembler,
    timeout: Duration,
) -> Result<Option<Message>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let f = tokio::time::timeout(left, reader.next()).await.map_err(|_| "timeout".to_string())?;
        let f = match f {
            None => return Err("connection closed".into()),
            Some(Err(e)) => return Err(e.to_string()),
            Some(Ok(f)) => f,
        };
        if let Some((code, body)) = asm.push(f).map_err(|e| e.to_string())? {
            match Message::decode(code, &body) {
                Ok(Some(m)) => return Ok(Some(m)),
                Ok(None) => continue, // unknown message code: ignored, like xdagj
                Err(_) => return Err(format!("malformed message 0x{code:02x}")),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    inner: &Arc<NetInner>,
    reader: &mut FramedRead<tokio::net::tcp::OwnedReadHalf, FrameCodec>,
    asm: &mut Assembler,
    tx: &mpsc::Sender<Message>,
    addr: SocketAddr,
    inbound: bool,
    conn: ConnId,
) -> Result<(), String> {
    let hs_timeout = inner.cfg.handshake_timeout;
    let expiry = inner.cfg.handshake_expiry_ms;
    let disconnect = |r: Reason| {
        let _ = tx.try_send(Message::Disconnect(r as u8));
    };
    let check = |h: &Handshake, secret: &[u8]| -> Result<(), Reason> {
        match handshake::validate(h, inner.cfg.network, inner.cfg.network_version, secret, now_ms(), expiry) {
            Ok(_) => Ok(()),
            Err(HandshakeError::BadNetwork) => Err(Reason::BadNetwork),
            Err(HandshakeError::BadVersion) => Err(Reason::BadNetworkVersion),
            Err(HandshakeError::Invalid(why)) => {
                tracing::debug!(%addr, "invalid handshake: {why}");
                Err(Reason::InvalidHandshake)
            }
        }
    };

    let remote: Handshake = if inbound {
        let mut secret = vec![0u8; SECRET_LEN];
        rand::thread_rng().fill_bytes(&mut secret);
        tx.send(Message::Init { secret: secret.clone(), timestamp: now_ms() }).await.map_err(|e| e.to_string())?;
        match next_message(reader, asm, hs_timeout).await? {
            Some(Message::Hello(h)) => {
                if let Err(r) = check(&h, &secret) {
                    disconnect(r);
                    return Err(format!("handshake rejected: {r:?}"));
                }
                tx.send(Message::World(my_handshake(inner, &secret))).await.map_err(|e| e.to_string())?;
                h
            }
            other => return Err(format!("expected HELLO, got {other:?}")),
        }
    } else {
        let secret = match next_message(reader, asm, hs_timeout).await? {
            Some(Message::Init { secret, timestamp }) if secret.len() == SECRET_LEN && timestamp > 0 => secret,
            other => {
                disconnect(Reason::InvalidHandshake);
                return Err(format!("expected INIT, got {other:?}"));
            }
        };
        tx.send(Message::Hello(my_handshake(inner, &secret))).await.map_err(|e| e.to_string())?;
        match next_message(reader, asm, hs_timeout).await? {
            Some(Message::World(h)) => {
                if let Err(r) = check(&h, &secret) {
                    disconnect(r);
                    return Err(format!("handshake rejected: {r:?}"));
                }
                h
            }
            other => return Err(format!("expected WORLD, got {other:?}")),
        }
    };

    let handle = Arc::new(PeerHandle {
        conn,
        peer_id: remote.peer_id.clone(),
        addr,
        listen_port: remote.port as u16,
        inbound,
        client_id: remote.client_id.clone(),
        node_tag: remote.node_tag.clone(),
        nova: remote.capabilities.iter().any(|c| c == CAP_NOVA),
        latest_block: AtomicI64::new(remote.latest_block_number),
        score: AtomicI64::new(0),
        connected: Instant::now(),
        tx: tx.clone(),
        closed: AtomicBool::new(false),
    });
    if let Err(r) = inner.register(handle.clone()) {
        disconnect(r);
        return Err(format!("not registered: {r:?}"));
    }
    inner.add_addr(SocketAddr::new(addr.ip(), remote.port as u16));
    tracing::info!(peer = %remote.peer_id, %addr, inbound, client = %remote.client_id, nova = handle.nova, "peer connected");

    let mut limits = PeerLimits::default();
    let mut last_rx = Instant::now();
    let mut ping = tokio::time::interval(Duration::from_secs(60));
    ping.tick().await;
    loop {
        if handle.closed.load(Ordering::SeqCst) || inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = ping.tick() => {
                if last_rx.elapsed() > inner.cfg.idle_timeout {
                    return Err("idle timeout".into());
                }
                handle.send(Message::Ping(now_ms()));
            }
            m = next_message(reader, asm, Duration::from_secs(5)) => {
                match m {
                    Ok(Some(m)) => {
                        last_rx = Instant::now();
                        if let Err(e) = dispatch(inner, &handle, m, &mut limits) {
                            inner.score(conn, -20, &e);
                            tracing::debug!(peer = %handle.peer_id, "protocol violation: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) if e == "timeout" => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(())
}

fn dispatch(inner: &Arc<NetInner>, p: &Arc<PeerHandle>, m: Message, lim: &mut PeerLimits) -> Result<(), String> {
    let chain = &inner.chain;
    match m {
        Message::Disconnect(r) => {
            tracing::debug!(peer = %p.peer_id, reason = ?Reason::from_u8(r), "peer sent DISCONNECT");
            p.closed.store(true, Ordering::SeqCst);
        }
        Message::Ping(t) => {
            p.send(Message::Pong(t));
        }
        Message::Pong(_) => {}
        Message::Init { .. } | Message::Hello(_) | Message::World(_) => {
            return Err("handshake message after handshake".into());
        }
        Message::NewBlock { block, ttl } => {
            if !lim.blocks.take(1.0) {
                return Err("block rate limit".into());
            }
            if block.len() != xdag_types::BLOCK_SIZE {
                return Err("bad block size".into());
            }
            chain.on_block(block, None, p.conn, ttl.saturating_sub(1).min(inner.cfg.ttl), false);
        }
        Message::NewBlockExt { block, ttl, payload } => {
            if !lim.blocks.take(1.0) {
                return Err("block rate limit".into());
            }
            if block.len() != xdag_types::BLOCK_SIZE {
                return Err("bad block size".into());
            }
            chain.on_block(block, Some(payload), p.conn, ttl.saturating_sub(1).min(inner.cfg.ttl), false);
        }
        Message::SyncBlock { block, .. } => {
            // The execution state xdagj attaches is ignored: execution results
            // are always computed locally (xdagj copied them from peers during
            // sync, letting any peer dictate which transactions fail).
            if !lim.blocks.take(0.2) {
                return Err("sync block rate limit".into());
            }
            if block.len() != xdag_types::BLOCK_SIZE {
                return Err("bad block size".into());
            }
            chain.on_block(block, None, p.conn, 0, true);
        }
        Message::BlockRequest(b) | Message::SyncBlockRequest(b) if !lim.requests.take(1.0) => {
            let _ = b;
            return Err("request rate limit".into());
        }
        Message::BlockRequest(b) => {
            let h = b.hashlow();
            if let Some((raw, _exec, payload)) = chain.get_block(&h) {
                match payload {
                    Some(pl) if p.nova => p.send(Message::NewBlockExt { block: raw, ttl: inner.cfg.ttl, payload: pl }),
                    _ => p.send(Message::NewBlock { block: raw, ttl: inner.cfg.ttl }),
                };
            }
        }
        Message::SyncBlockRequest(b) => {
            let h = b.hashlow();
            if let Some((raw, exec, payload)) = chain.get_block(&h) {
                p.send(Message::SyncBlock { block: raw, ttl: 1, exec_state: exec });
                if let (Some(pl), true) = (payload, p.nova) {
                    p.send(Message::Payload(h, pl));
                }
            }
        }
        Message::BlocksRequest(b) => {
            if !lim.ranges.take(1.0) {
                return Err("range request rate limit".into());
            }
            if b.end <= b.start || b.end - b.start > MAX_RANGE || b.start < 0 {
                return Err("oversized BLOCKS_REQUEST range".into());
            }
            for (raw, exec) in chain.blocks_in_range(b.start as u64, b.end as u64, MAX_RANGE_BLOCKS) {
                let h = xdag_types::BlockHash::of_raw(&raw).hashlow();
                p.send(Message::SyncBlock { block: raw, ttl: 1, exec_state: exec });
                if p.nova {
                    if let Some(pl) = chain.get_payload(&h) {
                        p.send(Message::Payload(h, pl));
                    }
                }
            }
            p.send(Message::BlocksReply(XdagBody::new(b.start, b.end, b.random, chain.stats())));
        }
        Message::BlocksReply(b) => {
            if let Some(s) = inner.pending_ranges.lock().remove(&b.random) {
                let _ = s.send(());
            }
        }
        Message::SumsRequest(b) => {
            if !lim.ranges.take(1.0) {
                return Err("sums request rate limit".into());
            }
            let sums = if b.start >= 0 && b.end > b.start { chain.sums(b.start as u64, b.end as u64) } else { None };
            let sums = sums.map(|s| s.to_vec()).unwrap_or_else(|| vec![0u8; 256]);
            p.send(Message::SumsReply(XdagBody::new(1, b.end, b.random, chain.stats()), sums));
        }
        Message::SumsReply(b, sums) => {
            if let Some(s) = inner.pending_sums.lock().remove(&b.random) {
                let _ = s.send(sums);
            }
        }
        Message::GetPeers => {
            if !lim.peers.take(1.0) {
                return Err("GET_PEERS rate limit".into());
            }
            p.send(Message::Peers(inner.known_addrs(32)));
        }
        Message::Peers(list) => {
            for (h, port) in list.into_iter().take(64) {
                if let Ok(ip) = h.parse::<std::net::IpAddr>() {
                    inner.add_addr(SocketAddr::new(ip, port));
                }
            }
        }
        Message::GetPayload(h) => {
            if !lim.requests.take(1.0) {
                return Err("payload request rate limit".into());
            }
            if let Some(pl) = chain.get_payload(&h) {
                p.send(Message::Payload(h, pl));
            }
        }
        Message::Payload(h, pl) => {
            if !lim.blocks.take(1.0) {
                return Err("payload rate limit".into());
            }
            chain.on_payload(h, pl, p.conn);
        }
        Message::NewTxs(txs) => {
            if !lim.txs.take(txs.len() as f64) {
                return Err("transaction rate limit".into());
            }
            chain.on_txs(txs, p.conn);
        }
    }
    Ok(())
}

/// Hashlow of a raw block (helper for callers).
pub fn raw_hashlow(raw: &[u8]) -> HashLow {
    xdag_types::BlockHash::of_raw(raw).hashlow()
}

#[allow(unused)]
fn _assert_send() {
    fn is_send<T: Send>(_: T) {}
    let _ = message::code::PING;
}
