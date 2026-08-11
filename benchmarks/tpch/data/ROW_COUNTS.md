# TPC-H Data — Row Counts

Generated via `dbgen` from [electrum/tpch-dbgen](https://github.com/electrum/tpch-dbgen) (the de-facto standard TPC-H data generator).

| Table | SF=1 | SF=10 (expected) |
|---|---|---|
| customer | 150000 | 1500000 |
| lineitem | 6001215 | 59986052 |
| nation | 25 | 25 |
| orders | 1500000 | 15000000 |
| partsupp | 800000 | 8000000 |
| part | 200000 | 2000000 |
| region | 5 | 5 |
| supplier | 10000 | 100000 |

Expected TPC-H row counts:
- SF=1: lineitem = 6,001,215; orders = 1,500,000; customer = 150,000; part = 200,000; partsupp = 800,000; supplier = 10,000; nation = 25; region = 5
- SF=10: lineitem = 59,986,052; orders = 15,000,000; customer = 1,500,000; part = 2,000,000; partsupp = 8,000,000; supplier = 100,000; nation = 25; region = 5
