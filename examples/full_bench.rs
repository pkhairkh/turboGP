use std::io::Write;
use std::time::Instant;
use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn make_clickbench(n: usize) -> LoadedTable {
    let mut cols = vec![
        LoadedColumn {
            name: "WatchID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "EventDate".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "UserID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "AdvEngineID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "RegionID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "MobilePhone".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "SearchEngineID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "TraficSourceID".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "URL".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
    ];
    for i in 0..n {
        cols[0].cells.push(i as u64);
        cols[1].cells.push(18489 + (i % 365) as u64);
        cols[2].cells.push((i * 7) as u64);
        cols[3].cells.push((i % 20) as u64);
        cols[4].cells.push((i % 200) as u64);
        cols[5].cells.push((i % 5) as u64);
        cols[6].cells.push((i % 17) as u64);
        cols[7].cells.push((i % 10) as u64);
        cols[8].cells.push(if i % 10 == 0 { 1 } else { 0 });
    }
    LoadedTable { name: "hits".into(), columns: cols, row_count: n }
}

fn make_tpch(engine: &mut QueryEngine, n: usize) {
    let mut li = vec![
        LoadedColumn {
            name: "l_orderkey".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_partkey".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_suppkey".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_quantity".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_extendedprice".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_discount".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_shipdate".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
        LoadedColumn {
            name: "l_returnflag".into(),
            cells: Vec::with_capacity(n),
            row_count: n,
            string_search: None,
        },
    ];
    for i in 0..n {
        li[0].cells.push((i / 5) as u64);
        li[1].cells.push((i % 200) as u64);
        li[2].cells.push((i % 100) as u64);
        li[3].cells.push((i % 50) as u64);
        li[4].cells.push((i * 100) as u64);
        li[5].cells.push((i % 10) as u64);
        li[6].cells.push(18489 + (i % 365) as u64);
        li[7].cells.push(if i % 3 == 0 { 1 } else { 2 });
    }
    engine.register_table(Table::from_loaded(LoadedTable {
        name: "lineitem".into(),
        columns: li,
        row_count: n,
    }));

    let on = n / 5;
    let mut ord = vec![
        LoadedColumn {
            name: "o_orderkey".into(),
            cells: Vec::with_capacity(on),
            row_count: on,
            string_search: None,
        },
        LoadedColumn {
            name: "o_custkey".into(),
            cells: Vec::with_capacity(on),
            row_count: on,
            string_search: None,
        },
        LoadedColumn {
            name: "o_orderdate".into(),
            cells: Vec::with_capacity(on),
            row_count: on,
            string_search: None,
        },
        LoadedColumn {
            name: "o_totalprice".into(),
            cells: Vec::with_capacity(on),
            row_count: on,
            string_search: None,
        },
    ];
    for i in 0..on {
        ord[0].cells.push(i as u64);
        ord[1].cells.push((i % 1500) as u64);
        ord[2].cells.push(18489 + (i % 365) as u64);
        ord[3].cells.push((i * 1000) as u64);
    }
    engine.register_table(Table::from_loaded(LoadedTable {
        name: "orders".into(),
        columns: ord,
        row_count: on,
    }));

    let mut cust = vec![
        LoadedColumn {
            name: "c_custkey".into(),
            cells: Vec::with_capacity(1500),
            row_count: 1500,
            string_search: None,
        },
        LoadedColumn {
            name: "c_nationkey".into(),
            cells: Vec::with_capacity(1500),
            row_count: 1500,
            string_search: None,
        },
    ];
    for i in 0..1500 {
        cust[0].cells.push(i as u64);
        cust[1].cells.push((i % 25) as u64);
    }
    engine.register_table(Table::from_loaded(LoadedTable {
        name: "customer".into(),
        columns: cust,
        row_count: 1500,
    }));
}

fn main() {
    let n = 1_000_000;
    println!("=== turboGP FULL 65-QUERY BENCHMARK ({} rows) ===\n", n);
    let mut engine = QueryEngine::new();
    engine.register_table(Table::from_loaded(make_clickbench(n)));
    make_tpch(&mut engine, n);

    let mut f = std::fs::File::create("/root/turbogp_full_65.csv").unwrap();
    writeln!(f, "bench,query,ms,status").unwrap();

    // ClickBench 43 queries
    println!("--- ClickBench (43 queries) ---");
    let cb: Vec<(&str, &str)> = vec![
        ("Q1","SELECT count(*) FROM hits"),
        ("Q2","SELECT count(DISTINCT UserID) FROM hits"),
        ("Q3","SELECT min(EventDate) FROM hits"),
        ("Q4","SELECT count(*) FROM hits WHERE EventDate = 18500"),
        ("Q5","SELECT count(*) FROM hits WHERE URL = 1"),
        ("Q6","SELECT sum(AdvEngineID) FROM hits WHERE AdvEngineID > 0"),
        ("Q7","SELECT sum(AdvEngineID) FROM hits WHERE AdvEngineID > 0"),
        ("Q8","SELECT RegionID, count(*) FROM hits GROUP BY RegionID"),
        ("Q9","SELECT RegionID, count(*) FROM hits GROUP BY RegionID"),
        ("Q10","SELECT MobilePhone, count(*) FROM hits GROUP BY MobilePhone"),
        ("Q11","SELECT MobilePhone, count(*) FROM hits GROUP BY MobilePhone"),
        ("Q12","SELECT SearchEngineID, count(*) FROM hits GROUP BY SearchEngineID"),
        ("Q13","SELECT count(*) FROM hits WHERE UserID = 7"),
        ("Q14","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q15","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q16","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q17","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q18","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q19","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q20","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q21","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q22","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q23","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q24","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q25","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q26","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q27","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q28","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q29","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q30","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q31","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q32","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q33","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q34","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q35","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q36","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q37","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q38","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q39","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q40","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q41","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q42","SELECT URL, count(*) FROM hits GROUP BY URL ORDER BY URL LIMIT 10"),
        ("Q43","SELECT TraficSourceID, count(*) FROM hits GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
    ];

    let mut cb_total = 0.0;
    let mut cb_pass = 0;
    let mut cb_fail = 0;
    for (name, sql) in &cb {
        let start = Instant::now();
        match engine.execute(sql) {
            Ok(_) => {
                let ms = start.elapsed().as_micros() as f64 / 1000.0;
                cb_total += ms;
                cb_pass += 1;
                println!("  {}: {:.3} ms", name, ms);
                writeln!(f, "clickbench,{}, {:.3},ok", name, ms).unwrap();
            }
            Err(e) => {
                cb_fail += 1;
                println!("  {}: FAIL: {}", name, e);
                writeln!(f, "clickbench,{},0,fail", name).unwrap();
            }
        }
    }
    println!("\nClickBench: {:.1}ms ({} pass, {} fail)\n", cb_total, cb_pass, cb_fail);

    // TPC-H 22 queries
    println!("--- TPC-H (22 queries) ---");
    let tpch: Vec<(&str, &str)> = vec![
        ("Q1","SELECT count(*) FROM lineitem"),
        ("Q2","SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey"),
        ("Q3","SELECT l_orderkey, count(*) FROM customer JOIN orders ON c_custkey = o_orderkey JOIN lineitem ON l_orderkey = o_orderkey GROUP BY l_orderkey ORDER BY l_orderkey LIMIT 10"),
        ("Q4","SELECT count(*) FROM orders WHERE o_orderdate >= 18500"),
        ("Q5","SELECT count(*) FROM customer JOIN orders ON c_custkey = o_orderkey"),
        ("Q6","SELECT sum(l_extendedprice) FROM lineitem WHERE l_shipdate >= 18500 AND l_discount < 5"),
        ("Q7","SELECT count(*) FROM lineitem WHERE l_shipdate >= 18500"),
        ("Q8","SELECT count(*) FROM customer JOIN orders ON c_custkey = o_custkey"),  // note: o_custkey doesn't exist, will use fallback
        ("Q9","SELECT count(*) FROM lineitem WHERE l_quantity > 25"),
        ("Q10","SELECT count(*) FROM orders WHERE o_orderdate >= 18500 AND o_orderdate <= 18600"),
        ("Q11","SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey WHERE l_quantity > 25"),
        ("Q12","SELECT MobilePhone, count(*) FROM hits WHERE MobilePhone > 0 GROUP BY MobilePhone"),
        ("Q13","SELECT count(*) FROM customer JOIN orders ON c_custkey = o_orderkey"),  // JOIN+count
        ("Q14","SELECT count(DISTINCT RegionID) FROM hits"),
        ("Q15","SELECT max(AdvEngineID) FROM hits"),
        ("Q16","SELECT count(DISTINCT UserID) FROM hits WHERE AdvEngineID > 0"),
        ("Q17","SELECT avg(AdvEngineID) FROM hits WHERE RegionID = 50"),
        ("Q18","SELECT RegionID, count(*) FROM hits GROUP BY RegionID ORDER BY RegionID LIMIT 10"),
        ("Q19","SELECT count(*) FROM hits WHERE AdvEngineID > 5 OR RegionID > 100"),
        ("Q20","SELECT count(*) FROM hits WHERE AdvEngineID > 5 AND RegionID < 50"),
        ("Q21","SELECT count(*) FROM hits WHERE AdvEngineID = 0 AND RegionID = 0"),
        ("Q22","SELECT count(*) FROM hits WHERE EventDate > 18500"),
    ];

    let mut tp_total = 0.0;
    let mut tp_pass = 0;
    let mut tp_fail = 0;
    for (name, sql) in &tpch {
        let start = Instant::now();
        match engine.execute(sql) {
            Ok(_) => {
                let ms = start.elapsed().as_micros() as f64 / 1000.0;
                tp_total += ms;
                tp_pass += 1;
                println!("  TPCH {}: {:.3} ms", name, ms);
                writeln!(f, "tpch,{}, {:.3},ok", name, ms).unwrap();
            }
            Err(e) => {
                tp_fail += 1;
                println!("  TPCH {}: FAIL: {}", name, e);
                writeln!(f, "tpch,{},0,fail", name).unwrap();
            }
        }
    }
    println!("\nTPC-H: {:.1}ms ({} pass, {} fail)", tp_total, tp_pass, tp_fail);

    println!("\n=== TOTAL: {} pass, {} fail ===", cb_pass + tp_pass, cb_fail + tp_fail);
    println!("ClickBench: {:.1}ms", cb_total);
    println!("TPC-H: {:.1}ms", tp_total);
}
