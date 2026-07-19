# Distributed Correctness Model

- **Status:** Normative target architecture
- **Tracks:** GitHub issues `#258` and `#289`

This document defines the correctness contract for horizontally scaling Convex.
It is the reference for design reviews. Implementation documents may describe
the code as it exists today, but they must not weaken the invariants here.

The words **must**, **must not**, **required**, and **may** are intentional.

## North Star

The cluster presents one logical Convex database. A developer using normal
Convex APIs must not be able to observe partitions, Raft leaders, placement
epochs, NATS subjects, partial transaction state, or replica catch-up.

Distribution may add latency or return an explicit retryable availability
error. It must not produce a result, committed history, subscription update,
document identity, or side effect that single-node Convex semantics would not
permit.

## What We Must Not Lose

Horizontal scaling is incomplete unless all of these remain true:

1. **Serializable transactions.** Arbitrary TypeScript mutations continue to
   use optimistic conflict detection and automatic retry. Partition placement
   cannot create an exception to serializability.
2. **One consistent snapshot.** Every read performed by one query, mutation,
   subscription rerun, export, or backup observes one logical database version,
   including reads discovered dynamically while JavaScript executes.
3. **Atomic visibility.** A cross-partition transaction is wholly visible or
   wholly invisible at every snapshot the cluster is allowed to serve.
4. **Complete reactivity.** Every committed write intersecting a subscription's
   dependencies eventually invalidates it. Conservative extra invalidations are
   acceptable; a false negative is not.
5. **Portable identity and causality.** Document IDs, commit identities,
   read-after-write tokens, and subscription cursors keep the same meaning
   across nodes and after failover.
6. **Topology-transparent APIs.** Application code does not provide shard
   hints, choose leaders, avoid valid cross-partition transactions, or select a
   weaker read mode to obtain correct results.
7. **Normal Convex feature behavior.** Deployments, schema, indexes, scheduled
   functions, actions, file metadata, import/export, and background maintenance
   either preserve their normal behavior or fail closed with an explicit
   unsupported error. They must not silently disappear or execute twice.

## Terms That Must Stay Separate

Several earlier bugs came from using one value as proof of a different fact.
The architecture keeps these concepts separate:

| Term | Meaning | It does not prove |
| --- | --- | --- |
| Logical commit identity | Immutable identity/version assigned to one logical transaction | That every participant or replica has applied it |
| Raft commit index | A quorum accepted a command in one partition group | That the state machine applied it or another partition observed it |
| Applied position | A node durably applied a specific committed command prefix | That no earlier unresolved transaction can become visible later |
| Resolved watermark | An owner proves no missing write can appear at or below a logical version | That every other owner has resolved through that version |
| Cluster safe snapshot | A version supported by the required owners' resolved watermarks and catalog version | That a node's local latest snapshot is globally safe |
| Replication delivery position | A transport consumer has received or acknowledged messages | That database apply succeeded or the stream has no gap |
| Placement epoch | Versioned statement assigning authority to a partition/group | That the named leader is alive and applied enough to serve |
| Interest registration | A best-effort statement that a node currently wants some data | That an unregistered future read or subscription cannot need it |

## Core Invariants

Every distributed change must name the invariant IDs it affects.

| ID | Invariant | Required behavior |
| --- | --- | --- |
| `INV-01` | Single authority per epoch | Every data range and global metadata category has exactly one consensus-backed authority in a placement epoch. |
| `INV-02` | Quorum before visibility | A command is not query-visible, subscription-visible, or presented to a client as durable before its Raft group commits it. |
| `INV-03` | Deterministic idempotent apply | A committed command has a stable identity. Replay, retry, and redelivery cannot create another logical version or invalidation. |
| `INV-04` | Logical time is not apply time | Nodes preserve the origin commit identity and track physical apply progress separately. |
| `INV-05` | One snapshot per operation | All local and remote reads in one operation use the same proven logical snapshot. |
| `INV-06` | Serializable owner validation | Every read dependency is validated by the authority owning the conflicting write history through the commit version. |
| `INV-07` | Atomic transaction visibility | Multi-partition writes become visible together at permitted snapshots, never participant by participant. |
| `INV-08` | No false-negative reactivity | Every intersecting committed write reaches the authoritative invalidation path. Uncertainty over-invalidates. |
| `INV-09` | Gap-free subscription registration | Evaluation and dependency registration overlap through versioned replay or validation, leaving no missed-write window. |
| `INV-10` | Applied before serving | A newly elected or restarted node serves only after applying its committed prefix and proving current authority. |
| `INV-11` | One catalog version per request | Modules, schema, components, index definitions, and relevant config activate as one immutable version. |
| `INV-12` | Fenced side effects | Background jobs and external effects have one current owner; stale terms or epochs cannot complete work. |
| `INV-13` | Canonical lifecycle state | Bootstrap, snapshots, restore, membership, and placement transitions are versioned consensus operations. |
| `INV-14` | Fail closed without proof | Missing authority, gaps, poison data, unresolved transactions, or insufficient progress cause wait, forward, retry, rebootstrap, or rejection. |
| `INV-15` | Topology transparency | Routing, retries, ownership, and cross-partition coordination remain server responsibilities. |
| `INV-16` | Authenticated internal authority | Internal requests carry authenticated node identity and are authorized for the claimed partition, role, and epoch. |

## Authority And Source-Of-Truth Boundaries

The target architecture separates authority from transport and caching.

| Concern | Correctness authority | Transport or cache | Forbidden shortcut |
| --- | --- | --- | --- |
| Partition data | Committed Raft log plus deterministic state-machine apply | Persistence snapshots, follower state, NATS cache deltas | Treating a NATS mirror as authoritative for a strong transactional read |
| Leadership | Raft term plus current placement epoch and serving-readiness barrier | Membership discovery and forwarding caches | Serving because a process still remembers that it was leader |
| Transaction outcome | Durable transaction-status record and participant intents replicated by their Raft groups | 2PC RPC retries and watcher scans | Inferring commit from a timeout, partial participant replies, or expiring status |
| Logical versions | TSO/sequencer allocation plus immutable origin transaction identity | Local apply counters | Rewriting a transaction's identity to fit local apply order |
| Safe snapshots | Per-owner resolved watermarks combined for the required ownership/catalog set | Local latest snapshots and applied frontiers | Calling a unique timestamp or local latest timestamp globally safe |
| Placement | Versioned consensus-backed placement metadata | Static environment bootstrap and router cache | Treating static addresses or a stale map as permanent authority |
| Catalog | One staged and atomically activated catalog version | Per-node materialization caches | Routing deploy calls successfully while nodes run mixed code/schema versions |
| Reactive invalidation | Owner-sharded, versioned dependency registrations matched against committed writes | NATS invalidation delivery and selective fanout | Using recent-query interest leases as proof that omitted data cannot matter |
| Document identity | Cluster-wide table/catalog identity allocation | Local table registries | Reassigning IDs independently and repairing only top-level metadata |
| Background effects | Consensus/lease owner fenced by term or epoch | Work queues and retries | Starting the same unfenced worker on every node |
| Backup and restore | Manifest pinning one safe snapshot and one catalog/placement version | Object storage and per-partition checkpoint files | Combining independently timed partition backups |
| Internal RPC | TLS peer identity plus role/partition authorization | Service discovery | Trusting a caller-supplied identity on a reachable port |

### Component Responsibilities

#### Metadata And Control Plane

A small consensus-backed metadata authority owns cluster identity, placement
epochs, catalog versions, membership lifecycle, table-number allocation, and
global operational state. Static environment variables and NATS KV may provide
bootstrap inputs, but the final production design cannot make every process's
local configuration an independent source of truth.

#### Partition Data Plane

Each table/tablet/range is owned by a Raft group. The committed Raft log is the
ordering and durability authority. Deterministic state-machine apply creates
the visible Convex snapshot. Same-partition followers contain every state item
needed to take over after failover. Joining peers install a checkpoint bound to
a specific Raft snapshot index before they can serve.

#### Logical Version And Safe-Time Service

The TSO allocates transaction versions; it does not make transactions committed
or snapshots safe. Each partition publishes applied and resolved progress in
the immutable origin-version domain. A safe snapshot is derived only when all
owners required by the operation prove that no missing transaction can appear
at or below it.

#### Distributed Read Service

An operation chooses one snapshot `R`. Every point read, range read, index read,
and scan routes transparently to the owner or to a replica proven safe through
`R`. Dynamically discovered reads remain at `R`. A local NATS-fed mirror may be
used as a cache only when its proof is sufficient; otherwise the server waits,
forwards, or fails closed.

#### Transaction Service

Execution records read and write dependencies at `R`. A single-partition
transaction validates and commits one deterministic Raft command. A
multi-partition transaction validates read sets at their owner/resolver groups,
replicates participant intents through Raft, records one durable outcome, and
uses one commit version `C`. Early acknowledgement is allowed only when all
production reads, subscriptions, recovery, and cleanup paths understand staged
transaction status.

#### Reactive Service

After evaluating a query at `R`, the system registers its dependency ranges
with owner invalidation shards starting at `R`. The owner replays or validates
writes covering the registration interval. Every later intersecting commit
produces a versioned invalidation. The subscription reruns at a proven common
snapshot and emits a versioned result. Session failover preserves dependency
cursors rather than silently starting over.

#### Transport And Changefeed Layer

NATS carries outbox records, invalidations, CDC events, and optional cache
updates. It is not a database authority and cannot independently advance a
strong-read proof. Delivery is at least once: every consumer must detect gaps,
apply idempotently, and block freshness across poison or missing records.
Selective delivery is permitted only when uncertain interest over-delivers or
falls back to an authoritative read.

#### Catalog And Feature Services

Deployments stage immutable module, schema, component, and index metadata, then
atomically activate one catalog version. Background workers declare one of four
roles: coordinator singleton, partition owner, replica-local derived work, or
observational. Mutating and external-side-effect workers require term/epoch
fencing and idempotency.

#### Lifecycle And Operations

Membership changes use learners, catch-up, promotion, and joint consensus.
Placement movement copies a canonical snapshot, catches up, fences source
writes, and atomically changes the placement epoch. Backups pin every partition
and the catalog to one safe snapshot. Internal links authenticate nodes and
their claimed roles.

## Protocol Contracts

### Single-Partition Commit

1. Choose read snapshot `R`.
2. Route each read to its owner or a replica proven safe through `R`.
3. Validate the complete read set at the owner through proposed commit version
   `C`.
4. Propose one stable command identity to the partition Raft group.
5. Wait for quorum commit.
6. Apply the committed command deterministically and publish it to the visible
   snapshot.
7. Emit invalidation/outbox work idempotently.
8. Acknowledge with a token whose meaning survives routing and failover.

No step may expose the write before step 5. A post-quorum transport failure
cannot roll the transaction back; it must block dependent safe progress and be
replayed from durable state.

### Cross-Partition Commit

1. Execute at one snapshot `R` and include every read owner and write owner in
   validation.
2. Replicate participant intents and prepare metadata through each participant
   Raft group.
3. Record one durable transaction status and one logical commit version `C`.
4. Resolve participants idempotently from that status.
5. Permit snapshots at or beyond `C` only when the required owners can expose
   the transaction atomically.
6. Return a read-after-write token that fences every participant.

A timeout is not an abort proof. A missing decision is an unresolved state that
must be recovered or failed closed.

### Strong Read

1. Choose or refresh a cluster-safe snapshot `S`.
2. Pin the operation's catalog and placement versions.
3. Execute every dynamic read at `S` on an authoritative or proven-safe node.
4. If proof is missing, wait within a bound, forward, refresh metadata, or
   return a retryable error.

Serving a locally available stale value is never an acceptable fallback.

### Reactive Subscription

1. Evaluate the query at `R` and record exact dependency ranges.
2. Register those ranges at their owner invalidation shards from `R`.
3. Replay or validate the interval between query evaluation and registration.
4. Match every intersecting committed write after `R`.
5. Rerun at a common safe snapshot `S` and emit a versioned result.

Interest leases may reduce transport fanout, but they do not replace owner
dependency registration or gap coverage.

### Leader Failover

1. Raft elects a leader in a newer term.
2. The candidate applies the committed prefix and any bound snapshot.
3. It recovers unresolved transaction intents and durable outbox work.
4. It establishes the current placement/catalog epoch and any serving lease.
5. Only then may it accept writes, strong reads, subscriptions, or fenced
   background work.

### Catalog Activation

1. Stage immutable code, schema, component, and index metadata.
2. Validate compatibility and materialization on required nodes.
3. Commit one activation record through the catalog authority.
4. Pin each request to the activated version.

Successful routing alone is not successful deployment.

## Required Failure Behavior

| Missing or failed proof | Required response | Forbidden response |
| --- | --- | --- |
| Unknown/stale placement | Refresh, forward using a newer epoch, or reject | Guess an owner or execute locally |
| Not current Raft leader | Forward to a proven candidate or reject | Accept because local state says leader |
| No quorum | Reject/retry without visibility | Apply locally and hope to reconcile |
| Committed prefix not applied | Wait or reject serving | Serve a stale snapshot |
| Replica lag | Wait, route to owner, or return retryable error | Treat local latest as current |
| Replication gap or poison delta | Quarantine, block watermark, alert, rebootstrap | Ack/TERM and advance freshness |
| Duplicate/replayed command | Return the original outcome without another logical apply | Assign a new timestamp and apply again |
| Unknown 2PC outcome | Read durable status, recover, or wait | Infer rollback from timeout |
| Stale/missing interest | Broadcast/over-deliver or use authoritative read | Omit data and advance freshness |
| Catalog mismatch | Refresh or reject the request | Execute with mixed code/schema |
| Stale worker lease/term | Fence completion and retry on owner | Complete external effects |
| Internal identity failure | Reject and audit | Trust payload identity |

## Architectural Paths That Are Not Final Designs

These paths can be useful compatibility or bootstrap mechanisms, but must not
be treated as the end-state architecture:

1. **NATS-replicated remote state as transactional read authority.** It creates
   a second, lagging truth source. Strong reads should route to owners unless a
   replica carries a sufficient safe-snapshot proof.
2. **A node's latest timestamp as a global snapshot.** Numeric ordering or
   uniqueness does not prove every partition has resolved through that point.
3. **Timestamp rewriting as transaction identity.** Preserve immutable origin
   identity and track local apply position separately.
4. **Selective delivery as correctness.** Recent subscriptions/queries cannot
   predict future mutation reads. Uncertainty must over-deliver.
5. **Deploy routing without atomic activation.** Reaching the coordinator does
   not prevent mixed module/schema/index versions.
6. **Independent local bootstrap before Raft.** Consensus replicas must derive
   state from one canonical genesis or snapshot.
7. **Parallel-2PC early acknowledgement before staged reads.** Durable staging
   is insufficient unless every read, subscription, failover, and recovery path
   understands staged status.
8. **Static addresses and maps as a production control plane.** They are valid
   bootstrap inputs and local-test conveniences, not dynamic membership or
   placement authority.
9. **Passing happy-path Docker tests as a semantic proof.** The harness is
   valuable regression coverage, but timing-sensitive protocols also require
   deterministic fault tests and history checking.

## Current Gap Map

The dependency-ordered implementation roadmap is issue `#289`. Major target
gaps include:

| Target gap | Tracking issues | Invariants |
| --- | --- | --- |
| Canonical Raft bootstrap, replay, snapshot, failover readiness | `#277`, `#274`, `#275`, `#276`, `#279` | `INV-02`, `INV-03`, `INV-10`, `INV-13`, `INV-14` |
| Authoritative reads and safe snapshots | `#285`, `#252`, `#278`, `#280` | `INV-04`, `INV-05`, `INV-06`, `INV-07`, `INV-15` |
| Catalog and normal feature behavior | `#286`, `#282`, `#250`, `#111` | `INV-11`, `INV-12`, `INV-14` |
| Distributed reactive correctness | `#132`, `#133`, `#74`, `#99` | `INV-05`, `INV-08`, `INV-09` |
| Dynamic lifecycle and recovery | `#283`, `#251`, `#104`, `#130`, `#287` | `INV-01`, `INV-10`, `INV-13`, `INV-15` |
| Internal data-plane identity | `#284` | `INV-16` |
| Semantic release proof | `#288` | All |

## Audit Issue Map: #240 Through #256

Closing one issue proves its named regression, not the entire invariant. Open
items remain release blockers for the affected behavior.

| Issue | State | Protects | Work type and required proof |
| --- | --- | --- | --- |
| `#240` ordered Raft-to-NATS outbox | Closed | `INV-03`, `INV-04`, `INV-14` | Code plus publish-failure ordering test |
| `#241` stale TSO batches after leadership change | Closed | `INV-01`, `INV-04`, `INV-06` | Code plus re-election timestamp-floor test |
| `#242` participant-aware read-after-write tokens | Closed | `INV-05`, `INV-07` | Code plus delayed-participant cluster test |
| `#243` bounded replica dedup metadata | Closed | `INV-03` | Code plus sequential/straggler pruning tests |
| `#244` resolved 2PC decision garbage collection | Closed | `INV-07`, `INV-13` | Code plus restart and retention tests |
| `#245` per-entry durable outbox | Closed | `INV-03`, `INV-14` | Code plus backlog ordering/stress tests |
| `#246` fair 2PC prepare admission | Closed | `INV-06`, `INV-07`, `INV-14` | Code plus concurrent local/cross-partition load test |
| `#247` Raft apply worker separation | Closed | `INV-02`, `INV-03`, `INV-10` | Code plus stalled-apply election/heartbeat test |
| `#248` commit hot-path serialization | Open | `INV-02`, `INV-03` | Performance code and benchmarks gated by all commit invariants |
| `#249` bounded deferred commit-prepared retry | Open | `INV-07`, `INV-14` | Code plus stuck-participant deadline fault test |
| `#250` scheduled action handoff fencing | Open | `INV-12` | Protocol/code plus leadership-handoff side-effect test |
| `#251` clock-skew-safe membership liveness | Open | `INV-01`, `INV-10`, `INV-13` | RFC/code plus skew simulation |
| `#252` remote read validation for local system writes | Open | `INV-06`, `INV-14` | Code plus owner-write race history test |
| `#253` fail-loud catalog parsing | Closed | `INV-01`, `INV-11`, `INV-14` | Code plus malformed-metadata unit tests |
| `#254` index consistency after commit rebase | Closed | `INV-03`, `INV-11` | Code plus rejected-commit/index-overlap test |
| `#255` blocked-delta redelivery backoff | Closed | `INV-03`, `INV-14` | Code plus retry cadence and eventual-apply test |
| `#256` internal gRPC rotation and comparison | Closed | `INV-16` | Code plus missing/invalid/rotated credential tests |

## Work Classification

Not every gap should start with a code patch. The implementation sequence uses
four classes:

| Class | Meaning | Current examples |
| --- | --- | --- |
| Known code correction | The authority and invariant are already clear; implementation violates them | `#274`, `#275`, `#276`, `#277`, `#279`, `#249`, `#252`, `#280`, `#282`, `#250` |
| Protocol RFC before code | Multiple correct designs have different costs; define state, epochs, recovery, and proof before implementation | `#278`, `#132`, `#286`, `#283`, `#104`, `#130`, `#287`, `#284` |
| Executable semantic proof | Convert the invariant into histories, traces, fault schedules, and release gates | `#288`, plus the dangerous interleaving required by every issue |
| Blocked optimization | Keep disabled or conservative until its dependency invariants have executable proof | `#281`, `#248`, `#74`, `#99` |

An RFC is not completion. A code change is not completion without the required
proof. A benchmark is not accepted when its semantic gate fails.

## Evidence Required Before Merge

The required evidence depends on the failure surface. More tests are not a
substitute for the right test.

| Layer | Use for | Examples |
| --- | --- | --- |
| Unit/property tests | Serialization, stable identity, state transitions, idempotency | Duplicate command, monotonic watermark, digest stability |
| Deterministic protocol tests | Crash windows, reordered messages, persistence/apply boundaries | Kill after hard-state persist, replay after apply-marker loss |
| Cluster integration tests | Real routing, process restart, NATS/Raft wiring | Leader failover, participant outage, retention gap |
| History checking | Serializable/linearizable observable histories | Elle transaction histories under faults |
| Subscription trace checking | No false-negative invalidations or registration gaps | Commit/evaluate/register/replay interleavings |
| Real infrastructure soak | Tail latency, disk/network faults, operational recovery | Multi-zone deployment, rolling replacement, long backlog |

Every correctness PR must state:

- the invariant IDs it changes;
- the authoritative state before and after the change;
- the exact point at which data becomes visible and acknowledged;
- retry, duplicate, reordering, restart, and stale-leader behavior;
- how missing proof fails closed;
- which executable test forces the dangerous interleaving rather than merely
  exercising the happy path.

Use the repository pull-request checklist in
`.github/pull_request_template.md` during review.

## Definition Of Done

The project is complete when the same generated application workload can run
against upstream-style single-node Convex and this cluster and produce
equivalent:

- committed transaction histories;
- query snapshots;
- subscription results and ordering;
- document identities and read-after-write behavior;
- deployment/schema/index behavior;
- scheduled/background feature outcomes;
- backup and restore contents.

Allowed differences are latency, throughput, operator-visible topology, and an
explicit fail-closed availability error when the required quorum or proof is
unavailable. A performance result is not accepted unless the semantic
conformance gate in issue `#288` also passes.
