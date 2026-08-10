//! Linear and affine memory handles (ADR-013).
//!
//! The type system is the cheapest safety mechanism available. By giving each
//! protocol boundary its own reference type, we make protocol violations a
//! compile-time error rather than a runtime check.
//!
//! ## The two handle types
//!
//! - [`CxlRef`] — a *linear* reference to CXL-resident data. Cannot be
//!   duplicated (no `Clone`/`Copy`), cannot escape the rack scope (`!Send`,
//!   `!Sync`). Created only for regions whose [`MemoryTier`] is
//!   [`MemoryTier::Cxl`].
//! - [`RaftRef`] — an *affine* reference to cross-rack data. Cannot be
//!   duplicated, but **can** cross rack boundaries via Raft (`Send` + `Sync`).
//!
//! ## Why this matters
//!
//! Without these types, a CXL-resident region's data could accidentally be
//! forwarded to a cross-rack transaction. The reference would compile, the
//! runtime check would pass (because the CXL coordinator lives in the same
//! process), and the bug would only surface as data corruption in production.
//! With `CxlRef`/`RaftRef`, the type system rejects the code at compile time.
//!
//! See `docs/adr/013-linear-typed-memory-handles.md` for the full design.
//!
//! [`MemoryTier`]: crate::memory::tier::MemoryTier
//! [`MemoryTier::Cxl`]: crate::memory::tier::MemoryTier::Cxl

pub mod cxl_ref;
pub mod datetime;
pub mod null;
pub mod null_bitmap;
pub mod raft_ref;
pub mod string_col;

#[cfg(test)]
mod tests;

pub use cxl_ref::CxlRef;
pub use datetime::{days_since_epoch_to_year, Date, Interval, Time, Timestamp};
pub use null::{NullBitmap, TriBool};
pub use raft_ref::RaftRef;
pub use string_col::{StringColumn, StringHeap};
