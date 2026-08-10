//! Wave 5 — Transaction integration test: BEGIN/COMMIT/ROLLBACK.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE users (id INT, name VARCHAR(50))").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')").unwrap();
    e
}

#[test]
fn commit_persists_writes() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (3, 'Carol')").unwrap();
    e.execute("COMMIT").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn rollback_discards_writes() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (3, 'Carol')").unwrap();
    e.execute("ROLLBACK").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn rollback_discards_delete() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("DELETE FROM users WHERE id = 1").unwrap();
    e.execute("ROLLBACK").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn rollback_discards_update() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("UPDATE users SET name = 'Modified' WHERE id = 1").unwrap();
    e.execute("ROLLBACK").unwrap();
    // The update should not have persisted — count is still 2.
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn commit_persists_delete() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("DELETE FROM users WHERE id = 1").unwrap();
    e.execute("COMMIT").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn multiple_statements_in_txn() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (3, 'Carol')").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (4, 'Dave')").unwrap();
    e.execute("UPDATE users SET name = 'Alicia' WHERE id = 1").unwrap();
    e.execute("DELETE FROM users WHERE id = 2").unwrap();
    e.execute("COMMIT").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn select_during_txn_sees_uncommitted_writes() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO users (id, name) VALUES (3, 'Carol')").unwrap();
    // Within the txn, we should see 3 rows.
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
    e.execute("ROLLBACK").unwrap();
    // After rollback, 2 rows.
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn commit_without_begin_errors() {
    let mut e = make_engine();
    let r = e.execute("COMMIT");
    assert!(r.is_err());
}

#[test]
fn rollback_without_begin_errors() {
    let mut e = make_engine();
    let r = e.execute("ROLLBACK");
    assert!(r.is_err());
}

#[test]
fn double_begin_errors() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    let r = e.execute("BEGIN");
    assert!(r.is_err());
}

#[test]
fn create_table_in_txn_commits() {
    let mut e = make_engine();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE TABLE orders (id INT)").unwrap();
    e.execute("INSERT INTO orders (id) VALUES (1)").unwrap();
    e.execute("COMMIT").unwrap();
    let r = e.execute("SELECT count(*) FROM orders").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}
