//! Wave 60 — Basic parser features: CASE WHEN, HAVING, UNION ALL, DISTINCT.
//!
//! These features were previously only available through the tpch fallback
//! interpreter (213x slower than DuckDB). Wave 60 adds them to the basic
//! parser so they go through the fast dispatch path (or at least don't
//! require the tpch parser to recognize them).

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, dept INT, salary INT)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (1, 1, 100)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (2, 1, 200)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (3, 1, 300)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (4, 2, 150)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (5, 2, 250)").unwrap();
    e.execute("INSERT INTO t (id, dept, salary) VALUES (6, 3, 350)").unwrap();
    e
}

// -----------------------------------------------------------------------
// Wave 60a: CASE WHEN
// -----------------------------------------------------------------------

#[test]
fn case_when_in_select() {
    let mut e = make_engine();
    // CASE WHEN salary > 200 THEN 1 ELSE 0 END
    // Rows: 1(100→0), 2(200→0), 3(300→1), 4(150→0), 5(250→1), 6(350→1)
    let r = e.execute("SELECT CASE WHEN salary > 200 THEN 1 ELSE 0 END FROM t").unwrap();
    assert_eq!(r.row_count, 6, "CASE WHEN must return one row per input row");
    let col = &r.columns[0];
    assert_eq!(col.values.len(), 6);
    assert_eq!(col.values[0], 0, "salary=100 → 0");
    assert_eq!(col.values[1], 0, "salary=200 → 0 (not > 200)");
    assert_eq!(col.values[2], 1, "salary=300 → 1");
    assert_eq!(col.values[3], 0, "salary=150 → 0");
    assert_eq!(col.values[4], 1, "salary=250 → 1");
    assert_eq!(col.values[5], 1, "salary=350 → 1");
}

#[test]
fn case_when_in_where() {
    let mut e = make_engine();
    // WHERE CASE WHEN salary > 200 THEN 1 ELSE 0 END = 1
    // Matches rows where salary > 200: rows 3, 5, 6 → 3 rows.
    let r = e
        .execute("SELECT count(*) FROM t WHERE CASE WHEN salary > 200 THEN 1 ELSE 0 END = 1")
        .unwrap();
    assert_eq!(r.scalar_u64(), Some(3), "CASE WHEN in WHERE must match 3 rows (salary > 200)");
}

#[test]
fn case_when_with_multiple_when_clauses() {
    let mut e = make_engine();
    // CASE WHEN salary > 300 THEN 3 WHEN salary > 200 THEN 2 ELSE 1 END
    // Rows: 1(100→1), 2(200→1), 3(300→1, not > 300, not > 200), 4(150→1), 5(250→2), 6(350→3)
    let r = e
        .execute("SELECT CASE WHEN salary > 300 THEN 3 WHEN salary > 200 THEN 2 ELSE 1 END FROM t")
        .unwrap();
    assert_eq!(r.row_count, 6);
    let col = &r.columns[0];
    assert_eq!(col.values[0], 1, "salary=100 → 1");
    assert_eq!(col.values[1], 1, "salary=200 → 1");
    assert_eq!(col.values[2], 2, "salary=300 → 2 (not > 300, but > 200)");
    assert_eq!(col.values[3], 1, "salary=150 → 1");
    assert_eq!(col.values[4], 2, "salary=250 → 2 (> 200)");
    assert_eq!(col.values[5], 3, "salary=350 → 3 (> 300)");
}

// -----------------------------------------------------------------------
// Wave 60b: HAVING
// -----------------------------------------------------------------------

#[test]
fn group_by_with_having() {
    let mut e = make_engine();
    // GROUP BY dept HAVING count(*) > 2
    // Dept 1 has 3 rows (> 2), dept 2 has 2 rows (not > 2), dept 3 has 1 row (not > 2).
    // Result: only dept 1.
    let r = e.execute("SELECT dept, count(*) FROM t GROUP BY dept HAVING count(*) > 2").unwrap();
    assert_eq!(r.row_count, 1, "HAVING count(*) > 2 must filter to only dept 1");
    let dept_col = r.columns.iter().find(|c| c.name == "dept").expect("dept column");
    assert_eq!(dept_col.values[0], 1, "the surviving group must be dept 1");
}

#[test]
fn group_by_with_having_on_sum() {
    let mut e = make_engine();
    // GROUP BY dept HAVING sum(salary) > 400
    // Dept 1: 100+200+300 = 600 (> 400). Dept 2: 150+250 = 400 (NOT > 400). Dept 3: 350 (not > 400).
    // Result: only dept 1.
    let r = e.execute("SELECT dept FROM t GROUP BY dept HAVING sum(salary) > 400").unwrap();
    assert_eq!(r.row_count, 1, "HAVING sum(salary) > 400 must filter to only dept 1");
    let dept_col = r.columns.iter().find(|c| c.name == "dept").expect("dept column");
    assert_eq!(dept_col.values[0], 1, "the surviving group must be dept 1 (sum=600)");
}

// -----------------------------------------------------------------------
// Wave 60c: UNION ALL
// -----------------------------------------------------------------------

#[test]
fn union_all_two_selects() {
    let mut e = make_engine();
    // SELECT 1 UNION ALL SELECT 2
    let r = e.execute("SELECT 1 UNION ALL SELECT 2").unwrap();
    assert_eq!(r.row_count, 2, "UNION ALL of two single-row SELECTs must return 2 rows");
    assert_eq!(r.columns[0].values.len(), 2);
    // The values should be 1 and 2 (order may vary, but typically left then right).
    assert!(r.columns[0].values.contains(&1), "must contain 1");
    assert!(r.columns[0].values.contains(&2), "must contain 2");
}

#[test]
fn union_all_three_selects() {
    let mut e = make_engine();
    // SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
    let r = e.execute("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3").unwrap();
    assert_eq!(r.row_count, 3, "UNION ALL of three single-row SELECTs must return 3 rows");
    for v in &[1u64, 2, 3] {
        assert!(r.columns[0].values.contains(v), "must contain {}", v);
    }
}

#[test]
fn union_all_with_table_data() {
    let mut e = make_engine();
    // SELECT id FROM t WHERE dept = 1 UNION ALL SELECT id FROM t WHERE dept = 3
    // Dept 1: ids 1, 2, 3. Dept 3: id 6. Total: 4 rows.
    let r = e
        .execute("SELECT id FROM t WHERE dept = 1 UNION ALL SELECT id FROM t WHERE dept = 3")
        .unwrap();
    assert_eq!(
        r.row_count, 4,
        "UNION ALL must concatenate 3 rows from dept=1 and 1 row from dept=3"
    );
    let id_col = &r.columns[0];
    // Should contain ids 1, 2, 3 (from dept=1) and 6 (from dept=3).
    assert!(id_col.values.contains(&1), "must contain id=1");
    assert!(id_col.values.contains(&2), "must contain id=2");
    assert!(id_col.values.contains(&3), "must contain id=3");
    assert!(id_col.values.contains(&6), "must contain id=6");
}

// -----------------------------------------------------------------------
// Wave 60d: SELECT DISTINCT
// -----------------------------------------------------------------------

#[test]
fn select_distinct_single_column() {
    let mut e = make_engine();
    // SELECT DISTINCT dept FROM t
    // Depts: 1, 1, 1, 2, 2, 3 → distinct: 1, 2, 3
    let r = e.execute("SELECT DISTINCT dept FROM t").unwrap();
    assert_eq!(r.row_count, 3, "DISTINCT must collapse 6 rows to 3 unique depts");
    let dept_col = &r.columns[0];
    assert!(dept_col.values.contains(&1), "must contain dept=1");
    assert!(dept_col.values.contains(&2), "must contain dept=2");
    assert!(dept_col.values.contains(&3), "must contain dept=3");
}

#[test]
fn select_distinct_multi_column() {
    let mut e = make_engine();
    // SELECT DISTINCT dept, salary FROM t
    // All 6 rows have unique (dept, salary) pairs → no duplicates removed.
    let r = e.execute("SELECT DISTINCT dept, salary FROM t").unwrap();
    assert_eq!(r.row_count, 6, "all (dept, salary) pairs are unique → 6 rows");
}

#[test]
fn select_distinct_with_duplicates() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE dups (v INT)").unwrap();
    e.execute("INSERT INTO dups (v) VALUES (1), (1), (2), (2), (2), (3)").unwrap();
    // SELECT DISTINCT v FROM dups → 1, 2, 3 (3 rows).
    let r = e.execute("SELECT DISTINCT v FROM dups").unwrap();
    assert_eq!(r.row_count, 3, "DISTINCT must collapse 6 rows to 3 unique values");
    let v_col = &r.columns[0];
    assert!(v_col.values.contains(&1), "must contain 1");
    assert!(v_col.values.contains(&2), "must contain 2");
    assert!(v_col.values.contains(&3), "must contain 3");
}
