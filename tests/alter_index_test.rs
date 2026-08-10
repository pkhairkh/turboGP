//! ALTER TABLE + CREATE INDEX integration tests.

use turbogp::engine::QueryEngine;

#[test]
fn alter_table_add_column() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)").unwrap();
    let r = e.execute("ALTER TABLE t ADD COLUMN w INT DEFAULT 0");
    assert!(r.is_ok(), "ALTER TABLE ADD COLUMN must succeed; got: {:?}", r.err());
    // Verify the new column exists by selecting it.
    let r = e.execute("SELECT w FROM t").unwrap();
    assert_eq!(r.row_count, 2);
}

#[test]
fn create_index_basic() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)").unwrap();
    let r = e.execute("CREATE INDEX idx_v ON t (v)");
    assert!(r.is_ok(), "CREATE INDEX must succeed; got: {:?}", r.err());
}

#[test]
fn drop_index_basic() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    let r = e.execute("DROP INDEX idx_v");
    assert!(r.is_ok(), "DROP INDEX must succeed; got: {:?}", r.err());
}
