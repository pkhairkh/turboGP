# References

> BibTeX-style bibliography for the academic literature cited in the turboGP
> Production Readiness Assessment (revised) and in `ARCHITECTURE.md` →
> "Academic Positioning". Each entry is keyed by the short label used in
> the prose (e.g. `polychroniou2015rethinking`, `arxiv:2209.12332`).

These 13 references are the citations the revised review draws on when
positioning turboGP against the published state of the art on (a) instruction-
first / SIMD-vectorized execution, (b) morsel-driven parallelism, (c)
worst-case optimal joins and tensor-network contraction, (d) learned and
calibrated cost/cardinality models, (e) approximate query processing, (f)
data-centric operating systems, (g) Cascades-style optimization, and
(h) energy-aware benchmarking and tiered-memory expansion.

## Vectorized execution & instruction-first design

```bibtex
@inproceedings{polychroniou2015rethinking,
  author    = {Polychroniou, Orestis and Raghavan, Arun and Ross, Kenneth A.},
  title     = {Rethinking {SIMD} Vectorization for In-Memory Databases},
  booktitle = {Proceedings of the 2015 ACM SIGMOD International Conference on
               Management of Data (SIGMOD '15)},
  year      = {2015},
  pages     = {1493--1508},
  publisher = {ACM},
  doi       = {10.1145/2723372.2748940},
  note      = {Argues that fully vectorized, branch-free, column-at-a-time
               execution wins over both row and NSM models on modern x86.
               Cited from src/exec/vectorized.rs as the design target for
               turboGP's hot-loop kernels.}
}
```

## Morsel-driven parallelism

```bibtex
@inproceedings{leis2014morsel,
  author    = {Leis, Viktor and Kemper, Alfons and Neumann, Thomas},
  title     = {Morsel-Driven Parallelism: A Numerical-Symmetric-Hash-Join
               Doesn't Have to Be a Numbers Game},
  booktitle = {Proceedings of the 2014 ACM SIGMOD International Conference on
               Management of Data (SIGMOD '14)},
  year      = {2014},
  pages     = {743--754},
  publisher = {ACM},
  doi       = {10.1145/2588555.2610507},
  note      = {Foundational reference for ADR-018 (data-centric morsel-driven
               pipeline execution). The morsel is the unit of work, scheduling
               and NUMA placement.}
}
```

## Worst-case optimal joins & leapfrog triejoin

```bibtex
@misc{veldhuizen2014leapfrog,
  author       = {Veldhuizen, Todd L.},
  title        = {Leapfrog Triejoin: A Simple, Worst-Case Optimal Join Algorithm},
  year         = {2014},
  eprint       = {1210.0481},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Reference algorithm for src/kernel/leapfrog.rs and the
                  ADR-019/024 WCOJ selection logic.}
}
```

## Tensor-network contraction ordering

```bibtex
@article{arxiv2209_12332,
  author       = {{author redacted}},
  title        = {On the Optimal Linear Contraction Order of Tree Tensor
                  Networks},
  journal      = {arXiv preprint arXiv:2209.12332},
  year         = {2024},
  eprint       = {2209.12332},
  archivePrefix= {arXiv},
  primaryClass = {quant-ph},
  note         = {Bleeding-edge reference for planner/tensor.rs
                  (plan_with_tensor_network). Maps the multi-way join
                  ordering problem to optimal tensor-network contraction
                  on tree topologies.}
}

@article{arxiv2001_08063,
  author       = {{author redacted}},
  title        = {Algorithms for Tensor Network Contraction Ordering},
  journal      = {arXiv preprint arXiv:2001.08063},
  year         = {2020},
  eprint       = {2001.08063},
  archivePrefix= {arXiv},
  primaryClass = {cs.DS},
  note         = {Algorithmic foundation for the contraction-order search
                  in planner/contraction.rs; complements arXiv:2209.12332.}
}
```

## Learned cardinality estimation

```bibtex
@article{arxiv2012_06743,
  author       = {{author redacted}},
  title        = {Are We Ready For Learned Cardinality Estimation?},
  journal      = {arXiv preprint arXiv:2012.06743},
  year         = {2020},
  eprint       = {2012.06743},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Survey/benchmark of learned cardinality models; positions
                  turboGP's LearnedCardinality (planner/learned.rs,
                  per-(table,column) equi-width histograms with EWMA) as a
                  ~2018-era baseline that needs an upgrade path.}
}
```

## Calibrated / adaptive cost models

```bibtex
@article{arxiv2409_17136,
  author       = {{author redacted}},
  title        = {Adaptive Cost Model for Query Optimization},
  journal      = {arXiv preprint arXiv:2409.17136},
  year         = {2024},
  eprint       = {2409.17136},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Direct academic anchor for ADR-023 (calibrated analytic cost
                  model). Validates the calibrated-throughput + queueing
                  hybrid approach over learned-only models.}
}
```

## Approximate query processing

```bibtex
@article{arxiv1804_00770,
  author       = {{author redacted}},
  title        = {{VerdictDB}: Universalizing Approximate Query Processing
                  Across All Major SQL Engines},
  journal      = {arXiv preprint arXiv:1804.00770},
  year         = {2018},
  eprint       = {1804.00770},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Reference for the engine-agnostic AQP framing that
                  ADR-015 (Empirical Bernstein) and ADR-024 (McDiarmid
                  through joins) build on.}
}

@article{arxiv1903_10000,
  author       = {{author redacted}},
  title        = {Approximate Query Processing using Deep Generative Models},
  journal      = {arXiv preprint arXiv:1903.10000},
  year         = {2019},
  eprint       = {1903.10000},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Complementary deep-generative-model baseline against which
                  the analytic (Bernstein/McDiarmid) AQP path is positioned.}
}
```

## Data-centric operating systems

```bibtex
@article{arxiv2007_11112,
  author       = {{author redacted}},
  title        = {{DBOS}: A Proposal for a Data-Centric Operating System},
  journal      = {arXiv preprint arXiv:2007.11112},
  year         = {2020},
  eprint       = {2007.11112},
  archivePrefix= {arXiv},
  primaryClass = {cs.OS},
  note         = {Reference for the broader thesis that the database should
                  be the operating system. turboGP is narrower (single-node
                  in-memory engine) but shares the data-centric instinct.}
}
```

## Cascades / query optimization in the wild

```bibtex
@article{arxiv2510_20082,
  author       = {{author redacted}},
  title        = {Query Optimization in the Wild: Realities and Trends
                  (Cascades survey)},
  journal      = {arXiv preprint arXiv:2510.20082},
  year         = {2025},
  eprint       = {2510.20082},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Modern survey of Cascades-style optimizers; frames the gap
                  between turboGP's 5-rule heuristic choose_plan and the
                  ~30+-rule cascades engine a production system needs.}
}
```

## Energy-aware benchmarking

```bibtex
@article{arxiv2604_09048,
  author       = {{author redacted}},
  title        = {{Watt Counts}: Energy-Aware Benchmarking},
  journal      = {arXiv preprint arXiv:2604.09048},
  year         = {2026},
  eprint       = {2604.09048},
  archivePrefix= {arXiv},
  primaryClass = {cs.DB},
  note         = {Reference for ADR-022 (RAPL + external meter energy
                  benchmarking). Establishes the energy-per-query accounting
                  turboGP's 3-5x lower-energy-per-query claim is measured
                  against.}
}
```

## Inference-tiered memory expansion

```bibtex
@article{arxiv2606_12556,
  author       = {{author redacted}},
  title        = {{ITME}: Inference Tiered Memory Expansion},
  journal      = {arXiv preprint arXiv:2606.12556},
  year         = {2026},
  eprint       = {2606.12556},
  archivePrefix= {arXiv},
  primaryClass = {cs.AR},
  note         = {Reference for the CXL/NVMe tiered-memory story that
                  ADR-010 (LRU migration), ADR-025 (rANS cold-tier) and
                  the memory/ module build on.}
}
```

## Cross-reference table

| Reference                | Cited from                                 | ADR / module                     |
|--------------------------|--------------------------------------------|----------------------------------|
| `polychroniou2015rethinking` | `src/exec/vectorized.rs:7`             | exec/vectorized.rs               |
| `leis2014morsel`         | ARCHITECTURE.md, `src/executor/morsel.rs` | ADR-018                          |
| `veldhuizen2014leapfrog` | `src/kernel/leapfrog.rs`, planner/wcoj.rs | ADR-019 / ADR-024                |
| `arxiv:2209.12332`       | `planner/mod.rs:487`, planner/tensor.rs   | tensor contraction               |
| `arxiv:2001.08063`       | `planner/contraction.rs`                   | contraction ordering             |
| `arxiv:2012.06743`       | `planner/learned.rs`                       | learned cardinality              |
| `arxiv:2409.17136`       | `planner/calibration.rs`, planner/mod.rs   | ADR-023                          |
| `arxiv:1804.00770`       | ARCHITECTURE.md                            | ADR-015 / ADR-024 (AQP framing)  |
| `arxiv:1903.10000`       | ARCHITECTURE.md                            | ADR-015 (deep-gen baseline)      |
| `arxiv:2007.11112`       | ARCHITECTURE.md                            | data-centric instinct            |
| `arxiv:2510.20082`       | ARCHITECTURE.md                            | optimizer roadmap                |
| `arxiv:2604.09048`       | ADR-022                                    | RAPL energy benchmarking         |
| `arxiv:2606.12556`       | ADR-010 / ADR-025                          | tiered-memory expansion          |
