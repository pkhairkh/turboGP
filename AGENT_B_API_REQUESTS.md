# Agent B — Storage & Transaction API Requests

This file documents API changes made by Agent B (storage & transaction layer)
that require awareness or wiring from other agents (Agent A: SQL frontend,
Agent C: engine/planner). Each entry lists the new/changed API, the engine
integration point, and the commit that introduced it.

All substantive logic lives in `src/storage/` and `src/txn/`. The engine
changes listed here are minimal wiring (calling the new storage APIs) needed
to satisfy the wave DoDs.

---

## Wave 1 — Checkpoint/WAL Truncation Fix

### `Checkpoint::save_and_truncate(catalog, path, wal)` — Task 1.1

**New method** in `src/storage/recovery.rs`:
```rust
impl Checkpoint {
    pub fn save_and_truncate(
        catalog: &Catalog,
        path: &Path,
        wal: &mut Wal,
    ) -> std::io::Result<usize>;
}
```
Saves the checkpoint file AND truncates the WAL atomically. After this call,
the WAL is empty and the checkpoint contains the full catalog state.

**Engine wiring** (`src/engine/mod.rs::QueryEngine::flush_with_checkpoint`):
Changed from `Checkpoint::save(&self.catalog, &path)` to
`Checkpoint::save_and_truncate(&self.catalog, &path, wal)`. This is the
data-corruption bug fix — previously the WAL was not truncated, causing
duplicate rows on every restart.

**Test impact**: `tests/acid.rs::test_acid_durability_commit_survives_checkpoint`
now asserts `count == 10` (was `count >= 10`).

---

## Wave 2 — WAL Durability Correctness

### `Wal::append_and_sync(&mut self, record: &WalRecord) -> std::io::Result<()>` — Task 2.1

**New method** in `src/storage/recovery.rs`. Appends a record AND fsyncs
atomically. Returns `Err` if either the append or the fsync fails.

**Engine wiring** (`src/engine/mod.rs`):
- `wal_append_txn()` and `wal_append_record()` now return `Result<()>`
  and call `wal.append_and_sync()` instead of `wal.append()` + `wal.sync()`
  with errors logged and swallowed.
- All call sites (BEGIN/COMMIT/ROLLBACK markers, DDL/DML execute paths)
  now propagate the error with `?`. COMMIT no longer returns success when
  fsync failed.

### `WalRecord.lsn: u64` + `Wal::current_lsn()` + `Wal::advance_lsn_to()` — Task 2.2

**Already implemented in Task 1.3** (LSN-based idempotent replay). Task 2.2
formalises the API:
- `WalRecord.lsn: u64` field with `#[serde(default)]`.
- `Wal::current_lsn() -> u64` — returns the last assigned LSN.
- `Wal::advance_lsn_to(lsn: u64)` — bumps `next_lsn` past `lsn`.
- `Wal::open()` scans existing records to recover `next_lsn`.
- `Wal::truncate()` preserves `next_lsn` (monotonic across checkpoints).
