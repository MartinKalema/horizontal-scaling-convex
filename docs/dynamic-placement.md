# Dynamic Placement Control Plane

Issue #130 is the path from startup-configured partitions to online placement
changes. The current implementation is still static: nodes read
`PARTITION_ID`, `PARTITION_MAP`, `NUM_PARTITIONS`, and `NODE_ADDRESSES` at
startup. This document records the safety contract added before online
rebalancing is introduced.

## Current State

Cluster routing state is now represented in three layers:

- `PlacementMetadata` is the versioned control-plane model. It records where
  the metadata came from, how many partitions exist, and which logical targets
  are owned by which partitions. Placement is logical only; it does not carry
  node addresses.
- `PartitionMap` is the runtime lookup object used by commit, routing, and 2PC
  paths.
- `PlacementState` is the refreshable in-process holder shared by the committer
  and commit client. Each transaction snapshots the current `PartitionMap`
  before routing so a placement refresh cannot change ownership halfway through
  one commit.
- `MembershipSnapshot` in `crates/database/src/membership.rs` is the physical
  node directory. It records node IDs, partition membership, gRPC endpoints,
  generation, and drain state. 2PC routing derives its `partition -> peer
  addresses` view from this directory.

The current metadata source is still static process config. Version `0` means
the startup-configured map. Operators may now set `PARTITION_MAP_VERSION` and
must bump it whenever table ownership changes.

When NATS is configured, startup now initializes or loads the shared
`convex_placement/current` NATS KV record and refreshes `PlacementState` from
that authoritative control-plane record. The static env map remains the
bootstrap fallback so `Database::load` can start before any external placement
source is available.

When a partitioned node also has NATS, startup initializes or loads the shared
`convex_membership/current` NATS KV record and refreshes the committer's 2PC
routing addresses from that membership directory. `NODE_ADDRESSES` is now only a
bootstrap seed for local/dev deployments or the first membership snapshot. Each
node can also advertise its own gRPC/HTTP/Raft endpoints, generation, drain
state, and heartbeat lease into that shared directory. A background refresh loop
renews the local node's lease and refreshes in-process routing addresses from
the live, non-draining membership entries. Once the shared membership directory
exists, a node can load physical endpoints from NATS instead of every process
carrying the full topology in its env.

Bootstrap records without heartbeat leases are treated as live so a brand-new
dev cluster can initialize before every node has self-registered. In the normal
cluster path, each node overwrites its bootstrap entry with the same stable
`node_id`, a fresh advertised endpoint, and a renewable lease. Operators should
prefer a small seed/control-plane config over a hand-maintained full topology.

Placement metadata is control-plane state, not an application API. Nodes,
routers, and operators may reason about placement versions and ownership, but
queries, mutations, actions, and subscriptions should continue to target one
logical Convex database without shard or partition hints.

Cross-partition 2PC `Prepare` requests carry the coordinator's placement
version. The participant compares it with its local `PartitionMap` version and
rejects mismatches before staging writes. This prevents a stale coordinator from
preparing writes against an old owner while the cluster is rolling to a new
placement map.

`CommitPrepared` and `RollbackPrepared` intentionally do not reject on placement
version. Once a participant has prepared a transaction, finishing or aborting
that exact transaction must remain possible even if placement metadata changes.

## Operator Rule

When changing `PARTITION_MAP`, roll all nodes with the same
`PARTITION_MAP_VERSION`. If one node is stale, cross-partition writes involving
that node fail fast with a placement-version mismatch instead of silently
preparing against the wrong owner.

## Remaining #130 Work

- Store placement metadata in a replicated system table instead of env vars.
- Add an authority/lease model for who may publish a new placement version.
- Decide whether NATS KV remains the placement authority long term or becomes a
  bootstrap/transport layer for a replicated placement system table.
- Move membership from a whole-snapshot KV record to independent per-node keys
  or another append/update-friendly control-plane representation. The current
  implementation uses CAS on the whole snapshot, which is adequate for the
  local harness but not the final large-cluster shape.
- Extend node membership to dynamic Raft peer discovery and membership changes.
  Recording `raft_addr` is not enough; adding/removing voters requires Raft
  learner catch-up, promotion, and removal through consensus configuration
  changes.
- Add an online movement workflow: freeze source writes for the moved range or
  table, copy/catch up data, publish the new placement version, then unfreeze.
- Add stale-map refresh behavior across every routing surface; 2PC prepare
  already refreshes from the placement store on version mismatch.
- Add operational procedures for add-node, remove-node, and rebalance.

Large distributed databases use the same shape at a larger scale: placement is
metadata with an owner, routers carry or observe a metadata version/epoch, stale
routers refresh, and data movement is separate from request routing.
