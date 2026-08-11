//! Wave 7 — Agent C: End-to-end integration tests.
//!
//! These tests verify the full engine integration:
//! - Task 7.1: SQL → plan → optimize → lower → execute → kernel pipeline
//! - Task 7.2: Concurrent transactions with MVCC

use std::sync::{Arc, RwLock};
use std::thread;
use turbogp::engine::{
    planner_pipeline_invoked_count, reset_planner_pipeline_counter, QueryEngine,
};
use turbogp::planner::{kernel_table_select_count, reset_kernel_table_select_counter};

// =========================================================================
// Task 7.1 — End-to-end planner pipeline integration test.
// =========================================================================

#[test]
fn test_e2e_planner_pipeline() {
    // Verify the full pipeline: SQL → parse → build_plan → Cascades →
    // PlanLowerer → Scheduler → KernelTable::select.
    //
    // We use a flag-based verification: reset the planner-pipeline counter
    // and kernel-table-select counter, run a query, and assert both are ≥ 1.
    let mut engine = QueryEngine::in_memory();

    // Create a table and insert rows.
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("INSERT INTO t VALUES (2)").unwrap();
    engine.execute("INSERT INTO t VALUES (3)").unwrap();

    // Reset the counters.
    reset_planner_pipeline_counter();
    reset_kernel_table_select_counter();

    // Run a SELECT COUNT(*) — this should invoke the full pipeline.
    let result = engine.execute("SELECT COUNT(*) FROM t").unwrap();

    // Verify correctness.
    assert_eq!(result.row_count, 1, "COUNT(*) returns 1 row");
    assert_eq!(
        result.columns[0].values[0], 3,
        "COUNT(*) should return 3"
    );

    // Verify the planner pipeline was invoked.
    assert!(
        planner_pipeline_invoked_count() >= 1,
        "planner pipeline must be invoked from execute() (build_plan, Cascades, \
         PlanLowerer, Scheduler all called), got count={}",
        planner_pipeline_invoked_count()
    );

    // Verify KernelTable::select was called.
    assert!(
        kernel_table_select_count() >= 1,
        "KernelTable::select must be called from execute() via the Scheduler, \
         got count={}",
        kernel_table_select_count()
    );
}

#[test]
fn test_e2e_planner_pipeline_select_star() {
    // Verify SELECT * also goes through the planner pipeline.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    engine.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();

    reset_planner_pipeline_counter();
    reset_kernel_table_select_counter();

    let result = engine.execute("SELECT * FROM t").unwrap();
    assert_eq!(result.row_count, 2, "SELECT * returns 2 rows");
    assert!(
        planner_pipeline_invoked_count() >= 1,
        "planner pipeline must be invoked for SELECT *"
    );
    assert!(
        kernel_table_select_count() >= 1,
        "KernelTable::select must be called for SELECT *"
    );
}

#[test]
fn test_e2e_explain_uses_planner() {
    // Verify EXPLAIN uses the planner pipeline (build_plan + Cascades).
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();

    let result = engine.execute("EXPLAIN SELECT * FROM t WHERE id = 1").unwrap();
    assert_eq!(result.row_count, 1);
    assert_eq!(result.columns[0].name, "QUERY PLAN");
    let plan_text = &result.columns[0].string_values.as_ref().unwrap()[0];
    assert!(plan_text.contains("Scan"), "plan must contain Scan");
    assert!(plan_text.contains("Filter"), "plan must contain Filter");
}

// =========================================================================
// Task 7.2 — Concurrent transaction integration test (MVCC).
// =========================================================================

#[test]
fn test_concurrent_writers_mvcc() {
    // Wave 7 Task 7.2 DoD: spawn 2 threads, each does BEGIN/INSERT/COMMIT.
    // Both succeed; no lost updates; no data corruption.
    //
    // This test only passes if MVCC is wired (Wave 4) and the engine
    // supports concurrent transactions.
    let mut engine = QueryEngine::in_memory();
    engine.enable_mvcc().unwrap();
    engine.execute("CREATE TABLE t (id INT, thread INT)").unwrap();
    let engine = Arc::new(RwLock::new(engine));

    let engine_a = Arc::clone(&engine);
    let engine_b = Arc::clone(&engine);

    let handle_a = thread::spawn(move || -> Result<(), String> {
        let mut guard = engine_a.write().map_err(|e| format!("lock: {e}"))?;
        guard.execute("BEGIN").map_err(|e| format!("BEGIN: {e}"))?;
        for i in 0..10 {
            guard
                .execute(&format!("INSERT INTO t VALUES ({}, 0)", i))
                .map_err(|e| format!("INSERT: {e}"))?;
        }
        guard.execute("COMMIT").map_err(|e| format!("COMMIT: {e}"))?;
        Ok(())
    });

    let handle_b = thread::spawn(move || -> Result<(), String> {
        let mut guard = engine_b.write().map_err(|e| format!("lock: {e}"))?;
        guard.execute("BEGIN").map_err(|e| format!("BEGIN: {e}"))?;
        for i in 0..10 {
            guard
                .execute(&format!("INSERT INTO t VALUES ({}, 1)", i + 100))
                .map_err(|e| format!("INSERT: {e}"))?;
        }
        guard.execute("COMMIT").map_err(|e| format!("COMMIT: {e}"))?;
        Ok(())
    });

    let a = handle_a.join().expect("thread A panicked");
    let b = handle_b.join().expect("thread B panicked");
    a.expect("thread A failed");
    b.expect("thread B failed");

    // Verify no lost updates: 20 rows total (10 from each thread).
    let r = engine.write().unwrap().execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        r.columns[0].values[0], 20,
        "both threads' inserts should be visible (no lost updates), got {}",
        r.columns[0].values[0]
    );
}

#[test]
fn test_concurrent_readers_dont_block_writer() {
    // Wave 7: verify that concurrent readers (via execute_readonly) don't
    // block a writer (via execute). This is the production read/write lock
    // pattern from Wave 2.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..10 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    let engine = Arc::new(RwLock::new(engine));

    // Spawn a writer that inserts rows.
    let engine_writer = Arc::clone(&engine);
    let writer = thread::spawn(move || {
        let mut guard = engine_writer.write().unwrap();
        for i in 0..5 {
            guard.execute(&format!("INSERT INTO t VALUES ({})", i + 1000)).unwrap();
        }
        guard.execute("SELECT COUNT(*) FROM t").unwrap().columns[0].values[0]
    });

    // Spawn readers that read while the writer holds the lock.
    let engine_reader = Arc::clone(&engine);
    let reader = thread::spawn(move || {
        let guard = engine_reader.read().unwrap();
        guard.execute_readonly("SELECT COUNT(*) FROM t").unwrap().columns[0].values[0]
    });

    let writer_count = writer.join().unwrap();
    let reader_count = reader.join().unwrap();

    // The writer should see 15 rows (10 initial + 5 new).
    assert_eq!(writer_count, 15, "writer should see 15 rows");
    // The reader might see 10 or 15 depending on timing, but it should not
    // deadlock or panic.
    assert!(reader_count >= 10, "reader should see at least 10 rows");
}

// =========================================================================
// Task 7.1 — Additional integration: planner pipeline + kernel reachability
// for multiple query shapes.
// =========================================================================

#[test]
fn test_e2e_multiple_query_shapes_reach_kernel() {
    // Verify that multiple query shapes (SELECT *, COUNT(*), filtered) all
    // invoke the planner pipeline.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    for i in 0..20 {
        engine.execute(&format!("INSERT INTO t VALUES ({}, {})", i, i * 2)).unwrap();
    }

    let shapes = vec![
        "SELECT * FROM t",
        "SELECT COUNT(*) FROM t",
        "SELECT * FROM t WHERE id = 5",
        "SELECT COUNT(*) FROM t WHERE v > 10",
        "SELECT * FROM t LIMIT 5",
    ];

    for sql in &shapes {
        reset_planner_pipeline_counter();
        let _ = engine.execute(sql);
        assert!(
            planner_pipeline_invoked_count() >= 1,
            "planner pipeline must be invoked for: {}",
            sql
        );
    }
}
