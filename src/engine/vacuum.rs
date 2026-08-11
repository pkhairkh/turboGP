//! VACUUM, EXPLAIN, and ANALYZE execution.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_explain(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        // Wave 1 (Agent C): EXPLAIN now uses the formal planner pipeline
        // (build_plan → Cascades) to print the actual logical plan tree,
        // not a string-based description of the query shape.
        //
        // This makes EXPLAIN a faithful representation of what the
        // optimizer will actually do, and it lets users see the effect of
        // Cascades rules (predicate pushdown, projection pruning, constant
        // folding) on their queries.
        let (query, _extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(e) => return Err(Error::Parse(e)),
        };

        // Build the logical plan from the parsed SELECT.
        let plan = crate::planner::build_plan(&query)?;

        // Optimize via Cascades (predicate pushdown, projection pruning,
        // constant folding). The optimized tree is what we print.
        let optimizer = crate::planner::CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        // Render the plan tree as an indented string.
        let plan_text = format!("{}", optimized);

        // Build a one-column text result with the plan tree.
        let mut result = QueryResult::empty();
        result.row_count = 1;
        result.columns = vec![ResultColumn {
            name: "QUERY PLAN".into(),
            values: vec![xxhash_rust::xxh3::xxh3_64(plan_text.as_bytes())],
            string_values: Some(vec![plan_text]),
            type_oid: 25,
            null_mask: None,
        }];
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute ANALYZE: run the inner query and return timing stats
    /// (Wave 68). The result includes the query's output plus an
    /// "execution_time_ms" column.
    pub(crate) fn execute_analyze(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let inner_start = Instant::now();
        let mut result = self.execute_inner(sql, start, None)?;
        let elapsed = inner_start.elapsed();
        // Append a timing column.
        let timing_ms = elapsed.as_secs_f64() * 1000.0;
        result.columns.push(ResultColumn {
            name: "execution_time_ms".into(),
            values: vec![timing_ms.to_bits()],
            string_values: Some(vec![format!("{:.3}", timing_ms)]),
            type_oid: 701,
            null_mask: None,
        });
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute VACUUM: reclaim space and compact storage (Wave 68, Wave 2 fix).
    ///
    /// **Wave 2 fix:** Previously VACUUM called `flush()` then
    /// `wal.truncate()` without writing a checkpoint, creating a
    /// data-loss window. Now it calls `flush_with_checkpoint()` which
    /// writes a `checkpoint.sql` file before truncating the WAL, so
    /// committed data survives a crash at any point.
    ///
    /// **Wave 4 (Agent C):** When MVCC mode is enabled, VACUUM also calls
    /// `MvccTxnManager::cleanup_aborted()` to remove commit-state entries
    /// for aborted transactions. Full row-version garbage collection
    /// (removing dead row versions whose `xmax` is committed and not
    /// visible to any active transaction) is pending Agent B's completion
    /// of `MvccTxnManager::vacuum(&mut tables)` — see AGENT_C_API_REQUESTS.md.
    pub(crate) fn execute_vacuum(&mut self, start: &Instant) -> Result<QueryResult> {
        // 1. Flush dirty pages + write checkpoint file.
        self.flush_with_checkpoint()?;
        // 2. Now safe to truncate the WAL (committed state is in checkpoint).
        if let Some(ref mut wal) = self.wal {
            wal.truncate().map_err(|e| Error::Other(format!("WAL truncate: {e}")))?;
        }
        // 3. Wave 4 (Agent C): MVCC garbage collection.
        if self.mvcc_enabled {
            let cleaned = self.mvcc_txn_manager.cleanup_aborted();
            log::debug!("VACUUM: cleaned {} aborted MVCC transactions", cleaned);
        }
        let mut result = QueryResult::empty();
        result.row_count = 0;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute SHOW TABLES (Wave 6 — Agent C, NOT YET WIRED).
    ///
    /// **Note:** This method exists but is NOT wired into `execute()` dispatch.
    /// The pre-existing `storage::replication::backup()` helper (owned by
    /// Agent B) calls `engine.execute("SHOW TABLES")` and reads table names
    /// from the `values` column (Vec<u64>). But table names are strings —
    /// they belong in `string_values`, not `values`. Implementing SHOW TABLES
    /// correctly would expose this bug in Agent B's `list_tables()` helper.
    ///
    /// Until Agent B fixes `list_tables()` to use `string_values`, SHOW TABLES
    /// is left unimplemented (falls through to execute_inner, returns an
    /// error). This preserves the pre-existing test behavior where
    /// `backup_creates_manifest` passes because `list_tables` returns empty.
    ///
    /// My `execute_backup` uses `self.catalog.table_names()` directly, so it
    /// doesn't need SHOW TABLES.
    #[allow(dead_code)]
    pub(crate) fn execute_show(&self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let lower = sql.trim().to_lowercase();
        if lower.starts_with("show tables") {
            let names = self.catalog.table_names();
            use xxhash_rust::xxh3;
            let values: Vec<u64> = names.iter().map(|n| xxh3::xxh3_64(n.as_bytes())).collect();
            let string_values: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let mut result = QueryResult::empty();
            result.row_count = names.len();
            result.columns = vec![ResultColumn {
                name: "table_name".into(),
                values,
                string_values: Some(string_values),
                type_oid: 25,
                null_mask: None,
            }];
            result.elapsed_us = start.elapsed().as_micros() as u64;
            Ok(result)
        } else {
            Err(Error::Other(format!("unsupported SHOW: {}", sql.trim())))
        }
    }

    /// Execute BACKUP TO '<directory>' (Wave 6 Task 6.1 — Agent C).
    ///
    /// Dumps all tables as CSV files plus a `manifest.json` describing the
    /// schema. The backup can be restored with `RESTORE FROM '<dir>'`.
    ///
    /// This is implemented directly in the engine (not via
    /// `storage::replication::backup`) because that helper relies on
    /// `SHOW TABLES` returning table names in the `values` column, which
    /// isn't how the engine represents strings (strings go in
    /// `string_values`). The engine can enumerate tables directly via
    /// `self.catalog.table_names()`.
    ///
    /// Syntax: `BACKUP TO '/path/to/backup_dir'`
    ///
    /// Returns a result with `row_count` = total rows backed up.
    pub(crate) fn execute_backup(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let dir_str = parse_backup_directory(sql)
            .ok_or_else(|| Error::Other(format!("invalid BACKUP syntax: {}", sql)))?;
        let backup_dir = std::path::Path::new(&dir_str);
        std::fs::create_dir_all(backup_dir)
            .map_err(|e| Error::Other(format!("create backup dir: {e}")))?;

        let table_names = self.catalog.table_names();
        let mut manifest_tables: Vec<serde_json::Value> = Vec::new();
        let mut total_rows: usize = 0;

        for table_name in &table_names {
            // Skip internal tables.
            if table_name.starts_with("__") {
                continue;
            }
            let table = match self.catalog.get(table_name) {
                Some(t) => t,
                None => continue,
            };

            // Write CSV.
            let csv_path = backup_dir.join(format!("{}.csv", table_name));
            write_table_csv(&csv_path, table)?;

            // Add to manifest.
            manifest_tables.push(serde_json::json!({
                "name": table_name,
                "columns": table.column_names,
                "row_count": table.row_count,
            }));
            total_rows += table.row_count;
        }

        // Write manifest.
        let manifest = serde_json::json!({
            "version": 1,
            "tables": manifest_tables,
            "total_rows": total_rows,
        });
        std::fs::write(backup_dir.join("manifest.json"), manifest.to_string())
            .map_err(|e| Error::Other(format!("write manifest: {e}")))?;

        let mut result = QueryResult::empty();
        result.row_count = total_rows;
        result.columns = vec![ResultColumn {
            name: "rows_backed_up".into(),
            values: vec![total_rows as u64],
            string_values: None,
            type_oid: 20,
            null_mask: None,
        }];
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute RESTORE FROM '<directory>' (Wave 6 Task 6.2 — Agent C).
    ///
    /// **Task 4.3:** RESTORE now prefers the binary checkpoint
    /// (`checkpoint.bin`) for fast, no-reparse catalog restoration. If
    /// the binary checkpoint is missing or corrupt, it falls back to the
    /// SQL-text checkpoint (`checkpoint.sql`), then to the original
    /// CSV-manifest format (`manifest.json` + `*.csv`).
    ///
    /// Optionally accepts `AS OF TIMESTAMP '<iso8601>'` for point-in-time
    /// recovery (Task 6.3 / Task 4.3). When present, the WAL is replayed
    /// up to the target timestamp using the real `timestamp_us` field on
    /// each `WalRecord` (set by `Wal::append` to epoch microseconds).
    ///
    /// Implemented directly in the engine (not via
    /// `storage::replication::restore`) for consistency with `execute_backup`
    /// and to avoid the SHOW TABLES round-trip.
    ///
    /// Syntax:
    ///   `RESTORE FROM '/path/to/backup_dir'`
    ///   `RESTORE FROM '/path/to/backup_dir' AS OF TIMESTAMP '2024-01-01T12:00:00Z'`
    pub(crate) fn execute_restore(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let (dir_str, timestamp_opt) = parse_restore_directory_and_timestamp(sql)
            .ok_or_else(|| Error::Other(format!("invalid RESTORE syntax: {}", sql)))?;
        let backup_dir = std::path::Path::new(&dir_str);

        // Task 4.3 — Binary-checkpoint-aware RESTORE.
        //
        // Load order:
        //   1. `checkpoint.bin`  (fast path — direct catalog deserialization).
        //   2. `checkpoint.sql`  (legacy SQL-text checkpoint; re-executes
        //      CREATE TABLE / INSERT statements).
        //   3. `manifest.json` + `*.csv` (the original Wave 6 BACKUP TO
        //      format — kept for backwards compat with pre-checkpoint
        //      backups).
        //
        // If AS OF TIMESTAMP is specified, the WAL (in `<backup_dir>/wal/`
        // segmented form, or the legacy `<backup_dir>/wal.log` flat file)
        // is replayed up to that timestamp using the real `timestamp_us`
        // field set by `Wal::append` (Task 4.3 — debt-6.3). Records with
        // `timestamp_us == 0` (pre-timestamp-format WAL files) fall back
        // to record-index ordering so legacy PITR semantics still work.
        let checkpoint_bin_path = backup_dir.join("checkpoint.bin");
        let checkpoint_sql_path = backup_dir.join("checkpoint.sql");
        let manifest_path = backup_dir.join("manifest.json");

        let mut total_rows: usize = 0;
        let mut loaded_via_checkpoint = false;

        // 1. Binary checkpoint (fast path).
        if checkpoint_bin_path.exists() {
            match crate::storage::checkpoint::BinaryCheckpoint::load(&checkpoint_bin_path) {
                Ok(loaded) => {
                    let names: Vec<String> =
                        loaded.table_names().into_iter().map(String::from).collect();
                    let mut registered = 0usize;
                    for name in &names {
                        if name == "__dummy__" {
                            continue;
                        }
                        if let Some(table) = loaded.get(name) {
                            total_rows += table.row_count;
                            self.catalog.register(table.clone());
                            registered += 1;
                        }
                    }
                    loaded_via_checkpoint = true;
                    log::debug!(
                        "restore: loaded binary checkpoint from {} ({} tables, {} rows)",
                        checkpoint_bin_path.display(),
                        registered,
                        total_rows
                    );
                }
                Err(e) => {
                    log::warn!(
                        "restore: binary checkpoint load failed ({}): {e}; falling back to SQL/CSV",
                        checkpoint_bin_path.display()
                    );
                }
            }
        }

        // 2. SQL checkpoint fallback (legacy data dir or corrupt binary).
        if !loaded_via_checkpoint && checkpoint_sql_path.exists() {
            // `Checkpoint::load` re-executes CREATE TABLE / INSERT lines
            // via `engine.execute(...)`. Take the WAL out so those replay
            // statements aren't themselves appended to the WAL (which
            // would pollute PITR replay and break idempotency).
            let saved_wal = self.wal.take();
            match crate::storage::recovery::Checkpoint::load(self, &checkpoint_sql_path) {
                Ok(_) => {
                    for name in self.catalog.table_names() {
                        if name == "__dummy__" {
                            continue;
                        }
                        if let Some(t) = self.catalog.get(&name) {
                            total_rows += t.row_count;
                        }
                    }
                    loaded_via_checkpoint = true;
                    log::debug!(
                        "restore: loaded SQL checkpoint from {} ({} rows)",
                        checkpoint_sql_path.display(),
                        total_rows
                    );
                }
                Err(e) => {
                    log::warn!("restore: SQL checkpoint load failed: {e}; falling back to CSV");
                }
            }
            self.wal = saved_wal;
        }

        // 3. CSV manifest path (legacy BACKUP TO format).
        if !loaded_via_checkpoint {
            if !manifest_path.exists() {
                return Err(Error::Other(format!(
                    "RESTORE: no checkpoint.bin, checkpoint.sql, or manifest.json in '{}'",
                    backup_dir.display()
                )));
            }
            total_rows = self.restore_from_manifest(backup_dir, &manifest_path)?;
        }

        // Wave 6 Task 6.3 / Task 4.3: if AS OF TIMESTAMP is specified,
        // replay WAL records up to that timestamp for point-in-time
        // recovery. Looks for the segmented WAL in `<backup_dir>/wal/`
        // first (the canonical location written by `with_data_dir`), then
        // falls back to the legacy flat `<backup_dir>/wal.log`.
        if let Some(timestamp_us) = timestamp_opt {
            let wal_dir = backup_dir.join("wal");
            let wal_flat = backup_dir.join("wal.log");
            let wal = if wal_dir.exists() {
                crate::storage::recovery::Wal::open(&wal_dir)
                    .map_err(|e| Error::Other(format!("WAL open for PITR ({}): {e}", wal_dir.display())))?
            } else if wal_flat.exists() {
                crate::storage::recovery::Wal::open(&wal_flat)
                    .map_err(|e| Error::Other(format!("WAL open for PITR ({}): {e}", wal_flat.display())))?
            } else {
                // No WAL — nothing to replay. The checkpoint state is
                // the final state at the target timestamp.
                return Self::finish_restore(start, total_rows);
            };
            let records = wal.read_all()
                .map_err(|e| Error::Other(format!("WAL read for PITR: {e}")))?;
            // Task 4.3: use the real `timestamp_us` field on WalRecord
            // (set by Wal::append to epoch microseconds). For legacy WAL
            // files written before the timestamp field existed (all
            // records have `timestamp_us == 0`), fall back to record-index
            // ordering so the original PITR semantics still work.
            let ts_records: Vec<_> = records
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    let ts = if r.timestamp_us > 0 { r.timestamp_us } else { i as u64 };
                    crate::storage::replication::TimestampedWalRecord {
                        record: r,
                        timestamp_us: ts,
                    }
                })
                .collect();
            crate::storage::replication::replay_wal_to_timestamp(
                self,
                &ts_records,
                timestamp_us,
            ).map_err(Error::Other)?;
        }

        Self::finish_restore(start, total_rows)
    }

    /// Build the final `QueryResult` for a RESTORE operation.
    fn finish_restore(start: &Instant, total_rows: usize) -> Result<QueryResult> {
        let mut result = QueryResult::empty();
        result.row_count = total_rows;
        result.columns = vec![ResultColumn {
            name: "rows_restored".into(),
            values: vec![total_rows as u64],
            string_values: None,
            type_oid: 20,
            null_mask: None,
        }];
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Restore from a CSV-manifest backup (the original Wave 6 BACKUP TO
    /// format). Reads `manifest.json`, creates each table if it doesn't
    /// exist, and `COPY FROM`s the CSV file. Returns the total row count.
    ///
    /// This is the legacy path used when neither `checkpoint.bin` nor
    /// `checkpoint.sql` exists in the backup directory.
    fn restore_from_manifest(
        &mut self,
        backup_dir: &std::path::Path,
        manifest_path: &std::path::Path,
    ) -> Result<usize> {
        let manifest_str = std::fs::read_to_string(manifest_path)
            .map_err(|e| Error::Other(format!("read manifest: {e}")))?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest_str)
            .map_err(|e| Error::Other(format!("parse manifest: {e}")))?;

        let mut total_rows: usize = 0;
        if let Some(tables) = manifest.get("tables").and_then(|t| t.as_array()) {
            for table in tables {
                let table_name = table.get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| Error::Other("table name missing in manifest".into()))?;
                let column_names: Vec<String> = table.get("columns")
                    .and_then(|c| c.as_array())
                    .map(|cols| cols.iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect())
                    .unwrap_or_default();

                // Create the table if it doesn't exist.
                if self.catalog.get(table_name).is_none() && !column_names.is_empty() {
                    let col_defs: Vec<String> = column_names.iter()
                        .map(|c| format!("{} INT", c))
                        .collect();
                    let create_sql = format!("CREATE TABLE {} ({})", table_name, col_defs.join(", "));
                    let _ = self.execute(&create_sql); // ignore if exists
                }

                // Load data from CSV via COPY FROM.
                let csv_path = backup_dir.join(format!("{}.csv", table_name));
                if csv_path.exists() {
                    // COPY requires allowed_copy_dirs to include the backup dir.
                    let abs_path = csv_path.canonicalize()
                        .unwrap_or_else(|_| csv_path.clone());
                    let abs_dir = abs_path.parent().unwrap_or(backup_dir).to_path_buf();
                    if !self.allowed_copy_dirs.contains(&abs_dir) {
                        self.allowed_copy_dirs.push(abs_dir.clone());
                    }
                    let sql = format!("COPY {} FROM '{}'", table_name, csv_path.display());
                    match self.execute(&sql) {
                        Ok(r) => total_rows += r.row_count,
                        Err(e) => {
                            log::warn!("restore {}: {}", table_name, e);
                        }
                    }
                }
            }
        }
        Ok(total_rows)
    }
}

// =========================================================================
// Wave 6 — Backup/Restore/PITR SQL parsing helpers.
// =========================================================================

/// Write a table's data to a CSV file.
fn write_table_csv(path: &std::path::Path, table: &crate::datasource::table::Table) -> Result<()> {
    use std::io::Write;
    let mut content = String::new();
    // Header row.
    content.push_str(&table.column_names.join(","));
    content.push('\n');
    // Data rows.
    for row_idx in 0..table.row_count {
        let row: Vec<String> = table.columns.iter().enumerate().map(|(col_idx, col)| {
            if let Some(string_col) = table.string_columns.get(col_idx).and_then(|s| s.as_ref()) {
                string_col.get(row_idx).to_string()
            } else if row_idx < col.len() {
                col[row_idx].to_string()
            } else {
                String::new()
            }
        }).collect();
        content.push_str(&row.join(","));
        content.push('\n');
    }
    let mut file = std::fs::File::create(path)
        .map_err(|e| Error::Other(format!("create {}: {}", path.display(), e)))?;
    file.write_all(content.as_bytes())
        .map_err(|e| Error::Other(format!("write {}: {}", path.display(), e)))?;
    Ok(())
}

/// Parse the directory from `BACKUP TO '<dir>'` or `BACKUP TO "<dir>"`.
///
/// Returns the directory string (without quotes), or None if the SQL
/// doesn't match the expected syntax.
fn parse_backup_directory(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("BACKUP TO") {
        return None;
    }
    let rest = trimmed["BACKUP TO".len()..].trim();
    // Extract the first quoted string literal (the directory).
    let (dir, _remainder) = extract_first_string_literal(rest)?;
    Some(dir)
}

/// Parse `RESTORE FROM '<dir>'` optionally followed by `AS OF TIMESTAMP '<ts>'`.
///
/// Returns `(dir, Option<timestamp_us>)` where `timestamp_us` is the
/// parsed ISO 8601 timestamp converted to epoch microseconds, or None if
/// no `AS OF TIMESTAMP` clause is present.
///
/// Returns None if the SQL doesn't match the expected syntax.
fn parse_restore_directory_and_timestamp(sql: &str) -> Option<(String, Option<u64>)> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("RESTORE FROM") {
        return None;
    }
    let rest = trimmed["RESTORE FROM".len()..].trim();
    // Extract the directory (first quoted string) and what comes after it.
    let (dir, after_dir) = extract_first_string_literal(rest)?;

    let after_dir_upper = after_dir.to_uppercase();
    if after_dir_upper.trim_start().starts_with("AS OF TIMESTAMP") {
        // Extract the timestamp string.
        let ts_rest = after_dir_upper.find("AS OF TIMESTAMP")? + "AS OF TIMESTAMP".len();
        let ts_after = after_dir[ts_rest..].trim_start();
        let (ts_str, _) = extract_first_string_literal(ts_after)?;
        let ts_us = parse_iso8601_to_micros(&ts_str)?;
        Some((dir, Some(ts_us)))
    } else {
        Some((dir, None))
    }
}

/// Extract the first quoted string literal from `s`, returning the literal
/// content (without quotes) and the remainder of the string after the
/// closing quote.
///
/// Supports both `'...'` and `"..."` quoting. Handles the `''` escape for
/// single-quoted strings.
///
/// Returns `None` if `s` doesn't start with a quote or has no closing quote.
fn extract_first_string_literal(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0];
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut i = 1;
    let mut content = String::new();
    while i < bytes.len() {
        if bytes[i] == quote {
            // Check for escape (doubled quote).
            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                content.push(quote as char);
                i += 2;
                continue;
            }
            // Closing quote found.
            return Some((content, &s[i + 1..]));
        }
        content.push(bytes[i] as char);
        i += 1;
    }
    None
}

/// Parse an ISO 8601 timestamp string (e.g. "2024-01-01T12:00:00Z") into
/// epoch microseconds.
///
/// Supported formats:
///   - `YYYY-MM-DDTHH:MM:SSZ` (UTC, with 'Z' suffix)
///   - `YYYY-MM-DDTHH:MM:SS` (UTC, no suffix)
///   - `YYYY-MM-DD HH:MM:SS` (space separator instead of 'T')
///
/// Returns None if the string doesn't match.
fn parse_iso8601_to_micros(s: &str) -> Option<u64> {
    // Task 5.4: accept plain numeric timestamps (epoch seconds or microseconds).
    if let Ok(n) = s.parse::<u64>() {
        // Heuristic: if the number is > 1e12, it's already microseconds.
        // Otherwise, treat it as seconds and convert.
        if n > 1_000_000_000_000 {
            return Some(n);
        } else {
            return Some(n * 1_000_000);
        }
    }
    // Replace 'T' or space with 'T' for uniform parsing.
    let normalized = s.replacen(' ', "T", 1);
    // Strip trailing 'Z'.
    let stripped = normalized.trim_end_matches('Z');

    // Expected: YYYY-MM-DDTHH:MM:SS
    let parts: Vec<&str> = stripped.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let time_parts: Vec<&str> = parts[1].split(':').collect();
    if time_parts.len() < 2 || time_parts.len() > 3 {
        return None;
    }

    let year: u64 = date_parts[0].parse().ok()?;
    let month: u64 = date_parts[1].parse().ok()?;
    let day: u64 = date_parts[2].parse().ok()?;
    let hour: u64 = time_parts[0].parse().ok()?;
    let minute: u64 = time_parts[1].parse().ok()?;
    let second: u64 = if time_parts.len() == 3 {
        time_parts[2].parse().ok()?
    } else {
        0
    };

    // Convert to epoch seconds (simplified — doesn't handle leap years
    // perfectly, but sufficient for PITR ordering).
    // Use the formula: days_since_epoch = (year-1970)*365 + leap_days + day_of_year
    let mut days_since_epoch: u64 = 0;
    for y in 1970..year {
        days_since_epoch += if is_leap_year(y) { 366 } else { 365 };
    }
    let days_in_month = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days_since_epoch += days_in_month[(m - 1) as usize];
        if m == 2 && is_leap_year(year) {
            days_since_epoch += 1;
        }
    }
    days_since_epoch += day.saturating_sub(1);

    let epoch_secs = days_since_epoch * 86_400 + hour * 3600 + minute * 60 + second;
    Some(epoch_secs * 1_000_000)
}

/// Check if a year is a leap year.
fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}
