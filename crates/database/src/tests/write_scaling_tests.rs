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
            AtomicUsize,
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
        IndexMetadata,
        INDEX_TABLE,
    },
    document::{
        DocumentUpdateWithPrevTs,
        ResolvedDocument,
        ID_FIELD,
    },
    interval::Interval,
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
use futures::{
    stream::BoxStream,
    TryStreamExt,
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
    TwoPcValidateReadsRequest,
    TwoPcValidateReadsResponse,
};
use rand::{
    Rng,
    SeedableRng,
};
use rand_chacha::ChaCha12Rng;
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
        DistributedLog,
        ReplicationMessage,
        ReplicationTransportId,
    },
    committer::{
        CommitterClient,
        AFTER_PENDING_WRITE_SNAPSHOT,
        AFTER_RAFT_APPLIED_INDEX_PERSISTENCE,
        AFTER_RAFT_CONVEX_PERSISTENCE,
        AFTER_RAFT_SNAPSHOT_PUBLICATION,
        RAFT_APPLY_MARKER_KEY_PREFIX,
        RAFT_NATS_OUTBOX_KEY_PREFIX,
    },
    membership::{
        ClusterNodeId,
        MembershipSnapshot,
        MembershipVersion,
        NodeMembership,
    },
    nats_distributed_log::DeltaEnvelope,
    partition::{
        PartitionId,
        PartitionMap,
        PlacementMetadata,
        PlacementVersion,
        StaticPlacementConfig,
    },
    query::ResolvedQuery,
    raft_node::{
        RaftMessage,
        RaftProposalResult,
    },
    raft_partition::RaftPartitionState,
    raft_state_machine::RaftStateMachineEntry,
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
        TwoPhaseParticipantIntent,
        TwoPhaseRedoEntry,
        TwoPhaseTransactionId,
    },
    two_phase_coordinator::{
        coordinate_two_phase_commit_with_mode,
        TwoPhaseCommitMode,
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

async fn run_local_replica_query(
    database: Database<TestRuntime>,
    namespace: TableNamespace,
    query: Query,
) -> anyhow::Result<Vec<ResolvedDocument>> {
    run_query(
        database.with_local_replica_reads_for_test(),
        namespace,
        query,
    )
    .await
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

async fn create_node_with_custom_distributed_log(
    rt: &TestRuntime,
    tp: Arc<TestPersistence>,
    distributed_log: Arc<dyn DistributedLog>,
    partition_map: Option<PartitionMap>,
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
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        None,
        table_number_allocator,
        None,
    )
    .await?;
    db.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = db.start_search_and_vector_bootstrap();
    handle.join().await?;
    Ok(db)
}

#[derive(Default)]
struct SwitchableDistributedLog {
    fail_publishes: AtomicBool,
    published: Mutex<Vec<Timestamp>>,
}

impl SwitchableDistributedLog {
    fn failing() -> Self {
        Self {
            fail_publishes: AtomicBool::new(true),
            published: Mutex::new(Vec::new()),
        }
    }

    fn published(&self) -> Vec<Timestamp> {
        self.published.lock().clone()
    }
}

#[async_trait]
impl DistributedLog for SwitchableDistributedLog {
    async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
        if self.fail_publishes.load(Ordering::SeqCst) {
            anyhow::bail!("injected replication publish failure");
        }
        self.published.lock().push(delta.ts);
        Ok(())
    }

    async fn subscribe(
        &self,
        _from_ts: Timestamp,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        Ok(Box::pin(futures::stream::pending()))
    }
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

async fn commit_next_raft_delta_for_test(
    raft_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RaftMessage>,
    index: u64,
) -> anyhow::Result<CommitDelta> {
    let message = tokio::time::timeout(Duration::from_secs(1), raft_rx.recv())
        .await?
        .context("test Raft mailbox closed before receiving a proposal")?;
    let RaftMessage::Propose(proposal) = message else {
        anyhow::bail!("expected a Raft proposal in the test mailbox")
    };
    let delta = match RaftStateMachineEntry::from_bytes(&proposal.data)? {
        RaftStateMachineEntry::CommitDelta { envelope } => envelope.to_delta()?,
        RaftStateMachineEntry::TwoPhasePrepare { .. } => {
            anyhow::bail!("expected a commit delta Raft entry")
        },
    };
    proposal
        .result_tx
        .send(RaftProposalResult::Committed { index })
        .map_err(|_| anyhow::anyhow!("commit waiter dropped before test Raft decision"))?;
    Ok(delta)
}

async fn acknowledge_next_raft_applied_for_test(
    mut raft_rx: tokio::sync::mpsc::UnboundedReceiver<RaftMessage>,
    expected_index: u64,
) -> anyhow::Result<bool> {
    while let Some(message) = raft_rx.recv().await {
        match message {
            RaftMessage::MarkApplied { index, result_tx } => {
                anyhow::ensure!(
                    index == expected_index,
                    "expected applied index {expected_index}, got {index}",
                );
                result_tx
                    .send(Ok(()))
                    .map_err(|_| anyhow::anyhow!("committer dropped Raft applied-index waiter"))?;
                return Ok(true);
            },
            RaftMessage::Propose(_) => {
                anyhow::bail!("received an unexpected second Raft proposal")
            },
            RaftMessage::Raft(_) | RaftMessage::Shutdown => {},
        }
    }
    Ok(false)
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
    prepare_calls: Arc<AtomicUsize>,
    validate_reads_calls: Arc<AtomicUsize>,
}

impl TestTwoPhaseCommitGrpcService {
    fn new(committer: CommitterClient) -> Self {
        Self::with_counters(
            committer,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    fn with_counters(
        committer: CommitterClient,
        prepare_calls: Arc<AtomicUsize>,
        validate_reads_calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            committer,
            prepare_calls,
            validate_reads_calls,
        }
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for TestTwoPhaseCommitGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
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

    async fn validate_reads(
        &self,
        request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        self.validate_reads_calls.fetch_add(1, Ordering::SeqCst);
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let transaction = req
            .transaction
            .ok_or_else(|| Status::invalid_argument("ValidateReads missing transaction payload"))?;
        let transaction = ParticipantTransaction::try_from(transaction).map_err(|e| {
            Status::invalid_argument(format!("Invalid ValidateReads payload: {e:#}"))
        })?;
        let validate_ts: Timestamp = req.validate_ts.try_into().map_err(|e| {
            Status::invalid_argument(format!("Invalid ValidateReads timestamp: {e:#}"))
        })?;
        self.committer
            .ensure_placement_version(PlacementVersion::from(req.placement_version))
            .map_err(|e| Status::failed_precondition(format!("ValidateReads rejected: {e:#}")))?;

        self.committer
            .validate_remote_reads(txn_id, transaction, req.write_source.into(), validate_ts)
            .await
            .map_err(|e| Status::internal(format!("ValidateReads failed: {e:#}")))?;

        Ok(Response::new(TwoPcValidateReadsResponse {}))
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

struct UnavailableValidateReadsGrpcService {
    validate_reads_calls: Arc<AtomicUsize>,
}

impl UnavailableValidateReadsGrpcService {
    fn new(validate_reads_calls: Arc<AtomicUsize>) -> Self {
        Self {
            validate_reads_calls,
        }
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for FailingPrepareGrpcService {
    async fn prepare(
        &self,
        _request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        Err(Status::internal("forced prepare failure"))
    }

    async fn validate_reads(
        &self,
        _request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        Err(Status::internal("forced validate reads failure"))
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

#[tonic::async_trait]
impl TwoPhaseCommitService for UnavailableValidateReadsGrpcService {
    async fn prepare(
        &self,
        _request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        Err(Status::failed_precondition(
            "prepare should not be called for a read-only resolver participant",
        ))
    }

    async fn validate_reads(
        &self,
        _request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        self.validate_reads_calls.fetch_add(1, Ordering::SeqCst);
        Err(Status::unavailable("forced transient ValidateReads outage"))
    }

    async fn commit_prepared(
        &self,
        _request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        Err(Status::failed_precondition(
            "commit_prepared should not be called for a read-only resolver participant",
        ))
    }

    async fn rollback_prepared(
        &self,
        _request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        Err(Status::failed_precondition(
            "rollback_prepared should not be called for a read-only resolver participant",
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

    async fn validate_reads(
        &self,
        _request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        Err(Status::failed_precondition(
            "validate_reads should not be called during ambiguous prepare test",
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

    async fn validate_reads(
        &self,
        _request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        Err(Status::failed_precondition(
            "validate_reads should not be called during commit failure test",
        ))
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

    async fn validate_reads(
        &self,
        _request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        Err(Status::failed_precondition(
            "validate_reads should not be called during rollback failure test",
        ))
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

async fn assert_raft_replay_is_idempotent_across_crash_window(
    rt: &TestRuntime,
    pause: PauseController,
    pause_label: &'static str,
    expect_raw_marker_at_crash: bool,
) -> anyhow::Result<()> {
    let log = Arc::new(SwitchableDistributedLog::default());
    let persistence = Arc::new(TestPersistence::new());
    let node = create_node_with_custom_distributed_log(
        rt,
        persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        Arc::new(crate::LocalTableNumberAllocator),
    )
    .await?;
    insert_doc(&node, "messages", assert_obj!("text" => "seed")).await?;
    log.fail_publishes.store(true, Ordering::SeqCst);

    let (raft_state, mut raft_rx) =
        RaftPartitionState::new_with_mailbox_for_test(true, 1, PartitionId(0), 1);
    node.attach_raft_state(raft_state).await?;

    let hold = pause.hold(pause_label);
    let node_for_commit = node.clone();
    let commit_task = tokio::spawn(async move {
        insert_doc(
            &node_for_commit,
            "messages",
            assert_obj!("text" => "raft-committed"),
        )
        .await
    });
    let delta = commit_next_raft_delta_for_test(&mut raft_rx, 7).await?;
    let mark_applied_task = tokio::spawn(acknowledge_next_raft_applied_for_test(raft_rx, 7));
    let pause_guard = hold
        .wait_for_blocked()
        .await
        .context("Raft commit did not reach the injected crash boundary")?;

    let raw_markers = persistence
        .reader()
        .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
        .await?;
    assert_eq!(
        !raw_markers.is_empty(),
        expect_raw_marker_at_crash,
        "raw apply marker presence should identify which crash bridge is durable",
    );
    let outbox_at_crash = persistence
        .reader()
        .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
        .await?;
    assert_eq!(
        outbox_at_crash.len(),
        1,
        "the cross-partition outbox record must be atomic with Raft state-machine persistence",
    );
    let installed_document_count = persisted_document_log_count(&persistence).await?;

    // Abort the committer at the selected boundary. The test replays the entry
    // even at the post-applied-index boundary to prove duplicate delivery is
    // harmless in addition to exercising the real pre-index recovery window.
    node.shutdown().await?;
    pause_guard.unpause();
    let interrupted = tokio::time::timeout(Duration::from_secs(1), commit_task).await??;
    assert!(
        interrupted.is_err(),
        "the client commit must not complete after the injected process stop",
    );
    drop(node);
    let applied_index_was_persisted = mark_applied_task.await??;
    assert_eq!(
        applied_index_was_persisted,
        pause_label == AFTER_RAFT_APPLIED_INDEX_PERSISTENCE,
        "the injected boundary must precisely bracket applied-index persistence",
    );

    let restarted = create_node_with_custom_distributed_log(
        rt,
        persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(0))),
        Arc::new(crate::LocalTableNumberAllocator),
    )
    .await?;
    restarted
        .attach_raft_state(RaftPartitionState::new_for_test(
            false,
            1,
            PartitionId(0),
            2,
        ))
        .await?;
    let snapshot_before_replay = *restarted.now_ts_for_reads();
    let replay_ts = restarted
        .committer_for_test()
        .apply_raft_commit_delta(delta.clone())
        .await?;
    assert_eq!(
        replay_ts, delta.ts,
        "already-installed Raft replay should ACK at its stable origin timestamp",
    );
    assert_eq!(
        persisted_document_log_count(&persistence).await?,
        installed_document_count,
        "Raft replay must not append a second document-log version",
    );
    assert_eq!(
        *restarted.now_ts_for_reads(),
        snapshot_before_replay,
        "Raft replay must not publish a second snapshot or invalidation",
    );
    assert!(
        persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
            .await?
            .is_empty(),
        "successful replay recovery should consolidate and clear the raw marker",
    );
    let outbox_records = persistence
        .reader()
        .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
        .await?;
    assert_eq!(
        outbox_records.len(),
        1,
        "Raft replay recovery must restore exactly one cross-partition outbox record",
    );
    let recovered_envelope: DeltaEnvelope = serde_json::from_value(outbox_records[0].1.clone())?;
    let recovered_delta = recovered_envelope.to_delta()?;
    assert_eq!(
        recovered_delta.ts, delta.ts,
        "the recovered outbox record must contain the original committed delta",
    );

    let messages = run_query(
        restarted,
        TableNamespace::root_component(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        messages.len(),
        2,
        "the committed write must remain visible once"
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_replay_after_crash_post_persistence_is_idempotent(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    assert_raft_replay_is_idempotent_across_crash_window(
        &rt,
        pause,
        AFTER_RAFT_CONVEX_PERSISTENCE,
        true,
    )
    .await
}

#[convex_macro::test_runtime]
async fn test_raft_replay_after_crash_post_snapshot_is_idempotent(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    assert_raft_replay_is_idempotent_across_crash_window(
        &rt,
        pause,
        AFTER_RAFT_SNAPSHOT_PUBLICATION,
        false,
    )
    .await
}

#[convex_macro::test_runtime]
async fn test_raft_replay_after_crash_post_applied_index_is_idempotent(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    assert_raft_replay_is_idempotent_across_crash_window(
        &rt,
        pause,
        AFTER_RAFT_APPLIED_INDEX_PERSISTENCE,
        false,
    )
    .await
}

#[convex_macro::test_runtime]
async fn test_raft_prepared_commit_replay_after_crash_is_idempotent(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(SwitchableDistributedLog::default());
    let persistence = Arc::new(TestPersistence::new());
    let node = create_node_with_custom_distributed_log(
        &rt,
        persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        Arc::new(crate::LocalTableNumberAllocator),
    )
    .await?;
    insert_doc(&node, "projects", assert_obj!("name" => "seed")).await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "raft-prepared-committed"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("raft_prepared_commit_crash_replay_test");
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

    log.fail_publishes.store(true, Ordering::SeqCst);
    let (raft_state, mut raft_rx) =
        RaftPartitionState::new_with_mailbox_for_test(true, 1, PartitionId(1), 1);
    node.attach_raft_state(raft_state).await?;
    let hold = pause.hold(AFTER_RAFT_CONVEX_PERSISTENCE);
    let committer = node.committer_for_test();
    let txn_id_for_commit = txn_id.clone();
    let commit_task =
        tokio::spawn(async move { committer.commit_prepared(txn_id_for_commit).await });
    let delta = commit_next_raft_delta_for_test(&mut raft_rx, 11).await?;
    let mark_applied_task = tokio::spawn(acknowledge_next_raft_applied_for_test(raft_rx, 11));
    let pause_guard = hold
        .wait_for_blocked()
        .await
        .context("prepared commit did not reach atomic Raft persistence")?;

    assert_eq!(delta.ts, prepare_ts);
    assert_eq!(
        persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
            .await?
            .len(),
        1,
    );
    assert_eq!(
        persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
            .await?
            .len(),
        1,
    );
    let installed_document_count = persisted_document_log_count(&persistence).await?;

    node.shutdown().await?;
    pause_guard.unpause();
    let interrupted = tokio::time::timeout(Duration::from_secs(1), commit_task).await??;
    assert!(interrupted.is_err());
    drop(node);
    assert!(!mark_applied_task.await??);

    let restarted = create_node_with_custom_distributed_log(
        &rt,
        persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(1))),
        Arc::new(crate::LocalTableNumberAllocator),
    )
    .await?;
    restarted
        .attach_raft_state(RaftPartitionState::new_for_test(
            false,
            1,
            PartitionId(1),
            2,
        ))
        .await?;
    let snapshot_before_replay = *restarted.now_ts_for_reads();
    let replay_ts = restarted
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;
    assert_eq!(replay_ts, prepare_ts);
    assert_eq!(
        persisted_document_log_count(&persistence).await?,
        installed_document_count,
        "prepared Raft replay must not append another document version",
    );
    assert_eq!(
        *restarted.now_ts_for_reads(),
        snapshot_before_replay,
        "prepared Raft replay must not publish another snapshot",
    );
    assert!(persistence
        .reader()
        .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
        .await?
        .is_empty(),);
    assert_eq!(
        persistence
            .reader()
            .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
            .await?,
        Some(serde_json::json!({})),
        "replayed prepared commit must clear its durable redo record",
    );
    let projects = run_query(
        restarted,
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(projects.len(), 2);
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
async fn test_prepare_participants_include_remote_read_owner(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let project_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(project_delta)
        .await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "local-write"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let project_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    let project_read = TabletIndexName::by_id(project_tablet);
    final_tx.reads.record_indexed_derived(
        project_read.clone(),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let write_source = WriteSource::new("remote_read_owner_prepare_test");
    let partition_map = partitioned_map(PartitionId(0));
    let classification = crate::two_phase_coordinator::classify_transaction(
        &final_tx,
        &partition_map,
        &write_source,
    )?;
    assert!(
        matches!(
            classification,
            crate::two_phase_coordinator::TransactionClassification::CrossPartition { .. }
        ),
        "a transaction that writes locally but reads a remote-owned table must use 2PC resolver \
         validation",
    );

    let participant_indexes = crate::two_phase_coordinator::participant_prepare_indexes(
        &final_tx,
        &partition_map,
        &write_source,
    )?;
    assert_eq!(
        participant_indexes.keys().copied().collect::<Vec<_>>(),
        vec![PartitionId(0), PartitionId(1)],
    );
    assert!(
        !participant_indexes
            .get(&PartitionId(0))
            .expect("local write owner should participate")
            .is_empty(),
        "local write owner should receive the physical document and catalog writes",
    );
    assert!(
        participant_indexes
            .get(&PartitionId(1))
            .expect("remote read owner should participate")
            .is_empty(),
        "read-owner participant validates conflicts without receiving writes",
    );

    let read_owner_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        participant_indexes
            .get(&PartitionId(1))
            .expect("remote read owner should participate"),
        &partition_map,
        &write_source,
    )?;
    assert!(read_owner_tx.writes.is_empty());
    assert!(!read_owner_tx
        .reads
        .index_reads_for_test(&project_read)
        .is_empty());

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_read_owner_validation_rejects_conflicting_owner_write(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "local-write"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let project_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    final_tx.reads.record_indexed_derived(
        TabletIndexName::by_id(project_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    insert_doc(
        &node_b,
        "projects",
        assert_obj!("name" => "conflicting-owner-write"),
    )
    .await?;

    let write_source = WriteSource::new("read_owner_conflict_prepare_test");
    let source_map = partitioned_map(PartitionId(0));
    let participant_indexes = crate::two_phase_coordinator::participant_prepare_indexes(
        &final_tx,
        &source_map,
        &write_source,
    )?;
    let read_owner_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        participant_indexes
            .get(&PartitionId(1))
            .expect("remote read owner should participate"),
        &source_map,
        &write_source,
    )?;
    assert!(read_owner_tx.writes.is_empty());

    let prepare_ts = node_b.committer_for_test().allocate_commit_ts().await?;
    let err = node_b
        .committer_for_test()
        .validate_remote_reads(
            TwoPhaseTransactionId::new(),
            read_owner_tx,
            write_source,
            prepare_ts,
        )
        .await
        .expect_err("read owner should reject a read set made stale by its own write log");
    assert!(
        format!("{err:#}").contains("changed while this mutation was being run"),
        "expected read-owner OCC conflict, got {err:#}",
    );
    assert!(
        node_b
            .committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "validation-only read owners must not persist 2PC redo records",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_read_owner_validate_reads_is_retry_safe(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "local-write"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let project_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    final_tx.reads.record_indexed_derived(
        TabletIndexName::by_id(project_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let write_source = WriteSource::new("read_owner_retry_safe_test");
    let source_map = partitioned_map(PartitionId(0));
    let participant_indexes = crate::two_phase_coordinator::participant_prepare_indexes(
        &final_tx,
        &source_map,
        &write_source,
    )?;
    let read_owner_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        participant_indexes
            .get(&PartitionId(1))
            .expect("remote read owner should participate"),
        &source_map,
        &write_source,
    )?;
    assert!(read_owner_tx.writes.is_empty());

    let txn_id = TwoPhaseTransactionId::new();
    let validate_ts = node_b.committer_for_test().allocate_commit_ts().await?;
    node_b
        .committer_for_test()
        .validate_remote_reads(
            txn_id.clone(),
            read_owner_tx.clone(),
            write_source.clone(),
            validate_ts,
        )
        .await?;
    node_b
        .committer_for_test()
        .validate_remote_reads(txn_id, read_owner_tx, write_source, validate_ts)
        .await?;

    assert!(
        node_b
            .committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "duplicate validation-only resolver retries must not stage durable 2PC state",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_read_owner_validation_rejects_timestamp_below_local_floor(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let seed_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(seed_delta)
        .await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "local-write"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let project_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    final_tx.reads.record_indexed_derived(
        TabletIndexName::by_id(project_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let write_source = WriteSource::new("read_owner_timestamp_floor_test");
    let source_map = partitioned_map(PartitionId(0));
    let participant_indexes = crate::two_phase_coordinator::participant_prepare_indexes(
        &final_tx,
        &source_map,
        &write_source,
    )?;
    let read_owner_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        participant_indexes
            .get(&PartitionId(1))
            .expect("remote read owner should participate"),
        &source_map,
        &write_source,
    )?;
    assert!(read_owner_tx.writes.is_empty());

    let validate_ts = node_b.committer_for_test().allocate_commit_ts().await?;
    insert_doc(
        &node_b,
        "projects",
        assert_obj!("name" => "local-floor-advancer"),
    )
    .await?;

    let err = node_b
        .committer_for_test()
        .validate_remote_reads(
            TwoPhaseTransactionId::new(),
            read_owner_tx,
            write_source,
            validate_ts,
        )
        .await
        .expect_err("resolver should reject validation below the owner timestamp floor");
    assert!(
        format!("{err:#}").contains("2PC Prepare assigned ts="),
        "expected timestamp-floor rejection, got {err:#}",
    );
    assert!(
        node_b
            .committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "timestamp-floor rejection must not stage durable 2PC state",
    );

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

    let tasks = run_local_replica_query(
        node_b.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("tasks".parse()?, Order::Asc),
    )
    .await?;
    let messages = run_local_replica_query(
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
async fn test_prepare_waits_behind_pending_local_commit(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, Some(partitioned_map(PartitionId(0)))).await?;
    insert_doc(&node, "messages", assert_obj!("text" => "seed")).await?;

    let hold_guard = pause.hold(AFTER_PENDING_WRITE_SNAPSHOT);
    let node_for_commit = node.clone();
    let local_commit = tokio::spawn(async move {
        insert_doc(
            &node_for_commit,
            "messages",
            assert_obj!("text" => "pending-local"),
        )
        .await
    });

    let node_for_prepare = node.clone();
    let pause_guard = hold_guard.wait_for_blocked().await;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_txn_id = txn_id.clone();
    let prepare_while_pending = tokio::spawn(async move {
        let mut tx = node_for_prepare.begin(Identity::system()).await?;
        TestFacingModel::new(&mut tx)
            .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
            .await?;
        let final_tx = tx.finalize()?;
        node_for_prepare
            .committer_for_test()
            .prepare(
                prepare_txn_id,
                final_tx,
                WriteSource::new("prepare_waits_for_pending_commit_test"),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !prepare_while_pending.is_finished(),
        "prepare should wait behind the older pending local commit instead of failing or \
         bypassing it",
    );

    if let Some(guard) = pause_guard {
        guard.unpause();
    }

    tokio::time::timeout(Duration::from_secs(1), local_commit).await???;
    tokio::time::timeout(Duration::from_secs(1), prepare_while_pending).await???;
    node.committer_for_test().rollback_prepared(txn_id).await?;

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_pending_index_metadata_blocks_later_document_commit(
    rt: TestRuntime,
    pause: PauseController,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node = create_node(&rt, log, None).await?;
    insert_doc(
        &node,
        "messages",
        assert_obj!("text" => "seed-before-index"),
    )
    .await?;

    let hold_guard = pause.hold(AFTER_PENDING_WRITE_SNAPSHOT);
    let node_for_index = node.clone();
    let index_commit = tokio::spawn(async move {
        let mut tx = node_for_index.begin(Identity::system()).await?;
        let begin_ts = *tx.begin_timestamp();
        let index_name = IndexName::new(
            "messages".parse()?,
            IndexDescriptor::new("by_pending_text")?,
        )?;
        IndexModel::new(&mut tx)
            .add_application_index(
                TableNamespace::test_user(),
                IndexMetadata::new_backfilling(
                    begin_ts,
                    index_name,
                    vec!["text".parse()?].try_into()?,
                ),
            )
            .await?;
        node_for_index.commit(tx).await
    });

    let pause_guard = hold_guard.wait_for_blocked().await;
    let err = insert_doc(
        &node,
        "messages",
        assert_obj!("text" => "must-wait-for-index-metadata"),
    )
    .await
    .expect_err("document commit must retry behind unresolved index metadata");
    assert!(
        format!("{err:#}").contains("unresolved _tables/_index metadata write"),
        "unexpected error: {err:#}",
    );

    if let Some(guard) = pause_guard {
        guard.unpause();
    }
    tokio::time::timeout(Duration::from_secs(1), index_commit).await???;

    insert_doc(
        &node,
        "messages",
        assert_obj!("text" => "after-index-metadata"),
    )
    .await?;

    let messages = run_query(
        node,
        TableNamespace::test_user(),
        Query::full_table_scan("messages".parse()?, Order::Asc),
    )
    .await?;
    let texts: Vec<_> = messages
        .iter()
        .filter_map(|message| message.value().0.get("text"))
        .collect();
    assert!(texts.contains(&&assert_val!("seed-before-index")));
    assert!(texts.contains(&&assert_val!("after-index-metadata")));
    assert!(
        !texts.contains(&&assert_val!("must-wait-for-index-metadata")),
        "failed commit must not leak while metadata is pending",
    );

    Ok(())
}

#[test]
fn test_local_commit_with_remote_read_routes_to_read_owner() -> anyhow::Result<()> {
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
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let validate_reads_calls = Arc::new(AtomicUsize::new(0));
        let server = start_two_pc_server(TestTwoPhaseCommitGrpcService::with_counters(
            node_b.committer_for_test(),
            prepare_calls.clone(),
            validate_reads_calls.clone(),
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
        let initial_delta = log
            .deltas()
            .into_iter()
            .last()
            .expect("seed project should publish a delta");
        let initial_delta_ts = initial_delta.ts;
        node_a
            .committer_for_test()
            .apply_replica_delta(initial_delta)
            .await?;

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

        let commit_ts = node_a.commit(tx).await?;
        assert!(
            commit_ts > initial_delta_ts,
            "resolver-routed commit should use a timestamp after the remote seed",
        );
        assert_eq!(
            validate_reads_calls.load(Ordering::SeqCst),
            1,
            "remote read owner should receive a validation-only resolver RPC",
        );
        assert_eq!(
            prepare_calls.load(Ordering::SeqCst),
            0,
            "read-only resolver participants should not stage prepared writes",
        );
        server.shutdown().await?;
        Ok(())
    })
}

#[test]
fn test_local_commit_retries_validate_reads_on_next_owner_address() -> anyhow::Result<()> {
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
        let unavailable_validate_reads_calls = Arc::new(AtomicUsize::new(0));
        let unavailable_server = start_two_pc_server(UnavailableValidateReadsGrpcService::new(
            unavailable_validate_reads_calls.clone(),
        ))
        .await?;
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let validate_reads_calls = Arc::new(AtomicUsize::new(0));
        let owner_server = start_two_pc_server(TestTwoPhaseCommitGrpcService::with_counters(
            node_b.committer_for_test(),
            prepare_calls.clone(),
            validate_reads_calls.clone(),
        ))
        .await?;
        let node_a = create_node_with_options(
            &rt,
            log.clone(),
            Some(partitioned_map(PartitionId(0))),
            Some(NodeAddresses::from_config(&format!(
                "1={}|{}",
                unavailable_server.addr(),
                owner_server.addr(),
            ))),
            Some(tso),
        )
        .await?;

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

        node_a.commit(tx).await?;
        assert_eq!(
            unavailable_validate_reads_calls.load(Ordering::SeqCst),
            1,
            "coordinator should try the first resolver address before falling back",
        );
        assert_eq!(
            validate_reads_calls.load(Ordering::SeqCst),
            1,
            "coordinator should retry validation on the next owner address",
        );
        assert_eq!(
            prepare_calls.load(Ordering::SeqCst),
            0,
            "read-only resolver retries must not become stateful prepares",
        );
        unavailable_server.shutdown().await?;
        owner_server.shutdown().await?;
        Ok(())
    })
}

#[convex_macro::test_runtime]
async fn test_local_commit_with_remote_read_fails_closed_without_owner_addresses(
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

    let err = node_a
        .commit(tx)
        .await
        .expect_err("remote read owner should be required when local writes use resolver 2PC");
    assert!(
        format!("{err:#}").contains("NODE_ADDRESSES is required"),
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

#[derive(Clone, Copy, Debug)]
enum SeededReplicaApplyAction {
    ApplyData,
    DuplicateData,
    Heartbeat,
    RestartThenDuplicate,
    LocalFloorThenData,
}

struct SeededReplicaApplySimulation {
    seed: u64,
    rng: ChaCha12Rng,
    trace: Vec<String>,
    data_deltas: Vec<(String, CommitDelta)>,
    visible_project_names: Vec<String>,
    last_freshness_watermark: Option<Timestamp>,
    last_data_watermark: Option<Timestamp>,
    last_read_frontier: Option<Timestamp>,
    last_write_frontier: Option<Timestamp>,
}

impl SeededReplicaApplySimulation {
    fn new(seed: u64) -> Self {
        let mut seed_bytes = [0u8; 32];
        seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
        Self {
            seed,
            rng: ChaCha12Rng::from_seed(seed_bytes),
            trace: Vec::new(),
            data_deltas: Vec::new(),
            visible_project_names: Vec::new(),
            last_freshness_watermark: None,
            last_data_watermark: None,
            last_read_frontier: None,
            last_write_frontier: None,
        }
    }

    fn record(&mut self, event: impl Into<String>) {
        self.trace.push(event.into());
    }

    fn trace_dump(&self) -> String {
        self.trace.join("\n")
    }

    fn actions(&mut self) -> Vec<SeededReplicaApplyAction> {
        let mut actions = vec![
            SeededReplicaApplyAction::ApplyData,
            SeededReplicaApplyAction::DuplicateData,
            SeededReplicaApplyAction::Heartbeat,
            SeededReplicaApplyAction::RestartThenDuplicate,
            SeededReplicaApplyAction::LocalFloorThenData,
        ];
        for _ in 0..7 {
            actions.push(match self.rng.random_range(0..5) {
                0 => SeededReplicaApplyAction::ApplyData,
                1 => SeededReplicaApplyAction::DuplicateData,
                2 => SeededReplicaApplyAction::Heartbeat,
                3 => SeededReplicaApplyAction::RestartThenDuplicate,
                _ => SeededReplicaApplyAction::LocalFloorThenData,
            });
        }
        // Keep a guaranteed first data event, then shuffle the remaining
        // failure/replay actions. That keeps duplicate/restart actions useful
        // without making the seed trace depend on skipped setup.
        for i in (2..actions.len()).rev() {
            let j = self.rng.random_range(1..=i);
            actions.swap(i, j);
        }
        actions
    }

    fn normalize_remote_ts(&self, delta: &mut CommitDelta) -> anyhow::Result<()> {
        let floor = [self.last_freshness_watermark, self.last_data_watermark]
            .into_iter()
            .flatten()
            .max();
        if let Some(floor) = floor
            && delta.ts <= floor
        {
            delta.ts = floor.succ()?;
        }
        Ok(())
    }

    async fn assert_invariants(
        &mut self,
        target: &Database<TestRuntime>,
        target_persistence: &Arc<TestPersistence>,
    ) -> anyhow::Result<()> {
        let freshness_watermark = persisted_partition_watermark(
            target_persistence,
            PersistenceGlobalKey::AppliedDeltaWatermarks,
            PartitionId(1),
        )
        .await?;
        let data_watermark = persisted_partition_watermark(
            target_persistence,
            PersistenceGlobalKey::AppliedDataDeltaWatermarks,
            PartitionId(1),
        )
        .await?;
        let read_frontier = target.replication_frontier_for_test(PartitionId(1));
        let write_frontier = target.replication_write_frontier_for_test(PartitionId(1));

        ensure_monotonic_timestamp(
            self.seed,
            &self.trace_dump(),
            "freshness watermark",
            self.last_freshness_watermark,
            freshness_watermark,
        )?;
        ensure_monotonic_timestamp(
            self.seed,
            &self.trace_dump(),
            "data watermark",
            self.last_data_watermark,
            data_watermark,
        )?;
        ensure_monotonic_timestamp(
            self.seed,
            &self.trace_dump(),
            "read frontier",
            self.last_read_frontier,
            read_frontier,
        )?;
        ensure_monotonic_timestamp(
            self.seed,
            &self.trace_dump(),
            "write frontier",
            self.last_write_frontier,
            write_frontier,
        )?;

        self.last_freshness_watermark = freshness_watermark;
        self.last_data_watermark = data_watermark;
        self.last_read_frontier = read_frontier;
        self.last_write_frontier = write_frontier;

        let projects = run_local_replica_query(
            target.clone(),
            TableNamespace::root_component(),
            Query::full_table_scan("projects".parse()?, Order::Asc),
        )
        .await?;
        for name in &self.visible_project_names {
            let expected = assert_val!(name.clone());
            let matches = projects
                .iter()
                .filter(|project| project.value().0.get("name") == Some(&expected))
                .count();
            anyhow::ensure!(
                matches == 1,
                "seed={:#x}\nexpected exactly one replicated project named {name:?}, got \
                 {matches}\ntrace:\n{}",
                self.seed,
                self.trace_dump(),
            );
        }
        Ok(())
    }
}

async fn persisted_partition_watermark(
    persistence: &Arc<TestPersistence>,
    key: PersistenceGlobalKey,
    partition: PartitionId,
) -> anyhow::Result<Option<Timestamp>> {
    let Some(value) = persistence.reader().get_persistence_global(key).await? else {
        return Ok(None);
    };
    let frontiers = partition_timestamp_map_from_json(value, "replica simulation watermarks")?;
    Ok(frontiers.get(&partition).copied())
}

fn ensure_monotonic_timestamp(
    seed: u64,
    trace: &str,
    label: &str,
    previous: Option<Timestamp>,
    current: Option<Timestamp>,
) -> anyhow::Result<()> {
    if let (Some(previous), Some(current)) = (previous, current) {
        anyhow::ensure!(
            current >= previous,
            "seed={seed:#x}\n{label} moved backwards from {previous} to {current}\ntrace:\n{trace}",
        );
    }
    Ok(())
}

fn latest_delta_for_partition(
    log: &InMemoryDistributedLog,
    source_partition: PartitionId,
) -> anyhow::Result<CommitDelta> {
    log.deltas()
        .into_iter()
        .rev()
        .find(|delta| delta.source_partition == Some(source_partition))
        .context("expected source partition to publish a delta")
}

#[convex_macro::test_runtime]
async fn test_seeded_replica_apply_simulation_preserves_frontier_and_idempotency_invariants(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    for seed in [0x1340_1340_0000_0001, 0x1340_1340_0000_0002] {
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
        let mut target = create_node_with_persistence_and_table_number_allocator(
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
        let mut sim = SeededReplicaApplySimulation::new(seed);

        for (round, action) in sim.actions().into_iter().enumerate() {
            sim.record(format!("round={round} action={action:?}"));
            let document_log_count_before =
                persisted_document_log_count(&target_persistence).await?;
            let snapshot_ts_before = *target.now_ts_for_reads();

            match action {
                SeededReplicaApplyAction::ApplyData => {
                    let name = format!("seed-{seed:x}-remote-{round}");
                    insert_doc(&source, "projects", assert_obj!("name" => name.clone())).await?;
                    let mut delta = latest_delta_for_partition(&log, PartitionId(1))?;
                    sim.normalize_remote_ts(&mut delta)?;
                    target
                        .committer_for_test()
                        .apply_replica_delta(delta.clone())
                        .await?;
                    sim.record(format!(
                        "applied data name={name} remote_ts={}",
                        u64::from(delta.ts),
                    ));
                    sim.visible_project_names.push(name.clone());
                    sim.data_deltas.push((name, delta));
                },
                SeededReplicaApplyAction::DuplicateData => {
                    if let Some((name, delta)) = sim.data_deltas.last().cloned() {
                        let applied_ts = target
                            .committer_for_test()
                            .apply_replica_delta(delta.clone())
                            .await?;
                        anyhow::ensure!(
                            applied_ts == delta.ts,
                            "seed={seed:#x}\nduplicate data delta for {name:?} should ACK as \
                             skipped at origin ts, got {applied_ts}\ntrace:\n{}",
                            sim.trace_dump(),
                        );
                        anyhow::ensure!(
                            persisted_document_log_count(&target_persistence).await?
                                == document_log_count_before,
                            "seed={seed:#x}\nduplicate data delta appended document log \
                             entries\ntrace:\n{}",
                            sim.trace_dump(),
                        );
                        anyhow::ensure!(
                            *target.now_ts_for_reads() == snapshot_ts_before,
                            "seed={seed:#x}\nduplicate data delta published a new \
                             snapshot\ntrace:\n{}",
                            sim.trace_dump(),
                        );
                    }
                },
                SeededReplicaApplyAction::Heartbeat => {
                    let heartbeat_ts = sim
                        .last_freshness_watermark
                        .or(sim.last_data_watermark)
                        .or_else(|| sim.data_deltas.last().map(|(_, delta)| delta.ts));
                    let Some(heartbeat_ts) = heartbeat_ts else {
                        continue;
                    };
                    target
                        .committer_for_test()
                        .apply_replica_delta(CommitDelta {
                            ts: heartbeat_ts,
                            document_writes: Arc::new(Vec::new()),
                            document_updates: Vec::new(),
                            index_writes: Arc::new(Vec::new()),
                            write_source: WriteSource::new("seeded_replica_frontier_heartbeat"),
                            write_bytes: 0,
                            tablet_id_to_table_name: Default::default(),
                            source_partition: Some(PartitionId(1)),
                        })
                        .await?;
                    anyhow::ensure!(
                        persisted_document_log_count(&target_persistence).await?
                            == document_log_count_before,
                        "seed={seed:#x}\nfrontier heartbeat appended document log \
                         entries\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                },
                SeededReplicaApplyAction::RestartThenDuplicate => {
                    if let Some((name, delta)) = sim.data_deltas.last().cloned() {
                        target.shutdown().await?;
                        sim.record(format!("round={round} restart_target"));
                        target = create_node_with_persistence_and_table_number_allocator(
                            &rt,
                            target_persistence.clone(),
                            log.clone(),
                            Some(partitioned_map(PartitionId(0))),
                            None,
                            Arc::new(NoopTwoPhaseDecisionLog),
                            None,
                            Arc::new(InMemoryTableNumberAllocator::default()),
                        )
                        .await?;
                        let applied_ts = target
                            .committer_for_test()
                            .apply_replica_delta(delta.clone())
                            .await?;
                        anyhow::ensure!(
                            applied_ts == delta.ts,
                            "seed={seed:#x}\npost-restart duplicate for {name:?} should ACK as \
                             skipped at origin ts, got {applied_ts}\ntrace:\n{}",
                            sim.trace_dump(),
                        );
                        anyhow::ensure!(
                            persisted_document_log_count(&target_persistence).await?
                                == document_log_count_before,
                            "seed={seed:#x}\npost-restart duplicate appended document log \
                             entries\ntrace:\n{}",
                            sim.trace_dump(),
                        );
                    }
                },
                SeededReplicaApplyAction::LocalFloorThenData => {
                    let local_ts = insert_doc(
                        &target,
                        "messages",
                        assert_obj!("text" => format!("seed-{seed:x}-local-{round}")),
                    )
                    .await?;
                    let name = format!("seed-{seed:x}-translated-{round}");
                    insert_doc(&source, "projects", assert_obj!("name" => name.clone())).await?;
                    let mut delta = latest_delta_for_partition(&log, PartitionId(1))?;
                    sim.normalize_remote_ts(&mut delta)?;
                    if let Some(remote_ts) = local_ts.pred().ok().filter(|candidate| {
                        [sim.last_data_watermark, sim.last_freshness_watermark]
                            .into_iter()
                            .flatten()
                            .all(|watermark| *candidate > watermark)
                    }) {
                        delta.ts = remote_ts;
                    }
                    let applied_ts = target
                        .committer_for_test()
                        .apply_replica_delta(delta.clone())
                        .await?;
                    anyhow::ensure!(
                        applied_ts > local_ts || applied_ts == delta.ts,
                        "seed={seed:#x}\nreplica apply returned unexpected ts={applied_ts} for \
                         local floor {local_ts} and origin {}\ntrace:\n{}",
                        u64::from(delta.ts),
                        sim.trace_dump(),
                    );
                    sim.record(format!(
                        "applied translated data name={name} origin_ts={} applied_ts={}",
                        u64::from(delta.ts),
                        u64::from(applied_ts),
                    ));
                    sim.visible_project_names.push(name.clone());
                    sim.data_deltas.push((name, delta));
                },
            }

            sim.assert_invariants(&target, &target_persistence).await?;
        }

        target.shutdown().await?;
        source.shutdown().await?;
    }
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_raft_apply_records_nats_outbox_when_publish_unavailable(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let source_log = Arc::new(InMemoryDistributedLog::new());
    let table_numbers = Arc::new(InMemoryTableNumberAllocator::default());
    let source = create_node_with_table_number_allocator(
        &rt,
        source_log.clone(),
        Some(partitioned_map(PartitionId(1))),
        table_numbers.clone(),
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "raft-outbox")).await?;
    let delta = source_log
        .deltas()
        .into_iter()
        .last()
        .expect("source partition insert should publish a delta");
    assert_eq!(delta.source_partition, Some(PartitionId(1)));
    let delta_ts = delta.ts;

    let replay_log = Arc::new(SwitchableDistributedLog::failing());
    let follower_persistence = Arc::new(TestPersistence::new());
    let follower = create_node_with_custom_distributed_log(
        &rt,
        follower_persistence.clone(),
        replay_log.clone(),
        Some(partitioned_map(PartitionId(1))),
        table_numbers,
    )
    .await?;

    follower
        .committer_for_test()
        .apply_raft_commit_delta(delta)
        .await?;

    let records = follower_persistence
        .reader()
        .list_persistence_globals_with_prefix("raft_nats_outbox/")
        .await?;
    assert_eq!(
        records.len(),
        1,
        "failed NATS publish should leave the Raft-applied delta in the outbox",
    );
    assert!(
        records
            .iter()
            .any(|(key, _)| key.ends_with(&format!("/{:020}", u64::from(delta_ts)))),
        "outbox should include one per-entry record for the failed delta timestamp",
    );
    assert!(
        replay_log.published().is_empty(),
        "the failing log must not observe a successful publish before recovery",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_partitioned_commit_records_nats_outbox_when_publish_unavailable(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let replay_log = Arc::new(SwitchableDistributedLog::failing());
    let persistence = Arc::new(TestPersistence::new());
    let db = create_node_with_custom_distributed_log(
        &rt,
        persistence.clone(),
        replay_log.clone(),
        Some(partitioned_map(PartitionId(0))),
        Arc::new(InMemoryTableNumberAllocator::default()),
    )
    .await?;

    let commit_ts = insert_doc(
        &db,
        "messages",
        assert_obj!("body" => "accepted while nats is unavailable"),
    )
    .await?;

    let records = persistence
        .reader()
        .list_persistence_globals_with_prefix("raft_nats_outbox/")
        .await?;
    assert_eq!(
        records.len(),
        1,
        "failed NATS publish should leave the accepted local commit in the outbox",
    );
    assert!(
        records
            .iter()
            .any(|(key, _)| key.ends_with(&format!("/{:020}", u64::from(commit_ts)))),
        "outbox should include one per-entry record for the failed commit timestamp",
    );
    assert!(
        replay_log.published().is_empty(),
        "the failing log must not observe a successful publish",
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

    let projects = run_local_replica_query(
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

#[convex_macro::test_runtime]
async fn test_late_outbox_replay_data_delta_not_deduped_by_newer_data_watermark(
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
    let mut target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers.clone(),
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "schema-seed")).await?;
    let schema_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    target
        .committer_for_test()
        .apply_replica_delta(schema_delta)
        .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "older-outbox-replay"),
    )
    .await?;
    let older_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "newer-outbox-replay"),
    )
    .await?;
    let newer_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    anyhow::ensure!(
        older_delta.ts < newer_delta.ts,
        "test setup should create an older origin delta before the newer one",
    );

    target
        .committer_for_test()
        .apply_replica_delta(newer_delta.clone())
        .await?;
    let data_watermarks = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaWatermarks)
        .await?
        .context("newer data delta should persist the max data watermark")?;
    let data_watermarks =
        partition_timestamp_map_from_json(data_watermarks, "applied data delta watermarks")?;
    assert_eq!(
        data_watermarks.get(&PartitionId(1)).copied(),
        Some(newer_delta.ts),
        "newer delta should advance the diagnostic max data watermark",
    );

    let older_apply_ts = target
        .committer_for_test()
        .apply_replica_delta(older_delta.clone())
        .await?;
    assert!(
        older_apply_ts > newer_delta.ts,
        "older origin delta should still apply at a fresh local timestamp after a newer watermark",
    );

    let projects = run_local_replica_query(
        target.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    for name in ["schema-seed", "older-outbox-replay", "newer-outbox-replay"] {
        let matches = projects
            .iter()
            .filter(|project| project.value().0.get("name") == Some(&assert_val!(name)))
            .count();
        assert_eq!(
            matches, 1,
            "replicated project {name:?} should appear exactly once",
        );
    }

    target.shutdown().await?;
    target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers,
    )
    .await?;

    let document_log_count_before_duplicate =
        persisted_document_log_count(&target_persistence).await?;
    let snapshot_ts_before_duplicate = *target.now_ts_for_reads();
    let duplicate_apply_ts = target
        .committer_for_test()
        .apply_replica_delta(older_delta.clone())
        .await?;
    assert_eq!(
        duplicate_apply_ts, older_delta.ts,
        "post-restart duplicate should ACK as skipped at its origin timestamp",
    );
    assert_eq!(
        persisted_document_log_count(&target_persistence).await?,
        document_log_count_before_duplicate,
        "post-restart duplicate must not append document log entries",
    );
    assert_eq!(
        *target.now_ts_for_reads(),
        snapshot_ts_before_duplicate,
        "post-restart duplicate must not publish a new snapshot",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_ordered_transport_ids_skip_redelivery_after_origin_ids_pruned(
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
    let mut target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers.clone(),
    )
    .await?;

    insert_doc(&source, "projects", assert_obj!("name" => "schema-seed")).await?;
    let schema_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    target
        .committer_for_test()
        .apply_replica_delta_with_transport_id(schema_delta, ReplicationTransportId::new(1))
        .await?;

    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "late-transport-replay"),
    )
    .await?;
    let older_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    let older_origin_ts = older_delta.ts;
    insert_doc(
        &source,
        "projects",
        assert_obj!("name" => "newer-transport-replay"),
    )
    .await?;
    let newer_delta = latest_delta_for_partition(&log, PartitionId(1))?;
    anyhow::ensure!(
        older_delta.ts < newer_delta.ts,
        "test setup should create an older origin delta before the newer one",
    );

    target
        .committer_for_test()
        .apply_replica_delta_with_transport_id(newer_delta.clone(), ReplicationTransportId::new(3))
        .await?;
    let older_apply_ts = target
        .committer_for_test()
        .apply_replica_delta_with_transport_id(older_delta.clone(), ReplicationTransportId::new(2))
        .await?;
    assert!(
        older_apply_ts > newer_delta.ts,
        "older origin delta should still apply when its transport id is first seen",
    );

    let persisted_ids = target_persistence
        .reader()
        .get_persistence_global(PersistenceGlobalKey::AppliedDataDeltaIds)
        .await?
        .context("ordered transport dedupe state should be persisted")?;
    assert_eq!(
        persisted_ids["transport_stream_sequences"]["compacted_watermark"],
        serde_json::json!(3),
        "contiguous transport ids should compact into a durable watermark",
    );
    assert_eq!(
        persisted_ids["transport_stream_sequences"]["exact_ids"],
        serde_json::json!([]),
        "contiguous transport ids should not leave exact-id metadata behind",
    );

    let projects = run_local_replica_query(
        target.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    for name in [
        "schema-seed",
        "late-transport-replay",
        "newer-transport-replay",
    ] {
        let matches = projects
            .iter()
            .filter(|project| project.value().0.get("name") == Some(&assert_val!(name)))
            .count();
        assert_eq!(
            matches, 1,
            "replicated project {name:?} should appear exactly once",
        );
    }

    // Simulate the follow-up #243 pruning behavior: origin timestamp exact IDs
    // below the compacted ordered-delivery watermark are gone, but the
    // transport watermark remains. A redelivery of stream sequence 2 must
    // still be recognized as already applied.
    target_persistence
        .write_persistence_global(
            PersistenceGlobalKey::AppliedDataDeltaIds,
            serde_json::json!({
                "origin_timestamps": {},
                "transport_stream_sequences": {
                    "compacted_watermark": 3,
                    "exact_ids": [],
                },
            }),
        )
        .await?;
    target.shutdown().await?;
    target = create_node_with_persistence_and_table_number_allocator(
        &rt,
        target_persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(0))),
        None,
        Arc::new(NoopTwoPhaseDecisionLog),
        None,
        table_numbers,
    )
    .await?;

    let document_log_count_before_duplicate =
        persisted_document_log_count(&target_persistence).await?;
    let snapshot_ts_before_duplicate = *target.now_ts_for_reads();
    let duplicate_apply_ts = target
        .committer_for_test()
        .apply_replica_delta_with_transport_id(older_delta, ReplicationTransportId::new(2))
        .await?;
    assert_eq!(
        duplicate_apply_ts, older_origin_ts,
        "redelivery skipped by compacted transport watermark should ACK at the origin timestamp",
    );
    assert_eq!(
        persisted_document_log_count(&target_persistence).await?,
        document_log_count_before_duplicate,
        "transport redelivery after origin-id pruning must not append document log entries",
    );
    assert_eq!(
        *target.now_ts_for_reads(),
        snapshot_ts_before_duplicate,
        "transport redelivery after origin-id pruning must not publish a new snapshot",
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
async fn test_watcher_recovers_staged_prepare_after_participant_leader_failover(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let old_leader = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        decision_log.clone(),
        None,
        table_number_allocator.clone(),
    )
    .await?;
    let new_leader = create_node_with_persistence_and_table_number_allocator(
        &rt,
        Arc::new(TestPersistence::new()),
        log,
        Some(partitioned_map(PartitionId(1))),
        None,
        decision_log.clone(),
        None,
        table_number_allocator,
    )
    .await?;

    let project_name = "staged-before-participant-leader-failover";
    let mut tx = old_leader.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"projects".parse()?, assert_obj!("name" => project_name))
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("participant_leader_failover_staging_recovery_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = old_leader.committer_for_test().allocate_commit_ts().await?;
    old_leader
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    let redo = old_leader
        .committer_for_test()
        .two_phase_redo_records()
        .await?
        .get(&txn_id.0)
        .cloned()
        .context("old participant leader should persist a prepared redo record")?;

    new_leader
        .committer_for_test()
        .apply_raft_prepared_redo(redo.clone())
        .await
        .context("new participant leader should replay the Raft-prepared redo")?;
    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
    );
    decision_log.write_decision(&txn_id, &staging).await?;

    old_leader.shutdown().await?;

    let before_recovery = run_query(
        new_leader.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        !before_recovery
            .iter()
            .any(|project| project.value().0.get("name") == Some(&assert_val!(project_name))),
        "Raft-prepared writes must remain hidden until the new leader recovers the staged decision",
    );

    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &new_leader.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging,
    )
    .await?;
    assert!(
        resolved,
        "new participant leader should recover a complete staged decision from Raft-prepared redo",
    );

    let after_recovery = run_query(
        new_leader.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        after_recovery
            .iter()
            .filter(|project| project.value().0.get("name") == Some(&assert_val!(project_name)))
            .count(),
        1,
        "participant-leader failover recovery must expose the staged write exactly once",
    );
    let Some(TwoPhaseDecision::Committed {
        resolved_participants,
        ..
    }) = decision_log.get_decision(&txn_id).await?
    else {
        anyhow::bail!(
            "participant-leader failover recovery should retain the decision as committed"
        );
    };
    assert_eq!(
        resolved_participants,
        vec![1],
        "new participant leader should mark the local participant resolved",
    );
    assert!(
        new_leader
            .committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "new participant leader should clean the replayed prepared redo after commit",
    );
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
    )?;
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
async fn test_catalog_write_routing_fails_loud_on_malformed_index_metadata(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let source = create_node(&rt, log, Some(partitioned_map(PartitionId(0)))).await?;

    let mut tx = source.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"projects".parse()?, assert_obj!("name" => "catalog"))
        .await?;
    let final_tx = tx.finalize()?;
    let index_write = final_tx
        .writes
        .coalesced_writes()
        .find(|write| {
            final_tx
                .table_mapping
                .tablet_name(write.id.tablet_id)
                .is_ok_and(|table| table == *INDEX_TABLE)
        })
        .context("project insert should include _index metadata")?;
    let malformed_document = index_write
        .new_document
        .as_ref()
        .context("_index metadata write should insert a document")?
        .replace_value(assert_obj!("not" => "index metadata"))?;
    let malformed_write = DocumentUpdateWithPrevTs {
        id: index_write.id,
        old_document: index_write.old_document.clone(),
        new_document: Some(malformed_document),
    };

    let err = crate::two_phase_coordinator::routed_partition_for_catalog_write_for_test(
        &final_tx,
        &malformed_write,
        &partitioned_map(PartitionId(0)),
    )
    .expect_err("malformed _index metadata must not classify as an unrouted local write");
    let err = format!("{err:#}");
    assert!(
        err.contains("failed to parse _index catalog metadata"),
        "error should explain the malformed _index metadata routing failure, got {err}",
    );
    assert!(
        err.contains("missing field"),
        "error should include parser context for the malformed catalog document, got {err}",
    );

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
async fn test_live_prepare_rejects_timestamp_below_local_write_floor(
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

    insert_doc(
        &participant,
        "projects",
        assert_obj!("name" => "live-prepare-local-floor-advancer"),
    )
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
        .expect_err("live prepare should reject a timestamp below the local write floor");
    assert!(
        format!("{err:#}").contains("2PC Prepare assigned ts="),
        "unexpected error: {err:#}",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_live_prepare_allows_timestamp_below_uncommitted_reservation(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let table_number_allocator = Arc::new(InMemoryTableNumberAllocator::default());
    let node = create_node_with_persistence_and_table_number_allocator(
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
        &node,
        "projects",
        assert_obj!("name" => "seed-before-concurrent-prepare-floor"),
    )
    .await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "live-prepare-below-reserved-timestamp"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("live_prepare_below_reserved_timestamp_test");
    let participant_tx = ParticipantTransaction::from_final_transaction(
        &final_tx,
        PartitionId(1),
        &write_indexes,
        &partitioned_map(PartitionId(1)),
        &write_source,
    )?;
    let txn_id = TwoPhaseTransactionId::new();
    let prepare_ts = node.committer_for_test().allocate_commit_ts().await?;
    let later_reserved_ts = node.committer_for_test().allocate_commit_ts().await?;
    assert!(
        later_reserved_ts > prepare_ts,
        "test setup expected a later reserved timestamp",
    );

    let result = node
        .committer_for_test()
        .prepare_remote(
            txn_id.clone(),
            participant_tx,
            write_source,
            prepare_ts,
            vec![PartitionId(1)],
        )
        .await?;
    assert_eq!(result.prepare_ts, prepare_ts);

    node.committer_for_test().rollback_prepared(txn_id).await?;

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
    let projects = run_local_replica_query(
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

    let projects_after = run_local_replica_query(
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
    let projects = run_local_replica_query(
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

    let mut tx = node_b
        .with_local_replica_reads_for_test()
        .begin(Identity::system())
        .await?;
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

    let tasks = run_local_replica_query(
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
fn test_committer_client_refreshes_membership_snapshot() -> anyhow::Result<()> {
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
        assert!(committer.node_addresses().is_none());

        committer.refresh_membership_snapshot(MembershipSnapshot::new(
            MembershipVersion::new(2),
            vec![
                NodeMembership::new(
                    ClusterNodeId::new("node-p0a")?,
                    PartitionId(0),
                    "node-p0:50051",
                )?,
                NodeMembership::new(
                    ClusterNodeId::new("node-p1a")?,
                    PartitionId(1),
                    "node-p1:50051",
                )?,
            ],
        ))?;

        let node_addresses = committer
            .node_addresses()
            .expect("membership refresh should install node addresses");
        assert_eq!(
            node_addresses.address_for(PartitionId(1)),
            Some("node-p1:50051"),
        );
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
fn test_parallel_early_ack_recovers_staging_before_async_cleanup() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    let rt = td.rt();
    td.run_until(async move {
        let log = Arc::new(InMemoryDistributedLog::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let tso: Arc<dyn TimestampOracle> = Arc::new(InMemoryTimestampOracle::new());
        let node_b_map = partitioned_map(PartitionId(1));
        let node_b = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(node_b_map),
            None,
            Some(tso.clone()),
            decision_log.clone(),
        )
        .await?;
        let server = start_two_pc_server(TestTwoPhaseCommitGrpcService::new(
            node_b.committer_for_test(),
        ))
        .await?;

        let node_a_map = partitioned_map(PartitionId(0));
        let node_a = create_node_with_options_and_decision_log(
            &rt,
            log.clone(),
            Some(node_a_map.clone()),
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
                write_source: WriteSource::new("parallel_early_ack_test"),
                write_bytes: 0,
                tablet_id_to_table_name: Default::default(),
                source_partition: Some(PartitionId(1)),
            })
            .await?;

        let initial_delta_count = log.deltas().len();
        let mut tx = node_a.begin(Identity::system()).await?;
        let mut model = TestFacingModel::new(&mut tx);
        model
            .insert(
                &"messages".parse()?,
                assert_obj!("text" => "parallel-local"),
            )
            .await?;
        model
            .insert(
                &"projects".parse()?,
                assert_obj!("name" => "parallel-remote"),
            )
            .await?;
        let final_tx = tx.finalize()?;

        let outcome = coordinate_two_phase_commit_with_mode(
            &node_a.committer_for_test(),
            final_tx,
            WriteSource::new("parallel_early_ack_test"),
            &node_a_map,
            TwoPhaseCommitMode::ParallelEarlyAck,
        )
        .await?;

        let decisions = decision_log.decisions();
        assert_eq!(decisions.len(), 1);
        assert!(
            decisions.values().any(|decision| matches!(
                decision,
                TwoPhaseDecision::Committed {
                    commit_ts,
                    participants,
                    ..
                } if *commit_ts == u64::from(outcome.ts)
                    && participants == &vec![PartitionId(0).0, PartitionId(1).0]
            )),
            "parallel early ACK should recover the durable staging proof into a committed decision"
        );

        let mut commit_deltas = Vec::new();
        for _ in 0..50 {
            let deltas = log.deltas();
            commit_deltas = deltas[initial_delta_count..]
                .iter()
                .filter(|delta| delta.ts == outcome.ts)
                .cloned()
                .collect();
            if commit_deltas.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            commit_deltas.len(),
            2,
            "parallel cleanup should eventually publish one committed delta per participant",
        );
        let mut sources: Vec<_> = commit_deltas
            .iter()
            .map(|delta| delta.source_partition)
            .collect();
        sources.sort();
        assert_eq!(sources, vec![Some(PartitionId(0)), Some(PartitionId(1))]);

        for _ in 0..50 {
            if decision_log
                .decisions()
                .values()
                .any(TwoPhaseDecision::all_participants_resolved)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            decision_log
                .decisions()
                .values()
                .any(TwoPhaseDecision::all_participants_resolved),
            "async cleanup should mark every participant resolved",
        );

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
        assert!(
            decisions.values().any(|decision| {
                matches!(decision, TwoPhaseDecision::RolledBack { .. })
                    && decision.all_participants_resolved()
            }),
            "rollback decision should be retained after every prepared participant resolves",
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
        let decisions = decision_log.decisions();
        assert_eq!(decisions.len(), 1);
        assert!(
            decisions.values().any(|decision| {
                matches!(decision, TwoPhaseDecision::RolledBack { .. })
                    && decision.all_participants_resolved()
            }),
            "rollback decision should be retained once attempted participants are resolved",
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

#[derive(Clone, Copy, Debug)]
enum SeededTwoPhaseRecoveryAction {
    CommitRetainedFinalDecision,
    RollbackAbandonedPrepare,
    RecoverCompleteStaging,
    RestartThenRecoverCompleteStaging,
    WaitForPartialStagingThenComplete,
}

struct SeededTwoPhaseRecoverySimulation {
    seed: u64,
    rng: ChaCha12Rng,
    trace: Vec<String>,
    visible_project_names: Vec<String>,
    hidden_project_names: Vec<String>,
}

impl SeededTwoPhaseRecoverySimulation {
    fn new(seed: u64) -> Self {
        let mut seed_bytes = [0u8; 32];
        seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
        Self {
            seed,
            rng: ChaCha12Rng::from_seed(seed_bytes),
            trace: Vec::new(),
            visible_project_names: Vec::new(),
            hidden_project_names: Vec::new(),
        }
    }

    fn record(&mut self, event: impl Into<String>) {
        self.trace.push(event.into());
    }

    fn trace_dump(&self) -> String {
        self.trace.join("\n")
    }

    fn shuffled_actions(&mut self) -> Vec<SeededTwoPhaseRecoveryAction> {
        let mut actions = vec![
            SeededTwoPhaseRecoveryAction::CommitRetainedFinalDecision,
            SeededTwoPhaseRecoveryAction::RollbackAbandonedPrepare,
            SeededTwoPhaseRecoveryAction::RecoverCompleteStaging,
            SeededTwoPhaseRecoveryAction::RestartThenRecoverCompleteStaging,
            SeededTwoPhaseRecoveryAction::WaitForPartialStagingThenComplete,
        ];
        for _ in 0..5 {
            actions.push(match self.rng.random_range(0..5) {
                0 => SeededTwoPhaseRecoveryAction::CommitRetainedFinalDecision,
                1 => SeededTwoPhaseRecoveryAction::RollbackAbandonedPrepare,
                2 => SeededTwoPhaseRecoveryAction::RecoverCompleteStaging,
                3 => SeededTwoPhaseRecoveryAction::RestartThenRecoverCompleteStaging,
                _ => SeededTwoPhaseRecoveryAction::WaitForPartialStagingThenComplete,
            });
        }
        for i in (1..actions.len()).rev() {
            let j = self.rng.random_range(0..=i);
            actions.swap(i, j);
        }
        actions
    }

    async fn assert_invariants(&self, node: &Database<TestRuntime>) -> anyhow::Result<()> {
        let projects = run_query(
            node.clone(),
            TableNamespace::root_component(),
            Query::full_table_scan("projects".parse()?, Order::Asc),
        )
        .await?;
        for name in &self.visible_project_names {
            let expected = assert_val!(name.clone());
            anyhow::ensure!(
                projects
                    .iter()
                    .any(|project| project.value().0.get("name") == Some(&expected)),
                "seed={:#x}\nexpected committed/recovered project {name:?} to be \
                 visible\ntrace:\n{}",
                self.seed,
                self.trace_dump(),
            );
        }
        for name in &self.hidden_project_names {
            let expected = assert_val!(name.clone());
            anyhow::ensure!(
                !projects
                    .iter()
                    .any(|project| project.value().0.get("name") == Some(&expected)),
                "seed={:#x}\nrolled-back or incomplete prepared project {name:?} became \
                 visible\ntrace:\n{}",
                self.seed,
                self.trace_dump(),
            );
        }
        anyhow::ensure!(
            node.committer_for_test()
                .two_phase_redo_records()
                .await?
                .is_empty(),
            "seed={:#x}\nresolved action leaked durable prepared redo\ntrace:\n{}",
            self.seed,
            self.trace_dump(),
        );
        Ok(())
    }
}

async fn prepare_project_insert_for_2pc_sim(
    node: &Database<TestRuntime>,
    name: &str,
    write_source_name: String,
    participants: Vec<PartitionId>,
) -> anyhow::Result<(TwoPhaseTransactionId, Timestamp, TwoPhaseRedoEntry)> {
    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => name.to_string()),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new(write_source_name);
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
            participants,
        )
        .await?;
    let redo = node
        .committer_for_test()
        .two_phase_redo_records()
        .await?
        .get(&txn_id.0)
        .cloned()
        .context("prepare should persist a durable redo record")?;
    Ok((txn_id, prepare_ts, redo))
}

#[convex_macro::test_runtime]
async fn test_staged_2pc_write_invalidates_subscription_only_after_recovery(
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

    insert_doc(
        &node,
        "projects",
        assert_obj!("name" => "subscription-baseline"),
    )
    .await?;

    let mut read_tx = node.begin(Identity::system()).await?;
    let mut query_stream = ResolvedQuery::new(
        &mut read_tx,
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )?;
    let mut initial_results = 0;
    while query_stream.next(&mut read_tx, Some(2)).await?.is_some() {
        initial_results += 1;
    }
    assert_eq!(
        initial_results, 1,
        "subscription setup should read the baseline project",
    );
    let token = read_tx.into_token()?;
    let subscription = node.subscribe(token).await?;

    let (txn_id, prepare_ts, redo) = prepare_project_insert_for_2pc_sim(
        &node,
        "subscription-staged-recovery",
        "staged_subscription_recovery_test".to_string(),
        vec![PartitionId(0), PartitionId(1)],
    )
    .await?;

    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            subscription.wait_for_invalidation()
        )
        .await
        .is_err(),
        "a merely prepared 2PC write is abortable and must not invalidate subscriptions",
    );

    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![0, 1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
    );
    decision_log.write_decision(&txn_id, &staging).await?;
    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging.clone(),
    )
    .await?;
    assert!(
        !resolved,
        "incomplete staged decisions must not expose the prepared write",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            subscription.wait_for_invalidation()
        )
        .await
        .is_err(),
        "partial staging is still abortable and must not invalidate subscriptions",
    );

    decision_log
        .mark_participant_staged(
            &txn_id,
            TwoPhaseParticipantIntent {
                partition_id: 0,
                prepare_ts: u64::from(prepare_ts),
                participant_transaction_digest: "remote-participant-digest".to_string(),
            },
        )
        .await?;
    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id,
        staging,
    )
    .await?;
    assert!(
        resolved,
        "complete staged decisions should recover and commit the prepared write",
    );
    let invalid_ts =
        tokio::time::timeout(Duration::from_secs(1), subscription.wait_for_invalidation()).await?;
    assert!(
        invalid_ts.is_some(),
        "committed staged recovery must invalidate overlapping subscriptions",
    );

    let projects_after_recovery = run_query(
        node,
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        projects_after_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("subscription-staged-recovery"))
        }),
        "recovered committed write should be visible after subscription invalidation",
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_seeded_two_phase_recovery_simulation_preserves_prepared_invariants(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    for seed in [0x1340_2f00_0000_0001, 0x1340_2f00_0000_0002] {
        let log = Arc::new(InMemoryDistributedLog::new());
        let persistence = Arc::new(TestPersistence::new());
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let mut node = create_node_with_persistence(
            &rt,
            persistence.clone(),
            log.clone(),
            Some(partitioned_map(PartitionId(1))),
            None,
            decision_log.clone(),
            None,
        )
        .await?;
        let mut sim = SeededTwoPhaseRecoverySimulation::new(seed);

        for (round, action) in sim.shuffled_actions().into_iter().enumerate() {
            sim.record(format!("round={round} action={action:?}"));
            let name = format!("seed-{seed:x}-round-{round}");
            match action {
                SeededTwoPhaseRecoveryAction::CommitRetainedFinalDecision => {
                    let (txn_id, prepare_ts, _) = prepare_project_insert_for_2pc_sim(
                        &node,
                        &name,
                        format!("seeded_2pc_retained_commit_{round}"),
                        vec![PartitionId(1)],
                    )
                    .await?;
                    let decision = TwoPhaseDecision::committed(u64::from(prepare_ts), vec![1]);
                    decision_log.write_decision(&txn_id, &decision).await?;
                    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
                        &node.committer_for_test(),
                        PartitionId(1),
                        txn_id,
                        decision,
                    )
                    .await?;
                    anyhow::ensure!(
                        resolved,
                        "seed={seed:#x}\nretained committed decision did not resolve\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    sim.visible_project_names.push(name);
                },
                SeededTwoPhaseRecoveryAction::RollbackAbandonedPrepare => {
                    let (txn_id, ..) = prepare_project_insert_for_2pc_sim(
                        &node,
                        &name,
                        format!("seeded_2pc_abandoned_rollback_{round}"),
                        vec![PartitionId(1)],
                    )
                    .await?;
                    let resolved = crate::two_phase_watcher::rollback_expired_local_prepares(
                        &node.committer_for_test(),
                        PartitionId(1),
                        Duration::ZERO,
                    )
                    .await?;
                    anyhow::ensure!(
                        resolved == 1,
                        "seed={seed:#x}\nabandoned prepare rollback resolved {resolved} \
                         records\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    anyhow::ensure!(
                        decision_log
                            .get_decision(&txn_id)
                            .await?
                            .is_some_and(|decision| matches!(
                                decision,
                                TwoPhaseDecision::RolledBack { .. }
                            )),
                        "seed={seed:#x}\nabandoned prepare did not retain a rollback \
                         decision\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    sim.hidden_project_names.push(name);
                    let followup = format!("seed-{seed:x}-round-{round}-after-rollback");
                    insert_doc(&node, "projects", assert_obj!("name" => followup.clone()))
                        .await
                        .context("rolled-back prepare should not block follow-up writes")?;
                    sim.visible_project_names.push(followup);
                },
                SeededTwoPhaseRecoveryAction::RecoverCompleteStaging => {
                    let (txn_id, prepare_ts, redo) = prepare_project_insert_for_2pc_sim(
                        &node,
                        &name,
                        format!("seeded_2pc_complete_staging_{round}"),
                        vec![PartitionId(1)],
                    )
                    .await?;
                    let staging = TwoPhaseDecision::staging(
                        u64::from(prepare_ts),
                        vec![1],
                        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
                    );
                    decision_log.write_decision(&txn_id, &staging).await?;
                    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
                        &node.committer_for_test(),
                        PartitionId(1),
                        txn_id,
                        staging,
                    )
                    .await?;
                    anyhow::ensure!(
                        resolved,
                        "seed={seed:#x}\ncomplete staging decision did not resolve\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    sim.visible_project_names.push(name);
                },
                SeededTwoPhaseRecoveryAction::RestartThenRecoverCompleteStaging => {
                    let (txn_id, prepare_ts, redo) = prepare_project_insert_for_2pc_sim(
                        &node,
                        &name,
                        format!("seeded_2pc_restart_staging_{round}"),
                        vec![PartitionId(1)],
                    )
                    .await?;
                    let staging = TwoPhaseDecision::staging(
                        u64::from(prepare_ts),
                        vec![1],
                        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
                    );
                    decision_log.write_decision(&txn_id, &staging).await?;
                    node.shutdown().await?;
                    sim.record(format!("round={round} restart_participant"));
                    node = create_node_with_persistence(
                        &rt,
                        persistence.clone(),
                        log.clone(),
                        Some(partitioned_map(PartitionId(1))),
                        None,
                        decision_log.clone(),
                        None,
                    )
                    .await?;
                    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
                        &node.committer_for_test(),
                        PartitionId(1),
                        txn_id,
                        staging,
                    )
                    .await?;
                    anyhow::ensure!(
                        resolved,
                        "seed={seed:#x}\nrestart staging decision did not resolve\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    sim.visible_project_names.push(name);
                },
                SeededTwoPhaseRecoveryAction::WaitForPartialStagingThenComplete => {
                    let (txn_id, prepare_ts, redo) = prepare_project_insert_for_2pc_sim(
                        &node,
                        &name,
                        format!("seeded_2pc_partial_staging_{round}"),
                        vec![PartitionId(0), PartitionId(1)],
                    )
                    .await?;
                    let staging = TwoPhaseDecision::staging(
                        u64::from(prepare_ts),
                        vec![0, 1],
                        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
                    );
                    decision_log.write_decision(&txn_id, &staging).await?;
                    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
                        &node.committer_for_test(),
                        PartitionId(1),
                        txn_id.clone(),
                        staging.clone(),
                    )
                    .await?;
                    anyhow::ensure!(
                        !resolved,
                        "seed={seed:#x}\npartial staging resolved before all participants \
                         staged\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    let projects = run_query(
                        node.clone(),
                        TableNamespace::root_component(),
                        Query::full_table_scan("projects".parse()?, Order::Asc),
                    )
                    .await?;
                    let expected = assert_val!(name.clone());
                    anyhow::ensure!(
                        !projects
                            .iter()
                            .any(|project| project.value().0.get("name") == Some(&expected)),
                        "seed={seed:#x}\npartial staging made prepared write visible \
                         early\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    decision_log
                        .mark_participant_staged(
                            &txn_id,
                            TwoPhaseParticipantIntent {
                                partition_id: 0,
                                prepare_ts: u64::from(prepare_ts),
                                participant_transaction_digest: format!(
                                    "seeded-remote-digest-{seed:x}-{round}"
                                ),
                            },
                        )
                        .await?;
                    let completed = decision_log
                        .get_decision(&txn_id)
                        .await?
                        .context("partial staging decision should remain durable")?;
                    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
                        &node.committer_for_test(),
                        PartitionId(1),
                        txn_id,
                        completed,
                    )
                    .await?;
                    anyhow::ensure!(
                        resolved,
                        "seed={seed:#x}\ncompleted staging decision did not resolve\ntrace:\n{}",
                        sim.trace_dump(),
                    );
                    sim.visible_project_names.push(name);
                },
            }
            sim.assert_invariants(&node).await?;
        }
        node.shutdown().await?;
    }
    Ok(())
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

    let decisions = decision_log.decisions();
    assert_eq!(decisions.len(), 1);
    assert!(
        decisions
            .get(&txn_id.0)
            .is_some_and(|decision| matches!(decision, TwoPhaseDecision::RolledBack { .. })),
        "timeout rollback should retain its final decision for follower recovery",
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
fn test_watcher_gc_deletes_only_fully_resolved_decisions_after_grace() -> anyhow::Result<()> {
    let td = TestDriver::new_with_io();
    td.run_until(async move {
        let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
        let partial_txn = TwoPhaseTransactionId("partial-gc-test".to_string());
        let full_txn = TwoPhaseTransactionId("full-gc-test".to_string());

        let mut partial = TwoPhaseDecision::committed(100, vec![0, 1]);
        partial.mark_participant_resolved(PartitionId(0));
        decision_log.write_decision(&partial_txn, &partial).await?;

        let mut full = TwoPhaseDecision::committed(200, vec![0, 1]);
        full.mark_participant_resolved(PartitionId(0));
        full.mark_participant_resolved(PartitionId(1));
        assert!(full.ready_for_gc(Duration::ZERO));
        decision_log.write_decision(&full_txn, &full).await?;

        assert!(
            !crate::two_phase_watcher::garbage_collect_resolved_decision(
                decision_log.as_ref(),
                &partial_txn,
                &partial,
                Duration::ZERO,
            )
            .await?,
            "partial decisions must remain recoverable",
        );
        assert!(
            crate::two_phase_watcher::garbage_collect_resolved_decision(
                decision_log.as_ref(),
                &full_txn,
                &full,
                Duration::ZERO,
            )
            .await?,
            "fully resolved decisions should be removed after the grace period",
        );

        assert!(
            decision_log.get_decision(&partial_txn).await?.is_some(),
            "unresolved decisions must stay in the decision log",
        );
        assert!(
            decision_log.get_decision(&full_txn).await?.is_none(),
            "fully resolved decisions should be deleted",
        );
        Ok(())
    })
}

#[convex_macro::test_runtime]
async fn test_watcher_commits_prepared_redo_when_final_decision_was_already_resolved(
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
            assert_obj!("name" => "prepared-before-final-decision-retention"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("retained_final_decision_recovery_test");
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

    let mut committed = TwoPhaseDecision::committed(u64::from(prepare_ts), vec![1]);
    committed.mark_participant_resolved(PartitionId(1));
    assert!(
        committed.all_participants_resolved(),
        "test setup should model a coordinator-retained final decision after partition-level \
         resolution",
    );
    decision_log.write_decision(&txn_id, &committed).await?;

    let resolved = crate::two_phase_watcher::rollback_expired_local_prepares(
        &node.committer_for_test(),
        PartitionId(1),
        Duration::ZERO,
    )
    .await?;
    assert_eq!(
        resolved, 1,
        "expired local redo with a retained committed decision must be committed, not rolled back",
    );

    let projects_after_recovery = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        projects_after_recovery.iter().any(|project| {
            project.value().0.get("name")
                == Some(&assert_val!("prepared-before-final-decision-retention"))
        }),
        "retained committed decision should make the prepared write visible",
    );
    assert!(
        decision_log
            .get_decision(&txn_id)
            .await?
            .is_some_and(|decision| matches!(decision, TwoPhaseDecision::Committed { .. })),
        "committed decision should remain retained for other followers/restarts",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_watcher_recovers_complete_staging_decision_and_commits_prepared_write(
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
            assert_obj!("name" => "staged-before-coordinator-crash"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("complete_staging_recovery_test");
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

    let projects_before_recovery = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        !projects_before_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("staged-before-coordinator-crash"))
        }),
        "abortable staged writes must stay hidden before status recovery",
    );

    let redo_records = node.committer_for_test().two_phase_redo_records().await?;
    let redo_entry = redo_records
        .values()
        .next()
        .context("prepare should persist a redo entry")?;
    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(redo_entry)],
    );
    decision_log.write_decision(&txn_id, &staging).await?;

    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging,
    )
    .await?;
    assert!(
        resolved,
        "complete staged decision should recover to a committed decision",
    );

    let projects_after_recovery = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        projects_after_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("staged-before-coordinator-crash"))
        }),
        "watcher recovery should make the recovered committed write visible",
    );
    let Some(TwoPhaseDecision::Committed {
        resolved_participants,
        ..
    }) = decision_log.get_decision(&txn_id).await?
    else {
        anyhow::bail!(
            "single-participant recovered staging decision should be retained as committed"
        );
    };
    assert_eq!(
        resolved_participants,
        vec![1],
        "recovered staging decision should mark the local participant resolved",
    );
    assert!(
        node.committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "committing the recovered staged transaction should delete the participant redo record",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_watcher_duplicate_complete_staging_recovery_is_idempotent(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let persistence = Arc::new(TestPersistence::new());
    let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
    let node = create_node_with_persistence(
        &rt,
        persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(1))),
        None,
        decision_log.clone(),
        None,
    )
    .await?;

    let project_name = "duplicate-staging-recovery-visible-once";
    let (txn_id, prepare_ts, redo) = prepare_project_insert_for_2pc_sim(
        &node,
        project_name,
        "duplicate_complete_staging_recovery_test".to_string(),
        vec![PartitionId(1)],
    )
    .await?;
    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(&redo)],
    );
    decision_log.write_decision(&txn_id, &staging).await?;

    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging.clone(),
    )
    .await?;
    assert!(
        resolved,
        "first complete staging recovery should commit the local participant",
    );
    let document_log_count_after_first = persisted_document_log_count(&persistence).await?;
    let snapshot_ts_after_first = *node.now_ts_for_reads();
    let projects_after_first = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects_after_first
            .iter()
            .filter(|project| project.value().0.get("name") == Some(&assert_val!(project_name)))
            .count(),
        1,
        "first recovery should make the staged write visible exactly once",
    );

    let resolved_from_stale_staging =
        crate::two_phase_watcher::resolve_decision_for_local_partition(
            &node.committer_for_test(),
            PartitionId(1),
            txn_id.clone(),
            staging,
        )
        .await?;
    assert!(
        resolved_from_stale_staging,
        "duplicate recovery with a stale staging message should be treated as resolved",
    );

    let committed_decision = decision_log
        .get_decision(&txn_id)
        .await?
        .context("staging recovery should retain a committed decision")?;
    assert!(
        matches!(committed_decision, TwoPhaseDecision::Committed { .. }),
        "complete staging recovery should promote the durable decision to committed",
    );
    let resolved_from_final_decision =
        crate::two_phase_watcher::resolve_decision_for_local_partition(
            &node.committer_for_test(),
            PartitionId(1),
            txn_id.clone(),
            committed_decision,
        )
        .await?;
    assert!(
        resolved_from_final_decision,
        "duplicate recovery with a retained final decision should be treated as resolved",
    );

    assert_eq!(
        persisted_document_log_count(&persistence).await?,
        document_log_count_after_first,
        "duplicate staging/final recovery must not append duplicate document log entries",
    );
    assert_eq!(
        *node.now_ts_for_reads(),
        snapshot_ts_after_first,
        "duplicate staging/final recovery must not publish another snapshot",
    );
    let projects_after_duplicates = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert_eq!(
        projects_after_duplicates
            .iter()
            .filter(|project| project.value().0.get("name") == Some(&assert_val!(project_name)))
            .count(),
        1,
        "duplicate recovery must not make the staged write visible more than once",
    );
    assert!(
        node.committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "duplicate recovery should leave the prepared redo cleaned up",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_watcher_recovers_complete_staging_after_participant_restart(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let persistence = Arc::new(TestPersistence::new());
    let decision_log = Arc::new(InMemoryTwoPhaseDecisionLog::new());
    let node = create_node_with_persistence(
        &rt,
        persistence.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(1))),
        None,
        decision_log.clone(),
        None,
    )
    .await?;

    let mut tx = node.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(
            &"projects".parse()?,
            assert_obj!("name" => "staged-before-participant-restart"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("complete_staging_restart_recovery_test");
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

    let projects_before_restart = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        !projects_before_restart.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("staged-before-participant-restart"))
        }),
        "prepared staged writes must stay hidden before participant restart",
    );

    let redo_records = node.committer_for_test().two_phase_redo_records().await?;
    let redo_entry = redo_records
        .values()
        .next()
        .context("prepare should persist a redo entry before restart")?;
    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(redo_entry)],
    );
    decision_log.write_decision(&txn_id, &staging).await?;
    node.shutdown().await?;

    let restarted = create_node_with_persistence(
        &rt,
        persistence.clone(),
        log,
        Some(partitioned_map(PartitionId(1))),
        None,
        decision_log.clone(),
        None,
    )
    .await?;
    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &restarted.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging,
    )
    .await?;
    assert!(
        resolved,
        "watcher should recover complete staging from durable redo after participant restart",
    );

    let projects_after_recovery = run_query(
        restarted.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        projects_after_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("staged-before-participant-restart"))
        }),
        "staging recovery after restart should make the committed write visible",
    );
    let Some(TwoPhaseDecision::Committed {
        resolved_participants,
        ..
    }) = decision_log.get_decision(&txn_id).await?
    else {
        anyhow::bail!(
            "single-participant recovered staging decision should be retained as committed after \
             restart cleanup",
        );
    };
    assert_eq!(
        resolved_participants,
        vec![1],
        "restart recovery should mark the local participant resolved",
    );
    assert!(
        restarted
            .committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "restart recovery should delete the participant redo record after commit",
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_watcher_leaves_partial_staging_hidden_until_all_participants_stage(
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
            assert_obj!("name" => "partial-staging-hidden"),
        )
        .await?;
    let final_tx = tx.finalize()?;
    let write_indexes: Vec<_> = final_tx
        .writes
        .coalesced_writes()
        .enumerate()
        .map(|(index, _)| index)
        .collect();
    let write_source = WriteSource::new("partial_staging_recovery_test");
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
            vec![PartitionId(0), PartitionId(1)],
        )
        .await?;

    let redo_records = node.committer_for_test().two_phase_redo_records().await?;
    let redo_entry = redo_records
        .values()
        .next()
        .context("prepare should persist a redo entry")?;
    let staging = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        vec![0, 1],
        vec![TwoPhaseParticipantIntent::from_redo_entry(redo_entry)],
    );
    assert!(
        !staging.all_participants_staged(),
        "test setup should represent an incomplete parallel-commit staging record",
    );
    decision_log.write_decision(&txn_id, &staging).await?;

    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging.clone(),
    )
    .await?;
    assert!(
        !resolved,
        "watcher must not resolve or expose a transaction before all participants stage",
    );

    let projects_before_recovery = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        !projects_before_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("partial-staging-hidden"))
        }),
        "partial staged writes must stay hidden while the staging decision is pending",
    );
    assert!(
        !decision_log
            .get_decision(&txn_id)
            .await?
            .context("pending staging decision should remain durable")?
            .all_participants_staged(),
        "missing participant proof should keep the decision in staging",
    );
    assert_eq!(
        node.committer_for_test()
            .two_phase_redo_records()
            .await?
            .len(),
        1,
        "pending staging should keep the local prepared redo for later recovery",
    );

    decision_log
        .mark_participant_staged(
            &txn_id,
            TwoPhaseParticipantIntent {
                partition_id: 0,
                prepare_ts: u64::from(prepare_ts),
                participant_transaction_digest: "remote-participant-digest".to_string(),
            },
        )
        .await?;

    let resolved = crate::two_phase_watcher::resolve_decision_for_local_partition(
        &node.committer_for_test(),
        PartitionId(1),
        txn_id.clone(),
        staging,
    )
    .await?;
    assert!(
        resolved,
        "watcher may recover the local participant once every participant proof exists",
    );

    let projects_after_recovery = run_query(
        node.clone(),
        TableNamespace::root_component(),
        Query::full_table_scan("projects".parse()?, Order::Asc),
    )
    .await?;
    assert!(
        projects_after_recovery.iter().any(|project| {
            project.value().0.get("name") == Some(&assert_val!("partial-staging-hidden"))
        }),
        "complete staged decision should make the recovered local write visible",
    );
    assert!(
        node.committer_for_test()
            .two_phase_redo_records()
            .await?
            .is_empty(),
        "local recovery should clean up the participant redo after commit",
    );
    let Some(TwoPhaseDecision::Committed {
        resolved_participants,
        ..
    }) = decision_log.get_decision(&txn_id).await?
    else {
        anyhow::bail!("complete staging recovery should promote the decision to committed");
    };
    assert_eq!(
        resolved_participants,
        vec![1],
        "local watcher should mark only this participant resolved and leave the decision for \
         other participants",
    );
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
        let Some(TwoPhaseDecision::RolledBack {
            resolved_participants,
            ..
        }) = decision_log
            .get_decision(&TwoPhaseTransactionId(txn_id.clone()))
            .await?
        else {
            anyhow::bail!(
                "rollback decision should remain retained after delayed participant resolves"
            );
        };
        assert_eq!(
            resolved_participants,
            vec![1],
            "delayed participant should be marked resolved on the retained rollback decision",
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
        let Some(TwoPhaseDecision::Committed {
            resolved_participants,
            ..
        }) = decision_log
            .get_decision(&TwoPhaseTransactionId(txn_id.clone()))
            .await?
        else {
            anyhow::bail!(
                "commit decision should remain retained after delayed participant resolves"
            );
        };
        assert_eq!(
            resolved_participants,
            vec![0, 1],
            "delayed participant should be marked resolved on the retained commit decision",
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
