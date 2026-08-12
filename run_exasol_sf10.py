#!/usr/bin/env python3
"""Run Exasol SF=10 TPC-H benchmark only."""
import csv, ssl, re, time
from pathlib import Path
from collections import defaultdict
import pyexasol
import pyexasol.connection as pc
from packaging.version import Version, InvalidVersion

def patched(self):
    rv = self.login_info.get('releaseVersion')
    if rv:
        try: return Version(rv)
        except: return Version(re.sub(r'-.*$', '', rv))
    return None
pc.ExaConnection.exasol_db_version = property(patched)

REPO = Path("/root/turboGP")
TPCH_QUERIES = REPO / "benchmarks/tpch/queries/exasol"
TPCH_RESULTS = REPO / "benchmarks/tpch/results"

def exasol_connect():
    return pyexasol.connect(
        dsn="localhost:8563", user="sys", password="exasol",
        websocket_sslopt={"cert_reqs": ssl.CERT_NONE},
    )

def run_query(conn, sql, timeout=300):
    t0 = time.time()
    try:
        result = conn.execute(sql)
        result.fetchall()
        ms = int((time.time()-t0)*1000)
        return 0, ms
    except Exception as e:
        ms = int((time.time()-t0)*1000)
        if ms >= timeout * 1000:
            return 124, ms
        return 1, ms

def main():
    conn = exasol_connect()
    conn.execute("OPEN SCHEMA TPCH_SF10")

    outfile = TPCH_RESULTS / "sf10_exasol_only.csv"
    with open(outfile, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["query_id", "database", "sf", "iteration", "mode", "latency_ms", "status"])
        writer.writeheader()

        for qnum in range(1, 23):
            qid = f"q{qnum:02d}"
            qf = TPCH_QUERIES / f"{qid}.sql"
            if not qf.exists():
                print(f"{qid}: file not found")
                continue
            sql = qf.read_text()

            # Cold run
            import subprocess
            subprocess.run("sync; echo 3 > /proc/sys/vm/drop_caches", shell=True, timeout=10)
            time.sleep(1)
            rc, ms = run_query(conn, sql)
            status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
            print(f"{qid} cold: {ms}ms [{status}]")
            writer.writerow({"query_id": qid, "database": "exasol", "sf": 10, "iteration": 0, "mode": "cold", "latency_ms": ms, "status": status})

            # Hot runs
            for i in range(4):
                rc, ms = run_query(conn, sql)
                status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                if i > 0:
                    print(f"{qid} hot[{i}]: {ms}ms [{status}]")
                    writer.writerow({"query_id": qid, "database": "exasol", "sf": 10, "iteration": i, "mode": "hot", "latency_ms": ms, "status": status})
                else:
                    print(f"{qid} warmup: {ms}ms [{status}]")
            f.flush()

    conn.close()
    print(f"\nExasol SF=10 results: {outfile}")

    # Merge with existing sf10_results.csv
    existing = TPCH_RESULTS / "sf10_results.csv"
    if existing.exists():
        existing_rows = list(csv.DictReader(open(existing)))
        new_rows = list(csv.DictReader(open(outfile)))
        # Remove old exasol rows from existing
        filtered = [r for r in existing_rows if r["database"] != "exasol"]
        # Add new exasol rows
        combined = filtered + new_rows
        with open(existing, "w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=["query_id", "database", "sf", "iteration", "mode", "latency_ms", "status"])
            writer.writeheader()
            writer.writerows(combined)
        print(f"Merged into {existing} ({len(combined)} rows)")

if __name__ == "__main__":
    main()
