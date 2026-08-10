# Changelog

All notable changes to turboGP are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### turboGP remediation plan (Waves 0–11)

The [Unreleased] section now collects the waves executed under the turboGP
remediation plan (see `waves/`). Earlier waves (12 onward) were captured
under [1.0.0-remediated] below; waves 0–11 are listed here for the first
time so the changelog matches the actual sequencing.

#### Wave 0 — Environment provisioning
- Added CI security workflow (`.github/workflows/`) gating PRs on clippy,
  rustfmt, and `cargo audit`.
- Added pre-commit hook (`scripts/check_no_panics.sh`) that fails if any
  production code introduces a new `unwrap`/`expect`/`panic!` outside
  `#[cfg(test)]` modules.
- Scaffolded the `waves/` directory with one markdown file per wave
  (`wave-00.md` ... `wave-12.md`) for orchestration traceability.

#### Wave 1 — Security hardening
- Fixed SQL injection in `sql/parser.rs` and `engine/executor.rs`: parameter
  values are now parsed as literals and never interpolated into a SQL
  string. `tests/sql_injection_test.rs` covers the regression.
- Fixed `COPY ... TO`/`FROM` escaping: paths are now validated and escaped
  against shell metacharacters in `engine/dispatch.rs`.
- Auth is now on by default (`server::ServerConfig::auth_enabled` defaults
  to `true`); `auth_required = false` is opt-in only.
- Added a per-session connection limit (`ServerConfig::max_connections`)
  to prevent trivial DoS via connection flooding.

#### Wave 2 — Persistence
- WAL is now on by default (`StorageConfig::wal_enabled = true`) — durability
  no longer requires opt-in.
- Added periodic `VACUUM`-style checkpoint that flushes the WAL and
  truncates the log; configurable via `StorageConfig::checkpoint_interval`.
- Switched the WAL frame checksum from CRC32 to **xxh3** (3–5× faster on
  Zen 5, matching the ADR-012 page-checksum throughput profile).

#### Wave 3 — Unified AST module
- Introduced `src/sql/ast.rs` as the single source of truth for the SQL AST.
- `Expr` (sql/parser.rs) and `Expr2` (engine/tpch.rs) now both re-export
  from the unified type, eliminating the dual-parser/dual-interpreter
  divergence noted in the Production Readiness Assessment.

#### Wave 6 — Robustness
- Fixed `BufferPool::acquire` panic on stale page id: now returns
  `Error::NotFound` instead of `panic!`.
- Fixed `agm_bound` panic when the hypergraph has no edges: returns 0
  (trivial cover) instead of dividing by zero.
- `Server::new` now generates a cryptographically random backend secret
  key per process (`rand::thread_rng`) instead of using a hard-coded
  constant.

#### Wave 11 — Deployment
- Added binary crate `src/bin/turbogp.rs` so the engine ships as a single
  `turbogp` executable (was library-only).
- Added `Dockerfile` (multi-stage, distroless final image) and
  `docker-compose.yml` (single-service + healthcheck) for one-command
  deployment.
- Added release CI workflow (`.github/workflows/release.yml`) that builds
  cross-platform binaries on tag push and attaches them to the GitHub
  release.

## [1.0.0-remediated] — 2026-08-04

### Production readiness remediation (Waves 49–62)

#### Wave 62 fixes (audit-response)
- **HAVING parser bug fixed**: the basic parser's `parse_primary` didn't
  handle `IDENT (` as a function call in expression context, so
  `HAVING count(*) > N` errored with "unexpected trailing token: LParen".
  Added `Expr::Function` variant and function-call parsing in `parse_primary`.
  HAVING queries still execute through tpch (the basic executor can't
  evaluate aggregates in HAVING), but the parser now SUCCEEDS instead of
  erroring — the difference is explicit routing vs error-fallback.
- **Dead code removed**: `eval_case_row` at `dispatch.rs:1462` was added in
  Wave 60a but never called (CASE WHEN goes to tpch). Removed.
- **CASE WHEN documentation corrected**: Wave 60a claimed CASE WHEN goes
  through "the fast dispatch path". It doesn't — `classify_query` marks
  `SelectItem::Expression` as `Complex`, and `execute_dispatched` returns
  None for CASE in WHERE. CASE WHEN goes through the tpch interpreter.
  The basic parser PARSES it correctly; the basic executor does NOT execute it.
- **ORCHESTRATION.md test count corrected**: was "1331+ / 26 files",
  now "1342 / 31 files" (the actual count).

#### Fixed (13 critical bugs)
- **LEFT JOIN** silently executed as INNER JOIN (Wave 49)
- **Multi-aggregate GROUP BY** dropped all but the first aggregate (Wave 49)
- **SelectMulti + ORDER BY** returned rows in scan order (Wave 49)
- **DML WHERE** only supported `=`; now supports `!=`, `<>`, `<`, `>`, `<=`, `>=` (Wave 50)
- **DML WHERE** broke on strings with spaces; now uses the SQL lexer (Wave 50)
- **UPDATE** didn't update the NULL bitmap; COUNT(col) now excludes NULLed rows (Wave 50)
- **Checkpoint** was type-destructive; now preserves FLOAT/VARCHAR/NULL (Wave 50)
- **WAL** had no commit markers; BEGIN/COMMIT/ROLLBACK now write proper records (Wave 51)
- **WAL** appended before execute; now appends after successful execute (Wave 51)
- **WAL** string escaping was ambiguous; now uses base64 (Wave 51)
- **pgwire** sent NULL values as "0"; now sends -1 length (Wave 52)
- **pgwire** Describe executed the query; now returns NoData without executing (Wave 52)
- **pgwire** max_rows was discarded; now honours cursor-style Execute (Wave 52)

#### Fixed (Wave 56–58: fake/stub wirings and dead code)
- **MERGE** (Wave 56a): `parse_merge` hardcoded `source_rows = Vec::new()` — the
  WHEN MATCHED branch was dead code. Now parses `USING (VALUES ...) AS source(cols)`
  and resolves `source.col` references in INSERT/UPDATE actions.
- **PIVOT** (Wave 56b): `extensions_pivot()` always returned None — dead code.
  Now detects `PIVOT (...)` in the SQL string, strips it, executes the underlying
  SELECT, and applies `pivot::pivot()` to the result.
- **JSON_VALUE** (Wave 56c): not parsed by any executor. Now intercepts
  `JSON_VALUE(col, 'path')` / `JSON_QUERY(col, 'path')` in the SQL, rewrites
  to `col`, executes, and post-processes with `json::json_value()`. Also fixed
  `execute_insert` to preserve original strings in the `string_columns` sidecar
  for VARCHAR/NVARCHAR/TEXT columns (previously strings were hashed and lost).
- **Temporal DDL** (Wave 56d): temporal tables only worked via the Rust API.
  Now `CREATE TABLE ... WITH (SYSTEM_VERSIONING = ON)` registers the table in
  `self.temporals`, and INSERT/UPDATE/DELETE sync to the TemporalTable sidecar.
- **CASE WHEN panic** (Wave 57): `tpch.rs:3584` panicked with index-out-of-bounds
  because `tpch_col_types()` returned an empty Vec for user-created tables.
  Fixed `ExecTable::from_catalog` to infer types from the table's schema.
- **Dead code** (Wave 58a): removed unused `hash_group_by_flat`, `AggFunc`
  imports and `first_agg` variable from `execute_group_by`.
- **Lying test** (Wave 58b): `subquery_in_where` tested `IN (1, 2)` not a real
  subquery. Renamed to `in_list_in_where` and added a real `subquery_in_where`
  test using `IN (SELECT ...)`.

#### Added (real wirings through engine.execute())
- **Views**: CREATE VIEW / DROP VIEW / SELECT FROM view (materialization) (Wave 53)
- **Procedures**: CREATE PROCEDURE / EXEC with positional params (Wave 53)
- **MERGE**: MERGE INTO ... USING (VALUES ...) ... WHEN MATCHED / NOT MATCHED (Wave 56a)
- **JSON_VALUE / JSON_QUERY**: SELECT JSON_VALUE(col, '$.path') FROM t (Wave 56c)
- **Temporal**: CREATE TABLE ... WITH (SYSTEM_VERSIONING = ON) + FOR SYSTEM_TIME AS OF (Wave 56d)
- **Window functions**: ROW_NUMBER, RANK, DENSE_RANK, SUM, COUNT with OVER (...) (Wave 53)
- **PIVOT**: SELECT * FROM t PIVOT (SUM(amt) FOR qtr IN (...)) AS p (Wave 56b)
- **Parquet NULL test**: real Parquet file with NULLs, count(col) excludes NULLs (Wave 58c)
- **Concurrency test**: 2 TCP clients, concurrent SELECT + INSERT, no deadlock (Wave 58d)
- **CASE WHEN**: works through engine.execute() via tpch fallback (Wave 57).
  The basic parser PARSES `Expr::Case` correctly (Wave 60a), but the basic
  executor does NOT evaluate it — `classify_query` marks `Expression` items
  as `Complex`, and `execute_dispatched` returns None for CASE in WHERE.
  CASE WHEN queries execute through the tpch interpreter, not the fast
  dispatch path.
- **Real subquery**: IN (SELECT ...) works through engine.execute() (Wave 58b)

#### Documentation
- README.md: fixed repo layout, updated research agenda, added "Current SQL Surface" and "Known Limitations" sections
- README.md: fixed license — now consistently CCL-X-1.2 across README, Cargo.toml, LICENSE.md (Wave 59a)
- ARCHITECTURE.md: replaced DAG executor description with dispatch-based flow, added CXL/RoCEv2 stub warnings
- ORCHESTRATION.md: added waves 19-61, updated test count to actual (Wave 59d)
- ROADMAP.md: updated feature table with actual implementation status
- CHANGELOG.md: added v0.3.0 through v1.0.0-remediated entries
- CONTRIBUTING.md: fixed test count, updated build instructions
- ADRs: added status notes to ADR-011 (ZNS WAL is not production WAL), ADR-018 (morsel executor not used), ADR-019 (DPccp not wired)
- Module doc comments: marked 7 dead modules with "NOT WIRED INTO SQL EXECUTION" notices (Wave 59c: corrected count from 10 to 7 — the previous count was inflated)
- Cargo.toml: bumped version from 0.2.0 to 1.0.0 to match CHANGELOG (Wave 59b)

## [0.9.0] — 2026-07-29

### Added (Waves 41-48)
- MVCC readonly select path (`try_readonly_select`)
- ORDER BY on string columns via StringSearchColumn sidecar
- Parquet loader populates NULL bitmaps
- Type OID threaded through ResultColumn to pgwire
- Dispatch-path arithmetic in aggregates (SUM(price * 2))
- Typed expression evaluator (mixed int/float)

## [0.8.0] — 2026-07-28

### Added (Waves 36-40)
- TableSchema preserving column types from DDL
- Expression evaluator for arithmetic in aggregate args
- NULL bitmap in dispatch path (COUNT(col) excludes NULLs)
- String range predicates on StringSearchColumn
- Parallel count for large tables

## [0.7.0] — 2026-07-27

### Added (Waves 29-35)
- Kernel-direct query dispatch (classify_query → QueryShape → kernel)
- StringSearchColumn sidecar for string columns
- NULL bitmap support in Table and dispatch
- Flat hash table for GROUP BY
- Vectorized filter / sum / avg / min / max / count_distinct kernels

## [0.6.0] — 2026-07-26

### Added (Waves 23-28)
- DDL parser (CREATE TABLE, DROP TABLE, CREATE SCHEMA)
- DML parser (INSERT, UPDATE, DELETE)
- CTE parser (WITH ... AS (...) SELECT ...)
- pgwire protocol server (simple query + extended query)
- TPC-H interpreter fallback (CASE WHEN, HAVING, subqueries, multi-table joins)

## [0.5.0] — 2026-07-25

### Added (Waves 19-22)
- JOIN support in the basic parser (INNER, LEFT)
- GROUP BY with single-key and multi-key paths
- ORDER BY with ascending/descending
- LIMIT clause

## [0.4.0] — 2026-07-24

### Added (Waves 13-18)
- WCOJ / Leapfrog triejoin
- Learned cardinality estimator
- MCTS plan search for n>15 joins
- Adaptive eddies
- Tensor-network contraction for join planning
- 3× proof benchmark

## [0.3.0] — 2026-07-23

### Added (Waves 7-12)
- SQL parser (SELECT, FROM, WHERE, GROUP BY, ORDER BY, LIMIT)
- WAL + checkpoint for durability
- Protocol coordinator stubs (HLC, CXL, Raft)
- Indexes + sketches (BSI, LSH, HLL, Count-Min, t-Digest)
- DPccp join ordering
- TPC-H and TPC-C benchmark harness

## [0.2.0] — 2025-07-30

### Added
- Instruction-first, memory-centric architecture (25 ADRs)
- Kernel table with 16 AVX-512/AVX2/scalar kernels
- Tier-aware memory manager (8 tiers, NUMA detection, LRU migration)
- Storage format: 4 KB page / 2 MB region / 2 GB tablet
- ZNS-aware WAL with CRC32C checksums
- Data-centric morsel-driven executor (ADR-018)
- DPccp join ordering (ADR-019)
- Approximate SQL with (ε,δ) guarantees (ADR-015, ADR-024)
- Similarity search via VPOPCNTDQ + LSH (ADR-017)
- rANS compression for cold-tier columns (ADR-025)
- Calibrated analytic cost model (ADR-023, measured on Zen 5)
- Formal specification (SPECIFICATION.md, 755 lines)
- Problem catalog: 99 problems across 10 files
- 5-wave research corpus with per-problem solution evaluations
- CCL-X 1.2 license

### Measured performance (AMD EPYC-Turin / Zen 5)
- scan_eq AVX-512: 24.1 G cells/sec
- sum_f64 AVX-512: 29.8 G cells/sec
- hamming VPOPCNTDQ: 24.2 G cells/sec
- Memory bandwidth: 40.6 GB/s

## [0.1.0] — 2025-07-28

### Added
- Initial NaN-boxed Cell prototype (superseded by instruction-first architecture)
- Basic encoders: TF-IDF, char n-gram, color histogram, DCT, FFT, feature hashing, random projection
- Non-ML tensor storage with int8 quantization and sparse CSR
- LSM-style storage (WAL + SSTable)
- LSH and brute-force indexes
- axum HTTP server + clap CLI
