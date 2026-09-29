//! Built-in CPU miner (devnets, solo mining). Production RandomX mining is
//! expected to use external pool software through the WebSocket interface.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use rand::RngCore;
use xdag_types::Address;

use crate::Coordinator;

pub struct Miner {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    pub hashes: Arc<AtomicU64>,
}

impl Miner {
    pub fn start(coord: Arc<Coordinator>, threads: usize, reward_address: Address) -> Miner {
        let stop = Arc::new(AtomicBool::new(false));
        let hashes = Arc::new(AtomicU64::new(0));
        let handles = (0..threads.max(1))
            .map(|i| {
                let (coord, stop, hashes) = (coord.clone(), stop.clone(), hashes.clone());
                std::thread::Builder::new()
                    .name(format!("miner-{i}"))
                    .spawn(move || {
                        let mut rng = rand::thread_rng();
                        while !stop.load(Ordering::Relaxed) {
                            let Some(task) = coord.current_task() else {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                continue;
                            };
                            for _ in 0..256 {
                                let mut nonce = [0u8; 32];
                                rng.fill_bytes(&mut nonce[..12]);
                                nonce[12..].copy_from_slice(&reward_address.0);
                                if let Some(score) = coord.score(&task, &nonce) {
                                    coord.offer(task.index, nonce, score);
                                }
                                hashes.fetch_add(1, Ordering::Relaxed);
                            }
                            std::thread::yield_now();
                        }
                    })
                    .expect("spawn miner")
            })
            .collect();
        Miner { stop, handles, hashes }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.handles {
            let _ = h.join();
        }
    }
}
