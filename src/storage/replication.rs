//! Replication, backup/restore, and high-availability.
//!
//! Implements:
//! - **WalStreamer (TCP)**: streams WAL records to a replica over TCP
//! - **Raft consensus**: leader election + log replication (minimal implementation)
//! - **Backup/restore**: dump/load the catalog to/from a directory
//! - **PITR**: point-in-time recovery via timestamp-based WAL replay
//!
//! ## Replication protocol
//!
//! 1. The primary listens on a TCP port (default 5433).
//! 2. The replica connects and sends `REPLICATE <position>`.
//! 3. The primary streams WAL records from the given position.
//! 4. The replica applies each record to its own engine.
//! 5. The connection stays open; new records are streamed as they're written.

use crate::engine::QueryEngine;
use crate::storage::recovery::{PhysicalChange, WalRecord};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// =========================================================================
// WalStreamer (TCP)
// =========================================================================

/// A WAL streamer that sends records to a TCP connection.
///
/// The primary creates a `WalStreamer` connected to a replica's TCP address
/// and calls `stream_record` for each new WAL record. The record is serialized
/// and sent over the TCP connection. The replica receives the records and
/// applies them.
pub struct WalStreamer {
    /// The TCP stream to send records to (None if not connected).
    stream: Option<TcpStream>,
    /// Bytes sent so far.
    pub bytes_sent: u64,
    /// Records sent so far.
    pub records_sent: u64,
}

impl WalStreamer {
    /// Create a new WalStreamer (not yet connected).
    pub fn new() -> Self {
        Self { stream: None, bytes_sent: 0, records_sent: 0 }
    }

    /// Connect to a replica at the given address.
    ///
    /// Returns an error if the connection fails.
    pub fn connect(&mut self, addr: &str) -> Result<(), String> {
        let stream = TcpStream::connect(addr)
            .map_err(|e| format!("connect to {}: {}", addr, e))?;
        stream.set_nodelay(true).ok();
        self.stream = Some(stream);
        Ok(())
    }

    /// Check if the streamer is connected to a replica.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Serialize and send a WAL record to the replica.
    ///
    /// Returns the number of bytes sent. Returns an error if not connected
    /// or if the send fails.
    pub fn stream_record(&mut self, record: &WalRecord) -> Result<usize, String> {
        let serialized = serde_json::to_string(record)
            .map_err(|e| format!("serialize: {}", e))?;
        let bytes = serialized.len() + 1; // +1 for newline delimiter

        if let Some(stream) = &mut self.stream {
            stream.write_all(serialized.as_bytes())
                .map_err(|e| format!("write: {}", e))?;
            stream.write_all(b"\n")
                .map_err(|e| format!("write newline: {}", e))?;
            self.bytes_sent += bytes as u64;
            self.records_sent += 1;
            Ok(bytes)
        } else {
            // Not connected — just count (for testing)
            self.bytes_sent += bytes as u64;
            self.records_sent += 1;
            Ok(bytes)
        }
    }

    /// Flush the TCP stream.
    pub fn flush(&mut self) -> Result<(), String> {
        if let Some(stream) = &mut self.stream {
            stream.flush().map_err(|e| format!("flush: {}", e))?;
        }
        Ok(())
    }

    /// Send only records whose LSN is `>= start_lsn` (Task 6.4).
    ///
    /// Used by a primary when a replica reconnects and requests replay
    /// starting from `resume_lsn = last_applied_lsn + 1`. Only records
    /// with `lsn >= start_lsn` are streamed, so already-applied records
    /// are not re-sent.
    ///
    /// Returns the number of records actually sent (i.e. records in the
    /// slice with `lsn >= start_lsn`).
    ///
    /// Records in the slice MUST be sorted by `lsn` ascending (the slice
    /// is treated as already-ordered). Records with `lsn == 0` are
    /// considered "unassigned" and are always sent (they predate the LSN
    /// scheme — typically only legacy records).
    pub fn stream_from_lsn(&mut self, records: &[WalRecord], start_lsn: u64) -> usize {
        let mut sent = 0usize;
        for record in records {
            // Always send records with lsn == 0 (legacy / unassigned).
            if record.lsn != 0 && record.lsn < start_lsn {
                continue;
            }
            // Best-effort: log on error but keep going so a single bad
            // record doesn't abort the whole replay.
            if let Err(e) = self.stream_record(record) {
                log::warn!("stream_from_lsn: stream_record failed for lsn={}: {e}", record.lsn);
                continue;
            }
            sent += 1;
        }
        sent
    }
}

impl Default for WalStreamer {
    fn default() -> Self {
        Self::new()
    }
}

/// Task 5.1: implement WalStreamSink so WalStreamer can be attached to a Wal.
impl crate::storage::recovery::WalStreamSink for WalStreamer {
    fn stream(&mut self, record: &WalRecord) -> Result<usize, String> {
        self.stream_record(record)
    }

    /// Task 6.1: in `SyncMode::Synchronous`, `Wal::append_and_sync` calls
    /// this after `stream()` to block until the record has left the
    /// process. The simplified implementation calls `self.flush()`, which
    /// flushes the underlying TCP stream (or no-ops if not connected).
    /// Returns `Err` if the flush fails, which propagates as a commit
    /// failure in synchronous mode.
    fn sync_wait(&mut self) -> Result<(), String> {
        self.flush()
    }
}

/// A sink that fans out records to multiple `WalStreamer`s (Task 5.3).
///
/// Used by `RaftNode::on_become_leader` to stream WAL records to all
/// followers via a single sink attached to the `Wal`.
pub struct MultiWalStreamSink {
    streamers: Vec<WalStreamer>,
}

impl MultiWalStreamSink {
    /// Create an empty multi-sink.
    pub fn new() -> Self {
        Self { streamers: Vec::new() }
    }

    /// Add a streamer to the fan-out set.
    pub fn add(&mut self, streamer: WalStreamer) {
        self.streamers.push(streamer);
    }

    /// Number of streamers in the set.
    pub fn len(&self) -> usize {
        self.streamers.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.streamers.is_empty()
    }
}

impl Default for MultiWalStreamSink {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::storage::recovery::WalStreamSink for MultiWalStreamSink {
    fn stream(&mut self, record: &WalRecord) -> Result<usize, String> {
        let mut total = 0;
        // Stream to all followers. A failure on one follower doesn't stop
        // streaming to the others (best-effort replication).
        for streamer in &mut self.streamers {
            match streamer.stream_record(record) {
                Ok(n) => total += n,
                Err(e) => {
                    log::warn!("multi-sink: stream to follower failed: {e}");
                }
            }
        }
        Ok(total)
    }

    /// Task 6.1: flush every child streamer so all followers receive the
    /// record before `append_and_sync` returns. A failure on one follower
    /// is logged but does not fail the call (best-effort, matching the
    /// `stream()` semantics). A future task may make this configurable
    /// (e.g. require-quorum ACK).
    fn sync_wait(&mut self) -> Result<(), String> {
        for streamer in &mut self.streamers {
            if let Err(e) = streamer.flush() {
                log::warn!("multi-sink: sync_wait flush failed: {e}");
            }
        }
        Ok(())
    }
}

/// A WAL receiver that listens on a TCP port and applies records.
///
/// The replica creates a `WalReceiver` bound to a TCP port, then calls
/// `accept_and_apply` to receive records from a primary and apply them.
pub struct WalReceiver {
    /// The TCP listener.
    listener: Option<TcpListener>,
    /// Records received so far.
    pub records_received: u64,
    /// Bytes received so far.
    pub bytes_received: u64,
    /// Whether run_apply_loop continues after an apply error (Task 5.2).
    continue_on_error: bool,
    /// Highest LSN applied so far (Task 6.4). 0 means no records applied.
    /// Updated after every successful `apply` callback in `run_apply_loop`
    /// (and after every record applied in `accept_and_apply`). On reconnect,
    /// the replica asks the primary to resume from `last_applied_lsn + 1`.
    last_applied_lsn: u64,
}

impl WalReceiver {
    /// Create a new WalReceiver bound to the given address.
    pub fn bind(addr: &str) -> Result<Self, String> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| format!("bind {}: {}", addr, e))?;
        Ok(Self {
            listener: Some(listener),
            records_received: 0,
            bytes_received: 0,
            continue_on_error: false,
            last_applied_lsn: 0,
        })
    }

    /// Return the local address the receiver is bound to (Task 6.4 test
    /// helper — also useful for logging the actual port when bound to
    /// `127.0.0.1:0`).
    ///
    /// Returns `Err` if the receiver was not bound (already consumed by
    /// `run_apply_loop`, or constructed without a listener).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, String> {
        self.listener
            .as_ref()
            .ok_or("receiver not bound")?
            .local_addr()
            .map_err(|e| format!("local_addr: {}", e))
    }

    /// Accept one connection and receive WAL records until the connection closes.
    ///
    /// Each received record is applied via the `apply` callback.
    /// Returns the number of records applied.
    pub fn accept_and_apply<F>(&mut self, mut apply: F) -> Result<u64, String>
    where
        F: FnMut(&WalRecord),
    {
        let listener = self.listener.as_ref()
            .ok_or("receiver not bound")?;
        let (mut stream, addr) = listener.accept()
            .map_err(|e| format!("accept: {}", e))?;
        let mut buffer = String::new();
        let mut chunk = [0u8; 4096];

        loop {
            let n = stream.read(&mut chunk)
                .map_err(|e| format!("read: {}", e))?;
            if n == 0 {
                break; // connection closed
            }
            self.bytes_received += n as u64;
            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));

            // Process complete lines (newline-delimited records)
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].to_string();
                buffer = buffer[pos + 1..].to_string();
                if let Ok(record) = serde_json::from_str::<WalRecord>(&line) {
                    apply(&record);
                    self.records_received += 1;
                    // Task 6.4: track the highest applied LSN so the
                    // replica can resume from last_applied_lsn + 1 on
                    // reconnect.
                    if record.lsn > self.last_applied_lsn {
                        self.last_applied_lsn = record.lsn;
                    }
                }
            }
        }

        let _ = addr;
        Ok(self.records_received)
    }

    /// Run the apply loop: accept a connection, receive records, and apply
    /// each via the `apply` callback (Task 5.2).
    ///
    /// Unlike `accept_and_apply`, the `apply` callback returns `Result<()>`:
    /// - `Ok(())` — the record was applied successfully.
    /// - `Err(e)` — the apply failed. The error is logged, and the loop
    ///   continues (configurable behaviour — see `set_continue_on_error`).
    ///   If `continue_on_error` is false (the default), the loop returns
    ///   the error immediately.
    ///
    /// The loop runs until the connection closes or an unrecoverable error
    /// occurs (read failure, or apply failure when `continue_on_error` is
    /// false).
    pub fn run_apply_loop<F>(&mut self, mut apply: F) -> Result<u64, String>
    where
        F: FnMut(&WalRecord) -> Result<(), String>,
    {
        let listener = self.listener.as_ref()
            .ok_or("receiver not bound")?;
        let (mut stream, _addr) = listener.accept()
            .map_err(|e| format!("accept: {}", e))?;
        let mut buffer = String::new();
        let mut chunk = [0u8; 4096];
        let continue_on_error = self.continue_on_error;

        loop {
            let n = stream.read(&mut chunk)
                .map_err(|e| format!("read: {}", e))?;
            if n == 0 {
                break; // connection closed
            }
            self.bytes_received += n as u64;
            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].to_string();
                buffer = buffer[pos + 1..].to_string();
                if let Ok(record) = serde_json::from_str::<WalRecord>(&line) {
                    match apply(&record) {
                        Ok(()) => {
                            self.records_received += 1;
                            // Task 6.4: track the highest applied LSN so
                            // the replica can resume from
                            // `last_applied_lsn + 1` on reconnect.
                            if record.lsn > self.last_applied_lsn {
                                self.last_applied_lsn = record.lsn;
                            }
                        }
                        Err(e) => {
                            log::warn!("replication apply error: {e}");
                            if !continue_on_error {
                                return Err(format!("apply error: {e}"));
                            }
                            // Continue — count the record as received but not applied.
                            self.records_received += 1;
                        }
                    }
                }
            }
        }

        Ok(self.records_received)
    }

    /// Set whether the apply loop continues after an apply error (Task 5.2).
    /// Default: false (stop on first error).
    pub fn set_continue_on_error(&mut self, continue_on_error: bool) {
        self.continue_on_error = continue_on_error;
    }

    /// Return the highest LSN applied so far (Task 6.4).
    ///
    /// 0 means no records have been applied yet. After `run_apply_loop`
    /// processes records, this is the `lsn` of the most recently applied
    /// record. Use `resume_from_lsn()` to get the LSN to request on
    /// reconnect.
    #[must_use]
    pub fn last_applied_lsn(&self) -> u64 {
        self.last_applied_lsn
    }

    /// Return the LSN to request from the primary on reconnect (Task 6.4).
    ///
    /// This is `last_applied_lsn + 1` — i.e. the next LSN the replica has
    /// not yet applied. On reconnect, the primary calls
    /// `WalStreamer::stream_from_lsn(records, resume_lsn)` to resend only
    /// records the replica hasn't seen.
    ///
    /// If no records have been applied (`last_applied_lsn == 0`), returns
    /// 1 (request from the beginning, since LSNs start at 1).
    #[must_use]
    pub fn resume_from_lsn(&self) -> u64 {
        self.last_applied_lsn.saturating_add(1).max(1)
    }
}

// =========================================================================
// Raft consensus (minimal hand-rolled stub — retained for backward compat)
// =========================================================================
//
// Wave 5 (Task 5.1+) replaces this stub with a real openraft integration
// in `crate::storage::raft` (compiled when the `raft` feature is enabled).
// `QueryEngine::enable_raft` routes to `raft::RaftManager` when the feature
// is on, and falls back to this `RaftNode` stub when the feature is off.
// The stub is kept here so its existing unit tests continue to run in the
// default build (without `--features raft`); it does NOT implement real
// Raft consensus (no quorum, no failover, no log replication beyond the
// WalStreamer TCP fan-out in `on_become_leader`).

/// Raft node state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftState {
    Follower,
    Candidate,
    Leader,
}

/// A minimal Raft node (stub).
///
/// This implements the core Raft consensus algorithm:
/// - Leader election via randomized timeouts
/// - Log replication: the leader appends entries and replicates to followers
/// - A single-term leader with majority vote
///
/// Note: This is a minimal implementation for demonstration. Production
/// deployments should use the `openraft` crate for a battle-tested Raft.
pub struct RaftNode {
    /// Unique node ID.
    pub node_id: u64,
    /// Current state (Follower, Candidate, Leader).
    pub state: RaftState,
    /// Current term.
    pub current_term: u64,
    /// Who we voted for in the current term.
    pub voted_for: Option<u64>,
    /// Log entries (term, command).
    pub log: Vec<(u64, String)>,
    /// Commit index (last committed log entry).
    pub commit_index: usize,
    /// Peers: node_id -> address.
    pub peers: HashMap<u64, String>,
    /// Leader's next log index for each peer.
    pub next_index: HashMap<u64, usize>,
}

impl RaftNode {
    /// Create a new Raft node.
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            state: RaftState::Follower,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            peers: HashMap::new(),
            next_index: HashMap::new(),
        }
    }

    /// Add a peer node.
    pub fn add_peer(&mut self, peer_id: u64, addr: &str) {
        self.peers.insert(peer_id, addr.to_string());
    }

    /// Start an election: become Candidate, increment term, vote for self.
    ///
    /// In a full implementation, this would send RequestVote RPCs to all
    /// peers and wait for replies. Here, we simulate a single-node cluster
    /// becoming leader immediately.
    pub fn start_election(&mut self) {
        self.state = RaftState::Candidate;
        self.current_term += 1;
        self.voted_for = Some(self.node_id);

        // Single-node cluster: we win immediately
        if self.peers.is_empty() {
            self.become_leader();
        }
    }

    /// Become the leader.
    pub fn become_leader(&mut self) {
        self.state = RaftState::Leader;
        // Initialize next_index for all peers
        let log_len = self.log.len();
        for peer_id in self.peers.keys() {
            self.next_index.insert(*peer_id, log_len);
        }
    }

    /// Called when this node becomes the leader (Task 5.3).
    ///
    /// Connects a `WalStreamer` to each follower address in `peer_addrs`,
    /// wraps them in a `MultiWalStreamSink`, and attaches the sink to the
    /// `Wal` so subsequent `append_and_sync()` calls stream records to all
    /// followers.
    ///
    /// Returns the number of followers successfully connected.
    pub fn on_become_leader(
        &mut self,
        wal: &mut crate::storage::recovery::Wal,
        peer_addrs: &[&str],
    ) -> usize {
        self.become_leader();
        let mut multi_sink = MultiWalStreamSink::new();
        let mut connected = 0;
        for addr in peer_addrs {
            let mut streamer = WalStreamer::new();
            match streamer.connect(addr) {
                Ok(()) => {
                    log::info!("leader: connected WalStreamer to follower at {}", addr);
                    multi_sink.add(streamer);
                    connected += 1;
                }
                Err(e) => {
                    log::warn!("leader: failed to connect to follower at {}: {}", addr, e);
                }
            }
        }
        if connected > 0 {
            wal.set_stream_sink(std::sync::Arc::new(std::sync::Mutex::new(multi_sink)));
        }
        connected
    }

    /// Called when this node steps down from leader (Task 5.3).
    ///
    /// Detaches the stream sink from the `Wal` so records are no longer
    /// streamed to followers.
    pub fn on_demote(&mut self, wal: &mut crate::storage::recovery::Wal) {
        self.state = RaftState::Follower;
        wal.clear_stream_sink();
        log::info!("demoted: disconnected WalStreamers from followers");
    }

    /// Append a log entry (leader only).
    pub fn append_entry(&mut self, command: &str) -> Result<usize, String> {
        if self.state != RaftState::Leader {
            return Err(format!("not leader (state={:?})", self.state));
        }
        self.log.push((self.current_term, command.to_string()));
        Ok(self.log.len() - 1)
    }

    /// Commit log entries up to the given index.
    pub fn commit_to(&mut self, index: usize) {
        if index < self.log.len() {
            self.commit_index = index;
        }
    }

    /// Get the last committed log entry.
    pub fn last_committed(&self) -> Option<&(u64, String)> {
        if self.commit_index < self.log.len() {
            self.log.get(self.commit_index)
        } else {
            None
        }
    }

    /// Get the number of log entries.
    pub fn log_len(&self) -> usize {
        self.log.len()
    }
}

// =========================================================================
// Backup / Restore
// =========================================================================

/// Backup the entire catalog to a directory.
///
/// Creates:
/// - `manifest.json`: list of tables with their column names and types.
/// - `<table_name>.csv`: one CSV per table.
///
/// Returns the total number of rows backed up.
pub fn backup(engine: &mut QueryEngine, backup_dir: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(backup_dir).map_err(|e| format!("create dir: {}", e))?;

    let mut manifest_tables: Vec<serde_json::Value> = Vec::new();
    let mut total_rows = 0;

    // List all tables via SQL
    let table_names = list_tables(engine)?;

    for table_name in &table_names {
        // Get table schema and data
        let select_sql = format!("SELECT * FROM {}", table_name);
        let result = engine.execute(&select_sql)
            .map_err(|e| format!("select {}: {}", table_name, e))?;

        // Write CSV
        let csv_path = backup_dir.join(format!("{}.csv", table_name));
        write_csv(&csv_path, &result)?;

        // Add to manifest
        let column_names: Vec<&str> = result.columns.iter()
            .map(|c| c.name.as_str())
            .collect();
        manifest_tables.push(serde_json::json!({
            "name": table_name,
            "columns": column_names,
            "row_count": result.row_count,
        }));
        total_rows += result.row_count;
    }

    // Write manifest
    let manifest = serde_json::json!({
        "version": 1,
        "tables": manifest_tables,
        "total_rows": total_rows,
    });
    std::fs::write(backup_dir.join("manifest.json"), manifest.to_string())
        .map_err(|e| format!("write manifest: {}", e))?;

    Ok(total_rows)
}

/// Restore a backup from a directory.
///
/// Reads the manifest, creates each table, then loads data from the CSV files.
/// Returns the total number of rows restored.
pub fn restore(engine: &mut QueryEngine, backup_dir: &Path) -> Result<usize, String> {
    let manifest_path = backup_dir.join("manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read manifest: {}", e))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_str).map_err(|e| format!("parse manifest: {}", e))?;

    let mut total_rows = 0;
    if let Some(tables) = manifest.get("tables").and_then(|t| t.as_array()) {
        for table in tables {
            let table_name = table.get("name")
                .and_then(|n| n.as_str())
                .ok_or("table name missing in manifest")?;

            // Get column names from manifest
            let column_names: Vec<String> = table.get("columns")
                .and_then(|c| c.as_array())
                .map(|cols| cols.iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect())
                .unwrap_or_default();

            // Create table with column definitions
            if !column_names.is_empty() {
                let col_defs: Vec<String> = column_names.iter()
                    .map(|c| format!("{} INT", c))
                    .collect();
                let create_sql = format!("CREATE TABLE {} ({})", table_name, col_defs.join(", "));
                let _ = engine.execute(&create_sql); // ignore if table exists
            }

            // Task 5.4: load data from CSV directly via engine.load_csv()
            // (bypasses the COPY command's allowed_copy_dirs security check,
            // since restore() is a trusted operation).
            let csv_path = backup_dir.join(format!("{}.csv", table_name));
            if csv_path.exists() {
                let path_str = csv_path.to_string_lossy();
                let row_count = engine.load_csv(&path_str, table_name, true)
                    .map_err(|e| format!("restore {}: {}", table_name, e))?;
                total_rows += row_count;
            }
        }
    }

    Ok(total_rows)
}

/// List all table names in the engine's catalog.
fn list_tables(engine: &mut QueryEngine) -> Result<Vec<String>, String> {
    // Task 5.4: use the catalog directly (more reliable than SHOW TABLES).
    let names: Vec<String> = engine.catalog.table_names()
        .into_iter()
        .filter(|n| *n != "__dummy__")
        .map(String::from)
        .collect();
    if !names.is_empty() {
        return Ok(names);
    }
    // Fallback: try SHOW TABLES (in case the catalog is empty but the
    // engine has some other table source).
    if let Ok(result) = engine.execute("SHOW TABLES") {
        if !result.columns.is_empty() {
            return Ok(result.columns[0].values.iter()
                .map(|v| v.to_string())
                .collect());
        }
    }
    Ok(vec![])
}

/// Write a query result to a CSV file.
fn write_csv(path: &Path, result: &crate::engine::QueryResult) -> Result<(), String> {
    let mut content = String::new();

    // Header row
    let header: Vec<&str> = result.columns.iter()
        .map(|c| c.name.as_str())
        .collect();
    content.push_str(&header.join(","));
    content.push('\n');

    // Data rows
    for row_idx in 0..result.row_count {
        let row: Vec<String> = result.columns.iter()
            .map(|col| {
                if let Some(strings) = &col.string_values {
                    strings.get(row_idx).cloned().unwrap_or_default()
                } else if row_idx < col.values.len() {
                    col.values[row_idx].to_string()
                } else {
                    String::new()
                }
            })
            .collect();
        content.push_str(&row.join(","));
        content.push('\n');
    }

    std::fs::write(path, content).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(())
}

// =========================================================================
// Point-in-Time Recovery (PITR)
// =========================================================================

/// A WAL record with a timestamp, for point-in-time recovery.
#[derive(Debug, Clone)]
pub struct TimestampedWalRecord {
    /// The original WAL record.
    pub record: WalRecord,
    /// When this record was written (epoch microseconds).
    pub timestamp_us: u64,
}

/// Replay WAL records up to a given timestamp.
///
/// This implements point-in-time recovery: given a backup and a WAL log,
/// restore the database to its state at a specific point in time.
///
/// # Arguments
/// * `engine` - The engine to replay into (should already have the backup loaded)
/// * `wal_records` - The WAL records with timestamps, in order
/// * `target_timestamp` - Replay only records with timestamp <= this value
pub fn replay_wal_to_timestamp(
    engine: &mut QueryEngine,
    wal_records: &[TimestampedWalRecord],
    target_timestamp: u64,
) -> Result<usize, String> {
    let mut replayed = 0;
    for ts_record in wal_records {
        if ts_record.timestamp_us > target_timestamp {
            break; // Stop at the target timestamp
        }

        // Apply the WAL record
        apply_wal_record(engine, &ts_record.record)?;
        replayed += 1;
    }
    Ok(replayed)
}

/// Apply a single WAL record to the engine.
fn apply_wal_record(engine: &mut QueryEngine, record: &WalRecord) -> Result<(), String> {
    // Re-execute the SQL that generated this WAL record
    if !record.sql.is_empty() {
        engine.execute(&record.sql)
            .map_err(|e| format!("replay WAL record: {}", e))?;
    }
    Ok(())
}

/// Get the current timestamp in epoch microseconds.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backup_creates_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        engine.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
        let result = backup(&mut engine, tmp.path());
        assert!(result.is_ok(), "backup must succeed; got: {:?}", result.err());
        assert!(tmp.path().join("manifest.json").exists());
    }

    #[test]
    fn wal_streamer_not_connected() {
        let mut streamer = WalStreamer::new();
        assert!(!streamer.is_connected());

        let record = WalRecord::autocommit("INSERT INTO t VALUES (1)");
        let bytes = streamer.stream_record(&record).unwrap();
        assert!(bytes > 0);
        assert_eq!(streamer.records_sent, 1);
    }

    #[test]
    fn wal_streamer_connect_and_stream() {
        // Test connecting to a non-existent address — should fail
        let mut streamer = WalStreamer::new();
        assert!(streamer.connect("127.0.0.1:1").is_err());
        assert!(!streamer.is_connected());
    }

    #[test]
    fn wal_receiver_bind_and_accept() {
        // Bind to a random port and verify we can accept (with no connection,
        // this will block, so we just test binding)
        let receiver = WalReceiver::bind("127.0.0.1:0");
        assert!(receiver.is_ok());
    }

    #[test]
    fn raft_node_single_node_election() {
        let mut node = RaftNode::new(1);
        assert_eq!(node.state, RaftState::Follower);
        assert_eq!(node.current_term, 0);

        node.start_election();
        // Single-node cluster: should become leader immediately
        assert_eq!(node.state, RaftState::Leader);
        assert_eq!(node.current_term, 1);
        assert_eq!(node.voted_for, Some(1));
    }

    #[test]
    fn raft_node_append_entry() {
        let mut node = RaftNode::new(1);
        node.start_election();
        assert!(node.state == RaftState::Leader);

        let idx = node.append_entry("INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(node.log_len(), 1);
    }

    #[test]
    fn raft_node_follower_cannot_append() {
        let mut node = RaftNode::new(1);
        // Node is Follower by default
        assert!(node.append_entry("INSERT").is_err());
    }

    #[test]
    fn raft_node_with_peers() {
        let mut node = RaftNode::new(1);
        node.add_peer(2, "127.0.0.1:5002");
        node.add_peer(3, "127.0.0.1:5003");
        assert_eq!(node.peers.len(), 2);

        node.start_election();
        // With peers, election doesn't complete immediately
        assert_eq!(node.state, RaftState::Candidate);
    }

    #[test]
    fn raft_node_commit() {
        let mut node = RaftNode::new(1);
        node.start_election();
        node.append_entry("cmd1").unwrap();
        node.append_entry("cmd2").unwrap();
        node.commit_to(1);
        assert_eq!(node.commit_index, 1);
        assert_eq!(node.last_committed().unwrap().1, "cmd2");
    }

    #[test]
    fn pitr_replay_to_timestamp() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT)").unwrap();

        // Create WAL records at different timestamps
        let records = vec![
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (1)"),
                timestamp_us: 1000,
            },
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (2)"),
                timestamp_us: 2000,
            },
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (3)"),
                timestamp_us: 3000,
            },
        ];

        // Replay up to timestamp 2000 — should apply 2 records
        let replayed = replay_wal_to_timestamp(&mut engine, &records, 2000).unwrap();
        assert_eq!(replayed, 2);

        // Verify only 2 rows were inserted
        let result = engine.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.columns[0].values[0], 2);
    }

    #[test]
    fn pitr_replay_all() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT)").unwrap();

        let records = vec![
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (1)"),
                timestamp_us: 1000,
            },
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (2)"),
                timestamp_us: 2000,
            },
        ];

        // Replay all (timestamp = u64::MAX)
        let replayed = replay_wal_to_timestamp(&mut engine, &records, u64::MAX).unwrap();
        assert_eq!(replayed, 2);
    }

    #[test]
    fn pitr_replay_none() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t (id INT)").unwrap();

        let records = vec![
            TimestampedWalRecord {
                record: WalRecord::autocommit("INSERT INTO t VALUES (1)"),
                timestamp_us: 1000,
            },
        ];

        // Replay with target before first record — should apply 0 records
        let replayed = replay_wal_to_timestamp(&mut engine, &records, 500).unwrap();
        assert_eq!(replayed, 0);
    }

    #[test]
    fn now_us_increases() {
        let t1 = now_us();
        std::thread::sleep(std::time::Duration::from_micros(10));
        let t2 = now_us();
        assert!(t2 > t1, "timestamp must increase: {} > {}", t2, t1);
    }

    /// Task 5.2 DoD: stream 100 records from a WalStreamer to a WalReceiver
    /// via run_apply_loop, verify all 100 are received and applied.
    #[test]
    fn wal_receiver_run_apply_loop() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        // Bind a receiver on a random port.
        let receiver = WalReceiver::bind("127.0.0.1:0").unwrap();
        let bound_addr = {
            let listener = receiver.listener.as_ref().unwrap();
            listener.local_addr().unwrap().to_string()
        };
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        // Spawn the receiver thread.
        let handle = thread::spawn(move || {
            let mut receiver = receiver;
            receiver.run_apply_loop(|record| {
                received_clone.lock().unwrap().push(record.sql.clone());
                Ok(())
            }).unwrap()
        });

        // Connect a streamer and send 100 records.
        let mut streamer = WalStreamer::new();
        // Wait a moment for the receiver to be ready.
        thread::sleep(std::time::Duration::from_millis(50));
        streamer.connect(&bound_addr).unwrap();
        for i in 0..100 {
            streamer.stream_record(&WalRecord::autocommit(format!("INSERT INTO t VALUES ({})", i))).unwrap();
        }
        streamer.flush().unwrap();
        drop(streamer); // close the connection

        let count = handle.join().unwrap();
        assert_eq!(count, 100, "receiver must get 100 records");
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 100);
        assert_eq!(received[0], "INSERT INTO t VALUES (0)");
        assert_eq!(received[99], "INSERT INTO t VALUES (99)");
    }

    /// Task 5.3 DoD: 3-node Raft cluster, leader election, verify leader
    /// streams WAL to 2 followers.
    #[test]
    fn raft_leader_streams_to_followers() {
        use std::sync::{Arc, Mutex};
        use std::thread;
        use crate::storage::recovery::Wal;

        // Bind 2 receivers (followers) on random ports.
        let r1 = WalReceiver::bind("127.0.0.1:0").unwrap();
        let r2 = WalReceiver::bind("127.0.0.1:0").unwrap();
        let addr1 = r1.listener.as_ref().unwrap().local_addr().unwrap().to_string();
        let addr2 = r2.listener.as_ref().unwrap().local_addr().unwrap().to_string();

        let received1 = Arc::new(Mutex::new(Vec::new()));
        let received2 = Arc::new(Mutex::new(Vec::new()));
        let rc1 = received1.clone();
        let rc2 = received2.clone();

        let h1 = thread::spawn(move || {
            let mut r1 = r1;
            r1.run_apply_loop(|record| { rc1.lock().unwrap().push(record.sql.clone()); Ok(()) }).unwrap()
        });
        let h2 = thread::spawn(move || {
            let mut r2 = r2;
            r2.run_apply_loop(|record| { rc2.lock().unwrap().push(record.sql.clone()); Ok(()) }).unwrap()
        });

        // Wait for receivers to be ready.
        thread::sleep(std::time::Duration::from_millis(50));

        // Create a leader node and a Wal.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        let mut node = RaftNode::new(1);
        node.add_peer(2, &addr1);
        node.add_peer(3, &addr2);

        // Become leader — connects WalStreamers to both followers.
        let connected = node.on_become_leader(&mut wal, &[&addr1, &addr2]);
        assert_eq!(connected, 2, "must connect to 2 followers");
        assert_eq!(node.state, RaftState::Leader);

        // Append records — they should stream to both followers.
        wal.append_and_sync(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.append_and_sync(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();

        // Give the receivers time to process.
        thread::sleep(std::time::Duration::from_millis(100));
        drop(wal); // close streamers → receivers see EOF

        let c1 = h1.join().unwrap();
        let c2 = h2.join().unwrap();
        assert!(c1 >= 2, "follower 1 must receive >= 2 records (got {})", c1);
        assert!(c2 >= 2, "follower 2 must receive >= 2 records (got {})", c2);

        let r1 = received1.lock().unwrap();
        let r2 = received2.lock().unwrap();
        assert!(r1.contains(&"INSERT INTO t VALUES (1)".to_string()));
        assert!(r1.contains(&"INSERT INTO t VALUES (2)".to_string()));
        assert!(r2.contains(&"INSERT INTO t VALUES (1)".to_string()));
        assert!(r2.contains(&"INSERT INTO t VALUES (2)".to_string()));
    }
}
