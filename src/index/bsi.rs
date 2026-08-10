//! Bit-Sliced Index (no ADR — see `src/index/bsi.rs`).
//!
//! ADR-014 covers HLC over PTP for clock synchronization, *not* bit-sliced
//! indexes; the historical cross-reference in this module was incorrect and
//! has been removed.
//!
//! A Bit-Sliced Index represents a column of `u64` values as 64 separate
//! bitmaps, one per bit position. Equality lookups then reduce to a fixed
//! 64-step sequence of AND / AND-NOT operations over the slices — branchless
//! and SIMD-friendly.
//!
//! ## Layout
//!
//! For `len` rows we keep 64 bitmaps each of `len` bits. `slices[i]` holds
//! bit `i` of every cell (`i = 0` is the LSB). Memory cost is
//! `64 * ceil(len/64) * 8` bytes — i.e. one byte per bit of the original
//! column, which is the asymptotically optimal bit-packed representation.
//!
//! ## Operations
//!
//! `find_eq(target)` computes, for each bit position `i`:
//!
//! ```text
//!   mask_i = target_i ? slice_i : ~slice_i
//! ```
//!
//! then ANDs all 64 masks together. The result is a bitmap whose set bits are
//! exactly the row indices whose cell equals `target`.

use std::collections::VecDeque;

/// A packed bit-vector of length `len` bits, stored as `ceil(len/64)` `u64`
/// limbs. Bits beyond `len` in the final limb are always zero.
#[derive(Clone, Debug)]
pub struct Bitmap {
    /// Packed bits, little-endian within each limb.
    bits: Vec<u64>,
    /// Logical length in bits.
    len: usize,
}

impl Bitmap {
    /// Create an all-zero bitmap of `len` bits.
    pub fn zeros(len: usize) -> Self {
        let limbs = len.div_ceil(64);
        Self { bits: vec![0u64; limbs], len }
    }

    /// Set the bit at index `i`. Panics if `i >= len`.
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len, "bitmap set out of range: {i} >= {}", self.len);
        let limb = i / 64;
        let bit = i % 64;
        self.bits[limb] |= 1u64 << bit;
    }

    /// Get the bit at index `i`. Returns `false` for `i >= len`.
    pub fn get(&self, i: usize) -> bool {
        if i >= self.len {
            return false;
        }
        let limb = i / 64;
        let bit = i % 64;
        (self.bits[limb] >> bit) & 1 == 1
    }

    /// Population count — number of set bits in `[0, len)`.
    pub fn popcount(&self) -> usize {
        let full = self.len / 64;
        let mut count: usize = self.bits[..full].iter().map(|x| x.count_ones() as usize).sum();
        if full < self.bits.len() {
            // Mask off the tail bits beyond `len` in the last limb.
            let tail = self.len % 64;
            if tail != 0 {
                let mask = (1u64 << tail) - 1;
                count += (self.bits[full] & mask).count_ones() as usize;
            }
        }
        count
    }

    /// Bitwise AND. Both operands must share the same `len`.
    pub fn and(&self, other: &Bitmap) -> Bitmap {
        debug_assert_eq!(
            self.len, other.len,
            "Bitmap::and length mismatch: {} vs {}",
            self.len, other.len
        );
        let mut out = Bitmap::zeros(self.len);
        for (i, (a, b)) in self.bits.iter().zip(other.bits.iter()).enumerate() {
            out.bits[i] = a & b;
        }
        out
    }

    /// Bitwise OR. Both operands must share the same `len`.
    pub fn or(&self, other: &Bitmap) -> Bitmap {
        debug_assert_eq!(
            self.len, other.len,
            "Bitmap::or length mismatch: {} vs {}",
            self.len, other.len
        );
        let mut out = Bitmap::zeros(self.len);
        for (i, (a, b)) in self.bits.iter().zip(other.bits.iter()).enumerate() {
            out.bits[i] = a | b;
        }
        out
    }

    /// Bitwise NOT, restricted to `[0, len)`. Bits beyond `len` stay zero.
    pub fn not(&self) -> Bitmap {
        let mut out = Bitmap::zeros(self.len);
        let full = self.len / 64;
        for i in 0..full {
            out.bits[i] = !self.bits[i];
        }
        let tail = self.len % 64;
        if tail != 0 {
            let mask = (1u64 << tail) - 1;
            out.bits[full] = (!self.bits[full]) & mask;
        }
        out
    }

    /// Collect the indices of all set bits in ascending order.
    pub fn set_indices(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.popcount());
        for (limb_idx, limb) in self.bits.iter().enumerate() {
            if *limb == 0 {
                continue;
            }
            let mut bits = *limb;
            while bits != 0 {
                let trailing = bits.trailing_zeros() as usize;
                let idx = limb_idx * 64 + trailing;
                if idx < self.len {
                    out.push(idx);
                }
                bits &= bits - 1;
            }
        }
        out
    }

    /// Logical length in bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the bitmap is empty (zero length).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Approximate byte footprint (limbs × 8).
    pub fn byte_size(&self) -> usize {
        self.bits.len() * 8
    }
}

/// A Bit-Sliced Index over a column of `u64` values.
///
/// 64 bitmaps, `slices[i]` holding bit `i` (LSB at `i = 0`) of every cell.
#[derive(Clone, Debug)]
pub struct BitSlicedIndex {
    /// `slices[i]` = bitmap for bit position `i`.
    slices: Vec<Bitmap>,
    /// Number of indexed rows.
    len: usize,
}

impl BitSlicedIndex {
    /// Build a BSI from a slice of `u64` cells.
    pub fn build(cells: &[u64]) -> Self {
        let len = cells.len();
        let mut slices: Vec<Bitmap> = (0..64).map(|_| Bitmap::zeros(len)).collect();
        for (row, &cell) in cells.iter().enumerate() {
            // Iterate set bits of `cell` and set the corresponding slice.
            let mut bits = cell;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                slices[b].set(row);
                bits &= bits - 1;
            }
        }
        Self { slices, len }
    }

    /// Number of indexed rows.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return a bitmap whose set bits are exactly the rows where the cell
    /// equals `target`.
    ///
    /// For each bit position `i`:
    /// - if bit `i` of `target` is set, the slice contributes `slices[i]`;
    /// - otherwise it contributes `NOT slices[i]`.
    ///
    /// ANDing all 64 contributions yields the equality mask.
    pub fn find_eq(&self, target: u64) -> Bitmap {
        if self.len == 0 {
            return Bitmap::zeros(0);
        }
        // Seed with bit 0.
        let mut acc = if target & 1 != 0 { self.slices[0].clone() } else { self.slices[0].not() };
        for i in 1..64u32 {
            let bit_set = (target >> i) & 1 != 0;
            // Use a queue-free pairwise reduction; we just fold left.
            let contribution = if bit_set {
                self.slices[i as usize].clone()
            } else {
                self.slices[i as usize].not()
            };
            acc = acc.and(&contribution);
        }
        acc
    }

    /// Total byte footprint: 64 limbs-of-bitmaps × 8 bytes.
    pub fn byte_size(&self) -> usize {
        self.slices.iter().map(|b| b.byte_size()).sum()
    }
}

// Make `VecDeque` reachable so future pairwise-AND optimisations can use it
// without re-importing at the call site.
#[allow(dead_code)]
type _LimbQueue = VecDeque<u64>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsi_find_eq_basic() {
        let cells = [1u64, 2, 3, 2, 4, 2, 5];
        let idx = BitSlicedIndex::build(&cells);
        let mask = idx.find_eq(2);
        let mut indices = mask.set_indices();
        indices.sort_unstable();
        assert_eq!(indices, vec![1, 3, 5]);
    }

    #[test]
    fn bsi_find_eq_empty() {
        let idx = BitSlicedIndex::build(&[]);
        let mask = idx.find_eq(42);
        assert!(mask.set_indices().is_empty());
        assert_eq!(mask.popcount(), 0);
    }

    #[test]
    fn bsi_find_eq_single() {
        let cells = [7u64, 7, 7, 7];
        let idx = BitSlicedIndex::build(&cells);
        let mask = idx.find_eq(7);
        assert_eq!(mask.popcount(), 4);
        assert_eq!(mask.set_indices(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn bsi_find_eq_none() {
        let cells = [1u64, 2, 3];
        let idx = BitSlicedIndex::build(&cells);
        let mask = idx.find_eq(99);
        assert_eq!(mask.popcount(), 0);
    }

    #[test]
    fn bsi_byte_size_is_64_bitmaps() {
        let cells = [0u64; 128];
        let idx = BitSlicedIndex::build(&cells);
        // 128 bits = 2 limbs per slice; 64 slices; 8 bytes per limb.
        assert_eq!(idx.byte_size(), 64 * 2 * 8);
    }

    #[test]
    fn bitmap_and_or_not_popcount() {
        let mut a = Bitmap::zeros(128);
        for i in [0, 2, 4, 6, 100] {
            a.set(i);
        }
        let mut b = Bitmap::zeros(128);
        for i in [0, 1, 4, 7, 100] {
            b.set(i);
        }
        assert_eq!(a.popcount(), 5);
        assert_eq!(b.popcount(), 5);
        let and = a.and(&b);
        assert_eq!(and.set_indices(), vec![0, 4, 100]);
        let or = a.or(&b);
        // Union of {0,2,4,6,100} and {0,1,4,7,100} = {0,1,2,4,6,7,100} → 7.
        assert_eq!(or.popcount(), 7);
        let not_a = a.not();
        assert_eq!(not_a.popcount(), 128 - 5);
        // NOT is involutive.
        assert_eq!(not_a.not().set_indices(), a.set_indices());
    }

    #[test]
    fn bitmap_get_out_of_range_is_false() {
        let bm = Bitmap::zeros(10);
        assert!(!bm.get(99));
    }
}
