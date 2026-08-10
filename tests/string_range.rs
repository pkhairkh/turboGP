//! Wave 42 — Range predicates on string columns.

use std::io::Write;
use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "id,name").unwrap();
    writeln!(tmp, "1,Alice").unwrap();
    writeln!(tmp, "2,Bob").unwrap();
    writeln!(tmp, "3,Charlie").unwrap();
    writeln!(tmp, "4,David").unwrap();
    writeln!(tmp, "5,Eve").unwrap();
    tmp.flush().unwrap();
    let mut e = QueryEngine::in_memory();
    e.load_csv(tmp.path().to_str().unwrap(), "users", true).unwrap();
    e
}

#[test]
fn where_string_greater_than() {
    let mut e = make_engine();
    // name > 'C' → Charlie, David, Eve → 3 rows
    let r = e.execute("SELECT count(*) FROM users WHERE name > 'C'").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn where_string_less_than() {
    let mut e = make_engine();
    // name < 'D' → Alice, Bob, Charlie → 3 rows
    let r = e.execute("SELECT count(*) FROM users WHERE name < 'D'").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn where_string_greater_equal() {
    let mut e = make_engine();
    // name >= 'Charlie' → Charlie, David, Eve → 3 rows
    let r = e.execute("SELECT count(*) FROM users WHERE name >= 'Charlie'").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn where_string_less_equal() {
    let mut e = make_engine();
    // name <= 'Bob' → Alice, Bob → 2 rows
    let r = e.execute("SELECT count(*) FROM users WHERE name <= 'Bob'").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn where_string_not_equal() {
    let mut e = make_engine();
    // name != 'Alice' → Bob, Charlie, David, Eve → 4 rows
    let r = e.execute("SELECT count(*) FROM users WHERE name != 'Alice'").unwrap();
    assert_eq!(r.scalar_u64(), Some(4));
}

#[test]
fn where_string_range_combined() {
    let mut e = make_engine();
    // name > 'B' AND name < 'E' → Charlie, David, Eve → 3 rows
    // ('Bob' > 'B' is false since 'Bob' starts with 'B' but is longer, so 'Bob' > 'B' is true...
    //  actually 'Bob' > 'B' IS true because 'Bob' is lexicographically after 'B')
    // So: Bob, Charlie, David → 3 (Eve < 'E' is false since 'Eve' > 'E')
    let r = e.execute("SELECT count(*) FROM users WHERE name > 'B' AND name < 'E'").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn where_string_equals_still_works() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM users WHERE name = 'Alice'").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}

#[test]
fn where_numeric_range_still_works() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM users WHERE id > 2").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}
