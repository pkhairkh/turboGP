//! Wave 3 — Agent C: verify parser-based statement dispatch.
//!
//! These tests prove `classify_statement()` correctly identifies the
//! statement kind via the formal tokenizer (not `starts_with()`), and that
//! `execute()` dispatches to the right executor method for each kind.

use turbogp::engine::dispatch::{classify_statement, StatementKind};

#[test]
fn test_classify_select() {
    assert_eq!(classify_statement("SELECT * FROM t"), StatementKind::Select);
    assert_eq!(classify_statement("select 1"), StatementKind::Select);
    assert_eq!(classify_statement("  SELECT DISTINCT x FROM t"), StatementKind::Select);
}

#[test]
fn test_classify_with_cte() {
    assert_eq!(classify_statement("WITH x AS (SELECT 1) SELECT * FROM x"), StatementKind::With);
}

#[test]
fn test_classify_insert_update_delete() {
    assert_eq!(classify_statement("INSERT INTO t VALUES (1)"), StatementKind::Insert);
    assert_eq!(classify_statement("UPDATE t SET x = 0"), StatementKind::Update);
    assert_eq!(classify_statement("DELETE FROM t"), StatementKind::Delete);
}

#[test]
fn test_classify_ddl() {
    assert_eq!(classify_statement("CREATE TABLE t (id INT)"), StatementKind::Create);
    assert_eq!(classify_statement("DROP TABLE t"), StatementKind::Drop);
    assert_eq!(classify_statement("ALTER TABLE t ADD COLUMN x INT"), StatementKind::Alter);
}

#[test]
fn test_classify_transaction_control() {
    assert_eq!(classify_statement("BEGIN"), StatementKind::Begin);
    assert_eq!(classify_statement("START TRANSACTION"), StatementKind::Begin);
    assert_eq!(classify_statement("COMMIT"), StatementKind::Commit);
    assert_eq!(classify_statement("ROLLBACK"), StatementKind::Rollback);
    assert_eq!(classify_statement("ROLLBACK TO sp1"), StatementKind::RollbackTo);
}

#[test]
fn test_classify_savepoint_release() {
    assert_eq!(classify_statement("SAVEPOINT sp1"), StatementKind::Savepoint);
    assert_eq!(classify_statement("RELEASE sp1"), StatementKind::Release);
}

#[test]
fn test_classify_utility() {
    assert_eq!(classify_statement("COPY t TO '/tmp/x.csv'"), StatementKind::Copy);
    assert_eq!(classify_statement("VACUUM"), StatementKind::Vacuum);
    assert_eq!(classify_statement("CHECKPOINT"), StatementKind::Checkpoint);
    assert_eq!(classify_statement("EXPLAIN SELECT * FROM t"), StatementKind::Explain);
    assert_eq!(classify_statement("ANALYZE SELECT * FROM t"), StatementKind::Analyze);
}

#[test]
fn test_classify_merge_backup_restore() {
    assert_eq!(classify_statement("MERGE INTO t USING s ON t.id = s.id ..."), StatementKind::Merge);
    assert_eq!(classify_statement("BACKUP TO '/tmp/backup'"), StatementKind::Backup);
    assert_eq!(classify_statement("RESTORE FROM '/tmp/backup'"), StatementKind::Restore);
}

#[test]
fn test_classify_show_exec_truncate() {
    assert_eq!(classify_statement("SHOW tables"), StatementKind::Show);
    assert_eq!(classify_statement("EXEC my_proc"), StatementKind::Exec);
    assert_eq!(classify_statement("EXECUTE my_proc"), StatementKind::Exec);
    assert_eq!(classify_statement("TRUNCATE TABLE t"), StatementKind::Truncate);
}

#[test]
fn test_classify_other_and_empty() {
    assert_eq!(classify_statement(""), StatementKind::Other);
    assert_eq!(classify_statement("   "), StatementKind::Other);
    assert_eq!(classify_statement("GARBAGE SQL"), StatementKind::Other);
    assert_eq!(classify_statement("-- comment only"), StatementKind::Other);
}

#[test]
fn test_classify_case_insensitive() {
    assert_eq!(classify_statement("select * from t"), StatementKind::Select);
    assert_eq!(classify_statement("Select * From t"), StatementKind::Select);
    assert_eq!(classify_statement("SELECT * FROM t"), StatementKind::Select);
    assert_eq!(classify_statement("insert into t values (1)"), StatementKind::Insert);
    assert_eq!(classify_statement("CREATE TABLE t (id INT)"), StatementKind::Create);
}

#[test]
fn test_classify_leading_whitespace() {
    assert_eq!(classify_statement("  SELECT 1"), StatementKind::Select);
    assert_eq!(classify_statement("\t\nINSERT INTO t VALUES (1)"), StatementKind::Insert);
    assert_eq!(classify_statement("   \n  EXPLAIN SELECT 1"), StatementKind::Explain);
}

#[test]
fn test_statement_kind_is_readonly() {
    assert!(StatementKind::Select.is_readonly());
    assert!(StatementKind::Explain.is_readonly());
    assert!(StatementKind::Show.is_readonly());

    assert!(!StatementKind::Insert.is_readonly());
    assert!(!StatementKind::Update.is_readonly());
    assert!(!StatementKind::Delete.is_readonly());
    assert!(!StatementKind::Create.is_readonly());
    assert!(!StatementKind::Drop.is_readonly());
    assert!(!StatementKind::Begin.is_readonly());
    assert!(!StatementKind::Commit.is_readonly());
    assert!(!StatementKind::Rollback.is_readonly());
    assert!(!StatementKind::Copy.is_readonly());
    assert!(!StatementKind::Vacuum.is_readonly());
    assert!(!StatementKind::Other.is_readonly());
}

// ---------------------------------------------------------------------------
// End-to-end dispatch tests: verify execute() routes each kind correctly.
// ---------------------------------------------------------------------------

#[test]
fn test_execute_dispatches_select() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 3);
}

#[test]
fn test_execute_dispatches_insert() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    let r = engine.execute("INSERT INTO t VALUES (42)").unwrap();
    assert_eq!(r.row_count, 1);
}

#[test]
fn test_execute_dispatches_update() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    let r = engine.execute("UPDATE t SET v = 99 WHERE id = 1").unwrap();
    // UPDATE affects 1 row.
    assert!(r.row_count >= 1);
    let r = engine.execute("SELECT v FROM t WHERE id = 1").unwrap();
    assert_eq!(r.columns[0].values[0], 99);
}

#[test]
fn test_execute_dispatches_delete() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let _ = engine.execute("DELETE FROM t WHERE id = 2").unwrap();
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 2);
}

#[test]
fn test_execute_dispatches_begin_commit() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    let r = engine.execute("BEGIN").unwrap();
    assert_eq!(r.row_count, 0);
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = engine.execute("COMMIT").unwrap();
    assert_eq!(r.row_count, 0);
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 1);
}

#[test]
fn test_execute_dispatches_rollback() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("ROLLBACK").unwrap();
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 0);
}

#[test]
fn test_execute_dispatches_explain() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    let r = engine.execute("EXPLAIN SELECT * FROM t").unwrap();
    assert_eq!(r.row_count, 1);
    assert_eq!(r.columns[0].name, "QUERY PLAN");
}

#[test]
fn test_execute_dispatches_vacuum_checkpoint() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    // VACUUM should succeed.
    let r = engine.execute("VACUUM");
    assert!(r.is_ok(), "VACUUM should succeed: {:?}", r.err());
    // CHECKPOINT should succeed.
    let r = engine.execute("CHECKPOINT");
    assert!(r.is_ok(), "CHECKPOINT should succeed: {:?}", r.err());
}

#[test]
fn test_execute_dispatches_savepoint_rollback_to() {
    let mut engine = turbogp::engine::QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("SAVEPOINT sp1").unwrap();
    engine.execute("INSERT INTO t VALUES (2)").unwrap();
    engine.execute("ROLLBACK TO sp1").unwrap();
    engine.execute("COMMIT").unwrap();
    // After ROLLBACK TO sp1, the second insert is undone.
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 1);
}
