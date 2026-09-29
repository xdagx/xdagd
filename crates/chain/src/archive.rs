//! Pre-snapshot history archive.
//!
//! Every xdagj upgrade so far restarted the chain from a balance snapshot,
//! so the transaction history before the snapshot vanished from the network.
//! Raw blocks of that period still exist (C xdag `storage/**.dat` files are
//! plain concatenations of 512-byte blocks, and xdagj's `BLOCK` column family
//! can be dumped the same way with `tools/xdagj-exporter`). This module imports
//! them into separate tables and indexes every transfer by address/block, so
//! explorers and wallets can show the complete history again.
//!
//! Archived blocks are *not* executed: the post-snapshot state stays exactly
//! what the network agreed on; the archive is a read-only record.

use std::io::Read;
use std::path::Path;

use xdag_storage::{Db, Table, WriteBatch};
use xdag_types::{Address, Block, FieldType, HashLow, LinkTarget, Nano};

use crate::{ChainError, Result};

fn subject_of(t: &LinkTarget) -> Vec<u8> {
    match t {
        LinkTarget::Address(a) => {
            let mut v = vec![1u8];
            v.extend_from_slice(&a.0);
            v
        }
        LinkTarget::Block(h) => {
            let mut v = vec![2u8];
            v.extend_from_slice(&h.0);
            v
        }
    }
}

/// Index one archived block. Returns the number of history rows written.
pub fn index_block(batch: &mut WriteBatch, b: &Block) -> usize {
    let h = b.hashlow();
    batch.put(Table::Archive, h.0.to_vec(), b.raw().to_vec());
    let mut n = 0;
    let inputs: Vec<&xdag_types::Link> = b.inputs.iter().collect();
    for (i, l) in b.links().enumerate() {
        let Ok(amount) = l.amount.to_nano_legacy() else { continue };
        if amount.is_zero() {
            continue;
        }
        let incoming = matches!(l.kind, FieldType::Out | FieldType::Output);
        // counterparty: first input for receivers, first output for senders
        let cp = if incoming {
            inputs.first().map(|x| subject_of(&x.target))
        } else {
            b.outputs.iter().find(|o| !matches!(o.kind, FieldType::Coinbase)).map(|x| subject_of(&x.target))
        };
        let mut key = subject_of(&l.target);
        key.extend_from_slice(&b.time.to_be_bytes());
        key.extend_from_slice(&h.0);
        key.push(i as u8);
        let mut val = Vec::with_capacity(64);
        val.push(1u8); // record version
        val.push(if incoming { 1 } else { 0 });
        val.extend_from_slice(&amount.0.to_le_bytes());
        match cp {
            Some(c) => {
                val.push(c.len() as u8);
                val.extend_from_slice(&c);
            }
            None => val.push(0),
        }
        val.extend_from_slice(&b.remark.unwrap_or([0u8; 32]));
        batch.put(Table::ArchiveHistory, key, val);
        n += 1;
    }
    n
}

/// Import a file of concatenated raw 512-byte blocks. The 8-byte transport
/// header is cleared (it is not part of the block identity on the xdagj network).
pub fn import_raw_file(db: &Db, path: &Path) -> Result<usize> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path).map_err(|e| ChainError::Other(e.to_string()))?);
    let mut buf = [0u8; 512];
    let mut batch = WriteBatch::new();
    let mut count = 0usize;
    loop {
        match f.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(ChainError::Other(e.to_string())),
        }
        buf[..8].fill(0);
        if let Ok(b) = Block::parse(&buf) {
            index_block(&mut batch, &b);
            count += 1;
        }
        if batch.len() > 50_000 {
            db.write(std::mem::take(&mut batch), false)?;
        }
    }
    db.write(batch, true)?;
    Ok(count)
}

/// Human-readable archived history of an address (Base58/0x) or block
/// (legacy base64 address or hash).
pub fn history(db: &Db, target: &str, limit: usize) -> Result<Vec<String>> {
    let subject = if let Ok(a) = Address::parse(target) {
        subject_of(&LinkTarget::Address(a))
    } else if let Ok(h) = HashLow::from_legacy_address(target) {
        subject_of(&LinkTarget::Block(h))
    } else if let Ok(h) = HashLow::from_xdagj_hex(target) {
        subject_of(&LinkTarget::Block(h))
    } else {
        return Err(ChainError::Invalid("not an address or block".into()));
    };
    let rows = db.scan_prefix(Table::ArchiveHistory, &subject, limit)?;
    let mut out = vec![];
    for (k, v) in rows {
        let off = subject.len();
        let time = u64::from_be_bytes(k[off..off + 8].try_into().unwrap());
        let tx = HashLow::from_slice(&k[off + 8..off + 32]).unwrap();
        let incoming = v[1] == 1;
        let amount = Nano(u64::from_le_bytes(v[2..10].try_into().unwrap()));
        let cpl = v[10] as usize;
        let cp = &v[11..11 + cpl];
        let cp = match cp.first() {
            Some(1) => Address::from_slice(&cp[1..]).map(|a| a.to_base58()).unwrap_or_default(),
            Some(2) => HashLow::from_slice(&cp[1..]).map(|h| h.to_legacy_address()).unwrap_or_default(),
            _ => String::new(),
        };
        out.push(format!(
            "{}  {}  {:>20}  {}  tx {}",
            time_str(time),
            if incoming { "IN " } else { "OUT" },
            amount.to_xdag_string(),
            cp,
            tx.to_legacy_address()
        ));
    }
    Ok(out)
}

fn time_str(t: u64) -> String {
    let ms = xdag_types::time::xdag_to_ms(t);
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdag_types::{BlockTemplate, CAmount, KeyPair};

    #[test]
    fn archive_import_and_history() {
        let (db, _g) = Db::open_temporary().unwrap();
        let k = KeyPair::random();
        let to = KeyPair::random().address();
        let mut t = BlockTemplate::new(FieldType::Head, 0x16a0_0000_1234);
        t.tx_nonce = Some(1);
        t.links.push((FieldType::Input, LinkTarget::Address(k.address()), CAmount(5 << 32)));
        t.links.push((FieldType::Output, LinkTarget::Address(to), CAmount(5 << 32)));
        t.sign_out = Some(k.clone());
        t.include_out_pubkey = true;
        let b = t.build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blocks.dat");
        let mut raw = b.raw().to_vec();
        raw.extend_from_slice(b.raw());
        std::fs::write(&p, raw).unwrap();
        assert_eq!(import_raw_file(&db, &p).unwrap(), 2);
        let h = history(&db, &to.to_base58(), 10).unwrap();
        assert_eq!(h.len(), 1, "same block twice is indexed once");
        assert!(h[0].contains("IN "));
        assert!(h[0].contains("5.000000000"));
        let h = history(&db, &k.address().to_base58(), 10).unwrap();
        assert!(h[0].contains("OUT"));
    }
}
