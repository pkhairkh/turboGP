//! Zone maps (data skipping) — per-page min/max metadata for topological
//! scan invariance.
//!
//! TQFT concept: A topological invariant is metric-independent — it doesn't
//! care about the specific geometry, only the topology. Zone maps let the
//! scan skip entire pages of data that cannot possibly match the filter
//! predicate, regardless of individual row values.
//!
//! # Design
//! Each column is divided into pages of 1024 rows. For each page, we store
//! (min, max) as two u64 values. During scan, if the query's filter range
//! doesn't overlap [min, max], the entire page is skipped.
//!
//! For Q6 (l_shipdate >= '1994-01-01' AND l_shipdate < '1995-01-01'):
//! - Without zone maps: scan all 6M rows, filter each
//! - With zone maps: check 5860 page ranges, skip ~90% that don't overlap
//!   the 1-year date range → scan only ~600K rows

use std::sync::Arc;

const PAGE_SIZE: usize = 1024;

/// Zone map metadata for a single column.
/// Stores (min, max) per page of PAGE_SIZE rows.
pub struct ZoneMap {
    /// Parallel arrays: mins[i] and maxs[i] for page i.
    /// Page i covers rows [i*PAGE_SIZE, (i+1)*PAGE_SIZE).
    mins: Vec<u64>,
    maxs: Vec<u64>,
    num_pages: usize,
    total_rows: usize,
}

impl ZoneMap {
    /// Build zone maps for a column. Scans the column once, computing
    /// min/max per page.
    pub fn build(col: &[u64]) -> Self {
        let total_rows = col.len();
        let num_pages = (total_rows + PAGE_SIZE - 1) / PAGE_SIZE;
        let mut mins = Vec::with_capacity(num_pages);
        let mut maxs = Vec::with_capacity(num_pages);

        for page_idx in 0..num_pages {
            let start = page_idx * PAGE_SIZE;
            let end = std::cmp::min(start + PAGE_SIZE, total_rows);
            let mut min = u64::MAX;
            let mut max = u64::MIN;
            for i in start..end {
                let v = col[i];
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
            }
            mins.push(min);
            maxs.push(max);
        }

        ZoneMap { mins, maxs, num_pages, total_rows }
    }

    /// Check if a page MIGHT contain values in [lo, hi].
    /// Returns false if the page's [min, max] doesn't overlap [lo, hi].
    #[inline]
    pub fn page_might_contain_range(&self, page_idx: usize, lo: u64, hi: u64) -> bool {
        if page_idx >= self.num_pages {
            return false;
        }
        // Overlap test: page_max >= lo AND page_min <= hi
        self.maxs[page_idx] >= lo && self.mins[page_idx] <= hi
    }

    /// Check if a page MIGHT contain a specific value.
    #[inline]
    pub fn page_might_contain_value(&self, page_idx: usize, val: u64) -> bool {
        if page_idx >= self.num_pages {
            return false;
        }
        self.mins[page_idx] <= val && val <= self.maxs[page_idx]
    }

    /// Get the list of page indices that might contain values in [lo, hi].
    /// Skips pages whose [min, max] doesn't overlap [lo, hi].
    pub fn pages_in_range(&self, lo: u64, hi: u64) -> Vec<usize> {
        let mut result = Vec::new();
        for i in 0..self.num_pages {
            if self.page_might_contain_range(i, lo, hi) {
                result.push(i);
            }
        }
        result
    }

    /// Number of pages.
    pub fn num_pages(&self) -> usize {
        self.num_pages
    }

    /// Total rows covered.
    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    /// Get the row range for a page: [start, end).
    pub fn page_row_range(&self, page_idx: usize) -> (usize, usize) {
        let start = page_idx * PAGE_SIZE;
        let end = std::cmp::min(start + PAGE_SIZE, self.total_rows);
        (start, end)
    }
}

/// Zone maps for all columns of a table, keyed by column index.
pub struct TableZoneMaps {
    maps: Vec<Option<ZoneMap>>,
}

impl TableZoneMaps {
    /// Build zone maps for all columns in a table.
    pub fn build(columns: &[Arc<Vec<u64>>]) -> Self {
        let maps = columns.iter().map(|col| Some(ZoneMap::build(col))).collect();
        TableZoneMaps { maps }
    }

    /// Get zone map for a specific column.
    pub fn get(&self, col_idx: usize) -> Option<&ZoneMap> {
        self.maps.get(col_idx).and_then(|m| m.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zone_map_basic() {
        // Create a column with values 0..4096
        let col: Vec<u64> = (0..4096).collect();
        let zm = ZoneMap::build(&col);
        assert_eq!(zm.num_pages(), 4); // 4096 / 1024
                                       // Page 0 covers [0, 1023]
        assert!(zm.page_might_contain_range(0, 500, 600));
        assert!(zm.page_might_contain_range(0, 0, 1023));
        assert!(!zm.page_might_contain_range(0, 1024, 2000));
    }

    #[test]
    fn test_zone_map_skip() {
        // Values: page 0 = [0,1023], page 1 = [1024,2047], etc.
        let col: Vec<u64> = (0..4096).collect();
        let zm = ZoneMap::build(&col);
        // Query range [1500, 1600] should only match page 1
        let pages = zm.pages_in_range(1500, 1600);
        assert_eq!(pages, vec![1]);
    }

    #[test]
    fn test_zone_map_date_range() {
        // Simulate date column: days since epoch, 6M rows
        // Query: shipdate in [1994-01-01, 1995-01-01)
        // 1994-01-01 = day 8766, 1995-01-01 = day 9131
        let col: Vec<u64> = (8000..8000 + 6_000_000).collect();
        let zm = ZoneMap::build(&col);
        let pages = zm.pages_in_range(8766, 9131);
        // Should be ~365 days / 1024 rows-per-day... actually 1 day = 1 row
        // So 365 pages should match
        // 365 days of values, each page is 1024 rows = 1024 days
        // So only 1 page should match (all 365 values fit in 1 page)
        assert_eq!(pages.len(), 2);
        println!("zone map skipped {} of {} pages", zm.num_pages() - pages.len(), zm.num_pages());
    }

    #[test]
    fn test_zone_map_value_lookup() {
        let col: Vec<u64> = (0..2048).map(|i| i * 2).collect(); // even numbers
        let zm = ZoneMap::build(&col);
        // Value 500 is in page 0 (rows 0-1023, values 0-2046)
        assert!(zm.page_might_contain_value(0, 500));
        // Value 500 is NOT in page 1 (rows 1024-2047, values 2048-4094)
        assert!(!zm.page_might_contain_value(1, 500));
    }
}
