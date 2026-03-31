# Why Convex Cannot Be Horizontally Scaled

## The Short Answer

A stateless Python app on Cloud Run can scale horizontally because **each instance is identical and disposable** — it reads from and writes to an external database, holds no local state, and any request can go to any instance.

Convex is fundamentally different: it is a **stateful system** where the backend process itself holds critical ground truth in memory. Running two Convex backend instances doesn't give you two workers sharing a database — it gives you two **divergent sources of truth** that would immediately conflict.

---

## The 6 Architectural Reasons

### 1. Single-Writer Serializable Committer

**The core blocker.** All database transactions in Convex are applied serially by a single-threaded `Committer` task.

- **File**: `crates/database/src/committer.rs` (lines 247-416)
- **File**: `crates/database/src/database.rs` (lines 264-278)

The Committer:
- Receives all commit requests through a single channel
- Validates each transaction against the current in-memory state
- Assigns **monotonically increasing timestamps** (a total ordering)
- Writes to persistence **in order**

This is not a limitation that can be fixed with a connection pool or a load balancer. The serial ordering is what gives Convex its **serializability guarantee**. Two committers on two instances would produce conflicting timestamp orderings, breaking the consistency model entirely.

**Cloud Run comparison**: A Python app on Cloud Run delegates ordering to the database (e.g., Postgres uses MVCC + locks). Convex **is** the database — it handles ordering itself, in-process.

---

### 2. In-Memory Snapshot Manager (Ground Truth Lives in RAM)

- **File**: `crates/database/src/snapshot_manager.rs` (lines 74-95)
- **File**: `crates/database/README.md` (lines 1-133)

The `SnapshotManager` holds a sliding window of multi-version snapshots in memory, containing:

| State Component | Purpose |
|---|---|
| Table registry | Table metadata, shapes, document counts |
| Index registry | Index metadata, backfill states |
| In-memory indexes | Text search, vector search indexes |
| Schema registry | Schema validation state |
| Component registry | Component tree state |

This in-memory state **is the ground truth**. The persistence layer (SQLite/Postgres/MySQL) is the durable backing store, but the live snapshot in RAM is what transactions read from and validate against.

If you spun up Instance B, it would have **its own separate snapshot** — immediately stale relative to Instance A's commits. There is no mechanism to synchronize these snapshots across processes.

**Cloud Run comparison**: A Python app holds no local state — it queries the database for every request. Convex's architecture is closer to **Redis** (in-memory data structure store) than to a stateless web server.

---

### 3. Optimistic Concurrency Control (OCC) Requires a Global Commit Log

- **File**: `crates/database/src/committer.rs` (lines 683-699)
- **Docs**: `npm-packages/docs/docs/database/advanced/occ.md` (lines 85-151)

Convex uses Optimistic Concurrency Control:

1. A transaction reads documents at a snapshot timestamp
2. It performs its logic (your Convex function)
3. At commit time, the Committer checks: "Have any of the documents this transaction read been modified since that snapshot?"
4. If yes → conflict → automatic retry
5. If no → commit succeeds → new timestamp assigned

This **only works** with a globally ordered commit log managed by a single authority. With two instances:

```
Instance A: commits TX1 at ts=100, modifies document X
Instance B: commits TX2 at ts=100, also modifies document X
```

Both would succeed because neither knows about the other's commit. **Serializability is broken.**

---

### 4. Real-Time Subscriptions Are Held In-Memory

- **File**: `crates/database/src/subscription.rs` (lines 1-360)
- **File**: `crates/database/src/write_log.rs` (lines 209-250)

Convex's real-time reactivity works like this:

1. A client subscribes to a query (e.g., `listMessages`)
2. The backend tracks which documents that query read (its **read set**)
3. On every commit, the write log is scanned to see if any subscription's read set was affected
4. Affected subscriptions trigger re-execution and push updates to the client via WebSocket

The subscription state lives **in the memory of the backend instance the client is connected to**. If Instance B commits a change that affects a subscription on Instance A, Instance A has no way to know — the client never gets the update.

**Cloud Run comparison**: Cloud Run apps don't hold WebSocket connections or subscription state. They handle a request and forget. Real-time features typically use a separate pub/sub system (e.g., Firebase, Pusher). Convex bundles this into the backend itself.

---

### 5. No Distributed Consensus in the Codebase

- **File**: `Cargo.toml` (lines 185, 208-229)

The dependency list includes:
- `rusqlite` — embedded single-process database
- `tokio-postgres` — PostgreSQL client
- `mysql_async` — MySQL client

It does **not** include:
- Any Raft implementation (e.g., `openraft`, `tikv/raft-rs`)
- Any Paxos implementation
- Any distributed locking (e.g., Redis-based locks, etcd client)
- Any distributed log (e.g., Kafka client)

Horizontal scaling of a stateful system requires **distributed consensus** to agree on ordering. Convex has no such mechanism — it doesn't need one, because it runs on a single node.

---

### 6. Single-Instance Deployment Architecture

- **File**: `self-hosted/README.md` (lines 24-36)
- **File**: `self-hosted/advanced/postgres_or_mysql.md` (lines 1-27)

The self-hosted deployment guide describes three services:
- **Backend** (single instance)
- **Dashboard** (stateless UI)
- **Frontend** (your app)

The docs explicitly recommend the backend be **"in the same region and as close as possible to the database"** — a single-instance assumption. There is no documentation for multi-instance backend deployment, replica sets, or sharding.

---

## Why a Python App on Cloud Run Is Different

| Property | Python on Cloud Run | Convex Backend |
|---|---|---|
| State | Stateless — no local data | Stateful — snapshots, indexes, subscriptions in RAM |
| Database | External (e.g., Cloud SQL) | Embedded / tightly coupled |
| Ordering | Delegated to database | Managed in-process by Committer |
| Subscriptions | None (or via external pub/sub) | In-memory, per-instance |
| Consistency | Database handles it | Application handles it |
| Scaling model | Add identical workers | Would require distributed consensus |

A Cloud Run instance is a **pure function**: `request -> response`. You can stamp out 100 of them and they all behave identically because the state lives elsewhere.

A Convex backend instance is a **stateful actor**: it holds the live database, manages subscriptions, orders transactions, and maintains indexes. Duplicating it doesn't help — it creates **split-brain**.

---

## What Would Horizontal Scaling Require?

To make Convex horizontally scalable, you would need to add:

1. **Distributed consensus** (Raft/Paxos) for global transaction ordering
2. **Distributed snapshot synchronization** so all nodes agree on current state
3. **Subscription routing** so writes on any node notify subscriptions on all nodes
4. **Distributed OCC validation** — conflict detection across nodes

This is essentially building a **distributed database** (like CockroachDB or TiDB). It's a massive architectural change — not a configuration tweak.

---

## Summary

Convex trades horizontal scalability for **strong guarantees**: serializability, real-time reactivity, and automatic conflict resolution. These guarantees are easy to provide on a single node. Providing them across multiple nodes is the hardest problem in distributed systems.

The answer to "why can't Convex scale horizontally?" is the same as the answer to "why can't SQLite scale horizontally?" — the architecture assumes a single process owns the data. The difference is that Convex gives you much more than SQLite (reactivity, OCC, TypeScript functions), but the single-node assumption is the same.
