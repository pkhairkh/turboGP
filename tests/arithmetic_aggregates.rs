//! Wave 40 — Arithmetic expressions in aggregates.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::in_memory();
    e.execute("CREATE TABLE lineitem (id INT, price INT, discount INT)").unwrap();
    e.execute("INSERT INTO lineitem (id, price, discount) VALUES (1, 100, 10)").unwrap();
    e.execute("INSERT INTO lineitem (id, price, discount) VALUES (2, 200, 20)").unwrap();
    e.execute("INSERT INTO lineitem (id, price, discount) VALUES (3, 300, 30)").unwrap();
    e
}

#[test]
fn sum_arithmetic_expr() {
    let mut e = make_engine();
    // SUM(price * 2) = (100*2) + (200*2) + (300*2) = 1200
    let r = e.execute("SELECT sum(price * 2) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 1200.0).abs() < 0.01, "expected 1200.0, got {val}");
}

#[test]
fn sum_with_addition() {
    let mut e = make_engine();
    // SUM(price + discount) = (100+10) + (200+20) + (300+30) = 660
    let r = e.execute("SELECT sum(price + discount) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 660.0).abs() < 0.01, "expected 660.0, got {val}");
}

#[test]
fn sum_with_subtraction() {
    let mut e = make_engine();
    // SUM(price - discount) = (100-10) + (200-20) + (300-30) = 540
    let r = e.execute("SELECT sum(price - discount) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 540.0).abs() < 0.01, "expected 540.0, got {val}");
}

#[test]
fn sum_with_parentheses() {
    let mut e = make_engine();
    // SUM((price - discount) * 2) = (90*2) + (180*2) + (270*2) = 1080
    let r = e.execute("SELECT sum((price - discount) * 2) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 1080.0).abs() < 0.01, "expected 1080.0, got {val}");
}

#[test]
fn sum_multiplication_two_columns() {
    let mut e = make_engine();
    // SUM(price * discount) = (100*10) + (200*20) + (300*30) = 14000
    let r = e.execute("SELECT sum(price * discount) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 14000.0).abs() < 0.01, "expected 14000.0, got {val}");
}

#[test]
fn sum_simple_column_still_works() {
    let mut e = make_engine();
    // SUM(price) = 100 + 200 + 300 = 600 (regression check)
    let r = e.execute("SELECT sum(price) FROM lineitem").unwrap();
    let val = r.scalar_f64().expect("f64");
    assert!((val - 600.0).abs() < 0.01, "expected 600.0, got {val}");
}

#[test]
fn count_star_still_works() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM lineitem").unwrap();
    assert_eq!(r.scalar_u64(), Some(3));
}
