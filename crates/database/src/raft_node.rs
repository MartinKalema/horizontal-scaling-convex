//! Raft consensus node for a single partition.
//!
//! Drives the raft-rs `RawNode` through the tick → propose → ready → advance
//! cycle. This is the same pattern TiKV uses: a loop that receives messages,
//! ticks the election timer, proposes client writes, and processes the Ready
//! state.
//!
//! The Raft node wraps the partition's Committer. When this node is the Raft
//! leader, it proposes client mutations through Raft. When committed entries
//! arrive, they're applied to the Committer's state machine.
//!
//! References:
//!   - [TiKV: Implement Raft in Rust](https://tikv.org/blog/implement-raft-in-rust/)
//!   - [raft-rs five_mem_node example](https://github.com/tikv/raft-rs/blob/master/examples/five_mem_node/main.rs)

use std::{
    collections::HashMap,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use raft::{
    prelude::*,
    raw_node::RawNode,
    StateRole,
};
use slog::o;
use tokio::sync::mpsc;

use crate::{
    partition::PartitionId,
    raft_storage::ConvexRaftStorage,
};

/// Configuration for a Raft node.
#[derive(Clone, Debug)]
pub struct RaftNodeConfig {
    /// This node's ID within the Raft group.
    pub node_id: u64,
    /// Partition this Raft group manages.
    pub partition_id: PartitionId,
    /// IDs of all nodes in this Raft group (including self).
    pub peers: Vec<u64>,
    /// Election timeout in ticks (each tick = 100ms).
    pub election_tick: usize,
    /// Heartbeat interval in ticks.
    pub heartbeat_tick: usize,
}

impl Default for RaftNodeConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10, // 1 second
            heartbeat_tick: 3, // 300ms
        }
    }
}

/// A proposal from a client waiting to be committed via Raft.
pub struct RaftProposal {
    /// Serialized mutation data.
    pub data: Vec<u8>,
    /// Channel to notify the client when the proposal is committed.
    pub result_tx: tokio::sync::oneshot::Sender<bool>,
}

/// Messages that can be sent to the Raft node.
pub enum RaftMessage {
    /// A Raft protocol message from another node.
    Raft(Message),
    /// A client proposal to be committed.
    Propose(RaftProposal),
    /// Shutdown the Raft node.
    Shutdown,
}

/// Callbacks for leadership changes.
/// TiKV's pattern: the Raft node notifies the application when this node
/// becomes leader or loses leadership, so the application can start/stop
/// the Committer accordingly.
pub struct LeadershipCallbacks {
    /// Called when this node becomes the Raft leader.
    /// The Committer should start accepting writes for this partition.
    pub on_became_leader: Box<dyn FnMut() + Send>,
    /// Called when this node loses leadership (stepped down or partitioned).
    /// The Committer should stop accepting writes and drain pending proposals.
    pub on_lost_leadership: Box<dyn FnMut() + Send>,
}

impl Default for LeadershipCallbacks {
    fn default() -> Self {
        Self {
            on_became_leader: Box::new(|| {}),
            on_lost_leadership: Box::new(|| {}),
        }
    }
}

/// A running Raft consensus node for one partition.
pub struct RaftNode {
    /// The raft-rs RawNode.
    pub(crate) raw_node: RawNode<ConvexRaftStorage>,
    /// Storage backend.
    pub(crate) storage: ConvexRaftStorage,
    /// Pending proposals waiting for commit (index → callback).
    pending_proposals: HashMap<u64, tokio::sync::oneshot::Sender<bool>>,
    /// Channel for receiving messages.
    mailbox: mpsc::UnboundedReceiver<RaftMessage>,
    /// Channels for sending Raft messages to other nodes.
    /// Maps node_id → sender.
    peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    /// Configuration.
    config: RaftNodeConfig,
    /// Whether this node is currently the leader.
    is_leader_flag: bool,
    /// Leadership change callbacks.
    leadership_callbacks: LeadershipCallbacks,
}

impl RaftNode {
    /// Create a new Raft node with persistent storage.
    ///
    /// The `engine` is a shared raft-engine instance (one per node, all
    /// partitions share it). On first boot, the storage is initialized
    /// with the given peers. On restart, existing state is loaded from
    /// raft-engine — this is what prevents the "to_commit X is out of
    /// range [last_index 0]" panic that MemStorage caused.
    pub fn new(
        config: RaftNodeConfig,
        engine: Arc<raft_engine::Engine>,
        mailbox: mpsc::UnboundedReceiver<RaftMessage>,
        peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    ) -> anyhow::Result<Self> {
        let storage = ConvexRaftStorage::new(
            config.partition_id,
            engine,
            config.node_id,
            config.peers.clone(),
        )?;

        // On restart, pick up where we left off (applied index from
        // the persisted hard state's commit). This prevents raft-rs
        // from re-applying already-committed entries.
        let initial_state = storage.initial_state().map_err(|e| anyhow::anyhow!("{e}"))?;
        let applied = initial_state.hard_state.get_commit();

        let raft_config = Config {
            id: config.node_id,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            applied,
            check_quorum: true,
            pre_vote: true,
            ..Default::default()
        };

        let logger = slog::Logger::root(slog::Fuse(slog_stdlog::StdLog), o!());
        let raw_node = RawNode::new(&raft_config, storage.clone(), &logger)?;

        Ok(Self {
            raw_node,
            storage,
            pending_proposals: HashMap::new(),
            mailbox,
            peer_senders,
            config,
            is_leader_flag: false,
            leadership_callbacks: LeadershipCallbacks::default(),
        })
    }

    /// Run the Raft loop. This is the main event loop following TiKV's pattern:
    /// tick → receive messages → propose → process ready → advance.
    pub async fn run(&mut self, mut on_committed: impl FnMut(&[u8]) -> anyhow::Result<()>) {
        let tick_interval = Duration::from_millis(100);
        let mut last_tick = Instant::now();

        loop {
            // Receive all pending messages (non-blocking).
            loop {
                match self.mailbox.try_recv() {
                    Ok(RaftMessage::Raft(msg)) => {
                        if let Err(e) = self.raw_node.step(msg) {
                            tracing::warn!("Raft step error: {e}");
                        }
                    },
                    Ok(RaftMessage::Propose(proposal)) => {
                        if self.raw_node.raft.state == StateRole::Leader {
                            let index = self.raw_node.raft.raft_log.last_index() + 1;
                            if let Err(e) = self.raw_node.propose(vec![], proposal.data) {
                                tracing::warn!("Raft propose error: {e}");
                                let _ = proposal.result_tx.send(false);
                            } else {
                                self.pending_proposals.insert(index, proposal.result_tx);
                            }
                        } else {
                            // Not leader — reject.
                            let _ = proposal.result_tx.send(false);
                        }
                    },
                    Ok(RaftMessage::Shutdown) => {
                        tracing::info!("Raft node {} shutting down", self.config.node_id);
                        return;
                    },
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        tracing::info!("Raft mailbox closed, shutting down");
                        return;
                    },
                }
            }

            // Tick the election/heartbeat timer.
            if last_tick.elapsed() >= tick_interval {
                self.raw_node.tick();
                last_tick = Instant::now();
            }

            // Process Ready state.
            if self.raw_node.has_ready() {
                let mut ready = self.raw_node.ready();

                // Detect leadership changes (TiKV SoftState pattern).
                // SoftState is present in Ready only when it changes.
                if let Some(ss) = ready.ss() {
                    let now_leader = ss.raft_state == StateRole::Leader;
                    if now_leader && !self.is_leader_flag {
                        tracing::info!(
                            "Raft node {}: became LEADER for partition {}",
                            self.config.node_id,
                            self.config.partition_id,
                        );
                        self.is_leader_flag = true;
                        (self.leadership_callbacks.on_became_leader)();
                    } else if !now_leader && self.is_leader_flag {
                        tracing::info!(
                            "Raft node {}: lost leadership for partition {}",
                            self.config.node_id,
                            self.config.partition_id,
                        );
                        self.is_leader_flag = false;
                        // Reject all pending proposals — we're no longer leader.
                        for (_, tx) in self.pending_proposals.drain() {
                            let _ = tx.send(false);
                        }
                        (self.leadership_callbacks.on_lost_leadership)();
                    }
                }

                // 1. Send messages to peers.
                for msg in ready.take_messages() {
                    let to = msg.to;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        if sender.send(msg).is_err() {
                            tracing::warn!("Failed to send Raft message to node {to}");
                        }
                    }
                }

                // 2. Snapshot transfer not yet implemented.
                // Nodes that fall too far behind must re-bootstrap.

                // 3+4. Persist log entries + hard state atomically.
                // TiKV WriteBatch pattern: entries and hard state go in one
                // atomic write to raft-engine for crash consistency.
                if let Some(hs) = ready.hs() {
                    if let Err(e) = self
                        .storage
                        .append_entries_and_hardstate(ready.entries(), hs)
                    {
                        tracing::error!("Failed to persist entries+hardstate: {e}");
                    }
                } else if let Err(e) = self.storage.append_entries(ready.entries()) {
                    tracing::error!("Failed to persist entries: {e}");
                }

                // 5. Send persisted messages.
                for msg in ready.take_persisted_messages() {
                    let to = msg.to;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        let _ = sender.send(msg);
                    }
                }

                // 6. Apply committed entries to state machine.
                for entry in ready.take_committed_entries() {
                    if entry.data.is_empty() {
                        // Raft internal entry (leader election, etc.)
                        continue;
                    }

                    match entry.get_entry_type() {
                        EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                            // Configuration change — apply to Raft membership.
                            let cc = ConfChange::default();
                            if let Ok(cs) = self.raw_node.apply_conf_change(&cc) {
                                self.storage.set_conf_state(cs);
                            }
                        },
                        EntryType::EntryNormal => {
                            // Normal entry — apply to state machine.
                            if let Err(e) = on_committed(&entry.data) {
                                tracing::error!("Failed to apply committed entry: {e}");
                            }

                            // Notify the proposer if this was our proposal.
                            if self.raw_node.raft.state == StateRole::Leader {
                                if let Some(tx) = self.pending_proposals.remove(&entry.index) {
                                    let _ = tx.send(true);
                                }
                            }
                        },
                    }
                }

                // 7. Advance the Raft state.
                let mut light_rd = self.raw_node.advance(ready);

                // Update commit index in persisted hard state.
                if let Some(commit) = light_rd.commit_index() {
                    let mut hs = self.storage.hard_state().unwrap_or_default();
                    hs.set_commit(commit);
                    if let Err(e) = self.storage.set_hardstate(&hs) {
                        tracing::error!("Failed to persist commit index: {e}");
                    }
                }

                // Send any remaining messages.
                for msg in light_rd.take_messages() {
                    let to = msg.to;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        let _ = sender.send(msg);
                    }
                }

                // Apply remaining committed entries.
                for entry in light_rd.take_committed_entries() {
                    if !entry.data.is_empty() && entry.get_entry_type() == EntryType::EntryNormal {
                        if let Err(e) = on_committed(&entry.data) {
                            tracing::error!("Failed to apply light committed entry: {e}");
                        }
                    }
                }

                self.raw_node.advance_apply();
            }

            // Yield to other tasks.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Set callbacks for leadership changes.
    /// Must be called before `run()`.
    pub fn set_leadership_callbacks(&mut self, callbacks: LeadershipCallbacks) {
        self.leadership_callbacks = callbacks;
    }

    /// Check if this node is the current Raft leader.
    pub fn is_leader(&self) -> bool {
        self.is_leader_flag
    }

    /// Get this node's ID.
    pub fn node_id(&self) -> u64 {
        self.config.node_id
    }

    /// Get the current leader ID (0 if unknown).
    pub fn leader_id(&self) -> u64 {
        self.raw_node.raft.leader_id
    }

    /// Get the partition this node manages.
    pub fn partition_id(&self) -> PartitionId {
        self.config.partition_id
    }

    /// Process one Ready cycle manually (for testing without the full run
    /// loop). Returns committed entry data.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn process_ready_test(&mut self) -> Vec<Vec<u8>> {
        let mut committed = Vec::new();
        if !self.raw_node.has_ready() {
            return committed;
        }
        let mut ready = self.raw_node.ready();

        // Track leadership and fire callbacks.
        if let Some(ss) = ready.ss() {
            let now_leader = ss.raft_state == StateRole::Leader;
            if now_leader && !self.is_leader_flag {
                self.is_leader_flag = true;
                (self.leadership_callbacks.on_became_leader)();
            } else if !now_leader && self.is_leader_flag {
                self.is_leader_flag = false;
                (self.leadership_callbacks.on_lost_leadership)();
            }
        }

        self.storage.append_entries(ready.entries()).unwrap();
        if let Some(hs) = ready.hs() {
            self.storage.set_hardstate(hs).unwrap();
        }
        for entry in ready.take_committed_entries() {
            if !entry.data.is_empty() {
                committed.push(entry.data.to_vec());
            }
        }
        let mut light_rd = self.raw_node.advance(ready);
        for entry in light_rd.take_committed_entries() {
            if !entry.data.is_empty() {
                committed.push(entry.data.to_vec());
            }
        }
        self.raw_node.advance_apply();
        committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft_storage::ConvexRaftStorage;

    fn test_engine() -> Arc<raft_engine::Engine> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so it lives for the test duration.
        let path = dir.into_path();
        ConvexRaftStorage::open_engine(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_single_node_election() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new()).unwrap();

        // Tick enough times to trigger election.
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();

        assert!(node.is_leader(), "Single node should become leader");
    }

    #[tokio::test]
    async fn test_propose_and_commit() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new()).unwrap();

        // Become leader first.
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();
        assert!(node.is_leader());

        // Propose data.
        node.raw_node
            .propose(vec![], b"test mutation".to_vec())
            .unwrap();

        // Process multiple ready cycles until the proposal is committed.
        let mut committed_data = Vec::new();
        for _ in 0..10 {
            node.raw_node.tick();
            committed_data.extend(node.process_ready_test());
            if committed_data.iter().any(|d| d == b"test mutation") {
                break;
            }
        }

        assert!(
            committed_data.iter().any(|d| d == b"test mutation"),
            "Proposed data should be committed. Got: {:?}",
            committed_data
        );
    }

    #[tokio::test]
    async fn test_leadership_callback() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new()).unwrap();

        let became_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let became_leader_clone = became_leader.clone();

        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new(move || {
                became_leader_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            on_lost_leadership: Box::new(|| {}),
        });

        assert!(!became_leader.load(std::sync::atomic::Ordering::SeqCst));

        // Trigger election.
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();

        assert!(node.is_leader());
        assert!(
            became_leader.load(std::sync::atomic::Ordering::SeqCst),
            "on_became_leader callback should have fired"
        );
    }
}
