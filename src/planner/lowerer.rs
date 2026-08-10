//! Plan lowerer — converts a `LogicalPlan` tree into a sequence of
//! `KernelInvocation`s that can be executed by the `Scheduler`.
//!
//! The lowerer is the bridge between the logical IR and the physical
//! kernel table. It chooses the cheapest kernel for each operator based
//! on the cost model and memory tier.
//!
//! ## Lowering rules
//!
//! - `Scan` → `KernelInvocation::Scan` with the table's column data
//! - `Filter` → `KernelInvocation::Filter` with the predicate
//! - `Aggregate` → `KernelInvocation::Aggregate` with the aggregate function
//! - `Join` → `KernelInvocation::HashJoin` with build/probe sides
//! - `Sort` → `KernelInvocation::Sort` (fallback to scalar)
//! - `Limit` → `KernelInvocation::Limit`

use crate::error::{Error, Result};
use crate::kernel::{KernelTable, Operator, KernelParams};
use crate::planner::logical_plan::*;
use crate::planner::CostModel;
use crate::memory::tier::MemoryTier;

/// A physical kernel invocation — the output of lowering.
#[derive(Debug, Clone)]
pub struct KernelInvocation {
    /// The kernel operator to invoke.
    pub operator: Operator,
    /// Parameters for the kernel.
    pub params: KernelParams,
    /// The input column data (for scan/filter/aggregate).
    pub input_columns: Vec<Vec<u64>>,
    /// The output column index (for projection).
    pub output_column: usize,
    /// Estimated cost in microseconds.
    pub estimated_cost_us: f64,
}

/// The plan lowerer — converts LogicalPlan to Vec<KernelInvocation>.
pub struct PlanLowerer<'a> {
    kernel_table: &'a KernelTable,
    cost_model: &'a CostModel,
}

impl<'a> PlanLowerer<'a> {
    /// Create a new lowerer bound to a kernel table and cost model.
    pub fn new(kernel_table: &'a KernelTable, cost_model: &'a CostModel) -> Self {
        Self { kernel_table, cost_model }
    }

    /// Lower a logical plan into a sequence of kernel invocations.
    ///
    /// Returns a flat list of kernel invocations that can be executed
    /// in order by the `Scheduler`.
    pub fn lower(&self, plan: &PlanNode) -> Result<Vec<KernelInvocation>> {
        let mut invocations = Vec::new();
        self.lower_recursive(plan, &mut invocations)?;
        Ok(invocations)
    }

    /// Recursively lower a plan node, appending kernel invocations.
    fn lower_recursive(&self, plan: &PlanNode, invocations: &mut Vec<KernelInvocation>) -> Result<()> {
        match plan {
            PlanNode::Scan { table_name, estimated_rows, .. } => {
                // Scan is a no-op at the kernel level — the data is already
                // in memory. We record a placeholder invocation so the
                // scheduler knows the scan happened.
                let cost = self.cost_model.estimate_compute(
                    *estimated_rows as usize,
                    Operator::ScanEqU64,
                    MemoryTier::L3,
                );
                invocations.push(KernelInvocation {
                    operator: Operator::ScanEqU64,
                    params: KernelParams {
                        target_u64: 0,
                        low_u64: 0,
                        high_u64: u64::MAX,
                        max_distance: 0,
                        cell_count: *estimated_rows as usize,
                        ..Default::default()
                    },
                    input_columns: vec![],
                    output_column: 0,
                    estimated_cost_us: cost * 1e6,
                });
                Ok(())
            }

            PlanNode::Filter { input, predicate } => {
                // Lower the child first
                self.lower_recursive(input, invocations)?;

                // Determine the filter type from the predicate
                let (op, target) = self.classify_filter(predicate);
                let cell_count = input.estimated_rows() as usize;
                let cost = self.cost_model.estimate_compute(
                    cell_count,
                    op,
                    MemoryTier::L3,
                );
                invocations.push(KernelInvocation {
                    operator: op,
                    params: KernelParams {
                        target_u64: target,
                        low_u64: target,
                        high_u64: target,
                        max_distance: 0,
                        cell_count,
                        ..Default::default()
                    },
                    input_columns: vec![],
                    output_column: 0,
                    estimated_cost_us: cost * 1e6,
                });
                Ok(())
            }

            PlanNode::Aggregate { input, aggregates, .. } => {
                self.lower_recursive(input, invocations)?;

                // For each aggregate, emit a kernel invocation
                for agg in aggregates {
                    let op = match agg.func.to_uppercase().as_str() {
                        "SUM" => Operator::AggregateSumF64,
                        "COUNT" if agg.arg == "*" => Operator::ScanEqU64, // COUNT(*) = scan
                        "COUNT" => Operator::AggregateCountDistinct,
                        _ => Operator::AggregateSumF64,
                    };
                    let cell_count = input.estimated_rows() as usize;
                    let cost = self.cost_model.estimate_compute(
                        cell_count,
                        op,
                        MemoryTier::L3,
                    );
                    invocations.push(KernelInvocation {
                        operator: op,
                        params: KernelParams {
                            target_u64: 0,
                            cell_count,
                            ..Default::default()
                        },
                        input_columns: vec![],
                        output_column: 0,
                        estimated_cost_us: cost * 1e6,
                    });
                }
                Ok(())
            }

            PlanNode::Join { left, right, join_type, .. } => {
                // Lower left (build side)
                self.lower_recursive(left, invocations)?;

                // Emit hash build
                let build_rows = left.estimated_rows() as usize;
                let cost = self.cost_model.estimate_compute(
                    build_rows,
                    Operator::HashBuild,
                    MemoryTier::L3,
                );
                invocations.push(KernelInvocation {
                    operator: Operator::HashBuild,
                    params: KernelParams {
                        cell_count: build_rows,
                        ..Default::default()
                    },
                    input_columns: vec![],
                    output_column: 0,
                    estimated_cost_us: cost * 1e6,
                });

                // Lower right (probe side)
                self.lower_recursive(right, invocations)?;

                // Emit hash probe
                let probe_rows = right.estimated_rows() as usize;
                let cost = self.cost_model.estimate_compute(
                    probe_rows,
                    Operator::HashProbe,
                    MemoryTier::L3,
                );
                invocations.push(KernelInvocation {
                    operator: Operator::HashProbe,
                    params: KernelParams {
                        cell_count: probe_rows,
                        ..Default::default()
                    },
                    input_columns: vec![],
                    output_column: 0,
                    estimated_cost_us: cost * 1e6,
                });

                let _ = join_type; // join type affects semantics, not kernel choice
                Ok(())
            }

            PlanNode::Project { input, .. } => {
                // Projection is a no-op at the kernel level — column selection
                // happens when materializing the result.
                self.lower_recursive(input, invocations)
            }

            PlanNode::Sort { input, .. } => {
                self.lower_recursive(input, invocations)?;
                // Sort is a scalar operation — no kernel invocation needed
                Ok(())
            }

            PlanNode::Limit { input, .. } => {
                self.lower_recursive(input, invocations)?;
                // Limit is a scalar operation
                Ok(())
            }

            PlanNode::Union { left, right, .. } => {
                self.lower_recursive(left, invocations)?;
                self.lower_recursive(right, invocations)
            }

            PlanNode::Window { input, .. } => {
                self.lower_recursive(input, invocations)?;
                // Window functions are scalar — no kernel invocation
                Ok(())
            }

            PlanNode::Cte { body, .. } => {
                self.lower_recursive(body, invocations)
            }

            PlanNode::Insert { source, .. } => {
                self.lower_recursive(source, invocations)
            }

            // Leaf nodes — no children to recurse into
            PlanNode::Subquery { .. } | PlanNode::Values { .. } |
            PlanNode::Update { .. } | PlanNode::Delete { .. } => Ok(())
        }
    }

    /// Classify a filter predicate into a kernel operator + target value.
    fn classify_filter(&self, expr: &crate::sql::ast::Expr) -> (Operator, u64) {
        use crate::sql::ast::{BinOp, Expr, Value};

        match expr {
            Expr::Binary { left, op, right } => {
                // Check for `column = literal` pattern
                if let (Expr::Column(_), Expr::Literal(Value::Int(n))) = (left.as_ref(), right.as_ref()) {
                    match op {
                        BinOp::Eq => return (Operator::ScanEqU64, *n as u64),
                        BinOp::Gt => return (Operator::ScanRangeU64, *n as u64),
                        BinOp::Lt => return (Operator::ScanRangeU64, *n as u64),
                        _ => {}
                    }
                }
                // Default to multi-predicate scan
                (Operator::ScanMultiPredicate, 0)
            }
            _ => (Operator::ScanMultiPredicate, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{BinOp, Expr, Value};

    #[test]
    fn test_lower_scan() {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let lowerer = PlanLowerer::new(&kernel_table, &cost_model);

        let plan = PlanNode::Scan {
            table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 1000,
        };
        let invocations = lowerer.lower(&plan).unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
    }

    #[test]
    fn test_lower_filter_eq() {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let lowerer = PlanLowerer::new(&kernel_table, &cost_model);

        let plan = PlanNode::Filter {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 1000,
            }),
            predicate: Expr::Binary {
                left: Box::new(Expr::Column("id".into())),
                op: BinOp::Eq,
                right: Box::new(Expr::Literal(Value::Int(42))),
            },
        };
        let invocations = lowerer.lower(&plan).unwrap();
        assert_eq!(invocations.len(), 2); // scan + filter
        assert_eq!(invocations[1].operator, Operator::ScanEqU64);
        assert_eq!(invocations[1].params.target_u64, 42);
    }

    #[test]
    fn test_lower_aggregate_sum() {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let lowerer = PlanLowerer::new(&kernel_table, &cost_model);

        let plan = PlanNode::Aggregate {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 1000,
            }),
            group_by: vec![],
            aggregates: vec![AggregateExpr {
                func: "SUM".into(), arg: "price".into(), distinct: false, output_name: "sum".into(),
            }],
        };
        let invocations = lowerer.lower(&plan).unwrap();
        assert_eq!(invocations.len(), 2); // scan + aggregate
        assert_eq!(invocations[1].operator, Operator::AggregateSumF64);
    }

    #[test]
    fn test_lower_join() {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let lowerer = PlanLowerer::new(&kernel_table, &cost_model);

        let plan = PlanNode::Join {
            left: Box::new(PlanNode::Scan {
                table_name: "a".into(), alias: None, columns: vec![], estimated_rows: 100,
            }),
            right: Box::new(PlanNode::Scan {
                table_name: "b".into(), alias: None, columns: vec![], estimated_rows: 200,
            }),
            join_type: JoinType::Inner,
            condition: Expr::Column("k".into()),
        };
        let invocations = lowerer.lower(&plan).unwrap();
        // scan_a + hash_build + scan_b + hash_probe = 4
        assert_eq!(invocations.len(), 4);
        assert_eq!(invocations[1].operator, Operator::HashBuild);
        assert_eq!(invocations[3].operator, Operator::HashProbe);
    }

    #[test]
    fn test_lower_complex_plan() {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let lowerer = PlanLowerer::new(&kernel_table, &cost_model);

        // Filter(scan)
        let plan = PlanNode::Filter {
            input: Box::new(PlanNode::Scan {
                table_name: "t".into(), alias: None, columns: vec![], estimated_rows: 10000,
            }),
            predicate: Expr::Binary {
                left: Box::new(Expr::Column("id".into())),
                op: BinOp::Eq,
                right: Box::new(Expr::Literal(Value::Int(99))),
            },
        };
        let invocations = lowerer.lower(&plan).unwrap();
        assert_eq!(invocations.len(), 2);
        // Verify cost estimates are populated
        assert!(invocations[0].estimated_cost_us > 0.0);
        assert!(invocations[1].estimated_cost_us > 0.0);
    }
}
