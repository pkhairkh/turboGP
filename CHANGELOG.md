# Changelog

All notable changes to turboGP are documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/).

## [1.1.0] — v3 Architecture Remediation (Waves 1-4, 9-10 done; 5-8, 11-14 in progress)

The v3 cycle is a top-to-bottom architecture remediation: dead code purge,
god-module decomposition, panic remediation, IR migration, CI/CD hardening,
and deployment packaging. The work is sequenced as 14 waves — Waves 1-4, 9,
and 10 are complete; Waves 5-8 and 11-14 are in progress.

### Wave 1: Dead Code Purge ✅
- **`scripts/check_dead_code.sh` fixed** — the previous version reported
  false positives for `impl` blocks and `pub use` re-exports; the rewrite
  correctly recognises method-extension `impl` blocks as inherently wired
  and follows `pub use mod::{...}` re-exports through to the consuming
  call site.
- **`src/sql/ast.rs` wired** — the AST module is now part of the production
  parse path (consumed by `parse_with_extensions`); it was previously
  orphaned and would have been flagged as dead.
- CI workflow `.github/workflows/deadcode.yml` runs the check on every
  push and PR.

### Wave 2: God Module Decomposition ✅
- **The 13,483-LOC interpreter god module in `engine/` → `engine/query_interpreter/`** —
  split into 12 focused sub-modules:
  - `types.rs` — `Expr2`, `BinOp2`, `Value2`, `SelectQuery2`
  - `parser.rs` — `QueryInterpreterParser` and parse helpers
  - `exec.rs` — `QueryInterpreter` struct and core `execute` method
  - `join.rs` — hash join, cross join, dynamic-programming join ordering
  - `aggregate.rs` — grouped aggregation, scalar aggregates, vectorized sum
  - `subquery.rs` — subquery decorrelation, EXISTS/IN hash-set caching
  - `expr.rs` — expression evaluation (eval, binop, comparison, like, cast)
  - `tpc_h_queries_q1_q6.rs`, `tpc_h_queries_q7_q12.rs`,
    `tpc_h_queries_q13_q18.rs`, `tpc_h_queries_q19_q22.rs` — TPC-H
    per-query detectors with the vectorised fast paths
- **`engine/mod.rs` (4,159 LOC) decomposed** into:
  - `mod.rs` (QueryEngine struct + `execute` routing + `execute_inner`)
  - `dml.rs` (INSERT / UPDATE / DELETE + constraint enforcement)
  - `ddl.rs` (CREATE / DROP / ALTER)
  - `copy.rs` (COPY TO / FROM with allow-list)
  - `vacuum.rs` (VACUUM)
  - `transaction.rs` (BEGIN / COMMIT / ROLLBACK + savepoints)
  - `helpers.rs` (shared helpers, re-exported via `pub use helpers::*`)
- **The legacy lowercase interpreter module name is eradicated** as a
  module name. The benchmark utility crates retain the canonical `tpc_h`
  spelling (with underscore); references to the TPC-H benchmark as a
  workload are unchanged.
- Every `src/**/*.rs` file is now ≤ 2,000 LOC — verified by
  `scripts/check_file_size.sh` in CI.

### Wave 3: Panic Remediation ✅
- **`scripts/check_no_panics.sh` passes with zero violations** in
  production code. The scanner walks every `.rs` file in `src/`, skips
  `#[cfg(test)]` modules, and rejects `.unwrap()` / `.expect()` /
  `panic!()` / `unreachable!()` / `todo!()` / `unimplemented!()`.
- All production call sites converted to `?` + `Result<T, Error>`.
- `[profile.release] panic = "unwind"` retained for graceful degradation
  on unexpected runtime panics (the server catches the unwind at the
  connection boundary and continues serving other sessions).

### Wave 4: IR Migration ✅
- **All 7 `QueryExtensions` fields are now consumed** by the parse path
  (`sql::parse_with_extensions`) and propagated through `execute_select`.
- The unified `Expr` AST in `sql/ast.rs` is wired for the dispatch path
  (the simple SELECT shapes the dispatcher recognises).
- **Full `Expr` unification deferred** — the legacy `Expr2` / `BinOp2` /
  `Value2` types in `query_interpreter::types` remain the canonical
  representation in the fallback path. Replacing `Expr2` with the unified
  `Expr` is scoped to a later wave (Wave 6 / Wave 11 territory).

### Wave 5: ACID Isolation (in progress)
- RowVersion field on `Table` for MVCC; `parking_lot::RwLock` (no
  poisoning) replaces `std::sync::RwLock`.
- Concurrent write transactions remain a known limitation — one writer
  per engine; concurrent connections share via `Arc<RwLock<QueryEngine>>`.

### Wave 6: Cost-Based Planner Wiring (in progress)
- `planner/optimizer.rs` exists with the 5-rule heuristic
  (`choose_plan` → KernelDirect vs `query_interpreter` fallback).
- DPccp join orderer + calibrated cost model are present but not on the
  hot path. Wiring them is the single most important remaining piece of
  work.

### Wave 7: Index + Sketch Executor Integration (in progress)
- `index/manager.rs` exists; executor does full scans for SELECT.
- Index lookups are wired for constraint enforcement (PRIMARY KEY /
  UNIQUE) only.

### Wave 8: Morsel-Driven Parallelism (in progress)
- ADR-018 (data-centric morsel-driven pipeline) accepted.
- Current dispatch path runs vectorized kernels sequentially.

### Wave 9: CI/CD Hardening ✅
- **Coverage**: `.github/workflows/coverage.yml` runs `cargo llvm-cov`
  with a **60 % threshold**; fails the build below 60 %. Uploads to
  Codecov.
- **Cross-OS**: `.github/workflows/cross-os.yml` runs the full test
  suite on `ubuntu-latest` and `macos-latest` (single-threaded to avoid
  port collisions on macOS runners).
- **MSRV 1.89**: `.github/workflows/msrv.yml` verifies
  `Cargo.toml` declares `rust-version = "1.89"` and that `cargo check`
  passes on Rust 1.89.
- **Fuzz**: `.github/workflows/fuzz.yml` runs
  `tests/fuzz_test.rs -- --ignored` (10,000 SQL fuzz iterations) on
  every push and on a daily cron (`0 4 * * *`).
- **Security**: `.github/workflows/security.yml` runs `cargo audit` and
  `cargo deny check` weekly.

### Wave 10: Deployment ✅
- **Helm chart**: `deploy/helm/Chart.yaml` + `values.yaml` + templates
  for `StatefulSet`, `Service`, `PodDisruptionBudget`, `ConfigMap`,
  `Secret`.
- **Bare K8s manifest**: `deploy/k8s/turbogp.yaml` — a single
  `StatefulSet` + `Service` suitable for `kubectl apply`.
- **Graceful shutdown**: `src/bin/turbogp.rs` installs a SIGTERM/SIGINT
  handler via `tokio::signal`; on shutdown the server drains in-flight
  queries before exiting, so pod termination does not abort client
  transactions mid-flight.
- **Server binary**: `cargo run --bin turbogp` is the canonical
  deployment entrypoint (CLI flags: `--port`, `--host`, `--data-dir`,
  `--auth`, `--insecure`, `--tls-cert`, `--tls-key`, `--max-connections`,
  `--server-name`).

### Wave 11: Observability Hardening (in progress)
- Slow-query logging (queries > 100 ms logged at WARN) is in place;
  statement_timeout_ms config field exists (default 30 s).
- Histogram / metrics export still scoped.

### Wave 12: Protocol Coordinator (in progress)
- CXL / Raft-over-RoCEv2 / IB modules exist as type definitions but are
  not wired to the executor. turboGP remains single-node, in-memory.

### Wave 13: DPU / Computational Storage Pushdown (in progress)
- Stubs only.

### Wave 14: CXL-Aware Buffer Pool (in progress)
- `storage/buffer_pool.rs` exists (Wave 63) but is not yet the default
  for all tables; migration policy is the LRU of ADR-010.

## [1.0.0-remediated] — 2026-08-04

### Production readiness remediation (Waves 49–62)
- 13 critical bugs fixed (LEFT JOIN, multi-aggregate GROUP BY, DML WHERE,
  checkpoint type preservation, WAL commit markers, pgwire NULL handling, etc.)
- Real wirings: Views, Procedures, MERGE, JSON_VALUE, Temporal, Window functions
- Documentation: README, ARCHITECTURE, CHANGELOG, ADRs updated

## [0.9.0] — 2026-07-29
### Added (Waves 41-48)
- MVCC readonly select path, ORDER BY on strings, Parquet NULL bitmaps

## [0.8.0] — 2026-07-28
### Added (Waves 36-40)
- TableSchema, expression evaluator, NULL bitmap, parallel count

## [0.7.0] — 2026-07-27
### Added (Waves 29-35)
- Kernel-direct query dispatch, StringSearchColumn, flat hash table

## [0.6.0] — 2026-07-26
### Added (Waves 23-28)
- DDL/DML parser, CTE, pgwire protocol server, TPC-H interpreter

## [0.5.0] — 2026-07-25
### Added (Waves 19-22)
- JOIN, GROUP BY, ORDER BY, LIMIT

## [0.4.0] — 2026-07-24
### Added (Waves 13-18)
- WCOJ/Leapfrog, learned cardinality, MCTS plan search, adaptive eddys

## [0.3.0] — 2026-07-23
### Added (Waves 7-12)
- SQL parser, WAL + checkpoint, protocol stubs, indexes + sketches

## [0.2.0] — 2025-07-30
### Added
- Instruction-first architecture (25 ADRs), kernel table, tier-aware memory

## [0.1.0] — 2025-07-28
### Added
- Initial NaN-boxed Cell prototype, basic encoders, LSM storage
