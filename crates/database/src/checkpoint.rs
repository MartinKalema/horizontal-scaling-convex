//! Checkpoint-based persistence snapshot for Replica bootstrap.
//!
//! The Primary periodically writes a checkpoint — a full persistence snapshot
//! at a known timestamp — to object storage. Replicas download the checkpoint,
//! deserialize it into a [`CheckpointPersistence`] (an in-memory
//! implementation of [`Persistence`]), then bootstrap their [`SnapshotManager`]
//! using the same `DatabaseSnapshot::load` path the Primary uses against
//! Postgres.
//!
//! After bootstrap, the Replica tails NATS from the checkpoint timestamp for
//! live delta replication. The Replica never connects to the Primary's
//! database.

use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::Arc,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    document::ResolvedDocument,
    persistence::{
        ConflictStrategy,
        DocumentLogEntry,
        DocumentPrevTsQuery,
        DocumentStream,
        IndexStream,
        Persistence,
        PersistenceGlobalKey,
        PersistenceIndexEntry,
        PersistenceReader,
        RetentionValidator,
        TimestampRange,
    },
    query::Order,
    types::{
        IndexId,
        PersistenceVersion,
        Timestamp,
    },
};
use futures::{
    stream,
    StreamExt,
    TryStreamExt,
};
use parking_lot::Mutex;
use serde_json::Value as JsonValue;
use value::{
    InternalDocumentId,
    TabletId,
};

/// Serializable checkpoint data. Contains the full persistence state at a
/// point-in-time timestamp.
#[derive(Clone)]
pub struct CheckpointData {
    pub timestamp: Timestamp,
    pub documents: Vec<DocumentLogEntry>,
    pub globals: BTreeMap<String, JsonValue>,
}

/// Creates a checkpoint from an existing persistence layer.
/// Called periodically on the Primary to produce checkpoint files.
pub async fn create_checkpoint(
    persistence: &dyn PersistenceReader,
    timestamp: Timestamp,
    retention_validator: Arc<dyn RetentionValidator>,
) -> anyhow::Result<CheckpointData> {
    let documents: Vec<DocumentLogEntry> = persistence
        .load_documents(
            TimestampRange::new((Bound::Unbounded, Bound::Included(timestamp))),
            Order::Asc,
            10_000,
            retention_validator,
        )
        .try_collect()
        .await
        .context("Failed to load documents for checkpoint")?;

    let mut globals = BTreeMap::new();
    if let Some(value) = persistence
        .get_persistence_global(PersistenceGlobalKey::MaxRepeatableTimestamp)
        .await?
    {
        globals.insert(
            String::from(PersistenceGlobalKey::MaxRepeatableTimestamp),
            value,
        );
    }
    for key in [
        PersistenceGlobalKey::TablesByIdIndex,
        PersistenceGlobalKey::TablesTabletId,
        PersistenceGlobalKey::IndexByIdIndex,
        PersistenceGlobalKey::IndexTabletId,
    ] {
        if let Some(value) = persistence.get_persistence_global(key).await? {
            globals.insert(String::from(key), value);
        }
    }

    Ok(CheckpointData {
        timestamp,
        documents,
        globals,
    })
}

/// In-memory persistence implementation backed by checkpoint data.
/// Implements `Persistence` and `PersistenceReader` so that
/// `DatabaseSnapshot::load` can bootstrap a Replica from it.
#[derive(Clone)]
pub struct CheckpointPersistence {
    inner: Arc<Mutex<CheckpointInner>>,
    version: PersistenceVersion,
}

struct CheckpointInner {
    log: BTreeMap<(Timestamp, InternalDocumentId), (Option<ResolvedDocument>, Option<Timestamp>)>,
    globals: BTreeMap<String, JsonValue>,
}

impl CheckpointPersistence {
    pub fn from_checkpoint(data: CheckpointData, version: PersistenceVersion) -> Self {
        let mut log = BTreeMap::new();
        for entry in data.documents {
            log.insert((entry.ts, entry.id), (entry.value, entry.prev_ts));
        }
        let inner = CheckpointInner {
            log,
            globals: data.globals,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            version,
        }
    }
}

#[async_trait]
impl Persistence for CheckpointPersistence {
    fn is_fresh(&self) -> bool {
        false
    }

    fn reader(&self) -> Arc<dyn PersistenceReader> {
        Arc::new(self.clone()) as Arc<_>
    }

    async fn write<'a>(
        &self,
        _documents: &'a [DocumentLogEntry],
        _indexes: &'a [PersistenceIndexEntry],
        _conflict_strategy: ConflictStrategy,
    ) -> anyhow::Result<()> {
        anyhow::bail!("CheckpointPersistence is read-only")
    }

    async fn write_persistence_global(
        &self,
        _key: PersistenceGlobalKey,
        _value: JsonValue,
    ) -> anyhow::Result<()> {
        anyhow::bail!("CheckpointPersistence is read-only")
    }

    async fn load_index_chunk(
        &self,
        _cursor: Option<common::index::IndexEntry>,
        _chunk_size: usize,
    ) -> anyhow::Result<Vec<common::index::IndexEntry>> {
        Ok(vec![])
    }

    async fn delete_index_entries(
        &self,
        _entries: Vec<common::index::IndexEntry>,
    ) -> anyhow::Result<usize> {
        anyhow::bail!("CheckpointPersistence is read-only")
    }

    async fn delete(
        &self,
        _documents: Vec<(Timestamp, InternalDocumentId)>,
    ) -> anyhow::Result<usize> {
        anyhow::bail!("CheckpointPersistence is read-only")
    }

    async fn delete_tablet_documents(
        &self,
        _tablet_id: TabletId,
        _chunk_size: usize,
    ) -> anyhow::Result<usize> {
        anyhow::bail!("CheckpointPersistence is read-only")
    }
}

#[async_trait]
impl PersistenceReader for CheckpointPersistence {
    fn load_documents(
        &self,
        range: TimestampRange,
        order: Order,
        _page_size: u32,
        _retention_validator: Arc<dyn RetentionValidator>,
    ) -> DocumentStream<'_> {
        let inner = self.inner.lock();
        let mut entries: Vec<DocumentLogEntry> = inner
            .log
            .iter()
            .filter(|((ts, _), _)| range.contains(*ts))
            .map(|((ts, id), (doc, prev_ts))| DocumentLogEntry {
                ts: *ts,
                id: *id,
                value: doc.clone(),
                prev_ts: *prev_ts,
            })
            .collect();

        if matches!(order, Order::Desc) {
            entries.reverse();
        }

        stream::iter(entries.into_iter().map(Ok)).boxed()
    }

    async fn previous_revisions(
        &self,
        ids: std::collections::BTreeSet<(InternalDocumentId, Timestamp)>,
        _retention_validator: Arc<dyn RetentionValidator>,
    ) -> anyhow::Result<BTreeMap<(InternalDocumentId, Timestamp), DocumentLogEntry>> {
        let inner = self.inner.lock();
        let mut result = BTreeMap::new();
        for (id, ts) in ids {
            let prev = inner
                .log
                .range(..(ts, id))
                .rev()
                .find(|((_, doc_id), _)| *doc_id == id);
            if let Some(((prev_ts, prev_id), (doc, prev_prev_ts))) = prev {
                result.insert(
                    (id, ts),
                    DocumentLogEntry {
                        ts: *prev_ts,
                        id: *prev_id,
                        value: doc.clone(),
                        prev_ts: *prev_prev_ts,
                    },
                );
            }
        }
        Ok(result)
    }

    async fn previous_revisions_of_documents(
        &self,
        ids: std::collections::BTreeSet<DocumentPrevTsQuery>,
        _retention_validator: Arc<dyn RetentionValidator>,
    ) -> anyhow::Result<BTreeMap<DocumentPrevTsQuery, DocumentLogEntry>> {
        let inner = self.inner.lock();
        let mut result = BTreeMap::new();
        for query in ids {
            let key = (query.prev_ts, query.id);
            if let Some((doc, prev_ts)) = inner.log.get(&key) {
                result.insert(
                    query,
                    DocumentLogEntry {
                        ts: key.0,
                        id: key.1,
                        value: doc.clone(),
                        prev_ts: *prev_ts,
                    },
                );
            }
        }
        Ok(result)
    }

    fn index_scan(
        &self,
        _index_id: IndexId,
        _tablet_id: TabletId,
        _read_timestamp: Timestamp,
        _range: &common::interval::Interval,
        _order: Order,
        _size_hint: usize,
        _retention_validator: Arc<dyn RetentionValidator>,
    ) -> IndexStream<'_> {
        // Index scans are not needed during the initial DatabaseSnapshot::load
        // bootstrap. The bootstrap code loads documents directly via
        // load_documents and constructs in-memory indexes from them.
        // If index_scan is called, return empty.
        stream::empty().boxed()
    }

    async fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> anyhow::Result<Option<JsonValue>> {
        let inner = self.inner.lock();
        Ok(inner.globals.get(&String::from(key)).cloned())
    }

    fn version(&self) -> PersistenceVersion {
        self.version
    }
}
