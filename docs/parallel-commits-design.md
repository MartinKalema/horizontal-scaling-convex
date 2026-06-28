# Convex-Safe Parallel Commits

## Status

Design and rollout notes for issue `#69`.

This document is intentionally conservative. It describes what is required to
turn `#69` into true CockroachDB-style parallel commits without weakening the
properties that make Convex feel like Convex.

The current codebase already has durable-decision 2PC, Raft-backed participant
prepare records, abandoned-prepare rollback, retained decision records,
same-partition Raft failover, staged decision metadata, staged status recovery,
and staged read/subscription policy hooks. The default runtime path remains the
conservative durable-decision protocol.

Issue `#69` now has a guarded early-ack implementation behind
`ENABLE_PARALLEL_2PC_EARLY_ACK=false` by default. When enabled, the coordinator
acknowledges a cross-partition transaction after it writes a complete durable
`Staging` record, recovers that proof into `Committed`, and schedules ordinary
participant cleanup asynchronously. This preserves the durable-decision path as
the production fallback while we continue hardening the staged cleanup and
read-after-write windows.

## Summary

True parallel commit changes the point where a cross-partition transaction can
be considered committed.

Today, the transaction is committed only after the coordinator writes a durable
`Committed` decision and participants apply `CommitPrepared`.

With true parallel commit, the transaction can be considered committed once the
system can prove:

1. a durable transaction record exists in a `Staging` state;
2. the record lists every participant and every staged write/intent that must
   exist for the transaction;
3. every listed participant intent has reached that participant partition's
   Raft quorum and can be recovered after leader failover;
4. transaction status recovery can deterministically convert that proof into a
   committed transaction if the coordinator dies before cleanup.

Only after those conditions hold may the coordinator acknowledge the client
before final `CommitPrepared` cleanup has completed everywhere.

## Why Fanout Is Not Parallel Commit

There are three different protocols that are easy to confuse:

| Protocol | Client ACK condition | Safety shape | Latency impact |
| --- | --- | --- | --- |
| Current durable-decision 2PC | Durable `Committed` decision, then participant commit path | Simple and conservative | Two synchronous phases |
| PR `#205` participant fanout | Same as current 2PC, but prepare/commit RPCs fan out concurrently | Same commit semantics as current 2PC | Removes serial participant waiting, not a protocol round trip |
| True parallel commit | Durable staging record plus all listed participant intents durably replicated | Needs status recovery and staged-write read semantics | Can remove the synchronous final decision/cleanup round trip |

PR `#205` is therefore safe performance work, not the end of issue `#69`.

## Non-Negotiable Convex Invariants

Parallel commits must preserve the project goals in `docs/project-goals.md`.

The protocol must not weaken:

- serializable TypeScript mutations with OCC and automatic retry;
- atomic visibility across all partitions touched by a transaction;
- consistent snapshots for latest queries, timestamped queries, exports, and
  subscriptions;
- exact reactive invalidation for every committed write;
- read-after-write for clients that issue a mutation and then query;
- one logical database API with no partition hints in user code.

If any part of the protocol cannot prove these properties, it must fail closed
or fall back to the existing durable-decision 2PC path.

## Existing Baseline

### Current Durable-Decision 2PC

The current model follows a Vitess-style durable decision:

1. The coordinator classifies the transaction as local, remote-single-partition,
   or cross-partition.
2. For cross-partition work, the coordinator assigns one global `prepare_ts`.
3. Each participant validates its slice, stages hidden prepared writes, persists
   a redo record, and replicates that redo record through its partition's Raft
   group before acknowledging prepare.
4. If all prepares succeed, the coordinator writes a durable `Committed`
   decision in the 2PC decision log.
5. Participants receive `CommitPrepared`, replicate/apply the final commit, then
   publish visibility through the normal write-log and snapshot path.
6. If prepare fails or times out, the coordinator/watcher writes a durable
   rollback decision and participants remove their prepared intents.

This is safe because the final decision is the point of no return. It is also
slow because the final decision and final participant commit phase are on the
client-visible path.

### PR `#205` Fanout

PR `#205` keeps the same durable-decision protocol but removes unnecessary
serial waiting:

- participant prepares are issued concurrently;
- participant commits are issued concurrently after the durable decision;
- prepare failures are collected before deciding whether to retry or roll back.

The commit predicate does not change. A client is not acknowledged based on a
staging proof. This is why PR `#205` should be treated as a safe precursor, not
as full CockroachDB parallel commits.

## Target Protocol

### New Transaction States

The 2PC metadata model should grow from `Committed` / `RolledBack` to an
explicit staged lifecycle:

| State | Meaning | Durable authority |
| --- | --- | --- |
| `Staging` | The coordinator is attempting a parallel commit. The record lists the transaction id, commit timestamp, participants, participant write descriptors, placement version, and transaction digest. | 2PC transaction log, initially the current NATS KV decision log; long term this should be allowed to move behind a replicated coordinator-partition control log. |
| `Committed` | Status recovery or the coordinator proved that all staged participant intents exist. Cleanup may still be incomplete. | CAS transition from `Staging` or direct durable-decision path. |
| `RolledBack` | The transaction definitely did not reach the commit proof. | CAS transition from `Staging` or prepare-failure path. |
| `Resolved` | Every participant has durably applied commit/rollback cleanup. The record may be deleted. | `resolved_participants` reaches the full participant set. |

The important new state is `Staging`. It is not visible user data. It is a
recoverable proof object.

### Staging Record Contents

The staging record must contain enough information for a different node to
recover status without trusting coordinator memory:

- transaction id;
- commit timestamp;
- coordinator partition and placement version;
- full participant set;
- per-participant write descriptor digest;
- per-participant read-set descriptor needed for validation/recovery;
- transaction digest covering the final transaction shape;
- creation time and retry metadata;
- resolved participant set for cleanup progress.

The record must be written with compare-and-swap semantics and no fixed TTL. It
cannot be a best-effort hint. Once a client may have seen success, the recovery
proof must outlive process crashes and participant outages.

### Participant Intent

Each participant needs a durable staged intent separate from ordinary visible
state. The existing prepared transaction machinery is close, but true parallel
commit requires the intent to be explicitly part of the commit proof:

- keyed by transaction id and commit timestamp;
- contains the participant transaction slice or a digest plus recoverable redo;
- hidden from ordinary snapshot readers;
- visible to OCC conflict checks as a pending write;
- replicated through the participant Raft group before the participant returns
  a stage acknowledgement;
- idempotent when the same stage request is delivered more than once;
- removable only after commit/rollback cleanup reaches the participant.

The participant acknowledgement means: "this intent is durable on a Raft quorum
and a future leader can recover it." It must not mean only "the current leader
has an in-memory prepared transaction."

### Commit Proof

A parallel transaction is committed if and only if:

```text
StagingRecord(txn_id, commit_ts, participants, descriptors) exists
AND for every participant P in participants:
    DurableParticipantIntent(P, txn_id, commit_ts, descriptor[P]) exists
    AND that intent reached P's Raft quorum
```

This proof must be checkable by transaction status recovery after the
coordinator dies.

If a participant returns a definite validation/conflict failure before staging,
the transaction can be rolled back. If a participant result is ambiguous after
the staging record exists, the coordinator must not guess. It should leave or
CAS the record into a recoverable state and let status recovery inspect the
participants.

### Client ACK Rule

The coordinator may acknowledge success before final cleanup only when it has
observed the complete commit proof.

This is the first place where Convex differs sharply from a plain SQL/KV
database: an acknowledged mutation is commonly followed immediately by a query
or subscription update. Therefore, early ACK is allowed only after the read and
subscription paths can handle committed-but-not-cleaned-up staged writes.

The production default remains disabled. The system can use the staging
machinery internally, but ordinary clusters should continue waiting for normal
`CommitPrepared` visibility before returning success unless
`ENABLE_PARALLEL_2PC_EARLY_ACK=true` is explicitly set for validation.

### Cleanup

After the commit proof exists, cleanup is asynchronous:

1. status is transitioned to `Committed`;
2. every participant receives `CommitPrepared`;
3. each participant applies the final commit through Raft, persistence, write
   log, and snapshot publication;
4. each participant is marked resolved with CAS;
5. the transaction record is deleted only after all participants resolve.

Rollback cleanup follows the same shape using `RollbackPrepared`.

## Read And Subscription Semantics

This is the part that protects Convex.

### Staged But Not Proven

If a write is staged but the commit proof is incomplete:

- ordinary reads must not see it;
- subscriptions must not invalidate from it as committed data;
- OCC validation must treat it as a pending conflicting write;
- reads that encounter it may trigger status recovery, wait, or fail closed.

### Proven Committed But Not Cleaned Up

If status recovery or the coordinator can prove the transaction committed but
participant cleanup is incomplete:

- latest reads must not return a snapshot that excludes the committed write if
  the client's requested freshness requires it;
- timestamped reads at or after `commit_ts` must resolve or wait for the
  committed staged write;
- subscriptions must eventually receive the same invalidation they would have
  received from the ordinary commit path;
- duplicate cleanup must be idempotent.

There are two viable implementation choices:

1. **Resolve-before-read:** a read that intersects a staged committed intent
   drives cleanup before returning.
2. **Intent-aware read:** snapshot reads can overlay committed staged intents
   without waiting for cleanup.

The safer first implementation is resolve-before-read. It is slower during the
rare ambiguous window, but it preserves the existing snapshot model and avoids
teaching every query path to merge provisional values.

### Subscription Invalidation

Subscription invalidation must be emitted exactly once for the committed
transaction, and only after the commit proof exists. The preferred first design
is:

- no invalidation at participant staging time;
- cleanup publishes the normal commit delta;
- if cleanup is delayed, the subscription manager or read path may trigger
  recovery, but it must not publish speculative results.

This preserves Convex's "clients see committed database state" model.

## Failure Handling

### Coordinator Crash Before All Intents Stage

The staging record exists, but the commit proof may be incomplete. Recovery
checks participants:

- if every listed intent exists, resolve `Committed`;
- if at least one listed intent definitely does not exist and no client success
  could have been returned, resolve `RolledBack`;
- if a participant cannot be reached, keep the record staged and retry.

### Coordinator Crash After Client ACK

This is the reason the commit proof must be durable. Recovery must be able to
prove committed status from the staging record plus participant intents, then
finish cleanup.

### Participant Leader Failover After Staging

The new leader must recover the participant intent from Raft-applied redo and
be able to answer status-recovery probes. A stage ACK before Raft quorum is
therefore invalid.

### Duplicate Stage Or Cleanup RPC

All participant RPCs must be idempotent by transaction id and commit timestamp.
Duplicate `StageParticipant`, `CommitPrepared`, `RollbackPrepared`, and recovery
probes must return the same logical outcome or a safe already-resolved outcome.

### NATS / Decision Log Interruption

If the transaction status log is unavailable before staging, fail closed. If it
is unavailable after staging, participants keep their intents and recovery
resumes when the log is available. No participant should infer commit from local
state alone.

## Relationship To Existing Issues

- `#69` remains the umbrella for CockroachDB-style parallel commits.
- `#205` added safe parallel participant fanout inside the durable-decision
  protocol.
- `#207` added the durable `Staging` record and participant intent proof.
- `#208` added transaction status recovery.
- `#209` made reads, OCC, and subscriptions staged-write-aware.
- `#210` added adversarial tests for 2PC/Raft/NATS ambiguity windows.
- `#131` remains the broader resolver-style conflict checking work. Parallel
  commits must not make current read validation weaker while that evolves.
- `#134` remains the long-term deterministic simulation goal.

## Implementation Sequence

1. Add metadata types only: `Staging` decision state, participant descriptors,
   transaction digest, and serialization tests.
2. Add idempotent participant staging RPCs backed by the existing prepared
   intent and Raft redo path.
3. Add transaction status recovery that can inspect staged participant intents
   and CAS `Staging` to `Committed` or `RolledBack`.
4. Keep client ACK after ordinary `CommitPrepared` cleanup while staging and
   recovery mature.
5. Teach read, OCC, and subscription paths to handle staged committed intents.
6. Add cluster fault-injection tests for every ambiguous window.
7. Enable early ACK behind `ENABLE_PARALLEL_2PC_EARLY_ACK` only after the tests
   prove no torn visibility, stale read-after-write, or speculative
   subscription result.

## Test Gates

True parallel commits should not be enabled until these pass:

- unit tests for metadata serialization, digest stability, and CAS transitions;
- participant tests for duplicate staging, duplicate cleanup, and restart;
- recovery tests for all staged/committed/rolled-back transitions;
- Raft failover tests where a participant leader dies after staging;
- coordinator crash tests before and after client ACK;
- latest-query and timestamped-query tests around staged writes;
- subscription tests proving no speculative invalidation and no missed
  committed invalidation;
- full Docker write-scaling and Raft failover suites on a freshly rebuilt image;
- `ENABLE_PARALLEL_2PC_EARLY_ACK=true BACKEND_PULL_POLICY=never bash
  self-hosted/docker/test.sh parallel` on a freshly rebuilt image, which
  requires every backend container to run with the early-ack knob enabled and
  verifies the `parallel-committed` coordinator path during the failure-window
  2PC checks;
- adversarial soak tests with repeated elections, NATS interruptions, and
  duplicate RPC delivery.

## Explicit Non-Goals

- Do not expose partitions, staging states, or placement versions to user
  TypeScript APIs.
- Do not let application developers opt into weaker consistency for speed.
- Do not publish subscription results from abortable staged writes.
- Do not delete the existing durable-decision 2PC fallback until true parallel
  commits has equivalent recovery coverage.

## References

- CockroachDB Parallel Commits:
  https://www.cockroachlabs.com/blog/parallel-commits/
- CockroachDB transaction layer:
  https://www.cockroachlabs.com/docs/stable/architecture/transaction-layer
- Vitess distributed transactions:
  https://vitess.io/docs/22.0/reference/features/distributed-transaction/
- Convex project goals:
  `docs/project-goals.md`
- Existing 2PC design:
  `docs/two-phase-commit.md`
