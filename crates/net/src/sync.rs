//! Range synchronisation with the C/xdagj "sums" protocol.
//!
//! Starting from `[0, 2^48)`, compare 16-way summaries of block sums with a
//! peer and descend into the sub-ranges that differ until they are at most
//! 2^20 ticks wide, then fetch those windows with `BLOCKS_REQUEST`.
//! Unlike xdagj, the work per round is bounded (a malicious peer answering
//! with random sums could otherwise make us walk the whole tree), and the
//! peer's claimed chain statistics never decide when syncing is complete.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::oneshot;

use crate::message::{Message, XdagBody};
use crate::peer::MAX_RANGE;
use crate::{NetInner, PeerHandle};

const MAX_SUMS_REQUESTS_PER_ROUND: usize = 512;
const MAX_WINDOWS_PER_ROUND: usize = 128;
const SUMS_TIMEOUT: Duration = Duration::from_secs(30);
const RANGE_TIMEOUT: Duration = Duration::from_secs(64);

pub(crate) async fn run(inner: Arc<NetInner>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut round: u64 = 0;
    loop {
        if inner.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let synced = inner.chain.is_synced();
        // when synced, still reconcile occasionally to heal gaps
        if !synced || round.is_multiple_of(6) {
            if let Some(peer) = inner.random_peer(false) {
                if let Err(e) = sync_with(&inner, &peer).await {
                    tracing::debug!(peer = %peer.peer_id, "sync round aborted: {e}");
                }
            }
        }
        round += 1;
        tokio::time::sleep(Duration::from_secs(if synced { 10 } else { 3 })).await;
    }
}

async fn request_sums(inner: &Arc<NetInner>, p: &Arc<PeerHandle>, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let random: i64 = rand::thread_rng().gen_range(0..i64::MAX);
    let (tx, rx) = oneshot::channel();
    inner.pending_sums.lock().insert(random, tx);
    if !p.send(Message::SumsRequest(XdagBody::new(start as i64, end as i64, random, inner.chain.stats()))) {
        inner.pending_sums.lock().remove(&random);
        return Err("send failed".into());
    }
    match tokio::time::timeout(SUMS_TIMEOUT, rx).await {
        Ok(Ok(s)) => Ok(s),
        _ => {
            inner.pending_sums.lock().remove(&random);
            Err("sums timeout".into())
        }
    }
}

async fn request_range(inner: &Arc<NetInner>, p: &Arc<PeerHandle>, start: u64, end: u64) -> Result<(), String> {
    let random: i64 = rand::thread_rng().gen_range(0..i64::MAX);
    let (tx, rx) = oneshot::channel();
    inner.pending_ranges.lock().insert(random, tx);
    if !p.send(Message::BlocksRequest(XdagBody::new(start as i64, end as i64, random, inner.chain.stats()))) {
        inner.pending_ranges.lock().remove(&random);
        return Err("send failed".into());
    }
    match tokio::time::timeout(RANGE_TIMEOUT, rx).await {
        Ok(Ok(())) => Ok(()),
        _ => {
            inner.pending_ranges.lock().remove(&random);
            Err("range timeout".into())
        }
    }
}

pub(crate) async fn sync_with(inner: &Arc<NetInner>, p: &Arc<PeerHandle>) -> Result<usize, String> {
    let last_time = inner.chain.last_main_time();
    let mut stack: Vec<(u64, u64)> = vec![(0, 1u64 << 48)];
    let mut windows: Vec<u64> = vec![];
    let mut requests = 0usize;
    while let Some((t, dt)) = stack.pop() {
        if dt as i64 > MAX_RANGE {
            if requests >= MAX_SUMS_REQUESTS_PER_ROUND {
                break;
            }
            let Some(local) = inner.chain.sums(t, t + dt) else { continue };
            let remote = request_sums(inner, p, t, t + dt).await?;
            requests += 1;
            if remote.len() != 256 {
                return Err("bad sums reply".into());
            }
            let sub = dt >> 4;
            // push in reverse so that older ranges are explored first
            for i in (0..16).rev() {
                let a = &local[i * 16..i * 16 + 16];
                let b = &remote[i * 16..i * 16 + 16];
                if a != b {
                    stack.push((t + i as u64 * sub, sub));
                }
            }
        } else if t + dt > last_time {
            windows.push(t);
            if windows.len() >= MAX_WINDOWS_PER_ROUND {
                break;
            }
        }
    }
    windows.sort_unstable();
    let n = windows.len();
    for t in windows {
        request_range(inner, p, t, t + MAX_RANGE as u64).await?;
    }
    if n > 0 {
        tracing::debug!(peer = %p.peer_id, windows = n, sums_requests = requests, "sync round done");
    }
    Ok(n)
}
