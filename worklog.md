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

