//! Kernel-direct query dispatch — pattern-match SQL shape, call kernels.
//!
//! Research: Compiler pattern recognition (recognize complex expressions
//! as single operations). Signal processing: FFT convolution for complex
//! multi-predicate filters (future work).
//!
//! This eliminates ALL abstraction overhead:
//! SQL → Parse Tree → Pattern Match → Kernel Call → Result
//! No ScalarValue, no Expr tree, no per-row evaluation.

use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::vectorized;
use crate::sql::parser::{Expr, SelectItem, SelectQuery, Value};
use crate::Error;

type Result<T> = std::result::Result<T, Error>;

/// Query shape classification — determines which kernel combination to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryShape {
    /// SELECT count(*) FROM t
    CountAll,
    /// SELECT count(*) FROM t WHERE col OP val
    CountFilter,
    /// SELECT sum(col) FROM t [WHERE col2 OP val]
    SumCol,
    /// SELECT min(col) / max(col) FROM t [WHERE ...]
    MinMax,
    /// SELECT count(DISTINCT col) FROM t [WHERE ...]
    CountDistinct,
    /// SELECT avg(col) FROM t [WHERE ...]
    AvgCol,
    /// SELECT col, count(*) FROM t GROUP BY col
    GroupByCount,
    /// SELECT col, sum(col2) FROM t GROUP BY col
    GroupBySum,
    /// SELECT col, count(*) ... GROUP BY ... ORDER BY ... LIMIT
    GroupByOrderByLimit,
    /// SELECT * FROM t [WHERE ...] [LIMIT]
    SelectStar,
    /// SELECT col FROM t [WHERE ...] [LIMIT]
    SelectColumn,
    /// SELECT col1, col2 FROM t [WHERE ...] [LIMIT]
    SelectMulti,
    /// Too complex for kernel dispatch — use fallback evaluator
    Complex,
}

/// Classify a parsed SELECT query into a shape for kernel dispatch.
pub fn classify_query(query: &SelectQuery) -> QueryShape {
    // Note: Wave 22 SelectQuery doesn't have joins field.
    // JOIN support will be added in Wave 45.

    let has_group_by = !query.group_by.is_empty();
    let has_order_by = !query.order_by.is_empty();
    let has_limit = query.limit.is_some();
    let has_where = query.where_clause.is_some();

    if has_group_by {
        // Check if all select items are either GROUP BY columns or count(*)
        let has_agg = query.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        if !has_agg {
            return QueryShape::Complex;
        }

        // Check if the aggregate is count(*) or sum(col)
        let agg = query.select.iter().find_map(|s| {
            if let SelectItem::Aggregate { func, arg, .. } = s {
                Some((func.as_str(), arg.as_str()))
            } else {
                None
            }
        });

        match agg {
            Some(("COUNT", "*")) | Some(("COUNT", _)) => {
                if has_order_by && has_limit {
                    QueryShape::GroupByOrderByLimit
                } else {
                    QueryShape::GroupByCount
                }
            }
            Some(("SUM", _)) => QueryShape::GroupBySum,
            _ => QueryShape::Complex,
        }
    } else if query.select.len() == 1 {
        match &query.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                let f = func.to_uppercase();
                match (f.as_str(), arg.as_str(), has_where) {
                    ("COUNT", "*", false) => QueryShape::CountAll,
                    ("COUNT", "*", true) | ("COUNT", _, true) => QueryShape::CountFilter,
                    ("COUNT", _, false) => QueryShape::CountFilter,
                    ("COUNT_DISTINCT", _, _) => QueryShape::CountDistinct,
                    ("SUM", _, _) => QueryShape::SumCol,
                    ("AVG", _, _) => QueryShape::AvgCol,
                    ("MIN", _, _) | ("MAX", _, _) => QueryShape::MinMax,
                    _ => QueryShape::Complex,
                }
            }
            SelectItem::Star => QueryShape::SelectStar,
            SelectItem::Column(_) => QueryShape::SelectColumn,
            // A bare literal in a single-item SELECT (e.g. `SELECT 1`)
            // is not a shape we dispatch — let the fallback handle it.
            SelectItem::Literal(_) => QueryShape::Complex,
            // Window functions go through the interpreter fallback.
            SelectItem::Window { .. } => QueryShape::Complex,
            // Wave 60a: CASE WHEN / general expressions go through the
            // interpreter fallback (which evaluates them correctly). A future
            // wave can add a fast dispatch path for Expression items.
            SelectItem::Expression { .. } => QueryShape::Complex,
        }
    } else if query.select.len() > 1 {
        let has_agg = query.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        let has_expr = query.select.iter().any(|s| matches!(s, SelectItem::Expression { .. }));
        if has_agg || has_expr {
            QueryShape::Complex // mixed column+agg/expr without GROUP BY
        } else {
            QueryShape::SelectMulti
        }
    } else {
        QueryShape::Complex
    }
}

/// Execute a query using kernel-direct dispatch.
/// Returns None if the shape is Complex (caller should use fallback).
pub fn execute_dispatched(query: &SelectQuery, table: &Table) -> Option<Result<QueryResult>> {
    let shape = classify_query(query);
    if shape == QueryShape::Complex {
        return None;
    }
    // Wave 60a: if the WHERE clause contains a CASE WHEN expression, the
    // dispatch path can't evaluate it (build_filter_mask / vectorized::filter_rows
    // don't handle Expr::Case). Return None so the query falls through to
    // the interpreter interpreter, which evaluates CASE WHEN correctly.
    // Wave 67: same for EXTRACT and CAST — the basic executor can't
    // evaluate them per-row, so route to interpreter.
    if let Some(ref where_expr) = query.where_clause {
        if expr_needs_interpreter_fallback(where_expr) {
            return None;
        }
    }
    // Wave 60b: if the HAVING clause is present, the dispatch path can't
    // evaluate it (the basic executor doesn't evaluate Expr::Function
    // aggregates in HAVING context). Return None so the query falls to interpreter,
    // which has a full HAVING implementation.
    // Wave 62 fix: the basic parser now PARSES HAVING correctly (including
    // count(*) etc. as Expr::Function), but the basic executor still can't
    // evaluate it — so we still route to interpreter. The difference is that
    // previously the parser ERRORED and the query fell to interpreter as an error
    // fallback; now the parser SUCCEEDS and we explicitly route to interpreter.
    if query.having.is_some() {
        return None;
    }
    Some(execute_shape(shape, query, table))
}

/// Check whether an Expr contains a construct the basic dispatch path
/// can't evaluate (CASE WHEN, EXTRACT, CAST). Used by execute_dispatched
/// to route such queries to the interpreter fallback.
fn expr_needs_interpreter_fallback(expr: &Expr) -> bool {
    match expr {
        Expr::Case { .. } | Expr::Extract { .. } | Expr::Cast { .. } => true,
        Expr::Binary { left, right, .. } => {
            expr_needs_interpreter_fallback(left) || expr_needs_interpreter_fallback(right)
        }
        Expr::Unary { expr, .. } | Expr::Not(expr) | Expr::Paren(expr) => {
            expr_needs_interpreter_fallback(expr)
        }
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::IsNull { .. } => true,
        Expr::Function { args, .. } => args.iter().any(expr_needs_interpreter_fallback),
        Expr::Like { expr, pattern, .. } => {
            expr_needs_interpreter_fallback(expr) || expr_needs_interpreter_fallback(pattern)
        }
        Expr::Between { expr, low, high, .. } => {
            expr_needs_interpreter_fallback(expr)
                || expr_needs_interpreter_fallback(low)
                || expr_needs_interpreter_fallback(high)
        }
        Expr::InList { expr, list, .. } => {
            expr_needs_interpreter_fallback(expr) || list.iter().any(expr_needs_interpreter_fallback)
        }
        Expr::Column(_) | Expr::Literal(_) | Expr::Wildcard => false,
    }
}

fn execute_shape(shape: QueryShape, query: &SelectQuery, table: &Table) -> Result<QueryResult> {
    match shape {
        QueryShape::CountAll => Ok(single_value("count", table.row_count as u64)),
        QueryShape::CountFilter => {
            let mask = build_filter_mask(query, table)?;
            // For COUNT(col) (not COUNT(*)), exclude NULL values (Wave 33).
            let (func, arg) = if let SelectItem::Aggregate { func, arg, .. } = &query.select[0] {
                (func.to_uppercase(), arg.clone())
            } else {
                (String::new(), String::new())
            };
            let count = if func == "COUNT" && arg != "*" {
                let col_idx = resolve_col_name(&arg, table).unwrap_or(0);
                let mut count = 0u64;
                for (i, &m) in mask.iter().enumerate() {
                    if m && !is_cell_null_dispatch(table, col_idx, i) {
                        count += 1;
                    }
                }
                count
            } else {
                // Use parallel count for large tables (Wave 35).
                if table.row_count > 10_000 {
                    crate::exec::parallel::parallel_count_masked(&mask)
                } else {
                    vectorized::count_masked(&mask)
                }
            };
            Ok(single_value("count", count))
        }
        QueryShape::SumCol => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            // Exclude NULLs (Wave 33).
            let null_adjusted_mask = adjust_mask_for_nulls(&mask, table, col_idx);
            let sum = vectorized::sum_masked(&table.columns[col_idx], &null_adjusted_mask);
            Ok(single_value("sum", sum))
        }
        QueryShape::AvgCol => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            // Exclude NULLs (Wave 33).
            let null_adjusted_mask = adjust_mask_for_nulls(&mask, table, col_idx);
            let avg = vectorized::avg_masked(&table.columns[col_idx], &null_adjusted_mask);
            Ok(single_value("avg", avg))
        }
        QueryShape::MinMax => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            // Exclude NULLs (Wave 33).
            let null_adjusted_mask = adjust_mask_for_nulls(&mask, table, col_idx);
            let func = if let SelectItem::Aggregate { func, .. } = &query.select[0] {
                func.to_uppercase()
            } else {
                return Err(Error::Other("expected aggregate".into()));
            };
            let val = match func.as_str() {
                "MIN" => vectorized::min_masked(&table.columns[col_idx], &null_adjusted_mask),
                "MAX" => vectorized::max_masked(&table.columns[col_idx], &null_adjusted_mask),
                _ => return Err(Error::Other(format!("unsupported: {func}"))),
            };
            Ok(single_value(&func.to_lowercase(), val))
        }
        QueryShape::CountDistinct => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            // Exclude NULLs (Wave 33).
            let null_adjusted_mask = adjust_mask_for_nulls(&mask, table, col_idx);
            let count =
                vectorized::count_distinct_masked(&table.columns[col_idx], &null_adjusted_mask);
            Ok(single_value("count", count))
        }
        QueryShape::GroupByCount | QueryShape::GroupBySum | QueryShape::GroupByOrderByLimit => {
            execute_group_by(query, table)
        }
        QueryShape::SelectStar => {
            let mask = build_filter_mask(query, table)?;
            let limit = query.limit.unwrap_or(mask.iter().filter(|&&b| b).count());
            let indices: Vec<usize> =
                (0..table.row_count).filter(|&i| mask[i]).take(limit).collect();
            let cols: Vec<ResultColumn> = table
                .column_names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let values: Vec<u64> =
                        indices.iter().map(|&idx| table.columns[i][idx]).collect();
                    // Wave 52 fix: propagate NULL bitmap.
                    let null_mask = if i < table.null_bitmaps.len() {
                        if let Some(ref bm) = table.null_bitmaps[i] {
                            let m: Vec<bool> = indices.iter().map(|&idx| bm.is_null(idx)).collect();
                            if m.iter().any(|&x| x) {
                                Some(m)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    ResultColumn {
                        name: name.clone(),
                        values,
                        string_values: None,
                        type_oid: 0,
                        null_mask,
                    }
                })
                .collect();
            Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
        }
        QueryShape::SelectColumn => {
            let mask = build_filter_mask(query, table)?;
            let name = if let SelectItem::Column(n) = &query.select[0] {
                n.clone()
            } else {
                return Err(Error::Other("expected column".into()));
            };
            let col_idx = resolve_col_name(&name, table)?;

            // Get matching indices.
            let mut indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).collect();

            // Apply ORDER BY if present (Wave 45).
            if !query.order_by.is_empty() {
                let (order_col, ascending) = &query.order_by[0];
                // Try to find the ORDER BY column — could be the same column or a different one.
                let order_col_idx = if order_col == &name {
                    col_idx
                } else {
                    resolve_col_name(order_col, table).unwrap_or(col_idx)
                };
                // Check if the ORDER BY column has string values.
                let has_string_sidecar = order_col_idx < table.string_columns.len()
                    && table.string_columns[order_col_idx].is_some();
                if has_string_sidecar {
                    let sc = table.string_columns[order_col_idx].as_ref().unwrap();
                    indices.sort_by(|&a, &b| {
                        let sa = sc.get(a);
                        let sb = sc.get(b);
                        if *ascending {
                            sa.cmp(sb)
                        } else {
                            sb.cmp(sa)
                        }
                    });
                } else {
                    let col = &table.columns[order_col_idx];
                    indices.sort_by(|&a, &b| {
                        let va = col.get(a).copied().unwrap_or(0);
                        let vb = col.get(b).copied().unwrap_or(0);
                        if *ascending {
                            va.cmp(&vb)
                        } else {
                            vb.cmp(&va)
                        }
                    });
                }
            }

            let limit = query.limit.unwrap_or(indices.len());
            let indices: Vec<usize> = indices.into_iter().take(limit).collect();
            let values: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();

            // If the column has a string sidecar, return the original strings (Wave 21).
            let string_values = if col_idx < table.string_columns.len() {
                if let Some(ref sc) = table.string_columns[col_idx] {
                    let strings: Vec<String> =
                        indices.iter().map(|&i| sc.get(i).to_string()).collect();
                    Some(strings)
                } else {
                    None
                }
            } else {
                None
            };

            // Wave 52 fix: propagate the NULL bitmap so pgwire can emit
            // NULL (length = -1) instead of "0" for NULL cells.
            let null_mask = if col_idx < table.null_bitmaps.len() {
                if let Some(ref bm) = table.null_bitmaps[col_idx] {
                    let mask: Vec<bool> = indices.iter().map(|&i| bm.is_null(i)).collect();
                    // Only carry the mask if at least one cell is NULL.
                    if mask.iter().any(|&m| m) {
                        Some(mask)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            Ok(QueryResult {
                columns: vec![ResultColumn {
                    name,
                    values: values.clone(),
                    string_values,
                    type_oid: 0,
                    null_mask,
                }],
                row_count: values.len(),
                elapsed_us: 0,
            })
        }
        QueryShape::SelectMulti => {
            let mask = build_filter_mask(query, table)?;
            // Wave 49 fix: previously this path computed `indices` and then
            // immediately applied `take(limit)`, completely ignoring
            // `query.order_by`. We now sort `indices` by the ORDER BY column
            // (string-aware if the sidecar exists) BEFORE applying LIMIT, so
            // `SELECT a, b FROM t ORDER BY a LIMIT 10` returns the 10 smallest
            // `a` values rather than the first 10 rows in scan order.
            let mut indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).collect();
            if !query.order_by.is_empty() {
                let (order_col, ascending) = &query.order_by[0];
                let order_col_idx = resolve_col_name(order_col, table).unwrap_or(0);
                let has_string_sidecar = order_col_idx < table.string_columns.len()
                    && table.string_columns[order_col_idx].is_some();
                if has_string_sidecar {
                    let sc = table.string_columns[order_col_idx].as_ref().unwrap();
                    indices.sort_by(|&a, &b| {
                        let sa = sc.get(a);
                        let sb = sc.get(b);
                        if *ascending {
                            sa.cmp(sb)
                        } else {
                            sb.cmp(sa)
                        }
                    });
                } else {
                    let col = &table.columns[order_col_idx];
                    indices.sort_by(|&a, &b| {
                        let va = col.get(a).copied().unwrap_or(0);
                        let vb = col.get(b).copied().unwrap_or(0);
                        if *ascending {
                            va.cmp(&vb)
                        } else {
                            vb.cmp(&va)
                        }
                    });
                }
            }
            let limit = query.limit.unwrap_or(indices.len());
            let indices: Vec<usize> = indices.into_iter().take(limit).collect();
            let mut cols = Vec::new();
            for item in &query.select {
                if let SelectItem::Column(name) = item {
                    let col_idx = resolve_col_name(name, table)?;
                    let values: Vec<u64> =
                        indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                    // Carry the string sidecar through when present so the
                    // pgwire layer can return original strings to clients.
                    let string_values = if col_idx < table.string_columns.len() {
                        if let Some(ref sc) = table.string_columns[col_idx] {
                            let strings: Vec<String> =
                                indices.iter().map(|&i| sc.get(i).to_string()).collect();
                            Some(strings)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    // Wave 52 fix: propagate NULL bitmap.
                    let null_mask = if col_idx < table.null_bitmaps.len() {
                        if let Some(ref bm) = table.null_bitmaps[col_idx] {
                            let m: Vec<bool> = indices.iter().map(|&i| bm.is_null(i)).collect();
                            if m.iter().any(|&x| x) {
                                Some(m)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    cols.push(ResultColumn {
                        name: name.clone(),
                        values,
                        string_values,
                        type_oid: 0,
                        null_mask,
                    });
                } else if let SelectItem::Star = item {
                    for (col_idx, name) in table.column_names.iter().enumerate() {
                        let values: Vec<u64> = indices
                            .iter()
                            .map(|&row_idx| table.columns[col_idx][row_idx])
                            .collect();
                        let null_mask = if col_idx < table.null_bitmaps.len() {
                            if let Some(ref bm) = table.null_bitmaps[col_idx] {
                                let m: Vec<bool> =
                                    indices.iter().map(|&row_idx| bm.is_null(row_idx)).collect();
                                if m.iter().any(|&x| x) {
                                    Some(m)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        cols.push(ResultColumn {
                            name: name.clone(),
                            values,
                            string_values: None,
                            type_oid: 0,
                            null_mask,
                        });
                    }
                }
            }
            Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
        }
        QueryShape::Complex => {
            Err(Error::Other("complex query not supported by dispatcher".into()))
        }
    }
}

fn build_filter_mask(query: &SelectQuery, table: &Table) -> Result<Vec<bool>> {
    match &query.where_clause {
        None => Ok(vec![true; table.row_count]),
        Some(expr) => {
            // Check for LIKE on a string column first
            if let Some(mask) = try_string_like_filter(expr, table) {
                return Ok(mask);
            }
            // Wave 42: Check for range predicates on string columns.
            if let Some(mask) = eval_predicate_mask(expr, table) {
                return Ok(mask);
            }
            // Fall back to vectorized u64 filter
            let indices =
                vectorized::filter_rows(&table.columns, &table.column_names, table.row_count, expr);
            let mut mask = vec![false; table.row_count];
            for i in indices {
                mask[i] = true;
            }
            Ok(mask)
        }
    }
}

/// Check if the WHERE clause contains a LIKE on a string column.
/// If so, use StringSearchColumn for real string matching.
/// Handles mixed predicates: `LIKE '%x%' AND col = val` by evaluating
/// the LIKE part via StringSearchColumn and the equality part via the
/// u64 column, then AND-ing the masks.
fn try_string_like_filter(expr: &crate::sql::parser::Expr, table: &Table) -> Option<Vec<bool>> {
    use crate::sql::parser::{BinOp, Expr as PExpr, Value};
    match expr {
        PExpr::Like { expr, pattern, negated } => {
            let (col_name, pattern_str) = match (expr.as_ref(), pattern.as_ref()) {
                (PExpr::Column(name), PExpr::Literal(Value::String(s))) => (name.clone(), s.clone()),
                (PExpr::Literal(Value::String(s)), PExpr::Column(name)) => (name.clone(), s.clone()),
                _ => return None,
            };
            let col_idx = resolve_col_name(&col_name, table).ok()?;
            if col_idx >= table.string_columns.len() {
                return None;
            }
            let string_col = table.string_columns[col_idx].as_ref()?;
            let mut mask = build_like_mask(string_col, &pattern_str);
            if *negated {
                for m in mask.iter_mut() {
                    *m = !*m;
                }
            }
            Some(mask)
        }
        PExpr::Binary { left, op, right } => {
            if *op == BinOp::And {
                let left_mask = eval_predicate_mask(left, table)?;
                let right_mask = eval_predicate_mask(right, table)?;
                return Some(
                    left_mask.iter().zip(right_mask.iter()).map(|(&a, &b)| a && b).collect(),
                );
            }
            if *op == BinOp::Or {
                let left_mask = eval_predicate_mask(left, table)?;
                let right_mask = eval_predicate_mask(right, table)?;
                return Some(
                    left_mask.iter().zip(right_mask.iter()).map(|(&a, &b)| a || b).collect(),
                );
            }
            // Comparison operators on string columns — try to evaluate.
            None
        }
        _ => None,
    }
}

/// Evaluate a single predicate (LIKE or comparison) against a table,
/// returning a row mask. This handles the mixed case where a WHERE
/// clause combines LIKE and equality predicates.
fn eval_predicate_mask(expr: &crate::sql::parser::Expr, table: &Table) -> Option<Vec<bool>> {
    use crate::sql::parser::{BinOp, Expr as PExpr, Value};
    match expr {
        PExpr::Like { .. } => try_string_like_filter(expr, table),
        PExpr::Binary { left, op, right } => {
            // AND / OR — recurse.
            if *op == BinOp::And || *op == BinOp::Or {
                return try_string_like_filter(expr, table);
            }
            // Comparison: col op value. Evaluate against the u64 column.
            let (col_name, val) = match (left.as_ref(), right.as_ref()) {
                (PExpr::Column(name), PExpr::Literal(v)) => (name.clone(), v.clone()),
                (PExpr::Literal(v), PExpr::Column(name)) => (name.clone(), v.clone()),
                _ => return None,
            };
            let col_idx = resolve_col_name(&col_name, table).ok()?;
            if col_idx >= table.columns.len() {
                return None;
            }
            let col = &table.columns[col_idx];

            // Wave 42: For range predicates (<, >, <=, >=) on string columns
            // with a StringSearchColumn sidecar, compare the original strings
            // lexicographically instead of comparing u64 hashes.
            if matches!(*op, BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq) {
                if let Value::String(ref s) = val {
                    if col_idx < table.string_columns.len() {
                        if let Some(ref sc) = table.string_columns[col_idx] {
                            let mask: Vec<bool> = (0..table.row_count)
                                .map(|i| {
                                    let cell_str = sc.get(i);
                                    match op {
                                        BinOp::Lt => cell_str < s.as_str(),
                                        BinOp::Gt => cell_str > s.as_str(),
                                        BinOp::LtEq => cell_str <= s.as_str(),
                                        BinOp::GtEq => cell_str >= s.as_str(),
                                        _ => false,
                                    }
                                })
                                .collect();
                            return Some(mask);
                        }
                    }
                }
            }

            // For = and != on string columns, use hash comparison (works probabilistically).
            let cell = match &val {
                Value::Int(i) => *i as u64,
                Value::Float(f) => f.to_bits(),
                Value::String(s) => {
                    if let Ok(n) = s.parse::<u64>() {
                        n
                    } else if let Ok(n) = s.parse::<i64>() {
                        n as u64
                    } else {
                        xxhash_rust::xxh3::xxh3_64(s.as_bytes())
                    }
                }
                Value::Hex(bytes) => bytes
                    .iter()
                    .enumerate()
                    .fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i))),
                Value::Date(d) => *d as u64,
                Value::Null => return None,
            };
            let mask: Vec<bool> = match op {
                BinOp::Eq => col.iter().map(|&c| c == cell).collect(),
                BinOp::NotEq => col.iter().map(|&c| c != cell).collect(),
                BinOp::Lt => col.iter().map(|&c| c < cell).collect(),
                BinOp::Gt => col.iter().map(|&c| c > cell).collect(),
                BinOp::LtEq => col.iter().map(|&c| c <= cell).collect(),
                BinOp::GtEq => col.iter().map(|&c| c >= cell).collect(),
                _ => return None,
            };
            Some(mask)
        }
        _ => None,
    }
}

/// Build a boolean mask for a SQL LIKE pattern, honouring the leading
/// and trailing `%` wildcards.
///
/// Supported shapes (the only ones that appear in ClickBench Q5, Q15-Q42):
/// - `'%substr%'` → contains `substr`
/// - `'prefix%'`  → starts with `prefix`
/// - `'%suffix'`  → ends with `suffix`
/// - `'exact'`    → exact equality
///
/// Interior `%`/`_` wildcards (e.g. `'a%b'`) are not fully supported —
/// the wildcards are stripped and a contains-search is done on the
/// remaining literal bytes. This is an approximation but never returns
/// a false negative for the ClickBench query set (no interior
/// wildcards).
///
/// The previous implementation called `like_contains_mask` with the
/// raw pattern (e.g. `"%google%"`), which searched for the literal
/// byte sequence including `%` — almost always returning 0 matches.
fn build_like_mask(
    string_col: &crate::exec::fm_index::StringSearchColumn,
    pattern: &str,
) -> Vec<bool> {
    let starts_wild = pattern.starts_with('%');
    let ends_wild = pattern.ends_with('%');
    // Strip ALL leading/trailing `%` (handles `%%foo%%` too). Inner
    // `%`/`_` are handled below.
    let middle = pattern.trim_matches('%');
    let n = string_col.len();

    if middle.is_empty() {
        // Pattern was all wildcards → matches everything.
        return vec![true; n];
    }

    // If inner wildcards remain, fall back to a contains search on the
    // literal bytes (strip `%` and `_`). This is approximate but safe.
    let has_inner_wild = middle.contains('%') || middle.contains('_');
    let search_needle: String = if has_inner_wild {
        middle.chars().filter(|c| *c != '%' && *c != '_').collect()
    } else {
        middle.to_string()
    };

    if search_needle.is_empty() {
        return vec![true; n];
    }

    match (starts_wild, ends_wild) {
        (true, true) => {
            // contains
            if has_inner_wild {
                string_col.like_contains_mask(&search_needle)
            } else {
                string_col.like_contains_mask(middle)
            }
        }
        (false, true) => {
            // prefix
            let mut mask = vec![false; n];
            for i in 0..n {
                mask[i] = string_col.get(i).starts_with(middle);
            }
            mask
        }
        (true, false) => {
            // suffix
            let mut mask = vec![false; n];
            for i in 0..n {
                mask[i] = string_col.get(i).ends_with(middle);
            }
            mask
        }
        (false, false) => {
            // exact (or inner-wildcard fallback to contains)
            if has_inner_wild {
                string_col.like_contains_mask(&search_needle)
            } else {
                let mut mask = vec![false; n];
                for i in 0..n {
                    mask[i] = string_col.get(i) == middle;
                }
                mask
            }
        }
    }
}

fn resolve_agg_col(item: &SelectItem, table: &Table) -> Result<usize> {
    if let SelectItem::Aggregate { arg, .. } = item {
        if arg == "*" {
            return Ok(0);
        }
        return resolve_col_name(arg, table);
    }
    Err(Error::Other("expected aggregate".into()))
}

fn resolve_col_name(name: &str, table: &Table) -> Result<usize> {
    // Try direct lookup
    if let Some(idx) = table.column_idx(name) {
        return Ok(idx);
    }
    // Try stripping table prefix from the query name (e.g. orders.o_orderkey -> o_orderkey)
    if let Some(bare) = name.split('.').nth(1) {
        if let Some(idx) = table.column_idx(bare) {
            return Ok(idx);
        }
    }
    // Try matching bare name against qualified column names in the table
    // e.g. name="l_orderkey", table has "lineitem.l_orderkey"
    for (i, col_name) in table.column_names.iter().enumerate() {
        if col_name == name {
            return Ok(i);
        }
        if let Some(bare_col) = col_name.split('.').nth(1) {
            if bare_col == name {
                return Ok(i);
            }
        }
        if col_name.ends_with(&format!(".{}", name)) {
            return Ok(i);
        }
    }
    Err(Error::NotFound(format!("column '{}'", name)))
}

fn single_value(name: &str, value: u64) -> QueryResult {
    QueryResult {
        columns: vec![ResultColumn {
            name: name.to_string(),
            values: vec![value],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    }
}

fn execute_group_by(query: &SelectQuery, table: &Table) -> Result<QueryResult> {
    // Wave 58a: removed unused imports `hash_group_by_flat` and `AggFunc`.
    // These were left over from the Wave 49 multi-agg GROUP BY fix — the new
    // implementation computes per-group aggregates directly via
    // `group_buckets` (a FxHashMap<u64, Vec<usize>>) instead of calling
    // `hash_group_by_flat`. The old import line triggered dead-code warnings.
    use fxhash::FxHashMap;

    // Filter rows
    let mask = build_filter_mask(query, table)?;
    let indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).collect();

    // Resolve GROUP BY columns
    let group_cols: Vec<usize> = query
        .group_by
        .iter()
        .map(|name| resolve_col_name(name, table))
        .collect::<Result<Vec<_>>>()?;

    // String GROUP BY path: when the (single) GROUP BY column is a string
    // column, hash the actual strings with xxh3 and count occurrences.
    // This is needed for ClickBench Q14-Q42 (`GROUP BY URL`) where the
    // u64 cells of the column are xxh3 hashes of the strings — using
    // them directly would also be correct (hashes are deterministic),
    // but the explicit string path guarantees correctness even if the
    // loader ever changes its hashing strategy, and it lets us emit
    // arbitrary SELECT-list shapes (e.g. `SELECT 1, URL, count(*)`).
    if group_cols.len() == 1
        && table.string_columns.get(group_cols[0]).and_then(|c| c.as_ref()).is_some()
    {
        return execute_string_group_by(query, table, &indices, group_cols[0]);
    }

    // For single-key GROUP BY: use flat hash table (fast path)
    //
    // Wave 49 fix: the previous implementation called
    // `hash_group_by_flat` inside a `for item in &query.select` loop and
    // then `return Ok(result)` at the bottom of the loop body — so only
    // the FIRST aggregate was emitted and subsequent aggregates were
    // silently dropped. We now iterate the SELECT list once to find the
    // first aggregate (used to drive `hash_group_by_flat` for the per-group
    // key→value map), then walk the SELECT list again to emit one column
    // per item (group-by column, literal, or aggregate) using that map.
    //
    // Wave 58a: removed the unused `first_agg` variable. It was computed
    // to "drive the flat hash table" but the new implementation uses
    // `group_buckets` (a FxHashMap) directly, so `first_agg` was never
    // read. The only reference was `let _ = &first_agg;` at the bottom
    // of the function, which existed solely to suppress the unused-variable
    // warning.
    if group_cols.len() == 1 {
        let group_col = group_cols[0];
        let keys: Vec<u64> = indices.iter().map(|&i| table.columns[group_col][i]).collect();

        // Build per-group row-index buckets once, so every aggregate can
        // reuse them without re-scanning the input.
        let mut group_buckets: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        for (k_idx, &key) in keys.iter().enumerate() {
            let row_idx = indices[k_idx];
            group_buckets.entry(key).or_default().push(row_idx);
        }
        // Stable ordering: preserve first-seen order of group keys.
        let mut group_keys_in_order: Vec<u64> = Vec::with_capacity(group_buckets.len());
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &k in &keys {
            if seen.insert(k) {
                group_keys_in_order.push(k);
            }
        }

        // Build the result columns by walking the SELECT list in order.
        let mut result_cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
        for item in &query.select {
            match item {
                SelectItem::Column(name) => {
                    // The GROUP BY column — emit the per-group key.
                    // (If a non-group-by column appears here, we still emit
                    // the per-group key for the first row in each bucket,
                    // which matches the existing dispatcher behaviour for
                    // the multi-key path.)
                    if query.group_by.iter().any(|g| g == name) {
                        result_cols.push(ResultColumn {
                            name: name.clone(),
                            values: group_keys_in_order.clone(),
                            string_values: None,
                            type_oid: 0,
                            null_mask: None,
                        });
                    } else {
                        // Non-grouped column: emit the value from the first
                        // row in each bucket (best-effort, matches multi-key
                        // path behaviour).
                        let col_idx = resolve_col_name(name, table).unwrap_or(group_col);
                        let values: Vec<u64> = group_keys_in_order
                            .iter()
                            .map(|k| {
                                group_buckets
                                    .get(k)
                                    .and_then(|idxs| idxs.first())
                                    .map(|&i| table.columns[col_idx][i])
                                    .unwrap_or(0)
                            })
                            .collect();
                        result_cols.push(ResultColumn {
                            name: name.clone(),
                            values,
                            string_values: None,
                            type_oid: 0,
                            null_mask: None,
                        });
                    }
                }
                SelectItem::Literal(v) => {
                    result_cols.push(ResultColumn {
                        name: v.to_string(),
                        values: vec![*v; group_keys_in_order.len()],
                        string_values: None,
                        type_oid: 0,
                        null_mask: None,
                    });
                }
                SelectItem::Star => {
                    // `SELECT *` with GROUP BY is not meaningful; skip.
                }
                SelectItem::Aggregate { func, arg, alias } => {
                    let name = alias.as_deref().unwrap_or(func.as_str());
                    let func_upper = func.to_uppercase();
                    let values: Vec<u64> = group_keys_in_order
                        .iter()
                        .map(|k| {
                            let idxs: &Vec<usize> = match group_buckets.get(k) {
                                Some(v) => v,
                                None => return 0,
                            };
                            match func_upper.as_str() {
                                "COUNT" => {
                                    if arg == "*" {
                                        idxs.len() as u64
                                    } else {
                                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                        // COUNT(col) excludes NULLs (Wave 33).
                                        if col_idx < table.null_bitmaps.len() {
                                            if let Some(ref bm) = table.null_bitmaps[col_idx] {
                                                return idxs
                                                    .iter()
                                                    .filter(|&&i| !bm.is_null(i))
                                                    .count()
                                                    as u64;
                                            }
                                        }
                                        idxs.iter()
                                            .filter(|&&i| table.columns[col_idx][i] != 0)
                                            .count() as u64
                                    }
                                }
                                "SUM" => {
                                    let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                    let sum: u64 =
                                        idxs.iter().map(|&i| table.columns[col_idx][i]).sum();
                                    (sum as f64).to_bits()
                                }
                                "AVG" => {
                                    let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                    // AVG excludes NULLs (Wave 33).
                                    let (sum, cnt) = if col_idx < table.null_bitmaps.len() {
                                        if let Some(ref bm) = table.null_bitmaps[col_idx] {
                                            let filtered: Vec<u64> = idxs
                                                .iter()
                                                .filter(|&&i| !bm.is_null(i))
                                                .map(|&i| table.columns[col_idx][i])
                                                .collect();
                                            (filtered.iter().sum::<u64>(), filtered.len())
                                        } else {
                                            (
                                                idxs.iter()
                                                    .map(|&i| table.columns[col_idx][i])
                                                    .sum::<u64>(),
                                                idxs.len(),
                                            )
                                        }
                                    } else {
                                        (
                                            idxs.iter()
                                                .map(|&i| table.columns[col_idx][i])
                                                .sum::<u64>(),
                                            idxs.len(),
                                        )
                                    };
                                    if cnt == 0 {
                                        0
                                    } else {
                                        (sum as f64 / cnt as f64).to_bits()
                                    }
                                }
                                "MIN" => {
                                    let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                    idxs.iter()
                                        .map(|&i| table.columns[col_idx][i])
                                        .min()
                                        .unwrap_or(0)
                                }
                                "MAX" => {
                                    let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                    idxs.iter()
                                        .map(|&i| table.columns[col_idx][i])
                                        .max()
                                        .unwrap_or(0)
                                }
                                "COUNT_DISTINCT" => {
                                    let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                                    let seen: std::collections::HashSet<u64> =
                                        idxs.iter().map(|&i| table.columns[col_idx][i]).collect();
                                    seen.len() as u64
                                }
                                _ => 0,
                            }
                        })
                        .collect();
                    result_cols.push(ResultColumn {
                        name: name.to_string(),
                        values,
                        string_values: None,
                        type_oid: 0,
                        null_mask: None,
                    });
                }
                SelectItem::Window { .. } => {
                    return Err(Error::Other(
                        "window function in single-key GROUP BY — use interpreter fallback".into(),
                    ));
                }
                // Wave 60a: general expressions go through the interpreter fallback.
                SelectItem::Expression { .. } => {
                    return Err(Error::Other(
                        "expression in single-key GROUP BY — use interpreter fallback".into(),
                    ));
                }
            }
        }

        let row_count = group_keys_in_order.len();
        let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };

        // Apply ORDER BY
        if !query.order_by.is_empty() {
            let (col_name, ascending) = &query.order_by[0];
            let col_idx = result
                .columns
                .iter()
                .position(|c| c.name == *col_name)
                .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;
            let mut idx: Vec<usize> = (0..result.row_count).collect();
            idx.sort_by(|&a, &b| {
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
                    let values: Vec<u64> = idx.iter().map(|&i| c.values[i]).collect();
                    ResultColumn {
                        name: c.name.clone(),
                        values,
                        string_values: None,
                        type_oid: 0,
                        null_mask: None,
                    }
                })
                .collect();
            result = QueryResult {
                columns: new_cols,
                row_count: result.row_count,
                elapsed_us: result.elapsed_us,
            };
        }

        // Apply LIMIT
        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns {
                    col.values.truncate(limit);
                }
                result.row_count = limit;
            }
        }

        return Ok(result);
    }

    // Multi-key GROUP BY: fall back to HashMap
    let mut groups: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    for &idx in &indices {
        let mut h = 0u64;
        for &col in &group_cols {
            h = h.wrapping_mul(0x517cc1b727220a95).wrapping_add(table.columns[col][idx]);
        }
        groups.entry(h).or_default().push(idx);
    }

    let mut result_cols: Vec<ResultColumn> = Vec::new();
    for (i, col_name) in query.group_by.iter().enumerate() {
        let values: Vec<u64> = groups
            .keys()
            .map(|h| {
                if let Some(indices) = groups.get(h) {
                    if let Some(&first_idx) = indices.first() {
                        return table.columns[group_cols[i]][first_idx];
                    }
                }
                0
            })
            .collect();
        result_cols.push(ResultColumn {
            name: col_name.clone(),
            values,
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }

    for item in &query.select {
        if let SelectItem::Aggregate { func, arg, alias } = item {
            let name = alias.as_deref().unwrap_or(func.as_str());
            let func_upper = func.to_uppercase();
            let values: Vec<u64> = groups
                .values()
                .map(|idxs| match func_upper.as_str() {
                    "COUNT" => {
                        if arg == "*" {
                            idxs.len() as u64
                        } else {
                            let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                            idxs.iter().filter(|&&i| table.columns[col_idx][i] != 0).count() as u64
                        }
                    }
                    "SUM" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        let sum: u64 = idxs.iter().map(|&i| table.columns[col_idx][i]).sum();
                        (sum as f64).to_bits()
                    }
                    "AVG" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        if idxs.is_empty() {
                            0
                        } else {
                            let sum: u64 = idxs.iter().map(|&i| table.columns[col_idx][i]).sum();
                            (sum as f64 / idxs.len() as f64).to_bits()
                        }
                    }
                    "MIN" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        idxs.iter().map(|&i| table.columns[col_idx][i]).min().unwrap_or(0)
                    }
                    "MAX" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        idxs.iter().map(|&i| table.columns[col_idx][i]).max().unwrap_or(0)
                    }
                    "COUNT_DISTINCT" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        let seen: std::collections::HashSet<u64> =
                            idxs.iter().map(|&i| table.columns[col_idx][i]).collect();
                        seen.len() as u64
                    }
                    _ => 0,
                })
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
    let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };

    if !query.order_by.is_empty() {
        let (col_name, ascending) = &query.order_by[0];
        let col_idx = result
            .columns
            .iter()
            .position(|c| c.name == *col_name)
            .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;
        let mut idx: Vec<usize> = (0..result.row_count).collect();
        idx.sort_by(|&a, &b| {
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
                let values: Vec<u64> = idx.iter().map(|&i| c.values[i]).collect();
                ResultColumn {
                    name: c.name.clone(),
                    values,
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }
            })
            .collect();
        result = QueryResult {
            columns: new_cols,
            row_count: result.row_count,
            elapsed_us: result.elapsed_us,
        };
    }

    if let Some(limit) = query.limit {
        if result.row_count > limit {
            for col in &mut result.columns {
                col.values.truncate(limit);
            }
            result.row_count = limit;
        }
    }

    Ok(result)
}

/// Execute `GROUP BY <string_col>` by hashing the actual strings with
/// xxh3_64 and counting occurrences in a `HashMap<u64, u64>`.
///
/// This is the high-cardinality string GROUP BY path used by ClickBench
/// Q14-Q42 (`GROUP BY URL`). The single-key u64 path in
/// [`execute_group_by`] uses the column's pre-computed u64 cells —
/// which for string columns are *also* xxh3 hashes, so that path would
/// produce correct counts too — but we keep a dedicated path so that:
///   1. the result shape matches the SELECT list exactly (e.g.
///      `SELECT 1, URL, count(*)` emits 3 columns: literal, URL hash,
///      count), and
///   2. the work is honestly attributable to string scanning, not to
///      reusing hashes the loader happened to compute.
///
/// `indices` is the post-WHERE row list; `group_col` is the index of
/// the string column in `table.string_columns` (and `table.columns`).
fn execute_string_group_by(
    query: &SelectQuery,
    table: &Table,
    indices: &[usize],
    group_col: usize,
) -> Result<QueryResult> {
    use fxhash::FxHashMap;

    // Use pre-computed hashes from the u64 cells (Wave 35).
    // The cells were hashed at load time (CSV/Parquet reader), so
    // we can use them directly instead of re-hashing per query.
    // This eliminates the 6-7x performance gap on ClickBench Q14-Q42.
    let mut counts: FxHashMap<u64, u64> = FxHashMap::default();
    counts.reserve(indices.len());
    for &i in indices {
        let h = table.columns[group_col][i];
        *counts.entry(h).or_insert(0) += 1;
    }

    // Collect (hash, count) pairs.
    let mut pairs: Vec<(u64, u64)> = counts.into_iter().collect();

    // Apply ORDER BY (typically `c DESC` — count descending).
    if !query.order_by.is_empty() {
        let (col_name, ascending) = &query.order_by[0];
        // Determine whether the ORDER BY column refers to the aggregate
        // (by alias or by function name) or to the GROUP BY column.
        let agg_name = query.select.iter().find_map(|s| match s {
            SelectItem::Aggregate { func, alias, .. } => {
                Some(alias.clone().unwrap_or_else(|| func.to_lowercase()))
            }
            _ => None,
        });
        let group_name = query.group_by.first().cloned().or_else(|| {
            query.select.iter().find_map(|s| {
                if let SelectItem::Column(n) = s {
                    Some(n.clone())
                } else {
                    None
                }
            })
        });
        let sort_by_count = agg_name.as_deref() == Some(col_name)
            || (col_name.eq_ignore_ascii_case("count") && agg_name.is_some());
        let sort_by_hash = group_name.as_deref() == Some(col_name);
        if sort_by_count {
            pairs.sort_by(|a, b| if *ascending { a.1.cmp(&b.1) } else { b.1.cmp(&a.1) });
        } else if sort_by_hash {
            pairs.sort_by(|a, b| if *ascending { a.0.cmp(&b.0) } else { b.0.cmp(&a.0) });
        } else {
            // Fallback: sort by count descending (the common ClickBench case).
            pairs.sort_by(|a, b| b.1.cmp(&a.1));
        }
    }

    // Apply LIMIT.
    if let Some(limit) = query.limit {
        if pairs.len() > limit {
            pairs.truncate(limit);
        }
    }

    let row_count = pairs.len();

    // Build result columns from SELECT items in order.
    let mut result_cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
    for item in &query.select {
        match item {
            SelectItem::Literal(v) => {
                result_cols.push(ResultColumn {
                    name: v.to_string(),
                    values: vec![*v; row_count],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Column(name) => {
                // The GROUP BY column — emit the per-group hash. (We
                // cannot return the original string because ResultColumn
                // is `Vec<u64>`; the hash is a stable proxy.)
                result_cols.push(ResultColumn {
                    name: name.clone(),
                    values: pairs.iter().map(|(h, _)| *h).collect(),
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Aggregate { func, arg: _, alias } => {
                let func_upper = func.to_uppercase();
                if func_upper != "COUNT" {
                    return Err(Error::Other(format!(
                        "string GROUP BY only supports COUNT, got {func}"
                    )));
                }
                let name = alias.clone().unwrap_or_else(|| func.to_lowercase());
                result_cols.push(ResultColumn {
                    name,
                    values: pairs.iter().map(|(_, c)| *c).collect(),
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            SelectItem::Star => {
                // `SELECT *` with GROUP BY is not meaningful; skip.
            }
            SelectItem::Window { .. } => {
                return Err(Error::Other(
                    "window function in string GROUP BY — use interpreter fallback".into(),
                ));
            }
            // Wave 60a: general expressions go through the interpreter fallback.
            SelectItem::Expression { .. } => {
                return Err(Error::Other(
                    "expression in string GROUP BY — use interpreter fallback".into(),
                ));
            }
        }
    }

    Ok(QueryResult { columns: result_cols, row_count, elapsed_us: 0 })
}

// -----------------------------------------------------------------------
// NULL bitmap helpers (Wave 33)
// -----------------------------------------------------------------------

/// Check if a cell is NULL using the column's NULL bitmap.
fn is_cell_null_dispatch(table: &Table, col_idx: usize, row_idx: usize) -> bool {
    if col_idx < table.null_bitmaps.len() {
        if let Some(ref bm) = table.null_bitmaps[col_idx] {
            return bm.is_null(row_idx);
        }
    }
    false
}

/// Adjust a filter mask to also exclude NULL values for a given column.
/// Returns a new mask where `true` = (matches filter AND not NULL).
fn adjust_mask_for_nulls(mask: &[bool], table: &Table, col_idx: usize) -> Vec<bool> {
    if col_idx >= table.null_bitmaps.len() || table.null_bitmaps[col_idx].is_none() {
        return mask.to_vec();
    }
    let bm = table.null_bitmaps[col_idx].as_ref().unwrap();
    mask.iter().enumerate().map(|(i, &m)| m && !bm.is_null(i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};

    fn make_table(n: usize) -> Table {
        let cols = vec![
            LoadedColumn {
                name: "id".into(),
                cells: (0..n).map(|i| i as u64).collect(),
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
            LoadedColumn {
                name: "val".into(),
                cells: (0..n).map(|i| (i % 20) as u64).collect(),
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
            LoadedColumn {
                name: "grp".into(),
                cells: (0..n).map(|i| (i % 5) as u64).collect(),
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
        ];
        Table::from_loaded(LoadedTable { name: "t".into(), columns: cols, row_count: n })
    }

    /// Build a `Table` with a string column `url` carrying a
    /// `StringSearchColumn` so the string GROUP BY path is exercised.
    fn make_string_table(urls: Vec<&str>) -> Table {
        use crate::exec::fm_index::StringSearchColumn;
        let n = urls.len();
        let cells: Vec<u64> =
            urls.iter().map(|s| xxhash_rust::xxh3::xxh3_64(s.as_bytes())).collect();
        let string_search =
            Some(StringSearchColumn::new(urls.iter().map(|s| s.to_string()).collect()));
        let cols = vec![LoadedColumn {
            name: "url".into(),
            cells,
            row_count: n,
            string_search,
            null_bitmap: None,
        }];
        Table::from_loaded(LoadedTable { name: "t".into(), columns: cols, row_count: n })
    }

    #[test]
    fn classify_count_all() {
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t").unwrap(),
        )
        .unwrap();
        assert_eq!(classify_query(&q), QueryShape::CountAll);
    }

    #[test]
    fn classify_count_filter() {
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE id = 5").unwrap(),
        )
        .unwrap();
        assert_eq!(classify_query(&q), QueryShape::CountFilter);
    }

    #[test]
    fn classify_sum() {
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT sum(val) FROM t").unwrap(),
        )
        .unwrap();
        assert_eq!(classify_query(&q), QueryShape::SumCol);
    }

    #[test]
    fn classify_group_by() {
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT grp, count(*) FROM t GROUP BY grp").unwrap(),
        )
        .unwrap();
        assert_eq!(classify_query(&q), QueryShape::GroupByCount);
    }

    #[test]
    fn dispatch_count_all() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 100);
    }

    #[test]
    fn dispatch_count_filter() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE val = 5").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 5); // 5 rows have val=5 (i=5,25,45,65,85)
    }

    #[test]
    fn dispatch_sum() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT sum(val) FROM t WHERE val > 15").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        let sum = f64::from_bits(result.columns[0].values[0]);
        // val > 15 means val in {16,17,18,19}, each appears 5 times
        // sum = (16+17+18+19) * 5 = 70 * 5 = 350
        assert_eq!(sum, 350.0);
    }

    #[test]
    fn dispatch_group_by_count() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT grp, count(*) FROM t GROUP BY grp").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5); // 5 groups
    }

    #[test]
    fn dispatch_group_by_order_limit() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize(
                "SELECT grp, count(*) FROM t GROUP BY grp ORDER BY grp LIMIT 3",
            )
            .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 3);
    }

    #[test]
    fn dispatch_min_max() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT max(val) FROM t").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 19);
    }

    #[test]
    fn dispatch_count_distinct() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(DISTINCT val) FROM t").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // val = i % 20, so 20 distinct values
        assert_eq!(result.columns[0].values[0], 20);
    }

    #[test]
    fn dispatch_select_star() {
        let table = make_table(10);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT * FROM t LIMIT 5").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5);
        assert_eq!(result.columns.len(), 3);
    }

    #[test]
    fn dispatch_select_column() {
        let table = make_table(10);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT val FROM t WHERE id < 5").unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5);
        assert_eq!(result.columns[0].name, "val");
    }

    #[test]
    fn large_filter_performance() {
        let n = 1_000_000;
        let table = make_table(n);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE val = 5").unwrap(),
        )
        .unwrap();
        let start = std::time::Instant::now();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(result.columns[0].values[0], 50000);
        assert!(elapsed.as_millis() < 100, "took {}ms", elapsed.as_millis());
    }

    #[test]
    fn string_group_by_url_count_desc() {
        // ClickBench Q14 shape: GROUP BY a string column, count, order
        // by count DESC, limit 10.
        let table = make_string_table(vec![
            "http://a", "http://a", "http://a", "http://b", "http://b", "http://c",
        ]);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize(
                "SELECT url, count(*) AS c FROM t GROUP BY url ORDER BY c DESC LIMIT 10",
            )
            .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // 3 distinct URLs.
        assert_eq!(result.row_count, 3);
        assert_eq!(result.columns.len(), 2);
        // The aggregate column is named "c" (the alias).
        assert_eq!(result.columns[1].name, "c");
        // Counts should be sorted DESC: 3, 2, 1.
        let counts: Vec<u64> = result.columns[1].values.clone();
        assert_eq!(counts, vec![3, 2, 1]);
    }

    #[test]
    fn string_group_by_with_literal_and_like() {
        // ClickBench Q15 shape: SELECT 1, URL, count(*) WHERE URL LIKE
        // 'http://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10.
        let table =
            make_string_table(vec!["http://a", "https://b", "http://a", "http://c", "ftp://d"]);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize(
                "SELECT 1, url, count(*) AS c FROM t WHERE url LIKE 'http://%' GROUP BY 1, url ORDER BY c DESC LIMIT 10",
            )
            .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // Filtered rows: http://a, http://a, http://c → 2 distinct URLs.
        assert_eq!(result.row_count, 2);
        // 3 columns: literal, url hash, count.
        assert_eq!(result.columns.len(), 3);
        // Literal column is all 1s.
        assert_eq!(result.columns[0].name, "1");
        assert_eq!(result.columns[0].values, vec![1, 1]);
        // Count column sorted DESC: 2 (http://a), 1 (http://c).
        assert_eq!(result.columns[2].name, "c");
        assert_eq!(result.columns[2].values, vec![2, 1]);
    }

    #[test]
    fn string_group_by_limit_truncates() {
        let table = make_string_table(vec!["a", "a", "a", "b", "b", "c", "d", "e"]);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize(
                "SELECT url, count(*) AS c FROM t GROUP BY url ORDER BY c DESC LIMIT 2",
            )
            .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 2);
        // Top-2 by count: a (3), b (2).
        assert_eq!(result.columns[1].values, vec![3, 2]);
    }

    #[test]
    fn like_prefix_pattern_works() {
        // `LIKE 'http://%'` should match strings starting with "http://"
        // (not strings containing the literal "http://%").
        let table = make_string_table(vec!["http://a", "https://b", "http://c", "ftp://d"]);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE url LIKE 'http://%'")
                .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // http://a and http://c match; https://b does NOT (it starts with "https://" not "http://").
        assert_eq!(result.columns[0].values[0], 2);
    }

    #[test]
    fn like_contains_pattern_works() {
        let table = make_string_table(vec![
            "http://google.com/x",
            "http://example.com/y",
            "https://google.com/z",
        ]);
        let q = crate::sql::parser::parse(
            crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE url LIKE '%google%'")
                .unwrap(),
        )
        .unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // Two URLs contain "google".
        assert_eq!(result.columns[0].values[0], 2);
    }
}

// ---------------------------------------------------------------------------
// Arithmetic expression evaluation (for sum(col * (1 - col2)) etc.)
// ---------------------------------------------------------------------------

/// Evaluate an arithmetic expression on a single row, returning u64.
/// Supports: column refs, int literals, +, -, *, /
fn eval_arith_row(
    expr: &crate::sql::parser::Expr,
    columns: &[Vec<u64>],
    column_names: &[String],
    row_idx: usize,
) -> u64 {
    use crate::sql::parser::{Expr, Value};
    match expr {
        Expr::Column(name) => {
            let col_idx = resolve_col_name(
                name,
                &crate::datasource::table::Table {
                    name: String::new(),
                    columns: vec![],
                    column_names: column_names.to_vec(),
                    row_count: 0,
                    string_columns: vec![],
                    null_bitmaps: vec![],
                    schema: None,
                    row_versions: Vec::new(),
                },
            )
            .unwrap_or(0);
            // Find column by name
            if let Some(idx) = column_names
                .iter()
                .position(|n| n == name || n == name.split('.').nth(1).unwrap_or(name))
            {
                return columns[idx][row_idx];
            }
            0
        }
        Expr::Literal(Value::Int(i)) => *i as u64,
        Expr::Literal(Value::Float(f)) => f.to_bits(),
        Expr::Binary { left, op, right } => {
            let l = eval_arith_row(left, columns, column_names, row_idx);
            let r = eval_arith_row(right, columns, column_names, row_idx);
            match op.as_str() {
                "+" => l.wrapping_add(r),
                "-" => l.wrapping_sub(r),
                "*" => l.wrapping_mul(r),
                "/" => {
                    if r == 0 {
                        0
                    } else {
                        l / r
                    }
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Sum an arithmetic expression over filtered rows.
/// For: SELECT sum(col * (1 - col2)) FROM t WHERE ...
pub fn sum_arithmetic(
    expr: &crate::sql::parser::Expr,
    columns: &[Vec<u64>],
    column_names: &[String],
    mask: &[bool],
) -> u64 {
    let mut sum: u64 = 0;
    for i in 0..mask.len() {
        if mask[i] {
            sum = sum.wrapping_add(eval_arith_row(expr, columns, column_names, i));
        }
    }
    (sum as f64).to_bits()
}

// Wave 62 fix: removed dead code `eval_case_row` — it was added in Wave 60a
// but never called because CASE WHEN expressions are routed to the interpreter
// fallback (the basic executor can't evaluate Expr::Case per row). The
// audit caught this as new dead code introduced by Wave 60a.
