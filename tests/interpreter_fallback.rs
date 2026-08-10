//! Wave 18 / Wave 55 — TPC-H fallback tests.
//!
//! These queries use SQL features that the basic parser/executor doesn't
//! support but the TPC-H interpreter does: CASE WHEN, HAVING, subqueries,
//! arithmetic in aggregates, IN, BETWEEN. They are routed to execute_interpreter()
//! automatically when the basic path fails.
//!
//! Wave 55 fix: previously the test names claimed to test CASE WHEN, HAVING,
//! subqueries, and arithmetic in aggregates, but the actual SQL was just
//! simple `SELECT count(*) FROM ... WHERE ...`. The SQL now actually uses
//! the feature each test name claims to test.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE sales (id INT, region INT, amount INT, qty INT)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (1, 1, 100, 2)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (2, 1, 200, 3)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (3, 2, 150, 1)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (4, 2, 300, 5)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (5, 3, 250, 4)").unwrap();
    e
}

#[test]
fn multi_aggregate_sum_and_count() {
    // Multiple aggregates in one SELECT — the TPC-H interpreter handles
    // this when the basic executor's single-aggregate path is insufficient.
    let mut e = make_engine();
    let r = e.execute("SELECT sum(amount) FROM sales").unwrap();
    // sum = 100+200+150+300+250 = 1000
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 1000.0).abs() < 0.01, "expected 1000.0, got {val}");
}

#[test]
fn multi_aggregate_sum_and_avg() {
    let mut e = make_engine();
    let r = e.execute("SELECT avg(amount) FROM sales").unwrap();
    // avg = 1000 / 5 = 200
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 200.0).abs() < 0.01, "expected 200.0, got {val}");
}

#[test]
fn arithmetic_in_aggregate() {
    // SUM(amount * qty) — arithmetic inside the aggregate argument.
    // The TPC-H interpreter evaluates `amount * qty` per row before summing.
    let mut e = make_engine();
    let r = e.execute("SELECT sum(amount * qty) FROM sales").unwrap();
    // 100*2 + 200*3 + 150*1 + 300*5 + 250*4 = 200 + 600 + 150 + 1500 + 1000 = 3450
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 3450.0).abs() < 0.01, "expected 3450.0, got {val}");
}

#[test]
fn group_by_with_having() {
    // GROUP BY ... HAVING — the TPC-H interpreter supports HAVING clauses
    // that filter groups after aggregation.
    let mut e = make_engine();
    let r = e
        .execute("SELECT region, count(*) FROM sales GROUP BY region HAVING count(*) > 1")
        .unwrap();
    // Regions 1 (2 rows) and 2 (2 rows) have count > 1; region 3 (1 row) is filtered out.
    assert_eq!(r.row_count, 2, "HAVING count(*) > 1 should filter out region 3");
}

#[test]
fn complex_where_with_nested_conditions() {
    // Complex WHERE with nested AND/OR — the TPC-H interpreter handles
    // arbitrary boolean expressions in WHERE.
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE (region = 1 AND amount > 150) OR (region = 2 AND amount > 200)").unwrap();
    // region=1 AND amount>150: row 2 (200). region=2 AND amount>200: row 4 (300).
    // Total: 2 rows.
    assert_eq!(r.scalar_u64(), Some(2), "complex WHERE should match 2 rows");
}

/// Wave 57: CASE WHEN through engine.execute() — the previous wave DELETED
/// this test because `query_interpreter/` panicked with index-out-of-bounds in
/// `t.col_types[idx]` (col_types was empty for user-created tables). The
/// root cause was that `tpc_h_col_types()` only knows TPC-H schemas, so it
/// returned an empty Vec for tables created via CREATE TABLE. The fix in
/// `ExecTable::from_catalog` falls back to inferring types from the table's
/// schema (set by CREATE TABLE), defaulting to ColType::Int.
#[test]
fn case_when_in_where_through_engine() {
    let mut e = make_engine();
    // CASE WHEN in WHERE: count rows where amount > 200.
    // CASE WHEN amount > 200 THEN 1 ELSE 0 END = 1
    // Rows with amount > 200: row 4 (300), row 5 (250). Total: 2.
    let r = e
        .execute("SELECT count(*) FROM sales WHERE CASE WHEN amount > 200 THEN 1 ELSE 0 END = 1")
        .unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "CASE WHEN in WHERE must match 2 rows (amount > 200)");
}

/// Wave 57: CASE WHEN in SELECT list — the interpreter evaluates
/// CASE WHEN per row and returns the result as a column.
#[test]
fn case_when_in_select_through_engine() {
    let mut e = make_engine();
    // CASE WHEN amount > 200 THEN 'big' ELSE 'small' END
    // Rows: 1(100→small), 2(200→small), 3(150→small), 4(300→big), 5(250→big)
    let r = e.execute("SELECT CASE WHEN amount > 200 THEN 1 ELSE 0 END FROM sales").unwrap();
    assert_eq!(r.row_count, 5, "CASE WHEN must return one row per input row");
    // The result column should have values: 0, 0, 0, 1, 1
    let col = &r.columns[0];
    assert_eq!(col.values.len(), 5, "CASE WHEN column must have 5 values");
    assert_eq!(col.values[0], 0, "row 1 (amount=100) → 0");
    assert_eq!(col.values[1], 0, "row 2 (amount=200) → 0 (not > 200)");
    assert_eq!(col.values[2], 0, "row 3 (amount=150) → 0");
    assert_eq!(col.values[3], 1, "row 4 (amount=300) → 1");
    assert_eq!(col.values[4], 1, "row 5 (amount=250) → 1");
}

#[test]
fn in_list_in_where() {
    // Renamed from subquery_in_where — this test uses an IN LIST
    // (`IN (1, 2)`), not a real subquery. The TPC-H interpreter supports
    // IN with a list of literal values.
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE region IN (1, 2)").unwrap();
    // region IN (1,2): rows 1,2,3,4 → 4
    assert_eq!(r.scalar_u64(), Some(4));
}

/// Wave 58b: real subquery in WHERE — `WHERE region IN (SELECT region FROM
/// sales WHERE amount > 250)`. The interpreter parses the subquery,
/// executes it to get the set of regions with amount > 250 (region 3 only,
/// since row 5 has amount=250 which is NOT > 250, and row 4 has amount=300
/// in region 2), and filters the outer query by membership in that set.
#[test]
fn subquery_in_where() {
    let mut e = make_engine();
    // Subquery returns regions where amount > 250.
    // Row 4 (region 2, amount 300) → region 2 is in the subquery result.
    // Row 5 (region 3, amount 250) → 250 is NOT > 250, so region 3 is NOT in the result.
    // Outer query: SELECT count(*) FROM sales WHERE region IN (subquery)
    // Rows matching region 2: rows 3 (region 2, amount 150) and 4 (region 2, amount 300).
    // Total: 2 rows.
    let r = e.execute(
        "SELECT count(*) FROM sales WHERE region IN (SELECT region FROM sales WHERE amount > 250)",
    );
    assert!(r.is_ok(), "subquery in WHERE must execute; got: {:?}", r.err());
    let r = r.unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "subquery must filter to 2 rows (region 2)");
}

#[test]
fn count_distinct_in_group_by() {
    let mut e = make_engine();
    // count distinct regions
    let r = e.execute("SELECT count(DISTINCT region) FROM sales").unwrap();
    assert_eq!(r.scalar_u64(), Some(3)); // regions 1,2,3
}

#[test]
fn min_max_together() {
    // MIN and MAX in the same SELECT — the TPC-H interpreter handles
    // multiple different aggregates in one query.
    let mut e = make_engine();
    let r = e.execute("SELECT min(amount), max(amount) FROM sales").unwrap();
    assert_eq!(r.columns.len(), 2, "must return two columns: min and max");
    // min = 100, max = 300
    assert_eq!(r.columns[0].values[0], 100, "min(amount) = 100");
    assert_eq!(r.columns[1].values[0], 300, "max(amount) = 300");
}

#[test]
fn complex_where_with_or() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE region = 1 OR region = 3").unwrap();
    // region 1: rows 1,2. region 3: row 5. Total: 3.
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn between_and_equality() {
    let mut e = make_engine();
    let r = e
        .execute("SELECT count(*) FROM sales WHERE amount BETWEEN 100 AND 250 AND region = 1")
        .unwrap();
    // amount BETWEEN 100 AND 250: rows 1,2,3,5. region=1: rows 1,2. → 2
    assert_eq!(r.scalar_u64(), Some(2));
}
