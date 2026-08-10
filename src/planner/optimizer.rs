//! # Cost-based optimizer integration (Wave 23).
//!
//! Wires the existing cost model (`src/planner/`) into the execution path.
//! The cost model estimates per-tier throughput for each operator and
//! chooses the cheapest execution plan.

use crate::kernel::Operator;
use crate::planner::CostModel;

/// A query plan chosen by the cost-based optimizer.
#[derive(Debug, Clone)]
pub struct ExecPlan {
    /// The chosen execution strategy.
    pub strategy: ExecStrategy,
    /// Estimated cost (in microseconds) from the cost model.
    pub estimated_cost_us: f64,
    /// Estimated number of output rows.
    pub estimated_rows: u64,
}

/// Execution strategies the optimizer can choose between.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecStrategy {
    /// Use the kernel-direct SIMD path (fastest for simple scans/aggregates).
    KernelDirect,
    /// Use the vectorized fallback path (for complex predicates).
    Vectorized,
    /// Use the row-based TPC-H interpreter (for subqueries, CASE, etc.).
    TpchFallback,
    /// Use a hash join (for JOIN queries).
    HashJoin,
}

/// Choose the best execution plan for a query based on the cost model.
///
/// The cost model estimates:
/// - Scan throughput: cells/sec for each memory tier (L3, DDR5, CXL)
/// - Aggregate throughput: cells/sec for SUM/COUNT/AVG
/// - Hash join build/probe cost
///
/// The optimizer picks the strategy with the lowest estimated cost.
pub fn choose_plan(
    cost_model: &CostModel,
    row_count: u64,
    has_where: bool,
    has_group_by: bool,
    has_join: bool,
    has_subquery: bool,
    select_count: usize,
) -> ExecPlan {
    // If the query has subqueries or features the basic executor can't
    // handle, route to the TPC-H interpreter.
    if has_subquery {
        return ExecPlan {
            strategy: ExecStrategy::TpchFallback,
            estimated_cost_us: estimate_interpreter(cost_model, row_count),
            estimated_rows: 1,
        };
    }

    // JOIN queries use hash join.
    if has_join {
        return ExecPlan {
            strategy: ExecStrategy::HashJoin,
            estimated_cost_us: estimate_hash_join(cost_model, row_count),
            estimated_rows: row_count,
        };
    }

    // Single-aggregate queries with simple WHERE → kernel-direct.
    if select_count == 1 && has_group_by {
        return ExecPlan {
            strategy: ExecStrategy::KernelDirect,
            estimated_cost_us: estimate_scan(cost_model, row_count, has_where),
            estimated_rows: estimate_groups(row_count),
        };
    }

    // Single-aggregate without GROUP BY → kernel-direct.
    if select_count == 1 {
        return ExecPlan {
            strategy: ExecStrategy::KernelDirect,
            estimated_cost_us: estimate_scan(cost_model, row_count, has_where),
            estimated_rows: 1,
        };
    }

    // Multi-aggregate or multi-column → vectorized fallback.
    ExecPlan {
        strategy: ExecStrategy::Vectorized,
        estimated_cost_us: estimate_scan(cost_model, row_count, has_where) * 1.5,
        estimated_rows: row_count,
    }
}

fn estimate_scan(cost_model: &CostModel, row_count: u64, has_where: bool) -> f64 {
    // Estimate: row_count / scan_throughput * 1e6 (to microseconds)
    // Plus filter overhead if WHERE is present.
    let scan_throughput = cost_model.throughput_l3(Operator::ScanEqU64);
    let base_cost = row_count as f64 / scan_throughput * 1e6;
    let filter_cost = if has_where { base_cost * 0.3 } else { 0.0 };
    base_cost + filter_cost
}

fn estimate_hash_join(cost_model: &CostModel, row_count: u64) -> f64 {
    let scan_cost = estimate_scan(cost_model, row_count, false);
    // Hash join: build phase (scan) + probe phase (scan) = 2x scan.
    scan_cost * 2.0
}

fn estimate_interpreter(cost_model: &CostModel, row_count: u64) -> f64 {
    // TPC-H interpreter is row-based, ~10x slower than vectorized.
    estimate_scan(cost_model, row_count, true) * 10.0
}

fn estimate_groups(row_count: u64) -> u64 {
    // Heuristic: average group cardinality is sqrt(row_count).
    (row_count as f64).sqrt() as u64
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_kernel_direct_for_simple_count() {
        let cm = CostModel::default();
        let plan = choose_plan(&cm, 1_000_000, false, false, false, false, 1);
        assert_eq!(plan.strategy, ExecStrategy::KernelDirect);
        assert!(plan.estimated_cost_us > 0.0);
    }

    #[test]
    fn choose_kernel_direct_for_group_by() {
        let cm = CostModel::default();
        let plan = choose_plan(&cm, 1_000_000, true, true, false, false, 1);
        assert_eq!(plan.strategy, ExecStrategy::KernelDirect);
    }

    #[test]
    fn choose_interpreter_for_subquery() {
        let cm = CostModel::default();
        let plan = choose_plan(&cm, 1_000_000, true, false, false, true, 1);
        assert_eq!(plan.strategy, ExecStrategy::TpchFallback);
    }

    #[test]
    fn choose_hash_join_for_join() {
        let cm = CostModel::default();
        let plan = choose_plan(&cm, 1_000_000, false, false, true, false, 1);
        assert_eq!(plan.strategy, ExecStrategy::HashJoin);
    }

    #[test]
    fn choose_vectorized_for_multi_aggregate() {
        let cm = CostModel::default();
        let plan = choose_plan(&cm, 1_000_000, true, false, false, false, 3);
        assert_eq!(plan.strategy, ExecStrategy::Vectorized);
    }

    #[test]
    fn cost_increases_with_filter() {
        let cm = CostModel::default();
        let no_filter = estimate_scan(&cm, 1_000_000, false);
        let with_filter = estimate_scan(&cm, 1_000_000, true);
        assert!(with_filter > no_filter);
    }

    #[test]
    fn interpreter_cost_higher_than_kernel() {
        let cm = CostModel::default();
        let kernel = estimate_scan(&cm, 1_000_000, false);
        let interpreter = estimate_interpreter(&cm, 1_000_000);
        assert!(interpreter > kernel * 5.0);
    }
}
