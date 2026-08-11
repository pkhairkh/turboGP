//! A 4 KB page — the fundamental I/O unit.
//!
//! A page is 4096 bytes = 64 cache lines = 512 u64 cells. The first 64 bytes
//! (1 cache line) is the header; the remaining 4032 bytes hold 504 cells.
//!
//! The page size is chosen because:
//! - 4 KB matches the OS page size and x86 TLB granularity
//! - 4 KB = 64×64-byte cache lines
//! - Scanning a 4 KB page with `VPCMPEQQ` takes ~64 cycles, fitting in L1
//!
//! ## Integrity (ADR-012)
//!
//! Each page carries two integrity fields in its header:
//!
//! - **`checksum`** — a CRC32C (Castagnoli, poly 0x1EDC6F41) of the cell
//!   payload. On x86-64 with SSE4.2 this is computed in hardware by
//!   `_mm_crc32_u64` at ~30 GB/s. Detects any odd number of bit flips.
//! - **`parity`** — the XOR of all 8-byte words in the cell payload. When the
//!   CRC mismatches, a single-bit corruption produces a syndrome (stored
//!   parity XOR recomputed parity) with exactly one bit set, identifying the
//!   corrupted bit position. [`Page::verify_and_correct`] then scans each
//!   8-byte word, flips that bit, and accepts the correction only if the CRC
//!   also recovers.
//!
//! This gives single-bit error correction with < 0.2% space overhead — the
//! dominant failure mode for DRAM and SSD bit-rot.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Page size: 4096 bytes.
pub const PAGE_SIZE: usize = 4096;

/// Header size: 64 bytes (1 cache line).
pub const HEADER_SIZE: usize = 64;

/// Number of u64 cells per page: (4096 - 64) / 8 = 504.
pub const PAGE_CELLS: usize = (PAGE_SIZE - HEADER_SIZE) / 8;

/// CRC32C (Castagnoli) polynomial in reversed (LSB-first) form.
///
/// The forward polynomial is `0x1EDC6F41`; bit-reversed (so it can be used
/// with the right-shift / XOR-low-bit algorithm and the SSE4.2 hardware
/// instruction) it becomes `0x82F63B78`.
pub const CRC32C_POLY_REVERSED: u32 = 0x82F63B78;

// ---------------------------------------------------------------------------
// CRC32C — hardware (SSE4.2) + scalar fallback
// ---------------------------------------------------------------------------

/// Compute CRC32C (Castagnoli) of `cells`.
///
/// On x86-64 with SSE4.2 this dispatches to `_mm_crc32_u64`, which runs at
/// ~30 GB/s. Otherwise it falls back to a branchless bit-by-bit loop.
///
/// The CRC32C convention matches the iSCSI / ext4 / btrfs / NVMe T10 PI
/// definition: initial value `0xFFFFFFFF`, final XOR with `0xFFFFFFFF`.
pub fn compute_crc32c(cells: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `is_x86_feature_detected!` is the runtime guard; the
        // intrinsic is only called inside the branch where SSE4.2 is
        // confirmed present.
        if is_x86_feature_detected!("sse4.2") {
            return unsafe { crc32c_sse42(cells) };
        }
    }
    crc32c_scalar(cells)
}

/// Hardware-accelerated CRC32C using SSE4.2 `_mm_crc32_u64`.
///
/// Processes 8 bytes per instruction; tail bytes are handled with the smaller
/// `_mm_crc32_u32` / `_mm_crc32_u16` / `_mm_crc32_u8` intrinsics.
#[cfg(target_arch = "x86_64")]
unsafe fn crc32c_sse42(cells: &[u8]) -> u32 {
    use std::arch::x86_64::*;

    // Initial CRC value per the CRC32C convention (0xFFFFFFFF). The
    // `_mm_crc32_u64` intrinsic reads the low 32 bits of its first argument;
    // the high 32 bits are ignored. We keep `crc` as `u64` to match the
    // intrinsic's signature.
    let mut crc: u64 = 0xFFFF_FFFF;

    let mut chunks = cells.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        // SAFETY: caller (via `is_x86_feature_detected!("sse4.2")`) has
        // verified SSE4.2 is present.
        crc = unsafe { _mm_crc32_u64(crc, word) };
    }

    // Tail — process remaining bytes with smaller-width intrinsics so the
    // result matches the scalar reference exactly. The narrower intrinsics
    // take a `u32` crc argument; the high bits of `crc` are zero at this
    // point (the hardware only ever sets the low 32 bits), so the truncation
    // is lossless.
    let mut crc32 = crc as u32;
    let mut tail = chunks.remainder();
    while tail.len() >= 4 {
        let word = u32::from_le_bytes(tail[..4].try_into().unwrap());
        // SAFETY: SSE4.2 verified by caller.
        crc32 = unsafe { _mm_crc32_u32(crc32, word) };
        tail = &tail[4..];
    }
    while tail.len() >= 2 {
        let word = u16::from_le_bytes(tail[..2].try_into().unwrap());
        // SAFETY: SSE4.2 verified by caller.
        crc32 = unsafe { _mm_crc32_u16(crc32, word) };
        tail = &tail[2..];
    }
    if !tail.is_empty() {
        // SAFETY: SSE4.2 verified by caller.
        crc32 = unsafe { _mm_crc32_u8(crc32, tail[0]) };
    }

    // Final XOR per convention.
    crc32 ^ 0xFFFF_FFFF
}

/// Scalar fallback CRC32C — branchless bit-by-bit.
///
/// Used when SSE4.2 is unavailable (older CPUs, non-x86 targets, or when
/// `is_x86_feature_detected!("sse4.2")` returns false). Slower than the
/// hardware path but produces bit-identical results.
fn crc32c_scalar(cells: &[u8]) -> u32 {
    let poly = CRC32C_POLY_REVERSED;
    let mut crc: u32 = 0xFFFF_FFFF;

    for &b in cells {
        crc ^= b as u32;
        // Eight right-shift / conditional-XOR rounds, branchless. The mask
        // is `0xFFFFFFFF` when the low bit is set, `0` otherwise.
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (poly & mask);
        }
    }

    crc ^ 0xFFFF_FFFF
}

/// Compute the per-page XOR parity: the XOR of all 8-byte words in `cells`.
///
/// If a single bit is flipped anywhere in the payload, exactly the
/// corresponding bit in the parity flips — which is what makes single-bit
/// error correction possible (see [`Page::verify_and_correct`]).
pub fn compute_parity(cells: &[u8]) -> u64 {
    let mut parity: u64 = 0;
    for chunk in cells.chunks_exact(8) {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        parity ^= word;
    }
    // Tail bytes (if the length isn't a multiple of 8): XOR into the low
    // bytes of `parity`. The page's cell payload is always 4032 bytes
    // (504 × 8), so this branch is not exercised on a Page, but the
    // function is general-purpose.
    let tail = cells.chunks_exact(8).remainder();
    if !tail.is_empty() {
        let mut word_bytes = [0u8; 8];
        word_bytes[..tail.len()].copy_from_slice(tail);
        parity ^= u64::from_le_bytes(word_bytes);
    }
    parity
}

// ---------------------------------------------------------------------------
// PageHeader
// ---------------------------------------------------------------------------

/// Page header — 64 bytes, exactly one cache line.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, Serialize, Deserialize, Default)]
pub struct PageHeader {
    /// Page type tag (which kernel operates on this page).
    pub page_type: u64,
    /// Tier hint (which memory tier this page prefers).
    pub tier_hint: u64,
    /// Homogeneity mask (which cell tags are present).
    pub homogeneity: u64,
    /// Number of valid cells in this page.
    pub row_count: u64,
    /// CRC32C of the cell data (stored zero-extended to 64 bits).
    pub checksum: u64,
    /// Predecessor page ID (for LSM chains).
    pub predecessor: u64,
    /// Successor page ID.
    pub successor: u64,
    /// XOR parity of all 8-byte words in the cell data (ADR-012).
    pub parity: u64,
}

impl PageHeader {
    /// Size of the header in bytes.
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Compute the CRC32C checksum of the cell data, returned as a `u64`
    /// (zero-extended) for in-header storage.
    ///
    /// Kept for backward compatibility with code that expects a `u64`
    /// checksum; the actual checksum is 32 bits and lives in the low bits
    /// of the returned value.
    pub fn compute_checksum(cells: &[u8]) -> u64 {
        compute_crc32c(cells) as u64
    }

    /// Compute the XOR parity of the cell data.
    pub fn compute_parity(cells: &[u8]) -> u64 {
        compute_parity(cells)
    }
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// A 4 KB page.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct Page {
    /// The header (64 bytes).
    pub header: PageHeader,
    /// The cell data (4032 bytes = 504 u64 cells).
    pub cells: [u8; PAGE_SIZE - HEADER_SIZE],
}

impl Page {
    /// Allocate a new zeroed page.
    pub fn new() -> Self {
        Self { header: PageHeader::default(), cells: [0u8; PAGE_SIZE - HEADER_SIZE] }
    }

    /// Get a cell as a u64.
    pub fn get_cell(&self, index: usize) -> u64 {
        assert!(index < PAGE_CELLS, "cell index {} out of range", index);
        let offset = index * 8;
        u64::from_le_bytes(self.cells[offset..offset + 8].try_into().unwrap())
    }

    /// Set a cell as a u64.
    pub fn set_cell(&mut self, index: usize, value: u64) {
        assert!(index < PAGE_CELLS, "cell index {} out of range", index);
        let offset = index * 8;
        self.cells[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// Get the cells as a slice of u64s.
    pub fn as_u64_slice(&self) -> &[u64] {
        let ptr = self.cells.as_ptr() as *const u64;
        // SAFETY: `cells` is `[u8; 4032]`, which is exactly 504 × 8 bytes and
        // 8-byte aligned within the cache-line-aligned `Page`. The slice
        // length matches the cell count.
        unsafe { std::slice::from_raw_parts(ptr, PAGE_CELLS) }
    }

    /// Get the cells as a mutable slice of u64s.
    pub fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let ptr = self.cells.as_mut_ptr() as *mut u64;
        // SAFETY: as above; the page is `repr(C, align(64))` so the cells
        // are 8-byte aligned and the borrow checker guards `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(ptr, PAGE_CELLS) }
    }

    /// Verify the page's CRC32C checksum against the stored value.
    ///
    /// Recomputes [`compute_crc32c`] over the cell payload and compares the
    /// low 32 bits of `header.checksum`. Returns `true` if they match.
    pub fn verify_checksum(&self) -> bool {
        let computed = compute_crc32c(&self.cells) as u64;
        computed == self.header.checksum
    }

    /// Recompute and store both the CRC32C checksum and the XOR parity.
    ///
    /// After writing cells, call this to refresh the header's integrity
    /// fields. Both fields must be kept in sync for
    /// [`verify_and_correct`](Self::verify_and_correct) to work.
    pub fn update_checksum(&mut self) {
        self.header.checksum = compute_crc32c(&self.cells) as u64;
        self.header.parity = compute_parity(&self.cells);
    }

    /// Detect and (if possible) correct a single-bit corruption in the cell
    /// payload, using the stored CRC32C for detection and the stored XOR
    /// parity for correction (ADR-012).
    ///
    /// Returns:
    /// - `Ok(false)` — no error; the stored CRC already matches the cells.
    /// - `Ok(true)`  — a single-bit corruption was identified and corrected
    ///   in place; the CRC now matches.
    /// - `Err(msg)`  — the corruption is uncorrectable (multi-bit syndrome,
    ///   or a single-bit syndrome that no word flip can repair, e.g. when
    ///   the stored CRC itself is the corrupted field).
    pub fn verify_and_correct(&mut self) -> Result<bool, String> {
        // Fast path: CRC matches, nothing to do.
        if self.verify_checksum() {
            return Ok(false);
        }

        let stored_parity = self.header.parity;
        let computed_parity = compute_parity(&self.cells);
        let syndrome = stored_parity ^ computed_parity;

        // A single-bit corruption in any 8-byte word flips exactly one bit
        // position in the parity. If the syndrome has more (or fewer) than
        // one bit set, the corruption is multi-bit (or the parity field
        // itself was corrupted) and we cannot correct it.
        if syndrome.count_ones() != 1 {
            return Err(format!(
                "uncorrectable corruption: syndrome = {:#018x} ({} bits set, expected 1)",
                syndrome,
                syndrome.count_ones()
            ));
        }

        // The single set bit in the syndrome tells us the bit position
        // *within* an 8-byte word. We still need to find which word was
        // corrupted — try flipping that bit in each word and accept the
        // correction only if the CRC also recovers.
        let bit_pos = syndrome.trailing_zeros() as usize; // 0..64
        let byte_in_word = bit_pos / 8; // 0..8
        let bit_in_byte = bit_pos % 8; // 0..8
        let flip_mask = 1u8 << bit_in_byte;

        let num_words = self.cells.len() / 8;
        for word_idx in 0..num_words {
            let byte_idx = word_idx * 8 + byte_in_word;
            self.cells[byte_idx] ^= flip_mask;
            if self.verify_checksum() {
                // The CRC now matches; the parity also matches because the
                // correction undid exactly the bit the syndrome pointed at.
                return Ok(true);
            }
            // Undo this candidate flip and try the next word.
            self.cells[byte_idx] ^= flip_mask;
        }

        Err(format!(
            "uncorrectable corruption: syndrome = {:#018x} indicates a single bit, \
             but no word flip repaired the CRC (stored CRC may itself be corrupt)",
            syndrome
        ))
    }

    /// Write the page to a byte slice (for serialization).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PAGE_SIZE);
        out.extend_from_slice(bytemuck::bytes_of(&self.header));
        out.extend_from_slice(&self.cells);
        out
    }

    /// Write the page to a byte slice, computing the checksum first
    /// (Task 3.4). This is the "safe" write path: the returned bytes
    /// always carry a valid CRC32C checksum, so a subsequent `read()`
    /// will pass verification.
    pub fn write(&mut self) -> Vec<u8> {
        self.update_checksum();
        self.to_bytes()
    }

    /// Read a page from a byte slice.
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() < PAGE_SIZE {
            return Err(crate::Error::Corruption(format!("page too small: {} bytes", bytes.len())));
        }
        let header: PageHeader = *bytemuck::from_bytes(&bytes[..HEADER_SIZE]);
        let mut cells = [0u8; PAGE_SIZE - HEADER_SIZE];
        cells.copy_from_slice(&bytes[HEADER_SIZE..PAGE_SIZE]);
        Ok(Self { header, cells })
    }

    /// Read a page from a byte slice AND verify its CRC32C checksum
    /// (Task 3.4). Returns `Err(Corruption)` if the checksum doesn't
    /// match — this detects torn writes and bit-rot. Use this instead
    /// of `from_bytes` when reading from disk to catch corruption early.
    ///
    /// A page with a zero checksum (never `update_checksum`-ed) is
    /// accepted only if all cells are also zero (a freshly-allocated
    /// page); otherwise the mismatch is reported as corruption.
    pub fn read(bytes: &[u8]) -> crate::Result<Self> {
        let page = Self::from_bytes(bytes)?;
        if !page.verify_checksum() {
            // Allow the all-zero page (freshly allocated, never written).
            let all_zero = page.cells.iter().all(|&b| b == 0) && page.header.checksum == 0;
            if !all_zero {
                return Err(crate::Error::Corruption(format!(
                    "page checksum mismatch: stored {:#018x}, computed {:#018x}",
                    page.header.checksum,
                    compute_crc32c(&page.cells) as u64
                )));
            }
        }
        Ok(page)
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_4kb() {
        assert_eq!(PAGE_SIZE, 4096);
    }

    #[test]
    fn page_header_is_64_bytes() {
        assert_eq!(HEADER_SIZE, 64);
        assert_eq!(PageHeader::SIZE, 64);
    }

    #[test]
    fn page_cells_is_504() {
        assert_eq!(PAGE_CELLS, 504);
        assert_eq!((PAGE_SIZE - HEADER_SIZE) / 8, 504);
    }

    #[test]
    fn page_get_set_cell() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(1, 0xDEADBEEF);
        p.set_cell(503, u64::MAX);
        assert_eq!(p.get_cell(0), 42);
        assert_eq!(p.get_cell(1), 0xDEADBEEF);
        assert_eq!(p.get_cell(503), u64::MAX);
    }

    #[test]
    fn page_checksum_roundtrip() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(1, 99);
        p.update_checksum();
        assert!(p.verify_checksum());
        // Tamper with a cell.
        p.set_cell(0, 100);
        assert!(!p.verify_checksum());
    }

    #[test]
    fn page_to_from_bytes_roundtrip() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(100, 12345);
        p.header.row_count = 101;
        p.update_checksum();
        let bytes = p.to_bytes();
        let p2 = Page::from_bytes(&bytes).unwrap();
        assert_eq!(p2.get_cell(0), 42);
        assert_eq!(p2.get_cell(100), 12345);
        assert_eq!(p2.header.row_count, 101);
        assert!(p2.verify_checksum());
    }

    #[test]
    fn page_as_u64_slice() {
        let mut p = Page::new();
        p.set_cell(0, 10);
        p.set_cell(1, 20);
        p.set_cell(2, 30);
        let slice = p.as_u64_slice();
        assert_eq!(slice.len(), PAGE_CELLS);
        assert_eq!(slice[0], 10);
        assert_eq!(slice[1], 20);
        assert_eq!(slice[2], 30);
    }

    // -----------------------------------------------------------------------
    // New ADR-012 tests (Wave 2)
    // -----------------------------------------------------------------------

    /// Test 1: write cells, compute CRC32C, verify it matches.
    #[test]
    fn page_crc32c_roundtrip() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
        let crc = compute_crc32c(&p.cells);
        // Manually store it in the header and confirm verify agrees.
        p.header.checksum = crc as u64;
        assert!(p.verify_checksum());

        // The CRC must be 32 bits — verify it round-trips through a u32 cast.
        assert_eq!(p.header.checksum as u32, crc);
    }

    /// Test 2: corrupt one bit, verify_checksum returns false.
    #[test]
    fn page_crc32c_detects_single_bit_corruption() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, (i as u64).wrapping_mul(31));
        }
        p.update_checksum();
        assert!(p.verify_checksum());

        // Flip a single bit somewhere in the middle of the payload.
        p.cells[1234] ^= 1 << 5;
        assert!(!p.verify_checksum());
    }

    /// Test 3: verify_and_correct fixes a single-bit error.
    #[test]
    fn page_verify_and_correct_fixes_single_bit_error() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, (i as u64).wrapping_mul(0x100_0000_0168));
        }
        p.update_checksum();
        assert!(p.verify_checksum());

        // Corrupt a single bit in cell #250 (byte offset 2000).
        let byte_idx = 250 * 8 + 3;
        let bit_idx = 4;
        p.cells[byte_idx] ^= 1u8 << bit_idx;

        // Detection fires.
        assert!(!p.verify_checksum());

        // Correction must succeed.
        let corrected = p.verify_and_correct().expect("single-bit error should be correctable");
        assert!(corrected, "verify_and_correct should report a correction was made");

        // The page now verifies cleanly.
        assert!(p.verify_checksum());
        // The parity field is back in sync (no need to call update_checksum).
        assert_eq!(p.header.parity, compute_parity(&p.cells));
    }

    /// Test 4: corrupt two bits, verify_and_correct returns Err.
    #[test]
    fn page_verify_and_correct_fails_on_double_bit_error() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, i as u64);
        }
        p.update_checksum();
        assert!(p.verify_checksum());

        // Flip two bits in two different words (so the syndrome has 2 bits set
        // and is therefore uncorrectable).
        p.cells[100] ^= 0b0000_0001;
        p.cells[200] ^= 0b1000_0000;

        assert!(!p.verify_checksum());
        let result = p.verify_and_correct();
        assert!(result.is_err(), "double-bit error must be uncorrectable");
    }

    /// Test 5: verify_and_correct returns Ok(false) when there's no error.
    #[test]
    fn page_verify_and_correct_returns_false_when_clean() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(1, 99);
        p.update_checksum();
        let result = p.verify_and_correct().expect("clean page should not error");
        assert!(!result, "no correction should be reported for a clean page");
    }

    /// Test 6: XOR parity for known values.
    ///
    /// Writes a small set of known 8-byte words and verifies the parity is
    /// their XOR. This catches off-by-one / endianness bugs in
    /// `compute_parity`.
    #[test]
    fn page_parity_correct_for_known_values() {
        let mut p = Page::new();
        // Cells 0..2 = known words; everything else stays zero (XOR identity).
        p.set_cell(0, 0x0102_0304_0506_0708);
        p.set_cell(1, 0x1111_1111_1111_1111);
        p.set_cell(2, 0xFFFF_FFFF_FFFF_FFFF);
        // Cell 3 is left at zero — XOR-ing zero is the identity, which is
        // exactly the property we want to confirm implicitly via the parity.
        let expected: u64 = 0x0102_0304_0506_0708 ^ 0x1111_1111_1111_1111 ^ 0xFFFF_FFFF_FFFF_FFFF;
        let actual = compute_parity(&p.cells);
        assert_eq!(actual, expected);

        // The header's compute_parity helper must agree.
        assert_eq!(PageHeader::compute_parity(&p.cells), expected);
    }

    /// Test 7: CRC32C of empty input matches the convention (0xFFFFFFFF XOR
    /// 0xFFFFFFFF = 0). Catches regressions in the initial-value / final-XOR
    /// handling.
    #[test]
    fn crc32c_empty_input_is_zero() {
        assert_eq!(compute_crc32c(&[]), 0);
    }

    /// Test 8: the hardware (SSE4.2) and scalar CRC32C paths must agree on a
    /// non-trivial input. On non-x86_64 targets this collapses to a tautology
    /// (only the scalar path exists), so the test still passes.
    #[test]
    fn crc32c_hardware_matches_scalar() {
        let input: Vec<u8> = (0..4032).map(|i| (i ^ (i >> 3)) as u8).collect();

        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("sse4.2") {
                // SAFETY: SSE4.2 was just detected.
                let hw = unsafe { crc32c_sse42(&input) };
                let sw = crc32c_scalar(&input);
                assert_eq!(hw, sw, "hardware CRC32C must match scalar fallback");
            }
        }

        // Always check the public API matches the scalar reference. This
        // covers non-x86 targets where `compute_crc32c` is the scalar path.
        let via_api = compute_crc32c(&input);
        let via_scalar = crc32c_scalar(&input);
        assert_eq!(via_api, via_scalar);
    }

    /// Test 9: CRC32C matches a known reference value.
    ///
    /// The CRC32C of the ASCII string "123456789" is the canonical check
    /// value for the Castagnoli polynomial: `0xE3069283`. This is the same
    /// test vector used by the Linux kernel, zlib, and the iSCSI RFC.
    #[test]
    fn crc32c_known_check_value() {
        let input = b"123456789";
        assert_eq!(compute_crc32c(input), 0xE306_9283);
    }

    /// Test 10: parity of two identical words cancels (XOR identity).
    #[test]
    fn parity_of_repeated_word_cancels() {
        let mut p = Page::new();
        let v = 0xA5A5_A5A5_A5A5_A5A5;
        // 504 cells, all the same value — parity is `v` if there's an odd
        // number of cells (504 is even, so parity = 0).
        for i in 0..PAGE_CELLS {
            p.set_cell(i, v);
        }
        assert_eq!(compute_parity(&p.cells), 0);

        // Flip one cell — parity becomes v (one odd contribution).
        p.set_cell(0, 0);
        assert_eq!(compute_parity(&p.cells), v);
    }

    // -----------------------------------------------------------------------
    // Task 3.4: Page::write() / Page::read() with automatic checksum
    // -----------------------------------------------------------------------

    /// Task 3.4 DoD: write() computes the checksum; read() verifies it.
    #[test]
    fn page_write_read_roundtrip() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
        p.header.row_count = PAGE_CELLS as u64;
        let bytes = p.write();
        // read() must succeed (checksum matches).
        let p2 = Page::read(&bytes).expect("read must succeed after write");
        assert_eq!(p2.header.row_count, PAGE_CELLS as u64);
        assert_eq!(p2.get_cell(0), p.get_cell(0));
        assert_eq!(p2.get_cell(100), p.get_cell(100));
    }

    /// Task 3.4 DoD: read() detects a torn page (corrupted cell data).
    #[test]
    fn page_read_detects_torn_page() {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, (i as u64).wrapping_mul(31));
        }
        let mut bytes = p.write();
        // Corrupt a byte in the cell payload (after the header).
        bytes[HEADER_SIZE + 100] ^= 0xFF;
        // read() must return a Corruption error.
        let result = Page::read(&bytes);
        assert!(result.is_err(), "read must reject a corrupted page");
    }

    /// Task 3.4 DoD: read() accepts a freshly-allocated all-zero page.
    #[test]
    fn page_read_accepts_zero_page() {
        let p = Page::new();
        let bytes = p.to_bytes();
        // read() must succeed (all-zero page is allowed).
        let _ = Page::read(&bytes).expect("read must accept an all-zero page");
    }
}
