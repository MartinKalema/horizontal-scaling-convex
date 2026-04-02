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
};

use raft::prelude::Message;
use tokio::sync::mpsc;

use crate::{
    partition::PartitionId,
    raft_node::{
        LeadershipCallbacks,
        RaftMessage,
        RaftNode,
        RaftNodeConfig,
    },
};

/// Shared state for a Raft-enabled partition, accessible from the Committer
/// and the HTTP layer.
#[derive(Clone)]
pub struct RaftPartitionState {
    /// Whether this node is the current Raft leader for this partition.
    is_leader: Arc<AtomicBool>,
    /// The current leader's node ID (0 if unknown).
    leader_id: Arc<AtomicU64>,
    /// Channel to send proposals to the Raft node.
    proposal_tx: mpsc::UnboundedSender<RaftMessage>,
    /// Partition ID.
    partition_id: PartitionId,
}

impl RaftPartitionState {
    /// Check if this node is the leader for this partition.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }

    /// Get the current leader's node ID.
    pub fn leader_id(&self) -> u64 {
        self.leader_id.load(Ordering::SeqCst)
    }

    /// Get the partition ID.
    pub fn partition_id(&self) -> PartitionId {
        self.partition_id
    }

    /// Send a Raft message to the node (proposals or forwarded Raft messages).
    pub fn send(&self, msg: RaftMessage) -> anyhow::Result<()> {
        self.proposal_tx.send(msg).map_err(|_| {
            anyhow::anyhow!("Raft node for partition {} not running", self.partition_id)
        })
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
    /// Create a new Raft partition manager.
    pub fn new(
        config: RaftNodeConfig,
        peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    ) -> anyhow::Result<Self> {
        let (mailbox_tx, mailbox_rx) = mpsc::unbounded_channel();

        let mut node = RaftNode::new(config.clone(), mailbox_rx, peer_senders)?;

        let is_leader = Arc::new(AtomicBool::new(false));
        let leader_id = Arc::new(AtomicU64::new(0));

        // Set up leadership callbacks that update shared atomic state.
        let is_leader_cb = is_leader.clone();
        let leader_id_cb = leader_id.clone();
        let partition_id = config.partition_id;

        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new(move || {
                is_leader_cb.store(true, Ordering::SeqCst);
                tracing::info!(
                    "Raft partition {}: Committer ACTIVATED (this node is leader)",
                    partition_id,
                );
            }),
            on_lost_leadership: Box::new({
                let is_leader_lost = is_leader.clone();
                let partition_id_lost = partition_id;
                move || {
                    is_leader_lost.store(false, Ordering::SeqCst);
                    tracing::info!(
                        "Raft partition {}: Committer DEACTIVATED (lost leadership)",
                        partition_id_lost,
                    );
                }
            }),
        });

        let state = RaftPartitionState {
            is_leader: is_leader.clone(),
            leader_id,
            proposal_tx: mailbox_tx.clone(),
            partition_id: config.partition_id,
        };

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
    use super::*;

    #[test]
    fn test_partition_state_defaults() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            ..Default::default()
        };

        let manager = RaftPartitionManager::new(config, HashMap::new()).unwrap();
        let state = manager.state();

        assert!(!state.is_leader());
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

        let mut manager = RaftPartitionManager::new(config, HashMap::new()).unwrap();
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
    }

    #[test]
    fn test_send_proposal() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            ..Default::default()
        };

        let manager = RaftPartitionManager::new(config, HashMap::new()).unwrap();
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
