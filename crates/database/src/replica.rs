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
    commit_delta::DistributedLog,
    committer::CommitterClient,
};

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
            let delta = result.context("Error reading from distributed log")?;
            let ts = delta.ts;
            let num_updates = delta.document_updates.len();

            match committer.apply_replica_delta(delta).await {
                Ok(applied_ts) => {
                    tracing::info!(
                        "Applied replica delta: ts={}, {} updates",
                        u64::from(applied_ts),
                        num_updates,
                    );
                },
                Err(e) => {
                    tracing::error!(
                        "Failed to apply replica delta at ts={}: {e:#}",
                        u64::from(ts),
                    );
                },
            }
        }

        tracing::warn!("ReplicaDeltaConsumer stream ended — no more deltas");
        Ok(())
    }
}
