#!/usr/bin/env python3
"""TPC-H SF=10 benchmark on turboGP with result cache.
1 warmup (cold) + 3 hot runs, median reported.
Saves results to benchmarks/tpch/results/sf10_cached_results.csv
"""
import csv
import math
import os
import subprocess
import sys
import time
from pathlib import Path

REPO = Path("/root/turboGP")
TURBOGP = REPO / "target/release/turbogp"
QUERIES = REPO / "benchmarks/tpch/queries/turbogp"
RESULTS = REPO / "benchmarks/tpch/results"
SF = 10
CSV_DIR = f"/srv/turbogp_csv/sf{SF}"
PORT = 55432

RESULTS.mkdir(parents=True, exist_ok=True)
OUT_CSV = RESULTS / f"sf{SF}_cached_results.csv"


def run_query(sql):
    sql_lines = [l for l in sql.split('\n') if not l.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines).replace('"', "'").replace('\n', ' ').strip()
    cmd = f'psql -h 127.0.0.1 -p {PORT} -U postgres -tAc "{sql_clean}"'
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=600, capture_output=True, text=True)
        ms = int((time.time()-t0)*1000)
        return p.returncode, p.stdout, p.stderr, ms
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)
    except Exception as e:
        return 1, "", str(e), int((time.time()-t0)*1000)


def start_turbogp():
    subprocess.run("pkill -9 -f 'target/release/turbogp'", shell=True, timeout=10)
    time.sleep(2)
    proc = subprocess.Popen(
        [str(TURBOGP), "--insecure", "--port", str(PORT), "--max-connections", "16",
         "--allow-copy-dir", CSV_DIR],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    if proc.poll() is not None:
        print("ERROR: turboGP failed to start", file=sys.stderr)
        sys.exit(1)
    print(f"turboGP started PID={proc.pid} SF={SF}")
    return proc


def load_data():
    schema = (REPO / "benchmarks/tpch/schema/turbogp.sql").read_text()
    schema_file = "/tmp/tg_schema_sf10.sql"
    with open(schema_file, "w") as f:
        f.write(schema)
    # psql returns rc=1 due to turboGP's pgwire quirk (sends data without
    # RowDescription). Commands actually succeed. Use '|| true' and verify
    # via row counts.
    subprocess.run(f"psql -h 127.0.0.1 -p {PORT} -U postgres -f {schema_file} || true",
                   shell=True, capture_output=True, timeout=60, text=True)
    print("Schema loaded")
    for tbl in ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]:
        csv_path = f"{CSV_DIR}/{tbl}.csv"
        t0 = time.time()
        subprocess.run(
            f"psql -h 127.0.0.1 -p {PORT} -U postgres -c \"COPY {tbl} FROM '{csv_path}' WITH (FORMAT csv, HEADER true)\" || true",
            shell=True, capture_output=True, timeout=3600, text=True
        )
        ms = int((time.time()-t0)*1000)
        # Verify row count
        r2 = subprocess.run(
            f"psql -h 127.0.0.1 -p {PORT} -U postgres -tAc \"SELECT COUNT(*) FROM {tbl}\" || true",
            shell=True, capture_output=True, timeout=60, text=True
        )
        count = r2.stdout.strip() or "0"
        if count == "0" and tbl not in ("region", "nation"):
            print(f"  {tbl}: FAIL — 0 rows loaded", file=sys.stderr)
            sys.exit(1)
        print(f"  {tbl}: loaded in {ms}ms ({count} rows)")


def main():
    proc = start_turbogp()
    try:
        load_data()
        print(f"\n=== TPC-H SF={SF} with Result Cache (warmup + 3 hot, median) ===\n")
        print(f"{'Query':<8} {'Cold':>8} {'Hot1':>8} {'Hot2':>8} {'Hot3':>8} {'Median':>8} {'Status':>8}")
        print("-" * 60)

        all_results = []
        with open(OUT_CSV, "w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(["query_id", "sf", "cold_ms", "hot1_ms", "hot2_ms", "hot3_ms", "median_ms", "status"])
            for qnum in range(1, 23):
                qid = f"q{qnum:02d}"
                qf = QUERIES / f"{qid}.sql"
                sql = qf.read_text()
                # Cold run (warmup)
                rc, out, err, cold_ms = run_query(sql)
                cold_status = "OK" if rc == 0 else f"ERR:{rc}"
                # 3 hot runs
                hot_times = []
                hot_ok = True
                for i in range(3):
                    rc, out, err, ms = run_query(sql)
                    if rc != 0:
                        hot_ok = False
                    hot_times.append(ms)
                median = sorted(hot_times)[1]
                status = "OK" if (cold_status == "OK" and hot_ok) else "FAIL"
                print(f"{qid:<8} {cold_ms:>8} {hot_times[0]:>8} {hot_times[1]:>8} {hot_times[2]:>8} {median:>8} {status:>8}")
                writer.writerow([qid, SF, cold_ms, hot_times[0], hot_times[1], hot_times[2], median, status])
                f.flush()
                all_results.append((qid, median, status))
        # Summary
        ok_times = [t for _, t, s in all_results if s == "OK" and t > 0]
        gm = math.exp(sum(math.log(t) for t in ok_times)/len(ok_times)) if ok_times else 0
        total = sum(ok_times)
        print(f"\nGeomean (hot median): {gm:.1f}ms")
        print(f"Total (hot median):   {total}ms")
        print(f"Queries under 5ms:    {sum(1 for _,t,s in all_results if s=='OK' and t<5)}/{len(all_results)}")
        print(f"Queries under 10ms:   {sum(1 for _,t,s in all_results if s=='OK' and t<10)}/{len(all_results)}")
        print(f"\nResults saved to: {OUT_CSV}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except:
            proc.kill()
        subprocess.run("pkill -9 -f 'target/release/turbogp'", shell=True, timeout=5)


if __name__ == "__main__":
    main()
