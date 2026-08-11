//! Executor tests — Task 5.2 + 5.3.
//!
//! Extracted from `src/engine/executor.rs` in Task 8.2-fix to satisfy
//! the 2000-LOC file-size limit. These are unit tests for the
//! parallel_scan integration in `filter_indices`. They exercise the
//! parallel path directly (not via `execute()`), so they don't need a
//! full `QueryEngine` setup. They build a `Table` by hand, populate
//! `row_versions` to model MVCC state, and call `filter_indices` with
//! `Some(&mgr)`.

#![cfg(test)]

use super::*;
use crate::datasource::parquet::{LoadedColumn, LoadedTable};
use crate::datasource::Table as DataSourceTable;
use crate::txn::mvcc::{MvccTxnManager, RowVersion};

/// Build a `Table` with a single integer column `id` = 0..n.
/// `row_versions` is left empty — the test sets it explicitly.
fn make_int_table(n: usize) -> DataSourceTable {
    let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
    let mut t = DataSourceTable::from_loaded(LoadedTable {
        name: "t".into(),
        columns: vec![LoadedColumn {
            name: "id".into(),
            cells: ids,
            row_count: n,
            string_search: None,
            null_bitmap: None,
        }],
        row_count: n,
    });
    // Pre-size row_versions so tests can fill it.
    t.row_versions = Vec::new();
    t
}

/// Build a `Table` with two columns: `id` (0..n) and `x` (i%7).
fn make_two_col_table(n: usize) -> DataSourceTable {
    let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
    let xs: Vec<u64> = (0..n).map(|i| (i % 7) as u64).collect();
    DataSourceTable::from_loaded(LoadedTable {
        name: "t".into(),
        columns: vec![
            LoadedColumn {
                name: "id".into(),
                cells: ids,
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
            LoadedColumn {
                name: "x".into(),
                cells: xs,
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
        ],
        row_count: n,
    })
}

/// Task 5.3 DoD: scan a 5,000-row table under MVCC. All rows are
/// committed-live (xmin=1 committed, xmax=None). The parallel path
/// should return all 5,000 indices, matching the serial path's
/// output exactly.
#[test]
fn test_execute_select_parallel_large_table() {
    let n = 5_000;
    let mut table = make_int_table(n);
    // All rows committed-live: xmin=1 (committed), xmax=None. Each row
    // gets a single-element chain (Task 3.1 layout: Vec<Vec<RowVersion>>).
    table.row_versions = (0..n)
        .map(|i| vec![RowVersion {
            xmin: 1,
            xmax: None,
            values: vec![i as u64],
            deleted: false,
        }])
        .collect();

    // Manager: txn 1 committed (so xmin=1 is visible), txn 2 active reader.
    let mut mgr = MvccTxnManager::new();
    let _t1 = mgr.begin(); // txn 1
    mgr.commit(1); // txn 1 committed
    let _t2 = mgr.begin(); // txn 2 = active reader

    let wc = WhereClause::None;
    let parallel_indices = filter_indices(&wc, &table, Some(&mgr));

    // All 5,000 rows should be visible.
    assert_eq!(
        parallel_indices.len(),
        n,
        "expected {} visible rows, got {}",
        n,
        parallel_indices.len()
    );

    // The result should be 0..n in order (morsels are contiguous chunks
    // processed in input order, so the concatenation is the input order).
    for (i, &idx) in parallel_indices.iter().enumerate() {
        assert_eq!(idx, i, "result index {} = {} (expected {})", i, idx, i);
    }
}

/// Task 5.3: parallel scan correctly excludes invisible rows.
/// Every 10th row is marked as deleted by a committed txn (xmax
/// committed → invisible to the active reader). The other 90% are
/// committed-live. Verifies the visibility filter is applied per
/// morsel across all worker threads.
#[test]
fn test_filter_indices_parallel_excludes_invisible() {
    let n = 5_000;
    let mut table = make_int_table(n);
    // Mark every 10th row as deleted by txn 2 (committed). Each row
    // gets a single-element chain (Task 3.1 layout).
    table.row_versions = (0..n)
        .map(|i| {
            if i % 10 == 0 {
                vec![RowVersion {
                    xmin: 1,           // committed by txn 1
                    xmax: Some(2),     // deleted by txn 2
                    values: vec![i as u64],
                    deleted: false,
                }]
            } else {
                vec![RowVersion {
                    xmin: 1,
                    xmax: None,
                    values: vec![i as u64],
                    deleted: false,
                }]
            }
        })
        .collect();

    let mut mgr = MvccTxnManager::new();
    let _t1 = mgr.begin();
    mgr.commit(1); // txn 1 committed
    let _t2 = mgr.begin();
    mgr.commit(2); // txn 2 committed (xmax=2 is committed → row invisible)
    let _t3 = mgr.begin(); // txn 3 = active reader

    let wc = WhereClause::None;
    let indices = filter_indices(&wc, &table, Some(&mgr));

    // 500 rows (every 10th) should be filtered out.
    let expected_visible = n - 500;
    assert_eq!(
        indices.len(),
        expected_visible,
        "expected {} visible rows, got {}",
        expected_visible,
        indices.len()
    );

    // Verify no deleted row (i % 10 == 0) appears in the result.
    for &i in &indices {
        assert!(i % 10 != 0, "deleted row {} appeared in result", i);
    }
}

/// Task 5.3: parallel scan respects the WHERE clause. Combines a
/// WHERE filter (x = 0) with MVCC visibility (all rows visible).
/// Verifies the worker applies BOTH predicates correctly across
/// morsels.
#[test]
fn test_filter_indices_parallel_where_and_mvcc() {
    let n = 5_000;
    let mut table = make_two_col_table(n);
    // All rows committed-live. Each row gets a single-element chain
    // (Task 3.1 layout).
    table.row_versions = (0..n)
        .map(|i| vec![RowVersion {
            xmin: 1,
            xmax: None,
            values: vec![i as u64, (i % 7) as u64],
            deleted: false,
        }])
        .collect();

    let mut mgr = MvccTxnManager::new();
    let _t1 = mgr.begin();
    mgr.commit(1);
    let _t2 = mgr.begin();

    // WHERE x = 0. The table is i%7, so x=0 for i in {0, 7, 14, ...}.
    // For n=5000, that's ceil(5000/7) = 715 rows.
    let filter = Filter { col_idx: 1, op: "=".into(), value: 0 };
    let wc = WhereClause::Single(filter);
    let indices = filter_indices(&wc, &table, Some(&mgr));

    // Count expected matches serially for comparison.
    let expected: Vec<usize> = (0..n).filter(|&i| (i % 7) == 0).collect();
    assert_eq!(indices.len(), expected.len(),
        "expected {} rows matching x=0, got {}", expected.len(), indices.len());
    assert_eq!(indices, expected, "parallel result differs from serial expected");

    // Verify every returned row actually has x=0.
    for &i in &indices {
        assert_eq!(table.columns[1][i], 0, "row {} has x={} (expected 0)", i, table.columns[1][i]);
    }
}

/// Task 5.3: parallel path returns the same result as the serial path
/// for a non-trivial mix of WHERE + visibility. Builds a 5,000-row
/// table where half the rows are invisible (deleted by committed txn)
/// and the WHERE clause selects a subset of the visible rows.
#[test]
fn test_filter_indices_parallel_matches_serial() {
    let n = 5_000;
    let mut table = make_two_col_table(n);
    // Half the rows deleted by committed txn 2. Each row gets a
    // single-element chain (Task 3.1 layout).
    table.row_versions = (0..n)
        .map(|i| vec![RowVersion {
            xmin: 1,
            xmax: if i % 2 == 0 { Some(2) } else { None },
            values: vec![i as u64, (i % 7) as u64],
            deleted: false,
        }])
        .collect();

    let mut mgr = MvccTxnManager::new();
    let _t1 = mgr.begin();
    mgr.commit(1);
    let _t2 = mgr.begin();
    mgr.commit(2); // txn 2 committed → even-indexed rows invisible
    let _t3 = mgr.begin(); // active reader

    // WHERE x = 3.
    let filter = Filter { col_idx: 1, op: "=".into(), value: 3 };
    let wc = WhereClause::Single(filter);

    // Run the parallel path (n > 1000 + MVCC active).
    let parallel_result = filter_indices(&wc, &table, Some(&mgr));

    // Compute the serial expected result by hand:
    // visible rows are odd indices (i % 2 == 1); WHERE x = (i%7) == 3.
    let expected: Vec<usize> = (0..n)
        .filter(|&i| i % 2 == 1 && (i % 7) as u64 == 3)
        .collect();

    assert_eq!(parallel_result.len(), expected.len(),
        "expected {} rows, got {}", expected.len(), parallel_result.len());
    assert_eq!(parallel_result, expected,
        "parallel result does not match serial expected");
}

/// Task 5.3: tables with row_count <= 1000 use the serial path
/// (MVCC still active). Verifies the threshold doesn't break
/// small-table correctness.
#[test]
fn test_filter_indices_small_table_uses_serial_path() {
    let n = 1_000; // exactly the threshold (NOT > 1000).
    let mut table = make_int_table(n);
    table.row_versions = (0..n)
        .map(|i| vec![RowVersion {
            xmin: 1,
            xmax: None,
            values: vec![i as u64],
            deleted: false,
        }])
        .collect();

    let mut mgr = MvccTxnManager::new();
    let _t1 = mgr.begin();
    mgr.commit(1);
    let _t2 = mgr.begin();

    let wc = WhereClause::None;
    let indices = filter_indices(&wc, &table, Some(&mgr));
    assert_eq!(indices.len(), n, "expected {} rows, got {}", n, indices.len());
}

/// Task 5.3: tables with row_count > 1000 but MVCC off use the
/// serial path. Verifies the parallel path is gated on BOTH
/// conditions.
#[test]
fn test_filter_indices_large_table_no_mvcc_uses_serial() {
    let n = 5_000;
    let table = make_int_table(n);
    let wc = WhereClause::None;
    // mvcc = None → serial path even for large tables.
    let indices = filter_indices(&wc, &table, None);
    assert_eq!(indices.len(), n, "expected {} rows, got {}", n, indices.len());
}
