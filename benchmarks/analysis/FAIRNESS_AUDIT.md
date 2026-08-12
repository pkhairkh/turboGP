# Fairness Audit — 5-Database Benchmark

## Hardware
- CPU: AMD EPYC-Turin, 16 vCPU (1 socket × 8 cores × 2 threads)
- RAM: 125 GB
- Disk: 960 GB virtio
- OS: Rocky Linux 10.2, kernel 6.12

## Database Versions
- turboGP: 1.0.0 (release build, LTO, in-memory)
- ClickHouse: 26.7.3.19 (Docker, MergeTree columnar)
- DuckDB: v1.1.0 (in-process, columnar)
- PostgreSQL: 16.14 (Docker, row-store OLTP)
- Exasol: 2026.2.0-nano (Docker exasol/nano, columnar in-memory)

## Configuration
1. Docker networking: iptables disabled (kernel lacks xt_addrtype); host networking used.
2. Exasol: exasol/nano image (lightweight single-node dev edition), port 8563, pyexasol client with self-signed cert.
3. PostgreSQL: shared_buffers=4GB, port 5433.
4. turboGP: --insecure --port 55432, --allow-copy-dir for benchmark CSV loading.

## Data Verification
- TPC-H SF=1: all 8 tables in all 5 databases. lineitem = 6,001,215 rows ✓
- TPC-H SF=10: all 8 tables. lineitem = 59,986,052 rows ✓
- ClickBench: 100M rows (if generated)

## Known Issues
1. turboGP DECIMAL bug: load_csv() hashes DECIMAL columns (f64::to_bits as u64). SUM/AVG on DECIMAL return incorrect values. Latency unaffected.
2. turboGP SQL parser: 12/22 TPC-H queries fail (CTEs, EXISTS, complex subqueries).
3. Exasol nano: single-node dev edition, not production-grade. No built-in exaplus client (used pyexasol WebSocket instead).
