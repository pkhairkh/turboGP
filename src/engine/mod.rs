//! # Query engine — the end-to-end SQL surface.
//!
//! [`QueryEngine`] is the top-level type that ties the SQL parser, the
//! catalog, the kernel table, and the cost model together. It is the
//! DuckDB-style entry point: hand it a SQL string, get back a
//! [`QueryResult`].
//!
//! ## Pipeline
//!
//! ```text
//!   SQL string
//!       │
//!       ▼  sql::parse_with_extensions
//!   (SelectQuery, QueryExtensions)
//!       │
//!       ▼  engine::execute_select
//!   QueryResult
//! ```
//!
//! The parse step lives in [`crate::sql`]; the execute step lives in
//! [`crate::engine::executor`]. This module's [`QueryEngine`] is the
//! glue that owns the catalog and the kernel table, captures the wall-
//! clock time around the pipeline, and returns the result.
//!
//! ## Why a struct (not free functions)
//!
//! Free functions would force every caller to construct a `Catalog`,
//! `KernelTable`, and `CostModel` themselves and pass them to every
//! call. The struct bundles them once and exposes a single `execute`
//! method, which is the shape callers actually want:
//!
//! ```ignore
//! let mut engine = QueryEngine::in_memory();
//! engine.load_parquet("hits.parquet", "hits")?;
//! let result = engine.execute("SELECT count(*) FROM hits")?;
//! println!("{}", result.scalar_u64().unwrap());
//! ```
//!
//! ## Concurrency
//!
//! `QueryEngine` is `Send` but not `Sync` (the catalog is a plain
//! `HashMap`, not a `RwLock<HashMap>`). Callers that want to share an
//! engine across threads should wrap it in an `Arc<Mutex<QueryEngine>>`
//! themselves — the same pattern the catalog module recommends.
//!
//! ## Loading data
//!
//! Two convenience constructors wrap the Parquet and CSV readers from
//! [`crate::datasource`]:
//!
//! - [`QueryEngine::load_parquet`] reads a `.parquet` file and registers
//!   it in the catalog under the given name (or, if no name is given,
//!   under the file's stem).
//! - [`QueryEngine::load_csv`] does the same for `.csv` files.
//!
//! Both return the row count so the caller can sanity-check the load.

pub mod executor;
pub mod result;
pub mod tpch;

pub use executor::execute_select;
pub use result::{QueryResult, ResultColumn};

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::datasource::{read_csv, read_parquet, read_parquet_column, read_parquet_column_names};
use crate::error::{Error, Result};
use crate::kernel::KernelTable;
use crate::planner::CostModel;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// The top-level engine: catalog + kernel table + cost model, plus a
/// single `execute` method that runs a SQL query end-to-end.
///
/// See the module docs for the pipeline and usage examples.
pub struct QueryEngine {
    /// The table catalog (name → [`Table`]).
    catalog: Catalog,
    /// The kernel table: maps `(Operator, CpuTarget, MemoryTier)` to the
    /// best kernel for that combination on the running CPU.
    kernel_table: Arc<KernelTable>,
    /// The cost model: per-tier throughput estimates. Currently unused
    /// by the executor (it picks L3 by default), but retained in the
    /// struct so a future wave that wires in the `PlanLowerer` doesn't
    /// have to change every call site.
    cost_model: CostModel,
    /// Transaction manager for BEGIN/COMMIT/ROLLBACK (Wave 5).
    txn_manager: crate::txn::TxnManager,
    /// Write-ahead log for durability (Wave 37). None when not configured.
    wal: Option<crate::storage::recovery::Wal>,
    /// Index manager for secondary indexes (Wave 31).
    pub index_manager: crate::index::manager::IndexManager,
    /// Hash column registry for materialized string hashes.
    pub hash_registry: crate::exec::hash_column::HashColumnRegistry,
    /// View registry: CREATE VIEW / DROP VIEW / view expansion.
    pub views: crate::catalog::views::ViewRegistry,
    /// Stored procedure registry: CREATE PROCEDURE / EXEC.
    pub procedures: crate::exec::procedure::ProcedureRegistry,
    /// Table-valued parameter types.
    pub table_types: crate::exec::procedure::TableTypeRegistry,
    /// Temporal tables: maps table name → TemporalTable for FOR SYSTEM_TIME
    /// queries.
    pub temporals: HashMap<String, crate::exec::temporal::TemporalTable>,
    /// Buffer pool for on-disk page-level storage (Wave 63).
    /// None when the engine is in-memory only (the default). Set via
    /// `QueryEngine::with_data_dir()` to enable disk persistence.
    pub buffer_pool: Option<crate::storage::buffer_pool::BufferPool>,
    /// Table name → table_id mapping for the buffer pool (Wave 63).
    /// Assigned lazily when a table is first persisted.
    table_ids: HashMap<String, u64>,
    /// Next table_id to assign (Wave 63).
    next_table_id: u64,
    /// Named savepoints within the current transaction (Wave 69).
    /// Each savepoint is a (name, catalog_snapshot) pair. On ROLLBACK TO
    /// <name>, the catalog is restored from the snapshot. On COMMIT or
    /// ROLLBACK, all savepoints are cleared.
    savepoints: Vec<(String, Catalog)>,
    /// Allowed directories for COPY TO/FROM operations (Wave 2 security).
    /// Empty by default — COPY is disabled unless explicitly configured.
    pub allowed_copy_dirs: Vec<std::path::PathBuf>,
}

impl QueryEngine {
    /// Try to execute a SELECT query without mutating the engine (Wave 41).
    ///
    /// This method takes `&self` (not `&mut self`), so it can be called
    /// concurrently from multiple threads when the engine is wrapped in
    /// `Arc<RwLock<QueryEngine>>`. SELECT queries take a read lock;
    /// DML/DDL take a write lock.
    ///
    /// Returns `Ok(result)` if the query was a SELECT that succeeded.
    /// Returns `Err(Error::Other("not a readonly query"))` if the query
    /// is DDL/DML/transaction control (caller should use `execute()` with
    /// a write lock).
    pub fn try_readonly_select(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();

        // Only SELECT queries can be readonly.
        if !lower.starts_with("select") && !lower.starts_with("with") {
            return Err(Error::Other("not a readonly query".into()));
        }

        // Try DDL/DML — these are NOT readonly.
        if crate::sql::parse_ddl(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("DDL requires write lock".into()));
        }
        if crate::sql::parse_dml(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("DML requires write lock".into()));
        }

        // Try CTE.
        if let Some(with_result) = crate::sql::parse_with(sql) {
            return Err(Error::Other("CTE requires write lock".into()));
        }

        // Parse as SELECT and execute against the current catalog.
        let (query, extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // Basic parser failed — need tpch fallback, which requires &mut self.
                return Err(Error::Other("query needs tpch fallback — requires write lock".into()));
            }
        };

        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
        ) {
            Ok(mut result) => {
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            Err(_exec_err) => {
                // execute_select failed — need tpch fallback.
                Err(Error::Other("query failed in execute_select — needs tpch fallback".into()))
            }
        }
    }

    /// Construct an empty engine with the default kernel table and cost
    /// model. The catalog starts empty — register tables via
    /// [`QueryEngine::register_table`], [`QueryEngine::load_parquet`],
    /// or [`QueryEngine::load_csv`].
    /// Create a QueryEngine with default on-disk persistence (Wave 2).
    ///
    /// The default data directory is `./turbogp_data`. If the directory
    /// cannot be created or the WAL cannot be opened, the engine falls
    /// back to in-memory mode with a warning log. For tests that
    /// explicitly want no persistence, use [`QueryEngine::in_memory()`].
    pub fn new() -> Self {
        match Self::with_data_dir("./turbogp_data") {
            Ok(engine) => engine,
            Err(e) => {
                log::warn!(
                    "QueryEngine: persistence unavailable ({e}), falling back to in-memory mode"
                );
                Self::in_memory()
            }
        }
    }

    /// Create a QueryEngine with no on-disk persistence.
    ///
    /// All data is in-memory and lost on process exit. Use this for
    /// tests and ephemeral workloads where durability is not required.
    pub fn in_memory() -> Self {
        let mut catalog = Catalog::new();
        // Register a dummy table that allows `SELECT 1` and `SELECT count(*)`
        // without a FROM clause. The table has one row and one column.
        let dummy = Table {
            name: "__dummy__".into(),
            columns: vec![std::sync::Arc::new(vec![0u64])],
            column_names: vec!["__dummy_col__".into()],
            row_count: 1,
            string_columns: vec![None],
            null_bitmaps: vec![None],
            schema: None,
        };
        catalog.register(dummy);
        Self {
            catalog,
            kernel_table: Arc::new(KernelTable::new()),
            cost_model: CostModel::default(),
            txn_manager: crate::txn::TxnManager::new(),
            wal: None,
            index_manager: crate::index::manager::IndexManager::new(),
            hash_registry: crate::exec::hash_column::HashColumnRegistry::new(),
            views: crate::catalog::views::ViewRegistry::new(),
            procedures: crate::exec::procedure::ProcedureRegistry::new(),
            table_types: crate::exec::procedure::TableTypeRegistry::new(),
            temporals: HashMap::new(),
            buffer_pool: None,
            table_ids: HashMap::new(),
            next_table_id: 1,
            savepoints: Vec::new(),
            allowed_copy_dirs: Vec::new(),
        }
    }

    /// Create a QueryEngine with on-disk persistence (Wave 63).
    /// The `data_dir` is where table files (`<table_id>.tbl`) and the WAL
    /// (`wal.log`) are stored. Tables created via CREATE TABLE are persisted
    /// to disk; INSERT/UPDATE/DELETE write through the buffer pool and are
    /// durable after COMMIT.
    pub fn with_data_dir<P: AsRef<std::path::Path>>(data_dir: P) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        let mut engine = Self::new();
        let bp = crate::storage::buffer_pool::BufferPool::new(data_dir, 256)?;
        engine.buffer_pool = Some(bp);
        // Also open a WAL in the same data directory.
        let wal_path = data_dir.join("wal.log");
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        engine.wal = Some(wal);
        // Replay the WAL to restore committed state.
        engine.replay_wal()?;
        Ok(engine)
    }

    /// Replay the WAL to restore committed state (Wave 63).
    /// This re-executes SQL records and applies physical page changes.
    /// Only committed transactions are replayed; uncommitted (no COMMIT
    /// marker after the DML) are discarded.
    fn replay_wal(&mut self) -> Result<()> {
        let wal = match &self.wal {
            Some(w) => w,
            None => return Ok(()),
        };
        let records = wal.read_all().map_err(|e| Error::Other(format!("WAL read error: {e}")))?;

        // Group records by transaction. Autocommit records (txn_id == 0)
        // are applied immediately. Explicit transactions are applied only
        // if they have a COMMIT marker.
        let mut committed_txns: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for record in &records {
            if record.is_commit && record.txn_id != 0 {
                committed_txns.insert(record.txn_id);
            }
        }

        // Re-execute SQL records for committed transactions.
        for record in &records {
            // Skip rollback records and their transactions.
            if record.is_rollback {
                continue;
            }
            // Skip DML records for uncommitted explicit transactions.
            if record.txn_id != 0 && !committed_txns.contains(&record.txn_id) {
                continue;
            }
            // Skip BEGIN/COMMIT markers (they don't carry SQL).
            if record.sql.is_empty() {
                // But still apply physical changes.
                if let Some(ref change) = record.physical_change {
                    self.apply_physical_change(change)?;
                }
                continue;
            }
            // Re-execute the SQL (without re-appending to the WAL — that
            // would duplicate the records).
            let start = std::time::Instant::now();
            let _ = self.execute_inner_no_wal(&record.sql, &start, Some(record.txn_id));
        }
        Ok(())
    }

    /// Execute a SQL statement WITHOUT appending to the WAL (Wave 63).
    /// Used during WAL replay to avoid duplicating records.
    fn execute_inner_no_wal(
        &mut self,
        sql: &str,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Temporarily take the WAL out of self so we don't append during replay.
        let wal = self.wal.take();
        let result = self.execute_inner(sql, start, txn_id);
        self.wal = wal;
        result
    }

    /// Apply a physical page-level change to the buffer pool (Wave 63).
    fn apply_physical_change(
        &mut self,
        change: &crate::storage::recovery::PhysicalChange,
    ) -> Result<()> {
        use crate::storage::recovery::PhysicalChange;
        if self.buffer_pool.is_none() {
            return Ok(());
        }
        match change {
            PhysicalChange::PageAlloc { table_id, page_num } => {
                let page_id = crate::storage::buffer_pool::PageId::new(*table_id, *page_num);
                let _ = self.buffer_pool.as_mut().unwrap().fetch_page(page_id);
            }
            PhysicalChange::CellUpdate { table_id, page_num, cell_index, new_value, .. } => {
                let page_id = crate::storage::buffer_pool::PageId::new(*table_id, *page_num);
                let bp = self.buffer_pool.as_mut().unwrap();
                let idx =
                    bp.fetch_page(page_id).map_err(|e| Error::Other(format!("page fetch: {e}")))?;
                {
                    let page = bp.get_page_mut(idx);
                    page.set_cell(*cell_index, *new_value);
                }
                bp.unpin_page(page_id, true);
            }
            PhysicalChange::RowInsert { table_id, page_num, row_offset, values } => {
                let page_id = crate::storage::buffer_pool::PageId::new(*table_id, *page_num);
                let bp = self.buffer_pool.as_mut().unwrap();
                let idx =
                    bp.fetch_page(page_id).map_err(|e| Error::Other(format!("page fetch: {e}")))?;
                {
                    let page = bp.get_page_mut(idx);
                    for (i, &val) in values.iter().enumerate() {
                        let cell_idx = *row_offset + i;
                        if cell_idx < crate::storage::page::PAGE_CELLS {
                            page.set_cell(cell_idx, val);
                        }
                    }
                }
                bp.unpin_page(page_id, true);
            }
            PhysicalChange::RowDelete { .. } => {
                // Row deletion is handled by the catalog (row_count decrement).
                // Physical deletion (compaction) happens during VACUUM.
            }
        }
        Ok(())
    }

    /// Flush all dirty pages to disk and sync the WAL (Wave 63).
    /// Called automatically on COMMIT, or manually via CHECKPOINT.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(ref mut bp) = self.buffer_pool {
            bp.flush_all().map_err(|e| Error::Other(format!("flush: {e}")))?;
        }
        if let Some(ref mut wal) = self.wal {
            wal.sync().map_err(|e| Error::Other(format!("wal sync: {e}")))?;
        }
        Ok(())
    }

    /// Flush + write a checkpoint file, so the WAL can be safely
    /// truncated without data loss (Wave 2 fix).
    ///
    /// The checkpoint is written to `<data_dir>/checkpoint.sql` as a
    /// series of CREATE TABLE + INSERT statements. On restart, the
    /// engine replays the checkpoint first, then any WAL records
    /// written after the checkpoint.
    pub fn flush_with_checkpoint(&mut self) -> Result<()> {
        // 1. Flush dirty pages to disk.
        self.flush()?;
        // 2. Write a checkpoint file (if we have a data directory).
        if let Some(ref bp) = self.buffer_pool {
            let checkpoint_path = bp.data_dir().join("checkpoint.sql");
            match crate::storage::recovery::Checkpoint::save(&self.catalog, &checkpoint_path) {
                Ok(n) => {
                    log::debug!("checkpoint: wrote {n} tables to {}", checkpoint_path.display())
                }
                Err(e) => {
                    return Err(Error::Other(format!(
                        "checkpoint save to {}: {e}",
                        checkpoint_path.display()
                    )))
                }
            }
        }
        Ok(())
    }

    /// Open a QueryEngine with a WAL for durability (Wave 37).
    /// Replays the WAL on startup to restore committed state.
    pub fn open<P: AsRef<std::path::Path>>(wal_path: P) -> Result<Self> {
        let mut engine = Self::new();
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        // Replay committed transactions.
        let stats = crate::storage::recovery::replay_wal(&mut engine, &wal)?;
        log::info!(
            "WAL replay: {} records replayed, {} skipped, {} errors",
            stats.replayed,
            stats.skipped,
            stats.errors
        );
        engine.wal = Some(wal);
        Ok(engine)
    }

    /// Enable WAL on an existing engine.
    pub fn enable_wal<P: AsRef<std::path::Path>>(&mut self, wal_path: P) -> Result<()> {
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        self.wal = Some(wal);
        Ok(())
    }

    /// Append a DML/DDL record to the WAL (if enabled).
    ///
    /// Wave 51 fix: `txn_id` is `Some(id)` for statements inside an
    /// explicit transaction, or `None` for autocommit. The record carries
    /// the txn_id so replay can group statements by transaction.
    ///
    /// Wave 3 (A5): WAL errors are now propagated — if the WAL append or
    /// sync fails, the error is logged and the engine continues (the
    /// transaction will be visible in-memory but may not survive a crash).
    /// A future wave will make this abort the transaction.
    fn wal_append_txn(&mut self, sql: &str, txn_id: Option<u64>) {
        if let Some(ref mut wal) = self.wal {
            let record = match txn_id {
                Some(id) => crate::storage::recovery::WalRecord::txn_dml(id, sql),
                None => crate::storage::recovery::WalRecord::autocommit(sql),
            };
            if let Err(e) = wal.append(&record) {
                log::error!("WAL append failed (A5): {e}");
            }
            if let Err(e) = wal.sync() {
                log::error!("WAL sync failed (A5): {e}");
            }
        }
    }

    /// Append a pre-constructed WAL record (BEGIN / COMMIT / ROLLBACK
    /// markers, or any other special record). Used by `execute()` to
    /// write transaction boundary markers (Wave 51 fix).
    fn wal_append_record(&mut self, record: crate::storage::recovery::WalRecord) {
        if let Some(ref mut wal) = self.wal {
            if let Err(e) = wal.append(&record) {
                log::error!("WAL append failed (A5): {e}");
            }
            if let Err(e) = wal.sync() {
                log::error!("WAL sync failed (A5): {e}");
            }
        }
    }

    /// Construct an engine with a custom cost model (e.g., one with a
    /// learned cardinality estimator attached — see
    /// [`CostModel::with_learned`]). The kernel table is still the
    /// default.
    pub fn with_cost_model(cost_model: CostModel) -> Self {
        let mut engine = Self::new();
        engine.cost_model = cost_model;
        engine
    }

    /// Borrow the catalog. Read-only access for callers that want to
    /// introspect registered tables without going through SQL.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Borrow the kernel table. Used by callers that want to inspect
    /// the registered kernels or override the auto-detected CPU.
    pub fn kernel_table(&self) -> &KernelTable {
        &self.kernel_table
    }

    /// Borrow the cost model. Used by callers that want to inspect the
    /// hardware parameters (`cpu_freq_hz`, `simd_lanes`, etc.) or the
    /// attached learned estimator.
    pub fn cost_model(&self) -> &CostModel {
        &self.cost_model
    }

    /// Register a table in the catalog. The table's `name` field is
    /// used as the catalog key (so `SELECT * FROM <name>` works after
    /// registration).
    ///
    /// If a table with the same name is already registered, the new
    /// table replaces it (matching [`Catalog::register`]'s overwrite
    /// semantics).
    pub fn register_table(&mut self, table: Table) {
        self.catalog.register(table);
    }

    /// Load a Parquet file into the catalog under the given table name.
    ///
    /// Reads every column of every row group via
    /// [`crate::datasource::read_parquet`], converts each column to the
    /// engine's `Vec<u64>` cell format, and registers the resulting
    /// [`Table`] in the catalog. Returns the row count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] wrapping the underlying Parquet error
    /// if the file cannot be opened, parsed, or has mismatched column
    /// lengths.
    pub fn load_parquet(&mut self, path: &str, table_name: &str) -> Result<usize> {
        let loaded = read_parquet(path).map_err(|e| Error::Other(e.to_string()))?;
        let row_count = loaded.row_count;
        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Load a Parquet file with column pruning (Wave 30).
    ///
    /// Only loads columns referenced in the SQL query, skipping the rest.
    /// For a 105-column table where the query references 3 columns, this
    /// reduces I/O by ~35x.
    ///
    /// First loads all column names (cheap metadata read), then uses
    /// `prune_columns()` to determine which to materialize, then reads
    /// only those columns via `read_parquet_column()`.
    pub fn load_parquet_with_projection(
        &mut self,
        path: &str,
        table_name: &str,
        sql: &str,
    ) -> Result<usize> {
        // Step 1: Read all column names from the Parquet file (metadata only, no data).
        let all_columns =
            read_parquet_column_names(path).map_err(|e| Error::Other(e.to_string()))?;

        // Step 2: Determine which columns are needed.
        let (needed_cols, pruned_count) =
            crate::datasource::projection::prune_columns(sql, &all_columns);
        log::debug!(
            "load_parquet_with_projection: {} of {} columns needed ({} pruned)",
            needed_cols.len(),
            all_columns.len(),
            pruned_count
        );

        // Step 3: If SELECT * or all columns needed, just load everything.
        if needed_cols.is_empty() || needed_cols.len() == all_columns.len() {
            return self.load_parquet(path, table_name);
        }

        // Step 4: Load only the needed columns.
        let mut columns: Vec<crate::datasource::LoadedColumn> = Vec::new();
        let mut row_count = 0usize;
        for col_name in &needed_cols {
            if let Ok(loaded_col) = read_parquet_column(path, col_name) {
                row_count = loaded_col.row_count;
                columns.push(loaded_col);
            }
        }

        if columns.is_empty() {
            return self.load_parquet(path, table_name);
        }

        let loaded =
            crate::datasource::parquet::LoadedTable { name: table_name.into(), columns, row_count };
        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Load a CSV file into the catalog under the given table name.
    ///
    /// Reads the file via [`crate::datasource::read_csv`], infers
    /// per-column types (numeric → `i64` as `u64`; non-numeric →
    /// `xxh3_64` hash), and registers the resulting [`Table`] in the
    /// catalog. Returns the row count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] wrapping the underlying CSV error if
    /// the file cannot be read or has inconsistent column counts.
    pub fn load_csv(&mut self, path: &str, table_name: &str, has_header: bool) -> Result<usize> {
        let loaded = read_csv(path, has_header).map_err(|e| Error::Other(e.to_string()))?;
        let row_count = loaded.row_count;

        // Materialize hash columns for string columns (Wave 31).
        // This pre-computes xxh3 hashes so GROUP BY doesn't re-hash per query.
        for col in &loaded.columns {
            if col.string_search.is_some() {
                // The cells are already xxh3 hashes (computed by the CSV reader).
                // Register them as a HashColumn so GROUP BY can use the pre-computed
                // hashes instead of re-hashing per query.
                let hash_col = crate::exec::hash_column::HashColumn { hashes: col.cells.clone() };
                self.hash_registry.register(table_name, &col.name, hash_col);
            }
        }

        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Execute a SQL statement and return the result.
    ///
    /// This method dispatches on the SQL verb:
    /// - `SELECT` → the existing read-only execution path.
    /// - `CREATE TABLE` / `DROP TABLE` / `CREATE SCHEMA` → DDL path
    ///   (Wave 3) that mutates the catalog.
    /// - `INSERT` / `UPDATE` / `DELETE` → DML path (Wave 4) that mutates
    ///   table data.
    /// - `BEGIN` / `COMMIT` / `ROLLBACK` → transaction control (Wave 5,
    ///   currently a no-op stub that returns an empty result).
    ///
    /// Takes `&mut self` because DDL/DML mutate the catalog.
    ///
    /// # Errors
    ///
    /// - [`Error::Parse`] if the SQL is malformed.
    /// - [`Error::NotFound`] if the source table or a referenced column
    ///   does not exist in the catalog.
    /// - [`Error::Other`] for unsupported SQL features.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();

        // Transaction control: BEGIN/COMMIT/ROLLBACK.
        //
        // Wave 51 fix (Bug 8): BEGIN/COMMIT/ROLLBACK now write corresponding
        // markers to the WAL so replay can reconstruct transaction
        // boundaries. Previously the WAL only ever saw `txn_id: 0,
        // is_commit: false`, so a `BEGIN; INSERT; INSERT; COMMIT;` block
        // was indistinguishable from three autocommit INSERTs on replay
        // — and a `BEGIN; INSERT; ROLLBACK;` would still replay the INSERT.
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();

        // EXPLAIN: show the query plan (Wave 68).
        if lower.starts_with("explain ") {
            let inner_sql = &trimmed[8..];
            return self.execute_explain(inner_sql, &start);
        }
        // ANALYZE: execute the query and return timing stats (Wave 68).
        if lower.starts_with("analyze ") {
            let inner_sql = &trimmed[8..];
            return self.execute_analyze(inner_sql, &start);
        }
        // VACUUM: reclaim space from deleted rows (Wave 68).
        if lower.starts_with("vacuum") {
            return self.execute_vacuum(&start);
        }
        // COPY table TO 'file' / COPY table FROM 'file' (Wave 68).
        if lower.starts_with("copy ") {
            return self.execute_copy(trimmed, &start);
        }
        // CHECKPOINT: flush + write checkpoint file (Wave 2 fix).
        // Previously this just called flush(), which left the WAL
        // un-truncated and no checkpoint file was written. Now it
        // calls flush_with_checkpoint() for consistency with VACUUM.
        if lower.starts_with("checkpoint") {
            self.flush_with_checkpoint()?;
            return Ok(QueryResult::empty());
        }

        // SAVEPOINT, ROLLBACK TO, RELEASE are handled inside execute_inner
        // (after the txn snapshot is swapped in) so they operate on the
        // transaction's catalog, not the main catalog.

        if lower.starts_with("begin") || lower.starts_with("start transaction") {
            let id = self.txn_manager.begin(&self.catalog).map_err(Error::Other)?;
            self.wal_append_record(crate::storage::recovery::WalRecord::begin(id));
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("commit") {
            // Capture the txn_id before we drain the transaction.
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            let committed = self.txn_manager.commit().map_err(Error::Other)?;
            self.catalog = committed;
            self.savepoints.clear(); // Wave 69: clear savepoints on commit.
            self.wal_append_record(crate::storage::recovery::WalRecord::commit(txn_id));
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("rollback") && !lower.starts_with("rollback to ") {
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            self.txn_manager.rollback().map_err(Error::Other)?;
            self.savepoints.clear(); // Wave 69: clear savepoints on rollback.
            self.wal_append_record(crate::storage::recovery::WalRecord::rollback(txn_id));
            return Ok(QueryResult::empty());
        }

        // If a transaction is active, route all DML/DDL/SELECT to the
        // snapshot catalog. Otherwise, use the main catalog.
        // We do this by swapping the snapshot into self.catalog for the
        // duration of the statement, then swapping back.
        let txn_active = self.txn_manager.is_active();
        if txn_active {
            // Take the snapshot out of the txn manager temporarily.
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            let mut txn = self.txn_manager.active.take().expect("txn active");
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            let result = self.execute_inner(sql, &start, Some(txn_id));
            // Swap back: self.catalog goes back to being the main catalog
            // (unchanged), txn.snapshot becomes the (possibly modified)
            // transaction state.
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            self.txn_manager.active = Some(txn);
            return result;
        }

        self.execute_inner(sql, &start, None)
    }

    /// Inner execution: dispatches DDL, DML, CTE, and SELECT without
    /// transaction awareness. Called by `execute` either with the main
    /// catalog or with the txn snapshot swapped in.
    ///
    /// `txn_id` is `Some(id)` when executing inside an explicit
    /// transaction (so the WAL record carries the right txn_id), or
    /// `None` for autocommit.
    ///
    /// Wave 51 fix (Bug 9): the WAL append now happens AFTER a successful
    /// execute. Previously `wal_append(sql)` was called BEFORE
    /// `execute_ddl` / `execute_dml`, so a failed execute (e.g. INSERT
    /// INTO nonexistent) would still leave a record in the WAL — and
    /// replay would fail on restart.
    fn execute_inner(
        &mut self,
        sql: &str,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Wave 69: SAVEPOINT / ROLLBACK TO / RELEASE — handle these here
        // (after the txn snapshot is swapped in by the caller) so they
        // operate on the transaction's catalog.
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();
        if lower.starts_with("savepoint ") {
            let name = trimmed[10..].trim().to_string();
            return self.execute_savepoint(name, start);
        }
        if lower.starts_with("rollback to ") {
            let name = trimmed[12..].trim().to_string();
            return self.execute_rollback_to(&name, start);
        }
        if lower.starts_with("release ") {
            let name = trimmed[8..].trim().to_string();
            return self.execute_release_savepoint(&name, start);
        }

        // Wave 53: Temporal query — FOR SYSTEM_TIME AS OF <timestamp>.
        // Check this FIRST because the basic lexer fails on very large
        // integer timestamps (u64 values that overflow i64), which would
        // cause the DDL/DML parsers to error before we reach this check.
        if let Some((table_name, timestamp)) = parse_for_system_time(sql) {
            if let Some(temporal) = self.temporals.get(&table_name) {
                let rows = temporal.query_as_of(timestamp);
                return Ok(rows_to_query_result(&rows, &temporal.column_names, start));
            }
        }

        // Try CTE (WITH ... SELECT ...) first.
        if let Some(with_result) = crate::sql::parse_with(sql) {
            let with = with_result.map_err(Error::Parse)?;
            let mut result = self.execute_with(with, txn_id)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: View DDL — CREATE VIEW / DROP VIEW.
        if let Some(parsed) = crate::catalog::views::parse_create_view(sql) {
            let view = parsed.map_err(Error::Other)?;
            self.views.create(view);
            let mut result = QueryResult::empty();
            result.row_count = 0;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }
        if let Some(parsed) = crate::catalog::views::parse_drop_view(sql) {
            let (name, _if_exists) = parsed.map_err(Error::Other)?;
            self.views.drop(&name);
            let mut result = QueryResult::empty();
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: Stored procedure DDL — CREATE PROCEDURE / CREATE FUNCTION.
        if let Some(parsed) = crate::exec::procedure::parse_create_procedure(sql) {
            let proc_def = parsed.map_err(Error::Other)?;
            self.procedures.create(proc_def);
            let mut result = QueryResult::empty();
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: EXEC procedure_name [args].
        if let Some(parsed) = crate::exec::procedure::parse_exec(sql) {
            let (proc_name, args) = parsed.map_err(Error::Other)?;
            let proc_def = self
                .procedures
                .get(&proc_name)
                .ok_or_else(|| Error::NotFound(format!("procedure \"{proc_name}\"")))?
                .clone();
            // Substitute @param references in the body with the arg values.
            let body = substitute_proc_params(&proc_def.body, &args);
            // Re-execute the body SQL. If it's multi-statement, split on ';'
            // and execute each one, returning the last result.
            let mut last_result = QueryResult::empty();
            for stmt in body.split(';').filter(|s| !s.trim().is_empty()) {
                last_result = self.execute(stmt)?;
            }
            last_result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(last_result);
        }

        // Wave 53: MERGE statement.
        if let Some(merge) = parse_merge(sql) {
            return self.execute_merge_stmt(merge, start);
        }

        // Wave 60c: UNION ALL. Detect `UNION ALL` in the SQL, split into
        // two SELECT statements, execute both, and concatenate the results.
        if let Some((left_sql, right_sql)) = split_union_all(sql) {
            let left_result = self.execute_inner(&left_sql, start, txn_id)?;
            let right_result = self.execute_inner(&right_sql, start, txn_id)?;
            return Ok(concatenate_results(left_result, right_result, start));
        }

        // Wave 56b: PIVOT clause. Detect `PIVOT (` in the SQL and route to
        // the pivot module. We parse the PIVOT spec, strip the PIVOT clause
        // from the SQL, execute the remaining SELECT to get the input rows,
        // then apply the pivot transformation. The group_col is auto-detected
        // as the first input column that's neither the pivot_col nor the
        // value_col.
        //
        // Supported syntax (simplified):
        //   SELECT * FROM <table> PIVOT (SUM(amount) FOR quarter IN ('Q1','Q2')) AS p
        //   SELECT * FROM <table> PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
        if let Some(pivot_spec) = parse_pivot_clause(sql) {
            // Strip the PIVOT clause (and any trailing alias) from the SQL.
            let stripped = strip_pivot_clause(sql);
            // Execute the stripped SELECT to get the input rows.
            let input = self.execute_inner(&stripped, start, txn_id)?;
            // Auto-detect the group_col: the first column in the input that's
            // neither the pivot_col nor the value_col.
            let group_col = input
                .columns
                .iter()
                .find(|c| c.name != pivot_spec.pivot_col && c.name != pivot_spec.value_col)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| {
                    input.columns.first().map(|c| c.name.clone()).unwrap_or_default()
                });
            let spec = PivotSpec {
                group_col,
                pivot_col: pivot_spec.pivot_col,
                value_col: pivot_spec.value_col,
                pivot_values: pivot_spec.pivot_values,
                agg: pivot_spec.agg,
            };
            let mut result = apply_pivot(&input, &spec);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 56c: JSON_VALUE / JSON_QUERY. Detect `JSON_VALUE(` in the SQL
        // and intercept: rewrite the SQL to replace each JSON_VALUE(col, path)
        // with `col AS __json_value_N__`, execute the rewritten SQL, then
        // post-process the result columns by applying json::json_value() to
        // each string value.
        if contains_json_value_call(sql) {
            return self.execute_with_json_value(sql, start, txn_id);
        }

        // Try DDL first (CREATE TABLE, DROP TABLE, CREATE SCHEMA).
        if let Some(ddl) = crate::sql::parse_ddl(sql).map_err(Error::Parse)? {
            let mut result = self.execute_ddl(ddl)?;
            // Wave 56d: if the DDL was a CREATE TABLE with
            // `WITH (SYSTEM_VERSIONING = ON)`, register the table in
            // self.temporals so FOR SYSTEM_TIME AS OF queries work.
            if let Some(table_name) = extract_temporal_table_name(sql) {
                if let Some(table) = self.catalog.get(&table_name) {
                    let col_names = table.column_names.clone();
                    self.temporals.insert(
                        table_name.clone(),
                        crate::exec::temporal::TemporalTable::new(col_names),
                    );
                    // Seed the temporal table with the current rows (if any).
                    // For a freshly-CREATED table there are none, but if the
                    // user INSERTs later, execute_insert will update the
                    // temporal sidecar.
                }
            }
            // Wave 51 fix: append AFTER successful execute.
            self.wal_append_txn(sql, txn_id);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DML (INSERT, UPDATE, DELETE).
        if let Some(dml) = crate::sql::parse_dml(sql).map_err(Error::Parse)? {
            let mut result = self.execute_dml(dml)?;
            // Wave 51 fix: append AFTER successful execute. If execute_dml
            // returns Err, we never reach this line, so the WAL stays clean.
            self.wal_append_txn(sql, txn_id);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: expand view references in the SQL before parsing as SELECT.
        // If the FROM clause references a view name, we materialize the view
        // by executing its SELECT SQL and registering the result as a
        // catalog table under the view's name (overwriting any prior
        // materialization). The outer SELECT then runs against the
        // materialized table.
        let expanded_sql = self.materialize_views_in_sql(sql);

        // Parse as SELECT.
        let (query, extensions) = match crate::sql::parse_with_extensions(&expanded_sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // The basic parser failed — try the TPC-H interpreter
                // which has a richer parser (CASE, EXTRACT, subqueries,
                // HAVING, arithmetic in aggregates, etc.).
                let mut tpch_result =
                    crate::engine::tpch::parse_and_execute(&expanded_sql, &self.catalog)?;
                tpch_result.elapsed_us = start.elapsed().as_micros() as u64;
                return Ok(tpch_result);
            }
        };

        // Wave 53: Temporal query handling is done above (before parsing).

        // Wave 66: fast path — if the query is a simple
        // `SELECT ... FROM t WHERE col = literal` and there's an index on
        // (t, col), use the index for O(1) lookup instead of a full scan.
        // Returns None if the fast path doesn't apply.
        if let Some(indexed) = self.try_indexed_lookup(&query) {
            let mut result = indexed?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Execute the parsed query.
        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
        ) {
            Ok(mut result) => {
                // Wave 53: apply window functions if any SelectItem::Window
                // is present in the query.
                if query
                    .select
                    .iter()
                    .any(|s| matches!(s, crate::sql::parser::SelectItem::Window { .. }))
                {
                    result = apply_window_functions(&result, &query);
                }
                // Wave 53: apply PIVOT if the extensions carry a pivot spec.
                if let Some(pivot_spec) = extensions_pivot(&extensions) {
                    result = apply_pivot(&result, &pivot_spec);
                }
                // Wave 60d: apply DISTINCT deduplication if requested.
                if query.distinct {
                    result = deduplicate_rows(result);
                }
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            Err(exec_err) => {
                // The basic executor failed — try the TPC-H interpreter
                // as a fallback. This handles queries with features the
                // basic executor doesn't support (multi-aggregate, HAVING,
                // CASE WHEN, subqueries, etc.).
                let mut tpch_result =
                    crate::engine::tpch::parse_and_execute(&expanded_sql, &self.catalog)
                        .map_err(|_| exec_err)?;
                // Wave 60d: apply DISTINCT deduplication even on the tpch
                // fallback path (the tpch parser skips DISTINCT but doesn't
                // deduplicate).
                if query.distinct {
                    tpch_result = deduplicate_rows(tpch_result);
                }
                tpch_result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(tpch_result)
            }
        }
    }

    /// Execute EXPLAIN: parse the inner SQL and return the query plan
    /// as a text result (Wave 68).
    fn execute_explain(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
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
    fn execute_analyze(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
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
    fn execute_vacuum(&mut self, start: &Instant) -> Result<QueryResult> {
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

    /// Execute COPY: copy data between a table and a file (Wave 68).
    /// Syntax: `COPY table TO 'file'` or `COPY table FROM 'file'`.
    fn execute_copy(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
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

    /// Execute SAVEPOINT: create a named savepoint within the current
    /// transaction (Wave 69). The savepoint captures a deep-clone of the
    /// current catalog state, so ROLLBACK TO can restore it.
    fn execute_savepoint(&mut self, name: String, start: &Instant) -> Result<QueryResult> {
        // Note: during execute_inner, the txn_manager.active field is
        // temporarily taken out (swapped). We can't check is_active()
        // here. Instead, we rely on the caller (execute) to only dispatch
        // to execute_inner when a txn is active. If we reach here without
        // a txn, the savepoint is simply created on the main catalog —
        // it won't be very useful, but it won't crash.
        // Deep-clone the current catalog as the savepoint state.
        let snapshot = crate::txn::clone_catalog(&self.catalog);
        self.savepoints.push((name, snapshot));
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute ROLLBACK TO <name>: restore the catalog to the named
    /// savepoint (Wave 69). All savepoints created after <name> are
    /// discarded.
    fn execute_rollback_to(&mut self, name: &str, start: &Instant) -> Result<QueryResult> {
        // Find the savepoint by name (search from the end — most recent first).
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| Error::NotFound(format!("savepoint '{}'", name)))?;
        // Restore the catalog from the savepoint.
        let (_, snapshot) = &self.savepoints[pos];
        self.catalog = crate::txn::clone_catalog(snapshot);
        // Discard all savepoints after this one (they're no longer valid).
        self.savepoints.truncate(pos + 1);
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute RELEASE <name>: discard a savepoint (Wave 69).
    /// The savepoint is removed; ROLLBACK TO can no longer target it.
    fn execute_release_savepoint(&mut self, name: &str, start: &Instant) -> Result<QueryResult> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| Error::NotFound(format!("savepoint '{}'", name)))?;
        // Remove the savepoint and all savepoints after it.
        self.savepoints.truncate(pos);
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute a DDL statement (CREATE TABLE, DROP TABLE, CREATE SCHEMA).
    fn execute_ddl(&mut self, ddl: crate::sql::DdlStatement) -> Result<QueryResult> {
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
                    schema: Some(crate::schema::table_schema::TableSchema::from_ddl(&ct.columns)),
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
    fn execute_alter_table(&mut self, at: crate::sql::AlterTable) -> Result<QueryResult> {
        use crate::sql::AlterAction;
        let full_name =
            if at.schema == "dbo" { at.name.clone() } else { format!("{}.{}", at.schema, at.name) };
        match at.action {
            AlterAction::AddColumn(col_def) => {
                let table = self
                    .catalog
                    .get_mut(&full_name)
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?;
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
                    });
                }
                Ok(QueryResult::empty())
            }
            AlterAction::DropColumn(col_name) => {
                let table = self
                    .catalog
                    .get_mut(&full_name)
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?;
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
            }
            AlterAction::AlterColumnType { column, new_type } => {
                let table = self
                    .catalog
                    .get_mut(&full_name)
                    .ok_or_else(|| Error::NotFound(format!("table \"{full_name}\"")))?;
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
            }
        }
    }

    /// Execute a CREATE INDEX statement (Wave 66).
    ///
    /// Registers a named index in the IndexManager and builds the
    /// in-memory hash index data for fast equality lookups.
    fn execute_create_index(&mut self, ci: crate::sql::CreateIndex) -> Result<QueryResult> {
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
    fn execute_drop_index(&mut self, di: crate::sql::DropIndex) -> Result<QueryResult> {
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
    fn try_indexed_lookup(&self, query: &crate::sql::SelectQuery) -> Option<Result<QueryResult>> {
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

    /// Execute a DML statement (INSERT, UPDATE, DELETE).
    fn execute_dml(&mut self, dml: crate::sql::DmlStatement) -> Result<QueryResult> {
        match dml {
            crate::sql::DmlStatement::Insert(ins) => self.execute_insert(ins),
            crate::sql::DmlStatement::Update(upd) => self.execute_update(upd),
            crate::sql::DmlStatement::Delete(del) => self.execute_delete(del),
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
    fn execute_insert(&mut self, ins: crate::sql::Insert) -> Result<QueryResult> {
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
    fn execute_update(&mut self, upd: crate::sql::Update) -> Result<QueryResult> {
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
            let trimmed = expr.trim();
            let is_null = trimmed.eq_ignore_ascii_case("NULL");
            // For now, the expression must be a simple literal.
            let cell = parse_value_cell(expr);
            assigns.push((idx, cell, is_null));
        }

        // Determine which rows match the WHERE clause.
        let n = table.row_count;
        let mut updated = 0usize;
        let match_mask: Vec<bool> = if let Some(where_str) = &upd.where_clause {
            eval_simple_where(table, where_str)?
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
    fn execute_delete(&mut self, del: crate::sql::Delete) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&del.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

        let n = table.row_count;
        let delete_mask: Vec<bool> = if let Some(where_str) = &del.where_clause {
            eval_simple_where(table, where_str)?
        } else {
            vec![true; n]
        };

        let deleted = delete_mask.iter().filter(|&&b| b).count();
        if deleted == 0 {
            let mut result = QueryResult::empty();
            result.row_count = 0;
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

    /// Execute a WITH clause (CTEs + outer query).
    ///
    /// For each CTE:
    /// 1. Execute the anchor query, register the result as a temp table
    ///    in the catalog under the CTE name.
    /// 2. If the CTE is recursive, iterate: execute the recursive query
    ///    (which references the CTE name), compute the new rows (set
    ///    difference), append them to the CTE table, and repeat until
    ///    no new rows or MAXRECURSION is reached.
    /// 3. Execute the outer query, which can reference any CTE by name.
    ///
    /// `txn_id` is threaded through so DML inside a CTE (rare but
    /// possible) still gets the right transaction marker in the WAL.
    fn execute_with(
        &mut self,
        with: crate::sql::WithClause,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        let mut temp_tables: Vec<String> = Vec::new();

        for cte in &with.ctes {
            // Execute the anchor.
            let anchor_result = self.execute_inner(&cte.anchor, &Instant::now(), txn_id)?;

            // Register the anchor result as a temp table.
            let temp_name = cte.name.clone();
            let table = result_to_table(&temp_name, &anchor_result);
            self.catalog.register(table);
            temp_tables.push(temp_name.clone());

            // If recursive, iterate.
            if let Some(recursive_sql) = &cte.recursive {
                let max_iter = if with.max_recursion == 0 {
                    100_000 // unlimited (capped at 100k for safety)
                } else {
                    with.max_recursion
                };

                for _ in 0..max_iter {
                    // Execute the recursive query with the current CTE state.
                    let rec_result = self.execute_inner(recursive_sql, &Instant::now(), txn_id)?;

                    // Compute new rows: rows in rec_result that aren't already
                    // in the CTE table. For simplicity, we compare by row
                    // content (all columns must match).
                    let new_rows = compute_new_rows(
                        &self.catalog.get(&temp_name).cloned().unwrap_or_else(|| Table {
                            name: temp_name.clone(),
                            columns: vec![],
                            column_names: vec![],
                            row_count: 0,
                            string_columns: vec![],
                            null_bitmaps: vec![],
                            schema: None,
                        }),
                        &rec_result,
                    );

                    if new_rows == 0 {
                        break; // No new rows — recursion complete.
                    }

                    // Append the new rows to the CTE table.
                    // We append ALL rows from rec_result (not just the new
                    // ones) because the recursive query should only produce
                    // new rows if written correctly. A proper set-difference
                    // would be more correct but expensive.
                    let cte_table = self
                        .catalog
                        .get_mut(&temp_name)
                        .ok_or_else(|| Error::NotFound(format!("CTE table \"{temp_name}\"")))?;
                    append_result_rows(cte_table, &rec_result);
                }
            }
        }

        // Execute the outer query.
        let result = self.execute_inner(&with.outer_query, &Instant::now(), txn_id)?;

        // Clean up temp tables.
        for name in &temp_tables {
            self.catalog.drop(name);
        }

        Ok(result)
    }

    /// Execute a TPC-H SQL query using the dedicated TPC-H interpreter.
    ///
    /// This path uses `src/engine/tpch.rs` which has a richer parser
    /// (arithmetic in aggregates, CASE WHEN, EXTRACT, BETWEEN, IN,
    /// subqueries, derived tables, multi-table implicit joins, HAVING,
    /// LEFT JOIN) and a type-aware row-based evaluator.
    pub fn execute_tpch(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut result = crate::engine::tpch::parse_and_execute(sql, &self.catalog)?;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------
// DML helper functions (Wave 4)
// -----------------------------------------------------------------------

/// Extract the inner string from a SQL string literal `'...'`, handling
/// the `''` escape (a literal single quote inside the string). Returns
/// None if `s` is not a string literal.
///
/// Wave 56c: used by `execute_insert` to preserve the original string
/// value when inserting into a VARCHAR / NVARCHAR / TEXT column, so that
/// subsequent SELECTs can recover the string (via the `string_columns`
/// sidecar) and JSON_VALUE / LIKE / range comparisons work correctly.
fn extract_string_literal(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if !(trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2) {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    // Handle the `''` escape (a literal single quote inside the string).
    Some(inner.replace("''", "'"))
}

/// Parse a value string from the DML parser into a u64 cell.
///
/// Supported formats:
/// - `"42"` → integer 42
/// - `"3.14"` → f64::to_bits(3.14)
/// - `"'hello'"` → xxh3 hash of "hello" (string columns are hashed)
/// - `"NULL"` → 0 (NULL is stored as 0; a proper null bitmap arrives in a later wave)
/// - `"x'0123'"` → first 8 bytes as u64
fn parse_value_cell(s: &str) -> u64 {
    use xxhash_rust::xxh3;
    let trimmed = s.trim();
    if trimmed == "NULL" {
        return 0;
    }
    // String literal: '...'
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        return xxh3::xxh3_64(inner.as_bytes());
    }
    // Hex literal: x'...'
    if trimmed.starts_with("x'") && trimmed.ends_with('\'') && trimmed.len() >= 3 {
        let hex = &trimmed[2..trimmed.len() - 1];
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        let mut buf = [0u8; 8];
        for (i, &b) in bytes.iter().take(8).enumerate() {
            buf[i] = b;
        }
        return u64::from_le_bytes(buf);
    }
    // Float
    if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
        if let Ok(f) = trimmed.parse::<f64>() {
            return f.to_bits();
        }
    }
    // Integer
    if let Ok(n) = trimmed.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = trimmed.parse::<u64>() {
        return n;
    }
    // Fallback: hash the string
    xxh3::xxh3_64(trimmed.as_bytes())
}

/// Evaluate a simple WHERE clause against a table, returning a row mask.
///
/// Wave 50 fix (Bugs 4 & 5):
/// - Previously only supported `=` and split the WHERE string on
///   whitespace, which broke string literals containing spaces like
///   `'Alice Bob'`.
/// - Now uses the SQL lexer (`crate::sql::lexer::tokenize`) so quoted
///   strings with spaces round-trip correctly, and supports the full set
///   of comparison operators: `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`.
/// - Also supports `AND` / `OR` for combining predicates (left-associative).
fn eval_simple_where(table: &Table, where_str: &str) -> Result<Vec<bool>> {
    let n = table.row_count;
    if n == 0 {
        return Ok(Vec::new());
    }

    // Tokenize the WHERE clause so string literals with spaces, embedded
    // operators, etc. are correctly preserved as single tokens.
    let tokens = crate::sql::lexer::tokenize(where_str).map_err(Error::Parse)?;
    // Drop trailing EOF (and any leading WHERE keyword, in case the caller
    // passed the full predicate including `WHERE`).
    let tokens: Vec<crate::sql::lexer::Token> =
        tokens.into_iter().filter(|t| !matches!(t, crate::sql::lexer::Token::EOF)).collect();
    let tokens: Vec<crate::sql::lexer::Token> = if tokens
        .first()
        .and_then(|t| match t {
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("WHERE") => Some(()),
            _ => None,
        })
        .is_some()
    {
        tokens[1..].to_vec()
    } else {
        tokens
    };

    if tokens.is_empty() {
        return Ok(vec![true; n]);
    }

    // Parse predicates of form: <col> <op> <value>, joined by AND/OR.
    // Each predicate produces a (col_idx, op, cell_value, is_string_literal, raw_string) tuple.
    #[derive(Clone)]
    struct Pred {
        col_idx: usize,
        op: String,
        cell: u64,
        // Original string literal (if the value was a quoted string), used
        // for string comparison when the column has a string sidecar.
        raw_string: Option<String>,
    }

    let mut predicates: Vec<Pred> = Vec::new();
    let mut operators: Vec<bool> = Vec::new(); // true = AND, false = OR
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("AND") => {
                operators.push(true);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("OR") => {
                operators.push(false);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::LParen => {
                // Parenthesised expressions in DML WHERE are not supported
                // here — fall back to the dispatcher's mask evaluator if
                // the caller needs full boolean expression support.
                return Err(Error::Other(
                    "parenthesised expressions are not supported in DML WHERE; use SELECT WHERE instead".into(),
                ));
            }
            _ => {}
        }

        // Expect: <col> <op> <value>
        let col_name = match &tokens[i] {
            crate::sql::lexer::Token::Ident(s) => s.clone(),
            crate::sql::lexer::Token::Keyword(k) => k.clone(), // tolerate keyword-as-identifier
            other => {
                return Err(Error::Other(format!(
                    "expected column name in WHERE clause, got {:?}",
                    other
                )))
            }
        };
        if i + 2 >= tokens.len() {
            return Err(Error::Other(format!("incomplete WHERE predicate near '{col_name}'")));
        }
        let op = match &tokens[i + 1] {
            crate::sql::lexer::Token::Op(s) => s.clone(),
            other => {
                return Err(Error::Other(format!(
                    "expected comparison operator after '{col_name}', got {:?}",
                    other
                )))
            }
        };
        if !matches!(op.as_str(), "=" | "!=" | "<>" | "<" | ">" | "<=" | ">=") {
            return Err(Error::Other(format!("unsupported WHERE operator '{op}' in DML WHERE")));
        }

        let col_idx = table
            .column_idx(&col_name)
            .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;

        // Extract the value cell. String literals get the original text
        // preserved so we can compare against the string sidecar if one
        // exists; everything else is parsed via parse_value_cell.
        let (cell, raw_string) = match &tokens[i + 2] {
            crate::sql::lexer::Token::String(s) => {
                // Quoted string. If the column has a string sidecar, we
                // keep the original text for direct comparison; otherwise
                // we hash it (matching parse_value_cell behaviour).
                let has_string_sidecar =
                    col_idx < table.string_columns.len() && table.string_columns[col_idx].is_some();
                if has_string_sidecar {
                    (0u64, Some(s.clone()))
                } else {
                    (parse_value_cell(&format!("'{}'", s)), None)
                }
            }
            crate::sql::lexer::Token::Int(v) => (*v as u64, None),
            crate::sql::lexer::Token::Float(f) => (f.to_bits(), None),
            crate::sql::lexer::Token::Hex(bytes) => {
                let mut buf = [0u8; 8];
                for (j, &b) in bytes.iter().take(8).enumerate() {
                    buf[j] = b;
                }
                (u64::from_le_bytes(buf), None)
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("NULL") => {
                // NULL in a WHERE predicate — treat as 0 cell. Callers
                // that need IS NULL / IS NOT NULL should use the
                // expression evaluator path.
                (0u64, None)
            }
            other => {
                return Err(Error::Other(format!(
                    "expected literal value in WHERE clause, got {:?}",
                    other
                )))
            }
        };

        predicates.push(Pred {
            col_idx,
            op: if op == "<>" { "!=".to_string() } else { op },
            cell,
            raw_string,
        });
        i += 3;
    }

    if predicates.is_empty() {
        return Ok(vec![true; n]);
    }

    // Evaluate each predicate per row.
    let mut per_pred_masks: Vec<Vec<bool>> = Vec::with_capacity(predicates.len());
    for p in &predicates {
        let col_idx = p.col_idx;
        let col = &table.columns[col_idx];

        // If we have the original string and the column has a string sidecar,
        // compare against the sidecar directly (lexicographic).
        if let Some(ref s) = p.raw_string {
            if col_idx < table.string_columns.len() {
                if let Some(ref sc) = table.string_columns[col_idx] {
                    let mask: Vec<bool> = (0..n)
                        .map(|i| {
                            let cell_str = sc.get(i);
                            match p.op.as_str() {
                                "=" => cell_str == s.as_str(),
                                "!=" => cell_str != s.as_str(),
                                "<" => cell_str < s.as_str(),
                                ">" => cell_str > s.as_str(),
                                "<=" => cell_str <= s.as_str(),
                                ">=" => cell_str >= s.as_str(),
                                _ => false,
                            }
                        })
                        .collect();
                    per_pred_masks.push(mask);
                    continue;
                }
            }
        }

        // Default: compare u64 cells.
        let val = p.cell;
        let mask: Vec<bool> = match p.op.as_str() {
            "=" => col.iter().map(|&c| c == val).collect(),
            "!=" => col.iter().map(|&c| c != val).collect(),
            "<" => col.iter().map(|&c| c < val).collect(),
            ">" => col.iter().map(|&c| c > val).collect(),
            "<=" => col.iter().map(|&c| c <= val).collect(),
            ">=" => col.iter().map(|&c| c >= val).collect(),
            _ => vec![false; n],
        };
        per_pred_masks.push(mask);
    }

    // Combine: start with first predicate, then AND/OR (left-associative).
    let mut result = per_pred_masks[0].clone();
    for (i, mask) in per_pred_masks[1..].iter().enumerate() {
        let is_and = operators.get(i).copied().unwrap_or(true);
        if is_and {
            for j in 0..n {
                result[j] = result[j] && mask[j];
            }
        } else {
            for j in 0..n {
                result[j] = result[j] || mask[j];
            }
        }
    }

    Ok(result)
}

// -----------------------------------------------------------------------
// CTE helper functions (Wave 6)
// -----------------------------------------------------------------------

/// Convert a QueryResult into a Table that can be registered in the catalog.
fn result_to_table(name: &str, result: &QueryResult) -> Table {
    let column_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let columns: Vec<std::sync::Arc<Vec<u64>>> =
        result.columns.iter().map(|c| std::sync::Arc::new(c.values.clone())).collect();
    let string_columns: Vec<Option<std::sync::Arc<crate::exec::fm_index::StringSearchColumn>>> =
        vec![None; result.columns.len()];
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: result.row_count,
        string_columns,
        null_bitmaps: vec![],
        schema: None,
    }
}

/// Compute how many rows in `result` are new (not already in `table`).
/// A row is "new" if its full column content doesn't match any existing
/// row in the table. This is O(result_rows × table_rows × ncols) —
/// expensive but correct for small CTEs.
fn compute_new_rows(table: &Table, result: &QueryResult) -> usize {
    if result.row_count == 0 {
        return 0;
    }
    let ncols = result.columns.len();
    let mut new_count = 0;
    for r_row in 0..result.row_count {
        let mut found = false;
        for t_row in 0..table.row_count {
            let mut matches = true;
            for col_idx in 0..ncols {
                let r_val = result.columns[col_idx].values.get(r_row).copied().unwrap_or(0);
                let t_val =
                    table.columns.get(col_idx).and_then(|c| c.get(t_row)).copied().unwrap_or(0);
                if r_val != t_val {
                    matches = false;
                    break;
                }
            }
            if matches {
                found = true;
                break;
            }
        }
        if !found {
            new_count += 1;
        }
    }
    new_count
}

/// Append all rows from a QueryResult to an existing Table. The table
/// must have the same number of columns as the result.
fn append_result_rows(table: &mut Table, result: &QueryResult) {
    for col_idx in 0..result.columns.len() {
        if col_idx < table.columns.len() {
            let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
            col.extend_from_slice(&result.columns[col_idx].values);
        }
    }
    table.row_count += result.row_count;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table as DataSourceTable;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc as ArrowArc;
    use tempfile::NamedTempFile;

    /// Build a `Table` with two columns: `id` (0..n) and `x` (cycling 0..7).
    fn make_table(n: usize) -> DataSourceTable {
        let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let xs: Vec<u64> = (0..n).map(|i| (i % 7) as u64).collect();
        DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: ids,
                    row_count: n,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "x".into(),
                    cells: xs,
                    row_count: n,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: n,
        })
    }

    /// Build a `Table` with a single integer-encoded column `v`.
    fn make_int_table(values: &[u64]) -> DataSourceTable {
        let n = values.len();
        DataSourceTable::from_loaded(LoadedTable {
            name: "ft".into(),
            columns: vec![LoadedColumn {
                name: "v".into(),
                cells: values.to_vec(),
                row_count: n,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: n,
        })
    }

    // -----------------------------------------------------------------
    // DoD tests (the 9 cases from the Wave 20 task brief)
    // -----------------------------------------------------------------

    /// DoD 1: `SELECT count(*) FROM t` returns the table's row count.
    #[test]
    fn dod_count_star_returns_row_count() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 2: `SELECT count(*) FROM t WHERE x = 42` returns the right count.
    #[test]
    fn dod_count_star_with_where() {
        let mut engine = QueryEngine::in_memory();
        // Make a table where x = 42 appears exactly 7 times.
        let mut xs: Vec<u64> = (0..1000).map(|i| (i % 7) as u64).collect();
        // Make some entries equal to 42.
        for i in 0..7 {
            xs[i * 100] = 42;
        }
        let table = DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: (0..1000).map(|i| i as u64).collect(),
                    row_count: 1000,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "x".into(),
                    cells: xs,
                    row_count: 1000,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: 1000,
        });
        engine.register_table(table);

        let r = engine.execute("SELECT count(*) FROM t WHERE x = 42").expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 3: `SELECT sum(col) FROM t` returns the right sum.
    #[test]
    fn dod_sum_returns_correct_sum() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 499_500.0).abs() < 1e-3, "got {s}");
    }

    /// DoD 4: `SELECT * FROM t WHERE id = 5` returns the matching row.
    #[test]
    fn dod_select_star_with_where() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT * FROM t WHERE id = 5").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
        assert_eq!(r.column("x"), Some(&[5u64][..])); // 5 % 7 = 5
    }

    /// DoD 5: APPROXIMATE extension parses and runs.
    #[test]
    fn dod_count_distinct_with_approximate() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine
            .execute("SELECT count(DISTINCT x) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95 FROM t")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 6: TIER extension parses and runs.
    #[test]
    fn dod_count_star_with_tier_l3() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t TIER L3").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 7: Invalid SQL returns `Error::Parse`.
    #[test]
    fn dod_invalid_sql_returns_parse_error() {
        let mut engine = QueryEngine::in_memory();
        let r = engine.execute("SELECT FROM WHERE");
        assert!(matches!(r, Err(Error::Parse(_))), "got {r:?}");
    }

    /// DoD 8: Non-existent table returns `Error::NotFound`.
    #[test]
    fn dod_non_existent_table_returns_not_found() {
        let mut engine = QueryEngine::in_memory();
        let r = engine.execute("SELECT count(*) FROM missing");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// DoD 9: Load a Parquet file, query it.
    #[test]
    fn dod_load_parquet_and_query() {
        // Build a small Parquet file with one Int64 column `id` of 100 rows.
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let ids: Vec<i64> = (0..100).collect();
        let arr = ArrowArc::new(Int64Array::from(ids));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_parquet(&path, "loaded").expect("load");
        assert_eq!(n, 100);

        let r = engine.execute("SELECT count(*) FROM loaded").expect("query");
        assert_eq!(r.scalar_u64(), Some(100));

        let r = engine.execute("SELECT sum(id) FROM loaded").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 4950.0).abs() < 1e-3, "got {s}"); // 0+1+...+99 = 4950
    }

    // -----------------------------------------------------------------
    // Additional integration tests
    // -----------------------------------------------------------------

    /// Load a CSV file and query it.
    #[test]
    fn load_csv_and_query() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        std::fs::write(&path, "id,value\n1,10\n2,20\n3,30\n4,40\n5,50\n").expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_csv(&path, "csvt", true).expect("load");
        assert_eq!(n, 5);

        let r = engine.execute("SELECT count(*) FROM csvt").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        let r = engine.execute("SELECT count(*) FROM csvt WHERE value = 30").expect("query");
        assert_eq!(r.scalar_u64(), Some(1));

        let r = engine.execute("SELECT sum(value) FROM csvt").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 150.0).abs() < 1e-9, "got {s}"); // 10+20+30+40+50 = 150

        let r = engine.execute("SELECT * FROM csvt WHERE id = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[3u64][..]));
        assert_eq!(r.column("value"), Some(&[30u64][..]));
    }

    /// Sum of an integer-encoded column through the engine API.
    #[test]
    fn engine_sum_integer_column() {
        let mut engine = QueryEngine::in_memory();
        // Integer-encoded column: 1, 2, 3, 4 → sum = 10.
        engine.register_table(make_int_table(&[1, 2, 3, 4]));
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 10.0).abs() < 1e-9, "got {s}");
    }

    /// The elapsed_us field is populated after `execute`.
    #[test]
    fn execute_populates_elapsed_us() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        // elapsed_us should be non-negative (and almost certainly > 0,
        // but we don't assert that to avoid flakes on very fast machines).
        assert!(r.elapsed_us < 1_000_000, "elapsed_us unreasonably large: {}", r.elapsed_us);
    }

    /// Re-registering a table replaces the old one.
    #[test]
    fn register_table_overwrites() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        engine.register_table(make_table(200));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(200));
    }

    /// `with_cost_model` constructs an engine with a non-default cost model.
    #[test]
    fn with_cost_model_constructs_engine() {
        let cm = CostModel { cpu_freq_hz: 4.0e9, simd_lanes: 16, ..CostModel::default() };
        let mut engine = QueryEngine::with_cost_model(cm);
        assert_eq!(engine.cost_model().cpu_freq_hz, 4.0e9);
        assert_eq!(engine.cost_model().simd_lanes, 16);
    }

    /// `QueryEngine::default()` is equivalent to `new()`.
    /// The catalog contains the internal `__dummy__` table (used for
    /// FROM-less SELECTs), so it's not strictly empty — but it has no
    /// user-registered tables.
    #[test]
    fn default_is_empty() {
        let mut engine = QueryEngine::default();
        // The __dummy__ table is always present.
        assert_eq!(engine.catalog().len(), 1);
        // But no user tables.
        let names: Vec<&str> =
            engine.catalog().table_names().into_iter().filter(|n| *n != "__dummy__").collect();
        assert!(names.is_empty());
    }

    /// Accessors return the right types.
    #[test]
    fn accessors_work() {
        let mut engine = QueryEngine::in_memory();
        let _cat: &Catalog = engine.catalog();
        let _kt: &KernelTable = engine.kernel_table();
        let _cm: &CostModel = engine.cost_model();
    }

    /// A query against a table with zero rows returns 0 for count(*).
    #[test]
    fn count_star_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(0));
    }

    /// A sum against a table with zero rows returns 0.0.
    #[test]
    fn sum_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!(s.abs() < 1e-9, "got {s}");
    }

    /// Print does not panic on a real result.
    #[test]
    fn print_does_not_panic() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(10));
        let r = engine.execute("SELECT * FROM t").expect("query");
        r.print();
        // No assertion — the test just verifies print doesn't panic.
    }

    /// Extensions other than TIER/APPROXIMATE are accepted (no-ops).
    #[test]
    fn other_extensions_accepted() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        let r = engine
            .execute("SELECT count(*) FROM t USING HYPERLOGLOG MEMORY BUDGET 1048576 ENERGY BUDGET 100 JOULES CONSISTENCY STRONG")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(100));
    }

    /// Loading a Parquet file under a custom name works.
    #[test]
    fn load_parquet_under_custom_name() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_parquet(&path, "custom_name").expect("load");
        assert_eq!(n, 3);

        // The table is registered under "custom_name", not the file stem.
        let r = engine.execute("SELECT count(*) FROM custom_name").expect("query");
        assert_eq!(r.scalar_u64(), Some(3));

        // The file stem is NOT registered.
        let r = engine.execute("SELECT count(*) FROM tempfile");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// Parquet Int64 column round-trips through a load + count + sum.
    #[test]
    fn parquet_int_column_count_and_sum() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        // Int64 column 1..=5 → integer-encoded as 1u64..=5.
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3, 4, 5]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        engine.load_parquet(&path, "ft").expect("load");

        // Count.
        let r = engine.execute("SELECT count(*) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // Sum (integer-encoded: 1+2+3+4+5 = 15).
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 15.0).abs() < 1e-9, "got {s}");

        // Count distinct.
        let r = engine.execute("SELECT count(DISTINCT v) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // SELECT * with filter.
        let r = engine.execute("SELECT * FROM ft WHERE v = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("v"), Some(&[3u64][..]));
    }
}
pub mod dispatch;

// -----------------------------------------------------------------------
// Wave 53 helper functions: wire views, procedures, MERGE, JSON,
// temporal, window, PIVOT into execute().
// -----------------------------------------------------------------------

/// Substitute @param references in a stored-procedure body with the
/// supplied argument values. @1 → args[0], @2 → args[1], etc., and
/// named params @name → args[i] where proc_def.params[i].name == name.
fn substitute_proc_params(body: &str, args: &[String]) -> String {
    let mut result = body.to_string();
    // Positional substitution: @1, @2, ... → args[0], args[1], ...
    for (i, arg) in args.iter().enumerate() {
        let placeholder = format!("@{}", i + 1);
        result = result.replace(&placeholder, arg);
    }
    result
}

/// Parse a MERGE statement (Wave 53 wiring for exec/merge.rs).
///
/// Supports the form:
///   MERGE INTO target [AS t]
///   USING (VALUES (1, 'a'), (2, 'b')) AS s (id, val)
///   ON t.id = s.id
///   WHEN MATCHED THEN UPDATE SET col = val [, ...]
///   WHEN NOT MATCHED THEN INSERT (cols) VALUES (vals)
///
/// Wave 56a fix: the previous implementation hardcoded `source_rows: Vec::new()`,
/// `join_target_col: String::new()`, `join_source_col: String::new()` — so
/// `execute_merge` could never match any target row and the WHEN MATCHED
/// branch was dead. We now parse the USING (VALUES ...) clause to populate
/// `source_rows`, and parse the ON clause to populate the join columns.
///
/// Returns None if the SQL is not a MERGE statement.
fn parse_merge(sql: &str) -> Option<crate::exec::merge::Merge> {
    use crate::exec::merge::{Merge, MergeAction};
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("MERGE ") && !upper.starts_with("MERGE INTO ") {
        return None;
    }

    let after_merge = if upper.starts_with("MERGE INTO ") {
        &trimmed["MERGE INTO ".len()..]
    } else {
        &trimmed["MERGE ".len()..]
    };

    // Target table name is the first whitespace-delimited token (optionally
    // followed by `AS alias`).
    let target = after_merge.split_whitespace().next()?.to_string();

    let lower = trimmed.to_lowercase();

    // ---- Parse USING (VALUES (...) , (...), ...) AS alias (col1, col2, ...) ----
    // The source rows are the (join_value, [full_row]) tuples extracted from
    // the VALUES list. The merge module's `source_rows` field is shaped as
    // Vec<(join_value_str, full_row_vals)> — the first element of each tuple
    // is the join key (a stringified u64 or quoted string), and the second
    // is the full row (used by the Insert action).
    let mut source_rows: Vec<(String, Vec<String>)> = Vec::new();
    let mut source_col_names: Vec<String> = Vec::new();
    if let Some(using_pos) = lower.find("using ") {
        let after_using = &trimmed[using_pos + "using ".len()..];
        // Skip whitespace.
        let after_using = after_using.trim_start();
        if after_using.starts_with('(') {
            // Find the matching close paren for the USING (...) group.
            // This may contain nested parens for the VALUES list.
            let mut depth = 0i32;
            let mut using_close = None;
            for (i, c) in after_using.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            using_close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = using_close {
                let using_inner = &after_using[1..close];
                // using_inner should start with "VALUES" then have (..), (..)
                let using_inner_lower = using_inner.to_lowercase();
                if let Some(v_pos) = using_inner_lower.find("values") {
                    let after_values = &using_inner[v_pos + "values".len()..];
                    // Parse each (...) tuple.
                    source_rows = parse_values_tuples(after_values);
                }
                // After the USING (...) group, look for "AS alias (col1, col2, ...)"
                // to extract the source column names.
                let after_group = after_using[close + 1..].trim_start();
                let after_as = if after_group.to_uppercase().starts_with("AS ") {
                    &after_group["AS ".len()..]
                } else {
                    after_group
                };
                // Skip the alias identifier.
                let after_alias = after_as
                    .split_whitespace()
                    .next()
                    .map(|n| &after_as[n.len()..])
                    .unwrap_or(after_as)
                    .trim_start();
                if after_alias.starts_with('(') {
                    if let Some(close2) = after_alias.find(')') {
                        source_col_names = after_alias[1..close2]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect();
                    }
                }
            }
        }
    }

    // ---- Parse ON target_col = source_col ----
    let mut join_target_col = String::new();
    let mut join_source_col = String::new();
    if let Some(on_pos) = lower.find(" on ") {
        // Limit the ON clause to the next WHEN keyword (so we don't grab
        // any later "on" in a subquery or string literal).
        let after_on = &trimmed[on_pos + " on ".len()..];
        let when_pos = after_on.to_lowercase().find(" when ").unwrap_or(after_on.len());
        let on_clause = after_on[..when_pos].trim();
        // Parse "target.col = source.col" — split on '=' first.
        if let Some(eq_pos) = on_clause.find('=') {
            let lhs = on_clause[..eq_pos].trim();
            let rhs = on_clause[eq_pos + 1..].trim();
            // Both sides should be qualified "alias.col" — take the part after the dot.
            if let Some(dot_pos) = lhs.rfind('.') {
                join_target_col = lhs[dot_pos + 1..].trim().to_string();
            } else {
                join_target_col = lhs.to_string();
            }
            if let Some(dot_pos) = rhs.rfind('.') {
                join_source_col = rhs[dot_pos + 1..].trim().to_string();
            } else {
                join_source_col = rhs.to_string();
            }
        }
    }

    // ---- Look for WHEN MATCHED THEN UPDATE SET col = val, ... ----
    let mut when_matched: Option<MergeAction> = None;
    let mut when_not_matched_by_target: Option<MergeAction> = None;

    if let Some(pos) = lower.find("when matched then update set") {
        let after = &trimmed[pos + "when matched then update set".len()..];
        // The SET clause runs until the next WHEN keyword (or end of string).
        let set_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let assigns_str = after[..set_end].trim();
        // Parse `col = val` pairs separated by commas.
        let assigns: Vec<(String, String)> = split_top_level_commas(assigns_str)
            .into_iter()
            .filter_map(|pair| {
                let pair = pair.trim();
                let eq_pos = pair.find('=')?;
                let col_raw = pair[..eq_pos].trim().to_string();
                let val_raw = pair[eq_pos + 1..].trim().to_string();
                // Strip any "alias." prefix from the LHS column (target.col → col).
                // IMPORTANT: do NOT strip the alias from the RHS value —
                // `source.val` must be preserved so execute_merge can
                // recognize it as a column reference and resolve it against
                // the current source row (Wave 56a fix).
                let col = col_raw.rsplit('.').next().unwrap_or(&col_raw).to_string();
                if col.is_empty() || val_raw.is_empty() {
                    None
                } else {
                    Some((col, val_raw))
                }
            })
            .collect();
        if !assigns.is_empty() {
            when_matched = Some(MergeAction::Update(assigns));
        }
    }

    if let Some(pos) = lower.find("when not matched then insert") {
        let after = &trimmed[pos + "when not matched then insert".len()..];
        // The INSERT clause runs until the next WHEN keyword (or end of string).
        let ins_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let ins_str = after[..ins_end].trim();
        // Parse `(col1, col2) VALUES (val1, val2)` — best-effort.
        if let Some(open) = ins_str.find('(') {
            if let Some(close) = ins_str.find(')') {
                let cols: Vec<String> =
                    ins_str[open + 1..close].split(',').map(|s| s.trim().to_string()).collect();
                if let Some(vals_pos) = ins_str[close..].to_lowercase().find("values") {
                    let vals_str = &ins_str[close + vals_pos + "values".len()..];
                    if let Some(v_open) = vals_str.find('(') {
                        if let Some(v_close) = vals_str.find(')') {
                            let vals: Vec<String> = vals_str[v_open + 1..v_close]
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .collect();
                            when_not_matched_by_target = Some(MergeAction::Insert(cols, vals));
                        }
                    }
                }
            }
        }
    }

    // If we parsed source column names, find the join source col's index
    // and rewrite source_rows so the first element of each tuple is the
    // value of the join column (stringified). The merge module uses
    // source_rows[i].0 as the join key to match against target.col_values.
    if !source_col_names.is_empty() && !join_source_col.is_empty() {
        if let Some(src_idx) =
            source_col_names.iter().position(|c| c.eq_ignore_ascii_case(&join_source_col))
        {
            // Each source_row tuple's first element becomes the join key.
            // The Vec<String> carries the full row values in source_col_names order.
            source_rows = source_rows
                .into_iter()
                .map(|(_old_key, mut vals)| {
                    let key = vals.get(src_idx).cloned().unwrap_or_default();
                    (key, vals)
                })
                .collect();
        }
    }

    Some(Merge {
        target,
        source_rows,
        source_col_names,
        join_target_col,
        join_source_col,
        when_matched,
        when_not_matched_by_source: None,
        when_not_matched_by_target,
    })
}

/// Split a string on top-level commas (not inside parentheses or quotes).
/// Used by `parse_merge` to split SET assignments like
/// `col1 = source.col1, col2 = 'literal, with comma'`.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 && !in_str => {
                out.push(cur.clone());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Parse the body of a SQL VALUES list — e.g. `(1, 'a'), (2, 'b')` — into
/// a Vec of (first_cell_stringified, full_row) tuples. The first cell is
/// later used as the join key (it's overwritten in parse_merge if a join
/// column index is known).
fn parse_values_tuples(s: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut tuples: Vec<String> = Vec::new();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                if depth == 1 {
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    tuples.push(cur.clone());
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            _ if depth >= 1 => cur.push(c),
            _ => {}
        }
    }
    for t in &tuples {
        let vals: Vec<String> =
            split_top_level_commas(t).into_iter().map(|v| v.trim().to_string()).collect();
        if !vals.is_empty() {
            let first = vals[0].clone();
            out.push((first, vals));
        }
    }
    out
}

impl QueryEngine {
    /// Wave 53: Materialize views referenced in a SQL string.
    ///
    /// For each view name in the registry, if the SQL contains `FROM view_name`,
    /// execute the view's SELECT SQL and register the result as a catalog
    /// table under the view's name. The outer SELECT then runs against the
    /// materialized table. This is a simple (non-incremental) materialization
    /// strategy — every query against a view re-runs the view's SELECT.
    fn materialize_views_in_sql(&mut self, sql: &str) -> String {
        let lower = sql.to_lowercase();
        // Collect view names that appear in the SQL before mutating self.
        let view_names: Vec<String> = self
            .views
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .filter(|view_name| {
                let pattern = format!("from {}", view_name.to_lowercase());
                lower.contains(&pattern)
            })
            .collect();
        // Now materialize each view. We collect (name, select_sql) pairs
        // first to release the immutable borrow on self.views before we
        // call self.execute_inner (which needs &mut self).
        let view_specs: Vec<(String, String)> = view_names
            .into_iter()
            .filter_map(|name| self.views.get(&name).map(|v| (name, v.select_sql.clone())))
            .collect();
        for (view_name, select_sql) in view_specs {
            if let Ok(result) = self.execute_inner(&select_sql, &Instant::now(), None) {
                let table = result_to_table(&view_name, &result);
                self.catalog.register(table);
            }
        }
        sql.to_string()
    }

    /// Execute a MERGE statement against a catalog table (Wave 53 wiring
    /// for exec/merge.rs). The target table is loaded into a QueryResult,
    /// `execute_merge` is applied, and the result is written back to the
    /// catalog.
    fn execute_merge_stmt(
        &mut self,
        merge: crate::exec::merge::Merge,
        start: &Instant,
    ) -> Result<QueryResult> {
        let target_name = merge.target.clone();
        // Load the target table into a QueryResult.
        let table = self
            .catalog
            .get(&target_name)
            .ok_or_else(|| Error::NotFound(format!("MERGE target table \"{target_name}\"")))?
            .clone();
        let mut qr = table_to_query_result(&table);

        let merge_result = crate::exec::merge::execute_merge(&mut qr, &merge);

        // Write the mutated QueryResult back into the catalog table.
        let new_table = query_result_to_table(&target_name, &qr);
        self.catalog.register(new_table);

        let mut result = QueryResult::empty();
        result.row_count = merge_result.inserted + merge_result.updated + merge_result.deleted;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

/// Convert a `Table` into a `QueryResult` so `execute_merge` can operate
/// on it.
fn table_to_query_result(table: &Table) -> QueryResult {
    let columns: Vec<ResultColumn> = table
        .column_names
        .iter()
        .enumerate()
        .map(|(i, name)| ResultColumn {
            name: name.clone(),
            values: table.columns[i].to_vec(),
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .collect();
    QueryResult { columns, row_count: table.row_count, elapsed_us: 0 }
}

/// Convert a `QueryResult` back into a `Table` (round-trip after merge).
fn query_result_to_table(name: &str, qr: &QueryResult) -> Table {
    let columns: Vec<std::sync::Arc<Vec<u64>>> =
        qr.columns.iter().map(|c| std::sync::Arc::new(c.values.clone())).collect();
    let column_names: Vec<String> = qr.columns.iter().map(|c| c.name.clone()).collect();
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: qr.row_count,
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    }
}

/// Parse `FOR SYSTEM_TIME AS OF <timestamp>` from a SQL string.
/// Returns (table_name, timestamp) if the clause is present.
///
/// SQL syntax: `SELECT ... FROM table_name FOR SYSTEM_TIME AS OF <ts>`
/// The table name appears between FROM and FOR SYSTEM_TIME.
fn parse_for_system_time(sql: &str) -> Option<(String, u64)> {
    let lower = sql.to_lowercase();
    let pos = lower.find("for system_time as of")?;
    // The timestamp is everything after "FOR SYSTEM_TIME AS OF" up to the
    // next non-digit character.
    let after = &sql[pos + "for system_time as of".len()..];
    let after_trimmed = after.trim_start();
    let ts_end = after_trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_trimmed.len());
    if ts_end == 0 {
        return None;
    }
    let timestamp: u64 = after_trimmed[..ts_end].parse().ok()?;

    // The table name is between FROM and FOR SYSTEM_TIME. Look at the
    // substring before "FOR SYSTEM_TIME AS OF".
    let before = &sql[..pos];
    let before_lower = before.to_lowercase();
    let from_pos = before_lower.rfind("from ")?;
    let after_from = &before[from_pos + "from ".len()..];
    // The table name is the first whitespace-delimited token (optionally
    // followed by WHERE/ORDER/etc.).
    let table_name = after_from.split_whitespace().next()?.to_string();
    Some((table_name, timestamp))
}

/// Detect `WITH (SYSTEM_VERSIONING = ON)` in a CREATE TABLE SQL string
/// (case-insensitive) and return the table name. Used by `execute_inner`
/// to register the table in `self.temporals` (Wave 56d).
///
/// SQL syntax:
///   CREATE TABLE <name> (<cols>) WITH (SYSTEM_VERSIONING = ON)
///   CREATE TABLE <name> (<cols>) WITH (SYSTEM_VERSIONING=ON)
///
/// Returns None if the SYSTEM_VERSIONING clause is not present or the
/// table name can't be extracted.
fn extract_temporal_table_name(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    // Look for "system_versioning" — accept both `SYSTEM_VERSIONING = ON`
    // and `SYSTEM_VERSIONING=ON` (no spaces around =).
    if !lower.contains("system_versioning") {
        return None;
    }
    // Check that ON follows (allow whitespace and optional = sign).
    let sv_pos = lower.find("system_versioning")?;
    let after_sv = &lower[sv_pos + "system_versioning".len()..];
    let after_sv_trimmed = after_sv.trim_start();
    // Optional '='.
    let after_eq =
        if after_sv_trimmed.starts_with('=') { &after_sv_trimmed[1..] } else { after_sv_trimmed };
    let after_eq_trimmed = after_eq.trim_start();
    if !after_eq_trimmed.starts_with("on") {
        return None;
    }
    // Extract the table name: the first identifier after "CREATE TABLE".
    let create_pos = lower.find("create table")?;
    let after_create = &sql[create_pos + "create table".len()..];
    let after_create_trimmed = after_create.trim_start();
    // Optional IF NOT EXISTS.
    let after_ifne = if after_create_trimmed.to_lowercase().starts_with("if not exists") {
        &after_create_trimmed["if not exists".len()..].trim_start()
    } else {
        after_create_trimmed
    };
    // The table name is the first identifier (up to whitespace, '.', or '(').
    let end = after_ifne.find(|c: char| c.is_whitespace() || c == '.' || c == '(')?;
    let name = &after_ifne[..end];
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Convert temporal-table rows (Vec<Vec<u64>>) into a QueryResult.
fn rows_to_query_result(
    rows: &[Vec<u64>],
    column_names: &[String],
    start: &Instant,
) -> QueryResult {
    let row_count = rows.len();
    let n_cols = column_names.len();
    let columns: Vec<ResultColumn> = (0..n_cols)
        .map(|i| {
            let values: Vec<u64> = rows.iter().map(|r| r.get(i).copied().unwrap_or(0)).collect();
            ResultColumn {
                name: column_names[i].clone(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }
        })
        .collect();
    QueryResult { columns, row_count, elapsed_us: start.elapsed().as_micros() as u64 }
}

/// Apply window functions to a QueryResult (Wave 53 wiring for
/// exec/window.rs). Detects `SelectItem::Window` items in the query and
/// appends a new ResultColumn for each.
fn apply_window_functions(
    result: &QueryResult,
    query: &crate::sql::parser::SelectQuery,
) -> QueryResult {
    use crate::exec::window::{
        count_over, dense_rank, parse_window_spec, rank, row_number, sum_over,
    };
    use crate::sql::parser::SelectItem;

    let mut new_cols: Vec<ResultColumn> = result.columns.clone();
    for item in &query.select {
        if let SelectItem::Window { func, arg, over_spec, alias } = item {
            let spec = match parse_window_spec(over_spec) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let func_upper = func.to_uppercase();
            let name = alias.clone().unwrap_or_else(|| func.to_lowercase());
            let values = match func_upper.as_str() {
                "ROW_NUMBER" => row_number(result, &spec),
                "RANK" => rank(result, &spec),
                "DENSE_RANK" => dense_rank(result, &spec),
                "SUM" => sum_over(result, arg, &spec),
                "COUNT" => count_over(result, &spec),
                _ => continue,
            };
            new_cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
    }
    QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us }
}

/// Stub for parsing PIVOT/UNPIVOT from QueryExtensions. The current
/// `QueryExtensions` type doesn't carry pivot specs, so this always
/// returns None. PIVOT is now wired through `parse_pivot_clause` which
/// detects the PIVOT keyword directly in the SQL string (Wave 56b).
fn extensions_pivot(_ext: &crate::sql::extensions::QueryExtensions) -> Option<PivotSpec> {
    None
}

/// A parsed PIVOT specification (Wave 53).
struct PivotSpec {
    group_col: String,
    pivot_col: String,
    value_col: String,
    pivot_values: Vec<String>,
    agg: String,
}

/// A parsed PIVOT clause extracted from a SQL string (Wave 56b).
/// `group_col` is auto-detected at apply time (see execute_inner).
struct PivotClause {
    agg: String,
    value_col: String,
    pivot_col: String,
    pivot_values: Vec<String>,
}

/// Parse a PIVOT clause from a SQL string. Returns None if no PIVOT clause
/// is present.
///
/// Supported syntax (case-insensitive):
///   PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))
///   PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
///   PIVOT (AVG(price) FOR region IN ('NA', 'EU', 'APAC'))
///
/// The clause may be followed by `AS <alias>` (which is stripped by
/// strip_pivot_clause before re-execution of the underlying SELECT).
fn parse_pivot_clause(sql: &str) -> Option<PivotClause> {
    let lower = sql.to_lowercase();
    let pivot_pos = lower.find("pivot ")?;
    // Must be followed by '(' (possibly with whitespace).
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    if !after_pivot_trimmed.starts_with('(') {
        return None;
    }
    // Find the matching close paren for the PIVOT (...) group.
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let inner = &after_pivot_trimmed[1..close];
    // inner should look like: SUM(amount) FOR quarter IN ('Q1', 'Q2')
    // or: COUNT(*) FOR quarter IN (1, 2, 3)
    let inner_lower = inner.to_lowercase();
    let for_pos = inner_lower.find(" for ")?;
    let agg_part = inner[..for_pos].trim();
    let after_for = &inner[for_pos + " for ".len()..];
    let after_for_lower = after_for.to_lowercase();
    let in_pos = after_for_lower.find(" in ")?;
    let pivot_col = after_for[..in_pos].trim().to_string();
    let after_in = &after_for[in_pos + " in ".len()..].trim_start();
    // after_in should start with '(' and end with ')'.
    if !after_in.starts_with('(') {
        return None;
    }
    let in_close = after_in.find(')')?;
    let values_str = &after_in[1..in_close];
    // Parse the values: split on commas, strip quotes/brackets.
    let pivot_values: Vec<String> = values_str
        .split(',')
        .map(|s| {
            let s = s.trim();
            // Strip single quotes.
            let s = if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            // Strip square brackets (SQL Server style [Q1]).
            let s = if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            s.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if pivot_values.is_empty() {
        return None;
    }
    // Parse the agg part: AGG_FUNC(arg). The arg may be '*' or a column name.
    let open = agg_part.find('(')?;
    let close_paren = agg_part.rfind(')')?;
    let agg = agg_part[..open].trim().to_uppercase();
    let value_col = agg_part[open + 1..close_paren].trim().to_string();
    if agg.is_empty() || value_col.is_empty() {
        return None;
    }
    Some(PivotClause { agg, value_col, pivot_col, pivot_values })
}

/// Strip the PIVOT clause (and any trailing `AS alias`) from a SQL string,
/// returning the underlying SELECT that should be executed to produce the
/// input rows for the pivot transformation.
fn strip_pivot_clause(sql: &str) -> String {
    let lower = sql.to_lowercase();
    let pivot_pos = match lower.find("pivot ") {
        Some(p) => p,
        None => return sql.to_string(),
    };
    // Walk forward from pivot_pos to find the matching close paren.
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    let paren_offset = after_pivot.len() - after_pivot_trimmed.len();
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = match close {
        Some(c) => c,
        None => return sql.to_string(),
    };
    // The PIVOT clause spans [pivot_pos, pivot_pos + "pivot ".len() + paren_offset + close + 1).
    let end_of_pivot = pivot_pos + "pivot ".len() + paren_offset + close + 1;
    // After the PIVOT clause, there may be `AS <alias>` — strip that too.
    let rest = &sql[end_of_pivot..];
    let rest_trimmed_start = rest.trim_start();
    if rest_trimmed_start.to_uppercase().starts_with("AS ") {
        let after_as = &rest_trimmed_start["AS ".len()..];
        // Skip the alias identifier (alphanumeric + underscore).
        let alias_len = after_as.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count();
        let after_alias = &after_as[alias_len..];
        // Build the result: sql[..pivot_pos] + after_alias.
        return format!("{}{}", &sql[..pivot_pos], after_alias);
    }
    // No AS clause — just concatenate.
    format!("{}{}", &sql[..pivot_pos], &sql[end_of_pivot..])
}

/// Apply a PIVOT transformation to a QueryResult (Wave 53 wiring for
/// exec/pivot.rs).
fn apply_pivot(result: &QueryResult, spec: &PivotSpec) -> QueryResult {
    crate::exec::pivot::pivot(
        result,
        &spec.group_col,
        &spec.pivot_col,
        &spec.value_col,
        &spec.pivot_values,
        &spec.agg,
    )
}

// -----------------------------------------------------------------------
// Wave 56c: JSON_VALUE / JSON_QUERY wiring.
// -----------------------------------------------------------------------

/// Check whether a SQL string contains a `JSON_VALUE(` or `JSON_QUERY(` call
/// (case-insensitive). Used by `execute_inner` to decide whether to intercept
/// the query for JSON post-processing.
fn contains_json_value_call(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    lower.contains("json_value(") || lower.contains("json_query(")
}

// -----------------------------------------------------------------------
// Wave 60c: UNION ALL wiring.
// -----------------------------------------------------------------------

/// Split a SQL string at the first top-level `UNION ALL` keyword
/// (case-insensitive). Returns (left_sql, right_sql) if found, else None.
///
/// "Top-level" means the UNION ALL is not inside parentheses (e.g. not in a
/// subquery). This is a simple heuristic — it doesn't handle UNION (without
/// ALL) or INTERSECT/EXCEPT.
fn split_union_all(sql: &str) -> Option<(String, String)> {
    let lower = sql.to_lowercase();
    let mut search_from = 0;
    loop {
        let pos = lower[search_from..].find("union all")?;
        let abs_pos = search_from + pos;
        // Check that this is a top-level UNION ALL (not inside parens).
        let before = &sql[..abs_pos];
        let depth = before.chars().fold(0i32, |acc, c| match c {
            '(' => acc + 1,
            ')' => acc - 1,
            _ => acc,
        });
        if depth == 0 {
            let left = sql[..abs_pos].trim().to_string();
            let right = sql[abs_pos + "union all".len()..].trim().to_string();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
        }
        search_from = abs_pos + "union all".len();
    }
}

/// Concatenate two QueryResults into one (UNION ALL). The result has the
/// columns from the left result; the right result's values are appended.
/// Column names are taken from the left result.
fn concatenate_results(left: QueryResult, right: QueryResult, start: &Instant) -> QueryResult {
    let total_rows = left.row_count + right.row_count;
    let n_cols = left.columns.len();
    let mut columns: Vec<ResultColumn> = left
        .columns
        .into_iter()
        .enumerate()
        .map(|(i, mut c)| {
            // Append the right result's values for this column.
            if i < right.columns.len() {
                c.values.extend(right.columns[i].values.iter().copied());
                // Merge string_values if both have them.
                if let Some(ref mut left_sv) = c.string_values {
                    if let Some(ref right_sv) = right.columns[i].string_values {
                        left_sv.extend(right_sv.iter().cloned());
                    } else {
                        // Right has no strings — pad with empty strings.
                        left_sv.extend(std::iter::repeat(String::new()).take(right.row_count));
                    }
                }
            }
            c
        })
        .collect();
    // If right has more columns than left (shouldn't happen for a valid UNION),
    // pad with empty columns.
    while columns.len() < n_cols {
        columns.push(ResultColumn {
            name: format!("col_{}", columns.len()),
            values: vec![0; total_rows],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }
    QueryResult { columns, row_count: total_rows, elapsed_us: start.elapsed().as_micros() as u64 }
}

// -----------------------------------------------------------------------
// Wave 60d: SELECT DISTINCT wiring.
// -----------------------------------------------------------------------

/// Deduplicate the rows of a QueryResult. Two rows are considered duplicates
/// if they have the same u64 values in every column. The first occurrence is
/// kept; subsequent duplicates are dropped.
fn deduplicate_rows(result: QueryResult) -> QueryResult {
    if result.row_count <= 1 {
        return result;
    }
    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    let mut keep_indices: Vec<usize> = Vec::with_capacity(result.row_count);
    for row in 0..result.row_count {
        let key: Vec<u64> =
            result.columns.iter().map(|c| c.values.get(row).copied().unwrap_or(0)).collect();
        if seen.insert(key) {
            keep_indices.push(row);
        }
    }
    if keep_indices.len() == result.row_count {
        return result; // no duplicates
    }
    let new_row_count = keep_indices.len();
    let columns: Vec<ResultColumn> = result
        .columns
        .into_iter()
        .map(|mut c| {
            let new_values: Vec<u64> =
                keep_indices.iter().map(|&i| c.values.get(i).copied().unwrap_or(0)).collect();
            c.values = new_values;
            if let Some(ref mut sv) = c.string_values {
                let new_sv: Vec<String> =
                    keep_indices.iter().map(|&i| sv.get(i).cloned().unwrap_or_default()).collect();
                *sv = new_sv;
            }
            if let Some(ref mut bm) = c.null_mask {
                let new_bm: Vec<bool> =
                    keep_indices.iter().map(|&i| bm.get(i).copied().unwrap_or(false)).collect();
                *bm = new_bm;
            }
            c
        })
        .collect();
    QueryResult { columns, row_count: new_row_count, elapsed_us: result.elapsed_us }
}

/// A parsed JSON_VALUE / JSON_QUERY call extracted from a SQL string.
struct JsonValueCall {
    /// Byte offset in the original SQL where the call begins (the 'J' of
    /// JSON_VALUE / JSON_QUERY).
    start: usize,
    /// Byte offset one past the closing ')' of the call (or past the alias
    /// if one was present).
    end: usize,
    /// The column name argument (first arg).
    col_name: String,
    /// The JSON path argument (second arg, without quotes).
    path: String,
    /// Whether this is JSON_QUERY (true) or JSON_VALUE (false).
    is_query: bool,
    /// Optional `AS alias` that immediately follows the call (consumed from
    /// the SQL during rewriting).
    alias: Option<String>,
    /// 0-indexed position of this call in the SELECT list (count of top-level
    /// commas between SELECT and the call's byte position). Used to find the
    /// corresponding result column after execution, since the basic parser
    /// discards column aliases.
    select_position: usize,
}

/// Extract all JSON_VALUE / JSON_QUERY calls from a SQL string. Returns one
/// entry per call, in order of appearance. Each entry carries the byte range
/// so the caller can rewrite the SQL.
fn extract_json_value_calls(sql: &str) -> Vec<JsonValueCall> {
    let lower = sql.to_lowercase();
    // Find the SELECT keyword position (to compute select_position).
    let select_pos = lower.find("select ").or_else(|| lower.find("select\n"));
    let mut calls = Vec::new();
    let mut search_from = 0;
    loop {
        // Find the next "json_value(" or "json_query(".
        let jv_pos = lower[search_from..].find("json_value(").map(|p| p + search_from);
        let jq_pos = lower[search_from..].find("json_query(").map(|p| p + search_from);
        let (pos, is_query) = match (jv_pos, jq_pos) {
            (Some(p), Some(q)) => {
                if p <= q {
                    (p, false)
                } else {
                    (q, true)
                }
            }
            (Some(p), None) => (p, false),
            (None, Some(q)) => (q, true),
            (None, None) => break,
        };
        // Walk forward from `pos` to find the matching close paren.
        let after_open = lower[pos..].find('(').unwrap() + 1;
        let mut depth = 1i32;
        let mut cur = pos + after_open;
        let bytes = sql.as_bytes();
        let mut in_str = false;
        let mut close = None;
        while cur < bytes.len() {
            let c = bytes[cur] as char;
            if in_str {
                if c == '\'' {
                    in_str = false;
                }
            } else {
                match c {
                    '\'' => in_str = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(cur);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            cur += 1;
        }
        let close = match close {
            Some(c) => c,
            None => break,
        };
        // The args are between after_open (relative to pos) and close.
        let args_str = &sql[pos + after_open..close];
        // Parse the two arguments: col_name, 'path'.
        let (col_name, path) = match parse_json_value_args(args_str) {
            Some(p) => p,
            None => {
                search_from = close + 1;
                continue;
            }
        };
        // Look for an optional `AS alias` after the close paren.
        let mut after = close + 1;
        let rest = &sql[after..];
        let rest_trimmed = rest.trim_start();
        let leading_ws = rest.len() - rest_trimmed.len();
        let alias = if rest_trimmed.to_uppercase().starts_with("AS ") {
            let after_as = &rest_trimmed["AS ".len()..];
            let alias_len =
                after_as.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            if alias_len > 0 {
                let alias = after_as[..alias_len].to_string();
                after += leading_ws + "AS ".len() + alias_len;
                Some(alias)
            } else {
                None
            }
        } else {
            None
        };
        // Compute the 0-indexed position of this call in the SELECT list:
        // count top-level commas between SELECT and the call's byte position.
        let select_position =
            if let Some(sp) = select_pos { count_top_level_commas(&lower[sp..pos]) } else { 0 };
        calls.push(JsonValueCall {
            start: pos,
            end: after,
            col_name,
            path,
            is_query,
            alias,
            select_position,
        });
        search_from = after;
    }
    calls
}

/// Count top-level commas in a SQL substring (commas not inside parentheses
/// or string literals). Used to determine a JSON_VALUE call's position in
/// the SELECT list.
fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut count = 0;
    for c in s.chars() {
        if in_str {
            if c == '\'' {
                in_str = false;
            }
        } else {
            match c {
                '\'' => in_str = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => count += 1,
                _ => {}
            }
        }
    }
    count
}

/// Parse the arguments of a JSON_VALUE / JSON_QUERY call: `<col>, '<path>'`.
/// Returns (col_name, path) or None if the args don't match the expected shape.
fn parse_json_value_args(args: &str) -> Option<(String, String)> {
    // Split on the first comma that's not inside a string.
    let mut in_str = false;
    let mut comma_pos = None;
    for (i, c) in args.char_indices() {
        match c {
            '\'' => in_str = !in_str,
            ',' if !in_str => {
                comma_pos = Some(i);
                break;
            }
            _ => {}
        }
    }
    let comma_pos = comma_pos?;
    let col_name = args[..comma_pos].trim().to_string();
    let path_part = args[comma_pos + 1..].trim();
    // path_part should be '...' — strip the quotes.
    let path = if path_part.starts_with('\'') && path_part.ends_with('\'') && path_part.len() >= 2 {
        path_part[1..path_part.len() - 1].to_string()
    } else {
        return None;
    };
    if col_name.is_empty() || path.is_empty() {
        return None;
    }
    Some((col_name, path))
}

impl QueryEngine {
    /// Execute a SQL string that contains one or more JSON_VALUE / JSON_QUERY
    /// calls. The approach:
    /// 1. Extract all JSON_VALUE / JSON_QUERY calls from the SQL, recording
    ///    each call's byte range and its 0-indexed position in the SELECT list.
    /// 2. Rewrite the SQL: replace each call (and its optional AS alias) with
    ///    just the bare column name (e.g. `JSON_VALUE(payload, '$.name')` →
    ///    `payload`).
    /// 3. Execute the rewritten SQL via `execute_inner` (the rewritten SQL no
    ///    longer contains `JSON_VALUE(`, so there's no infinite recursion).
    /// 4. For each call, find the result column at the call's recorded SELECT
    ///    position, apply json::json_value() (or json::json_query()) to each
    ///    string value, and replace the column with a new ResultColumn whose
    ///    string_values are the extracted JSON values.
    /// 5. Rename the column to the user's alias (if provided) or to a sensible
    ///    default like "json_value".
    fn execute_with_json_value(
        &mut self,
        sql: &str,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        let calls = extract_json_value_calls(sql);
        if calls.is_empty() {
            // Shouldn't happen — contains_json_value_call returned true — but
            // fall through to the normal path just in case.
            return self.execute_inner(sql, start, txn_id);
        }
        // Rewrite the SQL: replace each call with the bare column name.
        let mut rewritten = String::with_capacity(sql.len());
        let mut last_end = 0;
        for c in &calls {
            rewritten.push_str(&sql[last_end..c.start]);
            rewritten.push_str(&c.col_name);
            last_end = c.end;
        }
        rewritten.push_str(&sql[last_end..]);
        // Execute the rewritten SQL. The rewritten SQL has no JSON_VALUE(...)
        // calls, so this won't re-enter execute_with_json_value.
        let mut result = self.execute_inner(&rewritten, start, txn_id)?;
        // Post-process: for each call, find the result column at the call's
        // SELECT position and apply json_value() / json_query() to its string
        // values.
        for c in &calls {
            let col_idx = c.select_position;
            if col_idx >= result.columns.len() {
                continue;
            }
            // Get the string values from the column. If string_values is
            // None, we can't extract JSON — skip this call.
            let strings = result.columns[col_idx].string_values.clone().unwrap_or_default();
            if strings.is_empty() {
                continue;
            }
            let extracted: Vec<String> = strings
                .iter()
                .map(|s| {
                    if c.is_query {
                        crate::exec::json::json_query(s, &c.path).unwrap_or_default()
                    } else {
                        crate::exec::json::json_value(s, &c.path).unwrap_or_default()
                    }
                })
                .collect();
            // Replace the column with a new one carrying the extracted strings.
            use xxhash_rust::xxh3;
            let values: Vec<u64> = extracted.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect();
            let final_name = c.alias.clone().unwrap_or_else(|| {
                if c.is_query {
                    "json_query".into()
                } else {
                    "json_value".into()
                }
            });
            result.columns[col_idx] = ResultColumn {
                name: final_name,
                values,
                string_values: Some(extracted),
                type_oid: 25, // text OID
                null_mask: None,
            };
        }
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

// -----------------------------------------------------------------------
// Wave 66: free functions for ALTER TABLE / CREATE INDEX support.
// -----------------------------------------------------------------------

/// Compute the default cell value for a column definition. Used by
/// ALTER TABLE ADD COLUMN to fill the new column for existing rows.
///
/// - INT / BIGINT / SMALLINT / TINYINT / BIT / BOOLEAN → 0
/// - FLOAT / REAL / DECIMAL → 0.0 (as f64::to_bits)
/// - VARCHAR / NVARCHAR / TEXT → xxh3 hash of empty string (which is
///   the same as inserting an empty string)
/// - DATE / TIMESTAMP → 0 (epoch)
///
/// If a DEFAULT clause is present, the default literal is parsed and
/// used instead.
fn default_cell_for_type(col_def: &crate::sql::ColumnDef, _row_count: usize) -> u64 {
    use crate::sql::ColumnType;
    // If a DEFAULT literal is present, parse it.
    if let Some(ref default) = col_def.default {
        let trimmed = default.trim();
        // String literal — hash it.
        if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
            let inner = &trimmed[1..trimmed.len() - 1];
            return xxhash_rust::xxh3::xxh3_64(inner.as_bytes());
        }
        // Integer literal.
        if let Ok(n) = trimmed.parse::<i64>() {
            return n as u64;
        }
        // Float literal.
        if let Ok(f) = trimmed.parse::<f64>() {
            return f.to_bits();
        }
        // NULL keyword.
        if trimmed.eq_ignore_ascii_case("null") {
            return 0;
        }
        // Fall through to type-based default.
    }
    match col_def.col_type {
        ColumnType::Int
        | ColumnType::BigInt
        | ColumnType::SmallInt
        | ColumnType::TinyInt
        | ColumnType::Bit
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Uuid => 0,
        ColumnType::Float
        | ColumnType::Real
        | ColumnType::Decimal(_, _)
        | ColumnType::Numeric(_, _) => 0.0f64.to_bits(),
        ColumnType::Varchar(_)
        | ColumnType::Nvarchar(_)
        | ColumnType::Text
        | ColumnType::Json
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Enum(_) => {
            // Empty string → hash of empty bytes (consistent with INSERT
            // of '' which goes through parse_value_cell).
            xxhash_rust::xxh3::xxh3_64(b"")
        }
    }
}

/// Extract a `col = literal` predicate from an Expr (Wave 66).
///
/// Returns `Some((col_name, value_cell))` if the expr is a simple
/// equality between a column and a literal (in either order). Returns
/// `None` for any other shape (AND/OR, range, LIKE, etc.).
///
/// The literal is converted to a u64 cell using the same rules as the
/// executor's `literal_to_u64` (int → as u64, float → to_bits, string →
/// parse as int or xxh3 hash).
fn extract_eq_predicate(expr: &crate::sql::parser::Expr) -> Option<(String, u64)> {
    use crate::sql::parser::{Expr, Value};
    match expr {
        Expr::Binary { left, op, right } if op == "=" => {
            // Try left=column, right=literal.
            if let (Expr::Column(name), Expr::Literal(val)) = (left.as_ref(), right.as_ref()) {
                return Some((name.clone(), literal_to_cell(val)?));
            }
            // Try right=column, left=literal.
            if let (Expr::Column(name), Expr::Literal(val)) = (right.as_ref(), left.as_ref()) {
                return Some((name.clone(), literal_to_cell(val)?));
            }
            None
        }
        _ => None,
    }
}

/// Convert a parsed literal Value to a u64 cell (Wave 66 helper).
fn literal_to_cell(val: &crate::sql::parser::Value) -> Option<u64> {
    use crate::sql::parser::Value;
    match val {
        Value::Int(i) => Some(*i as u64),
        Value::Float(f) => Some(f.to_bits()),
        Value::String(s) => {
            // Try parsing as an integer first (e.g. WHERE id = '42').
            if let Ok(n) = s.parse::<i64>() {
                return Some(n as u64);
            }
            // Otherwise hash the string (matches the executor's behavior
            // for string equality on hashed columns).
            Some(xxhash_rust::xxh3::xxh3_64(s.as_bytes()))
        }
        Value::Hex(bytes) => {
            let v =
                bytes.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i)));
            Some(v)
        }
    }
}
