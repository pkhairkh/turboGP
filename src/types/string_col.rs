//! StringColumn — inline ≤7 bytes in u64, heap for longer.
//!
//! Research: Small String Optimization (SSO) from C++ std::string.
//! Bioinformatics insight: bit-parallel Shift-Or matching for LIKE patterns.
//!
//! Layout: top nibble of u64 = tag.
//! - 0x0 = empty string (all zeros)
//! - 0x1..0x7 = inline length, lower 60 bits = bytes
//! - 0xF = long string, lower 60 bits = heap offset

use std::sync::Arc;

pub const LONG_TAG: u8 = 0xF;

#[derive(Debug, Default, Clone)]
pub struct StringHeap {
    pub bytes: Vec<u8>,
}

impl StringHeap {
    pub fn append(&mut self, s: &[u8]) -> u64 {
        let len = s.len();
        if len > u32::MAX as usize {
            return 0;
        }
        let offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&(len as u32).to_le_bytes());
        self.bytes.extend_from_slice(s);
        offset
    }

    pub fn get(&self, offset: u64) -> &str {
        let start = offset as usize;
        if start + 4 > self.bytes.len() {
            return "";
        }
        let len =
            u32::from_le_bytes(self.bytes[start..start + 4].try_into().unwrap_or([0; 4])) as usize;
        let str_start = start + 4;
        if str_start + len > self.bytes.len() {
            return "";
        }
        std::str::from_utf8(&self.bytes[str_start..str_start + len]).unwrap_or("")
    }
}

#[derive(Debug, Clone, Default)]
pub struct StringColumn {
    pub handles: Vec<u64>,
    pub heap: Arc<StringHeap>,
}

impl StringColumn {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, s: &str) {
        let heap = Arc::make_mut(&mut self.heap);
        self.handles.push(pack_string(heap, s));
    }

    pub fn get_owned(&self, i: usize) -> String {
        if i >= self.handles.len() {
            return String::new();
        }
        unpack_string_owned(self.handles[i], &self.heap)
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    pub fn count_inline(&self) -> usize {
        self.handles.iter().filter(|h| ((**h >> 60) & 0xF) as u8 != LONG_TAG).count()
    }
}

pub fn pack_string(heap: &mut StringHeap, s: &str) -> u64 {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return 0;
    }
    if len <= 7 {
        let mut h = (len as u64) << 60;
        for (i, &b) in bytes.iter().enumerate() {
            h |= (b as u64) << (8 * i);
        }
        h
    } else {
        let offset = heap.append(bytes);
        if offset >= (1 << 60) {
            return 0;
        }
        (LONG_TAG as u64) << 60 | offset
    }
}

pub fn unpack_string_owned(handle: u64, heap: &StringHeap) -> String {
    if handle == 0 {
        return String::new();
    }
    let tag = ((handle >> 60) & 0xF) as u8;
    if tag == LONG_TAG {
        let offset = handle & ((1 << 60) - 1);
        heap.get(offset).to_string()
    } else {
        let len = tag as usize;
        let mut bytes = [0u8; 8];
        for (i, b) in bytes.iter_mut().enumerate().take(len) {
            *b = ((handle >> (8 * i)) & 0xFF) as u8;
        }
        String::from_utf8_lossy(&bytes[..len]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string() {
        let mut heap = StringHeap::default();
        let h = pack_string(&mut heap, "");
        assert_eq!(h, 0);
        assert_eq!(unpack_string_owned(h, &heap), "");
    }

    #[test]
    fn short_string_inline() {
        let mut heap = StringHeap::default();
        let h = pack_string(&mut heap, "hello");
        assert_eq!(((h >> 60) & 0xF) as u8, 5);
        assert_eq!(unpack_string_owned(h, &heap), "hello");
    }

    #[test]
    fn seven_bytes_inline() {
        let mut heap = StringHeap::default();
        let h = pack_string(&mut heap, "1234567");
        assert_eq!(((h >> 60) & 0xF) as u8, 7);
        assert_eq!(unpack_string_owned(h, &heap), "1234567");
    }

    #[test]
    fn eight_bytes_heap() {
        let mut heap = StringHeap::default();
        let h = pack_string(&mut heap, "12345678");
        assert_eq!(((h >> 60) & 0xF) as u8, LONG_TAG);
        assert_eq!(unpack_string_owned(h, &heap), "12345678");
    }

    #[test]
    fn long_string_round_trip() {
        let mut heap = StringHeap::default();
        let s = "The quick brown fox jumps over the lazy dog";
        let h = pack_string(&mut heap, s);
        assert_eq!(unpack_string_owned(h, &heap), s);
    }

    #[test]
    fn unicode_short() {
        let mut heap = StringHeap::default();
        let h = pack_string(&mut heap, "café");
        assert_eq!(unpack_string_owned(h, &heap), "café");
    }

    #[test]
    fn unicode_long() {
        let mut heap = StringHeap::default();
        let s = "日本語のテキスト";
        let h = pack_string(&mut heap, s);
        assert_eq!(((h >> 60) & 0xF) as u8, LONG_TAG);
        assert_eq!(unpack_string_owned(h, &heap), s);
    }

    #[test]
    fn column_push_get() {
        let mut col = StringColumn::new();
        col.push("alice");
        col.push("bob");
        col.push("the quick brown fox");
        assert_eq!(col.len(), 3);
        assert_eq!(col.get_owned(0), "alice");
        assert_eq!(col.get_owned(1), "bob");
        assert_eq!(col.get_owned(2), "the quick brown fox");
    }

    #[test]
    fn column_count_inline() {
        let mut col = StringColumn::new();
        col.push("a");
        col.push("bb");
        col.push("this is a longer string");
        col.push("ccc");
        assert_eq!(col.count_inline(), 3);
    }

    #[test]
    fn column_clone_shares_heap() {
        let mut col = StringColumn::new();
        col.push("short");
        col.push("a very long string that goes to the heap");
        let col2 = col.clone();
        assert!(Arc::ptr_eq(&col.heap, &col2.heap));
        assert_eq!(col.get_owned(1), col2.get_owned(1));
    }

    #[test]
    fn empty_column() {
        let col = StringColumn::new();
        assert!(col.is_empty());
        assert_eq!(col.len(), 0);
    }

    #[test]
    fn get_out_of_bounds() {
        let col = StringColumn::new();
        assert_eq!(col.get_owned(0), "");
        assert_eq!(col.get_owned(999), "");
    }

    #[test]
    fn heap_append_multiple() {
        let mut heap = StringHeap::default();
        let off1 = heap.append(b"hello");
        let off2 = heap.append(b"world");
        assert_ne!(off1, off2);
        assert_eq!(heap.get(off1), "hello");
        assert_eq!(heap.get(off2), "world");
    }

    #[test]
    fn thousand_strings_round_trip() {
        let mut col = StringColumn::new();
        let strings: Vec<String> = (0..1000).map(|i| format!("string_{i}")).collect();
        for s in &strings {
            col.push(s);
        }
        for (i, expected) in strings.iter().enumerate() {
            assert_eq!(col.get_owned(i), *expected, "mismatch at index {i}");
        }
    }

    #[test]
    fn special_chars_short() {
        let mut heap = StringHeap::default();
        let s = "!@#$%^&";
        let h = pack_string(&mut heap, s);
        assert_eq!(unpack_string_owned(h, &heap), s);
    }
}
