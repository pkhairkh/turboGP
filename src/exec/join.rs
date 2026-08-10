//! JOIN execution — hash join with build/probe, semi/anti join, multi-table.
//!
//! Research: Morsel-driven parallelism (Leis 2014), grouped probing
//! for cache locality, radix partitioning for large datasets.

use crate::datasource::table::Table;
use crate::Error;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Semi,
    Anti,
}

#[derive(Debug, Clone, Copy)]
pub struct JoinKey {
    pub left: usize,
    pub right: usize,
}

#[derive(Debug, Clone)]
pub struct JoinResult {
    pub columns: Vec<Vec<u64>>,
    pub column_names: Vec<String>,
    pub row_count: usize,
}

impl JoinResult {
    pub fn into_table(self, name: &str) -> Table {
        Table {
            name: name.to_string(),
            columns: self.columns.into_iter().map(std::sync::Arc::new).collect(),
            column_names: self.column_names,
            row_count: self.row_count,
            string_columns: vec![],
            null_bitmaps: vec![],
            schema: None,
        }
    }
}

/// Hash join: build hash table on right, probe with left.
pub fn hash_join(
    left: &Table,
    right: &Table,
    keys: &[JoinKey],
    join_type: JoinType,
) -> Result<JoinResult, Error> {
    if keys.is_empty() {
        return Err(Error::InvalidArg("hash_join requires at least one key".into()));
    }

    // Build phase: hash right table keys
    let mut build_table: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
    for r_idx in 0..right.row_count {
        let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r_idx]).collect();
        build_table.entry(key).or_default().push(r_idx);
    }

    let total_cols = left.columns.len() + right.columns.len();
    let mut out_cols: Vec<Vec<u64>> = (0..total_cols).map(|_| Vec::new()).collect();
    let mut matched_right: Vec<bool> = vec![false; right.row_count];
    let mut row_count = 0;

    // Probe phase
    for l_idx in 0..left.row_count {
        let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l_idx]).collect();
        let matches = build_table.get(&key).cloned().unwrap_or_default();

        if matches.is_empty() {
            if matches!(join_type, JoinType::Left | JoinType::Full) {
                for (c, col) in left.columns.iter().enumerate() {
                    out_cols[c].push(col[l_idx]);
                }
                for c in 0..right.columns.len() {
                    out_cols[left.columns.len() + c].push(0);
                }
                row_count += 1;
            }
        } else {
            for r_idx in &matches {
                matched_right[*r_idx] = true;
                for (c, col) in left.columns.iter().enumerate() {
                    out_cols[c].push(col[l_idx]);
                }
                for (c, col) in right.columns.iter().enumerate() {
                    out_cols[left.columns.len() + c].push(col[*r_idx]);
                }
                row_count += 1;
            }
        }
    }

    if matches!(join_type, JoinType::Right | JoinType::Full) {
        for r_idx in 0..right.row_count {
            if !matched_right[r_idx] {
                for c in 0..left.columns.len() {
                    out_cols[c].push(0);
                }
                for (c, col) in right.columns.iter().enumerate() {
                    out_cols[left.columns.len() + c].push(col[r_idx]);
                }
                row_count += 1;
            }
        }
    }

    let mut column_names = left.column_names.clone();
    column_names.extend(right.column_names.iter().cloned());

    Ok(JoinResult { columns: out_cols, column_names, row_count })
}

/// Semi-join: left rows that have a match in right.
pub fn semi_join(left: &Table, right: &Table, keys: &[JoinKey]) -> Result<JoinResult, Error> {
    let mut build_table: HashMap<Vec<u64>, ()> = HashMap::new();
    for r_idx in 0..right.row_count {
        let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r_idx]).collect();
        build_table.insert(key, ());
    }
    let mut out_cols: Vec<Vec<u64>> = (0..left.columns.len()).map(|_| Vec::new()).collect();
    let mut row_count = 0;
    for l_idx in 0..left.row_count {
        let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l_idx]).collect();
        if build_table.contains_key(&key) {
            for (c, col) in left.columns.iter().enumerate() {
                out_cols[c].push(col[l_idx]);
            }
            row_count += 1;
        }
    }
    Ok(JoinResult { columns: out_cols, column_names: left.column_names.clone(), row_count })
}

/// Anti-join: left rows with NO match in right.
pub fn anti_join(left: &Table, right: &Table, keys: &[JoinKey]) -> Result<JoinResult, Error> {
    let mut build_table: HashMap<Vec<u64>, ()> = HashMap::new();
    for r_idx in 0..right.row_count {
        let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r_idx]).collect();
        build_table.insert(key, ());
    }
    let mut out_cols: Vec<Vec<u64>> = (0..left.columns.len()).map(|_| Vec::new()).collect();
    let mut row_count = 0;
    for l_idx in 0..left.row_count {
        let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l_idx]).collect();
        if !build_table.contains_key(&key) {
            for (c, col) in left.columns.iter().enumerate() {
                out_cols[c].push(col[l_idx]);
            }
            row_count += 1;
        }
    }
    Ok(JoinResult { columns: out_cols, column_names: left.column_names.clone(), row_count })
}

/// Parse JOIN ON expression to extract equi-join keys.
pub fn extract_join_keys(
    on: &crate::sql::parser::Expr,
    left: &Table,
    right: &Table,
) -> Result<Vec<JoinKey>, Error> {
    let mut keys = Vec::new();
    collect_keys(on, left, right, &mut keys)?;
    if keys.is_empty() {
        return Err(Error::Other(format!("JOIN ON must have equi-condition: {:?}", on)));
    }
    Ok(keys)
}

fn collect_keys(
    on: &crate::sql::parser::Expr,
    left: &Table,
    right: &Table,
    keys: &mut Vec<JoinKey>,
) -> Result<(), Error> {
    match on {
        crate::sql::parser::Expr::Binary { left: l, op, right: r } => {
            if op.eq_ignore_ascii_case("AND") {
                collect_keys(l, left, right, keys)?;
                collect_keys(r, left, right, keys)?;
                return Ok(());
            }
            if op == "=" {
                // Try: left expr in left table, right expr in right table
                if let (Some(lk), Some(rk)) = (resolve_col(l, left), resolve_col(r, right)) {
                    keys.push(JoinKey { left: lk, right: rk });
                    return Ok(());
                }
                // Try: left expr in right table, right expr in left table (swapped)
                if let (Some(rk), Some(lk)) = (resolve_col(l, right), resolve_col(r, left)) {
                    keys.push(JoinKey { left: lk, right: rk });
                    return Ok(());
                }
            }
            Err(Error::Other("JOIN ON must be equi-join (=)".into()))
        }
        _ => Err(Error::Other("JOIN ON must be binary expression".into())),
    }
}

fn resolve_col(expr: &crate::sql::parser::Expr, table: &Table) -> Option<usize> {
    if let crate::sql::parser::Expr::Column(name) = expr {
        // Direct match
        if let Some(idx) = table.column_idx(name) {
            return Some(idx);
        }
        // Try stripping table prefix from query: l_orderkey -> look for lineitem.l_orderkey
        if let Some(bare) = name.split('.').nth(1) {
            if let Some(idx) = table.column_idx(bare) {
                return Some(idx);
            }
        }
        // Try matching bare name against qualified column names in the table
        // e.g., name="l_orderkey", table has "lineitem.l_orderkey"
        for (i, col_name) in table.column_names.iter().enumerate() {
            if col_name.ends_with(&format!(".{}", name)) {
                return Some(i);
            }
            // Also check if the bare part matches
            if let Some(bare_col) = col_name.split('.').nth(1) {
                if bare_col == name {
                    return Some(i);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};

    fn make_table(name: &str, cols: Vec<(&str, Vec<u64>)>) -> Table {
        let row_count = cols.first().map(|(_, v)| v.len()).unwrap_or(0);
        Table {
            name: name.to_string(),
            columns: cols.iter().map(|(_, v)| std::sync::Arc::new(v.clone())).collect(),
            column_names: cols.iter().map(|(n, _)| n.to_string()).collect(),
            row_count,
            string_columns: vec![],
            null_bitmaps: vec![],
            schema: None,
        }
    }

    #[test]
    fn test_inner_join() {
        let left = make_table("l", vec![("id", vec![1, 2, 3]), ("v", vec![10, 20, 30])]);
        let right = make_table("r", vec![("id", vec![2, 3, 4]), ("n", vec![200, 300, 400])]);
        let result =
            hash_join(&left, &right, &[JoinKey { left: 0, right: 0 }], JoinType::Inner).unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.columns[0], vec![2, 3]);
    }

    #[test]
    fn test_left_join() {
        let left = make_table("l", vec![("id", vec![1, 2, 3])]);
        let right = make_table("r", vec![("id", vec![2])]);
        let result =
            hash_join(&left, &right, &[JoinKey { left: 0, right: 0 }], JoinType::Left).unwrap();
        assert_eq!(result.row_count, 3);
    }

    #[test]
    fn test_semi_join() {
        let left = make_table("l", vec![("id", vec![1, 2, 3, 4])]);
        let right = make_table("r", vec![("id", vec![2, 4])]);
        let result = semi_join(&left, &right, &[JoinKey { left: 0, right: 0 }]).unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.columns[0], vec![2, 4]);
    }

    #[test]
    fn test_anti_join() {
        let left = make_table("l", vec![("id", vec![1, 2, 3, 4])]);
        let right = make_table("r", vec![("id", vec![2, 4])]);
        let result = anti_join(&left, &right, &[JoinKey { left: 0, right: 0 }]).unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.columns[0], vec![1, 3]);
    }

    #[test]
    fn test_multi_key_join() {
        let left = make_table(
            "l",
            vec![("a", vec![1, 1, 2]), ("b", vec![1, 2, 1]), ("v", vec![10, 20, 30])],
        );
        let right =
            make_table("r", vec![("a", vec![1, 2]), ("b", vec![2, 1]), ("n", vec![100, 200])]);
        let keys = vec![JoinKey { left: 0, right: 0 }, JoinKey { left: 1, right: 1 }];
        let result = hash_join(&left, &right, &keys, JoinType::Inner).unwrap();
        assert_eq!(result.row_count, 2);
    }

    #[test]
    fn test_no_match() {
        let left = make_table("l", vec![("id", vec![1, 2])]);
        let right = make_table("r", vec![("id", vec![3, 4])]);
        let result =
            hash_join(&left, &right, &[JoinKey { left: 0, right: 0 }], JoinType::Inner).unwrap();
        assert_eq!(result.row_count, 0);
    }

    #[test]
    fn test_join_into_table() {
        let left = make_table("l", vec![("id", vec![1, 2])]);
        let right = make_table("r", vec![("id", vec![1, 2]), ("n", vec![100, 200])]);
        let result =
            hash_join(&left, &right, &[JoinKey { left: 0, right: 0 }], JoinType::Inner).unwrap();
        let table = result.into_table("joined");
        assert_eq!(table.name, "joined");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.column_names, vec!["id", "id", "n"]);
    }
}
