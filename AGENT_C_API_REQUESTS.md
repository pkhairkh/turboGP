# AGENT_C_API_REQUESTS — Engine & Planner

This file lists APIs that Agent C (engine/planner) needs from Agent A (SQL
frontend) and Agent B (storage/txn/catalog), plus integration notes for the
integration agent that will merge `feat/sql-frontend`, `feat/storage-txn`,
and `feat/engine-planner` into `main`.

---

## Current state (end of Wave 2)

### What Agent C has completed

- **Wave 1:** Planner pipeline wired into `execute_select()`.
  `execute()` now invokes `build_plan → CascadesOptimizer::optimize() →
  Scheduler::execute_plan()` (which internally calls `PlanLowerer::lower()`
  and `KernelTable::select()`). For simple SELECT * and COUNT(*) shapes, the
  planner result is returned directly. For complex shapes, the planner is
  still invoked (incrementing a reachability counter) and the result falls
  back to the existing direct-scan path.

  `EXPLAIN` now prints the planner's plan tree (via `PlanNode`'s `Display`
  impl) instead of a string-based description.

  Integration tests in `tests/planner_pipeline_wired.rs` prove:
  - `SELECT * FROM t` invokes the planner pipeline.
  - `SELECT COUNT(*) FROM t` invokes the planner pipeline.
  - `EXPLAIN SELECT * FROM t WHERE id = 5` prints a tree with `Scan` and
    `Filter`.
  - `KernelTable::select` is called from `execute()` (not just from
    `tests/kernel_pipeline_test.rs`).

- **Wave 2:** Read-only fast path.
  `QueryEngine::execute_readonly(&self, sql: &str) -> Result<QueryResult>`
  takes `&self` (not `&mut self`) so callers can hold `RwLock::read()` and
  run multiple SELECTs concurrently. DML/DDL/transaction control are
  rejected with `Error::Other("read-only transaction: <verb> requires a
  write lock")`.

  `try_readonly_select` is retained as a deprecated alias for backwards
  compatibility with `src/server/pgwire.rs`.

  Public routing helpers:
  - `is_readonly_sql(sql: &str) -> bool` — parser-based classification.
  - `route_and_execute(engine: &Arc<RwLock<QueryEngine>>, sql: &str)` —
    acquires read lock for SELECT, write lock for DML/DDL.

  `src/bin/turbogp.rs` already wraps the engine in `Arc<RwLock<QueryEngine>>`
  (using `parking_lot::RwLock`).

### What's stubbed / pending

- **Task 2.3 (Catalog RwLock):** The Catalog (`src/catalog/mod.rs`) is a
  plain `HashMap`, owned by Agent B. Agent C cannot modify it. However,
  concurrent reads already work because `execute_readonly(&self)` only
  takes `&self.catalog` (a shared reference), and multiple `&self`
  references coexist via the `RwLock<QueryEngine>` wrapper. The
  QueryEngine-level RwLock provides the concurrent-read guarantee the
  DoD is after.

  **API request for Agent B:** Add an internal `RwLock<HashMap>` to
  `Catalog` so it can be shared via `Arc<Catalog>` without an external
  `RwLock` wrapper. Methods:
  - `Catalog::get(&self, name: &str) -> Option<...>` (read lock)
  - `Catalog::register(&self, table: Table)` (write lock, needs `&self`)
  - `Catalog::get_mut(&self, name: &str) -> Option<...>` (write lock,
    needs `&self`, returns a guard)

  This is **not blocking** — the current QueryEngine-level RwLock works.
  It's a nice-to-have for callers that want to share a Catalog without
  wrapping it themselves.

---

## API requests for Agent A (SQL frontend)

### Wave 3 — Parser-based dispatch

**Status:** Task 3.1 COMPLETE. Tasks 3.2, 3.3, 3.4 are DOCUMENTED AS DEBT.

#### Task 3.1 — Complete

`execute()` now dispatches via `classify_statement()` (in
`src/engine/dispatch.rs`), which tokenizes the SQL via
`crate::sql::lexer::tokenize` and inspects the first keyword. All
`starts_with()` calls have been removed from `src/engine/mod.rs`:

```
$ rg -n 'starts_with("' src/engine/mod.rs
# (zero matches)
```

`StatementKind` enum covers: Select, With, Insert, Update, Delete, Create,
Drop, Alter, Begin, Commit, Rollback, RollbackTo, Savepoint, Release, Copy,
Vacuum, Checkpoint, Explain, Analyze, Merge, Backup, Restore, Show, Exec,
Truncate, Other.

`is_readonly_sql()` and `execute_readonly()` also use `classify_statement`
instead of `starts_with()`.

#### Task 3.2 — UNION ALL (DEBT)

The string-based `split_union_all()` hack in `src/engine/helpers.rs` is
**still in use**. It detects `UNION ALL` by string-scanning the SQL and
splitting on the keyword, then executes both halves and concatenates.

**Why kept:** Agent A's unified AST (Wave 2) is not yet available. Once
Agent A adds `SetQuery::Union(left, right)` to the formal parser, the hack
in `execute_inner()` can be removed and `execute_select()` can handle
`SetQuery::Union` directly.

**Where it's used:** `src/engine/mod.rs::execute_inner()`:

```rust
if let Some((left_sql, right_sql)) = split_union_all(sql) {
    let left_result = self.execute_inner(&left_sql, start, txn_id)?;
    let right_result = self.execute_inner(&right_sql, start, txn_id)?;
    return Ok(concatenate_results(left_result, right_result, start));
}
```

**Regression test:** `tests/string_hacks_dispatch.rs::test_union_all_works`
verifies `SELECT * FROM t UNION ALL SELECT * FROM t2` returns the
concatenated rows.

#### Task 3.3 — MERGE (DEBT)

The string-based `parse_merge()` hack in `src/engine/helpers.rs` is **still
in use**. It parses `MERGE INTO target USING source ON ... WHEN MATCHED
THEN ... WHEN NOT MATCHED THEN ...` by string-scanning.

**Why kept:** Agent A hasn't added MERGE to the formal parser. Once they
do, `execute_inner()` can dispatch via `StatementKind::Merge` to a formal
MERGE AST handler.

**Where it's used:** `src/engine/mod.rs::execute_inner()`:

```rust
if let Some(merge) = parse_merge(sql) {
    return self.execute_merge_stmt(merge, start);
}
```

`classify_statement()` already returns `StatementKind::Merge` for MERGE
statements — the formal-parser path is ready to plug in.

**Regression test:** `tests/string_hacks_dispatch.rs::test_merge_works`
verifies a basic MERGE statement executes.

#### Task 3.4 — PIVOT / UNPIVOT (DEBT)

The string-based `parse_pivot_clause()` and `strip_pivot_clause()` hacks in
`src/engine/helpers.rs` are **still in use**. They detect `PIVOT (...)`
by string-scanning.

**Why kept:** Agent A hasn't added PIVOT/UNPIVOT to the formal parser. Once
they do, `execute_inner()` can handle the parsed PivotSpec directly.

**Where it's used:** `src/engine/mod.rs::execute_inner()`:

```rust
if let Some(pivot_spec) = parse_pivot_clause(sql) {
    let stripped = strip_pivot_clause(sql);
    let input = self.execute_inner(&stripped, start, txn_id)?;
    // ... apply pivot transformation
}
```

**Regression test:** `tests/string_hacks_dispatch.rs::test_pivot_works`
verifies a basic PIVOT query executes.

---

## Summary of stubs / debt

| Wave | Task | Status | Action needed |
|------|------|--------|---------------|
| 3 | 3.2 UNION ALL | DEBT | Agent A: add `SetQuery::Union` to parser |
| 3 | 3.3 MERGE | DEBT | Agent A: add MERGE to parser |
| 3 | 3.4 PIVOT | DEBT | Agent A: add PIVOT/UNPIVOT to parser |

All three hacks are tagged with comments referencing this file in
`src/engine/mod.rs::execute_inner()`.

---

## API requests for Agent B (storage / txn / catalog)

### Wave 4 — MVCC integration

**Status:** Not yet started.

`MvccTxnManager` already exists in `src/txn/mvcc.rs` with:
- `begin() -> Result<u64, String>`
- `commit() -> Result<u64, String>`
- `rollback() -> Result<u64, String>`
- `is_visible(txn_id: u64) -> bool`
- `is_row_visible(version: &RowVersion) -> bool`
- `is_active() -> bool`
- `active_id() -> Option<u64>`

Agent C will swap `QueryEngine.txn_manager: TxnManager` for
`MvccTxnManager`. The current `TxnManager` takes a full-catalog snapshot at
BEGIN and swaps it back on COMMIT — the new `MvccTxnManager` uses per-row
version chains (the `RowVersion { xmin, xmax }` already on `Table`).

**API request for Agent B:**
- `MvccTxnManager::begin_with_isolation(level: IsolationLevel) -> Result<u64, String>`
  (currently `begin()` always uses snapshot isolation).
- `MvccTxnManager::vacuum(&mut self, tables: &mut HashMap<String, Table>) -> usize`
  to remove dead row versions whose `xmax` is committed and not visible to
  any active transaction.
- Ensure `Table::row_versions: Vec<RowVersion>` is populated by INSERT/UPDATE
  (it's currently `Vec::new()`).

### Wave 5 — WAL durability and replication

**Status:** Not yet started.

`Wal` already has `append()` + `sync()` as separate calls. Agent C needs:
- `Wal::append_and_sync(record: &WalRecord) -> std::io::Result<()>` — atomic
  append + fsync, so we don't have a window where the append succeeded but
  sync failed.
- `Wal::set_streamer(streamer: WalStreamer)` — registers a streamer that's
  called after every successful append+sync, for replication.
- `RaftNode::on_become_leader(callback)` — hook so the engine can wire
  `Wal::set_streamer()` when this node wins leader election.

**Until Agent B completes these:** Agent C will:
- Make `wal_append_txn` return `Result<()>` (raise errors instead of
  logging them) — this is doable now.
- Keep the separate `wal.append()` + `wal.sync()` calls (documented as
  known debt).
- Stub `enable_replication()` and `enable_raft()` (documented as known
  debt).

### Wave 6 — Backup/restore/PITR

**Status:** Not yet started.

`storage::replication::backup(engine, dir)` and
`storage::replication::restore(engine, dir)` already exist.
`replay_wal_to_timestamp(engine, dir, ts)` also exists.

Agent C will wire `BACKUP TO '<dir>'`, `RESTORE FROM '<dir>'`, and
`RESTORE FROM '<dir>' AS OF TIMESTAMP '<ts>'` SQL commands to these
functions. No additional APIs needed from Agent B.

---

## Integration notes for the integration agent

When merging `feat/engine-planner` with `feat/sql-frontend` and
`feat/storage-txn`:

1. **`src/server/pgwire.rs`** (owned by another agent) currently calls
   `try_readonly_select()`. This is retained as a deprecated alias for
   `execute_readonly()`. The server should be updated to call
   `route_and_execute()` for automatic read/write lock routing.

2. **`src/server/session.rs`** should similarly use `route_and_execute()`.

3. **The string-based dispatch hacks** in `src/engine/mod.rs::execute()`
   (`starts_with("select")`, `starts_with("insert")`, etc.) will be
   removed in Wave 3 once Agent A's unified AST is available. Until then,
   they're tagged with comments referencing `AGENT_C_API_REQUESTS.md`.

4. **`MvccTxnManager`** (Wave 4) and **`Wal::set_streamer`** (Wave 5) are
   not yet wired into `QueryEngine`. The integration agent should verify
   Agent B has completed these APIs before merging.
