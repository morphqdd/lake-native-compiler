//! MPHF Builder - constructs minimal perfect hash at compile time.

use super::Mphf;
use std::collections::{HashMap, HashSet};

pub struct MphfBuilder;

impl MphfBuilder {
    /// Builds MPHF for the given keys.
    ///
    /// Algorithm:
    /// 1. Choose seed and num_buckets
    /// 2. Distribute keys into buckets: bucket_id = hash(key) % num_buckets
    /// 3. For each bucket, find displacement such that all keys
    ///    map to unique indices
    pub fn build(keys: &[u64]) -> Mphf {
        if keys.is_empty() {
            return Mphf {
                seed: 0,
                displacements: vec![],
                total_keys: 0,
                num_buckets: 0,
            };
        }

        let n = keys.len();

        // Compile-time construction can be slow - runtime lookup is what matters!

        // Strategy 1: Use N/4 buckets (good space/time tradeoff)
        let num_buckets = (n / 4).max(1);
        for seed in 0..3000 {
            if let Some(mphf) = Self::try_build(keys, seed, num_buckets) {
                return mphf;
            }
        }

        // Strategy 2: Use N/3 buckets
        let num_buckets = (n / 3).max(1);
        for seed in 0..3000 {
            if let Some(mphf) = Self::try_build(keys, seed, num_buckets) {
                return mphf;
            }
        }

        // Strategy 3: Use N/2 buckets (more buckets = easier to find solution)
        let num_buckets = (n / 2).max(1);
        for seed in 0..5000 {
            if let Some(mphf) = Self::try_build(keys, seed, num_buckets) {
                return mphf;
            }
        }

        // Strategy 4: Use N buckets (one bucket per key)
        let num_buckets = n;
        for seed in 0..10000 {
            if let Some(mphf) = Self::try_build(keys, seed, num_buckets) {
                return mphf;
            }
        }

        panic!(
            "Failed to build MPHF for {} keys after trying 21000 seeds",
            n
        );
    }

    fn try_build(keys: &[u64], seed: u64, num_buckets: usize) -> Option<Mphf> {
        let n = keys.len();

        // Group keys into buckets
        let mut buckets: Vec<Vec<u64>> = vec![vec![]; num_buckets];
        for &key in keys {
            let hash = Self::hash(key, seed);
            let bucket_id = (hash % num_buckets as u64) as usize;
            buckets[bucket_id].push(key);
        }

        // Sort buckets by size (largest first)
        // This increases the chance of finding a solution
        let mut bucket_order: Vec<usize> = (0..num_buckets).collect();
        bucket_order.sort_by_key(|&i| std::cmp::Reverse(buckets[i].len()));

        // Track already used indices
        let mut used_indices = HashSet::new();
        let mut displacements = vec![0u32; num_buckets];

        // For each bucket, find displacement
        for &bucket_id in &bucket_order {
            let bucket = &buckets[bucket_id];
            if bucket.is_empty() {
                continue;
            }

            // Try displacement from 0 to N*500 (compile-time only, so can be large)
            let mut found = false;
            'displacement_loop: for displacement in 0..(n * 500) {
                // Collect all indices this displacement would produce
                let mut bucket_indices = Vec::new();

                for &key in bucket {
                    let hash = Self::hash(key, seed);
                    let index = ((hash.wrapping_add(displacement as u64)) % n as u64) as u32;

                    // Check if this index is already used by another bucket
                    if used_indices.contains(&index) {
                        continue 'displacement_loop;
                    }

                    // Check if this index conflicts within this bucket
                    if bucket_indices.contains(&index) {
                        continue 'displacement_loop;
                    }

                    bucket_indices.push(index);
                }

                // All indices are unique and not used - success!
                for index in bucket_indices {
                    used_indices.insert(index);
                }
                displacements[bucket_id] = displacement as u32;
                found = true;
                break;
            }

            if !found {
                return None; // Failed to find displacement for this bucket
            }
        }

        Some(Mphf {
            seed,
            displacements,
            total_keys: n,
            num_buckets,
        })
    }

    #[inline]
    fn hash(key: u64, seed: u64) -> u64 {
        let mut hash = key.wrapping_add(seed);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xff51afd7ed558ccd);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xc4ceb9fe1a85ec53);
        hash ^= hash >> 33;
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_small() {
        let keys = vec![1, 2, 3, 4, 5];
        let mphf = MphfBuilder::build(&keys);

        // Check that we built it
        assert_eq!(mphf.total_keys, 5);
        assert!(mphf.num_buckets > 0);
    }

    #[test]
    fn test_build_medium() {
        // Test with 10 keys (reasonable for most guard scenarios)
        let keys: Vec<u64> = (0..10).map(|i| i * 17 + 42).collect();
        let mphf = MphfBuilder::build(&keys);

        assert_eq!(mphf.total_keys, 10);

        // Verify all keys map to unique indices
        let mut indices: Vec<_> = keys.iter().map(|&k| mphf.lookup(k)).collect();
        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 10);
    }

    #[test]
    fn test_build_with_collisions() {
        // Keys that might cause collisions
        let keys = vec![
            0x1234567890abcdef,
            0xfedcba0987654321,
            0x1111111111111111,
            0xffffffffffffffff,
        ];

        let mphf = MphfBuilder::build(&keys);

        // All indices should be unique
        let mut indices: Vec<_> = keys.iter().map(|&k| mphf.lookup(k)).collect();
        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 4);
    }
}
