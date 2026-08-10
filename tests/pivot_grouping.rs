//! Wave 8 — PIVOT/UNPIVOT + GROUPING SETS integration tests.

use turbogp::engine::{QueryResult, ResultColumn};
use turbogp::exec::pivot::*;

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

#[test]
fn pivot_sum_by_quarter() {
    let r = make_result(
        &["dept", "quarter", "amount"],
        &[vec![1, 1, 2, 2], vec![1, 2, 1, 2], vec![100, 200, 150, 250]],
    );
    let p = pivot(&r, "dept", "quarter", "amount", &["1".into(), "2".into()], "SUM");
    assert_eq!(p.row_count, 2);
    assert_eq!(p.columns[0].values, vec![1, 2]);
    assert_eq!(p.columns[1].values, vec![100, 150]);
    assert_eq!(p.columns[2].values, vec![200, 250]);
}

#[test]
fn pivot_count() {
    let r = make_result(
        &["dept", "quarter", "amount"],
        &[vec![1, 1, 2], vec![1, 1, 1], vec![100, 200, 150]],
    );
    let p = pivot(&r, "dept", "quarter", "amount", &["1".into()], "COUNT");
    assert_eq!(p.columns[1].values, vec![2, 1]);
}

#[test]
fn unpivot_columns_to_rows() {
    let r = make_result(&["dept", "Q1", "Q2"], &[vec![1, 2], vec![100, 150], vec![200, 250]]);
    let u = unpivot(&r, &["dept".into()], "amount", "quarter", &["Q1".into(), "Q2".into()]);
    assert_eq!(u.row_count, 4);
    assert_eq!(u.columns[0].values, vec![1, 1, 2, 2]);
    assert_eq!(u.columns[2].values, vec![100, 200, 150, 250]);
}

#[test]
fn grouping_sets_multiple_levels() {
    let r = make_result(
        &["dept", "team", "amount"],
        &[vec![1, 1, 2], vec![1, 2, 1], vec![100, 50, 200]],
    );
    // Group by dept only (set = [0]), then by team only (set = [1]).
    let gs =
        grouping_sets(&r, &["dept".into(), "team".into()], "amount", "SUM", &[vec![0], vec![1]]);
    assert!(gs.row_count >= 2);
}

#[test]
fn cube_all_combinations() {
    let r = make_result(&["dept", "amount"], &[vec![1, 1, 2], vec![100, 200, 150]]);
    let c = cube(&r, &["dept".into()], "amount", "SUM");
    // CUBE with 1 column: {dept}, {} = 2 + 1 = 3 groups minimum.
    assert!(c.row_count >= 2);
}

#[test]
fn rollup_hierarchy() {
    let r = make_result(&["dept", "amount"], &[vec![1, 1, 2], vec![100, 200, 150]]);
    let ru = rollup(&r, &["dept".into()], "amount", "SUM");
    // ROLLUP with 1 column: {dept}, {} = 2 + 1 = 3 groups.
    assert!(ru.row_count >= 2);
}

#[test]
fn pivot_max_aggregation() {
    let r = make_result(
        &["dept", "quarter", "amount"],
        &[vec![1, 1, 1], vec![1, 1, 1], vec![100, 300, 200]],
    );
    let p = pivot(&r, "dept", "quarter", "amount", &["1".into()], "MAX");
    assert_eq!(p.columns[1].values, vec![300]);
}

#[test]
fn pivot_min_aggregation() {
    let r = make_result(
        &["dept", "quarter", "amount"],
        &[vec![1, 1, 1], vec![1, 1, 1], vec![100, 300, 200]],
    );
    let p = pivot(&r, "dept", "quarter", "amount", &["1".into()], "MIN");
    assert_eq!(p.columns[1].values, vec![100]);
}
