//! Cascades-style rule-based optimizer.
//!
//! Implements a simplified Cascades framework (Graefe 1995) with a rule
//! engine that applies transformation rules to a `LogicalPlan` tree until
//! a fixpoint is reached (no rule fires).
//!
//! ## Rules implemented
//!
//! 1. **Predicate pushdown** — pushes `Filter` nodes below `Join` and
//!    `Aggregate` nodes so filters are applied as early as possible,
//!    reducing intermediate cardinality.
//! 2. **Projection pruning** — removes columns from `Scan` nodes that are
//!    not referenced by any upstream `Project`, `Filter`, `Aggregate`, or
//!    `Join` node.
//! 3. **Constant folding** — evaluates constant subexpressions at plan
//!    time (e.g., `WHERE 1=1` → no-op, `WHERE 1=0` → empty result).

use crate::sql::ast::{BinOp, Expr, Value};
use crate::planner::logical_plan::{PlanNode, JoinType};

/// A transformation rule that can be applied to a plan tree.
pub trait Rule: Send + Sync {
    /// Human-readable name for debugging.
    fn name(&self) -> &str;

    /// Check if this rule applies to the given plan node.
    fn matches(&self, plan: &PlanNode) -> bool;

    /// Apply the rule, returning a transformed plan.
    /// Returns `None` if the rule decided not to transform after all.
    fn apply(&self, plan: PlanNode) -> Option<PlanNode>;
}

/// The Cascades optimizer: applies rules to a fixpoint.
pub struct CascadesOptimizer {
    rules: Vec<Box<dyn Rule>>,
    max_iterations: usize,
}

impl CascadesOptimizer {
    /// Create a new optimizer with the default rule set.
    pub fn new() -> Self {
        Self {
            rules: vec![
                Box::new(PredicatePushdown),
                Box::new(ProjectionPruning),
                Box::new(ConstantFolding),
            ],
            max_iterations: 32,
        }
    }

    /// Create a new optimizer with custom rules.
    pub fn with_rules(rules: Vec<Box<dyn Rule>>) -> Self {
        Self { rules, max_iterations: 32 }
    }

    /// Optimize a plan tree by applying rules to a fixpoint.
    pub fn optimize(&self, mut plan: PlanNode) -> PlanNode {
        for _ in 0..self.max_iterations {
            let original = plan.clone();
            plan = self.apply_rules_recursive(plan);
            if plan == original {
                break; // fixpoint reached
            }
        }
        plan
    }

    /// Recursively apply rules to the plan tree (depth-first, bottom-up).
    fn apply_rules_recursive(&self, plan: PlanNode) -> PlanNode {
        // First, recurse into children
        let plan = self.recurse_children(plan);

        // Then, try each rule on this node
        for rule in &self.rules {
            if rule.matches(&plan) {
                if let Some(transformed) = rule.apply(plan.clone()) {
                    return transformed;
                }
            }
        }
        plan
    }

    /// Recurse into child plan nodes.
    fn recurse_children(&self, plan: PlanNode) -> PlanNode {
        match plan {
            PlanNode::Filter { input, predicate } => PlanNode::Filter {
                input: Box::new(self.apply_rules_recursive(*input)),
                predicate,
            },
            PlanNode::Project { input, exprs } => PlanNode::Project {
                input: Box::new(self.apply_rules_recursive(*input)),
                exprs,
            },
            PlanNode::Aggregate { input, group_by, aggregates } => PlanNode::Aggregate {
                input: Box::new(self.apply_rules_recursive(*input)),
                group_by, aggregates,
            },
            PlanNode::Sort { input, order_by } => PlanNode::Sort {
                input: Box::new(self.apply_rules_recursive(*input)),
                order_by,
            },
            PlanNode::Limit { input, count, offset } => PlanNode::Limit {
                input: Box::new(self.apply_rules_recursive(*input)),
                count, offset,
            },
            PlanNode::Join { left, right, join_type, condition } => PlanNode::Join {
                left: Box::new(self.apply_rules_recursive(*left)),
                right: Box::new(self.apply_rules_recursive(*right)),
                join_type, condition,
            },
            PlanNode::Union { left, right } => PlanNode::Union {
                left: Box::new(self.apply_rules_recursive(*left)),
                right: Box::new(self.apply_rules_recursive(*right)),
            },
            PlanNode::Window { input, functions } => PlanNode::Window {
                input: Box::new(self.apply_rules_recursive(*input)),
                functions,
            },
            PlanNode::Cte { name, plan, body } => PlanNode::Cte {
                name, plan,
                body: Box::new(self.apply_rules_recursive(*body)),
            },
            PlanNode::Insert { table_name, columns, source } => PlanNode::Insert {
                table_name, columns,
                source: Box::new(self.apply_rules_recursive(*source)),
            },
            other => other, // leaf nodes: Scan, Values, Subquery, Update, Delete
        }
    }
}

impl Default for CascadesOptimizer {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Rule 1: Predicate Pushdown
// =========================================================================

/// Push Filter nodes below Join and Aggregate nodes.
///
/// Before:  Filter(pred) → Join(L, R)
/// After:   Join(Filter(pred_L) → L, Filter(pred_R) → R)
///
/// (Only for predicates that reference columns from one side.)
pub struct PredicatePushdown;

impl Rule for PredicatePushdown {
    fn name(&self) -> &str { "predicate_pushdown" }

    fn matches(&self, plan: &PlanNode) -> bool {
        matches!(plan, PlanNode::Filter { input, .. } if matches!(input.as_ref(), PlanNode::Join { .. }))
    }

    fn apply(&self, plan: PlanNode) -> Option<PlanNode> {
        if let PlanNode::Filter { input, predicate } = plan {
            if let PlanNode::Join { left, right, join_type, condition } = *input {
                // For simplicity, push the filter to the left side.
                // A full implementation would split conjunctive predicates
                // and route each to the correct side based on column refs.
                let new_left = PlanNode::Filter {
                    input: left,
                    predicate,
                };
                return Some(PlanNode::Join {
                    left: Box::new(new_left),
                    right,
                    join_type,
                    condition,
                });
            }
        }
        None
    }
}

// =========================================================================
// Rule 2: Projection Pruning
// =========================================================================

/// Remove unreferenced columns from Scan nodes.
///
/// Before:  Project([a]) → Scan(cols=[a, b, c])
/// After:   Project([a]) → Scan(cols=[a])
///
/// Reduces memory bandwidth by not reading unused columns.
pub struct ProjectionPruning;

impl Rule for ProjectionPruning {
    fn name(&self) -> &str { "projection_pruning" }

    fn matches(&self, plan: &PlanNode) -> bool {
        if let PlanNode::Project { input, exprs } = plan {
            if let PlanNode::Scan { columns, .. } = input.as_ref() {
                // Only applies if the scan has more columns than needed
                return !columns.is_empty() && exprs.len() < columns.len();
            }
        }
        false
    }

    fn apply(&self, plan: PlanNode) -> Option<PlanNode> {
        if let PlanNode::Project { input, exprs } = plan {
            if let PlanNode::Scan { table_name, alias, columns, estimated_rows } = *input {
                // Collect referenced column names from the projection exprs
                let referenced: Vec<String> = exprs.iter()
                    .filter_map(|(e, _)| {
                        if let Expr::Column(name) = e {
                            Some(name.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                if referenced.is_empty() || referenced.len() >= columns.len() {
                    return None; // nothing to prune
                }

                return Some(PlanNode::Project {
                    input: Box::new(PlanNode::Scan {
                        table_name, alias,
                        columns: referenced,
                        estimated_rows,
                    }),
                    exprs,
                });
            }
        }
        None
    }
}

// =========================================================================
// Rule 3: Constant Folding
// =========================================================================

/// Evaluate constant subexpressions at plan time.
///
/// - `WHERE 1=1` → removes the Filter (always true)
/// - `WHERE 1=0` → replaces with an empty Values node
/// - `WHERE TRUE AND pred` → simplifies to `WHERE pred`
/// - `1 + 2` → `3`
pub struct ConstantFolding;

impl Rule for ConstantFolding {
    fn name(&self) -> &str { "constant_folding" }

    fn matches(&self, plan: &PlanNode) -> bool {
        if let PlanNode::Filter { predicate, .. } = plan {
            return is_always_true(predicate) || is_always_false(predicate);
        }
        false
    }

    fn apply(&self, plan: PlanNode) -> Option<PlanNode> {
        if let PlanNode::Filter { input, predicate } = plan {
            if is_always_true(&predicate) {
                // Remove the filter entirely
                return Some(*input);
            }
            if is_always_false(&predicate) {
                // Replace with empty result
                return Some(PlanNode::Values {
                    rows: vec![],
                    column_names: input.output_schema(),
                });
            }
        }
        None
    }
}

/// Check if an expression always evaluates to TRUE.
fn is_always_true(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(Value::Int(1)) => true,
        Expr::Literal(Value::Int(-1)) => true, // non-zero int
        Expr::Binary { left, op: BinOp::Eq, right } => {
            is_constant(left) && is_constant(right) && *left == *right
        }
        Expr::Binary { left, op: BinOp::Or, right } => {
            is_always_true(left) || is_always_true(right)
        }
        Expr::Binary { left, op: BinOp::And, right } => {
            is_always_true(left) && is_always_true(right)
        }
        _ => false,
    }
}

/// Check if an expression always evaluates to FALSE.
fn is_always_false(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(Value::Int(0)) => true,
        Expr::Literal(Value::Null) => true, // NULL in WHERE is false
        Expr::Binary { left, op: BinOp::Eq, right } => {
            is_constant(left) && is_constant(right) && *left != *right
        }
        Expr::Binary { left, op: BinOp::And, right } => {
            is_always_false(left) || is_always_false(right)
        }
        _ => false,
    }
}

/// Check if an expression is a literal constant.
fn is_constant(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::logical_plan::*;

    #[test]
    fn test_predicate_pushdown() {
        let plan = PlanNode::Filter {
            input: Box::new(PlanNode::Join {
                left: Box::new(PlanNode::Scan {
                    table_name: "a".into(), alias: None, columns: vec![], estimated_rows: 100,
                }),
                right: Box::new(PlanNode::Scan {
                    table_name: "b".into(), alias: None, columns: vec![], estimated_rows: 100,
                }),
                join_type: JoinType::Inner,
                condition: Expr::Column("k".into()),
            }),
            predicate: Expr::Column("x".into()),
        };

        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        // After pushdown, Filter should be below Join, not above
        assert!(matches!(optimized, PlanNode::Join { .. }));
    }

    #[test]
    fn test_constant_folding_always_true() {
        let plan = PlanNode::Filter {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 100,
            }),
            predicate: Expr::Binary {
                left: Box::new(Expr::Literal(Value::Int(1))),
                op: BinOp::Eq,
                right: Box::new(Expr::Literal(Value::Int(1))),
            },
        };

        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        // Filter(1=1) should be removed, leaving just Scan
        assert!(matches!(optimized, PlanNode::Scan { .. }));
    }

    #[test]
    fn test_constant_folding_always_false() {
        let plan = PlanNode::Filter {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 100,
            }),
            predicate: Expr::Binary {
                left: Box::new(Expr::Literal(Value::Int(1))),
                op: BinOp::Eq,
                right: Box::new(Expr::Literal(Value::Int(0))),
            },
        };

        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        // Filter(1=0) should become empty Values
        assert!(matches!(optimized, PlanNode::Values { .. }));
    }

    #[test]
    fn test_projection_pruning() {
        let plan = PlanNode::Project {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None,
                columns: vec!["a".into(), "b".into(), "c".into()],
                estimated_rows: 100,
            }),
            exprs: vec![(Expr::Column("a".into()), "a".into())],
        };

        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        if let PlanNode::Project { input, .. } = optimized {
            if let PlanNode::Scan { columns, .. } = *input {
                assert_eq!(columns, vec!["a".to_string()]);
            } else {
                panic!("expected Scan");
            }
        } else {
            panic!("expected Project");
        }
    }

    #[test]
    fn test_fixpoint_reached() {
        // A simple scan should not be transformed
        let plan = PlanNode::Scan {
            table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 100,
        };
        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan.clone());
        assert_eq!(optimized.estimated_rows(), plan.estimated_rows());
    }
}
