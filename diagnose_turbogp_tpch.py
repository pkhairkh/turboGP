#!/usr/bin/env python3
"""Run all 22 TPC-H queries on turboGP SF=1 and report which pass/fail."""
import subprocess
import time
import sys
from pathlib import Path

REPO = Path("/root/turboGP")
TURBOGP = REPO / "target/release/turbogp"
QUERIES = REPO / "benchmarks/tpch/queries/turbogp"

def start_turbogp():
    subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
    time.sleep(2)
    proc = subprocess.Popen(
        [str(TURBOGP), "--insecure", "--port", "55432", "--max-connections", "16",
         "--allow-copy-dir", "/srv/turbogp_csv/sf1"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    return proc

def run_query(sql):
    # Strip comment-only lines to avoid lexer issues
    sql_lines = [line for line in sql.split('\n') if not line.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines)
    sql_escaped = sql_clean.replace('"', "'").replace('\n', ' ').strip()
    cmd = f"psql -h 127.0.0.1 -p 55432 -U postgres -tAc \"{sql_escaped}\""
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=60, capture_output=True, text=True)
        ms = int((time.time()-t0)*1000)
        return p.returncode, p.stdout[:200], p.stderr[:300], ms
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)

def main():
    proc = start_turbogp()
    print(f"turboGP PID={proc.pid}")

    # Load schema + data
    schema = (REPO / "benchmarks/tpch/schema/turbogp.sql").read_text()
    with open("/tmp/tg_schema.sql", "w") as f:
        f.write(schema)
    subprocess.run(f"psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -f /tmp/tg_schema.sql",
                   shell=True, capture_output=True, timeout=30)
    for tbl in ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]:
        subprocess.run(
            f"psql -h 127.0.0.1 -p 55432 -U postgres -c \"COPY {tbl} FROM '/srv/turbogp_csv/sf1/{tbl}.csv' WITH (FORMAT csv, HEADER true)\"",
            shell=True, capture_output=True, timeout=600,
        )

    print("\n=== Running all 22 TPC-H queries on turboGP SF=1 ===\n")
    results = []
    for qnum in range(1, 23):
        qid = f"q{qnum:02d}"
        qf = QUERIES / f"{qid}.sql"
        if not qf.exists():
            print(f"{qid}: FILE NOT FOUND")
            results.append((qid, "MISSING", 0, ""))
            continue
        sql = qf.read_text()
        rc, out, err, ms = run_query(sql)
        status = "OK" if rc == 0 else "FAIL"
        err_short = err.replace('\n', ' ')[:150] if err else ""
        print(f"{qid}: {status} ({ms}ms) {err_short}")
        results.append((qid, status, ms, err_short))

    proc.terminate()
    try: proc.wait(timeout=5)
    except: proc.kill()

    print("\n=== Summary ===")
    ok = sum(1 for r in results if r[1] == "OK")
    fail = sum(1 for r in results if r[1] == "FAIL")
    print(f"OK: {ok}/22, FAIL: {fail}/22")
    print("\nFailing queries:")
    for qid, status, ms, err in results:
        if status == "FAIL":
            print(f"  {qid}: {err}")

if __name__ == "__main__":
    main()
