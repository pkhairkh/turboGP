#!/usr/bin/env python3
"""Generate ClickBench report with 5-database comparison."""
import csv, math, os
from pathlib import Path
from collections import defaultdict

REPO = Path("/root/turboGP")
CB_RESULTS = REPO / "benchmarks/clickbench/results"
CHARTS = REPO / "benchmarks/charts"
ANALYSIS = REPO / "benchmarks/analysis"

DATABASES = ["turbogp", "clickhouse", "duckdb", "postgres", "exasol"]
COLORS = ['#e74c3c', '#2ecc71', '#3498db', '#f39c12', '#9b59b6']

def geomean(v):
    v = [x for x in v if x > 0]
    return math.exp(sum(math.log(x) for x in v)/len(v)) if v else 0

def main():
    results = list(csv.DictReader(open(CB_RESULTS / "clickbench_results.csv")))
    db_q = defaultdict(lambda: defaultdict(list))
    for r in results:
        if r["mode"].startswith("hot") and r["status"] == "OK":
            db_q[r["database"]][r["query_id"]].append(int(r["latency_ms"]))

    # Compute medians
    db_medians = {}
    for db in DATABASES:
        medians = {}
        for qid, times in db_q[db].items():
            if times:
                medians[qid] = sorted(times)[len(times)//2]
        db_medians[db] = medians

    # Print summary
    print("\n=== ClickBench Summary (100M rows) ===")
    print("Database       Geomean(ms)  Queries OK")
    for db in DATABASES:
        medians = list(db_medians[db].values())
        gm = geomean(medians)
        print(f"{db:14s} {gm:10.1f}  {len(medians)}/43")

    # Generate charts
    CHARTS.mkdir(parents=True, exist_ok=True)
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
        plt.rcParams['font.sans-serif'] = ['DejaVu Sans']
        plt.rcParams['axes.unicode_minus'] = False

        # Geomean bar chart
        gms = [geomean(list(db_medians.get(db, {}).values())) for db in DATABASES]
        fig, ax = plt.subplots(figsize=(12, 7), constrained_layout=True)
        bars = ax.bar(DATABASES, gms, color=COLORS)
        ax.set_ylabel('Geomean Latency (ms)')
        ax.set_title('ClickBench 100M rows — Geomean Hot Run Latency (lower is better)')
        for b, v in zip(bars, gms):
            if v > 0: ax.text(b.get_x()+b.get_width()/2, b.get_height()+0.5, f'{v:.1f}ms', ha='center')
        ax.set_yscale('log')
        fig.savefig(CHARTS / "clickbench_geomean.png", dpi=150)
        plt.close(fig)
        print(f"  chart: clickbench_geomean.png")

        # Per-query chart
        queries = sorted(set(q for db in DATABASES for q in db_medians.get(db, {})))
        fig, ax = plt.subplots(figsize=(18, 8), constrained_layout=True)
        x = range(len(queries))
        width = 0.15
        for i, db in enumerate(DATABASES):
            vals = [db_medians.get(db, {}).get(q, 0) for q in queries]
            ax.bar([xi + i*width for xi in x], vals, width, label=db, color=COLORS[i])
        ax.set_xlabel('Query')
        ax.set_ylabel('Latency (ms)')
        ax.set_title('ClickBench 100M rows — Per-Query Latency (lower is better)')
        ax.set_xticks([xi + 2*width for xi in x])
        ax.set_xticklabels(queries, rotation=45)
        ax.legend()
        ax.set_yscale('log')
        fig.savefig(CHARTS / "clickbench_perquery.png", dpi=150)
        plt.close(fig)
        print(f"  chart: clickbench_perquery.png")
    except Exception as e:
        print(f"  charts failed: {e}")

    # Generate report section
    lines = ["## ClickBench Results (100M rows)\n"]
    lines.append("![ClickBench Geomean](benchmarks/charts/clickbench_geomean.png)\n")
    lines.append("![ClickBench Per-Query](benchmarks/charts/clickbench_perquery.png)\n")
    lines.append("### Summary (Geomean of Hot Run Medians, ms — lower is better)\n")
    lines.append("| Database | Geomean (ms) | Queries OK |")
    lines.append("|---|---|---|")
    for db in DATABASES:
        medians = list(db_medians.get(db, {}).values())
        gm = geomean(medians)
        lines.append(f"| {db} | {gm:.1f} | {len(medians)}/43 |")
    lines.append("")

    # Per-query table
    lines.append("### Per-Query Latency (Hot Run Median, ms)\n")
    header = "| Query | " + " | ".join(DATABASES) + " |"
    sep = "|---|" + "|".join(["---"] * len(DATABASES)) + "|"
    lines.append(header)
    lines.append(sep)
    for qnum in range(1, 44):
        qid = f"q{qnum:02d}"
        row = [qid]
        for db in DATABASES:
            v = db_medians.get(db, {}).get(qid)
            row.append(f"{v}" if v else "—")
        lines.append("| " + " | ".join(row) + " |")
    lines.append("")

    # Append to BENCHMARK_REPORT.md
    report = REPO / "BENCHMARK_REPORT.md"
    if report.exists():
        existing = report.read_text()
        # Find the ClickBench section and replace it, or append
        if "## ClickBench Results" in existing:
            idx = existing.find("## ClickBench Results")
            # Find the next ## section or end
            next_idx = existing.find("\n## ", idx + 1)
            if next_idx < 0: next_idx = len(existing)
            report.write_text(existing[:idx] + "\n".join(lines) + existing[next_idx:])
        else:
            report.write_text(existing + "\n" + "\n".join(lines))
    else:
        report.write_text("\n".join(lines))
    print(f"  report: BENCHMARK_REPORT.md updated")

if __name__ == "__main__":
    main()
