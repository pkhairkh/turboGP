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
}
