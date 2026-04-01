# Write Scaling Research: Learning from Vitess, TiDB, and CockroachDB

## Why No Single System Is "Best"

Each system made different tradeoffs for different problems.

**Vitess** (YouTube/PlanetScale/Shopify) — sharding proxy on top of MySQL. Application-level sharding with a smart routing layer. Doesn't change MySQL itself. Cross-shard transactions are weak — their own RFC admits "the current system lacks isolation guarantees." Best for: massive MySQL deployments that need horizontal scaling without rewriting the database engine.

**TiDB/TiKV** (PingCAP) — built from scratch as a distributed database. Raft consensus per data range. TSO for global ordering. Strong consistency. Full distributed transactions with proper isolation. Best for: greenfield distributed databases with strong correctness requirements.

**CockroachDB** — also built from scratch. Raft per range. Serializable isolation. Multi-region with locality awareness. Best for: PostgreSQL-compatible workloads needing global distribution.

## What's Best for Convex

None of these directly. Convex is not MySQL and not PostgreSQL. It's an in-memory state machine with its own Committer, SnapshotManager, and OCC model. We can't bolt Vitess or CockroachDB on top of it.

But we can take the best ideas from each:

| Idea | From | Why it fits |
| --- | --- | --- |
| Batch timestamp allocation | Vitess sequences | Simple, low-latency, no Raft needed for our throughput |
| Table-level partitioning | Vitess VSchema | Convex tables map cleanly to partitions |
| Reject cross-partition writes initially | Vitess Single mode | Most mutations touch 1-3 related tables |
| Every node reads all partitions | TiDB TiFlash / our v1.0.0 | Already built — NATS delta replication |
| 2PC for cross-partition writes later | Vitess TwoPC / TiDB Percolator | Only when needed, not day one |

## The Plan

Vitess's sharding model (simple, proven, practical) with our existing NATS replication (already working) and a batch timestamp allocator (simpler than TiDB's TSO). We skip Raft entirely — we don't need consensus because each partition has a single writer. That's the Vitess insight: if one node owns a shard, you don't need Raft for that shard.

## Deep Dive: Best Parts from Each System

### From Vitess: Batch Timestamp Allocation via Sequence Tables

Instead of calling a central service for every single timestamp (TiDB's approach — one network round trip per commit), Vitess allocates IDs in chunks. A node says "give me the next 1000 timestamps" and gets back a range. It uses those 1000 locally with zero network calls. When exhausted, it asks for another batch.

Why this is the best approach: Convex's Committer runs a tight synchronous loop — `next_commit_ts()` is called on every commit in the hot path. Adding a network call there would destroy latency. With batch allocation, the hot path is a local atomic increment. The network call only happens once every N commits.

### From Vitess: Table-Level Partitioning via VSchema

Vitess doesn't shard individual rows within a table (like CockroachDB does with range-based sharding). It assigns entire tables or groups of tables to shards. The VSchema is a declarative config that says "these tables go to shard A, those tables go to shard B."

Why this fits Convex: Convex's data model is table-based. Mutations operate on named tables (`messages`, `users`). The Committer processes transactions per-table. Partitioning by table is the natural grain — no need for row-level sharding, hash keys, or vindex calculations. A simple `TableName → NodeId` map is the entire routing logic.

### From Vitess: Reject Cross-Partition Writes First, Add 2PC Later

Vitess's "Single mode" refuses transactions that touch multiple shards. The application gets an error and must restructure. This sounds limiting but works in practice because good schema design puts related tables on the same shard. Vitess ships this as the default mode — 2PC is opt-in and came years later.

Why this is correct: Building 2PC before proving that single-partition commits work is premature engineering. Most Convex mutations call `ctx.db.insert("messages", ...)` — one table, one partition, no cross-partition coordination needed. We ship single-partition writes first, measure how often cross-partition writes actually happen, then decide if 2PC is worth building.

### From TiDB: Every Node Has a Full Read View

TiDB's TiFlash gives every node a columnar replica of the entire dataset for analytics. The Raft leader handles writes, but any node can serve reads by consuming the Raft log and maintaining a local copy.

Why this is already built: This is exactly what our v1.0.0 does. Every node consumes all NATS deltas and maintains a complete SnapshotManager. Queries can go to any node. We don't need to build this — it carries forward unchanged.

### From CockroachDB: Leaseholder Concept

CockroachDB's leaseholder is the single node that can propose writes AND serve consistent reads for a range of data. Other nodes can serve slightly stale reads, but the leaseholder is authoritative.

Why this maps to our partitioned committer: For tables a node owns, it's both the writer (Committer) and the authoritative reader (SnapshotManager with the latest state). For tables it doesn't own, it serves reads from replicated state (slightly behind, fed by NATS). Each node is the leaseholder for its tables.

### From CockroachDB: No Separate Timestamp Service in the Common Case

CockroachDB uses Hybrid Logical Clocks (HLC) — each node derives timestamps from its local clock combined with a logical counter. No central TSO. Nodes exchange HLC timestamps in every message, and clocks converge naturally.

Why we don't use this but learn from it: HLC eliminates the TSO as a single point of failure, but requires careful reasoning about clock skew and causality. For our first version, batch allocation from a central counter is simpler and correctness is easier to verify. But knowing HLC exists means if the central counter becomes a bottleneck, we have a fallback.

## Summary: Our Approach

| Aspect | Our choice | Alternative we considered | Why |
| --- | --- | --- | --- |
| Timestamp allocation | Batch from central counter | TiDB per-request TSO | Zero latency in hot path |
| Partitioning grain | Table-level | Row-level (CockroachDB ranges) | Matches Convex's data model |
| Cross-partition writes | Reject initially | 2PC from day one | Ship faster, measure first |
| Read replication | NATS delta to all nodes | Raft log (TiKV) | Already built and working |
| Consensus protocol | None (single writer per partition) | Raft (CockroachDB/TiKV) | Unnecessary with partition ownership |
| Clock synchronization | Central counter | HLC (CockroachDB) | Simpler correctness, upgrade path exists |

## Sources

- [TiDB vs Vitess vs CockroachDB comparison](https://db-engines.com/en/system/CockroachDB;TiDB;Vitess)
- [Ninja Van chose TiDB over Vitess and CockroachDB](https://www.pingcap.com/case-study/choose-a-mysql-alternative-over-vitess-and-crdb-to-scale-out-our-databases-on-k8s/)
- [Distributed SQL 2025 comparison](https://sanj.dev/post/distributed-sql-databases-comparison)
- [Vitess Atomic Distributed Transactions RFC](https://github.com/vitessio/vitess/issues/16245)
- [Vitess Sequences](https://vitess.io/docs/22.0/user-guides/vschema-guide/sequences/)
- [Supabase Read Replicas](https://supabase.com/docs/guides/platform/read-replicas)
- [PlanetScale Horizontal Sharding](https://planetscale.com/blog/horizontal-sharding-for-mysql-made-easy)
- [Shopify scaling with Vitess](https://shopify.engineering/horizontally-scaling-the-rails-backend-of-shop-app-with-vitess)
- [TiDB Timestamp Oracle](https://tikv.org/deep-dive/distributed-transaction/timestamp-oracle/)
- [CockroachDB Replication Layer](https://www.cockroachlabs.com/docs/stable/architecture/replication-layer)
