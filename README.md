# Horizontal Scaling for Convex

The first horizontal scaling implementation for the [Convex open-source backend](https://github.com/get-convex/convex-backend) — both reads and writes.

## What This Does

Two independent Convex nodes, each owning a partition of tables, writing in parallel, replicating to each other in real-time via NATS JetStream. A global Timestamp Oracle (TiDB PD pattern) ensures ordering. Two-phase commit handles cross-partition writes.

```
ALL 25 TESTS PASSED

 1. Cross-partition data verification     (Vitess VDiff)         — PASS
 2. Bank invariant — single table         (CockroachDB Jepsen)   — PASS
 3. Bank invariant — multi-table          (TiDB bank-multitable) — PASS
 4. Partition enforcement (5 subtests)    (Vitess Single mode)   — PASS
 5. Concurrent write scaling              (CockroachDB KV)       — PASS 175 writes/sec
 6. Monotonic reads                       (TiDB monotonic)       — PASS
 7. Node restart recovery                 (TiDB kill -9)         — PASS
 8. Idempotent re-run                     (CockroachDB workload) — PASS
 9. Two-phase commit cross-partition      (Vitess 2PC)           — PASS
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

Runs all 25 integration tests (9 categories) against the live partitioned deployment.

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

### Integration Tests (25 assertions across 9 categories)

```sh
cd self-hosted/docker && ./test-write-scaling.sh
```

| # | Test | Source | What it proves |
| --- | --- | --- | --- |
| 1 | Cross-partition data verification | Vitess VDiff | Both nodes see all data from both partitions |
| 2 | Bank invariant — single table | CockroachDB Jepsen bank | Numeric totals preserved across replication |
| 3 | Bank invariant — multi-table | TiDB bank-multitable | Cross-table invariants hold across partitions |
| 4 | Partition enforcement | Vitess Single mode | Wrong-partition writes rejected, no phantom data |
| 5 | Concurrent write scaling | CockroachDB KV | Parallel writes with zero data loss |
| 6 | Monotonic reads | TiDB monotonic | Values never go backward |
| 7 | Node restart recovery | TiDB kill -9 / CockroachDB nemesis | Recovers after crash, sees writes from downtime |
| 8 | Idempotent re-run | CockroachDB workload check | No corruption from repeated operations |
| 9 | Two-phase commit | Vitess 2PC | Atomic writes to tables on different partitions |

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
