#!/usr/bin/env python3
"""W10: Generate ClickBench hits dataset (100M rows) and load into all 5 databases.

Uses the official ClickBench hits table schema (simplified to core columns).
Generates 100M rows of synthetic web analytics data.
Loads into turboGP, ClickHouse, DuckDB, PostgreSQL, and Exasol.
"""
import csv
import os
import random
import sys
import time
import subprocess
import ssl
import re
from pathlib import Path

REPO = Path("/root/turboGP")
DATA_DIR = REPO / "bench/queries/clickbench/data"
NUM_ROWS = 100_000_000  # 100M rows
BATCH_WRITE = 10_000_000  # Write in 10M row batches

# ClickBench hits table — simplified to 15 core columns that queries reference
COLUMNS = [
    ("WatchID", "int"),
    ("CounterID", "int"),
    ("EventDate", "date"),
    ("EventTime", "datetime"),
    ("UserID", "int"),
    ("RegionID", "int"),
    ("OS", "int"),
    ("UserAgent", "int"),
    ("URL", "str"),
    ("Referer", "str"),
    ("IsRefresh", "int"),
    ("RefererCategoryID", "int"),
    ("SendLog", "int"),
    ("Age", "int"),
    ("Sex", "int"),
]

def generate_row(row_id):
    """Generate a single synthetic ClickBench row."""
    return [
        row_id,                                          # WatchID
        random.randint(1, 1000),                         # CounterID
        f"2020-01-{1 + row_id % 30:02d}",                # EventDate
        f"2020-01-{1 + row_id % 30:02d} {random.randint(0,23):02d}:{random.randint(0,59):02d}:{random.randint(0,59):02d}",  # EventTime
        random.randint(1, 1000000),                      # UserID
        random.randint(1, 10000),                        # RegionID
        random.randint(1, 100),                          # OS
        random.randint(1, 1000),                         # UserAgent
        f"http://example.com/page_{row_id % 10000}",     # URL
        f"http://referer.com/ref_{row_id % 5000}",       # Referer
        random.randint(0, 1),                            # IsRefresh
        random.randint(1, 100),                          # RefererCategoryID
        random.randint(0, 1),                            # SendLog
        random.randint(0, 100),                          # Age
        random.randint(0, 1),                            # Sex
    ]

def generate_pipe_delimited(output_path, num_rows=NUM_ROWS):
    """Generate pipe-delimited file (no header) for database loading."""
    print(f"Generating {num_rows:,} rows → {output_path}")
    random.seed(42)
    with open(output_path, "w") as f:
        for i in range(1, num_rows + 1):
            row = generate_row(i)
            f.write("|".join(str(v) for v in row) + "\n")
            if i % 10_000_000 == 0:
                print(f"  {i:,} rows ({i/num_rows*100:.0f}%)")
                f.flush()
    size_gb = os.path.getsize(output_path) / (1024**3)
    print(f"Done. File size: {size_gb:.2f} GB")

def load_clickhouse():
    """Load into ClickHouse."""
    print("\n=== Loading ClickHouse ===")
    # Create table
    schema = """
CREATE TABLE IF NOT EXISTS hits (
    WatchID Int64,
    CounterID Int32,
    EventDate Date,
    EventTime String,
    UserID Int64,
    RegionID Int64,
    OS Int32,
    UserAgent Int32,
    URL String,
    Referer String,
    IsRefresh Int32,
    RefererCategoryID Int32,
    SendLog Int32,
    Age Int32,
    Sex Int32
) ENGINE = MergeTree ORDER BY (WatchID);
TRUNCATE TABLE hits;
"""
    with open("/tmp/cb_ch_schema.sql", "w") as f:
        f.write(schema)
    subprocess.run("docker exec -i clickhouse clickhouse-client --multiquery < /tmp/cb_ch_schema.sql",
                   shell=True, timeout=60)

    # Load data
    data_file = DATA_DIR / "hits.tbl"
    t0 = time.time()
    cmd = f"docker exec -i clickhouse clickhouse-client --query \"INSERT INTO hits FORMAT CustomSeparated\" --format_custom_field_delimiter=\"|\" --format_custom_row_after_delimiter=\"\\n\" < {data_file}"
    subprocess.run(cmd, shell=True, timeout=3600)
    print(f"  Loaded in {time.time()-t0:.1f}s")

    # Verify
    r = subprocess.run("docker exec clickhouse clickhouse-client --query 'SELECT COUNT(*) FROM hits'",
                       shell=True, capture_output=True, text=True, timeout=60)
    print(f"  Row count: {r.stdout.strip()}")

def load_duckdb():
    """Load into DuckDB."""
    print("\n=== Loading DuckDB ===")
    db_path = "/srv/duckdb/clickbench.duckdb"
    if os.path.exists(db_path):
        os.remove(db_path)

    schema = """
CREATE TABLE hits (
    WatchID BIGINT,
    CounterID INTEGER,
    EventDate VARCHAR,
    EventTime VARCHAR,
    UserID BIGINT,
    RegionID BIGINT,
    OS INTEGER,
    UserAgent INTEGER,
    URL VARCHAR,
    Referer VARCHAR,
    IsRefresh INTEGER,
    RefererCategoryID INTEGER,
    SendLog INTEGER,
    Age INTEGER,
    Sex INTEGER
);
"""
    with open("/tmp/cb_duck_schema.sql", "w") as f:
        f.write(schema)
    subprocess.run(f"/usr/local/bin/duckdb {db_path} < /tmp/cb_duck_schema.sql", shell=True, timeout=60)

    data_file = DATA_DIR / "hits.tbl"
    t0 = time.time()
    sql = f"COPY hits FROM '{data_file}' (DELIMITER '|', HEADER false, NULL '');"
    subprocess.run(f'/usr/local/bin/duckdb {db_path} -c "{sql}"', shell=True, timeout=3600)
    print(f"  Loaded in {time.time()-t0:.1f}s")

    r = subprocess.run(f"/usr/local/bin/duckdb {db_path} -noheader -list -c 'SELECT COUNT(*) FROM hits'",
                       shell=True, capture_output=True, text=True, timeout=60)
    print(f"  Row count: {r.stdout.strip()}")

def load_postgres():
    """Load into PostgreSQL."""
    print("\n=== Loading PostgreSQL ===")
    subprocess.run("docker exec postgres psql -U postgres -p 5433 -c 'DROP DATABASE IF EXISTS clickbench'",
                   shell=True, timeout=60)
    subprocess.run("docker exec postgres psql -U postgres -p 5433 -c 'CREATE DATABASE clickbench'",
                   shell=True, timeout=60)

    schema = """
CREATE TABLE hits (
    WatchID BIGINT, CounterID INTEGER, EventDate VARCHAR, EventTime VARCHAR,
    UserID BIGINT, RegionID BIGINT, OS INTEGER, UserAgent INTEGER,
    URL VARCHAR, Referer VARCHAR, IsRefresh INTEGER, RefererCategoryID INTEGER,
    SendLog INTEGER, Age INTEGER, Sex INTEGER
);
"""
    with open("/tmp/cb_pg_schema.sql", "w") as f:
        f.write(schema)
    subprocess.run("docker exec -i postgres psql -U postgres -p 5433 -d clickbench < /tmp/cb_pg_schema.sql",
                   shell=True, timeout=60)

    data_file = DATA_DIR / "hits.tbl"
    t0 = time.time()
    cmd = f"sed 's/|$//' {data_file} | docker exec -i postgres psql -U postgres -p 5433 -d clickbench -c \"\\COPY hits FROM STDIN WITH (FORMAT csv, DELIMITER '|', NULL '')\""
    subprocess.run(cmd, shell=True, timeout=3600)
    print(f"  Loaded in {time.time()-t0:.1f}s")

    r = subprocess.run("docker exec postgres psql -U postgres -p 5433 -d clickbench -tAc 'SELECT COUNT(*) FROM hits'",
                       shell=True, capture_output=True, text=True, timeout=60)
    print(f"  Row count: {r.stdout.strip()}")

def load_exasol():
    """Load into Exasol."""
    print("\n=== Loading Exasol ===")
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

    conn = pyexasol.connect(
        dsn="localhost:8563", user="sys", password="exasol",
        websocket_sslopt={"cert_reqs": ssl.CERT_NONE},
    )
    conn.execute("DROP SCHEMA IF EXISTS CLICKBENCH CASCADE")
    conn.execute("CREATE SCHEMA CLICKBENCH")
    conn.execute("""CREATE TABLE CLICKBENCH.hits (
        WatchID DECIMAL(18,0), CounterID DECIMAL(10,0), EventDate VARCHAR(30), EventTime VARCHAR(30),
        UserID DECIMAL(18,0), RegionID DECIMAL(18,0), OS DECIMAL(10,0), UserAgent DECIMAL(10,0),
        URL VARCHAR(500), Referer VARCHAR(500), IsRefresh DECIMAL(10,0), RefererCategoryID DECIMAL(10,0),
        SendLog DECIMAL(10,0), Age DECIMAL(10,0), Sex DECIMAL(10,0)
    )""")

    data_file = DATA_DIR / "hits.tbl"
    t0 = time.time()
    BATCH = 50_000
    batch = []
    with open(data_file, "r") as f:
        for line in f:
            line = line.rstrip("\r\n")
            if not line: continue
            if line.endswith("|"): line = line[:-1]
            batch.append(line.split("|"))
            if len(batch) >= BATCH:
                conn.import_from_iterable(batch, ("CLICKBENCH", "hits"))
                batch = []
    if batch:
        conn.import_from_iterable(batch, ("CLICKBENCH", "hits"))
    print(f"  Loaded in {time.time()-t0:.1f}s")

    r = conn.execute("SELECT COUNT(*) FROM CLICKBENCH.hits")
    print(f"  Row count: {r.fetchone()[0]}")
    conn.close()

def load_turbogp():
    """Load into turboGP (in-memory). Data is loaded at benchmark time, not here."""
    print("\n=== turboGP: will load at benchmark time (in-memory) ===")
    # Generate CSV with header for turboGP
    csv_path = DATA_DIR / "hits.csv"
    tbl_path = DATA_DIR / "hits.tbl"
    if not csv_path.exists():
        print("  Converting .tbl to CSV with header...")
        with open(tbl_path, "r") as fin, open(csv_path, "w", newline="") as fout:
            w = csv.writer(fout)
            w.writerow([c[0] for c in COLUMNS])
            for line in fin:
                line = line.rstrip("\r\n")
                if not line: continue
                if line.endswith("|"): line = line[:-1]
                w.writerow(line.split("|"))
    print(f"  CSV ready at {csv_path}")

def main():
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    # Step 1: Generate data
    tbl_path = DATA_DIR / "hits.tbl"
    if not tbl_path.exists() or tbl_path.stat().st_size < 1_000_000_000:  # < 1GB
        generate_pipe_delimited(tbl_path)
    else:
        print(f"Data file already exists: {tbl_path.stat().st_size / (1024**3):.2f} GB")

    # Step 2: Load into all databases
    load_clickhouse()
    load_duckdb()
    load_postgres()
    load_exasol()
    load_turbogp()

    print("\n=== All databases loaded ===")

if __name__ == "__main__":
    main()
