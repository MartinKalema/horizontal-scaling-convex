//! Canonical clustered database genesis.
//!
//! Fresh Convex persistences contain random bootstrap IDs and deployment-global
//! values. A cluster therefore cannot treat independent local initialization as
//! equivalent state. Partition 0 creates one checkpoint candidate, commits it
//! through Raft, and publishes the committed payload for the other partition
//! leaders to commit through their own Raft groups. NATS transports the
//! payload; the per-partition Raft logs remain the installation authority.

use anyhow::Context;
use async_nats::jetstream::object_store::GetErrorKind;
use common::{
    persistence::PersistenceGlobalKey,
    sha256::Sha256,
};
use serde::{
    Deserialize,
    Serialize,
};
use tokio::io::AsyncReadExt;

use crate::{
    checkpoint::CheckpointData,
    partition::PartitionId,
    snapshot_checkpointer::{
        checkpoint_from_bytes,
        checkpoint_to_bytes,
    },
};

const GENESIS_FORMAT_VERSION: u32 = 1;
const GENESIS_KV_BUCKET: &str = "convex_cluster_genesis";
const GENESIS_OBJECT_BUCKET: &str = "convex_cluster_genesis_payloads";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClusterGenesisMarker {
    pub format_version: u32,
    pub genesis_id: String,
}

impl ClusterGenesisMarker {
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Self> {
        let marker: Self =
            serde_json::from_value(value).context("Cluster genesis marker is malformed")?;
        anyhow::ensure!(
            marker.format_version == GENESIS_FORMAT_VERSION,
            "Unsupported cluster genesis format version {}",
            marker.format_version,
        );
        anyhow::ensure!(
            !marker.genesis_id.is_empty(),
            "Cluster genesis ID must not be empty",
        );
        Ok(marker)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClusterGenesisManifest {
    pub format_version: u32,
    pub genesis_id: String,
    pub payload_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalGenesisPayload {
    pub manifest: ClusterGenesisManifest,
    pub checkpoint_bytes: Vec<u8>,
}

pub fn validate_manifest(manifest: &ClusterGenesisManifest) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.format_version == GENESIS_FORMAT_VERSION,
        "Unsupported cluster genesis manifest version {}",
        manifest.format_version,
    );
    anyhow::ensure!(
        !manifest.genesis_id.is_empty(),
        "Cluster genesis manifest ID must not be empty",
    );
    anyhow::ensure!(
        !manifest.payload_sha256.is_empty(),
        "Cluster genesis payload digest must not be empty",
    );
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::hash(bytes))
}

fn node_local_checkpoint_keys() -> impl Iterator<Item = PersistenceGlobalKey> {
    [
        PersistenceGlobalKey::ReplicationFrontiers,
        PersistenceGlobalKey::AppliedDeltaWatermarks,
        PersistenceGlobalKey::AppliedDataDeltaWatermarks,
        PersistenceGlobalKey::AppliedDataDeltaIds,
        PersistenceGlobalKey::ClusterGenesisRaftApplied,
    ]
    .into_iter()
}

pub fn canonicalize_checkpoint(
    mut checkpoint: CheckpointData,
) -> anyhow::Result<CanonicalGenesisPayload> {
    for key in node_local_checkpoint_keys() {
        checkpoint.globals.remove(&String::from(key));
    }
    checkpoint
        .globals
        .remove(&String::from(PersistenceGlobalKey::ClusterGenesis));

    let genesis_id = digest(&checkpoint_to_bytes(&checkpoint)?);
    let marker = ClusterGenesisMarker {
        format_version: GENESIS_FORMAT_VERSION,
        genesis_id: genesis_id.clone(),
    };
    checkpoint.globals.insert(
        String::from(PersistenceGlobalKey::ClusterGenesis),
        serde_json::to_value(marker)?,
    );
    let checkpoint_bytes = checkpoint_to_bytes(&checkpoint)?;
    let manifest = ClusterGenesisManifest {
        format_version: GENESIS_FORMAT_VERSION,
        genesis_id,
        payload_sha256: digest(&checkpoint_bytes),
    };
    Ok(CanonicalGenesisPayload {
        manifest,
        checkpoint_bytes,
    })
}

pub fn validate_payload(
    manifest: &ClusterGenesisManifest,
    checkpoint_bytes: &[u8],
) -> anyhow::Result<CheckpointData> {
    validate_manifest(manifest)?;
    anyhow::ensure!(
        digest(checkpoint_bytes) == manifest.payload_sha256,
        "Cluster genesis payload digest does not match manifest",
    );
    let checkpoint = checkpoint_from_bytes(checkpoint_bytes)
        .context("Failed to decode cluster genesis checkpoint")?;
    let marker_value = checkpoint
        .globals
        .get(&String::from(PersistenceGlobalKey::ClusterGenesis))
        .context("Cluster genesis checkpoint is missing its durable marker")?
        .clone();
    let marker = ClusterGenesisMarker::parse(marker_value)?;
    anyhow::ensure!(
        marker.genesis_id == manifest.genesis_id,
        "Cluster genesis checkpoint ID {} does not match manifest {}",
        marker.genesis_id,
        manifest.genesis_id,
    );
    Ok(checkpoint)
}

fn encode<T: Serialize>(value: &T, context: &str) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(value).with_context(|| format!("Failed to encode {context}"))
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8], context: &str) -> anyhow::Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("Failed to decode {context}"))
}

#[derive(Clone)]
pub struct NatsClusterGenesisStore {
    kv: async_nats::jetstream::kv::Store,
    objects: async_nats::jetstream::object_store::ObjectStore,
    namespace: String,
}

impl NatsClusterGenesisStore {
    pub async fn connect(nats_url: &str, cluster_secret: &[u8]) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = async_nats::connect(nats_url)
            .await
            .with_context(|| format!("Cluster genesis: failed to connect to NATS at {nats_url}"))?;
        let jetstream = async_nats::jetstream::new(client);
        let kv = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: GENESIS_KV_BUCKET.to_string(),
                history: 4,
                ..Default::default()
            })
            .await
            .context("Cluster genesis: failed to create metadata bucket")?;
        let objects = match jetstream.get_object_store(GENESIS_OBJECT_BUCKET).await {
            Ok(objects) => objects,
            Err(_) => match jetstream
                .create_object_store(async_nats::jetstream::object_store::Config {
                    bucket: GENESIS_OBJECT_BUCKET.to_string(),
                    description: Some("Canonical Convex cluster genesis checkpoints".to_string()),
                    ..Default::default()
                })
                .await
            {
                Ok(objects) => objects,
                Err(_) => jetstream
                    .get_object_store(GENESIS_OBJECT_BUCKET)
                    .await
                    .context("Cluster genesis: failed to create or open payload bucket")?,
            },
        };
        Ok(Self {
            kv,
            objects,
            namespace: digest(cluster_secret),
        })
    }

    fn manifest_key(&self) -> &str {
        &self.namespace
    }

    fn payload_key(&self, genesis_id: &str) -> String {
        format!("{}-{genesis_id}", self.namespace)
    }

    fn partition_ready_key(&self, partition: PartitionId) -> String {
        format!("{}.partition.{}", self.namespace, partition.0)
    }

    pub async fn load_manifest(&self) -> anyhow::Result<Option<ClusterGenesisManifest>> {
        let Some(entry) = self
            .kv
            .entry(self.manifest_key())
            .await
            .context("Cluster genesis: failed to read canonical manifest")?
        else {
            return Ok(None);
        };
        Ok(Some(decode(&entry.value, "cluster genesis manifest")?))
    }

    pub async fn put_payload(&self, payload: &CanonicalGenesisPayload) -> anyhow::Result<()> {
        let key = self.payload_key(&payload.manifest.genesis_id);
        if let Some(existing) = self.load_payload(&payload.manifest).await? {
            anyhow::ensure!(
                existing == payload.checkpoint_bytes,
                "Cluster genesis object {key} already contains different bytes",
            );
            return Ok(());
        }
        self.objects
            .put(key.as_str(), &mut payload.checkpoint_bytes.as_slice())
            .await
            .context("Cluster genesis: failed to upload checkpoint payload")?;
        Ok(())
    }

    pub async fn load_payload(
        &self,
        manifest: &ClusterGenesisManifest,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(bytes) = self
            .load_payload_by_genesis_id(&manifest.genesis_id)
            .await?
        else {
            return Ok(None);
        };
        validate_payload(manifest, &bytes)?;
        Ok(Some(bytes))
    }

    pub async fn load_payload_by_genesis_id(
        &self,
        genesis_id: &str,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let key = self.payload_key(genesis_id);
        let mut object = match self.objects.get(&key).await {
            Ok(object) => object,
            Err(error) if error.kind() == GetErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).context("Cluster genesis: failed to read checkpoint payload");
            },
        };
        let mut bytes = Vec::new();
        object
            .read_to_end(&mut bytes)
            .await
            .context("Cluster genesis: failed to download checkpoint payload")?;
        Ok(Some(bytes))
    }

    pub async fn publish_manifest(&self, manifest: &ClusterGenesisManifest) -> anyhow::Result<()> {
        if let Some(existing) = self.load_manifest().await? {
            anyhow::ensure!(
                existing == *manifest,
                "Cluster already has genesis {}, refusing conflicting genesis {}",
                existing.genesis_id,
                manifest.genesis_id,
            );
            return Ok(());
        }
        let bytes = encode(manifest, "cluster genesis manifest")?;
        if self
            .kv
            .create(self.manifest_key(), bytes.into())
            .await
            .is_err()
        {
            let existing = self
                .load_manifest()
                .await?
                .context("Cluster genesis manifest disappeared after create race")?;
            anyhow::ensure!(
                existing == *manifest,
                "Concurrent cluster genesis {} conflicts with {}",
                existing.genesis_id,
                manifest.genesis_id,
            );
        }
        Ok(())
    }

    pub async fn mark_partition_ready(
        &self,
        partition: PartitionId,
        manifest: &ClusterGenesisManifest,
    ) -> anyhow::Result<()> {
        let key = self.partition_ready_key(partition);
        let value = encode(manifest, "partition genesis readiness")?;
        match self.kv.entry(&key).await? {
            Some(entry) => {
                let existing: ClusterGenesisManifest =
                    decode(&entry.value, "partition genesis readiness")?;
                anyhow::ensure!(
                    existing == *manifest,
                    "Partition {} reports conflicting cluster genesis {}",
                    partition,
                    existing.genesis_id,
                );
            },
            None => {
                if self.kv.create(&key, value.into()).await.is_err() {
                    let entry = self
                        .kv
                        .entry(&key)
                        .await?
                        .context("Partition genesis readiness disappeared after create race")?;
                    let existing: ClusterGenesisManifest =
                        decode(&entry.value, "partition genesis readiness")?;
                    anyhow::ensure!(existing == *manifest, "Partition genesis race conflicted");
                }
            },
        }
        Ok(())
    }

    pub async fn all_partitions_ready(
        &self,
        num_partitions: u32,
        manifest: &ClusterGenesisManifest,
    ) -> anyhow::Result<bool> {
        for partition in 0..num_partitions {
            let Some(entry) = self
                .kv
                .entry(self.partition_ready_key(PartitionId(partition)))
                .await?
            else {
                return Ok(false);
            };
            let ready: ClusterGenesisManifest =
                decode(&entry.value, "partition genesis readiness")?;
            anyhow::ensure!(
                ready == *manifest,
                "Partition {partition} is ready on conflicting genesis {}",
                ready.genesis_id,
            );
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use common::types::Timestamp;

    use super::*;

    fn checkpoint() -> CheckpointData {
        CheckpointData {
            timestamp: Timestamp::must(7),
            documents: Vec::new(),
            globals: BTreeMap::from([
                (
                    String::from(PersistenceGlobalKey::TablesTabletId),
                    serde_json::json!("tables"),
                ),
                (
                    String::from(PersistenceGlobalKey::ReplicationFrontiers),
                    serde_json::json!({"0": 9}),
                ),
            ]),
        }
    }

    #[test]
    fn canonical_payload_is_stable_and_strips_node_local_progress() -> anyhow::Result<()> {
        let first = canonicalize_checkpoint(checkpoint())?;
        let second = canonicalize_checkpoint(checkpoint())?;
        assert_eq!(first, second);

        let decoded = validate_payload(&first.manifest, &first.checkpoint_bytes)?;
        assert!(!decoded
            .globals
            .contains_key(&String::from(PersistenceGlobalKey::ReplicationFrontiers)));
        let marker = ClusterGenesisMarker::parse(
            decoded.globals[&String::from(PersistenceGlobalKey::ClusterGenesis)].clone(),
        )?;
        assert_eq!(marker.genesis_id, first.manifest.genesis_id);
        Ok(())
    }

    #[test]
    fn payload_corruption_fails_closed() -> anyhow::Result<()> {
        let mut payload = canonicalize_checkpoint(checkpoint())?;
        payload.checkpoint_bytes.push(0);
        let error = match validate_payload(&payload.manifest, &payload.checkpoint_bytes) {
            Ok(_) => panic!("corrupt payload unexpectedly validated"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("digest does not match"));
        Ok(())
    }
}
