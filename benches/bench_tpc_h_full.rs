//! Full TPC-H benchmark: turboGP kernel throughput on TPC-H-style queries.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn generate_lineitem(n: usize) -> Table {
    let mut cols = vec![vec![], vec![], vec![], vec![]];
    let names = vec![
        "l_quantity".to_string(),
        "l_extendedprice".to_string(),
        "l_discount".to_string(),
        "l_shipdate".to_string(),
    ];
    for i in 0..n {
        cols[0].push((i % 50) as u64); // l_quantity 0-49
        cols[1].push((i * 100) as u64); // l_extendedprice
        cols[2].push((i % 10) as u64); // l_discount 0-9
        cols[3].push((i % 365) as u64); // l_shipdate
    }
    Table { name: "lineitem".into(), columns: cols, column_names: names, row_count: n }
}

fn bench_tpc_h(c: &mut Criterion) {
    let mut group = c.benchmark_group("tpc_h_full");

    // Q1: count with equality filter
    {
        let mut engine = QueryEngine::new();
        engine.register_table(generate_lineitem(100_000));
        group.throughput(Throughput::Elements(100_000));
        group.bench_function("q1_count_eq", |b| {
            b.iter(|| {
                let r = engine
                    .execute(black_box("SELECT count(*) FROM lineitem WHERE l_quantity = 10"))
                    .unwrap();
                black_box(r);
            });
        });
    }

    // Q6 variant: sum
    {
        let mut engine = QueryEngine::new();
        engine.register_table(generate_lineitem(100_000));
        group.bench_function("q6_sum", |b| {
            b.iter(|| {
                let r = engine.execute(black_box("SELECT sum(l_quantity) FROM lineitem")).unwrap();
                black_box(r);
            });
        });
    }

    // Full scan: count all
    {
        let mut engine = QueryEngine::new();
        engine.register_table(generate_lineitem(100_000));
        group.bench_function("full_scan_count", |b| {
            b.iter(|| {
                let r = engine.execute(black_box("SELECT count(*) FROM lineitem")).unwrap();
                black_box(r);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_tpc_h);
criterion_main!(benches);
