# Cluster Authority Routing

This document records the request-authority rules for the horizontally scaled
local backend. The goal is to make every externally reachable route explicit:
serve locally only when it is safe, forward when an owner path exists, and fail
closed otherwise.

## Authority Classes

The code-level registry for local-backend route authority lives in
`crates/local_backend/src/route_authority.rs`. It is intentionally typed so new
routes choose an authority class in code, not only in prose.

| Class | Meaning | Current handling |
| --- | --- | --- |
| Any-node safe | The route is static, health-only, or purely local observability. | Serve locally. |
| Partition owner | The route writes user data owned by one table partition. | Public mutations use the existing partition enforcement and mutation forwarding path. |
| Partition leader | The route writes Raft-protected partition state. | Serve only on that partition's Raft leader or through leader forwarding. |
| Coordinator owner | The route touches deployment-global metadata or APIs without partition-specific ownership. | Serve only on partition 0 / Raft leader; replicas and non-owner partitions reject with `ServiceUnavailable`. |
| Follower-safe read | The route is read-only and the node can prove its applied frontier is fresh enough for the requested timestamp. | Not generally available yet outside purpose-built read paths. |
| Explicit forwarding | The route has a purpose-built owner forwarding path. | The local handler may run on any node because it forwards first. |
| External side-effect owner | The route triggers external storage, import/export, or delivery side effects. | Needs a job/side-effect owner and idempotency model before it can run broadly. |
| Not yet safe | The route can mutate/read authoritative state but has no routing protocol. | Reject on non-authority nodes until forwarding is added. |

## Migrated `RouterState` Routes

These routes use `SelectiveQueryForwardingApi`, which centralizes clustered
authority checks.

| Route surface | Authority rule |
| --- | --- |
| `/api/query`, `/api/query_at_ts` | Public latest queries route to the selective query authority when necessary. Timestamped queries execute through the same API path after the timestamp is chosen. |
| `/api/mutation` | Public mutations preserve the existing replica/Raft forwarding and partition enforcement path. Non-owner writes are rejected by the write path, not by a generic coordinator guard. |
| `/api/action`, `/http/*` | Coordinator owner until action execution has explicit clustered routing. |
| `/api/function`, `/api/run/*` | Coordinator owner because the function type is not known before dispatch. |
| `/api/query_ts`, `/api/query_batch` timestamp selection | Coordinator owner because latest timestamp selection must not be made from a stale follower/replica. |
| `/api/storage/*` | Coordinator owner until file metadata and storage authorization have explicit clustered routing. |
| `/api/sync`, `/{client_version}/sync` | Coordinator owner for subscription setup until follower-safe subscription ownership is implemented. |

## Deploy `DeployRouterState` Routes

Deploy and CLI config routes use a smaller cluster-aware `DeployRouterState`
instead of the full `LocalAppState` route tree.

| Route surface | Authority rule |
| --- | --- |
| `/api/deploy2/*` | Explicit forwarding. Deploy metadata handlers forward to partition 0 and then to the Raft leader where needed. `report_push_completed` is local observability. |
| `/api/get_config`, `/api/get_config_hashes` | Any-node safe deploy introspection. The current CLI reads target-node module/config hashes before the modern `deploy2` push so each node can materialize its local module view. |
| `/api/push_config`, `/api/prepare_schema`, `/api/run_test_function`, `/api/schema_state/*` | Coordinator owner unless a narrower forwarding path is added. |

## Admin `AdminRouterState` Routes

Dashboard and platform admin routes use a smaller cluster-aware
`AdminRouterState` instead of the full `LocalAppState` route tree.

| Route surface | Authority rule |
| --- | --- |
| `/api/dashboard_openapi.json`, `/api/v1/openapi.json` | Any-node safe static schemas. |
| Dashboard admin routes, environment variables, canonical URLs, scheduled job cancellation, and app metrics | Coordinator owner because these routes read or write deployment-global metadata and function-log state. |
| `/api/v1/*` platform admin routes, including deployment info/state, canonical URLs, environment variables, and platform log-stream configuration | Coordinator owner unless a narrower follower-safe read or forwarding path is added. |

## Internal Action Callback `ActionCallbackRouterState` Routes

Internal action callbacks use a dedicated `ActionCallbackRouterState` instead
of the full `LocalAppState` route tree.

| Route surface | Authority rule |
| --- | --- |
| `/api/actions/*` internal query, mutation, action, scheduling, vector search, function-handle, and storage callbacks | Coordinator owner until callback reads and side effects have a narrower partition/index/storage ownership model or explicit forwarding path. |

## Import/Export `ImportExportRouterState` Routes

Snapshot import/export and streaming import/export routes use a dedicated
`ImportExportRouterState` instead of the full `LocalAppState` route tree.

| Route surface | Authority rule |
| --- | --- |
| `/api/import`, `/api/import/*`, `/api/perform_import`, `/api/cancel_import` | Coordinator owner because snapshot imports mutate deployment-global schema/table state and object-upload job state. |
| `/api/export/*` | Coordinator owner because snapshot export jobs, expiration changes, cancellation, and archive retrieval need a consistent deployment-global export owner. |
| `/api/streaming_import/*` | Coordinator owner because streaming import writes schemas, tables, indexes, and bulk row mutations until per-table/partition fanout and idempotency are introduced. |
| `/api/document_deltas`, `/api/list_snapshot`, `/api/json_schemas`, `/api/test_streaming_export_connection`, `/api/get_tables_and_columns`, `/api/get_table_column_names` | Coordinator owner until streaming export has a follower-safe snapshot/frontier proof or explicit export owner forwarding. |

## Unmigrated `LocalAppState` Routes

Unmigrated `/api` routes bypass `ApplicationApi`, so they are classified by
`route_authority.rs` and protected by the unmigrated-route authority middleware
in `router.rs`.

| Route surface | Authority rule |
| --- | --- |
| Deprecated `/api/logs/*` log-sink routes | Coordinator owner unless a narrower forwarding path is added. |

## Adding A Route

Every new local-backend route must choose one authority class before it is
exposed in a clustered build. If the route touches deployment metadata, file
metadata, subscriptions, function execution, imports, exports, or log sinks, do
not default to local execution on followers or non-owner partitions. Add an
explicit forwarding path or let the route fail closed on non-authority nodes.
