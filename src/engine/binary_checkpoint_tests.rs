//! Binary checkpoint integration tests (Task 4.1 + 4.2).
//!
//! Extracted from `src/engine/mod.rs` in Task 8.2-fix to satisfy the
//! 2000-LOC file-size limit. These tests verify the binary-checkpoint
//! persistence path end-to-end via `QueryEngine::with_data_dir`.

#![cfg(test)]

use super::*;
use tempfile::TempDir;

/// End-to-end persistence test: write 100 rows, CHECKPOINT, drop the
/// engine, reload via `with_data_dir`, and verify all 100 rows survive.
/// Also verifies `checkpoint.bin` exists after CHECKPOINT.
#[test]
fn test_binary_checkpoint_persistence() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // Phase 1: create a table, insert 100 rows, CHECKPOINT.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir");
        engine
            .execute("CREATE TABLE t (id INT, v INT)")
            .expect("create table");
        for i in 0..100 {
            let sql = format!("INSERT INTO t VALUES ({i}, {})", i * 2);
            engine.execute(&sql).expect("insert");
        }
        let r = engine.execute("SELECT count(*) FROM t").expect("count");
        assert_eq!(r.scalar_u64(), Some(100), "expected 100 rows before checkpoint");

        engine.execute("CHECKPOINT").expect("checkpoint");

        // Verify checkpoint.bin was written.
        let bin_path = data_dir.join("checkpoint.bin");
        assert!(bin_path.exists(), "checkpoint.bin should exist after CHECKPOINT");
        // The legacy checkpoint.sql is also written for backward compat.
        let sql_path = data_dir.join("checkpoint.sql");
        assert!(sql_path.exists(), "checkpoint.sql should also exist for backward compat");
    }
    // Phase 2: drop the engine and reload via with_data_dir.
    // The catalog should be restored from checkpoint.bin.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir reload");
        let r = engine.execute("SELECT count(*) FROM t").expect("count after reload");
        assert_eq!(
            r.scalar_u64(),
            Some(100),
            "expected 100 rows after reload from checkpoint.bin"
        );
        // Verify a specific row round-trips.
        let r = engine
            .execute("SELECT v FROM t WHERE id = 42")
            .expect("select v where id=42");
        // v = id * 2 = 84.
        assert_eq!(r.scalar_u64(), Some(84), "row id=42 should have v=84");
    }
}

/// If `checkpoint.bin` is missing, `with_data_dir` falls back to the
/// legacy SQL checkpoint. This verifies backward compat with data dirs
/// written by older engine versions.
#[test]
fn test_with_data_dir_falls_back_to_sql_checkpoint() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // Phase 1: write some data + checkpoint, then delete checkpoint.bin
    // to simulate an old data dir.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir");
        engine.execute("CREATE TABLE t (id INT)").expect("create");
        engine.execute("INSERT INTO t VALUES (7)").expect("insert");
        engine.execute("CHECKPOINT").expect("checkpoint");
        // Remove the binary checkpoint to force the legacy path.
        std::fs::remove_file(data_dir.join("checkpoint.bin")).expect("remove bin");
    }
    // Phase 2: reload — should fall back to checkpoint.sql.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir reload");
        let r = engine.execute("SELECT count(*) FROM t").expect("count after reload");
        assert_eq!(r.scalar_u64(), Some(1), "row should survive via SQL checkpoint fallback");
        let r = engine.execute("SELECT id FROM t").expect("select id");
        assert_eq!(r.scalar_u64(), Some(7));
    }
}

/// After CHECKPOINT, the WAL is truncated. New writes after the
/// checkpoint land in the WAL; on reload, the binary checkpoint
/// restores the checkpoint state, then WAL replay applies the
/// post-checkpoint writes.
#[test]
fn test_binary_checkpoint_then_wal_replay() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // Phase 1: insert 5 rows, CHECKPOINT, then insert 3 more rows.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir");
        engine.execute("CREATE TABLE t (id INT)").expect("create");
        for i in 0..5 {
            engine.execute(&format!("INSERT INTO t VALUES ({i})")).expect("insert");
        }
        engine.execute("CHECKPOINT").expect("checkpoint");
        // 3 more rows after the checkpoint — these go into the WAL.
        for i in 5..8 {
            engine.execute(&format!("INSERT INTO t VALUES ({i})")).expect("insert post-cp");
        }
        let r = engine.execute("SELECT count(*) FROM t").expect("count pre-reload");
        assert_eq!(r.scalar_u64(), Some(8));
    }
    // Phase 2: reload. Binary checkpoint restores 5 rows; WAL replay
    // applies the 3 post-checkpoint inserts. Total = 8.
    {
        let mut engine = QueryEngine::with_data_dir(data_dir).expect("with_data_dir reload");
        let r = engine.execute("SELECT count(*) FROM t").expect("count after reload");
        assert_eq!(
            r.scalar_u64(),
            Some(8),
            "binary checkpoint (5) + WAL replay (3) should yield 8 rows"
        );
    }
}
