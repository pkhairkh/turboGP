# turboGP Architecture Refactor Plan

**Created:** 2026-08-14
**Status:** Active — Waves 28-33

## Problem Statement

The codebase has two systemic issues:

1. **Benchmark gluecode pollution**: 6 Python scripts at repo root (1932 LOC), TPC-H-specific schema hardcoding (`tpc_h_schema`, `tpc_h_col_types`, `is_tpch_float_column`) embedded in the generic datasource and engine layers, and benchmark query files mixed with engine code.

2. **No domain boundaries**: The engine is a monolith — `mod.rs` (2242 LOC), `dispatch.rs` (2081), `executor.rs` (1966), `query_interpreter/expr.rs` (2338). The query interpreter (the actual hot path) is tangled with the legacy executor. Catalog, execution, planning, and storage concerns are interleaved.

3. **Performance gap**: TPC-H 7.11x slower than DuckDB, ClickBench 1.43x slower. The gap is concentrated in join-heavy queries (Q18: 39x, Q21: 27x, Q13: 23x, Q5: 29x).

## Target Architecture (DDD Bounded Contexts)

```
src/
├── catalog/          # Domain: table metadata, schema registry
├── datasource/       # Domain: data loading (CSV, Parquet)
│   ├── csv.rs        # Generic CSV reader (no TPC-H hardcoding)
│   └── schema.rs     # Data-driven schema inference (replaces tpc_h_schema)
├── execution/        # Domain: query execution
│   ├── interpreter/  # The hot path (query_interpreter/ renamed)
│   │   ├── exec.rs
│   │   ├── join.rs
│   │   ├── expr.rs
│   │   ├── aggregate.rs
│   │   └── ...
│   ├── executor.rs   # Legacy executor (to be deprecated)
│   └── dispatch.rs   # Statement dispatch
├── planning/         # Domain: query planning, optimization
├── storage/          # Domain: persistence, WAL, replication
├── server/           # Domain: pgwire protocol, connection handling
├── txn/              # Domain: transaction management
└── sql/              # Domain: SQL parsing (lexer, parser, AST)
```

## Wave Plan

### Wave 28: Architecture Plan (this document)
- Document current state and target architecture
- No code changes

### Wave 29: Benchmark Gluecode Cleanup
- Move 6 root Python scripts to `bench/scripts/`
- Consolidate bench_all.py, gen_comparison.py, generate_report.py
- Move benchmark query files to `bench/queries/`
- Clean root directory

### Wave 30: Remove TPC-H Hardcoding from Engine
- Replace `tpc_h_schema()` with data-driven schema inference
- Replace `tpc_h_col_types()` with schema-based type lookup
- Remove `is_tpch_float_column()` — use schema instead
- Move TPC-H schema definitions to `bench/tpch_schema.rs`

### Wave 31: TPC-H Performance — Join Optimization
- Profile slowest queries (Q18, Q21, Q13, Q5)
- Optimize join execution (bloom filter, hash table sizing)
- Optimize GROUP BY after join

### Wave 32: ClickBench Performance
- Profile Q27, Q04, Q12, Q17
- Optimize string aggregation and GROUP BY

### Wave 33: Final Verification
- Re-run all benchmarks
- Regenerate comparison report
