//! Wave 16 — ClickBench parser fixes: <> operator, BETWEEN, IN list.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE hits (id INT, val INT, name VARCHAR(50))").unwrap();
    e.execute("INSERT INTO hits (id, val, name) VALUES (1, 10, 'alpha'), (2, 20, 'beta'), (3, 30, 'gamma'), (4, 40, 'delta'), (5, 50, 'epsilon')").unwrap();
    e
}

#[test]
fn not_equal_operator() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val <> 10").unwrap();
    assert_eq!(r.scalar_u64(), Some(4));
}

#[test]
fn not_equal_string() {
    let mut e = make_engine();
    // <> '' would be used in ClickBench Q10/Q12 to filter empty strings.
    let r = e.execute("SELECT count(*) FROM hits WHERE val <> 0").unwrap();
    assert_eq!(r.scalar_u64(), Some(5));
}

#[test]
fn between_integers() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val BETWEEN 20 AND 40").unwrap();
    assert_eq!(r.scalar_u64(), Some(3)); // val=20,30,40
}

#[test]
fn between_inclusive_bounds() {
    let mut e = make_engine();
    // BETWEEN is inclusive on both ends.
    let r = e.execute("SELECT count(*) FROM hits WHERE val BETWEEN 10 AND 30").unwrap();
    assert_eq!(r.scalar_u64(), Some(3)); // val=10,20,30
}

#[test]
fn not_between() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val NOT BETWEEN 20 AND 40").unwrap();
    assert_eq!(r.scalar_u64(), Some(2)); // val=10,50
}

#[test]
fn in_list_integers() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val IN (10, 30, 50)").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn in_list_single_element() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val IN (42)").unwrap();
    assert_eq!(r.scalar_u64(), Some(0));
}

#[test]
fn not_in_list() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM hits WHERE val NOT IN (10, 30, 50)").unwrap();
    assert_eq!(r.scalar_u64(), Some(2)); // val=20,40
}

#[test]
fn combined_between_and_in() {
    let mut e = make_engine();
    let r = e
        .execute("SELECT count(*) FROM hits WHERE val BETWEEN 10 AND 40 AND val IN (10, 20, 30)")
        .unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn combined_not_equal_and_like() {
    let mut e = make_engine();
    // This pattern appears in ClickBench Q13: LIKE + equality.
    let r = e.execute("SELECT count(*) FROM hits WHERE val > 10 AND val < 50").unwrap();
    assert_eq!(r.scalar_u64(), Some(3)); // val=20,30,40
}
