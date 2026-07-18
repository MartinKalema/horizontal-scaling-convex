## Summary

<!-- What changed, why, and what user/operator behavior changes? -->

## Issue

<!-- Use "Fixes #..." only when every acceptance criterion is complete. -->

## Distributed Correctness Review

Complete this section for changes touching Raft, persistence, 2PC, timestamps,
replication, routing, placement, catalog state, background workers, reads, or
subscriptions. For an item that does not apply, mark it and briefly explain why.
The invariant definitions live in
[`docs/distributed-correctness-model.md`](https://github.com/MartinKalema/horizontal-scaling-convex/blob/dev/docs/distributed-correctness-model.md).

### Contract

- [ ] I listed the affected invariant IDs (`INV-01` through `INV-16`).
- [ ] I identified the authoritative state before and after this change.
- [ ] I identified the placement epoch, Raft term, catalog version, or
      transaction identity that fences stale work.
- [ ] Public Convex APIs remain topology-transparent, or the path fails closed.

Affected invariants and authority:

<!-- Example: INV-02, INV-03. Partition Raft log remains authoritative. -->

### Visibility And Time

- [ ] Nothing becomes query-visible, subscription-visible, or acknowledged as
      durable before its required consensus decision.
- [ ] Logical commit identity remains separate from local apply progress.
- [ ] Reads and subscription reruns use one proven snapshot across every owner.
- [ ] Cross-partition transactions remain wholly visible or wholly invisible.

Visibility/acknowledgement point:

<!-- State the exact durable event that permits visibility and client ack. -->

### Replay And Failure

- [ ] Duplicate, delayed, reordered, or replayed messages are idempotent.
- [ ] Restart between persistence, consensus commit, apply, and acknowledgement
      cannot lose or double-apply accepted work.
- [ ] A new leader cannot serve before applying the committed prefix.
- [ ] Gaps, poison data, unknown authority, unresolved status, and stale
      metadata wait, forward, retry, rebootstrap, or fail closed.
- [ ] Post-quorum transport failure has a durable recovery path.

Dangerous interleaving tested:

<!-- Describe the exact crash/message/order window forced by a test. -->

### Transactions And Reactivity

- [ ] Every read dependency is validated by its owner through the commit
      version, including dynamically discovered and system-table cases.
- [ ] 2PC prepare/status/cleanup survives participant and coordinator failover.
- [ ] Subscription evaluation and dependency registration have no uncovered
      commit interval.
- [ ] Missing or stale selective interest over-delivers; it never suppresses a
      correctness-critical write or invalidation.

### Catalog, Workers, And Security

- [ ] Requests cannot execute against mixed module/schema/index catalog
      versions.
- [ ] Mutating/background/external-side-effect workers have explicit ownership,
      idempotency, and term/epoch fencing.
- [ ] Internal requests authenticate node identity and authorize the claimed
      role/partition.

## Validation

<!-- Include exact commands, counts, fault injection, and local image digest. -->

- [ ] Focused unit/property tests
- [ ] Deterministic protocol/fault tests where applicable
- [ ] Full relevant Rust suite
- [ ] Clean locally built Docker image verified on every backend container
- [ ] Full write-scaling and Raft failover harness where applicable
- [ ] Semantic/history/subscription trace checker where required

## Remaining Risk

<!-- State what this PR does not prove and link follow-up issues. -->
