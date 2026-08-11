# turboGP Production Gaps

This document lists the 7 production-readiness gaps in turboGP after the
three-branch integration (commit `9ec9b4a`). Each gap is addressed by a
specific wave in the Production Hardening Programme.

---

## Gap 1: Isolation — Broken (no MVCC visibility filtering)

**Current behaviour:** `execute_select` does NOT filter rows by
`MvccTxnManager::visible()`. Even when MVCC mode is enabled, all rows are
visible to all transactions — dirty reads occur.

**Root cause:** The MVCC machinery exists (`MvccTxnManager`, `RowVersion`,
`visible()`, `scan_visible()`) but `execute_select` never calls it. The
engine's scan path iterates `table.columns` directly without checking
`row_versions`.

**Fix plan (Wave 2):**
1. Add `Table::append_row_version`, `Table::mark_deleted`, `Table::latest_visible_version` helpers.
2. Populate `row_versions` correctly in UPDATE (set `xmax` on old, append new) and DELETE (set `xmax`).
3. Wire `execute_select` to skip rows whose latest `RowVersion` is not visible.
4. Add snapshot isolation + write-write conflict integration tests.

**Wave:** 2 (Tasks 2.1–2.6)

---

## Gap 2: Atomicity — Partial (MVCC ROLLBACK doesn't mark rows invisible)

**Current behaviour:** In MVCC mode, ROLLBACK marks the transaction
aborted in `MvccTxnManager`, but rows already inserted by the transaction
remain visible (no `xmax` set, no visibility filtering).

**Root cause:** `execute_insert` creates `RowVersion { xmin: txn_id, xmax: None }`
but ROLLBACK never sets `xmax = Some(txn_id)` on those versions.

**Fix plan (Wave 3):**
1. On MVCC ROLLBACK, iterate all tables and set `xmax = Some(txn_id)` on every version where `xmin == txn_id` and `xmax == None`.
2. `execute_select` (fixed in Wave 2) filters these out.

**Wave:** 3 (Task 3.1)

---

## Gap 3: Consistency — Partial (UNIQUE/FK/CHECK not enforced)

**Current behaviour:**
- NOT NULL: enforced ✅
- PRIMARY KEY uniqueness: enforced ✅
- UNIQUE: not enforced (test in `acid.rs` is commented out) ❌
- FOREIGN KEY: syntax parsed, not enforced at DML time ❌
- CHECK: syntax parsed, not enforced ❌

**Fix plan (Wave 3):**
1. UNIQUE at INSERT/UPDATE: scan the column for duplicates (excluding NULL).
2. FK at INSERT/UPDATE: check the referenced table has a matching row.
3. FK at DELETE: check no child rows reference the row (or CASCADE/SET NULL).
4. CHECK at INSERT/UPDATE: evaluate the CHECK expression against the new row.

**Wave:** 3 (Tasks 3.2–3.5)

---

## Gap 4: Persistence — SQL-text checkpoint (inefficient)

**Current behaviour:** `flush_with_checkpoint` writes `checkpoint.sql` —
a series of `CREATE TABLE` + `INSERT` statements. On restart, the engine
re-executes every statement. This is slow for large datasets and lossy
for non-numeric types (floats, strings).

**Fix plan (Wave 4):**
1. New `BinaryCheckpoint::save(catalog, path)` using `bincode` to serialize schema + columns + row_versions + string_columns + null_bitmaps.
2. `flush_with_checkpoint` writes `checkpoint.bin` (atomic swap).
3. `with_data_dir` reads `.bin` first, falls back to `.sql` for backward compat.
4. Benchmark: binary must be ≥ 3x faster than SQL-text.

**Wave:** 4 (Tasks 4.1–4.5)

---

## Gap 5: Concurrency — Single-writer

**Current behaviour:** `QueryEngine` is `&mut self` for all mutations.
The server wraps it in `Arc<Mutex<QueryEngine>>`. Only one operation
runs at a time — no read parallelism.

**Root cause:** `Catalog` has no internal locking; `execute_select` takes
`&mut self` (via `execute`); the read-only fast path exists but the
server doesn't use `route_and_execute`.

**Fix plan (Wave 5):**
1. Add internal `RwLock` to `Catalog` (`get` → read lock, `register` → write lock).
2. MORS parallel scan primitive (`crossbeam::scope`, morsel-driven).
3. `execute_select` uses parallel scan for tables > 1000 rows.
4. `route_and_execute(engine: &Arc<RwLock<QueryEngine>>, sql)` routes SELECT→read lock, DML→write lock.
5. Stress test: 10 readers + 1 writer, no deadlocks.

**Wave:** 5 (Tasks 5.1–5.5)

---

## Gap 6: Replication — Best-effort (not HA)

**Current behaviour:**
- `WalStreamer` streams over TCP, best-effort (errors logged, not retried).
- `RaftNode` is a minimal hand-rolled implementation (code says "for demonstration, use openraft in production").
- No synchronous replication mode.
- No failover tested.

**Fix plan (Wave 6):**
1. `SyncMode::Synchronous` — `append_and_sync` waits for replica ACK.
2. Replace stub `RaftNode` with `openraft` (behind `raft` feature).
3. Leader change re-wires `WalStreamer` to new followers.
4. Replica replay with LSN consistency check (resume from `last_applied_lsn + 1`).
5. Failover integration test (kill leader, new leader elected, no data loss).

**Wave:** 6 (Tasks 6.1–6.5)

---

## Gap 7: Durability — Good, but can be improved

**Current behaviour:** WAL with segmented files, LSN tracking, atomic
checkpoint swap, CRC32C page checksums. Crash recovery tested (1000-txn
stress test passes). This is the strongest part of the engine.

**Improvement plan (Wave 4):**
1. Binary checkpoint (Gap 4) also improves durability — no SQL re-execution means no parser-induced corruption on restart.
2. Real WAL timestamps (already added in integration) enable precise PITR.

**Wave:** 4 (covered by binary checkpoint)

---

## Summary

| Gap | Property | Status | Wave |
|-----|----------|--------|------|
| 1 | Isolation | ❌ Broken | 2 |
| 2 | Atomicity | ⚠️ Partial | 3 |
| 3 | Consistency | ⚠️ Partial | 3 |
| 4 | Persistence | ⚠️ SQL-text | 4 |
| 5 | Concurrency | ❌ Single-writer | 5 |
| 6 | Replication | ⚠️ Best-effort | 6 |
| 7 | Durability | ✅ Good (improve via Wave 4) | 4 |
