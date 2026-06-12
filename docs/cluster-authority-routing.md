# Cluster Authority Routing

This document records the request-authority rules for the horizontally scaled
local backend. The goal is to make every externally reachable route explicit:
serve locally only when it is safe, forward when an owner path exists, and fail
closed otherwise.

## Authority Classes

The code-level registry for local-backend route authority lives in
`crates/local_backend/src/route_authority.rs`. It is intentionally typed so new
routes choose an authority class in code, not only in prose.

| Class                      | Meaning                                                                                                         | Current handling                                                                                             |
| -------------------------- | --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Any-node safe              | The route is static, health-only, or purely local observability.                                                | Serve locally.                                                                                               |
| Partition owner            | The route writes user data owned by one table partition.                                                        | Public mutations use the existing partition enforcement and mutation forwarding path.                        |
| Partition leader           | The route writes Raft-protected partition state.                                                                | Serve only on that partition's Raft leader or through leader forwarding.                                     |
| Coordinator owner          | The route touches deployment-global metadata or APIs without partition-specific ownership.                      | Serve only on partition 0 / Raft leader; replicas and non-owner partitions reject with `ServiceUnavailable`. |
| Follower-safe read         | The route is read-only and the node can prove its applied frontier is fresh enough for the requested timestamp. | Not generally available yet outside purpose-built read paths.                                                |
| Explicit forwarding        | The route has a purpose-built owner forwarding path.                                                            | The local handler may run on any node because it forwards first.                                             |
| External side-effect owner | The route triggers external storage, import/export, or delivery side effects.                                   | Needs a job/side-effect owner and idempotency model before it can run broadly.                               |
| Fail closed                | The route can mutate/read authoritative state but has no routing protocol.                                      | Reject on non-authority nodes until forwarding is added.                                                     |

## Migrated `RouterState` Routes

These routes use `SelectiveQueryForwardingApi`, which centralizes clustered
authority checks.

| Route surface                                           | Authority rule                                                                                                                                                                      |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/api/query`, `/api/query_at_ts`                        | Public latest queries route to the selective query authority when necessary. Timestamped queries execute through the same API path after the timestamp is chosen.                   |
| `/api/mutation`                                         | Public mutations preserve the existing replica/Raft forwarding path. Local-owner writes use the fast path, single remote-owner writes are routed to the owner through a one-participant 2PC path, and cross-partition writes use the normal 2PC coordinator. Missing owner address metadata still fails closed. |
| `/api/action`, `/http/*`                                | Coordinator owner until action execution has explicit clustered routing.                                                                                                            |
| `/api/function`, `/api/run/*`                           | Coordinator owner because the function type is not known before dispatch.                                                                                                           |
| `/api/query_ts`, `/api/query_batch` timestamp selection | Coordinator owner because latest timestamp selection must not be made from a stale follower/replica.                                                                                |
| `/api/storage/*`                                        | Coordinator owner until file metadata and storage authorization have explicit clustered routing.                                                                                    |
| `/api/sync`, `/api/{client_version}/sync`               | Coordinator owner for subscription setup until follower-safe subscription ownership is implemented. The router rejects non-authority nodes before accepting the WebSocket upgrade.   |

## Deploy `DeployRouterState` Routes

Deploy and CLI config routes use a smaller cluster-aware `DeployRouterState`
instead of the broad application assembly state.

| Route surface            | Authority rule                                                                                                                                                                        |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/api/deploy/*`          | Explicit forwarding. Deploy protocol handlers forward to partition 0 and then to the Raft leader where needed. `report_push_completed` is local observability.                        |
| `/api/get_config_hashes` | Any-node safe deploy introspection. The current CLI reads target-node module/config hashes before the modern deploy protocol runs so each node can materialize its local module view. |
| `/api/run_test_function` | Coordinator owner. This is a dashboard/MCP one-off function runner, not part of the deploy protocol.                                                                                  |

The removed old HTTP deploy endpoints are `/api/get_config`, `/api/push_config`,
`/api/prepare_schema`, and `/api/schema_state/*`. The current CLI callers use
`/api/get_config_hashes` and `/api/deploy/*` instead.

## Admin `AdminRouterState` Routes

Dashboard and platform admin routes use a smaller cluster-aware
`AdminRouterState` instead of the broad application assembly state.

| Route surface                                                                                                                                    | Authority rule                                                                                          |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| `/api/dashboard_openapi.json`, `/api/v1/openapi.json`                                                                                            | Any-node safe static schemas.                                                                           |
| Dashboard admin routes, environment variables, canonical URLs, scheduled job cancellation, and app metrics                                       | Coordinator owner because these routes read or write deployment-global metadata and function-log state. |
| `/api/v1/*` platform admin routes, including deployment info/state, canonical URLs, environment variables, and platform log-stream configuration | Coordinator owner unless a narrower follower-safe read or forwarding path is added.                     |

## Log And Observability `AdminRouterState` Routes

Log sink configuration and function-log observability routes use
`AdminRouterState` instead of the broad application assembly state.

| Route surface                                                                                                                             | Authority rule                                                                                                                                                                                                        |
| ----------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Deprecated `/api/logs/*` log-sink routes                                                                                                  | Coordinator owner because they mutate deployment-global log sink configuration and can trigger external delivery side effects.                                                                                        |
| `/api/v1/*` platform log-stream routes such as `create_log_stream`, `update_log_stream`, `delete_log_stream`, and webhook secret rotation | Coordinator owner because they mutate deployment-global log-stream configuration.                                                                                                                                     |
| `/api/stream_udf_execution`, `/api/stream_function_logs`, and `/api/app_metrics/*`                                                        | Coordinator owner because these read deployment-global function-log and app-metrics state. They are not documented as local-node-only metrics; any future local-node metrics should be named and documented as local. |

## Internal Action Callback `ActionCallbackRouterState` Routes

Internal action callbacks use a dedicated `ActionCallbackRouterState` instead of
the broad application assembly state.

| Route surface                                                                                                        | Authority rule                                                                                                                               |
| -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `/api/actions/*` internal query, mutation, action, scheduling, vector search, function-handle, and storage callbacks | Coordinator owner until callback reads and side effects have a narrower partition/index/storage ownership model or explicit forwarding path. |

## Import/Export `ImportExportRouterState` Routes

Snapshot import/export and streaming import/export routes use a dedicated
`ImportExportRouterState` instead of the broad application assembly state.

| Route surface                                                                                                                                                            | Authority rule                                                                                                                                                      |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/api/import`, `/api/import/*`, `/api/perform_import`, `/api/cancel_import`                                                                                              | Coordinator owner because snapshot imports mutate deployment-global schema/table state and object-upload job state.                                                 |
| `/api/export/*`                                                                                                                                                          | Coordinator owner because snapshot export jobs, expiration changes, cancellation, and archive retrieval need a consistent deployment-global export owner.           |
| `/api/streaming_import/*`                                                                                                                                                | Coordinator owner because streaming import writes schemas, tables, indexes, and bulk row mutations until per-table/partition fanout and idempotency are introduced. |
| `/api/document_deltas`, `/api/list_snapshot`, `/api/json_schemas`, `/api/test_streaming_export_connection`, `/api/get_tables_and_columns`, `/api/get_table_column_names` | Coordinator owner until streaming export has a follower-safe snapshot/frontier proof or explicit export owner forwarding.                                           |

## Final Router Shape

`BackendAppState` is the application assembly state used only for constructing
the local backend and serving local-process surfaces such as health checks,
static assets, and server bootstrap wiring. It is no longer used as a broad
`/api` route state.

Every externally reachable clustered `/api` route is now mounted through an
explicit smaller route state:

| Route state                 | Route families                                                                    |
| --------------------------- | --------------------------------------------------------------------------------- |
| `RouterState`               | Public API, browser API, sync, HTTP actions, and storage public surfaces.         |
| `DeployRouterState`         | Deploy and CLI config surfaces.                                                   |
| `AdminRouterState`          | Dashboard, platform admin, app metrics, log observability, and log sink surfaces. |
| `ActionCallbackRouterState` | Internal node action callback surfaces.                                           |
| `ImportExportRouterState`   | Snapshot and streaming import/export surfaces.                                    |

## Background Worker Authority

Foreground route authority is not enough on its own: background workers can
mutate the same deployment-global metadata or derived indexes without passing
through HTTP middleware. The local backend therefore chooses an
`ApplicationWorkerStartupPolicy` during `make_app`.

| Worker surface                                                                                         | Cluster-mode behavior                                                                                                                                      |
| ------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scheduled jobs and crons                                                                               | Start disabled, then run only on the coordinator singleton while partition 0 is Raft leader and has a fresh serving lease.                                  |
| Index backfill, fast-forward indexing, search/vector indexing, schema validation, table summaries       | Fail closed in clustered mode until each worker has a documented owner, failover, and idempotency model.                                                   |
| System-table cleanup, snapshot import, export, and migration workers                                    | Fail closed in clustered mode because they mutate deployment-global job/system state or external side-effect state without a clustered ownership protocol. |
| Usage gauges and log manager                                                                           | Continue to run as local observational/reporting plumbing. If they become authoritative state mutators, they must move into an explicit worker authority.   |

## Adding A Route

Every new local-backend route must choose one authority class before it is
exposed in a clustered build. If the route touches deployment metadata, file
metadata, subscriptions, function execution, imports, exports, or log sinks, do
not default to local execution on followers or non-owner partitions. Mount the
route on the narrowest explicit route state, add the matching authority
middleware, and either provide an explicit forwarding path or let the route fail
closed on non-authority nodes.
