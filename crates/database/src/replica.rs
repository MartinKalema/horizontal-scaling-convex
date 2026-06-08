//! Replica node support for horizontal scaling.
//!
//! The [`ReplicaDeltaConsumer`] subscribes to the distributed log and feeds
//! each [`CommitDelta`] through the Committer's apply loop — the single
//! serialized state machine update path (following the etcd/TiKV/Kafka
//! pattern of one apply thread per node).

use std::sync::Arc;

use anyhow::Context;
use common::runtime::{
    Runtime,
    SpawnHandle,
};
use futures::StreamExt;

use crate::{
    commit_delta::{
        CommitDelta,
        DistributedLog,
        ReplicationMessage,
    },
    committer::CommitterClient,
};

pub async fn apply_replication_message(
    committer: &CommitterClient,
    message: ReplicationMessage,
) -> anyhow::Result<(common::types::Timestamp, usize)> {
    let committer = committer.clone();
    apply_replication_message_with(message, move |delta| async move {
        committer.apply_replica_delta(delta).await
    })
    .await
}

async fn apply_replication_message_with<F, Fut>(
    message: ReplicationMessage,
    apply: F,
) -> anyhow::Result<(common::types::Timestamp, usize)>
where
    F: FnOnce(CommitDelta) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<common::types::Timestamp>>,
{
    let (delta, ack) = message.into_parts();
    let ts = delta.ts;
    let num_updates = delta.document_updates.len();

    match apply(delta).await {
        Ok(applied_ts) => {
            ack.ack().await.with_context(|| {
                format!(
                    "Failed to ack applied replica delta at ts={}",
                    u64::from(applied_ts),
                )
            })?;
            Ok((applied_ts, num_updates))
        },
        Err(e) => {
            tracing::error!(
                "Failed to apply replica delta at ts={}: {e:#}",
                u64::from(ts),
            );
            if let Err(ack_err) = ack.nak().await {
                tracing::warn!(
                    "Failed to NAK unapplied replica delta at ts={}: {ack_err:#}",
                    u64::from(ts),
                );
            }
            anyhow::bail!(
                "Failed to apply replica delta at ts={}: {e:#}",
                u64::from(ts)
            );
        },
    }
}

/// Consumes [`CommitDelta`]s from a [`DistributedLog`] and feeds them
/// through the Committer's apply loop via
/// `CommitterClient::apply_replica_delta`.
///
/// This is the Replica's replication consumer. It runs as a background task
/// after the Replica has finished local initialization.
pub struct ReplicaDeltaConsumer {
    _handle: Box<dyn SpawnHandle>,
}

impl ReplicaDeltaConsumer {
    /// Start the consumer as a background task.
    pub fn start<RT: Runtime>(
        runtime: RT,
        distributed_log: Arc<dyn DistributedLog>,
        committer: CommitterClient,
        from_ts: common::types::Timestamp,
    ) -> Self {
        let handle = runtime.spawn("replica_delta_consumer", async move {
            if let Err(e) = Self::run(distributed_log, committer, from_ts).await {
                tracing::error!("ReplicaDeltaConsumer failed: {e:#}");
            }
        });
        Self { _handle: handle }
    }

    async fn run(
        distributed_log: Arc<dyn DistributedLog>,
        committer: CommitterClient,
        from_ts: common::types::Timestamp,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "ReplicaDeltaConsumer starting — subscribing to distributed log from ts={}",
            u64::from(from_ts)
        );

        let mut stream = distributed_log
            .subscribe(from_ts)
            .await
            .context("Failed to subscribe to distributed log")?;

        tracing::info!("ReplicaDeltaConsumer subscribed, waiting for deltas...");

        while let Some(result) = stream.next().await {
            let message = result.context("Error reading from distributed log")?;
            let (applied_ts, num_updates) = apply_replication_message(&committer, message).await?;
            tracing::info!(
                "Applied replica delta: ts={}, {} updates",
                u64::from(applied_ts),
                num_updates,
            );
        }

        tracing::warn!("ReplicaDeltaConsumer stream ended — no more deltas");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{
                AtomicUsize,
                Ordering,
            },
            Arc,
        },
    };

    use async_trait::async_trait;
    use common::types::Timestamp;

    use super::apply_replication_message_with;
    use crate::{
        commit_delta::{
            CommitDelta,
            ReplicationAck,
            ReplicationMessage,
        },
        write_log::WriteSource,
    };

    #[derive(Default)]
    struct AckCounts {
        ack: AtomicUsize,
        nak: AtomicUsize,
        term: AtomicUsize,
    }

    struct RecordingAck {
        counts: Arc<AckCounts>,
    }

    #[async_trait]
    impl ReplicationAck for RecordingAck {
        async fn ack(self: Box<Self>) -> anyhow::Result<()> {
            self.counts.ack.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn nak(self: Box<Self>) -> anyhow::Result<()> {
            self.counts.nak.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn term(self: Box<Self>) -> anyhow::Result<()> {
            self.counts.term.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_message(ts: Timestamp, counts: Arc<AckCounts>) -> ReplicationMessage {
        ReplicationMessage::new(
            CommitDelta {
                ts,
                document_writes: Arc::new(Vec::new()),
                document_updates: Vec::new(),
                index_writes: Arc::new(Vec::new()),
                write_source: WriteSource::unknown(),
                write_bytes: 0,
                tablet_id_to_table_name: BTreeMap::new(),
                source_partition: None,
            },
            Box::new(RecordingAck { counts }),
        )
    }

    #[tokio::test]
    async fn apply_replication_message_acks_after_success() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;
        let (applied_ts, num_updates) =
            apply_replication_message_with(test_message(ts, counts.clone()), |delta| async move {
                Ok(delta.ts)
            })
            .await?;

        assert_eq!(applied_ts, ts);
        assert_eq!(num_updates, 0);
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn apply_replication_message_naks_after_apply_failure() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;
        let err =
            apply_replication_message_with(test_message(ts, counts.clone()), |_delta| async move {
                anyhow::bail!("forced apply failure")
            })
            .await
            .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains("Failed to apply replica delta"),
            "{message}"
        );
        assert!(message.contains("forced apply failure"), "{message}");
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }
}
