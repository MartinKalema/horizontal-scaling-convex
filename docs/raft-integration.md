# Raft Integration Design: tikv/raft-rs for Automatic Failover

## Overview

Each partition becomes a 3-node Raft group. The leader runs the Committer. If the leader dies, followers elect a new leader and start a Committer. Sub-second failover with zero data loss.

```
Partition 0:
  Node A (leader)  ──Raft──▶ Node C (follower)
                   ──Raft──▶ Node D (follower)

  Node A dies → C and D elect new leader in ~1s → new Committer starts
```

## Library: tikv/raft-rs

The `raft` crate (v0.7.0) provides only the core consensus module. We build:
1. **Storage** — persists Raft log entries and hard state
2. **State Machine** — applies committed entries (our Committer)
3. **Transport** — sends Raft messages between nodes (gRPC)

This is exactly how TiKV integrates raft-rs.

Source: [TiKV Raft in Rust](https://tikv.org/blog/implement-raft-in-rust/)

## Architecture

### The Raft Loop (per partition)

```rust
loop {
    // 1. Receive Raft messages from other nodes
    while let Ok(msg) = mailbox.try_recv() {
        raft_node.step(msg)?;
    }

    // 2. Tick (election/heartbeat timer)
    if elapsed >= 100ms {
        raft_node.tick();
    }

    // 3. If leader, propose pending client mutations
    if raft_node.raft.state == StateRole::Leader {
        for mutation in pending_proposals.drain(..) {
            raft_node.propose(vec![], mutation.serialize())?;
        }
    }

    // 4. Process Ready state
    if raft_node.has_ready() {
        let ready = raft_node.ready();

        // 4a. Send messages to followers/leader
        for msg in ready.messages() {
            transport.send(msg.to, msg);
        }

        // 4b. Apply snapshot if present
        if !ready.snapshot().is_empty() {
            storage.apply_snapshot(ready.snapshot());
        }

        // 4c. Persist log entries
        storage.append(ready.entries());

        // 4d. Persist hard state (term, vote, commit)
        if let Some(hs) = ready.hs() {
            storage.set_hardstate(hs);
        }

        // 4e. Apply committed entries to state machine (Committer)
        for entry in ready.committed_entries() {
            committer.apply(entry);
        }

        // 4f. Advance
        raft_node.advance(ready);
    }
}
```

### Mapping to Convex Components

| raft-rs Component | Convex Component | Implementation |
| --- | --- | --- |
| `Storage` trait | `RaftLogStorage` | PostgreSQL-backed: entries, hard state, snapshots |
| State machine apply | `Committer::apply_raft_entry()` | Deserialize entry → execute mutation → commit |
| `propose()` | Client mutation arrives | Serialize FinalTransaction → propose to Raft |
| `Ready::messages` | `RaftTransport` | gRPC service: AppendEntries, Vote, InstallSnapshot |
| `tick()` | Timer in Raft loop | 100ms interval alongside Committer |
| Snapshot | `SnapshotCheckpointer` | Existing checkpoint mechanism |
| Leader election | Automatic | raft-rs handles internally |

### What Changes

**Intra-partition replication: NATS → Raft**

Before: Leader publishes CommitDelta to NATS, followers consume.
After: Leader proposes to Raft, Raft replicates to followers via log.

**Cross-partition replication: stays NATS**

Partitions 0 and 1 are separate Raft groups. Cross-partition reads still use NATS delta replication between groups.

**Committer lifecycle**

Before: Committer starts at node boot, runs until shutdown.
After: Committer starts when this node becomes Raft leader for a partition. Stops when leadership is lost (demotion, network partition).

### New Files

| File | What it does |
| --- | --- |
| `crates/database/src/raft_node.rs` | Raft loop: tick, propose, on_ready, advance |
| `crates/database/src/raft_storage.rs` | PostgreSQL-backed Storage trait implementation |
| `crates/database/src/raft_transport.rs` | gRPC transport for Raft messages |
| `crates/database/src/raft_state_machine.rs` | Bridges Raft committed entries to Committer |
| `crates/pb/protos/raft_rpc.proto` | Protobuf: AppendEntries, Vote, InstallSnapshot RPCs |

### Modified Files

| File | Change |
| --- | --- |
| `committer.rs` | Add `apply_raft_entry()` method, leader/follower mode |
| `database.rs` | `Database::load()` starts Raft node per partition |
| `lib.rs` (local_backend) | Configure Raft group membership from NODE_ADDRESSES |
| `config.rs` | Add RAFT_PEERS env var |

## Implementation Sequence

1. **Raft Storage**: PostgreSQL-backed log and hard state persistence
2. **Raft Transport**: gRPC service for Raft message passing
3. **Raft Node**: The loop that drives the consensus module
4. **State Machine Bridge**: Connect committed entries to the Committer
5. **Leader Lifecycle**: Start/stop Committer on election/demotion
6. **Integration**: Wire into Database::load and node startup
7. **Tests**: Leader election, failover with writes, snapshot transfer

## Sources

- [tikv/raft-rs](https://github.com/tikv/raft-rs)
- [TiKV: Implement Raft in Rust](https://tikv.org/blog/implement-raft-in-rust/)
- [TiKV: Raft in TiKV source walkthrough](https://www.pingcap.com/blog/raft-in-tikv/)
- [raft-rs five_mem_node example](https://github.com/tikv/raft-rs/blob/master/examples/five_mem_node/main.rs)
- [raft-rs Architecture Overview](https://deepwiki.com/tikv/raft-rs/1.1-architecture-overview)
