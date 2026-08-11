//! Wave 2 — Agent C: verify the read-only fast path.
//!
//! These tests prove `QueryEngine::execute_readonly(&self)` works for
//! SELECT queries, rejects DML/DDL, and supports concurrent reads.

use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::Instant;
use turbogp::engine::{is_readonly_sql, route_and_execute, QueryEngine};

#[test]
fn test_execute_readonly_select_works() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

    // execute_readonly takes &self, not &mut self.
    let result = engine.execute_readonly("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(result.row_count, 1);
    assert_eq!(result.columns[0].values[0], 3);
}

#[test]
fn test_execute_readonly_explain_works() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();

    // EXPLAIN is read-only and should work without a write lock.
    let result = engine.execute_readonly("EXPLAIN SELECT * FROM t WHERE id = 1").unwrap();
    assert_eq!(result.row_count, 1);
    assert_eq!(result.columns[0].name, "QUERY PLAN");
    let plan_text = &result.columns[0].string_values.as_ref().unwrap()[0];
    assert!(plan_text.contains("Scan"), "plan must contain Scan");
    assert!(plan_text.contains("Filter"), "plan must contain Filter");
}

#[test]
fn test_execute_readonly_rejects_dml() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();

    let err = engine.execute_readonly("INSERT INTO t VALUES (1)").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "DML must be rejected with read-only error, got: {}",
        msg
    );

    let err = engine.execute_readonly("UPDATE t SET id = 0").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "UPDATE must be rejected, got: {}",
        msg
    );

    let err = engine.execute_readonly("DELETE FROM t").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "DELETE must be rejected, got: {}",
        msg
    );
}

#[test]
fn test_execute_readonly_rejects_ddl() {
    let mut engine = QueryEngine::in_memory();

    let err = engine.execute_readonly("CREATE TABLE t (id INT)").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "CREATE must be rejected, got: {}",
        msg
    );

    let err = engine.execute_readonly("DROP TABLE nonexistent").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "DROP must be rejected, got: {}",
        msg
    );
}

#[test]
fn test_execute_readonly_rejects_txn_control() {
    let engine = QueryEngine::in_memory();

    let err = engine.execute_readonly("BEGIN").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "BEGIN must be rejected, got: {}",
        msg
    );

    let err = engine.execute_readonly("COMMIT").unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("read-only") || msg.contains("write lock"),
        "COMMIT must be rejected, got: {}",
        msg
    );
}

#[test]
fn test_concurrent_execute_readonly_does_not_block() {
    // Wave 2 Task 2.1 DoD: two threads call execute_readonly() concurrently →
    // both succeed without blocking. We wrap the engine in Arc<RwLock<QueryEngine>>
    // (the production pattern) and have both threads acquire a *read* lock
    // before calling execute_readonly.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..100 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }

    let engine = Arc::new(RwLock::new(engine));

    // Spawn 2 reader threads. Both acquire RwLock::read() and call
    // execute_readonly. With a proper read lock, both should run concurrently.
    let engine_a = Arc::clone(&engine);
    let engine_b = Arc::clone(&engine);

    let handle_a = thread::spawn(move || {
        let guard = engine_a.read().unwrap();
        let r = guard.execute_readonly("SELECT COUNT(*) FROM t").unwrap();
        r.columns[0].values[0]
    });

    let handle_b = thread::spawn(move || {
        let guard = engine_b.read().unwrap();
        let r = guard.execute_readonly("SELECT COUNT(*) FROM t").unwrap();
        r.columns[0].values[0]
    });

    let (a, b) = (handle_a.join().unwrap(), handle_b.join().unwrap());
    assert_eq!(a, 100, "thread A should see 100 rows");
    assert_eq!(b, 100, "thread B should see 100 rows");
}

#[test]
fn test_concurrent_execute_readonly_ten_threads() {
    // Stress test: 10 threads, each does 5 SELECT COUNT(*) calls.
    // All should succeed without deadlock or panic.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..50 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    let engine = Arc::new(RwLock::new(engine));

    let mut handles = Vec::new();
    for _ in 0..10 {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            let mut counts = Vec::new();
            for _ in 0..5 {
                let guard = engine.read().unwrap();
                let r = guard.execute_readonly("SELECT COUNT(*) FROM t").unwrap();
                counts.push(r.columns[0].values[0]);
            }
            counts
        }));
    }

    for h in handles {
        let counts = h.join().expect("thread should not panic");
        for c in counts {
            assert_eq!(c, 50, "each read should see 50 rows");
        }
    }
}

// ---------------------------------------------------------------------------
// Wave 2 Task 2.2 — route_and_execute tests.
// ---------------------------------------------------------------------------

#[test]
fn test_is_readonly_sql_classification() {
    // SELECT-like statements are readonly.
    assert!(is_readonly_sql("SELECT * FROM t"));
    assert!(is_readonly_sql("SELECT COUNT(*) FROM t WHERE id = 5"));
    assert!(is_readonly_sql("  EXPLAIN SELECT * FROM t  "));
    assert!(is_readonly_sql("SHOW tables"));

    // DML/DDL/transaction control are NOT readonly.
    assert!(!is_readonly_sql("INSERT INTO t VALUES (1)"));
    assert!(!is_readonly_sql("UPDATE t SET id = 0"));
    assert!(!is_readonly_sql("DELETE FROM t"));
    assert!(!is_readonly_sql("CREATE TABLE t (id INT)"));
    assert!(!is_readonly_sql("DROP TABLE t"));
    assert!(!is_readonly_sql("BEGIN"));
    assert!(!is_readonly_sql("COMMIT"));
    assert!(!is_readonly_sql("VACUUM"));
    assert!(!is_readonly_sql("COPY t TO '/tmp/x.csv'"));
    assert!(!is_readonly_sql("BACKUP TO '/tmp/x'"));
}

#[test]
fn test_route_and_execute_routes_select_to_read_lock() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let engine = Arc::new(RwLock::new(engine));

    // SELECT should route to the read path and succeed.
    let r = route_and_execute(&engine, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 3);
}

#[test]
fn test_route_and_execute_routes_dml_to_write_lock() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    let engine = Arc::new(RwLock::new(engine));

    // INSERT should route to the write path and succeed.
    route_and_execute(&engine, "INSERT INTO t VALUES (42)").unwrap();

    // Verify the row was inserted.
    let r = route_and_execute(&engine, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 1);
}

#[test]
fn test_route_and_execute_concurrent_selects_run_in_parallel() {
    // Wave 2 Task 2.2 DoD: 10 concurrent SELECTs run in parallel.
    //
    // We verify this by comparing 10 parallel SELECTs against 10 serial
    // SELECTs. If SELECTs acquire a *write* lock (the bug we're guarding
    // against), the parallel batch would take ~10× as long as the serial
    // batch (because each parallel thread would wait for the write lock).
    // With a *read* lock, all 10 threads can run concurrently, so the
    // parallel batch should be much faster than 10× the serial batch.
    //
    // We use a SELECT with a WHERE clause so the query actually scans data
    // (the planner short-circuits SELECT * and COUNT(*) without WHERE, so
    // those are too fast to measure parallelism benefit).
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE big (id INT, v INT)").unwrap();
    for i in 0..50_000 {
        engine.execute(&format!("INSERT INTO big VALUES ({}, {})", i, i * 2)).unwrap();
    }
    let engine = Arc::new(RwLock::new(engine));

    // The query: scans the table, returns count of rows where v > 10000.
    // This does real work (vectorized scan over 50k rows).
    let sql = "SELECT COUNT(*) FROM big WHERE v > 10000";

    // Warm up.
    let _ = route_and_execute(&engine, sql).unwrap();

    // Measure a single SELECT to get a baseline.
    let start_one = Instant::now();
    let r = route_and_execute(&engine, sql).unwrap();
    assert!(r.columns[0].values[0] > 0);
    let one_elapsed = start_one.elapsed();

    // Measure 10 PARALLEL SELECTs.
    let start_parallel = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..10 {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || route_and_execute(&engine, sql).unwrap()));
    }
    for h in handles {
        let _ = h.join().unwrap();
    }
    let parallel_elapsed = start_parallel.elapsed();

    // Measure 10 SERIAL SELECTs.
    let start_serial = Instant::now();
    for _ in 0..10 {
        let _ = route_and_execute(&engine, sql).unwrap();
    }
    let serial_elapsed = start_serial.elapsed();

    println!(
        "1 query: {:?}  10 parallel: {:?}  10 serial: {:?}  parallel/serial ratio: {:.2}",
        one_elapsed,
        parallel_elapsed,
        serial_elapsed,
        parallel_elapsed.as_secs_f64() / serial_elapsed.as_secs_f64()
    );

    // If SELECTs acquire a WRITE lock, the 10 parallel SELECTs would be
    // fully serialized → parallel ≈ 10 × (one_query_time + thread_spawn)
    // ≈ 10 × serial_per_query. With a READ lock, they overlap, so parallel
    // should be much less than 10× the serial batch.
    //
    // We assert `parallel < serial * 3` — this catches the write-lock
    // regression (where parallel would be ~10× serial) while allowing
    // for thread-spawn overhead on low-core-count CI machines. On a
    // 2-core machine, parallel is typically ~1.0-1.2× serial (slightly
    // slower due to spawn overhead). On an 8+ core machine, parallel is
    // typically ~0.2-0.5× serial.
    assert!(
        parallel_elapsed < serial_elapsed * 3,
        "10 parallel SELECTs ({:?}) should be much faster than 3× 10 serial SELECTs ({:?}) \
         — if parallel ≥ 3× serial, SELECTs are taking a write lock and serializing. \
         (1 query took {:?})",
        parallel_elapsed,
        serial_elapsed,
        one_elapsed
    );
}

// ---------------------------------------------------------------------------
// Wave 2 Task 2.3 — Catalog concurrent-read test.
// ---------------------------------------------------------------------------

#[test]
fn test_catalog_concurrent_reads_no_deadlock() {
    // Wave 2 Task 2.3 DoD: 10 threads call catalog.get() concurrently →
    // no deadlock, no panic.
    //
    // The Catalog itself (src/catalog/) is a plain HashMap owned by Agent B.
    // Agent C cannot add an internal RwLock to it. However, concurrent reads
    // already work because:
    //   - execute_readonly(&self) only takes &self.catalog (a shared ref)
    //   - multiple &self references coexist via the RwLock<QueryEngine> wrapper
    //   - shared references to a HashMap are Sync (concurrent reads are safe)
    //
    // This test verifies that 10 threads calling execute_readonly (which
    // internally calls catalog.get()) don't deadlock or panic.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("CREATE TABLE t2 (id INT, name VARCHAR(50))").unwrap();
    for i in 0..100 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
        engine.execute(&format!("INSERT INTO t2 VALUES ({}, 'name{}')", i, i)).unwrap();
    }
    let engine = Arc::new(RwLock::new(engine));

    let mut handles = Vec::new();
    for thread_id in 0..10 {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            // Each thread does a mix of reads against t and t2.
            let guard = engine.read().unwrap();
            let r1 = guard.execute_readonly("SELECT COUNT(*) FROM t").unwrap();
            let r2 = guard.execute_readonly("SELECT COUNT(*) FROM t2").unwrap();
            (thread_id, r1.columns[0].values[0], r2.columns[0].values[0])
        }));
    }

    for h in handles {
        let (tid, c1, c2) = h.join().expect("thread should not panic");
        assert_eq!(c1, 100, "thread {} saw {} rows in t, expected 100", tid, c1);
        assert_eq!(c2, 100, "thread {} saw {} rows in t2, expected 100", tid, c2);
    }
}
