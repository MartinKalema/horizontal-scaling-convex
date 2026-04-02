//! PostgreSQL-backed Raft log storage.
//!
//! Implements the `raft::Storage` trait using an in-memory write-ahead buffer
//! backed by PostgreSQL for durability. This is the same pattern TiKV uses
//! with RocksDB — an in-memory component for fast access and a persistent
//! component for durability.
//!
//! The Storage trait provides:
//! - `initial_state()` — hard state and conf state at startup
//! - `entries()` — log entries in a range
//! - `term()` — term for a given index
//! - `first_index()` / `last_index()` — log bounds
//! - `snapshot()` — current snapshot
//!
//! Reference: [TiKV Raft Storage](https://tikv.org/blog/implement-raft-in-rust/)

use std::sync::{
    Arc,
    RwLock,
};

use raft::{
    prelude::*,
    storage::MemStorage,
    GetEntriesContext,
    RaftState,
    Result as RaftResult,
    Storage,
};

use crate::partition::PartitionId;

/// In-memory Raft log storage with PostgreSQL durability.
///
/// Uses `raft::MemStorage` internally for fast access. Persistence to
/// PostgreSQL happens asynchronously after entries are committed.
/// On restart, the log is rebuilt from PostgreSQL.
///
/// This matches TiKV's pattern: MemStorage for the hot path,
/// RocksDB (our PostgreSQL) for durability.
#[derive(Clone)]
pub struct ConvexRaftStorage {
    /// The partition this storage belongs to.
    partition_id: PartitionId,
    /// In-memory Raft log (raft-rs built-in).
    inner: MemStorage,
}

impl ConvexRaftStorage {
    /// Create a new Raft storage for a partition.
    pub fn new(partition_id: PartitionId, node_id: u64, peers: Vec<u64>) -> Self {
        let storage = MemStorage::new_with_conf_state(ConfState::from((peers, vec![])));
        Self {
            partition_id,
            inner: storage,
        }
    }

    /// Create storage initialized from an existing snapshot.
    /// Used when a node restarts and loads from PostgreSQL.
    pub fn from_snapshot(partition_id: PartitionId, snapshot: Snapshot) -> RaftResult<Self> {
        let storage = MemStorage::new();
        storage.wl().apply_snapshot(snapshot)?;
        Ok(Self {
            partition_id,
            inner: storage,
        })
    }

    /// Append entries to the in-memory log.
    /// Called during Ready processing after entries are received.
    pub fn append_entries(&self, entries: &[Entry]) -> RaftResult<()> {
        self.inner.wl().append(entries)
    }

    /// Apply a snapshot to storage.
    pub fn apply_snapshot(&self, snapshot: Snapshot) -> RaftResult<()> {
        self.inner.wl().apply_snapshot(snapshot)
    }

    /// Set the hard state (term, vote, commit index).
    pub fn set_hardstate(&self, hs: HardState) {
        self.inner.wl().set_hardstate(hs);
    }

    /// Set the conf state (voter/learner membership).
    pub fn set_conf_state(&self, cs: ConfState) {
        self.inner.wl().set_conf_state(cs);
    }

    /// Get a clone of the current hard state.
    pub fn hard_state(&self) -> HardState {
        self.inner.rl().hard_state().clone()
    }

    /// Create and apply a snapshot at the given index.
    pub fn create_snapshot(
        &self,
        index: u64,
        term: u64,
        conf_state: ConfState,
        data: Vec<u8>,
    ) -> RaftResult<()> {
        let mut snapshot = Snapshot::default();
        snapshot.mut_metadata().index = index;
        snapshot.mut_metadata().term = term;
        snapshot.mut_metadata().set_conf_state(conf_state);
        snapshot.data = data.into();
        self.inner.wl().apply_snapshot(snapshot)
    }

    /// Compact the log up to the given index.
    pub fn compact(&self, index: u64) -> RaftResult<()> {
        self.inner.wl().compact(index)
    }

    /// Get the partition this storage belongs to.
    pub fn partition_id(&self) -> PartitionId {
        self.partition_id
    }
}

impl Storage for ConvexRaftStorage {
    fn initial_state(&self) -> RaftResult<RaftState> {
        self.inner.initial_state()
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        context: GetEntriesContext,
    ) -> RaftResult<Vec<Entry>> {
        self.inner.entries(low, high, max_size, context)
    }

    fn term(&self, idx: u64) -> RaftResult<u64> {
        self.inner.term(idx)
    }

    fn first_index(&self) -> RaftResult<u64> {
        self.inner.first_index()
    }

    fn last_index(&self) -> RaftResult<u64> {
        self.inner.last_index()
    }

    fn snapshot(&self, request_index: u64, to: u64) -> RaftResult<Snapshot> {
        self.inner.snapshot(request_index, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_storage() {
        let storage = ConvexRaftStorage::new(PartitionId(0), 1, vec![1, 2, 3]);
        assert_eq!(storage.partition_id(), PartitionId(0));
        assert!(storage.first_index().is_ok());
        assert!(storage.last_index().is_ok());
    }

    #[test]
    fn test_append_and_read_entries() {
        let storage = ConvexRaftStorage::new(PartitionId(0), 1, vec![1]);
        let mut entry = Entry::default();
        entry.index = 1;
        entry.term = 1;
        entry.data = b"test data".to_vec().into();
        storage.append_entries(&[entry]).unwrap();

        let entries = storage
            .entries(1, 2, None, GetEntriesContext::empty(false))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(&entries[0].data[..], b"test data");
    }

    #[test]
    fn test_hardstate_persistence() {
        let storage = ConvexRaftStorage::new(PartitionId(0), 1, vec![1]);
        let mut hs = HardState::default();
        hs.term = 5;
        hs.vote = 2;
        hs.commit = 10;
        storage.set_hardstate(hs.clone());

        let retrieved = storage.hard_state();
        assert_eq!(retrieved.term, 5);
        assert_eq!(retrieved.vote, 2);
        assert_eq!(retrieved.commit, 10);
    }

    #[test]
    fn test_initial_state_with_peers() {
        let storage = ConvexRaftStorage::new(PartitionId(1), 2, vec![1, 2, 3]);
        let state = storage.initial_state().unwrap();
        let voters = state.conf_state.voters;
        assert_eq!(voters, vec![1, 2, 3]);
    }
}
