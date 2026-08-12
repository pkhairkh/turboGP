# Reproduction Guide (5-Database)

## Prerequisites
- Linux: 8+ vCPU, 32+ GB RAM, 200+ GB SSD
- Docker, Python 3.11+, Rust 1.97+, Git
- KVM support (for Exasol)

## Databases
1. ClickHouse: `docker run -d --name clickhouse --network host clickhouse/clickhouse-server:latest`
2. PostgreSQL: `docker run -d --name postgres --network host -e POSTGRES_PASSWORD=postgres postgres:16 -c port=5433`
3. Exasol: `docker run -d --name exasol --network host --privileged exasol/nano:latest`
4. DuckDB: `wget ... && install to /usr/local/bin/duckdb`
5. turboGP: `cargo build --release`

## Steps
1. `git clone ... && cd turboGP && git checkout feat/benchmarking-v2`
2. Generate TPC-H data: `bash benchmarks/tpch/generate_data.sh`
3. Load data into all 5 databases: `python3 benchmarks/tpch/load_tpch.py --sf 1` + `--sf 10` + `python3 benchmarks/tpch/load_exasol.py --sf 1 10`
4. Generate queries: `python3 benchmarks/tpch/generate_queries.py`
5. Run benchmark: `python3 benchmarks/tpch/run_benchmark.py --sf 1 10`
6. ClickBench: `python3 benchmarks/clickbench/generate_data.py && python3 benchmarks/clickbench/run_benchmark.py`
7. Report: `python3 benchmarks/analysis/generate_report.py`
