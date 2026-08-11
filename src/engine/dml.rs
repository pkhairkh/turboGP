//! DML execution — INSERT, UPDATE, DELETE.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_dml(
        &mut self,
        dml: crate::sql::DmlStatement,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        match dml {
            crate::sql::DmlStatement::Insert(ins) => self.execute_insert(ins, txn_id),
            crate::sql::DmlStatement::Update(upd) => self.execute_update(upd, txn_id),
            crate::sql::DmlStatement::Delete(del) => self.execute_delete(del, txn_id),
        }
    }

    // -----------------------------------------------------------------
    // Task 3.4 — FOREIGN KEY constraint enforcement.
    //
    // FK checks must run BEFORE the mutable column-extension / in-place
    // update / delete loop so a violation leaves the table unchanged
    // (atomicity). The child→parent existence check (INSERT/UPDATE) needs
    // an immutable borrow of both the child table (to read its schema +
    // column indices) and the parent table (to look up referenced rows);
    // these cannot coexist with a mutable borrow of the child table.
    // -----------------------------------------------------------------

    /// Validate that FOREIGN KEY constraints on `table_name` are satisfied
    /// by the proposed new rows (Task 3.4). Called from `execute_insert`
    /// and `execute_update` BEFORE the mutation loop.
    ///
    /// For each FK on the table's schema:
    /// 1. Resolve the FK column indices in the child table.
    /// 2. If any FK column value is NULL, skip (NULL FKs are allowed —
    ///    "no constraint", per SQL standard).
    /// 3. Look up the referenced (parent) table via `self.catalog.get`.
    /// 4. Check if the parent table has a row whose referenced columns
    ///    match the child's FK values.
    /// 5. If not, return `Err(Error::Other("23503: ..."))`.
    ///
    /// `new_rows` is a list of `(column_values, null_mask)` pairs — one
    /// per row to be inserted/updated. The null mask is `true` for NULL
    /// cells.
    fn validate_foreign_keys(
        &self,
        table_name: &str,
        new_rows: &[(Vec<u64>, Vec<bool>)],
    ) -> Result<()> {
        let child_table = match self.catalog.get(table_name) {
            Some(t) => t,
            None => return Ok(()), // table missing — caller will handle.
        };
        let Some(ref schema) = child_table.schema else {
            return Ok(());
        };
        if schema.foreign_keys.is_empty() {
            return Ok(());
        }
        for (row_idx, (vals, nulls)) in new_rows.iter().enumerate() {
            for fk in &schema.foreign_keys {
                // Resolve child column indices.
                let child_idxs: Vec<Option<usize>> =
                    fk.columns.iter().map(|name| child_table.column_idx(name)).collect();
                // Skip if any child column value is NULL.
                let any_null = child_idxs.iter().any(|idx| idx.map(|ci| nulls[ci]).unwrap_or(true));
                if any_null {
                    continue;
                }
                // Collect child values.
                let child_vals: Vec<u64> =
                    child_idxs.iter().map(|idx| idx.map(|ci| vals[ci]).unwrap_or(0)).collect();
                // Look up parent table.
                let parent_table = match self.catalog.get(&fk.ref_table) {
                    Some(t) => t,
                    None => {
                        return Err(Error::Other(format!(
                            "23503: FOREIGN KEY references non-existent table \"{}\"",
                            fk.ref_table
                        )))
                    }
                };
                // Resolve parent column indices.
                let parent_idxs: Vec<Option<usize>> =
                    fk.ref_columns.iter().map(|name| parent_table.column_idx(name)).collect();
                // Scan parent rows for a match.
                let mut found = false;
                'parent_row: for r in 0..parent_table.row_count {
                    for (i, &parent_ci) in parent_idxs.iter().enumerate() {
                        let Some(parent_ci) = parent_ci else { continue 'parent_row };
                        let existing = parent_table.columns[parent_ci].get(r).copied().unwrap_or(0);
                        if existing != child_vals[i] {
                            continue 'parent_row;
                        }
                    }
                    found = true;
                    break;
                }
                if !found {
                    return Err(Error::Other(format!(
                        "23503: FOREIGN KEY constraint violated: ({}) = ({}) references nonexistent row in table \"{}\" on row {}",
                        fk.columns.join(", "),
                        child_vals.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "),
                        fk.ref_table,
                        row_idx
                    )));
                }
            }
        }
        Ok(())
    }

    /// Task 3.4 — Enforce FK constraints when deleting rows from
    /// `parent_table_name` (the DELETE-side of FK enforcement).
    ///
    /// Scans every table in the catalog for FKs that reference
    /// `parent_table_name` (O(num_tables) — acceptable for now). For each
    /// such FK, applies the configured `ON DELETE` action:
    /// - **RESTRICT / NO ACTION** (default): if any child row references a
    ///   deleted parent row, return `Err(Error::Other("23504: ..."))`.
    /// - **CASCADE**: build a WHERE clause matching child rows that
    ///   reference deleted parent rows, then recursively call
    ///   `execute_dml(Delete)` on the child table. The recursive call
    ///   handles the child's own FK checks (grandchild CASCADE, etc.).
    /// - **SET NULL**: null the FK columns of child rows referencing
    ///   deleted parent rows. (**SET DEFAULT** is treated as SET NULL —
    ///   documented simplification.)
    ///
    /// `delete_mask` is a parallel-to-rows mask where `true` means the
    /// parent row will be deleted.
    fn enforce_fk_on_delete(
        &mut self,
        parent_table_name: &str,
        delete_mask: &[bool],
        txn_id: Option<u64>,
    ) -> Result<()> {
        // Snapshot all FK definitions that reference this parent table.
        // We collect owned clones so the catalog can be mutably borrowed
        // (for SET NULL) without holding an immutable borrow across the
        // mutation.
        let child_fks: Vec<(String, crate::sql::ddl::TableForeignKey)> = {
            let mut fks = Vec::new();
            let names: Vec<String> =
                self.catalog.table_names().into_iter().map(String::from).collect();
            for name in &names {
                if name == parent_table_name {
                    continue;
                }
                if let Some(t) = self.catalog.get(name) {
                    if let Some(ref schema) = t.schema {
                        for fk in &schema.foreign_keys {
                            if fk.ref_table == parent_table_name {
                                fks.push((name.clone(), fk.clone()));
                            }
                        }
                    }
                }
            }
            fks
        };
        if child_fks.is_empty() {
            return Ok(());
        }
        for (child_table_name, fk) in child_fks {
            // Collect the deleted parent rows' values in the FK's
            // referenced columns. These are the values we need to match
            // against child rows.
            let deleted_parent_vals: Vec<Vec<u64>> = {
                let parent_table = self
                    .catalog
                    .get(parent_table_name)
                    .ok_or_else(|| Error::NotFound(format!("table \"{}\"", parent_table_name)))?;
                let parent_idxs: Vec<Option<usize>> =
                    fk.ref_columns.iter().map(|name| parent_table.column_idx(name)).collect();
                let mut vals = Vec::new();
                for (row_idx, &deleted) in delete_mask.iter().enumerate() {
                    if !deleted {
                        continue;
                    }
                    let row_vals: Vec<u64> = parent_idxs
                        .iter()
                        .map(|idx| {
                            idx.and_then(|ci| parent_table.columns[ci].get(row_idx).copied())
                                .unwrap_or(0)
                        })
                        .collect();
                    vals.push(row_vals);
                }
                vals
            };
            let action = fk.on_delete.unwrap_or(crate::sql::ddl::ForeignKeyAction::NoAction);
            match action {
                crate::sql::ddl::ForeignKeyAction::Restrict
                | crate::sql::ddl::ForeignKeyAction::NoAction => {
                    // Check if any child row references a deleted parent row.
                    let child_table = self.catalog.get(&child_table_name).ok_or_else(|| {
                        Error::NotFound(format!("table \"{}\"", child_table_name))
                    })?;
                    let child_idxs: Vec<Option<usize>> =
                        fk.columns.iter().map(|name| child_table.column_idx(name)).collect();
                    for child_row_idx in 0..child_table.row_count {
                        // Skip if any child FK column is NULL.
                        let any_null = child_idxs.iter().any(|idx| {
                            idx.and_then(|ci| {
                                child_table
                                    .null_bitmaps
                                    .get(ci)
                                    .and_then(|bm| bm.as_ref().map(|b| b.is_null(child_row_idx)))
                            })
                            .unwrap_or(false)
                        });
                        if any_null {
                            continue;
                        }
                        let child_vals: Vec<u64> = child_idxs
                            .iter()
                            .map(|idx| {
                                idx.and_then(|ci| {
                                    child_table.columns[ci].get(child_row_idx).copied()
                                })
                                .unwrap_or(0)
                            })
                            .collect();
                        for parent_vals in &deleted_parent_vals {
                            if child_vals == *parent_vals {
                                return Err(Error::Other(format!(
                                    "23504: FOREIGN KEY constraint violated: cannot delete from table \"{}\" — row referenced by table \"{}\" ({})",
                                    parent_table_name,
                                    child_table_name,
                                    fk.columns.join(", ")
                                )));
                            }
                        }
                    }
                }
                crate::sql::ddl::ForeignKeyAction::Cascade => {
                    // Build a WHERE clause matching child rows that
                    // reference deleted parent rows, then recursively
                    // execute the delete on the child table. The
                    // recursive call handles the child's own FK checks.
                    let where_clause = build_cascade_where_expr(&fk.columns, &deleted_parent_vals);
                    let child_del = crate::sql::Delete {
                        table: child_table_name.clone(),
                        where_clause,
                        returning: None,
                    };
                    // Note: this recursive execute_dml call does NOT
                    // append to the WAL (only execute_inner does). The
                    // parent's WAL record (written by execute_inner
                    // after execute_dml returns) will re-trigger the
                    // CASCADE on replay, so committed CASCADE deletes
                    // are durable. A crash DURING the CASCADE (after
                    // the child delete but before the parent's WAL
                    // record is written) loses both deletes — the
                    // in-memory state is gone and the WAL has no
                    // record to replay. This is a known limitation
                    // (documented).
                    self.execute_dml(crate::sql::DmlStatement::Delete(child_del), txn_id)?;
                }
                crate::sql::ddl::ForeignKeyAction::SetNull
                | crate::sql::ddl::ForeignKeyAction::SetDefault => {
                    // SET NULL: null the FK columns of child rows
                    // referencing deleted parent rows.
                    // SET DEFAULT: treated as SET NULL (simplification —
                    // a proper implementation would set the column to its
                    // DEFAULT value, but DEFAULT-value resolution at DML
                    // time is not yet wired for all column types).
                    self.catalog
                        .with_mut(&child_table_name, |child_table| {
                            let child_idxs: Vec<Option<usize>> = fk
                                .columns
                                .iter()
                                .map(|name| child_table.column_idx(name))
                                .collect();
                            // Ensure null bitmaps exist for the FK columns.
                            for idx in &child_idxs {
                                if let Some(ci) = idx {
                                    while child_table.null_bitmaps.len() <= *ci {
                                        child_table.null_bitmaps.push(None);
                                    }
                                    if child_table.null_bitmaps[*ci].is_none() {
                                        let mut bm = crate::types::null_bitmap::NullBitmap::new(
                                            child_table.row_count,
                                        );
                                        for _ in 0..child_table.row_count {
                                            bm.push_non_null();
                                        }
                                        child_table.null_bitmaps[*ci] = Some(bm);
                                    }
                                }
                            }
                            for child_row_idx in 0..child_table.row_count {
                                let child_vals: Vec<u64> = child_idxs
                                    .iter()
                                    .map(|idx| {
                                        idx.and_then(|ci| {
                                            child_table.columns[ci].get(child_row_idx).copied()
                                        })
                                        .unwrap_or(0)
                                    })
                                    .collect();
                                let mut matches = false;
                                for parent_vals in &deleted_parent_vals {
                                    if child_vals == *parent_vals {
                                        matches = true;
                                        break;
                                    }
                                }
                                if matches {
                                    for idx in &child_idxs {
                                        if let Some(ci) = idx {
                                            let col = std::sync::Arc::make_mut(
                                                &mut child_table.columns[*ci],
                                            );
                                            col[child_row_idx] = 0;
                                            if let Some(ref mut bm) = child_table.null_bitmaps[*ci]
                                            {
                                                while bm.len() <= child_row_idx {
                                                    bm.push_non_null();
                                                }
                                                bm.set_null(child_row_idx);
                                            }
                                        }
                                    }
                                }
                            }
                        })
                        .ok_or_else(|| {
                            Error::NotFound(format!("table \"{}\"", child_table_name))
                        })?;
                }
            }
        }
        Ok(())
    }

    /// Execute an INSERT statement.
    ///
    /// Wave 56c fix: when inserting a string literal into a VARCHAR / NVARCHAR
    /// / TEXT column, the original string is now preserved in the column's
    /// `string_columns` sidecar (`StringSearchColumn`). Previously, the string
    /// was hashed to a u64 (via `parse_value_cell`) and the original was lost —
    /// so subsequent `SELECT col` could only return the hash, and JSON_VALUE
    /// / LIKE / range comparisons on inserted strings were broken.
    pub(crate) fn execute_insert(
        &mut self,
        ins: crate::sql::Insert,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Task 3.4 — Validate FOREIGN KEY constraints (child → parent
        // existence) BEFORE the mutable borrow of the child table extends
        // into the column-extension loop. We need immutable borrows of both
        // the child table (to read its schema + column indices) and the
        // parent table (to look up referenced rows); these cannot coexist
        // with a mutable borrow of the child table.
        {
            let fk_rows: Vec<(Vec<u64>, Vec<bool>)> = {
                let child_table = match self.catalog.get(&ins.table) {
                    Some(t) => t,
                    None => {
                        return Err(Error::NotFound(format!("table \"{}\"", ins.table)));
                    }
                };
                let needs_fk_check = child_table
                    .schema
                    .as_ref()
                    .map(|s| !s.foreign_keys.is_empty())
                    .unwrap_or(false);
                if !needs_fk_check {
                    Vec::new()
                } else {
                    let col_indices: Vec<usize> = match &ins.columns {
                        Some(cols) => {
                            let mut idxs = Vec::with_capacity(cols.len());
                            for col_name in cols {
                                let idx = child_table.column_idx(col_name).ok_or_else(|| {
                                    Error::NotFound(format!("column \"{col_name}\""))
                                })?;
                                idxs.push(idx);
                            }
                            idxs
                        }
                        None => (0..child_table.columns.len()).collect(),
                    };
                    if col_indices.len() != ins.values.first().map(|r| r.len()).unwrap_or(0) {
                        return Err(Error::Other(format!(
                            "column count ({}) doesn't match value count ({})",
                            col_indices.len(),
                            ins.values.first().map(|r| r.len()).unwrap_or(0)
                        )));
                    }
                    let ncols = child_table.columns.len();
                    ins.values
                        .iter()
                        .map(|row_vals| {
                            let mut vals = vec![0u64; ncols];
                            let mut nulls = vec![true; ncols];
                            for (i, &col_idx) in col_indices.iter().enumerate() {
                                let val_str = &row_vals[i];
                                let is_null = val_str.trim().eq_ignore_ascii_case("null");
                                vals[col_idx] = parse_value_cell(val_str);
                                nulls[col_idx] = is_null;
                            }
                            (vals, nulls)
                        })
                        .collect()
                }
            };
            if !fk_rows.is_empty() {
                self.validate_foreign_keys(&ins.table, &fk_rows)?;
            }
        }

        let table_name = ins.table.clone();
        let n_new_rows = ins.values.len();

        let temporal_rows: Vec<Vec<u64>> = self
            .catalog
            .with_mut(&ins.table, |table| -> Result<Vec<Vec<u64>>> {
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

        // Task 3.2 + 3.5 — Enforce UNIQUE and CHECK constraints at INSERT time.
        // We do this BEFORE the column-extension loop so we can bail out
        // cleanly without leaving partial inserts. The new row's u64 values
        // are built from `ins.values` (parsed via `parse_value_cell`); the
        // null mask records which columns were inserted as NULL (so CHECK
        // constraints become UNKNOWN → pass, per SQL standard).
        if let Some(ref schema) = table.schema {
            let ncols = table.columns.len();
            let column_names: &[String] = &table.column_names;
            for (row_idx, row_vals) in ins.values.iter().enumerate() {
                // Build the new row's u64 values + null mask.
                let mut new_row_values: Vec<u64> = vec![0u64; ncols];
                let mut new_row_nulls: Vec<bool> = vec![true; ncols];
                for (i, &col_idx) in col_indices.iter().enumerate() {
                    let val_str = &row_vals[i];
                    let is_null = val_str.trim().eq_ignore_ascii_case("null");
                    new_row_values[col_idx] = parse_value_cell(val_str);
                    new_row_nulls[col_idx] = is_null;
                }

                // 3.5: Evaluate column-level CHECK constraints.
                for col_schema in schema.columns.iter() {
                    if let Some(ref check_expr) = col_schema.check {
                        if !eval_check_expr(check_expr, column_names, &new_row_values, &new_row_nulls)
                        {
                            return Err(Error::Other(format!(
                                "23514: CHECK constraint violated for column \"{}\" on row {}",
                                col_schema.name, row_idx
                            )));
                        }
                    }
                }
                // 3.5: Evaluate table-level CHECK constraints.
                for check_expr in &schema.checks {
                    if !eval_check_expr(check_expr, column_names, &new_row_values, &new_row_nulls) {
                        return Err(Error::Other(format!(
                            "23514: CHECK constraint violated on row {}",
                            row_idx
                        )));
                    }
                }

                // 3.2: Check column-level UNIQUE constraints.
                // NULL values are skipped (NULLs are distinct, per SQL).
                for (col_idx, col_schema) in schema.columns.iter().enumerate() {
                    if col_schema.unique && !new_row_nulls[col_idx] {
                        let new_cell = new_row_values[col_idx];
                        let col = &table.columns[col_idx];
                        if col.iter().any(|&existing| existing == new_cell) {
                            // Verify the match isn't a NULL cell (which
                            // happens to be stored as 0 and could collide
                            // with a real zero value). We skip NULLs in
                            // the existing data via the null bitmap.
                            let conflict_idx = col.iter().position(|&existing| existing == new_cell);
                            if let Some(ci) = conflict_idx {
                                let existing_is_null = col_idx < table.null_bitmaps.len()
                                    && table.null_bitmaps[col_idx]
                                        .as_ref()
                                        .map(|bm| bm.is_null(ci))
                                        .unwrap_or(false);
                                if !existing_is_null {
                                    return Err(Error::Other(format!(
                                        "23505: UNIQUE constraint violated for column \"{}\" on row {}",
                                        col_schema.name, row_idx
                                    )));
                                }
                            }
                        }
                    }
                }

                // 3.2: Check table-level (multi-column) UNIQUE constraints.
                // If any column in the combination is NULL, skip (NULLs are
                // distinct, even in a multi-column UNIQUE).
                for cols in &schema.unique_constraints {
                    let new_combo: Vec<(u64, bool)> = cols
                        .iter()
                        .map(|name| {
                            let idx = column_names.iter().position(|c| c == name);
                            match idx {
                                Some(i) => (new_row_values[i], new_row_nulls[i]),
                                None => (0, true), // unknown column → treat as NULL
                            }
                        })
                        .collect();
                    if new_combo.iter().any(|&(_, is_null)| is_null) {
                        continue;
                    }
                    // Scan existing rows for a duplicate combination.
                    let n = table.row_count;
                    'outer: for r in 0..n {
                        for (combo_i, name) in cols.iter().enumerate() {
                            let idx = match column_names.iter().position(|c| c == name) {
                                Some(i) => i,
                                None => continue 'outer,
                            };
                            let existing = table.columns[idx].get(r).copied().unwrap_or(0);
                            if existing != new_combo[combo_i].0 {
                                continue 'outer;
                            }
                        }
                        return Err(Error::Other(format!(
                            "23505: UNIQUE constraint violated for columns ({}) on row {}",
                            cols.join(", "),
                            row_idx
                        )));
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

        // Task 3.1 (debt-4.2): populate Table.row_versions when MVCC mode
        // is enabled. Each inserted row gets a RowVersion with xmin = txn_id
        // (or 0 for autocommit) and xmax = None (still live). The new rows
        // live at indices [old_row_count, old_row_count + n_new_rows); since
        // `row_count` was already incremented above, that range is
        // [row_count - n_new_rows, row_count).
        if self.mvcc_enabled {
            let xmin = txn_id.unwrap_or(0);
            let first_new_row = table.row_count - n_new_rows;
            for i in 0..n_new_rows {
                let row_idx = first_new_row + i;
                table.append_row_version(
                    row_idx,
                    crate::txn::mvcc::RowVersion::new(xmin, Vec::new()),
                );
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

                Ok(temporal_rows)
            })
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", ins.table)))??;

        // Now release the table borrow and update the temporal sidecar.
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
    pub(crate) fn execute_update(
        &mut self,
        upd: crate::sql::Update,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Task 3.4 — Validate FOREIGN KEY constraints at UPDATE time.
        // Build the post-update row values for each matched row and check
        // that FK columns still reference an existing parent row. Done
        // BEFORE the mutable borrow extends into the in-place update loop
        // (atomicity: a violation leaves the table unchanged).
        {
            let fk_rows: Vec<(Vec<u64>, Vec<bool>)> = {
                let child_table = match self.catalog.get(&upd.table) {
                    Some(t) => t,
                    None => {
                        return Err(Error::NotFound(format!("table \"{}\"", upd.table)));
                    }
                };
                let needs_fk_check = child_table
                    .schema
                    .as_ref()
                    .map(|s| !s.foreign_keys.is_empty())
                    .unwrap_or(false);
                if !needs_fk_check {
                    Vec::new()
                } else {
                    // Parse assignments (mirror the mutable-phase parsing).
                    let assigns: Vec<(usize, u64, bool)> = {
                        let mut a = Vec::with_capacity(upd.assignments.len());
                        for (col_name, expr) in &upd.assignments {
                            let idx = child_table
                                .column_idx(col_name)
                                .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
                            let expr_str = expr.to_string();
                            let trimmed = expr_str.trim();
                            let is_null = trimmed.eq_ignore_ascii_case("NULL")
                                || matches!(
                                    expr,
                                    crate::sql::ast::Expr::Literal(crate::sql::ast::Value::Null)
                                );
                            let cell = parse_value_cell(&expr_str);
                            a.push((idx, cell, is_null));
                        }
                        a
                    };
                    // Compute match_mask.
                    let n = child_table.row_count;
                    let match_mask: Vec<bool> = if let Some(where_expr) = &upd.where_clause {
                        let where_str = where_expr.to_string();
                        eval_simple_where(&child_table, &where_str)?
                    } else {
                        vec![true; n]
                    };
                    // Build post-update rows for matched rows.
                    let ncols = child_table.columns.len();
                    let mut rows = Vec::new();
                    for (row_idx, &matches) in match_mask.iter().enumerate() {
                        if !matches {
                            continue;
                        }
                        let mut vals: Vec<u64> = (0..ncols)
                            .map(|ci| child_table.columns[ci].get(row_idx).copied().unwrap_or(0))
                            .collect();
                        let mut nulls: Vec<bool> = (0..ncols)
                            .map(|ci| {
                                if ci < child_table.null_bitmaps.len() {
                                    if let Some(ref bm) = child_table.null_bitmaps[ci] {
                                        return bm.is_null(row_idx);
                                    }
                                }
                                false
                            })
                            .collect();
                        for &(col_idx, val, is_null) in &assigns {
                            if col_idx < vals.len() {
                                vals[col_idx] = val;
                                nulls[col_idx] = is_null;
                            }
                        }
                        rows.push((vals, nulls));
                    }
                    rows
                }
            };
            if !fk_rows.is_empty() {
                self.validate_foreign_keys(&upd.table, &fk_rows)?;
            }
        }

        let table_name = upd.table.clone();

        let (updated, updates, is_temporal): (usize, Vec<(u64, Vec<u64>)>, bool) = self
            .catalog
            .with_mut(&upd.table, |table| -> Result<(usize, Vec<(u64, Vec<u64>)>, bool)> {
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

        // Task 3.3 + 3.5 — Enforce UNIQUE and CHECK constraints at UPDATE time.
        // We check BEFORE applying the in-place update to each row so we can
        // bail out cleanly without leaving partial updates (atomicity). All
        // matching rows are checked; if any would violate a constraint, the
        // entire UPDATE fails and no rows are modified.
        //
        // Known limitation: if two rows in the same UPDATE are both updated
        // to the same new value for a UNIQUE column, this check evaluates
        // against the pre-UPDATE data and may miss the intra-statement
        // conflict. This is rare (the WHERE clause would have to match
        // both rows); a future wave could check against the post-UPDATE
        // snapshot of all matched rows.
        {
            let schema_ref = table.schema.clone();
            let column_names_ref = table.column_names.clone();
            let ncols = table.columns.len();
            if let Some(ref schema) = schema_ref {
                for (row_idx, &matches) in match_mask.iter().enumerate() {
                    if !matches {
                        continue;
                    }
                    // Build the post-update row values + null mask for this row.
                    let mut new_row_values: Vec<u64> = (0..ncols)
                        .map(|ci| table.columns[ci].get(row_idx).copied().unwrap_or(0))
                        .collect();
                    let mut new_row_nulls: Vec<bool> = (0..ncols)
                        .map(|ci| {
                            if ci < table.null_bitmaps.len() {
                                if let Some(ref bm) = table.null_bitmaps[ci] {
                                    return bm.is_null(row_idx);
                                }
                            }
                            false
                        })
                        .collect();
                    for &(col_idx, val, is_null) in &assigns {
                        if col_idx < new_row_values.len() {
                            new_row_values[col_idx] = val;
                            new_row_nulls[col_idx] = is_null;
                        }
                    }

                    // 3.5: Evaluate column-level CHECK constraints against
                    // the post-update row.
                    for col_schema in schema.columns.iter() {
                        if let Some(ref check_expr) = col_schema.check {
                            if !eval_check_expr(
                                check_expr,
                                &column_names_ref,
                                &new_row_values,
                                &new_row_nulls,
                            ) {
                                return Err(Error::Other(format!(
                                    "23514: CHECK constraint violated for column \"{}\" on row {}",
                                    col_schema.name, row_idx
                                )));
                            }
                        }
                    }
                    // 3.5: Evaluate table-level CHECK constraints.
                    for check_expr in &schema.checks {
                        if !eval_check_expr(
                            check_expr,
                            &column_names_ref,
                            &new_row_values,
                            &new_row_nulls,
                        ) {
                            return Err(Error::Other(format!(
                                "23514: CHECK constraint violated on row {}",
                                row_idx
                            )));
                        }
                    }

                    // 3.3: Check column-level UNIQUE constraints. For each
                    // updated row, scan existing rows (excluding self) for
                    // a duplicate of the new value. NULLs are skipped.
                    for (col_idx, col_schema) in schema.columns.iter().enumerate() {
                        if col_schema.unique && !new_row_nulls[col_idx] {
                            let new_cell = new_row_values[col_idx];
                            let col = &table.columns[col_idx];
                            for (other_idx, &existing) in col.iter().enumerate() {
                                if other_idx == row_idx {
                                    continue;
                                }
                                if existing == new_cell {
                                    // Skip if the other row's cell is NULL
                                    // (NULLs are distinct).
                                    let existing_is_null = col_idx < table.null_bitmaps.len()
                                        && table.null_bitmaps[col_idx]
                                            .as_ref()
                                            .map(|bm| bm.is_null(other_idx))
                                            .unwrap_or(false);
                                    if !existing_is_null {
                                        return Err(Error::Other(format!(
                                            "23505: UNIQUE constraint violated for column \"{}\" on row {}",
                                            col_schema.name, row_idx
                                        )));
                                    }
                                }
                            }
                        }
                    }

                    // 3.3: Check table-level (multi-column) UNIQUE constraints.
                    for cols in &schema.unique_constraints {
                        let new_combo: Vec<(u64, bool)> = cols
                            .iter()
                            .map(|name| {
                                let idx = column_names_ref.iter().position(|c| c == name);
                                match idx {
                                    Some(i) => (new_row_values[i], new_row_nulls[i]),
                                    None => (0, true),
                                }
                            })
                            .collect();
                        if new_combo.iter().any(|&(_, is_null)| is_null) {
                            continue;
                        }
                        let n_existing = table.row_count;
                        'combo_outer: for r in 0..n_existing {
                            if r == row_idx {
                                continue;
                            }
                            for (combo_i, name) in cols.iter().enumerate() {
                                let idx = match column_names_ref.iter().position(|c| c == name) {
                                    Some(i) => i,
                                    None => continue 'combo_outer,
                                };
                                let existing = table.columns[idx].get(r).copied().unwrap_or(0);
                                if existing != new_combo[combo_i].0 {
                                    continue 'combo_outer;
                                }
                            }
                            return Err(Error::Other(format!(
                                "23505: UNIQUE constraint violated for columns ({}) on row {}",
                                cols.join(", "),
                                row_idx
                            )));
                        }
                    }
                }
            }
        }

        // Task 3.5 — Serializable write-write conflict detection.
        //
        // When MVCC is enabled AND the active transaction's isolation level
        // is `Serializable`, verify that no concurrent committed transaction
        // has modified any matched row (first-committer-wins). This runs
        // BEFORE the in-place column updates so a conflict leaves the table
        // unchanged (atomicity). RepeatableRead skips this check — per the
        // SQL standard, RR permits lost updates (the conflict would surface
        // as a stale read, not an abort).
        //
        // The check looks at the latest version VISIBLE TO the active txn
        // and errors if its `xmax` was set by a transaction that committed
        // AFTER the active txn's snapshot. See
        // [`MvccTxnManager::check_write_conflict_for_table`] for the full
        // rule.
        if self.mvcc_enabled
            && self.mvcc_txn_manager.active_isolation_level()
                == Some(crate::txn::IsolationLevel::Serializable)
        {
            let active_txn_id = txn_id.unwrap_or(0);
            let active_snapshot_id = self
                .mvcc_txn_manager
                .active_snapshot_id()
                .unwrap_or_else(|| self.mvcc_txn_manager.current_commit_id());
            for (row_idx, &matches) in match_mask.iter().enumerate() {
                if !matches {
                    continue;
                }
                self.mvcc_txn_manager
                    .check_write_conflict_for_table(
                        table,
                        active_txn_id,
                        active_snapshot_id,
                        row_idx,
                    )
                    .map_err(|e| Error::Other(e.message))?;
            }
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

        // Task 3.1 (debt-4.2): populate Table.row_versions when MVCC mode
        // is enabled. For each matched row, the old version is tombstoned
        // (xmax = txn_id) via `mark_deleted`, and a new RowVersion carrying
        // the post-update column values is APPENDED TO THE SAME CHAIN at
        // `row_idx`. This is the key Task 3.1 fix: previously UPDATE
        // appended to the END of the flat `row_versions` vec, which broke
        // the row-index alignment and made the new version invisible to
        // the updating transaction.
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
                // Tombstone the old version (sets xmax on the latest
                // version in the chain at `row_idx`). If the row had no
                // version yet (e.g. table loaded without MVCC tracking),
                // `mark_deleted` returns false and we skip the append — the
                // absence of a prior version means there is nothing to
                // supersede.
                let marked = table.mark_deleted(row_idx, xmin);
                if marked {
                    table.append_row_version(
                        row_idx,
                        crate::txn::mvcc::RowVersion::new(xmin, new_values),
                    );
                }
            }
        }

        // Wave 56d: if this is a temporal table, collect the matched row
        // indices and new values for the temporal sidecar. We apply the
        // updates AFTER releasing the table borrow (below).
        let is_temporal = self.temporals.contains_key(&table_name);
        let mut updates: Vec<(u64, Vec<u64>)> = Vec::new();
        if is_temporal {
            // Collect (predicate_fn, new_values) for the temporal update.
            // The predicate matches any row whose first column value equals
            // the matched row's first column value (best-effort — the
            // TemporalTable's update() takes a closure, so we match by PK).
            // We build a list of (old_pk, new_row_values) pairs.
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
        }
                Ok((updated, updates, is_temporal))
            })
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", upd.table)))??;

        // Apply the temporal updates now that the table borrow is released.
        if is_temporal {
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
    pub(crate) fn execute_delete(
        &mut self,
        del: crate::sql::Delete,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Task 3.4 — Enforce FK constraints on DELETE.
        // Compute the delete_mask with an immutable borrow, then run the
        // FK enforcement (RESTRICT/CASCADE/SET NULL) BEFORE the mutable
        // borrow extends into the column-rebuild / MVCC-tombstone path.
        // This keeps the parent table intact for the FK checks (we need
        // to read the parent's referenced column values for the rows
        // being deleted).
        let delete_mask: Vec<bool> = {
            let table = self
                .catalog
                .get(&del.table)
                .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;
            let n = table.row_count;
            if let Some(where_expr) = &del.where_clause {
                let where_str = where_expr.to_string();
                eval_simple_where(&table, &where_str)?
            } else {
                vec![true; n]
            }
        };
        let deleted = delete_mask.iter().filter(|&&b| b).count();
        if deleted == 0 {
            let mut result = QueryResult::empty();
            result.row_count = 0;
            return Ok(result);
        }
        // Enforce FK constraints (RESTRICT → error; CASCADE → recurse;
        // SET NULL → null FK columns).
        self.enforce_fk_on_delete(&del.table, &delete_mask, txn_id)?;

        let table_name = del.table.clone();
        let n = self
            .catalog
            .with(&del.table, |t| t.row_count)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

        // Task 2.3 (debt-4.2): in MVCC mode, tombstone the matched rows'
        // versions (xmax = txn_id) and leave the column data in place for
        // VACUUM to reclaim later. We do NOT rebuild the columns or
        // decrement `row_count` here — that's the VACUUM path's job. The
        // `row_versions` chain vec stays aligned with `columns` (both
        // reflect the original row indices, with tombstones marking
        // deleted rows).
        //
        // Task 3.1: `mark_deleted` now sets xmax on the LAST version in
        // the chain at `row_idx` (rather than on `row_versions[row_idx]`
        // directly). The semantics for DELETE are unchanged.
        //
        // The temporal sidecar (if any) still receives a logical delete
        // so FOR SYSTEM_TIME queries see the row's end-time, but its
        // column rebuild is skipped to preserve the row-version alignment.
        if self.mvcc_enabled {
            // Task 3.5 — Serializable write-write conflict detection.
            //
            // Before tombstoning any matched row, verify no concurrent
            // committed transaction has modified it (first-committer-wins).
            // Gated on Serializable isolation; RepeatableRead skips this.
            // The check runs BEFORE the tombstoning loop so a conflict
            // leaves all matched rows untouched (atomicity). On conflict,
            // we return the error immediately — the temporal sidecar sync
            // and column rebuild below are skipped.
            if self.mvcc_txn_manager.active_isolation_level()
                == Some(crate::txn::IsolationLevel::Serializable)
            {
                let active_txn_id = txn_id.unwrap_or(0);
                let active_snapshot_id = self
                    .mvcc_txn_manager
                    .active_snapshot_id()
                    .unwrap_or_else(|| self.mvcc_txn_manager.current_commit_id());
                // Read the table under a scoped read lock to run the
                // conflict checks. The tombstoning loop below takes its
                // own write lock; splitting the two avoids holding the
                // write lock during the (read-only) conflict scan.
                //
                // `catalog.with` returns `Option<R>` (None if the table
                // is missing). Our closure returns `Option<ConflictError>`
                // (None = no conflict, Some = conflict). We flatten the
                // outer Option: a missing table is an error we surface
                // as NotFound; otherwise we inspect the inner Option.
                let conflict_result: Option<Option<crate::txn::ConflictError>> =
                    self.catalog.with(&del.table, |table| {
                        for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                            if !delete_flag {
                                continue;
                            }
                            if let Err(e) = self.mvcc_txn_manager
                                .check_write_conflict_for_table(
                                    table,
                                    active_txn_id,
                                    active_snapshot_id,
                                    row_idx,
                                )
                            {
                                return Some(e);
                            }
                        }
                        None
                    });
                let conflict_err: Option<crate::txn::ConflictError> = match conflict_result {
                    Some(inner) => inner,
                    None => {
                        return Err(Error::NotFound(format!("table \"{}\"", del.table)));
                    }
                };
                if let Some(e) = conflict_err {
                    return Err(Error::Other(e.message));
                }
            }

            // Tombstone the deleted rows under a scoped write lock.
            self.catalog
                .with_mut(&del.table, |table| {
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
                })
                .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

            // Sync the temporal sidecar (if any) WITHOUT rebuilding columns.
            // The table write lock is released, so we can mutably borrow
            // self.temporals.
            let is_temporal = self.temporals.contains_key(&table_name);
            if is_temporal {
                let pks_to_delete: Vec<u64> = self
                    .catalog
                    .with(&del.table, |table| {
                        let mut pks = Vec::new();
                        for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                            if delete_flag {
                                let pk = table
                                    .columns
                                    .first()
                                    .and_then(|c| c.get(row_idx).copied())
                                    .unwrap_or(0);
                                pks.push(pk);
                            }
                        }
                        pks
                    })
                    .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;
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
        let is_temporal = self.temporals.contains_key(&table_name);
        if is_temporal {
            // Collect the PKs of rows to delete (first column value) under a
            // read lock, then release before mutating self.temporals.
            let pks_to_delete: Vec<u64> = self
                .catalog
                .with(&del.table, |table| {
                    let mut pks = Vec::new();
                    for (row_idx, &delete_flag) in delete_mask.iter().enumerate() {
                        if delete_flag {
                            let pk = table
                                .columns
                                .first()
                                .and_then(|c| c.get(row_idx).copied())
                                .unwrap_or(0);
                            pks.push(pk);
                        }
                    }
                    pks
                })
                .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;
            if let Some(temporal) = self.temporals.get_mut(&table_name) {
                for pk in pks_to_delete {
                    temporal.delete(|row| row.first().copied() == Some(pk));
                }
            }
        }

        // Rebuild each column keeping only non-deleted rows. This runs for
        // both temporal and non-temporal tables (the temporal branch above
        // has already synced its sidecar using the pre-rebuild PKs).
        self.catalog
            .with_mut(&del.table, |table| {
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
            })
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

        let mut result = QueryResult::empty();
        result.row_count = deleted;
        Ok(result)
    }

    /// Test-only: begin a background MVCC transaction with a specific
    /// isolation level (Task 3.5).
    ///
    /// Like [`begin_background_txn`](Self::begin_background_txn) but allows
    /// specifying the isolation level. Used by the Serializable conflict
    /// detection integration test to start T1/T2 as `Serializable` (the
    /// engine's `BEGIN` SQL always uses the default `RepeatableRead`).
    ///
    /// Returns the new transaction's ID. The previously-active transaction
    /// (if any) remains in the manager's `txn_states` map as `InProgress`;
    /// `current_active` is overwritten to the new txn.
    #[doc(hidden)]
    #[must_use]
    pub fn begin_background_txn_with_isolation(
        &mut self,
        level: crate::txn::IsolationLevel,
    ) -> u64 {
        self.mvcc_txn_manager.begin_with_isolation(level).id
    }
}

// -----------------------------------------------------------------
// Task 3.4 — CASCADE WHERE-clause builder.
//
// Constructs an `Expr` matching child rows whose FK columns equal one
// of the deleted parent rows' values. The resulting expression has the
// shape:
//   (col1 = v1a AND col2 = v2a) OR (col1 = v1b AND col2 = v2b) OR ...
// which `eval_simple_where` can evaluate (it skips parens).
// -----------------------------------------------------------------

/// Build a WHERE-clause `Expr` for CASCADE deletes: matches any child
/// row whose FK columns equal one of the `deleted_rows` value tuples.
///
/// Returns `None` if `deleted_rows` is empty (no rows to match → the
/// caller should issue an unconditional delete or skip the recursive
/// call).
fn build_cascade_where_expr(
    fk_cols: &[String],
    deleted_rows: &[Vec<u64>],
) -> Option<crate::sql::ast::Expr> {
    use crate::sql::ast::{BinOp, Expr, Value};

    if deleted_rows.is_empty() {
        return None;
    }
    let mut per_row_exprs: Vec<Expr> = Vec::new();
    for row in deleted_rows {
        let col_eqs: Vec<Expr> = fk_cols
            .iter()
            .enumerate()
            .map(|(i, col_name)| {
                let val = row.get(i).copied().unwrap_or(0);
                Expr::Binary {
                    left: Box::new(Expr::Column(col_name.clone())),
                    op: BinOp::Eq,
                    right: Box::new(Expr::Literal(Value::Int(val as i64))),
                }
            })
            .collect();
        let row_expr = col_eqs.into_iter().reduce(|a, b| Expr::Binary {
            left: Box::new(a),
            op: BinOp::And,
            right: Box::new(b),
        });
        if let Some(e) = row_expr {
            per_row_exprs.push(e);
        }
    }
    per_row_exprs.into_iter().reduce(|a, b| Expr::Binary {
        left: Box::new(a),
        op: BinOp::Or,
        right: Box::new(b),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 2.2 — `execute_update` sets `xmax` on the old version and
    /// appends a new `RowVersion` with the post-update column values when
    /// MVCC mode is enabled.
    ///
    /// Task 3.1: the new version is appended to the SAME chain at `row_idx`
    /// (not to the end of the flat `row_versions` vec), so the row-index
    /// alignment is preserved.
    ///
    /// Sequence: `BEGIN; INSERT (1,10); UPDATE SET v=99 WHERE id=1; COMMIT;`
    /// Expected:
    /// - `row_versions[0]` is a chain of length 2 (old + new version).
    /// - `row_versions[0][0].xmax == Some(txn_id)` (old version tombstoned).
    /// - `row_versions[0][1].xmin == txn_id`, `xmax == None`, and its
    ///   `values` contain `99` (the updated `v`).
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

        let table = engine.catalog().get("t").ok_or_else(|| "table t should exist".to_string())?;
        assert!(!table.row_versions.is_empty(), "row_versions should be populated by INSERT");

        // Row 0's chain should have 2 versions after INSERT + UPDATE.
        let chain = &table.row_versions[0];
        assert_eq!(
            chain.len(),
            2,
            "row 0 should have 2 versions after INSERT+UPDATE; got {}",
            chain.len()
        );

        // The old version (chain[0]) is tombstoned.
        assert_eq!(
            chain[0].xmax,
            Some(txn_id),
            "UPDATE should set xmax on the old version"
        );

        // The new version (chain[1]) is live and carries the updated value.
        let new_version = &chain[1];
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
    /// Task 3.1: `mark_deleted` now sets xmax on the LAST version in the
    /// chain at `row_idx`.
    ///
    /// Sequence: `BEGIN; INSERT (1); DELETE WHERE id=1; COMMIT;`
    /// Expected: `row_versions[0].last().xmax == Some(txn_id)`.
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

        let table = engine.catalog().get("t").ok_or_else(|| "table t should exist".to_string())?;
        assert!(!table.row_versions.is_empty(), "row_versions should be populated by INSERT");
        let chain = &table.row_versions[0];
        assert!(!chain.is_empty(), "row 0 should have at least one version");
        assert_eq!(
            chain.last().unwrap().xmax,
            Some(txn_id),
            "DELETE should set xmax on the latest version in the chain"
        );
        Ok(())
    }

    // -----------------------------------------------------------------
    // Task 3.2 + 3.3 + 3.5 — UNIQUE and CHECK constraint enforcement.
    // -----------------------------------------------------------------

    /// Task 3.2 — INSERT a duplicate value into a UNIQUE column fails with
    /// error 23505.
    #[test]
    fn test_unique_violation_at_insert() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT, email VARCHAR UNIQUE)")?;
        engine.execute("INSERT INTO t VALUES (1, 'a')")?;
        match engine.execute("INSERT INTO t VALUES (2, 'a')") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("23505") && msg.contains("email"),
                    "expected 23505 error mentioning column 'email', got: {msg}"
                );
            }
            Ok(_) => return Err("duplicate UNIQUE insert should have failed".into()),
        }
        Ok(())
    }

    /// Task 3.2 — NULL values are exempt from UNIQUE (NULLs are distinct,
    /// per SQL standard). Multiple NULLs in a UNIQUE column are allowed.
    #[test]
    fn test_unique_null_allowed() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT, email VARCHAR UNIQUE)")?;
        engine.execute("INSERT INTO t VALUES (1, NULL)")?;
        // A second NULL should not be a duplicate.
        engine.execute("INSERT INTO t VALUES (2, NULL)")?;
        let r = engine.execute("SELECT count(*) FROM t")?;
        assert_eq!(r.scalar_u64(), Some(2));
        Ok(())
    }

    /// Task 3.3 — UPDATE that would create a UNIQUE conflict fails with
    /// error 23505. The row is left unchanged (no partial update).
    #[test]
    fn test_unique_violation_at_update() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT, email VARCHAR UNIQUE)")?;
        engine.execute("INSERT INTO t VALUES (1, 'a')")?;
        engine.execute("INSERT INTO t VALUES (2, 'b')")?;
        match engine.execute("UPDATE t SET email = 'a' WHERE id = 2") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("23505") && msg.contains("email"),
                    "expected 23505 error mentioning 'email', got: {msg}"
                );
            }
            Ok(_) => return Err("UPDATE creating UNIQUE conflict should have failed".into()),
        }
        // Verify the row was NOT modified (no partial update).
        let r = engine.execute("SELECT count(*) FROM t WHERE email = 'b'")?;
        assert_eq!(r.scalar_u64(), Some(1), "row 2 should still have email='b'");
        Ok(())
    }

    /// Task 3.5 — INSERT that violates a CHECK constraint fails with
    /// error 23514. A valid INSERT succeeds.
    ///
    /// Note: the test uses `x = 0` (rather than `x = -1` as in the task
    /// description) because the DML parser's VALUES clause doesn't handle
    /// negative integer literals — `-1` is tokenized as `Op("-")` followed
    /// by `Int(1)`, producing 2 values for a 1-column table. Using `x = 0`
    /// still violates `CHECK (x > 0)` (0 is not > 0) and exercises the
    /// same enforcement path.
    #[test]
    fn test_check_violation_at_insert() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (x INT CHECK (x > 0))")?;
        // x = 0 violates CHECK (x > 0).
        match engine.execute("INSERT INTO t VALUES (0)") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("23514"),
                    "expected 23514 error for CHECK violation, got: {msg}"
                );
            }
            Ok(_) => return Err("x=0 should have violated CHECK (x > 0)".into()),
        }
        // x = 5 passes.
        engine.execute("INSERT INTO t VALUES (5)")?;
        let r = engine.execute("SELECT count(*) FROM t")?;
        assert_eq!(r.scalar_u64(), Some(1));
        Ok(())
    }

    /// Task 3.5 — UPDATE that would violate a CHECK constraint fails with
    /// error 23514. The row is left unchanged.
    ///
    /// Note: uses `x = 0` (rather than `x = -1`) for the same parser
    /// reason as `test_check_violation_at_insert` — the UPDATE assignment
    /// `x = -1` round-trips through `Expr::to_string()` as `"(-1)"`,
    /// which `parse_value_cell` would hash instead of parsing as -1.
    #[test]
    fn test_check_violation_at_update() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (x INT CHECK (x > 0))")?;
        engine.execute("INSERT INTO t VALUES (5)")?;
        match engine.execute("UPDATE t SET x = 0") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("23514"),
                    "expected 23514 error for CHECK violation at UPDATE, got: {msg}"
                );
            }
            Ok(_) => return Err("UPDATE to x=0 should have violated CHECK (x > 0)".into()),
        }
        // Verify the row was NOT modified.
        let r = engine.execute("SELECT count(*) FROM t WHERE x = 5")?;
        assert_eq!(r.scalar_u64(), Some(1), "row should still have x=5");
        Ok(())
    }

    // -----------------------------------------------------------------
    // Task 3.3 — UPDATE new version visible to updating txn
    // -----------------------------------------------------------------

    /// Task 3.3 DoD: `BEGIN; INSERT (1,10); UPDATE SET v=99 WHERE id=1;
    /// SELECT v FROM t WHERE id=1` returns `99` (not `10`).
    ///
    /// Verifies the snapshot-isolation read rule: the updating txn sees
    /// its own new version (xmin == active_txn_id, xmax None) and does
    /// NOT see the old version (xmax == active_txn_id → invisible).
    #[test]
    fn test_update_visible_to_updating_txn() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.enable_mvcc()?;
        engine.execute("CREATE TABLE t (id INT, v INT)")?;

        engine.execute("BEGIN")?;
        engine.execute("INSERT INTO t VALUES (1, 10)")?;
        engine.execute("UPDATE t SET v = 99 WHERE id = 1")?;

        // The SELECT inside the same txn must see the updated value 99.
        let r = engine.execute("SELECT v FROM t WHERE id = 1")?;
        let v = r
            .column("v")
            .and_then(|c| c.first().copied())
            .ok_or_else(|| "expected non-empty SELECT v result".to_string())?;
        assert_eq!(v, 99, "updating txn must see its own new version (99), not the old value (10)");

        engine.execute("COMMIT")?;

        // After COMMIT, an autocommit SELECT also sees 99 (the new
        // version is now committed).
        let r = engine.execute("SELECT v FROM t WHERE id = 1")?;
        let v = r
            .column("v")
            .and_then(|c| c.first().copied())
            .ok_or_else(|| "expected non-empty SELECT v result after COMMIT".to_string())?;
        assert_eq!(v, 99, "post-commit autocommit SELECT must see 99");
        Ok(())
    }

    /// Task 3.3 DoD: `INSERT; UPDATE; UPDATE` produces a 3-version chain
    /// for the row, and the LATEST version (carrying the second UPDATE's
    /// value) is visible to the updating txn.
    ///
    /// Note: the INSERT path stores an empty `values` vec (the version
    /// metadata records `xmin`/`xmax` only; the actual column data lives
    /// in `table.columns`). Only UPDATE versions carry non-empty `values`
    /// (read from the mutated columns at UPDATE time).
    #[test]
    fn test_version_chain_roundtrip() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut engine = QueryEngine::in_memory();
        engine.enable_mvcc()?;
        engine.execute("CREATE TABLE t (id INT, v INT)")?;

        engine.execute("BEGIN")?;
        let txn_id = engine
            .mvcc_txn_manager()
            .active_id()
            .ok_or_else(|| "active txn id should be Some after BEGIN".to_string())?;
        engine.execute("INSERT INTO t VALUES (1, 10)")?;
        engine.execute("UPDATE t SET v = 20 WHERE id = 1")?;
        engine.execute("UPDATE t SET v = 30 WHERE id = 1")?;

        // The chain at row 0 should have 3 versions: original INSERT,
        // first UPDATE, second UPDATE.
        let table = engine
            .catalog()
            .get("t")
            .ok_or_else(|| "table t should exist".to_string())?;
        let chain = &table.row_versions[0];
        assert_eq!(chain.len(), 3, "expected 3 versions in the chain; got {}", chain.len());

        // chain[0] = original INSERT (xmin=txn_id, xmax=txn_id — tombstoned by 1st UPDATE).
        // INSERT versions carry empty `values` (column data is in `table.columns`).
        assert_eq!(chain[0].xmin, txn_id);
        assert_eq!(chain[0].xmax, Some(txn_id), "original version tombstoned by 1st UPDATE");
        assert!(chain[0].values.is_empty(), "INSERT version carries empty values");

        // chain[1] = 1st UPDATE (xmin=txn_id, xmax=txn_id — tombstoned by 2nd UPDATE).
        assert_eq!(chain[1].xmin, txn_id);
        assert_eq!(chain[1].xmax, Some(txn_id), "1st UPDATE version tombstoned by 2nd UPDATE");
        assert!(chain[1].values.contains(&20), "1st UPDATE version carries v=20");

        // chain[2] = 2nd UPDATE (xmin=txn_id, xmax=None — LIVE).
        assert_eq!(chain[2].xmin, txn_id);
        assert_eq!(chain[2].xmax, None, "latest version (2nd UPDATE) is live");
        assert!(chain[2].values.contains(&30), "latest version carries v=30");

        // The updating txn's SELECT must see the latest version (v=30).
        // (The value is read from `table.columns[1][0]`, which UPDATE
        // mutated in place to 30.)
        let r = engine.execute("SELECT v FROM t WHERE id = 1")?;
        let v = r
            .column("v")
            .and_then(|c| c.first().copied())
            .ok_or_else(|| "expected non-empty SELECT v result".to_string())?;
        assert_eq!(v, 30, "updating txn must see the latest version (v=30)");

        engine.execute("COMMIT")?;
        Ok(())
    }
}
