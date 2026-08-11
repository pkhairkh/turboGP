# turboGP HA & Concurrency Completion Programme — Worklog

Base: main @ e839a87 (post production-hardening)
Branch: feat/ha-concurrency
Baseline: 850 lib tests, 466 warnings

---
Task ID: 1.1
Agent: orchestrator
Task: Provision environment and verify baseline.

Work Log:
- Cloned turboGP at commit e839a87.
- Created branch feat/ha-concurrency.
- Rust 1.97.1 installed.
- cargo check passes (466 warnings).
- cargo test --lib: 850 passed, 0 failed.

Stage Summary:
- Baseline verified. Ready to document gaps (Task 1.2).

---
Task ID: 2.1 + 2.2 + 2.3
Agent: ha-concurrency-agent (Wave 2)
Task: Refactor Catalog to use internal parking_lot::RwLock, update all
callers, add a concurrent stress test.

Work Log:
- Catalog (`src/catalog/mod.rs`) now wraps `tables` in
  `parking_lot::RwLock<HashMap<String, Table>>`.
- New API:
  - `get(&self, &str) -> Option<Table>` — read lock, returns an owned
    clone.
  - `with(&self, &str, F: FnOnce(&Table) -> R) -> Option<R>` — scoped
    read without cloning.
  - `with_mut(&self, &str, F: FnOnce(&mut Table) -> R) -> Option<R>` —
    scoped write; replaces the old `get_mut`.
  - `register(&self, Table)` — write lock; signature changed from
    `&mut self` to `&self`.
  - `drop(&self, &str) -> bool` — write lock; `&self`.
  - `table_names(&self) -> Vec<String>` — read lock, owned Strings.
  - `get_column(&self, &str, &str) -> Option<Vec<u64>>` — owned cells.
  - `get_mut` removed.
- Callers updated across 14 files:
  - `engine/ddl.rs`: the 3 ALTER TABLE `get_mut` scopes converted to
    `with_mut` closures (AddColumn / DropColumn / AlterColumnType).
    DropColumn's `self.index_manager.drop(...)` call now runs inside the
    closure; disjoint-field capture (edition 2021) keeps it borrow-clean.
  - `engine/dml.rs`: the 5 `get_mut` sites (FK SET NULL, INSERT, UPDATE,
    DELETE x2) converted to `with_mut`/`with` closures. The DML bodies
    are large; the closures scope the table write guard so the
    `self.temporals` sidecar updates run after the guard is released
    (preserving the original `drop(table)` ordering). DELETE's two
    branches (MVCC tombstone vs. column rebuild) were de-duplicated:
    PK collection uses `with` (read), column rebuild uses a single
    `with_mut` for both temporal and non-temporal tables.
  - `engine/executor.rs`: `let table = catalog.get(...)?; let table =
    &table;` shadow so the existing `&Table`-taking dispatch helpers
    compile unchanged. Same pattern for the JOIN `right` table.
  - `engine/mod.rs`: recursive-CTE `get_mut` → `with_mut`; removed a
    now-redundant `.cloned()` on the owned `get` result; `in_memory()`
    dropped a stale `let mut catalog`.
  - `engine/vacuum.rs`: `write_table_csv(&csv_path, &table)`.
  - `engine/query_interpreter/{expr,subquery,tpc_h_queries_q1_q6,
    q7_q12,q13_q18,q19_q22}.rs`: `ExecTable::from_catalog(tbl, ...)`
    → `from_catalog(&tbl, ...)` (the `_tbl` bindings are now owned
    `Table` rather than `&Table`).
  - `storage/checkpoint.rs`: `save()`'s `filter_map(|n| catalog.get(n))`
    → `catalog.get(&name)` + `.map(|t| serialize_table(&t))`; `load()`
    dropped `let mut catalog`.
  - `storage/recovery.rs`: `catalog.get(name)` → `catalog.get(&name)`.
  - `txn/mod.rs`: `clone_catalog` simplified (no redundant clone, no
    `mut`); test helper likewise.
- Stress test `catalog::tests::test_concurrent_catalog_access`: 10
  threads x 100 iterations of `get` + `with` (read lock) and, for a
  subset, `register` (write lock) against a shared `Arc<Catalog>`.
  Passes with no deadlock or panic; the originally-registered table
  survives intact.
- `cargo check --lib`: 0 errors, 463 warnings (down from 466 — a few
  stale `let mut` removed).
- `cargo test --lib`: 854 passed, 0 failed (850 baseline + 4 new
  catalog tests: `with_scoped_read`, `with_mut_scoped_write`,
  `drop_removes_table`, `test_concurrent_catalog_access`).

Files touched (1 commit, f244f04): 15 files, +535 / -388.

Notes / follow-ups:
- The DML `with_mut` closures in `dml.rs` are large (the INSERT/UPDATE
  bodies are ~290 lines each). Their internal indentation is slightly
  off in places (rustfmt left the multi-line closure bodies as-is); a
  dedicated `cargo fmt` pass on `dml.rs` (and the rest of the crate,
  which is not fmt-clean at baseline) would normalise this. Left
  out-of-scope to keep the commit's diff focused on the refactor.
- The engine-level `Arc<RwLock<QueryEngine>>` still provides the
  coarse-grained write lock for DML/DDL; the catalog's internal RwLock
  is what unlocks concurrent **read-only** queries once the engine
  read guard is held. Per-table locking remains a future wave.
- `helpers.rs` (`execute_merge_stmt`) still does
  `self.catalog.get(...)?.clone()` — the `.clone()` is now redundant
  (`get` already clones) but harmless; left untouched to minimise diff.

Stage Summary:
- Task 2.1 + 2.2 + 2.3 complete. Catalog has an internal
  `parking_lot::RwLock`; all callers updated; concurrent stress test
  passes. Ready for Wave 3.

---
Task ID: 3.1 + 3.2 + 3.3
Agent: ha-concurrency-agent (Wave 3)
Task: Refactor MVCC row_versions to a per-row version chain
(`Vec<Vec<RowVersion>>`), use snapshot_id in visibility checks, and
make UPDATE's new version visible to the updating transaction.

Work Log:

Commit 1 — `feat(3.1): mvcc: refactor Table.row_versions to
Vec<Vec<RowVersion>> (version chain per row)` (f780b73):
- `Table.row_versions` is now `Vec<Vec<RowVersion>>` — one chain per
  logical row. Previously a flat `Vec<RowVersion>` where UPDATE
  appended to the END of the vec, breaking the row-index alignment
  and making the new version invisible to the updating txn.
- `Table::append_row_version(row_idx, version)`: appends to the chain
  at `row_idx`, extending with empty chains when
  `row_idx >= row_versions.len()`.
- `Table::mark_deleted(row_idx, txn_id)`: sets `xmax` on the LAST
  version in `row_versions[row_idx]` (the chain), not on
  `row_versions[row_idx]` directly. Returns `false` for out-of-bounds
  or empty chains.
- `Table::latest_visible_version`: iterates the chain in reverse and
  returns the first visible version (snapshot-isolation read rule).
- `execute_insert` (dml.rs): for each new row at index `i`, calls
  `append_row_version(i, version)`. The new row indices are
  `[row_count - n_new_rows, row_count)` after `row_count` is
  incremented.
- `execute_update` (dml.rs): for each matched row, calls
  `mark_deleted(row_idx, txn_id)` then
  `append_row_version(row_idx, new_version)` — the new version lands
  in the SAME chain as the old, preserving row-index alignment.
- `execute_delete` (dml.rs): `mark_deleted(row_idx, txn_id)` — call
  site unchanged, semantics now chain-aware.
- `executor.rs`: extracted a `row_visible_to_active(table, mgr, i)`
  helper that iterates the chain at `row_versions[i]` in reverse and
  accepts the row if ANY version is visible (latest visible wins —
  short-circuits on first hit). Used by both the serial `retain` path
  in `filter_indices` and the parallel MORS-scan worker in
  `filter_indices_parallel`.
- `checkpoint.rs`: `SerializedTable.row_versions` is now
  `Vec<Vec<SerializedRowVersion>>` (one chain per row) so chain
  boundaries round-trip correctly. Updated `serialize_table` and
  `deserialize_table` to map over chains. Added a new test
  `test_row_versions_chain_roundtrip` covering the UPDATE pattern
  (old tombstoned + new live in the same chain).
- `executor_tests.rs`: all `row_versions` setups wrapped in `vec![...]`
  to match the new per-row chain layout.
- Tests: 855 passed (was 854 + 1 new chain-roundtrip test).

Commit 2 — `feat(3.2,3.3): mvcc: snapshot_id visibility + UPDATE new
version visible to updating txn` (5d6c19a):
- `MvccTxnManager::is_visible_with_snapshot(version, snapshot_id,
  active_txn_id)`: the snapshot-stable variant of
  `is_row_visible_to_active`. `xmin` visible if `xmin ==
  active_txn_id` (self — T sees its own writes, including the new
  version produced by an UPDATE inside the same txn) OR `xmin` is
  `Committed(cid)` with `cid <= snapshot_id`. `xmax` invisible if
  `xmax == active_txn_id` (we deleted it) OR `xmax` is
  `Committed(cid)` with `cid <= snapshot_id` (deleted before our
  snapshot). This is full snapshot isolation, replacing the
  read-committed `is_row_visible_to_active` (which accepted any
  committed `xmin` regardless of `commit_id`).
- `MvccTxnManager::active_snapshot_id()`: returns the active txn's
  `snapshot_id`, or `None` in autocommit mode.
- `executor.rs::row_visible_to_active`: now calls
  `is_visible_with_snapshot` with `active_id().unwrap_or(0)` and
  `active_snapshot_id().unwrap_or_else(|| current_commit_id())`.
  Autocommit readers (`active_txn_id=0`,
  `snapshot_id=current_commit_id`) see all committed data; in-txn
  readers see only their snapshot.
- UPDATE new-version visibility (Task 3.3): the new version has
  `xmin = txn_id`, `xmax = None` → `is_visible_with_snapshot` returns
  `true` (`xmin == active_txn_id`). The old version has `xmax =
  txn_id` → returns `false` (`xmax == active_txn_id`). Net: the
  updating txn sees its own new version immediately and does NOT see
  the old version.
- 8 new tests (863 total, 0 failed):
  - `mvcc::tests::active_snapshot_id_tracks_current_active`
  - `mvcc::tests::is_visible_with_snapshot_blocks_post_snapshot_commits`
  - `mvcc::tests::is_visible_with_snapshot_self_writes` (Task 3.3
    DoD — old version invisible to self, new version visible)
  - `mvcc::tests::is_visible_with_snapshot_autocommit`
  - `engine::dml::tests::test_update_visible_to_updating_txn`
    (Task 3.3 DoD): `BEGIN; INSERT (1,10); UPDATE SET v=99 WHERE
    id=1; SELECT v → 99`.
  - `engine::dml::tests::test_version_chain_roundtrip`: INSERT,
    UPDATE, UPDATE → 3-version chain; latest visible to updating txn
    (v=30). Note: INSERT versions carry empty `values` (column data
    lives in `table.columns`); only UPDATE versions carry non-empty
    `values` (read from the mutated columns at UPDATE time).
  - `engine::executor_tests::test_snapshot_isolation` (Task 3.2 DoD):
    T3 (snapshot=2) sees both rows; a reader with snapshot=1 must NOT
    see T2's row (committed at cid=2 > 1). Verified both via
    `filter_indices` (engine path) and directly via
    `is_visible_with_snapshot`.
  - `engine::executor_tests::test_filter_indices_update_visible_to_updating_txn`:
    `filter_indices` returns row 0 when the active txn's chain has
    the UPDATE pattern (old tombstoned + new live).

Files touched (2 commits):
- Commit 1 (5 files): src/datasource/table.rs, src/engine/dml.rs,
  src/engine/executor.rs, src/engine/executor_tests.rs,
  src/storage/checkpoint.rs. +343 / -176.
- Commit 2 (4 files): src/txn/mvcc.rs, src/engine/dml.rs,
  src/engine/executor.rs, src/engine/executor_tests.rs. +457 / -5.

Constraints honoured:
- No `unwrap()`/`expect()` in new production code (tests use `unwrap()`
  freely, including `.last().unwrap()` on chains known to be
  non-empty).
- Max 3 of the 4 listed files per commit:
  - Commit 1 touched table.rs, dml.rs, executor.rs (3 of 4) +
    checkpoint.rs/executor_tests.rs (auxiliary).
  - Commit 2 touched mvcc.rs, dml.rs, executor.rs (3 of 4) +
    executor_tests.rs (auxiliary).
- `cargo check --jobs 1 --lib` clean (462 warnings, all pre-existing).
- `cargo test --jobs 1 --lib`: 863 passed, 0 failed (was 854 baseline
  + 9 new tests across the two commits).

Notes / follow-ups:
- The engine-level `test_snapshot_isolation` test couldn't use
  `begin_background_txn`/`commit_background_txn` to simulate a truly
  concurrent T2 while T1 is `current_active`: those helpers overwrite
  `current_active` to T2 and clear it on T2's commit, leaving T1
  inactive. The engine's MVCC model is single-active-txn; there's no
  public `mvcc_txn_manager_mut()` accessor (would require touching
  `src/engine/mod.rs`, outside this task's file list). The test
  instead verifies snapshot isolation directly via
  `is_visible_with_snapshot` (the same method `filter_indices` uses)
  and via `filter_indices` itself with a T3 reader that DOES see both
  rows (proving the wiring works). The mvcc-layer
  `mvcc::tests::mvcc_snapshot_isolation` test (pre-existing) covers
  the same property at the `MvccTable` level.
- `is_row_visible_to_active` (the read-committed visibility check) is
  preserved for backward compat — no callers use it after Task 3.2,
  but it's a `pub` method and removing it would be a breaking API
  change. A future cleanup wave could delete it.
- The binary checkpoint format changed (SerializedTable.row_versions
  is now `Vec<Vec<SerializedRowVersion>>`). This is a breaking change
  for existing binary checkpoint files, but the binary checkpoint is
  documented as not being the primary checkpoint format (the
  SQL-text checkpoint is still written for full fidelity), so this is
  acceptable.

Stage Summary:
- Task 3.1 + 3.2 + 3.3 complete. MVCC now has a proper version chain
  per row, snapshot-id-aware visibility (full snapshot isolation), and
  UPDATE's new version is visible to the updating txn immediately.
  863 lib tests pass. Ready for Wave 4.
