//! turboGP server binary.
//!
//! Wave 11: A tokio::main entrypoint with clap CLI parsing and graceful
//! shutdown on SIGTERM/SIGINT. This makes turboGP deployable as a
//! standalone binary (no need to write your own tokio::main).
//!
//! Usage:
//! ```sh
//! turbogp --port 5432 --data-dir ./data --auth
//! turbogp --insecure  # no auth (for development)
//! turbogp --tls-cert cert.pem --tls-key key.pem
//! ```

use clap::Parser;
use std::sync::{Arc, RwLock};
use tokio::signal;
use turbogp::engine::QueryEngine;
use turbogp::server::{PasswordManager, Server, ServerConfig};

/// turboGP — an instruction-first, memory-centric relational database engine.
#[derive(Parser, Debug)]
#[command(name = "turbogp", version, about, long_about = None)]
struct Args {
    /// Port to listen on for pgwire connections (default: 5432).
    #[arg(long, default_value = "5432")]
    port: u16,

    /// Bind address (default: 0.0.0.0 for all interfaces).
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Data directory for WAL, checkpoint, and table files.
    /// If omitted, the engine runs in-memory (data is lost on exit).
    #[arg(long)]
    data_dir: Option<String>,

    /// Require SCRAM-SHA-256 authentication on every connection.
    /// Enabled by default; use --insecure to disable.
    #[arg(long, default_value = "true")]
    auth: bool,

    /// Disable authentication (for trusted-network development).
    #[arg(long, default_value = "false")]
    insecure: bool,

    /// Path to TLS certificate file (PEM format).
    #[arg(long)]
    tls_cert: Option<String>,

    /// Path to TLS private key file (PEM format).
    #[arg(long)]
    tls_key: Option<String>,

    /// Maximum number of concurrent connections (default: 128).
    #[arg(long, default_value = "128")]
    max_connections: usize,

    /// Server name reported in ParameterStatus (default: turboGP).
    #[arg(long, default_value = "turboGP")]
    server_name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging.
    let _ = env_logger::try_init();
    log::info!("turboGP server starting (pid={})", std::process::id());

    let args = Args::parse();

    // Determine auth mode.
    let auth_required = if args.insecure {
        log::warn!("Running in --insecure mode: authentication disabled");
        false
    } else {
        args.auth
    };

    // Create the engine.
    let engine: Arc<RwLock<QueryEngine>> = if let Some(ref data_dir) = args.data_dir {
        log::info!("Opening engine with data_dir: {data_dir}");
        match QueryEngine::with_data_dir(data_dir) {
            Ok(e) => Arc::new(RwLock::new(e)),
            Err(e) => {
                log::error!("Failed to open engine with data_dir '{data_dir}': {e}");
                return Err(e.into());
            }
        }
    } else {
        log::warn!("No --data-dir specified: running in-memory (data will be lost on exit)");
        Arc::new(RwLock::new(QueryEngine::in_memory()))
    };

    // Build the server config.
    let addr: std::net::SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| format!("invalid bind address {}:{}: {e}", args.host, args.port))?;

    let config = ServerConfig {
        addr,
        server_name: args.server_name.clone(),
        auth_required,
        tls: None, // TODO: wire TLS from --tls-cert/--tls-key (Wave 1 follow-up)
        passwords: Arc::new(RwLock::new(PasswordManager::new())),
        max_connections: args.max_connections,
    };

    // Start the server.
    let server = Server::bind(engine, config).await?;
    log::info!(
        "turboGP listening on {} (auth={}, max_connections={})",
        server.local_addr,
        auth_required,
        args.max_connections
    );
    log::info!("Press Ctrl+C to shut down gracefully");

    // Wait for shutdown signal.
    tokio::select! {
        _ = signal::ctrl_c() => {
            log::info!("Received SIGINT, shutting down...");
        }
        _ = async {
            #[cfg(unix)]
            {
                signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler")
                    .recv()
                    .await
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await
            }
        } => {
            log::info!("Received SIGTERM, shutting down...");
        }
    }

    log::info!("turboGP server stopped");
    Ok(())
}
