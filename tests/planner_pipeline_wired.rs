//! Wave 1 — Agent C: verify the planner pipeline is wired into `execute()`.
//!
//! These tests prove that `QueryEngine::execute("SELECT ...")` invokes the
//! full planner pipeline (`build_plan → Cascades → PlanLowerer → Scheduler`)
//! from the production execution path, not just from
//! `tests/kernel_pipeline_test.rs`.

use turbogp::engine::{planner_pipeline_invoked_count, reset_planner_pipeline_counter, QueryEngine};

#[test]
fn test_select_star_invokes_planner_pipeline() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    engine.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    engine.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();
    engine.execute("INSERT INTO t VALUES (3, 'carol')").unwrap();

    // Reset the counter, run a SELECT *, and verify the planner was invoked.
    reset_planner_pipeline_counter();
    let result = engine.execute("SELECT * FROM t").unwrap();
    assert_eq!(result.row_count, 3, "SELECT * should return 3 rows");
    assert!(
        planner_pipeline_invoked_count() >= 1,
        "planner pipeline must be invoked from execute(), got count={}",
        planner_pipeline_invoked_count()
    );
}

#[test]
fn test_count_star_invokes_planner_pipeline() {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..5 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }

    reset_planner_pipeline_counter();
    let result = engine.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(result.row_count, 1, "COUNT(*) should return 1 row");
    assert!(
        result.columns[0].values[0] == 5,
        "COUNT(*) should return 5, got {}",
        result.columns[0].values[0]
    );
    assert!(
        planner_pipeline_invoked_count() >= 1,
        "planner pipeline must be invoked for COUNT(*), got count={}",
        planner_pipeline_invoked_count()
    );
}

#[test]
fn test_filtered_select_still_invokes_planner() {
    // A filtered SELECT falls back to the direct path for results, but the
    // planner pipeline must still be invoked (the reachability counter must
    // be incremented).
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..10 {
        engine.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }

    reset_planner_pipeline_counter();
    let result = engine.execute("SELECT * FROM t WHERE id = 5").unwrap();
    assert!(result.row_count >= 1, "filtered SELECT should return ≥1 row");
    assert!(
        planner_pipeline_invoked_count() >= 1,
        "planner pipeline must be invoked even when falling back, got count={}",
        planner_pipeline_invoked_count()
    );
}

#[test]
fn test_explain_uses_planner_plan_tree() {
    // Wave 1 Task 1.2: EXPLAIN must print the planner's plan tree (with
    // Scan, Filter, etc.) — not the legacy string-based description.
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    engine.execute("INSERT INTO t VALUES (5, 'alice')").unwrap();

    let result = engine.execute("EXPLAIN SELECT * FROM t WHERE id = 5").unwrap();
    assert_eq!(result.row_count, 1, "EXPLAIN returns 1 row");
    assert_eq!(result.columns.len(), 1, "EXPLAIN returns 1 column");
    assert_eq!(result.columns[0].name, "QUERY PLAN");

    // The plan text must come from the planner's PlanNode Display impl,
    // which prints "Scan(table=...)" and "Filter(pred=[...])" lines.
    let plan_text = result.columns[0]
        .string_values
        .as_ref()
        .and_then(|v| v.first())
        .expect("EXPLAIN must produce plan text");
    assert!(
        plan_text.contains("Scan"),
        "plan tree must contain 'Scan' (got: {})",
        plan_text
    );
    assert!(
        plan_text.contains("Filter"),
        "plan tree must contain 'Filter' (got: {})",
        plan_text
    );
    assert!(
        plan_text.contains("t"),
        "plan tree must reference the table 't' (got: {})",
        plan_text
    );
}
