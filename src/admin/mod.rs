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
}
