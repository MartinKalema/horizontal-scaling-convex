//! Raft failover integration tests.
//!
//! Tests leader election, propose-commit, failover, and partition tolerance
//! using a 3-node in-process Raft group. Messages pass through direct step()
//! calls — no network transport.
//!
//! Test patterns from:
//!   - CockroachDB roachtest failover suite
//!   - TiKV fail-rs chaos testing
//!   - YugabyteDB Jepsen nightly resilience benchmarks

use std::{
    collections::{
        BTreeSet,
        HashMap,
    },
    sync::Arc,
};

use raft::{
    GetEntriesContext,
    SnapshotStatus,
    StateRole,
    Storage,
};
use rand::{
    Rng,
    SeedableRng,
};
use rand_chacha::ChaCha12Rng;
use tokio::sync::mpsc;

use crate::{
    partition::PartitionId,
    raft_node::{
        RaftNode,
        RaftNodeConfig,
    },
    raft_storage::ConvexRaftStorage,
};

fn test_engine() -> Arc<raft_engine::Engine> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path();
    ConvexRaftStorage::open_engine(path.to_str().unwrap()).unwrap()
}

/// Create a 3-node Raft group with persistent storage.
fn create_three_node_group() -> Vec<RaftNode> {
    let peer_ids = vec![1u64, 2, 3];
    let mut nodes = Vec::new();
    let snapshot_provider: Arc<dyn crate::raft_storage::RaftSnapshotProvider> =
        Arc::new(|| Ok(Vec::new()));

    for &id in &peer_ids {
        let engine = test_engine();
        let (_, rx) = mpsc::unbounded_channel();
        let config = RaftNodeConfig {
            node_id: id,
            partition_id: PartitionId(0),
            peers: peer_ids.clone(),
            election_tick: 10,
            heartbeat_tick: 3,
        };
        let node = RaftNode::new(
            config,
            engine,
            rx,
            HashMap::new(),
            Some(snapshot_provider.clone()),
        )
        .unwrap();
        nodes.push(node);
    }

    nodes
}

/// Run one Raft tick cycle: tick all nodes, process ready, route messages.
/// `active` controls which nodes participate (for simulating kills/partitions).
fn tick_cycle(nodes: &mut [RaftNode], active: &[bool]) -> Vec<Vec<u8>> {
    tick_cycle_with_message_filter(nodes, active, |_, _, _| true)
}

fn tick_cycle_with_message_filter(
    nodes: &mut [RaftNode],
    active: &[bool],
    mut should_deliver: impl FnMut(u64, u64, &raft::prelude::Message) -> bool,
) -> Vec<Vec<u8>> {
    let mut committed = Vec::new();

    for (i, node) in nodes.iter_mut().enumerate() {
        if active[i] {
            node.raw_node.tick();
        }
    }

    // Collect all messages from ready states.
    let mut all_messages: Vec<(u64, u64, raft::prelude::Message)> = Vec::new();

    for (i, node) in nodes.iter_mut().enumerate() {
        if !active[i] {
            continue;
        }
        if node.raw_node.has_ready() {
            let mut ready = node.raw_node.ready();

            for msg in ready.take_messages() {
                all_messages.push((node.node_id(), msg.to, msg));
            }

            if !ready.snapshot().is_empty() {
                node.storage.apply_snapshot(ready.snapshot()).unwrap();
                node.storage
                    .set_applied_index(ready.snapshot().get_metadata().index)
                    .unwrap();
            }
            node.storage.append_entries(ready.entries()).unwrap();
            if let Some(hs) = ready.hs() {
                node.storage.set_hardstate(hs).unwrap();
            }

            for entry in ready.take_committed_entries() {
                if !entry.data.is_empty()
                    && entry.get_entry_type() == raft::prelude::EntryType::EntryNormal
                {
                    committed.push(entry.data.to_vec());
                }
                node.storage.set_applied_index(entry.index).unwrap();
            }

            for msg in ready.take_persisted_messages() {
                all_messages.push((node.node_id(), msg.to, msg));
            }

            let mut light_rd = node.raw_node.advance(ready);
            for entry in light_rd.take_committed_entries() {
                if !entry.data.is_empty()
                    && entry.get_entry_type() == raft::prelude::EntryType::EntryNormal
                {
                    committed.push(entry.data.to_vec());
                }
                node.storage.set_applied_index(entry.index).unwrap();
            }
            node.raw_node.advance_apply();
        }
    }

    // Route messages to target nodes (only active ones).
    for (from, to, msg) in all_messages {
        let is_snapshot = msg.get_msg_type() == raft::prelude::MessageType::MsgSnapshot;
        let mut delivered = false;
        for (i, node) in nodes.iter_mut().enumerate() {
            if active[i] && node.node_id() == to && should_deliver(from, to, &msg) {
                let _ = node.raw_node.step(msg.clone());
                delivered = true;
                break;
            }
        }
        if is_snapshot && let Some(source) = nodes.iter_mut().find(|node| node.node_id() == from) {
            source.raw_node.report_snapshot(
                to,
                if delivered {
                    SnapshotStatus::Finish
                } else {
                    SnapshotStatus::Failure
                },
            );
        }
    }

    committed
}

/// Run ticks until a leader is elected. Returns leader's node_id.
fn elect_leader(nodes: &mut [RaftNode]) -> u64 {
    let all_active = vec![true; nodes.len()];
    for _ in 0..50 {
        tick_cycle(nodes, &all_active);
    }
    nodes
        .iter()
        .find(|n| n.raw_node.raft.state == StateRole::Leader)
        .map(|n| n.node_id())
        .expect("No leader elected after 50 ticks")
}

/// Find the index of a node by its ID.
fn node_idx(nodes: &[RaftNode], id: u64) -> usize {
    nodes.iter().position(|n| n.node_id() == id).unwrap()
}

fn persisted_entry_data(node: &RaftNode) -> Vec<Vec<u8>> {
    let first = node.storage.first_index().unwrap();
    let last = node.storage.last_index().unwrap();
    if first > last {
        return vec![];
    }
    node.storage
        .entries(first, last + 1, None, GetEntriesContext::empty(false))
        .unwrap()
        .into_iter()
        .map(|entry| entry.data.to_vec())
        .collect()
}

fn persisted_log_contains(node: &RaftNode, payload: &[u8]) -> bool {
    persisted_entry_data(node)
        .iter()
        .any(|data| data.as_slice() == payload)
}

fn wait_for_committed_payload(
    nodes: &mut [RaftNode],
    active: &[bool],
    payload: &[u8],
    ticks: usize,
) -> bool {
    for _ in 0..ticks {
        let committed = tick_cycle(nodes, active);
        if committed.iter().any(|data| data.as_slice() == payload) {
            return true;
        }
    }
    false
}

#[derive(Clone)]
struct SimMessage {
    from: u64,
    to: u64,
    deliver_at_tick: usize,
    msg: raft::prelude::Message,
}

struct SeededRaftSimulation {
    seed: u64,
    tick: usize,
    nodes: Vec<RaftNode>,
    active: Vec<bool>,
    rng: ChaCha12Rng,
    network: Vec<SimMessage>,
    reliable_network: bool,
    trace: Vec<String>,
    proposed_payloads: BTreeSet<Vec<u8>>,
    committed_payloads: BTreeSet<Vec<u8>>,
}

impl SeededRaftSimulation {
    const MAX_TRACE_LINES: usize = 512;

    fn new(seed: u64) -> Self {
        let mut nodes = create_three_node_group();
        nodes[0].raw_node.campaign().unwrap();
        let all_active = vec![true; nodes.len()];
        for _ in 0..20 {
            tick_cycle(&mut nodes, &all_active);
            if nodes[0].raw_node.raft.state == StateRole::Leader {
                break;
            }
        }
        assert_eq!(
            nodes[0].raw_node.raft.state,
            StateRole::Leader,
            "seed={seed:#x}: deterministic simulator setup expected node 1 to win the initial \
             campaign",
        );
        let leader = nodes[0].node_id();
        let mut seed_bytes = [0u8; 32];
        seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
        let mut sim = Self {
            seed,
            tick: 0,
            active: vec![true; nodes.len()],
            nodes,
            rng: ChaCha12Rng::from_seed(seed_bytes),
            network: Vec::new(),
            reliable_network: false,
            trace: Vec::new(),
            proposed_payloads: BTreeSet::new(),
            committed_payloads: BTreeSet::new(),
        };
        sim.record(format!("initial_leader={leader}"));
        sim
    }

    fn record(&mut self, event: impl Into<String>) {
        if self.trace.len() == Self::MAX_TRACE_LINES {
            self.trace.remove(0);
        }
        self.trace
            .push(format!("tick={}: {}", self.tick, event.into()));
    }

    fn trace_dump(&self) -> String {
        self.trace.join("\n")
    }

    fn current_leader_idx(&self) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .find(|(idx, node)| self.active[*idx] && node.raw_node.raft.state == StateRole::Leader)
            .map(|(idx, _)| idx)
    }

    fn active_count(&self) -> usize {
        self.active.iter().filter(|active| **active).count()
    }

    fn propose_on_current_leader(&mut self) {
        let Some(leader_idx) = self.current_leader_idx() else {
            self.record("proposal_skipped_no_active_leader");
            return;
        };
        let payload = format!(
            "seed-{:#x}-proposal-{}",
            self.seed,
            self.proposed_payloads.len()
        )
        .into_bytes();
        match self.nodes[leader_idx]
            .raw_node
            .propose(vec![], payload.clone())
        {
            Ok(()) => {
                self.record(format!(
                    "proposal node={} payload={}",
                    self.nodes[leader_idx].node_id(),
                    String::from_utf8_lossy(&payload),
                ));
                self.proposed_payloads.insert(payload);
            },
            Err(err) => {
                self.record(format!(
                    "proposal_rejected node={} error={err:?}",
                    self.nodes[leader_idx].node_id(),
                ));
            },
        }
    }

    fn pause_random_follower_if_safe(&mut self) {
        if self.active_count() <= 2 {
            return;
        }
        let leader_idx = self.current_leader_idx();
        let active_indices: Vec<_> = self
            .active
            .iter()
            .enumerate()
            .filter_map(|(idx, active)| (*active && Some(idx) != leader_idx).then_some(idx))
            .collect();
        if active_indices.is_empty() {
            return;
        }
        let idx = active_indices[self.rng.random_range(0..active_indices.len())];
        self.active[idx] = false;
        let node_id = self.nodes[idx].node_id();
        let before = self.network.len();
        self.network
            .retain(|msg| msg.from != node_id && msg.to != node_id);
        let dropped = before - self.network.len();
        self.record(format!("pause node={node_id} dropped_inflight={dropped}"));
    }

    fn heal_all_nodes(&mut self) {
        if self.active.iter().all(|active| *active) {
            return;
        }
        for active in &mut self.active {
            *active = true;
        }
        self.record("heal_all_nodes");
    }

    fn enqueue_or_drop_message(&mut self, from: u64, to: u64, msg: raft::prelude::Message) {
        let msg_type = msg.get_msg_type();
        let is_snapshot = msg_type == raft::prelude::MessageType::MsgSnapshot;
        if self.reliable_network {
            self.record(format!(
                "enqueue_reliable from={from} to={to} type={msg_type:?}"
            ));
            self.network.push(SimMessage {
                from,
                to,
                deliver_at_tick: self.tick,
                msg,
            });
            return;
        }

        let roll = self.rng.random_range(0..100);
        if roll < 8 {
            self.record(format!("drop from={from} to={to} type={msg_type:?}"));
            if is_snapshot {
                self.report_snapshot(from, to, SnapshotStatus::Failure);
            }
            return;
        }

        let delay = self.rng.random_range(0..=3);
        self.record(format!(
            "enqueue from={from} to={to} type={msg_type:?} delay={delay}",
        ));
        self.network.push(SimMessage {
            from,
            to,
            deliver_at_tick: self.tick + delay,
            msg: msg.clone(),
        });

        if roll >= 8 && roll < 13 {
            let duplicate_delay = delay + self.rng.random_range(0..=2);
            self.record(format!(
                "duplicate from={from} to={to} type={msg_type:?} delay={duplicate_delay}",
            ));
            self.network.push(SimMessage {
                from,
                to,
                deliver_at_tick: self.tick + duplicate_delay,
                msg,
            });
        }
    }

    fn report_snapshot(&mut self, from: u64, to: u64, status: SnapshotStatus) {
        if let Some(source) = self.nodes.iter_mut().find(|node| node.node_id() == from) {
            source.raw_node.report_snapshot(to, status);
        }
    }

    fn deliver_due_messages(&mut self) {
        let mut due = Vec::new();
        let mut pending = Vec::new();
        for msg in self.network.drain(..) {
            if msg.deliver_at_tick <= self.tick {
                due.push(msg);
            } else {
                pending.push(msg);
            }
        }
        self.network = pending;

        while !due.is_empty() {
            let idx = self.rng.random_range(0..due.len());
            let msg = due.swap_remove(idx);
            let msg_type = msg.msg.get_msg_type();
            let is_snapshot = msg_type == raft::prelude::MessageType::MsgSnapshot;
            let Some(target_idx) = self.nodes.iter().position(|node| node.node_id() == msg.to)
            else {
                self.record(format!(
                    "drop_unknown_target from={} to={} type={msg_type:?}",
                    msg.from, msg.to,
                ));
                if is_snapshot {
                    self.report_snapshot(msg.from, msg.to, SnapshotStatus::Failure);
                }
                continue;
            };
            if !self.active[target_idx] {
                self.record(format!(
                    "drop_inactive_target from={} to={} type={msg_type:?}",
                    msg.from, msg.to,
                ));
                if is_snapshot {
                    self.report_snapshot(msg.from, msg.to, SnapshotStatus::Failure);
                }
                continue;
            }

            self.record(format!(
                "deliver from={} to={} type={msg_type:?}",
                msg.from, msg.to,
            ));
            if let Err(err) = self.nodes[target_idx].raw_node.step(msg.msg) {
                self.record(format!(
                    "step_error target={} type={msg_type:?} error={err:?}",
                    msg.to,
                ));
                if is_snapshot {
                    self.report_snapshot(msg.from, msg.to, SnapshotStatus::Failure);
                }
            } else if is_snapshot {
                self.report_snapshot(msg.from, msg.to, SnapshotStatus::Finish);
            }
        }
    }

    fn tick_once(&mut self) {
        self.tick += 1;
        let mut all_messages: Vec<(u64, u64, raft::prelude::Message)> = Vec::new();
        let mut commit_events = Vec::new();
        let mut committed_payloads = Vec::new();
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            if self.active[idx] {
                node.raw_node.tick();
            }
        }

        for (idx, node) in self.nodes.iter_mut().enumerate() {
            if !self.active[idx] || !node.raw_node.has_ready() {
                continue;
            }

            let mut ready = node.raw_node.ready();
            for msg in ready.take_messages() {
                all_messages.push((node.node_id(), msg.to, msg));
            }

            if !ready.snapshot().is_empty() {
                node.storage.apply_snapshot(ready.snapshot()).unwrap();
                node.storage
                    .set_applied_index(ready.snapshot().get_metadata().index)
                    .unwrap();
            }
            node.storage.append_entries(ready.entries()).unwrap();
            if let Some(hs) = ready.hs() {
                node.storage.set_hardstate(hs).unwrap();
            }

            for entry in ready.take_committed_entries() {
                if !entry.data.is_empty()
                    && entry.get_entry_type() == raft::prelude::EntryType::EntryNormal
                {
                    commit_events.push(format!(
                        "commit node={} index={} payload={}",
                        node.node_id(),
                        entry.index,
                        String::from_utf8_lossy(&entry.data),
                    ));
                    committed_payloads.push(entry.data.to_vec());
                }
                node.storage.set_applied_index(entry.index).unwrap();
            }

            for msg in ready.take_persisted_messages() {
                all_messages.push((node.node_id(), msg.to, msg));
            }

            let mut light_rd = node.raw_node.advance(ready);
            for entry in light_rd.take_committed_entries() {
                if !entry.data.is_empty()
                    && entry.get_entry_type() == raft::prelude::EntryType::EntryNormal
                {
                    commit_events.push(format!(
                        "commit_light node={} index={} payload={}",
                        node.node_id(),
                        entry.index,
                        String::from_utf8_lossy(&entry.data),
                    ));
                    committed_payloads.push(entry.data.to_vec());
                }
                node.storage.set_applied_index(entry.index).unwrap();
            }
            node.raw_node.advance_apply();
        }

        for event in commit_events {
            self.record(event);
        }
        self.committed_payloads.extend(committed_payloads);

        for (from, to, msg) in all_messages {
            self.enqueue_or_drop_message(from, to, msg);
        }
        self.deliver_due_messages();
    }

    fn random_nemesis_step(&mut self) {
        match self.rng.random_range(0..100) {
            0..=34 => self.propose_on_current_leader(),
            35..=42 => self.pause_random_follower_if_safe(),
            43..=50 => self.heal_all_nodes(),
            _ => {},
        }
        self.tick_once();
    }

    fn drain_to_convergence(&mut self) {
        self.heal_all_nodes();
        self.reliable_network = true;
        for msg in &mut self.network {
            msg.deliver_at_tick = self.tick;
        }
        for _ in 0..500 {
            self.tick_once();
            if self.network.is_empty()
                && self.nodes.iter().all(|node| {
                    node.raw_node.raft.raft_log.committed
                        == self.nodes[0].raw_node.raft.raft_log.committed
                })
                && self.committed_payloads_replicated()
            {
                break;
            }
        }
    }

    fn committed_payloads_replicated(&self) -> bool {
        self.committed_payloads.iter().all(|payload| {
            self.nodes
                .iter()
                .all(|node| persisted_log_contains(node, payload))
        })
    }

    fn assert_invariants(&self) {
        assert!(
            self.proposed_payloads.len() >= 8,
            "seed={:#x}\nexpected enough proposals to exercise the simulator, got {}\ntrace:\n{}",
            self.seed,
            self.proposed_payloads.len(),
            self.trace_dump(),
        );
        assert!(
            self.committed_payloads.len() >= 4,
            "seed={:#x}\nexpected enough committed entries to exercise the simulator, got \
             {}\ntrace:\n{}",
            self.seed,
            self.committed_payloads.len(),
            self.trace_dump(),
        );

        let committed_indices: Vec<_> = self
            .nodes
            .iter()
            .map(|node| node.raw_node.raft.raft_log.committed)
            .collect();
        assert!(
            committed_indices
                .windows(2)
                .all(|window| window[0] == window[1]),
            "seed={:#x}\ncommitted indexes diverged after convergence: {:?}\ntrace:\n{}",
            self.seed,
            committed_indices,
            self.trace_dump(),
        );

        for payload in &self.committed_payloads {
            for node in &self.nodes {
                assert!(
                    persisted_log_contains(node, payload),
                    "seed={:#x}\ncommitted payload {} missing from node {}\ntrace:\n{}",
                    self.seed,
                    String::from_utf8_lossy(payload),
                    node.node_id(),
                    self.trace_dump(),
                );
            }
        }
    }
}

#[tokio::test]
async fn test_seeded_raft_nemesis_preserves_committed_payloads() {
    // This is the first bounded #134 deterministic simulation slice. It does
    // not replace Docker chaos tests; it explores many more Raft message
    // interleavings cheaply and prints a seed/trace if an invariant fails.
    for seed in [
        0x1340_0000_0000_0001,
        0x1340_0000_0000_0002,
        0x1340_0000_0000_0003,
    ] {
        let mut sim = SeededRaftSimulation::new(seed);
        for _ in 0..240 {
            sim.random_nemesis_step();
        }
        sim.drain_to_convergence();
        sim.assert_invariants();
    }
}

#[tokio::test]
async fn test_seeded_raft_simulation_trace_is_reproducible() {
    let seed = 0x1340_0000_0000_00aa;
    let run = |seed| {
        let mut sim = SeededRaftSimulation::new(seed);
        for _ in 0..80 {
            sim.random_nemesis_step();
        }
        sim.drain_to_convergence();
        let trace = sim.trace_dump();
        sim.assert_invariants();
        trace
    };

    let first_trace = run(seed);
    let second_trace = run(seed);
    assert_eq!(
        first_trace, second_trace,
        "same seed should produce the same Raft nemesis trace",
    );
}

// ============================================================
// Test 1: 3-node election (already existed, rewritten with helpers)
// ============================================================

#[tokio::test]
async fn test_three_node_election() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);

    // Exactly one leader.
    let leader_count = nodes
        .iter()
        .filter(|n| n.raw_node.raft.state == StateRole::Leader)
        .count();
    assert_eq!(leader_count, 1);

    // All agree on leader.
    for node in &nodes {
        assert_eq!(node.raw_node.raft.leader_id, leader_id);
    }
}

// ============================================================
// Test 2: Propose-commit across majority
// ============================================================

#[tokio::test]
async fn test_three_node_propose_commit() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);

    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"test data".to_vec())
        .unwrap();

    let all_active = vec![true; 3];
    let mut all_committed = Vec::new();
    for _ in 0..20 {
        all_committed.extend(tick_cycle(&mut nodes, &all_active));
        if all_committed.iter().any(|d| d == b"test data") {
            break;
        }
    }

    assert!(
        all_committed.iter().any(|d| d == b"test data"),
        "Proposal not committed: {:?}",
        all_committed
    );
}

// ============================================================
// Test 3: Kill leader, verify re-election (CockroachDB failover)
// ============================================================

#[tokio::test]
async fn test_kill_leader_reelection() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);

    // Kill the leader (mark as inactive).
    let mut active = vec![true; 3];
    active[old_leader_idx] = false;

    // Tick the remaining 2 nodes until a new leader is elected.
    let mut new_leader_id = 0u64;
    for _ in 0..50 {
        tick_cycle(&mut nodes, &active);
        for (i, node) in nodes.iter().enumerate() {
            if active[i] && node.raw_node.raft.state == StateRole::Leader {
                new_leader_id = node.node_id();
            }
        }
        if new_leader_id != 0 && new_leader_id != old_leader_id {
            break;
        }
    }

    assert_ne!(
        new_leader_id, 0,
        "No new leader elected after killing old leader"
    );
    assert_ne!(
        new_leader_id, old_leader_id,
        "New leader should be different from killed leader"
    );
}

// ============================================================
// Test 4: Write during leader transition (CockroachDB failover)
// ============================================================

#[tokio::test]
async fn test_write_during_leader_transition() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);

    // Propose data BEFORE killing the leader.
    nodes[old_leader_idx]
        .raw_node
        .propose(vec![], b"before kill".to_vec())
        .unwrap();

    // Let it replicate to at least one follower.
    let all_active = vec![true; 3];
    let mut committed = Vec::new();
    for _ in 0..10 {
        committed.extend(tick_cycle(&mut nodes, &all_active));
    }

    assert!(
        committed.iter().any(|d| d == b"before kill"),
        "Pre-kill write should be committed"
    );

    // Kill the leader.
    let mut active = vec![true; 3];
    active[old_leader_idx] = false;

    // Wait for new leader.
    for _ in 0..50 {
        tick_cycle(&mut nodes, &active);
    }

    let new_leader_id = nodes
        .iter()
        .enumerate()
        .find(|(i, n)| active[*i] && n.raw_node.raft.state == StateRole::Leader)
        .map(|(_, n)| n.node_id());

    assert!(new_leader_id.is_some(), "New leader should be elected");
    let new_leader_idx = node_idx(&nodes, new_leader_id.unwrap());

    // Propose data on the new leader.
    nodes[new_leader_idx]
        .raw_node
        .propose(vec![], b"after kill".to_vec())
        .unwrap();

    let mut post_committed = Vec::new();
    for _ in 0..20 {
        post_committed.extend(tick_cycle(&mut nodes, &active));
        if post_committed.iter().any(|d| d == b"after kill") {
            break;
        }
    }

    assert!(
        post_committed.iter().any(|d| d == b"after kill"),
        "Post-kill write should be committed on new leader"
    );
}

#[tokio::test]
async fn test_unquorumed_leader_proposal_is_truncated_after_failover() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);
    let all_active = vec![true; 3];
    let unquorumed_payload = b"old-leader-local-only";

    nodes[old_leader_idx]
        .raw_node
        .propose(vec![], unquorumed_payload.to_vec())
        .unwrap();

    let mut committed = Vec::new();
    for _ in 0..3 {
        committed.extend(tick_cycle_with_message_filter(
            &mut nodes,
            &all_active,
            |from, _, msg| {
                !(from == old_leader_id
                    && matches!(
                        msg.get_msg_type(),
                        raft::prelude::MessageType::MsgAppend
                            | raft::prelude::MessageType::MsgHeartbeat
                    ))
            },
        ));
    }

    assert!(
        persisted_log_contains(&nodes[old_leader_idx], unquorumed_payload),
        "old leader should have persisted the local proposal before losing leadership",
    );
    assert!(
        !committed
            .iter()
            .any(|data| data.as_slice() == unquorumed_payload),
        "proposal without quorum must not commit before failover",
    );

    let mut active = vec![true; 3];
    active[old_leader_idx] = false;
    let mut new_leader_id = 0;
    for _ in 0..80 {
        tick_cycle(&mut nodes, &active);
        if let Some(new_leader) = nodes
            .iter()
            .enumerate()
            .find(|(i, node)| {
                active[*i]
                    && node.raw_node.raft.state == StateRole::Leader
                    && node.node_id() != old_leader_id
            })
            .map(|(_, node)| node.node_id())
        {
            new_leader_id = new_leader;
            break;
        }
    }
    assert_ne!(
        new_leader_id, 0,
        "majority should elect a replacement leader"
    );

    let new_leader_idx = node_idx(&nodes, new_leader_id);
    let committed_payload = b"new-leader-committed";
    nodes[new_leader_idx]
        .raw_node
        .propose(vec![], committed_payload.to_vec())
        .unwrap();
    assert!(
        wait_for_committed_payload(&mut nodes, &active, committed_payload, 40),
        "new leader should commit a replacement entry with the surviving majority",
    );

    active[old_leader_idx] = true;
    for _ in 0..120 {
        tick_cycle(&mut nodes, &active);
        if nodes
            .iter()
            .all(|node| persisted_log_contains(node, committed_payload))
            && nodes
                .iter()
                .all(|node| !persisted_log_contains(node, unquorumed_payload))
        {
            break;
        }
    }

    for node in &nodes {
        assert!(
            !persisted_log_contains(node, unquorumed_payload),
            "uncommitted old-leader entry must be truncated on node {} after rejoin",
            node.node_id(),
        );
        assert!(
            persisted_log_contains(node, committed_payload),
            "replacement committed entry should be present on node {}",
            node.node_id(),
        );
    }
}

#[tokio::test]
async fn test_quorumed_proposal_survives_leader_death_before_all_followers_catch_up() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);
    let lagging_follower_id = nodes
        .iter()
        .find(|node| node.node_id() != leader_id)
        .expect("expected a follower")
        .node_id();
    let lagging_follower_idx = node_idx(&nodes, lagging_follower_id);
    let all_active = vec![true; 3];
    let payload = b"quorumed-before-leader-death";

    nodes[leader_idx]
        .raw_node
        .propose(vec![], payload.to_vec())
        .unwrap();

    let mut committed = false;
    for _ in 0..40 {
        let cycle_committed =
            tick_cycle_with_message_filter(&mut nodes, &all_active, |from, to, msg| {
                !(from == leader_id
                    && to == lagging_follower_id
                    && matches!(
                        msg.get_msg_type(),
                        raft::prelude::MessageType::MsgAppend
                            | raft::prelude::MessageType::MsgHeartbeat
                    ))
            });
        if cycle_committed
            .iter()
            .any(|data| data.as_slice() == payload)
        {
            committed = true;
            break;
        }
    }
    assert!(
        committed,
        "leader plus one follower should commit while the other follower is lagging",
    );
    assert!(
        !persisted_log_contains(&nodes[lagging_follower_idx], payload),
        "the isolated follower should genuinely lag before leader death",
    );

    let mut active = vec![true; 3];
    active[leader_idx] = false;
    let survivor_with_entry = nodes
        .iter()
        .enumerate()
        .find(|(i, node)| *i != leader_idx && persisted_log_contains(node, payload))
        .map(|(_, node)| node.node_id())
        .expect("one surviving follower should have the quorumed entry");

    let mut new_leader_id = 0;
    for _ in 0..80 {
        tick_cycle(&mut nodes, &active);
        if let Some(new_leader) = nodes
            .iter()
            .enumerate()
            .find(|(i, node)| active[*i] && node.raw_node.raft.state == StateRole::Leader)
            .map(|(_, node)| node.node_id())
        {
            new_leader_id = new_leader;
            break;
        }
    }
    assert_eq!(
        new_leader_id, survivor_with_entry,
        "only the surviving voter with the committed entry should be able to lead safely",
    );

    active[leader_idx] = true;
    for _ in 0..120 {
        tick_cycle(&mut nodes, &active);
        if nodes
            .iter()
            .all(|node| persisted_log_contains(node, payload))
        {
            break;
        }
    }

    for node in &nodes {
        assert!(
            persisted_log_contains(node, payload),
            "quorumed proposal should survive leader death and catch up node {}",
            node.node_id(),
        );
    }
}

#[tokio::test]
async fn test_isolated_old_leader_steps_down_without_quorum() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);
    let all_active = vec![true; 3];

    for _ in 0..80 {
        tick_cycle_with_message_filter(&mut nodes, &all_active, |from, to, _| {
            from != old_leader_id && to != old_leader_id
        });
        if nodes[old_leader_idx].raw_node.raft.state != StateRole::Leader {
            break;
        }
    }

    assert_ne!(
        nodes[old_leader_idx].raw_node.raft.state,
        StateRole::Leader,
        "old leader should step down after losing quorum",
    );
    assert_eq!(
        nodes[old_leader_idx].raw_node.raft.leader_id, 0,
        "isolated old leader should not believe it still has a known leader",
    );
}

#[tokio::test]
async fn test_majority_commits_while_old_leader_is_isolated() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);
    let all_active = vec![true; 3];

    for _ in 0..80 {
        tick_cycle_with_message_filter(&mut nodes, &all_active, |from, to, _| {
            from != old_leader_id && to != old_leader_id
        });
        if nodes
            .iter()
            .enumerate()
            .any(|(i, node)| i != old_leader_idx && node.raw_node.raft.state == StateRole::Leader)
        {
            break;
        }
    }

    let new_leader_idx = nodes
        .iter()
        .enumerate()
        .find(|(i, node)| i != &old_leader_idx && node.raw_node.raft.state == StateRole::Leader)
        .map(|(i, _)| i)
        .expect("surviving majority should elect a new leader");
    let majority_payload = b"majority-after-old-leader-isolated";
    nodes[new_leader_idx]
        .raw_node
        .propose(vec![], majority_payload.to_vec())
        .unwrap();

    let mut committed = Vec::new();
    for _ in 0..40 {
        committed.extend(tick_cycle_with_message_filter(
            &mut nodes,
            &all_active,
            |from, to, _| from != old_leader_id && to != old_leader_id,
        ));
        if committed
            .iter()
            .any(|data| data.as_slice() == majority_payload)
        {
            break;
        }
    }

    assert!(
        committed
            .iter()
            .any(|data| data.as_slice() == majority_payload),
        "surviving majority should continue committing while old leader is isolated",
    );
    assert!(
        !persisted_log_contains(&nodes[old_leader_idx], majority_payload),
        "isolated old leader should not see majority writes before the partition heals",
    );

    for _ in 0..120 {
        tick_cycle(&mut nodes, &all_active);
        if nodes
            .iter()
            .all(|node| persisted_log_contains(node, majority_payload))
        {
            break;
        }
    }

    for node in &nodes {
        assert!(
            persisted_log_contains(node, majority_payload),
            "majority commit should catch up node {} after isolation heals",
            node.node_id(),
        );
    }
}

// ============================================================
// Test 5: Network partition (1 isolated) — TiKV/YugabyteDB
// ============================================================

#[tokio::test]
async fn test_network_partition_minority_isolated() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);

    // Isolate one follower (not the leader).
    let follower_idx = (0..3).find(|&i| i != leader_idx).unwrap();
    let mut active = vec![true; 3];
    active[follower_idx] = false;

    // The majority (leader + 1 follower) should still accept writes.
    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"during partition".to_vec())
        .unwrap();

    let mut committed = Vec::new();
    for _ in 0..20 {
        committed.extend(tick_cycle(&mut nodes, &active));
        if committed.iter().any(|d| d == b"during partition") {
            break;
        }
    }

    assert!(
        committed.iter().any(|d| d == b"during partition"),
        "Majority should still commit during minority partition"
    );
}

#[tokio::test]
async fn test_asymmetric_leader_to_follower_message_loss_recovers() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);
    let lagging_follower_id = nodes
        .iter()
        .find(|node| node.node_id() != leader_id)
        .expect("expected a follower")
        .node_id();
    let lagging_follower_idx = node_idx(&nodes, lagging_follower_id);
    let all_active = vec![true; 3];

    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"asymmetric-loss".to_vec())
        .unwrap();

    let mut committed = Vec::new();
    for _ in 0..30 {
        committed.extend(tick_cycle_with_message_filter(
            &mut nodes,
            &all_active,
            |from, to, _| !(from == leader_id && to == lagging_follower_id),
        ));
        if committed.iter().any(|data| data == b"asymmetric-loss") {
            break;
        }
    }

    assert!(
        committed.iter().any(|data| data == b"asymmetric-loss"),
        "leader plus the other follower should still form a quorum under one-way message loss",
    );
    assert!(
        nodes[lagging_follower_idx].storage.last_index().unwrap()
            < nodes[leader_idx].storage.last_index().unwrap(),
        "the follower that cannot receive leader messages should actually lag before the link \
         heals",
    );

    for _ in 0..80 {
        tick_cycle(&mut nodes, &all_active);
        if nodes[lagging_follower_idx].storage.last_index().unwrap()
            == nodes[leader_idx].storage.last_index().unwrap()
            && nodes[lagging_follower_idx].raw_node.raft.raft_log.committed
                == nodes[leader_idx].raw_node.raft.raft_log.committed
        {
            break;
        }
    }

    assert_eq!(
        nodes[lagging_follower_idx].storage.last_index().unwrap(),
        nodes[leader_idx].storage.last_index().unwrap(),
        "lagging follower should catch up after asymmetric message loss heals",
    );
    assert_eq!(
        nodes[lagging_follower_idx].raw_node.raft.raft_log.committed,
        nodes[leader_idx].raw_node.raft.raft_log.committed,
        "lagging follower should recover the leader's committed index",
    );
}

// ============================================================
// Test 6: Leader rejoins as follower (CockroachDB)
// ============================================================

#[tokio::test]
async fn test_leader_rejoins_as_follower() {
    let mut nodes = create_three_node_group();
    let old_leader_id = elect_leader(&mut nodes);
    let old_leader_idx = node_idx(&nodes, old_leader_id);

    // Kill the leader.
    let mut active = vec![true; 3];
    active[old_leader_idx] = false;

    // Wait for new leader.
    for _ in 0..50 {
        tick_cycle(&mut nodes, &active);
    }

    let new_leader_id = nodes
        .iter()
        .enumerate()
        .find(|(i, n)| active[*i] && n.raw_node.raft.state == StateRole::Leader)
        .map(|(_, n)| n.node_id())
        .expect("New leader not elected");

    assert_ne!(new_leader_id, old_leader_id);

    // Bring the old leader back.
    active[old_leader_idx] = true;

    // Tick to let the old leader discover the new term and step down.
    for _ in 0..30 {
        tick_cycle(&mut nodes, &active);
    }

    // The old leader should now be a follower.
    assert_ne!(
        nodes[old_leader_idx].raw_node.raft.state,
        StateRole::Leader,
        "Old leader should have stepped down to follower"
    );

    // The new leader should still be leader.
    let new_leader_idx = node_idx(&nodes, new_leader_id);
    assert_eq!(
        nodes[new_leader_idx].raw_node.raft.state,
        StateRole::Leader,
        "New leader should still be leading"
    );
}

// ============================================================
// Test 7: All 3 nodes have same committed entries
// ============================================================

#[tokio::test]
async fn test_raft_log_consistency() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);

    // Propose 5 entries.
    for i in 0..5 {
        nodes[leader_idx]
            .raw_node
            .propose(vec![], format!("entry-{i}").into_bytes())
            .unwrap();
    }

    let all_active = vec![true; 3];
    for _ in 0..30 {
        tick_cycle(&mut nodes, &all_active);
    }

    // All nodes should have the same commit index.
    let commit_indices: Vec<u64> = nodes
        .iter()
        .map(|n| n.raw_node.raft.raft_log.committed)
        .collect();

    assert_eq!(
        commit_indices[0], commit_indices[1],
        "Nodes 1 and 2 commit indices differ: {:?}",
        commit_indices
    );
    assert_eq!(
        commit_indices[1], commit_indices[2],
        "Nodes 2 and 3 commit indices differ: {:?}",
        commit_indices
    );

    // Commit index should be at least 5 (our entries) + initial entries.
    assert!(
        commit_indices[0] >= 5,
        "Commit index too low: {}",
        commit_indices[0]
    );
}

// ============================================================
// Test 8: Lagging follower catches up via snapshot
// ============================================================

#[tokio::test]
async fn test_lagging_follower_catches_up_via_snapshot() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);
    let lagging_idx = (0..3).find(|&i| i != leader_idx).unwrap();

    let mut active = vec![true; 3];
    active[lagging_idx] = false;

    for i in 0..12 {
        nodes[leader_idx]
            .raw_node
            .propose(vec![], format!("snapshot-entry-{i}").into_bytes())
            .unwrap();
        for _ in 0..5 {
            tick_cycle(&mut nodes, &active);
        }
    }

    let compact_index = nodes[leader_idx].raw_node.raft.raft_log.committed;
    assert!(
        compact_index > 1,
        "expected committed work before compaction"
    );
    nodes[leader_idx].storage.compact(compact_index).unwrap();

    active[lagging_idx] = true;
    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"post-snapshot".to_vec())
        .unwrap();

    for _ in 0..80 {
        tick_cycle(&mut nodes, &active);
        if nodes[lagging_idx].storage.last_index().unwrap()
            == nodes[leader_idx].storage.last_index().unwrap()
            && nodes[lagging_idx].storage.first_index().unwrap() > 1
        {
            break;
        }
    }

    assert_eq!(
        nodes[lagging_idx].storage.last_index().unwrap(),
        nodes[leader_idx].storage.last_index().unwrap(),
        "lagging follower should catch up to leader's last index",
    );
    assert!(
        nodes[lagging_idx].storage.first_index().unwrap() > 1,
        "lagging follower should have installed a snapshot instead of replaying from index 1",
    );
}

#[tokio::test]
async fn test_failed_snapshot_delivery_retries_and_catches_up() {
    let mut nodes = create_three_node_group();
    let leader_id = elect_leader(&mut nodes);
    let leader_idx = node_idx(&nodes, leader_id);
    let lagging_idx = (0..3).find(|&i| i != leader_idx).unwrap();
    let lagging_id = nodes[lagging_idx].node_id();

    let mut active = vec![true; 3];
    active[lagging_idx] = false;

    for i in 0..12 {
        nodes[leader_idx]
            .raw_node
            .propose(vec![], format!("snapshot-retry-entry-{i}").into_bytes())
            .unwrap();
        for _ in 0..5 {
            tick_cycle(&mut nodes, &active);
        }
    }

    let compact_index = nodes[leader_idx].raw_node.raft.raft_log.committed;
    assert!(
        compact_index > 1,
        "expected committed work before compaction"
    );
    nodes[leader_idx].storage.compact(compact_index).unwrap();

    active[lagging_idx] = true;
    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"post-failed-snapshot".to_vec())
        .unwrap();

    for _ in 0..40 {
        tick_cycle_with_message_filter(&mut nodes, &active, |_, to, msg| {
            !(to == lagging_id && msg.get_msg_type() == raft::prelude::MessageType::MsgSnapshot)
        });
    }

    assert!(
        nodes[lagging_idx].storage.last_index().unwrap()
            < nodes[leader_idx].storage.last_index().unwrap(),
        "lagging follower should remain behind while snapshot delivery fails",
    );

    for _ in 0..120 {
        tick_cycle(&mut nodes, &active);
        if nodes[lagging_idx].storage.last_index().unwrap()
            == nodes[leader_idx].storage.last_index().unwrap()
            && nodes[lagging_idx].storage.first_index().unwrap() > 1
        {
            break;
        }
    }

    assert_eq!(
        nodes[lagging_idx].storage.last_index().unwrap(),
        nodes[leader_idx].storage.last_index().unwrap(),
        "lagging follower should catch up after snapshot delivery recovers",
    );
    assert!(
        nodes[lagging_idx].storage.first_index().unwrap() > 1,
        "lagging follower should install the retried snapshot",
    );
}
