#!/usr/bin/env python3
"""Run all 22 TPC-H queries on turboGP SF=1 + SF=10 with the optimized binary.
Outputs results for comparison.
"""
import subprocess, time, sys, os
from pathlib import Path

REPO = Path("/root/turboGP")
TURBOGP = REPO / "target/release/turbogp"
QUERIES = REPO / "benchmarks/tpch/queries/turbogp"

def start_turbogp(sf):
    subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
    time.sleep(2)
    csv_dir = f"/srv/turbogp_csv/sf{sf}"
    proc = subprocess.Popen(
        [str(TURBOGP), "--insecure", "--port", "55432", "--max-connections", "4",
         "--allow-copy-dir", csv_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    # Load schema + data
    schema = (REPO / "benchmarks/tpch/schema/turbogp.sql").read_text()
    with open("/tmp/tg_schema.sql", "w") as f: f.write(schema)
    subprocess.run(f"psql -h 127.0.0.1 -p 55432 -U postgres -f /tmp/tg_schema.sql",
                   shell=True, capture_output=True, timeout=30)
    for tbl in ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]:
        subprocess.run(
            f"psql -h 127.0.0.1 -p 55432 -U postgres -c \"COPY {tbl} FROM '/srv/turbogp_csv/sf{sf}/{tbl}.csv' WITH (FORMAT csv, HEADER true)\"",
            shell=True, capture_output=True, timeout=600,
        )
    return proc

def run_query(sql):
    sql_lines = [l for l in sql.split('\n') if not l.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines).replace('"', "'").replace('\n', ' ').strip()
    cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_clean}"'
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=120, capture_output=True, text=True)
        ms = int((time.time()-t0)*1000)
        return p.returncode, ms, p.stdout[:100], p.stderr[:200]
    except subprocess.TimeoutExpired:
        return 124, int((time.time()-t0)*1000), "", "TIMEOUT"

def benchmark_sf(sf):
    print(f"\n{'='*60}")
    print(f"  turboGP TPC-H SF={sf} (optimized binary)")
    print(f"{'='*60}")
    proc = start_turbogp(sf)

    results = []
    for qnum in range(1, 23):
        qid = f"q{qnum:02d}"
        qf = QUERIES / f"{qid}.sql"
        sql = qf.read_text()
        # Warmup
        run_query(sql)
        # 3 hot runs, take best
        best = 999999
        for i in range(3):
            rc, ms, out, err = run_query(sql)
            if rc == 0 and ms < best:
                best = ms
        if best == 999999:
            best = 0
            print(f"  {qid}: FAIL — {err[:80]}")
        else:
            print(f"  {qid}: {best}ms")
        results.append((qid, best))

    proc.terminate()
    try: proc.wait(timeout=5)
    except: proc.kill()
    return results

def main():
    import math
    sf1 = benchmark_sf(1)
    sf10 = benchmark_sf(10)

    print("\n\n=== SUMMARY ===")
    for label, results in [("SF=1", sf1), ("SF=10", sf10)]:
        times = [r[1] for r in results if r[1] > 0]
        gm = math.exp(sum(math.log(t) for t in times)/len(times)) if times else 0
        print(f"\n{label}: geomean={gm:.1f}ms, total={sum(times)}ms")
        for qid, ms in results:
            print(f"  {qid}: {ms}ms")

if __name__ == "__main__":
    main()
