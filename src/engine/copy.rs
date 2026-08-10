//! COPY execution — bulk import/export.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_copy(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let lower = sql.to_lowercase();
        let parts: Vec<&str> = sql.split_whitespace().collect();
        if parts.len() < 4 {
            return Err(Error::Other("COPY requires: COPY <table> TO|FROM 'file'".into()));
        }
        let table_name = parts[1];
        let direction = parts[2].to_uppercase();
        // The file path is the 4th part, possibly quoted.
        let file_path = parts[3].trim_matches(|c| c == '\'' || c == '"');
        // Wave 2 security: validate COPY path against allow-list.
        let path = std::path::Path::new(file_path);
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let allowed = self.allowed_copy_dirs.iter().any(|dir| canonical.starts_with(dir));
        if !allowed {
            return Err(Error::Other(format!(
                "COPY path '{}' not in allowed_copy_dirs (SQLSTATE 42501)",
                file_path
            )));
        }
        match direction.as_str() {
            "TO" => {
                // Export the table to a CSV file.
                let table = self
                    .catalog
                    .get(table_name)
                    .ok_or_else(|| Error::NotFound(format!("table '{}'", table_name)))?
                    .clone();
                let mut csv = String::new();
                // Header row.
                csv.push_str(&table.column_names.join(","));
                csv.push('\n');
                // Data rows.
                for row in 0..table.row_count {
                    let vals: Vec<String> = (0..table.columns.len())
                        .map(|ci| table.columns[ci].get(row).copied().unwrap_or(0).to_string())
                        .collect();
                    csv.push_str(&vals.join(","));
                    csv.push('\n');
                }
                std::fs::write(file_path, csv).map_err(|e| Error::Other(format!("write: {e}")))?;
                let mut result = QueryResult::empty();
                result.row_count = table.row_count;
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            "FROM" => {
                // Import from a CSV file.
                let content = std::fs::read_to_string(file_path)
                    .map_err(|e| Error::Other(format!("read: {e}")))?;
                let lines: Vec<&str> = content.lines().collect();
                if lines.is_empty() {
                    return Err(Error::Other("CSV file is empty".into()));
                }
                // First line is the header — skip it (or use it to verify columns).
                let mut count = 0;
                for line in &lines[1..] {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let vals: Vec<String> = line.split(',').map(|s| s.trim().to_string()).collect();
                    let val_strs: Vec<String> = vals
                        .iter()
                        .map(|v| {
                            // If it's a number, use it directly; otherwise quote it
                            // with single-quote doubling to prevent SQL injection.
                            if v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok() {
                                v.clone()
                            } else {
                                // Double internal single quotes to prevent injection
                                // from malicious CSV cell values.
                                let escaped = v.replace('\'', "''");
                                format!("'{}'", escaped)
                            }
                        })
                        .collect();
                    let insert_sql =
                        format!("INSERT INTO {} VALUES ({})", table_name, val_strs.join(", "));
                    self.execute_inner(&insert_sql, start, None)?;
                    count += 1;
                }
                let mut result = QueryResult::empty();
                result.row_count = count;
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            _ => {
                Err(Error::Other(format!("COPY direction must be TO or FROM, got: {}", direction)))
            }
        }
    }
}
