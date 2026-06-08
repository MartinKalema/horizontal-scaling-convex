# Cluster Authority Routing

This document records the request-authority rules for the horizontally scaled
local backend. The goal is to make every externally reachable route explicit:
serve locally only when it is safe, forward when an owner path exists, and fail
closed otherwise.

## Authority Classes

| Class | Meaning | Current handling |
| --- | --- | --- |
| Any-node safe | The route is static, health-only, or purely local observability. | Serve locally. |
| Partition owner | The route writes user data owned by one table partition. | Public mutations use the existing partition enforcement and mutation forwarding path. |
| Coordinator owner | The route touches deployment-global metadata or APIs without partition-specific ownership. | Serve only on partition 0 / Raft leader; replicas and non-owner partitions reject with `ServiceUnavailable`. |
| Explicit forwarding | The route has a purpose-built owner forwarding path. | The local handler may run on any node because it forwards first. |
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

## Legacy `LocalAppState` Routes

Legacy `/api` routes bypass `ApplicationApi`, so they are protected by the
legacy authority middleware in `router.rs`.

| Route surface | Authority rule |
| --- | --- |
| `/api/deploy2/*` | Explicit forwarding. Deploy metadata handlers forward to partition 0 and then to the Raft leader where needed. `report_push_completed` is local observability. |
| `/api/dashboard_openapi.json`, `/api/v1/openapi.json` | Any-node safe static schemas. |
| `/api/get_config`, `/api/get_config_hashes` | Any-node safe deploy introspection. The current CLI reads target-node module/config hashes before the modern `deploy2` push so each node can materialize its local module view. |
| Dashboard, platform, import/export, streaming import/export, log sinks, internal action callbacks, and mutating legacy CLI routes such as `/api/push_config` | Coordinator owner unless a narrower forwarding path is added. |

## Adding A Route

Every new local-backend route must choose one authority class before it is
exposed in a clustered build. If the route touches deployment metadata, file
metadata, subscriptions, function execution, imports, exports, or log sinks, do
not default to local execution on followers or non-owner partitions. Add an
explicit forwarding path or let the route fail closed on non-authority nodes.
