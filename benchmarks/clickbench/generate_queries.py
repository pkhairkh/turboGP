#!/usr/bin/env python3
"""Wave 6: Generate 43 ClickBench queries for 4 dialects."""
from pathlib import Path

REPO = Path("/root/turboGP")
QUERIES_DIR = REPO / "benchmarks/clickbench/queries"

# ClickBench standard 43 queries (from https://github.com/ClickHouse/ClickBench)
# Simplified for our schema (10-column subset)
QUERIES = {
    1: "SELECT COUNT(*) FROM hits",
    2: "SELECT COUNT(*) FROM hits WHERE CounterID = 500",
    3: "SELECT COUNT(*) FROM hits WHERE CounterID = 500 AND EventDate = '2020-01-15'",
    4: "SELECT COUNT(*) FROM hits WHERE CounterID IN (SELECT CounterID FROM hits WHERE UserID = 12345)",
    5: "SELECT SUM(WatchID) FROM hits",
    6: "SELECT SUM(WatchID) FROM hits WHERE CounterID = 500",
    7: "SELECT AVG(WatchID) FROM hits",
    8: "SELECT MIN(WatchID), MAX(WatchID) FROM hits",
    9: "SELECT COUNT(DISTINCT UserID) FROM hits",
    10: "SELECT COUNT(DISTINCT URL) FROM hits",
    11: "SELECT CounterID, COUNT(*) FROM hits GROUP BY CounterID ORDER BY COUNT(*) DESC LIMIT 10",
    12: "SELECT CounterID, SUM(WatchID) FROM hits GROUP BY CounterID ORDER BY SUM(WatchID) DESC LIMIT 10",
    13: "SELECT RegionID, COUNT(*) FROM hits GROUP BY RegionID ORDER BY COUNT(*) DESC LIMIT 10",
    14: "SELECT OS, COUNT(*) FROM hits GROUP BY OS ORDER BY COUNT(*) DESC LIMIT 10",
    15: "SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10",
    16: "SELECT CounterID, COUNT(*) FROM hits WHERE EventDate = '2020-01-15' GROUP BY CounterID ORDER BY COUNT(*) DESC LIMIT 10",
    17: "SELECT CounterID, MIN(WatchID), MAX(WatchID) FROM hits GROUP BY CounterID",
    18: "SELECT CounterID, AVG(WatchID) FROM hits GROUP BY CounterID",
    19: "SELECT COUNT(*) FROM hits WHERE WatchID % 2 = 0",
    20: "SELECT SUM(WatchID) FROM hits WHERE WatchID > 50000000",
    21: "SELECT COUNT(*) FROM hits WHERE Title LIKE '%Title_1%'",
    22: "SELECT COUNT(*) FROM hits WHERE URL LIKE '%page_1%'",
    23: "SELECT COUNT(*) FROM hits WHERE CounterID BETWEEN 100 AND 200",
    24: "SELECT COUNT(*) FROM hits WHERE UserID BETWEEN 1 AND 100000",
    25: "SELECT EventDate, COUNT(*) FROM hits GROUP BY EventDate ORDER BY EventDate",
    26: "SELECT EventDate, SUM(WatchID) FROM hits GROUP BY EventDate ORDER BY EventDate",
    27: "SELECT CounterID, UserID, COUNT(*) FROM hits GROUP BY CounterID, UserID ORDER BY COUNT(*) DESC LIMIT 100",
    28: "SELECT CounterID, RegionID, COUNT(*) FROM hits GROUP BY CounterID, RegionID",
    29: "SELECT COUNT(*) FROM hits WHERE CounterID = 500 AND UserID = 12345",
    30: "SELECT COUNT(*) FROM hits WHERE CounterID = 500 OR UserID = 12345",
    31: "SELECT COUNT(*) FROM hits WHERE NOT (CounterID = 500)",
    32: "SELECT COUNT(*) FROM hits WHERE CounterID != 500",
    33: "SELECT COUNT(*) FROM hits WHERE UserID IN (1, 2, 3, 4, 5)",
    34: "SELECT COUNT(*) FROM hits WHERE CounterID NOT IN (1, 2, 3, 4, 5)",
    35: "SELECT COUNT(*) FROM hits WHERE EventDate IN ('2020-01-01', '2020-01-02', '2020-01-03')",
    36: "SELECT SUM(WatchID) FROM hits GROUP BY CounterID HAVING SUM(WatchID) > 1000000",
    37: "SELECT CounterID, COUNT(*) FROM hits GROUP BY CounterID ORDER BY CounterID",
    38: "SELECT COUNT(*) FROM (SELECT CounterID FROM hits GROUP BY CounterID) AS sub",
    39: "SELECT COUNT(*) FROM (SELECT DISTINCT UserID FROM hits) AS sub",
    40: "SELECT MIN(WatchID) FROM hits WHERE CounterID = 500",
    41: "SELECT MAX(WatchID) FROM hits WHERE CounterID = 500",
    42: "SELECT AVG(WatchID) FROM hits WHERE CounterID = 500",
    43: "SELECT STDDEV(WatchID) FROM hits",
}

def main():
    for dialect in ["standard", "turbogp", "clickhouse", "duckdb", "postgres"]:
        d = QUERIES_DIR / dialect
        d.mkdir(parents=True, exist_ok=True)
        for qnum, sql in QUERIES.items():
            (d / f"q{qnum:02d}.sql").write_text(sql + ";")
        print(f"  {dialect}: {len(QUERIES)} queries")

if __name__ == "__main__":
    main()
