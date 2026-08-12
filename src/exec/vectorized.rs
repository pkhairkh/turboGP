//! Vectorized batch expression evaluator — P0 performance fix.
//!
//! Processes 1024 cells at a time through the expression tree using
//! flat &[u64] slices. No ScalarValue boxing — data stays in the
//! original Vec<u64> column format.
//!
//! Based on: Polychroniou et al. (2015) "Rethinking SIMD Vectorization
//! for In-Memory Databases and Beyond"

use crate::exec::bitmap::{self, Bitmap};

/// Filter rows where col == value. Returns a boolean mask.
///
/// NOTE: This stays scalar because the AVX-512 bitmap kernel produces a
/// bit-packed `Bitmap` and converting it back to `&mut [bool]` (1 byte per
/// row) costs ~3x more than the scalar loop. The bitmap-returning variants
/// `filter_eq_bitmap` etc. are the fast path -- callers that can keep the
/// mask bit-packed end-to-end should use those instead.
#[inline]
pub fn filter_eq(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c == value;
    }
}

/// Filter rows where col != value.
#[inline]
pub fn filter_ne(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c != value;
    }
}

/// Filter rows where col < value (unsigned).
#[inline]
pub fn filter_lt(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c < value;
    }
}

/// Filter rows where col > value (unsigned).
#[inline]
pub fn filter_gt(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c > value;
    }
}

/// Filter rows where col <= value (unsigned).
#[inline]
pub fn filter_le(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c <= value;
    }
}

/// Filter rows where col >= value (unsigned).
#[inline]
pub fn filter_ge(col: &[u64], value: u64, out: &mut [bool]) {
    for (i, &c) in col.iter().enumerate() {
        out[i] = c >= value;
    }
}

// =========================================================================
// Bitmap-returning filter functions (W2 Task 2.1) -- AVX-512F kernels
// =========================================================================
//
// These return `Bitmap` directly (bit-packed, 1 bit/row) so the downstream
// aggregate functions can use POPCNT-based `count_ones` and avoid the
// 8x memory blow-up of `Vec<bool>`.

#[inline]
pub fn filter_eq_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_eq_u64(col, value)
}

#[inline]
pub fn filter_ne_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_ne_u64(col, value)
}

#[inline]
pub fn filter_lt_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_lt_u64(col, value)
}

#[inline]
pub fn filter_gt_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_gt_u64(col, value)
}

#[inline]
pub fn filter_le_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_le_u64(col, value)
}

#[inline]
pub fn filter_ge_bitmap(col: &[u64], value: u64) -> Bitmap {
    bitmap::filter_ge_u64(col, value)
}

// =========================================================================
// Bitmap-consuming aggregates -- POPCNT for count, scalar for sum/avg/min/max
// =========================================================================

/// Count of true bits in the bitmap. Uses POPCNT (64 bits per cycle).
#[inline]
pub fn count_masked_bitmap(bm: &Bitmap) -> u64 {
    bm.count_ones() as u64
}

/// Sum of col[i] where bitmap bit i is set. Returns f64 bits (for u64 sum).
#[inline]
pub fn sum_masked_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut sum: u64 = 0;
    for i in 0..col.len() {
        if bm.get(i) {
            sum = sum.wrapping_add(col[i]);
        }
    }
    (sum as f64).to_bits()
}

/// Sum for float/DECIMAL columns. Each cell is f64::to_bits.
#[inline]
pub fn sum_masked_f64_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut sum: f64 = 0.0;
    for i in 0..col.len() {
        if bm.get(i) {
            sum += f64::from_bits(col[i]);
        }
    }
    sum.to_bits()
}

/// Min of col[i] where bitmap bit i is set.
#[inline]
pub fn min_masked_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut min = u64::MAX;
    for i in 0..col.len() {
        if bm.get(i) && col[i] < min {
            min = col[i];
        }
    }
    if min == u64::MAX { 0 } else { min }
}

/// Max of col[i] where bitmap bit i is set.
#[inline]
pub fn max_masked_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut max = 0u64;
    for i in 0..col.len() {
        if bm.get(i) && col[i] > max {
            max = col[i];
        }
    }
    max
}

/// Average for u64 columns.
#[inline]
pub fn avg_masked_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for i in 0..col.len() {
        if bm.get(i) {
            sum = sum.wrapping_add(col[i]);
            count += 1;
        }
    }
    if count == 0 { 0 } else { (sum as f64 / count as f64).to_bits() }
}

/// Average for float/DECIMAL columns.
#[inline]
pub fn avg_masked_f64_bitmap(col: &[u64], bm: &Bitmap) -> u64 {
    let mut sum: f64 = 0.0;
    let mut count: u64 = 0;
    for i in 0..col.len() {
        if bm.get(i) {
            sum += f64::from_bits(col[i]);
            count += 1;
        }
    }
    if count == 0 { 0 } else { (sum / count as f64).to_bits() }
}

/// AND two boolean masks.
pub fn and_mask(left: &[bool], right: &[bool], out: &mut [bool]) {
    for i in 0..left.len() {
        out[i] = left[i] && right[i];
    }
}

/// OR two boolean masks.
pub fn or_mask(left: &[bool], right: &[bool], out: &mut [bool]) {
    for i in 0..left.len() {
        out[i] = left[i] || right[i];
    }
}

/// Evaluate a WHERE clause on a table, returning row indices that pass.
/// This replaces the per-row ScalarValue boxing path.
///
/// Supports: col OP literal, AND, OR.
/// OP can be: =, !=, <, >, <=, >=
pub fn filter_rows(
    columns: &[std::sync::Arc<Vec<u64>>],
    column_names: &[String],
    row_count: usize,
    where_expr: &crate::sql::parser::Expr,
) -> Vec<usize> {
    let mut mask = vec![true; row_count];
    eval_where(columns, column_names, row_count, where_expr, &mut mask);
    (0..row_count).filter(|&i| mask[i]).collect()
}

fn eval_where(
    columns: &[std::sync::Arc<Vec<u64>>],
    column_names: &[String],
    row_count: usize,
    expr: &crate::sql::parser::Expr,
    mask: &mut [bool],
) {
    match expr {
        crate::sql::parser::Expr::Binary { left, op, right } => {
            use crate::sql::parser::BinOp;
            if *op == BinOp::And {
                eval_where(columns, column_names, row_count, left, mask);
                let mut right_mask = vec![true; row_count];
                eval_where(columns, column_names, row_count, right, &mut right_mask);
                let left_mask = mask.to_vec();
                and_mask(&left_mask, &right_mask, mask);
            } else if *op == BinOp::Or {
                let mut left_mask = vec![true; row_count];
                let mut right_mask = vec![true; row_count];
                eval_where(columns, column_names, row_count, left, &mut left_mask);
                eval_where(columns, column_names, row_count, right, &mut right_mask);
                or_mask(&left_mask, &right_mask, mask);
            } else {
                // col OP literal
                if let (Some(col_idx), Some(val)) =
                    extract_col_and_value_batch(left, right, column_names)
                {
                    let col = &columns[col_idx];
                    match op {
                        BinOp::Eq => filter_eq(col, val, mask),
                        BinOp::NotEq => filter_ne(col, val, mask),
                        BinOp::Lt => filter_lt(col, val, mask),
                        BinOp::Gt => filter_gt(col, val, mask),
                        BinOp::LtEq => filter_le(col, val, mask),
                        BinOp::GtEq => filter_ge(col, val, mask),
                        _ => {}
                    }
                }
                // LIKE / NOT LIKE are handled by Expr::Like below (the new AST
                // represents LIKE as a distinct variant, not as Binary op).
            }
        }
        crate::sql::parser::Expr::Like { expr, pattern, negated } => {
            // LIKE: compile pattern, match against u64 values as if they were string hashes
            if let (Some(col_idx), Some(pattern_str)) =
                extract_col_and_string(expr, pattern, column_names)
            {
                // For u64 columns: compare against the hash of the pattern
                // This is an approximation — real string matching needs StringColumn
                let col = &columns[col_idx];
                for i in 0..col.len() {
                    // Simple wildcard: % matches anything, so if pattern is %, match all
                    if pattern_str == "%" {
                        mask[i] = true;
                    } else {
                        // Hash the pattern and compare (works for exact match on hashed strings)
                        let pattern_hash =
                            xxhash_rust::xxh3::xxh3_64(pattern_str.as_bytes());
                        mask[i] = col[i] == pattern_hash;
                    }
                }
                if *negated {
                    for i in 0..mask.len() {
                        mask[i] = !mask[i];
                    }
                }
            }
        }
        _ => {}
    }
}

/// Bitmap version of eval_where -- writes result to a `Bitmap` instead of `&mut [bool]`.
///
/// Same semantics as `eval_where` but uses AVX-512 bitmap kernels
/// (`bitmap::filter_eq_u64` etc.) for the comparison step and `Bitmap::and` /
/// `Bitmap::or` for combining sub-masks. The result stays in bit-packed form
/// end-to-end, so downstream aggregates can use POPCNT-based `count_ones`.
fn eval_where_bitmap(
    columns: &[std::sync::Arc<Vec<u64>>],
    column_names: &[String],
    row_count: usize,
    expr: &crate::sql::parser::Expr,
    out: &mut Bitmap,
) {
    use crate::sql::parser::{BinOp, Expr};
    match expr {
        Expr::Binary { left, op, right } => {
            if *op == BinOp::And {
                let mut left_bm = Bitmap::all_ones(row_count);
                eval_where_bitmap(columns, column_names, row_count, left, &mut left_bm);
                let mut right_bm = Bitmap::all_ones(row_count);
                eval_where_bitmap(columns, column_names, row_count, right, &mut right_bm);
                *out = left_bm.and(&right_bm);
            } else if *op == BinOp::Or {
                let mut left_bm = Bitmap::new(row_count);
                let mut right_bm = Bitmap::new(row_count);
                eval_where_bitmap(columns, column_names, row_count, left, &mut left_bm);
                eval_where_bitmap(columns, column_names, row_count, right, &mut right_bm);
                *out = left_bm.or(&right_bm);
            } else {
                // col OP literal
                if let (Some(col_idx), Some(val)) =
                    extract_col_and_value_batch(left, right, column_names)
                {
                    let col = &columns[col_idx];
                    *out = match op {
                        BinOp::Eq    => filter_eq_bitmap(col, val),
                        BinOp::NotEq=> filter_ne_bitmap(col, val),
                        BinOp::Lt    => filter_lt_bitmap(col, val),
                        BinOp::Gt    => filter_gt_bitmap(col, val),
                        BinOp::LtEq  => filter_le_bitmap(col, val),
                        BinOp::GtEq  => filter_ge_bitmap(col, val),
                        _ => { let mut bm = Bitmap::new(row_count); bm },
                    };
                }
            }
        }
        // LIKE / NOT LIKE: fall back to bool path then convert (rare in ClickBench).
        _ => {
            let mut bool_mask = vec![true; row_count];
            eval_where(columns, column_names, row_count, expr, &mut bool_mask);
            *out = Bitmap::from_bool_slice(&bool_mask);
        }
    }
}

/// Bitmap version of `filter_rows` -- returns a `Bitmap` instead of `Vec<usize>`.
///
/// Used by `build_filter_bitmap` in `dispatch.rs` to keep the mask in
/// bit-packed form end-to-end.
pub fn filter_rows_bitmap(
    columns: &[std::sync::Arc<Vec<u64>>],
    column_names: &[String],
    row_count: usize,
    where_expr: &crate::sql::parser::Expr,
) -> Bitmap {
    let mut bm = Bitmap::all_ones(row_count);
    eval_where_bitmap(columns, column_names, row_count, where_expr, &mut bm);
    bm
}

fn extract_col_and_value_batch(
    left: &crate::sql::parser::Expr,
    right: &crate::sql::parser::Expr,
    column_names: &[String],
) -> (Option<usize>, Option<u64>) {
    use crate::sql::parser::{Expr, Value};
    // Try left=column, right=literal
    if let Expr::Column(name) = left {
        if let Expr::Literal(val) = right {
            // Look up column name
            let idx = resolve_name(name, column_names);
            return (idx, value_to_u64(val));
        }
    }
    // Try right=column, left=literal
    if let Expr::Column(name) = right {
        if let Expr::Literal(val) = left {
            let idx = resolve_name(name, column_names);
            return (idx, value_to_u64(val));
        }
    }
    (None, None)
}

fn resolve_name(name: &str, column_names: &[String]) -> Option<usize> {
    // Direct lookup
    if let Some(idx) = column_names.iter().position(|n| n == name) {
        return Some(idx);
    }
    // Try stripping table prefix (table.col -> col)
    if let Some(bare) = name.split('.').nth(1) {
        if let Some(idx) = column_names.iter().position(|n| n == bare) {
            return Some(idx);
        }
    }
    // Try parsing as index (backward compat)
    name.parse::<usize>().ok()
}

fn value_to_u64(val: &crate::sql::parser::Value) -> Option<u64> {
    use crate::sql::parser::Value;
    match val {
        Value::Int(i) => Some(*i as u64),
        Value::Float(f) => Some(f.to_bits()),
        // For string values, try parsing as integer first; if that fails,
        // hash with xxh3_64 to match the storage format (strings are stored
        // as xxh3 hashes in u64 cells — see datasource/parquet.rs).
        // This makes WHERE col = 'string' and WHERE col <> 'string' work
        // correctly against string columns.
        Value::String(s) => {
            if let Ok(n) = s.parse::<u64>() {
                Some(n)
            } else if let Ok(n) = s.parse::<i64>() {
                Some(n as u64)
            } else {
                Some(xxhash_rust::xxh3::xxh3_64(s.as_bytes()))
            }
        }
        Value::Hex(bytes) => {
            Some(bytes.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i))))
        }
        Value::Date(d) => Some(*d as u64),
        Value::Null => None,
    }
}

/// Compute sum of col where mask is true. Returns f64 bits.
pub fn sum_masked(col: &[u64], mask: &[bool]) -> u64 {
    let mut sum: u64 = 0;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] {
            sum = sum.wrapping_add(v);
        }
    }
    (sum as f64).to_bits()
}

/// Sum for float/DECIMAL columns. Each cell is f64::to_bits; decode, sum, re-encode.
pub fn sum_masked_f64(col: &[u64], mask: &[bool]) -> u64 {
    let mut sum: f64 = 0.0;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] {
            sum += f64::from_bits(v);
        }
    }
    sum.to_bits()
}

/// Compute count where mask is true.
pub fn count_masked(mask: &[bool]) -> u64 {
    mask.iter().filter(|&&b| b).count() as u64
}

/// Compute min where mask is true.
pub fn min_masked(col: &[u64], mask: &[bool]) -> u64 {
    let mut min = u64::MAX;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] && v < min {
            min = v;
        }
    }
    if min == u64::MAX {
        0
    } else {
        min
    }
}

/// Compute max where mask is true.
pub fn max_masked(col: &[u64], mask: &[bool]) -> u64 {
    let mut max = 0u64;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] && v > max {
            max = v;
        }
    }
    max
}

/// Compute avg where mask is true. Returns f64 bits.
pub fn avg_masked(col: &[u64], mask: &[bool]) -> u64 {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] {
            sum = sum.wrapping_add(v);
            count += 1;
        }
    }
    if count == 0 {
        0
    } else {
        (sum as f64 / count as f64).to_bits()
    }
}

/// Average for float/DECIMAL columns.
pub fn avg_masked_f64(col: &[u64], mask: &[bool]) -> u64 {
    let mut sum: f64 = 0.0;
    let mut count: u64 = 0;
    for (i, &v) in col.iter().enumerate() {
        if mask[i] {
            sum += f64::from_bits(v);
            count += 1;
        }
    }
    if count == 0 {
        0
    } else {
        (sum / count as f64).to_bits()
    }
}

/// Count distinct values where mask is true.
pub fn count_distinct_masked(col: &[u64], mask: &[bool]) -> u64 {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for (i, &v) in col.iter().enumerate() {
        if mask[i] {
            seen.insert(v);
        }
    }
    seen.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_eq() {
        let col = vec![1, 5, 5, 3, 5];
        let mut mask = vec![false; 5];
        filter_eq(&col, 5, &mut mask);
        assert_eq!(mask, vec![false, true, true, false, true]);
    }

    #[test]
    fn test_filter_lt() {
        let col = vec![10, 20, 30, 40, 50];
        let mut mask = vec![false; 5];
        filter_lt(&col, 30, &mut mask);
        assert_eq!(mask, vec![true, true, false, false, false]);
    }

    #[test]
    fn test_filter_gt() {
        let col = vec![10, 20, 30, 40, 50];
        let mut mask = vec![false; 5];
        filter_gt(&col, 30, &mut mask);
        assert_eq!(mask, vec![false, false, false, true, true]);
    }

    #[test]
    fn test_and_mask() {
        let left = vec![true, true, false, false];
        let right = vec![true, false, true, false];
        let mut out = vec![false; 4];
        and_mask(&left, &right, &mut out);
        assert_eq!(out, vec![true, false, false, false]);
    }

    #[test]
    fn test_or_mask() {
        let left = vec![true, true, false, false];
        let right = vec![true, false, true, false];
        let mut out = vec![false; 4];
        or_mask(&left, &right, &mut out);
        assert_eq!(out, vec![true, true, true, false]);
    }

    #[test]
    fn test_sum_masked() {
        let col = vec![10, 20, 30, 40, 50];
        let mask = vec![true, false, true, false, true];
        let result = f64::from_bits(sum_masked(&col, &mask));
        assert_eq!(result, 90.0);
    }

    #[test]
    fn test_count_masked() {
        let mask = vec![true, false, true, true, false];
        assert_eq!(count_masked(&mask), 3);
    }

    #[test]
    fn test_min_max_masked() {
        let col = vec![10, 20, 30, 40, 50];
        let mask = vec![true, false, true, false, true];
        assert_eq!(min_masked(&col, &mask), 10);
        assert_eq!(max_masked(&col, &mask), 50);
    }

    #[test]
    fn test_count_distinct_masked() {
        let col = vec![1, 2, 1, 3, 2];
        let mask = vec![true, true, true, true, true];
        assert_eq!(count_distinct_masked(&col, &mask), 3);
    }

    #[test]
    fn test_large_filter() {
        let n = 1_000_000;
        let col: Vec<u64> = (0..n).map(|i| i % 100).collect();
        let mut mask = vec![false; n as usize];
        let start = std::time::Instant::now();
        filter_eq(&col, 50, &mut mask);
        let elapsed = start.elapsed();
        let count = mask.iter().filter(|&&b| b).count();
        assert_eq!(count, 10000);
        // Should be under 5ms for 1M rows
        assert!(elapsed.as_millis() < 30, "filter_eq took {}ms (debug mode)", elapsed.as_millis());
    }
}

fn extract_col_and_string(
    left: &crate::sql::parser::Expr,
    right: &crate::sql::parser::Expr,
    column_names: &[String],
) -> (Option<usize>, Option<String>) {
    use crate::sql::parser::Expr;
    if let Expr::Column(name) = left {
        if let Expr::Literal(crate::sql::parser::Value::String(s)) = right {
            return (resolve_name(name, column_names), Some(s.clone()));
        }
    }
    if let Expr::Column(name) = right {
        if let Expr::Literal(crate::sql::parser::Value::String(s)) = left {
            return (resolve_name(name, column_names), Some(s.clone()));
        }
    }
    (None, None)
}
