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

### WAL segmentation — Task 2.3

**Changed** `Wal::open()` to take a **directory** (not a file path). The WAL
now manages segment files `wal-<N>.log` inside the directory. Segments
rotate at 64 MB (`DEFAULT_SEGMENT_LIMIT`).

**Engine wiring**: `with_data_dir()` now creates a `wal/` subdirectory inside
`data_dir` and passes it to `Wal::open()`. Previously it passed
`data_dir.join("wal.log")` (a single file).

**New API**:
- `Wal::open(dir)` — opens a segmented WAL in the directory.
- `Wal::open_with_segment_limit(dir, limit)` — for tests.
- `Wal::segment_count() -> io::Result<usize>` — number of segment files.
- `Wal::current_segment() -> u64` — current segment number.
- `Wal::read_all()` reads across ALL segments in order.
- `Wal::truncate()` deletes all segments and starts fresh at 0.

### Group commit framework — Task 2.4

**New API** in `src/storage/recovery.rs`:
- `Wal::append_async(&mut self, record) -> io::Result<u64>` — appends without
  fsync, returns the assigned LSN.
- `Wal::sync_to_lsn(&mut self, lsn: u64) -> io::Result<()>` — fsyncs only if
  `lsn > last_synced_lsn` (no-op otherwise).
- `Wal::flush_group(&mut self, lsns: &[u64]) -> io::Result<()>` — fsyncs once
  for a batch of LSNs (finds the max, calls `sync_to_lsn(max)`).

**Engine usage** (optional, Agent C can wire later): instead of calling
`append_and_sync()` per transaction, the engine can:
1. `let lsn = wal.append_async(&record)?;`
2. Collect LSNs from concurrent transactions.
3. `wal.flush_group(&lsns)?;` — one fsync for all.

---

## Wave 3 — Page-Level Delta Store

### `PhysicalChange` variants — Task 3.1

**Expanded** enum in `src/storage/recovery.rs`:
- `PageAlloc { table_id, page_num }`
- `CellUpdate { table_id, page_num, cell_index, old_value, new_value }`
- `RowInsert { table_id, page_num, slot, values }`
- `RowUpdate { table_id, page_num, slot, old_values, new_values }` (NEW)
- `RowDelete { table_id, page_num, slot }`
- `PageSplit { table_id, old_page, new_page, split_point }` (NEW)

**Engine wiring**: `apply_physical_change()` handles the new `RowUpdate` and
`PageSplit` variants.

### `replay_wal_physical()` — Task 3.2

**New function** in `src/storage/recovery.rs`:
```rust
pub fn replay_wal_physical(engine: &mut QueryEngine, wal: &Wal) -> io::Result<ReplayStats>;
```
Applies `PhysicalChange` records directly to the buffer pool without
re-executing SQL. The engine can call this as a fast-path before falling
back to `replay_wal()` for SQL-only records.

### Buffer pool API — Task 3.3

**New methods** on `BufferPool` in `src/storage/buffer_pool.rs`:
- `mark_dirty(page_id)` — marks a page dirty without unpinning.
- `flush_page(page_id)` — writes a single dirty page to disk.
- `flush_all()` (already existed) — writes all dirty pages.

### Page checksum API — Task 3.4

**New methods** on `Page` in `src/storage/page.rs`:
- `write(&mut self) -> Vec<u8>` — computes CRC32C, returns serialized bytes.
- `read(bytes) -> Result<Page>` — deserializes + verifies checksum.
  Returns `Err(Corruption)` on mismatch (torn-page detection).

---

## Wave 4 — MVCC Implementation

### `MvccTxnManager` (replaces `TxnManager`) — Tasks 4.1–4.5

**Full redesign** of `src/txn/mvcc.rs`. The engine (Agent C) should migrate
from `TxnManager` (deep-clone snapshot) to `MvccTxnManager` (O(1) BEGIN).

**Key API**:
- `MvccTxnManager::new() -> Self`
- `MvccTxnManager::begin() -> MvccTransaction` — O(1), no catalog clone.
- `MvccTxnManager::commit(txn_id) -> TxnId` — returns commit_id.
- `MvccTxnManager::rollback(txn_id)`
- `MvccTxnManager::visible(&self, version: &RowVersion, txn: &MvccTransaction) -> bool`
- `MvccTxnManager::scan_visible(&self, table: &MvccTable, txn: &MvccTransaction) -> Vec<&RowVersion>`
- `MvccTxnManager::check_write_conflict(&self, table, txn, row_idx) -> Result<(), ConflictError>`
- `MvccTxnManager::vacuum(&mut self, tables: &mut [MvccTable]) -> usize`

**New types**:
- `RowVersion { xmin, xmax: Option<TxnId>, values: Vec<u64>, deleted: bool }`
- `MvccTable { name, column_names, rows: Vec<Vec<RowVersion>> }`
- `MvccTransaction { id, snapshot_id, state, isolation_level }`
- `TxnState { InProgress, Committed(TxnId), Aborted }`
- `ConflictError { message, conflicting_txn }`

**Note**: `TxnManager` (the old deep-clone implementation) is kept in
`src/txn/mod.rs` for backward compatibility. Agent C should migrate the
engine to use `MvccTxnManager` when ready.

---

## Wave 5 — Replication Wiring

### `WalStreamSink` trait + `Wal::set_stream_sink()` — Task 5.1

**New trait** in `src/storage/recovery.rs`:
```rust
pub trait WalStreamSink: Send {
    fn stream(&mut self, record: &WalRecord) -> Result<usize, String>;
}
```
- `Wal::set_stream_sink(sink: Arc<Mutex<dyn WalStreamSink>>)` attaches a sink.
- `Wal::append_and_sync()` calls `sink.stream(&record)` after fsync.
- `WalStreamer` implements `WalStreamSink` (in `src/storage/replication.rs`).

### `WalReceiver::run_apply_loop()` — Task 5.2

**New method** in `src/storage/replication.rs`:
```rust
pub fn run_apply_loop<F>(&mut self, apply: F) -> Result<u64, String>
where F: FnMut(&WalRecord) -> Result<(), String>;
```
Continuous apply loop with error-returning callback. `set_continue_on_error`
configures whether the loop continues after an apply error.

### `RaftNode::on_become_leader()` — Task 5.3

**New methods** in `src/storage/replication.rs`:
- `RaftNode::on_become_leader(wal: &mut Wal, peer_addrs: &[&str]) -> usize`
  — connects a `WalStreamer` to each follower, attaches a `MultiWalStreamSink`
  to the Wal.
- `RaftNode::on_demote(wal: &mut Wal)` — detaches the sink.

### `BACKUP TO` / `RESTORE FROM` SQL — Tasks 5.4 + 5.5

**Engine wiring** (`src/engine/mod.rs`):
- `execute()` dispatches `BACKUP TO '<dir>'` → `execute_backup()`.
- `execute()` dispatches `RESTORE FROM '<dir>' [AS OF TIMESTAMP '<ts>']` →
  `execute_restore()`.
- `restore()` loads CSVs via `engine.load_csv()` (bypasses COPY's security).
- `RESTORE ... AS OF TIMESTAMP` calls `replay_wal_to_timestamp()`.

---

## Wave 6 — Isolation Levels

### `IsolationLevel` enum — Task 6.1

**New enum** in `src/txn/mvcc.rs`:
```rust
pub enum IsolationLevel {
    ReadUncommitted,  // dirty reads
    ReadCommitted,    // fresh snapshot per statement
    RepeatableRead,   // snapshot at BEGIN (default)
    Serializable,     // RepeatableRead + conflict detection
}
```
- `MvccTxnManager::begin_with_isolation(level) -> MvccTransaction`
- `MvccTransaction.isolation_level` field
- `visible()` respects the level (ReadUncommitted allows dirty reads)

### Engine integration summary

The engine (`src/engine/mod.rs`) has been updated with minimal wiring changes:
1. `wal` field is now `pub` (for `Checkpoint::load` to detach it).
2. `catalog` field is now `pub` (for `replication::list_tables`).
3. `flush_with_checkpoint()` calls `Checkpoint::save_and_truncate()`.
4. `wal_append_txn()` / `wal_append_record()` return `Result<()>` and use
   `append_and_sync()`.
5. `with_data_dir()` reads `Checkpoint::read_last_lsn()`, calls
   `wal.advance_lsn_to()`, and passes last_lsn to `replay_wal_with_lsn_filter()`.
6. `apply_physical_change_public()` is a public wrapper for physical replay.
7. `execute()` dispatches `BACKUP TO` and `RESTORE FROM` commands.

All other engine paths remain unchanged. Agent C should migrate the
transaction manager from `TxnManager` to `MvccTxnManager` when ready.
