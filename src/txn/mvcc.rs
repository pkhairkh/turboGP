//! # MVCC transaction manager (Wave 4 redesign).
//!
//! Real multi-version concurrency control with per-row version chains.
//! Each row carries a list of `RowVersion` entries, each tagged with
//! `(xmin, xmax)` transaction IDs:
//! - `xmin`: the transaction that created (INSERTed) this version.
//! - `xmax`: the transaction that deleted (DELETEd/UPDATEd) this version.
//!   `None` means the version is still live.
//!
//! A version is visible to transaction `T` (with snapshot_id `S`) if:
//! - `xmin` was committed before `T` started (`xmin`'s commit_id <= `S`),
//!   OR `xmin == T.id` (T sees its own writes), AND
//! - `xmax` is `None`, OR `xmax` was NOT committed before `T` started
//!   (`xmax`'s commit_id > `S` or `xmax` is InProgress/Aborted), OR
//!   `xmax == T.id` (T deleted it, so it's invisible to T).
//!
//! UPDATE is implemented as DELETE + INSERT: the old version gets
//! `xmax = T.id`, and a new version with `xmin = T.id` is appended.
//!
//! ## O(1) BEGIN
//!
//! `begin()` assigns `snapshot_id = current_commit_id` — O(1), no
//! catalog clone. This replaces the old HashSet-snapshot approach.
//!
//! ## VACUUM
//!
//! Dead versions (where `xmax` is committed and no active transaction
//! can see them) are reclaimed by `vacuum()`.

use std::collections::{HashMap, HashSet};

/// A transaction ID (monotonic).
pub type TxnId = u64;

/// The commit state of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// The transaction is still running (not committed or aborted).
    InProgress,
    /// The transaction committed successfully. The enclosed value is the
    /// `commit_id` assigned at commit time (monotonic across all commits).
    Committed(TxnId),
    /// The transaction was rolled back (aborted).
    Aborted,
}

/// A row version. Each logical row has a `Vec<RowVersion>` (version chain).
///
/// Task 4.1: carries `values` (the actual column data) and a `deleted`
/// flag so the version chain can represent both INSERT and DELETE without
/// relying on an external table.
#[derive(Debug, Clone)]
pub struct RowVersion {
    /// The transaction ID that created this row version.
    pub xmin: TxnId,
    /// The transaction ID that deleted this row version (`None` = still live).
    pub xmax: Option<TxnId>,
    /// The column values for this version.
    pub values: Vec<u64>,
    /// True if this version represents a logical delete (the row was
    /// deleted by `xmin`). When `deleted` is true, `values` is empty.
    pub deleted: bool,
}

impl RowVersion {
    /// Create a new live row version created by `txn_id` with the given values.
    pub fn new(txn_id: TxnId, values: Vec<u64>) -> Self {
        Self { xmin: txn_id, xmax: None, values, deleted: false }
    }

    /// Create a new delete-marker version created by `txn_id`.
    pub fn new_delete(txn_id: TxnId) -> Self {
        Self { xmin: txn_id, xmax: None, values: Vec::new(), deleted: true }
    }

    /// Mark this version as deleted by `txn_id` (sets `xmax`).
    pub fn delete(&mut self, txn_id: TxnId) {
        self.xmax = Some(txn_id);
    }

    /// Check if this version is deleted (has an xmax).
    pub fn is_deleted(&self) -> bool {
        self.xmax.is_some()
    }
}

/// An MVCC table: holds a version chain for each logical row.
///
/// Task 4.1: the version chain is `Vec<Vec<RowVersion>>` — one `Vec<RowVersion>`
/// per row. New versions are appended; old versions are left for VACUUM.
#[derive(Debug, Clone, Default)]
pub struct MvccTable {
    /// Table name (for debugging).
    pub name: String,
    /// Column names.
    pub column_names: Vec<String>,
    /// Version chains: one entry per logical row.
    pub rows: Vec<Vec<RowVersion>>,
}

impl MvccTable {
    /// Create a new empty MVCC table.
    pub fn new(name: impl Into<String>, column_names: Vec<String>) -> Self {
        Self { name: name.into(), column_names, rows: Vec::new() }
    }

    /// Insert a new row (append a fresh version chain with one version
    /// created by `txn_id`).
    pub fn insert(&mut self, txn_id: TxnId, values: Vec<u64>) -> usize {
        let row_idx = self.rows.len();
        self.rows.push(vec![RowVersion::new(txn_id, values)]);
        row_idx
    }

    /// Delete a row: mark the latest visible version as deleted by `txn_id`.
    /// Returns `true` if a version was marked, `false` if the row doesn't exist.
    pub fn delete(&mut self, txn_id: TxnId, row_idx: usize) -> bool {
        if let Some(chain) = self.rows.get_mut(row_idx) {
            if let Some(version) = chain.last_mut() {
                if version.xmax.is_none() {
                    version.delete(txn_id);
                    return true;
                }
            }
        }
        false
    }

    /// Update a row: mark the latest version as deleted, then append a new
    /// version with the new values. Returns `true` on success.
    pub fn update(&mut self, txn_id: TxnId, row_idx: usize, new_values: Vec<u64>) -> bool {
        if let Some(chain) = self.rows.get_mut(row_idx) {
            if let Some(version) = chain.last_mut() {
                if version.xmax.is_none() {
                    version.delete(txn_id);
                    chain.push(RowVersion::new(txn_id, new_values));
                    return true;
                }
            }
        }
        false
    }

    /// Get the version chain for a row.
    pub fn row_versions(&self, row_idx: usize) -> &[RowVersion] {
        self.rows.get(row_idx).map(|c| c.as_slice()).unwrap_or(&[])
    }

    /// Number of logical rows (version chains).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

// =========================================================================
// MvccTransaction and MvccTxnManager — Task 4.2
// =========================================================================

/// An active MVCC transaction.
///
/// Task 4.2: carries `snapshot_id` (the commit_id at BEGIN time) and `state`.
#[derive(Debug, Clone)]
pub struct MvccTransaction {
    /// The transaction's unique ID.
    pub id: TxnId,
    /// The commit_id at BEGIN time. The transaction sees all data committed
    /// by transactions with commit_id <= snapshot_id.
    pub snapshot_id: TxnId,
    /// The current state (InProgress, Committed, Aborted).
    pub state: TxnState,
}

/// Error type for MVCC write-write conflicts (Task 4.4).
#[derive(Debug, Clone)]
pub struct ConflictError {
    /// Human-readable description of the conflict.
    pub message: String,
    /// The transaction ID that caused the conflict.
    pub conflicting_txn: TxnId,
}

/// The MVCC transaction manager.
///
/// Tracks:
/// - `next_txn_id`: monotonic counter for assigning transaction IDs.
/// - `commit_id`: monotonic counter incremented on each COMMIT.
/// - `txn_states`: maps every txn_id to its current state + commit metadata.
/// - `active`: the set of currently-active transaction IDs (for VACUUM).
///
/// Task 4.2: `begin()` is O(1) — just assigns `snapshot_id = commit_id`.
/// Multiple concurrent transactions are supported.
pub struct MvccTxnManager {
    next_txn_id: TxnId,
    commit_id: TxnId,
    txn_states: HashMap<TxnId, TxnState>,
    active: HashSet<TxnId>,
    /// Maps each active txn_id to its snapshot_id (for VACUUM).
    active_snapshots: HashMap<TxnId, TxnId>,
}

impl MvccTxnManager {
    /// Create a new MVCC transaction manager.
    pub fn new() -> Self {
        Self {
            next_txn_id: 1,
            commit_id: 0,
            txn_states: HashMap::new(),
            active: HashSet::new(),
            active_snapshots: HashMap::new(),
        }
    }

    /// Begin a new transaction. O(1): assigns `snapshot_id = commit_id`.
    /// Returns the transaction handle.
    pub fn begin(&mut self) -> MvccTransaction {
        let id = self.next_txn_id;
        self.next_txn_id += 1;
        let snapshot_id = self.commit_id;
        self.txn_states.insert(id, TxnState::InProgress);
        self.active.insert(id);
        self.active_snapshots.insert(id, snapshot_id);
        MvccTransaction { id, snapshot_id, state: TxnState::InProgress }
    }

    /// Commit a transaction. Increments `commit_id` and records the
    /// transaction as `Committed(commit_id)`. Returns the commit_id.
    pub fn commit(&mut self, txn_id: TxnId) -> TxnId {
        self.commit_id += 1;
        let cid = self.commit_id;
        self.txn_states.insert(txn_id, TxnState::Committed(cid));
        self.active.remove(&txn_id);
        self.active_snapshots.remove(&txn_id);
        cid
    }

    /// Rollback (abort) a transaction.
    pub fn rollback(&mut self, txn_id: TxnId) {
        self.txn_states.insert(txn_id, TxnState::Aborted);
        self.active.remove(&txn_id);
        self.active_snapshots.remove(&txn_id);
    }

    /// Get the state of a transaction.
    pub fn txn_state(&self, txn_id: TxnId) -> TxnState {
        self.txn_states.get(&txn_id).copied().unwrap_or(TxnState::Aborted)
    }

    /// The current commit_id (monotonic).
    pub fn current_commit_id(&self) -> TxnId {
        self.commit_id
    }

    /// The oldest active snapshot_id (for VACUUM). If no transactions are
    /// active, returns the current commit_id.
    pub fn oldest_active_snapshot(&self) -> TxnId {
        let mut oldest = self.commit_id;
        for (&tid, snapshot_id) in &self.active_snapshots {
            if matches!(self.txn_states.get(&tid), Some(TxnState::InProgress)) {
                oldest = oldest.min(*snapshot_id);
            }
        }
        oldest
    }

    // -----------------------------------------------------------------
    // Task 4.3: visibility checks
    // -----------------------------------------------------------------

    /// Check if a transaction's effects are visible to the given snapshot.
    ///
    /// A transaction `author_id` is visible to snapshot `snapshot_id` if:
    /// - `author_id`'s state is `Committed(cid)` where `cid <= snapshot_id`.
    ///
    /// Returns `false` for InProgress (not yet committed) and Aborted txns.
    fn txn_visible_to_snapshot(&self, author_id: TxnId, snapshot_id: TxnId) -> bool {
        match self.txn_states.get(&author_id) {
            Some(TxnState::Committed(cid)) => *cid <= snapshot_id,
            _ => false,
        }
    }

    /// Check if a row version is visible to the given transaction (Task 4.3).
    ///
    /// A version is visible to `txn` if:
    /// - `xmin == txn.id` (T sees its own inserts), OR `xmin` was committed
    ///   before T's snapshot (`txn_visible_to_snapshot(xmin, txn.snapshot_id)`), AND
    /// - `xmax` is `None` (version is live), OR `xmax == txn.id` (T deleted
    ///   it, so it's invisible to T), OR `xmax` was NOT committed before T's
    ///   snapshot (the deleting txn hasn't committed yet, or committed after
    ///   T started — so the version is still visible to T).
    pub fn visible(&self, version: &RowVersion, txn: &MvccTransaction) -> bool {
        // Check xmin: the version must have been created by a transaction
        // visible to us (or by us).
        let xmin_visible = version.xmin == txn.id
            || self.txn_visible_to_snapshot(version.xmin, txn.snapshot_id);
        if !xmin_visible {
            return false;
        }
        // Check xmax: if the version is deleted, check if the deleting
        // transaction is visible to us.
        match version.xmax {
            None => true, // Still live — visible.
            Some(xmax) => {
                if xmax == txn.id {
                    // We deleted it — invisible to us.
                    false
                } else {
                    // If the deleting txn is NOT visible to us (not committed
                    // before our snapshot), the version is still visible.
                    !self.txn_visible_to_snapshot(xmax, txn.snapshot_id)
                }
            }
        }
    }

    /// Scan a table and return all versions visible to the transaction
    /// (Task 4.3). Returns references to the visible `RowVersion`s.
    ///
    /// For each row, only the LATEST visible version is included (snapshot
    /// isolation: a transaction sees at most one version of each row).
    pub fn scan_visible<'a>(
        &self,
        table: &'a MvccTable,
        txn: &MvccTransaction,
    ) -> Vec<&'a RowVersion> {
        let mut result = Vec::new();
        for chain in &table.rows {
            // Find the latest visible version in the chain.
            // Iterate in reverse so the newest visible version wins.
            for version in chain.iter().rev() {
                if self.visible(version, txn) {
                    result.push(version);
                    break;
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------
    // Task 4.4: write-write conflict detection (first-committer-wins)
    // -----------------------------------------------------------------

    /// Check if a transaction can update/delete a row without conflicting
    /// with concurrent transactions (Task 4.4 — first-committer-wins).
    ///
    /// The check finds the version VISIBLE TO `txn` (the one `txn` would
    /// read). A conflict occurs if:
    /// - That visible version has `xmax` set by a concurrent UNCOMMITTED
    ///   transaction (another active txn is modifying this row), OR
    /// - That visible version has `xmax` set by a transaction that
    ///   committed AFTER `txn`'s snapshot (the row was modified by a txn
    ///   `txn` can't see — first-committer-wins).
    ///
    /// Returns `Ok(())` if the write can proceed, or `Err(ConflictError)`
    /// if there's a conflict.
    pub fn check_write_conflict(
        &self,
        table: &MvccTable,
        txn: &MvccTransaction,
        row_idx: usize,
    ) -> Result<(), ConflictError> {
        let chain = match table.rows.get(row_idx) {
            Some(c) => c,
            None => return Ok(()), // Row doesn't exist — no conflict.
        };
        // Find the version visible to `txn` (the one it would read).
        // Iterate in reverse to get the newest visible version.
        let visible_version = chain.iter().rev().find(|v| self.visible(v, txn));
        let version = match visible_version {
            Some(v) => v,
            None => {
                // No visible version — the row doesn't exist from txn's
                // perspective. Not a conflict (it's a "row not found").
                return Ok(());
            }
        };
        // If the visible version is already deleted (xmax is Some), check
        // who deleted it.
        if let Some(xmax) = version.xmax {
            if xmax == txn.id {
                // We already deleted it — no conflict (idempotent).
                return Ok(());
            }
            match self.txn_states.get(&xmax) {
                Some(TxnState::InProgress) => {
                    // Another active transaction is modifying this row.
                    return Err(ConflictError {
                        message: format!(
                            "write-write conflict: row {} is being modified by active transaction {}",
                            row_idx, xmax
                        ),
                        conflicting_txn: xmax,
                    });
                }
                Some(TxnState::Committed(cid)) => {
                    // The deleting txn committed. If it committed AFTER our
                    // snapshot, we have a conflict (the row changed under us).
                    if *cid > txn.snapshot_id {
                        return Err(ConflictError {
                            message: format!(
                                "write-write conflict: row {} was modified by transaction {} (committed after our snapshot)",
                                row_idx, xmax
                            ),
                            conflicting_txn: xmax,
                        });
                    }
                    // It committed before our snapshot — the row is already
                    // deleted from our perspective. Not a conflict per se,
                    // but we can't update a deleted row.
                    return Err(ConflictError {
                        message: format!(
                            "write-write conflict: row {} was already deleted before our snapshot",
                            row_idx
                        ),
                        conflicting_txn: xmax,
                    });
                }
                Some(TxnState::Aborted) | None => {
                    // The deleting txn aborted — the version is still live.
                    return Ok(());
                }
            }
        }
        // The visible version is live (xmax is None) — no conflict.
        Ok(())
    }

    /// Update a row with conflict detection (Task 4.4). Returns
    /// `Err(ConflictError)` if a concurrent transaction has modified the
    /// row. On success, the old version is marked deleted and a new
    /// version is appended.
    pub fn update_with_conflict_check(
        &self,
        table: &mut MvccTable,
        txn: &MvccTransaction,
        row_idx: usize,
        new_values: Vec<u64>,
    ) -> Result<(), ConflictError> {
        self.check_write_conflict(table, txn, row_idx)?;
        table.update(txn.id, row_idx, new_values);
        Ok(())
    }

    /// Delete a row with conflict detection (Task 4.4).
    pub fn delete_with_conflict_check(
        &self,
        table: &mut MvccTable,
        txn: &MvccTransaction,
        row_idx: usize,
    ) -> Result<(), ConflictError> {
        self.check_write_conflict(table, txn, row_idx)?;
        table.delete(txn.id, row_idx);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Task 4.5: garbage collection (VACUUM)
    // -----------------------------------------------------------------

    /// Remove dead row versions from the given tables (Task 4.5).
    ///
    /// A version is "dead" if:
    /// - Its `xmax` is `Some(tid)` where `tid` is committed (Committed
    ///   state) AND `tid`'s commit_id < oldest_active_snapshot (no active
    ///   transaction can see this version anymore).
    /// - OR its `xmin` is `Aborted` (the creating transaction rolled back).
    ///
    /// Returns the total number of versions removed across all tables.
    pub fn vacuum(&mut self, tables: &mut [MvccTable]) -> usize {
        let oldest = self.oldest_active_snapshot();
        let mut removed = 0;
        for table in tables.iter_mut() {
            for chain in table.rows.iter_mut() {
                let before = chain.len();
                chain.retain(|version| {
                    // Keep versions whose xmin is aborted? No — aborted
                    // versions should be removed.
                    if matches!(self.txn_states.get(&version.xmin), Some(TxnState::Aborted)) {
                        return false; // Remove aborted inserts.
                    }
                    // If the version is deleted (xmax is Some), check if
                    // the deleting txn is committed before the oldest
                    // active snapshot. If so, the version is dead.
                    if let Some(xmax) = version.xmax {
                        if let Some(TxnState::Committed(cid)) = self.txn_states.get(&xmax) {
                            if *cid <= oldest {
                                return false; // Dead version — remove.
                            }
                        }
                    }
                    true // Keep all other versions.
                });
                removed += before - chain.len();
            }
        }
        removed
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

    /// Task 4.1 DoD: RowVersion carries xmin/xmax/values/deleted.
    #[test]
    fn row_version_basic() {
        let v = RowVersion::new(1, vec![10, 20, 30]);
        assert_eq!(v.xmin, 1);
        assert!(v.xmax.is_none());
        assert_eq!(v.values, vec![10, 20, 30]);
        assert!(!v.deleted);
        assert!(!v.is_deleted());

        let mut v2 = v.clone();
        v2.delete(2);
        assert_eq!(v2.xmax, Some(2));
        assert!(v2.is_deleted());
    }

    /// Task 4.1 DoD: MvccTable version chain — insert/delete/update.
    #[test]
    fn mvcc_table_version_chain() {
        let mut t = MvccTable::new("users", vec!["id".into(), "name".into()]);
        let row0 = t.insert(1, vec![1, 100]);
        let row1 = t.insert(1, vec![2, 200]);
        assert_eq!(t.row_count(), 2);

        // Each row has one version.
        assert_eq!(t.row_versions(row0).len(), 1);
        assert_eq!(t.row_versions(row0)[0].values, vec![1, 100]);

        // Update row 0: marks old version deleted, appends new version.
        t.update(2, row0, vec![1, 999]);
        assert_eq!(t.row_versions(row0).len(), 2);
        assert!(t.row_versions(row0)[0].is_deleted());
        assert_eq!(t.row_versions(row0)[1].values, vec![1, 999]);

        // Delete row 1.
        t.delete(2, row1);
        assert!(t.row_versions(row1)[0].is_deleted());
    }

    /// Task 4.2 DoD: begin() is O(1) and assigns snapshot_id = commit_id.
    #[test]
    fn mvcc_begin_assigns_snapshot_id() {
        let mut mgr = MvccTxnManager::new();
        assert_eq!(mgr.current_commit_id(), 0);
        let txn = mgr.begin();
        assert_eq!(txn.id, 1);
        assert_eq!(txn.snapshot_id, 0, "snapshot_id = commit_id at begin time");
        assert_eq!(txn.state, TxnState::InProgress);
    }

    /// Task 4.2 DoD: commit increments commit_id and records state.
    #[test]
    fn mvcc_commit_increments_commit_id() {
        let mut mgr = MvccTxnManager::new();
        let txn = mgr.begin();
        let cid = mgr.commit(txn.id);
        assert_eq!(cid, 1);
        assert_eq!(mgr.current_commit_id(), 1);
        assert_eq!(mgr.txn_state(txn.id), TxnState::Committed(1));
    }

    /// Task 4.2 DoD: multiple concurrent transactions are supported.
    #[test]
    fn mvcc_multiple_concurrent_transactions() {
        let mut mgr = MvccTxnManager::new();
        let t1 = mgr.begin();
        let t2 = mgr.begin();
        let t3 = mgr.begin();
        assert!(t1.id < t2.id);
        assert!(t2.id < t3.id);
        // All three are InProgress simultaneously.
        assert_eq!(mgr.txn_state(t1.id), TxnState::InProgress);
        assert_eq!(mgr.txn_state(t2.id), TxnState::InProgress);
        assert_eq!(mgr.txn_state(t3.id), TxnState::InProgress);
        // Commit t2 — t1 and t3 are still active.
        mgr.commit(t2.id);
        assert_eq!(mgr.txn_state(t1.id), TxnState::InProgress);
        assert_eq!(mgr.txn_state(t2.id), TxnState::Committed(1));
        assert_eq!(mgr.txn_state(t3.id), TxnState::InProgress);
    }

    /// Task 4.2 DoD: rollback marks the transaction as Aborted.
    #[test]
    fn mvcc_rollback() {
        let mut mgr = MvccTxnManager::new();
        let txn = mgr.begin();
        mgr.rollback(txn.id);
        assert_eq!(mgr.txn_state(txn.id), TxnState::Aborted);
    }

    // -----------------------------------------------------------------
    // Task 4.3: visibility checks
    // -----------------------------------------------------------------

    /// Task 4.3 DoD: T1 inserts a row but doesn't commit; T2 begins and
    /// scans → doesn't see T1's row. T1 commits; T3 begins and scans →
    /// sees T1's row.
    #[test]
    fn mvcc_visibility_uncommitted_not_visible() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        // T1 inserts a row but doesn't commit.
        let t1 = mgr.begin();
        table.insert(t1.id, vec![42]);

        // T2 begins and scans — must NOT see T1's uncommitted row.
        let t2 = mgr.begin();
        let visible = mgr.scan_visible(&table, &t2);
        assert_eq!(visible.len(), 0, "T2 must not see T1's uncommitted insert");

        // T1 commits.
        mgr.commit(t1.id);

        // T3 begins and scans — must see T1's committed row.
        let t3 = mgr.begin();
        let visible = mgr.scan_visible(&table, &t3);
        assert_eq!(visible.len(), 1, "T3 must see T1's committed row");
        assert_eq!(visible[0].values, vec![42]);
    }

    /// Task 4.3 DoD: a transaction sees its own writes.
    #[test]
    fn mvcc_visibility_own_writes() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        let t1 = mgr.begin();
        table.insert(t1.id, vec![99]);

        // T1 sees its own insert.
        let visible = mgr.scan_visible(&table, &t1);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].values, vec![99]);
    }

    /// Task 4.3 DoD: snapshot isolation — a transaction doesn't see
    /// changes committed AFTER it began.
    #[test]
    fn mvcc_snapshot_isolation() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        // T1 inserts and commits.
        let t1 = mgr.begin();
        table.insert(t1.id, vec![1]);
        mgr.commit(t1.id);

        // T2 begins (sees T1's row).
        let t2 = mgr.begin();

        // T3 inserts and commits (AFTER T2 began).
        let t3 = mgr.begin();
        table.insert(t3.id, vec![2]);
        mgr.commit(t3.id);

        // T2 scans — sees T1's row but NOT T3's row (snapshot isolation).
        let visible = mgr.scan_visible(&table, &t2);
        assert_eq!(visible.len(), 1, "T2 must see only T1's row (snapshot)");
        assert_eq!(visible[0].values, vec![1]);

        // T4 begins after T3 commits — sees both rows.
        let t4 = mgr.begin();
        let visible = mgr.scan_visible(&table, &t4);
        assert_eq!(visible.len(), 2, "T4 must see both rows");
    }

    /// Task 4.3 DoD: a deleted row is invisible after the deleting txn commits.
    #[test]
    fn mvcc_visibility_deleted_row() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        // T1 inserts and commits.
        let t1 = mgr.begin();
        let row0 = table.insert(t1.id, vec![1]);
        mgr.commit(t1.id);

        // T2 deletes the row and commits.
        let t2 = mgr.begin();
        table.delete(t2.id, row0);
        mgr.commit(t2.id);

        // T3 begins — must NOT see the deleted row.
        let t3 = mgr.begin();
        let visible = mgr.scan_visible(&table, &t3);
        assert_eq!(visible.len(), 0, "T3 must not see the deleted row");

        // T2 (before commit) would not see the row either (it deleted it).
        // But after commit, T2 is no longer active. Verify via a fresh txn.
    }

    /// Task 4.3 DoD: a row deleted by an uncommitted transaction is still
    /// visible to other transactions.
    #[test]
    fn mvcc_visibility_uncommitted_delete() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        // T1 inserts and commits.
        let t1 = mgr.begin();
        let row0 = table.insert(t1.id, vec![1]);
        mgr.commit(t1.id);

        // T2 deletes the row but doesn't commit.
        let t2 = mgr.begin();
        table.delete(t2.id, row0);

        // T3 begins — must STILL see the row (T2's delete is uncommitted).
        let t3 = mgr.begin();
        let visible = mgr.scan_visible(&table, &t3);
        assert_eq!(visible.len(), 1, "T3 must see the row (T2's delete is uncommitted)");
    }

    // -----------------------------------------------------------------
    // Task 4.4: write-write conflict detection
    // -----------------------------------------------------------------

    /// Task 4.4 DoD: T1 and T2 both read row R; T1 updates R and commits;
    /// T2 tries to update R → fails with conflict.
    #[test]
    fn mvcc_write_write_conflict_committed() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into(), "v".into()]);

        // Setup: insert a row and commit.
        let t0 = mgr.begin();
        let row0 = table.insert(t0.id, vec![1, 100]);
        mgr.commit(t0.id);

        // T1 and T2 both begin (both see the row).
        let t1 = mgr.begin();
        let t2 = mgr.begin();

        // T1 updates the row and commits.
        mgr.update_with_conflict_check(&mut table, &t1, row0, vec![1, 200]).unwrap();
        mgr.commit(t1.id);

        // T2 tries to update the same row — must fail (T1 committed after T2's snapshot).
        let result = mgr.update_with_conflict_check(&mut table, &t2, row0, vec![1, 300]);
        assert!(result.is_err(), "T2 must fail with a write-write conflict");
        let err = result.unwrap_err();
        assert_eq!(err.conflicting_txn, t1.id);
    }

    /// Task 4.4 DoD: T1 is updating a row (uncommitted); T2 tries to
    /// update the same row → fails (T1 is active).
    #[test]
    fn mvcc_write_write_conflict_uncommitted() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        let t0 = mgr.begin();
        let row0 = table.insert(t0.id, vec![1]);
        mgr.commit(t0.id);

        // T1 starts updating but doesn't commit.
        let t1 = mgr.begin();
        mgr.update_with_conflict_check(&mut table, &t1, row0, vec![99]).unwrap();

        // T2 tries to update the same row — must fail (T1 is active).
        let t2 = mgr.begin();
        let result = mgr.update_with_conflict_check(&mut table, &t2, row0, vec![100]);
        assert!(result.is_err(), "T2 must fail — T1 is actively modifying the row");
    }

    /// Task 4.4 DoD: no conflict when two transactions update different rows.
    #[test]
    fn mvcc_no_conflict_different_rows() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        let t0 = mgr.begin();
        let row0 = table.insert(t0.id, vec![1]);
        let row1 = table.insert(t0.id, vec![2]);
        mgr.commit(t0.id);

        let t1 = mgr.begin();
        let t2 = mgr.begin();

        // T1 updates row0, T2 updates row1 — no conflict.
        mgr.update_with_conflict_check(&mut table, &t1, row0, vec![10]).unwrap();
        mgr.update_with_conflict_check(&mut table, &t2, row1, vec![20]).unwrap();
    }

    // -----------------------------------------------------------------
    // Task 4.5: garbage collection (VACUUM)
    // -----------------------------------------------------------------

    /// Task 4.5 DoD: insert 100 rows, update all 100, commit, vacuum →
    /// 100 old versions removed, 100 new versions remain.
    #[test]
    fn mvcc_vacuum_removes_dead_versions() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into(), "v".into()]);

        // Insert 100 rows.
        let t1 = mgr.begin();
        for i in 0..100 {
            table.insert(t1.id, vec![i as u64, i as u64 * 10]);
        }
        mgr.commit(t1.id);

        // Update all 100 rows (each row now has 2 versions: old + new).
        let t2 = mgr.begin();
        for i in 0..100 {
            table.update(t2.id, i, vec![i as u64, i as u64 * 100]);
        }
        mgr.commit(t2.id);

        // Verify each row has 2 versions before vacuum.
        let total_before: usize = (0..100).map(|i| table.row_versions(i).len()).sum();
        assert_eq!(total_before, 200, "100 rows × 2 versions each");

        // VACUUM — should remove the 100 old (dead) versions.
        let mut tables = [table];
        let removed = mgr.vacuum(&mut tables);
        assert_eq!(removed, 100, "100 old versions removed");
        let table = &tables[0];
        let total_after: usize = (0..100).map(|i| table.row_versions(i).len()).sum();
        assert_eq!(total_after, 100, "100 new versions remain");
    }

    /// Task 4.5 DoD: aborted transactions' versions are removed by VACUUM.
    #[test]
    fn mvcc_vacuum_removes_aborted_versions() {
        let mut mgr = MvccTxnManager::new();
        let mut table = MvccTable::new("t", vec!["id".into()]);

        // T1 inserts a row but rolls back.
        let t1 = mgr.begin();
        table.insert(t1.id, vec![42]);
        mgr.rollback(t1.id);

        // The version exists but is from an aborted txn.
        assert_eq!(table.row_count(), 1);
        assert_eq!(table.row_versions(0).len(), 1);

        // VACUUM removes the aborted version.
        let mut tables = [table];
        let removed = mgr.vacuum(&mut tables);
        assert_eq!(removed, 1, "1 aborted version removed");
        let table = &tables[0];
        assert_eq!(table.row_versions(0).len(), 0);
    }
}
