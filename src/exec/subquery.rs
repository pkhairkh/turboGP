//! Subqueries + set operations.
//!
//! Research: Subquery decorrelation (Neumann-Kemper 2018 UNNEST).

use crate::datasource::table::Table;
use crate::exec::join::{anti_join, semi_join, JoinKey};
use crate::Error;
use std::collections::HashSet;

/// EXISTS: left rows that have a match in right on keys.
pub fn exists(left: &Table, right: &Table, keys: &[JoinKey]) -> Vec<bool> {
    let result = semi_join(left, right, keys).unwrap_or_else(|_| crate::exec::join::JoinResult {
        columns: vec![],
        column_names: vec![],
        row_count: 0,
    });
    let matched: HashSet<u64> =
        result.columns.first().map(|c| c.iter().copied().collect()).unwrap_or_default();
    (0..left.row_count).map(|i| matched.contains(&left.columns[0][i])).collect()
}

/// NOT EXISTS: left rows with NO match.
pub fn not_exists(left: &Table, right: &Table, keys: &[JoinKey]) -> Vec<bool> {
    exists(left, right, keys).iter().map(|&b| !b).collect()
}

/// UNION ALL: concatenate tables.
pub fn union_all(left: &Table, right: &Table) -> Result<Table, Error> {
    if left.column_names != right.column_names {
        return Err(Error::InvalidArg("UNION ALL: column mismatch".into()));
    }
    let mut columns = left.columns.clone();
    for (i, col) in right.columns.iter().enumerate() {
        if i < columns.len() {
            std::sync::Arc::make_mut(&mut columns[i]).extend(col.iter().copied());
        }
    }
    Ok(Table {
        name: format!("{}_union_{}", left.name, right.name),
        columns,
        column_names: left.column_names.clone(),
        row_count: left.row_count + right.row_count,
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    })
}

/// UNION (distinct): concatenate + deduplicate.
pub fn union_distinct(left: &Table, right: &Table) -> Result<Table, Error> {
    let combined = union_all(left, right)?;
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    let mut out_cols: Vec<Vec<u64>> = vec![Vec::new(); combined.columns.len()];
    for i in 0..combined.row_count {
        let key: Vec<u64> = combined.columns.iter().map(|c| c[i]).collect();
        if seen.insert(key.clone()) {
            for (c, col) in combined.columns.iter().enumerate() {
                out_cols[c].push(col[i]);
            }
        }
    }
    Ok(Table {
        name: combined.name,
        columns: out_cols.iter().map(|c| std::sync::Arc::new(c.clone())).collect(),
        column_names: combined.column_names.clone(),
        row_count: out_cols[0].len(),
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    })
}

/// INTERSECT: rows in both.
pub fn intersect(left: &Table, right: &Table) -> Result<Table, Error> {
    let right_keys: HashSet<Vec<u64>> =
        (0..right.row_count).map(|i| right.columns.iter().map(|c| c[i]).collect()).collect();
    let mut out_cols: Vec<Vec<u64>> = vec![Vec::new(); left.columns.len()];
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    for i in 0..left.row_count {
        let key: Vec<u64> = left.columns.iter().map(|c| c[i]).collect();
        if right_keys.contains(&key) && seen.insert(key.clone()) {
            for (c, col) in left.columns.iter().enumerate() {
                out_cols[c].push(col[i]);
            }
        }
    }
    Ok(Table {
        name: format!("{}_intersect", left.name),
        columns: out_cols.iter().map(|c| std::sync::Arc::new(c.clone())).collect(),
        column_names: left.column_names.clone(),
        row_count: out_cols[0].len(),
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    })
}

/// EXCEPT: rows in left not in right.
pub fn except(left: &Table, right: &Table) -> Result<Table, Error> {
    let right_keys: HashSet<Vec<u64>> =
        (0..right.row_count).map(|i| right.columns.iter().map(|c| c[i]).collect()).collect();
    let mut out_cols: Vec<Vec<u64>> = vec![Vec::new(); left.columns.len()];
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    for i in 0..left.row_count {
        let key: Vec<u64> = left.columns.iter().map(|c| c[i]).collect();
        if !right_keys.contains(&key) && seen.insert(key.clone()) {
            for (c, col) in left.columns.iter().enumerate() {
                out_cols[c].push(col[i]);
            }
        }
    }
    Ok(Table {
        name: format!("{}_except", left.name),
        columns: out_cols.iter().map(|c| std::sync::Arc::new(c.clone())).collect(),
        column_names: left.column_names.clone(),
        row_count: out_cols[0].len(),
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    })
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
    fn test_union_all() {
        let t1 = make_table("t1", vec![("id", vec![1, 2, 3])]);
        let t2 = make_table("t2", vec![("id", vec![4, 5])]);
        let result = union_all(&t1, &t2).unwrap();
        assert_eq!(result.row_count, 5);
    }

    #[test]
    fn test_union_distinct() {
        let t1 = make_table("t1", vec![("id", vec![1, 2, 2])]);
        let t2 = make_table("t2", vec![("id", vec![2, 3])]);
        let result = union_distinct(&t1, &t2).unwrap();
        assert_eq!(result.row_count, 3);
    }

    #[test]
    fn test_intersect() {
        let t1 = make_table("t1", vec![("id", vec![1, 2, 3, 4])]);
        let t2 = make_table("t2", vec![("id", vec![2, 4, 6])]);
        let result = intersect(&t1, &t2).unwrap();
        assert_eq!(result.row_count, 2);
    }

    #[test]
    fn test_except() {
        let t1 = make_table("t1", vec![("id", vec![1, 2, 3, 4, 5])]);
        let t2 = make_table("t2", vec![("id", vec![2, 4])]);
        let result = except(&t1, &t2).unwrap();
        assert_eq!(result.row_count, 3);
    }
}
