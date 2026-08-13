//! AVX-512 bitmap mask for vectorized row filtering.
//!
//! Stores 1 bit per row packed into `Vec<u8>`. AVX-512 mask compares
//! (`_mm512_cmpeq_epi64_mask`, `_mm512_cmp_pd_mask`, …) produce
//! `__mmask8` (8 bits per 8-lane vector compare) which pack directly
//! into the bitmap bytes — no expansion to bytes is needed in the hot
//! filter loop.
//!
//! # Multi-accumulator discipline (Wave 12 lesson)
//!
//! Wave 12 found that the existing hand-written AVX-512 kernels in
//! `src/kernel/scan.rs` and `src/kernel/aggregate.rs` *underperform*
//! rustc's auto-vectorized scalar code because they use a single
//! accumulator whose POPCNT/VADD depends on the previous iteration's
//! result (e.g. `scan_eq_avx512_l3` hits only 0.86 G cells/sec vs the
//! scalar `scan_eq_scalar` at 2.15 G).
//!
//! Every AVX-512 filter loop in this module therefore processes
//! **4 independent vectors (32 rows) per iteration**, issuing 4
//! independent loads, 4 independent compares, and 4 independent mask
//! stores. This gives the out-of-order scheduler 4 independent
//! dependency chains to overlap, hiding the load+compare latency and
//! keeping the AVX-512 ports saturated.
//!
//! # CPU dispatch
//!
//! Each public filter function dispatches at runtime to its AVX-512
//! implementation guarded by `is_x86_feature_detected!("avx512f")`
//! (plus `"avx512bw"` for the byte-mask expansion intrinsics used in
//! `and_into_bool`). A scalar fallback handles non-AVX-512 hosts so
//! the test runner does not require AVX-512.

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// A packed bitmap: 1 bit per row, LSB-first within each byte.
///
/// Bit `i` lives in `bits[i >> 3]`, at position `i & 7`. This matches
/// the natural output of AVX-512 mask compares
/// (`_mm512_cmpeq_epi64_mask` returns bit 0 = lane 0, bit 7 = lane 7).
#[derive(Debug, Clone)]
pub struct Bitmap {
    bits: Vec<u8>,
    len: usize,
}

impl Bitmap {
    /// Allocate an all-zero bitmap for `len` rows.
    pub fn new(len: usize) -> Self {
        let bytes = (len + 7) / 8;
        Self { bits: vec![0u8; bytes], len }
    }

    /// Allocate an all-ones bitmap for `len` rows. Tail bits beyond
    /// `len` in the last byte are cleared so `count_ones()` returns
    /// exactly `len`.
    pub fn all_ones(len: usize) -> Self {
        let bytes = (len + 7) / 8;
        let mut bits = vec![0xFFu8; bytes];
        let unused = (8 - (len & 7)) & 7;
        if unused > 0 {
            if let Some(last) = bits.last_mut() {
                *last >>= unused;
            }
        }
        Self { bits, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set bit `i` to 1.
    #[inline]
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.bits[i >> 3] |= 1 << (i & 7);
    }

    /// Set bit `i` to 0.
    #[inline]
    pub fn clear(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.bits[i >> 3] &= !(1 << (i & 7));
    }

    /// Get bit `i`.
    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.bits[i >> 3] >> (i & 7)) & 1 != 0
    }

    /// Bitwise AND of two bitmaps (length = min).
    pub fn and(&self, other: &Bitmap) -> Bitmap {
        let len = self.len.min(other.len);
        let mut out = Bitmap::new(len);
        let n = self.bits.len().min(other.bits.len());
        for i in 0..n {
            out.bits[i] = self.bits[i] & other.bits[i];
        }
        out
    }

    /// Bitwise OR of two bitmaps (length = max).
    pub fn or(&self, other: &Bitmap) -> Bitmap {
        let len = self.len.max(other.len);
        let mut out = Bitmap::new(len);
        let n = self.bits.len().min(other.bits.len());
        for i in 0..n {
            out.bits[i] = self.bits[i] | other.bits[i];
        }
        out
    }

    /// Bitwise NOT. Tail bits beyond `len` are cleared so
    /// `count_ones()` of the result equals `len - count_ones(self)`.
    pub fn not(&self) -> Bitmap {
        let mut out = Bitmap::new(self.len);
        for i in 0..self.bits.len() {
            out.bits[i] = !self.bits[i];
        }
        // Clear the unused HIGH bits of the last byte (do NOT shift —
        // shifting would also move the valid bits).
        let unused = (8 - (self.len & 7)) & 7;
        if unused > 0 {
            if let Some(last) = out.bits.last_mut() {
                let mask: u8 = (1u8 << (8 - unused)).wrapping_sub(1);
                *last &= mask;
            }
        }
        out
    }

    /// Count set bits (popcount over the packed bytes).
    pub fn count_ones(&self) -> usize {
        self.bits.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Expand to a `Vec<bool>` (1 byte per row).
    pub fn to_bool_vec(&self) -> Vec<bool> {
        (0..self.len).map(|i| self.get(i)).collect()
    }

    /// Pack a `&[bool]` into a bitmap.
    pub fn from_bool_slice(bools: &[bool]) -> Bitmap {
        let mut bm = Bitmap::new(bools.len());
        for (i, &b) in bools.iter().enumerate() {
            if b {
                bm.set(i);
            }
        }
        bm
    }

    /// In-place bitwise AND: `self &= other` (length = min).
    /// AVX-512BW fast path processes 64 bytes (512 bits) per iteration.
    #[inline]
    pub fn and_inplace(&mut self, other: &Bitmap) {
        let n = self.bits.len().min(other.bits.len());
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe {
                and_inplace_avx512(&mut self.bits[..n], &other.bits[..n]);
            }
            return;
        }
        for i in 0..n {
            self.bits[i] &= other.bits[i];
        }
    }

    /// In-place bitwise OR: `self |= other` (length = min).
    /// AVX-512BW fast path processes 64 bytes (512 bits) per iteration.
    #[inline]
    pub fn or_inplace(&mut self, other: &Bitmap) {
        let n = self.bits.len().min(other.bits.len());
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe {
                or_inplace_avx512(&mut self.bits[..n], &other.bits[..n]);
            }
            return;
        }
        for i in 0..n {
            self.bits[i] |= other.bits[i];
        }
    }

    /// Write the bitmap's bits into a caller-supplied bool slice
    /// (1 byte per row). Avoids the `Vec<bool>` allocation of
    /// `to_bool_vec` when the caller already has a reusable buffer.
    #[inline]
    pub fn to_bool_slice(&self, out: &mut [bool]) {
        debug_assert!(out.len() >= self.len, "to_bool_slice: out too small");
        for i in 0..self.len {
            out[i] = (self.bits[i >> 3] >> (i & 7)) & 1 != 0;
        }
    }

    /// Read-only access to the packed byte buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Mutable access to the packed byte buffer (used by AVX-512
    /// filter loops to write mask bytes directly).
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bits
    }

    /// Iterate over the indices of all set bits (true rows), in ascending
    /// order. Uses `tzcnt` (BMI1) on x86_64 for O(1) next-set-bit lookup,
    /// which is ~5x faster than the scalar `for i in 0..len { if get(i) }`
    /// pattern for sparse masks (e.g. selective WHERE filters).
    ///
    /// This is the primary consumer primitive for the W5A Bitmap migration:
    /// replaces `for i in 0..n { if mask[i] { ... } }` with
    /// `for i in mask.iter_set_bits() { ... }`, skipping false rows without
    /// a branch per row.
    pub fn iter_set_bits(&self) -> SetBitIter<'_> {
        SetBitIter { bits: &self.bits, len: self.len, pos: 0 }
    }

    /// Batch-lookup: return `true` for each index in `indices` that is set.
    /// Cheaper than `indices.iter().map(|&i| self.get(i)).collect()` because
    /// it avoids the per-element function-call overhead and lets the compiler
    /// vectorize the gather.
    pub fn get_batch(&self, indices: &[usize]) -> Vec<bool> {
        indices.iter().map(|&i| self.get(i)).collect()
    }

    /// Count of set bits in `[start, end)`. Uses POPCNT on the covered bytes,
    /// with bit-level masking for the partial head/tail bytes.
    pub fn count_ones_range(&self, start: usize, end: usize) -> usize {
        if start >= end || start >= self.len {
            return 0;
        }
        let end = end.min(self.len);
        let mut count = 0usize;
        // Partial head byte
        let head_byte = start >> 3;
        let head_bit = start & 7;
        let head_end = (end.min((head_byte + 1) << 3)).min(end);
        if head_bit != 0 && head_end > start {
            for i in start..head_end {
                if self.get(i) {
                    count += 1;
                }
            }
        }
        // Full middle bytes
        let full_start = if head_bit != 0 { head_byte + 1 } else { head_byte };
        let full_end = end >> 3;
        if full_start < full_end {
            let slice = &self.bits[full_start..full_end];
            count += slice.iter().map(|b| b.count_ones() as usize).sum::<usize>();
        }
        // Partial tail byte
        let tail_start = (full_end << 3).max(head_end);
        if tail_start < end {
            for i in tail_start..end {
                if self.get(i) {
                    count += 1;
                }
            }
        }
        count
    }
}

/// Iterator over the set-bit indices of a `Bitmap`. Yields `usize` indices
/// in ascending order. Uses `trailing_zeros()` (lowered to `tzcnt` on x86_64
/// with BMI1) to skip runs of zero bits in O(1) per run.
pub struct SetBitIter<'a> {
    bits: &'a [u8],
    len: usize,
    pos: usize,
}

impl<'a> Iterator for SetBitIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.pos < self.len {
            let byte_idx = self.pos >> 3;
            let bit_idx = self.pos & 7;
            let byte = self.bits[byte_idx];
            if byte == 0 {
                // Skip 8 bits at once (whole zero byte).
                self.pos = (byte_idx + 1) << 3;
                continue;
            }
            // Find the next set bit at or after self.pos within this byte.
            let shifted = byte >> bit_idx;
            if shifted == 0 {
                // No more set bits in this byte.
                self.pos = (byte_idx + 1) << 3;
                continue;
            }
            let tz = shifted.trailing_zeros() as usize;
            let result = self.pos + tz;
            if result >= self.len {
                return None;
            }
            self.pos = result + 1;
            return Some(result);
        }
        None
    }
}

// =============================================================================
// Public dispatch shims (runtime AVX-512 detection + scalar fallback)
// =============================================================================

/// `col == val` for a u64 (Int/Date) column.
pub fn filter_eq_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| _mm512_cmpeq_epi64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c == val {
            bm.set(i);
        }
    }
    bm
}

/// `col != val` for a u64 column.
pub fn filter_ne_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| !_mm512_cmpeq_epi64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c != val {
            bm.set(i);
        }
    }
    bm
}

/// `col < val` (unsigned) for a u64 column.
pub fn filter_lt_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| _mm512_cmplt_epu64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c < val {
            bm.set(i);
        }
    }
    bm
}

/// `col > val` (unsigned) for a u64 column.
pub fn filter_gt_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| _mm512_cmpgt_epu64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c > val {
            bm.set(i);
        }
    }
    bm
}

/// `col <= val` (unsigned) for a u64 column.
pub fn filter_le_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| _mm512_cmple_epu64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c <= val {
            bm.set(i);
        }
    }
    bm
}

/// `col >= val` (unsigned) for a u64 column.
pub fn filter_ge_u64(col: &[u64], val: u64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_u64_avx512_inner(col, val, |v, t| _mm512_cmpge_epu64_mask(v, t)) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c >= val {
            bm.set(i);
        }
    }
    bm
}

/// `col < val` (signed) for an Int column. Matches the existing scalar
/// path in `apply_comparison` which casts both sides to i64.
pub fn filter_lt_i64(col: &[u64], val: i64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe {
            filter_u64_avx512_inner(col, val as u64, |v, t| _mm512_cmplt_epi64_mask(v, t))
        };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if (c as i64) < val {
            bm.set(i);
        }
    }
    bm
}

/// `col <= val` (signed).
pub fn filter_le_i64(col: &[u64], val: i64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe {
            filter_u64_avx512_inner(col, val as u64, |v, t| _mm512_cmple_epi64_mask(v, t))
        };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if (c as i64) <= val {
            bm.set(i);
        }
    }
    bm
}

/// `col > val` (signed).
pub fn filter_gt_i64(col: &[u64], val: i64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe {
            filter_u64_avx512_inner(col, val as u64, |v, t| _mm512_cmpgt_epi64_mask(v, t))
        };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if (c as i64) > val {
            bm.set(i);
        }
    }
    bm
}

/// `col >= val` (signed).
pub fn filter_ge_i64(col: &[u64], val: i64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe {
            filter_u64_avx512_inner(col, val as u64, |v, t| _mm512_cmpge_epi64_mask(v, t))
        };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if (c as i64) >= val {
            bm.set(i);
        }
    }
    bm
}

// ---- f64 filters ----

/// `col == val` for an f64 column (col is f64::to_bits-encoded u64s).
pub fn filter_eq_f64(col: &[u64], val: f64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_f64_avx512_inner::<_CMP_EQ_OQ>(col, val) };
    }
    let vb = val.to_bits();
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if c == vb {
            bm.set(i);
        }
    }
    bm
}

/// `col != val` for an f64 column.
pub fn filter_ne_f64(col: &[u64], val: f64) -> Bitmap {
    filter_eq_f64_epsilon(col, val).not()
}

/// `col == val` with epsilon tolerance for f64 columns.
/// Used when comparing against subquery results that may differ by ULPs
/// due to different summation orders. Tolerance: relative 1e-9.
pub fn filter_eq_f64_epsilon(col: &[u64], val: f64) -> Bitmap {
    let mut bm = Bitmap::new(col.len());
    let abs_tol = 1e-6 * val.abs().max(1.0);
    for (i, &c) in col.iter().enumerate() {
        let cv = f64::from_bits(c);
        if (cv - val).abs() <= abs_tol {
            bm.set(i);
        }
    }
    bm
}

/// `col == val` for an f64 column (exact, no epsilon).
pub fn filter_eq_f64_exact(col: &[u64], val: f64) -> Bitmap {
    // No direct `_CMP_NEQ_OQ` immediate in stdarch; invert `==`.
    filter_eq_f64(col, val).not()
}

/// `col < val` for an f64 column.
pub fn filter_lt_f64(col: &[u64], val: f64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_f64_avx512_inner::<_CMP_LT_OQ>(col, val) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if f64::from_bits(c) < val {
            bm.set(i);
        }
    }
    bm
}

/// `col > val` for an f64 column.
pub fn filter_gt_f64(col: &[u64], val: f64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_f64_avx512_inner::<_CMP_GT_OQ>(col, val) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if f64::from_bits(c) > val {
            bm.set(i);
        }
    }
    bm
}

/// `col <= val` for an f64 column.
pub fn filter_le_f64(col: &[u64], val: f64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_f64_avx512_inner::<_CMP_LE_OQ>(col, val) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if f64::from_bits(c) <= val {
            bm.set(i);
        }
    }
    bm
}

/// `col >= val` for an f64 column.
pub fn filter_ge_f64(col: &[u64], val: f64) -> Bitmap {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        return unsafe { filter_f64_avx512_inner::<_CMP_GE_OQ>(col, val) };
    }
    let mut bm = Bitmap::new(col.len());
    for (i, &c) in col.iter().enumerate() {
        if f64::from_bits(c) >= val {
            bm.set(i);
        }
    }
    bm
}

// =============================================================================
// Bitmap-into-bool-mask combinators
// =============================================================================

/// `mask[i] = mask[i] && bm.get(i)` — fold a bitmap into an existing
/// bool mask in place. Uses AVX-512BW `_mm512_movm2b` to expand 64
/// packed bits to 64 bytes (0x00/0xFF), then ANDs against the 64-byte
/// chunk of `mask`. Falls back to a scalar loop on non-AVX-512 hosts.
pub fn and_into_bool(bm: &Bitmap, mask: &mut [bool]) {
    debug_assert_eq!(bm.len(), mask.len(), "bitmap and mask must have same length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe {
                and_into_bool_avx512(bm, mask);
            }
            return;
        }
    }
    let bits = bm.as_bytes();
    for i in 0..mask.len() {
        if (bits[i >> 3] >> (i & 7)) & 1 == 0 {
            mask[i] = false;
        }
    }
}

/// `mask[i] = mask[i] || bm.get(i)` — OR-fold a bitmap into a bool mask.
pub fn or_into_bool(bm: &Bitmap, mask: &mut [bool]) {
    debug_assert_eq!(bm.len(), mask.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe {
                or_into_bool_avx512(bm, mask);
            }
            return;
        }
    }
    let bits = bm.as_bytes();
    for i in 0..mask.len() {
        if (bits[i >> 3] >> (i & 7)) & 1 != 0 {
            mask[i] = true;
        }
    }
}

// =============================================================================
// AVX-512 inner loops
// =============================================================================

/// 4-way unrolled inner loop for u64 filters. The `cmp` closure is
/// inlined by the compiler, so each call site becomes a tight loop
/// with 4 independent compare chains (the Wave-12 multi-accumulator
/// discipline: 4 independent dependency chains hide load+compare
/// latency).
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn filter_u64_avx512_inner<F>(col: &[u64], val: u64, cmp: F) -> Bitmap
where
    F: Fn(__m512i, __m512i) -> u8,
{
    let n = col.len();
    let mut bm = Bitmap::new(n);
    let bytes = bm.as_bytes_mut();
    let target = _mm512_set1_epi64(val as i64);

    let mut i = 0usize;
    let mut byte_idx = 0usize;
    // 4-way unrolled: 4 independent loads + compares per iteration.
    while i + 32 <= n {
        let v0 = _mm512_loadu_epi64(col.as_ptr().add(i) as *const i64);
        let v1 = _mm512_loadu_epi64(col.as_ptr().add(i + 8) as *const i64);
        let v2 = _mm512_loadu_epi64(col.as_ptr().add(i + 16) as *const i64);
        let v3 = _mm512_loadu_epi64(col.as_ptr().add(i + 24) as *const i64);
        // 4 independent compares — no accumulator dependency chain.
        let m0 = cmp(v0, target);
        let m1 = cmp(v1, target);
        let m2 = cmp(v2, target);
        let m3 = cmp(v3, target);
        // Pack 4 mask bytes (32 bits) into 4 consecutive bitmap bytes.
        *bytes.get_unchecked_mut(byte_idx) = m0;
        *bytes.get_unchecked_mut(byte_idx + 1) = m1;
        *bytes.get_unchecked_mut(byte_idx + 2) = m2;
        *bytes.get_unchecked_mut(byte_idx + 3) = m3;
        i += 32;
        byte_idx += 4;
    }
    // 1-vector tail (8 rows).
    while i + 8 <= n {
        let v = _mm512_loadu_epi64(col.as_ptr().add(i) as *const i64);
        let m = cmp(v, target);
        *bytes.get_unchecked_mut(byte_idx) = m;
        i += 8;
        byte_idx += 1;
    }
    // Scalar tail (0..7 rows). Emulate the same comparison semantics
    // by rebuilding a single-lane __m512i and calling `cmp`.
    if i < n {
        let mut lane = [0i64; 8];
        while i < n {
            lane[0] = col[i] as i64;
            let v = _mm512_loadu_epi64(lane.as_ptr());
            if (cmp(v, target) & 1) != 0 {
                *bytes.get_unchecked_mut(byte_idx) |= 1u8 << (i & 7);
            }
            i += 1;
        }
    }
    bm
}

/// 4-way unrolled inner loop for f64 filters. The predicate is a
/// const generic so `_mm512_cmp_pd_mask` (which requires a const
/// immediate) can be specialized at compile time. Same multi-
/// accumulator discipline as the u64 version.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn filter_f64_avx512_inner<const PRED: i32>(col: &[u64], val: f64) -> Bitmap {
    let n = col.len();
    let mut bm = Bitmap::new(n);
    let bytes = bm.as_bytes_mut();
    let target = _mm512_set1_pd(val);

    let mut i = 0usize;
    let mut byte_idx = 0usize;
    // 4-way unrolled.
    while i + 32 <= n {
        // Reinterpret u64 bits as f64 via cast intrinsic.
        let v0 = _mm512_castsi512_pd(_mm512_loadu_epi64(col.as_ptr().add(i) as *const i64));
        let v1 = _mm512_castsi512_pd(_mm512_loadu_epi64(col.as_ptr().add(i + 8) as *const i64));
        let v2 = _mm512_castsi512_pd(_mm512_loadu_epi64(col.as_ptr().add(i + 16) as *const i64));
        let v3 = _mm512_castsi512_pd(_mm512_loadu_epi64(col.as_ptr().add(i + 24) as *const i64));
        let m0 = _mm512_cmp_pd_mask(v0, target, PRED);
        let m1 = _mm512_cmp_pd_mask(v1, target, PRED);
        let m2 = _mm512_cmp_pd_mask(v2, target, PRED);
        let m3 = _mm512_cmp_pd_mask(v3, target, PRED);
        *bytes.get_unchecked_mut(byte_idx) = m0;
        *bytes.get_unchecked_mut(byte_idx + 1) = m1;
        *bytes.get_unchecked_mut(byte_idx + 2) = m2;
        *bytes.get_unchecked_mut(byte_idx + 3) = m3;
        i += 32;
        byte_idx += 4;
    }
    while i + 8 <= n {
        let v = _mm512_castsi512_pd(_mm512_loadu_epi64(col.as_ptr().add(i) as *const i64));
        let m = _mm512_cmp_pd_mask(v, target, PRED);
        *bytes.get_unchecked_mut(byte_idx) = m;
        i += 8;
        byte_idx += 1;
    }
    // Scalar tail (0..7 rows).
    while i < n {
        let fv = f64::from_bits(col[i]);
        let set = match PRED {
            _CMP_EQ_OQ => fv == val,
            _CMP_LT_OQ => fv < val,
            _CMP_GT_OQ => fv > val,
            _CMP_LE_OQ => fv <= val,
            _CMP_GE_OQ => fv >= val,
            _ => false,
        };
        if set {
            *bytes.get_unchecked_mut(byte_idx) |= 1u8 << (i & 7);
        }
        i += 1;
    }
    bm
}

// ---- AVX-512BW combinators: bitmap ↔ bool mask ----

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn and_into_bool_avx512(bm: &Bitmap, mask: &mut [bool]) {
    use core::ptr;
    let n = mask.len();
    let bits = bm.as_bytes();
    let mut i = 0usize;
    // Process 64 bools (64 bytes of mask + 8 bytes of bitmap = 64 bits) per iter.
    while i + 64 <= n {
        let m = _mm512_loadu_epi8(mask.as_ptr().add(i) as *const i8);
        let packed: u64 = ptr::read_unaligned(bits.as_ptr().add(i / 8) as *const u64);
        // Expand 64 packed bits → 64 bytes of 0x00 / 0xFF via maskz-broadcast.
        // _mm512_maskz_set1_epi8(__mmask64, value) returns a __m512i where each
        // byte is `value` if the corresponding mask bit is set, else 0.
        let expanded = _mm512_maskz_set1_epi8(packed as __mmask64, 0xFFu8 as i8);
        // AND: bool (0x00/0x01) AND expanded (0x00/0xFF) = correct 0x00/0x01.
        let r = _mm512_and_si512(m, expanded);
        _mm512_storeu_epi8(mask.as_ptr().add(i) as *mut i8, r);
        i += 64;
    }
    // 8-bool tail: load 8 bytes, expand 8-bit mask, AND, store.
    while i + 8 <= n {
        let m8 = _mm_loadu_si64(mask.as_ptr().add(i) as *const u8);
        let byte = *bits.get_unchecked(i / 8);
        // _mm_maskz_set1_epi8(__mmask16, value): zero-masks a 16-byte broadcast.
        // We use the low 8 bits of the byte; only the low 8 bytes of the
        // result matter (we store 8 bytes via _mm_storeu_si64).
        let expanded = _mm_maskz_set1_epi8(byte as __mmask16, 0xFFu8 as i8);
        let r = _mm_and_si128(m8, expanded);
        _mm_storeu_si64(mask.as_ptr().add(i) as *mut u8, r);
        i += 8;
    }
    while i < n {
        if (bits[i >> 3] >> (i & 7)) & 1 == 0 {
            *mask.get_unchecked_mut(i) = false;
        }
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn or_into_bool_avx512(bm: &Bitmap, mask: &mut [bool]) {
    use core::ptr;
    let n = mask.len();
    let bits = bm.as_bytes();
    let one = _mm512_set1_epi8(1);
    let one128 = _mm_set1_epi8(1);
    let mut i = 0usize;
    while i + 64 <= n {
        let m = _mm512_loadu_epi8(mask.as_ptr().add(i) as *const i8);
        let packed: u64 = ptr::read_unaligned(bits.as_ptr().add(i / 8) as *const u64);
        let expanded = _mm512_maskz_set1_epi8(packed as __mmask64, 0xFFu8 as i8);
        // OR then collapse 0xFF → 0x01 via unsigned-byte min with 0x01.
        let ored = _mm512_or_si512(m, expanded);
        let collapsed = _mm512_min_epu8(ored, one);
        _mm512_storeu_epi8(mask.as_ptr().add(i) as *mut i8, collapsed);
        i += 64;
    }
    while i + 8 <= n {
        let m8 = _mm_loadu_si64(mask.as_ptr().add(i) as *const u8);
        let byte = *bits.get_unchecked(i / 8);
        let expanded = _mm_maskz_set1_epi8(byte as __mmask16, 0xFFu8 as i8);
        let ored = _mm_or_si128(m8, expanded);
        let collapsed = _mm_min_epu8(ored, one128);
        _mm_storeu_si64(mask.as_ptr().add(i) as *mut u8, collapsed);
        i += 8;
    }
    while i < n {
        if (bits[i >> 3] >> (i & 7)) & 1 != 0 {
            *mask.get_unchecked_mut(i) = true;
        }
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn and_inplace_avx512(dst: &mut [u8], src: &[u8]) {
    let n = dst.len();
    let mut i = 0usize;
    // 64-byte (512-bit) blocks — 4 cache lines, 1 vpandq + 1 store.
    while i + 64 <= n {
        let d = _mm512_loadu_epi8(dst.as_ptr().add(i) as *const i8);
        let s = _mm512_loadu_epi8(src.as_ptr().add(i) as *const i8);
        let r = _mm512_and_si512(d, s);
        _mm512_storeu_epi8(dst.as_mut_ptr().add(i) as *mut i8, r);
        i += 64;
    }
    // 16-byte tail.
    while i + 16 <= n {
        let d = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let r = _mm_and_si128(d, s);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, r);
        i += 16;
    }
    while i < n {
        *dst.get_unchecked_mut(i) &= *src.get_unchecked(i);
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn or_inplace_avx512(dst: &mut [u8], src: &[u8]) {
    let n = dst.len();
    let mut i = 0usize;
    while i + 64 <= n {
        let d = _mm512_loadu_epi8(dst.as_ptr().add(i) as *const i8);
        let s = _mm512_loadu_epi8(src.as_ptr().add(i) as *const i8);
        let r = _mm512_or_si512(d, s);
        _mm512_storeu_epi8(dst.as_mut_ptr().add(i) as *mut i8, r);
        i += 64;
    }
    while i + 16 <= n {
        let d = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let r = _mm_or_si128(d, s);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, r);
        i += 16;
    }
    while i < n {
        *dst.get_unchecked_mut(i) |= *src.get_unchecked(i);
        i += 1;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_bits(bm: &Bitmap, expected: &[bool]) {
        assert_eq!(bm.len(), expected.len());
        for (i, &e) in expected.iter().enumerate() {
            assert_eq!(bm.get(i), e, "bit {} mismatch: got {} expected {}", i, bm.get(i), e);
        }
    }

    #[test]
    fn test_bitmap_set_get() {
        let mut bm = Bitmap::new(20);
        bm.set(0);
        bm.set(7);
        bm.set(8);
        bm.set(19);
        assert!(bm.get(0));
        assert!(bm.get(7));
        assert!(bm.get(8));
        assert!(bm.get(19));
        assert!(!bm.get(1));
        assert!(!bm.get(18));
    }

    #[test]
    fn test_bitmap_and() {
        let mut a = Bitmap::new(16);
        let mut b = Bitmap::new(16);
        for i in [0, 1, 2, 3, 4] {
            a.set(i);
        }
        for i in [2, 3, 4, 5, 6] {
            b.set(i);
        }
        let c = a.and(&b);
        assert!(c.get(2) && c.get(3) && c.get(4));
        assert!(!c.get(0) && !c.get(1) && !c.get(5));
    }

    #[test]
    fn test_bitmap_count_ones() {
        let mut bm = Bitmap::new(32);
        for i in [0, 5, 7, 8, 15, 16, 31] {
            bm.set(i);
        }
        assert_eq!(bm.count_ones(), 7);
    }

    #[test]
    fn test_filter_eq_u64() {
        let col = vec![1u64, 5, 5, 3, 5, 7, 8, 5, 9, 10];
        let bm = filter_eq_u64(&col, 5);
        assert_bits(&bm, &[false, true, true, false, true, false, false, true, false, false]);
    }

    #[test]
    fn test_filter_lt_u64() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_lt_u64(&col, 50);
        assert_bits(&bm, &[true, true, true, true, false, false, false, false, false, false]);
    }

    #[test]
    fn test_filter_ge_u64() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_ge_u64(&col, 50);
        assert_bits(&bm, &[false, false, false, false, true, true, true, true, true, true]);
    }

    #[test]
    fn test_filter_le_u64() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_le_u64(&col, 50);
        assert_bits(&bm, &[true, true, true, true, true, false, false, false, false, false]);
    }

    #[test]
    fn test_filter_gt_u64() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_gt_u64(&col, 50);
        assert_bits(&bm, &[false, false, false, false, false, true, true, true, true, true]);
    }

    #[test]
    fn test_filter_ne_u64() {
        let col = vec![1u64, 5, 5, 3, 5, 7, 8, 5, 9, 10];
        let bm = filter_ne_u64(&col, 5);
        assert_bits(&bm, &[true, false, false, true, false, true, true, false, true, true]);
    }

    #[test]
    fn test_filter_ne_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.5, 2.5, 3.0, 2.5].iter().map(|f| f.to_bits()).collect();
        let bm = filter_ne_f64(&col, 2.5);
        assert_bits(&bm, &[true, false, false, true, false]);
    }

    #[test]
    fn test_filter_eq_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.5, 2.5, 3.0, 2.5].iter().map(|f| f.to_bits()).collect();
        let bm = filter_eq_f64(&col, 2.5);
        assert_bits(&bm, &[false, true, true, false, true]);
    }

    #[test]
    fn test_filter_lt_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
            .iter()
            .map(|f| f.to_bits())
            .collect();
        let bm = filter_lt_f64(&col, 5.0);
        assert_bits(&bm, &[true, true, true, true, false, false, false, false, false, false]);
    }

    #[test]
    fn test_filter_le_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
            .iter()
            .map(|f| f.to_bits())
            .collect();
        let bm = filter_le_f64(&col, 5.0);
        assert_bits(&bm, &[true, true, true, true, true, false, false, false, false, false]);
    }

    #[test]
    fn test_filter_ge_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
            .iter()
            .map(|f| f.to_bits())
            .collect();
        let bm = filter_ge_f64(&col, 5.0);
        assert_bits(&bm, &[false, false, false, false, true, true, true, true, true, true]);
    }

    #[test]
    fn test_filter_gt_f64() {
        let col: Vec<u64> = vec![1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
            .iter()
            .map(|f| f.to_bits())
            .collect();
        let bm = filter_gt_f64(&col, 5.0);
        assert_bits(&bm, &[false, false, false, false, false, true, true, true, true, true]);
    }

    #[test]
    fn test_filter_lt_i64_signed() {
        // Negative numbers — signed compare.
        let col: Vec<u64> = vec![-5i64, -3, -1, 0, 1, 3, 5].iter().map(|x| *x as u64).collect();
        let bm = filter_lt_i64(&col, 0);
        assert_bits(&bm, &[true, true, true, false, false, false, false]);
    }

    #[test]
    fn test_filter_ge_i64_signed() {
        let col: Vec<u64> = vec![-5i64, -3, -1, 0, 1, 3, 5].iter().map(|x| *x as u64).collect();
        let bm = filter_ge_i64(&col, 0);
        assert_bits(&bm, &[false, false, false, true, true, true, true]);
    }

    #[test]
    fn test_and_into_bool() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_ge_u64(&col, 50);
        let mut mask = vec![true; 10];
        mask[2] = false;
        and_into_bool(&bm, &mut mask);
        assert_eq!(mask, vec![false, false, false, false, true, true, true, true, true, true]);
    }

    #[test]
    fn test_or_into_bool() {
        let col = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let bm = filter_ge_u64(&col, 50);
        let mut mask = vec![false; 10];
        mask[2] = true;
        or_into_bool(&bm, &mut mask);
        assert_eq!(mask, vec![false, false, true, false, true, true, true, true, true, true]);
    }

    #[test]
    fn test_large_filter_u64() {
        let n = 1_000_000usize;
        let col: Vec<u64> = (0..n as u64).map(|i| i % 100).collect();
        let bm = filter_eq_u64(&col, 50);
        assert_eq!(bm.count_ones(), 10_000);
    }

    #[test]
    fn test_large_filter_f64() {
        let n = 1_000_000usize;
        let col: Vec<u64> =
            (0..n as u64).map(|i| (i as f64) * 0.001).map(|f| f.to_bits()).collect();
        let bm = filter_lt_f64(&col, 50.0);
        assert_eq!(bm.count_ones(), 50_000);
    }

    #[test]
    fn test_large_and_into_bool() {
        let n = 1_000_000usize;
        let col: Vec<u64> = (0..n as u64).map(|i| i % 100).collect();
        let bm1 = filter_ge_u64(&col, 10);
        let bm2 = filter_le_u64(&col, 50);
        let combined = bm1.and(&bm2);
        let mut mask = vec![true; n];
        and_into_bool(&combined, &mut mask);
        let count = mask.iter().filter(|&&b| b).count();
        // rows where 10 <= i%100 <= 50 → 41 values per 100, 10000 cycles → 410000
        assert_eq!(count, 41 * 10_000);
    }

    #[test]
    fn test_to_bool_vec_roundtrip() {
        let mut bm = Bitmap::new(20);
        for i in [0, 3, 7, 15, 19] {
            bm.set(i);
        }
        let bools = bm.to_bool_vec();
        let bm2 = Bitmap::from_bool_slice(&bools);
        for i in 0..20 {
            assert_eq!(bm.get(i), bm2.get(i), "bit {} mismatch", i);
        }
    }

    #[test]
    fn test_not() {
        let mut bm = Bitmap::new(10);
        for i in [0, 3, 7] {
            bm.set(i);
        }
        let n = bm.not();
        for i in 0..10 {
            assert_eq!(n.get(i), !bm.get(i), "bit {} mismatch", i);
        }
        // count_ones of NOT == len - count_ones(self)
        assert_eq!(n.count_ones(), 10 - bm.count_ones());
    }

    #[test]
    fn test_all_ones_count() {
        let bm = Bitmap::all_ones(20);
        assert_eq!(bm.count_ones(), 20);
        let bm2 = Bitmap::all_ones(64);
        assert_eq!(bm2.count_ones(), 64);
        let bm3 = Bitmap::all_ones(17);
        assert_eq!(bm3.count_ones(), 17);
    }

    // === W5A-T1 tests ===

    #[test]
    fn test_iter_set_bits_dense() {
        let mut bm = Bitmap::all_ones(20);
        let bits: Vec<usize> = bm.iter_set_bits().collect();
        assert_eq!(bits, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn test_iter_set_bits_sparse() {
        let mut bm = Bitmap::new(100);
        bm.set(5);
        bm.set(31);
        bm.set(63);
        bm.set(99);
        let bits: Vec<usize> = bm.iter_set_bits().collect();
        assert_eq!(bits, vec![5, 31, 63, 99]);
    }

    #[test]
    fn test_iter_set_bits_empty() {
        let bm = Bitmap::new(50);
        let bits: Vec<usize> = bm.iter_set_bits().collect();
        assert!(bits.is_empty());
    }

    #[test]
    fn test_iter_set_bits_skips_zero_bytes() {
        // 32-bit bitmap with bytes [0x00, 0x00, 0x00, 0x01] — only bit 24 set.
        let mut bm = Bitmap::new(32);
        bm.set(24);
        let bits: Vec<usize> = bm.iter_set_bits().collect();
        assert_eq!(bits, vec![24]);
    }

    #[test]
    fn test_get_batch() {
        let mut bm = Bitmap::new(20);
        bm.set(0);
        bm.set(3);
        bm.set(7);
        bm.set(15);
        let result = bm.get_batch(&[0, 1, 3, 7, 15, 19]);
        assert_eq!(result, vec![true, false, true, true, true, false]);
    }

    #[test]
    fn test_count_ones_range() {
        let mut bm = Bitmap::new(100);
        for i in 10..50 {
            bm.set(i);
        }
        assert_eq!(bm.count_ones_range(0, 100), 40);
        assert_eq!(bm.count_ones_range(10, 50), 40);
        assert_eq!(bm.count_ones_range(0, 10), 0);
        assert_eq!(bm.count_ones_range(50, 100), 0);
        assert_eq!(bm.count_ones_range(20, 30), 10);
        assert_eq!(bm.count_ones_range(15, 45), 30);
    }

}
