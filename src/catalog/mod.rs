//! # Catalog — table name → [`Table`] registry.
//!
//! A simple in-memory catalog. Maps a table name (e.g. `"hits"`,
//! `"lineitem"`) to a [`Table`] that has been loaded from Parquet or
//! CSV.
//!
//! ## Why now
//!
//! The SQL planner (see [`crate::sql::plan`]) currently hashes the
//! table name to a region ID — there is no schema lookup. Once a
//! catalog exists, the planner can resolve table names to actual
//! column data without going through the storage layer's region
//! abstraction. This is the first step toward DuckDB-style
//! `SELECT * FROM 'hits.parquet'`.
//!
//! ## Concurrency
//!
//! Not yet. The catalog is a single-threaded `HashMap`; callers that
//! need to share it across worker threads should wrap it in an
//! `Arc<RwLock<Catalog>>` themselves. The morsel executor currently
//! snapshots the catalog into per-worker borrows at scheduling time,
//! so the registry itself never sees concurrent access during a
//! query.

pub mod views;

use crate::datasource::table::Table;
use std::collections::HashMap;

/// An in-memory table catalog: table name → [`Table`].
pub struct Catalog {
    tables: HashMap<String, Table>,
}

impl Catalog {
    /// Create an empty catalog.
    pub fn new() -> Self {
        Catalog { tables: HashMap::new() }
    }

    /// Register a table under its own `name` field.
    ///
    /// If a table with the same name is already registered, the new
    /// table replaces it.
    pub fn register(&mut self, table: Table) {
        self.tables.insert(table.name.clone(), table);
    }

    /// Look up a table by name.
    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    /// Look up a table by name, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name)
    }

    /// Drop a table by name. Returns true if the table existed.
    pub fn drop(&mut self, name: &str) -> bool {
        self.tables.remove(name).is_some()
    }

    /// Look up a column by `(table, column)` pair.
    ///
    /// Convenience wrapper around [`Catalog::get`] +
    /// [`Table::column`].
    pub fn get_column(&self, table: &str, column: &str) -> Option<&[u64]> {
        self.get(table)?.column(column)
    }

    /// List every registered table name.
    ///
    /// The order is unspecified (it follows `HashMap` iteration). If
    /// callers need a stable order they should sort the result.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.keys().map(|s| s.as_str()).collect()
    }

    /// Number of registered tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// `true` if no tables are registered.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};

    /// Build a small `Table` for tests.
    fn make_table(name: &str, col_name: &str, cells: Vec<u64>) -> Table {
        Table::from_loaded(LoadedTable {
            name: name.into(),
            columns: vec![LoadedColumn {
                name: col_name.into(),
                cells,
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 3,
        })
    }

    /// `register` then `get` returns the same table.
    #[test]
    fn register_and_get() {
        let mut cat = Catalog::new();
        let t = make_table("hits", "id", vec![1, 2, 3]);
        cat.register(t);

        let got = cat.get("hits").expect("table found");
        assert_eq!(got.name, "hits");
        assert_eq!(got.column("id"), Some(&[1u64, 2, 3][..]));
    }

    /// `get_column` is the two-arg convenience lookup.
    #[test]
    fn get_column_works() {
        let mut cat = Catalog::new();
        cat.register(make_table("hits", "id", vec![1, 2, 3]));
        assert_eq!(cat.get_column("hits", "id"), Some(&[1u64, 2, 3][..]));
        assert_eq!(cat.get_column("hits", "missing"), None);
        assert_eq!(cat.get_column("missing", "id"), None);
    }

    /// Re-registering the same name overwrites.
    #[test]
    fn register_overwrites() {
        let mut cat = Catalog::new();
        cat.register(make_table("t", "a", vec![1, 2, 3]));
        cat.register(make_table("t", "b", vec![10, 20, 30]));

        let got = cat.get("t").expect("table found");
        assert_eq!(got.column("a"), None);
        assert_eq!(got.column("b"), Some(&[10u64, 20, 30][..]));
    }

    /// `table_names` lists every registered name.
    #[test]
    fn table_names_lists_all() {
        let mut cat = Catalog::new();
        cat.register(make_table("a", "x", vec![]));
        cat.register(make_table("b", "x", vec![]));
        cat.register(make_table("c", "x", vec![]));

        let mut names = cat.table_names();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    /// `len` and `is_empty` track registrations.
    #[test]
    fn len_and_is_empty() {
        let mut cat = Catalog::new();
        assert!(cat.is_empty());
        assert_eq!(cat.len(), 0);

        cat.register(make_table("a", "x", vec![]));
        assert!(!cat.is_empty());
        assert_eq!(cat.len(), 1);

        cat.register(make_table("b", "x", vec![]));
        assert_eq!(cat.len(), 2);
    }

    /// `Catalog::default()` is equivalent to `Catalog::new()`.
    #[test]
    fn default_is_empty() {
        let cat = Catalog::default();
        assert!(cat.is_empty());
    }

    /// `get` on a missing name returns `None`.
    #[test]
    fn get_missing_returns_none() {
        let cat = Catalog::new();
        assert!(cat.get("nope").is_none());
    }
}
