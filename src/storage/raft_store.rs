//! Disk-backed Raft storage backed by [`sled`].
//!
//! [`SledRaftStore`] implements openraft's v1 [`RaftStorage`] trait by
//! persisting every piece of consensus state to a [`sled::Db`]:
//!
//! - the Raft log (entries indexed by log index) — `raft_log` tree
//! - the last persisted vote — `raft_vote` tree (single key `v`)
//! - the last persisted committed log id — `raft_committed` tree (key `c`)
//! - the applied state machine (a `Vec<Vec<u8>>` of WAL record bytes) —
//!   `raft_sm` tree (single key `applied`)
//! - the last applied log id + membership — `raft_sm_meta` tree
//!   (keys: `last_applied`, `last_membership`)
//! - the current snapshot (encoded bytes + meta) — `raft_snapshot` tree
//!   (keys: `data`, `meta`)
//!
//! This replaces the in-memory [`crate::storage::raft::MemStore`] so the
//! Raft log survives process restarts. The store opens a sled database
//! at a caller-provided data directory; the directory and its contents
//! persist across drops and re-opens.
//!
//! ## Concurrency
//!
//! All operations go through `&self` (no `&mut self`). sled trees are
//! internally thread-safe; openraft's `RaftStorage` impls are required
//! to be `Send + Sync`. The store holds the [`sled::Db`] behind a plain
//! [`Arc`] — sled already serializes writes internally per tree.
//!
//! ## Encoding
//!
//! Log entries and snapshots are encoded with [`bincode`] (already a
//! turboGP dependency). Votes, log ids, and memberships derive `serde`
//! via openraft and are likewise bincode-encoded.
//!
//! [`RaftStorage`]: openraft::storage::RaftStorage

#![cfg(feature = "raft")]

use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openraft::entry::EntryPayload;
use openraft::storage::RaftLogReader;
use openraft::{
    AnyError, BasicNode, CommittedLeaderId, Entry, LogId, LogState, OptionalSend,
    RaftSnapshotBuilder, RaftStorage, Snapshot, SnapshotMeta, SnapshotId, StorageError,
    StorageIOError, StoredMembership, Vote,
};
use sled::{Db, IVec, Tree};
use tokio::sync::Mutex;

use crate::storage::raft::{decode_snapshot, encode_snapshot, TypeConfig};

// =========================================================================
// Sled key constants — kept short to keep tree keys compact.
// =========================================================================

/// Key under which the last persisted [`Vote`] is stored (in `raft_vote`).
const VOTE_KEY: &[u8] = b"v";
/// Key under which the last persisted committed [`LogId`] is stored (in
/// `raft_committed`).
const COMMITTED_KEY: &[u8] = b"c";
/// Key under which the applied-records `Vec<Vec<u8>>` is stored (in `raft_sm`).
const APPLIED_KEY: &[u8] = b"applied";
/// Key under which the last applied [`LogId`] is stored (in `raft_sm_meta`).
const LAST_APPLIED_KEY: &[u8] = b"last_applied";
/// Key under which the last applied [`StoredMembership`] is stored (in
/// `raft_sm_meta`).
const LAST_MEMBERSHIP_KEY: &[u8] = b"last_membership";
/// Key under which the current snapshot's payload bytes are stored.
const SNAPSHOT_DATA_KEY: &[u8] = b"data";
/// Key under which the current snapshot's meta is stored.
const SNAPSHOT_META_KEY: &[u8] = b"meta";

// =========================================================================
// SledRaftStore
// =========================================================================

/// Disk-backed Raft storage.
///
/// Persists the Raft log, vote, committed index, state machine, and
/// snapshots to a [`sled::Db`] rooted at `data_dir`. All sled trees are
/// created lazily on first access; the database file is flushed to disk
/// on every `append_to_log`, `save_vote`, `save_committed`,
/// `apply_to_state_machine`, `install_snapshot`, and `purge_logs_upto`
/// call via [`Tree::flush`] (sled's group-commit semantics ensure
/// durability without an explicit `fsync` per entry).
#[derive(Clone)]
pub struct SledRaftStore {
    /// The underlying sled database.
    db: Arc<Db>,
    /// A mutex guarding the snapshot-rebuild path (which is logically
    /// `&mut self` but openraft calls it via `&mut self`, so we serialize
    /// it here in case multiple tasks race).
    snapshot_lock: Arc<Mutex<()>>,
    /// The on-disk data directory (kept for inspection / re-open).
    data_dir: PathBuf,
}

impl SledRaftStore {
    /// Open (or create) a `SledRaftStore` rooted at `data_dir`.
    ///
    /// The directory is created if it does not exist. Multiple
    /// `SledRaftStore` instances pointing at the same directory should
    /// not be opened concurrently — sled is single-process.
    pub fn open<P: AsRef<Path>>(data_dir: P) -> Result<Self, StorageError<u64>> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let db = sled::open(&data_dir).map_err(sled_io_err)?;
        Ok(Self {
            db: Arc::new(db),
            snapshot_lock: Arc::new(Mutex::new(())),
            data_dir,
        })
    }

    /// Returns the on-disk data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Borrow the underlying sled database. Used by callers that need
    /// to inspect or manipulate sled trees directly (e.g. checking
    /// whether the `raft_log` tree is empty before initializing a
    /// cluster).
    pub fn db_ref(&self) -> &Db {
        &self.db
    }

    /// Convenience accessor for the `raft_log` tree.
    fn log_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_log").map_err(sled_io_err)
    }

    /// Convenience accessor for the `raft_vote` tree.
    fn vote_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_vote").map_err(sled_io_err)
    }

    /// Convenience accessor for the `raft_committed` tree.
    fn committed_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_committed").map_err(sled_io_err)
    }

    /// Convenience accessor for the `raft_sm` (state machine) tree.
    fn sm_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_sm").map_err(sled_io_err)
    }

    /// Convenience accessor for the `raft_sm_meta` tree.
    fn sm_meta_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_sm_meta").map_err(sled_io_err)
    }

    /// Convenience accessor for the `raft_snapshot` tree.
    fn snapshot_tree(&self) -> Result<Tree, StorageError<u64>> {
        self.db.open_tree("raft_snapshot").map_err(sled_io_err)
    }

    /// Returns the applied records (the state machine payload) —
    /// primarily for test inspection.
    pub fn applied_records(&self) -> Result<Vec<Vec<u8>>, StorageError<u64>> {
        let tree = self.sm_tree()?;
        let bytes = tree
            .get(APPLIED_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        bincode::deserialize::<Vec<Vec<u8>>>(&bytes)
            .map_err(|e| StorageError::from(StorageIOError::<u64>::read_state_machine(AnyError::new(&e))))
    }

    /// Returns the last applied log id (for test inspection).
    pub fn last_applied(&self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let tree = self.sm_meta_tree()?;
        let bytes = tree
            .get(LAST_APPLIED_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(None);
        }
        bincode::deserialize::<Option<LogId<u64>>>(&bytes)
            .map_err(|e| StorageError::from(StorageIOError::<u64>::read_state_machine(AnyError::new(&e))))
    }

    /// Encode a u64 log index as an 8-byte big-endian key (so lexicographic
    /// sled iteration = ascending index order).
    fn idx_key(idx: u64) -> [u8; 8] {
        idx.to_be_bytes()
    }

    /// Returns the on-disk data directory (kept for re-open and inspection).
    fn _unused_path_buf_marker(&self) -> PathBuf {
        self.data_dir.clone()
    }

    /// Decode a sled key back into a u64 log index.
    fn key_to_idx(key: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        if key.len() < 8 {
            return 0;
        }
        buf.copy_from_slice(&key[..8]);
        u64::from_be_bytes(buf)
    }
}

/// Cheap cloneable log reader backed by the same `Arc<Db>`.
#[derive(Clone)]
pub struct SledRaftLogReader {
    db: Arc<Db>,
}

impl SledRaftLogReader {
    /// Build a reader sharing the given database.
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

impl RaftLogReader<TypeConfig> for SledRaftLogReader {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>>
    where
        RB: RangeBounds<u64> + Clone + Debug + OptionalSend,
    {
        let tree = self.db.open_tree("raft_log").map_err(sled_io_err)?;
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
        let start_key = SledRaftStore::idx_key(start);
        for item in tree.range(start_key..) {
            let (k, v) = item.map_err(sled_io_err)?;
            let idx = SledRaftStore::key_to_idx(&k);
            if idx >= end {
                break;
            }
            let entry: Entry<TypeConfig> = bincode::deserialize(&v).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_logs(
                    AnyError::new(&e),
                ))
            })?;
            out.push(entry);
        }
        Ok(out)
    }
}

// SledRaftStore also implements RaftLogReader directly (used by Adaptor).
impl RaftLogReader<TypeConfig> for SledRaftStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>>
    where
        RB: RangeBounds<u64> + Clone + Debug + OptionalSend,
    {
        let tree = self.log_tree()?;
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
        let start_key = Self::idx_key(start);
        for item in tree.range(start_key..) {
            let (k, v) = item.map_err(sled_io_err)?;
            let idx = Self::key_to_idx(&k);
            if idx >= end {
                break;
            }
            let entry: Entry<TypeConfig> = bincode::deserialize(&v).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_logs(
                    AnyError::new(&e),
                ))
            })?;
            out.push(entry);
        }
        Ok(out)
    }
}

/// Snapshot builder for `SledRaftStore`. Builds a snapshot from the
/// current applied-records state.
pub struct SledRaftSnapshotBuilder {
    db: Arc<Db>,
}

impl RaftSnapshotBuilder<TypeConfig> for SledRaftSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        let sm_tree = self.db.open_tree("raft_sm").map_err(sled_io_err)?;
        let meta_tree = self.db.open_tree("raft_sm_meta").map_err(sled_io_err)?;
        let snap_tree = self.db.open_tree("raft_snapshot").map_err(sled_io_err)?;

        let applied_bytes = sm_tree
            .get(APPLIED_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        let applied: Vec<Vec<u8>> = if applied_bytes.is_empty() {
            Vec::new()
        } else {
            bincode::deserialize(&applied_bytes).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_state_machine(
                    AnyError::new(&e),
                ))
            })?
        };
        let last_applied: Option<LogId<u64>> = meta_tree
            .get(LAST_APPLIED_KEY)
            .map_err(sled_io_err)?
            .filter(|v| !v.is_empty())
            .map(|v| bincode::deserialize::<Option<LogId<u64>>>(&v).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_state_machine(
                    AnyError::new(&e),
                ))
            }))
            .transpose()?
            .flatten();
        let last_membership: StoredMembership<u64, BasicNode> = meta_tree
            .get(LAST_MEMBERSHIP_KEY)
            .map_err(sled_io_err)?
            .filter(|v| !v.is_empty())
            .map(|v| {
                bincode::deserialize::<StoredMembership<u64, BasicNode>>(&v).map_err(|e| {
                    StorageError::from(StorageIOError::<u64>::read_state_machine(
                        AnyError::new(&e),
                    ))
                })
            })
            .transpose()?
            .unwrap_or_default();

        let payload = encode_snapshot(&applied);
        let meta = SnapshotMeta {
            last_log_id: last_applied.clone(),
            last_membership,
            snapshot_id: SnapshotId::from("sled-snapshot"),
        };
        // Persist the snapshot so it survives restart.
        snap_tree
            .insert(SNAPSHOT_DATA_KEY, payload.as_slice())
            .map_err(sled_io_err)?;
        let _ = snap_tree.flush();
        let meta_bytes = bincode::serialize(&meta).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        snap_tree
            .insert(SNAPSHOT_META_KEY, meta_bytes.as_slice())
            .map_err(sled_io_err)?;
        snap_tree.flush().map_err(sled_io_err)?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(payload)),
        })
    }
}

impl RaftStorage<TypeConfig> for SledRaftStore {
    type LogReader = SledRaftLogReader;
    type SnapshotBuilder = SledRaftSnapshotBuilder;

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let tree = self.vote_tree()?;
        let bytes = bincode::serialize(vote).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        tree.insert(VOTE_KEY, bytes.as_slice()).map_err(sled_io_err)?;
        tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let tree = self.vote_tree()?;
        let bytes = tree.get(VOTE_KEY).map_err(sled_io_err)?.unwrap_or_default();
        if bytes.is_empty() {
            return Ok(None);
        }
        let vote = bincode::deserialize::<Vote<u64>>(&bytes).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::read_state_machine(
                AnyError::new(&e),
            ))
        })?;
        Ok(Some(vote))
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let tree = self.committed_tree()?;
        let bytes = bincode::serialize(&committed).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        tree.insert(COMMITTED_KEY, bytes.as_slice())
            .map_err(sled_io_err)?;
        tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let tree = self.committed_tree()?;
        let bytes = tree
            .get(COMMITTED_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(None);
        }
        bincode::deserialize::<Option<LogId<u64>>>(&bytes).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::read_state_machine(
                AnyError::new(&e),
            ))
        })
    }

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let tree = self.log_tree()?;
        let last_log_id = if let Some(item) = tree.last().map_err(sled_io_err)? {
            let (_k, v) = item;
            let entry: Entry<TypeConfig> = bincode::deserialize(&v).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_logs(
                    AnyError::new(&e),
                ))
            })?;
            Some(entry.log_id)
        } else {
            None
        };
        Ok(LogState {
            last_purged_log_id: None,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        SledRaftLogReader::new(self.db.clone())
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        let tree = self.log_tree()?;
        for entry in entries {
            let idx = entry.log_id.index;
            let bytes = bincode::serialize(&entry).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::write_logs(
                    AnyError::new(&e),
                ))
            })?;
            tree.insert(Self::idx_key(idx), bytes.as_slice())
                .map_err(sled_io_err)?;
        }
        tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<u64>,
    ) -> Result<(), StorageError<u64>> {
        let tree = self.log_tree()?;
        let start_key = Self::idx_key(log_id.index);
        let keys_to_delete: Vec<[u8; 8]> = tree
            .range(start_key..)
            .filter_map(|r| r.ok())
            .map(|(k, _)| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&k[..8]);
                buf
            })
            .collect();
        for k in keys_to_delete {
            tree.remove(k).map_err(sled_io_err)?;
        }
        tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let tree = self.log_tree()?;
        let end_key = Self::idx_key(log_id.index.saturating_add(1));
        let keys_to_delete: Vec<[u8; 8]> = tree
            .range(..end_key)
            .filter_map(|r| r.ok())
            .map(|(k, _)| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&k[..8]);
                buf
            })
            .collect();
        for k in keys_to_delete {
            tree.remove(k).map_err(sled_io_err)?;
        }
        tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        let tree = self.sm_meta_tree()?;
        let last_applied: Option<LogId<u64>> = tree
            .get(LAST_APPLIED_KEY)
            .map_err(sled_io_err)?
            .filter(|v| !v.is_empty())
            .map(|v| {
                bincode::deserialize::<Option<LogId<u64>>>(&v).map_err(|e| {
                    StorageError::from(StorageIOError::<u64>::read_state_machine(
                        AnyError::new(&e),
                    ))
                })
            })
            .transpose()?
            .flatten();
        let last_membership: StoredMembership<u64, BasicNode> = tree
            .get(LAST_MEMBERSHIP_KEY)
            .map_err(sled_io_err)?
            .filter(|v| !v.is_empty())
            .map(|v| {
                bincode::deserialize::<StoredMembership<u64, BasicNode>>(&v).map_err(|e| {
                    StorageError::from(StorageIOError::<u64>::read_state_machine(
                        AnyError::new(&e),
                    ))
                })
            })
            .transpose()?
            .unwrap_or_default();
        Ok((last_applied, last_membership))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<TypeConfig>],
    ) -> Result<Vec<()>, StorageError<u64>> {
        let sm_tree = self.sm_tree()?;
        let meta_tree = self.sm_meta_tree()?;

        // Load the current applied records.
        let mut applied: Vec<Vec<u8>> = {
            let bytes = sm_tree
                .get(APPLIED_KEY)
                .map_err(sled_io_err)?
                .unwrap_or_default();
            if bytes.is_empty() {
                Vec::new()
            } else {
                bincode::deserialize(&bytes).map_err(|e| {
                    StorageError::from(StorageIOError::<u64>::read_state_machine(
                        AnyError::new(&e),
                    ))
                })?
            }
        };

        let mut last_applied: Option<LogId<u64>> = None;
        let mut last_membership: StoredMembership<u64, BasicNode> =
            // Read current membership so we don't clobber it on non-membership entries.
            {
                let bytes = meta_tree
                    .get(LAST_MEMBERSHIP_KEY)
                    .map_err(sled_io_err)?
                    .unwrap_or_default();
                if bytes.is_empty() {
                    StoredMembership::default()
                } else {
                    bincode::deserialize(&bytes).map_err(|e| {
                        StorageError::from(StorageIOError::<u64>::read_state_machine(
                            AnyError::new(&e),
                        ))
                    })?
                }
            };

        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            last_applied = Some(entry.log_id.clone());
            match &entry.payload {
                EntryPayload::Blank => {}
                EntryPayload::Normal(d) => {
                    applied.push(d.clone());
                }
                EntryPayload::Membership(m) => {
                    last_membership =
                        StoredMembership::new(Some(entry.log_id.clone()), m.clone());
                }
            }
            out.push(());
        }

        // Persist.
        let applied_bytes = bincode::serialize(&applied).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        sm_tree
            .insert(APPLIED_KEY, applied_bytes.as_slice())
            .map_err(sled_io_err)?;
        if let Some(la) = &last_applied {
            let bytes = bincode::serialize(&Some(la.clone())).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::write_state_machine(
                    AnyError::new(&e),
                ))
            })?;
            meta_tree
                .insert(LAST_APPLIED_KEY, bytes.as_slice())
                .map_err(sled_io_err)?;
        }
        let lm_bytes = bincode::serialize(&last_membership).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        meta_tree
            .insert(LAST_MEMBERSHIP_KEY, lm_bytes.as_slice())
            .map_err(sled_io_err)?;
        sm_tree.flush().map_err(sled_io_err)?;
        meta_tree.flush().map_err(sled_io_err)?;
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SledRaftSnapshotBuilder {
            db: self.db.clone(),
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
        let _guard = self.snapshot_lock.lock().await;
        let mut data = Vec::new();
        snapshot
            .read_to_end(&mut data)
            .await
            .map_err(|e| StorageError::<u64>::from(StorageIOError::<u64>::read_state_machine(AnyError::new(&e))))?;
        let applied = decode_snapshot(&data);

        // Persist applied records + meta.
        let sm_tree = self.sm_tree()?;
        let meta_tree = self.sm_meta_tree()?;
        let snap_tree = self.snapshot_tree()?;

        let applied_bytes = bincode::serialize(&applied).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        sm_tree
            .insert(APPLIED_KEY, applied_bytes.as_slice())
            .map_err(sled_io_err)?;

        let la_bytes = bincode::serialize(&Some(meta.last_log_id.clone())).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        meta_tree
            .insert(LAST_APPLIED_KEY, la_bytes.as_slice())
            .map_err(sled_io_err)?;
        let lm_bytes = bincode::serialize(&meta.last_membership).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        meta_tree
            .insert(LAST_MEMBERSHIP_KEY, lm_bytes.as_slice())
            .map_err(sled_io_err)?;

        // Persist the snapshot itself.
        snap_tree
            .insert(SNAPSHOT_DATA_KEY, data.as_slice())
            .map_err(sled_io_err)?;
        let meta_bytes = bincode::serialize(meta).map_err(|e| {
            StorageError::from(StorageIOError::<u64>::write_state_machine(
                AnyError::new(&e),
            ))
        })?;
        snap_tree
            .insert(SNAPSHOT_META_KEY, meta_bytes.as_slice())
            .map_err(sled_io_err)?;

        sm_tree.flush().map_err(sled_io_err)?;
        meta_tree.flush().map_err(sled_io_err)?;
        snap_tree.flush().map_err(sled_io_err)?;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        let tree = self.snapshot_tree()?;
        let data = tree
            .get(SNAPSHOT_DATA_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        if data.is_empty() {
            return Ok(None);
        }
        let meta_bytes = tree
            .get(SNAPSHOT_META_KEY)
            .map_err(sled_io_err)?
            .unwrap_or_default();
        let meta: SnapshotMeta<u64, BasicNode> = if meta_bytes.is_empty() {
            SnapshotMeta::default()
        } else {
            bincode::deserialize(&meta_bytes).map_err(|e| {
                StorageError::from(StorageIOError::<u64>::read_state_machine(
                    AnyError::new(&e),
                ))
            })?
        };
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data.to_vec())),
        }))
    }
}

// =========================================================================
// Helpers
// =========================================================================

/// Map a sled error to an openraft `StorageError` (state-machine write context).
fn sled_write_sm_err(e: sled::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::write_state_machine(AnyError::new(&e)))
}

/// Map a sled error to an openraft `StorageError` (state-machine read context).
fn sled_read_sm_err(e: sled::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::read_state_machine(AnyError::new(&e)))
}

/// Map a sled error to an openraft `StorageError` (log write context).
fn sled_write_log_err(e: sled::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::write_logs(AnyError::new(&e)))
}

/// Map a sled error to an openraft `StorageError` (log read context).
fn sled_read_log_err(e: sled::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::read_logs(AnyError::new(&e)))
}

/// Collect a sled `Iter` (yields `Result<(IVec, IVec), sled::Error>`) into a
/// `Vec<(IVec, IVec)>`, short-circuiting on the first error.
fn collect_iter(iter: sled::Iter) -> Result<Vec<(IVec, IVec)>, StorageError<u64>> {
    let mut out = Vec::new();
    for item in iter {
        let (k, v) = item.map_err(sled_read_log_err)?;
        out.push((k, v));
    }
    Ok(out)
}

/// Collect just the keys from a sled `Iter`.
fn collect_keys(iter: sled::Iter) -> Result<Vec<IVec>, StorageError<u64>> {
    let mut out = Vec::new();
    for item in iter {
        let (k, _v) = item.map_err(sled_read_log_err)?;
        out.push(k);
    }
    Ok(out)
}

/// Map a bincode error to an openraft `StorageError` (state-machine write).
fn bincode_write_sm_err(e: bincode::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::write_state_machine(AnyError::new(&e)))
}

/// Map a bincode error to an openraft `StorageError` (state-machine read).
fn bincode_read_sm_err(e: bincode::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::read_state_machine(AnyError::new(&e)))
}

/// Map a bincode error to an openraft `StorageError` (log write).
fn bincode_write_log_err(e: bincode::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::write_logs(AnyError::new(&e)))
}

/// Map a bincode error to an openraft `StorageError` (log read).
fn bincode_read_log_err(e: bincode::Error) -> StorageError<u64> {
    StorageError::from(StorageIOError::<u64>::read_logs(AnyError::new(&e)))
}

/// Backwards-compat alias for `sled_write_log_err` (used everywhere a sled
/// tree operation can fail at write time).
fn sled_io_err(e: sled::Error) -> StorageError<u64> {
    sled_write_log_err(e)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Task 2.2 DoD: write 10 entries, close, reopen, verify all 10 present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sled_store_persists_log_entries_across_reopen() {
        let dir = tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf();

        // Build 10 fake log entries with indices 0..10.
        let mut entries: Vec<Entry<TypeConfig>> = Vec::with_capacity(10);
        for i in 0u64..10 {
            entries.push(Entry::<TypeConfig> {
                log_id: LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), i),
                payload: openraft::EntryPayload::Normal(vec![i as u8; 8]),
            });
        }

        // First open: append the entries.
        {
            let mut store = SledRaftStore::open(&dir_path).expect("open");
            <SledRaftStore as RaftStorage<TypeConfig>>::append_to_log(&mut store, entries.clone())
                .await
                .expect("append");
            // Verify last index = 9.
            let state = store.get_log_state().await.expect("log state");
            assert_eq!(
                state.last_log_id,
                Some(LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), 9))
            );
            // store drops here; sled closes when the last Arc<Db> is dropped.
        }

        // Re-open: the entries must all be present.
        {
            let mut store = SledRaftStore::open(&dir_path).expect("reopen");
            let got = store
                .try_get_log_entries(0u64..)
                .await
                .expect("read entries");
            assert_eq!(got.len(), 10, "expected 10 entries after reopen");
            for (i, entry) in got.iter().enumerate() {
                assert_eq!(entry.log_id.index, i as u64);
            }
        }
    }

    /// Task 2.2 DoD: vote, committed, and applied state survive reopen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sled_store_persists_vote_and_state_machine_across_reopen() {
        let dir = tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf();

        // First open: write vote + state machine.
        {
            let mut store = SledRaftStore::open(&dir_path).expect("open");

            let vote = Vote::<u64>::new(2, 1);
            store.save_vote(&vote).await.expect("save_vote");

            let committed = Some(LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), 7));
            store.save_committed(committed).await.expect("save_committed");

            let entries = vec![Entry::<TypeConfig> {
                log_id: LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), 0),
                payload: openraft::EntryPayload::Normal(vec![0xAA; 4]),
            }];
            store
                .apply_to_state_machine(&entries)
                .await
                .expect("apply");
        }

        // Reopen and verify.
        {
            let mut store = SledRaftStore::open(&dir_path).expect("reopen");
            let vote = store.read_vote().await.expect("read_vote");
            assert_eq!(vote, Some(Vote::<u64>::new(2, 1)));

            let committed = store.read_committed().await.expect("read_committed");
            assert_eq!(
                committed,
                Some(LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), 7))
            );

            let applied = store.applied_records().expect("applied");
            assert_eq!(applied.len(), 1);
            assert_eq!(applied[0], vec![0xAA; 4]);

            let last_applied = store.last_applied().expect("last_applied");
            assert_eq!(
                last_applied,
                Some(LogId::<u64>::new(CommittedLeaderId::<u64>::new(1, 1), 0))
            );
        }
    }
}
