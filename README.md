# Horizontal Scaling for Convex

Experimental fork of the [Convex open-source backend](https://github.com/get-convex/convex-backend) aiming to scale Convex horizontally while preserving the things that make Convex Convex:

- reactive subscriptions;
- serializable transactions with optimistic conflict detection;
- consistent snapshots;
- TypeScript functions running against a database-like execution model;
- developer APIs that do not expose partitions, leaders, placement versions, or routing details.

This project is currently an **alpha distributed-systems hardening effort**. It has working clustered read/write scaling milestones, Raft-backed partition durability, two-phase commit, NATS JetStream replication, and a growing correctness test suite. It should be treated as research-grade clustered infrastructure while the remaining correctness and operations roadmap is completed.

Latest milestone: [v2.1.0-alpha.1: Distributed Correctness Hardening](https://github.com/MartinKalema/horizontal-scaling-convex/releases/tag/v2.1.0-alpha.1)

## Why This Exists

Convex is a reactive database. The backend keeps live state in memory, tracks query dependencies, retries conflicting mutations, and pushes subscription updates in real time. That model is powerful, but the self-hosted backend is single-node by design.

The hard part is not just "add sharding." A useful horizontally scaled Convex must preserve the same high-level semantics developers expect from Convex:

- subscriptions should remain correct and current;
- transactions should remain serializable;
- queries should observe consistent snapshots;
- clients should not have to know which table lives on which node;
- failover should not silently change the meaning of timestamps, document IDs, or subscriptions.

That is the north star of this fork.

## Current Status

This repository has moved beyond the initial primary/replica prototype into a clustered architecture with table-level partitioning and per-partition Raft groups.

What is implemented today:

- table-level write ownership across partitions;
- a global timestamp oracle using NATS KV;
- globally allocated table numbers for portable Convex document IDs;
- cross-partition two-phase commit;
- Raft-backed partition leadership and failover;
- NATS JetStream commit-delta replication;
- idempotent replica delta apply and retention-gap detection;
- bounded remote-read frontier waits;
- shared-secret authentication for internal cluster gRPC calls;
- route authority checks that fail closed for unsafe clustered routes;
- write-owner mutation routing so clients do not have to hand-route simple writes;
- selective-delivery groundwork for reducing replication fanout;
- cluster observability metrics and an issue journal for validation history and open correctness work.

Important work still remains:

- make Raft the single visible apply point for partition commits;
- replicate full intra-partition state needed for failover;
- harden 2PC prepare durability, rollback, and decision retention;
- gate background workers with cluster authority or leases;
- unify timestamp domains for portable read-after-write fences;
- define distributed index metadata and backfill correctness;
- mature selective delivery into distributed reactive invalidation;
- add formal Jepsen / Elle coverage and real cloud benchmarks.

The open issues are the active roadmap: [GitHub Issues](https://github.com/MartinKalema/horizontal-scaling-convex/issues)

## Design Principles

| Principle | Meaning |
| --- | --- |
| Preserve Convex semantics | Scaling is incomplete if it weakens reactive subscriptions, serializable transactions, or consistent snapshots. |
| Hide topology from users | Partitions, leaders, Raft terms, NATS subjects, and placement versions are internal implementation details. |
| Fail closed | Routes that are not safe in a cluster should reject rather than execute on the wrong node. |
| Prefer explicit authority | Every route and worker should have a clear owner: partition owner, coordinator owner, Raft leader, or leased singleton. |
| Test like a database | Shell tests are useful, but the target is Jepsen/Elle-style validation and deterministic simulation for the core distributed protocols. |

Project-wide goals: [docs/project-goals.md](docs/project-goals.md)

Issue journal: [docs/issue-journal.md](docs/issue-journal.md)

Cluster authority routing: [docs/cluster-authority-routing.md](docs/cluster-authority-routing.md)

## Architecture

### Partitioned Write Ownership

```text
                    Global Timestamp Oracle
                    (NATS KV, CAS-backed)
                              |
              +---------------+---------------+
              |                               |
       Partition 0 owner               Partition 1 owner
       users, messages                 projects, tasks
              |                               |
              +------------+------------------+
                           |
                    NATS JetStream
                    commit deltas
                           |
              +------------+------------------+
              |                               |
       Other nodes apply                Other nodes apply
       replicated state                 replicated state
```

Each table is assigned to a partition. The owner partition is the authority for writes to that table. A single-partition mutation is routed to the owner. A mutation that touches tables from multiple partitions uses two-phase commit.

### Per-Partition Raft Groups

```text
Partition 0 Raft group:

  Node A leader  --Raft-->  Node B follower
       |          --Raft-->  Node C follower
       |
       +-- accepts partition-0 writes

If Node A fails:
  Node B or Node C can be elected leader and resume ownership.
```

Raft is used for partition leadership and durability. The current roadmap includes additional work to make Raft the single visible apply point for partition commits and to replicate all intra-partition state required for failover.

### Cross-Partition Commit

```text
Mutation touches users + tasks

1. Coordinator allocates commit timestamp.
2. Participant partitions prepare their writes.
3. Coordinator records the decision.
4. Participants commit or roll back the prepared transaction.
5. Commit deltas replicate to other nodes.
```

The implementation follows a Vitess/Percolator-style two-phase commit shape, adapted to Convex's in-memory snapshots and optimistic conflict detection.

### Route Authority

Clustered Convex has different authority classes:

| Authority class | Examples |
| --- | --- |
| Partition owner | Mutations that write one partition's tables |
| Coordinator owner | Sync setup, selected global metadata routes |
| Explicit forwarding | Deploy and selected API paths that can safely forward |
| Fail closed | Routes not yet migrated to a safe clustered execution model |

This is how the project avoids accidentally running deployment-global or subscription-owning code on the wrong node.

## Recently Landed

- **Global table numbers:** clustered nodes allocate table numbers from a shared allocator so Convex document IDs remain portable across nodes.
- **Replica replay hardening:** replica delta apply is idempotent, consumers are supervised, unmapped-table deltas retry instead of being silently dropped, and JetStream retention gaps fail closed with rebootstrap guidance.
- **Remote-read frontier hardening:** remote-read waits are bounded, and heartbeat naming now reflects its actual role.
- **Write-owner mutation routing:** single-partition mutations received by the wrong node can route to the owning partition instead of exposing topology to the client.
- **Route authority cleanup:** clustered routes are classified by authority and unsafe routes reject instead of running locally by accident.
- **Raft and 2PC hardening:** recent work added Raft persistence failure handling, prepared-intent separation, and durable 2PC recovery records.

## Validation

The repository currently includes:

- 77 write-scaling integration tests;
- 10 Raft failover tests;
- database unit tests for replica replay, NATS delivery, remote-read frontier behavior, partition enforcement, 2PC, and table-number allocation;
- Docker cluster scripts under [self-hosted/docker](self-hosted/docker).

These tests are useful regression coverage and have caught real bugs. They complement, but do not replace, the longer-term validation roadmap: Jepsen/Elle workloads, network nemeses, cloud benchmarks, and deterministic simulation for the core replication/commit protocols.

### Run The Local Test Harness

```sh
cd self-hosted/docker

./test.sh              # All Docker harness tests
./test.sh scaling      # Write-scaling tests
./test.sh failover     # Raft failover tests
```

## Quick Start

```sh
cd self-hosted/docker

# 1 backend node for local development
docker compose --profile single up

# 2 partitions x 3 Raft nodes.
# This wrapper gives each run a fresh advertised-address namespace so routing
# uses membership discovery instead of only the static Compose service names.
./start-cluster.sh
```

Images are published to:

```text
ghcr.io/martinkalema/convex-horizontal-scaling
```

### Deploy Functions

```sh
docker compose --profile cluster exec node-p0a ./generate_admin_key.sh
npx convex deploy --url http://127.0.0.1:3210 --admin-key <KEY>
```

## Key Components

| Component | File | Purpose |
| --- | --- | --- |
| `PartitionMap` / `PlacementState` | `crates/database/src/partition.rs` | Table ownership, placement versions, and route decisions. |
| `BatchTimestampOracle` | `crates/database/src/timestamp_oracle.rs` | Cluster timestamp allocation through NATS KV. |
| `TableNumberAllocator` | `crates/database/src/table_number_allocator.rs` | Shared table-number allocation for portable Convex IDs. |
| `TwoPhaseCoordinator` | `crates/database/src/two_phase_coordinator.rs` | Cross-partition prepare/commit/rollback orchestration. |
| `TwoPhaseCommitService` | `crates/local_backend/src/two_phase_service.rs` | Internal gRPC participant API for 2PC. |
| `RaftNode` | `crates/database/src/raft_node.rs` | Raft loop, Ready processing, leadership callbacks. |
| `RaftStorage` | `crates/database/src/raft_storage.rs` | Persistent Raft log backed by raft-engine. |
| `NatsDistributedLog` | `crates/database/src/nats_distributed_log.rs` | NATS JetStream commit-delta publishing and subscription. |
| `ReplicaDeltaConsumer` | `crates/database/src/replica.rs` | Supervised delta consumption and redelivery handling. |
| `DeltaInterestTracker` | `crates/database/src/delta_interest.rs` | Live table-interest tracking for selective delivery. |
| `RouteAuthority` | `crates/local_backend/src/route_authority.rs` | Cluster route classification and fail-closed decisions. |
| `MutationForwarder` | `crates/local_backend/src/mutation_forwarder.rs` | Internal forwarding for routed mutations. |
| `SelectiveQueryForwardingApi` | `crates/local_backend/src/query_forwarding_api.rs` | Query forwarding and read-interest warming. |

## Configuration

| Variable | Description | Example |
| --- | --- | --- |
| `RAFT_NODE_ID` | This node's Raft ID inside its partition group | `1` |
| `RAFT_PEERS` | Raft peer IDs and gRPC addresses | `1=http://node-a:50051,2=http://node-b:50051,3=http://node-c:50051` |
| `PARTITION_ID` | This node's partition number | `0` |
| `PARTITION_MAP` | Table-to-partition assignment | `messages=0,users=0,projects=1,tasks=1` |
| `NUM_PARTITIONS` | Number of table partitions | `2` |
| `NATS_URL` | NATS JetStream connection | `nats://nats:4222` |
| `NODE_ADDRESSES` | Partition owner gRPC addresses for forwarding and 2PC | `0=node-a:50051,1=node-b:50051` |
| `INSTANCE_SECRET` / `SHARED_INSTANCE_SECRET_PATH` | Shared deployment secret; also derives the current internal cluster gRPC auth token | `$(cat /run/secrets/convex_instance_secret)` |
| `CLUSTER_GRPC_PREVIOUS_INSTANCE_SECRET` / `CLUSTER_GRPC_PREVIOUS_INSTANCE_SECRET_PATH` | Previous deployment secret accepted only for rolling internal gRPC credential rotation | `$(cat /run/secrets/convex_instance_secret.previous)` |
| `INSTANCE_NAME` | Unique node identifier | `convex-node-a` |
| `REPLICATION_MODE` | Node role for primary/replica mode | `primary` |
| `REMOTE_READ_FRONTIER_HEARTBEAT_INTERVAL_MS` | Idle heartbeat interval for remote-read frontier progress | `1000` |

## Resource Requirements

Approximate local development guidance:

| Profile | Nodes | Minimum CPU | Minimum RAM | Minimum Storage |
| --- | --- | --- | --- | --- |
| `single` | 1 backend + Postgres + NATS | 4 vCPUs | 4 GB | 20 GB |
| `cluster` | 6 backends + Postgres + NATS | 16 vCPUs | 16 GB | 80 GB |

## Documentation

| Document | Contents |
| --- | --- |
| [Project Goals](docs/project-goals.md) | The semantic guarantees this fork must preserve. |
| [Issue Journal](docs/issue-journal.md) | Regressions, root causes, fixes, and validation notes. |
| [Cluster Authority Routing](docs/cluster-authority-routing.md) | Route authority classes and fail-closed behavior. |
| [Dynamic Placement](docs/dynamic-placement.md) | Placement metadata and future rebalancing control plane. |
| [Cluster Observability](docs/cluster-observability.md) | Metrics, alerts, and operator guidance. |
| [Raft Integration](docs/raft-integration.md) | Raft loop, storage, transport, and leadership lifecycle. |
| [Two-Phase Commit Design](docs/two-phase-commit.md) | 2PC architecture and recovery model. |
| [Write Scaling Tests](docs/write-scaling-tests.md) | Write-scaling test categories and coverage. |
| [Production Deployment](docs/production-deployment.md) | Deployment notes for Kubernetes, VMs, and persistent storage. |
| [Convex Internals](docs/convex-internals-explained.md) | Committer, SnapshotManager, WriteLog, subscriptions, and OCC. |

## Related Release Notes

- [v1.0.0: Early Read-Scaling Milestone](https://github.com/MartinKalema/horizontal-scaling-convex/releases/tag/v1.0.0)
- [v2.0.0: Early Write-Scaling Milestone](https://github.com/MartinKalema/horizontal-scaling-convex/releases/tag/v2.0.0)
- [v2.1.0-alpha.1: Distributed Correctness Hardening](https://github.com/MartinKalema/horizontal-scaling-convex/releases/tag/v2.1.0-alpha.1)

## License

The original Convex backend is licensed under [FSL-1.1-Apache-2.0](LICENSE.md). This fork follows the same license.
