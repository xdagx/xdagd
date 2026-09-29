//! Amounts.
//!
//! * [`CAmount`] — the fixed-point unit carried in block fields since the C
//!   implementation: 1 XDAG = 2^32 units.
//! * [`Nano`] — nano-XDAG (1 XDAG = 10^9), xdagj's `XAmount`, used for all
//!   balances.
//!
//! xdagj converts between the two through `double` arithmetic
//! (`XAmount.ofXAmount` / `XAmount.toXAmount`). Those conversions are *part of
//! consensus* on the existing chain (address balances are persisted as
//! `CAmount` and re-read through the lossy path on every update), so the
//! `*_legacy` functions below reproduce them bit-for-bit, including the IEEE-754
//! rounding. The Nova fork switches balances to exact integer arithmetic.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const NANO_PER_XDAG: u64 = 1_000_000_000;
/// 10^9: scale factor between nano-XDAG and the 18-decimal EVM unit.
pub const WEI_PER_NANO: u128 = 1_000_000_000;
pub const C_UNITS_PER_XDAG: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CAmount(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Nano(pub u64);

impl CAmount {
    pub const ZERO: CAmount = CAmount(0);

    /// Bit-exact replica of xdagj `XAmount.ofXAmount(long)`.
    ///
    /// ```java
    /// long first = xdag >> 32; long temp = xdag - (first << 32);
    /// double tem = temp / Math.pow(2, 32);
    /// new BigDecimal(first + tem).movePointRight(9).setScale(0, HALF_UP).longValueExact();
    /// ```
    /// Values with the top bit set are negative Java longs and make xdagj throw
    /// `XdagOverFlowException`; they are reported as [`Error::AmountOverflow`].
    pub fn to_nano_legacy(self) -> Result<Nano, Error> {
        let c = self.0;
        if c >> 63 != 0 {
            return Err(Error::AmountOverflow);
        }
        let first = c >> 32;
        let temp = c - (first << 32);
        let tem = temp as f64 / 4294967296.0; // exact: power-of-two divisor
        let s = first as f64 + tem; // IEEE round-to-nearest-even, as in Java
        Ok(Nano(f64_times_1e9_round_half_up(s)))
    }

    /// Exact conversion (Nova rules): round-half-up of `c * 10^9 / 2^32`.
    pub fn to_nano_exact(self) -> Nano {
        let num = self.0 as u128 * NANO_PER_XDAG as u128;
        let q = num >> 32;
        let r = num & 0xffff_ffff;
        Nano((q + u128::from(r >= 0x8000_0000)) as u64)
    }

    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }
}

impl Nano {
    pub const ZERO: Nano = Nano(0);

    pub const fn from_xdag(x: u64) -> Nano {
        Nano(x * NANO_PER_XDAG)
    }

    pub const fn from_milli(m: u64) -> Nano {
        Nano(m * 1_000_000)
    }

    /// Bit-exact replica of xdagj `XAmount.toXAmount()`:
    /// `xdag2amount(toDecimal(9, XDAG).doubleValue())`.
    pub fn to_camount_legacy(self) -> CAmount {
        let n = self.0;
        // BigDecimal.doubleValue(): correctly rounded nearest double of n / 1e9.
        let d = if n < (1u64 << 52) {
            n as f64 / 1e9 // single correctly-rounded IEEE division of exact operands
        } else {
            format!("{}.{:09}", n / NANO_PER_XDAG, n % NANO_PER_XDAG).parse::<f64>().expect("decimal literal")
        };
        // xdag2amount(double)
        let amount = d.floor();
        let int_part = amount as i64 as u64;
        let res = int_part.wrapping_shl(32);
        let frac = (d - amount) * 4294967296.0;
        let tmp = frac.ceil() as i64 as u64;
        CAmount(res.wrapping_add(tmp))
    }

    /// Exact conversion (Nova rules): ceil is not needed because 2^32/10^9 > 1,
    /// every nano value has a unique nearest C amount.
    pub fn to_camount_exact(self) -> CAmount {
        let num = (self.0 as u128) << 32;
        let q = num / NANO_PER_XDAG as u128;
        let r = num % NANO_PER_XDAG as u128;
        CAmount((q + u128::from(r * 2 >= NANO_PER_XDAG as u128)) as u64)
    }

    pub fn checked_add(self, o: Nano) -> Option<Nano> {
        self.0.checked_add(o.0).map(Nano)
    }

    pub fn checked_sub(self, o: Nano) -> Option<Nano> {
        self.0.checked_sub(o.0).map(Nano)
    }

    pub fn saturating_sub(self, o: Nano) -> Nano {
        Nano(self.0.saturating_sub(o.0))
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn to_wei(self) -> u128 {
        self.0 as u128 * WEI_PER_NANO
    }

    /// Floor conversion from the 18-decimal unit.
    pub fn from_wei_floor(wei: u128) -> Nano {
        Nano((wei / WEI_PER_NANO).min(u64::MAX as u128) as u64)
    }

    /// `"12.345000000"` — always 9 decimals, as xdagj's `toDecimal(9, XDAG).toPlainString()`.
    pub fn to_xdag_string(self) -> String {
        format!("{}.{:09}", self.0 / NANO_PER_XDAG, self.0 % NANO_PER_XDAG)
    }

    /// Exact decimal parser: up to 9 fractional digits, no exponent, no sign.
    pub fn parse_xdag(s: &str) -> Result<Nano, Error> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::InvalidAmount);
        }
        let (int_s, frac_s) = match s.split_once('.') {
            Some((a, b)) => (a, b),
            None => (s, ""),
        };
        if frac_s.len() > 9
            || (int_s.is_empty() && frac_s.is_empty())
            || !int_s.bytes().all(|c| c.is_ascii_digit())
            || !frac_s.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(Error::InvalidAmount);
        }
        let int: u64 = if int_s.is_empty() { 0 } else { int_s.parse().map_err(|_| Error::InvalidAmount)? };
        let mut frac: u64 = 0;
        for (i, c) in frac_s.bytes().enumerate() {
            frac += (c - b'0') as u64 * 10u64.pow(8 - i as u32);
        }
        int.checked_mul(NANO_PER_XDAG).and_then(|v| v.checked_add(frac)).map(Nano).ok_or(Error::InvalidAmount)
    }
}

impl fmt::Display for Nano {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_xdag_string())
    }
}

/// Exact value of `s * 10^9` rounded half-up (Java `BigDecimal(double)`
/// followed by `movePointRight(9).setScale(0, HALF_UP)`), for finite `s >= 0`.
fn f64_times_1e9_round_half_up(s: f64) -> u64 {
    debug_assert!(s.is_finite() && s >= 0.0);
    if s == 0.0 {
        return 0;
    }
    let bits = s.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mant, exp) = if exp_bits == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp_bits - 1075) };
    let num = mant as u128 * NANO_PER_XDAG as u128;
    if exp >= 0 {
        return (num << exp) as u64;
    }
    let shift = (-exp) as u32;
    if shift >= 128 {
        return 0;
    }
    let q = num >> shift;
    let rem = num & ((1u128 << shift) - 1);
    let half = 1u128 << (shift - 1);
    (q + u128::from(rem >= half)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdag2amount_vectors() {
        // BasicUtilsTest.testXdag2amount — via exact decimal nano amounts
        assert_eq!(Nano(972_800_000_000).to_camount_legacy().0, 4178144185549);
        assert_eq!(Nano(51_200_000_000).to_camount_legacy().0, 219902325556);
        assert_eq!(Nano(100_000_000_000).to_camount_legacy().0, 429496729600);
    }

    #[test]
    fn of_xamount_vectors() {
        // amount2xdag vectors, expressed in nano after HALF_UP rounding
        assert_eq!(CAmount(4178144185548).to_nano_legacy().unwrap().0, 972_800_000_000);
        assert_eq!(CAmount(219902325556).to_nano_legacy().unwrap().0, 51_200_000_000);
        assert_eq!(CAmount(6399501272).to_nano_legacy().unwrap().0, 1_490_000_000);
        assert_eq!(CAmount(44796508898).to_nano_legacy().unwrap().0, 10_430_000_000);
        assert_eq!(CAmount(42949672960).to_nano_legacy().unwrap().0, 10_000_000_000);
        assert_eq!(CAmount(4398046511104).to_nano_legacy().unwrap().0, 1_024_000_000_000);
        assert_eq!(CAmount(1 << 39).to_nano_legacy().unwrap(), Nano::from_xdag(128));
        assert!(CAmount(1 << 63).to_nano_legacy().is_err());
    }

    #[test]
    fn legacy_roundtrip_small_values_is_identity() {
        for n in [0u64, 1, 2, 3, 999, 100_000_000, 123_456_789_012, 1_999_999_999_999_999] {
            let c = Nano(n).to_camount_legacy();
            assert_eq!(c.to_nano_legacy().unwrap().0, n, "n={n}");
        }
    }

    #[test]
    fn exact_conversions() {
        assert_eq!(CAmount(1 << 32).to_nano_exact(), Nano::from_xdag(1));
        assert_eq!(Nano::from_xdag(3).to_camount_exact(), CAmount(3 << 32));
        assert_eq!(Nano(1).to_camount_exact().to_nano_exact(), Nano(1));
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(Nano::parse_xdag("1.5").unwrap(), Nano(1_500_000_000));
        assert_eq!(Nano::parse_xdag("0.000000001").unwrap(), Nano(1));
        assert_eq!(Nano::parse_xdag(".25").unwrap(), Nano(250_000_000));
        assert!(Nano::parse_xdag("1.0000000001").is_err());
        assert!(Nano::parse_xdag("-1").is_err());
        assert!(Nano::parse_xdag("1e3").is_err());
        assert_eq!(Nano(1_005_000_000).to_xdag_string(), "1.005000000");
    }
}
