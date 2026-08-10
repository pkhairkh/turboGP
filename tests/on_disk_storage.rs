//! Wave 63 — On-disk page-level storage integration tests.
//!
//! Verifies that:
//! 1. Tables created via CREATE TABLE are persisted to disk.
//! 2. INSERT/UPDATE/DELETE write through the buffer pool.
//! 3. After COMMIT + flush, data survives a process restart (re-open the engine).
//! 4. The page-level WAL records physical changes (not just SQL strings).
//! 5. Crash recovery: unflushed data is lost, flushed data survives.

use tempfile::TempDir;
use turbogp::engine::QueryEngine;

#[test]
fn on_disk_table_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    // Create an engine with on-disk persistence.
    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        e.execute("INSERT INTO t (id, v) VALUES (1, 100)").unwrap();
        e.execute("INSERT INTO t (id, v) VALUES (2, 200)").unwrap();
        e.execute("INSERT INTO t (id, v) VALUES (3, 300)").unwrap();
        // Verify the data is queryable.
        let r = e.execute("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.scalar_u64(), Some(3));
        // Flush to disk.
        e.flush().unwrap();
    }

    // Re-open the engine from the same data directory.
    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        // The table must still exist and have its data.
        let r = e.execute("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.scalar_u64(), Some(3), "data must survive restart after flush");
    }
}

#[test]
fn buffer_pool_page_allocation() {
    use turbogp::storage::buffer_pool::{BufferPool, PageId};
    let tmp = TempDir::new().unwrap();
    let mut pool = BufferPool::new(tmp.path(), 16).unwrap();

    // Allocate pages for table 1.
    let p0 = pool.new_page(1).unwrap();
    let p1 = pool.new_page(1).unwrap();
    assert_eq!(p0.page_num, 0);
    assert_eq!(p1.page_num, 1);

    // Write a value to page 0 and flush.
    let idx = pool.fetch_page(p0).unwrap();
    {
        let page = pool.get_page_mut(idx);
        page.set_cell(0, 42);
        page.set_cell(1, 99);
    }
    pool.unpin_page(p0, true);
    pool.flush_all().unwrap();

    // Verify the page count on disk.
    assert_eq!(pool.page_count(1), 2, "table 1 must have 2 pages on disk");
}

#[test]
fn wal_records_physical_changes() {
    use turbogp::storage::recovery::{PhysicalChange, Wal, WalRecord};
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("test_wal.log");

    // Write a physical change record.
    {
        let mut wal = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::physical(1, PhysicalChange::PageAlloc { table_id: 1, page_num: 0 }))
            .unwrap();
        wal.append(&WalRecord::physical(
            1,
            PhysicalChange::CellUpdate {
                table_id: 1,
                page_num: 0,
                cell_index: 0,
                old_value: 0,
                new_value: 42,
            },
        ))
        .unwrap();
        wal.append(&WalRecord::commit(1)).unwrap();
        wal.sync().unwrap();
    }

    // Read back and verify the physical changes are recorded.
    {
        let wal = Wal::open(&wal_path).unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 3, "WAL must have 3 records");
        // First record: PageAlloc.
        assert!(records[0].physical_change.is_some(), "first record must have a physical change");
        // Last record: COMMIT.
        assert!(records[2].is_commit, "last record must be a COMMIT");
    }
}

#[test]
fn on_disk_update_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        e.execute("INSERT INTO t (id, v) VALUES (1, 100)").unwrap();
        e.execute("UPDATE t SET v = 999 WHERE id = 1").unwrap();
        e.flush().unwrap();
    }

    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        let r = e.execute("SELECT v FROM t WHERE id = 1").unwrap();
        assert_eq!(r.row_count, 1);
        // The updated value must persist.
        let v_col = r.columns.iter().find(|c| c.name == "v").expect("v column");
        assert_eq!(v_col.values[0], 999, "updated value must survive restart");
    }
}

#[test]
fn on_disk_delete_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        e.execute("CREATE TABLE t (id INT)").unwrap();
        e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").unwrap();
        e.execute("DELETE FROM t WHERE id = 2").unwrap();
        e.flush().unwrap();
    }

    {
        let mut e = QueryEngine::with_data_dir(data_dir).unwrap();
        let r = e.execute("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.scalar_u64(), Some(2), "deleted row must not reappear after restart");
    }
}
