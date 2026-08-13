//! Eddy vs. fixed-pipeline benchmark (Wave 16: Adaptive Execution).
//!
//! This benchmark compares the adaptive eddy routing against the fixed-order
//! pipeline on three workloads:
//!
//! 1. **Uniform data** (`eddy/uniform`): 3 filters with equal selectivity
//!    (0.5 each). The eddy has no information to reorder on (all selectivities
//!    are equal), so it should match the fixed pipeline — both run all 3
//!    operators per morsel. The eddy's routing overhead should be negligible.
//!
//! 2. **Skewed data** (`eddy/skewed`): 3 filters where the *last* in the
//!    fixed pipeline is most selective (selectivity 0.0 — it filters out
//!    every cell). The eddy learns this on the first morsel, then on
//!    subsequent morsels it applies the most-selective filter first → zero
//!    output → early termination → skips the other two filters. The fixed
//!    pipeline always runs all 3. The eddy should be ~2–3× faster.
//!
//! 3. **Adaptive switching** (`eddy/adaptive_switching`): measures the
//!    [`AdaptiveExecutor`]'s divergence-detection overhead when the
//!    cardinality estimate is 10× off (the benchmark scenario from the Wave
//!    16 task description).
//!
//! ## Throughput
//!
//! Throughput is reported in `Elements/sec`, where "elements" is the number
//! of morsels processed (for benchmarks 1 and 2) or the number of `observe`
//! calls (for benchmark 3).
//!
//! ## Methodology
//!
//! For benchmarks 1 and 2, each criterion iteration processes `N_MORSELS =
//! 100` morsels of 1024 cells each. The kernel table is constructed once
//! outside the timed closure. Both the fixed pipeline and the eddy are
//! constructed fresh inside the timed closure (so the eddy's selectivity
//! estimates start from the initial `1.0` each iteration — this measures the
//! "cold start" scenario where the eddy must learn the selectivities from
//! scratch).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use turbogp::executor::plan::{LogicalPlan, PlanNode};
use turbogp::executor::{AdaptiveExecutor, Eddy, Morsel, Pipeline};
use turbogp::kernel::{KernelParams, KernelTable, Operator, PredicateOp};

/// Number of morsels processed per benchmark iteration.
const N_MORSELS: usize = 100;

/// The morsel size (1024 cells, per ADR-007 / ADR-018).
const MORSEL_CELLS: usize = 1024;

/// Build a morsel of 1024 u64 cells with the given generator.
fn make_morsel(gen: impl Fn(usize) -> u64) -> Morsel {
    let cells: Vec<u64> = (0..MORSEL_CELLS).map(gen).collect();
    Morsel::new(0, 0, &cells)
}

/// Uniform data: alternating 0 and 1. Half the cells are 0, half are 1.
/// Each filter (ScanRange(0,0), ScanEq(0), ScanMultiPredicate(Eq(0),count=1))
/// matches exactly the 0s → selectivity 0.5 for all three.
fn uniform_morsel() -> Morsel {
    make_morsel(|i| (i % 2) as u64)
}

/// Three operators with equal selectivity (0.5) on uniform data.
///
/// - `ScanRangeU64(low=0, high=0)`: matches cells == 0 (half the morsel).
/// - `ScanEqU64(target=0)`: matches cells == 0 (half the morsel).
/// - `ScanMultiPredicate(Eq(0), count=1)`: matches cells == 0 (half).
fn uniform_operators() -> Vec<Operator> {
    vec![Operator::ScanRangeU64, Operator::ScanEqU64, Operator::ScanMultiPredicate]
}

/// Params for the uniform workload: all three operators target the value 0.
fn uniform_params() -> KernelParams {
    KernelParams {
        target_u64: 0,
        low_u64: 0,
        high_u64: 0,
        pred1_op: PredicateOp::Eq,
        predicate_count: 1,
        ..Default::default()
    }
}

/// Three operators with skewed selectivity on the uniform (0/1) data.
///
/// - `ScanRangeU64(low=0, high=1)`: matches all cells (selectivity 1.0).
/// - `ScanEqU64(target=0)`: matches half (selectivity 0.5).
/// - `ScanMultiPredicate(Eq(0), Eq(1), count=2)`: matches no cells (a cell
///   can't be both 0 and 1) — selectivity 0.0.
///
/// In the fixed pipeline, `ScanMultiPredicate` is last. The eddy learns on
/// the first morsel that `ScanMultiPredicate` is most selective, then on
/// subsequent morsels applies it first → zero output → early termination →
/// skips the other two.
fn skewed_operators() -> Vec<Operator> {
    vec![Operator::ScanRangeU64, Operator::ScanEqU64, Operator::ScanMultiPredicate]
}

/// Params for the skewed workload.
fn skewed_params() -> KernelParams {
    KernelParams {
        target_u64: 0,
        target2_u64: 1,
        low_u64: 0,
        high_u64: 1,
        pred1_op: PredicateOp::Eq,
        pred2_op: PredicateOp::Eq,
        predicate_count: 2,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Benchmark 1: uniform data — eddy matches fixed pipeline
// ---------------------------------------------------------------------------

fn bench_uniform_eddy_vs_fixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("eddy/uniform");
    group.throughput(Throughput::Elements(N_MORSELS as u64));

    let kt = Arc::new(KernelTable::new());
    let morsel = uniform_morsel();
    let ops = uniform_operators();
    let params = uniform_params();

    // Sanity check: on uniform data, the eddy should apply all 3 operators
    // on every morsel (no early termination, since all produce non-zero
    // output). Verify this once at setup so the benchmark doesn't measure
    // a broken eddy.
    {
        let mut eddy = Eddy::new(ops.clone(), 1.0);
        let mut pipeline = Pipeline::new(ops.clone());
        pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).unwrap();
        assert_eq!(
            pipeline.results().len(),
            3,
            "uniform: eddy should apply all 3 operators (no early termination)"
        );
    }

    group.bench_function("fixed_pipeline", |b| {
        b.iter(|| {
            let mut pipeline = Pipeline::new(ops.clone());
            for _ in 0..N_MORSELS {
                pipeline
                    .execute_morsel(black_box(&morsel), black_box(&kt), black_box(&params))
                    .unwrap();
            }
            black_box(pipeline.results().len());
        });
    });

    group.bench_function("eddy", |b| {
        b.iter(|| {
            let mut eddy = Eddy::new(ops.clone(), 0.1);
            let mut pipeline = Pipeline::new(ops.clone());
            for _ in 0..N_MORSELS {
                pipeline
                    .execute_with_eddy(
                        black_box(&morsel),
                        black_box(&mut eddy),
                        black_box(&kt),
                        black_box(&params),
                    )
                    .unwrap();
            }
            black_box(pipeline.results().len());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: skewed data — eddy ~2-3x faster via early termination
// ---------------------------------------------------------------------------

fn bench_skewed_eddy_vs_fixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("eddy/skewed");
    group.throughput(Throughput::Elements(N_MORSELS as u64));

    let kt = Arc::new(KernelTable::new());
    let morsel = uniform_morsel(); // same data; skew is in the operator selectivities
    let ops = skewed_operators();
    let params = skewed_params();

    // Sanity check: verify the eddy early-terminates on the 2nd+ morsels.
    // (The first morsel runs all 3 to learn selectivities; subsequent
    // morsels run only 1.)
    {
        let mut eddy = Eddy::new(ops.clone(), 1.0);
        let mut pipeline = Pipeline::new(ops.clone());
        // First morsel: 3 results (declaration order, all sels start at 1.0).
        pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).unwrap();
        let first_count = pipeline.results().len();
        assert_eq!(first_count, 3, "first morsel should apply all 3 operators");
        pipeline.reset();
        // Second morsel: 1 result (ScanMultiPredicate first, sel=0.0, count=0,
        // early termination).
        pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).unwrap();
        let second_count = pipeline.results().len();
        assert_eq!(
            second_count, 1,
            "second morsel should early-terminate after 1 operator (ScanMultiPredicate sel=0.0)"
        );
        // Verify the early-terminating operator is indeed ScanMultiPredicate
        // (count = 0 on the 0/1 data with contradictory predicates).
        assert_eq!(pipeline.results()[0].count, 0);
    }

    group.bench_function("fixed_pipeline", |b| {
        b.iter(|| {
            let mut pipeline = Pipeline::new(ops.clone());
            for _ in 0..N_MORSELS {
                pipeline
                    .execute_morsel(black_box(&morsel), black_box(&kt), black_box(&params))
                    .unwrap();
            }
            // Fixed pipeline runs all 3 operators on every morsel.
            assert_eq!(pipeline.results().len(), 3 * N_MORSELS);
            black_box(pipeline.results().len());
        });
    });

    group.bench_function("eddy", |b| {
        b.iter(|| {
            let mut eddy = Eddy::new(ops.clone(), 1.0);
            let mut pipeline = Pipeline::new(ops.clone());
            for _ in 0..N_MORSELS {
                pipeline
                    .execute_with_eddy(
                        black_box(&morsel),
                        black_box(&mut eddy),
                        black_box(&kt),
                        black_box(&params),
                    )
                    .unwrap();
            }
            // Eddy: first morsel runs 3, subsequent morsels run 1 each.
            // Total = 3 + 1 * (N_MORSELS - 1) = N_MORSELS + 2.
            assert_eq!(pipeline.results().len(), N_MORSELS + 2);
            black_box(pipeline.results().len());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: adaptive switching — divergence detection overhead
// ---------------------------------------------------------------------------

/// A trivial LogicalPlan for the AdaptiveExecutor to monitor (a single Scan
/// node). The plan content doesn't affect the divergence logic — it's just
/// stored for context.
fn test_plan() -> LogicalPlan {
    LogicalPlan::new(PlanNode::Scan {
        region_id: 0,
        operator: Operator::ScanEqU64,
        params: KernelParams::default(),
    })
}

fn bench_adaptive_switching(c: &mut Criterion) {
    let mut group = c.benchmark_group("eddy/adaptive_switching");
    group.throughput(Throughput::Elements(1));

    let plan = test_plan();

    // The benchmark scenario from the Wave 16 task: cardinality estimate is
    // 10× off. Estimated 100, observed 1000 → divergence = 9.0.
    group.bench_function("observe_10x_misestimate", |b| {
        b.iter(|| {
            let mut exec = AdaptiveExecutor::new(plan.clone(), vec![100], 0.5);
            let switched = exec.observe(black_box(0), black_box(1000));
            let div = exec.max_divergence();
            // Sanity: 10× misestimate should trigger a switch.
            assert!(switched, "10x misestimate should trigger switch");
            assert!((div - 9.0).abs() < 1e-9, "divergence should be 9.0, got {div}");
            black_box((switched, div));
        });
    });

    // Baseline: accurate estimate. divergence = 0, no switch.
    group.bench_function("observe_accurate", |b| {
        b.iter(|| {
            let mut exec = AdaptiveExecutor::new(plan.clone(), vec![100], 0.5);
            let switched = exec.observe(black_box(0), black_box(100));
            let div = exec.max_divergence();
            assert!(!switched, "accurate estimate should not trigger switch");
            assert!((div - 0.0).abs() < 1e-9, "divergence should be 0.0, got {div}");
            black_box((switched, div));
        });
    });

    // Multi-stage: 5 stages, observe each. Measures the max_divergence
    // scan over a small vector.
    group.bench_function("observe_5_stages", |b| {
        b.iter(|| {
            let mut exec = AdaptiveExecutor::new(plan.clone(), vec![100, 200, 300, 400, 500], 0.5);
            exec.observe(0, 100); // accurate
            exec.observe(1, 220); // 10% off
            exec.observe(2, 300); // accurate
            exec.observe(3, 800); // 100% off
            exec.observe(4, 500); // accurate
            let switched = exec.should_switch();
            let div = exec.max_divergence();
            // Stage 3 is 100% off → divergence = 1.0 > 0.5 → switch.
            assert!(switched);
            assert!((div - 1.0).abs() < 1e-9, "max divergence should be 1.0, got {div}");
            black_box((switched, div));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_uniform_eddy_vs_fixed,
    bench_skewed_eddy_vs_fixed,
    bench_adaptive_switching,
);
criterion_main!(benches);
