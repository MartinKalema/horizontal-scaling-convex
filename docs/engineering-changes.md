# Engineering Changes: Horizontal Scaling for Convex

## Overview

1,718 lines of new code across 11 new files. 16 existing files modified. The Convex open-source backend was transformed from a single-node system into a Primary-Replica architecture with real-time delta replication via NATS JetStream.

## New Files Created

| File | Lines | Purpose |
|---|---|---|
| `crates/database/src/commit_delta.rs` | 177 | `CommitDelta` struct, `DistributedLog` trait, `NoopDistributedLog`, `InMemoryDistributedLog` (testing) |
| `crates/database/src/nats_distributed_log.rs` | 275 | NATS JetStream implementation of `DistributedLog` — publish/subscribe with durable consumers, proto-encoded document updates, TabletId mapping serialization |
| `crates/database/src/replica.rs` | 89 | `ReplicaDeltaConsumer` — subscribes to NATS and feeds deltas through the Committer's apply loop |
| `crates/database/src/commit_client.rs` | 109 | `CommitClient` trait abstracting local vs remote commits, `LocalCommitClient`, `ReadOnlyCommitClient` |
| `crates/database/src/checkpoint.rs` | 308 | `CheckpointData`, `CheckpointPersistence` (in-memory persistence from checkpoint), `create_checkpoint()` |
| `crates/database/src/snapshot_checkpointer.rs` | 324 | `SnapshotCheckpointer` — periodic persistence snapshot writer to object storage |
| `crates/database/src/tests/replication_tests.rs` | 77 | Integration test proving Primary publishes deltas to distributed log |
| `crates/local_backend/src/mutation_forwarder.rs` | 170 | gRPC `MutationForwarderService` (server) and `MutationForwarderGrpcClient` (client) for Replica-to-Primary mutation forwarding |
| `crates/pb/protos/replication.proto` | 63 | Protobuf service definition for `MutationForwarder` RPC |
| `self-hosted/docker/docker-compose.replicated.yml` | 126 | Docker Compose with PostgreSQL, NATS JetStream, Primary, Replica, and Dashboard |
| `self-hosted/docker/init-db.sql` | 2 | Creates separate databases for Primary and Replica |

## Existing Files Modified

### `crates/database/src/committer.rs` — The Core Change

The Committer is the single-threaded apply loop that serializes all state machine updates. We made three changes:

1. **Added `distributed_log` field** — After each commit, the Committer publishes a `CommitDelta` to the distributed log (NATS in production, noop in single-node mode).

2. **Added `ApplyReplicaDelta` message** — A new variant in `CommitterMessage` that carries a `CommitDelta` from NATS. This goes through the same `mpsc` channel as local commits, preserving the single-writer guarantee.

3. **Implemented `apply_replica_delta`** — The 5-phase delta application method:
   - Phase 1: Apply `_tables` documents first (creates new tables on Replica)
   - Phase 2: Rebuild TabletId remap and apply remaining documents
   - Phase 3: Write to Replica's persistence (enables function runner to load modules)
   - Phase 3b: Advance `max_repeatable_ts` so reads at the new timestamp work
   - Phase 4: Update write log for subscription invalidation
   - Phase 5: Push updated snapshot to SnapshotManager

4. **Added `tablet_id_to_table_name` mapping** — Built from the SnapshotManager's table registry before publishing each delta. This allows Replicas to remap Primary TabletIds to their own local TabletIds.

### `crates/database/src/database.rs`

- Added `distributed_log` and `replica_mode` parameters to `Database::load`
- In replica mode, the Committer uses `NoopDistributedLog` (doesn't re-publish received deltas)
- Added `committer_client()` accessor for external code to send deltas to the Committer

### `crates/database/src/write_log.rs`

- Added `WriteSource::as_str()` for NATS serialization
- Added `index_keys_from_document_updates()` for converting `DocumentUpdate` to write log format on Replicas

### `crates/common/src/document.rs`

- Added `ResolvedDocument::to_remapped(TabletId)` — creates a copy with a different TabletId for cross-database replication

### `crates/storage/src/lib.rs`

- Added `StorageUseCase::Checkpoints` variant for checkpoint storage

### `crates/application/src/lib.rs`

- Added `ApplicationStorage::new_local()` for Replicas to create storage without writing to the database

### `crates/local_backend/src/config.rs`

- Added `REPLICATION_MODE` (primary/replica), `NATS_URL`, `GRPC_PORT`, `PRIMARY_GRPC_URL` config options

### `crates/local_backend/src/main.rs`

- Install `rustls` ring crypto provider at process startup (required by async-nats)
- Start gRPC mutation forwarder server on Primary when NATS is configured

### `crates/local_backend/src/lib.rs`

- Connect to NATS and create `NatsDistributedLog` when `NATS_URL` is set
- Start `SnapshotCheckpointer` on Primary
- Skip `initialize_application_system_tables` and use `ApplicationStorage::new_local` on Replica
- Start `ReplicaDeltaConsumer` via `spawn_background` on Replica with dedicated NATS connection

### `crates/pb/src/lib.rs`

- Added `replication` module for generated protobuf code

### `Cargo.toml` and crate `Cargo.toml` files

- Added `async-nats`, `hex`, `bytes`, `prost`, `rustls` (with ring), `tonic`, `pb` dependencies

## Architectural Decisions

### Single Apply Loop (etcd/TiKV/Kafka Pattern)

Every node — Primary or Replica — has exactly one Committer that serializes all state machine updates. Both local commits and replicated deltas go through the same `mpsc` channel. This eliminates concurrent writer races without locks.

**Researched from:**
- etcd's `commitC` channel pattern
- TiKV's Apply Worker thread
- Kafka KRaft's state machine callback interface

### TabletId Remapping

Each database instance generates unique internal TabletIds for tables. The Primary's `_modules` table has a different TabletId than the Replica's. Deltas carry a `tablet_id_to_table_name` mapping so Replicas can translate Primary IDs to local IDs by matching on table name.

### Separate Databases Per Node

Each Replica has its own PostgreSQL database with its own lease, its own TabletIds, and its own persistence. Replicas are fully independent — they can restart, rebuild from checkpoints, and serve queries without any dependency on the Primary's database.

### Shared Module Storage

Function code (JavaScript modules) is stored in object storage, not in the database. Both Primary and Replica mount the same storage volume so the Replica's function runner can load module code uploaded by the Primary.

### NATS JetStream for Distributed Log

Chosen over Kafka for operational simplicity (single binary, no JVM/ZooKeeper). Uses:
- Limits-based retention (24h TTL)
- Durable pull consumer (`convex-replica`) for reliable delivery across reconnects
- Proto-encoded document updates with JSON envelope
- Double-await publish for server-acknowledged durability

## What We Did NOT Change

- The OCC (Optimistic Concurrency Control) model
- The Subscription/WebSocket real-time system
- The Funrun function execution architecture
- The client SDK or developer-facing APIs
- Any query or mutation semantics
- The 337 existing unit tests (all pass)
