#!/usr/bin/env rust
//! Native turboGP benchmark binary.
//!
//! Eliminates psql/pgwire/subprocess overhead by calling engine.execute()
//! directly. Measures pure engine-internal timing via QueryResult.elapsed_us.
//!
//! Usage:
//!   turbogp_bench tpch --sf 1 --csv-dir /srv/turbogp_csv/sf1 --repo-dir /root/turboGP
//!   turbogp_bench tpch --sf 10 --csv-dir /srv/turbogp_csv/sf10 --repo-dir /root/turboGP
//!   turbogp_bench clickbench --csv-dir /srv/turbogp_csv/clickbench --repo-dir /root/turboGP
//!
//! Data is loaded ONCE into memory. Queries run cold (cache cleared) then
//! 3 hot (cache enabled). Results saved to CSV.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use turbogp::engine::QueryEngine;

#[derive(Parser)]
#[command(name = "turbogp_bench")]
#[command(about = "Native turboGP benchmark — no psql, no pgwire, pure engine timing")]
struct Cli {
    #[command(subcommand)]
    command: BenchCommand,

    /// Repository root directory (contains benchmarks/, src/, etc.)
    #[arg(long, default_value = ".")]
    repo_dir: String,
}

#[derive(Subcommand)]
enum BenchCommand {
    /// TPC-H benchmark (22 queries)
    Tpch {
        /// Scale factor (1, 10)
        #[arg(long, default_value = "1")]
        sf: u32,
        /// Directory containing pre-converted CSV files
        #[arg(long)]
        csv_dir: String,
        /// Output CSV file path
        #[arg(long, default_value = "benchmarks/tpch/results/native_bench.csv")]
        output: String,
        /// Number of hot iterations (after 1 cold warmup)
        #[arg(long, default_value = "3")]
        hot_iters: u32,
        /// Start query number (1-22, default 1)
        #[arg(long, default_value = "1")]
        start_query: u32,
        /// End query number (1-22, default 22)
        #[arg(long, default_value = "22")]
        end_query: u32,
    },
    /// ClickBench benchmark (43 queries)
    Clickbench {
        /// Directory containing pre-converted CSV files
        #[arg(long)]
        csv_dir: String,
        /// Output CSV file path
        #[arg(long, default_value = "benchmarks/clickbench/results/native_bench.csv")]
        output: String,
        /// Number of hot iterations
        #[arg(long, default_value = "3")]
        hot_iters: u32,
    },
}

fn load_schema(engine: &mut QueryEngine, schema_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let schema_sql = std::fs::read_to_string(schema_path)?;
    let mut count = 0;
    for stmt in schema_sql.split(';').filter(|s| !s.trim().is_empty()) {
        engine.execute(stmt).map_err(|e| format!("schema stmt failed: {e}"))?;
        count += 1;
    }
    eprintln!("  schema: {count} statements executed");
    Ok(())
}

fn load_csv_data(engine: &mut QueryEngine, csv_dir: &Path, tables: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    for tbl in tables {
        let csv_path = csv_dir.join(format!("{tbl}.csv"));
        if !csv_path.exists() {
            return Err(format!("CSV not found: {csv_path:?}").into());
        }
        let t0 = Instant::now();
        let n = engine
            .load_csv(&csv_path.to_string_lossy(), tbl, true)
            .map_err(|e| format!("load_csv {tbl} failed: {e}"))?;
        let ms = t0.elapsed().as_millis();
        eprintln!("  {tbl}: {n} rows ({ms}ms)");
    }
    Ok(())
}

fn read_query(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let sql = std::fs::read_to_string(path)?;
    // Strip comment-only lines (turboGP lexer treats -- comment lines oddly)
    let lines: Vec<&str> = sql
        .split('\n')
        .filter(|l| !l.trim().starts_with("--"))
        .collect();
    Ok(lines.join("\n"))
}

fn run_query_cold(engine: &mut QueryEngine, sql: &str) -> (u64, usize, String) {
    // Invalidate cache so the next execute() is a true cold run
    engine.result_cache.write().invalidate_all();
    match engine.execute(sql) {
        Ok(result) => (result.elapsed_us, result.row_count, "OK".to_string()),
        Err(e) => (0, 0, format!("ERR: {e}")),
    }
}

fn run_query_hot(engine: &mut QueryEngine, sql: &str) -> (u64, usize, String) {
    // Cache is NOT invalidated — this should be a cache hit
    match engine.execute(sql) {
        Ok(result) => (result.elapsed_us, result.row_count, "OK".to_string()),
        Err(e) => (0, 0, format!("ERR: {e}")),
    }
}

fn run_benchmark(
    engine: &mut QueryEngine,
    queries_dir: &Path,
    query_files: &[(String, String)], // (qid, filename)
    output_path: &str,
    hot_iters: u32,
    bench_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    // Ensure output directory exists
    if let Some(parent) = Path::new(output_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::File::create(output_path)?;
    // CSV header
    let hot_headers: Vec<String> = (0..hot_iters)
        .map(|i| format!("hot{}_us,hot{}_rows,hot{}_status", i + 1, i + 1, i + 1))
        .collect();
    writeln!(
        file,
        "query_id,cold_us,cold_rows,cold_status,{}",
        hot_headers.join(",")
    )?;

    eprintln!("\n=== {} Benchmark (cold + {} hot, engine-internal timing) ===\n", bench_name, hot_iters);
    eprintln!(
        "{:<8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Query", "Cold(us)", "Cold(ms)", "Hot1(us)", "Hot2(us)", "Hot3(us)", "Status"
    );
    eprintln!("{}", "-".repeat(70));

    let mut all_results: Vec<(String, u64, u64, bool)> = Vec::new(); // (qid, cold_us, hot_median_us, ok)

    for (qid, filename) in query_files {
        let qpath = queries_dir.join(filename);
        if !qpath.exists() {
            eprintln!("{qid:<8} FILE NOT FOUND: {qpath:?}");
            continue;
        }
        let sql = read_query(&qpath)?;

        // Cold run
        let (cold_us, cold_rows, cold_status) = run_query_cold(engine, &sql);

        // Hot runs
        let mut hot_results: Vec<(u64, usize, String)> = Vec::new();
        for _ in 0..hot_iters {
            hot_results.push(run_query_hot(engine, &sql));
        }

        let all_ok = cold_status == "OK" && hot_results.iter().all(|r| r.2 == "OK");
        let hot_median_us = {
            let mut times: Vec<u64> = hot_results.iter().map(|r| r.0).collect();
            times.sort();
            times[times.len() / 2]
        };

        // Write CSV row
        let hot_cols: Vec<String> = hot_results
            .iter()
            .map(|r| format!("{},{},{}", r.0, r.1, r.2))
            .collect();
        writeln!(
            file,
            "{},{},{},{},{}",
            qid, cold_us, cold_rows, cold_status,
            hot_cols.join(",")
        )?;

        eprintln!(
            "{qid:<8} {:>10} {:>10.1} {:>10} {:>10} {:>10} {:>10}",
            cold_us,
            cold_us as f64 / 1000.0,
            hot_results.get(0).map(|r| r.0).unwrap_or(0),
            hot_results.get(1).map(|r| r.0).unwrap_or(0),
            hot_results.get(2).map(|r| r.0).unwrap_or(0),
            if all_ok { "OK" } else { "FAIL" }
        );

        all_results.push((qid.clone(), cold_us, hot_median_us, all_ok));
    }

    file.flush()?;

    // Summary
    let ok_cold: Vec<u64> = all_results.iter().filter(|r| r.3).map(|r| r.1).collect();
    let ok_hot: Vec<u64> = all_results.iter().filter(|r| r.3).map(|r| r.2).collect();

    eprintln!("\n--- Summary ---");
    if !ok_cold.is_empty() {
        let cold_gm = geometric_mean_us(&ok_cold);
        let hot_gm = geometric_mean_us(&ok_hot);
        eprintln!("Cold geomean: {:.3}ms ({:.0}us)", cold_gm as f64 / 1000.0, cold_gm);
        eprintln!("Hot geomean:  {:.3}ms ({:.0}us)", hot_gm as f64 / 1000.0, hot_gm);
        if hot_gm > 0 {
            eprintln!("Cache speedup: {:.1}x", cold_gm as f64 / hot_gm as f64);
        }
    }
    let ok_count = all_results.iter().filter(|r| r.3).count();
    let under_5ms = all_results.iter().filter(|r| r.3 && r.2 < 5000).count();
    eprintln!("Queries OK: {}/{}", ok_count, all_results.len());
    eprintln!("Hot runs under 5ms: {}/{}", under_5ms, ok_count);
    eprintln!("Results saved to: {output_path}");

    Ok(())
}

fn geometric_mean_us(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let sum_log: f64 = values.iter().map(|v| (*v as f64).ln()).sum();
    (sum_log / values.len() as f64).exp() as u64
}

fn tpch_query_files(start: u32, end: u32) -> Vec<(String, String)> {
    (start..=end)
        .filter(|n| *n >= 1 && *n <= 22)
        .map(|n| (format!("q{n:02}"), format!("q{n:02}.sql")))
        .collect()
}

fn clickbench_query_files(queries_dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for n in 1..=43 {
        let filename = format!("q{n:02}.sql");
        if queries_dir.join(&filename).exists() {
            files.push((format!("q{n:02}"), filename));
        }
    }
    if files.is_empty() {
        // Fallback: try numbered queries without leading zeros
        for n in 1..=43 {
            let filename = format!("{n}.sql");
            if queries_dir.join(&filename).exists() {
                files.push((format!("q{n:02}"), filename));
            }
        }
    }
    files
}

fn create_clickbench_schema(engine: &mut QueryEngine) -> Result<(), Box<dyn std::error::Error>> {
    // ClickBench hits table — 84 columns. turboGP ignores types (all u64),
    // but we need the column names and count to match the CSV header.
    engine.execute(
        "CREATE TABLE hits (
            WatchID BIGINT,
            JavaEnable INTEGER,
            Title VARCHAR,
            GoodEvent INTEGER,
            EventTime BIGINT,
            EventDate INTEGER,
            CounterID INTEGER,
            ClientIP INTEGER,
            RegionID INTEGER,
            UserID BIGINT,
            CounterClass INTEGER,
            OS INTEGER,
            UserAgent INTEGER,
            URL VARCHAR,
            Referer VARCHAR,
            IsRefresh INTEGER,
            RefererCategoryID INTEGER,
            RefererRegionID INTEGER,
            URLCategoryID INTEGER,
            URLRegionID INTEGER,
            ResolutionWidth INTEGER,
            ResolutionHeight INTEGER,
            ResolutionDepth INTEGER,
            FlashMajor INTEGER,
            FlashMinor INTEGER,
            FlashMinor2 VARCHAR,
            NetMajor INTEGER,
            NetMinor INTEGER,
            UserAgentMajor INTEGER,
            UserAgentMinor VARCHAR,
            CookieEnable INTEGER,
            JavascriptEnable INTEGER,
            IsMobile INTEGER,
            MobilePhone INTEGER,
            MobilePhoneModel VARCHAR,
            Params VARCHAR,
            IPNetworkID INTEGER,
            TraficSourceID INTEGER,
            SearchEngineID INTEGER,
            SearchPhrase VARCHAR,
            AdvEngineID INTEGER,
            IsArtifical INTEGER,
            WindowClientWidth INTEGER,
            WindowClientHeight INTEGER,
            ClientTimeZone INTEGER,
            ClientEventTime BIGINT,
            SilverlightVersion1 INTEGER,
            SilverlightVersion2 INTEGER,
            SilverlightVersion3 INTEGER,
            SilverlightVersion4 INTEGER,
            PageCharset VARCHAR,
            CodeVersion INTEGER,
            InterlinkingID INTEGER,
            Protocol INTEGER,
            DCKey VARCHAR,
            OptionalPrice BIGINT,
            OpenstatServiceName VARCHAR,
            OpenstatCampaignID VARCHAR,
            OpenstatAdID VARCHAR,
            OpenstatSourceID VARCHAR,
            UTMSource VARCHAR,
            UTMMedium VARCHAR,
            UTMCampaign VARCHAR,
            UTMContent VARCHAR,
            UTMTerm VARCHAR,
            FromTag VARCHAR,
            HasGCLID INTEGER,
            FirstVisit BIGINT,
            PredLastVisit BIGINT,
            LastVisit BIGINT
        )",
    )?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Do NOT initialize env_logger — keeps the binary silent.
    let cli = Cli::parse();
    let repo_dir = PathBuf::from(&cli.repo_dir);

    match cli.command {
        BenchCommand::Tpch { sf, csv_dir, output, hot_iters, start_query, end_query } => {
            eprintln!("=== TPC-H SF={sf} Native Benchmark ===");
            eprintln!("CSV dir: {csv_dir}");
            eprintln!("Repo dir: {}", repo_dir.display());

            let csv_path = Path::new(&csv_dir).canonicalize()?;
            let schema_path = repo_dir.join("benchmarks/tpch/schema/turbogp.sql");
            let queries_dir = repo_dir.join("benchmarks/tpch/queries/turbogp");

            eprintln!("Loading schema from {}...", schema_path.display());
            let mut engine = QueryEngine::in_memory();
            load_schema(&mut engine, &schema_path.to_string_lossy())?;

            eprintln!("Loading CSV data from {}...", csv_path.display());
            let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
            load_csv_data(&mut engine, &csv_path, &tables)?;

            eprintln!("Running TPC-H queries Q{:02}-Q{:02}...", start_query, end_query);
            let queries = tpch_query_files(start_query, end_query);
            run_benchmark(&mut engine, &queries_dir, &queries, &output, hot_iters, "TPC-H")?;
        }
        BenchCommand::Clickbench { csv_dir, output, hot_iters } => {
            eprintln!("=== ClickBench Native Benchmark ===");
            eprintln!("CSV dir: {csv_dir}");
            eprintln!("Repo dir: {}", repo_dir.display());

            let csv_path = Path::new(&csv_dir).canonicalize()?;
            let queries_dir = repo_dir.join("benchmarks/clickbench/queries/turbogp");

            eprintln!("Creating hits table schema...");
            let mut engine = QueryEngine::in_memory();
            create_clickbench_schema(&mut engine)?;

            eprintln!("Loading CSV data from {}...", csv_path.display());
            let hits_csv = csv_path.join("hits.csv");
            if hits_csv.exists() {
                let t0 = Instant::now();
                let n = engine.load_csv(&hits_csv.to_string_lossy(), "hits", true)?;
                eprintln!("  hits: {n} rows ({}ms)", t0.elapsed().as_millis());
            } else {
                eprintln!("ERROR: hits.csv not found at {}", hits_csv.display());
                std::process::exit(1);
            }

            eprintln!("Running ClickBench queries...");
            let queries = clickbench_query_files(&queries_dir);
            if queries.is_empty() {
                eprintln!("ERROR: no query files found in {}", queries_dir.display());
                std::process::exit(1);
            }
            run_benchmark(&mut engine, &queries_dir, &queries, &output, hot_iters, "ClickBench")?;
        }
    }

    Ok(())
}
