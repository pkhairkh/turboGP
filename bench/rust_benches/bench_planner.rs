//! DPccp vs MCTS join-ordering benchmark.
//!
//! This benchmark compares the two join-ordering planners in turboGP:
//!
//! 1. **[`turbogp::planner::dpccp`]** — optimal left-deep DPccp
//!    (`O(n²·2ⁿ)`). Fast for `n ≤ 15`, intractable beyond.
//! 2. **[`turbogp::planner::mcts::MctsJoinOrderer`]** — anytime MCTS
//!    (`O(iterations · n)`). Slower per-iteration than DPccp for small `n`,
//!    but scales to `n > 15`.
//!
//! ## Workloads
//!
//! - **`n = 5` star query** — both planners handle this. The benchmark
//!   checks that MCTS finds a plan whose cost is within 2× of the DPccp
//!   optimum (it usually finds the optimum exactly), and measures the
//!   planning-time gap.
//! - **`n = 10` chain query** — both planners handle this. Compares planning
//!   time at the upper end of DPccp's comfort zone.
//! - **`n = 20` chain query** — MCTS only (DPccp rejects `n > 15`). Measures
//!   MCTS's planning time at a scale DPccp cannot reach.
//! - **`n = 30` chain query** — MCTS only. Measures MCTS's planning time at
//!   a scale that would take DPccp ~`30² · 2³⁰ ≈ 10¹²` operations.
//!
//! ## Throughput
//!
//! Throughput is reported in `Elements/sec`, where "elements" is the number
//! of relations `n` — i.e., the planning throughput in joins/sec.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use turbogp::planner::dpccp::{dpccp, JoinRelation};
use turbogp::planner::mcts::MctsJoinOrderer;
use turbogp::planner::order_joins;

/// Build a chain join graph `R0 - R1 - ... - R(n-1)` with varying
/// cardinalities drawn from a deterministic splitmix64 PRNG (so the
/// benchmark is reproducible across runs).
fn chain_relations(n: usize) -> Vec<JoinRelation> {
    let mut state = 0xDEAD_BEEF_CAFE_BABE_u64;
    (0..n)
        .map(|i| {
            let card = 100 + (next_u64(&mut state) % 1000) as usize;
            JoinRelation {
                name: format!("R{i}"),
                cardinality: card,
                joins_with: {
                    let mut v = Vec::new();
                    if i > 0 {
                        v.push(i - 1);
                    }
                    if i + 1 < n {
                        v.push(i + 1);
                    }
                    v
                },
            }
        })
        .collect()
}

/// Build a star join graph: a center C plus `n - 1` satellites, each
/// satellite joins only with the center. The center has cardinality 1000
/// (large) and each satellite has cardinality 10 (small) — the optimal plan
/// starts with a satellite, not the center.
fn star_relations(n: usize) -> Vec<JoinRelation> {
    assert!(n >= 2);
    let mut relations = Vec::with_capacity(n);
    relations.push(JoinRelation {
        name: "C".into(),
        cardinality: 1000,
        joins_with: (1..n).collect(),
    });
    for i in 1..n {
        relations.push(JoinRelation {
            name: format!("S{i}"),
            cardinality: 10,
            joins_with: vec![0],
        });
    }
    relations
}

/// A deterministic splitmix64 PRNG step. (Same as in `bench_wcoj.rs` and
/// `bench_cardinality.rs` — kept inline to avoid a shared utility module.)
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Benchmark 1: n=5 star query — DPccp (optimal) vs MCTS (within 2x)
// ---------------------------------------------------------------------------

fn bench_n5_star_dpccp_vs_mcts(c: &mut Criterion) {
    let relations = star_relations(5);
    let n = relations.len();

    let mut group = c.benchmark_group("planner/n5_star");
    group.throughput(Throughput::Elements(n as u64));

    // DPccp reference plan (optimal).
    let dpccp_plan = dpccp(&relations).expect("DPccp n=5 should succeed");
    let dpccp_cost = dpccp_plan.cost();

    // Verify MCTS finds a plan within 2x of the DPccp optimum. This is a
    // sanity check that runs once at benchmark setup time (not measured).
    let mcts_check = MctsJoinOrderer::new()
        .with_iterations(2000)
        .order(&relations)
        .expect("MCTS n=5 should succeed");
    let mcts_check_cost = mcts_check.cost();
    assert!(
        mcts_check_cost <= 2.0 * dpccp_cost,
        "MCTS n=5 cost {mcts_check_cost} should be within 2x of DPccp optimal {dpccp_cost}"
    );

    group.bench_function("dpccp", |b| {
        b.iter(|| {
            let plan = dpccp(black_box(&relations)).expect("DPccp should succeed");
            black_box(plan.cost());
        });
    });

    group.bench_function("mcts_2000", |b| {
        b.iter(|| {
            let orderer = MctsJoinOrderer::new().with_iterations(2000);
            let plan = orderer.order(black_box(&relations)).expect("MCTS should succeed");
            black_box(plan.cost());
        });
    });

    group.bench_function("order_joins_dispatch", |b| {
        b.iter(|| {
            let plan = order_joins(black_box(&relations)).expect("order_joins should succeed");
            black_box(plan.cost());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: n=10 chain query — DPccp vs MCTS planning time
// ---------------------------------------------------------------------------

fn bench_n10_chain_dpccp_vs_mcts(c: &mut Criterion) {
    let relations = chain_relations(10);
    let n = relations.len();

    let mut group = c.benchmark_group("planner/n10_chain");
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("dpccp", |b| {
        b.iter(|| {
            let plan = dpccp(black_box(&relations)).expect("DPccp should succeed");
            black_box(plan.cost());
        });
    });

    group.bench_function("mcts_2000", |b| {
        b.iter(|| {
            let orderer = MctsJoinOrderer::new().with_iterations(2000);
            let plan = orderer.order(black_box(&relations)).expect("MCTS should succeed");
            black_box(plan.cost());
        });
    });

    group.bench_function("order_joins_dispatch", |b| {
        b.iter(|| {
            let plan = order_joins(black_box(&relations)).expect("order_joins should succeed");
            black_box(plan.cost());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: n=20 chain query — MCTS only (DPccp cannot handle this)
// ---------------------------------------------------------------------------

fn bench_n20_chain_mcts_only(c: &mut Criterion) {
    let relations = chain_relations(20);
    let n = relations.len();

    let mut group = c.benchmark_group("planner/n20_chain");
    group.throughput(Throughput::Elements(n as u64));

    // Sanity check: DPccp rejects this.
    assert!(dpccp(&relations).is_err(), "DPccp should reject n=20");

    // Vary the iteration budget to show the cost-vs-quality trade-off.
    for &iters in &[500usize, 2000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("mcts", format!("iters_{iters}")),
            &iters,
            |b, &iters| {
                b.iter(|| {
                    let orderer = MctsJoinOrderer::new().with_iterations(iters);
                    let plan = orderer.order(black_box(&relations)).expect("MCTS should succeed");
                    black_box(plan.cost());
                });
            },
        );
    }

    group.bench_function("order_joins_dispatch", |b| {
        b.iter(|| {
            let plan = order_joins(black_box(&relations)).expect("order_joins should succeed");
            black_box(plan.cost());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 4: n=30 chain query — MCTS only
// ---------------------------------------------------------------------------

fn bench_n30_chain_mcts_only(c: &mut Criterion) {
    let relations = chain_relations(30);
    let n = relations.len();

    let mut group = c.benchmark_group("planner/n30_chain");
    group.throughput(Throughput::Elements(n as u64));

    // Sanity check: DPccp rejects this.
    assert!(dpccp(&relations).is_err(), "DPccp should reject n=30");

    for &iters in &[500usize, 2000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("mcts", format!("iters_{iters}")),
            &iters,
            |b, &iters| {
                b.iter(|| {
                    let orderer = MctsJoinOrderer::new().with_iterations(iters);
                    let plan = orderer.order(black_box(&relations)).expect("MCTS should succeed");
                    black_box(plan.cost());
                });
            },
        );
    }

    group.bench_function("order_joins_dispatch", |b| {
        b.iter(|| {
            let plan = order_joins(black_box(&relations)).expect("order_joins should succeed");
            black_box(plan.cost());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_n5_star_dpccp_vs_mcts,
    bench_n10_chain_dpccp_vs_mcts,
    bench_n20_chain_mcts_only,
    bench_n30_chain_mcts_only,
);
criterion_main!(benches);
