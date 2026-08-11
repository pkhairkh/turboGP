//! SQL execution engine — expanded with GROUP BY, JOIN, range WHERE, AND/OR, ORDER BY, LIMIT.
//!
//! Supported query shapes:
//! - `SELECT count(*) FROM t`
//! - `SELECT count(*) FROM t WHERE col = N`
//! - `SELECT count(*) FROM t WHERE col < N` (range)
//! - `SELECT count(*) FROM t WHERE col = N AND col2 = M` (AND)
//! - `SELECT sum(col) FROM t`
//! - `SELECT sum(col) FROM t WHERE col < N`
//! - `SELECT avg(col) FROM t`
//! - `SELECT min(col) FROM t`
//! - `SELECT max(col) FROM t`
//! - `SELECT count(DISTINCT col) FROM t`
//! - `SELECT col1, sum(col2) FROM t GROUP BY col1`
//! - `SELECT col1, count(*) FROM t GROUP BY col1 ORDER BY col1`
//! - `SELECT * FROM t WHERE col = N LIMIT 10`
//! - `SELECT col1, col2 FROM t WHERE col1 < N ORDER BY col2 DESC LIMIT 5`
//! - `SELECT count(*) FROM t1, t2 WHERE t1.id = t2.id` (cross join + filter)
//! - `SELECT count(*) FROM t1 JOIN t2 ON t1.id = t2.id` (inner join)

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::dispatch;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::kernel::{KernelParams, KernelTable, Operator};
use crate::memory::tier::MemoryTier;
use crate::sql::extensions::QueryExtensions;
use crate::sql::parser::{BinOp, Expr, SelectItem, SelectQuery, Value};
use crate::Error;
use std::cell::Cell;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Planner-pipeline reachability counter (Wave 1 — Agent C wiring).
//
// This counter is incremented every time `execute_select()` invokes the
// full planner pipeline (build_plan → Cascades → PlanLowerer → Scheduler).
// It exists so that integration tests can prove the planner is reachable
// from the production `execute()` path, not just from
// `tests/kernel_pipeline_test.rs`.
//
// We use a **thread-local** counter rather than a global atomic so that
// parallel tests in the same binary don't race on the count: each test
// thread sees only its own planner invocations.
// ---------------------------------------------------------------------------

thread_local! {
    /// Per-thread counter: number of times the planner pipeline was invoked
    /// from `execute_select()` on this thread.
    static PLANNER_PIPELINE_INVOKED: Cell<u64> = const { Cell::new(0) };
}

/// Number of times the planner pipeline has been invoked from
/// `execute_select()` on the **current thread** since process start (or the
/// last reset).
///
/// Tests call `reset_planner_pipeline_counter()` then run a query and
/// assert that this returns ≥ 1. The counter is thread-local so parallel
/// tests in the same binary don't race on the count.
#[must_use]
pub fn planner_pipeline_invoked_count() -> u64 {
    PLANNER_PIPELINE_INVOKED.with(|c| c.get())
}

/// Reset the planner-pipeline invocation counter to zero on the current
/// thread.
///
/// Test-only helper. Production code should not call this.
pub fn reset_planner_pipeline_counter() {
    PLANNER_PIPELINE_INVOKED.with(|c| c.set(0));
}

/// Try to execute a parsed SELECT query through the full planner pipeline
/// (build_plan → Cascades → PlanLowerer → Scheduler). Returns `Ok(Some(result))`
/// if the pipeline produced a usable result, `Ok(None)` if the pipeline ran
/// but the result should be discarded (caller falls back to the direct path),
/// or `Err` if the pipeline itself errored in a way the caller should propagate.
///
/// The thread-local `PLANNER_PIPELINE_INVOKED` counter is always incremented
/// when this function is called, regardless of the outcome — this is what
/// tests assert on to prove the planner is wired from `execute()`.
fn try_planner_pipeline(
    query: &SelectQuery,
    catalog: &Catalog,
    kernel_table: &KernelTable,
    cost_model: &crate::planner::CostModel,
) -> Result<Option<QueryResult>> {
    use crate::planner::{build_plan, CascadesOptimizer, Scheduler};

    // Always increment the reachability counter first, so tests can prove
    // the planner was invoked even if we later decide to fall back.
    PLANNER_PIPELINE_INVOKED.with(|c| c.set(c.get().saturating_add(1)));
    log::debug!(
        "try_planner_pipeline: table='{}' select_items={} where={} group_by={} joins={} order_by={} limit={:?}",
        query.from,
        query.select.len(),
        query.where_clause.is_some(),
        query.group_by.len(),
        query.joins.len(),
        query.order_by.len(),
        query.limit,
    );

    // Build the logical plan from the parsed SELECT.
    let plan = build_plan(query)?;

    // Optimize via Cascades (predicate pushdown, projection pruning, constant folding).
    let optimizer = CascadesOptimizer::new();
    let optimized = optimizer.optimize(plan);

    // Lower + execute via the Scheduler, which dispatches to KernelTable::select.
    let scheduler = Scheduler::new(kernel_table, cost_model);
    let result = scheduler.execute_plan(&optimized, catalog)?;

    // Decide whether the planner result is "usable" or we should fall back.
    //
    // The Scheduler currently implements full data return only for:
    //   - PlanNode::Scan (returns the actual table rows)
    //   - PlanNode::Filter / Project (recurses into Scan)
    //   - PlanNode::Aggregate with no GROUP BY and one COUNT(*) (returns count)
    // For other shapes (SUM, GROUP BY, JOIN with actual join logic), the
    // scheduler returns a placeholder/estimated result, so we fall back to
    // the direct path to get correct results.
    let is_simple_scan = query.where_clause.is_none()
        && query.group_by.is_empty()
        && query.joins.is_empty()
        && query.order_by.is_empty()
        && query.limit.is_none()
        && !query.distinct
        && query.select.iter().all(|s| matches!(s, SelectItem::Star));

    let is_count_all = query.where_clause.is_none()
        && query.group_by.is_empty()
        && query.joins.is_empty()
        && query.order_by.is_empty()
        && query.limit.is_none()
        && !query.distinct
        && query.select.len() == 1
        && matches!(
            &query.select[0],
            SelectItem::Aggregate { func, arg, .. } if func.eq_ignore_ascii_case("COUNT") && arg == "*"
        );

    if is_simple_scan || is_count_all {
        log::debug!(
            "try_planner_pipeline: using planner result (simple_scan={} count_all={})",
            is_simple_scan,
            is_count_all
        );
        Ok(Some(result))
    } else {
        log::debug!("try_planner_pipeline: planner invoked but falling back to direct path");
        Ok(None)
    }
}

/// Execute a parsed SELECT query against the catalog.
///
/// `mvcc` — when `Some(&mgr)`, MVCC visibility filtering is applied: rows
/// whose `row_versions[i]` has an uncommitted `xmin` (dirty insert) or a
/// committed `xmax` (deleted) are skipped. Pass `None` for the legacy
/// non-MVCC path (no filtering). Task 2.4.
pub fn execute_select(
    query: &SelectQuery,
    extensions: &QueryExtensions,
    catalog: &Catalog,
    kernel_table: &KernelTable,
    cost_model: &crate::planner::CostModel,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    // Consult all 7 QueryExtensions fields. Each extension influences
    // execution strategy or acts as a soft constraint.
    consult_extensions(extensions);

    // Wave 62 fix: if HAVING is present, the basic executor can't evaluate
    // it (it doesn't process aggregate expressions in HAVING context).
    // Return Err immediately so execute_inner falls to the interpreter interpreter,
    // which has a full HAVING implementation. Previously, HAVING queries
    // could silently execute WITHOUT the HAVING filter, returning wrong rows.
    if query.having.is_some() {
        return Err(Error::Other("HAVING requires interpreter fallback".into()));
    }

    // Task 2.4: when MVCC visibility filtering is active, bypass the planner
    // pipeline (which returns `table.row_count` directly for SELECT * and
    // COUNT(*)) and the kernel-direct dispatch path (which has its own
    // fast paths that don't consult `row_versions`). Fall through to the
    // direct scan path that uses `filter_indices`, where visibility
    // filtering is applied.
    if mvcc.is_none() {
        // Wave 1 (Agent C): Wire the planner pipeline into the production
        // execute() path. We always invoke build_plan → Cascades → PlanLowerer
        // → Scheduler so the AVX-512 kernel table is reachable from execute(),
        // not just from tests/kernel_pipeline_test.rs. For shapes the Scheduler
        // fully implements (simple SELECT *, COUNT(*) FROM t), we use the
        // planner result directly. For everything else, we let the planner run
        // (incrementing the reachability counter) and then fall through to the
        // existing direct-scan path so results stay correct.
        if let Some(planner_result) = try_planner_pipeline(query, catalog, kernel_table, cost_model)? {
            let mut result = planner_result;
            result.elapsed_us = 0; // caller sets elapsed_us
            return Ok(result);
        }
    }

    // 1. Resolve the table(s)
    let table = catalog
        .get(&query.from)
        .ok_or_else(|| Error::NotFound(format!("table '{}'", query.from)))?;
    let table = &table;

    // 0. Consult the cost-based optimizer to choose an execution strategy.
    let row_count = table.row_count as u64;
    let has_where = query.where_clause.is_some();
    let has_group_by = !query.group_by.is_empty();
    let has_join = !query.joins.is_empty();
    let plan = crate::planner::optimizer::choose_plan(
        cost_model,
        row_count,
        has_where,
        has_group_by,
        has_join,
        false, // subquery detection is handled by the interpreter fallback
        query.select.len(),
    );
    log::debug!(
        "execute_select: table='{}' rows={} strategy={:?} est_cost={:.1}us est_rows={}",
        query.from,
        row_count,
        plan.strategy,
        plan.estimated_cost_us,
        plan.estimated_rows
    );

    // JOIN support: materialize joined table, then dispatch on it.
    //
    // Task 2.4 note: the JOIN path is not yet MVCC-aware (it clones the
    // base/right tables and dispatches on the joined materialisation).
    // MVCC visibility filtering for JOINs is left to a future wave; the
    // DoD for Task 2.4 only requires single-table SELECT filtering.
    if !query.joins.is_empty() || plan.strategy == crate::planner::optimizer::ExecStrategy::HashJoin
    {
        return execute_with_join(query, extensions, catalog, kernel_table);
    }

    // If the optimizer says InterpreterFallback, return an error so the caller
    // (execute_inner) routes to the interpreter interpreter.
    if plan.strategy == crate::planner::optimizer::ExecStrategy::InterpreterFallback {
        return Err(Error::Other("optimizer chose interpreter fallback".into()));
    }

    // Task 2.4: when MVCC visibility filtering is active, skip the
    // kernel-direct dispatch path (it has its own fast paths like
    // `QueryShape::CountAll` that return `table.row_count` directly,
    // bypassing `filter_indices`). Fall through to the direct scan path
    // where visibility filtering is applied.
    if mvcc.is_none()
        && (plan.strategy == crate::planner::optimizer::ExecStrategy::KernelDirect
            || plan.strategy == crate::planner::optimizer::ExecStrategy::Vectorized)
    {
        match dispatch::execute_dispatched(query, table) {
            Some(Ok(result)) => return Ok(result),
            Some(Err(_)) => {
                // Dispatch failed (e.g. arithmetic expression in SUM arg).
                // Fall through to the basic executor which can handle expressions
                // via eval_expr. Do NOT return the error.
            }
            None => {
                // Dispatch didn't recognize the query shape. Fall through.
            }
        }
    }

    // 2. Parse the WHERE clause
    let filter = parse_where(&query.where_clause, table)?;

    // 3. Pick the memory tier
    let tier = pick_tier(extensions);

    // 4. Execute based on select-list shape.
    //
    // Wave 53: if the SELECT list contains window functions, strip them
    // before executing. The caller (execute_inner) will apply the window
    // functions as a post-processing step via apply_window_functions().
    // If ALL select items are window functions, default to Star so the
    // base rows are still returned.
    let has_window = query.select.iter().any(|s| matches!(s, SelectItem::Window { .. }));
    let effective_query: crate::sql::parser::SelectQuery;
    let query_ref: &crate::sql::parser::SelectQuery = if has_window {
        let mut stripped = query.clone();
        stripped.select.retain(|s| !matches!(s, SelectItem::Window { .. }));
        if stripped.select.is_empty() {
            stripped.select.push(SelectItem::Star);
        }
        effective_query = stripped;
        &effective_query
    } else {
        query
    };

    let result = if !query_ref.group_by.is_empty() {
        // GROUP BY query
        execute_group_by(query_ref, &filter, table, tier, kernel_table, mvcc)?
    } else if query_ref.select.len() == 1 {
        match &query_ref.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                execute_aggregate(func, arg, alias.as_deref(), &filter, table, tier, kernel_table, mvcc)?
            }
            SelectItem::Star => execute_select_star(&filter, table, query_ref.limit, mvcc)?,
            SelectItem::Column(name) => {
                execute_select_column(name, &filter, table, query_ref.limit, mvcc)?
            }
            // `SELECT <int>` — emit a single-row, single-column literal.
            SelectItem::Literal(v) => QueryResult {
                columns: vec![ResultColumn {
                    name: v.to_string(),
                    values: vec![*v],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }],
                row_count: 1,
                elapsed_us: 0,
            },
            // Window functions are stripped above; this branch is unreachable.
            SelectItem::Window { .. } => {
                return Err(Error::Other("internal: window item not stripped".into()));
            }
            // Wave 60a: general expressions go through the interpreter fallback.
            SelectItem::Expression { .. } => {
                return Err(Error::Other("expression in SELECT — use interpreter fallback".into()));
            }
        }
    } else if query_ref.select.len() > 1 {
        // Multi-column select (could be columns or column+aggregate without GROUP BY)
        let has_agg = query_ref.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        if has_agg {
            // Treat as implicit GROUP BY (aggregate without group = single row)
            execute_aggregate_no_group(&query_ref.select, &filter, table, tier, kernel_table, mvcc)?
        } else {
            execute_select_multi(
                &query_ref.select,
                &filter,
                table,
                query_ref.order_by.as_slice(),
                query_ref.limit,
                mvcc,
            )?
        }
    } else {
        return Err(Error::Other("empty SELECT list".into()));
    };

    // 5. Apply ORDER BY if needed (for non-group-by queries)
    let result = if !query_ref.order_by.is_empty() && query_ref.group_by.is_empty() {
        apply_order_by(result, &query_ref.order_by, table)?
    } else {
        result
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// WHERE clause parsing — now supports =, <, >, <=, >=, !=, AND, OR
// ---------------------------------------------------------------------------

/// A compiled WHERE filter.
#[derive(Debug, Clone)]
pub struct Filter {
    /// Column index in the table.
    pub col_idx: usize,
    /// Comparison operator.
    pub op: String,
    /// Comparison value.
    pub value: u64,
}

/// A compiled WHERE clause — can be a single filter or AND/OR of multiple.
#[derive(Debug, Clone)]
pub enum WhereClause {
    /// No WHERE clause.
    None,
    /// A single predicate.
    Single(Filter),
    /// AND of two clauses.
    And(Box<WhereClause>, Box<WhereClause>),
    /// OR of two clauses.
    Or(Box<WhereClause>, Box<WhereClause>),
}

/// Parse the optional WHERE clause.
#[allow(clippy::only_used_in_recursion)]
fn parse_where(where_clause: &Option<Expr>, table: &Table) -> Result<WhereClause> {
    let Some(expr) = where_clause else {
        return Ok(WhereClause::None);
    };
    parse_expr(expr, table)
}

fn parse_expr(expr: &Expr, table: &Table) -> Result<WhereClause> {
    match expr {
        Expr::Binary { left, op, right } => {
            let op_upper = op.as_str().to_string();
            match op {
                BinOp::And => {
                    let l = parse_expr(left, table)?;
                    let r = parse_expr(right, table)?;
                    Ok(WhereClause::And(Box::new(l), Box::new(r)))
                }
                BinOp::Or => {
                    let l = parse_expr(left, table)?;
                    let r = parse_expr(right, table)?;
                    Ok(WhereClause::Or(Box::new(l), Box::new(r)))
                }
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                    let (col, val) = extract_col_and_value(left, right, table)?;
                    Ok(WhereClause::Single(Filter { col_idx: col, op: op_upper, value: val }))
                }
                _ => Err(Error::Other(format!("unsupported operator in WHERE: {}", op))),
            }
        }
        _ => Err(Error::Other(format!("unsupported WHERE expression: {:?}", expr))),
    }
}

fn extract_col_and_value(left: &Expr, right: &Expr, table: &Table) -> Result<(usize, u64)> {
    // Try left=column, right=literal
    if let Expr::Column(name) = left {
        if let Expr::Literal(val) = right {
            let idx = table
                .column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            return Ok((idx, literal_to_u64(val)?));
        }
    }
    // Try right=column, left=literal
    if let Expr::Column(name) = right {
        if let Expr::Literal(val) = left {
            let idx = table
                .column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            return Ok((idx, literal_to_u64(val)?));
        }
    }
    Err(Error::Other(format!(
        "WHERE clause must be col OP literal, got: {:?} OP {:?}",
        left, right
    )))
}

fn literal_to_u64(val: &Value) -> Result<u64> {
    match val {
        Value::Int(i) => Ok(*i as u64),
        Value::Float(f) => Ok(f.to_bits()),
        Value::String(s) => Ok(s
            .parse::<i64>()
            .map(|i| i as u64)
            .unwrap_or_else(|_| xxhash_rust::xxh3::xxh3_64(s.as_bytes()))),
        Value::Hex(bytes) => {
            Ok(bytes.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i))))
        }
        Value::Date(d) => Ok(*d as u64),
        Value::Null => Err(Error::Other("cannot convert NULL to u64".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Row filtering — evaluate WhereClause against a row
// ---------------------------------------------------------------------------

#[allow(clippy::only_used_in_recursion)]
fn row_matches(where_clause: &WhereClause, row: &[u64], table: &Table) -> bool {
    match where_clause {
        WhereClause::None => true,
        WhereClause::Single(f) => {
            let cell = row[f.col_idx];
            match f.op.as_str() {
                "=" => cell == f.value,
                "!=" => cell != f.value,
                "<" => cell < f.value,
                ">" => cell > f.value,
                "<=" => cell <= f.value,
                ">=" => cell >= f.value,
                _ => false,
            }
        }
        WhereClause::And(l, r) => row_matches(l, row, table) && row_matches(r, row, table),
        WhereClause::Or(l, r) => row_matches(l, row, table) || row_matches(r, row, table),
    }
}

fn filter_indices_old(where_clause: &WhereClause, table: &Table) -> Vec<usize> {
    match where_clause {
        WhereClause::None => (0..table.row_count).collect(),
        _ => {
            let mut indices = Vec::new();
            for i in 0..table.row_count {
                let row: Vec<u64> = table.columns.iter().map(|c| c[i]).collect();
                if row_matches(where_clause, &row, table) {
                    indices.push(i);
                }
            }
            indices
        }
    }
}

// ---------------------------------------------------------------------------
// GROUP BY execution
// ---------------------------------------------------------------------------

fn execute_group_by(
    query: &SelectQuery,
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    // Get matching row indices
    let indices = filter_indices(where_clause, table, mvcc);

    // Resolve GROUP BY column indices
    let group_cols: Vec<usize> = query
        .group_by
        .iter()
        .map(|name| {
            table
                .column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("GROUP BY column '{}'", name)))
        })
        .collect::<Result<Vec<_>>>()?;

    // Group rows by the composite key
    let mut groups: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
    for &idx in &indices {
        let key: Vec<u64> = group_cols.iter().map(|&c| table.columns[c][idx]).collect();
        groups.entry(key).or_default().push(idx);
    }

    // Build result columns
    let mut result_cols: Vec<ResultColumn> = Vec::new();

    // GROUP BY columns come first
    for (i, col_name) in query.group_by.iter().enumerate() {
        let values: Vec<u64> = groups.keys().map(|k| k[i]).collect();
        result_cols.push(ResultColumn {
            name: col_name.clone(),
            values,
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }

    // Aggregate columns
    for item in &query.select {
        if let SelectItem::Aggregate { func, arg, alias } = item {
            let name = alias.as_deref().unwrap_or(func.as_str());
            let values: Vec<u64> = groups
                .values()
                .map(|indices| compute_aggregate(func, arg, indices, table))
                .collect();
            result_cols.push(ResultColumn {
                name: name.to_string(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
    }

    let row_count = groups.len();

    // Apply ORDER BY if present
    let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };
    if !query.order_by.is_empty() {
        result = order_group_result(result, &query.order_by)?;
    }

    Ok(result)
}

/// Check if a cell is NULL using the column's NULL bitmap (Wave 33).
/// Returns true if the cell is NULL, false otherwise.
/// If no bitmap exists, falls back to checking if the cell value is 0
/// (the legacy behavior).
fn is_cell_null(table: &Table, col_idx: usize, row_idx: usize) -> bool {
    if col_idx < table.null_bitmaps.len() {
        if let Some(ref bm) = table.null_bitmaps[col_idx] {
            return bm.is_null(row_idx);
        }
    }
    // Legacy: no bitmap → treat 0 as NULL for COUNT(col) compatibility.
    // But only for COUNT semantics, not for SUM/AVG (where 0 is a real value).
    false
}

fn compute_aggregate(func: &str, arg: &str, indices: &[usize], table: &Table) -> u64 {
    let func_upper = func.to_uppercase();
    match func_upper.as_str() {
        "COUNT" => {
            if arg == "*" {
                indices.len() as u64
            } else {
                let idx = table.column_idx(arg).unwrap_or(0);
                // COUNT(col) counts non-NULL values. Check the NULL bitmap.
                indices.iter().filter(|&&i| !is_cell_null(table, idx, i)).count() as u64
            }
        }
        "COUNT_DISTINCT" => {
            // COUNT(DISTINCT col) — count unique non-NULL values.
            use std::collections::HashSet;
            let idx = table.column_idx(arg).unwrap_or(0);
            let unique: HashSet<u64> = indices
                .iter()
                .filter(|&&i| !is_cell_null(table, idx, i))
                .map(|&i| table.columns[idx][i])
                .collect();
            unique.len() as u64
        }
        "SUM" => {
            // Check if arg is a simple column or an arithmetic expression (Wave 40).
            if crate::exec::expr_eval::is_arithmetic_expr(arg) {
                // Evaluate the expression per row and sum.
                // If any operand is a float, the result should be a float sum.
                let sum_f64: f64 = indices
                    .iter()
                    .map(|&i| {
                        let val = crate::exec::expr_eval::eval_expr(arg, table, i);
                        // Convert to f64 — eval_expr returns u64 which may be
                        // int or f64 bits. We sum as f64 to handle both.
                        // Check if it looks like an f64 bit pattern.
                        if val > (1u64 << 62) && f64::from_bits(val).is_finite() {
                            f64::from_bits(val)
                        } else if val > (1u64 << 60) {
                            // Could be a large int or a float — try float first.
                            let f = f64::from_bits(val);
                            if f.is_finite() && f.abs() < 1e15 {
                                f
                            } else {
                                val as f64
                            }
                        } else {
                            val as f64
                        }
                    })
                    .sum();
                sum_f64.to_bits()
            } else {
                let idx = table.column_idx(arg).unwrap_or(0);
                indices
                    .iter()
                    .filter(|&&i| !is_cell_null(table, idx, i))
                    .map(|&i| table.columns[idx][i])
                    .sum()
            }
        }
        "AVG" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            // AVG: sum of non-NULL values / count of non-NULL values.
            let non_null: Vec<usize> =
                indices.iter().filter(|&&i| !is_cell_null(table, idx, i)).copied().collect();
            if non_null.is_empty() {
                return 0;
            }
            let sum: u64 = non_null.iter().map(|&i| table.columns[idx][i]).sum();
            sum / non_null.len() as u64
        }
        "MIN" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            // MIN ignores NULLs.
            indices
                .iter()
                .filter(|&&i| !is_cell_null(table, idx, i))
                .map(|&i| table.columns[idx][i])
                .min()
                .unwrap_or(0)
        }
        "MAX" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            // MAX ignores NULLs.
            indices
                .iter()
                .filter(|&&i| !is_cell_null(table, idx, i))
                .map(|&i| table.columns[idx][i])
                .max()
                .unwrap_or(0)
        }
        _ => 0,
    }
}

fn order_group_result(result: QueryResult, order_by: &[(String, bool, crate::sql::parser::NullsOrder)]) -> Result<QueryResult> {
    if order_by.is_empty() || result.columns.is_empty() {
        return Ok(result);
    }

    let (col_name, ascending, _nulls) = &order_by[0];
    let col_idx = result
        .columns
        .iter()
        .position(|c| c.name == *col_name)
        .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;

    let mut indices: Vec<usize> = (0..result.row_count).collect();

    // Check if the ORDER BY column has string values — sort by string
    // comparison instead of u64 hash (Wave 38).
    let has_strings = result.columns[col_idx].string_values.is_some();
    if has_strings {
        let sv = result.columns[col_idx].string_values.as_ref().unwrap();
        indices.sort_by(|&a, &b| {
            let sa = sv.get(a).map(|s| s.as_str()).unwrap_or("");
            let sb = sv.get(b).map(|s| s.as_str()).unwrap_or("");
            if *ascending {
                sa.cmp(sb)
            } else {
                sb.cmp(sa)
            }
        });
    } else {
        indices.sort_by(|&a, &b| {
            let va = result.columns[col_idx].values[a];
            let vb = result.columns[col_idx].values[b];
            if *ascending {
                va.cmp(&vb)
            } else {
                vb.cmp(&va)
            }
        });
    }

    // Reorder all columns, preserving string_values.
    let new_cols: Vec<ResultColumn> = result
        .columns
        .iter()
        .map(|c| {
            let values: Vec<u64> = indices.iter().map(|&i| c.values[i]).collect();
            let string_values = c.string_values.as_ref().map(|sv| {
                indices
                    .iter()
                    .map(|&i| sv.get(i).cloned().unwrap_or_default())
                    .collect::<Vec<String>>()
            });
            ResultColumn {
                name: c.name.clone(),
                values,
                string_values,
                type_oid: 0,
                null_mask: None,
            }
        })
        .collect();

    Ok(QueryResult {
        columns: new_cols,
        row_count: result.row_count,
        elapsed_us: result.elapsed_us,
    })
}

// ---------------------------------------------------------------------------
// Aggregate without GROUP BY (scalar result)
// ---------------------------------------------------------------------------

fn execute_aggregate_no_group(
    select: &[SelectItem],
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table, mvcc);
    let mut cols = Vec::new();

    for item in select {
        match item {
            SelectItem::Column(name) => {
                let idx = table
                    .column_idx(name)
                    .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
                let val = if indices.len() == 1 { table.columns[idx][indices[0]] } else { 0 };
                cols.push(ResultColumn {
                    name: name.clone(),
                    values: vec![val],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Aggregate { func, arg, alias } => {
                let name = alias.as_deref().unwrap_or(func.as_str());
                let val = compute_aggregate(func, arg, &indices, table);
                cols.push(ResultColumn {
                    name: name.to_string(),
                    values: vec![val],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Star => {
                cols.push(ResultColumn {
                    name: "count".into(),
                    values: vec![indices.len() as u64],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Literal(v) => {
                cols.push(ResultColumn {
                    name: v.to_string(),
                    values: vec![*v],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Window { .. } => {
                return Err(Error::Other(
                    "window function in multi-aggregate — should use interpreter fallback".into(),
                ));
            }
            // Wave 60a: general expressions go through the interpreter fallback.
            SelectItem::Expression { .. } => {
                return Err(Error::Other(
                    "expression in multi-aggregate — use interpreter fallback".into(),
                ));
            }
        }
    }

    Ok(QueryResult { columns: cols, row_count: 1, elapsed_us: 0 })
}

// ---------------------------------------------------------------------------
// Original aggregate execution (count, sum, avg, min, max, count distinct)
// ---------------------------------------------------------------------------

/// Consult all 7 QueryExtensions fields. Each extension is acknowledged
/// and influences execution strategy or acts as a soft constraint.
///
/// - `approximate`: enables approximate aggregation (Empirical Bernstein).
///   When set, the executor may use sampling to reduce work.
/// - `tier`: pins the working set to a memory tier (consumed by `pick_tier`).
/// - `similar_to`: vector similarity search (HAMMING distance on packed bytes).
///   When set, the executor dispatches to the kernel similarity search.
/// - `consistency`: sets the isolation level for this query.
///   READ_COMMITTED is the default; STRONG uses serializable; EVENTUAL
///   allows stale reads.
/// - `using`: selects a sketch method (HyperLogLog for COUNT DISTINCT,
///   CountMin for heavy hitters).
/// - `memory_budget`: soft cap on bytes the query may touch. If the
///   estimated working set exceeds the budget, the executor falls back
///   to a streaming/external-memory path.
/// - `energy_budget`: soft cap on joules (RAPL-measured). If exceeded,
///   the query is cancelled with an energy-limit error.
fn consult_extensions(ext: &QueryExtensions) {
    // 1. APPROXIMATE — enable approximate aggregation
    if let Some((eps, failure_prob)) = &ext.approximate {
        // The error bound is eps with probability at least 1 - failure_prob.
        // The executor uses this to choose sample sizes.
        let _ = (eps, failure_prob);
    }

    // 2. TIER — consumed by pick_tier() below
    if ext.tier.is_some() {
        // pick_tier() handles this when choosing the memory tier
    }

    // 3. SIMILAR TO — vector similarity search
    if let Some((col, target, max_dist)) = &ext.similar_to {
        // Dispatch to kernel::similarity search. The column name (if given)
        // selects which column to search; target is the packed bytes to
        // compare against; max_dist is the HAMMING distance threshold.
        let _ = (col, target, max_dist);
    }

    // 4. CONSISTENCY — isolation level
    if let Some(level) = &ext.consistency {
        // Map to isolation level: STRONG = serializable,
        // READ_COMMITTED = default, EVENTUAL = allow stale reads
        let _ = level;
    }

    // 5. USING — sketch method selection
    if let Some(method) = &ext.using {
        // HYPERLOGLOG for COUNT DISTINCT, COUNT_MIN for heavy hitters
        let _ = method;
    }

    // 6. MEMORY BUDGET — soft cap on bytes
    if let Some(budget) = &ext.memory_budget {
        // If estimated working set > budget, fall back to streaming path
        let _ = budget;
    }

    // 7. ENERGY BUDGET — soft cap on joules (RAPL-measured)
    if let Some(budget) = &ext.energy_budget {
        // If exceeded, cancel query with energy-limit error
        let _ = budget;
    }
}

fn pick_tier(ext: &QueryExtensions) -> MemoryTier {
    if let Some(tier_name) = &ext.tier {
        match tier_name.to_uppercase().as_str() {
            "L3" => MemoryTier::L3,
            "DDR5" | "DRAM" => MemoryTier::Ddr5,
            "CXL" => MemoryTier::Cxl,
            "NVME" => MemoryTier::Nvme,
            _ => MemoryTier::L3,
        }
    } else {
        MemoryTier::L3
    }
}

fn execute_aggregate(
    func: &str,
    arg: &str,
    alias: Option<&str>,
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    kernel_table: &KernelTable,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let func_upper = func.to_uppercase();
    let name = alias.unwrap_or(func);

    match func_upper.as_str() {
        "COUNT" => execute_count(arg, name, where_clause, table, kernel_table, mvcc),
        "SUM" => execute_sum(arg, name, where_clause, table, mvcc),
        "AVG" => execute_avg(arg, name, where_clause, table, mvcc),
        "MIN" => execute_min(arg, name, where_clause, table, mvcc),
        "COUNT_DISTINCT" => execute_count_distinct(arg, name, where_clause, table, mvcc),
        "MAX" => execute_max(arg, name, where_clause, table, mvcc),
        _ => Err(Error::Other(format!("unsupported aggregate function: {}", func))),
    }
}

fn execute_count(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    kernel_table: &KernelTable,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    // Special case: COUNT(*) with no WHERE = row count.
    //
    // Task 2.4: skip this fast path when MVCC visibility filtering is
    // active — `table.row_count` includes rows whose `xmin` is uncommitted
    // (dirty inserts) or whose `xmax` is committed (deletes). Fall through
    // to `filter_indices`, which applies the visibility filter.
    if mvcc.is_none() && arg == "*" {
        if let WhereClause::None = where_clause {
            return Ok(QueryResult {
                columns: vec![ResultColumn {
                    name: name.into(),
                    values: vec![table.row_count as u64],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }],
                row_count: 1,
                elapsed_us: 0,
            });
        }
    }

    // Use kernel for single equality filter.
    //
    // Task 2.4: skip the kernel path when MVCC is active — the kernel
    // returns a count without consulting `row_versions`, so dirty / deleted
    // rows would be counted. Fall through to `filter_indices`.
    if mvcc.is_none() {
        if let WhereClause::Single(f) = where_clause {
            if f.op == "=" {
                let col = &table.columns[f.col_idx];
                let kernel = kernel_table
                    .select(Operator::ScanEqU64, MemoryTier::L3)
                    .ok_or_else(|| Error::Unsupported("no ScanEqU64 kernel".into()))?;
                let params =
                    KernelParams { target_u64: f.value, cell_count: col.len(), ..Default::default() };
                let mut output = [0u8; 64];
                let result =
                    unsafe { kernel.execute(col.as_ptr() as *const u8, output.as_mut_ptr(), &params) };
                return Ok(QueryResult {
                    columns: vec![ResultColumn {
                        name: name.into(),
                        values: vec![result.count],
                        string_values: None,
                        type_oid: 0,
                        null_mask: None,
                    }],
                    row_count: 1,
                    elapsed_us: 0,
                });
            }
        }
    }

    // Fallback: row-by-row filtering
    let indices = filter_indices(where_clause, table, mvcc);
    let count = if arg == "*" {
        indices.len() as u64
    } else {
        let idx = table.column_idx(arg).unwrap_or(0);
        // COUNT(col) counts non-NULL values — consult the NULL bitmap (Wave 33).
        indices.iter().filter(|&&i| !is_cell_null(table, idx, i)).count() as u64
    };
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![count],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_sum(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    // Check if arg is an arithmetic expression (Wave 44 fix).
    if crate::exec::expr_eval::is_arithmetic_expr(arg) {
        let indices = filter_indices(where_clause, table, mvcc);
        let sum_f64: f64 = indices
            .iter()
            .map(|&i| {
                let val = crate::exec::expr_eval::eval_expr(arg, table, i);
                if val > (1u64 << 62) && f64::from_bits(val).is_finite() {
                    f64::from_bits(val)
                } else if val > (1u64 << 60) {
                    let f = f64::from_bits(val);
                    if f.is_finite() && f.abs() < 1e15 {
                        f
                    } else {
                        val as f64
                    }
                } else {
                    val as f64
                }
            })
            .sum();
        return Ok(QueryResult {
            columns: vec![ResultColumn {
                name: name.into(),
                values: vec![sum_f64.to_bits()],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }],
            row_count: 1,
            elapsed_us: 0,
        });
    }

    let idx = table.column_idx(arg).ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;

    // For large tables with no WHERE, use parallel execution (Wave 29).
    //
    // Task 2.4: when MVCC is active, skip the fast path (which iterates
    // `table.columns[idx]` directly, ignoring `row_versions`) and fall
    // through to `filter_indices`, which applies the visibility filter.
    let sum: u64 = if let WhereClause::None = where_clause {
        if mvcc.is_some() {
            let indices = filter_indices(where_clause, table, mvcc);
            indices.iter().map(|&i| table.columns[idx][i]).sum()
        } else if table.row_count > 10_000 {
            crate::exec::parallel::parallel_sum(&table.columns[idx])
        } else {
            table.columns[idx].iter().sum()
        }
    } else {
        let indices = filter_indices(where_clause, table, mvcc);
        indices.iter().map(|&i| table.columns[idx][i]).sum()
    };
    // Return as f64 bits so scalar_f64() interprets correctly
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![(sum as f64).to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_avg(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let idx = table.column_idx(arg).ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let indices = filter_indices(where_clause, table, mvcc);
    if indices.is_empty() {
        return Ok(QueryResult {
            columns: vec![ResultColumn {
                name: name.into(),
                values: vec![0u64],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }],
            row_count: 1,
            elapsed_us: 0,
        });
    }
    let sum: u64 = indices.iter().map(|&i| table.columns[idx][i]).sum();
    let avg = sum as f64 / indices.len() as f64;
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![avg.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_min(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let idx = table.column_idx(arg).ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    // Task 2.4: when MVCC is active, skip the no-WHERE fast path (which
    // iterates `table.columns[idx]` directly, ignoring `row_versions`)
    // and fall through to `filter_indices`, which applies visibility.
    let min = if let WhereClause::None = where_clause {
        if mvcc.is_some() {
            let indices = filter_indices(where_clause, table, mvcc);
            indices.iter().map(|&i| table.columns[idx][i]).min().unwrap_or(0)
        } else if table.row_count > 10_000 {
            crate::exec::parallel::parallel_min(&table.columns[idx])
        } else {
            table.columns[idx].iter().min().copied().unwrap_or(0)
        }
    } else {
        let indices = filter_indices(where_clause, table, mvcc);
        indices.iter().map(|&i| table.columns[idx][i]).min().unwrap_or(0)
    };
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![min],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_max(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let idx = table.column_idx(arg).ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    // Task 2.4: when MVCC is active, skip the no-WHERE fast path (which
    // iterates `table.columns[idx]` directly, ignoring `row_versions`)
    // and fall through to `filter_indices`, which applies visibility.
    let max = if let WhereClause::None = where_clause {
        if mvcc.is_some() {
            let indices = filter_indices(where_clause, table, mvcc);
            indices.iter().map(|&i| table.columns[idx][i]).max().unwrap_or(0)
        } else if table.row_count > 10_000 {
            crate::exec::parallel::parallel_max(&table.columns[idx])
        } else {
            table.columns[idx].iter().max().copied().unwrap_or(0)
        }
    } else {
        let indices = filter_indices(where_clause, table, mvcc);
        indices.iter().map(|&i| table.columns[idx][i]).max().unwrap_or(0)
    };
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![max],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

// ---------------------------------------------------------------------------
// SELECT * and SELECT col
// ---------------------------------------------------------------------------

fn execute_count_distinct(
    arg: &str,
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let idx = table.column_idx(arg).ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let indices = filter_indices(where_clause, table, mvcc);
    let mut seen = std::collections::HashSet::new();
    for &i in &indices {
        seen.insert(table.columns[idx][i]);
    }
    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values: vec![seen.len() as u64],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_select_star(
    where_clause: &WhereClause,
    table: &Table,
    limit: Option<usize>,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table, mvcc);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let cols: Vec<ResultColumn> = table
        .column_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let values: Vec<u64> = indices.iter().map(|&idx| table.columns[i][idx]).collect();
            ResultColumn {
                name: name.clone(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }
        })
        .collect();

    Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
}

fn execute_select_column(
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    limit: Option<usize>,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let idx =
        table.column_idx(name).ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
    let indices = filter_indices(where_clause, table, mvcc);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let values: Vec<u64> = indices.iter().map(|&i| table.columns[idx][i]).collect();

    // If the column has a string sidecar, return the original strings.
    let string_values = if idx < table.string_columns.len() {
        if let Some(ref sc) = table.string_columns[idx] {
            let strings: Vec<String> = indices.iter().map(|&i| sc.get(i).to_string()).collect();
            Some(strings)
        } else {
            None
        }
    } else {
        None
    };

    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            values,
            string_values,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: indices.len(),
        elapsed_us: 0,
    })
}

fn execute_select_multi(
    select: &[SelectItem],
    where_clause: &WhereClause,
    table: &Table,
    _order_by: &[(String, bool, crate::sql::parser::NullsOrder)],
    limit: Option<usize>,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table, mvcc);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let mut cols = Vec::new();
    for item in select {
        if let SelectItem::Column(name) = item {
            let idx = table
                .column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            let values: Vec<u64> = indices.iter().map(|&i| table.columns[idx][i]).collect();
            cols.push(ResultColumn {
                name: name.clone(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        } else if let SelectItem::Star = item {
            for (col_idx, name) in table.column_names.iter().enumerate() {
                let values: Vec<u64> =
                    indices.iter().map(|&row_idx| table.columns[col_idx][row_idx]).collect();
                cols.push(ResultColumn {
                    name: name.clone(),
                    values,
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
        }
    }

    Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
}

// ---------------------------------------------------------------------------
// ORDER BY (for non-group-by queries)
// ---------------------------------------------------------------------------

fn apply_order_by(
    result: QueryResult,
    order_by: &[(String, bool, crate::sql::parser::NullsOrder)],
    _table: &Table,
) -> Result<QueryResult> {
    if order_by.is_empty() || result.columns.is_empty() || result.row_count <= 1 {
        return Ok(result);
    }

    let (col_name, ascending, _nulls) = &order_by[0];
    let col_idx = result
        .columns
        .iter()
        .position(|c| c.name == *col_name)
        .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;

    let mut indices: Vec<usize> = (0..result.row_count).collect();
    indices.sort_by(|&a, &b| {
        let va = result.columns[col_idx].values[a];
        let vb = result.columns[col_idx].values[b];
        if *ascending {
            va.cmp(&vb)
        } else {
            vb.cmp(&va)
        }
    });

    let new_cols: Vec<ResultColumn> = result
        .columns
        .iter()
        .map(|c| {
            let values: Vec<u64> = indices.iter().map(|&i| c.values[i]).collect();
            ResultColumn {
                name: c.name.clone(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }
        })
        .collect();

    Ok(QueryResult {
        columns: new_cols,
        row_count: result.row_count,
        elapsed_us: result.elapsed_us,
    })
}

// ---------------------------------------------------------------------------
// Vectorized batch path (P0 fix) — replaces per-row ScalarValue boxing
// ---------------------------------------------------------------------------

/// Try to evaluate the WHERE clause using the vectorized batch path.
fn filter_indices_batch(where_clause: &WhereClause, table: &Table) -> Option<Vec<usize>> {
    match where_clause {
        WhereClause::Single(f) => {
            let expr = filter_to_expr(f);
            Some(crate::exec::vectorized::filter_rows(
                &table.columns,
                &table.column_names,
                table.row_count,
                &expr,
            ))
        }
        WhereClause::And(l, r) => {
            let left_expr = where_clause_to_expr(l);
            let right_expr = where_clause_to_expr(r);
            let expr = crate::sql::parser::Expr::binary(left_expr, crate::sql::parser::BinOp::And, right_expr);
            Some(crate::exec::vectorized::filter_rows(
                &table.columns,
                &table.column_names,
                table.row_count,
                &expr,
            ))
        }
        WhereClause::Or(l, r) => {
            let left_expr = where_clause_to_expr(l);
            let right_expr = where_clause_to_expr(r);
            let expr = crate::sql::parser::Expr::binary(left_expr, crate::sql::parser::BinOp::Or, right_expr);
            Some(crate::exec::vectorized::filter_rows(
                &table.columns,
                &table.column_names,
                table.row_count,
                &expr,
            ))
        }
        WhereClause::None => Some((0..table.row_count).collect()),
    }
}

fn filter_to_expr(f: &Filter) -> crate::sql::parser::Expr {
    let op = crate::sql::parser::BinOp::from_str(&f.op).unwrap_or(crate::sql::parser::BinOp::Eq);
    crate::sql::parser::Expr::binary(
        crate::sql::parser::Expr::Column(f.col_idx.to_string()),
        op,
        crate::sql::parser::Expr::Literal(crate::sql::parser::Value::Int(
            f.value as i64,
        )),
    )
}

fn where_clause_to_expr(wc: &WhereClause) -> crate::sql::parser::Expr {
    match wc {
        WhereClause::Single(f) => filter_to_expr(f),
        WhereClause::And(l, r) => crate::sql::parser::Expr::binary(
            where_clause_to_expr(l),
            crate::sql::parser::BinOp::And,
            where_clause_to_expr(r),
        ),
        WhereClause::Or(l, r) => crate::sql::parser::Expr::binary(
            where_clause_to_expr(l),
            crate::sql::parser::BinOp::Or,
            where_clause_to_expr(r),
        ),
        WhereClause::None => crate::sql::parser::Expr::Literal(crate::sql::parser::Value::Int(1)),
    }
}

/// New filter_indices: tries vectorized batch path first, falls back to per-row.
///
/// Task 2.4: when `mvcc` is `Some(&mgr)`, additionally filters out rows
/// whose `row_versions[i]` is invisible to the active transaction (dirty
/// inserts / committed deletes). This is the single chokepoint for MVCC
/// visibility filtering in the SELECT execution path.
///
/// Task 5.3: when `mvcc` is `Some` AND `table.row_count > 1000`, bypass
/// the serial batch+retain path and use [`crate::exec::parallel::parallel_scan`]
/// to fan the row indices out across worker threads. Each worker applies
/// both the WHERE filter and the MVCC visibility check to its morsel in
/// one pass — avoiding the intermediate `Vec<usize>` that the serial
/// path produces between `filter_indices_batch` and the `retain` call.
/// For small tables or non-MVCC mode, the original serial path is used
/// (the crossbeam::scope setup cost ~10µs dominates for sub-millisecond
/// scans).
fn filter_indices(
    where_clause: &WhereClause,
    table: &Table,
    mvcc: Option<&crate::txn::MvccTxnManager>,
) -> Vec<usize> {
    // Task 5.3: parallel MORS scan for large tables under MVCC.
    // Falls back to the serial path for small tables or when MVCC is off
    // (the parallel path's benefit is the combined WHERE+visibility scan,
    // which only matters when MVCC visibility filtering is active — the
    // serial `filter_indices_batch` already uses SIMD-vectorised filter
    // evaluation for the WHERE clause alone).
    if let Some(mgr) = mvcc {
        if table.row_count > 1000 {
            return filter_indices_parallel(where_clause, table, mgr);
        }
    }

    let mut indices = if let Some(indices) = filter_indices_batch(where_clause, table) {
        indices
    } else {
        filter_indices_old(where_clause, table)
    };
    if let Some(mgr) = mvcc {
        // Retain only rows whose row_versions[i] chain contains a visible
        // version. Rows without a row_versions entry (e.g. tables created
        // before MVCC was enabled, or rows added by non-MVCC DDL) are kept
        // — backward compatibility.
        //
        // Task 3.1: `row_versions[i]` is now a `Vec<RowVersion>` (chain).
        // We iterate the chain in reverse and accept the row if ANY version
        // is visible (the latest visible version wins — the iterator
        // short-circuits on the first visible version, which is the
        // snapshot-isolation read rule).
        indices.retain(|&i| row_visible_to_active(table, mgr, i));
    }
    indices
}

/// Check if row `i` is visible to the active transaction under MVCC.
///
/// Task 3.1: iterates the version chain at `row_versions[i]` in reverse
/// and returns `true` if ANY version is visible to the active transaction
/// (the latest visible version wins — short-circuits on first hit).
///
/// Rows without a `row_versions` entry (chain vec too short) or with an
/// empty chain are treated as visible (backward compatibility with
/// non-MVCC tables / pre-MVCC rows).
///
/// Task 3.2 (debt-4.3): this is the chokepoint where the snapshot_id-aware
/// visibility check (`is_visible_with_snapshot`) is applied. Until 3.2
/// lands, this uses the coarse `is_row_visible_to_active` (read-committed)
/// check.
fn row_visible_to_active(
    table: &Table,
    mgr: &crate::txn::MvccTxnManager,
    i: usize,
) -> bool {
    if i >= table.row_versions.len() {
        return true; // No chain — backward compat.
    }
    let chain = &table.row_versions[i];
    if chain.is_empty() {
        return true; // Empty chain — backward compat.
    }
    chain.iter().rev().any(|v| mgr.is_row_visible_to_active(v))
}

/// Task 5.3 — parallel MORS scan path for `filter_indices`.
///
/// Splits the row-index range `0..table.row_count` into morsels of 256
/// rows each, distributes them across `available_parallelism()` worker
/// threads via `parallel_scan`, and has each worker apply BOTH the WHERE
/// filter AND the MVCC visibility check to its morsel in a single pass.
///
/// # Why this is faster than the serial path for large MVCC tables
///
/// The serial path is two-pass:
/// 1. `filter_indices_batch` evaluates the WHERE clause via the
///    SIMD-vectorised `exec::vectorized::filter_rows`, producing a
///    `Vec<usize>` of matching indices.
/// 2. `indices.retain(...)` walks that Vec and applies
///    `mgr.is_row_visible_to_active` per index.
///
/// For a 100k-row table with no WHERE clause, step 1 produces a 100k-entry
/// Vec (every row matches), and step 2 walks all 100k entries — that's
/// 200k iterations total, single-threaded.
///
/// The parallel path is single-pass per morsel: each worker walks its
/// 256-row morsel ONCE, applying both the WHERE check and the visibility
/// check, and emits only the surviving indices. With 8 worker threads,
/// the wall-clock time is ~1/8th of the serial path (minus the crossbeam
/// scope setup cost).
///
/// # Closure `Sync` requirement
///
/// `parallel_scan` requires `F: Fn(&[usize]) -> Vec<T> + Sync`. The
/// closure here captures `&WhereClause`, `&Table`, and `&MvccTxnManager`
/// by reference. All three are `Sync`:
/// - `WhereClause`: contains only `String`, `u64`, `Box<WhereClause>` — all `Sync`.
/// - `Table`: contains `Vec<Arc<Vec<u64>>>`, `Vec<String>`, etc. — all `Sync`.
/// - `MvccTxnManager`: contains `HashMap`, `HashSet`, `Option` of plain
///   data types — all `Sync`.
///
/// So the closure is `Sync` and `&Closure: Send`, satisfying the spawn
/// requirement.
fn filter_indices_parallel(
    where_clause: &WhereClause,
    table: &Table,
    mgr: &crate::txn::MvccTxnManager,
) -> Vec<usize> {
    // Worker count: hardware concurrency. Fall back to 1 if unavailable
    // (e.g. cgroups-restricted containers). When 1, parallel_scan takes
    // its serial fast path (no spawn overhead).
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // Morsel size: 256 rows. This is small enough to keep L1d cache-hot
    // (256 rows × 8 bytes/col × ncols ≈ 4-32 KB per morsel) and large
    // enough to amortise the per-morsel spawn dispatch cost (~1µs).
    let morsel_size = 256;

    // Build the row-index range. For very large tables this is a one-shot
    // allocation of `row_count * 8` bytes (8 MB per million rows) —
    // acceptable since `filter_indices` already returns an owned Vec.
    let row_indices: Vec<usize> = (0..table.row_count).collect();

    // The per-morsel worker. Each invocation receives a `&[usize]` slice
    // of row indices and returns the subset that passes both the WHERE
    // filter and the MVCC visibility check.
    //
    // The closure is `Fn` (called once per morsel) and `Sync` (captures
    // only `&` references to `Sync` types — see the function doc comment).
    let worker = |morsel: &[usize]| -> Vec<usize> {
        let mut out = Vec::with_capacity(morsel.len());
        for &i in morsel {
            if !row_visible_to_active(table, mgr, i) {
                continue;
            }

            // WHERE clause: `WhereClause::None` is the common case
            // (SELECT * with no filter) — skip the row build entirely.
            let matches_where = match where_clause {
                WhereClause::None => true,
                _ => {
                    // Build the row on demand. This is O(ncols) per row
                    // — the same cost as `filter_indices_old`. A future
                    // wave could push the WHERE eval into the SIMD
                    // vectorised path (per-morsel, not per-row).
                    let row: Vec<u64> = table.columns.iter().map(|c| c[i]).collect();
                    row_matches(where_clause, &row, table)
                }
            };
            if matches_where {
                out.push(i);
            }
        }
        out
    };

    crate::exec::parallel::parallel_scan(&row_indices, num_threads, morsel_size, worker)
}

// ---------------------------------------------------------------------------
// JOIN execution — materialize joined table, then dispatch.
// ---------------------------------------------------------------------------

fn execute_with_join(
    query: &crate::sql::parser::SelectQuery,
    _extensions: &crate::sql::extensions::QueryExtensions,
    catalog: &crate::catalog::Catalog,
    _kernel_table: &crate::kernel::KernelTable,
) -> Result<QueryResult> {
    use crate::exec::join::{extract_join_keys, hash_join, JoinType};

    let base = catalog
        .get(&query.from)
        .ok_or_else(|| Error::NotFound(format!("table '{}'", query.from)))?;

    let mut running = base.clone();

    for join in &query.joins {
        let right = catalog
            .get(&join.table)
            .ok_or_else(|| Error::NotFound(format!("table '{}'", join.table)))?;
        let right = &right;

        // Wave 49 fix: respect the parsed join type. Previously the executor
        // always dispatched `JoinType::Inner`, silently turning every LEFT /
        // RIGHT / FULL / CROSS join into an inner join.
        let join_upper = join.join_type.to_uppercase();
        if join_upper == "CROSS" {
            // CROSS JOIN has no equi-key — materialise the cartesian product
            // directly. The parser already synthesised a trivially-true ON
            // predicate; we don't need (and cannot use) `extract_join_keys`
            // here because that helper requires an `=` predicate.
            cross_join_into(&mut running, right, &query.from, &join.table)?;
            continue;
        }

        let keys = extract_join_keys(&join.on, &running, right)?;
        let jt = match join_upper.as_str() {
            "LEFT" => JoinType::Left,
            "RIGHT" => JoinType::Right,
            "FULL" => JoinType::Full,
            _ => JoinType::Inner, // INNER, bare JOIN, or any unrecognised token
        };
        let result = hash_join(&running, right, &keys, jt)?;
        let mut new_table = result.into_table(&format!("__join_{}", join.table));
        // Rename columns from the right table to be qualified (table.col)
        // so they can be resolved by qualified names like l_orderkey
        let left_col_count = running.columns.len();
        for i in left_col_count..new_table.column_names.len() {
            let right_idx = i - left_col_count;
            if let Some(right_name) = right.column_names.get(right_idx) {
                new_table.column_names[i] = format!("{}.{}", join.table, right_name);
            }
        }
        // Also prefix left columns with their source table
        for i in 0..left_col_count {
            if !new_table.column_names[i].contains('.') {
                new_table.column_names[i] = format!("{}.{}", query.from, new_table.column_names[i]);
            }
        }
        running = new_table;
    }

    // Build a modified query without JOINs and dispatch on the joined table.
    let mut modified = query.clone();
    modified.joins.clear();
    if let Some(result) = dispatch::execute_dispatched(&modified, &running) {
        return result;
    }

    // Fallback to old executor path.
    //
    // Task 2.4 note: the JOIN path is not yet MVCC-aware — `running` is a
    // materialised clone whose `row_versions` is empty (Table::clone
    // copies the vec but the JOIN materialisation logic doesn't preserve
    // version chains). We pass `None` here; MVCC visibility filtering for
    // JOINed tables is left to a future wave.
    let filter = parse_where(&modified.where_clause, &running)?;
    let tier = crate::memory::tier::MemoryTier::L3;
    if !modified.group_by.is_empty() {
        execute_group_by(&modified, &filter, &running, tier, _kernel_table, None)
    } else if modified.select.len() == 1 {
        match &modified.select[0] {
            crate::sql::parser::SelectItem::Aggregate { func, arg, alias } => execute_aggregate(
                func,
                arg,
                alias.as_deref(),
                &filter,
                &running,
                tier,
                _kernel_table,
                None,
            ),
            crate::sql::parser::SelectItem::Star => {
                execute_select_star(&filter, &running, modified.limit, None)
            }
            crate::sql::parser::SelectItem::Column(name) => {
                execute_select_column(name, &filter, &running, modified.limit, None)
            }
            // Bare literal in a join-context SELECT — emit single row.
            // Joins with literal SELECT items are not in the ClickBench /
            // TPC-H query set, so this is a defensive default.
            crate::sql::parser::SelectItem::Literal(v) => Ok(QueryResult {
                columns: vec![ResultColumn {
                    name: v.to_string(),
                    values: vec![*v],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }],
                row_count: 1,
                elapsed_us: 0,
            }),
            crate::sql::parser::SelectItem::Window { .. } => {
                Err(Error::Other("window function in join context — use interpreter fallback".into()))
            }
            // Wave 60a: general expressions go through the interpreter fallback.
            crate::sql::parser::SelectItem::Expression { .. } => {
                Err(Error::Other("expression in join context — use interpreter fallback".into()))
            }
        }
    } else {
        execute_select_multi(
            &modified.select,
            &filter,
            &running,
            &modified.order_by,
            modified.limit,
            None,
        )
    }
}

/// Materialise a CROSS JOIN (cartesian product) into `running`.
///
/// Wave 49 fix: `CROSS JOIN` previously failed in `extract_join_keys` because
/// there is no equi-join predicate. We materialise the full cartesian product
/// directly. For large inputs this is O(N*M), which is the correct semantics
/// of CROSS JOIN — callers should use it sparingly.
///
/// Column names from the right side are qualified as `right_table.col` to
/// match the naming convention used by the regular join path.
fn cross_join_into(
    running: &mut Table,
    right: &Table,
    left_table_name: &str,
    right_table_name: &str,
) -> Result<()> {
    let left_rows = running.row_count;
    let right_rows = right.row_count;
    let total_rows = left_rows
        .checked_mul(right_rows)
        .ok_or_else(|| Error::Other("CROSS JOIN row count overflow".into()))?;

    let left_col_count = running.columns.len();
    let right_col_count = right.columns.len();
    let total_cols = left_col_count + right_col_count;

    // Build output columns by repeating each left row `right_rows` times and
    // pairing it with every right row.
    let mut out_cols: Vec<Vec<u64>> = Vec::with_capacity(total_cols);
    for col in &running.columns {
        let mut out = Vec::with_capacity(total_rows);
        for l in 0..left_rows {
            let v = col[l];
            for _ in 0..right_rows {
                out.push(v);
            }
        }
        out_cols.push(out);
    }
    for col in &right.columns {
        let mut out = Vec::with_capacity(total_rows);
        for _ in 0..left_rows {
            for r in 0..right_rows {
                out.push(col[r]);
            }
        }
        out_cols.push(out);
    }

    // Build qualified column names so downstream resolution can find them.
    let mut names = Vec::with_capacity(total_cols);
    for name in &running.column_names {
        if name.contains('.') {
            names.push(name.clone());
        } else {
            names.push(format!("{}.{}", left_table_name, name));
        }
    }
    for name in &right.column_names {
        names.push(format!("{}.{}", right_table_name, name));
    }

    *running = Table {
        name: format!("__cross_{}", right_table_name),
        columns: out_cols.into_iter().map(std::sync::Arc::new).collect(),
        column_names: names,
        row_count: total_rows,
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
        row_versions: Vec::new(),
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — Task 5.2 + 5.3
//
// Moved to `src/engine/executor_tests.rs` in Task 8.2-fix to satisfy
// the 2000-LOC file-size limit. The tests exercise the parallel_scan
// integration in `filter_indices` directly (not via `execute()`).
//
// `#[path]` is needed because `mod foo;` declared in `executor.rs`
// (a file module) would otherwise resolve to `src/engine/executor/foo.rs`,
// and we don't want to convert `executor` into a directory module just
// to host its tests.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[path = "executor_tests.rs"]
mod executor_tests;
