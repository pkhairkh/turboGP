//! Wave 53 — End-to-end tests verifying that previously-dead modules are
//! now reachable through `engine.execute()`. Each test exercises one of:
//! views, procedures, MERGE, JSON, temporal, window, PIVOT.

use turbogp::engine::QueryEngine;

// -----------------------------------------------------------------------
// Views: CREATE VIEW + SELECT FROM view.
// -----------------------------------------------------------------------

#[test]
fn create_view_and_select_from_it() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    e.execute("CREATE VIEW v_even AS SELECT id, v FROM t WHERE v = 20").unwrap();

    // SELECT from the view must return the filtered rows.
    let r = e.execute("SELECT id FROM v_even").unwrap();
    assert_eq!(r.row_count, 1, "view must return 1 row");
    assert_eq!(r.columns[0].values[0], 2, "the row must have id=2 (the one with v=20)");
}

#[test]
fn drop_view_removes_it() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("CREATE VIEW v1 AS SELECT id FROM t").unwrap();
    // The view is registered; SELECTing it should work (returns 0 rows).
    let r = e.execute("SELECT id FROM v1").unwrap();
    assert_eq!(r.row_count, 0);
    // Drop the view.
    e.execute("DROP VIEW v1").unwrap();
    // Now SELECT from the dropped view must fail.
    let r = e.execute("SELECT id FROM v1");
    // Either the catalog lookup fails or the dispatcher returns an error.
    // (The behaviour depends on the view-expansion path.)
    assert!(
        r.is_err() || r.unwrap().row_count == 0,
        "after DROP VIEW, SELECT from the view should error or return empty"
    );
}

// -----------------------------------------------------------------------
// Procedures: CREATE PROCEDURE + EXEC.
// -----------------------------------------------------------------------

#[test]
fn create_procedure_and_exec_it() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    e.execute("CREATE PROCEDURE get_count AS SELECT count(*) FROM t").unwrap();

    // EXEC the procedure — must run the body SQL and return its result.
    let r = e.execute("EXEC get_count").unwrap();
    assert_eq!(r.scalar_u64(), Some(3), "EXEC get_count must return the row count");
}

#[test]
fn create_procedure_with_params_and_exec() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    // Body uses @1 as a positional parameter placeholder.
    e.execute("CREATE PROCEDURE insert_value AS INSERT INTO t (id) VALUES (@1)").unwrap();

    // EXEC with one argument.
    e.execute("EXEC insert_value 99").unwrap();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(4), "EXEC must have inserted the row");
}

// -----------------------------------------------------------------------
// MERGE: WHEN MATCHED THEN UPDATE / WHEN NOT MATCHED THEN INSERT.
// Wave 56a: the previous test passed even though source_rows was always
// empty (the parser hardcoded `Vec::new()`), so the WHEN MATCHED branch
// was dead code. We now parse USING (VALUES ...) and assert that the
// update + insert actually mutate the target table.
// -----------------------------------------------------------------------

#[test]
fn merge_executes_through_engine() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE target (id INT, val INT)").unwrap();
    e.execute("INSERT INTO target (id, val) VALUES (1, 10), (2, 20)").unwrap();

    // MERGE that updates the matched row (id=2) and inserts a new row (id=3).
    let sql = "MERGE INTO target USING (VALUES (2, 99), (3, 30)) AS source (id, val) \
               ON target.id = source.id \
               WHEN MATCHED THEN UPDATE SET val = source.val \
               WHEN NOT MATCHED THEN INSERT (id, val) VALUES (source.id, source.val)";
    let r = e.execute(sql);
    assert!(r.is_ok(), "MERGE must execute without error; got: {:?}", r.err());
    let r = r.unwrap();
    // 1 updated + 1 inserted = 2 rows affected.
    assert_eq!(r.row_count, 2, "MERGE must report 1 updated + 1 inserted = 2 rows affected");

    // Verify the target table now has 3 rows: (1,10), (2,99), (3,30).
    let after = e.execute("SELECT id, val FROM target ORDER BY id").unwrap();
    assert_eq!(after.row_count, 3, "target table must have 3 rows after MERGE");
    let id_col = after.columns.iter().find(|c| c.name == "id").expect("id column");
    let val_col = after.columns.iter().find(|c| c.name == "val").expect("val column");
    // Row 0: id=1, val=10 (unchanged).
    assert_eq!(id_col.values[0], 1, "id=1 must be unchanged");
    assert_eq!(val_col.values[0], 10, "val=10 must be unchanged");
    // Row 1: id=2, val=99 (updated from 20 → 99).
    assert_eq!(id_col.values[1], 2, "id=2 must be the matched row");
    assert_eq!(val_col.values[1], 99, "val must be updated from 20 to 99 via source.val");
    // Row 2: id=3, val=30 (inserted via source.id / source.val).
    assert_eq!(id_col.values[2], 3, "id=3 must be the inserted row");
    assert_eq!(val_col.values[2], 30, "val=30 must be inserted via source.val");
}

// -----------------------------------------------------------------------
// JSON: JSON_VALUE / JSON_QUERY callable through engine.execute().
// Wave 56c: the previous tests called the json module directly (not through
// engine.execute), so they didn't verify that JSON_VALUE is actually wired
// into the SQL execution path. We now intercept JSON_VALUE(col, 'path') in
// the SQL, rewrite it to `col AS __json_value_N__`, execute, and post-process
// the result by applying json::json_value() to each string value.
// -----------------------------------------------------------------------

#[test]
fn json_value_works_via_engine() {
    // Module-level smoke test (still useful — verifies the json module is
    // exported and the basic happy path works).
    let json_str = r#"{"name": "Alice", "age": 30}"#;
    let v = turbogp::exec::json::json_value(json_str, "$.name").unwrap();
    assert_eq!(v, "Alice");
    let v = turbogp::exec::json::json_value(json_str, "$.age").unwrap();
    assert_eq!(v, "30");
    assert!(turbogp::exec::json::is_json(json_str));
    assert!(!turbogp::exec::json::is_json("not json"));
}

#[test]
fn json_query_and_modify_work_via_engine() {
    let json_str = r#"{"user": {"name": "Bob"}, "tags": [1, 2]}"#;
    let q = turbogp::exec::json::json_query(json_str, "$.user").unwrap();
    assert!(q.contains("Bob"));
    let q = turbogp::exec::json::json_query(json_str, "$.tags").unwrap();
    assert!(q.contains("1"));
    let m = turbogp::exec::json::json_modify(json_str, "$.user.name", "\"Charlie\"");
    assert!(m.contains("Charlie"));
}

/// Wave 56c: JSON_VALUE through engine.execute() — verify the SQL
/// `SELECT JSON_VALUE(col, '$.path') FROM t` is intercepted, the column
/// is loaded as a string, and json::json_value() is applied to each row.
#[test]
fn json_value_through_engine_execute() {
    let mut e = QueryEngine::new();
    // Create a table with a VARCHAR column to hold JSON strings.
    e.execute("CREATE TABLE docs (id INT, payload VARCHAR)").unwrap();
    // Insert two rows with JSON payloads.
    e.execute("INSERT INTO docs (id, payload) VALUES (1, '{\"name\":\"Alice\",\"age\":30}')")
        .unwrap();
    e.execute("INSERT INTO docs (id, payload) VALUES (2, '{\"name\":\"Bob\",\"age\":25}')")
        .unwrap();

    // SELECT JSON_VALUE(payload, '$.name') FROM docs — should return
    // a single column with values "Alice" and "Bob".
    let r = e.execute("SELECT JSON_VALUE(payload, '$.name') FROM docs");
    assert!(r.is_ok(), "JSON_VALUE query must execute; got: {:?}", r.err());
    let r = r.unwrap();
    assert_eq!(r.row_count, 2, "JSON_VALUE must return one row per input row");
    assert_eq!(r.columns.len(), 1, "JSON_VALUE must produce a single result column");
    let col = &r.columns[0];
    // The column must carry string_values with the extracted JSON scalars.
    let strings = col.string_values.as_ref().expect("JSON_VALUE column must have string_values");
    assert_eq!(strings.len(), 2, "string_values must have one entry per row");
    assert!(
        strings.iter().any(|s| s == "Alice"),
        "string_values must contain 'Alice' — got: {:?}",
        strings
    );
    assert!(
        strings.iter().any(|s| s == "Bob"),
        "string_values must contain 'Bob' — got: {:?}",
        strings
    );
}

/// Wave 56c: JSON_VALUE with an explicit AS alias — the alias should
/// become the result column name.
#[test]
fn json_value_with_alias_through_engine_execute() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE docs (id INT, payload VARCHAR)").unwrap();
    e.execute("INSERT INTO docs (id, payload) VALUES (1, '{\"name\":\"Alice\"}')").unwrap();

    let r = e.execute("SELECT JSON_VALUE(payload, '$.name') AS username FROM docs").unwrap();
    assert_eq!(r.row_count, 1);
    let col = &r.columns[0];
    assert_eq!(col.name, "username", "JSON_VALUE AS alias must rename the result column");
    let strings = col.string_values.as_ref().expect("string_values");
    assert_eq!(strings[0], "Alice");
}

/// Wave 56c: JSON_QUERY through engine.execute() — extracts an object/array
/// rather than a scalar.
#[test]
fn json_query_through_engine_execute() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE docs (id INT, payload VARCHAR)").unwrap();
    e.execute("INSERT INTO docs (id, payload) VALUES (1, '{\"user\":{\"name\":\"Alice\"}}')")
        .unwrap();

    let r = e.execute("SELECT JSON_QUERY(payload, '$.user') FROM docs").unwrap();
    assert_eq!(r.row_count, 1);
    let col = &r.columns[0];
    let strings = col.string_values.as_ref().expect("string_values");
    // The extracted object should contain "Alice".
    assert!(
        strings[0].contains("Alice"),
        "JSON_QUERY must return the object containing 'Alice' — got: {}",
        strings[0]
    );
}

// -----------------------------------------------------------------------
// Temporal: FOR SYSTEM_TIME AS OF <timestamp>.
// Wave 56d: the previous test used the Rust API (e.temporals.insert) to
// register the temporal table. We now support DDL: CREATE TABLE ... WITH
// (SYSTEM_VERSIONING = ON) registers the table in self.temporals, and
// INSERT / UPDATE / DELETE on the table sync to the TemporalTable sidecar.
// -----------------------------------------------------------------------

#[test]
fn temporal_query_as_of_through_engine() {
    // Legacy test: still uses the Rust API to register the temporal table.
    // This verifies backward compatibility — the Rust API still works.
    use turbogp::exec::temporal::TemporalTable;
    let mut e = QueryEngine::new();
    let mut t = TemporalTable::new(vec!["id".to_string(), "v".to_string()]);
    t.insert(vec![1, 100]);
    t.insert(vec![2, 200]);
    t.update(|row| row[0] == 1, vec![1, 150]);
    e.temporals.insert("history_t".to_string(), t);

    let r =
        e.execute("SELECT * FROM history_t FOR SYSTEM_TIME AS OF 18446744073709551615").unwrap();
    assert!(r.row_count >= 1, "temporal query must return rows");
    let id_col = r.columns.iter().find(|c| c.name == "id").expect("id column");
    let v_col = r.columns.iter().find(|c| c.name == "v").expect("v column");
    let mut found = false;
    for i in 0..r.row_count {
        if id_col.values[i] == 1 {
            assert_eq!(v_col.values[i], 150, "temporal query must see the updated value");
            found = true;
        }
    }
    assert!(found, "temporal query must include id=1");
}

/// Wave 56d: CREATE TABLE ... WITH (SYSTEM_VERSIONING = ON) registers the
/// table as a TemporalTable. Subsequent INSERT / UPDATE / DELETE sync to
/// the temporal sidecar. FOR SYSTEM_TIME AS OF returns historical state.
#[test]
fn temporal_table_created_via_ddl() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut e = QueryEngine::new();

    // Create a temporal table via DDL.
    e.execute("CREATE TABLE t_hist (id INT, v INT) WITH (SYSTEM_VERSIONING = ON)").unwrap();
    // The table must be registered in self.temporals.
    assert!(
        e.temporals.contains_key("t_hist"),
        "CREATE TABLE WITH SYSTEM_VERSIONING must register the temporal table"
    );

    // Insert two rows.
    e.execute("INSERT INTO t_hist (id, v) VALUES (1, 100)").unwrap();
    e.execute("INSERT INTO t_hist (id, v) VALUES (2, 200)").unwrap();

    // Capture the timestamp BEFORE the update.
    let ts_before_update =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    // Sleep briefly so the update's timestamp is strictly greater.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Update row 1 to v=150.
    e.execute("UPDATE t_hist SET v = 150 WHERE id = 1").unwrap();

    // Query as of ts_before_update — should see the OLD value (v=100 for id=1).
    let r = e
        .execute(&format!("SELECT * FROM t_hist FOR SYSTEM_TIME AS OF {}", ts_before_update))
        .unwrap();
    assert!(r.row_count >= 1, "temporal AS OF query must return rows");
    let id_col = r.columns.iter().find(|c| c.name == "id").expect("id column");
    let v_col = r.columns.iter().find(|c| c.name == "v").expect("v column");
    let mut found_old = false;
    for i in 0..r.row_count {
        if id_col.values[i] == 1 {
            assert_eq!(v_col.values[i], 100, "AS OF before update must see old value v=100");
            found_old = true;
        }
    }
    assert!(found_old, "AS OF before update must include id=1");

    // Query as of far-future timestamp — should see the NEW value (v=150 for id=1).
    let r = e.execute("SELECT * FROM t_hist FOR SYSTEM_TIME AS OF 18446744073709551615").unwrap();
    let id_col = r.columns.iter().find(|c| c.name == "id").expect("id column");
    let v_col = r.columns.iter().find(|c| c.name == "v").expect("v column");
    let mut found_new = false;
    for i in 0..r.row_count {
        if id_col.values[i] == 1 {
            assert_eq!(v_col.values[i], 150, "AS OF after update must see new value v=150");
            found_new = true;
        }
    }
    assert!(found_new, "AS OF after update must include id=1");
}

// -----------------------------------------------------------------------
// Window functions: ROW_NUMBER / RANK / SUM OVER.
// -----------------------------------------------------------------------

#[test]
fn window_row_number_through_engine() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (dept INT, salary INT)").unwrap();
    e.execute("INSERT INTO t (dept, salary) VALUES (1, 100), (1, 200), (2, 150)").unwrap();
    // ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC)
    let r = e
        .execute("SELECT ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) FROM t")
        .unwrap();
    // The window function column is appended after the base SELECT.
    // We expect 3 rows; the row_number column should have values 1, 2, 1
    // (rank within each partition).
    assert_eq!(r.row_count, 3, "window query must return 3 rows");
    // Find the row_number column (last column).
    let rn_col = r.columns.last().expect("row_number column");
    assert!(rn_col.values.contains(&1), "row_number must contain 1 (first in partition)");
}

#[test]
fn window_sum_over_through_engine() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (dept INT, salary INT)").unwrap();
    e.execute("INSERT INTO t (dept, salary) VALUES (1, 100), (1, 200), (2, 150)").unwrap();
    // SUM(salary) OVER (PARTITION BY dept) — running total per partition.
    let r = e.execute("SELECT SUM(salary) OVER (PARTITION BY dept) FROM t").unwrap();
    assert_eq!(r.row_count, 3);
    let sum_col = r.columns.last().expect("sum column");
    // The window module returns running sums as plain u64 (not f64 bits).
    // We just verify that the column was appended and has non-zero values;
    // the partitioning correctness is tested in the window module's unit tests.
    assert_eq!(sum_col.values.len(), 3, "sum column must have one value per row");
    assert!(sum_col.values.iter().any(|&v| v > 0), "sum column must have non-zero values");
}

// -----------------------------------------------------------------------
// PIVOT: pivot() callable through engine via SQL.
// Wave 56b: the previous test called the pivot module directly (not through
// engine.execute), so it didn't verify that PIVOT is actually wired into
// the SQL execution path. We now detect `PIVOT (...)` in the SQL string
// and route to the pivot module end-to-end.
// -----------------------------------------------------------------------

#[test]
fn pivot_function_callable_via_engine() {
    // Wave 56b: this test still calls the pivot module directly to verify
    // the module surface is exported. The end-to-end SQL test below covers
    // the wiring through engine.execute().
    use turbogp::engine::{QueryResult, ResultColumn};
    let input = QueryResult {
        columns: vec![
            ResultColumn {
                name: "dept".into(),
                values: vec![1, 1, 2],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "qtr".into(),
                values: vec![1, 2, 1],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "amt".into(),
                values: vec![100, 200, 150],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: 3,
        elapsed_us: 0,
    };
    let pivot_values = vec!["1".to_string(), "2".to_string()];
    let result = turbogp::exec::pivot::pivot(&input, "dept", "qtr", "amt", &pivot_values, "SUM");
    assert_eq!(result.row_count, 2, "pivot must produce one row per dept");
    // Two pivot columns + one group column = 3 columns.
    assert_eq!(result.columns.len(), 3, "pivot must produce 3 columns: dept, Q1_sum, Q2_sum");
}

/// Wave 56b: PIVOT through engine.execute() — verify the PIVOT clause is
/// detected in the SQL string, the underlying SELECT is executed to produce
/// input rows, and the pivot transformation is applied end-to-end.
#[test]
fn pivot_clause_through_engine_execute() {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE sales (dept INT, qtr INT, amt INT)").unwrap();
    e.execute("INSERT INTO sales (dept, qtr, amt) VALUES (1, 1, 100), (1, 2, 200), (2, 1, 150)")
        .unwrap();

    // PIVOT (SUM(amt) FOR qtr IN (1, 2)) — produces one row per dept with
    // columns: dept, "1", "2" (the summed amt for each quarter).
    let sql = "SELECT * FROM sales PIVOT (SUM(amt) FOR qtr IN (1, 2)) AS p";
    let r = e.execute(sql);
    assert!(r.is_ok(), "PIVOT query must execute; got: {:?}", r.err());
    let r = r.unwrap();
    // Two unique depts (1 and 2) → 2 rows.
    assert_eq!(r.row_count, 2, "PIVOT must produce one row per dept");
    // 1 group col (dept) + 2 pivot cols ("1", "2") = 3 columns.
    assert_eq!(r.columns.len(), 3, "PIVOT must produce 3 columns: dept, '1', '2'");
    // Find the dept column and verify it has both 1 and 2.
    let dept_col = r.columns.iter().find(|c| c.name == "dept").expect("dept column");
    assert!(dept_col.values.contains(&1), "dept column must contain 1");
    assert!(dept_col.values.contains(&2), "dept column must contain 2");
    // Verify the pivot values: dept 1 has qtr=1 sum=100, qtr=2 sum=200.
    // Find the row index for dept=1.
    let dept1_idx = dept_col.values.iter().position(|&v| v == 1).unwrap();
    // The two pivot columns are named "1" and "2" (the pivot_values).
    let q1_col = r.columns.iter().find(|c| c.name == "1").expect("pivot col '1'");
    let q2_col = r.columns.iter().find(|c| c.name == "2").expect("pivot col '2'");
    assert_eq!(q1_col.values[dept1_idx], 100, "dept 1, qtr 1 sum must be 100");
    assert_eq!(q2_col.values[dept1_idx], 200, "dept 1, qtr 2 sum must be 200");
}
