# Horizontal Scaling for Convex

The first horizontal scaling implementation for the [Convex open-source backend](https://github.com/get-convex/convex-backend) — both reads and writes.

## What This Does

Two independent Convex nodes, each owning a partition of tables, writing in parallel, replicating to each other in real-time via NATS JetStream. A global Timestamp Oracle (TiDB PD pattern) ensures ordering. Two-phase commit handles cross-partition writes.

```
ALL 56 TESTS PASSED — 2,365 messages | 1,814 tasks | 1,440 sustained writes/node

 1. Cross-partition data verification     (Vitess VDiff)         — PASS
 2. Bank invariant — single table         (CockroachDB Jepsen)   — PASS
 3. Bank invariant — multi-table          (TiDB bank-multitable) — PASS
 4. Partition enforcement (5 subtests)    (Vitess Single mode)   — PASS
 5. Concurrent write scaling              (CockroachDB KV)       — PASS 171 writes/sec
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
23. Final exhaustive invariant check      (workload check)       — PASS
```

## Architecture

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

### Read Scaling (Primary-Replica)

```
Client ──mutation──▶ Primary ──persist──▶ PostgreSQL
                        │
                        ├──publish delta──▶ NATS JetStream
                        │                        │
Client ──query──▶ Replica ◀──consume delta───────┘
```

## Quick Start

### Prerequisites

- Docker and Docker Compose
- Node.js 20+

### Build

```sh
docker build -f self-hosted/docker-build/Dockerfile.backend \
  -t convex-backend-replicated .
```

### Run Partitioned Multi-Writer (Write Scaling)

```sh
cd self-hosted/docker
docker compose -f docker-compose.partitioned.yml up
```

Two writer nodes start: Node A (port 3210, partition 0: messages, users) and Node B (port 3220, partition 1: projects, tasks). Both see all data.

### Run Primary-Replica (Read Scaling)

```sh
cd self-hosted/docker
docker compose -f docker-compose.replicated.yml up
```

One Primary (port 3210) handles writes, one Replica (port 3220) serves reads.

### Test

```sh
cd self-hosted/docker
./test-write-scaling.sh
```

Runs all 56 integration tests (23 categories) against the live partitioned deployment.

### Deploy Functions

```sh
# Generate admin key
docker compose -f docker-compose.partitioned.yml exec node-a ./generate_admin_key.sh

# Deploy
npx convex deploy --url http://127.0.0.1:3210 --admin-key <KEY>
```

### Ports

| Service | Port |
| --- | --- |
| Node A API | 3210 |
| Node B API | 3220 |
| Node A gRPC (2PC) | 50051 |
| Node B gRPC (2PC) | 50052 |
| Dashboard | 6791 |
| PostgreSQL | 5433 |
| NATS | 4222 |

## How It Works

### Core Patterns (from distributed database research)

| Pattern | Inspired by | What it does |
| --- | --- | --- |
| Table-level partitioning | Vitess VSchema | Each node owns specific tables, rejects writes to others |
| Global Timestamp Oracle | TiDB PD | Both nodes draw globally unique timestamps from shared NATS KV counter via atomic CAS |
| Single Committer per node | etcd, TiKV, Kafka | Serial apply loop preserves single-writer guarantee per partition |
| Delta replication via log | All three | NATS JetStream carries CommitDeltas between nodes with durable consumers |
| TabletId remapping | Custom | Each database has unique internal IDs; deltas carry table_id_to_table_name mapping for cross-database remapping |
| Table number reassignment | CockroachDB descriptor ID | Remote _tables entries get locally-unique table numbers to avoid collisions |
| System table classification | Custom | Updates classified by what they describe, not which system table stores them |
| Two-phase commit | Vitess 2PC | Coordinator detects cross-partition writes, orchestrates prepare/commit/rollback |
| Replica timestamp isolation | CockroachDB closed timestamp, TiDB resolved-ts | Replica delta apply uses monotonic counters only — no TSO, no system clock |

### Key Components

| Component | File | Description |
| --- | --- | --- |
| CommitDelta | `commit_delta.rs` | Captures everything changed in a transaction |
| NatsDistributedLog | `nats_distributed_log.rs` | Publish/subscribe deltas via NATS JetStream |
| apply_replica_delta | `committer.rs` | 5-phase delta apply with table creation, remapping, persistence |
| BatchTimestampOracle | `timestamp_oracle.rs` | Global TSO via NATS KV with batch allocation |
| PartitionMap | `partition.rs` | Table-to-partition assignment, ownership checking |
| TwoPhaseCoordinator | `two_phase_coordinator.rs` | Detects and orchestrates cross-partition 2PC |
| TwoPhaseCommitService | `two_phase_service.rs` | gRPC server/client for remote 2PC |
| Transaction Watcher | `two_phase_watcher.rs` | Background crash recovery for stuck 2PC transactions |

## Tests

### Unit Tests

```sh
cargo test -p database   # 346 tests
```

### Integration Tests (56 assertions across 23 categories)

```sh
cd self-hosted/docker && ./test-write-scaling.sh
```

**Correctness (Jepsen patterns):**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 1 | Cross-partition data verification | Vitess VDiff | Replication fails to propagate data |
| 2 | Bank invariant — single table | CockroachDB Jepsen bank | Numeric totals violated by replication |
| 3 | Bank invariant — multi-table | TiDB bank-multitable | Cross-table invariants broken |
| 14 | Sequential ordering | Jepsen sequential | Writes visible out of order |
| 15 | Set completeness (100 elements) | Jepsen set | Lost inserts |
| 16 | Concurrent counter | Jepsen counter / YugabyteDB | Phantom counts or lost increments |
| 22 | Duplicate insert idempotency | Custom | Deduplication or corruption |

**Partition enforcement:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 4 | Partition enforcement (5 subtests) | Vitess Single mode | Wrong-partition writes accepted |

**Scaling and performance:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 5 | Concurrent write scaling | CockroachDB KV | Data loss under parallel writes |
| 10 | Rapid-fire writes (50/node) | Jepsen stress | Crashes under burst load |
| 21 | Sustained writes (30 seconds) | CockroachDB endurance | Replication lag, data loss under sustained load |

**Consistency:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 6 | Monotonic reads | TiDB monotonic | Values going backward |
| 11 | Write-then-immediate-read | TiDB Jepsen stale read | Stale read on same node |
| 17 | Write-then-cross-node-read | TiDB Jepsen stale read | Cross-node stale read |
| 18 | Interleaved cross-partition reads | Read skew detection | Inconsistent snapshots |

**Two-phase commit and atomicity:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 9 | Cross-partition atomic write | Vitess 2PC | Partial commits |
| 19 | Large batch write (50 docs) | Custom | Partial batch |

**Chaos and recovery:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 7 | Single node restart | TiDB kill -9 | Data loss after restart |
| 12 | Double node restart | CockroachDB nemesis | Corruption from rapid restarts |
| 20 | Full cluster restart | CockroachDB nemesis | Data loss when all nodes down |

**Invariant preservation:**

| # | Test | Source | What it catches |
| --- | --- | --- | --- |
| 8 | Idempotent re-run | CockroachDB workload check | Corruption from repeated ops |
| 13 | Post-chaos invariant check | CockroachDB workload check | Invariants broken by stress |
| 23 | Final exhaustive invariant check | CockroachDB workload check | Any violation after all 22 tests |

## Documentation

| Document | Contents |
| --- | --- |
| [Write Scaling Research](docs/write-scaling-research.md) | Vitess, TiDB, CockroachDB comparison and what we took from each |
| [Two-Phase Commit Design](docs/two-phase-commit.md) | 2PC architecture, Vitess/TiDB/CockroachDB patterns |
| [TSO Timestamp Fix](docs/tso-replica-timestamp-fix.md) | Three timestamp ordering fixes with distributed database research |
| [Write Scaling Tests](docs/write-scaling-tests.md) | All 9 test categories with sources |
| [Engineering Changes](docs/engineering-changes.md) | Every file changed, every architectural decision |
| [Architecture Analysis](docs/why-convex-cannot-scale-horizontally.md) | The 6 bottlenecks in the original codebase |
| [Convex Internals](docs/convex-internals-explained.md) | How the Committer, SnapshotManager, WriteLog, Subscriptions, and OCC work |
| [Implementation Plan](docs/actual-implementation-plan.md) | Primary-Replica architecture design |
| [Full Scaling Proposal](docs/horizontal-scaling-proposal.md) | Complete partitioned-write architecture proposal |
| [Scalability Research](docs/convex-scalability-research.md) | Community research with 25+ source URLs |

## Configuration

### Partitioned Mode Environment Variables

| Variable | Description | Example |
| --- | --- | --- |
| `PARTITION_ID` | This node's partition number | `0` |
| `PARTITION_MAP` | Table-to-partition assignment | `messages=0,users=0,projects=1,tasks=1` |
| `NUM_PARTITIONS` | Total partitions in cluster | `2` |
| `NATS_URL` | NATS JetStream connection | `nats://nats:4222` |
| `NODE_ADDRESSES` | gRPC addresses for 2PC | `0=node-a:50051,1=node-b:50051` |
| `INSTANCE_NAME` | Unique node identifier | `convex-node-a` |
| `REPLICATION_MODE` | Node role | `primary` |

## License

The original Convex backend is licensed under [FSL-1.1-Apache-2.0](LICENSE.md). Our modifications follow the same license.
