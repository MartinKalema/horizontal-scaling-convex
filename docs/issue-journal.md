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

### 2026-04 — Forwarded deploy protocol requests failed auth on the metadata owner

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
  - `cargo test -p database test_idle_remote_read_frontier_heartbeat_tso_failure_is_nonfatal`
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

### 2026-04 — Same-partition followers still depend on NATS for application/deploy convergence

- **Status:** open
- **Related issue:** `#74`
- **Symptom:** the first safe-looking `#74` attempt added partition-filtered
  JetStream subscriptions and stopped partitioned nodes from consuming their
  own partition's NATS subject, on the assumption that same-partition followers
  already had everything they needed from Raft. The full clean-cluster
  validation split cleanly:
  - `77/77` write-scaling tests still passed
  - the `10`-test Raft failover suite broke immediately
  - followers in the same partition no longer agreed with the leader after a
    single write, and they could not take over correctly after the leader was
    killed
- **Root Cause:** our current follower state convergence path still relies on
  same-partition `CommitDelta` delivery over NATS. Removing the local-partition
  JetStream feed did not just reduce cross-partition fanout; it also stopped
  same-partition followers from converging application-visible deployment/data
  state well enough to serve queries and become healthy write leaders during
  failover. In other words, this codebase is not yet at the point where Raft
  alone makes a follower query-ready for its partition.
- **Fix:** roll back the runtime behavior change so partitioned nodes continue
  consuming all partition subjects for now, keep the filtered-subscribe support
  in the distributed-log abstraction/NATS transport as dormant groundwork, and
  treat `#74` as blocked on a precursor: make same-partition followers fully
  self-sufficient from their own Raft/apply path before we try to trim local
  partition NATS traffic.
- **Validation:**
  - focused cargo pass after rollback:
    `cargo test -p local_backend test_fresh_replica_bootstraps_from_checkpoint -- --nocapture`
  - attempted optimization image on a clean cluster:
    - `77/77` write-scaling tests passed
    - Raft failover suite failed at follower agreement and post-leader-kill
      writes
  - rebuilt rollback image and reran the full clean-cluster harness:
    `self-hosted/docker/test.sh`
    Result: all `77` write-scaling tests passed and all `10` Raft failover
    tests passed

### 2026-05 — Test 29 exposed that replication frontier was not a strong-read fence

- **Status:** fixed
- **Related issue:** `#74`
- **Symptom:** Write Scaling Test 29 (`Max Batch Size 200 docs`) failed in a
  very specific way on the clean cluster:
  - forwarded `messages:batchWrite(count=200)` returned success via Node A
  - the immediate local `messages:countBatch(prefix)` on that same node
    returned `0`
  - cross-node reads could already see all `200`
  - the local follower converged to `200` about a second later
- **Root Cause:** we were treating the per-partition `replication_frontier`
  like a strong local read fence. That frontier was allowed to advance on
  idle/empty replication heartbeats, so a same-partition follower could decide
  "I am fresh enough" before the actual batch write had become query-visible on
  that node. This is exactly the distinction that TiKV makes between
  leader-side progress (`resolved-ts`) and follower-safe local read progress
  (`safe-ts`): observed replication progress is not automatically a safe local
  strong-read timestamp.
- **Fix:** split the signal in the snapshot manager:
  - keep `replication_frontier` for observed/stale-read-style progress and
    observability
  - add a separate per-partition `replication_write_frontier` that advances
    only when a real replicated write is published into the local snapshot/write
    state
  - make forwarded follower mutations wait on this write frontier instead of
    the heartbeat-advancing frontier before returning success to the caller
- **Validation:**
  - targeted regression:
    `cargo test -p database test_remote_read_frontier_heartbeat_does_not_advance_snapshot_ts -- --nocapture`
  - focused forwarding proof:
    `cargo test -p local_backend test_http_mutation_forwards_when_replica_mode -- --nocapture`
  - fresh image rebuild, clean profile-scoped reset
    `docker compose --profile cluster down -v --remove-orphans`, cluster
    restart via `docker compose --profile cluster up -d`, and full harness
    rerun:
    `self-hosted/docker/test.sh`
    Result: all `77` write-scaling tests passed and all `10` Raft failover
    tests passed, including Test 29

### 2026-05 — Same-partition followers became self-sufficient from Raft/apply

- **Status:** fixed
- **Related issue:** `#74`
- **Symptom:** the first partition-filtering attempt had shown that removing a
  partition's own NATS subject broke failover, which looked like proof that
  same-partition followers still needed local-partition `CommitDelta` traffic.
- **Root Cause:** the follower gap was not inherent to the design; it was in
  our Raft apply path. We were skipping committed Raft entries based on whether
  the node was *currently* leader instead of whether the entry had originally
  been proposed locally. After leader changes, a node could become leader and
  incorrectly skip committed entries that originated on the old leader. NATS
  happened to mask that bug by delivering the same-partition delta through a
  second path.
- **Fix:** make Raft apply origin-aware and then trim NATS fanout:
  - tag intra-partition Raft-published deltas with the proposing
    `source_raft_node_id`
  - apply every committed Raft delta locally unless this exact node already
    proposed and applied it itself
  - once same-partition followers were converging correctly from Raft/apply,
    switch partitioned `ReplicaDeltaConsumer` subscriptions to only the *other*
    partitions' NATS subjects via `subscribe_filtered(...)`
- **Why this matches the big databases:** mature systems treat the ordered
  replicated log as the authoritative path for same-replica convergence, and
  then layer changefeeds/streaming on top of that. They do not depend on a
  secondary change stream to make Raft followers query-ready:
  - etcd/raft requires `Ready` batches to be handled and applied in order
  - TiKV's apply worker is explicitly responsible for committed Raft entries
  - CockroachDB followers catch up by replaying the Raft log or loading a
    snapshot and then replaying later actions
  - YugabyteDB followers apply committed log entries to their state machine
- **Validation:**
  - focused forwarding proof:
    `cargo test -p local_backend test_http_mutation_forwards_when_replica_mode -- --nocapture`
  - fresh image rebuild from the filtered-subscribe branch state
  - clean profile-scoped reset:
    `docker compose --profile cluster down -v --remove-orphans`
  - cluster restart:
    `docker compose --profile cluster up -d`
  - full clean-cluster harness rerun:
    `self-hosted/docker/test.sh`
    Result: all `77` write-scaling tests passed and all `10` Raft failover
    tests passed with partitioned nodes no longer consuming their own
    partition's NATS subject

### 2026-05 — Started explicit selective-delivery interest tracking

- **Status:** in progress
- **Related issue:** `#96`
- **Why this first:** before we can safely narrow cross-partition delivery, the
  system needs an explicit notion of what data a node is actually interested
  in. Today, the subscription/read path still gets its "read anywhere" behavior
  from a broad remote snapshot, so skipping remote deltas without a real
  interest model would silently change semantics.
- **What this slice adds:**
  - a database-side `DeltaInterestTracker` that aggregates live subscription
    interests as a node-local set of table names
  - `Database::subscribe(...)` now resolves a subscription token's read-set
    into concrete non-system table names before registering the subscription
  - `SubscriptionsClient` / `SubscriptionManager` now keep those per-subscription
    table interests and maintain reference counts as subscriptions start and end
  - a new `database_selective_delivery_interested_tables_info` metric and a
    `CommitDelta::touched_table_names()` helper for the next transport-side step
- **What this does not do yet:** it does **not** change runtime fanout or stop
  delivering any remote deltas. This is the seam the later NATS/changefeed
  routing work will use.
- **Validation:**
  - `cargo check -p local_backend`
  - `cargo test -p database delta_interest::tests::interest_tracker_ref_counts_tables -- --nocapture`
  - `cargo test -p database commit_delta::tests::touched_table_names_collects_unique_tables -- --nocapture`
  - `cargo test -p database subscription::tests::test_log_rewind_invalidates_subscriptions_and_reanchors_manager -- --nocapture`

### 2026-05 — Added selective-delivery shadow control plane

- **Status:** in progress
- **Related issue:** `#96`
- **What this slice adds:**
  - nodes now publish their live table-interest set into a dedicated NATS KV
    bucket (`convex_delta_interest`) with freshness timestamps
  - producers can now compute which nodes are interested in a delta's touched
    tables and shadow-publish those deltas to node-specific subjects
    (`convex.commits.node.<node>`) in addition to the existing broad partition
    subject
  - this gives us a real selective-delivery control plane and transport shape
    without yet changing the current broad read-anywhere data path
- **Why this is still shadow mode:** broad remote replication is still required
  for one-shot cross-partition queries. Live subscriptions tell us what a node
  is interested in over time, but ad hoc query execution still does not expose
  the full table set *before* execution. Until the request path can catch up or
  reroute those cold-table reads safely, switching consumers away from the broad
  path would change semantics.
- **Validation:**
  - `cargo check -p local_backend`
  - `cargo test -p database selective_delivery::tests::interested_nodes_match_tables_and_skip_stale_entries -- --nocapture`
  - `cargo test -p database selective_delivery::tests::node_subject_names_are_stable -- --nocapture`

### 2026-05 — Added recent-query interest leases for stateless reads

- **Status:** in progress
- **Related issue:** `#96`
- **Why this slice matters:** sync queries already warm table interest after
  their first execution because they immediately subscribe on the returned
  token. Plain HTTP queries do not; they learn their table set only after
  execution and then discard it. That made the control plane under-report real
  read traffic from stateless query paths.
- **What this slice adds:**
  - `DeltaInterestTracker` now supports short-lived recent-interest leases in
    addition to long-lived subscription ref-counts
  - the selective-delivery interest publisher prunes expired recent-interest
    leases before publishing to NATS KV
  - latest-timestamp public HTTP queries now feed their returned token back
    into the tracker for a short TTL, so repeated ad hoc reads can warm table
    interest over time
- **What this still does not solve:** this improves the signal for selective
  routing, but it still does not make the *first* cold-table query safe under a
  fully selective delivery regime. We still need either cold-table catch-up or
  request rerouting before the active cutover.
- **Validation:**
  - `cargo test -p local_backend test_http_query_records_recent_delta_interest -- --nocapture`

### 2026-05 — Added query-forwarder infrastructure for cold-read reroute

- **Status:** in progress
- **Related issue:** `#96`
- **What this slice adds:**
  - the existing gRPC `MutationForwarder` service now also supports forwarding
    public queries to an authoritative peer
  - forwarded query responses include the non-system table names touched by the
    returned token/read-set, so the caller can warm selective-delivery interest
    from a routed read instead of losing that information
- **Why this matters:** the remaining `#96` blocker is the first cold-table
  stateless query. This forwarder gives us the transport primitive for a safe
  reroute path without forcing us to hard-code the wrong policy prematurely.
- **What this still does not do:** the public HTTP query handlers do not yet
  automatically reroute cold reads. We still need to decide the active policy:
  query reroute target, fallback behavior, and when to prefer reroute versus
  local execution.
- **Validation:**
  - `cargo test -p local_backend test_query_forwarder_returns_touched_tables -- --nocapture`
  - `cargo check -p local_backend`

### 2026-05 — Completed leader-aware latest-query routing for selective delivery

- **Status:** complete
- **Related issue:** `#96`
- **What closed the loop:**
  - system-table deltas now continue to fan out on a dedicated global NATS
    subject so deploy/metadata state is never accidentally gated by table
    interest
  - latest public queries from non-authority partitions still reroute to the
    partition-0 authority
  - latest public queries from partition-0 followers now also reroute to the
    current Raft leader instead of trying to serve an unsafe local latest read
- **Why this was necessary:** once user-table deltas became selective, a
  partition-0 follower could no longer safely answer every latest query from
  its local snapshot. The remaining full-harness failures were exactly that
  shape: writes forwarded to the leader succeeded, but immediate reads on a
  partition-0 follower could still observe stale local state. Leader-aware
  latest-query forwarding closes that gap without re-broadcasting the old broad
  replication stream.
- **Focused validation:**
  - `cargo check -p local_backend`
  - `cargo test -p local_backend test_selective_query_forwarding_api_routes_partitioned_queries -- --nocapture`
  - `cargo test -p local_backend test_selective_query_forwarding_api_routes_authority_partition_follower_queries -- --nocapture`
  - `cargo test -p local_backend test_http_mutation_local_partitioned_write_does_not_wait_for_replication_frontier -- --nocapture`
  - `cargo test -p local_backend test_http_mutation_forwards_when_replica_mode -- --nocapture`
  - `cargo test -p database selective_delivery::tests -- --nocapture`
  - `cargo test -p database test_remote_single_partition_commit_rejects_before_waiting_for_remote_frontier -- --nocapture`
- **End-to-end validation:**
  - rebuilt backend image from branch state
  - `docker compose --profile cluster down -v --remove-orphans`
  - `docker compose --profile cluster up -d --pull never`
  - `bash self-hosted/docker/test.sh`
  - result: `ALL 77 TESTS PASSED`, `ALL 10 TESTS PASSED`, `ALL SUITES PASSED`

### 2026-06 — Added NATS retention-gap detection for replica consumers

- **Status:** complete
- **Related issue:** `#162`
- **What this fixes:** a durable JetStream consumer could previously resume from
  the earliest retained message even if finite stream retention had already
  deleted messages the node still needed. That is a silent-divergence failure
  mode, so the consumer now compares its durable sequence floor with the
  stream's first retained sequence before applying any deltas.
- **Behavior:** if the next required stream sequence is older than
  `first_seq`, subscription fails closed with an operator-facing rebootstrap
  error and increments a retention-gap metric instead of applying a truncated
  history.
- **Validation:**
  - `cargo test -p database nats_distributed_log::tests -- --nocapture`
  - `cargo check -p database`
  - `cargo check -p local_backend`

### 2026-06 — Routed remote single-partition mutations through the owner

- **Status:** complete
- **Related issue:** `#163`
- **What this fixes:** a mutation received by a healthy non-owner partition
  could execute user code locally and then fail at commit time with "route this
  mutation to the correct partition owner." That leaked partition topology to
  clients.
- **Behavior:** once a finalized transaction is classified as targeting a
  single remote owner, the committer now uses the existing participant gRPC path
  as a one-participant 2PC route. Local-owner writes still use the fast path,
  cross-partition writes still use 2PC, and clusters without `NODE_ADDRESSES`
  still fail closed instead of guessing.
- **Validation:**
  - `cargo test -p database test_partition_enforcement -- --nocapture`
  - `cargo test -p database test_remote_single_partition_write_routes_to_owner_over_grpc -- --nocapture`
  - `cargo test -p database test_cross_partition_commit_uses_remote_prepare_over_grpc -- --nocapture`
  - `cargo check -p database`

### 2026-06 — Authenticated internal cluster gRPC traffic

- **Status:** complete
- **Related issue:** `#164`
- **What this fixes:** internal forwarding, 2PC, and Raft gRPC services
  previously trusted any caller that could reach the internal port. Forwarded
  user identities were decoded with unchecked helpers, so the network boundary
  itself was doing too much security work.
- **Behavior:** cluster gRPC clients now attach a metadata token derived from
  the shared deployment instance secret. Mutation/query forwarding, 2PC
  participant RPCs, and Raft transport servers reject missing or mismatched
  tokens with `Unauthenticated`.
- **Operational note:** this is peer authentication for the cluster control
  plane. The gRPC port should still be private to cluster members, and all
  nodes in one deployment must share the same instance secret.
- **Validation:**
  - `cargo check -p local_backend --tests`
  - `cargo test -p common test_authenticate_requires_matching_token -- --nocapture`
  - `cargo test -p database test_raft_transport_rejects_missing_cluster_auth -- --nocapture`
  - `cargo test -p local_backend test_internal_grpc_services_reject_missing_cluster_auth -- --nocapture`
  - `cargo test -p local_backend test_http_mutation_forwards_when_replica_mode -- --nocapture`

### 2026-06 — Separated 2PC prepared intents from the commit publish queue

- **Status:** complete
- **Related issue:** `#154`
- **What this fixes:** 2PC prepare previously staged prepared writes in the
  same `PendingWrites` FIFO used by the normal publish path. That made a
  prepared transaction look like the next publishable commit and forced the
  committer to block all local writes while a prepare was unresolved.
- **Behavior:** prepared writes are now tracked as separate intents. They still
  participate in OCC conflict checks and document-ID write locks, but they are
  removed by `CommitPrepared`/`RollbackPrepared` directly instead of being
  popped from the normal publish FIFO. Because the current 2PC protocol fixes
  the prepare timestamp as the final visibility timestamp, normal commits still
  retry while a prepare is unresolved; allowing unrelated commits to publish
  ahead of a prepared transaction belongs with the later timestamp-domain /
  rebasable snapshot work.
- **Validation:**
  - `cargo test -p database test_local_commit_retries_while_prepare_is_unresolved -- --nocapture`
  - `cargo test -p database test_prepare_retries_while_local_commit_is_pending -- --nocapture`
  - `cargo test -p database test_remote_prepare_over_grpc_is_idempotent -- --nocapture`
  - `cargo test -p database test_prepared_participant_recovers_from_redo_after_restart -- --nocapture`
  - `cargo test -p database test_cross_partition_commit_uses_remote_prepare_over_grpc -- --nocapture`
  - `cargo check -p database`

### 2026-06 — Moved local partition visibility behind Raft quorum

- **Status:** complete
- **Related issue:** `#146`
- **What this fixes:** Raft-backed leaders previously wrote local persistence,
  appended the write log, and pushed `SnapshotManager` before the Raft proposal
  reached quorum. A rejected proposal could therefore leave a non-quorum write
  visible locally or reloadable from the node's persistence.
- **Behavior:** local commits now build the exact `CommitDelta`, propose it to
  Raft, and wait for quorum before writing local persistence or publishing to
  the readable snapshot/write log. `CommitPrepared` follows the same ordering.
  Non-Raft single-node mode still writes directly.
- **Validation:**
  - `cargo test -p database test_failed_raft_proposal_does_not_publish_or_persist -- --nocapture`
  - `cargo test -p database test_cross_partition_commit_uses_remote_prepare_over_grpc -- --nocapture`
  - `cargo test -p database test_prepared_participant_recovers_from_redo_after_restart -- --nocapture`
  - `cargo test -p database test_remote_single_partition_write_routes_to_owner_over_grpc -- --nocapture`
  - `cargo test -p database test_local_commit_retries_while_prepare_is_unresolved -- --nocapture`
  - `cargo check -p database`
  - `cargo check -p local_backend`

### 2026-06 — Blocked Raft applied-index advancement on state-machine apply failure

- **Status:** complete
- **Related issue:** `#144`
- **What this fixes:** committed Raft entries that failed Convex
  state-machine apply were previously logged and skipped while the Raft loop
  still advanced. A follower could then believe it had applied a quorum
  committed entry even though its `Committer`/`SnapshotManager` never accepted
  the delta.
- **Behavior:** Raft committed-entry and snapshot apply callbacks now fail the
  Raft loop before `advance_apply()` when the Convex state-machine apply fails.
  The local backend Raft callback also propagates `apply_replica_delta`
  failures instead of swallowing them.
- **Validation:**
  - `cargo test -p database test_state_machine_apply_failure_returns_before_advance_apply -- --nocapture`
  - `cargo test -p database test_three_node_propose_commit -- --nocapture`
  - `cargo test -p database test_kill_leader_reelection -- --nocapture`
  - `cargo check -p database`
  - `cargo check -p local_backend`

### 2026-06 — Persisted Raft state-machine applied index separately from commit index

- **Status:** complete
- **Related issue:** `#145`
- **What this fixes:** Raft restart previously initialized `Config.applied`
  from `HardState.commit`, treating quorum commit as if the local Convex state
  machine had already installed every committed entry. A crash after commit
  persistence but before state-machine apply could therefore skip a log entry
  forever on restart.
- **Behavior:** raft-engine now stores a separate durable applied index. Follower
  and replayed entries persist that index only after successful state-machine
  apply. Live local leader proposals report their committed log index back to
  the commit future; after the committer publishes local visibility, it marks
  that index applied through the Raft node. Raft snapshots are generated at the
  durable applied index, not the commit index.
- **Validation:**
  - `cargo test -p database test_restart_replays_committed_but_unapplied_entry -- --nocapture`
  - `cargo test -p database test_applied_index_persistence -- --nocapture`
  - `cargo test -p database test_snapshot_persists_and_reloads -- --nocapture`
  - `cargo test -p database raft_node::tests -- --nocapture`
  - `cargo test -p database raft_commit_waiter -- --nocapture`
  - `cargo test -p database raft_failover_tests -- --nocapture`
  - `cargo check -p database`
  - `cargo check -p local_backend`

### 2026-06 — Replicated 2PC prepare redo through Raft before prepare ACK

- **Status:** complete
- **Related issue:** `#155`
- **What this fixes:** 2PC participants previously acknowledged prepare after
  staging an in-memory intent and writing only their local redo record. If the
  participant leader failed before the final decision, the new Raft leader
  could lack the prepared transaction needed to commit or roll it back.
- **Behavior:** prepare records are now serialized as typed Raft
  state-machine entries. A participant ACKs prepare only after the redo record
  is locally durable and the prepare entry has reached Raft quorum. Followers
  apply the Raft prepare through the committer loop, persist the redo, and stage
  the prepared intent so a promoted follower can resolve the transaction.
  Same-partition Raft commit deltas clean up the prepared intent and redo on
  followers after the final commit applies.
- **Validation:**
  - `cargo test -p database raft_state_machine -- --nocapture`
  - `cargo test -p database test_raft_ -- --nocapture`
  - `cargo test -p database prepare -- --nocapture`
  - `cargo check -p local_backend`

## Open Issues

- `#74` is no longer blocked on follower self-sufficiency. The current
  remaining question is how far we want to push selective fanout beyond
  excluding same-partition NATS traffic.

## Notes

- This journal is intentionally short and high-signal. Deep architectural
  writeups still belong in the other docs.
- When an issue is fixed, append the exact validation command set or point to
  the test suite that proved it.
