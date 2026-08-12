#!/usr/bin/env python3
"""Wave 7: ClickBench benchmark — single + multi-threaded."""
import csv, subprocess, time, threading
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor, as_completed

REPO = Path("/root/turboGP")
QUERIES_DIR = REPO / "benchmarks/clickbench/queries"
RESULTS_DIR = REPO / "benchmarks/clickbench/results"

def run_cmd(cmd, timeout=300):
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=timeout, capture_output=True, text=True)
        return p.returncode, int((time.time()-t0)*1000)
    except subprocess.TimeoutExpired:
        return 124, int((time.time()-t0)*1000)
    except:
        return 1, int((time.time()-t0)*1000)

def run_query(db, sql):
    if db == "clickhouse":
        cmd = f"docker exec clickhouse clickhouse-client --query \"{sql}\""
    elif db == "duckdb":
        cmd = f"echo \"{sql}\" | /usr/local/bin/duckdb /srv/duckdb/clickbench.duckdb"
    elif db == "postgres":
        cmd = f"docker exec postgres psql -U postgres -p 5433 -d clickbench -tAc \"{sql}\""
    elif db == "turbogp":
        cmd = f"psql -h 127.0.0.1 -p 55432 -U postgres -tAc \"{sql}\""
    else:
        return 1, 0
    return run_cmd(cmd)

def main():
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    dbs = ["clickhouse", "duckdb", "postgres"]  # turbogp added if running
    NUM_QUERIES = 43

    # Single-threaded
    print("=== Single-threaded ===")
    with open(RESULTS_DIR / "single_threaded.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["query_id", "database", "mode", "latency_ms", "status"])
        w.writeheader()
        for db in dbs:
            for qnum in range(1, NUM_QUERIES + 1):
                qf = QUERIES_DIR / db / f"q{qnum:02d}.sql"
                if not qf.exists(): continue
                sql = qf.read_text().strip().rstrip(';')
                rc, ms = run_query(db, sql)
                status = "OK" if rc == 0 else "ERROR"
                w.writerow({"query_id": f"q{qnum:02d}", "database": db, "mode": "single", "latency_ms": ms, "status": status})
                f.flush()
                print(f"  {db} Q{qnum:02d}: {ms}ms [{status}]")

    # Multi-threaded (N parallel connections, N = vCPU count)
    print("\n=== Multi-threaded ===")
    N = 16  # vCPU count
    with open(RESULTS_DIR / "multi_threaded.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["query_id", "database", "mode", "latency_ms", "status", "thread_id"])
        w.writeheader()
        for db in dbs:
            for qnum in range(1, NUM_QUERIES + 1):
                qf = QUERIES_DIR / db / f"q{qnum:02d}.sql"
                if not qf.exists(): continue
                sql = qf.read_text().strip().rstrip(';')
                # Run the same query N times in parallel
                def run_one(tid):
                    rc, ms = run_query(db, sql)
                    return tid, rc, ms
                with ThreadPoolExecutor(max_workers=N) as pool:
                    futures = [pool.submit(run_one, i) for i in range(N)]
                    for fut in as_completed(futures):
                        tid, rc, ms = fut.result()
                        status = "OK" if rc == 0 else "ERROR"
                        w.writerow({"query_id": f"q{qnum:02d}", "database": db, "mode": "multi", "latency_ms": ms, "status": status, "thread_id": tid})
                f.flush()
                print(f"  {db} Q{qnum:02d} (×{N}): done")

    print("\nClickBench benchmark complete.")

if __name__ == "__main__":
    main()
