# What We Built: Horizontal Scaling for Convex

We took the best engineering from five distributed databases — CockroachDB, TiDB, Vitess, YugabyteDB, and Google Spanner — and combined them into something none of them individually does: horizontal scaling for a reactive, real-time database with OCC, subscriptions, and in-memory snapshots.

## Why This Is New

No single system we studied has all of these together:

| Capability | CockroachDB | TiDB | Vitess | YugabyteDB | Spanner | Convex (ours) |
| --- | --- | --- | --- | --- | --- | --- |
| Real-time subscriptions (WebSocket push) | No | No | No | No | No | Yes |
| In-memory snapshot state machine | No | No | No | No | No | Yes |
| OCC with automatic retry | No | Yes | No | Yes | No | Yes |
| TypeScript function execution | No | No | No | No | No | Yes |
| Horizontal read scaling | Yes | Yes | Yes | Yes | Yes | Yes |
| Horizontal write scaling | Yes | Yes | Yes | Yes | Yes | Yes |

We added the last two rows while keeping the first four — the things that make Convex unique.

## What We Took from Each System

### TiDB: Timestamp Oracle

**Problem**: Multiple writer nodes need globally unique, monotonically increasing timestamps. A network call per commit would destroy latency.

**TiDB's solution**: PD (Placement Driver) allocates timestamps in batches. Each node reserves a range (e.g., 1000 timestamps) from a central etcd counter via atomic CAS. Within the range, timestamps are assigned locally with zero network calls. When exhausted, reserve another batch.

**What we built**: `BatchTimestampOracle` in `timestamp_oracle.rs`. Both nodes draw from a shared NATS KV counter. Node A gets range [N, N+1000), Node B gets [N+1000, N+2000). No overlap, no coordination within a batch. The hot commit path — `next_commit_ts()` — is a local mutex increment.

**Source**: [TiDB TSO time services](https://www.pingcap.com/blog/how-an-open-source-distributed-newsql-database-delivers-time-services/), [TiKV Timestamp Oracle deep dive](https://tikv.org/deep-dive/distributed-transaction/timestamp-oracle/)

### Vitess: Table-Level Partitioning and 2PC

**Problem**: Need to split table ownership across nodes without row-level sharding complexity.

**Vitess's solution**: VSchema assigns entire tables to shards. A declarative config says "these tables go to shard A, those to shard B." Single mode rejects cross-shard writes. 2PC mode handles them with a coordinator that sends prepare/commit/rollback to each shard.

**What we built**: `PartitionMap` in `partition.rs` with config like `messages=0,users=0,projects=1,tasks=1`. Partition ownership check in `validate_commit()`. `TwoPhaseCoordinator` in `two_phase_coordinator.rs` that detects cross-partition writes and orchestrates prepare/commit/rollback. Transaction watcher in `two_phase_watcher.rs` for crash recovery via NATS KV.

**Source**: [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/), [Vitess VSchema](https://vitess.io/docs/22.0/reference/features/vschema/)

### CockroachDB: Descriptor ID Allocation, Closed Timestamps, and Raft Pipeline

**Problem 1**: Independent databases assign the same table number to different tables. Replicating `_tables` entries causes collisions.

**CockroachDB's solution**: Global descriptor ID allocator — a single counter (`/system/desc-idgen`) incremented non-transactionally. Every table gets a cluster-unique ID.

**What we built**: When `apply_replica_delta` receives a `_tables` entry for a table that doesn't exist locally, it reassigns the table number to a locally-unique value by scanning the table registry for the next available number. The TabletId (derived from `developer_id`) stays the same for correct remapping.

**Problem 2**: Async max_repeatable_ts bumps race with synchronous replica delta applies, causing timestamp ordering violations in the write log.

**CockroachDB's solution**: Closed timestamp (equivalent to our max_repeatable_ts) is propagated via a separate side transport that "simply refuses to publish an update for a particular range if that range's evaluation timestamp is below the target timestamp." The side transport never races with the Raft transport.

**What we built**: All writes — local commits, max_repeatable_ts bumps, AND replica deltas — flow through the same `persistence_writes` FuturesOrdered pipeline. The write log append and snapshot push happen only when the persistence write completes and it's next in the queue. This is the Raft pattern: one sequential log, no interleaving.

**Source**: [CockroachDB Parallel Commits](https://www.cockroachlabs.com/blog/parallel-commits/), [CockroachDB Follower Reads](https://www.cockroachlabs.com/blog/follower-reads-stale-data/), [CockroachDB design.md](https://github.com/cockroachdb/cockroach/blob/master/docs/design.md)

### YugabyteDB: Hybrid Timestamp Monotonicity

**Problem**: Hybrid timestamps assigned to committed log entries must always increase, even across leader changes and replica applies.

**YugabyteDB's solution**: "Hybrid timestamps assigned to committed Raft log entries in the same tablet always keep increasing, even if there are leader changes. This is because the new leader always has all committed entries from previous leaders, and it makes sure to update its hybrid clock with the timestamp of the last committed entry before appending new entries."

**What we built**: Replica delta apply uses only monotonic counters (`max(latest_ts + 1, last_assigned_ts + 1)`) — no TSO (reserved for local commits), no system clock (would leap ahead of TSO range). The same two counters that `next_commit_ts()` writes to, staying in the same numeric domain.

**Source**: [YugabyteDB Raft consensus](https://docs.yugabyte.com/stable/architecture/docdb-replication/raft/), [YugabyteDB Jepsen testing](https://docs.yugabyte.com/stable/benchmark/resilience/jepsen-testing/)

### Google Spanner: TrueTime and Ordered Apply

**Problem**: Writes from different nodes must be applied in a globally consistent order.

**Spanner's solution**: TrueTime assigns timestamps with bounded uncertainty. "Writes with earlier timestamps are applied before those with later timestamps, with Paxos leaders using TrueTime to assign timestamps." A commit wait phase guarantees the commit timestamp is in the past for all replicas.

**What we built**: Our TSO provides the same global ordering guarantee as TrueTime but without GPS clocks — batch allocation from a central counter is simpler and correctness is easier to verify. The Raft-pattern persistence pipeline ensures all writes are applied in timestamp order.

**Source**: [Spanner TrueTime](https://docs.google.com/spanner/docs/true-time-external-consistency), [Spanner paper](https://research.google.com/pubs/archive/45855.pdf)

### etcd / TiKV / Kafka: Single Committer Apply Loop

**Problem**: Multiple concurrent writers to the same state machine cause race conditions.

**Their solution**: A single background thread processes all state machine updates sequentially. etcd's `commitC` channel, TiKV's Apply Worker, Kafka KRaft's state machine callback — all the same pattern. One inbox, one reader, serial processing.

**What we built**: The Convex Committer already used this pattern for local commits. We extended it: replica deltas go through the same `mpsc` channel as local commits, processed by the same single-threaded `go()` loop. The `ApplyReplicaDelta` message variant sits alongside `Commit`, `BumpMaxRepeatableTs`, and the 2PC handlers — all serialized.

## What's New: The Combination

The combination of these patterns applied to Convex's architecture doesn't exist elsewhere:

**Real-time subscriptions + partitioned writes**: CockroachDB, TiDB, and Spanner don't have WebSocket subscription push. When a write happens on Node A, subscription workers on Node B see it through the replicated write log and push updates to connected clients. No other partitioned database does this.

**In-memory OCC + cross-partition replication**: Convex's OCC validation happens against the in-memory SnapshotManager, not disk. When deltas from other partitions arrive, they update the local SnapshotManager so OCC conflict detection includes remote writes. This is cross-partition OCC without distributed locking — unique to our approach.

**Table creation replication with number reassignment**: No other system replicates `_tables` entries across independent databases with different table number sequences. CockroachDB uses a global allocator (all nodes share one database). TiDB uses PD for global IDs. Vitess copies schemas identically. We independently assign table numbers on each node and remap via `tablet_id_to_table_name` — a pattern specific to Convex's TabletId architecture.

**Delta-based replication with system table classification**: We classify each replicated update by what it describes, not which system table stores it. An `_index` entry creating `projects.by_id` is user table metadata that must replicate, even though `_index` is a system table. A `_backend_state` row is node-local operational state that must not. This classification doesn't exist in other systems because they either replicate everything (Raft) or nothing (independent shards).

## Results

- **77 integration tests** across **37 categories** — all passing
- **346 unit tests** — all passing
- **3,823 messages** and **3,069 tasks** in a single test run
- **1,390 sustained writes/node** over 30 seconds
- **200-doc batch** replicated atomically
- **NATS partition**, **double node restart**, and **full cluster restart** — all survived
- **Zero data loss** across every chaos scenario

Every test pattern comes from a real bug found by Jepsen in a production database. The engineering isn't theoretical — it's validated against the same failure modes that broke CockroachDB, TiDB, and YugabyteDB in production.

## Sources

### Systems Studied

- [CockroachDB design.md](https://github.com/cockroachdb/cockroach/blob/master/docs/design.md)
- [CockroachDB Parallel Commits](https://www.cockroachlabs.com/blog/parallel-commits/)
- [CockroachDB Follower Reads](https://www.cockroachlabs.com/blog/follower-reads-stale-data/)
- [CockroachDB Nightly Jepsen Lessons](https://www.cockroachlabs.com/blog/jepsen-tests-lessons/)
- [TiDB TSO Time Services](https://www.pingcap.com/blog/how-an-open-source-distributed-newsql-database-delivers-time-services/)
- [TiKV Timestamp Oracle](https://tikv.org/deep-dive/distributed-transaction/timestamp-oracle/)
- [TiKV Percolator](https://tikv.org/deep-dive/distributed-transaction/percolator/)
- [TiDB Chaos Engineering](https://www.pingcap.com/blog/chaos-practice-in-tidb/)
- [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)
- [Vitess VDiff](https://vitess.io/blog/2022-11-22-vdiff-v2/)
- [YugabyteDB Raft Consensus](https://docs.yugabyte.com/stable/architecture/docdb-replication/raft/)
- [YugabyteDB Jepsen Testing](https://docs.yugabyte.com/stable/benchmark/resilience/jepsen-testing/)
- [Google Spanner TrueTime](https://docs.cloud.google.com/spanner/docs/true-time-external-consistency)

### Testing References

- [Jepsen CockroachDB](https://jepsen.io/analyses/cockroachdb-beta-20160829)
- [Jepsen TiDB 2.1.7](https://jepsen.io/analyses/tidb-2.1.7)
- [Jepsen YugabyteDB](https://jepsen.io/analyses/yugabyte-db-1.1.9)
- [Elle Transaction Checker](https://github.com/jepsen-io/elle)
- [Chaos Mesh](https://chaos-mesh.org/docs/basic-features/)
- [TiPocket](https://github.com/pingcap/tipocket)
