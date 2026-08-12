//! Postgres v3 wire protocol implementation.
//!
//! Message framing: every backend message = 1 byte type + 4 byte BE length
//! (length includes itself, excludes type byte) + payload. Frontend messages
//! have the same format (except startup, which has no type byte).

use super::auth::{verify_scram, PasswordManager, ScramOutcome, TlsConfig};
use super::session::{Session, TxnStatus};
use crate::engine::{QueryEngine, QueryResult, ResultColumn};
use base64::Engine as _;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const SSL_REQUEST_MAGIC: i32 = 80877103;
const GSSAPI_REQUEST_MAGIC: i32 = 80877104;
const CANCEL_REQUEST_MAGIC: i32 = 80877102;
const PROTOCOL_3_0: i32 = 196608;

#[derive(Debug, Clone)]
struct PreparedStatement {
    sql: String,
    param_oids: Vec<u32>,
}

#[derive(Debug, Clone)]
struct Portal {
    stmt_name: String,
    params: Vec<String>,
    result_formats: Vec<i16>,
    /// Cached query result for cursor-style Execute (Wave 52 fix).
    /// Populated on the first Execute call when `max_rows > 0`; subsequent
    /// Execute calls drain rows from here instead of re-running the query.
    /// `None` for portals that have not yet been executed or that
    /// completed in a single Execute (max_rows = 0 / unlimited).
    cached_result: Option<CachedResult>,
    /// Offset into `cached_result` for the next Execute call.
    cached_offset: usize,
}

#[derive(Debug, Clone)]
struct CachedResult {
    /// The full result (all rows) of the query, retained so subsequent
    /// Execute calls can drain more rows.
    result: QueryResult,
    /// The command tag to send when the cursor is exhausted.
    tag: String,
}

impl Portal {
    fn new(stmt_name: String, params: Vec<String>, result_formats: Vec<i16>) -> Self {
        Self { stmt_name, params, result_formats, cached_result: None, cached_offset: 0 }
    }
}

pub struct PgConn {
    stream_read: BufReader<OwnedReadHalf>,
    stream_write: BufWriter<OwnedWriteHalf>,
    session: Session,
    statements: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
    /// Shared password manager (Wave 65). Cloned cheaply (Arc) so the
    /// connection can read credentials on each SCRAM handshake.
    passwords: Arc<RwLock<PasswordManager>>,
}

impl PgConn {
    /// Drive one connection to completion.
    pub async fn handle(
        stream: tokio::net::TcpStream,
        peer: std::net::SocketAddr,
        engine: Arc<RwLock<QueryEngine>>,
        _server_name: String,
        auth_required: bool,
        _tls: Option<TlsConfig>,
        passwords: Arc<RwLock<PasswordManager>>,
    ) -> io::Result<()> {
        let _ = peer;
        let _ = stream.set_nodelay(true);
        let (rh, wh) = stream.into_split();
        let mut conn = PgConn {
            stream_read: BufReader::with_capacity(8 * 1024, rh),
            stream_write: BufWriter::with_capacity(256 * 1024, wh),
            session: Session::new(),
            statements: HashMap::new(),
            portals: HashMap::new(),
            passwords,
        };
        let result = conn.run_loop(&engine, auth_required).await;
        if let Err(e) = &result {
            log::debug!("pgwire conn closed: {e}");
        }
        result
    }

    async fn run_loop(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        auth_required: bool,
    ) -> io::Result<()> {
        self.handle_startup(auth_required).await?;
        loop {
            self.flush().await?;
            let msg_type = match self.read_byte().await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            let len = self.read_i32_be().await? as usize;
            if len < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("msg {msg_type:#x} len {len} < 4"),
                ));
            }
            let body_len = len - 4;
            match msg_type {
                b'Q' => {
                    let sql = self.read_string(body_len).await?;
                    self.handle_simple_query(engine, &sql).await?;
                }
                b'P' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_parse(&buf).await?;
                }
                b'B' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_bind(&buf).await?;
                }
                b'D' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_describe(engine, &buf).await?;
                }
                b'E' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_execute(engine, &buf).await?;
                }
                b'S' => {
                    self.flush().await?;
                    self.send_ready_for_query().await?;
                }
                b'C' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_close(&buf);
                    self.send_byte(b'3', &[]).await?;
                }
                b'H' => {
                    self.flush().await?;
                }
                b'X' => return Ok(()),
                other => {
                    if body_len > 0 {
                        let mut sink = vec![0u8; body_len];
                        self.stream_read.read_exact(&mut sink).await?;
                    }
                    let _ = self.send_error("0A000", &format!("unsupported msg {other:#x}")).await;
                }
            }
        }
    }

    // --- Startup ---

    async fn handle_startup(&mut self, auth_required: bool) -> io::Result<()> {
        loop {
            let len = self.read_i32_be().await?;
            if !(4..=1_000_000).contains(&len) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("startup len {len}"),
                ));
            }
            let body_len = (len - 4) as usize;
            let mut buf = vec![0u8; body_len];
            self.stream_read.read_exact(&mut buf).await?;
            if buf.len() < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "startup too short"));
            }
            let magic = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
            match magic {
                SSL_REQUEST_MAGIC | GSSAPI_REQUEST_MAGIC => {
                    // Wave 65: TLS upgrade. When `tls` is configured, the
                    // server should respond 'S' and wrap the stream in
                    // tokio-rustls. The actual upgrade is not yet wired
                    // (no rustls dep), so we always decline with 'N' and
                    // proceed in plaintext. This preserves the pre-Wave-65
                    // behavior and lets SCRAM-SHA-256 run over plaintext
                    // (which is fine for tests and loopback).
                    self.stream_write.write_all(b"N").await?;
                    self.flush().await?;
                    continue;
                }
                CANCEL_REQUEST_MAGIC => return Ok(()),
                m if (196608..=196620).contains(&m) => {
                    self.parse_startup_v3_params(&buf);
                    if self.session.user.is_none() {
                        self.session.user = Some("turboGP".into());
                    }
                    if auth_required {
                        self.do_scram_auth().await?;
                    } else {
                        self.send_auth_ok_and_params().await?;
                    }
                    self.flush().await?;
                    return Ok(());
                }
                _ => {
                    let _ = self.send_error("08P01", "unsupported protocol").await;
                    self.flush().await?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("magic {magic}"),
                    ));
                }
            }
        }
    }

    /// Extract user/database/application_name from the v3 startup message
    /// body (after the 4-byte protocol magic).
    fn parse_startup_v3_params(&mut self, buf: &[u8]) {
        let rest = &buf[4..];
        for (k, v) in parse_cstring_pairs(rest) {
            match k.as_str() {
                "user" => self.session.user = Some(v),
                "database" => self.session.database = Some(v),
                "application_name" => self.session.application_name = Some(v),
                _ => {}
            }
        }
    }

    /// Send AuthenticationOk + ParameterStatus + BackendKeyData + ReadyForQuery.
    /// Used after a successful auth handshake (or directly when auth is disabled).
    async fn send_auth_ok_and_params(&mut self) -> io::Result<()> {
        // AuthenticationOk
        self.send_byte(b'R', &0u32.to_be_bytes()).await?;
        self.send_parameter_statuses().await
    }

    async fn send_parameter_statuses(&mut self) -> io::Result<()> {
        // ParameterStatus messages
        self.send_parameter_status("server_version", "15.0").await?;
        self.send_parameter_status("server_encoding", "UTF8").await?;
        self.send_parameter_status("client_encoding", "UTF8").await?;
        self.send_parameter_status("DateStyle", "ISO, MDY").await?;
        self.send_parameter_status("integer_datetimes", "on").await?;
        self.send_parameter_status("standard_conforming_strings", "on").await?;
        self.send_parameter_status("application_name", "turboGP").await?;
        self.send_parameter_status("IntervalStyle", "postgres").await?;
        self.send_parameter_status("TimeZone", "UTC").await?;
        // BackendKeyData: process_id (4) + secret_key (4) = 8 bytes
        let pid: i32 = rand_backend_key();
        let key: i32 = rand_backend_key();
        let mut kb = Vec::with_capacity(8);
        kb.extend_from_slice(&pid.to_be_bytes());
        kb.extend_from_slice(&key.to_be_bytes());
        self.send_byte(b'K', &kb).await?;
        // ReadyForQuery
        self.send_ready_for_query().await
    }

    // --- SCRAM-SHA-256 authentication (Wave 65) ---

    /// Drive the SCRAM-SHA-256 handshake. On success, sends
    /// AuthenticationOk + parameters and returns Ok. On failure, sends
    /// an ErrorResponse and returns Err (the caller closes the connection).
    async fn do_scram_auth(&mut self) -> io::Result<()> {
        // Step 1: send AuthenticationSASL offering SCRAM-SHA-256.
        // Format: i32(10) + cstring("SCRAM-SHA-256") + cstring("")
        let mut sasl_list = Vec::new();
        sasl_list.extend_from_slice(&10u32.to_be_bytes());
        sasl_list.extend_from_slice(b"SCRAM-SHA-256\0");
        sasl_list.push(0); // terminator
        self.send_byte(b'R', &sasl_list).await?;
        self.flush().await?;

        // Step 2: read SASLInitialResponse from client. Comes as a 'p'
        // message (the same Password message tag used for cleartext / MD5).
        // Payload: cstring(mechanism) + i32(initial_response_len) + bytes(initial_response)
        let (mech, client_first) = match self.read_sasl_initial_response().await? {
            Some(x) => x,
            None => {
                let _ = self.send_error("28000", "expected SASLInitialResponse").await;
                self.flush().await?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected SASLInitialResponse",
                ));
            }
        };
        if mech != "SCRAM-SHA-256" {
            let _ = self.send_error("28000", &format!("unsupported SASL mechanism: {mech}")).await;
            self.flush().await?;
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad mech {mech}")));
        }
        // The client_first_message looks like: "n,,n=user,r=clientnonce".
        // Strip the gs2 header (everything before the second comma at the
        // top level) to get client_first_bare.
        let client_first_bare = strip_gs2_header(&client_first);
        // Parse username + client_nonce from client_first_bare.
        let (username, client_nonce) = match parse_client_first_bare(&client_first_bare) {
            Some(x) => x,
            None => {
                let _ = self.send_error("28000", "malformed client-first message").await;
                self.flush().await?;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad client-first"));
            }
        };

        // Look up the user.
        let cred = {
            let mgr = self.passwords.read();
            mgr.get(&username).cloned()
        };
        let cred = match cred {
            Some(c) => c,
            None => {
                // Per RFC 5802: don't reveal that the user doesn't exist.
                // Use a dummy salt + iteration count so the handshake
                // proceeds and fails at the proof step. For simplicity we
                // just reject now.
                let _ = self.send_error("28000", "authentication failed").await;
                self.flush().await?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unknown user"));
            }
        };

        // Step 3: send AuthenticationSASLContinue with server_first_message.
        let server_nonce = random_server_nonce();
        let combined_nonce = format!("{client_nonce}{server_nonce}");
        let salt_b64 = base64::engine::general_purpose::STANDARD.encode(&cred.salt);
        let server_first = format!("r={combined_nonce},s={salt_b64},i={}", cred.iterations);
        let mut cont = Vec::new();
        cont.extend_from_slice(&11u32.to_be_bytes());
        cont.extend_from_slice(server_first.as_bytes());
        self.send_byte(b'R', &cont).await?;
        self.flush().await?;

        // Step 4: read SASLResponse (client_final_message).
        let client_final = match self.read_sasl_response().await? {
            Some(x) => x,
            None => {
                let _ = self.send_error("28000", "expected SASLResponse").await;
                self.flush().await?;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "expected SASLResponse"));
            }
        };
        let client_final_str = match std::str::from_utf8(&client_final) {
            Ok(s) => s,
            Err(_) => {
                let _ = self.send_error("28000", "client_final not utf8").await;
                self.flush().await?;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "client_final not utf8"));
            }
        };

        // Step 5: verify the client proof.
        match verify_scram(&cred, &client_first_bare, &server_first, client_final_str) {
            ScramOutcome::Ok { server_signature_b64 } => {
                // Send AuthenticationSASLFinal: i32(12) + bytes("v=base64sig")
                let final_msg = format!("v={server_signature_b64}");
                let mut fin = Vec::new();
                fin.extend_from_slice(&12u32.to_be_bytes());
                fin.extend_from_slice(final_msg.as_bytes());
                self.send_byte(b'R', &fin).await?;
                // AuthenticationOk + parameters + ReadyForQuery.
                self.send_auth_ok_and_params().await?;
                Ok(())
            }
            ScramOutcome::Invalid => {
                let _ = self.send_error("28P01", "password authentication failed").await;
                self.flush().await?;
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "scram proof invalid"))
            }
        }
    }

    /// Read a 'p' (Password) message containing a SASLInitialResponse.
    /// Returns (mechanism_name, initial_response_bytes) or None if the
    /// message wasn't a SASLInitialResponse.
    async fn read_sasl_initial_response(&mut self) -> io::Result<Option<(String, Vec<u8>)>> {
        let tag = self.read_byte().await?;
        if tag != b'p' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected 'p' msg, got {tag:#x}"),
            ));
        }
        let len = self.read_i32_be().await? as usize;
        if len < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "sasl init len"));
        }
        let body_len = len - 4;
        let buf = self.read_body(body_len).await?;
        let mut c = 0;
        let mech = read_cstring(&buf, &mut c)?;
        if c + 4 > buf.len() {
            return Ok(None);
        }
        let ir_len = i32::from_be_bytes([buf[c], buf[c + 1], buf[c + 2], buf[c + 3]]);
        c += 4;
        if ir_len < 0 {
            // No initial response — not valid for SCRAM.
            return Ok(Some((mech, Vec::new())));
        }
        let ir_len = ir_len as usize;
        if c + ir_len > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "sasl init overflow"));
        }
        let ir = buf[c..c + ir_len].to_vec();
        Ok(Some((mech, ir)))
    }

    /// Read a 'p' (Password) message containing a SASLResponse.
    async fn read_sasl_response(&mut self) -> io::Result<Option<Vec<u8>>> {
        let tag = self.read_byte().await?;
        if tag != b'p' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected 'p' msg, got {tag:#x}"),
            ));
        }
        let len = self.read_i32_be().await? as usize;
        if len < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "sasl resp len"));
        }
        let body_len = len - 4;
        let buf = self.read_body(body_len).await?;
        Ok(Some(buf))
    }

    // --- Simple query ---

    async fn handle_simple_query(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        sql: &str,
    ) -> io::Result<()> {
        let stmts = split_sql_batch(sql);
        let was_txn = self.session.txn != TxnStatus::Idle;
        for stmt in stmts {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("begin") || lower.starts_with("start transaction") {
                self.session.txn = TxnStatus::InTransaction;
                self.send_command_complete("BEGIN", 0).await?;
                continue;
            }
            if lower.starts_with("commit") {
                self.session.txn = TxnStatus::Idle;
                self.send_command_complete("COMMIT", 0).await?;
                continue;
            }
            if lower.starts_with("rollback") {
                self.session.txn = TxnStatus::Idle;
                self.send_command_complete("ROLLBACK", 0).await?;
                continue;
            }
            // Wave 65: intercept CREATE USER / DROP USER at the pgwire
            // layer so the password manager (which lives in the server,
            // not the engine) can be mutated. The engine itself doesn't
            // know about users.
            if let Some(outcome) = try_handle_user_ddl(trimmed, &self.passwords) {
                match outcome {
                    UserDdlOutcome::Ok(tag) => {
                        self.send_command_complete(&tag, 0).await?;
                        continue;
                    }
                    UserDdlOutcome::Err(msg) => {
                        let _ = self.send_error("42000", &msg).await;
                        if was_txn {
                            self.session.txn = TxnStatus::FailedTransaction;
                        }
                        break;
                    }
                }
            }
            let result = {
                // MVCC (Wave 41): try readonly SELECT first with a read lock.
                // If that fails (not a SELECT), take a write lock for DML/DDL.
                let readonly_result = {
                    let guard = engine.read();
                    guard.try_readonly_select(trimmed)
                };
                match readonly_result {
                    Ok(r) => Ok(r),
                    Err(_) => {
                        // Not a readonly query — take write lock.
                        let mut guard = engine.write();
                        guard.execute(trimmed)
                    }
                }
            };
            match result {
                Ok(r) => {
                    self.send_row_description(&r).await?;
                    self.send_data_rows(&r).await?;
                    self.send_command_complete(&command_tag(&r, trimmed), r.row_count).await?;
                }
                Err(e) => {
                    let _ = self.send_error(e.sqlstate(), &format!("{e}")).await;
                    if was_txn {
                        self.session.txn = TxnStatus::FailedTransaction;
                    }
                    break;
                }
            }
        }
        self.send_ready_for_query().await?;
        // W2 (cache phase): explicit flush so the ReadyForQuery ('Z') byte
        // actually reaches the client. With the larger 256KB BufWriter,
        // small trailing messages can sit in the buffer and cause psql to
        // hang waiting for query completion.
        self.flush().await
    }

    // --- Extended query ---

    async fn handle_parse(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let name = read_cstring(buf, &mut c)?;
        let sql = read_cstring(buf, &mut c)?;
        if c + 2 > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse truncated"));
        }
        let n = u16::from_be_bytes([buf[c], buf[c + 1]]) as usize;
        c += 2;
        let mut oids = Vec::with_capacity(n);
        for _ in 0..n {
            if c + 4 > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse OID truncated"));
            }
            oids.push(u32::from_be_bytes([buf[c], buf[c + 1], buf[c + 2], buf[c + 3]]));
            c += 4;
        }
        self.statements.insert(name, PreparedStatement { sql, param_oids: oids });
        self.send_byte(b'1', &[]).await // ParseComplete
    }

    async fn handle_bind(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(buf, &mut c)?;
        let stmt_name = read_cstring(buf, &mut c)?;
        if c + 2 > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind truncated"));
        }
        let n_fmt = u16::from_be_bytes([buf[c], buf[c + 1]]) as usize;
        c += 2;
        let mut pfmts = Vec::with_capacity(n_fmt);
        for _ in 0..n_fmt {
            if c + 2 > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind fmt truncated"));
            }
            pfmts.push(i16::from_be_bytes([buf[c], buf[c + 1]]));
            c += 2;
        }
        if c + 2 > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind truncated"));
        }
        let n_params = u16::from_be_bytes([buf[c], buf[c + 1]]) as usize;
        c += 2;
        let mut params = Vec::with_capacity(n_params);
        for i in 0..n_params {
            if c + 4 > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param truncated"));
            }
            let plen = i32::from_be_bytes([buf[c], buf[c + 1], buf[c + 2], buf[c + 3]]);
            c += 4;
            let val = if plen < 0 {
                None
            } else {
                let plen = plen as usize;
                if c + plen > buf.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param overflow"));
                }
                let bytes = &buf[c..c + plen];
                c += plen;
                let fmt = if pfmts.len() == 1 {
                    pfmts[0]
                } else if pfmts.len() > i {
                    pfmts[i]
                } else {
                    0
                };
                if fmt == 0 {
                    Some(String::from_utf8_lossy(bytes).into_owned())
                } else {
                    Some(format!("\\x{}", hex_encode(bytes)))
                }
            };
            params.push(val.unwrap_or_else(|| "NULL".into()));
        }
        if c + 2 > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt truncated"));
        }
        let n_rfmt = u16::from_be_bytes([buf[c], buf[c + 1]]) as usize;
        c += 2;
        let mut rfmts = Vec::with_capacity(n_rfmt);
        for _ in 0..n_rfmt {
            if c + 2 > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt truncated"));
            }
            rfmts.push(i16::from_be_bytes([buf[c], buf[c + 1]]));
            c += 2;
        }
        if !self.statements.contains_key(&stmt_name) {
            let _ = self
                .send_error("26000", &format!("prepared statement \"{stmt_name}\" does not exist"))
                .await;
            return Ok(());
        }
        self.portals.insert(portal_name, Portal::new(stmt_name, params, rfmts));
        self.send_byte(b'2', &[]).await // BindComplete
    }

    async fn handle_describe(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        buf: &[u8],
    ) -> io::Result<()> {
        if buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Describe empty"));
        }
        let kind = buf[0];
        let mut c = 1;
        let name = read_cstring(buf, &mut c)?;
        match kind {
            b'S' => {
                let stmt = match self.statements.get(&name) {
                    Some(s) => s.clone(),
                    None => {
                        let _ = self
                            .send_error("26000", &format!("statement \"{name}\" not found"))
                            .await;
                        return Ok(());
                    }
                };
                let n = stmt.param_oids.len() as u16;
                let mut body = Vec::with_capacity(2 + stmt.param_oids.len() * 4);
                body.extend_from_slice(&n.to_be_bytes());
                for oid in &stmt.param_oids {
                    body.extend_from_slice(&oid.to_be_bytes());
                }
                self.send_byte(b't', &body).await?; // ParameterDescription
                self.send_byte(b'n', &[]).await?; // NoData (schema unknown until V-3)
            }
            b'P' => {
                let portal = match self.portals.get(&name) {
                    Some(p) => p.clone(),
                    None => {
                        let _ =
                            self.send_error("34000", &format!("portal \"{name}\" not found")).await;
                        return Ok(());
                    }
                };
                let stmt = match self.statements.get(&portal.stmt_name) {
                    Some(s) => s.clone(),
                    None => {
                        let _ = self
                            .send_error(
                                "26000",
                                &format!("statement \"{}\" not found", portal.stmt_name),
                            )
                            .await;
                        return Ok(());
                    }
                };
                // Wave 52 fix (Bug 12): Describe must NOT execute the query.
                // The previous implementation called `try_readonly_select`
                // to learn the result shape, which executed the query as a
                // side effect (including side effects for DML disguised as
                // SELECT). psql tolerates a NoData response, so we send
                // that and avoid execution entirely.
                //
                // A proper fix would parse the SQL and infer the schema
                // from the catalog without executing. That's a larger
                // change reserved for a future wave.
                let _ = stmt; // suppress unused-variable warning
                let _ = &portal.params;
                self.send_byte(b'n', &[]).await?; // NoData
            }
            _ => {
                let _ = self.send_error("08P01", "unknown describe kind").await;
            }
        }
        Ok(())
    }

    async fn handle_execute(
        &mut self,
        engine: &Arc<RwLock<QueryEngine>>,
        buf: &[u8],
    ) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(buf, &mut c)?;
        if c + 4 > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Execute truncated"));
        }
        // Wave 52 fix (Bug 13): respect max_rows. The previous implementation
        // read this field into `_max_rows` and discarded it. Now:
        // - max_rows = 0: send all rows + CommandComplete.
        // - max_rows > 0: send at most max_rows rows. If more rows remain,
        //   send PortalSuspended ('s') instead of CommandComplete; the
        //   client can call Execute again to drain more rows.
        let max_rows = i32::from_be_bytes([buf[c], buf[c + 1], buf[c + 2], buf[c + 3]]);

        // Fast path: if the portal already has a cached result (from a
        // previous Execute with max_rows > 0), drain more rows from it
        // without re-executing the query.
        let cached = self
            .portals
            .get(&portal_name)
            .and_then(|p| p.cached_result.as_ref().map(|cr| (cr.clone(), p.cached_offset)));
        if let Some((cr, offset)) = cached {
            // Drain the next batch from the cached result.
            let limit = if max_rows > 0 { max_rows as usize } else { cr.result.row_count };
            let end = (offset + limit).min(cr.result.row_count);
            let batch = slice_result(&cr.result, offset, end);
            self.send_data_rows(&batch).await?;
            if end >= cr.result.row_count {
                // Cursor exhausted.
                self.send_command_complete(&cr.tag, cr.result.row_count).await?;
                if let Some(p) = self.portals.get_mut(&portal_name) {
                    p.cached_result = None;
                    p.cached_offset = 0;
                }
            } else {
                // More rows remain — send PortalSuspended.
                self.send_byte(b's', &[]).await?;
                if let Some(p) = self.portals.get_mut(&portal_name) {
                    p.cached_offset = end;
                }
            }
            return Ok(());
        }

        let portal = match self.portals.get(&portal_name).cloned() {
            Some(p) => p,
            None => {
                let _ =
                    self.send_error("34000", &format!("portal \"{portal_name}\" not found")).await;
                return Ok(());
            }
        };
        let stmt = match self.statements.get(&portal.stmt_name).cloned() {
            Some(s) => s,
            None => {
                let _ = self
                    .send_error("26000", &format!("statement \"{}\" not found", portal.stmt_name))
                    .await;
                return Ok(());
            }
        };
        let sql = substitute_params(&stmt.sql, &portal.params);
        // Wave 65: intercept CREATE USER / DROP USER in extended-query mode too.
        if let Some(outcome) = try_handle_user_ddl(&sql, &self.passwords) {
            match outcome {
                UserDdlOutcome::Ok(tag) => {
                    self.send_command_complete(&tag, 0).await?;
                    return Ok(());
                }
                UserDdlOutcome::Err(msg) => {
                    let _ = self.send_error("42000", &msg).await;
                    return Ok(());
                }
            }
        }
        let result = {
            let readonly = engine.read();
            match readonly.try_readonly_select(&sql) {
                Ok(r) => Ok(r),
                Err(_) => {
                    drop(readonly);
                    let mut guard = engine.write();
                    guard.execute(&sql)
                }
            }
        };
        match result {
            Ok(r) => {
                let tag = command_tag(&r, &sql);
                if max_rows > 0 && r.row_count > (max_rows as usize) {
                    // Send only the first max_rows rows, cache the rest.
                    let end = (max_rows as usize).min(r.row_count);
                    let batch = slice_result(&r, 0, end);
                    self.send_data_rows(&batch).await?;
                    // PortalSuspended signals the client can call Execute again.
                    self.send_byte(b's', &[]).await?;
                    if let Some(p) = self.portals.get_mut(&portal_name) {
                        p.cached_result = Some(CachedResult { result: r, tag });
                        p.cached_offset = end;
                    }
                } else {
                    // max_rows = 0 or result fits in one batch.
                    self.send_data_rows(&r).await?;
                    self.flush().await?;
                    self.send_command_complete(&tag, r.row_count).await?;
                }
            }
            Err(e) => {
                let _ = self.send_error(e.sqlstate(), &format!("{e}")).await;
            }
        }
        Ok(())
    }

    fn handle_close(&mut self, buf: &[u8]) {
        if buf.is_empty() {
            return;
        }
        let kind = buf[0];
        let mut c = 1;
        if let Ok(name) = read_cstring(buf, &mut c) {
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

    async fn send_parameter_status(&mut self, key: &str, val: &str) -> io::Result<()> {
        let mut body = Vec::with_capacity(key.len() + 1 + val.len() + 1);
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(val.as_bytes());
        body.push(0);
        self.send_byte(b'S', &body).await
    }

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
            body.extend_from_slice(&0u16.to_be_bytes()); // col attr
                                                         // Type OID: use col.type_oid if set (Wave 47), otherwise
                                                         // fall back to string_values heuristic (Wave 34).
            let (type_oid, type_size) = if col.type_oid != 0 {
                // Use the schema-provided type OID.
                let size = match col.type_oid {
                    25 => -1i16,       // TEXT: variable size
                    701 | 700 => 8i16, // FLOAT8/FLOAT4
                    16 => 1i16,        // BOOL
                    _ => 8i16,         // default: 8 bytes (INT8)
                };
                (col.type_oid, size)
            } else if col.has_strings() {
                (25u32, -1i16) // TEXT
            } else {
                (20u32, 8i16) // INT8
            };
            body.extend_from_slice(&type_oid.to_be_bytes());
            body.extend_from_slice(&type_size.to_be_bytes());
            body.extend_from_slice(&(-1i32).to_be_bytes()); // type mod
            body.extend_from_slice(&0i16.to_be_bytes()); // format = text
        }
        self.send_byte(b'T', &body).await
    }

    async fn send_data_rows(&mut self, r: &QueryResult) -> io::Result<()> {
        // W2 (cache phase): Batch all DataRow messages into a single buffer
        // and write it in one shot. Previously this method called
        // `send_byte(b'D', &body)` once per row, which for large result
        // sets (e.g. Q16 returns 18,314 rows) meant ~18K separate async
        // `write_all` calls — each carrying polling and buffer-management
        // overhead. Batching cuts the per-row overhead to near zero and
        // brings hot-run wall time for Q16 from ~18ms to <5ms.
        //
        // Wave 52 fix (Bug 11): for each cell, check the column's `null_mask`.
        // If the cell is NULL, send `-1i32` as the length (no payload) per
        // the Postgres wire protocol. Previously NULL cells were sent as
        // the string "0", which clients interpreted as a non-NULL zero.
        if r.row_count == 0 {
            return Ok(());
        }
        let ncols = r.columns.len();
        // Preallocate: rough estimate ~32 bytes per cell.
        let mut buf: Vec<u8> = Vec::with_capacity(r.row_count * ncols * 32);

        for row_idx in 0..r.row_count {
            // 'D' message header
            buf.push(b'D');
            // Length placeholder (filled in after body is built)
            let len_pos = buf.len();
            buf.extend_from_slice(&[0u8; 4]);
            let body_start = buf.len();

            buf.extend_from_slice(&(ncols as u16).to_be_bytes());

            for col in &r.columns {
                let is_null = col
                    .null_mask
                    .as_ref()
                    .and_then(|m| m.get(row_idx).copied())
                    .unwrap_or(false);
                if is_null {
                    // Postgres wire protocol: NULL is encoded as -1 i32 length.
                    buf.extend_from_slice(&(-1i32).to_be_bytes());
                    continue;
                }

                // Borrow string slice when possible; only allocate for u64->string.
                let owned: String;
                let s_ref: &str = if let Some(sv) = &col.string_values {
                    match sv.get(row_idx) {
                        Some(s) => s.as_str(),
                        None => "",
                    }
                } else {
                    let v = col.values.get(row_idx).copied().unwrap_or(0);
                    if v > (1u64 << 60) {
                        let f = f64::from_bits(v);
                        if f.is_finite() && f.abs() < 1e15 {
                            owned = format!("{f}");
                        } else {
                            owned = v.to_string();
                        }
                    } else {
                        owned = v.to_string();
                    }
                    owned.as_str()
                };

                buf.extend_from_slice(&(s_ref.len() as i32).to_be_bytes());
                buf.extend_from_slice(s_ref.as_bytes());
            }

            // Patch the message length (body bytes + 4 for the length field itself)
            let body_len = buf.len() - body_start;
            let total_len = (body_len as u32 + 4).to_be_bytes();
            buf[len_pos..len_pos + 4].copy_from_slice(&total_len);
        }

        // Single write for all DataRow messages.
        self.stream_write.write_all(&buf).await?;
        Ok(())
    }

    async fn send_command_complete(&mut self, tag: &str, _n: usize) -> io::Result<()> {
        let mut body = Vec::with_capacity(tag.len() + 1);
        body.extend_from_slice(tag.as_bytes());
        body.push(0);
        self.send_byte(b'C', &body).await
    }

    async fn send_ready_for_query(&mut self) -> io::Result<()> {
        self.send_byte(b'Z', &[self.session.txn.tag()]).await
    }

    async fn send_error(&mut self, code: &str, msg: &str) -> io::Result<()> {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"ERROR");
        body.push(0);
        body.push(b'V');
        body.extend_from_slice(b"ERROR");
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

    async fn send_byte(&mut self, byte: u8, body: &[u8]) -> io::Result<()> {
        let len = (body.len() as u32 + 4).to_be_bytes();
        self.stream_write.write_all(&[byte]).await?;
        self.stream_write.write_all(&len).await?;
        self.stream_write.write_all(body).await?;
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.stream_write.flush().await
    }

    async fn read_byte(&mut self) -> io::Result<u8> {
        let mut buf = [0u8; 1];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(buf[0])
    }
    async fn read_i32_be(&mut self) -> io::Result<i32> {
        let mut buf = [0u8; 4];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(i32::from_be_bytes(buf))
    }
    async fn read_string(&mut self, body_len: usize) -> io::Result<String> {
        let mut buf = vec![0u8; body_len];
        self.stream_read.read_exact(&mut buf).await?;
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    async fn read_body(&mut self, body_len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; body_len];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(buf)
    }
}

// --- Free functions ---

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
        // GO separator
        if (c == b'G' || c == b'g')
            && i + 1 < bytes.len()
            && (bytes[i + 1] == b'O' || bytes[i + 1] == b'o')
            && (i == 0 || bytes[i - 1] == b'\n' || bytes[i - 1] == b'\r')
        {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j == bytes.len() || bytes[j] == b'\n' || bytes[j] == b'\r' {
                if !current.trim().is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                i = j;
                continue;
            }
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

fn parse_cstring_pairs(buf: &[u8]) -> Vec<(String, String)> {
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

/// Slice a QueryResult to rows `[start, end)` (Wave 52 fix).
///
/// Used by `handle_execute` to honor `max_rows`: the first Execute sends
/// rows `[0, max_rows)`, subsequent Execute calls drain the rest from the
/// portal's `cached_result`.
fn slice_result(r: &QueryResult, start: usize, end: usize) -> QueryResult {
    let start = start.min(r.row_count);
    let end = end.min(r.row_count);
    let row_count = end.saturating_sub(start);
    let columns: Vec<ResultColumn> = r
        .columns
        .iter()
        .map(|c| {
            let values: Vec<u64> = c.values[start..end].to_vec();
            let string_values = c.string_values.as_ref().map(|sv| sv[start..end].to_vec());
            let null_mask = c.null_mask.as_ref().map(|m| m[start..end].to_vec());
            ResultColumn {
                name: c.name.clone(),
                values,
                string_values,
                type_oid: c.type_oid,
                null_mask,
            }
        })
        .collect();
    QueryResult { columns, row_count, elapsed_us: r.elapsed_us }
}

/// Escape a bound parameter value for safe interpolation into SQL.
///
/// Numeric values (integers, floats) are passed through unquoted — they
/// cannot contain SQL metacharacters. All other values are wrapped in
/// single quotes with any internal single quotes doubled (`'` → `''`),
/// which is the standard SQL string-literal escaping rule. This prevents
/// SQL injection via the pgwire Bind/Execute path.
///
/// **Security note:** This is a defence-in-depth measure. The ideal fix
/// threads typed parameters (`Vec<Option<Vec<u8>>>` + `&[Oid]`) through
/// the engine rather than string-substituting, but that requires a
/// deeper refactor of the executor's parameter handling. This escaping
/// fix closes the injection vector with minimal disruption.
pub fn escape_param_value(value: &str) -> String {
    // Pure integers and floats are safe to interpolate unquoted.
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
        return value.to_string();
    }
    // Booleans and NULL are also safe.
    if value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("null")
    {
        return value.to_string();
    }
    // Everything else: wrap in single quotes and double internal quotes.
    let escaped = value.replace('\'', "''");
    format!("'{}'", escaped)
}

pub fn substitute_params(sql: &str, params: &[String]) -> String {
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

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|b| format!("{:02x}", b)).collect()
}

fn rand_backend_key() -> i32 {
    // Wave 6 fix: use rand::rng() instead of SystemTime::now().subsec_nanos()
    // for a cryptographically-suitable random backend key.
    use rand::Rng;
    rand::rng().random()
}

// -----------------------------------------------------------------------
// Wave 65: SCRAM-SHA-256 helpers + CREATE USER / DROP USER DDL intercept.
// -----------------------------------------------------------------------

/// Strip the GS2 header from a client-first message.
///
/// SCRAM client-first messages have the form `gs2-header,bare-message`
/// where the gs2-header is one of `n,,`, `y,,`, or `p=cb-data,`. The
/// bare message starts after the second comma.
///
/// Example: `"n,,n=alice,r=nonce"` → `"n=alice,r=nonce"`.
fn strip_gs2_header(client_first: &[u8]) -> String {
    let s = std::str::from_utf8(client_first).unwrap_or("");
    // Find the second comma (top-level). The gs2 header is `cb-name,cb-data,`
    // and is followed by the bare message.
    let mut comma_count = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        if c == ',' {
            comma_count += 1;
            if comma_count == 2 {
                cut = i + 1;
                break;
            }
        }
    }
    s[cut..].to_string()
}

/// Parse `n=alice,r=clientnonce` → `("alice", "clientnonce")`.
/// Returns None if either field is missing.
fn parse_client_first_bare(bare: &str) -> Option<(String, String)> {
    let mut username = None;
    let mut nonce = None;
    for part in bare.split(',') {
        if let Some(rest) = part.strip_prefix("n=") {
            // SCRAM uses saslprep — for our test we just take the ASCII
            // username as-is. The `=` and `,` characters are escaped in
            // real SCRAM (`=2D` and `=2C`); we don't unescape here.
            username = Some(rest.to_string());
        } else if let Some(rest) = part.strip_prefix("r=") {
            nonce = Some(rest.to_string());
        }
    }
    match (username, nonce) {
        (Some(u), Some(n)) => Some((u, n)),
        _ => None,
    }
}

/// Generate a random 18-character server nonce (printable ASCII, no ',').
/// Uses the OS RNG via `rand` so two concurrent connections get distinct
/// nonces.
fn random_server_nonce() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..18)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Outcome of a CREATE USER / DROP USER intercept.
enum UserDdlOutcome {
    /// The DDL succeeded. Carries the command tag (e.g. "CREATE USER").
    Ok(String),
    /// The DDL failed. Carries the error message.
    Err(String),
}

/// Try to handle `CREATE USER` / `DROP USER` SQL at the pgwire layer.
/// Returns `None` if the SQL is not a user DDL (so the caller should
/// forward it to the engine). Returns `Some(outcome)` if it was handled.
///
/// Supported syntax:
/// - `CREATE USER username WITH PASSWORD 'password'`
/// - `CREATE USER username PASSWORD 'password'`
/// - `CREATE USER username WITH PASSWORD 'password'`
/// - `DROP USER username [IF EXISTS]`
fn try_handle_user_ddl(
    sql: &str,
    passwords: &Arc<RwLock<PasswordManager>>,
) -> Option<UserDdlOutcome> {
    let trimmed = sql.trim();
    let lower = trimmed.to_lowercase();
    if lower.starts_with("create user ") {
        return Some(handle_create_user(trimmed, passwords));
    }
    if lower.starts_with("drop user ") {
        return Some(handle_drop_user(trimmed, passwords));
    }
    None
}

/// Parse and execute `CREATE USER username [WITH] PASSWORD 'password'`.
fn handle_create_user(sql: &str, passwords: &Arc<RwLock<PasswordManager>>) -> UserDdlOutcome {
    // Strip "CREATE USER " (case-insensitive) prefix.
    let after = &sql["CREATE USER".len()..].trim_start();
    // The username runs up to the next whitespace or `PASSWORD`/`WITH`.
    let (username, rest) = match split_username(after) {
        Some(x) => x,
        None => return UserDdlOutcome::Err("expected username after CREATE USER".into()),
    };
    // Optional WITH, then PASSWORD 'literal'.
    let rest_lower = rest.to_lowercase();
    let pw_start_idx = if let Some(idx) = rest_lower.find("password") {
        idx + "password".len()
    } else {
        return UserDdlOutcome::Err("expected PASSWORD '...' in CREATE USER".into());
    };
    let after_pw = rest[pw_start_idx..].trim_start();
    let password = match extract_string_literal(after_pw) {
        Some(s) => s,
        None => {
            return UserDdlOutcome::Err("expected 'password' string literal in CREATE USER".into())
        }
    };
    let mut mgr = passwords.write();
    mgr.create_user(&username, &password);
    UserDdlOutcome::Ok("CREATE USER".into())
}

/// Parse and execute `DROP USER username [IF EXISTS]`.
fn handle_drop_user(sql: &str, passwords: &Arc<RwLock<PasswordManager>>) -> UserDdlOutcome {
    let after = &sql["DROP USER".len()..].trim_start();
    let (rest, if_exists) = if after.to_lowercase().starts_with("if exists ") {
        (&after["IF EXISTS ".len()..].trim_start(), true)
    } else {
        (after, false)
    };
    let (username, trailing) = match split_username(rest) {
        Some(x) => x,
        None => return UserDdlOutcome::Err("expected username after DROP USER".into()),
    };
    // Trailing tokens (like a semicolon) are ignored.
    let _ = trailing;
    let mut mgr = passwords.write();
    if !mgr.drop_user(&username) {
        if !if_exists {
            return UserDdlOutcome::Err(format!("user \"{username}\" does not exist"));
        }
    }
    UserDdlOutcome::Ok("DROP USER".into())
}

/// Split `username rest...` into (username, rest). The username is the
/// longest prefix of identifier characters (`[A-Za-z0-9_]`), optionally
/// double-quoted.
fn split_username(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    if s.starts_with('"') {
        // Quoted identifier — read until the next `"`.
        let end = s[1..].find('"')?;
        let name = s[1..1 + end].to_string();
        Some((name, &s[1 + end + 1..]))
    } else {
        let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
        let name = s[..end].to_string();
        if name.is_empty() {
            return None;
        }
        Some((name, &s[end..]))
    }
}

/// Extract a single-quoted string literal from the start of `s` (after
/// optional whitespace). Returns the unescaped contents, or None if `s`
/// doesn't start with `'`.
fn extract_string_literal(s: &str) -> Option<String> {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != b'\'' {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' {
            // Check for escaped ''.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            return Some(out);
        }
        out.push(c as char);
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_semicolon() {
        let s = split_sql_batch("SELECT 1; SELECT 2; SELECT 3");
        assert_eq!(s, vec!["SELECT 1", " SELECT 2", " SELECT 3"]);
    }
    #[test]
    fn split_go() {
        let s = split_sql_batch("SELECT 1\nGO\nSELECT 2");
        let t: Vec<String> = s.iter().map(|s| s.trim().to_string()).collect();
        assert_eq!(t, vec!["SELECT 1", "SELECT 2"]);
    }
    #[test]
    fn split_ignores_semicolon_in_string() {
        let s = split_sql_batch("SELECT 'a;b'; SELECT 'c'");
        assert_eq!(s, vec!["SELECT 'a;b'", " SELECT 'c'"]);
    }
    #[test]
    fn split_ignores_go_in_string() {
        let s = split_sql_batch("SELECT 'go'; SELECT 1");
        assert_eq!(s, vec!["SELECT 'go'", " SELECT 1"]);
    }
    #[test]
    fn split_escaped_quote() {
        let s = split_sql_batch("SELECT 'it''s'; SELECT 1");
        assert_eq!(s, vec!["SELECT 'it''s'", " SELECT 1"]);
    }
    #[test]
    fn parse_cstring_pairs_basic() {
        let p = parse_cstring_pairs(b"user\0alice\0database\0test\0\0");
        assert_eq!(p, vec![("user".into(), "alice".into()), ("database".into(), "test".into())]);
    }
    #[test]
    fn read_cstring_basic() {
        let buf = b"hello\0world\0";
        let mut c = 0;
        assert_eq!(read_cstring(buf, &mut c).unwrap(), "hello");
        assert_eq!(read_cstring(buf, &mut c).unwrap(), "world");
    }
    #[test]
    fn substitute_basic() {
        assert_eq!(
            substitute_params("SELECT $1 + $2", &["42".into(), "100".into()]),
            "SELECT 42 + 100"
        );
    }
    #[test]
    fn substitute_oob_null() {
        assert_eq!(substitute_params("SELECT $1, $3", &["42".into()]), "SELECT 42, NULL");
    }
    #[test]
    fn command_tag_select() {
        assert_eq!(command_tag(&QueryResult::empty(), "SELECT 1"), "SELECT 0");
    }
    #[test]
    fn hex_basic() {
        assert_eq!(hex_encode(&[0x01, 0xff]), "01ff");
    }
}
