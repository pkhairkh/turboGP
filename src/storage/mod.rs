//! Instruction-shaped storage format.
//!
//! The storage format is chosen so the cheapest SIMD instructions can extract
//! value at peak throughput. Every value is a 64-bit word; pages are 4 KB
//! (OS page size, 64 cache lines, 512 cells); regions are 2 MB (huge page
//! granularity, unit of migration); tablets are 2 GB (NUMA placement unit).

pub mod buffer_pool;
pub mod checkpoint;
pub mod page;
pub mod recovery;
pub mod replication;

// Wave 5 (Task 5.1+): real Raft consensus via openraft. Compiled only
// when the `raft` feature is enabled, so the default build does not pull
// in openraft or its tokio dependency.
#[cfg(feature = "raft")]
pub mod raft;
// Production Wiring Wave 2: persistent (disk-backed) Raft storage.
// Compiled only when the `raft` feature is enabled.
#[cfg(feature = "raft")]
pub mod raft_store;
// Production Wiring Wave 3: TCP transport for openraft RPCs.
// Compiled only when the `raft` feature is enabled.
#[cfg(feature = "raft")]
pub mod raft_network;

pub use buffer_pool::{BufferPool, PageId};
pub use checkpoint::BinaryCheckpoint;
pub use page::{Page, PageHeader, HEADER_SIZE, PAGE_CELLS, PAGE_SIZE};
pub use replication::{backup, restore, WalStreamer};
