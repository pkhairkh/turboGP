//! Tensor-network contraction vs DPccp join-ordering benchmark, plus
//! tensor-train compression measurement (Wave 17).
//!
//! ## Workloads
//!
//! 1. **Contraction ordering** for `n = 3, 5, 10` tables — compares the
//!    tensor-network greedy contraction planner
//!    ([`turbogp::planner::plan_with_tensor_network`]) against
//!    [`turbogp::planner::dpccp::dpccp`] on acyclic chain queries. The
//!    tensor-network planner is `O(n³)` vs. DPccp's `O(n² · 2ⁿ)`, so
//!    it should scale better at larger `n`.
//!
//! 2. **Tensor-train decomposition** on a `100 × 50` rank-3 matrix —
//!    measures the compression ratio achieved by
//!    [`turbogp::compress::TensorTrain::decompose`] with `max_rank = 5`.
//!    Expected ratio: ~`5000 / (100·3 + 3·50) = 11.1` for a clean
//!    rank-3 input.
//!
//! ## Throughput
//!
//! For the contraction-ordering benchmark, throughput is reported in
//! `Elements/sec`, where "elements" is the number of relations `n`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use turbogp::compress::TensorTrain;
use turbogp::planner::agm::JoinHypergraph;
use turbogp::planner::dpccp::{dpccp, JoinRelation};
use turbogp::planner::tensor::TensorNetwork;
use turbogp::planner::{contraction::contraction_to_join_tree, plan_with_tensor_network};

/// Build an acyclic chain query of `n` relations on `n + 1` attributes.
///
/// `R0(A0, A1), R1(A1, A2), …, R_{n-1}(A_{n-1}, A_n)`.
fn chain_query(n: usize) -> (Vec<JoinRelation>, JoinHypergraph, Vec<usize>) {
    let attrs: Vec<String> = (0..=n).map(|i| format!("A{i}")).collect();
    let attr_refs: Vec<&str> = (0..=n).map(|i| attrs[i].as_str()).collect();
    let rels: Vec<Vec<&str>> = (0..n).map(|i| vec![attr_refs[i], attr_refs[i + 1]]).collect();
    let graph = JoinHypergraph::from_named(&attr_refs, &rels);

    let relations: Vec<JoinRelation> = (0..n)
        .map(|i| JoinRelation {
            name: format!("R{i}"),
            cardinality: 100,
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
        })
        .collect();
    let cards = vec![100usize; n];
    (relations, graph, cards)
}

/// Benchmark contraction ordering vs. DPccp for `n = 3, 5, 10`.
fn bench_contraction_vs_dpccp(c: &mut Criterion) {
    let mut group = c.benchmark_group("contraction_ordering");
    group.throughput(Throughput::Elements(1));

    for &n in &[3usize, 5, 10] {
        let (relations, graph, cards) = chain_query(n);

        group.bench_with_input(BenchmarkId::new("tensor_network", n), &n, |b, _| {
            b.iter(|| {
                let plan = plan_with_tensor_network(
                    black_box(&relations),
                    black_box(&graph),
                    black_box(&cards),
                )
                .expect("tensor-network plan should succeed");
                black_box(plan.cost());
            });
        });

        group.bench_with_input(BenchmarkId::new("dpccp", n), &n, |b, _| {
            b.iter(|| {
                let plan = dpccp(black_box(&relations)).expect("DPccp plan should succeed");
                black_box(plan.cost());
            });
        });

        // Also bench the bare contraction-order + tree-build path,
        // without the wrapping plan_with_tensor_network entry point.
        group.bench_with_input(BenchmarkId::new("tensor_bare", n), &n, |b, _| {
            b.iter(|| {
                let net = TensorNetwork::from_hypergraph(black_box(&graph), black_box(&cards));
                let order = net.optimal_contraction_order();
                let tree = contraction_to_join_tree(
                    black_box(&net),
                    black_box(&order),
                    black_box(&relations),
                )
                .expect("contraction_to_join_tree should succeed");
                black_box(tree.cost());
            });
        });
    }

    group.finish();
}

/// Benchmark tensor-train decomposition on a 100×50 matrix.
///
/// The matrix is constructed as a sum of `r` outer products (rank `r`),
/// so the SVD finds exactly `r` non-zero singular values. We benchmark
/// with `r = 3` and `max_rank = 5` (so the truncation is non-binding).
fn bench_tensor_train_decompose(c: &mut Criterion) {
    let mut group = c.benchmark_group("tensor_train_decompose");

    let m = 100usize;
    let n = 50usize;
    let rank = 3usize;
    let max_rank = 5usize;

    // Build a 100×50 rank-3 matrix as sum of 3 outer products of
    // polynomial-Vandermonde vectors (linearly independent).
    let mut data = vec![vec![0.0_f64; n]; m];
    for k in 0..rank {
        let degree_a = k + 1;
        let degree_b = k + 2;
        let a: Vec<f64> = (0..m)
            .map(|i| {
                let x = (i as f64 + 1.0) * 0.01;
                x.powi(degree_a as i32)
            })
            .collect();
        let b: Vec<f64> = (0..n)
            .map(|j| {
                let y = (j as f64 + 1.0) * 0.05;
                y.powi(degree_b as i32)
            })
            .collect();
        for i in 0..m {
            for j in 0..n {
                data[i][j] += a[i] * b[j];
            }
        }
    }

    group.bench_function("100x50_rank3_maxrank5", |b| {
        b.iter(|| {
            let tt = TensorTrain::decompose(black_box(&data), black_box(max_rank));
            black_box(tt.compression_ratio());
            black_box(tt.reconstruct());
        });
    });

    // Report the compression ratio as a separate measurement.
    let tt = TensorTrain::decompose(&data, max_rank);
    group.bench_function("compression_ratio_check", |b| {
        b.iter(|| black_box(tt.compression_ratio()));
    });

    // Print the achieved compression ratio in the benchmark output.
    println!(
        "tensor_train 100×50 rank-3 max_rank={max_rank}: effective_rank = {}, compression_ratio = {:.3}",
        tt.effective_rank(),
        tt.compression_ratio()
    );

    group.finish();
}

criterion_group!(benches, bench_contraction_vs_dpccp, bench_tensor_train_decompose);
criterion_main!(benches);
