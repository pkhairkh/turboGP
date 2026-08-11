//! TCP transport for openraft RPCs.
//!
//! [`TcpRaftNetworkFactory`] and [`TcpRaftNetwork`] implement openraft's
//! `RaftNetworkFactory` + `RaftNetwork` traits over real `tokio::net::TcpStream`
//! connections. RPCs (AppendEntries, InstallSnapshot, Vote) are serialized
//! with [`bincode`] and framed with a 1-byte type tag + 4-byte length prefix.
//!
//! A [`TcpRaftServer`] task runs on each node: it accepts inbound TCP
//! connections, reads RPC requests, dispatches them to the node's
//! [`RaftType`] handle, and writes the serialized response back. The
//! server runs for the lifetime of the [`RaftManager`](super::raft::RaftManager).
//!
//! This replaces the in-process `mpsc` channel transport
//! ([`ChannelNetworkFactory`](super::raft::ChannelNetworkFactory)) so a
//! turboGP Raft cluster can span multiple machines.
//!
//! ## Wire format
//!
//! ```text
//! ┌─────────┬──────────────┬──────────────────┐
//! │ type u8 │ len u32 (LE) │ bincode payload  │
//! └─────────┴──────────────┴──────────────────┘
//! ```
//!
//! `type` is `1 = AppendEntries`, `2 = InstallSnapshot`, `3 = Vote`.
//! Responses use the same framing with the same type tag.

#![cfg(feature = "raft")]

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::error::{InstallSnapshotError, RaftError, RPCError, RemoteError, Unreachable};
use openraft::{BasicNode, Raft};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::timeout;

use crate::storage::raft::{RaftType, TypeConfig};

// =========================================================================
// Wire format constants
// =========================================================================

/// RPC type tag: AppendEntries.
const RPC_APPEND_ENTRIES: u8 = 1;
/// RPC type tag: InstallSnapshot.
const RPC_INSTALL_SNAPSHOT: u8 = 2;
/// RPC type tag: Vote.
const RPC_VOTE: u8 = 3;
/// Maximum RPC frame size (16 MiB — large enough for chunked InstallSnapshot).
const MAX_RPC_FRAME: u32 = 16 * 1024 * 1024;
/// Default per-RPC timeout if `RPCOption` does not specify one.
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Inbound RPC request — the server reads these off the wire and dispatches
/// them to the local [`RaftType`] handle.
#[derive(Debug)]
enum InboundRpc {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
    Vote(VoteRequest<u64>),
}

impl InboundRpc {
    /// Returns the 1-byte type tag for this RPC.
    fn type_tag(&self) -> u8 {
        match self {
            InboundRpc::AppendEntries(_) => RPC_APPEND_ENTRIES,
            InboundRpc::InstallSnapshot(_) => RPC_INSTALL_SNAPSHOT,
            InboundRpc::Vote(_) => RPC_VOTE,
        }
    }

    /// Serialize the RPC payload with bincode.
    fn serialize(&self) -> Result<Vec<u8>, bincode::Error> {
        match self {
            InboundRpc::AppendEntries(r) => bincode::serialize(r),
            InboundRpc::InstallSnapshot(r) => bincode::serialize(r),
            InboundRpc::Vote(r) => bincode::serialize(r),
        }
    }

    /// Deserialize an RPC of the given type tag from raw bytes.
    fn deserialize(tag: u8, bytes: &[u8]) -> Result<Self, WireError> {
        match tag {
            RPC_APPEND_ENTRIES => Ok(InboundRpc::AppendEntries(
                bincode::deserialize(bytes).map_err(WireError::Bincode)?,
            )),
            RPC_INSTALL_SNAPSHOT => Ok(InboundRpc::InstallSnapshot(
                bincode::deserialize(bytes).map_err(WireError::Bincode)?,
            )),
            RPC_VOTE => Ok(InboundRpc::Vote(
                bincode::deserialize(bytes).map_err(WireError::Bincode)?,
            )),
            other => Err(WireError::UnknownTag(other)),
        }
    }
}

/// Outbound RPC response — same wire framing as the request.
#[derive(Debug)]
enum OutboundResponse {
    AppendEntries(Result<AppendEntriesResponse<u64>, RaftError<u64>>),
    InstallSnapshot(Result<InstallSnapshotResponse<u64>, RaftError<u64, InstallSnapshotError>>),
    Vote(Result<VoteResponse<u64>, RaftError<u64>>),
}

impl OutboundResponse {
    /// Returns the 1-byte type tag matching the original request.
    fn type_tag(&self) -> u8 {
        match self {
            OutboundResponse::AppendEntries(_) => RPC_APPEND_ENTRIES,
            OutboundResponse::InstallSnapshot(_) => RPC_INSTALL_SNAPSHOT,
            OutboundResponse::Vote(_) => RPC_VOTE,
        }
    }

    /// Serialize the response payload with bincode.
    fn serialize(&self) -> Result<Vec<u8>, bincode::Error> {
        match self {
            OutboundResponse::AppendEntries(r) => bincode::serialize(r),
            OutboundResponse::InstallSnapshot(r) => bincode::serialize(r),
            OutboundResponse::Vote(r) => bincode::serialize(r),
        }
    }
}

/// Errors that can occur on the wire.
#[derive(Debug, thiserror::Error)]
enum WireError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("unknown rpc type tag: {0}")]
    UnknownTag(u8),
    #[error("frame too large: {0}")]
    TooLarge(u32),
    #[error("eof")]
    Eof,
}

// =========================================================================
// TcpRaftNetworkFactory
// =========================================================================

/// Factory for [`TcpRaftNetwork`] clients. Holds a map of
/// `node_id → socket_addr` so a client knows where to connect for any
/// target. The factory is `Clone + Send + Sync` so openraft can hold one
/// per node.
#[derive(Clone)]
pub struct TcpRaftNetworkFactory {
    /// Map of node_id → bind_addr (where each node listens for RPCs).
    /// Wrapped in `Arc<Mutex>` so new nodes can be added dynamically.
    addrs: Arc<Mutex<BTreeMap<u64, SocketAddr>>>,
}

impl TcpRaftNetworkFactory {
    /// Create a new factory with no registered nodes.
    pub fn new() -> Self {
        Self {
            addrs: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Register a node's TCP bind address. Other nodes will connect to
    /// this address when sending RPCs to `node_id`.
    pub async fn register(&self, node_id: u64, addr: SocketAddr) {
        self.addrs.lock().await.insert(node_id, addr);
    }

    /// Remove a node's registration (e.g. when the node leaves the cluster).
    pub async fn unregister(&self, node_id: u64) {
        self.addrs.lock().await.remove(&node_id);
    }

    /// Returns the bind address for a node, if registered.
    pub async fn addr_of(&self, node_id: u64) -> Option<SocketAddr> {
        self.addrs.lock().await.get(&node_id).copied()
    }
}

impl Default for TcpRaftNetworkFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl RaftNetworkFactory<TypeConfig> for TcpRaftNetworkFactory {
    type Network = TcpRaftNetwork;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        let addr = self.addr_of(target).await;
        TcpRaftNetwork {
            target,
            addr,
            factory: self.clone(),
        }
    }
}

/// A `RaftNetwork` impl that sends RPCs over a fresh TCP connection per
/// RPC. Each call opens a `TcpStream` to the target's bind address,
/// sends the framed request, and reads the framed response.
pub struct TcpRaftNetwork {
    target: u64,
    addr: Option<SocketAddr>,
    factory: TcpRaftNetworkFactory,
}

impl TcpRaftNetwork {
    /// Build an `Unreachable` error for this target.
    fn unreachable<E>(&self) -> RPCError<u64, BasicNode, E>
    where
        E: std::error::Error + 'static,
    {
        RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("node {} unreachable (no addr / connect failed)", self.target),
        )))
    }

    /// Resolve the target's address. If `self.addr` is None, look it up
    /// in the factory (the target may have been registered after this
    /// client was created).
    async fn resolve_addr(&mut self) -> Option<SocketAddr> {
        if self.addr.is_some() {
            return self.addr;
        }
        self.addr = self.factory.addr_of(self.target).await;
        self.addr
    }

    /// Send an RPC over a fresh TCP connection. Returns the framed
    /// response bytes (without the type tag) or an `Unreachable` error.
    async fn send_rpc(
        &mut self,
        rpc: &InboundRpc,
        option: &RPCOption,
    ) -> Result<Vec<u8>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let addr = match self.resolve_addr().await {
            Some(a) => a,
            None => return Err(self.unreachable::<RaftError<u64>>()),
        };

        let connect_timeout = option.hard_ttl();
        let stream_res = timeout(connect_timeout, TcpStream::connect(addr)).await;
        let mut stream = match stream_res {
            Ok(Ok(s)) => s,
            _ => return Err(self.unreachable::<RaftError<u64>>()),
        };

        // Serialize the payload.
        let payload = rpc.serialize().map_err(|_| self.unreachable::<RaftError<u64>>())?;
        let len = payload.len() as u32;
        if len > MAX_RPC_FRAME {
            return Err(self.unreachable::<RaftError<u64>>());
        }

        // Write the frame: tag + len + payload.
        let mut header = [0u8; 5];
        header[0] = rpc.type_tag();
        header[1..5].copy_from_slice(&len.to_le_bytes());
        stream
            .write_all(&header)
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;
        stream
            .write_all(&payload)
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;
        stream
            .flush()
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;

        // Read the response frame.
        let resp = read_frame(&mut stream, option.hard_ttl().as_millis() as u64)
            .await
            .map_err(|_| self.unreachable::<RaftError<u64>>())?;
        Ok(resp.1)
    }
}

impl RaftNetwork<TypeConfig> for TcpRaftNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let payload = self
            .send_rpc(&InboundRpc::AppendEntries(rpc), &option)
            .await?;
        let resp: Result<AppendEntriesResponse<u64>, RaftError<u64>> =
            bincode::deserialize(&payload).map_err(|_| self.unreachable::<RaftError<u64>>())?;
        match resp {
            Ok(r) => Ok(r),
            Err(e) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<InstallSnapshotResponse<u64>, RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>> {
        // For InstallSnapshot we use the same wire path as other RPCs; the
        // `RPCOption`'s hard_ttl already accounts for large payloads.
        // send_rpc returns `RPCError<u64, BasicNode, RaftError<u64>>`; convert
        // to the InstallSnapshot variant by extracting the Unreachable case
        // and re-wrapping it with the new error type.
        let payload = match self
            .send_rpc(&InboundRpc::InstallSnapshot(rpc), &option)
            .await
        {
            Ok(p) => p,
            Err(RPCError::Unreachable(u)) => return Err(RPCError::Unreachable(u)),
            Err(RPCError::Timeout(t)) => return Err(RPCError::Timeout(t)),
            Err(RPCError::PayloadTooLarge(p)) => return Err(RPCError::PayloadTooLarge(p)),
            Err(RPCError::RemoteError(_)) => {
                return Err(RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "remote error on install_snapshot",
                ))));
            }
            Err(e) => {
                return Err(RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e,
                ))));
            }
        };
        let resp: Result<InstallSnapshotResponse<u64>, RaftError<u64, InstallSnapshotError>> =
            bincode::deserialize(&payload).map_err(|_| {
                let u: RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>> =
                    RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "decode failure",
                    )));
                u
            })?;
        match resp {
            Ok(r) => Ok(r),
            Err(e) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let payload = self.send_rpc(&InboundRpc::Vote(rpc), &option).await?;
        let resp: Result<VoteResponse<u64>, RaftError<u64>> =
            bincode::deserialize(&payload).map_err(|_| self.unreachable::<RaftError<u64>>())?;
        match resp {
            Ok(r) => Ok(r),
            Err(e) => Err(RPCError::RemoteError(RemoteError::new(self.target, e))),
        }
    }
}

// =========================================================================
// TcpRaftServer — inbound RPC listener
// =========================================================================

/// A TCP server that listens on `bind_addr` for inbound openraft RPCs and
/// dispatches them to the local [`RaftType`] handle.
///
/// The server runs as a tokio task for the lifetime of the owning
/// [`RaftManager`](super::raft::RaftManager). Dropping the
/// `TcpRaftServer` handle aborts the task and the node stops responding.
pub struct TcpRaftServer {
    /// The actual bind address (useful when `0.0.0.0:0` was requested).
    pub local_addr: SocketAddr,
    abort: AbortHandle,
    _join: JoinHandle<()>,
}

impl TcpRaftServer {
    /// Start a TCP server bound to `bind_addr`. The server dispatches
    /// inbound RPCs to the given `RaftType` handle. Returns once the
    /// listener is bound and accepting.
    pub async fn start(bind_addr: SocketAddr, raft: RaftType) -> io::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let join = tokio::spawn(async move {
            Self::run(listener, raft).await;
        });
        let abort = join.abort_handle();
        Ok(Self {
            local_addr,
            abort,
            _join: join,
        })
    }

    /// The main accept loop: each new connection is handled in its own
    /// task. The task reads one RPC frame, dispatches it, and writes the
    /// response back, then closes the connection.
    async fn run(listener: TcpListener, raft: RaftType) {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let raft = raft.clone();
                    tokio::spawn(async move {
                        let _ = Self::handle_conn(stream, raft).await;
                    });
                }
                Err(_) => {
                    // Transient accept error; back off briefly.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Handle a single inbound TCP connection: read one RPC frame,
    /// dispatch it, write the response back, close.
    async fn handle_conn(mut stream: TcpStream, raft: RaftType) -> Result<(), WireError> {
        // Read the request frame.
        let (tag, payload) = read_frame(&mut stream, DEFAULT_RPC_TIMEOUT.as_millis() as u64).await?;
        let rpc = InboundRpc::deserialize(tag, &payload)?;

        // Dispatch to the local Raft handle.
        let response = match rpc {
            InboundRpc::AppendEntries(req) => {
                let res = raft.append_entries(req).await;
                OutboundResponse::AppendEntries(res)
            }
            InboundRpc::InstallSnapshot(req) => {
                let res = raft.install_snapshot(req).await;
                OutboundResponse::InstallSnapshot(res)
            }
            InboundRpc::Vote(req) => {
                let res = raft.vote(req).await;
                OutboundResponse::Vote(res)
            }
        };

        // Write the response frame.
        let resp_payload = response.serialize()?;
        let resp_len = resp_payload.len() as u32;
        let mut header = [0u8; 5];
        header[0] = response.type_tag();
        header[1..5].copy_from_slice(&resp_len.to_le_bytes());
        stream.write_all(&header).await?;
        stream.write_all(&resp_payload).await?;
        stream.flush().await?;
        Ok(())
    }
}

impl Drop for TcpRaftServer {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

// =========================================================================
// Framing helpers
// =========================================================================

/// Read a length-prefixed frame: 1-byte tag + 4-byte LE length + payload.
/// Returns `(tag, payload)` or an error.
async fn read_frame(
    stream: &mut TcpStream,
    timeout_ms: u64,
) -> Result<(u8, Vec<u8>), WireError> {
    let dur = Duration::from_millis(timeout_ms.max(100));
    let mut header = [0u8; 5];
    timeout(dur, stream.read_exact(&mut header))
        .await
        .map_err(|_| WireError::Eof)??;
    let tag = header[0];
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&header[1..5]);
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_RPC_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        timeout(dur, stream.read_exact(&mut payload))
            .await
            .map_err(|_| WireError::Eof)??;
    }
    Ok((tag, payload))
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::CommittedLeaderId;
    use tempfile::tempdir;

    /// A round-trip helper: send a Vote RPC over TCP, get a Vote response back.
    /// Used to verify the wire protocol independent of a full Raft cluster.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tcp_network_round_trips_vote_rpc() {
        // Two echo servers on localhost ports.
        let dir1 = tempdir().expect("tempdir1");
        let dir2 = tempdir().expect("tempdir2");

        // Build two persistent single-node RaftManagers (so we have real Raft
        // instances to dispatch to). They will NOT form a cluster — each is
        // its own single-node cluster — but the TCP transport test only
        // needs *a* Raft handle on the receiving end.
        let mgr1 = crate::storage::raft::RaftManager::new_single_node_persistent(
            1,
            dir1.path(),
        )
        .await
        .expect("mgr1");
        let mgr2 = crate::storage::raft::RaftManager::new_single_node_persistent(
            2,
            dir2.path(),
        )
        .await
        .expect("mgr2");

        // Start a TcpRaftServer for node 2 on an ephemeral port.
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = TcpRaftServer::start(bind, mgr2.raft.clone()).await.expect("server");

        // Register node 2's address with the factory.
        let factory = TcpRaftNetworkFactory::new();
        factory.register(2, server.local_addr).await;

        // Build a client for node 2 and send a Vote RPC.
        let mut client_factory = factory.clone();
        let mut client = client_factory.new_client(2, &BasicNode::new(String::from("node-2"))).await;
        let req = VoteRequest {
            vote: openraft::Vote::new(99, 2),
            last_log_id: Some(openraft::LogId::new(
                CommittedLeaderId::<u64>::new(99, 2),
                5,
            )),
        };
        let option = RPCOption::new(Duration::from_secs(2));
        let resp = client.vote(req, option).await.expect("vote RPC");
        // Node 2 will reject (it has its own state), but the wire round-trip
        // succeeds — that's what we're verifying.
        let _ = resp;

        // Clean up.
        let _ = mgr1.shutdown().await;
        let _ = mgr2.shutdown().await;
        drop(server);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// Verify that sending an RPC to an unregistered node returns Unreachable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_network_unregistered_target_is_unreachable() {
        let factory = TcpRaftNetworkFactory::new();
        let mut client_factory = factory.clone();
        let mut client = client_factory.new_client(99, &BasicNode::new(String::from("node-99"))).await;
        let req = VoteRequest {
            vote: openraft::Vote::new(1, 99),
            last_log_id: None,
        };
        let option = RPCOption::new(Duration::from_millis(500));
        let res = client.vote(req, option).await;
        assert!(res.is_err(), "expected error for unregistered target");
        match res.unwrap_err() {
            RPCError::Unreachable(_) => { /* expected */ }
            other => panic!("expected Unreachable, got {:?}", other),
        }
    }
}
