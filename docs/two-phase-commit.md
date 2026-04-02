# Two-Phase Commit for Cross-Partition Writes

## Why

Currently, mutations that write to tables on different partitions are rejected:

```
Write to table 'projects' rejected: owned by partition-1, not this node (partition-0).
```

Most mutations touch 1-3 related tables on the same partition. But some operations span partitions — e.g., creating a project and logging an audit entry, where `projects` is on partition 1 and `audit_log` is on partition 0.

## How Others Solve This

### Vitess (our model)

VTGate detects multi-shard transactions on commit. Sends "prepare" to each shard. Each shard saves a redo log and keeps the transaction open. First shard becomes "Metadata Manager" (MM) storing the commit decision. If all prepares succeed, MM commits. If any fails, MM rolls back. A watcher service recovers stuck transactions.

Source: [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)

### TiDB (optimization we already use)

When a transaction touches only one region, skip 2PC entirely — just commit directly. This is the 1PC optimization. We already do this: single-partition commits go through the normal fast path.

Source: [TiDB 1PC](https://pingcap.github.io/tidb-dev-guide/understand-tidb/1pc.html)

### CockroachDB (more complex than needed)

Parallel Commits protocol with write intents as provisional values + transaction records. Can respond to client before all intents are resolved. We don't need this complexity because our partitions are coarser (table-level, not range-level).

Source: [CockroachDB Parallel Commits](https://www.cockroachlabs.com/blog/parallel-commits/)

## Architecture

```
Client mutation writes to tables on partitions 0 and 1:

   CommitterClient._commit()
         │
         │  Detects cross-partition writes
         ▼
   TwoPhaseCoordinator
         │
    ┌────┴────┐
    ▼         ▼
 Prepare    Prepare
 Node A     Node B (via gRPC)
    │         │
    │  OCC    │  OCC
    │  check  │  check
    │         │
    ▼         ▼
  Both OK? ──Yes──► Record COMMITTED in NATS KV
    │                     │
    │              ┌──────┴──────┐
    │              ▼             ▼
    │         CommitPrepared  CommitPrepared
    │         Node A         Node B
    │              │             │
    No             ▼             ▼
    │         publish_commit  publish_commit
    ▼         (visible)      (visible)
  Rollback
  all participants
```

## Design

### Transaction Classification (Fast Path)

In `CommitterClient._commit()`, before sending to the Committer, inspect the transaction's write set:

- **Single partition**: Normal commit path. No protocol change. This is TiDB's 1PC optimization.
- **Cross-partition**: Enter 2PC path via `TwoPhaseCoordinator`.

### Prepare Phase

New `CommitterMessage::Prepare` variant. The coordinator splits writes by partition and sends each partition's subset to its Committer.

Each Committer's `handle_prepare()`:

1. OCC conflict check against local write log
2. Assign prepare timestamp via `next_commit_ts()` (TSO)
3. Compute writes (index updates, document writes)
4. Push to PendingWrites (blocks conflicting transactions)
5. Persist redo log entry for crash recovery
6. Return PrepareResult with the prepare timestamp

The prepare does NOT call `publish_commit` — writes are staged but not visible.

### Commit Decision

After all partitions prepare successfully:

1. Coordinator records `COMMITTED` in NATS KV (atomic, the point of no return)
2. Sends `CommitPrepared` to all participants
3. Each participant writes to persistence, calls `publish_commit`, deletes redo log

If any partition fails to prepare:

1. Coordinator records `ROLLED_BACK` in NATS KV
2. Sends `RollbackPrepared` to participants that did prepare
3. Each participant removes from PendingWrites, deletes redo log

### Recovery (Transaction Watcher)

Background task on every node, scanning NATS KV for unresolved 2PC transactions:

- **COMMITTED but not fully resolved**: Coordinator crashed after decision. Watcher sends `CommitPrepared` to participants with redo logs.
- **No decision after timeout**: Coordinator crashed before deciding. Watcher records `ROLLED_BACK` and sends rollback.
- **ROLLED_BACK but not fully resolved**: Watcher sends `RollbackPrepared` to remaining participants.

### Node-to-Node Communication

Extend the existing gRPC service with 2PC RPCs:

```protobuf
service TwoPhaseCommit {
    rpc Prepare(PrepareRequest) returns (PrepareResponse);
    rpc CommitPrepared(CommitPreparedRequest) returns (CommitPreparedResponse);
    rpc RollbackPrepared(RollbackPreparedRequest) returns (RollbackPreparedResponse);
}
```

Nodes need a config mapping: `partition_id -> gRPC address`.

## Implementation Order

1. Core types: `TwoPhaseTransactionId`, `TwoPhaseState`, `PrepareResult`, `TwoPhaseRedoEntry`
2. CommitterMessage variants: `Prepare`, `CommitPrepared`, `RollbackPrepared`
3. Committer handlers: `handle_prepare()`, `handle_commit_prepared()`, `handle_rollback_prepared()`
4. CommitterClient extensions: `prepare()`, `commit_prepared()`, `rollback_prepared()`
5. gRPC service: Extend replication.proto, implement server + client
6. Coordinator: `TwoPhaseCoordinator` called from `CommitterClient._commit()`
7. Transaction watcher: Background recovery task
8. Config: `NODE_ADDRESSES` env var (partition_id=host:port mapping)
9. Tests: Cross-partition mutation test, coordinator failure recovery test

## Sources

- [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)
- [Vitess 2PC Blog Post](https://vitess.io/blog/2016-06-07-distributed-transactions-in-vitess/)
- [TiKV Percolator Deep Dive](https://tikv.org/deep-dive/distributed-transaction/percolator/)
- [TiDB 1PC Optimization](https://pingcap.github.io/tidb-dev-guide/understand-tidb/1pc.html)
- [CockroachDB Parallel Commits](https://www.cockroachlabs.com/blog/parallel-commits/)
- [CockroachDB Transaction Layer](https://www.cockroachlabs.com/docs/stable/architecture/transaction-layer)
