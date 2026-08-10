//! Wave 51 — End-to-end (engine.execute + WAL) tests for the three bugs
//! fixed in this wave:
//!
//! 1. WAL had no commit markers — `BEGIN; INSERT; INSERT; COMMIT;` was
//!    indistinguishable from three autocommit INSERTs, and
//!    `BEGIN; INSERT; ROLLBACK;` would still replay the INSERT. Now
//!    BEGIN/COMMIT/ROLLBACK write proper boundary records.
//! 2. WAL appended the SQL BEFORE executing — a failed execute (e.g.
//!    INSERT INTO nonexistent) still left a record in the WAL. Now the
//!    append happens AFTER a successful execute.
//! 3. WAL string escaping was ambiguous (`\\|` / `\\n`). A SQL string
//!    containing literal `\n` bytes was indistinguishable from a real
//!    newline on replay. Now the SQL payload is base64-encoded.

use turbogp::engine::QueryEngine;
use turbogp::storage::recovery::{replay_wal, Wal, WalRecord};

// -----------------------------------------------------------------------
// Bug 8: WAL commit markers — BEGIN/COMMIT/ROLLBACK round-trip.
// -----------------------------------------------------------------------

#[test]
fn wal_begin_commit_markers_round_trip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    // Simulate: BEGIN; INSERT; INSERT; COMMIT;
    wal.append(&WalRecord::begin(1)).unwrap();
    wal.append(&WalRecord::txn_dml(1, "CREATE TABLE t (id INT)")).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (1)")).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (2)")).unwrap();
    wal.append(&WalRecord::commit(1)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0].txn_id, 1);
    assert!(!records[0].is_commit && !records[0].is_rollback);
    assert_eq!(records[0].sql, ""); // BEGIN marker has empty SQL
    assert!(records[4].is_commit);
    assert_eq!(records[4].txn_id, 1);
}

#[test]
fn wal_rollback_marker_round_trip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    wal.append(&WalRecord::begin(1)).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (1)")).unwrap();
    wal.append(&WalRecord::rollback(1)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 3);
    assert!(records[2].is_rollback);
    assert_eq!(records[2].txn_id, 1);
}

#[test]
fn wal_replay_commit_marker_replays_transaction() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    // BEGIN; CREATE TABLE; INSERT; INSERT; COMMIT;
    wal.append(&WalRecord::begin(1)).unwrap();
    wal.append(&WalRecord::txn_dml(1, "CREATE TABLE t (id INT)")).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (1)")).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (2)")).unwrap();
    wal.append(&WalRecord::commit(1)).unwrap();
    wal.sync().unwrap();

    let mut engine = QueryEngine::new();
    let stats = replay_wal(&mut engine, &wal).unwrap();
    assert_eq!(stats.replayed, 3, "CREATE TABLE + 2 INSERTs should replay");
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.skipped, 0);

    let r = engine.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn wal_replay_rollback_marker_skips_transaction() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    // Autocommit: CREATE TABLE.
    wal.append(&WalRecord::autocommit("CREATE TABLE t (id INT)")).unwrap();
    // BEGIN; INSERT; ROLLBACK;
    wal.append(&WalRecord::begin(1)).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (1)")).unwrap();
    wal.append(&WalRecord::rollback(1)).unwrap();
    wal.sync().unwrap();

    let mut engine = QueryEngine::new();
    let stats = replay_wal(&mut engine, &wal).unwrap();
    // The CREATE TABLE autocommit record replays; the rolled-back txn is skipped.
    assert_eq!(stats.replayed, 1, "only the autocommit CREATE TABLE should replay");
    assert_eq!(stats.skipped, 1, "the rolled-back txn must be skipped");
    assert_eq!(stats.errors, 0);

    let r = engine.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(0), "rolled-back INSERT must not be visible");
}

#[test]
fn wal_replay_uncommitted_transaction_is_skipped() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    // Autocommit: CREATE TABLE.
    wal.append(&WalRecord::autocommit("CREATE TABLE t (id INT)")).unwrap();
    // BEGIN; INSERT; (no COMMIT — crash before commit).
    wal.append(&WalRecord::begin(1)).unwrap();
    wal.append(&WalRecord::txn_dml(1, "INSERT INTO t VALUES (1)")).unwrap();
    wal.sync().unwrap();

    let mut engine = QueryEngine::new();
    let stats = replay_wal(&mut engine, &wal).unwrap();
    assert_eq!(stats.replayed, 1, "only the autocommit CREATE TABLE should replay");
    assert_eq!(stats.skipped, 1, "the uncommitted txn must be skipped");
    assert_eq!(stats.errors, 0);

    let r = engine.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(0), "uncommitted INSERT must not be visible");
}

#[test]
fn engine_writes_begin_commit_markers_through_execute() {
    // End-to-end: engine.execute("BEGIN"); execute("INSERT"); execute("COMMIT")
    // must produce BEGIN/txn_dml/COMMIT records in the WAL.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut engine = QueryEngine::new();
    engine.enable_wal(tmp.path()).unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("INSERT INTO t VALUES (2)").unwrap();
    engine.execute("COMMIT").unwrap();

    let wal = Wal::open(tmp.path()).unwrap();
    let records = wal.read_all().unwrap();
    // Expected: CREATE TABLE (autocommit), BEGIN, INSERT, INSERT, COMMIT.
    assert_eq!(records.len(), 5, "expected 5 WAL records, got {records:?}");

    // The CREATE TABLE is an autocommit record.
    assert_eq!(records[0].txn_id, 0);
    assert!(records[0].sql.contains("CREATE TABLE"));

    // BEGIN marker: empty SQL, non-zero txn_id.
    assert_eq!(records[1].sql, "");
    assert_ne!(records[1].txn_id, 0);
    assert!(!records[1].is_commit && !records[1].is_rollback);

    // The two INSERTs carry the same txn_id as the BEGIN.
    assert_eq!(records[2].txn_id, records[1].txn_id);
    assert_eq!(records[3].txn_id, records[1].txn_id);
    assert!(records[2].sql.contains("INSERT"));
    assert!(records[3].sql.contains("INSERT"));

    // COMMIT marker.
    assert!(records[4].is_commit);
    assert_eq!(records[4].txn_id, records[1].txn_id);
}

#[test]
fn engine_writes_rollback_marker_through_execute() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut engine = QueryEngine::new();
    engine.enable_wal(tmp.path()).unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    engine.execute("ROLLBACK").unwrap();

    let wal = Wal::open(tmp.path()).unwrap();
    let records = wal.read_all().unwrap();
    // CREATE TABLE (autocommit), BEGIN, INSERT, ROLLBACK.
    assert_eq!(records.len(), 4);
    assert!(records[3].is_rollback);
    assert_eq!(records[3].txn_id, records[1].txn_id);
}

// -----------------------------------------------------------------------
// Bug 9: WAL append happens AFTER successful execute.
// -----------------------------------------------------------------------

#[test]
fn wal_does_not_append_failed_dml() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut engine = QueryEngine::new();
    engine.enable_wal(tmp.path()).unwrap();
    engine.execute("CREATE TABLE t (id INT)").unwrap();

    // INSERT INTO nonexistent must fail.
    let result = engine.execute("INSERT INTO nonexistent VALUES (1)");
    assert!(result.is_err(), "INSERT into nonexistent table must error");

    // Read the WAL — it should NOT contain the failed INSERT.
    let wal = Wal::open(tmp.path()).unwrap();
    let records = wal.read_all().unwrap();
    let has_failed = records.iter().any(|r| r.sql.contains("nonexistent"));
    assert!(!has_failed, "WAL must not contain failed INSERT, but found: {records:?}");
}

#[test]
fn wal_does_not_append_failed_ddl() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut engine = QueryEngine::new();
    engine.enable_wal(tmp.path()).unwrap();

    // DDL with a parse error should fail and not be appended.
    let result = engine.execute("CREATE TABLE (bad syntax)");
    assert!(result.is_err());

    let wal = Wal::open(tmp.path()).unwrap();
    let records = wal.read_all().unwrap();
    assert!(records.is_empty(), "WAL must be empty after failed DDL, got: {records:?}");
}

// -----------------------------------------------------------------------
// Bug 10: WAL string escaping round-trips pipe / newline / backslash.
// -----------------------------------------------------------------------

#[test]
fn wal_sql_with_pipe_round_trips() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    let sql = "INSERT INTO t VALUES ('a|b|c')";
    wal.append(&WalRecord::autocommit(sql)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sql, sql, "SQL with pipes must round-trip");
}

#[test]
fn wal_sql_with_newline_round_trips() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    // SQL containing a literal newline (e.g. multi-line INSERT).
    let sql = "INSERT INTO t VALUES ('line1\nline2')";
    wal.append(&WalRecord::autocommit(sql)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sql, sql, "SQL with newlines must round-trip");
}

#[test]
fn wal_sql_with_backslash_n_round_trips() {
    // The crucial test for Bug 10: a SQL string containing the literal
    // two-byte sequence `\n` (backslash + n) must NOT be confused with a
    // real newline on replay. The old escaping couldn't distinguish them.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    let sql = r"INSERT INTO t VALUES ('has\nliteral')";
    wal.append(&WalRecord::autocommit(sql)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].sql, sql,
        "literal backslash-n must round-trip unchanged (was ambiguous before Wave 51)"
    );
}

#[test]
fn wal_sql_with_pipe_and_newline_round_trips() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    let sql = "INSERT INTO t VALUES ('a|b\nc|d')";
    wal.append(&WalRecord::autocommit(sql)).unwrap();
    wal.sync().unwrap();

    let records = wal.read_all().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sql, sql);
}

#[test]
fn wal_replay_handles_sql_with_special_chars() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut wal = Wal::open(tmp.path()).unwrap();
    wal.append(&WalRecord::autocommit("CREATE TABLE t (id INT, name VARCHAR)")).unwrap();
    // Insert a value containing a pipe and a backslash-n.
    wal.append(&WalRecord::autocommit(r"INSERT INTO t (id, name) VALUES (1, 'a|b\n')")).unwrap();
    wal.sync().unwrap();

    let mut engine = QueryEngine::new();
    let stats = replay_wal(&mut engine, &wal).unwrap();
    assert_eq!(stats.replayed, 2);
    assert_eq!(stats.errors, 0);
    let r = engine.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}
