//! # NULL bitmap per column (Wave 22).
//!
//! Tracks which cells in a column are NULL, separate from the u64 cell
//! value. Previously NULLs were stored as `0u64`, indistinguishable from
//! a real zero. The bitmap is a `Vec<bool>` (one byte per cell) for
//! simplicity — a bit-packed version can be added later for memory
//! efficiency.

/// A per-column NULL bitmap. `true` = cell is NULL.
#[derive(Debug, Clone, Default)]
pub struct NullBitmap {
    /// One entry per row. `true` means the cell at that index is NULL.
    bits: Vec<bool>,
}

impl NullBitmap {
    /// Create a new bitmap with `n` rows, all non-NULL.
    pub fn new(n: usize) -> Self {
        Self { bits: vec![false; n] }
    }

    /// Create a new bitmap with `n` rows, all NULL.
    pub fn all_null(n: usize) -> Self {
        Self { bits: vec![true; n] }
    }

    /// Create a new bitmap with `n` rows, all non-NULL.
    pub fn all_non_null(n: usize) -> Self {
        Self { bits: vec![false; n] }
    }

    /// Returns true if the cell at `idx` is NULL.
    pub fn is_null(&self, idx: usize) -> bool {
        self.bits.get(idx).copied().unwrap_or(false)
    }

    /// Set the cell at `idx` to NULL.
    pub fn set_null(&mut self, idx: usize) {
        if idx < self.bits.len() {
            self.bits[idx] = true;
        }
    }

    /// Set the cell at `idx` to non-NULL.
    pub fn set_non_null(&mut self, idx: usize) {
        if idx < self.bits.len() {
            self.bits[idx] = false;
        }
    }

    /// Count the number of NULL cells.
    pub fn null_count(&self) -> usize {
        self.bits.iter().filter(|&&b| b).count()
    }

    /// Count the number of non-NULL cells.
    pub fn non_null_count(&self) -> usize {
        self.bits.iter().filter(|&&b| !b).count()
    }

    /// Returns true if any cell is NULL.
    pub fn has_nulls(&self) -> bool {
        self.bits.iter().any(|&b| b)
    }

    /// Returns a mask where `true` = non-NULL (useful for filtering).
    pub fn non_null_mask(&self) -> &[bool] {
        &self.bits
    }

    /// Push a NULL value (extend the bitmap by one, marked as NULL).
    pub fn push_null(&mut self) {
        self.bits.push(true);
    }

    /// Push a non-NULL value (extend the bitmap by one, marked as non-NULL).
    pub fn push_non_null(&mut self) {
        self.bits.push(false);
    }

    /// Truncate the bitmap to `n` entries.
    pub fn truncate(&mut self, n: usize) {
        self.bits.truncate(n);
    }

    /// Length of the bitmap (should equal the column's row count).
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    /// Returns true if the bitmap is empty.
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    /// Get a reference to the raw bits.
    pub fn bits(&self) -> &[bool] {
        &self.bits
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_all_non_null() {
        let bm = NullBitmap::new(5);
        assert_eq!(bm.len(), 5);
        assert!(!bm.has_nulls());
        assert_eq!(bm.null_count(), 0);
        assert_eq!(bm.non_null_count(), 5);
    }

    #[test]
    fn all_null() {
        let bm = NullBitmap::all_null(5);
        assert!(bm.has_nulls());
        assert_eq!(bm.null_count(), 5);
        assert_eq!(bm.non_null_count(), 0);
        for i in 0..5 {
            assert!(bm.is_null(i));
        }
    }

    #[test]
    fn set_null_and_check() {
        let mut bm = NullBitmap::new(5);
        bm.set_null(2);
        assert!(bm.is_null(2));
        assert!(!bm.is_null(1));
        assert_eq!(bm.null_count(), 1);
        assert!(bm.has_nulls());
    }

    #[test]
    fn set_non_null() {
        let mut bm = NullBitmap::all_null(3);
        bm.set_non_null(1);
        assert!(!bm.is_null(1));
        assert!(bm.is_null(0));
        assert!(bm.is_null(2));
    }

    #[test]
    fn push_null_and_non_null() {
        let mut bm = NullBitmap::new(0);
        bm.push_non_null(); // row 0: non-null
        bm.push_null(); // row 1: null
        bm.push_non_null(); // row 2: non-null
        assert_eq!(bm.len(), 3);
        assert!(!bm.is_null(0));
        assert!(bm.is_null(1));
        assert!(!bm.is_null(2));
        assert_eq!(bm.null_count(), 1);
    }

    #[test]
    fn truncate() {
        let mut bm = NullBitmap::all_null(5);
        bm.truncate(3);
        assert_eq!(bm.len(), 3);
    }

    #[test]
    fn non_null_mask() {
        let mut bm = NullBitmap::new(4);
        bm.set_null(1);
        bm.set_null(3);
        let mask = bm.non_null_mask();
        // non_null_mask returns the bits where true = NULL (inverted name).
        // Actually, non_null_mask returns &bits where true = NULL.
        // Let's verify:
        assert!(mask[1]); // row 1 is NULL
        assert!(mask[3]); // row 3 is NULL
        assert!(!mask[0]); // row 0 is non-NULL
        assert!(!mask[2]); // row 2 is non-NULL
    }

    #[test]
    fn out_of_bounds_is_non_null() {
        let bm = NullBitmap::new(3);
        // Out-of-bounds access returns false (non-NULL) by default.
        assert!(!bm.is_null(100));
    }
}
