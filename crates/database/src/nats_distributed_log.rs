//! NATS JetStream implementation of [`DistributedLog`].
//!
//! Uses a NATS JetStream stream to publish and subscribe to [`CommitDelta`]s
//! between Primary and Replica nodes.
//!
//! Reference: https://natsbyexample.com/examples/jetstream/limits-stream/rust
//! Reference: https://natsbyexample.com/examples/jetstream/pull-consumer/rust

use std::sync::Arc;

use anyhow::Context;
use async_nats::jetstream::{
    self,
    consumer::PullConsumer,
    stream::Stream as JsStream,
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
    },
    write_log::WriteSource,
};

const STREAM_NAME: &str = "CONVEX_COMMITS";
const SUBJECT: &str = "convex.commits";

/// Configuration for connecting to NATS.
#[derive(Clone, Debug)]
pub struct NatsConfig {
    pub url: String,
    /// Consumer name for this node. Each Replica needs a unique consumer name
    /// so NATS delivers all messages to each Replica independently.
    pub consumer_name: Option<String>,
}

/// NATS JetStream implementation of [`DistributedLog`].
pub struct NatsDistributedLog {
    jetstream: jetstream::Context,
    stream: JsStream,
    consumer_name: String,
}

impl NatsDistributedLog {
    /// Connect to NATS and create/get the JetStream stream.
    pub async fn connect(config: NatsConfig) -> anyhow::Result<Self> {
        // async-nats pulls in rustls which needs a crypto provider.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = async_nats::connect(&config.url)
            .await
            .with_context(|| format!("Failed to connect to NATS at {}", config.url))?;

        let jetstream = jetstream::new(client);

        // Create the stream using create_stream (not get_or_create_stream)
        // to ensure it exists with our exact configuration.
        // If it already exists with matching config, this is a no-op.
        let stream = jetstream
            .get_or_create_stream(jetstream::stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: vec![SUBJECT.to_string()],
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
        let consumer_name = config.consumer_name.unwrap_or_else(|| "convex-replica".to_string());
        tracing::info!(
            "Connected to NATS JetStream at {}. Stream '{}': {} messages, {} bytes. Consumer: {}",
            config.url,
            STREAM_NAME,
            info.state.messages,
            info.state.bytes,
            consumer_name,
        );

        Ok(Self { jetstream, stream, consumer_name })
    }
}

/// Serializable envelope for transporting CommitDelta over NATS.
#[derive(serde::Serialize, serde::Deserialize)]
struct DeltaEnvelope {
    ts: u64,
    write_source: Option<String>,
    write_bytes: u64,
    /// DocumentUpdates encoded as proto bytes.
    document_updates_proto: Vec<Vec<u8>>,
    /// Mapping from Primary's TabletId (16 bytes, hex-encoded) to table name.
    /// Used by Replicas to remap document IDs to their own local TabletIds.
    #[serde(default)]
    tablet_mapping: Vec<(String, String)>, // (hex TabletId, table name string)
}

impl DeltaEnvelope {
    fn from_delta(delta: &CommitDelta) -> anyhow::Result<Self> {
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
            .map(|(id, name)| (hex::encode(id.0.0), name.to_string()))
            .collect();

        Ok(Self {
            ts: u64::from(delta.ts),
            write_source: delta.write_source.as_str().map(|s| s.to_string()),
            write_bytes: delta.write_bytes,
            document_updates_proto,
            tablet_mapping,
        })
    }

    fn to_delta(self) -> anyhow::Result<CommitDelta> {
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
        })
    }
}

#[async_trait]
impl DistributedLog for NatsDistributedLog {
    async fn publish(&self, delta: CommitDelta) -> anyhow::Result<()> {
        let ts = u64::from(delta.ts);
        let num_updates = delta.document_updates.len();

        let envelope = DeltaEnvelope::from_delta(&delta)?;
        let payload =
            serde_json::to_vec(&envelope).context("Failed to serialize CommitDelta")?;
        let payload_size = payload.len();

        // Publish and wait for acknowledgment from NATS server.
        // The double .await is intentional:
        // - First .await sends the publish request
        // - Second .await waits for the server acknowledgment
        let ack = self
            .jetstream
            .publish(SUBJECT, payload.into())
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
        Ok(())
    }

    async fn subscribe(
        &self,
        from_ts: Timestamp,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<CommitDelta>>> {
        // Create a durable consumer so it survives reconnections.
        // DeliverPolicy::All replays all messages from the stream beginning,
        // and we filter out messages at or before from_ts ourselves.
        let consumer_name = self.consumer_name.clone();
        let consumer: PullConsumer = self
            .stream
            .get_or_create_consumer(
                &consumer_name,
                jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    deliver_policy: jetstream::consumer::DeliverPolicy::All,
                    ack_policy: jetstream::consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create NATS durable consumer")?;

        let from_ts_u64 = u64::from(from_ts);
        let messages = consumer
            .messages()
            .await
            .context("Failed to start consuming NATS messages")?;

        tracing::info!(
            "Subscribed to NATS stream '{}' with durable consumer '{}', from_ts={}",
            STREAM_NAME,
            consumer_name,
            from_ts_u64,
        );

        let stream = messages.filter_map(move |msg_result| async move {
            match msg_result {
                Ok(msg) => {
                    if let Err(e) = msg.ack().await {
                        tracing::warn!("Failed to ack NATS message: {e:?}");
                    }
                    let envelope: DeltaEnvelope = match serde_json::from_slice(&msg.payload) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::error!("Failed to deserialize delta from NATS: {e}");
                            return Some(Err(anyhow::anyhow!(
                                "Failed to deserialize delta: {e}"
                            )));
                        },
                    };
                    if envelope.ts <= from_ts_u64 {
                        return None;
                    }
                    tracing::debug!(
                        "Received commit delta from NATS: ts={}",
                        envelope.ts,
                    );
                    Some(envelope.to_delta())
                },
                Err(e) => {
                    tracing::error!("NATS message error: {e}");
                    Some(Err(anyhow::anyhow!("NATS message error: {e}")))
                },
            }
        });

        Ok(Box::pin(stream))
    }
}
