# Engineering Issue Journal

This document tracks the concrete distributed-systems issues we hit while
building horizontal scaling for Convex, how we diagnosed them, how we fixed
them, and how we validated the fix.

The goal is simple: no important bug should live only in chat history or in
someone's memory.

## Update Rule

For each meaningful issue, record:

1. **Symptom** — what failed and where it showed up.
2. **Root Cause** — what was actually wrong in the code or design.
3. **Fix** — what changed.
4. **Validation** — exactly which tests or cluster runs confirmed the result.
5. **Status** — `fixed`, `open`, or `regressed`.

## Validation Policy

For changes that affect clustered behavior, validation is not complete until we
do all of the following:

1. Build a fresh backend image from the current branch.
2. Push that exact image to GHCR with a unique tag.
3. Reset the cluster with `docker compose down -v`.
4. Start the cluster on that exact image.
5. Run the full write-scaling suite (`77` tests), and when relevant the full
   top-level suite (`self-hosted/docker/test.sh`).

Unit and integration tests are still useful, but they are not enough on their
own for distributed changes.

## Solved Issues

### 2026-04 — Cross-partition OCC was unsound under replication lag

- **Status:** fixed
- **Related issues:** `#78`, `#84`
- **Symptom:** cross-partition transactions could commit even when a node had
  not actually caught up on the remote partition it had read from.
- **Root Cause:** OCC used a coarse replicated timestamp and could log a
  warning about lag, then continue anyway. That made validation best-effort
  instead of strict.
- **Fix:** track replication freshness per partition, wait for the exact remote
  partitions read by the transaction to reach the transaction begin timestamp,
  and reject stale validation paths instead of warning and continuing.
- **Validation:** targeted database tests covering prepare/commit wait behavior
  and replication frontier persistence across restart.

### 2026-04 — Remote 2PC `Prepare` over gRPC was missing

- **Status:** fixed
- **Related issue:** `#79`
- **Symptom:** cross-partition transactions had coordinator scaffolding, but
  the real remote participant prepare path was incomplete.
- **Root Cause:** the coordinator could not send a real participant-local
  prepare request carrying the correct read/write slice and assigned prepare
  timestamp over gRPC.
- **Fix:** implement participant transaction protobufs, database-layer gRPC
  client/server handling, participant-local validation, prepare timestamp
  fencing, and coordinator retries for too-low prepare timestamps.
- **Validation:** database tests covering idempotent remote prepare,
  cross-partition commit, and rollback on participant failure; `cargo check`
  for `local_backend`.

### 2026-04 — Forwarded `deploy2` requests failed auth on the metadata owner

- **Status:** fixed
- **Symptom:** deploys sent to a non-owner node failed once that node forwarded
  them to the metadata owner. The owner rejected them with invalid admin key
  errors.
- **Root Cause:** we changed deploy ownership, but kept per-instance admin-key
  semantics. The forwarded request arrived at the owner with credentials valid
  for the original node, not for the owner.
- **Fix:** authenticate on the receiving node, then forward with owner-valid
  credentials instead of blindly proxying the original request body.
- **Validation:** fresh image build and full cluster deploy run; the suite
  moved past `Deploying functions...` on both nodes and no longer failed with
  owner auth errors.

### 2026-04 — Idle replication frontier heartbeats polluted write-log state

- **Status:** fixed
- **Symptom:** heartbeat-style replica deltas that carried no user writes could
  still advance local write-log state, which then interfered with timestamp and
  frontier logic.
- **Root Cause:** no-op replica deltas were treated like real writes for
  write-log append purposes.
- **Fix:** publish idle replication frontier heartbeats explicitly, but do not
  append them to the write log when the delta contains no writes.
- **Validation:** fresh image build and clean-cluster rerun changed the failure
  mode away from the earlier heartbeat/write-log corruption path.

### 2026-04 — Queued local table creation could be erased by later replica publish

- **Status:** fixed
- **Symptom:** on a clean cluster, the write-scaling suite failed in Test 1.
  All writes reported success, but one node returned `500` for dashboard-style
  queries and the other only saw one `tasks` row out of three.
- **Root Cause:** replica deltas were building from the latest published
  snapshot instead of the latest queued unpublished snapshot. If a local table
  creation was staged but not yet published, a later replica publish could
  overwrite the request node’s next visible snapshot with a version that did
  not include the locally created table. The next local write then recreated
  `_tables` metadata for the same logical table.
- **Fix:** make local commits and replica deltas both build on the latest
  queued snapshot state, not just the latest published snapshot, and add a
  regression test covering “paused local table creation + replica delta +
  second local write”.
- **Validation:** the queued-snapshot regression no longer reproduced, and the
  full clean-cluster suite later passed end to end on
  `backend-heartbeat-nonfatal-c2410ab-20260424-101611-dirty`.

### 2026-04 — Idle frontier heartbeat crashed nodes during NATS partition

- **Status:** fixed
- **Symptom:** the full clean-cluster suite reached Test 26 (`NATS Partition
  Simulation`), then Node B killed itself and the Raft failover suite could
  not start cleanly afterward.
- **Root Cause:** the idle replication frontier heartbeat path asked the
  timestamp oracle for a fresh timestamp even though it was only trying to
  advance follower-read safety metadata. When NATS JetStream KV was
  unavailable, the TSO call timed out and the `?` in the committer loop treated
  that background heartbeat failure as a fatal process error.
- **Fix:** keep idle frontier heartbeats best-effort. The committer now logs a
  warning and skips the heartbeat interval when the TSO is unavailable instead
  of shutting the node down. A targeted regression test covers the exact
  failing-oracle path.
- **Validation:**
  - `cargo test -p database test_idle_replication_frontier_heartbeat_tso_failure_is_nonfatal`
  - fresh image build and push:
    `ghcr.io/martinkalema/convex-horizontal-scaling:backend-heartbeat-nonfatal-c2410ab-20260424-101611-dirty`
  - pushed digest:
    `sha256:4894b55eb0fb95e7e5ba95c5be5f740c9fa27996dda5797b693e7f91b0228fc4`
  - clean-cluster rerun via `self-hosted/docker/test.sh`: all `77` write-scaling
    tests passed, Test 26 passed, and the Raft failover suite passed.

## Open Issues

None currently tracked in this journal after the latest clean-cluster run.

## Notes

- This journal is intentionally short and high-signal. Deep architectural
  writeups still belong in the other docs.
- When an issue is fixed, append the exact validation command set or point to
  the test suite that proved it.
