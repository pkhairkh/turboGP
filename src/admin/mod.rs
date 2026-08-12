//! # turboGP admin CLI.
//!
//! Wave 9 of the Production Wiring Programme. This module implements the
//! command dispatch and operational tooling behind the `turbogp-admin`
//! binary: `backup`, `restore`, `cluster-status`, `vacuum`, and
//! `checkpoint` subcommands that an operator can run against a
//! `--data-dir` without a SQL connection.
//!
//! The binary entry point lives at `src/bin/turbogp-admin.rs` and is a
//! thin shim that calls [`run`]. Tests live in this module so that
//! `cargo test --lib admin` exercises them.
//!
//! ## File-level backup / restore
//!
//! Task 9.1's `backup` and `restore` perform a file-level recursive
//! copy of the data directory. turboGP does not yet have an online
//! backup story; a file-level copy is acceptable for Wave 9 but MUST
//! be run while the server is stopped (otherwise the copy may capture
//! an inconsistent on-disk state — partial WAL segments, half-fsynced
//! buffer-pool pages, or an in-flight `checkpoint.bin.tmp`).

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

/// `turbogp-admin` CLI arguments (parsed by `clap` derive).
#[derive(Parser, Debug)]
#[command(
    name = "turbogp-admin",
    version,
    about = "turboGP admin CLI — operational tooling",
    long_about = None
)]
pub struct AdminCli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: AdminCommand,
}

/// Admin subcommands.
///
/// Each variant maps 1:1 to a `turboGP admin <sub>` invocation. The
/// `--data-dir` flag is required on every subcommand and points at the
/// on-disk turboGP data directory (the same path passed to
/// `turbogp --data-dir`).
#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Copy the data directory (sled DB + WAL segments + checkpoints)
    /// to a backup location. Run while the server is stopped.
    Backup {
        /// Path to the live turboGP data directory.
        #[arg(long)]
        data_dir: PathBuf,

        /// Path where the backup will be written. Must not be inside
        /// `data_dir` (the admin tool refuses to back up into itself).
        #[arg(long)]
        output: PathBuf,
    },

    /// Restore the data directory from a backup. The destination
    /// `--data-dir` must not exist or must be empty.
    Restore {
        /// Path where the data directory will be recreated. Must be
        /// empty or non-existent.
        #[arg(long)]
        data_dir: PathBuf,

        /// Path to a backup previously produced by `backup`.
        #[arg(long)]
        input: PathBuf,
    },

    /// Print a human-readable summary of the Raft state stored in the
    /// sled DB at `--data-dir` (vote, last committed log id, last
    /// applied log id, current snapshot).
    ///
    /// Only available when turboGP is compiled with `--features raft`.
    ClusterStatus {
        /// Path to the turboGP data directory.
        #[arg(long)]
        data_dir: PathBuf,
    },

    /// Run `VACUUM` on every table in the catalog, reclaiming space
    /// from deleted rows and truncating the WAL.
    Vacuum {
        /// Path to the turboGP data directory.
        #[arg(long)]
        data_dir: PathBuf,
    },

    /// Flush the WAL and write `checkpoint.bin` + `checkpoint.sql` so
    /// the WAL can be safely truncated. After `checkpoint`, a restart
    /// loads state from `checkpoint.bin` and replays only post-checkpoint
    /// WAL records.
    Checkpoint {
        /// Path to the turboGP data directory.
        #[arg(long)]
        data_dir: PathBuf,
    },
}

/// Entry point: parse CLI args and dispatch to the appropriate handler.
///
/// Returns the process exit code (`0` on success, `1` on error).
pub fn run() -> i32 {
    let cli = AdminCli::parse();
    dispatch(cli)
}

/// Dispatch a parsed [`AdminCli`] to the appropriate handler.
///
/// Returns the process exit code (`0` on success, `1` on error).
/// Exposed separately from [`run`] so tests can construct an
/// [`AdminCli`] and invoke dispatch without re-parsing `std::env::args`.
pub fn dispatch(cli: AdminCli) -> i32 {
    match cli.command {
        AdminCommand::Backup { data_dir, output } => match backup(&data_dir, &output) {
            Ok(()) => {
                println!("backup: copied {} -> {}", data_dir.display(), output.display());
                0
            }
            Err(e) => {
                eprintln!("backup failed: {e}");
                1
            }
        },
        AdminCommand::Restore { data_dir, input } => match restore(&data_dir, &input) {
            Ok(()) => {
                println!("restore: copied {} -> {}", input.display(), data_dir.display());
                0
            }
            Err(e) => {
                eprintln!("restore failed: {e}");
                1
            }
        },
        AdminCommand::ClusterStatus { data_dir } => match cluster_status(&data_dir) {
            Ok(report) => {
                print!("{report}");
                0
            }
            Err(e) => {
                eprintln!("cluster-status failed: {e}");
                1
            }
        },
        AdminCommand::Vacuum { data_dir } => match vacuum(&data_dir) {
            Ok(report) => {
                print!("{report}");
                0
            }
            Err(e) => {
                eprintln!("vacuum failed: {e}");
                1
            }
        },
        AdminCommand::Checkpoint { data_dir } => match checkpoint(&data_dir) {
            Ok(report) => {
                print!("{report}");
                0
            }
            Err(e) => {
                eprintln!("checkpoint failed: {e}");
                1
            }
        },
    }
}

/// Recursively copy `src` into `dst` (creating `dst` if needed).
///
/// Symlinks are skipped (avoids following a link out of the data dir
/// or into a cycle). File contents are copied byte-for-byte via
/// [`std::fs::copy`]. Subdirectories are recursed into.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let metadata = entry.metadata()?;
        // Skip symlinks — they may point outside the data dir.
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// File-level backup: recursively copy `data_dir` to `output`.
///
/// Creates `output` (and any missing parent directories) if it does
/// not exist. The data directory is copied byte-for-byte — this
/// includes the WAL segments (`wal/`), the sled DB files (when the
/// `raft` feature is enabled), the binary checkpoint
/// (`checkpoint.bin`), and the legacy SQL checkpoint
/// (`checkpoint.sql`).
///
/// # Errors
///
/// Returns an error string if `data_dir` does not exist, is not a
/// directory, is inside `output` (or vice-versa), or any underlying
/// I/O operation fails.
pub fn backup(data_dir: &Path, output: &Path) -> Result<(), String> {
    if !data_dir.exists() {
        return Err(format!("data dir does not exist: {}", data_dir.display()));
    }
    if !data_dir.is_dir() {
        return Err(format!("data dir is not a directory: {}", data_dir.display()));
    }
    // Refuse to back up into a subdirectory of the data dir — that
    // would cause the recursion to copy the backup into itself.
    if output.starts_with(data_dir) {
        return Err(format!(
            "backup output must not be inside the data dir: {} is inside {}",
            output.display(),
            data_dir.display()
        ));
    }
    if data_dir.starts_with(output) {
        return Err(format!(
            "data dir must not be inside the backup output: {} is inside {}",
            data_dir.display(),
            output.display()
        ));
    }
    copy_dir_recursive(data_dir, output).map_err(|e| e.to_string())
}

/// File-level restore: recursively copy `input` into `data_dir`.
///
/// `data_dir` must not exist, or must be empty. The backup is copied
/// byte-for-byte; on success, `data_dir` contains the same files as
/// `input` and can be opened by `QueryEngine::with_data_dir`.
///
/// # Errors
///
/// Returns an error string if `data_dir` is non-empty, `input` does
/// not exist or is not a directory, or any underlying I/O operation
/// fails.
pub fn restore(data_dir: &Path, input: &Path) -> Result<(), String> {
    if !input.exists() {
        return Err(format!("backup input does not exist: {}", input.display()));
    }
    if !input.is_dir() {
        return Err(format!("backup input is not a directory: {}", input.display()));
    }
    if data_dir.exists() {
        let mut entries = match std::fs::read_dir(data_dir) {
            Ok(it) => it,
            Err(e) => return Err(format!("read data dir: {e}")),
        };
        if entries.next().is_some() {
            return Err(format!(
                "data dir is not empty (refusing to overwrite): {}",
                data_dir.display()
            ));
        }
    } else {
        std::fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
    }
    copy_dir_recursive(input, data_dir).map_err(|e| e.to_string())
}

/// Open the sled DB at `data_dir` and read the persisted Raft state
/// (vote, last committed log id, last log id, last applied log id,
/// membership, current snapshot, applied-records count) into a
/// human-readable summary string.
///
/// This function does NOT start a Raft instance — it reads the sled
/// trees directly via [`SledRaftStore`]'s `RaftStorage` trait impl.
///
/// # Feature gate
///
/// Requires the `raft` feature. When turboGP is compiled without
/// `--features raft`, this function returns an error.
///
/// # Errors
///
/// Returns an error string if the sled DB cannot be opened, the tokio
/// runtime cannot be created, or any underlying Raft-storage read
/// fails.
#[cfg(feature = "raft")]
pub fn cluster_status(data_dir: &Path) -> Result<String, String> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| format!("create tokio runtime: {e}"))?;
    rt.block_on(cluster_status_async(data_dir))
}

/// Async implementation of [`cluster_status`].
///
/// Exposed separately so tests running inside an existing tokio
/// runtime (e.g. `#[tokio::test]`) can call it directly without
/// spawning a nested runtime (which would panic).
#[cfg(feature = "raft")]
pub async fn cluster_status_async(data_dir: &Path) -> Result<String, String> {
    use openraft::storage::RaftStorage;
    use crate::storage::raft_store::SledRaftStore;

    let mut store = SledRaftStore::open(data_dir)
        .map_err(|e| format!("open sled store at {}: {e}", data_dir.display()))?;

    let vote = store.read_vote().await.ok().flatten();
    let committed = store.read_committed().await.ok().flatten();
    let log_state = store.get_log_state().await.ok();
    let last_applied_pair = store.last_applied_state().await.ok();
    let snapshot = store.get_current_snapshot().await.ok().flatten();
    let applied_records = store.applied_records().ok();

    let mut out = String::new();
    out.push_str(&format!("turboGP cluster status ({})\n", data_dir.display()));
    out.push_str("==========================================\n");

    out.push_str(&format!(
        "Vote:              {}\n",
        vote.as_ref().map(|v| v.to_string()).unwrap_or_else(|| "none (uninitialized)".to_string())
    ));
    out.push_str(&format!(
        "Last committed:    {}\n",
        committed.as_ref().map(|c| c.to_string()).unwrap_or_else(|| "none".to_string())
    ));

    if let Some(state) = log_state {
        out.push_str(&format!(
            "Last log id:       {}\n",
            state.last_log_id.as_ref().map(|l| l.to_string()).unwrap_or_else(|| "none (empty log)".to_string())
        ));
        out.push_str(&format!(
            "Last purged log:   {}\n",
            state.last_purged_log_id.as_ref().map(|l| l.to_string()).unwrap_or_else(|| "none".to_string())
        ));
    } else {
        out.push_str("Last log id:       <error reading log state>\n");
    }

    if let Some((last_applied, last_membership)) = last_applied_pair {
        out.push_str(&format!(
            "Last applied:      {}\n",
            last_applied.as_ref().map(|l| l.to_string()).unwrap_or_else(|| "none".to_string())
        ));
        out.push_str(&format!("Membership:        {}\n", last_membership));
    } else {
        out.push_str("Last applied:      <error reading applied state>\n");
    }

    if let Some(snap) = snapshot {
        out.push_str(&format!("Snapshot:          present (meta: {})\n", snap.meta));
    } else {
        out.push_str("Snapshot:          none\n");
    }

    if let Some(records) = applied_records {
        out.push_str(&format!("Applied records:   {} entries\n", records.len()));
    }

    Ok(out)
}

/// Stub for [`cluster_status`] when the `raft` feature is disabled.
///
/// Returns an error indicating that the `cluster-status` subcommand
/// requires turboGP to be compiled with `--features raft`.
#[cfg(not(feature = "raft"))]
pub fn cluster_status(_data_dir: &Path) -> Result<String, String> {
    Err("cluster-status requires turboGP to be compiled with --features raft".to_string())
}

/// Open a `QueryEngine` rooted at `data_dir` and execute `VACUUM` on
/// every table in the catalog.
///
/// The summary string reports per-table row counts before and after
/// VACUUM, plus a total. (Without MVCC enabled, VACUUM still runs the
/// checkpoint + WAL truncation; it just doesn't compact column vectors.
/// Column compaction kicks in when MVCC is enabled.)
///
/// # Errors
///
/// Returns an error string if the engine cannot be opened or `VACUUM`
/// fails.
pub fn vacuum(data_dir: &Path) -> Result<String, String> {
    let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
        .map_err(|e| format!("open engine at {}: {e}", data_dir.display()))?;

    let table_names: Vec<String> = engine
        .catalog
        .table_names()
        .into_iter()
        .filter(|n| !n.starts_with("__"))
        .collect();

    let before_counts: Vec<(String, usize)> = table_names
        .iter()
        .map(|name| {
            let count = engine.catalog.with(name, |t| t.row_count).unwrap_or(0);
            (name.clone(), count)
        })
        .collect();
    let before_total: usize = before_counts.iter().map(|(_, c)| *c).sum();

    engine.execute("VACUUM").map_err(|e| format!("VACUUM: {e}"))?;

    let after_counts: Vec<(String, usize)> = table_names
        .iter()
        .map(|name| {
            let count = engine.catalog.with(name, |t| t.row_count).unwrap_or(0);
            (name.clone(), count)
        })
        .collect();
    let after_total: usize = after_counts.iter().map(|(_, c)| *c).sum();

    let mut out = String::new();
    out.push_str("VACUUM summary:\n");
    for (i, (name, before)) in before_counts.iter().enumerate() {
        let after = after_counts[i].1;
        out.push_str(&format!(
            "  table '{}': {} rows (before) -> {} rows (after)\n",
            name, before, after
        ));
    }
    out.push_str(&format!(
        "VACUUM complete: {} tables, {} -> {} rows\n",
        table_names.len(),
        before_total,
        after_total
    ));
    Ok(out)
}

/// Open a `QueryEngine` rooted at `data_dir` and execute `CHECKPOINT`,
/// which flushes dirty buffer-pool pages, fsyncs the WAL, writes
/// `checkpoint.bin` (binary catalog snapshot, atomic swap), writes
/// `checkpoint.sql` (legacy SQL-text checkpoint), and truncates the WAL
/// (the committed state is now durably in the checkpoints).
///
/// # Errors
///
/// Returns an error string if the engine cannot be opened or
/// `CHECKPOINT` fails.
pub fn checkpoint(data_dir: &Path) -> Result<String, String> {
    let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
        .map_err(|e| format!("open engine at {}: {e}", data_dir.display()))?;
    engine.execute("CHECKPOINT").map_err(|e| format!("CHECKPOINT: {e}"))?;

    let bin_path = data_dir.join("checkpoint.bin");
    let sql_path = data_dir.join("checkpoint.sql");
    let mut out = String::new();
    out.push_str("CHECKPOINT complete:\n");
    out.push_str(&format!(
        "  checkpoint.bin: {}\n",
        if bin_path.exists() { "present" } else { "missing" }
    ));
    out.push_str(&format!(
        "  checkpoint.sql: {}\n",
        if sql_path.exists() { "present" } else { "missing" }
    ));
    out.push_str("  WAL flushed + truncated\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// `backup` copies every file in the data dir to the output dir,
    /// preserving subdirectory structure and file contents.
    #[test]
    fn admin_backup_creates_copy_of_data_dir() {
        let src = TempDir::new().expect("tempdir src");
        let data_dir = src.path();
        // Populate the data dir with some files mimicking a real
        // turboGP data dir: a WAL segment, a checkpoint, and a buffer
        // pool page.
        fs::create_dir_all(data_dir.join("wal")).expect("mkdir wal");
        fs::write(data_dir.join("wal").join("wal-0.log"), b"waldata").expect("write wal");
        fs::write(data_dir.join("checkpoint.bin"), b"BINCP").expect("write bin");
        fs::write(data_dir.join("checkpoint.sql"), b"CREATE TABLE t;").expect("write sql");
        fs::write(data_dir.join("1.tbl"), b"page1").expect("write tbl");

        let backup_root = TempDir::new().expect("tempdir backup");
        let output = backup_root.path().join("snapshot");

        backup(data_dir, &output).expect("backup ok");

        // Every file should be present at the output path with the
        // same contents.
        let got_wal = fs::read(output.join("wal").join("wal-0.log")).expect("read wal");
        assert_eq!(got_wal, b"waldata", "WAL segment must round-trip");
        let got_bin = fs::read(output.join("checkpoint.bin")).expect("read bin");
        assert_eq!(got_bin, b"BINCP", "checkpoint.bin must round-trip");
        let got_sql = fs::read(output.join("checkpoint.sql")).expect("read sql");
        assert_eq!(got_sql, b"CREATE TABLE t;", "checkpoint.sql must round-trip");
        let got_tbl = fs::read(output.join("1.tbl")).expect("read tbl");
        assert_eq!(got_tbl, b"page1", "buffer-pool page must round-trip");
    }

    /// `restore` copies files from the backup into a fresh (empty or
    /// non-existent) data directory, and refuses to overwrite a
    /// non-empty destination.
    #[test]
    fn admin_restore_copies_files_into_empty_dir() {
        // Build a fake backup.
        let backup_root = TempDir::new().expect("tempdir backup");
        let backup_dir = backup_root.path();
        fs::create_dir_all(backup_dir.join("wal")).expect("mkdir wal");
        fs::write(backup_dir.join("wal").join("wal-0.log"), b"waldata").expect("write wal");
        fs::write(backup_dir.join("checkpoint.bin"), b"BINCP").expect("write bin");

        // Restore into a fresh (non-existent) data dir.
        let dst = TempDir::new().expect("tempdir dst");
        let data_dir = dst.path().join("data");
        // data_dir does not exist yet — restore should create it.
        restore(&data_dir, backup_dir).expect("restore ok");

        let got_wal = fs::read(data_dir.join("wal").join("wal-0.log")).expect("read wal");
        assert_eq!(got_wal, b"waldata", "WAL segment must round-trip on restore");
        let got_bin = fs::read(data_dir.join("checkpoint.bin")).expect("read bin");
        assert_eq!(got_bin, b"BINCP", "checkpoint.bin must round-trip on restore");

        // Restoring into a non-empty dir must fail.
        let err = restore(&data_dir, backup_dir).err().expect("expected error on non-empty dst");
        assert!(
            err.contains("not empty"),
            "expected 'not empty' error, got: {err}"
        );

        // Restoring from a non-existent input must fail.
        let err = restore(&dst.path().join("other"), Path::new("/nonexistent/backup"))
            .err()
            .expect("expected error on missing input");
        assert!(
            err.contains("does not exist"),
            "expected 'does not exist' error, got: {err}"
        );
    }

    /// `cluster_status_async` opens a sled DB at `--data-dir`, reads
    /// the persisted Raft state via the `RaftStorage` trait, and
    /// formats a human-readable summary. This test seeds the store
    /// with a vote, committed id, log entry, and applied state
    /// machine, then verifies the summary mentions every field.
    ///
    /// Gated on `feature = "raft"` (the `SledRaftStore` requires the
    /// `openraft` + `sled` dependencies).
    #[cfg(feature = "raft")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admin_cluster_status_prints_raft_state() {
        use openraft::storage::RaftStorage;
        use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, Vote};
        use crate::storage::raft::TypeConfig;
        use crate::storage::raft_store::SledRaftStore;

        let dir = TempDir::new().expect("tempdir");
        let data_dir = dir.path().to_path_buf();

        // Phase 1: open the store and persist a representative slice
        // of Raft state.
        let mut store = SledRaftStore::open(&data_dir).expect("open");
        let vote = Vote::<u64>::new_committed(7, 3);
        store.save_vote(&vote).await.expect("save_vote");

        let committed = Some(LogId::<u64>::new(CommittedLeaderId::<u64>::new(7, 3), 42));
        store.save_committed(committed).await.expect("save_committed");

        let entry = Entry::<TypeConfig> {
            log_id: LogId::<u64>::new(CommittedLeaderId::<u64>::new(7, 3), 42),
            payload: EntryPayload::Normal(vec![0xAB; 4]),
        };
        store.append_to_log(vec![entry]).await.expect("append");

        let entries = vec![Entry::<TypeConfig> {
            log_id: LogId::<u64>::new(CommittedLeaderId::<u64>::new(7, 3), 42),
            payload: EntryPayload::Normal(vec![0xCD; 8]),
        }];
        store.apply_to_state_machine(&entries).await.expect("apply");

        // Drop the store so the sled lock is released before
        // cluster_status_async reopens it.
        drop(store);

        // Phase 2: read state back via the admin API.
        let out = cluster_status_async(&data_dir)
            .await
            .expect("cluster_status_async ok");

        // Verify the report mentions every persisted field.
        // openraft's Display for Vote<u64> is "T<term>-N<node_id>:committed"
        // and for LogId<u64> is "T<term>-N<node_id>-<index>".
        assert!(out.contains("Vote:"), "expected Vote line, got:\n{out}");
        assert!(
            out.contains("T7-N3"),
            "expected T7-N3 (term=7 node_id=3) in vote, got:\n{out}"
        );
        assert!(
            out.contains("committed"),
            "expected 'committed' flag in vote display, got:\n{out}"
        );
        assert!(
            out.contains("Last committed:"),
            "expected Last committed line, got:\n{out}"
        );
        assert!(
            out.contains("Last log id:"),
            "expected Last log id line, got:\n{out}"
        );
        assert!(
            out.contains("Last applied:"),
            "expected Last applied line, got:\n{out}"
        );
        assert!(
            out.contains("Applied records:"),
            "expected Applied records line, got:\n{out}"
        );
        assert!(
            out.contains("1 entries"),
            "expected exactly 1 applied record, got:\n{out}"
        );
    }

    /// `vacuum` opens a `QueryEngine` rooted at `--data-dir`, calls
    /// `engine.execute("VACUUM")`, and returns a summary with
    /// per-table before/after row counts.
    ///
    /// The test creates a table with 3 rows, then calls `vacuum` and
    /// verifies the summary enumerates the table and reports 3 rows.
    #[test]
    fn admin_vacuum_runs_vacuum_on_all_tables() {
        let dir = TempDir::new().expect("tempdir");
        let data_dir = dir.path();

        // Phase 1: create a table with 3 rows.
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
                .expect("with_data_dir");
            engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");
            engine.execute("INSERT INTO t VALUES (1), (2), (3)").expect("INSERT");
        }

        // Phase 2: call admin vacuum on the data dir.
        let out = vacuum(data_dir).expect("vacuum ok");

        // Verify the summary enumerates the table and reports the
        // expected row count.
        assert!(
            out.contains("VACUUM summary"),
            "expected 'VACUUM summary' header, got: {out}"
        );
        assert!(
            out.contains("table 't'"),
            "expected table 't' in output, got: {out}"
        );
        assert!(
            out.contains("VACUUM complete"),
            "expected 'VACUUM complete' footer, got: {out}"
        );
        assert!(
            out.contains("3 rows"),
            "expected '3 rows' in before/after counts, got: {out}"
        );
    }

    /// `checkpoint` opens a `QueryEngine` rooted at `--data-dir`,
    /// calls `engine.execute("CHECKPOINT")`, and verifies the
    /// resulting `checkpoint.bin` + `checkpoint.sql` files exist.
    ///
    /// After `checkpoint`, a fresh engine reopens the data dir and
    /// the previously-inserted row must survive (the binary
    /// checkpoint is preferred over the WAL on restart).
    #[test]
    fn admin_checkpoint_flushes_wal() {
        let dir = TempDir::new().expect("tempdir");
        let data_dir = dir.path();

        // Phase 1: create a table with 1 row (but no explicit
        // CHECKPOINT — the WAL has uncheckpointed records).
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
                .expect("with_data_dir");
            engine.execute("CREATE TABLE t (id INT)").expect("CREATE TABLE");
            engine.execute("INSERT INTO t VALUES (42)").expect("INSERT");
        }

        // Phase 2: call admin checkpoint.
        let out = checkpoint(data_dir).expect("checkpoint ok");
        assert!(
            out.contains("CHECKPOINT complete"),
            "expected 'CHECKPOINT complete' header, got: {out}"
        );
        assert!(
            out.contains("checkpoint.bin: present"),
            "expected 'checkpoint.bin: present', got: {out}"
        );

        // Verify the binary checkpoint file exists on disk.
        assert!(
            data_dir.join("checkpoint.bin").exists(),
            "checkpoint.bin should exist after admin checkpoint"
        );

        // Phase 3: reopen the engine and verify the row survived
        // via the binary checkpoint.
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
                .expect("reopen engine");
            let r = engine.execute("SELECT count(*) FROM t").expect("SELECT count(*)");
            assert_eq!(
                r.scalar_u64(),
                Some(1),
                "row must survive admin checkpoint round-trip"
            );
            let r = engine.execute("SELECT id FROM t").expect("SELECT id");
            assert_eq!(
                r.scalar_u64(),
                Some(42),
                "specific row value must round-trip"
            );
        }
    }

    /// End-to-end admin CLI round-trip: write data via `QueryEngine`,
    /// `backup` the data dir, `restore` into a fresh dir, then reopen
    /// via `QueryEngine::with_data_dir` and verify every row survived.
    ///
    /// This is the Task 9.3 DoD test. It calls the admin functions
    /// directly (no subprocess) — faster than spawning the binary and
    /// avoids build-order complexity.
    #[test]
    fn admin_end_to_end_backup_restore_round_trip() {
        let src_dir = TempDir::new().expect("tempdir src");
        let data_dir = src_dir.path();
        let backup_root = TempDir::new().expect("tempdir backup");
        let backup_dir = backup_root.path().join("snapshot");
        let restore_root = TempDir::new().expect("tempdir restore");
        let restored_data_dir = restore_root.path().join("data");

        // Phase 1: start a turboGP instance, insert 50 rows, CHECKPOINT
        // so the catalog is on disk (checkpoint.bin) and the WAL is
        // truncated.
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(data_dir)
                .expect("with_data_dir");
            engine.execute("CREATE TABLE t (id INT, v INT)").expect("CREATE TABLE");
            for i in 0..50u64 {
                let sql = format!("INSERT INTO t VALUES ({}, {})", i, i * 2);
                engine.execute(&sql).expect("INSERT");
            }
            // Verify the data is queryable before backup.
            let r = engine.execute("SELECT count(*) FROM t").expect("count before backup");
            assert_eq!(r.scalar_u64(), Some(50), "50 rows must be visible before backup");

            // CHECKPOINT so the catalog is durably on disk and the WAL
            // is truncated. Without this, the data lives only in the
            // WAL (which would also round-trip, but CHECKPOINT is the
            // more interesting case to verify the binary snapshot
            // survives the file-level copy).
            engine.execute("CHECKPOINT").expect("CHECKPOINT");
        } // engine dropped — files closed

        // Phase 2: admin backup — recursively copy data_dir to backup_dir.
        backup(data_dir, &backup_dir).expect("backup ok");

        // Sanity check: the backup should contain checkpoint.bin (the
        // binary catalog snapshot) and the wal/ directory.
        assert!(
            backup_dir.join("checkpoint.bin").exists(),
            "backup must contain checkpoint.bin"
        );
        assert!(
            backup_dir.join("wal").is_dir(),
            "backup must contain the wal/ directory"
        );

        // Phase 3: admin restore — copy backup_dir into a fresh data dir.
        // restored_data_dir does not exist yet; restore() creates it.
        restore(&restored_data_dir, &backup_dir).expect("restore ok");

        // Verify the restored data dir contains the same key files.
        assert!(
            restored_data_dir.join("checkpoint.bin").exists(),
            "restored data dir must contain checkpoint.bin"
        );

        // Phase 4: reopen the engine at the restored data dir and
        // verify every row survived.
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(&restored_data_dir)
                .expect("reopen restored engine");
            let r = engine.execute("SELECT count(*) FROM t").expect("count after restore");
            assert_eq!(
                r.scalar_u64(),
                Some(50),
                "row count must round-trip through backup -> restore"
            );
            // Spot-check a specific row: v = id * 2.
            let r = engine
                .execute("SELECT v FROM t WHERE id = 42")
                .expect("select v where id=42");
            assert_eq!(
                r.scalar_u64(),
                Some(84),
                "row id=42 must have v=84 after restore"
            );
            // Spot-check another row.
            let r = engine
                .execute("SELECT id FROM t WHERE v = 30")
                .expect("select id where v=30");
            assert_eq!(
                r.scalar_u64(),
                Some(15),
                "row with v=30 must have id=15 after restore"
            );
        }

        // Phase 5: the admin tooling must compose — running backup
        // again from the restored dir, then restoring into a second
        // fresh dir, must round-trip the same data.
        let backup2_root = TempDir::new().expect("tempdir backup2");
        let backup2_dir = backup2_root.path().join("snapshot2");
        let restore2_root = TempDir::new().expect("tempdir restore2");
        let restored2_data_dir = restore2_root.path().join("data");
        backup(&restored_data_dir, &backup2_dir).expect("backup2 ok");
        restore(&restored2_data_dir, &backup2_dir).expect("restore2 ok");
        {
            let mut engine = crate::engine::QueryEngine::with_data_dir(&restored2_data_dir)
                .expect("reopen second restored engine");
            let r = engine
                .execute("SELECT count(*) FROM t")
                .expect("count after second restore");
            assert_eq!(
                r.scalar_u64(),
                Some(50),
                "row count must round-trip through two backup -> restore cycles"
            );
        }
    }
}
