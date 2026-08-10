//! Instruction-shaped storage format.
//!
//! The storage format is chosen so the cheapest SIMD instructions can extract
//! value at peak throughput. Every value is a 64-bit word; pages are 4 KB
//! (OS page size, 64 cache lines, 512 cells); regions are 2 MB (huge page
//! granularity, unit of migration); tablets are 2 GB (NUMA placement unit).

pub mod buffer_pool;
pub mod page;
pub mod recovery;
pub mod replication;

pub use buffer_pool::{BufferPool, PageId};
pub use page::{Page, PageHeader, HEADER_SIZE, PAGE_CELLS, PAGE_SIZE};
pub use replication::{backup, restore, WalStreamer};
