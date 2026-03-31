# Testing Strategy for Primary-Replica Implementation

How to verify that the primary-replica architecture works correctly without breaking Convex's guarantees.

---

## Table of Contents

1. [What Convex Already Has](#1-what-convex-already-has)
2. [The Five Things We Need to Prove](#2-the-five-things-we-need-to-prove)
3. [Layer 1: Unit Tests](#3-layer-1-unit-tests)
4. [Layer 2: Integration Tests](#4-layer-2-integration-tests)
5. [Layer 3: Consistency Tests](#5-layer-3-consistency-tests)
6. [Layer 4: Failure and Chaos Tests](#6-layer-4-failure-and-chaos-tests)
7. [Layer 5: Load and Performance Tests](#7-layer-5-load-and-performance-tests)
8. [Layer 6: Regression — The Existing Suite](#8-layer-6-regression--the-existing-suite)
9. [Test Infrastructure We Need to Build](#9-test-infrastructure-we-need-to-build)
10. [Test Matrix](#10-test-matrix)

---

## 1. What Convex Already Has

The existing test infrastructure is solid. Before writing a single new test, we inherit:

| Infrastructure | What It Does | Where It Lives |
|---|---|---|
| **TestRuntime** | Deterministic async runtime with paused time, seeded RNG, manual clock advancement | `crates/common/src/runtime/testing/` |
| **TestPersistence** | In-memory mock of the Persistence trait. Full read/write support, can simulate failures. | `crates/common/src/testing/test_persistence.rs` |
| **PauseController** | Injects deterministic breakpoints into async code. Used to test race conditions by pausing execution at labeled points. | `crates/common/src/pause.rs` |
| **DbFixtures** | Spins up a fully configured `Database` with TestPersistence, mock searcher, and storage. The standard way to get a test database. | `crates/database/src/test_helpers/db_fixtures.rs` |
| **SimulationTest** | Full end-to-end harness that simulates JS clients talking to a real Convex backend. Tests sync, consistency, and client behavior. | `crates/simulation/src/` |
| **ApplicationFixtures** | Complete Application-level setup with InProcessFunctionRunner, mock auth, and configurable persistence. | `crates/application/src/test_helpers.rs` |
| **Persistence Test Suite** | Macro (`run_persistence_test_suite!`) that runs 30+ tests against any Persistence implementation. | `crates/common/src/testing/persistence_test_suite.rs` |
| **LoadGenerator** | Benchmarking tool that runs workloads against a live Convex instance with Datadog metrics. | `crates/load_generator/` |
| **Property-based tests** | Uses `proptest` for randomized testing of search, indexes, and data structures. | Various crates |
| **`#[convex_macro::test_runtime]`** | Test macro that injects TestRuntime and PauseController automatically. | Applied to test functions |

This is a strong foundation. The test patterns — deterministic concurrency, fixture-based setup, feature-gated test code, mock implementations behind traits — are exactly what we need to test a distributed system component by component.

---

## 2. The Five Things We Need to Prove

Every test we write should map to one of these guarantees:

| # | Guarantee | What It Means | How Failure Would Look |
|---|-----------|--------------|----------------------|
| 1 | **Serializability preserved** | Every transaction still behaves as if it executed alone, in some serial order | Two mutations read the same document, both commit, data is inconsistent |
| 2 | **Subscriptions see all writes** | A subscription on Replica A gets invalidated when the Primary commits a relevant write | Client shows stale data, never updates |
| 3 | **Read-after-write consistency** | After a client's mutation commits, that client's next query sees the result | User sends a message, their message list doesn't show it |
| 4 | **Replicas converge** | All Replicas eventually reach the same state as the Primary | Two users on different Replicas see different data permanently |
| 5 | **Failure recovery** | The system recovers from crashes, restarts, and log lag without data loss | Replica falls behind, never catches up; Primary crashes, writes lost |

---

## 3. Layer 1: Unit Tests

Test each new component in isolation using mocks and the existing TestRuntime.

### 3.1 DistributedLogWriter

```
Test: writes are published to the log after commit
- Create a DistributedLogWriter wrapping an InMemoryDistributedLog
- Publish a CommitDelta with known timestamp and document writes
- Read from the log, verify the delta matches exactly

Test: writes are published in order
- Publish 100 deltas with sequential timestamps
- Consume all 100, verify ordering is preserved

Test: publish failure doesn't corrupt local state
- Configure the log to fail on the 5th publish
- Verify the first 4 are in the log
- Verify the Committer can retry the 5th

Test: serialization roundtrip
- Create a CommitDelta with every field populated (documents, index updates, table updates)
- Serialize → deserialize → compare to original
- Test with edge cases: empty writes, max-size documents, unicode field names
```

### 3.2 DistributedLogConsumer

```
Test: consumer tails the log and emits deltas in order
- Pre-populate a log with 50 deltas
- Start a consumer at timestamp 0
- Verify it yields all 50 in order

Test: consumer blocks when caught up, resumes when new data arrives
- Start consumer on empty log
- Spawn a task that publishes a delta after 100ms
- Verify consumer receives it after the publish

Test: consumer resumes from last offset after restart
- Consume 10 deltas, record offset
- Create new consumer from saved offset
- Publish 5 more deltas
- Verify new consumer only sees the 5 new ones

Test: consumer handles gaps gracefully
- Consumer should detect if it missed a timestamp and raise an error rather than silently skip
```

### 3.3 RemoteCommitterClient

```
Test: mutation is forwarded to Primary and result is returned
- Start a mock gRPC CommitService that always returns timestamp 42
- Create RemoteCommitterClient pointing at it
- Call commit(), verify it returns timestamp 42

Test: network failure results in a retryable error, not a panic
- Start a mock gRPC service, then kill it
- Call commit(), verify it returns an error (not panic)
- Restart the mock service, verify next commit() succeeds

Test: timeout handling
- Start a mock gRPC service that sleeps for 10 seconds before responding
- Set RemoteCommitterClient timeout to 1 second
- Verify commit() returns a timeout error
```

### 3.4 ReplicaSnapshotUpdater

```
Test: applying a CommitDelta updates the snapshot correctly
- Create a SnapshotManager, insert a document at ts=100
- Apply a CommitDelta at ts=101 that modifies the document
- Read the snapshot at ts=101, verify it reflects the change

Test: snapshot versions advance monotonically
- Apply 10 deltas with increasing timestamps
- Verify SnapshotManager.versions has entries at each timestamp

Test: applying a delta with a gap in timestamps raises an error
- Create snapshot at ts=100, try to apply delta at ts=105 (skipping 101-104)
- Verify this is detected and handled (either error or wait)
```

### 3.5 CommitDelta Extraction

```
Test: CommitDelta from Committer matches what publish_commit() does
- Run a real commit through the Committer via DbFixtures
- Capture the CommitDelta that would be published
- Apply that delta to a fresh SnapshotManager
- Verify the two SnapshotManagers match

This is the most critical unit test. It proves that the delta we serialize
and send over the wire contains enough information to replicate the commit.
```

---

## 4. Layer 2: Integration Tests

Test the Primary and Replica working together using real (but in-process) components.

### 4.1 ClusterFixtures — The Test Harness

Build a new test fixture that spins up a Primary + N Replicas in one process:

```
ClusterFixtures:
  - Primary: full Database<TestRuntime> with Committer, SnapshotManager, WriteLog
  - DistributedLog: InMemoryDistributedLog (no Kafka needed for tests)
  - Replica(s): ReplicaDatabase<TestRuntime> with LogConsumer, RemoteCommitterClient
  - Methods:
    - commit_on_primary(transaction) → timestamp
    - query_on_replica(replica_id, query) → result
    - wait_for_replica_catchup(replica_id, timestamp)
    - subscribe_on_replica(replica_id, token) → subscription
```

All communication goes through the in-memory distributed log and direct function calls (no actual gRPC in unit/integration tests). This makes tests fast and deterministic.

### 4.2 Integration Test Cases

```
Test: write on Primary, read on Replica
- Insert a document on Primary at ts=100
- Wait for Replica to consume ts=100
- Query the document on Replica
- Verify it matches

Test: write on Primary, subscription on Replica fires
- Subscribe to a query on Replica
- Insert a document on Primary that matches the query
- Wait for the subscription to invalidate
- Verify the subscription received the correct invalidation timestamp

Test: multiple Replicas all converge
- Start 3 Replicas
- Insert 100 documents on Primary
- Wait for all 3 Replicas to consume all 100
- Query each Replica, verify all 3 return identical results

Test: mutation forwarded from Replica to Primary
- Call commit() on a Replica (via RemoteCommitterClient)
- Verify the commit happened on the Primary
- Wait for the committing Replica to see its own write via the log
- Query the Replica, verify the document exists

Test: OCC conflict detection still works
- Begin TX-A on Primary, read document X
- Begin TX-B on Primary, read document X
- Commit TX-A (succeeds, modifies X)
- Commit TX-B (should fail with OCC conflict, because TX-A modified X after TX-B's read)
- Verify TX-B gets an OCC error

Test: read-after-write consistency via timestamp tracking
- Commit a mutation on Primary, get back ts=100
- Immediately query Replica with min_timestamp=100
- Verify Replica either waits for catchup or returns correct data
- Verify Replica does NOT return stale pre-ts=100 data

Test: high-throughput pipeline
- Commit 10,000 mutations on Primary in rapid succession
- Verify Replica eventually has all 10,000
- Verify no writes were lost or duplicated
- Verify final snapshot on Replica matches Primary
```

---

## 5. Layer 3: Consistency Tests

These prove the hard guarantees. Use the existing simulation framework where possible.

### 5.1 Linearizability Check with Elle

Convex already has Elle consistency tests in `crates/simulation/src/tests/elle/`. Elle is a tool (from Jepsen) that generates transaction histories and checks them against consistency models.

We extend these tests to run against a Primary + Replica cluster:

```
Test: Elle linearizability check on cluster
- Run Elle-generated workloads against ClusterFixtures
- Some transactions commit via Primary, some via Replica forwarding
- Some reads go to Primary, some to Replicas
- Elle verifies the history is serializable

This is the gold standard. If Elle says the history is serializable,
the implementation is correct.
```

### 5.2 Subscription Completeness

```
Test: no missed invalidations
- Subscribe to a query on Replica that reads from table T
- Commit 1000 mutations to table T on Primary, each modifying a different document
- For each mutation, verify the subscription was invalidated
- Verify no invalidation was missed (count invalidations == count of relevant mutations)

Test: no spurious invalidations
- Subscribe to a query on Replica that reads from table T where channel = "general"
- Commit mutations to table T where channel = "random"
- Verify the subscription was NOT invalidated (the writes don't affect its read set)
```

### 5.3 Snapshot Equivalence

```
Test: Replica snapshot matches Primary snapshot at every timestamp
- Commit 500 mutations on Primary
- After each commit, compare Primary's snapshot at ts=N with Replica's snapshot at ts=N
- Compare: table registry, index registry, document counts, table summaries
- Any divergence is a bug

This is an invariant check, not a behavioral test. Run it in CI on every commit.
```

---

## 6. Layer 4: Failure and Chaos Tests

Test what happens when things break. Use PauseController and TestRuntime for deterministic failure injection.

### 6.1 Replica Falls Behind

```
Test: Replica recovers after log consumer stalls
- Pause the Replica's log consumer (using PauseController)
- Commit 100 mutations on Primary
- Resume the consumer
- Verify Replica catches up and final state matches Primary

Test: Replica rejects queries at timestamps it hasn't consumed
- Pause Replica's log consumer
- Commit on Primary at ts=100
- Try to query Replica with min_timestamp=100
- Verify Replica either waits or returns an appropriate error
- It must NOT serve stale data
```

### 6.2 Primary Crashes and Restarts

```
Test: Primary restarts, Replicas reconnect
- Commit 50 mutations on Primary
- Shut down Primary
- Start a new Primary that loads from the same persistence
- Verify Replicas can forward mutations to the new Primary
- Verify no data was lost

Test: in-flight mutation during Primary crash
- Begin a mutation on Replica that gets forwarded to Primary
- Kill Primary before it responds
- Verify Replica gets a retryable error (not a success)
- Restart Primary, retry the mutation, verify it succeeds
```

### 6.3 Distributed Log Failures

```
Test: log publish failure — Primary retries
- Configure InMemoryDistributedLog to fail on the next publish
- Commit a mutation on Primary
- Verify the commit succeeds in persistence (durability)
- Verify the Primary retries publishing to the log
- Verify Replica eventually receives the write

Test: log becomes unavailable — Replicas serve stale reads with warning
- Pause the distributed log
- Commit on Primary (succeeds in persistence, but can't publish to log)
- Replicas can still serve reads at their last consumed timestamp
- Resume the log, verify Replicas catch up

Test: log data loss — Replica detects gap and re-bootstraps
- Consume up to ts=50 on Replica
- Delete entries ts=51-60 from the log (simulating log data loss)
- Publish ts=61 to the log
- Replica should detect the gap (expected ts=51, got ts=61)
- Replica should trigger a re-bootstrap from persistence rather than serving wrong data
```

### 6.4 Network Partition Between Primary and Replica

```
Test: Replica can't reach Primary — mutations fail, reads still work
- Sever the RemoteCommitterClient's connection
- Verify mutations through Replica return errors
- Verify reads on Replica still work (from local snapshot)
- Restore connection, verify mutations work again

Test: Replica can't reach log — reads become increasingly stale
- Sever the log consumer's connection
- Commit on Primary
- Replica continues serving reads but at an old timestamp
- After a staleness threshold, Replica should stop serving reads (or mark them as stale)
- Restore connection, verify Replica catches up
```

---

## 7. Layer 5: Load and Performance Tests

Use the existing LoadGenerator infrastructure, extended for cluster mode.

### 7.1 Throughput Scaling

```
Test: read throughput scales linearly with Replica count
- Baseline: measure QPS with 1 Primary (no Replicas, Primary serves reads)
- Add 1 Replica: measure QPS (reads go to Replica, Primary only writes)
- Add 2 Replicas: measure QPS
- Add 4 Replicas: measure QPS
- Verify roughly linear scaling (2 Replicas ≈ 2x, 4 Replicas ≈ 4x)
- Acceptable deviation: ±20% from linear

Test: subscription capacity scales linearly
- Baseline: max WebSocket connections on single node before degradation
- Add Replicas, distribute connections
- Verify capacity scales with Replica count
```

### 7.2 Latency

```
Test: query latency on Replica is comparable to single-node
- Measure p50, p95, p99 query latency on single node
- Measure same on Replica
- Verify Replica latency is within 10% of single-node (no overhead from replication)

Test: replication lag stays bounded
- Run sustained write load (500 mutations/sec) on Primary
- Continuously measure lag between Primary's latest timestamp and Replica's consumed timestamp
- Verify p99 lag < 10ms within same region
- Verify p99 lag < 50ms cross-region
```

### 7.3 Write Throughput Unchanged

```
Test: Primary write throughput is not degraded by Replicas
- Measure max mutation throughput on single node
- Add 4 Replicas
- Measure max mutation throughput on Primary
- Verify throughput is equal or better (Primary is now unburdened from reads)
```

### 7.4 Endurance

```
Test: 24-hour soak test
- Run mixed workload (80% reads, 15% subscriptions, 5% writes)
- 1 Primary + 3 Replicas
- Monitor: memory usage, replication lag, error rates, query latency
- Verify no memory leaks, no growing lag, no error rate increase
- Verify snapshot versions don't accumulate unboundedly on Replicas
```

---

## 8. Layer 6: Regression — The Existing Suite

**Every existing test must still pass.** This is non-negotiable. The primary-replica architecture must be a superset of the current single-node behavior.

### How We Ensure This

1. **Single-node mode is the default.** When no distributed log is configured, the system behaves identically to today. All existing tests run in this mode without modification.

2. **Run the full suite in cluster mode.** Create a CI job that runs the existing database tests, application tests, and simulation tests against ClusterFixtures instead of DbFixtures. Any failure is a regression.

3. **Feature-gate new code.** All new components are behind `#[cfg(feature = "distributed")]` or similar. The `testing` feature includes them. Production builds can opt in.

```
CI Pipeline:
├── Job 1: cargo test -p database --features testing           (single-node, existing tests)
├── Job 2: cargo test -p database --features testing,distributed  (cluster mode, existing tests against ClusterFixtures)  
├── Job 3: cargo test -p application --features testing        (single-node, existing tests)
├── Job 4: cargo test -p application --features testing,distributed
├── Job 5: cargo test -p simulation --features testing         (single-node, Elle + sync tests)
├── Job 6: cargo test -p simulation --features testing,distributed  (cluster mode, Elle + sync tests)
└── Job 7: New cluster-specific tests (all tests from Layers 1-4 above)
```

---

## 9. Test Infrastructure We Need to Build

| Component | Purpose | Effort |
|---|---|---|
| `InMemoryDistributedLog` | In-process mock of the distributed log for tests. Implements `DistributedLog` trait using channels. | 1-2 days |
| `ClusterFixtures` | Test harness that spins up Primary + N Replicas with in-memory log. | 3-5 days |
| `SnapshotComparator` | Utility that compares two SnapshotManagers field-by-field and reports differences. Used in convergence tests. | 1-2 days |
| `ClusterSimulationTest` | Extension of existing SimulationTest that routes operations across Primary/Replicas. | 3-5 days |
| `ReplicationLagMonitor` | Test utility that tracks and asserts on replication lag between Primary and Replicas. | 1 day |
| `FaultInjector` | Wraps `DistributedLog` and `RemoteCommitterClient` with configurable failure injection (drop messages, add delay, partition). Built on PauseController. | 2-3 days |

**Total test infrastructure effort: ~2-3 weeks**

---

## 10. Test Matrix

Summary of all tests and what guarantee they prove:

| Test | Layer | Guarantee | Priority |
|---|---|---|---|
| CommitDelta serialization roundtrip | Unit | Correctness | P0 |
| CommitDelta from Committer matches publish_commit() | Unit | Correctness | P0 |
| DistributedLogWriter publishes in order | Unit | Ordering | P0 |
| DistributedLogConsumer tails and emits in order | Unit | Ordering | P0 |
| RemoteCommitterClient forwards and returns | Unit | Correctness | P0 |
| ReplicaSnapshotUpdater applies deltas correctly | Unit | Convergence | P0 |
| Write on Primary, read on Replica | Integration | Replication | P0 |
| Write on Primary, subscription on Replica fires | Integration | Subscriptions see all writes | P0 |
| Multiple Replicas converge | Integration | Convergence | P0 |
| OCC conflict detection still works | Integration | Serializability | P0 |
| Read-after-write via timestamp tracking | Integration | Read-after-write | P0 |
| High-throughput pipeline (10K mutations) | Integration | No lost writes | P0 |
| Elle linearizability check on cluster | Consistency | Serializability | P0 |
| No missed subscription invalidations | Consistency | Subscriptions see all writes | P0 |
| Snapshot equivalence at every timestamp | Consistency | Convergence | P0 |
| Replica recovers after stall | Failure | Recovery | P1 |
| Primary crash and restart | Failure | Durability | P1 |
| Log publish failure and retry | Failure | Durability | P1 |
| Log data loss detection | Failure | Correctness | P1 |
| Network partition — reads still work | Failure | Availability | P1 |
| Staleness threshold enforcement | Failure | Consistency | P1 |
| Read throughput scales linearly | Performance | Scaling | P1 |
| Replication lag stays bounded | Performance | Latency | P1 |
| Primary throughput not degraded | Performance | No regression | P1 |
| 24-hour soak test | Performance | Stability | P2 |
| All existing tests pass in single-node mode | Regression | No regression | P0 |
| All existing tests pass in cluster mode | Regression | No regression | P0 |
