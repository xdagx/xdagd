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
/// Do not ask for another window while this many blocks await import.
const MAX_IMPORT_BACKLOG: usize = 20_000;
const BACKLOG_PATIENCE: Duration = Duration::from_secs(300);
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
    p.ranges_pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let done = tokio::time::timeout(RANGE_TIMEOUT, rx).await;
    p.ranges_pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    match done {
        Ok(Ok(())) => Ok(()),
        _ => {
            inner.pending_ranges.lock().remove(&random);
            Err("range timeout".into())
        }
    }
}

/// The 16 sub-ranges of `[t, t + dt)` worth descending into: those whose sums
/// differ and that end after `last_time`.
///
/// Only windows ending after the last main block are ever fetched (as in
/// xdagj), so a range that ends at or before it cannot contribute one. It must
/// not be explored either: a node bootstrapped from a pruned snapshot lacks old
/// blocks its peers still have, differs from them in every old range, and
/// would spend the whole round budget there without reaching the recent ranges.
fn differing_subranges(t: u64, dt: u64, local: &[u8], remote: &[u8], last_time: u64) -> Vec<(u64, u64)> {
    let sub = dt >> 4;
    (0..16usize)
        .filter(|i| local[i * 16..i * 16 + 16] != remote[i * 16..i * 16 + 16])
        .map(|i| (t + i as u64 * sub, sub))
        .filter(|(start, sub)| start + sub > last_time)
        .collect()
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
            // push in reverse so that older ranges are explored first
            stack.extend(differing_subranges(t, dt, &local, &remote, last_time).into_iter().rev());
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
        // importing can be much slower than fetching (RandomX): keep pace with it
        let waiting = std::time::Instant::now();
        while inner.chain.import_backlog() > MAX_IMPORT_BACKLOG {
            if waiting.elapsed() > BACKLOG_PATIENCE {
                return Err("import does not keep up".into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        request_range(inner, p, t, t + MAX_RANGE as u64).await?;
    }
    if n > 0 {
        tracing::debug!(peer = %p.peer_id, windows = n, sums_requests = requests, "sync round done");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sums(nonzero: &[usize]) -> Vec<u8> {
        let mut s = vec![0u8; 256];
        for i in nonzero {
            s[i * 16] = 1;
        }
        s
    }

    #[test]
    fn only_differing_ranges_after_the_last_main_block_are_explored() {
        let (t, dt) = (1u64 << 32, 1u64 << 28);
        let sub = dt >> 4;
        let local = sums(&[]);
        let remote = sums(&[0, 1, 5, 9, 15]);
        // a fresh node explores every differing range
        assert_eq!(differing_subranges(t, dt, &local, &remote, 0).len(), 5);
        // last main block inside sub-range 5: older ranges are skipped, the
        // one containing it and the later ones are kept
        let last = t + 5 * sub + 17;
        assert_eq!(differing_subranges(t, dt, &local, &remote, last), vec![(t + 5 * sub, sub), (t + 9 * sub, sub), (t + 15 * sub, sub)]);
        // a range ending exactly at the last main block holds nothing newer
        assert_eq!(differing_subranges(t, dt, &local, &remote, t + 6 * sub), vec![(t + 9 * sub, sub), (t + 15 * sub, sub)]);
        // equal sums are never explored
        assert!(differing_subranges(t, dt, &remote, &remote, 0).is_empty());
    }
}
