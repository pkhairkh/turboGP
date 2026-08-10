//! Wave 4 — DML integration test: INSERT / UPDATE / DELETE.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE users (id INT, name VARCHAR(50), active BIT)").unwrap();
    e
}

#[test]
fn insert_single_row() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1)").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn insert_multiple_rows() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0), (3, 'Carol', 1)")
        .unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn insert_without_column_list() {
    let mut e = make_engine();
    e.execute("INSERT INTO users VALUES (1, 'Alice', 1)").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn insert_null() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, NULL, 0)").unwrap();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn insert_then_select_count_with_where() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0), (3, 'Carol', 1)")
        .unwrap();
    // The kernel-direct path only handles single-equality WHERE on count(*).
    // This should match active = 1 → 2 rows.
    let r = e.execute("SELECT count(*) FROM users WHERE active = 1").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn update_with_where() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0)")
        .unwrap();
    let r = e.execute("UPDATE users SET active = 1 WHERE id = 2").unwrap();
    assert_eq!(r.row_count, 1);
    // Now both users should have active = 1.
    let r = e.execute("SELECT count(*) FROM users WHERE active = 1").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn update_all_rows() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0)")
        .unwrap();
    let r = e.execute("UPDATE users SET active = 0").unwrap();
    assert_eq!(r.row_count, 2);
    let r = e.execute("SELECT count(*) FROM users WHERE active = 1").unwrap();
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn delete_with_where() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0), (3, 'Carol', 1)")
        .unwrap();
    let r = e.execute("DELETE FROM users WHERE active = 0").unwrap();
    assert_eq!(r.row_count, 1);
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn delete_all_rows() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0)")
        .unwrap();
    let r = e.execute("DELETE FROM users").unwrap();
    assert_eq!(r.row_count, 2);
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn insert_float_values() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, price FLOAT)").unwrap();
    e.execute("INSERT INTO t (id, price) VALUES (1, 19.99), (2, 29.50)").unwrap();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn update_with_and_clause() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0), (3, 'Carol', 1)")
        .unwrap();
    // Update only the row where id = 2 AND active = 0.
    let r = e.execute("UPDATE users SET active = 1 WHERE id = 2 AND active = 0").unwrap();
    assert_eq!(r.row_count, 1);
    let r = e.execute("SELECT count(*) FROM users WHERE active = 1").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn delete_with_or_clause() {
    let mut e = make_engine();
    e.execute("INSERT INTO users (id, name, active) VALUES (1, 'Alice', 1), (2, 'Bob', 0), (3, 'Carol', 0)")
        .unwrap();
    // Delete rows where active = 0 OR id = 1.
    let r = e.execute("DELETE FROM users WHERE active = 0 OR id = 1").unwrap();
    assert_eq!(r.row_count, 3);
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn insert_into_qualified_table() {
    let mut e = QueryEngine::new();
    e.execute("CREATE SCHEMA HR").unwrap();
    e.execute("CREATE TABLE HR.Employees (id INT)").unwrap();
    e.execute("INSERT INTO HR.Employees (id) VALUES (1), (2), (3)").unwrap();
    let r = e.execute("SELECT count(*) FROM HR.Employees").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}
