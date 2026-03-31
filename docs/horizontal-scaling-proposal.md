# Engineering Proposal: Horizontally Scaling Convex

**Author:** Systems Architecture  
**Date:** March 31, 2026  
**Status:** Proposal  

---

## Executive Summary

Convex today is a single-node stateful system. It trades horizontal scalability for strong guarantees: serializability, sub-millisecond real-time reactivity, and automatic OCC-based conflict resolution. This proposal describes how to make Convex horizontally scalable **without sacrificing those guarantees**.

The core insight: Convex has already proven that **function execution** can be separated and scaled (Funrun). We extend that pattern to the remaining stateful components — the Committer, the Snapshot Manager, the Subscription system, and the Write Log — using a combination of log-based architecture, deterministic partitioning, and distributed subscription fanout.

This is not a weekend project. This is a distributed database design. But the existing codebase has clean abstractions that make it tractable.

---

## Table of Contents

1. [Current Architecture: What We're Working With](#1-current-architecture)
2. [The Five Bottlenecks](#2-the-five-bottlenecks)
3. [Design Principles](#3-design-principles)
4. [Proposed Architecture: Overview](#4-proposed-architecture-overview)
5. [Component 1: Distributed Commit Log (Replacing Single Committer)](#5-component-1-distributed-commit-log)
6. [Component 2: Partitioned Snapshot Managers](#6-component-2-partitioned-snapshot-managers)
7. [Component 3: Distributed Subscription Fanout](#7-component-3-distributed-subscription-fanout)
8. [Component 4: Cross-Node OCC Validation](#8-component-4-cross-node-occ-validation)
9. [Component 5: Persistence Layer Scaling](#9-component-5-persistence-layer-scaling)
10. [Phased Rollout Plan](#10-phased-rollout-plan)
11. [What We Explicitly Do NOT Change](#11-what-we-explicitly-do-not-change)
12. [Trade-offs and Risks](#12-trade-offs-and-risks)
13. [Appendix: Key Code Interfaces](#13-appendix-key-code-interfaces)

---

## 1. Current Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │              Convex Backend (Single Node)    │
                    │                                             │
  Client ──WS──────▶  SubscriptionsClient ◄── LogReader          │
                    │        │                     ▲              │
                    │        │                     │              │
  Funrun ──gRPC────▶  Database.commit() ──▶  Committer ──▶ LogWriter
                    │        │                     │              │
                    │        ▼                     ▼              │
                    │  SnapshotManager        Persistence         │
                    │  (in-memory truth)     (SQLite/PG/MySQL)    │
                    └─────────────────────────────────────────────┘
```

### What's Already Horizontally Scaled
- **Funrun** (function execution): Separate gRPC service, scaled independently. Uses Rendezvous hashing for cache affinity. Returns `FunctionFinalTransaction` to the backend for commit.
- **SubscriptionManagers**: Already run as multiple workers within a single process (`NUM_SUBSCRIPTION_MANAGERS`), round-robin load balanced. But they all read from the same in-process `LogReader`.

### What Cannot Scale Today
- **Committer**: Single `mpsc::Receiver<CommitterMessage>` loop. All commits globally serialized.
- **SnapshotManager**: In-memory `VecDeque<(Timestamp, Snapshot)>` with copy-on-write `imbl::OrdMap`. Single process owns it.
- **Write Log**: In-memory `WriteLogManager` behind `Arc<Mutex<>>`. Connects commits to subscriptions within a single process.
- **Persistence writes**: Single writer assumed. `ConflictStrategy` enum controls upsert vs error on conflict.

---

## 2. The Five Bottlenecks

| # | Bottleneck | Why It Blocks Horizontal Scaling | Code Location |
|---|-----------|----------------------------------|---------------|
| 1 | **Single Committer** | All transactions serialize through one `mpsc` channel. Assigns monotonic timestamps. | `committer.rs:247-416` |
| 2 | **In-Memory Snapshots** | Ground truth (table registry, indexes, schemas) lives in one process's RAM. | `snapshot_manager.rs:88-93` |
| 3 | **In-Memory Write Log** | Connects commits to subscription invalidation. Single `Arc<Mutex<WriteLogManager>>`. | `write_log.rs:386-596` |
| 4 | **Single-Process Subscriptions** | Subscription workers read from local LogReader. Can't see commits from other nodes. | `subscription.rs:292-296` |
| 5 | **OCC Validation** | `LogWriter::is_stale()` checks read sets against local write log. No cross-node visibility. | `write_log.rs:571-596` |

---

## 3. Design Principles

1. **Don't break serializability.** This is Convex's core value proposition. Every design choice must preserve it.

2. **Follow Convex's own pattern.** Funrun proved that separating a component behind a trait boundary + gRPC works. We extend this to the remaining components.

3. **Partition where possible, coordinate where necessary.** Not everything needs global coordination. Table-level or deployment-level partitioning can eliminate most cross-node traffic.

4. **Incremental adoption.** Each phase must be deployable independently. No big-bang rewrite.

5. **Respect existing abstractions.** The `Persistence` trait, `FunctionRunner` trait, `CommitterClient`, and `SubscriptionsClient` are already clean boundaries. We add new implementations behind these traits, not new interfaces.

---

## 4. Proposed Architecture: Overview

```
                         ┌──────────────────────────────┐
                         │     Distributed Commit Log    │
                         │  (e.g., Kafka / FoundationDB) │
                         │                              │
                         │  Partition 0  Partition 1 .. │
                         └──────┬───────────┬───────────┘
                                │           │
                    ┌───────────┴──┐  ┌─────┴──────────┐
                    │  Backend A   │  │   Backend B     │
                    │              │  │                 │
                    │ Committer    │  │  Committer      │
                    │ (partition 0)│  │  (partition 1)  │
                    │              │  │                 │
                    │ Snapshot Mgr │  │  Snapshot Mgr   │
                    │ (partition 0)│  │  (partition 1)  │
                    │              │  │                 │
                    │ Subscriptions│  │  Subscriptions  │
                    │              │  │                 │
                    │ Log Consumer │  │  Log Consumer   │
                    │ (ALL parts)  │  │  (ALL parts)    │
                    └──────────────┘  └────────────────┘
                           ▲                  ▲
                           │                  │
                    ┌──────┴──────────────────┴──────┐
                    │          Funrun Pool           │
                    │    (already horizontally       │
                    │     scaled, unchanged)         │
                    └───────────────────────────────┘
                           ▲                  ▲
                    ┌──────┴──────────────────┴──────┐
                    │       Client Connections        │
                    │    (load balanced by table      │
                    │     or deployment partition)    │
                    └───────────────────────────────┘
```

### The Key Idea: Table-Level Partitioning

Instead of trying to make a single global Committer distributed (which would require Raft/Paxos for every commit), we **partition ownership by table**. Each backend node owns a set of tables and is the single Committer for those tables. This preserves the single-writer property within each partition while distributing load across nodes.

Cross-partition transactions (mutations touching tables on different nodes) require a two-phase commit protocol — but in practice, most Convex mutations touch 1-3 tables owned by the same deployment, making single-partition commits the common case.

---

## 5. Component 1: Distributed Commit Log

### Problem
The Committer writes to persistence and the write log in a single process. Other nodes can't see these writes.

### Solution: Replace the In-Memory Write Log with a Distributed Log

**Technology choice:** Apache Kafka, Redpanda, or FoundationDB's log abstraction.

**Why a log, not a database?** The write log is already an append-only ordered sequence of `(Timestamp, OrderedIndexKeyWrites, WriteSource)` entries. A distributed log is the natural external equivalent.

### How It Works

1. When the Committer on Node A commits a transaction at timestamp `ts=100`:
   - Writes to persistence (unchanged)
   - Publishes the write set to the distributed log on the appropriate partition
   - Updates its local SnapshotManager (unchanged)

2. Node B's **Log Consumer** tails the distributed log:
   - Sees the write from Node A
   - Applies it to Node B's local SnapshotManager (for cross-partition reads)
   - Feeds it into Node B's SubscriptionManagers (for cross-partition subscription invalidation)

### Interface Change

```rust
// Current: in-process write log
pub fn new_write_log(initial_timestamp: Timestamp) -> (LogOwner, LogReader, LogWriter)

// Proposed: distributed write log with the same LogReader interface
pub fn new_distributed_write_log(
    config: DistributedLogConfig,
    partition_id: PartitionId,
    initial_timestamp: Timestamp,
) -> (LogOwner, LogReader, DistributedLogWriter)
```

The `LogReader` interface stays identical. Subscription workers don't change. They still call `log.wait_for_higher_ts()` and `log.for_each()`. The only difference is that the underlying data now comes from both local commits AND remote commits consumed from the distributed log.

### Ordering Guarantee

Each partition has a single Committer, so writes within a partition are totally ordered. Cross-partition ordering uses the distributed log's partition-level ordering plus a global timestamp oracle (see Component 4).

---

## 6. Component 2: Partitioned Snapshot Managers

### Problem
The SnapshotManager holds all table metadata, index metadata, and in-memory indexes for the entire deployment in one process.

### Solution: Partition Ownership + Read Replicas

Each node is the **owner** of a subset of tables (the tables it commits for). For those tables, it maintains the authoritative SnapshotManager — identical to today.

For tables owned by other nodes, each node maintains a **read replica** of the snapshot, fed by the distributed log consumer.

```
Node A (owns tables: messages, users)
├── SnapshotManager (authoritative for messages, users)
├── SnapshotReplica (read-only for channels, reactions)  ◄── from distributed log
└── Combined view for cross-table queries

Node B (owns tables: channels, reactions)
├── SnapshotManager (authoritative for channels, reactions)
├── SnapshotReplica (read-only for messages, users)  ◄── from distributed log
└── Combined view for cross-table queries
```

### Staleness and Consistency

Read replicas may lag behind the authoritative copy by the distributed log's propagation delay (typically single-digit milliseconds with Kafka/Redpanda in the same region).

For **queries** (read-only): This is acceptable. Convex already reads at a `RepeatableTimestamp` that may be slightly behind the latest commit. We simply don't advance the readable timestamp on a replica until the replica has consumed up to that point.

For **mutations**: The Committer for the owning partition always uses the authoritative snapshot, so writes are never against stale data.

For **cross-partition reads within a mutation**: The transaction's read set records what it read and at what timestamp. At commit time, the Committer validates freshness (see Component 4).

### Interface Change

```rust
// The existing SnapshotManager is unchanged for owned tables.
// We add a new composite snapshot that merges owned + replicated:
pub struct CompositeSnapshotManager {
    owned: Writer<SnapshotManager>,      // Authoritative, writable
    replicas: HashMap<PartitionId, Reader<SnapshotManager>>,  // Read-only
}

impl CompositeSnapshotManager {
    pub fn snapshot_at(&self, ts: Timestamp) -> CompositeSnapshot {
        // Merges owned snapshot + replica snapshots at `ts`
        // Only returns a snapshot if all replicas have advanced past `ts`
    }
}
```

---

## 7. Component 3: Distributed Subscription Fanout

### Problem
Subscription workers only see writes from the local Committer via the in-process LogReader.

### Solution: Every Node Consumes the Full Distributed Log

This is the simplest component because the existing architecture already supports it:

1. Each node runs its own `SubscriptionsClient` with multiple `SubscriptionManager` workers (already the case today).
2. Each node's `LogReader` is backed by the distributed log consumer, which tails **all partitions** — not just the local node's partition.
3. When a client connects via WebSocket to Node A and subscribes to a query that reads from tables on both Node A and Node B, the subscription works correctly because Node A's LogReader sees writes from both partitions.

### No Interface Changes Required

The `SubscriptionsClient::subscribe()` method, `SubscriptionRequest`, and `Subscription` types are completely unchanged. The `LogReader::wait_for_higher_ts()` and `LogReader::for_each()` methods work identically — they just have more data flowing through them.

### Client Connection Routing

For efficiency, clients should be routed to the node that owns most of the tables their queries read from. This minimizes the cross-partition subscription invalidation path. Use consistent hashing on the primary table in the client's query set.

---

## 8. Component 4: Cross-Node OCC Validation

### Problem
OCC validation currently happens in the Committer by calling `LogWriter::is_stale()`, which checks the local write log. If a transaction on Node A reads a table owned by Node B, and Node B commits a conflicting write, Node A's Committer won't see it.

### Solution: Timestamp Oracle + Cross-Partition Read Validation

#### 8a. Global Timestamp Oracle

Replace per-node monotonic timestamps with a **centralized timestamp oracle** (TSO).

```rust
#[async_trait]
pub trait TimestampOracle: Send + Sync + 'static {
    /// Get the next globally unique, monotonically increasing timestamp.
    async fn next_ts(&self) -> anyhow::Result<Timestamp>;
    
    /// Get the current max committed timestamp across all partitions.
    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp>;
}
```

**Implementation options:**
- **FoundationDB's versionstamp** (if using FDB as persistence): Built-in, globally ordered, high throughput.
- **Dedicated TSO service** (like TiDB's PD): A small Raft group that serves timestamps. ~100K timestamps/sec from a single leader, which is far more than Convex needs.
- **Hybrid Logical Clocks (HLC)**: No central service, but requires careful conflict resolution. Trades some ordering guarantees for availability.

**Recommendation:** Dedicated TSO service. It's simple, proven (TiDB serves millions of QPS with this pattern), and Convex's commit rate (hundreds to low thousands per second) is well within a single TSO's capacity.

#### 8b. Cross-Partition Read Validation

When a transaction's read set includes tables from multiple partitions, the committing node must validate against the remote partition's state.

**Option A: Eager validation via distributed log**

Before committing, the Committer checks that its local replica of the remote partition is caught up to at least the transaction's begin timestamp. If the replica shows no conflicting writes, the commit proceeds. If the replica is too far behind, the commit waits (bounded) or aborts.

```rust
// In Committer::start_commit(), after local validation:
if transaction.reads_remote_partitions() {
    for partition in transaction.remote_partitions() {
        let replica_ts = self.composite_snapshot.replica_ts(partition);
        if replica_ts < transaction.begin_ts() {
            // Wait for replica to catch up, or abort
            self.wait_for_replica(partition, transaction.begin_ts()).await?;
        }
        // Check replica's write log for conflicts
        if self.composite_snapshot.has_conflict(partition, &transaction.read_set()) {
            return Err(OccConflict);
        }
    }
}
```

**Option B: Two-phase commit for cross-partition mutations**

For mutations that write to tables on multiple partitions, use 2PC:
1. **Prepare phase**: Each involved partition's Committer validates the transaction's read set and locks the write set.
2. **Commit phase**: If all partitions acknowledge, the TSO assigns a commit timestamp and all partitions commit atomically.
3. **Abort phase**: If any partition rejects, all partitions roll back.

**Recommendation:** Option A for the common case (reads across partitions, writes to one partition). Option B only for the rare case of cross-partition writes. This keeps the common path fast.

---

## 9. Component 5: Persistence Layer Scaling

### Problem
Persistence today is single-writer (one Committer writing to one SQLite/Postgres/MySQL instance).

### Solution: Partitioned Persistence

Each partition gets its own persistence instance. The `Persistence` trait is already a clean abstraction — we just instantiate one per partition.

```rust
pub struct PartitionedPersistence {
    partitions: HashMap<PartitionId, Arc<dyn Persistence>>,
}

impl PartitionedPersistence {
    pub fn for_partition(&self, id: PartitionId) -> Arc<dyn Persistence> {
        self.partitions[&id].clone()
    }
}
```

**For PostgreSQL:** Use separate schemas or databases per partition, or a single Postgres instance with partition-aware writes (Postgres handles concurrent writers fine).

**For SQLite:** Each partition gets its own SQLite file. This is the natural scaling model for SQLite — you shard by file.

**For reads that span partitions:** The `PersistenceReader` trait supports `load_documents_from_table()` which is already table-scoped. Cross-partition reads query multiple PersistenceReaders and merge results.

---

## 10. Phased Rollout Plan

### Phase 0: Preparation (Low Risk)
**Goal:** Create the abstractions without changing behavior.

- [ ] Introduce `PartitionId` type and table-to-partition mapping
- [ ] Wrap `SnapshotManager` in a `CompositeSnapshotManager` that initially contains only one partition (the whole deployment)
- [ ] Wrap `WriteLogManager` in a `DistributedLogAdapter` that initially writes to the local in-memory log only
- [ ] Add `TimestampOracle` trait with a local implementation that delegates to the existing Committer's timestamp assignment
- [ ] **Ship it.** Behavior is identical. All tests pass.

### Phase 1: External Write Log (Medium Risk)
**Goal:** Replace the in-memory write log with an external distributed log.

- [ ] Deploy Kafka/Redpanda alongside the Convex backend
- [ ] Implement `DistributedLogWriter` that publishes to Kafka after persistence writes
- [ ] Implement `DistributedLogConsumer` that tails Kafka and feeds into the local `LogReader`
- [ ] In single-node mode, this is just a durability improvement — the write log survives backend restarts
- [ ] **Ship it.** Single node, but write log is now external.

### Phase 2: Timestamp Oracle (Medium Risk)
**Goal:** Decouple timestamp assignment from the Committer.

- [ ] Deploy a TSO service (can be a simple Raft group of 3 nodes, or FoundationDB)
- [ ] Modify Committer to request timestamps from TSO instead of incrementing locally
- [ ] All existing behavior preserved — single Committer still serializes, but timestamps come from outside
- [ ] **Ship it.** Single committer, external timestamps.

### Phase 3: Multi-Node with Partitioning (High Complexity)
**Goal:** Run multiple backend nodes, each owning a partition of tables.

- [ ] Implement partition assignment service (which node owns which tables)
- [ ] Deploy multiple backend nodes, each running a Committer for its partition
- [ ] Implement `CompositeSnapshotManager` with read replicas fed by distributed log
- [ ] Implement cross-partition OCC validation (Option A: eager via replica)
- [ ] Implement client connection routing (consistent hashing on primary table)
- [ ] **Ship it.** Multi-node, partitioned, horizontally scaled.

### Phase 4: Cross-Partition Transactions (High Complexity)
**Goal:** Support mutations that write to tables on different nodes.

- [ ] Implement 2PC coordinator for cross-partition writes
- [ ] Add transaction routing: detect which partitions a mutation touches, route to coordinator
- [ ] Implement deadlock detection / timeout for cross-partition locks
- [ ] **Ship it.** Full horizontal scaling with cross-partition write support.

### Phase 5: Dynamic Repartitioning (Operational Excellence)
**Goal:** Rebalance partitions without downtime.

- [ ] Implement partition split/merge operations
- [ ] Implement live migration: drain a table from one node, transfer ownership to another
- [ ] Implement automatic hot-spot detection and rebalancing
- [ ] **Ship it.** Self-managing, horizontally scalable Convex.

---

## 11. What We Explicitly Do NOT Change

| Component | Why It Stays |
|-----------|-------------|
| **Funrun** | Already horizontally scaled. No changes needed. |
| **OCC model** | Still optimistic, still automatic retries. Just validated across partitions now. |
| **Subscription API** | `subscribe()` returns `Subscription` with invalidation notification. Identical API. |
| **Persistence trait** | Same interface, just multiple instances. |
| **Client SDK** | Clients still connect via WebSocket, still get real-time updates. Routing is transparent. |
| **Developer-facing function model** | Queries, mutations, actions all work identically. Developers don't see partitions. |

---

## 12. Trade-offs and Risks

### What Gets Worse

| Trade-off | Impact | Mitigation |
|-----------|--------|------------|
| **Latency for cross-partition reads** | +2-5ms for reads that span partitions (distributed log propagation) | Partition tables that are commonly queried together onto the same node |
| **Cross-partition mutation cost** | 2PC adds ~10-20ms and complexity | Rare in practice; most mutations touch 1-3 related tables |
| **Operational complexity** | Kafka/TSO are new dependencies to operate | Use managed services (Confluent Cloud, FoundationDB Cloud) |
| **Tail latency** | p99 increases due to cross-node coordination | Acceptable if p50 stays under 20ms (Funrun's current baseline) |
| **Consistency window for replicas** | Read replicas may lag by single-digit ms | Bounded by `RepeatableTimestamp` — reads never see partial state |

### What Gets Better

| Benefit | Impact |
|---------|--------|
| **Write throughput** | Linear scaling with number of partitions. 4 nodes = ~4x commit throughput. |
| **Read throughput** | Each node serves reads for its partition from RAM. More nodes = more RAM = more cached data. |
| **Subscription capacity** | Each node handles subscriptions for its partition. More nodes = more concurrent subscriptions. |
| **Fault isolation** | A crash on Node A only affects tables owned by Node A. Node B continues serving. |
| **Memory capacity** | In-memory indexes distributed across nodes. No single-node memory ceiling. |

### Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| **Split-brain during partition reassignment** | Medium | High — data loss | Use distributed log as source of truth; fencing tokens on partition ownership |
| **TSO becomes SPOF** | Low (if Raft-backed) | High — all commits stall | 3-node Raft group; TSO failure = read-only mode until recovery |
| **Distributed log data loss** | Very low (if replicated) | Critical | Kafka replication factor 3; persistence layer is still the durable ground truth |
| **OCC conflict rate increases** | Low | Medium — more retries | Smarter partition assignment; co-locate hot tables |

---

## 13. Appendix: Key Code Interfaces

These are the existing interfaces we build on. None are broken; all get new implementations.

### Persistence Trait (`common/src/persistence.rs:225-296`)
```rust
#[async_trait]
pub trait Persistence: Sync + Send + 'static {
    async fn write(&self, documents: &[DocumentLogEntry], indexes: &[PersistenceIndexEntry], conflict_strategy: ConflictStrategy) -> anyhow::Result<()>;
    fn reader(&self) -> Arc<dyn PersistenceReader>;
    // ... 
}
```
**Change:** One instance per partition. No trait change.

### CommitterClient (`database/src/committer.rs:1157-1163`)
```rust
pub struct CommitterClient {
    sender: mpsc::Sender<CommitterMessage>,
    // ...
}
```
**Change:** Each node runs its own Committer for its partition. The `sender` is still local. Cross-partition commits route to the owning node via gRPC.

### SubscriptionsClient (`database/src/subscription.rs:93-98`)
```rust
pub struct SubscriptionsClient {
    senders: Vec<mpsc::Sender<SubscriptionRequest>>,
    log: LogReader,
    // ...
}
```
**Change:** `LogReader` now backed by distributed log consumer. No trait change.

### FunctionRunner Trait (`function_runner/src/lib.rs:86-150`)
```rust
#[async_trait]
pub trait FunctionRunner<RT: Runtime>: Send + Sync + 'static {
    async fn run_function(&self, ...) -> anyhow::Result<(Option<FunctionFinalTransaction>, FunctionOutcome, FunctionUsageStats)>;
}
```
**Change:** None. Already horizontally scaled via Funrun.

### Database Struct (`database/src/database.rs:280-308`)
```rust
pub struct Database<RT: Runtime> {
    committer: CommitterClient,
    subscriptions: SubscriptionsClient,
    log: LogReader,
    snapshot_manager: Reader<SnapshotManager>,
    // ...
}
```
**Change:** `snapshot_manager` becomes `CompositeSnapshotManager`. `log` backed by distributed log. `committer` may route cross-partition commits externally.

---

## Final Thought

Convex's original architects made the right call. Single-node serializability is simple, correct, and fast. The system serves the vast majority of use cases within its current limits.

But the architecture is more scalable than it appears. The trait boundaries are clean. Funrun proved the decomposition pattern works. The write log is already a logical stream. The subscription system already shards across workers.

What remains is the hardest part of distributed systems: **making multiple nodes agree on the order of events while pretending to be one node.** This proposal doesn't pretend that's easy. But it shows a concrete, phased path from where Convex is today to a horizontally scalable system — without breaking the guarantees that make Convex worth using in the first place.

The estimated effort for Phases 0-3 (basic horizontal scaling) is **6-12 months** for a team of 3-4 senior distributed systems engineers. Phase 4 (cross-partition transactions) adds another **3-6 months**. Phase 5 (dynamic repartitioning) is ongoing operational work.

The alternative — telling users to "just use a different database when you outgrow us" — is a business decision, not a technical one. The technical path exists.
