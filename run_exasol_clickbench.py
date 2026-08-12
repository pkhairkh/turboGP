#!/usr/bin/env python3
"""Run Exasol ClickBench queries only and merge with existing results."""
import csv, ssl, re, time, subprocess
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
CB_QUERIES = REPO / "benchmarks/clickbench/queries/exasol"
CB_RESULTS = REPO / "benchmarks/clickbench/results"

def main():
    conn = pyexasol.connect(
        dsn="localhost:8563", user="sys", password="exasol",
        websocket_sslopt={"cert_reqs": ssl.CERT_NONE},
    )
    conn.execute("OPEN SCHEMA CLICKBENCH")

    outfile = CB_RESULTS / "clickbench_exasol_only.csv"
    CB_RESULTS.mkdir(parents=True, exist_ok=True)

    with open(outfile, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["query_id", "database", "mode", "latency_ms", "status"])
        writer.writeheader()

        for qnum in range(1, 44):
            qid = f"q{qnum:02d}"
            qf = CB_QUERIES / f"{qid}.sql"
            if not qf.exists():
                print(f"{qid}: file not found")
                continue
            sql = qf.read_text()

            # Cold run
            subprocess.run("sync; echo 3 > /proc/sys/vm/drop_caches", shell=True, timeout=10)
            time.sleep(1)
            t0 = time.time()
            try:
                result = conn.execute(sql)
                result.fetchall()
                ms = int((time.time()-t0)*1000)
                status = "OK"
            except Exception as e:
                ms = int((time.time()-t0)*1000)
                status = "TIMEOUT" if ms >= 300000 else "ERROR"
            print(f"{qid} cold: {ms}ms [{status}]")
            writer.writerow({"query_id": qid, "database": "exasol", "mode": "cold", "latency_ms": ms, "status": status})

            # Hot runs
            for i in range(4):
                t0 = time.time()
                try:
                    result = conn.execute(sql)
                    result.fetchall()
                    ms = int((time.time()-t0)*1000)
                    status = "OK"
                except Exception as e:
                    ms = int((time.time()-t0)*1000)
                    status = "TIMEOUT" if ms >= 300000 else "ERROR"
                if i > 0:
                    print(f"{qid} hot[{i}]: {ms}ms [{status}]")
                    writer.writerow({"query_id": qid, "database": "exasol", "mode": f"hot{i}", "latency_ms": ms, "status": status})
                else:
                    print(f"{qid} warmup: {ms}ms [{status}]")
            f.flush()

    conn.close()

    # Merge with existing results
    existing = CB_RESULTS / "clickbench_results.csv"
    if existing.exists():
        existing_rows = list(csv.DictReader(open(existing)))
        new_rows = list(csv.DictReader(open(outfile)))
        filtered = [r for r in existing_rows if r["database"] != "exasol"]
        combined = filtered + new_rows
        with open(existing, "w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=["query_id", "database", "mode", "latency_ms", "status"])
            writer.writeheader()
            writer.writerows(combined)
        print(f"\nMerged into {existing} ({len(combined)} rows)")

if __name__ == "__main__":
    main()
