# Fairness Audit

## Hardware
- CPU: AMD EPYC-Turin, 16 vCPU (1 socket × 8 cores × 2 threads)
- RAM: 125 GB
- Disk: 960 GB virtio
- OS: Rocky Linux 10.2, kernel 6.12

## Database Versions
- turboGP: 1.0.0 (release build, LTO)
- ClickHouse: 26.7.3.19 (Docker)
- DuckDB: v1.1.0
- PostgreSQL: 16.14 (Docker) — fallback for Exasol

## Configuration Deviations
1. Docker networking: iptables disabled (kernel lacks xt_addrtype); host networking used.
2. Exasol → PostgreSQL fallback: Exasol Docker image failed on Rocky 10 kernel.
3. turboGP COPY FROM patches: --allow-copy-dir CLI flag; load_csv fast path.
4. Known limitation: turboGP load_csv hashes DECIMAL columns, affecting SUM/AVG correctness (not latency).
5. PostgreSQL: shared_buffers=4GB, port 5433 (avoids turboGP conflict).

## Data Verification
- TPC-H SF=1: 8 tables, lineitem=6,001,215 rows ✓
- TPC-H SF=10: 8 tables, lineitem=59,986,052 rows ✓
- ClickBench: 100M rows (if generated)
