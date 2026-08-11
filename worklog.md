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
