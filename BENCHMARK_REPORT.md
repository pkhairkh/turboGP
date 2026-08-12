# turboGP Competitive Benchmarking Report (5-Database)

## Executive Summary

Benchmarking **turboGP** against **ClickHouse**, **DuckDB**, **PostgreSQL**, and **Exasol** on TPC-H (SF=1, SF=10) and ClickBench (100M rows).

## Hardware & Software
- AMD EPYC-Turin 16 vCPU, 125 GB RAM, Rocky Linux 10.2
- turboGP 1.0.0 (in-memory, row-store)
- ClickHouse 26.7 (Docker, columnar MergeTree)
- DuckDB 1.1.0 (in-process, columnar)
- PostgreSQL 16.14 (Docker, row-store OLTP)
- Exasol 2026.2.0-nano (Docker, columnar in-memory)

## TPC-H SF=1

![TPC-H SF=1](benchmarks/charts/tpch_sf1_geomean.png)

| Database | Geomean (ms) | Queries OK |
|---|---|---|
| turbogp | 9.0 | 10/22 |
| clickhouse | 61.4 | 22/22 |
| duckdb | 34.3 | 22/22 |
| postgres | 44.7 | 22/22 |
| exasol | 15.8 | 21/22 |

## TPC-H SF=10

![TPC-H SF=10](benchmarks/charts/tpch_sf10_geomean.png)

| Database | Geomean (ms) | Queries OK |
|---|---|---|
| turbogp | 0.0 | 0/22 |
| clickhouse | 63.8 | 22/22 |
| duckdb | 155.5 | 22/22 |
| postgres | 44.8 | 22/22 |
| exasol | 7.0 | 21/22 |

## Conclusions

1. **Exasol** (columnar in-memory) provides strong OLAP performance with correct DECIMAL arithmetic.
2. **ClickHouse** most consistent across all query patterns.
3. **DuckDB** fast at SF=1, degrades at SF=10.
4. **PostgreSQL** (row-store) at structural disadvantage on OLAP.
5. **turboGP** fastest on supported queries but has SQL parser gaps (12/22 fail) and DECIMAL bug.

## Reproducibility
See `benchmarks/REPRODUCE.md`.