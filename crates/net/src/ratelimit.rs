//! Token buckets for per-peer message rate limiting.

use std::time::Instant;

#[derive(Debug)]
pub struct Bucket {
    tokens: f64,
    capacity: f64,
    rate: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Bucket { tokens: burst, capacity: burst, rate: rate_per_sec, last: Instant::now() }
    }

    pub fn take(&mut self, n: f64) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.capacity);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

/// Per-peer limits by message class.
#[derive(Debug)]
pub struct PeerLimits {
    pub blocks: Bucket,
    pub requests: Bucket,
    pub ranges: Bucket,
    pub txs: Bucket,
    pub peers: Bucket,
}

impl Default for PeerLimits {
    fn default() -> Self {
        PeerLimits {
            blocks: Bucket::new(1000.0, 5000.0),
            requests: Bucket::new(500.0, 2000.0),
            ranges: Bucket::new(20.0, 200.0),
            txs: Bucket::new(5000.0, 20000.0),
            peers: Bucket::new(0.1, 3.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_limits_bursts() {
        let mut b = Bucket::new(1.0, 3.0);
        assert!(b.take(1.0) && b.take(1.0) && b.take(1.0));
        assert!(!b.take(1.0));
    }
}
