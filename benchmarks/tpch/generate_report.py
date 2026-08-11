#!/usr/bin/env python3
"""Wave 8/9: Generate TPC-H results report with tables and charts."""
import csv
import os
import sys
from pathlib import Path
from collections import defaultdict
import math

REPO = Path("/root/turboGP")
RESULTS_DIR = REPO / "benchmarks/tpch/results"
REPORT = RESULTS_DIR / "REPORT.md"

def geomean(values):
    """Geometric mean of a list of positive values."""
    vals = [v for v in values if v > 0]
    if not vals:
        return 0
    return math.exp(sum(math.log(v) for v in vals) / len(vals))

def load_results(csv_path):
    """Load benchmark results from CSV."""
    results = []
    if not csv_path.exists():
        return results
    with open(csv_path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            row["latency_ms"] = int(row["latency_ms"])
            results.append(row)
    return results

def generate_report():
    """Generate TPC-H REPORT.md with summary tables."""
    lines = ["# TPC-H Benchmark Results\n"]

    for sf in [1, 10]:
        csv_path = RESULTS_DIR / f"sf{sf}_results.csv"
        results = load_results(csv_path)
        if not results:
            lines.append(f"## SF={sf}\n\n*No results found.*\n")
            continue

        # Compute geomean per database (hot runs only, median of 3)
        db_query_times = defaultdict(lambda: defaultdict(list))
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_query_times[r["database"]][r["query_id"]].append(r["latency_ms"])

        lines.append(f"## SF={sf}\n")
        lines.append("### Summary (Geomean of Hot Run Medians, ms)\n")
        lines.append("| Database | Geomean (ms) | Queries Completed |")
        lines.append("|---|---|---|")

        for db in ["turbogp", "clickhouse", "duckdb", "postgres"]:
            medians = []
            completed = 0
            for qid, times in db_query_times[db].items():
                if times:
                    medians.append(sorted(times)[len(times)//2])
                    completed += 1
            gm = geomean(medians) if medians else 0
            lines.append(f"| {db} | {gm:.1f} | {completed}/22 |")

        lines.append("")

        # Per-query comparison
        lines.append("### Per-Query Comparison (Hot Run Median, ms)\n")
        lines.append("| Query | turboGP | ClickHouse | DuckDB | PostgreSQL |")
        lines.append("|---|---|---|---|---|")
        for qnum in range(1, 23):
            qid = f"q{qnum:02d}"
            row = [qid]
            for db in ["turbogp", "clickhouse", "duckdb", "postgres"]:
                times = db_query_times[db].get(qid, [])
                if times:
                    median = sorted(times)[len(times)//2]
                    row.append(f"{median}")
                else:
                    row.append("—")
            lines.append("| " + " | ".join(row) + " |")

        lines.append("")

    REPORT.write_text("\n".join(lines))
    print(f"Report written to {REPORT}")

if __name__ == "__main__":
    generate_report()
