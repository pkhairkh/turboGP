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
use crate::server::pool::{ConnectionPool, PoolConfig};

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

/// Build a 'P' (Parse) message: 'P' + int32(length) + cstring(stmt_name)
/// + cstring(sql) + int16(0) (no param OIDs).
fn build_parse(stmt_name: &str, sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(stmt_name.as_bytes());
    body.push(0);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes()); // 0 param types
    let len = body.len() as u32 + 4;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(b'P');
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Build a 'B' (Bind) message: 'B' + int32(length) + cstring(portal_name)
/// + cstring(stmt_name) + int16(0) param fmts + int16(n_params) +
/// for each param: int32(len) + bytes + int16(0) result fmts.
fn build_bind(portal_name: &str, stmt_name: &str, params: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal_name.as_bytes());
    body.push(0);
    body.extend_from_slice(stmt_name.as_bytes());
    body.push(0);
    // 0 parameter format codes (use text).
    body.extend_from_slice(&0u16.to_be_bytes());
    // n_params + each param's length-prefixed bytes.
    body.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        body.extend_from_slice(p.as_bytes());
    }
    // 0 result format codes.
    body.extend_from_slice(&0u16.to_be_bytes());
    let len = body.len() as u32 + 4;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(b'B');
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Build an 'E' (Execute) message: 'E' + int32(length) + cstring(portal_name)
/// + int32(0) (max_rows = 0 = unlimited).
fn build_execute(portal_name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal_name.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i32.to_be_bytes()); // max_rows = 0
    let len = body.len() as u32 + 4;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(b'E');
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Build an 'S' (Sync) message: 'S' + int32(4).
fn build_sync() -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(b'S');
    msg.extend_from_slice(&4u32.to_be_bytes());
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

// ---------------------------------------------------------------------------
// Task 5.2: extended query protocol (Parse / Bind / Execute)
// ---------------------------------------------------------------------------

/// Helper: send a batch of byte buffers (Parse, Bind, Execute, Sync)
/// back-to-back over a single connection. Used by the extended-query
/// tests to mirror what psql / JDBC do (pipeline the Parse+Bind+Execute+
/// Sync without waiting for individual responses).
async fn write_all_batch(stream: &mut TcpStream, bufs: &[&[u8]]) {
    for b in bufs {
        stream.write_all(b).await.expect("write batch");
    }
}

/// Task 5.2 DoD — Parse an INSERT with a `$1` placeholder, Bind a
/// concrete value, Execute, verify `CommandComplete`. Then Parse a
/// SELECT, Bind, Execute, verify rows.
#[tokio::test]
async fn async_pgwire_extended_query_parse_bind_execute() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    // Pre-create the table for the INSERT.
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
    // Drain startup → ReadyForQuery.
    let _ = read_until_ready(&mut stream).await;

    // --- Phase 1: Parse + Bind + Execute an INSERT with $1. ---
    let parse = build_parse("ins_stmt", "INSERT INTO t VALUES ($1)");
    let bind = build_bind("ins_portal", "ins_stmt", &["42"]);
    let exec = build_execute("ins_portal");
    let sync = build_sync();
    write_all_batch(&mut stream, &[&parse, &bind, &exec, &sync]).await;

    // Expect: ParseComplete ('1') + BindComplete ('2') + DataRow* +
    // CommandComplete ('C' = "INSERT 0 1") + ReadyForQuery ('Z'). The
    // engine may emit 0 or more DataRows depending on whether INSERT
    // returns the inserted row; we assert on the structural messages.
    let msgs = read_until_ready(&mut stream).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags[0], b'1', "first message should be ParseComplete, got {:?}", tags);
    assert_eq!(tags[1], b'2', "second message should be BindComplete, got {:?}", tags);
    assert!(tags.contains(&b'C'), "expected a CommandComplete, got {:?}", tags);
    assert_eq!(*tags.last().unwrap(), b'Z', "last message should be ReadyForQuery");
    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete");
    assert_eq!(cc, "INSERT 0 1", "insert command tag, got {cc:?}");

    // --- Phase 2: Parse + Bind + Execute a SELECT, verify rows. ---
    let parse2 = build_parse("sel_stmt", "SELECT * FROM t");
    let bind2 = build_bind("sel_portal", "sel_stmt", &[]);
    let exec2 = build_execute("sel_portal");
    let sync2 = build_sync();
    write_all_batch(&mut stream, &[&parse2, &bind2, &exec2, &sync2]).await;

    let msgs = read_until_ready(&mut stream).await;
    // ParseComplete + BindComplete + DataRow + CommandComplete + ReadyForQuery.
    let data_rows: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'D').collect();
    assert_eq!(data_rows.len(), 1, "expected 1 DataRow, got {}", data_rows.len());

    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete for SELECT");
    assert_eq!(cc, "SELECT 1", "select command tag, got {cc:?}");

    drop(stream);
    task.abort();
}

/// Task 5.2 — Describe on a parsed statement emits
/// `ParameterDescription` ('t') + `NoData` ('n').
#[tokio::test]
async fn async_pgwire_extended_query_describe_statement() {
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
    stream.write_all(&build_startup("alice")).await.expect("startup");
    let _ = read_until_ready(&mut stream).await;

    // Parse "sel" then Describe statement 'S' "sel".
    let parse = build_parse("sel", "SELECT 1");
    let mut desc_body = Vec::new();
    desc_body.push(b'S'); // describe a statement
    desc_body.extend_from_slice(b"sel\0");
    let desc_len = desc_body.len() as u32 + 4;
    let mut desc = Vec::new();
    desc.push(b'D');
    desc.extend_from_slice(&desc_len.to_be_bytes());
    desc.extend_from_slice(&desc_body);
    let sync = build_sync();
    write_all_batch(&mut stream, &[&parse, &desc, &sync]).await;

    let msgs = read_until_ready(&mut stream).await;
    // ParseComplete + ParameterDescription + NoData + ReadyForQuery.
    let has_param_desc = msgs.iter().any(|(t, _)| *t == b't');
    assert!(has_param_desc, "expected ParameterDescription ('t')");
    let has_nodata = msgs.iter().any(|(t, _)| *t == b'n');
    assert!(has_nodata, "expected NoData ('n')");

    drop(stream);
    task.abort();
}

/// Task 5.2 — Close ('C') on a parsed statement drops it; subsequent
/// Describe returns an ErrorResponse.
#[tokio::test]
async fn async_pgwire_extended_query_close_drops_statement() {
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
    stream.write_all(&build_startup("alice")).await.expect("startup");
    let _ = read_until_ready(&mut stream).await;

    // Parse + Close + Describe + Sync.
    let parse = build_parse("foo", "SELECT 1");
    // Close: 'C' + len + 'S' + cstring(name).
    let mut close_body = Vec::new();
    close_body.push(b'S');
    close_body.extend_from_slice(b"foo\0");
    let close_len = close_body.len() as u32 + 4;
    let mut close = Vec::new();
    close.push(b'C');
    close.extend_from_slice(&close_len.to_be_bytes());
    close.extend_from_slice(&close_body);
    // Describe.
    let mut desc_body = Vec::new();
    desc_body.push(b'S');
    desc_body.extend_from_slice(b"foo\0");
    let desc_len = desc_body.len() as u32 + 4;
    let mut desc = Vec::new();
    desc.push(b'D');
    desc.extend_from_slice(&desc_len.to_be_bytes());
    desc.extend_from_slice(&desc_body);
    let sync = build_sync();
    write_all_batch(&mut stream, &[&parse, &close, &desc, &sync]).await;

    let msgs = read_until_ready(&mut stream).await;
    // Expect: ParseComplete + CloseComplete + ErrorResponse (statement
    // not found) + ReadyForQuery.
    let has_err = msgs.iter().any(|(t, _)| *t == b'E');
    assert!(has_err, "expected an ErrorResponse after Close + Describe of closed statement");

    drop(stream);
    task.abort();
}

// ---------------------------------------------------------------------------
// Task 5.3: connection-pool admission control
// ---------------------------------------------------------------------------

/// Helper: connect, send startup, and drain AuthOk + ParameterStatus* +
/// BackendKeyData + ReadyForQuery. Returns the connected stream (with
/// the startup handshake already completed). Used by pool tests to
/// establish "ready" connections that hold their pool permits.
async fn connect_and_startup(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(&build_startup("u")).await.expect("startup write");
    let _ = read_until_ready(&mut s).await;
    s
}

/// Task 5.3 DoD — start a server with pool size 2, open 2 connections
/// (which hold their permits by not sending any queries), then open 2
/// more and verify they receive FATAL ErrorResponse ("too many
/// connections") after the acquire timeout elapses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_pgwire_pool_limits_concurrency() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    let pool = Arc::new(ConnectionPool::new(
        engine,
        PoolConfig { max_size: 2, acquire_timeout_secs: 30 },
    ));
    let server = AsyncPgwireServer::bind_with_pool("127.0.0.1:0", pool.clone())
        .await
        .expect("bind")
        .with_acquire_timeout(Duration::from_millis(200));
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 1. Open 2 connections — they acquire permits and sit idle (holding
    //    the permits) by not sending any query.
    let mut c1 = connect_and_startup(addr).await;
    let mut c2 = connect_and_startup(addr).await;

    // Sanity: the pool reports 2 active / 0 idle.
    let m = pool.metrics();
    assert_eq!(m.active, 2, "pool should have 2 active permits, got {m:?}");
    assert_eq!(m.idle, 0, "pool should have 0 idle permits, got {m:?}");

    // 2. Open connections 3 and 4 — they should time out (200ms) and
    //    receive a FATAL ErrorResponse with "too many connections".
    let mut c3 = TcpStream::connect(addr).await.expect("c3 connect");
    c3.write_all(&build_startup("u")).await.expect("c3 startup");
    let (tag, body) = tokio::time::timeout(Duration::from_secs(2), read_message(&mut c3))
        .await
        .expect("c3 read timed out > 2s (acquire should fail at 200ms)");
    assert_eq!(tag, b'E', "c3 should receive ErrorResponse ('E'), got {tag:#x}");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("too many connections"),
        "c3 body should contain 'too many connections', got: {body_str:?}"
    );

    let mut c4 = TcpStream::connect(addr).await.expect("c4 connect");
    c4.write_all(&build_startup("u")).await.expect("c4 startup");
    let (tag, body) = tokio::time::timeout(Duration::from_secs(2), read_message(&mut c4))
        .await
        .expect("c4 read timed out > 2s (acquire should fail at 200ms)");
    assert_eq!(tag, b'E', "c4 should receive ErrorResponse ('E'), got {tag:#x}");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("too many connections"),
        "c4 body should contain 'too many connections', got: {body_str:?}"
    );

    // 3. Verify the pool counters reflect the rejected acquires.
    let m = pool.metrics();
    assert_eq!(m.active, 2, "active should still be 2 (c1/c2 hold permits), got {m:?}");
    // total_acquired should be exactly 2 (c1, c2) — c3 and c4 timed out
    // before acquire() returned Ok.
    assert_eq!(m.total_acquired, 2, "total_acquired should be 2, got {m:?}");

    drop(c1);
    drop(c2);
    drop(c3);
    drop(c4);
    task.abort();
}

/// Task 5.3 — when a permit is released (a connection closes), a
/// waiting connection can acquire it and proceed. This proves the pool
/// isn't permanently stuck after rejections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_pgwire_pool_releases_permit_on_disconnect() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    let pool = Arc::new(ConnectionPool::new(
        engine,
        PoolConfig { max_size: 1, acquire_timeout_secs: 30 },
    ));
    let server = AsyncPgwireServer::bind_with_pool("127.0.0.1:0", pool.clone())
        .await
        .expect("bind")
        .with_acquire_timeout(Duration::from_millis(200));
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Open 1 connection — fills the pool (max_size = 1).
    let c1 = connect_and_startup(addr).await;
    assert_eq!(pool.metrics().active, 1);

    // Drop c1 — releases the permit.
    drop(c1);
    // Give the server a moment to drop the permit.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(pool.metrics().active, 0, "permit should be released after c1 drops");

    // Now a new connection should succeed.
    let _c2 = connect_and_startup(addr).await;
    assert_eq!(pool.metrics().active, 1, "new connection should acquire the freed permit");

    task.abort();
}

// ---------------------------------------------------------------------------
// Task 5.4: end-to-end integration test
// ---------------------------------------------------------------------------

/// Helper: send a simple Query ('Q') and return the messages up to and
/// including ReadyForQuery.
async fn simple_query(stream: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    stream.write_all(&build_query(sql)).await.expect("write query");
    read_until_ready(stream).await
}

/// Helper: extract the integer value of the first column of a DataRow.
/// Body format: int16(num_cols) + int32(col_len) + col_bytes. Returns
/// the parsed i64 (panics on malformed input).
fn first_col_as_i64(body: &[u8]) -> i64 {
    assert!(body.len() >= 6, "DataRow too short: {body:?}");
    let _num_cols = u16::from_be_bytes([body[0], body[1]]);
    let col_len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
    let col_bytes = &body[6..6 + col_len];
    let s = std::str::from_utf8(col_bytes).expect("col utf8");
    s.parse::<i64>().unwrap_or_else(|_| panic!("col not an i64: {s:?}"))
}

/// Task 5.4 DoD — comprehensive end-to-end test: start an async pgwire
/// server, connect with a raw TCP client, execute CREATE TABLE t (id
/// INTEGER), INSERT 3 rows, SELECT all 3 back, verify row count and
/// column count. Then run 3 concurrent SELECTs and verify all return
/// the correct results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_pgwire_end_to_end_integration() {
    let engine = Arc::new(RwLock::new(QueryEngine::in_memory()));
    let server = AsyncPgwireServer::bind("127.0.0.1:0", engine)
        .await
        .expect("bind");
    let addr = server.local_addr;
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ---- Phase 1: single connection CREATE + INSERT*3 + SELECT. ----
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(&build_startup("alice")).await.expect("startup");
    let _ = read_until_ready(&mut s).await;

    // CREATE TABLE t (id INTEGER)
    let msgs = simple_query(&mut s, "CREATE TABLE t (id INTEGER)").await;
    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete for CREATE");
    assert_eq!(cc, "CREATE", "create command tag, got {cc:?}");

    // INSERT 3 rows.
    for i in 1..=3i64 {
        let msgs = simple_query(&mut s, &format!("INSERT INTO t VALUES ({i})")).await;
        let cc = msgs
            .iter()
            .find(|(t, _)| *t == b'C')
            .map(|(_, b)| cc_tag(b))
            .expect("expected CommandComplete for INSERT");
        assert_eq!(cc, "INSERT 0 1", "insert {i} command tag, got {cc:?}");
    }

    // SELECT * FROM t — expect RowDescription (1 col) + 3 DataRows +
    // CommandComplete ("SELECT 3") + ReadyForQuery.
    let msgs = simple_query(&mut s, "SELECT * FROM t").await;
    let row_desc = msgs
        .iter()
        .find(|(t, _)| *t == b'T')
        .expect("expected RowDescription for SELECT");
    let num_fields = u16::from_be_bytes([row_desc.1[0], row_desc.1[1]]);
    assert_eq!(num_fields, 1, "expected 1 column, got {num_fields}");

    let data_rows: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'D').collect();
    assert_eq!(data_rows.len(), 3, "expected 3 DataRows, got {}", data_rows.len());

    // Verify the actual row values are 1, 2, 3 (in some order — the
    // engine doesn't guarantee order without ORDER BY, but for an
    // INSERT-then-SELECT it typically preserves insertion order).
    let mut values: Vec<i64> = data_rows
        .iter()
        .map(|(_, b)| first_col_as_i64(b))
        .collect();
    values.sort();
    assert_eq!(values, vec![1, 2, 3], "row values should be 1, 2, 3, got {values:?}");

    let cc = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| cc_tag(b))
        .expect("expected CommandComplete for SELECT");
    assert_eq!(cc, "SELECT 3", "select command tag, got {cc:?}");

    drop(s);

    // ---- Phase 2: 3 concurrent SELECTs, each returns 3 rows. ----
    let mut handles = Vec::new();
    for i in 0..3 {
        let addr_clone = addr;
        handles.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr_clone).await.expect("connect");
            s.write_all(&build_startup(&format!("concurrent_{i}"))).await.expect("startup");
            let _ = read_until_ready(&mut s).await;
            let msgs = simple_query(&mut s, "SELECT * FROM t").await;
            let n_data = msgs.iter().filter(|(t, _)| *t == b'D').count();
            let cc = msgs
                .iter()
                .find(|(t, _)| *t == b'C')
                .map(|(_, b)| cc_tag(b))
                .unwrap_or_default();
            (i, n_data, cc)
        }));
    }

    for h in handles {
        let (i, n_data, cc) = h.await.expect("join");
        assert_eq!(n_data, 3, "concurrent client {i} should see 3 rows, got {n_data}");
        assert_eq!(cc, "SELECT 3", "concurrent client {i} command tag, got {cc:?}");
    }

    task.abort();
}
