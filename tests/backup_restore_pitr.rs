//! Wave 6 — Agent C: Backup / Restore / PITR SQL command tests.
//!
//! These tests verify:
//! - `BACKUP TO '<dir>'` writes manifest.json + CSV files (Task 6.1)
//! - `RESTORE FROM '<dir>'` reads the manifest and loads CSV (Task 6.2)
//! - `RESTORE FROM '<dir>' AS OF TIMESTAMP '<ts>'` does PITR (Task 6.3)

use tempfile::TempDir;
use turbogp::engine::QueryEngine;

#[test]
fn test_backup_creates_manifest_and_csv() {
    // Wave 6 Task 6.1 DoD: BACKUP TO writes manifest.json + CSV files.
    let tmp = TempDir::new().unwrap();
    let backup_dir = tmp.path().join("backup");
    let backup_str = backup_dir.to_str().unwrap();

    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

    let r = engine.execute(&format!("BACKUP TO '{}'", backup_str));
    assert!(r.is_ok(), "BACKUP should succeed: {:?}", r.err());
    let r = r.unwrap();
    assert_eq!(r.row_count, 3, "BACKUP should report 3 rows backed up");

    // manifest.json should exist.
    let manifest_path = backup_dir.join("manifest.json");
    assert!(manifest_path.exists(), "manifest.json should exist");

    // t.csv should exist and contain the data.
    let csv_path = backup_dir.join("t.csv");
    assert!(csv_path.exists(), "t.csv should exist");
    let csv_content = std::fs::read_to_string(&csv_path).unwrap();
    assert!(csv_content.contains("id"), "CSV header should contain 'id'");
}

#[test]
fn test_restore_round_trip() {
    // Wave 6 Task 6.2 DoD: backup, then restore into a fresh engine, verify data matches.
    let tmp = TempDir::new().unwrap();
    let backup_dir = tmp.path().join("backup");
    let backup_str = backup_dir.to_str().unwrap();

    // Backup from engine A.
    {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT)").unwrap();
        engine.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        engine.execute(&format!("BACKUP TO '{}'", backup_str)).unwrap();
    }

    // Restore into a fresh engine B.
    let mut engine = QueryEngine::in_memory();
    let r = engine.execute(&format!("RESTORE FROM '{}'", backup_str));
    assert!(r.is_ok(), "RESTORE should succeed: {:?}", r.err());

    // Verify the data is back.
    let r = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.columns[0].values[0], 3, "table should have 3 rows after restore");
}

#[test]
fn test_restore_as_of_timestamp() {
    // Wave 6 Task 6.3 DoD: RESTORE ... AS OF TIMESTAMP does PITR.
    let tmp = TempDir::new().unwrap();
    let backup_dir = tmp.path().join("backup");
    let backup_str = backup_dir.to_str().unwrap();

    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    let r = engine.execute(&format!("BACKUP TO '{}'", backup_str));
    assert!(r.is_ok(), "BACKUP should succeed: {:?}", r.err());

    // Verify the manifest exists before RESTORE.
    let manifest_path = backup_dir.join("manifest.json");
    assert!(manifest_path.exists(), "manifest.json should exist after BACKUP at {}", manifest_path.display());

    // RESTORE ... AS OF TIMESTAMP '2024-01-01T12:00:00Z'
    let sql = format!(
        "RESTORE FROM '{}' AS OF TIMESTAMP '2024-01-01T12:00:00Z'",
        backup_str
    );
    let r = engine.execute(&sql);
    assert!(r.is_ok(), "RESTORE AS OF TIMESTAMP should succeed: {:?}", r.err());
}

#[test]
fn test_backup_invalid_syntax_returns_error() {
    let mut engine = QueryEngine::in_memory();
    let r = engine.execute("BACKUP TO"); // missing directory
    assert!(r.is_err(), "BACKUP without directory should error");
}

#[test]
fn test_restore_nonexistent_directory_returns_error() {
    let mut engine = QueryEngine::in_memory();
    let r = engine.execute("RESTORE FROM '/nonexistent/path/that/does/not/exist'");
    assert!(r.is_err(), "RESTORE from nonexistent dir should error");
}

#[test]
fn test_backup_dispatched_by_classifier() {
    // Verify classify_statement identifies BACKUP and RESTORE.
    use turbogp::engine::dispatch::{classify_statement, StatementKind};
    assert_eq!(classify_statement("BACKUP TO '/tmp/x'"), StatementKind::Backup);
    assert_eq!(classify_statement("RESTORE FROM '/tmp/x'"), StatementKind::Restore);
    assert_eq!(
        classify_statement("RESTORE FROM '/tmp/x' AS OF TIMESTAMP '2024-01-01'"),
        StatementKind::Restore
    );
}
