# Write Scaling Tests: Verifying Partitioned Multi-Writer Architecture

Based on how CockroachDB, TiDB, and Vitess test their distributed read/write scaling.

## Test Matrix

All three systems test across the same core categories. We implement the ones applicable to our architecture.

| # | Test | Source | CockroachDB | TiDB | Vitess | Ours |
|---|------|--------|-------------|------|--------|------|
| 1 | Cross-partition data verification | Vitess VDiff | - | - | VDiff row-by-row | test 1 |
| 2 | Bank invariant (single table) | Jepsen bank | bank workload | bank test | - | test 2 |
| 3 | Bank invariant (multi-table) | TiDB bank-multitable | - | bank-multitable | - | test 3 |
| 4 | Partition enforcement | Vitess Single mode | - | - | reject cross-shard | test 4 |
| 5 | Concurrent write scaling | CockroachDB KV | KV 95 benchmark | sysbench | - | test 5 |
| 6 | Monotonic reads | TiDB monotonic | sequential check | monotonic register | - | test 6 |
| 7 | Node restart recovery | CockroachDB nemesis | node crash test | kill -9 + verify | - | test 7 |
| 8 | Idempotent re-run | CockroachDB workload check | workload check | - | - | test 8 |

## Test Descriptions

### Test 1: Cross-Partition Data Verification (Vitess VDiff)

Vitess uses VDiff to row-by-row compare data between source and target shards after MoveTables or resharding. Our equivalent: write data to each partition, verify all data visible from every node.

**What it proves**: NATS delta replication correctly propagates user table data across partitions, including table creation (via `_tables`), index creation (via `_index`), and document data.

**How it works**:
1. Write messages and users to Node A (partition 0)
2. Write projects and tasks to Node B (partition 1)
3. Wait for NATS replication
4. Query dashboard from both nodes
5. Both must return identical counts

**Source**: [Vitess VDiff](https://vitess.io/blog/2022-11-22-vdiff-v2/)

### Test 2: Bank Invariant — Single Table (CockroachDB Jepsen)

CockroachDB's bank workload creates N accounts with known initial balances, then runs concurrent transfers between random accounts across nodes. The invariant: total balance is always preserved.

**What it proves**: Numeric invariants are preserved across replication. No data created or destroyed.

**How it works**:
1. Create projects with known budgets on Node B
2. Wait for replication
3. Sum budgets from Node A's view and Node B's view
4. Both must return the same total

**Source**: [Jepsen CockroachDB bank test](https://jepsen.io/analyses/cockroachdb-beta-20160829)

### Test 3: Bank Invariant — Multi-Table (TiDB bank-multitable)

TiDB extends the bank test across multiple tables. This catches bugs where single-table replication works but cross-table invariants break.

**What it proves**: Invariants hold even when the data spans tables owned by different partitions.

**How it works**:
1. Create users with salaries on Node A
2. Create projects with budgets on Node B
3. Compute combined total (all salaries + all budgets) from each node
4. Both must agree

**Source**: [TiDB Jepsen bank-multitable](https://github.com/jepsen-io/jepsen/tree/main/tidb)

### Test 4: Partition Enforcement (Vitess Single Mode)

Vitess's "Single mode" rejects transactions that touch multiple shards. Our partition ownership check does the same.

**What it proves**: The Committer correctly enforces table ownership. No phantom data from rejected writes.

**How it works**:
1. Attempt writes to wrong-partition tables from each node — all must fail
2. Verify error messages identify the correct partition owner
3. Verify no data was created by rejected writes

**Source**: [Vitess sharding](https://vitess.io/docs/22.0/reference/features/sharding/)

### Test 5: Concurrent Write Scaling (CockroachDB KV)

CockroachDB's KV 95 benchmark runs on increasing node counts and verifies linear throughput scaling.

**What it proves**: Two partitions writing independently achieve higher throughput with zero data loss.

**How it works**:
1. Write N records to each partition concurrently
2. Measure wall-clock time
3. Wait for replication
4. Verify all records visible from both nodes

**Source**: [CockroachDB scaling benchmark](https://www.cockroachlabs.com/blog/how-we-stress-test-and-benchmark-cockroachdb-for-global-scale/)

### Test 6: Monotonic Reads (TiDB monotonic)

TiDB's monotonic test verifies that successive reads by any single client observe monotonically increasing values. A counter that goes backward means the replication layer is returning stale data.

**What it proves**: A client reading from one node always sees values that advance forward, never backward. The TSO ensures global ordering.

**How it works**:
1. Write a sequence of incrementing values to Node B
2. After each write, immediately read from Node A
3. Each successive read from Node A must return a value >= the previous read
4. No value ever goes backward

**Source**: [TiDB Jepsen monotonic](https://github.com/pingcap/tidb/issues/26359)

### Test 7: Node Restart Recovery (TiDB kill -9 / CockroachDB nemesis)

TiDB uses kill -9 to force-kill nodes, then verifies recovery. CockroachDB's nemesis framework does the same with random node crashes during transactions.

**What it proves**: After a node crashes and restarts, it recovers its state from persistence and catches up on missed NATS deltas. No data loss.

**How it works**:
1. Write data to both nodes
2. Verify replication works
3. Kill Node B (docker restart)
4. Write more data to Node A while Node B is down
5. Wait for Node B to come back
6. Verify Node B sees all data (pre-crash + written during downtime)

**Source**: [TiDB chaos engineering](https://www.pingcap.com/blog/chaos-practice-in-tidb/), [CockroachDB DIY Jepsen](https://www.cockroachlabs.com/blog/diy-jepsen-testing-cockroachdb/)

### Test 8: Idempotent Re-Run (CockroachDB workload check)

CockroachDB's `workload check` runs consistency invariants after any duration of load. Running the same test suite twice back-to-back should produce no corruption.

**What it proves**: The system handles repeated operations gracefully. No duplicate tables, no duplicate data, no state corruption from re-running replication.

**How it works**:
1. Run the full test suite (tests 1-5)
2. Without restarting, run it again
3. All tests must still pass (baseline-relative counts handle accumulation)

**Source**: [CockroachDB workload check](https://www.cockroachlabs.com/docs/stable/cockroach-workload)

## Sources

### CockroachDB
- [cockroach workload](https://www.cockroachlabs.com/docs/stable/cockroach-workload)
- [Stress testing for global scale](https://www.cockroachlabs.com/blog/how-we-stress-test-and-benchmark-cockroachdb-for-global-scale/)
- [DIY Jepsen testing](https://www.cockroachlabs.com/blog/diy-jepsen-testing-cockroachdb/)
- [Jepsen analysis](https://jepsen.io/analyses/cockroachdb-beta-20160829)
- [TPC-C 140K warehouses](https://www.cockroachlabs.com/blog/cockroachdb-performance-20-2/)
- [Roachtest README](https://github.com/cockroachdb/cockroach/blob/master/pkg/cmd/roachtest/README.md)
- [Metamorphic testing](https://www.cockroachlabs.com/blog/metamorphic-testing-the-database/)

### TiDB
- [TiDB Jepsen tests](https://github.com/jepsen-io/jepsen/tree/main/tidb)
- [Jepsen TiDB 2.1.7 analysis](https://jepsen.io/analyses/tidb-2.1.7)
- [TiDB meets Jepsen](https://www.pingcap.com/blog/tidb-meets-jepsen/)
- [Chaos engineering at PingCAP](https://www.pingcap.com/blog/chaos-practice-in-tidb/)
- [TiPocket testing toolkit](https://github.com/pingcap/tipocket)
- [Safety pitfalls found by Jepsen](https://pingcap.com/blog/safety-first-common-safety-pitfalls-in-distributed-databases-found-by-jepsen-tests/)

### Vitess
- [VDiff v2](https://vitess.io/blog/2022-11-22-vdiff-v2/)
- [VDiff reference](https://vitess.io/docs/archive/17.0/reference/vreplication/vdiff/)
- [Sharding test package](https://pkg.go.dev/vitess.io/vitess/go/test/endtoend/sharding)
- [Atomic distributed transactions RFC](https://github.com/vitessio/vitess/issues/16245)
