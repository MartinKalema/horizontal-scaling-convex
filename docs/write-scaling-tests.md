# Write Scaling Tests: 77 Assertions Across 37 Categories

Based on how CockroachDB, TiDB, YugabyteDB, Google Spanner, and Vitess test their distributed read/write scaling.

## Test Results

```
ALL 77 TESTS PASSED

3,823 messages | 3,069 tasks | 8 users | 4 projects
1,390 sustained writes/node in 30 seconds
200-doc batch replicated atomically
NATS partition survived
Full cluster restart recovery
TSO monotonic after restart
Zero data loss
```

## Test Categories by Domain

### Correctness — Jepsen Patterns (7 categories, 15 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 1 | Cross-partition data verification | Vitess VDiff | 2 | Replication fails to propagate data |
| 2 | Bank invariant — single table | CockroachDB Jepsen bank | 3 | Numeric totals violated by replication |
| 3 | Bank invariant — multi-table | TiDB bank-multitable | 3 | Cross-table invariants broken across partitions |
| 14 | Sequential ordering | Jepsen sequential | 1 | Writes by one client visible out of order |
| 15 | Set completeness (100 elements) | Jepsen set | 2 | Lost inserts — element written but never visible |
| 16 | Concurrent counter | Jepsen counter / YugabyteDB | 3 | Phantom counts or lost increments |
| 22 | Duplicate insert idempotency | Custom | 1 | Insert deduplication or corruption |

### Partition Enforcement (1 category, 5 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 4 | Partition enforcement | Vitess Single mode | 5 | Wrong-partition writes accepted, phantom data |

### Scaling and Performance (3 categories, 13 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 5 | Concurrent write scaling | CockroachDB KV 95 | 3 | Data loss under parallel writes |
| 10 | Rapid-fire writes (50/node) | Jepsen stress | 5 | Crashes or lost writes under burst load |
| 21 | Sustained writes (30 seconds) | CockroachDB endurance | 5 | Replication lag, data loss, node crashes under sustained load |

### Consistency (5 categories, 5 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 6 | Monotonic reads | TiDB monotonic | 1 | Values going backward (stale reads) |
| 11 | Write-then-immediate-read | TiDB Jepsen stale read | 1 | Read returns stale data on same node |
| 17 | Write-then-cross-node-read | TiDB Jepsen stale read | 1 | Cross-node stale read after write |
| 18 | Interleaved cross-partition reads | Read skew detection | 1 | Nodes return inconsistent snapshots |
| 33 | Read during active replication | Custom | 1 | Read counts go backward during delta apply |

### Two-Phase Commit and Atomicity (3 categories, 8 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 9 | Cross-partition atomic write | Vitess 2PC | 4 | Partial commits, non-atomic cross-partition writes |
| 19 | Large batch write (50 docs) | Custom | 2 | Partial batch — some docs written, others lost |
| 29 | Max batch size (200 docs) | Boundary | 2 | Large transactions breaking replication |

### Chaos and Recovery (5 categories, 10 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 7 | Single node restart | TiDB kill-9 | 2 | Data loss or inability to write after restart |
| 12 | Double node restart | CockroachDB nemesis | 1 | Corruption from rapid consecutive restarts |
| 20 | Full cluster restart | CockroachDB nemesis | 2 | Data loss when all nodes go down simultaneously |
| 26 | NATS partition simulation | Chaos Mesh network partition | 2 | Nodes crash when replication log disconnects |
| 34 | TSO monotonicity after restart | TiDB TSO | 2 | TSO counter regresses after node crash |

### Register and Document Operations (3 categories, 6 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 24 | Single-key register (read/write/CAS) | CockroachDB Jepsen register | 2 | Linearizability violation on single document |
| 35 | Single document read-modify-write | CockroachDB register | 2 | Document state inconsistency after multiple updates |
| 36 | Write skew detection | CockroachDB Jepsen G2 | 1 | Concurrent disjoint updates violate constraints |

### Deploy Safety (3 categories, 4 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 27 | Write during deploy | Custom | 1 | Deploy corrupts in-flight data |
| 32 | Rapid deploy cycle (3x with writes) | Custom | 1 | Repeated deploys cause data loss |
| 25 | Disjoint record ordering | CockroachDB Jepsen comments | 1 | Records from different partitions invisible after write |

### Boundary and Edge Cases (3 categories, 5 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 28 | Empty table cross-node query | Boundary | 1 | Empty results inconsistency between nodes |
| 30 | Null and empty field values | Boundary | 2 | Special values corrupted by replication |
| 31 | Concurrent writes from both nodes | Race condition | 1 | Simultaneous data creation causes divergence |

### Invariant Preservation (3 categories, 8 assertions)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 8 | Idempotent re-run | CockroachDB workload check | 2 | Corruption from repeated operations |
| 13 | Post-chaos invariant check | CockroachDB workload check | 3 | Invariants broken by stress and restarts |
| 23 | Mid-suite exhaustive check | CockroachDB workload check | 3 | Any invariant violation after tests 1-22 |
| 37 | Ultimate final invariant check | CockroachDB workload check | 2 | Any violation after all 36 previous tests |

## Full Test List

| # | Test | Assertions |
| --- | --- | --- |
| 1 | Cross-partition data verification (Vitess VDiff) | 2 |
| 2 | Bank invariant — single table (CockroachDB Jepsen bank) | 3 |
| 3 | Bank invariant — multi-table (TiDB bank-multitable) | 3 |
| 4 | Partition enforcement (Vitess Single mode) | 5 |
| 5 | Concurrent write scaling (CockroachDB KV) | 3 |
| 6 | Monotonic reads (TiDB monotonic) | 1 |
| 7 | Node restart recovery (TiDB kill-9) | 2 |
| 8 | Idempotent re-run (CockroachDB workload check) | 2 |
| 9 | Two-phase commit cross-partition (Vitess 2PC) | 4 |
| 10 | Rapid-fire writes 50/node (Jepsen stress) | 5 |
| 11 | Write-then-immediate-read (stale read detection) | 1 |
| 12 | Double node restart (CockroachDB nemesis) | 1 |
| 13 | Post-chaos invariant check (workload check) | 3 |
| 14 | Sequential ordering (Jepsen sequential) | 1 |
| 15 | Set completeness 100 elements (Jepsen set) | 2 |
| 16 | Concurrent counter (Jepsen counter / YugabyteDB) | 3 |
| 17 | Write-then-cross-node-read (cross-node stale) | 1 |
| 18 | Interleaved cross-partition reads (read skew) | 1 |
| 19 | Large batch write 50 docs (atomicity) | 2 |
| 20 | Full cluster restart (CockroachDB nemesis) | 2 |
| 21 | Sustained writes 30 seconds (endurance) | 5 |
| 22 | Duplicate insert idempotency | 1 |
| 23 | Mid-suite exhaustive invariant check | 3 |
| 24 | Single-key register (CockroachDB Jepsen register) | 2 |
| 25 | Disjoint record ordering (CockroachDB Jepsen comments) | 1 |
| 26 | NATS partition simulation (Chaos Mesh) | 2 |
| 27 | Write during deploy | 1 |
| 28 | Empty table cross-node query (boundary) | 1 |
| 29 | Max batch size 200 docs (boundary) | 2 |
| 30 | Null and empty field values (boundary) | 2 |
| 31 | Concurrent writes from both nodes (race) | 1 |
| 32 | Rapid deploy cycle 3x (stability) | 1 |
| 33 | Read during active replication (consistency) | 1 |
| 34 | TSO monotonicity after restart | 2 |
| 35 | Single document read-modify-write (register) | 2 |
| 36 | Write skew detection (G2 anomaly) | 1 |
| 37 | Ultimate final invariant check | 2 |
| | **Total** | **77** |

## Sources

### CockroachDB

- [Jepsen CockroachDB analysis](https://jepsen.io/analyses/cockroachdb-beta-20160829)
- [Nightly Jepsen test lessons](https://www.cockroachlabs.com/blog/jepsen-tests-lessons/)
- [cockroach workload](https://www.cockroachlabs.com/docs/stable/cockroach-workload)
- [Stress testing for global scale](https://www.cockroachlabs.com/blog/how-we-stress-test-and-benchmark-cockroachdb-for-global-scale/)
- [DIY Jepsen testing](https://www.cockroachlabs.com/blog/diy-jepsen-testing-cockroachdb/)
- [Roachtest README](https://github.com/cockroachdb/cockroach/blob/master/pkg/cmd/roachtest/README.md)

### TiDB

- [Jepsen TiDB 2.1.7](https://jepsen.io/analyses/tidb-2.1.7)
- [TiDB meets Jepsen](https://www.pingcap.com/blog/tidb-meets-jepsen/)
- [TiPocket testing toolkit](https://github.com/pingcap/tipocket)
- [Chaos engineering at PingCAP](https://www.pingcap.com/blog/chaos-practice-in-tidb/)
- [Chaos Mesh fault types](https://chaos-mesh.org/docs/basic-features/)

### YugabyteDB

- [Jepsen YugabyteDB 1.1.9](https://jepsen.io/analyses/yugabyte-db-1.1.9)
- [YugabyteDB Jepsen testing docs](https://docs.yugabyte.com/stable/benchmark/resilience/jepsen-testing/)

### Vitess

- [VDiff v2](https://vitess.io/blog/2022-11-22-vdiff-v2/)
- [Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)
- [Atomic Distributed Transactions RFC](https://github.com/vitessio/vitess/issues/16245)

### Jepsen Framework

- [Elle transaction checker](https://github.com/jepsen-io/elle)
- [Jepsen analyses](https://jepsen.io/analyses)
- [Jepsen framework](https://github.com/jepsen-io/jepsen)

### Database Anomaly Theory

- [Read and Write Skew phenomena](https://vladmihalcea.com/a-beginners-guide-to-read-and-write-skew-phenomena/)
- [Critique of ANSI SQL Isolation Levels](https://blog.acolyer.org/2016/02/24/a-critique-of-ansi-sql-isolation-levels/)
