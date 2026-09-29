//! Ethereum transaction envelopes accepted by XDAG Nova:
//! legacy (EIP-155 only), EIP-2930 (type 1) and EIP-1559 (type 2).
//! Blob (type 3) and set-code (type 4) transactions are rejected.

use alloy_rlp::{Decodable, Encodable, Header};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, SECP256K1};
use sha3::{Digest, Keccak256};
use xdag_types::Address;

pub fn keccak(b: &[u8]) -> [u8; 32] {
    Keccak256::digest(b).into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessItem {
    pub address: Address,
    pub keys: Vec<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthTx {
    pub tx_type: u8,
    pub chain_id: u64,
    pub nonce: u64,
    /// Legacy/2930: gas price. 1559: max fee per gas.
    pub max_fee_per_gas: u128,
    /// 1559 only.
    pub max_priority_fee_per_gas: Option<u128>,
    pub gas_limit: u64,
    pub to: Option<Address>,
    /// Big-endian 256-bit value.
    pub value: [u8; 32],
    pub data: Vec<u8>,
    pub access_list: Vec<AccessItem>,
    pub sender: Address,
    pub hash: [u8; 32],
}

impl EthTx {
    pub fn value_u128(&self) -> Option<u128> {
        if self.value[..16].iter().any(|&b| b != 0) {
            return None;
        }
        Some(u128::from_be_bytes(self.value[16..].try_into().unwrap()))
    }

    /// Price actually paid per gas when the block base fee is `base_fee`.
    pub fn effective_gas_price(&self, base_fee: u128) -> u128 {
        match self.max_priority_fee_per_gas {
            None => self.max_fee_per_gas,
            Some(tip) => self.max_fee_per_gas.min(base_fee.saturating_add(tip)),
        }
    }
}

fn err<T>(m: &str) -> Result<T, String> {
    Err(m.to_string())
}

struct Rlp<'a> {
    buf: &'a [u8],
}

impl<'a> Rlp<'a> {
    fn list(buf: &mut &'a [u8]) -> Result<Rlp<'a>, String> {
        let h = Header::decode(buf).map_err(|e| e.to_string())?;
        if !h.list {
            return err("expected RLP list");
        }
        if buf.len() < h.payload_length {
            return err("truncated RLP list");
        }
        let (inner, rest) = buf.split_at(h.payload_length);
        *buf = rest;
        Ok(Rlp { buf: inner })
    }
    fn u64(&mut self) -> Result<u64, String> {
        u64::decode(&mut self.buf).map_err(|e| e.to_string())
    }
    fn u128(&mut self) -> Result<u128, String> {
        u128::decode(&mut self.buf).map_err(|e| e.to_string())
    }
    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let h = Header::decode(&mut self.buf).map_err(|e| e.to_string())?;
        if h.list {
            return err("expected RLP string");
        }
        if self.buf.len() < h.payload_length {
            return err("truncated RLP string");
        }
        let (v, rest) = self.buf.split_at(h.payload_length);
        self.buf = rest;
        Ok(v.to_vec())
    }
    fn u256(&mut self) -> Result<[u8; 32], String> {
        let b = self.bytes()?;
        if b.len() > 32 || (b.first() == Some(&0)) {
            return err("bad 256-bit integer");
        }
        let mut out = [0u8; 32];
        out[32 - b.len()..].copy_from_slice(&b);
        Ok(out)
    }
    fn to(&mut self) -> Result<Option<Address>, String> {
        let b = self.bytes()?;
        match b.len() {
            0 => Ok(None),
            20 => Ok(Address::from_slice(&b)),
            _ => err("bad destination"),
        }
    }
    fn access_list(&mut self) -> Result<Vec<AccessItem>, String> {
        let mut outer = Rlp::list(&mut self.buf)?;
        let mut items = vec![];
        while !outer.buf.is_empty() {
            let mut item = Rlp::list(&mut outer.buf)?;
            let a = item.bytes()?;
            let address = Address::from_slice(&a).ok_or("bad access list address")?;
            let mut keys_list = Rlp::list(&mut item.buf)?;
            let mut keys = vec![];
            while !keys_list.buf.is_empty() {
                let k = keys_list.bytes()?;
                if k.len() != 32 {
                    return err("bad access list key");
                }
                keys.push(k.try_into().unwrap());
            }
            if !item.buf.is_empty() {
                return err("trailing access list data");
            }
            items.push(AccessItem { address, keys });
        }
        Ok(items)
    }
    fn done(&self) -> Result<(), String> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            err("trailing RLP data")
        }
    }
}

fn recover(sighash: &[u8; 32], r: &[u8; 32], s: &[u8; 32], parity: u8) -> Result<Address, String> {
    // EIP-2: s must be in the lower half
    const HALF_N: [u8; 32] = [
        0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50,
        0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
    ];
    if s.as_slice() > HALF_N.as_slice() {
        return err("high-s signature");
    }
    let mut c = [0u8; 64];
    c[..32].copy_from_slice(r);
    c[32..].copy_from_slice(s);
    let rid = RecoveryId::try_from(parity as i32).map_err(|_| "bad recovery id".to_string())?;
    let sig = RecoverableSignature::from_compact(&c, rid).map_err(|e| e.to_string())?;
    let pk = SECP256K1.recover_ecdsa(&Message::from_digest(*sighash), &sig).map_err(|e| e.to_string())?;
    Ok(xdag_types::crypto::evm_address(&pk))
}

fn sig_part(b: Vec<u8>) -> Result<[u8; 32], String> {
    if b.len() > 32 {
        return err("bad signature scalar");
    }
    let mut out = [0u8; 32];
    out[32 - b.len()..].copy_from_slice(&b);
    Ok(out)
}

/// Decode and recover the sender of a raw (EIP-2718) transaction.
pub fn decode(raw: &[u8]) -> Result<EthTx, String> {
    if raw.is_empty() {
        return err("empty transaction");
    }
    let hash = keccak(raw);
    if raw[0] >= 0xc0 {
        return decode_legacy(raw, hash);
    }
    let ty = raw[0];
    let mut body = &raw[1..];
    let h = Header::decode(&mut body).map_err(|e| e.to_string())?;
    if !h.list || body.len() != h.payload_length {
        return err("malformed typed transaction");
    }
    let payload = body;
    let mut l = Rlp { buf: payload };
    let (chain_id, nonce, prio, max_fee, gas_limit, to, value, data, access_list) = match ty {
        1 => {
            let chain_id = l.u64()?;
            let nonce = l.u64()?;
            let gas_price = l.u128()?;
            let gas_limit = l.u64()?;
            let to = l.to()?;
            let value = l.u256()?;
            let data = l.bytes()?;
            let al = l.access_list()?;
            (chain_id, nonce, None, gas_price, gas_limit, to, value, data, al)
        }
        2 => {
            let chain_id = l.u64()?;
            let nonce = l.u64()?;
            let prio = l.u128()?;
            let max_fee = l.u128()?;
            let gas_limit = l.u64()?;
            let to = l.to()?;
            let value = l.u256()?;
            let data = l.bytes()?;
            let al = l.access_list()?;
            if prio > max_fee {
                return err("max priority fee above max fee");
            }
            (chain_id, nonce, Some(prio), max_fee, gas_limit, to, value, data, al)
        }
        3 => return err("blob transactions are not supported"),
        4 => return err("EIP-7702 transactions are not supported"),
        _ => return err("unknown transaction type"),
    };
    let consumed = payload.len() - l.buf.len();
    let unsigned_fields = &payload[..consumed];
    let parity = l.u64()?;
    let r = sig_part(l.bytes()?)?;
    let s = sig_part(l.bytes()?)?;
    l.done()?;
    if parity > 1 {
        return err("bad y parity");
    }
    let mut out = vec![ty];
    Header { list: true, payload_length: unsigned_fields.len() }.encode(&mut out);
    out.extend_from_slice(unsigned_fields);
    let sender = recover(&keccak(&out), &r, &s, parity as u8)?;
    Ok(EthTx {
        tx_type: ty,
        chain_id,
        nonce,
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: prio,
        gas_limit,
        to,
        value,
        data,
        access_list,
        sender,
        hash,
    })
}

fn decode_legacy(raw: &[u8], hash: [u8; 32]) -> Result<EthTx, String> {
    let mut buf = raw;
    let mut l = Rlp::list(&mut buf)?;
    if !buf.is_empty() {
        return err("trailing bytes after legacy transaction");
    }
    let start = l.buf;
    let nonce = l.u64()?;
    let gas_price = l.u128()?;
    let gas_limit = l.u64()?;
    let to = l.to()?;
    let value = l.u256()?;
    let data = l.bytes()?;
    let consumed = start.len() - l.buf.len();
    let unsigned_fields = &start[..consumed];
    let v = l.u64()?;
    let r = sig_part(l.bytes()?)?;
    let s = sig_part(l.bytes()?)?;
    l.done()?;
    if v < 35 {
        return err("pre-EIP-155 transactions are not accepted (no replay protection)");
    }
    let chain_id = (v - 35) / 2;
    let parity = ((v - 35) % 2) as u8;
    let mut payload = unsigned_fields.to_vec();
    chain_id.encode(&mut payload);
    0u8.encode(&mut payload);
    0u8.encode(&mut payload);
    let mut out = vec![];
    Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    let sender = recover(&keccak(&out), &r, &s, parity)?;
    Ok(EthTx {
        tx_type: 0,
        chain_id,
        nonce,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: None,
        gas_limit,
        to,
        value,
        data,
        access_list: vec![],
        sender,
        hash,
    })
}

/// Build and sign a transaction (tests, CLI, wallet).
#[allow(clippy::too_many_arguments)]
pub fn sign_eip1559(
    key: &xdag_types::KeyPair,
    chain_id: u64,
    nonce: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    to: Option<Address>,
    value: u128,
    data: &[u8],
) -> Vec<u8> {
    let mut fields = vec![];
    chain_id.encode(&mut fields);
    nonce.encode(&mut fields);
    max_priority_fee_per_gas.encode(&mut fields);
    max_fee_per_gas.encode(&mut fields);
    gas_limit.encode(&mut fields);
    match to {
        Some(a) => a.0.as_slice().encode(&mut fields),
        None => (&[] as &[u8]).encode(&mut fields),
    }
    value.encode(&mut fields);
    data.encode(&mut fields);
    Header { list: true, payload_length: 0 }.encode(&mut fields); // empty access list
    let mut unsigned = vec![2u8];
    Header { list: true, payload_length: fields.len() }.encode(&mut unsigned);
    unsigned.extend_from_slice(&fields);
    let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(keccak(&unsigned)), key.secret());
    let (rid, c) = sig.serialize_compact();
    let mut all = fields;
    (i32::from(rid) as u64).encode(&mut all);
    strip(&c[..32]).encode(&mut all);
    strip(&c[32..]).encode(&mut all);
    let mut out = vec![2u8];
    Header { list: true, payload_length: all.len() }.encode(&mut out);
    out.extend_from_slice(&all);
    out
}

/// Legacy EIP-155 transaction (what many tools still send).
#[allow(clippy::too_many_arguments)]
pub fn sign_legacy(
    key: &xdag_types::KeyPair,
    chain_id: u64,
    nonce: u64,
    gas_price: u128,
    gas_limit: u64,
    to: Option<Address>,
    value: u128,
    data: &[u8],
) -> Vec<u8> {
    let mut fields = vec![];
    nonce.encode(&mut fields);
    gas_price.encode(&mut fields);
    gas_limit.encode(&mut fields);
    match to {
        Some(a) => a.0.as_slice().encode(&mut fields),
        None => (&[] as &[u8]).encode(&mut fields),
    }
    value.encode(&mut fields);
    data.encode(&mut fields);
    let mut unsigned_payload = fields.clone();
    chain_id.encode(&mut unsigned_payload);
    0u8.encode(&mut unsigned_payload);
    0u8.encode(&mut unsigned_payload);
    let mut unsigned = vec![];
    Header { list: true, payload_length: unsigned_payload.len() }.encode(&mut unsigned);
    unsigned.extend_from_slice(&unsigned_payload);
    let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(keccak(&unsigned)), key.secret());
    let (rid, c) = sig.serialize_compact();
    let v = chain_id * 2 + 35 + i32::from(rid) as u64;
    let mut all = fields;
    v.encode(&mut all);
    strip(&c[..32]).encode(&mut all);
    strip(&c[32..]).encode(&mut all);
    let mut out = vec![];
    Header { list: true, payload_length: all.len() }.encode(&mut out);
    out.extend_from_slice(&all);
    out
}

fn strip(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    &b[i..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdag_types::KeyPair;

    #[test]
    fn eip1559_roundtrip_recovers_sender() {
        let k = KeyPair::random();
        let to = KeyPair::random().evm_address();
        let raw = sign_eip1559(&k, 30822, 7, 2_000_000_000, 3_000_000_000, 21000, Some(to), 12345, b"\x01\x02");
        let tx = decode(&raw).unwrap();
        assert_eq!(tx.sender, k.evm_address());
        assert_eq!(tx.nonce, 7);
        assert_eq!(tx.chain_id, 30822);
        assert_eq!(tx.to, Some(to));
        assert_eq!(tx.value_u128(), Some(12345));
        assert_eq!(tx.data, vec![1, 2]);
        assert_eq!(tx.effective_gas_price(0), 2_000_000_000);
    }

    #[test]
    fn legacy_roundtrip_and_create() {
        let k = KeyPair::random();
        let raw = sign_legacy(&k, 30822, 0, 1_000_000_000, 100_000, None, 0, &[0x60, 0x00]);
        let tx = decode(&raw).unwrap();
        assert_eq!(tx.sender, k.evm_address());
        assert_eq!(tx.to, None);
        assert_eq!(tx.chain_id, 30822);
    }

    #[test]
    fn known_mainnet_legacy_vector() {
        // EIP-155 example transaction (chain id 1), signed by 0x9d8A62f656a8d1615C1294fd71e9CFb3E4855A4F
        let raw = hex::decode("f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83").unwrap();
        let tx = decode(&raw).unwrap();
        assert_eq!(tx.chain_id, 1);
        assert_eq!(tx.sender.to_hex(), "0x9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0x03, 0xc0]).is_err());
        assert!(decode(&[0x02, 0xc1, 0x80]).is_err());
    }
}
