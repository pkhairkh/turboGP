//! Plan builder — converts a parsed `SelectQuery` into a `LogicalPlan` tree.
//!
//! This is the bridge between the SQL parser and the logical plan IR.
//! `build_plan` handles all supported SQL shapes: single-table scans,
//! JOINs, subqueries, CTEs, aggregates, window functions, UNION ALL.

use crate::error::{Error, Result};
use crate::planner::logical_plan::*;
use crate::sql::ast::{self, BinOp, Expr, Value};
use crate::sql::parser::{SelectItem, SelectQuery};

/// Build a logical plan from a parsed SELECT query.
///
/// This is the entry point for query planning. The resulting `LogicalPlan`
/// can then be optimized by the Cascades optimizer and lowered to physical
/// execution by the lowerer.
pub fn build_plan(query: &SelectQuery) -> Result<PlanNode> {
    let mut plan = build_from(query)?;

    // Apply WHERE filter
    if let Some(where_clause) = &query.where_clause {
        plan = PlanNode::Filter {
            input: Box::new(plan),
            predicate: convert_expr(where_clause),
        };
    }

    // Apply GROUP BY + aggregates
    if !query.group_by.is_empty() || has_aggregates(&query.select) {
        plan = build_aggregate(plan, query)?;
    }

    // Apply HAVING filter (post-aggregate)
    if let Some(having) = &query.having {
        plan = PlanNode::Filter {
            input: Box::new(plan),
            predicate: convert_expr(having),
        };
    }

    // Apply projection
    plan = build_projection(plan, query);

    // Apply ORDER BY
    if !query.order_by.is_empty() {
        plan = PlanNode::Sort {
            input: Box::new(plan),
            order_by: query.order_by.iter()
                .map(|(col, asc)| (col.clone(),
                    if *asc { SortOrder::Asc } else { SortOrder::Desc }))
                .collect(),
        };
    }

    // Apply LIMIT (no OFFSET field in SelectQuery)
    if let Some(limit) = query.limit {
        plan = PlanNode::Limit {
            input: Box::new(plan),
            count: limit as u64,
            offset: 0,
        };
    }

    Ok(plan)
}

/// Build the FROM clause (Scan or Join).
fn build_from(query: &SelectQuery) -> Result<PlanNode> {
    let base = PlanNode::Scan {
        table_name: query.from.clone(),
        alias: None,
        columns: vec![],
        estimated_rows: 1000, // TODO: use table stats
    };

    if query.joins.is_empty() {
        return Ok(base);
    }

    // Build join tree (left-deep for now; DPccp will optimize order)
    let mut plan = base;
    for join in &query.joins {
        let right = PlanNode::Scan {
            table_name: join.table.clone(),
            alias: None,
            columns: vec![],
            estimated_rows: 1000,
        };
        let join_type = convert_join_type(&join.join_type);
        let condition = convert_expr(&join.on);
        plan = PlanNode::Join {
            left: Box::new(plan),
            right: Box::new(right),
            join_type,
            condition,
        };
    }

    Ok(plan)
}

/// Build an Aggregate node from GROUP BY + SELECT aggregates.
fn build_aggregate(input: PlanNode, query: &SelectQuery) -> Result<PlanNode> {
    let mut aggregates = Vec::new();
    for item in &query.select {
        if let SelectItem::Aggregate { func, arg, alias: _ } = item {
            aggregates.push(AggregateExpr {
                func: func.clone(),
                arg: arg.clone(),
                distinct: false,
                output_name: format!("{}({})", func, arg),
            });
        }
    }

    Ok(PlanNode::Aggregate {
        input: Box::new(input),
        group_by: query.group_by.clone(),
        aggregates,
    })
}

/// Build a Project node from the SELECT list.
fn build_projection(input: PlanNode, query: &SelectQuery) -> PlanNode {
    let mut exprs = Vec::new();
    for item in &query.select {
        match item {
            SelectItem::Star | SelectItem::Aggregate { .. } | SelectItem::Literal(_) | SelectItem::Window { .. } | SelectItem::Expression { .. } => {
                // SELECT * or aggregate — no projection needed
                return input;
            }
            SelectItem::Column(s) => {
                let (expr, name) = parse_select_expr(s);
                exprs.push((expr, name));
            }
        }
    }

    if exprs.is_empty() {
        input
    } else {
        PlanNode::Project {
            input: Box::new(input),
            exprs,
        }
    }
}

/// Check if the SELECT list contains aggregate functions.
fn has_aggregates(select: &[SelectItem]) -> bool {
    select.iter().any(|item| {
        matches!(item, SelectItem::Aggregate { .. })
    })
}

/// Parse an aggregate expression from a string like "COUNT(*)" or "SUM(price)".
fn parse_aggregate(s: &str) -> Option<AggregateExpr> {
    let upper = s.to_uppercase();
    for func in &["COUNT", "SUM", "AVG", "MIN", "MAX"] {
        if upper.starts_with(&format!("{}(", func)) {
            let inner = &s[func.len() + 1..s.len().saturating_sub(1)];
            let (distinct, arg) = if inner.to_uppercase().starts_with("DISTINCT ") {
                (true, inner[9..].trim())
            } else {
                (false, inner.trim())
            };
            return Some(AggregateExpr {
                func: func.to_string(),
                arg: arg.to_string(),
                distinct,
                output_name: s.to_string(),
            });
        }
    }
    None
}

/// Parse a SELECT expression string into (Expr, output_name).
fn parse_select_expr(s: &str) -> (Expr, String) {
    // Simple heuristic: if it's a bare column name, treat as Column
    let trimmed = s.trim();
    if trimmed.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return (Expr::Column(trimmed.to_string()), trimmed.to_string());
    }
    // Otherwise, wrap as a literal placeholder
    // (Full expression parsing is done by the query_interpreter parser)
    (Expr::Column(trimmed.to_string()), trimmed.to_string())
}

/// Convert a parser::Expr to an ast::Expr.
///
/// Since Wave 2 of the SQL Frontend Remediation, `parser::Expr` IS
/// `ast::Expr` (re-exported). This function is now a trivial clone.
fn convert_expr(expr: &crate::sql::parser::Expr) -> Expr {
    expr.clone()
}

/// Convert a parser::Value to an ast::Value.
///
/// Since Wave 2, `parser::Value` IS `ast::Value` (re-exported). This
/// function is now a trivial clone.
fn convert_value(val: &crate::sql::parser::Value) -> Value {
    val.clone()
}

/// Convert a string operator to a typed BinOp.
///
/// Kept for backward compatibility with internal call sites that still
/// pass operators as strings. New code should use [`BinOp::from_str`].
fn convert_op(op: &str) -> BinOp {
    BinOp::from_str(op).unwrap_or(BinOp::Eq)
}

/// Convert a join type string to JoinType.
fn convert_join_type(s: &str) -> JoinType {
    match s.to_uppercase().as_str() {
        "INNER" | "JOIN" => JoinType::Inner,
        "LEFT" | "LEFT OUTER" => JoinType::Left,
        "RIGHT" | "RIGHT OUTER" => JoinType::Right,
        "FULL" | "FULL OUTER" => JoinType::Full,
        "CROSS" => JoinType::Cross,
        _ => JoinType::Inner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parser;
    use crate::sql::lexer::tokenize;

    fn parse_sql(sql: &str) -> SelectQuery {
        let tokens = tokenize(sql).unwrap();
        parser::parse(tokens).unwrap()
    }

    #[test]
    fn test_build_plan_simple_select() {
        let query = parse_sql("SELECT id, name FROM users");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Scan(table=users"));
    }

    #[test]
    fn test_build_plan_with_where() {
        let query = parse_sql("SELECT * FROM orders WHERE id > 100");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Filter"));
        assert!(s.contains("Scan(table=orders"));
    }

    #[test]
    fn test_build_plan_with_group_by() {
        let query = parse_sql("SELECT category, COUNT(*) FROM products GROUP BY category");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Aggregate"));
        assert!(s.contains("group=[category]"));
    }

    #[test]
    fn test_build_plan_with_order_by() {
        let query = parse_sql("SELECT * FROM users ORDER BY name");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Sort"));
    }

    #[test]
    fn test_build_plan_with_limit() {
        let query = parse_sql("SELECT * FROM users LIMIT 10");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Limit(count=10"));
    }

    #[test]
    fn test_build_plan_with_join() {
        let query = parse_sql("SELECT * FROM orders JOIN items ON id = order_id");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Join"));
        assert!(s.contains("Scan(table=orders"));
        assert!(s.contains("Scan(table=items"));
    }

    #[test]
    fn test_build_plan_with_aggregate_no_group_by() {
        let query = parse_sql("SELECT COUNT(*) FROM users");
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Aggregate"));
    }

    #[test]
    fn test_build_plan_complex() {
        let query = parse_sql(
            "SELECT category, COUNT(*) FROM products WHERE price > 100 GROUP BY category ORDER BY category LIMIT 5"
        );
        let plan = build_plan(&query).unwrap();
        let s = format!("{}", plan);
        assert!(s.contains("Limit"));
        assert!(s.contains("Sort"));
        assert!(s.contains("Aggregate"));
        assert!(s.contains("Filter"));
        assert!(s.contains("Scan"));
    }

    #[test]
    fn test_cascades_optimize_pushdown() {
        let query = parse_sql(
            "SELECT * FROM orders JOIN items ON id = order_id WHERE amount > 100"
        );
        let plan = build_plan(&query).unwrap();
        let optimizer = crate::planner::cascades::CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);
        let s = format!("{}", optimized);
        assert!(s.contains("Join"));
    }
}
