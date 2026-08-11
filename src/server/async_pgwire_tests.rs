//! Tests for [`super`]. Lives in its own file (registered as
//! `#[cfg(test)] mod async_pgwire_tests;` inside `async_pgwire.rs`) so
//! the production module stays compact.
//!
//! Tests use raw `tokio::net::TcpStream` byte-level clients (no actual
//! `psql` dependency) so they exercise the wire protocol exactly as a
//! real client would.

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::AsyncPgwireServer;
use crate::engine::QueryEngine;

// ---------------------------------------------------------------------------
// Byte-level pgwire client helpers
// ---------------------------------------------------------------------------

/// Build a startup message (no tag byte): int32(length) + int32(196608)
/// + `user\0<user>\0\0`. Returns the raw bytes ready to `write_all`.
fn build_startup(user: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(format!("user\0{user}\0\0").as_bytes());
    let len = body.len() as u32 + 4;
    let mut msg = Vec::with_capacity(len as usize);
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Build a 'Q' (simple Query) message: 'Q' + int32(length) + sql\0.
fn build_query(sql: &str) -> Vec<u8> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let len = body.len() as u32 + 4;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(b'Q');
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Read one backend message: tag byte + 4-byte BE length + body.
///
/// Returns `(tag, body)`. Panics on EOF (tests use `?`/`unwrap`).
async fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag).await.expect("read tag");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    assert!(len >= 4, "message length {len} < 4");
    let body_len = len - 4;
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).await.expect("read body");
    (tag[0], body)
}

/// Read messages until a `ReadyForQuery` ('Z') message arrives.
/// Returns the collected `(tag, body)` pairs (including the final 'Z').
async fn read_until_ready(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut msgs = Vec::new();
    loop {
        let (tag, body) = read_message(stream).await;
        let is_ready = tag == b'Z';
        msgs.push((tag, body));
        if is_ready {
            return msgs;
        }
    }
}

/// Extract the command-complete tag string (without the trailing NUL)
/// from a 'C' message body.
fn cc_tag(body: &[u8]) -> String {
    let mut end = body.len();
    while end > 0 && body[end - 1] == 0 {
        end -= 1;
    }
    String::from_utf8_lossy(&body[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Task 5.1: startup + simple query round trip
// ---------------------------------------------------------------------------

/// Task 5.1 DoD — start a server bound to 127.0.0.1:0, connect with a
/// raw TcpStream, send the startup message, then run CREATE TABLE +
/// INSERT + SELECT and verify the wire responses come back correctly.
#[tokio::test]
async fn async_pgwire_startup_and_simple_select_round_trip() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    let server = AsyncPgwireServer::bind("127.0.0.1:0", engine)
        .await
        .expect("bind");
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // 1. Send startup.
    stream.write_all(&build_startup("alice")).await.expect("write startup");

    // 2. Read AuthenticationOk (R + len=8 + int32(0)).
    let (tag, body) = read_message(&mut stream).await;
    assert_eq!(tag, b'R', "expected AuthenticationOk 'R', got {tag:#x}");
    assert_eq!(body.len(), 4, "AuthenticationOk body should be 4 bytes");
    assert_eq!(&body, &0u32.to_be_bytes(), "AuthenticationOk payload should be 0");

    // 3. Skip ParameterStatus ('S') and BackendKeyData ('K') messages
    //    until we see ReadyForQuery ('Z').
    let msgs = read_until_ready(&mut stream).await;
    assert!(msgs.iter().any(|(t, _)| *t == b'S'), "expected at least one ParameterStatus");
    assert!(msgs.iter().any(|(t, _)| *t == b'K'), "expected BackendKeyData");
    let last = msgs.last().expect("at least one message");
    assert_eq!(last.0, b'Z', "last message should be ReadyForQuery");
    assert_eq!(last.1, b"I", "txn status should be 'I' (idle)");

    // 4. CREATE TABLE.
    stream.write_all(&build_query("CREATE TABLE t (id INT)")).await.expect("write create");
    let msgs = read_until_ready(&mut stream).await;
    // CREATE returns NoData ('n') + CommandComplete ('C') + ReadyForQuery ('Z').
    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete");
    assert_eq!(cc, "CREATE", "create command tag, got {cc:?}");

    // 5. INSERT.
    stream.write_all(&build_query("INSERT INTO t VALUES (1)")).await.expect("write insert");
    let msgs = read_until_ready(&mut stream).await;
    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete for INSERT");
    assert_eq!(cc, "INSERT 0 1", "insert command tag, got {cc:?}");

    // 6. SELECT — expect RowDescription ('T') + DataRow ('D') + CommandComplete ('C') + 'Z'.
    stream.write_all(&build_query("SELECT * FROM t")).await.expect("write select");
    let msgs = read_until_ready(&mut stream).await;
    let row_desc = msgs.iter().find(|(t, _)| *t == b'T');
    assert!(row_desc.is_some(), "expected RowDescription for SELECT");
    // RowDescription body: int16(num_fields) + per-field metadata.
    let row_desc_body = &row_desc.unwrap().1;
    let num_fields = u16::from_be_bytes([row_desc_body[0], row_desc_body[1]]);
    assert_eq!(num_fields, 1, "expected 1 column in RowDescription, got {num_fields}");

    let data_rows: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'D').collect();
    assert_eq!(data_rows.len(), 1, "expected 1 DataRow, got {}", data_rows.len());

    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete for SELECT");
    assert_eq!(cc, "SELECT 1", "select command tag, got {cc:?}");

    // Cleanup.
    drop(stream);
    task.abort();
}

/// Task 5.1 — an error from the engine (e.g. SELECT from a missing
/// table) returns an ErrorResponse ('E') followed by ReadyForQuery.
#[tokio::test]
async fn async_pgwire_simple_query_error_returns_error_response() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    let server = AsyncPgwireServer::bind("127.0.0.1:0", engine)
        .await
        .expect("bind");
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(&build_startup("alice")).await.expect("write startup");
    // Drain startup → ReadyForQuery.
    let _ = read_until_ready(&mut stream).await;

    // Send an obviously invalid SQL statement — the engine should
    // return a parse error. (SELECT * FROM does_not_exist actually
    // returns an empty result in turboGP, not an error, so we use a
    // syntactically invalid statement instead.)
    stream.write_all(&build_query("FOOBAR baz quux")).await.expect("write");
    let msgs = read_until_ready(&mut stream).await;
    let err = msgs.iter().find(|(t, _)| *t == b'E');
    assert!(err.is_some(), "expected an ErrorResponse, got messages: {:?}", msgs.iter().map(|(t, _)| *t as char).collect::<Vec<_>>());

    // Last message should still be ReadyForQuery (server stays alive).
    let last = msgs.last().expect("at least one message");
    assert_eq!(last.0, b'Z', "ReadyForQuery must follow ErrorResponse");

    drop(stream);
    task.abort();
}

/// Task 5.1 — a multi-statement simple query (e.g.
/// `INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)`) returns one
/// CommandComplete per statement and a single ReadyForQuery at the end.
#[tokio::test]
async fn async_pgwire_multi_statement_simple_query() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    // Pre-create the table so the multi-statement INSERT works.
    {
        let mut g = engine.write().unwrap();
        g.execute("CREATE TABLE t (id INT)").expect("create");
    }
    let server = AsyncPgwireServer::bind("127.0.0.1:0", engine)
        .await
        .expect("bind");
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(&build_startup("alice")).await.expect("startup");
    let _ = read_until_ready(&mut stream).await;

    stream
        .write_all(&build_query("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)"))
        .await
        .expect("write");
    let msgs = read_until_ready(&mut stream).await;
    let ccs: Vec<String> = msgs
        .iter()
        .filter(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .collect();
    assert_eq!(ccs, vec!["INSERT 0 1", "INSERT 0 1"], "two INSERT tags, got {ccs:?}");

    drop(stream);
    task.abort();
}
