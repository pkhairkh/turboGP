# Documentation Index

> Entry point for all turboGP documentation. Read in order if you're new;
> jump to a section if you're looking for something specific.

## Start here

| Document | What it is | When to read |
|----------|-----------|--------------|
| **[FINE_DRAFT.md](./FINE_DRAFT.md)** | The master document: the venture, the architecture, the problem catalog with solutions, the build plan | **Read first.** This is the comprehensive fine draft. |
| **[../README.md](../README.md)** | Project overview, quick start, repository layout | If you just want to run the code |
| **[../ARCHITECTURE.md](../ARCHITECTURE.md)** | The instruction-first architecture in 1 page | If you want the design summary |

## Directory structure

```
docs/
├── README.md                  ← documentation index (start here)
├── FINE_DRAFT.md              ← THE definitive synthesis (read first)
├── SPECIFICATION.md           ← formal technical specification
├── adr/                       ← Architecture Decision Records (≥80% confidence)
│   ├── README.md              ← ADR index with compatibility matrix
│   ├── OPEN_QUESTIONS.md      ← decisions below 80% confidence
│   ├── 001-64-bit-word-...    ← 25 accepted ADRs
│   └── ...
├── architecture/              ← design docs + CPU energy knowledgebase
├── research/                  ← math foundations + domain deep-dives + wave evaluations
│   ├── math-foundations.md
│   ├── math-enhancements.md
│   ├── domains/               ← 5 mathematical pillars
│   └── waves/                 ← per-problem solution evaluations (performance/time/energy)
├── problems/                  ← problem catalog: 99 problems across 10 files
├── benchmarks/                ← TPC-C and TPC-H analysis
```

## Reading order for a new contributor

1. **[FINE_DRAFT.md](./FINE_DRAFT.md)** — the definitive synthesis of the venture
2. **[adr/README.md](./adr/README.md)** — the 25 accepted decisions (≥80% confidence)
3. **[adr/OPEN_QUESTIONS.md](./adr/OPEN_QUESTIONS.md)** — the 10 undecided questions (<80% confidence)
4. **[architecture/instruction-first.md](./architecture/instruction-first.md)** — the design philosophy
5. **[problems/README.md](./problems/README.md)** — the problem catalog index
6. **[research/math-foundations.md](./research/math-foundations.md)** — the mathematical grounding

## Reading order for a researcher

1. **[research/math-foundations.md](./research/math-foundations.md)** — the 5-pillar synthesis
2. **[research/domains/](./research/domains/)** — pick the domain closest to your expertise
3. **[research/waves/](./research/waves/)** — see the per-problem solution evaluations
4. **[problems/09-open-research.md](./problems/09-open-research.md)** — the 12 PhD-thesis-scale open questions

## Reading order for an engineer

1. **[../ARCHITECTURE.md](../ARCHITECTURE.md)** — the design summary
4. **[research/waves/](./research/waves/)** — see the candidate solutions with effort estimates
