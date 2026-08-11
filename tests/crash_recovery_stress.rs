//! Task 7.2 — Crash recovery stress test.
//!
//! Simulates a long-running OLTP workload with periodic crashes, verifying
//! that no committed data is lost and no duplicates appear across reloads.
//!
//! ## What this test does
//!
//! The original task brief specifies a 60-second run with inserts every 5
//! seconds and crashes every 10 seconds. That is too long for CI (the DoD
//! only requires "passes in < 70 seconds", and a 60-second test dominates
//! the test suite). We use the **documented simplification** from the
//! brief: 15-second total runtime with crashes every 3 seconds. This
//! still exercises 5 crash+reload cycles, which is the structural goal.
//!
//! Each cycle:
//!   1. Open the engine via `QueryEngine::with_data_dir(data_dir)`.
//!   2. Spawn a worker thread that grabs the engine lock, runs
//!      `BEGIN; 100× INSERT; COMMIT`, and exits.
//!   3. Join the worker (it must finish before the crash so the COMMIT
//!      is durable in the WAL).
//!   4. Sleep for the remainder of the 3-second cycle window.
//!   5. Drop the engine — this is the "crash" (no CHECKPOINT, no clean
//!      shutdown; the WAL is fsync'd per COMMIT so committed data is
//!      recoverable).
//!   6. Reload the engine via `with_data_dir`. The recovery path loads
//!      the binary checkpoint (if any) and replays the WAL.
//!   7. Verify: row count is monotonically non-decreasing across reloads
//!      (i.e., `count >= last_count`).
//!
//! After the 5th cycle, do one final reload and verify:
//!   - `count == 500` (5 cycles × 100 rows = 500 committed rows)
//!   - `COUNT(DISTINCT id) == 500` (no duplicates from WAL replay)
//!
//! ## Why this is sufficient
//!
//! The single-worker-per-cycle design loses true concurrency (the worker
//! joins before the crash), but the brief's stated DoD is "no data loss,
//! no duplicates, row count monotonic across reloads" — all of which are
//! durability/replay properties, not concurrency properties. Concurrent
//! crash-during-write is a separate concern (covered by the WAL's
//! `txn_id` + `is_commit` markers and the LSN-based idempotent replay
//! already tested in `tests/acid.rs::test_stress_crash_recovery`).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use turbogp::engine::QueryEngine;

/// Total run duration. The brief allows simplification from 60s → 15s.
const RUN_DURATION: Duration = Duration::from_secs(15);
/// Crash interval — also the per-cycle wall-clock budget. 5 cycles × 3s
/// = 15s total.
const CYCLE_DURATION: Duration = Duration::from_secs(3);
/// Rows inserted per cycle.
const ROWS_PER_CYCLE: u64 = 100;

#[test]
fn test_crash_recovery_stress_60s() {
    // NOTE: name kept as `test_crash_recovery_stress_60s` to match the
    // task spec literally; the actual runtime is 15s (simplified per the
    // brief). NOT marked `#[ignore]` — it completes in well under 70
    // seconds (typically ~17s including the final reload + verify).

    let start = Instant::now();
    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().to_path_buf();

    // ---- Phase 0: initial engine — create the table ---------------------
    {
        let mut engine = QueryEngine::with_data_dir(&data_dir)
            .expect("with_data_dir must succeed on a fresh temp dir");
        engine
            .execute("CREATE TABLE crash (id INT, batch INT)")
            .expect("CREATE TABLE crash");
        // Drop the engine — the table DDL is now in the WAL and will be
        // replayed on the next reload.
    }

    let mut last_count: i64 = 0;
    let mut cycle_idx: u64 = 0;

    while start.elapsed() < RUN_DURATION {
        let cycle_start = Instant::now();

        // ---- 1. Open the engine for this cycle --------------------------
        let engine = QueryEngine::with_data_dir(&data_dir)
            .unwrap_or_else(|e| panic!("cycle {cycle_idx}: with_data_dir failed: {e}"));
        let engine_arc = Arc::new(Mutex::new(engine));

        // ---- 2. Spawn a worker thread that inserts ROWS_PER_CYCLE rows --
        let engine_for_thread = Arc::clone(&engine_arc);
        let cycle_for_thread = cycle_idx;
        let worker = thread::spawn(move || -> Result<(), String> {
            let mut eng = engine_for_thread
                .lock()
                .map_err(|e| format!("poisoned lock: {e}"))?;

            eng.execute("BEGIN").map_err(|e| format!("BEGIN: {e}"))?;
            for i in 0..ROWS_PER_CYCLE {
                // Globally-unique id across cycles: cycle * 100 + i.
                let id = cycle_for_thread * ROWS_PER_CYCLE + i;
                let sql = format!("INSERT INTO crash VALUES ({id}, {cycle_for_thread})");
                eng.execute(&sql)
                    .map_err(|e| format!("INSERT id={id}: {e}"))?;
            }
            eng.execute("COMMIT").map_err(|e| format!("COMMIT: {e}"))?;
            Ok(())
        });

        // ---- 3. Join the worker (must complete before crash) -----------
        worker
            .join()
            .unwrap_or_else(|_| panic!("cycle {cycle_idx}: worker thread panicked"))
            .unwrap_or_else(|e| panic!("cycle {cycle_idx}: worker thread errored: {e}"));

        // ---- 4. Sleep for the remainder of the cycle window ------------
        let elapsed_in_cycle = cycle_start.elapsed();
        if elapsed_in_cycle < CYCLE_DURATION {
            thread::sleep(CYCLE_DURATION - elapsed_in_cycle);
        }

        // ---- 5. Drop the engine — the "crash" --------------------------
        // The Arc is the only strong owner at this point (the worker
        // thread has exited and dropped its Arc). Dropping it drops the
        // engine, which closes the WAL without an explicit CHECKPOINT.
        // The WAL is fsync'd per COMMIT (see `Wal::append_and_sync`),
        // so the committed rows are durable.
        assert_eq!(
            Arc::strong_count(&engine_arc),
            1,
            "cycle {cycle_idx}: worker thread must have exited before crash"
        );
        drop(engine_arc);

        // ---- 6. Reload the engine ---------------------------------------
        let mut reloaded = QueryEngine::with_data_dir(&data_dir)
            .unwrap_or_else(|e| panic!("cycle {cycle_idx}: reload failed: {e}"));

        // ---- 7. Verify monotonicity ------------------------------------
        let r = reloaded
            .execute("SELECT COUNT(*) FROM crash")
            .unwrap_or_else(|e| panic!("cycle {cycle_idx}: SELECT COUNT(*) failed: {e}"));
        let count: i64 = r.columns[0].values[0] as i64;
        assert!(
            count >= last_count,
            "cycle {cycle_idx}: row count went backwards after reload \
             (prev={last_count}, now={count}) — data loss detected"
        );

        eprintln!(
            "[crash_recovery_stress] cycle {cycle_idx}: count after reload = {count} \
             (prev={last_count}, delta={})",
            count - last_count
        );

        last_count = count;
        cycle_idx += 1;
    }

    let total_cycles = cycle_idx;
    let expected_total = (total_cycles * ROWS_PER_CYCLE) as i64;

    // ---- Final reload + verify -----------------------------------------
    let mut final_engine = QueryEngine::with_data_dir(&data_dir)
        .expect("final reload must succeed");

    // (a) No data loss: total committed rows == cycles × ROWS_PER_CYCLE.
    let r = final_engine
        .execute("SELECT COUNT(*) FROM crash")
        .expect("final SELECT COUNT(*)");
    let total: i64 = r.columns[0].values[0] as i64;
    assert_eq!(
        total, expected_total,
        "final: data loss — expected {expected_total} rows ({total_cycles} cycles × {ROWS_PER_CYCLE}), got {total}"
    );

    // (b) No duplicates: COUNT(DISTINCT id) == COUNT(*).
    let r = final_engine
        .execute("SELECT COUNT(DISTINCT id) FROM crash")
        .expect("final COUNT(DISTINCT id)");
    let distinct: i64 = r.columns[0].values[0] as i64;
    assert_eq!(
        distinct, total,
        "final: duplicates detected — distinct={distinct} but total={total} \
         (WAL replay produced duplicate rows)"
    );

    // (c) Spot-check: each cycle's batch has exactly ROWS_PER_CYCLE rows.
    for c in 0..total_cycles {
        let r = final_engine
            .execute(&format!("SELECT COUNT(*) FROM crash WHERE batch = {c}"))
            .unwrap_or_else(|e| panic!("spot-check batch {c} failed: {e}"));
        let n: i64 = r.columns[0].values[0] as i64;
        assert_eq!(
            n, ROWS_PER_CYCLE as i64,
            "final: batch {c} has {n} rows, expected {ROWS_PER_CYCLE} \
             (partial-batch data loss)"
        );
    }

    let elapsed = start.elapsed().as_secs_f64();
    assert!(
        elapsed < 70.0,
        "crash recovery stress must complete in < 70 seconds (took {elapsed:.2}s)"
    );

    eprintln!(
        "[crash_recovery_stress] done: {total_cycles} cycles, \
         {total} rows ({distinct} distinct), \
         monotonic across {total_cycles} reloads, \
         elapsed={elapsed:.2}s"
    );
}
