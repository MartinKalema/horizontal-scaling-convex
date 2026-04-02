# TSO Batch Depletion Fix: Replica Deltas Must Not Consume Global Timestamps

## The Problem

Under concurrent writes from both nodes, Node B crashes with:

```
Timestamp(1775106222636772512) >= 1775106222636772511
```

The write log requires strictly increasing timestamps. A replica delta application produced a timestamp equal to (not greater than) the current max.

## Root Cause

`apply_replica_delta()` called `next_commit_ts()`, which draws from the shared BatchTimestampOracle (TSO). Both local commits AND replica delta applications consumed timestamps from the same TSO batch.

Under 20 concurrent writes per node:

1. Node A commits locally → TSO gives timestamp 100
2. Node A publishes delta to NATS
3. Node B receives delta, calls `next_commit_ts()` → TSO gives timestamp 101
4. Node B commits locally → TSO gives timestamp 102
5. Node B receives another delta, calls `next_commit_ts()` → TSO gives 103
6. ...rapidly, the TSO batch (1000 entries) gets consumed by a mix of local commits and replica delta applications
7. Eventually the TSO batch is exhausted, and `next_commit_ts()` returns a value that equals `last_assigned_ts` from a `max_repeatable_ts` bump → panic

Replica delta applications are not new writes. They're replayed writes that already happened on another node. Consuming TSO entries for replays wastes the finite batch and creates contention with real local commits.

## How CockroachDB, TiDB, and Vitess Handle This

### CockroachDB: Local HLC on apply, not proposer's timestamp

CockroachDB uses Raft for replication. When a Raft entry is proposed (by the leaseholder), it gets an HLC timestamp. When a follower applies the entry to its state machine, it advances its local HLC using the command's timestamp — it does not generate a new one.

From their design doc: "update the local HLC with the command's timestamp." Followers observe and advance to the proposer's timestamp. They don't consume a new HLC value.

Source: [CockroachDB design.md](https://github.com/cockroachdb/cockroach/blob/master/docs/design.md)

### TiDB: TSO only for transaction start/commit, not for apply

TiDB's TSO (via PD) allocates timestamps at two points only: `startTS` when a transaction begins, and `commitTS` when it commits. The Raft apply loop on followers does NOT request new TSO timestamps. Followers apply committed log entries using the already-assigned `commitTS` from the leader.

PD explicitly handles batch depletion: "when the logical clock reaches its threshold value during the time services, it stops and waits." They prevent depletion by separating who consumes TSO entries — only leaders/coordinators, never followers applying replicated entries.

Source: [TiDB TSO time services](https://www.pingcap.com/blog/how-an-open-source-distributed-newsql-database-delivers-time-services/)

### Vitess: Each shard applies with MySQL's own GTID

Vitess uses MySQL's GTID for replication within a shard. Cross-shard replication via VReplication doesn't generate new GTIDs on the target — it replays with the source's positioning. The target shard's MySQL assigns its own local values.

Source: [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)

### The Universal Pattern

All three systems follow the same rule: **the global timestamp allocator is consumed only by the node originating the write, never by nodes replaying it.**

| System | Who consumes global timestamps | What replicas use on apply |
| --- | --- | --- |
| CockroachDB | Leaseholder (HLC on propose) | Advance local HLC to command's timestamp |
| TiDB | Leader (TSO on commit) | Apply with the leader's commitTS |
| Vitess | Source shard (MySQL auto-increment) | Target replays with source positioning |
| Convex (fixed) | Local Committer (TSO on commit) | Local clock + monotonic counter |

## Our Fix

Before:

```rust
// apply_replica_delta called next_commit_ts() which consumed TSO batch entries
let commit_ts = self.next_commit_ts()?;
```

After:

```rust
// Monotonic counters only: no TSO, no system clock.
// Stays in the same timestamp domain as next_commit_ts().
let latest_ts = self.snapshot_manager.read().latest_ts();
let commit_ts = cmp::max(latest_ts.succ()?, self.last_assigned_ts.succ()?);
self.last_assigned_ts = commit_ts;
```

No TSO (reserved for local commits), no system clock (would leap ahead of TSO range and poison `last_assigned_ts`). Only the two monotonic counters that stay in the same domain as `next_commit_ts()`: the snapshot manager's latest timestamp and the last assigned timestamp.

## Why System Clock Was Also Wrong (Second Fix)

The first attempt replaced the TSO call with the single-node formula: `max(latest_ts + 1, system_clock, last_assigned_ts + 1)`. This still crashed under concurrent load. The reason:

1. The TSO batch is allocated from the system clock at startup: `~1775107167065...`
2. By the time concurrent writes start, the system clock has advanced to `~1775107167847...`
3. A replica delta uses `runtime.generate_timestamp()` → gets `~1775107167847...`
4. `last_assigned_ts` jumps to `~1775107167847...` (far above the TSO batch range)
5. Next local commit: TSO gives `~1775107167065528382` (from the batch), but `last_assigned_ts + 1` is `~1775107167847...+1` — uses the higher value
6. `next_max_repeatable_ts()` calls `next_commit_ts()` → also forced to use `last_assigned_ts + 1`
7. Both paths now increment the same poisoned counter → eventually collide

**Two different timestamp sources (TSO batch and system clock) produce values in different ranges.** Mixing them in the same `last_assigned_ts` counter corrupts the monotonic ordering.

### Confirmed by CockroachDB, TiDB, and etcd

All three systems follow the same rule on their apply paths:

**CockroachDB** — From the [follower reads RFC](https://github.com/cockroachdb/cockroach/blob/master/docs/RFCS/20180603_follower_reads.md): followers "can start serving reads with timestamps at or below <timestamp> as soon as they've applied all the Raft commands through <log position>." The closed timestamp is "propagated from the range's leaseholder to its followers via the raft log." Followers advance to the leaseholder's timestamp — they do not generate new timestamps. The timestamp is embedded in the Raft command by the proposer. No HLC generation on apply.

**TiDB/TiKV** — From [How TiKV Reads and Writes](https://tikv.org/blog/how-tikv-reads-writes/): "When Prewrite finishes, a new timestamp is obtained from TSO and is set as commitTS." Only at prewrite/commit time, by the coordinator. "When the Leader finds that the entry has been appended by majority of nodes, it considers that the entry Committed. Then it can apply them to the state machine." The apply step uses the already-assigned commitTS. No new TSO allocation during apply.

**The universal rule**: followers/replicas apply entries using the originator's timestamp or monotonic advancement from it. They never allocate new timestamps from the global oracle OR the system clock during the apply phase. This prevents:

1. Global counter waste (TSO batch depletion)
2. Timestamp domain mixing (system clock vs TSO range)
3. Contention between apply and local commit paths

### The Correct Fix

Use only monotonic counters that stay in the same domain as `next_commit_ts()`:

```rust
let commit_ts = cmp::max(latest_ts.succ()?, self.last_assigned_ts.succ()?);
```

No TSO. No system clock. These two values are always in the same numeric range as TSO-assigned timestamps because they're derived from the same `last_assigned_ts` counter that `next_commit_ts()` writes to.

## Third Fix: Async bump_max_repeatable_ts Racing with apply_replica_delta

Even with TSO and system clock removed from the delta apply path, crashes continued under concurrent load:

```
Timestamp(1775107897167326910) >= 1775107897167326909
```

### Root Cause

Two paths write to the write log:

1. **`bump_max_repeatable_ts`** → assigns timestamp N via `next_commit_ts()` → async persistence write → eventually `publish_max_repeatable_ts(N)` → appends N to write log
2. **`apply_replica_delta`** → synchronously assigns timestamp N+1 → appends N+1 to write log

The race: `bump_max_repeatable_ts` assigns N, then the persistence write goes to `FuturesOrdered`. Before it completes, `apply_replica_delta` runs, appends N+1 to the log, and calls `bump_persisted_max_repeatable_ts(N+1)`. When the persistence write finally completes, `publish_max_repeatable_ts(N)` calls `bump_persisted_max_repeatable_ts(N)` which checks `N >= N+1` → assertion failure.

### How CockroachDB and TiKV Solve This

**CockroachDB** separates the closed timestamp from write application using two distinct transport mechanisms:

- **Raft transport**: Carries actual writes. Applied in strict log order.
- **Side transport**: Carries closed timestamp updates separately. "Simply refuses to publish an update for a particular range if that range's evaluation timestamp is below the target timestamp." The side transport never races with in-progress writes.

Source: [CockroachDB Follower Reads](https://www.cockroachlabs.com/blog/follower-reads-stale-data/)

**TiKV** derives `resolved-ts` from the apply state itself:

- The resolver tracks locks "by receiving change logs when Raft applies" — the resolved timestamp is computed FROM the apply state, not an independent async timer.
- `safe-ts` (per-peer) is always `<= resolved-ts` (leader-only), ensuring monotonicity.

Source: [TiDB Stale Read and safe-ts](https://docs.pingcap.com/tidb/stable/troubleshoot-stale-read/)

### The Fix

Remove `bump_persisted_max_repeatable_ts` from `apply_replica_delta`. That function is for the async bump path (CockroachDB's side transport equivalent). The delta apply path only pushes the snapshot. The existing bump timer handles repeatable timestamp advancement on its next cycle.

```rust
// Phase 5: Push updated snapshot.
// Do NOT call bump_persisted_max_repeatable_ts here.
let mut sm = self.snapshot_manager.write();
sm.push(commit_ts, snapshot, delta.write_bytes);
// bump timer will advance repeatable ts on its next cycle
```

## Sources

- [CockroachDB design.md — Raft apply and HLC](https://github.com/cockroachdb/cockroach/blob/master/docs/design.md)
- [CockroachDB Replication Layer](https://www.cockroachlabs.com/docs/stable/architecture/replication-layer)
- [CockroachDB Follower Reads RFC — closed timestamp propagation](https://github.com/cockroachdb/cockroach/blob/master/docs/RFCS/20180603_follower_reads.md)
- [CockroachDB Follower Reads Blog — timestamp advancement](https://www.cockroachlabs.com/blog/follower-reads-stale-data/)
- [TiDB TSO time services — batch depletion handling](https://www.pingcap.com/blog/how-an-open-source-distributed-newsql-database-delivers-time-services/)
- [TiKV How reads and writes work — apply uses leader's commitTS](https://tikv.org/blog/how-tikv-reads-writes/)
- [TiKV Percolator deep dive — TSO only at prewrite/commit](https://tikv.org/deep-dive/distributed-transaction/percolator/)
- [TiKV Raft in TiKV — apply worker](https://www.pingcap.com/blog/raft-in-tikv/)
- [TiDB Raft-based replication](https://www.mydbops.com/blog/tidbs-raft-based-replication)
- [Vitess Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)
