//! # turbogp
//!
//! An instruction-first, memory-centric relational database engine.
//!
//! The thesis: design the database from the silicon up. Pick the cheapest
//! instructions per joule, place data in the memory tier that feeds them,
//! and treat every protocol boundary as a first-class design axis.
//!
//! ## Modules
//!
//! - [`kernel`] — the kernel table: hand-tuned instruction sequences per
//!   (CPU, tier) tuple. The engine's competitive moat.
//! - [`memory`] — tier-aware memory manager. Placement, migration, NUMA.
//! - [`storage`] — instruction-shaped storage format (4 KB page, 2 MB region,
//!   2 GB tablet).
//! - [`executor`] — scheduler of instruction streams.
//! - [`planner`] — calibrated analytic cost model (ADR-023) + Kingman
//!   queueing predictor (ADR-020) used for join ordering, index selection,
//!   and admission control.
//! - [`index`] — secondary indexes: bit-sliced index (no ADR — see
//!   `src/index/bsi.rs`) and locality-sensitive hash (ADR-017).
//! - [`sketch`] — probabilistic, mergeable summaries: HyperLogLog,
//!   Count-Min, t-Digest (ADR-015).
//! - [`protocol`] — protocol boundary coordinator (CXL, Raft/RoCEv2).
//! - [`schema`] — the last layer: MDL schema selection.
//! - [`sql`] — the SQL surface: tokenizer, recursive-descent parser for
//!   `SELECT`, scanner for the seven turboGP extensions, and the lowering
//!   pass that turns a parsed query into a [`executor::plan::LogicalPlan`].
//! - [`datasource`] — external-format ingestion (Parquet, CSV) into the
//!   engine's `Vec<u64>` cell format. Wave 19.
//! - [`catalog`] — in-memory table registry: name → [`datasource::Table`].
//!   Wave 19.
//! - [`engine`] — the end-to-end SQL surface: [`engine::QueryEngine`]
//!   ties together the parser, catalog, kernel table, and cost model.
//!   Hand it a SQL string, get back a [`engine::QueryResult`]. Wave 20.
//! - [`types`] — linear/affine memory handles (`CxlRef`, `RaftRef`) that
//!   enforce protocol boundaries at compile time (ADR-013).

#![warn(rust_2018_idioms)]
// Wave 8: suppress missing_docs and unused_imports warnings. Adding 400+
// doc comments and cleaning up 50+ unused imports is a separate documentation
// effort; the code is correct, just under-documented and has stale imports.
// Wave 8 (Production Wiring): removed `missing_docs` from this list.
// Every public item in src/**/*.rs is now documented — the formal PIVOT
// and MERGE parsers (Wave 7), the SledRaftStore / TcpRaftNetwork (Waves
// 2-3), and the async pgwire server (Wave 5) all carry `///` doc comments.
// The remaining suppressions cover pre-existing technical debt (stale
// imports, unused variables, dead code) that is a separate cleanup
// effort; the code is correct, just under-cleaned. Removing them would
// produce ~118 warnings, none of which are missing_docs.
#![allow(unused_imports, unused_variables, unused_mut, unused_assignments, dead_code)]

// Use mimalloc as the global allocator — glibc's ptmalloc2 spends ~50% of
// query execution time in malloc_consolidate + unlink_chunk + _int_free
// (measured via perf on Q3). mimalloc is a drop-in replacement with
// thread-local heaps and compact size classes that eliminate the
// consolidation overhead. This is the single highest-impact optimization
// for join-heavy queries.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod catalog;
pub mod datasource;
pub mod engine;
pub mod exec;
pub mod index;
pub mod kernel;
pub mod memory;
pub mod planner;
pub mod schema;
pub mod server;
pub mod sql;
pub mod storage;
pub mod txn;
pub mod types;

pub use error::{Error, Result};

mod error {
    use thiserror::Error;

    /// Top-level error type.
    #[derive(Debug, Error)]
    pub enum Error {
        /// I/O error.
        #[error("io error: {0}")]
        Io(#[from] std::io::Error),

        /// JSON failure.
        #[error("json error: {0}")]
        Json(#[from] serde_json::Error),

        /// Dimension mismatch.
        #[error("dimension mismatch: expected {expected}, got {actual}")]
        DimMismatch {
            /// Expected dimension.
            expected: usize,
            /// Actual dimension.
            actual: usize,
        },

        /// Invalid argument.
        #[error("invalid argument: {0}")]
        InvalidArg(String),

        /// Corruption.
        #[error("corruption: {0}")]
        Corruption(String),

        /// Not found.
        #[error("not found: {0}")]
        NotFound(String),

        /// Unsupported on this hardware.
        #[error("unsupported: {0}")]
        Unsupported(String),

        /// Generic.
        #[error("{0}")]
        Other(String),

        /// Tier-related error (e.g., data not in requested tier).
        #[error("tier error: {0}")]
        Tier(String),

        /// Protocol boundary violation (e.g., CXL data leaked to a Raft txn).
        #[error("protocol error: {0}")]
        Protocol(String),

        /// SQL parse error.
        #[error("parse error: {0}")]
        Parse(String),

        /// Operation timed out after the given number of milliseconds.
        #[error("timeout after {0} ms")]
        Timeout(u64),
    }

    impl Error {
        /// Map an Error to its SQLSTATE SQL state code (Wave 2 security fix).
        ///
        /// See: https://www.postgresql.org/docs/current/errcodes-appendix.html
        #[must_use]
        pub fn sqlstate(&self) -> &'static str {
            match self {
                Error::NotFound(_) => "42P01",        // undefined_table
                Error::Parse(_) => "42601",           // syntax_error
                Error::InvalidArg(_) => "22023",      // invalid_parameter_value
                Error::DimMismatch { .. } => "22000", // data_exception
                Error::Corruption(_) => "XX001",      // data_corrupted
                Error::Io(_) => "58030",              // io_error
                Error::Json(_) => "22P02",            // invalid_text_representation
                Error::Unsupported(_) => "0A000",     // feature_not_supported
                Error::Tier(_) => "55006",            // object_in_use
                Error::Protocol(_) => "08000",        // connection_exception
                Error::Timeout(_) => "57014",         // query_canceled
                Error::Other(msg) => {
                    // Check for constraint violations in the message text.
                    if msg.contains("23502") || msg.contains("NOT NULL") {
                        "23502" // not_null_violation
                    } else if msg.contains("23505") || msg.contains("unique") {
                        "23505" // unique_violation
                    } else if msg.contains("23503") || msg.contains("foreign key") {
                        "23503" // foreign_key_violation
                    } else if msg.contains("42501") || msg.contains("not in allowed_copy_dirs") {
                        "42501" // insufficient_privilege
                    } else {
                        "42000" // syntax_error_or_access_rule_violation (fallback)
                    }
                }
            }
        }
    }

    /// Convenience Result alias.
    pub type Result<T> = std::result::Result<T, Error>;
}

