# Changelog

All notable changes to turboGP are documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased] — v2 Remediation (Waves 0-12)

### Wave 0: Environment Provisioning
- CI: added cargo-audit/deny security workflow
- CI: added deadcode workflow with check_dead_code.sh + check_no_panics.sh
- Repo: waves/ directory with DoD checklists, pre-commit hook installed

### Wave 1: Dead Code Purge (USER PRIORITY)
- **51 files deleted, 16,942 LOC removed** (-27% codebase)
- Deleted: src/executor/ (9 files), src/protocol/ (4 files), src/sketch/ (4 files),
  src/compress/ (2 files)
- Deleted dead modules: 12 planner modules, 3 storage modules, 4 memory modules,
  4 type modules, 6 exec modules, 2 index modules
- Cleaned CostModel: removed learned field, 6 methods, estimate_cost, order_joins
- Added scripts/check_dead_code.sh wired to CI

### Wave 2: Security Hardening
- S1: COPY path allow-list (allowed_copy_dirs field + validation, SQLSTATE 42501)
- S3: Username enumeration eliminated (generic "authentication failed")
- S7: SQLSTATE mapping (Error::sqlstate() method, 12 variants mapped)

### Wave 3: ACID Atomicity + Consistency
- A2: NOT NULL + PRIMARY KEY constraints enforced in execute_insert (SQLSTATE 23502/23505)
- A5: WAL errors propagated via log::error! (no more silent discard)

### Wave 4: ACID Isolation
- RowVersion field added to Table struct for MVCC
- Switched from std::sync::RwLock to parking_lot::RwLock (no poisoning)

### Wave 5: ACID Durability
- A4 (CRITICAL): Checkpoint::load() implemented and wired into with_data_dir
  (fixes data-loss bug where checkpoint.sql was written but never loaded)

### Wave 9: Index Fixes
- I1: Fixed inverted selectivity in should_use_index (< 0.1 = use, not > 0.1)

### Wave 12: Observability
- O6: Slow query logging (queries > 100ms logged at WARN)
- O5: statement_timeout_ms config field (default 30s)

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
