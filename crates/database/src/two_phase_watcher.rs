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
    metrics,
    partition::PartitionId,
    two_phase::{
        StagingRecoveryResult,
        TwoPhaseDecision,
        TwoPhaseDecisionLog,
        TwoPhaseTransactionId,
        PREPARE_TIMEOUT_SECS,
        TWO_PHASE_KV_BUCKET,
        TWO_PHASE_KV_PREFIX,
    },
};

const RESOLVED_DECISION_GC_GRACE: Duration = Duration::from_secs(60);

async fn apply_final_decision_for_local_partition(
    committer: &CommitterClient,
    local_partition: PartitionId,
    txn_id: &TwoPhaseTransactionId,
    decision: TwoPhaseDecision,
) -> anyhow::Result<bool> {
    match decision {
        TwoPhaseDecision::Staging { .. } => Ok(false),
        TwoPhaseDecision::Committed { participants, .. } => {
            if !participants.contains(&local_partition.0) {
                return Ok(false);
            }
            // Transaction was committed but the coordinator may have crashed
            // before sending CommitPrepared to all participants. Try to commit
            // locally — if already committed, this is a no-op.
            match committer.try_commit_prepared(txn_id.clone()).await {
                Ok(ts) => {
                    tracing::info!(
                        "2PC Watcher: resolved committed txn={} at ts={}",
                        txn_id,
                        u64::from(ts),
                    );
                    Ok(true)
                },
                Err(e) => {
                    // "unknown transaction" means it was already committed or
                    // this participant never prepared.
                    if e.to_string().contains("unknown transaction") {
                        tracing::debug!(
                            "2PC Watcher: committed txn={} already resolved locally",
                            txn_id,
                        );
                        Ok(true)
                    } else {
                        tracing::debug!(
                            "2PC Watcher: CommitPrepared for {txn_id} not ready: {e:#}"
                        );
                        Ok(false)
                    }
                },
            }
        },
        TwoPhaseDecision::RolledBack { participants, .. } => {
            if !participants.contains(&local_partition.0) {
                return Ok(false);
            }
            // Transaction was rolled back but cleanup may be incomplete.
            match committer.rollback_prepared(txn_id.clone()).await {
                Ok(()) => {
                    tracing::debug!(
                        "2PC Watcher: rolled back txn={} locally or it was already gone",
                        txn_id,
                    );
                    Ok(true)
                },
                Err(e) if e.to_string().contains("unknown transaction") => {
                    tracing::debug!(
                        "2PC Watcher: rollback txn={} already resolved locally",
                        txn_id,
                    );
                    Ok(true)
                },
                Err(e) => {
                    tracing::debug!("2PC Watcher: RollbackPrepared for {txn_id} not ready: {e:#}");
                    Ok(false)
                },
            }
        },
    }
}

pub async fn garbage_collect_resolved_decision(
    decision_log: &dyn TwoPhaseDecisionLog,
    txn_id: &TwoPhaseTransactionId,
    decision: &TwoPhaseDecision,
    grace: Duration,
) -> anyhow::Result<bool> {
    if !decision.ready_for_gc(grace) {
        return Ok(false);
    }
    decision_log.delete_decision(txn_id).await?;
    tracing::info!(
        "2PC Watcher: deleted fully resolved decision for txn={} after {:?} grace",
        txn_id,
        grace,
    );
    Ok(true)
}

/// Apply an existing durable 2PC decision to this local participant.
pub async fn resolve_decision_for_local_partition(
    committer: &CommitterClient,
    local_partition: PartitionId,
    txn_id: TwoPhaseTransactionId,
    decision: TwoPhaseDecision,
) -> anyhow::Result<bool> {
    let resolved = match decision {
        TwoPhaseDecision::Staging { .. } => {
            let decision_log = committer.two_phase_decision_log();
            match decision_log.recover_staging_decision(&txn_id).await? {
                StagingRecoveryResult::Committed(recovered) => {
                    metrics::log_two_phase_staging_recovery("committed");
                    apply_final_decision_for_local_partition(
                        committer,
                        local_partition,
                        &txn_id,
                        recovered,
                    )
                    .await?
                },
                StagingRecoveryResult::AlreadyFinal(recovered) => {
                    metrics::log_two_phase_staging_recovery("already_final");
                    apply_final_decision_for_local_partition(
                        committer,
                        local_partition,
                        &txn_id,
                        recovered,
                    )
                    .await?
                },
                StagingRecoveryResult::Pending(pending) => {
                    metrics::log_two_phase_staging_recovery("pending");
                    if let Some(age) = pending.staged_age() {
                        metrics::log_two_phase_staging_age(age);
                    }
                    tracing::debug!(
                        "2PC Watcher: staging txn={} still missing participant proofs",
                        txn_id,
                    );
                    false
                },
                StagingRecoveryResult::Missing => {
                    metrics::log_two_phase_staging_recovery("missing");
                    false
                },
            }
        },
        final_decision => {
            apply_final_decision_for_local_partition(
                committer,
                local_partition,
                &txn_id,
                final_decision,
            )
            .await?
        },
    };

    if resolved {
        let decision_log = committer.two_phase_decision_log();
        if let Some(updated) = decision_log
            .mark_participant_resolved(&txn_id, local_partition)
            .await?
            && updated.all_participants_resolved()
        {
            if !garbage_collect_resolved_decision(
                decision_log.as_ref(),
                &txn_id,
                &updated,
                RESOLVED_DECISION_GC_GRACE,
            )
            .await?
            {
                tracing::info!(
                    "2PC Watcher: fully resolved decision for txn={} retained for follower \
                     recovery grace",
                    txn_id,
                );
            }
        }
    }
    Ok(resolved)
}

/// Resolve local prepared transactions that no longer have a coordinator
/// decision. The watcher writes an authoritative rollback decision first, then
/// applies that decision locally. The create-if-absent write prevents a timeout
/// rollback from overwriting a coordinator's committed decision.
pub async fn rollback_expired_local_prepares(
    committer: &CommitterClient,
    local_partition: PartitionId,
    timeout: Duration,
) -> anyhow::Result<usize> {
    let records = committer.two_phase_redo_records().await?;
    let decision_log = committer.two_phase_decision_log();
    let mut resolved = 0usize;
    for entry in records.values() {
        if entry.partition_id() != local_partition || !entry.prepare_timed_out(timeout) {
            continue;
        }
        let txn_id = entry.transaction_id();
        let decision = match decision_log.get_decision(&txn_id).await? {
            Some(decision) => decision,
            None => {
                let participants = entry
                    .participants()
                    .into_iter()
                    .map(|partition| partition.0)
                    .collect();
                let rollback = TwoPhaseDecision::rolled_back(
                    format!(
                        "2PC prepare timed out after {:?} without a durable coordinator decision",
                        timeout,
                    ),
                    participants,
                );
                if decision_log
                    .write_decision_if_absent(&txn_id, &rollback)
                    .await?
                {
                    tracing::warn!(
                        "2PC Watcher: wrote timeout rollback decision for abandoned txn={}",
                        txn_id,
                    );
                    rollback
                } else {
                    decision_log.get_decision(&txn_id).await?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "2PC Watcher: decision for txn={} appeared during timeout rollback \
                             race but could not be read",
                            txn_id,
                        )
                    })?
                }
            },
        };
        if resolve_decision_for_local_partition(committer, local_partition, txn_id, decision)
            .await?
        {
            resolved += 1;
        }
    }
    Ok(resolved)
}

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

            // Resolve expired prepares before final decisions. Otherwise a
            // committed transaction ordered after an abandoned prepare can
            // wait forever and prevent this single watcher loop from ever
            // reaching the timeout rollback that unblocks it.
            if let Err(err) = rollback_expired_local_prepares(
                &committer,
                local_partition,
                Duration::from_secs(PREPARE_TIMEOUT_SECS),
            )
            .await
            {
                tracing::debug!("2PC Watcher: failed to resolve expired prepares: {err:#}");
            }

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

                if let Err(err) = resolve_decision_for_local_partition(
                    &committer,
                    local_partition,
                    txn_id,
                    decision,
                )
                .await
                {
                    tracing::debug!("2PC Watcher: failed to resolve local decision: {err:#}");
                }
            }
        }
    });
}
