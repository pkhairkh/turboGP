# turboGP

> An **instruction-first, memory-centric** relational database engine.
>
> The thesis: design the database from the silicon up. Pick the cheapest
> instructions per joule, place data in the memory tier that feeds them, and
> treat every protocol boundary (CXL / RoCEv2 / IB) as a first-class design
> axis. The table-and-column model is the last layer, not the first.

## Quick links

| Document | What it is |
|----------|-----------|
| **[ARCHITECTURE.md](ARCHITECTURE.md)** | The architecture in 1 page (dispatch path + module map) |
| **[CHANGELOG.md](CHANGELOG.md)** | Per-wave change log (v3 remediation Waves 0-14) |
| **[CONTRIBUTING.md](CONTRIBUTING.md)** | MSRV, coding standards, CI gates, branch/PR process |
| **[docs/README.md](docs/README.md)** | Documentation index (reading order for new contributors) |
| **[docs/adr/](docs/adr/)** | 25 accepted ADRs + open questions |
| **[docs/REFERENCES.md](docs/REFERENCES.md)** | Academic bibliography (instruction-first, morsel, WCOJ, AQP) |
| **[waves/](waves/)** | Per-wave Definition-of-Done checklists (Waves 0-12) |

## Why this exists

Every existing database engine — Postgres, MySQL, DuckDB, ClickHouse — starts
from the table-and-column abstraction and works down to the hardware. This
leads to engines that use generic executors, treat the memory hierarchy as a
performance afterthought, and pay 1.5–2× energy and latency penalties because
their inner loops weren't designed around the actual instructions the silicon
can execute.

turboGP inverts the design order:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage Layout → Executor → Schema (last)
```

## The three invariants

1. **The hot loop is a fixed instruction sequence.** Each operator compiles
   to a hand-tuned kernel per `(CPU vendor, CPU generation, memory tier)`
   tuple. The kernel table is the engine's competitive moat.

2. **Data placement follows the hierarchy, not the schema.** Every piece of
   data lives in a specific tier (L1/L2 → L3 → DDR5 → HBM → CXL → NVMe →
   NVMe-oF → RoCEv2/IB), chosen by access pattern. The memory manager
   migrates whole 2 MB regions between tiers based on telemetry.

3. **Protocols define coherence and reach boundaries.** The transaction
   coordinator runs at protocol boundaries: CXL for single-rack (~250 ns
   commit), Raft over RoCEv2 for cross-rack (~10 µs), async for cross-region.

## Repository layout

```
turboGP/
├── README.md                         ← you are here
├── ARCHITECTURE.md                   ← the dispatch-based architecture (1-page summary)
├── CHANGELOG.md                      ← per-wave change log
├── CONTRIBUTING.md                   ← MSRV, coding standards, CI gates
├── Cargo.toml                        ← package metadata (MSRV 1.89, edition 2021)
├── src/                              ← Rust source code
│   ├── kernel/                       ← SIMD kernels (the moat)
│   ├── memory/                       ← tier-aware memory manager
│   ├── storage/                      ← WAL + checkpoint + buffer pool
│   ├── engine/                       ← QueryEngine + dispatch + query_interpreter
│   │   ├── mod.rs                    ← QueryEngine::execute() entry point + routing
│   │   ├── dispatch.rs               ← kernel-direct query dispatch
│   │   ├── executor.rs               ← execute_select() — optimizer → dispatch → fallback
│   │   ├── result.rs                 ← QueryResult + ResultColumn
│   │   ├── dml.rs                    ← INSERT / UPDATE / DELETE
│   │   ├── ddl.rs                    ← CREATE / DROP / ALTER
│   │   ├── copy.rs                   ← COPY TO / COPY FROM (allow-listed dirs)
│   │   ├── vacuum.rs                 ← VACUUM / dead-tuple reclamation
│   │   ├── transaction.rs            ← BEGIN / COMMIT / ROLLBACK + savepoints
│   │   ├── helpers.rs                ← shared engine helpers (re-exported *)
│   │   └── query_interpreter/        ← rich-SQL fallback (formerly the god module)
│   │       ├── mod.rs                ← parse_and_execute() + per-query fast paths
│   │       ├── types.rs              ← Expr2 / BinOp2 / Value2 / SelectQuery2
│   │       ├── parser.rs             ← QueryInterpreterParser + parse helpers
│   │       ├── exec.rs               ← QueryInterpreter struct + execute
│   │       ├── join.rs               ← hash/cross join + DP join ordering
│   │       ├── aggregate.rs          ← grouped/scalar aggregates + vectorized sum
│   │       ├── subquery.rs           ← decorrelation + EXISTS/IN hash-set caching
│   │       ├── expr.rs               ← expression eval (binop, comparison, like, cast)
│   │       └── tpc_h_queries_q{1_6, 7_12, 13_18, 19_22}.rs  ← TPC-H per-query detectors
│   ├── sql/                          ← lexer, parser, AST, DDL, DML, CTE, extensions
│   ├── exec/                         ← window, pivot, merge, json, temporal, etc.
│   ├── datasource/                   ← CSV/Parquet loaders + Table struct
│   ├── catalog/                      ← table + view registries
│   ├── server/                       ← pgwire protocol server (auth, session)
│   ├── bin/turbogp.rs                ← `cargo run --bin turbogp` server entrypoint
│   └── schema/                       ← column type schema (TableSchema)
├── examples/smoke.rs                 ← end-to-end demo
├── benches/                          ← criterion benchmarks (TPC-H, WCOJ, kernels)
├── tests/                            ← integration tests (33+ suites)
├── scripts/                          ← check_no_panics.sh, check_dead_code.sh, check_file_size.sh
├── deploy/                           ← Helm chart + K8s manifests (Wave 10)
│   ├── helm/                         ← Chart.yaml + templates/{statefulset,service,pdb,configmap,secret}.yaml
│   └── k8s/turbogp.yaml              ← bare K8s StatefulSet manifest
├── .github/workflows/                ← CI: ci, cross-os, msrv, coverage, fuzz, deadcode, security, release
└── docs/
    ├── README.md                     ← documentation index (start here)
    ├── REFERENCES.md                 ← academic bibliography
    └── adr/                          ← 25 ADRs + OPEN_QUESTIONS.md
```

## The storage format: instruction-shaped, not schema-shaped

Every value is a **64-bit word** — not for type uniformity, but because the
cheapest SIMD instructions on modern x86 and ARM operate on 64-bit lanes
(`VPCMPEQQ`, `VPADDQ`, `VPOPCNTDQ`, `VPTERNLOGQ`).

The hierarchy of storage units:

| Unit | Size | Why this size |
|------|------|---------------|
| **Word** | 8 bytes | Matches `VPCMPEQQ` / `VPOPCNTDQ` lane width |
| **Page** | 4 KB | OS page size, TLB granularity, 64 cache lines, 512 cells |
| **Region** | 2 MB | Huge page granularity, unit of migration between tiers |
| **Tablet** | 2 GB | NUMA placement unit, smallest CXL-pinnable structure |

## The kernel table

Each operator has multiple kernel implementations, one per
`(CPU vendor, CPU generation, memory tier)` tuple. Example:

| Operator | CPU | Tier | Throughput |
|----------|-----|------|-----------|
| `scan_eq` | SPR AVX-512 | L3 | 19 G cells/sec |
| `scan_eq` | SPR AVX-512 | DDR5 | 5 G cells/sec (4-page prefetch) |
| `scan_eq` | SPR AVX-512 | CXL | 3 G cells/sec (8-page prefetch) |
| `hash_probe` | SPR | L3 | 8 G probes/sec (SwissTable) |
| `aggregate_sum` | SPR | L3 | 16 G cells/sec (`VFMADD231PS`) |
| `similarity_hamming` | Zen 5 | L3 | 8 G cells/sec (`VPOPCNTDQ`) |

The kernel table is indexed at startup via CPUID; the best kernel per
`(operator, tier)` is selected for the running hardware.

## Quick start

```bash
# Build the library and tests
cargo build
cargo test --lib --tests

# Run the standalone pgwire server (Wave 11 binary entrypoint)
cargo run --bin turbogp -- --port 5432 --data-dir ./data
cargo run --bin turbogp -- --insecure           # no auth (development)
cargo run --bin turbogp -- --tls-cert c.pem --tls-key k.pem

# End-to-end smoke demo (in-process)
cargo run --release --example smoke

# AVX-512 throughput benchmarks (external baselines opt-in)
cargo bench --features bench-external
```

The server speaks the pgwire protocol — connect with `psql -h 127.0.0.1 -p 5432`.
For deployment, the `deploy/` directory ships a Helm chart
(`deploy/helm/Chart.yaml`) and a bare K8s StatefulSet manifest
(`deploy/k8s/turbogp.yaml`), both with graceful shutdown wired through
SIGTERM/SIGINT.

## What this is not

- **Not a faster OLAP engine.** On TPC-H, this loses to DuckDB by 1.2–1.5×
  because DuckDB's type-stable columns are more compact than 64-bit-everywhere
  (see ADR-021 — accepted as the cost of the design point).
- **Not a production database.** This is a research prototype demonstrating
  the instruction-first architecture.

## What this is

- A **unified substrate for tier-aware, instruction-tuned data processing**
  that wins on:
  - Heterogeneous/semi-structured analytics: 5–10× faster than DuckDB
  - Memory-disaggregated scale-up: 2–3× effective capacity via CXL
  - Energy efficiency: 3–5× lower energy per query
  - Schema evolution: near-zero cost (metadata only)

## Research agenda

| Phase | Status | Description |
|-------|--------|-------------|
| 1 | ✅ Done (v3 Wave 1) | Dead-code purge + `ast.rs` wired; check_dead_code.sh in CI |
| 2 | ✅ Done (v3 Wave 2) | God-module decomposition: the 13.5 K-LOC interpreter → `query_interpreter/` (12 sub-modules); `engine/mod.rs` (4.2 K LOC) → 7 sub-modules |
| 3 | ✅ Done (v3 Wave 3) | Panic remediation — `check_no_panics.sh` passes (zero `unwrap`/`expect`/`panic!` in production code) |
| 4 | ✅ Done (v3 Wave 4) | IR migration — all 7 `QueryExtensions` fields consumed; full `Expr` unification deferred to a later wave |
| 5 | 🚧 In progress | ACID isolation + concurrent write transactions |
| 6 | 🚧 In progress | Cost-based planner wiring (DPccp / MCTS off the hot path) |
| 7 | 🚧 In progress | Index + sketch executor integration |
| 8 | 🚧 In progress | Morsel-driven parallelism (ADR-018) |
| 9 | ✅ Done (v3 Wave 9) | CI/CD — coverage 60 %, cross-OS (ubuntu+macos), MSRV 1.89, fuzz 10 k iterations |
| 10 | ✅ Done (v3 Wave 10) | Deployment — Helm chart, K8s StatefulSet, graceful shutdown |
| 11 | 🚧 In progress | Observability + slow-query logging hardening |
| 12 | 🚧 In progress | Protocol coordinator (CXL / Raft-over-RoCEv2) — currently stubs |
| 13 | 🚧 In progress | DPU / computational-storage pushdown — currently stubs |
| 14 | 🚧 In progress | CXL-aware buffer pool + migration policy — currently stubs |

## Current SQL surface

The following SQL features work end-to-end through `QueryEngine::execute()`:

- **DDL**: `CREATE TABLE`, `DROP TABLE`, `CREATE SCHEMA`, `CREATE VIEW`, `DROP VIEW`, `CREATE PROCEDURE`
- **DML**: `INSERT`, `UPDATE`, `DELETE` with WHERE clauses supporting `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `AND`, `OR`
- **SELECT**: `SELECT *`, `SELECT col`, `SELECT col1, col2`, `SELECT count(*)`, `SELECT sum/avg/min/max(col)`, `SELECT count(DISTINCT col)`
- **JOIN**: `INNER JOIN`, `LEFT [OUTER] JOIN`, `RIGHT [OUTER] JOIN`, `FULL [OUTER] JOIN`, `CROSS JOIN` (with ON clause)
- **GROUP BY**: single-key and multi-key, with multiple aggregates in one query
- **ORDER BY**: ascending/descending, string-aware (uses StringSearchColumn sidecar when present)
- **LIMIT**: row count limiting
- **WHERE**: `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `LIKE` (with `%` wildcards), `AND`, `OR`
- **NULL semantics**: NULL bitmaps track NULL cells; `COUNT(col)` excludes NULLs; pgwire sends NULL as `-1` length
- **Transactions**: `BEGIN`, `COMMIT`, `ROLLBACK` with snapshot isolation, plus `SAVEPOINT` / `ROLLBACK TO`
- **WAL**: write-ahead log with BEGIN/COMMIT/ROLLBACK markers, base64-encoded SQL, replay on restart
- **Checkpoint**: type-preserving (FLOAT, VARCHAR, NULL all round-trip correctly), loaded on startup via `with_data_dir`
- **CTE**: `WITH ... AS (...) SELECT ...` including recursive CTEs
- **Views**: `CREATE VIEW` + `SELECT FROM view` (materialized on query)
- **Procedures**: `CREATE PROCEDURE` + `EXEC proc_name [args]`
- **MERGE**: `MERGE INTO target WHEN MATCHED THEN UPDATE/DELETE/INSERT`
- **Temporal**: `FOR SYSTEM_TIME AS OF <timestamp>` (requires pre-registered TemporalTable)
- **Window functions**: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `SUM()`, `COUNT()` with `OVER (PARTITION BY ... ORDER BY ...)`
- **pgwire server**: extended query protocol (Parse/Bind/Describe/Execute/Sync), NULL handling, max_rows/cursor support, SCRAM-SHA-256 auth, TLS
- **Data loading**: CSV, Parquet (with NULL bitmap and StringSearchColumn sidecar)
- **VACUUM**: dead-tuple reclamation
- **COPY**: `COPY table TO/FROM '/path'` (gated by `allowed_copy_dirs` allow-list, SQLSTATE 42501 on violation)

## Known limitations

- **No persistent page store**: WAL + checkpoint provide durability across
  restarts, but the buffer pool (`storage/buffer_pool.rs`) is a recent
  addition (Wave 63) and is not yet the default for all tables.
- **CXL / RoCEv2 / IB are stubs**: the protocol modules exist as type
  definitions but are not wired to the executor. turboGP is currently
  single-node, in-memory.
- **Morsel executor not used**: ADR-018 (data-centric morsel-driven pipeline)
  is accepted, but the SQL executor uses dispatch + vectorized kernels, not
  morsel-driven parallelism.
- **DPccp / MCTS planners not wired**: `planner/optimizer.rs` exists with
  DPccp + MCTS, but the executor uses a 5-rule heuristic
  (`choose_plan` → KernelDirect vs `query_interpreter` fallback). Wiring the
  cost-based planner to the hot path is Wave 6.
- **No concurrent write transactions**: snapshot isolation supports one
  writer at a time per engine; concurrent connections each get their own
  engine via `Arc<RwLock<QueryEngine>>`.
- **String columns hashed**: strings are stored as xxh3 hashes in `u64`
  cells; the original text is preserved in a `StringSearchColumn` sidecar
  (not all operations consult the sidecar).
- **`PIVOT (...)` clause not parsed**: the `pivot()` / `unpivot()` functions
  are callable but `PIVOT (...)` in SELECT is not yet parsed.
- **JSON functions not in expression evaluator**: `JSON_VALUE`,
  `JSON_QUERY`, etc. are callable as module functions but not yet
  integrated into the SELECT expression evaluator.
- **Describe returns NoData**: the pgwire `Describe` message always returns
  NoData without inferring the schema (psql tolerates this).
- **Indexes not used by executor**: `index/manager.rs` exists but the
  executor does full scans for SELECT; index lookups are wired for
  constraint enforcement (PRIMARY KEY / UNIQUE) only.
- **`Expr` unification deferred (Wave 4)**: the legacy `Expr2` /
  `BinOp2` / `Value2` types in `query_interpreter::types` are still the
  canonical expression representation in the fallback path. The unified
  `Expr` AST in `sql/ast.rs` is wired for the dispatch path only.

See `ARCHITECTURE.md` and `docs/` for the full design.

## License

CCL-X (Civil Common License X), Version 1.2 — see `LICENSE.md` for the full
text. The `Cargo.toml` declares `license = "CCL-X-1.2"` and the `LICENSE.md`
file in the repo root is the canonical CCL-X v1.2 text. All three sources
(README, Cargo.toml, LICENSE.md) agree on CCL-X-1.2.
