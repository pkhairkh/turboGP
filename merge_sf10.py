#!/usr/bin/env python3
"""Merge native SF=10 results (Q01-Q17) with psql-based results (Q18-Q22).
psql times are in milliseconds; native times are in microseconds.
For Q18-Q22, convert psql ms to us and note the source.
"""
import csv
from pathlib import Path

NATIVE_SF10 = Path("/root/turboGP/benchmarks/tpch/results/native_sf10.csv")
PSQL_SF10 = Path("/root/turboGP/benchmarks/tpch/results/sf10_cached_results.csv")
OUTPUT = Path("/root/turboGP/benchmarks/tpch/results/native_sf10_merged.csv")

# Read native results (Q01-Q17)
native_rows = {}
with open(NATIVE_SF10) as f:
    reader = csv.DictReader(f)
    for row in reader:
        native_rows[row["query_id"]] = row

# Read psql results (all 22 queries, times in ms)
psql_rows = {}
with open(PSQL_SF10) as f:
    reader = csv.DictReader(f)
    for row in reader:
        psql_rows[row["query_id"]] = row

# Merge: Q01-Q17 from native, Q18-Q22 from psql (converted ms->us)
fieldnames = ["query_id", "cold_us", "cold_rows", "cold_status",
              "hot1_us", "hot1_rows", "hot1_status",
              "hot2_us", "hot2_rows", "hot2_status",
              "hot3_us", "hot3_rows", "hot3_status", "source"]

with open(OUTPUT, "w", newline="") as f:
    writer = csv.DictWriter(f, fieldnames=fieldnames)
    writer.writeheader()
    for qnum in range(1, 23):
        qid = f"q{qnum:02d}"
        if qid in native_rows:
            row = dict(native_rows[qid])
            row["source"] = "native"
            writer.writerow(row)
        elif qid in psql_rows:
            # Convert psql ms to us
            p = psql_rows[qid]
            row = {
                "query_id": qid,
                "cold_us": int(p["cold_ms"]) * 1000,
                "cold_rows": 0,
                "cold_status": p["status"],
                "hot1_us": int(p["hot1_ms"]) * 1000,
                "hot1_rows": 0,
                "hot1_status": p["status"],
                "hot2_us": int(p["hot2_ms"]) * 1000,
                "hot2_rows": 0,
                "hot2_status": p["status"],
                "hot3_us": int(p["hot3_ms"]) * 1000,
                "hot3_rows": 0,
                "hot3_status": p["status"],
                "source": "psql_ms_to_us",
            }
            writer.writerow(row)

print(f"Merged results saved to {OUTPUT}")
print(f"Native: {len(native_rows)} queries, PSQL: {len([q for q in psql_rows if q not in native_rows])} queries")
