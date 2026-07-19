# Cluster-Safe Read Timestamps

Convex queries must observe one database snapshot. A horizontally scaled query
must not combine a new value from one partition with an old value from another
partition, including while a two-phase commit is finishing.

## Three Timestamp Meanings

The cluster intentionally distinguishes three timestamps:

1. **Origin commit timestamp**: the TSO timestamp assigned once to the logical
   write by its source partition. It remains the replication identity used for
   deduplication and source-partition progress.
2. **Local apply timestamp**: the timestamp at which a receiving node installs
   that write in its own MVCC timeline. Replica apply may translate the origin
   timestamp upward to preserve strict local ordering.
3. **Client snapshot timestamp**: an exact timestamp certified by every current
   partition owner for one placement version. Latest queries and subscription
   reruns use this timestamp.

An origin timestamp is not automatically a safe client snapshot. A local
replication frontier or `max_repeatable_ts` is also not a cluster-wide proof.

## Exact Read Barrier

For a latest clustered read, the coordinator performs this protocol:

1. Pin the current placement map and its version.
2. Start with the coordinator's current local readable timestamp as candidate
   `R`. Read barriers do not allocate write timestamps.
3. Ask every partition's current Raft serving leader to close its local MVCC
   timeline at exactly `R`.
4. Each owner returns one of:
   - `Closed(R)`: all earlier ordered persistence work is complete, no prepared
     transaction can cross `R`, and future live commits must be greater than
     `R`.
   - `RetryAt(T)`: the owner's translated local timeline is already above `R`.
     The coordinator raises the common candidate to the greatest returned
     owner floor and retries every owner.
   - `Blocked(P)`: an unresolved 2PC participant is prepared at `P <= R`. The
     coordinator waits for that decision and retries the same candidate.
5. Return `R` only if every owner returns `Closed(R)` and the placement version
   remains unchanged.

Closure is inserted into the Committer's ordered persistence pipeline. It is
therefore ordered after writes already admitted by that owner. The persisted
repeatable timestamp also forces future leaders and future live prepares above
the barrier. A leader or serving-lease change during closure rejects the
barrier.

The coordinator currently closes all partitions because an arbitrary Convex
JavaScript query can discover its read set while it runs. Once `R` is certified,
local ranges are read locally and remote ranges are fetched from their
authoritative owners at the same `R`. The system does not depend on every local
NATS mirror having caught up to serve this read.

The negotiation itself uses the cached cluster membership snapshot and direct
owner gRPC calls. It does not contact the NATS-backed timestamp oracle, so
latest reads remain available during a NATS outage while the Raft owners and
their previously discovered addresses remain reachable.

Idle frontier heartbeats and background repeatable-timestamp advancement also
cannot turn into a hidden TSO dependency for reads. Optional maintenance may
consume an already-reserved local TSO value, but batch exhaustion only starts a
coalesced asynchronous refill and schedules maintenance to retry. It never
waits for NATS from the single-threaded committer loop. Live writes continue to
use the normal fail-closed TSO allocation path.

## 2PC Atomicity

The barrier handles every relevant 2PC state:

- If no participant has committed, all owners can close before the transaction
  or report the unresolved prepare as blocked.
- If some participants have committed and another remains prepared, the
  unresolved owner returns `Blocked`; no query starts.
- After the remaining participant commits, every owner closes at one timestamp
  above the complete transaction.
- Once a timestamp is closed, a late live prepare at or below it is rejected.

As a result, a latest query or reactive rerun cannot observe only one half of a
2PC transaction.

## Query Surfaces

The barrier is used by:

- public and admin latest queries;
- query batches, which obtain one timestamp and run every query at it;
- every sync worker transition, including WebSocket subscription reruns;
- `ctx.runQuery` calls made from actions, including internal action callbacks.

Query-cache tokens containing remote-owner reads are never refreshed from the
coordinator's local write log. That log can lag the owner even after a new
cluster timestamp is certified, so such tokens force a query rerun against the
owner at the new certified timestamp.

Read-after-write fences from #140 and participant fences from #242 remain
session-freshness guarantees. They are complementary to this barrier: a fence
proves a particular write is visible, while the barrier proves the complete
cross-partition snapshot is atomic.

## Explicit Timestamp Contract

`query_at_ts` does not silently translate a client timestamp. Every owner must
be able to certify that exact timestamp on its current leader. If a leadership
change, certificate eviction, placement change, or translated local timeline
makes that impossible, the request fails closed and the client must obtain a
new latest timestamp.

Each leader retains a bounded set of exact barrier certificates. Certificates
are deliberately not inferred from `timestamp <= max_closed_timestamp`, because
timestamp translation means an arbitrary older value is not necessarily a
portable point on every local timeline.

This preserves correctness now without claiming cross-node timestamp identity.
A future identity-preserving global apply order could make historical cursors
portable across failover without this re-certification rule.

## Relationship To Other Work

- #132 can distribute reactive invalidation ownership, but every rerun still
  needs this common snapshot contract.
- #133 may reduce changefeed fanout, but selective delivery cannot be used as
  proof that a query snapshot is complete.
- #139 defined the origin/local timestamp translation model used here.
- #140 and #242 provide portable session fences, not a global closed timestamp.
- #278 adds the missing cluster-wide atomic snapshot proof.

## References

- CockroachDB permits follower reads only at or below a closed timestamp:
  <https://www.cockroachlabs.com/docs/stable/follower-reads>
- FoundationDB obtains one read version and evaluates transaction reads against
  that version across storage owners:
  <https://apple.github.io/foundationdb/architecture.html>
- Spanner uses a single timestamp for a read-only transaction across all data it
  reads:
  <https://research.google/pubs/spanner-googles-globally-distributed-database-2/>
