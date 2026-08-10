//! Wave 3 — DDL integration test: CREATE TABLE, DROP TABLE, CREATE SCHEMA.

use turbogp::engine::QueryEngine;

#[test]
fn create_table_and_select_count() {
    let mut engine = QueryEngine::new();
    engine
        .execute("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL)")
        .expect("create table");
    let r = engine.execute("SELECT count(*) FROM users").expect("count");
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn create_table_with_all_types() {
    let mut engine = QueryEngine::new();
    engine
        .execute(
            "CREATE TABLE t (
        a INT, b BIGINT, c SMALLINT, d TINYINT,
        e VARCHAR(50), f NVARCHAR(100), g TEXT,
        h FLOAT, i REAL, j DECIMAL(18,2), k NUMERIC(10,4),
        l BIT, m BOOLEAN, n DATE, o TIMESTAMP
    )",
        )
        .expect("create table");
    let r = engine.execute("SELECT count(*) FROM t").expect("count");
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn create_qualified_table() {
    let mut engine = QueryEngine::new();
    engine.execute("CREATE SCHEMA HR").expect("schema");
    engine
        .execute("CREATE TABLE HR.Employees (id INT, name VARCHAR(50))")
        .expect("create qualified");
    let r = engine.execute("SELECT count(*) FROM HR.Employees").expect("count");
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn create_if_not_exists() {
    let mut engine = QueryEngine::new();
    engine.execute("CREATE TABLE t (id INT)").expect("first create");
    // Second create with IF NOT EXISTS should succeed (no-op).
    engine.execute("CREATE TABLE IF NOT EXISTS t (id INT)").expect("if not exists");
    // Without IF NOT EXISTS should fail.
    let r = engine.execute("CREATE TABLE t (id INT)");
    assert!(r.is_err(), "expected error for duplicate table");
}

#[test]
fn drop_table() {
    let mut engine = QueryEngine::new();
    engine.execute("CREATE TABLE temp (id INT)").expect("create");
    engine.execute("DROP TABLE temp").expect("drop");
    // Selecting from dropped table should fail.
    let r = engine.execute("SELECT count(*) FROM temp");
    assert!(r.is_err());
}

#[test]
fn drop_table_if_exists() {
    let mut engine = QueryEngine::new();
    // Dropping a non-existent table with IF EXISTS should succeed.
    engine.execute("DROP TABLE IF EXISTS nonexistent").expect("drop if exists");
    // Without IF EXISTS should fail.
    let r = engine.execute("DROP TABLE nonexistent");
    assert!(r.is_err());
}

#[test]
fn create_with_defaults_and_identity() {
    let mut engine = QueryEngine::new();
    engine
        .execute(
            "CREATE TABLE t (
        id INT IDENTITY(1,1) PRIMARY KEY,
        active BIT DEFAULT 1 NOT NULL,
        created DATE DEFAULT '2026-01-01'
    )",
        )
        .expect("create with defaults");
    let r = engine.execute("SELECT count(*) FROM t").expect("count");
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn create_with_references() {
    let mut engine = QueryEngine::new();
    engine.execute("CREATE TABLE users (id INT PRIMARY KEY)").expect("create users");
    engine
        .execute(
            "CREATE TABLE orders (
        id INT PRIMARY KEY,
        user_id INT REFERENCES users(id)
    )",
        )
        .expect("create orders with FK");
}

#[test]
fn create_then_drop_then_recreate() {
    let mut engine = QueryEngine::new();
    engine.execute("CREATE TABLE t (id INT)").expect("create");
    engine.execute("DROP TABLE t").expect("drop");
    engine.execute("CREATE TABLE t (id INT)").expect("recreate");
    let r = engine.execute("SELECT count(*) FROM t").expect("count");
    assert_eq!(r.scalar_u64(), Some(0));
}
