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

### 2026-04 — Fresh replica checkpoint bootstrap rewound the write log behind active subscription workers

- **Status:** fixed
- **Related issue:** `#77`
- **Symptom:** the new fresh-replica checkpoint bootstrap test could restore a
  snapshot, but startup emitted repeated `Can't refresh token to newer
  timestamp ... than max ts ...` errors. The bootstrap path was technically
  succeeding while derived subscription state was still anchored to a newer
  pre-snapshot timestamp.
- **Root Cause:** `Database::load` started the write log and subscription
  workers from fresh local state before the checkpoint snapshot was installed.
  `InstallSnapshot` then reset the write log back to the checkpoint timestamp,
  but already-running subscription workers kept their older `processed_ts` and
  assumed the log only moved upward. That left them asking the write log to
  refresh tokens beyond its new max timestamp.
- **Fix:** teach write-log waiters to wake on reset, and teach subscription
  workers to detect a log rewind, invalidate their derived subscriptions, and
  re-anchor `processed_ts` to the new log max before continuing. This keeps
  snapshot install and any future log rewind consistent with follower-read
  derived state. The fresh-replica bootstrap path also now stores and loads a
  stable `checkpoint-latest` object so a brand-new replica can install the
  latest checkpoint directly.
- **Validation:**
  - `cargo test -p database test_wait_for_higher_ts_wakes_on_reset`
  - `cargo test -p database test_log_rewind_invalidates_subscriptions_and_reanchors_manager`
  - `cargo test -p database test_load_latest_checkpoint_reads_stable_latest_object`
  - `cargo test -p local_backend test_fresh_replica_bootstraps_from_checkpoint -- --nocapture`
  - the focused local-backend bootstrap test now passes on the real local
    environment without the earlier token-refresh storm.

### 2026-04 — Replica public mutations were not routed through the active forwarding path

- **Status:** fixed
- **Related issue:** `#76`
- **Symptom:** the repo already had a gRPC mutation forwarder service and a
  `PRIMARY_GRPC_URL` config knob, but live `/api/mutation` requests on a
  replica still executed the normal local path. The forwarding machinery
  existed, but it was not actually on the active public mutation route.
- **Root Cause:** the request path in `public_api.rs` authenticated and then
  called into the local application/database commit path directly. The
  primary/replica forwarding client was never consulted by the live HTTP
  handler, so the gateway-node behavior used by CockroachDB, TiDB/TiKV,
  YugabyteDB, and Spanner was only partially implemented.
- **Fix:** wire replica-mode `/api/mutation` requests through the
  `MutationForwarderGrpcClient`, preserving serialized args, caller identity,
  returned value, log lines, and structured mutation error data. Add a focused
  local-backend regression test that proves a replica-shaped HTTP router
  forwards the mutation to the primary and does not execute it locally.
- **Validation:**
  - `cargo test -p local_backend test_http_mutation_forwards_when_replica_mode -- --nocapture`
  - fresh image build and push:
    `ghcr.io/martinkalema/convex-horizontal-scaling:backend-issue76-forwarding-4389d7f-20260425-224218-dirty`
  - pushed digest:
    `sha256:8f74c123c9e225da8f32851b3702e8460fd9dcaaa4b1f073c8758e425b2aeb64`
  - clean-cluster rerun via `self-hosted/docker/test.sh`: all `77` write-scaling
    tests passed and all `10` Raft failover tests passed, confirming the
    routing change did not regress the distributed cluster paths.

### 2026-04 — Cluster correctness outpaced operator visibility and recovery guidance

- **Status:** fixed
- **Related issue:** `#81`
- **Symptom:** the cluster could pass the full write-scaling and failover
  suites, but operators still had to infer leadership, follower lag,
  checkpoint health, snapshot install activity, and 2PC health from logs and
  code. There was no concise runbook for the most likely recovery cases.
- **Root Cause:** we had correctness mechanisms for Raft, checkpoint bootstrap,
  snapshot install, and 2PC, but we had not turned those internal state
  machines into a dedicated operator-facing metrics and documentation surface.
- **Fix:** add cluster metrics for Raft leadership and progress, replication
  frontiers, repeatable timestamps, checkpoint write/load activity, snapshot
  installs, non-leader write rejection, fresh-replica bootstrap outcome, and
  2PC decisions/errors/retries. Document the new `/metrics` surface, alert
  suggestions, and short runbooks in `docs/cluster-observability.md`, and link
  that guide from the README.
- **Validation:**
  - `cargo check -p database`
  - `cargo check -p local_backend`
  - fresh image build and push:
    `ghcr.io/martinkalema/convex-horizontal-scaling:backend-issue81-observability-db1454f-20260428-074208-dirty`
  - pushed digest:
    `sha256:1c5c280c6587a9b3bc6d64a45437665b5286d89c33b68048b3963028226c0fa0`
  - clean-cluster rerun via `self-hosted/docker/test.sh`: all `77` write-scaling
    tests passed and all `10` Raft failover tests passed on the exact image
    above.

### 2026-04 — The first observability slice still missed leader-lag, transport, snapshot-cost, and richer 2PC signals

- **Status:** fixed
- **Related issue:** `#81`
- **Symptom:** the initial clustered observability work exposed leadership,
  checkpoint, bootstrap, and coarse 2PC health, but we still could not answer
  some operator questions cleanly: which followers are behind, whether a
  partition is under-replicated, whether JetStream transport is backing up,
  how expensive snapshot apply is, and whether 2PC latency/fanout/retry
  distributions are drifting.
- **Root Cause:** the first `#81` slice focused on basic correctness and
  recovery signals. It did not yet include the richer leader-progress,
  transport-latency, snapshot-cost, and 2PC-distribution metrics that mature
  systems like CockroachDB, TiDB/TiKV, YugabyteDB, and Spanner commonly
  surface for operators.
- **Fix:** add leader-side Raft follower-lag and quorum-health gauges,
  JetStream transport lag/backlog/sequence histograms and gauges, snapshot
  apply duration and bytes histograms, and 2PC coordinator latency,
  participant-count, and prepare-attempt histograms. Update the operator guide
  to document the actual exported metric names, leader-local gauge semantics,
  and lazy histogram behavior.
- **Validation:**
  - `cargo check -p database`
  - host-environment `cargo check -p local_backend`
  - fresh image build and push:
    `ghcr.io/martinkalema/convex-horizontal-scaling:backend-issue81-observability-followup-db1454f-20260428-102451-dirty`
  - pushed digest:
    `sha256:11d5896bc7b7c226822e3a9efa7fb2c8b2ea4d69946e2530b845bd1dfb899af9`
  - clean-cluster rerun via `self-hosted/docker/test.sh`: all `77` write-scaling
    tests passed and all `10` Raft failover tests passed on the exact image
    above
  - live scrape verification from the running cluster confirmed the new
    exported names and behavior for:
    `convex_local_backend_database_raft_configured_voters_info`,
    `..._recent_active_voters_info`, `..._lagging_followers_info`,
    `..._under_replicated_info`, and the
    `convex_local_backend_database_replication_transport_*` metrics
  - a targeted post-suite redeploy plus one direct `crossPartitionWrite`
    mutation confirmed the new 2PC histograms:
    `convex_local_backend_database_two_phase_coordinator_seconds`,
    `..._participants_total`, and `..._prepare_attempts_total`
  - the snapshot-apply histograms are wired and documented as lazy/event-driven;
    this standard `77 + 10` harness did not force a fresh Raft snapshot install
    on the scraped node, so those names will appear after the next real
    snapshot-apply event on that node

## Open Issues

None currently tracked in this journal after the latest clean-cluster run.

## Notes

- This journal is intentionally short and high-signal. Deep architectural
  writeups still belong in the other docs.
- When an issue is fixed, append the exact validation command set or point to
  the test suite that proved it.
