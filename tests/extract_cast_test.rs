//! EXTRACT + CAST integration tests.

use turbogp::engine::QueryEngine;

#[test]
fn cast_int_to_float() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (42)").unwrap();
    let r = e.execute("SELECT CAST(id AS FLOAT) FROM t");
    assert!(r.is_ok(), "CAST must execute; got: {:?}", r.err());
    let r = r.unwrap();
    assert_eq!(r.row_count, 1);
}

#[test]
fn cast_float_to_int() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (v FLOAT)").unwrap();
    e.execute("INSERT INTO t (v) VALUES (3.14)").unwrap();
    let r = e.execute("SELECT CAST(v AS INT) FROM t");
    assert!(r.is_ok(), "CAST FLOAT AS INT must execute; got: {:?}", r.err());
}

#[test]
fn extract_year_basic() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (d INT)").unwrap();
    e.execute("INSERT INTO t (d) VALUES (20240115)").unwrap();
    // EXTRACT on an integer column — the tpch interpreter handles this.
    let r = e.execute("SELECT EXTRACT(YEAR FROM d) FROM t");
    // This may fall to tpch; just verify it doesn't error.
    assert!(r.is_ok(), "EXTRACT must execute; got: {:?}", r.err());
}
