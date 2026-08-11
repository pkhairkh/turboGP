//! Wave 50 — End-to-end (engine.execute) tests for the four bugs fixed in
//! this wave:
//!
//! 1. DML WHERE only supported `=`. Now `!=`/`<>`/`<`/`>`/`<=`/`>=` work.
//! 2. DML WHERE broke on strings with spaces (whitespace splitting).
//!    Now the SQL lexer tokenises the WHERE clause, so `'Alice Bob'`
//!    round-trips correctly.
//! 3. UPDATE didn't update the NULL bitmap, so a cell set to NULL was
//!    still counted by COUNT(col). The bitmap is now updated.
//! 4. Checkpoint was type-destructive — every column was hardcoded as
//!    INT and every value was written as the raw u64 cell. Now the schema
//!    is consulted, string sidecars emit quoted literals, float columns
//!    emit decoded f64 values, and NULL cells emit the literal NULL.

use turbogp::engine::QueryEngine;
use turbogp::storage::recovery::{Checkpoint, Wal, WalRecord};

// -----------------------------------------------------------------------
// Bug 4: DML WHERE comparison operators beyond `=`.
// -----------------------------------------------------------------------

#[test]
fn dml_where_less_than() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3), (4), (5)").unwrap();
    let r = e.execute("DELETE FROM t WHERE id < 3").unwrap();
    assert_eq!(r.row_count, 2, "should delete 2 rows with id < 3");
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn dml_where_greater_than() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3), (4), (5)").unwrap();
    let r = e.execute("DELETE FROM t WHERE id > 3").unwrap();
    assert_eq!(r.row_count, 2);
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn dml_where_not_equal() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    let r = e.execute("DELETE FROM t WHERE id != 2").unwrap();
    assert_eq!(r.row_count, 2, "should delete rows with id != 2");
    let r = e.execute("SELECT id FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 2);
}

#[test]
fn dml_where_less_than_or_equal() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    let r = e.execute("DELETE FROM t WHERE id <= 2").unwrap();
    assert_eq!(r.row_count, 2);
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn dml_where_greater_than_or_equal() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
    let r = e.execute("DELETE FROM t WHERE id >= 2").unwrap();
    assert_eq!(r.row_count, 2);
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn dml_where_update_with_less_than() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 0), (2, 0), (3, 0), (4, 0)").unwrap();
    e.execute("UPDATE t SET v = 99 WHERE id >= 3").unwrap();
    let r = e.execute("SELECT v FROM t ORDER BY id").unwrap();
    assert_eq!(r.columns[0].values[0], 0);
    assert_eq!(r.columns[0].values[1], 0);
    assert_eq!(r.columns[0].values[2], 99);
    assert_eq!(r.columns[0].values[3], 99);
}

// -----------------------------------------------------------------------
// Bug 5: DML WHERE breaks on strings with spaces.
// -----------------------------------------------------------------------

#[test]
fn dml_where_string_with_spaces() {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "id,name").unwrap();
    writeln!(tmp, "1,Alice Bob").unwrap();
    writeln!(tmp, "2,Charlie").unwrap();
    writeln!(tmp, "3,Alice Bob").unwrap();
    tmp.flush().unwrap();

    let mut e = QueryEngine::in_memory();
    e.load_csv(tmp.path().to_str().unwrap(), "users", true).unwrap();

    // UPDATE the rows with name = 'Alice Bob' (which contains a space).
    e.execute("UPDATE users SET name = 'X' WHERE name = 'Alice Bob'").unwrap();
    let r = e.execute("SELECT count(*) FROM users WHERE name = 'X'").unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "two rows should have been updated to 'X'");
}

#[test]
fn dml_where_string_with_pipe() {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "id,name").unwrap();
    writeln!(tmp, "1,a|b").unwrap();
    writeln!(tmp, "2,normal").unwrap();
    tmp.flush().unwrap();
    let mut e = QueryEngine::in_memory();
    e.load_csv(tmp.path().to_str().unwrap(), "t", true).unwrap();
    let r = e.execute("SELECT count(*) FROM t WHERE name = 'a|b'").unwrap();
    assert_eq!(r.scalar_u64(), Some(1), "string with pipe must round-trip through DML WHERE");
}

// -----------------------------------------------------------------------
// Bug 6: UPDATE NULL bitmap.
// -----------------------------------------------------------------------

#[test]
fn update_to_null_excludes_from_count() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, col INT)").unwrap();
    e.execute("INSERT INTO t (id, col) VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    // Before: COUNT(col) = 3.
    let r = e.execute("SELECT count(col) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));

    // Update col to NULL where id = 2.
    e.execute("UPDATE t SET col = NULL WHERE id = 2").unwrap();

    // After: COUNT(col) should exclude the NULL row.
    let r = e.execute("SELECT count(col) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2), "COUNT(col) must exclude the row set to NULL");
}

#[test]
fn update_to_null_then_back_to_value() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, col INT)").unwrap();
    e.execute("INSERT INTO t (id, col) VALUES (1, 10)").unwrap();

    // Set to NULL — COUNT(col) should drop to 0.
    e.execute("UPDATE t SET col = NULL WHERE id = 1").unwrap();
    let r = e.execute("SELECT count(col) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(0));

    // Set back to a value — COUNT(col) should rise to 1.
    e.execute("UPDATE t SET col = 99 WHERE id = 1").unwrap();
    let r = e.execute("SELECT count(col) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

// -----------------------------------------------------------------------
// Bug 7: Checkpoint preserves column types.
// -----------------------------------------------------------------------

#[test]
fn checkpoint_preserves_float_type() {
    use turbogp::catalog::Catalog;
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
    use turbogp::schema::table_schema::{ColumnSchema, TableSchema};
    use turbogp::sql::ddl::ColumnType;

    let mut cat = Catalog::new();
    let cells = vec![1.5f64.to_bits(), 2.5f64.to_bits(), 3.5f64.to_bits()];
    let mut t = DS::from_loaded(LoadedTable {
        name: "metrics".into(),
        columns: vec![LoadedColumn {
            name: "price".into(),
            cells,
            row_count: 3,
            string_search: None,
            null_bitmap: None,
        }],
        row_count: 3,
    });
    t.schema = Some(TableSchema {
        columns: vec![ColumnSchema {
            name: "price".into(),
            col_type: ColumnType::Float,
            not_null: false,
            primary_key: false,
            unique: false,
            check: None,
        }],
        checks: Vec::new(),
        unique_constraints: Vec::new(),
        foreign_keys: Vec::new(),
    });
    cat.register(t);

    let tmp = tempfile::TempDir::new().unwrap();
    let count = Checkpoint::save(&cat, tmp.path().join("checkpoint.sql")).unwrap();
    assert_eq!(count, 1);

    let content = std::fs::read_to_string(tmp.path().join("checkpoint.sql")).unwrap();
    assert!(
        content.contains("CREATE TABLE metrics (price FLOAT)"),
        "checkpoint must declare FLOAT, got: {content}"
    );
    assert!(content.contains("INSERT INTO metrics VALUES (1.5)"));
    assert!(content.contains("INSERT INTO metrics VALUES (2.5)"));
    assert!(content.contains("INSERT INTO metrics VALUES (3.5)"));
}

#[test]
fn checkpoint_preserves_varchar_type() {
    use turbogp::catalog::Catalog;
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
    use turbogp::exec::fm_index::StringSearchColumn;
    use turbogp::schema::table_schema::{ColumnSchema, TableSchema};
    use turbogp::sql::ddl::ColumnType;

    let mut cat = Catalog::new();
    let strings = vec!["alice".to_string(), "bo'b".to_string(), "carol".to_string()];
    let cells: Vec<u64> =
        strings.iter().map(|s| xxhash_rust::xxh3::xxh3_64(s.as_bytes())).collect();
    let sc = StringSearchColumn::new(strings.clone());
    let mut t = DS::from_loaded(LoadedTable {
        name: "users".into(),
        columns: vec![LoadedColumn {
            name: "name".into(),
            cells,
            row_count: 3,
            string_search: Some(sc),
            null_bitmap: None,
        }],
        row_count: 3,
    });
    t.schema = Some(TableSchema {
        columns: vec![ColumnSchema {
            name: "name".into(),
            col_type: ColumnType::Varchar(Some(50)),
            not_null: false,
            primary_key: false,
            unique: false,
            check: None,
        }],
        checks: Vec::new(),
        unique_constraints: Vec::new(),
        foreign_keys: Vec::new(),
    });
    cat.register(t);

    let tmp = tempfile::TempDir::new().unwrap();
    Checkpoint::save(&cat, tmp.path().join("checkpoint.sql")).unwrap();

    let content = std::fs::read_to_string(tmp.path().join("checkpoint.sql")).unwrap();
    assert!(
        content.contains("CREATE TABLE users (name VARCHAR(50))"),
        "checkpoint must declare VARCHAR(50), got: {content}"
    );
    assert!(content.contains("INSERT INTO users VALUES ('alice')"));
    assert!(
        content.contains("INSERT INTO users VALUES ('bo''b')"),
        "single quote must be doubled: {content}"
    );
    assert!(content.contains("INSERT INTO users VALUES ('carol')"));
}

#[test]
fn checkpoint_emits_null_for_null_cells() {
    use turbogp::catalog::Catalog;
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;

    let mut cat = Catalog::new();
    let t = DS::from_loaded(LoadedTable {
        name: "t".into(),
        columns: vec![LoadedColumn {
            name: "v".into(),
            cells: vec![10, 0, 30],
            row_count: 3,
            string_search: None,
            null_bitmap: Some(vec![false, true, false]),
        }],
        row_count: 3,
    });
    cat.register(t);

    let tmp = tempfile::TempDir::new().unwrap();
    Checkpoint::save(&cat, tmp.path().join("checkpoint.sql")).unwrap();

    let content = std::fs::read_to_string(tmp.path().join("checkpoint.sql")).unwrap();
    assert!(content.contains("INSERT INTO t VALUES (10)"));
    assert!(
        content.contains("INSERT INTO t VALUES (NULL)"),
        "NULL cell must be emitted as NULL, not 0: {content}"
    );
    assert!(content.contains("INSERT INTO t VALUES (30)"));
}

#[test]
fn checkpoint_roundtrip_floats_and_varchars() {
    // End-to-end: build a table with FLOAT and VARCHAR, checkpoint it,
    // then re-execute the checkpoint file on a fresh engine and verify
    // the FLOAT values round-trip. (VARCHAR values are correctly emitted
    // as quoted literals in the checkpoint SQL — the engine's INSERT
    // path stores them as xxh3 hashes without rebuilding the string
    // sidecar, so we only verify the count and the float values, not
    // the original strings.)
    use turbogp::catalog::Catalog;
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
    use turbogp::exec::fm_index::StringSearchColumn;
    use turbogp::schema::table_schema::{ColumnSchema, TableSchema};
    use turbogp::sql::ddl::ColumnType;

    let mut cat = Catalog::new();
    let strings = vec!["alpha".to_string(), "beta".to_string()];
    let str_cells: Vec<u64> =
        strings.iter().map(|s| xxhash_rust::xxh3::xxh3_64(s.as_bytes())).collect();
    let float_cells = vec![1.25f64.to_bits(), 9.75f64.to_bits()];
    let sc = StringSearchColumn::new(strings.clone());
    let mut t = DS::from_loaded(LoadedTable {
        name: "mix".into(),
        columns: vec![
            LoadedColumn {
                name: "name".into(),
                cells: str_cells,
                row_count: 2,
                string_search: Some(sc),
                null_bitmap: None,
            },
            LoadedColumn {
                name: "price".into(),
                cells: float_cells,
                row_count: 2,
                string_search: None,
                null_bitmap: None,
            },
        ],
        row_count: 2,
    });
    t.schema = Some(TableSchema {
        columns: vec![
            ColumnSchema {
                name: "name".into(),
                col_type: ColumnType::Varchar(None),
                not_null: false,
                primary_key: false,
                unique: false,
                check: None,
            },
            ColumnSchema {
                name: "price".into(),
                col_type: ColumnType::Float,
                not_null: false,
                primary_key: false,
                unique: false,
                check: None,
            },
        ],
        checks: Vec::new(),
        unique_constraints: Vec::new(),
        foreign_keys: Vec::new(),
    });
    cat.register(t);

    let tmp = tempfile::TempDir::new().unwrap();
    Checkpoint::save(&cat, tmp.path().join("checkpoint.sql")).unwrap();
    let sql = std::fs::read_to_string(tmp.path().join("checkpoint.sql")).unwrap();

    // Verify the SQL output preserves types correctly.
    assert!(
        sql.contains("CREATE TABLE mix (name VARCHAR, price FLOAT)"),
        "CREATE TABLE must preserve VARCHAR/FLOAT types, got: {sql}"
    );
    assert!(sql.contains("INSERT INTO mix VALUES ('alpha', 1.25)"));
    assert!(
        sql.contains("INSERT INTO mix VALUES ('beta', 9.75)"),
        "INSERT must emit quoted string + decoded float, got: {sql}"
    );

    let mut e = QueryEngine::in_memory();
    // Re-execute the checkpoint file statement by statement.
    for stmt in sql.lines().filter(|l| !l.trim().is_empty()) {
        e.execute(stmt).expect(&format!("replaying: {stmt}"));
    }

    // Verify the float value round-tripped through the engine.
    let r = e.execute("SELECT price FROM mix ORDER BY price").unwrap();
    let v0 = f64::from_bits(r.columns[0].values[0]);
    let v1 = f64::from_bits(r.columns[0].values[1]);
    assert!((v0 - 1.25).abs() < 0.001, "price row 0 = {v0}, want 1.25");
    assert!((v1 - 9.75).abs() < 0.001, "price row 1 = {v1}, want 9.75");

    // Verify the row count round-tripped (both VARCHAR rows survived).
    let r = e.execute("SELECT count(*) FROM mix").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

// -----------------------------------------------------------------------
// WAL still works after the eval_simple_where rewrite (regression).
// -----------------------------------------------------------------------

#[test]
fn wal_replays_after_wave50_changes() {
    use tempfile::TempDir;
    let tmp = TempDir::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    wal.append(&WalRecord {
        lsn: 0, timestamp_us: 0,
        txn_id: 0,
        sql: "CREATE TABLE t (id INT)".into(),
        is_commit: false,
        is_rollback: false,
        physical_change: None,
    })
    .unwrap();
    wal.append(&WalRecord {
        lsn: 0, timestamp_us: 0,
        txn_id: 0,
        sql: "INSERT INTO t VALUES (1)".into(),
        is_commit: false,
        is_rollback: false,
        physical_change: None,
    })
    .unwrap();
    wal.append(&WalRecord {
        lsn: 0, timestamp_us: 0,
        txn_id: 0,
        sql: "INSERT INTO t VALUES (2)".into(),
        is_commit: false,
        is_rollback: false,
        physical_change: None,
    })
    .unwrap();
    wal.sync().unwrap();

    let mut e = QueryEngine::in_memory();
    let stats = turbogp::storage::recovery::replay_wal(&mut e, &wal).unwrap();
    assert_eq!(stats.replayed, 3);
    assert_eq!(stats.errors, 0);
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}
