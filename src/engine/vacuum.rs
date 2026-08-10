//! VACUUM, EXPLAIN, and ANALYZE execution.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_explain(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let (query, _extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(e) => return Err(Error::Parse(e)),
        };
        // Build a textual plan description.
        let mut plan_lines = Vec::new();
        plan_lines.push(format!("Query: {}", sql.trim()));
        plan_lines.push(format!("Table: {}", query.from));
        plan_lines.push(format!("Select items: {}", query.select.len()));
        if !query.joins.is_empty() {
            plan_lines.push(format!("Joins: {}", query.joins.len()));
        }
        if query.where_clause.is_some() {
            plan_lines.push("Where: present".into());
        }
        if !query.group_by.is_empty() {
            plan_lines.push(format!("Group By: {:?}", query.group_by));
        }
        if query.having.is_some() {
            plan_lines.push("Having: present".into());
        }
        if !query.order_by.is_empty() {
            plan_lines.push(format!("Order By: {} columns", query.order_by.len()));
        }
        if let Some(limit) = query.limit {
            plan_lines.push(format!("Limit: {}", limit));
        }
        if query.distinct {
            plan_lines.push("Distinct: true".into());
        }
        let table = self.catalog.get(&query.from);
        if let Some(t) = table {
            plan_lines.push(format!("Rows: {}", t.row_count));
            plan_lines.push(format!("Columns: {}", t.column_names.join(", ")));
        }
        // Return as a single-column text result.
        let plan_text = plan_lines.join("\n");
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
    pub(crate) fn execute_vacuum(&mut self, start: &Instant) -> Result<QueryResult> {
        // 1. Flush dirty pages + write checkpoint file.
        self.flush_with_checkpoint()?;
        // 2. Now safe to truncate the WAL (committed state is in checkpoint).
        if let Some(ref mut wal) = self.wal {
            wal.truncate().map_err(|e| Error::Other(format!("WAL truncate: {e}")))?;
        }
        let mut result = QueryResult::empty();
        result.row_count = 0;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}
