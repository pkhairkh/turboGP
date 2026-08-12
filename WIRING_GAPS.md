# turboGP — Production Wiring Gaps

This file enumerates the gaps that prevent turboGP from being a deployable
production database. Each section lists: the current (toy/unwired) state, the
target production state, and the wave that closes the gap. The Orchestrator
updates the "Resolved" flag when the wave's DoD is satisfied.

Base commit: `8e7d013` on `main` (post HA & Concurrency Completion).
Branch: `feat/prod-wiring`.

---

## Gap 1 — Raft is not wired into the write path

- **Current state:** `Wal::append_and_sync` writes only to the local WAL file.
  Although `RaftManager::propose()` exists (when the `raft` feature is enabled)
  and replicates an opaque byte payload through openraft, the engine never
  calls it. Commits succeed without quorum ACK; the Raft log and the WAL can
  diverge.
- **Target state:** When `RaftManager` is attached, `Wal::append_and_sync`
  routes the record bytes through `RaftManager::propose()`. The local WAL
  append happens only after the Raft commit returns. When Raft is not
  enabled, the original local-only path is used (backward compatible).
- **Resolved by:** Wave 4 (Task 4.1 + 4.2).
- **Resolved:** ☑ (commit bab42a7)

---

## Gap 2 — Raft storage is in-memory (`MemStore`)

- **Current state:** `src/storage/raft.rs` ships a hand-rolled `MemStore`
  implementing openraft's `RaftLogStorage` + `RaftStateMachine` traits.
  Log entries, votes, and snapshots live in process memory and are lost the
  instant the process exits. A cluster restart loses all consensus state.
- **Target state:** A disk-backed store (`SledRaftStore`) backed by `sled::Db`
  persists log entries, votes, snapshots, and the applied state machine across
  process restarts. The Raft log survives crash recovery.
- **Resolved by:** Wave 2 (Tasks 2.1–2.3).
- **Resolved:** ☑ (commit d66ddd2)

---

## Gap 3 — Raft network is in-process (`mpsc` channels)

- **Current state:** `ChannelNetworkFactory` / `ChannelNetwork` route all
  RPCs through a process-local `mpsc::Sender`/`Receiver` pair registered in a
  `NetworkRegistry`. A "3-node cluster" is actually three tasks in the same
  process. There is no way to run a turboGP node on a separate machine.
- **Target state:** `TcpRaftNetwork` implements `openraft::RaftNetworkFactory`
  + `RaftNetwork` over `tokio::net::TcpStream`. RPCs are serialized with
  `bincode`. Each node binds a TCP port. A 3-node cluster on localhost
  demonstrates real RPC transport, and a kill-leader test demonstrates
  automatic failover.
- **Resolved by:** Wave 3 (Tasks 3.1–3.3).
- **Resolved:** ☑ (commit 594e696)

---

## Gap 4 — Production pgwire server is a line-based skeleton

- **Current state:** `src/server/async_server.rs` (292 LOC) is a line-based
  tokio TCP server that does not speak the PostgreSQL wire protocol. The
  blocking `src/server/pgwire.rs` (1468 LOC) implements pgwire but blocks
  the executor thread. There is no async, production-grade pgwire server.
- **Target state:** `src/server/async_pgwire.rs` implements the full
  PostgreSQL wire protocol over tokio: startup message, authentication
  (trust + MD5), simple query protocol (Q → RowDescription → DataRow* →
  CommandComplete → ReadyForQuery), extended query protocol (Parse, Bind,
  Describe, Execute, Sync), and proper error/notice propagation. It is the
  default server for production deployments.
- **Resolved by:** Wave 5 (Tasks 5.1–5.4).
- **Resolved:** ☑ (commits 40fa9df, 59df02a, 48403f7, 5a49ae6)

---

## Gap 5 — Connection pool is not on the production path

- **Current state:** `src/server/pool.rs` (372 LOC) implements a
  `ConnectionPool` with permits, but the async server skeleton does not use
  it. Every connection gets unbounded executor access; there is no admission
  control. Under load the engine can be OOM-killed.
- **Target state:** The async pgwire server acquires a `PoolPermit` for every
  accepted connection. When the pool is full, new connections wait (or are
  rejected with a `too_many_connections` error). The pool size is the upper
  bound on concurrent queries.
- **Resolved by:** Wave 5 (Task 5.3).
- **Resolved:** ☑ (commit 48403f7)

---

## Gap 6 — Sync replication is opt-in, not default

- **Current state:** `Wal::sync_mode` defaults to `Asynchronous`.
  `MultiWalStreamSink` exists but `enable_raft()` does not attach one with
  `QuorumPolicy::Majority`. A user who enables Raft still gets
  fire-and-forget WAL flushing; commits can return before quorum.
- **Target state:** `enable_raft()` sets `Wal::sync_mode = Synchronous` and
  attaches a `MultiWalStreamSink` with `QuorumPolicy::Majority`. HA
  deployments get durable sync replication out of the box.
- **Resolved by:** Wave 6 (Task 6.1).
- **Resolved:** ☑ (commit f3d6ba5)

---

## Gap 7 — VACUUM does not reclaim column space

- **Current state:** `src/engine/vacuum.rs` (678 LOC) compacts version
  chains but does not rebuild `columns: Vec<Arc<Vec<u64>>>`. Dead rows
  (where the latest version has a committed `xmax`) remain in the column
  vectors, wasting memory and skewing scans.
- **Target state:** `vacuum_table` removes dead rows from `columns`, decrements
  `row_count`, and compacts `row_versions` chains. After VACUUM,
  `columns[0].len() == row_count == SELECT COUNT(*)`.
- **Resolved by:** Wave 6 (Tasks 6.2–6.3).
- **Resolved:** ☑ (commits ea2d0e7, 4af4c10)

---

## Gap 8 — Three parser hacks remain in `execute_inner`

- **Current state:** `src/engine/mod.rs::execute_inner` dispatches three
  SQL constructs via string-hack helpers in `src/engine/helpers.rs`:
  1. `split_union_all(sql)` — splits `UNION ALL` by string scan instead of
     using the formal `SetQuery::UnionAll` AST.
  2. `parse_merge(sql)` — hand-rolled MERGE parser instead of a formal
     `MergeStmt` AST and `parse_merge_stmt()` in `src/sql/parser.rs`.
  3. `parse_pivot_clause(sql)` + `strip_pivot_clause(sql)` — hand-rolled
     PIVOT clause parser instead of a formal `PivotSpec` AST.
- **Target state:** All three hacks are deleted. UNION ALL dispatches via
  the parsed `SetQuery::UnionAll` AST. MERGE produces a `MergeStmt` AST via
  `parse_merge_stmt()`. PIVOT is part of the SELECT AST as a `PivotSpec`.
  `grep 'split_union_all\|parse_merge\|parse_pivot_clause\|strip_pivot_clause'`
  on `src/engine/` returns zero matches.
- **Resolved by:** Wave 7 (Tasks 7.1–7.4).
- **Resolved:** ☑ (commits 3ba550d, 3400d41, c95786f)

---

## Gap 9 — 400+ missing doc comments suppressed

- **Current state:** `src/lib.rs` carries
  `#![allow(missing_docs, unused_imports, unused_variables, unused_mut,
  unused_assignments, dead_code)]`. The `cargo check` output is silent on
  ~400 missing public-item doc comments and ~50 stale imports. The crate
  compiles "clean" only because the warnings are suppressed.
- **Target state:** `#![allow(missing_docs)]` (and the other suppression
  attributes) are removed. Every public item in `src/**/*.rs` carries a
  `///` doc comment. `cargo check --jobs 1` has zero warnings.
- **Resolved by:** Wave 8 (Tasks 8.1–8.5).
- **Resolved:** ☑ (commit 70b84d0)

---

## Gap 10 — No operational tooling

- **Current state:** There is no CLI for operators. Backup and restore are
  library calls; cluster status requires reading Raft metrics from inside
  the process; VACUUM requires executing `VACUUM` via a SQL connection.
  An operator cannot manage a turboGP deployment from the shell.
- **Target state:** `turboGP admin` CLI exists with `backup`, `restore`,
  `cluster-status`, `vacuum`, and `checkpoint` subcommands. Operators
  can run all five against a `--data-dir` without a SQL connection.
- **Resolved by:** Wave 9 (Tasks 9.1–9.3).
- **Resolved:** ☑ (commits 37266cd, 2298844, 3eb8ad0)

---

## Summary table

| Gap | Wave | Target | Resolved |
|-----|------|--------|----------|
| 1 — Raft write path | 4 | `append_and_sync` → `RaftManager::propose()` | ☑ |
| 2 — Persistent Raft storage | 2 | `SledRaftStore` (disk-backed) | ☑ |
| 3 — TCP Raft network | 3 | `TcpRaftNetwork` over tokio TCP | ☑ |
| 4 — Production pgwire server | 5 | Full PostgreSQL wire protocol over tokio | ☑ |
| 5 — Connection pool on production path | 5 | pgwire server uses `ConnectionPool` | ☑ |
| 6 — Sync replication default | 6 | `enable_raft` sets sync + quorum | ☑ |
| 7 — VACUUM reclaims column space | 6 | Dead rows removed from `Vec<u64>` | ☑ |
| 8 — Remove parser hacks | 7 | Formal AST for UNION ALL / MERGE / PIVOT | ☑ |
| 9 — Real doc comments | 8 | `#![allow(missing_docs)]` removed | ☑ |
| 10 — Operational tooling | 9 | `turboGP admin` CLI | ☑ |
