//! Transaction Watcher for 2PC crash recovery.
//!
//! Monitors NATS KV for unresolved 2PC transactions and resolves them:
//! - COMMITTED but not fully applied → send CommitPrepared to participants
//! - No decision after timeout → roll back
//! - ROLLED_BACK but not fully cleaned up → send RollbackPrepared
//!
//! Runs on every partitioned node. Uses NATS KV CAS to avoid conflicts
//! between watchers on different nodes.
//!
//! Follows Vitess's watcher pattern:
//! https://vitess.io/docs/22.0/reference/features/distributed-transaction/

use std::time::Duration;

use common::runtime::Runtime;

use crate::{
    committer::CommitterClient,
    partition::PartitionId,
    two_phase::{
        TwoPhaseDecision,
        TwoPhaseTransactionId,
        TWO_PHASE_DECISION_TTL_SECS,
        TWO_PHASE_KV_BUCKET,
        TWO_PHASE_KV_PREFIX,
    },
};

/// Start the transaction watcher as a background task.
/// Periodically scans NATS KV for unresolved 2PC transactions.
pub fn start<RT: Runtime>(
    runtime: RT,
    committer: CommitterClient,
    nats_url: String,
    local_partition: PartitionId,
) {
    runtime.spawn_background("two_phase_watcher", async move {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("2PC Watcher: Failed to connect to NATS: {e:#}");
                return;
            },
        };
        let jetstream = async_nats::jetstream::new(client);

        // Create or get the KV bucket for 2PC decisions.
        let kv = match jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: TWO_PHASE_KV_BUCKET.to_string(),
                history: 1,
                max_age: Duration::from_secs(TWO_PHASE_DECISION_TTL_SECS),
                ..Default::default()
            })
            .await
        {
            Ok(kv) => kv,
            Err(e) => {
                tracing::error!("2PC Watcher: Failed to create KV bucket: {e:#}");
                return;
            },
        };

        tracing::info!("2PC Watcher: started, scanning every 10s");

        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;

            // Scan all 2PC keys.
            let keys = match kv.keys().await {
                Ok(keys) => {
                    use futures::StreamExt;
                    keys.collect::<Vec<_>>().await
                },
                Err(e) => {
                    tracing::debug!("2PC Watcher: Failed to list keys: {e:#}");
                    continue;
                },
            };

            for key_result in keys {
                let key = match key_result {
                    Ok(k) => k,
                    Err(_) => continue,
                };
                if !key.starts_with(TWO_PHASE_KV_PREFIX) {
                    continue;
                }
                let txn_id_str = &key[TWO_PHASE_KV_PREFIX.len()..];
                let txn_id = TwoPhaseTransactionId(txn_id_str.to_string());

                let entry = match kv.entry(&key).await {
                    Ok(Some(e)) => e,
                    _ => continue,
                };

                let decision: TwoPhaseDecision = match serde_json::from_slice(&entry.value) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("2PC Watcher: Failed to parse decision for {key}: {e}");
                        continue;
                    },
                };

                match decision {
                    TwoPhaseDecision::Committed { participants, .. } => {
                        if !participants.contains(&local_partition.0) {
                            continue;
                        }
                        // Transaction was committed but the coordinator may have
                        // crashed before sending CommitPrepared to all participants.
                        // Try to commit locally — if already committed, this is a no-op.
                        match committer.commit_prepared(txn_id.clone()).await {
                            Ok(ts) => {
                                tracing::info!(
                                    "2PC Watcher: resolved committed txn={} at ts={}",
                                    txn_id,
                                    u64::from(ts),
                                );
                            },
                            Err(e) => {
                                // "unknown transaction" means it was already committed.
                                if e.to_string().contains("unknown transaction") {
                                    tracing::debug!(
                                        "2PC Watcher: committed txn={} already resolved locally",
                                        txn_id,
                                    );
                                } else {
                                    tracing::debug!(
                                        "2PC Watcher: CommitPrepared for {txn_id} not ready: {e:#}"
                                    );
                                }
                            },
                        }
                    },
                    TwoPhaseDecision::RolledBack { participants, .. } => {
                        if !participants.contains(&local_partition.0) {
                            continue;
                        }
                        // Transaction was rolled back but cleanup may be incomplete.
                        match committer.rollback_prepared(txn_id.clone()).await {
                            Ok(()) | Err(_) => {
                                tracing::debug!(
                                    "2PC Watcher: rolled back txn={} locally or it was already \
                                     gone",
                                    txn_id,
                                );
                            },
                        }
                    },
                }
            }
        }
    });
}
