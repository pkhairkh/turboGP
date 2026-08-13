//! Kernel throughput benchmarks — direct invocation of the kernel table's
//! scan / aggregate / similarity / hash operators.
//!
//! These benchmarks drive each kernel via the [`Scheduler`] so the hot path
//! matches what a real query takes: region lock → kernel select → execute.
//! Throughput is reported in `Elements/sec` (cells/sec), the canonical unit
//! for vectorized scan kernels (ADR-023).
//!
//! ## What is measured
//!
//! - `scan_eq`  — equality scan, `Operator::ScanEqU64`, 1 M u64 cells.
//! - `scan_range` — range scan, `Operator::ScanRangeU64`, 1 M u64 cells.
//! - `sum_f64` — vectorized aggregate, `Operator::AggregateSumF64`, 1 M f64 cells.
//! - `hamming` — Hamming-distance similarity, `Operator::SimilarityHamming`, 1 M cells.
//! - `hash_probe` — hash table probe, `Operator::HashProbe`, 1 M probe keys.
//!
//! ## What is NOT measured
//!
//! - Allocation: input regions are pre-built once outside the timed closure.
//! - NUMA effects: the benchmark thread runs on whatever core the OS assigns.
//!   For NUMA-pinned measurements, run under `numactl --cpunodebind=0 --membind=0`.
//! - Energy: see ADR-022 and `docs/benchmark-results.md`.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use turbogp::{
    executor::Scheduler,
    kernel::KernelTable,
    memory::{region::Region, tier::MemoryTier},
};

/// Cell count for the 1 M-cell benchmark group.
const CELL_COUNT: usize = 1_000_000;

/// Build a u64 region with `i % 100` (so equality / range predicates match
/// a known fraction of the cells) on the given tier.
fn u64_region(id: u64, tier: MemoryTier) -> Arc<Region> {
    let mut bytes = vec![0u8; CELL_COUNT * 8];
    for i in 0..CELL_COUNT {
        let v = (i % 100) as u64;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
    }
    Arc::new(Region::from_bytes(id, tier, &bytes))
}

/// Build an f64 region with `i + 1.0` as f64-bits on the given tier.
fn f64_region(id: u64, tier: MemoryTier) -> Arc<Region> {
    let mut bytes = vec![0u8; CELL_COUNT * 8];
    for i in 0..CELL_COUNT {
        let v = (i as f64) + 1.0;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    Arc::new(Region::from_bytes(id, tier, &bytes))
}

/// `scan_eq` throughput — counts cells equal to 42 (1 % of 1 M = 10 000 hits).
fn bench_scan_eq(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_eq");
    group.throughput(Throughput::Elements(CELL_COUNT as u64));

    let region = u64_region(0, MemoryTier::L3);
    let sched = Scheduler::new(Arc::new(KernelTable::new()));
    sched.register_region(region);

    group.bench_function("1M_cells", |b| {
        b.iter(|| black_box(sched.scan_eq(black_box(0), black_box(42)).unwrap()));
    });
    group.finish();
}

/// `scan_range` throughput — counts cells in [10, 20] (11 % of 1 M = 110 000).
fn bench_scan_range(c: &mut Criterion) {
    use turbogp::executor::plan::KernelInvocation;
    use turbogp::kernel::{KernelParams, Operator};

    let mut group = c.benchmark_group("scan_range");
    group.throughput(Throughput::Elements(CELL_COUNT as u64));

    let region = u64_region(0, MemoryTier::L3);
    let sched = Scheduler::new(Arc::new(KernelTable::new()));
    sched.register_region(region);

    // Build the invocation once and re-execute it each iteration. This
    // mirrors how a real query runs: the plan lowerer produces a static
    // `KernelInvocation` list, and the scheduler dispatches them.
    let inv = KernelInvocation {
        operator: Operator::ScanRangeU64,
        tier: MemoryTier::L3,
        region_id: 0,
        params: KernelParams {
            low_u64: 10,
            high_u64: 20,
            cell_count: CELL_COUNT,
            ..Default::default()
        },
    };
    group.bench_function("1M_cells", |b| {
        b.iter(|| black_box(sched.execute_invocation(black_box(&inv)).unwrap()));
    });
    group.finish();
}

/// `sum_f64` throughput — sums 1 M f64 cells treated as bits.
fn bench_sum_f64(c: &mut Criterion) {
    let mut group = c.benchmark_group("sum_f64");
    group.throughput(Throughput::Elements(CELL_COUNT as u64));

    let region = f64_region(0, MemoryTier::L3);
    let sched = Scheduler::new(Arc::new(KernelTable::new()));
    sched.register_region(region);

    group.bench_function("1M_cells", |b| {
        b.iter(|| black_box(sched.sum_f64(black_box(0)).unwrap()));
    });
    group.finish();
}

/// `hamming` (count_similar) throughput — counts cells within Hamming distance
/// 0 of the target. All cells `i % 100` differ from the target by ≥ 1 bit, so
/// only exact matches count.
fn bench_hamming(c: &mut Criterion) {
    let mut group = c.benchmark_group("hamming");
    group.throughput(Throughput::Elements(CELL_COUNT as u64));

    let region = u64_region(0, MemoryTier::L3);
    let sched = Scheduler::new(Arc::new(KernelTable::new()));
    sched.register_region(region);

    group.bench_function("1M_cells", |b| {
        b.iter(|| {
            black_box(sched.count_similar(black_box(0), black_box(42), black_box(8)).unwrap())
        });
    });
    group.finish();
}

/// `hash_probe` throughput — builds a hash table from 1 M keys and probes
/// 1 M of the same keys (every probe hits).
fn bench_hash_probe(c: &mut Criterion) {
    use turbogp::kernel::hash::HashTable;

    let mut group = c.benchmark_group("hash_probe");
    group.throughput(Throughput::Elements(CELL_COUNT as u64));

    // Pre-build the hash table outside the timed closure.
    let keys: Vec<u64> = (0..CELL_COUNT as u64).map(|i| i % 100_000).collect();
    let table = HashTable::build(&keys);

    let probe_keys: Vec<u64> = keys.clone();
    group.bench_function("1M_cells", |b| {
        b.iter(|| {
            let mut hits = 0u64;
            for &k in black_box(&probe_keys) {
                hits += table.probe(k).len() as u64;
            }
            black_box(hits);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_scan_eq,
    bench_scan_range,
    bench_sum_f64,
    bench_hamming,
    bench_hash_probe,
);
criterion_main!(benches);
