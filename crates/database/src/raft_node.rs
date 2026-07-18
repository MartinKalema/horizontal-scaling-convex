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
    collections::{
        BTreeSet,
        HashMap,
    },
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use raft::{
    prelude::*,
    raw_node::RawNode,
    SnapshotStatus,
    StateRole,
};
use slog::o;
use tokio::sync::mpsc;

use crate::{
    metrics,
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

impl RaftNodeConfig {
    /// The Raft run loop ticks every 100ms.
    pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

    pub fn leader_serving_lease_duration(&self) -> Duration {
        let heartbeat = Self::TICK_INTERVAL * self.heartbeat_tick as u32;
        let election = Self::TICK_INTERVAL * self.election_tick as u32;
        let max_lease = election.saturating_sub(Self::TICK_INTERVAL);
        (heartbeat * 2).min(max_lease).max(Self::TICK_INTERVAL)
    }
}

/// A proposal from a client waiting to be committed via Raft.
pub struct RaftProposal {
    /// Serialized mutation data.
    pub data: Vec<u8>,
    /// Channel to notify the client when the proposal is committed.
    pub result_tx: tokio::sync::oneshot::Sender<RaftProposalResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftProposalResult {
    Committed { index: u64 },
    Rejected,
}

/// Messages that can be sent to the Raft node.
pub enum RaftMessage {
    /// A Raft protocol message from another node.
    Raft(Message),
    /// A client proposal to be committed.
    Propose(RaftProposal),
    /// Mark a live local proposal as durably applied to the Convex state
    /// machine after the committer has installed it locally.
    MarkApplied {
        index: u64,
        result_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    /// Shutdown the Raft node.
    Shutdown,
}

struct StateMachineApplyJob {
    index: u64,
    data: Vec<u8>,
}

struct StateMachineApplyResult {
    index: u64,
    result: anyhow::Result<()>,
}

enum CommittedEntryAction {
    WaitForLocalApply,
    MarkApplied(u64),
    Apply(StateMachineApplyJob),
}

/// Callbacks for leadership changes.
/// TiKV's pattern: the Raft node notifies the application when this node
/// becomes leader or loses leadership, so the application can start/stop
/// the Committer accordingly.
pub struct LeadershipCallbacks {
    /// Called when this node becomes the elected Raft leader. Application work
    /// must still wait for [`LeadershipCallbacks::on_leader_ready`].
    pub on_became_leader: Box<dyn FnMut() + Send>,
    /// Called when this node loses leadership (stepped down or partitioned).
    /// The Committer should stop accepting writes and drain pending proposals.
    pub on_lost_leadership: Box<dyn FnMut() + Send>,
    /// Called whenever the known leader changes (including on other nodes).
    /// Receives the new leader's node ID (0 if unknown).
    pub on_leader_changed: Box<dyn FnMut(u64) + Send>,
    /// Called after the current-term leader barrier has committed and the
    /// local Convex state machine has durably applied through it.
    pub on_leader_ready: Box<dyn FnMut() + Send>,
    /// Called when the current leader observes enough recent peer responses
    /// to keep serving coordinator-owned reads and subscriptions.
    pub on_leader_serving_lease_refreshed: Box<dyn FnMut() + Send>,
}

impl Default for LeadershipCallbacks {
    fn default() -> Self {
        Self {
            on_became_leader: Box::new(|| {}),
            on_lost_leadership: Box::new(|| {}),
            on_leader_changed: Box::new(|_| {}),
            on_leader_ready: Box::new(|| {}),
            on_leader_serving_lease_refreshed: Box::new(|| {}),
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
    pending_proposals: HashMap<u64, tokio::sync::oneshot::Sender<RaftProposalResult>>,
    /// Channel for receiving messages.
    mailbox: mpsc::UnboundedReceiver<RaftMessage>,
    /// Channels for sending Raft messages to other nodes.
    /// Maps node_id → sender.
    peer_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    /// Configuration.
    config: RaftNodeConfig,
    /// Whether this node is currently the leader.
    is_leader_flag: bool,
    /// Whether this leader has durably applied its current-term barrier.
    is_leader_ready_flag: bool,
    /// The current term and log index that must be durably applied before
    /// this elected leader may serve application traffic.
    leader_readiness_barrier: Option<(u64, u64)>,
    /// Leadership change callbacks.
    leadership_callbacks: LeadershipCallbacks,
    /// Highest Raft log index durably applied to the local Convex state
    /// machine.
    durable_applied_index: u64,
    /// Applied entries above `durable_applied_index` waiting for earlier gaps.
    applied_indexes_pending_persistence: BTreeSet<u64>,
}

fn is_leader_quorum_response(msg: &Message) -> bool {
    matches!(
        msg.get_msg_type(),
        MessageType::MsgAppendResponse
            | MessageType::MsgHeartbeatResponse
            | MessageType::MsgSnapStatus
    )
}

fn has_recent_peer_quorum(
    node_id: u64,
    peers: &[u64],
    recent_peer_responses: &HashMap<u64, Instant>,
    now: Instant,
    lease_duration: Duration,
) -> bool {
    let mut recent_voters = 1usize; // self
    for peer in peers {
        if *peer == node_id {
            continue;
        }
        if recent_peer_responses
            .get(peer)
            .is_some_and(|response_at| now.duration_since(*response_at) <= lease_duration)
        {
            recent_voters += 1;
        }
    }
    recent_voters * 2 > peers.len()
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
        snapshot_provider: Option<Arc<dyn crate::raft_storage::RaftSnapshotProvider>>,
    ) -> anyhow::Result<Self> {
        let storage = ConvexRaftStorage::new(
            config.partition_id,
            engine,
            config.node_id,
            config.peers.clone(),
            snapshot_provider,
        )?;

        let applied = storage.applied_index()?;

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
            is_leader_ready_flag: false,
            leader_readiness_barrier: None,
            leadership_callbacks: LeadershipCallbacks::default(),
            durable_applied_index: applied,
            applied_indexes_pending_persistence: BTreeSet::new(),
        })
    }

    fn record_applied_index(&mut self, index: u64) -> anyhow::Result<()> {
        if index <= self.durable_applied_index {
            return Ok(());
        }
        self.applied_indexes_pending_persistence.insert(index);

        let mut next_applied = self.durable_applied_index;
        while self
            .applied_indexes_pending_persistence
            .remove(&(next_applied + 1))
        {
            next_applied += 1;
        }
        if next_applied > self.durable_applied_index {
            self.storage.set_applied_index(next_applied)?;
            self.durable_applied_index = next_applied;
        }
        Ok(())
    }

    fn record_snapshot_applied_index(&mut self, index: u64) -> anyhow::Result<()> {
        if index > self.durable_applied_index {
            self.storage.set_applied_index(index)?;
            self.durable_applied_index = index;
        }
        self.applied_indexes_pending_persistence
            .retain(|pending_index| *pending_index > index);
        Ok(())
    }

    fn maybe_mark_leader_ready(&mut self) -> bool {
        if !self.is_leader_flag || self.is_leader_ready_flag {
            return false;
        }
        let Some((barrier_term, barrier_index)) = self.leader_readiness_barrier else {
            return false;
        };
        if self.raw_node.raft.term != barrier_term
            || self.raw_node.raft.raft_log.committed < barrier_index
            || self.durable_applied_index < barrier_index
        {
            return false;
        }

        self.is_leader_ready_flag = true;
        self.leader_readiness_barrier = None;
        tracing::info!(
            "Raft node {}: leader READY for partition {} after applying term {} barrier at index \
             {}",
            self.config.node_id,
            self.config.partition_id,
            barrier_term,
            barrier_index,
        );
        (self.leadership_callbacks.on_leader_ready)();
        true
    }

    fn maybe_refresh_leader_serving_lease(
        &mut self,
        recent_peer_responses: &HashMap<u64, Instant>,
        lease_duration: Duration,
    ) {
        if !self.is_leader_ready_flag {
            return;
        }
        if has_recent_peer_quorum(
            self.config.node_id,
            &self.config.peers,
            recent_peer_responses,
            Instant::now(),
            lease_duration,
        ) {
            (self.leadership_callbacks.on_leader_serving_lease_refreshed)();
        }
    }

    fn classify_committed_entry(&mut self, entry: Entry) -> anyhow::Result<CommittedEntryAction> {
        let index = entry.index;
        if entry.data.is_empty() {
            return Ok(CommittedEntryAction::MarkApplied(index));
        }

        match entry.get_entry_type() {
            EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                // Configuration change — apply to Raft membership.
                let cc = ConfChange::default();
                if let Ok(cs) = self.raw_node.apply_conf_change(&cc) {
                    if let Err(e) = self.storage.set_conf_state(cs) {
                        tracing::error!("Failed to persist conf state: {e}");
                        return Err(e);
                    }
                }
                Ok(CommittedEntryAction::MarkApplied(index))
            },
            EntryType::EntryNormal => {
                if let Some(tx) = self.pending_proposals.remove(&index) {
                    if tx.send(RaftProposalResult::Committed { index }).is_ok() {
                        return Ok(CommittedEntryAction::WaitForLocalApply);
                    }
                    tracing::warn!(
                        "Local Raft proposal at index {index} was committed but waiter dropped; \
                         applying through the Raft state-machine callback"
                    );
                }

                Ok(CommittedEntryAction::Apply(StateMachineApplyJob {
                    index,
                    data: entry.data.to_vec(),
                }))
            },
        }
    }

    fn finish_state_machine_apply(
        &mut self,
        applied: StateMachineApplyResult,
        pending_state_machine_applies: &mut BTreeSet<u64>,
    ) -> anyhow::Result<()> {
        pending_state_machine_applies.remove(&applied.index);
        applied.result?;
        self.record_applied_index(applied.index)?;
        self.raw_node.advance_apply_to(self.durable_applied_index);
        Ok(())
    }

    fn process_mailbox_message(
        &mut self,
        msg: RaftMessage,
        recent_peer_responses: &mut HashMap<u64, Instant>,
        leader_serving_lease_duration: Duration,
        pending_local_applies: &mut BTreeSet<u64>,
    ) -> anyhow::Result<bool> {
        match msg {
            RaftMessage::Raft(msg) => {
                if self.raw_node.raft.state == StateRole::Leader
                    && is_leader_quorum_response(&msg)
                    && msg.from != self.config.node_id
                {
                    let now = Instant::now();
                    recent_peer_responses.insert(msg.from, now);
                    self.maybe_refresh_leader_serving_lease(
                        recent_peer_responses,
                        leader_serving_lease_duration,
                    );
                }
                if let Err(e) = self.raw_node.step(msg) {
                    tracing::warn!("Raft step error: {e}");
                }
                Ok(false)
            },
            RaftMessage::Propose(proposal) => {
                if self.raw_node.raft.state == StateRole::Leader && self.is_leader_ready_flag {
                    let index = self.raw_node.raft.raft_log.last_index() + 1;
                    if let Err(e) = self.raw_node.propose(vec![], proposal.data) {
                        tracing::warn!("Raft propose error: {e}");
                        let _ = proposal.result_tx.send(RaftProposalResult::Rejected);
                    } else {
                        self.pending_proposals.insert(index, proposal.result_tx);
                    }
                } else {
                    // Not leader — reject.
                    let _ = proposal.result_tx.send(RaftProposalResult::Rejected);
                }
                Ok(false)
            },
            RaftMessage::MarkApplied { index, result_tx } => {
                let result = self.record_applied_index(index);
                if result.is_ok() {
                    pending_local_applies.remove(&index);
                    self.raw_node.advance_apply_to(self.durable_applied_index);
                    self.maybe_mark_leader_ready();
                    self.maybe_refresh_leader_serving_lease(
                        recent_peer_responses,
                        leader_serving_lease_duration,
                    );
                }
                let _ = result_tx.send(result);
                Ok(false)
            },
            RaftMessage::Shutdown => {
                tracing::info!("Raft node {} shutting down", self.config.node_id);
                Ok(true)
            },
        }
    }

    fn tick_election_and_heartbeat(&mut self, last_tick: &mut Instant) {
        self.raw_node.tick();
        if self.raw_node.raft.state == StateRole::Leader
            && self.is_leader_ready_flag
            && self.config.peers.len() == 1
        {
            (self.leadership_callbacks.on_leader_serving_lease_refreshed)();
        }
        *last_tick = Instant::now();
    }

    /// Run the Raft loop. This is the main event loop following TiKV's pattern:
    /// tick → receive messages → propose → process ready → advance.
    pub async fn run(
        &mut self,
        mut on_committed: impl FnMut(&[u8]) -> anyhow::Result<()> + Send + 'static,
        mut on_snapshot: impl FnMut(&[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let tick_interval = RaftNodeConfig::TICK_INTERVAL;
        let leader_serving_lease_duration = self.config.leader_serving_lease_duration();
        let mut recent_peer_responses = HashMap::new();
        let mut last_tick = Instant::now();
        let mut pending_state_machine_applies = BTreeSet::new();
        let mut pending_local_applies = BTreeSet::new();
        let (apply_tx, mut apply_rx) = mpsc::unbounded_channel::<StateMachineApplyJob>();
        let (applied_tx, mut applied_rx) = mpsc::unbounded_channel::<StateMachineApplyResult>();
        let _apply_worker = tokio::task::spawn_blocking(move || {
            while let Some(job) = apply_rx.blocking_recv() {
                let result = on_committed(&job.data);
                let should_stop = result.is_err();
                if applied_tx
                    .send(StateMachineApplyResult {
                        index: job.index,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                if should_stop {
                    break;
                }
            }
        });

        loop {
            // Receive all pending messages (non-blocking).
            let mut handled_message = false;
            loop {
                match self.mailbox.try_recv() {
                    Ok(msg) => {
                        handled_message = true;
                        if self.process_mailbox_message(
                            msg,
                            &mut recent_peer_responses,
                            leader_serving_lease_duration,
                            &mut pending_local_applies,
                        )? {
                            return Ok(());
                        }
                    },
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        tracing::info!("Raft mailbox closed, shutting down");
                        return Ok(());
                    },
                }
            }

            let mut handled_apply_result = false;
            loop {
                match applied_rx.try_recv() {
                    Ok(applied) => {
                        handled_apply_result = true;
                        if let Err(e) = self
                            .finish_state_machine_apply(applied, &mut pending_state_machine_applies)
                        {
                            tracing::error!("Failed to finish state-machine apply: {e}");
                            return Err(e);
                        }
                        self.maybe_mark_leader_ready();
                        self.maybe_refresh_leader_serving_lease(
                            &recent_peer_responses,
                            leader_serving_lease_duration,
                        );
                    },
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        if pending_state_machine_applies.is_empty() {
                            break;
                        }
                        anyhow::bail!(
                            "Raft state-machine apply worker stopped with {} pending applies",
                            pending_state_machine_applies.len(),
                        );
                    },
                }
            }

            let should_tick = last_tick.elapsed() >= tick_interval;
            if should_tick {
                self.tick_election_and_heartbeat(&mut last_tick);
            }

            let mut processed_ready = false;
            while self.raw_node.has_ready() {
                if !pending_state_machine_applies.is_empty() || !pending_local_applies.is_empty() {
                    break;
                }
                processed_ready = true;

                let replication_health = {
                    let status = self.raw_node.status();
                    status.progress.map(|progress| {
                        let voter_ids: Vec<u64> = progress.conf().voters().ids().iter().collect();
                        let configured_voters = voter_ids.len();
                        let leader_committed = status.hs.commit;
                        let mut recent_active_voters = 0usize;
                        let mut lagging_followers = 0usize;
                        let mut max_follower_lag_entries = 0u64;
                        let mut total_follower_lag_entries = 0u64;

                        for voter_id in voter_ids {
                            if voter_id == self.config.node_id {
                                recent_active_voters += 1;
                                continue;
                            }
                            let Some(peer_progress) = progress.get(voter_id) else {
                                continue;
                            };
                            if peer_progress.recent_active {
                                recent_active_voters += 1;
                            }
                            let lag = leader_committed.saturating_sub(peer_progress.matched);
                            if lag > 0 {
                                lagging_followers += 1;
                            }
                            max_follower_lag_entries = max_follower_lag_entries.max(lag);
                            total_follower_lag_entries =
                                total_follower_lag_entries.saturating_add(lag);
                        }

                        (
                            configured_voters,
                            recent_active_voters,
                            lagging_followers,
                            max_follower_lag_entries,
                            total_follower_lag_entries,
                        )
                    })
                };
                let mut ready = self.raw_node.ready();

                // Detect leadership changes (TiKV SoftState pattern).
                // SoftState is present in Ready only when it changes.
                if let Some(ss) = ready.ss() {
                    let now_leader = ss.raft_state == StateRole::Leader;

                    // Always propagate current leader ID — followers need
                    // this to redirect write rejections to the correct node.
                    (self.leadership_callbacks.on_leader_changed)(ss.leader_id);

                    if now_leader && !self.is_leader_flag {
                        tracing::info!(
                            "Raft node {}: became LEADER for partition {}",
                            self.config.node_id,
                            self.config.partition_id,
                        );
                        self.is_leader_flag = true;
                        self.is_leader_ready_flag = false;
                        // raft-rs appends a no-op entry in the new term when it
                        // becomes leader. Applying through the current last
                        // index proves this state machine has caught up through
                        // that current-term barrier.
                        self.leader_readiness_barrier = Some((
                            self.raw_node.raft.term,
                            self.raw_node.raft.raft_log.last_index(),
                        ));
                        (self.leadership_callbacks.on_became_leader)();
                    } else if !now_leader && self.is_leader_flag {
                        tracing::info!(
                            "Raft node {}: lost leadership for partition {}",
                            self.config.node_id,
                            self.config.partition_id,
                        );
                        self.is_leader_flag = false;
                        self.is_leader_ready_flag = false;
                        self.leader_readiness_barrier = None;
                        // Reject all pending proposals — we're no longer leader.
                        for (_, tx) in self.pending_proposals.drain() {
                            let _ = tx.send(RaftProposalResult::Rejected);
                        }
                        (self.leadership_callbacks.on_lost_leadership)();
                    }
                }

                metrics::log_raft_term(self.config.partition_id, self.raw_node.raft.term);
                metrics::log_raft_committed_index(
                    self.config.partition_id,
                    self.raw_node.raft.raft_log.committed,
                );
                metrics::log_raft_applied_index(
                    self.config.partition_id,
                    self.raw_node.raft.raft_log.applied,
                );
                if let Some((
                    configured_voters,
                    recent_active_voters,
                    lagging_followers,
                    max_follower_lag_entries,
                    total_follower_lag_entries,
                )) = replication_health
                {
                    metrics::log_raft_replication_health(
                        self.config.partition_id,
                        configured_voters,
                        recent_active_voters,
                        lagging_followers,
                        max_follower_lag_entries,
                        total_follower_lag_entries,
                    );
                } else {
                    metrics::reset_raft_replication_health(self.config.partition_id);
                }

                // 1. Send messages to peers.
                for msg in ready.take_messages() {
                    let to = msg.to;
                    let is_snapshot = msg.get_msg_type() == MessageType::MsgSnapshot;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        if sender.send(msg).is_err() {
                            tracing::warn!("Failed to send Raft message to node {to}");
                            if is_snapshot {
                                self.raw_node.report_snapshot(to, SnapshotStatus::Failure);
                            }
                        } else if is_snapshot {
                            self.raw_node.report_snapshot(to, SnapshotStatus::Finish);
                        }
                    }
                }

                // 2. Apply and persist any incoming snapshot before appending entries.
                if !ready.snapshot().is_empty() {
                    let snapshot_timer = metrics::raft_snapshot_apply_timer();
                    let snapshot_bytes = ready.snapshot().get_data().len();
                    metrics::log_raft_snapshot_apply_bytes(snapshot_bytes);
                    if let Err(e) = on_snapshot(ready.snapshot().get_data()) {
                        tracing::error!("Failed to apply snapshot to state machine: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else if let Err(e) = self.storage.apply_snapshot(ready.snapshot()) {
                        tracing::error!("Failed to persist snapshot: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else if let Err(e) =
                        self.record_snapshot_applied_index(ready.snapshot().get_metadata().index)
                    {
                        tracing::error!("Failed to persist snapshot applied index: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else {
                        snapshot_timer.finish();
                    }
                }

                // 3+4. Persist log entries + hard state atomically.
                // TiKV WriteBatch pattern: entries and hard state go in one
                // atomic write to raft-engine for crash consistency.
                if let Some(hs) = ready.hs() {
                    if let Err(e) = self
                        .storage
                        .append_entries_and_hardstate(ready.entries(), hs)
                    {
                        tracing::error!("Failed to persist entries+hardstate: {e}");
                        return Err(e);
                    }
                } else if let Err(e) = self.storage.append_entries(ready.entries()) {
                    tracing::error!("Failed to persist entries: {e}");
                    return Err(e);
                }

                // 5. Send persisted messages.
                for msg in ready.take_persisted_messages() {
                    let to = msg.to;
                    let is_snapshot = msg.get_msg_type() == MessageType::MsgSnapshot;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        if sender.send(msg).is_err() {
                            if is_snapshot {
                                self.raw_node.report_snapshot(to, SnapshotStatus::Failure);
                            }
                        } else if is_snapshot {
                            self.raw_node.report_snapshot(to, SnapshotStatus::Finish);
                        }
                    }
                }

                let mut applied_indexes = Vec::new();
                let mut apply_jobs = Vec::new();

                // 6. Classify committed entries. Entries that require Convex
                // state-machine work are offloaded after append/persist progress
                // has advanced.
                for entry in ready.take_committed_entries() {
                    let index = entry.index;
                    match self.classify_committed_entry(entry)? {
                        CommittedEntryAction::WaitForLocalApply => {
                            pending_local_applies.insert(index);
                        },
                        CommittedEntryAction::MarkApplied(index) => applied_indexes.push(index),
                        CommittedEntryAction::Apply(job) => apply_jobs.push(job),
                    }
                }

                // 7. Advance append/persist state without claiming
                // state-machine apply completion. `advance_apply_to` is called
                // only when local MarkApplied or the apply worker records a
                // durable applied index.
                let mut light_rd = self.raw_node.advance_append(ready);

                // Update commit index in persisted hard state.
                if let Some(commit) = light_rd.commit_index() {
                    let mut hs = self.storage.hard_state().unwrap_or_default();
                    hs.set_commit(commit);
                    if let Err(e) = self.storage.set_hardstate(&hs) {
                        tracing::error!("Failed to persist commit index: {e}");
                        return Err(e);
                    }
                }

                // Send any remaining messages.
                for msg in light_rd.take_messages() {
                    let to = msg.to;
                    if let Some(sender) = self.peer_senders.get(&to) {
                        let _ = sender.send(msg);
                    }
                }

                // Classify remaining committed entries.
                for entry in light_rd.take_committed_entries() {
                    let index = entry.index;
                    match self.classify_committed_entry(entry)? {
                        CommittedEntryAction::WaitForLocalApply => {
                            pending_local_applies.insert(index);
                        },
                        CommittedEntryAction::MarkApplied(index) => applied_indexes.push(index),
                        CommittedEntryAction::Apply(job) => apply_jobs.push(job),
                    }
                }

                for index in applied_indexes {
                    self.record_applied_index(index)?;
                }
                if !apply_jobs.is_empty() {
                    for job in apply_jobs {
                        pending_state_machine_applies.insert(job.index);
                        if apply_tx.send(job).is_err() {
                            anyhow::bail!("Raft state-machine apply worker stopped");
                        }
                    }
                }
                self.raw_node.advance_apply_to(self.durable_applied_index);
                self.maybe_mark_leader_ready();
                self.maybe_refresh_leader_serving_lease(
                    &recent_peer_responses,
                    leader_serving_lease_duration,
                );
            }

            if handled_message || handled_apply_result || should_tick || processed_ready {
                tokio::task::yield_now().await;
                continue;
            }

            let next_tick = last_tick + tick_interval;
            tokio::select! {
                maybe_msg = self.mailbox.recv() => {
                    let Some(msg) = maybe_msg else {
                        tracing::info!("Raft mailbox closed, shutting down");
                        return Ok(());
                    };
                    if self.process_mailbox_message(
                        msg,
                        &mut recent_peer_responses,
                        leader_serving_lease_duration,
                        &mut pending_local_applies,
                    )? {
                        return Ok(());
                    }
                }
                maybe_applied = applied_rx.recv() => {
                    let Some(applied) = maybe_applied else {
                        if pending_state_machine_applies.is_empty() {
                            continue;
                        }
                        anyhow::bail!(
                            "Raft state-machine apply worker stopped with {} pending applies",
                            pending_state_machine_applies.len(),
                        );
                    };
                    if let Err(e) = self.finish_state_machine_apply(
                        applied,
                        &mut pending_state_machine_applies,
                    ) {
                        tracing::error!("Failed to finish state-machine apply: {e}");
                        return Err(e);
                    }
                    self.maybe_mark_leader_ready();
                    self.maybe_refresh_leader_serving_lease(
                        &recent_peer_responses,
                        leader_serving_lease_duration,
                    );
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_tick)) => {
                    self.tick_election_and_heartbeat(&mut last_tick);
                }
            }
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

    /// Check whether this elected leader has durably applied through its
    /// current-term barrier and may accept application work.
    pub fn is_leader_ready(&self) -> bool {
        self.is_leader_flag && self.is_leader_ready_flag
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
    #[cfg(test)]
    pub(crate) fn try_process_ready_test_with_apply(
        &mut self,
        mut on_committed: impl FnMut(&[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut committed = Vec::new();
        if !self.raw_node.has_ready() {
            return Ok(committed);
        }
        let mut ready = self.raw_node.ready();

        // Track leadership and fire callbacks.
        if let Some(ss) = ready.ss() {
            (self.leadership_callbacks.on_leader_changed)(ss.leader_id);
            let now_leader = ss.raft_state == StateRole::Leader;
            if now_leader && !self.is_leader_flag {
                self.is_leader_flag = true;
                self.is_leader_ready_flag = false;
                self.leader_readiness_barrier = Some((
                    self.raw_node.raft.term,
                    self.raw_node.raft.raft_log.last_index(),
                ));
                (self.leadership_callbacks.on_became_leader)();
            } else if !now_leader && self.is_leader_flag {
                self.is_leader_flag = false;
                self.is_leader_ready_flag = false;
                self.leader_readiness_barrier = None;
                (self.leadership_callbacks.on_lost_leadership)();
            }
        }

        if !ready.snapshot().is_empty() {
            self.storage.apply_snapshot(ready.snapshot())?;
            self.record_snapshot_applied_index(ready.snapshot().get_metadata().index)?;
        }
        if let Some(hs) = ready.hs() {
            self.storage
                .append_entries_and_hardstate(ready.entries(), hs)?;
        } else {
            self.storage.append_entries(ready.entries())?;
        }
        let mut applied_indexes = Vec::new();
        let mut apply_jobs = Vec::new();
        for entry in ready.take_committed_entries() {
            match self.classify_committed_entry(entry)? {
                CommittedEntryAction::WaitForLocalApply => {},
                CommittedEntryAction::MarkApplied(index) => applied_indexes.push(index),
                CommittedEntryAction::Apply(job) => apply_jobs.push(job),
            }
        }
        let mut light_rd = self.raw_node.advance_append(ready);
        if let Some(commit) = light_rd.commit_index() {
            let mut hs = self.storage.hard_state().unwrap_or_default();
            hs.set_commit(commit);
            self.storage.set_hardstate(&hs)?;
        }
        for entry in light_rd.take_committed_entries() {
            match self.classify_committed_entry(entry)? {
                CommittedEntryAction::WaitForLocalApply => {},
                CommittedEntryAction::MarkApplied(index) => applied_indexes.push(index),
                CommittedEntryAction::Apply(job) => apply_jobs.push(job),
            }
        }
        for index in applied_indexes {
            self.record_applied_index(index)?;
        }
        for job in apply_jobs {
            on_committed(&job.data)?;
            self.record_applied_index(job.index)?;
            committed.push(job.data);
        }
        self.raw_node.advance_apply_to(self.durable_applied_index);
        self.maybe_mark_leader_ready();
        if self.config.peers.len() == 1 && self.is_leader_ready_flag {
            (self.leadership_callbacks.on_leader_serving_lease_refreshed)();
        }
        Ok(committed)
    }

    #[cfg(test)]
    pub(crate) fn try_process_ready_test(&mut self) -> anyhow::Result<Vec<Vec<u8>>> {
        self.try_process_ready_test_with_apply(|_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn process_ready_test(&mut self) -> Vec<Vec<u8>> {
        self.try_process_ready_test().unwrap()
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
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        // Tick enough times to trigger election.
        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();

        assert!(node.is_leader(), "Single node should become leader");
        assert!(
            node.is_leader_ready(),
            "single-node leader should apply its current-term barrier"
        );
    }

    #[tokio::test]
    async fn test_elected_leader_waits_for_committed_prefix_before_ready() -> anyhow::Result<()> {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let engine = test_engine();
        {
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine.clone(), 1, vec![1], None).unwrap();
            let mut entry = Entry::default();
            entry.set_index(1);
            entry.set_term(1);
            entry.set_data(b"committed before election".to_vec().into());
            let mut hs = HardState::default();
            hs.set_term(1);
            hs.set_commit(1);
            storage.append_entries_and_hardstate(&[entry], &hs).unwrap();
            assert_eq!(storage.applied_index().unwrap(), 0);
        }

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new({
                let events = events.clone();
                move || events.lock().unwrap().push("elected")
            }),
            on_leader_ready: Box::new({
                let events = events.clone();
                move || events.lock().unwrap().push("ready")
            }),
            on_leader_serving_lease_refreshed: Box::new({
                let events = events.clone();
                move || events.lock().unwrap().push("lease")
            }),
            on_lost_leadership: Box::new(|| {}),
            on_leader_changed: Box::new(|_| {}),
        });

        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.try_process_ready_test_with_apply({
            let events = events.clone();
            move |data| {
                assert_eq!(data, b"committed before election");
                let observed = events.lock().unwrap().clone();
                assert_eq!(
                    observed,
                    vec!["elected"],
                    "readiness and lease must remain closed while the committed prefix applies",
                );
                events.lock().unwrap().push("applied");
                Ok(())
            }
        })?;

        assert!(node.is_leader());
        assert!(node.is_leader_ready());
        assert_eq!(node.storage.applied_index()?, 2);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["elected", "applied", "ready", "lease"],
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_elected_but_unready_leader_rejects_proposals() -> anyhow::Result<()> {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let engine = test_engine();
        let (_mailbox_tx, mailbox_rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, mailbox_rx, HashMap::new(), None)?;
        for _ in 0..20 {
            node.raw_node.tick();
        }
        assert_eq!(node.raw_node.raft.state, StateRole::Leader);
        assert!(!node.is_leader_ready());

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        node.process_mailbox_message(
            RaftMessage::Propose(RaftProposal {
                data: b"must wait for readiness".to_vec(),
                result_tx,
            }),
            &mut HashMap::new(),
            Duration::from_secs(1),
            &mut BTreeSet::new(),
        )?;
        assert_eq!(result_rx.await?, RaftProposalResult::Rejected);
        Ok(())
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
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

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
    async fn test_run_loop_wakes_on_mailbox_proposal() -> anyhow::Result<()> {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();
        assert!(node.is_leader());

        let run_task = tokio::spawn(async move { node.run(|_| Ok(()), |_| Ok(())).await });
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tx.send(RaftMessage::Propose(RaftProposal {
            data: b"mailbox proposal".to_vec(),
            result_tx,
        }))?;

        let result = tokio::time::timeout(Duration::from_millis(50), result_rx).await??;
        assert!(matches!(result, RaftProposalResult::Committed { .. }));

        tx.send(RaftMessage::Shutdown)?;
        run_task.await??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_loop_handles_mailbox_while_state_machine_apply_is_blocked(
    ) -> anyhow::Result<()> {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        {
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine.clone(), 1, vec![1], None).unwrap();
            let mut entry = Entry::default();
            entry.set_index(1);
            entry.set_term(1);
            entry.set_data(b"blocked apply".to_vec().into());

            let mut hs = HardState::default();
            hs.set_term(1);
            hs.set_commit(1);
            storage.append_entries_and_hardstate(&[entry], &hs).unwrap();
            assert_eq!(storage.applied_index().unwrap(), 0);
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel();
        let run_task = tokio::spawn(async move {
            let mut entered_tx = Some(entered_tx);
            node.run(
                move |_| {
                    if let Some(entered_tx) = entered_tx.take() {
                        let _ = entered_tx.send(());
                    }
                    unblock_rx.recv().map_err(|_| {
                        anyhow::anyhow!("blocked apply test unblock channel closed")
                    })?;
                    Ok(())
                },
                |_| Ok(()),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .map_err(|_| anyhow::anyhow!("state-machine apply callback did not start"))??;
        tx.send(RaftMessage::Shutdown)?;

        tokio::time::timeout(Duration::from_secs(1), run_task)
            .await
            .map_err(|_| {
                anyhow::anyhow!("Raft run loop did not process shutdown while apply blocked")
            })???;
        unblock_tx.send(())?;
        Ok(())
    }

    #[tokio::test]
    async fn test_ready_persistence_failure_returns_before_advance() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();
        assert!(node.is_leader());

        let persisted_last_index = node.storage.last_index().unwrap();
        node.raw_node
            .propose(vec![], b"must not persist".to_vec())
            .unwrap();
        node.storage.fail_next_write_for_test();

        let err = node
            .try_process_ready_test()
            .expect_err("Ready processing should fail on raft-engine write failure");
        assert!(
            format!("{err:#}").contains("injected raft-engine write failure"),
            "unexpected error: {err:#}",
        );
        assert_eq!(
            node.storage.last_index().unwrap(),
            persisted_last_index,
            "failed Ready processing must not persist the proposed entry",
        );
    }

    #[tokio::test]
    async fn test_state_machine_apply_failure_returns_before_advance_apply() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        for _ in 0..20 {
            node.raw_node.tick();
        }
        node.process_ready_test();
        assert!(node.is_leader());

        node.raw_node
            .propose(vec![], b"poison entry".to_vec())
            .unwrap();
        let applied_before = node.raw_node.raft.raft_log.applied;

        let err = node
            .try_process_ready_test_with_apply(|_| {
                anyhow::bail!("injected state machine apply failure")
            })
            .expect_err("Ready processing should fail on state-machine apply failure");
        assert!(
            format!("{err:#}").contains("injected state machine apply failure"),
            "unexpected error: {err:#}",
        );
        assert_eq!(
            node.raw_node.raft.raft_log.applied, applied_before,
            "failed state-machine apply must not advance the Raft applied index",
        );
    }

    #[tokio::test]
    async fn test_restart_replays_committed_but_unapplied_entry() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let engine = test_engine();

        {
            let storage =
                ConvexRaftStorage::new(PartitionId(0), engine.clone(), 1, vec![1], None).unwrap();
            let mut entry = Entry::default();
            entry.set_index(1);
            entry.set_term(1);
            entry.set_data(b"committed but unapplied".to_vec().into());

            let mut hs = HardState::default();
            hs.set_term(1);
            hs.set_commit(1);
            storage.append_entries_and_hardstate(&[entry], &hs).unwrap();
            assert_eq!(storage.applied_index().unwrap(), 0);
        }

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();
        assert_eq!(
            node.raw_node.raft.raft_log.applied, 0,
            "restart must initialize applied from durable applied index, not HardState.commit",
        );

        let committed = node.process_ready_test();
        assert!(
            committed
                .iter()
                .any(|data| data == b"committed but unapplied"),
            "committed-but-unapplied entry should replay after restart: {committed:?}",
        );
        assert_eq!(node.storage.applied_index().unwrap(), 1);
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
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        let observed_leader_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed_leader_id_clone = observed_leader_id.clone();
        let became_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let became_leader_clone = became_leader.clone();

        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new(move || {
                became_leader_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            on_leader_changed: Box::new(move |leader_id| {
                observed_leader_id_clone.store(leader_id, std::sync::atomic::Ordering::SeqCst);
            }),
            on_lost_leadership: Box::new(|| {}),
            on_leader_ready: Box::new(|| {}),
            on_leader_serving_lease_refreshed: Box::new(|| {}),
        });

        assert!(!became_leader.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            observed_leader_id.load(std::sync::atomic::Ordering::SeqCst),
            0
        );

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
        assert_eq!(
            observed_leader_id.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "on_leader_changed should publish the elected leader ID"
        );
    }
}
