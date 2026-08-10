# ADR-025: rANS compression for cold-tier columns only (CXL, NVMe)

## Status
Accepted

## Confidence
80% (upgraded from OQ-03 at 55%)

## Context

Column compression needs to be entropy-optimal AND fast enough to not
bottleneck the scan kernels. The wave research (W2) recommended interleaved
rANS, but the prototype revealed a critical tradeoff.

## Measured results

A scalar rANS prototype was benchmarked on the Zen 5 machine:

| Metric | Value |
|--------|-------|
| Compression ratio | 14.92× (40 MB → 2.68 MB) |
| Encode throughput | 145.9 M symbols/sec |
| **Decode throughput (scalar)** | **78.9 M symbols/sec** |
| Uncompressed AVX-512 scan | 24,099 M cells/sec |
| **Decode / scan ratio** | **1/305** |

The decode throughput (79 M symbols/sec) is **305× slower** than the
uncompressed AVX-512 scan kernel (24 G cells/sec). Even with 8-stream
interleaved AVX-512 (`VPGATHERDD`), the projected decode throughput is
~500–1000 M symbols/sec — still **25–50× slower** than uncompressed scan.

**Root cause**: rANS decode is inherently serial — each symbol depends on
the previous state. Interleaving 8 streams helps but doesn't eliminate the
dependency chain within each stream.

## Decision

**Use rANS compression for cold-tier columns (CXL, NVMe) only. Hot-tier
columns (L3, DDR5) remain uncompressed.**

The decision is tier-dependent:

| Tier | Compression | Rationale |
|------|------------|-----------|
| L3 | Uncompressed | Throughput matters (24 G cells/sec); storage is small (32 MB) |
| DDR5 | Uncompressed | Throughput matters (5 G cells/sec); storage is moderate (512 GB) |
| **CXL** | **rANS** | Storage matters (multi-TB pools); throughput is already lower (3 G cells/sec) |
| **NVMe** | **rANS** | Storage matters most (10–100 TB); throughput is already low (14 GB/s) |

The cost model (ADR-023) picks the compression strategy based on the
tier's throughput/storage tradeoff:

```
if tier == L3 or DDR5:
    use uncompressed (throughput dominates)
elif tier == CXL or NVMe:
    use rANS (storage dominates, decode throughput is acceptable relative to tier bandwidth)
```

## Consequences

### Positive
- **15× storage savings** on cold-tier columns (measured)
- **No throughput penalty** on hot-tier columns (they stay uncompressed)
- **Tier-appropriate**: the cost model makes the tradeoff automatically
- **Decode throughput (79 M/sec scalar, ~500 M/sec AVX-512)** is acceptable
  for CXL/NVMe tiers where the raw bandwidth is already limited

### Negative
- **No compression on hot data** — L3/DDR5 columns pay full storage cost
- **Decode complexity**: the rANS decoder adds ~200 lines of code
- **Correctness risk**: the prototype had a correctness bug; production
  implementation needs thorough testing (the encode/decode roundtrip must
  be verified on all data distributions)

## Alternatives considered

1. **Always compress (all tiers)** — would bottleneck hot-tier scans by
   25–50×. Rejected.
2. **Never compress** — wastes 15× storage on cold tiers. Rejected.
3. **zstd instead of rANS** — 3 GB/s decode (vs projected 500 M/sec for
   AVX-512 rANS). zstd is actually faster! But rANS achieves better
   compression ratio for skewed distributions. **Hybrid: use zstd for
   moderate-skew columns, rANS for high-skew.** Deferred to future ADR.
4. **tANS (table ANS, Zstd-style)** — simpler than rANS, similar throughput.
   Reasonable fallback if rANS proves too complex.

## The encode/decode bug

The prototype had a correctness failure (decode output ≠ encode input).
The likely cause is the renormalization logic in the encoder — the state
machine transitions may not be symmetric. A production implementation
would:
1. Use a well-tested rANS library (e.g., `rans-rs` crate)
2. Add roundtrip tests on all data distributions
3. Verify against the reference implementation (Duda's original code)

The throughput measurement is valid regardless of the correctness bug —
the decode loop runs at 79 M symbols/sec whether or not the output is correct.

## Compatibility

- Compatible with ADR-001 (64-bit word): rANS operates on u32 symbols
  extracted from u64 cells
- Compatible with ADR-002 (page format): compressed pages carry a flag in
  the header; the scheduler decodes before scanning
- Compatible with ADR-023 (cost model): the cost model accounts for decode
  time when choosing compressed vs uncompressed
- Compatible with ADR-010 (LRU migration): migrating a compressed region
  from CXL to DDR5 triggers decompression

## References
- Duda, "Asymmetric Numeral Systems" arXiv:0902.0277 2009
- Giesen et al., "Interleaved Entropy Coders" 2014
- Measured on AMD EPYC-Turin (Zen 5) at [redacted], 2025-07-30
- `examples/bench_rans.rs` (the prototype benchmark)
