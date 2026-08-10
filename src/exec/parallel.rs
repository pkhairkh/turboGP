//! # Multi-threaded execution (Wave 24).
//!
//! Implements morsel-driven parallelism for scan-heavy queries. The table
//! is split into morsels (chunks of rows), each processed by a worker
//! thread. Partial results are merged at the end.
//!
//! Uses rayon for thread pool management. Each worker processes a morsel
//! independently and produces a partial aggregate; the driver merges them.

use rayon::prelude::*;

/// A morsel: a contiguous range of rows [start, end) in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Morsel {
    pub start: usize,
    pub end: usize,
}

/// Split a row count into morsels of approximately `morsel_size` rows each.
pub fn split_morsels(row_count: usize, morsel_size: usize) -> Vec<Morsel> {
    if row_count == 0 || morsel_size == 0 {
        return Vec::new();
    }
    let mut morsels = Vec::new();
    let mut start = 0;
    while start < row_count {
        let end = (start + morsel_size).min(row_count);
        morsels.push(Morsel { start, end });
        start = end;
    }
    morsels
}

/// Parallel count(*) using rayon. Splits the row count into morsels and
/// counts in parallel. Returns the total count.
pub fn parallel_count(row_count: usize) -> u64 {
    if row_count == 0 {
        return 0;
    }
    let morsel_size = (row_count / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(row_count, morsel_size);
    morsels.par_iter().map(|m| (m.end - m.start) as u64).sum()
}

/// Parallel sum of a column. Splits into morsels and sums in parallel.
pub fn parallel_sum(col: &[u64]) -> u64 {
    if col.is_empty() {
        return 0;
    }
    let morsel_size = (col.len() / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(col.len(), morsel_size);
    morsels.par_iter().map(|m| col[m.start..m.end].iter().sum::<u64>()).sum()
}

/// Parallel min of a column.
pub fn parallel_min(col: &[u64]) -> u64 {
    if col.is_empty() {
        return 0;
    }
    let morsel_size = (col.len() / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(col.len(), morsel_size);
    morsels.par_iter().filter_map(|m| col[m.start..m.end].iter().min().copied()).min().unwrap_or(0)
}

/// Parallel max of a column.
pub fn parallel_max(col: &[u64]) -> u64 {
    if col.is_empty() {
        return 0;
    }
    let morsel_size = (col.len() / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(col.len(), morsel_size);
    morsels.par_iter().filter_map(|m| col[m.start..m.end].iter().max().copied()).max().unwrap_or(0)
}

/// Parallel count with a filter mask. Counts cells where mask is true.
pub fn parallel_count_masked(mask: &[bool]) -> u64 {
    if mask.is_empty() {
        return 0;
    }
    let morsel_size = (mask.len() / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(mask.len(), morsel_size);
    morsels.par_iter().map(|m| mask[m.start..m.end].iter().filter(|&&b| b).count() as u64).sum()
}

/// Parallel sum with a filter mask. Sums cells where mask is true.
pub fn parallel_sum_masked(col: &[u64], mask: &[bool]) -> u64 {
    assert_eq!(col.len(), mask.len());
    if col.is_empty() {
        return 0;
    }
    let morsel_size = (col.len() / rayon::current_num_threads().max(1)).max(1024);
    let morsels = split_morsels(col.len(), morsel_size);
    morsels
        .par_iter()
        .map(|m| {
            col[m.start..m.end]
                .iter()
                .zip(mask[m.start..m.end].iter())
                .filter(|(_, &b)| b)
                .map(|(&c, _)| c)
                .sum::<u64>()
        })
        .sum()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_morsels_basic() {
        let morsels = split_morsels(100, 25);
        assert_eq!(morsels.len(), 4);
        assert_eq!(morsels[0], Morsel { start: 0, end: 25 });
        assert_eq!(morsels[3], Morsel { start: 75, end: 100 });
    }

    #[test]
    fn split_morsels_uneven() {
        let morsels = split_morsels(100, 30);
        assert_eq!(morsels.len(), 4);
        assert_eq!(morsels[0], Morsel { start: 0, end: 30 });
        assert_eq!(morsels[3], Morsel { start: 90, end: 100 });
    }

    #[test]
    fn split_morsels_empty() {
        assert!(split_morsels(0, 100).is_empty());
    }

    #[test]
    fn parallel_count_basic() {
        assert_eq!(parallel_count(1000), 1000);
        assert_eq!(parallel_count(0), 0);
        assert_eq!(parallel_count(1_000_000), 1_000_000);
    }

    #[test]
    fn parallel_sum_basic() {
        let col: Vec<u64> = (1..=1000).collect();
        assert_eq!(parallel_sum(&col), 500500);
    }

    #[test]
    fn parallel_sum_empty() {
        assert_eq!(parallel_sum(&[]), 0);
    }

    #[test]
    fn parallel_min_max() {
        let col: Vec<u64> = vec![5, 3, 8, 1, 9, 2, 7, 4, 6];
        assert_eq!(parallel_min(&col), 1);
        assert_eq!(parallel_max(&col), 9);
    }

    #[test]
    fn parallel_count_masked_test() {
        let mask: Vec<bool> = vec![true, false, true, true, false];
        assert_eq!(parallel_count_masked(&mask), 3);
    }

    #[test]
    fn parallel_sum_masked_test() {
        let col: Vec<u64> = vec![10, 20, 30, 40, 50];
        let mask: Vec<bool> = vec![true, false, true, false, true];
        assert_eq!(parallel_sum_masked(&col, &mask), 90); // 10+30+50
    }

    #[test]
    fn parallel_large_dataset() {
        let col: Vec<u64> = (0..100_000).map(|i| i as u64).collect();
        let expected: u64 = (0..100_000).map(|i| i as u64).sum();
        assert_eq!(parallel_sum(&col), expected);
        assert_eq!(parallel_min(&col), 0);
        assert_eq!(parallel_max(&col), 99_999);
    }
}
