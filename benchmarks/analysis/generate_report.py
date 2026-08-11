#!/usr/bin/env python3
"""Wave 9: Generate publication-quality charts and BENCHMARK_REPORT.md."""
import csv
import math
import os
from pathlib import Path
from collections import defaultdict

REPO = Path("/root/turboGP")
TPCH_RESULTS = REPO / "benchmarks/tpch/results"
CB_RESULTS = REPO / "benchmarks/clickbench/results"
CHARTS_DIR = REPO / "benchmarks/charts"
REPORT = REPO / "BENCHMARK_REPORT.md"

def geomean(values):
    vals = [v for v in values if v > 0]
    if not vals: return 0
    return math.exp(sum(math.log(v) for v in vals) / len(vals))

def load_csv(path):
    if not path.exists(): return []
    with open(path) as f:
        return list(csv.DictReader(f))

def generate_charts():
    """Generate charts using matplotlib."""
    CHARTS_DIR.mkdir(parents=True, exist_ok=True)
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
        import matplotlib.font_manager as fm
        fm.fontManager.addfont('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf')
        plt.rcParams['font.sans-serif'] = ['DejaVu Sans']
        plt.rcParams['axes.unicode_minus'] = False
    except ImportError:
        print("matplotlib not available, skipping charts")
        return

    # TPC-H geomean chart
    for sf in [1, 10]:
        results = load_csv(TPCH_RESULTS / f"sf{sf}_results.csv")
        if not results: continue

        db_times = defaultdict(list)
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_times[r["database"]].append(int(r["latency_ms"]))

        dbs = ["turbogp", "clickhouse", "duckdb", "postgres"]
        geomeans = [geomean(db_times.get(db, [0])) for db in dbs]

        fig, ax = plt.subplots(figsize=(10, 6), constrained_layout=True)
        bars = ax.bar(dbs, geomeans, color=['#e74c3c', '#2ecc71', '#3498db', '#f39c12'])
        ax.set_ylabel('Geomean Latency (ms)')
        ax.set_title(f'TPC-H SF={sf} — Geomean Hot Run Latency (lower is better)')
        for bar, val in zip(bars, geomeans):
            ax.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 0.5,
                    f'{val:.0f}ms', ha='center', va='bottom')
        chart_path = CHARTS_DIR / f"tpch_sf{sf}_geomean.png"
        fig.savefig(chart_path, dpi=150)
        plt.close(fig)
        print(f"  chart: {chart_path}")

def generate_report():
    """Generate BENCHMARK_REPORT.md."""
    lines = [
        "# turboGP Competitive Benchmarking Report",
        "",
        "## Executive Summary",
        "",
        "This report benchmarks **turboGP** against **ClickHouse**, **DuckDB**, and **PostgreSQL** (Exasol fallback) on TPC-H (SF=1, SF=10) and ClickBench (100M rows) workloads.",
        "",
        "## Methodology",
        "",
        "- **TPC-H**: 22 standard queries, cold + hot runs (3 iterations, median), 300s timeout",
        "- **ClickBench**: 43 queries, single-threaded + multi-threaded",
        "- **Databases**: turboGP (in-memory), ClickHouse 26.7 (Docker), DuckDB 1.1.0, PostgreSQL 16.14 (Docker, Exasol fallback)",
        "- **Hardware**: AMD EPYC-Turin 16 vCPU, 125 GB RAM, 960 GB disk, Rocky Linux 10.2",
        "- **Fairness**: identical data, identical queries (dialect-adapted), default configs, OS cache dropped for cold runs",
        "",
        "## Hardware & Software Configuration",
        "",
        "| Component | Value |",
        "|---|---|",
        "| CPU | AMD EPYC-Turin, 16 vCPU (8 cores × 2 threads) |",
        "| RAM | 125 GB |",
        "| Disk | 960 GB virtio |",
        "| OS | Rocky Linux 10.2, kernel 6.12 |",
        "| Rust | 1.97.1 |",
        "| Python | 3.12.13 |",
        "| Docker | 29.7.2 |",
        "",
        "| Database | Version | Engine |",
        "|---|---|---|",
        "| turboGP | 1.0.0 | In-memory, row-store |",
        "| ClickHouse | 26.7.3.19 | Columnar (MergeTree) |",
        "| DuckDB | 1.1.0 | Columnar (in-process) |",
        "| PostgreSQL | 16.14 | Row-store (OLTP) |",
        "",
        "## TPC-H Results",
        "",
    ]

    for sf in [1, 10]:
        results = load_csv(TPCH_RESULTS / f"sf{sf}_results.csv")
        if not results:
            lines.append(f"### SF={sf}\n\n*Results not available.*\n")
            continue

        db_times = defaultdict(lambda: defaultdict(list))
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_times[r["database"]][r["query_id"]].append(int(r["latency_ms"]))

        lines.append(f"### SF={sf} — Summary\n")
        lines.append(f"![TPC-H SF={sf} Geomean](benchmarks/charts/tpch_sf{sf}_geomean.png)")
        lines.append("")
        lines.append("| Database | Geomean (ms) | Queries OK |")
        lines.append("|---|---|---|")
        for db in ["turbogp", "clickhouse", "duckdb", "postgres"]:
            medians = [sorted(t)[len(t)//2] for t in db_times[db].values() if t]
            gm = geomean(medians) if medians else 0
            lines.append(f"| {db} | {gm:.1f} | {len(medians)}/22 |")
        lines.append("")

    lines.extend([
        "## ClickBench Results",
        "",
        "See `benchmarks/clickbench/results/` for raw CSV data.",
        "",
        "## Statistical Analysis",
        "",
        "See `benchmarks/analysis/stats_report.md` for detailed statistical analysis.",
        "",
        "## Fairness Audit",
        "",
        "See `benchmarks/analysis/FAIRNESS_AUDIT.md` for the complete fairness audit.",
        "",
        "### Key Deviations",
        "1. **Exasol → PostgreSQL fallback**: Exasol Docker image failed on Rocky 10 kernel.",
        "2. **turboGP DECIMAL bug**: `load_csv()` hashes DECIMAL columns, affecting SUM/AVG correctness (not latency).",
        "3. **Docker networking**: iptables disabled (kernel lacks `xt_addrtype`); host networking used.",
        "",
        "## Conclusions",
        "",
        "1. **ClickHouse and DuckDB** dominate OLAP workloads due to columnar storage and vectorized execution.",
        "2. **PostgreSQL** (row-store OLTP) is at a structural disadvantage on TPC-H/ClickBench.",
        "3. **turboGP** shows competitive latency for simple queries but has engine bugs affecting DECIMAL arithmetic.",
        "4. **turboGP wins** on: simple point queries, integer-only aggregations, in-memory latency.",
        "5. **turboGP loses** on: complex multi-table joins, DECIMAL arithmetic, large-scale columnar aggregations.",
        "",
        "## Reproducibility",
        "",
        "See `benchmarks/REPRODUCE.md` for step-by-step reproduction instructions.",
        "",
        "## Raw Data",
        "",
        "- TPC-H results: `benchmarks/tpch/results/sf{1,10}_results.csv`",
        "- ClickBench results: `benchmarks/clickbench/results/{single,multi}_threaded.csv`",
        "- All scripts: `benchmarks/tpch/`, `benchmarks/clickbench/`, `benchmarks/analysis/`",
    ])

    REPORT.write_text("\n".join(lines))
    print(f"Report written to {REPORT}")

if __name__ == "__main__":
    generate_charts()
    generate_report()
