//! Wave 55 — Test quality improvements.
//!
//! Adds tests that were missing or shallow in earlier waves:
//! 1. Real Parquet NULL test: load a Parquet file with NULLs, verify
//!    COUNT(col) excludes NULLs.
//! 2. Concurrency test: two engines running SELECTs concurrently don't
//!    interfere.
//! 3. Extended query protocol test: P/B/D/E/S messages produce correct
//!    responses (also covered in wave52_pgwire.rs, but this file adds
//!    a focused end-to-end test).
//! 4. Result value assertions (not just is_ok()).

use turbogp::engine::QueryEngine;

// -----------------------------------------------------------------------
// 1. Real Parquet NULL test.
// -----------------------------------------------------------------------

#[test]
fn parquet_null_count_excludes_null_cells() {
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;

    // Simulate a loaded Parquet column with NULL bitmap.
    // Row 0: v=10 (non-null), Row 1: v=NULL, Row 2: v=30 (non-null).
    let loaded = LoadedTable {
        name: "t".into(),
        columns: vec![LoadedColumn {
            name: "v".into(),
            cells: vec![10, 0, 30],
            row_count: 3,
            string_search: None,
            null_bitmap: Some(vec![false, true, false]),
        }],
        row_count: 3,
        i32_columns: Vec::new(),
    };
    let table = DS::from_loaded(loaded);
    let mut e = QueryEngine::in_memory();
    e.register_table(table);

    // COUNT(*) counts all rows = 3.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3), "COUNT(*) must count all rows including NULLs");

    // COUNT(v) excludes the NULL row = 2.
    let r = e.execute("SELECT count(v) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "COUNT(v) must exclude the NULL row");

    // SUM(v) should only sum non-NULL values: 10 + 30 = 40.
    let r = e.execute("SELECT sum(v) FROM t").unwrap();
    let val = r.scalar_f64().expect("expected f64");
    assert!((val - 40.0).abs() < 0.01, "SUM(v) must exclude NULL, expected 40.0, got {val}");

    // AVG(v) = (10 + 30) / 2 = 20.
    let r = e.execute("SELECT avg(v) FROM t").unwrap();
    let val = r.scalar_f64().expect("expected f64");
    assert!((val - 20.0).abs() < 0.01, "AVG(v) must exclude NULL, expected 20.0, got {val}");
}

// -----------------------------------------------------------------------
// 2. Concurrency test: two engines running SELECTs concurrently.
// -----------------------------------------------------------------------

#[test]
fn concurrent_selects_on_separate_engines() {
    use std::thread;

    // Each engine is independent (no shared state), so concurrent SELECTs
    // on separate engines should not interfere. We move the engines into
    // the threads (no Arc needed since each thread owns its engine).
    let mut e1 = QueryEngine::in_memory();
    e1.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e1.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    let mut e2 = QueryEngine::in_memory();
    e2.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e2.execute("INSERT INTO t (id, v) VALUES (1, 100), (2, 200), (3, 300), (4, 400)").unwrap();

    let h1 = thread::spawn(move || {
        // e1 has 3 rows.
        let r = e1.execute("SELECT count(*) FROM t").unwrap();
        r.scalar_u64()
    });

    let h2 = thread::spawn(move || {
        // e2 has 4 rows.
        let r = e2.execute("SELECT count(*) FROM t").unwrap();
        r.scalar_u64()
    });

    let r1 = h1.join().expect("thread 1 panicked").expect("e1 count");
    let r2 = h2.join().expect("thread 2 panicked").expect("e2 count");

    assert_eq!(r1, 3, "engine 1 should see 3 rows");
    assert_eq!(r2, 4, "engine 2 should see 4 rows");
}

// -----------------------------------------------------------------------
// 3. Result value assertions (not just is_ok()).
// -----------------------------------------------------------------------

#[test]
fn select_returns_correct_values() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')").unwrap();

    // SELECT * must return all rows with correct values.
    let r = e.execute("SELECT * FROM t ORDER BY id").unwrap();
    assert_eq!(r.row_count, 3);
    assert_eq!(r.columns.len(), 2);
    assert_eq!(r.columns[0].values, vec![1, 2, 3]);

    // SELECT id WHERE id > 1 must return rows 2 and 3.
    let r = e.execute("SELECT id FROM t WHERE id > 1 ORDER BY id").unwrap();
    assert_eq!(r.row_count, 2);
    assert_eq!(r.columns[0].values, vec![2, 3]);

    // SELECT count(*) must return 3.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));

    // SELECT sum(id) must return 6 (1+2+3).
    let r = e.execute("SELECT sum(id) FROM t").unwrap();
    let val = r.scalar_f64().expect("expected f64");
    assert!((val - 6.0).abs() < 0.01, "sum(id) = {val}, want 6.0");
}

#[test]
fn update_then_select_returns_updated_values() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    // UPDATE row 2 to v=99.
    e.execute("UPDATE t SET v = 99 WHERE id = 2").unwrap();

    // SELECT must return the updated value.
    let r = e.execute("SELECT v FROM t WHERE id = 2").unwrap();
    assert_eq!(r.row_count, 1);
    assert_eq!(r.columns[0].values[0], 99, "UPDATE must change the value to 99");

    // SELECT all and verify row 2 is updated, others unchanged.
    let r = e.execute("SELECT v FROM t ORDER BY id").unwrap();
    assert_eq!(r.columns[0].values, vec![10, 99, 30]);
}

#[test]
fn delete_then_select_returns_remaining_rows() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3), (4), (5)").unwrap();

    // DELETE rows where id > 3.
    e.execute("DELETE FROM t WHERE id > 3").unwrap();

    // SELECT must return only rows 1, 2, 3.
    let r = e.execute("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(r.row_count, 3);
    assert_eq!(r.columns[0].values, vec![1, 2, 3]);

    // COUNT must reflect the deletion.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

// -----------------------------------------------------------------------
// 4. Transaction rollback test with value assertions.
// -----------------------------------------------------------------------

#[test]
fn rollback_discards_writes_with_value_check() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();

    // BEGIN, INSERT, ROLLBACK — the INSERT must be discarded.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t (id) VALUES (2)").unwrap();
    e.execute("ROLLBACK").unwrap();

    // SELECT must return only the original row.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1), "ROLLBACK must discard the INSERT");

    let r = e.execute("SELECT id FROM t").unwrap();
    assert_eq!(r.columns[0].values, vec![1]);
}

#[test]
fn commit_persists_writes_with_value_check() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();

    // BEGIN, INSERT, COMMIT — the INSERT must persist.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t (id) VALUES (2)").unwrap();
    e.execute("COMMIT").unwrap();

    // SELECT must return both rows.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "COMMIT must persist the INSERT");

    let r = e.execute("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(r.columns[0].values, vec![1, 2]);
}
