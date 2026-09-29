//! "Sums" index of the C/xdagj synchronisation protocol.
//!
//! Four levels of 256-entry arrays (16 bytes per entry: wrapping sum of the
//! block words, total size) keyed by the time bytes 5, 4, 3 and 2. A peer
//! compares 16-way summaries of a time range and descends into the ranges that
//! differ.

use xdag_storage::Table;

use crate::overlay::Overlay;
use crate::Result;

fn level_key(time: u64, level: usize) -> Vec<u8> {
    // level 0: root (indexed by time>>40), level 1: byte 5 (indexed by time>>32), ...
    let mut k = vec![level as u8];
    for i in 0..level {
        k.push(((time >> (40 - 8 * i)) & 0xff) as u8);
    }
    k
}

fn index_at(time: u64, level: usize) -> usize {
    ((time >> (40 - 8 * level)) & 0xff) as usize
}

pub fn add_block(ov: &mut Overlay, time: u64, sum: u64) -> Result<()> {
    add(ov, time, sum, 512, false)
}

pub fn remove_block(ov: &mut Overlay, time: u64, sum: u64) -> Result<()> {
    add(ov, time, sum, 512, true)
}

fn add(ov: &mut Overlay, time: u64, sum: u64, size: u64, remove: bool) -> Result<()> {
    for level in 0..4 {
        let key = level_key(time, level);
        let mut buf = ov.get(Table::Sums, &key)?.unwrap_or_else(|| vec![0u8; 4096]);
        let idx = index_at(time, level) * 16;
        let s = u64::from_le_bytes(buf[idx..idx + 8].try_into().unwrap());
        let z = u64::from_le_bytes(buf[idx + 8..idx + 16].try_into().unwrap());
        let (s, z) = if remove { (s.wrapping_sub(sum), z.wrapping_sub(size)) } else { (s.wrapping_add(sum), z.wrapping_add(size)) };
        buf[idx..idx + 8].copy_from_slice(&s.to_le_bytes());
        buf[idx + 8..idx + 16].copy_from_slice(&z.to_le_bytes());
        ov.put(Table::Sums, key, buf)?;
    }
    Ok(())
}

/// xdagj `loadSum`: 16 (sum, size) pairs covering `[start, end)`, which must
/// span a power of two. Returns `None` for an invalid range.
pub fn load(ov: &Overlay, start: u64, end: u64) -> Result<Option<[u8; 256]>> {
    let mut dt = end.wrapping_sub(start);
    if dt == 0 || dt & (dt - 1) != 0 {
        return Ok(None);
    }
    let mut level: i32 = -6;
    while dt != 0 {
        level += 1;
        dt >>= 4;
    }
    let base = start & 0xffff_ff00_0000;
    let file_level = if level < 2 {
        3
    } else if level < 4 {
        2
    } else if level < 6 {
        1
    } else {
        0
    };
    let mut out = [0u8; 256];
    let Some(buf) = ov.get(Table::Sums, &level_key(base, file_level))? else {
        return Ok(Some(out));
    };
    if level & 1 != 0 {
        for group in 0..16 {
            let mut s = 0u64;
            let mut z = 0u64;
            for j in 0..16 {
                let o = (group * 16 + j) * 16;
                s = s.wrapping_add(u64::from_le_bytes(buf[o..o + 8].try_into().unwrap()));
                z = z.wrapping_add(u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap()));
            }
            out[group * 16..group * 16 + 8].copy_from_slice(&s.to_le_bytes());
            out[group * 16 + 8..group * 16 + 16].copy_from_slice(&z.to_le_bytes());
        }
    } else {
        let index = ((start >> ((level + 4) * 4)) & 0xf0) as usize;
        out.copy_from_slice(&buf[index * 16..index * 16 + 256]);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdag_storage::Db;

    #[test]
    fn sums_aggregate_consistently() {
        let (db, _g) = Db::open_temporary().unwrap();
        let mut ov = Overlay::new(db);
        let t1 = 0x0169_4012_3456u64;
        let t2 = 0x0169_4013_0001u64;
        add_block(&mut ov, t1, 10).unwrap();
        add_block(&mut ov, t2, 20).unwrap();
        // whole 2^48 range → one group holds both
        let s = load(&ov, 0, 1 << 48).unwrap().unwrap();
        let total: u64 = (0..16).map(|g| u64::from_le_bytes(s[g * 16..g * 16 + 8].try_into().unwrap())).sum();
        assert_eq!(total, 30);
        // 2^20 range around t1's epoch group
        let start = t1 & !0xfffff;
        let s = load(&ov, start, start + (1 << 20)).unwrap().unwrap();
        let sizes: u64 = (0..16).map(|g| u64::from_le_bytes(s[g * 16 + 8..g * 16 + 16].try_into().unwrap())).sum();
        assert_eq!(sizes, 1024); // both blocks fall inside this 2^20 window
        assert!(load(&ov, 0, 3).unwrap().is_none());
    }
}
