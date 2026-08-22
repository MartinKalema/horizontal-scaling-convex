//! Persistent Raft log storage backed by TiKV's raft-engine.
//!
//! raft-engine is a log-structured append-only storage engine purpose-built
//! for Multi-Raft logs. TiKV switched from RocksDB to raft-engine as the
//! default in v6.1 because:
//!   - Write amplification ~1x (vs RocksDB's up to 30x)
//!   - No LSM compaction stalls
//!   - Designed for FIFO Raft log patterns (append, read, truncate)
//!
//! We store per-partition:
//!   - Log entries via `add_entries` (raft-engine tracks first/last index)
//!   - HardState via `put_message` (term, vote, commit)
//!   - ConfState via `put_message` (voter/learner membership)
//!   - Applied index via a small raw KV value (state-machine apply progress)
//!
//! Reference: [TiKV raft-engine](https://github.com/tikv/raft-engine)
//! Reference: [PingCAP Blog](https://www.pingcap.com/blog/raft-engine-a-log-structured-embedded-storage-engine-for-multi-raft-logs-in-tikv/)

use std::sync::Arc;

use anyhow::Context;
use parking_lot::RwLock;
use raft::{
    prelude::*,
    GetEntriesContext,
    RaftState,
    Result as RaftResult,
    Storage,
    StorageError,
};
use raft_engine::{
    Config as EngineConfig,
    Engine,
    LogBatch,
    MessageExt,
};

use crate::partition::PartitionId;

/// Key for storing HardState in raft-engine's KV space.
const HARD_STATE_KEY: &[u8] = b"hard_state";
/// Key for storing ConfState in raft-engine's KV space.
const CONF_STATE_KEY: &[u8] = b"conf_state";
/// Key for storing the highest Raft log index durably applied to Convex state.
const APPLIED_INDEX_KEY: &[u8] = b"applied_index";
/// Key for storing the latest installed/generated snapshot in raft-engine's KV
/// space.
const SNAPSHOT_KEY: &[u8] = b"snapshot";
/// Incoming snapshot staged before mutating Convex persistence. This is the
/// durable cross-store install intent used to resume after a process crash.
const PENDING_SNAPSHOT_KEY: &[u8] = b"pending_snapshot";
const PENDING_SNAPSHOT_HARD_STATE_KEY: &[u8] = b"pending_snapshot_hard_state";

pub trait RaftSnapshotProvider: Send + Sync {
    fn snapshot_bytes(&self, raft_index: u64, raft_term: u64) -> anyhow::Result<Vec<u8>>;
}

impl<F> RaftSnapshotProvider for F
where
    F: Fn(u64, u64) -> anyhow::Result<Vec<u8>> + Send + Sync,
{
    fn snapshot_bytes(&self, raft_index: u64, raft_term: u64) -> anyhow::Result<Vec<u8>> {
        self(raft_index, raft_term)
    }
}

#[derive(Clone, Debug)]
pub struct PendingSnapshotInstall {
    pub snapshot: Snapshot,
    pub hard_state: HardState,
}

/// Bridge between raft-rs Entry type and raft-engine's MessageExt trait.
/// Tells raft-engine how to extract the log index from an Entry.
/// This is the same pattern TiKV uses in its PeerStorage.
struct RaftMessageExt;

impl MessageExt for RaftMessageExt {
    type Entry = Entry;

    fn index(entry: &Entry) -> u64 {
        entry.get_index()
    }
}

/// Persistent Raft log storage backed by raft-engine (TiKV pattern).
///
/// Each node runs one raft-engine instance shared across all partitions.
/// Partitions are isolated by `region_id` (= partition_id as u64).
#[derive(Clone)]
pub struct ConvexRaftStorage {
    /// The partition this storage belongs to (used as raft-engine region_id).
    partition_id: PartitionId,
    /// Shared raft-engine instance.
    engine: Arc<Engine>,
    /// Cached ConfState for the Storage trait (raft-engine doesn't track this
    /// separately from entries, so we cache it in memory and persist to KV).
    conf_state: Arc<RwLock<ConfState>>,
    /// Hook for generating state-machine snapshots on demand.
    snapshot_provider: Option<Arc<dyn RaftSnapshotProvider>>,
    #[cfg(test)]
    fail_next_write: Arc<std::sync::atomic::AtomicBool>,
}

impl ConvexRaftStorage {
    /// Open or create the raft-engine at the given path.
    /// Called once per node at startup. Returns the engine to be shared
    /// across all partition storages on this node.
    pub fn open_engine(path: &str) -> anyhow::Result<Arc<Engine>> {
        let cfg = EngineConfig {
            dir: path.to_owned(),
            // Minimal config following TiKV defaults for raft-engine.
            // purge_threshold triggers GC when total log size exceeds this.
            ..Default::default()
        };
        let engine =
            Engine::open(cfg).context("Failed to open raft-engine for persistent Raft storage")?;
        tracing::info!("Opened raft-engine at {path}");
        Ok(Arc::new(engine))
    }

    /// Create storage for a partition using a shared engine.
    /// On first boot: initializes with the given peers.
    /// On restart: loads existing state from the engine.
    pub fn new(
        partition_id: PartitionId,
        engine: Arc<Engine>,
        _node_id: u64,
        peers: Vec<u64>,
        snapshot_provider: Option<Arc<dyn RaftSnapshotProvider>>,
    ) -> anyhow::Result<Self> {
        let region_id = partition_id.0 as u64;

        // Check if this partition already has persisted state (restart case).
        let existing_cs: Option<ConfState> = engine
            .get_message(region_id, CONF_STATE_KEY)
            .context("Failed to read ConfState from raft-engine")?;

        let conf_state = if let Some(cs) = existing_cs {
            tracing::info!(
                "Raft storage: loaded existing state for partition {partition_id}, voters={:?}",
                cs.get_voters(),
            );
            cs
        } else {
            // First boot: initialize with peers.
            let cs = ConfState::from((peers.clone(), vec![]));
            let mut batch = LogBatch::default();
            batch
                .put_message(region_id, CONF_STATE_KEY.to_vec(), &cs)
                .context("Failed to persist initial ConfState")?;
            engine
                .write(&mut batch, true)
                .context("Failed to write initial ConfState to raft-engine")?;
            tracing::info!(
                "Raft storage: initialized partition {partition_id} with peers {peers:?}",
            );
            cs
        };

        Ok(Self {
            partition_id,
            engine,
            conf_state: Arc::new(RwLock::new(conf_state)),
            snapshot_provider,
            #[cfg(test)]
            fail_next_write: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_write_for_test(&self) {
        self.fail_next_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn check_fail_next_write_for_test(&self) -> anyhow::Result<()> {
        if self
            .fail_next_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected raft-engine write failure");
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn check_fail_next_write_for_test(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Region ID for raft-engine (partition_id as u64).
    fn region_id(&self) -> u64 {
        self.partition_id.0 as u64
    }

    /// Append entries to the persistent log.
    /// Called during Ready processing after entries are received.
    pub fn append_entries(&self, entries: &[Entry]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.check_fail_next_write_for_test()?;
        let mut batch = LogBatch::default();
        batch
            .add_entries::<RaftMessageExt>(self.region_id(), entries)
            .context("Failed to add entries to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write entries to raft-engine")?;
        Ok(())
    }

    /// Persist hard state (term, vote, commit index).
    /// Called during Ready processing alongside entry append.
    pub fn set_hardstate(&self, hs: &HardState) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let mut batch = LogBatch::default();
        batch
            .put_message(self.region_id(), HARD_STATE_KEY.to_vec(), hs)
            .context("Failed to add HardState to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write HardState to raft-engine")?;
        Ok(())
    }

    /// Persist hard state and entries atomically in one batch.
    /// This is the TiKV WriteBatch pattern — entries + hard state go
    /// in one atomic write to ensure crash consistency.
    pub fn append_entries_and_hardstate(
        &self,
        entries: &[Entry],
        hs: &HardState,
    ) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let mut batch = LogBatch::default();
        if !entries.is_empty() {
            batch
                .add_entries::<RaftMessageExt>(self.region_id(), entries)
                .context("Failed to add entries to LogBatch")?;
        }
        batch
            .put_message(self.region_id(), HARD_STATE_KEY.to_vec(), hs)
            .context("Failed to add HardState to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write entries+HardState to raft-engine")?;
        Ok(())
    }

    /// Highest Raft log index durably applied to the Convex state machine.
    ///
    /// This intentionally differs from `HardState.commit`: commit means a
    /// quorum accepted the log entry; applied means this local replica
    /// installed it.
    pub fn applied_index(&self) -> anyhow::Result<u64> {
        if let Some(raw) = self.engine.get(self.region_id(), APPLIED_INDEX_KEY) {
            let bytes: [u8; 8] = raw
                .as_slice()
                .try_into()
                .with_context(|| format!("Invalid Raft applied index value: {raw:?}"))?;
            return Ok(u64::from_be_bytes(bytes));
        }
        // Backward compatibility for snapshots created before the applied-index
        // key existed: an installed snapshot implies the state machine is applied
        // through the snapshot metadata index.
        Ok(self
            .snapshot_metadata()?
            .map(|metadata| metadata.index)
            .unwrap_or(0))
    }

    pub fn set_applied_index(&self, index: u64) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let mut batch = LogBatch::default();
        batch
            .put(
                self.region_id(),
                APPLIED_INDEX_KEY.to_vec(),
                index.to_be_bytes().to_vec(),
            )
            .context("Failed to add applied index to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write applied index to raft-engine")?;
        Ok(())
    }

    /// Set the conf state (voter/learner membership).
    pub fn set_conf_state(&mut self, cs: ConfState) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let mut batch = LogBatch::default();
        batch
            .put_message(self.region_id(), CONF_STATE_KEY.to_vec(), &cs)
            .context("Failed to add ConfState to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write ConfState to raft-engine")?;
        *self.conf_state.write() = cs;
        Ok(())
    }

    /// Atomically persist a committed membership configuration and the Raft
    /// index at which it became applied.
    ///
    /// A configuration change is itself part of the replicated state machine.
    /// Persisting these values separately could leave a restarted node with the
    /// new membership but an applied index that causes the same change to
    /// replay.
    pub fn set_conf_state_and_applied_index(
        &mut self,
        cs: ConfState,
        index: u64,
    ) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        anyhow::ensure!(
            index >= self.applied_index()?,
            "Cannot move Raft applied index backwards while persisting ConfState"
        );

        let mut batch = LogBatch::default();
        batch
            .put_message(self.region_id(), CONF_STATE_KEY.to_vec(), &cs)
            .context("Failed to add ConfState to LogBatch")?;
        batch
            .put(
                self.region_id(),
                APPLIED_INDEX_KEY.to_vec(),
                index.to_be_bytes().to_vec(),
            )
            .context("Failed to add configuration applied index to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write ConfState and applied index to raft-engine")?;
        *self.conf_state.write() = cs;
        Ok(())
    }

    pub fn conf_state(&self) -> ConfState {
        self.conf_state.read().clone()
    }

    fn persisted_snapshot(&self) -> anyhow::Result<Option<Snapshot>> {
        self.engine
            .get_message(self.region_id(), SNAPSHOT_KEY)
            .context("Failed to read Snapshot from raft-engine")
    }

    fn pending_snapshot_install_from_engine(
        engine: &Engine,
        region_id: u64,
    ) -> anyhow::Result<Option<PendingSnapshotInstall>> {
        let snapshot: Option<Snapshot> = engine
            .get_message(region_id, PENDING_SNAPSHOT_KEY)
            .context("Failed to read pending Raft snapshot install")?;
        let hard_state: Option<HardState> = engine
            .get_message(region_id, PENDING_SNAPSHOT_HARD_STATE_KEY)
            .context("Failed to read pending Raft snapshot hard state")?;
        match (snapshot, hard_state) {
            (None, None) => Ok(None),
            (Some(snapshot), Some(hard_state)) => Ok(Some(PendingSnapshotInstall {
                snapshot,
                hard_state,
            })),
            _ => anyhow::bail!("Incomplete durable Raft snapshot install intent"),
        }
    }

    pub fn pending_snapshot_install_for_engine(
        engine: &Engine,
        partition_id: PartitionId,
    ) -> anyhow::Result<Option<PendingSnapshotInstall>> {
        Self::pending_snapshot_install_from_engine(engine, partition_id.0 as u64)
    }

    pub fn pending_snapshot_install(&self) -> anyhow::Result<Option<PendingSnapshotInstall>> {
        Self::pending_snapshot_install_from_engine(&self.engine, self.region_id())
    }

    /// Durably stage the complete incoming snapshot before Convex persistence
    /// is changed. The staged record survives every later install crash window.
    pub fn stage_snapshot(
        &self,
        snapshot: &Snapshot,
        ready_hard_state: Option<&HardState>,
    ) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let metadata = snapshot.get_metadata();
        anyhow::ensure!(
            metadata.index > self.applied_index()?,
            "Rejecting stale Raft snapshot at index {} because durable applied index is {}",
            metadata.index,
            self.applied_index()?,
        );
        crate::raft_snapshot::decode_snapshot(snapshot.get_data(), metadata.index, metadata.term)?;
        if let Some(pending) = self.pending_snapshot_install()? {
            anyhow::ensure!(
                pending.snapshot == *snapshot,
                "A different Raft snapshot install is already pending at index {}",
                pending.snapshot.get_metadata().index,
            );
            return Ok(());
        }

        let mut hard_state = ready_hard_state.cloned().unwrap_or(self.hard_state()?);
        hard_state.commit = metadata.index;
        let mut batch = LogBatch::default();
        batch
            .put_message(self.region_id(), PENDING_SNAPSHOT_KEY.to_vec(), snapshot)
            .context("Failed to stage pending Snapshot")?;
        batch
            .put_message(
                self.region_id(),
                PENDING_SNAPSHOT_HARD_STATE_KEY.to_vec(),
                &hard_state,
            )
            .context("Failed to stage pending snapshot HardState")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to stage snapshot install in raft-engine")?;
        Ok(())
    }

    fn commit_pending_snapshot_install_to_engine(
        engine: &Engine,
        region_id: u64,
    ) -> anyhow::Result<PendingSnapshotInstall> {
        let pending = Self::pending_snapshot_install_from_engine(engine, region_id)?
            .context("No staged Raft snapshot install to commit")?;
        let metadata = pending.snapshot.get_metadata();
        crate::raft_snapshot::decode_snapshot(
            pending.snapshot.get_data(),
            metadata.index,
            metadata.term,
        )?;
        anyhow::ensure!(
            pending.hard_state.commit == metadata.index,
            "Pending snapshot HardState commit {} does not match snapshot index {}",
            pending.hard_state.commit,
            metadata.index,
        );

        // `Clean` plus every replacement metadata key is one synced
        // raft-engine batch. A restart observes either the old generation and
        // pending intent, or the complete new generation, never a mixture.
        let mut batch = LogBatch::default();
        batch.add_command(region_id, raft_engine::Command::Clean);
        batch
            .put_message(region_id, SNAPSHOT_KEY.to_vec(), &pending.snapshot)
            .context("Failed to add Snapshot to install batch")?;
        batch
            .put_message(
                region_id,
                CONF_STATE_KEY.to_vec(),
                metadata.get_conf_state(),
            )
            .context("Failed to add snapshot ConfState to install batch")?;
        batch
            .put_message(region_id, HARD_STATE_KEY.to_vec(), &pending.hard_state)
            .context("Failed to add snapshot HardState to install batch")?;
        batch
            .put(
                region_id,
                APPLIED_INDEX_KEY.to_vec(),
                metadata.index.to_be_bytes().to_vec(),
            )
            .context("Failed to add snapshot applied index to install batch")?;
        engine
            .write(&mut batch, true)
            .context("Failed to atomically commit snapshot metadata to raft-engine")?;
        Ok(pending)
    }

    pub fn commit_pending_snapshot_install_for_engine(
        engine: &Engine,
        partition_id: PartitionId,
    ) -> anyhow::Result<PendingSnapshotInstall> {
        Self::commit_pending_snapshot_install_to_engine(engine, partition_id.0 as u64)
    }

    pub fn commit_pending_snapshot_install(&mut self) -> anyhow::Result<u64> {
        self.check_fail_next_write_for_test()?;
        let pending =
            Self::commit_pending_snapshot_install_to_engine(&self.engine, self.region_id())?;
        let metadata = pending.snapshot.get_metadata();
        *self.conf_state.write() = metadata.get_conf_state().clone();
        Ok(metadata.index)
    }

    pub fn apply_snapshot(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        self.stage_snapshot(snapshot, None)?;
        self.commit_pending_snapshot_install()?;
        Ok(())
    }

    /// Get the current hard state.
    pub fn hard_state(&self) -> anyhow::Result<HardState> {
        let hs: Option<HardState> = self
            .engine
            .get_message(self.region_id(), HARD_STATE_KEY)
            .context("Failed to read HardState from raft-engine")?;
        Ok(hs.unwrap_or_default())
    }

    /// Compact the log up to the given index.
    /// Removes entries before `index` to reclaim disk space.
    pub fn compact(&self, index: u64) -> anyhow::Result<()> {
        self.engine.compact_to(self.region_id(), index);
        Ok(())
    }

    /// Purge expired files from the engine.
    /// Should be called periodically (TiKV does every 10 seconds).
    pub fn purge(&self) -> anyhow::Result<()> {
        let _ = self
            .engine
            .purge_expired_files()
            .context("Failed to purge raft-engine files")?;
        Ok(())
    }

    /// Get the partition this storage belongs to.
    pub fn partition_id(&self) -> PartitionId {
        self.partition_id
    }

    fn snapshot_metadata(&self) -> anyhow::Result<Option<SnapshotMetadata>> {
        Ok(self
            .persisted_snapshot()?
            .map(|snapshot| snapshot.metadata.unwrap_or_default()))
    }
}

/// Implement raft-rs Storage trait backed by raft-engine.
/// This is the equivalent of TiKV's PeerStorage.
impl Storage for ConvexRaftStorage {
    fn initial_state(&self) -> RaftResult<RaftState> {
        let hs: HardState = self
            .engine
            .get_message(self.region_id(), HARD_STATE_KEY)
            .map_err(|e| {
                raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))))
            })?
            .unwrap_or_default();

        Ok(RaftState {
            hard_state: hs,
            conf_state: self.conf_state(),
        })
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> RaftResult<Vec<Entry>> {
        let first = self.first_index()?;
        if low < first {
            return Err(raft::Error::Store(StorageError::Compacted));
        }
        let max = max_size.into().map(|s| s as usize);
        let mut entries = Vec::new();
        self.engine
            .fetch_entries_to::<RaftMessageExt>(self.region_id(), low, high, max, &mut entries)
            .map_err(|e| {
                raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))))
            })?;
        Ok(entries)
    }

    fn term(&self, idx: u64) -> RaftResult<u64> {
        if let Some(snapshot_metadata) = self.snapshot_metadata().map_err(|e| {
            raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                e.to_string(),
            ))))
        })? {
            if idx == snapshot_metadata.index {
                return Ok(snapshot_metadata.term);
            }
            if idx < snapshot_metadata.index {
                return Err(raft::Error::Store(StorageError::Compacted));
            }
        }
        let entry: Option<Entry> = self
            .engine
            .get_entry::<RaftMessageExt>(self.region_id(), idx)
            .map_err(|e| {
                raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))))
            })?;
        match entry {
            Some(e) => Ok(e.get_term()),
            None => {
                // Check if this is the dummy entry at index 0.
                if idx == 0 {
                    return Ok(0);
                }
                Err(raft::Error::Store(StorageError::Compacted))
            },
        }
    }

    fn first_index(&self) -> RaftResult<u64> {
        let engine_first = self.engine.first_index(self.region_id());
        let snapshot_first = self
            .snapshot_metadata()
            .map_err(|e| {
                raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))))
            })?
            .map(|metadata| metadata.index.saturating_add(1));
        Ok(match (engine_first, snapshot_first) {
            (Some(engine_first), Some(snapshot_first)) => engine_first.max(snapshot_first),
            (Some(engine_first), None) => engine_first,
            (None, Some(snapshot_first)) => snapshot_first,
            (None, None) => 1,
        })
    }

    fn last_index(&self) -> RaftResult<u64> {
        let engine_last = self.engine.last_index(self.region_id()).unwrap_or(0);
        let snapshot_last = self
            .snapshot_metadata()
            .map_err(|e| {
                raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))))
            })?
            .map(|metadata| metadata.index)
            .unwrap_or(0);
        Ok(engine_last.max(snapshot_last))
    }

    fn snapshot(&self, _request_index: u64, _to: u64) -> RaftResult<Snapshot> {
        let Some(snapshot_provider) = &self.snapshot_provider else {
            return Err(raft::Error::Store(
                StorageError::SnapshotTemporarilyUnavailable,
            ));
        };

        let snapshot_index = self.applied_index().map_err(|e| {
            raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;
        if snapshot_index == 0 {
            return Err(raft::Error::Store(
                StorageError::SnapshotTemporarilyUnavailable,
            ));
        }
        let snapshot_term = self.term(snapshot_index)?;
        let snapshot_data = snapshot_provider
            .snapshot_bytes(snapshot_index, snapshot_term)
            .map_err(|e| {
                if e.is::<crate::raft_snapshot::RaftSnapshotTemporarilyUnavailable>() {
                    raft::Error::Store(StorageError::SnapshotTemporarilyUnavailable)
                } else {
                    raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                        e.to_string(),
                    ))))
                }
            })?;

        let mut metadata = SnapshotMetadata::default();
        metadata.index = snapshot_index;
        metadata.term = snapshot_term;
        metadata.set_conf_state(self.conf_state());

        let mut snapshot = Snapshot::default();
        snapshot.set_metadata(metadata);
        snapshot.set_data(snapshot_data.into());
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Arc<Engine> {
        let dir = tempfile::tempdir().unwrap();
        ConvexRaftStorage::open_engine(dir.path().to_str().unwrap()).unwrap()
    }

    fn test_snapshot(index: u64, term: u64, voters: Vec<u64>) -> Snapshot {
        let mut metadata = SnapshotMetadata::default();
        metadata.index = index;
        metadata.term = term;
        metadata.set_conf_state(ConfState::from((voters, vec![])));
        let mut snapshot = Snapshot::default();
        snapshot.set_metadata(metadata);
        snapshot.set_data(
            crate::raft_snapshot::test_snapshot_bytes(index, term)
                .unwrap()
                .into(),
        );
        snapshot
    }

    #[test]
    fn test_create_storage() {
        let engine = test_engine();
        let storage =
            ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();
        assert_eq!(storage.partition_id(), PartitionId(0));
        assert!(storage.first_index().is_ok());
        assert!(storage.last_index().is_ok());
    }

    #[test]
    fn test_append_and_read_entries() {
        let engine = test_engine();
        let storage = ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1], None).unwrap();

        let mut entry = Entry::default();
        entry.set_index(1);
        entry.set_term(1);
        entry.set_data(b"test data".to_vec().into());
        storage.append_entries(&[entry]).unwrap();

        let entries = storage
            .entries(1, 2, None, GetEntriesContext::empty(false))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(&entries[0].get_data()[..], b"test data");
    }

    #[test]
    fn test_hardstate_persistence() {
        let engine = test_engine();
        let storage = ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1], None).unwrap();

        let mut hs = HardState::default();
        hs.set_term(5);
        hs.set_vote(2);
        hs.set_commit(10);
        storage.set_hardstate(&hs).unwrap();

        let retrieved = storage.hard_state().unwrap();
        assert_eq!(retrieved.get_term(), 5);
        assert_eq!(retrieved.get_vote(), 2);
        assert_eq!(retrieved.get_commit(), 10);
    }

    #[test]
    fn test_applied_index_persistence() {
        let engine = test_engine();
        let storage = ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1], None).unwrap();

        assert_eq!(storage.applied_index().unwrap(), 0);
        storage.set_applied_index(7).unwrap();
        assert_eq!(storage.applied_index().unwrap(), 7);
    }

    #[test]
    fn test_initial_state_with_peers() {
        let engine = test_engine();
        let storage =
            ConvexRaftStorage::new(PartitionId(1), engine, 2, vec![1, 2, 3], None).unwrap();
        let state = storage.initial_state().unwrap();
        let voters = state.conf_state.get_voters().to_vec();
        assert_eq!(voters, vec![1, 2, 3]);
    }

    #[test]
    fn test_clones_share_configuration_updates_and_snapshot_membership() {
        let engine = test_engine();
        let mut node_storage =
            ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1], None).unwrap();
        let mut raw_node_storage = node_storage.clone();

        node_storage
            .set_conf_state_and_applied_index(ConfState::from((vec![1, 2], vec![])), 1)
            .unwrap();
        assert_eq!(
            raw_node_storage
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[1, 2]
        );

        raw_node_storage
            .apply_snapshot(&test_snapshot(2, 2, vec![2, 3]))
            .unwrap();
        assert_eq!(
            node_storage
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[2, 3]
        );
        assert_eq!(node_storage.applied_index().unwrap(), 2);
    }

    #[test]
    fn test_state_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        // First boot: write entries and hard state.
        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();

            let mut entry = Entry::default();
            entry.set_index(1);
            entry.set_term(1);
            entry.set_data(b"persisted".to_vec().into());

            let mut hs = HardState::default();
            hs.set_term(1);
            hs.set_vote(1);
            hs.set_commit(1);

            storage.append_entries_and_hardstate(&[entry], &hs).unwrap();
        }
        // Engine dropped, simulating restart.

        // Reopen: verify state survived.
        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();

            // Hard state persisted.
            let state = storage.initial_state().unwrap();
            assert_eq!(state.hard_state.get_term(), 1);
            assert_eq!(state.hard_state.get_vote(), 1);
            assert_eq!(state.conf_state.get_voters(), &[1, 2, 3]);

            // Entries persisted.
            assert_eq!(storage.last_index().unwrap(), 1);
            let entries = storage
                .entries(1, 2, None, GetEntriesContext::empty(false))
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(&entries[0].get_data()[..], b"persisted");
        }
    }

    #[test]
    fn test_snapshot_persists_and_reloads() {
        let source_dir = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().to_str().unwrap();

        let snapshot = {
            let engine = ConvexRaftStorage::open_engine(source_path).unwrap();
            let provider: Arc<dyn RaftSnapshotProvider> =
                Arc::new(crate::raft_snapshot::test_snapshot_bytes);
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], Some(provider))
                    .unwrap();

            let mut entries = Vec::new();
            for index in 1..=5 {
                let mut entry = Entry::default();
                entry.set_index(index);
                entry.set_term(2);
                entries.push(entry);
            }
            storage.append_entries(&entries).unwrap();

            let mut hs = HardState::default();
            hs.set_term(2);
            hs.set_commit(5);
            storage.set_hardstate(&hs).unwrap();

            assert!(
                storage.snapshot(5, 2).is_err(),
                "snapshot must not use commit index before applied index is persisted",
            );
            storage.set_applied_index(5).unwrap();
            let snapshot = storage.snapshot(5, 2).unwrap();
            assert_eq!(snapshot.get_metadata().index, 5);
            assert_eq!(snapshot.get_metadata().term, 2);
            let decoded = crate::raft_snapshot::decode_snapshot(snapshot.get_data(), 5, 2).unwrap();
            assert_eq!(decoded.state_machine_index, 5);
            snapshot
        };

        let destination_dir = tempfile::tempdir().unwrap();
        let destination_path = destination_dir.path().to_str().unwrap();
        {
            let engine = ConvexRaftStorage::open_engine(destination_path).unwrap();
            let mut storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 2, vec![1, 2, 3], None).unwrap();
            storage.apply_snapshot(&snapshot).unwrap();
            assert_eq!(storage.first_index().unwrap(), 6);
            assert_eq!(storage.last_index().unwrap(), 5);
            assert_eq!(storage.term(5).unwrap(), 2);
        }

        {
            let engine = ConvexRaftStorage::open_engine(destination_path).unwrap();
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();
            assert_eq!(storage.first_index().unwrap(), 6);
            assert_eq!(storage.last_index().unwrap(), 5);
            assert_eq!(storage.term(5).unwrap(), 2);
            assert_eq!(storage.applied_index().unwrap(), 5);
            assert_eq!(storage.hard_state().unwrap().commit, 5);
            assert!(storage.pending_snapshot_install().unwrap().is_none());
        }
    }

    #[test]
    fn test_staged_snapshot_survives_restart_and_finalize_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let snapshot = test_snapshot(5, 2, vec![1, 2, 3]);

        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 2, vec![1, 2, 3], None).unwrap();
            storage.set_applied_index(2).unwrap();
            storage.stage_snapshot(&snapshot, None).unwrap();
            assert_eq!(storage.applied_index().unwrap(), 2);
            assert_eq!(
                storage
                    .pending_snapshot_install()
                    .unwrap()
                    .unwrap()
                    .snapshot,
                snapshot,
            );
        }

        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let mut storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 2, vec![1, 2, 3], None).unwrap();
            storage.fail_next_write_for_test();
            assert!(storage.commit_pending_snapshot_install().is_err());
            assert_eq!(storage.applied_index().unwrap(), 2);
            assert!(storage.pending_snapshot_install().unwrap().is_some());

            assert_eq!(storage.commit_pending_snapshot_install().unwrap(), 5);
            assert_eq!(storage.applied_index().unwrap(), 5);
            assert_eq!(storage.hard_state().unwrap().commit, 5);
            assert!(storage.pending_snapshot_install().unwrap().is_none());
        }
    }

    #[test]
    fn test_snapshot_stage_rejects_stale_mismatched_and_competing_generations() {
        let engine = test_engine();
        let storage =
            ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();
        storage.set_applied_index(5).unwrap();

        let stale = test_snapshot(5, 2, vec![1, 2, 3]);
        assert!(storage.stage_snapshot(&stale, None).is_err());

        let mut mismatched = test_snapshot(6, 2, vec![1, 2, 3]);
        mismatched.mut_metadata().index = 7;
        assert!(storage.stage_snapshot(&mismatched, None).is_err());

        let accepted = test_snapshot(6, 2, vec![1, 2, 3]);
        storage.stage_snapshot(&accepted, None).unwrap();
        storage.stage_snapshot(&accepted, None).unwrap();
        let competing = test_snapshot(7, 2, vec![1, 2, 3]);
        assert!(storage.stage_snapshot(&competing, None).is_err());
    }
}
