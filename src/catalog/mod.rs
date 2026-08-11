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
//! ## Concurrency (Task 2.1 — internal `RwLock`)
//!
//! The internal `tables` map is wrapped in a
//! [`parking_lot::RwLock<HashMap<String, Table>>`]. Readers
//! ([`Catalog::get`], [`Catalog::with`], [`Catalog::table_names`])
//! take a shared read guard; writers ([`Catalog::register`],
//! [`Catalog::with_mut`], [`Catalog::drop`]) take an exclusive write
//! guard. Callers no longer need to wrap the `Catalog` in an external
//! `Arc<RwLock<Catalog>>` for concurrent read access — multiple
//! worker threads can call `get()` / `with()` simultaneously.
//!
//! ### API shape
//!
//! Because the `HashMap` lives behind a `RwLock`, methods can no
//! longer return borrowed references into the map (the borrow would
//! outlive the read guard). The API therefore returns owned data:
//!
//! - [`Catalog::get`] returns `Option<Table>` (a clone). For
//!   read-heavy paths that touch only a few fields, prefer
//!   [`Catalog::with`], which runs a closure under the read guard
//!   and avoids the clone.
//! - [`Catalog::table_names`] returns `Vec<String>`.
//! - [`Catalog::with_mut`] replaces the old `get_mut` — it runs a
//!   `FnOnce(&mut Table) -> R` closure under the write guard, which
//!   scopes the mutable borrow and prevents callers from holding a
//!   `&mut Table` across other `&mut self` engine calls.
//!
//! The engine-level `Arc<RwLock<QueryEngine>>` still provides
//! coarse-grained locking (DML/DDL take an exclusive engine write
//! guard), so per-statement atomicity is preserved. The catalog's
//! internal lock is what allows concurrent **read-only** queries to
//! proceed in parallel once the engine read guard is held.

pub mod views;

use crate::datasource::table::Table;
use std::collections::HashMap;

/// An in-memory table catalog: table name → [`Table`].
pub struct Catalog {
    tables: parking_lot::RwLock<HashMap<String, Table>>,
}

impl Catalog {
    /// Create an empty catalog.
    pub fn new() -> Self {
        Catalog { tables: parking_lot::RwLock::new(HashMap::new()) }
    }

    /// Register a table under its own `name` field.
    ///
    /// If a table with the same name is already registered, the new
    /// table replaces it. Takes an exclusive write lock internally, so
    /// the signature is `&self` (callers no longer need `&mut self`).
    pub fn register(&self, table: Table) {
        let mut tables = self.tables.write();
        tables.insert(table.name.clone(), table);
    }

    /// Look up a table by name, returning an **owned clone**.
    ///
    /// This takes a shared read lock. For read-heavy paths that only
    /// touch a few fields, prefer [`Catalog::with`] to avoid the
    /// clone.
    pub fn get(&self, name: &str) -> Option<Table> {
        self.tables.read().get(name).cloned()
    }

    /// Scoped read-only access to a table.
    ///
    /// Runs `f` under the read guard and returns `Some(f(&table))` if
    /// the table exists, `None` otherwise. This avoids cloning the
    /// whole [`Table`] when the caller only needs to read a few
    /// fields.
    pub fn with<F, R>(&self, name: &str, f: F) -> Option<R>
    where
        F: FnOnce(&Table) -> R,
    {
        let tables = self.tables.read();
        tables.get(name).map(f)
    }

    /// Scoped mutable access to a table.
    ///
    /// Runs `f` under an exclusive write guard and returns
    /// `Some(f(&mut table))` if the table exists, `None` otherwise.
    /// Scoping the mutation inside a closure prevents callers from
    /// holding a `&mut Table` across other `&mut self` engine calls
    /// (which would conflict with the catalog's internal lock).
    pub fn with_mut<F, R>(&self, name: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut Table) -> R,
    {
        let mut tables = self.tables.write();
        tables.get_mut(name).map(f)
    }

    /// Drop a table by name. Returns `true` if the table existed.
    ///
    /// Takes an exclusive write lock internally (signature is `&self`).
    pub fn drop(&self, name: &str) -> bool {
        self.tables.write().remove(name).is_some()
    }

    /// Look up a column by `(table, column)` pair, returning owned
    /// cells.
    ///
    /// Convenience wrapper around [`Catalog::with`] +
    /// [`Table::column`]. Returns `None` if the table or column is
    /// missing.
    pub fn get_column(&self, table: &str, column: &str) -> Option<Vec<u64>> {
        let tables = self.tables.read();
        let t = tables.get(table)?;
        t.column(column).map(|c| c.to_vec())
    }

    /// List every registered table name.
    ///
    /// The order is unspecified (it follows `HashMap` iteration). If
    /// callers need a stable order they should sort the result.
    pub fn table_names(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }

    /// Number of registered tables.
    pub fn len(&self) -> usize {
        self.tables.read().len()
    }

    /// `true` if no tables are registered.
    pub fn is_empty(&self) -> bool {
        self.tables.read().is_empty()
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
    use std::sync::Arc;
    use std::thread;

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
        let cat = Catalog::new();
        let t = make_table("hits", "id", vec![1, 2, 3]);
        cat.register(t);

        let got = cat.get("hits").expect("table found");
        assert_eq!(got.name, "hits");
        assert_eq!(got.column("id"), Some(&[1u64, 2, 3][..]));
    }

    /// `get_column` is the two-arg convenience lookup.
    #[test]
    fn get_column_works() {
        let cat = Catalog::new();
        cat.register(make_table("hits", "id", vec![1, 2, 3]));
        assert_eq!(cat.get_column("hits", "id"), Some(vec![1u64, 2, 3]));
        assert_eq!(cat.get_column("hits", "missing"), None);
        assert_eq!(cat.get_column("missing", "id"), None);
    }

    /// Re-registering the same name overwrites.
    #[test]
    fn register_overwrites() {
        let cat = Catalog::new();
        cat.register(make_table("t", "a", vec![1, 2, 3]));
        cat.register(make_table("t", "b", vec![10, 20, 30]));

        let got = cat.get("t").expect("table found");
        assert_eq!(got.column("a"), None);
        assert_eq!(got.column("b"), Some(&[10u64, 20, 30][..]));
    }

    /// `table_names` lists every registered name.
    #[test]
    fn table_names_lists_all() {
        let cat = Catalog::new();
        cat.register(make_table("a", "x", vec![]));
        cat.register(make_table("b", "x", vec![]));
        cat.register(make_table("c", "x", vec![]));

        let mut names = cat.table_names();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    /// `len` and `is_empty` track registrations.
    #[test]
    fn len_and_is_empty() {
        let cat = Catalog::new();
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

    /// `with` runs a closure under the read guard without cloning.
    #[test]
    fn with_scoped_read() {
        let cat = Catalog::new();
        cat.register(make_table("t", "id", vec![1, 2, 3]));
        let n = cat.with("t", |t| t.row_count).expect("table found");
        assert_eq!(n, 3);
        assert!(cat.with("missing", |t| t.row_count).is_none());
    }

    /// `with_mut` runs a closure under the write guard.
    #[test]
    fn with_mut_scoped_write() {
        let cat = Catalog::new();
        cat.register(make_table("t", "id", vec![1, 2, 3]));
        cat.with_mut("t", |t| {
            t.row_count = 99;
        });
        assert_eq!(cat.get("t").unwrap().row_count, 99);
        // Missing table → None (closure not run).
        let ran = cat.with_mut("missing", |t: &mut Table| t.row_count);
        assert!(ran.is_none());
    }

    /// `drop` removes a table.
    #[test]
    fn drop_removes_table() {
        let cat = Catalog::new();
        cat.register(make_table("t", "id", vec![1, 2, 3]));
        assert!(cat.drop("t"));
        assert!(cat.get("t").is_none());
        assert!(!cat.drop("t")); // already gone
    }

    /// Task 2.3 — concurrent stress test.
    ///
    /// 10 threads each perform 100 iterations of `get` + `with` (read
    /// lock) and, for a subset, `register` (write lock) against a
    /// shared `Arc<Catalog>`. The test passes if all threads join
    /// without deadlock or panic, and the originally-registered table
    /// survives intact.
    #[test]
    fn test_concurrent_catalog_access() {
        let catalog = Arc::new(Catalog::new());
        catalog.register(make_table("t", "id", vec![1, 2, 3]));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let cat = catalog.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        // Concurrent reads (shared read lock).
                        let _ = cat.get("t");
                        cat.with("t", |t| {
                            let _ = t.row_count;
                            let _ = t.column("id").map(|c| c.len());
                        });
                        // A subset of threads also register extra
                        // tables (exclusive write lock). parking_lot's
                        // RwLock must not deadlock against the
                        // concurrent readers.
                        if i % 3 == 0 {
                            cat.register(make_table(&format!("extra_{i}"), "id", vec![9, 9, 9]));
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // The original table survives the concurrent churn.
        let t = catalog.get("t").expect("table 't' survives");
        assert_eq!(t.column("id"), Some(&[1u64, 2, 3][..]));
    }
}
