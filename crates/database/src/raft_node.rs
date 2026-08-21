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
        VecDeque,
    },
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use protobuf::Message as ProtoMessage;
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
    ConfigurationApplied,
    Apply(StateMachineApplyJob),
}

/// Callbacks for leadership changes.
/// TiKV's pattern: the Raft node notifies the application when this node
/// becomes leader or loses leadership, so the application can start/stop
/// the Committer accordingly.
pub struct LeadershipCallbacks {
    /// Called with the new term when this node becomes the elected Raft leader.
    /// Application work must still wait for
    /// [`LeadershipCallbacks::on_leader_ready`].
    pub on_became_leader: Box<dyn FnMut(u64) + Send>,
    /// Called when this node loses leadership (stepped down or partitioned).
    /// The Committer should stop accepting writes and drain pending proposals.
    pub on_lost_leadership: Box<dyn FnMut() + Send>,
    /// Called whenever the known leader changes (including on other nodes).
    /// Receives the new leader's node ID (0 if unknown).
    pub on_leader_changed: Box<dyn FnMut(u64) + Send>,
    /// Called after the current-term leader barrier has committed and the
    /// local Convex state machine has durably applied through it.
    pub on_leader_ready: Box<dyn FnMut() + Send>,
    /// Called with the absolute deadline justified by validated, current-term
    /// quorum evidence. Callers must install this exact deadline rather than
    /// extending a fresh lease from callback time.
    pub on_leader_serving_lease_refreshed: Box<dyn FnMut(Instant) + Send>,
}

impl Default for LeadershipCallbacks {
    fn default() -> Self {
        Self {
            on_became_leader: Box::new(|_| {}),
            on_lost_leadership: Box::new(|| {}),
            on_leader_changed: Box::new(|_| {}),
            on_leader_ready: Box::new(|| {}),
            on_leader_serving_lease_refreshed: Box::new(|_| {}),
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
    /// Monotonic nonce used to namespace this node's Safe ReadIndex lease
    /// rounds from every other ReadOnly request.
    next_serving_lease_round_nonce: u64,
    /// At most one quorum round may mint a serving lease. Its deadline is
    /// fixed when the ReadIndex request is sent, never when responses arrive.
    pending_serving_lease_round: Option<ServingLeaseRound>,
    /// Last absolute deadline published to the application.
    published_serving_lease_expires_at: Option<Instant>,
    /// Identity proof for local proposals that were already committed before
    /// the currently pending incoming snapshot entered Raft.
    pending_snapshot_proposal_identity: Option<SnapshotProposalIdentity>,
}

const SERVING_LEASE_READ_INDEX_CONTEXT_PREFIX: &[u8] = b"convex-serving-lease-v1\0";

#[derive(Clone, Debug)]
struct ServingLeaseRound {
    term: u64,
    context: Vec<u8>,
    valid_until: Instant,
}

#[derive(Clone, Debug)]
struct SnapshotProposalIdentity {
    snapshot_index: u64,
    snapshot_term: u64,
    locally_committed_proposal_indexes: BTreeSet<u64>,
}

impl SnapshotProposalIdentity {
    fn matches(&self, snapshot: &Snapshot) -> bool {
        let metadata = snapshot.get_metadata();
        self.snapshot_index == metadata.index && self.snapshot_term == metadata.term
    }
}

fn serving_lease_read_index_context(node_id: u64, term: u64, nonce: u64) -> Vec<u8> {
    let mut context = Vec::with_capacity(SERVING_LEASE_READ_INDEX_CONTEXT_PREFIX.len() + 24);
    context.extend_from_slice(SERVING_LEASE_READ_INDEX_CONTEXT_PREFIX);
    context.extend_from_slice(&node_id.to_be_bytes());
    context.extend_from_slice(&term.to_be_bytes());
    context.extend_from_slice(&nonce.to_be_bytes());
    context
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
            read_only_option: ReadOnlyOption::Safe,
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
            next_serving_lease_round_nonce: 0,
            pending_serving_lease_round: None,
            published_serving_lease_expires_at: None,
            pending_snapshot_proposal_identity: None,
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
        anyhow::ensure!(
            self.storage.applied_index()? == index,
            "Raft snapshot metadata committed index {} but storage reports applied index {}",
            index,
            self.storage.applied_index()?,
        );
        self.durable_applied_index = self.durable_applied_index.max(index);
        self.applied_indexes_pending_persistence
            .retain(|pending_index| *pending_index > index);
        Ok(())
    }

    fn snapshot_proposal_identity_at_frontier(
        &self,
        snapshot: &Snapshot,
        local_committed_frontier: u64,
    ) -> SnapshotProposalIdentity {
        let metadata = snapshot.get_metadata();
        let locally_committed_proposal_indexes = self
            .pending_proposals
            .keys()
            .copied()
            .filter(|index| *index <= local_committed_frontier && *index <= metadata.index)
            .collect();
        SnapshotProposalIdentity {
            snapshot_index: metadata.index,
            snapshot_term: metadata.term,
            locally_committed_proposal_indexes,
        }
    }

    fn snapshot_proposal_identity_before_step(
        &self,
        snapshot: &Snapshot,
    ) -> SnapshotProposalIdentity {
        // Once an incoming snapshot has entered Raft, its index may become the
        // in-memory committed frontier. A replacement snapshot must carry
        // forward the original local proposal proof rather than deriving new
        // proof from that snapshot-influenced frontier.
        let carried_proposal_indexes = self
            .raw_node
            .snap()
            .and_then(|pending_snapshot| {
                self.pending_snapshot_proposal_identity
                    .as_ref()
                    .filter(|identity| identity.matches(pending_snapshot))
            })
            .map(|identity| identity.locally_committed_proposal_indexes.clone());
        let metadata = snapshot.get_metadata();
        match carried_proposal_indexes {
            Some(locally_committed_proposal_indexes) => {
                let locally_committed_proposal_indexes = locally_committed_proposal_indexes
                    .into_iter()
                    .filter(|index| {
                        *index <= metadata.index && self.pending_proposals.contains_key(index)
                    })
                    .collect();
                SnapshotProposalIdentity {
                    snapshot_index: metadata.index,
                    snapshot_term: metadata.term,
                    locally_committed_proposal_indexes,
                }
            },
            None => self.snapshot_proposal_identity_at_frontier(
                snapshot,
                self.raw_node.raft.raft_log.committed,
            ),
        }
    }

    fn snapshot_proposal_identity_for_install(
        &self,
        snapshot: &Snapshot,
    ) -> SnapshotProposalIdentity {
        if let Some(identity) = self
            .pending_snapshot_proposal_identity
            .as_ref()
            .filter(|identity| identity.matches(snapshot))
        {
            return identity.clone();
        }

        // A snapshot that bypassed the mailbox has no pre-acceptance identity
        // proof. Fail closed: snapshot coverage alone cannot prove any pending
        // local proposal committed.
        let metadata = snapshot.get_metadata();
        SnapshotProposalIdentity {
            snapshot_index: metadata.index,
            snapshot_term: metadata.term,
            locally_committed_proposal_indexes: BTreeSet::new(),
        }
    }

    fn reconcile_snapshot_proposals(
        &mut self,
        snapshot_index: u64,
        identity: &SnapshotProposalIdentity,
    ) {
        let pending_proposals = std::mem::take(&mut self.pending_proposals);
        for (index, tx) in pending_proposals {
            if identity.locally_committed_proposal_indexes.contains(&index) {
                let _ = tx.send(RaftProposalResult::Committed { index });
            } else if index <= snapshot_index {
                // Snapshot coverage proves only that *some* entry occupies this
                // index. Without pre-snapshot local commit evidence, it may be a
                // replacement for an unresolved old-leader proposal.
                let _ = tx.send(RaftProposalResult::Rejected);
            } else {
                self.pending_proposals.insert(index, tx);
            }
        }
    }

    fn finish_snapshot_application(
        &mut self,
        snapshot: &Snapshot,
        proposal_identity: &SnapshotProposalIdentity,
        applied_at: Instant,
    ) -> anyhow::Result<()> {
        let metadata = snapshot.get_metadata();
        anyhow::ensure!(
            proposal_identity.matches(snapshot),
            "Raft snapshot proposal identity [{}, {}] does not match installed snapshot [{}, {}]",
            proposal_identity.snapshot_index,
            proposal_identity.snapshot_term,
            metadata.index,
            metadata.term,
        );
        self.record_snapshot_applied_index(metadata.index)?;
        self.reconcile_snapshot_proposals(metadata.index, proposal_identity);
        self.pending_snapshot_proposal_identity = None;
        self.invalidate_leader_serving_lease_for_configuration_change(applied_at);
        self.reconcile_application_leadership(metadata.get_conf_state());
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
        let _ = self.maybe_start_leader_serving_lease_round(Instant::now());
        true
    }

    fn reset_leader_serving_lease_round(&mut self) {
        self.pending_serving_lease_round = None;
        self.published_serving_lease_expires_at = None;
    }

    fn invalidate_leader_serving_lease_for_configuration_change(&mut self, now: Instant) {
        if !self.is_leader_flag {
            self.reset_leader_serving_lease_round();
            return;
        }
        self.reset_leader_serving_lease_round();
        self.published_serving_lease_expires_at = Some(now);
        (self.leadership_callbacks.on_leader_serving_lease_refreshed)(now);
    }

    fn maybe_start_leader_serving_lease_round(&mut self, sent_at: Instant) -> Option<Vec<u8>> {
        if self.raw_node.raft.state != StateRole::Leader
            || !self.is_leader_flag
            || !self.is_leader_ready_flag
        {
            return None;
        }

        if let Some(round) = self.pending_serving_lease_round.as_ref() {
            if round.term == self.raw_node.raft.term {
                // Keep even an expired round correlated until raft-rs returns
                // its ReadState. Regular heartbeats continue carrying the
                // latest pending context, so recovery can close this round
                // without accumulating unbounded ReadOnly requests.
                return None;
            }
        }
        self.pending_serving_lease_round = None;

        let renewal_margin = RaftNodeConfig::TICK_INTERVAL * self.config.heartbeat_tick as u32;
        let renew_by = sent_at.checked_add(renewal_margin)?;
        if self
            .published_serving_lease_expires_at
            .is_some_and(|published| published > renew_by)
        {
            return None;
        }

        let nonce = self.next_serving_lease_round_nonce.checked_add(1)?;
        let valid_until = sent_at.checked_add(self.config.leader_serving_lease_duration())?;
        self.next_serving_lease_round_nonce = nonce;
        let context =
            serving_lease_read_index_context(self.config.node_id, self.raw_node.raft.term, nonce);
        self.pending_serving_lease_round = Some(ServingLeaseRound {
            term: self.raw_node.raft.term,
            context: context.clone(),
            valid_until,
        });
        self.raw_node.read_index(context.clone());
        Some(context)
    }

    fn process_leader_serving_lease_read_states(
        &mut self,
        read_states: Vec<ReadState>,
        handled_at: Instant,
    ) {
        for read_state in read_states {
            let Some(round) = self.pending_serving_lease_round.as_ref() else {
                continue;
            };
            if read_state.request_ctx != round.context {
                continue;
            }

            let round = self
                .pending_serving_lease_round
                .take()
                .expect("matching serving-lease round disappeared");
            if self.raw_node.raft.state != StateRole::Leader
                || !self.is_leader_flag
                || !self.is_leader_ready_flag
                || self.raw_node.raft.term != round.term
                || handled_at >= round.valid_until
            {
                continue;
            }
            if self
                .published_serving_lease_expires_at
                .is_none_or(|published| round.valid_until > published)
            {
                self.published_serving_lease_expires_at = Some(round.valid_until);
                (self.leadership_callbacks.on_leader_serving_lease_refreshed)(round.valid_until);
            }
        }
    }

    fn revoke_application_leadership(&mut self) {
        if !self.is_leader_flag {
            return;
        }
        let committed_through = self.raw_node.raft.raft_log.committed;
        let applied_through = self.durable_applied_index;
        self.is_leader_flag = false;
        self.is_leader_ready_flag = false;
        self.leader_readiness_barrier = None;
        self.reset_leader_serving_lease_round();

        let pending_proposals = std::mem::take(&mut self.pending_proposals);
        for (index, tx) in pending_proposals {
            if index <= applied_through {
                let _ = tx.send(RaftProposalResult::Committed { index });
            } else if index <= committed_through {
                // Keep committed-but-not-yet-applied entries for ordered
                // classification from this or a later Ready batch.
                self.pending_proposals.insert(index, tx);
            } else {
                let _ = tx.send(RaftProposalResult::Rejected);
            }
        }
        (self.leadership_callbacks.on_lost_leadership)();
    }

    fn reconcile_application_leadership(&mut self, conf_state: &ConfState) {
        if !conf_state.get_voters().contains(&self.config.node_id) {
            self.revoke_application_leadership();
        }
    }

    fn apply_committed_configuration(&mut self, entry: &Entry) -> anyhow::Result<()> {
        let index = entry.index;
        anyhow::ensure!(
            index == self.durable_applied_index + 1,
            "Raft configuration entry at index {index} is not next after durable applied index {}",
            self.durable_applied_index,
        );
        let conf_state = match entry.get_entry_type() {
            EntryType::EntryConfChange => {
                anyhow::ensure!(
                    !entry.data.is_empty(),
                    "v1 Raft configuration entry at index {index} has an empty payload"
                );
                let change = ConfChange::parse_from_bytes(entry.get_data()).map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to decode v1 Raft configuration entry at index {index}: {error}"
                    )
                })?;
                self.raw_node.apply_conf_change(&change).map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to apply v1 Raft configuration entry at index {index}: {error}"
                    )
                })?
            },
            EntryType::EntryConfChangeV2 => {
                let change = ConfChangeV2::parse_from_bytes(entry.get_data()).map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to decode v2 Raft configuration entry at index {index}: {error}"
                    )
                })?;
                self.raw_node.apply_conf_change(&change).map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to apply v2 Raft configuration entry at index {index}: {error}"
                    )
                })?
            },
            entry_type => anyhow::bail!(
                "Expected a Raft configuration entry at index {index}, got {entry_type:?}"
            ),
        };

        self.storage
            .set_conf_state_and_applied_index(conf_state.clone(), index)?;
        self.durable_applied_index = index;
        self.applied_indexes_pending_persistence
            .retain(|pending_index| *pending_index > index);
        let configuration_applied_at = Instant::now();
        self.invalidate_leader_serving_lease_for_configuration_change(configuration_applied_at);
        self.reconcile_application_leadership(&conf_state);
        Ok(())
    }

    fn classify_committed_entry(&mut self, entry: Entry) -> anyhow::Result<CommittedEntryAction> {
        let index = entry.index;

        match entry.get_entry_type() {
            EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                self.apply_committed_configuration(&entry)?;
                Ok(CommittedEntryAction::ConfigurationApplied)
            },
            EntryType::EntryNormal => {
                if entry.data.is_empty() {
                    return Ok(CommittedEntryAction::MarkApplied(index));
                }
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

    fn schedule_next_committed_entry(
        &mut self,
        deferred_committed_entries: &mut VecDeque<Entry>,
        pending_state_machine_applies: &mut BTreeSet<u64>,
        pending_local_applies: &mut BTreeSet<u64>,
        apply_tx: &mpsc::UnboundedSender<StateMachineApplyJob>,
    ) -> anyhow::Result<bool> {
        if !pending_state_machine_applies.is_empty() || !pending_local_applies.is_empty() {
            return Ok(false);
        }

        let mut made_progress = false;
        while let Some(entry) = deferred_committed_entries.pop_front() {
            made_progress = true;
            let index = entry.index;
            match self.classify_committed_entry(entry)? {
                CommittedEntryAction::WaitForLocalApply => {
                    pending_local_applies.insert(index);
                    break;
                },
                CommittedEntryAction::MarkApplied(index) => {
                    self.record_applied_index(index)?;
                },
                CommittedEntryAction::ConfigurationApplied => {},
                CommittedEntryAction::Apply(job) => {
                    pending_state_machine_applies.insert(job.index);
                    if apply_tx.send(job).is_err() {
                        anyhow::bail!("Raft state-machine apply worker stopped");
                    }
                    break;
                },
            }
        }

        if made_progress {
            self.raw_node.advance_apply_to(self.durable_applied_index);
        }
        Ok(made_progress)
    }

    fn step_raft_message(&mut self, msg: Message) {
        let snapshot_proposal_identity = (msg.get_msg_type() == MessageType::MsgSnapshot
            && msg.has_snapshot())
        .then(|| self.snapshot_proposal_identity_before_step(msg.get_snapshot()));
        match self.raw_node.step(msg) {
            Ok(()) => {
                if let Some(identity) = snapshot_proposal_identity {
                    // `step` can legitimately ignore a stale snapshot. Retain
                    // its proof only when raft-rs accepted that exact snapshot
                    // as the unstable snapshot that will appear in Ready.
                    if self
                        .raw_node
                        .snap()
                        .is_some_and(|snapshot| identity.matches(snapshot))
                    {
                        self.pending_snapshot_proposal_identity = Some(identity);
                    }
                }
            },
            Err(e) => {
                tracing::warn!("Raft step error: {e}");
            },
        }
    }

    fn process_mailbox_message(
        &mut self,
        msg: RaftMessage,
        pending_local_applies: &mut BTreeSet<u64>,
    ) -> anyhow::Result<bool> {
        match msg {
            RaftMessage::Raft(msg) => {
                self.step_raft_message(msg);
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
        let now = Instant::now();
        *last_tick = now;
        let _ = self.maybe_start_leader_serving_lease_round(now);
    }

    /// Run the Raft loop. This is the main event loop following TiKV's pattern:
    /// tick → receive messages → propose → process ready → advance.
    pub async fn run(
        &mut self,
        mut on_committed: impl FnMut(u64, &[u8]) -> anyhow::Result<()> + Send + 'static,
        mut on_snapshot: impl FnMut(&Snapshot) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let tick_interval = RaftNodeConfig::TICK_INTERVAL;
        let mut last_tick = Instant::now();
        let mut pending_state_machine_applies = BTreeSet::new();
        let mut pending_local_applies = BTreeSet::new();
        let mut deferred_committed_entries = VecDeque::new();
        let (apply_tx, mut apply_rx) = mpsc::unbounded_channel::<StateMachineApplyJob>();
        let (applied_tx, mut applied_rx) = mpsc::unbounded_channel::<StateMachineApplyResult>();
        let _apply_worker = tokio::task::spawn_blocking(move || {
            while let Some(job) = apply_rx.blocking_recv() {
                let result = on_committed(job.index, &job.data);
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
                        if self.process_mailbox_message(msg, &mut pending_local_applies)? {
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
                // Snapshot installation replaces the database state machine, so it must remain
                // serialized with application work. Ordinary Ready processing must continue
                // while apply is pending so leaders can send heartbeats and
                // retain a healthy quorum.
                let apply_pending = !pending_state_machine_applies.is_empty()
                    || !pending_local_applies.is_empty()
                    || !deferred_committed_entries.is_empty();
                if apply_pending
                    && self
                        .raw_node
                        .snap()
                        .is_some_and(|snapshot| !snapshot.is_empty())
                {
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
                let snapshot_proposal_identity = if ready.snapshot().is_empty() {
                    None
                } else {
                    // Capture/fetch local proposal identity before SoftState
                    // revocation and before storage accepts the snapshot.
                    Some(self.snapshot_proposal_identity_for_install(ready.snapshot()))
                };
                let serving_lease_read_states = ready.take_read_states();

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
                        self.reset_leader_serving_lease_round();
                        (self.leadership_callbacks.on_became_leader)(self.raw_node.raft.term);
                    } else if !now_leader && self.is_leader_flag {
                        tracing::info!(
                            "Raft node {}: lost leadership for partition {}",
                            self.config.node_id,
                            self.config.partition_id,
                        );
                        self.revoke_application_leadership();
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
                    if let Err(e) = self.storage.stage_snapshot(ready.snapshot(), ready.hs()) {
                        tracing::error!("Failed to stage snapshot install intent: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else if let Err(e) = on_snapshot(ready.snapshot()) {
                        tracing::error!("Failed to apply snapshot to state machine: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else if let Err(e) = self.storage.commit_pending_snapshot_install() {
                        tracing::error!("Failed to commit staged snapshot metadata: {e}");
                        drop(snapshot_timer);
                        return Err(e);
                    } else {
                        if let Err(e) = self.finish_snapshot_application(
                            ready.snapshot(),
                            snapshot_proposal_identity
                                .as_ref()
                                .expect("non-empty snapshot must have proposal identity"),
                            Instant::now(),
                        ) {
                            tracing::error!("Failed to persist snapshot applied index: {e}");
                            drop(snapshot_timer);
                            return Err(e);
                        }
                        snapshot_timer.finish();
                    }
                }

                self.process_leader_serving_lease_read_states(
                    serving_lease_read_states,
                    Instant::now(),
                );

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

                // 6. Preserve committed-entry order independently from append/transport
                // progress. The state machine scheduler below releases only one blocking entry
                // at a time, but Raft can continue processing Ready and sending
                // heartbeats.
                deferred_committed_entries.extend(ready.take_committed_entries());

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

                deferred_committed_entries.extend(light_rd.take_committed_entries());
                self.maybe_mark_leader_ready();
            }

            let scheduled_committed_entry = self.schedule_next_committed_entry(
                &mut deferred_committed_entries,
                &mut pending_state_machine_applies,
                &mut pending_local_applies,
                &apply_tx,
            )?;
            if scheduled_committed_entry {
                self.maybe_mark_leader_ready();
            }

            if handled_message
                || handled_apply_result
                || should_tick
                || processed_ready
                || scheduled_committed_entry
            {
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
                    if self.process_mailbox_message(msg, &mut pending_local_applies)? {
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
    fn apply_committed_entries_test(
        &mut self,
        entries: Vec<Entry>,
        on_committed: &mut impl FnMut(&[u8]) -> anyhow::Result<()>,
        committed: &mut Vec<Vec<u8>>,
    ) -> anyhow::Result<()> {
        for entry in entries {
            let index = entry.index;
            match self.classify_committed_entry(entry)? {
                // A local proposal was installed in the Convex state machine
                // before it entered Raft. The production path waits for the
                // committer's MarkApplied message; this synchronous test
                // driver records that ordered completion directly.
                CommittedEntryAction::WaitForLocalApply => {
                    self.record_applied_index(index)?;
                },
                CommittedEntryAction::MarkApplied(index) => {
                    self.record_applied_index(index)?;
                },
                CommittedEntryAction::ConfigurationApplied => {},
                CommittedEntryAction::Apply(job) => {
                    on_committed(&job.data)?;
                    self.record_applied_index(job.index)?;
                    committed.push(job.data);
                },
            }
        }
        Ok(())
    }

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
        let snapshot_proposal_identity = if ready.snapshot().is_empty() {
            None
        } else {
            Some(self.snapshot_proposal_identity_for_install(ready.snapshot()))
        };
        let serving_lease_read_states = ready.take_read_states();

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
                self.reset_leader_serving_lease_round();
                (self.leadership_callbacks.on_became_leader)(self.raw_node.raft.term);
            } else if !now_leader && self.is_leader_flag {
                self.revoke_application_leadership();
            }
        }

        if !ready.snapshot().is_empty() {
            self.storage.apply_snapshot(ready.snapshot())?;
            self.finish_snapshot_application(
                ready.snapshot(),
                snapshot_proposal_identity
                    .as_ref()
                    .expect("non-empty snapshot must have proposal identity"),
                Instant::now(),
            )?;
        }
        self.process_leader_serving_lease_read_states(serving_lease_read_states, Instant::now());
        if let Some(hs) = ready.hs() {
            self.storage
                .append_entries_and_hardstate(ready.entries(), hs)?;
        } else {
            self.storage.append_entries(ready.entries())?;
        }
        self.apply_committed_entries_test(
            ready.take_committed_entries(),
            &mut on_committed,
            &mut committed,
        )?;
        let mut light_rd = self.raw_node.advance_append(ready);
        if let Some(commit) = light_rd.commit_index() {
            let mut hs = self.storage.hard_state().unwrap_or_default();
            hs.set_commit(commit);
            self.storage.set_hardstate(&hs)?;
        }
        self.apply_committed_entries_test(
            light_rd.take_committed_entries(),
            &mut on_committed,
            &mut committed,
        )?;
        self.raw_node.advance_apply_to(self.durable_applied_index);
        self.maybe_mark_leader_ready();
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

    fn test_config_with_peers(peers: Vec<u64>) -> RaftNodeConfig {
        RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers,
            election_tick: 10,
            heartbeat_tick: 3,
        }
    }

    fn test_config() -> RaftNodeConfig {
        test_config_with_peers(vec![1])
    }

    fn configuration_entry(index: u64, entry_type: EntryType, data: Vec<u8>) -> Entry {
        let mut entry = Entry::default();
        entry.set_index(index);
        entry.set_term(1);
        entry.set_entry_type(entry_type);
        entry.set_data(data.into());
        entry
    }

    fn persist_committed_entries_with_peers(
        engine: Arc<raft_engine::Engine>,
        peers: Vec<u64>,
        entries: Vec<Entry>,
    ) {
        let storage = ConvexRaftStorage::new(PartitionId(0), engine, 1, peers, None).unwrap();
        let last_entry = entries
            .last()
            .expect("test must persist at least one entry");
        let mut hard_state = HardState::default();
        hard_state.set_term(last_entry.term);
        hard_state.set_commit(last_entry.index);
        storage
            .append_entries_and_hardstate(&entries, &hard_state)
            .unwrap();
    }

    fn persist_committed_entries(engine: Arc<raft_engine::Engine>, entries: Vec<Entry>) {
        persist_committed_entries_with_peers(engine, vec![1], entries);
    }

    fn persist_committed_entry(engine: Arc<raft_engine::Engine>, entry: Entry) {
        persist_committed_entries(engine, vec![entry]);
    }

    fn add_voter_v1(node_id: u64) -> ConfChange {
        let mut change = ConfChange::default();
        change.set_node_id(node_id);
        change.set_change_type(ConfChangeType::AddNode);
        change
    }

    fn v2_change(node_id: u64, change_type: ConfChangeType) -> ConfChangeSingle {
        let mut single = ConfChangeSingle::default();
        single.set_node_id(node_id);
        single.set_change_type(change_type);
        single
    }

    fn enter_joint_v2() -> ConfChangeV2 {
        let mut change = ConfChangeV2::default();
        change.set_transition(ConfChangeTransition::Explicit);
        change
            .mut_changes()
            .push(v2_change(3, ConfChangeType::RemoveNode));
        change
            .mut_changes()
            .push(v2_change(4, ConfChangeType::AddNode));
        change
    }

    fn sorted_members(members: &[u64]) -> Vec<u64> {
        let mut members = members.to_vec();
        members.sort_unstable();
        members
    }

    fn elect_single_node_leader(node: &mut RaftNode) {
        for _ in 0..20 {
            node.raw_node.tick();
        }
        while node.raw_node.has_ready() {
            node.process_ready_test();
        }
        assert!(node.is_leader_ready());
    }

    fn apply_v1_voter_change(node: &mut RaftNode, node_id: u64) {
        let index = node.durable_applied_index + 1;
        assert!(matches!(
            node.classify_committed_entry(configuration_entry(
                index,
                EntryType::EntryConfChange,
                add_voter_v1(node_id).write_to_bytes().unwrap(),
            ))
            .unwrap(),
            CommittedEntryAction::ConfigurationApplied
        ));
    }

    fn heartbeat_response(node: &RaftNode, from: u64, context: Vec<u8>) -> Message {
        let mut response = Message::default();
        response.set_msg_type(MessageType::MsgHeartbeatResponse);
        response.from = from;
        response.to = node.config.node_id;
        response.term = node.raw_node.raft.term;
        response.set_context(context.into());
        response
    }

    fn self_demoting_snapshot(index: u64, term: u64) -> Snapshot {
        let mut metadata = SnapshotMetadata::default();
        metadata.index = index;
        metadata.term = term;
        metadata.set_conf_state(ConfState::from((vec![2, 3, 4], vec![1])));
        let mut snapshot = Snapshot::default();
        snapshot.set_metadata(metadata);
        snapshot.set_data(
            crate::raft_snapshot::test_snapshot_bytes(index, term)
                .unwrap()
                .into(),
        );
        snapshot
    }

    fn accept_incoming_snapshot(
        node: &mut RaftNode,
        snapshot: &Snapshot,
    ) -> SnapshotProposalIdentity {
        let mut message = Message::default();
        message.set_msg_type(MessageType::MsgSnapshot);
        message.from = 2;
        message.to = node.config.node_id;
        message.term = snapshot.get_metadata().term + 1;
        message.set_snapshot(snapshot.clone());
        node.step_raft_message(message);

        assert!(
            node.raw_node
                .snap()
                .is_some_and(|pending| pending == snapshot),
            "raft-rs must accept the test snapshot"
        );
        node.pending_snapshot_proposal_identity
            .clone()
            .expect("accepted snapshot must retain pre-step proposal identity")
    }

    fn take_raft_read_states(node: &mut RaftNode) -> Vec<ReadState> {
        std::mem::take(&mut node.raw_node.raft.read_states)
    }

    #[test]
    fn test_applies_v1_configuration_payload_and_restores_it_after_restart() {
        let engine = test_engine();
        let change = add_voter_v1(2);
        let mut normal = Entry::default();
        normal.set_index(1);
        normal.set_term(1);
        normal.set_data(b"normal before membership".to_vec().into());
        persist_committed_entries(
            engine.clone(),
            vec![
                normal,
                configuration_entry(
                    2,
                    EntryType::EntryConfChange,
                    change.write_to_bytes().unwrap(),
                ),
            ],
        );

        {
            let (_tx, rx) = mpsc::unbounded_channel();
            let mut node =
                RaftNode::new(test_config(), engine.clone(), rx, HashMap::new(), None).unwrap();
            assert_eq!(
                node.try_process_ready_test().unwrap(),
                vec![b"normal before membership".to_vec()]
            );
            let state = node.storage.initial_state().unwrap();
            assert_eq!(state.conf_state.get_voters(), &[1, 2]);
            assert_eq!(node.storage.applied_index().unwrap(), 2);
            assert_eq!(
                node.raw_node
                    .store()
                    .initial_state()
                    .unwrap()
                    .conf_state
                    .get_voters(),
                &[1, 2],
                "RawNode-owned storage clone must observe committed membership"
            );
        }

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut restarted = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        let state = restarted.storage.initial_state().unwrap();
        assert_eq!(state.conf_state.get_voters(), &[1, 2]);
        assert_eq!(restarted.raw_node.raft.raft_log.applied, 2);
        assert!(restarted.try_process_ready_test().unwrap().is_empty());
    }

    #[test]
    fn test_applies_v2_joint_configuration_and_empty_leave_payload() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(
            test_config_with_peers(vec![1, 2, 3]),
            engine,
            rx,
            HashMap::new(),
            None,
        )
        .unwrap();

        let enter_joint = configuration_entry(
            1,
            EntryType::EntryConfChangeV2,
            enter_joint_v2().write_to_bytes().unwrap(),
        );
        assert!(matches!(
            node.classify_committed_entry(enter_joint).unwrap(),
            CommittedEntryAction::ConfigurationApplied
        ));
        let joint = node.storage.initial_state().unwrap().conf_state;
        assert_eq!(sorted_members(joint.get_voters()), vec![1, 2, 4]);
        assert_eq!(sorted_members(joint.get_voters_outgoing()), vec![1, 2, 3]);
        assert_eq!(node.storage.applied_index().unwrap(), 1);

        // An empty v2 payload is the raft-rs representation for leaving joint
        // consensus, so it must be decoded and applied rather than treated as
        // a normal empty entry or malformed configuration.
        let leave_joint = configuration_entry(2, EntryType::EntryConfChangeV2, Vec::new());
        assert!(matches!(
            node.classify_committed_entry(leave_joint).unwrap(),
            CommittedEntryAction::ConfigurationApplied
        ));
        let final_state = node.storage.initial_state().unwrap().conf_state;
        assert_eq!(sorted_members(final_state.get_voters()), vec![1, 2, 4]);
        assert!(final_state.get_voters_outgoing().is_empty());
        assert_eq!(node.storage.applied_index().unwrap(), 2);
    }

    #[test]
    fn test_committed_joint_demotion_immediately_revokes_application_leadership() {
        use std::sync::atomic::{
            AtomicUsize,
            Ordering,
        };

        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(
            test_config_with_peers(vec![1, 2, 3]),
            engine,
            rx,
            HashMap::new(),
            None,
        )
        .unwrap();
        node.is_leader_flag = true;
        node.is_leader_ready_flag = true;
        node.leader_readiness_barrier = Some((1, 1));
        let lost_count = Arc::new(AtomicUsize::new(0));
        node.set_leadership_callbacks(LeadershipCallbacks {
            on_lost_leadership: Box::new({
                let lost_count = lost_count.clone();
                move || {
                    lost_count.fetch_add(1, Ordering::SeqCst);
                }
            }),
            ..LeadershipCallbacks::default()
        });

        let mut demote = ConfChangeV2::default();
        demote.set_transition(ConfChangeTransition::Explicit);
        demote
            .mut_changes()
            .push(v2_change(1, ConfChangeType::AddLearnerNode));
        node.classify_committed_entry(configuration_entry(
            1,
            EntryType::EntryConfChangeV2,
            demote.write_to_bytes().unwrap(),
        ))
        .unwrap();

        let joint = node.storage.conf_state();
        assert!(!joint.get_voters().contains(&1));
        assert!(joint.get_voters_outgoing().contains(&1));
        assert!(!node.is_leader());
        assert!(!node.is_leader_ready());
        assert!(node.leader_readiness_barrier.is_none());
        assert_eq!(lost_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_self_removal_preserves_later_committed_local_waiter_in_same_ready() {
        use std::sync::atomic::{
            AtomicUsize,
            Ordering,
        };

        let engine = test_engine();
        let mut remove_self = ConfChange::default();
        remove_self.set_node_id(1);
        remove_self.set_change_type(ConfChangeType::RemoveNode);
        let mut committed_local = Entry::default();
        committed_local.set_index(2);
        committed_local.set_term(1);
        committed_local.set_data(b"already committed locally".to_vec().into());
        persist_committed_entries_with_peers(
            engine.clone(),
            vec![1, 2],
            vec![
                configuration_entry(
                    1,
                    EntryType::EntryConfChange,
                    remove_self.write_to_bytes().unwrap(),
                ),
                committed_local,
            ],
        );

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(
            test_config_with_peers(vec![1, 2]),
            engine,
            rx,
            HashMap::new(),
            None,
        )
        .unwrap();
        node.is_leader_flag = true;
        node.is_leader_ready_flag = true;
        node.reset_leader_serving_lease_round();
        let lost_count = Arc::new(AtomicUsize::new(0));
        node.leadership_callbacks.on_lost_leadership = Box::new({
            let lost_count = lost_count.clone();
            move || {
                lost_count.fetch_add(1, Ordering::SeqCst);
            }
        });

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        node.pending_proposals.insert(2, result_tx);
        let applied_remotely = Arc::new(AtomicUsize::new(0));
        let committed = node
            .try_process_ready_test_with_apply({
                let applied_remotely = applied_remotely.clone();
                move |_| {
                    applied_remotely.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap();

        assert!(committed.is_empty());
        assert_eq!(
            result_rx.await.unwrap(),
            RaftProposalResult::Committed { index: 2 }
        );
        assert_eq!(lost_count.load(Ordering::SeqCst), 1);
        assert_eq!(applied_remotely.load(Ordering::SeqCst), 0);
        assert_eq!(node.storage.applied_index().unwrap(), 2);
        assert_eq!(node.storage.conf_state().get_voters(), &[2]);
        assert!(node.pending_proposals.is_empty());
        assert!(node.try_process_ready_test().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_resolves_only_waiter_known_committed_before_snapshot() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        node.is_leader_flag = true;
        node.is_leader_ready_flag = true;

        let snapshot = self_demoting_snapshot(5, 2);
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        node.pending_proposals.insert(4, committed_tx);
        node.raw_node.raft.raft_log.committed = 4;
        let proposal_identity = accept_incoming_snapshot(&mut node, &snapshot);
        assert_eq!(
            proposal_identity.locally_committed_proposal_indexes,
            BTreeSet::from([4])
        );

        // Ready can report follower SoftState before installing its snapshot.
        // Revocation must retain the locally committed waiter until durable
        // snapshot completion classifies it by the captured identity proof.
        node.revoke_application_leadership();
        assert!(node.pending_proposals.contains_key(&4));
        node.storage.apply_snapshot(&snapshot).unwrap();
        node.finish_snapshot_application(&snapshot, &proposal_identity, Instant::now())
            .unwrap();

        assert_eq!(
            committed_rx.await.unwrap(),
            RaftProposalResult::Committed { index: 4 }
        );
        assert!(node.pending_proposals.is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_rejects_same_index_waiter_not_known_committed_before_snapshot() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        node.is_leader_flag = true;
        node.is_leader_ready_flag = true;

        let snapshot = self_demoting_snapshot(5, 2);
        let (overwritten_tx, overwritten_rx) = tokio::sync::oneshot::channel();
        node.pending_proposals.insert(5, overwritten_tx);
        node.raw_node.raft.raft_log.committed = 4;
        let proposal_identity = accept_incoming_snapshot(&mut node, &snapshot);
        assert!(proposal_identity
            .locally_committed_proposal_indexes
            .is_empty());

        node.revoke_application_leadership();
        assert!(
            node.pending_proposals.contains_key(&5),
            "snapshot-influenced committed frontier must not decide proposal identity"
        );
        node.storage.apply_snapshot(&snapshot).unwrap();
        node.finish_snapshot_application(&snapshot, &proposal_identity, Instant::now())
            .unwrap();

        assert_eq!(
            overwritten_rx.await.unwrap(),
            RaftProposalResult::Rejected,
            "same-index snapshot entry may have overwritten the old leader proposal"
        );
        assert!(node.pending_proposals.is_empty());
    }

    #[tokio::test]
    async fn test_self_demoting_snapshot_rejects_uncommitted_suffix_waiter() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        node.is_leader_flag = true;
        node.is_leader_ready_flag = true;

        let snapshot = self_demoting_snapshot(5, 2);
        let (suffix_tx, mut suffix_rx) = tokio::sync::oneshot::channel();
        node.pending_proposals.insert(6, suffix_tx);
        node.raw_node.raft.raft_log.committed = 4;
        let proposal_identity = accept_incoming_snapshot(&mut node, &snapshot);

        node.revoke_application_leadership();
        assert_eq!(
            suffix_rx.try_recv().unwrap(),
            RaftProposalResult::Rejected,
            "uncovered proposal must be rejected by ordinary leadership revocation"
        );
        node.storage.apply_snapshot(&snapshot).unwrap();
        node.finish_snapshot_application(&snapshot, &proposal_identity, Instant::now())
            .unwrap();

        assert!(node.pending_proposals.is_empty());
    }

    #[test]
    fn test_delayed_same_term_read_index_round_cannot_authorize_serving() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        elect_single_node_leader(&mut node);
        apply_v1_voter_change(&mut node, 2);
        node.published_serving_lease_expires_at = None;

        let published = Arc::new(std::sync::Mutex::new(Vec::new()));
        node.leadership_callbacks.on_leader_serving_lease_refreshed = Box::new({
            let published = published.clone();
            move |expires_at| published.lock().unwrap().push(expires_at)
        });

        let sent_at = Instant::now();
        let context = node
            .maybe_start_leader_serving_lease_round(sent_at)
            .expect("leader should start a lease ReadIndex round");
        let valid_until = node
            .pending_serving_lease_round
            .as_ref()
            .unwrap()
            .valid_until;
        node.step_raft_message(heartbeat_response(&node, 2, context));
        let read_states = take_raft_read_states(&mut node);
        assert_eq!(
            read_states.len(),
            1,
            "same-term response must be protocol-valid"
        );

        node.process_leader_serving_lease_read_states(
            read_states,
            valid_until + Duration::from_millis(1),
        );
        assert!(published.lock().unwrap().is_empty());
        assert!(node.pending_serving_lease_round.is_none());
        assert_eq!(node.published_serving_lease_expires_at, None);
    }

    #[test]
    fn test_mismatched_and_duplicate_lease_round_contexts_are_ignored() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        elect_single_node_leader(&mut node);
        apply_v1_voter_change(&mut node, 2);
        node.published_serving_lease_expires_at = None;

        let published = Arc::new(std::sync::Mutex::new(Vec::new()));
        node.leadership_callbacks.on_leader_serving_lease_refreshed = Box::new({
            let published = published.clone();
            move |expires_at| published.lock().unwrap().push(expires_at)
        });

        let sent_at = Instant::now();
        let context = node
            .maybe_start_leader_serving_lease_round(sent_at)
            .expect("leader should start a lease ReadIndex round");
        let valid_until = node
            .pending_serving_lease_round
            .as_ref()
            .unwrap()
            .valid_until;
        let mismatched_context = serving_lease_read_index_context(
            node.config.node_id,
            node.raw_node.raft.term,
            node.next_serving_lease_round_nonce + 1,
        );
        node.step_raft_message(heartbeat_response(&node, 2, mismatched_context));
        assert!(take_raft_read_states(&mut node).is_empty());
        assert_eq!(
            node.pending_serving_lease_round
                .as_ref()
                .map(|round| round.context.as_slice()),
            Some(context.as_slice())
        );
        assert!(published.lock().unwrap().is_empty());

        node.step_raft_message(heartbeat_response(&node, 2, context));
        let read_state = take_raft_read_states(&mut node)
            .pop()
            .expect("matching quorum response should complete ReadIndex");
        node.process_leader_serving_lease_read_states(
            vec![read_state.clone(), read_state],
            sent_at,
        );
        assert_eq!(*published.lock().unwrap(), vec![valid_until]);
        assert!(node.pending_serving_lease_round.is_none());
    }

    #[test]
    fn test_joint_configuration_lease_round_requires_both_voter_majorities() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        elect_single_node_leader(&mut node);
        apply_v1_voter_change(&mut node, 2);
        apply_v1_voter_change(&mut node, 3);
        let enter_joint = enter_joint_v2();
        assert!(matches!(
            node.classify_committed_entry(configuration_entry(
                node.durable_applied_index + 1,
                EntryType::EntryConfChangeV2,
                enter_joint.write_to_bytes().unwrap(),
            ))
            .unwrap(),
            CommittedEntryAction::ConfigurationApplied
        ));
        let joint = node.storage.conf_state();
        assert_eq!(sorted_members(joint.get_voters()), vec![1, 2, 4]);
        assert_eq!(sorted_members(joint.get_voters_outgoing()), vec![1, 2, 3]);
        node.published_serving_lease_expires_at = None;

        let published = Arc::new(std::sync::Mutex::new(Vec::new()));
        node.leadership_callbacks.on_leader_serving_lease_refreshed = Box::new({
            let published = published.clone();
            move |expires_at| published.lock().unwrap().push(expires_at)
        });
        let sent_at = Instant::now();
        let context = node
            .maybe_start_leader_serving_lease_round(sent_at)
            .expect("joint-config leader should start a lease ReadIndex round");
        let valid_until = node
            .pending_serving_lease_round
            .as_ref()
            .unwrap()
            .valid_until;

        node.step_raft_message(heartbeat_response(&node, 4, context.clone()));
        assert!(
            take_raft_read_states(&mut node).is_empty(),
            "incoming voter majority alone must not complete a joint ReadIndex"
        );
        assert!(published.lock().unwrap().is_empty());

        node.step_raft_message(heartbeat_response(&node, 2, context));
        let read_states = take_raft_read_states(&mut node);
        assert_eq!(read_states.len(), 1);
        node.process_leader_serving_lease_read_states(read_states, sent_at);
        assert_eq!(*published.lock().unwrap(), vec![valid_until]);
    }

    #[test]
    fn test_rejects_malformed_configuration_payload_without_advancing() {
        let engine = test_engine();
        persist_committed_entry(
            engine.clone(),
            configuration_entry(1, EntryType::EntryConfChangeV2, vec![0xff]),
        );

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node =
            RaftNode::new(test_config(), engine.clone(), rx, HashMap::new(), None).unwrap();
        let err = node
            .try_process_ready_test()
            .expect_err("malformed configuration payload must fail closed");
        assert!(
            format!("{err:#}").contains("Failed to decode v2 Raft configuration entry"),
            "unexpected error: {err:#}"
        );
        assert_eq!(node.raw_node.raft.raft_log.applied, 0);
        assert_eq!(node.storage.applied_index().unwrap(), 0);
        assert_eq!(
            node.storage
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[1]
        );
    }

    #[test]
    fn test_rejects_empty_v1_configuration_payload_without_advancing() {
        let engine = test_engine();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();

        let err = match node.classify_committed_entry(configuration_entry(
            1,
            EntryType::EntryConfChange,
            Vec::new(),
        )) {
            Ok(_) => panic!("empty v1 configuration payload must fail closed"),
            Err(err) => err,
        };
        assert!(
            format!("{err:#}")
                .contains("v1 Raft configuration entry at index 1 has an empty payload"),
            "unexpected error: {err:#}"
        );
        assert_eq!(node.raw_node.raft.raft_log.applied, 0);
        assert_eq!(node.storage.applied_index().unwrap(), 0);
        assert_eq!(
            node.storage
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[1]
        );
    }

    #[test]
    fn test_configuration_persistence_failure_does_not_advance_applied_index() {
        let engine = test_engine();
        let change = add_voter_v1(2);
        let entry = configuration_entry(
            1,
            EntryType::EntryConfChange,
            change.write_to_bytes().unwrap(),
        );
        persist_committed_entry(engine.clone(), entry.clone());

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut node =
            RaftNode::new(test_config(), engine.clone(), rx, HashMap::new(), None).unwrap();
        node.storage.fail_next_write_for_test();

        let err = match node.classify_committed_entry(entry) {
            Ok(_) => panic!("membership persistence failure must fail closed"),
            Err(err) => err,
        };
        assert!(
            format!("{err:#}").contains("injected raft-engine write failure"),
            "unexpected error: {err:#}"
        );
        assert_eq!(node.durable_applied_index, 0);
        assert_eq!(node.raw_node.raft.raft_log.applied, 0);
        assert_eq!(node.storage.applied_index().unwrap(), 0);
        assert_eq!(
            node.storage
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[1]
        );
        assert_eq!(
            node.raw_node
                .store()
                .initial_state()
                .unwrap()
                .conf_state
                .get_voters(),
            &[1],
            "failed persistence must not publish membership to RawNode storage"
        );

        // apply_conf_change has already changed raft-rs's volatile progress
        // tracker, so the only safe response to the persistence error is to
        // stop this node. A fresh node must recover the still-unapplied
        // committed entry from durable log/hard-state and apply it once.
        drop(node);
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut restarted = RaftNode::new(test_config(), engine, rx, HashMap::new(), None).unwrap();
        assert!(restarted.try_process_ready_test().unwrap().is_empty());
        assert_eq!(restarted.storage.applied_index().unwrap(), 1);
        assert_eq!(restarted.storage.conf_state().get_voters(), &[1, 2]);
        assert!(restarted.try_process_ready_test().unwrap().is_empty());
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
                move |_| events.lock().unwrap().push("elected")
            }),
            on_leader_ready: Box::new({
                let events = events.clone();
                move || events.lock().unwrap().push("ready")
            }),
            on_leader_serving_lease_refreshed: Box::new({
                let events = events.clone();
                move |_| events.lock().unwrap().push("lease")
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
            vec!["elected", "applied", "ready"],
            "readiness must precede lease authorization",
        );

        let lease_round = node
            .pending_serving_lease_round
            .clone()
            .expect("ready leader should start a Safe ReadIndex lease round");
        let read_states = take_raft_read_states(&mut node);
        assert_eq!(read_states.len(), 1);
        assert_eq!(
            read_states[0].request_ctx, lease_round.context,
            "only the correlated Safe ReadIndex result may authorize serving"
        );
        node.process_leader_serving_lease_read_states(read_states, Instant::now());
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
            &mut BTreeSet::new(),
        )?;
        assert_eq!(result_rx.await?, RaftProposalResult::Rejected);
        Ok(())
    }

    #[tokio::test]
    async fn test_local_proposal_commits_once_and_records_ordered_apply() {
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

        let applied_before = node.storage.applied_index().unwrap();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        node.process_mailbox_message(
            RaftMessage::Propose(RaftProposal {
                data: b"test mutation".to_vec(),
                result_tx,
            }),
            &mut BTreeSet::new(),
        )
        .unwrap();

        // The test Ready driver must classify this as a local proposal, avoid
        // invoking the remote state-machine callback, and record its ordered
        // local apply completion.
        let mut committed_data = Vec::new();
        for _ in 0..10 {
            node.raw_node.tick();
            committed_data.extend(node.process_ready_test());
            if node.pending_proposals.is_empty() {
                break;
            }
        }

        let result = result_rx.await.unwrap();
        let RaftProposalResult::Committed { index } = result else {
            panic!("local proposal should commit, got {result:?}");
        };
        assert!(
            committed_data.is_empty(),
            "local proposal must not be replayed through the remote apply callback"
        );
        assert_eq!(node.storage.applied_index().unwrap(), index);
        assert!(index > applied_before);
        assert!(node.pending_proposals.is_empty());
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

        let run_task = tokio::spawn(async move { node.run(|_, _| Ok(()), |_| Ok(())).await });
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
                move |_, _| {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_leader_keeps_quorum_while_local_apply_is_pending() -> anyhow::Result<()> {
        use std::sync::atomic::{
            AtomicBool,
            Ordering,
        };

        let peer_ids = vec![1u64, 2, 3];
        let mut mailbox_txs = Vec::new();
        let mut mailbox_rxs = Vec::new();
        let mut transport_txs = Vec::new();
        let mut transport_rxs = Vec::new();
        for _ in &peer_ids {
            let (mailbox_tx, mailbox_rx) = mpsc::unbounded_channel();
            mailbox_txs.push(mailbox_tx);
            mailbox_rxs.push(Some(mailbox_rx));

            let (transport_tx, transport_rx) = mpsc::unbounded_channel::<Message>();
            transport_txs.push(transport_tx);
            transport_rxs.push(transport_rx);
        }

        let leader_ready = Arc::new(AtomicBool::new(false));
        let leader_lost = Arc::new(AtomicBool::new(false));
        let mut nodes = Vec::new();
        for (node_index, node_id) in peer_ids.iter().copied().enumerate() {
            let peer_senders = peer_ids
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, peer_id)| *peer_id != node_id)
                .map(|(peer_index, peer_id)| (peer_id, transport_txs[peer_index].clone()))
                .collect();
            let mut node = RaftNode::new(
                RaftNodeConfig {
                    node_id,
                    partition_id: PartitionId(0),
                    peers: peer_ids.clone(),
                    election_tick: 10,
                    heartbeat_tick: 3,
                },
                test_engine(),
                mailbox_rxs[node_index].take().unwrap(),
                peer_senders,
                None,
            )?;
            if node_id == 1 {
                let ready = leader_ready.clone();
                let lost = leader_lost.clone();
                node.set_leadership_callbacks(LeadershipCallbacks {
                    on_leader_ready: Box::new(move || ready.store(true, Ordering::SeqCst)),
                    on_lost_leadership: Box::new(move || lost.store(true, Ordering::SeqCst)),
                    ..LeadershipCallbacks::default()
                });
            }
            nodes.push(node);
        }

        nodes[0].raw_node.campaign()?;

        let mut forward_tasks = Vec::new();
        for (mut transport_rx, mailbox_tx) in
            transport_rxs.into_iter().zip(mailbox_txs.iter().cloned())
        {
            forward_tasks.push(tokio::spawn(async move {
                while let Some(message) = transport_rx.recv().await {
                    if mailbox_tx.send(RaftMessage::Raft(message)).is_err() {
                        break;
                    }
                }
            }));
        }

        let mut run_tasks = Vec::new();
        for mut node in nodes {
            run_tasks.push(tokio::spawn(async move {
                node.run(|_, _| Ok(()), |_| Ok(())).await
            }));
        }

        tokio::time::timeout(Duration::from_secs(5), async {
            while !leader_ready.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("node 1 did not become ready leader"))?;

        let (first_result_tx, first_result_rx) = tokio::sync::oneshot::channel();
        mailbox_txs[0].send(RaftMessage::Propose(RaftProposal {
            data: b"slow local apply".to_vec(),
            result_tx: first_result_tx,
        }))?;
        let first_index = match tokio::time::timeout(Duration::from_secs(5), first_result_rx)
            .await
            .map_err(|_| anyhow::anyhow!("first proposal did not commit"))??
        {
            RaftProposalResult::Committed { index } => index,
            RaftProposalResult::Rejected => anyhow::bail!("ready leader rejected first proposal"),
        };

        // Hold the local database apply beyond the randomized election timeout.
        // Consensus transport must continue independently or a healthy follower
        // will start an election.
        tokio::time::sleep(Duration::from_secs(3)).await;

        let (mark_tx, mark_rx) = tokio::sync::oneshot::channel();
        mailbox_txs[0].send(RaftMessage::MarkApplied {
            index: first_index,
            result_tx: mark_tx,
        })?;
        tokio::time::timeout(Duration::from_secs(2), mark_rx)
            .await
            .map_err(|_| anyhow::anyhow!("first local apply was not acknowledged"))???;

        let (second_result_tx, second_result_rx) = tokio::sync::oneshot::channel();
        mailbox_txs[0].send(RaftMessage::Propose(RaftProposal {
            data: b"leader retained quorum".to_vec(),
            result_tx: second_result_tx,
        }))?;
        let second_index = match tokio::time::timeout(Duration::from_secs(5), second_result_rx)
            .await
            .map_err(|_| anyhow::anyhow!("second proposal did not resolve"))??
        {
            RaftProposalResult::Committed { index } => index,
            RaftProposalResult::Rejected => {
                anyhow::bail!("leader lost quorum while local apply was pending")
            },
        };
        anyhow::ensure!(
            !leader_lost.load(Ordering::SeqCst),
            "node 1 reported leadership loss while all three nodes were healthy"
        );

        let (mark_tx, mark_rx) = tokio::sync::oneshot::channel();
        mailbox_txs[0].send(RaftMessage::MarkApplied {
            index: second_index,
            result_tx: mark_tx,
        })?;
        tokio::time::timeout(Duration::from_secs(2), mark_rx)
            .await
            .map_err(|_| anyhow::anyhow!("second local apply was not acknowledged"))???;

        for mailbox_tx in &mailbox_txs {
            let _ = mailbox_tx.send(RaftMessage::Shutdown);
        }
        for run_task in run_tasks {
            run_task.await??;
        }
        for forward_task in forward_tasks {
            forward_task.abort();
        }
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
    async fn test_restart_after_snapshot_applies_each_later_entry_exactly_once() {
        let config = RaftNodeConfig {
            node_id: 1,
            partition_id: PartitionId(0),
            peers: vec![1],
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let engine = test_engine();

        {
            let mut storage =
                ConvexRaftStorage::new(PartitionId(0), engine.clone(), 1, vec![1], None).unwrap();
            let mut metadata = SnapshotMetadata::default();
            metadata.index = 8;
            metadata.term = 2;
            metadata.set_conf_state(ConfState::from((vec![1], vec![])));
            let mut snapshot = Snapshot::default();
            snapshot.set_metadata(metadata);
            snapshot.set_data(
                crate::raft_snapshot::test_snapshot_bytes(8, 2)
                    .unwrap()
                    .into(),
            );
            storage.apply_snapshot(&snapshot).unwrap();

            let mut entry = Entry::default();
            entry.set_index(9);
            entry.set_term(3);
            entry.set_data(b"after snapshot".to_vec().into());
            let mut hard_state = HardState::default();
            hard_state.set_term(3);
            hard_state.set_vote(1);
            hard_state.set_commit(9);
            storage
                .append_entries_and_hardstate(&[entry], &hard_state)
                .unwrap();
        }

        {
            let (_tx, rx) = mpsc::unbounded_channel();
            let mut node =
                RaftNode::new(config.clone(), engine.clone(), rx, HashMap::new(), None).unwrap();
            assert_eq!(node.raw_node.raft.raft_log.applied, 8);
            let committed = node.process_ready_test();
            assert_eq!(
                committed
                    .iter()
                    .filter(|data| data.as_slice() == b"after snapshot")
                    .count(),
                1,
            );
            assert_eq!(node.storage.applied_index().unwrap(), 9);
        }

        let (_tx, rx) = mpsc::unbounded_channel();
        let mut restarted = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();
        assert_eq!(restarted.raw_node.raft.raft_log.applied, 9);
        assert!(
            restarted.process_ready_test().is_empty(),
            "a second restart must not replay an entry already durably applied",
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
        let mut node = RaftNode::new(config, engine, rx, HashMap::new(), None).unwrap();

        let observed_leader_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed_leader_id_clone = observed_leader_id.clone();
        let became_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let became_leader_clone = became_leader.clone();

        node.set_leadership_callbacks(LeadershipCallbacks {
            on_became_leader: Box::new(move |_| {
                became_leader_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            on_leader_changed: Box::new(move |leader_id| {
                observed_leader_id_clone.store(leader_id, std::sync::atomic::Ordering::SeqCst);
            }),
            on_lost_leadership: Box::new(|| {}),
            on_leader_ready: Box::new(|| {}),
            on_leader_serving_lease_refreshed: Box::new(|_| {}),
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
