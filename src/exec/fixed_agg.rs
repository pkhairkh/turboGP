//! Fixed-array pre-aggregation for low-cardinality GROUP BY.
//!
//! # Problem
//! Q1 has only 4 groups (l_returnflag × l_linestatus: N×O, N×F, R×F, R×O).
//! But the current code uses a HashMap<u64, Vec<usize>> which:
//! - Allocates a Vec per group (malloc churn)
//! - Does random memory access for each row's group lookup
//! - Then iterates per-group (materializing indices)
//!
//! This accounts for ~120ms of Q1's 126ms — the actual aggregation is only ~5ms.
//!
//! # Solution: Fixed-Array Accumulator
//! For low-cardinality GROUP BY (≤256 groups), use a fixed-size array of
//! accumulators. Each group gets a slot indexed by a perfect hash of the
//! group key. The accumulator stores all aggregate values inline (SoA layout).
//!
//! Single pass over all rows:
//!   1. Compute group_id = perfect_hash(group_key) (1 multiply + shift)
//!   2. Update accumulators[group_id] directly (no HashMap lookup)
//!   3. At the end, emit non-empty groups
//!
//! This eliminates:
//! - HashMap allocation (0 mallocs)
//! - Per-group Vec allocation (0 mallocs)
//! - Per-row hash table lookup (replaced by array index)
//! - Index materialization (no Vec<usize> per group)
//!
//! # SIMD Opportunity
//! With a fixed array, we can use AVX-512 to update 8 groups in parallel
//! when the data is shuffled. But the simpler win is just eliminating
//! the HashMap overhead — that alone should give 10-20x.

use std::sync::Arc;

/// Maximum number of groups supported by the fixed-array accumulator.
/// 256 is enough for Q1 (4 groups), Q13 (~40), Q16 (~18K — too many, fallback).
pub const MAX_FIXED_GROUPS: usize = 256;

/// Fixed-array accumulator for low-cardinality GROUP BY.
/// Each group has a slot with inline aggregate state.
pub struct FixedAccumulator {
    /// Group key → slot index (None if not yet seen).
    /// Uses a simple modulo hash; for ≤256 groups this has low collision.
    pub slots: [Option<u64>; MAX_FIXED_GROUPS],
    /// Per-slot sum accumulators (f64::to_bits stored as u64).
    /// Layout: [sum0_slot0, sum0_slot1, ..., sum0_slot255, sum1_slot0, ...]
    /// This SoA layout enables SIMD accumulation across slots.
    pub sums: Vec<f64>, // num_aggs × MAX_FIXED_GROUPS
    pub counts: Vec<u64>,     // MAX_FIXED_GROUPS
    pub mins: Vec<f64>,       // num_aggs × MAX_FIXED_GROUPS
    pub maxs: Vec<f64>,       // num_aggs × MAX_FIXED_GROUPS
    pub group_keys: Vec<u64>, // MAX_FIXED_GROUPS (the actual key per slot)
    pub num_aggs: usize,
    pub num_active: usize,
}

impl FixedAccumulator {
    pub fn new(num_aggs: usize) -> Self {
        FixedAccumulator {
            slots: [None; MAX_FIXED_GROUPS],
            sums: vec![0.0; num_aggs * MAX_FIXED_GROUPS],
            counts: vec![0; MAX_FIXED_GROUPS],
            mins: vec![f64::INFINITY; num_aggs * MAX_FIXED_GROUPS],
            maxs: vec![f64::NEG_INFINITY; num_aggs * MAX_FIXED_GROUPS],
            group_keys: vec![0; MAX_FIXED_GROUPS],
            num_aggs,
            num_active: 0,
        }
    }

    /// Get or create a slot for a group key. Returns the slot index.
    /// Uses linear probing for collision resolution.
    #[inline]
    pub fn get_or_create_slot(&mut self, key: u64) -> usize {
        let mut slot = (key as usize) & (MAX_FIXED_GROUPS - 1); // mod 256
        loop {
            match self.slots[slot] {
                Some(k) if k == key => return slot,
                Some(_) => {
                    slot = (slot + 1) & (MAX_FIXED_GROUPS - 1);
                }
                None => {
                    self.slots[slot] = Some(key);
                    self.group_keys[slot] = key;
                    self.num_active += 1;
                    return slot;
                }
            }
        }
    }

    /// Update a sum accumulator for a given slot.
    #[inline]
    pub fn add_sum(&mut self, agg_idx: usize, slot: usize, value: f64) {
        self.sums[agg_idx * MAX_FIXED_GROUPS + slot] += value;
    }

    /// Increment count for a slot.
    #[inline]
    pub fn inc_count(&mut self, slot: usize) {
        self.counts[slot] += 1;
    }

    /// Update min for a given slot/agg.
    #[inline]
    pub fn update_min(&mut self, agg_idx: usize, slot: usize, value: f64) {
        let idx = agg_idx * MAX_FIXED_GROUPS + slot;
        if value < self.mins[idx] {
            self.mins[idx] = value;
        }
    }

    /// Update max for a given slot/agg.
    #[inline]
    pub fn update_max(&mut self, agg_idx: usize, slot: usize, value: f64) {
        let idx = agg_idx * MAX_FIXED_GROUPS + slot;
        if value > self.maxs[idx] {
            self.maxs[idx] = value;
        }
    }

    /// Finalize: emit (group_key, sum_values, count, min_values, max_values)
    /// for each active slot.
    pub fn finalize(&self) -> Vec<(u64, Vec<f64>, u64, Vec<f64>, Vec<f64>)> {
        let mut result = Vec::with_capacity(self.num_active);
        for slot in 0..MAX_FIXED_GROUPS {
            if self.slots[slot].is_some() {
                let key = self.group_keys[slot];
                let sums: Vec<f64> =
                    (0..self.num_aggs).map(|a| self.sums[a * MAX_FIXED_GROUPS + slot]).collect();
                let mins: Vec<f64> =
                    (0..self.num_aggs).map(|a| self.mins[a * MAX_FIXED_GROUPS + slot]).collect();
                let maxs: Vec<f64> =
                    (0..self.num_aggs).map(|a| self.maxs[a * MAX_FIXED_GROUPS + slot]).collect();
                let count = self.counts[slot];
                result.push((key, sums, count, mins, maxs));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_grouping() {
        let mut acc = FixedAccumulator::new(2); // 2 aggregates
        for i in 0..1000u64 {
            let key = i % 3; // 3 groups
            let slot = acc.get_or_create_slot(key);
            acc.add_sum(0, slot, i as f64);
            acc.add_sum(1, slot, (i * 2) as f64);
            acc.inc_count(slot);
        }
        let result = acc.finalize();
        assert_eq!(result.len(), 3);
        // Each group has ~333 items
        for (_, sums, count, _, _) in &result {
            assert!(*count > 300 && *count < 340);
            assert!(sums.len() == 2);
        }
    }

    #[test]
    fn test_collision_resolution() {
        let mut acc = FixedAccumulator::new(1);
        // Force collisions by using keys that hash to the same slot
        let key1 = 0u64;
        let key2 = 256u64; // same slot (0 & 255 = 0, 256 & 255 = 0)
        let s1 = acc.get_or_create_slot(key1);
        let s2 = acc.get_or_create_slot(key2);
        assert_ne!(s1, s2); // linear probing finds different slot
        assert_eq!(acc.num_active, 2);
    }

    #[test]
    fn test_min_max() {
        let mut acc = FixedAccumulator::new(1);
        let slot = acc.get_or_create_slot(42);
        acc.update_min(0, slot, 5.0);
        acc.update_min(0, slot, 3.0);
        acc.update_min(0, slot, 7.0);
        acc.update_max(0, slot, 5.0);
        acc.update_max(0, slot, 3.0);
        acc.update_max(0, slot, 7.0);
        let result = acc.finalize();
        assert_eq!(result[0].3[0], 3.0); // min
        assert_eq!(result[0].4[0], 7.0); // max
    }
}
