# The Actual Implementation Plan: Primary-Replica Architecture for Convex

**This is not the textbook answer. This is what I would actually build.**

---

## Why Not the Full Partitioning Approach

The [earlier proposal](./horizontal-scaling-proposal.md) describes table-level partitioning with distributed consensus, 2PC for cross-partition transactions, and a global timestamp oracle. That design is correct. It's also:

- 12-18 months of work for a senior team
- Introduces Kafka, a TSO service, and cross-node coordination as new operational dependencies
- Solves horizontal *write* scaling, which most apps don't need yet
- Requires changes to nearly every core component simultaneously

The engineering principle that matters here: **solve the problem you actually have, not the one you might have later.** Most Convex deployments will hit read/subscription limits long before they hit write limits. So we scale reads first.

---

## The Architecture: One Primary, Many Replicas

```
                    ┌──────────────────────────────┐
                    │         PRIMARY NODE         │
                    │                              │
                    │  Committer (single writer)   │
                    │  SnapshotManager (authority) │
                    │  Persistence (writes here)   │
                    │  WriteLog → publishes to ──────────┐
                    │                              │     │
                    └──────────────────────────────┘     │
                              ▲                          │
                              │ mutations forwarded      │  distributed log (Kafka/Redpanda/NATS)
                              │                          │   
                    ┌─────────┴────────┐                 │
                    │   Router/LB      │                 │
                    │ mutations→primary│                 │
                    │ queries→replicas │                 │
                    └─────────┬────────┘                 │
                              │ queries/subscriptions    │
                    ┌─────────┴──────────────────────────┴───────────┐
                    │                                                │
          ┌─────────┴──────────┐                    ┌────────────────┴─────┐
          │   REPLICA NODE A   │                    │   REPLICA NODE B     │
          │                    │                    │                      │
          │  LogConsumer ◄─────┤── tails log ──────▶│  LogConsumer         │
          │  SnapshotManager   │                    │  SnapshotManager     │
          │  Subscriptions     │                    │  Subscriptions       │
          │  Query serving     │                    │  Query serving       │
          │  (read-only)       │                    │  (read-only)         │
          └────────────────────┘                    └──────────────────────┘
                    ▲                                          ▲
                    │                                          │
              ┌─────┴──────────────────────────────────────────┴────┐
              │                    Funrun Pool                      │
              │             (already horizontally scaled)           │
              └─────────────────────────────────────────────────────┘
```

### How It Works

**Writes (mutations):**
1. Client sends a mutation
2. Router forwards it to the Primary node
3. Primary's Committer processes it exactly as today — single-threaded, serial, OCC validation
4. After committing to persistence, Primary publishes the write to a distributed log
5. Nothing changes about write semantics. Serializability is preserved trivially because there's still one writer.

**Reads (queries):**
1. Client sends a query
2. Router forwards it to any Replica node (round-robin, least-connections, whatever)
3. Replica serves the query from its local SnapshotManager
4. Replica's SnapshotManager is kept up-to-date by tailing the distributed log

**Subscriptions:**
1. Client opens WebSocket to a Replica node
2. Replica runs the query, creates a subscription
3. Replica's subscription workers tail the distributed log (same as tailing the local write log today)
4. When a write from the Primary appears in the log that affects this subscription, Replica invalidates it, re-runs the query, pushes the update

**The only new component is the distributed log sitting between Primary and Replicas.** Everything else uses existing code paths with minimal modification.

---

## Why This Works With Convex's Existing Code

This is the key insight that makes the plan tractable. The codebase already has the right abstractions:

### 1. The Write Log Already Has a Reader/Writer Split

```
Current:  LogWriter (Committer) → Arc<Mutex<WriteLogManager>> ← LogReader (Subscriptions)
Proposed: LogWriter (Committer) → Kafka topic ← LogConsumer → LogReader (Subscriptions)
```

The `LogReader` interface doesn't change. `wait_for_higher_ts()` and `for_each()` work identically — the data just comes from Kafka instead of shared memory. Subscription workers are completely unaware of the change.

### 2. The SnapshotManager Already Uses Copy-on-Write Structures

The SnapshotManager uses `imbl::OrdMap` — persistent data structures with structural sharing. Replaying a write from the distributed log to update the snapshot is the same operation the Committer already does after each commit: push a new version with the delta applied.

The Committer already calls `snapshot_manager.push(new_snapshot)` after each commit. On replicas, a `LogConsumer` calls the same method after receiving each write from the distributed log.

### 3. The Persistence Trait Supports Read-Only Access

`PersistenceReader` is already a separate trait from `Persistence`. Replicas only need a `PersistenceReader` pointing at the same underlying database (Postgres/MySQL) — they never write. For SQLite, replicas can use SQLite's WAL mode for concurrent read access, or replicas can use their own persistence populated from the distributed log.

### 4. CommitterClient Already Uses Message Passing

```rust
pub struct CommitterClient {
    sender: mpsc::Sender<CommitterMessage>,
    // ...
}
```

On the Primary, this is unchanged — local mpsc channel.
On Replicas, we replace this with a `RemoteCommitterClient` that sends the message over gRPC to the Primary. Same interface, different transport. The rest of the codebase doesn't know or care.

### 5. Funrun Already Demonstrates the Pattern

Funrun is a stateless compute service that reads from the database, executes functions, and returns results to the backend. It communicates via gRPC. It's horizontally scaled.

We're applying the exact same pattern, but in reverse: instead of scaling out compute (Funrun), we're scaling out the read/subscribe path (Replicas). Both leave the write path centralized.

---

## What Changes in Code

### New Components to Build

| Component | What It Does | Complexity |
|-----------|-------------|-----------|
| `DistributedLogWriter` | Publishes committed writes to Kafka/Redpanda after persistence. Wraps existing `LogWriter`. | Low — append to a topic after each commit |
| `DistributedLogConsumer` | Tails the distributed log on Replica nodes. Feeds into a local `LogReader`. | Medium — consumer group management, offset tracking |
| `ReplicaSnapshotUpdater` | Applies writes from the log consumer to the Replica's SnapshotManager. Same logic as the Committer's `publish_commit()`. | Medium — extract and reuse existing Committer logic |
| `RemoteCommitterClient` | Forwards mutations from Replica to Primary via gRPC. Implements the same interface as `CommitterClient`. | Low — gRPC wrapper around CommitterMessage |
| `NodeRouter` | Routes mutations to Primary, queries/subscriptions to Replicas. | Low — simple routing rules |
| `ReplicaDatabase` | A read-only variant of `Database<RT>` that has a `RemoteCommitterClient` instead of a local one. | Medium — mostly constructor changes |

### Existing Components That Change

| Component | Change | Risk |
|-----------|--------|------|
| `Committer::publish_commit()` | Add a call to `DistributedLogWriter::publish()` after existing `log.append()` and `snapshot_manager.push()`. One line of code. | Very low |
| `Database::load()` | Add a constructor path for Replica mode that skips Committer creation and uses `RemoteCommitterClient`. | Low |
| `Database::commit()` | On Replicas, route through `RemoteCommitterClient` instead of local `CommitterClient`. | Low |
| `new_write_log()` | On Replicas, the `LogWriter` side is replaced by `DistributedLogConsumer` feeding into the `LogReader`. | Medium |

### Components That Don't Change At All

| Component | Why |
|-----------|-----|
| Subscription workers | They read from `LogReader`. They don't care where the data comes from. |
| OCC validation | Still happens on the Primary's Committer. Single writer = no cross-node conflicts possible. |
| Funrun | Already independent. Routes to whichever backend node handles the request. |
| Client SDK | Clients connect via WebSocket. The router handles placement. SDK is unaware. |
| Persistence writes | Only the Primary writes. Unchanged. |
| All developer-facing APIs | Queries, mutations, actions behave identically. |

---

## The Consistency Model

### Read-After-Write Consistency

A client sends a mutation, then immediately queries for the result. If the mutation went to the Primary but the query goes to a Replica that hasn't consumed the write yet, the client sees stale data.

**Solution: Sticky sessions + write timestamps.**

1. When a mutation commits on the Primary, it returns the commit timestamp to the client (already happens today).
2. The client includes this timestamp in subsequent queries.
3. The Replica checks: "Have I consumed up to this timestamp?" If not, it waits (bounded) for the log consumer to catch up, or it forwards the query to the Primary.

This is the same pattern as `WAIT_FOR_REPLICATION` in MySQL or `synchronous_commit` scoping in Postgres. The Convex client SDK already tracks timestamps via `RepeatableTimestamp` — we piggyback on that.

### Subscription Consistency

Subscriptions are always consistent because they're driven by the write log. A Replica's subscription worker processes writes in order. The subscription invalidation happens at the correct timestamp. The client sees updates in the same order they were committed.

The only observable difference: the latency between "mutation committed on Primary" and "subscription invalidated on Replica" includes the distributed log propagation delay (~1-5ms within a region). This is imperceptible to users.

### Cross-Replica Consistency

Two clients on different Replicas always see a consistent view because:
- All Replicas consume the same ordered log
- The SnapshotManager on each Replica advances through the same sequence of timestamps
- Queries execute at a `RepeatableTimestamp` that the Replica has fully consumed

A Replica never serves a query at a timestamp it hasn't fully applied. This is enforced by the existing `SnapshotManager::versions` deque — it only contains timestamps that have been fully applied.

---

## Capacity Impact

### Before (Single Node)

| Resource | Limit |
|----------|-------|
| Write throughput | Single Committer (~1000-5000 mutations/sec depending on complexity) |
| Read throughput | Single node RAM + CPU |
| Subscriptions | Single node WebSocket capacity (~10K-50K concurrent) |
| Memory for snapshots/indexes | Single node RAM (typically 8-64 GB) |

### After (1 Primary + N Replicas)

| Resource | Limit |
|----------|-------|
| Write throughput | **Unchanged** — still one Committer. But the Committer is now unburdened from serving reads, so it can dedicate all resources to commits. |
| Read throughput | **Linear scaling with N.** Each Replica serves reads from its own RAM. 4 Replicas = ~4x read capacity. |
| Subscriptions | **Linear scaling with N.** Each Replica handles its own WebSocket connections. 4 Replicas = ~4x subscription capacity. |
| Memory for snapshots/indexes | **Each Replica has its own RAM.** Hot data is replicated, but each Replica can hold the full working set. Total cluster memory = N * node memory. |

### When You'd Actually Need Partitioned Writes

If your app commits more than ~5000 mutations/sec sustained, the single Committer becomes the bottleneck. At that point, you graduate to the [partitioned write architecture](./horizontal-scaling-proposal.md). But that's a scale that very few applications reach — it's roughly equivalent to a mid-tier financial exchange or a very popular multiplayer game server.

For context: Twitter's entire write volume at peak is ~12,000 tweets/sec. Most Convex apps will never need partitioned writes.

---

## Implementation Order

### Step 1: Extract Committer Publish Logic (1-2 weeks)

The Committer's `publish_commit()` method currently does three things in sequence:
1. Updates the SnapshotManager
2. Appends to the WriteLog
3. Notifies waiters

Extract steps 1 and 2 into a `CommitDelta` struct that can be serialized and sent over the wire. This is a pure refactor — no behavior change.

```rust
pub struct CommitDelta {
    pub ts: Timestamp,
    pub document_writes: Vec<DocumentLogEntry>,
    pub index_updates: Vec<DatabaseIndexUpdate>,
    pub table_updates: Vec<TableUpdate>,
    pub write_source: WriteSource,
}
```

### Step 2: Add Distributed Log (2-3 weeks)

Introduce a `DistributedLog` trait:

```rust
#[async_trait]
pub trait DistributedLog: Send + Sync + 'static {
    async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()>;
    async fn subscribe(&self, from_ts: Timestamp) -> Box<dyn Stream<Item = CommitDelta>>;
}
```

Implementations:
- `InMemoryDistributedLog` — for tests and single-node mode (wraps existing WriteLogManager)
- `KafkaDistributedLog` — publishes to / consumes from a Kafka topic
- `NatsDistributedLog` — alternative using NATS JetStream (simpler operationally)

Wire the Primary's Committer to call `distributed_log.publish(delta)` after `publish_commit()`.

### Step 3: Build ReplicaDatabase (2-3 weeks)

Create a `ReplicaDatabase<RT>` that:
- Bootstraps its SnapshotManager from persistence (same as today's `Database::load()`)
- Starts a `LogConsumer` that tails the distributed log
- Applies each `CommitDelta` to its local SnapshotManager and WriteLog
- Has a `RemoteCommitterClient` that forwards mutations to the Primary via gRPC
- Exposes the same `begin()`, `commit()`, `subscribe()` API as `Database<RT>`

### Step 4: Mutation Forwarding (1-2 weeks)

Build the gRPC service on the Primary that accepts remote commits:

```protobuf
service CommitService {
    rpc Commit(CommitRequest) returns (CommitResponse);
}

message CommitRequest {
    bytes serialized_transaction = 1;
    string write_source = 2;
}

message CommitResponse {
    uint64 commit_timestamp = 1;
}
```

Build `RemoteCommitterClient` that implements the same interface as `CommitterClient` but sends over gRPC.

### Step 5: Router + Client Session Affinity (1-2 weeks)

Build a thin routing layer (can be as simple as an nginx/envoy config):
- Mutations → Primary
- Queries/Subscriptions → Replicas (round-robin)
- Include `X-Convex-Min-Timestamp` header for read-after-write consistency

### Step 6: Integration Testing (2-3 weeks)

- Spin up 1 Primary + 2 Replicas
- Run the existing test suite against the cluster
- Test failure scenarios: Primary crash, Replica crash, log lag, network partition
- Benchmark: measure read throughput scaling, subscription capacity, replication lag

### Total: ~10-14 weeks for a team of 2-3 engineers

---

## What This Doesn't Solve (And When You'd Need To)

| Limitation | When It Matters | Solution (Future Work) |
|-----------|----------------|----------------------|
| Write throughput is still single-node | >5000 mutations/sec sustained | Partitioned writes (the full proposal) |
| Primary is a single point of failure | Production HA requirements | Primary failover via leader election (Raft among Primary candidates) |
| All Replicas hold the full dataset in RAM | Dataset exceeds single-node RAM (>64-128 GB of index data) | Sharded Replicas that each hold a subset of tables |
| Replication lag (~1-5ms) | Ultra-low-latency read-after-write | Route reads to Primary when freshness is critical (already handled by sticky sessions) |

---

## Decision: What Distributed Log to Use

| Option | Pros | Cons | Recommendation |
|--------|------|------|----------------|
| **NATS JetStream** | Simple to operate, single binary, low latency, built-in persistence | Smaller ecosystem, less battle-tested at massive scale | Best for self-hosted / small-medium scale |
| **Redpanda** | Kafka-compatible API, single binary (no JVM/ZooKeeper), very fast | Newer, smaller community | Best overall balance |
| **Apache Kafka** | Most battle-tested, massive ecosystem, exactly-once semantics | Operationally heavy (JVM, ZooKeeper/KRaft), higher latency | Best for large-scale cloud deployments |
| **FoundationDB** | Used by Apple at massive scale, strong ordering guarantees | Steep learning curve, niche community | Best if already using FDB for persistence |

**My recommendation: Start with NATS JetStream** for self-hosted deployments (simple, single binary, easy to embed), with a Kafka/Redpanda implementation for cloud-scale deployments. The `DistributedLog` trait abstracts over the choice.

---

## Summary

The full partitioned-write architecture is the end-state for Convex at massive scale. But the primary-replica architecture gets **80% of the scaling benefit with 20% of the complexity**:

- Read throughput: scales linearly
- Subscription capacity: scales linearly
- Write throughput: unchanged but unburdened
- Consistency: fully preserved
- Existing code changes: minimal
- New operational dependencies: one distributed log
- Time to implement: ~3 months

This is the move.
