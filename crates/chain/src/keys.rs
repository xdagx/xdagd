//! Key layouts of the chain tables.

use xdag_types::{Address, HashLow};

pub fn height(h: u64) -> Vec<u8> {
    h.to_be_bytes().to_vec()
}

pub fn time_index(epoch: u64, hl: &HashLow) -> Vec<u8> {
    let mut k = Vec::with_capacity(32);
    k.extend_from_slice(&epoch.to_be_bytes());
    k.extend_from_slice(&hl.0);
    k
}

/// History subjects are either addresses (20 bytes) or blocks (24 bytes).
pub fn history(subject: &[u8], main_height: u64, seq: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(subject.len() + 12);
    k.extend_from_slice(subject);
    k.extend_from_slice(&main_height.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

pub fn storage_slot(a: &Address, slot: &[u8; 32]) -> Vec<u8> {
    let mut k = Vec::with_capacity(52);
    k.extend_from_slice(&a.0);
    k.extend_from_slice(slot);
    k
}

pub const META_CHAIN: &[u8] = b"chain";
pub const META_RX: &[u8] = b"randomx";
pub const META_HISTORY_SEQ: &[u8] = b"history_seq";
/// Present while a snapshot import is in progress (see `snapshot::import`).
pub const META_SNAPSHOT_IMPORT: &[u8] = b"snapshot_import";
