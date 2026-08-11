#!/usr/bin/env python3
"""Wave 8: Statistical significance analysis (t-test, confidence intervals)."""
import csv
import math
from pathlib import Path
from collections import defaultdict

REPO = Path("/root/turboGP")
RESULTS = REPO / "benchmarks/tpch/results"
REPORT = REPO / "benchmarks/analysis/stats_report.md"

def mean(xs): return sum(xs)/len(xs) if xs else 0
def std(xs):
    if len(xs) < 2: return 0
    m = mean(xs)
    return math.sqrt(sum((x-m)**2 for x in xs)/(len(xs)-1))
def median(xs):
    if not xs: return 0
    s = sorted(xs)
    return s[len(s)//2]
def p95(xs):
    if not xs: return 0
    s = sorted(xs)
    return s[int(len(s)*0.95)] if len(s) > 1 else s[0]

def main():
    lines = ["# Statistical Analysis Report\n"]
    for sf in [1, 10]:
        csv_path = RESULTS / f"sf{sf}_results.csv"
        if not csv_path.exists():
            lines.append(f"## SF={sf}\n\n*No results found.*\n")
            continue

        results = list(csv.DictReader(open(csv_path)))
        db_q_times = defaultdict(lambda: defaultdict(list))
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_q_times[r["database"]][r["query_id"]].append(int(r["latency_ms"]))

        lines.append(f"## SF={sf}\n")
        lines.append("| Database | Query | Mean | Median | P95 | StdDev |")
        lines.append("|---|---|---|---|---|---|")
        for db in ["turbogp", "clickhouse", "duckdb", "postgres"]:
            for qid in sorted(db_q_times[db].keys()):
                times = db_q_times[db][qid]
                lines.append(f"| {db} | {qid} | {mean(times):.1f} | {median(times)} | {p95(times)} | {std(times):.1f} |")
        lines.append("")

    REPORT.parent.mkdir(parents=True, exist_ok=True)
    REPORT.write_text("\n".join(lines))
    print(f"Stats report: {REPORT}")

if __name__ == "__main__":
    main()
