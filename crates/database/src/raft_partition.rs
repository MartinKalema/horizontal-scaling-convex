//! Raft partition manager: lifecycle management for a Raft-enabled partition.
//!
//! Wraps the RaftNode and integrates it with the Committer. Manages the
//! leader lifecycle: start accepting writes when elected leader, stop when
//! leadership is lost.
//!
//! This follows TiKV's pattern where each Region has a Raft group, and the
//! Raft leader runs the Apply Worker (our Committer). When leadership changes,
//! the Apply Worker starts or stops.
//!
//! ## Architecture
//!
//! ```text
//! Client mutation
//!     │
//!     ▼
//! RaftPartitionManager
//!     │
//!     ├── is_leader? ──No──▶ reject with "not leader, forward to {leader_id}"
//!     │
//!     ▼ Yes
//! Committer (local commit)
//!     │
//!     ├── publish delta to NATS (cross-partition reads)
//!     │
//!     └── propose to Raft (intra-partition replication)
//!             │
//!             ▼
//!         Raft replicates to followers
//!             │
//!             ▼
//!         Followers apply via apply_replica_delta
//! ```
//!
//! Reference: [TiKV Raft in TiKV](https://www.pingcap.com/blog/raft-in-tikv/)

use std::{
    collections::HashMap,
    sync::{
        atomic::{
            AtomicBool,
            AtomicU64,
            Ordering,
        },
        Arc,
    },
    time::Instant,
};

use parking_lot::Mutex;
use raft::prelude::Message;
use tokio::sync::mpsc;

use crate::{
    metrics,
    partition::PartitionId,
    raft_node::{
        LeadershipCallbacks,
        RaftMessage,
        RaftNode,
        RaftNodeConfig,
    },
    timestamp_oracle::TimestampOracle,
};

/// Shared state for a Raft-enabled partition, accessible from the Committer
/// and the HTTP layer.
#[derive(Clone)]
pub struct RaftPartitionState {
    /// Whether this node is the current Raft leader for this partition.
    is_leader: Arc<AtomicBool>,
    /// Whether the elected leader has durably applied its current-term barrier.
    leader_ready: Arc<AtomicBool>,
    /// The current leader's node ID (0 if unknown).
    leader_id: Arc<AtomicU64>,
    /// Channel to send proposals to the Raft node.
    proposal_tx: mpsc::UnboundedSender<RaftMessage>,
    /// Partition ID.
    partition_id: PartitionId,
    /// This node's ID within the Raft group.
    node_id: u64,
    /// Local serving lease for coordinator-owned reads/subscriptions.
    leader_serving_lease_valid_until: Arc<Mutex<Option<Instant>>>,
    /// Cluster-wide genesis has been installed by every configured partition.
    cluster_genesis_ready: Arc<AtomicBool>,
}

impl RaftPartitionState {
    /// Check if this node is the leader for this partition.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }

    /// Check whether this node is both elected leader and caught up through
    /// the current-term Raft barrier.
    pub fn is_leader_ready(&self) -> bool {
        self.is_leader() && self.leader_ready.load(Ordering::SeqCst)
    }

    pub fn has_leader_serving_lease(&self) -> bool {
        if !self.is_leader_ready() {
            return false;
        }
        self.leader_serving_lease_valid_until
            .lock()
            .is_some_and(|valid_until| valid_until > Instant::now())
    }

    pub fn is_cluster_genesis_ready(&self) -> bool {
        self.cluster_genesis_ready.load(Ordering::SeqCst)
    }

    pub fn mark_cluster_genesis_ready(&self) {
        self.cluster_genesis_ready.store(true, Ordering::SeqCst);
    }

    pub fn can_serve_as_leader(&self) -> bool {
        self.is_cluster_genesis_ready() && self.is_leader() && self.has_leader_serving_lease()
    }

    /// Get the current leader's node ID.
    pub fn leader_id(&self) -> u64 {
        self.leader_id.load(Ordering::SeqCst)
    }

    /// Get the partition ID.
    pub fn partition_id(&self) -> PartitionId {
        self.partition_id
    }

    /// Get this node's ID within the Raft group.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Send a Raft message to the node (proposals or forwarded Raft messages).
    pub fn send(&self, msg: RaftMessage) -> anyhow::Result<()> {
        self.proposal_tx.send(msg).map_err(|_| {
            anyhow::anyhow!("Raft node for partition {} not running", self.partition_id)
        })
    }

    /// Mark a locally proposed Raft entry as durably applied after the
    /// Committer has installed it into Convex state.
    pub async fn mark_applied(&self, index: u64) -> anyhow::Result<()> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.send(RaftMessage::MarkApplied { index, result_tx })?;
        result_rx.await.map_err(|_| {
            anyhow::anyhow!(
                "Raft node for partition {} stopped before marking index {} applied",
                self.partition_id,
                index,
            )
        })?
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test(
        is_leader: bool,
        leader_id: u64,
        partition_id: PartitionId,
        node_id: u64,
    ) -> Self {
        let (proposal_tx, _proposal_rx) = mpsc::unbounded_channel();
        Self {
            is_leader: Arc::new(AtomicBool::new(is_leader)),
            leader_ready: Arc::new(AtomicBool::new(is_leader)),
            leader_id: Arc::new(AtomicU64::new(leader_id)),
            proposal_tx,
            partition_id,
            node_id,
            leader_serving_lease_valid_until: Arc::new(Mutex::new(if is_leader {
                Some(Instant::now() + std::time::Duration::from_secs(60))
            } else {
                None
            })),
            cluster_genesis_ready: Arc::new(AtomicBool::new(true)),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn new_with_mailbox_for_test(
        is_leader: bool,
        leader_id: u64,
        partition_id: PartitionId,
        node_id: u64,
    ) -> (Self, mpsc::UnboundedReceiver<RaftMessage>) {
        let (proposal_tx, proposal_rx) = mpsc::unbounded_channel();
        (
            Self {
                is_leader: Arc::new(AtomicBool::new(is_leader)),
                leader_ready: Arc::new(AtomicBool::new(is_leader)),
                leader_id: Arc::new(AtomicU64::new(leader_id)),
                proposal_tx,
                partition_id,
                node_id,
                leader_serving_lease_valid_until: Arc::new(Mutex::new(if is_leader {
                    Some(Instant::now() + std::time::Duration::from_secs(60))
                } else {
                    None
                })),
                cluster_genesis_ready: Arc::new(AtomicBool::new(true)),
            },
            proposal_rx,
        )
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn expire_leader_serving_lease_for_test(&self) {
        *self.leader_serving_lease_valid_until.lock() = Some(Instant::now());
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn mark_cluster_genesis_unready_for_test(&self) {
        self.cluster_genesis_ready.store(false, Ordering::SeqCst);
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn set_leader_ready_for_test(&self, ready: bool) {
        self.leader_ready.store(ready, Ordering::SeqCst);
        if !ready {
            *self.leader_serving_lease_valid_until.lock() = None;
        }
    }
}

/// Manager for a Raft-enabled partition.
///
/// Creates the Raft node, sets up leadership callbacks, and provides
/// the shared state for the Committer to check leadership.
pub struct RaftPartitionManager {
    /// The Raft node (owned, moved into the background task on start).
    node: Option<RaftNode>,
    /// Shared state readable by Committer and HTTP layer.
    state: RaftPartitionState,
    /// Mailbox sender for the Raft node (kept for the transport server).
    mailbox_tx: mpsc::UnboundedSender<RaftMessage>,
}

impl RaftPartitionManager {
    /// Create a new Raft partition manager with persistent storage.
    pub fn new(
        config: RaftNodeConfig,
        engine: Arc<raft_engine::Engine>,
        peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
        snapshot_provider: Option<Arc<dyn crate::raft_storage::RaftSnapshotProvider>>,
        timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_cluster_genesis_gate(
            config,
            engine,
            peer_senders,
            snapshot_provider,
            timestamp_oracle,
            Arc::new(AtomicBool::new(true)),
        )
    }

    pub fn new_with_cluster_genesis_gate(
        config: RaftNodeConfig,
        engine: Arc<raft_engine::Engine>,
        peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
        snapshot_provider: Option<Arc<dyn crate::raft_storage::RaftSnapshotProvider>>,
        timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
        cluster_genesis_ready: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let (mailbox_tx, mailbox_rx) = mpsc::unbounded_channel();

        let mut node = RaftNode::new(
            config.clone(),
            engine,
            mailbox_rx,
            peer_senders,
            snapshot_provider,
        )?;

        let is_leader = Arc::new(AtomicBool::new(false));
        let leader_ready = Arc::new(AtomicBool::new(false));
        let leader_id = Arc::new(AtomicU64::new(0));
        let leader_serving_lease_valid_until = Arc::new(Mutex::new(None));
        let leader_serving_lease_duration = config.leader_serving_lease_duration();

        // Set up leadership callbacks that update shared atomic state.
        let is_leader_cb = is_leader.clone();
        let leader_ready_became_leader = leader_ready.clone();
        let serving_lease_became_leader = leader_serving_lease_valid_until.clone();
        let serving_lease_refresh = leader_serving_lease_valid_until.clone();
        let serving_lease_duration_refresh = leader_serving_lease_duration;
        let partition_id = config.partition_id;
        let tso_became_leader = timestamp_oracle.clone();

        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new(move || {
                if let Some(tso) = tso_became_leader.as_ref() {
                    tso.discard_reserved_batch();
                }
                leader_ready_became_leader.store(false, Ordering::SeqCst);
                *serving_lease_became_leader.lock() = None;
                is_leader_cb.store(true, Ordering::SeqCst);
                metrics::log_raft_is_leader(partition_id, true);
                metrics::log_raft_leader_ready(partition_id, false);
                tracing::info!(
                    "Raft partition {}: Committer ACTIVATED (this node is leader)",
                    partition_id,
                );
            }),
            on_lost_leadership: Box::new({
                let is_leader_lost = is_leader.clone();
                let leader_ready_lost = leader_ready.clone();
                let serving_lease_lost = leader_serving_lease_valid_until.clone();
                let partition_id_lost = partition_id;
                let tso_lost_leadership = timestamp_oracle.clone();
                move || {
                    if let Some(tso) = tso_lost_leadership.as_ref() {
                        tso.discard_reserved_batch();
                    }
                    is_leader_lost.store(false, Ordering::SeqCst);
                    leader_ready_lost.store(false, Ordering::SeqCst);
                    *serving_lease_lost.lock() = None;
                    metrics::log_raft_is_leader(partition_id_lost, false);
                    metrics::log_raft_leader_ready(partition_id_lost, false);
                    metrics::reset_raft_replication_health(partition_id_lost);
                    tracing::info!(
                        "Raft partition {}: Committer DEACTIVATED (lost leadership)",
                        partition_id_lost,
                    );
                }
            }),
            on_leader_ready: Box::new({
                let leader_ready = leader_ready.clone();
                move || {
                    leader_ready.store(true, Ordering::SeqCst);
                    metrics::log_raft_leader_ready(partition_id, true);
                    tracing::info!(
                        "Raft partition {}: leader is caught up and READY to serve",
                        partition_id,
                    );
                }
            }),
            on_leader_serving_lease_refreshed: Box::new(move || {
                *serving_lease_refresh.lock() =
                    Some(Instant::now() + serving_lease_duration_refresh);
            }),
            on_leader_changed: Box::new({
                let leader_id_changed = leader_id.clone();
                let partition_id_changed = partition_id;
                move |new_leader_id| {
                    let old = leader_id_changed.swap(new_leader_id, Ordering::SeqCst);
                    metrics::log_raft_leader_id(partition_id_changed, new_leader_id);
                    if old != new_leader_id {
                        metrics::log_raft_leader_change(partition_id_changed);
                        tracing::info!(
                            "Raft partition {}: leader changed from {} to {}",
                            partition_id_changed,
                            old,
                            new_leader_id,
                        );
                    }
                }
            }),
        });

        let state = RaftPartitionState {
            is_leader: is_leader.clone(),
            leader_ready,
            leader_id,
            proposal_tx: mailbox_tx.clone(),
            partition_id: config.partition_id,
            node_id: config.node_id,
            leader_serving_lease_valid_until,
            cluster_genesis_ready,
        };
        metrics::log_raft_is_leader(config.partition_id, false);
        metrics::log_raft_leader_ready(config.partition_id, false);
        metrics::log_raft_leader_id(config.partition_id, 0);
        metrics::reset_raft_replication_health(config.partition_id);

        Ok(Self {
            node: Some(node),
            state,
            mailbox_tx,
        })
    }

    /// Get the shared state (clone-able, thread-safe).
    pub fn state(&self) -> RaftPartitionState {
        self.state.clone()
    }

    /// Get the mailbox sender for the transport server to forward
    /// Raft messages from peers.
    pub fn mailbox_tx(&self) -> mpsc::UnboundedSender<RaftMessage> {
        self.mailbox_tx.clone()
    }

    /// Take ownership of the Raft node for spawning into a background task.
    pub fn take_node(&mut self) -> Option<RaftNode> {
        self.node.take()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{
            AtomicUsize,
            Ordering,
        },
        Arc,
    };

    use async_trait::async_trait;
    use common::types::Timestamp;

    use super::*;
    use crate::raft_storage::ConvexRaftStorage;

    struct RecordingTimestampOracle {
        discards: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TimestampOracle for RecordingTimestampOracle {
        async fn next_ts_at_or_after(&self, _min_ts: Timestamp) -> anyhow::Result<Timestamp> {
            Ok(Timestamp::must(1))
        }

        async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
            Ok(Timestamp::MIN)
        }

        async fn advance_committed_ts(&self, _ts: Timestamp) -> anyhow::Result<()> {
            Ok(())
        }

        fn discard_reserved_batch(&self) {
            self.discards.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_engine() -> Arc<raft_engine::Engine> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.into_path();
        ConvexRaftStorage::open_engine(path.to_str().unwrap()).unwrap()
    }

    #[test]
    fn test_partition_state_defaults() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            ..Default::default()
        };

        let manager =
            RaftPartitionManager::new(config, test_engine(), HashMap::new(), None, None).unwrap();
        let state = manager.state();

        assert!(!state.is_leader());
        assert!(!state.is_leader_ready());
        assert!(!state.has_leader_serving_lease());
        assert_eq!(state.leader_id(), 0);
        assert_eq!(state.partition_id(), PartitionId(0));
    }

    #[tokio::test]
    async fn test_leadership_updates_shared_state() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let mut manager =
            RaftPartitionManager::new(config, test_engine(), HashMap::new(), None, None).unwrap();
        let state = manager.state();

        assert!(!state.is_leader());

        // Take the node and trigger election manually.
        let mut node = manager.take_node().unwrap();
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();

        // The shared state should now reflect leadership.
        assert!(state.is_leader());
        assert!(state.is_leader_ready());
        assert!(state.has_leader_serving_lease());
        assert_eq!(state.leader_id(), 1);
        assert_eq!(
            state.leader_id(),
            1,
            "Shared state should expose the elected leader ID"
        );
    }

    #[tokio::test]
    async fn test_became_leader_discards_tso_reserved_batch() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let discards = Arc::new(AtomicUsize::new(0));
        let tso = Arc::new(RecordingTimestampOracle {
            discards: discards.clone(),
        });

        let mut manager =
            RaftPartitionManager::new(config, test_engine(), HashMap::new(), None, Some(tso))
                .unwrap();

        let mut node = manager.take_node().unwrap();
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();

        assert_eq!(discards.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_send_proposal() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            ..Default::default()
        };

        let manager =
            RaftPartitionManager::new(config, test_engine(), HashMap::new(), None, None).unwrap();
        let state = manager.state();

        // Should be able to send without error (node exists).
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let result = state.send(RaftMessage::Propose(crate::raft_node::RaftProposal {
            data: b"test".to_vec(),
            result_tx: tx,
        }));
        assert!(result.is_ok());
    }
}
