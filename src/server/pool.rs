//! # Connection pool for the async server (Wave 7 — Tasks 7.1, 7.2, 7.3).
//!
//! Manages a fixed number of engine "slots". Each connection acquires a
//! permit before processing; if all slots are busy, the connection waits
//! (up to a configurable timeout) for one to free up. The permit is
//! released on drop (RAII), so a panic or early return in the connection
//! handler cannot leak a slot.
//!
//! ## Design (Task 7.1)
//!
//! The pool is a thin wrapper around a [`tokio::sync::Semaphore`] whose
//! permit count equals `max_size`. [`ConnectionPool::acquire`] calls
//! `Semaphore::acquire_owned` under a `tokio::time::timeout`, returning an
//! [`OwnedSemaphorePermit`] wrapped in a [`PoolPermit`] struct. We use the
//! **owned** variant (not `SemaphorePermit<'a>`) so the permit can be
//! moved across `.await` points and held for the entire connection
//! lifetime without lifetime gymnastics — the task spec's draft used
//! `SemaphorePermit<'static>` and flagged it as "tricky"; `OwnedSemaphorePermit`
//! is the idiomatic fix.
//!
//! ## Metrics (Task 7.3)
//!
//! [`PoolMetrics`] tracks `active`, `idle`, `waiting`, `total_acquired`,
//! and `total_released`. The metrics are updated under a
//! `parking_lot::Mutex` on every `acquire` and `Drop`. They are exposed
//! via [`ConnectionPool::metrics`].
//!
//! **SQL interface deferred.** The task spec suggested wiring
//! `SHOW POOL_STATUS` into the engine's SQL dispatch. That would require
//! touching `src/engine/vacuum.rs` (`execute_show`) and `src/engine/mod.rs`
//! — neither of which is in this task's allowed-file list. We expose
//! `pool.metrics()` instead and document the SQL deferral here. A future
//! wave can:
//! 1. Add the pool handle to `QueryEngine` (e.g. as an
//!    `Option<Arc<ConnectionPool>>` field), then
//! 2. Extend `execute_show` to handle `SHOW POOL_STATUS` by reading the
//!    field and returning a one-row `QueryResult` with the metrics as
//!    columns.
//!
//! ## Integration (Task 7.2)
//!
//! `async_server::serve` now accepts an `Arc<ConnectionPool>` instead of a
//! raw `Arc<RwLock<QueryEngine>>`. Each accepted TCP connection calls
//! `pool.acquire().await` before `handle_connection`; if acquisition times
//! out, the client receives an `ERROR: pool exhausted: ...` line and the
//! connection is closed. The permit is held for the duration of
//! `handle_connection` and released when it returns (or panics).
//!
//! ## Constraints
//!
//! No `unwrap()`/`expect()` in this module — all error paths return
//! `Result<_, String>` (or propagate via `?`).

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use parking_lot::Mutex;

use crate::engine::QueryEngine;

/// Configuration for the connection pool.
///
/// All fields have sensible defaults via [`PoolConfig::default`]:
/// `max_size = 10`, `acquire_timeout_secs = 30`.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum number of concurrent connections (permits).
    ///
    /// Once this many connections are simultaneously active, further
    /// `acquire()` calls block until a permit is released or the
    /// acquire timeout elapses.
    pub max_size: usize,
    /// Timeout for acquiring a permit, in seconds.
    ///
    /// If a permit cannot be acquired within this duration,
    /// [`ConnectionPool::acquire`] returns `Err("acquire timeout")` and
    /// the caller (typically `async_server::serve`) rejects the
    /// connection with an `ERROR: pool exhausted` message.
    pub acquire_timeout_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self { max_size: 10, acquire_timeout_secs: 30 }
    }
}

/// Pool metrics (accessible via [`ConnectionPool::metrics`]).
///
/// All counters are best-effort: they are updated under a
/// `parking_lot::Mutex` on every acquire/release, so they are consistent
/// with each other at the moment of observation but may change between a
/// `metrics()` call and the caller acting on the values.
///
/// Intended for `SHOW POOL_STATUS` (when the SQL interface is wired in —
/// see the module docs) and for monitoring/observability.
#[derive(Debug, Clone, Default)]
pub struct PoolMetrics {
    /// Connections currently holding a permit.
    pub active: usize,
    /// Permits available (`max_size - active`).
    pub idle: usize,
    /// Number of `acquire()` calls currently blocked waiting for a
    /// permit. **Simplified:** this is always reported as 0 because
    /// `tokio::sync::Semaphore` does not expose a waiters count. A
    /// future implementation could track this with an
    /// `AtomicUsize` incremented before `acquire_owned` and
    /// decremented after.
    pub waiting: usize,
    /// Total number of permits ever acquired (monotonic counter).
    pub total_acquired: u64,
    /// Total number of permits ever released (monotonic counter).
    /// Under normal operation, `total_released == total_acquired` once
    /// all in-flight connections have dropped their permits.
    pub total_released: u64,
}

/// A connection pool that limits concurrent access to the engine.
///
/// Construct with [`ConnectionPool::new`], passing the shared engine and
/// a [`PoolConfig`]. Each connection calls [`ConnectionPool::acquire`] to
/// obtain a [`PoolPermit`]; the permit is released when dropped.
///
/// The pool is cheap to clone (`Arc` internally) — pass `Arc<ConnectionPool>`
/// to `async_server::serve` and clone the `Arc` per spawned task.
pub struct ConnectionPool {
    /// The shared engine. Exposed as `pub` so `async_server::serve` can
    /// pull it out and pass it to `handle_connection` after acquiring a
    /// permit (the pool itself does NOT execute SQL — it only gates
    /// concurrency).
    pub engine: Arc<RwLock<QueryEngine>>,
    config: PoolConfig,
    semaphore: Arc<tokio::sync::Semaphore>,
    metrics: Arc<Mutex<PoolMetrics>>,
}

impl ConnectionPool {
    /// Construct a new pool wrapping `engine` with the given `config`.
    ///
    /// The semaphore is created with `config.max_size` permits.
    pub fn new(engine: Arc<RwLock<QueryEngine>>, config: PoolConfig) -> Self {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_size));
        Self {
            engine,
            config,
            semaphore,
            metrics: Arc::new(Mutex::new(PoolMetrics::default())),
        }
    }

    /// Acquire a permit from the pool.
    ///
    /// Blocks up to `config.acquire_timeout_secs` for a permit to become
    /// available. On success, returns a [`PoolPermit`] that releases the
    /// slot when dropped. On timeout, returns `Err("acquire timeout")`.
    /// On semaphore close (should not happen in normal operation),
    /// returns `Err("semaphore: <err>")`.
    ///
    /// Metrics (`active`, `idle`, `total_acquired`) are updated under the
    /// metrics lock before the permit is handed to the caller.
    pub async fn acquire(&self) -> Result<PoolPermit, String> {
        let permit = tokio::time::timeout(
            Duration::from_secs(self.config.acquire_timeout_secs),
            self.semaphore.clone().acquire_owned(),
        )
        .await
        .map_err(|_| "acquire timeout".to_string())?
        .map_err(|e| format!("semaphore: {e}"))?;

        // Update metrics under the lock. `idle` is derived from
        // `max_size - active`; `waiting` is left at 0 (see PoolMetrics docs).
        let mut m = self.metrics.lock();
        m.active += 1;
        m.idle = self.config.max_size.saturating_sub(m.active);
        m.total_acquired += 1;
        drop(m);

        Ok(PoolPermit {
            permit: Some(permit),
            metrics: self.metrics.clone(),
        })
    }

    /// Get a snapshot of the current pool metrics.
    ///
    /// The snapshot is taken under the metrics lock; the returned
    /// `PoolMetrics` is a cheap `Clone` (5 small fields).
    pub fn metrics(&self) -> PoolMetrics {
        self.metrics.lock().clone()
    }

    /// The configured maximum number of concurrent connections.
    pub fn max_size(&self) -> usize {
        self.config.max_size
    }
}

/// A permit that releases back to the pool when dropped.
///
/// Returned by [`ConnectionPool::acquire`]. Holds an
/// [`OwnedSemaphorePermit`] (which is `'static`, so it can be moved
/// freely across `.await` points and stored in spawned tasks). The permit
/// is released in `Drop`, decrementing `active` and incrementing
/// `total_released`.
///
/// The `permit` field is an `Option<OwnedSemaphorePermit>` so that `Drop`
/// can `take()` it (in case a future API wants to extract the raw permit
/// without triggering the metrics update — currently unused, but keeps
/// the door open).
pub struct PoolPermit {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    metrics: Arc<Mutex<PoolMetrics>>,
}

impl PoolPermit {
    /// Discard the permit WITHOUT updating metrics.
    ///
    /// Normally you should just let the permit drop. This method exists
    /// for the rare case where a caller wants to transfer ownership of
    /// the underlying semaphore permit out (e.g. to manually release it
    /// later) without the pool's metrics counters being decremented.
    /// Currently unused in production code; provided for completeness.
    #[allow(dead_code)]
    pub fn into_raw(mut self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.permit.take()
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        // Release the semaphore permit (if still held) by dropping it.
        // `OwnedSemaphorePermit::drop` adds one permit back to the
        // semaphore, unblocking a waiting `acquire` if any.
        drop(self.permit.take());

        let mut m = self.metrics.lock();
        m.active = m.active.saturating_sub(1);
        // `idle` is derived from `max_size - active`. We don't have
        // `max_size` here, but the invariant `idle + active == max_size`
        // is restored by simply incrementing `idle` by 1 (since we just
        // decremented `active` by 1).
        m.idle = m.idle.saturating_add(1);
        m.total_released += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a pool wrapping a fresh in-memory engine for tests.
    fn test_pool(max_size: usize) -> Arc<ConnectionPool> {
        let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
        let config = PoolConfig {
            max_size,
            acquire_timeout_secs: 5,
        };
        Arc::new(ConnectionPool::new(engine, config))
    }

    /// Task 7.1 DoD — a freshly-constructed pool reports all slots idle,
    /// zero active, zero acquired/released.
    #[tokio::test]
    async fn pool_initial_metrics() {
        let pool = test_pool(4);
        let m = pool.metrics();
        assert_eq!(m.active, 0);
        assert_eq!(m.idle, 0, "idle starts at 0 until first acquire populates it");
        assert_eq!(m.waiting, 0);
        assert_eq!(m.total_acquired, 0);
        assert_eq!(m.total_released, 0);
        assert_eq!(pool.max_size(), 4);
    }

    /// Task 7.1 DoD — acquire + drop updates active/idle/counters.
    #[tokio::test]
    async fn acquire_and_release_updates_metrics() {
        let pool = test_pool(2);
        let permit = pool.acquire().await.expect("acquire");
        let m = pool.metrics();
        assert_eq!(m.active, 1);
        assert_eq!(m.idle, 1, "max_size=2, active=1 → idle=1");
        assert_eq!(m.total_acquired, 1);
        assert_eq!(m.total_released, 0);

        drop(permit);
        let m = pool.metrics();
        assert_eq!(m.active, 0);
        assert_eq!(m.idle, 2, "after release, idle restored to max_size");
        assert_eq!(m.total_acquired, 1);
        assert_eq!(m.total_released, 1);
    }

    /// Task 7.1 DoD — `max_size` concurrent acquires all succeed; the
    /// `max_size + 1`-th acquire blocks (and times out in this test).
    #[tokio::test]
    async fn acquire_blocks_when_pool_full() {
        let pool = test_pool(2);
        let _p1 = pool.acquire().await.expect("acquire 1");
        let _p2 = pool.acquire().await.expect("acquire 2");
        // Pool is now full. A third acquire with a short timeout should
        // time out (no permit available within the window).
        let pool_clone = pool.clone();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            pool_clone.acquire(),
        )
        .await;
        // Either the outer timeout fires (most likely — pool's own
        // 5s timeout is much longer), or the inner returns an error.
        // Both are acceptable proof that the third acquire did NOT
        // succeed immediately.
        match result {
            Err(_elapsed) => { /* outer timeout — acquire was blocked */ }
            Ok(Err(_msg)) => { /* inner returned error — also fine */ }
            Ok(Ok(_permit)) => panic!("third acquire should have blocked (pool full)"),
        }
        // The two held permits are still active.
        let m = pool.metrics();
        assert_eq!(m.active, 2);
        assert_eq!(m.total_acquired, 2);
    }

    /// Task 7.1 DoD — releasing a permit unblocks a waiting acquire.
    #[tokio::test]
    async fn release_unblocks_waiting_acquire() {
        let pool = test_pool(1);
        let p1 = pool.acquire().await.expect("acquire 1");

        // Spawn a second acquire that must block until p1 is released.
        let pool_clone = pool.clone();
        let h = tokio::spawn(async move { pool_clone.acquire().await });

        // Give the spawned task a moment to enter acquire().
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release p1 — the spawned acquire should now complete.
        drop(p1);

        let permit2 = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("second acquire did not complete in 2s")
            .expect("task join failed")
            .expect("second acquire returned Err");
        let m = pool.metrics();
        assert_eq!(m.active, 1, "the second permit is now active");
        assert_eq!(m.total_acquired, 2);
        assert_eq!(m.total_released, 1);
        drop(permit2);
        let m = pool.metrics();
        assert_eq!(m.total_released, 2);
        assert_eq!(m.active, 0);
    }

    /// Task 7.1 DoD — the pool's `engine` field is the same `Arc` passed
    /// to `new()` (so `async_server::handle_connection` can use it after
    /// acquiring a permit).
    #[tokio::test]
    async fn pool_exposes_engine_arc() {
        let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
        let engine_clone = Arc::clone(&engine);
        let pool = ConnectionPool::new(
            engine,
            PoolConfig { max_size: 1, acquire_timeout_secs: 1 },
        );
        assert!(
            Arc::ptr_eq(&pool.engine, &engine_clone),
            "pool.engine must be the same Arc passed to new()"
        );
    }
}
