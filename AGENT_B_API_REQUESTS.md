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
