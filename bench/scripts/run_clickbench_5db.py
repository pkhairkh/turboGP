#!/usr/bin/env python3
"""W12: ClickBench benchmark — run 43 queries on all 5 databases.

Single-threaded, 3 hot iterations (median), 300s timeout.
turboGP loads data in-memory at startup.
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
CB_QUERIES = REPO / "bench/queries/clickbench/queries"
CB_RESULTS = REPO / "bench/queries/clickbench/results"
CHARTS = REPO / "benchmarks/charts"
ANALYSIS = REPO / "benchmarks/analysis"

DATABASES = ["turbogp", "clickhouse", "duckdb", "postgres", "exasol"]
NUM_CB = 43
TIMEOUT = 300
HOT_ITERS = 3

import pyexasol
import pyexasol.connection as pc
from packaging.version import Version, InvalidVersion
def _patched_version(self):
    rv = self.login_info.get('releaseVersion')
    if rv:
        try: return Version(rv)
        except: return Version(re.sub(r'-.*$', '', rv))
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

def start_turbogp():
    """Start turboGP and load ClickBench data."""
    subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
    time.sleep(2)
    csv_dir = "/srv/turbogp_csv/clickbench"
    # Generate turboGP CSV if needed
    csv_path = Path("/srv/turbogp_csv/clickbench/hits.csv")
    if not csv_path.exists():
        print("  Converting hits.tbl to turboGP CSV...")
        csv_path.parent.mkdir(parents=True, exist_ok=True)
        tbl_path = REPO / "bench/queries/clickbench/data/hits.tbl"
        import csv as csvmod
        with open(tbl_path, "r") as fin, open(csv_path, "w", newline="") as fout:
            w = csvmod.writer(fout)
            w.writerow(["WatchID","CounterID","EventDate","EventTime","UserID","RegionID",
                        "OS","UserAgent","URL","Referer","IsRefresh","RefererCategoryID",
                        "SendLog","Age","Sex"])
            for line in fin:
                line = line.rstrip("\r\n")
                if not line: continue
                if line.endswith("|"): line = line[:-1]
                w.writerow(line.split("|"))

    proc = subprocess.Popen(
        [str(TURBOGP), "--insecure", "--port", "55432", "--max-connections", "16",
         "--allow-copy-dir", str(csv_path.parent)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    if proc.poll() is not None:
        raise RuntimeError("turboGP failed to start")
    print(f"  turboGP started (PID={proc.pid})")

    # Create schema and load data
    schema = """CREATE TABLE hits (
        WatchID BIGINT, CounterID INTEGER, EventDate VARCHAR, EventTime VARCHAR,
        UserID BIGINT, RegionID BIGINT, OS INTEGER, UserAgent INTEGER,
        URL VARCHAR, Referer VARCHAR, IsRefresh INTEGER, RefererCategoryID INTEGER,
        SendLog INTEGER, Age INTEGER, Sex INTEGER
    )"""
    subprocess.run(f'psql -h 127.0.0.1 -p 55432 -U postgres -c "{schema}"',
                   shell=True, capture_output=True, timeout=30)
    print("  Loading hits data into turboGP...")
    t0 = time.time()
    subprocess.run(
        f"psql -h 127.0.0.1 -p 55432 -U postgres -c \"COPY hits FROM '{csv_path}' WITH (FORMAT csv, HEADER true)\"",
        shell=True, capture_output=True, timeout=3600,
    )
    print(f"  Loaded in {time.time()-t0:.1f}s")
    return proc

def stop_turbogp(proc):
    if proc:
        proc.terminate()
        try: proc.wait(timeout=5)
        except: proc.kill()

def run_query(db, sql):
    sql_lines = [line for line in sql.split('\n') if not line.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines)
    sql_escaped = sql_clean.replace('"', "'").replace('\n', ' ').strip().rstrip(';')
    if db == "turbogp":
        cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_escaped}"'
        return run(cmd)
    elif db == "clickhouse":
        cmd = f'docker exec clickhouse clickhouse-client --query "{sql_escaped}"'
        return run(cmd)
    elif db == "duckdb":
        tmpf = "/tmp/cb_duckdb_q.sql"
        with open(tmpf, "w") as f: f.write(sql)
        cmd = f"/usr/local/bin/duckdb /srv/duckdb/clickbench.duckdb < {tmpf}"
        return run(cmd)
    elif db == "postgres":
        cmd = f'docker exec postgres psql -U postgres -p 5433 -d clickbench -tAc "{sql_escaped}"'
        return run(cmd)
    elif db == "exasol":
        try:
            conn = exasol_connect()
            conn.execute("OPEN SCHEMA CLICKBENCH")
            t0 = time.time()
            result = conn.execute(sql)
            result.fetchall()
            ms = int((time.time()-t0)*1000)
            conn.close()
            return 0, "", "", ms
        except Exception as e:
            return 1, "", str(e), 0
    return 1, "", "unknown db", 0

def geomean(v):
    v = [x for x in v if x > 0]
    return math.exp(sum(math.log(x) for x in v)/len(v)) if v else 0

def main():
    print("=== ClickBench 5-Database Benchmark ===")
    CB_RESULTS.mkdir(parents=True, exist_ok=True)

    tg_proc = start_turbogp() if "turbogp" in DATABASES else None

    outfile = CB_RESULTS / "clickbench_results.csv"
    with open(outfile, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["query_id", "database", "mode", "latency_ms", "status"])
        writer.writeheader()

        for db in DATABASES:
            print(f"\n  --- {db} ---")
            for qnum in range(1, NUM_CB + 1):
                qid = f"q{qnum:02d}"
                qf = CB_QUERIES / db / f"{qid}.sql"
                if not qf.exists():
                    print(f"    {qid}: file not found, skipping")
                    continue
                sql = qf.read_text()

                # Cold run (skip for turbogp — in-memory)
                if db != "turbogp":
                    drop_caches()
                rc, out, err, ms = run_query(db, sql)
                status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                print(f"    {qid} cold: {ms}ms [{status}]")
                writer.writerow({"query_id": qid, "database": db, "mode": "cold", "latency_ms": ms, "status": status})

                # Hot runs (1 warmup + 3 measured)
                for i in range(HOT_ITERS + 1):
                    rc, out, err, ms = run_query(db, sql)
                    status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                    if i > 0:
                        print(f"    {qid} hot[{i}]: {ms}ms [{status}]")
                        writer.writerow({"query_id": qid, "database": db, "mode": f"hot{i}", "latency_ms": ms, "status": status})
                    else:
                        print(f"    {qid} warmup: {ms}ms [{status}]")
                f.flush()

    stop_turbogp(tg_proc)
    print(f"\n  Results: {outfile}")

    # Generate summary
    results = list(csv.DictReader(open(outfile)))
    db_q = defaultdict(lambda: defaultdict(list))
    for r in results:
        if r["mode"].startswith("hot") and r["status"] == "OK":
            db_q[r["database"]][r["query_id"]].append(int(r["latency_ms"]))

    print("\n=== ClickBench Summary ===")
    print("Database       Geomean(ms)  Queries OK")
    for db in DATABASES:
        medians = [sorted(t)[len(t)//2] for t in db_q[db].values() if t]
        gm = geomean(medians)
        print(f"{db:14s} {gm:10.1f}  {len(medians)}/43")

    # Commit
    commit_msg = "test(12): ClickBench 5-database benchmark complete. Refs: W12. Signed-off-by: benchmarking-agent"
    os.system(f"cd /root/turboGP && git add -A && git commit -m '{commit_msg}' 2>&1 | tail -3")
    os.system("cd /root/turboGP && git push origin feat/dominance-v1 2>&1 | tail -3")
    print("\n=== DONE ===")

if __name__ == "__main__":
    main()
