#!/usr/bin/env python3
"""Load TPC-H data into Exasol for SF=1 and SF=10.

Uses pyexasol's import_from_iterable for efficient bulk loading (~32k rows/sec).
Exasol correctly handles DECIMAL arithmetic (unlike turboGP).
"""
import csv
import ssl
import re
import sys
import time
from pathlib import Path
import pyexasol
import pyexasol.connection as pc
from packaging.version import Version, InvalidVersion

REPO = Path("/root/turboGP")
DATA_DIR = REPO / "benchmarks/tpch/data"
SCHEMA_DIR = REPO / "benchmarks/tpch/schema"

TABLES = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]

# Patch version parser for exasol/nano
def patched_version(self):
    rv = self.login_info.get('releaseVersion')
    if rv:
        try:
            return Version(rv)
        except InvalidVersion:
            return Version(re.sub(r'-.*$', '', rv))
    return None
pc.ExaConnection.exasol_db_version = property(patched_version)

def connect():
    return pyexasol.connect(
        dsn="localhost:8563", user="sys", password="exasol",
        websocket_sslopt={"cert_reqs": ssl.CERT_NONE},
    )

def read_tbl(tbl_path):
    """Read a .tbl file (pipe-separated, trailing pipe) and yield rows as lists."""
    with open(tbl_path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip("\r\n")
            if not line: continue
            if line.endswith("|"): line = line[:-1]
            yield line.split("|")

def load_sf(sf):
    print(f"\n========== Exasol SF={sf} ==========")
    conn = connect()
    print("Connected to Exasol.")

    schema_name = f"TPCH_SF{sf}"
    conn.execute(f"DROP SCHEMA IF EXISTS {schema_name} CASCADE")
    conn.execute(f"CREATE SCHEMA {schema_name}")
    print(f"Created schema {schema_name}")

    # Apply DDL from exasol.sql, prefixing table names with schema
    ddl = (SCHEMA_DIR / "exasol.sql").read_text()
    # Remove comment lines and split into statements
    statements = []
    for line in ddl.split("\n"):
        line = line.strip()
        if line.startswith("--") or not line:
            continue
        statements.append(line)
    ddl_text = " ".join(statements)
    for stmt in ddl_text.split(";"):
        stmt = stmt.strip()
        if not stmt: continue
        # Add schema prefix
        stmt = stmt.replace("DROP TABLE IF EXISTS ", f"DROP TABLE IF EXISTS {schema_name}.")
        stmt = stmt.replace("CREATE TABLE ", f"CREATE TABLE {schema_name}.")
        conn.execute(stmt)
    print(f"Created {len(TABLES)} tables")

    # Load each table in batches to avoid OOM on large tables
    BATCH_SIZE = 100_000
    for tbl in TABLES:
        tbl_path = DATA_DIR / f"sf{sf}" / f"{tbl}.tbl"
        print(f"  loading {tbl}...", end=" ", flush=True)
        t0 = time.time()
        total = 0
        batch = []
        for row in read_tbl(tbl_path):
            batch.append(row)
            if len(batch) >= BATCH_SIZE:
                conn.import_from_iterable(batch, (schema_name, tbl))
                total += len(batch)
                batch = []
        if batch:
            conn.import_from_iterable(batch, (schema_name, tbl))
            total += len(batch)
        elapsed = time.time() - t0
        print(f"{total} rows in {elapsed:.1f}s ({total/max(elapsed,0.01):.0f} rows/sec)")

    # Verify row counts
    print("\n  Row counts:")
    for tbl in TABLES:
        result = conn.execute(f"SELECT COUNT(*) FROM {schema_name}.{tbl}")
        count = result.fetchone()[0]
        print(f"    {tbl}: {count}")

    conn.close()
    print(f"\nExasol SF={sf} load complete.")

def main():
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--sf", type=int, nargs="+", default=[1, 10])
    args = ap.parse_args()
    for sf in args.sf:
        load_sf(sf)

if __name__ == "__main__":
    main()
