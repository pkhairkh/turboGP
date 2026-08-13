//! HyperLogLog — probabilistic cardinality estimation.
//!
//! Replaces `HashSet<u64>` in COUNT(DISTINCT) for high-cardinality columns.
//! Uses O(1) memory (16 KB for precision 14) instead of O(n) for HashSet.
//! Approximate count with ~0.81% error at precision 14.
//!
//! Reference: Flajolet et al., "HyperLogLog: the analysis of a near-optimal
//! cardinality estimation algorithm", AOFA 2007.

use xxhash_rust::xxh3::xxh3_64;

/// Precision parameter: 2^P registers. P=14 gives 16384 registers (16 KB)
/// and standard error ~0.81%.
const P: u32 = 14;
const M: usize = 1 << P; // 16384
const ALPHA: f64 = 0.7213 / (1.0 + 1.079 / M as f64); // bias correction

/// HyperLogLog cardinality estimator.
pub struct HyperLogLog {
    registers: Vec<u8>,
}

impl HyperLogLog {
    /// Create a new HLL with all-zero registers.
    pub fn new() -> Self {
        Self {
            registers: vec![0u8; M],
        }
    }

    /// Add a value (hashed with xxh3 for uniform distribution).
    #[inline]
    pub fn add(&mut self, value: u64) {
        let h = xxh3_64(&value.to_le_bytes());
        self.add_hashed(h);
    }

    /// Add a pre-hashed 64-bit value.
    #[inline]
    pub fn add_hashed(&mut self, h: u64) {
        // Use the top P bits as the register index.
        let idx = (h >> (64 - P)) as usize;
        // Count leading zeros in the remaining (64 - P) bits, plus 1.
        // `leading_zeros` of the shifted-out portion.
        let w = h << P; // the remaining bits, shifted to the top
        let lz = (w.leading_zeros() + 1) as u8;
        // Update: register[idx] = max(register[idx], lz)
        if lz > self.registers[idx] {
            self.registers[idx] = lz;
        }
    }

    /// Estimate the cardinality.
    pub fn estimate(&self) -> u64 {
        let mut sum: f64 = 0.0;
        let mut zeros: usize = 0;
        for &r in &self.registers {
            sum += 2.0_f64.powi(-(r as i32));
            if r == 0 {
                zeros += 1;
            }
        }
        let raw = ALPHA * (M as f64).powi(2) / sum;

        // Small-range correction: if raw <= 2.5 * M and there are zero registers,
        // use the linear counting formula.
        let estimate = if raw <= 2.5 * M as f64 && zeros > 0 {
            M as f64 * (M as f64 / zeros as f64).ln()
        } else {
            raw
        };

        // Large-range correction (for cardinalities near 2^64) is not needed
        // since we use 64-bit hashes and our cardinalities are well below 2^64.
        estimate as u64
    }

    /// Merge another HLL into this one (for parallel accumulation).
    pub fn merge(&mut self, other: &HyperLogLog) {
        for i in 0..M {
            if other.registers[i] > self.registers[i] {
                self.registers[i] = other.registers[i];
            }
        }
    }
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Parallel COUNT(DISTINCT) using HLL. Splits the column into chunks,
/// each thread builds a local HLL, then merges.
///
/// Returns an approximate distinct count with ~0.81% error.
pub fn count_distinct_hll(col: &[u64]) -> u64 {
    use rayon::prelude::*;

    // For small inputs, use exact FxHashSet (faster + exact).
    if col.len() < 100_000 {
        let mut seen = fxhash::FxHashSet::default();
        for &v in col {
            seen.insert(v);
        }
        return seen.len() as u64;
    }

    const CHUNK_SIZE: usize = 1_000_000;
    let num_chunks = (col.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

    let local_hlls: Vec<HyperLogLog> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, col.len());
            let mut hll = HyperLogLog::new();
            for &v in &col[start..end] {
                hll.add(v);
            }
            hll
        })
        .collect();

    let mut global = HyperLogLog::new();
    for hll in local_hlls {
        global.merge(&hll);
    }
    global.estimate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hll_empty() {
        let hll = HyperLogLog::new();
        assert_eq!(hll.estimate(), 0);
    }

    #[test]
    fn test_hll_single() {
        let mut hll = HyperLogLog::new();
        hll.add(42);
        assert!(hll.estimate() >= 1 && hll.estimate() <= 2);
    }

    #[test]
    fn test_hll_small_exact() {
        // Small input should use exact FxHashSet path.
        let col: Vec<u64> = (0..1000).collect();
        let count = count_distinct_hll(&col);
        assert_eq!(count, 1000);
    }

    #[test]
    fn test_hll_large_approximate() {
        // 1M distinct values — HLL should be within 2%.
        let col: Vec<u64> = (0..1_000_000u64).collect();
        let count = count_distinct_hll(&col);
        let error = ((count as f64 - 1_000_000.0).abs() / 1_000_000.0) * 100.0;
        assert!(error < 2.0, "HLL error too large: {}%", error);
    }

    #[test]
    fn test_hll_with_duplicates() {
        let col: Vec<u64> = (0..500_000u64).chain(0..500_000u64).collect();
        let count = count_distinct_hll(&col);
        let error = ((count as f64 - 500_000.0).abs() / 500_000.0) * 100.0;
        assert!(error < 2.0, "HLL error too large: {}%", error);
    }

    #[test]
    fn test_hll_merge() {
        let mut hll1 = HyperLogLog::new();
        let mut hll2 = HyperLogLog::new();
        for i in 0..500_000u64 {
            hll1.add(i);
            hll2.add(i + 500_000);
        }
        hll1.merge(&hll2);
        let count = hll1.estimate();
        let error = ((count as f64 - 1_000_000.0).abs() / 1_000_000.0) * 100.0;
        assert!(error < 2.0, "HLL merge error too large: {}%", error);
    }
}
