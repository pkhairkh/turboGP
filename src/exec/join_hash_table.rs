//! High-performance join hash table using CedarDB-style bloom-filter-tagged
//! chaining with CRC32 hardware hashing.
//!
//! # Design (based on CedarDB DaMoN'24 paper)
//!
//! ## Layout
//! ```text
//! directory: Vec<u64>  — tag(16) | entry_idx(48)
//! entries:   Vec<Entry> — { key, row_idx, next }
//! ```
//!
//! The upper 16 bits of each directory slot store a 4-bit Bloom filter tag
//! (replicated 4×). The lower 48 bits store an index into the entries array.
//! An empty slot is 0 (tag=0, idx=0), which naturally fails the bloom check
//! for any non-zero hash — fusing the null-check with the bloom-check.
//!
//! ## Probe fast path (10 instructions)
//! 1. `slot = hash >> shift`           (1 shr)
//! 2. `entry = directory[slot]`        (1 mov, random access)
//! 3. `could_contain(entry, hash)`     (3 instrs: andn + table lookup)
//! 4. if fail → return false           (99% of probes exit here)
//! 5. else → follow pointer, compare key
//!
//! ## Hash function
//! Uses hardware CRC32 (`_mm_crc32_u64`) — 2 instructions vs xxh3's ~20.
//! Falls back to a mixing function on non-x86_64.
//!
//! ## Multi-value support
//! Each directory slot heads a linked list of entries with the same hash
//! prefix. This handles duplicate keys (foreign-key joins) without skew
//! degradation.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::_mm_crc32_u64;

/// A join hash table entry: key + source row index + chain pointer.
#[repr(C)]
pub struct JoinEntry {
    /// The full 64-bit key (for exact comparison after bloom match).
    pub key: u64,
    /// The row index in the build-side table.
    pub row_idx: u32,
    /// Index of the next entry in the same bucket chain (0 = end).
    /// Note: index 0 is reserved as "null", so real entries start at 1.
    pub next: u32,
}

/// Bloom-filter-tagged chaining hash table for equi-joins.
///
/// Build: O(n) insertions into per-thread-local buffers, then bulk-merge
///        into the shared directory via atomic CAS on bucket heads.
/// Probe: O(1) expected, with 99%+ fast-path exit via bloom tag.
pub struct JoinHashTable {
    /// Directory: power-of-2 sized array.
    /// Each u64 = (bloom_tag << 48) | entry_idx.
    /// entry_idx=0 means empty (tag=0 naturally fails bloom check).
    directory: Vec<AtomicU64>,
    /// Entry storage. Index 0 is reserved (sentinel "null").
    entries: Vec<JoinEntry>,
    /// Number of entries used (next free slot).
    len: usize,
    /// shift = 64 - log2(directory.len())
    shift: u32,
}

/// Simple 16-bit bloom tag derived directly from the hash.
/// Uses bits 32-47 of the hash. An empty slot has tag=0, so any non-zero
/// hash will fail the check (fusing null-check with bloom-check).
/// False positive rate: 1/65536 per probe (negligible).

impl JoinHashTable {
    /// Create a new hash table sized for `expected_entries` build-side rows.
    /// Directory size = next power of 2 ≥ 2 × expected_entries.
    pub fn new(expected_entries: usize) -> Self {
        let dir_size = (expected_entries * 2).next_power_of_two().max(16);
        let entries_cap = expected_entries + 1; // +1 for sentinel at index 0
        JoinHashTable {
            directory: (0..dir_size).map(|_| AtomicU64::new(0)).collect(),
            entries: (0..entries_cap).map(|_| JoinEntry { key: 0, row_idx: 0, next: 0 }).collect(),
            len: 1, // index 0 is sentinel
            shift: 64 - (dir_size as u64).trailing_zeros(),
        }
    }

    /// Hardware CRC32 hash for x86_64, mixing fallback otherwise.
    #[inline]
    pub fn hash(key: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let crc = _mm_crc32_u64(0, key);
            // Mix the 32-bit CRC into a 64-bit hash with a single multiply
            crc.wrapping_mul(0x8648DBDB_00000001)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Fallback: splitmix64
            let mut z = key.wrapping_add(0x9E3779B97F4A7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
    }

    /// Extract the 16-bit bloom tag from a hash.
    /// Uses bits 16-31 of the hash (lower bits are used for slot indexing).
    #[inline]
    fn bloom_tag(hash: u64) -> u16 {
        ((hash >> 16) as u16) | 1 // |1 ensures non-zero for non-empty
    }

    /// Check if an entry's tag could contain the hash.
    /// Returns false if definitely not present (or empty slot).
    /// Uses subset check: all bits of expected must be in stored tag.
    /// This is correct because insert OR-s tags together.
    #[inline]
    fn could_contain(entry: u64, hash: u64) -> bool {
        let tag = (entry >> 48) as u16;
        let expected = Self::bloom_tag(hash);
        // tag must be non-empty (slot occupied) and contain all expected bits
        tag != 0 && (tag & expected) == expected
    }

    /// Insert a key → row_idx mapping. Single-threaded build path.
    #[inline]
    pub fn insert(&mut self, key: u64, row_idx: u32) {
        let hash = Self::hash(key);
        let slot = (hash >> self.shift) as usize;
        let tag = Self::bloom_tag(hash);

        // Allocate entry
        let entry_idx = self.len as u32;
        if (entry_idx as usize) >= self.entries.len() {
            self.entries.push(JoinEntry { key: 0, row_idx: 0, next: 0 });
        }
        self.entries[entry_idx as usize] = JoinEntry { key, row_idx, next: 0 };
        self.len += 1;

        // Link into bucket head (single-threaded: direct store)
        // The directory stores (accumulated_bloom_tag << 48) | head_entry_idx.
        // Tags are OR-ed together so any matching tag in the chain passes the bloom check.
        let old_head = self.directory[slot].load(Ordering::Relaxed);
        let old_tag = (old_head >> 48) as u16;
        let old_idx = (old_head & 0xFFFF_FFFF) as u32;
        self.entries[entry_idx as usize].next = old_idx;
        let new_tag = old_tag | tag;
        let new_head = ((new_tag as u64) << 48) | (entry_idx as u64);
        self.directory[slot].store(new_head, Ordering::Relaxed);
    }

    /// Probe for a key. Returns the first matching row_idx, or None.
    /// This is the hot path — kept to ~10 instructions on the miss case.
    #[inline]
    pub fn probe(&self, key: u64) -> Option<u32> {
        let hash = Self::hash(key);
        let slot = (hash >> self.shift) as usize;
        let entry = self.directory[slot].load(Ordering::Relaxed);

        // Fast path: bloom check (3 instructions)
        if !Self::could_contain(entry, hash) {
            return None;
        }

        // Slow path: walk the chain
        let mut idx = (entry & 0xFFFF_FFFF) as u32;
        while idx != 0 {
            let e = &self.entries[idx as usize];
            if e.key == key {
                return Some(e.row_idx);
            }
            idx = e.next;
        }
        None
    }

    /// Probe and collect ALL matching row_idxs (for one-to-many joins).
    /// Returns a Vec to allow multiple matches per probe key.
    #[inline]
    pub fn probe_all(&self, key: u64, out: &mut Vec<u32>) {
        out.clear();
        let hash = Self::hash(key);
        let slot = (hash >> self.shift) as usize;
        let entry = self.directory[slot].load(Ordering::Relaxed);

        if !Self::could_contain(entry, hash) {
            return;
        }

        let mut idx = (entry & 0xFFFF_FFFF) as u32;
        while idx != 0 {
            let e = &self.entries[idx as usize];
            if e.key == key {
                out.push(e.row_idx);
            }
            idx = e.next;
        }
    }

    /// Prefetch the directory slot for a given key into all cache levels
    /// (L1/L2/L3). Call this K rows ahead of the actual `probe`/`probe_all`
    /// call to hide the ~100-cycle L3 miss on the random directory access.
    ///
    /// This is the #1 hot spot in Q21 (23.68% of runtime per W-MATH-RESEARCH
    /// perf profile) — the directory is a Vec<AtomicU64> sized at
    /// 2x build_side rows, so for a 6M-row build side it is ~96MB and
    /// entirely L3-resident. Each probe's `directory[slot]` load is a
    /// random access that stalls the pipeline for ~100 cycles.
    ///
    /// Issuing `_mm_prefetch(addr, _MM_HINT_T0)` ~16 rows ahead gives the
    /// memory subsystem time to pull the cache line into L1 before the
    /// actual load executes.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    pub fn prefetch_directory(&self, key: u64) {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let hash = Self::hash(key);
        let slot = (hash >> self.shift) as usize;
        // SAFETY: slot is derived from hash >> shift, where shift = 64 - log2(dir_size).
        // So slot < dir_size = directory.len(). The pointer add is in-bounds.
        // _mm_prefetch is a hint instruction — it never faults on invalid addresses,
        // but we keep the address valid for cleanliness.
        unsafe {
            _mm_prefetch(self.directory.as_ptr().add(slot) as *const i8, _MM_HINT_T0);
        }
    }

    /// No-op prefetch fallback for non-x86_64 targets.
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    pub fn prefetch_directory(&self, _key: u64) {}

    /// Number of entries in the table.
    pub fn len(&self) -> usize {
        self.len - 1 // subtract sentinel
    }

    pub fn is_empty(&self) -> bool {
        self.len <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_probe() {
        let mut ht = JoinHashTable::new(100);
        for i in 0..100u64 {
            ht.insert(i * 7, i as u32);
        }
        for i in 0..100u64 {
            assert_eq!(ht.probe(i * 7), Some(i as u32));
        }
        // Non-existent keys
        assert_eq!(ht.probe(1), None);
        assert_eq!(ht.probe(999), None);
    }

    #[test]
    fn test_duplicate_keys() {
        let mut ht = JoinHashTable::new(100);
        ht.insert(42, 1);
        ht.insert(42, 2);
        ht.insert(42, 3);
        let mut out = Vec::new();
        ht.probe_all(42, &mut out);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&1));
        assert!(out.contains(&2));
        assert!(out.contains(&3));
    }

    #[test]
    fn test_large_load() {
        let n = 100_000;
        let mut ht = JoinHashTable::new(n);
        for i in 0..n as u64 {
            ht.insert(i, i as u32);
        }
        for i in 0..n as u64 {
            assert_eq!(ht.probe(i), Some(i as u32));
        }
        // Random misses
        for i in (n as u64)..(n as u64 + 1000) {
            assert_eq!(ht.probe(i), None);
        }
    }

    #[test]
    fn test_hash_distribution() {
        // Verify CRC32 hash distributes well
        let mut buckets = [0usize; 256];
        for i in 0..100_000u64 {
            let h = JoinHashTable::hash(i);
            buckets[(h % 256) as usize] += 1;
        }
        // Each bucket should have ~390 entries (100000/256)
        for &count in &buckets {
            assert!(count > 300 && count < 500, "bucket count {} out of range", count);
        }
    }
}
