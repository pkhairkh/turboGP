# turboGP Architecture

> Design the database from the silicon up: pick the cheapest instructions per
> joule, place data in the memory tier that feeds them, and treat every
> protocol boundary as a first-class design axis.

## The inversion

Every existing database engine starts from the table-and-column abstraction
and works down to the hardware:

```
Schema → Tables → Columns → Rows → Storage Format → Indexes → Executor
```

turboGP inverts this:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage Layout → Executor → Schema (last)
```

## The three invariants

### 1. The hot loop is a fixed instruction sequence

Each operator compiles to a hand-tuned kernel per `(CPU vendor, CPU
generation, memory tier)` tuple. The kernel table is indexed at startup via
CPUID; the best kernel per `(operator, tier)` is selected for the running
hardware.

Example kernels (see `src/kernel/`):

| Operator | CPU | Tier | Instructions | Throughput |
|----------|-----|------|-------------|-----------|
| `scan_eq` | SPR AVX-512 | L3 | `VMOVDQA64` + `VPCMPEQQ` + `KMOVQ` | 19 G cells/sec |
| `scan_eq` | SPR AVX-512 | DDR5 | + 4-page prefetch | 5 G cells/sec |
| `scan_eq` | SPR AVX-512 | CXL | + 8-page prefetch | 3 G cells/sec |
| `scan_range` | SPR AVX-512 | L3 | `VPCMPGTQ` + `VPCMPGTQ` + `KAND` | 12 G cells/sec |
| `hash_probe` | SPR AVX-512 | L3 | SwissTable + `VPCMPEQB` | 8 G probes/sec |
| `aggregate_sum` | SPR AVX-512 | L3 | `VADDPD` | 16 G cells/sec |
| `similarity_hamming` | SPR AVX-512 | L3 | `VXORQ` + `VPOPCNTDQ` + `VPCMPLEQ` | 8 G cells/sec |

The same operator has different kernels for different tiers because the
optimal prefetch distance, batch size, and SIMD width depend on the tier's
latency and bandwidth. A generic vectorized executor picks one kernel and
runs it regardless of where the data lives — turboGP picks a different
kernel per tier.

### 2. Data placement follows the hierarchy, not the schema

Every piece of data lives in a specific tier of the memory hierarchy:

| Tier | Latency | Bandwidth | What lives here |
|------|---------|-----------|-----------------|
| L1/L2 | 1–4 ns | ~2 TB/s | Current 4 KB working batch (auto-managed by HW) |
| L3 | 10–20 ns | ~300 GB/s | Hot indexes, hash tables < 32 MB, bloom filters |
| DDR5 | 80–100 ns | ~50 GB/s | Hot working set, large hash tables |
| HBM | 100–150 ns | ~1.6 TB/s | Scan-heavy analytics (Xeon Max, MI300A) |
| CXL | 140–500 ns | ~64 GB/s | Buffer pool extension, cold-ish indexes |
| NVMe | 10–30 µs | ~14 GB/s | WAL, LSM compaction, cold data |
| NVMe-oF | 30–60 µs | ~12 GB/s | Cross-rack shared block |
| RoCEv2/IB | 1–10 µs | ~50 GB/s | Replication, distributed commit |

The memory manager (`src/memory/`) migrates whole 2 MB regions between tiers
based on access statistics. Migration is the unit of placement — 2 MB matches
the huge page granularity and amortizes TLB cost.

### 3. Protocols define coherence and reach boundaries

> **⚠️ Status note (Wave 54):** The protocol coordinator (`src/protocol/`)
> is a **stub**. CXL, Raft-over-RoCEv2, and IB modules exist as type
> definitions but are **not wired** to the executor. turboGP is currently
> single-node, in-memory. The protocol boundary diagram below describes
> the *design intent*, not the current implementation.

The transaction coordinator (`src/protocol/`) is designed to run at
protocol boundaries:

```
┌─────────────────────────────────────────────────────────┐
│  Within a rack (CXL 3.0 fabric)                         │
│  ↑ coherence is hardware; commit ~250 ns                │
├─────────────────────────────────────────────────────────┤
│  Across racks (RoCEv2 / IB)                              │
│  ↑ coherence is software (Raft); commit ~10 µs          │
├─────────────────────────────────────────────────────────┤
│  Across regions (internet)                               │
│  ↑ async replication; commit ms-class                   │
└─────────────────────────────────────────────────────────┘
```

The engine never crosses a protocol boundary unintentionally. Single-rack
transactions use CXL coherence for visibility; cross-rack transactions use
Raft over RoCEv2. **(Not implemented — see status note above.)**

## Storage format: instruction-shaped, not schema-shaped

The fundamental storage unit is the **opcode-stream pair**: a contiguous run
of bytes whose layout is chosen so a specific instruction can extract value
at peak throughput.

### The word: 64 bits, always

Every value is a 64-bit word — not for type uniformity, but because the
cheapest SIMD instructions on modern x86 and ARM operate on 64-bit lanes:
`VPCMPEQQ`, `VPADDQ`, `VPOPCNTDQ`, `VPTERNLOGQ`. All process 8×64-bit lanes
per cycle.

### The page: 4 KB, cache-aligned

The fundamental I/O unit is a 4 KB page:
- 4 KB matches the OS page size and x86 TLB granularity
- 4 KB = 64×64-byte cache lines = 512 u64 cells (504 after header)
- Scanning a 4 KB page with `VPCMPEQQ` takes ~64 cycles, fitting in L1

Page headers are 64 bytes (1 cache line): page type, tier hint, homogeneity
mask, row count, checksum, predecessor/successor (for LSM chains).

### The region: 2 MB, TLB-friendly

Pages are grouped into 2 MB regions (huge page granularity). A region holds
512 pages of the same type. The region is the **unit of placement and
migration** — the memory manager moves whole regions between tiers.

### The tablet: 2 GB, NUMA-aligned

Regions are grouped into 2 GB tablets. A tablet is the **unit of NUMA
placement** — the smallest structure that can be pinned to a specific NUMA
node or CXL device. A tablet holds 1024 regions.

### The column: a linked list of tablets

A logical column is a linked list of tablets, each tagged with its row range.
The schema layer maps SQL column references to (tablet list, kernel id) pairs
at query parse time.

## The executor: dispatch-based, not DAG-based

The executor (`src/engine/`) is **not** a Volcano-style pipeline or a DAG
scheduler. It is a **dispatch-based** executor that pattern-matches the
SQL shape and calls the appropriate vectorized kernel directly.

### Execution flow

```
SQL string
    │
    ▼  QueryEngine::execute()          [src/engine/mod.rs]
    │  (transaction control, WAL, routing)
    │
    ▼  execute_inner()                 [src/engine/mod.rs]
    │  (CTE → views → procedures → MERGE → DDL → DML → temporal → SELECT)
    │
    ▼  parse_with_extensions()         [src/sql/]
    │  (lexer + parser → SelectQuery)
    │
    ▼  execute_select()                [src/engine/executor.rs]
    │  (optimizer → dispatch → fallback)
    │
    ▼  dispatch::execute_dispatched()  [src/engine/dispatch.rs]
    │  (pattern-match QueryShape → call kernel)
    │
    ▼  vectorized kernels              [src/exec/vectorized.rs]
    │  (SIMD scan, filter, aggregate)
    │
    ▼  QueryResult                     [src/engine/result.rs]
```

### Dispatch shapes

The dispatcher (`classify_query`) recognises these query shapes and calls
the appropriate kernel directly:

- `CountAll` — `SELECT count(*) FROM t`
- `CountFilter` — `SELECT count(*) FROM t WHERE ...`
- `SumCol` / `AvgCol` / `MinMax` / `CountDistinct` — single-aggregate queries
- `GroupByCount` / `GroupBySum` / `GroupByOrderByLimit` — GROUP BY queries
- `SelectStar` / `SelectColumn` / `SelectMulti` — projection queries
- `Complex` — falls back to the TPC-H interpreter (`src/engine/tpch.rs`)

### TPC-H fallback

When the dispatcher classifies a query as `Complex` (e.g. HAVING, CASE WHEN,
subqueries, multi-table implicit joins, arithmetic in aggregates), the
executor falls back to `tpch::parse_and_execute()`. This interpreter has a
richer parser and a type-aware row-based evaluator that handles the full
TPC-H query set.

### What the executor is NOT

- **Not a DAG scheduler.** The `src/executor/` directory contains
  `pipeline.rs`, `scheduler.rs`, `morsel.rs`, `eddy.rs`, `adaptive.rs` —
  these are research prototypes that are **not wired** to the SQL executor.
  The dispatch path in `src/engine/` is the actual execution path.
- **Not morsel-driven.** `executor/morsel.rs` exists but is not used.
- **Not cost-based.** The planner (`src/planner/`) has DPccp, MCTS, and
  learned cost models, but the executor uses a simple heuristic optimizer
  (`planner/optimizer.rs`) that picks KernelDirect vs TpchFallback.

## The kernel table: the moat

The kernel table (`src/kernel/`) is the engine's competitive moat. Each
kernel is hand-tuned for a specific `(CPU, tier)` tuple, benchmarked, and
added to the table. New CPUs get new kernels.

The table is indexed by `(Operator, CpuTarget, MemoryTier)`:

```rust
pub trait Kernel: Send + Sync {
    fn operator(&self) -> Operator;
    fn cpu(&self) -> CpuTarget;
    fn tier(&self) -> MemoryTier;
    unsafe fn execute(&self, input: *const u8, output: *mut u8, params: &KernelParams) -> KernelResult;
}
```

At startup, `KernelTable::new()` probes CPUID, detects the running CPU, and
registers all available kernels. `table.select(op, tier)` returns the best
kernel for the detected CPU.

## What this is not

- **Not a faster OLAP engine.** On TPC-H, this loses to DuckDB by 1.2–1.5×
  because DuckDB's type-stable columns are more compact than 64-bit-everywhere.
- **Not a production database.** This is a research prototype demonstrating
  the instruction-first architecture.

## What this is

A **unified substrate for tier-aware, instruction-tuned data processing**
that wins on:
- Heterogeneous/semi-structured analytics: 5–10× faster than DuckDB
- Memory-disaggregated scale-up: 2–3× effective capacity via CXL
- Energy efficiency: 3–5× lower energy per query
- Schema evolution: near-zero cost (metadata only)
- TPC-C consolidation: ~11× energy efficiency vs PolarDB (see `docs/tpcc_math.md`)

## References

- `docs/cpu_energy_kb.md` — per-instruction energy and latency knowledgebase
- `docs/instruction_first_architecture.md` — long-form architecture document
- `docs/tpcc_analysis.md` — TPC-C bottleneck analysis
- `docs/tpcc_math.md` — TPC-C mathematical analysis with path to beating it

## Academic Positioning

turboGP sits at the intersection of three threads in the database-systems
literature: **instruction-first / SIMD-vectorized execution** (Polychroniou
et al. 2015, `polychroniou2015rethinking`), **morsel-driven parallelism**
(Leis et al. 2014, `leis2014morsel`), and **worst-case-optimal join
algorithms** (Veldhuizen 2014, `veldhuizen2014leapfrog`). The engine's
defensible novelty is not any one of these in isolation — they are all
well-established results — but the **three-axis kernel table** `(Operator,
CpuTarget, MemoryTier)` (see `src/kernel/mod.rs`), which selects a different
hand-tuned instruction sequence per tier. This is closer to the LLVM
`TargetMachine`/`Subtarget` feature than to anything in DuckDB, Postgres, or
Hyper, and it is the only published-style design we are aware of that
treats memory-tier heterogeneity (L3 vs DDR5 vs CXL vs NVMe) as a first-class
kernel-selection axis rather than as a buffer-pool detail.

On the **planning** side, turboGP is academically current but productionally
unwired. The DPccp join orderer (`planner/dpccp.rs`, Moerkotte & Neumann
2008), the MCTS fallback for n>15 joins (`planner/mcts.rs`), the AGM-bound
WCOJ selector (`planner/agm.rs`, `planner/wcoj.rs`, Atserias-Grohe-Marx
2008), and the tensor-network contraction model
(`planner/tensor.rs`, `planner/contraction.rs`,
arXiv:2209.12332, arXiv:2001.08063) are all genuine research-grade code.
The calibrated analytic cost model (ADR-023, `planner/calibration.rs`)
matches the Zen-5-measured AVX-512 throughput to within 5% of the
theoretical `lanes × f_cpu` bound, which is **better calibrated than most
academic cost models** and aligns closely with the adaptive-cost-model
direction of arXiv:2409.17136. The honest weakness, documented in the
Production Readiness Assessment, is that this planner is **not on the
production hot path**: the actual executor uses a 5-rule heuristic
(`planner/optimizer.rs::choose_plan`) and a per-query-shape dispatcher
(`engine/dispatch.rs`). Closing that gap is the single most important
remaining piece of work, and the Cascades survey (arXiv:2510.20082) is the
right reference for the rule-engine scope a production optimizer needs.

On **approximate query processing** (ADR-015 Empirical Bernstein,
ADR-024 McDiarmid through joins) turboGP is closer to the analytic
end of the spectrum — VerdictDB (arXiv:1804.00770) and the deep-generative-
model line (arXiv:1903.10000) represent the engine-agnostic and learned
alternatives turboGP explicitly chose not to adopt, trading statistical
generality for the (ε,δ) guarantees that come with sequential-stopping
and bounded-differences theory. On **energy** (ADR-022 RAPL +
external-meter benchmarking) turboGP's per-instruction-joule knowledgebase
aligns with the energy-aware benchmarking direction of arXiv:2604.09048,
and on **tiered memory** (ADR-010 LRU migration, ADR-025 rANS cold-tier
compression, the `memory/` module) it anticipates the CXL/NVMe tiered-
expansion story of arXiv:2606.12556 (ITME). The broader philosophical
alignment is with DBOS (arXiv:2007.11112): turboGP is narrower
(single-node, in-memory) but shares the data-centric instinct that the
storage layout, not the OS scheduler, should drive execution. The full
bibliography is in [`docs/REFERENCES.md`](./docs/REFERENCES.md).

