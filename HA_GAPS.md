# turboGP HA & Concurrency Gaps

This document lists the 6 remaining production-readiness gaps after the
Production Hardening Programme (commit `e839a87`).

---

## Gap 1: Real Raft Consensus — Stub (no openraft)

**Current:** `RaftNode` is a hand-rolled minimal implementation. No real
leader election, no quorum, no failover.

**Fix (Wave 5):** Replace with `openraft::Raft`. Requires async runtime (Wave 4).

---

## Gap 2: Sync Replication ACK — Flush-based (not real ACK)

**Current:** `SyncMode::Synchronous` calls `flush()` on the TCP stream.
No replica confirmation.

**Fix (Wave 6):** Wire protocol: primary sends `REPLICATE <lsn> <record>`,
replica responds `ACK <lsn>`. Quorum-based.

---

## Gap 3: Catalog Internal RwLock — Deferred

**Current:** `Catalog` has no internal locking. `QueryEngine` is wrapped
in `Arc<RwLock<QueryEngine>>` — single-writer at the engine level.

**Fix (Wave 2):** `parking_lot::RwLock<HashMap>` inside `Catalog`.

---

## Gap 4: Full Snapshot Isolation — Read-committed level

**Current:** Visibility check uses `txn_state()` without `snapshot_id`.
`row_versions` is flat `Vec<RowVersion>`, not a chain per row.

**Fix (Wave 3):** `Vec<Vec<RowVersion>>`, `snapshot_id` comparison,
Serializable conflict detection.

---

## Gap 5: Connection Pooling — None

**Current:** No server-level connection pool. Each session gets its own engine.

**Fix (Wave 7):** `ConnectionPool` with configurable size, metrics, stress test.

---

## Gap 6: Code Quality — 466 warnings

**Current:** 466 compiler warnings (mostly missing docs).

**Fix (Wave 8):** Fix all warnings, run clippy, zero warnings.
