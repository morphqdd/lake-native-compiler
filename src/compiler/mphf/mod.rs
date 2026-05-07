//! Minimal Perfect Hash Function (MPHF) for guard dispatch.
//!
//! MPHF maps N keys to exactly 0..N-1 without collisions.
//! Used to convert hash(guard_values) to sequential indices
//! for Cranelift Switch (jump table).
//!
//! Algorithm: Hash + Bucket + Displacement
//! - Construction: O(N)
//! - Lookup: O(1) - 2 memory loads + arithmetic
//! - Space: ~4 bytes per key

mod builder;
mod codegen;

pub use builder::MphfBuilder;
pub use codegen::{emit_fxhash, emit_hash_function, emit_mphf_lookup};

use std::collections::HashMap;

/// MPHF data structure.
///
/// Lookup algorithm:
/// ```ignored
/// bucket_id = hash(key) % num_buckets
/// displacement = displacements[bucket_id]
/// index = (hash(key) + displacement) % total_keys
/// ```
#[derive(Debug, Clone)]
pub struct Mphf {
    /// Seed for hash function
    pub seed: u64,
    /// Displacement table: bucket_id → displacement value
    pub displacements: Vec<u32>,
    /// Total number of keys (N)
    pub total_keys: usize,
    /// Number of buckets (typically N / 4)
    pub num_buckets: usize,
}

impl Mphf {
    /// Creates a new MPHF for the given keys.
    pub fn new(keys: &[u64]) -> Self {
        MphfBuilder::build(keys)
    }

    /// Lookup: returns index 0..N-1 for the key.
    ///
    /// WARNING: returns correct result only for keys that were used during construction!
    pub fn lookup(&self, key: u64) -> u32 {
        let hash = self.hash(key);
        let bucket_id = (hash % self.num_buckets as u64) as usize;
        let displacement = self.displacements[bucket_id];
        ((hash + displacement as u64) % self.total_keys as u64) as u32
    }

    /// FxHash-like hash function (fast).
    #[inline]
    fn hash(&self, key: u64) -> u64 {
        let mut hash = key.wrapping_add(self.seed);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xff51afd7ed558ccd);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xc4ceb9fe1a85ec53);
        hash ^= hash >> 33;
        hash
    }
}
pub fn fxhash(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mphf_basic() {
        let keys = vec![100, 200, 300, 400, 500];
        let mphf = Mphf::new(&keys);

        // Check that all keys map to 0..4
        let mut indices = vec![];
        for &key in &keys {
            let idx = mphf.lookup(key);
            assert!(idx < 5, "Index {} out of range", idx);
            indices.push(idx);
        }

        // Check that all indices are unique
        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 5, "Indices not unique: {:?}", indices);
    }

    #[test]
    fn test_mphf_strings() {
        // For strings we use pre-hashed values
        let keys = vec![
            fxhash("hello".as_bytes()),
            fxhash("world".as_bytes()),
            fxhash("lake".as_bytes()),
            fxhash("compiler".as_bytes()),
        ];

        let mphf = Mphf::new(&keys);

        let mut indices = vec![];
        for &key in &keys {
            let idx = mphf.lookup(key);
            assert!(idx < 4);
            indices.push(idx);
        }

        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 4);
    }
}
