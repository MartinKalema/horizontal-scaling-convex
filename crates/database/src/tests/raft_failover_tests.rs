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
    collections::HashMap,
    sync::Arc,
};

use raft::{
    SnapshotStatus,
    StateRole,
    Storage,
};
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
