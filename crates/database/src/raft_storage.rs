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
//!
//! Reference: [TiKV raft-engine](https://github.com/tikv/raft-engine)
//! Reference: [PingCAP Blog](https://www.pingcap.com/blog/raft-engine-a-log-structured-embedded-storage-engine-for-multi-raft-logs-in-tikv/)

use std::sync::Arc;

use anyhow::Context;
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
/// Key for storing the latest installed/generated snapshot in raft-engine's KV
/// space.
const SNAPSHOT_KEY: &[u8] = b"snapshot";

pub trait RaftSnapshotProvider: Send + Sync {
    fn snapshot_bytes(&self) -> anyhow::Result<Vec<u8>>;
}

impl<F> RaftSnapshotProvider for F
where
    F: Fn() -> anyhow::Result<Vec<u8>> + Send + Sync,
{
    fn snapshot_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self()
    }
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
    conf_state: ConfState,
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
            conf_state,
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
        self.conf_state = cs;
        Ok(())
    }

    fn persisted_snapshot(&self) -> anyhow::Result<Option<Snapshot>> {
        self.engine
            .get_message(self.region_id(), SNAPSHOT_KEY)
            .context("Failed to read Snapshot from raft-engine")
    }

    pub fn apply_snapshot(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        self.check_fail_next_write_for_test()?;
        let metadata = snapshot.get_metadata();
        let mut batch = LogBatch::default();
        batch.add_command(self.region_id(), raft_engine::Command::Clean);
        batch
            .put_message(self.region_id(), SNAPSHOT_KEY.to_vec(), snapshot)
            .context("Failed to add Snapshot to LogBatch")?;
        batch
            .put_message(
                self.region_id(),
                CONF_STATE_KEY.to_vec(),
                metadata.get_conf_state(),
            )
            .context("Failed to add snapshot ConfState to LogBatch")?;
        self.engine
            .write(&mut batch, true)
            .context("Failed to write snapshot to raft-engine")?;
        self.conf_state = metadata.get_conf_state().clone();
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
            conf_state: self.conf_state.clone(),
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

        let hard_state = self.hard_state().map_err(|e| {
            raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;
        let snapshot_index = hard_state.commit;
        if snapshot_index == 0 {
            return Err(raft::Error::Store(
                StorageError::SnapshotTemporarilyUnavailable,
            ));
        }
        let snapshot_term = self.term(snapshot_index)?;
        let snapshot_data = snapshot_provider.snapshot_bytes().map_err(|e| {
            raft::Error::Store(StorageError::Other(Box::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        let mut metadata = SnapshotMetadata::default();
        metadata.index = snapshot_index;
        metadata.term = snapshot_term;
        metadata.set_conf_state(self.conf_state.clone());

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
    fn test_initial_state_with_peers() {
        let engine = test_engine();
        let storage =
            ConvexRaftStorage::new(PartitionId(1), engine, 2, vec![1, 2, 3], None).unwrap();
        let state = storage.initial_state().unwrap();
        let voters = state.conf_state.get_voters().to_vec();
        assert_eq!(voters, vec![1, 2, 3]);
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let provider: Arc<dyn RaftSnapshotProvider> = Arc::new(|| Ok(b"checkpoint".to_vec()));
            let mut storage =
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

            let snapshot = storage.snapshot(5, 2).unwrap();
            assert_eq!(snapshot.get_metadata().index, 5);
            assert_eq!(snapshot.get_metadata().term, 2);
            assert_eq!(snapshot.get_data(), b"checkpoint");

            storage.apply_snapshot(&snapshot).unwrap();
            assert_eq!(storage.first_index().unwrap(), 6);
            assert_eq!(storage.last_index().unwrap(), 5);
            assert_eq!(storage.term(5).unwrap(), 2);
        }

        {
            let engine = ConvexRaftStorage::open_engine(path).unwrap();
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine, 1, vec![1, 2, 3], None).unwrap();
            assert_eq!(storage.first_index().unwrap(), 6);
            assert_eq!(storage.last_index().unwrap(), 5);
            assert_eq!(storage.term(5).unwrap(), 2);
        }
    }
}
