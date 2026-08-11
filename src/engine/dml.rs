//! DML execution — INSERT, UPDATE, DELETE.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_dml(&mut self, dml: crate::sql::DmlStatement, txn_id: Option<u64>) -> Result<QueryResult> {
        match dml {
            crate::sql::DmlStatement::Insert(ins) => self.execute_insert(ins, txn_id),
            crate::sql::DmlStatement::Update(upd) => self.execute_update(upd, txn_id),
            crate::sql::DmlStatement::Delete(del) => self.execute_delete(del, txn_id),
        }
    }

    /// Execute an INSERT statement.
    ///
    /// Wave 56c fix: when inserting a string literal into a VARCHAR / NVARCHAR
    /// / TEXT column, the original string is now preserved in the column's
    /// `string_columns` sidecar (`StringSearchColumn`). Previously, the string
    /// was hashed to a u64 (via `parse_value_cell`) and the original was lost —
    /// so subsequent `SELECT col` could only return the hash, and JSON_VALUE
    /// / LIKE / range comparisons on inserted strings were broken.
    pub(crate) fn execute_insert(&mut self, ins: crate::sql::Insert, txn_id: Option<u64>) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&ins.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", ins.table)))?;

        // Determine column indices.
        let col_indices: Vec<usize> = match &ins.columns {
            Some(cols) => {
                let mut idxs = Vec::with_capacity(cols.len());
                for col_name in cols {
                    let idx = table
                        .column_idx(col_name)
                        .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
                    idxs.push(idx);
                }
                idxs
            }
            None => (0..table.columns.len()).collect(),
        };

        if col_indices.len() != ins.values.first().map(|r| r.len()).unwrap_or(0) {
            return Err(Error::Other(format!(
                "column count ({}) doesn't match value count ({})",
                col_indices.len(),
                ins.values.first().map(|r| r.len()).unwrap_or(0)
            )));
        }

        let n_new_rows = ins.values.len();

        // Wave 3 (A2): Enforce NOT NULL and PRIMARY KEY constraints.
        if let Some(ref schema) = table.schema {
            for (row_idx, row_vals) in ins.values.iter().enumerate() {
                for (i, &col_idx) in col_indices.iter().enumerate() {
                    let val_str = &row_vals[i];
                    let is_null = val_str.trim().eq_ignore_ascii_case("null");
                    // Check NOT NULL constraint.
                    if let Some(col_schema) = schema.columns.get(col_idx) {
                        if col_schema.not_null && is_null {
                            return Err(Error::Other(format!(
                                "23502: NOT NULL constraint violated for column \"{}\" on row {}",
                                col_schema.name, row_idx
                            )));
                        }
                        // Check PRIMARY KEY uniqueness.
                        if col_schema.primary_key && !is_null {
                            let new_cell = parse_value_cell(val_str);
                            let col = &table.columns[col_idx];
                            if col.iter().any(|&existing| existing == new_cell) {
                                return Err(Error::Other(format!(
                                    "23505: duplicate key value violates UNIQUE constraint for PRIMARY KEY column \"{}\"",
                                    col_schema.name
                                )));
                            }
                        }
                    }
                }
            }
        }

        // Wave 56c: track which columns had string literals inserted, so we
        // can update their string_columns sidecar after the loop. We collect
        // the string values into a per-column Vec<String> and rebuild the
        // StringSearchColumn at the end.
        let mut string_inserts: std::collections::HashMap<usize, Vec<String>> =
            std::collections::HashMap::new();
        // Determine which columns are string-typed (VARCHAR / NVARCHAR / TEXT).
        let string_cols: std::collections::HashSet<usize> = (0..table.columns.len())
            .filter(|&i| table.schema.as_ref().map(|s| s.is_string(i)).unwrap_or(false))
            .collect();

        // Extend each column with the new values.
        for row_vals in &ins.values {
            for (i, &col_idx) in col_indices.iter().enumerate() {
                let val_str = &row_vals[i];
                let is_null = val_str.trim().eq_ignore_ascii_case("null");
                let cell = parse_value_cell(val_str);
                // COW: Arc::make_mut gives us a mutable Vec if we're the
                // sole owner, or clones if shared.
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col.push(cell);

                // Wave 56c: if this is a string column and the value is a
                // string literal, preserve the original string.
                if string_cols.contains(&col_idx) && !is_null {
                    let inner = extract_string_literal(val_str);
                    if let Some(s) = inner {
                        string_inserts.entry(col_idx).or_default().push(s);
                    } else {
                        // Non-literal value in a string column (e.g. a number).
                        // Push the raw string as a fallback so the sidecar
                        // stays aligned with the column length.
                        string_inserts.entry(col_idx).or_default().push(val_str.trim().to_string());
                    }
                }

                // Update the NULL bitmap (Wave 32): mark the cell as NULL
                // if the value was explicitly NULL.
                if is_null {
                    // Ensure a bitmap exists for this column.
                    if col_idx >= table.null_bitmaps.len() {
                        table.null_bitmaps.resize(table.columns.len(), None);
                    }
                    if table.null_bitmaps[col_idx].is_none() {
                        // Initialize bitmap: all existing rows are non-NULL.
                        let mut bm = crate::types::null_bitmap::NullBitmap::new(table.row_count);
                        // The new row (at index table.row_count) is NULL.
                        bm.push_null();
                        table.null_bitmaps[col_idx] = Some(bm);
                    } else {
                        table.null_bitmaps[col_idx].as_mut().unwrap().push_null();
                    }
                    // Wave 56c: also push an empty string to keep the sidecar aligned.
                    if string_cols.contains(&col_idx) {
                        string_inserts.entry(col_idx).or_default().push(String::new());
                    }
                } else {
                    // Non-NULL value: ensure bitmap exists and push non-null.
                    if col_idx < table.null_bitmaps.len() {
                        if let Some(ref mut bm) = table.null_bitmaps[col_idx] {
                            bm.push_non_null();
                        }
                    }
                }
            }
        }
        table.row_count += n_new_rows;

        // Task 3.2 (debt-4.2): populate Table.row_versions when MVCC mode
        // is enabled. Each inserted row gets a RowVersion with xmin = txn_id
        // (or 0 for autocommit) and xmax = None (still live).
        if self.mvcc_enabled {
            let xmin = txn_id.unwrap_or(0);
            for _ in 0..n_new_rows {
                table.row_versions.push(crate::txn::mvcc::RowVersion::new(xmin, Vec::new()));
            }
        }

        // Wave 56c: rebuild the string_columns sidecar for any column that
        // received string inserts. We merge with any existing strings.
        for (col_idx, new_strings) in string_inserts {
            // Ensure string_columns is sized.
            while table.string_columns.len() <= col_idx {
                table.string_columns.push(None);
            }
            // If there's an existing StringSearchColumn, merge; else build fresh.
            let existing = table.string_columns[col_idx].clone();
            let merged_strings: Vec<String> = if let Some(sc) = existing {
                let mut v = sc.strings.clone();
                v.extend(new_strings);
                v
            } else {
                // Pad with empty strings for any rows before the inserted ones
                // (in case the column had rows before string tracking was added).
                let mut v = Vec::with_capacity(table.row_count);
                for _ in 0..(table.row_count - new_strings.len()) {
                    v.push(String::new());
                }
                v.extend(new_strings);
                v
            };
            table.string_columns[col_idx] = Some(std::sync::Arc::new(
                crate::exec::fm_index::StringSearchColumn::new(merged_strings),
            ));
        }

        // Wave 56d: if this is a temporal table, sync the inserted rows to
        // the TemporalTable sidecar so FOR SYSTEM_TIME AS OF queries see them.
        // We collect the row values (as Vec<u64>) BEFORE releasing the table
        // borrow, then update the temporal sidecar.
        let table_name = ins.table.clone();
        let mut temporal_rows: Vec<Vec<u64>> = Vec::new();
        if self.temporals.contains_key(&table_name) {
            // Re-read the table (immutable borrow) to get the just-inserted rows.
            // The new rows are the last `n_new_rows` of each column.
            for row_i in 0..n_new_rows {
                let row_idx = table.row_count - n_new_rows + row_i;
                let mut row_vals = Vec::with_capacity(table.columns.len());
                for col_idx in 0..table.columns.len() {
                    let v = table.columns[col_idx].get(row_idx).copied().unwrap_or(0);
                    row_vals.push(v);
                }
                temporal_rows.push(row_vals);
            }
        }

        // Now release the table borrow and update the temporal sidecar.
        drop(table);
        if let Some(temporal) = self.temporals.get_mut(&table_name) {
            for row_vals in temporal_rows {
                temporal.insert(row_vals);
            }
        }

        // Return a result with the number of rows inserted.
        let mut result = QueryResult::empty();
        result.row_count = n_new_rows;
        Ok(result)
    }

    /// Execute an UPDATE statement. Supports simple `col = value` assignments
    /// and a WHERE clause with `col = value` equality (AND/OR supported
    /// via the existing expression evaluator in a future wave).
    ///
    /// Wave 50 fix (Bug 6): when an assignment sets a column to NULL, the
    /// column's NULL bitmap is now updated so subsequent `COUNT(col)` /
    /// `AVG(col)` correctly exclude the row. Previously the cell was set
    /// to 0 but the bitmap still considered it non-NULL.
    pub(crate) fn execute_update(&mut self, upd: crate::sql::Update, txn_id: Option<u64>) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&upd.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", upd.table)))?;

        // Parse assignments into (col_idx, new_value_cell, is_null) triples.
        // `is_null` is true when the RHS is the literal `NULL`.
        let mut assigns: Vec<(usize, u64, bool)> = Vec::with_capacity(upd.assignments.len());
        for (col_name, expr) in &upd.assignments {
            let idx = table
                .column_idx(col_name)
                .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
            // Wave 5: `expr` is now a parsed `ast::Expr`. Convert back to
            // SQL string for the existing parse_value_cell helper. A proper
            // refactor would update parse_value_cell to accept &Expr, but
            // that's a larger change deferred to a later wave.
            let expr_str = expr.to_string();
            let trimmed = expr_str.trim();
            let is_null = trimmed.eq_ignore_ascii_case("NULL") || matches!(expr, crate::sql::ast::Expr::Literal(crate::sql::ast::Value::Null));
            // For now, the expression must be a simple literal.
            let cell = parse_value_cell(&expr_str);
            assigns.push((idx, cell, is_null));
        }

        // Determine which rows match the WHERE clause.
        let n = table.row_count;
        let mut updated = 0usize;
        let match_mask: Vec<bool> = if let Some(where_expr) = &upd.where_clause {
            // Wave 5: where_clause is now ast::Expr. Convert to SQL string
            // for the existing eval_simple_where helper.
            let where_str = where_expr.to_string();
            eval_simple_where(table, &where_str)?
        } else {
            vec![true; n]
        };

        // Ensure NULL bitmaps exist for every column that we might mark NULL.
        // We grow `null_bitmaps` to match `columns.len()` if needed.
        while table.null_bitmaps.len() < table.columns.len() {
            table.null_bitmaps.push(None);
        }

        for (row_idx, &matches) in match_mask.iter().enumerate() {
            if !matches {
                continue;
            }
            for &(col_idx, val, is_null) in &assigns {
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col[row_idx] = val;
                // Wave 50 fix: update the NULL bitmap to reflect the new
                // value. If we set the cell to NULL, mark the bitmap; if
                // we set it to a non-NULL value, clear the bitmap entry.
                if col_idx < table.null_bitmaps.len() {
                    if is_null {
                        // Ensure a bitmap exists, then mark this row NULL.
                        if table.null_bitmaps[col_idx].is_none() {
                            let mut bm = crate::types::null_bitmap::NullBitmap::new(0);
                            // Backfill existing rows as non-null so the
                            // bitmap is correctly sized up to row_idx.
                            for _ in 0..row_idx {
                                bm.push_non_null();
                            }
                            table.null_bitmaps[col_idx] = Some(bm);
                        }
                        // Ensure the bitmap has entries up to row_idx.
                        let bm = table.null_bitmaps[col_idx].as_mut().unwrap();
                        while bm.len() <= row_idx {
                            bm.push_non_null();
                        }
                        bm.set_null(row_idx);
                    } else {
                        // Clear the NULL flag if a bitmap exists.
                        if let Some(ref mut bm) = table.null_bitmaps[col_idx] {
                            while bm.len() <= row_idx {
                                bm.push_non_null();
                            }
                            bm.set_non_null(row_idx);
                        }
                    }
                }
            }
            updated += 1;
        }

        // Task 2.2 (debt-4.2): populate Table.row_versions when MVCC mode
        // is enabled. For each matched row, the old version is tombstoned
        // (xmax = txn_id) via `mark_deleted`, and a new RowVersion carrying
        // the post-update column values is appended. This must run BEFORE
        // the temporal-sync block below, which drops the `table` borrow.
        //
        // The new version's `values` are read from the already-mutated
        // `columns` (the in-place update loop above wrote the new cells),
        // so they reflect the post-UPDATE state of the row.
        if self.mvcc_enabled {
            let xmin = txn_id.unwrap_or(0);
            let ncols = table.columns.len();
            for (row_idx, &matches) in match_mask.iter().enumerate() {
                if !matches {
                    continue;
                }
                // Build the new values from the (already updated) columns.
                let mut new_values = Vec::with_capacity(ncols);
                for ci in 0..ncols {
                    new_values.push(table.columns[ci].get(row_idx).copied().unwrap_or(0));
                }
                // Tombstone the old version (sets xmax). If the row had no
                // version yet (e.g. table loaded without MVCC tracking),
                // `mark_deleted` returns false and we skip the append — the
                // absence of a prior version means there is nothing to
                // supersede.
                let marked = table.mark_deleted(row_idx, xmin);
                if marked {
                    table.append_row_version(crate::txn::mvcc::RowVersion::new(xmin, new_values));
                }
            }
        }

        // Wave 56d: if this is a temporal table, sync the update to the
        // TemporalTable sidecar. We collect the matched row indices and
        // the new values, then call temporal.update(...).
        let table_name = upd.table.clone();
        let is_temporal = self.temporals.contains_key(&table_name);
        if is_temporal {
            // Collect (predicate_fn, new_values) for the temporal update.
            // The predicate matches any row whose first column value equals
            // the matched row's first column value (best-effort — the
            // TemporalTable's update() takes a closure, so we match by PK).
            // We build a list of (old_pk, new_row_values) pairs.
            let mut updates: Vec<(u64, Vec<u64>)> = Vec::new();
            for (row_idx, &matches) in match_mask.iter().enumerate() {
                if !matches {
                    continue;
                }
                // Get the old PK (first column) — used to find the row in the
                // TemporalTable.
                let old_pk =
                    table.columns.first().and_then(|c| c.get(row_idx).copied()).unwrap_or(0);
                // Build the new row values: copy the current row, then apply
                // the assignments.
                let mut new_row: Vec<u64> = (0..table.columns.len())
                    .map(|ci| table.columns[ci].get(row_idx).copied().unwrap_or(0))
                    .collect();
                for &(col_idx, val, _is_null) in &assigns {
                    if col_idx < new_row.len() {
                        new_row[col_idx] = val;
                    }
                }
                updates.push((old_pk, new_row));
            }
            drop(table);
            if let Some(temporal) = self.temporals.get_mut(&table_name) {
                for (old_pk, new_row) in updates {
                    temporal.update(|row| row.first().copied() == Some(old_pk), new_row);
                }
            }
        }

        let mut result = QueryResult::empty();
        result.row_count = updated;
        Ok(result)
    }

    /// Execute a DELETE statement.
    pub(crate) fn execute_delete(&mut self, del: crate::sql::Delete, txn_id: Option<u64>) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&del.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

        let n = table.row_count;
        let delete_mask: Vec<bool> = if let Some(where_expr) = &del.where_clause {
            // Wave 5: where_clause is now ast::Expr. Convert to SQL string
            // for the existing eval_simple_where helper.
            let where_str = where_expr.to_string();
            eval_simple_where(table, &where_str)?
        } else {
            vec![true; n]
        };

        let deleted = delete_mask.iter().filter(|&&b| b).count();
        if deleted == 0 {
            let mut result = QueryResult::empty();
            result.row_count = 0;
            return Ok(result);
        }

        // Task 2.3 (debt-4.2): in MVCC mode, tombstone the matched rows'
        // versions (xmax = txn_id) and leave the column data in place for
        // VACUUM to reclaim later. We do NOT rebuild the columns or
        // decrement `row_count` here — that's the VACUUM path's job. The
        // `row_versions` vec stays aligned with `columns` (both reflect
        // the original row indices, with tombstones marking deleted rows).
        //
        // The temporal sidecar (if any) still receives a logical delete
        // so FOR SYSTEM_TIME queries see the row's end-time, but its
        // column rebuild is skipped to preserve the row-version alignment.
        if self.mvcc_enabled {
            let xmax = txn_id.unwrap_or(0);
            for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                if delete_flag {
                    // `mark_deleted` returns false if the row had no version
                    // (e.g. a table loaded before MVCC tracking was enabled)
                    // or was already deleted. Either way there is nothing
                    // useful to do here — we leave the row in place.
                    let _ = table.mark_deleted(row_idx, xmax);
                }
            }

            // Sync the temporal sidecar (if any) WITHOUT rebuilding columns.
            let table_name = del.table.clone();
            let is_temporal = self.temporals.contains_key(&table_name);
            if is_temporal {
                let mut pks_to_delete: Vec<u64> = Vec::new();
                for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                    if delete_flag {
                        let pk = table
                            .columns
                            .first()
                            .and_then(|c| c.get(row_idx).copied())
                            .unwrap_or(0);
                        pks_to_delete.push(pk);
                    }
                }
                drop(table);
                if let Some(temporal) = self.temporals.get_mut(&table_name) {
                    for pk in pks_to_delete {
                        temporal.delete(|row| row.first().copied() == Some(pk));
                    }
                }
            }

            let mut result = QueryResult::empty();
            result.row_count = deleted;
            return Ok(result);
        }

        // Wave 56d: if this is a temporal table, sync the delete to the
        // TemporalTable sidecar BEFORE rebuilding the columns (we need the
        // old row values to identify which rows to delete from the temporal).
        let table_name = del.table.clone();
        let is_temporal = self.temporals.contains_key(&table_name);
        if is_temporal {
            // Collect the PKs of rows to delete (first column value).
            let mut pks_to_delete: Vec<u64> = Vec::new();
            for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                if delete_flag {
                    let pk =
                        table.columns.first().and_then(|c| c.get(row_idx).copied()).unwrap_or(0);
                    pks_to_delete.push(pk);
                }
            }
            drop(table);
            if let Some(temporal) = self.temporals.get_mut(&table_name) {
                for pk in pks_to_delete {
                    temporal.delete(|row| row.first().copied() == Some(pk));
                }
            }
            // Re-acquire the table borrow to rebuild the columns.
            let table = self
                .catalog
                .get_mut(&table_name)
                .ok_or_else(|| Error::NotFound(format!("table \"{}\"", table_name)))?;
            // Rebuild each column keeping only non-deleted rows.
            let keep_mask: Vec<bool> = delete_mask.iter().map(|&d| !d).collect();
            for col in &mut table.columns {
                let col_ref = std::sync::Arc::make_mut(col);
                let mut new_vals = Vec::with_capacity(n - deleted);
                for (i, &keep) in keep_mask.iter().enumerate() {
                    if keep {
                        new_vals.push(col_ref[i]);
                    }
                }
                *col_ref = new_vals;
            }
            table.row_count -= deleted;
        } else {
            // Rebuild each column keeping only non-deleted rows.
            let keep_mask: Vec<bool> = delete_mask.iter().map(|&d| !d).collect();
            for col in &mut table.columns {
                let col_ref = std::sync::Arc::make_mut(col);
                let mut new_vals = Vec::with_capacity(n - deleted);
                for (i, &keep) in keep_mask.iter().enumerate() {
                    if keep {
                        new_vals.push(col_ref[i]);
                    }
                }
                *col_ref = new_vals;
            }
            table.row_count -= deleted;
        }

        let mut result = QueryResult::empty();
        result.row_count = deleted;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 2.2 — `execute_update` sets `xmax` on the old version and
    /// appends a new `RowVersion` with the post-update column values when
    /// MVCC mode is enabled.
    ///
    /// Sequence: `BEGIN; INSERT (1,10); UPDATE SET v=99 WHERE id=1; COMMIT;`
    /// Expected:
    /// - `row_versions[0].xmax == Some(txn_id)` (old version tombstoned).
    /// - `row_versions.len() == 2` (a new version was appended).
    /// - The new version (`row_versions[1]`) has `xmin == txn_id`,
    ///   `xmax == None`, and its `values` contain `99` (the updated `v`).
    #[test]
    fn test_update_sets_xmax() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.enable_mvcc()?;
        engine.execute("CREATE TABLE t (id INT, v INT)")?;

        engine.execute("BEGIN")?;
        let txn_id = engine
            .mvcc_txn_manager()
            .active_id()
            .ok_or_else(|| "active txn id should be Some after BEGIN".to_string())?;
        engine.execute("INSERT INTO t VALUES (1, 10)")?;
        engine.execute("UPDATE t SET v = 99 WHERE id = 1")?;
        engine.execute("COMMIT")?;

        let table = engine
            .catalog()
            .get("t")
            .ok_or_else(|| "table t should exist".to_string())?;
        assert!(
            !table.row_versions.is_empty(),
            "row_versions should be populated by INSERT"
        );

        // The original version (at index 0) should have been tombstoned.
        assert_eq!(
            table.row_versions[0].xmax,
            Some(txn_id),
            "UPDATE should set xmax on the old version"
        );

        // A new version should have been appended for the updated row.
        assert!(
            table.row_versions.len() >= 2,
            "UPDATE should append a new version; got len={}",
            table.row_versions.len()
        );

        let new_version = &table.row_versions[1];
        assert_eq!(new_version.xmin, txn_id, "new version's xmin is the updating txn");
        assert_eq!(new_version.xmax, None, "new version is live (xmax == None)");
        assert!(
            new_version.values.contains(&99),
            "new version should carry the updated v=99; got {:?}",
            new_version.values
        );
        Ok(())
    }

    /// Task 2.3 — `execute_delete` sets `xmax` on the old version when
    /// MVCC mode is enabled, without removing the row from `columns`.
    ///
    /// Sequence: `BEGIN; INSERT (1); DELETE WHERE id=1; COMMIT;`
    /// Expected: `row_versions[0].xmax == Some(txn_id)`.
    #[test]
    fn test_delete_sets_xmax() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.enable_mvcc()?;
        engine.execute("CREATE TABLE t (id INT)")?;

        engine.execute("BEGIN")?;
        let txn_id = engine
            .mvcc_txn_manager()
            .active_id()
            .ok_or_else(|| "active txn id should be Some after BEGIN".to_string())?;
        engine.execute("INSERT INTO t VALUES (1)")?;
        engine.execute("DELETE FROM t WHERE id = 1")?;
        engine.execute("COMMIT")?;

        let table = engine
            .catalog()
            .get("t")
            .ok_or_else(|| "table t should exist".to_string())?;
        assert!(
            !table.row_versions.is_empty(),
            "row_versions should be populated by INSERT"
        );
        assert_eq!(
            table.row_versions[0].xmax,
            Some(txn_id),
            "DELETE should set xmax on the old version"
        );
        Ok(())
    }
}
