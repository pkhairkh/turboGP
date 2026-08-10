//! Learned cardinality estimator.
//!
//! Augments the analytic cost model with data-driven selectivity estimates.
//! Per-(table, column) equi-width histograms + an exponentially-weighted
//! correction factor that learns from (predicted, actual) cardinality pairs.
//!
//! ## Design
//!
//! 1. **Histograms**: For each (table, column) pair, maintain an equi-width
//!    histogram with `N_BUCKETS` buckets. When a query filters on
//!    `column = value`, the histogram gives the fraction of rows matching.
//!
//! 2. **Correction factor**: An EWMA that tracks the ratio of actual to
//!    predicted cardinality. When the actual count is less than predicted,
//!    the correction factor shrinks future estimates.
//!
//! 3. **Online learning**: After each query executes, `update()` is called
//!    with the predicted and actual row counts. The correction factor
//!    converges to the true selectivity bias.

use std::collections::HashMap;

const N_BUCKETS: usize = 256;
const EWMA_ALPHA: f64 = 0.1;

/// Per-column histogram statistics.
#[derive(Debug, Clone)]
pub struct ColumnStats {
    /// Minimum value observed.
    pub min_val: i64,
    /// Maximum value observed.
    pub max_val: i64,
    /// Number of distinct values.
    pub distinct_count: u64,
    /// Total row count when stats were collected.
    pub row_count: u64,
    /// Equi-width histogram buckets (normalized to [0, 1]).
    pub buckets: Vec<f64>,
}

/// Learned cardinality estimator.
#[derive(Debug, Clone)]
pub struct LearnedCardinality {
    /// Per-(table, column) statistics.
    stats: HashMap<(String, String), ColumnStats>,
    /// Global correction factor (EWMA of actual/predicted ratio).
    correction: f64,
}

impl LearnedCardinality {
    /// Create a new empty estimator.
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            correction: 1.0,
        }
    }

    /// Attach column statistics.
    pub fn with_stats(mut self, table: &str, column: &str, stats: ColumnStats) -> Self {
        self.stats.insert((table.to_string(), column.to_string()), stats);
        self
    }

    /// Estimate the selectivity of an equality predicate: `column = value`.
    ///
    /// Returns a fraction in [0, 1] representing the estimated fraction
    /// of matching rows.
    pub fn selectivity_eq(&self, table: &str, column: &str, value: i64) -> f64 {
        if let Some(stats) = self.stats.get(&(table.to_string(), column.to_string())) {
            let selectivity = histogram_selectivity(stats, value);
            selectivity * self.correction
        } else {
            // No stats: fall back to analytic default
            0.1 * self.correction
        }
    }

    /// Estimate the selectivity of a range predicate: `column < value`.
    pub fn selectivity_lt(&self, table: &str, column: &str, value: i64) -> f64 {
        if let Some(stats) = self.stats.get(&(table.to_string(), column.to_string())) {
            if stats.max_val <= stats.min_val {
                return 0.5 * self.correction;
            }
            let frac = (value - stats.min_val) as f64 / (stats.max_val - stats.min_val) as f64;
            frac.clamp(0.0, 1.0) * self.correction
        } else {
            0.33 * self.correction
        }
    }

    /// Estimate the selectivity of a range predicate: `column > value`.
    pub fn selectivity_gt(&self, table: &str, column: &str, value: i64) -> f64 {
        if let Some(stats) = self.stats.get(&(table.to_string(), column.to_string())) {
            if stats.max_val <= stats.min_val {
                return 0.5 * self.correction;
            }
            let frac = (stats.max_val - value) as f64 / (stats.max_val - stats.min_val) as f64;
            frac.clamp(0.0, 1.0) * self.correction
        } else {
            0.33 * self.correction
        }
    }

    /// Update the estimator with observed (predicted, actual) cardinality.
    ///
    /// The correction factor is updated via EWMA:
    /// `correction = α × (actual/predicted) + (1-α) × correction`
    pub fn update(&mut self, predicted: u64, actual: u64) {
        if predicted == 0 {
            return;
        }
        let ratio = actual as f64 / predicted as f64;
        self.correction = EWMA_ALPHA * ratio + (1.0 - EWMA_ALPHA) * self.correction;
    }

    /// Get the current correction factor.
    pub fn correction(&self) -> f64 {
        self.correction
    }

    /// Collect statistics for a column from a vector of values.
    pub fn collect_stats(values: &[i64]) -> ColumnStats {
        if values.is_empty() {
            return ColumnStats {
                min_val: 0, max_val: 0, distinct_count: 0,
                row_count: 0, buckets: vec![0.0; N_BUCKETS],
            };
        }

        let min_val = *values.iter().min().unwrap();
        let max_val = *values.iter().max().unwrap();
        let distinct_count = values.iter().collect::<std::collections::HashSet<_>>().len() as u64;
        let row_count = values.len() as u64;

        let mut buckets = vec![0u64; N_BUCKETS];
        let range = (max_val - min_val).max(1) as usize;
        for &v in values {
            let bucket_idx = ((v - min_val) as usize * N_BUCKETS / range).min(N_BUCKETS - 1);
            buckets[bucket_idx] += 1;
        }

        // Normalize to fractions
        let max_bucket = *buckets.iter().max().unwrap_or(&1) as f64;
        let buckets: Vec<f64> = buckets.iter()
            .map(|&c| c as f64 / max_bucket)
            .collect();

        ColumnStats {
            min_val, max_val, distinct_count, row_count, buckets,
        }
    }
}

impl Default for LearnedCardinality {
    fn default() -> Self {
        Self::new()
    }
}

/// Look up the selectivity of `column = value` from the histogram.
fn histogram_selectivity(stats: &ColumnStats, value: i64) -> f64 {
    if stats.buckets.is_empty() || stats.max_val <= stats.min_val {
        return 0.1;
    }
    let range = (stats.max_val - stats.min_val) as usize;
    let bucket_idx = ((value - stats.min_val) as usize * N_BUCKETS / range).min(N_BUCKETS - 1);
    let bucket_density = stats.buckets[bucket_idx];

    // Selectivity = bucket_density / distinct_per_bucket
    let distinct_per_bucket = (stats.distinct_count as f64 / N_BUCKETS as f64).max(1.0);
    (bucket_density / distinct_per_bucket).clamp(0.001, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_stats() {
        let values: Vec<i64> = (0..1000).map(|i| i * 2).collect();
        let stats = LearnedCardinality::collect_stats(&values);
        assert_eq!(stats.min_val, 0);
        assert_eq!(stats.max_val, 1998);
        assert_eq!(stats.distinct_count, 1000);
        assert_eq!(stats.row_count, 1000);
        assert_eq!(stats.buckets.len(), N_BUCKETS);
    }

    #[test]
    fn test_selectivity_eq_with_stats() {
        let values: Vec<i64> = (0..1000).collect();
        let stats = LearnedCardinality::collect_stats(&values);
        let est = LearnedCardinality::new().with_stats("t", "id", stats);

        let sel = est.selectivity_eq("t", "id", 500);
        assert!(sel > 0.0 && sel < 1.0);
    }

    #[test]
    fn test_selectivity_eq_without_stats() {
        let est = LearnedCardinality::new();
        let sel = est.selectivity_eq("t", "id", 500);
        // Default 0.1 × correction(1.0) = 0.1
        assert!((sel - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_correction_factor_update() {
        let mut est = LearnedCardinality::new();
        assert!((est.correction() - 1.0).abs() < 0.01);

        // Predicted 1000, actual 500 → ratio 0.5
        est.update(1000, 500);
        // correction = 0.1 × 0.5 + 0.9 × 1.0 = 0.95
        assert!((est.correction() - 0.95).abs() < 0.01);
    }

    #[test]
    fn test_selectivity_lt() {
        let values: Vec<i64> = (0..1000).collect();
        let stats = LearnedCardinality::collect_stats(&values);
        let est = LearnedCardinality::new().with_stats("t", "id", stats);

        let sel = est.selectivity_lt("t", "id", 500);
        // Should be approximately 0.5 (half the range)
        assert!(sel > 0.4 && sel < 0.6);
    }

    #[test]
    fn test_empty_stats() {
        let stats = LearnedCardinality::collect_stats(&[]);
        assert_eq!(stats.min_val, 0);
        assert_eq!(stats.max_val, 0);
        assert_eq!(stats.buckets.len(), N_BUCKETS);
    }
}
