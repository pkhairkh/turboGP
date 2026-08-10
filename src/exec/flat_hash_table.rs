//! Flat hash table with Robin Hood linear probing + inline aggregation.
//!
//! Research: Robin Hood hashing (16% faster at 85% load factor).
//! xxh3 for hashing (30% fewer cycles than xxh64).
//! Perfect hashing via graph coloring (future: for static group keys).
//!
//! Zero allocations after setup. Cache-friendly contiguous storage.

use xxhash_rust::xxh3;

/// A flat, open-addressed hash table with Robin Hood linear probing.
/// Each slot stores a key + inline aggregation state (32 bytes).
pub struct FlatHashTable {
    keys: Vec<u64>,
    occupied: Vec<bool>,
    probe_dist: Vec<u8>,
    states: Vec<AggState>,
    capacity: usize,
    mask: usize,
    len: usize,
}

/// Inline aggregation state — 32 bytes, one cache line.
#[derive(Clone, Copy, Default)]
pub struct AggState {
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
}

impl AggState {
    pub fn new() -> Self {
        AggState { count: 0, sum: 0, min: u64::MAX, max: 0 }
    }

    pub fn update(&mut self, value: u64) {
        self.count += 1;
        self.sum = self.sum.wrapping_add(value);
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
    }

    pub fn finalize_count(&self) -> u64 {
        self.count
    }
    pub fn finalize_sum(&self) -> u64 {
        (self.sum as f64).to_bits()
    }
    pub fn finalize_min(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.min
        }
    }
    pub fn finalize_max(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.max
        }
    }
    pub fn finalize_avg(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            (self.sum as f64 / self.count as f64).to_bits()
        }
    }
}

impl FlatHashTable {
    /// Create a new table sized for `expected_groups` entries.
    pub fn new(expected_groups: usize) -> Self {
        let capacity = (expected_groups * 2).next_power_of_two().max(16);
        FlatHashTable {
            keys: vec![0; capacity],
            occupied: vec![false; capacity],
            probe_dist: vec![0; capacity],
            states: vec![AggState::new(); capacity],
            capacity,
            mask: capacity - 1,
            len: 0,
        }
    }

    /// Insert a key and return its slot index. Updates Robin Hood displacement.
    pub fn insert(&mut self, key: u64) -> usize {
        if (self.len * 4) > (self.capacity * 3) {
            self.resize();
        }
        let hash = xxh3::xxh3_64(&key.to_le_bytes());
        let mut idx = (hash as usize) & self.mask;
        let mut dist: u8 = 0;
        let mut current_key = key;

        loop {
            if !self.occupied[idx] {
                self.keys[idx] = current_key;
                self.occupied[idx] = true;
                self.probe_dist[idx] = dist;
                self.len += 1;
                return idx;
            }
            if self.keys[idx] == current_key {
                return idx;
            }
            if self.probe_dist[idx] < dist {
                std::mem::swap(&mut self.keys[idx], &mut current_key);
                std::mem::swap(&mut self.probe_dist[idx], &mut dist);
            }
            idx = (idx + 1) & self.mask;
            dist = dist.saturating_add(1);
        }
    }

    fn resize(&mut self) {
        let old_keys = self.keys.clone();
        let old_occupied = self.occupied.clone();
        let old_states = self.states.clone();

        self.capacity *= 2;
        self.mask = self.capacity - 1;
        self.keys = vec![0; self.capacity];
        self.occupied = vec![false; self.capacity];
        self.probe_dist = vec![0; self.capacity];
        self.states = vec![AggState::new(); self.capacity];
        self.len = 0;

        for i in 0..old_keys.len() {
            if old_occupied[i] {
                let new_slot = self.insert(old_keys[i]);
                self.states[new_slot] = old_states[i];
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &AggState)> {
        self.occupied.iter().enumerate().filter_map(move |(i, &occ)| {
            if occ {
                Some((self.keys[i], &self.states[i]))
            } else {
                None
            }
        })
    }
}

/// Aggregation function type.
#[derive(Debug, Clone, Copy)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    CountDistinct,
}

/// Vectorized hash aggregation: group keys, compute aggregate per group.
/// Returns (key, result) pairs.
pub fn hash_group_by_flat(
    keys: &[u64],
    values: Option<&[u64]>,
    agg_func: AggFunc,
) -> Vec<(u64, u64)> {
    // Estimate group count: sample-based (check first 1000 rows)
    let sample: std::collections::HashSet<u64> = keys.iter().take(1000).copied().collect();
    let estimated_groups = if keys.len() <= 1000 {
        sample.len().max(16)
    } else {
        (sample.len() as f64 * keys.len() as f64 / 1000.0) as usize + 16
    };

    let mut table = FlatHashTable::new(estimated_groups);

    match agg_func {
        AggFunc::Count => {
            for &key in keys {
                let slot = table.insert(key);
                table.states[slot].count += 1;
            }
        }
        AggFunc::Sum | AggFunc::Avg => {
            let vals = values.unwrap_or(keys);
            for i in 0..keys.len() {
                let slot = table.insert(keys[i]);
                table.states[slot].update(vals[i]);
            }
        }
        AggFunc::Min | AggFunc::Max => {
            let vals = values.unwrap_or(keys);
            for i in 0..keys.len() {
                let slot = table.insert(keys[i]);
                table.states[slot].update(vals[i]);
            }
        }
        AggFunc::CountDistinct => {
            // For count distinct, we need a set per group.
            // Use a HashMap<u64, HashSet<u64>> — slower but correct.
            let mut groups: std::collections::HashMap<u64, std::collections::HashSet<u64>> =
                std::collections::HashMap::new();
            let vals = values.unwrap_or(keys);
            for i in 0..keys.len() {
                groups.entry(keys[i]).or_default().insert(vals[i]);
            }
            return groups.into_iter().map(|(k, s)| (k, s.len() as u64)).collect();
        }
    }

    table
        .iter()
        .map(|(key, state)| {
            let result = match agg_func {
                AggFunc::Count => state.finalize_count(),
                AggFunc::Sum => state.finalize_sum(),
                AggFunc::Avg => state.finalize_avg(),
                AggFunc::Min => state.finalize_min(),
                AggFunc::Max => state.finalize_max(),
                AggFunc::CountDistinct => unreachable!(),
            };
            (key, result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_insert_lookup() {
        let mut table = FlatHashTable::new(100);
        let s1 = table.insert(42);
        let s2 = table.insert(99);
        let s3 = table.insert(42); // same key → same slot
        assert_eq!(s1, s3);
        assert_ne!(s1, s2);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_aggregate_count() {
        let keys = vec![1u64, 1, 1, 2, 2, 3, 1, 2, 3, 3, 3];
        let results = hash_group_by_flat(&keys, None, AggFunc::Count);
        let map: std::collections::HashMap<u64, u64> = results.into_iter().collect();
        assert_eq!(map[&1], 4);
        assert_eq!(map[&2], 3);
        assert_eq!(map[&3], 4);
    }

    #[test]
    fn test_aggregate_sum() {
        let keys = vec![1u64, 1, 2, 2];
        let vals = vec![10u64, 20, 30, 40];
        let results = hash_group_by_flat(&keys, Some(&vals), AggFunc::Sum);
        let map: std::collections::HashMap<u64, u64> = results.into_iter().collect();
        assert_eq!(f64::from_bits(map[&1]), 30.0);
        assert_eq!(f64::from_bits(map[&2]), 70.0);
    }

    #[test]
    fn test_aggregate_min_max() {
        let keys = vec![1u64, 1, 2];
        let vals = vec![10u64, 5, 20];
        let min_results = hash_group_by_flat(&keys, Some(&vals), AggFunc::Min);
        let max_results = hash_group_by_flat(&keys, Some(&vals), AggFunc::Max);
        let min_map: std::collections::HashMap<u64, u64> = min_results.into_iter().collect();
        let max_map: std::collections::HashMap<u64, u64> = max_results.into_iter().collect();
        assert_eq!(min_map[&1], 5);
        assert_eq!(max_map[&1], 10);
        assert_eq!(min_map[&2], 20);
        assert_eq!(max_map[&2], 20);
    }

    #[test]
    fn test_aggregate_avg() {
        let keys = vec![1u64, 1, 1, 2, 2];
        let vals = vec![10u64, 20, 30, 40, 60];
        let results = hash_group_by_flat(&keys, Some(&vals), AggFunc::Avg);
        let map: std::collections::HashMap<u64, u64> = results.into_iter().collect();
        assert_eq!(f64::from_bits(map[&1]), 20.0);
        assert_eq!(f64::from_bits(map[&2]), 50.0);
    }

    #[test]
    fn test_count_distinct() {
        let keys = vec![1u64, 1, 2];
        let vals = vec![10u64, 10, 20];
        let results = hash_group_by_flat(&keys, Some(&vals), AggFunc::CountDistinct);
        let map: std::collections::HashMap<u64, u64> = results.into_iter().collect();
        assert_eq!(map[&1], 1);
        assert_eq!(map[&2], 1);
    }

    #[test]
    fn test_large_groupby() {
        let n = 1_000_000;
        let keys: Vec<u64> = (0..n).map(|i| i % 200).collect();
        let start = Instant::now();
        let results = hash_group_by_flat(&keys, None, AggFunc::Count);
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 200);
        for (_, count) in &results {
            assert_eq!(*count, 5000);
        }
        assert!(elapsed.as_millis() < 200, "GROUP BY took {}ms", elapsed.as_millis());
    }

    #[test]
    fn test_resize() {
        let mut table = FlatHashTable::new(4);
        for i in 0..100 {
            table.insert(i);
        }
        assert_eq!(table.len(), 100);
    }

    #[test]
    fn test_iter() {
        let mut table = FlatHashTable::new(10);
        table.insert(1);
        table.insert(2);
        table.insert(3);
        let keys: Vec<u64> = table.iter().map(|(k, _)| k).collect();
        assert!(keys.contains(&1));
        assert!(keys.contains(&2));
        assert!(keys.contains(&3));
    }
}
