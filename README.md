# Horizontal Scaling for Convex

The first Primary-Replica horizontal scaling implementation for the [Convex open-source backend](https://github.com/get-convex/convex-backend).

Write to the Primary, read from the Replica. Data replicates in real-time via NATS JetStream. Each node has its own independent PostgreSQL database.

```
=== WRITE TO PRIMARY (port 3210) ===
{"status":"success","value":null}

=== READ FROM PRIMARY (port 3210) ===
{"status":"success","value":[{"text":"hello horizontal scaling","author":"admin","channel":"announcements"}]}

=== READ FROM REPLICA (port 3220) ===
{"status":"success","value":[{"text":"hello horizontal scaling","author":"admin","channel":"announcements"}]}
```

## Architecture

```
Client ──mutation──▶ Primary ──persist──▶ PostgreSQL (primary-db)
                        │
                        ├──publish delta──▶ NATS JetStream
                        │                        │
Client ──query──▶ Replica ◀──consume delta───────┘
                    │
                    └──persist──▶ PostgreSQL (replica-db)
```

- **Primary** handles writes, publishes `CommitDelta` to NATS after each commit
- **Replica** consumes deltas from NATS, remaps TabletIds, applies to its own SnapshotManager + Persistence
- **NATS JetStream** carries the commit log between nodes with durable consumers
- **Each node** has its own PostgreSQL database — fully independent, different TabletIds
- **Shared storage** volume for JavaScript module files
- **Single Committer** per node as the apply loop (etcd/TiKV/Kafka pattern)

## Quick Start

### Prerequisites

- Docker and Docker Compose
- Rust nightly-2026-02-18 (installs automatically from `rust-toolchain`)
- Node.js 20.19.5
- Just (`brew install just`)

### Build and Run

```sh
# Build the custom Docker image (first build ~10min, subsequent ~2min)
docker build -f self-hosted/docker-build/Dockerfile.backend \
  -t convex-backend-replicated \
  --build-arg VERGEN_GIT_SHA=$(git rev-parse HEAD) \
  --build-arg VERGEN_GIT_COMMIT_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ) .

# Start Primary + Replica + PostgreSQL + NATS + Dashboard
cd self-hosted/docker
docker compose -f docker-compose.replicated.yml up
```

### Test Replication

```sh
# Generate admin key
docker compose -f docker-compose.replicated.yml exec primary ./generate_admin_key.sh

# Deploy functions (from a Convex project directory)
npx convex deploy --url http://127.0.0.1:3210 --admin-key <KEY>

# Write to Primary
curl -s -X POST http://localhost:3210/api/mutation \
  -H "Content-Type: application/json" \
  -H "Convex-Client: npm-1.34.1" \
  -d '{"path":"messages:send","args":{"text":"hello"},"format":"json"}'

# Read from Replica
curl -s -X POST http://localhost:3220/api/query \
  -H "Content-Type: application/json" \
  -H "Convex-Client: npm-1.34.1" \
  -d '{"path":"messages:list","args":{},"format":"json"}'
```

### Ports

| Service | Port |
|---|---|
| Primary API | 3210 |
| Primary Site Proxy | 3211 |
| Primary gRPC | 50051 |
| Replica API | 3220 |
| Replica Site Proxy | 3221 |
| Dashboard | 6791 |
| PostgreSQL | 5433 |
| NATS Client | 4222 |
| NATS Monitoring | 8222 |

## What Was Built

11 new files, 16 modified files, 1,718 lines of new Rust code. 337 existing tests pass.

| Component | What It Does |
|---|---|
| `CommitDelta` | Captures everything that changed in a transaction — documents, indexes, table mappings |
| `NatsDistributedLog` | Publishes/subscribes deltas via NATS JetStream with durable consumers |
| `ReplicaDeltaConsumer` | Background task tailing NATS and feeding deltas through the Committer |
| `apply_replica_delta` | 5-phase apply: table creation, TabletId remap, snapshot update, persistence write, log update |
| `MutationForwarderService` | gRPC server on Primary accepting forwarded mutations from Replicas |
| `SnapshotCheckpointer` | Periodic persistence snapshots to object storage for Replica bootstrap |
| `CheckpointPersistence` | In-memory persistence from checkpoint data for bootstrap without database |

## Development

```sh
cargo build -p database          # Build
cargo test -p database            # Test (337 tests)
cargo test -p database "replica"  # Run specific tests
cargo fmt -p database             # Format
```

## Documentation

| Document | Contents |
|---|---|
| [Engineering Changes](docs/engineering-changes.md) | Every file changed, every architectural decision, every pattern used |
| [Architecture Analysis](docs/why-convex-cannot-scale-horizontally.md) | Source code analysis of the 6 bottlenecks |
| [Convex Internals](docs/convex-internals-explained.md) | How the Committer, SnapshotManager, WriteLog, Subscriptions, and OCC work |
| [Implementation Plan](docs/actual-implementation-plan.md) | Primary-Replica architecture design |
| [Testing Strategy](docs/testing-strategy.md) | 6 layers of testing across unit, integration, consistency, chaos, performance, regression |
| [Scalability Research](docs/convex-scalability-research.md) | Community research with 25+ source URLs |
| [Bank Analogy](docs/convex-bank-analogy.md) | Plain-language explanation of the architecture |

## Prior Art

No one has done this before. Convex's own [Funrun](https://stack.convex.dev/horizontally-scaling-functions) scales function execution but not the database. The 657+ forks on GitHub show no horizontal scaling attempts. [Issue #95](https://github.com/get-convex/convex-backend/issues/95) and [#188](https://github.com/get-convex/convex-backend/issues/188) raised scalability concerns but no solutions. Convex's docs explicitly state self-hosted is single-node by design.

## License

The original Convex backend is licensed under [FSL-1.1-Apache-2.0](LICENSE.md). Our modifications follow the same license.
