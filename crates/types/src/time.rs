//! XDAG time: 64-bit count of 1/1024 seconds since the Unix epoch.
//! The low `epoch_bits` (16 on mainnet, i.e. 64 s) select the position inside a
//! main-block epoch; candidate main blocks carry the last tick of their epoch.

use std::time::{SystemTime, UNIX_EPOCH};

/// Bit-exact replica of xdagj `XdagTime.msToXdagtimestamp`:
/// `(long) Math.ceil((double)(ms << 10) / 1000 + 0.5)`.
pub fn ms_to_xdag(ms: u64) -> u64 {
    let v = (ms << 10) as f64;
    (v / 1000.0 + 0.5).ceil() as u64
}

pub fn xdag_to_ms(t: u64) -> u64 {
    t.wrapping_mul(1000) >> 10
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_xdag() -> u64 {
    ms_to_xdag(now_ms())
}

/// Epoch arithmetic parameterised by the number of low bits per epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epochs {
    pub bits: u32,
}

impl Epochs {
    pub const MAINNET: Epochs = Epochs { bits: 16 };

    pub fn mask(self) -> u64 {
        (1u64 << self.bits) - 1
    }
    pub fn epoch(self, t: u64) -> u64 {
        t >> self.bits
    }
    pub fn end_of_epoch(self, t: u64) -> u64 {
        t | self.mask()
    }
    pub fn is_end_of_epoch(self, t: u64) -> bool {
        t & self.mask() == self.mask()
    }
    pub fn start_of_epoch(self, epoch: u64) -> u64 {
        epoch << self.bits
    }
    /// Length of an epoch in xdag ticks.
    pub fn period(self) -> u64 {
        1u64 << self.bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let t = ms_to_xdag(1_700_000_000_000);
        assert!(xdag_to_ms(t).abs_diff(1_700_000_000_000) <= 1);
        let e = Epochs::MAINNET;
        assert_eq!(e.end_of_epoch(0x1234_5678), 0x1234_ffff);
        assert!(e.is_end_of_epoch(0x1234_ffff));
        assert_eq!(e.epoch(0x1234_ffff), 0x1234);
    }
}
