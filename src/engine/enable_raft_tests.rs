//! Tests for the Production Wiring Wave 6 Task 6.1 DoD:
//! `QueryEngine::enable_raft` defaults `Wal::sync_mode = Synchronous`
//! AND attaches a `MultiWalStreamSink` (default `QuorumPolicy::Majority`)
//! to the Wal.
//!
//! Combined with Wave 4's Raft routing in `Wal::append_and_sync`, a user
//! who calls `enable_raft` gets durable sync replication out of the box —
//! every commit goes through Raft consensus and is propagated to the
//! quorum fan-out sink before the local WAL append returns.

#![cfg(all(test, feature = "raft"))]

use crate::engine::QueryEngine;
use crate::storage::recovery::SyncMode;
use tempfile::TempDir;

/// Calling `QueryEngine::enable_raft(node_id, peers)` sets
/// `Wal::sync_mode = Synchronous` AND attaches a `MultiWalStreamSink`
/// (default `QuorumPolicy::Majority`).
///
/// Asserts:
/// - Before `enable_raft`, `Wal::sync_mode()` is `Asynchronous` and
///   `Wal::has_stream_sink()` is `false` (the default local-only path).
/// - After `enable_raft`, `Wal::sync_mode()` is `Synchronous`,
///   `Wal::has_stream_sink()` is `true`, and the attached sink's type
///   name contains `MultiWalStreamSink`.
#[test]
fn enable_raft_sets_sync_mode_and_quorum() {
    let wal_dir = TempDir::new().expect("wal dir");
    let mut engine = QueryEngine::new();
    engine.enable_wal(wal_dir.path()).expect("enable_wal");

    // Before enable_raft: async, no sink.
    {
        let wal = engine.wal.as_ref().expect("wal");
        assert_eq!(wal.sync_mode(), SyncMode::Asynchronous);
        assert!(!wal.has_stream_sink());
    }

    engine.enable_raft(1, vec![]).expect("enable_raft");

    // After enable_raft: Synchronous + MultiWalStreamSink attached.
    let wal = engine.wal.as_ref().expect("wal");
    assert_eq!(
        wal.sync_mode(),
        SyncMode::Synchronous,
        "enable_raft must default Wal.sync_mode to Synchronous"
    );
    assert!(
        wal.has_stream_sink(),
        "enable_raft must attach a MultiWalStreamSink to the Wal"
    );
    let type_name = wal
        .stream_sink_type_name()
        .expect("stream_sink_type_name should be Some after enable_raft");
    assert!(
        type_name.contains("MultiWalStreamSink"),
        "expected MultiWalStreamSink to be attached, got type name {type_name:?}"
    );

    // Clean up: explicitly shut down the RaftManager to avoid leaks.
    if let Some(mgr) = engine.raft_manager.take() {
        if let Some(rt) = engine.raft_runtime.take() {
            let _ = rt.block_on(mgr.shutdown());
        }
    }
}
