//! Wave 33 — NULL bitmap consulted by aggregates.
//! Verifies COUNT(col), SUM, AVG, MIN, MAX correctly ignore NULL values.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, val INT)").unwrap();
    e.execute("INSERT INTO t (id, val) VALUES (1, 10)").unwrap();
    e.execute("INSERT INTO t (id, val) VALUES (2, NULL)").unwrap();
    e.execute("INSERT INTO t (id, val) VALUES (3, 30)").unwrap();
    e.execute("INSERT INTO t (id, val) VALUES (4, NULL)").unwrap();
    e.execute("INSERT INTO t (id, val) VALUES (5, 50)").unwrap();
    e
}

#[test]
fn count_star_counts_all_rows() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(5)); // all rows, including NULLs
}

#[test]
fn count_col_excludes_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(val) FROM t").unwrap();
    // val is non-NULL for rows 1,3,5 → 3
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn sum_ignores_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT sum(val) FROM t").unwrap();
    // 10 + 30 + 50 = 90 (NULLs ignored)
    let val = r.scalar_f64().expect("f64");
    assert!((val - 90.0).abs() < 0.01, "expected 90.0, got {val}");
}

#[test]
fn avg_ignores_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT avg(val) FROM t").unwrap();
    // (10 + 30 + 50) / 3 = 30 (not / 5 = 18)
    let val = r.scalar_f64().expect("f64");
    assert!((val - 30.0).abs() < 0.01, "expected 30.0, got {val}");
}

#[test]
fn min_ignores_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT min(val) FROM t").unwrap();
    // min of 10,30,50 = 10 (NULLs ignored)
    assert_eq!(r.scalar_u64(), Some(10));
}

#[test]
fn max_ignores_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT max(val) FROM t").unwrap();
    // max of 10,30,50 = 50 (NULLs ignored)
    assert_eq!(r.scalar_u64(), Some(50));
}

#[test]
fn count_distinct_ignores_nulls() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(DISTINCT val) FROM t").unwrap();
    // distinct non-NULL values: 10, 30, 50 → 3
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn count_col_with_where() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(val) FROM t WHERE id > 2").unwrap();
    // id > 2: rows 3,4,5. val is non-NULL for rows 3,5 → 2
    assert_eq!(r.scalar_u64(), Some(2));
}
