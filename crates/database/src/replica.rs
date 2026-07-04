//! Replica node support for horizontal scaling.
//!
//! The [`ReplicaDeltaConsumer`] subscribes to the distributed log and feeds
//! each [`CommitDelta`] through the Committer's apply loop — the single
//! serialized state machine update path (following the etcd/TiKV/Kafka
//! pattern of one apply thread per node).

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use common::runtime::{
    Runtime,
    SpawnHandle,
};
use futures::{
    stream::BoxStream,
    StreamExt,
};

use crate::{
    commit_delta::{
        CommitDelta,
        DistributedLog,
        ReplicationMessage,
        ReplicationTransportId,
        RetryableReplicaApplyError,
    },
    committer::CommitterClient,
};

const RETRYABLE_REPLICA_APPLY_NAK_DELAY: Duration = Duration::from_millis(250);

pub async fn apply_replication_message(
    committer: &CommitterClient,
    message: ReplicationMessage,
) -> anyhow::Result<(common::types::Timestamp, usize)> {
    let committer = committer.clone();
    apply_replication_message_with(message, move |delta, transport_id| {
        let committer = committer.clone();
        async move {
            match transport_id {
                Some(transport_id) => {
                    committer
                        .apply_replica_delta_with_transport_id(delta, transport_id)
                        .await
                },
                None => committer.apply_replica_delta(delta).await,
            }
        }
    })
    .await
}

pub async fn consume_replication_stream<F>(
    stream: BoxStream<'static, anyhow::Result<ReplicationMessage>>,
    committer: CommitterClient,
    after_success: F,
) -> anyhow::Result<()>
where
    F: FnMut(common::types::Timestamp, usize),
{
    consume_replication_stream_with(
        stream,
        move |message| {
            let committer = committer.clone();
            async move { apply_replication_message(&committer, message).await }
        },
        after_success,
    )
    .await
}

async fn consume_replication_stream_with<F, Fut, S>(
    mut stream: BoxStream<'static, anyhow::Result<ReplicationMessage>>,
    mut apply_message: F,
    mut after_success: S,
) -> anyhow::Result<()>
where
    F: FnMut(ReplicationMessage) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<(common::types::Timestamp, usize)>>,
    S: FnMut(common::types::Timestamp, usize),
{
    while let Some(result) = stream.next().await {
        let message = result.context("Error reading from distributed log")?;
        match apply_message(message).await {
            Ok((applied_ts, num_updates)) => {
                after_success(applied_ts, num_updates);
            },
            Err(e) => {
                tracing::error!("Failed to apply replica delta; continuing consumer: {e:#}");
            },
        }
    }

    anyhow::bail!("ReplicaDeltaConsumer stream ended")
}

async fn apply_replication_message_with<F, Fut>(
    message: ReplicationMessage,
    mut apply: F,
) -> anyhow::Result<(common::types::Timestamp, usize)>
where
    F: FnMut(CommitDelta, Option<ReplicationTransportId>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<common::types::Timestamp>>,
{
    let (delta, transport_id, ack) = message.into_parts();
    let ts = delta.ts;
    let num_updates = delta.document_updates.len();

    match apply(delta, transport_id).await {
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
            if e.downcast_ref::<RetryableReplicaApplyError>().is_some() {
                tracing::info!(
                    "Replica delta at ts={} is temporarily blocked; delayed-NAKing for {:?}: {e:#}",
                    u64::from(ts),
                    RETRYABLE_REPLICA_APPLY_NAK_DELAY,
                );
                if let Err(ack_err) = ack.nak_with_delay(RETRYABLE_REPLICA_APPLY_NAK_DELAY).await {
                    tracing::warn!(
                        "Failed to delayed-NAK blocked replica delta at ts={}: {ack_err:#}",
                        u64::from(ts),
                    );
                }
                anyhow::bail!(
                    "Retryable failure applying replica delta at ts={}: {e:#}",
                    u64::from(ts),
                );
            }
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
        let runtime_for_task = runtime.clone();
        let handle = runtime.spawn("replica_delta_consumer", async move {
            Self::run_supervised(runtime_for_task, distributed_log, committer, from_ts).await;
        });
        Self { _handle: handle }
    }

    async fn run_supervised<RT: Runtime>(
        runtime: RT,
        distributed_log: Arc<dyn DistributedLog>,
        committer: CommitterClient,
        from_ts: common::types::Timestamp,
    ) {
        let mut backoff = Duration::from_millis(250);
        loop {
            match Self::run_once(distributed_log.clone(), committer.clone(), from_ts).await {
                Ok(()) => {
                    tracing::warn!("ReplicaDeltaConsumer exited cleanly; restarting");
                },
                Err(e) => {
                    tracing::error!(
                        "ReplicaDeltaConsumer failed; restarting after {:?}: {e:#}",
                        backoff,
                    );
                },
            }
            runtime.wait(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn run_once(
        distributed_log: Arc<dyn DistributedLog>,
        committer: CommitterClient,
        from_ts: common::types::Timestamp,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "ReplicaDeltaConsumer starting — subscribing to distributed log from ts={}",
            u64::from(from_ts)
        );

        let stream = distributed_log
            .subscribe(from_ts)
            .await
            .context("Failed to subscribe to distributed log")?;

        tracing::info!("ReplicaDeltaConsumer subscribed, waiting for deltas...");

        consume_replication_stream(stream, committer, |applied_ts, num_updates| {
            tracing::info!(
                "Applied replica delta: ts={}, {} updates",
                u64::from(applied_ts),
                num_updates,
            );
        })
        .await
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
        time::Duration,
    };

    use async_trait::async_trait;
    use common::types::Timestamp;

    use super::{
        apply_replication_message_with,
        consume_replication_stream_with,
        RETRYABLE_REPLICA_APPLY_NAK_DELAY,
    };
    use crate::{
        commit_delta::{
            CommitDelta,
            ReplicationAck,
            ReplicationMessage,
            RetryableReplicaApplyError,
        },
        write_log::WriteSource,
    };

    #[derive(Default)]
    struct AckCounts {
        ack: AtomicUsize,
        nak: AtomicUsize,
        delayed_nak: AtomicUsize,
        term: AtomicUsize,
        delayed_nak_delays: parking_lot::Mutex<Vec<Duration>>,
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

        async fn nak_with_delay(self: Box<Self>, delay: Duration) -> anyhow::Result<()> {
            self.counts.delayed_nak.fetch_add(1, Ordering::SeqCst);
            self.counts.delayed_nak_delays.lock().push(delay);
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
        let (applied_ts, num_updates) = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |delta, _transport_id| async move { Ok(delta.ts) },
        )
        .await?;

        assert_eq!(applied_ts, ts);
        assert_eq!(num_updates, 0);
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn apply_replication_message_naks_after_apply_failure() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;
        let err = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |_delta, _transport_id| async move { anyhow::bail!("forced apply failure") },
        )
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
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn retryable_apply_failure_delayed_naks_for_redelivery() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;
        let attempts = Arc::new(AtomicUsize::new(0));

        let err = apply_replication_message_with(test_message(ts, counts.clone()), {
            let attempts = attempts.clone();
            move |_delta, _transport_id| {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::Error::new(RetryableReplicaApplyError::new(
                        "prepared transaction still unresolved",
                    )))
                }
            }
        })
        .await
        .expect_err("retryable apply failure should be delayed-NAKed for redelivery");

        let message = format!("{err:#}");
        assert!(
            message.contains("Retryable failure applying replica delta"),
            "{message}",
        );
        assert!(message.contains("prepared transaction still unresolved"));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "retryable failures should not block the consumer by retrying in-process",
        );
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        assert_eq!(
            counts.delayed_nak_delays.lock().as_slice(),
            &[RETRYABLE_REPLICA_APPLY_NAK_DELAY],
        );
        Ok(())
    }

    #[tokio::test]
    async fn retryable_redelivery_can_ack_after_block_clears() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;

        let first_attempt = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |_delta, _transport_id| async move {
                Err(anyhow::Error::new(RetryableReplicaApplyError::new(
                    "prepared transaction still unresolved",
                )))
            },
        )
        .await;
        assert!(first_attempt.is_err());
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 1);

        let (applied_ts, num_updates) = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |delta, _transport_id| async move { Ok(delta.ts) },
        )
        .await?;

        assert_eq!(applied_ts, ts);
        assert_eq!(num_updates, 0);
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn consumer_continues_after_retryable_apply_failure() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let first_ts = Timestamp::try_from(42u64)?;
        let second_ts = Timestamp::try_from(43u64)?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let successes_for_callback = successes.clone();
        let stream = futures::stream::iter(vec![
            Ok(test_message(first_ts, counts.clone())),
            Ok(test_message(second_ts, counts.clone())),
        ]);

        let err = consume_replication_stream_with(
            Box::pin(stream),
            move |message| {
                let attempts = attempts.clone();
                async move {
                    apply_replication_message_with(message, |delta, _transport_id| {
                        let attempts = attempts.clone();
                        async move {
                            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                                Err(anyhow::Error::new(RetryableReplicaApplyError::new(
                                    "prepared transaction still unresolved",
                                )))
                            } else {
                                Ok(delta.ts)
                            }
                        }
                    })
                    .await
                }
            },
            move |_ts, _num_updates| {
                successes_for_callback.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .expect_err("finite test stream should report stream end");

        assert!(
            format!("{err:#}").contains("ReplicaDeltaConsumer stream ended"),
            "unexpected error: {err:#}",
        );
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 1);
        assert_eq!(successes.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn apply_failure_naks_then_redelivered_message_can_ack() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;

        let first_attempt = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |_delta, _transport_id| async move { anyhow::bail!("transient apply failure") },
        )
        .await;
        assert!(first_attempt.is_err());
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);

        let (applied_ts, _) = apply_replication_message_with(
            test_message(ts, counts.clone()),
            |delta, _transport_id| async move { Ok(delta.ts) },
        )
        .await?;
        assert_eq!(applied_ts, ts);
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn consumer_continues_after_apply_failure() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let first_ts = Timestamp::try_from(42u64)?;
        let second_ts = Timestamp::try_from(43u64)?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let successes_for_callback = successes.clone();
        let stream = futures::stream::iter(vec![
            Ok(test_message(first_ts, counts.clone())),
            Ok(test_message(second_ts, counts.clone())),
        ]);

        let err = consume_replication_stream_with(
            Box::pin(stream),
            move |message| {
                let attempts = attempts.clone();
                async move {
                    apply_replication_message_with(message, |delta, _transport_id| {
                        let attempts = attempts.clone();
                        async move {
                            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                                anyhow::bail!("transient apply failure");
                            }
                            Ok(delta.ts)
                        }
                    })
                    .await
                }
            },
            move |_ts, _num_updates| {
                successes_for_callback.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .expect_err("finite test stream should report stream end");

        assert!(
            format!("{err:#}").contains("ReplicaDeltaConsumer stream ended"),
            "unexpected error: {err:#}",
        );
        assert_eq!(counts.ack.load(Ordering::SeqCst), 1);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(successes.load(Ordering::SeqCst), 1);
        Ok(())
    }
}
