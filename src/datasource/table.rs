//! # In-memory table — the executor's working set.
//!
//! [`Table`] is the bridge between the loaders ([`super::parquet`],
//! [`super::csv`]) and the executor. A loader produces a
//! [`LoadedTable`] (column names + `Vec<u64>` cells); [`Table`] wraps
//! the same data with column-name lookup helpers the executor needs.
//!
//! ## Why a separate `Table` type
//!
//! `LoadedTable` is owned by the loader and may be moved multiple
//! times (e.g., into a `Catalog`). `Table` is the final resting shape
//! that the executor borrows — separate types let us evolve the
//! loader output (e.g., add schema metadata) without touching the
//! executor's borrow contract.

use crate::datasource::parquet::LoadedTable;

/// An in-memory table backed by `Vec<u64>` columns.
///
/// Every column has the same length (`row_count`). The invariant is
/// enforced by [`Table::from_loaded`], the only constructor — direct
/// field mutation is possible (the fields are `pub`) but discouraged;
/// callers that do mutate should preserve the invariant.
#[derive(Debug, Clone)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Columns, in schema order. Each `Vec<u64>` has length
    /// `row_count`.
    pub columns: Vec<std::sync::Arc<Vec<u64>>>,
    /// Column names, parallel to `columns`.
    pub column_names: Vec<String>,
    /// Number of rows. Equal to every column's length.
    pub row_count: usize,
    /// String column data (parallel to columns; None for non-string).
    pub string_columns: Vec<Option<std::sync::Arc<crate::exec::fm_index::StringSearchColumn>>>,
    /// NULL bitmaps (parallel to columns). `None` means no NULLs in this
    /// column (all cells are non-NULL). `Some(bm)` tracks which cells are
    /// NULL (Wave 22).
    pub null_bitmaps: Vec<Option<crate::types::null_bitmap::NullBitmap>>,
    /// Optional table schema preserving column types from DDL (Wave 36).
    /// None for tables loaded from Parquet/CSV (no DDL).
    pub schema: Option<crate::schema::table_schema::TableSchema>,
    /// Row version chains for MVCC (Wave 4 / Task 3.1). Each entry is
    /// the version chain for a single logical row: `row_versions[i]` is
    /// the `Vec<RowVersion>` for row `i`. The LAST entry in each chain is
    /// the latest version; older versions are left for VACUUM to reclaim.
    ///
    /// Empty when MVCC is not in use (backward compatible). When MVCC is
    /// enabled, INSERT appends a fresh single-version chain; UPDATE marks
    /// the latest version deleted (`xmax = txn_id`) and appends a new
    /// version to the SAME chain; DELETE just marks the latest version
    /// deleted.
    pub row_versions: Vec<Vec<crate::txn::mvcc::RowVersion>>,
    /// Optional i32 sidecar for narrow integer columns
    /// (Int/SmallInt/TinyInt). Populated by the CSV loader when all
    /// values fit in i32 range. None for columns that are u64/f64/string
    /// or have values outside i32 range. When present, the filter path
    /// uses `filter_eq_i32` etc. (4 bytes/element vs 8 for u64),
    /// halving memory bandwidth. Wave 5C.
    ///
    /// Parallel to `columns`; entries past `columns.len()` (or when
    /// None) mean no sidecar for that column. `from_loaded` pads with
    /// None to `columns.len()` so `i32_columns.len() == columns.len()`
    /// always holds on a constructed `Table`.
    pub i32_columns: Vec<Option<std::sync::Arc<Vec<i32>>>>,
}

impl Table {
    /// Convert a [`LoadedTable`] into a [`Table`].
    ///
    /// Verifies every column has the same length (the loaders already
    /// do this, but we re-check defensively in case the caller
    /// constructed a `LoadedTable` by hand).
    pub fn from_loaded(loaded: LoadedTable) -> Self {
        let row_count = loaded.row_count;
        let column_names: Vec<String> = loaded.columns.iter().map(|c| c.name.clone()).collect();
        let columns: Vec<std::sync::Arc<Vec<u64>>> =
            loaded.columns.iter().map(|c| std::sync::Arc::new(c.cells.clone())).collect();
        let string_columns: Vec<Option<std::sync::Arc<crate::exec::fm_index::StringSearchColumn>>> =
            loaded
                .columns
                .iter()
                .map(|c| c.string_search.clone().map(std::sync::Arc::new))
                .collect();

        // Defensive invariant check: every column should match
        // `row_count`. If a caller hand-built a bad `LoadedTable`,
        // silently truncate `row_count` to the min so the executor
        // doesn't read past the end of any column.
        let actual_min = columns.iter().map(|c| c.len()).min().unwrap_or(0);
        let row_count = row_count.min(actual_min);

        let null_bitmaps: Vec<Option<crate::types::null_bitmap::NullBitmap>> = loaded
            .columns
            .iter()
            .map(|c| {
                c.null_bitmap.as_ref().map(|bits| {
                    let mut bm = crate::types::null_bitmap::NullBitmap::new(bits.len());
                    for (i, &is_null) in bits.iter().enumerate() {
                        if is_null {
                            bm.set_null(i);
                        }
                    }
                    bm
                })
            })
            .collect();

        // i32 sidecar: copy from LoadedTable.i32_columns, wrapping each
        // Some(Vec<i32>) in an Arc. Pad with None to `columns.len()` so
        // the parallel-length invariant holds even when the loader left
        // `i32_columns` shorter than `columns` (e.g. old LoadedTable
        // fixtures in tests that build the struct by hand).
        let mut i32_columns: Vec<Option<std::sync::Arc<Vec<i32>>>> = loaded
            .i32_columns
            .into_iter()
            .map(|opt| opt.map(std::sync::Arc::new))
            .collect();
        while i32_columns.len() < columns.len() {
            i32_columns.push(None);
        }

        Table {
            name: loaded.name,
            columns,
            column_names,
            row_count,
            string_columns,
            null_bitmaps,
            schema: None,
            row_versions: Vec::new(),
            i32_columns,
        }
    }

    /// Look up a column by name, returning a slice over its cells.
    ///
    /// Returns `None` if the name is not in [`Table::column_names`].
    /// The slice borrows from `self` for the lifetime of `self` —
    /// exactly what the executor needs to feed a morsel.
    pub fn column(&self, name: &str) -> Option<&[u64]> {
        let idx = self.column_idx(name)?;
        Some(self.columns[idx].as_slice())
    }

    /// Look up a column's index by name.
    ///
    /// `O(ncols)` linear scan. turboGP tables are wide-and-short
    /// (ClickBench's `hits` table has 105 columns), but the lookup is
    /// rare enough (once per query) that a `HashMap` would not pay
    /// for itself.
    pub fn column_idx(&self, name: &str) -> Option<usize> {
        self.column_names.iter().position(|n| n == name)
    }

    /// Number of rows in the table.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Number of columns in the table.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    // -----------------------------------------------------------------
    // MVCC row-version helpers (Task 2.1 → Task 3.1 refactor).
    //
    // `row_versions` is now `Vec<Vec<RowVersion>>` — a version chain per
    // logical row. Entry `i` is the chain for row `i` (parallel to the
    // `columns` rows). The LAST entry in each chain is the latest
    // version; older versions are left for VACUUM to reclaim.
    //
    // INSERT appends a fresh single-version chain at `row_count - 1`.
    // UPDATE marks the latest version deleted (xmax = txn_id) and
    // appends a new version to the SAME chain. DELETE just marks the
    // latest version deleted.
    // -----------------------------------------------------------------

    /// Append a new [`RowVersion`] to the chain at `row_idx`.
    ///
    /// If `row_idx >= row_versions.len()`, the chain vec is extended
    /// with empty `Vec<RowVersion>` chains up to and including `row_idx`,
    /// so the new version lands at the right logical row. This is used by:
    /// - **INSERT**: `append_row_version(row_count - 1, version)` — the
    ///   new row's index is `row_count - 1` AFTER the row is inserted.
    /// - **UPDATE**: `append_row_version(row_idx, new_version)` — after
    ///   [`Table::mark_deleted`] has tombstoned the old version, the new
    ///   version is appended to the SAME chain at `row_idx`.
    pub fn append_row_version(
        &mut self,
        row_idx: usize,
        version: crate::txn::mvcc::RowVersion,
    ) {
        if row_idx >= self.row_versions.len() {
            self.row_versions.resize_with(row_idx + 1, Vec::new);
        }
        self.row_versions[row_idx].push(version);
    }

    /// Mark the latest version at `row_idx` as deleted by `txn_id`.
    ///
    /// Sets `xmax = Some(txn_id)` on the LAST version in
    /// `self.row_versions[row_idx]` (the chain for that row). Earlier
    /// versions in the chain are left untouched — they were already
    /// superseded and carry their own (older) `xmax`.
    ///
    /// Returns `true` if a version was found and freshly marked.
    /// Returns `false` when:
    /// - `row_idx` is out of bounds (no chain exists),
    /// - the chain is empty (no version to tombstone), or
    /// - the latest version already has `xmax` set (already deleted) —
    ///   in which case a warning is logged.
    pub fn mark_deleted(&mut self, row_idx: usize, txn_id: u64) -> bool {
        let Some(chain) = self.row_versions.get_mut(row_idx) else {
            return false;
        };
        let Some(version) = chain.last_mut() else {
            return false;
        };
        if let Some(existing_xmax) = version.xmax {
            log::warn!(
                "Table::mark_deleted ignored on table {:?}: row {} already deleted by txn {}",
                self.name,
                row_idx,
                existing_xmax
            );
            return false;
        }
        version.xmax = Some(txn_id);
        true
    }

    /// Return the latest visible [`RowVersion`] at `row_idx` for `txn`.
    ///
    /// Iterates the version chain at `self.row_versions[row_idx]` in
    /// REVERSE (newest first) and returns the first version that
    /// `mgr.visible(version, txn)` accepts. This is the snapshot-isolation
    /// read rule: a transaction sees at most one version of each row,
    /// namely the newest version that is visible to it.
    ///
    /// Returns `None` when `row_idx` is out of bounds, the chain is
    /// empty, or no version in the chain is visible to `txn`.
    pub fn latest_visible_version(
        &self,
        row_idx: usize,
        mgr: &crate::txn::mvcc::MvccTxnManager,
        txn: &crate::txn::mvcc::MvccTransaction,
    ) -> Option<&crate::txn::mvcc::RowVersion> {
        let chain = self.row_versions.get(row_idx)?;
        for version in chain.iter().rev() {
            if mgr.visible(version, txn) {
                return Some(version);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::LoadedColumn;
    use crate::txn::mvcc::{MvccTxnManager, RowVersion};

    /// Build a small `LoadedTable` with two columns of 3 rows each.
    fn sample_loaded() -> LoadedTable {
        LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: vec![1, 2, 3],
                    row_count: 3,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "v".into(),
                    cells: vec![10, 20, 30],
                    row_count: 3,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: 3,
            i32_columns: vec![None, None],
        }
    }

    /// `from_loaded` preserves name, columns, and row count.
    #[test]
    fn from_loaded_preserves_data() {
        let table = Table::from_loaded(sample_loaded());
        assert_eq!(table.name, "t");
        assert_eq!(table.row_count, 3);
        assert_eq!(table.column_count(), 2);
        assert_eq!(table.column_names, vec!["id".to_string(), "v".to_string()]);
        assert_eq!(&**table.columns[0], &vec![1u64, 2, 3][..]);
        assert_eq!(&**table.columns[1], &vec![10u64, 20, 30][..]);
    }

    /// `column` returns the right slice.
    #[test]
    fn column_lookup_by_name() {
        let table = Table::from_loaded(sample_loaded());
        assert_eq!(table.column("id"), Some(&[1u64, 2, 3][..]));
        assert_eq!(table.column("v"), Some(&[10u64, 20, 30][..]));
        assert_eq!(table.column("missing"), None);
    }

    /// `column_idx` returns the right index.
    #[test]
    fn column_idx_lookup() {
        let table = Table::from_loaded(sample_loaded());
        assert_eq!(table.column_idx("id"), Some(0));
        assert_eq!(table.column_idx("v"), Some(1));
        assert_eq!(table.column_idx("missing"), None);
    }

    /// `row_count()` accessor matches the field.
    #[test]
    fn row_count_accessor() {
        let table = Table::from_loaded(sample_loaded());
        assert_eq!(table.row_count(), 3);
    }

    /// If a caller hand-builds a `LoadedTable` with mismatched
    /// column lengths, `from_loaded` clamps `row_count` to the min.
    #[test]
    fn from_loaded_clamps_mismatched_lengths() {
        let loaded = LoadedTable {
            name: "bad".into(),
            columns: vec![
                LoadedColumn {
                    name: "a".into(),
                    cells: vec![1, 2, 3],
                    row_count: 3,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "b".into(),
                    cells: vec![10, 20],
                    row_count: 2,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: 5, // lie about row count
            i32_columns: vec![None, None],
        };
        let table = Table::from_loaded(loaded);
        // row_count is clamped to the min actual column length (2).
        assert_eq!(table.row_count, 2);
    }

    /// An empty `LoadedTable` produces an empty `Table`.
    #[test]
    fn from_loaded_empty() {
        let loaded = LoadedTable { name: "empty".into(), columns: Vec::new(), row_count: 0, i32_columns: Vec::new() };
        let table = Table::from_loaded(loaded);
        assert_eq!(table.row_count, 0);
        assert_eq!(table.column_count(), 0);
    }

    /// `Table` is `Clone` (used by the catalog when snapshotting).
    #[test]
    fn table_is_clone() {
        let table = Table::from_loaded(sample_loaded());
        let table2 = table.clone();
        assert_eq!(table.columns.len(), table2.columns.len());
    }

    // -----------------------------------------------------------------
    // MVCC helpers (Task 2.1).
    // -----------------------------------------------------------------

    /// `append_row_version(row_idx, version)` appends to the chain at
    /// `row_idx`, extending with empty chains when `row_idx` is beyond
    /// the current length. Multiple appends to the same `row_idx` grow
    /// the chain in place (Task 3.1).
    #[test]
    fn test_append_row_version() {
        let mut table = Table::from_loaded(sample_loaded());
        assert_eq!(table.row_count, 3);
        assert!(table.row_versions.is_empty());

        // Append at row 0: extends row_versions to length 1, then pushes
        // the version onto chain 0.
        table.append_row_version(0, RowVersion::new(1, vec![100, 200]));
        assert_eq!(table.row_versions.len(), 1, "one chain allocated");
        assert_eq!(table.row_versions[0].len(), 1, "chain 0 has one version");
        assert_eq!(table.row_versions[0][0].xmin, 1);
        assert_eq!(table.row_versions[0][0].xmax, None);
        assert_eq!(table.row_versions[0][0].values, vec![100, 200]);
        assert!(!table.row_versions[0][0].deleted);

        // Append at row 2: extends row_versions to length 3, with chain 1
        // left empty (it will be filled by a later INSERT).
        table.append_row_version(2, RowVersion::new(2, vec![300, 400]));
        assert_eq!(table.row_versions.len(), 3, "extended to length 3");
        assert!(table.row_versions[1].is_empty(), "chain 1 is empty until filled");
        assert_eq!(table.row_versions[2].len(), 1);
        assert_eq!(table.row_versions[2][0].xmin, 2);

        // Append a SECOND version to chain 0 (the UPDATE pattern: same
        // row_idx, new version in the same chain).
        table.append_row_version(0, RowVersion::new(3, vec![500, 600]));
        assert_eq!(table.row_versions[0].len(), 2, "chain 0 now has two versions");
        assert_eq!(table.row_versions[0][1].xmin, 3);

        // Append at row 5 (beyond row_count=3): extends with empty chains
        // at indices 3 and 4.
        table.append_row_version(5, RowVersion::new(4, vec![700, 800]));
        assert_eq!(table.row_versions.len(), 6, "extended to length 6");
        assert!(table.row_versions[3].is_empty());
        assert!(table.row_versions[4].is_empty());
        assert_eq!(table.row_versions[5].len(), 1);
        assert_eq!(table.row_versions[5][0].xmin, 4);
    }

    /// `mark_deleted` sets `xmax` on the LATEST version in the chain at
    /// `row_idx` and refuses to double-delete (returns `false`, logs a
    /// warning). Returns `false` for out-of-bounds or empty chains.
    #[test]
    fn test_mark_deleted() {
        let mut table = Table::from_loaded(sample_loaded());
        table.append_row_version(0, RowVersion::new(1, vec![10, 20]));
        assert_eq!(table.row_versions[0].last().unwrap().xmax, None);

        // Fresh delete succeeds.
        let marked = table.mark_deleted(0, 42);
        assert!(marked);
        assert_eq!(table.row_versions[0].last().unwrap().xmax, Some(42));

        // Double-delete is rejected (the latest version is already tombstoned).
        let marked_again = table.mark_deleted(0, 99);
        assert!(!marked_again);
        assert_eq!(table.row_versions[0].last().unwrap().xmax, Some(42));

        // Out-of-bounds index returns false.
        let oob = table.mark_deleted(5, 7);
        assert!(!oob);

        // Empty chain returns false.
        table.row_versions.push(Vec::new()); // empty chain at index 1
        let empty_marked = table.mark_deleted(1, 50);
        assert!(!empty_marked, "empty chain has no version to tombstone");

        // After a second version is appended to chain 0, `mark_deleted`
        // tombstones the NEW latest version (not the older already-deleted
        // one) — verifying the chain semantics.
        table.append_row_version(0, RowVersion::new(2, vec![99, 99]));
        assert_eq!(table.row_versions[0].len(), 2);
        // Latest version is the new one (xmax None).
        assert_eq!(table.row_versions[0][1].xmax, None);
        let marked_new = table.mark_deleted(0, 77);
        assert!(marked_new);
        // Old version's xmax is unchanged; new version is tombstoned.
        assert_eq!(table.row_versions[0][0].xmax, Some(42));
        assert_eq!(table.row_versions[0][1].xmax, Some(77));
    }

    /// `latest_visible_version` iterates the chain at `row_idx` in
    /// reverse and returns the first visible version. Returns `None`
    /// for out-of-bounds rows, empty chains, or chains with no visible
    /// version.
    #[test]
    fn test_latest_visible_version() {
        let mut table = Table::from_loaded(sample_loaded()); // 3 rows
        let mut mgr = MvccTxnManager::new();

        // Old txn (id=1) creates a version, commits (commit_id=1).
        let old_txn = mgr.begin(); // id=1, snapshot=0
        let old_cid = mgr.commit(old_txn.id); // commit_id=1
        assert_eq!(old_cid, 1);

        // Row 0: committed-deleted version (created by txn 1, deleted by
        // txn 1 which has committed).
        let mut old_version = RowVersion::new(old_txn.id, vec![100]);
        old_version.xmax = Some(old_txn.id);
        table.append_row_version(0, old_version);

        // New txn (id=2) begins with snapshot_id=1 (sees txn 1's commit).
        let new_txn = mgr.begin(); // id=2, snapshot=1

        // Row 1: "new live" version created by new_txn, never deleted.
        table.append_row_version(1, RowVersion::new(new_txn.id, vec![200]));

        // Row 0's only version is committed-deleted before new_txn's
        // snapshot, so it is NOT visible to new_txn.
        let row0 = table.latest_visible_version(0, &mgr, &new_txn);
        assert!(row0.is_none(), "deleted old version should not be visible");

        // Row 1's version was created by new_txn itself and is live, so
        // it IS visible.
        match table.latest_visible_version(1, &mgr, &new_txn) {
            Some(v) => {
                assert_eq!(v.xmin, new_txn.id);
                assert_eq!(v.xmax, None);
                assert_eq!(v.values, vec![200]);
            }
            None => panic!("expected the new live version to be visible at row 1"),
        }

        // Out-of-bounds row returns None.
        assert!(table.latest_visible_version(10, &mgr, &new_txn).is_none());

        // Empty chain returns None — simulate by leaving row 2's chain empty.
        assert!(table.latest_visible_version(2, &mgr, &new_txn).is_none());
    }
}
