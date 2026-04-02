//! Raft failover integration tests.
//!
//! Tests leader election and failover using a 3-node in-process Raft group.
//! No network — messages pass through mpsc channels directly.

use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::{
    partition::PartitionId,
    raft_node::{
        RaftMessage,
        RaftNode,
        RaftNodeConfig,
    },
};

/// Create a 3-node Raft group with in-memory channels.
fn create_three_node_group() -> (Vec<RaftNode>, Vec<mpsc::UnboundedSender<RaftMessage>>) {
    let peer_ids = vec![1, 2, 3];
    let mut nodes = Vec::new();
    let mut mailbox_txs = Vec::new();
    let mut mailbox_rxs = Vec::new();

    // Create channels for each node.
    for _ in 0..3 {
        let (tx, rx) = mpsc::unbounded_channel();
        mailbox_txs.push(tx);
        mailbox_rxs.push(rx);
    }

    // Create Raft message sender channels between nodes.
    // Each node needs senders for its peers.
    let mut raft_msg_txs: Vec<HashMap<u64, mpsc::UnboundedSender<raft::prelude::Message>>> =
        Vec::new();
    let mut raft_msg_rxs: Vec<Vec<(u64, mpsc::UnboundedReceiver<raft::prelude::Message>)>> =
        Vec::new();

    for i in 0..3 {
        let mut senders = HashMap::new();
        let mut receivers = Vec::new();
        for j in 0..3 {
            if i == j {
                continue;
            }
            let (tx, rx) = mpsc::unbounded_channel();
            senders.insert(peer_ids[j], tx);
            receivers.push((peer_ids[j], rx));
        }
        raft_msg_txs.push(senders);
        raft_msg_rxs.push(receivers);
    }

    // Create nodes.
    for (i, rx) in mailbox_rxs.into_iter().enumerate() {
        let config = RaftNodeConfig {
            node_id: peer_ids[i],
            partition_id: PartitionId(0),
            peers: peer_ids.clone(),
            election_tick: 10,
            heartbeat_tick: 3,
        };

        let node = RaftNode::new(config, rx, raft_msg_txs[i].clone()).unwrap();
        nodes.push(node);
    }

    (nodes, mailbox_txs)
}

/// Route Raft messages between nodes (simulates network).
fn route_messages(nodes: &mut [RaftNode]) {
    // Collect all outgoing messages from each node's peer senders.
    // Since we're using unbounded channels, messages are already in the
    // channels. We need to deliver them by calling step() on the receiving
    // nodes. But the peer_senders in each node send to channels that aren't
    // connected to the receiving nodes' mailboxes — they're separate
    // channels.
    //
    // For the in-process test, we directly step messages by reading from
    // each node's Ready state.
}

/// Test that a 3-node group elects a leader.
#[tokio::test]
async fn test_three_node_election() {
    let (mut nodes, _txs) = create_three_node_group();

    // Tick all nodes to trigger election.
    // Node 1 will likely win since it has the lowest ID.
    for _ in 0..30 {
        for node in nodes.iter_mut() {
            node.raw_node.tick();
        }

        // Process ready on all nodes and route messages.
        let mut all_messages: Vec<(u64, raft::prelude::Message)> = Vec::new();

        for node in nodes.iter_mut() {
            if node.raw_node.has_ready() {
                let mut ready = node.raw_node.ready();

                if let Some(ss) = ready.ss() {
                    let now_leader = ss.raft_state == raft::StateRole::Leader;
                    if now_leader {
                        tracing::info!("Node {} became leader", node.node_id());
                    }
                }

                // Collect messages to route.
                for msg in ready.take_messages() {
                    all_messages.push((msg.to, msg));
                }

                node.storage.append_entries(ready.entries()).unwrap();
                if let Some(hs) = ready.hs() {
                    node.storage.set_hardstate(hs.clone());
                }

                for msg in ready.take_persisted_messages() {
                    all_messages.push((msg.to, msg));
                }

                node.raw_node.advance(ready);
            }
        }

        // Route messages to target nodes.
        for (to, msg) in all_messages {
            for node in nodes.iter_mut() {
                if node.node_id() == to {
                    let _ = node.raw_node.step(msg.clone());
                    break;
                }
            }
        }
    }

    // Exactly one node should be leader.
    let leaders: Vec<_> = nodes
        .iter()
        .filter(|n| n.raw_node.raft.state == raft::StateRole::Leader)
        .collect();

    assert_eq!(
        leaders.len(),
        1,
        "Expected exactly 1 leader, got {}",
        leaders.len()
    );

    let leader_id = leaders[0].node_id();
    tracing::info!("Leader elected: node {}", leader_id);

    // All nodes should agree on the leader.
    for node in &nodes {
        assert_eq!(
            node.raw_node.raft.leader_id,
            leader_id,
            "Node {} disagrees on leader: thinks {} is leader",
            node.node_id(),
            node.raw_node.raft.leader_id,
        );
    }
}

/// Test that a proposal is committed across 3 nodes.
#[tokio::test]
async fn test_three_node_propose_commit() {
    let (mut nodes, _txs) = create_three_node_group();

    // Elect a leader first.
    for _ in 0..30 {
        for node in nodes.iter_mut() {
            node.raw_node.tick();
        }
        let mut msgs = Vec::new();
        for node in nodes.iter_mut() {
            if node.raw_node.has_ready() {
                let mut ready = node.raw_node.ready();
                for msg in ready.take_messages() {
                    msgs.push((msg.to, msg));
                }
                node.storage.append_entries(ready.entries()).unwrap();
                if let Some(hs) = ready.hs() {
                    node.storage.set_hardstate(hs.clone());
                }
                for msg in ready.take_persisted_messages() {
                    msgs.push((msg.to, msg));
                }
                node.raw_node.advance(ready);
            }
        }
        for (to, msg) in msgs {
            for node in nodes.iter_mut() {
                if node.node_id() == to {
                    let _ = node.raw_node.step(msg.clone());
                    break;
                }
            }
        }
    }

    // Find the leader and propose data.
    let leader_idx = nodes
        .iter()
        .position(|n| n.raw_node.raft.state == raft::StateRole::Leader)
        .expect("No leader elected");

    nodes[leader_idx]
        .raw_node
        .propose(vec![], b"replicated mutation".to_vec())
        .unwrap();

    // Process more rounds until committed on majority.
    let mut committed_on: Vec<u64> = Vec::new();
    for _ in 0..20 {
        let mut msgs = Vec::new();
        for node in nodes.iter_mut() {
            node.raw_node.tick();
            if node.raw_node.has_ready() {
                let mut ready = node.raw_node.ready();
                for msg in ready.take_messages() {
                    msgs.push((msg.to, msg));
                }
                node.storage.append_entries(ready.entries()).unwrap();
                if let Some(hs) = ready.hs() {
                    node.storage.set_hardstate(hs.clone());
                }
                for entry in ready.take_committed_entries() {
                    if entry.data == b"replicated mutation" {
                        committed_on.push(node.node_id());
                    }
                }
                for msg in ready.take_persisted_messages() {
                    msgs.push((msg.to, msg));
                }
                let mut light_rd = node.raw_node.advance(ready);
                for entry in light_rd.take_committed_entries() {
                    if entry.data == b"replicated mutation" {
                        committed_on.push(node.node_id());
                    }
                }
                node.raw_node.advance_apply();
            }
        }
        for (to, msg) in msgs {
            for node in nodes.iter_mut() {
                if node.node_id() == to {
                    let _ = node.raw_node.step(msg.clone());
                    break;
                }
            }
        }
        if committed_on.len() >= 2 {
            break;
        }
    }

    assert!(
        committed_on.len() >= 2,
        "Expected majority (>=2) to commit, got {} nodes: {:?}",
        committed_on.len(),
        committed_on,
    );
}
