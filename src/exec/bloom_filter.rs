//! AVX-512 bloom filter for join semi-join pre-filtering.
//!
//! # TQFT Motivation — Wilson Loop / Frobenius μ
//!
//! In topological quantum field theory, the **Wilson loop** is a non-local
//! observable that tests whether a path is contractible (holonomy = identity)
//! *without* computing the full holonomy. The Wilson loop is a "cheap test
//! for an expensive property".
//!
//! In database terms: a **bloom filter** tests whether a join key is
//! *definitely absent* from the build side *without* probing the hash table.
//! This is the Wilson loop analogue — a cheap pre-filter that lets us skip
//! the expensive hash-table probe for keys that cannot match.
//!
//! The Frobenius μ invariant corresponds to the false-positive rate: a
//! non-zero μ (false positive) lets a few spurious keys through to the
//! hash-table probe, but never causes incorrect query results (the hash
//! table is the source of truth). A correctly-tuned bloom filter has μ
//! small (~1%) so the savings dominate.
//!
//! # Design
//!
//! - **Bit array** stored as `Vec<u64>` (64-bit word lanes).
//! - **Power-of-2 size** so modulo is a bitmask (`& mask`).
//! - **Double hashing**: `h1 = crc32(key) * K1`, `h2 = (crc32(key) >> 32) | 1`.
//!   Bit positions are `h1 + i*h2 (mod nbits)` for `i in 0..num_hashes`.
//! - **3 hash functions** gives ~1% FPR at 10 bits/item (optimal).
//! - **AVX-512 batch insert** uses `_mm512_conflict_epi64` (avx512cd) to
//!   detect duplicate hashes within an 8-key batch and skip redundant
//!   writes to the same word.
//! - **AVX-512 batch check** uses `_mm512_cmpeq_epi64_mask` (avx512f) to
//!   test 8 keys in parallel by gathering bit-test results.
//!
//! # Hot path: `might_contain` — ~8 instructions, fully scalar
//!
//! ```text
//! 1. crc32(key)              → 2 cycles (hardware CRC)
//! 2. h1 = crc * K1            → 1 imul
//! 3. h2 = (crc >> 32) | 1     → 1 shr + 1 or (non-zero)
//! 4. mask = self.mask         → 1 mov
//! 5. word0 = bits[h1 & mask / 64]  → 1 load (L1 hit)
//! 6. test bit (h1 & 63)       → 1 shr + 1 and
//! 7. combined positions h1+h2, h1+2*h2  → 2 add + 2 loads + 2 tests
//! 8. AND results              → 1 and
//! ```
//! Total: ~10 cycles when L1-resident (5-6 cycles for the second/third
//! bit tests overlap with the first). Compare to ~14 cycles for an L2
//! hash-table directory probe — the bloom filter pays for itself even
//! at FPR=1%.

use core::arch::x86_64::{
    _mm512_and_si512, _mm512_cmpeq_epi64_mask, _mm512_conflict_epi64, _mm512_loadu_epi64,
    _mm512_set1_epi64, _mm512_sllv_epi64, _mm512_storeu_epi64,
};

/// AVX-512 bloom filter for join semi-join pre-filtering.
///
/// Uses double hashing with hardware CRC32 for the primary hash.
/// Sized for ~1% false positive rate at 10 bits per item with 3 hash
/// functions (optimal). The bit array length is always a power of two
/// so modulo reduces to a bitmask.
pub struct BloomFilter {
    /// Bit array, length = 2^k words (so 64 * 2^k bits).
    bits: Vec<u64>,
    /// `bits.len() - 1`, used as a bitmask for fast word-index modulo.
    word_mask: usize,
    /// Number of hash functions (typically 3 for 1% FPR @ 10 bits/item).
    num_hashes: usize,
    /// Number of items inserted (for FPR estimation).
    num_items: usize,
}

impl BloomFilter {
    /// Create a new bloom filter sized for `expected_items` entries.
    ///
    /// Sizes the bit array for ~1% false-positive rate:
    ///   bits = expected_items * 10  (10 bits per item, k=3 hashes → optimal)
    ///
    /// Rounded up to the next power of 2 (in 64-bit words) for fast
    /// bitmask modulo. Minimum 1 word to avoid degenerate cases.
    pub fn new(expected_items: usize) -> Self {
        // 10 bits per item, 8 bits per byte, 64 bits per word → 10/64 words per item
        let min_words = (expected_items * 10 + 63) / 64;
        // Round up to power of 2, minimum 1
        let nwords = (min_words.max(1)).next_power_of_two();
        BloomFilter { bits: vec![0u64; nwords], word_mask: nwords - 1, num_hashes: 3, num_items: 0 }
    }

    /// Create with a custom number of hash functions (for testing).
    pub fn with_hashes(expected_items: usize, num_hashes: usize) -> Self {
        let mut bf = Self::new(expected_items);
        bf.num_hashes = num_hashes.max(1).min(7);
        bf
    }

    /// Number of items inserted.
    pub fn len(&self) -> usize {
        self.num_items
    }

    /// Is the filter empty?
    pub fn is_empty(&self) -> bool {
        self.num_items == 0
    }

    /// Number of bits in the filter.
    pub fn bit_count(&self) -> usize {
        self.bits.len() * 64
    }

    /// Estimate the false-positive rate given the current load.
    ///
    /// FPR ≈ (1 - e^(-kn/m))^k where k=num_hashes, n=items, m=bits.
    pub fn estimated_fpr(&self) -> f64 {
        if self.num_items == 0 {
            return 0.0;
        }
        let m = self.bit_count() as f64;
        let k = self.num_hashes as f64;
        let n = self.num_items as f64;
        let exponent = -k * n / m; // = -kn/m
        let base = 1.0 - exponent.exp(); // = 1 - e^(-kn/m), in [0, 1]
        base.powf(k)
    }

    /// Primary hash: hardware CRC32 (two passes with different seeds for
    /// h1 and h2), then mix each 32-bit CRC into a full 64-bit value with
    /// splitmix64-style mixing to ensure all 64 bits have good entropy.
    ///
    /// The mixing step is critical: without it, h1 = crc * constant would
    /// have all its low-17 bits (which determine the bit position in a
    /// 128K-bit filter) derived only from the low-17 bits of the 32-bit
    /// CRC — causing clustered bit-sets and high FPR.
    #[inline]
    fn hash_pair(key: u64) -> (u64, u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use core::arch::x86_64::_mm_crc32_u64;
            // Two CRC32 passes with different seeds → two independent 32-bit hashes.
            let crc1 = _mm_crc32_u64(0, key);
            let crc2 = _mm_crc32_u64(!0u64, key);
            // Mix each 32-bit CRC into a full 64-bit value to spread entropy
            // across all 64 bits (avoids clustering in the low bits).
            let h1 = Self::mix64(crc1);
            let h2 = Self::mix64(crc2) | 1; // OR 1 ensures non-zero step.
            (h1, h2)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Fallback: splitmix64 with the key as input.
            let h1 = Self::mix64(key);
            let h2 = Self::mix64(key.wrapping_add(0x9E3779B97F4A7C15)) | 1;
            (h1, h2)
        }
    }

    /// splitmix64-style 64-bit mixing function. Takes a 64-bit input and
    /// produces a well-distributed 64-bit output. Used to spread the
    /// 32-bit CRC result across all 64 bits so that the bit-position
    /// selection (which uses low bits) has full entropy.
    #[inline]
    fn mix64(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9E3779B97F4A7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Set the bit at position `bit_idx` (mod bit_count).
    #[inline]
    fn set_bit(&mut self, bit_idx: u64) {
        let word_idx = ((bit_idx as usize) >> 6) & self.word_mask;
        let bit_in_word = (bit_idx & 63) as u64;
        // SAFETY: word_idx is masked to bits.len()-1, so always in bounds.
        unsafe {
            *self.bits.get_unchecked_mut(word_idx) |= 1u64 << bit_in_word;
        }
    }

    /// Test the bit at position `bit_idx` (mod bit_count).
    #[inline]
    fn test_bit(&self, bit_idx: u64) -> bool {
        let word_idx = ((bit_idx as usize) >> 6) & self.word_mask;
        let bit_in_word = (bit_idx & 63) as u64;
        // SAFETY: word_idx is masked to bits.len()-1.
        unsafe { (*self.bits.get_unchecked(word_idx) >> bit_in_word) & 1 != 0 }
    }

    /// Insert a key. Uses double hashing: bit positions are
    /// `h1, h1+h2, h1+2*h2, ...` for `num_hashes` positions (mod bit_count).
    #[inline]
    pub fn insert(&mut self, key: u64) {
        let (h1, h2) = Self::hash_pair(key);
        let mut bit = h1;
        for _ in 0..self.num_hashes {
            self.set_bit(bit);
            bit = bit.wrapping_add(h2);
        }
        self.num_items += 1;
    }

    /// Batch insert using AVX-512 `_mm512_conflict_epi64` (avx512cd) to
    /// detect keys that produce the *same value* within an 8-key batch.
    /// For each conflict group, only the first key (lowest lane index)
    /// performs the CRC32 hash and bit-set; subsequent duplicates skip
    /// (idempotent insert — setting the same bit twice is a no-op).
    ///
    /// This is most useful when many probe keys are repeated (e.g.
    /// foreign-key joins with skewed distributions) — the conflict
    /// detection avoids redundant CRC32 + 3× memory RMW cycles per
    /// duplicate.
    ///
    /// # Safety
    /// Requires AVX-512F + AVX-512CD at runtime. Caller must ensure
    /// `keys.len()` is a multiple of 8 (caller pads with a sentinel
    /// value if needed — inserting a duplicate is harmless).
    #[target_feature(enable = "avx512f,avx512cd")]
    #[inline]
    pub unsafe fn insert_batch(&mut self, keys: &[u64]) {
        debug_assert!(keys.len() % 8 == 0);
        let mut i = 0;
        while i < keys.len() {
            // Load 8 keys as a 512-bit vector.
            let vkeys = _mm512_loadu_epi64(keys.as_ptr().add(i) as *const i64);
            // _mm512_conflict_epi64 returns a vector where lane i contains
            // a bitmask of OTHER lanes j (j != i) with the same value as
            // lane i. Bit j of lane i's value = 1 if lane j == lane i.
            let conflict_vec = _mm512_conflict_epi64(vkeys);
            // Extract per-lane conflict bitmasks via _mm512_movepi64_mask,
            // which gives one bit per i64 lane (the LSB of each lane).
            // That's not what we want — we want the FULL 8-bit conflict
            // mask per lane. Instead, store the conflict vector to memory
            // and read each lane's mask.
            let mut lane_buf = [0u64; 8];
            _mm512_storeu_epi64(lane_buf.as_mut_ptr() as *mut i64, vkeys);
            let mut conflict_lanes = [0u64; 8];
            _mm512_storeu_epi64(conflict_lanes.as_mut_ptr() as *mut i64, conflict_vec);
            for j in 0..8 {
                // earlier_mask = bits 0..j-1 (lanes BEFORE this one).
                // If any earlier lane has the same key, skip — that lane
                // already inserted this key.
                let earlier_mask: u64 = if j == 0 { 0 } else { (1u64 << j) - 1 };
                if conflict_lanes[j] & earlier_mask != 0 {
                    continue;
                }
                self.insert(lane_buf[j]);
            }
            i += 8;
        }
    }

    /// Check if a key MIGHT be present. Returns `false` = definitely not
    /// present (skip hash-table probe). Returns `true` = might be present
    /// (caller must probe the hash table to confirm).
    ///
    /// Hot path: ~8 instructions when L1-resident.
    #[inline]
    pub fn might_contain(&self, key: u64) -> bool {
        let (h1, h2) = Self::hash_pair(key);
        let mut bit = h1;
        // Unroll 3 iterations — compiler typically emits a tight 3-test
        // sequence with the loads pipelined.
        for _ in 0..self.num_hashes {
            if !self.test_bit(bit) {
                return false;
            }
            bit = bit.wrapping_add(h2);
        }
        true
    }

    /// Prefetch the bloom filter bits for a given key into all cache levels.
    /// Call this K rows ahead of the actual `might_contain` call to hide
    /// the random access latency for large bloom filters (>L2 size).
    ///
    /// For a 6M-item build side, the bloom filter is ~7.5MB — too large
    /// for L1 (32KB) or L2 (1MB), so each `might_contain` check incurs
    /// 3 random L3 accesses. Prefetching the first hash position's word
    /// (the first of 3) hides most of the latency.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    pub fn prefetch(&self, key: u64) {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let (h1, _h2) = Self::hash_pair(key);
        let word_idx = ((h1 as usize) >> 6) & self.word_mask;
        // SAFETY: word_idx = (h1 >> 6) & word_mask, where word_mask = bits.len()-1.
        // So word_idx < bits.len(). Pointer add is in-bounds.
        unsafe {
            _mm_prefetch(self.bits.as_ptr().add(word_idx) as *const i8, _MM_HINT_T0);
        }
    }

    /// No-op prefetch fallback for non-x86_64 targets.
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    pub fn prefetch(&self, _key: u64) {}

    /// Batch check 8 keys at once using AVX-512. Returns a `u8` mask
    /// where bit `i` = 1 if `keys[i]` might be present (caller must
    /// probe the hash table to confirm), and bit `i` = 0 if `keys[i]`
    /// is definitely not present (skip probe).
    ///
    /// The bit-test phase (3 cycles × 3 hashes = 9 cycles for 8 keys)
    /// is vectorized via `_mm512_cmpeq_epi64_mask` — 8× throughput vs
    /// scalar. The CRC32 hashing phase remains scalar (no AVX-512 CRC
    /// intrinsic exists), but the bit-test phase is the bottleneck
    /// because it requires 3 random L1 loads per key.
    ///
    /// # Safety
    /// Requires AVX-512F at runtime. Caller must pass exactly 8 keys.
    #[target_feature(enable = "avx512f")]
    #[inline]
    pub unsafe fn might_contain_batch(&self, keys: &[u64; 8]) -> u8 {
        // Compute h1 and h2 for all 8 keys (scalar — no vectorized CRC32).
        let mut h1s = [0u64; 8];
        let mut h2s = [0u64; 8];
        for i in 0..8 {
            let (h1, h2) = Self::hash_pair(keys[i]);
            h1s[i] = h1;
            h2s[i] = h2;
        }

        let mut result_mask: u8 = 0xFF; // assume all might_contain until proven absent
        let mut bit = h1s;
        for _ in 0..self.num_hashes {
            // Compute word_idx = (bit >> 6) & word_mask and bit_in_word = bit & 63.
            let mut word_indices = [0usize; 8];
            let mut bit_in_words = [0u64; 8];
            for i in 0..8 {
                word_indices[i] = ((bit[i] as usize) >> 6) & self.word_mask;
                bit_in_words[i] = bit[i] & 63;
            }

            // Gather the 8 words (scalar — _mm512_i64gather_epi64 would be
            // used here for very large filters, but for L1-resident filters
            // scalar pointer-deref is faster due to load-port pressure).
            let mut words = [0u64; 8];
            for i in 0..8 {
                words[i] = *self.bits.get_unchecked(word_indices[i]);
            }

            // AVX-512: build test masks (1 << bit_in_word) for all 8 lanes
            // in parallel, AND with the gathered words, and compare against
            // zero to identify which lanes have the bit UNSET.
            let shift_vec = _mm512_loadu_epi64(bit_in_words.as_ptr() as *const i64);
            let ones = _mm512_set1_epi64(1i64);
            let test_masks = _mm512_sllv_epi64(ones, shift_vec);
            let word_vec = _mm512_loadu_epi64(words.as_ptr() as *const i64);
            let and_result = _mm512_and_si512(word_vec, test_masks);
            // _mm512_cmpeq_epi64_mask returns __mmask8 (one bit per i64 lane).
            // Lanes where and_result == 0 → bit NOT set → key definitely absent.
            let not_set_mask = _mm512_cmpeq_epi64_mask(and_result, _mm512_set1_epi64(0)) as u8;
            // Clear those lanes from the result mask.
            result_mask &= !not_set_mask;

            // Advance bit positions by h2 for the next hash function.
            for i in 0..8 {
                bit[i] = bit[i].wrapping_add(h2s[i]);
            }
        }
        result_mask
    }
}

impl Default for BloomFilter {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_check() {
        let mut bf = BloomFilter::new(100);
        for i in 0..100u64 {
            bf.insert(i * 7);
        }
        for i in 0..100u64 {
            assert!(bf.might_contain(i * 7), "key {} should be present", i * 7);
        }
        // Definitely absent keys should mostly return false (some may
        // false-positive at ~1% rate).
        let mut false_positives = 0;
        for i in 100..200u64 {
            if bf.might_contain(i * 7) {
                false_positives += 1;
            }
        }
        // 100 absent keys × 1% FPR → expect ~1 false positive, allow up to 10.
        assert!(false_positives < 10, "too many false positives: {}", false_positives);
    }

    #[test]
    fn test_empty_filter_returns_false() {
        let bf = BloomFilter::new(100);
        // An empty filter should never claim a key might be present.
        for k in [0u64, 1, 42, 1000, u64::MAX] {
            assert!(!bf.might_contain(k), "empty filter should not contain {}", k);
        }
    }

    #[test]
    fn test_estimated_fpr_at_1_percent() {
        // For 1% FPR with k=3 hashes, we need m/n ≈ 10 bits per item.
        let mut bf = BloomFilter::new(10_000);
        for i in 0..10_000u64 {
            bf.insert(i);
        }
        let fpr = bf.estimated_fpr();
        // Should be near 1% — allow 0.1% to 5%.
        assert!(fpr > 0.001 && fpr < 0.05, "FPR estimate out of range: {}", fpr);

        // Empirical FPR: probe with absent keys and count false positives.
        let mut fp = 0;
        let trials = 10_000;
        for i in 10_000..(10_000 + trials) as u64 {
            if bf.might_contain(i) {
                fp += 1;
            }
        }
        let empirical_fpr = fp as f64 / trials as f64;
        // Empirical FPR should also be in a reasonable range.
        assert!(empirical_fpr < 0.10, "empirical FPR too high: {}", empirical_fpr);
    }

    #[test]
    fn test_large_load_distribution() {
        let n = 100_000;
        let mut bf = BloomFilter::new(n);
        for i in 0..n as u64 {
            bf.insert(i);
        }
        // All inserted keys must be found.
        for i in 0..n as u64 {
            assert!(bf.might_contain(i), "missing key {}", i);
        }
        // Spot-check absent keys.
        let mut fp = 0;
        for i in (n as u64)..((n + 10_000) as u64) {
            if bf.might_contain(i) {
                fp += 1;
            }
        }
        // At 1% FPR, expect ~100 false positives out of 10_000 absent probes.
        assert!(fp < 500, "too many false positives: {} / 10000", fp);
    }

    #[test]
    fn test_batch_check_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            eprintln!("avx512f not available — skipping batch check test");
            return;
        }
        let mut bf = BloomFilter::new(1000);
        for i in 0..1000u64 {
            bf.insert(i * 13);
        }
        // 8 keys: 4 present, 4 absent.
        let keys: [u64; 8] = [
            0 * 13,   // present
            100 * 13, // present
            500 * 13, // present
            999 * 13, // present
            1,        // absent
            2,        // absent
            3,        // absent
            4,        // absent (might FP at ~1%)
        ];
        let batch_mask = unsafe { bf.might_contain_batch(&keys) };
        for (i, &k) in keys.iter().enumerate() {
            let scalar = bf.might_contain(k);
            let batch = (batch_mask >> i) & 1 == 1;
            // Batch and scalar should always agree (same algorithm).
            assert_eq!(scalar, batch, "key {} (idx {}): scalar={} batch={}", k, i, scalar, batch);
        }
        // First 4 (present keys) must all have bit=1.
        assert_eq!(batch_mask & 0x0F, 0x0F, "first 4 keys should all be present");
    }

    #[test]
    fn test_batch_insert_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") || !is_x86_feature_detected!("avx512cd") {
            eprintln!("avx512f/avx512cd not available — skipping batch insert test");
            return;
        }
        // Build two identical filters, one via scalar insert, one via batch.
        let n = 1024; // multiple of 8
        let keys: Vec<u64> = (0..n).map(|i| (i as u64).wrapping_mul(0x9E3779B97F4A7C15)).collect();

        let mut bf_scalar = BloomFilter::new(n);
        for &k in &keys {
            bf_scalar.insert(k);
        }

        let mut bf_batch = BloomFilter::new(n);
        unsafe {
            bf_batch.insert_batch(&keys);
        }

        // Both filters must agree on all probe keys.
        for &k in &keys {
            assert!(bf_batch.might_contain(k), "batch filter missing key {}", k);
            assert!(bf_scalar.might_contain(k), "scalar filter missing key {}", k);
        }
        // Both must have the same false-positive behavior on absent keys.
        for i in 0..1000u64 {
            let s = bf_scalar.might_contain(i + 1_000_000);
            let b = bf_batch.might_contain(i + 1_000_000);
            assert_eq!(s, b, "FPR mismatch on absent key {}: scalar={} batch={}", i, s, b);
        }
    }

    #[test]
    fn test_filter_size_grows_with_items() {
        let bf_small = BloomFilter::new(100);
        let bf_large = BloomFilter::new(1_000_000);
        // Large filter must be much bigger.
        assert!(bf_large.bit_count() > bf_small.bit_count() * 100);
        // Both should be powers of 2 in word count.
        assert!(bf_small.bit_count().is_power_of_two());
        assert!(bf_large.bit_count().is_power_of_two());
    }

    #[test]
    fn test_zero_key_handling() {
        let mut bf = BloomFilter::new(100);
        bf.insert(0);
        assert!(bf.might_contain(0));
        // h2 for key=0 must be non-zero (we OR with 1) so the 3 bit
        // positions are distinct. If h2 were 0, all 3 hashes would test
        // the same bit, giving a degenerate filter.
        let (h1, h2) = BloomFilter::hash_pair(0);
        assert_ne!(h2, 0, "h2 must be non-zero for key=0");
        assert!(h1 != h1.wrapping_add(h2), "h1 and h1+h2 must differ for key=0");
    }
}
