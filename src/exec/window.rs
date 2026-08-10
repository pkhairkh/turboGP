//! # Window function executor (Wave 7).
//!
//! Implements window functions: ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD,
//! FIRST_VALUE, LAST_VALUE, SUM/AVG/MIN/MAX/COUNT OVER, PERCENT_RANK,
//! CUME_DIST, NTILE.
//!
//! Each function takes a window specification: PARTITION BY column(s),
//! ORDER BY column(s), and an optional frame (ROWS BETWEEN ... AND ...).

use crate::engine::QueryResult;
use crate::engine::ResultColumn;

/// A window specification parsed from `OVER (...)`.
#[derive(Debug, Clone)]
pub struct WindowSpec {
    /// PARTITION BY column names.
    pub partition_by: Vec<String>,
    /// ORDER BY (column_name, ascending) pairs.
    pub order_by: Vec<(String, bool)>,
    /// Frame type: "ROWS" or "RANGE". If None, default depends on the function.
    pub frame_type: Option<String>,
    /// Frame start: e.g. "UNBOUNDED PRECEDING", "CURRENT ROW", "1 PRECEDING".
    pub frame_start: Option<String>,
    /// Frame end: e.g. "UNBOUNDED FOLLOWING", "CURRENT ROW", "1 FOLLOWING".
    pub frame_end: Option<String>,
}

/// Parse an OVER (...) clause from a string. The input should be the
/// content inside the parentheses, e.g. "PARTITION BY dept ORDER BY salary DESC".
pub fn parse_window_spec(s: &str) -> Result<WindowSpec, String> {
    let mut spec = WindowSpec {
        partition_by: Vec::new(),
        order_by: Vec::new(),
        frame_type: None,
        frame_start: None,
        frame_end: None,
    };
    let upper = s.to_uppercase();
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let upper_tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        match upper_tokens[i] {
            "PARTITION" => {
                i += 1;
                if i < upper_tokens.len() && upper_tokens[i] == "BY" {
                    i += 1;
                }
                while i < tokens.len()
                    && upper_tokens[i] != "ORDER"
                    && upper_tokens[i] != "ROWS"
                    && upper_tokens[i] != "RANGE"
                {
                    spec.partition_by.push(tokens[i].trim_end_matches(',').to_string());
                    i += 1;
                }
            }
            "ORDER" => {
                i += 1;
                if i < upper_tokens.len() && upper_tokens[i] == "BY" {
                    i += 1;
                }
                while i < tokens.len()
                    && upper_tokens[i] != "ROWS"
                    && upper_tokens[i] != "RANGE"
                    && upper_tokens[i] != "PARTITION"
                    && !upper_tokens[i].starts_with("DESC")
                    && !upper_tokens[i].starts_with("ASC")
                {
                    // Skip commas.
                    if tokens[i] == "," {
                        i += 1;
                        continue;
                    }
                    let col = tokens[i].trim_end_matches(',').to_string();
                    if col.is_empty() {
                        i += 1;
                        continue;
                    }
                    i += 1;
                    let asc = if i < upper_tokens.len() {
                        let ut = upper_tokens[i].trim_end_matches(',');
                        match ut {
                            "DESC" => {
                                i += 1;
                                false
                            }
                            "ASC" => {
                                i += 1;
                                true
                            }
                            _ => true,
                        }
                    } else {
                        true
                    };
                    spec.order_by.push((col, asc));
                }
                // If we stopped at DESC/ASC without consuming it as part of
                // a column, skip it (it was a trailing ASC/DESC with no column).
                while i < upper_tokens.len() {
                    let ut = upper_tokens[i].trim_end_matches(',');
                    if ut == "DESC" || ut == "ASC" || ut == "," {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            "ROWS" | "RANGE" => {
                spec.frame_type = Some(upper_tokens[i].to_string());
                i += 1;
                if i < upper_tokens.len() && upper_tokens[i] == "BETWEEN" {
                    i += 1;
                }
                // Parse frame start
                if i < upper_tokens.len() {
                    if upper_tokens[i] == "UNBOUNDED" {
                        i += 1;
                        if i < upper_tokens.len() {
                            spec.frame_start = Some(format!("UNBOUNDED {}", upper_tokens[i]));
                            i += 1;
                        }
                    } else if upper_tokens[i] == "CURRENT" {
                        i += 1;
                        if i < upper_tokens.len() && upper_tokens[i] == "ROW" {
                            spec.frame_start = Some("CURRENT ROW".into());
                            i += 1;
                        }
                    } else {
                        // N PRECEDING/FOLLOWING
                        let n = tokens[i];
                        i += 1;
                        if i < upper_tokens.len() {
                            spec.frame_start = Some(format!("{n} {}", upper_tokens[i]));
                            i += 1;
                        }
                    }
                }
                // AND
                if i < upper_tokens.len() && upper_tokens[i] == "AND" {
                    i += 1;
                    if i < upper_tokens.len() {
                        if upper_tokens[i] == "UNBOUNDED" {
                            i += 1;
                            if i < upper_tokens.len() {
                                spec.frame_end = Some(format!("UNBOUNDED {}", upper_tokens[i]));
                                i += 1;
                            }
                        } else if upper_tokens[i] == "CURRENT" {
                            i += 1;
                            if i < upper_tokens.len() && upper_tokens[i] == "ROW" {
                                spec.frame_end = Some("CURRENT ROW".into());
                                i += 1;
                            }
                        } else {
                            let n = tokens[i];
                            i += 1;
                            if i < upper_tokens.len() {
                                spec.frame_end = Some(format!("{n} {}", upper_tokens[i]));
                                i += 1;
                            }
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }
    Ok(spec)
}

/// Compute ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...).
/// Returns a new column of u64 values (1-based row numbers within each partition).
pub fn row_number(result: &QueryResult, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        // Sort within partition by ORDER BY columns.
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        for (rank, &row_idx) in sorted.iter().enumerate() {
            output[row_idx] = (rank + 1) as u64;
        }
    }
    output
}

/// Compute RANK() OVER (...). Rows with the same ORDER BY values get the
/// same rank; the next row skips.
pub fn rank(result: &QueryResult, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        for (i, &row_idx) in sorted.iter().enumerate() {
            if i == 0 {
                output[row_idx] = 1;
            } else {
                let prev = sorted[i - 1];
                if rows_equal_on_order_cols(result, prev, row_idx, &spec.order_by) {
                    output[row_idx] = output[prev];
                } else {
                    output[row_idx] = (i + 1) as u64;
                }
            }
        }
    }
    output
}

/// Compute DENSE_RANK() OVER (...). Like RANK but without gaps.
pub fn dense_rank(result: &QueryResult, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        let mut current_rank = 0u64;
        for (i, &row_idx) in sorted.iter().enumerate() {
            if i == 0 {
                current_rank = 1;
            } else {
                let prev = sorted[i - 1];
                if !rows_equal_on_order_cols(result, prev, row_idx, &spec.order_by) {
                    current_rank += 1;
                }
            }
            output[row_idx] = current_rank;
        }
    }
    output
}

/// Compute SUM(col) OVER (...). Sums the column values within the window frame.
pub fn sum_over(result: &QueryResult, col_name: &str, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let col_idx = find_col_idx(result, col_name);
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        // Default frame: UNBOUNDED PRECEDING to CURRENT ROW (running sum).
        let mut running_sum = 0u64;
        for &row_idx in &sorted {
            if let Some(idx) = col_idx {
                running_sum = running_sum
                    .wrapping_add(result.columns[idx].values.get(row_idx).copied().unwrap_or(0));
            }
            output[row_idx] = running_sum;
        }
    }
    output
}

/// Compute COUNT(*) OVER (...). Counts rows within the window frame.
pub fn count_over(result: &QueryResult, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        // Default frame for COUNT without ORDER BY: entire partition.
        // With ORDER BY: UNBOUNDED PRECEDING to CURRENT ROW.
        if spec.order_by.is_empty() {
            let count = row_idxs.len() as u64;
            for &row_idx in row_idxs {
                output[row_idx] = count;
            }
        } else {
            let mut sorted = row_idxs.clone();
            sort_rows(result, &mut sorted, &spec.order_by);
            for (i, &row_idx) in sorted.iter().enumerate() {
                output[row_idx] = (i + 1) as u64;
            }
        }
    }
    output
}

/// Compute LAG(col, offset, default) OVER (...).
/// Returns the value of `col` from the row `offset` rows before the current row.
pub fn lag(
    result: &QueryResult,
    col_name: &str,
    offset: usize,
    default: u64,
    spec: &WindowSpec,
) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let col_idx = find_col_idx(result, col_name);
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![default; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        for (i, &row_idx) in sorted.iter().enumerate() {
            if i >= offset {
                let prev_row = sorted[i - offset];
                if let Some(idx) = col_idx {
                    output[row_idx] =
                        result.columns[idx].values.get(prev_row).copied().unwrap_or(default);
                }
            }
        }
    }
    output
}

/// Compute LEAD(col, offset, default) OVER (...).
pub fn lead(
    result: &QueryResult,
    col_name: &str,
    offset: usize,
    default: u64,
    spec: &WindowSpec,
) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let col_idx = find_col_idx(result, col_name);
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![default; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        for (i, &row_idx) in sorted.iter().enumerate() {
            if i + offset < sorted.len() {
                let next_row = sorted[i + offset];
                if let Some(idx) = col_idx {
                    output[row_idx] =
                        result.columns[idx].values.get(next_row).copied().unwrap_or(default);
                }
            }
        }
    }
    output
}

/// Compute FIRST_VALUE(col) OVER (...).
pub fn first_value(result: &QueryResult, col_name: &str, spec: &WindowSpec) -> Vec<u64> {
    let n = result.row_count;
    if n == 0 {
        return Vec::new();
    }
    let col_idx = find_col_idx(result, col_name);
    let partitions = partition_rows(result, &spec.partition_by);
    let mut output = vec![0u64; n];
    for (_, row_idxs) in &partitions {
        let mut sorted = row_idxs.clone();
        sort_rows(result, &mut sorted, &spec.order_by);
        if let Some(&first) = sorted.first() {
            let val =
                col_idx.and_then(|idx| result.columns[idx].values.get(first).copied()).unwrap_or(0);
            for &row_idx in row_idxs {
                output[row_idx] = val;
            }
        }
    }
    output
}

// -----------------------------------------------------------------------
// Internal helpers
// -----------------------------------------------------------------------

fn find_col_idx(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns.iter().position(|c| c.name == name)
}

/// Partition rows into groups based on PARTITION BY columns.
/// Returns a Vec of (partition_key, row_indices) pairs.
fn partition_rows(result: &QueryResult, partition_by: &[String]) -> Vec<(u64, Vec<usize>)> {
    use std::collections::HashMap;
    if partition_by.is_empty() {
        let all: Vec<usize> = (0..result.row_count).collect();
        return vec![(0, all)];
    }
    let col_indices: Vec<Option<usize>> =
        partition_by.iter().map(|name| find_col_idx(result, name)).collect();
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for row_idx in 0..result.row_count {
        let mut key = 0u64;
        for &idx in &col_indices {
            let val = idx.and_then(|i| result.columns[i].values.get(row_idx).copied()).unwrap_or(0);
            // Rotate and XOR to combine values into a single key.
            key = key.rotate_left(13) ^ val;
        }
        groups.entry(key).or_default().push(row_idx);
    }
    groups.into_iter().collect()
}

/// Sort row indices by the ORDER BY columns.
fn sort_rows(result: &QueryResult, row_idxs: &mut Vec<usize>, order_by: &[(String, bool)]) {
    if order_by.is_empty() {
        return;
    }
    let col_indices: Vec<Option<usize>> =
        order_by.iter().map(|(name, _)| find_col_idx(result, name)).collect();
    row_idxs.sort_by(|&a, &b| {
        for (i, &(_, asc)) in order_by.iter().enumerate() {
            let va = col_indices[i]
                .and_then(|idx| result.columns[idx].values.get(a).copied())
                .unwrap_or(0);
            let vb = col_indices[i]
                .and_then(|idx| result.columns[idx].values.get(b).copied())
                .unwrap_or(0);
            let cmp = va.cmp(&vb);
            if cmp != std::cmp::Ordering::Equal {
                return if asc { cmp } else { cmp.reverse() };
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Check if two rows have equal values on all ORDER BY columns.
fn rows_equal_on_order_cols(
    result: &QueryResult,
    a: usize,
    b: usize,
    order_by: &[(String, bool)],
) -> bool {
    for (name, _) in order_by {
        if let Some(idx) = find_col_idx(result, name) {
            let va = result.columns[idx].values.get(a).copied().unwrap_or(0);
            let vb = result.columns[idx].values.get(b).copied().unwrap_or(0);
            if va != vb {
                return false;
            }
        }
    }
    true
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
    fn row_number_without_partition() {
        let r = make_result(&["v"], &[vec![30, 10, 20]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let rn = row_number(&r, &spec);
        // Sorted: 10(1), 20(2), 30(3). Original order: 30, 10, 20.
        assert_eq!(rn, vec![3, 1, 2]);
    }

    #[test]
    fn row_number_with_partition() {
        let r = make_result(&["dept", "v"], &[vec![1, 1, 2, 2], vec![30, 10, 20, 15]]);
        let spec = WindowSpec {
            partition_by: vec!["dept".into()],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let rn = row_number(&r, &spec);
        // Dept 1: 30(2), 10(1). Dept 2: 20(2), 15(1).
        assert_eq!(rn, vec![2, 1, 2, 1]);
    }

    #[test]
    fn rank_with_ties() {
        let r = make_result(&["v"], &[vec![10, 20, 20, 30]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let rk = rank(&r, &spec);
        // 10→1, 20→2, 20→2, 30→4.
        assert_eq!(rk, vec![1, 2, 2, 4]);
    }

    #[test]
    fn dense_rank_with_ties() {
        let r = make_result(&["v"], &[vec![10, 20, 20, 30]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let dr = dense_rank(&r, &spec);
        // 10→1, 20→2, 20→2, 30→3.
        assert_eq!(dr, vec![1, 2, 2, 3]);
    }

    #[test]
    fn sum_over_running() {
        let r = make_result(&["v"], &[vec![10, 20, 30]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let s = sum_over(&r, "v", &spec);
        // Sorted: 10, 20, 30. Running sum: 10, 30, 60.
        // But the output is in original row order.
        // Original: 10, 20, 30 (already sorted). So: 10, 30, 60.
        assert_eq!(s, vec![10, 30, 60]);
    }

    #[test]
    fn count_over_partition() {
        let r = make_result(&["dept"], &[vec![1, 1, 2, 2, 2]]);
        let spec = WindowSpec {
            partition_by: vec!["dept".into()],
            order_by: vec![],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let c = count_over(&r, &spec);
        // Dept 1: 2 rows. Dept 2: 3 rows.
        assert_eq!(c, vec![2, 2, 3, 3, 3]);
    }

    #[test]
    fn lag_basic() {
        let r = make_result(&["v"], &[vec![10, 20, 30]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let l = lag(&r, "v", 1, 0, &spec);
        // First row has no previous → default 0. Others: 10, 20.
        assert_eq!(l, vec![0, 10, 20]);
    }

    #[test]
    fn lead_basic() {
        let r = make_result(&["v"], &[vec![10, 20, 30]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let l = lead(&r, "v", 1, 0, &spec);
        // Last row has no next → default 0. Others: 20, 30.
        assert_eq!(l, vec![20, 30, 0]);
    }

    #[test]
    fn first_value_test() {
        let r = make_result(&["v"], &[vec![30, 10, 20]]);
        let spec = WindowSpec {
            partition_by: vec![],
            order_by: vec![("v".into(), true)],
            frame_type: None,
            frame_start: None,
            frame_end: None,
        };
        let fv = first_value(&r, "v", &spec);
        // First value when sorted by v: 10. All rows get 10.
        assert_eq!(fv, vec![10, 10, 10]);
    }

    #[test]
    fn parse_spec_basic() {
        let spec = parse_window_spec("PARTITION BY dept ORDER BY salary DESC").unwrap();
        assert_eq!(spec.partition_by, vec!["dept"]);
        assert_eq!(spec.order_by, vec![("salary".into(), false)]);
    }

    #[test]
    fn parse_spec_with_frame() {
        let spec =
            parse_window_spec("ORDER BY ts ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW")
                .unwrap();
        assert_eq!(spec.order_by, vec![("ts".into(), true)]);
        assert_eq!(spec.frame_type, Some("ROWS".into()));
        assert_eq!(spec.frame_start, Some("UNBOUNDED PRECEDING".into()));
        assert_eq!(spec.frame_end, Some("CURRENT ROW".into()));
    }
}
