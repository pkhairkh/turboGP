//! SAVEPOINT + nested transactions integration tests (Wave 69).

use turbogp::engine::QueryEngine;

#[test]
fn savepoint_rollback_to() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    e.execute("SAVEPOINT sp1").unwrap();
    e.execute("INSERT INTO t (id) VALUES (2)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (3)").unwrap();
    // Roll back to sp1 — should undo inserts 2 and 3, keeping insert 1.
    e.execute("ROLLBACK TO sp1").unwrap();
    e.execute("COMMIT").unwrap();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1), "ROLLBACK TO must undo inserts after the savepoint");
}

#[test]
fn savepoint_release() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    e.execute("SAVEPOINT sp1").unwrap();
    e.execute("INSERT INTO t (id) VALUES (2)").unwrap();
    e.execute("RELEASE sp1").unwrap();
    e.execute("COMMIT").unwrap();
    // Both inserts should be visible (RELEASE doesn't undo, it just discards the savepoint).
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "RELEASE must not undo inserts");
}

#[test]
fn savepoint_requires_transaction() {
    let mut e = QueryEngine::in_memory();
    // SAVEPOINT without BEGIN creates a savepoint on the main catalog.
    // It's not useful but shouldn't crash.
    let r = e.execute("SAVEPOINT sp1");
    assert!(r.is_ok(), "SAVEPOINT without BEGIN should not crash; got: {:?}", r.err());
}

#[test]
fn rollback_to_nonexistent_savepoint_errors() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    let r = e.execute("ROLLBACK TO nonexistent");
    assert!(r.is_err(), "ROLLBACK TO nonexistent savepoint must error");
    e.execute("ROLLBACK").unwrap();
}
