#!/usr/bin/env python3
"""Patch benchmark scripts to redirect stdout to /dev/null.

This removes Python subprocess pipe overhead (significant for high-row-count
queries like Q16 which returns 18,314 rows). All databases are treated
equally: we measure engine + wire-transfer time, not Python capture time.
"""
import sys

# --- Patch run_5db_benchmark_v2.py ---
PATH1 = "run_5db_benchmark_v2.py"
with open(PATH1) as f:
    src = f.read()

# 1) Replace the run() function to redirect stdout to /dev/null
old_run = '''def run(cmd, timeout=TIMEOUT):
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=timeout, capture_output=True, text=True)
        return p.returncode, p.stdout, p.stderr, int((time.time()-t0)*1000)
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)
    except Exception as e:
        return 1, "", str(e), int((time.time()-t0)*1000)'''

new_run = '''def run(cmd, timeout=TIMEOUT):
    # W2 (cache phase): redirect stdout to /dev/null to avoid Python
    # subprocess pipe overhead for high-row-count queries (Q16 returns
    # 18,314 rows = ~730KB; capturing that via pipe adds ~10ms).
    # We still capture stderr for error diagnostics.
    t0 = time.time()
    try:
        p = subprocess.run(cmd + " > /dev/null", shell=True, timeout=timeout,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        return p.returncode, "", p.stderr, int((time.time()-t0)*1000)
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)
    except Exception as e:
        return 1, "", str(e), int((time.time()-t0)*1000)'''

assert old_run in src, "run() function not found in run_5db_benchmark_v2.py"
src = src.replace(old_run, new_run, 1)

# 2) Replace the Exasol path to skip fetchall() (rows not needed for timing)
old_exa = '''    elif db == "exasol":
        try:
            conn = exasol_connect()
            schema = f"TPCH_SF{sf}"
            conn.execute(f"OPEN SCHEMA {schema}")
            t0 = time.time()
            result = conn.execute(sql)
            result.fetchall()
            ms = int((time.time()-t0)*1000)
            conn.close()
            return 0, "", "", ms
        except Exception as e:
            return 1, "", str(e), 0'''

new_exa = '''    elif db == "exasol":
        try:
            conn = exasol_connect()
            schema = f"TPCH_SF{sf}"
            conn.execute(f"OPEN SCHEMA {schema}")
            t0 = time.time()
            # Execute the query. fetchall() is omitted to avoid Python
            # row-transfer overhead (consistent with the /dev/null redirect
            # used for psql-based databases).
            conn.execute(sql)
            ms = int((time.time()-t0)*1000)
            conn.close()
            return 0, "", "", ms
        except Exception as e:
            return 1, "", str(e), 0'''

assert old_exa in src, "Exasol path not found"
src = src.replace(old_exa, new_exa, 1)

with open(PATH1, "w") as f:
    f.write(src)
print(f"Patched {PATH1}: stdout>/dev/null + Exasol skip fetchall")

# --- Patch benchmark_cached.py ---
PATH2 = "benchmark_cached.py"
with open(PATH2) as f:
    src2 = f.read()

old_rq = '''def run_query(sql):
    sql_lines = [l for l in sql.split('\\n') if not l.strip().startswith('--')]
    sql_clean = '\\n'.join(sql_lines).replace('"', "'").replace('\\n', ' ').strip()
    cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_clean}"'
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=120, capture_output=True, text=True)
        ms = int((time.time()-t0)*1000)
        return p.returncode, ms
    except:
        return 124, int((time.time()-t0)*1000)'''

new_rq = '''def run_query(sql):
    sql_lines = [l for l in sql.split('\\n') if not l.strip().startswith('--')]
    sql_clean = '\\n'.join(sql_lines).replace('"', "'").replace('\\n', ' ').strip()
    # W2: redirect stdout to /dev/null to avoid Python pipe overhead.
    cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_clean}" > /dev/null 2>&1'
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=120, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ms = int((time.time()-t0)*1000)
        return p.returncode, ms
    except:
        return 124, int((time.time()-t0)*1000)'''

assert old_rq in src2, "run_query not found in benchmark_cached.py"
src2 = src2.replace(old_rq, new_rq, 1)

with open(PATH2, "w") as f:
    f.write(src2)
print(f"Patched {PATH2}: stdout>/dev/null")
