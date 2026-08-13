#!/usr/bin/env python3
"""Generate a comparison report from the benchmark CSVs."""
import csv
import os
from datetime import datetime

REPO = "/root/turboGP"
RESULTS = f"{REPO}/benchmarks/results"

def read_turbogp_tpch(path):
    """turboGP TPC-H CSV: query,cold_us,cold_rows,cold_status,..."""
    results = {}
    with open(path) as f:
        r = csv.DictReader(f)
        for row in r:
            q = row.get("query_id", row.get("query", ""))
            ms = float(row.get("cold_us", 0)) / 1000.0
            status = row.get("cold_status", "OK")
            results[q] = (ms, status)
    return results

def read_generic(path):
    """Generic CSV: query,cold_ms,status,rows OR query_id,cold_us,...,cold_status"""
    results = {}
    with open(path) as f:
        r = csv.DictReader(f)
        for row in r:
            q = row.get("query", row.get("query_id", ""))
            # Handle turboGP format (cold_us in microseconds) vs generic (cold_ms)
            if "cold_us" in row:
                ms = float(row["cold_us"]) / 1000.0
                status = row.get("cold_status", "OK")
            else:
                ms = float(row.get("cold_ms", 0))
                status = row.get("status", "OK")
            results[q] = (ms, status)
    return results

def geomean(values):
    if not values:
        return 0
    product = 1.0
    for v in values:
        product *= v
    return product ** (1.0 / len(values))

def main():
    report = []
    report.append("# turboGP vs DuckDB vs Exasol — Full Benchmark Comparison")
    report.append(f"\n**Generated:** {datetime.now().strftime('%Y-%m-%d %H:%M:%S UTC')}")
    report.append(f"**Sandbox:** root@192.248.158.130 (AMD EPYC-Turin, 125GB RAM)")
    report.append(f"**turboGP commit:** 327f436 (post-Q9 bloom pushdown fix)")
    report.append("")

    # === TPC-H SF=1 ===
    report.append("## TPC-H SF=1 (22 queries, sequential execution)")
    report.append("")

    tg = read_turbogp_tpch(f"{RESULTS}/turbogp_tpch_sf1.csv")
    duck = read_generic(f"{RESULTS}/duckdb_tpch_sf1.csv")
    exa = read_generic(f"{RESULTS}/exasol_tpch_sf1.csv")

    report.append("| Query | turboGP (ms) | DuckDB (ms) | Exasol (ms) | turboGP/DuckDB | turboGP/Exasol |")
    report.append("|-------|-------------|-------------|-------------|----------------|----------------|")

    tg_times = []
    duck_times = []
    exa_times = []

    for i in range(1, 23):
        q = f"q{i:02d}"
        tg_ms, tg_st = tg.get(q, (0, "MISSING"))
        duck_ms, duck_st = duck.get(q, (0, "MISSING"))
        exa_ms, exa_st = exa.get(q, (0, "MISSING"))

        if "OK" in tg_st:
            tg_times.append(tg_ms)
        if "OK" in duck_st:
            duck_times.append(duck_ms)
        if "OK" in exa_st:
            exa_times.append(exa_ms)

        ratio_duck = f"{tg_ms/duck_ms:.2f}x" if duck_ms > 0 and "OK" in duck_st else "—"
        ratio_exa = f"{tg_ms/exa_ms:.2f}x" if exa_ms > 0 and "OK" in exa_st else "—"

        tg_str = f"{tg_ms:.1f}" if "OK" in tg_st else f"ERR"
        duck_str = f"{duck_ms:.1f}" if "OK" in duck_st else f"ERR"
        exa_str = f"{exa_ms:.1f}" if "OK" in exa_st else f"ERR"

        report.append(f"| {q} | {tg_str} | {duck_str} | {exa_str} | {ratio_duck} | {ratio_exa} |")

    tg_gm = geomean(tg_times)
    duck_gm = geomean(duck_times)
    exa_gm = geomean(exa_times)

    report.append(f"| **Geomean** | **{tg_gm:.1f}** | **{duck_gm:.1f}** | **{exa_gm:.1f}** | **{tg_gm/duck_gm:.2f}x** | **{tg_gm/exa_gm:.2f}x** |")
    report.append(f"| **Queries OK** | **{len(tg_times)}/22** | **{len(duck_times)}/22** | **{len(exa_times)}/22** | | |")
    report.append("")

    # === ClickBench ===
    report.append("## ClickBench (43 queries, sequential execution)")
    report.append("")

    tg_cb = read_generic(f"{RESULTS}/turbogp_clickbench.csv")
    duck_cb = read_generic(f"{RESULTS}/duckdb_clickbench.csv")
    exa_cb = read_generic(f"{RESULTS}/exasol_clickbench.csv")

    report.append("| Query | turboGP (ms) | DuckDB (ms) | Exasol (ms) | turboGP/DuckDB | turboGP/Exasol |")
    report.append("|-------|-------------|-------------|-------------|----------------|----------------|")

    tg_cb_times = []
    duck_cb_times = []
    exa_cb_times = []

    for i in range(1, 44):
        q = f"q{i:02d}"
        tg_ms, tg_st = tg_cb.get(q, (0, "MISSING"))
        duck_ms, duck_st = duck_cb.get(q, (0, "MISSING"))
        exa_ms, exa_st = exa_cb.get(q, (0, "MISSING"))

        if "OK" in tg_st:
            tg_cb_times.append(tg_ms)
        if "OK" in duck_st:
            duck_cb_times.append(duck_ms)
        if "OK" in exa_st:
            exa_cb_times.append(exa_ms)

        ratio_duck = f"{tg_ms/duck_ms:.2f}x" if duck_ms > 0 and "OK" in duck_st else "—"
        ratio_exa = f"{tg_ms/exa_ms:.2f}x" if exa_ms > 0 and "OK" in exa_st else "—"

        tg_str = f"{tg_ms:.1f}" if "OK" in tg_st else f"ERR"
        duck_str = f"{duck_ms:.1f}" if "OK" in duck_st else f"ERR"
        exa_str = f"{exa_ms:.1f}" if "OK" in exa_st else f"ERR"

        report.append(f"| {q} | {tg_str} | {duck_str} | {exa_str} | {ratio_duck} | {ratio_exa} |")

    tg_cb_gm = geomean(tg_cb_times)
    duck_cb_gm = geomean(duck_cb_times)
    exa_cb_gm = geomean(exa_cb_times)

    report.append(f"| **Geomean** | **{tg_cb_gm:.1f}** | **{duck_cb_gm:.1f}** | **{exa_cb_gm:.1f}** | **{tg_cb_gm/duck_cb_gm:.2f}x** | **{tg_cb_gm/exa_cb_gm:.2f}x** |")
    report.append(f"| **Queries OK** | **{len(tg_cb_times)}/43** | **{len(duck_cb_times)}/43** | **{len(exa_cb_times)}/43** | | |")
    report.append("")

    # === Summary ===
    report.append("## Summary")
    report.append("")
    report.append("| Benchmark | turboGP | DuckDB | Exasol | turboGP vs DuckDB | turboGP vs Exasol |")
    report.append("|-----------|---------|--------|--------|-------------------|-------------------|")
    report.append(f"| TPC-H SF=1 cold geomean | {tg_gm:.1f}ms | {duck_gm:.1f}ms | {exa_gm:.1f}ms | {tg_gm/duck_gm:.2f}x | {tg_gm/exa_gm:.2f}x |")
    report.append(f"| ClickBench cold geomean | {tg_cb_gm:.1f}ms | {duck_cb_gm:.1f}ms | {exa_cb_gm:.1f}ms | {tg_cb_gm/duck_cb_gm:.2f}x | {tg_cb_gm/exa_cb_gm:.2f}x |")
    report.append("")

    report_text = "\n".join(report)
    print(report_text)

    with open(f"{REPO}/BENCHMARK_COMPARISON.md", "w") as f:
        f.write(report_text)
    print(f"\nReport saved to {REPO}/BENCHMARK_COMPARISON.md")

if __name__ == "__main__":
    main()
