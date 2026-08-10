# Documentation Index

> Entry point for all turboGP documentation. Read in order if you're new;
> jump to a section if you're looking for something specific.

## Start here

| Document | What it is | When to read |
|----------|-----------|--------------|
| **[../README.md](../README.md)** | Project overview, quick start, repository layout, known limitations | If you just want to run the code |
| **[../ARCHITECTURE.md](../ARCHITECTURE.md)** | The instruction-first architecture in 1 page — dispatch path + module map | If you want the design summary |
| **[../CHANGELOG.md](../CHANGELOG.md)** | Per-wave change log (v3 Waves 0-14) | If you want to know what changed and when |
| **[../CONTRIBUTING.md](../CONTRIBUTING.md)** | MSRV 1.89, coding standards, CI gates, PR process | If you are about to open a PR |
| **[./adr/README.md](./adr/README.md)** | ADR index + compatibility matrix | If you want to understand a specific design decision |

## Directory structure

```
docs/
├── README.md                  ← documentation index (you are here)
├── REFERENCES.md              ← academic bibliography (Polychroniou, Leis, Veldhuizen, etc.)
└── adr/                       ← Architecture Decision Records (≥80% confidence)
    ├── README.md              ← ADR index with compatibility matrix
    ├── OPEN_QUESTIONS.md      ← decisions below 80% confidence (7 open)
    ├── 001-64-bit-word-universal-storage.md
    ├── 002-page-region-tablet-hierarchy.md
    ├── ...
    └── 025-rans-cold-tier-only.md   ← 25 accepted ADRs total
```

The waves directory (per-wave Definition-of-Done checklists) lives at the
repo root: [`../waves/`](../waves/).

## Reading order for a new contributor

1. **[../README.md](../README.md)** — project overview, the three invariants,
   the kernel table, quick start, current SQL surface, known limitations
2. **[../ARCHITECTURE.md](../ARCHITECTURE.md)** — the dispatch-based
   architecture, the engine module structure, the execution flow, the
   query interpreter fallback
3. **[./adr/README.md](./adr/README.md)** — the 25 accepted ADRs and how
   they compose (compatibility matrix)
4. **[./adr/OPEN_QUESTIONS.md](./adr/OPEN_QUESTIONS.md)** — the 7 undecided
   questions below 80 % confidence
5. **[../CONTRIBUTING.md](../CONTRIBUTING.md)** — MSRV 1.89, the no-panic
   coding standard, CI gates, the PR process
6. **[../CHANGELOG.md](../CHANGELOG.md)** — what each v3 wave changed
   (Waves 1-4, 9-10 done; 5-8, 11-14 in progress)

## Reading order for a researcher

1. **[../ARCHITECTURE.md](../ARCHITECTURE.md) → "Academic Positioning"** —
   the engine's defensible novelty (the three-axis kernel table) and where
   it sits relative to the published state of the art
2. **[./REFERENCES.md](./REFERENCES.md)** — the BibTeX-style bibliography
   (Polychroniou 2015, Leis 2014, Veldhuizen 2014, Atserias-Grohe-Marx
   2008, Cascades survey, VerdictDB, energy-aware benchmarking, ITME)
3. **[./adr/](./adr/)** — pick the ADR closest to your expertise:
   - Instruction-first kernels: ADR-001, ADR-003, ADR-004, ADR-017
   - Tiered memory: ADR-002, ADR-009, ADR-010, ADR-025
   - Morsel-driven execution: ADR-007, ADR-008, ADR-018
   - Planning: ADR-019, ADR-023
   - Approximate query processing: ADR-015, ADR-024
   - Energy: ADR-022
   - TPC-H positioning: ADR-021
4. **[./adr/OPEN_QUESTIONS.md](./adr/OPEN_QUESTIONS.md)** — the open
   research questions the engine has not yet committed to

## Reading order for an engineer

1. **[../ARCHITECTURE.md](../ARCHITECTURE.md)** — the design summary,
   especially the "Engine module structure" section and the execution flow
2. **[../README.md](../README.md) → "Quick start"** — `cargo run --bin turbogp`,
   the smoke example, the benchmark workflow
3. **[../CONTRIBUTING.md](../CONTRIBUTING.md)** — the CI gates you must
   pass before merge (CI, Cross-OS, MSRV, Coverage, Fuzz, Dead Code,
   Security)
4. **[../waves/](../waves/)** — per-wave Definition-of-Done checklists
   for the v3 remediation cycle
5. **[../deploy/](../deploy/)** — Helm chart + K8s manifest for the
   deployment story (Wave 10)

## Document inventory

| Path | Lines (approx) | Last updated |
|------|-----------------|--------------|
| `README.md` | ~250 | v3 Wave 2 |
| `ARCHITECTURE.md` | ~290 | v3 Wave 2 |
| `CHANGELOG.md` | ~190 | v3 Wave 10 |
| `CONTRIBUTING.md` | ~120 | v3 Wave 9 |
| `docs/README.md` | this file | v3 Wave 10 |
| `docs/REFERENCES.md` | ~250 | (academic bibliography) |
| `docs/adr/README.md` | ~95 | v3 Wave 2 |
| `docs/adr/OPEN_QUESTIONS.md` | ~140 | (7 open questions) |
| `docs/adr/00{1..9}-*.md` | per-ADR | (see each ADR) |
| `docs/adr/01{0..9}-*.md` | per-ADR | (see each ADR) |
| `docs/adr/02{0..5}-*.md` | per-ADR | (see each ADR) |

**25 ADRs accepted. 7 open questions remain.**
