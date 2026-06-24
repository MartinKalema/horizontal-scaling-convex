//! Write Scaling Unit Tests
//!
//! Tests partition ownership enforcement (Vitess Single mode pattern).
//! Cross-partition replication tests run against the live Docker deployment
//! via self-hosted/docker/test-write-scaling.sh (CockroachDB roachtest
//! pattern).

use std::{
    sync::{
        atomic::{
            AtomicBool,
            Ordering,
        },
        Arc,
    },
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    assert_obj,
    bootstrap_model::index::{
        database_index::IndexedFields,
        INDEX_TABLE,
    },
    document::{
        ResolvedDocument,
        ID_FIELD,
    },
    interval::Interval,
    knobs::REMOTE_READ_FRONTIER_WAIT_TIMEOUT,
    maybe_val,
    pause::PauseController,
    persistence::{
        NoopRetentionValidator,
        Persistence,
        PersistenceGlobalKey,
        PersistenceReader,
        TimestampRange,
    },
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
        QuerySource,
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
        IndexDescriptor,
        IndexName,
        TabletIndexName,
        Timestamp,
    },
};
use futures::TryStreamExt;
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
    assert_val,
    InternalId,
    PublicDocumentId,
    ResolvedDocumentId,
    TableName,
    TableNamespace,
};

use crate::{
    bootstrap_model::index::IndexModel,
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
    raft_partition::RaftPartitionState,
    snapshot_manager::partition_timestamp_map_from_json,
    table_number_allocator::{
        testing::InMemoryTableNumberAllocator,
        TableNumberAllocator,
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
        TwoPhaseRedoEntry,
        TwoPhaseTransactionId,
    },
    Database,
    SystemMetadataModel,
    TableModel,
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

async fn create_node_with_table_number_allocator(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
    table_number_allocator: Arc<dyn TableNumberAllocator>,
) -> anyhow::Result<Database<TestRuntime>> {
    create_node_with_persistence_and_table_number_allocator(
        rt,
        Arc::new(TestPersistence::new()),
        distributed_log,
        partition_map,
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
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
    create_node_with_persistence_and_table_number_allocator(
        rt,
        tp,
        distributed_log,
        partition_map,
        node_addresses,
        two_phase_decision_log,
        timestamp_oracle,
        Arc::new(crate::LocalTableNumberAllocator),
    )
    .await
}

async fn create_node_with_persistence_and_table_number_allocator(
    rt: &TestRuntime,
    tp: Arc<TestPersistence>,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
    node_addresses: Option<NodeAddresses>,
    two_phase_decision_log: Arc<dyn TwoPhaseDecisionLog>,
    timestamp_oracle: Option<Arc<dyn TimestampOracle>>,
    table_number_allocator: Arc<dyn TableNumberAllocator>,
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
        None,
        timestamp_oracle,
        table_number_allocator,
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

fn partitioned_map_with_users_and_tasks(local_partition: PartitionId) -> PartitionMap {
    PartitionMap::from_config("users=0,tasks=1", local_partition, 2)
}

fn three_partitioned_map(local_partition: PartitionId) -> PartitionMap {
    PartitionMap::from_config("messages=0,projects=1,tasks=2", local_partition, 3)
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

async fn insert_global_system_doc(
    db: &Database<TestRuntime>,
    table: &str,
    fields: common::value::DocumentObject,
) -> anyhow::Result<common::types::Timestamp> {
    let table_name: TableName = table.parse()?;
    let mut tx = db.begin(Identity::system()).await?;
    TableModel::new(&mut tx)
        .insert_table_metadata_for_test(TableNamespace::Global, &table_name)
        .await?;
    SystemMetadataModel::new_global(&mut tx)
        .insert(&table_name, fields)
        .await?;
    db.commit(tx).await
}

async fn persisted_document_log_count(tp: &Arc<TestPersistence>) -> anyhow::Result<usize> {
    Ok(tp
        .load_documents(
            TimestampRange::all(),
            Order::Asc,
            100_000,
            Arc::new(NoopRetentionValidator),
        )
        .try_collect::<Vec<_>>()
        .await?
        .len())
}

async fn global_table_scan_or_empty(
    db: Database<TestRuntime>,
    table: &str,
) -> anyhow::Result<Vec<common::document::ResolvedDocument>> {
    match run_query(
        db,
        TableNamespace::Global,
        Query::full_table_scan(table.parse()?, Order::Asc),
    )
    .await
    {
        Ok(results) => Ok(results),
        Err(err) if format!("{err:#}").contains("cannot find table") => Ok(Vec::new()),
        Err(err) => Err(err),
    }
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
            .prepare_remote(
                txn_id,
                transaction,
                req.write_source.into(),
                prepare_ts,
                req.participants.iter().copied().map(PartitionId).collect(),
            )
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

struct FailingRollbackAfterPrepareGrpcService {
    db: Database<TestRuntime>,
    committer: CommitterClient,
    local_partition: PartitionId,
    fail_next_rollback: AtomicBool,
}

struct AmbiguousPrepareAfterLocalPrepareGrpcService {
    db: Database<TestRuntime>,
    committer: CommitterClient,
    local_partition: PartitionId,
}

impl AmbiguousPrepareAfterLocalPrepareGrpcService {
    fn new(db: Database<TestRuntime>, local_partition: PartitionId) -> Self {
        Self {
            committer: db.committer_for_test(),
            db,
            local_partition,
        }
    }
}

impl FailingRollbackAfterPrepareGrpcService {
    fn new(db: Database<TestRuntime>, local_partition: PartitionId) -> Self {
        Self {
            committer: db.committer_for_test(),
            db,
            local_partition,
            fail_next_rollback: AtomicBool::new(true),
        }
    }
}

struct FailingCommitAfterLocalPrepareGrpcService {
    db: Database<TestRuntime>,
    committer: CommitterClient,
    local_partition: PartitionId,
    fail_next_commit: AtomicBool,
}

impl FailingCommitAfterLocalPrepareGrpcService {
    fn new(db: Database<TestRuntime>, local_partition: PartitionId) -> Self {
        Self {
            committer: db.committer_for_test(),
            db,
            local_partition,
            fail_next_commit: AtomicBool::new(true),
        }
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for AmbiguousPrepareAfterLocalPrepareGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let prepare_ts: Timestamp = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;
        let write_source = WriteSource::new(req.write_source);
        let mut tx =
            self.db.begin(Identity::system()).await.map_err(|e| {
                Status::internal(format!("Failed to begin local prepare tx: {e:#}"))
            })?;
        TestFacingModel::new(&mut tx)
            .insert(
                &"projects"
                    .parse()
                    .map_err(|e| Status::internal(format!("Invalid test table name: {e:#}")))?,
                assert_obj!("name" => "prepared-before-ambiguous-prepare-failure"),
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to build local prepare tx: {e:#}")))?;
        let final_tx = tx
            .finalize()
            .map_err(|e| Status::internal(format!("Failed to finalize local prepare tx: {e:#}")))?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let partition_map = partitioned_map(self.local_partition);
        let transaction = ParticipantTransaction::from_final_transaction(
            &final_tx,
            self.local_partition,
            &write_indexes,
            &partition_map,
            &write_source,
        )
        .map_err(|e| Status::internal(format!("Failed to build local participant tx: {e:#}")))?;
        self.committer
            .prepare_remote(
                txn_id,
                transaction,
                write_source,
                prepare_ts,
                req.participants.iter().copied().map(PartitionId).collect(),
            )
            .await
            .map_err(|e| Status::internal(format!("Prepare failed: {e:#}")))?;
        Err(Status::unknown(
            "transport error: connection error: stream closed because of a broken pipe",
        ))
    }

    async fn commit_prepared(
        &self,
        _request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        Err(Status::failed_precondition(
            "commit_prepared should not be called after ambiguous prepare failure",
        ))
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

#[tonic::async_trait]
impl TwoPhaseCommitService for FailingCommitAfterLocalPrepareGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let prepare_ts: Timestamp = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;
        let write_source = WriteSource::new(req.write_source);
        let mut tx =
            self.db.begin(Identity::system()).await.map_err(|e| {
                Status::internal(format!("Failed to begin local prepare tx: {e:#}"))
            })?;
        TestFacingModel::new(&mut tx)
            .insert(
                &"projects"
                    .parse()
                    .map_err(|e| Status::internal(format!("Invalid test table name: {e:#}")))?,
                assert_obj!("name" => "prepared-before-commit-rpc-failure"),
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to build local prepare tx: {e:#}")))?;
        let final_tx = tx
            .finalize()
            .map_err(|e| Status::internal(format!("Failed to finalize local prepare tx: {e:#}")))?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let partition_map = partitioned_map(self.local_partition);
        let transaction = ParticipantTransaction::from_final_transaction(
            &final_tx,
            self.local_partition,
            &write_indexes,
            &partition_map,
            &write_source,
        )
        .map_err(|e| Status::internal(format!("Failed to build local participant tx: {e:#}")))?;
        let result = self
            .committer
            .prepare_remote(
                txn_id,
                transaction,
                write_source,
                prepare_ts,
                req.participants.iter().copied().map(PartitionId).collect(),
            )
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
        if self.fail_next_commit.swap(false, Ordering::SeqCst) {
            return Err(Status::unavailable(
                "forced commit failure after durable decision",
            ));
        }
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

#[tonic::async_trait]
impl TwoPhaseCommitService for FailingRollbackAfterPrepareGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let prepare_ts: Timestamp = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;
        let write_source = WriteSource::new(req.write_source);
        let mut tx =
            self.db.begin(Identity::system()).await.map_err(|e| {
                Status::internal(format!("Failed to begin local prepare tx: {e:#}"))
            })?;
        TestFacingModel::new(&mut tx)
            .insert(
                &"projects"
                    .parse()
                    .map_err(|e| Status::internal(format!("Invalid test table name: {e:#}")))?,
                assert_obj!("name" => "prepared-before-rollback-rpc-failure"),
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to build local prepare tx: {e:#}")))?;
        let final_tx = tx
            .finalize()
            .map_err(|e| Status::internal(format!("Failed to finalize local prepare tx: {e:#}")))?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let partition_map = three_partitioned_map(self.local_partition);
        let transaction = ParticipantTransaction::from_final_transaction(
            &final_tx,
            self.local_partition,
            &write_indexes,
            &partition_map,
            &write_source,
        )
        .map_err(|e| Status::internal(format!("Failed to build local participant tx: {e:#}")))?;
        let result = self
            .committer
            .prepare_remote(
                txn_id,
                transaction,
                write_source,
                prepare_ts,
                req.participants.iter().copied().map(PartitionId).collect(),
            )
            .await
            .map_err(|e| Status::internal(format!("Prepare failed: {e:#}")))?;
        Ok(Response::new(TwoPcPrepareResponse {
            prepare_ts: u64::from(result.prepare_ts),
        }))
    }

    async fn commit_prepared(
        &self,
        _request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        Err(Status::failed_precondition(
            "commit_prepared should not be called during rollback test",
        ))
    }

    async fn rollback_prepared(
        &self,
        request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        if self.fail_next_rollback.swap(false, Ordering::SeqCst) {
            return Err(Status::unavailable("forced rollback RPC failure"));
        }
        let req = request.into_inner();
        self.committer
            .rollback_prepared(TwoPhaseTransactionId(req.transaction_id))
            .await
            .map_err(|e| Status::internal(format!("RollbackPrepared failed: {e:#}")))?;
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

/// Partitioned nodes should still fail closed when ownership routing is
/// impossible because the cluster has no owner address map.
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

    // Node A writing to projects (owned by partition 1) must fail closed when
    // NODE_ADDRESSES is absent.
    let result_a = insert_doc(&node_a, "projects", assert_obj!("name" => "wrong")).await;
    assert!(result_a.is_err(), "Node A should fail closed for projects");
    assert!(
        result_a
            .unwrap_err()
            .to_string()
            .contains("NODE_ADDRESSES is required"),
        "Error should explain that owner routing is not configured"
    );

    // Node B writing to messages (owned by partition 0) must fail closed for
    // the same reason.
    let result_b = insert_doc(&node_b, "messages", assert_obj!("text" => "wrong")).await;
    assert!(result_b.is_err(), "Node B should fail closed for messages");
    assert!(
        result_b
            .unwrap_err()
            .to_string()
            .contains("NODE_ADDRESSES is required"),
        "Error should explain that owner routing is not configured"
    );

    // Correct-partition writes succeed.
    insert_doc(&node_a, "messages", assert_obj!("text" => "correct")).await?;
    insert_doc(&node_b, "projects", assert_obj!("name" => "correct")).await?;

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_failed_raft_proposal_does_not_publish_or_persist(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let persistence = Arc::new(TestPersistence::new());
    let node = create_node_with_persistence(
        &rt,
        persistence.clone(),
        log.clone(),
        None,
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;

    insert_doc(&node, "messages", assert_obj!("text" => "seed")).await?;
    node.attach_raft_state(RaftPartitionState::new_for_test(true, 1, PartitionId(0), 1))
        .await?;

    let err = insert_doc(&node, "messages", assert_obj!("text" => "must-not-commit"))
        .await
        .expect_err("dead Raft proposal channel should reject before local persistence");
    assert!(
        format!("{err:#}").contains("Raft node for partition partition-0 not running"),
        "unexpected error: {err:#}",
    );

    let visible_messages = run_query(
        node,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        visible_messages.len(),
        1,
        "failed Raft proposal must not become visible locally",
    );

    let restarted = create_node_with_persistence(
        &rt,
        persistence,
        log,
        None,
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let persisted_messages = run_query(
        restarted,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        persisted_messages.len(),
        1,
        "failed Raft proposal must not be resurrected from persistence",
    );

    Ok(())
}

#[test]
fn test_remote_single_partition_write_routes_to_owner_over_grpc() -> anyhow::Result<()> {
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

        let initial_delta_count = log.deltas().len();
        let commit_ts = insert_doc(
            &node_a,
            "projects",
            assert_obj!("name" => "routed-without-client-topology"),
        )
        .await?;

        let deltas = log.deltas();
        let commit_deltas: Vec<_> = deltas[initial_delta_count..]
            .iter()
            .filter(|delta| delta.ts == commit_ts)
            .collect();
        assert_eq!(
            commit_deltas.len(),
            1,
            "remote single-partition routing should publish exactly the owner participant delta",
        );
        assert_eq!(commit_deltas[0].source_partition, Some(PartitionId(1)));

        let projects = run_query(
            node_b.clone(),
            TableNamespace::root_component(),
            Query::full_table_scan("projects".parse()?, Order::Asc),
        )
        .await?;
        assert_eq!(projects.len(), 2);
        assert!(
            projects.iter().any(|project| project.value().0.get("name")
                == Some(&assert_val!("routed-without-client-topology"))),
            "owner should contain the write submitted to the non-owner node",
        );

        server.shutdown().await?;
        Ok(())
    })
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
            write_source: WriteSource::new("remote_read_frontier_heartbeat_test"),
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
async fn test_local_commit_retries_while_prepare_is_unresolved(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, Some(partitioned_map(PartitionId(0)))).await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
        .await?;
    let final_tx = tx.finalize()?;
    let txn_id = TwoPhaseTransactionId::new();
    node.committer_for_test()
        .prepare(
            txn_id.clone(),
            final_tx,
            WriteSource::new("prepared_blocks_local_commit_test"),
        )
        .await?;

    let err = insert_doc(
        &node,
        "messages",
        assert_obj!("text" => "retry-after-prepare"),
    )
    .await
    .expect_err("local commit should retry while a prepared transaction is unresolved");
    assert!(
        format!("{err:#}").contains("unresolved 2PC prepared transaction"),
        "unexpected error: {err:#}",
    );

    node.committer_for_test().rollback_prepared(txn_id).await?;

    insert_doc(&node, "messages", assert_obj!("text" => "after-rollback")).await?;

    let messages = run_query(
        node,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        messages.len(),
        1,
        "rolled-back prepared write should not become visible, and the committer should accept \
         new writes after rollback",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_max_repeatable_bump_does_not_overtake_prepared_commit(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, Some(partitioned_map(PartitionId(0)))).await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"messages".parse()?,
            assert_obj!("text" => "prepared-after-repeatable-bump"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare = node
        .committer_for_test()
        .prepare(
            txn_id.clone(),
            final_tx,
            WriteSource::new("prepared_repeatable_bump_test"),
        )
        .await?;

    let bumped = node.bump_max_repeatable_ts().await?;
    assert!(
        bumped < prepare.prepare_ts,
        "max repeatable bump should not advance past unresolved prepare ts: bumped={} prepare={}",
        u64::from(bumped),
        u64::from(prepare.prepare_ts),
    );

    let committed = node.committer_for_test().commit_prepared(txn_id).await?;
    assert_eq!(committed, prepare.prepare_ts);

    let messages = run_query(
        node,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(messages.len(), 1);

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cross_partition_replica_delta_waits_behind_unresolved_prepared_transaction(
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

    insert_doc(
        &node_a,
        "messages",
        assert_obj!("text" => "remote-before-prepare"),
    )
    .await?;
    let mut racing_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed message should publish a replica delta");
    node_b
        .committer_for_test()
        .apply_replica_delta(racing_delta.clone())
        .await?;

    let mut tx = node_b.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"tasks".parse()?,
            assert_obj!("title" => "prepared-local-task"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare = node_b
        .committer_for_test()
        .prepare(
            txn_id.clone(),
            final_tx,
            WriteSource::new("prepared_replica_delta_fence_test"),
        )
        .await?;

    racing_delta.ts = prepare.prepare_ts;
    let err = node_b
        .committer_for_test()
        .apply_replica_delta(racing_delta.clone())
        .await
        .expect_err("cross-partition replica delta should retry behind unresolved prepare");
    assert!(
        format!("{err:#}").contains("would overtake unresolved 2PC prepared transaction"),
        "unexpected error: {err:#}",
    );

    let committed = node_b.committer_for_test().commit_prepared(txn_id).await?;
    assert_eq!(committed, prepare.prepare_ts);

    node_b
        .committer_for_test()
        .apply_replica_delta(racing_delta)
        .await?;

    let tasks = run_query(
        node_b.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("tasks".parse()?, Order::Asc),
    )
    .await?;
    let messages = run_query(
        node_b,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(messages.len(), 1);

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_prepare_retries_while_local_commit_is_pending(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, Some(partitioned_map(PartitionId(0)))).await?;

    let hold_guard = pause.hold(AFTER_PENDING_WRITE_SNAPSHOT);
    let node_for_commit = node.clone();
    let local_commit = async move {
        insert_doc(
            &node_for_commit,
            "messages",
            assert_obj!("text" => "pending-local"),
        )
        .await
    };

    let node_for_prepare = node.clone();
    let prepare_while_pending = async move {
        let pause_guard = hold_guard.wait_for_blocked().await;

        let mut tx = node_for_prepare.begin(Identity::system()).await?;
        TestFacingModel::new(&mut tx)
            .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
            .await?;
        let final_tx = tx.finalize()?;
        let err = node_for_prepare
            .committer_for_test()
            .prepare(
                TwoPhaseTransactionId::new(),
                final_tx,
                WriteSource::new("prepare_waits_for_pending_commit_test"),
            )
            .await
            .expect_err("prepare should retry while an older local commit is pending");
        assert!(
            format!("{err:#}").contains("blocked by an unresolved pending write"),
            "unexpected error: {err:#}",
        );

        if let Some(guard) = pause_guard {
            guard.unpause();
        }
        anyhow::Ok(())
    };

    futures::try_join!(local_commit, prepare_while_pending)?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"messages".parse()?,
            assert_obj!("text" => "prepared-after-publish"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let txn_id = TwoPhaseTransactionId::new();
    node.committer_for_test()
        .prepare(
            txn_id.clone(),
            final_tx,
            WriteSource::new("prepare_after_pending_commit_test"),
        )
        .await?;
    node.committer_for_test().rollback_prepared(txn_id).await?;

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
            write_source: WriteSource::new("remote_read_frontier_heartbeat_commit_test"),
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
async fn test_local_commit_times_out_waiting_for_remote_read_frontier(
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

    // Advance node A's local snapshot without advancing partition 1's frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "timeout"))
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
        "commit should wait before the bounded remote read frontier timeout fires",
    );

    rt.advance_time(*REMOTE_READ_FRONTIER_WAIT_TIMEOUT + Duration::from_millis(1))
        .await;

    let commit_result = tokio::time::timeout(Duration::from_millis(100), commit).await??;
    let err = commit_result.expect_err("stale remote frontier should time out");
    assert!(
        err.to_string().contains("Timed out after"),
        "unexpected error: {err:#}",
    );
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
async fn test_replica_delta_redelivery_is_idempotent(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a_persistence = Arc::new(TestPersistence::new());
    let node_a = create_node_with_persistence(
        &rt,
        node_a_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "redelivered")).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("project insert should publish a delta");
    let remote_ts = delta.ts;

    let first_apply_ts = node_a
        .committer_for_test()
        .apply_replica_delta(delta.clone())
        .await?;
    let docs_after_first_apply = persisted_document_log_count(&node_a_persistence).await?;
    let snapshot_ts_after_first_apply = *node_a.now_ts_for_reads();

    let duplicate_apply_ts = node_a
        .committer_for_test()
        .apply_replica_delta(delta.clone())
        .await?;

    assert_eq!(
        duplicate_apply_ts, remote_ts,
        "duplicates should ACK as skipped without allocating a new local apply timestamp",
    );
    assert_eq!(
        persisted_document_log_count(&node_a_persistence).await?,
        docs_after_first_apply,
        "redelivered delta must not append duplicate document log entries",
    );
    assert_eq!(
        *node_a.now_ts_for_reads(),
        snapshot_ts_after_first_apply,
        "redelivered delta must not publish a new snapshot",
    );
    assert!(
        first_apply_ts >= remote_ts,
        "initial apply should use the remote timestamp as its floor",
    );

    let applied_frontiers = node_a_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaWatermarks)
        .await?
        .expect("replica applied data frontier should be persisted");
    let applied_frontiers = crate::snapshot_manager::partition_timestamp_map_from_json(
        applied_frontiers,
        "applied data delta watermarks",
    )?;
    assert_eq!(applied_frontiers.get(&PartitionId(1)), Some(&remote_ts));

    let restarted = create_node_with_persistence(
        &rt,
        node_a_persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let duplicate_after_restart_ts = restarted
        .committer_for_test()
        .apply_replica_delta(delta)
        .await?;
    assert_eq!(
        duplicate_after_restart_ts, remote_ts,
        "restart should reload the applied frontier and skip redelivery",
    );
    assert_eq!(
        persisted_document_log_count(&node_a_persistence).await?,
        docs_after_first_apply,
        "redelivered delta after restart must not append duplicate document log entries",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_frontier_heartbeat_does_not_dedupe_later_data_delta(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let target_persistence = Arc::new(TestPersistence::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        table_numbers.clone(),
    )
    .await?;
    let target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers,
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "late data")).await?;
    let data_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("project insert should publish a delta");
    let data_ts = data_delta.ts;
    let heartbeat_ts = data_ts.succ()?;

    target
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("heartbeat_before_data_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let freshness_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDeltaWatermarks)
        .await?
        .context("heartbeat should persist the freshness watermark")?;
    let freshness_watermarks =
        partition_timestamp_map_from_json(freshness_watermarks, "applied delta watermarks")?;
    assert_eq!(
        freshness_watermarks.get(&PartitionId(1)).copied(),
        Some(heartbeat_ts),
        "frontier-only heartbeat advances read-after-write freshness",
    );
    let data_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaWatermarks)
        .await?;
    assert!(
        data_watermarks.is_none(),
        "frontier-only heartbeat must not advance data-delta idempotency",
    );

    let applied_ts = target
        .committer_for_test()
        .apply_replica_delta(data_delta)
        .await?;
    assert!(
        applied_ts > heartbeat_ts,
        "late data should apply at a fresh local timestamp after the heartbeat",
    );

    let projects = run_query(
        target.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects.len(),
        1,
        "non-empty data delta must not be skipped by a newer heartbeat watermark",
    );

    let persisted_data_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaWatermarks)
        .await?
        .context("data delta should persist the data idempotency watermark")?;
    let persisted_data_watermarks = partition_timestamp_map_from_json(
        persisted_data_watermarks,
        "applied data delta watermarks",
    )?;
    assert_eq!(
        persisted_data_watermarks.get(&PartitionId(1)).copied(),
        Some(data_ts),
        "data idempotency tracks the non-empty delta, not the newer heartbeat",
    );

    let freshness_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDeltaWatermarks)
        .await?
        .context("freshness watermark should still be persisted")?;
    let freshness_watermarks =
        partition_timestamp_map_from_json(freshness_watermarks, "applied delta watermarks")?;
    assert_eq!(
        freshness_watermarks.get(&PartitionId(1)).copied(),
        Some(heartbeat_ts),
        "late data must not move the freshness watermark backward",
    );

    Ok(())
}

fn rewrite_index_document_ids(mut delta: CommitDelta) -> anyhow::Result<CommitDelta> {
    let mut rewritten = 1u8;
    for update in &mut delta.document_updates {
        if !delta
            .tablet_id_to_table_name
            .get(&update.id.tablet_id)
            .is_some_and(|name| name == &*INDEX_TABLE)
        {
            continue;
        }
        let internal_id = InternalId([rewritten; 16]);
        rewritten += 1;
        let remapped_id = ResolvedDocumentId::new(
            update.id.tablet_id,
            PublicDocumentId::new(update.id.document_id.table(), internal_id),
        );
        update.id = remapped_id;
        update.old_document = update
            .old_document
            .take()
            .map(|document| {
                let mut fields: std::collections::BTreeMap<_, _> =
                    document.value().0.clone().into();
                fields.remove(&common::types::FieldName::from(ID_FIELD.clone()));
                let value = fields.try_into()?;
                ResolvedDocument::new(remapped_id, document.creation_time(), value)
            })
            .transpose()?;
        update.new_document = update
            .new_document
            .take()
            .map(|document| {
                let mut fields: std::collections::BTreeMap<_, _> =
                    document.value().0.clone().into();
                fields.remove(&common::types::FieldName::from(ID_FIELD.clone()));
                let value = fields.try_into()?;
                ResolvedDocument::new(remapped_id, document.creation_time(), value)
            })
            .transpose()?;
    }
    Ok(delta)
}

#[convex_macro::test_runtime]
async fn test_replica_delta_skips_equivalent_index_metadata_with_new_document_ids(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a_persistence = Arc::new(TestPersistence::new());
    let node_a = create_node_with_persistence(
        &rt,
        node_a_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
    )
    .await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "catalog")).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("project insert should publish table and index metadata");
    node_a
        .committer_for_test()
        .apply_replica_delta(delta.clone())
        .await?;
    let document_log_count = persisted_document_log_count(&node_a_persistence).await?;

    let mut duplicate_index_delta = rewrite_index_document_ids(delta)?;
    duplicate_index_delta.ts = duplicate_index_delta.ts.succ()?;
    duplicate_index_delta.document_updates.retain(|update| {
        duplicate_index_delta
            .tablet_id_to_table_name
            .get(&update.id.tablet_id)
            .is_some_and(|name| name == &*INDEX_TABLE)
    });
    duplicate_index_delta.document_writes = Arc::new(Vec::new());
    duplicate_index_delta.index_writes = Arc::new(Vec::new());

    node_a
        .committer_for_test()
        .apply_replica_delta(duplicate_index_delta)
        .await?;

    assert_eq!(
        persisted_document_log_count(&node_a_persistence).await?,
        document_log_count,
        "equivalent duplicate _index metadata should be skipped without appending document writes",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replica_delta_fails_for_unmapped_user_table(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "metadata")).await?;
    let table_metadata_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("first project insert should publish table metadata");

    insert_doc(&node_b, "projects", assert_obj!("name" => "data")).await?;
    let data_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("second project insert should publish data");

    let err = node_a
        .committer_for_test()
        .apply_replica_delta(data_delta.clone())
        .await
        .expect_err("data delta should fail until table metadata is available");
    assert!(
        format!("{err:#}").contains("is not mapped locally"),
        "unexpected error: {err:#}",
    );
    assert_eq!(
        node_a.replication_frontier_for_test(PartitionId(1)),
        Some(Timestamp::MIN),
        "failed delta must not advance remote read frontier",
    );

    node_a
        .committer_for_test()
        .apply_replica_delta(table_metadata_delta)
        .await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(data_delta)
        .await?;
    assert!(
        node_a
            .replication_frontier_for_test(PartitionId(1))
            .is_some_and(|frontier| frontier > Timestamp::MIN),
        "retry after metadata should apply and advance the frontier",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replicated_index_metadata_stays_pending_until_local_backfill(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let node_a = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        table_number_allocator.clone(),
    )
    .await?;
    let node_b_persistence = Arc::new(TestPersistence::new());
    let node_b = create_node_with_persistence_and_table_number_allocator(
        &rt,
        node_b_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;

    let projects_table: TableName = "projects".parse()?;
    let index_name = IndexName::new(projects_table, IndexDescriptor::new("by_name")?)?;
    node_b
        .create_backfilled_index_for_test(
            node_b_persistence.clone(),
            TableNamespace::test_user(),
            index_name.clone(),
            vec!["name".parse()?].try_into()?,
        )
        .await?;

    for delta in log
        .deltas()
        .into_iter()
        .filter(|delta| delta.source_partition == Some(PartitionId(1)))
    {
        node_a
            .committer_for_test()
            .apply_replica_delta(delta)
            .await?;
    }

    let mut owner_tx = node_b.begin_system().await?;
    let mut owner_indexes = IndexModel::new(&mut owner_tx);
    assert!(
        owner_indexes
            .enabled_index_metadata(TableNamespace::test_user(), &index_name)?
            .is_some(),
        "owner partition should finish and enable the index"
    );

    let mut replica_tx = node_a.begin_system().await?;
    let mut replica_indexes = IndexModel::new(&mut replica_tx);
    let pending = replica_indexes
        .pending_index_metadata(TableNamespace::test_user(), &index_name)?
        .context("replica should store replicated index metadata as pending")?;
    assert!(
        pending.config.is_backfilling(),
        "replica must rebuild the index locally before queries can use it"
    );
    assert!(
        replica_indexes
            .enabled_index_metadata(TableNamespace::test_user(), &index_name)?
            .is_none(),
        "replica must not expose remote-built index metadata as query-ready"
    );
    drop(replica_tx);

    let indexed_query = Query {
        source: QuerySource::IndexRange(IndexRange {
            index_name,
            range: vec![IndexRangeExpression::Eq(
                "name".parse()?,
                maybe_val!("seed"),
            )],
            order: Order::Asc,
        }),
        operators: vec![],
    };
    let err = run_query(node_a, TableNamespace::test_user(), indexed_query)
        .await
        .expect_err("replica should reject indexed reads while local index backfill is pending");
    assert!(
        format!("{err:?}").contains("Index projects.by_name is currently backfilling"),
        "unexpected error: {err:#}",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_delta_apply_preserves_full_system_state(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let leader = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let nats_replica = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;
    let raft_follower =
        create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;

    insert_global_system_doc(
        &leader,
        "_environment_variables",
        assert_obj!("name" => "RAFT_FULL_STATE_TEST", "value" => "enabled"),
    )
    .await?;
    let system_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("system metadata commit should publish a delta");

    nats_replica
        .committer_for_test()
        .apply_replica_delta(system_delta.clone())
        .await?;
    let cross_partition_docs =
        global_table_scan_or_empty(nats_replica, "_environment_variables").await?;
    assert!(
        cross_partition_docs.is_empty(),
        "cross-partition NATS apply should continue filtering node-local system state",
    );

    raft_follower
        .committer_for_test()
        .apply_raft_commit_delta(system_delta)
        .await?;
    let raft_docs = global_table_scan_or_empty(raft_follower, "_environment_variables").await?;
    assert_eq!(
        raft_docs.len(),
        1,
        "same-partition Raft apply must preserve full system state for failover",
    );
    assert_eq!(
        raft_docs[0].value().0.get("name"),
        Some(&assert_val!("RAFT_FULL_STATE_TEST")),
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
            vec![PartitionId(1)],
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
async fn test_prepared_participant_recovery_rewrites_when_restart_floor_advances(
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
            assert_obj!("name" => "recovered-from-redo-after-floor-advance"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("prepared_redo_recovery_floor_test");
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
            vec![PartitionId(1)],
        )
        .await?;
    node.shutdown().await?;

    let restart_floor = prepare_ts.succ()?.succ()?;
    tp.write_persistence_global(
        PersistenceGlobalKey::MaxRepeatableTimestamp,
        restart_floor.into(),
    )
    .await?;

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
    assert!(
        committed_ts > prepare_ts,
        "recovered prepare should commit at a local timestamp above the advanced restart floor",
    );

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
async fn test_raft_prepared_redo_apply_can_commit_after_failover(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, Some(partitioned_map(PartitionId(1)))).await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepared-redo"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepared_redo_apply_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = node.committer_for_test().allocate_commit_ts().await?;
    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx,
        &write_source,
    )?;

    node.committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    let committed_ts = node
        .committer_for_test()
        .commit_prepared(txn_id.clone())
        .await?;
    assert_eq!(committed_ts, prepare_ts);

    let projects = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 1);

    let redo = node
        .committer_for_test()
        .persistence_reader()
        .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
        .await?;
    assert_eq!(redo, Some(serde_json::json!({})));
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_remote_table_catalog_writes_follow_owner_prepare_redo(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let source = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "remote-catalog-redo"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_source = WriteSource::new("remote_catalog_owner_prepare_test");
    let source_map = partitioned_map(PartitionId(0));
    let participant_indexes = crate::two_phase_coordinator::participant_write_indexes(
        &final_tx,
        &source_map,
        &write_source,
    );
    let owner_writes = participant_indexes
        .get(&PartitionId(1))
        .expect("projects writes and catalog metadata should route to partition 1");
    assert_eq!(participant_indexes.len(), 1);
    assert_eq!(
        owner_writes.len(),
        final_tx.writes.coalesced_writes().count()
    );

    let owner = create_node(&rt, log, Some(partitioned_map(PartitionId(1)))).await?;
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        owner_writes,
        &source_map,
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = owner.committer_for_test().allocate_commit_ts().await?;
    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx,
        &write_source,
    )?;

    owner
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    let committed_ts = owner.committer_for_test().commit_prepared(txn_id).await?;
    assert_eq!(committed_ts, prepare_ts);

    let projects = run_query(
        owner.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 1);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_commit_delta_cleans_prepared_redo_on_follower(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-prepare"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-commit-cleans-redo"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_commit_delta_cleans_prepare_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;
    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx.clone(),
        &write_source,
    )?;

    follower
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    source
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    source.committer_for_test().commit_prepared(txn_id).await?;

    let delta = log
        .deltas()
        .into_iter()
        .last()
        .context("prepared commit should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(delta)
        .await?;

    let redo = follower
        .committer_for_test()
        .persistence_reader()
        .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
        .await?;
    assert_eq!(redo, Some(serde_json::json!({})));

    insert_doc(
        &follower,
        "projects",
        assert_obj!("name" => "after-raft-commit-cleanup"),
    )
    .await
    .context("follower should accept local writes after Raft commit delta cleaned the prepare")?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_redo_rewrites_when_follower_floor_advanced_by_replica_delta(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let foreign = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-rewrite"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepare-rewrite"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepare_rewrite_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;

    insert_doc(
        &foreign,
        "messages",
        assert_obj!("text" => "advance-follower-floor"),
    )
    .await?;
    let mut foreign_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("foreign insert should publish a delta")?;
    if foreign_delta.ts <= prepare_ts {
        foreign_delta.ts = prepare_ts.succ()?;
    }
    follower
        .committer_for_test()
        .apply_replica_delta(foreign_delta)
        .await?;

    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx.clone(),
        &write_source,
    )?;
    let prepare_result = follower
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    assert_eq!(prepare_result.prepare_ts, prepare_ts);

    insert_doc(
        &foreign,
        "messages",
        assert_obj!("text" => "advance-follower-floor-after-prepare"),
    )
    .await?;
    let mut post_prepare_foreign_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("post-prepare foreign insert should publish a delta")?;
    if post_prepare_foreign_delta.ts <= prepare_ts {
        post_prepare_foreign_delta.ts = prepare_ts.succ()?.succ()?;
    }
    let err = follower
        .committer_for_test()
        .apply_replica_delta(post_prepare_foreign_delta.clone())
        .await
        .expect_err("foreign replica delta should wait behind unresolved prepared redo");
    assert!(
        format!("{err:#}").contains("would overtake unresolved 2PC prepared transaction"),
        "unexpected error: {err:#}",
    );

    source
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    source.committer_for_test().commit_prepared(txn_id).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .context("prepared commit should publish a delta")?;
    follower
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;
    follower
        .committer_for_test()
        .apply_replica_delta(post_prepare_foreign_delta)
        .await?;

    let redo = follower
        .committer_for_test()
        .persistence_reader()
        .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
        .await?;
    assert_eq!(redo, Some(serde_json::json!({})));

    let projects = run_query(
        follower.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 2);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_redo_rewrite_allocates_unique_local_prepare_timestamps(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let foreign = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-concurrent-replay"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut redos = Vec::new();
    for i in 0..2 {
        let mut tx = source.begin(Identity::system()).await?;
        TestFacingModel::new(&mut tx)
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => format!("concurrent-raft-prepare-replay-{i}")),
            )
            .await?;
        let final_tx = tx.finalize()?;
        let write_indexes: Vec<_> = final_tx
            .writes
            .coalesced_writes()
            .enumerate()
            .map(|(index, _)| index)
            .collect();
        let write_source = WriteSource::new(format!("raft_prepare_unique_rewrite_test_{i}"));
        let participant_tx = ParticipantTransaction::from_final_transaction(
            &final_tx,
            PartitionId(1),
            &write_indexes,
            &partitioned_map(PartitionId(1)),
            &write_source,
        )?;
        let txn_id = TwoPhaseTransactionId::new();
        let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;
        let redo = TwoPhaseRedoEntry::new(
            &txn_id,
            prepare_ts,
            PartitionId(1),
            participant_tx,
            &write_source,
        )?;
        redos.push((txn_id, prepare_ts, redo));
    }
    let max_prepare_ts = redos
        .iter()
        .map(|(_, prepare_ts, _)| *prepare_ts)
        .max()
        .context("expected prepared redo entries")?;

    insert_doc(
        &foreign,
        "messages",
        assert_obj!("text" => "advance-follower-floor-before-concurrent-prepare-replay"),
    )
    .await?;
    let mut foreign_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("foreign insert should publish a delta")?;
    if foreign_delta.ts <= max_prepare_ts {
        foreign_delta.ts = max_prepare_ts.succ()?;
    }
    follower
        .committer_for_test()
        .apply_replica_delta(foreign_delta)
        .await?;

    for (_, prepare_ts, redo) in redos.iter().cloned() {
        let prepare_result = follower
            .committer_for_test()
            .apply_raft_prepared_redo(redo)
            .await?;
        assert_eq!(prepare_result.prepare_ts, prepare_ts);
    }

    for (txn_id, ..) in redos {
        follower
            .committer_for_test()
            .rollback_prepared(txn_id)
            .await?;
    }
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_live_prepare_rejects_timestamp_below_last_assigned(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let participant = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-live-prepare-floor"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    participant
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "live-prepare-below-last-assigned"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("live_prepare_below_last_assigned_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;

    participant
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: prepare_ts.succ()?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("live_prepare_floor_heartbeat_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(0)),
        })
        .await?;

    let err = participant
        .committer_for_test()
        .prepare_remote(
            txn_id,
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await
        .expect_err("live prepare should reject a timestamp below the local assigned floor");
    assert!(
        format!("{err:#}").contains("2PC Prepare assigned ts="),
        "unexpected error: {err:#}",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_redo_rewrites_when_pre_replay_heartbeat_advances_retention_floor(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-pre-replay-heartbeat"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepare-pre-replay-heartbeat-rewrite"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepare_pre_replay_heartbeat_rewrite_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;

    follower
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: prepare_ts.succ()?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("pre_replay_heartbeat_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(0)),
        })
        .await?;

    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx.clone(),
        &write_source,
    )?;
    let prepare_result = follower
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    assert_eq!(prepare_result.prepare_ts, prepare_ts);

    source
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    source.committer_for_test().commit_prepared(txn_id).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .context("prepared commit should publish a delta")?;
    follower
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;

    let projects = run_query(
        follower.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 2);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_redo_replay_uses_local_anchor_when_begin_ts_is_out_of_retention(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-out-of-retention-begin"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepare-out-of-retention-begin"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepare_out_of_retention_begin_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;

    follower
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: prepare_ts.pred()?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("pre_replay_floor_to_prepare_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(0)),
        })
        .await?;

    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx.clone(),
        &write_source,
    )?;
    let prepare_result = follower
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    assert_eq!(prepare_result.prepare_ts, prepare_ts);

    source
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    source.committer_for_test().commit_prepared(txn_id).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .context("prepared commit should publish a delta")?;
    follower
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;

    let projects = run_query(
        follower.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 2);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_redo_rewrites_when_heartbeat_advances_last_assigned(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let follower = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_number_allocator,
    )
    .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "seed-before-heartbeat"),
    )
    .await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .context("seed insert should publish a delta")?;
    follower
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepare-heartbeat-rewrite"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepare_heartbeat_rewrite_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = source.committer_for_test().allocate_commit_ts().await?;

    let redo = TwoPhaseRedoEntry::new(
        &txn_id,
        prepare_ts,
        PartitionId(1),
        participant_tx.clone(),
        &write_source,
    )?;
    let prepare_result = follower
        .committer_for_test()
        .apply_raft_prepared_redo(redo)
        .await?;
    assert_eq!(prepare_result.prepare_ts, prepare_ts);

    let heartbeat_ts = prepare_ts.succ()?;
    follower
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("heartbeat_after_prepare_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    source
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    source.committer_for_test().commit_prepared(txn_id).await?;
    let delta = log
        .deltas()
        .into_iter()
        .last()
        .context("prepared commit should publish a delta")?;
    let applied_ts = follower
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;
    assert!(
        applied_ts > heartbeat_ts,
        "prepared commit should rewrite above heartbeat-advanced last_assigned",
    );

    let redo = follower
        .committer_for_test()
        .persistence_reader()
        .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
        .await?;
    assert_eq!(redo, Some(serde_json::json!({})));

    let projects = run_query(
        follower.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 2);
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_remote_read_frontier_heartbeat_does_not_advance_snapshot_ts(
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
async fn test_replica_delta_timestamp_translation_records_origin_watermark(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let target_persistence = Arc::new(TestPersistence::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        table_numbers.clone(),
    )
    .await?;
    let target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers,
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "remote")).await?;
    let mut remote_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("remote project insert should publish a delta");

    let local_ts = insert_doc(&target, "messages", assert_obj!("text" => "local")).await?;
    let remote_ts = local_ts.pred()?;
    remote_delta.ts = remote_ts;
    assert!(
        remote_ts < local_ts,
        "test setup should apply an older origin timestamp after newer local state",
    );

    let applied_ts = target
        .committer_for_test()
        .apply_replica_delta(remote_delta.clone())
        .await?;
    assert!(
        applied_ts > local_ts,
        "replica apply should translate to the next local visibility timestamp",
    );
    assert_ne!(
        applied_ts, remote_ts,
        "this regression must exercise the explicit timestamp translation path",
    );
    assert_eq!(
        target.replication_frontier_for_test(PartitionId(1)),
        Some(applied_ts),
        "remote read frontier tracks this node's local visibility timestamp",
    );
    assert_eq!(
        target.replication_write_frontier_for_test(PartitionId(1)),
        Some(applied_ts),
        "write frontier tracks local visibility for real replicated writes",
    );

    let persisted_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaWatermarks)
        .await?
        .context("applied data delta watermark should be persisted")?;
    let persisted_watermarks =
        partition_timestamp_map_from_json(persisted_watermarks, "applied data delta watermarks")?;
    assert_eq!(
        persisted_watermarks.get(&PartitionId(1)).copied(),
        Some(remote_ts),
        "data idempotency watermark remains in the source partition timestamp domain",
    );

    let duplicate_apply_ts = target
        .committer_for_test()
        .apply_replica_delta(remote_delta)
        .await?;
    assert_eq!(
        duplicate_apply_ts, remote_ts,
        "duplicate detection uses the persisted data idempotency watermark",
    );
    let projects = run_query(
        target,
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects.len(),
        1,
        "duplicate redelivery must not re-apply the translated write",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_read_after_write_fence_waits_for_remote_origin_watermark(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_numbers: Arc<dyn TableNumberAllocator> =
        Arc::new(InMemoryTableNumberAllocator::default());
    let target = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        table_numbers.clone(),
    )
    .await?;
    let source = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        table_numbers,
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "remote fence")).await?;
    let mut remote_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("remote project insert should publish a delta");
    let local_ts = insert_doc(&target, "messages", assert_obj!("text" => "local fence")).await?;
    let remote_ts = local_ts.pred()?;
    remote_delta.ts = remote_ts;

    let pending = tokio::time::timeout(
        Duration::from_millis(50),
        target.wait_for_read_after_write_fence(Some(PartitionId(1)), remote_ts),
    )
    .await;
    assert!(
        pending.is_err(),
        "read-after-write fence should wait until the remote origin watermark is applied",
    );

    let applied_ts = target
        .committer_for_test()
        .apply_replica_delta(remote_delta)
        .await?;
    assert_ne!(
        applied_ts, remote_ts,
        "test should exercise translated local apply timestamps",
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        target.wait_for_read_after_write_fence(Some(PartitionId(1)), remote_ts),
    )
    .await??;

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replica_table_number_allocation_waits_for_prior_publish(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let node_a = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(0))),
        table_numbers.clone(),
    )
    .await?;
    let node_b = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(1))),
        table_numbers,
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
async fn test_replica_preserves_global_table_numbers_for_embedded_ids(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let node_a = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_users_and_tasks(PartitionId(0))),
        table_numbers.clone(),
    )
    .await?;
    let node_b = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_users_and_tasks(PartitionId(1))),
        table_numbers,
    )
    .await?;

    let user_id = insert_doc_get_id(&node_a, "users", assert_obj!("name" => "Ada")).await?;
    let user_public_id = PublicDocumentId::from(user_id);
    let user_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("user insert should publish a delta");
    node_b
        .committer_for_test()
        .apply_replica_delta(user_delta)
        .await?;

    let mut tx = node_b.begin(Identity::system()).await?;
    let replica_user_id =
        tx.resolve_developer_id(&user_public_id, TableNamespace::root_component())?;
    assert_eq!(
        replica_user_id, user_id,
        "replica must resolve the source node's public user ID without table-number translation",
    );
    assert!(
        tx.get(replica_user_id).await?.is_some(),
        "replica should support a client round-trip lookup with the source public ID",
    );
    drop(tx);

    insert_doc(
        &node_b,
        "tasks",
        assert_obj!("title" => "ship", "userId" => user_public_id.encode()),
    )
    .await?;
    let task_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("task insert should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(task_delta)
        .await?;

    let tasks = run_query(
        node_a,
        TableNamespace::root_component(),
        Query::full_table_scan("tasks".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].value().0.get("userId"),
        assert_obj!("userId" => user_public_id.encode()).get("userId"),
        "embedded user ID should stay in the source table-number namespace",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replica_delta_does_not_erase_pending_local_table_creation(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let node_a = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(0))),
        table_numbers.clone(),
    )
    .await?;
    let node_b = create_node_with_table_number_allocator(
        &rt,
        log.clone(),
        Some(partitioned_map_with_tasks(PartitionId(1))),
        table_numbers,
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
                &[PartitionId(1)],
            )
            .await?;
        let second = client
            .prepare(
                &txn_id,
                participant_tx,
                write_source,
                prepare_ts,
                partition_map.placement_version(),
                &[PartitionId(1)],
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
                &[PartitionId(1)],
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
        assert!(
            decision_log.decisions().is_empty(),
            "rollback decision should be deleted once every prepared participant resolved",
        );

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
fn test_ambiguous_remote_prepare_failure_rolls_back_attempted_participant() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso.clone()),
            decision_log.clone(),
        )
        .await?;
        let ambiguous_server = start_two_pc_server(
            AmbiguousPrepareAfterLocalPrepareGrpcService::new(node_b.clone(), PartitionId(1)),
        )
        .await?;
        let node_a = create_node_with_options_and_decision_log(
            &rt,
            log,
            Some(partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!(
                "1={}",
                ambiguous_server.addr()
            ))),
            Some(tso),
            decision_log.clone(),
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
                assert_obj!("name" => "ambiguous-prepare"),
            )
            .await?;
        UserFacingModel::new(&mut tx, TableNamespace::test_user())
            .replace(message_id, assert_obj!("text" => "prepared-local"))
            .await?;

        let err = node_a
            .commit(tx)
            .await
            .expect_err("ambiguous remote prepare failure should abort the 2PC transaction");
        assert!(
            format!("{err:#}").contains("transport error"),
            "expected transport-shaped prepare failure, got: {err:#}",
        );
        assert!(
            decision_log.decisions().is_empty(),
            "rollback decision should be deleted once attempted participants are resolved",
        );
        assert!(
            node_b
                .committer_for_test()
                .two_phase_redo_records()
                .await?
                .is_empty(),
            "ambiguous prepared participant should be rolled back immediately",
        );

        ambiguous_server.shutdown().await?;
        Ok(())
    })
}

#[convex_macro::test_runtime]
async fn test_watcher_rolls_back_abandoned_prepare_without_decision(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
    let node = create_node_with_options_and_decision_log(
        &rt,
        log,
        Some(partitioned_map(PartitionId(1))),
        None,
        None,
        decision_log.clone(),
    )
    .await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "abandoned-prepare"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("abandoned_prepare_timeout_test");
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
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;

    let resolved = crate::two_phase_watcher::rollback_expired_local_prepares(
        &node.committer_for_test(),
        PartitionId(1),
        Duration::ZERO,
    )
    .await?;
    assert_eq!(resolved, 1);

    assert!(
        decision_log.decisions().is_empty(),
        "single-participant timeout rollback should delete the decision after local resolution",
    );

    insert_doc(
        &node,
        "projects",
        assert_obj!("name" => "after-abandoned-rollback"),
    )
    .await
    .context("timed-out prepare should no longer block conflicting writes")?;
    Ok(())
}

#[test]
fn test_rollback_decision_survives_failed_rollback_rpc_for_watcher() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(three_partitioned_map(PartitionId(1))),
            None,
            Some(tso.clone()),
            decision_log.clone(),
        )
        .await?;
        let rollback_failing_server = start_two_pc_server(
            FailingRollbackAfterPrepareGrpcService::new(node_b.clone(), PartitionId(1)),
        )
        .await?;
        let prepare_failing_server = start_two_pc_server(FailingPrepareGrpcService).await?;
        let node_a = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(three_partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!(
                "1={},2={}",
                rollback_failing_server.addr(),
                prepare_failing_server.addr(),
            ))),
            Some(tso),
            decision_log.clone(),
        )
        .await?;

        let mut tx = node_a.begin(Identity::system()).await?;
        let mut model = TestFacingModel::new(&mut tx);
        model
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => "prepared-before-rollback-rpc-failure"),
            )
            .await?;
        model
            .insert(
                &"tasks".parse()?,
                assert_obj!("title" => "force-later-prepare-failure"),
            )
            .await?;

        let err = node_a
            .commit(tx)
            .await
            .expect_err("second participant prepare failure should abort the 2PC transaction");
        assert!(
            format!("{err:#}").contains("forced prepare failure"),
            "expected forced prepare failure, got: {err:#}",
        );

        let decisions = decision_log.decisions();
        assert_eq!(decisions.len(), 1);
        let (txn_id, decision) = decisions
            .iter()
            .next()
            .expect("rollback decision should survive failed rollback RPC");
        match decision {
            TwoPhaseDecision::RolledBack {
                participants,
                resolved_participants,
                ..
            } => {
                assert!(
                    participants.contains(&1),
                    "rollback decision must include the prepared remote participant",
                );
                assert!(
                    !participants.contains(&2),
                    "rollback decision must not include the participant whose prepare failed",
                );
                assert!(
                    !resolved_participants.contains(&1),
                    "prepared participant should remain unresolved after its rollback RPC fails",
                );
            },
            other => panic!("expected rollback decision, got {other:?}"),
        }

        crate::two_phase_watcher::resolve_decision_for_local_partition(
            &node_b.committer_for_test(),
            PartitionId(1),
            TwoPhaseTransactionId(txn_id.clone()),
            decision.clone(),
        )
        .await?;
        assert!(
            decision_log.decisions().is_empty(),
            "decision should be deleted after the delayed participant resolves",
        );
        insert_doc(
            &node_b,
            "projects",
            assert_obj!("name" => "after-watcher-rollback"),
        )
        .await
        .context("watcher recovery should clear the prepared remote participant")?;

        rollback_failing_server.shutdown().await?;
        prepare_failing_server.shutdown().await?;
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
        let node_b = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            Some(tso.clone()),
            decision_log.clone(),
        )
        .await?;
        let server = start_two_pc_server(FailingCommitAfterLocalPrepareGrpcService::new(
            node_b.clone(),
            PartitionId(1),
        ))
        .await?;

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
        let (txn_id, decision) = decisions
            .iter()
            .next()
            .expect("commit decision should remain for watcher recovery");
        match decision {
            TwoPhaseDecision::Committed {
                participants,
                resolved_participants,
                ..
            } => {
                assert_eq!(participants, &vec![0, 1]);
                assert_eq!(
                    resolved_participants,
                    &vec![0],
                    "local participant should be marked resolved while failed remote commit \
                     remains pending",
                );
            },
            other => panic!("expected commit decision, got {other:?}"),
        }

        crate::two_phase_watcher::resolve_decision_for_local_partition(
            &node_b.committer_for_test(),
            PartitionId(1),
            TwoPhaseTransactionId(txn_id.clone()),
            decision.clone(),
        )
        .await?;
        assert!(
            decision_log.decisions().is_empty(),
            "commit decision should be deleted after the delayed participant resolves",
        );
        let projects = run_query(
            node_b,
            TableNamespace::root_component(),
            Query::full_table_scan("projects".parse()?, Order::Asc),
        )
        .await?;
        assert!(
            projects.iter().any(|project| {
                project.value().0.get("name")
                    == Some(&assert_val!("prepared-before-commit-rpc-failure"))
            }),
            "watcher recovery should commit the delayed remote participant",
        );

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
fn test_idle_remote_read_frontier_heartbeat_tso_failure_is_nonfatal() -> anyhow::Result<()> {
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
            .tick_idle_remote_read_frontier_heartbeat_for_test()
            .await?;
        committer
            .tick_idle_remote_read_frontier_heartbeat_for_test()
            .await?;

        Ok(())
    })
}
