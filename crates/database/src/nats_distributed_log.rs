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
    TableName,
    TabletId,
};

use crate::{
    commit_delta::{
        CommitDelta,
        DistributedLog,
        ReplicationAck,
        ReplicationMessage,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionGap {
    expected_stream_sequence: u64,
    first_retained_sequence: u64,
    ack_floor_stream_sequence: u64,
    delivered_stream_sequence: u64,
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

    fn selective_node_subjects(node_name: &str) -> Vec<String> {
        vec![
            SelectiveDeliveryRegistry::node_subject(node_name),
            SYSTEM_TABLE_SUBJECT.to_string(),
            FRONTIER_HEARTBEAT_SUBJECT.to_string(),
        ]
    }

    fn is_frontier_heartbeat_delta(delta: &CommitDelta) -> bool {
        delta.source_partition.is_some()
            && delta.document_writes.is_empty()
            && delta.document_updates.is_empty()
            && delta.index_writes.is_empty()
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
            anyhow::bail!(
                "NATS replication stream retention gap for consumer '{}': next required stream \
                 sequence {} is older than first retained sequence {} (ack_floor={}, \
                 delivered={}, from_ts={}). Rebootstrap from checkpoint before serving.",
                consumer_name,
                gap.expected_stream_sequence,
                gap.first_retained_sequence,
                gap.ack_floor_stream_sequence,
                gap.delivered_stream_sequence,
                from_ts_u64,
            );
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
                        let envelope: DeltaEnvelope = match serde_json::from_slice(&msg.payload) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::error!("Failed to deserialize delta from NATS: {e}");
                                if let Err(ack_err) = msg.ack_with(AckKind::Term).await {
                                    tracing::warn!(
                                        "Failed to terminate poison NATS message after \
                                         deserialize error: {ack_err:?}"
                                    );
                                }
                                return Some(Err(anyhow::anyhow!(
                                    "Failed to deserialize delta: {e}"
                                )));
                            },
                        };
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
                        let delta = match envelope.to_delta() {
                            Ok(delta) => delta,
                            Err(e) => {
                                tracing::error!("Failed to decode NATS delta envelope: {e:#}");
                                if let Err(ack_err) = msg.ack_with(AckKind::Term).await {
                                    tracing::warn!(
                                        "Failed to terminate poison NATS message after delta \
                                         decode error: {ack_err:?}"
                                    );
                                }
                                return Some(Err(e));
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
        let filter_subjects = source_partitions.map(|partitions| {
            partitions
                .into_iter()
                .map(Self::partition_subject)
                .collect::<Vec<_>>()
        });
        self.subscribe_subjects(from_ts, filter_subjects).await
    }

    pub async fn subscribe_node_targeted(
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

    pub async fn subscribe_selective_node(
        &self,
        from_ts: Timestamp,
        node_name: &str,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        self.subscribe_subjects(from_ts, Some(Self::selective_node_subjects(node_name)))
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DeltaEnvelope {
    ts: u64,
    write_source: Option<String>,
    write_bytes: u64,
    /// DocumentUpdates encoded as proto bytes.
    document_updates_proto: Vec<Vec<u8>>,
    /// Mapping from Primary's TabletId (16 bytes, hex-encoded) to table name.
    /// Used by Replicas to remap document IDs to their own local TabletIds.
    #[serde(default)]
    tablet_mapping: Vec<(String, String)>, // (hex TabletId, table name string)
    /// Name of the node that published this delta.
    /// Consumers skip deltas from their own node to avoid double-applying.
    #[serde(default)]
    source_node: String,
    /// Raft node ID that originally proposed this delta, when serialized for
    /// intra-partition Raft replication.
    #[serde(default)]
    source_raft_node_id: Option<u64>,
    /// Partition that originated this replication event.
    #[serde(default)]
    source_partition: Option<u32>,
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
            .tablet_id_to_table_name
            .iter()
            .map(|(id, name)| (hex::encode(id.0 .0), name.to_string()))
            .collect();

        Ok(Self {
            ts: u64::from(delta.ts),
            write_source: delta.write_source.as_str().map(|s| s.to_string()),
            write_bytes: delta.write_bytes,
            document_updates_proto,
            tablet_mapping,
            source_node: source_node.to_string(),
            source_raft_node_id,
            source_partition: delta.source_partition.map(|partition| partition.0),
        })
    }

    pub fn to_delta(self) -> anyhow::Result<CommitDelta> {
        let document_updates = self
            .document_updates_proto
            .into_iter()
            .map(|bytes| {
                let proto = pb::common::DocumentUpdate::decode(bytes.as_slice())?;
                DocumentUpdate::try_from(proto)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

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
            tablet_id_to_table_name: self
                .tablet_mapping
                .into_iter()
                .filter_map(|(id_hex, name_str)| {
                    let bytes = hex::decode(&id_hex).ok()?;
                    if bytes.len() != 16 {
                        return None;
                    }
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(&bytes);
                    let tablet_id = TabletId(value::InternalId(arr));
                    let name: TableName = name_str.parse().ok()?;
                    Some((tablet_id, name))
                })
                .collect(),
            source_partition: self.source_partition.map(PartitionId),
        })
    }

    pub fn source_raft_node_id(&self) -> Option<u64> {
        self.source_raft_node_id
    }
}

#[async_trait]
impl DistributedLog for NatsDistributedLog {
    async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
        let ts = u64::from(delta.ts);
        let num_updates = delta.document_updates.len();

        let envelope = DeltaEnvelope::from_delta(&delta, &self.consumer_name, None)?;
        let payload = serde_json::to_vec(&envelope).context("Failed to serialize CommitDelta")?;
        let payload_size = payload.len();
        let publish_timer = metrics::replication_transport_publish_timer();
        metrics::log_replication_transport_message_bytes("publish", payload_size);

        // Publish and wait for acknowledgment from NATS server.
        // The double .await is intentional:
        // - First .await sends the publish request
        // - Second .await waits for the server acknowledgment
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

    use common::types::Timestamp;

    use super::{
        NatsDistributedLog,
        FRONTIER_HEARTBEAT_SUBJECT,
        SYSTEM_TABLE_SUBJECT,
    };
    use crate::{
        commit_delta::CommitDelta,
        partition::PartitionId,
        selective_delivery::SelectiveDeliveryRegistry,
        write_log::WriteSource,
    };

    #[test]
    fn selective_node_subscriptions_include_frontier_heartbeats() {
        assert_eq!(
            NatsDistributedLog::selective_node_subjects("node-b"),
            vec![
                SelectiveDeliveryRegistry::node_subject("node-b"),
                SYSTEM_TABLE_SUBJECT.to_string(),
                FRONTIER_HEARTBEAT_SUBJECT.to_string(),
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
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        };
        assert!(NatsDistributedLog::is_frontier_heartbeat_delta(&heartbeat));

        let mut no_source = heartbeat.clone();
        no_source.source_partition = None;
        assert!(!NatsDistributedLog::is_frontier_heartbeat_delta(&no_source));
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
