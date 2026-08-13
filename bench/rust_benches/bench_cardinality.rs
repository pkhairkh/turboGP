//! Learned-cardinality estimation benchmark.
//!
//! This benchmark measures the **accuracy** and **throughput** of the
//! learned cardinality estimator
//! ([`turbogp::planner::learned::LearnedCardinality`]) on three synthetic
//! distributions:
//!
//! 1. **Uniform** — values drawn uniformly from `[0, N)`. The histogram
//!    should produce ~equal bucket counts; equality selectivity for any
//!    value ≈ `1/N`.
//! 2. **Zipfian** — values drawn with frequency proportional to `1/v`
//!    (hot-key distribution). The first bucket should dominate; the
//!    learned estimator's correction factor should adapt to the skew.
//! 3. **Normal** — values drawn from a Gaussian centered at `N/2` with
//!    standard deviation `N/8`. The middle buckets should dominate.
//!
//! ## Methodology
//!
//! For each distribution:
//!
//! 1. Generate `N = 100 000` values.
//! 2. Train the histogram on the values.
//! 3. Generate `1 000` random equality predicates (values drawn from the
//!    same distribution).
//! 4. For each predicate, compute:
//!    - **Raw estimate**: `estimator.estimate_selectivity(table, col, v)`
//!      (histogram bucket density).
//!    - **Corrected estimate**: `estimator.correct(raw_estimate)` (apply
//!      the global correction factor).
//!    - **True selectivity**: `count(v) / N` (the actual fraction of rows
//!      equal to `v`).
//! 5. Measure the MAPE before calibration (correction = 1.0) and after
//!    100 observations (correction has converged).
//!
//! ## Throughput groups
//!
//! - `train/{uniform,zipfian,normal}` — building a 100-bucket histogram
//!   over 100 K values. Measures the `Histogram::build` cost.
//! - `estimate_selectivity/{uniform,zipfian,normal}` — 1 K equality
//!   selectivity lookups. Measures the `estimate_selectivity` cost.
//! - `estimate_range/{uniform,zipfian,normal}` — 1 K range selectivity
//!   lookups. Measures the `estimate_range` cost.
//! - `calibrate/{uniform,zipfian,normal}` — 100 `observe` calls on
//!   biased `(predicted, actual)` pairs. Measures the calibration cost
//!   and prints the MAPE before/after.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use turbogp::planner::calibration::CalibrationLoop;
use turbogp::planner::learned::LearnedCardinality;

/// Number of values to train each histogram on.
const N: usize = 100_000;

/// Number of predicates to evaluate per benchmark iteration.
const N_PREDS: usize = 1_000;

/// Number of calibration observations for the "after" MAPE measurement.
const N_OBS: usize = 100;

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64)
// ---------------------------------------------------------------------------

/// Initialize a splitmix64 state from a seed.
fn splitmix64(seed: u64) -> u64 {
    seed.wrapping_add(0x9E37_79B9_7F4A_7C15)
}

/// Step a splitmix64 state forward and return the next `u64`.
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Convert a splitmix64 output to `f64` in `[0, 1)`.
fn next_f64(state: &mut u64) -> f64 {
    let v = next_u64(state) >> 11;
    v as f64 / (1u64 << 53) as f64
}

/// Box-Muller transform: convert two uniform `[0, 1)` samples to a
/// standard-normal sample.
fn next_normal(state: &mut u64) -> f64 {
    let u1 = next_f64(state).max(1e-10);
    let u2 = next_f64(state);
    let r = (-2.0_f64 * u1.ln()).sqrt();
    let theta = 2.0_f64 * std::f64::consts::PI * u2;
    r * theta.cos()
}

// ---------------------------------------------------------------------------
// Distribution generators
// ---------------------------------------------------------------------------

/// Generate `N` uniform values in `[0, N)`.
fn uniform_values(seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    (0..N).map(|_| next_u64(&mut state) % (N as u64)).collect()
}

/// Generate `N` zipfian values in `[0, N)` with frequency ∝ `1/(v+1)`.
///
/// Uses the rejection method: draw `v` uniformly; accept with
/// probability `1/(v+1)` (normalized by the max probability `1/1 = 1`).
/// This produces a true Zipf(1) distribution over `[0, N)`.
fn zipfian_values(seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    let mut out = Vec::with_capacity(N);
    while out.len() < N {
        let v = next_u64(&mut state) % (N as u64);
        let accept_prob = 1.0 / ((v + 1) as f64);
        if next_f64(&mut state) < accept_prob {
            out.push(v);
        }
    }
    out
}

/// Generate `N` normal-distributed values: `round(N/2 + N/8 · Z)` where
/// `Z ~ N(0, 1)`, clamped to `[0, N)`.
fn normal_values(seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    let mean = (N / 2) as f64;
    let sd = (N / 8) as f64;
    (0..N)
        .map(|_| {
            let v = mean + sd * next_normal(&mut state);
            let v = v.round() as i64;
            let v = v.max(0) as u64;
            v.min((N - 1) as u64)
        })
        .collect()
}

/// Generate `N_PREDS` predicate values from the same distribution as the
/// training data (so the predicates have non-trivial selectivity).
fn predicate_values(values: &[u64], seed: u64) -> Vec<u64> {
    let mut state = splitmix64(seed);
    (0..N_PREDS)
        .map(|_| {
            let idx = (next_u64(&mut state) as usize) % values.len();
            values[idx]
        })
        .collect()
}

// ---------------------------------------------------------------------------
// MAPE helper (mirrors CalibrationLoop::mape)
// ---------------------------------------------------------------------------

/// Compute the mean absolute percentage error of (predicted, actual) pairs.
fn mape(obs: &[(f64, f64)]) -> f64 {
    if obs.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    for &(p, a) in obs {
        sum += (a - p).abs() / a.max(1.0);
    }
    sum / obs.len() as f64
}

// ---------------------------------------------------------------------------
// Benchmark 1: train (build histogram over N values)
// ---------------------------------------------------------------------------

fn bench_train(c: &mut Criterion) {
    let mut group = c.benchmark_group("learned_cardinality/train");
    group.throughput(Throughput::Elements(N as u64));

    let uniform = uniform_values(1);
    let zipfian = zipfian_values(2);
    let normal = normal_values(3);

    let cases: [(&str, &[u64]); 3] =
        [("uniform", &uniform[..]), ("zipfian", &zipfian[..]), ("normal", &normal[..])];

    for (name, values) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), values, |b, vals| {
            b.iter(|| {
                let mut est = LearnedCardinality::new();
                est.train_table("t", "c", black_box(vals));
                black_box(est);
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: estimate_selectivity (1000 lookups)
// ---------------------------------------------------------------------------

fn bench_estimate_selectivity(c: &mut Criterion) {
    let mut group = c.benchmark_group("learned_cardinality/estimate_selectivity");
    group.throughput(Throughput::Elements(N_PREDS as u64));

    let cases: [(&str, Vec<u64>); 3] = [
        ("uniform", uniform_values(1)),
        ("zipfian", zipfian_values(2)),
        ("normal", normal_values(3)),
    ];

    for (name, values) in cases {
        let mut est = LearnedCardinality::new();
        est.train_table("t", "c", &values);
        let preds = predicate_values(&values, 42);
        group.bench_with_input(BenchmarkId::from_parameter(name), &preds, |b, preds| {
            b.iter(|| {
                let mut sum = 0.0_f64;
                for &v in black_box(preds) {
                    sum += est.estimate_selectivity("t", "c", v);
                }
                black_box(sum);
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: estimate_range (1000 lookups)
// ---------------------------------------------------------------------------

fn bench_estimate_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("learned_cardinality/estimate_range");
    group.throughput(Throughput::Elements(N_PREDS as u64));

    let cases: [(&str, Vec<u64>); 3] = [
        ("uniform", uniform_values(1)),
        ("zipfian", zipfian_values(2)),
        ("normal", normal_values(3)),
    ];

    for (name, values) in cases {
        let mut est = LearnedCardinality::new();
        est.train_table("t", "c", &values);
        let preds = predicate_values(&values, 42);
        // Build (low, high) pairs: low = pred, high = pred + bucket_width
        // (so each range spans 1-2 buckets).
        let ranges: Vec<(u64, u64)> = preds.iter().map(|&v| (v, v + 10)).collect();
        group.bench_with_input(BenchmarkId::from_parameter(name), &ranges, |b, ranges| {
            b.iter(|| {
                let mut sum = 0.0_f64;
                for &(lo, hi) in black_box(ranges) {
                    sum += est.estimate_range("t", "c", lo, hi);
                }
                black_box(sum);
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 4: calibrate (100 observe calls, measure MAPE before/after)
// ---------------------------------------------------------------------------

/// Calibration benchmark: for each distribution, build the histogram,
/// then run 100 calibration observations where `predicted` is the raw
/// histogram estimate (with a 2× systematic bias injected — simulating a
/// stale histogram trained on half the data) and `actual` is the true
/// selectivity (with 10 % zero-mean noise).
///
/// Prints the MAPE before calibration (raw predictions vs actual) and
/// after calibration (corrected predictions vs actual), plus the
/// converged correction factor.
///
/// The 2× bias lets the benchmark demonstrate the correction factor's
/// effect: with no bias, the raw estimate is already accurate and the
/// correction stays at 1.0, so `mape_before == mape_after`. With the 2×
/// bias, `mape_before` is high (~100 %) and `mape_after` is low (~10 %,
/// dominated by the residual noise) — the correction has converged to
/// ~2.0, undoing the bias.
fn bench_calibrate(c: &mut Criterion) {
    let mut group = c.benchmark_group("learned_cardinality/calibrate");
    group.throughput(Throughput::Elements(N_OBS as u64));

    let cases: [(&str, Vec<u64>); 3] = [
        ("uniform", uniform_values(1)),
        ("zipfian", zipfian_values(2)),
        ("normal", normal_values(3)),
    ];

    for (name, values) in cases {
        // Pre-compute the per-value true frequency.
        let mut true_counts: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::new();
        for &v in &values {
            *true_counts.entry(v).or_insert(0) += 1;
        }
        let total = values.len() as f64;

        // Pre-generate (predicted, actual) observation pairs.
        // - `actual` = true selectivity (with 10 % noise), in [0, 1].
        // - `predicted` = raw histogram estimate * 0.5 (simulating a
        //   stale histogram that under-predicts by 2×).
        // The correction factor should converge to ~2.0 (since
        // `actual / predicted ≈ 2.0`).
        let mut state = splitmix64(99);
        let mut obs_pairs: Vec<(f64, f64)> = Vec::with_capacity(N_OBS);
        let mut est_for_gen = LearnedCardinality::new();
        est_for_gen.train_table("t", "c", &values);
        for _ in 0..N_OBS {
            let idx = (next_u64(&mut state) as usize) % values.len();
            let v = values[idx];
            let raw = est_for_gen.estimate_selectivity("t", "c", v);
            let biased_predicted = raw * 0.5; // 2× under-prediction.
            let true_sel = (*true_counts.get(&v).unwrap_or(&0) as f64) / total;
            let noise = (next_f64(&mut state) * 2.0 - 1.0) * 0.10;
            let actual = (true_sel * (1.0 + noise)).max(0.0);
            obs_pairs.push((biased_predicted, actual));
        }

        // MAPE before calibration (raw biased predictions vs actual).
        // For selectivities in [0, 1], the `max(actual, 1.0)` guard in
        // the MAPE definition kicks in (actual < 1.0), so this measures
        // the absolute error scaled by 1.0 — still a useful relative
        // metric for before/after comparison.
        let mape_before = mape(&obs_pairs);

        // Run the calibration loop and compute the corrected MAPE.
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        cl.estimator_mut().train_table("t", "c", &values);
        for &(p, a) in &obs_pairs {
            cl.record(p, a);
        }
        let correction = cl.correction();

        // MAPE after calibration: apply the correction to each predicted
        // value and recompute. (CalibrationLoop::mape measures the raw
        // MAPE; we want the corrected MAPE here.)
        let corrected_obs: Vec<(f64, f64)> =
            obs_pairs.iter().map(|&(p, a)| (p * correction, a)).collect();
        let mape_after = mape(&corrected_obs);

        println!(
            "[bench_cardinality] {name}: MAPE before = {:.4} ({:.2}%), after = {:.4} ({:.2}%), correction = {:.4}",
            mape_before,
            mape_before * 100.0,
            mape_after,
            mape_after * 100.0,
            correction
        );

        group.bench_with_input(BenchmarkId::from_parameter(name), &obs_pairs, |b, obs| {
            b.iter(|| {
                let mut cl = CalibrationLoop::new(LearnedCardinality::new());
                cl.estimator_mut().train_table("t", "c", &values);
                for &(p, a) in black_box(obs) {
                    cl.record(p, a);
                }
                black_box(cl.correction());
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 5: estimate_join (1000 join lookups)
// ---------------------------------------------------------------------------

fn bench_estimate_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("learned_cardinality/estimate_join");
    group.throughput(Throughput::Elements(N_PREDS as u64));

    // Two tables with overlapping value ranges.
    let left = uniform_values(1);
    let right = uniform_values(2);

    let mut est = LearnedCardinality::new();
    est.train_table("L", "k", &left);
    est.train_table("R", "k", &right);

    // Pre-generate (left_rows, right_rows) pairs to vary the FK fallback.
    let mut state = splitmix64(7);
    let sizes: Vec<(usize, usize)> = (0..N_PREDS)
        .map(|_| {
            let l = 100 + (next_u64(&mut state) as usize) % 1000;
            let r = 100 + (next_u64(&mut state) as usize) % 1000;
            (l, r)
        })
        .collect();

    group.bench_function("1000_joins", |b| {
        b.iter(|| {
            let mut sum = 0.0_f64;
            for &(l, r) in black_box(&sizes) {
                sum += est.estimate_join("L", "R", "k", "k", l, r);
            }
            black_box(sum);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_train,
    bench_estimate_selectivity,
    bench_estimate_range,
    bench_calibrate,
    bench_estimate_join,
);
criterion_main!(benches);
