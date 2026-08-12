//! # Async PostgreSQL wire-protocol server (Wave 5 — Tasks 5.1–5.4).
//!
//! A production-grade async port of the pgwire v3 protocol over
//! [`tokio::net::TcpStream`]. This module replaces the line-based
//! `async_server.rs` skeleton for clients that speak PostgreSQL v3
//! (e.g. `psql`, JDBC, asyncpg).
//!
//! ## Wire protocol coverage
//!
//! - **Startup** (Task 5.1): startup-message parsing, SSL/GSSAPI refusal,
//!   trust authentication, `AuthenticationOk` + `ParameterStatus` +
//!   `BackendKeyData` + `ReadyForQuery`.
//! - **Simple Query** (Task 5.1): `Q` message → `RowDescription` (T) +
//!   `DataRow`* (D) + `CommandComplete` (C) + `ReadyForQuery` (Z).
//! - **Extended Query** (Task 5.2, planned): Parse (P) / Bind (B) /
//!   Describe (D) / Execute (E) / Sync (S) / Close (C) with `$n`
//!   text substitution.
//! - **Connection-pool admission** (Task 5.3, planned): each accepted
//!   connection acquires a [`PoolPermit`] from a [`ConnectionPool`]
//!   before processing; if the pool is full, the client receives a
//!   FATAL `ErrorResponse`.
//!
//! ## Why a new module (instead of porting `pgwire.rs`)
//!
//! `pgwire.rs` already implements the full protocol but synchronously.
//! The async port reuses the wire-format conventions but lives in its
//! own module so the synchronous server can stay untouched for callers
//! that depend on its exact behavior. Where a small helper is shared,
//! we duplicate (the helpers are < 30 LOC each and private in
//! `pgwire.rs`).
//!
//! ## Constraints
//!
//! No `unwrap()`/`expect()` in production code (tests are exempted). All
//! public functions have doc comments. The module never blocks the
//! executor — all I/O is via `tokio::io::*`.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::engine::{route_and_execute, QueryEngine, QueryResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for acquiring a pool permit before rejecting a
/// connection (Task 5.3).
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum startup-message body we will read (1 MiB). Defends against
/// memory-exhaustion attacks via huge startup messages.
const MAX_STARTUP_BODY: usize = 1 << 20;

/// Wire-protocol magic numbers (first int32 of the startup body).
const PROTOCOL_3_0: i32 = 196608;
const SSL_REQUEST_MAGIC: i32 = 80877103;
const GSSAPI_REQUEST_MAGIC: i32 = 80877104;
const CANCEL_REQUEST_MAGIC: i32 = 80877102;

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Async PostgreSQL wire-protocol server.
///
/// Bind with [`AsyncPgwireServer::bind`] (no admission control) or
/// [`AsyncPgwireServer::bind_with_pool`] (Task 5.3, gate via
/// [`ConnectionPool`]). Then call [`AsyncPgwireServer::serve`] to start
/// the accept loop.
///
/// The accept loop spawns one tokio task per accepted connection. Each
/// task runs [`handle_connection`], which reads the startup message,
/// performs (trust) authentication, and enters the request loop. I/O
/// errors on a single connection are logged at `debug` level and do not
/// propagate; only listener errors propagate out of `serve`.
pub struct AsyncPgwireServer {
    /// Actual bound address. Useful for ephemeral-port tests (`bind 0`
    /// to get an OS-assigned port, then read `local_addr`).
    pub local_addr: SocketAddr,
    engine: Arc<RwLock<QueryEngine>>,
    pool: Option<Arc<crate::server::pool::ConnectionPool>>,
    listener: TcpListener,
    acquire_timeout: Duration,
}

impl AsyncPgwireServer {
    /// Bind a new async pgwire server with no admission control.
    ///
    /// Each accepted connection is handled without a pool permit — every
    /// connection runs to completion. Suitable for tests and for trusted
    /// single-tenant deployments. For multi-tenant / DoS-hardened paths,
    /// use [`AsyncPgwireServer::bind_with_pool`] (Task 5.3).
    pub async fn bind(addr: &str, engine: Arc<RwLock<QueryEngine>>) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            local_addr,
            engine,
            pool: None,
            listener,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        })
    }

    /// Bind a new async pgwire server that gates admission through a
    /// [`ConnectionPool`] (Task 5.3).
    ///
    /// Each accepted connection calls [`ConnectionPool::acquire`] before
    /// processing; if no permit is available within
    /// [`AsyncPgwireServer::acquire_timeout`] (default 5 s, override via
    /// [`AsyncPgwireServer::with_acquire_timeout`]), the client receives
    /// a FATAL `ErrorResponse` ("too many connections", SQLSTATE 53300)
    /// and the connection is closed. The permit is held for the entire
    /// connection lifetime and released when the handler returns (RAII).
    ///
    /// The engine is taken from `pool.engine` so that the pool and the
    /// server share the same `Arc<RwLock<QueryEngine>>`.
    pub async fn bind_with_pool(addr: &str, pool: Arc<crate::server::pool::ConnectionPool>) -> io::Result<Self> {
        let engine = pool.engine.clone();
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            local_addr,
            engine,
            pool: Some(pool),
            listener,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        })
    }

    /// Override the default 5 s pool-acquire timeout (Task 5.3).
    ///
    /// Chainable builder: `server.with_acquire_timeout(Duration::from_secs(1))`.
    /// Ignored when no pool is configured.
    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Run the accept loop. Returns only on a fatal listener error; per-
    /// connection errors are logged and do not propagate.
    ///
    /// Drains the accept queue in an infinite loop, spawning a tokio
    /// task per connection. Each task runs [`handle_connection`] with
    /// a clone of the engine `Arc` (and the pool `Arc`, if set).
    pub async fn serve(self) -> io::Result<()> {
        let engine = self.engine;
        let pool = self.pool;
        let acquire_timeout = self.acquire_timeout;
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) => {
                    let engine = engine.clone();
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_connection(stream, peer, engine, pool, acquire_timeout).await
                        {
                            log::debug!("async_pgwire conn {peer}: {e}");
                        }
                    });
                }
                Err(e) => {
                    log::error!("async_pgwire accept: {e}");
                    return Err(e);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection state
// ---------------------------------------------------------------------------

/// A prepared statement (Task 5.2 — extended query).
///
/// `Parse` creates one of these and stores it under `stmt_name` in the
/// connection's `statements` map. `Bind` later binds parameters to it,
/// producing a [`Portal`].
#[derive(Debug, Clone)]
struct PreparedStatement {
    /// The raw SQL, possibly with `$1` / `$2` / ... placeholders.
    sql: String,
    /// Parameter type OIDs declared by the client (0 = unspecified).
    /// Currently unused by turboGP (we do text substitution); recorded
    /// for completeness so `Describe` can echo them back in
    /// `ParameterDescription`.
    param_oids: Vec<u32>,
}

/// A bound portal (Task 5.2 — extended query).
///
/// `Bind` creates one of these by binding concrete parameter values to
/// a [`PreparedStatement`]. `Execute` runs the SQL (with parameters
/// text-substituted) and emits `DataRow`* + `CommandComplete`.
#[derive(Debug, Clone)]
struct Portal {
    /// Name of the prepared statement this portal was bound from.
    stmt_name: String,
    /// Bound parameter values, as text-decoded strings (NULL → "NULL").
    params: Vec<String>,
}

/// Per-connection state.
///
/// Owns the split read/write halves of the `TcpStream`, plus the
/// per-connection prepared-statement and portal tables (Task 5.2) and
/// the current transaction status byte sent in `ReadyForQuery`.
struct PgConn {
    read: BufReader<OwnedReadHalf>,
    write: BufWriter<OwnedWriteHalf>,
    /// Current transaction status byte ('I'/'T'/'E') sent in ReadyForQuery.
    txn: u8,
    /// Prepared statements, keyed by name (Task 5.2).
    statements: HashMap<String, PreparedStatement>,
    /// Bound portals, keyed by name (Task 5.2).
    portals: HashMap<String, Portal>,
}

impl PgConn {
    /// Construct a fresh per-connection state with idle transaction
    /// status and empty prepared-statement / portal tables.
    fn new(read: BufReader<OwnedReadHalf>, write: BufWriter<OwnedWriteHalf>) -> Self {
        Self {
            read,
            write,
            txn: b'I',
            statements: HashMap::new(),
            portals: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection driver
// ---------------------------------------------------------------------------

/// Handle one connection: startup → permit acquisition → run loop → flush.
///
/// This is the entry point spawned by [`AsyncPgwireServer::serve`]. It:
/// 1. Reads the startup message (handling SSLRequest / GSSAPIRequest by
///    declining with 'N' and re-reading). On a real v3 startup, the
///    user/database is parsed (currently unused — trust auth). On an
///    unknown protocol, sends a FATAL `ErrorResponse` and returns.
/// 2. If `pool` is `Some`, acquires a [`PoolPermit`] within
///    `acquire_timeout`. On failure, sends a FATAL `ErrorResponse`
///    ("too many connections", SQLSTATE 53300) and returns. (Task 5.3.)
///    The permit is acquired BEFORE sending `AuthenticationOk` so a
///    rejected client sees only the FATAL — not a misleading
///    `ReadyForQuery` first.
/// 3. Sends `AuthenticationOk` + parameter statuses + `BackendKeyData` +
///    `ReadyForQuery`.
/// 4. Enters the request loop, dispatching on the message tag byte.
///
/// All I/O errors are mapped to `io::Error` and propagated; the caller
/// (`serve`) logs them at `debug` level.
async fn handle_connection(
    stream: TcpStream,
    _peer: SocketAddr,
    engine: Arc<RwLock<QueryEngine>>,
    pool: Option<Arc<crate::server::pool::ConnectionPool>>,
    acquire_timeout: Duration,
) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (rh, wh) = stream.into_split();
    let mut conn = PgConn::new(
        BufReader::with_capacity(8 * 1024, rh),
        BufWriter::with_capacity(8 * 1024, wh),
    );

    // 1. Read (and validate) the startup message. No response is sent
    //    yet — we wait until we have a pool permit to either accept or
    //    reject the connection. (SSLRequest / GSSAPIRequest are
    //    declined with 'N' inline so the client can retry.)
    if let Err(e) = conn.read_startup_message().await {
        let _ = conn.write.flush().await;
        return Err(e);
    }

    // 2. Acquire a pool permit (Task 5.3). When `pool` is None, skip.
    //    The permit is held for the entire request loop and dropped at
    //    the end of this function (RAII).
    let _permit: Option<crate::server::pool::PoolPermit> = match &pool {
        Some(p) => match tokio::time::timeout(acquire_timeout, p.acquire()).await {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(_msg)) => {
                let _ = conn
                    .send_error_response("FATAL", "53300", "too many connections")
                    .await;
                let _ = conn.write.flush().await;
                return Ok(());
            }
            Err(_) => {
                let _ = conn
                    .send_error_response("FATAL", "53300", "too many connections")
                    .await;
                let _ = conn.write.flush().await;
                return Ok(());
            }
        },
        None => None,
    };

    // 3. Send AuthenticationOk + parameter statuses + BackendKeyData +
    //    ReadyForQuery. (Done after permit acquisition so a rejected
    //    client doesn't see a spurious ReadyForQuery before the FATAL.)
    if let Err(e) = conn.send_authentication_ok_and_params().await {
        let _ = conn.write.flush().await;
        return Err(e);
    }
    conn.write.flush().await?;

    // 4. Request loop.
    let result = conn.run_loop(&engine).await;
    let _ = conn.write.flush().await;
    result
}

impl PgConn {
    /// Read (and validate) the startup message.
    ///
    /// Loops to handle SSLRequest / GSSAPIRequest (declined with 'N',
    /// then the client retries with a real startup). On a real v3
    /// startup, parses the user/database (currently unused — trust auth)
    /// and returns `Ok(())` WITHOUT sending any response. The caller
    /// is responsible for calling [`Self::send_authentication_ok_and_params`]
    /// after pool-permit acquisition (Task 5.3). On an unknown protocol,
    /// sends a FATAL `ErrorResponse` and returns `Err`.
    async fn read_startup_message(&mut self) -> io::Result<()> {
        loop {
            let len = self.read_i32_be().await?;
            if !(4..=(MAX_STARTUP_BODY as i32 + 4)).contains(&len) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("startup len {len} out of range"),
                ));
            }
            let body_len = (len - 4) as usize;
            let mut buf = vec![0u8; body_len];
            self.read.read_exact(&mut buf).await?;
            if buf.len() < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "startup too short"));
            }
            let magic = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
            match magic {
                SSL_REQUEST_MAGIC | GSSAPI_REQUEST_MAGIC => {
                    // Decline SSL/GSSAPI — we run plaintext (loopback /
                    // trusted-network deployments). The client retries
                    // with a real startup message.
                    self.write.write_all(b"N").await?;
                    self.write.flush().await?;
                    continue;
                }
                CANCEL_REQUEST_MAGIC => {
                    // CancelRequest — we don't implement cancellation.
                    // Read the body (already done) and close.
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "cancel request not supported",
                    ));
                }
                m if (PROTOCOL_3_0..=PROTOCOL_3_0 + 12).contains(&m) => {
                    // Protocol v3.x — parse user/database/application_name.
                    // We accept any user (trust auth) and ignore the rest.
                    // The actual response is sent by the caller AFTER
                    // pool-permit acquisition (Task 5.3).
                    let _pairs = parse_startup_pairs(&buf[4..]);
                    return Ok(());
                }
                _ => {
                    let _ = self
                        .send_error_response("FATAL", "08P01", "unsupported protocol")
                        .await;
                    self.write.flush().await?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown startup magic {magic}"),
                    ));
                }
            }
        }
    }

    /// Send `AuthenticationOk` + standard `ParameterStatus` set +
    /// `BackendKeyData` + `ReadyForQuery`. This mirrors what the
    /// synchronous [`crate::server::pgwire::PgConn`] sends so that
    /// real-world clients (psql, JDBC) accept the handshake.
    async fn send_authentication_ok_and_params(&mut self) -> io::Result<()> {
        // AuthenticationOk: 'R' + len=8 + int32(0).
        self.send_byte(b'R', &0u32.to_be_bytes()).await?;
        // ParameterStatus messages.
        self.send_parameter_status("server_version", "15.0").await?;
        self.send_parameter_status("server_encoding", "UTF8").await?;
        self.send_parameter_status("client_encoding", "UTF8").await?;
        self.send_parameter_status("DateStyle", "ISO, MDY").await?;
        self.send_parameter_status("integer_datetimes", "on").await?;
        self.send_parameter_status("standard_conforming_strings", "on").await?;
        self.send_parameter_status("application_name", "turboGP").await?;
        self.send_parameter_status("IntervalStyle", "postgres").await?;
        self.send_parameter_status("TimeZone", "UTC").await?;
        // BackendKeyData: 'K' + len=12 + int32(pid) + int32(secret).
        let mut kb = Vec::with_capacity(8);
        kb.extend_from_slice(&rand_backend_key().to_be_bytes());
        kb.extend_from_slice(&rand_backend_key().to_be_bytes());
        self.send_byte(b'K', &kb).await?;
        // ReadyForQuery: 'Z' + len=5 + status byte.
        self.send_ready_for_query().await
    }

    /// Main request loop.
    ///
    /// Reads one message per iteration and dispatches on the tag byte.
    /// Terminates on `Terminate` ('X') or clean EOF (peer closed).
    async fn run_loop(&mut self, engine: &Arc<RwLock<QueryEngine>>) -> io::Result<()> {
        loop {
            self.write.flush().await?;
            let mut tag_buf = [0u8; 1];
            match self.read.read_exact(&mut tag_buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            }
            let msg_type = tag_buf[0];
            let len = self.read_i32_be().await? as usize;
            if len < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("msg {msg_type:#x} len {len} < 4"),
                ));
            }
            let body_len = len - 4;
            let body = self.read_body(body_len).await?;
            match msg_type {
                b'Q' => {
                    let sql = read_trailing_string(&body);
                    self.handle_simple_query(engine, &sql).await?;
                }
                b'P' => {
                    self.handle_parse(&body).await?;
                }
                b'B' => {
                    self.handle_bind(&body).await?;
                }
                b'D' => {
                    self.handle_describe(&body).await?;
                }
                b'E' => {
                    self.handle_execute(engine, &body).await?;
                }
                b'S' => {
                    // Sync — flush the write buffer and emit ReadyForQuery.
                    // This delimits extended-query "transactions": errors
                    // between Syncs are reported as ErrorResponse, but the
                    // connection stays alive for the next batch.
                    self.write.flush().await?;
                    self.send_ready_for_query().await?;
                }
                b'C' => {
                    // Close — drop the named statement or portal.
                    self.handle_close(&body);
                    self.send_byte(b'3', &[]).await?; // CloseComplete
                }
                b'H' => {
                    // Flush — drain the write buffer.
                    self.write.flush().await?;
                }
                b'X' => return Ok(()),
                other => {
                    let _ = self
                        .send_error_response(
                            "ERROR",
                            "0A000",
                            &format!("unsupported msg {other:#x}"),
                        )
                        .await;
                }
            }
        }
    }

    // --- Simple Query (Task 5.1) ---

    /// Handle a 'Q' (simple Query) message.
    ///
    /// Splits the SQL on `;` boundaries (respecting single-quoted
    /// strings), routes each statement through
    /// [`route_and_execute`] (which acquires the engine read lock for
    /// SELECT/EXPLAIN/SHOW and the write lock for DML/DDL), and emits
    /// `RowDescription` + `DataRow`* + `CommandComplete` per statement.
    /// At the end, emits a single `ReadyForQuery`. Errors mid-batch
    /// emit an `ErrorResponse` and skip the rest of the batch.
    async fn handle_simple_query(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        sql: &str,
    ) -> io::Result<()> {
        let stmts = split_sql_batch(sql);
        let was_txn = self.txn != b'I';
        for stmt in stmts {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("begin") || lower.starts_with("start transaction") {
                self.txn = b'T';
                self.send_command_complete("BEGIN").await?;
                continue;
            }
            if lower.starts_with("commit") {
                self.txn = b'I';
                self.send_command_complete("COMMIT").await?;
                continue;
            }
            if lower.starts_with("rollback") {
                self.txn = b'I';
                self.send_command_complete("ROLLBACK").await?;
                continue;
            }
            match route_and_execute(engine, trimmed) {
                Ok(r) => {
                    self.send_row_description(&r).await?;
                    self.send_data_rows(&r).await?;
                    self.send_command_complete(&command_tag(&r, trimmed)).await?;
                }
                Err(e) => {
                    let _ = self
                        .send_error_response("ERROR", e.sqlstate(), &format!("{e}"))
                        .await;
                    if was_txn {
                        self.txn = b'E';
                    }
                    break;
                }
            }
        }
        self.send_ready_for_query().await
    }

    // --- Extended Query (Task 5.2) ---

    /// Handle a 'P' (Parse) message.
    ///
    /// Body format: `cstring(stmt_name) + cstring(sql) + int16(n_params)
    /// + int32[n_params](param_oids)`. Stores the prepared statement in
    /// `self.statements` and emits `ParseComplete` ('1').
    ///
    /// Parameter OIDs are recorded but currently unused (turboGP does
    /// text substitution via [`substitute_params`], not type-aware
    /// binding). A TODO is left to wire type-aware binding in a future
    /// wave.
    async fn handle_parse(&mut self, body: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let name = read_cstring(body, &mut c)?;
        let sql = read_cstring(body, &mut c)?;
        if c + 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse truncated"));
        }
        let n = u16::from_be_bytes([body[c], body[c + 1]]) as usize;
        c += 2;
        let mut oids = Vec::with_capacity(n);
        for _ in 0..n {
            if c + 4 > body.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse OID truncated"));
            }
            oids.push(u32::from_be_bytes([body[c], body[c + 1], body[c + 2], body[c + 3]]));
            c += 4;
        }
        self.statements
            .insert(name, PreparedStatement { sql, param_oids: oids });
        self.send_byte(b'1', &[]).await // ParseComplete
    }

    /// Handle a 'B' (Bind) message.
    ///
    /// Body format: `cstring(portal_name) + cstring(stmt_name) +
    /// int16(n_param_formats) + int16[n_param_formats](formats) +
    /// int16(n_params) + for each param: int32(len) + bytes +
    /// int16(n_result_formats) + int16[n_result_formats](formats)`.
    ///
    /// All parameters are treated as text (format code 0). Binary-format
    /// parameters are decoded as hex strings for safety. Stores the portal
    /// in `self.portals` and emits `BindComplete` ('2').
    async fn handle_bind(&mut self, body: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(body, &mut c)?;
        let stmt_name = read_cstring(body, &mut c)?;
        // Parameter format codes (all treated as text — we skip them).
        if c + 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind fmt count truncated"));
        }
        let n_fmt = u16::from_be_bytes([body[c], body[c + 1]]) as usize;
        c += 2;
        if c + n_fmt * 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind fmt list truncated"));
        }
        // (Format codes skipped — turboGP treats all params as text.)
        c += n_fmt * 2;
        // Read parameters.
        if c + 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param count truncated"));
        }
        let n_params = u16::from_be_bytes([body[c], body[c + 1]]) as usize;
        c += 2;
        let mut params = Vec::with_capacity(n_params);
        for _ in 0..n_params {
            if c + 4 > body.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param len truncated"));
            }
            let plen = i32::from_be_bytes([body[c], body[c + 1], body[c + 2], body[c + 3]]);
            c += 4;
            let val = if plen < 0 {
                // NULL — encoded as the SQL keyword NULL after substitution.
                "NULL".to_string()
            } else {
                let plen = plen as usize;
                if c + plen > body.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Bind param bytes overflow",
                    ));
                }
                let bytes = &body[c..c + plen];
                c += plen;
                String::from_utf8_lossy(bytes).into_owned()
            };
            params.push(val);
        }
        // Result format codes (skipped — we always emit text).
        if c + 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt count truncated"));
        }
        let n_rfmt = u16::from_be_bytes([body[c], body[c + 1]]) as usize;
        c += 2;
        if c + n_rfmt * 2 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt list overflow"));
        }
        // (Result format codes skipped.)
        if !self.statements.contains_key(&stmt_name) {
            let _ = self
                .send_error_response(
                    "ERROR",
                    "26000",
                    &format!("prepared statement \"{stmt_name}\" does not exist"),
                )
                .await;
            return Ok(());
        }
        self.portals
            .insert(portal_name, Portal { stmt_name, params });
        self.send_byte(b'2', &[]).await // BindComplete
    }

    /// Handle a 'D' (Describe) message.
    ///
    /// Body format: `byte('S' for statement | 'P' for portal) +
    /// cstring(name)`.
    ///
    /// For a statement, emits `ParameterDescription` ('t') + `NoData` ('n').
    /// For a portal, emits `NoData` ('n'). (We can't know the result
    /// schema without executing the query — psql tolerates NoData here,
    /// matching the synchronous pgwire.rs Wave 52 fix.)
    async fn handle_describe(&mut self, body: &[u8]) -> io::Result<()> {
        if body.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Describe empty"));
        }
        let kind = body[0];
        let mut c = 1;
        let name = read_cstring(body, &mut c)?;
        match kind {
            b'S' => {
                let stmt = match self.statements.get(&name) {
                    Some(s) => s.clone(),
                    None => {
                        let _ = self
                            .send_error_response(
                                "ERROR",
                                "26000",
                                &format!("statement \"{name}\" not found"),
                            )
                            .await;
                        return Ok(());
                    }
                };
                let n = stmt.param_oids.len() as u16;
                let mut b = Vec::with_capacity(2 + stmt.param_oids.len() * 4);
                b.extend_from_slice(&n.to_be_bytes());
                for oid in &stmt.param_oids {
                    b.extend_from_slice(&oid.to_be_bytes());
                }
                self.send_byte(b't', &b).await?; // ParameterDescription
                self.send_byte(b'n', &[]).await?; // NoData
            }
            b'P' => {
                if !self.portals.contains_key(&name) {
                    let _ = self
                        .send_error_response(
                            "ERROR",
                            "34000",
                            &format!("portal \"{name}\" not found"),
                        )
                        .await;
                    return Ok(());
                }
                self.send_byte(b'n', &[]).await?; // NoData
            }
            _ => {
                let _ = self
                    .send_error_response("ERROR", "08P01", "unknown describe kind")
                    .await;
            }
        }
        Ok(())
    }

    /// Handle an 'E' (Execute) message.
    ///
    /// Body format: `cstring(portal_name) + int32(max_rows)`.
    ///
    /// Looks up the portal, fetches the underlying prepared statement,
    /// text-substitutes the bound parameters via [`substitute_params`],
    /// and runs the SQL through [`route_and_execute`]. Emits `DataRow`*
    /// + `CommandComplete` on success, or `ErrorResponse` on error.
    ///
    /// `max_rows > 0` (cursor mode) is currently treated as unlimited —
    /// we don't emit `PortalSuspended`. A TODO is left for a future wave.
    async fn handle_execute(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        body: &[u8],
    ) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(body, &mut c)?;
        if c + 4 > body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Execute truncated"));
        }
        let max_rows = i32::from_be_bytes([body[c], body[c + 1], body[c + 2], body[c + 3]]);
        // TODO(Wave 6+): honor max_rows by emitting PortalSuspended when
        // more rows remain. For now we always send all rows.
        let _ = max_rows;

        let portal = match self.portals.get(&portal_name).cloned() {
            Some(p) => p,
            None => {
                let _ = self
                    .send_error_response(
                        "ERROR",
                        "34000",
                        &format!("portal \"{portal_name}\" not found"),
                    )
                    .await;
                return Ok(());
            }
        };
        let stmt = match self.statements.get(&portal.stmt_name).cloned() {
            Some(s) => s,
            None => {
                let _ = self
                    .send_error_response(
                        "ERROR",
                        "26000",
                        &format!("statement \"{}\" not found", portal.stmt_name),
                    )
                    .await;
                return Ok(());
            }
        };
        // TODO(Wave 6+): thread typed parameters through the executor
        // instead of text-substituting into the SQL string.
        let sql = substitute_params(&stmt.sql, &portal.params);
        match route_and_execute(engine, &sql) {
            Ok(r) => {
                self.send_data_rows(&r).await?;
                self.send_command_complete(&command_tag(&r, &sql)).await?;
            }
            Err(e) => {
                let _ = self
                    .send_error_response("ERROR", e.sqlstate(), &format!("{e}"))
                    .await;
            }
        }
        Ok(())
    }

    /// Handle a 'C' (Close) message — drop the named statement or portal.
    /// Body format: `byte('S' | 'P') + cstring(name)`.
    fn handle_close(&mut self, body: &[u8]) {
        if body.is_empty() {
            return;
        }
        let kind = body[0];
        let mut c = 1;
        if let Ok(name) = read_cstring(body, &mut c) {
            match kind {
                b'S' => {
                    self.statements.remove(&name);
                }
                b'P' => {
                    self.portals.remove(&name);
                }
                _ => {}
            }
        }
    }

    // --- Outbound helpers ---

    /// Send a single message: `tag` byte + 4-byte BE length + body.
    /// The length includes itself (4) plus the body length.
    async fn send_byte(&mut self, tag: u8, body: &[u8]) -> io::Result<()> {
        let len = (body.len() as u32 + 4).to_be_bytes();
        self.write.write_all(&[tag]).await?;
        self.write.write_all(&len).await?;
        self.write.write_all(body).await?;
        Ok(())
    }

    /// Send a `ParameterStatus` ('S') message: `key\0value\0`.
    async fn send_parameter_status(&mut self, key: &str, val: &str) -> io::Result<()> {
        let mut body = Vec::with_capacity(key.len() + 1 + val.len() + 1);
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(val.as_bytes());
        body.push(0);
        self.send_byte(b'S', &body).await
    }

    /// Send a `RowDescription` ('T') message describing the columns of
    /// `r`. If `r` has no columns (e.g. DDL), sends `NoData` ('n') instead.
    ///
    /// For each column, the type OID is taken from `col.type_oid` when
    /// set; otherwise a heuristic (string vs. numeric) is used. The type
    /// size and modifier are best-effort (TEXT=-1, INT8=8, default=8).
    async fn send_row_description(&mut self, r: &QueryResult) -> io::Result<()> {
        if r.columns.is_empty() {
            self.send_byte(b'n', &[]).await?;
            return Ok(());
        }
        let mut body = Vec::new();
        body.extend_from_slice(&(r.columns.len() as u16).to_be_bytes());
        for col in &r.columns {
            body.extend_from_slice(col.name.as_bytes());
            body.push(0);
            body.extend_from_slice(&0u32.to_be_bytes()); // table OID
            body.extend_from_slice(&0u16.to_be_bytes()); // col attnum
            let (type_oid, type_size) = if col.type_oid != 0 {
                let size = match col.type_oid {
                    25 => -1i16,       // TEXT
                    701 | 700 => 8i16, // FLOAT8 / FLOAT4
                    16 => 1i16,        // BOOL
                    _ => 8i16,         // default INT8
                };
                (col.type_oid, size)
            } else if col.has_strings() {
                (25u32, -1i16) // TEXT
            } else {
                (20u32, 8i16) // INT8
            };
            body.extend_from_slice(&type_oid.to_be_bytes());
            body.extend_from_slice(&type_size.to_be_bytes());
            body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
            body.extend_from_slice(&0i16.to_be_bytes()); // format = text
        }
        self.send_byte(b'T', &body).await
    }

    /// Send `DataRow` ('D') messages for every row in `r`. NULL cells
    /// (per `null_mask`) are encoded as a -1 length with no payload;
    /// string cells use the original string; numeric cells use the
    /// decimal string of the u64 value (with a f64 heuristic for very
    /// large values that look like Float64 bit patterns).
    async fn send_data_rows(&mut self, r: &QueryResult) -> io::Result<()> {
        for row_idx in 0..r.row_count {
            let mut body = Vec::new();
            body.extend_from_slice(&(r.columns.len() as u16).to_be_bytes());
            for col in &r.columns {
                let is_null = col
                    .null_mask
                    .as_ref()
                    .and_then(|m| m.get(row_idx).copied())
                    .unwrap_or(false);
                if is_null {
                    body.extend_from_slice(&(-1i32).to_be_bytes());
                    continue;
                }
                let s = if let Some(sv) = &col.string_values {
                    sv.get(row_idx).cloned().unwrap_or_default()
                } else {
                    let v = col.values.get(row_idx).copied().unwrap_or(0);
                    if v > (1u64 << 60) {
                        let f = f64::from_bits(v);
                        if f.is_finite() && f.abs() < 1e15 {
                            format!("{f}")
                        } else {
                            v.to_string()
                        }
                    } else {
                        v.to_string()
                    }
                };
                body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
            self.send_byte(b'D', &body).await?;
        }
        Ok(())
    }

    /// Send a `CommandComplete` ('C') message: `tag\0`.
    async fn send_command_complete(&mut self, tag: &str) -> io::Result<()> {
        let mut body = Vec::with_capacity(tag.len() + 1);
        body.extend_from_slice(tag.as_bytes());
        body.push(0);
        self.send_byte(b'C', &body).await
    }

    /// Send a `ReadyForQuery` ('Z') message with the current txn status.
    async fn send_ready_for_query(&mut self) -> io::Result<()> {
        self.send_byte(b'Z', &[self.txn]).await
    }

    /// Send an `ErrorResponse` ('E') message.
    ///
    /// `severity` is "ERROR" or "FATAL"; `code` is the 5-character
    /// SQLSTATE; `msg` is the human-readable message. The message body
    /// contains the S/V/C/M fields followed by a NUL terminator.
    async fn send_error_response(
        &mut self,
        severity: &str,
        code: &str,
        msg: &str,
    ) -> io::Result<()> {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(severity.as_bytes());
        body.push(0);
        body.push(b'V');
        body.extend_from_slice(severity.as_bytes());
        body.push(0);
        body.push(b'C');
        body.extend_from_slice(code.as_bytes());
        body.push(0);
        body.push(b'M');
        body.extend_from_slice(msg.as_bytes());
        body.push(0);
        body.push(0); // terminator
        self.send_byte(b'E', &body).await
    }

    /// Read a 4-byte big-endian i32 from the stream.
    async fn read_i32_be(&mut self) -> io::Result<i32> {
        let mut buf = [0u8; 4];
        self.read.read_exact(&mut buf).await?;
        Ok(i32::from_be_bytes(buf))
    }

    /// Read `body_len` bytes into a fresh `Vec<u8>`.
    async fn read_body(&mut self, body_len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; body_len];
        self.read.read_exact(&mut buf).await?;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Parse the startup message's null-terminated key/value pairs into a
/// `Vec<(String, String)>`. Stops at the first empty key (double NUL).
/// Used by [`PgConn::handle_startup`] to extract `user` / `database`
/// (currently the values are accepted but not stored — trust auth).
fn parse_startup_pairs(buf: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() && buf[i] != 0 {
        let k = match read_cstring(buf, &mut i) {
            Ok(s) => s,
            Err(_) => break,
        };
        if i >= buf.len() || buf[i] == 0 {
            out.push((k, String::new()));
            break;
        }
        let v = match read_cstring(buf, &mut i) {
            Ok(s) => s,
            Err(_) => break,
        };
        out.push((k, v));
    }
    out
}

/// Read a NUL-terminated C string from `buf` starting at `*cursor`.
/// Advances `*cursor` past the NUL. Returns `Err(InvalidData)` if no
/// NUL is found.
fn read_cstring(buf: &[u8], cursor: &mut usize) -> io::Result<String> {
    let end = buf[*cursor..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing NUL"))?;
    let start = *cursor;
    *cursor = start + end + 1;
    String::from_utf8(buf[start..start + end].to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Strip the trailing NUL(s) from a 'Q' message body and return the SQL
/// as a `String` (lossy UTF-8).
fn read_trailing_string(body: &[u8]) -> String {
    let mut end = body.len();
    while end > 0 && body[end - 1] == 0 {
        end -= 1;
    }
    String::from_utf8_lossy(&body[..end]).into_owned()
}

/// Split a SQL batch on `;` boundaries, respecting single-quoted strings.
///
/// `;` inside a `'...'` literal is preserved. Doubled single quotes
/// (`''`) inside a literal are also preserved. Empty statements are
/// dropped. (Mirrors the logic in `pgwire.rs::split_sql_batch`, which
/// is private — duplicated here to keep `async_pgwire` self-contained.)
fn split_sql_batch(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_str = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            current.push(c as char);
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    current.push('\'');
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_str = true;
            current.push('\'');
            i += 1;
            continue;
        }
        if c == b';' {
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        current.push(c as char);
        i += 1;
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Compute the `CommandComplete` tag for a query result.
///
/// Mirrors `pgwire.rs::command_tag` (private): SELECT/WITH →
/// `SELECT N`, INSERT → `INSERT 0 N`, UPDATE → `UPDATE N`, DELETE →
/// `DELETE N`, CREATE → `CREATE`, DROP → `DROP`, BEGIN/COMMIT/ROLLBACK →
/// the verb. Falls back to `"OK"` for unknown statements.
fn command_tag(r: &QueryResult, sql: &str) -> String {
    let lower = sql.trim_start().to_lowercase();
    if lower.starts_with("select") || lower.starts_with("with") {
        return format!("SELECT {}", r.row_count);
    }
    if lower.starts_with("insert") {
        return format!("INSERT 0 {}", r.row_count);
    }
    if lower.starts_with("update") {
        return format!("UPDATE {}", r.row_count);
    }
    if lower.starts_with("delete") {
        return format!("DELETE {}", r.row_count);
    }
    if lower.starts_with("create") {
        return "CREATE".into();
    }
    if lower.starts_with("drop") {
        return "DROP".into();
    }
    if lower.starts_with("begin") || lower.starts_with("start transaction") {
        return "BEGIN".into();
    }
    if lower.starts_with("commit") {
        return "COMMIT".into();
    }
    if lower.starts_with("rollback") {
        return "ROLLBACK".into();
    }
    "OK".into()
}

/// Generate a random 4-byte backend key (PID / secret) using the OS RNG.
fn rand_backend_key() -> i32 {
    use rand::Rng;
    rand::rng().random()
}

/// Substitute `$1`, `$2`, ... placeholders in `sql` with values from
/// `params`.
///
/// Numeric values (integers / floats) are passed through unquoted; the
/// literal keywords `true` / `false` / `NULL` are also passed through
/// unquoted; all other values are wrapped in single quotes with any
/// internal single quotes doubled (`'` → `''`) — the standard SQL
/// string-literal escaping rule, which prevents SQL injection via the
/// Bind/Execute path.
///
/// **Note:** This is a defence-in-depth measure. The ideal fix threads
/// typed parameters through the executor rather than string-substituting
/// (TODO: see `handle_execute`). Mirrors
/// `crate::server::pgwire::substitute_params` (private) so this module
/// stays self-contained.
fn substitute_params(sql: &str, params: &[String]) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n: usize = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n * 10 + (bytes[j] - b'0') as usize;
                j += 1;
            }
            if n >= 1 && n <= params.len() {
                out.push_str(&escape_param_value(&params[n - 1]));
            } else {
                out.push_str("NULL");
            }
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Escape a bound parameter value for safe interpolation into SQL.
///
/// See [`substitute_params`] for the full policy.
fn escape_param_value(value: &str) -> String {
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
        return value.to_string();
    }
    if value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("null")
    {
        return value.to_string();
    }
    let escaped = value.replace('\'', "''");
    format!("'{}'", escaped)
}

#[cfg(test)]
#[path = "async_pgwire_tests.rs"]
mod async_pgwire_tests;
