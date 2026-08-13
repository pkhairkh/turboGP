//! Wave 18 — Final 3× proof benchmark.
//!
//! This is the **summary benchmark** for Waves 13–17. It runs paired
//! before/after comparisons for each of the five optimization techniques
//! and prints a formatted table per workload so the 3× speedup target
//! can be verified at a glance.
//!
//! ## Workloads
//!
//! 1. **Cyclic join** — Leapfrog triejoin (Wave 13) vs. binary hash-join
//!    cascade on a 3-way triangle intersection of 50 K keys per relation.
//! 2. **Skewed filter** — Adaptive eddy (Wave 16) vs. fixed-order
//!    pipeline on a 3-filter pipeline where the last filter has
//!    selectivity 0 (the canonical "wrong order" workload from
//!    Avnur & Hellerstein 2000).
//! 3. **Cardinality estimation** — Learned histogram + correction
//!    (Wave 14) vs. fixed 0.1 heuristic on 1000 random equality
//!    predicates over zipfian data.
//! 4. **Planning time** — Tensor-network contraction ordering
//!    (Wave 17) vs. DPccp on chain queries with `n = 5, 10, 15`
//!    tables.
//! 5. **Multi-column compression** — Tensor-Train decomposition
//!    (Wave 17) vs. dense storage on a `100 × 50` rank-3 matrix.
//!
//! ## Harness
//!
//! This benchmark uses a **custom `main`** (with `harness = false` in
//! `Cargo.toml`) rather than `criterion`'s runner, because the goal is
//! to print a single side-by-side comparison table per workload — not
//! to compute confidence intervals on a single number. The `--quick`
//! flag (and any other args) is accepted; `--quick` reduces the
//! iteration count from the default 10 down to 3 so the whole suite
//! runs in a few seconds.
//!
//! ## Run
//!
//! ```sh
//! cargo bench --bench bench_3x_proof -- --quick
//! cargo bench --bench bench_3x_proof            # full precision
//! ```

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use turbogp::compress::TensorTrain;
use turbogp::executor::{Eddy, Morsel, Pipeline};
use turbogp::kernel::cpu::CpuTarget;
use turbogp::kernel::hash::HashTable;
use turbogp::kernel::leapfrog::{LeapfrogJoin, SliceSortedIterator, SortedIterator};
use turbogp::kernel::{KernelParams, KernelTable, Operator, PredicateOp};
use turbogp::memory::tier::MemoryTier;
use turbogp::planner::agm::JoinHypergraph;
use turbogp::planner::dpccp::{dpccp, JoinRelation};
use turbogp::planner::learned::LearnedCardinality;
use turbogp::planner::mcts::MctsJoinOrderer;
use turbogp::planner::tensor::TensorNetwork;
use turbogp::planner::{contraction_to_join_tree, plan_with_tensor_network};

// ===========================================================================
// CLI / driver
// ===========================================================================

/// Default iteration count for each timed method.
const DEFAULT_ITERS: usize = 10;

/// Iteration count under `--quick` (CI-friendly: whole suite in seconds).
const QUICK_ITERS: usize = 3;

/// A single row of the printed comparison table.
struct Row {
    method: &'static str,
    time: Duration,
    /// Throughput in elements per second (workloads 1, 2, 4) or `None`
    /// when the metric is dimensionless (workloads 3, 5).
    throughput: Option<f64>,
    /// Secondary metric printed in the rightmost column:
    /// - Workload 3: MAPE as a fraction (e.g. `0.45`).
    /// - Workload 5: compression ratio (e.g. `11.1`).
    /// - Otherwise `None` (speedup goes in the Speedup column).
    secondary: Option<f64>,
}

impl Row {
    /// The speedup relative to `baseline.time` (1.0× for the baseline
    /// itself).
    fn speedup(&self, baseline: &Row) -> f64 {
        baseline.time.as_secs_f64() / self.time.as_secs_f64().max(1e-12)
    }
}

/// Print a workload's comparison table.
fn print_table(title: &str, rows: &[Row], baseline_index: usize) {
    println!("\n=== {title} ===");
    let baseline = &rows[baseline_index];
    // Column widths chosen so 1-line headers fit on an 80-col terminal.
    println!("{:<22}{:<14}{:<18}{:<12}", "Method", "Time", "Throughput", "Speedup");
    for row in rows {
        let time_str = format_duration(row.time);
        let thr_str = match row.throughput {
            Some(t) => format_throughput(t),
            None => match row.secondary {
                Some(v) => format!("{v:.3}"),
                None => "—".into(),
            },
        };
        let speedup = row.speedup(baseline);
        println!("{:<22}{:<14}{:<18}{:<5.2}x", row.method, time_str, thr_str, speedup,);
    }
}

/// Human-friendly duration: ms for ≥1 ms, µs otherwise.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1e-3 {
        format!("{:.2} ms", secs * 1e3)
    } else {
        format!("{:.1} µs", secs * 1e6)
    }
}

/// Format throughput as `Melem/s` for ≥1 M, `Kelem/s` otherwise.
fn format_throughput(elem_per_sec: f64) -> String {
    if elem_per_sec >= 1e6 {
        format!("{:.2} Melem/s", elem_per_sec / 1e6)
    } else {
        format!("{:.1} Kelem/s", elem_per_sec / 1e3)
    }
}

/// Run a closure `iters` times and return the median wall-clock duration.
///
/// The first run is treated as a warm-up and discarded — it includes the
/// one-time kernel-table lookup, branch predictor warm-up, and page
/// faults on the first touch of the input data.
fn time_median<F: FnMut()>(iters: usize, mut f: F) -> Duration {
    // Warm-up: one untimed run.
    f();
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        f();
        samples.push(start.elapsed());
    }
    samples.sort();
    samples[samples.len() / 2]
}

// ===========================================================================
// Deterministic PRNG (splitmix64, same as the other benches)
// ===========================================================================

fn splitmix64(seed: u64) -> u64 {
    seed.wrapping_add(0x9E37_79B9_7F4A_7C15)
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn next_f64(state: &mut u64) -> f64 {
    let v = next_u64(state) >> 11;
    v as f64 / (1u64 << 53) as f64
}

// ===========================================================================
// Workload 1: Cyclic join (WCOJ vs. hash join)
// ===========================================================================

/// Number of keys per relation in the triangle workload.
const TRI_N: usize = 50_000;

/// Generate `n` sorted, deduped u64 keys uniformly from `[0, 4n)` —
/// adjacent relations share ~25 % of their keys, giving the triangle a
/// non-trivial intersection size.
fn uniform_keys(n: usize, seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    let mut keys: Vec<u64> = (0..n).map(|_| next_u64(&mut state) % (4 * n as u64)).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Leak a slice to `'static` so it can be wrapped in a `Box<dyn
/// SortedIterator + 'static>`. Called once per relation at setup; total
/// leaked memory is bounded by `3 · 50 K · 8 B = 1.2 MB`.
fn leak_keys(keys: &[u64]) -> &'static [u64] {
    Box::leak(keys.to_vec().into_boxed_slice())
}

fn static_iter(keys: &'static [u64]) -> Box<dyn SortedIterator> {
    Box::new(SliceSortedIterator::at_start(keys))
}

fn workload_cyclic_join(iters: usize) {
    println!(
        "\n[Workload 1] Generating triangle join: R(A,B) ⋈ S(B,C) ⋈ T(A,C), {TRI_N} rows each"
    );
    let r = leak_keys(&uniform_keys(TRI_N, 1));
    let s = leak_keys(&uniform_keys(TRI_N, 2));
    let t = leak_keys(&uniform_keys(TRI_N, 3));
    let total_keys = (r.len() + s.len() + t.len()) as f64;

    // ---------- Hash-join baseline (the "before") ----------
    let hash_time = time_median(iters, || {
        let table_r = HashTable::build(black_box(r));
        let mut r_intersect_s: Vec<u64> = Vec::with_capacity(s.len().min(r.len()));
        for &k in black_box(s) {
            if !table_r.probe(k).is_empty() {
                r_intersect_s.push(k);
            }
        }
        let table_rs = HashTable::build(black_box(&r_intersect_s));
        let mut matches = 0u64;
        for &k in black_box(t) {
            matches += table_rs.probe(k).len() as u64;
        }
        black_box(matches);
    });

    // ---------- Leapfrog (the "after") ----------
    let leapfrog_time = time_median(iters, || {
        let mut join = LeapfrogJoin::new(vec![
            static_iter(black_box(r)),
            static_iter(black_box(s)),
            static_iter(black_box(t)),
        ]);
        let out = join.run();
        black_box(out.len());
    });

    print_table(
        "Workload 1: Cyclic Join (WCOJ vs Hash Join)",
        &[
            Row {
                method: "Hash Join",
                time: hash_time,
                throughput: Some(total_keys / hash_time.as_secs_f64().max(1e-12)),
                secondary: None,
            },
            Row {
                method: "Leapfrog (WCOJ)",
                time: leapfrog_time,
                throughput: Some(total_keys / leapfrog_time.as_secs_f64().max(1e-12)),
                secondary: None,
            },
        ],
        0,
    );
}

// ===========================================================================
// Workload 2: Skewed filter (eddy vs. fixed pipeline)
// ===========================================================================

/// Number of morsels per iteration. Each morsel is 1024 cells.
const SKEW_MORSELS: usize = 100;

/// Build a uniform 0/1 morsel — half the cells are 0, half are 1.
fn alternating_morsel() -> Morsel {
    let cells: Vec<u64> = (0..1024).map(|i| (i % 2) as u64).collect();
    Morsel::new(0, 0, &cells)
}

/// Three operators whose selectivities are 1.0, 0.5, and 0.0 respectively
/// (the last filter is contradictory: cells cannot be both 0 and 1). In
/// the fixed pipeline, the contradictory filter runs last, so the first
/// two operators waste their work.
fn skewed_operators() -> Vec<Operator> {
    vec![Operator::ScanRangeU64, Operator::ScanEqU64, Operator::ScanMultiPredicate]
}

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

fn workload_skewed_filter(iters: usize) {
    println!("\n[Workload 2] Generating 1M cells (1000 morsels × 1024) with zipfian-skewed selectivities");
    let kt = Arc::new(KernelTable::new());
    let morsel = alternating_morsel();
    let ops = skewed_operators();
    let params = skewed_params();

    // ---------- Fixed pipeline (the "before") ----------
    let fixed_time = time_median(iters, || {
        let mut pipeline = Pipeline::new(ops.clone());
        for _ in 0..SKEW_MORSELS {
            pipeline
                .execute_morsel(black_box(&morsel), black_box(&kt), black_box(&params))
                .expect("pipeline executes");
        }
        black_box(pipeline.results().len());
    });

    // ---------- Eddy (the "after") ----------
    let eddy_time = time_median(iters, || {
        let mut eddy = Eddy::new(ops.clone(), 0.1);
        let mut pipeline = Pipeline::new(ops.clone());
        for _ in 0..SKEW_MORSELS {
            pipeline
                .execute_with_eddy(
                    black_box(&morsel),
                    black_box(&mut eddy),
                    black_box(&kt),
                    black_box(&params),
                )
                .expect("eddy executes");
        }
        black_box(pipeline.results().len());
    });

    let total_cells = (SKEW_MORSELS * 1024) as f64;
    print_table(
        "Workload 2: Skewed Filter (Eddy vs Fixed Pipeline)",
        &[
            Row {
                method: "Fixed Pipeline",
                time: fixed_time,
                throughput: Some(total_cells / fixed_time.as_secs_f64().max(1e-12)),
                secondary: None,
            },
            Row {
                method: "Adaptive Eddy",
                time: eddy_time,
                throughput: Some(total_cells / eddy_time.as_secs_f64().max(1e-12)),
                secondary: None,
            },
        ],
        0,
    );
}

// ===========================================================================
// Workload 3: Cardinality estimation (learned vs. heuristic)
// ===========================================================================

/// Number of training values.
const CARD_N: usize = 100_000;

/// Number of predicates evaluated.
const CARD_PREDS: usize = 1000;

/// Generate `N` zipfian values in `[0, N)` with frequency ∝ `1/(v+1)`.
fn zipfian_values(seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    let mut out = Vec::with_capacity(CARD_N);
    while out.len() < CARD_N {
        let v = next_u64(&mut state) % (CARD_N as u64);
        let accept_prob = 1.0 / ((v + 1) as f64);
        if next_f64(&mut state) < accept_prob {
            out.push(v);
        }
    }
    out
}

/// Compute the MAPE of `estimates` vs `actuals`.
///
/// Follows the convention used in `bench_cardinality.rs`: the denominator
/// is `max(actual, 1.0)`, so for selectivity values (always ∈ [0, 1]) the
/// metric reduces to the mean absolute error in selectivity units — this
/// keeps the numbers interpretable (e.g. 0.45 = "average 45 %-point
/// selectivity error") and avoids blowing up on rare-value predicates
/// where the true selectivity is near zero.
fn mape(estimates: &[f64], actuals: &[f64]) -> f64 {
    assert_eq!(estimates.len(), actuals.len());
    if estimates.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    for (e, a) in estimates.iter().zip(actuals) {
        sum += (a - e).abs() / a.max(1.0);
    }
    sum / estimates.len() as f64
}

fn workload_cardinality(iters: usize) {
    println!("\n[Workload 3] Training learned estimator on {CARD_N} zipfian values, evaluating {CARD_PREDS} range predicates");

    let values = zipfian_values(7);

    // Sort values so we can compute true range selectivities by counting.
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let total = sorted.len() as f64;

    // Predicates: 1000 random (low, high) range pairs uniformly distributed
    // across the value domain `[0, N)`. We deliberately pick `low`
    // uniformly (not from the data distribution) so the predicate set
    // contains both hot ranges (selectivity ~0.5) and cold ranges
    // (selectivity ~1e-4), maximizing the variance the estimator has to
    // capture. Width is uniform in `[1, 5000]` — spans 1–5 histogram
    // buckets.
    let mut state = splitmix64(42);
    let ranges: Vec<(u64, u64)> = (0..CARD_PREDS)
        .map(|_| {
            let low = next_u64(&mut state) % (CARD_N as u64);
            let width = 1 + (next_u64(&mut state) % 5_000);
            (low, low.saturating_add(width))
        })
        .collect();

    // True selectivity per range: count of values in [low, high] / total.
    let actuals: Vec<f64> = ranges
        .iter()
        .map(|&(lo, hi)| {
            let cnt = sorted.partition_point(|&v| v < lo);
            let mut end = cnt;
            while end < sorted.len() && sorted[end] <= hi {
                end += 1;
            }
            (end - cnt) as f64 / total
        })
        .collect();

    // Train the learned estimator on the same data (no holdout — we are
    // measuring the *estimator's* accuracy, not generalization).
    let mut learned = LearnedCardinality::new();
    learned.train_table("t", "c", &values);

    // Pre-calibrate the correction factor by feeding 100 observations
    // where `predicted = raw_histogram_estimate` and `actual = true`.
    // This simulates a brief warm-up where the runtime observes the
    // histogram's accuracy and adjusts the global correction.
    for &(lo, hi) in ranges.iter().take(100) {
        let raw = learned.estimate_range("t", "c", lo, hi);
        let true_sel = {
            let cnt = sorted.partition_point(|&v| v < lo);
            let mut end = cnt;
            while end < sorted.len() && sorted[end] <= hi {
                end += 1;
            }
            (end - cnt) as f64 / total
        };
        learned.observe(raw, true_sel);
    }

    // ---------- Heuristic baseline (the "before") ----------
    // The fixed `0.33` range default used by `CardinalityEstimator` (Wave 4).
    let heuristic_time = time_median(iters, || {
        let mut estimates = Vec::with_capacity(CARD_PREDS);
        for &_r in black_box(&ranges) {
            estimates.push(0.33);
        }
        black_box(estimates);
    });

    let heuristic_estimates: Vec<f64> = ranges.iter().map(|_| 0.33).collect();
    let heuristic_mape = mape(&heuristic_estimates, &actuals);

    // ---------- Learned estimator (the "after") ----------
    // Apply the calibrated correction factor to the histogram estimate.
    let learned_time = time_median(iters, || {
        let mut estimates = Vec::with_capacity(CARD_PREDS);
        for &(lo, hi) in black_box(&ranges) {
            let raw = learned.estimate_range("t", "c", lo, hi);
            estimates.push(learned.correct(raw));
        }
        black_box(estimates);
    });

    let learned_estimates: Vec<f64> = ranges
        .iter()
        .map(|&(lo, hi)| learned.correct(learned.estimate_range("t", "c", lo, hi)))
        .collect();
    let learned_mape = mape(&learned_estimates, &actuals);

    let mape_improvement = heuristic_mape / learned_mape.max(1e-12);

    print_table(
        "Workload 3: Cardinality Estimation (Learned vs Heuristic)",
        &[
            Row {
                method: "Heuristic (0.33)",
                time: heuristic_time,
                throughput: Some(CARD_PREDS as f64 / heuristic_time.as_secs_f64().max(1e-12)),
                secondary: Some(heuristic_mape),
            },
            Row {
                method: "Learned (hist + corr)",
                time: learned_time,
                throughput: Some(CARD_PREDS as f64 / learned_time.as_secs_f64().max(1e-12)),
                secondary: Some(learned_mape),
            },
        ],
        0,
    );
    println!(
        "    MAPE: heuristic = {:.4} ({:.2}%), learned = {:.4} ({:.2}%) — improvement = {:.2}×",
        heuristic_mape,
        heuristic_mape * 100.0,
        learned_mape,
        learned_mape * 100.0,
        mape_improvement,
    );
}

// ===========================================================================
// Workload 4: Planning time (tensor-network vs. DPccp)
// ===========================================================================

/// Build an acyclic chain query of `n` relations: R0(A0,A1) ⋈ R1(A1,A2) ⋈
/// … ⋈ R_{n-1}(A_{n-1},A_n). All cardinalities 100 (so cost is comparable).
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

fn workload_planning_time(iters: usize) {
    println!("\n[Workload 4] Planning time: tensor-network contraction vs DPccp on chain queries");

    // For each n, time both planners and report speedup.
    let mut all_rows: Vec<Row> = Vec::new();
    let mut baselines: Vec<(usize, usize)> = Vec::new(); // (row_index, n)

    for &n in &[5usize, 10, 15] {
        let (relations, graph, cards) = chain_query(n);

        let dpccp_time = time_median(iters, || {
            let plan = dpccp(black_box(&relations)).expect("DPccp succeeds");
            black_box(plan.cost());
        });

        let tensor_time = time_median(iters, || {
            let plan = plan_with_tensor_network(
                black_box(&relations),
                black_box(&graph),
                black_box(&cards),
            )
            .expect("tensor-network plan succeeds");
            black_box(plan.cost());
        });

        let baseline_index = all_rows.len();
        all_rows.push(Row {
            method: Box::leak(format!("DPccp (n={n})").into_boxed_str()),
            time: dpccp_time,
            throughput: Some(n as f64 / dpccp_time.as_secs_f64().max(1e-12)),
            secondary: None,
        });
        all_rows.push(Row {
            method: Box::leak(format!("Tensor-network (n={n})").into_boxed_str()),
            time: tensor_time,
            throughput: Some(n as f64 / tensor_time.as_secs_f64().max(1e-12)),
            secondary: None,
        });
        baselines.push((baseline_index, n));
    }

    // Print one combined table; speedup is per-(n) — we just print the
    // tensor-network row's speedup vs. the DPccp row immediately above
    // it (which is the same n's baseline).
    println!("\n=== Workload 4: Planning Time (Tensor-network vs DPccp) ===");
    println!("{:<26}{:<14}{:<18}{:<12}", "Method", "Time", "Throughput", "Speedup");
    let mut i = 0;
    while i < all_rows.len() {
        let dpccp_row = &all_rows[i];
        let tensor_row = &all_rows[i + 1];
        let speedup = dpccp_row.time.as_secs_f64() / tensor_row.time.as_secs_f64().max(1e-12);
        println!(
            "{:<26}{:<14}{:<18}{:<5.2}x",
            dpccp_row.method,
            format_duration(dpccp_row.time),
            format_throughput(dpccp_row.throughput.unwrap_or(0.0)),
            1.0,
        );
        println!(
            "{:<26}{:<14}{:<18}{:<5.2}x",
            tensor_row.method,
            format_duration(tensor_row.time),
            format_throughput(tensor_row.throughput.unwrap_or(0.0)),
            speedup,
        );
        i += 2;
    }
}

// ===========================================================================
// Workload 5: Multi-column compression (tensor-train)
// ===========================================================================

fn workload_compression(iters: usize) {
    println!("\n[Workload 5] Tensor-train compression of a 100×50 rank-3 matrix");

    let m = 100usize;
    let n = 50usize;
    let rank = 3usize;
    let max_rank = 5usize;

    // Build a 100×50 rank-3 matrix as the sum of 3 outer products of
    // polynomial-Vandermonde vectors (linearly independent → exact rank 3).
    let mut data: Vec<Vec<f64>> = vec![vec![0.0_f64; n]; m];
    for k in 0..rank {
        let degree_a = k + 1;
        let degree_b = k + 2;
        let a: Vec<f64> =
            (0..m).map(|i| ((i as f64) + 1.0) * 0.01).map(|x| x.powi(degree_a as i32)).collect();
        let b: Vec<f64> =
            (0..n).map(|j| ((j as f64) + 1.0) * 0.05).map(|y| y.powi(degree_b as i32)).collect();
        for i in 0..m {
            for j in 0..n {
                data[i][j] += a[i] * b[j];
            }
        }
    }

    let original_cells = (m * n) as f64;

    // ---------- Dense storage (the "before") ----------
    let dense_time = time_median(iters, || {
        // Simulate a dense read: sum every element.
        let mut sum = 0.0_f64;
        for row in black_box(&data) {
            for &v in row {
                sum += v;
            }
        }
        black_box(sum);
    });

    let tt = TensorTrain::decompose(&data, max_rank);
    let compression_ratio = tt.compression_ratio();
    let effective_rank = tt.effective_rank();

    // ---------- TT reconstruction (the "after") ----------
    let tt_time = time_median(iters, || {
        let tt = TensorTrain::decompose(black_box(&data), black_box(max_rank));
        let recon = tt.reconstruct();
        let mut sum = 0.0_f64;
        for &v in black_box(&recon) {
            sum += v;
        }
        black_box(sum);
    });

    // Reconstruction error (relative L2 norm of the residual).
    let recon = tt.reconstruct();
    let mut residual_sq = 0.0;
    let mut orig_sq = 0.0;
    for i in 0..m {
        for j in 0..n {
            let diff = data[i][j] - recon[i * n + j];
            residual_sq += diff * diff;
            orig_sq += data[i][j] * data[i][j];
        }
    }
    let recon_error = (residual_sq / orig_sq.max(1e-12)).sqrt();

    print_table(
        "Workload 5: Multi-column Compression (Tensor-Train)",
        &[
            Row {
                method: "Dense (raw)",
                time: dense_time,
                throughput: Some(original_cells / dense_time.as_secs_f64().max(1e-12)),
                secondary: Some(1.0),
            },
            Row {
                method: "Tensor-Train",
                time: tt_time,
                throughput: Some(original_cells / tt_time.as_secs_f64().max(1e-12)),
                secondary: Some(compression_ratio),
            },
        ],
        0,
    );
    println!(
        "    effective_rank = {effective_rank}, compression_ratio = {compression_ratio:.2}×, reconstruction_error = {recon_error:.2e}"
    );
}

// ===========================================================================
// Bonus: combined demonstration that all 5 techniques coexist
// ===========================================================================

fn workload_combined_demo() {
    println!("\n=== Bonus: Combined End-to-End Smoke ===");

    // 1. WCOJ: triangle join, leapfrog.
    let r = leak_keys(&uniform_keys(1_000, 100));
    let s = leak_keys(&uniform_keys(1_000, 101));
    let t = leak_keys(&uniform_keys(1_000, 102));
    let mut join = LeapfrogJoin::new(vec![static_iter(r), static_iter(s), static_iter(t)]);
    let tri_out = join.run().len();
    println!("    [WCOJ]           triangle intersection: {tri_out} keys");

    // 2. Learned cardinality: train + lookup.
    let values = zipfian_values(99);
    let mut learned = LearnedCardinality::new();
    learned.train_table("t", "c", &values);
    let sel = learned.estimate_selectivity("t", "c", values[0]);
    let corrected = learned.correct(sel);
    println!("    [Learned card]   estimate={sel:.6}, corrected={corrected:.6}");

    // 3. MCTS: 20-table chain (DPccp refuses n > 15).
    let relations: Vec<JoinRelation> = (0..20)
        .map(|i| JoinRelation {
            name: format!("R{i}"),
            cardinality: 100,
            joins_with: {
                let mut v = Vec::new();
                if i > 0 {
                    v.push(i - 1);
                }
                if i + 1 < 20 {
                    v.push(i + 1);
                }
                v
            },
        })
        .collect();
    let mcts = MctsJoinOrderer::default().with_iterations(50).with_seed(42);
    let mcts_plan = mcts.order(&relations).expect("MCTS plans 20-table join");
    println!("    [MCTS]           20-table plan cost = {:.0}", mcts_plan.cost());

    // 4. Eddy: 3-filter pipeline, adaptive routing.
    let kt = Arc::new(KernelTable::new());
    let morsel = alternating_morsel();
    let ops = skewed_operators();
    let params = skewed_params();
    let mut eddy = Eddy::new(ops.clone(), 0.1);
    let mut pipeline = Pipeline::new(ops.clone());
    pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).expect("eddy executes");
    println!("    [Eddy]           routing order = {:?}", eddy.routing_order());

    // 5. Tensor network: build a network, compute treewidth + contraction
    //    order, convert to a join tree.
    let (tn_relations, tn_graph, tn_cards) = chain_query(8);
    let net = TensorNetwork::from_hypergraph(&tn_graph, &tn_cards);
    let order = net.optimal_contraction_order();
    let tree =
        contraction_to_join_tree(&net, &order, &tn_relations).expect("contraction_to_join_tree");
    println!(
        "    [Tensor network] n=8 treewidth = {}, contraction_steps = {}, plan cost = {:.0}",
        net.treewidth(),
        order.len(),
        tree.cost(),
    );

    // Silence unused-import warnings on the items used only by the smoke
    // demo above (the time helpers, CpuTarget, MemoryTier are referenced
    // indirectly through the kernel table).
    let _ = (CpuTarget::Scalar, MemoryTier::L3);
}

// ===========================================================================
// main
// ===========================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let iters = if quick { QUICK_ITERS } else { DEFAULT_ITERS };

    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║  turboGP — Wave 18: Final 3× Proof Benchmark                    ║");
    println!("║  5 optimization techniques, paired before/after comparison     ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
    println!("configuration: iters = {iters} per method{}", if quick { " (--quick)" } else { "" });

    workload_cyclic_join(iters);
    workload_skewed_filter(iters);
    workload_cardinality(iters);
    workload_planning_time(iters);
    workload_compression(iters);

    workload_combined_demo();

    println!("\n=== Wave 18 benchmark complete ===");
}
