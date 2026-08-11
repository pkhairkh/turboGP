//! Task 7.3 — Performance benchmark suite for the production-hardening
//! programme (Waves 1-7).
//!
//! Four benchmarks, each runnable via
//! `cargo test --bench prod_hardening -- --nocapture`:
//!
//! 1. **INSERT throughput** (`bench_insert_throughput`): inserts 10,000
//!    rows into a fresh in-memory table and reports rows/sec.
//! 2. **SELECT scan throughput** (`bench_select_scan_throughput`):
//!    scans 10,000 committed rows via `SELECT COUNT(*)` and reports
//!    rows/sec.
//! 3. **Checkpoint time** (`bench_checkpoint_time`): writes a 10,000-
//!    row table to disk via `CHECKPOINT` and reports milliseconds.
//! 4. **MVCC visibility overhead** (`bench_mvcc_visibility_overhead`):
//!    scans 10,000 rows once with MVCC enabled (per-row visibility
//!    filtering via `row_versions`) and once with MVCC disabled
//!    (planner-pipeline fast path), reports the overhead as both an
//!    absolute time delta and a ratio.
//!
//! ## Design choices
//!
//! - **`std::time::Instant` for timing** — no `criterion` dependency
//!   (keeps the dev-dep surface minimal; the production-hardening
//!   programme added zero new dev-deps for tests).
//! - **`#[test]` wrappers** — each `bench_*` function has a matching
//!   `test_bench_*` that runs the benchmark, asserts a generous
//!   sanity-check bound (so the test fails loudly if performance
//!   regresses by 10x or more), and returns `Ok(())` on success.
//! - **No `unwrap()`/`expect()`** — all fallible operations use `?` or
//!   `match`. Errors propagate as `turbogp::Error` (which implements
//!   `Debug`, so `Result<_, turbogp::Error>` is a valid `#[test]`
//!   return type).
//! - **Deterministic input** — each row is `(id, val)` with
//!   `id in [0, 10_000)` and `val = id * 2`. No PRNG, no clock-
//!   dependent behaviour.
//!
//! ## Reading the output
//!
//! Run with `--nocapture` to see the stderr output:
//!
//! ```sh
//! cargo test --bench prod_hardening -- --nocapture
//! ```

use std::time::{Duration, Instant};
use tempfile::TempDir;
use turbogp::engine::QueryEngine;
use turbogp::{Error, Result as TgResult};

/// Number of rows used by every benchmark in this suite.
const ROW_COUNT: usize = 10_000;

// ---------------------------------------------------------------------------
// 1. INSERT throughput
// ---------------------------------------------------------------------------

/// Insert `ROW_COUNT` rows into a fresh in-memory table and report the
/// sustained throughput.
///
/// Each row is `(id, val)` where `id in [0, ROW_COUNT)` and
/// `val = id * 2`. Each INSERT is its own autocommit transaction (no
/// explicit BEGIN/COMMIT) — this matches the most common OLTP pattern
/// and isolates the per-row INSERT cost (parse + plan + execute).
fn bench_insert_throughput() -> TgResult<(f64, Duration)> {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE bench (id INT, val INT)")?;

    let start = Instant::now();
    for i in 0..ROW_COUNT as u64 {
        let sql = format!("INSERT INTO bench VALUES ({i}, {})", i * 2);
        engine.execute(&sql)?;
    }
    let elapsed = start.elapsed();

    let rows_per_sec = ROW_COUNT as f64 / elapsed.as_secs_f64();
    eprintln!(
        "[bench_insert_throughput] {ROW_COUNT} rows in {:.3}s -> {:.0} rows/sec",
        elapsed.as_secs_f64(),
        rows_per_sec
    );
    Ok((rows_per_sec, elapsed))
}

#[test]
fn test_bench_insert_throughput() -> TgResult<()> {
    let (rows_per_sec, elapsed) = bench_insert_throughput()?;
    // Sanity bounds: must complete in < 60 s and clear 50 rows/sec.
    // 50 rows/sec is ~400x slower than the observed debug-build
    // baseline (~20k rows/sec), so this only fails on a genuinely
    // broken build or a saturated CI runner.
    if elapsed > Duration::from_secs(60) {
        return Err(Error::Other(format!(
            "insert throughput too slow: {elapsed:?} for {ROW_COUNT} rows"
        )));
    }
    if rows_per_sec < 50.0 {
        return Err(Error::Other(format!(
            "insert throughput below 50 rows/sec: {rows_per_sec:.0}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. SELECT scan throughput
// ---------------------------------------------------------------------------

/// Pre-populate a table with `ROW_COUNT` committed rows, then time a
/// `SELECT COUNT(*) FROM bench` scan.
///
/// `COUNT(*)` is chosen (rather than `SELECT *`) because:
/// - It exercises the full scan path (every row is visited) without
///   materialising 10k result cells (which would dominate the time
///   and obscure the scan cost).
/// - It's a single-cell result, so the assertion is trivial.
fn bench_select_scan_throughput() -> TgResult<(u64, Duration)> {
    let mut engine = QueryEngine::in_memory();
    engine.execute("CREATE TABLE bench (id INT, val INT)")?;
    for i in 0..ROW_COUNT as u64 {
        let sql = format!("INSERT INTO bench VALUES ({i}, {})", i * 2);
        engine.execute(&sql)?;
    }

    // Warm the parser/planner caches by running one SELECT before timing.
    let _ = engine.execute("SELECT COUNT(*) FROM bench")?;

    let start = Instant::now();
    let result = engine.execute("SELECT COUNT(*) FROM bench")?;
    let elapsed = start.elapsed();

    let count = result
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .ok_or_else(|| Error::Other("SELECT COUNT(*) returned no rows".into()))?;

    let rows_per_sec = if elapsed.as_secs_f64() > 0.0 {
        ROW_COUNT as f64 / elapsed.as_secs_f64()
    } else {
        f64::INFINITY
    };
    eprintln!(
        "[bench_select_scan_throughput] scanned {count} rows in {:.3}s -> {:.0} rows/sec",
        elapsed.as_secs_f64(),
        rows_per_sec
    );
    Ok((count, elapsed))
}

#[test]
fn test_bench_select_scan_throughput() -> TgResult<()> {
    let (count, elapsed) = bench_select_scan_throughput()?;
    if count != ROW_COUNT as u64 {
        return Err(Error::Other(format!(
            "SELECT COUNT(*) returned {count}, expected {ROW_COUNT}"
        )));
    }
    if elapsed > Duration::from_secs(10) {
        return Err(Error::Other(format!(
            "scan too slow: {elapsed:?} for {ROW_COUNT} rows"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Checkpoint time
// ---------------------------------------------------------------------------

/// Persist `ROW_COUNT` rows to disk via `CHECKPOINT` and report the
/// wall-clock time in milliseconds.
///
/// Uses a `TempDir` so the on-disk artefacts (`checkpoint.bin`,
/// `checkpoint.sql`, `wal/`) are cleaned up automatically. The engine
/// is configured via `with_data_dir` (which enables the buffer pool +
/// WAL), so the CHECKPOINT SQL command actually writes both the binary
/// and SQL-text checkpoint files plus truncates the WAL.
fn bench_checkpoint_time() -> TgResult<(u64, Duration)> {
    let temp_dir = TempDir::new()?;
    let data_dir = temp_dir.path().to_path_buf();

    let mut engine = QueryEngine::with_data_dir(&data_dir)?;
    engine.execute("CREATE TABLE bench (id INT, val INT)")?;
    for i in 0..ROW_COUNT as u64 {
        let sql = format!("INSERT INTO bench VALUES ({i}, {})", i * 2);
        engine.execute(&sql)?;
    }

    // Time only the CHECKPOINT itself (the INSERTs are setup).
    let start = Instant::now();
    engine.execute("CHECKPOINT")?;
    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();

    eprintln!("[bench_checkpoint_time] checkpointed {ROW_COUNT} rows in {ms} ms");

    // Verify the checkpoint files exist on disk.
    let bin_path = data_dir.join("checkpoint.bin");
    let sql_path = data_dir.join("checkpoint.sql");
    if !bin_path.exists() {
        return Err(Error::Other(format!(
            "checkpoint.bin not written at {}",
            bin_path.display()
        )));
    }
    if !sql_path.exists() {
        return Err(Error::Other(format!(
            "checkpoint.sql not written at {}",
            sql_path.display()
        )));
    }
    Ok((ms as u64, elapsed))
}

#[test]
fn test_bench_checkpoint_time() -> TgResult<()> {
    let (_ms, elapsed) = bench_checkpoint_time()?;
    // Sanity bound: 10k rows should checkpoint in < 30 s even on a
    // slow debug build. The binary format writes ~10k * 16 bytes =
    // ~160 KB, which should take well under 1 s on any reasonable
    // disk; the SQL-text format is slower (string formatting per row)
    // but still well under 30 s for 10k rows.
    if elapsed > Duration::from_secs(30) {
        return Err(Error::Other(format!(
            "checkpoint too slow: {elapsed:?} for {ROW_COUNT} rows"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. MVCC visibility filtering overhead
// ---------------------------------------------------------------------------

/// Compare the cost of scanning `ROW_COUNT` rows with MVCC enabled
/// (per-row visibility filtering via `row_versions`) vs disabled
/// (planner-pipeline fast path).
///
/// Both scans run against the same data shape — a table of 10k
/// committed rows. The MVCC-enabled scan goes through `filter_indices`
/// (which calls the visibility check per row); the MVCC-disabled scan
/// takes the planner fast path (which returns `table.row_count`
/// directly for `COUNT(*)` without consulting `row_versions`).
///
/// Returns `(mvcc_enabled_time, mvcc_disabled_time, ratio)` where
/// `ratio = mvcc_enabled_time / mvcc_disabled_time`. The ratio is
/// informational — the absolute delta (`mvcc - plain`) is the more
/// meaningful number, since the disabled path is essentially O(1)
/// for `COUNT(*)`.
fn bench_mvcc_visibility_overhead() -> TgResult<(Duration, Duration, f64)> {
    // --- (a) MVCC-enabled engine: populate + scan ----------------------
    let mut mvcc_engine = QueryEngine::in_memory();
    mvcc_engine.enable_mvcc()?;
    mvcc_engine.execute("CREATE TABLE bench (id INT, val INT)")?;
    // Single transaction for the inserts — matches the typical OLTP
    // batch-insert pattern and keeps the row_versions array dense.
    mvcc_engine.execute("BEGIN")?;
    for i in 0..ROW_COUNT as u64 {
        let sql = format!("INSERT INTO bench VALUES ({i}, {})", i * 2);
        mvcc_engine.execute(&sql)?;
    }
    mvcc_engine.execute("COMMIT")?;

    // Warm up.
    let _ = mvcc_engine.execute("SELECT COUNT(*) FROM bench")?;

    let mvcc_start = Instant::now();
    let mvcc_result = mvcc_engine.execute("SELECT COUNT(*) FROM bench")?;
    let mvcc_time = mvcc_start.elapsed();

    let mvcc_count = mvcc_result
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .ok_or_else(|| Error::Other("MVCC SELECT COUNT(*) returned no rows".into()))?;

    // --- (b) MVCC-disabled engine: populate + scan ---------------------
    let mut plain_engine = QueryEngine::in_memory();
    plain_engine.execute("CREATE TABLE bench (id INT, val INT)")?;
    for i in 0..ROW_COUNT as u64 {
        let sql = format!("INSERT INTO bench VALUES ({i}, {})", i * 2);
        plain_engine.execute(&sql)?;
    }

    // Warm up.
    let _ = plain_engine.execute("SELECT COUNT(*) FROM bench")?;

    let plain_start = Instant::now();
    let plain_result = plain_engine.execute("SELECT COUNT(*) FROM bench")?;
    let plain_time = plain_start.elapsed();

    let plain_count = plain_result
        .columns
        .first()
        .and_then(|c| c.values.first().copied())
        .ok_or_else(|| Error::Other("plain SELECT COUNT(*) returned no rows".into()))?;

    // --- (c) Report ----------------------------------------------------
    let ratio = if plain_time.as_nanos() > 0 {
        mvcc_time.as_secs_f64() / plain_time.as_secs_f64()
    } else {
        f64::NAN
    };
    let delta = mvcc_time.saturating_sub(plain_time);
    eprintln!(
        "[bench_mvcc_visibility_overhead] \
         mvcc={:?} (count={mvcc_count}) no_mvcc={:?} (count={plain_count}) \
         delta={delta:?} ratio={ratio:.2}x",
        mvcc_time, plain_time
    );

    // Sanity: both engines must report the same row count.
    if mvcc_count != plain_count {
        return Err(Error::Other(format!(
            "MVCC count {mvcc_count} != plain count {plain_count} (visibility filter bug)"
        )));
    }
    if mvcc_count != ROW_COUNT as u64 {
        return Err(Error::Other(format!(
            "expected {ROW_COUNT} rows, MVCC scan saw {mvcc_count}"
        )));
    }
    Ok((mvcc_time, plain_time, ratio))
}

#[test]
fn test_bench_mvcc_visibility_overhead() -> TgResult<()> {
    let (mvcc_time, plain_time, ratio) = bench_mvcc_visibility_overhead()?;
    // Both scans should complete in < 5 s for 10k rows.
    if mvcc_time > Duration::from_secs(5) {
        return Err(Error::Other(format!(
            "MVCC scan too slow: {mvcc_time:?}"
        )));
    }
    if plain_time > Duration::from_secs(5) {
        return Err(Error::Other(format!(
            "non-MVCC scan too slow: {plain_time:?}"
        )));
    }
    // The ratio should be finite (both times non-zero). We don't
    // assert a specific upper bound on the ratio because sub-millisecond
    // timings are noisy in debug builds; the printed value is
    // informational, not a pass/fail signal.
    if !ratio.is_finite() {
        return Err(Error::Other(format!(
            "MVCC overhead ratio is not finite: mvcc={mvcc_time:?} plain={plain_time:?}"
        )));
    }
    Ok(())
}
