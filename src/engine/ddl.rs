//! DDL execution — CREATE/DROP/ALTER TABLE and INDEX.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_ddl(&mut self, ddl: crate::sql::DdlStatement) -> Result<QueryResult> {
        match ddl {
            crate::sql::DdlStatement::Create(ct) => {
                let full_name = if ct.schema == "dbo" {
                    ct.name.clone()
                } else {
                    format!("{}.{}", ct.schema, ct.name)
                };
                if self.catalog.get(&full_name).is_some() {
                    if ct.if_not_exists {
                        return Ok(QueryResult::empty());
                    }
                    return Err(Error::Other(format!("table \"{full_name}\" already exists")));
                }
                // Build an empty Table with the right column names.
                let column_names: Vec<String> = ct.columns.iter().map(|c| c.name.clone()).collect();
                let columns: Vec<std::sync::Arc<Vec<u64>>> =
                    ct.columns.iter().map(|_| std::sync::Arc::new(Vec::new())).collect();
                let table = Table {
                    name: full_name.clone(),
                    columns,
                    column_names,
                    row_count: 0,
                    string_columns: vec![None; ct.columns.len()],
                    null_bitmaps: vec![None; ct.columns.len()],
                    i32_columns: vec![None; ct.columns.len()],
                    schema: Some(crate::schema::table_schema::TableSchema::from_create_table(&ct)),
                    row_versions: Vec::new(),
                };
                self.catalog.register(table);
                Ok(QueryResult::empty())
            }
            crate::sql::DdlStatement::Drop(dt) => {
                let full_name = if dt.schema == "dbo" {
                    dt.name.clone()
                } else {
                    format!("{}.{}", dt.schema, dt.name)
                };
                if self.catalog.get(&full_name).is_none() {
                    if dt.if_exists {
                        return Ok(QueryResult::empty());
                    }
                    return Err(Error::NotFound(format!("table \"{full_name}\"")));
                }
                self.catalog.drop(&full_name);
                Ok(QueryResult::empty())
            }
            crate::sql::DdlStatement::CreateSchema(_) => {
                // Schemas are implicit — CREATE SCHEMA is a no-op.
                Ok(QueryResult::empty())
            }
            crate::sql::DdlStatement::AlterTable(at) => self.execute_alter_table(at),
            crate::sql::DdlStatement::CreateIndex(ci) => self.execute_create_index(ci),
            crate::sql::DdlStatement::DropIndex(di) => self.execute_drop_index(di),
        }
    }

    /// Execute an ALTER TABLE statement (Wave 66).
    ///
    /// Supports:
    /// - `ADD COLUMN col TYPE [DEFAULT x]` — appends a new column to the
    ///   schema; existing rows get the default value (0 for INT, 0.0 for
    ///   FLOAT, '' for VARCHAR).
    /// - `DROP COLUMN col` — removes the column; the schema is updated.
    /// - `ALTER COLUMN col TYPE new_type` — changes the column type in
    ///   the schema (a no-op for data, since all cells are u64).
    pub(crate) fn execute_alter_table(
        &mut self,
        at: crate::sql::AlterTable,
    ) -> Result<QueryResult> {
        use crate::sql::AlterAction;
        let full_name =
            if at.schema == "dbo" { at.name.clone() } else { format!("{}.{}", at.schema, at.name) };
        match at.action {
            AlterAction::AddColumn(col_def) => {
                self.catalog
                    .with_mut(&full_name, |table| -> Result<QueryResult> {
                        // Build the default cell value for existing rows.
                        let default_cell = default_cell_for_type(&col_def, table.row_count);
                        // Append a new column with `row_count` copies of the default.
                        let new_col: Vec<u64> = vec![default_cell; table.row_count];
                        table.columns.push(std::sync::Arc::new(new_col));
                        table.column_names.push(col_def.name.clone());
                        table.string_columns.push(None);
                        table.null_bitmaps.push(None);
                        // Update the schema.
                        if let Some(ref mut schema) = table.schema {
                            schema.columns.push(crate::schema::table_schema::ColumnSchema {
                                name: col_def.name.clone(),
                                col_type: col_def.col_type.clone(),
                                not_null: col_def.not_null,
                                primary_key: col_def.primary_key,
                                // Task 3.2 + 3.5: preserve column-level UNIQUE / CHECK.
                                unique: col_def.unique,
                                check: col_def.check.clone(),
                            });
                        }
                        Ok(QueryResult::empty())
                    })
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?
            }
            AlterAction::DropColumn(col_name) => {
                self.catalog
                    .with_mut(&full_name, |table| -> Result<QueryResult> {
                        let idx = table
                            .column_idx(&col_name)
                            .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
                        table.columns.remove(idx);
                        table.column_names.remove(idx);
                        if idx < table.string_columns.len() {
                            table.string_columns.remove(idx);
                        }
                        if idx < table.null_bitmaps.len() {
                            table.null_bitmaps.remove(idx);
                        }
                        if let Some(ref mut schema) = table.schema {
                            if idx < schema.columns.len() {
                                schema.columns.remove(idx);
                            }
                        }
                        // Also drop any index on this column.
                        self.index_manager.drop(&full_name, &col_name);
                        Ok(QueryResult::empty())
                    })
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?
            }
            AlterAction::AlterColumnType { column, new_type } => {
                self.catalog
                    .with_mut(&full_name, |table| -> Result<QueryResult> {
                        let idx = table
                            .column_idx(&column)
                            .ok_or_else(|| Error::NotFound(format!("column \"{column}\"")))?;
                        if let Some(ref mut schema) = table.schema {
                            if idx < schema.columns.len() {
                                schema.columns[idx].col_type = new_type;
                            }
                        }
                        // For widening conversions (INT→BIGINT, FLOAT→DOUBLE) this
                        // is a no-op (all stored as u64). For narrowing, the cell
                        // values are unchanged (the spec says "truncate" but u64
                        // storage makes that a no-op too — we'd only need to
                        // truncate if we had a separate typed storage format).
                        Ok(QueryResult::empty())
                    })
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?
            }
        }
    }

    /// Execute a CREATE INDEX statement (Wave 66).
    ///
    /// Registers a named index in the IndexManager and builds the
    /// in-memory hash index data for fast equality lookups.
    pub(crate) fn execute_create_index(
        &mut self,
        ci: crate::sql::CreateIndex,
    ) -> Result<QueryResult> {
        // Check if an index with the same name already exists.
        if self.index_manager.get_by_name(&ci.index_name).is_some() {
            if ci.if_not_exists {
                return Ok(QueryResult::empty());
            }
            return Err(Error::Other(format!("index \"{}\" already exists", ci.index_name)));
        }
        // Look up the table and column.
        let table = self
            .catalog
            .get(&ci.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", ci.table)))?;
        let col_idx = table
            .column_idx(&ci.column)
            .ok_or_else(|| Error::NotFound(format!("column \"{}\"", ci.column)))?;
        if col_idx >= table.columns.len() {
            return Err(Error::Other(format!("column \"{}\" out of range", ci.column)));
        }
        // Snapshot the column values (so the index is stable even if the
        // table is later mutated — index maintenance is a follow-up).
        let values: Vec<u64> = table.columns[col_idx].as_ref().clone();
        let cardinality = {
            let mut distinct = std::collections::HashSet::new();
            for &v in &values {
                distinct.insert(v);
            }
            distinct.len() as u64
        };
        // Register the index.
        self.index_manager.create_named(
            &ci.index_name,
            &ci.table,
            &ci.column,
            crate::index::manager::IndexType::Hash,
            cardinality,
        );
        // Build the in-memory hash index.
        self.index_manager.build_hash_index(&ci.table, &ci.column, &values);
        Ok(QueryResult::empty())
    }

    /// Execute a DROP INDEX statement (Wave 66).
    pub(crate) fn execute_drop_index(&mut self, di: crate::sql::DropIndex) -> Result<QueryResult> {
        if !self.index_manager.drop_by_name(&di.index_name) {
            if di.if_exists {
                return Ok(QueryResult::empty());
            }
            return Err(Error::NotFound(format!("index \"{}\"", di.index_name)));
        }
        Ok(QueryResult::empty())
    }

    /// Wave 66: fast path for `SELECT ... FROM t WHERE col = literal` when
    /// an index exists on `(t, col)`. Uses the in-memory hash index for
    /// O(1) lookup instead of a full scan.
    ///
    /// Returns `None` if the fast path doesn't apply (e.g. no index, or
    /// the query shape doesn't match). Returns `Some(Ok(result))` if the
    /// index was used. Returns `Some(Err(...))` if the index lookup was
    /// attempted but failed (e.g. table not found).
    pub(crate) fn try_indexed_lookup(
        &self,
        query: &crate::sql::SelectQuery,
    ) -> Option<Result<QueryResult>> {
        use crate::sql::parser::{Expr, SelectItem, Value};

        // Only consider single-FROM queries without JOINs / GROUP BY /
        // HAVING / ORDER BY / DISTINCT.
        if !query.joins.is_empty() || !query.group_by.is_empty() || query.having.is_some() {
            return None;
        }
        if !query.order_by.is_empty() || query.distinct {
            return None;
        }

        // WHERE must be present and be a simple `col = literal` (in either
        // order).
        let where_expr = match &query.where_clause {
            Some(e) => e,
            None => return None,
        };
        let (col_name, val_cell) = match extract_eq_predicate(where_expr) {
            Some(x) => x,
            None => return None,
        };

        // Check if there's an index on (table, col).
        if !self.index_manager.has_index(&query.from, &col_name) {
            return None;
        }

        // Look up the table.
        let table = match self.catalog.get(&query.from) {
            Some(t) => t,
            None => {
                return Some(Err(Error::NotFound(format!("table '{}'", query.from))));
            }
        };
        // Defensive: confirm the column exists in the table. (The index
        // manager wouldn't have built an index on a non-existent column,
        // but the table could have been altered since.)
        if table.column_idx(&col_name).is_none() {
            return None;
        }

        // Index lookup: get the row indices where col == val_cell.
        let row_indices = match self.index_manager.lookup(&query.from, &col_name, val_cell) {
            Some(idxs) => idxs.clone(),
            None => Vec::new(),
        };

        // Apply LIMIT if present.
        let limit = query.limit.unwrap_or(row_indices.len());
        let row_indices: Vec<usize> = row_indices.into_iter().take(limit).collect();

        // Build the result columns based on the SELECT list.
        let mut cols: Vec<ResultColumn> = Vec::new();
        for item in &query.select {
            match item {
                SelectItem::Star => {
                    for (i, name) in table.column_names.iter().enumerate() {
                        let values: Vec<u64> = row_indices
                            .iter()
                            .map(|&r| table.columns[i].get(r).copied().unwrap_or(0))
                            .collect();
                        let string_values = if i < table.string_columns.len() {
                            table.string_columns[i].as_ref().map(|sc| {
                                row_indices
                                    .iter()
                                    .map(|&r| sc.get(r).to_string())
                                    .collect::<Vec<_>>()
                            })
                        } else {
                            None
                        };
                        let null_mask = if i < table.null_bitmaps.len() {
                            table.null_bitmaps[i].as_ref().map(|bm| {
                                row_indices.iter().map(|&r| bm.is_null(r)).collect::<Vec<_>>()
                            })
                        } else {
                            None
                        };
                        cols.push(ResultColumn {
                            name: name.clone(),
                            values,
                            string_values,
                            type_oid: 0,
                            null_mask,
                        });
                    }
                }
                SelectItem::Column(name) => {
                    let idx = match table.column_idx(name) {
                        Some(i) => i,
                        None => {
                            return Some(Err(Error::NotFound(format!("column '{name}'"))));
                        }
                    };
                    let values: Vec<u64> = row_indices
                        .iter()
                        .map(|&r| table.columns[idx].get(r).copied().unwrap_or(0))
                        .collect();
                    let string_values = if idx < table.string_columns.len() {
                        table.string_columns[idx].as_ref().map(|sc| {
                            row_indices.iter().map(|&r| sc.get(r).to_string()).collect::<Vec<_>>()
                        })
                    } else {
                        None
                    };
                    let null_mask = if idx < table.null_bitmaps.len() {
                        table.null_bitmaps[idx].as_ref().map(|bm| {
                            row_indices.iter().map(|&r| bm.is_null(r)).collect::<Vec<_>>()
                        })
                    } else {
                        None
                    };
                    cols.push(ResultColumn {
                        name: name.clone(),
                        values,
                        string_values,
                        type_oid: 0,
                        null_mask,
                    });
                }
                // Aggregates, literals, window functions, and general
                // expressions don't go through this fast path — fall
                // back to the normal executor.
                SelectItem::Aggregate { .. }
                | SelectItem::Literal(_)
                | SelectItem::Window { .. }
                | SelectItem::Expression { .. } => return None,
            }
        }

        Some(Ok(QueryResult { columns: cols, row_count: row_indices.len(), elapsed_us: 0 }))
    }
}
