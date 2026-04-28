# Cluster Observability

This document is the operator-facing guide for the clustered backend.

The goals are:

1. Make it obvious which node currently owns a partition.
2. Make follower lag and stalled replication visible without digging through
   logs.
3. Make transport backlog and publish-to-delivery lag visible.
4. Make checkpoint bootstrap and snapshot install success/failure visible.
5. Make 2PC latency, fanout, and retry pressure visible before they turn into
   user-visible incidents.

## Where To Look

- `/metrics` is the source of truth for machine-readable health and alerting.
- logs are for forensics and root-cause detail after an alert fires.
- the dashboard is useful for quick orientation, but it is not the canonical
  place to judge replication health.

All metric names below are shown as their **base names**. The runtime prefixes
them with the binary/service name automatically. For example,
`database_raft_is_leader_info` is exported from the local backend process as:

- `convex_local_backend_database_raft_is_leader_info`

Likewise, the new transport and 2PC metrics below appear on `/metrics` with
that same `convex_local_backend_` prefix.

Two important scrape behaviors to know up front:

- The follower-lag / under-replication Raft gauges are **leader-local**. On
  follower nodes they are intentionally reset to `0`, because only the current
  leader has authoritative per-follower progress.
- Histogram metrics are **lazy**. They appear on `/metrics` only after the
  first relevant event happens on that node, such as a 2PC transaction,
  replication publish, or snapshot apply.

## Core Metrics

- `database_raft_is_leader_info{partition="N"}`
  `1` means this node is the Raft leader for partition `N`; `0` means it is
  not.
- `database_raft_leader_id_info{partition="N"}`
  Current known leader node ID for partition `N`. `0` means unknown.
- `database_raft_leader_changes_total{partition="N"}`
  Monotonic counter of leader changes observed on this node.
- `database_raft_term_info{partition="N"}`
  Current Raft term for partition `N`.
- `database_raft_committed_index_info{partition="N"}`
  Current committed Raft log index for partition `N`.
- `database_raft_applied_index_info{partition="N"}`
  Current applied Raft log index for partition `N`.
- `database_raft_configured_voters_info{partition="N"}`
  Number of configured voting replicas for partition `N`, reported by the
  current leader. Followers reset this gauge to `0`.
- `database_raft_recent_active_voters_info{partition="N"}`
  Number of voting replicas the leader currently considers recently active.
  Followers reset this gauge to `0`.
- `database_raft_lagging_followers_info{partition="N"}`
  Count of followers whose matched index is behind the leader commit index.
  Followers reset this gauge to `0`.
- `database_raft_max_follower_lag_entries_info{partition="N"}`
  Worst-case follower log lag, in entries, for partition `N`.
  Followers reset this gauge to `0`.
- `database_raft_total_follower_lag_entries_info{partition="N"}`
  Sum of all follower lag, in entries, for partition `N`.
  Followers reset this gauge to `0`.
- `database_raft_under_replicated_info{partition="N"}`
  `1` means fewer voters are active than configured for partition `N`.
  Followers reset this gauge to `0`.
- `database_raft_quorum_unavailable_info{partition="N"}`
  `1` means partition `N` currently lacks an active write quorum.
  Followers reset this gauge to `0`.
- `database_replication_frontier_ts_info{source_partition="N"}`
  Latest remote frontier from source partition `N` that this node has applied.
- `database_latest_repeatable_ts_info`
  Latest repeatable timestamp currently visible on this node.
- `database_persisted_max_repeatable_ts_info`
  Latest repeatable timestamp durably persisted for this node.
- `database_checkpoint_written_ts_info`
  Latest checkpoint timestamp this node successfully wrote.
- `database_checkpoint_write_total{status="success|error"}`
  Counter of checkpoint write attempts.
- `database_checkpoint_loaded_ts_info`
  Latest checkpoint timestamp this node successfully loaded.
- `database_checkpoint_load_total{status="success|error"}`
  Counter of checkpoint load attempts.
- `database_snapshot_installed_ts_info`
  Latest checkpoint timestamp this node successfully installed into the local
  database state.
- `database_snapshot_install_total{status="success|error"}`
  Counter of snapshot install attempts.
- `database_raft_snapshot_apply_seconds{status="success|error"}`
  Histogram of end-to-end snapshot persist-and-apply time. It appears after
  the first real snapshot install on that node.
- `database_raft_snapshot_apply_bytes`
  Histogram of incoming Raft snapshot payload sizes. It appears after the
  first real snapshot install on that node.
- `database_replica_bootstrap_total{status="success|error"}`
  Counter of fresh-replica checkpoint bootstrap attempts.
- `database_write_rejected_not_leader_total{partition="N"}`
  Counter of writes rejected because the node was not the leader for partition
  `N`.
- `database_two_phase_coordinator_decisions_total{outcome="commit|rollback"}`
  Counter of 2PC coordinator decisions.
- `database_two_phase_coordinator_errors_total{phase="prepare|commit"}`
  Counter of 2PC coordinator failures by phase.
- `database_two_phase_coordinator_seconds{status="success|error"}`
  Histogram of end-to-end coordinator runtime. It appears after the first 2PC
  transaction coordinated by that node.
- `database_two_phase_participants_total`
  Histogram of how many participant partitions each 2PC transaction touched.
  It appears after the first 2PC transaction coordinated by that node.
- `database_two_phase_prepare_attempts_total`
  Histogram of how many prepare attempts were needed before a 2PC transaction
  either committed or gave up. It appears after the first 2PC transaction
  coordinated by that node.
- `database_two_phase_prepare_retries_total`
  Counter of 2PC prepare timestamp retries.
- `database_replication_transport_publish_seconds`
  Histogram of JetStream publish latency for replication messages.
- `database_replication_transport_message_bytes{phase="publish|consume"}`
  Histogram of replication message sizes by publish and consume phase.
- `database_replication_transport_lag_seconds`
  Histogram of publish-to-delivery lag for replication messages.
- `database_replication_transport_pending_messages_info`
  Current JetStream pending message count for the consumer on this node.
- `database_replication_transport_stream_sequence_info`
  Latest stream sequence observed by this node’s consumer.
- `database_replication_transport_consumer_sequence_info`
  Latest consumer sequence acknowledged by this node’s consumer.

## Suggested Alerts

- No leader:
  alert if no node reports `database_raft_is_leader_info{partition="N"} == 1`
  for more than 60 seconds.
- Leader churn:
  alert if `increase(database_raft_leader_changes_total[10m])` is unusually
  high for any partition.
- Follower lag:
  compare `database_replication_frontier_ts_info{source_partition="N"}` across
  nodes and watch `database_raft_lagging_followers_info`. Alert when one node
  stays materially behind the max frontier for that partition or the leader
  continues to report lagging followers for several minutes.
- Under-replication:
  alert if `database_raft_under_replicated_info{partition="N"} == 1` for more
  than a short blip after a restart or deploy.
- Quorum risk:
  alert immediately if `database_raft_quorum_unavailable_info{partition="N"} == 1`.
- Stalled apply path:
  alert when `database_raft_committed_index_info` keeps moving but
  `database_raft_applied_index_info` stalls on the same node.
- Transport backlog:
  alert when `database_replication_transport_pending_messages_info` grows
  steadily instead of draining, or when
  `database_replication_transport_stream_sequence_info -
  database_replication_transport_consumer_sequence_info` remains large.
- Transport latency:
  alert when `database_replication_transport_lag_seconds` shifts upward
  materially for sustained periods.
- Checkpoint failure:
  alert on any sustained increase in `database_checkpoint_write_total{status="error"}`.
- Snapshot/bootstrap failure:
  alert on any increase in `database_snapshot_install_total{status="error"}`
  or `database_replica_bootstrap_total{status="error"}`.
- Snapshot cost:
  page or investigate when `database_raft_snapshot_apply_seconds` or
  `database_raft_snapshot_apply_bytes` jumps sharply relative to normal
  follower recovery, because large or slow snapshots usually imply a badly
  lagging follower or storage trouble.
- Wrong-node write traffic:
  alert when `database_write_rejected_not_leader_total` spikes unexpectedly.
- 2PC trouble:
  alert when `database_two_phase_coordinator_errors_total` increases or when
  `database_two_phase_prepare_retries_total` grows steadily.
- 2PC latency or fanout growth:
  investigate when `database_two_phase_coordinator_seconds`,
  `database_two_phase_participants_total`, or
  `database_two_phase_prepare_attempts_total` drifts upward because that often
  precedes visible write latency regressions.

## Runbooks

### Lagging Follower

1. Check `database_raft_leader_id_info` and confirm which node is leader for
   the partition.
2. Compare `database_raft_committed_index_info` and
   `database_raft_applied_index_info` between leader and follower.
3. Compare `database_raft_max_follower_lag_entries_info` and
   `database_raft_total_follower_lag_entries_info` on the leader for the
   affected partition.
4. Compare `database_replication_frontier_ts_info` for the lagging source
   partition on the follower.
5. If JetStream transport is also behind, inspect
   `database_replication_transport_pending_messages_info`,
   `database_replication_transport_stream_sequence_info`, and
   `database_replication_transport_consumer_sequence_info`.
6. If the follower is far behind and log replay is not closing the gap, look
   for a new increase in `database_snapshot_install_total{status="success"}`
   to confirm snapshot catch-up.
7. If snapshot install is failing, inspect logs around `install_snapshot` and
   checkpoint decode/load.

### Under-Replicated Partition

1. Check `database_raft_under_replicated_info` and
   `database_raft_recent_active_voters_info` for the affected partition.
2. Confirm whether the missing voters are intentionally restarting or whether
   a node is stuck.
3. If `database_raft_quorum_unavailable_info == 1`, treat the partition as
   write-unhealthy until voters return.
4. Correlate with transport lag metrics and node logs before manually
   restarting more nodes.

### No Leader Or Heavy Churn

1. Check whether any node reports `database_raft_is_leader_info == 1` for the
   affected partition.
2. If leadership keeps flipping, inspect recent increases in
   `database_raft_leader_changes_total`.
3. Correlate with network, restart, or NATS disruption events in logs.
4. Do not trust write availability until a stable leader is present and
   `database_raft_committed_index_info` is advancing again.

### Fresh Replica Bootstrap Failure

1. Check `database_replica_bootstrap_total{status="error"}`.
2. Check whether `database_checkpoint_loaded_ts_info` moved at all.
3. If load never succeeded, inspect checkpoint storage availability and the
   checkpoint object itself.
4. If load succeeded but install failed, inspect
   `database_snapshot_install_total{status="error"}` and logs around snapshot
   install/index rebuild.
5. If installs are succeeding but are slow, inspect
   `database_raft_snapshot_apply_seconds` and
   `database_raft_snapshot_apply_bytes` to determine whether the issue is
   snapshot size, disk apply time, or repeated re-installs.

### Replication Transport Backlog

1. Check `database_replication_transport_pending_messages_info`.
2. Compare `database_replication_transport_stream_sequence_info` and
   `database_replication_transport_consumer_sequence_info` to see whether the
   consumer is catching up or stuck.
3. Inspect `database_replication_transport_lag_seconds` to distinguish a slow
   but draining consumer from a consumer that is no longer making progress.
4. Correlate backlog growth with NATS availability, follower lag metrics, and
   any snapshot installs in progress.

### 2PC Failures

1. Check `database_two_phase_coordinator_errors_total` to see whether the
   failures are in `prepare` or `commit`.
2. If `prepare` retries are climbing, inspect
   `database_two_phase_prepare_retries_total`; this usually means timestamp
   allocation pressure or stale participant state.
3. Compare `database_two_phase_coordinator_seconds`,
   `database_two_phase_participants_total`, and
   `database_two_phase_prepare_attempts_total` to see whether the failures are
   tied to wider fanout or repeated prepare retries.
4. If `commit` errors are climbing, inspect the participant logs and confirm
   which partition owner is failing.
5. Treat repeated 2PC rollbacks as a correctness-risk signal even if retries
   hide the issue from clients.

## Scope Notes

- This document covers the operator signals we expose directly from the
  clustered backend.
- It intentionally does not prescribe a single dashboard product. Prometheus,
  Grafana, Datadog, or another metrics backend can all consume this surface.
- It also intentionally stops short of shipping packaged Prometheus alert
  rules. The metrics and runbooks are in place first; alert-rule files can be
  layered on later if we decide to standardize on Prometheus.
