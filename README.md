# Horizontal Scaling for Convex

The first horizontal scaling implementation for the [Convex open-source backend](https://github.com/get-convex/convex-backend) — reads, writes, and automatic failover.

Convex is a reactive database: real-time subscriptions, in-memory snapshots, OCC with automatic retry, TypeScript function execution. No distributed database — CockroachDB, TiDB, Vitess, YugabyteDB, or Spanner — has all of these. We made it scale horizontally without losing any of them.

## The Problem

Convex is a stateful system where the backend process holds the live database in memory. Two instances don't share state — they diverge. The [6 architectural bottlenecks](docs/why-convex-cannot-scale-horizontally.md) that prevent scaling: single Committer, in-memory SnapshotManager, in-memory WriteLog, single-process subscriptions, single-node OCC, and no distributed consensus. Convex's docs explicitly state self-hosted is single-node by design.

## The Solution

We took the best engineering from five distributed databases and combined them:

| Problem | Pattern | Source |
| --- | --- | --- |
| Global timestamp ordering | Batch TSO via NATS KV — zero network calls in hot path | TiDB PD |
| Table ownership | VSchema-style partition map — each node owns specific tables | Vitess |
| Single writer per partition | Committer apply loop — one thread, one channel, serial processing | etcd, TiKV, Kafka |
| Cross-partition writes | 2PC coordinator with prepare/commit/rollback | Vitess |
| Table number conflicts | Descriptor ID reassignment on receiving node | CockroachDB |
| Async commit ordering | All writes through single FuturesOrdered pipeline | CockroachDB Raft, TiKV apply worker |
| Replica timestamp isolation | No TSO or system clock on apply — monotonic counters only | CockroachDB closed timestamp, TiDB resolved-ts |
| Delta replication | NATS JetStream with durable consumers and self-delta skip | All five systems |
| System table classification | Classify by what data describes, not which table stores it | CockroachDB system ranges |
| Automatic leader failover | tikv/raft-rs consensus per partition — sub-second leader election | TiKV, etcd |
| Raft transport | gRPC with batched messages and exponential backoff retry | TiKV RaftClient |
| Leadership lifecycle | Committer starts on election, stops on demotion via SoftState | TiKV, CockroachDB |
| Deployment state replication | GLOBAL table locality — `_modules`, `_udf_config`, `_source_packages` replicate to all nodes | CockroachDB GLOBAL tables, YugabyteDB system catalog |

The combination — real-time subscriptions + in-memory OCC + partitioned multi-writer + delta replication + 2PC — doesn't exist in any of those systems. CockroachDB doesn't have subscriptions. TiDB doesn't have in-memory snapshots. Vitess doesn't have OCC. We kept Convex's unique architecture and grafted distributed database patterns onto it.

Full details: [docs/what-we-built.md](docs/what-we-built.md)

## Results

### Raft Failover Tests

```
ALL 10 TESTS PASSED

Test 1: All 3 Nodes Healthy
  PASS  Node A (port 3210) healthy
  PASS  Node B (port 3220) healthy
  PASS  Node C (port 3230) healthy

Test 2: Write to Leader
  PASS  Write to Node A succeeded

Test 3: Read from All Nodes
  PASS  Node A sees data: 1 messages
  PASS  All 3 nodes agree: 1 messages

Test 4: Kill Leader, Verify Failover
  PASS  Failover: writes accepted on http://127.0.0.1:3220 after leader kill
  PASS  Data written after failover: B=2 C=2 (pre-kill=1)

Test 5: Restart Killed Node, Verify Rejoin
  PASS  Node A recovered: sees 2 messages (>=1)
  PASS  All nodes converged after rejoin: 2 messages
```

Based on CockroachDB roachtest failover/non-system/crash, TiKV fail-rs chaos testing, and YugabyteDB Jepsen nightly resilience benchmarks.

### Write Scaling Tests

```
ALL 77 TESTS PASSED — 3,823 messages | 3,069 tasks | 1,390 sustained writes/node

 1. Cross-partition data verification     (Vitess VDiff)         — PASS
 2. Bank invariant — single table         (CockroachDB Jepsen)   — PASS
 3. Bank invariant — multi-table          (TiDB bank-multitable) — PASS
 4. Partition enforcement (5 subtests)    (Vitess Single mode)   — PASS
 5. Concurrent write scaling              (CockroachDB KV)       — PASS 176 writes/sec
 6. Monotonic reads                       (TiDB monotonic)       — PASS
 7. Node restart recovery                 (TiDB kill -9)         — PASS
 8. Idempotent re-run                     (CockroachDB workload) — PASS
 9. Two-phase commit cross-partition      (Vitess 2PC)           — PASS
10. Rapid-fire writes 50/node             (Jepsen stress)        — PASS
11. Write-then-immediate-read             (stale read detection) — PASS
12. Double node restart                   (CockroachDB nemesis)  — PASS
13. Post-chaos invariant check            (workload check)       — PASS
14. Sequential ordering                   (Jepsen sequential)    — PASS
15. Set completeness (100 elements)       (Jepsen set)           — PASS
16. Concurrent counter                    (Jepsen counter)       — PASS
17. Write-then-cross-node-read            (cross-node stale)     — PASS
18. Interleaved cross-partition reads     (read skew detection)  — PASS
19. Large batch write (50 docs)           (atomicity)            — PASS
20. Full cluster restart                  (CockroachDB nemesis)  — PASS
21. Sustained writes 30 seconds           (endurance)            — PASS
22. Duplicate insert idempotency          (correctness)          — PASS
23. Mid-suite exhaustive invariant check  (workload check)       — PASS
24. Single-key register                   (CockroachDB register) — PASS
25. Disjoint record ordering              (CockroachDB comments) — PASS
26. NATS partition simulation             (Chaos Mesh)           — PASS
27. Write during deploy                   (deploy safety)        — PASS
28. Empty table cross-node query          (boundary)             — PASS
29. Max batch size 200 docs               (boundary)             — PASS
30. Null and empty field values           (boundary)             — PASS
31. Concurrent writes from both nodes     (race condition)       — PASS
32. Rapid deploy cycle 3x                 (deploy stability)     — PASS
33. Read during active replication        (consistency)          — PASS
34. TSO monotonicity after restart        (TiDB TSO)             — PASS
35. Single document read-modify-write     (register)             — PASS
36. Write skew detection                  (G2 anomaly)           — PASS
37. Ultimate final invariant check        (workload check)       — PASS
```

Every test pattern comes from a real bug found by Jepsen in a production database.

## Architecture

### Raft Consensus (Automatic Failover)

```
Partition 0 — 3-node Raft group (tikv/raft-rs):

  Node A (leader)  ────Raft────▶ Node B (follower)
        │          ────Raft────▶ Node C (follower)
        │
        ▼
   Committer active         Committers dormant
   Accepts writes           Reject writes (redirect)
        │
        ├── NATS delta ──▶ Node B applies replica delta
        └── NATS delta ──▶ Node C applies replica delta

  Node A dies:
    Node B elected leader (~1s) → Committer activates → accepts writes
    Node C remains follower → applies deltas from Node B
    Node A restarts → rejoins as follower → converges via NATS
```

Each partition is a 3-node Raft group. The leader runs the Committer. If the leader dies, followers elect a new leader within ~1 second and the Committer activates automatically. Zero manual intervention, zero data loss. Deployment state (`_modules`, `_udf_config`, `_source_packages`) replicates to all nodes via the CockroachDB GLOBAL table locality pattern so every node can serve queries.

### Write Scaling (Partitioned Multi-Writer)

```
                    ┌──────────────────────────────┐
                    │   Global Timestamp Oracle     │
                    │   (NATS KV, TiDB PD pattern)  │
                    └──────────┬───────────────────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
        ┌─────┴──────┐        │        ┌───────┴─────┐
        │  Node A    │        │        │   Node B    │
        │  partition 0│        │        │  partition 1│
        │  messages  │        │        │  projects   │
        │  users     │        │        │  tasks      │
        └─────┬──────┘        │        └───────┬─────┘
              │        ┌──────┴──────┐         │
              └───────▶│    NATS     │◀────────┘
                       │  JetStream  │
                       └──────┬──────┘
                              │
              ┌───────────────┼───────────────┐
              ▼                               ▼
         Node A sees                     Node B sees
         all data                        all data
```

Each node owns specific tables and is the single Committer for those tables. Both nodes consume all NATS deltas for a complete read view. Writes to non-owned tables are rejected with partition routing info. Cross-partition writes go through 2PC.

### Read Scaling (Primary-Replica)

```
Client ──mutation──▶ Primary ──persist──▶ PostgreSQL
                        │
                        ├──publish delta──▶ NATS JetStream
                        │                        │
Client ──query──▶ Replica ◀──consume delta───────┘
```

One Primary handles writes, multiple Replicas serve reads. Replicas remap TabletIds from the Primary's namespace to their own using the `tablet_id_to_table_name` mapping in each CommitDelta.

## Quick Start

One Docker Compose file, three deployment modes via profiles. Raft consensus is always on — same as CockroachDB, etcd, and YugabyteDB (single-node is a Raft group of 1).

### Run

```sh
cd self-hosted/docker

# 1 Raft node (dev/test)
docker compose --profile single up

# 3 Raft nodes, 1 partition (read scaling + HA)
docker compose --profile replicated up

# 2 partitions × 3 Raft nodes (write scaling + HA)
docker compose --profile partitioned up
```

Images are published to `ghcr.io/martinkalema/convex-horizontal-scaling` — no local build needed.

### Test

```sh
cd self-hosted/docker

# Raft failover tests (10 tests — election, failover, rejoin)
./test-raft-failover.sh

# Write scaling tests (77 tests — Jepsen workloads, 2PC, chaos)
./test-write-scaling.sh
```

### Deploy Functions

```sh
docker compose --profile replicated exec node-1a ./generate_admin_key.sh
npx convex deploy --url http://127.0.0.1:3210 --admin-key <KEY>
```

### Ports (Raft Mode)

| Service | Port |
| --- | --- |
| Node A API | 3210 |
| Node B API | 3220 |
| Node C API | 3230 |
| Node A gRPC (Raft + 2PC) | 50051 |
| Node B gRPC (Raft + 2PC) | 50052 |
| Node C gRPC (Raft + 2PC) | 50053 |
| Dashboard | 6791 |
| PostgreSQL | 5433 |
| NATS | 4222 |

## Key Components

| Component | File | What it does |
| --- | --- | --- |
| RaftNode | `raft_node.rs` | Raft loop: tick, receive, propose, process Ready, advance. Leadership callbacks via SoftState |
| RaftPartitionManager | `raft_partition.rs` | Wraps RaftNode, activates/deactivates Committer on leader election/demotion |
| RaftTransport | `raft_transport.rs` | gRPC transport with batched messages, exponential backoff retry (TiKV RaftClient pattern) |
| RaftStorage | `raft_storage.rs` | MemStorage wrapper with partition awareness for raft-rs |
| RaftStateMachine | `raft_state_machine.rs` | Serialization format for Raft log entries, bridges committed entries to Committer |
| CommitDelta | `commit_delta.rs` | Captures everything changed in a transaction — documents, indexes, table mappings |
| NatsDistributedLog | `nats_distributed_log.rs` | Publishes/subscribes deltas via NATS JetStream with per-partition subjects and self-delta skip |
| apply_replica_delta | `committer.rs` | Classifies updates as GLOBAL or node-local, creates tables with reassigned numbers, applies through Raft-pattern pipeline |
| BatchTimestampOracle | `timestamp_oracle.rs` | Reserves timestamp ranges from NATS KV via atomic CAS. Zero network calls in hot path |
| PartitionMap | `partition.rs` | Table-to-partition assignment. System tables always on partition 0 |
| TwoPhaseCoordinator | `two_phase_coordinator.rs` | Detects cross-partition writes, orchestrates prepare/commit/rollback |
| TwoPhaseCommitService | `two_phase_service.rs` | gRPC Prepare/CommitPrepared/RollbackPrepared for remote partitions |
| Transaction Watcher | `two_phase_watcher.rs` | Scans NATS KV for stuck 2PC transactions, resolves via commit or rollback |

## Tests

**346 unit tests** + **87 integration tests** across **42 categories**.

### Raft Failover (10 tests, 5 categories)

Leader election, write to leader, read from all nodes, kill leader + verify failover, restart killed node + verify rejoin. Based on CockroachDB roachtest failover/non-system/crash, TiKV fail-rs, and YugabyteDB Jepsen resilience benchmarks.

### Write Scaling (77 tests, 37 categories)

Every test pattern from CockroachDB's 7 nightly Jepsen workloads (bank, register, sequential, set, monotonic, G2, comments), TiDB's Jepsen suite (bank-multitable, monotonic, stale read), YugabyteDB's Jepsen tests (counter, linearizable set), Vitess (VDiff, partition enforcement, 2PC), CockroachDB roachtest (KV scaling, nemesis, workload check), Chaos Mesh (NATS partition), Elle anomaly classes (read skew, write skew), and boundary testing (empty tables, null fields, 200-doc batch).

Full test details: [docs/write-scaling-tests.md](docs/write-scaling-tests.md)

## Documentation

| Document | Contents |
| --- | --- |
| [Raft Integration](docs/raft-integration.md) | tikv/raft-rs integration design — Raft loop, storage, transport, state machine, leader lifecycle |
| [What We Built](docs/what-we-built.md) | What we took from each distributed database and what's new |
| [Write Scaling Research](docs/write-scaling-research.md) | Vitess, TiDB, CockroachDB comparison |
| [Two-Phase Commit Design](docs/two-phase-commit.md) | 2PC architecture with Vitess/TiDB/CockroachDB patterns |
| [TSO Timestamp Fix](docs/tso-replica-timestamp-fix.md) | Three timestamp ordering fixes with distributed database research |
| [Write Scaling Tests](docs/write-scaling-tests.md) | All 37 test categories with 77 assertions |
| [Engineering Changes](docs/engineering-changes.md) | Every file changed, every architectural decision |
| [Architecture Analysis](docs/why-convex-cannot-scale-horizontally.md) | The 6 bottlenecks in the original codebase |
| [Convex Internals](docs/convex-internals-explained.md) | How the Committer, SnapshotManager, WriteLog, Subscriptions, and OCC work |
| [Implementation Plan](docs/actual-implementation-plan.md) | Primary-Replica architecture design |
| [Full Scaling Proposal](docs/horizontal-scaling-proposal.md) | Complete partitioned-write architecture proposal |
| [Scalability Research](docs/convex-scalability-research.md) | Community research with 25+ source URLs |

## Configuration

| Variable | Description | Example |
| --- | --- | --- |
| `RAFT_NODE_ID` | This node's Raft ID (1-based) | `1` |
| `RAFT_PEERS` | All Raft peers with gRPC addresses | `1=http://node-a:50051,2=http://node-b:50051,3=http://node-c:50051` |
| `PARTITION_ID` | This node's partition number | `0` |
| `PARTITION_MAP` | Table-to-partition assignment | `messages=0,users=0,projects=1,tasks=1` |
| `NUM_PARTITIONS` | Total partitions in cluster | `2` |
| `NATS_URL` | NATS JetStream connection | `nats://nats:4222` |
| `NODE_ADDRESSES` | gRPC addresses for 2PC | `0=node-a:50051,1=node-b:50051` |
| `INSTANCE_NAME` | Unique node identifier | `convex-node-a` |
| `REPLICATION_MODE` | Node role | `primary` |

## License

The original Convex backend is licensed under [FSL-1.1-Apache-2.0](LICENSE.md). Our modifications follow the same license.
