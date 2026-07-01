# Resolver-Style Conflict Checking

Issue: #131

## Goal

Convex's transaction contract is serializable ACID execution over arbitrary
TypeScript mutations. Horizontal write scaling must preserve that contract even
when a transaction reads data owned by one partition and writes data owned by
another.

The long-term direction is resolver-style conflict checking: route read/write
set validation to the owner of the affected table/range/key, rather than relying
only on a coordinator's local replica freshness.

## Current Baseline

The current system still uses the existing 2PC machinery for cross-partition
transactions:

1. The coordinator allocates one commit timestamp.
2. Participants validate their local read set and stage prepared writes.
3. A durable decision commits or rolls back the transaction.

This is still a 2PC protocol, not a full FoundationDB-style resolver subsystem.
The important change is that prepare participation is no longer based only on
write ownership.

## First Slice Implemented

Prepare participants now include:

- partitions that own one or more writes, and
- partitions that own tables read by the transaction.

That means a mutation that writes table `messages` on partition 0 but reads
table `projects` on partition 1 is no longer treated as a local fast-path
commit. Partition 1 participates as a read-owner resolver, receives no writes,
and validates that its owned read ranges have not changed between the
transaction begin timestamp and the assigned prepare timestamp.

This closes a correctness gap where a coordinator could previously validate
against its local replica and miss a concurrent owner-side write that had not
arrived through replication yet.

## Conservative Behavior

Read-owner participants currently use the same prepare/commit/rollback path as
write participants. They stage an empty prepared write and resolve through the
normal 2PC cleanup path.

This is intentionally conservative:

- correctness comes first;
- read-owner validation happens at the owner partition;
- clusters without `NODE_ADDRESSES` fail closed instead of falling back to local
  replica validation.

The tradeoff is that read-only resolver participants briefly participate in the
2PC lifecycle even though they do not write documents. A later optimization can
replace this with a dedicated validation-only resolver RPC once the correctness
contract is proven.

## Remaining Work

- Add a dedicated resolver/check-read-set RPC that does not stage empty writes.
- Split resolver ownership from table-level write ownership once placement moves
  beyond static table maps.
- Track resolver latency, retry rate, and per-partition conflict load.
- Extend cluster tests to cover concurrent owner-side writes racing with
  cross-partition read/write mutations.
- Keep 2PC as the durable apply protocol until the resolver layer has its own
  recovery and duplicate-message semantics.

## Non-Goals

- Do not remove 2PC in this slice.
- Do not allow application developers to choose or observe resolver shards.
- Do not treat selective delivery as a substitute for conflict checking.
