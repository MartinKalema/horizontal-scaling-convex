//! Periodic checkpoint writer for Primary nodes.
//!
//! The [`SnapshotCheckpointer`] runs as a background task on the Primary,
//! periodically dumping the persistence state to object storage via the
//! [`Storage`] trait. Replicas download these checkpoints to bootstrap
//! without connecting to the Primary's database.
//!
//! Checkpoints are stored as prost-encoded blobs under a well-known key
//! pattern: `checkpoint-{timestamp}`. The latest checkpoint key is stored
//! separately as `checkpoint-latest` for fast lookup.

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use common::{
    persistence::PersistenceReader,
    runtime::{
        Runtime,
        SpawnHandle,
    },
    types::Timestamp,
};
use prost::Message;
use storage::{
    Storage,
    Upload,
};
use value::{
    InternalDocumentId,
    TabletId,
};

use crate::checkpoint::{
    create_checkpoint,
    CheckpointData,
};

/// How often the Primary writes a new checkpoint.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);

/// Well-known key for the latest checkpoint pointer.
const LATEST_KEY: &str = "checkpoint-latest";

/// Background task that periodically writes checkpoints to object storage.
pub struct SnapshotCheckpointer {
    _handle: Box<dyn SpawnHandle>,
}

impl SnapshotCheckpointer {
    /// Start the checkpointer as a background task.
    pub fn start<RT: Runtime>(
        runtime: RT,
        persistence_reader: Arc<dyn PersistenceReader>,
        retention_validator: Arc<dyn common::persistence::RetentionValidator>,
        checkpoint_storage: Arc<dyn Storage>,
    ) -> Self {
        let rt = runtime.clone();
        let handle = runtime.spawn("snapshot_checkpointer", async move {
            if let Err(e) = Self::run(
                rt,
                persistence_reader,
                retention_validator,
                checkpoint_storage,
            )
            .await
            {
                tracing::error!("SnapshotCheckpointer failed: {e:?}");
            }
        });
        Self { _handle: handle }
    }

    async fn run<RT: Runtime>(
        runtime: RT,
        persistence_reader: Arc<dyn PersistenceReader>,
        retention_validator: Arc<dyn common::persistence::RetentionValidator>,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<()> {
        loop {
            match Self::write_checkpoint(&persistence_reader, retention_validator.clone(), &storage)
                .await
            {
                Ok(ts) => {
                    tracing::info!("Wrote checkpoint at ts={ts}");
                },
                Err(e) => {
                    tracing::error!("Failed to write checkpoint: {e:?}");
                },
            }
            runtime.wait(CHECKPOINT_INTERVAL).await;
        }
    }

    async fn write_checkpoint(
        persistence_reader: &Arc<dyn PersistenceReader>,
        retention_validator: Arc<dyn common::persistence::RetentionValidator>,
        storage: &Arc<dyn Storage>,
    ) -> anyhow::Result<Timestamp> {
        // Get the current max timestamp from persistence.
        let max_ts = persistence_reader
            .max_ts()
            .await?
            .context("No data in persistence")?;

        // Create the checkpoint.
        let checkpoint =
            create_checkpoint(persistence_reader.as_ref(), max_ts, retention_validator).await?;

        let ts = checkpoint.timestamp;
        let num_docs = checkpoint.documents.len();

        // Serialize to proto bytes.
        let proto = checkpoint_to_proto(&checkpoint)?;
        let bytes = proto.encode_to_vec();

        tracing::info!(
            "Checkpoint at ts={ts}: {num_docs} documents, {} bytes",
            bytes.len()
        );

        // Upload to storage.
        let checkpoint_key = format!("checkpoint-{}", u64::from(ts));
        let mut upload = storage.start_upload().await?;
        upload.write(Bytes::from(bytes)).await?;
        let _key = upload.complete().await?;

        // Write the latest pointer.
        let mut latest_upload = storage.start_upload().await?;
        latest_upload
            .write(Bytes::from(checkpoint_key.into_bytes()))
            .await?;
        latest_upload.complete().await?;

        Ok(ts)
    }
}

/// Load the latest checkpoint from storage.
/// Called by Replicas at startup to bootstrap their SnapshotManager.
pub async fn load_latest_checkpoint(
    _storage: &dyn Storage,
    persistence_reader: &dyn PersistenceReader,
    retention_validator: Arc<dyn common::persistence::RetentionValidator>,
) -> anyhow::Result<Option<CheckpointData>> {
    // For now, create a checkpoint directly from the persistence reader.
    // In production, this would download from object storage.
    // The persistence_reader here would point to the Primary's database
    // (read-only) or the checkpoint would be fetched from S3/R2.
    let max_ts = match persistence_reader.max_ts().await? {
        Some(ts) => ts,
        None => return Ok(None),
    };

    let checkpoint = create_checkpoint(persistence_reader, max_ts, retention_validator).await?;
    Ok(Some(checkpoint))
}

/// Proto encoding for checkpoint data.
mod checkpoint_proto {
    #[derive(Clone, prost::Message)]
    pub struct CheckpointData {
        #[prost(uint64, tag = "1")]
        pub timestamp: u64,
        #[prost(message, repeated, tag = "2")]
        pub documents: Vec<DocumentEntry>,
        #[prost(message, repeated, tag = "3")]
        pub globals: Vec<GlobalEntry>,
    }

    #[derive(Clone, prost::Message)]
    pub struct DocumentEntry {
        #[prost(uint64, tag = "1")]
        pub ts: u64,
        #[prost(bytes, tag = "2")]
        pub id_bytes: Vec<u8>,
        #[prost(bytes, tag = "3")]
        pub document_bytes: Vec<u8>,
        #[prost(bool, tag = "4")]
        pub has_document: bool,
        #[prost(uint64, tag = "5")]
        pub prev_ts: u64,
        #[prost(bool, tag = "6")]
        pub has_prev_ts: bool,
    }

    #[derive(Clone, prost::Message)]
    pub struct GlobalEntry {
        #[prost(string, tag = "1")]
        pub key: String,
        #[prost(string, tag = "2")]
        pub value_json: String,
    }
}

fn checkpoint_to_proto(data: &CheckpointData) -> anyhow::Result<checkpoint_proto::CheckpointData> {
    let documents = data
        .documents
        .iter()
        .map(|entry| {
            let doc_bytes = entry
                .value
                .as_ref()
                .map(|doc| {
                    let proto: pb::common::ResolvedDocument = doc.clone().try_into()?;
                    Ok::<_, anyhow::Error>(proto.encode_to_vec())
                })
                .transpose()?
                .unwrap_or_default();

            // Serialize InternalDocumentId as its raw bytes.
            let id_bytes = {
                let tablet_bytes = entry.id.table().0 .0;
                let internal_bytes = entry.id.internal_id().0;
                let mut buf = Vec::with_capacity(32);
                buf.extend_from_slice(&tablet_bytes);
                buf.extend_from_slice(&internal_bytes);
                buf
            };

            Ok(checkpoint_proto::DocumentEntry {
                ts: u64::from(entry.ts),
                id_bytes,
                document_bytes: doc_bytes,
                has_document: entry.value.is_some(),
                prev_ts: entry.prev_ts.map(u64::from).unwrap_or(0),
                has_prev_ts: entry.prev_ts.is_some(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let globals = data
        .globals
        .iter()
        .map(|(k, v)| checkpoint_proto::GlobalEntry {
            key: k.clone(),
            value_json: serde_json::to_string(v).unwrap_or_default(),
        })
        .collect();

    Ok(checkpoint_proto::CheckpointData {
        timestamp: u64::from(data.timestamp),
        documents,
        globals,
    })
}

pub fn checkpoint_from_proto(
    proto: checkpoint_proto::CheckpointData,
) -> anyhow::Result<CheckpointData> {
    let timestamp = Timestamp::try_from(proto.timestamp)?;

    let documents = proto
        .documents
        .into_iter()
        .map(|entry| {
            let ts = Timestamp::try_from(entry.ts)?;

            // Deserialize InternalDocumentId from raw bytes.
            anyhow::ensure!(
                entry.id_bytes.len() == 32,
                "Expected 32 bytes for InternalDocumentId, got {}",
                entry.id_bytes.len()
            );
            let mut tablet_bytes = [0u8; 16];
            let mut internal_bytes = [0u8; 16];
            tablet_bytes.copy_from_slice(&entry.id_bytes[..16]);
            internal_bytes.copy_from_slice(&entry.id_bytes[16..]);
            let tablet_id = TabletId(value::InternalId(tablet_bytes));
            let internal_id = value::InternalId(internal_bytes);
            let id = InternalDocumentId::new(tablet_id, internal_id);

            let value = if entry.has_document {
                let doc_proto =
                    pb::common::ResolvedDocument::decode(entry.document_bytes.as_slice())?;
                Some(common::document::ResolvedDocument::try_from(doc_proto)?)
            } else {
                None
            };

            let prev_ts = if entry.has_prev_ts {
                Some(Timestamp::try_from(entry.prev_ts)?)
            } else {
                None
            };

            Ok(common::persistence::DocumentLogEntry {
                ts,
                id,
                value,
                prev_ts,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let globals = proto
        .globals
        .into_iter()
        .map(|entry| {
            let value: serde_json::Value = serde_json::from_str(&entry.value_json)?;
            Ok((entry.key, value))
        })
        .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()?;

    Ok(CheckpointData {
        timestamp,
        documents,
        globals,
    })
}
