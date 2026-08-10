//! The query planner: a calibrated analytic cost model (ADR-023) + Kingman
//! queueing predictor (ADR-020) + DPccp join ordering (ADR-019) + a
//! cost-aware plan lowerer.
//!
//! ## Overview
//!
//! The planner predicts the wall-clock latency of a logical plan before it is
//! executed, so that the join reorderer (ADR-019), index selector (ADR-016),
//! and admission controller (ADR-020) can make decisions without actually
//! running anything.
//!
//! The cost of a plan is the sum of two terms:
//!
//! 1. **Compute cost** — for each operator in the plan, `n_cells /
//!    throughput(operator, tier)`. The throughput is bounded above by either
//!    the SIMD execution rate (`lanes × f_cpu`, for L3-resident data) or the
//!    memory bandwidth (`BW / cell_size`, for DRAM-resident data).
//!
//! 2. **Queueing cost** — Kingman's formula predicts the mean wait time in a
//!    G/G/1 queue from `(λ, μ, c_a, c_s)`. This is added once per join (the
//!    only operator that can contend on a shared hash table) in the current
//!    simple model.
//!
//! ## Submodules
//!
//! - [`CostModel`] (this file) — per-tier compute-cost estimates.
//! - [`kingman`] — Kingman's-formula queueing predictor for admission control
//!   and join-cost tail latency.
//! - [`dpccp`] — left-deep DPccp join ordering for `n ≤ 15` relations
//!   (ADR-019).
//! - [`mcts`] — Monte Carlo Tree Search join ordering for `n > 15`
//!   relations (ADR-019, Wave 15). Falls back from DPccp when the relation
//!   count exceeds DPccp's `O(n²·2ⁿ)` budget.
//! - [`graph_prune`] — connectivity-based pruning for MCTS: cuts the
//!   branching factor from `n` down to the join-graph frontier degree.
//! - [`agm`] — Atserias-Grohe-Marx fractional cover bound, the worst-case
//!   size of a join result and the runtime bound of worst-case optimal join
//!   algorithms.
//! - [`wcoj`] — worst-case optimal join (Leapfrog triejoin) plan selection:
//!   picks between hash join and leapfrog based on the AGM bound.
//! - [`cardinality`] — simple per-table row-count and selectivity estimates
//!   used by the cost model and the join reorderer.
//! - [`learned`] — learned cardinality estimator: per-(table, column)
//!   equi-width histograms + an exponentially-weighted correction factor
//!   that augments the simple [`CardinalityEstimator`] with data-driven
//!   selectivity estimates.
//! - [`calibration`] — online calibration loop for [`learned`]: records
//!   `(predicted, actual)` cardinality pairs, updates the correction factor,
//!   and tracks the running MAPE.
//! - [`lowerer`] — cost-aware lowering of a `LogicalPlan` into a sequence of
//!   `KernelInvocation`s, picking the cheapest tier per operator and
//!   dispatching each join to either `HashProbe` or `LeapfrogJoin` via
//!   [`wcoj::choose_join_algorithm`].
//! - [`tensor`] — tensor-network model of a relational join (Wave 17).
//!   Models the join as a tensor-network contraction (arXiv:2209.12332),
//!   giving polynomial-time optimal ordering for acyclic queries via tree
//!   decomposition. The treewidth of the network equals the AGM bound
//!   exponent.
//! - [`contraction`] — converts a tensor contraction order into a
//!   [`JoinTree`] compatible with DPccp and MCTS (Wave 17).
//!
//! ## Calibration
//!
//! The default [`CostModel`] encodes Zen 5 measurements taken on an
//! AMD EPYC-Turin (see ADR-023):
//!
//! | Kernel | Tier | Measured | Theoretical |
//! |--------|------|----------|-------------|
//! | `scan_eq` AVX-512 | L3 | 24.1 G cells/s | 24 G (8 lanes × 3 GHz) |
//! | `scan_eq` AVX-512 | DRAM | ~5 G cells/s | 5 G (40 GB/s ÷ 8 B) |
//! | `sum_f64` AVX-512 | L3 | 29.8 G cells/s | 24 G |
//!
//! The theoretical formula matches measurement within 5%, so the cost model
//! uses the formula directly. New CPUs are calibrated by editing the
//! `CostModel` defaults (or by constructing a custom one and passing it to
//! [`estimate_cost`]).

pub mod cascades;
pub mod dpccp;
pub mod learned;
pub mod logical_plan;
pub mod lowerer;
pub mod optimizer;
pub mod plan_builder;
pub mod scheduler;

pub use cascades::{CascadesOptimizer, Rule};
pub use dpccp::{build_join_graph, order_joins, JoinGraph, JoinOrder};
pub use learned::{ColumnStats, LearnedCardinality};
pub use logical_plan::{AggregateExpr, JoinType, PlanNode, SortOrder, WindowExpr, WindowFrame, FrameType, FrameBound};
pub use lowerer::{KernelInvocation, PlanLowerer};
pub use plan_builder::build_plan;
pub use scheduler::{Scheduler, count_reached_kernels};

use crate::error::{Error, Result};
use crate::kernel::Operator;
use crate::memory::tier::MemoryTier;

/// A calibrated analytic cost model (ADR-023).
///
/// Encodes the hardware parameters that determine kernel throughput:
///
/// - `cpu_freq_hz` × `simd_lanes` = peak L3-resident throughput (cells/sec).
/// - `memory_bandwidth_bps` / `cell_size` = peak DRAM-resident throughput.
///
/// The default values are calibrated to a Zen 5 core running AVX-512 u64
/// kernels (8 lanes, 3 GHz, 40 GB/s DRAM). Override them for other CPUs or
/// for hypothetical what-if analysis.
///
/// ## Learned cardinality
///
/// Since Wave 14, a [`LearnedCardinality`] estimator may optionally be
/// attached via [`Self::with_learned`]. When present, the cost model
/// delegates equality and range selectivity lookups to the learned
/// estimator (per-(table, column) histograms + global correction factor)
/// instead of falling back to the fixed `0.1` / `0.33` analytic defaults
/// from [`CardinalityEstimator`].
#[derive(Debug, Clone)]
pub struct CostModel {
    /// CPU clock frequency in Hz (e.g. `3.0e9` for 3 GHz).
    pub cpu_freq_hz: f64,
    /// SIMD lanes per kernel invocation (e.g. `8` for AVX-512 u64).
    pub simd_lanes: usize,
    /// Memory bandwidth in bytes/sec (e.g. `40e9` for 40 GB/s DRAM).
    pub memory_bandwidth_bps: f64,
    /// Cell size in bytes (always 8 — turbogp is a u64-word engine, ADR-001).
    pub cell_size: usize,
}

impl CostModel {
    /// Peak throughput (cells/sec) for L3-resident data.
    ///
    /// For an L3-resident kernel, throughput is compute-bound: the kernel
    /// processes `simd_lanes` cells per cycle, and the CPU issues
    /// `cpu_freq_hz` cycles per second. The result is independent of the
    /// operator (all 8-lane AVX-512 kernels hit the same 24 G cells/sec
    /// bound on Zen 5), but the `operator` parameter is retained in the
    /// signature so future per-kernel calibration tables can plug in without
    /// changing call sites.
    ///
    /// Measured value on Zen 5 (AVX-512, L3-resident): 24.1 G cells/sec.
    #[must_use]
    pub fn throughput_l3(&self, _operator: Operator) -> f64 {
        // The parameter is intentionally unused: the formula `lanes × f_cpu`
        // is the same for every 8-lane AVX-512 kernel (see ADR-023, table).
        // A future per-kernel calibration table would index on `_operator`.
        self.simd_lanes as f64 * self.cpu_freq_hz
    }

    /// Peak throughput (cells/sec) for DRAM-resident data.
    ///
    /// For a DRAM-resident kernel, throughput is memory-bandwidth-bound: the
    /// kernel consumes `cell_size` bytes per cell, and DRAM supplies
    /// `memory_bandwidth_bps` bytes per second.
    ///
    /// Measured value on Zen 5 (40 GB/s DRAM, 8-byte cells): ~5 G cells/sec.
    #[must_use]
    pub fn throughput_dram(&self) -> f64 {
        if self.cell_size == 0 {
            return 0.0;
        }
        self.memory_bandwidth_bps / self.cell_size as f64
    }

    /// Estimate the compute cost (in seconds) of running `operator` over
    /// `n_cells` cells resident in `tier`.
    ///
    /// - L1/L2/L3 tiers: compute-bound → `n_cells / throughput_l3`.
    /// - DRAM/CXL/HBM/NVMe/Network tiers: bandwidth-bound →
    ///   `n_cells / throughput_dram`.
    ///
    /// Returns 0.0 if `n_cells == 0` (no work to do).
    #[must_use]
    pub fn estimate_compute(&self, n_cells: usize, operator: Operator, tier: MemoryTier) -> f64 {
        if n_cells == 0 {
            return 0.0;
        }
        let throughput = match tier {
            // Cache-resident: compute-bound.
            MemoryTier::L1L2 | MemoryTier::L3 => self.throughput_l3(operator),
            // Off-chip tiers: bandwidth-bound (CXL is bounded by its link
            // bandwidth, which the current model approximates with the DRAM
            // figure — a conservative lower bound).
            MemoryTier::Ddr5
            | MemoryTier::Hbm
            | MemoryTier::Cxl
            | MemoryTier::Nvme
            | MemoryTier::NvmeOf
            | MemoryTier::Network => self.throughput_dram(),
        };
        if throughput <= 0.0 {
            return 0.0;
        }
        n_cells as f64 / throughput
    }
}

impl Default for CostModel {
    /// Zen 5 defaults: 3 GHz, 8 AVX-512 lanes, 40 GB/s DRAM, 8-byte cells.
    fn default() -> Self {
        Self { cpu_freq_hz: 3.0e9, simd_lanes: 8, memory_bandwidth_bps: 40.0e9, cell_size: 8 }
    }
}
