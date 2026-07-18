# Project Goals

This fork is not only trying to run the Convex backend on more machines. The
goal is to preserve the product shape that makes Convex different while adding
horizontal scale and high availability.

## Preserve Convex's Core Attributes

Scaling work is incomplete if it improves throughput by weakening the
properties that make Convex feel like Convex:

- serializable transactions with optimistic conflict detection and automatic
  retry;
- consistent snapshots for queries, mutations, exports, and subscriptions;
- reactive invalidation that keeps clients current without application-managed
  cache invalidation;
- the TypeScript function model where application code talks to one logical
  database.

FoundationDB, Spanner, CockroachDB, TiDB, Vitess, and YugabyteDB prove that
parts of this problem can be distributed. They do not, by themselves, provide
Convex's full combination of arbitrary TypeScript functions, automatic read-set
tracking, consistent snapshots, exact reactive invalidation, and transparent
horizontal write scaling.

## Do Not Leak Topology Into Developer APIs

Internal topology includes partitions, ranges, shards, Raft leaders, placement
versions, ownership leases, NATS subjects, resolver shards, replication
frontiers, and subscription invalidation workers. Application developers should
not need to reason about those details to write normal Convex queries,
mutations, actions, or subscriptions.

The following are topology leaks and should be treated as design failures
unless the project explicitly accepts the trade-off:

- requiring shard, partition, range, or placement hints in `ctx.db.query`,
  `ctx.db.get`, `ctx.db.insert`, `ctx.db.patch`, or mutation APIs;
- making a transaction invalid only because records live on different
  partitions;
- requiring application code to choose strong-vs-stale read modes for basic
  correctness;
- exposing placement versions or partition ownership to client logic;
- requiring subscription code to register with a specific shard or invalidation
  worker;
- documenting "put these tables together or correctness breaks" as an
  application contract.

The system may expose topology to operators through metrics, runbooks,
placement tooling, debugging APIs, and administrative dashboards. Operator
visibility is good. Developer-visible topology requirements are the problem.

## Accept Performance Reality Without Moving Correctness To Users

Cross-partition work may be slower than single-partition work. That can be
documented as performance guidance, and operators may use placement tools to
co-locate related tables or ranges. It must not become a correctness
precondition for application developers.

The public API should still look like one logical Convex database. Placement,
routing, conflict checking, replication, and invalidation are system
responsibilities.

## Fail Closed Before Weakening Semantics

If a route, subscription path, write path, placement transition, or failover
case cannot prove the same Convex semantics in a clustered deployment, it should
fail closed, forward to an authority, or remain coordinator-owned until the
distributed design is complete.

The normative invariant definitions, authority boundaries, and proof
requirements are maintained in
[Distributed Correctness Model](distributed-correctness-model.md).
