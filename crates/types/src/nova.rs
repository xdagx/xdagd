//! Nova extension payloads.
//!
//! A Nova *batch block* is an ordinary 512-byte XDAG block whose field of type
//! [`FieldType::Extension`](crate::FieldType::Extension) holds
//! `sha256d(payload)`. The payload travels next to the block and carries many
//! user-signed transactions, so one DAG vertex settles thousands of
//! transfers instead of one:
//!
//! ```text
//! payload := "XNP1" | count:u32le | { kind:u8 | len:u32le | bytes[len] }*
//! kind 1  := NativeTransfer (below)
//! kind 2  := EIP-2718 typed or legacy RLP Ethereum transaction
//! ```

use crate::amount::Nano;
use crate::crypto::{pubkey_address, KeyPair, PublicKey, RecSignature};
use crate::hash::sha256d;
use crate::{Address, Error};

pub const PAYLOAD_MAGIC: &[u8; 4] = b"XNP1";
pub const NATIVE_TX_DOMAIN: &[u8] = b"XDAG/NOVA/TRANSFER/v1";
pub const MAX_REMARK: usize = 32;

pub const KIND_NATIVE: u8 = 1;
pub const KIND_EVM: u8 = 2;

/// A compact native transfer between accounts (sender pays `amount + fee`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTransfer {
    pub chain_id: u64,
    /// Must equal the sender's executed-transaction count + 1 (same rule as
    /// legacy account transactions).
    pub nonce: u64,
    pub to: Address,
    pub amount: Nano,
    pub fee: Nano,
    pub remark: Vec<u8>,
    pub sig: RecSignature,
}

impl NativeTransfer {
    fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.remark.len());
        b.push(1u8);
        b.extend_from_slice(&self.chain_id.to_le_bytes());
        b.extend_from_slice(&self.nonce.to_le_bytes());
        b.extend_from_slice(&self.to.0);
        b.extend_from_slice(&self.amount.0.to_le_bytes());
        b.extend_from_slice(&self.fee.0.to_le_bytes());
        b.push(self.remark.len() as u8);
        b.extend_from_slice(&self.remark);
        b
    }

    pub fn signing_digest(&self) -> [u8; 32] {
        let mut d = NATIVE_TX_DOMAIN.to_vec();
        d.extend_from_slice(&self.body());
        sha256d(&d)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_signed(key: &KeyPair, chain_id: u64, nonce: u64, to: Address, amount: Nano, fee: Nano, remark: &[u8]) -> Result<Self, Error> {
        if remark.len() > MAX_REMARK {
            return Err(Error::Payload("remark longer than 32 bytes".into()));
        }
        let mut tx = NativeTransfer { chain_id, nonce, to, amount, fee, remark: remark.to_vec(), sig: RecSignature([0u8; 65]) };
        tx.sig = key.sign_recoverable(&tx.signing_digest());
        Ok(tx)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.body();
        b.extend_from_slice(&self.sig.0);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        let bad = |m: &str| Error::Payload(m.to_string());
        if b.len() < 1 + 8 + 8 + 20 + 8 + 8 + 1 + 65 {
            return Err(bad("native tx too short"));
        }
        if b[0] != 1 {
            return Err(bad("unknown native tx version"));
        }
        let rd = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let chain_id = rd(1);
        let nonce = rd(9);
        let to = Address::from_slice(&b[17..37]).unwrap();
        let amount = Nano(rd(37));
        let fee = Nano(rd(45));
        let rlen = b[53] as usize;
        if rlen > MAX_REMARK || b.len() != 54 + rlen + 65 {
            return Err(bad("bad native tx length"));
        }
        let remark = b[54..54 + rlen].to_vec();
        let mut sig = [0u8; 65];
        sig.copy_from_slice(&b[54 + rlen..]);
        Ok(NativeTransfer { chain_id, nonce, to, amount, fee, remark, sig: RecSignature(sig) })
    }

    pub fn tx_hash(&self) -> [u8; 32] {
        sha256d(&self.encode())
    }

    pub fn recover_signer(&self) -> Result<(PublicKey, Address), Error> {
        let pk = self.sig.recover(&self.signing_digest())?;
        Ok((pk, pubkey_address(&pk)))
    }
}

/// One entry of a payload. EVM transactions stay opaque here; the EVM crate
/// decodes and verifies them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NovaTx {
    Native(NativeTransfer),
    Evm(Vec<u8>),
}

impl NovaTx {
    pub fn kind(&self) -> u8 {
        match self {
            NovaTx::Native(_) => KIND_NATIVE,
            NovaTx::Evm(_) => KIND_EVM,
        }
    }

    pub fn encode_inner(&self) -> Vec<u8> {
        match self {
            NovaTx::Native(t) => t.encode(),
            NovaTx::Evm(b) => b.clone(),
        }
    }
}

pub fn encode_payload(txs: &[NovaTx]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + txs.len() * 180);
    out.extend_from_slice(PAYLOAD_MAGIC);
    out.extend_from_slice(&(txs.len() as u32).to_le_bytes());
    for tx in txs {
        let inner = tx.encode_inner();
        out.push(tx.kind());
        out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        out.extend_from_slice(&inner);
    }
    out
}

pub fn decode_payload(b: &[u8], max_txs: usize) -> Result<Vec<NovaTx>, Error> {
    let bad = |m: &str| Error::Payload(m.to_string());
    if b.len() < 8 || &b[..4] != PAYLOAD_MAGIC {
        return Err(bad("bad payload magic"));
    }
    let count = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    if count == 0 || count > max_txs {
        return Err(bad("bad payload tx count"));
    }
    let mut txs = Vec::with_capacity(count);
    let mut o = 8usize;
    for _ in 0..count {
        if o + 5 > b.len() {
            return Err(bad("truncated payload"));
        }
        let kind = b[o];
        let len = u32::from_le_bytes(b[o + 1..o + 5].try_into().unwrap()) as usize;
        o += 5;
        if len == 0 || o + len > b.len() {
            return Err(bad("truncated payload entry"));
        }
        let inner = &b[o..o + len];
        o += len;
        txs.push(match kind {
            KIND_NATIVE => NovaTx::Native(NativeTransfer::decode(inner)?),
            KIND_EVM => NovaTx::Evm(inner.to_vec()),
            _ => return Err(bad("unknown payload entry kind")),
        });
    }
    if o != b.len() {
        return Err(bad("trailing bytes in payload"));
    }
    Ok(txs)
}

pub fn payload_root(payload: &[u8]) -> [u8; 32] {
    sha256d(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_transfer_roundtrip_and_recover() {
        let k = KeyPair::random();
        let to = KeyPair::random().address();
        let tx = NativeTransfer::new_signed(&k, 30822, 1, to, Nano::from_xdag(3), Nano::from_milli(100), b"hi").unwrap();
        let enc = tx.encode();
        let dec = NativeTransfer::decode(&enc).unwrap();
        assert_eq!(dec, tx);
        assert_eq!(dec.recover_signer().unwrap().1, k.address());
        let mut tampered = dec.clone();
        tampered.amount = Nano::from_xdag(4);
        assert_ne!(tampered.recover_signer().map(|x| x.1).ok(), Some(k.address()));
    }

    #[test]
    fn payload_roundtrip() {
        let k = KeyPair::random();
        let tx = NativeTransfer::new_signed(&k, 1, 1, k.address(), Nano(5), Nano(1), b"").unwrap();
        let p = encode_payload(&[NovaTx::Native(tx.clone()), NovaTx::Evm(vec![1, 2, 3])]);
        let d = decode_payload(&p, 10).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], NovaTx::Native(tx));
        assert!(decode_payload(&p[..p.len() - 1], 10).is_err());
        assert!(decode_payload(&p, 1).is_err());
    }
}
