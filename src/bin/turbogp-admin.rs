//! turboGP admin CLI binary.
//!
//! Wave 9 of the Production Wiring Programme: a command-line tool for
//! operators to back up, restore, inspect, and maintain a turboGP
//! data directory without a SQL connection.
//!
//! The actual command implementations live in [`turbogp::admin`]; this
//! binary is a thin shim that parses CLI args via `clap` (inside
//! [`turbogp::admin::run`]) and dispatches to the appropriate handler.
//!
//! # Subcommands
//!
//! - `turboGP admin backup --data-dir ./data --output ./backup` —
//!   file-level copy of the data directory.
//! - `turboGP admin restore --data-dir ./data --input ./backup` —
//!   copy a backup back into a fresh data directory.
//! - `turboGP admin cluster-status --data-dir ./data` — print the
//!   Raft state stored in the sled DB (requires `--features raft`).
//! - `turboGP admin vacuum --data-dir ./data` — run `VACUUM` on every
//!   table in the catalog.
//! - `turboGP admin checkpoint --data-dir ./data` — flush the WAL and
//!   write a checkpoint.

fn main() {
    let exit_code = turbogp::admin::run();
    std::process::exit(exit_code);
}
