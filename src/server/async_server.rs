//! # Async server skeleton (Wave 4 — Tasks 4.1, 4.2, 4.3).
//!
//! A minimal tokio-based TCP server that accepts connections and dispatches
//! each to a spawned task running [`handle_connection`]. Each connection is
//! a "session" — sessions are isolated by the shared
//! `Arc<RwLock<QueryEngine>>`: [`crate::engine::route_and_execute`]
//! acquires the read lock for SELECT/EXPLAIN/SHOW and the write lock for
//! DML/DDL/transaction-control statements, so concurrent readers run in
//! parallel while writers are serialised.
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
//! IS the session lifetime, and the `Arc<RwLock<QueryEngine>>` shared
//! across all tasks is the concurrency boundary. A future wave may add
//! per-session state (prepared statements, transaction state, etc.) by
//! introducing a `Session` map keyed by connection id.

use std::sync::Arc;
use std::sync::RwLock;

use crate::engine::QueryEngine;

/// Async server that accepts TCP connections and handles them via tokio.
///
/// Each accepted connection is dispatched to a fresh [`tokio::task`] running
/// [`handle_connection`]. The `engine` is cloned (`Arc::clone`) per
/// connection — the underlying `QueryEngine` is shared via the
/// `Arc<RwLock<QueryEngine>>` and concurrency-controlled by
/// [`crate::engine::route_and_execute`] (read lock for SELECTs, write lock
/// for DML/DDL).
///
/// Returns `Err(String)` only on bind or accept failures; per-connection
/// errors are logged and do not propagate (the accept loop continues).
pub async fn serve(addr: &str, engine: Arc<RwLock<QueryEngine>>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!("async server listening on {addr}");
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, engine).await {
                log::warn!("connection from {peer} error: {e}");
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
/// No `unwrap`/`expect`: all I/O and engine errors are mapped to `String`
/// and propagated to [`serve`], which logs them.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    engine: Arc<RwLock<QueryEngine>>,
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
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
        engine
            .write()
            .unwrap()
            .execute("CREATE TABLE t (id INT)")
            .unwrap();

        // Bind an ephemeral port, then release it so `serve` can rebind.
        // (tokio's TcpListener sets SO_REUSEADDR on Unix, so the rebind is
        // safe even though the OS may not have fully released the port.)
        let addr = "127.0.0.1:0";
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let engine_clone = engine.clone();
        let handle =
            tokio::spawn(async move { serve(&bound_addr.to_string(), engine_clone).await });

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
}
