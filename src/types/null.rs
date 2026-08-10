//! NULL bitmap + TriBool 3-valued logic.
//!
//! Research: SQL 3-valued logic (Kleene). Quantum info insight: could
//! pack 3-valued logic into 2 bits (superdense coding), but 1 bit per
//! row + separate TriBool is simpler and cache-friendly.

use std::fmt;

#[derive(Debug, Clone, Default)]
pub struct NullBitmap {
    bits: Vec<u64>,
    len: usize,
}

impl NullBitmap {
    pub fn new_none_null(len: usize) -> Self {
        NullBitmap { bits: vec![0u64; len.div_ceil(64)], len }
    }

    pub fn new_all_null(len: usize) -> Self {
        let words = len.div_ceil(64);
        let mut bits = vec![u64::MAX; words];
        if !bits.is_empty() && len % 64 != 0 {
            let last = bits.len() - 1;
            let mask = (1u64 << (len % 64)) - 1;
            bits[last] = mask;
        }
        NullBitmap { bits, len }
    }

    pub fn set_null(&mut self, i: usize) {
        if i < self.len {
            self.bits[i / 64] |= 1u64 << (i % 64);
        }
    }

    pub fn clear_null(&mut self, i: usize) {
        if i < self.len {
            self.bits[i / 64] &= !(1u64 << (i % 64));
        }
    }

    pub fn is_null(&self, i: usize) -> bool {
        if i >= self.len {
            return false;
        }
        (self.bits[i / 64] >> (i % 64)) & 1 == 1
    }

    pub fn count_nulls(&self) -> usize {
        let full = self.len / 64;
        let mut count: u64 = self.bits[..full].iter().map(|w| w.count_ones() as u64).sum();
        let rem = self.len % 64;
        if rem > 0 && full < self.bits.len() {
            count += (self.bits[full] & ((1u64 << rem) - 1)).count_ones() as u64;
        }
        count as usize
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriBool {
    True,
    False,
    Null,
}

impl TriBool {
    pub fn passes_where(&self) -> bool {
        matches!(self, TriBool::True)
    }

    pub fn not(self) -> TriBool {
        match self {
            TriBool::True => TriBool::False,
            TriBool::False => TriBool::True,
            TriBool::Null => TriBool::Null,
        }
    }

    pub fn and(self, other: TriBool) -> TriBool {
        if matches!(self, TriBool::False) || matches!(other, TriBool::False) {
            return TriBool::False;
        }
        if matches!(self, TriBool::Null) || matches!(other, TriBool::Null) {
            return TriBool::Null;
        }
        TriBool::True
    }

    pub fn or(self, other: TriBool) -> TriBool {
        if matches!(self, TriBool::True) || matches!(other, TriBool::True) {
            return TriBool::True;
        }
        if matches!(self, TriBool::Null) || matches!(other, TriBool::Null) {
            return TriBool::Null;
        }
        TriBool::False
    }

    pub fn eq_with_null<T: PartialEq>(a: Option<&T>, b: Option<&T>) -> TriBool {
        match (a, b) {
            (Some(x), Some(y)) => {
                if x == y {
                    TriBool::True
                } else {
                    TriBool::False
                }
            }
            _ => TriBool::Null,
        }
    }

    pub fn is_null<T>(a: Option<&T>) -> TriBool {
        if a.is_none() {
            TriBool::True
        } else {
            TriBool::False
        }
    }

    pub fn is_not_null<T>(a: Option<&T>) -> TriBool {
        Self::is_null(a).not()
    }

    pub fn is_distinct_from<T: PartialEq>(a: Option<&T>, b: Option<&T>) -> TriBool {
        match (a, b) {
            (Some(x), Some(y)) => {
                if x != y {
                    TriBool::True
                } else {
                    TriBool::False
                }
            }
            (None, None) => TriBool::False,
            _ => TriBool::True,
        }
    }
}

impl fmt::Display for TriBool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriBool::True => write!(f, "TRUE"),
            TriBool::False => write!(f, "FALSE"),
            TriBool::Null => write!(f, "NULL"),
        }
    }
}

impl From<bool> for TriBool {
    fn from(b: bool) -> Self {
        if b {
            TriBool::True
        } else {
            TriBool::False
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_new_none_null() {
        let bm = NullBitmap::new_none_null(100);
        for i in 0..100 {
            assert!(!bm.is_null(i));
        }
        assert_eq!(bm.count_nulls(), 0);
    }

    #[test]
    fn bitmap_new_all_null() {
        let bm = NullBitmap::new_all_null(100);
        for i in 0..100 {
            assert!(bm.is_null(i));
        }
        assert_eq!(bm.count_nulls(), 100);
    }

    #[test]
    fn bitmap_set_clear() {
        let mut bm = NullBitmap::new_none_null(10);
        bm.set_null(3);
        bm.set_null(7);
        assert!(bm.is_null(3));
        assert!(bm.is_null(7));
        assert_eq!(bm.count_nulls(), 2);
        bm.clear_null(3);
        assert!(!bm.is_null(3));
        assert_eq!(bm.count_nulls(), 1);
    }

    #[test]
    fn bitmap_out_of_bounds() {
        let bm = NullBitmap::new_none_null(5);
        assert!(!bm.is_null(999));
    }

    #[test]
    fn bitmap_word_boundary() {
        let mut bm = NullBitmap::new_none_null(130);
        for i in [0, 63, 64, 127, 128, 129] {
            bm.set_null(i);
        }
        assert_eq!(bm.count_nulls(), 6);
    }

    #[test]
    fn tribool_not() {
        assert_eq!(TriBool::True.not(), TriBool::False);
        assert_eq!(TriBool::False.not(), TriBool::True);
        assert_eq!(TriBool::Null.not(), TriBool::Null);
    }

    #[test]
    fn tribool_and() {
        assert_eq!(TriBool::True.and(TriBool::True), TriBool::True);
        assert_eq!(TriBool::True.and(TriBool::False), TriBool::False);
        assert_eq!(TriBool::True.and(TriBool::Null), TriBool::Null);
        assert_eq!(TriBool::False.and(TriBool::Null), TriBool::False);
        assert_eq!(TriBool::Null.and(TriBool::Null), TriBool::Null);
    }

    #[test]
    fn tribool_or() {
        assert_eq!(TriBool::True.or(TriBool::False), TriBool::True);
        assert_eq!(TriBool::True.or(TriBool::Null), TriBool::True);
        assert_eq!(TriBool::False.or(TriBool::Null), TriBool::Null);
        assert_eq!(TriBool::Null.or(TriBool::Null), TriBool::Null);
    }

    #[test]
    fn tribool_passes_where() {
        assert!(TriBool::True.passes_where());
        assert!(!TriBool::False.passes_where());
        assert!(!TriBool::Null.passes_where());
    }

    #[test]
    fn tribool_eq_with_null() {
        assert_eq!(TriBool::eq_with_null(Some(&42), Some(&42)), TriBool::True);
        assert_eq!(TriBool::eq_with_null(Some(&42), Some(&99)), TriBool::False);
        assert_eq!(TriBool::eq_with_null(Some(&42), None), TriBool::Null);
    }

    #[test]
    fn tribool_is_distinct_from() {
        assert_eq!(TriBool::is_distinct_from(Some(&1), Some(&1)), TriBool::False);
        assert_eq!(TriBool::is_distinct_from(Some(&1), Some(&2)), TriBool::True);
        assert_eq!(TriBool::is_distinct_from::<i32>(None, None), TriBool::False);
        assert_eq!(TriBool::is_distinct_from::<i32>(None, Some(&5)), TriBool::True);
    }

    #[test]
    fn tribool_from_bool() {
        assert_eq!(TriBool::from(true), TriBool::True);
        assert_eq!(TriBool::from(false), TriBool::False);
    }

    #[test]
    fn tribool_display() {
        assert_eq!(TriBool::True.to_string(), "TRUE");
        assert_eq!(TriBool::Null.to_string(), "NULL");
    }

    #[test]
    fn bitmap_all_null_non_aligned() {
        let bm = NullBitmap::new_all_null(70);
        assert_eq!(bm.count_nulls(), 70);
        assert!(!bm.is_null(70));
    }

    #[test]
    fn bitmap_empty() {
        let bm = NullBitmap::new_none_null(0);
        assert!(bm.is_empty());
    }

    #[test]
    fn bitmap_clone() {
        let mut bm = NullBitmap::new_none_null(10);
        bm.set_null(3);
        let bm2 = bm.clone();
        bm.set_null(4);
        assert!(bm2.is_null(3));
        assert!(!bm2.is_null(4));
    }
}
