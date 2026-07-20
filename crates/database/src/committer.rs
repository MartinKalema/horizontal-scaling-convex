use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
        VecDeque,
    },
    future::Future,
    ops::Bound,
    sync::{
        atomic::{
            AtomicBool,
            Ordering,
        },
        Arc,
    },
    time::Duration,
};

use ::metrics::{
    StatusTimer,
    Subgauge,
    Timer,
};
use anyhow::Context as _;
use common::{
    backoff::Backoff,
    bootstrap_model::tables::{
        TableMetadata,
        TableState,
        TABLES_TABLE,
    },
    components::{
        ComponentId,
        ComponentPath,
    },
    document::{
        DocumentUpdateWithPrevTs,
        PackedDocument,
        ParseDocument,
        ParsedDocument,
        ResolvedDocument,
        ID_FIELD,
    },
    document_index_keys::DocumentIndexKeyValue,
    errors::{
        recapture_stacktrace,
        report_error,
        DatabaseOperationalError,
        DatabaseTimeoutError,
    },
    fastrace_helpers::{
        initialize_root_from_parent,
        EncodedSpan,
    },
    grpc::ClusterGrpcAuth,
    knobs::{
        COMMITTER_QUEUE_SIZE,
        COMMIT_TRACE_THRESHOLD,
        MAX_REPEATABLE_TIMESTAMP_COMMIT_DELAY,
        MAX_REPEATABLE_TIMESTAMP_IDLE_FREQUENCY,
        REMOTE_READ_FRONTIER_HEARTBEAT_INTERVAL,
        REMOTE_READ_FRONTIER_WAIT_TIMEOUT,
        TRANSACTION_WARN_READ_SET_INTERVALS,
    },
    persistence::{
        ConflictStrategy,
        DocumentLogEntry,
        NoopRetentionValidator,
        Persistence,
        PersistenceGlobalKey,
        PersistenceGlobalWrite,
        PersistenceIndexEntry,
        PersistenceReader,
        RepeatablePersistence,
        RetentionValidator,
        TimestampRange,
    },
    query::Order,
    runtime::{
        block_in_place,
        tokio_spawn,
        Runtime,
        SpawnHandle,
    },
    shutdown::ShutdownSignal,
    sync::split_rw_lock::{
        Reader,
        Writer,
    },
    types::{
        DatabaseIndexUpdate,
        DatabaseIndexValue,
        RepeatableReason,
        RepeatableTimestamp,
        Timestamp,
        WriteTimestamp,
    },
    virtual_system_mapping::VirtualSystemMapping,
};
use errors::ErrorMetadata;
use fastrace::prelude::*;
use futures::{
    future::{
        join_all,
        BoxFuture,
        Either,
    },
    select_biased,
    stream::FuturesOrdered,
    FutureExt,
    StreamExt,
    TryStreamExt,
};
use indexing::index_registry::IndexRegistry;
use itertools::Itertools;
use parking_lot::{
    Mutex,
    RwLock,
};
use prometheus::VMHistogram;
use rand::Rng;
use search::{
    query::tokenize,
    TextIndexWriteSize,
};
use serde::{
    Deserialize,
    Serialize,
};
use tokio::sync::{
    mpsc::{
        self,
        error::TrySendError,
    },
    oneshot,
};
use tokio_util::task::AbortOnDropHandle;
use usage_tracking::FunctionUsageTracker;
use value::{
    heap_size::WithHeapSize,
    id_v6::PublicDocumentId,
    InternalDocumentId,
    ResolvedDocumentId,
    TableMapping,
    TableName,
};
use vector::VectorIndexWriteSize;

use crate::{
    bootstrap_model::defaults::BootstrapTableIds,
    checkpoint::{
        create_checkpoint,
        CheckpointData,
        CheckpointPersistence,
    },
    commit_delta::{
        CommitDelta,
        DistributedLog,
        ReplicationTransportId,
        RetryableReplicaApplyError,
    },
    database::{
        ConflictingReadWithWriteSource,
        DatabaseSnapshot,
    },
    metrics::{
        self,
        bootstrap_update_timer,
        finish_bootstrap_update,
        next_commit_ts_seconds,
        table_summary_finish_bootstrap_timer,
        user_documents_size_subgauge,
    },
    nats_distributed_log::DeltaEnvelope,
    raft_node::RaftProposalResult,
    raft_snapshot::{
        encode_snapshot as encode_raft_snapshot,
        load_state_machine_index,
        state_machine_index_persistence_write,
        RaftSnapshotTemporarilyUnavailable,
        RAFT_STATE_MACHINE_INDEX_KEY,
    },
    raft_state_machine::RaftStateMachineEntry,
    reads::ReadSet,
    search_index_bootstrap::{
        stream_revision_pairs_for_indexes,
        BootstrappedSearchIndexes,
    },
    snapshot_manager::{
        partition_timestamp_map_from_json,
        partition_timestamp_map_to_json,
        replication_frontiers_to_json,
        SnapshotManager,
        TableSummaries,
    },
    table_summary::{
        self,
        TableSummarySnapshot,
    },
    transaction::FinalTransaction,
    write_log::{
        index_keys_from_full_documents,
        LogWriter,
        PackedDocumentUpdate,
        PendingWriteHandle,
        PendingWrites,
        PreparedWriteHandle,
        PreparedWrites,
        WriteSource,
    },
    ComponentRegistry,
    Snapshot,
    Transaction,
    TransactionReadSet,
};

const INITIAL_PERSISTENCE_WRITES_BACKOFF: Duration = Duration::from_secs(1);
const MAX_PERSISTENCE_WRITES_BACKOFF: Duration = Duration::from_secs(60);
const RAFT_NATS_OUTBOX_REPLAY_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const RAFT_NATS_OUTBOX_KEY_PREFIX: &str = "raft_nats_outbox/";
pub(crate) const RAFT_APPLY_MARKER_KEY_PREFIX: &str = "raft_apply_marker/";
const MAX_APPLIED_DATA_ORIGIN_TIMESTAMPS_PER_PARTITION: usize = 1024;
const MAX_CLOSED_READ_TIMESTAMPS: usize = 4096;

type LegacyRaftNatsOutboxRecords = BTreeMap<String, serde_json::Value>;

/// Stable identity journaled in the same persistence transaction as a
/// Raft-applied Convex write. The raw marker bridges the crash window until
/// the identity is consolidated into `AppliedDataDeltaIds`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RaftApplyMarker {
    source_partition: crate::partition::PartitionId,
    origin_ts: Timestamp,
}

#[derive(Deserialize, Serialize)]
struct RaftApplyMarkerValue {
    source_partition: u32,
    origin_ts: u64,
}

impl RaftApplyMarker {
    fn from_delta(delta: &CommitDelta) -> anyhow::Result<Self> {
        let source_partition = delta.source_partition.with_context(|| {
            format!(
                "Raft apply delta at ts={} did not include a source partition",
                u64::from(delta.ts),
            )
        })?;
        Ok(Self {
            source_partition,
            origin_ts: delta.ts,
        })
    }

    fn key(self) -> String {
        format!(
            "{RAFT_APPLY_MARKER_KEY_PREFIX}{:020}/{:020}",
            self.source_partition.0,
            u64::from(self.origin_ts),
        )
    }

    fn persistence_write(self) -> anyhow::Result<PersistenceGlobalWrite> {
        Ok(PersistenceGlobalWrite::new(
            self.key(),
            serde_json::to_value(RaftApplyMarkerValue {
                source_partition: self.source_partition.0,
                origin_ts: u64::from(self.origin_ts),
            })?,
        ))
    }

    fn from_record(key: &str, value: serde_json::Value) -> anyhow::Result<Self> {
        let suffix = key
            .strip_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
            .with_context(|| format!("Invalid Raft apply marker key {key:?}"))?;
        let (partition, origin_ts) = suffix
            .split_once('/')
            .with_context(|| format!("Invalid Raft apply marker key {key:?}"))?;
        let marker = Self {
            source_partition: crate::partition::PartitionId(
                partition
                    .parse::<u32>()
                    .with_context(|| format!("Invalid Raft apply marker partition in {key:?}"))?,
            ),
            origin_ts: Timestamp::try_from(
                origin_ts
                    .parse::<u64>()
                    .with_context(|| format!("Invalid Raft apply marker timestamp in {key:?}"))?,
            )?,
        };
        let persisted: RaftApplyMarkerValue = serde_json::from_value(value)
            .with_context(|| format!("Invalid Raft apply marker value for {key:?}"))?;
        anyhow::ensure!(
            persisted.source_partition == marker.source_partition.0
                && persisted.origin_ts == u64::from(marker.origin_ts),
            "Raft apply marker key/value identity mismatch for {key:?}",
        );
        Ok(marker)
    }
}

pub(crate) async fn load_raft_apply_markers(
    reader: Arc<dyn PersistenceReader>,
) -> anyhow::Result<BTreeSet<RaftApplyMarker>> {
    reader
        .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
        .await?
        .into_iter()
        .map(|(key, value)| RaftApplyMarker::from_record(&key, value))
        .collect()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OrderedDedupeState {
    compacted_watermark: u64,
    exact_ids: BTreeSet<u64>,
}

impl OrderedDedupeState {
    fn contains(&self, id: u64) -> bool {
        id <= self.compacted_watermark || self.exact_ids.contains(&id)
    }

    fn insert(&mut self, id: u64) {
        if id <= self.compacted_watermark {
            return;
        }
        self.exact_ids.insert(id);
        while self
            .exact_ids
            .remove(&self.compacted_watermark.saturating_add(1))
        {
            self.compacted_watermark = self.compacted_watermark.saturating_add(1);
        }
    }

    fn merge(&mut self, proposed: OrderedDedupeState) {
        if proposed.compacted_watermark > self.compacted_watermark {
            self.exact_ids
                .retain(|id| *id > proposed.compacted_watermark);
            self.compacted_watermark = proposed.compacted_watermark;
        }
        for id in proposed.exact_ids {
            self.insert(id);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AppliedDataDeltaIds {
    origin_timestamps: BTreeMap<crate::partition::PartitionId, BTreeSet<Timestamp>>,
    transport_stream_sequences: OrderedDedupeState,
}

impl AppliedDataDeltaIds {
    fn contains_origin_timestamp(
        &self,
        source_partition: crate::partition::PartitionId,
        ts: Timestamp,
    ) -> bool {
        self.origin_timestamps
            .get(&source_partition)
            .is_some_and(|timestamps| timestamps.contains(&ts))
    }

    fn insert_origin_timestamp(
        &mut self,
        source_partition: crate::partition::PartitionId,
        ts: Timestamp,
    ) {
        self.origin_timestamps
            .entry(source_partition)
            .or_default()
            .insert(ts);
    }

    fn contains_transport_id(&self, transport_id: ReplicationTransportId) -> bool {
        self.transport_stream_sequences
            .contains(transport_id.stream_sequence())
    }

    fn insert_transport_id(&mut self, transport_id: ReplicationTransportId) {
        self.transport_stream_sequences
            .insert(transport_id.stream_sequence());
    }

    fn prune_origin_timestamps_below_watermarks(
        &mut self,
        watermarks: &BTreeMap<crate::partition::PartitionId, Timestamp>,
    ) {
        self.origin_timestamps.retain(|partition, timestamps| {
            let Some(watermark) = watermarks.get(partition).copied() else {
                return !timestamps.is_empty();
            };
            while timestamps.len() > MAX_APPLIED_DATA_ORIGIN_TIMESTAMPS_PER_PARTITION {
                let Some(oldest) = timestamps.first().copied() else {
                    break;
                };
                if oldest > watermark {
                    break;
                }
                timestamps.remove(&oldest);
            }
            !timestamps.is_empty()
        });
    }
}

#[derive(Debug)]
struct RaftNatsOutboxRecord {
    key: String,
    ts: Timestamp,
    value: serde_json::Value,
    storage: RaftNatsOutboxRecordStorage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RaftNatsOutboxRecordStorage {
    LegacyBlob,
    PerEntry,
}

/// State held for a 2PC-prepared transaction awaiting commit/rollback.
struct PreparedTransaction {
    prepared_write: PreparedWriteHandle,
    origin_commit_ts: Timestamp,
    commit_ts: Timestamp,
    write_bytes: u64,
    document_writes: Arc<Vec<DocumentLogEntry>>,
    index_writes: Arc<Vec<PersistenceIndexEntry>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    pub ts: Timestamp,
    pub source_partition: Option<crate::partition::PartitionId>,
    pub read_after_write_partitions: Vec<crate::partition::PartitionId>,
    pub invalidated_tables: BTreeSet<TableName>,
}

/// Result of asking one partition leader to make an exact timestamp readable.
///
/// A cluster read barrier is complete only when every partition returns
/// [`Closed`](Self::Closed) for the same timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadTimestampClosure {
    Closed(Timestamp),
    /// This node's translated local timeline is already ahead of the requested
    /// timestamp. The coordinator must choose a newer common timestamp.
    RetryAt(Timestamp),
    /// A prepared transaction at or below the requested timestamp is
    /// unresolved. Retrying the same barrier after the decision resolves is
    /// safe.
    Blocked(Timestamp),
}

enum RepeatableTimestampResult {
    Background(oneshot::Sender<Timestamp>),
    ClusterReadBarrier(oneshot::Sender<anyhow::Result<ReadTimestampClosure>>),
}

enum PersistenceWrite {
    Commit {
        pending_write: PendingWriteHandle,
        commit_timer: StatusTimer,
        result: oneshot::Sender<anyhow::Result<CommitOutcome>>,
        parent_trace: EncodedSpan,
        commit_id: usize,
        delta: CommitDelta,
        raft_applied_index: Option<u64>,
        raft_apply_marker: Option<RaftApplyMarker>,
    },
    RejectedBeforePersistence {
        pending_write: PendingWriteHandle,
        commit_timer: StatusTimer,
        result: oneshot::Sender<anyhow::Result<CommitOutcome>>,
        commit_id: usize,
        err: anyhow::Error,
    },
    PreparedCommit {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        commit_id: usize,
        delta: CommitDelta,
        raft_applied_index: Option<u64>,
        raft_apply_marker: Option<RaftApplyMarker>,
    },
    PreparedRejectedBeforePersistence {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        commit_id: usize,
        err: anyhow::Error,
    },
    PreparedRollback {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        commit_id: usize,
        raft_applied_index: Option<u64>,
    },
    PreparedRollbackRejected {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        commit_id: usize,
        err: anyhow::Error,
    },
    MaxRepeatableTimestamp {
        new_max_repeatable: Timestamp,
        timer: Timer<VMHistogram>,
        result: RepeatableTimestampResult,
        commit_id: usize,
    },
    /// Replica delta persistence write. Goes through the same ordered pipeline
    /// as local commits and max_repeatable_ts bumps, ensuring all write log
    /// appends are serialized. This is the Raft pattern: all writes — local
    /// and replicated — flow through one sequential log.
    ReplicaDelta {
        commit_ts: Timestamp,
        remote_ts: Timestamp,
        snapshot: Snapshot,
        remapped_updates: Vec<common::document::DocumentUpdate>,
        write_source: WriteSource,
        write_bytes: u64,
        source_partition: Option<crate::partition::PartitionId>,
        applied_delta_watermarks: Option<BTreeMap<crate::partition::PartitionId, Timestamp>>,
        applied_data_delta_watermarks: Option<BTreeMap<crate::partition::PartitionId, Timestamp>>,
        applied_data_delta_ids: Option<AppliedDataDeltaIds>,
        raft_apply_marker: Option<RaftApplyMarker>,
        raft_nats_outbox_delta: Option<CommitDelta>,
        raft_state_machine_index: Option<u64>,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    },
    /// A crash left an atomic Raft apply marker next to already-installed
    /// Convex revisions. Consolidate the identity and ACK replay without
    /// publishing another logical write.
    RaftReplayRecovery {
        marker: RaftApplyMarker,
        delta: CommitDelta,
        raft_state_machine_index: u64,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    },
    RemoteReadFrontierHeartbeat {
        commit_ts: Timestamp,
        source_partition: crate::partition::PartitionId,
        applied_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    },
    InstallSnapshot {
        source: SnapshotInstallSource,
        snapshot_ts: Timestamp,
        snapshot: Snapshot,
        replication_frontiers: BTreeMap<crate::partition::PartitionId, Timestamp>,
        applied_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
        applied_data_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
        applied_data_delta_ids: AppliedDataDeltaIds,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotInstallSource {
    Bootstrap,
    Raft,
}

struct RaftSnapshotRequest {
    raft_index: u64,
    raft_term: u64,
    result: oneshot::Sender<anyhow::Result<Vec<u8>>>,
}

struct SnapshotInstallRequest {
    checkpoint: CheckpointData,
    source: SnapshotInstallSource,
    result: oneshot::Sender<anyhow::Result<Timestamp>>,
}

struct PreparedCheckpointInstall {
    snapshot_ts: Timestamp,
    compacted_documents: Vec<DocumentLogEntry>,
    index_entries: Vec<PersistenceIndexEntry>,
    snapshot: Snapshot,
    replication_frontiers: BTreeMap<crate::partition::PartitionId, Timestamp>,
    applied_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
    applied_data_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
    applied_data_delta_ids: AppliedDataDeltaIds,
}

async fn prepare_checkpoint_install<RT: Runtime>(
    runtime: RT,
    persistence: Arc<dyn Persistence>,
    virtual_system_mapping: VirtualSystemMapping,
    partition_map: Option<crate::partition::PartitionMap>,
    checkpoint: &CheckpointData,
) -> anyhow::Result<PreparedCheckpointInstall> {
    let snapshot_ts = checkpoint.timestamp;
    let compacted_documents = compact_checkpoint_documents(checkpoint);
    let checkpoint_persistence = Arc::new(CheckpointPersistence::from_checkpoint(
        CheckpointData {
            timestamp: checkpoint.timestamp,
            documents: compacted_documents.clone(),
            globals: checkpoint.globals.clone(),
        },
        persistence.reader().version(),
    ));
    let repeatable_ts =
        RepeatableTimestamp::new_validated(snapshot_ts, RepeatableReason::SnapshotManagerLatest);
    let mut db_snapshot = DatabaseSnapshot::load(
        runtime,
        checkpoint_persistence,
        repeatable_ts,
        Arc::new(NoopRetentionValidator),
        virtual_system_mapping,
    )
    .await?;

    if let Some(value) = checkpoint
        .globals
        .get(&String::from(PersistenceGlobalKey::TableSummary))
    {
        let summary_snapshot = TableSummarySnapshot::try_from(value.clone())?;
        let table_summaries = TableSummaries::new(
            summary_snapshot,
            db_snapshot.table_registry().table_mapping(),
            &db_snapshot.snapshot.virtual_system_mapping,
        )?;
        db_snapshot.snapshot.table_summaries = Some(table_summaries);
    }

    let index_entries = checkpoint_index_entries(&db_snapshot.snapshot, &compacted_documents);
    let replication_frontiers = checkpoint
        .globals
        .get(&String::from(PersistenceGlobalKey::ReplicationFrontiers))
        .map(|value| crate::snapshot_manager::replication_frontiers_from_json(value.clone()))
        .transpose()?
        .unwrap_or_else(|| {
            partition_map
                .as_ref()
                .map(|map| {
                    map.all_partitions()
                        .into_iter()
                        .filter(|partition| *partition != map.local_partition())
                        .map(|partition| (partition, Timestamp::MIN))
                        .collect()
                })
                .unwrap_or_default()
        });
    let applied_delta_watermarks = checkpoint
        .globals
        .get(&String::from(PersistenceGlobalKey::AppliedDeltaWatermarks))
        .map(|value| partition_timestamp_map_from_json(value.clone(), "applied delta watermarks"))
        .transpose()?
        .unwrap_or_default();
    let applied_data_delta_watermarks = checkpoint
        .globals
        .get(&String::from(
            PersistenceGlobalKey::AppliedDataDeltaWatermarks,
        ))
        .map(|value| {
            partition_timestamp_map_from_json(value.clone(), "applied data delta watermarks")
        })
        .transpose()?
        .unwrap_or_default();
    let applied_data_delta_ids = checkpoint
        .globals
        .get(&String::from(PersistenceGlobalKey::AppliedDataDeltaIds))
        .map(|value| applied_data_delta_ids_from_json(value.clone()))
        .transpose()?
        .unwrap_or_default();

    Ok(PreparedCheckpointInstall {
        snapshot_ts,
        compacted_documents,
        index_entries,
        snapshot: db_snapshot.snapshot,
        replication_frontiers,
        applied_delta_watermarks,
        applied_data_delta_watermarks,
        applied_data_delta_ids,
    })
}

async fn persist_checkpoint_install(
    persistence: Arc<dyn Persistence>,
    checkpoint: &CheckpointData,
    prepared: &PreparedCheckpointInstall,
    source: SnapshotInstallSource,
) -> anyhow::Result<()> {
    let reader = persistence.reader();
    let mut existing_documents = Vec::new();
    let mut document_stream = reader.load_documents(
        TimestampRange::all(),
        Order::Asc,
        1024,
        Arc::new(NoopRetentionValidator),
    );
    while let Some(entry) = document_stream.try_next().await? {
        existing_documents.push((entry.ts, entry.id));
    }
    if !existing_documents.is_empty() {
        persistence.delete(existing_documents).await?;
    }

    let mut cursor = None;
    loop {
        let chunk = persistence.load_index_chunk(cursor.clone(), 1024).await?;
        if chunk.is_empty() {
            break;
        }
        cursor = chunk.last().cloned();
        persistence.delete_index_entries(chunk).await?;
    }

    for chunk in prepared.compacted_documents.chunks(1024) {
        persistence
            .write(chunk, &[], ConflictStrategy::Overwrite)
            .await?;
    }
    for chunk in prepared.index_entries.chunks(1024) {
        persistence
            .write(&[], chunk, ConflictStrategy::Overwrite)
            .await?;
    }

    for prefix in [RAFT_NATS_OUTBOX_KEY_PREFIX, RAFT_APPLY_MARKER_KEY_PREFIX] {
        for (key, _) in reader.list_persistence_globals_with_prefix(prefix).await? {
            persistence.delete_persistence_global_raw(&key).await?;
        }
    }

    let mut copied_keys = PersistenceGlobalKey::checkpoint_keys();
    if source == SnapshotInstallSource::Raft {
        copied_keys.extend([
            PersistenceGlobalKey::TwoPhaseRedoRecords,
            PersistenceGlobalKey::RaftNatsOutbox,
        ]);
    }
    for key in copied_keys {
        let raw_key = String::from(key);
        if let Some(value) = checkpoint.globals.get(&raw_key) {
            persistence
                .write_persistence_global(key, value.clone())
                .await?;
        } else {
            persistence.delete_persistence_global_raw(&raw_key).await?;
        }
    }

    if source == SnapshotInstallSource::Raft {
        if !checkpoint
            .globals
            .contains_key(&String::from(PersistenceGlobalKey::TwoPhaseRedoRecords))
        {
            persistence
                .write_persistence_global(
                    PersistenceGlobalKey::TwoPhaseRedoRecords,
                    serde_json::json!({}),
                )
                .await?;
        }
        for (key, value) in checkpoint.globals.iter().filter(|(key, _)| {
            key.starts_with(RAFT_NATS_OUTBOX_KEY_PREFIX)
                || key.as_str() == RAFT_STATE_MACHINE_INDEX_KEY
        }) {
            persistence
                .write_persistence_global_raw(key, value.clone())
                .await?;
        }
    } else {
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::TwoPhaseRedoRecords,
                serde_json::json!({}),
            )
            .await?;
        persistence
            .delete_persistence_global_raw(RAFT_STATE_MACHINE_INDEX_KEY)
            .await?;
    }

    if !checkpoint.globals.contains_key(&String::from(
        PersistenceGlobalKey::AppliedDataDeltaWatermarks,
    )) {
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::AppliedDataDeltaWatermarks,
                serde_json::json!({}),
            )
            .await?;
    }
    if !checkpoint
        .globals
        .contains_key(&String::from(PersistenceGlobalKey::AppliedDataDeltaIds))
    {
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::AppliedDataDeltaIds,
                serde_json::json!({}),
            )
            .await?;
    }
    Ok(())
}

pub async fn recover_staged_raft_snapshot_install<RT: Runtime>(
    engine: Arc<raft_engine::Engine>,
    partition_id: crate::partition::PartitionId,
    persistence: Arc<dyn Persistence>,
    runtime: RT,
    virtual_system_mapping: VirtualSystemMapping,
    partition_map: Option<crate::partition::PartitionMap>,
) -> anyhow::Result<bool> {
    let Some(pending) =
        crate::raft_storage::ConvexRaftStorage::pending_snapshot_install_for_engine(
            &engine,
            partition_id,
        )?
    else {
        return Ok(false);
    };
    let metadata = pending.snapshot.get_metadata();
    let decoded = crate::raft_snapshot::decode_snapshot(
        pending.snapshot.get_data(),
        metadata.index,
        metadata.term,
    )?;
    let prepared = prepare_checkpoint_install(
        runtime,
        persistence.clone(),
        virtual_system_mapping,
        partition_map,
        &decoded.checkpoint,
    )
    .await?;
    persist_checkpoint_install(
        persistence,
        &decoded.checkpoint,
        &prepared,
        SnapshotInstallSource::Raft,
    )
    .await?;
    crate::raft_storage::ConvexRaftStorage::commit_pending_snapshot_install_for_engine(
        &engine,
        partition_id,
    )?;
    tracing::info!(
        "Recovered staged Raft snapshot install for partition {} at index {} and ts={}",
        partition_id.0,
        decoded.raft_index,
        u64::from(decoded.checkpoint.timestamp),
    );
    Ok(true)
}

impl PersistenceWrite {
    fn commit_id(&self) -> usize {
        match self {
            Self::Commit { commit_id, .. } => *commit_id,
            Self::RejectedBeforePersistence { commit_id, .. } => *commit_id,
            Self::PreparedCommit { commit_id, .. } => *commit_id,
            Self::PreparedRejectedBeforePersistence { commit_id, .. } => *commit_id,
            Self::PreparedRollback { commit_id, .. } => *commit_id,
            Self::PreparedRollbackRejected { commit_id, .. } => *commit_id,
            Self::MaxRepeatableTimestamp { commit_id, .. } => *commit_id,
            Self::ReplicaDelta { commit_id, .. } => *commit_id,
            Self::RaftReplayRecovery { commit_id, .. } => *commit_id,
            Self::RemoteReadFrontierHeartbeat { commit_id, .. } => *commit_id,
            Self::InstallSnapshot { commit_id, .. } => *commit_id,
        }
    }
}

pub const AFTER_PENDING_WRITE_SNAPSHOT: &str = "after_pending_write_snapshot";
pub const AFTER_RAFT_CONVEX_PERSISTENCE: &str = "after_raft_convex_persistence";
pub const AFTER_RAFT_SNAPSHOT_PUBLICATION: &str = "after_raft_snapshot_publication";
pub const AFTER_RAFT_APPLIED_INDEX_PERSISTENCE: &str = "after_raft_applied_index_persistence";

fn remote_read_partitions(
    reads: &ReadSet,
    table_mapping: &TableMapping,
    partition_map: &crate::partition::PartitionMap,
    write_source: &WriteSource,
) -> BTreeSet<crate::partition::PartitionId> {
    let mut partitions = BTreeSet::new();
    let mut visit_tablet = |tablet_id| {
        if let Ok(table_name) = table_mapping.tablet_name(tablet_id) {
            if let Some(partition) = crate::partition::routed_partition_for_table(
                &table_name,
                partition_map,
                write_source,
            ) && partition != partition_map.local_partition()
            {
                partitions.insert(partition);
            }
        }
    };

    for (index_name, _) in reads.iter_indexed() {
        visit_tablet(*index_name.table());
    }
    for (index_name, _) in reads.iter_search() {
        visit_tablet(*index_name.table());
    }
    partitions
}

fn resolver_read_set_interval_count(reads: &ReadSet) -> usize {
    reads
        .iter_indexed()
        .map(|(_, reads)| reads.intervals.len())
        .sum::<usize>()
        + reads.iter_search().count()
}

fn stale_replica_frontiers(
    snapshot_manager: &SnapshotManager,
    begin_ts: Timestamp,
    reads: &ReadSet,
    table_mapping: &TableMapping,
    partition_map: &crate::partition::PartitionMap,
    write_source: &WriteSource,
) -> Vec<(crate::partition::PartitionId, Timestamp)> {
    remote_read_partitions(reads, table_mapping, partition_map, write_source)
        .into_iter()
        .filter_map(|partition| {
            let frontier = snapshot_manager
                .replication_frontier(partition)
                .unwrap_or(Timestamp::MIN);
            (frontier < begin_ts).then_some((partition, frontier))
        })
        .collect()
}

fn merge_partition_timestamp_max(
    current: &BTreeMap<crate::partition::PartitionId, Timestamp>,
    proposed: BTreeMap<crate::partition::PartitionId, Timestamp>,
) -> BTreeMap<crate::partition::PartitionId, Timestamp> {
    let mut merged = current.clone();
    for (partition, ts) in proposed {
        if merged
            .get(&partition)
            .is_none_or(|current_ts| *current_ts < ts)
        {
            merged.insert(partition, ts);
        }
    }
    merged
}

fn merge_applied_data_delta_ids(
    current: &AppliedDataDeltaIds,
    proposed: AppliedDataDeltaIds,
) -> AppliedDataDeltaIds {
    let mut merged = current.clone();
    for (partition, timestamps) in proposed.origin_timestamps {
        merged
            .origin_timestamps
            .entry(partition)
            .or_default()
            .extend(timestamps);
    }
    merged
        .transport_stream_sequences
        .merge(proposed.transport_stream_sequences);
    merged
}

fn applied_data_delta_ids_to_json(ids: &AppliedDataDeltaIds) -> serde_json::Value {
    let mut origin_timestamps = serde_json::Map::new();
    for (partition, timestamps) in &ids.origin_timestamps {
        origin_timestamps.insert(
            partition.0.to_string(),
            serde_json::Value::Array(
                timestamps
                    .iter()
                    .map(|ts| serde_json::Value::from(u64::from(*ts)))
                    .collect(),
            ),
        );
    }
    serde_json::json!({
        "origin_timestamps": origin_timestamps,
        "transport_stream_sequences": {
            "compacted_watermark": ids.transport_stream_sequences.compacted_watermark,
            "exact_ids": ids.transport_stream_sequences
                .exact_ids
                .iter()
                .copied()
                .map(serde_json::Value::from)
                .collect::<Vec<_>>(),
        },
    })
}

pub(crate) fn applied_data_delta_ids_from_json(
    value: serde_json::Value,
) -> anyhow::Result<AppliedDataDeltaIds> {
    let object = value
        .as_object()
        .context("applied data delta ids should be an object")?;
    let origin_value = object.get("origin_timestamps").unwrap_or(&value);
    let origin_object = origin_value
        .as_object()
        .context("applied data delta origin timestamps should be an object")?;
    let mut ids = AppliedDataDeltaIds::default();
    for (partition, timestamps) in origin_object {
        if partition == "transport_stream_sequences" {
            continue;
        }
        let partition_id = partition
            .parse::<u32>()
            .with_context(|| format!("invalid applied data delta ids partition {partition:?}"))?;
        let timestamps = timestamps
            .as_array()
            .context("applied data delta ids entries should be arrays")?
            .iter()
            .map(|value| {
                let ts = value
                    .as_u64()
                    .context("applied data delta id should be a u64 timestamp")?;
                Timestamp::try_from(ts)
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()?;
        ids.origin_timestamps
            .insert(crate::partition::PartitionId(partition_id), timestamps);
    }
    if let Some(transport_value) = object.get("transport_stream_sequences") {
        let transport_object = transport_value
            .as_object()
            .context("applied data transport ids should be an object")?;
        ids.transport_stream_sequences.compacted_watermark = transport_object
            .get("compacted_watermark")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        ids.transport_stream_sequences.exact_ids = transport_object
            .get("exact_ids")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .map(|value| {
                value
                    .as_u64()
                    .context("applied data transport id should be a u64")
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()?;
        ids.transport_stream_sequences
            .exact_ids
            .retain(|id| *id > ids.transport_stream_sequences.compacted_watermark);
    }
    Ok(ids)
}

fn compact_checkpoint_documents(checkpoint: &CheckpointData) -> Vec<DocumentLogEntry> {
    let mut latest_documents = BTreeMap::new();
    for entry in &checkpoint.documents {
        latest_documents.insert(entry.id, entry.clone());
    }
    latest_documents
        .into_values()
        .filter(|entry| entry.value.is_some())
        .collect()
}

fn checkpoint_index_entries(
    snapshot: &Snapshot,
    documents: &[DocumentLogEntry],
) -> Vec<PersistenceIndexEntry> {
    let mut index_entries = Vec::new();
    for entry in documents {
        let Some(document) = entry.value.as_ref() else {
            continue;
        };
        let keys = snapshot
            .index_registry
            .document_index_keys(&PackedDocument::pack(document), tokenize);
        for (index_name, key_value) in keys.iter() {
            let DocumentIndexKeyValue::Standard(index_key) = key_value else {
                continue;
            };
            let Some(index) = snapshot.index_registry.get_enabled(index_name) else {
                continue;
            };
            index_entries.push(PersistenceIndexEntry {
                ts: entry.ts,
                index_id: index.id(),
                key: index_key.clone(),
                value: Some(InternalDocumentId::new(
                    document.id().tablet_id,
                    document.id().internal_id(),
                )),
            });
        }
    }
    index_entries.sort();
    index_entries
}

fn insert_delta_table_mapping(
    table_mapping: &TableMapping,
    tablet_id_to_table_name: &mut BTreeMap<value::TabletId, TableName>,
    tablet_id: value::TabletId,
) {
    if !tablet_id_to_table_name.contains_key(&tablet_id) {
        if let Ok(name) = table_mapping.tablet_name(tablet_id) {
            tablet_id_to_table_name.insert(tablet_id, name);
        }
    }
}

fn index_metadata_tablet_id(document: &ResolvedDocument) -> Option<value::TabletId> {
    let table_id_field = "table_id".parse::<common::types::FieldName>().ok()?;
    let value::ConvexValue::String(table_id) = document.value().0.get(&table_id_field)? else {
        return None;
    };
    table_id.parse().ok()
}

fn remap_index_metadata_document(
    document: &ResolvedDocument,
    local_index_tablet: value::TabletId,
    tablet_remap: &BTreeMap<value::TabletId, value::TabletId>,
    downgrade_query_ready_state: bool,
) -> anyhow::Result<ResolvedDocument> {
    let remote_metadata =
        common::bootstrap_model::index::TabletIndexMetadata::from_document(document.clone())?
            .into_value();
    let mut local_metadata = remote_metadata.map_table(&|remote_tablet| {
        tablet_remap.get(&remote_tablet).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot remap replicated _index metadata for source TabletId {:?}: table is not \
                 mapped locally",
                remote_tablet,
            )
        })
    })?;
    if downgrade_query_ready_state {
        downgrade_replicated_index_metadata(&mut local_metadata);
    }
    let remapped_id = ResolvedDocumentId {
        tablet_id: local_index_tablet,
        document_id: document.id().document_id,
    };
    ResolvedDocument::new(
        remapped_id,
        document.creation_time(),
        local_metadata.try_into()?,
    )
}

fn remap_index_metadata_update(
    update: &common::document::DocumentUpdate,
    local_index_tablet: value::TabletId,
    tablet_remap: &BTreeMap<value::TabletId, value::TabletId>,
    downgrade_query_ready_state: bool,
) -> anyhow::Result<common::document::DocumentUpdate> {
    let remapped_id = ResolvedDocumentId {
        tablet_id: local_index_tablet,
        document_id: update.id.document_id,
    };
    Ok(common::document::DocumentUpdate {
        id: remapped_id,
        old_document: update
            .old_document
            .as_ref()
            .map(|document| {
                remap_index_metadata_document(
                    document,
                    local_index_tablet,
                    tablet_remap,
                    downgrade_query_ready_state,
                )
            })
            .transpose()?,
        new_document: update
            .new_document
            .as_ref()
            .map(|document| {
                remap_index_metadata_document(
                    document,
                    local_index_tablet,
                    tablet_remap,
                    downgrade_query_ready_state,
                )
            })
            .transpose()?,
    })
}

fn replicated_index_update_is_already_present(
    snapshot: &Snapshot,
    update: &common::document::DocumentUpdate,
) -> anyhow::Result<bool> {
    let Some(document) = update
        .new_document
        .as_ref()
        .or(update.old_document.as_ref())
    else {
        return Ok(false);
    };
    let metadata =
        common::bootstrap_model::index::TabletIndexMetadata::from_document(document.clone())?
            .into_value();
    let existing = if metadata.config.is_enabled() {
        snapshot.index_registry.get_enabled(&metadata.name)
    } else {
        snapshot.index_registry.get_pending(&metadata.name)
    };
    let Some(existing) = existing else {
        return Ok(false);
    };
    if existing.id() == document.id().internal_id() {
        return Ok(false);
    }
    anyhow::ensure!(
        existing.metadata().config == metadata.config,
        "Replicated _index metadata for {} has document id {} but local index id {} already \
         exists with different config",
        metadata.name,
        document.id().internal_id(),
        existing.id(),
    );
    Ok(true)
}

fn downgrade_replicated_index_metadata(
    metadata: &mut common::bootstrap_model::index::TabletIndexMetadata,
) {
    use common::bootstrap_model::index::{
        database_index::{
            DatabaseIndexBackfillState,
            DatabaseIndexState,
        },
        text_index::{
            TextIndexBackfillState,
            TextIndexState,
        },
        vector_index::{
            VectorIndexBackfillState,
            VectorIndexState,
        },
        IndexConfig,
    };

    if metadata.name.is_by_id_or_creation_time() {
        return;
    }

    match &mut metadata.config {
        IndexConfig::Database { on_disk_state, .. } => {
            let staged = on_disk_state.is_staged();
            if !matches!(on_disk_state, DatabaseIndexState::Backfilling(_)) {
                *on_disk_state = DatabaseIndexState::Backfilling(DatabaseIndexBackfillState {
                    index_created_lower_bound: Timestamp::MIN,
                    retention_started: false,
                    staged,
                });
            }
        },
        IndexConfig::Text { on_disk_state, .. } => {
            let staged = on_disk_state.is_staged();
            if !matches!(on_disk_state, TextIndexState::Backfilling(_)) {
                *on_disk_state = TextIndexState::Backfilling(TextIndexBackfillState::new(staged));
            }
        },
        IndexConfig::Vector { on_disk_state, .. } => {
            let staged = on_disk_state.is_staged();
            if !matches!(on_disk_state, VectorIndexState::Backfilling(_)) {
                *on_disk_state =
                    VectorIndexState::Backfilling(VectorIndexBackfillState::new(staged));
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplicaDeltaTimestamps {
    /// Timestamp assigned by the source partition when it committed the write.
    origin_ts: Timestamp,
    /// Timestamp at which this node can safely publish the replicated state.
    local_apply_ts: Timestamp,
}

fn replica_delta_timestamps(
    origin_ts: Timestamp,
    latest_local_ts: Timestamp,
    last_assigned_ts: Timestamp,
) -> anyhow::Result<ReplicaDeltaTimestamps> {
    Ok(ReplicaDeltaTimestamps {
        origin_ts,
        local_apply_ts: cmp::max(
            origin_ts,
            cmp::max(latest_local_ts.succ()?, last_assigned_ts.succ()?),
        ),
    })
}

type RaftCommitWaiter = Option<oneshot::Receiver<RaftProposalResult>>;

struct PublishedCommit {
    delta: CommitDelta,
    raft_applied_index: Option<u64>,
}

pub struct Committer<RT: Runtime> {
    // Internal staged commits for conflict checking.
    pending_writes: PendingWrites,
    // 2PC prepared intents for conflict checking. These are intentionally
    // separate from `pending_writes`, whose FIFO is only for publishable commits.
    prepared_writes: PreparedWrites,
    // External log of writes for subscriptions.
    log: LogWriter,

    snapshot_manager: Writer<SnapshotManager>,
    persistence: Arc<dyn Persistence>,
    runtime: RT,

    last_assigned_ts: Timestamp,
    // Highest exact read barrier admitted by this committer. Unlike
    // `last_assigned_ts`, ordinary timestamp allocation does not advance this
    // floor, so a coordinator may allocate T and then prepare at T.
    read_barrier_floor: Timestamp,
    // Exact cluster barriers certified by this leader. We intentionally do not
    // infer that every timestamp below the maximum is safe under timestamp
    // translation.
    closed_read_timestamps: BTreeSet<Timestamp>,

    persistence_writes: FuturesOrdered<BoxFuture<'static, anyhow::Result<PersistenceWrite>>>,

    retention_validator: Arc<dyn RetentionValidator>,
    virtual_system_mapping: VirtualSystemMapping,

    user_documents_size_gauge: Subgauge,

    // Distributed log for replication. Publishes CommitDeltas after each commit
    // so Replica nodes can update their state.
    distributed_log: Arc<dyn DistributedLog>,

    // Refreshable placement state for write routing. Determines which tables
    // this node owns.
    // None means single-partition mode (owns everything).
    placement_state: Option<crate::partition::PlacementState>,

    // Global timestamp oracle for multi-node deployments (TiDB PD pattern).
    // When set, next_commit_ts() draws from the TSO instead of the local clock,
    // ensuring globally unique timestamps across all nodes.
    // None means single-node mode (local clock, existing behavior).
    timestamp_oracle: Option<Arc<dyn crate::timestamp_oracle::TimestampOracle>>,
    maintenance_tso_refill_in_flight: Arc<AtomicBool>,

    // 2PC prepared transactions awaiting CommitPrepared or RollbackPrepared.
    // Keyed by TwoPhaseTransactionId, storing the prepared intent and metadata
    // needed to finalize the commit. (Vitess redo log pattern)
    prepared_transactions:
        std::collections::HashMap<crate::two_phase::TwoPhaseTransactionId, PreparedTransaction>,

    // 2PC CommitPrepared requests that are already past the decision point and
    // waiting on the async Raft+persistence path. Duplicate CommitPrepared
    // messages must join the same in-flight commit instead of writing the same
    // prepared delta twice.
    prepared_commit_waiters: std::collections::HashMap<
        crate::two_phase::TwoPhaseTransactionId,
        Vec<oneshot::Sender<anyhow::Result<Timestamp>>>,
    >,

    // Rollback is also a replicated state-machine operation. Keep duplicate
    // requests attached to one in-flight marker and prevent a contradictory
    // CommitPrepared from starting while that marker is unresolved.
    prepared_rollback_waiters: std::collections::HashMap<
        crate::two_phase::TwoPhaseTransactionId,
        Vec<oneshot::Sender<anyhow::Result<()>>>,
    >,

    // Snapshots for queued-but-not-yet-published state machine writes.
    // Local commits and replica deltas must both build on the latest queued
    // snapshot, not just the latest published one, or a later publish can
    // erase earlier in-flight catalog changes from the next transaction view.
    queued_snapshots: VecDeque<(usize, Timestamp, Snapshot)>,

    // Raft partition state for leadership checking.
    // When set, the Committer rejects writes if this node is not the Raft leader.
    // None means no Raft (existing behavior — always accept writes).
    raft_state: Option<crate::raft_partition::RaftPartitionState>,

    // Durable origin-timestamp freshness frontier for replicated transport
    // messages. This includes data deltas and frontier-only heartbeats, so it
    // is safe for read-after-write waits but not for data redelivery dedupe.
    applied_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,

    // Durable origin-timestamp idempotency frontier for non-empty replicated
    // data deltas only. Heartbeats must never advance this map. This is only a
    // max watermark; exact idempotency lives in `applied_data_delta_ids` because
    // Raft->NATS outbox replay can deliver older origin timestamps after newer
    // ones.
    applied_data_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,

    // Exact durable origin ids for non-empty replicated data deltas. Keyed by
    // source partition and origin commit timestamp.
    applied_data_delta_ids: AppliedDataDeltaIds,

    // Raft data writes that reached Convex persistence atomically but had not
    // yet been consolidated into `applied_data_delta_ids` when the process
    // stopped. Replay must repair bookkeeping without installing them again.
    raft_apply_markers: BTreeSet<RaftApplyMarker>,

    // Sender for requeueing internal work that must wait for earlier state
    // machine timestamps to become visible without blocking the committer loop.
    sender: mpsc::Sender<CommitterMessage>,
}

impl<RT: Runtime> Committer<RT> {
    fn latest_queued_snapshot(&self) -> Option<(Timestamp, Snapshot)> {
        self.queued_snapshots
            .back()
            .map(|(_, ts, snapshot)| (*ts, snapshot.clone()))
    }

    fn base_snapshot_for_new_writes(&self) -> Snapshot {
        let queued = self.latest_queued_snapshot();
        let pending = self.pending_writes.latest();
        match (queued, pending) {
            (Some((queued_ts, queued_snapshot)), Some((pending_ts, pending_snapshot))) => {
                if queued_ts >= pending_ts {
                    queued_snapshot
                } else {
                    pending_snapshot
                }
            },
            (Some((_, queued_snapshot)), None) => queued_snapshot,
            (None, Some((_, pending_snapshot))) => pending_snapshot,
            (None, None) => self.snapshot_manager.read().latest_snapshot(),
        }
    }

    fn enqueue_snapshot(&mut self, commit_id: usize, ts: Timestamp, snapshot: Snapshot) {
        if let Some((last_commit_id, last_ts, _)) = self.queued_snapshots.back() {
            assert!(
                *last_commit_id < commit_id,
                "queued snapshots must be ordered by commit id"
            );
            assert!(
                *last_ts <= ts,
                "queued snapshots must be monotonic by timestamp"
            );
        }
        self.queued_snapshots.push_back((commit_id, ts, snapshot));
    }

    fn local_partition_for_two_phase(&self) -> crate::partition::PartitionId {
        self.placement_state
            .as_ref()
            .map(|placement_state| placement_state.local_partition())
            .unwrap_or(crate::partition::PartitionId(0))
    }

    fn prepare_admission_blocker_ts(&self) -> Option<Timestamp> {
        let pending_write_ts = self.pending_writes.min_ts();
        let committing_prepared_ts = self
            .prepared_commit_waiters
            .keys()
            .filter_map(|transaction_id| {
                self.prepared_transactions
                    .get(transaction_id)
                    .map(|prepared| prepared.commit_ts)
            })
            .min();
        match (pending_write_ts, committing_prepared_ts) {
            (Some(pending), Some(prepared)) => Some(cmp::min(pending, prepared)),
            (Some(pending), None) => Some(pending),
            (None, Some(prepared)) => Some(prepared),
            (None, None) => None,
        }
    }

    fn dequeue_snapshot(&mut self, commit_id: usize) {
        let (queued_commit_id, ..) = self
            .queued_snapshots
            .pop_front()
            .expect("missing queued snapshot for published write");
        assert_eq!(
            queued_commit_id, commit_id,
            "published snapshot order diverged from queued write order"
        );
    }

    pub(crate) fn start(
        log: LogWriter,
        snapshot_manager: Writer<SnapshotManager>,
        persistence: Arc<dyn Persistence>,
        runtime: RT,
        retention_validator: Arc<dyn RetentionValidator>,
        shutdown: ShutdownSignal,
        virtual_system_mapping: VirtualSystemMapping,
        distributed_log: Arc<dyn DistributedLog>,
        partition_map: Option<crate::partition::PartitionMap>,
        node_addresses: Option<crate::two_phase::NodeAddresses>,
        two_phase_decision_log: Arc<dyn crate::two_phase::TwoPhaseDecisionLog>,
        cluster_grpc_auth: Option<ClusterGrpcAuth>,
        timestamp_oracle: Option<Arc<dyn crate::timestamp_oracle::TimestampOracle>>,
        raft_state: Option<crate::raft_partition::RaftPartitionState>,
        applied_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
        applied_data_delta_watermarks: BTreeMap<crate::partition::PartitionId, Timestamp>,
        applied_data_delta_ids: AppliedDataDeltaIds,
        raft_apply_markers: BTreeSet<RaftApplyMarker>,
    ) -> CommitterClient {
        let mut applied_data_delta_ids = applied_data_delta_ids;
        applied_data_delta_ids
            .prune_origin_timestamps_below_watermarks(&applied_data_delta_watermarks);
        let persistence_reader = persistence.reader();
        let (tx, rx) = mpsc::channel(*COMMITTER_QUEUE_SIZE);
        let snapshot_reader = snapshot_manager.reader();
        let read_barrier_floor = *snapshot_manager.read().persisted_max_repeatable_ts();
        let placement_state =
            partition_map.map(crate::partition::PlacementState::from_partition_map);
        let client_placement_state = placement_state.clone();
        let client_node_addresses = Arc::new(RwLock::new(node_addresses));
        let committer = Self {
            pending_writes: PendingWrites::new(),
            prepared_writes: PreparedWrites::new(),
            log,
            snapshot_manager,
            persistence,
            runtime: runtime.clone(),
            last_assigned_ts: Timestamp::MIN,
            read_barrier_floor,
            closed_read_timestamps: BTreeSet::new(),
            persistence_writes: FuturesOrdered::new(),
            retention_validator: retention_validator.clone(),
            virtual_system_mapping,
            user_documents_size_gauge: user_documents_size_subgauge(),
            distributed_log,
            placement_state,
            timestamp_oracle,
            maintenance_tso_refill_in_flight: Arc::new(AtomicBool::new(false)),
            prepared_transactions: std::collections::HashMap::new(),
            prepared_commit_waiters: std::collections::HashMap::new(),
            prepared_rollback_waiters: std::collections::HashMap::new(),
            queued_snapshots: VecDeque::new(),
            raft_state,
            applied_delta_watermarks,
            applied_data_delta_watermarks,
            applied_data_delta_ids,
            raft_apply_markers,
            sender: tx.clone(),
        };
        let handle = runtime.spawn("committer", async move {
            if let Err(err) = committer.go(rx).await {
                // Committer hit a fatal error. This should only happen if a
                // persistence write fails or in case of unrecoverable logic
                // errors.
                shutdown.signal(err);
                tracing::error!("Shutting down committer");
            }
        });
        CommitterClient {
            handle: Arc::new(Mutex::new(handle)),
            sender: tx,
            persistence_reader,
            retention_validator,
            snapshot_reader,
            placement_state: client_placement_state,
            node_addresses: client_node_addresses,
            two_phase_decision_log,
            two_phase_client_pool: Arc::new(crate::two_phase::TwoPhaseCommitGrpcClientPool::new(
                cluster_grpc_auth,
            )),
        }
    }

    async fn go(mut self, mut rx: mpsc::Receiver<CommitterMessage>) -> anyhow::Result<()> {
        self.recover_two_phase_redo_records().await?;
        if self.should_replay_raft_nats_outbox()
            && let Err(err) = Self::replay_raft_nats_outbox_once(
                self.persistence.clone(),
                self.distributed_log.clone(),
            )
            .await
        {
            tracing::warn!("Failed to replay Raft->NATS outbox during committer startup: {err:#}");
        }

        let mut last_bumped_repeatable_ts = self.runtime.monotonic_now();
        let mut last_remote_read_frontier_heartbeat = self.runtime.monotonic_now();
        let mut last_remote_read_frontier_heartbeat_ts = Timestamp::MIN;
        let mut last_raft_nats_outbox_replay = self.runtime.monotonic_now();
        // Assume there were commits just before the backend restarted, so first do a
        // quick bump.
        // None means a bump is ongoing. Avoid parallel bumps in case they
        // commit out of order and regress the repeatable timestamp.
        let mut next_bump_wait = Some(*MAX_REPEATABLE_TIMESTAMP_COMMIT_DELAY);

        // This span starts with receiving a commit message and ends with that same
        // commit getting published. It captures all of the committer activity
        // in between.
        let mut committer_span = None;
        // Keep a monotonically increasing id to keep track of honeycomb traces
        // Each commit_id tracks a single write to persistence, from the time the commit
        // message is received until the time the commit has been published. We skip
        // read-only transactions or commits that fail to validate.
        let mut commit_id = 0;
        // Keep track of the commit_id that is currently being traced.
        let mut span_commit_id = None;
        let mut deferred_prepares = VecDeque::new();
        let mut pending_raft_snapshot: Option<RaftSnapshotRequest> = None;
        let mut pending_snapshot_install: Option<SnapshotInstallRequest> = None;
        let mut snapshot_install_in_progress = false;
        loop {
            if let Some(request) = pending_snapshot_install.take() {
                if self.persistence_writes.is_empty() {
                    self.install_snapshot(
                        request.checkpoint,
                        request.source,
                        request.result,
                        commit_id,
                    )?;
                    commit_id += 1;
                    snapshot_install_in_progress = true;
                    continue;
                }
                pending_snapshot_install = Some(request);
            }
            if let Some(request) = pending_raft_snapshot.take() {
                if self.persistence_writes.is_empty() {
                    let result = self
                        .build_raft_snapshot_bytes(request.raft_index, request.raft_term)
                        .await;
                    let _ = request.result.send(result);
                    continue;
                }
                pending_raft_snapshot = Some(request);
            }
            let snapshot_barrier_active = pending_raft_snapshot.is_some()
                || pending_snapshot_install.is_some()
                || snapshot_install_in_progress;
            if !snapshot_barrier_active
                && self.prepare_admission_blocker_ts().is_none()
                && let Some(deferred_prepare) = deferred_prepares.pop_front()
            {
                self.handle_deferred_prepare(deferred_prepare).await;
                continue;
            }
            let bump_fut = if !snapshot_barrier_active && let Some(wait) = &next_bump_wait {
                Either::Left(
                    self.runtime
                        .wait(wait.saturating_sub(last_bumped_repeatable_ts.elapsed())),
                )
            } else {
                Either::Right(std::future::pending())
            };
            let remote_read_frontier_heartbeat_fut = if !snapshot_barrier_active
                && self
                    .placement_state
                    .as_ref()
                    .is_some_and(|placement_state| placement_state.num_partitions() > 1)
            {
                Either::Left(
                    self.runtime.wait(
                        REMOTE_READ_FRONTIER_HEARTBEAT_INTERVAL
                            .saturating_sub(last_remote_read_frontier_heartbeat.elapsed()),
                    ),
                )
            } else {
                Either::Right(std::future::pending())
            };
            let raft_nats_outbox_replay_fut = if !snapshot_barrier_active
                && self
                    .placement_state
                    .as_ref()
                    .is_some_and(|placement_state| placement_state.num_partitions() > 1)
            {
                Either::Left(
                    self.runtime.wait(
                        RAFT_NATS_OUTBOX_REPLAY_INTERVAL
                            .saturating_sub(last_raft_nats_outbox_replay.elapsed()),
                    ),
                )
            } else {
                Either::Right(std::future::pending())
            };
            let receive_message_fut = if deferred_prepares.is_empty() && !snapshot_barrier_active {
                Either::Left(rx.recv())
            } else {
                Either::Right(std::future::pending())
            };
            select_biased! {
                _ = bump_fut.fuse() => {
                    let committer_span = committer_span.get_or_insert_with(|| {
                        span_commit_id = Some(commit_id);
                        Span::root("bump_max_repeatable", SpanContext::random())
                    });
                    // Advance the repeatable read timestamp so non-leaders can
                    // establish a recent repeatable snapshot.
                    next_bump_wait = None;
                    let (tx, _rx) = oneshot::channel();
                    if self.bump_max_repeatable_ts(tx, commit_id, committer_span) {
                        commit_id += 1;
                    } else {
                        // No locally reserved TSO value can satisfy the floor.
                        // Retry maintenance later while an asynchronous refill
                        // runs outside the committer loop.
                        next_bump_wait = Some(*MAX_REPEATABLE_TIMESTAMP_COMMIT_DELAY);
                    }
                    last_bumped_repeatable_ts = self.runtime.monotonic_now();
                }
                _ = remote_read_frontier_heartbeat_fut.fuse() => {
                    self.maybe_publish_idle_remote_read_frontier_heartbeat(
                        &mut last_remote_read_frontier_heartbeat_ts,
                    );
                    last_remote_read_frontier_heartbeat = self.runtime.monotonic_now();
                }
                _ = raft_nats_outbox_replay_fut.fuse() => {
                    if self.should_replay_raft_nats_outbox() {
                        match Self::replay_raft_nats_outbox_once(
                            self.persistence.clone(),
                            self.distributed_log.clone(),
                        )
                        .await
                        {
                            Ok(replayed) if replayed > 0 => {
                                tracing::info!("Replayed {replayed} Raft->NATS outbox deltas");
                            },
                            Ok(_) => {},
                            Err(err) => {
                                tracing::warn!("Failed to replay Raft->NATS outbox: {err:#}");
                            },
                        }
                    }
                    last_raft_nats_outbox_replay = self.runtime.monotonic_now();
                }
                result = self.persistence_writes.select_next_some() => {
                    let pending_commit = result.context("Write failed. Unsure if transaction committed to disk.")?;
                    let pending_commit_id = pending_commit.commit_id();
                    match pending_commit {
                        PersistenceWrite::Commit {
                            pending_write,
                            commit_timer,
                            result,
                            parent_trace,
                            delta,
                            raft_applied_index,
                            raft_apply_marker,
                            ..
                        } => {
                            let commit_ts = pending_write.must_commit_ts();
                            if let Some(marker) = raft_apply_marker
                                && let Err(err) = self.consolidate_raft_apply_marker(marker).await
                            {
                                return Self::fail_committed_write(
                                    result,
                                    commit_ts,
                                    "consolidate atomic Raft apply marker",
                                    err,
                                );
                            }
                            let parent_span = initialize_root_from_parent("Committer::publish_commit", parent_trace);
                            let publish_commit_span = committer_span.as_ref().map(|root| Span::enter_with_parents("publish_commit", [root, &parent_span])).unwrap_or_else(|| parent_span);
                            let _guard = publish_commit_span.set_local_parent();
                            self.dequeue_snapshot(pending_commit_id);
                            let published_commit = match self.publish_commit(
                                pending_write, delta,
                            ) {
                                Ok(published_commit) => published_commit,
                                Err(err) => {
                                    return Self::fail_committed_write(
                                        result,
                                        commit_ts,
                                        "publish committed write",
                                        err,
                                    );
                                },
                            };
                            drop(_guard);
                            if raft_applied_index.is_some() {
                                self.runtime
                                    .pause_client()
                                    .wait(AFTER_RAFT_SNAPSHOT_PUBLICATION)
                                    .await;
                            }
                            if let Some(raft_applied_index) = raft_applied_index {
                                let raft_mark_timer =
                                    metrics::commit_hot_path_stage_timer("local", "raft_mark_applied");
                                let Some(raft_state) = self.raft_state.as_ref() else {
                                    return Self::fail_committed_write(
                                        result,
                                        commit_ts,
                                        "mark Raft applied index",
                                        anyhow::anyhow!(
                                            "commit returned Raft applied index {} without Raft state",
                                            raft_applied_index,
                                        ),
                                    );
                                };
                                if let Err(err) =
                                    raft_state.mark_applied(raft_applied_index).await
                                {
                                    return Self::fail_committed_write(
                                        result,
                                        commit_ts,
                                        "mark Raft applied index",
                                        err,
                                    );
                                }
                                raft_mark_timer.finish();
                                self.runtime
                                    .pause_client()
                                    .wait(AFTER_RAFT_APPLIED_INDEX_PERSISTENCE)
                                    .await;
                            }
                            let use_raft_nats_outbox =
                                Self::should_record_raft_nats_outbox_delta(&published_commit.delta);
                            if use_raft_nats_outbox {
                                if let Err(err) = Self::record_and_replay_raft_nats_outbox_delta(
                                    self.persistence.clone(),
                                    self.distributed_log.clone(),
                                    &published_commit.delta,
                                    "local",
                                )
                                .await
                                {
                                    return Self::fail_committed_write(
                                        result,
                                        commit_ts,
                                        "record committed write in Raft->NATS outbox",
                                        err,
                                    );
                                }
                            } else if let Err(err) = Self::publish_commit_delta(
                                self.distributed_log.clone(),
                                published_commit.delta.clone(),
                                "local",
                            )
                            .await
                            {
                                return Self::fail_committed_write(
                                    result,
                                    commit_ts,
                                    "publish committed write to replication log",
                                    err,
                                );
                            }
                            let _ = result.send(Ok(CommitOutcome {
                                ts: commit_ts,
                                source_partition: published_commit.delta.source_partition,
                                read_after_write_partitions: published_commit
                                    .delta
                                    .source_partition
                                    .into_iter()
                                    .collect(),
                                invalidated_tables: published_commit
                                    .delta
                                    .tablet_id_to_table_name
                                    .values()
                                    .cloned()
                                    .collect(),
                            }));

                            // When we next get free cycles and there is no ongoing bump,
                            // bump max_repeatable_ts so followers can read this commit.
                            if next_bump_wait.is_some() {
                                next_bump_wait = Some(*MAX_REPEATABLE_TIMESTAMP_COMMIT_DELAY);
                            }
                            commit_timer.finish();
                        },
                        PersistenceWrite::RejectedBeforePersistence {
                            pending_write,
                            commit_timer,
                            result,
                            err,
                            ..
                        } => {
                            self.dequeue_snapshot(pending_commit_id);
                            let commit_ts = pending_write.must_commit_ts();
                            self.pending_writes.remove(pending_write).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "pre-persistence rejected commit at ts={} was not pending",
                                    u64::from(commit_ts),
                                )
                            })?;
                            let (_, latest_snapshot) = self.snapshot_manager.read().latest();
                            self.pending_writes
                                .recompute_pending_snapshots(latest_snapshot);
                            let _ = result.send(Err(err));
                            commit_timer.finish();
                        },
                        PersistenceWrite::PreparedCommit {
                            transaction_id,
                            delta,
                            raft_applied_index,
                            raft_apply_marker,
                            ..
                        } => {
                            let commit_ts = delta.ts;
                            if let Some(marker) = raft_apply_marker
                                && let Err(err) = self.consolidate_raft_apply_marker(marker).await
                            {
                                return self.fail_prepared_committed_write(
                                    &transaction_id,
                                    commit_ts,
                                    "consolidate atomic Raft apply marker",
                                    err,
                                );
                            }
                            let published_commit = match self.publish_prepared_commit(
                                transaction_id.clone(),
                                delta,
                                raft_applied_index,
                            ) {
                                Ok(published_commit) => published_commit,
                                Err(err) => {
                                    return self.fail_prepared_committed_write(
                                        &transaction_id,
                                        commit_ts,
                                        "publish prepared committed write",
                                        err,
                                    );
                                },
                            };
                            if published_commit.raft_applied_index.is_some() {
                                self.runtime
                                    .pause_client()
                                    .wait(AFTER_RAFT_SNAPSHOT_PUBLICATION)
                                    .await;
                            }
                            if let Some(raft_applied_index) = published_commit.raft_applied_index {
                                let raft_mark_timer = metrics::commit_hot_path_stage_timer(
                                    "2pc_participant",
                                    "raft_mark_applied",
                                );
                                let Some(raft_state) = self.raft_state.as_ref() else {
                                    return self.fail_prepared_committed_write(
                                        &transaction_id,
                                        commit_ts,
                                        "mark Raft applied index",
                                        anyhow::anyhow!(
                                            "2PC CommitPrepared txn={} returned Raft applied index \
                                             {} without Raft state",
                                            transaction_id,
                                            raft_applied_index,
                                        ),
                                    );
                                };
                                if let Err(err) =
                                    raft_state.mark_applied(raft_applied_index).await
                                {
                                    return self.fail_prepared_committed_write(
                                        &transaction_id,
                                        commit_ts,
                                        "mark Raft applied index",
                                        err,
                                    );
                                }
                                raft_mark_timer.finish();
                                self.runtime
                                    .pause_client()
                                    .wait(AFTER_RAFT_APPLIED_INDEX_PERSISTENCE)
                                    .await;
                            }
                            let use_raft_nats_outbox =
                                Self::should_record_raft_nats_outbox_delta(&published_commit.delta);
                            if use_raft_nats_outbox {
                                if let Err(err) = Self::record_and_replay_raft_nats_outbox_delta(
                                    self.persistence.clone(),
                                    self.distributed_log.clone(),
                                    &published_commit.delta,
                                    "2pc_participant",
                                )
                                .await
                                {
                                    return self.fail_prepared_committed_write(
                                        &transaction_id,
                                        commit_ts,
                                        "record prepared committed write in Raft->NATS outbox",
                                        err,
                                    );
                                }
                            } else if let Err(err) = Self::publish_commit_delta(
                                self.distributed_log.clone(),
                                published_commit.delta.clone(),
                                "2pc_participant",
                            )
                            .await
                            {
                                return self.fail_prepared_committed_write(
                                    &transaction_id,
                                    commit_ts,
                                    "publish prepared committed write to replication log",
                                    err,
                                );
                            }
                            if let Err(err) = Self::delete_two_phase_redo(
                                self.persistence.clone(),
                                &transaction_id,
                            )
                            .await
                            {
                                return self.fail_prepared_committed_write(
                                    &transaction_id,
                                    commit_ts,
                                    "delete prepared committed write redo",
                                    err,
                                );
                            }
                            self.send_prepared_commit_waiters(&transaction_id, Ok(commit_ts));
                            if next_bump_wait.is_some() {
                                next_bump_wait = Some(*MAX_REPEATABLE_TIMESTAMP_COMMIT_DELAY);
                            }
                        },
                        PersistenceWrite::PreparedRejectedBeforePersistence {
                            transaction_id,
                            err,
                            ..
                        } => {
                            self.send_prepared_commit_waiters(&transaction_id, Err(err));
                        },
                        PersistenceWrite::PreparedRollback {
                            transaction_id,
                            raft_applied_index,
                            ..
                        } => {
                            let apply_result = async {
                                self.handle_rollback_prepared(transaction_id.clone())?;
                                if let Some(raft_applied_index) = raft_applied_index {
                                    Self::persist_two_phase_rollback_with_state_machine_index(
                                        self.persistence.clone(),
                                        &transaction_id,
                                        raft_applied_index,
                                    )
                                    .await?;
                                    let Some(raft_state) = self.raft_state.as_ref() else {
                                        anyhow::bail!(
                                            "2PC RollbackPrepared txn={} returned Raft applied index {} without Raft state",
                                            transaction_id,
                                            raft_applied_index,
                                        );
                                    };
                                    raft_state.mark_applied(raft_applied_index).await?;
                                } else {
                                    Self::delete_two_phase_redo(
                                        self.persistence.clone(),
                                        &transaction_id,
                                    )
                                    .await?;
                                }
                                Ok::<(), anyhow::Error>(())
                            }
                            .await;
                            match apply_result {
                                Ok(()) => self.send_prepared_rollback_waiters(
                                    &transaction_id,
                                    Ok(()),
                                ),
                                Err(err) => {
                                    let message = format!(
                                        "Failed to apply committed 2PC rollback for txn={}: {err:#}",
                                        transaction_id,
                                    );
                                    self.send_prepared_rollback_waiters(
                                        &transaction_id,
                                        Err(anyhow::anyhow!(message.clone())),
                                    );
                                    anyhow::bail!(message);
                                },
                            }
                        },
                        PersistenceWrite::PreparedRollbackRejected {
                            transaction_id,
                            err,
                            ..
                        } => {
                            self.send_prepared_rollback_waiters(&transaction_id, Err(err));
                        },
                        PersistenceWrite::MaxRepeatableTimestamp {
                            new_max_repeatable,
                            timer,
                            result,
                            ..
                        } => {
                            let span = committer_span.as_ref().map(|root| Span::enter_with_parent("publish_max_repeatable_ts", root)).unwrap_or_else(Span::noop);
                            span.set_local_parent();
                            if let Some(min_prepared_ts) = self.prepared_writes.min_ts()
                                && min_prepared_ts <= new_max_repeatable
                            {
                                tracing::debug!(
                                    "Skipping max repeatable timestamp publish at ts={} behind \
                                     unresolved 2PC prepared ts={}",
                                    u64::from(new_max_repeatable),
                                    u64::from(min_prepared_ts),
                                );
                                let current = *self.snapshot_manager
                                    .read()
                                    .persisted_max_repeatable_ts();
                                let base_period = *MAX_REPEATABLE_TIMESTAMP_IDLE_FREQUENCY;
                                next_bump_wait = Some(
                                    self.runtime.rng().random_range(base_period..base_period * 2),
                                );
                                match result {
                                    RepeatableTimestampResult::Background(result) => {
                                        let _ = result.send(current);
                                    },
                                    RepeatableTimestampResult::ClusterReadBarrier(result) => {
                                        let _ = result.send(Ok(ReadTimestampClosure::Blocked(
                                            min_prepared_ts,
                                        )));
                                    },
                                }
                                drop(timer);
                                continue;
                            }
                            let is_cluster_read_barrier = matches!(
                                &result,
                                RepeatableTimestampResult::ClusterReadBarrier(_)
                            );
                            if is_cluster_read_barrier
                                && let Err(err) = self.ensure_leader_for_read_barrier()
                            {
                                if let RepeatableTimestampResult::ClusterReadBarrier(result) =
                                    result
                                {
                                    let _ = result.send(Err(err));
                                }
                                drop(timer);
                                continue;
                            }
                            self.publish_max_repeatable_ts(new_max_repeatable)?;
                            if is_cluster_read_barrier {
                                self.closed_read_timestamps.insert(new_max_repeatable);
                                while self.closed_read_timestamps.len()
                                    > MAX_CLOSED_READ_TIMESTAMPS
                                {
                                    self.closed_read_timestamps.pop_first();
                                }
                            }
                            let base_period = *MAX_REPEATABLE_TIMESTAMP_IDLE_FREQUENCY;
                            next_bump_wait = Some(
                                self.runtime.rng().random_range(base_period..base_period * 2),
                            );
                            match result {
                                RepeatableTimestampResult::Background(result) => {
                                    let _ = result.send(new_max_repeatable);
                                },
                                RepeatableTimestampResult::ClusterReadBarrier(result) => {
                                    let _ = result.send(Ok(ReadTimestampClosure::Closed(
                                        new_max_repeatable,
                                    )));
                                },
                            }
                            drop(timer);
                        },
                        PersistenceWrite::ReplicaDelta {
                            commit_ts,
                            remote_ts,
                            snapshot,
                            remapped_updates,
                            write_source,
                            write_bytes,
                            source_partition,
                            applied_delta_watermarks,
                            applied_data_delta_watermarks,
                            applied_data_delta_ids,
                            raft_apply_marker,
                            raft_nats_outbox_delta,
                            raft_state_machine_index,
                            result,
                            ..
                        } => {
                            // Publish replica delta — same position in the
                            // pipeline as local commits (Raft pattern).
                            self.dequeue_snapshot(pending_commit_id);
                            let writes = crate::write_log::index_keys_from_document_updates(
                                &remapped_updates,
                                &snapshot.index_registry,
                            );
                            if !writes.is_empty() {
                                self.log.append(commit_ts, writes, write_source);
                            }
                            if let Some(frontiers) = applied_delta_watermarks {
                                let frontiers = merge_partition_timestamp_max(
                                    &self.applied_delta_watermarks,
                                    frontiers,
                                );
                                self.persistence
                                    .write_persistence_global(
                                        PersistenceGlobalKey::AppliedDeltaWatermarks,
                                        partition_timestamp_map_to_json(&frontiers),
                                    )
                                    .await?;
                                self.applied_delta_watermarks = frontiers;
                            }
                            if let Some(frontiers) = applied_data_delta_watermarks {
                                let frontiers = merge_partition_timestamp_max(
                                    &self.applied_data_delta_watermarks,
                                    frontiers,
                                );
                                self.persistence
                                    .write_persistence_global(
                                        PersistenceGlobalKey::AppliedDataDeltaWatermarks,
                                        partition_timestamp_map_to_json(&frontiers),
                                    )
                                    .await?;
                                self.applied_data_delta_watermarks = frontiers;
                            }
                            if let Some(applied_ids) = applied_data_delta_ids {
                                self.applied_data_delta_ids = merge_applied_data_delta_ids(
                                    &self.applied_data_delta_ids,
                                    applied_ids,
                                );
                                self.applied_data_delta_ids
                                    .prune_origin_timestamps_below_watermarks(
                                        &self.applied_data_delta_watermarks,
                                    );
                                let applied_data_delta_ids_json =
                                    applied_data_delta_ids_to_json(&self.applied_data_delta_ids);
                                self.persistence
                                    .write_persistence_global(
                                        PersistenceGlobalKey::AppliedDataDeltaIds,
                                        applied_data_delta_ids_json,
                                    )
                                    .await?;
                            }
                            if let Some(marker) = raft_apply_marker {
                                anyhow::ensure!(
                                    self.applied_data_delta_ids.contains_origin_timestamp(
                                        marker.source_partition,
                                        marker.origin_ts,
                                    ),
                                    "Raft apply marker {} was not consolidated before cleanup",
                                    marker.key(),
                                );
                                self.persistence
                                    .delete_persistence_global_raw(&marker.key())
                                    .await?;
                                self.raft_apply_markers.remove(&marker);
                            }
                            if let Some(raft_state_machine_index) = raft_state_machine_index {
                                let durable_index = load_state_machine_index(
                                    self.persistence.reader().as_ref(),
                                )
                                .await?;
                                anyhow::ensure!(
                                    durable_index >= raft_state_machine_index,
                                    "Raft apply at index {} persisted without its state-machine \
                                     binding (durable index {})",
                                    raft_state_machine_index,
                                    durable_index,
                                );
                            }
                            if let Some(delta) = raft_nats_outbox_delta.as_ref() {
                                Self::add_raft_nats_outbox_delta(
                                    self.persistence.clone(),
                                    delta,
                                )
                                .await?;
                            }
                            {
                                let mut sm = self.snapshot_manager.write();
                                sm.push(commit_ts, snapshot, write_bytes);
                                if let Some(source_partition) = source_partition {
                                    if sm
                                        .replication_frontier(source_partition)
                                        .is_none_or(|current| current < commit_ts)
                                    {
                                        sm.update_replication_frontier(
                                            source_partition,
                                            commit_ts,
                                        )?;
                                    }
                                    if sm
                                        .replication_write_frontier(source_partition)
                                        .is_none_or(|current| current < commit_ts)
                                    {
                                        sm.update_replication_write_frontier(
                                            source_partition,
                                            commit_ts,
                                        )?;
                                    }
                                    let origin_watermark = self
                                        .applied_delta_watermarks
                                        .get(&source_partition)
                                        .copied()
                                        .unwrap_or(remote_ts);
                                    sm.update_applied_delta_watermark(
                                        source_partition,
                                        origin_watermark,
                                    )?;
                                }
                            }
                            let committed_prepared_txn = if source_partition
                                == Some(self.local_partition_for_two_phase())
                            {
                                self.remove_prepared_transaction_at_origin_ts(remote_ts)?
                            } else {
                                None
                            };
                            if let Some(transaction_id) = committed_prepared_txn {
                                if let Err(err) = Self::delete_two_phase_redo(
                                    self.persistence.clone(),
                                    &transaction_id,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        "2PC Raft Commit Apply: committed txn={} but failed to \
                                         delete redo: {err:#}",
                                        transaction_id,
                                    );
                                }
                            }
                            if raft_apply_marker.is_some() {
                                self.runtime
                                    .pause_client()
                                    .wait(AFTER_RAFT_SNAPSHOT_PUBLICATION)
                                    .await;
                            }
                            let _ = result.send(Ok(commit_ts));
                        },
                        PersistenceWrite::RaftReplayRecovery {
                            marker,
                            delta,
                            raft_state_machine_index,
                            result,
                            ..
                        } => {
                            let recovery_result = async {
                                let durable_index = load_state_machine_index(
                                    self.persistence.reader().as_ref(),
                                )
                                .await?;
                                anyhow::ensure!(
                                    durable_index >= raft_state_machine_index,
                                    "Raft replay at index {} found state without its durable \
                                     state-machine binding (durable index {})",
                                    raft_state_machine_index,
                                    durable_index,
                                );
                                if self.raft_apply_markers.contains(&marker) {
                                    self.consolidate_raft_apply_marker(marker).await?;
                                }
                                if Self::should_record_raft_nats_outbox_delta(&delta) {
                                    Self::add_raft_nats_outbox_delta(
                                        self.persistence.clone(),
                                        &delta,
                                    )
                                    .await?;
                                }
                                if delta.source_partition
                                    == Some(self.local_partition_for_two_phase())
                                    && let Some(transaction_id) = self
                                        .remove_prepared_transaction_at_origin_ts(delta.ts)?
                                {
                                    Self::delete_two_phase_redo(
                                        self.persistence.clone(),
                                        &transaction_id,
                                    )
                                    .await?;
                                }
                                anyhow::Ok(marker.origin_ts)
                            }
                            .await;
                            let _ = result.send(recovery_result);
                        },
                        PersistenceWrite::RemoteReadFrontierHeartbeat {
                            commit_ts,
                            source_partition,
                            applied_delta_watermarks,
                            result,
                            ..
                        } => {
                            let applied_delta_watermarks = merge_partition_timestamp_max(
                                &self.applied_delta_watermarks,
                                applied_delta_watermarks,
                            );
                            self.persistence
                                .write_persistence_global(
                                    PersistenceGlobalKey::AppliedDeltaWatermarks,
                                    partition_timestamp_map_to_json(&applied_delta_watermarks),
                                )
                                .await?;
                            let origin_watermark = applied_delta_watermarks
                                .get(&source_partition)
                                .copied()
                                .unwrap_or(commit_ts);
                            self.applied_delta_watermarks = applied_delta_watermarks;
                            self.publish_replication_frontier_progress(
                                commit_ts,
                                source_partition,
                                origin_watermark,
                            )?;
                            let _ = result.send(Ok(commit_ts));
                        },
                        PersistenceWrite::InstallSnapshot {
                            source,
                            snapshot_ts,
                            snapshot,
                            replication_frontiers,
                            applied_delta_watermarks,
                            applied_data_delta_watermarks,
                            applied_data_delta_ids,
                            result,
                            ..
                        } => {
                            self.pending_writes = PendingWrites::new();
                            self.prepared_writes = PreparedWrites::new();
                            self.prepared_transactions.clear();
                            self.queued_snapshots.clear();
                            self.log.reset(snapshot_ts);
                            self.last_assigned_ts = snapshot_ts;
                            self.read_barrier_floor = snapshot_ts;
                            self.closed_read_timestamps.clear();
                            self.applied_delta_watermarks = applied_delta_watermarks.clone();
                            self.applied_data_delta_watermarks = applied_data_delta_watermarks;
                            self.applied_data_delta_ids = applied_data_delta_ids;
                            self.snapshot_manager.write().replace(
                                snapshot_ts,
                                snapshot,
                                replication_frontiers,
                                applied_delta_watermarks,
                            );
                            self.raft_apply_markers.clear();
                            if source == SnapshotInstallSource::Raft {
                                self.recover_two_phase_redo_records().await?;
                            }
                            snapshot_install_in_progress = false;
                            let _ = result.send(Ok(snapshot_ts));
                        },
                    }
                    // Report the trace if it is longer than the threshold
                    if let Some(id) = span_commit_id && id == pending_commit_id
                        && let Some(span) = committer_span.take() {
                            if span.elapsed() < Some(*COMMIT_TRACE_THRESHOLD) {
                                tracing::debug!("Not sending span to honeycomb because it is below the threshold");
                                span.cancel();
                            } else {
                                tracing::debug!("Sending trace to honeycomb");
                            }
                        }
                }
                maybe_message = receive_message_fut.fuse() => {
                    match maybe_message {
                        None => {
                            tracing::info!("All clients have gone away, shutting down committer...");
                            return Ok(());
                        },
                        Some(CommitterMessage::Commit {
                            queue_timer,
                            transaction,
                            result,
                            write_source,
                            parent_trace,
                        }) => {

                            let parent_span = initialize_root_from_parent("handle_commit_message", parent_trace.clone())
                                .with_property(|| ("time_in_queue_ms", format!("{}", queue_timer.elapsed().as_secs_f64() * 1000.0)));
                            let committer_span_ref = committer_span.get_or_insert_with(|| {
                                span_commit_id = Some(commit_id);
                                Span::root("commit", SpanContext::random())
                            });
                            let start_commit_span =
                                Span::enter_with_parents("start_commit", [committer_span_ref, &parent_span]);
                            let _guard = start_commit_span.set_local_parent();
                            drop(queue_timer);
                            if let Some(persistence_write_future) = self.start_commit(transaction,
                                result,
                                write_source,
                                parent_trace,
                                commit_id,
                                committer_span_ref) {
                                    self.persistence_writes.push_back(persistence_write_future);
                                    commit_id += 1;
                            } else if span_commit_id == Some(commit_id) {
                                // If the span_commit_id is the same as the commit_id, that means we created a root span in this block
                                // and it didn't get incremented, so it's not a write to persistence and we should not trace it.
                                // We also need to reset the span_commit_id and committer_span.
                                committer_span_ref.cancel();
                                committer_span = None;
                                span_commit_id = None;
                            }
                        },
                        Some(CommitterMessage::ApplyReplicaDelta {
                            delta,
                            mode,
                            raft_state_machine_index,
                            transport_id,
                            result,
                        }) => {
                            if let Err(e) = self.apply_replica_delta(
                                delta,
                                mode,
                                raft_state_machine_index,
                                transport_id,
                                result,
                                commit_id,
                            ) {
                                tracing::error!("Failed to queue replica delta for apply: {e:#}");
                            } else {
                                commit_id += 1;
                            }
                        },
                        Some(CommitterMessage::ApplyRaftPreparedRedo {
                            raft_state_machine_index,
                            redo,
                            result,
                        }) => {
                            let deferred_prepare = DeferredPrepare::ApplyRaftPreparedRedo {
                                raft_state_machine_index,
                                redo,
                                result,
                            };
                            if let Some(min_pending_ts) = self.prepare_admission_blocker_ts() {
                                Self::defer_prepare_admission(
                                    &mut deferred_prepares,
                                    deferred_prepare,
                                    min_pending_ts,
                                );
                            } else {
                                self.handle_deferred_prepare(deferred_prepare).await;
                            }
                        },
                        Some(CommitterMessage::SetRaftState { raft_state, result }) => {
                            tracing::info!(
                                "Committer attached to Raft partition {} (leader={}, leader_id={})",
                                raft_state.partition_id(),
                                raft_state.is_leader(),
                                raft_state.leader_id(),
                            );
                            self.raft_state = Some(raft_state);
                            let _ = result.send(());
                        },
                        Some(CommitterMessage::InstallSnapshot { checkpoint, result }) => {
                            anyhow::ensure!(
                                pending_snapshot_install.is_none(),
                                "multiple snapshot installs were admitted concurrently",
                            );
                            pending_snapshot_install = Some(SnapshotInstallRequest {
                                checkpoint,
                                source: SnapshotInstallSource::Bootstrap,
                                result,
                            });
                        },
                        Some(CommitterMessage::InstallRaftSnapshot { checkpoint, result }) => {
                            anyhow::ensure!(
                                pending_snapshot_install.is_none(),
                                "multiple snapshot installs were admitted concurrently",
                            );
                            pending_snapshot_install = Some(SnapshotInstallRequest {
                                checkpoint,
                                source: SnapshotInstallSource::Raft,
                                result,
                            });
                        },
                        Some(CommitterMessage::BuildRaftSnapshot {
                            raft_index,
                            raft_term,
                            result,
                        }) => {
                            anyhow::ensure!(
                                pending_raft_snapshot.is_none(),
                                "multiple Raft snapshot barriers were admitted concurrently",
                            );
                            if self.persistence_writes.is_empty() {
                                pending_raft_snapshot = Some(RaftSnapshotRequest {
                                    raft_index,
                                    raft_term,
                                    result,
                                });
                            } else {
                                let _ = result.send(Err(
                                    RaftSnapshotTemporarilyUnavailable.into(),
                                ));
                            }
                        },
                        #[cfg(any(test, feature = "testing"))]
                        Some(CommitterMessage::BumpMaxRepeatableTs { result }) => {
                            let span = Span::noop();
                            if self.bump_max_repeatable_ts(result, commit_id, &span) {
                                commit_id += 1;
                            }
                        },
                        #[cfg(any(test, feature = "testing"))]
                        Some(CommitterMessage::TickIdleRemoteReadFrontierHeartbeat { result }) => {
                            self.maybe_publish_idle_remote_read_frontier_heartbeat(
                                &mut last_remote_read_frontier_heartbeat_ts,
                            );
                            let _ = result.send(());
                        },
                        Some(CommitterMessage::FinishTextAndVectorBootstrap {
                            bootstrapped_indexes,
                            bootstrap_ts,
                            result,
                        }) => {
                            self.finish_search_and_vector_bootstrap(
                                bootstrapped_indexes,
                                bootstrap_ts,
                                result
                            ).await;
                        },
                        Some(CommitterMessage::FinishTableSummaryBootstrap {
                            result,
                        }) => {
                            self.finish_table_summary_bootstrap(result).await;
                        },
                        Some(CommitterMessage::LoadIndexesIntoMemory {
                            tables, result
                        }) => {
                            let response = self.load_indexes_into_memory(tables).await;
                            let _ = result.send(response);
                        },
                        Some(CommitterMessage::Prepare {
                            transaction_id,
                            transaction,
                            write_source,
                            result,
                        }) => {
                            let deferred_prepare = DeferredPrepare::Prepare {
                                transaction_id,
                                transaction,
                                write_source,
                                result,
                            };
                            if let Some(min_pending_ts) = self.prepare_admission_blocker_ts() {
                                Self::defer_prepare_admission(
                                    &mut deferred_prepares,
                                    deferred_prepare,
                                    min_pending_ts,
                                );
                            } else {
                                self.handle_deferred_prepare(deferred_prepare).await;
                            }
                        },
                        Some(CommitterMessage::PrepareRemote {
                            transaction_id,
                            transaction,
                            write_source,
                            prepare_ts,
                            participants,
                            result,
                        }) => {
                            let deferred_prepare = DeferredPrepare::PrepareRemote {
                                transaction_id,
                                transaction,
                                write_source,
                                prepare_ts,
                                participants,
                                result,
                            };
                            if let Some(min_pending_ts) = self.prepare_admission_blocker_ts() {
                                Self::defer_prepare_admission(
                                    &mut deferred_prepares,
                                    deferred_prepare,
                                    min_pending_ts,
                                );
                            } else {
                                self.handle_deferred_prepare(deferred_prepare).await;
                            }
                        },
                        Some(CommitterMessage::ValidateRemoteReads {
                            transaction_id,
                            transaction,
                            write_source,
                            validate_ts,
                            result,
                        }) => {
                            let r = self.handle_validate_remote_reads(
                                transaction_id,
                                transaction,
                                write_source,
                                validate_ts,
                            );
                            let _ = result.send(r);
                        },
                        Some(CommitterMessage::AllocateCommitTs { min_ts, result }) => {
                            let r = self
                                .ensure_leader_for_writes()
                                .and_then(|_| self.next_commit_ts_at_or_after(min_ts));
                            let _ = result.send(r);
                        },
                        Some(CommitterMessage::CloseReadTimestamp {
                            target,
                            include_latest_floor,
                            result,
                        }) => {
                            let span = Span::noop();
                            self.close_read_timestamp(
                                target,
                                include_latest_floor,
                                result,
                                commit_id,
                                &span,
                            );
                            commit_id += 1;
                        },
                        Some(CommitterMessage::CommitPrepared {
                            transaction_id,
                            result,
                        }) => {
                            if let Some(persistence_write_future) =
                                self.start_commit_prepared(transaction_id, result, commit_id)
                            {
                                self.persistence_writes.push_back(persistence_write_future);
                                commit_id += 1;
                            }
                        },
                        Some(CommitterMessage::RollbackPrepared {
                            transaction_id,
                            result,
                        }) => {
                            if let Some(persistence_write_future) =
                                self.start_rollback_prepared_through_raft(
                                    transaction_id,
                                    result,
                                    commit_id,
                                )
                            {
                                self.persistence_writes.push_back(persistence_write_future);
                                commit_id += 1;
                            }
                        },
                        Some(CommitterMessage::ApplyRaftRollback {
                            raft_state_machine_index,
                            transaction_id,
                            result,
                        }) => {
                            let r = self
                                .handle_raft_rollback(
                                    raft_state_machine_index,
                                    transaction_id,
                                )
                                .await;
                            let _ = result.send(r);
                        },
                    }
                },
            }
        }
    }

    async fn update_indexes_since_bootstrap(
        BootstrappedSearchIndexes {
            text_index_manager,
            vector_index_manager,
            tables_with_indexes,
        }: &mut BootstrappedSearchIndexes,
        bootstrap_ts: Timestamp,
        persistence: RepeatablePersistence,
        registry: &IndexRegistry,
    ) -> anyhow::Result<()> {
        let _timer = bootstrap_update_timer();
        anyhow::ensure!(
            !text_index_manager.is_bootstrapping(),
            "Trying to update search index while it's still bootstrapping"
        );
        anyhow::ensure!(
            !vector_index_manager.is_bootstrapping(),
            "Trying to update vector index while it's still bootstrapping"
        );
        let range = TimestampRange::new((Bound::Excluded(bootstrap_ts), Bound::Unbounded));

        let revision_stream =
            stream_revision_pairs_for_indexes(tables_with_indexes, &persistence, range);
        futures::pin_mut!(revision_stream);

        let mut num_revisions = 0;
        let mut total_size = 0;
        while let Some(revision_pair) = revision_stream.try_next().await? {
            num_revisions += 1;
            total_size += revision_pair.document().map(|d| d.size()).unwrap_or(0);
            text_index_manager.update(
                registry,
                revision_pair.prev_document(),
                revision_pair.document(),
                WriteTimestamp::Committed(revision_pair.ts()),
            )?;
            vector_index_manager.update(
                registry,
                revision_pair.prev_document(),
                revision_pair.document(),
                WriteTimestamp::Committed(revision_pair.ts()),
            )?;
        }
        finish_bootstrap_update(num_revisions, total_size);
        Ok(())
    }

    async fn finish_search_and_vector_bootstrap(
        &mut self,
        mut bootstrapped_indexes: BootstrappedSearchIndexes,
        bootstrap_ts: RepeatableTimestamp,
        result: oneshot::Sender<anyhow::Result<()>>,
    ) {
        let (last_snapshot, latest_ts) = {
            let snapshot_manager = self.snapshot_manager.read();
            (
                snapshot_manager.latest_snapshot(),
                snapshot_manager.latest_ts(),
            )
        };
        if latest_ts > bootstrap_ts {
            let repeatable_persistence = RepeatablePersistence::new(
                self.persistence.reader(),
                latest_ts,
                self.retention_validator.clone(),
            );

            let res = Self::update_indexes_since_bootstrap(
                &mut bootstrapped_indexes,
                *bootstrap_ts,
                repeatable_persistence,
                &last_snapshot.index_registry,
            )
            .await;
            if res.is_err() {
                let _ = result.send(res);
                return;
            }
        }
        // Committer is currently single threaded, so commits should be blocked until we
        // finish and the timestamp shouldn't be able to advance.
        let mut snapshot_manager = self.snapshot_manager.write();
        if latest_ts != snapshot_manager.latest_ts() {
            panic!("Snapshots were changed concurrently during commit?");
        }
        snapshot_manager.overwrite_last_snapshot_text_and_vector_indexes(
            bootstrapped_indexes.text_index_manager,
            bootstrapped_indexes.vector_index_manager,
            &mut self.pending_writes,
        );

        tracing::info!("Committed backfilled vector indexes");
        let _ = result.send(Ok(()));
    }

    async fn finish_table_summary_bootstrap(
        &mut self,
        result: oneshot::Sender<anyhow::Result<()>>,
    ) {
        let _timer = table_summary_finish_bootstrap_timer();
        let latest_ts = {
            let snapshot_manager = self.snapshot_manager.read();
            snapshot_manager.latest_ts()
        };
        // This gets called by the TableSummaryWorker when it has successfully
        // checkpointed a TableSummarySnapshot.
        // Walk any changes since the last checkpoint, and update the snapshot manager
        // with the new TableSummarySnapshot.
        let bootstrap_result = table_summary::bootstrap(
            self.runtime.clone(),
            self.persistence.reader(),
            self.retention_validator.clone(),
            latest_ts,
            table_summary::BootstrapKind::FromCheckpoint,
        )
        .await;
        let (table_summary_snapshot, _) = match bootstrap_result {
            Ok(res) => res,
            Err(err) => {
                let _ = result.send(Err(err));
                return;
            },
        };
        // Committer is currently single threaded, so commits should be blocked until we
        // finish and the timestamp shouldn't be able to advance.
        let mut snapshot_manager = self.snapshot_manager.write();
        if latest_ts != snapshot_manager.latest_ts() {
            panic!("Snapshots were changed concurrently during commit?");
        }
        if let Err(e) = snapshot_manager
            .overwrite_last_snapshot_table_summary(table_summary_snapshot, &mut self.pending_writes)
        {
            let _ = result.send(Err(e));
            return;
        }
        tracing::info!("Bootstrapped table summaries at ts {}", latest_ts);
        let _ = result.send(Ok(()));
    }

    // This blocks the committer and loads the in-memory indexes for the latest
    // snapshot in memory. A potential further improvement is to pick a base
    // timestamp and load the indexes at that timestamp outside of the committer.
    // The committer can then replay recent writes to derive the latest in-memory
    // indexes. This would either need to do another database query for the log
    // or rely on the write log to not have trimmed the base timestamp yet.
    async fn load_indexes_into_memory(
        &mut self,
        tables: BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        let (last_snapshot, latest_ts) = {
            let snapshot_manager = self.snapshot_manager.read();
            (
                snapshot_manager.latest_snapshot(),
                snapshot_manager.latest_ts(),
            )
        };

        let repeatable_persistence = RepeatablePersistence::new(
            self.persistence.reader(),
            latest_ts,
            self.retention_validator.clone(),
        );
        let mut in_memory_indexes = last_snapshot.in_memory_indexes.clone();
        in_memory_indexes
            .load_enabled_for_tables(
                &last_snapshot.index_registry,
                last_snapshot.table_mapping(),
                &repeatable_persistence.read_snapshot(latest_ts)?,
                &tables,
            )
            .await?;

        // Committer is currently single threaded, so commits should be blocked until we
        // finish and the timestamp shouldn't be able to advance.
        let mut snapshot_manager = self.snapshot_manager.write();
        if latest_ts != snapshot_manager.latest_ts() {
            panic!("Snapshots were changed concurrently during commit?");
        }
        snapshot_manager
            .overwrite_last_snapshot_in_memory_indexes(in_memory_indexes, &mut self.pending_writes);

        tracing::info!("Loaded indexes into memory");
        Ok(())
    }

    fn bump_max_repeatable_ts(
        &mut self,
        result: oneshot::Sender<Timestamp>,
        commit_id: usize,
        root_span: &Span,
    ) -> bool {
        let timer = metrics::bump_repeatable_ts_timer();
        // next_max_repeatable_ts bumps the last_assigned_ts, so all future commits on
        // this committer will be after new_max_repeatable.
        let new_max_repeatable = match self.try_next_max_repeatable_ts() {
            Ok(Some(ts)) => ts,
            Ok(None) => {
                let current = *self.snapshot_manager.read().persisted_max_repeatable_ts();
                let _ = result.send(current);
                return false;
            },
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "Skipping optional max repeatable timestamp maintenance",
                );
                let current = *self.snapshot_manager.read().persisted_max_repeatable_ts();
                let _ = result.send(current);
                return false;
            },
        };
        self.enqueue_max_repeatable_ts(
            new_max_repeatable,
            RepeatableTimestampResult::Background(result),
            commit_id,
            root_span,
            timer,
        );
        true
    }

    fn close_read_timestamp(
        &mut self,
        target: Timestamp,
        include_latest_floor: bool,
        result: oneshot::Sender<anyhow::Result<ReadTimestampClosure>>,
        commit_id: usize,
        root_span: &Span,
    ) {
        if let Err(err) = self.ensure_leader_for_read_barrier() {
            let _ = result.send(Err(err));
            return;
        }
        // An exact historical read may reuse a timestamp that was fenced before
        // later commits. A latest-read barrier must first discover those later
        // commits so it cannot mistake an old cached fence for the cluster tip.
        if !include_latest_floor && self.closed_read_timestamps.contains(&target) {
            let _ = result.send(Ok(ReadTimestampClosure::Closed(target)));
            return;
        }

        let latest_ts = *self.snapshot_manager.read().latest_ts();
        let mut minimum = cmp::max(
            self.last_assigned_ts,
            cmp::max(self.log.max_ts(), latest_ts),
        );
        if let Some((queued_ts, _)) = self.latest_queued_snapshot() {
            minimum = cmp::max(minimum, queued_ts);
        }
        if let Some((pending_ts, _)) = self.pending_writes.latest() {
            minimum = cmp::max(minimum, pending_ts);
        }
        if let Some(prepared_ts) = self.prepared_writes.max_ts() {
            minimum = cmp::max(minimum, prepared_ts);
        }

        if minimum > target {
            let _ = result.send(Ok(ReadTimestampClosure::RetryAt(minimum)));
            return;
        }
        if self.closed_read_timestamps.contains(&target) {
            let _ = result.send(Ok(ReadTimestampClosure::Closed(target)));
            return;
        }
        if let Some(prepared_ts) = self.prepared_writes.min_ts()
            && prepared_ts <= target
        {
            let _ = result.send(Ok(ReadTimestampClosure::Blocked(prepared_ts)));
            return;
        }

        // From this point onward every locally allocated commit is above the
        // barrier. Persistence writes already queued before this message are
        // ordered before the barrier by `FuturesOrdered`.
        self.last_assigned_ts = target;
        self.read_barrier_floor = target;
        self.enqueue_max_repeatable_ts(
            target,
            RepeatableTimestampResult::ClusterReadBarrier(result),
            commit_id,
            root_span,
            metrics::bump_repeatable_ts_timer(),
        );
    }

    fn enqueue_max_repeatable_ts(
        &mut self,
        new_max_repeatable: Timestamp,
        result: RepeatableTimestampResult,
        commit_id: usize,
        root_span: &Span,
        timer: Timer<VMHistogram>,
    ) {
        let persistence = self.persistence.clone();
        let span = Span::enter_with_parent("bump_max_repeatable_ts", root_span);
        let runtime = self.runtime.clone();
        self.persistence_writes.push_back(
            async move {
                // The MaxRepeatableTimestamp persistence global ensures all future
                // commits on future leaders will be after new_max_repeatable, and followers
                // can know this timestamp is repeatable.

                // If we fail to bump the timestamp, we'll backoff and retry
                // which will block the committer from making forward progress until we
                // succceed.  We don't want to kill the committer and reload the
                // instance if we can avoid it, as that would exacerbate any
                // load-related issues.
                let mut backoff = Backoff::new(
                    INITIAL_PERSISTENCE_WRITES_BACKOFF,
                    MAX_PERSISTENCE_WRITES_BACKOFF,
                );
                loop {
                    match persistence
                        .write_persistence_global(
                            PersistenceGlobalKey::MaxRepeatableTimestamp,
                            new_max_repeatable.into(),
                        )
                        .await
                    {
                        Ok(()) => {
                            backoff.reset();
                            break;
                        },
                        Err(mut e) => {
                            let delay = backoff.fail(&mut runtime.rng());
                            report_error(&mut e).await;
                            tracing::error!(
                                "Failed to bump max repeatable timestamp, retrying after {:.2}s",
                                delay.as_secs_f32()
                            );
                            runtime.wait(delay).await;
                            continue;
                        },
                    }
                }
                Ok(PersistenceWrite::MaxRepeatableTimestamp {
                    new_max_repeatable,
                    timer,
                    result,
                    commit_id,
                })
            }
            .in_span(span)
            .boxed(),
        );
    }

    fn publish_max_repeatable_ts(&mut self, new_max_repeatable: Timestamp) -> anyhow::Result<()> {
        // Bump the latest snapshot in snapshot_manager so reads on this leader
        // can know this timestamp is repeatable.
        let mut snapshot_manager = self.snapshot_manager.write();
        let advanced = snapshot_manager.bump_persisted_max_repeatable_ts(new_max_repeatable)?;
        if advanced {
            self.log.append(
                new_max_repeatable,
                WithHeapSize::default(),
                "publish_max_repeatable_ts".into(),
            );
        }
        drop(snapshot_manager);
        if advanced {
            self.publish_remote_read_frontier_heartbeat(new_max_repeatable);
        }
        Ok(())
    }

    fn publish_replication_frontier_progress(
        &mut self,
        frontier_ts: Timestamp,
        source_partition: crate::partition::PartitionId,
        origin_watermark: Timestamp,
    ) -> anyhow::Result<()> {
        // Idle replica heartbeats advance the safe frontier for remote reads,
        // but they do not publish a newer readable snapshot. Keep that
        // follower-read frontier separate from the persisted repeatable
        // snapshot timestamp so we do not advertise a snapshot that does not
        // exist.
        let mut snapshot_manager = self.snapshot_manager.write();
        if snapshot_manager
            .replication_frontier(source_partition)
            .is_none_or(|current| current < frontier_ts)
        {
            snapshot_manager.update_replication_frontier(source_partition, frontier_ts)?;
        }
        snapshot_manager.update_applied_delta_watermark(source_partition, origin_watermark)?;
        Ok(())
    }

    fn maybe_publish_idle_remote_read_frontier_heartbeat(
        &mut self,
        last_remote_read_frontier_heartbeat_ts: &mut Timestamp,
    ) {
        match self.idle_remote_read_frontier_heartbeat_ts() {
            Ok(Some(frontier_ts)) => {
                if frontier_ts > *last_remote_read_frontier_heartbeat_ts {
                    self.publish_remote_read_frontier_heartbeat(frontier_ts);
                    *last_remote_read_frontier_heartbeat_ts = frontier_ts;
                }
            },
            Ok(None) => {},
            Err(e) => {
                let local_partition = self
                    .placement_state
                    .as_ref()
                    .map(|placement_state| placement_state.local_partition());
                tracing::warn!(
                    ?local_partition,
                    error = %format!("{e:#}"),
                    "Skipping idle remote read frontier heartbeat"
                );
            },
        }
    }

    fn publish_remote_read_frontier_heartbeat(&self, frontier_ts: Timestamp) {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return;
        };
        let partition_map = placement_state.partition_map();
        if partition_map.num_partitions() <= 1 {
            return;
        }
        if self
            .raft_state
            .as_ref()
            .is_some_and(|raft_state| !raft_state.can_serve_as_leader())
        {
            return;
        }

        let delta = CommitDelta {
            ts: frontier_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("remote_read_frontier_heartbeat"),
            write_bytes: 0,
            tablet_id_to_table_name: BTreeMap::new(),
            source_partition: Some(partition_map.local_partition()),
        };
        let distributed_log = self.distributed_log.clone();
        tokio_spawn("publish_remote_read_frontier_heartbeat", async move {
            if let Err(e) = distributed_log.publish(delta).await {
                tracing::error!(
                    "Failed to publish remote read frontier heartbeat at ts={}: {e:#}",
                    u64::from(frontier_ts),
                );
            }
        });
    }

    fn idle_remote_read_frontier_heartbeat_ts(&mut self) -> anyhow::Result<Option<Timestamp>> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return Ok(None);
        };
        let partition_map = placement_state.partition_map();
        if partition_map.num_partitions() <= 1 {
            return Ok(None);
        }
        if self
            .raft_state
            .as_ref()
            .is_some_and(|raft_state| !raft_state.can_serve_as_leader())
        {
            return Ok(None);
        }

        self.try_next_max_repeatable_ts()
    }

    fn ensure_leader_for_writes(&self) -> anyhow::Result<()> {
        if let Some(ref raft) = self.raft_state {
            anyhow::ensure!(
                raft.is_cluster_genesis_ready(),
                "Cluster genesis is not ready for partition {}; retry after bootstrap completes",
                raft.partition_id(),
            );
            if !raft.is_leader_ready() {
                let leader = raft.leader_id();
                metrics::log_write_rejected_not_leader(raft.partition_id());
                anyhow::bail!(
                    "Raft node is not ready to accept writes for partition {}. Current leader: \
                     node {}. Forward to the ready leader or retry after catch-up.",
                    raft.partition_id(),
                    leader,
                );
            }
        }
        Ok(())
    }

    fn ensure_leader_for_read_barrier(&self) -> anyhow::Result<()> {
        self.ensure_leader_for_writes()?;
        if let Some(raft) = self.raft_state.as_ref() {
            anyhow::ensure!(
                raft.has_leader_serving_lease(),
                "Raft leader {} for partition {} has no valid serving lease",
                raft.node_id(),
                raft.partition_id(),
            );
        }
        Ok(())
    }

    fn ensure_prepare_ownership(
        &self,
        writes: &[DocumentUpdateWithPrevTs],
        table_mapping: &TableMapping,
        write_source: &WriteSource,
    ) -> anyhow::Result<()> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return Ok(());
        };
        let partition_map = placement_state.partition_map();

        for write in writes {
            let tablet_id = write.id.tablet_id;
            let table_name = table_mapping
                .tablet_name(tablet_id)
                .with_context(|| format!("Missing table mapping for prepared write {tablet_id}"))?;
            if let Some(partition) = crate::partition::routed_partition_for_table(
                &table_name,
                &partition_map,
                write_source,
            ) && partition != partition_map.local_partition()
            {
                anyhow::bail!(
                    "Write to table '{}' rejected during prepare: owned by {}, not this node \
                     ({}). Coordinator routed this transaction to the wrong participant.",
                    table_name,
                    partition,
                    partition_map.local_partition(),
                );
            }
        }
        Ok(())
    }

    fn ensure_read_ownership(
        &self,
        reads: &ReadSet,
        table_mapping: &TableMapping,
        write_source: &WriteSource,
    ) -> anyhow::Result<()> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return Ok(());
        };
        let partition_map = placement_state.partition_map();

        let validate_tablet = |tablet_id| -> anyhow::Result<()> {
            let table_name = table_mapping
                .tablet_name(tablet_id)
                .with_context(|| format!("Missing table mapping for validated read {tablet_id}"))?;
            if let Some(partition) = crate::partition::routed_partition_for_table(
                &table_name,
                &partition_map,
                write_source,
            ) && partition != partition_map.local_partition()
            {
                anyhow::bail!(
                    "Read validation for table '{}' rejected: owned by {}, not this node ({}). \
                     Coordinator routed this read set to the wrong participant.",
                    table_name,
                    partition,
                    partition_map.local_partition(),
                );
            }
            Ok(())
        };

        for (index_name, _) in reads.iter_indexed() {
            validate_tablet(*index_name.table())?;
        }
        for (index_name, _) in reads.iter_search() {
            validate_tablet(*index_name.table())?;
        }
        Ok(())
    }

    fn validate_remote_read_frontiers(
        &self,
        begin_ts: Timestamp,
        reads: &ReadSet,
        table_mapping: &TableMapping,
        write_source: &WriteSource,
    ) -> anyhow::Result<()> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return Ok(());
        };
        let partition_map = placement_state.partition_map();

        let stale = {
            let snapshot_manager = self.snapshot_manager.read();
            stale_replica_frontiers(
                &snapshot_manager,
                begin_ts,
                reads,
                table_mapping,
                &partition_map,
                write_source,
            )
        };
        if stale.is_empty() {
            return Ok(());
        }

        let details = stale
            .iter()
            .map(|(partition, frontier)| format!("{partition}: frontier={frontier}"))
            .join(", ");
        let metadata = ErrorMetadata::system_occ(
            Some(u64::from(begin_ts)),
            write_source.as_str().map(|source| source.to_string()),
        );
        Err(anyhow::anyhow!(metadata).context(format!(
            "Cross-partition OCC requires remote read frontiers >= {begin_ts}, but found {details}",
        )))
    }

    fn stage_prepared_transaction(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        begin_ts: Timestamp,
        reads: &ReadSet,
        mut writes: Vec<DocumentUpdateWithPrevTs>,
        table_mapping: &TableMapping,
        write_source: WriteSource,
        explicit_commit_ts: Option<Timestamp>,
        require_leader: bool,
        allow_explicit_ts_rewrite: bool,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        if require_leader {
            self.ensure_leader_for_writes()?;
        }
        self.ensure_prepare_ownership(&writes, table_mapping, &write_source)?;
        self.validate_remote_read_frontiers(begin_ts, reads, table_mapping, &write_source)?;

        let local_commit_floor = if allow_explicit_ts_rewrite {
            self.replayed_prepare_commit_floor()?
        } else {
            self.explicit_prepare_commit_floor()?
        };

        if let Some(existing) = self.prepared_transactions.get(&transaction_id) {
            if let Some(commit_ts) = explicit_commit_ts {
                anyhow::ensure!(
                    existing.origin_commit_ts == commit_ts,
                    "2PC Prepare retried with a different commit timestamp for {}",
                    transaction_id
                );
            }
            return Ok(crate::two_phase::PrepareResult {
                prepare_ts: existing.origin_commit_ts,
            });
        }
        if let Some(min_pending_ts) = self.pending_writes.min_ts() {
            let metadata = ErrorMetadata::system_occ(
                Some(u64::from(begin_ts)),
                write_source.as_str().map(|source| source.to_string()),
            );
            anyhow::bail!(anyhow::anyhow!(metadata).context(format!(
                "2PC Prepare for txn={} blocked by an unresolved pending write at ts={}; retry \
                 after the pending write publishes",
                transaction_id,
                u64::from(min_pending_ts),
            )));
        }

        let (origin_commit_ts, commit_ts) = if let Some(commit_ts) = explicit_commit_ts {
            let local_commit_ts = if commit_ts >= local_commit_floor {
                commit_ts
            } else if allow_explicit_ts_rewrite {
                tracing::info!(
                    "2PC Raft Prepare Apply: staging origin ts={} at local ts={} because the \
                     follower snapshot floor has advanced",
                    u64::from(commit_ts),
                    u64::from(local_commit_floor),
                );
                local_commit_floor
            } else {
                return Err(crate::two_phase::PrepareTimestampTooLow {
                    proposed_ts: commit_ts,
                    required_ts: local_commit_floor,
                }
                .into());
            };
            (commit_ts, local_commit_ts)
        } else {
            let commit_ts = self.next_commit_ts()?;
            (commit_ts, commit_ts)
        };
        if commit_ts > self.last_assigned_ts {
            self.last_assigned_ts = commit_ts;
        }
        // Replay/recovery paths are reconstructing a prepare that was already
        // accepted by the leader and written durably or through Raft. Do not
        // re-scan from the original transaction begin timestamp: followers may
        // have already compacted that range even when the origin prepare
        // timestamp itself still does not need rewriting.
        let conflict_begin_ts = if allow_explicit_ts_rewrite {
            let rewritten_begin_ts = commit_ts.pred()?;
            tracing::info!(
                "2PC Raft Prepare Apply: validating replayed prepare with local begin ts={} \
                 instead of origin begin ts={} to stay within the follower write-log retention \
                 window",
                u64::from(rewritten_begin_ts),
                u64::from(begin_ts),
            );
            rewritten_begin_ts
        } else {
            begin_ts
        };
        let timer = metrics::commit_is_stale_timer();
        if let Some(conflicting_read) =
            self.commit_has_conflict(reads, conflict_begin_ts, commit_ts)?
        {
            anyhow::bail!(conflicting_read.into_error(table_mapping, &write_source));
        }
        timer.finish();

        writes.sort_by_key(|update| {
            table_dependency_sort_key(
                BootstrapTableIds::new(table_mapping),
                InternalDocumentId::from(update.id),
                update.new_document.as_ref(),
            )
        });

        let ordered_update_refs: Vec<_> = writes.iter().collect();
        let (document_writes, index_writes, snapshot) =
            self.compute_writes(commit_ts, &ordered_update_refs)?;

        let prepared_write = self
            .prepared_writes
            .insert(
                commit_ts,
                writes
                    .into_iter()
                    .map(|update| (update.id, PackedDocumentUpdate::pack(&update)))
                    .collect(),
                write_source.clone(),
                snapshot,
            )
            .with_context(|| {
                format!("2PC Prepare failed to stage prepared intent for txn={transaction_id}")
            })?;

        let mut write_bytes: u64 = 0;
        let doc_entries: Vec<DocumentLogEntry> = document_writes
            .into_iter()
            .map(|w| {
                let entry = DocumentLogEntry {
                    ts: w.commit_ts,
                    id: w.id,
                    value: w.write,
                    prev_ts: w.prev_ts,
                };
                write_bytes += entry.size();
                entry
            })
            .collect();
        let idx_entries: Vec<PersistenceIndexEntry> = index_writes
            .into_iter()
            .map(|(ts, update)| {
                let entry = PersistenceIndexEntry::from_index_update(ts, &update);
                write_bytes += entry.size();
                entry
            })
            .collect();

        self.prepared_transactions.insert(
            transaction_id.clone(),
            PreparedTransaction {
                prepared_write,
                origin_commit_ts,
                commit_ts,
                write_bytes,
                document_writes: Arc::new(doc_entries),
                index_writes: Arc::new(idx_entries),
            },
        );

        tracing::info!(
            "2PC Prepared: txn={}, ts={}",
            transaction_id,
            u64::from(commit_ts),
        );

        Ok(crate::two_phase::PrepareResult {
            prepare_ts: origin_commit_ts,
        })
    }

    /// First, check that it's valid to apply this transaction in-memory. If it
    /// passes validation, we can rebase the transaction to a new timestamp
    /// if other transactions have committed.
    #[fastrace::trace]
    fn validate_commit(
        &mut self,
        transaction: FinalTransaction,
        write_source: WriteSource,
    ) -> anyhow::Result<ValidatedCommit> {
        self.ensure_leader_for_writes()?;
        if !self.prepared_transactions.is_empty() {
            let metadata = ErrorMetadata::system_occ(
                Some(u64::from(*transaction.begin_timestamp)),
                write_source.as_str().map(|source| source.to_string()),
            );
            anyhow::bail!(anyhow::anyhow!(metadata).context(format!(
                "Commit blocked by {} unresolved 2PC prepared transaction(s); retry after the \
                 prepared transaction commits or rolls back",
                self.prepared_writes.len(),
            )));
        }
        if let Some(pending_ts) =
            self.pending_bootstrap_metadata_write_ts(&transaction.table_mapping)
        {
            let metadata = ErrorMetadata::system_occ(
                Some(u64::from(*transaction.begin_timestamp)),
                write_source.as_str().map(|source| source.to_string()),
            );
            anyhow::bail!(anyhow::anyhow!(metadata).context(format!(
                "Commit blocked by unresolved _tables/_index metadata write at ts={}; retry after \
                 the metadata write publishes",
                u64::from(pending_ts),
            )));
        }

        // Partition ownership check: verify that all authoritative writes for
        // this operation target tables owned by this node. Deploy metadata
        // operations route system tables to the metadata owner on partition 0.
        if let Some(placement_state) = self.placement_state.as_ref() {
            let partition_map = placement_state.partition_map();
            for write in transaction.writes.coalesced_writes() {
                let tablet_id = write.id.tablet_id;
                if let Ok(table_name) = transaction.table_mapping.tablet_name(tablet_id) {
                    if let Some(partition) = crate::partition::routed_partition_for_table(
                        &table_name,
                        &partition_map,
                        &write_source,
                    ) && partition != partition_map.local_partition()
                    {
                        anyhow::bail!(
                            "Write to table '{}' rejected: owned by {}, not this node ({}). Route \
                             this mutation to the correct partition owner.",
                            table_name,
                            partition,
                            partition_map.local_partition(),
                        );
                    }
                }
            }
        }

        if let Some((intent_ts, intent_source)) = self
            .prepared_writes
            .conflicts_document_ids(transaction.writes.coalesced_writes().map(|write| write.id))
        {
            let metadata = ErrorMetadata::system_occ(
                Some(u64::from(*transaction.begin_timestamp)),
                write_source.as_str().map(|source| source.to_string()),
            );
            anyhow::bail!(anyhow::anyhow!(metadata).context(format!(
                "Commit conflicts with unresolved 2PC prepared intent at ts={} from {:?}",
                u64::from(intent_ts),
                intent_source,
            )));
        }

        self.validate_remote_read_frontiers(
            *transaction.begin_timestamp,
            transaction.reads.read_set(),
            &transaction.table_mapping,
            &write_source,
        )?;

        let commit_ts = self.next_commit_ts()?;
        let timer = metrics::commit_is_stale_timer();
        if let Some(conflicting_read) = self.commit_has_conflict(
            transaction.reads.read_set(),
            *transaction.begin_timestamp,
            commit_ts,
        )? {
            anyhow::bail!(conflicting_read.into_error(&transaction.table_mapping, &write_source));
        }
        timer.finish();

        let updates: Vec<_> = transaction.writes.coalesced_writes().collect();
        // The updates are ordered using table_dependency_sort_key,
        // which is the same order they should be applied to database metadata
        // and index data structures
        let mut ordered_updates = updates;
        ordered_updates.sort_by_key(|update| {
            table_dependency_sort_key(
                BootstrapTableIds::new(&transaction.table_mapping),
                InternalDocumentId::from(update.id),
                update.new_document.as_ref(),
            )
        });

        let (document_writes, index_writes, snapshot) =
            self.compute_writes(commit_ts, &ordered_updates)?;

        // Append the updates to pending_writes, so future conflicting commits
        // will fail the `commit_has_conflict` check above, even before
        // this transaction writes to persistence or is visible to reads. Note that
        // this can cause theoretical false conflicts, where transaction has a conflict
        // with another one, and the latter never ended up committing. This
        // should be very rare, and false positives are acceptable by design.
        let timer = metrics::pending_writes_append_timer();
        let pending_write = self.pending_writes.push_back(
            commit_ts,
            ordered_updates
                .into_iter()
                .map(|update| (update.id, PackedDocumentUpdate::pack(update)))
                .collect(),
            write_source,
            snapshot,
        );
        drop(timer);

        Ok(ValidatedCommit {
            index_writes,
            document_writes,
            pending_write,
        })
    }

    fn pending_bootstrap_metadata_write_ts(
        &self,
        table_mapping: &TableMapping,
    ) -> Option<Timestamp> {
        let bootstrap_tables = BootstrapTableIds::new(table_mapping);
        self.pending_writes
            .iter(Timestamp::MIN, Timestamp::MAX)
            .find_map(|(ts, writes, _)| {
                writes
                    .into_iter()
                    .any(|(id, _)| {
                        bootstrap_tables.is_tables_table(id.tablet_id)
                            || bootstrap_tables.is_index_table(id.tablet_id)
                    })
                    .then_some(*ts)
            })
    }

    #[fastrace::trace]
    fn compute_writes(
        &self,
        commit_ts: Timestamp,
        ordered_updates: &Vec<&DocumentUpdateWithPrevTs>,
    ) -> anyhow::Result<(
        Vec<ValidatedDocumentWrite>,
        BTreeSet<(Timestamp, DatabaseIndexUpdate)>,
        Snapshot,
    )> {
        let timer = metrics::commit_prepare_writes_timer();
        let mut document_writes = Vec::new();
        let mut index_writes = Vec::new();
        // We have to compute the new snapshot from the latest pending snapshot in case
        // there are pending writes that need to be included. We have already
        // checked for conflicts, so the latest pending snapshot must
        // have the same tables and indexes as the base snapshot and the final
        // publishing snapshot. Therefore index writes can be computed from the
        // latest pending snapshot.
        let mut latest_pending_snapshot = self.base_snapshot_for_new_writes();
        for &document_update in ordered_updates.iter() {
            let (updates, vector_index_write_size, text_index_write_size) =
                latest_pending_snapshot.update(document_update, commit_ts)?;
            index_writes.extend(updates);
            document_writes.push(ValidatedDocumentWrite {
                commit_ts,
                id: document_update.id.into(),
                write: document_update.new_document.clone(),
                vector_index_write_size,
                text_index_write_size,
                prev_ts: document_update.old_document.as_ref().map(|&(_, ts)| ts),
            });
        }
        let index_writes = index_writes
            .into_iter()
            .map(|index_update| (commit_ts, index_update))
            .collect();

        timer.finish();
        Ok((document_writes, index_writes, latest_pending_snapshot))
    }

    #[fastrace::trace]
    fn commit_has_conflict(
        &self,
        reads: &ReadSet,
        reads_ts: Timestamp,
        commit_ts: Timestamp,
    ) -> anyhow::Result<Option<ConflictingReadWithWriteSource>> {
        if let Some(conflicting_read) = self.log.is_stale(reads, reads_ts, commit_ts)? {
            return Ok(Some(conflicting_read));
        }
        if let Some(conflicting_read) = self.pending_writes.is_stale(reads, reads_ts, commit_ts)? {
            return Ok(Some(conflicting_read));
        }
        if let Some(conflicting_read) = self.prepared_writes.is_stale(reads, reads_ts, commit_ts)? {
            return Ok(Some(conflicting_read));
        }
        Ok(None)
    }

    /// Commit the transaction to persistence (without the lock held).
    /// This is the commit point of a transaction. If this succeeds, the
    /// transaction must be published and made visible. If we are unsure whether
    /// the write went through, we crash the process and recover from whatever
    /// has been written to persistence.
    async fn write_to_persistence(
        persistence: Arc<dyn Persistence>,
        index_writes: Arc<Vec<PersistenceIndexEntry>>,
        document_writes: Arc<Vec<DocumentLogEntry>>,
        write_source: WriteSource,
        raft_apply_marker: Option<RaftApplyMarker>,
        raft_nats_outbox_delta: Option<CommitDelta>,
        raft_state_machine_index: Option<u64>,
    ) -> anyhow::Result<()> {
        let timer = metrics::commit_persistence_write_timer();
        let raft_apply_writes = Self::raft_apply_persistence_writes(
            raft_apply_marker,
            raft_nats_outbox_delta.as_ref(),
            raft_state_machine_index,
        )?;
        persistence
            .write_with_persistence_globals(
                document_writes.as_slice(),
                &index_writes,
                ConflictStrategy::Error,
                &raft_apply_writes,
            )
            .await
            .with_context(|| format!("Commit ({write_source:?}) failed to write to persistence"))?;

        timer.finish();
        Ok(())
    }

    async fn wait_for_raft_commit(
        raft_commit_waiter: RaftCommitWaiter,
        commit_ts: Timestamp,
        path: &'static str,
    ) -> anyhow::Result<Option<u64>> {
        let Some(raft_commit_waiter) = raft_commit_waiter else {
            return Ok(None);
        };
        let timer = metrics::commit_hot_path_stage_timer(path, "raft_quorum_wait");
        match raft_commit_waiter.await {
            Ok(RaftProposalResult::Committed { index }) => {
                timer.finish();
                Ok(Some(index))
            },
            Ok(RaftProposalResult::Rejected) => anyhow::bail!(
                "Raft rejected commit at ts={} before quorum commit",
                u64::from(commit_ts),
            ),
            Err(_) => anyhow::bail!(
                "Raft proposal result channel closed before commit at ts={} reached quorum",
                u64::from(commit_ts),
            ),
        }
    }

    async fn wait_for_raft_prepare(
        raft_commit_waiter: RaftCommitWaiter,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        prepare_ts: Timestamp,
    ) -> anyhow::Result<Option<u64>> {
        let Some(raft_commit_waiter) = raft_commit_waiter else {
            return Ok(None);
        };
        match raft_commit_waiter.await {
            Ok(RaftProposalResult::Committed { index }) => Ok(Some(index)),
            Ok(RaftProposalResult::Rejected) => anyhow::bail!(
                "Raft rejected 2PC prepare for txn={} at ts={} before quorum commit",
                transaction_id,
                u64::from(prepare_ts),
            ),
            Err(_) => anyhow::bail!(
                "Raft proposal result channel closed before 2PC prepare txn={} at ts={} reached \
                 quorum",
                transaction_id,
                u64::from(prepare_ts),
            ),
        }
    }

    async fn wait_for_raft_rollback(
        raft_commit_waiter: RaftCommitWaiter,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<Option<u64>> {
        let Some(raft_commit_waiter) = raft_commit_waiter else {
            return Ok(None);
        };
        match raft_commit_waiter.await {
            Ok(RaftProposalResult::Committed { index }) => Ok(Some(index)),
            Ok(RaftProposalResult::Rejected) => anyhow::bail!(
                "Raft rejected 2PC rollback for txn={} before quorum commit",
                transaction_id,
            ),
            Err(_) => anyhow::bail!(
                "Raft proposal result channel closed before 2PC rollback txn={} reached quorum",
                transaction_id,
            ),
        }
    }

    fn block_on_current_runtime<F: Future>(future: F) -> F::Output {
        block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            if rt.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                futures::executor::block_on(future)
            } else {
                rt.block_on(future)
            }
        })
    }

    fn fail_committed_write<T>(
        result: oneshot::Sender<anyhow::Result<T>>,
        commit_ts: Timestamp,
        operation: &'static str,
        err: anyhow::Error,
    ) -> anyhow::Result<()> {
        let message = format!(
            "Failed to {operation} after local persistence commit at ts={}: {err:#}",
            u64::from(commit_ts),
        );
        let _ = result.send(Err(anyhow::anyhow!(message.clone())));
        anyhow::bail!(message)
    }

    fn send_prepared_commit_waiters(
        &mut self,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        result: anyhow::Result<Timestamp>,
    ) {
        let Some(waiters) = self.prepared_commit_waiters.remove(transaction_id) else {
            return;
        };
        match result {
            Ok(commit_ts) => {
                for waiter in waiters {
                    let _ = waiter.send(Ok(commit_ts));
                }
            },
            Err(err) => {
                let message = format!("{err:#}");
                for waiter in waiters {
                    let _ = waiter.send(Err(anyhow::anyhow!(message.clone())));
                }
            },
        }
    }

    fn send_prepared_rollback_waiters(
        &mut self,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        result: anyhow::Result<()>,
    ) {
        let Some(waiters) = self.prepared_rollback_waiters.remove(transaction_id) else {
            return;
        };
        match result {
            Ok(()) => {
                for waiter in waiters {
                    let _ = waiter.send(Ok(()));
                }
            },
            Err(err) => {
                let message = format!("{err:#}");
                for waiter in waiters {
                    let _ = waiter.send(Err(anyhow::anyhow!(message.clone())));
                }
            },
        }
    }

    fn fail_prepared_committed_write(
        &mut self,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        commit_ts: Timestamp,
        operation: &'static str,
        err: anyhow::Error,
    ) -> anyhow::Result<()> {
        let message = format!(
            "Failed to {operation} after prepared persistence commit at ts={}: {err:#}",
            u64::from(commit_ts),
        );
        self.send_prepared_commit_waiters(transaction_id, Err(anyhow::anyhow!(message.clone())));
        anyhow::bail!(message)
    }

    async fn publish_commit_delta(
        distributed_log: Arc<dyn DistributedLog>,
        delta: CommitDelta,
        path: &'static str,
    ) -> anyhow::Result<()> {
        let timer = metrics::commit_hot_path_stage_timer(path, "replication_publish");
        let delta_ts = delta.ts;
        let result = distributed_log.publish(delta).await.with_context(|| {
            format!(
                "Failed to publish commit delta at ts={}",
                u64::from(delta_ts)
            )
        });
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    async fn consolidate_raft_apply_marker(
        &mut self,
        marker: RaftApplyMarker,
    ) -> anyhow::Result<()> {
        let mut data_watermarks = self.applied_data_delta_watermarks.clone();
        if data_watermarks
            .get(&marker.source_partition)
            .is_none_or(|current| *current < marker.origin_ts)
        {
            data_watermarks.insert(marker.source_partition, marker.origin_ts);
        }

        let mut applied_ids = self.applied_data_delta_ids.clone();
        applied_ids.insert_origin_timestamp(marker.source_partition, marker.origin_ts);
        applied_ids.prune_origin_timestamps_below_watermarks(&data_watermarks);

        // The raw marker remains the crash-safe source of truth until both
        // compact durable summaries have been written. Deleting it last makes
        // every interruption point recoverable.
        self.persistence
            .write_persistence_global(
                PersistenceGlobalKey::AppliedDataDeltaWatermarks,
                partition_timestamp_map_to_json(&data_watermarks),
            )
            .await?;
        self.persistence
            .write_persistence_global(
                PersistenceGlobalKey::AppliedDataDeltaIds,
                applied_data_delta_ids_to_json(&applied_ids),
            )
            .await?;
        self.persistence
            .delete_persistence_global_raw(&marker.key())
            .await?;

        self.applied_data_delta_watermarks = data_watermarks;
        self.applied_data_delta_ids = applied_ids;
        self.raft_apply_markers.remove(&marker);
        Ok(())
    }

    fn raft_apply_persistence_writes(
        marker: Option<RaftApplyMarker>,
        delta: Option<&CommitDelta>,
        raft_state_machine_index: Option<u64>,
    ) -> anyhow::Result<Vec<PersistenceGlobalWrite>> {
        let Some(marker) = marker else {
            anyhow::ensure!(
                delta.is_none() && raft_state_machine_index.is_none(),
                "Raft apply metadata supplied without an atomic apply marker",
            );
            return Ok(Vec::new());
        };
        let delta = delta.context("Raft apply marker supplied without its committed delta")?;
        anyhow::ensure!(
            marker == RaftApplyMarker::from_delta(delta)?,
            "Raft apply marker did not match its committed delta",
        );
        let raft_state_machine_index = raft_state_machine_index
            .context("Raft apply marker supplied without a state-machine index")?;
        Ok(vec![
            marker.persistence_write()?,
            Self::raft_nats_outbox_persistence_write(delta)?,
            state_machine_index_persistence_write(raft_state_machine_index),
        ])
    }

    fn raft_nats_outbox_key(ts: Timestamp) -> String {
        u64::from(ts).to_string()
    }

    fn raft_nats_outbox_entry_key(delta: &CommitDelta) -> anyhow::Result<String> {
        let source_partition = delta.source_partition.with_context(|| {
            format!(
                "Raft->NATS outbox delta at ts={} did not include a source partition",
                u64::from(delta.ts)
            )
        })?;
        Ok(format!(
            "{RAFT_NATS_OUTBOX_KEY_PREFIX}{:020}/{:020}",
            source_partition.0,
            u64::from(delta.ts)
        ))
    }

    fn parse_raft_nats_outbox_entry_key(key: &str) -> anyhow::Result<Timestamp> {
        let (_, ts) = key
            .strip_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
            .and_then(|suffix| suffix.rsplit_once('/'))
            .with_context(|| format!("Failed to parse Raft->NATS outbox key {key:?}"))?;
        ts.parse::<u64>()
            .with_context(|| format!("Failed to parse Raft->NATS outbox timestamp in {key:?}"))
            .and_then(Timestamp::try_from)
    }

    async fn load_raft_nats_outbox_records(
        persistence: Arc<dyn Persistence>,
    ) -> anyhow::Result<Vec<RaftNatsOutboxRecord>> {
        let mut records = Vec::new();
        for (key, value) in persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
            .await?
        {
            let ts = Self::parse_raft_nats_outbox_entry_key(&key)?;
            records.push(RaftNatsOutboxRecord {
                key,
                ts,
                value,
                storage: RaftNatsOutboxRecordStorage::PerEntry,
            });
        }

        let Some(value) = persistence
            .reader()
            .get_persistence_global(PersistenceGlobalKey::RaftNatsOutbox)
            .await?
        else {
            records.sort_by_key(|record| record.ts);
            return Ok(records);
        };
        let legacy_records: LegacyRaftNatsOutboxRecords =
            serde_json::from_value(value).context("Failed to parse Raft->NATS outbox records")?;
        for (key, value) in legacy_records {
            let ts = key
                .parse::<u64>()
                .with_context(|| format!("Failed to parse Raft->NATS outbox key {key:?}"))
                .and_then(Timestamp::try_from)?;
            if records.iter().any(|record| record.ts == ts) {
                continue;
            }
            records.push(RaftNatsOutboxRecord {
                key,
                ts,
                value,
                storage: RaftNatsOutboxRecordStorage::LegacyBlob,
            });
        }
        records.sort_by_key(|record| record.ts);
        Ok(records)
    }

    async fn add_raft_nats_outbox_delta(
        persistence: Arc<dyn Persistence>,
        delta: &CommitDelta,
    ) -> anyhow::Result<()> {
        let PersistenceGlobalWrite { key, value } =
            Self::raft_nats_outbox_persistence_write(delta)?;
        persistence.write_persistence_global_raw(&key, value).await
    }

    fn raft_nats_outbox_persistence_write(
        delta: &CommitDelta,
    ) -> anyhow::Result<PersistenceGlobalWrite> {
        let envelope = DeltaEnvelope::from_delta(delta, "", None)
            .context("Failed to encode Raft->NATS outbox delta")?;
        Ok(PersistenceGlobalWrite::new(
            Self::raft_nats_outbox_entry_key(delta)?,
            serde_json::to_value(envelope)
                .context("Failed to serialize Raft->NATS outbox delta")?,
        ))
    }

    async fn record_raft_nats_outbox_delta(
        persistence: Arc<dyn Persistence>,
        delta: &CommitDelta,
        path: &'static str,
    ) -> anyhow::Result<()> {
        let timer = metrics::commit_hot_path_stage_timer(path, "raft_nats_outbox_record");
        let result = Self::add_raft_nats_outbox_delta(persistence, delta).await;
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    async fn delete_legacy_raft_nats_outbox_delta(
        persistence: Arc<dyn Persistence>,
        ts: Timestamp,
    ) -> anyhow::Result<()> {
        let Some(value) = persistence
            .reader()
            .get_persistence_global(PersistenceGlobalKey::RaftNatsOutbox)
            .await?
        else {
            return Ok(());
        };
        let mut records: LegacyRaftNatsOutboxRecords =
            serde_json::from_value(value).context("Failed to parse Raft->NATS outbox records")?;
        records.remove(&Self::raft_nats_outbox_key(ts));
        if records.is_empty() {
            return persistence
                .delete_persistence_global_raw(&String::from(PersistenceGlobalKey::RaftNatsOutbox))
                .await;
        }
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::RaftNatsOutbox,
                serde_json::to_value(records)
                    .context("Failed to serialize Raft->NATS outbox records")?,
            )
            .await
    }

    async fn clear_raft_nats_outbox_record(
        persistence: Arc<dyn Persistence>,
        record: &RaftNatsOutboxRecord,
        path: &'static str,
    ) -> anyhow::Result<()> {
        let timer = metrics::commit_hot_path_stage_timer(path, "raft_nats_outbox_clear");
        let result = match record.storage {
            RaftNatsOutboxRecordStorage::LegacyBlob => {
                Self::delete_legacy_raft_nats_outbox_delta(persistence, record.ts).await
            },
            RaftNatsOutboxRecordStorage::PerEntry => {
                persistence.delete_persistence_global_raw(&record.key).await
            },
        };
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    async fn replay_raft_nats_outbox_once(
        persistence: Arc<dyn Persistence>,
        distributed_log: Arc<dyn DistributedLog>,
    ) -> anyhow::Result<usize> {
        let records = Self::load_raft_nats_outbox_records(persistence.clone()).await?;
        let mut replayed = 0usize;
        for record in records {
            let envelope: DeltaEnvelope = serde_json::from_value(record.value.clone())
                .with_context(|| {
                    format!("Failed to parse Raft->NATS outbox delta {}", record.ts)
                })?;
            let delta = envelope.to_delta().with_context(|| {
                format!("Failed to decode Raft->NATS outbox delta {}", record.ts)
            })?;
            anyhow::ensure!(
                delta.ts == record.ts,
                "Raft->NATS outbox key {} did not match encoded delta ts {}",
                u64::from(record.ts),
                u64::from(delta.ts),
            );
            Self::publish_commit_delta(distributed_log.clone(), delta, "raft_nats_outbox_replay")
                .await?;
            Self::clear_raft_nats_outbox_record(
                persistence.clone(),
                &record,
                "raft_nats_outbox_replay",
            )
            .await?;
            replayed += 1;
        }
        Ok(replayed)
    }

    async fn record_and_replay_raft_nats_outbox_delta(
        persistence: Arc<dyn Persistence>,
        distributed_log: Arc<dyn DistributedLog>,
        delta: &CommitDelta,
        path: &'static str,
    ) -> anyhow::Result<()> {
        Self::record_raft_nats_outbox_delta(persistence.clone(), delta, path).await?;
        if let Err(err) = Self::replay_raft_nats_outbox_once(persistence, distributed_log).await {
            tracing::error!(
                "Failed to publish committed write at ts={} to replication log through ordered \
                 Raft->NATS outbox; leaving outbox entries for replay: {err:#}",
                u64::from(delta.ts),
            );
        }
        Ok(())
    }

    fn should_record_raft_nats_outbox_delta(delta: &CommitDelta) -> bool {
        delta.source_partition.is_some()
    }

    fn should_replay_raft_nats_outbox(&self) -> bool {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return false;
        };
        if placement_state.num_partitions() <= 1 {
            return false;
        }
        self.raft_state.as_ref().is_none_or(|raft_state| {
            raft_state.is_cluster_genesis_ready() && raft_state.is_leader_ready()
        })
    }

    fn propose_commit_to_raft_state(
        raft_state: Option<&crate::raft_partition::RaftPartitionState>,
        delta: &CommitDelta,
    ) -> anyhow::Result<RaftCommitWaiter> {
        let Some(raft) = raft_state else {
            return Ok(None);
        };
        anyhow::ensure!(
            raft.is_cluster_genesis_ready(),
            "Cluster genesis is not ready for partition {}",
            raft.partition_id(),
        );
        if !raft.is_leader_ready() {
            anyhow::bail!(
                "Raft leader is not ready before proposing commit at ts={} for partition {}",
                u64::from(delta.ts),
                raft.partition_id(),
            );
        }
        let data = RaftStateMachineEntry::commit_delta(delta, raft.node_id())?
            .to_bytes()
            .with_context(|| {
                format!(
                    "Failed to serialize commit delta at ts={} for Raft proposal",
                    u64::from(delta.ts),
                )
            })?;
        let (tx, rx) = oneshot::channel();
        raft.send(crate::raft_node::RaftMessage::Propose(
            crate::raft_node::RaftProposal {
                data,
                result_tx: tx,
            },
        ))
        .with_context(|| {
            format!(
                "Failed to propose commit at ts={} to Raft partition {}",
                u64::from(delta.ts),
                raft.partition_id(),
            )
        })?;
        Ok(Some(rx))
    }

    fn propose_commit_to_raft(&self, delta: &CommitDelta) -> anyhow::Result<RaftCommitWaiter> {
        Self::propose_commit_to_raft_state(self.raft_state.as_ref(), delta)
    }

    fn propose_two_phase_prepare_to_raft(
        &self,
        redo: &crate::two_phase::TwoPhaseRedoEntry,
    ) -> anyhow::Result<RaftCommitWaiter> {
        let Some(raft) = self.raft_state.as_ref() else {
            return Ok(None);
        };
        anyhow::ensure!(
            raft.is_cluster_genesis_ready(),
            "Cluster genesis is not ready for partition {}",
            raft.partition_id(),
        );
        if !raft.is_leader_ready() {
            anyhow::bail!(
                "Raft leader is not ready before proposing 2PC prepare for txn={} on partition {}",
                redo.transaction_id,
                raft.partition_id(),
            );
        }
        let data = RaftStateMachineEntry::two_phase_prepare(redo.clone())
            .to_bytes()
            .with_context(|| {
                format!(
                    "Failed to serialize 2PC prepare for txn={} for Raft proposal",
                    redo.transaction_id,
                )
            })?;
        let (tx, rx) = oneshot::channel();
        raft.send(crate::raft_node::RaftMessage::Propose(
            crate::raft_node::RaftProposal {
                data,
                result_tx: tx,
            },
        ))
        .with_context(|| {
            format!(
                "Failed to propose 2PC prepare for txn={} to Raft partition {}",
                redo.transaction_id,
                raft.partition_id(),
            )
        })?;
        Ok(Some(rx))
    }

    fn propose_two_phase_rollback_to_raft(
        &self,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<RaftCommitWaiter> {
        let Some(raft) = self.raft_state.as_ref() else {
            return Ok(None);
        };
        anyhow::ensure!(
            raft.is_cluster_genesis_ready(),
            "Cluster genesis is not ready for partition {}",
            raft.partition_id(),
        );
        if !raft.is_leader_ready() {
            anyhow::bail!(
                "Raft leader is not ready before proposing 2PC rollback for txn={} on partition {}",
                transaction_id,
                raft.partition_id(),
            );
        }
        let data = RaftStateMachineEntry::two_phase_rollback(transaction_id.clone())
            .to_bytes()
            .with_context(|| {
                format!(
                    "Failed to serialize 2PC rollback for txn={} for Raft proposal",
                    transaction_id,
                )
            })?;
        let (tx, rx) = oneshot::channel();
        raft.send(crate::raft_node::RaftMessage::Propose(
            crate::raft_node::RaftProposal {
                data,
                result_tx: tx,
            },
        ))
        .with_context(|| {
            format!(
                "Failed to propose 2PC rollback for txn={} to Raft partition {}",
                transaction_id,
                raft.partition_id(),
            )
        })?;
        Ok(Some(rx))
    }

    fn build_commit_delta(
        commit_ts: Timestamp,
        ordered_updates: &crate::write_log::OrderedDocumentWrites,
        write_source: WriteSource,
        new_snapshot: &Snapshot,
        write_bytes: u64,
        document_writes: Arc<Vec<DocumentLogEntry>>,
        index_writes: Arc<Vec<PersistenceIndexEntry>>,
        source_partition: Option<crate::partition::PartitionId>,
    ) -> CommitDelta {
        let document_updates: Vec<_> = ordered_updates
            .iter()
            .map(|(_, update)| update.unpack())
            .collect();
        let table_mapping = new_snapshot.table_registry.table_mapping().clone();
        let mut tablet_id_to_table_name = std::collections::BTreeMap::new();
        let index_table_name: &TableName = &common::bootstrap_model::index::INDEX_TABLE;
        for update in &document_updates {
            let tablet_id = update.id.tablet_id;
            insert_delta_table_mapping(&table_mapping, &mut tablet_id_to_table_name, tablet_id);

            if tablet_id_to_table_name
                .get(&tablet_id)
                .is_some_and(|name| name == index_table_name)
            {
                for document in update.old_document.iter().chain(update.new_document.iter()) {
                    if let Some(indexed_tablet_id) = index_metadata_tablet_id(document) {
                        insert_delta_table_mapping(
                            &table_mapping,
                            &mut tablet_id_to_table_name,
                            indexed_tablet_id,
                        );
                    }
                }
            }
        }
        CommitDelta {
            ts: commit_ts,
            document_writes,
            document_updates,
            index_writes,
            write_source,
            write_bytes,
            tablet_id_to_table_name,
            source_partition,
        }
    }

    /// After writing the new rows to persistence, mark the commit as complete
    /// and allow the updated rows to be read by other transactions.
    #[fastrace::trace]
    fn publish_commit(
        &mut self,
        pending_write: PendingWriteHandle,
        delta: CommitDelta,
    ) -> anyhow::Result<PublishedCommit> {
        let commit_ts = pending_write.must_commit_ts();
        anyhow::ensure!(
            delta.ts == commit_ts,
            "pending write at ts={} had Raft delta at ts={}",
            u64::from(commit_ts),
            u64::from(delta.ts),
        );
        let (ordered_updates, write_source, new_snapshot) =
            match self.pending_writes.pop_first(pending_write) {
                None => panic!("commit at {commit_ts} not pending"),
                Some((ts, document_updates, write_source, snapshot)) => {
                    if ts != commit_ts {
                        panic!("commits out of order {ts} != {commit_ts}");
                    }
                    (document_updates, write_source, snapshot)
                },
            };

        self.publish_commit_from_updates(
            commit_ts,
            ordered_updates,
            write_source,
            new_snapshot,
            delta,
            "local",
        )
    }

    #[fastrace::trace]
    fn publish_commit_from_updates(
        &mut self,
        commit_ts: Timestamp,
        ordered_updates: crate::write_log::OrderedDocumentWrites,
        write_source: WriteSource,
        new_snapshot: Snapshot,
        delta: CommitDelta,
        path: &'static str,
    ) -> anyhow::Result<PublishedCommit> {
        let apply_timer = metrics::commit_apply_timer();
        anyhow::ensure!(
            delta.ts == commit_ts,
            "publish commit called for ts={} with delta ts={}",
            u64::from(commit_ts),
            u64::from(delta.ts),
        );
        let write_bytes = delta.write_bytes;
        let latest_snapshot_ts = *self.snapshot_manager.read().latest_ts();
        anyhow::ensure!(
            latest_snapshot_ts < commit_ts,
            "Commit at ts={} can no longer publish because latest snapshot is {}",
            u64::from(commit_ts),
            latest_snapshot_ts,
        );

        if let Some(ref tso) = self.timestamp_oracle {
            let tso = tso.clone();
            let tso_advance_timer =
                metrics::commit_hot_path_stage_timer(path, "tso_advance_committed");
            let advance_result = block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                if rt.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                    futures::executor::block_on(tso.advance_committed_ts(commit_ts))
                } else {
                    rt.block_on(tso.advance_committed_ts(commit_ts))
                }
            });
            if advance_result.is_ok() {
                tso_advance_timer.finish();
            }
            if let Err(err) = advance_result {
                tracing::warn!(
                    "Failed to advance global max_committed to {} before publishing commit: \
                     {err:#}",
                    u64::from(commit_ts),
                );
            }
        }

        // Write transaction state at the commit ts to the document store.
        let hot_path_apply_timer = metrics::commit_hot_path_stage_timer(path, "apply_visible");
        metrics::commit_rows(ordered_updates.len() as u64);

        let timer = metrics::pending_writes_to_write_log_timer();
        // See the comment in `overlaps_index_keys` for why it’s safe
        // to use indexes from the current snapshot.
        let writes = index_keys_from_full_documents(ordered_updates, &new_snapshot.index_registry);
        drop(timer);
        metrics::write_log_commit_bytes(delta.write_bytes as usize);

        let timer = metrics::write_log_append_timer();
        self.log.append(commit_ts, writes, write_source.clone());
        drop(timer);

        if let Some(table_summaries) = new_snapshot.table_summaries.as_ref() {
            metrics::log_num_keys(table_summaries.num_user_documents);
            metrics::log_user_table_documents_size(table_summaries.user_tables_size);
            self.user_documents_size_gauge
                .set(table_summaries.user_docs_size as i64);
        }

        // Publish the new version of our database metadata and the index.
        let mut snapshot_manager = self.snapshot_manager.write();
        snapshot_manager.push(commit_ts, new_snapshot, write_bytes);
        drop(snapshot_manager);

        apply_timer.finish();
        hot_path_apply_timer.finish();
        Ok(PublishedCommit {
            delta,
            raft_applied_index: None,
        })
    }

    /// Capture one stable Convex generation for the exact Raft prefix the
    /// storage layer is snapshotting. The committer loop admits no later work
    /// while this method runs, and all earlier persistence futures have
    /// already drained.
    fn build_raft_snapshot_bytes(
        &self,
        raft_index: u64,
        raft_term: u64,
    ) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> {
        let (checkpoint_ts, replication_frontiers, table_summary_snapshot) = {
            let snapshot_manager = self.snapshot_manager.read();
            let (ts, snapshot) = snapshot_manager.latest();
            let table_summary_snapshot =
                snapshot
                    .table_summaries
                    .as_ref()
                    .map(|summaries| TableSummarySnapshot {
                        tables: summaries
                            .tables
                            .iter()
                            .map(|(key, value)| (*key, value.clone()))
                            .collect(),
                        ts: *ts,
                    });
            (
                *ts,
                snapshot_manager.replication_frontiers(),
                table_summary_snapshot,
            )
        };
        let persistence = self.persistence.clone();
        let retention_validator = self.retention_validator.clone();
        async move {
            let reader = persistence.reader();
            let state_machine_index = load_state_machine_index(reader.as_ref()).await?;
            anyhow::ensure!(
                state_machine_index <= raft_index,
                "Convex state includes Raft index {state_machine_index}, beyond requested \
                 snapshot index {raft_index}",
            );
            anyhow::ensure!(
                reader
                    .list_persistence_globals_with_prefix(RAFT_APPLY_MARKER_KEY_PREFIX)
                    .await?
                    .is_empty(),
                "Cannot generate a Raft snapshot while an apply marker is unresolved",
            );

            let mut checkpoint =
                create_checkpoint(reader.as_ref(), checkpoint_ts, retention_validator).await?;

            let mut globals = BTreeMap::new();
            for key in PersistenceGlobalKey::checkpoint_keys().into_iter().chain([
                PersistenceGlobalKey::TwoPhaseRedoRecords,
                PersistenceGlobalKey::RaftNatsOutbox,
            ]) {
                if let Some(value) = reader.get_persistence_global(key).await? {
                    globals.insert(String::from(key), value);
                }
            }
            for (key, value) in reader
                .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
                .await?
            {
                globals.insert(key, value);
            }
            globals.insert(
                RAFT_STATE_MACHINE_INDEX_KEY.to_string(),
                state_machine_index.into(),
            );
            globals.insert(
                String::from(PersistenceGlobalKey::MaxRepeatableTimestamp),
                serde_json::Value::from(u64::from(checkpoint_ts)),
            );
            globals.insert(
                String::from(PersistenceGlobalKey::ReplicationFrontiers),
                replication_frontiers_to_json(&replication_frontiers),
            );
            if let Some(table_summary_snapshot) = table_summary_snapshot {
                globals.insert(
                    String::from(PersistenceGlobalKey::TableSummary),
                    serde_json::Value::from(&table_summary_snapshot),
                );
            }
            checkpoint.globals = globals;
            encode_raft_snapshot(raft_index, raft_term, state_machine_index, &checkpoint)
        }
        .boxed()
    }

    fn install_snapshot(
        &mut self,
        checkpoint: CheckpointData,
        source: SnapshotInstallSource,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    ) -> anyhow::Result<()> {
        let persistence = self.persistence.clone();
        let runtime = self.runtime.clone();
        let virtual_system_mapping = self.virtual_system_mapping.clone();
        let partition_map = self
            .placement_state
            .as_ref()
            .map(|placement_state| placement_state.partition_map());

        self.persistence_writes.push_back(
            async move {
                let prepared = prepare_checkpoint_install(
                    runtime,
                    persistence.clone(),
                    virtual_system_mapping,
                    partition_map,
                    &checkpoint,
                )
                .await?;
                persist_checkpoint_install(persistence, &checkpoint, &prepared, source).await?;

                Ok(PersistenceWrite::InstallSnapshot {
                    source,
                    snapshot_ts: prepared.snapshot_ts,
                    snapshot: prepared.snapshot,
                    replication_frontiers: prepared.replication_frontiers,
                    applied_delta_watermarks: prepared.applied_delta_watermarks,
                    applied_data_delta_watermarks: prepared.applied_data_delta_watermarks,
                    applied_data_delta_ids: prepared.applied_data_delta_ids,
                    result,
                    commit_id,
                })
            }
            .boxed(),
        );

        Ok(())
    }

    /// Apply a replicated delta from the Primary to this Replica's state.
    ///
    /// This is the Replica's apply path — equivalent to the Primary's
    /// validate_commit + write_to_persistence + publish_commit, but for
    /// deltas received from NATS instead of local transactions.
    ///
    /// Steps:
    /// 1. Apply `_tables` updates first (creates new tables on Replica)
    /// 2. Rebuild TabletId remapping after table creation
    /// 3. Apply remaining document updates with remapped IDs
    /// 4. Write to Replica's persistence (so function runner can read modules)
    /// 5. Update SnapshotManager and WriteLog
    fn apply_replica_delta(
        &mut self,
        delta: CommitDelta,
        mode: ReplicaDeltaApplyMode,
        raft_state_machine_index: Option<u64>,
        transport_id: Option<ReplicationTransportId>,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    ) -> anyhow::Result<()> {
        use std::collections::BTreeMap;

        use common::{
            bootstrap_model::tables::TABLES_TABLE,
            document::DocumentUpdate,
            persistence::{
                ConflictStrategy,
                DocumentLogEntry,
            },
        };
        use value::ResolvedDocumentId;

        let raft_apply_marker = (mode == ReplicaDeltaApplyMode::FullRaftState
            && !delta.document_updates.is_empty())
        .then(|| RaftApplyMarker::from_delta(&delta))
        .transpose()?;
        if let Some(marker) = raft_apply_marker
            && self.raft_apply_markers.contains(&marker)
        {
            let raft_state_machine_index = raft_state_machine_index
                .context("full-state Raft replay did not include its committed log index")?;
            tracing::info!(
                "Recovering already-installed Raft delta without reapplying: partition={}, \
                 origin_ts={}",
                marker.source_partition.0,
                u64::from(marker.origin_ts),
            );
            self.persistence_writes.push_back(
                async move {
                    Ok(PersistenceWrite::RaftReplayRecovery {
                        marker,
                        delta,
                        raft_state_machine_index,
                        result,
                        commit_id,
                    })
                }
                .boxed(),
            );
            return Ok(());
        }

        // Apply replica deltas in the same timestamp domain as the leader's
        // commit. Followers should never "forget" a higher remote commit
        // timestamp and then hand out a lower local one for a future 2PC
        // prepare. We therefore use the remote commit timestamp as a floor and
        // only bump above it when local monotonicity requires it.
        let remote_ts = delta.ts;
        let local_prepared_commit_ts = if mode == ReplicaDeltaApplyMode::FullRaftState
            && delta.source_partition == Some(self.local_partition_for_two_phase())
        {
            self.local_prepared_commit_ts_for_origin(remote_ts)?
        } else {
            None
        };
        let latest_local_ts = *self.snapshot_manager.read().latest_ts();
        let timestamps =
            replica_delta_timestamps(remote_ts, latest_local_ts, self.last_assigned_ts)?;
        let remote_ts = timestamps.origin_ts;
        let commit_ts = if let Some(local_prepared_commit_ts) = local_prepared_commit_ts {
            if local_prepared_commit_ts > latest_local_ts
                && local_prepared_commit_ts > self.last_assigned_ts
            {
                local_prepared_commit_ts
            } else {
                tracing::info!(
                    "2PC Raft Commit Apply: origin ts={} prepared locally at ts={} but local \
                     floor is latest_snapshot={}, last_assigned={}; applying committed delta at \
                     ts={}",
                    u64::from(remote_ts),
                    u64::from(local_prepared_commit_ts),
                    u64::from(latest_local_ts),
                    u64::from(self.last_assigned_ts),
                    u64::from(timestamps.local_apply_ts),
                );
                timestamps.local_apply_ts
            }
        } else {
            timestamps.local_apply_ts
        };
        if mode == ReplicaDeltaApplyMode::CrossPartitionReplica
            && delta.source_partition != Some(self.local_partition_for_two_phase())
            && let Some(min_prepared_ts) = self
                .prepared_transactions
                .values()
                .map(|prepared| prepared.commit_ts)
                .min()
            && commit_ts >= min_prepared_ts
        {
            let err = anyhow::Error::new(RetryableReplicaApplyError::new(format!(
                "Replica delta at ts={} would overtake unresolved 2PC prepared transaction at \
                 ts={}; retry after the prepared transaction commits or rolls back",
                u64::from(commit_ts),
                u64::from(min_prepared_ts),
            )));
            let _ = result.send(Err(err));
            return Ok(());
        }
        tracing::info!(
            "Applying replica delta: remote_ts={}, local_ts={}, {} document updates, {} tablet \
             mappings",
            u64::from(remote_ts),
            u64::from(commit_ts),
            delta.document_updates.len(),
            delta.tablet_id_to_table_name.len(),
        );

        // Helper: build remap from current snapshot state.
        let build_remap = |snapshot: &Snapshot,
                           tablet_map: &BTreeMap<value::TabletId, value::TableName>|
         -> BTreeMap<value::TabletId, value::TabletId> {
            let mapping = snapshot.table_registry.table_mapping();
            let mut remap = BTreeMap::new();
            for (primary_id, name) in tablet_map {
                for ns in mapping.namespaces_for_name(name) {
                    let ns_mapping = mapping.namespace(ns);
                    if let Ok(local_id) = ns_mapping.name_to_tablet()(name.clone()) {
                        remap.insert(*primary_id, local_id);
                        break;
                    }
                }
            }
            remap
        };

        // Classify each update for the selected replication scope.
        //
        // Cross-partition NATS replication follows CockroachDB GLOBAL table
        // locality: user data and globally needed deployment metadata are
        // replicated, while node-local system state is filtered out.
        //
        // Same-partition Raft replication is different: followers are HA
        // copies that may become leader, so they must apply the full
        // authoritative state, including system tables intentionally skipped
        // by cross-partition read replicas.
        //
        // The key insight: we classify by what the data DESCRIBES, not by
        // which system table it's stored in. An _index entry creating
        // "projects.by_id" is user table metadata even though _index is a
        // system table.
        use common::bootstrap_model::index::INDEX_TABLE;

        let tables_table_name: &value::TableName = &TABLES_TABLE;
        let index_table_name: &value::TableName = &INDEX_TABLE;

        // GLOBAL deployment system tables (CockroachDB GLOBAL locality pattern).
        //
        // These tables contain deployment state that every node needs to serve
        // queries and mutations — function code, runtime config, and bundled
        // source. Without these, followers can't execute user functions after
        // a deploy to the leader.
        //
        // Equivalent to CockroachDB's system.descriptor (schema definitions),
        // YugabyteDB's pg_proc (stored procedures), and TiDB's mysql.tidb
        // (DDL schema state).
        let global_deployment_tables: &[&str] = &[
            "_modules",         // JavaScript/TypeScript function source code
            "_udf_config",      // UDF runtime configuration
            "_source_packages", // Bundled source packages
        ];
        let is_global_deployment_table = |name: &value::TableName| -> bool {
            let name_str = name.to_string();
            global_deployment_tables.iter().any(|&t| name_str == t)
        };

        let mut tables_updates = Vec::new();
        let mut other_updates = Vec::new();
        let mut skipped_system = 0usize;

        for update in &delta.document_updates {
            let primary_tablet = update.id.tablet_id;
            let table_name = delta.tablet_id_to_table_name.get(&primary_tablet);

            match mode {
                ReplicaDeltaApplyMode::FullRaftState => {
                    if table_name.map(|n| n == tables_table_name).unwrap_or(false) {
                        tables_updates.push(update);
                    } else {
                        other_updates.push(update);
                    }
                },
                ReplicaDeltaApplyMode::CrossPartitionReplica => {
                    if table_name.map(|n| n == tables_table_name).unwrap_or(false) {
                        // _tables entry: kept for Phase 1 (user table creation).
                        // System table entries will be skipped in Phase 1 by the
                        // table_exists check.
                        tables_updates.push(update);
                    } else if table_name.map(|n| n == index_table_name).unwrap_or(false) {
                        // _index entry: check if it describes a user table index
                        // or a GLOBAL deployment table index.
                        let is_replicated_index =
                            update.new_document.as_ref().map_or(false, |doc| {
                                doc.value()
                                    .0
                                    .get(&"table_id".parse::<common::types::FieldName>().unwrap())
                                    .and_then(|v| {
                                        if let value::ConvexValue::String(s) = v {
                                            let tid: Result<value::TabletId, _> = s.parse();
                                            tid.ok()
                                        } else {
                                            None
                                        }
                                    })
                                    .map_or(false, |tid| {
                                        delta.tablet_id_to_table_name.get(&tid).map_or(
                                            false,
                                            |name| {
                                                !name.is_system()
                                                    || is_global_deployment_table(name)
                                            },
                                        )
                                    })
                            });
                        if is_replicated_index {
                            other_updates.push(update);
                        } else {
                            skipped_system += 1;
                        }
                    } else if table_name.map(|n| !n.is_system()).unwrap_or(false) {
                        // User table data: replicate.
                        other_updates.push(update);
                    } else if table_name
                        .map(|n| is_global_deployment_table(n))
                        .unwrap_or(false)
                    {
                        // GLOBAL deployment system table: replicate.
                        // These contain function code and config needed by every node
                        // to execute queries and mutations (CockroachDB GLOBAL pattern).
                        other_updates.push(update);
                    } else {
                        // Node-local system data: skip for cross-partition replicas.
                        skipped_system += 1;
                    }
                },
            }
        }
        if skipped_system > 0 {
            tracing::debug!(
                "Filtered {} node-local system updates from delta",
                skipped_system,
            );
        }

        let is_frontier_only = tables_updates.is_empty() && other_updates.is_empty();
        if is_frontier_only {
            if commit_ts > self.last_assigned_ts {
                self.last_assigned_ts = commit_ts;
            }
            let Some(source_partition) = delta.source_partition else {
                tracing::debug!(
                    "Skipping no-op replica delta at ts {} with no source partition",
                    u64::from(commit_ts),
                );
                let _ = result.send(Ok(commit_ts));
                return Ok(());
            };
            let updated_replication_frontiers = {
                let mut frontiers = self.snapshot_manager.read().replication_frontiers();
                let should_update = frontiers
                    .get(&source_partition)
                    .is_none_or(|current| *current < commit_ts);
                if should_update {
                    frontiers.insert(source_partition, commit_ts);
                }
                frontiers
            };
            let updated_applied_delta_watermarks = {
                let mut frontiers = self.applied_delta_watermarks.clone();
                if frontiers
                    .get(&source_partition)
                    .is_none_or(|current| *current < remote_ts)
                {
                    frontiers.insert(source_partition, remote_ts);
                }
                frontiers
            };
            if self
                .applied_delta_watermarks
                .get(&source_partition)
                .is_none_or(|current| *current < remote_ts)
            {
                self.applied_delta_watermarks
                    .insert(source_partition, remote_ts);
            }
            let persistence = self.persistence.clone();
            self.persistence_writes.push_back(
                async move {
                    persistence
                        .write_persistence_global(
                            PersistenceGlobalKey::ReplicationFrontiers,
                            replication_frontiers_to_json(&updated_replication_frontiers),
                        )
                        .await?;
                    Ok(PersistenceWrite::RemoteReadFrontierHeartbeat {
                        commit_ts,
                        source_partition,
                        applied_delta_watermarks: updated_applied_delta_watermarks,
                        result,
                        commit_id,
                    })
                }
                .boxed(),
            );

            tracing::info!(
                "Queued replication frontier heartbeat: ts={}, partition={}",
                u64::from(commit_ts),
                source_partition.0,
            );
            return Ok(());
        }

        let duplicate_transport_delivery = transport_id.is_some_and(|transport_id| {
            self.applied_data_delta_ids
                .contains_transport_id(transport_id)
        });
        let duplicate_origin_delta = delta.source_partition.is_some_and(|source_partition| {
            self.applied_data_delta_ids
                .contains_origin_timestamp(source_partition, remote_ts)
        });
        if mode == ReplicaDeltaApplyMode::FullRaftState
            && (duplicate_transport_delivery || duplicate_origin_delta)
        {
            let marker = raft_apply_marker.context(
                "non-empty full-state Raft replay should have a stable apply marker identity",
            )?;
            let raft_state_machine_index = raft_state_machine_index
                .context("full-state Raft replay did not include its committed log index")?;
            tracing::info!(
                "Repairing side effects for durably deduplicated Raft replay: partition={}, \
                 origin_ts={}",
                marker.source_partition.0,
                u64::from(marker.origin_ts),
            );
            self.persistence_writes.push_back(
                async move {
                    Ok(PersistenceWrite::RaftReplayRecovery {
                        marker,
                        delta,
                        raft_state_machine_index,
                        result,
                        commit_id,
                    })
                }
                .boxed(),
            );
            return Ok(());
        }
        if local_prepared_commit_ts.is_none()
            && (duplicate_transport_delivery || duplicate_origin_delta)
        {
            tracing::info!(
                "Skipping duplicate non-empty replica delta: source_partition={:?}, remote_ts={}, \
                 transport_stream_sequence={:?}, duplicate_transport={}, duplicate_origin={}",
                delta.source_partition.map(|partition| partition.0),
                u64::from(remote_ts),
                transport_id.map(ReplicationTransportId::stream_sequence),
                duplicate_transport_delivery,
                duplicate_origin_delta,
            );
            let _ = result.send(Ok(remote_ts));
            return Ok(());
        }
        if commit_ts > self.last_assigned_ts {
            self.last_assigned_ts = commit_ts;
        }

        let mut snapshot = self.base_snapshot_for_new_writes();
        let mut all_remapped_updates = Vec::new();
        let mut all_index_updates = Vec::new();

        // Phase 1: Apply _tables updates (creates new tables).
        //
        // Clustered deployments allocate user table numbers from a global
        // allocator, so public document IDs embedded in document values are
        // portable across nodes. Replica apply must therefore preserve the
        // source TableMetadata.number exactly. If a local table with the same
        // name already exists with a different number, fail loudly instead of
        // silently rewriting IDs into a different table-number namespace.
        if !tables_updates.is_empty() {
            use value::DocumentObject;

            let remap = build_remap(&snapshot, &delta.tablet_id_to_table_name);

            for update in &tables_updates {
                let primary_tablet = update.id.tablet_id;
                let local_tablet = match remap.get(&primary_tablet) {
                    Some(t) => *t,
                    None => continue,
                };

                // Parse the TableMetadata to check if the table already exists
                // locally and to reassign the table number if needed.
                let new_doc = match &update.new_document {
                    Some(d) => d,
                    None => {
                        // Deletion — remap and apply directly.
                        let remapped = DocumentUpdate {
                            id: ResolvedDocumentId {
                                tablet_id: local_tablet,
                                document_id: update.id.document_id,
                            },
                            old_document: update
                                .old_document
                                .as_ref()
                                .map(|d| d.to_remapped(local_tablet)),
                            new_document: None,
                        };
                        let (idx_updates, ..) = snapshot.update(&remapped, commit_ts)?;
                        all_index_updates.extend(idx_updates);
                        all_remapped_updates.push(remapped);
                        continue;
                    },
                };

                let metadata: TableMetadata = new_doc.value().0.clone().try_into()?;

                // If the table already exists locally by name, skip this update.
                // Each node may have independently created the same table, but
                // the table number must now be globally stable.
                if snapshot
                    .table_registry
                    .table_exists(metadata.namespace, &metadata.name)
                {
                    let existing = snapshot
                        .table_registry
                        .table_mapping()
                        .namespace(metadata.namespace)
                        .id(&metadata.name)?;
                    anyhow::ensure!(
                        existing.table_number == metadata.number,
                        "Replicated table '{}' has table number {} but local table uses {}",
                        metadata.name,
                        u32::from(metadata.number),
                        u32::from(existing.table_number),
                    );
                    tracing::debug!(
                        "Skipping _tables update for '{}': already exists locally with stable \
                         table number {}",
                        metadata.name,
                        u32::from(metadata.number),
                    );
                    continue;
                }

                let local_value: DocumentObject = metadata.clone().try_into()?;

                tracing::info!(
                    "Creating replicated table '{}' with stable table number {}",
                    metadata.name,
                    u32::from(metadata.number),
                );

                // Build a new ResolvedDocument with the remapped tablet ID and
                // the globally-stable table number from the source metadata.
                let remapped_id = ResolvedDocumentId {
                    tablet_id: local_tablet,
                    document_id: update.id.document_id,
                };
                let remapped_doc =
                    ResolvedDocument::new(remapped_id, new_doc.creation_time(), local_value)?;
                let remapped = DocumentUpdate {
                    id: remapped_id,
                    old_document: update
                        .old_document
                        .as_ref()
                        .map(|d| d.to_remapped(local_tablet)),
                    new_document: Some(remapped_doc),
                };
                let (idx_updates, ..) = snapshot.update(&remapped, commit_ts)?;
                all_index_updates.extend(idx_updates);
                all_remapped_updates.push(remapped);
            }
            tracing::debug!("Applied {} _tables updates", tables_updates.len());
        }

        // Phase 2: Rebuild remap after table creation, apply remaining updates.
        let remap = build_remap(&snapshot, &delta.tablet_id_to_table_name);
        for update in &other_updates {
            let primary_tablet = update.id.tablet_id;
            let local_tablet = match remap.get(&primary_tablet) {
                Some(t) => *t,
                None => {
                    let table_name = delta.tablet_id_to_table_name.get(&primary_tablet);
                    let err = anyhow::anyhow!(
                        "Cannot apply replica delta at ts={} because table {:?} (TabletId {:?}) \
                         is not mapped locally; retry after table metadata is available",
                        u64::from(remote_ts),
                        table_name,
                        primary_tablet,
                    );
                    let _ = result.send(Err(err));
                    return Ok(());
                },
            };
            let remapped = if delta
                .tablet_id_to_table_name
                .get(&primary_tablet)
                .is_some_and(|name| name == index_table_name)
            {
                let remapped = remap_index_metadata_update(
                    update,
                    local_tablet,
                    &remap,
                    mode == ReplicaDeltaApplyMode::CrossPartitionReplica,
                )?;
                if replicated_index_update_is_already_present(&snapshot, &remapped)? {
                    tracing::debug!(
                        "Skipping replicated _index metadata for existing local index: {:?}",
                        remapped
                            .new_document
                            .as_ref()
                            .or(remapped.old_document.as_ref())
                            .map(|document| document.id()),
                    );
                    continue;
                }
                remapped
            } else {
                DocumentUpdate {
                    id: ResolvedDocumentId {
                        tablet_id: local_tablet,
                        document_id: update.id.document_id,
                    },
                    old_document: update
                        .old_document
                        .as_ref()
                        .map(|d| d.to_remapped(local_tablet)),
                    new_document: update
                        .new_document
                        .as_ref()
                        .map(|d| d.to_remapped(local_tablet)),
                }
            };
            let (idx_updates, ..) = snapshot.update(&remapped, commit_ts)?;
            all_index_updates.extend(idx_updates);
            all_remapped_updates.push(remapped);
        }

        // Phase 3: Push persistence write + publish to the ordered pipeline.
        //
        // This is the Raft pattern: ALL writes (local commits, max_repeatable
        // bumps, AND replica deltas) flow through the same persistence_writes
        // FuturesOrdered queue. The publish handler (write log append +
        // snapshot push) only runs when the persistence write completes and
        // it's the next in the queue. This prevents interleaving between
        // async local commits and synchronous delta applies — the exact bug
        // that caused timestamp ordering violations.
        //
        // CockroachDB, TiDB, YugabyteDB, and Spanner all do this: both
        // local proposals and replicated entries flow through one sequential
        // Raft log before being applied to the state machine.
        let document_writes: Vec<DocumentLogEntry> = all_remapped_updates
            .iter()
            .map(|update| DocumentLogEntry {
                ts: commit_ts,
                id: value::InternalDocumentId::new(update.id.tablet_id, update.id.internal_id()),
                value: update.new_document.clone(),
                prev_ts: None,
            })
            .collect();
        let index_writes: Vec<PersistenceIndexEntry> = all_index_updates
            .iter()
            .map(|update| PersistenceIndexEntry::from_index_update(commit_ts, update))
            .collect();

        let num_doc_writes = document_writes.len();
        let num_remapped = all_remapped_updates.len();
        let skipped = delta.document_updates.len() - num_remapped;
        let updated_replication_frontiers = delta.source_partition.map(|source_partition| {
            let mut frontiers = self.snapshot_manager.read().replication_frontiers();
            let should_update = frontiers
                .get(&source_partition)
                .is_none_or(|current| *current < commit_ts);
            if should_update {
                frontiers.insert(source_partition, commit_ts);
            }
            frontiers
        });
        let updated_applied_delta_watermarks = delta.source_partition.map(|source_partition| {
            let mut frontiers = self.applied_delta_watermarks.clone();
            if frontiers
                .get(&source_partition)
                .is_none_or(|current| *current < remote_ts)
            {
                frontiers.insert(source_partition, remote_ts);
            }
            frontiers
        });
        let updated_applied_data_delta_watermarks =
            delta.source_partition.map(|source_partition| {
                let mut frontiers = self.applied_data_delta_watermarks.clone();
                if frontiers
                    .get(&source_partition)
                    .is_none_or(|current| *current < remote_ts)
                {
                    frontiers.insert(source_partition, remote_ts);
                }
                frontiers
            });
        let updated_applied_data_delta_ids =
            (delta.source_partition.is_some() || transport_id.is_some()).then(|| {
                let mut ids = self.applied_data_delta_ids.clone();
                if let Some(source_partition) = delta.source_partition {
                    ids.insert_origin_timestamp(source_partition, remote_ts);
                }
                if let Some(transport_id) = transport_id {
                    ids.insert_transport_id(transport_id);
                }
                ids.prune_origin_timestamps_below_watermarks(
                    updated_applied_data_delta_watermarks
                        .as_ref()
                        .unwrap_or(&self.applied_data_delta_watermarks),
                );
                ids
            });
        if let Some(source_partition) = delta.source_partition {
            if self
                .applied_delta_watermarks
                .get(&source_partition)
                .is_none_or(|current| *current < remote_ts)
            {
                self.applied_delta_watermarks
                    .insert(source_partition, remote_ts);
            }
            if self
                .applied_data_delta_watermarks
                .get(&source_partition)
                .is_none_or(|current| *current < remote_ts)
            {
                self.applied_data_delta_watermarks
                    .insert(source_partition, remote_ts);
            }
            self.applied_data_delta_ids
                .insert_origin_timestamp(source_partition, remote_ts);
        }
        if let Some(transport_id) = transport_id {
            self.applied_data_delta_ids
                .insert_transport_id(transport_id);
        }
        self.applied_data_delta_ids
            .prune_origin_timestamps_below_watermarks(&self.applied_data_delta_watermarks);
        let raft_nats_outbox_delta = (mode == ReplicaDeltaApplyMode::FullRaftState
            && delta.source_partition.is_some())
        .then(|| delta.clone());

        let persistence = self.persistence.clone();
        let pause_client = self.runtime.pause_client();
        let raft_apply_writes = Self::raft_apply_persistence_writes(
            raft_apply_marker,
            raft_nats_outbox_delta.as_ref(),
            raft_state_machine_index,
        )?;
        self.enqueue_snapshot(commit_id, commit_ts, snapshot.clone());

        self.persistence_writes.push_back({
            let doc_writes = document_writes.clone();
            let idx_writes = index_writes.clone();
            let replication_frontiers = updated_replication_frontiers.clone();
            let applied_delta_watermarks = updated_applied_delta_watermarks.clone();
            let applied_data_delta_watermarks = updated_applied_data_delta_watermarks.clone();
            let applied_data_delta_ids = updated_applied_data_delta_ids.clone();
            let source_partition = delta.source_partition;
            async move {
                // Write to persistence (so function runner can load modules).
                persistence
                    .write_with_persistence_globals(
                        &doc_writes,
                        &idx_writes,
                        ConflictStrategy::Overwrite,
                        &raft_apply_writes,
                    )
                    .await?;
                if raft_apply_marker.is_some() {
                    pause_client.wait(AFTER_RAFT_CONVEX_PERSISTENCE).await;
                }

                // Advance max_repeatable_ts in persistence.
                persistence
                    .write_persistence_global(
                        PersistenceGlobalKey::MaxRepeatableTimestamp,
                        serde_json::Value::from(u64::from(commit_ts)),
                    )
                    .await?;

                if let Some(frontiers) = replication_frontiers {
                    persistence
                        .write_persistence_global(
                            PersistenceGlobalKey::ReplicationFrontiers,
                            replication_frontiers_to_json(&frontiers),
                        )
                        .await?;
                }

                Ok(PersistenceWrite::ReplicaDelta {
                    commit_ts,
                    remote_ts,
                    snapshot,
                    remapped_updates: all_remapped_updates,
                    write_source: delta.write_source,
                    write_bytes: delta.write_bytes,
                    source_partition,
                    applied_delta_watermarks,
                    applied_data_delta_watermarks,
                    applied_data_delta_ids,
                    raft_apply_marker,
                    raft_nats_outbox_delta,
                    raft_state_machine_index,
                    result,
                    commit_id,
                })
            }
            .boxed()
        });

        tracing::info!(
            "Queued replica delta for publish: ts={}, {} updates, {} skipped, {} persistence \
             writes",
            u64::from(commit_ts),
            num_remapped,
            skipped,
            num_doc_writes,
        );

        Ok(())
    }

    // ========== 2PC Handlers (Vitess prepare/commit/rollback pattern) ==========

    async fn load_two_phase_redo_records(
        persistence: Arc<dyn Persistence>,
    ) -> anyhow::Result<BTreeMap<String, crate::two_phase::TwoPhaseRedoEntry>> {
        let Some(value) = persistence
            .reader()
            .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
            .await?
        else {
            return Ok(BTreeMap::new());
        };
        serde_json::from_value(value).context("2PC: Failed to parse durable redo records")
    }

    async fn write_two_phase_redo_records(
        persistence: Arc<dyn Persistence>,
        records: &BTreeMap<String, crate::two_phase::TwoPhaseRedoEntry>,
    ) -> anyhow::Result<()> {
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::TwoPhaseRedoRecords,
                serde_json::to_value(records).context("2PC: Failed to serialize redo records")?,
            )
            .await
            .context("2PC: Failed to persist redo records")
    }

    async fn persist_two_phase_redo(
        persistence: Arc<dyn Persistence>,
        entry: crate::two_phase::TwoPhaseRedoEntry,
    ) -> anyhow::Result<()> {
        let mut records = Self::load_two_phase_redo_records(persistence.clone()).await?;
        records.insert(entry.transaction_id.clone(), entry);
        Self::write_two_phase_redo_records(persistence, &records).await
    }

    async fn persist_two_phase_redo_with_state_machine_index(
        persistence: Arc<dyn Persistence>,
        entry: crate::two_phase::TwoPhaseRedoEntry,
        raft_state_machine_index: u64,
    ) -> anyhow::Result<()> {
        let mut records = Self::load_two_phase_redo_records(persistence.clone()).await?;
        records.insert(entry.transaction_id.clone(), entry);
        let redo_value = serde_json::to_value(records)
            .context("2PC: Failed to serialize Raft-applied redo records")?;
        persistence
            .write_with_persistence_globals(
                &[],
                &[],
                ConflictStrategy::Overwrite,
                &[
                    PersistenceGlobalWrite::new(
                        String::from(PersistenceGlobalKey::TwoPhaseRedoRecords),
                        redo_value,
                    ),
                    state_machine_index_persistence_write(raft_state_machine_index),
                ],
            )
            .await
            .context("2PC: Failed to atomically persist redo and Raft state-machine index")
    }

    async fn delete_two_phase_redo(
        persistence: Arc<dyn Persistence>,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        let mut records = Self::load_two_phase_redo_records(persistence.clone()).await?;
        if records.remove(&transaction_id.0).is_some() {
            Self::write_two_phase_redo_records(persistence, &records).await?;
        }
        Ok(())
    }

    async fn persist_two_phase_rollback_with_state_machine_index(
        persistence: Arc<dyn Persistence>,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        raft_state_machine_index: u64,
    ) -> anyhow::Result<()> {
        let mut records = Self::load_two_phase_redo_records(persistence.clone()).await?;
        records.remove(&transaction_id.0);
        let redo_value = serde_json::to_value(records)
            .context("2PC: Failed to serialize Raft-applied rollback redo records")?;
        persistence
            .write_with_persistence_globals(
                &[],
                &[],
                ConflictStrategy::Overwrite,
                &[
                    PersistenceGlobalWrite::new(
                        String::from(PersistenceGlobalKey::TwoPhaseRedoRecords),
                        redo_value,
                    ),
                    state_machine_index_persistence_write(raft_state_machine_index),
                ],
            )
            .await
            .context("2PC: Failed to atomically persist rollback and Raft state-machine index")
    }

    async fn redo_entry_is_already_committed(
        persistence_reader: Arc<dyn PersistenceReader>,
        retention_validator: Arc<dyn RetentionValidator>,
        entry: &crate::two_phase::TwoPhaseRedoEntry,
        transaction: &crate::two_phase::ParticipantTransaction,
        prepare_ts: Timestamp,
    ) -> anyhow::Result<bool> {
        if transaction.writes.is_empty() {
            return Ok(false);
        }
        let expected: BTreeSet<_> = transaction
            .writes
            .iter()
            .map(|write| InternalDocumentId::new(write.id.tablet_id, write.id.internal_id()))
            .collect();
        let mut seen = BTreeSet::new();
        let mut stream = persistence_reader.load_documents(
            TimestampRange::at(prepare_ts),
            Order::Asc,
            1024,
            retention_validator,
        );
        while let Some(doc) = stream.try_next().await? {
            if expected.contains(&doc.id) {
                seen.insert(doc.id);
            }
        }
        let already_committed = seen.len() == expected.len();
        if already_committed {
            tracing::info!(
                "2PC Recovery: redo txn={} is already committed at ts={}, deleting redo",
                entry.transaction_id,
                u64::from(prepare_ts),
            );
        }
        Ok(already_committed)
    }

    fn stage_participant_prepared_transaction(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        prepare_ts: Timestamp,
        require_leader: bool,
        allow_explicit_ts_rewrite: bool,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        let local_table_mapping = self
            .snapshot_manager
            .read()
            .latest_snapshot()
            .table_mapping()
            .clone();
        Self::validate_participant_catalog_creations(
            &transaction,
            &local_table_mapping,
            &write_source,
        )?;
        let mut table_mapping = local_table_mapping.clone();
        transaction.augment_table_mapping(&mut table_mapping)?;
        let writes = Self::remap_participant_system_table_writes(
            transaction.writes,
            &table_mapping,
            &local_table_mapping,
        )?;
        self.stage_prepared_transaction(
            transaction_id,
            *transaction.begin_timestamp,
            &transaction.reads,
            writes,
            &table_mapping,
            write_source,
            Some(prepare_ts),
            require_leader,
            allow_explicit_ts_rewrite,
        )
    }

    fn validate_participant_catalog_creations(
        transaction: &crate::two_phase::ParticipantTransaction,
        local_mapping: &TableMapping,
        write_source: &WriteSource,
    ) -> anyhow::Result<()> {
        for write in &transaction.writes {
            if write.old_document.is_some() {
                continue;
            }
            let Some(new_document) = write.new_document.as_ref() else {
                continue;
            };
            let Some(catalog_metadata) = transaction.tablet_metadata.get(&write.id.tablet_id)
            else {
                continue;
            };
            if &catalog_metadata.table_name != &*TABLES_TABLE {
                continue;
            }

            let metadata: ParsedDocument<TableMetadata> =
                new_document.parse().with_context(|| {
                    format!(
                        "failed to parse _tables metadata while validating 2PC catalog write {:?}",
                        write.id
                    )
                })?;
            let metadata = metadata.into_value();
            if metadata.name.is_system() || !metadata.is_active() {
                continue;
            }

            let proposed_tablet = value::TabletId(write.id.internal_id());
            let existing_tablet = local_mapping
                .namespace(metadata.namespace)
                .id_if_exists(&metadata.name);
            if existing_tablet.is_some_and(|existing| existing != proposed_tablet) {
                let error = ErrorMetadata::system_occ(
                    Some(u64::from(*transaction.begin_timestamp)),
                    write_source.as_str().map(str::to_string),
                );
                anyhow::bail!(anyhow::anyhow!(error).context(format!(
                    "2PC catalog create for table '{}' lost a concurrent creation race; retry \
                     against the committed catalog",
                    metadata.name,
                )));
            }
        }
        Ok(())
    }

    fn validate_participant_reads(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        validate_ts: Timestamp,
    ) -> anyhow::Result<()> {
        let local_partition = self.local_partition_for_two_phase();
        let timer = metrics::two_phase_resolver_validation_timer(local_partition);
        metrics::log_two_phase_resolver_read_set_intervals(
            local_partition,
            resolver_read_set_interval_count(&transaction.reads),
        );

        let mut table_mapping = self
            .snapshot_manager
            .read()
            .latest_snapshot()
            .table_mapping()
            .clone();
        if let Err(err) = transaction.augment_table_mapping(&mut table_mapping) {
            metrics::log_two_phase_resolver_validation(local_partition, "metadata");
            timer.finish_with("metadata");
            return Err(err);
        }
        if let Err(err) = self.ensure_leader_for_writes() {
            metrics::log_two_phase_resolver_validation(local_partition, "not_leader");
            timer.finish_with("not_leader");
            return Err(err);
        }
        if !transaction.writes.is_empty() {
            metrics::log_two_phase_resolver_validation(local_partition, "invalid_request");
            timer.finish_with("invalid_request");
            anyhow::bail!(
                "2PC ValidateReads txn={} received {} writes; write participants must use Prepare",
                transaction_id,
                transaction.writes.len(),
            );
        }
        if let Err(err) =
            self.ensure_read_ownership(&transaction.reads, &table_mapping, &write_source)
        {
            metrics::log_two_phase_resolver_validation(local_partition, "ownership");
            timer.finish_with("ownership");
            return Err(err);
        }
        if let Err(err) = self.validate_remote_read_frontiers(
            *transaction.begin_timestamp,
            &transaction.reads,
            &table_mapping,
            &write_source,
        ) {
            metrics::log_two_phase_resolver_validation(local_partition, "frontier");
            timer.finish_with("frontier");
            return Err(err);
        }

        let local_commit_floor = match self.explicit_prepare_commit_floor() {
            Ok(local_commit_floor) => local_commit_floor,
            Err(err) => {
                metrics::log_two_phase_resolver_validation(local_partition, "error");
                timer.finish_with("error");
                return Err(err);
            },
        };
        if validate_ts < local_commit_floor {
            metrics::log_two_phase_resolver_validation(local_partition, "timestamp_floor");
            metrics::log_two_phase_resolver_timestamp_floor_rejection(local_partition);
            timer.finish_with("timestamp_floor");
            return Err(crate::two_phase::PrepareTimestampTooLow {
                proposed_ts: validate_ts,
                required_ts: local_commit_floor,
            }
            .into());
        }

        let stale_timer = metrics::commit_is_stale_timer();
        let conflicting_read = match self.commit_has_conflict(
            &transaction.reads,
            *transaction.begin_timestamp,
            validate_ts,
        ) {
            Ok(conflicting_read) => conflicting_read,
            Err(err) => {
                metrics::log_two_phase_resolver_validation(local_partition, "error");
                timer.finish_with("error");
                return Err(err);
            },
        };
        if let Some(conflicting_read) = conflicting_read {
            metrics::log_two_phase_resolver_validation(local_partition, "conflict");
            metrics::log_two_phase_resolver_conflict(local_partition);
            timer.finish_with("conflict");
            anyhow::bail!(conflicting_read.into_error(&table_mapping, &write_source));
        }
        stale_timer.finish();

        if validate_ts > self.last_assigned_ts {
            self.last_assigned_ts = validate_ts;
        }
        metrics::log_two_phase_resolver_validation(local_partition, "success");
        timer.finish();
        tracing::info!(
            "2PC ValidateReads: txn={}, validate_ts={}",
            transaction_id,
            u64::from(validate_ts),
        );
        Ok(())
    }

    fn remap_participant_system_table_writes(
        writes: Vec<DocumentUpdateWithPrevTs>,
        source_mapping: &TableMapping,
        local_mapping: &TableMapping,
    ) -> anyhow::Result<Vec<DocumentUpdateWithPrevTs>> {
        writes
            .into_iter()
            .map(|write| {
                let Ok(table_name) = source_mapping.tablet_name(write.id.tablet_id) else {
                    return Ok(write);
                };
                if &table_name != &*TABLES_TABLE
                    && &table_name != &*common::bootstrap_model::index::INDEX_TABLE
                {
                    return Ok(write);
                }
                let local_table = local_mapping
                    .namespace(value::TableNamespace::Global)
                    .id(&table_name)?;
                let remapped_id = ResolvedDocumentId::new(
                    local_table.tablet_id,
                    PublicDocumentId::new(local_table.table_number, write.id.internal_id()),
                );
                let old_document = match write.old_document {
                    Some((document, ts)) => {
                        Some((Self::remap_document_id(document, remapped_id)?, ts))
                    },
                    None => None,
                };
                let new_document = match write.new_document {
                    Some(document) => Some(Self::remap_document_id(document, remapped_id)?),
                    None => None,
                };
                Ok(DocumentUpdateWithPrevTs {
                    id: remapped_id,
                    old_document,
                    new_document,
                })
            })
            .collect()
    }

    fn remap_document_id(
        document: ResolvedDocument,
        remapped_id: ResolvedDocumentId,
    ) -> anyhow::Result<ResolvedDocument> {
        let mut value: BTreeMap<_, _> = document.value().0.clone().into();
        value.remove(&common::types::FieldName::from(ID_FIELD.clone()));
        ResolvedDocument::new(remapped_id, document.creation_time(), value.try_into()?)
    }

    fn stage_two_phase_redo_entry(
        &mut self,
        entry: &crate::two_phase::TwoPhaseRedoEntry,
        require_leader: bool,
        allow_explicit_ts_rewrite: bool,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        let transaction_id = entry.transaction_id();
        let prepare_ts = entry.prepare_ts()?;
        let transaction = entry.participant_transaction().with_context(|| {
            format!(
                "2PC: failed to decode participant transaction for redo txn={}",
                transaction_id
            )
        })?;
        anyhow::ensure!(
            entry.partition_id() == self.local_partition_for_two_phase(),
            "2PC redo txn={} belongs to partition {}, local partition is {}",
            transaction_id,
            entry.partition_id().0,
            self.local_partition_for_two_phase().0,
        );
        self.stage_participant_prepared_transaction(
            transaction_id,
            transaction,
            entry.write_source(),
            prepare_ts,
            require_leader,
            allow_explicit_ts_rewrite,
        )
    }

    fn stage_durable_two_phase_redo(
        &mut self,
        transaction_id: &crate::two_phase::TwoPhaseTransactionId,
        require_leader: bool,
    ) -> anyhow::Result<bool> {
        if self.prepared_transactions.contains_key(transaction_id) {
            return Ok(false);
        }
        let mut records = Self::block_on_current_runtime(Self::load_two_phase_redo_records(
            self.persistence.clone(),
        ))?;
        let Some(entry) = records.remove(&transaction_id.0) else {
            return Ok(false);
        };
        self.stage_two_phase_redo_entry(&entry, require_leader, false)
            .with_context(|| format!("2PC: failed to stage durable redo txn={transaction_id}"))?;
        Ok(true)
    }

    fn remove_prepared_transaction_at_origin_ts(
        &mut self,
        origin_commit_ts: Timestamp,
    ) -> anyhow::Result<Option<crate::two_phase::TwoPhaseTransactionId>> {
        self.remove_prepared_transaction_matching_ts(origin_commit_ts, |prepared| {
            prepared.origin_commit_ts
        })
    }

    fn local_prepared_commit_ts_for_origin(
        &self,
        origin_commit_ts: Timestamp,
    ) -> anyhow::Result<Option<Timestamp>> {
        let matching = self
            .prepared_transactions
            .iter()
            .filter_map(|(transaction_id, prepared)| {
                (prepared.origin_commit_ts == origin_commit_ts)
                    .then(|| (transaction_id.clone(), prepared.commit_ts))
            })
            .collect_vec();
        anyhow::ensure!(
            matching.len() <= 1,
            "Found {} prepared transactions at origin ts={}; expected at most one",
            matching.len(),
            u64::from(origin_commit_ts),
        );
        Ok(matching.into_iter().next().map(|(_, commit_ts)| commit_ts))
    }

    fn remove_prepared_transaction_matching_ts(
        &mut self,
        ts: Timestamp,
        ts_selector: impl Fn(&PreparedTransaction) -> Timestamp,
    ) -> anyhow::Result<Option<crate::two_phase::TwoPhaseTransactionId>> {
        let matching = self
            .prepared_transactions
            .iter()
            .filter_map(|(transaction_id, prepared)| {
                (ts_selector(prepared) == ts).then(|| transaction_id.clone())
            })
            .collect_vec();
        anyhow::ensure!(
            matching.len() <= 1,
            "Found {} prepared transactions at ts={}; expected at most one",
            matching.len(),
            u64::from(ts),
        );
        let Some(transaction_id) = matching.into_iter().next() else {
            return Ok(None);
        };
        let prepared = self
            .prepared_transactions
            .remove(&transaction_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "2PC cleanup: prepared transaction {} disappeared while cleaning ts={}",
                    transaction_id,
                    u64::from(ts),
                )
            })?;
        self.prepared_writes
            .remove(prepared.prepared_write)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "2PC cleanup: prepared intent for txn={} at ts={} was missing",
                    transaction_id,
                    u64::from(prepared.commit_ts),
                )
            })?;
        Ok(Some(transaction_id))
    }

    async fn recover_two_phase_redo_records(&mut self) -> anyhow::Result<()> {
        let records = Self::load_two_phase_redo_records(self.persistence.clone()).await?;
        if records.is_empty() {
            return Ok(());
        }

        tracing::info!(
            "2PC Recovery: rebuilding {} prepared transaction(s) from durable redo",
            records.len(),
        );
        for entry in records.values() {
            let transaction_id = entry.transaction_id();
            let prepare_ts = entry.prepare_ts()?;
            let transaction = entry.participant_transaction().with_context(|| {
                format!(
                    "2PC Recovery: failed to decode participant transaction for txn={}",
                    transaction_id
                )
            })?;
            if entry.partition_id() != self.local_partition_for_two_phase() {
                tracing::warn!(
                    "2PC Recovery: ignoring redo txn={} for partition {}, local partition is {}",
                    transaction_id,
                    entry.partition_id().0,
                    self.local_partition_for_two_phase().0,
                );
                continue;
            }
            if Self::redo_entry_is_already_committed(
                self.persistence.reader(),
                self.retention_validator.clone(),
                entry,
                &transaction,
                prepare_ts,
            )
            .await?
            {
                Self::delete_two_phase_redo(self.persistence.clone(), &transaction_id).await?;
                continue;
            }

            self.stage_two_phase_redo_entry(entry, false, true)
                .with_context(|| {
                    format!("2PC Recovery: failed to stage prepared txn={transaction_id}")
                })?;
            tracing::info!(
                "2PC Recovery: restored prepared txn={} at ts={}",
                transaction_id,
                u64::from(prepare_ts),
            );
        }
        Ok(())
    }

    /// Prepare phase: validate OCC, assign timestamp, stage writes.
    /// Does NOT publish — the transaction is held in prepared_transactions
    /// until CommitPrepared or RollbackPrepared arrives.
    async fn handle_prepare(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: FinalTransaction,
        write_source: WriteSource,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        tracing::info!(
            "2PC Prepare: txn={}, {} writes",
            transaction_id,
            transaction.writes.coalesced_writes().count(),
        );

        let participant_transaction =
            crate::two_phase::ParticipantTransaction::from_entire_final_transaction(&transaction)?;
        let result = self.stage_prepared_transaction(
            transaction_id.clone(),
            *transaction.begin_timestamp,
            transaction.reads.read_set(),
            transaction.writes.coalesced_writes().cloned().collect(),
            &transaction.table_mapping,
            write_source.clone(),
            None,
            true,
            false,
        )?;
        let redo = crate::two_phase::TwoPhaseRedoEntry::new(
            &transaction_id,
            result.prepare_ts,
            self.local_partition_for_two_phase(),
            participant_transaction,
            &write_source,
        )?;
        if let Err(err) = Self::persist_two_phase_redo(self.persistence.clone(), redo.clone()).await
        {
            if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                tracing::error!(
                    "2PC Prepare: failed to rollback in-memory txn={} after redo write failure: \
                     {rollback_err:#}",
                    transaction_id,
                );
            }
            return Err(err);
        }
        let raft_prepare_waiter = match self.propose_two_phase_prepare_to_raft(&redo) {
            Ok(waiter) => waiter,
            Err(err) => {
                if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                    tracing::error!(
                        "2PC Prepare: failed to rollback in-memory txn={} after Raft proposal \
                         failure: {rollback_err:#}",
                        transaction_id,
                    );
                }
                if let Err(delete_err) =
                    Self::delete_two_phase_redo(self.persistence.clone(), &transaction_id).await
                {
                    tracing::warn!(
                        "2PC Prepare: failed to delete redo for txn={} after Raft proposal \
                         failure: {delete_err:#}",
                        transaction_id,
                    );
                }
                return Err(err);
            },
        };
        let raft_applied_index = match Self::wait_for_raft_prepare(
            raft_prepare_waiter,
            &transaction_id,
            result.prepare_ts,
        )
        .await
        {
            Ok(index) => index,
            Err(err) => {
                if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                    tracing::error!(
                        "2PC Prepare: failed to rollback in-memory txn={} after Raft quorum \
                         failure: {rollback_err:#}",
                        transaction_id,
                    );
                }
                if let Err(delete_err) =
                    Self::delete_two_phase_redo(self.persistence.clone(), &transaction_id).await
                {
                    tracing::warn!(
                        "2PC Prepare: failed to delete redo for txn={} after Raft quorum failure: \
                         {delete_err:#}",
                        transaction_id,
                    );
                }
                return Err(err);
            },
        };
        if let Some(raft_applied_index) = raft_applied_index {
            Self::persist_two_phase_redo_with_state_machine_index(
                self.persistence.clone(),
                redo,
                raft_applied_index,
            )
            .await?;
            let Some(raft_state) = self.raft_state.as_ref() else {
                anyhow::bail!(
                    "2PC Prepare txn={} returned Raft applied index {} without Raft state",
                    transaction_id,
                    raft_applied_index,
                );
            };
            raft_state.mark_applied(raft_applied_index).await?;
        }
        Ok(result)
    }

    async fn handle_prepare_remote(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        prepare_ts: Timestamp,
        participants: Vec<crate::partition::PartitionId>,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        tracing::info!(
            "2PC Remote Prepare: txn={}, {} writes, prepare_ts={}",
            transaction_id,
            transaction.writes.len(),
            u64::from(prepare_ts),
        );

        let redo = crate::two_phase::TwoPhaseRedoEntry::new_with_participants(
            &transaction_id,
            prepare_ts,
            self.local_partition_for_two_phase(),
            &participants,
            transaction.clone(),
            &write_source,
        )?;
        let result = self.stage_participant_prepared_transaction(
            transaction_id.clone(),
            transaction,
            write_source,
            prepare_ts,
            true,
            false,
        )?;
        if let Err(err) = Self::persist_two_phase_redo(self.persistence.clone(), redo.clone()).await
        {
            if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                tracing::error!(
                    "2PC Prepare: failed to rollback in-memory txn={} after redo write failure: \
                     {rollback_err:#}",
                    transaction_id,
                );
            }
            return Err(err);
        }
        let raft_prepare_waiter = match self.propose_two_phase_prepare_to_raft(&redo) {
            Ok(waiter) => waiter,
            Err(err) => {
                if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                    tracing::error!(
                        "2PC Prepare: failed to rollback in-memory txn={} after Raft proposal \
                         failure: {rollback_err:#}",
                        transaction_id,
                    );
                }
                if let Err(delete_err) =
                    Self::delete_two_phase_redo(self.persistence.clone(), &transaction_id).await
                {
                    tracing::warn!(
                        "2PC Prepare: failed to delete redo for txn={} after Raft proposal \
                         failure: {delete_err:#}",
                        transaction_id,
                    );
                }
                return Err(err);
            },
        };
        let raft_applied_index = match Self::wait_for_raft_prepare(
            raft_prepare_waiter,
            &transaction_id,
            result.prepare_ts,
        )
        .await
        {
            Ok(index) => index,
            Err(err) => {
                if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                    tracing::error!(
                        "2PC Prepare: failed to rollback in-memory txn={} after Raft quorum \
                         failure: {rollback_err:#}",
                        transaction_id,
                    );
                }
                if let Err(delete_err) =
                    Self::delete_two_phase_redo(self.persistence.clone(), &transaction_id).await
                {
                    tracing::warn!(
                        "2PC Prepare: failed to delete redo for txn={} after Raft quorum failure: \
                         {delete_err:#}",
                        transaction_id,
                    );
                }
                return Err(err);
            },
        };
        if let Some(raft_applied_index) = raft_applied_index {
            Self::persist_two_phase_redo_with_state_machine_index(
                self.persistence.clone(),
                redo,
                raft_applied_index,
            )
            .await?;
            let Some(raft_state) = self.raft_state.as_ref() else {
                anyhow::bail!(
                    "2PC Prepare txn={} returned Raft applied index {} without Raft state",
                    transaction_id,
                    raft_applied_index,
                );
            };
            raft_state.mark_applied(raft_applied_index).await?;
        }
        Ok(result)
    }

    fn defer_prepare_admission(
        deferred_prepares: &mut VecDeque<DeferredPrepare>,
        deferred_prepare: DeferredPrepare,
        blocking_ts: Timestamp,
    ) {
        tracing::debug!(
            transaction_id = %deferred_prepare.transaction_id(),
            kind = deferred_prepare.kind(),
            blocking_ts = u64::from(blocking_ts),
            deferred_prepares = deferred_prepares.len() + 1,
            "Deferring 2PC prepare admission behind earlier unpublished state-machine write",
        );
        deferred_prepares.push_back(deferred_prepare);
    }

    async fn handle_deferred_prepare(&mut self, deferred_prepare: DeferredPrepare) {
        match deferred_prepare {
            DeferredPrepare::ApplyRaftPreparedRedo {
                raft_state_machine_index,
                redo,
                result,
            } => {
                let r = self.handle_raft_prepared_redo(raft_state_machine_index, redo);
                let _ = result.send(r);
            },
            DeferredPrepare::Prepare {
                transaction_id,
                transaction,
                write_source,
                result,
            } => {
                let r = self
                    .handle_prepare(transaction_id, transaction, write_source)
                    .await;
                let _ = result.send(r);
            },
            DeferredPrepare::PrepareRemote {
                transaction_id,
                transaction,
                write_source,
                prepare_ts,
                participants,
                result,
            } => {
                let r = self
                    .handle_prepare_remote(
                        transaction_id,
                        transaction,
                        write_source,
                        prepare_ts,
                        participants,
                    )
                    .await;
                let _ = result.send(r);
            },
        }
    }

    fn handle_validate_remote_reads(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        validate_ts: Timestamp,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "2PC ValidateReads: txn={}, {} indexed reads, validate_ts={}",
            transaction_id,
            transaction.reads.iter_indexed().count(),
            u64::from(validate_ts),
        );
        self.validate_participant_reads(transaction_id, transaction, write_source, validate_ts)
    }

    /// Start committing a previously prepared transaction without blocking the
    /// committer loop on Raft quorum or persistence I/O.
    fn start_commit_prepared(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
        commit_id: usize,
    ) -> Option<BoxFuture<'static, anyhow::Result<PersistenceWrite>>> {
        if self.prepared_rollback_waiters.contains_key(&transaction_id) {
            let _ = result.send(Err(anyhow::anyhow!(
                "2PC CommitPrepared: transaction {} is already rolling back",
                transaction_id,
            )));
            return None;
        }
        if let Some(waiters) = self.prepared_commit_waiters.get_mut(&transaction_id) {
            tracing::debug!(
                "2PC CommitPrepared: txn={} already committing; attaching duplicate waiter",
                transaction_id,
            );
            waiters.push(result);
            return None;
        }

        let redo_stage_timer =
            metrics::commit_hot_path_stage_timer("2pc_participant", "two_phase_redo");
        if let Err(err) = self.stage_durable_two_phase_redo(&transaction_id, true) {
            let _ = result.send(Err(err));
            return None;
        }
        redo_stage_timer.finish();
        let prepared = self
            .prepared_transactions
            .get(&transaction_id)
            .ok_or_else(|| {
                anyhow::anyhow!("2PC CommitPrepared: unknown transaction {}", transaction_id)
            });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                let _ = result.send(Err(err));
                return None;
            },
        };
        let commit_ts = prepared.commit_ts;
        let write_bytes = prepared.write_bytes;
        if let Some(min_prepared_ts) = self.prepared_writes.min_ts()
            && min_prepared_ts < commit_ts
        {
            tracing::debug!(
                "2PC CommitPrepared: deferring txn={} at ts={} behind unresolved prepared ts={}",
                transaction_id,
                u64::from(commit_ts),
                u64::from(min_prepared_ts),
            );
            let sender = self.sender.clone();
            let rt = self.runtime.clone();
            tokio_spawn(
                "defer_commit_prepared_until_prior_intent_resolves",
                async move {
                    rt.wait(Duration::from_millis(5)).await;
                    let message = CommitterMessage::CommitPrepared {
                        transaction_id,
                        result,
                    };
                    if let Err(err) = sender.try_send(message) {
                        match err {
                            TrySendError::Full(CommitterMessage::CommitPrepared {
                                result, ..
                            }) => {
                                let _ = result.send(Err(metrics::committer_full_error().into()));
                            },
                            TrySendError::Closed(CommitterMessage::CommitPrepared {
                                result,
                                ..
                            }) => {
                                let _ = result.send(Err(metrics::shutdown_error()));
                            },
                            _ => {
                                unreachable!("deferred CommitPrepared requeued a different message")
                            },
                        }
                    }
                },
            );
            return None;
        }

        tracing::info!(
            "2PC CommitPrepared: txn={}, ts={}",
            transaction_id,
            u64::from(commit_ts),
        );

        let (ordered_updates, write_source, snapshot) = match self.prepared_writes.get(commit_ts) {
            Some(write) => write,
            None => {
                let _ = result.send(Err(anyhow::anyhow!(
                    "2PC CommitPrepared: prepared intent for txn={} at ts={} was missing",
                    transaction_id,
                    u64::from(commit_ts),
                )));
                return None;
            },
        };
        let source_partition = self
            .placement_state
            .as_ref()
            .map(|placement_state| placement_state.local_partition());
        let delta = Self::build_commit_delta(
            commit_ts,
            &ordered_updates,
            write_source.clone(),
            &snapshot,
            write_bytes,
            prepared.document_writes.clone(),
            prepared.index_writes.clone(),
            source_partition,
        );
        let raft_commit_waiter = match self.propose_commit_to_raft(&delta) {
            Ok(waiter) => waiter,
            Err(err) => {
                let _ = result.send(Err(err));
                return None;
            },
        };

        self.prepared_commit_waiters
            .insert(transaction_id.clone(), vec![result]);

        let persistence = self.persistence.clone();
        let pause_client = self.runtime.pause_client();
        let rt = self.runtime.clone();
        Some(
            async move {
                let raft_applied_index = match Self::wait_for_raft_commit(
                    raft_commit_waiter,
                    commit_ts,
                    "2pc_participant",
                )
                .await
                {
                    Ok(index) => index,
                    Err(err) => {
                        return Ok(PersistenceWrite::PreparedRejectedBeforePersistence {
                            transaction_id,
                            commit_id,
                            err,
                        });
                    },
                };
                let raft_apply_marker = raft_applied_index
                    .map(|_| RaftApplyMarker::from_delta(&delta))
                    .transpose()?;
                let persistence_stage_timer =
                    metrics::commit_hot_path_stage_timer("2pc_participant", "persistence_write");
                let mut backoff = Backoff::new(
                    INITIAL_PERSISTENCE_WRITES_BACKOFF,
                    MAX_PERSISTENCE_WRITES_BACKOFF,
                );
                loop {
                    let name = "CommitPrepared::write_to_persistence";
                    let handle = AbortOnDropHandle::new(tokio_spawn(
                        name,
                        Self::write_to_persistence(
                            persistence.clone(),
                            delta.index_writes.clone(),
                            delta.document_writes.clone(),
                            delta.write_source.clone(),
                            raft_apply_marker,
                            raft_apply_marker.map(|_| delta.clone()),
                            raft_applied_index,
                        )
                        .in_span(Span::enter_with_local_parent(name)),
                    ));
                    if let Err(mut e) = handle.await? {
                        if e.is::<DatabaseTimeoutError>() || e.is::<DatabaseOperationalError>() {
                            let delay = backoff.fail(&mut rt.rng());
                            tracing::error!(
                                "Failed to commit prepared transaction because database timed out"
                            );
                            report_error(&mut e).await;
                            rt.wait(delay).await;
                        } else {
                            return Err(e);
                        }
                    } else {
                        if raft_apply_marker.is_some() {
                            pause_client.wait(AFTER_RAFT_CONVEX_PERSISTENCE).await;
                        }
                        pause_client.wait(AFTER_PENDING_WRITE_SNAPSHOT).await;
                        persistence_stage_timer.finish();
                        return Ok(PersistenceWrite::PreparedCommit {
                            transaction_id,
                            commit_id,
                            delta,
                            raft_applied_index,
                            raft_apply_marker,
                        });
                    }
                }
            }
            .boxed(),
        )
    }

    /// Publish a prepared transaction after Raft quorum and persistence have
    /// already succeeded.
    fn publish_prepared_commit(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        delta: CommitDelta,
        raft_applied_index: Option<u64>,
    ) -> anyhow::Result<PublishedCommit> {
        let commit_ts = delta.ts;
        let prepared = self
            .prepared_transactions
            .remove(&transaction_id)
            .ok_or_else(|| {
                anyhow::anyhow!("2PC CommitPrepared: unknown transaction {}", transaction_id)
            })?;
        let (intent_ts, ordered_updates, write_source, snapshot) = self
            .prepared_writes
            .remove(prepared.prepared_write)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "2PC CommitPrepared: prepared intent for txn={} at ts={} was missing",
                    transaction_id,
                    u64::from(prepared.commit_ts),
                )
            })?;
        anyhow::ensure!(
            intent_ts == prepared.commit_ts,
            "2PC CommitPrepared: prepared intent for txn={} had ts={} but transaction expected \
             ts={}",
            transaction_id,
            intent_ts,
            prepared.commit_ts,
        );
        anyhow::ensure!(
            commit_ts == prepared.commit_ts,
            "2PC CommitPrepared: persisted delta for txn={} had ts={} but transaction expected \
             ts={}",
            transaction_id,
            commit_ts,
            prepared.commit_ts,
        );

        // Publish commit — makes writes visible to reads.
        let mut published_commit = self.publish_commit_from_updates(
            commit_ts,
            ordered_updates,
            write_source,
            snapshot,
            delta,
            "2pc_participant",
        )?;
        published_commit.raft_applied_index = raft_applied_index;
        Ok(published_commit)
    }

    /// Rollback a previously prepared transaction: remove from PendingWrites.
    fn handle_rollback_prepared(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.prepared_commit_waiters.contains_key(&transaction_id),
            "2PC RollbackPrepared: transaction {} is already committing",
            transaction_id,
        );

        if !self.prepared_transactions.contains_key(&transaction_id)
            && Self::block_on_current_runtime(Self::load_two_phase_redo_records(
                self.persistence.clone(),
            ))?
            .contains_key(&transaction_id.0)
        {
            tracing::info!(
                "2PC RollbackPrepared: txn={} only existed as durable redo; deleting redo",
                transaction_id,
            );
            Self::block_on_current_runtime(Self::delete_two_phase_redo(
                self.persistence.clone(),
                &transaction_id,
            ))?;
            return Ok(());
        }

        let Some(prepared) = self.prepared_transactions.remove(&transaction_id) else {
            tracing::debug!(
                "2PC RollbackPrepared: txn={} was already absent on this Raft replica",
                transaction_id,
            );
            return Ok(());
        };

        tracing::info!(
            "2PC RollbackPrepared: txn={}, ts={}",
            transaction_id,
            u64::from(prepared.commit_ts),
        );

        // Remove this exact prepared intent so it no longer blocks conflicting
        // transactions. Prepared intents are not stored in PendingWrites, whose
        // FIFO is reserved for normal commit publication.
        self.prepared_writes
            .remove(prepared.prepared_write)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "2PC RollbackPrepared: prepared intent for txn={} at ts={} was missing",
                    transaction_id,
                    u64::from(prepared.commit_ts),
                )
            })?;

        Ok(())
    }

    /// Queue a rollback marker without waiting inside the single committer
    /// loop. Raft applies entries in order, so synchronously waiting here
    /// can deadlock behind an earlier prepared commit that still needs this
    /// loop to publish and send `MarkApplied`.
    fn start_rollback_prepared_through_raft(
        &mut self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        result: oneshot::Sender<anyhow::Result<()>>,
        commit_id: usize,
    ) -> Option<BoxFuture<'static, anyhow::Result<PersistenceWrite>>> {
        if let Some(waiters) = self.prepared_rollback_waiters.get_mut(&transaction_id) {
            waiters.push(result);
            return None;
        }
        if self.prepared_commit_waiters.contains_key(&transaction_id) {
            let _ = result.send(Err(anyhow::anyhow!(
                "2PC RollbackPrepared: transaction {} is already committing",
                transaction_id,
            )));
            return None;
        }
        if let Err(err) = self.ensure_leader_for_writes() {
            let _ = result.send(Err(err));
            return None;
        }

        // A prepare is a replicated state-machine entry, so its abort must be
        // one too. The local leader may no longer have the intent after a
        // failover, but followers can still have replayed it; always append the
        // idempotent rollback marker before declaring the partition resolved.
        let raft_rollback_waiter = match self.propose_two_phase_rollback_to_raft(&transaction_id) {
            Ok(waiter) => waiter,
            Err(err) => {
                let _ = result.send(Err(err));
                return None;
            },
        };
        self.prepared_rollback_waiters
            .insert(transaction_id.clone(), vec![result]);

        Some(
            async move {
                match Self::wait_for_raft_rollback(raft_rollback_waiter, &transaction_id).await {
                    Ok(raft_applied_index) => Ok(PersistenceWrite::PreparedRollback {
                        transaction_id,
                        commit_id,
                        raft_applied_index,
                    }),
                    Err(err) => Ok(PersistenceWrite::PreparedRollbackRejected {
                        transaction_id,
                        commit_id,
                        err,
                    }),
                }
            }
            .boxed(),
        )
    }

    async fn handle_raft_rollback(
        &mut self,
        raft_state_machine_index: u64,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "2PC Raft Rollback Apply: txn={}, index={}",
            transaction_id,
            raft_state_machine_index,
        );
        self.handle_rollback_prepared(transaction_id.clone())?;
        Self::persist_two_phase_rollback_with_state_machine_index(
            self.persistence.clone(),
            &transaction_id,
            raft_state_machine_index,
        )
        .await
    }

    fn handle_raft_prepared_redo(
        &mut self,
        raft_state_machine_index: u64,
        redo: crate::two_phase::TwoPhaseRedoEntry,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        let transaction_id = redo.transaction_id();
        let prepare_ts = redo.prepare_ts()?;
        tracing::info!(
            "2PC Raft Prepare Apply: txn={}, ts={}",
            transaction_id,
            u64::from(prepare_ts),
        );

        let result = self
            .stage_two_phase_redo_entry(&redo, false, true)
            .with_context(|| {
                format!("2PC Raft Prepare Apply: failed to stage txn={transaction_id}")
            })?;
        if let Err(err) =
            Self::block_on_current_runtime(Self::persist_two_phase_redo_with_state_machine_index(
                self.persistence.clone(),
                redo,
                raft_state_machine_index,
            ))
        {
            if let Err(rollback_err) = self.handle_rollback_prepared(transaction_id.clone()) {
                tracing::error!(
                    "2PC Raft Prepare Apply: failed to rollback in-memory txn={} after redo \
                     persistence failure: {rollback_err:#}",
                    transaction_id,
                );
            }
            return Err(err);
        }
        Ok(result)
    }

    #[fastrace::trace]
    /// Returns a future to add to the pending_writes queue, if the commit
    /// should be written.
    fn start_commit(
        &mut self,
        transaction: FinalTransaction,
        result: oneshot::Sender<anyhow::Result<CommitOutcome>>,
        write_source: WriteSource,
        parent_trace: EncodedSpan,
        commit_id: usize,
        root_span: &Span,
    ) -> Option<BoxFuture<'static, anyhow::Result<PersistenceWrite>>> {
        // Skip read-only transactions.
        if transaction.is_readonly() {
            let _ = result.send(Ok(CommitOutcome {
                ts: *transaction.begin_timestamp,
                source_partition: self
                    .placement_state
                    .as_ref()
                    .map(|placement_state| placement_state.local_partition()),
                read_after_write_partitions: Vec::new(),
                invalidated_tables: BTreeSet::new(),
            }));
            return None;
        }
        let commit_timer = metrics::commit_timer();
        metrics::log_write_tx(&transaction);

        // Trace if the transaction has a lot of read intervals.
        if transaction.reads.num_intervals() > *TRANSACTION_WARN_READ_SET_INTERVALS {
            tracing::warn!(
                "Transaction with write source {write_source:?} has {} read intervals: {}",
                transaction.reads.num_intervals(),
                transaction.reads.top_three_intervals()
            );
        }

        let table_mapping = transaction.table_mapping.clone();
        let component_registry = transaction.component_registry.clone();
        let usage_tracking = transaction.usage_tracker.clone();
        let ValidatedCommit {
            index_writes,
            document_writes,
            pending_write,
        } = match block_in_place(|| self.validate_commit(transaction, write_source.clone())) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(e));
                return None;
            },
        };
        let queued_snapshot = self
            .pending_writes
            .latest_snapshot()
            .expect("validated commit should stage a pending snapshot");
        let commit_ts = pending_write.must_commit_ts();
        self.enqueue_snapshot(commit_id, commit_ts, queued_snapshot);
        let virtual_system_mapping = self.virtual_system_mapping.clone();

        Self::track_commit(
            usage_tracking,
            &index_writes,
            &document_writes,
            &table_mapping,
            &component_registry,
            &virtual_system_mapping,
        );

        let mut write_bytes: u64 = 0;
        let document_writes = Arc::new(
            document_writes
                .into_iter()
                .map(|write| {
                    let entry = DocumentLogEntry {
                        ts: write.commit_ts,
                        id: write.id,
                        value: write.write,
                        prev_ts: write.prev_ts,
                    };
                    write_bytes += entry.size();
                    entry
                })
                .collect_vec(),
        );
        let index_writes = Arc::new(
            index_writes
                .into_iter()
                .map(|(ts, update)| {
                    let entry = PersistenceIndexEntry::from_index_update(ts, &update);
                    write_bytes += entry.size();
                    entry
                })
                .collect_vec(),
        );
        let (ordered_updates, staged_write_source, staged_snapshot) = self
            .pending_writes
            .get(commit_ts)
            .expect("validated commit should remain pending");
        let source_partition = self
            .placement_state
            .as_ref()
            .map(|placement_state| placement_state.local_partition());
        let delta = Self::build_commit_delta(
            commit_ts,
            &ordered_updates,
            staged_write_source.clone(),
            &staged_snapshot,
            write_bytes,
            document_writes,
            index_writes,
            source_partition,
        );
        let raft_commit_waiter = match self.propose_commit_to_raft(&delta) {
            Ok(waiter) => waiter,
            Err(err) => {
                return Some(
                    async move {
                        Ok(PersistenceWrite::RejectedBeforePersistence {
                            pending_write,
                            commit_timer,
                            result,
                            commit_id,
                            err,
                        })
                    }
                    .boxed(),
                );
            },
        };

        // necessary because this value is moved
        let parent_trace_copy = parent_trace.clone();
        let persistence = self.persistence.clone();
        let request_span =
            initialize_root_from_parent("Committer::persistence_writes_future", parent_trace);
        let outer_span = Span::enter_with_parents("outer_write_commit", [root_span, &request_span]);
        let pause_client = self.runtime.pause_client();
        let rt = self.runtime.clone();
        Some(
            async move {
                let raft_applied_index = match Self::wait_for_raft_commit(
                    raft_commit_waiter,
                    commit_ts,
                    "local",
                )
                .await
                {
                    Ok(index) => index,
                    Err(err) => {
                        return Ok(PersistenceWrite::RejectedBeforePersistence {
                            pending_write,
                            commit_timer,
                            result,
                            commit_id,
                            err,
                        });
                    },
                };
                let raft_apply_marker = raft_applied_index
                    .map(|_| RaftApplyMarker::from_delta(&delta))
                    .transpose()?;
                let persistence_stage_timer =
                    metrics::commit_hot_path_stage_timer("local", "persistence_write");
                let mut backoff = Backoff::new(
                    INITIAL_PERSISTENCE_WRITES_BACKOFF,
                    MAX_PERSISTENCE_WRITES_BACKOFF,
                );
                loop {
                    // Inline try_join so we don't recapture the stacktrace on error
                    let name = "Commit::write_to_persistence";
                    let handle = AbortOnDropHandle::new(tokio_spawn(
                        name,
                        Self::write_to_persistence(
                            persistence.clone(),
                            delta.index_writes.clone(),
                            delta.document_writes.clone(),
                            delta.write_source.clone(),
                            raft_apply_marker,
                            raft_apply_marker.map(|_| delta.clone()),
                            raft_applied_index,
                        )
                        .in_span(Span::enter_with_local_parent(name)),
                    ));
                    if let Err(mut e) = handle.await? {
                        if e.is::<DatabaseTimeoutError>() || e.is::<DatabaseOperationalError>() {
                            let delay = backoff.fail(&mut rt.rng());
                            tracing::error!(
                                "Failed to write to persistence because database timed out"
                            );
                            report_error(&mut e).await;
                            rt.wait(delay).await;
                        } else {
                            return Err(e);
                        }
                    } else {
                        if raft_apply_marker.is_some() {
                            pause_client.wait(AFTER_RAFT_CONVEX_PERSISTENCE).await;
                        }
                        pause_client.wait(AFTER_PENDING_WRITE_SNAPSHOT).await;
                        persistence_stage_timer.finish();
                        return Ok(PersistenceWrite::Commit {
                            pending_write,
                            commit_timer,
                            result,
                            parent_trace: parent_trace_copy,
                            commit_id,
                            delta,
                            raft_applied_index,
                            raft_apply_marker,
                        });
                    }
                }
            }
            .in_span(outer_span)
            .in_span(request_span)
            .boxed(),
        )
    }

    #[fastrace::trace]
    fn track_commit(
        usage_tracker: FunctionUsageTracker,
        index_writes: &BTreeSet<(Timestamp, DatabaseIndexUpdate)>,
        document_writes: &Vec<ValidatedDocumentWrite>,
        table_mapping: &TableMapping,
        component_registry: &ComponentRegistry,
        virtual_system_mapping: &VirtualSystemMapping,
    ) {
        for (_, index_write) in index_writes {
            if let DatabaseIndexValue::NonClustered(doc) = index_write.value {
                let tablet_id = doc.tablet_id;
                let Ok(table_namespace) = table_mapping.tablet_namespace(tablet_id) else {
                    continue;
                };
                let component_id = ComponentId::from(table_namespace);
                let component_path = component_registry
                    .get_component_path(component_id, &mut TransactionReadSet::new())
                    // It's possible that the component gets deleted in this transaction. In that case, miscount the usage as root.
                    .unwrap_or(ComponentPath::root());
                if let Ok(table_name) = table_mapping.tablet_name(tablet_id) {
                    // Index metadata is never a vector
                    // Database bandwidth for index writes
                    usage_tracker.track_database_ingress(
                        component_path.clone(),
                        table_name.to_string(),
                        index_write.key.size() as u64,
                        // Exclude indexes on system tables or reserved system indexes on user
                        // tables
                        table_name.is_system() || index_write.is_system_index,
                    );
                    usage_tracker.track_database_ingress_v2(
                        component_path,
                        virtual_system_mapping
                            .associated_virtual_table_name(&table_name)
                            .unwrap_or(&table_name)
                            .to_string(),
                        index_write.key.size() as u64,
                        // Exclude indexes on system tables that are not virtual tables or reserved
                        // system indexes on user tables
                        (table_name.is_system()
                            && !virtual_system_mapping.has_virtual_table(&table_name))
                            || index_write.is_system_index,
                    );
                }
            }
        }
        for validated_write in document_writes {
            let ValidatedDocumentWrite {
                write: document,
                vector_index_write_size,
                text_index_write_size,
                ..
            } = validated_write;
            if let Some(document) = document {
                let document_write_size = document.size();
                let tablet_id = document.id().tablet_id;
                let Ok(table_namespace) = table_mapping.tablet_namespace(tablet_id) else {
                    continue;
                };
                let component_id = ComponentId::from(table_namespace);
                let component_path = component_registry
                    .get_component_path(component_id, &mut TransactionReadSet::new())
                    // It's possible that the component gets deleted in this transaction. In that case, miscount the usage as root.
                    .unwrap_or(ComponentPath::root());
                if let Ok(table_name) = table_mapping.tablet_name(tablet_id) {
                    // Database bandwidth for document writes
                    usage_tracker.track_database_ingress(
                        component_path.clone().clone(),
                        table_name.to_string(),
                        document_write_size as u64,
                        table_name.is_system(),
                    );
                    usage_tracker.track_database_ingress_v2(
                        component_path.clone(),
                        virtual_system_mapping
                            .associated_virtual_table_name(&table_name)
                            .unwrap_or(&table_name)
                            .to_string(),
                        document_write_size as u64,
                        table_name.is_system()
                            && !virtual_system_mapping.has_virtual_table(&table_name),
                    );
                    if vector_index_write_size.0 > 0 {
                        usage_tracker.track_vector_ingress(
                            component_path.clone(),
                            table_name.to_string(),
                            document_write_size as u64,
                            vector_index_write_size.0,
                            table_name.is_system(),
                        );
                    }
                    if text_index_write_size.0 > 0 {
                        usage_tracker.track_text_ingress(
                            component_path.clone(),
                            table_name.to_string(),
                            text_index_write_size.0,
                            table_name.is_system(),
                        );
                    }
                }
            }
        }
    }

    fn next_commit_ts(&mut self) -> anyhow::Result<Timestamp> {
        self.next_commit_ts_at_or_after(Timestamp::MIN)
    }

    fn next_commit_ts_at_or_after(
        &mut self,
        requested_floor: Timestamp,
    ) -> anyhow::Result<Timestamp> {
        let _timer = next_commit_ts_seconds();
        let local_floor = cmp::max(self.local_commit_floor()?, requested_floor);
        let timestamp_stage_path = if self.timestamp_oracle.is_some() {
            "tso"
        } else {
            "local"
        };
        let timestamp_stage_timer =
            metrics::commit_hot_path_stage_timer(timestamp_stage_path, "timestamp_allocate");

        // When a global TSO is configured (TiDB PD pattern), draw timestamps
        // from it instead of the local clock. This ensures globally unique,
        // monotonically increasing timestamps across all nodes in the cluster.
        // The BatchTimestampOracle reserves globally unique ranges and refreshes
        // the committed floor at batch/leadership boundaries so in-batch
        // assignment stays local.
        if let Some(ref tso) = self.timestamp_oracle {
            let tso = tso.clone();
            let ts = block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                if rt.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                    futures::executor::block_on(tso.next_ts_at_or_after(local_floor))
                } else {
                    rt.block_on(tso.next_ts_at_or_after(local_floor))
                }
            })?;
            anyhow::ensure!(
                ts >= local_floor,
                "TSO returned ts={} below requested local floor {}",
                ts,
                local_floor,
            );
            self.last_assigned_ts = ts;
            timestamp_stage_timer.finish();
            return Ok(ts);
        }

        // Single-node mode: existing behavior (local clock + monotonic counter).
        let max = cmp::max(local_floor, self.runtime.generate_timestamp()?);
        self.last_assigned_ts = max;
        timestamp_stage_timer.finish();
        Ok(max)
    }

    fn local_commit_floor(&self) -> anyhow::Result<Timestamp> {
        let log_floor = self.log.max_ts().succ()?;
        let latest_ts = self.snapshot_manager.read().latest_ts();
        Ok(cmp::max(
            log_floor,
            cmp::max(latest_ts.succ()?, self.last_assigned_ts.succ()?),
        ))
    }

    fn explicit_prepare_commit_floor(&self) -> anyhow::Result<Timestamp> {
        let log_floor = self.log.max_ts().succ()?;
        let latest_ts = self.snapshot_manager.read().latest_ts();
        let read_barrier_floor = self.read_barrier_floor.succ()?;
        let queued_floor = self
            .latest_queued_snapshot()
            .map(|(ts, _)| ts.succ())
            .transpose()?
            .unwrap_or(Timestamp::MIN);
        Ok(cmp::max(
            read_barrier_floor,
            cmp::max(log_floor, cmp::max(latest_ts.succ()?, queued_floor)),
        ))
    }

    fn replayed_prepare_commit_floor(&self) -> anyhow::Result<Timestamp> {
        let log_floor = self.log.max_ts().succ()?;
        let latest_ts = self.snapshot_manager.read().latest_ts();
        let prepared_floor = self
            .prepared_writes
            .max_ts()
            .map(|ts| ts.succ())
            .transpose()?
            .unwrap_or(Timestamp::MIN);
        Ok(cmp::max(
            prepared_floor,
            cmp::max(log_floor, latest_ts.succ()?),
        ))
    }

    fn try_next_max_repeatable_ts(&mut self) -> anyhow::Result<Option<Timestamp>> {
        if !self.prepared_transactions.is_empty() {
            let current = *self.snapshot_manager.read().persisted_max_repeatable_ts();
            tracing::debug!(
                prepared_transactions = self.prepared_transactions.len(),
                current = u64::from(current),
                "Skipping max repeatable timestamp bump while 2PC prepared transactions are \
                 unresolved",
            );
            Ok(Some(current))
        } else if let Some(min_pending) = self.pending_writes.min_ts() {
            // If there's a pending write, push max_repeatable_ts to be right
            // before the pending write, so followers can choose recent
            // timestamps but can't read at the timestamp of the pending write.
            anyhow::ensure!(min_pending <= self.last_assigned_ts);
            Ok(Some(min_pending.pred()?))
        } else {
            // Optional maintenance may use the same globally reserved timestamp
            // space as commits, but it must not wait indefinitely for a new TSO
            // batch while holding the single-threaded committer loop.
            self.try_next_maintenance_ts()
        }
    }

    fn try_next_maintenance_ts(&mut self) -> anyhow::Result<Option<Timestamp>> {
        let Some(tso) = self.timestamp_oracle.clone() else {
            return self.next_commit_ts().map(Some);
        };
        let local_floor = self.local_commit_floor()?;
        let ts = match tso.try_next_ts_at_or_after(local_floor)? {
            Some(ts) => ts,
            None => {
                self.prefetch_maintenance_tso_batch(tso, local_floor);
                return Ok(None);
            },
        };
        anyhow::ensure!(
            ts >= local_floor,
            "TSO returned maintenance ts={} below requested local floor {}",
            ts,
            local_floor,
        );
        self.last_assigned_ts = ts;
        Ok(Some(ts))
    }

    fn prefetch_maintenance_tso_batch(
        &self,
        tso: Arc<dyn crate::timestamp_oracle::TimestampOracle>,
        local_floor: Timestamp,
    ) {
        if self
            .maintenance_tso_refill_in_flight
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let refill_in_flight = self.maintenance_tso_refill_in_flight.clone();
        tokio_spawn("prefetch_maintenance_tso_batch", async move {
            if let Err(error) = tso.prefetch_ts_at_or_after(local_floor).await {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "Failed to prefetch timestamp batch for optional maintenance",
                );
            }
            refill_in_flight.store(false, Ordering::Release);
        });
    }
}

struct ValidatedDocumentWrite {
    commit_ts: Timestamp,
    id: InternalDocumentId,
    write: Option<ResolvedDocument>,
    vector_index_write_size: VectorIndexWriteSize,
    text_index_write_size: TextIndexWriteSize,
    prev_ts: Option<Timestamp>,
}

#[derive(Clone)]
pub struct CommitterClient {
    handle: Arc<Mutex<Box<dyn SpawnHandle>>>,
    sender: mpsc::Sender<CommitterMessage>,
    persistence_reader: Arc<dyn PersistenceReader>,
    retention_validator: Arc<dyn RetentionValidator>,
    snapshot_reader: Reader<SnapshotManager>,
    placement_state: Option<crate::partition::PlacementState>,
    node_addresses: Arc<RwLock<Option<crate::two_phase::NodeAddresses>>>,
    two_phase_decision_log: Arc<dyn crate::two_phase::TwoPhaseDecisionLog>,
    two_phase_client_pool: Arc<crate::two_phase::TwoPhaseCommitGrpcClientPool>,
}

impl CommitterClient {
    pub fn local_partition(&self) -> Option<crate::partition::PartitionId> {
        self.placement_state
            .as_ref()
            .map(|placement_state| placement_state.local_partition())
    }

    /// Snapshot the placement map for one transaction or internal request.
    /// Callers keep this value pinned so a placement refresh cannot split one
    /// operation across ownership versions.
    pub fn partition_map(&self) -> Option<crate::partition::PartitionMap> {
        self.placement_state
            .as_ref()
            .map(|placement_state| placement_state.partition_map())
    }

    pub async fn finish_search_and_vector_bootstrap(
        &self,
        bootstrapped_indexes: BootstrappedSearchIndexes,
        bootstrap_ts: RepeatableTimestamp,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::FinishTextAndVectorBootstrap {
            bootstrapped_indexes,
            bootstrap_ts,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        // The only reason we might fail here if the committer is shutting down.
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    /// Send a replica delta through the Committer's apply loop.
    /// Called by the NATS replica consumer to feed cross-partition deltas into
    /// the single-writer state machine. This path intentionally filters
    /// node-local system tables that are not part of cross-partition read
    /// replication.
    pub async fn apply_replica_delta(&self, delta: CommitDelta) -> anyhow::Result<Timestamp> {
        self.apply_delta_with_mode(
            delta,
            ReplicaDeltaApplyMode::CrossPartitionReplica,
            None,
            None,
        )
        .await
    }

    pub async fn apply_replica_delta_with_transport_id(
        &self,
        delta: CommitDelta,
        transport_id: ReplicationTransportId,
    ) -> anyhow::Result<Timestamp> {
        self.apply_delta_with_mode(
            delta,
            ReplicaDeltaApplyMode::CrossPartitionReplica,
            None,
            Some(transport_id),
        )
        .await
    }

    /// Apply a same-partition Raft commit delta through the Committer's apply
    /// loop. Unlike cross-partition NATS replication, Raft followers are HA
    /// copies that can become leader, so this path applies full partition
    /// state including system tables.
    pub async fn apply_raft_commit_delta(
        &self,
        raft_state_machine_index: u64,
        delta: CommitDelta,
    ) -> anyhow::Result<Timestamp> {
        self.apply_delta_with_mode(
            delta,
            ReplicaDeltaApplyMode::FullRaftState,
            Some(raft_state_machine_index),
            None,
        )
        .await
    }

    async fn apply_delta_with_mode(
        &self,
        delta: CommitDelta,
        mode: ReplicaDeltaApplyMode,
        raft_state_machine_index: Option<u64>,
        transport_id: Option<ReplicationTransportId>,
    ) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::ApplyReplicaDelta {
            delta,
            mode,
            raft_state_machine_index,
            transport_id,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    /// Apply a 2PC prepare redo record that has reached this node through
    /// Raft. This installs the prepared intent through the same single-writer
    /// committer loop used by local prepares, so a promoted follower can
    /// safely finish or roll back the transaction.
    pub async fn apply_raft_prepared_redo(
        &self,
        raft_state_machine_index: u64,
        redo: crate::two_phase::TwoPhaseRedoEntry,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::ApplyRaftPreparedRedo {
            raft_state_machine_index,
            redo,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    /// Apply an idempotent 2PC rollback marker committed through Raft.
    pub async fn apply_raft_rollback(
        &self,
        raft_state_machine_index: u64,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::ApplyRaftRollback {
            raft_state_machine_index,
            transaction_id,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn set_raft_state(
        &self,
        raft_state: crate::raft_partition::RaftPartitionState,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::SetRaftState {
            raft_state,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?;
        Ok(())
    }

    pub async fn finish_table_summary_bootstrap(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::FinishTableSummaryBootstrap { result: tx };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        // The only reason we might fail here if the committer is shutting down.
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn tick_idle_remote_read_frontier_heartbeat_for_test(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::TickIdleRemoteReadFrontierHeartbeat { result: tx };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?;
        Ok(())
    }

    // Tell the committer to load all indexes for the given tables into memory.
    pub async fn load_indexes_into_memory(
        &self,
        tables: BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::LoadIndexesIntoMemory { tables, result: tx };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        // The only reason we might fail here if the committer is shutting down.
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub fn persistence_reader(&self) -> Arc<dyn PersistenceReader> {
        self.persistence_reader.clone()
    }

    pub(crate) fn node_addresses(&self) -> Option<crate::two_phase::NodeAddresses> {
        self.node_addresses.read().clone()
    }

    pub(crate) async fn two_phase_client(
        &self,
        addr: &str,
    ) -> anyhow::Result<Arc<crate::two_phase::TwoPhaseCommitGrpcClient>> {
        self.two_phase_client_pool.client(addr).await
    }

    pub fn placement_version(&self) -> crate::partition::PlacementVersion {
        self.placement_state
            .as_ref()
            .map(|placement_state| placement_state.placement_version())
            .unwrap_or(crate::partition::PlacementVersion::STATIC)
    }

    pub fn refresh_placement_metadata(
        &self,
        metadata: crate::partition::PlacementMetadata,
    ) -> anyhow::Result<()> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            anyhow::bail!("Cannot refresh placement metadata in single-partition mode");
        };
        placement_state.refresh(metadata)
    }

    pub fn refresh_membership_snapshot(
        &self,
        snapshot: crate::membership::MembershipSnapshot,
    ) -> anyhow::Result<()> {
        let node_addresses =
            snapshot.to_live_node_addresses(crate::membership::current_unix_timestamp_millis());
        *self.node_addresses.write() =
            (!node_addresses.partitions().is_empty()).then_some(node_addresses);
        Ok(())
    }

    pub fn ensure_placement_version(
        &self,
        requested: crate::partition::PlacementVersion,
    ) -> anyhow::Result<()> {
        let local = self.placement_version();
        anyhow::ensure!(
            local == requested,
            "Placement version mismatch: request used {requested}, local node is on {local}. \
             Refresh placement metadata before retrying the write."
        );
        Ok(())
    }

    pub(crate) fn two_phase_decision_log(&self) -> Arc<dyn crate::two_phase::TwoPhaseDecisionLog> {
        self.two_phase_decision_log.clone()
    }

    pub(crate) async fn two_phase_redo_records(
        &self,
    ) -> anyhow::Result<BTreeMap<String, crate::two_phase::TwoPhaseRedoEntry>> {
        let Some(value) = self
            .persistence_reader
            .get_persistence_global(PersistenceGlobalKey::TwoPhaseRedoRecords)
            .await?
        else {
            return Ok(BTreeMap::new());
        };
        serde_json::from_value(value).context("2PC: Failed to parse durable redo records")
    }

    async fn wait_for_remote_read_frontiers(
        &self,
        begin_ts: Timestamp,
        reads: &ReadSet,
        write_source: &WriteSource,
    ) -> anyhow::Result<()> {
        let Some(placement_state) = self.placement_state.as_ref() else {
            return Ok(());
        };
        let partition_map = placement_state.partition_map();

        let required = {
            let snapshot_reader = self.snapshot_reader.lock();
            remote_read_partitions(
                reads,
                snapshot_reader.latest_snapshot().table_mapping(),
                &partition_map,
                write_source,
            )
        };
        if required.is_empty() {
            return Ok(());
        }

        let waits: Vec<_> = {
            let snapshot_reader = self.snapshot_reader.lock();
            required
                .iter()
                .filter_map(|partition| {
                    let frontier = snapshot_reader
                        .replication_frontier(*partition)
                        .unwrap_or(Timestamp::MIN);
                    (frontier < begin_ts).then(|| {
                        snapshot_reader.wait_for_replication_frontier(*partition, begin_ts)
                    })
                })
                .collect()
        };

        if waits.is_empty() {
            return Ok(());
        }

        tracing::info!(
            "Waiting for remote read frontiers to reach begin_ts={} on partitions {:?}",
            begin_ts,
            required,
        );
        let started = tokio::time::Instant::now();
        let wait_all = join_all(waits);
        let timeout = tokio::time::sleep(*REMOTE_READ_FRONTIER_WAIT_TIMEOUT);
        futures::pin_mut!(wait_all, timeout);
        select_biased! {
            _ = wait_all.fuse() => {
                metrics::log_remote_read_frontier_wait_seconds(
                    "success",
                    started.elapsed().as_secs_f64(),
                );
                Ok(())
            },
            _ = timeout.fuse() => {
                metrics::log_remote_read_frontier_wait_seconds(
                    "timeout",
                    started.elapsed().as_secs_f64(),
                );
                metrics::log_remote_read_frontier_wait_timeout();
                anyhow::bail!(
                    "Timed out after {:?} waiting for remote read frontiers to reach begin_ts={} \
                     on partitions {:?}",
                    *REMOTE_READ_FRONTIER_WAIT_TIMEOUT,
                    begin_ts,
                    required,
                )
            },
        }
    }

    pub fn commit<RT: Runtime>(
        &self,
        transaction: Transaction<RT>,
        write_source: WriteSource,
    ) -> BoxFuture<'_, anyhow::Result<Timestamp>> {
        async move { Ok(self.commit_outcome(transaction, write_source).await?.ts) }.boxed()
    }

    pub fn commit_outcome<RT: Runtime>(
        &self,
        transaction: Transaction<RT>,
        write_source: WriteSource,
    ) -> BoxFuture<'_, anyhow::Result<CommitOutcome>> {
        self._commit_outcome(transaction, write_source).boxed()
    }

    #[fastrace::trace]
    async fn _commit_outcome<RT: Runtime>(
        &self,
        transaction: Transaction<RT>,
        write_source: WriteSource,
    ) -> anyhow::Result<CommitOutcome> {
        let _timer = metrics::commit_client_timer(transaction.identity());
        self.check_generated_ids(&transaction).await?;

        // Finish reading everything from persistence.
        let transaction = transaction.finalize()?;
        let partition_map = self
            .placement_state
            .as_ref()
            .map(|placement_state| placement_state.partition_map());
        let transaction_classification = partition_map
            .as_ref()
            .map(|partition_map| {
                crate::two_phase_coordinator::classify_transaction(
                    &transaction,
                    partition_map,
                    &write_source,
                )
            })
            .transpose()?;

        // Note that we do a best effort validation for memory index sizes. We
        // use the latest snapshot instead of the transaction base snapshot. This
        // is both more accurate and also avoids pedant hitting transient errors.
        let latest_snapshot = self.snapshot_reader.lock().latest_snapshot();
        transaction.validate_memory_index_sizes(&latest_snapshot)?;

        // Classify: single-partition (fast path) vs cross-partition (2PC).
        // TiDB 1PC optimization: skip 2PC when all writes target one partition.
        if let Some(ref partition_map) = partition_map {
            match transaction_classification
                .expect("partitioned commit should always classify the finalized transaction")
            {
                crate::two_phase_coordinator::TransactionClassification::SinglePartition => {
                    self.wait_for_remote_read_frontiers(
                        *transaction.begin_timestamp,
                        transaction.reads.read_set(),
                        &write_source,
                    )
                    .await?;
                },
                crate::two_phase_coordinator::TransactionClassification::RemoteSinglePartition {
                    owner,
                } => {
                    tracing::info!(
                        "Remote single-partition transaction targets {}; routing from local {} \
                         through the 2PC owner path",
                        owner,
                        partition_map.local_partition(),
                    );
                    return crate::two_phase_coordinator::coordinate_two_phase_commit(
                        self,
                        transaction,
                        write_source,
                        partition_map,
                    )
                    .await;
                },
                crate::two_phase_coordinator::TransactionClassification::CrossPartition {
                    ..
                } => {
                    tracing::info!("Cross-partition transaction detected, using 2PC coordinator");
                    return crate::two_phase_coordinator::coordinate_two_phase_commit(
                        self,
                        transaction,
                        write_source,
                        partition_map,
                    )
                    .await;
                },
            }
        }

        // Single-partition fast path (existing behavior).
        let queue_timer = metrics::commit_queue_timer();
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::Commit {
            queue_timer,
            transaction,
            result: tx,
            write_source,
            parent_trace: EncodedSpan::from_parent(),
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        let Ok(result) = rx.await else {
            anyhow::bail!(metrics::shutdown_error());
        };
        if let Err(e) = result {
            return Err(recapture_stacktrace(e).await);
        }
        result
    }

    pub fn shutdown(&self) {
        self.handle.lock().shutdown();
    }

    /// 2PC Prepare: validate and stage writes for a cross-partition
    /// transaction.
    pub async fn prepare(
        &self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: FinalTransaction,
        write_source: WriteSource,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        self.wait_for_remote_read_frontiers(
            *transaction.begin_timestamp,
            transaction.reads.read_set(),
            &write_source,
        )
        .await?;
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::Prepare {
            transaction_id,
            transaction,
            write_source,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn allocate_commit_ts(&self) -> anyhow::Result<Timestamp> {
        self.allocate_commit_ts_at_or_after(Timestamp::MIN).await
    }

    pub async fn allocate_commit_ts_at_or_after(
        &self,
        min_ts: Timestamp,
    ) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::AllocateCommitTs { min_ts, result: tx };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn close_read_timestamp(
        &self,
        target: Timestamp,
    ) -> anyhow::Result<ReadTimestampClosure> {
        self.close_read_timestamp_with_latest_floor(target, false)
            .await
    }

    pub async fn close_latest_read_timestamp(
        &self,
        target: Timestamp,
    ) -> anyhow::Result<ReadTimestampClosure> {
        self.close_read_timestamp_with_latest_floor(target, true)
            .await
    }

    async fn close_read_timestamp_with_latest_floor(
        &self,
        target: Timestamp,
        include_latest_floor: bool,
    ) -> anyhow::Result<ReadTimestampClosure> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::CloseReadTimestamp {
            target,
            include_latest_floor,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn prepare_remote(
        &self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        prepare_ts: Timestamp,
        participants: Vec<crate::partition::PartitionId>,
    ) -> anyhow::Result<crate::two_phase::PrepareResult> {
        self.wait_for_remote_read_frontiers(
            *transaction.begin_timestamp,
            &transaction.reads,
            &write_source,
        )
        .await?;
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::PrepareRemote {
            transaction_id,
            transaction,
            write_source,
            prepare_ts,
            participants,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn validate_remote_reads(
        &self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        validate_ts: Timestamp,
    ) -> anyhow::Result<()> {
        self.wait_for_remote_read_frontiers(
            *transaction.begin_timestamp,
            &transaction.reads,
            &write_source,
        )
        .await?;
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::ValidateRemoteReads {
            transaction_id,
            transaction,
            write_source,
            validate_ts,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    /// 2PC CommitPrepared: finalize a prepared transaction.
    pub async fn commit_prepared(
        &self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::CommitPrepared {
            transaction_id,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    /// 2PC RollbackPrepared: abort a prepared transaction.
    pub async fn rollback_prepared(
        &self,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::RollbackPrepared {
            transaction_id,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn install_snapshot(&self, checkpoint: CheckpointData) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::InstallSnapshot {
            checkpoint,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        let result = rx.await.map_err(|_| metrics::shutdown_error())?;
        match &result {
            Ok(ts) => {
                metrics::log_snapshot_install_result(true);
                metrics::log_snapshot_installed_ts(*ts);
            },
            Err(_) => {
                metrics::log_snapshot_install_result(false);
            },
        }
        result
    }

    pub async fn install_raft_snapshot(
        &self,
        checkpoint: CheckpointData,
    ) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::InstallRaftSnapshot {
            checkpoint,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    pub async fn build_raft_snapshot(
        &self,
        raft_index: u64,
        raft_term: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::BuildRaftSnapshot {
            raft_index,
            raft_term,
            result: tx,
        };
        self.sender.try_send(message).map_err(|e| match e {
            TrySendError::Full(..) => metrics::committer_full_error().into(),
            TrySendError::Closed(..) => metrics::shutdown_error(),
        })?;
        rx.await.map_err(|_| metrics::shutdown_error())?
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn bump_max_repeatable_ts(&self) -> anyhow::Result<Timestamp> {
        let (tx, rx) = oneshot::channel();
        let message = CommitterMessage::BumpMaxRepeatableTs { result: tx };
        self.sender
            .try_send(message)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(rx.await?)
    }

    async fn check_generated_ids<RT: Runtime>(
        &self,
        transaction: &Transaction<RT>,
    ) -> anyhow::Result<()> {
        // Check that none of the DocumentIds generated in this transaction
        // are already in use.
        // We can check at the begin_timestamp+1 because generated_ids are also
        // checked for conflict against all writes after begin_timestamp.
        let ts = transaction.begin_timestamp().succ()?;
        let timer = metrics::commit_id_reuse_timer();
        let generated_ids = transaction.writes.as_flat()?.generated_ids();
        if !generated_ids.is_empty() {
            let repeatable_persistence = RepeatablePersistence::new(
                self.persistence_reader.clone(),
                transaction.begin_timestamp(),
                self.retention_validator.clone(),
            );
            let generated_ids_with_ts: BTreeSet<_> = generated_ids
                .iter()
                .map(|id| (InternalDocumentId::from(*id), ts))
                .collect();
            let mut previous_revisions_of_ids = repeatable_persistence
                .previous_revisions(generated_ids_with_ts)
                .await?;
            if let Some((
                (document_id, _),
                DocumentLogEntry {
                    value: maybe_doc, ..
                },
            )) = previous_revisions_of_ids.pop_first()
            {
                let display_id = generated_ids
                    .iter()
                    .find(|id| InternalDocumentId::from(**id) == document_id)
                    .map(|id| PublicDocumentId::from(*id).encode())
                    .unwrap_or(document_id.to_string());
                if maybe_doc.is_none() {
                    anyhow::bail!(ErrorMetadata::bad_request(
                        "DocumentDeleted",
                        format!(
                            "Cannot recreate document with _id {display_id} that was deleted. Try \
                             to insert it without an _id or insert into another table."
                        ),
                    ));
                } else {
                    let table_mapping = transaction.metadata.table_mapping();
                    if transaction
                        .metadata
                        .table_mapping()
                        .is_system_tablet(document_id.table())
                        && let Ok(table_name) = table_mapping.tablet_name(document_id.table())
                        && is_retryable_system_generated_id_conflict(&table_name, true)
                    {
                        let metadata = ErrorMetadata::system_occ(None, None);
                        return Err(anyhow::anyhow!(metadata).context(format!(
                            "System metadata row with _id {display_id} in table {table_name} \
                             already exists; retrying against a fresher metadata view"
                        )));
                    }
                    anyhow::bail!(ErrorMetadata::bad_request(
                        "DocumentExists",
                        format!(
                            "Cannot create document with _id {display_id} that already exists. \
                             Try to update it with `db.patch` or `db.replace`."
                        ),
                    ));
                }
            }
        }
        timer.finish();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplicaDeltaApplyMode {
    CrossPartitionReplica,
    FullRaftState,
}

enum CommitterMessage {
    Commit {
        queue_timer: Timer<VMHistogram>,
        transaction: FinalTransaction,
        result: oneshot::Sender<anyhow::Result<CommitOutcome>>,
        write_source: WriteSource,
        parent_trace: EncodedSpan,
    },
    /// Apply a replicated delta from the distributed log.
    /// Used on Replica nodes to apply changes from the Primary's commits.
    /// Goes through the same serial apply loop as local commits.
    ApplyReplicaDelta {
        delta: CommitDelta,
        mode: ReplicaDeltaApplyMode,
        raft_state_machine_index: Option<u64>,
        transport_id: Option<ReplicationTransportId>,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
    },
    ApplyRaftPreparedRedo {
        raft_state_machine_index: u64,
        redo: crate::two_phase::TwoPhaseRedoEntry,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
    ApplyRaftRollback {
        raft_state_machine_index: u64,
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        result: oneshot::Sender<anyhow::Result<()>>,
    },
    SetRaftState {
        raft_state: crate::raft_partition::RaftPartitionState,
        result: oneshot::Sender<()>,
    },
    InstallSnapshot {
        checkpoint: CheckpointData,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
    },
    InstallRaftSnapshot {
        checkpoint: CheckpointData,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
    },
    BuildRaftSnapshot {
        raft_index: u64,
        raft_term: u64,
        result: oneshot::Sender<anyhow::Result<Vec<u8>>>,
    },
    #[cfg(any(test, feature = "testing"))]
    BumpMaxRepeatableTs { result: oneshot::Sender<Timestamp> },
    #[cfg(any(test, feature = "testing"))]
    TickIdleRemoteReadFrontierHeartbeat { result: oneshot::Sender<()> },
    LoadIndexesIntoMemory {
        tables: BTreeSet<TableName>,
        result: oneshot::Sender<anyhow::Result<()>>,
    },
    FinishTextAndVectorBootstrap {
        bootstrapped_indexes: BootstrappedSearchIndexes,
        bootstrap_ts: RepeatableTimestamp,
        result: oneshot::Sender<anyhow::Result<()>>,
    },
    FinishTableSummaryBootstrap {
        result: oneshot::Sender<anyhow::Result<()>>,
    },
    /// 2PC Prepare: validate OCC and stage writes for this partition's portion
    /// of a cross-partition transaction. Does NOT publish — waits for
    /// CommitPrepared or RollbackPrepared. (Vitess prepare pattern)
    Prepare {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: FinalTransaction,
        write_source: WriteSource,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
    PrepareRemote {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        prepare_ts: Timestamp,
        participants: Vec<crate::partition::PartitionId>,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
    ValidateRemoteReads {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        validate_ts: Timestamp,
        result: oneshot::Sender<anyhow::Result<()>>,
    },
    AllocateCommitTs {
        min_ts: Timestamp,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
    },
    CloseReadTimestamp {
        target: Timestamp,
        include_latest_floor: bool,
        result: oneshot::Sender<anyhow::Result<ReadTimestampClosure>>,
    },
    /// 2PC CommitPrepared: finalize a previously prepared transaction.
    /// Writes to persistence, publishes commit, deletes redo log.
    CommitPrepared {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        result: oneshot::Sender<anyhow::Result<Timestamp>>,
    },
    /// 2PC RollbackPrepared: abort a previously prepared transaction.
    /// Removes from PendingWrites, deletes redo log.
    RollbackPrepared {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        result: oneshot::Sender<anyhow::Result<()>>,
    },
}

enum DeferredPrepare {
    ApplyRaftPreparedRedo {
        raft_state_machine_index: u64,
        redo: crate::two_phase::TwoPhaseRedoEntry,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
    Prepare {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: FinalTransaction,
        write_source: WriteSource,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
    PrepareRemote {
        transaction_id: crate::two_phase::TwoPhaseTransactionId,
        transaction: crate::two_phase::ParticipantTransaction,
        write_source: WriteSource,
        prepare_ts: Timestamp,
        participants: Vec<crate::partition::PartitionId>,
        result: oneshot::Sender<anyhow::Result<crate::two_phase::PrepareResult>>,
    },
}

impl DeferredPrepare {
    fn transaction_id(&self) -> crate::two_phase::TwoPhaseTransactionId {
        match self {
            Self::ApplyRaftPreparedRedo { redo, .. } => redo.transaction_id(),
            Self::Prepare { transaction_id, .. } | Self::PrepareRemote { transaction_id, .. } => {
                transaction_id.clone()
            },
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::ApplyRaftPreparedRedo { .. } => "raft_prepared_redo",
            Self::Prepare { .. } => "local_prepare",
            Self::PrepareRemote { .. } => "remote_prepare",
        }
    }
}

fn is_retryable_system_generated_id_conflict(
    table_name: &TableName,
    document_exists: bool,
) -> bool {
    document_exists && &**table_name == "_udf_config"
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{
            BTreeMap,
            BTreeSet,
        },
        str::FromStr,
        sync::{
            atomic::{
                AtomicBool,
                Ordering,
            },
            Arc,
        },
    };

    use async_trait::async_trait;
    use common::{
        bootstrap_model::index::{
            database_index::IndexedFields,
            IndexMetadata,
            TabletIndexMetadata,
        },
        document::{
            CreationTime,
            DocumentUpdate,
            ResolvedDocument,
        },
        persistence::{
            Persistence,
            PersistenceGlobalKey,
        },
        testing::TestPersistence,
        types::{
            GenericIndexName,
            IndexDescriptor,
        },
    };
    use futures::stream::BoxStream;
    use runtime::testing::TestRuntime;
    use sync_types::Timestamp;
    use tokio::sync::oneshot;
    use value::{
        InternalId,
        PublicDocumentId,
        ResolvedDocumentId,
        TableName,
        TableNumber,
        TabletId,
    };

    use super::{
        is_retryable_system_generated_id_conflict,
        remap_index_metadata_update,
        replica_delta_timestamps,
        AppliedDataDeltaIds,
        Committer,
        OrderedDedupeState,
        RaftNatsOutboxRecordStorage,
        MAX_APPLIED_DATA_ORIGIN_TIMESTAMPS_PER_PARTITION,
        RAFT_NATS_OUTBOX_KEY_PREFIX,
    };
    use crate::{
        commit_delta::{
            CommitDelta,
            DistributedLog,
            ReplicationMessage,
            ReplicationTransportId,
        },
        nats_distributed_log::DeltaEnvelope,
        partition::PartitionId,
        raft_node::RaftProposalResult,
        write_log::WriteSource,
    };

    #[derive(Default)]
    struct RecordingDistributedLog {
        fail_publishes: AtomicBool,
        published: parking_lot::Mutex<Vec<Timestamp>>,
    }

    struct FailTimestampDistributedLog {
        fail_ts: Timestamp,
        published: parking_lot::Mutex<Vec<Timestamp>>,
    }

    impl FailTimestampDistributedLog {
        fn new(fail_ts: Timestamp) -> Self {
            Self {
                fail_ts,
                published: parking_lot::Mutex::new(Vec::new()),
            }
        }

        fn published(&self) -> Vec<Timestamp> {
            self.published.lock().clone()
        }
    }

    impl RecordingDistributedLog {
        fn failing() -> Self {
            Self {
                fail_publishes: AtomicBool::new(true),
                published: parking_lot::Mutex::new(Vec::new()),
            }
        }

        fn allow_publishes(&self) {
            self.fail_publishes.store(false, Ordering::SeqCst);
        }

        fn published(&self) -> Vec<Timestamp> {
            self.published.lock().clone()
        }
    }

    #[test]
    fn ordered_dedupe_state_compacts_contiguous_stream_sequences() {
        let mut state = OrderedDedupeState::default();

        state.insert(1);
        state.insert(3);
        assert_eq!(state.compacted_watermark, 1);
        assert_eq!(state.exact_ids, BTreeSet::from([3]));

        state.insert(2);
        assert_eq!(state.compacted_watermark, 3);
        assert!(state.exact_ids.is_empty());
        assert!(state.contains(1));
        assert!(state.contains(3));

        state.insert(5);
        assert_eq!(state.compacted_watermark, 3);
        assert_eq!(state.exact_ids, BTreeSet::from([5]));

        state.insert(4);
        assert_eq!(state.compacted_watermark, 5);
        assert!(state.exact_ids.is_empty());
        assert!(state.contains(5));
    }

    #[test]
    fn applied_data_delta_origin_ids_prune_below_watermark_with_bounded_tail() -> anyhow::Result<()>
    {
        let mut ids = AppliedDataDeltaIds::default();
        let partition = PartitionId(1);
        let count = MAX_APPLIED_DATA_ORIGIN_TIMESTAMPS_PER_PARTITION + 128;
        for ts in 1..=count {
            ids.insert_origin_timestamp(partition, Timestamp::try_from(ts as u64)?);
        }

        let mut watermarks = BTreeMap::new();
        watermarks.insert(partition, Timestamp::try_from(count as u64)?);
        ids.prune_origin_timestamps_below_watermarks(&watermarks);

        let retained = ids
            .origin_timestamps
            .get(&partition)
            .expect("partition should keep bounded origin dedupe tail");
        assert_eq!(
            retained.len(),
            MAX_APPLIED_DATA_ORIGIN_TIMESTAMPS_PER_PARTITION
        );
        assert_eq!(
            retained.first().copied(),
            Some(Timestamp::try_from(129u64)?),
            "oldest origin IDs below the watermark should be pruned first",
        );
        assert_eq!(
            retained.last().copied(),
            Some(Timestamp::try_from(count as u64)?)
        );
        Ok(())
    }

    #[test]
    fn applied_data_delta_transport_ids_stay_bounded_after_sequential_apply() {
        let mut ids = AppliedDataDeltaIds::default();

        for sequence in 1..=10_000 {
            ids.insert_transport_id(ReplicationTransportId::new(sequence));
        }

        assert_eq!(ids.transport_stream_sequences.compacted_watermark, 10_000);
        assert!(
            ids.transport_stream_sequences.exact_ids.is_empty(),
            "sequential transport deliveries should compact fully into the watermark",
        );
    }

    #[async_trait]
    impl DistributedLog for RecordingDistributedLog {
        async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
            if self.fail_publishes.load(Ordering::SeqCst) {
                anyhow::bail!("injected publish failure");
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

    #[async_trait]
    impl DistributedLog for FailTimestampDistributedLog {
        async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
            if delta.ts == self.fail_ts {
                anyhow::bail!("injected publish failure at {}", u64::from(delta.ts));
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

    fn test_outbox_delta(ts: Timestamp) -> CommitDelta {
        CommitDelta {
            ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("test_outbox_delta"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(0)),
        }
    }

    fn test_tablet(byte: u8) -> TabletId {
        TabletId(InternalId([byte; 16]))
    }

    fn test_resolved_id(
        tablet_id: TabletId,
        table_number: u32,
        document_byte: u8,
    ) -> anyhow::Result<ResolvedDocumentId> {
        Ok(ResolvedDocumentId::new(
            tablet_id,
            PublicDocumentId::new(
                TableNumber::try_from(table_number)?,
                InternalId([document_byte; 16]),
            ),
        ))
    }

    fn test_index_metadata_update(
        source_index_tablet: TabletId,
        source_index_table_number: u32,
        source_user_tablet: TabletId,
    ) -> anyhow::Result<DocumentUpdate> {
        let metadata = IndexMetadata::new_enabled(
            GenericIndexName::new(
                source_user_tablet,
                IndexDescriptor::new("by_status".to_string())?,
            )?,
            IndexedFields::by_id(),
        );
        let document = ResolvedDocument::new(
            test_resolved_id(source_index_tablet, source_index_table_number, 99)?,
            CreationTime::ONE,
            metadata.try_into()?,
        )?;
        Ok(DocumentUpdate {
            id: document.id(),
            old_document: None,
            new_document: Some(document),
        })
    }

    fn test_by_id_index_metadata_update(
        source_index_tablet: TabletId,
        source_index_table_number: u32,
        source_user_tablet: TabletId,
    ) -> anyhow::Result<DocumentUpdate> {
        let metadata = IndexMetadata::new_enabled(
            GenericIndexName::by_id(source_user_tablet),
            IndexedFields::by_id(),
        );
        let document = ResolvedDocument::new(
            test_resolved_id(source_index_tablet, source_index_table_number, 98)?,
            CreationTime::ONE,
            metadata.try_into()?,
        )?;
        Ok(DocumentUpdate {
            id: document.id(),
            old_document: None,
            new_document: Some(document),
        })
    }

    #[test]
    fn replicated_index_metadata_remaps_target_tablet_and_downgrades_query_ready_state(
    ) -> anyhow::Result<()> {
        let source_index_tablet = test_tablet(1);
        let local_index_tablet = test_tablet(2);
        let source_user_tablet = test_tablet(3);
        let local_user_tablet = test_tablet(4);
        let update = test_index_metadata_update(source_index_tablet, 2, source_user_tablet)?;
        let tablet_remap = BTreeMap::from([(source_user_tablet, local_user_tablet)]);

        let remapped =
            remap_index_metadata_update(&update, local_index_tablet, &tablet_remap, true)?;

        assert_eq!(remapped.id.tablet_id, local_index_tablet);
        let metadata =
            TabletIndexMetadata::from_document(remapped.new_document.unwrap())?.into_value();
        assert_eq!(*metadata.name.table(), local_user_tablet);
        assert!(metadata.config.is_backfilling());
        assert!(!metadata.config.is_enabled());
        Ok(())
    }

    #[test]
    fn replicated_builtin_index_metadata_remains_query_ready() -> anyhow::Result<()> {
        let source_index_tablet = test_tablet(21);
        let local_index_tablet = test_tablet(22);
        let source_user_tablet = test_tablet(23);
        let local_user_tablet = test_tablet(24);
        let update = test_by_id_index_metadata_update(source_index_tablet, 2, source_user_tablet)?;
        let tablet_remap = BTreeMap::from([(source_user_tablet, local_user_tablet)]);

        let remapped =
            remap_index_metadata_update(&update, local_index_tablet, &tablet_remap, true)?;

        let metadata =
            TabletIndexMetadata::from_document(remapped.new_document.unwrap())?.into_value();
        assert_eq!(*metadata.name.table(), local_user_tablet);
        assert!(metadata.config.is_enabled());
        Ok(())
    }

    #[test]
    fn raft_index_metadata_remap_preserves_query_ready_state() -> anyhow::Result<()> {
        let source_index_tablet = test_tablet(11);
        let local_index_tablet = test_tablet(12);
        let source_user_tablet = test_tablet(13);
        let local_user_tablet = test_tablet(14);
        let update = test_index_metadata_update(source_index_tablet, 2, source_user_tablet)?;
        let tablet_remap = BTreeMap::from([(source_user_tablet, local_user_tablet)]);

        let remapped =
            remap_index_metadata_update(&update, local_index_tablet, &tablet_remap, false)?;

        assert_eq!(remapped.id.tablet_id, local_index_tablet);
        let metadata =
            TabletIndexMetadata::from_document(remapped.new_document.unwrap())?.into_value();
        assert_eq!(*metadata.name.table(), local_user_tablet);
        assert!(metadata.config.is_enabled());
        Ok(())
    }

    #[test]
    fn replica_delta_timestamps_distinguish_origin_from_local_apply_ts() -> anyhow::Result<()> {
        let timestamps = replica_delta_timestamps(
            Timestamp::must(10),
            Timestamp::must(30),
            Timestamp::must(40),
        )?;

        assert_eq!(timestamps.origin_ts, Timestamp::must(10));
        assert_eq!(timestamps.local_apply_ts, Timestamp::must(41));
        Ok(())
    }

    #[tokio::test]
    async fn raft_commit_waiter_accepts_committed_proposal() -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        tx.send(RaftProposalResult::Committed { index: 7 }).unwrap();

        let applied_index =
            Committer::<TestRuntime>::wait_for_raft_commit(Some(rx), Timestamp::must(1), "test")
                .await?;
        assert_eq!(applied_index, Some(7));
        Ok(())
    }

    #[tokio::test]
    async fn raft_commit_waiter_rejects_failed_proposal() {
        let (tx, rx) = oneshot::channel();
        tx.send(RaftProposalResult::Rejected).unwrap();

        let err =
            Committer::<TestRuntime>::wait_for_raft_commit(Some(rx), Timestamp::must(1), "test")
                .await
                .unwrap_err();
        assert!(format!("{err:#}").contains("Raft rejected commit"));
    }

    #[tokio::test]
    async fn raft_commit_waiter_allows_non_raft_commit() -> anyhow::Result<()> {
        let applied_index =
            Committer::<TestRuntime>::wait_for_raft_commit(None, Timestamp::must(1), "test")
                .await?;
        assert_eq!(applied_index, None);
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_replay_publishes_and_clears() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let log = Arc::new(RecordingDistributedLog::default());
        let ts = Timestamp::must(10);
        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts),
        )
        .await?;

        let replayed = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await?;

        assert_eq!(replayed, 1);
        assert_eq!(log.published(), vec![ts]);
        assert!(
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_replay_uses_numeric_timestamp_order() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let log = Arc::new(RecordingDistributedLog::default());
        let ts_2 = Timestamp::must(2);
        let ts_10 = Timestamp::must(10);

        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts_10),
        )
        .await?;
        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts_2),
        )
        .await?;

        let replayed = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await?;

        assert_eq!(replayed, 2);
        assert_eq!(log.published(), vec![ts_2, ts_10]);
        assert!(
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_records_use_per_entry_globals() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let ts_2 = Timestamp::must(2);
        let ts_10 = Timestamp::must(10);

        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts_10),
        )
        .await?;
        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts_2),
        )
        .await?;

        assert!(
            persistence
                .reader()
                .get_persistence_global(PersistenceGlobalKey::RaftNatsOutbox)
                .await?
                .is_none(),
            "new outbox records should not rewrite the legacy aggregate blob",
        );
        let records = Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence).await?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].ts, ts_2);
        assert_eq!(records[1].ts, ts_10);
        assert!(records.iter().all(|record| {
            record.storage == RaftNatsOutboxRecordStorage::PerEntry
                && record.key.starts_with(RAFT_NATS_OUTBOX_KEY_PREFIX)
        }));
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_backlog_stays_per_entry() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let backlog_len = 256;

        for ts in 1..=backlog_len {
            Committer::<TestRuntime>::add_raft_nats_outbox_delta(
                persistence.clone(),
                &test_outbox_delta(Timestamp::must(ts)),
            )
            .await?;
        }

        let raw_records = persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
            .await?;
        assert_eq!(raw_records.len(), backlog_len as usize);
        assert!(
            persistence
                .reader()
                .get_persistence_global(PersistenceGlobalKey::RaftNatsOutbox)
                .await?
                .is_none(),
            "a large backlog should not recreate the legacy aggregate blob",
        );

        let records =
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence.clone()).await?;
        assert_eq!(records.len(), backlog_len as usize);
        assert_eq!(records.first().unwrap().ts, Timestamp::must(1));
        assert_eq!(records.last().unwrap().ts, Timestamp::must(backlog_len));

        let key_to_clear = records[127].key.clone();
        Committer::<TestRuntime>::clear_raft_nats_outbox_record(
            persistence.clone(),
            &records[127],
            "test",
        )
        .await?;

        let raw_records_after_clear = persistence
            .reader()
            .list_persistence_globals_with_prefix(RAFT_NATS_OUTBOX_KEY_PREFIX)
            .await?;
        assert_eq!(raw_records_after_clear.len(), backlog_len as usize - 1);
        assert!(
            raw_records_after_clear
                .iter()
                .all(|(key, _)| key != &key_to_clear),
            "clearing one outbox entry should delete only that per-entry row",
        );
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_replays_and_clears_legacy_blob() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let log = Arc::new(RecordingDistributedLog::default());
        let ts = Timestamp::must(12);
        let envelope = DeltaEnvelope::from_delta(&test_outbox_delta(ts), "", None)?;
        let legacy_records = BTreeMap::from([(
            Committer::<TestRuntime>::raft_nats_outbox_key(ts),
            serde_json::to_value(envelope)?,
        )]);
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::RaftNatsOutbox,
                serde_json::to_value(legacy_records)?,
            )
            .await?;

        let replayed = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await?;

        assert_eq!(replayed, 1);
        assert_eq!(log.published(), vec![ts]);
        assert!(
            persistence
                .reader()
                .get_persistence_global(PersistenceGlobalKey::RaftNatsOutbox)
                .await?
                .is_none(),
            "legacy aggregate blob should be deleted after its final record replays",
        );
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_replay_does_not_bypass_failed_older_delta() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let older_ts = Timestamp::must(2);
        let newer_ts = Timestamp::must(10);
        let log = Arc::new(FailTimestampDistributedLog::new(older_ts));

        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(newer_ts),
        )
        .await?;
        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(older_ts),
        )
        .await?;

        let err = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await
        .unwrap_err();

        assert!(format!("{err:#}").contains("injected publish failure"));
        assert_eq!(
            log.published(),
            Vec::<Timestamp>::new(),
            "newer deltas must not publish while an older outbox entry is unresolved",
        );
        assert_eq!(
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence)
                .await?
                .len(),
            2,
            "both entries should remain for ordered replay",
        );
        Ok(())
    }

    #[tokio::test]
    async fn raft_nats_outbox_replay_keeps_record_on_publish_failure() -> anyhow::Result<()> {
        let persistence: Arc<dyn Persistence> = Arc::new(TestPersistence::new());
        let log = Arc::new(RecordingDistributedLog::failing());
        let ts = Timestamp::must(11);
        Committer::<TestRuntime>::add_raft_nats_outbox_delta(
            persistence.clone(),
            &test_outbox_delta(ts),
        )
        .await?;

        let err = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("injected publish failure"));
        assert_eq!(log.published(), Vec::<Timestamp>::new());
        assert_eq!(
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence.clone())
                .await?
                .len(),
            1
        );

        log.allow_publishes();
        let replayed = Committer::<TestRuntime>::replay_raft_nats_outbox_once(
            persistence.clone(),
            log.clone(),
        )
        .await?;
        assert_eq!(replayed, 1);
        assert_eq!(log.published(), vec![ts]);
        assert!(
            Committer::<TestRuntime>::load_raft_nats_outbox_records(persistence)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn retryable_system_generated_id_conflict_matches_udf_config_table() {
        let table_name = TableName::from_str("_udf_config").unwrap();
        assert!(is_retryable_system_generated_id_conflict(&table_name, true));
    }

    #[test]
    fn retryable_system_generated_id_conflict_rejects_other_tables() {
        let table_name = TableName::from_str("_components").unwrap();
        assert!(!is_retryable_system_generated_id_conflict(
            &table_name,
            true
        ));
        let udf_table = TableName::from_str("_udf_config").unwrap();
        assert!(!is_retryable_system_generated_id_conflict(
            &udf_table, false
        ));
    }
}

// Within a single transaction that writes multiple documents, this is the order
// in which we write them.
//
// Dependencies:
// - _tables table created before other tables.
// - table created before its indexes.
// - indexes created before documents in the table.
// - indexes deleted before other indexes created, in case of naming conflicts.
// - tables deleted before other tables created, in case of naming conflicts.
// - indexes on a table deleted before the table itself.
pub fn table_dependency_sort_key(
    bootstrap_tables: BootstrapTableIds,
    id: InternalDocumentId,
    update: Option<&ResolvedDocument>,
) -> (usize, InternalDocumentId) {
    let table = id.table();
    let sort_key = if table == bootstrap_tables.tables_id.tablet_id {
        match update {
            Some(insertion) => {
                let table_metadata: ParsedDocument<TableMetadata> =
                    insertion.parse().unwrap_or_else(|e| {
                        panic!("Writing invalid TableMetadata {}: {e}", insertion.value().0)
                    });
                match table_metadata.state {
                    TableState::Active => {
                        if &table_metadata.name == &*TABLES_TABLE {
                            // In bootstrapping, create _tables table first.
                            2
                        } else {
                            // Create other tables, especially the _index table, next.
                            3
                        }
                    },
                    TableState::Hidden => 3,
                    // Deleting index must come before table deletion,
                    // so we can delete the table.by_id index while the table still exists.
                    TableState::Deleting => 1,
                }
            },
            // Legacy method of deleting _tables, supported here when walking the log.
            None => 1,
        }
    } else if table == bootstrap_tables.index_id.tablet_id {
        if update.is_none() {
            // Index deletes come first, in case one is being deleted and another
            // created with the same name.
            0
        } else {
            4
        }
    } else {
        5
    };
    (sort_key, id)
}

struct ValidatedCommit {
    index_writes: BTreeSet<(Timestamp, DatabaseIndexUpdate)>,
    document_writes: Vec<ValidatedDocumentWrite>,
    pending_write: PendingWriteHandle,
}
