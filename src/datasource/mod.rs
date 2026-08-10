//! # Data source — external format ingestion.
//!
//! turboGP's storage layer is built around 64-bit cells (the universal
//! column format consumed by the morsel executor). Before Wave 19 the
//! engine could only consume data it had written itself: the WAL, the
//! SSTable, and the kernel table. There was no way to load a Parquet
//! file produced by DuckDB, a CSV exported from Postgres, or any other
//! externally generated dataset.
//!
//! This module closes that gap. It provides readers for the two formats
//! the engine most needs to ingest for benchmarking:
//!
//! - [`parquet`] — Parquet via the `arrow` + `parquet` crates. Reads
//!   any Parquet file (Int32/Int64/Float64/Utf8/Boolean/Date32) and
//!   converts each column into turboGP's `Vec<u64>` cell format.
//! - [`csv`] — Plain-text CSV via `std::fs`. Numeric columns are parsed
//!   as `i64` and cast to `u64`; non-numeric columns are hashed to
//!   `u64` for filter-only use.
//! - [`table`] — The in-memory [`Table`] struct that the executor
//!   operates on, with a [`Table::from_loaded`] constructor that
//!   re-uses the [`parquet::LoadedTable`] / [`csv`] output directly.
//!
//! ## The u64 cell contract
//!
//! Every column in turboGP is a `Vec<u64>` of length `row_count`. The
//! type conversion is documented per-format:
//!
//! | Arrow type   | u64 encoding                       |
//! |--------------|------------------------------------|
//! | Int32        | `value as u64` (zero-extends)      |
//! | Int64        | `value as u64` (bit-reinterpret)   |
//! | Float64      | `f64::to_bits(value)`              |
//! | Utf8/LargeUtf8 | `xxh3_64(bytes)` (hash)          |
//! | Boolean      | `0u64` / `1u64`                    |
//! | Date32       | days since epoch as `u64`          |
//! | Null         | `0u64` sentinel                    |
//!
//! String hashing is lossy — the engine can filter on a hashed value
//! but cannot recover the original bytes. Full string column support
//! (a sidecar bytes arena keyed by the hash) is deferred to a future
//! wave.

pub mod csv;
pub mod parquet;
pub mod projection;
pub mod table;

pub use csv::read_csv;
pub use parquet::{
    read_parquet, read_parquet_column, read_parquet_column_names, LoadedColumn, LoadedTable,
};
pub use table::Table;
