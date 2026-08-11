//! Wave 6 — Agent C: Backup / Restore / PITR SQL command tests.
//!
//! These tests verify:
//! - `BACKUP TO '<dir>'` writes manifest.json + CSV files (Task 6.1)
//! - `RESTORE FROM '<dir>'` reads the manifest and loads CSV (Task 6.2)
//! - `RESTORE FROM '<dir>' AS OF TIMESTAMP '<ts>'` does PITR (Task 6.3)
//!
//! Wave 4 (Tasks 4.3, 4.4, 4.5):
//! - `test_binary_checkpoint_pitr`: PITR via binary checkpoint + WAL replay
//!   with real `timestamp_us` filtering.
//! - `test_migration_sql_to_binary`: legacy SQL-only data dir upgrades to
//!   binary checkpoints after the first CHECKPOINT.
//! - `test_checkpoint_binary_faster_than_sql`: benchmark asserting binary
//!   checkpoint is ≥3x faster than the legacy SQL-text path.

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

// =========================================================================
// Wave 4 — Tasks 4.3, 4.4, 4.5
// =========================================================================

/// Task 4.3 — PITR with binary checkpoints.
///
/// Verifies that `RESTORE FROM '<dir>' AS OF TIMESTAMP '<ts>'`:
///   1. Loads the binary checkpoint (`checkpoint.bin`) when present.
///   2. Replays WAL records with `timestamp_us <= target` using the real
///      `WalRecord::timestamp_us` field (not the legacy record-index fake).
///   3. Skips post-checkpoint WAL records whose timestamp exceeds the
///      target, yielding the checkpoint state at the target time.
#[test]
fn test_binary_checkpoint_pitr() {
    let tmp = TempDir::new().expect("tempdir");
    let backup_dir = tmp.path();
    let backup_str = backup_dir.to_str().expect("backup dir path is utf-8");

    // Phase 1: with_data_dir, CREATE TABLE, insert 5 rows at T1, CHECKPOINT
    // (writes checkpoint.bin + truncates the WAL), then capture T_target
    // between T1 and T2 before inserting 3 more rows at T2.
    let mut engine = QueryEngine::with_data_dir(backup_dir).expect("with_data_dir");
    engine.execute("CREATE TABLE pitr_t (id INT)").expect("create table");
    for i in 0..5u64 {
        engine
            .execute(&format!("INSERT INTO pitr_t VALUES ({i})"))
            .expect("insert pre-cp");
    }
    let r = engine.execute("SELECT count(*) FROM pitr_t").expect("count pre-cp");
    assert_eq!(r.scalar_u64(), Some(5), "5 rows should be present before checkpoint");

    // CHECKPOINT writes checkpoint.bin AND truncates the WAL.
    engine.execute("CHECKPOINT").expect("checkpoint");
    assert!(
        backup_dir.join("checkpoint.bin").exists(),
        "checkpoint.bin must exist after CHECKPOINT"
    );

    // Capture T_target between T1 and T2. Sleep before the post-checkpoint
    // inserts to guarantee their WAL records have `timestamp_us` strictly
    // greater than T_target (Wal::append sets timestamp_us = now() at the
    // moment of append, which is after this sleep).
    std::thread::sleep(std::time::Duration::from_millis(20));
    let t_target_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .expect("epoch");
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Insert 3 more rows at T2 (> T_target). These go into the WAL.
    for i in 5..8u64 {
        engine
            .execute(&format!("INSERT INTO pitr_t VALUES ({i})"))
            .expect("insert post-cp");
    }
    let r = engine.execute("SELECT count(*) FROM pitr_t").expect("count post-cp");
    assert_eq!(r.scalar_u64(), Some(8), "8 rows should be present before drop");
    drop(engine);

    // Phase 2: RESTORE AS OF TIMESTAMP 't_target' on a fresh in-memory
    // engine. The binary checkpoint has 5 rows. The 3 post-checkpoint WAL
    // records have timestamp_us > t_target, so they should be skipped.
    let mut engine2 = QueryEngine::in_memory();
    let sql = format!("RESTORE FROM '{}' AS OF TIMESTAMP '{}'", backup_str, t_target_us);
    let r = engine2.execute(&sql);
    assert!(r.is_ok(), "RESTORE AS OF TIMESTAMP should succeed: {:?}", r.err());

    let r = engine2.execute("SELECT count(*) FROM pitr_t").expect("count after pitr");
    assert_eq!(
        r.scalar_u64(),
        Some(5),
        "PITR should yield 5 rows (post-checkpoint WAL records with \
         timestamp_us > target should be skipped)"
    );

    // Verify the 5 rows are the pre-checkpoint ones (ids 0..5), not the
    // post-checkpoint ones (ids 5..8).
    let r = engine2.execute("SELECT id FROM pitr_t").expect("select ids");
    let mut ids: Vec<u64> = r.columns[0].values.iter().copied().collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![0, 1, 2, 3, 4],
        "PITR should preserve pre-checkpoint ids 0..=4, not post-checkpoint ids 5..=7"
    );
}

/// Task 4.3 — PITR with no AS OF TIMESTAMP loads the full checkpoint state
/// (no WAL filtering) when the WAL is empty (post-CHECKPOINT).
#[test]
fn test_binary_checkpoint_restore_no_timestamp() {
    let tmp = TempDir::new().expect("tempdir");
    let backup_dir = tmp.path();
    let backup_str = backup_dir.to_str().expect("backup dir path is utf-8");

    // Phase 1: with_data_dir, CREATE TABLE, insert 4 rows, CHECKPOINT.
    {
        let mut engine = QueryEngine::with_data_dir(backup_dir).expect("with_data_dir");
        engine.execute("CREATE TABLE rt (id INT)").expect("create");
        for i in 0..4u64 {
            engine.execute(&format!("INSERT INTO rt VALUES ({i})")).expect("insert");
        }
        engine.execute("CHECKPOINT").expect("checkpoint");
    }

    // Phase 2: RESTORE (no timestamp) on a fresh engine.
    let mut engine2 = QueryEngine::in_memory();
    let r = engine2.execute(&format!("RESTORE FROM '{}'", backup_str));
    assert!(r.is_ok(), "RESTORE should succeed: {:?}", r.err());

    let r = engine2.execute("SELECT count(*) FROM rt").expect("count");
    assert_eq!(r.scalar_u64(), Some(4), "all 4 checkpoint rows should be restored");
}

/// Task 4.4 — Migration path: legacy SQL-only data dir upgrades to binary.
///
/// Verifies:
///   1. A data dir with only `checkpoint.sql` (no `checkpoint.bin`) loads
///      via the SQL fallback path on `with_data_dir`.
///   2. After a CHECKPOINT, `checkpoint.bin` is written.
///   3. On the next `with_data_dir`, the binary checkpoint is loaded.
///   4. Data matches across both load paths.
#[test]
fn test_migration_sql_to_binary() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // Phase 1: write a legacy checkpoint.sql (no checkpoint.bin).
    // We use the legacy `Checkpoint::save_and_truncate` directly (NOT
    // `flush_with_checkpoint`, which would write both formats) to simulate
    // a data dir written by a pre-binary-checkpoint engine version.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir");
        engine
            .execute("CREATE TABLE mig_t (id INT, v INT)")
            .expect("create");
        engine
            .execute("INSERT INTO mig_t VALUES (1, 10), (2, 20), (3, 30)")
            .expect("insert");
        let sql_path = data_dir.join("checkpoint.sql");
        let wal = engine.wal.as_mut().expect("wal should be present after with_data_dir");
        turbogp::storage::recovery::Checkpoint::save_and_truncate(
            &engine.catalog,
            &sql_path,
            wal,
        )
        .expect("legacy save_and_truncate");

        // Sanity: no checkpoint.bin should exist (we used the legacy path).
        let bin_path = data_dir.join("checkpoint.bin");
        assert!(
            !bin_path.exists(),
            "checkpoint.bin must NOT exist after legacy save_and_truncate"
        );
        // checkpoint.sql should exist.
        assert!(sql_path.exists(), "checkpoint.sql should exist");
    }

    // Phase 2: with_data_dir should fall back to the SQL checkpoint.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir reload (sql)");
        let r = engine.execute("SELECT count(*) FROM mig_t").expect("count after sql load");
        assert_eq!(r.scalar_u64(), Some(3), "3 rows should survive via SQL checkpoint fallback");
        let r = engine.execute("SELECT v FROM mig_t WHERE id = 2").expect("select v");
        assert_eq!(r.scalar_u64(), Some(20), "id=2 should have v=20");

        // Now CHECKPOINT — this writes checkpoint.bin via flush_with_checkpoint.
        engine.execute("CHECKPOINT").expect("checkpoint");
        let bin_path = data_dir.join("checkpoint.bin");
        assert!(
            bin_path.exists(),
            "checkpoint.bin must exist after CHECKPOINT (migration to binary)"
        );
    }

    // Phase 3: with_data_dir should now load from the binary checkpoint.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir reload (bin)");
        let r = engine.execute("SELECT count(*) FROM mig_t").expect("count after bin load");
        assert_eq!(
            r.scalar_u64(),
            Some(3),
            "3 rows should survive binary checkpoint load (post-migration)"
        );
        let r = engine.execute("SELECT v FROM mig_t WHERE id = 3").expect("select v");
        assert_eq!(r.scalar_u64(), Some(30), "id=3 should have v=30 after binary load");
    }
}

/// Task 4.5 — Benchmark: binary checkpoint is ≥3x faster than SQL-text.
///
/// Inserts 10,000 rows into a table, then times:
///   - `Checkpoint::save` (legacy SQL-text path — emits CREATE TABLE +
///     one INSERT per row).
///   - `BinaryCheckpoint::save` (bincode serialization of the catalog).
///
/// Asserts `binary_elapsed * 3 < sql_elapsed` and prints the speedup.
#[test]
fn test_checkpoint_binary_faster_than_sql() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // Use in_memory() to avoid WAL fsync overhead during the insert phase
    // (we're benchmarking the checkpoint write, not the insert path).
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE bench_t (id INT, v INT)").expect("create");

    // Insert 10,000 rows in chunks of 1,000 (multi-row INSERT to keep the
    // total statement count manageable; avoids parser limits on a single
    // 10,000-row INSERT).
    for chunk in 0..10u64 {
        let start = chunk * 1000;
        let values: Vec<String> = (0..1000u64)
            .map(|i| format!("{}, {}", start + i, (start + i) * 2))
            .collect();
        let sql = format!("INSERT INTO bench_t VALUES ({})", values.join("), ("));
        engine.execute(&sql).expect("insert chunk");
    }
    let r = engine.execute("SELECT count(*) FROM bench_t").expect("count");
    assert_eq!(r.scalar_u64(), Some(10_000), "10,000 rows should be present");

    let sql_path = data_dir.join("bench_checkpoint.sql");
    let bin_path = data_dir.join("bench_checkpoint.bin");

    // Time the legacy SQL-text checkpoint.
    let sql_start = std::time::Instant::now();
    turbogp::storage::recovery::Checkpoint::save(&engine.catalog, &sql_path)
        .expect("legacy checkpoint save");
    let sql_elapsed = sql_start.elapsed();

    // Time the binary checkpoint.
    let bin_start = std::time::Instant::now();
    turbogp::storage::checkpoint::BinaryCheckpoint::save(&engine.catalog, &bin_path)
        .expect("binary checkpoint save");
    let bin_elapsed = bin_start.elapsed();

    let sql_us = sql_elapsed.as_micros();
    let bin_us = bin_elapsed.as_micros();
    let speedup = (sql_us as f64) / (bin_us as f64).max(1.0);
    eprintln!(
        "checkpoint benchmark (10,000 rows): sql={}μs, bin={}μs, speedup={:.2}x",
        sql_us, bin_us, speedup
    );

    // Verify both checkpoint files were written.
    assert!(sql_path.exists(), "SQL checkpoint file should exist");
    assert!(bin_path.exists(), "binary checkpoint file should exist");

    // Assert binary is ≥3x faster (binary_elapsed * 3 < sql_elapsed).
    assert!(
        bin_elapsed * 3 < sql_elapsed,
        "binary checkpoint should be ≥3x faster than SQL-text: \
         bin={:?}, sql={:?}, speedup={:.2}x",
        bin_elapsed,
        sql_elapsed,
        speedup
    );
}
