//! The 512-byte XDAG block: parsing (bit-compatible with xdagj 0.8.4),
//! signature digests, and a builder for new blocks.

use crate::amount::CAmount;
use crate::crypto::{pubkey_address, pubkey_from_x, KeyPair, PublicKey, Signature};
use crate::field::{field_type_at, set_field_type, FieldType};
use crate::hash::{sha256d_parts, BlockHash, HashLow};
use crate::{Address, Error};
use std::sync::Arc;

pub const BLOCK_SIZE: usize = 512;
pub const FIELD_COUNT: usize = 16;
/// Index of the last field; holds the mining nonce on main-block candidates.
pub const MAX_LINKS: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkTarget {
    Block(HashLow),
    Address(Address),
}

/// A reference field (IN / OUT / INPUT / OUTPUT / COINBASE).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Link {
    pub kind: FieldType,
    pub index: u8,
    pub target: LinkTarget,
    /// Raw C-unit amount from the field.
    pub amount: CAmount,
}

impl Link {
    pub fn is_address(&self) -> bool {
        matches!(self.target, LinkTarget::Address(_))
    }
    pub fn block(&self) -> Option<HashLow> {
        match self.target {
            LinkTarget::Block(h) => Some(h),
            _ => None,
        }
    }
    pub fn address(&self) -> Option<Address> {
        match self.target {
            LinkTarget::Address(a) => Some(a),
            _ => None,
        }
    }
}

/// A parsed block. Cheap to clone (raw bytes are shared).
#[derive(Clone, Debug)]
pub struct Block {
    raw: Arc<[u8; BLOCK_SIZE]>,
    hash: BlockHash,
    pub transport: u64,
    pub type_word: u64,
    pub time: u64,
    /// Header fee word as xdagj reads it: nano-XDAG, signed.
    pub header_fee: i64,
    /// IN and INPUT links, in field order.
    pub inputs: Vec<Link>,
    /// OUT, OUTPUT and COINBASE links, in field order.
    pub outputs: Vec<Link>,
    pub coinbase: Option<Address>,
    pub tx_nonce: Option<u64>,
    pub remark: Option<[u8; 32]>,
    pub pubkeys: Vec<PublicKey>,
    /// Input signatures with the index of their `r` field, in xdagj's
    /// `LinkedHashMap` order (first insertion wins the position, duplicates merge).
    pub insigs: Vec<(Signature, usize)>,
    pub outsig: Option<Signature>,
    pub nonce: Option<[u8; 32]>,
    /// Nova: root of the extension payload (field type 0xF).
    pub ext_root: Option<[u8; 32]>,
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl Eq for Block {}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Block {
    /// Parse a raw block. Mirrors `io.xdag.core.Block.parse()` including its
    /// failure modes: invalid public keys, out-of-range signature scalars and
    /// link amounts `>= 2^63` all make the block unparseable.
    pub fn parse(raw_bytes: &[u8]) -> Result<Block, Error> {
        if raw_bytes.len() != BLOCK_SIZE {
            return Err(Error::InvalidBlockSize(raw_bytes.len()));
        }
        let mut raw = [0u8; BLOCK_SIZE];
        raw.copy_from_slice(raw_bytes);
        Self::parse_owned(Arc::new(raw))
    }

    pub fn parse_owned(raw: Arc<[u8; BLOCK_SIZE]>) -> Result<Block, Error> {
        let hash = BlockHash::of_raw(&raw[..]);
        let f = |i: usize| &raw[i * 32..i * 32 + 32];
        let h = f(0);
        let transport = le64(&h[0..]);
        let type_word = le64(&h[8..]);
        let time = le64(&h[16..]);
        let header_fee = le64(&h[24..]) as i64;

        let mut b = Block {
            raw: raw.clone(),
            hash,
            transport,
            type_word,
            time,
            header_fee,
            inputs: vec![],
            outputs: vec![],
            coinbase: None,
            tx_nonce: None,
            remark: None,
            pubkeys: vec![],
            insigs: vec![],
            outsig: None,
            nonce: None,
            ext_root: None,
        };

        for i in 1..FIELD_COUNT {
            let ft = field_type_at(type_word, i);
            let data = f(i);
            match ft {
                FieldType::TxNonce => b.tx_nonce = Some(le64(&data[24..])),
                FieldType::In | FieldType::Out => {
                    let amount = CAmount(le64(&data[24..]));
                    amount.to_nano_legacy()?; // xdagj throws while constructing the Address
                    let link = Link { kind: ft, index: i as u8, target: LinkTarget::Block(HashLow::from_slice(&data[..24]).unwrap()), amount };
                    if ft == FieldType::In {
                        b.inputs.push(link)
                    } else {
                        b.outputs.push(link)
                    }
                }
                FieldType::Input | FieldType::Output | FieldType::Coinbase => {
                    let amount = CAmount(le64(&data[24..]));
                    amount.to_nano_legacy()?;
                    let addr = Address::read_field(data);
                    let link = Link { kind: ft, index: i as u8, target: LinkTarget::Address(addr), amount };
                    match ft {
                        FieldType::Input => b.inputs.push(link),
                        FieldType::Output => b.outputs.push(link),
                        _ => {
                            b.coinbase = Some(addr);
                            b.outputs.push(link);
                        }
                    }
                }
                FieldType::Remark => {
                    let mut r = [0u8; 32];
                    r.copy_from_slice(data);
                    b.remark = Some(r);
                }
                FieldType::SignIn | FieldType::SignOut => {
                    // Pair with the first later field of the same type (xdagj quirk:
                    // pairs may overlap, e.g. (13,14) and (14,15)).
                    for j in (i + 1)..FIELD_COUNT {
                        if field_type_at(type_word, j) == ft {
                            let sig = Signature::from_fields(data, f(j))?;
                            if ft == FieldType::SignIn {
                                if let Some(e) = b.insigs.iter_mut().find(|(s, _)| *s == sig) {
                                    e.1 = i;
                                } else {
                                    b.insigs.push((sig, i));
                                }
                            } else {
                                b.outsig = Some(sig);
                            }
                            break;
                        }
                    }
                    if i == MAX_LINKS && ft == FieldType::SignIn {
                        let mut n = [0u8; 32];
                        n.copy_from_slice(data);
                        b.nonce = Some(n);
                    }
                }
                FieldType::PublicKey0 | FieldType::PublicKey1 => {
                    b.pubkeys.push(pubkey_from_x(data, ft == FieldType::PublicKey1)?);
                }
                FieldType::Extension => {
                    let mut r = [0u8; 32];
                    r.copy_from_slice(data);
                    b.ext_root = Some(r);
                }
                FieldType::Nonce | FieldType::Head | FieldType::HeadTest | FieldType::Snapshot => {}
            }
        }
        Ok(b)
    }

    pub fn raw(&self) -> &[u8; BLOCK_SIZE] {
        &self.raw
    }

    pub fn raw_arc(&self) -> Arc<[u8; BLOCK_SIZE]> {
        self.raw.clone()
    }

    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    pub fn hashlow(&self) -> HashLow {
        self.hash.hashlow()
    }

    pub fn field(&self, i: usize) -> &[u8] {
        &self.raw[i * 32..i * 32 + 32]
    }

    pub fn field_type(&self, i: usize) -> FieldType {
        field_type_at(self.type_word, i)
    }

    /// Header type of field 0 (HEAD on mainnet, HEAD_TEST elsewhere).
    pub fn header_type(&self) -> FieldType {
        self.field_type(0)
    }

    /// xdagj `getLinks()`: inputs followed by outputs.
    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.inputs.iter().chain(self.outputs.iter())
    }

    pub fn block_links(&self) -> impl Iterator<Item = HashLow> + '_ {
        self.links().filter_map(|l| l.block())
    }

    /// xdagj `getSubRawData(length)`: the first `length + 1` fields verbatim,
    /// later fields verbatim unless they are signature fields (zeroed).
    pub fn sub_raw_data(&self, length: i64) -> [u8; BLOCK_SIZE] {
        sub_raw_data(&self.raw, self.type_word, length)
    }

    /// xdagj `getOutsigIndex()`: one past the first SIGN_OUT nibble (nibble 0 included).
    pub fn outsig_index(&self) -> i64 {
        let mut i = 1i64;
        let mut t = self.type_word;
        while i < FIELD_COUNT as i64 && (t & 0xf) != 5 {
            t >>= 4;
            i += 1;
        }
        i
    }

    /// Digest the output signature commits to, for a candidate public key.
    pub fn outsig_digest(&self, pk: &PublicKey) -> [u8; 32] {
        let sub = self.sub_raw_data(self.outsig_index() - 2);
        sha256d_parts(&[&sub, &pk.serialize()])
    }

    /// xdagj `verifiedKeys()`: public keys (with multiplicity) that verify one
    /// of the block's input signatures or its output signature.
    ///
    /// Errors when the block carries public keys but no output signature — the
    /// Java code dereferences a null signature there and the block is rejected.
    pub fn verified_keys(&self) -> Result<Vec<PublicKey>, Error> {
        let mut res = Vec::new();
        let serialized: Vec<[u8; 33]> = self.pubkeys.iter().map(|k| k.serialize()).collect();
        for (sig, idx) in &self.insigs {
            let sub = self.sub_raw_data(*idx as i64 - 1);
            for (pk, ser) in self.pubkeys.iter().zip(&serialized) {
                let d = sha256d_parts(&[&sub, ser]);
                if sig.verify(&d, pk) {
                    res.push(*pk);
                }
            }
        }
        if !self.pubkeys.is_empty() {
            let outsig = self.outsig.ok_or(Error::MissingOutputSignature)?;
            let sub = self.sub_raw_data(self.outsig_index() - 2);
            for (pk, ser) in self.pubkeys.iter().zip(&serialized) {
                let d = sha256d_parts(&[&sub, ser]);
                if outsig.verify(&d, pk) {
                    res.push(*pk);
                }
            }
        }
        Ok(res)
    }

    /// True if `pk` produced this block's output signature (used to decide
    /// which key may spend a block balance, and whether a block is "ours").
    pub fn outsig_signed_by(&self, pk: &PublicKey) -> bool {
        match self.outsig {
            Some(sig) => sig.verify(&self.outsig_digest(pk), pk),
            None => false,
        }
    }

    /// Sum of all 64 little-endian words (the C "sums" used by the sync protocol).
    pub fn sum(&self) -> u64 {
        self.raw.as_chunks::<8>().0.iter().fold(0u64, |acc, w| acc.wrapping_add(le64(w)))
    }

    pub fn has_in(&self) -> bool {
        self.inputs.iter().any(|l| l.kind == FieldType::In)
    }

    /// xdagj `isAccountTx`: exactly one INPUT and no IN.
    pub fn is_account_tx(&self) -> bool {
        !self.has_in() && self.inputs.iter().filter(|l| l.kind == FieldType::Input).count() == 1
    }

    /// xdagj `isMainTxBlock`: at least one IN and no INPUT.
    pub fn is_main_tx(&self) -> bool {
        self.has_in() && !self.inputs.iter().any(|l| l.kind == FieldType::Input)
    }

    pub fn is_tx(&self) -> bool {
        self.is_account_tx() || self.is_main_tx()
    }

    /// The account spent by an account transaction.
    pub fn account_input(&self) -> Option<(Address, CAmount)> {
        if !self.is_account_tx() {
            return None;
        }
        self.inputs.iter().find(|l| l.kind == FieldType::Input).and_then(|l| l.address().map(|a| (a, l.amount)))
    }

    pub fn remark_string(&self) -> String {
        match &self.remark {
            Some(r) => String::from_utf8_lossy(r).trim_matches(char::from(0)).trim().to_string(),
            None => String::new(),
        }
    }

    /// The pool/miner address embedded in the last 20 bytes of the nonce
    /// (xdagj `getNonce().slice(12, 20)`).
    pub fn nonce_address(&self) -> Option<Address> {
        self.nonce.map(|n| Address::from_slice(&n[12..32]).unwrap())
    }

    pub fn signer_addresses(&self) -> Vec<Address> {
        self.pubkeys.iter().map(pubkey_address).collect()
    }
}

pub fn sub_raw_data(raw: &[u8; BLOCK_SIZE], type_word: u64, length: i64) -> [u8; BLOCK_SIZE] {
    let mut res = [0u8; BLOCK_SIZE];
    let prefix = ((length + 1).max(0) as usize).min(FIELD_COUNT) * 32;
    res[..prefix].copy_from_slice(&raw[..prefix]);
    let start = (length + 1).max(0) as usize;
    for i in start..FIELD_COUNT {
        if field_type_at(type_word, i).is_sign() {
            continue;
        }
        res[i * 32..i * 32 + 32].copy_from_slice(&raw[i * 32..i * 32 + 32]);
    }
    res
}

/// Everything needed to lay out and sign a new block.
#[derive(Clone, Debug)]
pub struct BlockTemplate {
    pub header_type: FieldType,
    pub time: u64,
    /// Header fee word (nano). xdagj wallets put the *extra* fee here.
    pub header_fee: u64,
    pub tx_nonce: Option<u64>,
    /// (field type, target, amount) in the order they should appear.
    pub links: Vec<(FieldType, LinkTarget, CAmount)>,
    pub remark: Option<[u8; 32]>,
    pub ext_root: Option<[u8; 32]>,
    /// Keys that produce SIGN_IN pairs (their public keys are included).
    pub sign_in: Vec<KeyPair>,
    /// Key producing the mandatory SIGN_OUT pair.
    pub sign_out: Option<KeyPair>,
    /// Include the SIGN_OUT key's public key (needed when it must be verifiable
    /// from this block alone, e.g. account inputs).
    pub include_out_pubkey: bool,
    /// Mining nonce placed in field 15 with type SIGN_IN.
    pub mining_nonce: Option<[u8; 32]>,
}

impl BlockTemplate {
    pub fn new(header_type: FieldType, time: u64) -> Self {
        BlockTemplate {
            header_type,
            time,
            header_fee: 0,
            tx_nonce: None,
            links: vec![],
            remark: None,
            ext_root: None,
            sign_in: vec![],
            sign_out: None,
            include_out_pubkey: false,
            mining_nonce: None,
        }
    }

    /// Lay out, sign and parse. Signatures are produced over exactly the
    /// digests xdagj verifies (`getSubRawData`), so field order is irrelevant.
    pub fn build(&self) -> Result<Block, Error> {
        let mut raw = [0u8; BLOCK_SIZE];
        let mut tw: u64 = 0;
        let mut n = 0usize;
        let put = |raw: &mut [u8; BLOCK_SIZE], tw: &mut u64, n: &mut usize, t: FieldType, data: &[u8; 32]| -> Result<usize, Error> {
            let limit = if self.mining_nonce.is_some() { MAX_LINKS } else { FIELD_COUNT };
            if *n >= limit {
                return Err(Error::TooManyFields);
            }
            set_field_type(tw, *n, t);
            raw[*n * 32..*n * 32 + 32].copy_from_slice(data);
            *n += 1;
            Ok(*n - 1)
        };
        // header placeholder (filled once the type word is final)
        put(&mut raw, &mut tw, &mut n, self.header_type, &[0u8; 32])?;
        if let Some(nonce) = self.tx_nonce {
            let mut d = [0u8; 32];
            d[24..].copy_from_slice(&nonce.to_le_bytes());
            put(&mut raw, &mut tw, &mut n, FieldType::TxNonce, &d)?;
        }
        for (t, target, amount) in &self.links {
            let mut d = [0u8; 32];
            match target {
                LinkTarget::Block(h) => d[..24].copy_from_slice(&h.0),
                LinkTarget::Address(a) => a.write_field(&mut d),
            }
            d[24..].copy_from_slice(&amount.0.to_le_bytes());
            put(&mut raw, &mut tw, &mut n, *t, &d)?;
        }
        if let Some(r) = self.remark {
            put(&mut raw, &mut tw, &mut n, FieldType::Remark, &r)?;
        }
        if let Some(r) = self.ext_root {
            put(&mut raw, &mut tw, &mut n, FieldType::Extension, &r)?;
        }
        let mut pubkeys: Vec<PublicKey> = self.sign_in.iter().map(|k| *k.public()).collect();
        if self.include_out_pubkey {
            if let Some(k) = &self.sign_out {
                if !pubkeys.contains(k.public()) {
                    pubkeys.push(*k.public());
                }
            }
        }
        for pk in &pubkeys {
            let c = pk.serialize();
            let mut d = [0u8; 32];
            d.copy_from_slice(&c[1..]);
            let t = if c[0] == 0x03 { FieldType::PublicKey1 } else { FieldType::PublicKey0 };
            put(&mut raw, &mut tw, &mut n, t, &d)?;
        }
        let mut sig_slots: Vec<(usize, FieldType, &KeyPair)> = vec![];
        for k in &self.sign_in {
            let i = put(&mut raw, &mut tw, &mut n, FieldType::SignIn, &[0u8; 32])?;
            put(&mut raw, &mut tw, &mut n, FieldType::SignIn, &[0u8; 32])?;
            sig_slots.push((i, FieldType::SignIn, k));
        }
        if let Some(k) = &self.sign_out {
            let i = put(&mut raw, &mut tw, &mut n, FieldType::SignOut, &[0u8; 32])?;
            put(&mut raw, &mut tw, &mut n, FieldType::SignOut, &[0u8; 32])?;
            sig_slots.push((i, FieldType::SignOut, k));
        }
        if let Some(nonce) = self.mining_nonce {
            set_field_type(&mut tw, MAX_LINKS, FieldType::SignIn);
            raw[MAX_LINKS * 32..].copy_from_slice(&nonce);
        }
        // header
        raw[0..8].copy_from_slice(&0u64.to_le_bytes());
        raw[8..16].copy_from_slice(&tw.to_le_bytes());
        raw[16..24].copy_from_slice(&self.time.to_le_bytes());
        raw[24..32].copy_from_slice(&self.header_fee.to_le_bytes());
        // signatures, in field order
        let first_out = sig_slots.iter().find(|s| s.1 == FieldType::SignOut).map(|s| s.0);
        for (idx, t, k) in &sig_slots {
            let len = if *t == FieldType::SignOut { first_out.unwrap() as i64 - 1 } else { *idx as i64 - 1 };
            let sub = sub_raw_data(&raw, tw, len);
            let d = sha256d_parts(&[&sub, &k.compressed()]);
            let sig = k.sign(&d);
            raw[idx * 32..idx * 32 + 32].copy_from_slice(&sig.r);
            raw[(idx + 1) * 32..(idx + 1) * 32 + 32].copy_from_slice(&sig.s);
        }
        Block::parse(&raw)
    }
}

/// Replace the mining nonce (field 15) of a candidate block.
pub fn with_nonce(raw: &[u8; BLOCK_SIZE], nonce: &[u8; 32]) -> [u8; BLOCK_SIZE] {
    let mut r = *raw;
    r[MAX_LINKS * 32..].copy_from_slice(nonce);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::Nano;

    const TX_RAW: &str = concat!(
        "000000000000000038324654050000004d3782fa780100000000000000000000",
        "c86357a2f57bb9df4f8b43b7a60e24d1ccc547c606f2d7980000000000000000",
        "afa5fec4f56f7935125806e235d5280d7092c6840f35b397000000000a000000",
        "a08202c3f60123df5e3a973e21a2dd0418b9926a2eb7c4fc000000000a000000",
        "08b65d2e2816c0dea73bf1b226c95c2ae3bc683574f559bbc5dd484864b1dbeb",
        "f02a041d5f7ff83a69c0e35e7eeeb64496f76f69958485787d2c50fd8d9614e6",
        "7c2b69c79eddeff5d05b2bfc1ee487b9c691979d315586e9928c04ab3ace15bb",
        "3866f1a25ed00aa18dde715d2a4fc05147d16300c31fefc0f3ebe4d77c63fcbb",
        "ec6ece350f6be4c84b8705d3b49866a83986578a3a20e876eefe74de0c094bac",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
    );

    const MAIN_RAW: &str = concat!(
        "00000000000000003833550000000040ffff89507a0100000000000000000000",
        "c86357a2f57bb9df4f8b43b7a60e24d1ccc547c606f2d7980000000000000000",
        "07488d5de5ee0058014320b25d700d8b3ba4d08c532cf3950000000000000000",
        "c983f2413c65c4bfee0379919e8d1f67f133f98929bc267b0000000000000000",
        "a1f4af8d31449d5bb1acf7b6a27124eff6c348968ad427e676529d71c0d8bc0c",
        "4ca945ca7cf4ec778be7c9afb9076f75361cee1124cd6a438b73ac47a7e2a6f1",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "1aae3b19aa0b8c24f6837c10bfc981a303b54f978a314c5baf69e7a4f44eb5a6",
    );

    #[test]
    fn parse_c_era_transaction_block() {
        let raw = hex::decode(TX_RAW).unwrap();
        let b = Block::parse(&raw).unwrap();
        // type word 0x0000000554463238: fields 1..3 OUT/IN..., keys, signatures
        assert_eq!(b.type_word, 0x0000_0005_5446_3238);
        assert_eq!(b.field_type(0), FieldType::HeadTest);
        assert_eq!(b.inputs.len() + b.outputs.len(), 3);
        assert_eq!(b.pubkeys.len(), 1);
        assert!(b.outsig.is_some());
        assert_eq!(b.insigs.len(), 1);
        // C-era (pre low-S) signatures may or may not verify; parsing must succeed.
        let _ = b.verified_keys();
        assert_eq!(b.sum(), raw.as_chunks::<8>().0.iter().fold(0u64, |a, w| a.wrapping_add(le64(w))));
    }

    #[test]
    fn parse_main_block_with_nonce() {
        let raw = hex::decode(MAIN_RAW).unwrap();
        let b = Block::parse(&raw).unwrap();
        assert!(b.nonce.is_some());
        assert_eq!(b.time & 0xffff, 0xffff);
        assert_eq!(b.field_type(MAX_LINKS), FieldType::SignIn);
        assert!(b.outsig.is_some());
    }

    #[test]
    fn built_account_tx_verifies() {
        let k = KeyPair::random();
        let to = KeyPair::random().address();
        let mut t = BlockTemplate::new(FieldType::HeadTest, 0x1690_0001_2345);
        t.tx_nonce = Some(7);
        t.links.push((FieldType::Input, LinkTarget::Address(k.address()), Nano::from_xdag(5).to_camount_legacy()));
        t.links.push((FieldType::Output, LinkTarget::Address(to), Nano::from_xdag(5).to_camount_legacy()));
        t.sign_out = Some(k.clone());
        t.include_out_pubkey = true;
        let b = t.build().unwrap();
        assert!(b.is_account_tx());
        assert_eq!(b.tx_nonce, Some(7));
        assert_eq!(b.account_input().unwrap().0, k.address());
        let keys = b.verified_keys().unwrap();
        assert!(keys.contains(k.public()));
        assert!(b.outsig_signed_by(k.public()));
        // round-trip through raw bytes keeps the hash
        assert_eq!(Block::parse(b.raw()).unwrap().hash(), b.hash());
    }

    #[test]
    fn built_multi_key_block_verifies_all_keys() {
        let k1 = KeyPair::random();
        let k2 = KeyPair::random();
        let out = KeyPair::random();
        let mut t = BlockTemplate::new(FieldType::Head, 0x1694_0001_0000);
        t.links.push((FieldType::In, LinkTarget::Block(HashLow([1u8; 24])), CAmount(1 << 32)));
        t.links.push((FieldType::Output, LinkTarget::Address(out.address()), CAmount(1 << 32)));
        t.sign_in = vec![k1.clone(), k2.clone()];
        t.sign_out = Some(out.clone());
        t.include_out_pubkey = true;
        let b = t.build().unwrap();
        let keys = b.verified_keys().unwrap();
        assert!(keys.contains(k1.public()));
        assert!(keys.contains(k2.public()));
        assert!(keys.contains(out.public()));
        // overlapping-pair quirk: (a,a+1),(a+1,c),(c,c+1)
        assert_eq!(b.insigs.len(), 3);
    }

    #[test]
    fn rejects_huge_amount_and_bad_key() {
        let k = KeyPair::random();
        let mut t = BlockTemplate::new(FieldType::Head, 1);
        t.links.push((FieldType::Output, LinkTarget::Address(k.address()), CAmount(1 << 63)));
        t.sign_out = Some(k);
        assert!(matches!(t.build(), Err(Error::AmountOverflow)));
    }

    #[test]
    fn mining_nonce_not_covered_by_signature() {
        let k = KeyPair::random();
        let mut t = BlockTemplate::new(FieldType::Head, 0x1694_0000_ffff);
        t.links.push((FieldType::Coinbase, LinkTarget::Address(k.address()), CAmount(0)));
        t.sign_out = Some(k.clone());
        t.mining_nonce = Some([7u8; 32]);
        let b = t.build().unwrap();
        let b2 = Block::parse(&with_nonce(b.raw(), &[9u8; 32])).unwrap();
        assert!(b2.outsig_signed_by(k.public()));
        assert_ne!(b.hash(), b2.hash());
        assert_eq!(b2.nonce, Some([9u8; 32]));
    }
}
