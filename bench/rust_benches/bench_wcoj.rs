//! WCOJ (Leapfrog triejoin) vs binary hash join benchmark.
//!
//! This benchmark compares the worst-case optimal leapfrog join
//! ([`turbogp::kernel::leapfrog::LeapfrogJoin`]) against a cascade of binary
//! hash joins ([`turbogp::kernel::hash::HashTable`]) on three workloads:
//!
//! 1. **Triangle query (cyclic, 3-way intersection)** — where WCOJ should be
//!    much faster. Leapfrog runs in `O(IN + OUT + AGM)`; a binary hash-join
//!    cascade runs in `O(|R| · |S| + (|R∩S|) · |T|)`, which is asymptotically
//!    worse on cyclic queries.
//! 2. **Path query (acyclic, 2-way intersection)** — where hash join should
//!    be comparable. Both algorithms run in `O(|R| + |S|)` on a 2-way
//!    intersection with uniform data; the hash join's tight inner loop
//!    (SwissTable `VPCMPEQB` metadata scan) typically wins by a constant
//!    factor.
//! 3. **Skewed triangle (power-law distribution)** — where WCOJ shines
//!    most. The AGM bound is much smaller than the product when a few hot
//!    keys dominate; leapfrog's seek-based traversal skips cold keys in
//!    `O(log |R|)` per seek, while a hash join cascade must probe every
//!    duplicate.
//!
//! ## Workload model
//!
//! Each workload models the join as a **multi-way intersection of u64 key
//! sets** — the canonical case for [`LeapfrogJoin`], which intersects N
//! sorted iterators on a single attribute. The hash-join baseline
//! implements the same intersection via `N-1` sequential
//! [`HashTable::probe`] passes (build set 1, probe with set 2 → result_12;
//! build result_12, probe with set 3 → final).
//!
//! This single-attribute abstraction captures the algorithmic essence of
//! the cyclic-vs-acyclic distinction: leapfrog's `O(AGM)` runtime vs the
//! hash cascade's `O(∏ |Ri|)` runtime. A full multi-attribute leapfrog
//! triejoin (which iterates over one attribute at a time, intersecting
//! per-attribute iterators) is left to a future wave.
//!
//! ## Throughput
//!
//! Throughput is reported in `Elements/sec`, where "elements" is the total
//! input size (`Σ |Ri|`). The leapfrog kernel reads every input key once
//! (plus seeks), so `Σ |Ri|` is the right denominator; the hash cascade
//! also reads every input key once (build + probe), so the metric is
//! directly comparable between the two algorithms.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use turbogp::kernel::hash::HashTable;
use turbogp::kernel::leapfrog::{LeapfrogJoin, SliceSortedIterator, SortedIterator};

/// Number of keys per relation in the uniform workloads.
const N: usize = 100_000;

/// Number of keys per relation in the skewed workload (smaller, because
/// the power-law distribution creates many duplicates that blow up the hash
/// cascade's runtime).
const N_SKEW: usize = 20_000;

/// Build a sorted, deduped `Vec<u64>` of `n` keys drawn uniformly from
/// `[0, 4*N)` — so adjacent sets overlap by ~25 %.
fn uniform_keys(n: usize, seed: u64) -> Vec<u64> {
    let mut rng = splitmix64(seed);
    let mut keys: Vec<u64> = (0..n).map(|_| next_u64(&mut rng) % (4 * n as u64)).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Build a sorted, deduped `Vec<u64>` of `n` keys drawn from a power-law
/// distribution: keys are `i^2 % domain` so a few hot keys appear many
/// times before dedup leaves only the unique set, which still has the
/// hot-key distribution at the value level (the probe of a hot key hits
/// many build rows).
fn power_law_keys(n: usize, seed: u64) -> Vec<u64> {
    let mut rng = splitmix64(seed);
    let domain = (n as u64) / 4; // small domain ⇒ many duplicates
    let mut keys: Vec<u64> = (0..n)
        .map(|i| {
            // Mix uniform noise with a quadratic hot-key term so some
            // values dominate.
            let noise = next_u64(&mut rng) % domain;
            let hot = ((i as u64) * (i as u64)) % domain;
            hot ^ noise
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// A deterministic splitmix64 PRNG — used to make the benchmark
/// reproducible across runs (no `thread_rng`).
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = z;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Step a splitmix64 state forward and return the next `u64`.
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Leak a slice to get a `'static` reference, so it can be wrapped in a
/// `Box<dyn SortedIterator + 'static>`.
///
/// Called **once per relation per benchmark** (at setup time, outside the
/// timed closure), so the total leaked memory is bounded by
/// `Σ |Ri| × 8 bytes` — a few MB per benchmark. The leak is intentional:
/// the `SortedIterator` trait object needs to outlive the `LeapfrogJoin`,
/// and the simplest way to express that without restructuring the trait
/// (to take an iterator-by-value) is to use `'static` slices.
fn leak_keys(keys: &[u64]) -> &'static [u64] {
    Box::leak(keys.to_vec().into_boxed_slice())
}

/// Build a `Box<dyn SortedIterator>` over a `'static` slice.
fn static_iter(keys: &'static [u64]) -> Box<dyn SortedIterator> {
    Box::new(SliceSortedIterator::at_start(keys))
}

// ---------------------------------------------------------------------------
// Benchmark 1: cyclic triangle query (3-way intersection)
// ---------------------------------------------------------------------------

/// Triangle query — 3-way intersection of three sets of ~100K u64 keys.
///
/// WCOJ (Leapfrog) intersects all three at once in `O(IN + OUT + AGM)`.
/// Hash join builds a table from set1, probes with set2 to get
/// `result_12 = set1 ∩ set2`, then builds a table from `result_12` and
/// probes with set3.
fn bench_triangle_leapfrog(c: &mut Criterion) {
    let mut group = c.benchmark_group("triangle_leapfrog");
    let total_keys = 3 * N;
    group.throughput(Throughput::Elements(total_keys as u64));

    // Leak once at setup so the timed closure does not allocate.
    let r = leak_keys(&uniform_keys(N, 1));
    let s = leak_keys(&uniform_keys(N, 2));
    let t = leak_keys(&uniform_keys(N, 3));

    group.bench_function("3way_intersection", |b| {
        b.iter(|| {
            let mut join = LeapfrogJoin::new(vec![
                static_iter(black_box(r)),
                static_iter(black_box(s)),
                static_iter(black_box(t)),
            ]);
            let out = join.run();
            black_box(out.len());
        });
    });
    group.finish();
}

/// Triangle query — hash-join baseline (two sequential hash joins).
fn bench_triangle_hash_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("triangle_hash_join");
    let total_keys = 3 * N;
    group.throughput(Throughput::Elements(total_keys as u64));

    let r = uniform_keys(N, 1);
    let s = uniform_keys(N, 2);
    let t = uniform_keys(N, 3);

    group.bench_function("2pass_hash_join", |b| {
        b.iter(|| {
            // Pass 1: build hash table from R, probe with S → R∩S.
            let table_r = HashTable::build(black_box(&r));
            let mut r_intersect_s: Vec<u64> = Vec::new();
            for &k in black_box(&s) {
                if !table_r.probe(k).is_empty() {
                    r_intersect_s.push(k);
                }
            }
            // Pass 2: build hash table from R∩S, probe with T → final.
            let table_rs = HashTable::build(black_box(&r_intersect_s));
            let mut matches = 0u64;
            for &k in black_box(&t) {
                matches += table_rs.probe(k).len() as u64;
            }
            black_box(matches);
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: acyclic path query (2-way intersection)
// ---------------------------------------------------------------------------

/// Path query — 2-way intersection. Leapfrog and hash join are both
/// `O(|R| + |S|)` here; the hash join's tight inner loop typically wins.
fn bench_path_leapfrog(c: &mut Criterion) {
    let mut group = c.benchmark_group("path_leapfrog");
    let total_keys = 2 * N;
    group.throughput(Throughput::Elements(total_keys as u64));

    let r = leak_keys(&uniform_keys(N, 4));
    let s = leak_keys(&uniform_keys(N, 5));

    group.bench_function("2way_intersection", |b| {
        b.iter(|| {
            let mut join =
                LeapfrogJoin::new(vec![static_iter(black_box(r)), static_iter(black_box(s))]);
            let out = join.run();
            black_box(out.len());
        });
    });
    group.finish();
}

/// Path query — hash-join baseline (single hash join).
fn bench_path_hash_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("path_hash_join");
    let total_keys = 2 * N;
    group.throughput(Throughput::Elements(total_keys as u64));

    let r = uniform_keys(N, 4);
    let s = uniform_keys(N, 5);

    group.bench_function("1pass_hash_join", |b| {
        b.iter(|| {
            let table_r = HashTable::build(black_box(&r));
            let mut matches = 0u64;
            for &k in black_box(&s) {
                matches += table_r.probe(k).len() as u64;
            }
            black_box(matches);
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: skewed triangle (power-law distribution)
// ---------------------------------------------------------------------------

/// Skewed triangle — same as the triangle benchmark but with power-law
/// distributed keys. A few hot keys appear in all three sets; leapfrog
/// seeks past cold keys in `O(log |R|)` per seek, while the hash cascade
/// must probe every duplicate.
fn bench_skewed_triangle_leapfrog(c: &mut Criterion) {
    let mut group = c.benchmark_group("skewed_triangle_leapfrog");
    let total_keys = 3 * N_SKEW;
    group.throughput(Throughput::Elements(total_keys as u64));

    let r = leak_keys(&power_law_keys(N_SKEW, 10));
    let s = leak_keys(&power_law_keys(N_SKEW, 11));
    let t = leak_keys(&power_law_keys(N_SKEW, 12));

    group.bench_function("3way_intersection_skewed", |b| {
        b.iter(|| {
            let mut join = LeapfrogJoin::new(vec![
                static_iter(black_box(r)),
                static_iter(black_box(s)),
                static_iter(black_box(t)),
            ]);
            let out = join.run();
            black_box(out.len());
        });
    });
    group.finish();
}

/// Skewed triangle — hash-join baseline.
fn bench_skewed_triangle_hash_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("skewed_triangle_hash_join");
    let total_keys = 3 * N_SKEW;
    group.throughput(Throughput::Elements(total_keys as u64));

    let r = power_law_keys(N_SKEW, 10);
    let s = power_law_keys(N_SKEW, 11);
    let t = power_law_keys(N_SKEW, 12);

    group.bench_function("2pass_hash_join_skewed", |b| {
        b.iter(|| {
            let table_r = HashTable::build(black_box(&r));
            let mut r_intersect_s: Vec<u64> = Vec::new();
            for &k in black_box(&s) {
                if !table_r.probe(k).is_empty() {
                    r_intersect_s.push(k);
                }
            }
            let table_rs = HashTable::build(black_box(&r_intersect_s));
            let mut matches = 0u64;
            for &k in black_box(&t) {
                matches += table_rs.probe(k).len() as u64;
            }
            black_box(matches);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_triangle_leapfrog,
    bench_triangle_hash_join,
    bench_path_leapfrog,
    bench_path_hash_join,
    bench_skewed_triangle_leapfrog,
    bench_skewed_triangle_hash_join,
);
criterion_main!(benches);
