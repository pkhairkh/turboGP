#!/usr/bin/env python3
"""Fix benchmark scripts: use stdout=DEVNULL without shell redirect.

The previous patch added `> /dev/null 2>&1` to the shell command, which
causes an extra fork. This patch uses only subprocess.DEVNULL (no shell
redirect), which is faster.
"""

# --- Patch benchmark_cached.py ---
PATH2 = "benchmark_cached.py"
with open(PATH2) as f:
    src2 = f.read()

old_rq = '''def run_query(sql):
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

new_rq = '''def run_query(sql):
    sql_lines = [l for l in sql.split('\\n') if not l.strip().startswith('--')]
    sql_clean = '\\n'.join(sql_lines).replace('"', "'").replace('\\n', ' ').strip()
    cmd = f'psql -h 127.0.0.1 -p 55432 -U postgres -tAc "{sql_clean}"'
    t0 = time.time()
    try:
        # W2: stdout=DEVNULL avoids Python pipe overhead for high-row-count
        # queries (Q16 returns 18,314 rows). No shell redirect needed.
        p = subprocess.run(cmd, shell=True, timeout=120,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ms = int((time.time()-t0)*1000)
        return p.returncode, ms
    except:
        return 124, int((time.time()-t0)*1000)'''

assert old_rq in src2, f"run_query pattern not found in {PATH2}"
src2 = src2.replace(old_rq, new_rq, 1)
with open(PATH2, "w") as f:
    f.write(src2)
print(f"Fixed {PATH2}: removed shell redirect, kept subprocess.DEVNULL")

# --- Patch run_5db_benchmark_v2.py ---
PATH1 = "run_5db_benchmark_v2.py"
with open(PATH1) as f:
    src = f.read()

old_run = '''def run(cmd, timeout=TIMEOUT):
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

new_run = '''def run(cmd, timeout=TIMEOUT):
    # W2 (cache phase): stdout=DEVNULL avoids Python pipe overhead for
    # high-row-count queries. stderr is captured for error diagnostics.
    t0 = time.time()
    try:
        p = subprocess.run(cmd, shell=True, timeout=timeout,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        return p.returncode, "", p.stderr, int((time.time()-t0)*1000)
    except subprocess.TimeoutExpired:
        return 124, "", "TIMEOUT", int((time.time()-t0)*1000)
    except Exception as e:
        return 1, "", str(e), int((time.time()-t0)*1000)'''

assert old_run in src, f"run() pattern not found in {PATH1}"
src = src.replace(old_run, new_run, 1)
with open(PATH1, "w") as f:
    f.write(src)
print(f"Fixed {PATH1}: removed shell redirect, kept subprocess.DEVNULL")
