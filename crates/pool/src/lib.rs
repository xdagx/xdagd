//! Mining coordination.
//!
//! Each epoch the node builds a main-block candidate and publishes a *task*;
//! miners (external pools over WebSocket, or the built-in CPU miner) search
//! nonces for it; the best share becomes the candidate's nonce when the epoch
//! ends. Task/share messages are byte-compatible with xdagj's pool interface:
//!
//! ```text
//! node → pool {"msgType":1,"msgContent":{"task":{"preHash":hex,"taskSeed":hex},"taskTime":epoch,"taskIndex":n}}
//! pool → node {"msgType":2,"msgContent":{"share":hex32,"hash":preHashHex,"taskIndex":n}}
//! ```

pub mod miner;
pub mod rewards;
pub mod server;

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use xdag_types::block::with_nonce;
use xdag_types::{Block, BLOCK_SIZE};

/// How a candidate's nonce is scored.
#[derive(Clone, Debug)]
pub enum PowKind {
    /// sha256d of the whole block (before the RandomX fork).
    Sha256d,
    /// RandomX(key, sha256(raw[0..480]) ‖ nonce).
    RandomX { key: [u8; 32] },
}

pub type RandomXFn = Arc<dyn Fn(&[u8; 32], &[u8]) -> Option<[u8; 32]> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Task {
    pub index: u64,
    pub epoch: u64,
    pub pow: PowKind,
    pub candidate: Arc<[u8; BLOCK_SIZE]>,
    pub pre_hash: [u8; 32],
}

impl Task {
    pub fn to_json(&self) -> String {
        let seed = match &self.pow {
            PowKind::RandomX { key } => hex::encode(key),
            PowKind::Sha256d => String::new(),
        };
        serde_json::json!({
            "msgType": 1,
            "msgContent": {
                "task": {"preHash": hex::encode(self.pre_hash), "taskSeed": seed},
                "taskTime": self.epoch,
                "taskIndex": self.index,
            }
        })
        .to_string()
    }
}

struct Best {
    nonce: [u8; 32],
    /// Hash value compared as a little-endian 256-bit number (smaller is better).
    score: [u8; 32],
}

/// Shared state between the task producer, WebSocket pools and the built-in miner.
pub struct Coordinator {
    current: Mutex<Option<(Task, Best)>>,
    next_index: Mutex<u64>,
    pub tasks: broadcast::Sender<String>,
    randomx: Option<RandomXFn>,
}

fn le_less(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

impl Coordinator {
    pub fn new(randomx: Option<RandomXFn>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        Arc::new(Coordinator { current: Mutex::new(None), next_index: Mutex::new(0), tasks: tx, randomx })
    }

    /// Publish a new candidate (its current nonce is the node's own default).
    pub fn new_task(&self, candidate: &Block, epoch: u64, pow: PowKind) -> Task {
        let mut idx = self.next_index.lock();
        *idx += 1;
        let raw = *candidate.raw();
        let pre_hash = xdag_types::hash::sha256(&raw[..480]);
        let task = Task { index: *idx, epoch, pow, candidate: Arc::new(raw), pre_hash };
        let own_nonce = candidate.nonce.unwrap_or([0u8; 32]);
        let score = self.score(&task, &own_nonce).unwrap_or([0xff; 32]);
        *self.current.lock() = Some((task.clone(), Best { nonce: own_nonce, score }));
        let _ = self.tasks.send(task.to_json());
        task
    }

    pub fn current_task(&self) -> Option<Task> {
        self.current.lock().as_ref().map(|(t, _)| t.clone())
    }

    /// Hash of a nonce for a task (as a little-endian number; smaller = more work).
    pub fn score(&self, t: &Task, nonce: &[u8; 32]) -> Option<[u8; 32]> {
        match &t.pow {
            PowKind::Sha256d => Some(xdag_types::hash::sha256d(&with_nonce(&t.candidate, nonce))),
            PowKind::RandomX { key } => {
                let mut input = [0u8; 64];
                input[..32].copy_from_slice(&t.pre_hash);
                input[32..].copy_from_slice(nonce);
                self.randomx.as_ref().and_then(|f| f(key, &input))
            }
        }
    }

    /// Offer a share; returns true if it improved the best nonce.
    pub fn submit_share(&self, task_index: u64, pre_hash_hex: Option<&str>, nonce: [u8; 32]) -> bool {
        let task = match self.current.lock().as_ref() {
            Some((t, _)) if t.index == task_index => t.clone(),
            _ => return false,
        };
        if let Some(h) = pre_hash_hex {
            if !h.eq_ignore_ascii_case(&hex::encode(task.pre_hash)) {
                return false;
            }
        }
        let Some(score) = self.score(&task, &nonce) else { return false };
        self.offer(task.index, nonce, score)
    }

    /// Record an already-scored nonce (built-in miner).
    pub fn offer(&self, task_index: u64, nonce: [u8; 32], score: [u8; 32]) -> bool {
        let mut cur = self.current.lock();
        match cur.as_mut() {
            Some((t, best)) if t.index == task_index && le_less(&score, &best.score) => {
                best.nonce = nonce;
                best.score = score;
                true
            }
            _ => false,
        }
    }

    /// Finish the current task: the candidate with the best nonce installed.
    pub fn finish(&self) -> Option<Block> {
        let cur = self.current.lock().take()?;
        let raw = with_nonce(&cur.0.candidate, &cur.1.nonce);
        Block::parse(&raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdag_types::{BlockTemplate, FieldType, KeyPair};

    #[test]
    fn best_share_wins_and_stale_tasks_are_ignored() {
        let k = KeyPair::random();
        let mut t = BlockTemplate::new(FieldType::HeadTest, 0x1690_0001_ffff);
        t.sign_out = Some(k.clone());
        t.mining_nonce = Some([0u8; 32]);
        let cand = t.build().unwrap();
        let c = Coordinator::new(None);
        let task = c.new_task(&cand, 0x16900001, PowKind::Sha256d);
        let mut best = Some((c.score(&task, &[0u8; 32]).unwrap(), [0u8; 32]));
        for i in 0..200u8 {
            let mut n = [0u8; 32];
            n[0] = i;
            n[12..].copy_from_slice(&k.address().0);
            c.submit_share(task.index, None, n);
            let s = c.score(&task, &n).unwrap();
            if best.as_ref().map(|(bs, _)| le_less(&s, bs)).unwrap_or(true) {
                best = Some((s, n));
            }
        }
        assert!(!c.submit_share(task.index + 1, None, [9u8; 32]));
        let b = c.finish().unwrap();
        assert_eq!(b.nonce.unwrap(), best.unwrap().1);
        assert!(b.outsig_signed_by(k.public()), "nonce is outside the signed data");
    }
}
