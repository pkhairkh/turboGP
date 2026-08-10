# Architecture Decision Records

> ADRs document the decisions where we have ≥80% confidence, chosen to be
> mutually compatible. Each ADR follows a standard format: context, decision,
> consequences, alternatives, confidence.
>
> Decisions below 80% confidence are in [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md).

## ADR index

The 25 accepted ADRs are grouped by architectural axis. Each ADR file lives
in this directory and is named `NNN-<short-slug>.md` (zero-padded).

### Storage format & layout (ADRs 001-002)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [001](./001-64-bit-word-universal-storage.md) | Use 64-bit word as the universal storage unit | 95% | Accepted |
| [002](./002-page-region-tablet-hierarchy.md) | 4 KB page / 2 MB region / 2 GB tablet storage hierarchy | 95% | Accepted |

### Kernel dispatch & hot-loop design (ADRs 003-007)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [003](./003-cpuid-guarded-kernel-dispatch.md) | CPUID-guarded kernel dispatch for BMI2/AVX-512 | 95% | Accepted |
| [004](./004-branchless-hot-loops.md) | Branchless hot loops via mask accumulation + CMOV | 90% | Accepted |
| [005](./005-cache-line-alignment-for-atomics.md) | Cache-line alignment for all atomic-containing structs | 95% | Accepted |
| [006](./006-rep-movsb-for-bulk-copy.md) | REP MOVSB with ERMS for bulk page copy | 100% | Accepted |
| [007](./007-fixed-1024-cell-batch.md) | Fixed 1024-cell batch size for SIMD amortization | 85% | Accepted |

### Memory tiering & NUMA (ADRs 008-010)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [008](./008-numa-thread-pinning.md) | NUMA-aware thread pinning via pthread_setaffinity_np | 90% | Accepted |
| [009](./009-huge-pages-for-regions.md) | Transparent huge pages + explicit mmap for regions | 85% | Accepted |
| [010](./010-lru-tier-migration.md) | LRU for tier migration policy (k-competitive) | 90% | Accepted |

### Recovery, integrity & protocol boundaries (ADRs 011-014)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [011](./011-zns-aware-wal.md) | ZNS-aware WAL via io_uring | 85% | Accepted |
| [012](./012-crc32c-page-checksum.md) | CRC32C + per-page XOR parity for checksum | 85% | Accepted |
| [013](./013-linear-typed-memory-handles.md) | Linear-typed memory handles (CxlRef, RaftRef) | 85% | Accepted |
| [014](./014-hlc-over-ptp.md) | HLC over PTP for clock synchronization | 80% | Accepted |

### Approximate query processing, planning & indexing (ADRs 015-019)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [015](./015-empirical-bernstein-approximate-sql.md) | Empirical Bernstein + sequential stopping for (ε,δ) | 85% | Accepted |
| [016](./016-greedy-submodular-index-selection.md) | Greedy submodular maximization for index selection | 85% | Accepted |
| [017](./017-brute-vpopcntdq-then-lsh.md) | Similarity: brute VPOPCNTDQ ≤10⁶, LSH above | 85% | Accepted |
| [018](./018-data-centric-morsel-executor.md) | Data-centric morsel-driven pipeline execution | 90% | Accepted |
| [019](./019-dpccp-join-ordering.md) | DPccp for n≤15 joins, IDP for n>15 | 85% | Accepted |

### Benchmarking, cost models & query-set positioning (ADRs 020-025)

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [020](./020-kingman-admission-control.md) | Kingman ρ-guard + token bucket for admission | 80% | Accepted |
| [021](./021-tpc-h-accept-loss.md) | TPC-H: run as-is, accept 1.2–1.5× loss | 95% | Accepted |
| [022](./022-rapl-energy-benchmarking.md) | RAPL + external meter for energy benchmarking | 85% | Accepted |
| [023](./023-calibrated-analytic-cost-model.md) | Calibrated analytic cost model (Kingman + measured AVX-512) | 85% | Accepted |
| [024](./024-mcdiarmid-eps-delta-joins.md) | McDiarmid bounded-differences for (ε,δ) through joins | 85% | Accepted |
| [025](./025-rans-cold-tier-only.md) | rANS compression for cold-tier columns only (CXL, NVMe) | 80% | Accepted |

**25 ADRs accepted. 7 open questions remain** (see [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md)).

## How ADRs relate to the v3 waves

The v3 remediation cycle (see [`../../CHANGELOG.md`](../../CHANGELOG.md))
does not introduce new ADRs — it operationalises the existing 25. The
mapping from wave to the ADRs it touches:

| Wave | ADRs touched | What changed |
|------|-------------|--------------|
| Wave 1 (dead code) | — | Removed orphaned modules that referenced ADRs no longer on a code path |
| Wave 2 (decomposition) | — | Interpreter god module → `query_interpreter/`; no ADR change, just file layout |
| Wave 3 (panics) | — | Coding standard; no ADR change |
| Wave 4 (IR migration) | — | All 7 `QueryExtensions` fields consumed |
| Wave 9 (CI/CD) | — | Coverage 60 %, cross-OS, MSRV 1.89, fuzz 10 k |
| Wave 10 (deployment) | — | Helm + K8s + graceful shutdown |
| Wave 6 (planner wiring, in progress) | 019, 023 | DPccp + calibrated cost model onto the hot path |
| Wave 8 (morsel, in progress) | 007, 008, 018 | Morsel-driven parallelism on the dispatch path |
| Wave 12 (protocol, in progress) | 013, 014 | CXL / Raft-over-RoCEv2 wired to the executor |
| Wave 14 (CXL buffer pool, in progress) | 010, 025 | LRU migration + rANS cold-tier in the buffer pool |

## Compatibility matrix

These ADRs are chosen to be **mutually compatible** — no two accepted ADRs
conflict. Key compatibility relationships:

| ADRs | Why they're compatible |
|------|----------------------|
| 001 (64-bit word) + 004 (branchless) + 017 (VPOPCNTDQ) | All assume 64-bit lanes; VPCMPEQQ/VPOPCNTDQ operate on u64 |
| 002 (page/region/tablet) + 006 (REP MOVSB) + 009 (huge pages) | Region size = huge page = 2 MB; copy uses ERMS |
| 003 (CPUID dispatch) + 007 (1024 batch) + 018 (morsel) | Morsel size = batch size; kernel selected per CPU |
| 008 (NUMA pinning) + 010 (LRU migration) + 018 (morsel) | Morsels are NUMA-local; migration moves whole regions |
| 013 (linear types) + 014 (HLC) | Both are type/time system foundations; no overlap |
| 015 (Bernstein) + 016 (submodular) + 019 (DPccp) | All feed the planner; different decision points |
| 020 (Kingman admission) + 018 (morsel) | Admission controls query rate; morsel controls execution |
| 021 (TPC-H loss) + 022 (RAPL) | Honest baseline + honest energy measurement |

## ADR format

Each ADR follows this structure:

```
# ADR-NNN: Title

## Status
Accepted

## Confidence
XX% (with rationale)

## Context
[Why this decision is needed]

## Decision
[What we decided]

## Consequences
### Positive
### Negative

## Alternatives considered
[What else we looked at and why we didn't pick it]

## References
[Papers, docs, evidence]
```

When proposing a new ADR, copy this template, name the file
`NNN-<short-slug>.md` (next available number, zero-padded to 3 digits), and
open a PR. The ADR is "Accepted" once review concludes ≥80 % confidence;
below that threshold it lives in [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md)
until the evidence strengthens.

## See also

- [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md) — decisions below 80% confidence
- [../REFERENCES.md](../REFERENCES.md) — academic bibliography cited by the ADRs
- [../README.md](../README.md) — documentation index
- [../../ARCHITECTURE.md](../../ARCHITECTURE.md) — the architecture in 1 page
- [../../CHANGELOG.md](../../CHANGELOG.md) — per-wave change log
