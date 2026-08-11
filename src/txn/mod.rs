//! # Transaction manager.
//!
//! Two implementations coexist:
//! - [`TxnManager`]: the original deep-clone snapshot isolation (kept for
//!   backward compatibility). On `BEGIN`, the entire catalog is deep-cloned.
//! - [`MvccTxnManager`]: real multi-version concurrency control (Wave 64).
//!   Uses `(xmin, xmax)` version chains per row instead of deep-cloning.
//!   The engine uses MvccTxnManager when available; TxnManager remains as
//!   a fallback for code paths that haven't been migrated.
//!
//! ## Snapshot isolation vs MVCC
//!
//! The deep-clone approach is O(total_rows) per BEGIN — expensive for large
//! tables. MVCC is O(1) per BEGIN (just record the snapshot timestamp) and
//! O(1) per row visibility check.

pub mod mvcc;
pub use mvcc::{ConflictError, IsolationLevel, MvccTable, MvccTransaction, MvccTxnManager, RowVersion, TxnState};

use crate::catalog::Catalog;
use std::collections::HashMap;

/// A transaction snapshot: a deep copy of the catalog at BEGIN time.
pub struct Transaction {
    /// A unique transaction ID (monotonic).
    pub id: u64,
    /// The snapshot of the catalog at BEGIN time.
    pub snapshot: Catalog,
    /// Whether the transaction is active (not yet committed/rolled back).
    pub active: bool,
}

/// The transaction manager: tracks the next txn ID and the active
/// transaction (if any). turboGP supports one transaction at a time per
/// `QueryEngine` — concurrent connections each have their own engine
/// instance (wrapped in `Arc<Mutex<>>` by the server).
pub struct TxnManager {
    next_id: u64,
    /// The active transaction, if BEGIN has been called.
    pub active: Option<Transaction>,
}

impl TxnManager {
    /// Create a new transaction manager with no active transaction.
    pub fn new() -> Self {
        Self { next_id: 1, active: None }
    }

    /// Begin a new transaction. Returns an error if a transaction is
    /// already active.
    pub fn begin(&mut self, catalog: &Catalog) -> Result<u64, String> {
        if self.active.is_some() {
            return Err(
                "a transaction is already active (nested transactions not supported)".into()
            );
        }
        let id = self.next_id;
        self.next_id += 1;
        // Deep-clone the catalog. This is O(total_rows) — expensive but
        // correct for snapshot isolation. A future MVCC implementation
        // will use page-level versioning instead.
        let snapshot = clone_catalog(catalog);
        self.active = Some(Transaction { id, snapshot, active: true });
        Ok(id)
    }

    /// Commit the active transaction. The snapshot replaces the main
    /// catalog. Returns an error if no transaction is active.
    pub fn commit(&mut self) -> Result<Catalog, String> {
        let txn = self.active.take().ok_or("no active transaction to commit")?;
        Ok(txn.snapshot)
    }

    /// Rollback the active transaction. The snapshot is discarded.
    pub fn rollback(&mut self) -> Result<(), String> {
        self.active.take().ok_or("no active transaction to rollback")?;
        Ok(())
    }

    /// Returns true if a transaction is active.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Deep-clone a catalog. This clones every table and every column's
/// `Vec<u64>`. The `Arc<Vec<u64>>` columns are cloned into new `Arc`s
/// pointing at fresh `Vec`s.
pub fn clone_catalog(catalog: &Catalog) -> Catalog {
    let mut new_cat = Catalog::new();
    for name in catalog.table_names() {
        if let Some(table) = catalog.get(name) {
            let cloned = table.clone();
            new_cat.register(cloned);
        }
    }
    new_cat
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table as DS;

    fn make_catalog() -> Catalog {
        let mut c = Catalog::new();
        let t = DS::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2, 3],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 3,
        });
        c.register(t);
        c
    }

    #[test]
    fn begin_commit() {
        let cat = make_catalog();
        let mut mgr = TxnManager::new();
        let id = mgr.begin(&cat).unwrap();
        assert_eq!(id, 1);
        assert!(mgr.is_active());
        let committed = mgr.commit().unwrap();
        assert!(!mgr.is_active());
        assert_eq!(committed.get("t").unwrap().row_count, 3);
    }

    #[test]
    fn begin_rollback() {
        let cat = make_catalog();
        let mut mgr = TxnManager::new();
        mgr.begin(&cat).unwrap();
        assert!(mgr.is_active());
        mgr.rollback().unwrap();
        assert!(!mgr.is_active());
    }

    #[test]
    fn double_begin_fails() {
        let cat = make_catalog();
        let mut mgr = TxnManager::new();
        mgr.begin(&cat).unwrap();
        assert!(mgr.begin(&cat).is_err());
    }

    #[test]
    fn commit_without_begin_fails() {
        let mut mgr = TxnManager::new();
        assert!(mgr.commit().is_err());
    }

    #[test]
    fn rollback_without_begin_fails() {
        let mut mgr = TxnManager::new();
        assert!(mgr.rollback().is_err());
    }

    #[test]
    fn txn_ids_are_monotonic() {
        let cat = make_catalog();
        let mut mgr = TxnManager::new();
        let id1 = mgr.begin(&cat).unwrap();
        mgr.commit().unwrap();
        let id2 = mgr.begin(&cat).unwrap();
        mgr.rollback().unwrap();
        let id3 = mgr.begin(&cat).unwrap();
        mgr.commit().unwrap();
        assert!(id2 > id1);
        assert!(id3 > id2);
    }
}
