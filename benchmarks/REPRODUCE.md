# Reproduction Guide

## Prerequisites
- Linux: 8+ vCPU, 32+ GB RAM, 200+ GB SSD
- Docker, Python 3.11+, Rust 1.97+, Git

## Steps
1. Clone: `git clone https://github.com/pkhairkh/turboGP.git && cd turboGP && git checkout feat/benchmarking`
2. Start databases: ClickHouse (Docker), DuckDB (native), PostgreSQL (Docker, port 5433)
3. Build turboGP: `cargo build --release`
4. Generate TPC-H data: `bash benchmarks/tpch/generate_data.sh`
5. Load data: `python3 benchmarks/tpch/load_tpch.py --sf 1` and `--sf 10`
6. Generate queries: `python3 benchmarks/tpch/generate_queries.py`
7. Run benchmark: `python3 benchmarks/tpch/run_benchmark.py --sf 1` and `--sf 10`
8. Generate report: `python3 benchmarks/analysis/generate_report.py`

## Expected Runtime
- Data generation: 30 min
- Data loading: 30 min per SF
- Benchmark: 2-4 hours
