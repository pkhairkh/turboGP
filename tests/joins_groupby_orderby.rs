//! Wave 49 — End-to-end (engine.execute) tests for the three bugs fixed in
//! this wave:
//!
//! 1. LEFT JOIN silently executed as INNER JOIN — now the parsed join type
//!    is propagated to `hash_join`, so unmatched left rows survive.
//! 2. Single-key GROUP BY dropped all but the first aggregate — the loop
//!    `return Ok(result)` is gone, so `SELECT grp, sum(a), count(*), avg(b)`
//!    returns four columns.
//! 3. SelectMulti + ORDER BY returned rows in scan order — the dispatcher
//!    now sorts the row indices by the ORDER BY column before applying
//!    LIMIT.

use turbogp::engine::QueryEngine;

// -----------------------------------------------------------------------
// Bug 1: LEFT JOIN preserves unmatched left rows.
// -----------------------------------------------------------------------

#[test]
fn left_join_keeps_unmatched_left_rows() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE orders (id INT, cust INT)").unwrap();
    e.execute("CREATE TABLE returns (oid INT, reason INT)").unwrap();
    e.execute("INSERT INTO orders (id, cust) VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    // Only one matching return for order 1.
    e.execute("INSERT INTO returns (oid, reason) VALUES (1, 99)").unwrap();

    let r = e
        .execute("SELECT * FROM orders o LEFT JOIN returns r ON o.id = r.oid")
        .expect("LEFT JOIN must execute");
    // All three orders must be returned; rows 2 and 3 have no match.
    assert_eq!(r.row_count, 3, "LEFT JOIN must preserve all left rows; got {}", r.row_count);
}

#[test]
fn inner_join_drops_unmatched_left_rows() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE orders (id INT, cust INT)").unwrap();
    e.execute("CREATE TABLE returns (oid INT, reason INT)").unwrap();
    e.execute("INSERT INTO orders (id, cust) VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    e.execute("INSERT INTO returns (oid, reason) VALUES (1, 99)").unwrap();

    let r = e
        .execute("SELECT * FROM orders o INNER JOIN returns r ON o.id = r.oid")
        .expect("INNER JOIN must execute");
    // Only the matched row should be returned.
    assert_eq!(r.row_count, 1, "INNER JOIN must drop unmatched rows; got {}", r.row_count);
}

#[test]
fn left_join_count_returns_zero_for_unmatched() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE o (id INT)").unwrap();
    e.execute("CREATE TABLE r (oid INT)").unwrap();
    e.execute("INSERT INTO o (id) VALUES (1), (2), (3), (4)").unwrap();
    // No matching rows on the right side.
    let r = e.execute("SELECT count(*) FROM o LEFT JOIN r ON o.id = r.oid").unwrap();
    assert_eq!(r.scalar_u64(), Some(4), "LEFT JOIN with no matches must still count all left rows");
}

// -----------------------------------------------------------------------
// Bug 2: Single-key GROUP BY emits every aggregate in the SELECT list.
// -----------------------------------------------------------------------

#[test]
fn group_by_emits_multiple_aggregates() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (grp INT, a INT, b INT)").unwrap();
    e.execute("INSERT INTO t (grp, a, b) VALUES (1, 10, 100), (1, 20, 200), (2, 30, 300)").unwrap();

    let r = e.execute("SELECT grp, sum(a), count(*), avg(b) FROM t GROUP BY grp").unwrap();
    assert_eq!(
        r.columns.len(),
        4,
        "must emit 4 columns: grp, sum, count, avg; got {}",
        r.columns.len()
    );
    assert_eq!(r.row_count, 2, "must emit 2 groups");
}

#[test]
fn group_by_sum_count_values() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (grp INT, a INT)").unwrap();
    e.execute("INSERT INTO t (grp, a) VALUES (1, 10), (1, 20), (1, 30), (2, 40), (2, 50)").unwrap();

    let r = e.execute("SELECT grp, sum(a), count(*) FROM t GROUP BY grp ORDER BY grp").unwrap();
    // group 1 → sum=60, count=3
    // group 2 → sum=90, count=2
    assert_eq!(r.columns.len(), 3);
    assert_eq!(r.row_count, 2);

    // Column 0: grp (1, 2)
    assert_eq!(r.columns[0].values[0], 1);
    assert_eq!(r.columns[0].values[1], 2);

    // Column 1: sum — stored as f64 bits.
    let sum0 = f64::from_bits(r.columns[1].values[0]);
    let sum1 = f64::from_bits(r.columns[1].values[1]);
    assert!((sum0 - 60.0).abs() < 0.01, "sum(group 1) = {sum0}, want 60");
    assert!((sum1 - 90.0).abs() < 0.01, "sum(group 2) = {sum1}, want 90");

    // Column 2: count(*) — stored as plain u64.
    assert_eq!(r.columns[2].values[0], 3);
    assert_eq!(r.columns[2].values[1], 2);
}

// -----------------------------------------------------------------------
// Bug 3: SelectMulti + ORDER BY returns rows sorted by the ORDER BY column.
// -----------------------------------------------------------------------

#[test]
fn select_multi_order_by_sorts_results() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    // Insert rows out of order.
    e.execute("INSERT INTO t (a, b) VALUES (3, 30), (1, 10), (2, 20)").unwrap();

    let r = e.execute("SELECT a, b FROM t ORDER BY a").unwrap();
    assert_eq!(r.row_count, 3);
    assert_eq!(r.columns[0].values[0], 1, "first row a must be 1");
    assert_eq!(r.columns[0].values[1], 2, "second row a must be 2");
    assert_eq!(r.columns[0].values[2], 3, "third row a must be 3");

    // b must follow a (since (a, b) pairs are (1,10), (2,20), (3,30)).
    assert_eq!(r.columns[1].values[0], 10);
    assert_eq!(r.columns[1].values[1], 20);
    assert_eq!(r.columns[1].values[2], 30);
}

#[test]
fn select_multi_order_by_desc() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    e.execute("INSERT INTO t (a, b) VALUES (1, 10), (3, 30), (2, 20)").unwrap();

    let r = e.execute("SELECT a, b FROM t ORDER BY a DESC").unwrap();
    assert_eq!(r.row_count, 3);
    assert_eq!(r.columns[0].values[0], 3);
    assert_eq!(r.columns[0].values[1], 2);
    assert_eq!(r.columns[0].values[2], 1);
}

#[test]
fn select_multi_order_by_with_limit() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    e.execute("INSERT INTO t (a, b) VALUES (5, 50), (1, 10), (3, 30), (2, 20), (4, 40)").unwrap();

    let r = e.execute("SELECT a, b FROM t ORDER BY a LIMIT 2").unwrap();
    assert_eq!(r.row_count, 2, "LIMIT 2 must return 2 rows");
    assert_eq!(r.columns[0].values[0], 1, "smallest a must come first");
    assert_eq!(r.columns[0].values[1], 2, "second-smallest a must come second");
}
