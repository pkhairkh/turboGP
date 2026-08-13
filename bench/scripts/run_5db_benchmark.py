#!/usr/bin/env python3
"""Modified 5-database benchmark that skips Exasol loading (already loaded in previous run).
Only runs the TPC-H queries on all 5 databases.
"""
import csv
import math
import os
import re
import ssl
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

REPO = Path("/root/turboGP")
TURBOGP = REPO / "target/release/turbogp"
TPCH_QUERIES = REPO / "benchmarks/tpch/queries"
TPCH_RESULTS = REPO / "benchmarks/tpch/results"
CHARTS = REPO / "benchmarks/charts"
ANALYSIS = REPO / "benchmarks/analysis"

DATABASES = ["turbogp", "clickhouse", "duckdb", "postgres", "exasol"]
NUM_TPCH = 22
TIMEOUT = 300

import pyexasol
import pyexasol.connection as pc
from packaging.version import Version, InvalidVersion
def _patched_version(self):
    rv = self.login_info.get('releaseVersion')
    if rv:
        try: return Version(rv)
        except InvalidVersion: return Version(re.sub(r'-.*$', '', rv))
    return None
pc.ExaConnection.exasol_db_version = property(_patched_version)

def exasol_connect():
    return pyexasol.connect(
        dsn="localhost:8563", user="sys", password="exasol",
        websocket_sslopt={"cert_reqs": ssl.CERT_NONE},
    )

def run(cmd, timeout=TIMEOUT):
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=timeout, capture_output=True, text=True)
        return p.returncode, p.stdout, p.stderr, int((time.time()-t0)*1000)
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)
    except Exception as e:
        return 1, "", str(e), int((time.time()-t0)*1000)

def drop_caches():
    subprocess.run("sync; echo 3 > /proc/sys/vm/drop_caches", shell=True, timeout=10)
    time.sleep(1)

def start_turbogp(sf):
    subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
    time.sleep(2)
    csv_dir = f"/srv/turbogp_csv/sf{sf}"
    proc = subprocess.Popen(
        [str(TURBOGP), "--insecure", "--port", "55432", "--max-connections", "16",
         "--allow-copy-dir", csv_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    if proc.poll() is not None:
        raise RuntimeError(f"turboGP failed to start for SF={sf}")
    print(f"  turboGP started (PID={proc.pid}) for SF={sf}")
    schema = (TPCH_QUERIES.parent / "schema" / "turbogp.sql").read_text()
    schema_file = "/tmp/turbogp_schema.sql"
    with open(schema_file, "w") as f: f.write(schema)
    subprocess.run(f"psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -f {schema_file}",
                   shell=True, capture_output=True, timeout=30)
    for tbl in ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]:
        csv_path = f"{csv_dir}/{tbl}.csv"
        subprocess.run(
            f"psql -h 127.0.0.1 -p 55432 -U postgres -c \"COPY {tbl} FROM '{csv_path}' WITH (FORMAT csv, HEADER true)\"",
            shell=True, capture_output=True, timeout=600,
        )
    return proc

def stop_turbogp(proc):
    if proc:
        proc.terminate()
        try: proc.wait(timeout=5)
        except: proc.kill()

def run_query(db, sql, sf):
    # Strip comment-only lines
    sql_lines = [line for line in sql.split('\n') if not line.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines)
    sql_escaped = sql_clean.replace('"', "'").replace('\n', ' ').strip()
    if db == "turbogp":
        cmd = f"psql -h 127.0.0.1 -p 55432 -U postgres -tAc \"{sql_escaped}\""
        return run(cmd)
    elif db == "clickhouse":
        cmd = f"docker exec clickhouse clickhouse-client --query \"{sql_escaped}\""
        return run(cmd)
    elif db == "duckdb":
        tmpf = "/tmp/duckdb_q.sql"
        with open(tmpf, "w") as f: f.write(sql)
        cmd = f"/usr/local/bin/duckdb /srv/duckdb/tpch_sf{sf}.duckdb < {tmpf}"
        return run(cmd)
    elif db == "postgres":
        db_name = f"tpch_sf{sf}"
        cmd = f"docker exec postgres psql -U postgres -p 5433 -d {db_name} -tAc \"{sql_escaped}\""
        return run(cmd)
    elif db == "exasol":
        try:
            conn = exasol_connect()
            schema = f"TPCH_SF{sf}"
            conn.execute(f"OPEN SCHEMA {schema}")
            t0 = time.time()
            result = conn.execute(sql)
            result.fetchall()
            ms = int((time.time()-t0)*1000)
            conn.close()
            return 0, "", "", ms
        except Exception as e:
            return 1, "", str(e), 0
    return 1, "", "unknown db", 0

def benchmark_tpch_sf(sf):
    print(f"\n{'='*60}")
    print(f"  TPC-H SF={sf} Benchmark (5 databases)")
    print(f"{'='*60}")
    tg_proc = start_turbogp(sf) if "turbogp" in DATABASES else None
    outfile = TPCH_RESULTS / f"sf{sf}_results.csv"
    TPCH_RESULTS.mkdir(parents=True, exist_ok=True)
    with open(outfile, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["query_id", "database", "sf", "iteration", "mode", "latency_ms", "status"])
        writer.writeheader()
        for db in DATABASES:
            print(f"\n  --- {db} ---")
            for qnum in range(1, NUM_TPCH + 1):
                qid = f"q{qnum:02d}"
                qf = TPCH_QUERIES / db / f"{qid}.sql"
                if not qf.exists():
                    print(f"    {qid}: file not found, skipping")
                    continue
                sql = qf.read_text()
                # Cold run
                if db != "turbogp":
                    drop_caches()
                rc, out, err, ms = run_query(db, sql, sf)
                status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                print(f"    {qid} cold: {ms}ms [{status}]")
                writer.writerow({"query_id": qid, "database": db, "sf": sf, "iteration": 0, "mode": "cold", "latency_ms": ms, "status": status})
                # Hot runs (1 warmup + 3 measured)
                for i in range(4):
                    rc, out, err, ms = run_query(db, sql, sf)
                    status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                    if i > 0:
                        print(f"    {qid} hot[{i}]: {ms}ms [{status}]")
                        writer.writerow({"query_id": qid, "database": db, "sf": sf, "iteration": i, "mode": "hot", "latency_ms": ms, "status": status})
                    else:
                        print(f"    {qid} warmup: {ms}ms [{status}]")
                f.flush()
    stop_turbogp(tg_proc)
    print(f"\n  SF={sf} results: {outfile}")

def geomean(v):
    v = [x for x in v if x > 0]
    return math.exp(sum(math.log(x) for x in v)/len(v)) if v else 0

def generate_report():
    print("\n=== Generating report ===")
    CHARTS.mkdir(parents=True, exist_ok=True)
    ANALYSIS.mkdir(parents=True, exist_ok=True)
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
        plt.rcParams['font.sans-serif'] = ['DejaVu Sans']
        plt.rcParams['axes.unicode_minus'] = False
        for sf in [1, 10]:
            p = TPCH_RESULTS / f"sf{sf}_results.csv"
            if not p.exists(): continue
            results = list(csv.DictReader(open(p)))
            db_times = defaultdict(list)
            for r in results:
                if r["mode"] == "hot" and r["status"] == "OK":
                    db_times[r["database"]].append(int(r["latency_ms"]))
            dbs = ["turbogp", "clickhouse", "duckdb", "postgres", "exasol"]
            gms = [geomean(db_times.get(d, [0])) for d in dbs]
            colors = ['#e74c3c', '#2ecc71', '#3498db', '#f39c12', '#9b59b6']
            fig, ax = plt.subplots(figsize=(12, 7), constrained_layout=True)
            bars = ax.bar(dbs, gms, color=colors)
            ax.set_ylabel('Geomean Latency (ms)')
            ax.set_title(f'TPC-H SF={sf} — Geomean Hot Run Latency (lower is better)')
            for b, v in zip(bars, gms):
                if v > 0: ax.text(b.get_x()+b.get_width()/2, b.get_height()+0.5, f'{v:.0f}ms', ha='center')
            fig.savefig(CHARTS / f"tpch_sf{sf}_geomean.png", dpi=150)
            plt.close(fig)
            print(f"  chart: tpch_sf{sf}_geomean.png")
    except Exception as e:
        print(f"  charts failed: {e}")

    # Stats report
    stats_lines = ["# Statistical Analysis Report\n"]
    for sf in [1, 10]:
        p = TPCH_RESULTS / f"sf{sf}_results.csv"
        if not p.exists(): continue
        results = list(csv.DictReader(open(p)))
        db_q = defaultdict(lambda: defaultdict(list))
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_q[r["database"]][r["query_id"]].append(int(r["latency_ms"]))
        stats_lines.append(f"## SF={sf}\n")
        stats_lines.append("| Database | Geomean (ms) | Queries OK |")
        stats_lines.append("|---|---|---|")
        for db in DATABASES:
            medians = [sorted(t)[len(t)//2] for t in db_q[db].values() if t]
            gm = geomean(medians)
            stats_lines.append(f"| {db} | {gm:.1f} | {len(medians)}/22 |")
        stats_lines.append("")
    (ANALYSIS / "stats_report.md").write_text("\n".join(stats_lines))

    # BENCHMARK_REPORT.md
    report_lines = ["# turboGP Competitive Benchmarking Report (5-Database)\n", "## Executive Summary\n"]
    for sf in [1, 10]:
        p = TPCH_RESULTS / f"sf{sf}_results.csv"
        if not p.exists(): continue
        results = list(csv.DictReader(open(p)))
        db_q = defaultdict(lambda: defaultdict(list))
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_q[r["database"]][r["query_id"]].append(int(r["latency_ms"]))
        report_lines.append(f"## TPC-H SF={sf}\n")
        report_lines.append(f"![TPC-H SF={sf}](benchmarks/charts/tpch_sf{sf}_geomean.png)\n")
        report_lines.append("| Database | Geomean (ms) | Queries OK |")
        report_lines.append("|---|---|---|")
        for db in DATABASES:
            medians = [sorted(t)[len(t)//2] for t in db_q[db].values() if t]
            gm = geomean(medians)
            report_lines.append(f"| {db} | {gm:.1f} | {len(medians)}/22 |")
        report_lines.append("")
    (REPO / "BENCHMARK_REPORT.md").write_text("\n".join(report_lines))
    print("  report: BENCHMARK_REPORT.md")

def main():
    print("=== 5-Database TPC-H Benchmark (Exasol pre-loaded) ===")
    # Exasol is already loaded from previous run — just run benchmarks
    for sf in [1, 10]:
        benchmark_tpch_sf(sf)
    generate_report()
    commit_msg = "test(8): 5-database TPC-H benchmark with all fixes - Refs: W8. Signed-off-by: benchmarking-agent"
    os.system(f"cd /root/turboGP && git add -A && git commit -m '{commit_msg}' 2>&1 | tail -5")
    os.system("cd /root/turboGP && git push origin feat/dominance-v1 2>&1 | tail -3")
    print("\n=== DONE ===")

if __name__ == "__main__":
    main()
