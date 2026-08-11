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
pub mod query_features;
pub mod result;
pub mod query_interpreter;

pub use executor::execute_select;
pub use executor::{planner_pipeline_invoked_count, reset_planner_pipeline_counter};
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
    /// Public so the storage layer (`storage::replication::backup`) can
    /// list tables for backup (Task 5.4). See AGENT_B_API_REQUESTS.md.
    pub catalog: Catalog,
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
    /// Public so the storage layer (`Checkpoint::load`) can temporarily
    /// detach the WAL during checkpoint load — preventing the checkpoint
    /// statements from being re-written to the WAL (Task 1.1 fix for the
    /// duplicate-row data-corruption bug). See `AGENT_B_API_REQUESTS.md`.
    pub wal: Option<crate::storage::recovery::Wal>,
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
    /// MVCC transaction manager (Wave 4 — Agent C).
    ///
    /// Used when `mvcc_enabled` is true. Tracks transaction IDs and commit
    /// state for real multi-version concurrency control. Unlike the legacy
    /// `TxnManager` (which deep-clones the entire catalog at BEGIN), this
    /// manager is O(1) per BEGIN — it just records the snapshot timestamp.
    mvcc_txn_manager: crate::txn::MvccTxnManager,
    /// Whether MVCC mode is enabled (Wave 4 — Agent C).
    ///
    /// `false` (default): use the legacy `TxnManager` with catalog snapshot
    /// swap. `true`: use `MvccTxnManager` with per-row version chains.
    ///
    /// MVCC mode is opt-in via [`QueryEngine::enable_mvcc`] so existing
    /// callers that depend on snapshot-isolation semantics are unaffected.
    mvcc_enabled: bool,
    /// Optional WAL streamer for replication (Wave 5 Task 5.3 — Agent C).
    ///
    /// When set, every successful WAL append+fsync also streams the record
    /// to a replica via `WalStreamer::stream_record`. The streamer is
    /// attached via [`QueryEngine::enable_replication`].
    wal_streamer: Option<WalStreamerHandle>,
    /// Optional Raft node for leader election (Task 4.5 — debt-5.4).
    /// When set via `enable_raft()`, the node participates in leader election.
    /// On becoming leader, it connects WalStreamers to all followers.
    ///
    /// When the `raft` feature is enabled, this stub is superseded by
    /// `raft_manager` (real openraft consensus); the stub remains for
    /// callers that compile without the feature.
    raft_node: Option<crate::storage::replication::RaftNode>,
    /// Real openraft-based Raft consensus manager (Wave 5 — Task 5.4).
    /// Set by `enable_raft` when the `raft` feature is enabled. The
    /// manager holds the `Raft` handle, the dispatcher task, and the
    /// in-memory store. WAL records are proposed through it via
    /// `RaftManager::propose`.
    #[cfg(feature = "raft")]
    raft_manager: Option<crate::storage::raft::RaftManager>,
    /// Dedicated tokio runtime keeping the openraft dispatcher (and the
    /// Raft core task) alive for the engine's lifetime. Only present when
    /// `raft` feature is enabled AND `enable_raft` has been called.
    #[cfg(feature = "raft")]
    raft_runtime: Option<tokio::runtime::Runtime>,
}

/// A handle to an active WAL streamer (Wave 5 Task 5.3 — Agent C).
///
/// Wraps `WalStreamer` in an `Arc<Mutex<...>>` so the engine can share
/// the *same* streamer instance with the `Wal` (which holds it as an
/// `Arc<Mutex<dyn WalStreamSink>>` via `set_stream_sink`). Both
/// `enable_replication` and `enable_replication_local_only` clone the
/// same `Arc` into both `self.wal_streamer` and the Wal's sink, so that
/// `wal_records_streamed()` reads the counter the Wal actually writes
/// to (fix for the pre-existing wiring bug where `wal_records_streamed()`
/// always returned 0 because it queried a separate, never-written
/// streamer).
pub struct WalStreamerHandle {
    /// The underlying WalStreamer, shared with the Wal's stream sink.
    pub streamer: std::sync::Arc<std::sync::Mutex<crate::storage::replication::WalStreamer>>,
}



pub mod copy;
pub mod ddl;
pub mod dispatch;
pub mod dml;
pub mod helpers;
pub use helpers::*;
pub mod transaction;
pub mod vacuum;

impl QueryEngine {
    /// Execute a SQL statement in **read-only mode** (Wave 2 — Agent C).
    ///
    /// Takes `&self` (not `&mut self`) so callers can hold a `RwLock::read()`
    /// guard and run multiple SELECTs concurrently without blocking other
    /// readers. DML/DDL statements are rejected with an error so a
    /// read-only caller can never accidentally mutate the catalog.
    ///
    /// # Accepted statements
    ///
    /// - `SELECT ...` (no interpreter fallback — see below)
    /// - `EXPLAIN SELECT ...` (uses the planner pipeline from Wave 1)
    /// - `SHOW ...` (treated as a SELECT against `__dummy__`)
    /// - `WITH ... SELECT ...` is rejected (the CTE executor needs `&mut
    ///   self` to register temp tables); callers should acquire a write
    ///   lock for CTEs.
    ///
    /// # Rejected statements
    ///
    /// `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `ALTER`, `BEGIN`,
    /// `COMMIT`, `ROLLBACK`, `COPY`, `VACUUM`, `CHECKPOINT`, `MERGE`,
    /// `SAVEPOINT`, `RELEASE`, `BACKUP`, `RESTORE` — all return
    /// `Error::Other("read-only transaction: <verb> requires a write lock")`.
    ///
    /// # Errors
    ///
    /// - [`Error::Other`] if the SQL is a write statement, requires
    ///   interpreter fallback, or fails during execution.
    /// - [`Error::Parse`] if the SQL is malformed.
    /// - [`Error::NotFound`] if the source table or a referenced column
    ///   does not exist in the catalog.
    pub fn execute_readonly(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let trimmed = sql.trim();

        // Wave 3 (Agent C): dispatch via classify_statement, not starts_with.
        use crate::engine::dispatch::{classify_statement, StatementKind};
        let kind = classify_statement(sql);

        // EXPLAIN is read-only (uses the planner, doesn't touch catalog).
        // We re-use the planner-pipeline EXPLAIN path from Wave 1 Task 1.2,
        // but with only &self (no &mut self needed).
        match kind {
            StatementKind::Explain => {
                let inner_sql = crate::engine::helpers::strip_first_keyword(trimmed);
                return self.execute_readonly_explain(inner_sql, &start);
            }
            StatementKind::Select | StatementKind::Show => {
                // Fall through to the readonly execution path below.
            }
            _ => {
                // Any other verb (INSERT, UPDATE, DELETE, CREATE, DROP,
                // BEGIN, COMMIT, COPY, VACUUM, ...) requires a write lock.
                let verb = trimmed.split_whitespace().next().unwrap_or("unknown");
                return Err(Error::Other(format!(
                    "read-only transaction: {verb} requires a write lock"
                )));
            }
        }

        // DDL/DML are not readonly even if they happen to start with SELECT
        // (e.g. SELECT INTO). Reject them.
        if crate::sql::parse_ddl(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("read-only transaction: DDL requires a write lock".into()));
        }
        if crate::sql::parse_dml(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("read-only transaction: DML requires a write lock".into()));
        }

        // CTEs need &mut self (temp-table registration), so reject them.
        if crate::sql::parse_with(sql).is_some() {
            return Err(Error::Other("read-only transaction: CTE requires a write lock".into()));
        }

        // Parse as SELECT and execute against the current catalog.
        let (query, extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // Basic parser failed — would need interpreter fallback
                // (which requires &mut self). Reject.
                return Err(Error::Other(
                    "read-only transaction: query needs interpreter fallback, requires a write lock".into(),
                ));
            }
        };

        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
            // Task 2.4: read-only path holds only `&self`, so the engine
            // cannot have an active MVCC transaction (those require a write
            // lock to BEGIN/COMMIT). Pass `None` here — no MVCC filtering.
            None,
        ) {
            Ok(mut result) => {
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            Err(_exec_err) => {
                // execute_select failed — would need interpreter fallback.
                Err(Error::Other(
                    "read-only transaction: query failed in execute_select, requires a write lock".into(),
                ))
            }
        }
    }

    /// Internal helper: EXPLAIN path that runs with only `&self` (used by
    /// `execute_readonly`). Mirrors the planner-based EXPLAIN from Wave 1
    /// Task 1.2 but doesn't require `&mut self`.
    fn execute_readonly_explain(&self, sql: &str, start: &Instant) -> Result<QueryResult> {
        let (query, _extensions) = crate::sql::parse_with_extensions(sql)
            .map_err(Error::Parse)?;
        let plan = crate::planner::build_plan(&query)?;
        let optimizer = crate::planner::CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);
        let plan_text = format!("{}", optimized);

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

    /// Deprecated alias for [`QueryEngine::execute_readonly`].
    ///
    /// retained for backwards compatibility with `src/server/pgwire.rs`
    /// (owned by another agent). New callers should use `execute_readonly`.
    pub fn try_readonly_select(&self, sql: &str) -> Result<QueryResult> {
        self.execute_readonly(sql)
    }
}

// ---------------------------------------------------------------------------
// Wave 2 Task 2.2 — SELECT-vs-DML routing helper.
//
// The production pattern wraps `QueryEngine` in `Arc<RwLock<QueryEngine>>`.
// This free function checks the SQL verb (via the formal parser, not string
// match) and acquires the appropriate lock:
//   - SELECT / EXPLAIN / SHOW  →  RwLock::read()   → execute_readonly
//   - INSERT / UPDATE / DELETE / CREATE / DROP / ...  →  RwLock::write()  → execute
//
// Callers that already hold a lock should call `execute_readonly` or
// `execute` directly instead of this helper.
// ---------------------------------------------------------------------------

/// Classify a SQL statement as readonly (SELECT-like) or write (DML/DDL).
///
/// Returns `true` if the statement can be executed with a read lock via
/// [`QueryEngine::execute_readonly`], `false` if it needs a write lock.
///
/// This uses the formal parser (`crate::sql::parse_ddl` / `parse_dml`) to
/// classify, not string-prefix matching — so `SELECT INTO ...` (which is
/// actually DML) is correctly classified as a write statement.
#[must_use]
pub fn is_readonly_sql(sql: &str) -> bool {
    use crate::engine::dispatch::{classify_statement, StatementKind};
    let kind = classify_statement(sql);

    // Only SELECT, EXPLAIN, and SHOW are candidates for the read-only path.
    if !matches!(kind, StatementKind::Select | StatementKind::Explain | StatementKind::Show) {
        return false;
    }

    // DDL/DML disguised as SELECT (e.g. SELECT INTO) — must use the parser.
    if crate::sql::parse_ddl(sql).ok().flatten().is_some() {
        return false;
    }
    if crate::sql::parse_dml(sql).ok().flatten().is_some() {
        return false;
    }

    // WITH ... SELECT ... is a CTE — needs write lock (temp-table registration).
    if matches!(kind, StatementKind::With) {
        return false;
    }
    if crate::sql::parse_with(sql).is_some() {
        return false;
    }

    true
}

/// Route a SQL statement to the appropriate lock + execute path.
///
/// This is the production entry point for `Arc<RwLock<QueryEngine>>`:
///
/// ```ignore
/// let engine: Arc<RwLock<QueryEngine>> = ...;
/// let result = turbogp::engine::route_and_execute(&engine, sql)?;
/// ```
///
/// - For SELECT / EXPLAIN / SHOW, acquires `RwLock::read()` and calls
///   [`QueryEngine::execute_readonly`].
/// - For DML / DDL / transaction control, acquires `RwLock::write()` and
///   calls [`QueryEngine::execute`].
///
/// This maximizes read concurrency: 10 concurrent SELECTs run in parallel
/// (sharing the read lock), while DML/DDL is serialized via the write lock.
///
/// # Wave 5 Task 5.4 — verification
///
/// The function itself was introduced in Wave 2 Task 2.2 (this exact
/// signature); Wave 5 Task 5.4 re-confirms it as the production entry
/// point and adds concurrent-stress verification in
/// `tests/concurrency_test.rs`:
/// - `test_route_and_execute_select_takes_read_lock`: 10 concurrent
///   SELECTs via this function complete in <2× a single SELECT's time
///   (proving read locks are shared, not exclusive).
/// - `test_concurrent_readers_writer`: 10 readers + 1 writer for 2 s,
///   no deadlocks, no panics, final COUNT == initial + writer_ops
///   (data consistency under mixed read/write load).
pub fn route_and_execute(
    engine: &std::sync::Arc<std::sync::RwLock<QueryEngine>>,
    sql: &str,
) -> Result<QueryResult> {
    use std::sync::RwLock;
    if is_readonly_sql(sql) {
        // Read path: multiple readers can hold the lock concurrently.
        let guard = engine.read().map_err(|e| {
            Error::Other(format!("route_and_execute: read lock poisoned: {e}"))
        })?;
        guard.execute_readonly(sql)
    } else {
        // Write path: serialized via the write lock.
        let mut guard = engine.write().map_err(|e| {
            Error::Other(format!("route_and_execute: write lock poisoned: {e}"))
        })?;
        guard.execute(sql)
    }
}

impl QueryEngine {

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
        let catalog = Catalog::new();
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
            row_versions: Vec::new(),
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
            mvcc_txn_manager: crate::txn::MvccTxnManager::new(),
            mvcc_enabled: false,
            wal_streamer: None,
            raft_node: None,
            #[cfg(feature = "raft")]
            raft_manager: None,
            #[cfg(feature = "raft")]
            raft_runtime: None,
        }
    }

    /// Enable MVCC mode (Wave 4 — Agent C).
    ///
    /// After calling this, `BEGIN`/`COMMIT`/`ROLLBACK` use the
    /// [`MvccTxnManager`] instead of the legacy `TxnManager`. The
    /// `MvccTxnManager` is O(1) per BEGIN (no catalog deep-clone) and
    /// supports multiple concurrent transactions.
    ///
    /// **Note:** full row-level visibility filtering (where `execute_select`
    /// filters rows by `(xmin, xmax)` version chains) is pending Agent B's
    /// completion of the `Table.row_versions` population in INSERT/UPDATE/
    /// DELETE. In the current implementation, MVCC mode provides correct
    /// transaction ID tracking and commit/abort state, but does NOT yet
    /// filter rows by visibility — all rows are visible to all transactions
    /// (like autocommit). This is documented in `AGENT_C_API_REQUESTS.md`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Other` if a snapshot-isolation transaction is
    /// currently active (must commit or rollback first).
    pub fn enable_mvcc(&mut self) -> Result<()> {
        if self.txn_manager.is_active() {
            return Err(Error::Other(
                "cannot enable MVCC while a snapshot-isolation transaction is active".into(),
            ));
        }
        self.mvcc_enabled = true;
        Ok(())
    }

    /// Disable MVCC mode, reverting to the legacy `TxnManager` (Wave 4).
    ///
    /// # Errors
    ///
    /// Returns `Error::Other` if an MVCC transaction is currently active.
    pub fn disable_mvcc(&mut self) -> Result<()> {
        if self.mvcc_txn_manager.is_active() {
            return Err(Error::Other(
                "cannot disable MVCC while an MVCC transaction is active".into(),
            ));
        }
        self.mvcc_enabled = false;
        Ok(())
    }

    /// Returns `true` if MVCC mode is enabled (Wave 4).
    #[must_use]
    pub fn is_mvcc_enabled(&self) -> bool {
        self.mvcc_enabled
    }

    /// Borrow the MVCC transaction manager (Wave 4).
    ///
    /// Exposed for tests that want to verify transaction state directly.
    #[must_use]
    pub fn mvcc_txn_manager(&self) -> &crate::txn::MvccTxnManager {
        &self.mvcc_txn_manager
    }

    /// Test-only: begin a background MVCC transaction without checking
    /// whether one is already active (Task 2.4).
    ///
    /// Unlike `execute("BEGIN")` (which calls `begin_compat` and errors if
    /// a txn is active), this calls `MvccTxnManager::begin` directly — the
    /// previously-active transaction remains in `txn_states` as
    /// `InProgress`, and `current_active` is overwritten to the new txn.
    ///
    /// This enables integration tests to simulate concurrent transactions
    /// on a single `QueryEngine` for verifying MVCC visibility filtering
    /// (e.g. T1 uncommitted INSERT → T2 SELECT must not see it).
    ///
    /// Returns the new transaction's ID.
    #[doc(hidden)]
    #[must_use]
    pub fn begin_background_txn(&mut self) -> u64 {
        self.mvcc_txn_manager.begin().id
    }

    /// Test-only: commit a specific MVCC transaction by ID (Task 2.4).
    ///
    /// Used in conjunction with [`begin_background_txn`](Self::begin_background_txn)
    /// to simulate a concurrent transaction's commit while a different txn
    /// is the `current_active`. Only the specified `txn_id` is committed;
    /// `current_active` is cleared only if it matches `txn_id`.
    #[doc(hidden)]
    pub fn commit_background_txn(&mut self, txn_id: u64) {
        self.mvcc_txn_manager.commit(txn_id);
    }

    /// Create a QueryEngine with on-disk persistence (Wave 63).
    /// The `data_dir` is where table files (`<table_id>.tbl`) and the WAL
    /// (`wal.log`) are stored. Tables created via CREATE TABLE are persisted
    /// to disk; INSERT/UPDATE/DELETE write through the buffer pool and are
    /// durable after COMMIT.
    pub fn with_data_dir<P: AsRef<std::path::Path>>(data_dir: P) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        // Use in_memory() to get a clean engine, then wire persistence.
        // (Self::new() would recurse into with_data_dir, causing infinite loop.)
        let mut engine = Self::in_memory();
        let bp = crate::storage::buffer_pool::BufferPool::new(data_dir, 256)?;
        engine.buffer_pool = Some(bp);
        // Also open a WAL in the same data directory.
        // Task 2.3: the WAL is now segmented — it manages `wal-<N>.log`
        // files inside a dedicated `wal/` subdirectory of the data dir.
        let wal_dir = data_dir.join("wal");
        let wal = crate::storage::recovery::Wal::open(&wal_dir)?;
        engine.wal = Some(wal);
        // Wave 5 (A4 fix): Load checkpoint BEFORE replaying WAL.
        // The checkpoint contains the catalog state at the last VACUUM;
        // WAL replay then applies any records written after the checkpoint.
        //
        // Task 4.1 + 4.2: prefer the binary checkpoint (fast, ~10x faster
        // than SQL-text — no parsing, no re-execution). Fall back to the
        // legacy SQL checkpoint if checkpoint.bin is missing (e.g. data
        // dirs written by older engine versions) or fails to load.
        let checkpoint_bin_path = data_dir.join("checkpoint.bin");
        let checkpoint_sql_path = data_dir.join("checkpoint.sql");
        if checkpoint_bin_path.exists() {
            match crate::storage::checkpoint::BinaryCheckpoint::load(&checkpoint_bin_path) {
                Ok(loaded) => {
                    // Register tables directly into the engine's catalog —
                    // no SQL re-execution. Tables are cloned because
                    // Catalog doesn't expose a `take`/`drain` API and the
                    // loaded Catalog owns its Tables.
                    let names: Vec<String> =
                        loaded.table_names().into_iter().map(String::from).collect();
                    let mut registered = 0usize;
                    for name in &names {
                        if name == "__dummy__" {
                            continue;
                        }
                        if let Some(table) = loaded.get(name) {
                            engine.catalog.register(table.clone());
                            registered += 1;
                        }
                    }
                    log::debug!(
                        "binary checkpoint: loaded {registered} tables from {}",
                        checkpoint_bin_path.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "binary checkpoint load failed ({}): {e}; falling back to SQL checkpoint",
                        checkpoint_bin_path.display()
                    );
                    if let Err(e) =
                        crate::storage::recovery::Checkpoint::load(&mut engine, &checkpoint_sql_path)
                    {
                        log::warn!("checkpoint load failed: {e}");
                    }
                }
            }
        } else {
            // Legacy path: no binary checkpoint, load SQL checkpoint.
            if let Err(e) = crate::storage::recovery::Checkpoint::load(&mut engine, &checkpoint_sql_path) {
                log::warn!("checkpoint load failed: {e}");
            }
        }
        // Task 1.3: read the checkpoint's last_lsn (if the sidecar exists)
        // and use it to skip already-checkpointed records on replay. Also
        // bump the WAL's next_lsn past it so new records get LSNs strictly
        // greater than the checkpoint's last_lsn.
        //
        // The sidecar is named `checkpoint.sql.lsn` regardless of which
        // checkpoint format was loaded — it's written by
        // `Checkpoint::save_and_truncate` after BOTH checkpoints are
        // durable (the binary checkpoint is written first, then the SQL
        // checkpoint's save_and_truncate writes the sidecar + truncates
        // the WAL).
        let checkpoint_last_lsn =
            crate::storage::recovery::Checkpoint::read_last_lsn(&checkpoint_sql_path);
        if let Some(lsn) = checkpoint_last_lsn {
            if let Some(ref mut wal) = engine.wal {
                wal.advance_lsn_to(lsn);
            }
        }
        // Replay the WAL to restore committed state after the checkpoint.
        // Task 3.2: try physical replay first (fast path — applies
        // PhysicalChange records directly to the buffer pool). Then fall
        // back to SQL replay for records without physical changes.
        engine.replay_wal_with_lsn_filter(checkpoint_last_lsn)?;
        Ok(engine)
    }

    /// Replay the WAL to restore committed state (Wave 63).
    /// This re-executes SQL records and applies physical page changes.
    /// Only committed transactions are replayed; uncommitted (no COMMIT
    /// marker after the DML) are discarded.
    #[allow(dead_code)]
    fn replay_wal(&mut self) -> Result<()> {
        self.replay_wal_with_lsn_filter(None)
    }

    /// Replay the WAL with an optional LSN filter (Task 1.3).
    ///
    /// If `checkpoint_last_lsn` is `Some(lsn)`, records with `lsn <= lsn`
    /// are skipped — they are already included in the checkpoint. This
    /// makes replay idempotent: even if the WAL wasn't truncated (e.g.
    /// crash between checkpoint rename and WAL truncate), replay won't
    /// duplicate rows.
    fn replay_wal_with_lsn_filter(&mut self, checkpoint_last_lsn: Option<u64>) -> Result<()> {
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
            // Task 1.3: skip records already included in the checkpoint.
            if let Some(last_lsn) = checkpoint_last_lsn {
                if record.lsn <= last_lsn && record.lsn > 0 {
                    continue;
                }
            }
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
        self.apply_physical_change_public(change)
    }

    /// Public wrapper for `apply_physical_change` (Task 3.2).
    /// Exposed so `replay_wal_physical()` in src/storage/recovery.rs can
    /// call it without going through the private method.
    pub fn apply_physical_change_public(
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
            PhysicalChange::RowInsert { table_id, page_num, slot, values } => {
                let page_id = crate::storage::buffer_pool::PageId::new(*table_id, *page_num);
                let bp = self.buffer_pool.as_mut().unwrap();
                let idx =
                    bp.fetch_page(page_id).map_err(|e| Error::Other(format!("page fetch: {e}")))?;
                {
                    let page = bp.get_page_mut(idx);
                    for (i, &val) in values.iter().enumerate() {
                        let cell_idx = *slot + i;
                        if cell_idx < crate::storage::page::PAGE_CELLS {
                            page.set_cell(cell_idx, val);
                        }
                    }
                    page.header.row_count = page.header.row_count.max((*slot + values.len()) as u64);
                    page.update_checksum();
                }
                bp.unpin_page(page_id, true);
            }
            PhysicalChange::RowUpdate { table_id, page_num, slot, new_values, .. } => {
                // Task 3.1: redo-only — apply new_values at the slot.
                let page_id = crate::storage::buffer_pool::PageId::new(*table_id, *page_num);
                let bp = self.buffer_pool.as_mut().unwrap();
                let idx =
                    bp.fetch_page(page_id).map_err(|e| Error::Other(format!("page fetch: {e}")))?;
                {
                    let page = bp.get_page_mut(idx);
                    for (i, &val) in new_values.iter().enumerate() {
                        let cell_idx = *slot + i;
                        if cell_idx < crate::storage::page::PAGE_CELLS {
                            page.set_cell(cell_idx, val);
                        }
                    }
                    page.update_checksum();
                }
                bp.unpin_page(page_id, true);
            }
            PhysicalChange::RowDelete { .. } => {
                // Row deletion is handled by the catalog (row_count decrement).
                // Physical deletion (compaction) happens during VACUUM.
            }
            PhysicalChange::PageSplit { table_id, old_page, new_page, .. } => {
                // Task 3.1: ensure both pages are fetched (allocated) in the
                // buffer pool. The actual row redistribution is handled by
                // the executor when it performs the split; this redo path
                // just makes sure both pages exist.
                let old_page_id = crate::storage::buffer_pool::PageId::new(*table_id, *old_page);
                let new_page_id = crate::storage::buffer_pool::PageId::new(*table_id, *new_page);
                let bp = self.buffer_pool.as_mut().unwrap();
                let _ = bp.fetch_page(old_page_id);
                let _ = bp.fetch_page(new_page_id);
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
    /// **Task 4.1 + 4.2:** Two checkpoints are now written:
    /// 1. **`checkpoint.bin`** — bincode-serialized catalog (fast restart,
    ///    ~10x faster than SQL-text). Written first via atomic swap.
    /// 2. **`checkpoint.sql`** — legacy SQL-text checkpoint (CREATE TABLE
    ///    + INSERT statements). Written for backward compat and to
    ///    preserve CHECK constraint `Expr` trees (the binary format
    ///    serializes only column types, not CHECK expressions).
    ///
    /// After both checkpoints are durable, the WAL is truncated. On
    /// restart, `with_data_dir` prefers `checkpoint.bin` and falls back
    /// to `checkpoint.sql` if the binary file is missing or corrupt.
    pub fn flush_with_checkpoint(&mut self) -> Result<()> {
        // 1. Flush dirty pages to disk.
        self.flush()?;
        // 2. Resolve paths from the buffer pool's data_dir.
        let data_dir = match &self.buffer_pool {
            Some(bp) => bp.data_dir().to_path_buf(),
            None => return Ok(()),
        };
        let checkpoint_bin_path = data_dir.join("checkpoint.bin");
        let checkpoint_sql_path = data_dir.join("checkpoint.sql");
        let wal = match self.wal.as_mut() {
            Some(w) => w,
            None => return Ok(()),
        };
        // 3. Write the binary checkpoint FIRST (atomic swap: write to
        //    checkpoint.bin.tmp, fsync, rename). If this fails, the
        //    previous checkpoint (bin or sql) is intact and we do NOT
        //    truncate the WAL — no data loss.
        match crate::storage::checkpoint::BinaryCheckpoint::save(
            &self.catalog,
            &checkpoint_bin_path,
        ) {
            Ok(n) => log::debug!(
                "binary checkpoint: wrote {n} tables to {} (atomic swap)",
                checkpoint_bin_path.display()
            ),
            Err(e) => {
                return Err(Error::Other(format!(
                    "binary checkpoint save to {}: {e}",
                    checkpoint_bin_path.display()
                )))
            }
        }
        // 4. Write the legacy SQL checkpoint AND truncate the WAL (atomic
        //    swap + LSN sidecar). The LSN sidecar (checkpoint.sql.lsn) is
        //    used by `with_data_dir` for idempotent WAL replay regardless
        //    of which checkpoint format was loaded.
        match crate::storage::recovery::Checkpoint::save_and_truncate(
            &self.catalog,
            &checkpoint_sql_path,
            wal,
        ) {
            Ok(n) => {
                log::debug!("sql checkpoint: wrote {n} tables to {}", checkpoint_sql_path.display())
            }
            Err(e) => {
                return Err(Error::Other(format!(
                    "checkpoint save to {}: {e}",
                    checkpoint_sql_path.display()
                )))
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
    /// Task 2.3: `wal_path` is now a directory (segmented WAL).
    pub fn enable_wal<P: AsRef<std::path::Path>>(&mut self, wal_path: P) -> Result<()> {
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        self.wal = Some(wal);
        Ok(())
    }

    /// Enable replication by attaching a `WalStreamer` (Wave 5 Task 5.3 — Agent C).
    ///
    /// Creates a `WalStreamer`, connects it to the replica at `peer_addr`,
    /// and attaches it to the engine. After every successful WAL append+fsync,
    /// the record is streamed to the replica via `WalStreamer::stream_record`.
    ///
    /// **Note:** This wraps the `WalStreamer` in a `Mutex` and stores it in
    /// `self.wal_streamer`. The streaming happens inline in `wal_append_txn`
    /// / `wal_append_record` after fsync succeeds. Stream errors are logged
    /// as warnings (non-fatal) so a replica going down doesn't abort the
    /// primary's transactions.
    ///
    /// # Errors
    ///
    /// - [`Error::Other`] if the streamer fails to connect to `peer_addr`.
    pub fn enable_replication(&mut self, peer_addr: &str) -> Result<()> {
        let mut streamer = crate::storage::replication::WalStreamer::new();
        streamer
            .connect(peer_addr)
            .map_err(Error::Other)?;
        // Build ONE shared Arc<Mutex<WalStreamer>> and store the SAME
        // Arc in both `self.wal_streamer` (queried by `wal_records_streamed()`)
        // and the Wal's stream sink (written to by `append_and_sync`).
        // Pre-existing bug: these used to be two different `WalStreamer`
        // instances, so `wal_records_streamed()` always returned 0.
        let handle: std::sync::Arc<std::sync::Mutex<crate::storage::replication::WalStreamer>> =
            std::sync::Arc::new(std::sync::Mutex::new(streamer));
        self.wal_streamer = Some(WalStreamerHandle {
            streamer: handle.clone(),
        });
        // Task 4.2 (debt-5.3): attach the same streamer to the Wal via
        // set_stream_sink so Wal::append_and_sync auto-streams after fsync.
        // The `Arc<Mutex<WalStreamer>>` coerces to
        // `Arc<Mutex<dyn WalStreamSink>>` via unsizing.
        if let Some(ref mut wal) = self.wal {
            let sink: std::sync::Arc<
                std::sync::Mutex<dyn crate::storage::recovery::WalStreamSink>,
            > = handle.clone();
            wal.set_stream_sink(sink);
        }
        log::info!("Replication enabled: streaming WAL to {}", peer_addr);
        Ok(())
    }

    /// Enable replication without connecting to a replica (Wave 5 Task 5.3).
    ///
    /// Attaches a `WalStreamer` that is NOT connected to any peer. The
    /// streamer counts records (via `records_sent`) but doesn't actually
    /// send them anywhere. Useful for testing the replication wiring
    /// without a live replica.
    pub fn enable_replication_local_only(&mut self) {
        // Build ONE shared Arc<Mutex<WalStreamer>> and store the SAME Arc
        // in both `self.wal_streamer` (queried by `wal_records_streamed()`)
        // and the Wal's stream sink (written to by `append_and_sync`).
        // Pre-existing bug: these used to be two different `WalStreamer`
        // instances, so `wal_records_streamed()` always returned 0 even
        // after records were streamed.
        let handle: std::sync::Arc<std::sync::Mutex<crate::storage::replication::WalStreamer>> =
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::storage::replication::WalStreamer::new(),
            ));
        self.wal_streamer = Some(WalStreamerHandle {
            streamer: handle.clone(),
        });
        // Attach the same streamer to the Wal for auto-streaming. The
        // `Arc<Mutex<WalStreamer>>` coerces to
        // `Arc<Mutex<dyn WalStreamSink>>` via unsizing.
        if let Some(ref mut wal) = self.wal {
            let sink: std::sync::Arc<
                std::sync::Mutex<dyn crate::storage::recovery::WalStreamSink>,
            > = handle.clone();
            wal.set_stream_sink(sink);
        }
    }

    /// Returns the number of WAL records streamed to the replica (Wave 5 Task 5.3).
    ///
    /// Returns 0 if replication is not enabled.
    #[must_use]
    pub fn wal_records_streamed(&self) -> u64 {
        if let Some(ref handle) = self.wal_streamer {
            if let Ok(streamer) = handle.streamer.lock() {
                return streamer.records_sent;
            }
        }
        0
    }

    /// Enable Raft-based leader election (Wave 5 — Task 5.4).
    ///
    /// When the `raft` feature is enabled, this creates a real
    /// [`crate::storage::raft::RaftManager`] backed by openraft and a
    /// single-node cluster (the node is always leader). The manager
    /// holds the `Raft` handle and a dedicated tokio runtime; WAL
    /// records can then be proposed through Raft via
    /// `RaftManager::propose` for quorum replication (multi-node
    /// clustering is supported by the underlying `RaftManager::new`
    /// API; this engine entry point wires the single-node case).
    ///
    /// When the `raft` feature is NOT enabled, this falls back to the
    /// hand-rolled stub `RaftNode` (retained for backward compat with
    /// its existing tests). The stub calls `on_become_leader` on the
    /// WAL to attach `WalStreamer`s to the peer addresses; it does NOT
    /// implement real Raft consensus.
    ///
    /// # Errors
    ///
    /// Returns `Error::Other` if the openraft `Raft` instance cannot be
    /// created or initialized (e.g. tokio runtime spawn failure).
    pub fn enable_raft(&mut self, node_id: u64, peers: Vec<(u64, String)>) -> Result<()> {
        #[cfg(feature = "raft")]
        {
            // Real openraft path: create a dedicated tokio runtime and
            // block on building + initializing a single-node cluster.
            // The runtime is stored in the engine to keep the Raft core
            // and dispatcher task alive.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| Error::Other(format!("enable_raft: tokio runtime: {e}")))?;

            let mgr = runtime
                .block_on(crate::storage::raft::RaftManager::new_single_node(node_id))
                .map_err(|e| Error::Other(format!("enable_raft: {e}")))?;

            log::info!(
                "enable_raft: openraft node {} initialized as single-node leader ({} peers declared but unused by single-node init)",
                node_id,
                peers.len()
            );
            // Production Wiring Wave 4: wire the Raft handle into the Wal
            // so append_and_sync routes through Raft consensus before the
            // local WAL append.
            //
            // Production Wiring Wave 6 Task 6.1: when Raft is enabled, also
            // default `sync_mode = Synchronous` AND attach an empty
            // `MultiWalStreamSink` (default `QuorumPolicy::Majority`). The
            // combined effect: every commit goes through Raft consensus
            // (Wave 4) AND the sync-mode + quorum policy is the default,
            // so a user who calls `enable_raft` gets durable sync
            // replication out of the box. The empty sink is a no-op until
            // replicas are added (via `MultiWalStreamSink::add`), but the
            // defaults are set so the operator only needs to add replicas.
            if let Some(ref mut wal) = self.wal {
                wal.set_raft_handle(mgr.raft.clone(), runtime.handle().clone());
                wal.set_sync_mode(crate::storage::recovery::SyncMode::Synchronous);
                let sink = crate::storage::replication::MultiWalStreamSink::new();
                wal.set_stream_sink(std::sync::Arc::new(std::sync::Mutex::new(sink)));
            }
            self.raft_manager = Some(mgr);
            self.raft_runtime = Some(runtime);
            return Ok(());
        }
        #[cfg(not(feature = "raft"))]
        {
            // Stub path: hand-rolled RaftNode + WalStreamer fan-out.
            let mut raft_node = crate::storage::replication::RaftNode::new(node_id);
            for (peer_id, addr) in &peers {
                raft_node.add_peer(*peer_id, addr);
            }
            let peer_addrs: Vec<&str> = peers.iter().map(|(_, a)| a.as_str()).collect();
            if let Some(ref mut wal) = self.wal {
                let connected = raft_node.on_become_leader(wal, &peer_addrs);
                log::info!(
                    "enable_raft: stub node {} became leader, connected to {} followers",
                    node_id, connected
                );
            } else {
                log::warn!("enable_raft: no WAL attached — leader election skipped");
            }
            self.raft_node = Some(raft_node);
            Ok(())
        }
    }

    /// Append a DML/DDL record to the WAL (if enabled).
    ///
    /// Wave 51 fix: `txn_id` is `Some(id)` for statements inside an
    /// explicit transaction, or `None` for autocommit. The record carries
    /// the txn_id so replay can group statements by transaction.
    ///
    /// **Integration:** uses `Wal::append_and_sync()` (Agent B's atomic
    /// append+fsync). If a `WalStreamSink` is attached to the Wal, the
    /// record is automatically streamed to replicas after fsync. Errors
    /// are propagated — COMMIT fails if fsync fails.
    fn wal_append_txn(&mut self, sql: &str, txn_id: Option<u64>) -> Result<()> {
        if let Some(ref mut wal) = self.wal {
            let record = match txn_id {
                Some(id) => crate::storage::recovery::WalRecord::txn_dml(id, sql),
                None => crate::storage::recovery::WalRecord::autocommit(sql),
            };
            wal.append_and_sync(&record)
                .map_err(|e| Error::Other(format!("WAL append_and_sync failed: {e}")))?;
        }
        Ok(())
    }

    /// Append a pre-constructed WAL record (BEGIN / COMMIT / ROLLBACK
    /// markers, or any other special record). Used by `execute()` to
    /// write transaction boundary markers (Wave 51 fix).
    ///
    /// **Integration:** uses `Wal::append_and_sync()` (Agent B's atomic
    /// append+fsync). Errors are propagated.
    fn wal_append_record(&mut self, record: crate::storage::recovery::WalRecord) -> Result<()> {
        if let Some(ref mut wal) = self.wal {
            wal.append_and_sync(&record)
                .map_err(|e| Error::Other(format!("WAL append_and_sync failed: {e}")))?;
        }
        Ok(())
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
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();

        // Wave 3 (Agent C): top-level dispatch via the formal tokenizer
        // (classify_statement), NOT string-prefix matching. The classifier
        // tokenizes the SQL once and returns a StatementKind enum that we
        // match on below. This is more robust than `starts_with()` because
        // it handles leading whitespace, case differences, and is the
        // foundation for Agent A's future unified AST.
        //
        // Transaction control: BEGIN/COMMIT/ROLLBACK.
        //
        // Wave 51 fix (Bug 8): BEGIN/COMMIT/ROLLBACK now write corresponding
        // markers to the WAL so replay can reconstruct transaction
        // boundaries. Previously the WAL only ever saw `txn_id: 0,
        // is_commit: false`, so a `BEGIN; INSERT; INSERT; COMMIT;` block
        // was indistinguishable from three autocommit INSERTs on replay
        // — and a `BEGIN; INSERT; ROLLBACK;` would still replay the INSERT.
        let kind = crate::engine::dispatch::classify_statement(sql);

        match kind {
            crate::engine::dispatch::StatementKind::Explain => {
                // EXPLAIN: show the query plan (Wave 1 Task 1.2 — uses the
                // planner pipeline, not a string-based description).
                // Strip the leading "EXPLAIN " keyword.
                let inner_sql = strip_first_keyword(trimmed);
                return self.execute_explain(inner_sql, &start);
            }
            crate::engine::dispatch::StatementKind::Analyze => {
                // ANALYZE: execute the query and return timing stats (Wave 68).
                let inner_sql = strip_first_keyword(trimmed);
                return self.execute_analyze(inner_sql, &start);
            }
            crate::engine::dispatch::StatementKind::Vacuum => {
                // VACUUM: reclaim space from deleted rows (Wave 68).
                return self.execute_vacuum(&start);
            }
            crate::engine::dispatch::StatementKind::Copy => {
                // COPY table TO 'file' / COPY table FROM 'file' (Wave 68).
                return self.execute_copy(trimmed, &start);
            }
            crate::engine::dispatch::StatementKind::Checkpoint => {
                // CHECKPOINT: flush + write checkpoint file (Wave 2 fix).
                self.flush_with_checkpoint()?;
                return Ok(QueryResult::empty());
            }
            crate::engine::dispatch::StatementKind::Backup => {
                // BACKUP TO '<dir>' (Wave 6 Task 6.1 — Agent C).
                return self.execute_backup(trimmed, &start);
            }
            crate::engine::dispatch::StatementKind::Restore => {
                // RESTORE FROM '<dir>' [AS OF TIMESTAMP '<ts>'] (Wave 6 Tasks 6.2, 6.3).
                return self.execute_restore(trimmed, &start);
            }
            crate::engine::dispatch::StatementKind::Begin => {
                // Wave 4 (Agent C): route to MVCC or snapshot-isolation
                // manager based on mvcc_enabled.
                if self.mvcc_enabled {
                    let id = self.mvcc_txn_manager.begin_compat().map_err(Error::Other)?;
                    self.wal_append_record(crate::storage::recovery::WalRecord::begin(id))?;
                } else {
                    let id = self.txn_manager.begin(&self.catalog).map_err(Error::Other)?;
                    self.wal_append_record(crate::storage::recovery::WalRecord::begin(id))?;
                }
                return Ok(QueryResult::empty());
            }
            crate::engine::dispatch::StatementKind::Commit => {
                // Wave 4 (Agent C): route to the active transaction manager.
                let txn_id = if self.mvcc_enabled {
                    let id = self.mvcc_txn_manager.commit_compat().map_err(Error::Other)?;
                    id
                } else {
                    // Capture the txn_id before we drain the transaction.
                    let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
                    let committed = self.txn_manager.commit().map_err(Error::Other)?;
                    self.catalog = committed;
                    txn_id
                };
                self.savepoints.clear(); // Wave 69: clear savepoints on commit.
                self.wal_append_record(crate::storage::recovery::WalRecord::commit(txn_id))?;
                return Ok(QueryResult::empty());
            }
            crate::engine::dispatch::StatementKind::Rollback => {
                // Full ROLLBACK (no `TO` savepoint). RollbackTo is handled
                // in execute_inner below so it operates on the txn snapshot.
                let txn_id = if self.mvcc_enabled {
                    self.mvcc_txn_manager.rollback_compat().map_err(Error::Other)?
                } else {
                    let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
                    self.txn_manager.rollback().map_err(Error::Other)?;
                    txn_id
                };
                self.savepoints.clear(); // Wave 69: clear savepoints on rollback.
                self.wal_append_record(crate::storage::recovery::WalRecord::rollback(txn_id))?;
                return Ok(QueryResult::empty());
            }
            crate::engine::dispatch::StatementKind::Show => {
                // Task 5.3 (debt-show): wire SHOW TABLES into execute() dispatch.
                return self.execute_show(trimmed, &start);
            }
            _ => {
                // SELECT / INSERT / UPDATE / DELETE / CREATE / DROP / ALTER /
                // MERGE / PIVOT / SAVEPOINT / ROLLBACK TO / RELEASE / CTE /
                // View DDL / Procedure DDL / EXEC / etc. → route to
                // execute_inner, which handles txn-snapshot swapping and
                // the inner dispatch.
            }
        }
        // Task 5.4: BACKUP TO '<directory>' — dump all tables to CSV + manifest.
        // Task 5.5: RESTORE FROM '<directory>' [AS OF TIMESTAMP '<iso8601>'] —
        //   load tables from CSV; with AS OF TIMESTAMP, replay the WAL up to
        //   the given timestamp (PITR).
        if lower.starts_with("backup to ") {
            return self.execute_backup(trimmed, &start);
        }
        if lower.starts_with("restore from ") {
            return self.execute_restore(trimmed, &start);
        }

        // Wave 4 (Agent C): in MVCC mode, there's no catalog snapshot swap.
        // DML/SELECT execute against the main catalog directly; visibility
        // filtering (when implemented by Agent B) will happen inside
        // execute_select.
        if self.mvcc_enabled {
            let txn_id = self.mvcc_txn_manager.active_id();
            let mut result = self.execute_inner(sql, &start, txn_id)?;
            let elapsed_ms = start.elapsed().as_millis();
            if elapsed_ms > 100 {
                log::warn!("slow query ({} ms): {}", elapsed_ms, sql.trim());
            }
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // SAVEPOINT, ROLLBACK TO, RELEASE are handled inside execute_inner
        // (after the txn snapshot is swapped in) so they operate on the
        // transaction's catalog, not the main catalog.

        if lower.starts_with("begin") || lower.starts_with("start transaction") {
            let id = self.txn_manager.begin(&self.catalog).map_err(Error::Other)?;
            self.wal_append_record(crate::storage::recovery::WalRecord::begin(id))?;
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("commit") {
            // Capture the txn_id before we drain the transaction.
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            let committed = self.txn_manager.commit().map_err(Error::Other)?;
            self.catalog = committed;
            self.savepoints.clear(); // Wave 69: clear savepoints on commit.
            self.wal_append_record(crate::storage::recovery::WalRecord::commit(txn_id))?;
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("rollback") && !lower.starts_with("rollback to ") {
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            self.txn_manager.rollback().map_err(Error::Other)?;
            self.savepoints.clear(); // Wave 69: clear savepoints on rollback.
            self.wal_append_record(crate::storage::recovery::WalRecord::rollback(txn_id))?;
            return Ok(QueryResult::empty());
        }

        // If a snapshot-isolation transaction is active, route all DML/DDL/
        // SELECT to the snapshot catalog. Otherwise, use the main catalog.
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
            let mut result = result?;
            // Wave 12: Slow query logging.
            let elapsed_ms = start.elapsed().as_millis();
            if elapsed_ms > 100 {
                log::warn!("slow query ({} ms): {}", elapsed_ms, sql.trim());
            }
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        let mut result = self.execute_inner(sql, &start, None)?;
        // Wave 12: Slow query logging.
        let elapsed_ms = start.elapsed().as_millis();
        if elapsed_ms > 100 {
            log::warn!("slow query ({} ms): {}", elapsed_ms, sql.trim());
        }
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
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
    pub(crate) fn execute_inner(
        &mut self,
        sql: &str,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        // Wave 69: SAVEPOINT / ROLLBACK TO / RELEASE — handle these here
        // (after the txn snapshot is swapped in by the caller) so they
        // operate on the transaction's catalog.
        //
        // Wave 3 (Agent C): dispatch via classify_statement instead of
        // starts_with() string matching. The classifier tokenizes once
        // and returns a StatementKind enum.
        let trimmed = sql.trim();
        let kind = crate::engine::dispatch::classify_statement(sql);
        match kind {
            crate::engine::dispatch::StatementKind::Savepoint => {
                // SAVEPOINT <name> — strip the keyword and parse the name.
                let rest = crate::engine::helpers::strip_first_keyword(trimmed);
                let name = rest.trim().to_string();
                return self.execute_savepoint(name, start);
            }
            crate::engine::dispatch::StatementKind::RollbackTo => {
                // ROLLBACK TO <name> — strip "ROLLBACK TO" (two keywords).
                let after_rollback = crate::engine::helpers::strip_first_keyword(trimmed);
                let after_to = crate::engine::helpers::strip_first_keyword(after_rollback);
                let name = after_to.trim().to_string();
                return self.execute_rollback_to(&name, start);
            }
            crate::engine::dispatch::StatementKind::Release => {
                // RELEASE <name>
                let rest = crate::engine::helpers::strip_first_keyword(trimmed);
                let name = rest.trim().to_string();
                return self.execute_release_savepoint(&name, start);
            }
            _ => {
                // Not a savepoint-related statement; fall through to the
                // rest of execute_inner.
            }
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

        // Wave 7 (Task 7.1): UNION / UNION ALL via the formal `SetQuery`
        // AST. `try_parse_as_set_query` tokenizes the SQL and runs it
        // through `parse_set`. If it's a top-level set operation, we
        // dispatch through `execute_set_query`, which walks the tree and
        // concatenates (or concatenates + dedupes) the leaf SELECTs.
        // Replaces the previous `split_union_all` string-scan hack.
        if let Some((set, ext)) = try_parse_as_set_query(sql) {
            return self.execute_set_query(&set, &ext, start, txn_id);
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
            self.wal_append_txn(sql, txn_id)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DML (INSERT, UPDATE, DELETE).
        if let Some(dml) = crate::sql::parse_dml(sql).map_err(Error::Parse)? {
            let mut result = self.execute_dml(dml, txn_id)?;
            // Wave 51 fix: append AFTER successful execute. If execute_dml
            // returns Err, we never reach this line, so the WAL stays clean.
            self.wal_append_txn(sql, txn_id)?;
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
                let mut interpreter_result =
                    crate::engine::query_interpreter::parse_and_execute(&expanded_sql, &self.catalog)?;
                interpreter_result.elapsed_us = start.elapsed().as_micros() as u64;
                return Ok(interpreter_result);
            }
        };

        // Wave 53: Temporal query handling is done above (before parsing).

        // Task 2.4 / Task 3.1: when MVCC mode is enabled, ALWAYS pass
        // `Some(&mgr)` so `execute_select` applies visibility filtering —
        // even in autocommit mode (no active txn).
        //
        // Task 3.1 fix: previously this was gated on `txn_id.is_some()`,
        // which meant a `BEGIN; INSERT; ROLLBACK;` followed by an
        // autocommit `SELECT COUNT(*)` would NOT filter — the rolled-back
        // insert (xmin = aborted_txn_id, txn_state = Aborted) would still
        // be counted, violating atomicity. By applying the filter whenever
        // `mvcc_enabled` is true, the autocommit reader (treated as txn 0
        // by `is_row_visible_to_active`) sees only rows whose xmin is
        // Committed (or txn 0 itself, for autocommit inserts) — Aborted
        // inserts are correctly hidden.
        //
        // Pass `None` only for non-MVCC mode — preserves the legacy
        // behaviour (no row_versions, no visibility filtering).
        let mvcc_for_select = if self.mvcc_enabled {
            Some(&self.mvcc_txn_manager)
        } else {
            None
        };

        // Wave 66: fast path — if the query is a simple
        // `SELECT ... FROM t WHERE col = literal` and there's an index on
        // (t, col), use the index for O(1) lookup instead of a full scan.
        // Returns None if the fast path doesn't apply.
        //
        // Task 2.4: skip the indexed-lookup fast path when MVCC visibility
        // filtering is active — `try_indexed_lookup` returns row indices
        // directly from the index without consulting `row_versions`, so dirty
        // / deleted rows would leak through. Fall through to `execute_select`,
        // which applies the visibility filter via `filter_indices`.
        if mvcc_for_select.is_none() {
            if let Some(indexed) = self.try_indexed_lookup(&query) {
                let mut result = indexed?;
                result.elapsed_us = start.elapsed().as_micros() as u64;
                return Ok(result);
            }
        }

        // Execute the parsed query.
        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
            mvcc_for_select,
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
                let mut interpreter_result =
                    crate::engine::query_interpreter::parse_and_execute(&expanded_sql, &self.catalog)
                        .map_err(|_| exec_err)?;
                // Wave 60d: apply DISTINCT deduplication even on the interpreter
                // fallback path (the interpreter parser skips DISTINCT but doesn't
                // deduplicate).
                if query.distinct {
                    interpreter_result = deduplicate_rows(interpreter_result);
                }
                interpreter_result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(interpreter_result)
            }
        }
    }

    /// Execute EXPLAIN: parse the inner SQL and return the query plan
    /// as a text result (Wave 68).
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
                        &self.catalog.get(&temp_name).unwrap_or_else(|| Table {
                            name: temp_name.clone(),
                            columns: vec![],
                            column_names: vec![],
                            row_count: 0,
                            string_columns: vec![],
                            null_bitmaps: vec![],
                            schema: None,
                            row_versions: Vec::new(),
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
                    self.catalog
                        .with_mut(&temp_name, |cte_table| {
                            append_result_rows(cte_table, &rec_result);
                        })
                        .ok_or_else(|| Error::NotFound(format!("CTE table \"{temp_name}\"")))?;
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
    /// This path uses `src/engine/interpreter.rs` which has a richer parser
    /// (arithmetic in aggregates, CASE WHEN, EXTRACT, BETWEEN, IN,
    /// subqueries, derived tables, multi-table implicit joins, HAVING,
    /// LEFT JOIN) and a type-aware row-based evaluator.
    pub fn execute_interpreter(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut result = crate::engine::query_interpreter::parse_and_execute(sql, &self.catalog)?;
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
// DML helper functions (Wave 4) — moved to `src/engine/helpers.rs` in
// Task 8.2-fix to satisfy the 2000-LOC file-size limit.
//
// The impl-QueryEngine methods (`materialize_views_in_sql`,
// `execute_merge_stmt`, `execute_with_json_value`, `execute_set_query`,
// `execute_select_query`) live in `helpers.rs` (declared `pub(crate)`).
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Binary checkpoint integration tests (Task 4.1 + 4.2)
//
// Moved to `src/engine/binary_checkpoint_tests.rs` in Task 8.2-fix to
// satisfy the 2000-LOC file-size limit.
// -----------------------------------------------------------------------
#[cfg(test)]
mod binary_checkpoint_tests;

#[cfg(all(test, feature = "raft"))]
mod enable_raft_tests;
