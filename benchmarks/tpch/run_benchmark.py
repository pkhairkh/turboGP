#!/usr/bin/env python3
"""Wave 4: TPC-H benchmark orchestration.

Runs all 22 TPC-H queries on all 4 databases at SF=1 and SF=10.
For each query×database×SF: cold run (drop OS cache) + 3 hot runs (median).
Records: query_id, database, sf, iteration, mode, latency_ms, status.
Outputs CSV to benchmarks/tpch/results/.
"""
import argparse
import csv
import os
import subprocess
import sys
import time
from pathlib import Path

REPO = Path("/root/turboGP")
QUERIES_DIR = REPO / "benchmarks/tpch/queries"
RESULTS_DIR = REPO / "benchmarks/tpch/results"
TURBOGP_BIN = REPO / "target/release/turbogp"

DATABASES = ["turbogp", "clickhouse", "duckdb", "postgres"]
NUM_QUERIES = 22
TIMEOUT_SEC = 300
HOT_ITERATIONS = 3  # Run 3 times, take median (discard first as warmup)


def run_cmd(cmd, timeout=TIMEOUT_SEC, capture=True):
    """Run a shell command, return (rc, stdout, stderr, elapsed_ms)."""
    t0 = time.time()
    try:
        proc = subprocess.run(
            cmd, shell=True, timeout=timeout,
            capture_output=capture, text=True,
        )
        elapsed_ms = int((time.time() - t0) * 1000)
        return proc.returncode, proc.stdout, proc.stderr, elapsed_ms
    except subprocess.TimeoutExpired:
        elapsed_ms = int((time.time() - t0) * 1000)
        return 124, "", "TIMEOUT", elapsed_ms
    except Exception as e:
        elapsed_ms = int((time.time() - t0) * 1000)
        return 1, "", str(e), elapsed_ms


def drop_caches():
    """Drop OS page cache for cold runs."""
    subprocess.run("sync; echo 3 > /proc/sys/vm/drop_caches", shell=True, timeout=10)
    time.sleep(1)


def run_query_turbogp(sql, sf, port=55432):
    """Run a query on turboGP (pgwire)."""
    db_name = f"tpch_sf{sf}"  # Not used — turboGP loads tables directly
    cmd = f"psql -h 127.0.0.1 -p {port} -U postgres -tAc \"{sql.replace(chr(34), chr(39)).replace(chr(10), ' ')}\""
    return run_cmd(cmd)


def run_query_clickhouse(sql, sf):
    """Run a query on ClickHouse."""
    cmd = f"docker exec clickhouse clickhouse-client --query \"{sql.replace(chr(34), chr(39)).replace(chr(10), ' ')}\""
    return run_cmd(cmd)


def run_query_duckdb(sql, sf):
    """Run a query on DuckDB."""
    db_path = f"/srv/duckdb/tpch_sf{sf}.duckdb"
    # Write SQL to a temp file to avoid shell escaping issues
    tmpf = f"/tmp/duckdb_q.sql"
    with open(tmpf, "w") as f:
        f.write(sql)
    cmd = f"/usr/local/bin/duckdb {db_path} < {tmpf}"
    return run_cmd(cmd)


def run_query_postgres(sql, sf):
    """Run a query on PostgreSQL."""
    db_name = f"tpch_sf{sf}"
    tmpf = f"/tmp/pg_q.sql"
    with open(tmpf, "w") as f:
        f.write(sql)
    cmd = f"docker exec -i postgres psql -U postgres -p 5433 -d {db_name} -tA -f {tmpf}"
    # Can't use -f with docker exec -i easily; use -c instead
    # Read the file content and pass via -c
    with open(tmpf, "r") as f:
        sql_content = f.read()
    cmd = f"docker exec postgres psql -U postgres -p 5433 -d {db_name} -tAc \"{sql_content.replace(chr(34), chr(39)).replace(chr(10), ' ')}\""
    return run_cmd(cmd)


def run_query(db, sql, sf):
    """Dispatch to the right database runner."""
    if db == "turbogp":
        return run_query_turbogp(sql, sf)
    elif db == "clickhouse":
        return run_query_clickhouse(sql, sf)
    elif db == "duckdb":
        return run_query_duckdb(sql, sf)
    elif db == "postgres":
        return run_query_postgres(sql, sf)
    else:
        return 1, "", f"Unknown database: {db}", 0


def load_query(dialect, qnum):
    """Load a query from the queries directory."""
    f = QUERIES_DIR / dialect / f"q{qnum:02d}.sql"
    return f.read_text()


def benchmark_database(db, sf, results_csv, cold=True, hot=True):
    """Run all 22 queries on one database at one SF."""
    dialect = db  # dialect name matches db name
    print(f"\n{'='*60}")
    print(f"  Database: {db}  SF: {sf}  Cold: {cold}  Hot: {hot}")
    print(f"{'='*60}")

    for qnum in range(1, NUM_QUERIES + 1):
        try:
            sql = load_query(dialect, qnum)
        except FileNotFoundError:
            print(f"  Q{qnum:02d}: query file not found, skipping")
            continue

        # Cold run
        if cold:
            drop_caches()
            rc, out, err, ms = run_query(db, sql, sf)
            status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
            print(f"  Q{qnum:02d} cold: {ms}ms [{status}]")
            results_csv.writerow({
                "query_id": f"q{qnum:02d}",
                "database": db,
                "sf": sf,
                "iteration": 0,
                "mode": "cold",
                "latency_ms": ms,
                "status": status,
            })

        # Hot runs (3 iterations, take median; discard first as warmup)
        if hot:
            hot_times = []
            for i in range(HOT_ITERATIONS + 1):  # +1 for warmup
                rc, out, err, ms = run_query(db, sql, sf)
                status = "OK" if rc == 0 else ("TIMEOUT" if rc == 124 else "ERROR")
                if i > 0:  # Skip warmup (iteration 0)
                    hot_times.append(ms)
                    print(f"  Q{qnum:02d} hot[{i}]: {ms}ms [{status}]")
                    results_csv.writerow({
                        "query_id": f"q{qnum:02d}",
                        "database": db,
                        "sf": sf,
                        "iteration": i,
                        "mode": "hot",
                        "latency_ms": ms,
                        "status": status,
                    })
                else:
                    print(f"  Q{qnum:02d} warmup: {ms}ms [{status}]")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sf", type=int, nargs="+", default=[1, 10])
    ap.add_argument("--databases", type=str, nargs="+", default=DATABASES)
    ap.add_argument("--no-cold", action="store_true")
    ap.add_argument("--no-hot", action="store_true")
    args = ap.parse_args()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    for sf in args.sf:
        outfile = RESULTS_DIR / f"sf{sf}_results.csv"
        print(f"\n{'#'*60}")
        print(f"# TPC-H SF={sf} Benchmark")
        print(f"# Output: {outfile}")
        print(f"{'#'*60}")

        with open(outfile, "w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=[
                "query_id", "database", "sf", "iteration", "mode", "latency_ms", "status"
            ])
            writer.writeheader()

            for db in args.databases:
                benchmark_database(db, sf, writer,
                                   cold=not args.no_cold,
                                   hot=not args.no_hot)
                f.flush()

        print(f"\nResults written to {outfile}")


if __name__ == "__main__":
    main()
