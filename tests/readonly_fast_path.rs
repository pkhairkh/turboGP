//! Wave 2 — Agent C: verify the read-only fast path.
//!
//! These tests prove `QueryEngine::execute_readonly(&self)` works for
//! SELECT queries, rejects DML/DDL, and supports concurrent reads.

use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use turbogp::engine::QueryEngine;

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
