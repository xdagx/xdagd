//! Block difficulty.

pub use ruint::aliases::U256;

/// Cumulative (chain) difficulty. xdagj uses `BigInteger`; 256 bits is ample.
pub type Difficulty = U256;

/// xdagj `BasicUtils.getDiffByHash`: `(2^128 - 1) / top96(hash)` where
/// `top96` is the little-endian integer formed by wire bytes 20..32.
/// (A zero divisor is unreachable in practice; like C xdag we return the max.)
pub fn hash_difficulty(h: &[u8; 32]) -> u128 {
    let mut v: u128 = 0;
    for i in (20..32).rev() {
        v = (v << 8) | h[i] as u128;
    }
    if v == 0 {
        return u128::MAX;
    }
    u128::MAX / v
}

pub fn to_u256(v: u128) -> Difficulty {
    Difficulty::from(v)
}

/// xdagj renders difficulties with `toQuantityJsonHex`: `0x` + lowercase hex, no leading zeros.
pub fn to_quantity_hex(d: Difficulty) -> String {
    format!("{:#x}", d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_uses_top_96_bits() {
        let mut h = [0u8; 32];
        h[31] = 0x01; // top96 = 2^88
        assert_eq!(hash_difficulty(&h), u128::MAX >> 88);
        let mut low = [0xffu8; 32];
        low[20..].fill(0);
        low[20] = 1; // top96 = 1
        assert_eq!(hash_difficulty(&low), u128::MAX);
    }
}
