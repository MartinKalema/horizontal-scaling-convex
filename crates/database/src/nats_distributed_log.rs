//! NATS JetStream implementation of [`DistributedLog`].
//!
//! Uses a NATS JetStream stream to publish and subscribe to [`CommitDelta`]s
//! between Primary and Replica nodes.
//!
//! Reference: https://natsbyexample.com/examples/jetstream/limits-stream/rust
//! Reference: https://natsbyexample.com/examples/jetstream/pull-consumer/rust

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_nats::jetstream::{
    self,
    consumer::PullConsumer,
    stream::Stream as JsStream,
    AckKind,
    Message as JetStreamMessage,
};
use async_trait::async_trait;
use common::{
    document::DocumentUpdate,
    types::Timestamp,
};
use futures::{
    stream::BoxStream,
    StreamExt,
};
use prost::Message;
use value::{
    PublicDocumentId,
    TableName,
    TableNamespace,
    TabletId,
};

use crate::{
    commit_delta::{
        CommitDelta,
        DeltaTableIdentity,
        DistributedLog,
        RaftNatsOutboxOrigin,
        ReplicationAck,
        ReplicationMessage,
        ReplicationPoisonGap,
        ReplicationPoisonKind,
        ReplicationTransportId,
    },
    metrics,
    partition::PartitionId,
    selective_delivery::SelectiveDeliveryRegistry,
    write_log::WriteSource,
};

const STREAM_NAME: &str = "CONVEX_COMMITS";
/// Base subject for commit deltas. In single-partition mode, publishes to
/// "convex.commits". In partitioned mode, publishes to
/// "convex.commits.{partition_id}". The stream subscribes to "convex.commits.>"
/// to capture all partitions.
const SUBJECT_BASE: &str = "convex.commits";
const SYSTEM_TABLE_SUBJECT: &str = "convex.commits.system";
const FRONTIER_HEARTBEAT_SUBJECT: &str = "convex.commits.frontier_heartbeat";
const DELTA_ENVELOPE_SCHEMA_VERSION: u32 = 2;
const POISON_REDELIVERY_DELAY: Duration = Duration::from_secs(30);

type EnvelopePoison = (ReplicationPoisonKind, String);

fn deserialize_delta_envelope(payload: &[u8]) -> Result<DeltaEnvelope, EnvelopePoison> {
    serde_json::from_slice(payload).map_err(|error| {
        (
            ReplicationPoisonKind::MalformedEnvelope,
            format!("failed to deserialize delta envelope: {error}"),
        )
    })
}

fn validate_delta_envelope_version(envelope: &DeltaEnvelope) -> Result<(), EnvelopePoison> {
    if envelope.schema_version == DELTA_ENVELOPE_SCHEMA_VERSION {
        return Ok(());
    }
    Err((
        ReplicationPoisonKind::UnsupportedEnvelopeVersion,
        format!(
            "unsupported delta envelope schema version {}; expected {}",
            envelope.schema_version, DELTA_ENVELOPE_SCHEMA_VERSION,
        ),
    ))
}

fn validate_nats_delta_envelope(envelope: &DeltaEnvelope) -> Result<(), EnvelopePoison> {
    envelope.validate_nats_transport().map_err(|error| {
        (
            ReplicationPoisonKind::InvalidDeltaPayload,
            format!("invalid NATS delta origin: {error:#}"),
        )
    })
}

fn decode_delta_envelope(envelope: DeltaEnvelope) -> Result<CommitDelta, EnvelopePoison> {
    validate_nats_delta_envelope(&envelope)?;
    envelope.to_delta().map_err(|error| {
        (
            ReplicationPoisonKind::InvalidDeltaPayload,
            format!("failed to decode delta payload: {error:#}"),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionGap {
    expected_stream_sequence: u64,
    first_retained_sequence: u64,
    ack_floor_stream_sequence: u64,
    delivered_stream_sequence: u64,
}

async fn reject_poison_message(msg: &JetStreamMessage, gap: ReplicationPoisonGap) -> anyhow::Error {
    tracing::error!("{gap}");
    if let Err(ack_err) = msg
        .ack_with(AckKind::Nak(Some(POISON_REDELIVERY_DELAY)))
        .await
    {
        tracing::warn!("Failed to keep poison NATS message pending for redelivery: {ack_err:?}");
    }
    anyhow::Error::new(gap)
}

/// Configuration for connecting to NATS.
#[derive(Clone, Debug)]
pub struct NatsConfig {
    pub url: String,
    /// Consumer name for this node. Each node needs a unique consumer name
    /// so NATS delivers all messages to each node independently.
    pub consumer_name: Option<String>,
    /// Partition ID for this node's publish subject.
    /// None = single-partition mode (publishes to "convex.commits").
    /// Some(id) = partitioned mode (publishes to "convex.commits.{id}").
    pub partition_id: Option<u32>,
}

/// NATS JetStream implementation of [`DistributedLog`].
pub struct NatsDistributedLog {
    jetstream: jetstream::Context,
    stream: JsStream,
    consumer_name: String,
    /// Subject this node publishes to.
    publish_subject: String,
    selective_registry: Option<SelectiveDeliveryRegistry>,
}

impl NatsDistributedLog {
    fn retention_gap(
        stream_messages: u64,
        first_retained_sequence: u64,
        ack_floor_stream_sequence: u64,
        delivered_stream_sequence: u64,
        from_ts: Timestamp,
    ) -> Option<RetentionGap> {
        if stream_messages == 0 || first_retained_sequence == 0 {
            return None;
        }
        let expected_stream_sequence = if ack_floor_stream_sequence > 0 {
            ack_floor_stream_sequence.saturating_add(1)
        } else if delivered_stream_sequence > 0 || u64::from(from_ts) == 0 {
            1
        } else {
            // A checkpointed consumer without durable sequence state cannot
            // prove continuity from JetStream sequence numbers alone.
            return None;
        };
        (expected_stream_sequence < first_retained_sequence).then_some(RetentionGap {
            expected_stream_sequence,
            first_retained_sequence,
            ack_floor_stream_sequence,
            delivered_stream_sequence,
        })
    }

    fn partition_subject(partition: PartitionId) -> String {
        format!("{SUBJECT_BASE}.{}", partition.0)
    }

    fn partition_filter_subjects(partitions: Vec<PartitionId>) -> Vec<String> {
        partitions
            .into_iter()
            .map(Self::partition_subject)
            .collect()
    }

    fn is_frontier_heartbeat_delta(delta: &CommitDelta) -> bool {
        delta.source_partition.is_some()
            && delta.document_writes.is_empty()
            && delta.document_updates.is_empty()
            && delta.index_writes.is_empty()
    }

    async fn publish_envelope(
        &self,
        delta: CommitDelta,
        envelope: DeltaEnvelope,
    ) -> anyhow::Result<()> {
        let ts = u64::from(delta.ts);
        let num_updates = delta.document_updates.len();
        let payload = serde_json::to_vec(&envelope).context("Failed to serialize CommitDelta")?;
        let payload_size = payload.len();
        let publish_timer = metrics::replication_transport_publish_timer();
        metrics::log_replication_transport_message_bytes("publish", payload_size);

        let ack = self
            .jetstream
            .publish(self.publish_subject.clone(), payload.clone().into())
            .await
            .context("Failed to send publish to NATS")?
            .await
            .context("Failed to get publish acknowledgment from NATS")?;

        tracing::info!(
            "Published commit delta to NATS: ts={}, updates={}, bytes={}, stream_seq={}",
            ts,
            num_updates,
            payload_size,
            ack.sequence,
        );
        let touched_tables = delta.touched_table_names();
        if touched_tables.iter().any(TableName::is_system) {
            let ack = self
                .jetstream
                .publish(SYSTEM_TABLE_SUBJECT.to_string(), payload.clone().into())
                .await
                .context("Failed to send selective-delivery system-table publish")?
                .await
                .context("Failed to get selective-delivery system-table acknowledgment")?;
            metrics::log_selective_delivery_targeted_deliveries(1);
            tracing::debug!(
                "Published selective-delivery system-table delta: ts={}, stream_seq={}",
                ts,
                ack.sequence,
            );
        } else if Self::is_frontier_heartbeat_delta(&delta) {
            let ack = self
                .jetstream
                .publish(
                    FRONTIER_HEARTBEAT_SUBJECT.to_string(),
                    payload.clone().into(),
                )
                .await
                .context("Failed to send selective-delivery frontier heartbeat publish")?
                .await
                .context("Failed to get selective-delivery frontier heartbeat acknowledgment")?;
            metrics::log_selective_delivery_targeted_deliveries(1);
            tracing::debug!(
                "Published selective-delivery frontier heartbeat: ts={}, stream_seq={}",
                ts,
                ack.sequence,
            );
        } else if let Some(registry) = &self.selective_registry {
            let interested_nodes = registry
                .interested_nodes_for_tables(&touched_tables)
                .into_iter()
                .filter(|node| node != &self.consumer_name)
                .collect::<Vec<_>>();
            metrics::log_selective_delivery_targeted_deliveries(interested_nodes.len());
            for node in interested_nodes {
                let ack = self
                    .jetstream
                    .publish(
                        SelectiveDeliveryRegistry::node_subject(&node),
                        payload.clone().into(),
                    )
                    .await
                    .with_context(|| {
                        format!("Failed to send selective-delivery publish to {node}")
                    })?
                    .await
                    .with_context(|| {
                        format!("Failed to get selective-delivery ack for target node {node}")
                    })?;
                tracing::debug!(
                    "Published selective-delivery shadow delta to {}: ts={}, stream_seq={}",
                    node,
                    ts,
                    ack.sequence,
                );
            }
        }
        drop(publish_timer);
        Ok(())
    }

    async fn subscribe_subjects(
        &self,
        from_ts: Timestamp,
        filter_subjects: Option<Vec<String>>,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        // Create a durable consumer so it survives reconnections.
        // DeliverPolicy::All replays all messages from the stream beginning,
        // and we filter out messages at or before from_ts ourselves.
        let consumer_name = self.consumer_name.clone();
        let mut consumer: PullConsumer = self
            .stream
            .get_or_create_consumer(
                &consumer_name,
                jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    deliver_policy: jetstream::consumer::DeliverPolicy::All,
                    ack_policy: jetstream::consumer::AckPolicy::Explicit,
                    max_ack_pending: 1,
                    filter_subjects: filter_subjects.clone().unwrap_or_default(),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create NATS durable consumer")?;
        let consumer_info = consumer
            .info()
            .await
            .context("Failed to get NATS consumer info")?
            .clone();
        let from_ts_u64 = u64::from(from_ts);
        let mut stream = self.stream.clone();
        let stream_info = stream
            .info()
            .await
            .context("Failed to get NATS stream info for retention-gap check")?
            .clone();
        if let Some(gap) = Self::retention_gap(
            stream_info.state.messages,
            stream_info.state.first_sequence,
            consumer_info.ack_floor.stream_sequence,
            consumer_info.delivered.stream_sequence,
            from_ts,
        ) {
            metrics::log_replication_transport_retention_gap();
            return Err(ReplicationPoisonGap::new(
                ReplicationPoisonKind::RetentionGap,
                Some(consumer_name.clone()),
                Some(STREAM_NAME.to_string()),
                Some(gap.expected_stream_sequence),
                None,
                None,
                None,
                format!(
                    "next required stream sequence {} is older than first retained sequence {} \
                     (ack_floor={}, delivered={}, from_ts={}). Rebootstrap from checkpoint before \
                     serving",
                    gap.expected_stream_sequence,
                    gap.first_retained_sequence,
                    gap.ack_floor_stream_sequence,
                    gap.delivered_stream_sequence,
                    from_ts_u64,
                ),
            )
            .into());
        }
        metrics::log_replication_transport_pending_messages(consumer_info.num_pending);
        metrics::log_replication_transport_stream_sequence(consumer_info.delivered.stream_sequence);
        metrics::log_replication_transport_consumer_sequence(
            consumer_info.delivered.consumer_sequence,
        );

        let messages = consumer
            .messages()
            .await
            .context("Failed to start consuming NATS messages")?;

        tracing::info!(
            ?filter_subjects,
            "Subscribed to NATS stream '{}' with durable consumer '{}', from_ts={}",
            STREAM_NAME,
            consumer_name,
            from_ts_u64,
        );

        let self_node_name = consumer_name.clone();
        let stream = messages.filter_map(move |msg_result| {
            let node_name = self_node_name.clone();
            async move {
                match msg_result {
                    Ok(msg) => {
                        metrics::log_replication_transport_message_bytes(
                            "consume",
                            msg.payload.len(),
                        );
                        let transport_id = match msg.info() {
                            Ok(info) => {
                                metrics::log_replication_transport_pending_messages(info.pending);
                                metrics::log_replication_transport_stream_sequence(
                                    info.stream_sequence,
                                );
                                metrics::log_replication_transport_consumer_sequence(
                                    info.consumer_sequence,
                                );
                                let now_ns = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_nanos()
                                    as i128;
                                let published_ns = info.published.unix_timestamp_nanos();
                                if now_ns >= published_ns {
                                    metrics::log_replication_transport_lag(
                                        (now_ns - published_ns) as f64 / 1_000_000_000.0,
                                    );
                                }
                                Some(ReplicationTransportId::new(info.stream_sequence))
                            },
                            Err(e) => {
                                tracing::debug!("Failed to parse JetStream message info: {e}");
                                None
                            },
                        };
                        let stream_sequence =
                            transport_id.map(ReplicationTransportId::stream_sequence);
                        let subject = msg.subject.to_string();
                        let envelope = match deserialize_delta_envelope(&msg.payload) {
                            Ok(e) => e,
                            Err((kind, reason)) => {
                                let gap = ReplicationPoisonGap::new(
                                    kind,
                                    Some(node_name),
                                    Some(subject),
                                    stream_sequence,
                                    None,
                                    None,
                                    None,
                                    reason,
                                );
                                return Some(Err(reject_poison_message(&msg, gap).await));
                            },
                        };
                        let source_node = (!envelope.source_node.is_empty())
                            .then(|| envelope.source_node.clone());
                        let source_partition = envelope.source_partition.0.map(PartitionId);
                        let origin_ts = Timestamp::try_from(envelope.ts).ok();
                        if let Err((kind, reason)) = validate_delta_envelope_version(&envelope) {
                            let gap = ReplicationPoisonGap::new(
                                kind,
                                Some(node_name),
                                Some(subject),
                                stream_sequence,
                                source_node,
                                source_partition,
                                origin_ts,
                                reason,
                            );
                            return Some(Err(reject_poison_message(&msg, gap).await));
                        }
                        if let Err((kind, reason)) = validate_nats_delta_envelope(&envelope) {
                            let gap = ReplicationPoisonGap::new(
                                kind,
                                Some(node_name),
                                Some(subject),
                                stream_sequence,
                                source_node,
                                source_partition,
                                origin_ts,
                                reason,
                            );
                            return Some(Err(reject_poison_message(&msg, gap).await));
                        }
                        if envelope.ts <= from_ts_u64 {
                            if let Err(e) = msg.ack().await {
                                return Some(Err(anyhow::anyhow!(
                                    "Failed to ack skipped NATS delta at ts={}: {e:#}",
                                    envelope.ts
                                )));
                            }
                            return None;
                        }
                        // Skip deltas published by this node to avoid double-applying.
                        if !envelope.source_node.is_empty() && envelope.source_node == node_name {
                            tracing::debug!("Skipping self-published delta at ts={}", envelope.ts,);
                            if let Err(e) = msg.ack().await {
                                return Some(Err(anyhow::anyhow!(
                                    "Failed to ack self-published NATS delta at ts={}: {e:#}",
                                    envelope.ts
                                )));
                            }
                            return None;
                        }
                        tracing::debug!(
                            "Received commit delta from NATS: ts={}, source={}",
                            envelope.ts,
                            envelope.source_node,
                        );
                        let delta = match decode_delta_envelope(envelope) {
                            Ok(delta) => delta,
                            Err((kind, reason)) => {
                                let gap = ReplicationPoisonGap::new(
                                    kind,
                                    Some(node_name),
                                    Some(subject),
                                    stream_sequence,
                                    source_node,
                                    source_partition,
                                    origin_ts,
                                    reason,
                                );
                                return Some(Err(reject_poison_message(&msg, gap).await));
                            },
                        };
                        let ack = Box::new(NatsReplicationAck { msg });
                        Some(Ok(match transport_id {
                            Some(transport_id) => {
                                ReplicationMessage::new_with_transport_id(delta, transport_id, ack)
                            },
                            None => ReplicationMessage::new(delta, ack),
                        }))
                    },
                    Err(e) => {
                        tracing::error!("NATS message error: {e}");
                        Some(Err(anyhow::anyhow!("NATS message error: {e}")))
                    },
                }
            }
        });

        Ok(Box::pin(stream))
    }

    async fn subscribe_inner(
        &self,
        from_ts: Timestamp,
        source_partitions: Option<Vec<PartitionId>>,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        let filter_subjects = source_partitions.map(Self::partition_filter_subjects);
        self.subscribe_subjects(from_ts, filter_subjects).await
    }

    /// Observe best-effort selective-delivery traffic without applying it to
    /// the database state machine. Correctness-critical replica apply must use
    /// the broad partition subjects through `subscribe_filtered`.
    pub async fn subscribe_node_targeted_shadow(
        &self,
        from_ts: Timestamp,
        node_name: &str,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        self.subscribe_subjects(
            from_ts,
            Some(vec![SelectiveDeliveryRegistry::node_subject(node_name)]),
        )
        .await
    }

    /// Connect to NATS and create/get the JetStream stream.
    pub async fn connect(config: NatsConfig) -> anyhow::Result<Self> {
        // async-nats pulls in rustls which needs a crypto provider.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = async_nats::connect(&config.url)
            .await
            .with_context(|| format!("Failed to connect to NATS at {}", config.url))?;

        let registry_client = client.clone();
        let jetstream = jetstream::new(client);

        // Create the stream using create_stream (not get_or_create_stream)
        // to ensure it exists with our exact configuration.
        // If it already exists with matching config, this is a no-op.
        // Stream subjects: use wildcard "convex.commits.>" to capture all
        // partitions. Also include bare "convex.commits" for backward compat
        // with single-partition mode.
        let stream = jetstream
            .get_or_create_stream(jetstream::stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: vec![format!("{SUBJECT_BASE}"), format!("{SUBJECT_BASE}.>")],
                retention: jetstream::stream::RetentionPolicy::Limits,
                max_age: std::time::Duration::from_secs(86400),
                storage: jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .context("Failed to create/get NATS JetStream stream")?;

        // Verify the stream is accessible.
        let mut stream = stream;
        let info = stream.info().await.context("Failed to get stream info")?;
        let consumer_name = config
            .consumer_name
            .unwrap_or_else(|| "convex-node".to_string());
        let publish_subject = match config.partition_id {
            Some(id) => format!("{SUBJECT_BASE}.{id}"),
            None => SUBJECT_BASE.to_string(),
        };
        let selective_registry =
            SelectiveDeliveryRegistry::from_client(registry_client, consumer_name.clone())
                .await
                .ok();
        tracing::info!(
            "Connected to NATS JetStream at {}. Stream '{}': {} messages, {} bytes. Consumer: {}, \
             Publish subject: {}",
            config.url,
            STREAM_NAME,
            info.state.messages,
            info.state.bytes,
            consumer_name,
            publish_subject,
        );
        metrics::log_replication_transport_pending_messages(0);
        metrics::log_replication_transport_stream_sequence(0);
        metrics::log_replication_transport_consumer_sequence(0);

        Ok(Self {
            jetstream,
            stream,
            consumer_name,
            publish_subject,
            selective_registry,
        })
    }
}

struct NatsReplicationAck {
    msg: JetStreamMessage,
}

#[async_trait]
impl ReplicationAck for NatsReplicationAck {
    async fn ack(self: Box<Self>) -> anyhow::Result<()> {
        self.msg
            .ack()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to ack NATS replication message: {e}"))
    }

    async fn nak(self: Box<Self>) -> anyhow::Result<()> {
        self.msg
            .ack_with(AckKind::Nak(None))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to NAK NATS replication message: {e}"))
    }

    async fn nak_with_delay(self: Box<Self>, delay: Duration) -> anyhow::Result<()> {
        self.msg
            .ack_with(AckKind::Nak(Some(delay)))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to delayed-NAK NATS replication message: {e}"))
    }

    async fn term(self: Box<Self>) -> anyhow::Result<()> {
        self.msg
            .ack_with(AckKind::Term)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to terminate NATS replication message: {e}"))
    }
}

/// Serializable envelope for transporting CommitDelta over NATS and Raft.
/// Used by both NatsDistributedLog and the Raft state machine to serialize
/// deltas for transport between nodes.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum SerializedDeltaTableNamespace {
    Global,
    ByComponent { component_id: String },
}

impl From<TableNamespace> for SerializedDeltaTableNamespace {
    fn from(namespace: TableNamespace) -> Self {
        match namespace {
            TableNamespace::Global => Self::Global,
            TableNamespace::ByComponent(component_id) => Self::ByComponent {
                component_id: component_id.to_string(),
            },
        }
    }
}

impl TryFrom<SerializedDeltaTableNamespace> for TableNamespace {
    type Error = anyhow::Error;

    fn try_from(namespace: SerializedDeltaTableNamespace) -> Result<Self, Self::Error> {
        Ok(match namespace {
            SerializedDeltaTableNamespace::Global => TableNamespace::Global,
            SerializedDeltaTableNamespace::ByComponent { component_id } => {
                TableNamespace::ByComponent(component_id.parse::<PublicDocumentId>()?)
            },
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SerializedDeltaTabletIdentity {
    tablet_id: String,
    namespace: SerializedDeltaTableNamespace,
    table_name: String,
}

/// A nullable wire field whose presence is mandatory. Serde treats a bare
/// `Option<T>` field as absent when the key is missing, which would make a v2
/// envelope unable to distinguish an explicit `null` from an older schema.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct RequiredNullable<T>(Option<T>);

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> Result<RequiredNullable<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(RequiredNullable)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeltaEnvelope {
    schema_version: u32,
    ts: u64,
    write_source: Option<String>,
    write_bytes: u64,
    /// DocumentUpdates encoded as proto bytes.
    document_updates_proto: Vec<Vec<u8>>,
    /// Stable source identity for every referenced tablet. Namespace is
    /// mandatory so same-named component system tables cannot cross-apply.
    tablet_mapping: Vec<SerializedDeltaTabletIdentity>,
    /// Name of the node that published this delta.
    /// Consumers skip deltas from their own node to avoid double-applying.
    source_node: String,
    /// Raft node ID that originally proposed this delta, when serialized for
    /// intra-partition Raft replication.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    source_raft_node_id: RequiredNullable<u64>,
    /// Partition that originated this replication event.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    source_partition: RequiredNullable<u32>,
}

impl DeltaEnvelope {
    pub fn from_delta(
        delta: &CommitDelta,
        source_node: &str,
        source_raft_node_id: Option<u64>,
    ) -> anyhow::Result<Self> {
        let document_updates_proto = delta
            .document_updates
            .iter()
            .map(|update| {
                let proto: pb::common::DocumentUpdate = update.clone().try_into()?;
                Ok(proto.encode_to_vec())
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let tablet_mapping = delta
            .tablet_id_to_table_identity
            .iter()
            .map(|(id, identity)| SerializedDeltaTabletIdentity {
                tablet_id: hex::encode(id.0 .0),
                namespace: identity.namespace.into(),
                table_name: identity.table_name.to_string(),
            })
            .collect();

        for update in &delta.document_updates {
            anyhow::ensure!(
                delta
                    .tablet_id_to_table_identity
                    .contains_key(&update.id.tablet_id),
                "Delta is missing stable tablet identity for {:?}",
                update.id.tablet_id,
            );
        }
        if !document_updates_proto.is_empty() {
            anyhow::ensure!(
                delta.source_partition.is_some(),
                "data delta must include a source partition",
            );
            anyhow::ensure!(
                !source_node.is_empty() || source_raft_node_id.is_some(),
                "data delta must include a NATS source node or Raft source node ID",
            );
        }

        Ok(Self {
            schema_version: DELTA_ENVELOPE_SCHEMA_VERSION,
            ts: u64::from(delta.ts),
            write_source: delta.write_source.as_str().map(|s| s.to_string()),
            write_bytes: delta.write_bytes,
            document_updates_proto,
            tablet_mapping,
            source_node: source_node.to_string(),
            source_raft_node_id: RequiredNullable(source_raft_node_id),
            source_partition: RequiredNullable(delta.source_partition.map(|partition| partition.0)),
        })
    }

    pub fn from_raft_nats_outbox(
        delta: &CommitDelta,
        origin: &RaftNatsOutboxOrigin,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !origin.source_node.is_empty(),
            "Raft->NATS outbox origin requires a non-empty logical source node",
        );
        anyhow::ensure!(
            delta.source_partition == Some(origin.source_partition),
            "Raft->NATS outbox origin partition {} does not match delta partition {:?}",
            origin.source_partition.0,
            delta.source_partition.map(|partition| partition.0),
        );
        Self::from_delta(delta, &origin.source_node, Some(origin.source_raft_node_id))
    }

    pub fn raft_nats_outbox_origin(&self) -> anyhow::Result<RaftNatsOutboxOrigin> {
        RaftNatsOutboxOrigin::new(
            self.source_node.clone(),
            self.source_raft_node_id
                .0
                .context("Raft->NATS outbox envelope is missing source_raft_node_id")?,
            PartitionId(
                self.source_partition
                    .0
                    .context("Raft->NATS outbox envelope is missing source_partition")?,
            ),
        )
    }

    pub fn raft_nats_outbox_origin_if_present(
        &self,
    ) -> anyhow::Result<Option<RaftNatsOutboxOrigin>> {
        if self.source_node.is_empty() {
            return Ok(None);
        }
        self.raft_nats_outbox_origin().map(Some)
    }

    pub fn to_delta(self) -> anyhow::Result<CommitDelta> {
        anyhow::ensure!(
            self.schema_version == DELTA_ENVELOPE_SCHEMA_VERSION,
            "Unsupported delta envelope schema version {}; expected {}",
            self.schema_version,
            DELTA_ENVELOPE_SCHEMA_VERSION,
        );
        self.validate_data_source_partition()?;
        let document_updates = self
            .document_updates_proto
            .into_iter()
            .map(|bytes| {
                let proto = pb::common::DocumentUpdate::decode(bytes.as_slice())?;
                DocumentUpdate::try_from(proto)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut tablet_id_to_table_identity = std::collections::BTreeMap::new();
        for mapping in self.tablet_mapping {
            let bytes = hex::decode(&mapping.tablet_id)
                .with_context(|| format!("Invalid delta TabletId hex {}", mapping.tablet_id))?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("Delta TabletId must contain exactly 16 bytes"))?;
            let tablet_id = TabletId(value::InternalId(arr));
            let identity = DeltaTableIdentity::new(
                mapping.namespace.try_into()?,
                mapping.table_name.parse::<TableName>()?,
            );
            anyhow::ensure!(
                tablet_id_to_table_identity
                    .insert(tablet_id, identity)
                    .is_none(),
                "Delta contains duplicate tablet identity for {:?}",
                tablet_id,
            );
        }

        for update in &document_updates {
            anyhow::ensure!(
                tablet_id_to_table_identity.contains_key(&update.id.tablet_id),
                "Delta is missing stable tablet identity for {:?}",
                update.id.tablet_id,
            );
        }

        Ok(CommitDelta {
            ts: Timestamp::try_from(self.ts)?,
            document_writes: Arc::new(Vec::new()),
            document_updates,
            index_writes: Arc::new(Vec::new()),
            write_source: match self.write_source {
                Some(s) => WriteSource::new(s),
                None => WriteSource::unknown(),
            },
            write_bytes: self.write_bytes,
            tablet_id_to_table_identity,
            source_partition: self.source_partition.0.map(PartitionId),
        })
    }

    pub fn source_raft_node_id(&self) -> Option<u64> {
        self.source_raft_node_id.0
    }

    fn contains_data(&self) -> bool {
        !self.document_updates_proto.is_empty()
    }

    fn validate_data_source_partition(&self) -> anyhow::Result<()> {
        if self.contains_data() {
            anyhow::ensure!(
                self.source_partition.0.is_some(),
                "data delta must include a non-null source_partition",
            );
        }
        Ok(())
    }

    fn validate_nats_transport(&self) -> anyhow::Result<()> {
        self.validate_data_source_partition()?;
        if self.contains_data() {
            anyhow::ensure!(
                !self.source_node.is_empty(),
                "data delta must include a non-empty source_node for NATS self-filtering",
            );
        }
        Ok(())
    }

    pub(crate) fn validate_raft_transport(&self) -> anyhow::Result<()> {
        self.validate_data_source_partition()?;
        if self.contains_data() {
            anyhow::ensure!(
                self.source_raft_node_id.0.is_some(),
                "data delta must include a non-null source_raft_node_id in a Raft entry",
            );
        }
        Ok(())
    }
}

#[async_trait]
impl DistributedLog for NatsDistributedLog {
    fn source_node(&self) -> Option<String> {
        Some(self.consumer_name.clone())
    }

    async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
        let envelope = DeltaEnvelope::from_delta(&delta, &self.consumer_name, None)?;
        self.publish_envelope(delta, envelope).await
    }

    async fn publish_with_origin(
        &self,
        delta: CommitDelta,
        origin: RaftNatsOutboxOrigin,
    ) -> anyhow::Result<()> {
        let envelope = DeltaEnvelope::from_raft_nats_outbox(&delta, &origin)?;
        self.publish_envelope(delta, envelope).await
    }

    async fn subscribe(
        &self,
        from_ts: Timestamp,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        self.subscribe_inner(from_ts, None).await
    }

    async fn subscribe_filtered(
        &self,
        from_ts: Timestamp,
        source_partitions: Option<Vec<PartitionId>>,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        self.subscribe_inner(from_ts, source_partitions).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Context;
    use common::types::Timestamp;

    use super::{
        decode_delta_envelope,
        deserialize_delta_envelope,
        validate_delta_envelope_version,
        DeltaEnvelope,
        NatsDistributedLog,
        DELTA_ENVELOPE_SCHEMA_VERSION,
    };
    use crate::{
        commit_delta::{
            CommitDelta,
            DeltaTableIdentity,
            ReplicationPoisonKind,
        },
        partition::PartitionId,
        write_log::WriteSource,
    };

    #[test]
    fn replica_apply_filters_use_broad_partition_subjects() {
        assert_eq!(
            NatsDistributedLog::partition_filter_subjects(vec![PartitionId(0), PartitionId(2)]),
            vec![
                "convex.commits.0".to_string(),
                "convex.commits.2".to_string(),
            ],
        );
    }

    #[test]
    fn empty_source_partition_delta_is_frontier_heartbeat() -> anyhow::Result<()> {
        let heartbeat = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("remote_read_frontier_heartbeat_test"),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        assert!(NatsDistributedLog::is_frontier_heartbeat_delta(&heartbeat));

        let mut no_source = heartbeat.clone();
        no_source.source_partition = None;
        assert!(!NatsDistributedLog::is_frontier_heartbeat_delta(&no_source));
        Ok(())
    }

    #[test]
    fn malformed_envelope_is_classified_as_poison() {
        let (kind, reason) = deserialize_delta_envelope(br#"{"schema_version":"broken"}"#)
            .expect_err("invalid JSON types must be rejected");

        assert_eq!(kind, ReplicationPoisonKind::MalformedEnvelope);
        assert!(reason.contains("failed to deserialize delta envelope"));
    }

    #[test]
    fn unknown_envelope_version_is_classified_as_poison() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let mut envelope = DeltaEnvelope::from_delta(&delta, "source", None)?;
        envelope.schema_version = DELTA_ENVELOPE_SCHEMA_VERSION + 1;

        let (kind, reason) = validate_delta_envelope_version(&envelope)
            .expect_err("unknown schema versions must be rejected");
        assert_eq!(kind, ReplicationPoisonKind::UnsupportedEnvelopeVersion);
        assert!(reason.contains("unsupported delta envelope schema version"));
        Ok(())
    }

    #[test]
    fn unversioned_envelope_fails_closed() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let envelope = DeltaEnvelope::from_delta(&delta, "source", None)?;
        let mut value = serde_json::to_value(envelope)?;
        value
            .as_object_mut()
            .context("serialized envelope must be an object")?
            .remove("schema_version");
        let payload = serde_json::to_vec(&value)?;

        let (kind, reason) = deserialize_delta_envelope(&payload)
            .expect_err("unversioned envelopes must fail closed");
        assert_eq!(kind, ReplicationPoisonKind::MalformedEnvelope);
        assert!(reason.contains("schema_version"));
        Ok(())
    }

    #[test]
    fn old_table_name_only_mapping_fails_closed() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let envelope = DeltaEnvelope::from_delta(&delta, "source", None)?;
        let mut value = serde_json::to_value(envelope)?;
        value["tablet_mapping"] =
            serde_json::json!([[hex::encode(value::TabletId::MIN.0 .0), "_modules"]]);

        let payload = serde_json::to_vec(&value)?;
        let (kind, _) = deserialize_delta_envelope(&payload)
            .expect_err("table-name-only mappings must not enter cluster apply");
        assert_eq!(kind, ReplicationPoisonKind::MalformedEnvelope);
        Ok(())
    }

    #[test]
    fn v2_origin_fields_are_mandatory_on_the_wire() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let envelope = serde_json::to_value(DeltaEnvelope::from_delta(&delta, "source", None)?)?;

        for field in ["source_node", "source_raft_node_id", "source_partition"] {
            let mut missing = envelope.clone();
            missing
                .as_object_mut()
                .context("serialized envelope must be an object")?
                .remove(field);
            let payload = serde_json::to_vec(&missing)?;
            let (kind, reason) = deserialize_delta_envelope(&payload)
                .expect_err("every v2 source field must be present");
            assert_eq!(kind, ReplicationPoisonKind::MalformedEnvelope);
            assert!(reason.contains(field), "unexpected error: {reason}");
        }
        Ok(())
    }

    #[test]
    fn explicit_null_origin_fields_are_not_treated_as_missing() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let envelope = DeltaEnvelope::from_delta(&delta, "source", None)?;
        let mut nullable = serde_json::to_value(envelope)?;
        nullable["source_raft_node_id"] = serde_json::Value::Null;
        nullable["source_partition"] = serde_json::Value::Null;

        let decoded: DeltaEnvelope = serde_json::from_value(nullable.clone())?;
        assert_eq!(decoded.source_raft_node_id(), None);
        assert_eq!(decoded.to_delta()?.source_partition, None);

        nullable["source_node"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<DeltaEnvelope>(nullable).is_err(),
            "source_node is present but cannot be null",
        );
        Ok(())
    }

    #[test]
    fn data_delta_requires_non_null_nats_origin_identity() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let mut envelope =
            serde_json::to_value(DeltaEnvelope::from_delta(&delta, "source", None)?)?;
        // The origin checks run before protobuf decoding, so an opaque payload
        // is sufficient to prove malformed origin metadata fails closed.
        envelope["document_updates_proto"] = serde_json::json!([[]]);
        envelope["source_partition"] = serde_json::Value::Null;
        let decoded: DeltaEnvelope = serde_json::from_value(envelope.clone())?;
        let (kind, reason) = decode_delta_envelope(decoded)
            .expect_err("a data delta with null source_partition must fail closed");
        assert_eq!(kind, ReplicationPoisonKind::InvalidDeltaPayload);
        assert!(reason.contains("non-null source_partition"));

        envelope["source_partition"] = serde_json::json!(1);
        envelope["source_node"] = serde_json::json!("");
        let decoded: DeltaEnvelope = serde_json::from_value(envelope)?;
        let (kind, reason) = decode_delta_envelope(decoded)
            .expect_err("a NATS data delta with an empty source_node must fail closed");
        assert_eq!(kind, ReplicationPoisonKind::InvalidDeltaPayload);
        assert!(reason.contains("non-empty source_node"));
        Ok(())
    }

    #[test]
    fn envelope_roundtrip_preserves_component_namespace_identity() -> anyhow::Result<()> {
        let child_namespace = value::TableNamespace::ByComponent(value::PublicDocumentId::new(
            value::TableNumber::try_from(10_001u32)?,
            value::InternalId([0x44; 16]),
        ));
        let table_name: value::TableName = "_modules".parse()?;
        let mut tablet_id_to_table_identity = std::collections::BTreeMap::new();
        tablet_id_to_table_identity.insert(
            value::TabletId::MIN,
            DeltaTableIdentity::new(value::TableNamespace::Global, table_name.clone()),
        );
        tablet_id_to_table_identity.insert(
            value::TabletId::MAX,
            DeltaTableIdentity::new(child_namespace, table_name),
        );
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: tablet_id_to_table_identity.clone(),
            source_partition: Some(PartitionId(1)),
        };

        let decoded = DeltaEnvelope::from_delta(&delta, "source", None)?.to_delta()?;
        assert_eq!(
            decoded.tablet_id_to_table_identity,
            tablet_id_to_table_identity,
        );
        Ok(())
    }

    #[test]
    fn invalid_document_proto_is_classified_as_poison() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::try_from(42u64)?,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::unknown(),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        let mut envelope = DeltaEnvelope::from_delta(&delta, "source", None)?;
        envelope.document_updates_proto.push(vec![0x80]);

        let (kind, reason) =
            decode_delta_envelope(envelope).expect_err("invalid protobuf must be rejected");
        assert_eq!(kind, ReplicationPoisonKind::InvalidDeltaPayload);
        assert!(reason.contains("failed to decode delta payload"));
        Ok(())
    }

    #[test]
    fn retention_gap_detects_ack_floor_behind_first_retained() -> anyhow::Result<()> {
        assert_eq!(
            NatsDistributedLog::retention_gap(10, 100, 98, 98, Timestamp::try_from(0u64)?,),
            Some(super::RetentionGap {
                expected_stream_sequence: 99,
                first_retained_sequence: 100,
                ack_floor_stream_sequence: 98,
                delivered_stream_sequence: 98,
            }),
        );
        Ok(())
    }

    #[test]
    fn retention_gap_detects_fresh_from_beginning_after_retention() -> anyhow::Result<()> {
        assert_eq!(
            NatsDistributedLog::retention_gap(10, 2, 0, 0, Timestamp::try_from(0u64)?),
            Some(super::RetentionGap {
                expected_stream_sequence: 1,
                first_retained_sequence: 2,
                ack_floor_stream_sequence: 0,
                delivered_stream_sequence: 0,
            }),
        );
        Ok(())
    }

    #[test]
    fn retention_gap_allows_contiguous_resume() -> anyhow::Result<()> {
        assert_eq!(
            NatsDistributedLog::retention_gap(10, 99, 98, 98, Timestamp::try_from(0u64)?,),
            None,
        );
        Ok(())
    }

    #[test]
    fn retention_gap_allows_empty_stream() -> anyhow::Result<()> {
        assert_eq!(
            NatsDistributedLog::retention_gap(0, 0, 0, 0, Timestamp::try_from(0u64)?),
            None,
        );
        Ok(())
    }

    #[test]
    fn retention_gap_allows_checkpointed_consumer_without_sequence() -> anyhow::Result<()> {
        assert_eq!(
            NatsDistributedLog::retention_gap(10, 100, 0, 0, Timestamp::try_from(42u64)?),
            None,
        );
        Ok(())
    }
}
