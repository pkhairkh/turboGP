//! EXPLAIN, ANALYZE, VACUUM, COPY integration tests (Wave 68).

use tempfile::TempDir;
use turbogp::engine::QueryEngine;

#[test]
fn explain_basic() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)").unwrap();
    let r = e.execute("EXPLAIN SELECT count(*) FROM t WHERE v > 15").unwrap();
    assert_eq!(r.row_count, 1);
    let plan = r.columns[0].string_values.as_ref().expect("plan text");
    assert!(plan[0].contains("SELECT"), "plan must contain the query");
    assert!(plan[0].contains("Table: t"), "plan must mention the table");
}

#[test]
fn analyze_basic() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    let r = e.execute("ANALYZE SELECT count(*) FROM t").unwrap();
    // ANALYZE returns the query result plus an execution_time_ms column.
    assert!(r.columns.len() >= 2, "ANALYZE must return result + timing column");
    let timing_col =
        r.columns.iter().find(|c| c.name == "execution_time_ms").expect("timing column");
    let timing_str = timing_col.string_values.as_ref().unwrap();
    let ms: f64 = timing_str[0].parse().unwrap();
    assert!(ms >= 0.0, "timing must be non-negative");
}

#[test]
fn vacuum_basic() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2)").unwrap();
    e.execute("DELETE FROM t WHERE id = 1").unwrap();
    // VACUUM should not error.
    let r = e.execute("VACUUM");
    assert!(r.is_ok(), "VACUUM must succeed; got: {:?}", r.err());
}

#[test]
fn copy_to_and_from() {
    let tmp = TempDir::new().unwrap();
    let csv_path = tmp.path().join("export.csv");
    let csv_str = csv_path.to_str().unwrap();

    // Create a table and export it.
    {
        let mut e = QueryEngine::new();
        e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)").unwrap();
        let r = e.execute(&format!("COPY t TO '{}'", csv_str)).unwrap();
        assert_eq!(r.row_count, 2, "COPY TO must export 2 rows");
    }

    // Verify the CSV file was created.
    let content = std::fs::read_to_string(csv_str).unwrap();
    assert!(content.contains("id,v"), "CSV must have header");
    assert!(content.contains("1,10"), "CSV must have first row");
    assert!(content.contains("2,20"), "CSV must have second row");

    // Import the CSV into a new table.
    {
        let mut e = QueryEngine::new();
        e.execute("CREATE TABLE t2 (id INT, v INT)").unwrap();
        let r = e.execute(&format!("COPY t2 FROM '{}'", csv_str)).unwrap();
        assert_eq!(r.row_count, 2, "COPY FROM must import 2 rows");
        // Verify the data.
        let r = e.execute("SELECT count(*) FROM t2").unwrap();
        assert_eq!(r.scalar_u64(), Some(2));
    }
}
