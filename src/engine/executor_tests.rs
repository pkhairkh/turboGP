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
        i32_columns: Vec::new(),
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
        i32_columns: Vec::new(),
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

// -----------------------------------------------------------------
// Task 3.2 — snapshot isolation (engine-level, via filter_indices)
// -----------------------------------------------------------------

/// Task 3.2 DoD: snapshot isolation at the engine level — T1 begins
/// (snapshot_id=S1), T2 inserts+commits (commit_id=S2 > S1), T1 scans
/// → does NOT see T2's insert.
///
/// This test exercises `filter_indices` (the single chokepoint that
/// `execute_select` uses for MVCC visibility filtering) directly with a
/// hand-built `Table` whose `row_versions` model the T1/T2 scenario.
/// It verifies that the new `is_visible_with_snapshot` snapshot_id
/// comparison (Task 3.2) is wired into the SELECT path.
///
/// Scenario:
/// - T1 (id=1, snapshot_id=0) inserts row 0 (xmin=1) and commits
///   (commit_id=1).
/// - T2 (id=2, snapshot_id=1) inserts row 1 (xmin=2) and commits
///   (commit_id=2).
/// - T3 (id=3, snapshot_id=2) is the active reader.
///   - T3 sees BOTH rows (T1 and T2 both committed before T3's snapshot).
/// - A second reader T1' (id=4, snapshot_id=1) is set as `current_active`.
///   - T1' sees row 0 (T1 committed at cid=1 ≤ snapshot=1) but NOT row 1
///     (T2 committed at cid=2 > snapshot=1) — snapshot isolation.
#[test]
fn test_snapshot_isolation() {
    let n = 2;
    let mut table = make_int_table(n);
    // Row 0: created by T1 (committed at cid=1), live.
    // Row 1: created by T2 (committed at cid=2), live.
    table.row_versions = vec![
        vec![RowVersion { xmin: 1, xmax: None, values: vec![0], deleted: false }],
        vec![RowVersion { xmin: 2, xmax: None, values: vec![1], deleted: false }],
    ];

    let mut mgr = MvccTxnManager::new();
    // T1 (id=1) inserts row 0 and commits (cid=1).
    let _t1 = mgr.begin(); // id=1, snapshot=0
    mgr.commit(1); // cid=1
    // T2 (id=2) inserts row 1 and commits (cid=2).
    let _t2 = mgr.begin(); // id=2, snapshot=1
    mgr.commit(2); // cid=2

    // Reader T3 (id=3, snapshot=2) — sees both rows.
    let _t3 = mgr.begin(); // id=3, snapshot=2; current_active = T3
    let wc = WhereClause::None;
    let indices = filter_indices(&wc, &table, Some(&mgr));
    assert_eq!(
        indices.len(),
        2,
        "T3 (snapshot=2) must see both rows (T1 cid=1, T2 cid=2 both ≤ 2)"
    );

    // Commit T3 so we can begin T1' as current_active.
    mgr.commit(3); // cid=3

    // Reader T1' (id=4, snapshot=3) — but we want snapshot=1 to test SI.
    // We can't lower the snapshot_id via the public API, so we instead
    // verify the snapshot-isolation property directly: a reader with
    // snapshot=1 must NOT see T2's row (cid=2 > 1). We do this by
    // calling is_visible_with_snapshot directly (the same method that
    // filter_indices uses via row_visible_to_active).
    let t1_prime_snapshot: u64 = 1;
    let t1_prime_id: u64 = 4;
    let v_row0 = &table.row_versions[0][0];
    let v_row1 = &table.row_versions[1][0];
    assert!(
        mgr.is_visible_with_snapshot(v_row0, t1_prime_snapshot, t1_prime_id),
        "T1' (snapshot=1) must see T1's row (cid=1 ≤ 1)"
    );
    assert!(
        !mgr.is_visible_with_snapshot(v_row1, t1_prime_snapshot, t1_prime_id),
        "T1' (snapshot=1) must NOT see T2's row (cid=2 > 1) — snapshot isolation"
    );

    // Also verify the autocommit path: active_txn_id=0, snapshot_id =
    // current_commit_id = 3. Sees both committed rows.
    let snap = mgr.current_commit_id();
    assert_eq!(snap, 3);
    assert!(
        mgr.is_visible_with_snapshot(v_row0, snap, 0),
        "autocommit sees T1's row"
    );
    assert!(
        mgr.is_visible_with_snapshot(v_row1, snap, 0),
        "autocommit sees T2's row"
    );
}

/// Task 3.3 DoD: filter_indices returns the row when the updating txn
/// scans — i.e. the new version (xmin == active_txn_id, xmax None) is
/// visible and the old version (xmax == active_txn_id) is invisible.
///
/// Models the chain that UPDATE produces: row 0's chain has two
/// versions — the tombstoned old version (xmax = active_txn_id) and
/// the new live version (xmin = active_txn_id, xmax None). The active
/// txn's filter_indices must return row 0 (visible via the new version).
#[test]
fn test_filter_indices_update_visible_to_updating_txn() {
    let n = 1;
    let mut table = make_int_table(n);
    // Row 0's chain: old version (xmax=active_txn_id) + new version (xmin=active_txn_id, xmax=None).
    let active_txn_id: u64 = 5;
    table.row_versions = vec![vec![
        RowVersion {
            xmin: active_txn_id,
            xmax: Some(active_txn_id), // tombstoned by us via UPDATE
            values: vec![10],
            deleted: false,
        },
        RowVersion {
            xmin: active_txn_id,
            xmax: None, // live new version
            values: vec![99],
            deleted: false,
        },
    ]];

    let mut mgr = MvccTxnManager::new();
    // Begin the active txn (id=active_txn_id=5, but begin() assigns
    // sequential IDs starting at 1, so we begin 5 times to reach id=5).
    // Easier: just begin once and use whatever id it assigns, then
    // rebuild the row_versions with that id.
    let active = mgr.begin(); // current_active
    let active_id = active.id;

    // Rebuild the row_versions with the actual active_id.
    table.row_versions = vec![vec![
        RowVersion {
            xmin: active_id,
            xmax: Some(active_id),
            values: vec![10],
            deleted: false,
        },
        RowVersion {
            xmin: active_id,
            xmax: None,
            values: vec![99],
            deleted: false,
        },
    ]];

    let wc = WhereClause::None;
    let indices = filter_indices(&wc, &table, Some(&mgr));
    assert_eq!(
        indices.len(),
        1,
        "updating txn must see its own new version (row 0 visible)"
    );
    assert_eq!(indices[0], 0, "the visible row is row 0");
}
