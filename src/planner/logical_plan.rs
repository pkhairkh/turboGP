//! Logical plan tree — the IR between SQL parsing and physical execution.
//!
//! A `LogicalPlan` is a tree of `PlanNode`s produced by `build_plan()`.
//! Each node carries schema and cardinality metadata so the cost model
//! and Cascades optimizer can make decisions without executing.
//!
//! ## Plan tree example
//!
//! ```text
//! Aggregate(group=[category], aggs=[count(*)])
//!   Filter(pred=[value > 100])
//!     Scan(table=[bench], columns=[id, category, value])
//! ```
//!
//! ## Design
//!
//! - 15 variants covering all SQL shapes turboGP supports.
//! - Each node stores `output_schema` (column names) and `estimated_rows`
//!   so the optimizer can compute costs bottom-up.
//! - `Display` prints an indented tree (like DuckDB's EXPLAIN).

use crate::sql::ast;
use std::fmt;

/// A logical plan tree node.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanNode {
    /// Full-table scan: `SELECT ... FROM table`.
    Scan {
        table_name: String,
        alias: Option<String>,
        columns: Vec<String>, // empty = all columns (*)
        estimated_rows: u64,
    },
    /// Filter: `WHERE expr`.
    Filter {
        input: Box<PlanNode>,
        predicate: ast::Expr,
    },
    /// Projection: `SELECT a, b, ...`.
    Project {
        input: Box<PlanNode>,
        exprs: Vec<(ast::Expr, String)>, // (expr, output_name)
    },
    /// Aggregate: `GROUP BY a, b ... aggs`.
    Aggregate {
        input: Box<PlanNode>,
        group_by: Vec<String>,
        aggregates: Vec<AggregateExpr>,
    },
    /// Sort: `ORDER BY a ASC, b DESC`.
    Sort {
        input: Box<PlanNode>,
        order_by: Vec<(String, SortOrder)>,
    },
    /// Limit: `LIMIT n OFFSET m`.
    Limit {
        input: Box<PlanNode>,
        count: u64,
        offset: u64,
    },
    /// Hash join: `t1 JOIN t2 ON t1.k = t2.k`.
    Join {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        join_type: JoinType,
        condition: ast::Expr,
    },
    /// UNION ALL of two subplans.
    Union {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
    },
    /// Subquery as a scalar value.
    Subquery {
        sql: String,
        estimated_rows: u64,
    },
    /// Window function over a partition.
    Window {
        input: Box<PlanNode>,
        functions: Vec<WindowExpr>,
    },
    /// Common table expression: `WITH name AS (...)`.
    Cte {
        name: String,
        plan: Box<PlanNode>,
        body: Box<PlanNode>,
    },
    /// Literal values: `VALUES (1,2), (3,4)`.
    Values {
        rows: Vec<Vec<ast::Value>>,
        column_names: Vec<String>,
    },
    /// INSERT INTO table SELECT ... or VALUES ...
    Insert {
        table_name: String,
        columns: Vec<String>,
        source: Box<PlanNode>,
    },
    /// UPDATE table SET col = expr WHERE pred.
    Update {
        table_name: String,
        assignments: Vec<(String, ast::Expr)>,
        predicate: Option<ast::Expr>,
    },
    /// DELETE FROM table WHERE pred.
    Delete {
        table_name: String,
        predicate: Option<ast::Expr>,
    },
}

/// An aggregate expression in a plan node.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    /// Function name: COUNT, SUM, AVG, MIN, MAX.
    pub func: String,
    /// Argument column name (or "*" for COUNT(*)).
    pub arg: String,
    /// Whether DISTINCT is specified.
    pub distinct: bool,
    /// Output column name.
    pub output_name: String,
}

/// A window function expression.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowExpr {
    /// Function name: ROW_NUMBER, RANK, LAG, LEAD, SUM, etc.
    pub func: String,
    /// Argument expression (empty for ROW_NUMBER).
    pub arg: String,
    /// PARTITION BY columns.
    pub partition_by: Vec<String>,
    /// ORDER BY columns within the partition.
    pub order_by: Vec<(String, SortOrder)>,
    /// Optional frame: ROWS BETWEEN N PRECEDING AND M FOLLOWING.
    pub frame: Option<WindowFrame>,
    /// Output column name.
    pub output_name: String,
}

/// Window frame specification.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrame {
    pub frame_type: FrameType,
    pub start: FrameBound,
    pub end: FrameBound,
}

/// Frame type: ROWS or RANGE.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameType {
    Rows,
    Range,
}

/// Frame bound.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
}

/// Join type.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// Sort order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl PlanNode {
    /// Estimate the number of output rows from this plan node.
    /// Used by the cost model and Cascades optimizer.
    pub fn estimated_rows(&self) -> u64 {
        match self {
            PlanNode::Scan { estimated_rows, .. } => *estimated_rows,
            PlanNode::Filter { input, .. } => {
                // Filter with default 10% selectivity
                (input.estimated_rows() as f64 * 0.1) as u64
            }
            PlanNode::Project { input, .. } => input.estimated_rows(),
            PlanNode::Aggregate { input, group_by, .. } => {
                if group_by.is_empty() {
                    1 // scalar aggregate
                } else {
                    // Estimate distinct groups as sqrt(n) — a rough heuristic
                    let n = input.estimated_rows();
                    (n as f64).sqrt() as u64
                }
            }
            PlanNode::Sort { input, .. } => input.estimated_rows(),
            PlanNode::Limit { input, count, .. } => {
                input.estimated_rows().min(*count)
            }
            PlanNode::Join { left, right, .. } => {
                // Hash join: output ≈ min(left, right) * selectivity
                let l = left.estimated_rows();
                let r = right.estimated_rows();
                (l.min(r) as f64 * 0.1) as u64
            }
            PlanNode::Union { left, right } => {
                left.estimated_rows() + right.estimated_rows()
            }
            PlanNode::Subquery { estimated_rows, .. } => *estimated_rows,
            PlanNode::Window { input, .. } => input.estimated_rows(),
            PlanNode::Cte { body, .. } => body.estimated_rows(),
            PlanNode::Values { rows, .. } => rows.len() as u64,
            PlanNode::Insert { source, .. } => source.estimated_rows(),
            PlanNode::Update { table_name: _, predicate, .. } => {
                // Without table stats, assume 10% of rows match
                if predicate.is_some() { 10 } else { 100 }
            }
            PlanNode::Delete { predicate, .. } => {
                if predicate.is_some() { 10 } else { 100 }
            }
        }
    }

    /// Get the output schema (column names) of this plan node.
    pub fn output_schema(&self) -> Vec<String> {
        match self {
            PlanNode::Scan { columns, alias, table_name, .. } => {
                if columns.is_empty() {
                    vec![format!("{}.*", alias.as_ref().unwrap_or(table_name))]
                } else {
                    columns.clone()
                }
            }
            PlanNode::Filter { input, .. } => input.output_schema(),
            PlanNode::Project { exprs, .. } => {
                exprs.iter().map(|(_, name)| name.clone()).collect()
            }
            PlanNode::Aggregate { group_by, aggregates, .. } => {
                let mut schema = group_by.clone();
                schema.extend(aggregates.iter().map(|a| a.output_name.clone()));
                schema
            }
            PlanNode::Sort { input, .. } => input.output_schema(),
            PlanNode::Limit { input, .. } => input.output_schema(),
            PlanNode::Join { left, right, .. } => {
                let mut s = left.output_schema();
                s.extend(right.output_schema());
                s
            }
            PlanNode::Union { left, .. } => left.output_schema(),
            PlanNode::Subquery { .. } => vec!["subquery".to_string()],
            PlanNode::Window { input, functions } => {
                let mut s = input.output_schema();
                s.extend(functions.iter().map(|f| f.output_name.clone()));
                s
            }
            PlanNode::Cte { body, .. } => body.output_schema(),
            PlanNode::Values { column_names, .. } => column_names.clone(),
            PlanNode::Insert { .. } => vec![],
            PlanNode::Update { .. } => vec![],
            PlanNode::Delete { .. } => vec![],
        }
    }
}

impl fmt::Display for PlanNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indent(f, 0)
    }
}

impl PlanNode {
    fn fmt_indent(&self, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        let pad = "  ".repeat(indent);
        match self {
            PlanNode::Scan { table_name, alias, columns, estimated_rows } => {
                let cols = if columns.is_empty() { "*".to_string() } else { columns.join(", ") };
                let a = alias.as_ref().map(|a| format!(" AS {}", a)).unwrap_or_default();
                writeln!(f, "{}Scan(table={}{} cols=[{}] ~{}rows)", pad, table_name, a, cols, estimated_rows)
            }
            PlanNode::Filter { input, predicate } => {
                writeln!(f, "{}Filter(pred=[{}])", pad, predicate)?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Project { input, exprs } => {
                let names: Vec<&str> = exprs.iter().map(|(_, n)| n.as_str()).collect();
                writeln!(f, "{}Project(out=[{}])", pad, names.join(", "))?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Aggregate { input, group_by, aggregates } => {
                let aggs: Vec<String> = aggregates.iter()
                    .map(|a| format!("{}({}{})", a.func,
                        if a.distinct { "DISTINCT " } else { "" }, a.arg))
                    .collect();
                writeln!(f, "{}Aggregate(group=[{}] aggs=[{}])", pad, group_by.join(", "), aggs.join(", "))?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Sort { input, order_by } => {
                let orders: Vec<String> = order_by.iter()
                    .map(|(c, o)| format!("{} {:?}", c, o)).collect();
                writeln!(f, "{}Sort(order=[{}])", pad, orders.join(", "))?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Limit { input, count, offset } => {
                writeln!(f, "{}Limit(count={} offset={})", pad, count, offset)?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Join { left, right, join_type, condition } => {
                writeln!(f, "{}Join(type={:?} on=[{}])", pad, join_type, condition)?;
                left.fmt_indent(f, indent + 1)?;
                right.fmt_indent(f, indent + 1)
            }
            PlanNode::Union { left, right } => {
                writeln!(f, "{}Union", pad)?;
                left.fmt_indent(f, indent + 1)?;
                right.fmt_indent(f, indent + 1)
            }
            PlanNode::Subquery { sql, estimated_rows } => {
                writeln!(f, "{}Subquery(~{}rows sql='{}')", pad, estimated_rows, sql)
            }
            PlanNode::Window { input, functions } => {
                let fns: Vec<String> = functions.iter()
                    .map(|w| format!("{}({}) OVER (...)", w.func, w.arg)).collect();
                writeln!(f, "{}Window(fns=[{}])", pad, fns.join(", "))?;
                input.fmt_indent(f, indent + 1)
            }
            PlanNode::Cte { name, plan, body } => {
                writeln!(f, "{}Cte(name={})", pad, name)?;
                plan.fmt_indent(f, indent + 1)?;
                body.fmt_indent(f, indent + 1)
            }
            PlanNode::Values { rows, column_names } => {
                writeln!(f, "{}Values({} rows, cols=[{}])", pad, rows.len(), column_names.join(", "))
            }
            PlanNode::Insert { table_name, columns, source } => {
                writeln!(f, "{}Insert(table={} cols=[{}])", pad, table_name, columns.join(", "))?;
                source.fmt_indent(f, indent + 1)
            }
            PlanNode::Update { table_name, assignments, predicate } => {
                let sets: Vec<String> = assignments.iter()
                    .map(|(c, _)| c.clone()).collect();
                writeln!(f, "{}Update(table={} set=[{}] where={:?})", pad, table_name, sets.join(", "), predicate.is_some())
            }
            PlanNode::Delete { table_name, predicate } => {
                writeln!(f, "{}Delete(table={} where={:?})", pad, table_name, predicate.is_some())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_node_display_scan() {
        let plan = PlanNode::Scan {
            table_name: "users".to_string(),
            alias: None,
            columns: vec![],
            estimated_rows: 1000,
        };
        let s = format!("{}", plan);
        assert!(s.contains("Scan(table=users"));
    }

    #[test]
    fn test_plan_node_display_filter() {
        let scan = PlanNode::Scan {
            table_name: "orders".to_string(),
            alias: None,
            columns: vec![],
            estimated_rows: 5000,
        };
        let plan = PlanNode::Filter {
            input: Box::new(scan),
            predicate: ast::Expr::Column("amount".to_string()),
        };
        let s = format!("{}", plan);
        assert!(s.contains("Filter"));
        assert!(s.contains("Scan"));
    }

    #[test]
    fn test_estimated_rows_aggregate() {
        let scan = PlanNode::Scan {
            table_name: "t".to_string(),
            alias: None,
            columns: vec![],
            estimated_rows: 10000,
        };
        let agg = PlanNode::Aggregate {
            input: Box::new(scan),
            group_by: vec!["category".to_string()],
            aggregates: vec![],
        };
        // sqrt(10000) = 100
        assert_eq!(agg.estimated_rows(), 100);
    }

    #[test]
    fn test_estimated_rows_limit() {
        let scan = PlanNode::Scan {
            table_name: "t".to_string(),
            alias: None,
            columns: vec![],
            estimated_rows: 10000,
        };
        let limit = PlanNode::Limit {
            input: Box::new(scan),
            count: 10,
            offset: 0,
        };
        assert_eq!(limit.estimated_rows(), 10);
    }
}
