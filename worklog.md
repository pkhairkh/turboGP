# turboGP Production Hardening Programme — Worklog

This file is the shared multi-agent worklog for the production hardening
programme. All sub-tasks append to this file.

Base commit: `9ec9b4a` on `main`
Branch: `feat/prod-hardening`
Baseline: 817 lib tests pass

---
Task ID: 1.1
Agent: orchestrator
Task: Provision environment and verify baseline.

Work Log:
- Cloned turboGP at commit 9ec9b4a (post three-branch integration).
- Created branch `feat/prod-hardening` from `main`.
- Verified Rust toolchain: cargo 1.97.1, rustc 1.97.1.
- Verified baseline: `cargo check --jobs 1` passes (467 pre-existing warnings).
- Verified baseline: `cargo test --jobs 1 --lib` → 817 passed, 0 failed.

Stage Summary:
- Environment provisioned. Baseline established at 817 tests.
- Ready to add new dependencies (Task 1.2) and document gaps (Task 1.3).

---
Task ID: 2.1
Agent: general-purpose
Task: Add Table MVCC helpers (append_row_version, mark_deleted, latest_visible_version)

Work Log:
- Read `src/datasource/table.rs`, `src/txn/mvcc.rs` to learn the `Table`
  struct (flat `row_versions: Vec<RowVersion>` field, parallel to rows)
  and the `RowVersion` / `MvccTxnManager` / `MvccTransaction` types.
- Added 3 public methods to `impl Table` in `src/datasource/table.rs`:
  - `append_row_version(version)`: appends to `row_versions`, logs a
    `log::warn!` and no-ops when `row_versions.len() >= row_count`
    (preserves the parallel-to-rows invariant).
  - `mark_deleted(row_idx, txn_id) -> bool`: sets `xmax = Some(txn_id)`
    on the version at `row_idx`; returns `false` for out-of-bounds or
    already-deleted versions (warning logged in the latter case). No
    `unwrap()` — uses `if let Some(existing) = ...`.
  - `latest_visible_version(row_idx, mgr, txn) -> Option<&RowVersion>`:
    returns the visible version at `row_idx` via `mgr.visible()`, or
    `None` for out-of-bounds / invisible.
- All three methods carry doc comments.
- Added unit tests in the existing `#[cfg(test)] mod tests` block:
  `test_append_row_version`, `test_mark_deleted`,
  `test_latest_visible_version` (old deleted version invisible +
  new live version visible, plus out-of-bounds case).
- Verified `cargo check --jobs 1` passes (no new warnings introduced
  beyond the pre-existing 467).
- Verified `cargo test --jobs 1 --lib datasource` → 47 passed, 0
  failed (3 new tests present and green).
- Committed on `feat/prod-hardening` as `c45ec31` with the task
  commit-message template.

Stage Summary:
- 3 helper methods landed on `Table` with doc comments and tests.
- `cargo check` and `cargo test --lib datasource` both pass.
- Only `src/datasource/table.rs` was modified (188 LOC added); the
  `mvcc.rs` and `mod.rs` files required no changes (the existing
  `crate::txn::mvcc::RowVersion` path is reachable from `table.rs`).
- Ready for downstream Wave 2 tasks to consume the helpers.

---
Task ID: 2.2 + 2.3
Agent: general-purpose
Task: Wire `execute_update` and `execute_delete` to populate
`Table.row_versions` with MVCC version chains (set xmax on old
versions; UPDATE appends a new version).

Work Log:
- Read `worklog.md` (Task 2.1 context), `src/engine/dml.rs` (the
  INSERT/UPDATE/DELETE executor), `src/datasource/table.rs` (the
  Task 2.1 helpers), and `src/txn/mvcc.rs` (RowVersion /
  MvccTxnManager reference). Confirmed `execute_insert` already
  populates `row_versions` via direct `table.row_versions.push(...)`
  and used it as the reference pattern.
- Adjusted `Table::append_row_version` in `src/datasource/table.rs`:
  removed the `row_versions.len() >= row_count` no-op bound check.
  Rationale: UPDATE must append a *new* version for an existing row
  (growing the chain beyond `row_count`), which the old bound
  forbade. The flat `row_versions` vec now grows freely; VACUUM is
  responsible for compacting dead versions (xmax set, no active txn
  can see them). The first `row_count` entries stay aligned with
  the logical rows in `columns`. Updated the doc comment and the
  Task 2.1 `test_append_row_version` unit test to assert the 4th
  append now succeeds (len == 4) instead of no-oping.
- `execute_update` (`src/engine/dml.rs`): added an MVCC block right
  after the in-place column-update loop (and BEFORE the temporal-
  sync block, which `drop(table)`s). When `self.mvcc_enabled`:
  for each row in `match_mask` that matched, build `new_values`
  from the already-mutated `columns`, call `table.mark_deleted(
  row_idx, txn_id.unwrap_or(0))` to tombstone the old version
  (sets `xmax`), and if that succeeded call `table.append_row_version(
  RowVersion::new(txn_id.unwrap_or(0), new_values))`. Removed the
  old Task 3.3 placeholder no-op block that sat after the temporal
  sync.
- `execute_delete` (`src/engine/dml.rs`): added an MVCC branch
  immediately after the `deleted == 0` early-return. When
  `self.mvcc_enabled`: iterate `delete_mask`, call
  `table.mark_deleted(row_idx, txn_id.unwrap_or(0))` on each
  matched row (return value discarded via `let _ = ...`), sync the
  temporal sidecar (if any) WITHOUT rebuilding `columns`, and
  early-return with `row_count = deleted`. The existing non-MVCC
  path (rebuild columns + decrement `row_count`) is preserved
  verbatim in the `else` fall-through. This honours the task's
  "Do NOT remove the row from `columns` — that's VACUUM's job"
  directive for MVCC mode.
- Added 2 unit tests in a new `#[cfg(test)] mod tests` block at the
  bottom of `src/engine/dml.rs`:
  - `test_update_sets_xmax`: `BEGIN; INSERT (1,10); UPDATE SET
    v=99 WHERE id=1; COMMIT;` → asserts `row_versions[0].xmax ==
    Some(txn_id)`, `row_versions.len() >= 2`, and the new version
    (`row_versions[1]`) has `xmin == txn_id`, `xmax == None`,
    `values` contains `99`.
  - `test_delete_sets_xmax`: `BEGIN; INSERT (1); DELETE FROM t
    WHERE id=1; COMMIT;` → asserts `row_versions[0].xmax ==
    Some(txn_id)`.
  - Both tests return `std::result::Result<(), Box<dyn std::error::Error>>`
    and use `?` + `ok_or_else` (no `unwrap()`/`expect()` in new
    code, per the constraint).
- Verified `cargo check --jobs 1` passes (466 pre-existing warnings,
  no new ones).
- Verified `cargo test --jobs 1 --lib engine::dml` → 2 passed, 0
  failed (the 2 new tests).
- Verified `cargo test --jobs 1 --lib` → 822 passed, 0 failed
  (820 baseline after Task 2.1 + 2 new; no regressions).
- Verified `cargo test --jobs 1 --test mvcc_integration` (8),
  `--test dml` (13), `--test txn` (11), `--test acid` (12),
  `--test concurrency_test` (2) all still pass — the non-MVCC
  DELETE/UPDATE paths are untouched.
- Committed on `feat/prod-hardening` as `d2888a6` with the task
  commit-message template.

Stage Summary:
- 2 files modified (`src/engine/dml.rs` +197/-31, `src/datasource/
  table.rs` +15/-19); 200 insertions, 31 deletions total.
- UPDATE: sets `xmax` on the old version and appends a new
  `RowVersion` carrying the post-update column values (DoD met).
- DELETE: sets `xmax` on the old version and leaves the column
  data in place for VACUUM (DoD met).
- `Table::append_row_version` now allows the version chain to grow
  beyond `row_count` (required for UPDATE); the Task 2.1 test was
  updated to reflect the new semantics. `mark_deleted` and
  `latest_visible_version` are unchanged.
- Known limitation: the flat `row_versions: Vec<RowVersion>` design
  means `latest_visible_version(row_idx, ...)` still reads
  `row_versions[row_idx]` (the FIRST version for that row), not the
  chain's true latest. A future wave should refactor to
  `Vec<Vec<RowVersion>>` (or have `latest_visible_version` walk the
  tail of the vec) so post-UPDATE visibility checks return the new
  version. Out of scope for Task 2.2/2.3 — the DoD only requires
  `xmax` to be set and a new version to be appended, both of which
  are verified by the new tests.
- Ready for downstream Wave 2 tasks (e.g. visibility filtering in
  `execute_select`, VACUUM compaction of tombstoned versions).

---
Task ID: 2.4
Agent: general-purpose
Task: Wire `execute_select` to filter rows by `MvccTxnManager` visibility,
eliminating dirty reads.

Work Log:
- Read `worklog.md` (Tasks 2.1–2.3 context), `src/engine/executor.rs`
  (the SELECT executor — `execute_select`, `filter_indices`, and all the
  per-shape helpers `execute_count` / `execute_sum` / `execute_avg` /
  `execute_min` / `execute_max` / `execute_count_distinct` /
  `execute_select_star` / `execute_select_column` / `execute_select_multi` /
  `execute_group_by` / `execute_aggregate` / `execute_aggregate_no_group`),
  `src/engine/mod.rs` (`execute_inner` call-site of `execute_select`,
  `execute_readonly` call-site, `try_indexed_lookup` dispatch), and
  `src/txn/mvcc.rs` (`MvccTxnManager`, `RowVersion`, `MvccTransaction`,
  `TxnState`). Confirmed the flat `row_versions: Vec<RowVersion>` design
  (entry `i` = original version for row `i`; UPDATE appends at the end)
  and the approach (c) from the task description: check `row_versions[i]`
  directly for `xmin`/`xmax` visibility.
- Added `MvccTxnManager::is_row_visible_to_active(&self, version:
  &RowVersion) -> bool` in `src/txn/mvcc.rs` (simplified variant of
  `visible()` that uses `active_id()` instead of requiring a full
  `MvccTransaction`). Visibility rule:
    - `xmin` must equal `active_id` (T sees its own writes) OR be
      `TxnState::Committed(_)`; otherwise the version is invisible
      (uncommitted / aborted insert).
    - `xmax == None` → visible (live version).
    - `xmax == Some(active_id)` → invisible (we deleted it).
    - `xmax == Some(other)` → visible iff `other` is NOT
      `TxnState::Committed(_)` (the deleter hasn't committed yet, or
      aborted — version is still live from our perspective).
  When no txn is active (`active_id() == None`), the reader is treated
  as txn `0` (never in `txn_states`), so only committed `xmin` + non-
  committed `xmax` versions are visible — matches autocommit semantics.
- Threaded `mvcc: Option<&crate::txn::MvccTxnManager>` through the SELECT
  execution path in `src/engine/executor.rs`:
    - `execute_select` (new 6th parameter).
    - `filter_indices` (new 3rd parameter) — the single chokepoint. When
      `mvcc.is_some()`, after computing the WHERE-matched indices, it
      `retain`s only those `i` for which `mgr.is_row_visible_to_active(
      &table.row_versions[i])` returns `true`. Rows without a
      `row_versions` entry (backward-compat for non-MVCC tables) are
      kept.
    - All per-shape helpers that call `filter_indices` —
      `execute_group_by`, `execute_aggregate`, `execute_aggregate_no_group`,
      `execute_select_star`, `execute_select_column`, `execute_select_multi`,
      `execute_count`, `execute_sum`, `execute_avg`, `execute_min`,
      `execute_max`, `execute_count_distinct` — received the `mvcc`
      parameter and forward it to `filter_indices`.
- Bypassed the SELECT fast-paths that don't consult `row_versions` when
  MVCC visibility filtering is active (in `src/engine/executor.rs` and
  `src/engine/mod.rs`):
    - `try_planner_pipeline` (executor.rs): skipped when `mvcc.is_some()`
      — the planner's `build_scan_result` returns `table.row_count`
      directly, bypassing `filter_indices`.
    - `dispatch::execute_dispatched` (executor.rs): skipped when
      `mvcc.is_some()` — the `QueryShape::CountAll` branch returns
      `table.row_count`, and `CountFilter` uses the kernel which returns
      a count without consulting `row_versions`.
    - `execute_count`'s `COUNT(*) no-WHERE` fast path (executor.rs):
      skipped when `mvcc.is_some()` — returns `table.row_count` directly.
    - `execute_count`'s kernel-direct path for `WHERE col = N`
      (executor.rs): skipped when `mvcc.is_some()` — the kernel returns
      a count, not indices, so we can't post-filter by visibility.
    - `execute_sum` / `execute_min` / `execute_max`'s `WhereClause::None`
      fast paths (executor.rs): skipped when `mvcc.is_some()` — they
      iterate `table.columns[idx]` directly, ignoring `row_versions`.
      Fall through to `filter_indices`, which applies the visibility
      filter.
    - `try_indexed_lookup` (mod.rs `execute_inner`): skipped when
      `mvcc_for_select.is_some()` — returns row indices from the index
      without consulting `row_versions`.
  Rationale: these fast paths all bypass `filter_indices`, so they would
  leak dirty / deleted rows. Skipping them routes the scan through
  `filter_indices`, which applies the visibility filter. The non-MVCC
  path (`mvcc.is_none()`) is unchanged.
- `src/engine/mod.rs` call-sites:
    - `execute_readonly` passes `None` (read-only path holds `&self`;
      can't have an active MVCC txn — those require a write lock).
    - `execute_inner` computes `mvcc_for_select = if self.mvcc_enabled
      && txn_id.is_some() { Some(&self.mvcc_txn_manager) } else { None }`
      and passes it to `execute_select`. Autocommit (no active txn) →
      `None` → no filtering (preserves legacy behaviour).
- Added two test-only helpers on `QueryEngine` (mod.rs, `#[doc(hidden)]`)
  so the integration test can simulate concurrent transactions on a
  single engine (which is single-transaction-at-a-time via
  `execute("BEGIN")`):
    - `begin_background_txn(&mut self) -> u64` — calls
      `mvcc_txn_manager.begin()` directly (not `begin_compat`); the
      previously-active txn remains InProgress in `txn_states`, while
      `current_active` is overwritten to the new txn.
    - `commit_background_txn(&mut self, txn_id: u64)` — calls
      `mvcc_txn_manager.commit(txn_id)`; only clears `current_active`
      if it matches `txn_id`.
- Added `test_execute_select_filters_uncommitted` to
  `tests/mvcc_integration.rs`:
    - T1: `BEGIN; INSERT INTO t VALUES (1);` (uncommitted).
    - Capture `t1_id = active_id()`.
    - T2: `begin_background_txn()` — T1 stays InProgress; T2 is now
      `current_active`.
    - T2 `SELECT COUNT(*) FROM t` → asserts 0 (T1's insert has
      `xmin = t1_id`, `txn_state(t1_id) = InProgress`, `t1_id !=
      active_id (T2)` → `is_row_visible_to_active` returns false → row
      filtered out). **Dirty read eliminated.**
    - `commit_background_txn(t1_id)` — T1 becomes Committed; T2 stays
      `current_active`.
    - T2 `SELECT COUNT(*) FROM t` → asserts 1 (`txn_state(t1_id)` is
      now `Committed(_)` → `is_row_visible_to_active` returns true →
      row visible). **Commit visible.**
    - Cleanup: `COMMIT` (commits T2).
- Verified `cargo check --jobs 1` passes (467 pre-existing warnings,
  no new warnings in the modified files).
- Verified `cargo test --jobs 1 --lib` → 822 passed, 0 failed (matches
  the Task 2.2/2.3 baseline; no regressions).
- Verified `cargo test --jobs 1 --test mvcc_integration` → 9 passed,
  0 failed (the new `test_execute_select_filters_uncommitted` is green;
  the 8 pre-existing MVCC tests still pass).
- Verified `cargo test --jobs 1 --test dml` (13), `--test txn` (11),
  `--test acid` (12), `--test concurrency_test` (2),
  `--test planner_pipeline_wired` (5), `--test e2e_integration` (6),
  `--test dispatch_path_features` (10) all pass — the non-MVCC SELECT
  path (`mvcc = None`) is unchanged, so the planner pipeline / kernel
  dispatch / indexed-lookup fast paths still fire for non-MVCC queries.
- Committed on `feat/prod-hardening` as `40acc60` with the task
  commit-message template.

Stage Summary:
- 3 source files modified + 1 test file:
    - `src/txn/mvcc.rs`: +46 LOC (new `is_row_visible_to_active` method
      with doc comment).
    - `src/engine/executor.rs`: +248/-79 LOC (`mvcc` parameter threaded
      through `execute_select` + 12 helpers; visibility filter added to
      `filter_indices`; 5 fast paths bypassed when `mvcc.is_some()`).
    - `src/engine/mod.rs`: +62 LOC (`mvcc_for_select` computed in
      `execute_inner`; `try_indexed_lookup` skipped when MVCC active;
      `execute_readonly` passes `None`; 2 test-only helpers
      `begin_background_txn` / `commit_background_txn`).
    - `tests/mvcc_integration.rs`: +76 LOC (1 new test
      `test_execute_select_filters_uncommitted`).
- DoD met: `execute_select` filters by MVCC visibility; dirty reads
  eliminated (verified by `test_execute_select_filters_uncommitted` —
  T2 sees 0 rows while T1 is uncommitted, 1 row after T1 commits).
- Known limitations (out of scope for Task 2.4, documented in code
  comments):
    - The JOIN path (`execute_with_join`) is not yet MVCC-aware — it
      passes `None` for `mvcc` because the joined materialisation
      (`running`) is a cloned `Table` whose `row_versions` don't
      preserve the version chain. A future wave should either filter
      the base/right tables before joining, or thread `mvcc` through
      the JOIN execution.
    - The flat `row_versions: Vec<RowVersion>` design means UPDATE's
      new version (appended at the end of the vec) is NOT visible to
      other transactions until they re-scan — the row-index-based loop
      only checks `row_versions[i]` (the original version). This is
      the same limitation noted in Task 2.2/2.3; resolving it requires
      refactoring to `Vec<Vec<RowVersion>>` or having
      `latest_visible_version` walk the vec's tail.
    - The visibility check uses `txn_state()` (Committed / InProgress /
      Aborted) without consulting `snapshot_id` — it's a coarse check
      that eliminates dirty reads but doesn't provide true snapshot
      isolation (a txn that commits AFTER our BEGIN but BEFORE our
      SELECT would be visible). True snapshot isolation requires
      comparing commit_id to snapshot_id; the existing `visible(&self,
      version, txn: &MvccTransaction)` method does this, but plumbing
      a full `MvccTransaction` through `execute_select` is left for a
      future wave.
- Ready for downstream Wave 2/3 tasks (e.g. VACUUM compaction of
  tombstoned versions, JOIN MVCC-awareness, snapshot-stable visibility).
