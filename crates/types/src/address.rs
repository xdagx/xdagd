//! 20-byte account addresses.
//!
//! One account space serves both worlds:
//! * XDAG keys own `hash160(compressed pubkey)` addresses (Base58Check text form);
//! * EVM keys own `keccak256(uncompressed pubkey)[12..]` addresses (0x-hex text form).
//!
//! Any 20-byte address can be written in either text form; they are the same
//! account.

use crate::{base58, Error};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Address(pub [u8; 20]);

impl Address {
    pub const ZERO: Address = Address([0u8; 20]);

    pub fn from_slice(s: &[u8]) -> Option<Self> {
        if s.len() != 20 {
            return None;
        }
        let mut b = [0u8; 20];
        b.copy_from_slice(s);
        Some(Address(b))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_base58(&self) -> String {
        base58::encode_check(&self.0)
    }

    pub fn from_base58(s: &str) -> Result<Self, Error> {
        let v = base58::decode_check(s).ok_or(Error::InvalidAddress)?;
        Address::from_slice(&v).ok_or(Error::InvalidAddress)
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }

    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or(Error::InvalidAddress)?;
        let v = hex::decode(s).map_err(|_| Error::InvalidAddress)?;
        Address::from_slice(&v).ok_or(Error::InvalidAddress)
    }

    /// Accepts Base58Check (XDAG style) or 0x-prefixed hex (EVM style).
    pub fn parse(s: &str) -> Result<Self, Error> {
        let s = s.trim();
        if s.starts_with("0x") || s.starts_with("0X") {
            Address::from_hex(s)
        } else {
            Address::from_base58(s)
        }
    }

    /// Encoding inside a 32-byte link field (wire order): bytes 4..24 hold the
    /// address reversed; bytes 0..4 are zero.
    pub fn write_field(&self, field: &mut [u8]) {
        field[0..4].fill(0);
        for i in 0..20 {
            field[4 + i] = self.0[19 - i];
        }
    }

    pub fn read_field(field: &[u8]) -> Self {
        let mut b = [0u8; 20];
        for i in 0..20 {
            b[i] = field[23 - i];
        }
        Address(b)
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.to_base58())
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

impl FromStr for Address {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Address::parse(s)
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Address::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_vectors_from_xdagj() {
        // BasicUtilsTest.pubAddress2Hash / testHexPubAddress2Hashlow
        let a = Address::from_base58("KD77RGFihFaqrJQrKK8MJ21hocJeq32Pf").unwrap();
        assert_eq!(hex::encode(a.0), "c7bc5b48517bf2da9e845eacebacf65008e9e763");
        assert_eq!(a.to_base58(), "KD77RGFihFaqrJQrKK8MJ21hocJeq32Pf");
        // BasicUtilsTest.hash2PubAddress
        let b = Address::from_hex("0x1eadb24287735969f08c33d5a410ca4aa2440fbc").unwrap();
        assert_eq!(b.to_base58(), "3oDMPTzmLvvy7mgkpvn1nhPDfW9tghrwB");
        assert_eq!(Address::parse("0x1eadb24287735969f08c33d5a410ca4aa2440fbc").unwrap(), b);
    }

    #[test]
    fn field_roundtrip() {
        let a = Address::from_base58("KD77RGFihFaqrJQrKK8MJ21hocJeq32Pf").unwrap();
        let mut f = [0xaau8; 32];
        a.write_field(&mut f);
        assert_eq!(Address::read_field(&f), a);
        assert_eq!(&f[0..4], &[0, 0, 0, 0]);
    }
}
