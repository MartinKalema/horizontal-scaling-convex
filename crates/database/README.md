# Database Crate — Architecture

This crate contains the core database engine for Convex, extended with horizontal scaling support via Primary-Replica replication.

## Original Architecture (Single Node)

The `Database` consists of a `SnapshotManager` that maintains multiple `Snapshot`s at different `Timestamp`s. Each snapshot contains table metadata, index metadata, and in-memory indexes.

```
Database
  │
  ▼
SnapshotManager
  ├── Snapshot @ ts=100
  ├── Snapshot @ ts=101
  └── Snapshot @ ts=102 (latest)
```

Each `Snapshot` holds:
- **TableRegistry** — table names, IDs, shapes, sizes (from `_tables` system table)
- **IndexRegistry** — index names, fields, backfill state (from `_index` system table)
- **BackendInMemoryIndexes** — actual index data for system tables kept in memory
- **SchemaRegistry** — schema validation rules
- **ComponentRegistry** — component tree metadata

### Committer (The Apply Loop)

All writes go through a single `Committer` — an async loop that processes `CommitterMessage`s from an `mpsc` channel one at a time. This serializes all state machine updates, providing serializability without distributed consensus.

The Committer:
1. Validates the transaction (OCC conflict check via `WriteLog`)
2. Assigns a monotonically increasing timestamp
3. Writes to persistence (PostgreSQL/SQLite)
4. Updates the `SnapshotManager`
5. Appends to the `WriteLog` (for subscription invalidation)

## Horizontal Scaling Extensions

### New Modules

| Module | Purpose |
| --- | --- |
| `commit_delta.rs` | `CommitDelta` struct capturing all changes per transaction. `DistributedLog` trait with `NoopDistributedLog` and `InMemoryDistributedLog` (testing). |
| `nats_distributed_log.rs` | NATS JetStream implementation of `DistributedLog`. Publishes deltas with proto-encoded documents and TabletId-to-table-name mappings. Durable consumers per Replica. |
| `replica.rs` | `ReplicaDeltaConsumer` — background task that subscribes to NATS and feeds deltas through the Committer's `apply_replica_delta` method. |
| `commit_client.rs` | `CommitClient` trait abstracting local vs remote commits. `LocalCommitClient` wraps `CommitterClient`. `ReadOnlyCommitClient` rejects writes on Replicas. |
| `checkpoint.rs` | `CheckpointData` and `CheckpointPersistence` — in-memory persistence from a serialized snapshot, for bootstrapping Replicas without connecting to the Primary's database. |
| `snapshot_checkpointer.rs` | `SnapshotCheckpointer` — periodic persistence snapshot writer to object storage (local or R2/S3). |

### Modified Modules

**`committer.rs`** — Three additions:
1. `distributed_log` field — publishes `CommitDelta` to NATS after each commit
2. `ApplyReplicaDelta` message variant — receives deltas from NATS via the Replica consumer
3. `apply_replica_delta` method — 5-phase delta application:
   - Phase 1: Apply `_tables` documents first (creates new tables on Replica)
   - Phase 2: Rebuild TabletId remap, apply remaining documents
   - Phase 3: Write to Replica's persistence + advance `max_repeatable_ts`
   - Phase 4: Update write log for subscription invalidation
   - Phase 5: Push updated snapshot to SnapshotManager

**`database.rs`** — `Database::load` accepts `distributed_log` and `replica_mode` parameters. In replica mode, the Committer uses `NoopDistributedLog` to avoid re-publishing received deltas. Added `committer_client()` accessor.

**`write_log.rs`** — Added `WriteSource::as_str()` and `index_keys_from_document_updates()` for Replica write log integration.

### Data Flow

**Primary:**
```
Client mutation
  → Committer validates + assigns timestamp
  → Writes to PostgreSQL
  → Updates SnapshotManager
  → Publishes CommitDelta to NATS JetStream
```

**Replica:**
```
NATS JetStream delivers CommitDelta
  → ReplicaDeltaConsumer receives it
  → Sends ApplyReplicaDelta to Committer via mpsc channel
  → Committer remaps TabletIds (Primary → Replica)
  → Applies to SnapshotManager
  → Writes to Replica's PostgreSQL
  → Updates WriteLog for subscriptions
```

### TabletId Remapping

Each database instance generates unique `TabletId`s for tables. The Primary's `_modules` table has a different TabletId than the Replica's. Each `CommitDelta` carries a `tablet_id_to_table_name` mapping so Replicas can translate Primary IDs to local IDs by matching on table name.

### Single Apply Loop Pattern

Following etcd, TiKV, and Kafka KRaft: one Committer per node serializes all state machine updates. Both local commits and replicated deltas go through the same `mpsc` channel. No concurrent writers, no locks beyond the channel.

## Development

```sh
cargo build -p database          # Build
cargo test -p database            # Test (337 tests)
cargo test -p database "replica"  # Run replication tests
```
