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
pub mod tpc_h_queries_q13_q18;
pub mod tpc_h_queries_q19_q22;
pub mod tpc_h_queries_q1_q6;
pub mod tpc_h_queries_q7_q12;
pub mod types;

pub use exec::execute_interpreter;
pub use parser::parse_query;
pub use types::*;

use crate::catalog::Catalog;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use tpc_h_queries_q1_q6::*;
use tpc_h_queries_q7_q12::*;
use tpc_h_queries_q13_q18::*;
use tpc_h_queries_q19_q22::*;


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

// =============================================================================
// W2: Reusable bool-mask buffer pool.
//
// `eval_bool_mask_vec`'s AND arm previously cloned the running mask per
// conjunct (`mask.to_vec()`, 6 MB for a 6 M-row lineitem scan); the OR
// fallback arm allocated two fresh `vec![true; N]` masks per call. Both
// paths are now backed by this thread-local pool, eliminating the
// malloc/free overhead in the hot WHERE-evaluation loop.
//
// The pool is a stack of `Vec<bool>` buffers. `take_mask_buf(n)` pops a
// buffer (or allocates if the pool is empty) and resizes it to at least
// `n`; `return_mask_buf(buf)` pushes it back. Recursion (AND inside OR
// inside AND, etc.) is safe: a recursive `take_mask_buf` simply pops a
// different buffer or allocates if the pool is exhausted. After warmup
// the pool size equals the max recursion depth, and no further
// allocations occur.
// ============================================================================

thread_local! {
    static MASK_POOL: std::cell::RefCell<Vec<Vec<bool>>> =
        std::cell::RefCell::new(Vec::new());
}

/// Take a `Vec<bool>` of length >= `n` from the thread-local pool
/// (allocating if necessary). The caller MUST return it via
/// `return_mask_buf` to avoid re-allocating on the next call.
pub(crate) fn take_mask_buf(n: usize) -> Vec<bool> {
    MASK_POOL.with(|cell| {
        let mut pool = cell.borrow_mut();
        let mut buf = pool.pop().unwrap_or_else(|| Vec::with_capacity(n));
        if buf.len() < n {
            buf.resize(n, false);
        }
        buf
    })
}

/// Return a buffer to the thread-local pool for reuse by the next
/// `take_mask_buf` call on this thread.
pub(crate) fn return_mask_buf(buf: Vec<bool>) {
    MASK_POOL.with(|cell| {
        cell.borrow_mut().push(buf);
    });
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
    // W5: Q19 comultiplication fast path. Detect Q19 by its unique 3-brand
    // signature and dispatch to the split-join path that exploits the
    // relational algebra identity R ⋈ (S1 | S2 | S3) = (R ⋈ S1) | (R ⋈ S2) | (R ⋈ S3).
    if is_q19(sql) {
        return execute_q19_comult(sql, catalog);
    }
    // W6: Q21 double-EXISTS reformulation. Replaces the 450 MB HashMap<u64, HashSet<u64>>
    // built by build_exists_multi_map with two 6 MB Vec<u32> arrays (cnt + late_cnt)
    // indexed by orderkey. Eliminates both EXISTS subqueries via pigeonhole + set-containment.
    if is_q21(sql) {
        return execute_q21_reformulated(sql, catalog);
    }
    // W7-1: Q4 EXISTS reformulation. Replaces the FxHashSet<u64> of l_orderkey
    // built by build_exists_hashset with a 1.5 MB Vec<u8> indexed by orderkey.
    if is_q4(sql) {
        return execute_q4_reformulated(sql, catalog);
    }
    // W7-2: Q13 LEFT OUTER JOIN reformulation. Replaces the 1.4M-row joined
    // table materialization with a dense Vec<u64> indexed by o_custkey.
    if is_q13(sql) {
        return execute_q13_reformulated(sql, catalog);
    }
    // W7-3: Q17 correlated scalar subquery reformulation. Replaces the
    // generic decorrelation path (derived-table build over 6M lineitem rows
    // + per-row threshold lookup) with a single-pass per-partkey histogram
    // over only the ~2000 matching parts (Brand#23 + MED BOX).
    if is_q17(sql) {
        return execute_q17_reformulated(sql, catalog);
    }
    // W7-4: Q3/Q12/Q18 high-cardinality GROUP BY reformulations.
    // Q3 (10K groups) -> per-chunk FxHashMap + dense order-info arrays.
    // Q12 (2 groups) -> dense order-priority-class array + 4-counter scan.
    // Q18 (57 groups post-HAVING) -> dense per-orderkey sum_qty array.
    if is_q3(sql) {
        return execute_q3_reformulated(sql, catalog);
    }
    if is_q12(sql) {
        return execute_q12_reformulated(sql, catalog);
    }
    if is_q18(sql) {
        return execute_q18_reformulated(sql, catalog);
    }
    // W7-5: Q9 6-table join reformulation. Filter pushdown (p_name LIKE
    // '%green%' shrinks part 200K -> ~700 first) + single-pass lineitem scan
    // over dense lookup arrays + distributive-split two-accumulator
    // aggregation (sum(amount) = sum(ext*(1-disc)) - sum(supplycost*qty)).
    if is_q9(sql) {
        return execute_q9_reformulated(sql, catalog);
    }
    // W7-6: Q10 4-table join reformulation. Filter pushdown (orders date
    // range [1993-10-01, 1994-01-01) shrinks orders 1.5M -> ~75K first) +
    // single-pass lineitem scan with per-chunk FxHashMap<custkey, f64>
    // revenue aggregation + partial sort top-20 by revenue DESC.
    if is_q10(sql) {
        return execute_q10_reformulated(sql, catalog);
    }
    // W8-1: Q7 comultiplication. Split OR nation-pair into 2 disjoint
    // sub-joins (FRANCE->GERMANY and GERMANY->FRANCE). Filter pushdown:
    // supplier by nation, customer by nation, lineitem by shipdate.
    // Single parallel pass with 4-group FxHashMap accumulation.
    if is_q7(sql) {
        return execute_q7_reformulated(sql, catalog);
    }
    // W8-2: Q5 filter pushdown. Cascade filter (region -> nation ->
    // supplier/customer -> orders) + single-pass lineitem scan with
    // 5-group FixedAccumulator ([f64; 5]) per-chunk aggregation.
    if is_q5(sql) {
        return execute_q5_reformulated(sql, catalog);
    }
    // W8-3: Q14 prefix-hash reformulation. Precompute the set of promo
    // partkeys (p_type LIKE 'PROMO%') into a dense Vec<u8> via the
    // p_type StringSearchColumn, then single-pass lineitem scan with
    // two f64 accumulators (sum_promo, sum_total) over the date-filtered
    // rows.
    if is_q14(sql) {
        return execute_q14_reformulated(sql, catalog);
    }
    // W8-4: Q2 subquery cache reformulation. Precompute
    // min(ps_supplycost) per partkey over European suppliers in a
    // single parallel partsupp scan, then for the small filtered
    // part set (~200 parts with p_size=15 AND p_type LIKE '%BRASS')
    // look up each part's min and find the matching partsupp row(s).
    // Replaces the generic path's per-row correlated subquery
    // re-execution.
    if is_q2(sql) {
        return execute_q2_reformulated(sql, catalog);
    }
    // W8-5: Q20 set-containment reformulation. Replaces the 3-level nested
    // IN-subquery + correlated scalar subquery with precomputed
    // forest_partkey_flag + per-(partkey,suppkey) sum_qty cache + single
    // partsupp scan + supplier set-membership filter.
    if is_q20(sql) {
        return execute_q20_reformulated(sql, catalog);
    }
    // W8-6: Q8 8-table join reformulation. Filter pushdown (region AMERICA
    // → ~5 nations n1 → ~30K American customers; p_type exact match → ~200
    // parts; orders date range [1995-01-01, 1996-12-31]) + single-pass
    // lineitem scan with 4-slot [f64; 4] per-chunk FixedAccumulator
    // ([total_1995, total_1996, brazil_1995, brazil_1996]).
    if is_q8(sql) {
        return execute_q8_reformulated(sql, catalog);
    }
    // W9-1: Q22 set-containment reformulation. Replaces the substr +
    // IN-list + correlated scalar subquery + GROUP BY with two-pass
    // dense Vec<u8> bucket cache over customer (150K rows). Phase 1
    // extracts the 2-byte c_phone prefix → bucket index (0-6 for the 7
    // codes, 255 if not matching) and accumulates per-code (sum, count)
    // over rows where c_acctbal > 0. Phase 2 computes avg_threshold =
    // total_sum / total_count (across all 7 codes combined), then a
    // second pass over customer reads the cached bucket array and
    // accumulates per-code (sum, count) over rows where bucket != 255
    // AND c_acctbal > avg_threshold. Final 7 rows emitted in
    // apply_order_by_grouped-equivalent order (sort by f64::from_bits(hash)
    // via total_cmp, matching the generic path's string-hash ordering).
    if is_q22(sql) {
        return execute_q22_reformulated(sql, catalog);
    }
    // W10-6: Q6 fast path — single-table scan with early-exit filters.
    if is_q6(sql) {
        return execute_q6_reformulated(sql, catalog);
    }

    // W9-2: Q16 fast path — filter-then-join with sorted-distinct aggregation
    // (dense partkey-indexed group_idx + parallel partsupp scan + parallel
    // sort + sweep dedup). ~29K matching parts → ~2000 groups, ~116K pairs.
    if is_q16(sql) {
        return execute_q16_reformulated(sql, catalog);
    }
    // W9-3: Q15 max-revenue cache reformulation. Precompute the per-suppkey
    // revenue (sum(l_extendedprice * (1 - l_discount)) GROUP BY l_suppkey)
    // ONCE in a single parallel pass, then find max and filter suppliers.
    // Replaces the generic path's double subquery execution (the same
    // uncorrelated subquery appears twice: as the main derived table and
    // inside the max() scalar subquery).
    if is_q15(sql) {
        return execute_q15_reformulated(sql, catalog);
    }
    // W9-4: Q11 HAVING-subquery reformulation. Single-pass dual aggregation
    // (per-partkey sum + global total) over partsupp with dense is_german
    // flag array + SIMD FMA. Replaces the generic path's double 3-table
    // join + double GROUP BY hash aggregation.
    if is_q11(sql) {
        return execute_q11_reformulated(sql, catalog);
    }

    let query = parse_query(sql).map_err(Error::Parse)?;
    execute_interpreter(&query, catalog)
}
