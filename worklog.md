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
