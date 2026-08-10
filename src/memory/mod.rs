//! Tier-aware memory manager.
//!
//! Every piece of data lives in a specific tier of the memory hierarchy,
//! chosen by access pattern. The memory manager migrates whole 2 MB regions
//! between tiers based on telemetry.
//!
//! ## Tiers
//!
//! | Tier | Latency | What lives here |
//! |------|---------|-----------------|
//! | L1/L2 | 1–4 ns | Current 4 KB working batch (auto-managed by HW) |
//! | L3 | 10–20 ns | Hot indexes, hash tables < 32 MB, bloom filters |
//! | DDR5 | 80–100 ns | Hot working set, large hash tables |
//! | HBM | 100–150 ns | Scan-heavy analytics (Xeon Max, MI300A) |
//! | CXL | 140–500 ns | Buffer pool extension, cold-ish indexes |
//! | NVMe | 10–30 µs | WAL, LSM compaction, cold data |
//!
//! ## Modules
//!
//! - [`tier`] — the [`MemoryTier`] enum (L1L2, L3, Ddr5, Hbm, Cxl, Nvme, …).
//! - [`region`] — the [`Region`] struct (2 MB, mmap-backed, with
//!   [`RegionBacking`]).
//! - [`numa`] — [`NumaTopology`], [`NumaNode`], and CPU-affinity helpers
//!   ([`numa::pin_thread_to_cpu`], [`numa::get_current_cpu`]) per ADR-008.
//! - [`manager`] — [`MemoryManager`]: per-tier LRU eviction (ADR-010).
//! - [`bandwidth`] — [`BandwidthMonitor`]: heuristic memory-bandwidth
//!   polling and per-tier utilization.
//!
//! [`RegionBacking`]: region::RegionBacking

pub mod tier;

pub use tier::MemoryTier;
