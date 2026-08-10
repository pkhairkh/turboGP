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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::LoadedColumn;

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
}
