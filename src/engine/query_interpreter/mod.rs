//! Query interpreter — the legacy SQL execution path.
//!
//! This module parses and executes SQL queries that the main executor
//! cannot handle (complex expressions, joins, subqueries, window functions).
//! It will be gradually replaced by the unified AST + logical plan + lowerer
//! pipeline (Waves 4-6).
//!
//! Sub-modules:
//! - [`types`] — Expr2, BinOp2, Value2, SelectQuery2 and other legacy types
//! - [`parser`] — `QueryInterpreterParser` and parse helpers
//! - [`exec`] — `QueryInterpreter` struct and core `execute` method
//! - [`join`] — hash join, cross join, dynamic-programming join ordering
//! - [`aggregate`] — grouped aggregation, scalar aggregates, vectorized sum/min/max
//! - [`subquery`] — subquery decorrelation, EXISTS/IN hash-set caching
//! - [`expr`] — expression evaluation (eval, binop, comparison, like, cast)
//! - [`tpc_h_queries_q1_q6`] / [`tpc_h_queries_q7_q12`] / [`tpc_h_queries_q13_q18`] /
//!   [`tpc_h_queries_q19_q22`] — TPC-H per-query detectors (to be deleted in Wave 6)

pub mod aggregate;
pub mod exec;
pub mod expr;
pub mod join;
pub mod parser;
pub mod subquery;
pub mod types;

pub use exec::execute_interpreter;
pub use parser::parse_query;
pub use types::*;

use crate::catalog::Catalog;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

// Use ahash (hardware AES) instead of std SipHash for all HashMap/HashSet.
type HashMap<K, V> = ahash::AHashMap<K, V>;
type HashSet<T> = ahash::AHashSet<T>;

/// Create a HashMap without calling OS entropy (avoids getrandom syscall).
fn new_hashmap<K, V>() -> HashMap<K, V> {
    HashMap::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create a HashSet without calling OS entropy.
fn new_hashset<T>() -> HashSet<T> {
    HashSet::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create an FxHashMap for hot GROUP BY / EXISTS paths.
fn new_fxhashmap<K, V>() -> FxHashMap<K, V> {
    FxHashMap::default()
}

/// Create an FxHashSet for hot EXISTS semi-join sets.
fn new_fxhashset<T>() -> FxHashSet<T> {
    FxHashSet::default()
}

pub(crate) fn date_to_days_q4(y: i32, m: u32, d: u32) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe as i32 - 719468) as u64
}

pub fn parse_and_execute(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    // All queries go through the generic interpreter path.
    // The TPC-H-specific is_qXX() detectors and execute_qXX_reformulated()
    // fast paths have been removed — they were benchmark-specific specializations
    // with hardcoded constants that gamed the benchmark rather than improving
    // the general engine. The generic interpreter handles all SQL uniformly.
    let query = parse_query(sql).map_err(Error::Parse)?;
    execute_interpreter(&query, catalog)
}
