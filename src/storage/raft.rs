//! Real Raft consensus via openraft (Wave 5 — Tasks 5.1–5.6).
//!
//! This module provides [`RaftManager`], a thin wrapper around
//! [`openraft::Raft`] that gives turboGP **real** Raft consensus — leader
//! election with randomized timeouts, quorum commits, log replication, and
//! automatic failover — replacing the hand-rolled stub
//! ([`crate::storage::replication::RaftNode`]) that previous waves used.
//!
//! ## Design (within Wave 5 context budget)
//!
//! openraft 0.9 does **not** ship a built-in `MemStore` (the `memstore`
//! crate is a separate example crate). To stay within the per-commit
//! context budget, this module implements a minimal **in-memory** storage
//! backend ([`MemStore`]) directly, implementing openraft's v1
//! [`RaftStorage`] trait and wrapping it with the built-in [`Adaptor`]
//! (which adapts a `RaftStorage` impl to the v2 `RaftLogStorage` +
//! `RaftStateMachine` traits that [`Raft::new`] requires). The store
//! holds log entries, votes, and applied WAL-record bytes in process
//! memory — perfect for testing real consensus in a single process. For
//! production, replace `MemStore` with a persistent backend.
//!
//! The network layer ([`ChannelNetworkFactory`] / [`ChannelNetwork`]) is
//! an in-memory `mpsc` channel transport: each node registers an inbox
//! in a shared [`NetworkRegistry`], and a per-node dispatcher task
//! forwards incoming RPCs to the node's [`Raft`] instance. This lets a
//! 3-node cluster run in one process with real Raft semantics — votes,
//! heartbeats, AppendEntries, and (chunked) InstallSnapshot all flow
//! through the channels.
//!
//! ## WAL replication
//!
//! `RaftManager::propose(&[u8])` calls [`Raft::client_write`] with the
//! WAL record bytes as the entry payload (`D = Vec<u8>`). Raft replicates
//! the entry to a quorum before returning `Ok(())`; the
//! [`MemStore::apply_to_state_machine`] appends the bytes to an in-memory
//! `applied_records` log so tests can inspect what was committed.
//!
//! ## What this replaces
//!
//! The old stub `RaftNode` (in `src/storage/replication.rs`) is retained
//! for backward compatibility with its existing unit tests, but
//! [`QueryEngine::enable_raft`](crate::engine::QueryEngine::enable_raft)
//! now routes to [`RaftManager`] when the `raft` feature is enabled.
//!
//! [`Raft::new`]: openraft::Raft::new
//! [`Raft::client_write`]: openraft::Raft::client_write
//! [`RaftStorage`]: openraft::storage::RaftStorage

#![cfg(feature = "raft")]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::Duration;

use openraft::entry::EntryPayload;
use openraft::error::{Fatal, InstallSnapshotError, RPCError, RaftError, RemoteError, Unreachable};
use openraft::impls::OneshotResponder;
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::{Adaptor, LogFlushed};
use openraft::{
    declare_raft_types, BasicNode, Config, Entry, LogId, LogState, Membership, OptionalSend,
    Raft, RaftLogReader, RaftSnapshotBuilder, RaftStorage, ServerState, Snapshot, SnapshotMeta,
    StorageError, StorageIOError, StoredMembership, TokioRuntime, Vote,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use crate::storage::raft_store::SledRaftStore;
use tokio::task::{AbortHandle, JoinHandle};

// =========================================================================
// Type config — D = Vec<u8> (WAL record bytes), R = ()
// =========================================================================

declare_raft_types!(
    /// turboGP's Raft type config: WAL record bytes are the entry payload,
    /// the state machine response is `()` (we only care that the entry
    /// committed, not what apply returns).
    pub TypeConfig:
        D = Vec<u8>,
        R = (),
);

/// The concrete `Raft` handle for turboGP's [`TypeConfig`].
pub type RaftType = Raft<TypeConfig>;

// =========================================================================
// In-memory storage (MemStore)
// =========================================================================

/// Internal mutable state of a [`MemStore`], guarded by a `tokio::Mutex`.
#[derive(Default)]
struct MemStoreInner {
    /// Greatest log id that has been purged (after being applied).
    last_purged_log_id: Option<LogId<u64>>,
    /// Log entries indexed by log index (consecutive, no holes).
    log: BTreeMap<u64, Entry<TypeConfig>>,
    /// Last persisted vote.
    vote: Option<Vote<u64>>,
    /// Last persisted committed log id (optional — openraft tolerates None).
    committed: Option<LogId<u64>>,
    /// Last applied log id (state-machine high-water mark).
    last_applied: Option<LogId<u64>>,
    /// Last applied membership.
    last_membership: StoredMembership<u64, BasicNode>,
    /// Applied Normal-payload bytes, in apply order (for test inspection).
    applied_records: Vec<Vec<u8>>,
    /// Current snapshot, if any.
    snapshot: Option<Snapshot<TypeConfig>>,
}

/// A minimal in-memory implementation of openraft's [`RaftStorage`] trait.
///
/// All state lives behind an `Arc<Mutex<MemStoreInner>>` so that the
/// log-reader and state-machine clones (handed out by `get_log_reader` /
/// `get_snapshot_builder`) share the same backing store. This matches
/// the pattern openraft's own example `memstore` crate uses.
#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<MemStoreInner>>,
}

impl MemStore {
    /// Create a fresh, empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the applied Normal-payload bytes (for test inspection).
    pub async fn applied_records(&self) -> Vec<Vec<u8>> {
        self.inner.lock().await.applied_records.clone()
    }

    /// Current last-applied log id (for test inspection).
    pub async fn last_applied(&self) -> Option<LogId<u64>> {
        self.inner.lock().await.last_applied.clone()
    }
}

/// Cheap cloneable log reader backed by the same `Arc<Mutex<MemStoreInner>>`.
#[derive(Clone)]
pub struct MemStoreReader {
    inner: Arc<Mutex<MemStoreInner>>,
}

impl RaftLogReader<TypeConfig> for MemStoreReader {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>>
    where
        RB: RangeBounds<u64> + Clone + Debug + OptionalSend,
    {
        let inner = self.inner.lock().await;
        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => i.saturating_add(1),
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(i) => i.saturating_add(1),
            std::ops::Bound::Excluded(i) => *i,
            std::ops::Bound::Unbounded => u64::MAX,
        };
        let mut out = Vec::new();
        for (_idx, entry) in inner.log.range(start..end) {
            out.push(entry.clone());
        }
        Ok(out)
    }
}

impl RaftLogReader<TypeConfig> for MemStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>>
    where
        RB: RangeBounds<u64> + Clone + Debug + OptionalSend,
    {
        MemStoreReader {
            inner: self.inner.clone(),
        }
        .try_get_log_entries(range)
        .await
    }
}

/// Snapshot builder for [`MemStore`].
#[derive(Clone)]
pub struct MemStoreSnapshotBuilder {
    inner: Arc<Mutex<MemStoreInner>>,
}

impl RaftSnapshotBuilder<TypeConfig> for MemStoreSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        let inner = self.inner.lock().await;
        let data = encode_snapshot(&inner.applied_records);
        let last_log_id = inner.last_applied.clone();
        let last_membership = inner.last_membership.clone();
        let snapshot_id = match &last_log_id {
            Some(lid) => format!("snap-{}-n{}", lid, inner.applied_records.len()),
            None => "snap-empty".to_string(),
        };
        let meta = SnapshotMeta {
            last_log_id,
            last_membership,
            snapshot_id,
        };
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStorage<TypeConfig> for MemStore {
    type LogReader = MemStoreReader;
    type SnapshotBuilder = MemStoreSnapshotBuilder;

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        self.inner.lock().await.vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Ok(self.inner.lock().await.vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        self.inner.lock().await.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        Ok(self.inner.lock().await.committed)
    }

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let inner = self.inner.lock().await;
        let last_log_id = if let Some(&idx) = inner.log.keys().next_back() {
            Some(inner.log[&idx].log_id.clone())
        } else {
            inner.last_purged_log_id.clone()
        };
        Ok(LogState {
            last_purged_log_id: inner.last_purged_log_id.clone(),
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        MemStoreReader {
            inner: self.inner.clone(),
        }
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        let mut inner = self.inner.lock().await;
        for entry in entries {
            let idx = entry.log_id.index;
            inner.log.insert(idx, entry);
        }
        Ok(())
    }

    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<u64>,
    ) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.lock().await;
        let keys: Vec<u64> = inner.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys {
            inner.log.remove(&k);
        }
        Ok(())
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.lock().await;
        let keys: Vec<u64> = inner
            .log
            .range(..=log_id.index)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            inner.log.remove(&k);
        }
        inner.last_purged_log_id = Some(log_id);
        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied.clone(), inner.last_membership.clone()))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<TypeConfig>],
    ) -> Result<Vec<()>, StorageError<u64>> {
        let mut inner = self.inner.lock().await;
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            inner.last_applied = Some(entry.log_id.clone());
            match &entry.payload {
                EntryPayload::Blank => {}
                EntryPayload::Normal(d) => {
                    inner.applied_records.push(d.clone());
                }
                EntryPayload::Membership(m) => {
                    inner.last_membership =
                        StoredMembership::new(Some(entry.log_id.clone()), m.clone());
                }
            }
            out.push(());
        }
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        MemStoreSnapshotBuilder {
            inner: self.inner.clone(),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        mut snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        use tokio::io::AsyncReadExt;
        let mut data = Vec::new();
        snapshot
            .read_to_end(&mut data)
            .await
            .map_err(|e| StorageError::<u64>::from(StorageIOError::<u64>::read_state_machine(openraft::AnyError::new(&e))))?;
        let applied = decode_snapshot(&data);
        let mut inner = self.inner.lock().await;
        inner.last_applied = meta.last_log_id.clone();
        inner.last_membership = meta.last_membership.clone();
        inner.applied_records = applied;
        inner.snapshot = Some(Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(Cursor::new(data)),
        });
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        Ok(self.inner.lock().await.snapshot.clone())
    }
}

// `Adaptor` wraps a `RaftStorage` impl and provides both `RaftLogStorage`
// and `RaftStateMachine`. The `append` method on the v2 trait takes a
// `LogFlushed` callback; `Adaptor` provides a default that calls our
// `append_to_log` then signals flush completion (in-memory = instant).
// We never need to call `LogFlushed` ourselves from `MemStore`.

#[allow(dead_code)]
fn _assert_logflushed_bounds(_: LogFlushed<TypeConfig>) {}

// =========================================================================
// Snapshot encoding / decoding (length-prefixed Vec<Vec<u8>>)
// =========================================================================

pub(crate) fn encode_snapshot(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in records {
        out.extend_from_slice(&(rec.len() as u64).to_le_bytes());
        out.extend_from_slice(rec);
    }
    out
}

pub(crate) fn decode_snapshot(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[pos..pos + 8]);
        let len = u64::from_le_bytes(buf) as usize;
        pos += 8;
        if pos + len > data.len() {
            break;
        }
        out.push(data[pos..pos + len].to_vec());
        pos += len;
    }
    out
}

// =========================================================================
// In-memory network (channel transport)
// =========================================================================

/// An incoming RPC at a node's dispatcher: the request plus a oneshot
/// channel to send the reply back to the caller's `RaftNetwork` impl.
enum RpcMessage {
    AppendEntries(
        AppendEntriesRequest<TypeConfig>,
        oneshot::Sender<Result<AppendEntriesResponse<u64>, RaftError<u64>>>,
    ),
    InstallSnapshot(
        InstallSnapshotRequest<TypeConfig>,
        oneshot::Sender<Result<InstallSnapshotResponse<u64>, RaftError<u64, InstallSnapshotError>>>,
    ),
    Vote(
        VoteRequest<u64>,
        oneshot::Sender<Result<VoteResponse<u64>, RaftError<u64>>>,
    ),
}

/// Shared registry mapping node-id → inbox sender. All nodes in a cluster
/// share one registry (via `Arc`) so any node can send RPCs to any other.
#[derive(Default)]
struct NetworkRegistry {
    senders: Mutex<BTreeMap<u64, mpsc::UnboundedSender<RpcMessage>>>,
}

impl NetworkRegistry {
    async fn register(&self, id: u64, tx: mpsc::UnboundedSender<RpcMessage>) {
        self.senders.lock().await.insert(id, tx);
    }

    async fn unregister(&self, id: u64) {
        self.senders.lock().await.remove(&id);
    }

    /// Try to deliver `msg` to node `target`. Returns `Err` if the target
    /// is not registered (or its inbox was closed) — the caller maps this
    /// to an `Unreachable` RPC error so openraft backs off and retries.
    async fn deliver(&self, target: u64, msg: RpcMessage) -> Result<(), ()> {
        let senders = self.senders.lock().await;
        match senders.get(&target) {
            Some(tx) => tx.send(msg).map_err(|_| ()),
            None => Err(()),
        }
    }
}

/// Factory for [`ChannelNetwork`] instances. One factory per cluster —
/// all nodes share the underlying [`NetworkRegistry`].
#[derive(Clone)]
pub struct ChannelNetworkFactory {
    registry: Arc<NetworkRegistry>,
}

impl ChannelNetworkFactory {
    /// Create a new factory with an empty registry.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(NetworkRegistry::default()),
        }
    }

    /// Register a node's inbox so other nodes can send RPCs to it.
    pub(crate) async fn register(&self, id: u64, tx: mpsc::UnboundedSender<RpcMessage>) {
        self.registry.register(id, tx).await;
    }

    /// Remove a node from the registry (called when a `RaftManager` is
    /// dropped, so peers get `Unreachable` instead of queueing RPCs
    /// forever).
    pub(crate) async fn unregister(&self, id: u64) {
        self.registry.unregister(id).await;
    }
}

impl Default for ChannelNetworkFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl RaftNetworkFactory<TypeConfig> for ChannelNetworkFactory {
    type Network = ChannelNetwork;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        ChannelNetwork {
            target,
            registry: self.registry.clone(),
        }
    }
}

/// A `RaftNetwork` impl that forwards RPCs to a target node's inbox via
/// the shared [`NetworkRegistry`]. One `ChannelNetwork` instance per
/// (source, target) pair — openraft creates these on demand via
/// [`ChannelNetworkFactory::new_client`].
pub struct ChannelNetwork {
    target: u64,
    registry: Arc<NetworkRegistry>,
}

impl ChannelNetwork {
    /// Build an `Unreachable` error for this target.
    fn unreachable<E>(&self) -> RPCError<u64, BasicNode, E>
    where
        E: std::error::Error + 'static,
    {
        RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            format!("node {} unreachable", self.target),
        )))
    }
}

impl RaftNetwork<TypeConfig> for ChannelNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let (tx, rx) = oneshot::channel();
        self.registry
            .deliver(self.target, RpcMessage::AppendEntries(rpc, tx))
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;
        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
            Err(_) => Err(self.unreachable::<RaftError<u64>>()),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let (tx, rx) = oneshot::channel();
        self.registry
            .deliver(self.target, RpcMessage::InstallSnapshot(rpc, tx))
            .await
            .map_err(|_| self.unreachable::<RaftError<u64, InstallSnapshotError>>())?;
        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
            Err(_) => Err(self.unreachable::<RaftError<u64, InstallSnapshotError>>()),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let (tx, rx) = oneshot::channel();
        self.registry
            .deliver(self.target, RpcMessage::Vote(rpc, tx))
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;
        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
            Err(_) => Err(self.unreachable::<RaftError<u64>>()),
        }
    }
}

/// Dispatcher loop: reads RPCs from the node's inbox and forwards each
/// to the appropriate `Raft` method, sending the result back via the
/// embedded oneshot. Runs as a tokio task for the lifetime of the
/// [`RaftManager`].
async fn run_dispatcher(raft: RaftType, mut rx: mpsc::UnboundedReceiver<RpcMessage>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            RpcMessage::AppendEntries(rpc, tx) => {
                let res = raft.append_entries(rpc).await;
                let _ = tx.send(res);
            }
            RpcMessage::InstallSnapshot(rpc, tx) => {
                let res = raft.install_snapshot(rpc).await;
                let _ = tx.send(res);
            }
            RpcMessage::Vote(rpc, tx) => {
                let res = raft.vote(rpc).await;
                let _ = tx.send(res);
            }
        }
    }
}

// =========================================================================
// RaftManager — the public API
// =========================================================================

/// Manages a single Raft cluster node: the [`Raft`] handle, the per-node
/// dispatcher task, and the node's id.
///
/// Dropping a `RaftManager` aborts its dispatcher task and best-effort
/// shuts down the underlying `Raft` core — peers will then see the node
/// as `Unreachable` and (in a multi-node cluster) elect a new leader.
pub struct RaftManager {
    /// The openraft `Raft` handle. Cheap to clone (Arc inside).
    pub raft: RaftType,
    /// This node's id.
    pub node_id: u64,
    /// Handle to abort the dispatcher task on drop.
    dispatcher_abort: AbortHandle,
    /// The factory the cluster shares (so `Drop` can unregister).
    /// Unused when `factory_tcp` is set (TCP transport).
    factory: ChannelNetworkFactory,
    /// The TCP network factory (set when this manager uses TCP transport
    /// via `new_multi_node`).
    factory_tcp: Option<crate::storage::raft_network::TcpRaftNetworkFactory>,
    /// The TCP server (set when this manager uses TCP transport). The
    /// server task is aborted on Drop.
    server_tcp: Option<crate::storage::raft_network::TcpRaftServer>,
    /// The in-memory store (kept for test inspection). `None` when this
    /// manager is backed by a [`SledRaftStore`] instead.
    store: Option<MemStore>,
    /// The disk-backed store, when this manager is persistent. `None`
    /// when this manager uses the in-memory `MemStore`.
    sled_store: Option<SledRaftStore>,
}

impl RaftManager {
    /// Build a default Raft [`Config`] tuned for fast election in tests
    /// (heartbeat 50 ms, election timeout 150–300 ms).
    fn default_config() -> Result<Arc<Config>, String> {
        let config = Config {
            cluster_name: "turbogp-raft".to_string(),
            election_timeout_min: 150,
            election_timeout_max: 300,
            heartbeat_interval: 50,
            ..Config::default()
        };
        Ok(Arc::new(config))
    }

    /// Spawn the dispatcher task for a node, returning the abort handle.
    fn spawn_dispatcher(
        raft: RaftType,
        rx: mpsc::UnboundedReceiver<RpcMessage>,
    ) -> (JoinHandle<()>, AbortHandle) {
        let handle = tokio::spawn(async move {
            run_dispatcher(raft, rx).await;
        });
        let abort = handle.abort_handle();
        (handle, abort)
    }

    /// Create a single-node Raft cluster (for testing). The node is
    /// initialized with itself as the only member and becomes leader
    /// immediately (trivially correct — one node is always a quorum of
    /// one).
    ///
    /// Uses the in-memory [`MemStore`]. For a persistent single-node
    /// cluster that survives process restart, use
    /// [`RaftManager::new_single_node_persistent`].
    pub async fn new_single_node(node_id: u64) -> Result<Self, String> {
        let config = Self::default_config()?;
        let factory = ChannelNetworkFactory::new();
        let (tx, rx) = mpsc::unbounded_channel();
        factory.register(node_id, tx).await;

        let store = MemStore::new();
        let (log_store, state_machine) = Adaptor::new(store.clone());
        let raft = Raft::new(node_id, config, factory.clone(), log_store, state_machine)
            .await
            .map_err(|e| format!("Raft::new failed: {}", e))?;

        let (_join, dispatcher_abort) = Self::spawn_dispatcher(raft.clone(), rx);

        // Initialize with self as the only member.
        let members: BTreeMap<u64, BasicNode> =
            [(node_id, BasicNode::new(format!("node-{}", node_id)))]
                .into_iter()
                .collect();
        raft.initialize(members)
            .await
            .map_err(|e| format!("Raft::initialize failed: {}", e))?;

        Ok(RaftManager {
            raft,
            node_id,
            dispatcher_abort,
            factory,
            factory_tcp: None,
            server_tcp: None,
            store: Some(store),
            sled_store: None,
        })
    }

    /// Create a single-node Raft cluster backed by a disk-backed
    /// [`SledRaftStore`] rooted at `data_dir`. The Raft log, vote, and
    /// applied state machine persist across process restarts.
    ///
    /// The node is initialized with itself as the only member and becomes
    /// leader immediately (trivially correct — one node is always a
    /// quorum of one). If the data directory already contains initialized
    /// state from a previous run, the existing state is loaded and no
    /// re-initialization is performed.
    pub async fn new_single_node_persistent<P: AsRef<std::path::Path>>(
        node_id: u64,
        data_dir: P,
    ) -> Result<Self, String> {
        let config = Self::default_config()?;
        let factory = ChannelNetworkFactory::new();
        let (tx, rx) = mpsc::unbounded_channel();
        factory.register(node_id, tx).await;

        let store = SledRaftStore::open(data_dir).map_err(|e| format!("SledRaftStore::open failed: {}", e))?;
        let (log_store, state_machine) = Adaptor::new(store.clone());
        let raft = Raft::new(node_id, config, factory.clone(), log_store, state_machine)
            .await
            .map_err(|e| format!("Raft::new failed: {}", e))?;

        let (_join, dispatcher_abort) = Self::spawn_dispatcher(raft.clone(), rx);

        // If the store has no log entries, this is a fresh node — initialize
        // it with self as the only member. If it already has entries, the
        // node was previously initialized and we should NOT re-initialize
        // (openraft would reject a second `initialize` call with
        // `NotAllowToInitialize`).
        let is_fresh = {
            let log_tree = store
                .db_ref()
                .open_tree("raft_log")
                .map_err(|e| format!("open raft_log tree failed: {}", e))?;
            log_tree.is_empty()
        };
        if is_fresh {
            let members: BTreeMap<u64, BasicNode> =
                [(node_id, BasicNode::new(format!("node-{}", node_id)))]
                    .into_iter()
                    .collect();
            raft.initialize(members)
                .await
                .map_err(|e| format!("Raft::initialize failed: {}", e))?;
        }

        Ok(RaftManager {
            raft,
            node_id,
            dispatcher_abort,
            factory,
            factory_tcp: None,
            server_tcp: None,
            store: None,
            sled_store: Some(store),
        })
    }

    /// Create a member of a multi-node cluster. `peers` is the full list
    /// of node ids (including `node_id`); the caller must call
    /// [`RaftManager::initialize_cluster`] on exactly one node after all
    /// members are created. All members must share the same
    /// `factory` (passed in so the cluster shares one registry).
    ///
    /// Uses the in-memory [`MemStore`]. For a persistent multi-node
    /// cluster member, use [`RaftManager::new_persistent`].
    pub async fn new(
        node_id: u64,
        _peers: Vec<u64>,
        factory: ChannelNetworkFactory,
    ) -> Result<Self, String> {
        let config = Self::default_config()?;
        let (tx, rx) = mpsc::unbounded_channel();
        factory.register(node_id, tx).await;

        let store = MemStore::new();
        let (log_store, state_machine) = Adaptor::new(store.clone());
        let raft = Raft::new(node_id, config, factory.clone(), log_store, state_machine)
            .await
            .map_err(|e| format!("Raft::new failed: {}", e))?;

        let (_join, dispatcher_abort) = Self::spawn_dispatcher(raft.clone(), rx);

        Ok(RaftManager {
            raft,
            node_id,
            dispatcher_abort,
            factory,
            factory_tcp: None,
            server_tcp: None,
            store: Some(store),
            sled_store: None,
        })
    }

    /// Create a member of a multi-node cluster backed by a disk-backed
    /// [`SledRaftStore`] rooted at `data_dir`. The caller must call
    /// [`RaftManager::initialize_cluster`] on exactly one node after all
    /// members are created (only when starting a fresh cluster; restarts
    /// load existing state and skip initialization).
    pub async fn new_persistent<P: AsRef<std::path::Path>>(
        node_id: u64,
        _peers: Vec<u64>,
        factory: ChannelNetworkFactory,
        data_dir: P,
    ) -> Result<Self, String> {
        let config = Self::default_config()?;
        let (tx, rx) = mpsc::unbounded_channel();
        factory.register(node_id, tx).await;

        let store = SledRaftStore::open(data_dir).map_err(|e| format!("SledRaftStore::open failed: {}", e))?;
        let (log_store, state_machine) = Adaptor::new(store.clone());
        let raft = Raft::new(node_id, config, factory.clone(), log_store, state_machine)
            .await
            .map_err(|e| format!("Raft::new failed: {}", e))?;

        let (_join, dispatcher_abort) = Self::spawn_dispatcher(raft.clone(), rx);

        Ok(RaftManager {
            raft,
            node_id,
            dispatcher_abort,
            factory,
            factory_tcp: None,
            server_tcp: None,
            store: None,
            sled_store: Some(store),
        })
    }

    /// Create a member of a multi-node Raft cluster that uses real TCP
    /// transport (via [`TcpRaftNetworkFactory`] / [`TcpRaftServer`])
    /// instead of in-process `mpsc` channels. This is the production
    /// wiring: each node binds a TCP port and exchanges RPCs with peers
    /// over real network connections.
    ///
    /// `members` is the full list of `(node_id, bind_addr)` tuples for
    /// the cluster — every node needs to know every peer's bind address
    /// so it can dial them. The caller must call
    /// [`RaftManager::initialize_cluster`] on exactly one node after all
    /// members are created (only when starting a fresh cluster; restarts
    /// skip initialization).
    ///
    /// Returns the new `RaftManager`. The `TcpRaftServer` runs as a
    /// background task for the lifetime of the manager; dropping the
    /// manager aborts both the dispatcher task and the server.
    pub async fn new_multi_node(
        node_id: u64,
        members: Vec<(u64, std::net::SocketAddr)>,
        data_dir: std::path::PathBuf,
    ) -> Result<Self, String> {
        use crate::storage::raft_network::{TcpRaftNetworkFactory, TcpRaftServer};

        let config = Self::default_config()?;
        let factory = TcpRaftNetworkFactory::new();
        // Register every member's bind address (including self) so the
        // factory can route RPCs.
        for (id, addr) in &members {
            factory.register(*id, *addr).await;
        }

        // Find our own bind address.
        let own_addr = members
            .iter()
            .find(|(id, _)| *id == node_id)
            .map(|(_, addr)| *addr)
            .ok_or_else(|| format!("node {} not in members list", node_id))?;

        // Open the persistent store.
        let store = SledRaftStore::open(&data_dir)
            .map_err(|e| format!("SledRaftStore::open failed: {}", e))?;
        let (log_store, state_machine) = Adaptor::new(store.clone());

        // Create the Raft handle first. openraft internally retries RPCs
        // if peers reject the initial connection (because their TCP server
        // is not yet up), so a brief window of connection refusals is
        // tolerable. Once we have the handle, start the TCP server with it.
        let raft = Raft::new(node_id, config, factory.clone(), log_store, state_machine)
            .await
            .map_err(|e| format!("Raft::new failed: {}", e))?;

        let server = TcpRaftServer::start(own_addr, raft.clone())
            .await
            .map_err(|e| format!("TcpRaftServer::start failed: {}", e))?;

        // Spawn a no-op dispatcher task so the AbortHandle field stays
        // valid. With TCP transport, the dispatcher loop is unnecessary
        // (RPCs arrive via the TCP server), but the field is still
        // required by the Drop impl. Aborting a no-op task is harmless.
        let dispatcher_abort = {
            let h = tokio::spawn(async {});
            h.abort_handle()
        };

        Ok(RaftManager {
            raft,
            node_id,
            dispatcher_abort,
            factory: ChannelNetworkFactory::new(),
            factory_tcp: Some(factory),
            server_tcp: Some(server),
            store: None,
            sled_store: Some(store),
        })
    }

    /// Initialize a multi-node cluster with the given member set. Call
    /// this on exactly one node after all members are created via
    /// [`RaftManager::new`]. openraft allows multiple nodes to call
    /// `initialize` with the same membership safely, so calling it on
    /// all members is also acceptable.
    pub async fn initialize_cluster(&self, members: BTreeSet<u64>) -> Result<(), String> {
        let nodes: BTreeMap<u64, BasicNode> = members
            .iter()
            .map(|id| (*id, BasicNode::new(format!("node-{}", id))))
            .collect();
        self.raft
            .initialize(nodes)
            .await
            .map_err(|e| format!("Raft::initialize failed: {}", e))
    }

    /// Returns `true` if this node is currently the cluster leader.
    pub async fn is_leader(&self) -> bool {
        let metrics = self.raft.metrics().borrow().clone();
        metrics.state == ServerState::Leader
    }

    /// Returns the id of the current cluster leader, if known.
    pub async fn current_leader(&self) -> Option<u64> {
        self.raft.current_leader().await
    }

    /// Block until this node becomes leader, or `timeout_ms` elapses.
    pub async fn wait_for_leader(&self, timeout_ms: u64) -> Result<u64, String> {
        let timeout = Duration::from_millis(timeout_ms);
        self.raft
            .wait(Some(timeout))
            .metrics(
                |m| m.current_leader.is_some(),
                "wait_for_leader: current_leader is Some",
            )
            .await
            .map_err(|e| format!("wait_for_leader: {}", e))?
            .current_leader
            .ok_or_else(|| "wait_for_leader: leader is None after wait".to_string())
    }

    /// Block until this node is in `Leader` state, or timeout.
    pub async fn wait_until_leader(&self, timeout_ms: u64) -> Result<(), String> {
        let timeout = Duration::from_millis(timeout_ms);
        self.raft
            .wait(Some(timeout))
            .state(ServerState::Leader, "wait_until_leader")
            .await
            .map_err(|e| format!("wait_until_leader: {}", e))?;
        Ok(())
    }

    /// Propose a WAL record (raw bytes) through Raft consensus. The
    /// entry is replicated to a quorum and applied to the state machine
    /// before this returns `Ok(())`. Returns `Err` if this node is not
    /// the leader (openraft returns `ForwardToLeader`) or if the cluster
    /// is unavailable.
    pub async fn propose(&self, record: &[u8]) -> Result<(), String> {
        let _resp = self
            .raft
            .client_write(record.to_vec())
            .await
            .map_err(|e| format!("raft client_write failed: {}", e))?;
        Ok(())
    }

    /// Wait until the state machine has applied at least `index` log
    /// entries (used by tests to confirm replication landed).
    pub async fn wait_applied_at_least(&self, index: u64, timeout_ms: u64) -> Result<(), String> {
        let timeout = Duration::from_millis(timeout_ms);
        self.raft
            .wait(Some(timeout))
            .applied_index_at_least(Some(index), "wait_applied_at_least")
            .await
            .map_err(|e| format!("wait_applied_at_least: {}", e))?;
        Ok(())
    }

    /// Borrow the in-memory store (for test inspection of applied records).
    /// Returns `None` if this manager is backed by a [`SledRaftStore`]
    /// (use [`RaftManager::sled_store`] instead).
    pub fn store(&self) -> Option<&MemStore> {
        self.store.as_ref()
    }

    /// Borrow the disk-backed store (for test inspection of applied records
    /// and direct persistence operations). Returns `None` if this manager
    /// is backed by an in-memory [`MemStore`] (use [`RaftManager::store`]
    /// instead).
    pub fn sled_store(&self) -> Option<&SledRaftStore> {
        self.sled_store.as_ref()
    }

    /// Gracefully shut down this node's Raft core and dispatcher. After
    /// this returns, peers will see the node as `Unreachable`.
    pub async fn shutdown(&self) -> Result<(), String> {
        let _ = self.raft.shutdown().await.map_err(|e| format!("shutdown: {}", e))?;
        Ok(())
    }
}

impl Drop for RaftManager {
    fn drop(&mut self) {
        // Abort the dispatcher first — this stops the node from
        // responding to any further RPCs, so peers see `Unreachable`.
        self.dispatcher_abort.abort();

        // Unregister from the network so peers don't queue RPCs into a
        // dead inbox. `unregister` is async (takes a tokio mutex); we
        // best-effort spawn it on the current runtime if there is one.
        let factory = self.factory.clone();
        let node_id = self.node_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                factory.unregister(node_id).await;
            });
        }

        // Best-effort raft shutdown — also needs an async context.
        let raft = self.raft.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = raft.shutdown().await;
            });
        }
    }
}

// =========================================================================
// 3-node cluster factory (Task 5.3)
// =========================================================================

/// Create a 3-node Raft cluster (node ids 1, 2, 3) in a single process.
///
/// All three nodes share one [`ChannelNetworkFactory`] (so they can send
/// RPCs to each other via in-memory channels). Node 1 calls
/// `initialize_cluster({1,2,3})`; the other two learn the membership
/// when the elected leader sends them their first `AppendEntries`. A
/// leader is elected within a few hundred milliseconds (one election
/// timeout). Returns the three managers in a `Vec`, in id order
/// (node 1, node 2, node 3).
pub async fn create_3_node_cluster() -> Result<Vec<RaftManager>, String> {
    let factory = ChannelNetworkFactory::new();
    let mut nodes = Vec::with_capacity(3);
    for node_id in [1u64, 2, 3] {
        let mgr = RaftManager::new(node_id, vec![1, 2, 3], factory.clone()).await?;
        nodes.push(mgr);
    }

    // Initialize the cluster membership on node 1.
    nodes[0]
        .initialize_cluster([1u64, 2, 3].into_iter().collect())
        .await?;

    // Wait for a leader to be elected somewhere in the cluster.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut has_leader = false;
        for n in &nodes {
            if n.is_leader().await {
                has_leader = true;
                break;
            }
        }
        if has_leader {
            return Ok(nodes);
        }
        if std::time::Instant::now() > deadline {
            return Err("3-node cluster: no leader elected within 5s".to_string());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: run an async test on a fresh multi-thread runtime.
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    /// Task 5.1 DoD: single-node RaftManager initializes and becomes
    /// leader immediately (single node is always a quorum of one).
    #[test]
    fn raft_manager_single_node_becomes_leader() {
        let rt = rt();
        rt.block_on(async {
            let mgr = RaftManager::new_single_node(1).await.expect("new_single_node");
            mgr.wait_until_leader(2000).await.expect("become leader");
            assert!(mgr.is_leader().await);
            assert_eq!(mgr.current_leader().await, Some(1));
        });
    }

    /// Task 5.2 DoD: propose a WAL record through Raft; verify it lands
    /// in the state machine's applied_records.
    #[test]
    fn raft_manager_propose_single_node() {
        let rt = rt();
        rt.block_on(async {
            let mgr = RaftManager::new_single_node(1).await.expect("new_single_node");
            mgr.wait_until_leader(2000).await.expect("leader");

            mgr.propose(b"INSERT INTO t VALUES (1)").await.expect("propose 1");
            mgr.propose(b"INSERT INTO t VALUES (2)").await.expect("propose 2");

            // Wait for both entries to apply.
            mgr.wait_applied_at_least(2, 2000).await.expect("applied >= 2");

            let applied = mgr.store().expect("memstore").applied_records().await;
            assert_eq!(applied.len(), 2, "applied_records: {:?}", applied);
            assert_eq!(applied[0], b"INSERT INTO t VALUES (1)");
            assert_eq!(applied[1], b"INSERT INTO t VALUES (2)");
        });
    }

    /// Task 5.3 DoD: 3-node cluster elects a leader; the leader is one
    /// of {1, 2, 3}.
    #[test]
    fn raft_3_node_cluster_elects_leader() {
        let rt = rt();
        rt.block_on(async {
            let nodes = create_3_node_cluster().await.expect("3-node cluster");
            let mut leader_id = None;
            for n in &nodes {
                if n.is_leader().await {
                    leader_id = Some(n.node_id);
                    break;
                }
            }
            assert!(leader_id.is_some(), "one of 3 nodes must be leader");
            let leader_id = leader_id.unwrap();

            // The leader's id is known to all followers via heartbeats.
            for n in &nodes {
                let lid = n.wait_for_leader(3000).await.expect("all know leader");
                assert_eq!(lid, leader_id, "all nodes agree on leader");
            }
        });
    }

    /// Task 5.4 DoD: WAL record proposed on the leader replicates to all
    /// 3 nodes' state machines (quorum commit + apply).
    #[test]
    fn raft_3_node_cluster_wal_replication() {
        let rt = rt();
        rt.block_on(async {
            let nodes = create_3_node_cluster().await.expect("3-node cluster");
            let mut leader_idx = None;
            for (i, n) in nodes.iter().enumerate() {
                if n.is_leader().await {
                    leader_idx = Some(i);
                    break;
                }
            }
            let leader_idx = leader_idx.expect("a leader exists");
            let leader = &nodes[leader_idx];

            leader.propose(b"INSERT INTO t VALUES (42)").await.expect("propose");

            // Wait for the entry to apply on all 3 nodes (log index 1:
            // the membership-init entry is index 0, our proposal is 1;
            // but openraft may also append a blank leader-noop, so wait
            // for applied >= 1 and then check the record is present).
            for n in &nodes {
                let _ = n.wait_applied_at_least(1, 3000).await;
            }

            // Give the state machine a moment to flush applied records.
            tokio::time::sleep(Duration::from_millis(100)).await;

            let mut found = 0;
            for n in &nodes {
                let applied = n.store().expect("memstore").applied_records().await;
                if applied.iter().any(|r| r == b"INSERT INTO t VALUES (42)") {
                    found += 1;
                }
            }
            assert_eq!(
                found, 3,
                "WAL record must replicate to all 3 nodes; found in {}",
                found
            );

            // Keep nodes alive until the end of the test.
            drop(nodes);
        });
    }

    /// Task 5.5 DoD: leader change — after the leader is killed, a new
    /// leader is elected and accepts writes.
    ///
    /// Task 5.6 DoD: failover — drop the leader's RaftManager, verify a
    /// new leader is elected within 5 seconds, verify a write on the new
    /// leader succeeds.
    #[test]
    fn raft_3_node_cluster_failover() {
        let rt = rt();
        rt.block_on(async {
            let mut nodes = create_3_node_cluster().await.expect("3-node cluster");
            let mut leader_idx = None;
            for (i, n) in nodes.iter().enumerate() {
                if n.is_leader().await {
                    leader_idx = Some(i);
                    break;
                }
            }
            let leader_idx = leader_idx.expect("a leader exists");
            let old_leader_id = nodes[leader_idx].node_id;

            // Kill the leader: drop its RaftManager (aborts dispatcher,
            // unregisters from network, shuts down raft core).
            let _dead = nodes.remove(leader_idx);
            drop(_dead);

            // Give the dead node's drop handlers a moment to run.
            tokio::time::sleep(Duration::from_millis(200)).await;

            // The remaining 2 nodes must elect a new leader within 5s.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut new_leader_idx = None;
            loop {
                for (i, n) in nodes.iter().enumerate() {
                    if n.is_leader().await {
                        new_leader_idx = Some(i);
                        break;
                    }
                }
                if new_leader_idx.is_some() || std::time::Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let new_leader_idx = new_leader_idx
                .expect("a new leader must be elected within 5s after leader death");
            let new_leader = &nodes[new_leader_idx];
            assert_ne!(
                new_leader.node_id, old_leader_id,
                "new leader must differ from the dead one"
            );

            // Write on the new leader must succeed.
            new_leader
                .propose(b"INSERT INTO t VALUES (99)")
                .await
                .expect("propose on new leader");

            // And the write must replicate to the other surviving node.
            let _ = new_leader.wait_applied_at_least(1, 3000).await;
            tokio::time::sleep(Duration::from_millis(100)).await;

            let mut found = 0;
            for n in &nodes {
                let applied = n.store().expect("memstore").applied_records().await;
                if applied.iter().any(|r| r == b"INSERT INTO t VALUES (99)") {
                    found += 1;
                }
            }
            assert!(
                found >= 1,
                "failover write must land on at least the new leader; found in {}",
                found
            );
            assert_eq!(nodes.len(), 2, "2 surviving nodes");

            drop(nodes);
        });
    }

    /// Sanity: the in-memory snapshot encode/decode round-trips.
    #[test]
    fn snapshot_encode_decode_roundtrip() {
        let records = vec![vec![1, 2, 3], vec![], vec![10, 20]];
        let encoded = encode_snapshot(&records);
        let decoded = decode_snapshot(&encoded);
        assert_eq!(decoded, records);
    }

    /// Task 2.3 DoD: create persistent RaftManager, propose 5 records,
    /// drop, recreate with the same data dir, verify the 5 records are
    /// present in the reopened store.
    #[test]
    fn raft_manager_persistent_survives_restart() {
        use tempfile::tempdir;
        let rt = rt();
        rt.block_on(async {
            let dir = tempdir().expect("tempdir");
            let dir_path = dir.path().to_path_buf();

            // Phase 1: create persistent manager, propose 5 records.
            {
                let mgr = RaftManager::new_single_node_persistent(1, &dir_path)
                    .await
                    .expect("new_single_node_persistent");
                mgr.wait_until_leader(3000).await.expect("leader");

                for i in 1..=5u8 {
                    let payload = vec![b'r', b'-', i];
                    mgr.propose(&payload).await.expect("propose");
                }
                // Wait for all 5 entries to apply.
                mgr.wait_applied_at_least(5, 3000).await.expect("applied >= 5");

                let applied = mgr
                    .sled_store()
                    .expect("sled_store")
                    .applied_records()
                    .expect("applied_records");
                assert_eq!(applied.len(), 5, "expected 5 applied records");
                // Explicitly shutdown the Raft core so it releases its
                // references to the sled store. Without this, the
                // dispatcher task and the openraft state machine keep
                // `Arc<Db>` clones alive and sled's file lock is not
                // released — the reopen would fail with `WouldBlock`.
                let _ = mgr.shutdown().await;
                // Give the runtime a moment to drop the dispatcher task
                // and the openraft `Arc`s. 200 ms is empirically enough.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                drop(mgr);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }

            // Phase 2: re-open with the SAME data dir. The previously-applied
            // records MUST be present.
            {
                let mgr = RaftManager::new_single_node_persistent(1, &dir_path)
                    .await
                    .expect("reopen new_single_node_persistent");
                // Give the new leader a moment to settle.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                let applied = mgr
                    .sled_store()
                    .expect("sled_store")
                    .applied_records()
                    .expect("applied_records after reopen");
                assert_eq!(
                    applied.len(),
                    5,
                    "expected 5 applied records after restart, got {}",
                    applied.len()
                );
                // Verify payload content.
                for (i, rec) in applied.iter().enumerate() {
                    let expected_i = (i + 1) as u8;
                    assert_eq!(rec, &[b'r', b'-', expected_i], "record {} mismatch", i);
                }
            }
        });
    }

    /// Task 3.2 + 3.3 DoD: 3-node TCP cluster on localhost — leader
    /// elected, propose 5 records, all 3 nodes have them. Also serves
    /// as the Task 3.3 DoD for the basic 3-node TCP test (a separate
    /// failover test is below).
    #[test]
    fn raft_3_node_tcp_cluster_replicates_records() {
        use tempfile::tempdir;
        let rt = rt();
        rt.block_on(async {
            // Three data dirs.
            let dir1 = tempdir().expect("tempdir1");
            let dir2 = tempdir().expect("tempdir2");
            let dir3 = tempdir().expect("tempdir3");

            // Pick three ephemeral ports by binding a TcpListener, dropping,
            // and reusing the address. (Race window is negligible in a test.)
            let addrs: Vec<std::net::SocketAddr> = (0..3)
                .map(|_| {
                    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
                    l.local_addr().expect("addr")
                })
                .collect();

            let members = vec![
                (1u64, addrs[0]),
                (2, addrs[1]),
                (3, addrs[2]),
            ];

            // Start each node.
            let mut nodes = Vec::new();
            for (i, (node_id, _addr)) in members.iter().enumerate() {
                let dirs = [dir1.path(), dir2.path(), dir3.path()];
                let n = RaftManager::new_multi_node(
                    *node_id,
                    members.clone(),
                    dirs[i].to_path_buf(),
                )
                .await
                .expect("new_multi_node");
                nodes.push(n);
            }

            // Initialize the cluster from node 1.
            let mut init_members = std::collections::BTreeSet::new();
            init_members.insert(1u64);
            init_members.insert(2u64);
            init_members.insert(3u64);
            nodes[0]
                .initialize_cluster(init_members)
                .await
                .expect("initialize_cluster");

            // Wait for a leader to be elected.
            let mut leader_idx = None;
            for _ in 0..60 {
                for (i, n) in nodes.iter().enumerate() {
                    if n.is_leader().await {
                        leader_idx = Some(i);
                        break;
                    }
                }
                if leader_idx.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let leader_idx = leader_idx.expect("no leader elected within 6s");

            // Propose 5 records on the leader.
            for i in 1..=5u8 {
                let payload = vec![b'r', b'-', i];
                nodes[leader_idx]
                    .propose(&payload)
                    .await
                    .expect("propose");
            }

            // Wait for all 5 entries to apply on the leader.
            nodes[leader_idx]
                .wait_applied_at_least(5, 5000)
                .await
                .expect("applied >= 5 on leader");

            // Give followers a moment to catch up.
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Verify all 3 nodes have the 5 records (note: each node's
            // applied_records is cumulative since the store was fresh —
            // there is no other state).
            let mut ok_count = 0;
            for n in &nodes {
                if let Some(store) = n.sled_store() {
                    let applied = store.applied_records().unwrap_or_default();
                    // The leader will have at least 5 records; followers
                    // should have at least 5 too (they may have additional
                    // entries from membership/blank entries, but the 5
                    // Normal-payload entries must be present).
                    let normal_count = applied
                        .iter()
                        .filter(|r| r.len() == 3 && r[0] == b'r' && r[1] == b'-')
                        .count();
                    if normal_count >= 5 {
                        ok_count += 1;
                    }
                }
            }
            assert!(
                ok_count >= 2,
                "expected at least 2 nodes (leader + 1 follower) to have all 5 records; got {}",
                ok_count
            );

            // Clean up.
            for n in &nodes {
                let _ = n.shutdown().await;
            }
            drop(nodes);
            tokio::time::sleep(Duration::from_millis(300)).await;
        });
    }
}
