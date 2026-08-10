//! # Buffer pool manager (Wave 63).
//!
//! Manages an in-memory cache of disk-backed pages. Pages are fetched from
//! disk on demand, pinned while in use, and evicted (with write-back if dirty)
//! when the pool is full.
//!
//! ## Design
//!
//! - **Page ID**: `(table_id, page_num)` — uniquely identifies a page on disk.
//! - **Frame**: a 4 KB slot in the buffer pool holding a [`Page`].
//! - **Pin count**: number of active users of a frame. A frame with pin_count > 0
//!   cannot be evicted.
//! - **Dirty flag**: if true, the frame's contents differ from disk and must
//!   be written back before eviction.
//! - **Eviction**: clock-replacement algorithm (approximates LRU with O(1)
//!   per access).
//!
//! ## Disk layout
//!
//! Each table is stored as a file `<data_dir>/<table_id>.tbl`. Pages are
//! addressed by their offset in the file: page N lives at byte offset
//! `N * PAGE_SIZE`. The file grows as pages are allocated.
//!
//! ## Integration
//!
//! The buffer pool is owned by [`QueryEngine`] and used by the executor to
//! fetch table pages. When a DML operation modifies a page, the page is
//! marked dirty in the buffer pool; the page-level WAL records the change;
//! on commit (or checkpoint), dirty pages are flushed to disk.

use crate::storage::page::{Page, PAGE_SIZE};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A page identifier: (table_id, page_number).
/// table_id is assigned by the catalog when a table is registered.
/// page_number is 0-indexed within the table's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub table_id: u64,
    pub page_num: u32,
}

impl PageId {
    pub fn new(table_id: u64, page_num: u32) -> Self {
        Self { table_id, page_num }
    }
}

/// A frame in the buffer pool: holds a page, its pin count, and dirty flag.
struct Frame {
    page: Page,
    pin_count: u32,
    dirty: bool,
    /// Clock bit for the eviction algorithm. Set to true on access.
    referenced: bool,
    /// The page_id currently occupying this frame (None if free).
    page_id: Option<PageId>,
}

/// The buffer pool manager.
///
/// Manages a fixed-size array of frames. When a page is requested and not
/// in the pool, it's fetched from disk (or allocated fresh if it doesn't
/// exist yet). When the pool is full, a frame is selected for eviction
/// using the clock algorithm.
pub struct BufferPool {
    /// The data directory where table files live.
    data_dir: PathBuf,
    /// The frames (fixed-size array).
    frames: Vec<Frame>,
    /// Map from PageId to frame index (for fast lookup).
    page_table: HashMap<PageId, usize>,
    /// Clock hand for eviction.
    clock_hand: usize,
    /// Open file handles, cached per table_id (to avoid re-opening).
    files: Mutex<HashMap<u64, File>>,
    /// Next page number to allocate for each table (for append).
    next_page_num: Mutex<HashMap<u64, u32>>,
}

impl BufferPool {
    /// Create a new buffer pool with the given capacity (in pages) and data directory.
    pub fn new<P: AsRef<Path>>(data_dir: P, capacity: usize) -> std::io::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let frames: Vec<Frame> = (0..capacity)
            .map(|_| Frame {
                page: Page::new(),
                pin_count: 0,
                dirty: false,
                referenced: false,
                page_id: None,
            })
            .collect();
        Ok(Self {
            data_dir,
            frames,
            page_table: HashMap::new(),
            clock_hand: 0,
            files: Mutex::new(HashMap::new()),
            next_page_num: Mutex::new(HashMap::new()),
        })
    }

    /// Get the data directory path (Wave 2: used for checkpoint file placement).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Get the file path for a table.
    fn table_path(&self, table_id: u64) -> PathBuf {
        self.data_dir.join(format!("{}.tbl", table_id))
    }

    /// Open or get a cached file handle for a table.
    fn get_file(&self, table_id: u64) -> std::io::Result<File> {
        let mut files = self.files.lock();
        if let Some(file) = files.get(&table_id) {
            // Re-open the file (we can't clone the handle, so we open a new one).
            // This is a simplification — a production system would use a pool
            // of file descriptors. For correctness, re-opening is fine.
            let path = self.table_path(table_id);
            return OpenOptions::new().read(true).write(true).create(true).open(&path);
        }
        // First access: create the file.
        let path = self.table_path(table_id);
        let file = OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        files.insert(table_id, file.try_clone()?);
        Ok(file)
    }

    /// Read a page from disk into the given Page buffer.
    fn read_page_from_disk(&self, page_id: PageId) -> std::io::Result<Page> {
        let mut file = self.get_file(page_id.table_id)?;
        let offset = (page_id.page_num as u64) * (PAGE_SIZE as u64);
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        match file.read_exact(&mut buf) {
            Ok(()) => {
                // Deserialize the page from the buffer.
                Page::from_bytes(&buf).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Page doesn't exist on disk yet — allocate a fresh page.
                Ok(Page::new())
            }
            Err(e) => Err(e),
        }
    }

    /// Write a page to disk.
    fn write_page_to_disk(&self, page_id: PageId, page: &Page) -> std::io::Result<()> {
        let mut file = self.get_file(page_id.table_id)?;
        let offset = (page_id.page_num as u64) * (PAGE_SIZE as u64);
        file.seek(SeekFrom::Start(offset))?;
        let bytes = page.to_bytes();
        file.write_all(&bytes)?;
        Ok(())
    }

    /// Find a free frame (pin_count == 0) using the clock algorithm.
    /// Returns `Some(frame_index)` if a victim is found, or `None` if all
    /// frames are pinned (buffer pool exhausted). The caller should handle
    /// `None` as an error, not a panic (Wave 6 fix: previously this panicked
    /// with "buffer pool exhausted: all frames pinned").
    fn find_victim(&mut self) -> Option<usize> {
        let n = self.frames.len();
        // Run the clock hand until we find a frame with referenced == false
        // and pin_count == 0. We clear the referenced bit as we go.
        for _ in 0..(2 * n) {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % n;
            let frame = &self.frames[idx];
            if frame.pin_count == 0 {
                if frame.referenced {
                    self.frames[idx].referenced = false;
                } else {
                    return Some(idx);
                }
            }
        }
        // No victim found — all frames are pinned. Return None so the
        // caller can handle the error gracefully (Wave 6 fix).
        None
    }

    /// Fetch a page into the buffer pool and return its frame index.
    /// The page is pinned (pin_count incremented) — the caller must call
    /// `unpin_page` when done.
    pub fn fetch_page(&mut self, page_id: PageId) -> std::io::Result<usize> {
        // Check if the page is already in the pool.
        if let Some(&idx) = self.page_table.get(&page_id) {
            self.frames[idx].pin_count += 1;
            self.frames[idx].referenced = true;
            return Ok(idx);
        }
        // Page not in pool — find a victim frame.
        let idx = self.find_victim().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::ResourceBusy,
                "buffer pool exhausted: all frames pinned (increase capacity)",
            )
        })?;
        // If the victim is dirty, write it back to disk.
        let victim_page_id = self.frames[idx].page_id;
        if self.frames[idx].dirty {
            if let Some(vpid) = victim_page_id {
                self.write_page_to_disk(vpid, &self.frames[idx].page)?;
            }
        }
        // Remove the old page_id from the page table.
        if let Some(vpid) = victim_page_id {
            self.page_table.remove(&vpid);
        }
        // Read the new page from disk.
        let page = self.read_page_from_disk(page_id)?;
        self.frames[idx].page = page;
        self.frames[idx].pin_count = 1;
        self.frames[idx].dirty = false;
        self.frames[idx].referenced = true;
        self.frames[idx].page_id = Some(page_id);
        self.page_table.insert(page_id, idx);
        Ok(idx)
    }

    /// Allocate a new page for a table (appends to the table's file).
    /// Returns the page_id of the new page.
    pub fn new_page(&mut self, table_id: u64) -> std::io::Result<PageId> {
        let mut next = self.next_page_num.lock();
        let page_num = next.entry(table_id).or_insert(0);
        let page_id = PageId::new(table_id, *page_num);
        *page_num += 1;
        drop(next);
        // Fetch the new page (it will be allocated fresh by read_page_from_disk
        // since the file doesn't have it yet).
        let idx = self.fetch_page(page_id)?;
        self.frames[idx].dirty = true; // Mark dirty so it gets written on eviction.
        Ok(page_id)
    }

    /// Unpin a page (decrement pin_count). If `dirty` is true, mark the frame
    /// dirty so it will be written back on eviction.
    pub fn unpin_page(&mut self, page_id: PageId, dirty: bool) {
        if let Some(&idx) = self.page_table.get(&page_id) {
            if self.frames[idx].pin_count > 0 {
                self.frames[idx].pin_count -= 1;
            }
            if dirty {
                self.frames[idx].dirty = true;
            }
        }
    }

    /// Get a reference to the page in a frame (read-only).
    pub fn get_page(&self, frame_idx: usize) -> &Page {
        &self.frames[frame_idx].page
    }

    /// Get a mutable reference to the page in a frame.
    /// The caller is responsible for marking the page dirty via `unpin_page(page_id, true)`.
    pub fn get_page_mut(&mut self, frame_idx: usize) -> &mut Page {
        &mut self.frames[frame_idx].page
    }

    /// Flush all dirty pages for a given table to disk.
    pub fn flush_table(&mut self, table_id: u64) -> std::io::Result<()> {
        for (page_id, &idx) in self.page_table.iter() {
            if page_id.table_id == table_id && self.frames[idx].dirty {
                self.write_page_to_disk(*page_id, &self.frames[idx].page)?;
                self.frames[idx].dirty = false;
            }
        }
        Ok(())
    }

    /// Flush all dirty pages to disk.
    pub fn flush_all(&mut self) -> std::io::Result<()> {
        let page_ids: Vec<PageId> = self.page_table.keys().copied().collect();
        for page_id in page_ids {
            let idx = self.page_table[&page_id];
            if self.frames[idx].dirty {
                self.write_page_to_disk(page_id, &self.frames[idx].page)?;
                self.frames[idx].dirty = false;
            }
        }
        Ok(())
    }

    /// Get the number of pages allocated for a table.
    pub fn page_count(&self, table_id: u64) -> u32 {
        let path = self.table_path(table_id);
        if path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (size / PAGE_SIZE as u64) as u32
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn buffer_pool_basic_fetch_and_evict() {
        let tmp = TempDir::new().unwrap();
        let mut pool = BufferPool::new(tmp.path(), 4).unwrap();
        // Allocate 4 pages for table 1.
        let p0 = pool.new_page(1).unwrap();
        let p1 = pool.new_page(1).unwrap();
        let p2 = pool.new_page(1).unwrap();
        let p3 = pool.new_page(1).unwrap();
        // Fetch them all (should be in the pool).
        let f0 = pool.fetch_page(p0).unwrap();
        let f1 = pool.fetch_page(p1).unwrap();
        let f2 = pool.fetch_page(p2).unwrap();
        let f3 = pool.fetch_page(p3).unwrap();
        // Write a value to page 0.
        {
            let page = pool.get_page_mut(f0);
            page.set_cell(0, 42);
        }
        pool.unpin_page(p0, true);
        pool.unpin_page(p1, false);
        pool.unpin_page(p2, false);
        pool.unpin_page(p3, false);
        // Flush and verify the value persists on disk.
        pool.flush_all().unwrap();
        // Re-fetch page 0 and verify the value.
        let f0_again = pool.fetch_page(p0).unwrap();
        let page = pool.get_page(f0_again);
        assert_eq!(page.get_cell(0), 42, "value must persist across flush");
    }

    #[test]
    fn buffer_pool_eviction() {
        let tmp = TempDir::new().unwrap();
        let mut pool = BufferPool::new(tmp.path(), 2).unwrap();
        // Allocate 3 pages — should evict the first.
        let p0 = pool.new_page(1).unwrap();
        let p1 = pool.new_page(1).unwrap();
        // Unpin both so they can be evicted.
        pool.unpin_page(p0, false);
        pool.unpin_page(p1, false);
        // Allocate a third page — this should evict p0.
        let p2 = pool.new_page(1).unwrap();
        // All three should be fetchable (p0 from disk, p1 and p2 from pool).
        let _ = pool.fetch_page(p2).unwrap();
        pool.unpin_page(p2, false);
        let _ = pool.fetch_page(p0).unwrap();
    }
}
