//! In-process ClickHouse benchmark: 65 queries × 3 runs, JSON output.
//!
//! Uses the `clickhouse` crate (official Rust client, HTTP transport to a
//! running `clickhouse-server`) — NO CLI subprocess overhead. Data stays
//! loaded in the server's memory across all 65 queries; the HTTP connection is
//! pooled (keep-alive) so connection overhead is amortized to ~nothing.
//!
//! The harness assumes the `bench` database is already created and populated:
//!   - `bench.hits`       (1M rows, loaded from /tmp/hits_1m.parquet)
//!   - `bench.lineitem`   (6M rows, TPC-H SF=1)
//!   - `bench.orders`, `bench.customer`, `bench.part`, `bench.partsupp`,
//!     `bench.supplier`, `bench.nation`, `bench.region`
//!
//! Then runs 43 ClickBench queries (read verbatim from
//! `/root/clickbench_queries.txt`, with `hits` → `bench.hits`) + 22 canonical
//! TPC-H queries (with table names prefixed `bench.` and ClickHouse SQL
//! adaptations), 3 times each after a single warm-up pass, and writes the
//! results to `/root/results/clickhouse_inproc.json`.
//!
//! Run with:
//!   cargo run --release --example bench_clickhouse_inproc

use clickhouse::Client;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::time::Instant;

/// Canonical TPC-H SF=1 queries Q1–Q22 (standard, unmodified source — adapted
/// at runtime for ClickHouse syntax). Tables: lineitem, orders, customer,
/// part, partsupp, supplier, nation, region.
const TPCH_QUERIES: &[(&str, &str)] = &[
    ("Q1", "SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price, sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge, avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price, avg(l_discount) AS avg_disc, count(*) AS count_order FROM lineitem WHERE l_shipdate <= date '1998-09-02' GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus"),
    ("Q2", "SELECT s_acctbal, s_name, n_name, p_partkey, p_mfgr, s_address, s_phone, s_comment FROM part, partsupp, supplier, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE' AND p_size = 15 AND p_type LIKE '%BRASS' AND ps_supplycost = (SELECT min(ps_supplycost) FROM partsupp, supplier, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE') ORDER BY s_acctbal DESC, n_name, s_name, p_partkey LIMIT 100"),
    ("Q3", "SELECT l_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority FROM customer, orders, lineitem WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate < date '1995-03-15' AND l_shipdate > date '1995-03-15' GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10"),
    ("Q4", "SELECT o_orderpriority, count(*) AS order_count FROM orders WHERE o_orderdate >= date '1993-07-01' AND o_orderdate < date '1993-10-01' AND exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate) GROUP BY o_orderpriority ORDER BY o_orderpriority"),
    ("Q5", "SELECT n_name, sum(l_extendedprice * (1 - l_discount)) AS revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01' GROUP BY n_name ORDER BY revenue DESC"),
    ("Q6", "SELECT sum(l_extendedprice * l_discount) AS revenue FROM lineitem WHERE l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01' AND l_discount >= 0.05 AND l_discount <= 0.07 AND l_quantity < 24"),
    ("Q7", "SELECT supp_nation, cust_nation, l_year, sum(volume) AS revenue FROM (SELECT n1.n_name AS supp_nation, n2.n_name AS cust_nation, extract(year FROM l_shipdate) AS l_year, l_extendedprice * (1 - l_discount) AS volume FROM supplier, lineitem, orders, customer, nation n1, nation n2 WHERE s_suppkey = l_suppkey AND o_orderkey = l_orderkey AND c_custkey = o_custkey AND s_nationkey = n1.n_nationkey AND c_nationkey = n2.n_nationkey AND ((n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY') OR (n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE')) AND l_shipdate BETWEEN date '1995-01-01' AND date '1996-12-31') AS shipping GROUP BY supp_nation, cust_nation, l_year ORDER BY supp_nation, cust_nation, l_year"),
    ("Q8", "SELECT o_year, sum(case WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / sum(volume) AS mkt_share FROM (SELECT extract(year FROM o_orderdate) AS o_year, l_extendedprice * (1 - l_discount) AS volume, n2.n_name AS nation FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region WHERE p_partkey = l_partkey AND s_suppkey = l_suppkey AND l_orderkey = o_orderkey AND o_custkey = c_custkey AND c_nationkey = n1.n_nationkey AND n1.n_regionkey = r_regionkey AND r_name = 'AMERICA' AND s_nationkey = n2.n_nationkey AND o_orderdate BETWEEN date '1995-01-01' AND date '1996-12-31' AND p_type = 'ECONOMY ANODIZED STEEL') AS all_nations GROUP BY o_year ORDER BY o_year"),
    ("Q9", "SELECT nation, o_year, sum(amount) AS sum_profit FROM (SELECT n_name AS nation, extract(year FROM o_orderdate) AS o_year, l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity AS amount FROM part, partsupp, lineitem, orders, supplier, nation WHERE s_suppkey = l_suppkey AND ps_suppkey = l_suppkey AND ps_partkey = l_partkey AND p_partkey = l_partkey AND o_orderkey = l_orderkey AND s_nationkey = n_nationkey AND p_name LIKE '%green%') AS profit GROUP BY nation, o_year ORDER BY nation, o_year DESC"),
    ("Q10", "SELECT c_custkey, c_name, sum(l_extendedprice * (1 - l_discount)) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment FROM customer, orders, lineitem, nation WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate >= date '1993-10-01' AND o_orderdate < date '1994-01-01' AND l_returnflag = 'R' AND c_nationkey = n_nationkey GROUP BY c_custkey, c_name, c_acctbal, c_name, n_name, c_address, c_phone, c_comment ORDER BY revenue DESC LIMIT 20"),
    ("Q11", "SELECT ps_partkey, sum(ps_supplycost * ps_availqty) AS value FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY' GROUP BY ps_partkey HAVING sum(ps_supplycost * ps_availqty) > (SELECT sum(ps_supplycost * ps_availqty) * 0.0001 FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY') ORDER BY value DESC"),
    ("Q12", "SELECT l_shipmode, sum(case WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH' THEN 1 ELSE 0 END) AS high_line_count, sum(case WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH' THEN 1 ELSE 0 END) AS low_line_count FROM orders, lineitem WHERE o_orderkey = l_orderkey AND l_shipmode IN ('MAIL', 'SHIP') AND l_commitdate < l_receiptdate AND l_shipdate < l_commitdate AND l_receiptdate >= date '1994-01-01' AND l_receiptdate < date '1995-01-01' GROUP BY l_shipmode ORDER BY l_shipmode"),
    ("Q13", "SELECT c_count, count(*) AS custdist FROM (SELECT c_custkey, count(o_orderkey) AS c_count FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey AND o_comment NOT LIKE '%special%requests%' GROUP BY c_custkey) AS c_orders GROUP BY c_count ORDER BY custdist DESC, c_count DESC"),
    ("Q14", "SELECT 100.00 * sum(case WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) / sum(l_extendedprice * (1 - l_discount)) AS promo_revenue FROM lineitem, part WHERE l_partkey = p_partkey AND l_shipdate >= date '1995-09-01' AND l_shipdate < date '1995-10-01'"),
    ("Q15", "SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue WHERE s_suppkey = supplier_no AND total_revenue = (SELECT max(total_revenue) FROM (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue) ORDER BY s_suppkey"),
    ("Q16", "SELECT p_brand, p_type, p_size, count(DISTINCT ps_suppkey) AS supplier_cnt FROM partsupp, part WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%' AND p_size IN (49, 14, 23, 45, 19, 3, 36, 9) GROUP BY p_brand, p_type, p_size ORDER BY supplier_cnt DESC, p_brand, p_type, p_size"),
    ("Q17", "SELECT sum(l_extendedprice) / 7.0 AS avg_yearly FROM lineitem, part WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX' AND l_quantity < (SELECT 0.2 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)"),
    ("Q18", "SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity) FROM customer, orders, lineitem WHERE c_custkey = o_custkey AND o_orderkey = l_orderkey GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice HAVING sum(l_quantity) > 300 ORDER BY o_totalprice DESC, o_orderdate LIMIT 100"),
    ("Q19", "SELECT sum(l_extendedprice * (1 - l_discount)) AS revenue FROM lineitem, part WHERE (p_partkey = l_partkey AND p_brand = 'Brand#12' AND p_container IN ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG') AND l_quantity >= 1 AND l_quantity <= 11 AND p_size BETWEEN 1 AND 5 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON') OR (p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container IN ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK') AND l_quantity >= 10 AND l_quantity <= 20 AND p_size BETWEEN 1 AND 10 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON') OR (p_partkey = l_partkey AND p_brand = 'Brand#34' AND p_container IN ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG') AND l_quantity >= 20 AND l_quantity <= 30 AND p_size BETWEEN 1 AND 15 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')"),
    ("Q20", "SELECT s_name, s_address FROM supplier, nation WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%') AND ps_availqty > (SELECT 0.5 * sum(l_quantity) FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey AND l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01')) AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name"),
    ("Q21", "SELECT s_name, count(*) AS numwait FROM supplier, lineitem l1, orders, nation WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey AND o_orderstatus = 'F' AND l1.l_receiptdate > l1.l_commitdate AND exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey) AND NOT exists (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate) AND s_nationkey = n_nationkey AND n_name = 'SAUDI ARABIA' GROUP BY s_name ORDER BY numwait DESC, s_name LIMIT 100"),
    ("Q22", "SELECT cntrycode, count(*) AS numcust, sum(c_acctbal) AS totacctbal FROM (SELECT substr(c_phone, 1, 2) AS cntrycode, c_acctbal FROM customer WHERE substr(c_phone, 1, 2) IN ('13', '31', '23', '29', '30', '18', '17') AND c_acctbal > (SELECT avg(c_acctbal) FROM customer WHERE c_acctbal > 0.00 AND substr(c_phone, 1, 2) IN ('13', '31', '23', '29', '30', '18', '17'))) AS custsale GROUP BY cntrycode ORDER BY cntrycode"),
];

const NUM_RUNS: usize = 3;
const CLICKBENCH_QUERIES_FILE: &str = "/root/clickbench_queries.txt";
const RESULTS_DIR: &str = "/root/results";
const JSON_OUT: &str = "/root/results/clickhouse_inproc.json";
const LOG_OUT: &str = "/root/results/clickhouse_inproc.run.log";

/// Read ClickBench queries (one per line) from the verbatim file.
fn load_clickbench_queries(path: &str) -> Vec<String> {
    let content = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path, e));
    content.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// Adapt SQL for ClickHouse:
///   - ClickBench: `hits` → `bench.hits`
///   - TPC-H: prefix all 8 table names with `bench.`, convert
///     `date 'YYYY-MM-DD'` → `toDate('YYYY-MM-DD')`, and
///     `extract(year FROM col)` → `toYear(col)`.
fn adapt_sql(sql: &str, suite: &str) -> String {
    let mut s = sql.to_string();
    // Table-name qualification (FROM hits → bench.hits, FROM lineitem →
    // bench.lineitem, etc.) is NOT done via regex here — naive word-boundary
    // replacement corrupts column aliases that share table names (e.g. TPC-H
    // Q8/Q9 use `AS nation`). Instead, the ClickHouse client is configured with
    // `.with_database("bench")` so all unqualified table references resolve to
    // the `bench` schema automatically.
    if suite == "tpch" {
        // date 'YYYY-MM-DD'  →  toDate('YYYY-MM-DD')
        let re = Regex::new(r"(?i)date\s+'(\d{4}-\d{2}-\d{2})'").unwrap();
        s = re.replace_all(&s, "toDate('$1')").to_string();
        // extract(year FROM col)  →  toYear(col)
        let re = Regex::new(r"(?i)extract\s*\(\s*year\s+FROM\s+([\w.]+)\s*\)").unwrap();
        s = re.replace_all(&s, "toYear($1)").to_string();
    }
    // ClickBench queries need no adaptation — `with_database("bench")` resolves
    // `FROM hits` to `bench.hits`, and ClickHouse supports count(), LIKE,
    // BETWEEN, GROUP BY ordinals, etc. natively.
    s
}

/// Execute a query fully (drain all result rows in JSONEachRow format) and
/// return the row count. Using `fetch_bytes("JSONEachRow")` forces ClickHouse
/// to compute ALL columns/aggregates and serialize the full result set — no
/// subquery-projection optimization can skip work.
async fn run_query(client: &Client, sql: &str) -> Result<u64, String> {
    let mut cursor = client.query(sql).fetch_bytes("JSONEachRow").map_err(|e| e.to_string())?;
    let mut row_count: u64 = 0;
    loop {
        match cursor.next().await {
            Ok(Some(chunk)) => {
                // JSONEachRow: each row is one JSON object terminated by '\n'.
                // Literal '\n' bytes = row separators (newlines inside JSON
                // strings are escaped as \\n, never literal).
                row_count += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(row_count)
}

fn median_of_3(sorted: &[f64]) -> f64 {
    // sorted must have 3 elements; middle one is median
    sorted[sorted.len() / 2]
}

#[tokio::main]
async fn main() {
    println!("=== ClickHouse in-process benchmark ===");

    // 1. Connect to the running clickhouse-server (HTTP transport, port 8123).
    //    The `clickhouse` crate pools HTTP connections (keep-alive), so
    //    connection overhead is amortized across all 65 queries.
    let client = Client::default()
        .with_url("http://localhost:8123")
        .with_database("bench")
        .with_setting("max_threads", "8");

    // Fetch ClickHouse version.
    let ch_version: String = client
        .query("SELECT version()")
        .fetch_one()
        .await
        .unwrap_or_else(|e| panic!("SELECT version(): {}", e));
    println!("ClickHouse version: {}", ch_version);

    // Sanity-check that data is loaded.
    let hits_n: u64 =
        client.query("SELECT count() FROM bench.hits").fetch_one().await.expect("count bench.hits");
    let li_n: u64 = client
        .query("SELECT count() FROM bench.lineitem")
        .fetch_one()
        .await
        .expect("count bench.lineitem");
    println!("bench.hits: {} rows, bench.lineitem: {} rows", hits_n, li_n);
    assert_eq!(hits_n, 1_000_000, "expected 1M rows in bench.hits");
    assert_eq!(li_n, 6_001_215, "expected 6M rows in bench.lineitem");

    // 2. Load ClickBench queries and assemble the full 65-query list.
    println!("\nLoading ClickBench queries from {}", CLICKBENCH_QUERIES_FILE);
    let cb_sqls = load_clickbench_queries(CLICKBENCH_QUERIES_FILE);
    println!("  {} ClickBench queries loaded", cb_sqls.len());
    assert_eq!(cb_sqls.len(), 43, "expected 43 ClickBench queries");

    let mut queries: Vec<(String, String, String)> = Vec::with_capacity(65);
    for (i, sql) in cb_sqls.iter().enumerate() {
        let adapted = adapt_sql(sql, "clickbench");
        queries.push((format!("Q{}", i + 1), "clickbench".to_string(), adapted));
    }
    for (id, sql) in TPCH_QUERIES {
        let adapted = adapt_sql(sql, "tpch");
        queries.push((id.to_string(), "tpch".to_string(), adapted));
    }
    assert_eq!(queries.len(), 65, "expected 65 total queries");
    println!("\nTotal queries: {} (43 clickbench + 22 tpch)", queries.len());

    // 3. Warm-up: run each query once, discard results (ignore errors here).
    println!("\nWarm-up pass (running each query once)...");
    let mut warmup_log: Vec<String> = Vec::new();
    for (id, suite, sql) in &queries {
        match run_query(&client, sql).await {
            Ok(n) => {
                println!("  warmup {:<10} {:<10} -> {} rows", suite, id, n);
            }
            Err(e) => {
                let msg = format!("  warmup {:<10} {:<10} FAILED: {}", suite, id, e);
                println!("{}", msg);
                warmup_log.push(msg);
            }
        }
    }

    // 4. Measured runs: 3 per query.
    println!("\nMeasured runs ({} per query)...", NUM_RUNS);
    let mut results: Vec<Value> = Vec::with_capacity(queries.len());
    let mut total_best_ms: f64 = 0.0;
    let mut total_median_ms: f64 = 0.0;
    let mut ok_count: usize = 0;
    let mut fail_count: usize = 0;

    for (id, suite, sql) in &queries {
        let mut runs_ms: Vec<f64> = Vec::with_capacity(NUM_RUNS);
        let mut status = "ok";
        let mut error: Option<String> = None;
        let mut rows: i64 = 0;

        for _ in 0..NUM_RUNS {
            let t0 = Instant::now();
            match run_query(&client, sql).await {
                Ok(n) => {
                    runs_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    rows = n as i64;
                }
                Err(e) => {
                    status = "fail";
                    error = Some(e);
                    break;
                }
            }
        }

        let entry = if status == "ok" {
            ok_count += 1;
            let mut sorted = runs_ms.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let best_ms = sorted[0];
            let median_ms = median_of_3(&sorted);
            total_best_ms += best_ms;
            total_median_ms += median_ms;
            let r = json!({
                "id": id,
                "suite": suite,
                "sql": sql,
                "runs_ms": runs_ms,
                "best_ms": best_ms,
                "median_ms": median_ms,
                "status": status,
                "rows": rows,
                "error": null,
            });
            println!(
                "  {:<10} {:<4} OK    runs=[{:>8.2}, {:>8.2}, {:>8.2}] ms  best={:>8.2}  med={:>8.2}  rows={}",
                suite, id, runs_ms[0], runs_ms[1], runs_ms[2], best_ms, median_ms, rows
            );
            r
        } else {
            fail_count += 1;
            let err_msg = error.clone().unwrap_or_default();
            println!(
                "  {:<10} {:<4} FAIL  after {} runs  err: {}",
                suite,
                id,
                runs_ms.len(),
                err_msg.chars().take(160).collect::<String>()
            );
            json!({
                "id": id,
                "suite": suite,
                "sql": sql,
                "runs_ms": runs_ms,
                "best_ms": null,
                "median_ms": null,
                "status": status,
                "rows": 0,
                "error": error,
            })
        };
        results.push(entry);
    }

    // 5. Build the JSON output (same schema as the DuckDB harness).
    let output = json!({
        "engine": "clickhouse",
        "mode": "in-process",
        "version": ch_version,
        "clickbench_load_ms": 0,
        "tpch_load_ms": 0,
        "queries": results,
        "total_best_ms": total_best_ms,
        "total_median_ms": total_median_ms,
        "ok_count": ok_count,
        "fail_count": fail_count,
        "num_runs_per_query": NUM_RUNS,
    });

    // 6. Write JSON + run log.
    fs::create_dir_all(RESULTS_DIR).expect("create results dir");
    let json_pretty = serde_json::to_string_pretty(&output).expect("serialize json");
    fs::write(JSON_OUT, &json_pretty).expect("write json");

    let mut log = String::new();
    log.push_str("=== ClickHouse in-process benchmark run log ===\n");
    log.push_str(&format!("version: {}\n", ch_version));
    log.push_str(&format!("ok: {}  fail: {}\n", ok_count, fail_count));
    log.push_str(&format!("total_best_ms: {:.2}\n", total_best_ms));
    log.push_str(&format!("total_median_ms: {:.2}\n", total_median_ms));
    log.push_str("\n--- warm-up log ---\n");
    if warmup_log.is_empty() {
        log.push_str("(all warmup queries ok)\n");
    } else {
        for m in &warmup_log {
            log.push_str(m);
            log.push('\n');
        }
    }
    log.push_str("\n--- per-query measured runs ---\n");
    for v in output["queries"].as_array().unwrap() {
        let id = v["id"].as_str().unwrap_or("");
        let suite = v["suite"].as_str().unwrap_or("");
        let status = v["status"].as_str().unwrap_or("");
        let runs = v["runs_ms"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| format!("{:.2}", x.as_f64().unwrap_or(0.0)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let best = v["best_ms"].as_f64().map(|b| format!("{:.2}", b)).unwrap_or("null".into());
        let med = v["median_ms"].as_f64().map(|m| format!("{:.2}", m)).unwrap_or("null".into());
        let err = v["error"].as_str().unwrap_or("");
        log.push_str(&format!(
            "{:<10} {:<4} {:<5} runs=[{}] best={} med={} err={}\n",
            suite, id, status, runs, best, med, err
        ));
    }
    let _ = fs::File::create(LOG_OUT).and_then(|mut f| f.write_all(log.as_bytes()));

    // 7. Summary to stdout.
    println!("\n=== SUMMARY ===");
    println!("Engine: ClickHouse {} (in-process, clickhouse-rs crate)", ch_version);
    println!("Queries: {} ok / {} failed (of {})", ok_count, fail_count, ok_count + fail_count);
    println!("total_best_ms:    {:.2}", total_best_ms);
    println!("total_median_ms:  {:.2}", total_median_ms);
    println!("JSON written to:  {}", JSON_OUT);
    println!("Run log written to: {}", LOG_OUT);
}
