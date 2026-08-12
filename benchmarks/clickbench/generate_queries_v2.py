#!/usr/bin/env python3
"""W11: Generate 43 ClickBench queries for 5 databases.

ClickBench standard queries adapted from https://github.com/ClickHouse/ClickBench
Simplified for our 15-column hits schema.
"""
from pathlib import Path

REPO = Path("/root/turboGP")
QUERIES_DIR = REPO / "benchmarks/clickbench/queries"

# 43 ClickBench queries
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
    11: "SELECT CounterID, COUNT(*) AS c FROM hits GROUP BY CounterID ORDER BY c DESC LIMIT 10",
    12: "SELECT CounterID, SUM(WatchID) AS s FROM hits GROUP BY CounterID ORDER BY s DESC LIMIT 10",
    13: "SELECT RegionID, COUNT(*) AS c FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10",
    14: "SELECT OS, COUNT(*) AS c FROM hits GROUP BY OS ORDER BY c DESC LIMIT 10",
    15: "SELECT UserID, COUNT(*) AS c FROM hits GROUP BY UserID ORDER BY c DESC LIMIT 10",
    16: "SELECT CounterID, COUNT(*) AS c FROM hits WHERE EventDate = '2020-01-15' GROUP BY CounterID ORDER BY c DESC LIMIT 10",
    17: "SELECT CounterID, MIN(WatchID), MAX(WatchID) FROM hits GROUP BY CounterID",
    18: "SELECT CounterID, AVG(WatchID) FROM hits GROUP BY CounterID",
    19: "SELECT COUNT(*) FROM hits WHERE WatchID % 2 = 0",
    20: "SELECT SUM(WatchID) FROM hits WHERE WatchID > 50000000",
    21: "SELECT COUNT(*) FROM hits WHERE URL LIKE '%page_1%'",
    22: "SELECT COUNT(*) FROM hits WHERE URL LIKE '%page_10%'",
    23: "SELECT COUNT(*) FROM hits WHERE CounterID BETWEEN 100 AND 200",
    24: "SELECT COUNT(*) FROM hits WHERE UserID BETWEEN 1 AND 100000",
    25: "SELECT EventDate, COUNT(*) AS c FROM hits GROUP BY EventDate ORDER BY EventDate",
    26: "SELECT EventDate, SUM(WatchID) AS s FROM hits GROUP BY EventDate ORDER BY EventDate",
    27: "SELECT CounterID, UserID, COUNT(*) AS c FROM hits GROUP BY CounterID, UserID ORDER BY c DESC LIMIT 100",
    28: "SELECT CounterID, RegionID, COUNT(*) AS c FROM hits GROUP BY CounterID, RegionID",
    29: "SELECT COUNT(*) FROM hits WHERE CounterID = 500 AND UserID = 12345",
    30: "SELECT COUNT(*) FROM hits WHERE CounterID = 500 OR UserID = 12345",
    31: "SELECT COUNT(*) FROM hits WHERE NOT (CounterID = 500)",
    32: "SELECT COUNT(*) FROM hits WHERE CounterID != 500",
    33: "SELECT COUNT(*) FROM hits WHERE UserID IN (1, 2, 3, 4, 5)",
    34: "SELECT COUNT(*) FROM hits WHERE CounterID NOT IN (1, 2, 3, 4, 5)",
    35: "SELECT COUNT(*) FROM hits WHERE EventDate IN ('2020-01-01', '2020-01-02', '2020-01-03')",
    36: "SELECT CounterID, COUNT(*) AS c FROM hits GROUP BY CounterID HAVING COUNT(*) > 1000 ORDER BY c DESC",
    37: "SELECT CounterID, COUNT(*) AS c FROM hits GROUP BY CounterID ORDER BY CounterID",
    38: "SELECT COUNT(*) FROM (SELECT CounterID FROM hits GROUP BY CounterID) AS sub",
    39: "SELECT COUNT(*) FROM (SELECT DISTINCT UserID FROM hits) AS sub",
    40: "SELECT MIN(WatchID) FROM hits WHERE CounterID = 500",
    41: "SELECT MAX(WatchID) FROM hits WHERE CounterID = 500",
    42: "SELECT AVG(WatchID) FROM hits WHERE CounterID = 500",
    43: "SELECT SUM(WatchID), COUNT(*), AVG(WatchID), MIN(WatchID), MAX(WatchID) FROM hits",
}

def adapt_for_dialect(sql, dialect):
    """Adapt query for specific database dialect."""
    q = sql
    if dialect == "clickhouse":
        # ClickHouse: % needs modulo function, but supports % operator
        pass
    elif dialect == "exasol":
        # Exasol: same as standard SQL
        pass
    elif dialect == "turbogp":
        # turboGP: same as standard SQL, may need some adjustments
        pass
    return q

def main():
    dialects = ["standard", "turbogp", "clickhouse", "duckdb", "postgres", "exasol"]
    for dialect in dialects:
        d = QUERIES_DIR / dialect
        d.mkdir(parents=True, exist_ok=True)
        for qnum, sql in QUERIES.items():
            adapted = adapt_for_dialect(sql, dialect) if dialect != "standard" else sql
            (d / f"q{qnum:02d}.sql").write_text(adapted + ";")
        print(f"  {dialect}: {len(QUERIES)} queries")
    print(f"\nTotal: {len(dialects)} dialects × {len(QUERIES)} queries = {len(dialects) * len(QUERIES)} files")

if __name__ == "__main__":
    main()
