//! Wave 21 — Original strings from SELECT.
//!
//! Verifies that SELECT on string columns returns readable strings via
//! ResultColumn.string_values, not just xxh3 hashes.

use turbogp::engine::QueryEngine;

fn make_engine_with_csv() -> QueryEngine {
    // We need a CSV-loaded table to get StringSearchColumn sidecars,
    // which DDL+INSERT doesn't build.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "id,name,score").unwrap();
    writeln!(tmp, "1,Alice,100").unwrap();
    writeln!(tmp, "2,Bob,200").unwrap();
    writeln!(tmp, "3,Carol,300").unwrap();
    tmp.flush().unwrap();

    let mut e = QueryEngine::in_memory();
    e.load_csv(tmp.path().to_str().unwrap(), "users", true).unwrap();
    e
}

#[test]
fn select_string_column_returns_strings() {
    let mut e = make_engine_with_csv();
    let r = e.execute("SELECT name FROM users").unwrap();
    assert_eq!(r.row_count, 3);
    assert_eq!(r.columns.len(), 1);
    let col = &r.columns[0];
    assert!(col.has_strings(), "column should have string_values");
    assert_eq!(col.get_string(0), Some("Alice"));
    assert_eq!(col.get_string(1), Some("Bob"));
    assert_eq!(col.get_string(2), Some("Carol"));
}

#[test]
fn select_string_with_where() {
    let mut e = make_engine_with_csv();
    let r = e.execute("SELECT name FROM users WHERE id = 2").unwrap();
    assert_eq!(r.row_count, 1);
    let col = &r.columns[0];
    assert!(col.has_strings());
    assert_eq!(col.get_string(0), Some("Bob"));
}

#[test]
fn select_string_with_limit() {
    let mut e = make_engine_with_csv();
    let r = e.execute("SELECT name FROM users LIMIT 2").unwrap();
    assert_eq!(r.row_count, 2);
    let col = &r.columns[0];
    assert!(col.has_strings());
    assert_eq!(col.get_string(0), Some("Alice"));
    assert_eq!(col.get_string(1), Some("Bob"));
}

#[test]
fn select_numeric_column_no_strings() {
    let mut e = make_engine_with_csv();
    let r = e.execute("SELECT score FROM users").unwrap();
    assert_eq!(r.row_count, 3);
    let col = &r.columns[0];
    assert!(!col.has_strings(), "numeric column should not have string_values");
    assert_eq!(col.values, vec![100, 200, 300]);
}

#[test]
fn count_star_no_strings() {
    let mut e = make_engine_with_csv();
    let r = e.execute("SELECT count(*) FROM users").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
    assert!(!r.columns[0].has_strings());
}

#[test]
fn from_strings_constructor() {
    use turbogp::engine::ResultColumn;
    let col = ResultColumn::from_strings("name", vec!["Alice".into(), "Bob".into()]);
    assert!(col.has_strings());
    assert_eq!(col.get_string(0), Some("Alice"));
    assert_eq!(col.get_string(1), Some("Bob"));
    // The u64 values should be xxh3 hashes.
    assert_eq!(col.values.len(), 2);
}

#[test]
fn from_u64_constructor() {
    use turbogp::engine::ResultColumn;
    let col = ResultColumn::from_u64("count", vec![1, 2, 3]);
    assert!(!col.has_strings());
    assert_eq!(col.values, vec![1, 2, 3]);
}
