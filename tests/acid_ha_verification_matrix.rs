//! Task 8.3 — Final ACID + HA verification matrix.
//!
//! Single-test sweep across all 7 production-hardening properties. Runs
//! every check, prints a PASS/FAIL summary table to stderr, then panics
//! with the list of failed properties (if any) so the test fails loudly.
//!
//! ## Properties verified
//!
//! 1. **Atomicity** (MVCC): `BEGIN; INSERT; ROLLBACK; SELECT COUNT(*)` → 0
//!    (rolled-back inserts are invisible).
//! 2. **Consistency**: `CREATE TABLE t (x INT CHECK (x > 0))`; `INSERT x=0`
//!    → error (SQLSTATE 23514 — check_violation).
//! 3. **Isolation** (MVCC): T1 BEGIN+INSERT (uncommitted); T2 BEGIN+SELECT
//!    → 0 rows (no dirty read). Uses `begin_background_txn` /
//!    `commit_background_txn` test helpers to simulate the concurrent
//!    transaction lifecycle on a single engine (mirrors the approach in
//!    `tests/mvcc_integration.rs::test_execute_select_filters_uncommitted`).
//! 4. **Durability**: `with_data_dir(temp_dir)`; INSERT; CHECKPOINT; drop
//!    engine; reload via `with_data_dir`; SELECT COUNT → same count.
//! 5. **Persistence**: after CHECKPOINT, `checkpoint.bin` exists on disk
//!    (binary-checkpoint format — Task 4.1/4.2).
//! 6. **Concurrency**: 10 concurrent SELECTs via `route_and_execute` on
//!    `Arc<RwLock<QueryEngine>>` — all succeed.
//! 7. **Replication**: `enable_replication_local_only`; INSERT;
//!    `wal_records_streamed() >= 1`.
//!
//! ## Why one test, not seven
//!
//! The brief specifies a single test (`test_acid_ha_verification_matrix`)
//! that verifies all 7 properties and prints the summary table. Splitting
//! into 7 separate `#[test]` functions would defeat the "verification
//! matrix" framing — the summary is the deliverable. Each sub-check is
//! isolated (separate engine / temp dir) so a failure in one cannot
//! perturb another's state.

use std::sync::{Arc, RwLock};
use std::thread;

use tempfile::TempDir;
use turbogp::engine::{route_and_execute, QueryEngine};

/// Result of a single verification sub-check: `(passed, detail)`. The
/// property name is added by the caller when building the summary table.
type VerifyResult = (bool, String);

#[test]
fn test_acid_ha_verification_matrix() {
    // Each entry: (property_name, passed, detail). The verify_* helpers
    // return (bool, String); we destructure and prepend the property name.
    let mut results: Vec<(&'static str, bool, String)> = Vec::with_capacity(7);
    {
        let (ok, detail) = verify_atomicity();
        results.push(("Atomicity", ok, detail));
    }
    {
        let (ok, detail) = verify_consistency();
        results.push(("Consistency", ok, detail));
    }
    {
        let (ok, detail) = verify_isolation();
        results.push(("Isolation", ok, detail));
    }
    {
        let (ok, detail) = verify_durability();
        results.push(("Durability", ok, detail));
    }
    {
        let (ok, detail) = verify_persistence();
        results.push(("Persistence", ok, detail));
    }
    {
        let (ok, detail) = verify_concurrency();
        results.push(("Concurrency", ok, detail));
    }
    {
        let (ok, detail) = verify_replication();
        results.push(("Replication", ok, detail));
    }

    // ---- Print the summary table to stderr (visible via --nocapture). ----
    eprintln!();
    eprintln!("ACID+HA Verification Matrix:");
    for (name, ok, detail) in &results {
        let status = if *ok { "PASS" } else { "FAIL" };
        eprintln!("  {name:<13} {status}   {detail}");
    }
    eprintln!();

    // ---- Panic with a clear message if any property failed. ----
    let failures: Vec<&str> = results
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(name, _, _)| *name)
        .collect();
    if !failures.is_empty() {
        panic!(
            "ACID+HA verification FAILED for: {}. See the summary table above for details.",
            failures.join(", ")
        );
    }

    eprintln!("All 7 properties verified.");
}

// =========================================================================
// Property 1 — Atomicity (MVCC mode).
// =========================================================================

/// `BEGIN; INSERT; ROLLBACK; SELECT COUNT(*)` → 0 (rolled-back inserts are
/// invisible). Uses `engine.enable_mvcc()` so the MvccTxnManager tracks the
/// transaction's commit/abort state; the visibility filter
/// (`is_row_visible_to_active`) hides rows whose `xmin` txn is `Aborted`.
fn verify_atomicity() -> VerifyResult {
    let mut engine = QueryEngine::in_memory();
    if let Err(e) = engine.enable_mvcc() {
        return (false, format!("enable_mvcc failed: {e}"));
    }
    if let Err(e) = engine.execute("CREATE TABLE t (id INT)") {
        return (false, format!("CREATE TABLE failed: {e}"));
    }

    if let Err(e) = engine.execute("BEGIN") {
        return (false, format!("BEGIN failed: {e}"));
    }
    if let Err(e) = engine.execute("INSERT INTO t VALUES (1)") {
        return (false, format!("INSERT failed: {e}"));
    }
    if let Err(e) = engine.execute("ROLLBACK") {
        return (false, format!("ROLLBACK failed: {e}"));
    }

    let r = match engine.execute("SELECT COUNT(*) FROM t") {
        Ok(r) => r,
        Err(e) => return (false, format!("SELECT COUNT(*) failed: {e}")),
    };
    let count = r
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .unwrap_or(u64::MAX);
    if count == 0 {
        (
            true,
            "BEGIN; INSERT; ROLLBACK; SELECT COUNT(*) → 0 (rolled-back insert invisible)".into(),
        )
    } else {
        (
            false,
            format!("expected count=0 after ROLLBACK, got {count} (rolled-back insert visible)"),
        )
    }
}

// =========================================================================
// Property 2 — Consistency (CHECK constraint enforcement).
// =========================================================================

/// `CREATE TABLE t (x INT CHECK (x > 0))`; `INSERT x=0` → error
/// (SQLSTATE 23514). Uses `x=0` rather than `x=-1` because the DML parser
/// tokenizes `-1` as `Op("-") Int(1)`, producing a column-count mismatch
/// instead of a CHECK violation (documented in the Task 3.5 worklog entry;
/// same approach as `tests/acid.rs::test_acid_atomicity_consistency_mvcc`).
fn verify_consistency() -> VerifyResult {
    let mut engine = QueryEngine::in_memory();
    if let Err(e) = engine.execute("CREATE TABLE t (x INT CHECK (x > 0))") {
        return (false, format!("CREATE TABLE with CHECK failed: {e}"));
    }

    let bad = engine.execute("INSERT INTO t VALUES (0)");
    match bad {
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("23514") {
                (
                    true,
                    "INSERT x=0 rejected with SQLSTATE 23514 (check_violation)".into(),
                )
            } else {
                (
                    false,
                    format!("INSERT x=0 errored but missing SQLSTATE 23514: {msg}"),
                )
            }
        }
        Ok(_) => {
            // Double-check by counting: a successful bad INSERT would leave 1 row.
            let r = engine
                .execute("SELECT COUNT(*) FROM t")
                .expect("SELECT COUNT(*) after bad INSERT");
            let count = r
                .columns
                .first()
                .and_then(|c| c.values.first().copied())
                .unwrap_or(0);
            (
                false,
                format!("INSERT x=0 should have violated CHECK (x > 0) but succeeded (count={count})"),
            )
        }
    }
}

// =========================================================================
// Property 3 — Isolation (MVCC, no dirty read).
// =========================================================================

/// T1 BEGIN+INSERT (uncommitted); T2 BEGIN+SELECT → 0 rows (no dirty read).
///
/// Uses the `begin_background_txn` / `commit_background_txn` `#[doc(hidden)]`
/// test helpers (Task 2.4) to simulate the concurrent transaction lifecycle
/// on a single `QueryEngine` — `begin_background_txn` overwrites
/// `current_active` to T2 without aborting T1, leaving T1 InProgress in the
/// `txn_states` map. T2's SELECT routes through `execute_select`, whose
/// `filter_indices` step retains only rows whose `row_versions[i]` is
/// visible to T2: T1's insert has `xmin = t1_id`, `txn_state(t1_id) =
/// InProgress`, `t1_id != active_id (T2)` → invisible.
fn verify_isolation() -> VerifyResult {
    let mut engine = QueryEngine::in_memory();
    if let Err(e) = engine.enable_mvcc() {
        return (false, format!("enable_mvcc failed: {e}"));
    }
    if let Err(e) = engine.execute("CREATE TABLE t (id INT)") {
        return (false, format!("CREATE TABLE failed: {e}"));
    }

    // T1: BEGIN, INSERT (uncommitted). T1 is current_active.
    if let Err(e) = engine.execute("BEGIN") {
        return (false, format!("T1 BEGIN failed: {e}"));
    }
    if let Err(e) = engine.execute("INSERT INTO t VALUES (1)") {
        return (false, format!("T1 INSERT failed: {e}"));
    }
    let t1_id = match engine.mvcc_txn_manager().active_id() {
        Some(id) => id,
        None => return (false, "T1 should be active after BEGIN".into()),
    };

    // T2: begin a background txn. T1 remains InProgress; T2 becomes
    // current_active. T2's snapshot_id = commit_id before T1 commits.
    let _t2_id = engine.begin_background_txn();
    if !engine.mvcc_txn_manager().is_active() {
        return (false, "T2 should be active after begin_background_txn".into());
    }

    // T2 SELECT COUNT(*) → must be 0 (T1's uncommitted insert filtered out).
    let r = match engine.execute("SELECT COUNT(*) FROM t") {
        Ok(r) => r,
        Err(e) => return (false, format!("T2 SELECT failed: {e}")),
    };
    let count = r
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .unwrap_or(u64::MAX);

    // Cleanup: commit T1 in the background, then COMMIT T2 (current_active).
    engine.commit_background_txn(t1_id);
    let _ = engine.execute("COMMIT");

    if count == 0 {
        (
            true,
            "T1 uncommitted INSERT invisible to T2 (count=0, no dirty read)".into(),
        )
    } else {
        (
            false,
            format!("expected count=0 (no dirty read), got {count} (T1's uncommitted INSERT visible)"),
        )
    }
}

// =========================================================================
// Property 4 — Durability (CHECKPOINT + reload).
// =========================================================================

/// `with_data_dir(temp_dir)`; INSERT N rows; CHECKPOINT; drop engine; reload
/// via `with_data_dir`; SELECT COUNT → same N. Proves committed data survives
/// a clean restart via the binary-checkpoint persistence path.
fn verify_durability() -> VerifyResult {
    let tmp = match TempDir::new() {
        Ok(t) => t,
        Err(e) => return (false, format!("TempDir::new failed: {e}")),
    };
    let data_dir = tmp.path();

    const N_ROWS: u64 = 25;

    // Phase 1: create + insert + CHECKPOINT.
    {
        let mut engine = match QueryEngine::with_data_dir(data_dir) {
            Ok(e) => e,
            Err(e) => return (false, format!("with_data_dir (phase 1) failed: {e}")),
        };
        if let Err(e) = engine.execute("CREATE TABLE dur (id INT)") {
            return (false, format!("CREATE TABLE failed: {e}"));
        }
        for i in 0..N_ROWS {
            let sql = format!("INSERT INTO dur VALUES ({i})");
            if let Err(e) = engine.execute(&sql) {
                return (false, format!("INSERT {i} failed: {e}"));
            }
        }
        // Sanity: pre-checkpoint count.
        let r = match engine.execute("SELECT COUNT(*) FROM dur") {
            Ok(r) => r,
            Err(e) => return (false, format!("pre-CHECKPOINT COUNT failed: {e}")),
        };
        let pre: u64 = r
            .columns
            .first()
            .and_then(|c| c.values.first().copied())
            .unwrap_or(0);
        if pre != N_ROWS {
            return (false, format!("pre-CHECKPOINT count={pre}, expected {N_ROWS}"));
        }
        if let Err(e) = engine.execute("CHECKPOINT") {
            return (false, format!("CHECKPOINT failed: {e}"));
        }
        // Engine drops here — clean shutdown.
    }

    // Phase 2: reload and verify count.
    let mut reloaded = match QueryEngine::with_data_dir(data_dir) {
        Ok(e) => e,
        Err(e) => return (false, format!("with_data_dir (reload) failed: {e}")),
    };
    let r = match reloaded.execute("SELECT COUNT(*) FROM dur") {
        Ok(r) => r,
        Err(e) => return (false, format!("post-reload COUNT failed: {e}")),
    };
    let post: u64 = r
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .unwrap_or(0);

    if post == N_ROWS {
        (
            true,
            format!("CHECKPOINT + reload preserved all {N_ROWS} rows (count={post})"),
        )
    } else {
        (
            false,
            format!("post-reload count={post}, expected {N_ROWS} (durability lost)"),
        )
    }
}

// =========================================================================
// Property 5 — Persistence (binary checkpoint file on disk).
// =========================================================================

/// After CHECKPOINT, `data_dir/checkpoint.bin` exists on disk. This is the
/// bincode-serialized catalog format introduced in Task 4.1/4.2 (the fast
/// restart path — ~10× faster than re-executing `checkpoint.sql`).
fn verify_persistence() -> VerifyResult {
    let tmp = match TempDir::new() {
        Ok(t) => t,
        Err(e) => return (false, format!("TempDir::new failed: {e}")),
    };
    let data_dir = tmp.path();

    {
        let mut engine = match QueryEngine::with_data_dir(data_dir) {
            Ok(e) => e,
            Err(e) => return (false, format!("with_data_dir failed: {e}")),
        };
        if let Err(e) = engine.execute("CREATE TABLE persist (id INT)") {
            return (false, format!("CREATE TABLE failed: {e}"));
        }
        if let Err(e) = engine.execute("INSERT INTO persist VALUES (7)") {
            return (false, format!("INSERT failed: {e}"));
        }
        if let Err(e) = engine.execute("CHECKPOINT") {
            return (false, format!("CHECKPOINT failed: {e}"));
        }
        // Engine drops here.
    }

    let bin_path = data_dir.join("checkpoint.bin");
    if bin_path.exists() {
        // Verify the file is non-empty (binary format must be written, not
        // just touched). A 0-byte file would indicate a serialization bug.
        match std::fs::metadata(&bin_path) {
            Ok(meta) => {
                let len = meta.len();
                if len > 0 {
                    (
                        true,
                        format!("checkpoint.bin exists on disk ({} bytes, binary format)", len),
                    )
                } else {
                    (false, "checkpoint.bin exists but is 0 bytes".into())
                }
            }
            Err(e) => (false, format!("checkpoint.bin stat failed: {e}")),
        }
    } else {
        (
            false,
            "checkpoint.bin does not exist after CHECKPOINT (binary format not persisted)".into(),
        )
    }
}

// =========================================================================
// Property 6 — Concurrency (10 concurrent SELECTs via route_and_execute).
// =========================================================================

/// 10 concurrent SELECTs via `route_and_execute` on
/// `Arc<RwLock<QueryEngine>>` — all succeed. `route_and_execute` takes a
/// shared read lock for SELECT (multiple readers can hold the lock
/// concurrently), so 10 threads each running SELECT COUNT(*) should all
/// return Ok and report the same count.
fn verify_concurrency() -> VerifyResult {
    const N_READERS: usize = 10;
    const N_ROWS: u64 = 50;

    // Build the engine with a small table — concurrency is the property
    // under test, not throughput, so a 50-row table is sufficient and
    // keeps the test fast (< 100 ms typically).
    let mut engine = QueryEngine::in_memory();
    if let Err(e) = engine.execute("CREATE TABLE conc (id INT)") {
        return (false, format!("CREATE TABLE failed: {e}"));
    }
    for i in 0..N_ROWS {
        let sql = format!("INSERT INTO conc VALUES ({i})");
        if let Err(e) = engine.execute(&sql) {
            return (false, format!("INSERT {i} failed: {e}"));
        }
    }

    // Wrap in Arc<RwLock<QueryEngine>> — the production pattern.
    let engine: Arc<RwLock<QueryEngine>> = Arc::new(RwLock::new(engine));
    let sql = "SELECT COUNT(*) FROM conc";

    // Spawn N_READERS threads, each running route_and_execute.
    let mut handles = Vec::with_capacity(N_READERS);
    for reader_id in 0..N_READERS {
        let e = Arc::clone(&engine);
        handles.push(thread::spawn(move || -> (usize, Result<u64, String>) {
            match route_and_execute(&e, sql) {
                Ok(r) => {
                    let count = r
                        .columns
                        .first()
                        .and_then(|c| c.values.first().copied())
                        .unwrap_or(u64::MAX);
                    (reader_id, Ok(count))
                }
                Err(e) => (reader_id, Err(format!("{e:?}"))),
            }
        }));
    }

    // Join all threads — a deadlock or panic would hang / fail here.
    let mut ok_count = 0usize;
    let mut wrong_count = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok((_reader_id, Ok(count))) => {
                if count == N_ROWS {
                    ok_count += 1;
                } else {
                    wrong_count += 1;
                    errors.push(format!("reader {i}: count={count}, expected {N_ROWS}"));
                }
            }
            Ok((_reader_id, Err(e))) => {
                errors.push(format!("reader {i}: route_and_execute failed: {e}"));
            }
            Err(panic_payload) => {
                errors.push(format!("reader {i}: thread panicked: {panic_payload:?}"));
            }
        }
    }

    if ok_count == N_READERS {
        (
            true,
            format!("{N_READERS} concurrent SELECTs via route_and_execute all succeeded (count={N_ROWS})"),
        )
    } else {
        (
            false,
            format!(
                "{ok_count}/{N_READERS} readers succeeded, {wrong_count} wrong count, {} errors: {}",
                errors.len(),
                errors.join("; ")
            ),
        )
    }
}

// =========================================================================
// Property 7 — Replication (local-only WalStreamer receives records).
// =========================================================================

/// `enable_replication_local_only`; INSERT; `wal_records_streamed() >= 1`.
/// Requires `with_data_dir` (the WAL must be attached for the streamer to
/// receive records via `Wal::append_and_sync`). The local-only streamer
/// counts records via `records_sent` without actually connecting to a peer
/// — this verifies the replication wiring (Task 5.3) end-to-end.
fn verify_replication() -> VerifyResult {
    let tmp = match TempDir::new() {
        Ok(t) => t,
        Err(e) => return (false, format!("TempDir::new failed: {e}")),
    };
    let data_dir = tmp.path();

    let mut engine = match QueryEngine::with_data_dir(data_dir) {
        Ok(e) => e,
        Err(e) => return (false, format!("with_data_dir failed: {e}")),
    };
    if let Err(e) = engine.execute("CREATE TABLE repl (id INT)") {
        return (false, format!("CREATE TABLE failed: {e}"));
    }

    // Attach the local-only streamer. records_sent starts at 0.
    engine.enable_replication_local_only();
    let before = engine.wal_records_streamed();
    if before != 0 {
        return (false, format!("records_sent={before} before any INSERT (expected 0)"));
    }

    if let Err(e) = engine.execute("INSERT INTO repl VALUES (1)") {
        return (false, format!("INSERT failed: {e}"));
    }

    let after = engine.wal_records_streamed();
    if after >= 1 {
        (
            true,
            format!("INSERT streamed {after} WAL record(s) to local replica (>= 1)"),
        )
    } else {
        (
            false,
            format!("INSERT did not stream any WAL records (after={after}, before={before})"),
        )
    }
}
