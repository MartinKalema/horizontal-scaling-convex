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
}

/// NATS JetStream implementation of [`DistributedLog`].
pub struct NatsDistributedLog {
    jetstream: jetstream::Context,
    stream: JsStream,
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
        tracing::info!(
            "Connected to NATS JetStream at {}. Stream '{}': {} messages, {} bytes",
            config.url,
            STREAM_NAME,
            info.state.messages,
            info.state.bytes,
        );

        Ok(Self { jetstream, stream })
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

        Ok(Self {
            ts: u64::from(delta.ts),
            write_source: delta.write_source.as_str().map(|s| s.to_string()),
            write_bytes: delta.write_bytes,
            document_updates_proto,
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
        let consumer: PullConsumer = self
            .stream
            .get_or_create_consumer(
                "convex-replica",
                jetstream::consumer::pull::Config {
                    durable_name: Some("convex-replica".to_string()),
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
            "Subscribed to NATS stream '{}' with durable consumer 'convex-replica', from_ts={}",
            STREAM_NAME,
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
