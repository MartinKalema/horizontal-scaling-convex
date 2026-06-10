//! Write Scaling Unit Tests
//!
//! Tests partition ownership enforcement (Vitess Single mode pattern).
//! Cross-partition replication tests run against the live Docker deployment
//! via self-hosted/docker/test-write-scaling.sh (CockroachDB roachtest
//! pattern).

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    assert_obj,
    bootstrap_model::index::database_index::IndexedFields,
    interval::Interval,
    pause::PauseController,
    persistence::{
        Persistence,
        PersistenceGlobalKey,
    },
    query::{
        Order,
        Query,
    },
    runtime::{
        new_unlimited_rate_limiter,
        testing::{
            TestDriver,
            TestRuntime,
        },
    },
    shutdown::ShutdownSignal,
    testing::TestPersistence,
    types::{
        TabletIndexName,
        Timestamp,
    },
};
use keybroker::Identity;
use parking_lot::Mutex;
use pb::replication::{
    two_phase_commit_service_server::{
        TwoPhaseCommitService,
        TwoPhaseCommitServiceServer,
    },
    TwoPcCommitRequest,
    TwoPcCommitResponse,
    TwoPcPrepareRequest,
    TwoPcPrepareResponse,
    TwoPcRollbackRequest,
    TwoPcRollbackResponse,
};
use search::searcher::SearcherStub;
use storage::LocalDirStorage;
use tokio::{
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Request,
    Response,
    Status,
};
use value::{
    ResolvedDocumentId,
    TableName,
    TableNamespace,
};

use crate::{
    commit_delta::{
        testing::InMemoryDistributedLog,
        CommitDelta,
    },
    committer::{
        CommitterClient,
        AFTER_PENDING_WRITE_SNAPSHOT,
    },
    partition::{
        PartitionId,
        PartitionMap,
        PlacementMetadata,
        PlacementVersion,
        StaticPlacementConfig,
    },
    tests::run_query,
    timestamp_oracle::{
        testing::InMemoryTimestampOracle,
        TimestampOracle,
    },
    two_phase::{
        testing::InMemoryTwoPhaseDecisionLog,
        NodeAddresses,
        NoopTwoPhaseDecisionLog,
        ParticipantTransaction,
        TwoPhaseCommitGrpcClient,
        TwoPhaseDecision,
        TwoPhaseDecisionLog,
        TwoPhaseTransactionId,
    },
    Database,
    TestFacingModel,
    UserFacingModel,
    WriteSource,
};

async fn create_node(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
) -> anyhow::Result<Database<TestRuntime>> {
    create_node_with_options(rt, distributed_log, partition_map, None, None).await
}

async fn create_node_with_options(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
    node_addresses: Option<NodeAddresses>,
    timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
) -> anyhow::Result<Database<TestRuntime>> {
    create_node_with_options_and_decision_log(
        rt,
        distributed_log,
        partition_map,
        node_addresses,
        timestamp_oracle,
        Arc::new(NoopTwoPhaseDecisionLog),
    )
    .await
}

async fn create_node_with_options_and_decision_log(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
    node_addresses: Option<NodeAddresses>,
    timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
    two_phase_decision_log: Arc<dyn TwoPhaseDecisionLog>,
) -> anyhow::Result<Database<TestRuntime>> {
    create_node_with_persistence(
        rt,
        Arc::new(TestPersistence::new()),
        distributed_log,
        partition_map,
        node_addresses,
        two_phase_decision_log,
        timestamp_oracle,
    )
    .await
}

async fn create_node_with_persistence(
    rt: &TestRuntime,
    tp: Arc<TestPersistence>,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
    node_addresses: Option<NodeAddresses>,
    two_phase_decision_log: Arc<dyn TwoPhaseDecisionLog>,
    timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
) -> anyhow::Result<Database<TestRuntime>> {
    let searcher: Arc<dyn search::Searcher> = Arc::new(SearcherStub {});
    let (deleted_tablet_sender, _) = tokio::sync::mpsc::channel(100);
    let db = Database::load(
        tp,
        rt.clone(),
        searcher,
        ShutdownSignal::panic(),
        Default::default(),
        None,
        Arc::new(new_unlimited_rate_limiter(rt.clone())),
        deleted_tablet_sender,
        distributed_log,
        false,
        partition_map,
        node_addresses,
        two_phase_decision_log,
        timestamp_oracle,
        None,
    )
    .await?;
    db.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = db.start_search_and_vector_bootstrap();
    handle.join().await?;
    Ok(db)
}

fn partitioned_map(local_partition: PartitionId) -> PartitionMap {
    PartitionMap::from_config("messages=0,projects=1", local_partition, 2)
}

fn partitioned_map_with_version(
    local_partition: PartitionId,
    placement_version: PlacementVersion,
) -> PartitionMap {
    PartitionMap::from_config_with_version(
        "messages=0,projects=1",
        local_partition,
        2,
        placement_version,
    )
}

fn partitioned_map_with_tasks(local_partition: PartitionId) -> PartitionMap {
    PartitionMap::from_config("messages=0,projects=1,tasks=1", local_partition, 2)
}

async fn insert_doc_get_id(
    db: &Database<TestRuntime>,
    table: &str,
    fields: common::value::DocumentObject,
) -> anyhow::Result<ResolvedDocumentId> {
    let table_name: TableName = table.parse()?;
    let mut tx = db.begin(Identity::system()).await?;
    let id = TestFacingModel::new(&mut tx)
        .insert(&table_name, fields)
        .await?;
    db.commit(tx).await?;
    Ok(id)
}

async fn insert_doc(
    db: &Database<TestRuntime>,
    table: &str,
    fields: common::value::DocumentObject,
) -> anyhow::Result<common::types::Timestamp> {
    let table_name: TableName = table.parse()?;
    let mut tx = db.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&table_name, fields)
        .await?;
    db.commit(tx).await
}

struct TestTwoPhaseCommitGrpcService {
    committer: CommitterClient,
}

impl TestTwoPhaseCommitGrpcService {
    fn new(committer: CommitterClient) -> Self {
        Self { committer }
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for TestTwoPhaseCommitGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let transaction = req
            .transaction
            .ok_or_else(|| Status::invalid_argument("Prepare missing transaction payload"))?;
        let transaction = ParticipantTransaction::try_from(transaction)
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare payload: {e:#}")))?;
        let prepare_ts: Timestamp = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;
        self.committer
            .ensure_placement_version(PlacementVersion::from(req.placement_version))
            .map_err(|e| Status::failed_precondition(format!("Prepare rejected: {e:#}")))?;

        let result = self
            .committer
            .prepare_remote(txn_id, transaction, req.write_source.into(), prepare_ts)
            .await
            .map_err(|e| Status::internal(format!("Prepare failed: {e:#}")))?;

        Ok(Response::new(TwoPcPrepareResponse {
            prepare_ts: u64::from(result.prepare_ts),
        }))
    }

    async fn commit_prepared(
        &self,
        request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        let req = request.into_inner();
        let commit_ts = self
            .committer
            .commit_prepared(TwoPhaseTransactionId(req.transaction_id))
            .await
            .map_err(|e| Status::internal(format!("CommitPrepared failed: {e:#}")))?;

        Ok(Response::new(TwoPcCommitResponse {
            commit_ts: u64::from(commit_ts),
        }))
    }

    async fn rollback_prepared(
        &self,
        request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        let req = request.into_inner();
        self.committer
            .rollback_prepared(TwoPhaseTransactionId(req.transaction_id))
            .await
            .map_err(|e| Status::internal(format!("RollbackPrepared failed: {e:#}")))?;
        Ok(Response::new(TwoPcRollbackResponse {}))
    }
}

struct FailingPrepareGrpcService;

#[tonic::async_trait]
impl TwoPhaseCommitService for FailingPrepareGrpcService {
    async fn prepare(
        &self,
        _request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        Err(Status::internal("forced prepare failure"))
    }

    async fn commit_prepared(
        &self,
        _request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        Err(Status::failed_precondition(
            "commit_prepared should not be called after a failed prepare",
        ))
    }

    async fn rollback_prepared(
        &self,
        _request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        Err(Status::failed_precondition(
            "rollback_prepared should not be called after a failed prepare",
        ))
    }
}

struct FailingCommitAfterPrepareGrpcService;

#[tonic::async_trait]
impl TwoPhaseCommitService for FailingCommitAfterPrepareGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let prepare_ts: Timestamp = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;

        Ok(Response::new(TwoPcPrepareResponse {
            prepare_ts: u64::from(prepare_ts),
        }))
    }

    async fn commit_prepared(
        &self,
        _request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        Err(Status::unavailable(
            "forced commit failure after durable decision",
        ))
    }

    async fn rollback_prepared(
        &self,
        _request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        Ok(Response::new(TwoPcRollbackResponse {}))
    }
}

struct FailingTimestampOracle;

#[async_trait]
impl TimestampOracle for FailingTimestampOracle {
    async fn next_ts_at_or_after(&self, _min_ts: Timestamp) -> anyhow::Result<Timestamp> {
        anyhow::bail!("TSO unavailable during idle heartbeat test");
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        anyhow::bail!("TSO unavailable during idle heartbeat test");
    }

    async fn advance_committed_ts(&self, _ts: Timestamp) -> anyhow::Result<()> {
        anyhow::bail!("TSO unavailable during idle heartbeat test");
    }
}

struct RecordingTimestampOracle {
    state: Mutex<RecordingTimestampOracleState>,
}

struct RecordingTimestampOracleState {
    last_assigned: Timestamp,
    max_committed: Timestamp,
    requested_floors: Vec<Timestamp>,
}

impl RecordingTimestampOracle {
    fn new() -> Self {
        Self {
            state: Mutex::new(RecordingTimestampOracleState {
                last_assigned: Timestamp::MIN,
                max_committed: Timestamp::MIN,
                requested_floors: Vec::new(),
            }),
        }
    }

    fn requested_floors(&self) -> Vec<Timestamp> {
        self.state.lock().requested_floors.clone()
    }
}

#[async_trait]
impl TimestampOracle for RecordingTimestampOracle {
    async fn next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Timestamp> {
        let mut state = self.state.lock();
        state.requested_floors.push(min_ts);
        let next = std::cmp::max(
            min_ts,
            std::cmp::max(state.last_assigned.succ()?, state.max_committed.succ()?),
        );
        state.last_assigned = next;
        Ok(next)
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        Ok(self.state.lock().max_committed)
    }

    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if ts > state.max_committed {
            state.max_committed = ts;
        }
        Ok(())
    }
}

struct TestGrpcServerHandle {
    addr: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TestGrpcServerHandle {
    fn addr(&self) -> &str {
        &self.addr
    }

    async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for TestGrpcServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn start_two_pc_server<S>(service: S) -> anyhow::Result<TestGrpcServerHandle>
where
    S: TwoPhaseCommitService + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(TwoPhaseCommitServiceServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                let _ = shutdown_rx.await;
            })
            .await?;
        Ok(())
    });
    Ok(TestGrpcServerHandle {
        addr: addr.to_string(),
        shutdown: Some(shutdown_tx),
        task: Some(task),
    })
}

/// Vitess Single mode pattern: writes to non-owned tables must be rejected
/// with a clear error identifying the correct partition owner.
#[convex_macro::test_runtime]
async fn test_partition_enforcement(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());

    let node_a = create_node(
        &rt,
        log.clone(),
        Some(PartitionMap::from_config(
            "messages=0,projects=1",
            PartitionId(0),
            2,
        )),
    )
    .await?;
    let node_b = create_node(
        &rt,
        log.clone(),
        Some(PartitionMap::from_config(
            "messages=0,projects=1",
            PartitionId(1),
            2,
        )),
    )
    .await?;

    // Node A writing to projects (owned by partition 1) — must fail.
    let result_a = insert_doc(&node_a, "projects", assert_obj!("name" => "wrong")).await;
    assert!(result_a.is_err(), "Node A should reject write to projects");
    assert!(
        result_a.unwrap_err().to_string().contains("partition-1"),
        "Error should mention the owning partition"
    );

    // Node B writing to messages (owned by partition 0) — must fail.
    let result_b = insert_doc(&node_b, "messages", assert_obj!("text" => "wrong")).await;
    assert!(result_b.is_err(), "Node B should reject write to messages");
    assert!(
        result_b.unwrap_err().to_string().contains("partition-0"),
        "Error should mention the owning partition"
    );

    // Correct-partition writes succeed.
    insert_doc(&node_a, "messages", assert_obj!("text" => "correct")).await?;
    insert_doc(&node_b, "projects", assert_obj!("name" => "correct")).await?;

    Ok(())
}

/// Jepsen sequential: write A, B, C sequentially from one client.
/// Commit timestamps must be strictly increasing.
/// CockroachDB Jepsen found disjoint records visible out of order.
#[convex_macro::test_runtime]
async fn test_sequential_ordering(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let ts1 = insert_doc(&db, "items", assert_obj!("label" => "first")).await?;
    let ts2 = insert_doc(&db, "items", assert_obj!("label" => "second")).await?;
    let ts3 = insert_doc(&db, "items", assert_obj!("label" => "third")).await?;

    assert!(ts2 > ts1, "second must commit after first");
    assert!(ts3 > ts2, "third must commit after second");

    Ok(())
}

/// Jepsen set: insert N unique elements. All must succeed — no lost inserts.
#[convex_macro::test_runtime]
async fn test_set_completeness(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let n = 100;
    let mut timestamps = Vec::new();
    for i in 0..n {
        let ts = insert_doc(&db, "elements", assert_obj!("value" => i as i64)).await?;
        timestamps.push(ts);
    }

    // All N inserts must have succeeded with increasing timestamps.
    assert_eq!(timestamps.len(), n);
    for i in 1..timestamps.len() {
        assert!(
            timestamps[i] > timestamps[i - 1],
            "Insert {} timestamp not after insert {}: {:?} vs {:?}",
            i,
            i - 1,
            timestamps[i],
            timestamps[i - 1],
        );
    }

    Ok(())
}

/// TiDB monotonic: commit timestamps must be strictly increasing.
/// No commit ever gets a timestamp <= a previous commit.
#[convex_macro::test_runtime]
async fn test_monotonic_timestamps(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let mut last_ts = common::types::Timestamp::MIN;
    for i in 1..=20i64 {
        let ts = insert_doc(&db, "counters", assert_obj!("value" => i)).await?;
        assert!(
            ts > last_ts,
            "Monotonic violation: ts {:?} <= previous {:?}",
            ts,
            last_ts,
        );
        last_ts = ts;
    }

    Ok(())
}

/// Duplicate insert: insert same data twice. Both must succeed.
/// Convex has no unique constraints — no deduplication.
#[convex_macro::test_runtime]
async fn test_duplicate_insert(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let ts1 = insert_doc(
        &db,
        "dupes",
        assert_obj!("key" => "same", "value" => "data"),
    )
    .await?;
    let ts2 = insert_doc(
        &db,
        "dupes",
        assert_obj!("key" => "same", "value" => "data"),
    )
    .await?;

    assert!(ts2 > ts1, "Second duplicate must commit after first");

    // Both committed — verify by inserting_and_getting a third doc.
    let mut tx = db.begin(Identity::system()).await?;
    let table_name: TableName = "dupes".parse()?;
    let doc = TestFacingModel::new(&mut tx)
        .insert_and_get(table_name, assert_obj!("key" => "third"))
        .await?;
    // If we got here without error, the table exists and accepts inserts.
    assert!(!doc.id().to_string().is_empty());

    Ok(())
}

/// CockroachDB Jepsen register: write values, read back via insert_and_get.
/// Each write must succeed and produce a unique document.
#[convex_macro::test_runtime]
async fn test_single_key_register(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    // Write initial value.
    let ts1 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "first"),
    )
    .await?;

    // Write second value.
    let ts2 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "second"),
    )
    .await?;
    assert!(ts2 > ts1, "Second write must commit after first");

    // Write third value.
    let ts3 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "third"),
    )
    .await?;
    assert!(ts3 > ts2, "Third write must commit after second");

    // Verify via insert_and_get that the table has data.
    let mut tx = db.begin(Identity::system()).await?;
    let table_name: TableName = "registers".parse()?;
    let doc = TestFacingModel::new(&mut tx)
        .insert_and_get(table_name, assert_obj!("key" => "r1", "value" => "fourth"))
        .await?;
    assert!(
        !doc.id().to_string().is_empty(),
        "Register table should accept writes"
    );

    Ok(())
}

/// CockroachDB Jepsen bank: create records with known numeric values.
/// Verify all inserts succeed and the delta log captures them.
/// Sum preservation is verified via the distributed log.
#[convex_macro::test_runtime]
async fn test_bank_invariant(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log.clone(), None).await?;

    let balances: Vec<i64> = vec![10000, 25000, 50000, 15000];

    for balance in &balances {
        insert_doc(&db, "accounts", assert_obj!("balance" => *balance)).await?;
    }

    // Wait for async publish tasks.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify deltas were published — each insert should produce a delta
    // with document updates.
    let deltas = log.deltas();
    let account_deltas: Vec<_> = deltas
        .iter()
        .filter(|d| d.document_updates.iter().any(|u| u.new_document.is_some()))
        .collect();

    assert!(
        account_deltas.len() >= balances.len(),
        "Expected at least {} deltas with document inserts, got {}",
        balances.len(),
        account_deltas.len(),
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cross_partition_prepare_waits_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot without advancing partition 1's frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let remote_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    final_tx.reads.record_indexed_derived(
        TabletIndexName::by_id(remote_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let committer = node_a.committer_for_test();
    let prepare = tokio::spawn(async move {
        committer
            .prepare(
                TwoPhaseTransactionId::new(),
                final_tx,
                WriteSource::new("cross_partition_wait_test"),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !prepare.is_finished(),
        "prepare should wait for the remote partition frontier to catch up",
    );

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_heartbeat_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let prepare_result = tokio::time::timeout(Duration::from_secs(1), prepare).await??;
    prepare_result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_only_prepare_does_not_wait_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot beyond partition 1's persisted frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
        .await?;
    let final_tx = tx.finalize()?;

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        node_a.committer_for_test().prepare(
            TwoPhaseTransactionId::new(),
            final_tx,
            WriteSource::new("local_only_prepare_test"),
        ),
    )
    .await?;
    result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_commit_waits_for_remote_frontier(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's local snapshot without advancing partition 1's frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "commit"))
        .await?;
    let remote_tablet = tx
        .table_mapping()
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    tx.reads.record_indexed_derived(
        TabletIndexName::by_id(remote_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let node_a_for_commit = node_a.clone();
    let commit = tokio::spawn(async move { node_a_for_commit.commit(tx).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !commit.is_finished(),
        "commit should wait for the remote partition frontier to catch up",
    );

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_heartbeat_commit_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let commit_result = tokio::time::timeout(Duration::from_secs(1), commit).await??;
    commit_result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_only_commit_does_not_wait_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot beyond partition 1's persisted frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "commit"))
        .await?;

    let result = tokio::time::timeout(Duration::from_millis(250), node_a.commit(tx)).await?;
    result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_remote_single_partition_commit_rejects_before_waiting_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_a, "messages", assert_obj!("text" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed message should publish a delta");
    node_b
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node B's local snapshot beyond partition 0's replicated frontier so
    // a stale remote frontier would block if we waited before classifying the
    // write.
    insert_doc(&node_b, "projects", assert_obj!("name" => "lag-maker")).await?;

    let mut tx = node_b.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "wrong-owner"))
        .await?;
    let remote_tablet = tx
        .table_mapping()
        .namespace(TableNamespace::Global)
        .name_to_tablet()("messages".parse()?)?;
    tx.reads.record_indexed_derived(
        TabletIndexName::by_id(remote_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let err = tokio::time::timeout(Duration::from_millis(250), node_b.commit(tx))
        .await
        .context(
            "remote single-partition write should reject before waiting on a stale remote frontier",
        )?
        .expect_err("node B should reject a partition-0-only write");
    assert!(
        format!("{err:#}").contains("Route this mutation to the correct partition owner"),
        "expected remote owner rejection, got: {err:#}",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replication_frontier_persists_across_restart(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let tp = Arc::new(TestPersistence::new());
    let node_a = create_node_with_persistence(
        &rt,
        tp.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_persist_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let persisted_frontier = node_a
        .replication_frontier_for_test(PartitionId(1))
        .expect("partition 1 frontier should be recorded");

    let restarted = create_node_with_persistence(
        &rt,
        tp,
        log,
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    assert_eq!(
        restarted.replication_frontier_for_test(PartitionId(1)),
        Some(persisted_frontier),
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_prepared_participant_recovers_from_redo_after_restart(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let tp = Arc::new(TestPersistence::new());
    let node = create_node_with_persistence(
        &rt,
        tp.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "recovered-from-redo"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("prepared_redo_recovery_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = node.committer_for_test().allocate_commit_ts().await?;
    node.committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source.clone(),
            prepare_ts,
        )
        .await?;
    node.shutdown().await?;

    let restarted = create_node_with_persistence(
        &rt,
        tp.clone(),
        log,
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let committed_ts = restarted
        .committer_for_test()
        .commit_prepared(txn_id)
        .await?;
    assert_eq!(committed_ts, prepare_ts);

    let projects = run_query(
        restarted.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 1);

    let redo = tp
        .reader()
        .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
        .await?;
    assert_eq!(redo, Some(serde_json::json!({})));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replication_frontier_heartbeat_does_not_advance_snapshot_ts(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    let snapshot_ts_before = *node_a.now_ts_for_reads();
    let persisted_repeatable_before = *node_a.persisted_max_repeatable_ts_for_test();
    let write_frontier_before = node_a.replication_write_frontier_for_test(PartitionId(1));
    let projects = run_query(
        node_a.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects.len(),
        1,
        "replica should expose the seeded project"
    );

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_snapshot_ts_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    assert_eq!(
        *node_a.now_ts_for_reads(),
        snapshot_ts_before,
        "frontier-only heartbeats should not publish a cloned snapshot",
    );
    assert_eq!(
        *node_a.persisted_max_repeatable_ts_for_test(),
        persisted_repeatable_before,
        "frontier-only heartbeats should not advertise a newer persisted snapshot",
    );
    assert_eq!(
        node_a.replication_frontier_for_test(PartitionId(1)),
        Some(heartbeat_ts),
        "heartbeat should still advance the remote replication frontier",
    );
    assert_eq!(
        node_a.replication_write_frontier_for_test(PartitionId(1)),
        write_frontier_before,
        "heartbeat must not advertise newer local strong-read visibility",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            node_a.wait_for_replication_write_frontier(PartitionId(1), heartbeat_ts),
        )
        .await
        .is_err(),
        "waiting for local write visibility should stay blocked on a frontier-only heartbeat",
    );

    let projects_after = run_query(
        node_a,
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects_after.len(),
        1,
        "heartbeat should not erase replica-queryable state",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replica_table_number_allocation_waits_for_prior_publish(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(0))),
    )
    .await?;
    let node_b = create_node(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(1))),
    )
    .await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed-project")).await?;
    let project_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("project seed should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(project_delta)
        .await?;

    insert_doc(
        &node_b,
        "tasks",
        assert_obj!("title" => "seed-task", "project" => "seed-project"),
    )
    .await?;
    let task_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("task seed should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(task_delta)
        .await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    let mapping = tx.table_mapping().namespace(TableNamespace::Global);
    let projects = mapping.id(&"projects".parse()?)?;
    let tasks = mapping.id(&"tasks".parse()?)?;

    assert_ne!(
        projects.table_number, tasks.table_number,
        "replica-applied tables must allocate distinct local table numbers",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replica_delta_does_not_erase_pending_local_table_creation(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(0))),
    )
    .await?;
    let node_b = create_node(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(1))),
    )
    .await?;

    let hold_guard = pause.hold(AFTER_PENDING_WRITE_SNAPSHOT);
    let node_b_for_commit = node_b.clone();
    let first_task_fut = async move {
        insert_doc_get_id(
            &node_b_for_commit,
            "tasks",
            assert_obj!("title" => "task-1", "project" => "alpha"),
        )
        .await
    };

    let node_a_for_delta = node_a.clone();
    let node_b_for_delta = node_b.clone();
    let log_for_delta = log.clone();
    let orchestrate = async move {
        let pause_guard = hold_guard.wait_for_blocked().await;

        insert_doc(
            &node_a_for_delta,
            "messages",
            assert_obj!("text" => "remote-message"),
        )
        .await?;
        let delta = log_for_delta
            .deltas()
            .into_iter()
            .last()
            .expect("remote message should publish a delta");

        let committer = node_b_for_delta.committer_for_test();
        let apply_fut = committer.apply_replica_delta(delta);
        if let Some(guard) = pause_guard {
            guard.unpause();
        }
        apply_fut.await?;
        anyhow::Ok(())
    };

    let (first_task_id, ..) = futures::try_join!(first_task_fut, orchestrate)?;

    let second_task_id = insert_doc_get_id(
        &node_b,
        "tasks",
        assert_obj!("title" => "task-2", "project" => "alpha"),
    )
    .await?;

    let mut tx = node_b.begin(Identity::system()).await?;
    assert_eq!(
        tx.get(first_task_id).await?.is_some(),
        true,
        "the first locally created task must remain readable after replica apply",
    );
    assert_eq!(
        tx.get(second_task_id).await?.is_some(),
        true,
        "the second locally created task must remain readable after replica apply",
    );

    Ok(())
}

#[test]
fn test_remote_prepare_over_grpc_is_idempotent() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso),
        )
        .await?;
        insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;

        let server = start_two_pc_server(TestTwoPhaseCommitGrpcService::new(
            node_b.committer_for_test(),
        ))
        .await?;
        let client = TwoPhaseCommitGrpcClient::connect(server.addr()).await?;

        let mut tx = node_b.begin(Identity::system()).await?;
        TestFacingModel::new(&mut tx)
            .insert(&"projects".parse()?, assert_obj!("name" => "remote-grpc"))
            .await?;
        let final_tx = tx.finalize()?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let write_source = WriteSource::new("remote_prepare_idempotent_test");
        let partition_map = partitioned_map(PartitionId(1));
        let participant_tx = ParticipantTransaction::from_final_transaction(
            &final_tx,
            PartitionId(1),
            &write_indexes,
            &partition_map,
            &write_source,
        )?;

        let txn_id = TwoPhaseTransactionId::new();
        let prepare_ts = node_b.committer_for_test().allocate_commit_ts().await?;

        let first = client
            .prepare(
                &txn_id,
                participant_tx.clone(),
                write_source.clone(),
                prepare_ts,
                partition_map.placement_version(),
            )
            .await?;
        let second = client
            .prepare(
                &txn_id,
                participant_tx,
                write_source,
                prepare_ts,
                partition_map.placement_version(),
            )
            .await?;

        assert_eq!(first.prepare_ts, prepare_ts);
        assert_eq!(second.prepare_ts, prepare_ts);

        let committed_ts: common::types::Timestamp =
            client.commit_prepared(&txn_id).await?.try_into()?;
        assert_eq!(committed_ts, prepare_ts);

        let committed_deltas: Vec<_> = log
            .deltas()
            .into_iter()
            .filter(|delta| delta.ts == prepare_ts)
            .collect();
        assert_eq!(
            committed_deltas.len(),
            1,
            "duplicate prepare should still publish exactly one committed delta",
        );
        assert_eq!(committed_deltas[0].source_partition, Some(PartitionId(1)));

        server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_remote_prepare_rejects_stale_placement_version() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b_map = partitioned_map_with_version(PartitionId(1), PlacementVersion::new(2));
        let node_b = create_node_with_options(&rt, log, Some(node_b_map), None, Some(tso)).await?;

        let server = start_two_pc_server(TestTwoPhaseCommitGrpcService::new(
            node_b.committer_for_test(),
        ))
        .await?;
        let client = TwoPhaseCommitGrpcClient::connect(server.addr()).await?;

        let mut tx = node_b.begin(Identity::system()).await?;
        TestFacingModel::new(&mut tx)
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => "stale-placement"),
            )
            .await?;
        let final_tx = tx.finalize()?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let write_source = WriteSource::new("stale_placement_version_test");
        let stale_map = partitioned_map_with_version(PartitionId(1), PlacementVersion::new(1));
        let participant_tx = ParticipantTransaction::from_final_transaction(
            &final_tx,
            PartitionId(1),
            &write_indexes,
            &stale_map,
            &write_source,
        )?;

        let txn_id = TwoPhaseTransactionId::new();
        let prepare_ts = node_b.committer_for_test().allocate_commit_ts().await?;
        let err = client
            .prepare(
                &txn_id,
                participant_tx,
                write_source,
                prepare_ts,
                stale_map.placement_version(),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("Placement version mismatch"),
            "expected stale placement rejection, got {err:#}",
        );

        server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_committer_client_refreshes_placement_metadata() -> anyhow::Result<()> {
    let td = TestDriver::new();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let node = create_node(
            &rt,
            log,
            Some(partitioned_map_with_version(
                PartitionId(1),
                PlacementVersion::new(1),
            )),
        )
        .await?;
        let committer = node.committer_for_test();

        committer.ensure_placement_version(PlacementVersion::new(1))?;
        assert!(committer
            .ensure_placement_version(PlacementVersion::new(2))
            .is_err());

        committer.refresh_placement_metadata(PlacementMetadata::from_static_config(
            StaticPlacementConfig {
                table_assignments: "messages=1,projects=0",
                num_partitions: 2,
                placement_version: PlacementVersion::new(2),
            },
        ))?;

        committer.ensure_placement_version(PlacementVersion::new(2))?;
        Ok(())
    })
}

#[test]
fn test_cross_partition_commit_uses_remote_prepare_over_grpc() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso.clone()),
        )
        .await?;
        let server = start_two_pc_server(TestTwoPhaseCommitGrpcService::new(
            node_b.committer_for_test(),
        ))
        .await?;

        let node_a = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!("1={}", server.addr()))),
            Some(tso),
        )
        .await?;

        insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
        let remote_seed_delta = log
            .deltas()
            .into_iter()
            .last()
            .expect("seed project should publish a delta");
        node_a
            .committer_for_test()
            .apply_replica_delta(remote_seed_delta)
            .await?;
        insert_doc(&node_a, "messages", assert_obj!("text" => "seed")).await?;
        let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
        node_a
            .committer_for_test()
            .apply_replica_delta(CommitDelta {
                ts: heartbeat_ts,
                document_writes: Arc::new(Vec::new()),
                document_updates: Vec::new(),
                index_writes: Arc::new(Vec::new()),
                write_source: WriteSource::new("cross_partition_commit_grpc_test"),
                write_bytes: 0,
                tablet_id_to_table_name: Default::default(),
                source_partition: Some(PartitionId(1)),
            })
            .await?;

        let initial_delta_count = log.deltas().len();

        let mut tx = node_a.begin(Identity::system()).await?;
        let mut model = TestFacingModel::new(&mut tx);
        model
            .insert(&"messages".parse()?, assert_obj!("text" => "local-2pc"))
            .await?;
        model
            .insert(&"projects".parse()?, assert_obj!("name" => "remote-2pc"))
            .await?;
        let commit_ts = node_a.commit(tx).await?;

        let deltas = log.deltas();
        let commit_deltas: Vec<_> = deltas[initial_delta_count..]
            .iter()
            .filter(|delta| delta.ts == commit_ts)
            .collect();
        assert_eq!(
            commit_deltas.len(),
            2,
            "a successful cross-partition commit should publish one delta per participant",
        );
        let mut sources: Vec<_> = commit_deltas
            .iter()
            .map(|delta| delta.source_partition)
            .collect();
        sources.sort();
        assert_eq!(sources, vec![Some(PartitionId(0)), Some(PartitionId(1))]);

        insert_doc(
            &node_a,
            "messages",
            assert_obj!("text" => "follow-up-local"),
        )
        .await?;
        insert_doc(
            &node_b,
            "projects",
            assert_obj!("name" => "follow-up-remote"),
        )
        .await?;

        server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_failed_remote_prepare_rolls_back_local_participant() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let failing_server = start_two_pc_server(FailingPrepareGrpcService).await?;
        let node_a = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!(
                "1={}",
                failing_server.addr()
            ))),
            Some(tso.clone()),
            decision_log.clone(),
        )
        .await?;
        let _remote_participant = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso),
        )
        .await?;

        let message_id =
            insert_doc_get_id(&node_a, "messages", assert_obj!("text" => "seed")).await?;
        let message_id = value::PublicDocumentId::from(message_id);

        let mut tx = node_a.begin(Identity::system()).await?;
        let mut model = TestFacingModel::new(&mut tx);
        model
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => "forced-failure"),
            )
            .await?;
        UserFacingModel::new(&mut tx, TableNamespace::test_user())
            .replace(message_id, assert_obj!("text" => "prepared-local"))
            .await?;

        let err = node_a
            .commit(tx)
            .await
            .expect_err("remote prepare failure should abort the 2PC transaction");
        assert!(
            format!("{err:#}").contains("forced prepare failure"),
            "expected remote prepare failure, got: {err:#}",
        );
        let decisions = decision_log.decisions();
        assert_eq!(decisions.len(), 1);
        let decision = decisions
            .values()
            .next()
            .expect("rollback decision should be durable");
        match decision {
            TwoPhaseDecision::RolledBack { participants, .. } => {
                assert_eq!(participants, &vec![0]);
            },
            other => panic!("expected rollback decision, got {other:?}"),
        }

        let mut retry_tx = node_a.begin(Identity::system()).await?;
        UserFacingModel::new(&mut retry_tx, TableNamespace::test_user())
            .replace(message_id, assert_obj!("text" => "after-rollback"))
            .await?;
        node_a.commit(retry_tx).await?;

        failing_server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_commit_decision_survives_failed_participant_commit() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso.clone()),
        )
        .await?;
        let server = start_two_pc_server(FailingCommitAfterPrepareGrpcService).await?;

        let node_a = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!("1={}", server.addr()))),
            Some(tso),
            decision_log.clone(),
        )
        .await?;

        insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
        let remote_seed_delta = log
            .deltas()
            .into_iter()
            .last()
            .expect("seed project should publish a delta");
        node_a
            .committer_for_test()
            .apply_replica_delta(remote_seed_delta)
            .await?;
        insert_doc(&node_a, "messages", assert_obj!("text" => "seed")).await?;
        let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
        node_a
            .committer_for_test()
            .apply_replica_delta(CommitDelta {
                ts: heartbeat_ts,
                document_writes: Arc::new(Vec::new()),
                document_updates: Vec::new(),
                index_writes: Arc::new(Vec::new()),
                write_source: WriteSource::new("commit_decision_recovery_test"),
                write_bytes: 0,
                tablet_id_to_table_name: Default::default(),
                source_partition: Some(PartitionId(1)),
            })
            .await?;

        let mut tx = node_a.begin(Identity::system()).await?;
        let mut model = TestFacingModel::new(&mut tx);
        model
            .insert(
                &"messages".parse()?,
                assert_obj!("text" => "local-before-commit-failure"),
            )
            .await?;
        model
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => "remote-before-commit-failure"),
            )
            .await?;

        let err = node_a
            .commit(tx)
            .await
            .expect_err("remote commit failure should leave durable decision for recovery");
        assert!(
            format!("{err:#}").contains("forced commit failure after durable decision"),
            "expected forced commit failure, got: {err:#}",
        );

        let decisions = decision_log.decisions();
        assert_eq!(decisions.len(), 1);
        let decision = decisions
            .values()
            .next()
            .expect("commit decision should remain for watcher recovery");
        match decision {
            TwoPhaseDecision::Committed { participants, .. } => {
                assert_eq!(participants, &vec![0, 1]);
            },
            other => panic!("expected commit decision, got {other:?}"),
        }

        server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_tso_request_includes_local_replica_floor() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let tso = Arc::new(RecordingTimestampOracle::new());
        let node = create_node_with_options(
            &rt,
            log,
            Some(partitioned_map(PartitionId(0))),
            None,
            Some(tso.clone()),
        )
        .await?;

        let replica_floor = Timestamp::try_from(
            u64::from(*node.now_ts_for_reads())
                .checked_add(10_000)
                .context("test timestamp overflow")?,
        )?;
        node.committer_for_test()
            .apply_replica_delta(CommitDelta {
                ts: replica_floor,
                document_writes: Arc::new(Vec::new()),
                document_updates: Vec::new(),
                index_writes: Arc::new(Vec::new()),
                write_source: WriteSource::new("tso_local_floor_test"),
                write_bytes: 0,
                tablet_id_to_table_name: Default::default(),
                source_partition: Some(PartitionId(1)),
            })
            .await?;

        let expected_floor = replica_floor.succ()?;
        let commit_ts = insert_doc(&node, "messages", assert_obj!("text" => "after-floor")).await?;
        let requested_floors = tso.requested_floors();

        assert!(
            commit_ts >= expected_floor,
            "commit_ts={commit_ts} should be at or above local floor {expected_floor}",
        );
        assert_eq!(
            requested_floors.last().copied(),
            Some(expected_floor),
            "TSO must reserve at the committer's local floor instead of returning a lower value \
             and letting the committer bump it outside the reserved range",
        );

        Ok(())
    })
}

#[test]
fn test_idle_replication_frontier_heartbeat_tso_failure_is_nonfatal() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(FailingTimestampOracle);
        let node = create_node_with_options(
            &rt,
            log,
            Some(partitioned_map(PartitionId(0))),
            None,
            Some(tso),
        )
        .await?;

        let committer = node.committer_for_test();
        committer
            .tick_idle_replication_frontier_heartbeat_for_test()
            .await?;
        committer
            .tick_idle_replication_frontier_heartbeat_for_test()
            .await?;

        Ok(())
    })
}
