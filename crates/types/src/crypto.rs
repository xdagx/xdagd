//! secp256k1 keys and ECDSA as used by XDAG.
//!
//! Semantics mirrored from xdagj-crypto 0.1.2:
//! * signing is RFC6979-deterministic with low-S normalisation;
//! * verification rejects high-S signatures;
//! * a signature scalar outside `[1, n-1]` is a *parse* error (xdagj throws
//!   while decoding the block, so such a block is invalid as a whole).

use crate::hash::{hash160, keccak256};
use crate::{Address, Error};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId, Signature as EcdsaSig};
use secp256k1::{Message, SECP256K1};
use std::fmt;

pub use secp256k1::{PublicKey, SecretKey};

/// secp256k1 group order n, big-endian.
const CURVE_N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b,
    0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

/// An (r, s) pair as it appears in two 32-byte block fields (big-endian).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature {
    pub r: [u8; 32],
    pub s: [u8; 32],
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sig(r={}, s={})", hex::encode(self.r), hex::encode(self.s))
    }
}

fn scalar_in_range(v: &[u8; 32]) -> bool {
    v.iter().any(|&b| b != 0) && v.as_slice() < CURVE_N.as_slice()
}

impl Signature {
    /// xdagj `Signature.create(r, s, 0)` semantics, including the special case
    /// `r == 0 && s == 0 → (1, 1)` applied by `Block.parse` beforehand.
    pub fn from_fields(r: &[u8], s: &[u8]) -> Result<Self, Error> {
        let mut rr = [0u8; 32];
        let mut ss = [0u8; 32];
        rr.copy_from_slice(r);
        ss.copy_from_slice(s);
        if rr == [0u8; 32] && ss == [0u8; 32] {
            rr[31] = 1;
            ss[31] = 1;
        }
        if !scalar_in_range(&rr) || !scalar_in_range(&ss) {
            return Err(Error::InvalidSignature);
        }
        Ok(Signature { r: rr, s: ss })
    }

    fn to_ecdsa(self) -> Option<EcdsaSig> {
        let mut c = [0u8; 64];
        c[..32].copy_from_slice(&self.r);
        c[32..].copy_from_slice(&self.s);
        EcdsaSig::from_compact(&c).ok()
    }

    /// Verify against a 32-byte digest. High-S signatures never verify
    /// (libsecp256k1 and xdagj agree on this).
    pub fn verify(&self, digest: &[u8; 32], pk: &PublicKey) -> bool {
        let Some(sig) = self.to_ecdsa() else { return false };
        let msg = Message::from_digest(*digest);
        SECP256K1.verify_ecdsa(&msg, &sig, pk).is_ok()
    }
}

/// A 65-byte `r || s || v` recoverable signature (handshakes, Nova txs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RecSignature(pub [u8; 65]);

impl RecSignature {
    pub fn recover(&self, digest: &[u8; 32]) -> Result<PublicKey, Error> {
        let v = self.0[64];
        if v > 1 {
            return Err(Error::InvalidSignature);
        }
        let rid = RecoveryId::try_from(v as i32).map_err(|_| Error::InvalidSignature)?;
        let sig = RecoverableSignature::from_compact(&self.0[..64], rid).map_err(|_| Error::InvalidSignature)?;
        // reject high-S like xdagj's Signer.verify
        let plain = sig.to_standard();
        let mut normalized = plain;
        normalized.normalize_s();
        if normalized != plain {
            return Err(Error::InvalidSignature);
        }
        let msg = Message::from_digest(*digest);
        SECP256K1.recover_ecdsa(&msg, &sig).map_err(|_| Error::InvalidSignature)
    }

    pub fn r_s(&self) -> Signature {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&self.0[..32]);
        s.copy_from_slice(&self.0[32..64]);
        Signature { r, s }
    }
}

impl fmt::Debug for RecSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecSig({})", hex::encode(self.0))
    }
}

/// A key pair with cached public data.
#[derive(Clone)]
pub struct KeyPair {
    secret: SecretKey,
    public: PublicKey,
}

impl PartialEq for KeyPair {
    fn eq(&self, other: &Self) -> bool {
        self.public == other.public
    }
}
impl Eq for KeyPair {}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyPair({})", self.address())
    }
}

impl KeyPair {
    pub fn from_secret(secret: SecretKey) -> Self {
        let public = PublicKey::from_secret_key(SECP256K1, &secret);
        KeyPair { secret, public }
    }

    pub fn from_secret_bytes(b: &[u8]) -> Result<Self, Error> {
        let mut k = [0u8; 32];
        if b.len() > 32 {
            return Err(Error::InvalidKey);
        }
        // BigInteger-style: left-pad shorter encodings
        k[32 - b.len()..].copy_from_slice(b);
        let secret = SecretKey::from_byte_array(&k).map_err(|_| Error::InvalidKey)?;
        Ok(Self::from_secret(secret))
    }

    pub fn random() -> Self {
        let secret = SecretKey::new(&mut rand::thread_rng());
        Self::from_secret(secret)
    }

    pub fn secret(&self) -> &SecretKey {
        &self.secret
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.secret_bytes()
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    pub fn compressed(&self) -> [u8; 33] {
        self.public.serialize()
    }

    /// XDAG address: hash160 of the compressed public key.
    pub fn address(&self) -> Address {
        pubkey_address(&self.public)
    }

    /// EVM address: keccak256 of the uncompressed key (sans prefix), last 20 bytes.
    pub fn evm_address(&self) -> Address {
        evm_address(&self.public)
    }

    /// RFC6979 + low-S, the (r, s) pair written into block fields.
    pub fn sign(&self, digest: &[u8; 32]) -> Signature {
        let msg = Message::from_digest(*digest);
        let sig = SECP256K1.sign_ecdsa(&msg, &self.secret);
        let c = sig.serialize_compact();
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&c[..32]);
        s.copy_from_slice(&c[32..]);
        Signature { r, s }
    }

    pub fn sign_recoverable(&self, digest: &[u8; 32]) -> RecSignature {
        let msg = Message::from_digest(*digest);
        let sig = SECP256K1.sign_ecdsa_recoverable(&msg, &self.secret);
        let (rid, c) = sig.serialize_compact();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&c);
        out[64] = i32::from(rid) as u8;
        RecSignature(out)
    }
}

pub fn pubkey_address(pk: &PublicKey) -> Address {
    Address(hash160(&pk.serialize()))
}

pub fn evm_address(pk: &PublicKey) -> Address {
    let unc = pk.serialize_uncompressed();
    let h = keccak256(&unc[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

/// Reconstruct a public key from the 32-byte x coordinate stored in a
/// PUBLIC_KEY_0/1 field (big-endian) and the parity implied by the field type.
pub fn pubkey_from_x(x: &[u8], odd: bool) -> Result<PublicKey, Error> {
    let mut c = [0u8; 33];
    c[0] = if odd { 0x03 } else { 0x02 };
    c[1..].copy_from_slice(x);
    PublicKey::from_slice(&c).map_err(|_| Error::InvalidKey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_key_matches_xdagj() {
        // io.xdag.crypto.SampleKeys
        let kp = KeyPair::from_secret_bytes(&hex::decode("a392604efc2fad9c0b3da43b5f698a2e3f270f170d859912be0d54742275c5f6").unwrap()).unwrap();
        assert_eq!(hex::encode(kp.compressed()), "02506bc1dc099358e5137292f4efdd57e400f29ba5132aa5d12b18dac1c1f6aaba");
        // EVM address matches web3j's SampleKeys (xdagj's SampleKeys.ADDRESS is unused/stale).
        assert_eq!(kp.evm_address().to_hex(), "0xef678007d18427e6022059dbc264f27507cd1ffc");
        // hash160 of the compressed generator point (private key 1), a Bitcoin vector.
        let one = KeyPair::from_secret_bytes(&[1u8]).unwrap();
        assert_eq!(one.address().to_hex(), "0x751e76e8199196d454941c45d1b3a323f1433bd6");
    }

    #[test]
    fn sign_verify_and_high_s_rejected() {
        let kp = KeyPair::random();
        let d = crate::hash::sha256d(b"hello");
        let sig = kp.sign(&d);
        assert!(sig.verify(&d, kp.public()));
        // flip s to n - s (high-S) → must not verify
        let mut s = num_sub(&CURVE_N, &sig.s);
        let high = Signature { r: sig.r, s };
        assert!(!high.verify(&d, kp.public()));
        s[31] ^= 1;
        let _ = s;
        let rec = kp.sign_recoverable(&d);
        assert_eq!(rec.recover(&d).unwrap(), *kp.public());
    }

    #[test]
    fn scalar_bounds() {
        assert!(Signature::from_fields(&[0u8; 32], &[0u8; 32]).is_ok()); // (1,1)
        let mut one = [0u8; 32];
        one[31] = 1;
        assert!(Signature::from_fields(&[0u8; 32], &one).is_err());
        assert!(Signature::from_fields(&CURVE_N, &one).is_err());
    }

    fn num_sub(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut borrow = 0i32;
        for i in (0..32).rev() {
            let v = a[i] as i32 - b[i] as i32 - borrow;
            if v < 0 {
                out[i] = (v + 256) as u8;
                borrow = 1;
            } else {
                out[i] = v as u8;
                borrow = 0;
            }
        }
        out
    }
}
