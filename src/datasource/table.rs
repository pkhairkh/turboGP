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
    /// Row version metadata for MVCC (Wave 4). Each entry is parallel to
    /// a row in `columns`. `xmin` = creating txn, `xmax` = deleting txn (0 = live).
    /// Empty when MVCC is not in use (backward compatible).
    pub row_versions: Vec<crate::txn::mvcc::RowVersion>,
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

        Table {
            name: loaded.name,
            columns,
            column_names,
            row_count,
            string_columns,
            null_bitmaps,
            schema: None,
            row_versions: Vec::new(),
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
    // MVCC row-version helpers (Task 2.1).
    //
    // `row_versions` is a flat `Vec<RowVersion>` parallel to the table's
    // rows: entry `i` is the latest (and currently only) version for
    // logical row `i`. These helpers preserve that invariant while
    // giving the executor a small surface to grow versions, tombstone
    // rows, and read the latest visible version under MVCC.
    // -----------------------------------------------------------------

    /// Append a new [`RowVersion`] to the table's MVCC version chain.
    ///
    /// The version is appended to `self.row_versions`. The table
    /// invariant requires `row_versions.len() <= row_count` (one entry
    /// per logical row), so if that bound has already been reached this
    /// call is a no-op and a warning is logged.
    ///
    /// Typical use: a loader or INSERT path calls this once per newly
    /// created row, filling the next slot.
    pub fn append_row_version(&mut self, version: crate::txn::mvcc::RowVersion) {
        if self.row_versions.len() >= self.row_count {
            log::warn!(
                "Table::append_row_version ignored on table {:?}: row_versions.len()={} >= row_count={}",
                self.name,
                self.row_versions.len(),
                self.row_count
            );
            return;
        }
        self.row_versions.push(version);
    }

    /// Mark the latest version at `row_idx` as deleted by `txn_id`.
    ///
    /// Sets `xmax = Some(txn_id)` on the version stored at
    /// `self.row_versions[row_idx]` (the latest — and currently only —
    /// version in the chain for that row).
    ///
    /// Returns `true` if a version was found and freshly marked.
    /// Returns `false` when:
    /// - `row_idx` is out of bounds (no version exists), or
    /// - the latest version already has `xmax` set (already deleted) —
    ///   in which case a warning is logged.
    pub fn mark_deleted(&mut self, row_idx: usize, txn_id: u64) -> bool {
        let Some(version) = self.row_versions.get_mut(row_idx) else {
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
    /// Looks up the version stored at `self.row_versions[row_idx]` and
    /// asks `mgr.visible(version, txn)` whether it is visible to the
    /// given transaction. Iterating "the chain in reverse" reduces to a
    /// single visibility check here because the flat `row_versions`
    /// field holds one version per logical row; the iterator shape is
    /// preserved so the method signature stays compatible with a
    /// future multi-version-per-row layout.
    ///
    /// Returns `None` when `row_idx` is out of bounds or the version is
    /// not visible to `txn`.
    pub fn latest_visible_version(
        &self,
        row_idx: usize,
        mgr: &crate::txn::mvcc::MvccTxnManager,
        txn: &crate::txn::mvcc::MvccTransaction,
    ) -> Option<&crate::txn::mvcc::RowVersion> {
        let version = self.row_versions.get(row_idx)?;
        if mgr.visible(version, txn) {
            Some(version)
        } else {
            None
        }
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
        };
        let table = Table::from_loaded(loaded);
        // row_count is clamped to the min actual column length (2).
        assert_eq!(table.row_count, 2);
    }

    /// An empty `LoadedTable` produces an empty `Table`.
    #[test]
    fn from_loaded_empty() {
        let loaded = LoadedTable { name: "empty".into(), columns: Vec::new(), row_count: 0 };
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

    /// `append_row_version` pushes a version into `row_versions` and
    /// respects the `row_versions.len() <= row_count` invariant.
    #[test]
    fn test_append_row_version() {
        let mut table = Table::from_loaded(sample_loaded());
        assert_eq!(table.row_count, 3);
        assert!(table.row_versions.is_empty());

        let v1 = RowVersion::new(1, vec![100, 200]);
        table.append_row_version(v1);

        assert_eq!(table.row_versions.len(), 1);
        assert_eq!(table.row_versions[0].xmin, 1);
        assert_eq!(table.row_versions[0].xmax, None);
        assert_eq!(table.row_versions[0].values, vec![100, 200]);
        assert!(!table.row_versions[0].deleted);

        // Fill the remaining slots.
        table.append_row_version(RowVersion::new(2, vec![300, 400]));
        table.append_row_version(RowVersion::new(3, vec![500, 600]));
        assert_eq!(table.row_versions.len(), 3);

        // The table is now full — appending a 4th version is a no-op.
        table.append_row_version(RowVersion::new(4, vec![700, 800]));
        assert_eq!(table.row_versions.len(), 3);
    }

    /// `mark_deleted` sets `xmax` on the version at `row_idx` and
    /// refuses to double-delete (returns `false`, logs a warning).
    #[test]
    fn test_mark_deleted() {
        let mut table = Table::from_loaded(sample_loaded());
        table.append_row_version(RowVersion::new(1, vec![10, 20]));
        assert_eq!(table.row_versions[0].xmax, None);

        // Fresh delete succeeds.
        let marked = table.mark_deleted(0, 42);
        assert!(marked);
        assert_eq!(table.row_versions[0].xmax, Some(42));

        // Double-delete is rejected.
        let marked_again = table.mark_deleted(0, 99);
        assert!(!marked_again);
        // xmax unchanged.
        assert_eq!(table.row_versions[0].xmax, Some(42));

        // Out-of-bounds index returns false.
        let oob = table.mark_deleted(5, 7);
        assert!(!oob);
    }

    /// `latest_visible_version` returns the live version visible to a
    /// transaction and returns `None` for already-deleted versions or
    /// out-of-bounds rows.
    #[test]
    fn test_latest_visible_version() {
        let mut table = Table::from_loaded(sample_loaded()); // 3 rows
        let mut mgr = MvccTxnManager::new();

        // Old txn (id=1) creates a version, commits (commit_id=1), and
        // is then used to delete that same version.
        let old_txn = mgr.begin(); // id=1, snapshot=0
        let old_cid = mgr.commit(old_txn.id); // commit_id=1
        assert_eq!(old_cid, 1);

        // "Old deleted" version at row 0: created by txn 1 and deleted
        // by txn 1 (which has committed).
        let mut old_version = RowVersion::new(old_txn.id, vec![100]);
        old_version.xmax = Some(old_txn.id);
        table.append_row_version(old_version);

        // New txn (id=2) begins with snapshot_id=1 (sees txn 1's commit).
        let new_txn = mgr.begin(); // id=2, snapshot=1

        // "New live" version at row 1: created by new_txn, never deleted.
        table.append_row_version(RowVersion::new(new_txn.id, vec![200]));

        // Row 0's version is committed-deleted before new_txn's snapshot,
        // so it is NOT visible to new_txn.
        let row0 = table.latest_visible_version(0, &mgr, &new_txn);
        assert!(row0.is_none(), "deleted old version should not be visible");

        // Row 1's version was created by new_txn itself and is live, so
        // it IS visible — this is "the new one" that should be returned.
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
    }
}
