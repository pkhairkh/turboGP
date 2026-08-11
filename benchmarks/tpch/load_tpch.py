#!/usr/bin/env python3
"""
Task 2.3 / 2.4: TPC-H data loading for all 4 databases (fixed version).

Strategy per database:
  - turboGP: convert .tbl to .csv (with header, proper CSV quoting) via Python,
             start turboGP with --allow-copy-dir, then `COPY <tbl> FROM '<csv>'`.
  - ClickHouse: use FORMAT CustomSeparated with field_delimiter='|'.
  - DuckDB: COPY FROM '<file>' (DELIMITER '|', HEADER false). Row count via -c.
  - PostgreSQL: \COPY FROM STDIN WITH (FORMAT csv, DELIMITER '|', NULL ''). (Already works.)

Usage:
    python3 load_tpch_v2.py --sf 1
    python3 load_tpch_v2.py --sf 10
    python3 load_tpch_v2.py --sf 1 --only turbogp,clickhouse
"""
import argparse
import csv
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path("/root/turboGP")
DATA_DIR = REPO / "benchmarks/tpch/data"
SCHEMA_DIR = REPO / "benchmarks/tpch/schema"
TURBOGP_BIN = REPO / "target/release/turbogp"
CSV_TMP_DIR = Path("/srv/turbogp_csv")  # pre-converted CSV files for turboGP

TABLES = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]
EXPECTED_ROWS_SF1 = {
    "region": 5, "nation": 25, "supplier": 10000, "customer": 150000,
    "part": 200000, "partsupp": 800000, "orders": 1500000, "lineitem": 6001215,
}
EXPECTED_ROWS_SF10 = {
    "region": 5, "nation": 25, "supplier": 100000, "customer": 1500000,
    "part": 2000000, "partsupp": 8000000, "orders": 15000000, "lineitem": 59986052,
}

# Column names per table (must match the schema files)
COLS = {
    "region":   ["r_regionkey","r_name","r_comment"],
    "nation":   ["n_nationkey","n_name","n_regionkey","n_comment"],
    "supplier": ["s_suppkey","s_name","s_address","s_nationkey","s_phone","s_acctbal","s_comment"],
    "customer": ["c_custkey","c_name","c_address","c_nationkey","c_phone","c_acctbal","c_mktsegment","c_comment"],
    "part":     ["p_partkey","p_name","p_mfgr","p_brand","p_type","p_size","p_container","p_retailprice","p_comment"],
    "partsupp": ["ps_partkey","ps_suppkey","ps_availqty","ps_supplycost","ps_comment"],
    "orders":   ["o_orderkey","o_custkey","o_orderstatus","o_totalprice","o_orderdate","o_orderpriority","o_clerk","o_shippriority","o_comment"],
    "lineitem": ["l_orderkey","l_partkey","l_suppkey","l_linenumber","l_quantity","l_extendedprice","l_discount","l_tax","l_returnflag","l_linestatus","l_shipdate","l_commitdate","l_receiptdate","l_shipinstruct","l_shipmode","l_comment"],
}


def run(cmd, timeout=3600, check=True, capture=False, env=None):
    """Run a shell command, stream output, return (rc, stdout, stderr)."""
    if isinstance(cmd, str):
        print(f"$ {cmd[:200]}{'...' if len(cmd)>200 else ''}", flush=True)
    else:
        print(f"$ {' '.join(cmd)[:200]}", flush=True)
    t0 = time.time()
    try:
        proc = subprocess.run(
            cmd, shell=isinstance(cmd, str), timeout=timeout,
            capture_output=capture, text=True, env=env,
        )
    except subprocess.TimeoutExpired:
        print(f"[TIMEOUT after {timeout}s]")
        if check:
            raise
        return 124, "", ""
    elapsed = time.time() - t0
    if capture:
        if proc.stdout:
            print(proc.stdout[-1500:])
        if proc.stderr:
            sys.stderr.write(proc.stderr[-1500:])
    print(f"[exit={proc.returncode}, {elapsed:.1f}s]")
    if check and proc.returncode != 0:
        raise RuntimeError(f"command failed: {cmd}")
    return proc.returncode, proc.stdout, proc.stderr


# ---------- turboGP ----------
def tbl_to_csv(tbl, tbl_path, out_path):
    """Convert a .tbl file (pipe-separated, trailing pipe) to proper CSV with header."""
    cols = COLS[tbl]
    with open(tbl_path, "r", encoding="utf-8", errors="replace") as fin, \
         open(out_path, "w", encoding="utf-8", newline="") as fout:
        w = csv.writer(fout, quoting=csv.QUOTE_MINIMAL)
        w.writerow(cols)
        for line in fin:
            line = line.rstrip("\n")
            if not line:
                continue
            # Strip trailing pipe, then split on pipe
            if line.endswith("|"):
                line = line[:-1]
            fields = line.split("|")
            # Coerce numeric fields to avoid quotes (turboGP COPY FROM parses numbers vs strings)
            w.writerow(fields)


def load_turbogp(sf):
    print("\n========== turboGP ==========")
    # Pre-convert .tbl to .csv (only for tables not yet converted)
    print(f"Converting .tbl to .csv in {CSV_TMP_DIR}/sf{sf}/ ...")
    out_dir = CSV_TMP_DIR / f"sf{sf}"
    out_dir.mkdir(parents=True, exist_ok=True)
    for tbl in TABLES:
        tbl_path = DATA_DIR / f"sf{sf}" / f"{tbl}.tbl"
        csv_path = out_dir / f"{tbl}.csv"
        if not csv_path.exists() or csv_path.stat().st_size == 0:
            t0 = time.time()
            tbl_to_csv(tbl, tbl_path, csv_path)
            print(f"  converted {tbl} -> {csv_path.name} ({csv_path.stat().st_size//1024} KB, {time.time()-t0:.1f}s)")
        else:
            print(f"  {tbl}.csv already exists ({csv_path.stat().st_size//1024} KB), skipping")

    # Start turboGP with --allow-copy-dir pointing to the CSV dir
    print("Starting turboGP on port 55432 (in-memory, insecure, allow-copy-dir)...")
    subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
    time.sleep(1)
    tg_proc = subprocess.Popen(
        [str(TURBOGP_BIN), "--insecure", "--port", "55432", "--max-connections", "16",
         "--allow-copy-dir", str(out_dir)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    if tg_proc.poll() is not None:
        raise RuntimeError("turboGP failed to start")
    print(f"turboGP PID={tg_proc.pid}")

    try:
        # Apply schema
        schema = (SCHEMA_DIR / "turbogp.sql").read_text()
        tmpf = Path("/tmp/turbogp_schema.sql")
        tmpf.write_text(schema)
        run(f"psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -f {tmpf}",
            timeout=120, check=False)

        # COPY each table from its CSV file (server-side COPY)
        for tbl in TABLES:
            csv_path = out_dir / f"{tbl}.csv"
            t0 = time.time()
            run(
                f"psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -c "
                f"\"COPY {tbl} FROM '{csv_path}' WITH (FORMAT csv, HEADER true)\"",
                timeout=3600, check=False,
            )
            print(f"  loaded {tbl} in {time.time()-t0:.1f}s")

        # Verify row counts
        print("Row counts in turboGP:")
        for tbl in TABLES:
            rc, out, _ = run(
                f"psql -h 127.0.0.1 -p 55432 -U postgres -tAc 'SELECT COUNT(*) FROM {tbl}'",
                timeout=60, check=False, capture=True,
            )
            print(f"  {tbl}: {out.strip()}")
    finally:
        print(f"Stopping turboGP (pid {tg_proc.pid})...")
        tg_proc.terminate()
        try:
            tg_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            tg_proc.kill()


# ---------- ClickHouse ----------
def load_clickhouse(sf):
    print("\n========== ClickHouse ==========")
    # Apply schema
    schema = (SCHEMA_DIR / "clickhouse.sql").read_text()
    tmpf = Path("/tmp/ch_schema.sql")
    tmpf.write_text(schema)
    run(f"docker exec -i clickhouse clickhouse-client --multiquery < {tmpf}",
        timeout=120, check=False)

    # Load each table via INSERT FORMAT CustomSeparated with field_delimiter='|'
    for tbl in TABLES:
        csv = DATA_DIR / f"sf{sf}" / f"{tbl}.tbl"
        # Strip trailing pipe, then INSERT FORMAT CustomSeparated
        cmd = (
            f"sed 's/|$//' {csv} | docker exec -i clickhouse clickhouse-client "
            f"--query \"INSERT INTO {tbl} FORMAT CustomSeparated\" "
            f"--format_custom_field_delimiter=\"|\" "
            f"--format_custom_row_after_delimiter=\"\\n\""
        )
        t0 = time.time()
        run(cmd, timeout=3600, check=False)
        print(f"  loaded {tbl} in {time.time()-t0:.1f}s")

    # Verify row counts
    print("Row counts in ClickHouse:")
    for tbl in TABLES:
        rc, out, _ = run(
            f"docker exec clickhouse clickhouse-client --query 'SELECT COUNT(*) FROM {tbl}'",
            timeout=60, check=False, capture=True,
        )
        print(f"  {tbl}: {out.strip()}")


# ---------- DuckDB ----------
def load_duckdb(sf):
    print("\n========== DuckDB ==========")
    db_path = f"/srv/duckdb/tpch_sf{sf}.duckdb"
    if os.path.exists(db_path):
        os.remove(db_path)

    schema = (SCHEMA_DIR / "duckdb.sql").read_text()
    tmpf = Path("/tmp/duck_schema.sql")
    tmpf.write_text(schema)
    run(f"/usr/local/bin/duckdb {db_path} < {tmpf}", timeout=120, check=False)

    for tbl in TABLES:
        csv = DATA_DIR / f"sf{sf}" / f"{tbl}.tbl"
        # DuckDB COPY FROM supports DELIMITER '|'. Strip trailing pipe via a temp file.
        stripped = Path(f"/tmp/{tbl}_stripped.csv")
        run(f"sed 's/|$//' {csv} > {stripped}", timeout=300, check=True)
        sql = f"COPY {tbl} FROM '{stripped}' (DELIMITER '|', HEADER false, NULL '');"
        t0 = time.time()
        run(f"/usr/local/bin/duckdb {db_path} -c \"{sql}\"", timeout=3600, check=False)
        print(f"  loaded {tbl} in {time.time()-t0:.1f}s")
        stripped.unlink(missing_ok=True)

    # Verify row counts — DuckDB CLI uses -c (no -tA flags)
    print("Row counts in DuckDB:")
    for tbl in TABLES:
        rc, out, _ = run(
            f"/usr/local/bin/duckdb {db_path} -noheader -list -c 'SELECT COUNT(*) FROM {tbl}'",
            timeout=60, check=False, capture=True,
        )
        print(f"  {tbl}: {out.strip()}")


# ---------- PostgreSQL ----------
def load_postgres(sf):
    print("\n========== PostgreSQL ==========")
    db_name = f"tpch_sf{sf}"
    run(f"docker exec postgres psql -U postgres -p 5433 -c 'DROP DATABASE IF EXISTS {db_name};'",
        timeout=60, check=False)
    run(f"docker exec postgres psql -U postgres -p 5433 -c 'CREATE DATABASE {db_name};'",
        timeout=60, check=False)

    schema = (SCHEMA_DIR / "postgres.sql").read_text()
    tmpf = Path("/tmp/pg_schema.sql")
    tmpf.write_text(schema)
    run(f"docker exec -i postgres psql -U postgres -p 5433 -d {db_name} -v ON_ERROR_STOP=1 < {tmpf}",
        timeout=120, check=False)

    for tbl in TABLES:
        csv = DATA_DIR / f"sf{sf}" / f"{tbl}.tbl"
        cmd = (
            f"sed 's/|$//' {csv} | docker exec -i postgres psql -U postgres -p 5433 -d {db_name} -c "
            f"\"\\COPY {tbl} FROM STDIN WITH (FORMAT csv, DELIMITER '|', NULL '')\""
        )
        t0 = time.time()
        run(cmd, timeout=3600, check=False)
        print(f"  loaded {tbl} in {time.time()-t0:.1f}s")

    print("Row counts in PostgreSQL:")
    for tbl in TABLES:
        rc, out, _ = run(
            f"docker exec postgres psql -U postgres -p 5433 -d {db_name} -tAc 'SELECT COUNT(*) FROM {tbl}'",
            timeout=60, check=False, capture=True,
        )
        print(f"  {tbl}: {out.strip()}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sf", type=int, required=True, choices=[1, 10])
    ap.add_argument("--only", type=str, default="turbogp,clickhouse,duckdb,postgres")
    args = ap.parse_args()

    targets = [t.strip() for t in args.only.split(",") if t.strip()]
    print(f"Loading TPC-H SF={args.sf} into: {targets}")
    print(f"Expected row counts: {(EXPECTED_ROWS_SF1 if args.sf == 1 else EXPECTED_ROWS_SF10)}")

    for db in targets:
        t0 = time.time()
        try:
            if db == "turbogp":
                load_turbogp(args.sf)
            elif db == "clickhouse":
                load_clickhouse(args.sf)
            elif db == "duckdb":
                load_duckdb(args.sf)
            elif db == "postgres":
                load_postgres(args.sf)
            else:
                print(f"Unknown database: {db}")
                continue
            print(f"\n*** {db} SF={args.sf} completed in {time.time()-t0:.1f}s ***")
        except Exception as e:
            print(f"\n!!! {db} SF={args.sf} FAILED: {e}")
            raise


if __name__ == "__main__":
    main()
