# Dynamic Placement Control Plane

Issue #104 is the path from startup-configured partitions to online placement
changes. The current implementation is still static: nodes read
`PARTITION_ID`, `PARTITION_MAP`, `NUM_PARTITIONS`, and `NODE_ADDRESSES` at
startup. This document records the safety contract added before online
rebalancing is introduced.

## Current State

Placement ownership is now represented in two layers in
`crates/database/src/partition.rs`:

- `PlacementMetadata` is the versioned control-plane model. It records where
  the metadata came from, how many partitions exist, and which logical targets
  are owned by which partitions.
- `PartitionMap` is the runtime lookup object used by commit, routing, and 2PC
  paths.
- `PlacementState` is the refreshable in-process holder shared by the committer
  and commit client. Each transaction snapshots the current `PartitionMap`
  before routing so a placement refresh cannot change ownership halfway through
  one commit.

The current metadata source is still static process config. Version `0` means
the startup-configured map. Operators may now set `PARTITION_MAP_VERSION` and
must bump it whenever table ownership changes.

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

## Remaining #104 Work

- Store placement metadata in a replicated system table instead of env vars.
- Add an authority/lease model for who may publish a new placement version.
- Plug `PlacementState` into that authoritative source so nodes refresh newer
  placement metadata when they observe a stale-version rejection.
- Add node membership metadata so new partitions can be added without editing
  every node config by hand.
- Add an online movement workflow: freeze source writes for the moved range or
  table, copy/catch up data, publish the new placement version, then unfreeze.
- Add stale-map refresh behavior so clients/nodes retry after loading the newer
  placement version.
- Add operational procedures for add-node, remove-node, and rebalance.

Large distributed databases use the same shape at a larger scale: placement is
metadata with an owner, routers carry or observe a metadata version/epoch, stale
routers refresh, and data movement is separate from request routing.
