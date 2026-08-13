//! Sketch performance benchmarks — HLL, Count-Min, t-Digest.
//!
//! These benchmarks measure the **per-update throughput** of each sketch
//! structure. Each sketch's hot path is a single hash + a bounded number of
//! register / counter updates; the throughput is therefore reported in
//! `Elements/sec` (one element = one `add` call).
//!
//! ## Methodology
//!
//! - **HLL**: 100 K distinct 64-bit hashes → `add` then `estimate`. The
//!   `estimate` call is amortized over the 100 K adds (called once per
//!   benchmark iteration, not once per add).
//! - **Count-Min**: 100 K `(key, 1)` updates → `add` then `estimate`. Keys
//!   are drawn from a 10 K universe so heavy hitters emerge (the sketch's
//!   intended workload).
//! - **t-Digest**: 10 K observations → `add` then `quantile(0.5)`. Fewer
//!   items than HLL/CM because t-Digest's `add` is O(log n) (sorted-insert)
//!   rather than O(1); we keep the iteration count smaller to keep total
//!   benchmark wall time reasonable.
//!
//! ## Why the sketch counts matter
//!
//! Sketch throughput determines whether the engine can afford to maintain
//! per-column statistics for the cost model (ADR-023). At >10 M updates/sec
//! on a single core, sketches are cheap enough to maintain on every INSERT;
//! below 1 M/sec the engine would have to sample.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use turbogp::sketch::{count_min::CountMin, hll::HyperLogLog, tdigest::TDigest};

/// HLL: 100 K adds followed by a single `estimate`.
fn bench_hll(c: &mut Criterion) {
    let n = 100_000u64;
    let mut group = c.benchmark_group("hll");
    group.throughput(Throughput::Elements(n));

    // Pre-generate the hashes so the RNG is not on the hot path.
    let hashes: Vec<u64> = (0..n).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).collect();

    group.bench_function("100K_adds_then_estimate", |b| {
        b.iter(|| {
            let mut hll = HyperLogLog::new(14); // m = 16384, RSE ≈ 0.81 %.
            for &h in black_box(&hashes) {
                hll.add(h);
            }
            black_box(hll.estimate());
        });
    });
    group.finish();
}

/// Count-Min: 100 K `(key, 1)` updates on a 10 K-key universe, then
/// `estimate` on the heaviest key.
fn bench_count_min(c: &mut Criterion) {
    let n = 100_000u64;
    let mut group = c.benchmark_group("count_min");
    group.throughput(Throughput::Elements(n));

    // Pre-generate the keys (10 K distinct values, cycled to 100 K updates).
    let keys: Vec<u64> = (0..n).map(|i| i % 10_000).collect();
    // ε = 1/w, δ = 1/2^d. With w=1024, d=5: ε ≈ 1e-3, δ ≈ 3 %.
    // (depth=5, width=1024 are typical "sketch statistics" parameters.)

    group.bench_function("100K_adds_then_estimate", |b| {
        b.iter(|| {
            let mut cm = CountMin::new(5, 1024);
            for &k in black_box(&keys) {
                cm.add(k, 1);
            }
            // Probe the heaviest key (key 0 — it appears 10 times in 100 K).
            black_box(cm.estimate(0));
        });
    });
    group.finish();
}

/// t-Digest: 10 K observations then a median query.
///
/// Smaller N than HLL/CM because `add` is O(log n) (sorted insert +
/// occasional compress). 10 K observations still gives a stable throughput
/// number and runs in well under a second.
fn bench_tdigest(c: &mut Criterion) {
    let n = 10_000u64;
    let mut group = c.benchmark_group("tdigest");
    group.throughput(Throughput::Elements(n));

    // Observations: 0.0, 1.0, 2.0, ..., 9999.0 — uniform on [0, 10_000).
    let values: Vec<f64> = (0..n).map(|i| i as f64).collect();

    group.bench_function("10K_adds_then_quantile", |b| {
        b.iter(|| {
            // max_centroids = 100 keeps the digest small (each `add` is
            // O(log 100) ≈ 7 comparisons on average).
            let mut td = TDigest::new(100);
            for &v in black_box(&values) {
                td.add(v);
            }
            // p50 of a uniform [0, 10_000) stream should be ≈ 5_000.
            black_box(td.quantile(0.5));
        });
    });
    group.finish();
}

criterion_group!(benches, bench_hll, bench_count_min, bench_tdigest);
criterion_main!(benches);
