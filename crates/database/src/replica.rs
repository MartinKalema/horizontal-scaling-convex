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
        replication_poison_gap,
        CommitDelta,
        DistributedLog,
        ReplicationMessage,
        ReplicationPoisonGap,
        ReplicationPoisonKind,
        ReplicationTransportId,
        RetryableReplicaApplyError,
    },
    committer::CommitterClient,
    metrics,
    partition::PartitionId,
};

const RETRYABLE_REPLICA_APPLY_NAK_DELAY: Duration = Duration::from_millis(250);

pub async fn apply_replication_message(
    committer: &CommitterClient,
    message: ReplicationMessage,
    excluded_source_partition: Option<PartitionId>,
) -> anyhow::Result<(common::types::Timestamp, usize)> {
    let committer = committer.clone();
    apply_replication_message_with(
        message,
        excluded_source_partition,
        move |delta, transport_id| {
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
        },
    )
    .await
}

pub async fn consume_replication_stream<F>(
    stream: BoxStream<'static, anyhow::Result<ReplicationMessage>>,
    committer: CommitterClient,
    excluded_source_partition: Option<PartitionId>,
    after_success: F,
) -> anyhow::Result<()>
where
    F: FnMut(common::types::Timestamp, usize),
{
    consume_replication_stream_with(
        stream,
        move |message| {
            let committer = committer.clone();
            async move {
                apply_replication_message(&committer, message, excluded_source_partition).await
            }
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
        let (applied_ts, num_updates) = apply_message(message).await?;
        after_success(applied_ts, num_updates);
    }

    anyhow::bail!("ReplicaDeltaConsumer stream ended")
}

async fn apply_replication_message_with<F, Fut>(
    message: ReplicationMessage,
    excluded_source_partition: Option<PartitionId>,
    mut apply: F,
) -> anyhow::Result<(common::types::Timestamp, usize)>
where
    F: FnMut(CommitDelta, Option<ReplicationTransportId>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<common::types::Timestamp>>,
{
    let (delta, transport_id, ack) = message.into_parts();
    let ts = delta.ts;
    let source_partition = delta.source_partition;
    let num_updates = delta.document_updates.len();

    if let Some(excluded_source_partition) = excluded_source_partition {
        if delta.source_partition == Some(excluded_source_partition) {
            tracing::debug!(
                source_partition = excluded_source_partition.0,
                origin_ts = u64::from(ts),
                "Acknowledging same-partition NATS delta without applying it; Raft is \
                 authoritative"
            );
            ack.ack().await.with_context(|| {
                format!(
                    "Failed to ack same-partition NATS delta at ts={}",
                    u64::from(ts),
                )
            })?;
            return Ok((ts, 0));
        }
    }

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
            if metrics::is_shutdown_error(&e) {
                tracing::info!(
                    "Replica delta at ts={} was interrupted by database shutdown; NAKing for \
                     redelivery",
                    u64::from(ts),
                );
                if let Err(ack_err) = ack.nak().await {
                    tracing::warn!(
                        "Failed to NAK shutdown-interrupted replica delta at ts={}: {ack_err:#}",
                        u64::from(ts),
                    );
                }
                return Err(e.context(format!(
                    "Database shutdown interrupted replica delta at ts={}",
                    u64::from(ts),
                )));
            }
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
                return Err(e.context(format!(
                    "Retryable failure applying replica delta at ts={}",
                    u64::from(ts),
                )));
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
            Err(ReplicationPoisonGap::new(
                ReplicationPoisonKind::DeterministicApplyFailure,
                None,
                None,
                transport_id.map(ReplicationTransportId::stream_sequence),
                None,
                source_partition,
                Some(ts),
                format!("failed to apply replica delta: {e:#}"),
            )
            .into())
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
                    if replication_poison_gap(&e).is_some() {
                        tracing::error!(
                            "ReplicaDeltaConsumer encountered a fatal replication gap and will \
                             not restart: {e:#}"
                        );
                        return;
                    }
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

        consume_replication_stream(stream, committer, None, |applied_ts, num_updates| {
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
            replication_poison_gap,
            CommitDelta,
            ReplicationAck,
            ReplicationMessage,
            ReplicationPoisonGap,
            ReplicationPoisonKind,
            RetryableReplicaApplyError,
        },
        metrics,
        partition::PartitionId,
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
        test_message_from_partition(ts, None, counts)
    }

    fn test_message_from_partition(
        ts: Timestamp,
        source_partition: Option<PartitionId>,
        counts: Arc<AckCounts>,
    ) -> ReplicationMessage {
        ReplicationMessage::new(
            CommitDelta {
                ts,
                document_writes: Arc::new(Vec::new()),
                document_updates: Vec::new(),
                index_writes: Arc::new(Vec::new()),
                write_source: WriteSource::unknown(),
                write_bytes: 0,
                tablet_id_to_table_identity: BTreeMap::new(),
                source_partition,
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
            None,
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
    async fn same_partition_nats_delta_is_acked_without_apply() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let attempts = Arc::new(AtomicUsize::new(0));
        let ts = Timestamp::try_from(42u64)?;
        let local_partition = PartitionId(1);

        let (applied_ts, num_updates) = apply_replication_message_with(
            test_message_from_partition(ts, Some(local_partition), counts.clone()),
            Some(local_partition),
            {
                let attempts = attempts.clone();
                move |delta, _transport_id| {
                    let attempts = attempts.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        Ok(delta.ts)
                    }
                }
            },
        )
        .await?;

        assert_eq!(applied_ts, ts);
        assert_eq!(num_updates, 0);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
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
            None,
            |_delta, _transport_id| async move { anyhow::bail!("forced apply failure") },
        )
        .await
        .unwrap_err();

        let message = format!("{err:#}");
        let gap = replication_poison_gap(&err).expect("apply failure must become a poison gap");
        assert_eq!(gap.kind, ReplicationPoisonKind::DeterministicApplyFailure);
        assert!(message.contains("replication poison gap"), "{message}");
        assert!(message.contains("forced apply failure"), "{message}");
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.term.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_interrupted_apply_naks_without_poisoning() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;
        let err = apply_replication_message_with(
            test_message(ts, counts.clone()),
            None,
            |_delta, _transport_id| async move { Err(metrics::shutdown_error()) },
        )
        .await
        .expect_err("shutdown-interrupted apply must return to the retry supervisor");

        let message = format!("{err:#}");
        assert!(metrics::is_shutdown_error(&err), "{message}");
        assert!(
            replication_poison_gap(&err).is_none(),
            "shutdown must not create durable replication poison: {message}",
        );
        assert!(
            message.contains("Database shutdown interrupted replica delta"),
            "{message}",
        );
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

        let err = apply_replication_message_with(test_message(ts, counts.clone()), None, {
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
            None,
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
            None,
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
    async fn consumer_stops_before_later_delta_after_retryable_apply_failure() -> anyhow::Result<()>
    {
        let counts = Arc::new(AckCounts::default());
        let first_ts = Timestamp::try_from(42u64)?;
        let second_ts = Timestamp::try_from(43u64)?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let successes_for_callback = successes.clone();
        let stream = futures::stream::iter(vec![
            Ok(test_message(first_ts, counts.clone())),
            Ok(test_message_from_partition(
                second_ts,
                Some(PartitionId(1)),
                counts.clone(),
            )),
        ]);
        let attempts_for_consumer = attempts.clone();

        let err = consume_replication_stream_with(
            Box::pin(stream),
            move |message| {
                let attempts = attempts_for_consumer.clone();
                async move {
                    apply_replication_message_with(message, None, |delta, _transport_id| {
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

        assert!(format!("{err:#}").contains("Retryable failure applying replica delta"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 0);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 1);
        assert_eq!(
            successes.load(Ordering::SeqCst),
            0,
            "later frontier heartbeat must not cross the blocked delta",
        );
        Ok(())
    }

    #[tokio::test]
    async fn apply_failure_naks_then_redelivered_message_can_ack() -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let ts = Timestamp::try_from(42u64)?;

        let first_attempt = apply_replication_message_with(
            test_message(ts, counts.clone()),
            None,
            |_delta, _transport_id| async move { anyhow::bail!("transient apply failure") },
        )
        .await;
        assert!(first_attempt.is_err());
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);

        let (applied_ts, _) = apply_replication_message_with(
            test_message(ts, counts.clone()),
            None,
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
    async fn consumer_fails_closed_before_heartbeat_after_deterministic_apply_failure(
    ) -> anyhow::Result<()> {
        let counts = Arc::new(AckCounts::default());
        let first_ts = Timestamp::try_from(42u64)?;
        let second_ts = Timestamp::try_from(43u64)?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let successes_for_callback = successes.clone();
        let stream = futures::stream::iter(vec![
            Ok(test_message(first_ts, counts.clone())),
            Ok(test_message_from_partition(
                second_ts,
                Some(PartitionId(1)),
                counts.clone(),
            )),
        ]);
        let attempts_for_consumer = attempts.clone();

        let err = consume_replication_stream_with(
            Box::pin(stream),
            move |message| {
                let attempts = attempts_for_consumer.clone();
                async move {
                    apply_replication_message_with(message, None, |delta, _transport_id| {
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

        let gap = replication_poison_gap(&err).expect("apply failure must become a poison gap");
        assert_eq!(gap.kind, ReplicationPoisonKind::DeterministicApplyFailure);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "consumer must stop before applying the later heartbeat",
        );
        assert_eq!(counts.ack.load(Ordering::SeqCst), 0);
        assert_eq!(counts.nak.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delayed_nak.load(Ordering::SeqCst), 0);
        assert_eq!(
            successes.load(Ordering::SeqCst),
            0,
            "frontier callback must not advance beyond a poisoned delta",
        );
        Ok(())
    }

    #[tokio::test]
    async fn transport_poison_stops_before_following_heartbeat() -> anyhow::Result<()> {
        for kind in [
            ReplicationPoisonKind::MalformedEnvelope,
            ReplicationPoisonKind::UnsupportedEnvelopeVersion,
            ReplicationPoisonKind::InvalidDeltaPayload,
        ] {
            let counts = Arc::new(AckCounts::default());
            let attempts = Arc::new(AtomicUsize::new(0));
            let successes = Arc::new(AtomicUsize::new(0));
            let heartbeat_ts = Timestamp::try_from(43u64)?;
            let gap = ReplicationPoisonGap::new(
                kind,
                Some("node-1".to_string()),
                Some("convex.commits.0".to_string()),
                Some(42),
                Some("node-0".to_string()),
                Some(PartitionId(0)),
                Some(Timestamp::try_from(42u64)?),
                "forced transport poison",
            );
            let stream = futures::stream::iter(vec![
                Err(anyhow::Error::new(gap.clone())),
                Ok(test_message_from_partition(
                    heartbeat_ts,
                    Some(PartitionId(0)),
                    counts.clone(),
                )),
            ]);
            let attempts_for_consumer = attempts.clone();
            let successes_for_callback = successes.clone();

            let error = consume_replication_stream_with(
                Box::pin(stream),
                move |_message| {
                    let attempts = attempts_for_consumer.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        Ok((heartbeat_ts, 0))
                    }
                },
                move |_ts, _num_updates| {
                    successes_for_callback.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .expect_err("transport poison must terminate the consumer");

            assert_eq!(replication_poison_gap(&error), Some(&gap));
            assert_eq!(attempts.load(Ordering::SeqCst), 0);
            assert_eq!(successes.load(Ordering::SeqCst), 0);
            assert_eq!(
                counts.ack.load(Ordering::SeqCst),
                0,
                "heartbeat after {kind:?} must remain unacknowledged",
            );
        }
        Ok(())
    }
}
