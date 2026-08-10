//! **WIRED INTO SQL EXECUTION (Wave 56b)** — this module is reachable
//! through `QueryEngine::execute()` via `parse_pivot_clause` in
//! `engine/mod.rs`. The engine detects `PIVOT (...)` in the SQL string,
//! parses the spec (agg, value_col, pivot_col, pivot_values), strips the
//! PIVOT clause, executes the underlying SELECT, and applies `pivot()` to
//! the result. The group_col is auto-detected as the first input column
//! that's neither the pivot_col nor the value_col.
//!
//! Supported SQL syntax:
//! ```sql
//!   SELECT * FROM sales PIVOT (SUM(amt) FOR qtr IN (1, 2)) AS p
//!   SELECT * FROM sales PIVOT (COUNT(*) FOR qtr IN ('Q1', 'Q2'))
//! ```
//! # PIVOT / UNPIVOT / GROUPING SETS executor (Wave 8).
//!
//! Implements:
//! - PIVOT: rotates rows to columns (cross-tabulation)
//! - UNPIVOT: rotates columns to rows (normalization)
//! - GROUPING SETS: multiple GROUP BY levels in one query
//! - CUBE: all combinations of grouping columns
//! - ROLLUP: hierarchical grouping

use crate::engine::{QueryResult, ResultColumn};

/// Pivot a result set: for each group of rows sharing the same value in
/// the `group_col`, produce one output row with the aggregated values
/// spread across columns named after the pivot values.
///
/// Example: `PIVOT (SUM(amount) FOR quarter IN [Q1, Q2, Q3, Q4])`
/// Input: rows of (dept, quarter, amount). Output: rows of (dept, Q1_sum, Q2_sum, ...).
pub fn pivot(
    input: &QueryResult,
    group_col: &str,
    pivot_col: &str,
    value_col: &str,
    pivot_values: &[String],
    agg: &str,
) -> QueryResult {
    let group_idx = find_col(input, group_col);
    let pivot_idx = find_col(input, pivot_col);
    let value_idx = find_col(input, value_col);

    if group_idx.is_none() || pivot_idx.is_none() || value_idx.is_none() {
        return QueryResult::empty();
    }

    let group_idx = group_idx.unwrap();
    let pivot_idx = pivot_idx.unwrap();
    let value_idx = value_idx.unwrap();

    // Collect unique group values (preserve first-seen order).
    let mut groups: Vec<u64> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for row in 0..input.row_count {
        let g = input.columns[group_idx].values.get(row).copied().unwrap_or(0);
        if seen.insert(g) {
            groups.push(g);
        }
    }

    // Build output: one column for group_col, one per pivot value.
    let mut out_cols = Vec::new();
    out_cols.push(ResultColumn {
        name: group_col.to_string(),
        values: groups.clone(),
        string_values: None,
        type_oid: 0,
        null_mask: None,
    });

    for pv in pivot_values {
        let pv_cell = string_to_cell(pv);
        let mut col_vals = Vec::with_capacity(groups.len());
        for &g in &groups {
            // Aggregate all rows where group_col = g AND pivot_col = pv.
            let mut agg_val = 0u64;
            let mut count = 0u64;
            for row in 0..input.row_count {
                let rg = input.columns[group_idx].values.get(row).copied().unwrap_or(0);
                let rp = input.columns[pivot_idx].values.get(row).copied().unwrap_or(0);
                if rg == g && rp == pv_cell {
                    let v = input.columns[value_idx].values.get(row).copied().unwrap_or(0);
                    if agg == "SUM" || agg == "AVG" {
                        agg_val = agg_val.wrapping_add(v);
                    } else if agg == "MAX" {
                        agg_val = agg_val.max(v);
                    } else if agg == "MIN" {
                        agg_val = if count == 0 { v } else { agg_val.min(v) };
                    }
                    count += 1;
                }
            }
            let final_val = match agg {
                "COUNT" => count,
                "AVG" => {
                    if count > 0 {
                        agg_val / count
                    } else {
                        0
                    }
                }
                _ => agg_val,
            };
            col_vals.push(final_val);
        }
        out_cols.push(ResultColumn {
            name: pv.clone(),
            values: col_vals,
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }

    let mut result = QueryResult::empty();
    result.row_count = groups.len();
    for col in out_cols {
        result.push_column(col).unwrap();
    }
    result
}

/// Unpivot a result set: rotates columns to rows.
///
/// Example: `UNPIVOT (amount FOR quarter IN (Q1, Q2, Q3, Q4))`
/// Input: rows of (dept, Q1, Q2, Q3, Q4). Output: rows of (dept, quarter, amount).
pub fn unpivot(
    input: &QueryResult,
    id_cols: &[String],
    value_col_name: &str,
    label_col_name: &str,
    unpivot_cols: &[String],
) -> QueryResult {
    let mut out_cols: Vec<ResultColumn> = Vec::new();
    let mut out_row_count = 0usize;

    // For each id column, we'll have one output column.
    // For the label and value columns, we'll have one each.
    let total_out_rows = input.row_count * unpivot_cols.len();
    out_row_count = total_out_rows;

    // ID columns: repeat each value unpivot_cols.len() times.
    for id_col_name in id_cols {
        let idx = find_col(input, id_col_name);
        let mut vals = Vec::with_capacity(total_out_rows);
        if let Some(idx) = idx {
            for row in 0..input.row_count {
                let v = input.columns[idx].values.get(row).copied().unwrap_or(0);
                for _ in 0..unpivot_cols.len() {
                    vals.push(v);
                }
            }
        } else {
            vals.resize(total_out_rows, 0);
        }
        out_cols.push(ResultColumn {
            name: id_col_name.clone(),
            values: vals,
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }

    // Label column: the column names repeated for each input row.
    let mut label_vals = Vec::with_capacity(total_out_rows);
    for row in 0..input.row_count {
        for col_name in unpivot_cols {
            label_vals.push(string_to_cell(col_name));
        }
    }
    out_cols.push(ResultColumn {
        name: label_col_name.to_string(),
        values: label_vals,
        string_values: None,
        type_oid: 0,
        null_mask: None,
    });

    // Value column: the actual values from each unpivot column.
    let mut value_vals = Vec::with_capacity(total_out_rows);
    for row in 0..input.row_count {
        for col_name in unpivot_cols {
            let idx = find_col(input, col_name);
            let v = idx.and_then(|i| input.columns[i].values.get(row).copied()).unwrap_or(0);
            value_vals.push(v);
        }
    }
    out_cols.push(ResultColumn {
        name: value_col_name.to_string(),
        values: value_vals,
        string_values: None,
        type_oid: 0,
        null_mask: None,
    });

    let mut result = QueryResult::empty();
    result.row_count = out_row_count;
    for col in out_cols {
        result.push_column(col).unwrap();
    }
    result
}

/// Compute GROUPING SETS: runs the query for each grouping set and
/// concatenates the results. Each grouping set is a list of column names
/// to GROUP BY. Rows are padded with NULL (0) for columns not in the
/// current grouping set.
pub fn grouping_sets(
    input: &QueryResult,
    group_cols: &[String],
    agg_col: &str,
    agg: &str,
    sets: &[Vec<usize>],
) -> QueryResult {
    let agg_idx = find_col(input, agg_col);
    let mut out_cols: Vec<ResultColumn> = Vec::new();
    for col_name in group_cols {
        out_cols.push(ResultColumn {
            name: col_name.clone(),
            values: Vec::new(),
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }
    out_cols.push(ResultColumn {
        name: format!("{}_{}", agg.to_lowercase(), agg_col),
        values: Vec::new(),
        string_values: None,
        type_oid: 0,
        null_mask: None,
    });

    // For each grouping set, compute the aggregate.
    for set in sets {
        // Group rows by the columns in this set.
        let mut groups: std::collections::HashMap<Vec<u64>, Vec<usize>> =
            std::collections::HashMap::new();
        for row in 0..input.row_count {
            let key: Vec<u64> = set
                .iter()
                .map(|&col_idx| {
                    input.columns.get(col_idx).and_then(|c| c.values.get(row)).copied().unwrap_or(0)
                })
                .collect();
            groups.entry(key).or_default().push(row);
        }

        for (key, rows) in &groups {
            // For each group column in the full list, output the key value
            // if it's in this set, or 0 (NULL) otherwise.
            for (col_i, _col_name) in group_cols.iter().enumerate() {
                let in_set = set.iter().position(|&s| s == col_i);
                let val = if let Some(key_idx) = in_set {
                    key[key_idx]
                } else {
                    0 // NULL
                };
                out_cols[col_i].values.push(val);
            }

            // Compute the aggregate.
            let mut agg_val = 0u64;
            let mut count = 0u64;
            for &row in rows {
                let v = agg_idx
                    .and_then(|idx| input.columns[idx].values.get(row).copied())
                    .unwrap_or(0);
                if agg == "SUM" || agg == "AVG" {
                    agg_val = agg_val.wrapping_add(v);
                } else if agg == "MAX" {
                    agg_val = agg_val.max(v);
                } else if agg == "MIN" {
                    agg_val = if count == 0 { v } else { agg_val.min(v) };
                }
                count += 1;
            }
            let final_val = match agg {
                "COUNT" => count,
                "AVG" => {
                    if count > 0 {
                        agg_val / count
                    } else {
                        0
                    }
                }
                _ => agg_val,
            };
            out_cols.last_mut().unwrap().values.push(final_val);
        }
    }

    let mut result = QueryResult::empty();
    result.row_count = out_cols.first().map(|c| c.values.len()).unwrap_or(0);
    for col in out_cols {
        result.push_column(col).unwrap();
    }
    result
}

/// Compute CUBE: all 2^n combinations of the grouping columns.
pub fn cube(input: &QueryResult, group_cols: &[String], agg_col: &str, agg: &str) -> QueryResult {
    let n = group_cols.len();
    let mut sets = Vec::new();
    for mask in 1u32..(1 << n) {
        let set: Vec<usize> = (0..n).filter(|&i| (mask >> i) & 1 == 1).collect();
        sets.push(set);
    }
    sets.push(Vec::new()); // grand total
    grouping_sets(input, group_cols, agg_col, agg, &sets)
}

/// Compute ROLLUP: hierarchical grouping (prefix subsets).
pub fn rollup(input: &QueryResult, group_cols: &[String], agg_col: &str, agg: &str) -> QueryResult {
    let n = group_cols.len();
    let mut sets = Vec::new();
    for k in (1..=n).rev() {
        sets.push((0..k).collect());
    }
    sets.push(Vec::new()); // grand total
    grouping_sets(input, group_cols, agg_col, agg, &sets)
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn find_col(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns.iter().position(|c| c.name == name)
}

fn string_to_cell(s: &str) -> u64 {
    // Try to parse as integer first (for numeric pivot values).
    if let Ok(n) = s.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = s.parse::<u64>() {
        return n;
    }
    // Fall back to hashing (for string pivot values).
    use xxhash_rust::xxh3;
    xxh3::xxh3_64(s.as_bytes())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(names: &[&str], cols: &[Vec<u64>]) -> QueryResult {
        let mut r = QueryResult::empty();
        for (i, name) in names.iter().enumerate() {
            r.push_column(ResultColumn {
                name: name.to_string(),
                values: cols[i].clone(),
                string_values: None,
                type_oid: 0,
                null_mask: None,
            })
            .unwrap();
        }
        r
    }

    #[test]
    fn pivot_basic() {
        // Input: (dept, quarter, amount) = (1,1,100), (1,2,200), (2,1,150)
        // quarter values are integers 1 and 2 (matching pivot_values "1","2").
        let r = make_result(
            &["dept", "quarter", "amount"],
            &[vec![1, 1, 2], vec![1, 2, 1], vec![100, 200, 150]],
        );
        let p = pivot(&r, "dept", "quarter", "amount", &["1".into(), "2".into()], "SUM");
        assert_eq!(p.row_count, 2);
        assert_eq!(p.columns.len(), 3); // dept + "1" + "2"
        assert_eq!(p.columns[0].values, vec![1, 2]);
        // Dept 1: quarter 1 = 100, quarter 2 = 200. Dept 2: quarter 1 = 150, quarter 2 = 0.
        assert_eq!(p.columns[1].values, vec![100, 150]);
        assert_eq!(p.columns[2].values, vec![200, 0]);
    }

    #[test]
    fn unpivot_basic() {
        // Input: (dept, Q1, Q2) = (1, 100, 200), (2, 150, 250)
        let r = make_result(&["dept", "Q1", "Q2"], &[vec![1, 2], vec![100, 150], vec![200, 250]]);
        let u = unpivot(&r, &["dept".into()], "amount", "quarter", &["Q1".into(), "Q2".into()]);
        assert_eq!(u.row_count, 4); // 2 rows × 2 quarters
        assert_eq!(u.columns[0].values, vec![1, 1, 2, 2]); // dept repeated
    }

    #[test]
    fn grouping_sets_basic() {
        // Input: (dept, team, amount)
        let r = make_result(
            &["dept", "team", "amount"],
            &[vec![1, 1, 2], vec![1, 2, 1], vec![100, 50, 200]],
        );
        // Group by dept only (set = [0]).
        let gs = grouping_sets(&r, &["dept".into(), "team".into()], "amount", "SUM", &[vec![0]]);
        assert!(gs.row_count > 0);
    }

    #[test]
    fn cube_basic() {
        let r = make_result(&["dept", "amount"], &[vec![1, 1, 2], vec![100, 200, 150]]);
        let c = cube(&r, &["dept".into()], "amount", "SUM");
        assert!(c.row_count > 0);
    }

    #[test]
    fn rollup_basic() {
        let r = make_result(&["dept", "amount"], &[vec![1, 1, 2], vec![100, 200, 150]]);
        let ru = rollup(&r, &["dept".into()], "amount", "SUM");
        assert!(ru.row_count > 0);
    }
}
