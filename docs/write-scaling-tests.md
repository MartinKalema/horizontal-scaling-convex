# Write Scaling Tests: 56 Assertions Across 23 Categories

Based on how CockroachDB, TiDB, YugabyteDB, Google Spanner, and Vitess test their distributed read/write scaling.

## Test Results

```
ALL 56 TESTS PASSED

2,365 messages | 1,814 tasks | 16 users | 8 projects
1,440 sustained writes/node in 30 seconds
Full cluster restart recovery
Zero data loss
```

## Test Categories

### Correctness (Jepsen Patterns)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 1 | Cross-partition data verification | Vitess VDiff | 2 | Both nodes see all data from both partitions |
| 2 | Bank invariant — single table | CockroachDB Jepsen bank | 3 | Numeric totals violated by replication |
| 3 | Bank invariant — multi-table | TiDB bank-multitable | 3 | Cross-table invariants broken across partitions |
| 14 | Sequential ordering | Jepsen sequential | 1 | Writes by one client visible out of order |
| 15 | Set completeness | Jepsen set | 2 | Lost inserts — element written but never visible |
| 16 | Concurrent counter | Jepsen counter / YugabyteDB | 3 | Phantom counts or lost increments |
| 22 | Duplicate insert idempotency | Custom | 1 | Insert deduplication or corruption |

### Partition Enforcement (Vitess Single Mode)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 4 | Partition enforcement | Vitess Single mode | 5 | Wrong-partition writes accepted, phantom data |

### Scaling and Performance (CockroachDB KV)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 5 | Concurrent write scaling | CockroachDB KV 95 | 3 | Data loss under parallel writes |
| 10 | Rapid-fire writes (50/node) | Jepsen stress | 5 | Crashes or lost writes under burst load |
| 21 | Sustained writes (30 seconds) | CockroachDB endurance | 5 | Replication lag, data loss, node crashes under sustained load |

### Consistency (TiDB / CockroachDB Patterns)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 6 | Monotonic reads | TiDB monotonic | 1 | Values going backward (stale reads) |
| 11 | Write-then-immediate-read | TiDB Jepsen stale read | 1 | Read returns stale data on same node |
| 17 | Write-then-cross-node-read | TiDB Jepsen stale read | 1 | Cross-node stale read after write |
| 18 | Interleaved cross-partition reads | Read skew detection | 1 | Nodes return inconsistent snapshots |

### Two-Phase Commit (Vitess 2PC)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 9 | Cross-partition atomic write | Vitess 2PC | 4 | Partial commits, non-atomic cross-partition writes |

### Atomicity

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 19 | Large batch write (50 docs) | Custom | 2 | Partial batch — some docs written, others lost |

### Chaos and Recovery (TiDB kill-9 / CockroachDB Nemesis)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 7 | Single node restart | TiDB kill-9 | 2 | Data loss or inability to write after restart |
| 12 | Double node restart | CockroachDB nemesis | 1 | Corruption from rapid consecutive restarts |
| 20 | Full cluster restart | CockroachDB nemesis | 2 | Data loss when all nodes go down simultaneously |

### Invariant Preservation (CockroachDB workload check)

| # | Test | Source | Assertions | What it catches |
| --- | --- | --- | --- | --- |
| 8 | Idempotent re-run | CockroachDB workload check | 2 | Corruption from repeated operations |
| 13 | Post-chaos invariant check | CockroachDB workload check | 3 | Invariants broken by stress and restarts |
| 23 | Final exhaustive invariant check | CockroachDB workload check | 3 | Any invariant violation after all 22 previous tests |

## Test Details

### Test 1: Cross-Partition Data Verification (Vitess VDiff)

Write 3 messages + 2 users to Node A (partition 0), 2 projects + 3 tasks to Node B (partition 1). After NATS replication, both nodes must see identical counts. Row-by-row equivalence check across nodes.

### Test 2: Bank Invariant — Single Table (CockroachDB Jepsen bank)

Create projects with known budgets ($10,000 + $25,000) on Node B. After replication, sum budgets on both nodes. Both must return the same total. No money created or destroyed.

### Test 3: Bank Invariant — Multi-Table (TiDB bank-multitable)

Create users with salaries ($60,000 + $80,000) on Node A, projects with budgets on Node B. Compute total compensation (salaries + budgets) on both nodes. Both must agree. Tests cross-table invariants across partitions.

### Test 4: Partition Enforcement (Vitess Single Mode)

Attempt writes to wrong-partition tables from each node (4 combinations). All must fail with partition identification. Verify zero phantom data created by rejected writes.

### Test 5: Concurrent Write Scaling (CockroachDB KV)

20 messages to Node A + 20 tasks to Node B in parallel. Measure throughput (~170 writes/sec). After replication, both nodes must see all 40 writes. Zero data loss.

### Test 6: Monotonic Reads (TiDB monotonic)

Write incrementing sequence values (1-10) to Node B. After each write, read from Node A. Each successive read must return a value >= the previous. Values must never go backward.

### Test 7: Node Restart Recovery (TiDB kill-9)

Restart Node B. Write to Node A during downtime. After recovery, Node B must see the write made during its downtime. Node B must be able to write after recovery.

### Test 8: Idempotent Re-Run (CockroachDB workload check)

Write additional data and verify counts are correct. Both nodes must still be converged. No corruption from repeated operations.

### Test 9: Two-Phase Commit (Vitess 2PC)

Single mutation writes to messages (partition 0) AND tasks (partition 1). Both must be committed atomically. Both nodes must see the data. Messages and tasks must increment equally (invariant).

### Test 10: Rapid-Fire Writes (Jepsen Stress)

50 rapid writes per node concurrently (100 total). Both nodes must survive (no crash). All 100 writes must be present. Nodes must converge.

### Test 11: Write-Then-Immediate-Read (Stale Read Detection)

Write 5 messages and immediately read on the same node. Read must reflect all 5 writes. Catches stale reads that TiDB Jepsen found.

### Test 12: Double Node Restart (Crash Recovery Stress)

Restart Node B twice in succession, writing to Node A during each downtime. After second recovery, Node B must see all writes including those made during both downtimes.

### Test 13: Post-Chaos Invariant Check (CockroachDB workload check)

After stress tests and restarts, verify budget invariant, cross-table invariant, and full table convergence. All numeric totals must match between nodes.

### Test 14: Sequential Ordering (Jepsen sequential)

Write "first", "second", "third" sequentially from one client. Read back. Must appear in exact order. CockroachDB Jepsen found disjoint records visible out of order.

### Test 15: Set Completeness (Jepsen set)

Insert 100 unique numbered elements. Read back all. All 100 must be present on Node A. After replication, all 100 must be present on Node B. No lost inserts.

### Test 16: Concurrent Counter (Jepsen counter / YugabyteDB)

30 concurrent increments on Node A (messages) + 30 on Node B (tasks). Each node's counter must reach 30. Node A's counter must replicate to Node B.

### Test 17: Write-Then-Cross-Node-Read

Write to Node A, read from Node B after replication delay. Node B must see the write. Catches cross-node stale reads.

### Test 18: Interleaved Cross-Partition Reads (Read Skew Detection)

Read from both nodes 10 times rapidly. Every read pair must agree. Catches read skew where nodes return inconsistent snapshots of the same data.

### Test 19: Large Batch Write Atomicity

Single mutation writes 50 documents. All 50 must appear on Node A. After replication, all 50 must appear on Node B. No partial batches.

### Test 20: Full Cluster Restart

Kill Node A, then Node B. Restart both. After recovery and redeploy, all data must be intact. Both nodes must converge.

### Test 21: Sustained Writes (30 seconds)

Write continuously to both nodes for 30 seconds. Both nodes must survive. All writes must be present. Nodes must converge. Tests replication under sustained load (~1,440 writes per node).

### Test 22: Duplicate Insert Idempotency

Insert the same data twice. Both rows must persist (Convex has no unique constraints). Verifies no deduplication bugs.

### Test 23: Final Exhaustive Invariant Check

After all 22 previous tests including chaos, stress, batch writes, and restarts: verify every table matches between nodes, budget invariant preserved, cross-table invariant preserved. The ultimate correctness gate.

## Sources

### CockroachDB

- [Jepsen CockroachDB analysis](https://jepsen.io/analyses/cockroachdb-beta-20160829)
- [cockroach workload](https://www.cockroachlabs.com/docs/stable/cockroach-workload)
- [Stress testing for global scale](https://www.cockroachlabs.com/blog/how-we-stress-test-and-benchmark-cockroachdb-for-global-scale/)
- [DIY Jepsen testing](https://www.cockroachlabs.com/blog/diy-jepsen-testing-cockroachdb/)
- [Roachtest README](https://github.com/cockroachdb/cockroach/blob/master/pkg/cmd/roachtest/README.md)

### TiDB

- [Jepsen TiDB 2.1.7](https://jepsen.io/analyses/tidb-2.1.7)
- [TiDB meets Jepsen](https://www.pingcap.com/blog/tidb-meets-jepsen/)
- [TiPocket testing toolkit](https://github.com/pingcap/tipocket)
- [Chaos engineering at PingCAP](https://www.pingcap.com/blog/chaos-practice-in-tidb/)

### YugabyteDB

- [Jepsen YugabyteDB](https://jepsen.io/analyses/yugabyte-db-1.1.9)
- [YugabyteDB Jepsen testing docs](https://docs.yugabyte.com/stable/benchmark/resilience/jepsen-testing/)

### Vitess

- [VDiff v2](https://vitess.io/blog/2022-11-22-vdiff-v2/)
- [Distributed Transactions](https://vitess.io/docs/22.0/reference/features/distributed-transaction/)
- [Atomic Distributed Transactions RFC](https://github.com/vitessio/vitess/issues/16245)

### Database Anomaly Theory

- [Read and Write Skew phenomena](https://vladmihalcea.com/a-beginners-guide-to-read-and-write-skew-phenomena/)
- [Critique of ANSI SQL Isolation Levels](https://blog.acolyer.org/2016/02/24/a-critique-of-ansi-sql-isolation-levels/)
