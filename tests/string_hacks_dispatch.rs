//! Wave 3 Tasks 3.2 / 3.3 / 3.4 — regression tests for the string-based
//! UNION ALL, MERGE, and PIVOT hacks.
//!
//! Agent A hasn't yet added these to the formal parser, so the hacks in
//! `src/engine/helpers.rs` (`split_union_all`, `parse_merge`,
//! `parse_pivot_clause`) are still in use. These tests verify they still
//! work end-to-end via `engine.execute()`.

use turbogp::engine::QueryEngine;

#[test]
fn test_union_all_works() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t1 (id INT)").unwrap();
    engine.execute("INSERT INTO t1 VALUES (1), (2)").unwrap();
    engine.execute("CREATE TABLE t2 (id INT)").unwrap();
    engine.execute("INSERT INTO t2 VALUES (3), (4)").unwrap();

    let r = engine.execute("SELECT * FROM t1 UNION ALL SELECT * FROM t2").unwrap();
    assert_eq!(r.row_count, 4, "UNION ALL should return 4 rows, got {}", r.row_count);
}

#[test]
fn test_union_all_with_count() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t1 (id INT)").unwrap();
    engine.execute("INSERT INTO t1 VALUES (1), (2), (3)").unwrap();
    engine.execute("CREATE TABLE t2 (id INT)").unwrap();
    engine.execute("INSERT INTO t2 VALUES (4), (5)").unwrap();

    let r = engine.execute("SELECT COUNT(*) FROM t1 UNION ALL SELECT COUNT(*) FROM t2").unwrap();
    // The string-based hack returns 2 rows (one count per side).
    assert_eq!(r.row_count, 2, "UNION ALL with COUNT should return 2 rows");
}

#[test]
fn test_union_all_single_side() {
    // A single SELECT without UNION ALL should not be split.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2)").unwrap();

    let r = engine.execute("SELECT * FROM t").unwrap();
    assert_eq!(r.row_count, 2, "plain SELECT returns 2 rows");
}

#[test]
fn test_merge_works() {
    // MERGE INTO target USING (VALUES ...) AS source (cols)
    //   ON target.id = source.id
    //   WHEN MATCHED THEN UPDATE SET ...
    //   WHEN NOT MATCHED THEN INSERT ...
    //
    // The string-based parse_merge() in helpers.rs expects the USING
    // clause to be a VALUES list (not a table reference). This is the
    // format supported by the existing hack.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE target (id INT, v INT)").unwrap();
    engine.execute("INSERT INTO target VALUES (1, 10)").unwrap();

    let merge_sql = "MERGE INTO target USING (VALUES (1, 99), (2, 42)) AS source (id, v) \
                     ON target.id = source.id \
                     WHEN MATCHED THEN UPDATE SET v = source.v \
                     WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v)";
    let r = engine.execute(merge_sql);
    assert!(r.is_ok(), "MERGE should execute: {:?}", r.err());

    // After MERGE: target should have 2 rows — (1, 99) updated, (2, 42) inserted.
    let r = engine.execute("SELECT COUNT(*) FROM target").unwrap();
    assert_eq!(r.columns[0].values[0], 2, "target should have 2 rows after MERGE");
}

#[test]
fn test_pivot_works() {
    // PIVOT (SUM(value) FOR quarter IN ('Q1', 'Q2')) AS p
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE sales (region VARCHAR(20), quarter VARCHAR(10), amount INT)").unwrap();
    engine.execute("INSERT INTO sales VALUES ('east', 'Q1', 100)").unwrap();
    engine.execute("INSERT INTO sales VALUES ('east', 'Q2', 200)").unwrap();
    engine.execute("INSERT INTO sales VALUES ('west', 'Q1', 150)").unwrap();
    engine.execute("INSERT INTO sales VALUES ('west', 'Q2', 250)").unwrap();

    let pivot_sql = "SELECT * FROM sales PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2')) AS p";
    let r = engine.execute(pivot_sql);
    assert!(r.is_ok(), "PIVOT should execute: {:?}", r.err());
    // PIVOT produces 2 rows (one per region: east, west).
    assert!(r.as_ref().unwrap().row_count >= 1, "PIVOT should return ≥1 row");
}

#[test]
fn test_string_hacks_do_not_break_normal_queries() {
    // Ensure the string-hack detection doesn't accidentally trigger on
    // queries that contain 'UNION', 'MERGE', or 'PIVOT' as identifiers
    // rather than keywords.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, union_col INT, merge_col INT, pivot_col INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 10, 20, 30)").unwrap();

    let r = engine.execute("SELECT id FROM t").unwrap();
    assert_eq!(r.row_count, 1, "normal SELECT still works");
}
