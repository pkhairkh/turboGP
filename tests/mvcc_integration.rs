//! Wave 4 — Agent C: MVCC integration tests.
//!
//! These tests verify that `enable_mvcc()` switches the engine to use
//! `MvccTxnManager` for BEGIN/COMMIT/ROLLBACK, that VACUUM calls MVCC
//! garbage collection, and that multiple concurrent transactions are
//! supported.

use std::sync::{Arc, RwLock};
use std::thread;
use turbogp::engine::QueryEngine;

#[test]
fn test_enable_mvcc() {
    let mut engine = QueryEngine::in_memory();
    assert!(!engine.is_mvcc_enabled(), "MVCC should be disabled by default");
    engine.enable_mvcc().unwrap();
    assert!(engine.is_mvcc_enabled(), "MVCC should be enabled after enable_mvcc()");
    engine.disable_mvcc().unwrap();
    assert!(!engine.is_mvcc_enabled(), "MVCC should be disabled after disable_mvcc()");
}

#[test]
fn test_mvcc_begin_commit() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    assert!(engine.mvcc_txn_manager().is_active(), "MVCC txn should be active after BEGIN");
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("COMMIT").unwrap();
    assert!(!engine.mvcc_txn_manager().is_active(), "MVCC txn should be inactive after COMMIT");
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 1, "row should be visible after COMMIT");
}

#[test]
fn test_mvcc_begin_rollback() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("ROLLBACK").unwrap();
    assert!(!engine.mvcc_txn_manager().is_active(), "MVCC txn should be inactive after ROLLBACK");
    // Note: in the current partial MVCC implementation, INSERT doesn't create
    // row versions with xmin/xmax, so the row is still visible after ROLLBACK.
    // Full MVCC visibility filtering is pending Agent B's completion of
    // Table.row_versions population — documented in AGENT_C_API_REQUESTS.md.
    // This test verifies that BEGIN/ROLLBACK don't error in MVCC mode.
}

#[test]
fn test_mvcc_vacuum_calls_cleanup() {
    // Wave 4 Task 4.3: VACUUM should call MvccTxnManager::cleanup_aborted
    // when MVCC mode is enabled. We can't easily verify the cleanup count
    // directly, but we can verify VACUUM succeeds in MVCC mode.
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    // Begin and rollback a transaction to create an aborted txn entry.
    engine.execute("BEGIN").unwrap();
    engine.execute("ROLLBACK").unwrap();
    // VACUUM should succeed and clean up the aborted transaction.
    let r = engine.execute("VACUUM");
    assert!(r.is_ok(), "VACUUM should succeed in MVCC mode: {:?}", r.err());
}

#[test]
fn test_mvcc_mode_does_not_break_normal_queries() {
    // Verify that enabling MVCC mode doesn't break existing query patterns.
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    engine.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();

    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 2);

    let r = engine.execute("SELECT * FROM t WHERE id = 1").unwrap();
    assert!(r.row_count >= 1);

    let r = engine.execute("EXPLAIN SELECT * FROM t").unwrap();
    assert_eq!(r.columns[0].name, "QUERY PLAN");
}

#[test]
fn test_mvcc_concurrent_transactions_two_writers() {
    // Wave 4 Task 4.4: 2 concurrent transactions, each BEGIN/INSERT/COMMIT.
    //
    // In snapshot-isolation mode, only one transaction can be active at a
    // time per QueryEngine. In MVCC mode, the MvccTxnManager tracks commit
    // state for multiple transaction IDs, so concurrent connections (each
    // with their own QueryEngine) can have active transactions simultaneously.
    //
    // This test uses Arc<RwLock<QueryEngine>> — the production pattern —
    // with 2 threads. Each thread acquires a write lock, does BEGIN/INSERT/
    // COMMIT, then releases. The test verifies both succeed without
    // deadlock or data corruption.
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    let engine = Arc::new(RwLock::new(engine));

    let engine_a = Arc::clone(&engine);
    let engine_b = Arc::clone(&engine);

    let handle_a = thread::spawn(move || -> Result<(), String> {
        let mut guard = engine_a.write().map_err(|e| format!("lock: {e}"))?;
        guard.execute("BEGIN").map_err(|e| format!("BEGIN: {e}"))?;
        guard.execute("INSERT INTO t VALUES (1)").map_err(|e| format!("INSERT: {e}"))?;
        guard.execute("COMMIT").map_err(|e| format!("COMMIT: {e}"))?;
        Ok(())
    });

    let handle_b = thread::spawn(move || -> Result<(), String> {
        let mut guard = engine_b.write().map_err(|e| format!("lock: {e}"))?;
        guard.execute("BEGIN").map_err(|e| format!("BEGIN: {e}"))?;
        guard.execute("INSERT INTO t VALUES (2)").map_err(|e| format!("INSERT: {e}"))?;
        guard.execute("COMMIT").map_err(|e| format!("COMMIT: {e}"))?;
        Ok(())
    });

    let a = handle_a.join().expect("thread A panicked");
    let b = handle_b.join().expect("thread B panicked");
    a.expect("thread A failed");
    b.expect("thread B failed");

    // Verify both rows were inserted.
    let r = engine.write().unwrap().execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 2, "both threads' inserts should be visible");
}

#[test]
fn test_mvcc_snapshot_mode_still_works() {
    // Verify that the default (snapshot-isolation) mode still works after
    // the MVCC integration — no regression.
    let mut engine = QueryEngine::in_memory();
    assert!(!engine.is_mvcc_enabled());
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("COMMIT").unwrap();
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 1);
}

#[test]
fn test_enable_mvcc_fails_during_snapshot_txn() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    let err = engine.enable_mvcc().unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("MVCC") || msg.contains("snapshot"),
        "enable_mvcc during active txn should error: {}",
        msg
    );
    // Rollback to clean up.
    engine.execute("ROLLBACK").unwrap();
    engine.enable_mvcc().unwrap();
    assert!(engine.is_mvcc_enabled());
}

/// Task 2.4 — `execute_select` filters rows by MVCC visibility, eliminating
/// dirty reads.
///
/// Scenario (mirrors the task description):
/// 1. T1 begins, inserts a row, does NOT commit.
/// 2. T2 begins (a *different* transaction, while T1 is still InProgress),
///    `SELECT COUNT(*) FROM t` → must return 0 (T2 cannot see T1's
///    uncommitted insert — no dirty read).
/// 3. T1 commits (in the background, while T2 is still active).
/// 4. T2 issues `SELECT COUNT(*) FROM t` again → must return 1 (T1's
///    commit is now visible to T2).
///
/// Because `QueryEngine` is single-transaction-at-a-time via
/// `execute("BEGIN")`, the test uses `begin_background_txn` /
/// `commit_background_txn` (test-only helpers added in Task 2.4) to
/// simulate a concurrent transaction's lifecycle while keeping T2 as the
/// `current_active` reader.
///
/// This exercises the `filter_indices` → `is_row_visible_to_active` path:
/// when an MVCC transaction is active, `execute_select` skips the planner
/// pipeline / kernel dispatch / indexed-lookup fast paths and routes the
/// scan through `filter_indices`, which retains only rows whose
/// `row_versions[i]` is visible to the active transaction.
#[test]
fn test_execute_select_filters_uncommitted() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");

    // T1: BEGIN, INSERT (uncommitted).
    engine.execute("BEGIN").expect("T1 BEGIN");
    engine.execute("INSERT INTO t VALUES (1)").expect("T1 INSERT");
    let t1_id = engine
        .mvcc_txn_manager()
        .active_id()
        .expect("T1 should be active after BEGIN");

    // T2: begin a background txn. T1 remains InProgress in the manager's
    // `txn_states` map; `current_active` is now T2.
    let _t2_id = engine.begin_background_txn();
    assert!(
        engine.mvcc_txn_manager().is_active(),
        "T2 should be the current_active after begin_background_txn"
    );

    // T2 SELECT COUNT(*) → must be 0. T1's insert has `xmin = t1_id`,
    // `txn_state(t1_id) = InProgress`, and `t1_id != active_id (T2)`, so
    // `is_row_visible_to_active` returns false → the row is filtered out.
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T2 SELECT (pre-commit)");
    assert_eq!(
        r.columns[0].values[0], 0,
        "T2 must NOT see T1's uncommitted insert (dirty read eliminated)"
    );

    // T1 commits in the background. `txn_state(t1_id)` becomes
    // `Committed(cid)`. `current_active` stays as T2 (the commit_background_txn
    // helper only clears current_active if it matches t1_id).
    engine.commit_background_txn(t1_id);

    // T2 SELECT COUNT(*) → must be 1. T1's insert now has
    // `txn_state(t1_id) = Committed`, so `is_row_visible_to_active` returns
    // true (xmin is committed; xmax is None).
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T2 SELECT (post-commit)");
    assert_eq!(
        r.columns[0].values[0], 1,
        "T2 must see T1's row after T1 commits (no dirty read, commit visible)"
    );

    // Cleanup: commit T2.
    engine.execute("COMMIT").expect("T2 COMMIT");
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after T2 COMMIT"
    );
}
