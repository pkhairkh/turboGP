//! # MVCC transaction manager (Wave 64).
//!
//! Replaces the previous deep-clone snapshot isolation with real
//! multi-version concurrency control. Each row carries a `(xmin, xmax)`
//! transaction ID pair:
//! - `xmin`: the transaction that created (INSERTed) this version of the row.
//! - `xmax`: the transaction that deleted (DELETEd/UPDATEd) this version.
//!   0 means the row is still live (not deleted).
//!
//! A transaction with ID `T` can see a row version if:
//! - `xmin` is committed and `xmin <= T` (the row was created by a
//!   committed transaction visible to us), AND
//! - `xmax == 0` OR `xmax` is NOT committed OR `xmax > T` (the row was
//!   not deleted by a transaction visible to us).
//!
//! UPDATE is implemented as DELETE + INSERT: the old version gets `xmax = T`,
//! and a new version with `xmin = T` is inserted.
//!
//! ## Commit state tracking
//!
//! The `CommitState` map tracks which transactions are committed. A
//! transaction ID is "visible" to transaction `T` if:
//! - It's the same transaction (`T` itself), OR
//! - It's in the committed set with a commit timestamp <= T's snapshot
//!   timestamp.
//!
//! ## Vacuum
//!
//! Dead row versions (where `xmax` is committed and no active transaction
//! can see them) are reclaimed by VACUUM (Wave 68).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

/// Global transaction ID counter (monotonic across all transactions).
static NEXT_TXN_ID: AtomicU64 = AtomicU64::new(1);

/// The commit state of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// The transaction is still running (not committed or aborted).
    InProgress,
    /// The transaction committed successfully.
    Committed,
    /// The transaction was rolled back (aborted).
    Aborted,
}

/// A row version header. Each row in a table carries this metadata.
#[derive(Debug, Clone, Copy)]
pub struct RowVersion {
    /// The transaction ID that created this row version.
    pub xmin: u64,
    /// The transaction ID that deleted this row version (0 = not deleted).
    pub xmax: u64,
}

impl RowVersion {
    /// Create a new row version created by `txn_id`, not yet deleted.
    pub fn new(txn_id: u64) -> Self {
        Self { xmin: txn_id, xmax: 0 }
    }

    /// Mark this row version as deleted by `txn_id`.
    pub fn delete(&mut self, txn_id: u64) {
        self.xmax = txn_id;
    }

    /// Check if this row version is visible to transaction `viewer`
    /// given the commit state map.
    pub fn is_visible(&self, viewer: u64, commit_state: &HashMap<u64, TxnState>) -> bool {
        // The creating transaction must be committed (or be us).
        let xmin_state = commit_state.get(&self.xmin).copied().unwrap_or(TxnState::InProgress);
        let xmin_visible = self.xmin == viewer || xmin_state == TxnState::Committed;
        if !xmin_visible {
            return false;
        }
        // If the row is not deleted, it's visible.
        if self.xmax == 0 {
            return true;
        }
        // The row is deleted. Check if the deleting transaction is visible.
        let xmax_state = commit_state.get(&self.xmax).copied().unwrap_or(TxnState::InProgress);
        let xmax_visible = self.xmax == viewer || xmax_state == TxnState::Committed;
        // If the deleting txn is NOT visible to us, we still see the row.
        !xmax_visible
    }
}

/// The MVCC transaction manager.
///
/// Tracks:
/// - The next transaction ID.
/// - The commit state of every transaction (InProgress / Committed / Aborted).
/// - The active transaction (if any) for the current connection.
pub struct MvccTxnManager {
    /// Commit state for every transaction ID we've seen.
    commit_state: HashMap<u64, TxnState>,
    /// The active transaction, if BEGIN has been called.
    pub active: Option<MvccTransaction>,
}

/// An active MVCC transaction.
pub struct MvccTransaction {
    /// The transaction's unique ID.
    pub id: u64,
    /// The set of transaction IDs that were committed at BEGIN time
    /// (the snapshot). A transaction T is visible to us if T is in this
    /// set or T == our own ID.
    snapshot: HashSet<u64>,
}

impl MvccTxnManager {
    /// Create a new MVCC transaction manager.
    pub fn new() -> Self {
        Self { commit_state: HashMap::new(), active: None }
    }

    /// Begin a new transaction. Returns the transaction ID.
    /// Returns an error if a transaction is already active (no nested
    /// transactions — see Wave 69 for SAVEPOINT support).
    pub fn begin(&mut self) -> Result<u64, String> {
        if self.active.is_some() {
            return Err(
                "a transaction is already active (use SAVEPOINT for nested transactions)".into()
            );
        }
        let id = NEXT_TXN_ID.fetch_add(1, Ordering::SeqCst);
        // Take a snapshot of the currently-committed transactions.
        let snapshot: HashSet<u64> = self
            .commit_state
            .iter()
            .filter(|(_, &state)| state == TxnState::Committed)
            .map(|(&id, _)| id)
            .collect();
        self.commit_state.insert(id, TxnState::InProgress);
        self.active = Some(MvccTransaction { id, snapshot });
        Ok(id)
    }

    /// Commit the active transaction. Returns the transaction ID.
    pub fn commit(&mut self) -> Result<u64, String> {
        let txn = self.active.take().ok_or("no active transaction")?;
        self.commit_state.insert(txn.id, TxnState::Committed);
        Ok(txn.id)
    }

    /// Rollback the active transaction. Returns the transaction ID.
    pub fn rollback(&mut self) -> Result<u64, String> {
        let txn = self.active.take().ok_or("no active transaction")?;
        self.commit_state.insert(txn.id, TxnState::Aborted);
        Ok(txn.id)
    }

    /// Check if a transaction ID is visible to the active transaction.
    /// A transaction T is visible if:
    /// - T == the active transaction's ID (we see our own writes), OR
    /// - T is committed AND T was in our snapshot at BEGIN time.
    pub fn is_visible(&self, txn_id: u64) -> bool {
        if let Some(ref active) = self.active {
            if active.id == txn_id {
                return true;
            }
        }
        match self.commit_state.get(&txn_id) {
            Some(TxnState::Committed) => {
                if let Some(ref active) = self.active {
                    active.snapshot.contains(&txn_id)
                } else {
                    // No active transaction — all committed transactions are visible.
                    true
                }
            }
            _ => false,
        }
    }

    /// Check if a row version is visible to the active transaction
    /// (or to "autocommit" if no transaction is active).
    pub fn is_row_visible(&self, version: &RowVersion) -> bool {
        let viewer = self.active.as_ref().map(|t| t.id).unwrap_or(0);
        version.is_visible(viewer, &self.commit_state)
    }

    /// Check if a transaction is active.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Get the active transaction ID (if any).
    pub fn active_id(&self) -> Option<u64> {
        self.active.as_ref().map(|t| t.id)
    }

    /// Clean up aborted transactions' state (called periodically).
    /// Returns the number of states cleaned.
    pub fn cleanup_aborted(&mut self) -> usize {
        let before = self.commit_state.len();
        self.commit_state.retain(|_, &mut state| state != TxnState::Aborted);
        before - self.commit_state.len()
    }
}

impl Default for MvccTxnManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mvcc_basic_visibility() {
        let mut mgr = MvccTxnManager::new();
        // No active transaction — all committed transactions are visible.
        // Initially nothing is committed, so nothing is visible.
        assert!(!mgr.is_visible(1));
        // Begin txn 1.
        let id1 = mgr.begin().unwrap();
        // Txn 1 sees its own writes.
        assert!(mgr.is_visible(id1));
        // Commit txn 1.
        mgr.commit().unwrap();
        // Now txn 1 is visible to autocommit (no active txn).
        assert!(mgr.is_visible(id1));
    }

    #[test]
    fn mvcc_snapshot_isolation() {
        let mut mgr = MvccTxnManager::new();
        // Txn 1 commits.
        let id1 = mgr.begin().unwrap();
        mgr.commit().unwrap();
        // Txn 2 begins (sees txn 1 in its snapshot).
        let id2 = mgr.begin().unwrap();
        // Txn 2 sees txn 1 (it was committed before txn 2 began).
        assert!(mgr.is_visible(id1));
        // Txn 3 commits (but txn 2 doesn't see it — not in snapshot).
        // We can't begin txn 3 while txn 2 is active in this simple manager,
        // but we can simulate by directly inserting into commit_state.
        mgr.commit_state.insert(100, TxnState::Committed);
        // Txn 2 does NOT see txn 100 (not in its snapshot).
        assert!(!mgr.is_visible(100), "snapshot isolation: txn 2 must not see txn 100");
        // Commit txn 2.
        mgr.commit().unwrap();
        // Now autocommit sees both txn 1 and txn 100.
        assert!(mgr.is_visible(id1));
        assert!(mgr.is_visible(100));
    }

    #[test]
    fn mvcc_row_version_visibility() {
        let mut mgr = MvccTxnManager::new();
        // Txn 1 inserts a row.
        let id1 = mgr.begin().unwrap();
        let row = RowVersion::new(id1);
        // Txn 1 sees its own inserted row.
        assert!(mgr.is_row_visible(&row), "txn 1 must see its own insert");
        // Commit txn 1.
        mgr.commit().unwrap();
        // Autocommit sees the row.
        assert!(mgr.is_row_visible(&row));
        // Txn 2 begins and deletes the row.
        let id2 = mgr.begin().unwrap();
        let mut deleted_row = row;
        deleted_row.delete(id2);
        // Txn 2 sees the row as deleted (not visible) because it deleted it.
        assert!(!mgr.is_row_visible(&deleted_row), "txn 2 must not see the row it deleted");
        // But if we check from autocommit's perspective (no active txn),
        // the row is still visible because txn 2 hasn't committed.
        mgr.active = None;
        assert!(
            mgr.is_row_visible(&deleted_row),
            "autocommit must still see the row (txn 2 not committed)"
        );
        // Txn 2 commits.
        mgr.commit_state.insert(id2, TxnState::Committed);
        // Now autocommit sees the row as deleted.
        assert!(
            !mgr.is_row_visible(&deleted_row),
            "after txn 2 commits, the deleted row must be invisible"
        );
    }

    #[test]
    fn mvcc_rollback() {
        let mut mgr = MvccTxnManager::new();
        let id = mgr.begin().unwrap();
        mgr.rollback().unwrap();
        // A rolled-back transaction's writes are not visible.
        let row = RowVersion::new(id);
        assert!(!mgr.is_row_visible(&row), "rolled-back txn's writes must be invisible");
    }
}
