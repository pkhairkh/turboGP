#!/usr/bin/env python3
"""
Comprehensive benchmark runner: TPC-H (22 queries) + ClickBench (43 queries)
for turboGP, DuckDB, and Exasol. Runs every query SEQUENTIALLY and records
cold timing for each.

Usage:
    python3 /root/turboGP/bench_all.py --db turbogp --bench tpch --sf 1
    python3 /root/turboGP/bench_all.py --db duckdb --bench tpch --sf 1
    python3 /root/turboGP/bench_all.py --db exasol --bench tpch --sf 1
    python3 /root/turboGP/bench_all.py --db turbogp --bench clickbench
    python3 /root/turboGP/bench_all.py --db duckdb --bench clickbench
    python3 /root/turboGP/bench_all.py --db exasol --bench clickbench

Output: CSV with columns: query, cold_ms, status, rows
"""
import subprocess
import time
import sys
import os
import csv
import argparse
import re

REPO = "/root/turboGP"

def read_query(path):
    """Read a SQL file, strip -- comments."""
    with open(path) as f:
        lines = [l for l in f if not l.strip().startswith("--")]
    return "\n".join(lines).strip()

def list_queries(db, bench, sf=None):
    """Return list of (qid, filepath) for the given db/bench."""
    if bench == "tpch":
        qdir = f"{REPO}/benchmarks/tpch/queries/{db}"
        queries = []
        for i in range(1, 23):
            for ext in [".sql"]:
                p = f"{qdir}/q{i:02d}{ext}"
                if os.path.exists(p):
                    queries.append((f"q{i:02d}", p))
                    break
        return queries
    elif bench == "clickbench":
        qdir = f"{REPO}/benchmarks/clickbench/queries/{db}"
        queries = []
        for i in range(1, 44):
            p = f"{qdir}/q{i:02d}.sql"
            if os.path.exists(p):
                queries.append((f"q{i:02d}", p))
        return queries
    return []

def run_turbogp_tpch(sf, queries, output):
    """Run TPC-H on turboGP using the native bench binary."""
    bin_path = f"{REPO}/target/release/turbogp_bench"
    csv_dir = f"/srv/turbogp_csv/sf{sf}"
    cmd = [bin_path, "--repo-dir", REPO, "tpch", "--sf", str(sf),
           "--csv-dir", csv_dir, "--output", output]
    print(f"  Running: {' '.join(cmd)}")
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    print(r.stderr[-500:] if r.stderr else "")
    return r.returncode == 0

def run_turbogp_clickbench(queries, output):
    """Run ClickBench on turboGP using the native bench binary."""
    bin_path = f"{REPO}/target/release/turbogp_bench"
    csv_dir = "/srv/turbogp_csv/clickbench"
    cmd = [bin_path, "--repo-dir", REPO, "clickbench",
           "--csv-dir", csv_dir, "--output", output]
    print(f"  Running: {' '.join(cmd)}")
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    print(r.stderr[-500:] if r.stderr else "")
    return r.returncode == 0

def run_duckdb_tpch(sf, queries, output):
    """Run TPC-H on DuckDB sequentially."""
    db_path = f"/srv/duckdb/tpch_sf{sf}.duckdb"
    results = []
    for qid, qpath in queries:
        sql = read_query(qpath)
        # DuckDB CLI: run query, time it
        full_cmd = f'.timer on\n{sql}\n'
        t0 = time.time()
        try:
            r = subprocess.run(
                ["duckdb", db_path, "--csv"],
                input=full_cmd, capture_output=True, text=True, timeout=300
            )
            elapsed_ms = (time.time() - t0) * 1000
            status = "OK" if r.returncode == 0 else f"ERR: {r.stderr[:200]}"
            rows = len(r.stdout.strip().split('\n')) - 1 if r.stdout else 0
        except subprocess.TimeoutExpired:
            elapsed_ms = (time.time() - t0) * 1000
            status = "TIMEOUT"
            rows = 0
        except Exception as e:
            elapsed_ms = (time.time() - t0) * 1000
            status = f"ERR: {e}"
            rows = 0
        print(f"  {qid}: {elapsed_ms:.1f}ms {status}")
        results.append((qid, elapsed_ms, status, rows))
    write_csv(output, results)
    return True

def run_duckdb_clickbench(queries, output):
    """Run ClickBench on DuckDB sequentially."""
    db_path = "/srv/duckdb/clickbench.duckdb"
    results = []
    for qid, qpath in queries:
        sql = read_query(qpath)
        full_cmd = f'.timer on\n{sql}\n'
        t0 = time.time()
        try:
            r = subprocess.run(
                ["duckdb", db_path, "--csv"],
                input=full_cmd, capture_output=True, text=True, timeout=300
            )
            elapsed_ms = (time.time() - t0) * 1000
            status = "OK" if r.returncode == 0 else f"ERR: {r.stderr[:200]}"
            rows = len(r.stdout.strip().split('\n')) - 1 if r.stdout else 0
        except subprocess.TimeoutExpired:
            elapsed_ms = (time.time() - t0) * 1000
            status = "TIMEOUT"
            rows = 0
        except Exception as e:
            elapsed_ms = (time.time() - t0) * 1000
            status = f"ERR: {e}"
            rows = 0
        print(f"  {qid}: {elapsed_ms:.1f}ms {status}")
        results.append((qid, elapsed_ms, status, rows))
    write_csv(output, results)
    return True

def run_exasol_tpch(sf, queries, output):
    """Run TPC-H on Exasol sequentially."""
    import pyexasol, ssl, re
    from packaging.version import Version, InvalidVersion
    def patched(self):
        rv = self.login_info.get('releaseVersion')
        if rv:
            try: return Version(rv)
            except: return Version(re.sub(r'-.*$', '', rv))
        return None
    pyexasol.connection.ExaConnection.exasol_db_version = property(patched)
    conn = pyexasol.connect(dsn="localhost:8563", user="sys", password="exasol",
                            websocket_sslopt={"cert_reqs": ssl.CERT_NONE})
    # Determine schema
    schema = f"TPCH_SF{sf}"
    try:
        conn.execute(f"OPEN SCHEMA {schema}")
    except:
        print(f"  WARN: schema {schema} not found, trying TPCH")
        try:
            conn.execute("OPEN SCHEMA TPCH")
        except:
            print(f"  ERROR: no TPCH schema found")
            return False
    results = []
    for qid, qpath in queries:
        sql = read_query(qpath)
        t0 = time.time()
        try:
            r = conn.execute(sql)
            rows = len(r.fetchall())
            elapsed_ms = (time.time() - t0) * 1000
            status = "OK"
        except Exception as e:
            elapsed_ms = (time.time() - t0) * 1000
            status = f"ERR: {str(e)[:200]}"
            rows = 0
        print(f"  {qid}: {elapsed_ms:.1f}ms {status}")
        results.append((qid, elapsed_ms, status, rows))
    conn.close()
    write_csv(output, results)
    return True

def run_exasol_clickbench(queries, output):
    """Run ClickBench on Exasol sequentially."""
    import pyexasol, ssl, re
    from packaging.version import Version, InvalidVersion
    def patched(self):
        rv = self.login_info.get('releaseVersion')
        if rv:
            try: return Version(rv)
            except: return Version(re.sub(r'-.*$', '', rv))
        return None
    pyexasol.connection.ExaConnection.exasol_db_version = property(patched)
    conn = pyexasol.connect(dsn="localhost:8563", user="sys", password="exasol",
                            websocket_sslopt={"cert_reqs": ssl.CERT_NONE})
    try:
        conn.execute("OPEN SCHEMA CLICKBENCH")
    except:
        print(f"  WARN: CLICKBENCH schema not found, trying hits")
        try:
            conn.execute("OPEN SCHEMA HITS")
        except:
            print(f"  ERROR: no ClickBench schema found")
            return False
    results = []
    for qid, qpath in queries:
        sql = read_query(qpath)
        t0 = time.time()
        try:
            r = conn.execute(sql)
            rows = len(r.fetchall())
            elapsed_ms = (time.time() - t0) * 1000
            status = "OK"
        except Exception as e:
            elapsed_ms = (time.time() - t0) * 1000
            status = f"ERR: {str(e)[:200]}"
            rows = 0
        print(f"  {qid}: {elapsed_ms:.1f}ms {status}")
        results.append((qid, elapsed_ms, status, rows))
    conn.close()
    write_csv(output, results)
    return True

def write_csv(path, results):
    with open(path, 'w', newline='') as f:
        w = csv.writer(f)
        w.writerow(["query", "cold_ms", "status", "rows"])
        for qid, ms, status, rows in results:
            w.writerow([qid, f"{ms:.1f}", status, rows])
    print(f"  Results saved to {path}")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True, choices=["turbogp", "duckdb", "exasol"])
    ap.add_argument("--bench", required=True, choices=["tpch", "clickbench"])
    ap.add_argument("--sf", type=int, default=1)
    ap.add_argument("--output", required=True)
    args = ap.parse_args()

    queries = list_queries(args.db, args.bench, args.sf)
    if not queries:
        print(f"ERROR: no queries found for {args.db}/{args.bench}")
        sys.exit(1)
    print(f"\n=== {args.db.upper()} {args.bench.upper()} SF={args.sf} ({len(queries)} queries) ===")

    if args.db == "turbogp":
        if args.bench == "tpch":
            run_turbogp_tpch(args.sf, queries, args.output)
        else:
            run_turbogp_clickbench(queries, args.output)
    elif args.db == "duckdb":
        if args.bench == "tpch":
            run_duckdb_tpch(args.sf, queries, args.output)
        else:
            run_duckdb_clickbench(queries, args.output)
    elif args.db == "exasol":
        if args.bench == "tpch":
            run_exasol_tpch(args.sf, queries, args.output)
        else:
            run_exasol_clickbench(queries, args.output)

if __name__ == "__main__":
    main()
