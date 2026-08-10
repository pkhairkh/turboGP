//! Native type tests (Wave 70): JSON, UUID, BYTEA, ARRAY, ENUM.

use turbogp::engine::QueryEngine;

#[test]
fn create_table_with_json_column() {
    let mut e = QueryEngine::in_memory();
    let r = e.execute("CREATE TABLE docs (id INT, payload JSON)");
    assert!(r.is_ok(), "CREATE TABLE with JSON column must succeed; got: {:?}", r.err());
}

#[test]
fn create_table_with_uuid_column() {
    let mut e = QueryEngine::in_memory();
    let r = e.execute("CREATE TABLE t (id UUID, name VARCHAR)");
    assert!(r.is_ok(), "CREATE TABLE with UUID column must succeed; got: {:?}", r.err());
}

#[test]
fn create_table_with_bytea_column() {
    let mut e = QueryEngine::in_memory();
    let r = e.execute("CREATE TABLE t (id INT, data BYTEA)");
    assert!(r.is_ok(), "CREATE TABLE with BYTEA column must succeed; got: {:?}", r.err());
}

#[test]
fn create_table_with_array_column() {
    let mut e = QueryEngine::in_memory();
    let r = e.execute("CREATE TABLE t (id INT, tags ARRAY)");
    assert!(r.is_ok(), "CREATE TABLE with ARRAY column must succeed; got: {:?}", r.err());
}

#[test]
fn json_column_accepts_json_value() {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE docs (id INT, payload JSON)").unwrap();
    // Insert a JSON string — it's stored as a VARCHAR sidecar.
    let r = e.execute("INSERT INTO docs (id, payload) VALUES (1, '{\"name\":\"Alice\"}')");
    assert!(r.is_ok(), "INSERT into JSON column must succeed; got: {:?}", r.err());
    // JSON_VALUE should work on the JSON column.
    let r = e.execute("SELECT JSON_VALUE(payload, '$.name') FROM docs").unwrap();
    assert_eq!(r.row_count, 1);
    let strings = r.columns[0].string_values.as_ref().unwrap();
    assert_eq!(strings[0], "Alice");
}
