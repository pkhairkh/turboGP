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
pub mod accum_grouped;
pub mod exec;
pub mod expr;
pub mod join;
pub mod parser;
pub mod profiler;
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
    let mut query = parse_query(sql).map_err(Error::Parse)?;

    // W19-T1: COUNT(DISTINCT) fusion — detect the pattern
    // SELECT COUNT(*) FROM (SELECT DISTINCT col FROM table) AS sub
    // and rewrite to SELECT COUNT(DISTINCT col) FROM table.
    // This eliminates materializing 100M rows for the inner subquery
    // and instead uses HyperLogLog (O(1) memory).
    if let Some(rewritten) = try_count_distinct_fusion(&mut query) {
        if rewritten {
            // Query was rewritten in-place.
        }
    }

    // W19-T2: Apply DISTINCT deduplication if the query has DISTINCT.
    // The interpreter parser now captures the DISTINCT flag.
    let mut result = execute_interpreter(&query, catalog)?;
    if query.distinct {
        result = deduplicate_result(result);
    }
    Ok(result)
}

/// W19-T1: Detect `SELECT COUNT(*) FROM (SELECT DISTINCT col FROM table) AS sub`
/// and rewrite the query in-place to `SELECT COUNT(DISTINCT col) FROM table`.
/// Returns Some(true) if the rewrite was applied.
fn try_count_distinct_fusion(query: &mut SelectQuery2) -> Option<bool> {
    // Check: outer query has exactly one SELECT item: COUNT(*)
    if query.select.len() != 1 {
        return None;
    }
    let is_count_star = match &query.select[0].expr {
        Expr2::CountStar => true,
        Expr2::Agg { func: AggFunc::Count, arg, distinct: false } => {
            matches!(arg.as_ref(), Expr2::CountStar)
        }
        _ => false,
    };
    if !is_count_star {
        return None;
    }

    // Check: no GROUP BY, no WHERE, no HAVING, no JOINs, no ORDER BY
    if !query.group_by.is_empty() || query.where_clause.is_some()
        || query.having.is_some() || !query.joins.is_empty()
        || !query.order_by.is_empty()
    {
        return None;
    }

    // Check: FROM has exactly one item: a derived table (subquery)
    if query.from.len() != 1 {
        return None;
    }
    let inner_query = match &query.from[0] {
        FromItem::Derived(sub, _) => sub.as_ref(),
        FromItem::Table(_) => return None,
    };

    // Check: inner query has DISTINCT
    if !inner_query.distinct {
        return None;
    }

    // Check: inner query has exactly one SELECT item: a simple column
    if inner_query.select.len() != 1 {
        return None;
    }
    let col_expr = match &inner_query.select[0].expr {
        Expr2::Col(_) => &inner_query.select[0].expr,
        _ => return None,
    };

    // Check: inner query has no GROUP BY, no HAVING, no JOINs, no ORDER BY, no LIMIT
    // (WHERE is OK — it gets pushed down to the scan)
    if !inner_query.group_by.is_empty() || inner_query.having.is_some()
        || !inner_query.joins.is_empty() || !inner_query.order_by.is_empty()
        || inner_query.limit.is_some()
    {
        return None;
    }

    // Rewrite: replace the outer query with COUNT(DISTINCT col) FROM inner_table
    let inner_from = inner_query.from.clone();
    let inner_where = inner_query.where_clause.clone();
    let col_clone = col_expr.clone();

    query.from = inner_from;
    query.where_clause = inner_where;
    query.select = vec![SelectItem2 {
        expr: Expr2::Agg {
            func: AggFunc::Count,
            arg: Box::new(col_clone),
            distinct: true,
        },
        alias: query.select[0].alias.clone(),
    }];

    Some(true)
}

/// Deduplicate the rows of a QueryResult (for SELECT DISTINCT).
/// Uses sort-based dedup for single-column results, HashSet for multi-column.
fn deduplicate_result(result: QueryResult) -> QueryResult {
    if result.row_count <= 1 {
        return result;
    }
    // Sort-based dedup for single-column (no string values).
    if result.columns.len() == 1 && result.columns[0].string_values.is_none() {
        use rayon::prelude::*;
        let col = &result.columns[0];
        let n = result.row_count;
        let mut pairs: Vec<(u64, u32)> = (0..n)
            .map(|i| (col.values.get(i).copied().unwrap_or(0), i as u32))
            .collect();
        pairs.par_sort_unstable_by_key(|(v, _)| *v);
        let mut keep_indices: Vec<usize> = Vec::new();
        if !pairs.is_empty() {
            let mut cur_val = pairs[0].0;
            keep_indices.push(pairs[0].1 as usize);
            for &(v, idx) in &pairs[1..] {
                if v != cur_val {
                    keep_indices.push(idx as usize);
                    cur_val = v;
                }
            }
        }
        keep_indices.sort_unstable();
        let new_row_count = keep_indices.len();
        let columns: Vec<ResultColumn> = result
            .columns
            .into_iter()
            .map(|mut c| {
                let new_values: Vec<u64> =
                    keep_indices.iter().map(|&i| c.values.get(i).copied().unwrap_or(0)).collect();
                c.values = new_values;
                c
            })
            .collect();
        return QueryResult { columns, row_count: new_row_count, elapsed_us: result.elapsed_us };
    }

    // Multi-column: use HashSet.
    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    let mut keep_indices: Vec<usize> = Vec::with_capacity(result.row_count);
    for row in 0..result.row_count {
        let key: Vec<u64> =
            result.columns.iter().map(|c| c.values.get(row).copied().unwrap_or(0)).collect();
        if seen.insert(key) {
            keep_indices.push(row);
        }
    }
    let new_row_count = keep_indices.len();
    let columns: Vec<ResultColumn> = result
        .columns
        .into_iter()
        .map(|mut c| {
            let new_values: Vec<u64> =
                keep_indices.iter().map(|&i| c.values.get(i).copied().unwrap_or(0)).collect();
            c.values = new_values;
            c
        })
        .collect();
    QueryResult { columns, row_count: new_row_count, elapsed_us: result.elapsed_us }
}
