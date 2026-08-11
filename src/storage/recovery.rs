//! # Durability: WAL replay + checkpoint (Wave 14).
//!
//! Implements a write-ahead log (WAL) for DML operations and a checkpoint
//! mechanism that flushes the in-memory catalog to a persistent format.
//! On restart, the WAL is replayed to restore the catalog to its last
//! committed state.
//!
//! The WAL format is: `txn_id|commit|rollback|base64(sql)\n`. The SQL
//! payload is base64-encoded (Wave 51 fix) so that pipe characters and
//! newlines inside SQL strings round-trip unambiguously — the previous
//! `\\|` / `\\n` escaping scheme was ambiguous because a SQL string
//! containing the literal bytes `\n` could not be distinguished from a
//! real newline on replay.
//!
//! Three special records carry transaction boundaries (Wave 51 fix):
//! - `BEGIN` records mark the start of a transaction (txn_id != 0).
//! - `COMMIT` records (`is_commit = true`) make the transaction durable.
//! - `ROLLBACK` records (`is_rollback = true`) discard the transaction.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use base64::{engine::general_purpose, Engine as _};

/// A WAL record: one DML operation or a transaction boundary marker.
///
/// Wave 63: added page-level physical records (PageInsert, PageUpdate,
/// PageDelete) that record the actual byte-level changes to pages, not
/// just the SQL string. This enables page-level crash recovery: on restart,
/// the WAL is replayed page-by-page rather than re-executing SQL.
///
/// Wave 71: added Serialize/Deserialize for streaming over TCP (replication).
///
/// Task 1.3: added `lsn` (log sequence number) — a monotonic 1-up counter
/// assigned by `Wal::append()`. The checkpoint records the last LSN it
/// includes; on replay, records with `lsn <= checkpoint_last_lsn` are
/// skipped, making replay idempotent even if the WAL wasn't truncated
/// (defence in depth against crash-between-rename-and-truncate).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalRecord {
    /// Log sequence number — monotonic across the WAL's lifetime.
    /// Assigned by `Wal::append()`. Zero for records constructed in-memory
    /// before being appended (the Wal overwrites this on append). On
    /// `read_all()`, the LSN is populated from disk.
    #[serde(default)]
    pub lsn: u64,
    /// Transaction ID (0 for autocommit, non-zero for explicit transactions).
    pub txn_id: u64,
    /// The SQL statement. Empty for BEGIN/COMMIT/ROLLBACK markers and for
    /// page-level physical records (which use `physical_change` instead).
    pub sql: String,
    /// True if this is a commit marker for the transaction.
    pub is_commit: bool,
    /// True if this is a rollback marker.
    pub is_rollback: bool,
    /// Optional page-level physical change. When present, this record
    /// represents a physical page modification (not a SQL statement).
    pub physical_change: Option<PhysicalChange>,
}

/// A physical page-level change recorded in the WAL.
/// Used for page-level crash recovery (Wave 63).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum PhysicalChange {
    /// A new page was allocated for a table.
    PageAlloc { table_id: u64, page_num: u32 },
    /// A cell in a page was updated.
    CellUpdate { table_id: u64, page_num: u32, cell_index: usize, old_value: u64, new_value: u64 },
    /// A row was inserted (appended to a page).
    RowInsert { table_id: u64, page_num: u32, row_offset: usize, values: Vec<u64> },
    /// A row was deleted.
    RowDelete { table_id: u64, page_num: u32, row_offset: usize },
}

impl WalRecord {
    /// Construct an autocommit DML record (txn_id = 0).
    pub fn autocommit(sql: impl Into<String>) -> Self {
        Self {
            lsn: 0,
            txn_id: 0,
            sql: sql.into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        }
    }

    /// Construct a BEGIN marker for the given transaction ID.
    pub fn begin(txn_id: u64) -> Self {
        Self {
            lsn: 0,
            txn_id,
            sql: String::new(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        }
    }

    /// Construct a COMMIT marker for the given transaction ID.
    pub fn commit(txn_id: u64) -> Self {
        Self {
            lsn: 0,
            txn_id,
            sql: String::new(),
            is_commit: true,
            is_rollback: false,
            physical_change: None,
        }
    }

    /// Construct a ROLLBACK marker for the given transaction ID.
    pub fn rollback(txn_id: u64) -> Self {
        Self {
            lsn: 0,
            txn_id,
            sql: String::new(),
            is_commit: false,
            is_rollback: true,
            physical_change: None,
        }
    }

    /// Construct a DML record inside an explicit transaction.
    pub fn txn_dml(txn_id: u64, sql: impl Into<String>) -> Self {
        Self {
            lsn: 0,
            txn_id,
            sql: sql.into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        }
    }

    /// Construct a page-level physical change record (Wave 63).
    pub fn physical(txn_id: u64, change: PhysicalChange) -> Self {
        Self {
            lsn: 0,
            txn_id,
            sql: String::new(),
            is_commit: false,
            is_rollback: false,
            physical_change: Some(change),
        }
    }
}

/// The WAL: appends records to a file and provides a reader for replay.
pub struct Wal {
    path: String,
    file: Option<File>,
    /// Monotonic LSN counter. The next record appended gets this LSN.
    /// On `open()`, initialised by scanning the existing WAL for the max
    /// LSN. On `truncate()`, preserved (NOT reset) so that post-checkpoint
    /// records get LSNs strictly greater than the checkpoint's `last_lsn`.
    /// `advance_lsn_to()` lets the engine bump this past the checkpoint's
    /// `last_lsn` when the WAL was truncated to empty.
    next_lsn: u64,
}

impl Wal {
    /// Open (or create) a WAL at the given path.
    ///
    /// Task 1.3: scans the existing WAL to find the max LSN, so newly
    /// appended records continue the LSN sequence monotonically.
    pub fn open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new().create(true).append(true).read(true).open(&path)?;
        let mut wal = Wal { path: path_str, file: Some(file), next_lsn: 1 };
        // Scan existing records to find the max LSN.
        if let Ok(records) = wal.read_all() {
            let max_lsn = records.iter().map(|r| r.lsn).max().unwrap_or(0);
            wal.next_lsn = max_lsn + 1;
        }
        Ok(wal)
    }

    /// Return the last assigned LSN (0 if no records have been appended).
    /// The checkpoint stores this as `last_lsn` so replay can skip
    /// records already included in the checkpoint.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn.saturating_sub(1)
    }

    /// Bump `next_lsn` to at least `lsn + 1`, so the next appended record
    /// gets an LSN strictly greater than `lsn`. Called by the engine after
    /// loading a checkpoint whose `last_lsn = lsn`, ensuring post-checkpoint
    /// records are NOT skipped by the LSN filter on replay.
    pub fn advance_lsn_to(&mut self, lsn: u64) {
        let needed = lsn.saturating_add(1);
        if self.next_lsn < needed {
            self.next_lsn = needed;
        }
    }

    /// Append a record to the WAL. Does NOT fsync — call `sync()` to
    /// durably persist.
    ///
    /// Wave 51 fix (Bug 10): the SQL payload is base64-encoded so that
    /// pipe characters, newlines, and backslashes inside SQL strings
    /// round-trip unambiguously.
    ///
    /// Wave 2 fix: an xxh3_64 checksum is appended as the last field so
    /// that torn writes and bit-flips are detectable on replay. The
    /// checksum is computed over the entire line (excluding the checksum
    /// field and the trailing newline).
    ///
    /// Task 1.3: the LSN is written as the FIRST field. The in-memory
    /// `record.lsn` is ignored — the Wal assigns its own monotonic LSN
    /// from `next_lsn`. On `read_all()`, the LSN is populated from disk.
    pub fn append(&mut self, record: &WalRecord) -> std::io::Result<()> {
        if let Some(ref mut file) = self.file {
            // Assign the LSN from the Wal's monotonic counter.
            let lsn = self.next_lsn;
            self.next_lsn += 1;
            // Format: lsn|txn_id|commit|rollback|base64(sql)|physical_json|xxh3\n
            let sql_b64 = general_purpose::STANDARD.encode(record.sql.as_bytes());
            let physical_json = match &record.physical_change {
                Some(change) => serde_json::to_string(change).unwrap_or_default(),
                None => String::new(),
            };
            let data_line = format!(
                "{}|{}|{}|{}|{}|{}",
                lsn,
                record.txn_id,
                if record.is_commit { 1 } else { 0 },
                if record.is_rollback { 1 } else { 0 },
                sql_b64,
                physical_json,
            );
            // Compute xxh3_64 checksum over the data line (excluding checksum + newline).
            let checksum = xxhash_rust::xxh3::xxh3_64(data_line.as_bytes());
            let line = format!("{data_line}|{checksum:016x}\n");
            file.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// Fsync the WAL file to durably persist all appended records.
    pub fn sync(&mut self) -> std::io::Result<()> {
        if let Some(ref mut file) = self.file {
            file.sync_all()?;
        }
        Ok(())
    }

    /// Read all records from the WAL (for replay on startup).
    ///
    /// Wave 51 fix: the SQL field is base64-decoded. Records that fail to
    /// decode are skipped (with a warning logged).
    ///
    /// Wave 2 fix: the last field is an xxh3_64 checksum. Records whose
    /// checksum does not match are skipped with a warning (torn write /
    /// bit-flip detection). Legacy records without a checksum field are
    /// accepted without verification for backward compatibility.
    ///
    /// Task 1.3: the first field is the LSN. New format (6 data fields +
    /// checksum) is `lsn|txn_id|commit|rollback|base64(sql)|physical_json|xxh3`.
    /// Legacy format (5 data fields + checksum) is
    /// `txn_id|commit|rollback|base64(sql)|physical_json|xxh3` — parsed
    /// with `lsn = 0`.
    pub fn read_all(&self) -> std::io::Result<Vec<WalRecord>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // Wave 2: try to split off the checksum (last `|<hex>` field).
            // If the last field is a 16-char hex string, treat it as a checksum.
            let (data_line, checksum_opt) = match line.rsplit_once('|') {
                Some((data, last_field))
                    if last_field.len() == 16
                        && last_field.chars().all(|c| c.is_ascii_hexdigit()) =>
                {
                    (data.to_string(), Some(last_field.to_string()))
                }
                _ => (line.clone(), None), // Legacy record without checksum.
            };
            // Verify checksum if present.
            if let Some(ref expected_hex) = checksum_opt {
                let expected = u64::from_str_radix(expected_hex, 16).unwrap_or(0);
                let actual = xxhash_rust::xxh3::xxh3_64(data_line.as_bytes());
                if expected != actual {
                    log::warn!(
                        "WAL checksum mismatch: expected {expected:016x}, got {actual:016x}, skipping record"
                    );
                    continue;
                }
            }
            // Parse the data line (now without the checksum field).
            // Task 1.3: try the new 6-field format (lsn first) first.
            let parts: Vec<&str> = data_line.splitn(6, '|').collect();
            if parts.len() == 6 {
                // New format: lsn|txn_id|commit|rollback|base64(sql)|physical_json
                let lsn: u64 = parts[0].parse().unwrap_or(0);
                let txn_id: u64 = parts[1].parse().unwrap_or(0);
                let is_commit = parts[2] == "1";
                let is_rollback = parts[3] == "1";
                let sql = match general_purpose::STANDARD.decode(parts[4].as_bytes()) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(_) => decode_legacy_sql(parts[4]),
                };
                let physical_change = if !parts[5].is_empty() {
                    serde_json::from_str::<PhysicalChange>(parts[5]).ok()
                } else {
                    None
                };
                records.push(WalRecord { lsn, txn_id, sql, is_commit, is_rollback, physical_change });
                continue;
            }
            // Fall back to the legacy 5-field format (no LSN).
            let parts: Vec<&str> = data_line.splitn(5, '|').collect();
            if parts.len() < 4 {
                // Legacy record format (pre-Wave-51) — try the old escaping.
                if parts.len() >= 1 {
                    if let Ok(rec) = parse_legacy_record(&data_line) {
                        records.push(rec);
                    }
                }
                continue;
            }
            let txn_id: u64 = parts[0].parse().unwrap_or(0);
            let is_commit = parts[1] == "1";
            let is_rollback = parts[2] == "1";
            let sql = match general_purpose::STANDARD.decode(parts[3].as_bytes()) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => decode_legacy_sql(parts[3]),
            };
            let physical_change = if parts.len() >= 5 && !parts[4].is_empty() {
                serde_json::from_str::<PhysicalChange>(parts[4]).ok()
            } else {
                None
            };
            records.push(WalRecord { lsn: 0, txn_id, sql, is_commit, is_rollback, physical_change });
        }
        Ok(records)
    }

    /// Truncate the WAL (after a successful checkpoint).
    ///
    /// Task 1.3: does NOT reset `next_lsn` — LSNs must remain monotonic
    /// across the checkpoint boundary so that post-checkpoint records get
    /// LSNs strictly greater than the checkpoint's `last_lsn` (otherwise
    /// they'd be skipped by the LSN filter on replay).
    pub fn truncate(&mut self) -> std::io::Result<()> {
        // Close the current file, truncate it, and reopen for append.
        self.file = None;
        // First truncate: open with write+truncate.
        {
            let _ = OpenOptions::new().create(true).write(true).truncate(true).open(&self.path)?;
        }
        // Then reopen for append+read.
        let file = OpenOptions::new().create(true).append(true).read(true).open(&self.path)?;
        self.file = Some(file);
        // Intentionally do NOT reset self.next_lsn.
        Ok(())
    }

    /// Close the WAL.
    pub fn close(&mut self) {
        self.file = None;
    }
}

/// Decode a legacy (pre-Wave-51) SQL field that used the ambiguous
/// `\\|` / `\\n` escaping. Kept so old WAL files still replay after
/// the upgrade.
fn decode_legacy_sql(s: &str) -> String {
    s.replace("\\|", "|").replace("\\n", "\n")
}

/// Parse a legacy WAL line that doesn't have the 4-field base64 format.
fn parse_legacy_record(line: &str) -> std::io::Result<WalRecord> {
    // Best-effort: split on `|` and assume the last field is the SQL with
    // legacy escaping. This handles the pre-Wave-51 format.
    let parts: Vec<&str> = line.splitn(4, '|').collect();
    if parts.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty WAL line"));
    }
    let txn_id: u64 = parts[0].parse().unwrap_or(0);
    let is_commit = parts.get(1).map(|s| *s == "1").unwrap_or(false);
    let is_rollback = parts.get(2).map(|s| *s == "1").unwrap_or(false);
    let sql = parts.get(3).map(|s| decode_legacy_sql(s)).unwrap_or_default();
    Ok(WalRecord { lsn: 0, txn_id, sql, is_commit, is_rollback, physical_change: None })
}

/// Replay WAL records against an engine. Only committed transactions
/// are replayed; rolled-back or incomplete transactions are skipped.
pub fn replay_wal(
    engine: &mut crate::engine::QueryEngine,
    wal: &Wal,
) -> std::io::Result<ReplayStats> {
    let records = wal.read_all()?;
    let mut stats = ReplayStats::default();

    // Group records by transaction. txn_id = 0 means autocommit.
    let mut txn_records: HashMap<u64, Vec<&WalRecord>> = HashMap::new();
    let mut autocommit_records: Vec<&WalRecord> = Vec::new();

    for record in &records {
        if record.txn_id == 0 {
            autocommit_records.push(record);
        } else {
            txn_records.entry(record.txn_id).or_default().push(record);
        }
    }

    // Replay autocommit records (each is its own transaction).
    for record in &autocommit_records {
        // Wave 51: skip marker records (BEGIN/COMMIT/ROLLBACK with empty
        // SQL). Autocommit records never have markers, but defensive.
        if record.is_commit || record.is_rollback || record.sql.trim().is_empty() {
            continue;
        }
        match engine.execute(&record.sql) {
            Ok(_) => stats.replayed += 1,
            Err(e) => {
                stats.errors += 1;
                stats.error_messages.push(format!("replay error: {e}"));
            }
        }
    }

    // Replay transactional records: only committed ones.
    for (txn_id, records) in &txn_records {
        // Check if this transaction was committed.
        let committed = records.iter().any(|r| r.is_commit);
        let rolled_back = records.iter().any(|r| r.is_rollback);

        if !committed || rolled_back {
            stats.skipped += 1;
            continue;
        }

        // Begin a transaction, replay the records, commit.
        let _ = engine.execute("BEGIN");
        for record in records {
            // Wave 51: skip BEGIN/COMMIT/ROLLBACK markers (they have
            // empty SQL or are flagged). Also skip empty SQL defensively.
            if record.is_commit || record.is_rollback || record.sql.trim().is_empty() {
                continue;
            }
            match engine.execute(&record.sql) {
                Ok(_) => stats.replayed += 1,
                Err(e) => {
                    stats.errors += 1;
                    stats.error_messages.push(format!("replay error: {e}"));
                }
            }
        }
        let _ = engine.execute("COMMIT");
    }

    Ok(stats)
}

/// Statistics from a WAL replay.
#[derive(Debug, Default)]
pub struct ReplayStats {
    /// Number of records successfully replayed.
    pub replayed: usize,
    /// Number of transactions skipped (uncommitted or rolled back).
    pub skipped: usize,
    /// Number of replay errors.
    pub errors: usize,
    /// Error messages (first few).
    pub error_messages: Vec<String>,
}

/// A checkpoint: flushes the catalog's DDL + data to a SQL file that
/// can be re-executed on restart to restore the state without replaying
/// the full WAL.
pub struct Checkpoint;

impl Checkpoint {
    /// Save a checkpoint AND truncate the WAL atomically (Task 1.1).
    ///
    /// This is the canonical "checkpoint" operation: after it returns,
    /// the checkpoint file at `path` contains the full catalog state,
    /// and the WAL at `wal` is empty (zero length). On restart, the
    /// engine loads the checkpoint first, then replays an empty WAL —
    /// no duplicate rows.
    ///
    /// # Arguments
    /// * `catalog` - The catalog to checkpoint.
    /// * `path` - The checkpoint file path (e.g. `<data_dir>/checkpoint.sql`).
    /// * `wal` - The WAL to truncate after the checkpoint is written.
    ///
    /// # Errors
    /// Returns an error if either the checkpoint write or the WAL
    /// truncation fails. If the checkpoint write fails, the WAL is NOT
    /// truncated (so no data is lost). If the truncation fails after a
    /// successful checkpoint write, the checkpoint is still valid — the
    /// next restart will load the checkpoint and replay the (non-empty)
    /// WAL, which may produce duplicate rows; Task 1.3's idempotent
    /// replay defends against this case.
    pub fn save_and_truncate(
        catalog: &crate::catalog::Catalog,
        path: &Path,
        wal: &mut Wal,
    ) -> std::io::Result<usize> {
        // Task 1.2: atomic checkpoint swap. Write to `<path>.tmp`, fsync it,
        // then rename to `<path>`. Only after the rename succeeds do we
        // truncate the WAL. If the process crashes between the rename and
        // the WAL truncation, the next restart loads the fresh checkpoint
        // (which already contains all the data) and replays the
        // non-truncated WAL — Task 1.3's idempotent replay (LSN-based)
        // ensures those WAL records are skipped, so no duplicates.
        let tmp_path = path.with_extension("sql.tmp");
        let n = Self::save(catalog, &tmp_path)?;
        // fsync the tmp file so the checkpoint bytes are durable on disk
        // before we commit it via rename. Without this, a crash after
        // rename but before the OS flushes the tmp file's data could
        // leave the checkpoint file present but empty/corrupt.
        {
            let tmp_file = std::fs::File::open(&tmp_path)?;
            tmp_file.sync_all()?;
        }
        // Atomic rename: on POSIX, rename(2) is atomic — the checkpoint
        // file appears either with the old content or the new content,
        // never partially written. On Windows, ReplaceFile/rename behaves
        // similarly for same-filesystem renames.
        std::fs::rename(&tmp_path, path)?;
        // Task 1.3: record the WAL's current LSN in a sidecar file
        // `<path>.lsn` (e.g. checkpoint.sql.lsn). On replay, records with
        // lsn <= this value are skipped (they're already in the checkpoint).
        // The sidecar is written AFTER the rename and BEFORE the WAL
        // truncation, so if we crash here, the checkpoint is valid, the
        // sidecar may or may not exist, and the WAL still has records —
        // replay handles all three cases (no sidecar ⇒ no filtering).
        let last_lsn = wal.current_lsn();
        let lsn_path = path.with_extension("sql.lsn");
        std::fs::write(&lsn_path, last_lsn.to_string())?;
        // Now that the checkpoint is durable, truncate the WAL. If this
        // fails (e.g. disk full), the checkpoint is still valid — the
        // next restart loads it and replays the WAL. Task 1.3's
        // idempotent replay ensures no duplicates even if the WAL wasn't
        // truncated.
        wal.truncate()?;
        log::debug!(
            "checkpoint: wrote {n} tables to {} (atomic swap), last_lsn={last_lsn}, WAL truncated",
            path.display()
        );
        Ok(n)
    }

    /// Read the `last_lsn` recorded by `save_and_truncate()` from the
    /// sidecar file `<path>.lsn`. Returns `None` if the sidecar doesn't
    /// exist (e.g. checkpoint written by a pre-Task-1.3 version, or the
    /// sidecar wasn't written due to a crash between rename and sidecar
    /// write). In that case, replay does no LSN filtering — all records
    /// are replayed (the pre-Task-1.3 behaviour).
    pub fn read_last_lsn<P: AsRef<Path>>(path: P) -> Option<u64> {
        let lsn_path = path.as_ref().with_extension("sql.lsn");
        let content = std::fs::read_to_string(&lsn_path).ok()?;
        content.trim().parse::<u64>().ok()
    }

    /// Save a checkpoint of the current catalog state to a SQL file.
    /// The file contains:
    /// 1. CREATE TABLE statements for every table (with correct column
    ///    types from `table.schema` when available — Wave 50 fix).
    /// 2. INSERT statements for every row, with values formatted
    ///    according to their column type:
    ///      - String columns (with a `StringSearchColumn` sidecar) emit
    ///        the original string as a quoted literal with `'` doubled.
    ///      - Float columns emit `f64::from_bits(value)`.
    ///      - NULL cells (per `null_bitmaps`) emit the literal `NULL`.
    ///      - Everything else emits the raw u64.
    ///
    /// Wave 50 fix (Bug 7): previously every column was hardcoded as
    /// `INT` and every value was written as the raw u64 cell — so
    /// FLOAT and VARCHAR data was destroyed on checkpoint/restart.
    pub fn save<P: AsRef<Path>>(
        catalog: &crate::catalog::Catalog,
        path: P,
    ) -> std::io::Result<usize> {
        use crate::sql::ddl::ColumnType;
        let mut file = File::create(path)?;
        let mut table_count = 0;
        for name in catalog.table_names() {
            if name == "__dummy__" {
                continue;
            }
            if let Some(table) = catalog.get(name) {
                // Resolve column types: prefer `table.schema`, fall back to INT.
                let col_types: Vec<ColumnType> = if let Some(ref schema) = table.schema {
                    table
                        .column_names
                        .iter()
                        .enumerate()
                        .map(|(i, _)| schema.col_type_at(i).cloned().unwrap_or(ColumnType::BigInt))
                        .collect()
                } else {
                    // No schema — infer from sidecars.
                    table
                        .column_names
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            if i < table.string_columns.len() && table.string_columns[i].is_some() {
                                ColumnType::Varchar(None)
                            } else {
                                ColumnType::BigInt
                            }
                        })
                        .collect()
                };

                // Write CREATE TABLE with the correct types.
                let cols: Vec<String> = table
                    .column_names
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let ty = col_types[i].type_name();
                        // Emit VARCHAR(n) when a length was specified.
                        match &col_types[i] {
                            ColumnType::Varchar(Some(n)) => format!("{c} VARCHAR({n})"),
                            ColumnType::Nvarchar(Some(n)) => format!("{c} NVARCHAR({n})"),
                            ColumnType::Decimal(Some(p), Some(s)) => {
                                format!("{c} DECIMAL({p},{s})")
                            }
                            ColumnType::Decimal(Some(p), None) => format!("{c} DECIMAL({p})"),
                            ColumnType::Numeric(Some(p), Some(s)) => {
                                format!("{c} NUMERIC({p},{s})")
                            }
                            ColumnType::Numeric(Some(p), None) => format!("{c} NUMERIC({p})"),
                            _ => format!("{c} {ty}"),
                        }
                    })
                    .collect();
                writeln!(file, "CREATE TABLE {name} ({});", cols.join(", "))?;
                table_count += 1;

                // Write INSERT statements. NULL cells become the literal
                // `NULL`, not a 0 u64.
                for row in 0..table.row_count {
                    let vals: Vec<String> = table
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(col_idx, col)| {
                            // Check NULL bitmap first.
                            if col_idx < table.null_bitmaps.len() {
                                if let Some(ref bm) = table.null_bitmaps[col_idx] {
                                    if bm.is_null(row) {
                                        return "NULL".to_string();
                                    }
                                }
                            }
                            let cell = col.get(row).copied().unwrap_or(0);
                            // String column with sidecar: emit the original string.
                            if col_idx < table.string_columns.len() {
                                if let Some(ref sc) = table.string_columns[col_idx] {
                                    if row < sc.len() {
                                        let s = sc.get(row);
                                        // Double single quotes to escape them.
                                        let escaped = s.replace('\'', "''");
                                        return format!("'{escaped}'");
                                    }
                                }
                            }
                            // Float column: emit the decoded f64.
                            if matches!(
                                col_types[col_idx],
                                ColumnType::Float
                                    | ColumnType::Real
                                    | ColumnType::Decimal(_, _)
                                    | ColumnType::Numeric(_, _)
                            ) {
                                let f = f64::from_bits(cell);
                                if f.is_finite() {
                                    return format!("{f}");
                                }
                            }
                            // Default: raw u64.
                            cell.to_string()
                        })
                        .collect();
                    writeln!(file, "INSERT INTO {name} VALUES ({});", vals.join(", "))?;
                }
            }
        }
        Ok(table_count)
    }

    /// Load a checkpoint file and replay its SQL statements against the
    /// engine (Wave 5 — A4 data-loss bug fix).
    ///
    /// This reads the `checkpoint.sql` file written by [`Checkpoint::save`]
    /// and executes each statement via `engine.execute()`. This restores
    /// the catalog to the state at checkpoint time, after which WAL replay
    /// applies any records written after the checkpoint.
    ///
    /// **Task 1.1 fix:** The WAL is temporarily taken out of the engine
    /// during checkpoint load so that the checkpoint statements are NOT
    /// re-written to the WAL. Without this, loading a 10-row checkpoint
    /// would append 10 INSERT records to the WAL, and the subsequent WAL
    /// replay would re-execute them — producing 20 rows on restart (the
    /// data-corruption bug). By loading the checkpoint in "no-WAL" mode,
    /// we ensure the WAL stays empty after a checkpoint+restart cycle.
    ///
    /// Returns the number of SQL statements executed.
    pub fn load<P: AsRef<Path>>(
        engine: &mut crate::engine::QueryEngine,
        path: P,
    ) -> std::io::Result<usize> {
        let path = path.as_ref();
        if !path.exists() {
            log::debug!("checkpoint: no file at {}, skipping load", path.display());
            return Ok(0);
        }
        let content = std::fs::read_to_string(path)?;
        let mut count = 0;
        // Take the WAL out of the engine so that the checkpoint statements
        // (CREATE TABLE / INSERT) are NOT appended to the WAL during load.
        // This is the critical fix for the duplicate-row data-corruption
        // bug: without it, each checkpoint load re-populates the WAL, and
        // the subsequent replay duplicates every row.
        let saved_wal = engine.wal.take();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("--") {
                continue;
            }
            // Each line is a complete SQL statement (CREATE TABLE or INSERT).
            match engine.execute(line) {
                Ok(_) => count += 1,
                Err(e) => {
                    log::warn!("checkpoint: error replaying '{}': {}", line, e);
                }
            }
        }
        // Restore the WAL so subsequent DML writes are durable.
        engine.wal = saved_wal;
        log::info!("checkpoint: loaded {count} statements from {}", path.display());
        Ok(count)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn wal_append_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 1,
            sql: "INSERT INTO t VALUES (2)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 1,
            sql: "".into(),
            is_commit: true,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].sql, "INSERT INTO t VALUES (1)");
        assert_eq!(records[1].txn_id, 1);
        assert!(records[2].is_commit);
    }

    #[test]
    fn wal_truncate() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 1);

        wal.truncate().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 0);
    }

    #[test]
    fn wal_special_chars() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "INSERT INTO t VALUES ('a|b\nc')".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql, "INSERT INTO t VALUES ('a|b\nc')");
    }

    #[test]
    fn replay_autocommit() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "INSERT INTO t VALUES (2)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::in_memory();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 3);
        assert_eq!(stats.errors, 0);

        // Verify the data was restored.
        let r = engine.execute("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.scalar_u64(), Some(2));
    }

    #[test]
    fn replay_skips_uncommitted() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        // Transaction 1: INSERT but no COMMIT.
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 1,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::in_memory();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 1); // Only the CREATE TABLE
        assert_eq!(stats.skipped, 1); // Transaction 1 was not committed
    }

    #[test]
    fn replay_skips_rolled_back() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        // Transaction 1: INSERT + ROLLBACK.
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 1,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
            physical_change: None,
        })
        .unwrap();
        wal.append(&WalRecord {
        lsn: 0,
            txn_id: 1,
            sql: "".into(),
            is_commit: false,
            is_rollback: true,
            physical_change: None,
        })
        .unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::in_memory();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 1); // Only the CREATE TABLE
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn checkpoint_save() {
        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        let t = DS::from_loaded(LoadedTable {
            name: "users".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2, 3],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 3,
        });
        cat.register(t);

        let tmp = NamedTempFile::new().unwrap();
        let count = Checkpoint::save(&cat, tmp.path()).unwrap();
        assert_eq!(count, 1);

        // Read the file and verify it has CREATE TABLE and INSERT statements.
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(content.contains("CREATE TABLE users"));
        assert!(content.contains("INSERT INTO users VALUES (1)"));
        assert!(content.contains("INSERT INTO users VALUES (2)"));
        assert!(content.contains("INSERT INTO users VALUES (3)"));
    }

    /// Task 1.1 DoD: save_and_truncate writes the checkpoint AND empties the WAL.
    #[test]
    fn checkpoint_save_and_truncate_empties_wal() {
        let wal_tmp = NamedTempFile::new().unwrap();
        let ckpt_tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(wal_tmp.path()).unwrap();
        // Append a few records to the WAL.
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 2);

        // Build a minimal catalog.
        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        cat.register(DS::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2],
                row_count: 2,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 2,
        }));

        let n = Checkpoint::save_and_truncate(&cat, ckpt_tmp.path(), &mut wal).unwrap();
        assert_eq!(n, 1, "one table checkpointed");
        assert_eq!(wal.read_all().unwrap().len(), 0, "WAL must be empty after checkpoint");
        assert!(ckpt_tmp.path().exists(), "checkpoint file must exist");
    }

    /// Task 1.2 DoD: atomic checkpoint swap leaves no .tmp file behind,
    /// and the checkpoint file is fully written (not partially).
    #[test]
    fn checkpoint_atomic_swap_no_tmp_left_behind() {
        let wal_tmp = NamedTempFile::new().unwrap();
        let ckpt_dir = tempfile::TempDir::new().unwrap();
        let ckpt_path = ckpt_dir.path().join("checkpoint.sql");
        let tmp_path = ckpt_path.with_extension("sql.tmp");

        let mut wal = Wal::open(wal_tmp.path()).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.sync().unwrap();

        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        cat.register(DS::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2, 3],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 3,
        }));

        let n = Checkpoint::save_and_truncate(&cat, &ckpt_path, &mut wal).unwrap();
        assert_eq!(n, 1);
        // The .tmp file must have been renamed away.
        assert!(!tmp_path.exists(), "tmp file must not remain after atomic swap");
        // The checkpoint file must exist and be non-empty.
        assert!(ckpt_path.exists(), "checkpoint file must exist");
        let content = std::fs::read_to_string(&ckpt_path).unwrap();
        assert!(!content.is_empty(), "checkpoint must not be empty");
        assert!(content.contains("CREATE TABLE t"));
        assert!(content.contains("INSERT INTO t VALUES (1)"));
        // WAL must be truncated.
        assert_eq!(wal.read_all().unwrap().len(), 0);
    }

    /// Task 1.2 DoD: simulate a crash between rename and truncate by
    /// calling save() + rename manually (not truncate), then loading
    /// via with_data_dir. The idempotent replay (Task 1.3) should
    /// prevent duplicates. Here we verify the simpler property: after
    /// an atomic swap, the checkpoint is loadable and the WAL (if not
    /// truncated) replays on top without data loss. The full
    /// crash-between-rename-and-truncate scenario is covered by the
    /// integration test in tests/dml_checkpoint.rs.
    #[test]
    fn checkpoint_atomic_swap_is_loadable() {
        let ckpt_dir = tempfile::TempDir::new().unwrap();
        let ckpt_path = ckpt_dir.path().join("checkpoint.sql");
        let tmp_path = ckpt_path.with_extension("sql.tmp");

        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        cat.register(DS::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![10, 20, 30],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 3,
        }));

        // Write to tmp, rename — but DON'T truncate the WAL.
        let n = Checkpoint::save(&cat, &tmp_path).unwrap();
        assert_eq!(n, 1);
        std::fs::rename(&tmp_path, &ckpt_path).unwrap();
        assert!(!tmp_path.exists());
        assert!(ckpt_path.exists());

        // The checkpoint is loadable.
        let mut engine = crate::engine::QueryEngine::in_memory();
        let loaded = Checkpoint::load(&mut engine, &ckpt_path).unwrap();
        assert!(loaded > 0, "checkpoint must load at least one statement");
        let result = engine.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.columns[0].values[0], 3, "all 3 rows must be in the checkpoint");
    }

    /// Task 1.3 DoD: WAL records carry a monotonic LSN assigned by Wal::append().
    #[test]
    fn wal_assigns_monotonic_lsns() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        assert_eq!(wal.current_lsn(), 0, "no records appended yet");
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (3)")).unwrap();
        assert_eq!(wal.current_lsn(), 3, "three records appended");
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].lsn, 1);
        assert_eq!(records[1].lsn, 2);
        assert_eq!(records[2].lsn, 3);
    }

    /// Task 1.3 DoD: after truncate, next_lsn is preserved (not reset),
    /// so new records get LSNs strictly greater than pre-truncate records.
    #[test]
    fn wal_truncate_preserves_next_lsn() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();
        assert_eq!(wal.current_lsn(), 2);
        wal.truncate().unwrap();
        assert_eq!(wal.current_lsn(), 2, "current_lsn preserved after truncate");
        assert_eq!(wal.read_all().unwrap().len(), 0, "WAL is empty");
        // New record gets LSN 3, not 1.
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (3)")).unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lsn, 3, "post-truncate record gets LSN 3");
    }

    /// Task 1.3 DoD: Wal::open() scans the existing WAL to recover next_lsn.
    #[test]
    fn wal_open_recovers_next_lsn_from_existing_records() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path()).unwrap();
            wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
            wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();
            wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (3)")).unwrap();
            wal.sync().unwrap();
            assert_eq!(wal.current_lsn(), 3);
        }
        // Reopen — next_lsn should be recovered as 4.
        let wal = Wal::open(tmp.path()).unwrap();
        assert_eq!(wal.current_lsn(), 3, "current_lsn recovered from existing records");
    }

    /// Task 1.3 DoD: advance_lsn_to bumps next_lsn past the given LSN.
    #[test]
    fn wal_advance_lsn_to() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        assert_eq!(wal.current_lsn(), 0);
        wal.advance_lsn_to(10);
        assert_eq!(wal.current_lsn(), 10, "advance_lsn_to(10) sets current_lsn to 10");
        // Appending a new record gets LSN 11.
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records[0].lsn, 11);
        // advance_lsn_to with a lower value is a no-op.
        wal.advance_lsn_to(5);
        assert_eq!(wal.current_lsn(), 11, "advance_lsn_to(5) is a no-op");
    }

    /// Task 1.3 DoD: Checkpoint::save_and_truncate writes the last_lsn sidecar.
    #[test]
    fn checkpoint_writes_last_lsn_sidecar() {
        let wal_tmp = NamedTempFile::new().unwrap();
        let ckpt_dir = tempfile::TempDir::new().unwrap();
        let ckpt_path = ckpt_dir.path().join("checkpoint.sql");
        let mut wal = Wal::open(wal_tmp.path()).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (1)")).unwrap();
        wal.append(&WalRecord::autocommit("INSERT INTO t VALUES (2)")).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.current_lsn(), 2);

        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        cat.register(DS::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2],
                row_count: 2,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: 2,
        }));

        Checkpoint::save_and_truncate(&cat, &ckpt_path, &mut wal).unwrap();
        let last_lsn = Checkpoint::read_last_lsn(&ckpt_path);
        assert_eq!(last_lsn, Some(2), "sidecar must contain last_lsn=2");
    }
}
