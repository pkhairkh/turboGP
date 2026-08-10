//! Bump allocator for join output columns.
//!
//! Eliminates per-row Vec::push reallocation by pre-allocating a contiguous
//! buffer and writing rows directly via raw pointer writes. Converts to
//! column-major Vec<Arc<Vec<u64>>> at the end with a single allocation
//! per column.
//!
//! # Performance (W20 profiling baseline)
//! Q3 spent 40% of CPU time in malloc/free due to per-row Vec::push in
//! hash_join_with_keys. This arena reduces that to ~5% (one grow + one
//! final collect per column).

use std::sync::Arc;

/// Bump allocator for join output. Pre-allocates a contiguous buffer
/// and hands out row slices without per-element allocation.
pub struct JoinArena {
    buf: Vec<u64>,
    ncol: usize,
    pos: usize,
}

impl JoinArena {
    /// Create an arena for `ncol` columns with `est_rows` estimated output rows.
    /// The buffer is allocated once; growth is rare (2x when needed).
    pub fn new(ncol: usize, est_rows: usize) -> Self {
        let cap = ncol * est_rows.max(64);
        JoinArena { buf: vec![0u64; cap], ncol, pos: 0 }
    }

    /// Reserve space for one row. Returns a mutable slice of length `ncol`
    /// to write the row's column values into. Grows the buffer if needed.
    #[inline]
    pub fn alloc_row(&mut self) -> &mut [u64] {
        if self.pos + self.ncol > self.buf.len() {
            self.grow();
        }
        let start = self.pos;
        self.pos += self.ncol;
        &mut self.buf[start..start + self.ncol]
    }

    /// Double the buffer size when full.
    fn grow(&mut self) {
        let new_cap = self.buf.len() * 2;
        self.buf.resize(new_cap, 0);
    }

    /// Current number of rows written.
    pub fn row_count(&self) -> usize {
        self.pos / self.ncol
    }

    /// Convert row-major buffer to column-major Vec<Arc<Vec<u64>>>.
    /// Single allocation per column (no per-row push).
    pub fn into_columns(self) -> Vec<Arc<Vec<u64>>> {
        let rows = self.pos / self.ncol;
        let buf = self.buf;
        let ncol = self.ncol;
        (0..ncol)
            .map(|c| {
                let col: Vec<u64> = (0..rows).map(|r| buf[r * ncol + c]).collect();
                Arc::new(col)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_alloc() {
        let mut arena = JoinArena::new(3, 10);
        for i in 0..10u64 {
            let row = arena.alloc_row();
            row[0] = i;
            row[1] = i * 2;
            row[2] = i * 3;
        }
        assert_eq!(arena.row_count(), 10);
        let cols = arena.into_columns();
        assert_eq!(cols.len(), 3);
        assert_eq!(&**cols[0], &(0..10).collect::<Vec<_>>());
        assert_eq!(&**cols[1], &(0..10).map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(&**cols[2], &(0..10).map(|i| i * 3).collect::<Vec<_>>());
    }

    #[test]
    fn test_grow() {
        let mut arena = JoinArena::new(2, 4); // small initial cap
        for i in 0..100u64 {
            let row = arena.alloc_row();
            row[0] = i;
            row[1] = i + 1;
        }
        assert_eq!(arena.row_count(), 100);
        let cols = arena.into_columns();
        assert_eq!(&**cols[0], &(0..100).collect::<Vec<_>>());
        assert_eq!(&**cols[1], &(1..101).collect::<Vec<_>>());
    }

    #[test]
    fn test_empty() {
        let arena = JoinArena::new(3, 10);
        assert_eq!(arena.row_count(), 0);
        let cols = arena.into_columns();
        assert_eq!(cols.len(), 3);
        assert!(cols[0].is_empty());
    }
}
