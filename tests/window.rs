//! Wave 7 — Window function integration tests.
//!
//! These tests verify the window function implementations by calling them
//! directly on QueryResult structures. Integration with the SQL parser
//! (parsing `OVER (...)` in a SELECT list) will be added in a later
//! refinement; for now we test the computational core.

use turbogp::engine::{QueryResult, ResultColumn};
use turbogp::exec::window::*;

fn make_result(names: &[&str], cols: &[Vec<u64>]) -> QueryResult {
    let mut r = QueryResult::empty();
    for (i, name) in names.iter().enumerate() {
        r.push_column(ResultColumn {
            name: name.to_string(),
            values: cols[i].clone(),
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .unwrap();
    }
    r
}

fn empty_spec(partition: &[&str], order: &[(&str, bool)]) -> WindowSpec {
    WindowSpec {
        partition_by: partition.iter().map(|s| s.to_string()).collect(),
        order_by: order.iter().map(|(s, b)| (s.to_string(), *b)).collect(),
        frame_type: None,
        frame_start: None,
        frame_end: None,
    }
}

#[test]
fn row_number_department_ranking() {
    // Employees: dept 1 has salaries 50k, 30k, 40k; dept 2 has 60k, 35k.
    let r = make_result(
        &["dept", "salary"],
        &[vec![1, 1, 1, 2, 2], vec![50000, 30000, 40000, 60000, 35000]],
    );
    let spec = empty_spec(&["dept"], &[("salary", false)]);
    let rn = row_number(&r, &spec);
    // Dept 1 sorted by salary DESC: 50k(1), 40k(2), 30k(3).
    // Dept 2 sorted by salary DESC: 60k(1), 35k(2).
    assert_eq!(rn, vec![1, 3, 2, 1, 2]);
}

#[test]
fn rank_with_ties_in_same_partition() {
    let r = make_result(&["v"], &[vec![10, 20, 20, 30]]);
    let spec = empty_spec(&[], &[("v", true)]);
    let rk = rank(&r, &spec);
    assert_eq!(rk, vec![1, 2, 2, 4]);
}

#[test]
fn dense_rank_no_gaps() {
    let r = make_result(&["v"], &[vec![10, 20, 20, 30]]);
    let spec = empty_spec(&[], &[("v", true)]);
    let dr = dense_rank(&r, &spec);
    assert_eq!(dr, vec![1, 2, 2, 3]);
}

#[test]
fn running_total_per_partition() {
    // Dept 1: salaries 100, 200, 300. Dept 2: 50, 150.
    let r = make_result(&["dept", "salary"], &[vec![1, 1, 1, 2, 2], vec![100, 200, 300, 50, 150]]);
    let spec = empty_spec(&["dept"], &[("salary", true)]);
    let rt = sum_over(&r, "salary", &spec);
    // Dept 1 sorted ASC: 100→100, 200→300, 300→600.
    // Dept 2 sorted ASC: 50→50, 150→200.
    assert_eq!(rt, vec![100, 300, 600, 50, 200]);
}

#[test]
fn count_per_partition() {
    let r = make_result(&["dept"], &[vec![1, 1, 2, 2, 2]]);
    let spec = empty_spec(&["dept"], &[]);
    let c = count_over(&r, &spec);
    assert_eq!(c, vec![2, 2, 3, 3, 3]);
}

#[test]
fn lag_previous_value() {
    let r = make_result(&["v"], &[vec![10, 20, 30]]);
    let spec = empty_spec(&[], &[("v", true)]);
    let l = lag(&r, "v", 1, 0, &spec);
    assert_eq!(l, vec![0, 10, 20]);
}

#[test]
fn lead_next_value() {
    let r = make_result(&["v"], &[vec![10, 20, 30]]);
    let spec = empty_spec(&[], &[("v", true)]);
    let l = lead(&r, "v", 1, 0, &spec);
    assert_eq!(l, vec![20, 30, 0]);
}

#[test]
fn first_value_in_partition() {
    let r = make_result(&["dept", "v"], &[vec![1, 1, 2, 2], vec![30, 10, 50, 20]]);
    let spec = empty_spec(&["dept"], &[("v", true)]);
    let fv = first_value(&r, "v", &spec);
    // Dept 1 sorted ASC: first = 10. Dept 2 sorted ASC: first = 20.
    assert_eq!(fv, vec![10, 10, 20, 20]);
}

#[test]
fn parse_complex_window_spec() {
    let spec = parse_window_spec(
        "PARTITION BY dept, team ORDER BY salary DESC, hire_date ROWS BETWEEN 2 PRECEDING AND CURRENT ROW",
    )
    .unwrap();
    assert_eq!(spec.partition_by, vec!["dept", "team"]);
    assert_eq!(spec.order_by, vec![("salary".into(), false), ("hire_date".into(), true)]);
    assert_eq!(spec.frame_type, Some("ROWS".into()));
    assert_eq!(spec.frame_start, Some("2 PRECEDING".into()));
    assert_eq!(spec.frame_end, Some("CURRENT ROW".into()));
}

#[test]
fn empty_result_handling() {
    let r = QueryResult::empty();
    let spec = empty_spec(&[], &[("v", true)]);
    assert_eq!(row_number(&r, &spec), Vec::<u64>::new());
    assert_eq!(rank(&r, &spec), Vec::<u64>::new());
    assert_eq!(dense_rank(&r, &spec), Vec::<u64>::new());
}

#[test]
fn single_row_partition() {
    let r = make_result(&["v"], &[vec![42]]);
    let spec = empty_spec(&[], &[("v", true)]);
    assert_eq!(row_number(&r, &spec), vec![1]);
    assert_eq!(rank(&r, &spec), vec![1]);
    assert_eq!(dense_rank(&r, &spec), vec![1]);
    assert_eq!(sum_over(&r, "v", &spec), vec![42]);
    assert_eq!(count_over(&r, &spec), vec![1]);
}
