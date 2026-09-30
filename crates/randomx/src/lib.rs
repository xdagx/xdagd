//! RandomX proof of work (FFI to the vendored reference implementation).
//!
//! XDAG uses RandomX in *light* mode for verification (256 MiB cache per key)
//! and optionally *fast* mode (2 GiB dataset) for pools that hash many shares.
//! A key is the 32-byte seed derived from the main chain every 4096 blocks;
//! the node keeps VMs for the two most recent keys.

use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::sync::Arc;

use parking_lot::Mutex;
use xdag_chain::pow::PowEngine;

#[allow(non_camel_case_types)]
type randomx_flags = c_int;

pub const FLAG_DEFAULT: randomx_flags = 0;
pub const FLAG_LARGE_PAGES: randomx_flags = 1;
pub const FLAG_HARD_AES: randomx_flags = 2;
pub const FLAG_FULL_MEM: randomx_flags = 4;
pub const FLAG_JIT: randomx_flags = 8;
pub const FLAG_SECURE: randomx_flags = 16;

#[repr(C)]
struct RxCache {
    _p: [u8; 0],
}
#[repr(C)]
struct RxDataset {
    _p: [u8; 0],
}
#[repr(C)]
struct RxVm {
    _p: [u8; 0],
}

extern "C" {
    fn randomx_get_flags() -> randomx_flags;
    fn randomx_alloc_cache(flags: randomx_flags) -> *mut RxCache;
    fn randomx_init_cache(cache: *mut RxCache, key: *const c_void, key_size: usize);
    fn randomx_release_cache(cache: *mut RxCache);
    fn randomx_alloc_dataset(flags: randomx_flags) -> *mut RxDataset;
    fn randomx_dataset_item_count() -> std::os::raw::c_ulong;
    fn randomx_init_dataset(dataset: *mut RxDataset, cache: *mut RxCache, start_item: std::os::raw::c_ulong, item_count: std::os::raw::c_ulong);
    fn randomx_release_dataset(dataset: *mut RxDataset);
    fn randomx_create_vm(flags: randomx_flags, cache: *mut RxCache, dataset: *mut RxDataset) -> *mut RxVm;
    fn randomx_destroy_vm(vm: *mut RxVm);
    fn randomx_calculate_hash(vm: *mut RxVm, input: *const c_void, input_size: usize, output: *mut c_void);
}

/// Recommended flags for this CPU (JIT, hardware AES when available).
pub fn recommended_flags() -> i32 {
    unsafe { randomx_get_flags() }
}

/// A RandomX VM bound to one key. Not thread-safe; wrap in a Mutex.
pub struct Hasher {
    cache: *mut RxCache,
    dataset: *mut RxDataset,
    vm: *mut RxVm,
    key: Vec<u8>,
}

unsafe impl Send for Hasher {}

impl Hasher {
    /// Light-mode hasher (256 MiB).
    pub fn light(key: &[u8]) -> Option<Hasher> {
        Self::new(key, false)
    }

    /// Fast mode additionally builds the 2 GiB dataset (single-threaded here).
    pub fn new(key: &[u8], full_mem: bool) -> Option<Hasher> {
        let mut flags = recommended_flags();
        unsafe {
            let cache = randomx_alloc_cache(flags);
            if cache.is_null() {
                return None;
            }
            randomx_init_cache(cache, key.as_ptr() as *const c_void, key.len());
            let mut dataset = std::ptr::null_mut();
            if full_mem {
                dataset = randomx_alloc_dataset(flags);
                if dataset.is_null() {
                    randomx_release_cache(cache);
                    return None;
                }
                randomx_init_dataset(dataset, cache, 0, randomx_dataset_item_count());
                flags |= FLAG_FULL_MEM;
            }
            let vm = randomx_create_vm(flags, cache, dataset);
            if vm.is_null() {
                if !dataset.is_null() {
                    randomx_release_dataset(dataset);
                }
                randomx_release_cache(cache);
                return None;
            }
            Some(Hasher { cache, dataset, vm, key: key.to_vec() })
        }
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    pub fn hash(&mut self, input: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        unsafe {
            randomx_calculate_hash(self.vm, input.as_ptr() as *const c_void, input.len(), out.as_mut_ptr() as *mut c_void);
        }
        out
    }
}

impl Drop for Hasher {
    fn drop(&mut self) {
        unsafe {
            randomx_destroy_vm(self.vm);
            if !self.dataset.is_null() {
                randomx_release_dataset(self.dataset);
            }
            randomx_release_cache(self.cache);
        }
    }
}

/// [`PowEngine`] backed by RandomX, keeping hashers for recent keys.
pub struct RandomXEngine {
    hashers: Mutex<VecDeque<([u8; 32], Arc<Mutex<Hasher>>)>>,
    capacity: usize,
    full_mem: bool,
}

impl RandomXEngine {
    /// Keeps the hashers of two seeds: around a seed change blocks of the old
    /// and of the new seed arrive side by side.
    pub fn new(full_mem: bool) -> Self {
        Self::with_capacity(full_mem, 2)
    }

    /// Each cached seed costs 256 MiB (2 GiB in fast mode). With one seed a
    /// node still works, but re-initialises whenever the seed in use changes.
    pub fn with_capacity(full_mem: bool, seeds: usize) -> Self {
        RandomXEngine { hashers: Mutex::new(VecDeque::new()), capacity: seeds.max(1), full_mem }
    }

    fn hasher(&self, key: &[u8; 32]) -> Option<Arc<Mutex<Hasher>>> {
        let mut hs = self.hashers.lock();
        if let Some((_, h)) = hs.iter().find(|(k, _)| k == key) {
            return Some(h.clone());
        }
        // make room before allocating the next cache
        while hs.len() >= self.capacity {
            hs.pop_front();
        }
        tracing::info!(key = %hex_key(key), full_mem = self.full_mem, "initialising RandomX for new seed");
        let h = Arc::new(Mutex::new(Hasher::new(key, self.full_mem)?));
        hs.push_back((*key, h.clone()));
        Some(h)
    }

    /// Hash arbitrary input under `key` (pool share verification).
    pub fn hash(&self, key: &[u8; 32], input: &[u8]) -> Option<[u8; 32]> {
        let h = self.hasher(key)?;
        let mut g = h.lock();
        Some(g.hash(input))
    }
}

fn hex_key(k: &[u8]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

impl PowEngine for RandomXEngine {
    fn randomx(&self, key: &[u8; 32], input: &[u8; 64]) -> Option<[u8; 32]> {
        self.hash(key, input)
    }

    fn prepare(&self, key: &[u8; 32]) {
        let _ = self.hasher(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vectors() {
        // tevador/RandomX src/tests/tests.cpp, "Hash test 1a"/"1b"
        let mut h = Hasher::light(b"test key 000").expect("randomx init");
        assert_eq!(hex::encode(h.hash(b"This is a test")), "639183aae1bf4c9a35884cb46b09cad9175f04efd7684e7262a0ac1c2f0b4e3f");
        assert_eq!(hex::encode(h.hash(b"Lorem ipsum dolor sit amet")), "300a0adb47603dedb42228ccb2b211104f4da45af709cd7547cd049e9489c969");
    }
}
