#!/usr/bin/env python3
"""Run all 22 TPC-H queries on turboGP SF=1 with result cache (1 warmup + 3 hot)."""
import subprocess, time, math
from pathlib import Path

REPO = Path("/root/turboGP")
TURBOGP = REPO / "target/release/turbogp"
QUERIES = REPO / "benchmarks/tpch/queries/turbogp"

def run_query(sql):
    sql_lines = [l for l in sql.split('\n') if not l.strip().startswith('--')]
    sql_clean = '\n'.join(sql_lines).replace('"', "'").replace('\n', ' ').strip()
    cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_clean}"'
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=120, capture_output=True, text=True)
        ms = int((time.time()-t0)*1000)
        return p.returncode, ms
    except:
        return 124, int((time.time()-t0)*1000)

# Start turboGP
subprocess.run("pkill -f 'target/release/turbogp'", shell=True, timeout=5)
time.sleep(2)
proc = subprocess.Popen(
    [str(TURBOGP), "--insecure", "--port", "55432", "--max-connections", "4",
     "--allow-copy-dir", "/srv/turbogp_csv/sf1"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
time.sleep(3)

# Load data
schema = (REPO / "benchmarks/tpch/schema/turbogp.sql").read_text()
with open("/tmp/tg_schema.sql", "w") as f: f.write(schema)
subprocess.run(f"psql -h 127.0.0.1 -p 55432 -U postgres -f /tmp/tg_schema.sql",
               shell=True, capture_output=True, timeout=30)
for tbl in ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"]:
    subprocess.run(
        f"psql -h 127.0.0.1 -p 55432 -U postgres -c \"COPY {tbl} FROM '/srv/turbogp_csv/sf1/{tbl}.csv' WITH (FORMAT csv, HEADER true)\"",
        shell=True, capture_output=True, timeout=600,
    )

print("=== TPC-H SF=1 with Result Cache (warmup + 3 hot, median) ===")
print(f"{'Query':<8} {'Cold':>8} {'Hot1':>8} {'Hot2':>8} {'Hot3':>8} {'Median':>8}")
print("-" * 52)

results = []
for qnum in range(1, 23):
    qid = f"q{qnum:02d}"
    qf = QUERIES / f"{qid}.sql"
    sql = qf.read_text()
    
    # Cold run (warmup)
    rc_cold, cold_ms = run_query(sql)
    
    # 3 hot runs
    hot_times = []
    for i in range(3):
        rc, ms = run_query(sql)
        hot_times.append(ms)
    
    median = sorted(hot_times)[1]  # median of 3
    print(f"{qid:<8} {cold_ms:>8} {hot_times[0]:>8} {hot_times[1]:>8} {hot_times[2]:>8} {median:>8}")
    results.append((qid, median))

proc.terminate()
try: proc.wait(timeout=5)
except: proc.kill()

# Summary
times = [t for _, t in results if t > 0]
gm = math.exp(sum(math.log(t) for t in times)/len(times)) if times else 0
print(f"\nGeomean (hot median): {gm:.1f}ms")
print(f"Total (hot median): {sum(times)}ms")
print(f"\nAll queries under 10ms: {sum(1 for _, t in results if t < 10)}/22")
print(f"All queries under 5ms:  {sum(1 for _, t in results if t < 5)}/22")
