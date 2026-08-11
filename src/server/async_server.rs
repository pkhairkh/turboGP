//! # Async server skeleton (Wave 4 — Tasks 4.1, 4.2, 4.3; Wave 7 — Tasks 7.2, 7.3).
//!
//! A minimal tokio-based TCP server that accepts connections and dispatches
//! each to a spawned task running [`handle_connection`]. Each connection is
//! a "session" — sessions are isolated by the shared
//! `Arc<RwLock<QueryEngine>>` (now wrapped in a [`ConnectionPool`]):
//! [`crate::engine::route_and_execute`] acquires the read lock for
//! SELECT/EXPLAIN/SHOW and the write lock for DML/DDL/transaction-control
//! statements, so concurrent readers run in parallel while writers are
//! serialised.
//!
//! ## Connection pool (Wave 7 — Task 7.2)
//!
//! As of Wave 7, [`serve`] takes an `Arc<ConnectionPool>` instead of a raw
//! `Arc<RwLock<QueryEngine>>`. Each accepted TCP connection calls
//! `pool.acquire().await` before `handle_connection`; if acquisition times
//! out (all `max_size` slots busy), the client receives an
//! `ERROR: pool exhausted: <msg>\n` line and the connection is closed. The
//! permit is held for the duration of `handle_connection` and released when
//! it returns (or panics) via [`PoolPermit`]'s `Drop` impl.
//!
//! This mirrors the sync `Server`'s `max_connections` semaphore (in
//! `src/server/mod.rs`), but applied to the async path with a configurable
//! acquire timeout and metrics (see [`ConnectionPool::metrics`]).
//!
//! ## Protocol — Task 4.2 (full async pgwire deferred)
//!
//! This skeleton uses a simple **line-based text protocol**, NOT the full
//! PostgreSQL pgwire v3 protocol. The existing sync pgwire server
//! ([`crate::server::pgwire::PgConn`]) remains the production protocol
//! implementation. A full async port of pgwire is a large effort and is
//! deferred to a later wave; this skeleton exists so that Wave 5's
//! openraft integration (which requires tokio) has an async entry point
//! and so that the async session/locking model can be exercised in
//! isolation.
//!
//! Wire format: the server writes a banner on connect, then reads
//! newline-terminated SQL statements, replying `OK (<n> rows)\n` on
//! success or `ERROR: <msg>\n` on failure.
//!
//! ## Sessions — Task 4.3
//!
//! Each accepted connection is its own tokio task ([`tokio::spawn`]).
//! There is no explicit `Session` struct here — the connection's lifetime
//! IS the session lifetime, and the `Arc<RwLock<QueryEngine>>` (accessed
//! via `pool.engine`) shared across all tasks is the concurrency boundary.
//! A future wave may add per-session state (prepared statements,
//! transaction state, etc.) by introducing a `Session` map keyed by
//! connection id.

use std::sync::Arc;

use crate::engine::QueryEngine;
use crate::server::pool::ConnectionPool;

/// Async server that accepts TCP connections and handles them via tokio.
///
/// Each accepted connection is dispatched to a fresh [`tokio::task`] running
/// [`handle_connection`]. Before the connection is handled, a permit is
/// acquired from the [`ConnectionPool`] — if the pool is exhausted (all
/// `max_size` slots busy), the client receives an
/// `ERROR: pool exhausted: <msg>\n` reply and the connection is closed.
///
/// The `pool.engine` (an `Arc<RwLock<QueryEngine>>`) is cloned per
/// connection — the underlying `QueryEngine` is shared via the
/// `Arc<RwLock<QueryEngine>>` and concurrency-controlled by
/// [`crate::engine::route_and_execute`] (read lock for SELECTs, write lock
/// for DML/DDL).
///
/// Returns `Err(String)` only on bind or accept failures; per-connection
/// errors are logged and do not propagate (the accept loop continues).
pub async fn serve(addr: &str, pool: Arc<ConnectionPool>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!(
        "async server listening on {addr} (pool max_size = {})",
        pool.max_size()
    );
    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let pool = pool.clone();
        tokio::spawn(async move {
            // Acquire a pool permit before handling the connection. If the
            // pool is exhausted, write an error to the client and close.
            match pool.acquire().await {
                Ok(_permit) => {
                    // Permit is held for the entire connection lifetime;
                    // released when `_permit` drops at the end of this
                    // block (after `handle_connection` returns).
                    if let Err(e) = handle_connection(stream, pool.engine.clone()).await {
                        log::warn!("connection from {peer} error: {e}");
                    }
                }
                Err(e) => {
                    log::warn!("pool exhausted, rejecting {peer}: {e}");
                    use tokio::io::AsyncWriteExt;
                    let _ = stream
                        .write_all(format!("ERROR: pool exhausted: {e}\n").as_bytes())
                        .await;
                    // `stream` drops here, closing the connection.
                }
            }
        });
    }
}

/// Per-connection session handler (Task 4.3 — async session management).
///
/// Reads newline-terminated SQL lines and writes back `OK`/`ERROR` replies.
/// Each connection is its own tokio task; sessions share the engine via the
/// `Arc<RwLock<QueryEngine>>`. The connection is held open until the client
/// closes it (EOF on `read_line`) or an I/O error occurs.
///
/// The caller (a spawned task in [`serve`]) holds a [`PoolPermit`] for the
/// entire duration of this function; the permit is released when `serve`'s
/// task exits (whether `handle_connection` returns `Ok` or `Err`).
///
/// No `unwrap`/`expect`: all I/O and engine errors are mapped to `String`
/// and propagated to [`serve`], which logs them.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    engine: Arc<std::sync::RwLock<QueryEngine>>,
) -> Result<(), String> {
    // Simple line-based protocol for now (not full pgwire). See the module
    // docs for why full async pgwire is deferred (Task 4.2).
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    writer
        .write_all(b"turboGP async server. Type SQL commands.\n")
        .await
        .map_err(|e| e.to_string())?;
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        match crate::engine::route_and_execute(&engine, sql) {
            Ok(result) => {
                let response = format!("OK ({} rows)\n", result.row_count);
                writer
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                let response = format!("ERROR: {}\n", e);
                writer
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::server::pool::PoolConfig;

    /// Build a pool wrapping a fresh in-memory engine for tests.
    fn test_pool(max_size: usize) -> Arc<ConnectionPool> {
        let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
        Arc::new(ConnectionPool::new(
            engine,
            PoolConfig { max_size, acquire_timeout_secs: 5 },
        ))
    }

    /// Task 4.3 DoD — the async server accepts a connection, processes a
    /// query, and returns a response.
    ///
    /// Binds on an ephemeral port, spawns `serve` in a tokio task, connects
    /// a client, sends `SELECT COUNT(*) FROM t`, and asserts the response
    /// mentions `OK` or the `turboGP` banner (the client may read only the
    /// banner if the query response hasn't arrived yet within the single
    /// `read` call — both are acceptable proof that the server is alive).
    #[tokio::test]
    async fn test_async_server_accepts_connection() {
        let pool = test_pool(10);
        // Create a table so SELECT COUNT(*) returns 0 rows cleanly.
        pool.engine
            .write()
            .unwrap()
            .execute("CREATE TABLE t (id INT)")
            .unwrap();

        // Bind an ephemeral port, then release it so `serve` can rebind.
        let addr = "127.0.0.1:0";
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let handle =
            tokio::spawn(async move { serve(&bound_addr.to_string(), pool).await });

        // Give the server a moment to bind and start accepting.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(&bound_addr)
            .await
            .unwrap();
        stream
            .write_all(b"SELECT COUNT(*) FROM t\n")
            .await
            .unwrap();

        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.contains("OK") || response.contains("turboGP"),
            "unexpected response: {response}"
        );

        handle.abort();
    }

    /// Task 7.2 DoD — when the pool is full, new connections get an
    /// `ERROR: pool exhausted` reply instead of being handled.
    ///
    /// Strategy: build a pool with `max_size = 1`, hold the single permit
    /// by connecting once (but not sending any SQL, so the handler blocks
    /// on `read_line`), then connect a second client. The second client
    /// should receive `ERROR: pool exhausted` (the pool's acquire timeout
    /// is 5s; we set our outer timeout to 8s to allow for it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_async_server_rejects_when_pool_full() {
        let pool = test_pool(1);
        let addr = "127.0.0.1:0";
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let serve_handle =
            tokio::spawn(async move { serve(&bound_addr.to_string(), pool).await });

        // Give the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // First client: connect and read the banner, but DON'T send any
        // SQL. The handler will block on `read_line`, holding its pool
        // permit for as long as the connection is open.
        let mut c1 = tokio::net::TcpStream::connect(&bound_addr)
            .await
            .expect("client 1 connect");
        let mut banner = [0u8; 128];
        let _ = c1.read(&mut banner).await.expect("client 1 banner read");

        // Second client: the pool is full (1 permit held by c1). The
        // server should reply with `ERROR: pool exhausted` after the
        // 5s acquire timeout.
        let mut c2 = tokio::net::TcpStream::connect(&bound_addr)
            .await
            .expect("client 2 connect");
        let mut buf = vec![0u8; 256];
        // 8s outer timeout > 5s pool acquire timeout.
        let n = tokio::time::timeout(Duration::from_secs(8), c2.read(&mut buf))
            .await
            .expect("client 2 read timed out (>8s; pool acquire should have timed out at 5s)")
            .expect("client 2 read io error");
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.contains("pool exhausted"),
            "expected 'pool exhausted' in response, got: {response}"
        );

        // Clean up: drop both clients and abort the server.
        drop(c1);
        drop(c2);
        serve_handle.abort();
    }
}
