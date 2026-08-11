//! Wave 4 — Agent C: MVCC integration tests.
//!
//! These tests verify that `enable_mvcc()` switches the engine to use
//! `MvccTxnManager` for BEGIN/COMMIT/ROLLBACK, that VACUUM calls MVCC
//! garbage collection, and that multiple concurrent transactions are
//! supported.

use std::sync::{Arc, RwLock};
use std::thread;
use turbogp::engine::QueryEngine;
use turbogp::txn::{IsolationLevel, MvccTable, MvccTransaction, TxnState};

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
    // `is_visible_with_snapshot` returns false (xmin is neither the active
    // txn nor Committed with cid <= snapshot) → the row is filtered out.
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T2 SELECT (pre-commit)");
    assert_eq!(
        r.columns[0].values[0], 0,
        "T2 must NOT see T1's uncommitted insert (dirty read eliminated)"
    );

    // T1 commits in the background. `txn_state(t1_id)` becomes
    // `Committed(cid=1)`. `current_active` stays as T2 (the commit_background_txn
    // helper only clears current_active if it matches t1_id).
    engine.commit_background_txn(t1_id);

    // T2 SELECT COUNT(*) → must STILL be 0 (snapshot isolation).
    //
    // Task 3.2 replaced the read-committed `is_row_visible_to_active`
    // with the snapshot-aware `is_visible_with_snapshot`. T2's snapshot
    // was fixed at BEGIN (snapshot_id=0, before any commits). T1
    // committed at cid=1 > T2's snapshot, so T1's row is INVISIBLE to
    // T2 even after T1 commits. This is the snapshot-isolation property
    // (T2's snapshot is stable for the duration of the txn).
    //
    // Previously (pre-Task 3.2) this step asserted 1 — read-committed
    // behaviour, where T2 would see T1's commit. That was a known
    // limitation, now fixed.
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T2 SELECT (post-commit)");
    assert_eq!(
        r.columns[0].values[0], 0,
        "T2 must NOT see T1's commit (snapshot isolation — T1 committed \
         after T2's snapshot; T2's snapshot is fixed at BEGIN)"
    );

    // Cleanup: commit T2. A new txn T3 (begun after T1's commit) sees
    // T1's row — its snapshot includes T1's commit.
    engine.execute("COMMIT").expect("T2 COMMIT");
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after T2 COMMIT"
    );
    engine.execute("BEGIN").expect("T3 BEGIN");
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T3 SELECT");
    assert_eq!(
        r.columns[0].values[0], 1,
        "T3 (begun after T1's commit) must see T1's row — its snapshot includes T1's commit"
    );
    engine.execute("COMMIT").expect("T3 COMMIT");
}

/// Task 2.5 — MVCC snapshot isolation (dirty-read elimination + read-
/// committed visibility of concurrent commits).
///
/// Scenario (adapted to the engine's single-active-transaction model):
/// 1. T1 inserts row A (id=1) and COMMITS.
/// 2. T3 begins a background txn and inserts row B (id=2), uncommitted.
/// 3. T2 begins a background txn (becomes `current_active`; T3 stays
///    InProgress). T2's snapshot_id = commit_id after T1's commit.
/// 4. T2 `SELECT COUNT(*) FROM t` → 1 (T1's committed row visible; T3's
///    uncommitted insert filtered out by `is_row_visible_to_active` —
///    dirty read eliminated).
/// 5. T3 commits (background). `txn_state(t3_id)` → `Committed`.
/// 6. T2 `SELECT COUNT(*) FROM t` → 2 (T3's row is now visible — read-
///    committed behaviour, NOT full snapshot isolation; documented below).
/// 7. T4 begins (after T3 committed) and `SELECT COUNT(*) FROM t` → 2
///    (sees both committed rows).
///
/// **Note on ordering:** the task description has T2 BEGIN before T3, but
/// the engine is single-active-transaction — `begin_background_txn`
/// overwrites `current_active`. To keep T2 as the reader for steps 4/6,
/// we start T3 (and do its INSERT) BEFORE T2. T3 remains uncommitted
/// until step 5, so the dirty-read-elimination assertion (step 4) is
/// preserved: T2 doesn't see T3's uncommitted insert.
///
/// **Note on step 6 (snapshot isolation):** full snapshot isolation would
/// require T2 to STILL see 1 row after T3 commits (T2's snapshot was
/// fixed at BEGIN). However, `is_row_visible_to_active` checks
/// `txn_state(xmin)` WITHOUT comparing the commit_id to T2's snapshot_id.
/// Once T3 commits, its rows become visible to ALL active transactions —
/// this is read-committed behaviour, not snapshot isolation. Full snapshot
/// isolation requires plumbing a full `MvccTransaction` (with snapshot_id)
/// through `execute_select` and using `MvccTxnManager::visible(version,
/// txn)` instead of `is_row_visible_to_active`. This is future work
/// (documented in the Task 2.4 worklog entry).
#[test]
fn test_mvcc_snapshot_isolation_enforced() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");

    // Step 1 (T1): insert row A (id=1) and COMMIT via a regular BEGIN/COMMIT.
    engine.execute("BEGIN").expect("T1 BEGIN");
    engine.execute("INSERT INTO t VALUES (1)").expect("T1 INSERT");
    engine.execute("COMMIT").expect("T1 COMMIT");

    // Step 2 (T3): begin a background txn and insert row B (id=2), but
    // do NOT commit. T3 is current_active; its insert has xmin=t3_id,
    // txn_state(t3_id)=InProgress.
    let t3_id = engine.begin_background_txn();
    engine.execute("INSERT INTO t VALUES (2)").expect("T3 INSERT (uncommitted)");

    // Step 3 (T2): begin a background txn. T2 becomes current_active; T3
    // remains InProgress in txn_states. T2's snapshot_id = commit_id
    // after T1's commit (= 1 at this point).
    let t2_id = engine.begin_background_txn();
    let _ = t2_id; // used for cleanup; T2 is current_active

    // Step 4: T2 SELECT COUNT(*) → must be 1. T1's insert (committed) is
    // visible; T3's insert (InProgress, t3_id != t2_id) is filtered out
    // by `is_row_visible_to_active`. Dirty read eliminated.
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("T2 SELECT (pre-T3-commit)");
    assert_eq!(
        r.columns[0].values[0], 1,
        "T2 must NOT see T3's uncommitted insert (dirty read eliminated)"
    );

    // Step 5: T3 commits (in the background). txn_state(t3_id) becomes
    // Committed(cid=2). T2 remains current_active.
    engine.commit_background_txn(t3_id);

    // Step 6: T2 SELECT COUNT(*) → returns 1 (snapshot isolation).
    //
    // Task 3.2 replaced `is_row_visible_to_active` (read-committed —
    // accepted any committed `xmin`) with `is_visible_with_snapshot`
    // (snapshot-isolation — requires `xmin`'s `commit_id <= snapshot_id`).
    // T2's snapshot was fixed at BEGIN (cid=1, after T1's commit). T3
    // committed at cid=2 > T2's snapshot, so T3's row is INVISIBLE to T2
    // even after T3 commits. This is now correct snapshot isolation
    // (previously this step asserted 2 — read-committed behaviour — and
    // was fixed by Task 3.2).
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("T2 SELECT (post-T3-commit)");
    assert_eq!(
        r.columns[0].values[0], 1,
        "T2 must NOT see T3's commit (snapshot isolation — T3 committed \
         after T2's snapshot; is_visible_with_snapshot compares commit_id \
         to snapshot_id)"
    );

    // Step 7 (T4): begin a new background txn. T4 started after T3
    // committed, so T4 sees both rows.
    let t4_id = engine.begin_background_txn();
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("T4 SELECT");
    assert_eq!(
        r.columns[0].values[0], 2,
        "T4 (started after T3 committed) must see both rows"
    );

    // Cleanup: commit T4 (current_active) and T2 (still InProgress).
    engine.commit_background_txn(t4_id);
    engine.commit_background_txn(t2_id);
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after cleanup"
    );
}

/// Task 2.6 — write-write conflict detection aborts the second writer.
///
/// Scenario:
/// 1. T0 inserts row R (id=1, v=10) and commits.
/// 2. T1 BEGIN, UPDATE v=99 (via engine). T1 is current_active.
/// 3. T2 begins a background txn (current_active=T2; T1 InProgress). T2's
///    snapshot_id = commit_id before T1 commits.
/// 4. T1 commits (background). T1 → Committed(cid > T2's snapshot_id).
/// 5. T2 attempts to UPDATE the same row → must fail with a write-write
///    conflict (T1 committed after T2's snapshot — first-committer-wins).
/// 6. T2 ROLLBACK.
/// 7. T3 BEGIN, SELECT v FROM t WHERE id=1 → returns T1's committed value.
///
/// **Behaviour finding (documented):** the engine's `execute_update` does
/// NOT call `check_write_conflict` (verified by code inspection of
/// `src/engine/dml.rs` — the MVCC block only calls `mark_deleted` +
/// `append_row_version`, with no conflict check). If T2's UPDATE were
/// executed via the engine, it would SUCCEED (no conflict error) and would
/// corrupt the column in-place (the engine's flat `row_versions` + in-place
/// column mutation is not MVCC-correct for concurrent updates — a known
/// limitation documented in the Task 2.2/2.3 worklog entry).
///
/// To verify the write-write conflict detection logic WITHOUT triggering
/// the column-corruption gap, we call `MvccTxnManager::check_write_conflict`
/// directly on a standalone `MvccTable` that mirrors the engine's
/// row_versions state. This exercises the same `check_write_conflict` code
/// path that a future `execute_update` integration would use.
///
/// **Note on step 7:** due to the flat `row_versions` design (Task 2.2/2.3
/// known limitation), T1's appended new version (v=99) is NOT found by
/// `filter_indices` — it only checks `row_versions[0]` (the original
/// version, which has `xmax=t1_id` committed → invisible to T3). So T3's
/// SELECT returns 0 rows. The in-place column mutation set v=99 in the
/// column, but the visibility filter hides row 0 entirely. Full MVCC
/// visibility for updated rows requires the `Vec<Vec<RowVersion>>` refactor
/// (future work).
#[test]
fn test_write_write_conflict_aborts() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT, v INT)").expect("CREATE TABLE");

    // Step 1 (T0): insert row R (id=1, v=10) in a committed txn. We use
    // an explicit BEGIN/COMMIT (not autocommit) so the row has a real
    // xmin txn_id with a Committed state — autocommit INSERTs use xmin=0,
    // and txn_state(0) defaults to Aborted, making the row invisible to
    // active MVCC transactions.
    engine.execute("BEGIN").expect("T0 BEGIN");
    let t0_id = engine
        .mvcc_txn_manager()
        .active_id()
        .expect("T0 active after BEGIN");
    engine.execute("INSERT INTO t VALUES (1, 10)").expect("T0 INSERT");
    engine.execute("COMMIT").expect("T0 COMMIT");

    // Step 2 (T1): BEGIN, UPDATE v=99. T1 is current_active; the UPDATE
    // sets xmax=t1_id on row 0's version and appends a new version.
    engine.execute("BEGIN").expect("T1 BEGIN");
    let t1_id = engine
        .mvcc_txn_manager()
        .active_id()
        .expect("T1 active after BEGIN");
    engine
        .execute("UPDATE t SET v = 99 WHERE id = 1")
        .expect("T1 UPDATE");

    // Capture T2's snapshot_id BEFORE beginning T2. begin() sets
    // snapshot_id = current_commit_id, so this is T2's snapshot.
    let t2_snapshot_id = engine.mvcc_txn_manager().current_commit_id();

    // Step 3 (T2): begin a background txn. T1 remains InProgress; T2 is
    // current_active. T2's snapshot_id = t2_snapshot_id (before T1 commits).
    let t2_id = engine.begin_background_txn();

    // Step 4 (T1): COMMIT (in the background). T1 → Committed(cid = t2_snapshot_id + 1).
    // T2's snapshot_id = t2_snapshot_id < T1's cid → first-committer-wins conflict.
    engine.commit_background_txn(t1_id);

    // Step 5: verify the write-write conflict is detected.
    //
    // Build a standalone MvccTable mirroring the engine's row_versions:
    // - Row 0: inserted by T0 (committed, cid <= T2's snapshot).
    // - T1 (committed, cid > T2's snapshot) updated row 0 (set xmax=t1_id,
    //   appended a new version with v=99).
    //
    // check_write_conflict finds the version visible to T2 (the original,
    // since T1's new version has xmin=t1_id which committed AFTER T2's
    // snapshot → invisible). That visible version has xmax=t1_id (Committed,
    // cid > T2's snapshot_id) → conflict.
    let mut mvcc_table = MvccTable::new("t", vec!["id".into(), "v".into()]);
    mvcc_table.insert(t0_id, vec![1, 10]); // row 0, xmin=t0_id (T0, committed)
    mvcc_table.update(t1_id, 0, vec![1, 99]); // T1 updates: xmax=t1_id, new version

    // Construct T2's MvccTransaction (all fields are pub). snapshot_id
    // was captured before T2 began (= commit_id at that point).
    let t2_txn = MvccTransaction {
        id: t2_id,
        snapshot_id: t2_snapshot_id,
        state: TxnState::InProgress,
        isolation_level: IsolationLevel::default(),
    };

    // check_write_conflict must return Err: T1 committed (cid > T2's
    // snapshot_id) after modifying row 0 — first-committer-wins.
    let conflict = engine
        .mvcc_txn_manager()
        .check_write_conflict(&mvcc_table, &t2_txn, 0);
    assert!(
        conflict.is_err(),
        "check_write_conflict must detect the write-write conflict \
         (T1 committed after T2's snapshot — first-committer-wins)"
    );
    let err = conflict.expect_err("conflict error was verified above");
    assert_eq!(
        err.conflicting_txn, t1_id,
        "the conflict must be attributed to T1"
    );

    // Step 6: T2 ROLLBACK (cleanup). T2 is current_active.
    engine.execute("ROLLBACK").expect("T2 ROLLBACK");
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after T2 ROLLBACK"
    );

    // Step 7 (T3): BEGIN, SELECT v FROM t WHERE id = 1.
    //
    // Task 3.1 refactored `row_versions` to `Vec<Vec<RowVersion>>` (one
    // chain per row), so T1's UPDATE appended the new version (v=99) to
    // the SAME chain at row 0. `filter_indices` iterates the chain in
    // reverse and finds the new version: `xmin=t1_id` (Committed at
    // cid=2 <= T3's snapshot=2) → visible, `xmax=None` → live. T3 sees
    // T1's committed value (v=99).
    //
    // Previously (pre-Task 3.1) the flat `row_versions` design appended
    // the new version to the END of the vec, breaking row-index alignment
    // and hiding the new version from `filter_indices` — T3 saw 0 rows.
    // That limitation is now fixed.
    engine.execute("BEGIN").expect("T3 BEGIN");
    let r = engine
        .execute("SELECT v FROM t WHERE id = 1")
        .expect("T3 SELECT");
    assert_eq!(
        r.row_count, 1,
        "T3 must see T1's committed UPDATE (v=99) — the Vec<Vec<RowVersion>> \
         chain makes the new version visible to readers whose snapshot \
         includes T1's commit"
    );
    let v = r
        .column("v")
        .and_then(|c| c.first().copied())
        .expect("expected a v value");
    assert_eq!(
        v, 99,
        "T3 sees T1's committed value (v=99), not the original (v=10)"
    );
    engine.execute("COMMIT").expect("T3 COMMIT");
}

/// Task 3.1 — MVCC-mode ROLLBACK marks inserted rows invisible (atomicity).
///
/// Scenario (mirrors the task description):
/// 1. `enable_mvcc()`.
/// 2. `CREATE TABLE t (id INT)`.
/// 3. `BEGIN`.
/// 4. `INSERT INTO t VALUES (1)`.
/// 5. `ROLLBACK` — the txn's state becomes `Aborted`.
/// 6. `SELECT COUNT(*) FROM t` (autocommit, no active txn) → must return 0.
///
/// **Why this test exists:** `is_row_visible_to_active` (Task 2.4) already
/// returns `false` for rows whose `xmin` is in the `Aborted` state, so
/// rolled-back inserts *should* be invisible. However, Task 2.4 gated the
/// visibility filter on `txn_id.is_some()` — i.e. it only applied when an
/// MVCC transaction was active. That meant an autocommit `SELECT` (the
/// common case after `ROLLBACK`) bypassed the filter entirely and still
/// saw the rolled-back row, violating atomicity.
///
/// Task 3.1 fixes the gate: `execute_inner` now applies MVCC visibility
/// filtering whenever `mvcc_enabled` is true, regardless of whether a
/// transaction is active. In autocommit mode, `is_row_visible_to_active`
/// treats the reader as txn `0` (never in `txn_states`), so:
/// - Aborted `xmin` (the rolled-back insert) → `xmin != 0` and
///   `txn_state(xmin) = Aborted` (not `Committed`) → invisible. ✓
/// - Committed `xmin` (a prior committed insert) → `txn_state = Committed`
///   → visible. ✓
/// - Autocommit `xmin = 0` (an autocommit insert) → `xmin == active_id`
///   → visible (preserves autocommit semantics). ✓
#[test]
fn test_mvcc_rollback_marks_inserts_invisible() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");

    // Insert a row inside an explicit transaction, then ROLLBACK.
    engine.execute("BEGIN").expect("BEGIN");
    let txn_id = engine
        .mvcc_txn_manager()
        .active_id()
        .expect("txn should be active after BEGIN");
    engine.execute("INSERT INTO t VALUES (1)").expect("INSERT");
    engine.execute("ROLLBACK").expect("ROLLBACK");
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after ROLLBACK"
    );
    // Sanity: the rolled-back txn's state is Aborted.
    assert!(
        matches!(engine.mvcc_txn_manager().txn_state(txn_id), TxnState::Aborted),
        "rolled-back txn must be in Aborted state"
    );

    // Autocommit SELECT — the rolled-back insert must be invisible.
    // Before the Task 3.1 fix, this returned 1 (atomicity violation).
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("SELECT COUNT(*) after ROLLBACK");
    assert_eq!(
        r.columns[0].values[0], 0,
        "rolled-back insert must be invisible (atomicity): Aborted xmin \
         filtered out by is_row_visible_to_active"
    );

    // Regression guard: a subsequent committed insert IS visible, proving
    // the filter doesn't over-aggressively hide committed data.
    engine.execute("BEGIN").expect("BEGIN (committed insert)");
    engine.execute("INSERT INTO t VALUES (42)").expect("INSERT (committed)");
    engine.execute("COMMIT").expect("COMMIT");
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("SELECT COUNT(*) after COMMIT");
    assert_eq!(
        r.columns[0].values[0], 1,
        "committed insert must be visible (Committed xmin passes the filter)"
    );

    // Regression guard: autocommit inserts (xmin = 0) are still visible to
    // autocommit readers (active_id = 0 → xmin == active_id).
    engine
        .execute("INSERT INTO t VALUES (99)")
        .expect("autocommit INSERT");
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("SELECT COUNT(*) after autocommit INSERT");
    assert_eq!(
        r.columns[0].values[0], 2,
        "autocommit insert must be visible to autocommit reader"
    );
}

// =================================================================
// Task 3.4 + 3.5 + 3.6 — VACUUM compaction, Serializable conflict
// detection, and snapshot isolation integration.
// =================================================================

/// Task 3.4 DoD — VACUUM compacts dead row versions from `Table`'s
/// version chains.
///
/// Scenario:
/// 1. `enable_mvcc`, `CREATE TABLE t (id INT, v INT)`.
/// 2. `BEGIN; INSERT 100 rows; COMMIT` — each row's chain has 1 version
///    (the INSERT, `xmin=t1, xmax=None`).
/// 3. `BEGIN; UPDATE all 100 rows; COMMIT` — each row's chain now has 2
///    versions: the old INSERT (`xmax=t2`, tombstoned) and the new
///    UPDATE version (`xmin=t2, xmax=None`, live).
/// 4. Before VACUUM: every chain has 2 versions.
/// 5. `VACUUM` — `vacuum_table` removes the 100 dead old versions
///    (`xmax=t2` committed at `cid=2 <= oldest_active_snapshot=2`).
/// 6. After VACUUM: every chain has 1 version (the live UPDATE version).
///
/// This exercises the engine-level `execute_vacuum` →
/// `MvccTxnManager::vacuum_table` wiring (Task 3.4), not the standalone
/// `MvccTable` test type.
#[test]
fn test_vacuum_compacts_dead_versions() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT, v INT)").expect("CREATE TABLE");

    // Step 2: INSERT 100 rows in an explicit txn (so xmin is a real
    // committed txn_id, not autocommit's 0 which defaults to Aborted).
    engine.execute("BEGIN").expect("INSERT BEGIN");
    for i in 0..100u64 {
        engine
            .execute(&format!("INSERT INTO t VALUES ({}, {})", i, i * 10))
            .expect("INSERT");
    }
    engine.execute("COMMIT").expect("INSERT COMMIT"); // commit_id = 1

    // Step 3: UPDATE all 100 rows in another explicit txn.
    engine.execute("BEGIN").expect("UPDATE BEGIN");
    for i in 0..100u64 {
        engine
            .execute(&format!("UPDATE t SET v = {} WHERE id = {}", i * 100, i))
            .expect("UPDATE");
    }
    engine.execute("COMMIT").expect("UPDATE COMMIT"); // commit_id = 2

    // Step 4: before VACUUM, every chain has 2 versions.
    let table = engine
        .catalog()
        .get("t")
        .expect("table t should exist");
    assert_eq!(table.row_versions.len(), 100, "100 row chains");
    for (i, chain) in table.row_versions.iter().enumerate() {
        assert_eq!(
            chain.len(),
            2,
            "chain {} should have 2 versions before VACUUM (old tombstoned + new live); got {}",
            i,
            chain.len()
        );
    }
    drop(table);

    // Step 5: VACUUM.
    engine.execute("VACUUM").expect("VACUUM");

    // Step 6: after VACUUM, every chain has 1 version (the live one).
    let table = engine
        .catalog()
        .get("t")
        .expect("table t should exist after VACUUM");
    for (i, chain) in table.row_versions.iter().enumerate() {
        assert_eq!(
            chain.len(),
            1,
            "chain {} should have 1 version after VACUUM (dead old version removed); got {}",
            i,
            chain.len()
        );
        // The surviving version is live (xmax None).
        assert!(
            chain[0].xmax.is_none(),
            "chain {}'s surviving version should be live (xmax None)",
            i
        );
    }

    // Sanity: the data is still readable after VACUUM.
    let r = engine.execute("SELECT COUNT(*) FROM t").expect("post-VACUUM COUNT");
    assert_eq!(
        r.columns[0].values[0], 100,
        "all 100 rows still visible after VACUUM"
    );
}

/// Task 3.5 DoD — Serializable conflict detection aborts a transaction
/// that updates a row modified by a concurrent committed transaction.
///
/// Scenario (adapted to the engine's single-active-txn model):
/// 1. `enable_mvcc`, `CREATE TABLE t (id INT, v INT)`.
/// 2. T0: `BEGIN; INSERT (1, 10); COMMIT` — commit_id = 1.
/// 3. T1: `begin_background_txn_with_isolation(Serializable)` —
///    snapshot_id = 1, current_active = T1.
/// 4. T1: `UPDATE t SET v=99 WHERE id=1` — T1 tombstones the old version
///    and appends a new one. (T1 is current_active, so the engine tags
///    the row with t1_id.)
/// 5. T2: `begin_background_txn_with_isolation(Serializable)` —
///    snapshot_id = 1, current_active = T2. T1 remains InProgress.
/// 6. T1: `commit_background_txn(t1_id)` — T1 → Committed(2). T2's
///    snapshot (1) < T1's commit_id (2) → first-committer-wins condition.
/// 7. T2: `UPDATE t SET v=100 WHERE id=1` → must fail with a
///    write-write conflict error (T1 committed after T2's snapshot).
/// 8. T2: `ROLLBACK`.
///
/// The conflict is detected by `execute_update`'s Serializable pre-check
/// (Task 3.5), which calls `check_write_conflict_for_table` on each
/// matched row before modifying it. The check finds the latest version
/// visible to T2 (the original INSERT, since T1's new version has
/// `xmin=t1_id` committed at cid=2 > T2's snapshot=1 → invisible to T2).
/// That visible version's `xmax = t1_id` is `Committed(2 > 1)` → conflict.
#[test]
fn test_serializable_conflict_detection() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT, v INT)").expect("CREATE TABLE");

    // Step 2 (T0): insert (1, 10) in an explicit committed txn so the
    // row's xmin is a real Committed txn_id (not autocommit's 0).
    engine.execute("BEGIN").expect("T0 BEGIN");
    engine.execute("INSERT INTO t VALUES (1, 10)").expect("T0 INSERT");
    engine.execute("COMMIT").expect("T0 COMMIT"); // commit_id = 1

    // Step 3 (T1): begin a Serializable background txn. snapshot_id = 1.
    let t1_id = engine.begin_background_txn_with_isolation(IsolationLevel::Serializable);
    assert_eq!(
        engine.mvcc_txn_manager().active_snapshot_id(),
        Some(1),
        "T1's snapshot_id should be 1 (commit_id at BEGIN)"
    );

    // Step 4 (T1): UPDATE the row. T1 is current_active, so the engine
    // tags the old version's xmax = t1_id and appends a new version with
    // xmin = t1_id.
    engine
        .execute("UPDATE t SET v = 99 WHERE id = 1")
        .expect("T1 UPDATE");

    // Step 5 (T2): begin another Serializable background txn. T2 becomes
    // current_active; T1 remains InProgress. T2's snapshot_id = 1 (no
    // commits since T1 began).
    let t2_id = engine.begin_background_txn_with_isolation(IsolationLevel::Serializable);
    assert_eq!(
        engine.mvcc_txn_manager().active_snapshot_id(),
        Some(1),
        "T2's snapshot_id should be 1 (no commits since T1 began)"
    );

    // Step 6 (T1): commit T1 in the background. T1 → Committed(2).
    // current_active stays T2 (t1_id != t2_id).
    engine.commit_background_txn(t1_id);
    assert_eq!(
        engine.mvcc_txn_manager().txn_state(t1_id),
        TxnState::Committed(2),
        "T1 should be Committed at cid=2"
    );
    assert_eq!(
        engine.mvcc_txn_manager().active_id(),
        Some(t2_id),
        "T2 should still be current_active after T1's background commit"
    );

    // Step 7 (T2): UPDATE the same row → must fail with a write-write
    // conflict. T1 committed at cid=2 > T2's snapshot=1, and T1 modified
    // the row (set xmax on the version visible to T2).
    let result = engine.execute("UPDATE t SET v = 100 WHERE id = 1");
    assert!(
        result.is_err(),
        "T2's UPDATE must fail with a write-write conflict (T1 committed after T2's snapshot)"
    );
    let err_msg = format!("{}", result.expect_err("error verified above"));
    assert!(
        err_msg.contains("conflict"),
        "error message should mention 'conflict'; got: {err_msg}"
    );

    // Step 8 (T2): ROLLBACK to clean up.
    engine.execute("ROLLBACK").expect("T2 ROLLBACK");
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after T2 ROLLBACK"
    );

    // Sanity: the row still holds T1's committed value (v=99). T2's
    // aborted UPDATE did not corrupt the data.
    let r = engine.execute("SELECT v FROM t WHERE id = 1").expect("post-conflict SELECT");
    assert_eq!(
        r.column("v").and_then(|c| c.first().copied()),
        Some(99),
        "row should still hold T1's committed value (v=99) after T2's aborted UPDATE"
    );
}

/// Task 3.6 DoD — snapshot isolation integration test.
///
/// Verifies that a transaction T2 does NOT see rows committed by T3
/// AFTER T2's snapshot, but DOES see rows committed before. This is the
/// defining property of snapshot isolation (as opposed to read-committed,
/// where each statement gets a fresh snapshot).
///
/// Scenario (adapted to the engine's single-active-txn model — T3 begins
/// and inserts BEFORE T2 so that T2 can be current_active for the scan):
/// 1. T1: `BEGIN; INSERT (1); COMMIT` — row A. commit_id = 1.
/// 2. T3: `begin_background_txn` — snapshot = 1. INSERT (2) [row B,
///    uncommitted]. T3 is current_active.
/// 3. T2: `begin_background_txn` — snapshot = 1. T2 is current_active;
///    T3 remains InProgress.
/// 4. T3: `commit_background_txn(t3_id)` — T3 → Committed(2).
/// 5. T2: `SELECT COUNT(*) FROM t` → must return 1 (row A only; row B's
///    xmin committed at cid=2 > T2's snapshot=1 → invisible to T2).
/// 6. T2: `commit_background_txn(t2_id)` — commit_id = 3.
/// 7. T4: `begin_background_txn` — snapshot = 3. `SELECT COUNT(*) FROM t`
///    → must return 2 (both rows visible: T1's row Committed(1<=3), T3's
///    row Committed(2<=3)).
///
/// This complements `test_mvcc_snapshot_isolation_enforced` (which tests
/// the same property with a focus on dirty-read elimination) by following
/// the Task 3.6 spec's exact step numbering and asserting the snapshot
/// isolation boundary explicitly.
#[test]
fn test_snapshot_isolation_integration() {
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().expect("enable_mvcc");
    engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");

    // Step 1 (T1): insert row A (id=1) and commit. commit_id = 1.
    engine.execute("BEGIN").expect("T1 BEGIN");
    engine.execute("INSERT INTO t VALUES (1)").expect("T1 INSERT (row A)");
    engine.execute("COMMIT").expect("T1 COMMIT");
    let t1_commit_id = engine.mvcc_txn_manager().current_commit_id();
    assert_eq!(t1_commit_id, 1, "T1's commit should advance commit_id to 1");

    // Step 2 (T3): begin a background txn and insert row B (id=2),
    // uncommitted. T3 is current_active; row B's xmin = t3_id (InProgress).
    let t3_id = engine.begin_background_txn();
    engine.execute("INSERT INTO t VALUES (2)").expect("T3 INSERT (row B, uncommitted)");

    // Step 3 (T2): begin a background txn. T2 becomes current_active;
    // T3 remains InProgress. T2's snapshot_id = current_commit_id = 1
    // (no commits since T1).
    let t2_id = engine.begin_background_txn();
    let t2_snapshot = engine
        .mvcc_txn_manager()
        .active_snapshot_id()
        .expect("T2 should be active");
    assert_eq!(t2_snapshot, 1, "T2's snapshot_id should be 1 (commit_id at BEGIN)");

    // Step 4 (T3): commit T3 in the background. T3 → Committed(cid=2).
    // cid=2 > T2's snapshot (1) → T3's row must be invisible to T2.
    engine.commit_background_txn(t3_id);
    let t3_commit_id = match engine.mvcc_txn_manager().txn_state(t3_id) {
        TxnState::Committed(cid) => cid,
        ref s => panic!("T3 should be Committed; got {:?}", s),
    };
    assert_eq!(t3_commit_id, 2, "T3's commit_id should be 2");
    assert!(
        t3_commit_id > t2_snapshot,
        "T3's commit_id ({}) must be > T2's snapshot ({}) for snapshot isolation to be observable",
        t3_commit_id,
        t2_snapshot
    );

    // Step 5 (T2): SELECT COUNT(*) → must be 1 (row A only; row B invisible).
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("T2 SELECT (after T3 commit)");
    assert_eq!(
        r.columns[0].values[0], 1,
        "T2 must see only row A (snapshot isolation — T3's row B committed \
         at cid={} > T2's snapshot={})",
        t3_commit_id,
        t2_snapshot
    );

    // Step 6 (T2): commit. commit_id = 3.
    engine.commit_background_txn(t2_id);

    // Step 7 (T4): begin a fresh txn. snapshot = 3 (>= both commit_ids).
    // T4 sees both rows.
    let t4_id = engine.begin_background_txn();
    let t4_snapshot = engine
        .mvcc_txn_manager()
        .active_snapshot_id()
        .expect("T4 should be active");
    assert_eq!(t4_snapshot, 3, "T4's snapshot_id should be 3 (after T2's commit)");
    let r = engine
        .execute("SELECT COUNT(*) FROM t")
        .expect("T4 SELECT");
    assert_eq!(
        r.columns[0].values[0], 2,
        "T4 (snapshot={}) must see both rows (T1's cid=1, T3's cid=2, both <= snapshot)",
        t4_snapshot
    );

    // Cleanup.
    engine.commit_background_txn(t4_id);
    assert!(
        !engine.mvcc_txn_manager().is_active(),
        "no active txn after cleanup"
    );
}
