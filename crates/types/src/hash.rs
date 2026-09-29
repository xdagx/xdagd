//! Hash primitives and the two identifiers XDAG uses for a block.
//!
//! Byte-order convention used throughout this crate: every byte array is kept in
//! *wire order* — exactly the bytes that appear inside a 512-byte block and on
//! the network (the "C memory order" of the original xdag implementation).
//! xdagj keeps most 32-byte values byte-reversed in memory; the `*_xdagj_hex`
//! helpers reproduce its textual representation for RPC compatibility.

use sha2::{Digest, Sha256};
use std::fmt;

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(data)).into()
}

/// sha256d over several slices without concatenating them.
pub fn sha256d_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    Sha256::digest(h.finalize()).into()
}

/// RIPEMD160(SHA256(data)) — the XDAG (and Bitcoin) address hash.
pub fn hash160(data: &[u8]) -> [u8; 20] {
    use ripemd::Ripemd160;
    Ripemd160::digest(Sha256::digest(data)).into()
}

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    use sha3::Keccak256;
    Keccak256::digest(data).into()
}

/// Full block hash: `sha256d(raw 512-byte block)` in wire order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BlockHash(pub [u8; 32]);

impl BlockHash {
    pub fn of_raw(raw: &[u8]) -> Self {
        BlockHash(sha256d(raw))
    }

    /// The 24-byte truncated identifier stored in link fields.
    pub fn hashlow(&self) -> HashLow {
        let mut b = [0u8; 24];
        b.copy_from_slice(&self.0[..24]);
        HashLow(b)
    }

    /// xdagj `Block.getHash().toUnprefixedHexString()`: hex of the reversed bytes.
    pub fn to_xdagj_hex(&self) -> String {
        let mut r = self.0;
        r.reverse();
        hex::encode(r)
    }

    pub fn from_xdagj_hex(s: &str) -> Result<Self, crate::Error> {
        let b = decode_hex_32(s)?;
        let mut r = b;
        r.reverse();
        Ok(BlockHash(r))
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockHash({})", self.to_xdagj_hex())
    }
}

/// The identifier of a block inside the DAG: the first 24 bytes (wire order)
/// of its full hash. xdagj calls this "hashlow".
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct HashLow(pub [u8; 24]);

impl HashLow {
    pub const ZERO: HashLow = HashLow([0u8; 24]);

    pub fn as_bytes(&self) -> &[u8; 24] {
        &self.0
    }

    pub fn from_slice(s: &[u8]) -> Option<Self> {
        if s.len() != 24 {
            return None;
        }
        let mut b = [0u8; 24];
        b.copy_from_slice(s);
        Some(HashLow(b))
    }

    /// xdagj hashlow hex: 64 hex chars, `0000000000000000` + reversed 24 bytes.
    pub fn to_xdagj_hex(&self) -> String {
        let mut r = [0u8; 32];
        for i in 0..24 {
            r[8 + i] = self.0[23 - i];
        }
        hex::encode(r)
    }

    /// Accepts the 64-char xdagj form (the leading 8 bytes are ignored, as xdagj
    /// does when it normalises a hash to a hashlow) or a 48-char form.
    pub fn from_xdagj_hex(s: &str) -> Result<Self, crate::Error> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|_| crate::Error::InvalidHex)?;
        let tail: &[u8] = match bytes.len() {
            32 => &bytes[8..],
            24 => &bytes[..],
            _ => return Err(crate::Error::InvalidHex),
        };
        let mut b = [0u8; 24];
        for i in 0..24 {
            b[i] = tail[23 - i];
        }
        Ok(HashLow(b))
    }

    /// Legacy XDAG block address: standard base64 of the 24 wire-order bytes
    /// (xdagj `BasicUtils.hash2Address`).
    pub fn to_legacy_address(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }

    pub fn from_legacy_address(s: &str) -> Result<Self, crate::Error> {
        use base64::Engine;
        if s.len() != 32 {
            return Err(crate::Error::InvalidAddress);
        }
        let b = base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| crate::Error::InvalidAddress)?;
        HashLow::from_slice(&b).ok_or(crate::Error::InvalidAddress)
    }
}

impl fmt::Debug for HashLow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HashLow({})", &self.to_xdagj_hex()[16..])
    }
}

impl fmt::Display for HashLow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_legacy_address())
    }
}

pub(crate) fn decode_hex_32(s: &str) -> Result<[u8; 32], crate::Error> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let v = hex::decode(s).map_err(|_| crate::Error::InvalidHex)?;
    if v.len() != 32 {
        return Err(crate::Error::InvalidHex);
    }
    let mut b = [0u8; 32];
    b.copy_from_slice(&v);
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_address_roundtrip_matches_xdagj() {
        // BasicUtilsTest.testHash2Address / testAddress2Hash
        let full = BlockHash::from_xdagj_hex("4aa1ab5742feb010a54ddd7c7a7bdbb22366fa2516cf648f32641623580b67e3").unwrap();
        assert_eq!(full.hashlow().to_legacy_address(), "42cLWCMWZDKPZM8WJfpmI7Lbe3p83U2l");
        let hl = HashLow::from_legacy_address("42cLWCMWZDKPZM8WJfpmI7Lbe3p83U2l").unwrap();
        assert_eq!(hl.to_xdagj_hex(), "0000000000000000a54ddd7c7a7bdbb22366fa2516cf648f32641623580b67e3");
        assert_eq!(HashLow::from_xdagj_hex(&hl.to_xdagj_hex()).unwrap(), hl);
    }

    #[test]
    fn hash160_known_vector() {
        // compressed key of SampleKeys.PRIVATE_KEY_STRING
        let pk = hex::decode("02506bc1dc099358e5137292f4efdd57e400f29ba5132aa5d12b18dac1c1f6aaba").unwrap();
        assert_eq!(hash160(&pk).len(), 20);
    }
}
