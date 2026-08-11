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
| 4 | 4.2 Row-version creation | DEBT | Agent B: populate `Table.row_versions` in INSERT/UPDATE/DELETE; add `Table::append_row_version`, `Table::mark_deleted`; update `execute_select` to filter by visibility |
| 4 | 4.1 begin_with_isolation | DEBT | Agent B: add `MvccTxnManager::begin_with_isolation(level)` |
| 4 | 4.3 vacuum dead row versions | DEBT | Agent B: add `MvccTxnManager::vacuum(&mut tables)` that removes dead row versions |
| 5 | 5.2 `Wal::append_and_sync` | DEBT | Agent B: add atomic append+fsync method |
| 5 | 5.4 `RaftNode::on_become_leader` | DEBT | Agent B: add leader-election callback API |

All hacks are tagged with comments referencing this file in the relevant
source files.

---

## API requests for Agent B (storage / txn / catalog)

### Wave 4 — MVCC integration

**Status:** Tasks 4.1, 4.3, 4.4 COMPLETE. Task 4.2 is DOCUMENTED AS DEBT.

#### Task 4.1 — Complete (MVCC manager wired)

`QueryEngine` now has an `mvcc_txn_manager: MvccTxnManager` field
(alongside the legacy `txn_manager: TxnManager`). MVCC mode is opt-in
via `QueryEngine::enable_mvcc()`.

When MVCC mode is enabled:
- `BEGIN` calls `mvcc_txn_manager.begin()` (O(1), no catalog deep-clone)
- `COMMIT` calls `mvcc_txn_manager.commit()`
- `ROLLBACK` calls `mvcc_txn_manager.rollback()`
- DML/SELECT execute against the main catalog directly (no snapshot swap)

When MVCC mode is disabled (default): existing snapshot-isolation behavior
is unchanged.

**API request for Agent B:** The DoD specifies
`begin_with_isolation(IsolationLevel::ReadCommitted)`. The current
`MvccTxnManager::begin()` always uses snapshot isolation. Agent B should
add `begin_with_isolation(level)` so the engine can request
`ReadCommitted` for the MVCC mode.

#### Task 4.2 — Row-version creation (DEBT)

`execute_insert` / `execute_update` / `execute_delete` do NOT yet create
`RowVersion { xmin, xmax }` entries. The `Table.row_versions` field
exists but is `Vec::new()` (empty). As a result, `execute_select` cannot
filter rows by visibility — all rows are visible to all transactions
(like autocommit).

**Why kept:** Agent B hasn't completed the DML→row-version wiring. The
engine's DML path is owned by Agent C (engine/dml.rs) but the row-version
data structure (`RowVersion`, `Table.row_versions`) is owned by Agent B
(storage/table). Agent C cannot modify `Table` to populate `row_versions`
during INSERT.

**Required API from Agent B:**
- `Table::append_row_version(&mut self, version: RowVersion)` — appends a
  new row version to the version chain.
- `Table::mark_deleted(&mut self, row_idx: usize, txn_id: u64)` — sets
  `xmax` on the row version at `row_idx`.
- Ensure `execute_select` is updated to filter by
  `mvcc_txn_manager.is_row_visible(&version)`.

Until Agent B completes this, MVCC mode provides correct transaction ID
tracking and commit/abort state, but does NOT enforce snapshot isolation
visibility. This is documented in `enable_mvcc()`'s doc comment.

#### Task 4.3 — Complete (VACUUM calls MVCC cleanup)

`execute_vacuum()` now calls `mvcc_txn_manager.cleanup_aborted()` when
MVCC mode is enabled, removing commit-state entries for aborted
transactions.

**API request for Agent B:** The DoD specifies
`MvccTxnManager::vacuum(&mut tables)` to remove dead row versions whose
`xmax` is committed and not visible to any active transaction. The current
`cleanup_aborted()` only removes commit-state entries — it doesn't touch
`Table.row_versions`. Agent B should add `vacuum(&mut self, tables: &mut
HashMap<String, Table>)` that scans row versions and removes dead ones.

#### Task 4.4 — Complete (concurrent transactions supported)

`MvccTxnManager` already tracks commit state for multiple transaction IDs.
In MVCC mode, the engine doesn't use the single-active-transaction
`TxnManager`, so concurrent connections (each with their own
`QueryEngine`) can have active transactions simultaneously.

Test: `tests/mvcc_integration.rs::test_mvcc_concurrent_transactions_two_writers`
spawns 2 threads, each does BEGIN/INSERT/COMMIT. Both succeed.

---

### Wave 5 — WAL durability and replication

**Status:** Tasks 5.1, 5.3 COMPLETE. Tasks 5.2, 5.4 are DOCUMENTED AS DEBT.

#### Task 5.1 — Complete (WAL errors raised)

`wal_append_txn` and `wal_append_record` now return `Result<()>` (was `()`).
If `wal.append()` or `wal.sync()` fails, the error is propagated to
`execute()`, which aborts the transaction and returns the error to the user.
Previously, WAL sync failures were silently logged — risking data loss on
crash.

All callers in `execute()` and `execute_inner()` use `?` to propagate errors.

#### Task 5.2 — `Wal::append_and_sync` (DEBT)

The DoD specifies using `Wal::append_and_sync(record)` — an atomic
append + fsync. The current `Wal` API (owned by Agent B) only has separate
`append()` + `sync()` calls. Agent C uses the separate calls (with errors
raised, from Task 5.1).

**Why kept:** `Wal::append_and_sync()` doesn't exist in `src/storage/recovery.rs`.
Agent B should add it so we don't have a window where the append succeeded
but sync failed (the `?` propagation in Task 5.1 catches this, but a single
atomic call would be cleaner).

#### Task 5.3 — Complete (WalStreamer wired)

`QueryEngine::enable_replication(peer_addr: &str)` creates a `WalStreamer`,
connects it to the replica, and attaches it to the engine. After every
successful WAL append+fsync, the record is streamed to the replica via
`WalStreamer::stream_record`.

The streamer is wrapped in a `Mutex` (`WalStreamerHandle`) so it can be
shared across threads. Stream errors are logged as warnings (non-fatal) so
a replica going down doesn't abort the primary's transactions.

`enable_replication_local_only()` attaches a streamer that counts records
but doesn't connect — useful for testing.

`wal_records_streamed()` returns the count of records streamed (for tests).

**Why this approach:** The DoD specifies `Wal::set_streamer(streamer)`. The
current `Wal` API doesn't have `set_streamer`. Agent C stores the streamer
in `QueryEngine.wal_streamer` instead of in the `Wal` object. The effect
is the same: every WAL append triggers a stream call.

**API request for Agent B:** Add `Wal::set_streamer(streamer: WalStreamer)`
and have `Wal::append_and_sync()` automatically call `streamer.stream_record()`
after fsync. This would let the streamer live inside `Wal` (cleaner
encapsulation) instead of in `QueryEngine`.

#### Task 5.4 — `RaftNode` leader election (DEBT/STUB)

`QueryEngine::enable_raft(node_id, peers)` is a **stub** — it logs a warning
and returns `Ok(())`. It does NOT start leader election or wire
`Wal::set_streamer()` on becoming leader.

**Why stubbed:** `RaftNode::on_become_leader(callback)` is not yet
implemented by Agent B. The `RaftNode` struct exists in
`src/storage/replication.rs` but has no callback API for leader election.

**Required API from Agent B:**
- `RaftNode::on_become_leader<F: Fn(&mut Self)>(&mut self, callback: F)`
  — registers a callback fired when this node wins leader election.
- The engine would use this to call `enable_replication(peer_addr)` for
  each peer on becoming leader.

Until Agent B completes this, `enable_raft` is a no-op stub. Documented in
the method's doc comment.

---

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
