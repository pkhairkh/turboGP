//! Throughput benchmarks for the instruction-first engine.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use turbogp::{
    executor::Scheduler,
    kernel::KernelTable,
    memory::{region::Region, tier::MemoryTier},
};

fn bench_scan_eq(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_eq");
    group.throughput(Throughput::Elements(1_000_000));

    let cell_count = 1_000_000;
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i % 1000) as u64;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
    }

    for tier in [MemoryTier::L3, MemoryTier::Ddr5, MemoryTier::Cxl] {
        let table = Arc::new(KernelTable::new());
        let sched = Scheduler::new(table);
        let region = Arc::new(Region::from_bytes(0, tier, &bytes));
        sched.register_region(region);

        group.bench_function(format!("tier={}", tier.name()), |b| {
            b.iter(|| black_box(sched.scan_eq(black_box(0), black_box(42)).unwrap()));
        });
    }
    group.finish();
}

fn bench_sum_f64(c: &mut Criterion) {
    let mut group = c.benchmark_group("sum_f64");
    group.throughput(Throughput::Elements(1_000_000));

    let cell_count = 1_000_000;
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i as f64) + 1.0;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }

    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    sched.register_region(region);

    group.bench_function("1M_f64", |b| {
        b.iter(|| black_box(sched.sum_f64(black_box(0)).unwrap()));
    });
    group.finish();
}

fn bench_count_similar(c: &mut Criterion) {
    let mut group = c.benchmark_group("count_similar");
    group.throughput(Throughput::Elements(1_000_000));

    let cell_count = 1_000_000;
    let bytes = vec![0u8; cell_count * 8]; // all zeros

    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    sched.register_region(region);

    group.bench_function("hamming_le_8", |b| {
        b.iter(|| {
            black_box(sched.count_similar(black_box(0), black_box(0), black_box(8)).unwrap())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_scan_eq, bench_sum_f64, bench_count_similar);
criterion_main!(benches);
