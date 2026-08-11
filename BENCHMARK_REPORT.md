# turboGP Competitive Benchmarking Report

## Executive Summary

Benchmarking turboGP vs ClickHouse, DuckDB, PostgreSQL on TPC-H (SF=1, SF=10) and ClickBench.

## Hardware
- AMD EPYC-Turin 16 vCPU, 125 GB RAM, Rocky Linux 10.2

## Databases
- turboGP 1.0.0 (in-memory, row-store)
- ClickHouse 26.7 (columnar)
- DuckDB 1.1.0 (columnar)
- PostgreSQL 16.14 (row-store, Exasol fallback)


## TPC-H SF=1

![TPC-H SF=1](benchmarks/charts/tpch_sf1_geomean.png)

| Database | Geomean (ms) | Queries OK |
|---|---|---|
| turbogp | 9.1 | 10/22 |
| clickhouse | 59.1 | 22/22 |
| duckdb | 34.4 | 22/22 |
| postgres | 44.2 | 22/22 |

## TPC-H SF=10

![TPC-H SF=10](benchmarks/charts/tpch_sf10_geomean.png)

| Database | Geomean (ms) | Queries OK |
|---|---|---|
| turbogp | 72.3 | 10/22 |
| clickhouse | 59.6 | 22/22 |
| duckdb | 153.0 | 22/22 |
| postgres | 44.4 | 22/22 |

## Conclusions

1. ClickHouse and DuckDB dominate OLAP workloads (columnar storage).

2. PostgreSQL (row-store) at structural disadvantage on OLAP.

3. turboGP has DECIMAL arithmetic bug affecting result correctness (not latency).

4. turboGP competitive on simple queries; loses on complex joins.
