//! Two-Phase Commit Coordinator (Vitess VTGate pattern).
//!
//! When a transaction writes to tables on multiple partitions, the coordinator:
//! 1. Splits writes by partition
//! 2. Allocates one global commit timestamp
//! 3. Sends Prepare to each participant
//! 4. If all succeed: sends CommitPrepared to all
//! 5. If any fail: sends RollbackPrepared to prepared participants
//!
//! The coordinator runs on the node where the mutation was received.
//! Remote partitions are reached via gRPC.

use std::collections::BTreeMap;

use anyhow::Context;
use common::{
    grpc::ClusterGrpcAuth,
    types::Timestamp,
};

use crate::{
    committer::CommitterClient,
    metrics,
    partition::{
        routed_partition_for_table,
        PartitionId,
        PartitionMap,
    },
    transaction::FinalTransaction,
    two_phase::{
        NodeAddresses,
        ParticipantTransaction,
        TwoPhaseCommitGrpcClient,
        TwoPhaseDecision,
        TwoPhaseTransactionId,
    },
    write_log::WriteSource,
};

const MAX_PREPARE_TS_RETRIES: usize = 8;

/// Result of classifying a transaction's writes by partition.
pub enum TransactionClassification {
    /// All writes target a single partition — use the normal fast path (TiDB
    /// 1PC).
    SinglePartition,
    /// All writes target a single remote partition. Route the finalized
    /// transaction to that owner instead of exposing partition topology to the
    /// client.
    RemoteSinglePartition { owner: PartitionId },
    /// Writes span multiple partitions — use 2PC.
    CrossPartition {
        /// Writes grouped by owning partition.
        partitions: BTreeMap<PartitionId, Vec<usize>>,
    },
}

/// Classify a transaction's writes by partition ownership.
pub fn classify_transaction(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> TransactionClassification {
    let mut partitions: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();

    for (i, write) in transaction.writes.coalesced_writes().enumerate() {
        let tablet_id = write.id.tablet_id;
        if let Ok(table_name) = transaction.table_mapping.tablet_name(tablet_id) {
            if let Some(partition) =
                routed_partition_for_table(&table_name, partition_map, write_source)
            {
                partitions.entry(partition).or_default().push(i);
            }
        }
    }

    if partitions.is_empty() {
        TransactionClassification::SinglePartition
    } else if partitions.len() == 1 {
        let owner = *partitions
            .keys()
            .next()
            .expect("single-partition classification should have one owner");
        if owner == partition_map.local_partition() {
            TransactionClassification::SinglePartition
        } else {
            TransactionClassification::RemoteSinglePartition { owner }
        }
    } else {
        TransactionClassification::CrossPartition { partitions }
    }
}

fn participant_write_indexes(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> BTreeMap<PartitionId, Vec<usize>> {
    let mut participant_indexes: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();
    let local_partition = partition_map.local_partition();

    for (index, write) in transaction.writes.coalesced_writes().enumerate() {
        let Ok(table_name) = transaction.table_mapping.tablet_name(write.id.tablet_id) else {
            continue;
        };
        let participant = routed_partition_for_table(&table_name, partition_map, write_source)
            .unwrap_or(local_partition);
        participant_indexes
            .entry(participant)
            .or_default()
            .push(index);
    }

    participant_indexes.retain(|_, indexes| !indexes.is_empty());
    participant_indexes
}

async fn prepare_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
    participant_tx: ParticipantTransaction,
    write_source: &WriteSource,
    prepare_ts: Timestamp,
    cluster_auth: Option<ClusterGrpcAuth>,
) -> anyhow::Result<()> {
    let result = if participant == partition_map.local_partition() {
        local_committer
            .prepare_remote(
                transaction_id.clone(),
                participant_tx,
                write_source.clone(),
                prepare_ts,
            )
            .await?
    } else {
        let node_addresses = node_addresses.with_context(|| {
            format!(
                "NODE_ADDRESSES is required to route this transaction to the authoritative owner \
                 {participant}"
            )
        })?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = match cluster_auth {
            Some(auth) => TwoPhaseCommitGrpcClient::connect_with_auth(addr, auth).await?,
            None => TwoPhaseCommitGrpcClient::connect(addr).await?,
        };
        client
            .prepare(
                transaction_id,
                participant_tx,
                write_source.clone(),
                prepare_ts,
                partition_map.placement_version(),
            )
            .await?
    };

    anyhow::ensure!(
        result.prepare_ts == prepare_ts,
        "Participant {} prepared at ts={} but coordinator assigned ts={}",
        participant,
        result.prepare_ts,
        prepare_ts,
    );
    Ok(())
}

async fn rollback_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
    cluster_auth: Option<ClusterGrpcAuth>,
) -> anyhow::Result<()> {
    if participant == partition_map.local_partition() {
        local_committer
            .rollback_prepared(transaction_id.clone())
            .await
    } else {
        let node_addresses = node_addresses.with_context(|| {
            format!(
                "NODE_ADDRESSES is required to route rollback to the authoritative owner \
                 {participant}"
            )
        })?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = match cluster_auth {
            Some(auth) => TwoPhaseCommitGrpcClient::connect_with_auth(addr, auth).await?,
            None => TwoPhaseCommitGrpcClient::connect(addr).await?,
        };
        client.rollback_prepared(transaction_id).await
    }
}

async fn commit_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
    cluster_auth: Option<ClusterGrpcAuth>,
) -> anyhow::Result<Timestamp> {
    if participant == partition_map.local_partition() {
        local_committer
            .commit_prepared(transaction_id.clone())
            .await
    } else {
        let node_addresses = node_addresses.with_context(|| {
            format!(
                "NODE_ADDRESSES is required to route commit to the authoritative owner \
                 {participant}"
            )
        })?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = match cluster_auth {
            Some(auth) => TwoPhaseCommitGrpcClient::connect_with_auth(addr, auth).await?,
            None => TwoPhaseCommitGrpcClient::connect(addr).await?,
        };
        Ok(client.commit_prepared(transaction_id).await?.try_into()?)
    }
}

async fn write_decision_record(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
    decision: &TwoPhaseDecision,
) -> anyhow::Result<()> {
    local_committer
        .two_phase_decision_log()
        .write_decision(transaction_id, decision)
        .await
}

async fn delete_decision_record(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
) -> anyhow::Result<()> {
    local_committer
        .two_phase_decision_log()
        .delete_decision(transaction_id)
        .await
}

fn is_retryable_prepare_ts_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("2PC Prepare assigned ts="))
}

/// Execute the 2PC protocol for a transaction that cannot commit on the local
/// fast path.
pub async fn coordinate_two_phase_commit(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
) -> anyhow::Result<Timestamp> {
    let timer = metrics::two_phase_coordinator_timer();
    match classify_transaction(&transaction, partition_map, &write_source) {
        TransactionClassification::SinglePartition => {
            anyhow::bail!("2PC coordinator called for a local single-partition transaction");
        },
        TransactionClassification::RemoteSinglePartition { owner } => {
            tracing::info!(
                "2PC Coordinator: routing single-partition transaction to remote owner {}",
                owner,
            );
        },
        TransactionClassification::CrossPartition { partitions: _ } => {},
    }

    let participant_indexes = participant_write_indexes(&transaction, partition_map, &write_source);
    let txn_id = TwoPhaseTransactionId::new();
    let node_addresses = local_committer.node_addresses();
    let cluster_auth = local_committer.cluster_grpc_auth();
    let participants: Vec<_> = participant_indexes
        .iter()
        .map(|(participant, write_indexes)| -> anyhow::Result<_> {
            Ok((
                *participant,
                ParticipantTransaction::from_final_transaction(
                    &transaction,
                    *participant,
                    write_indexes,
                    partition_map,
                    &write_source,
                )?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    metrics::log_two_phase_participants(participants.len());

    let mut last_retryable_error = None;
    let mut prepare_attempts = 0usize;
    for attempt in 0..MAX_PREPARE_TS_RETRIES {
        prepare_attempts = attempt + 1;
        let prepare_ts = local_committer.allocate_commit_ts().await?;
        tracing::info!(
            "2PC Coordinator: starting txn={}, participants={:?}, prepare_ts={}, attempt={}",
            txn_id,
            participant_indexes.keys().collect::<Vec<_>>(),
            u64::from(prepare_ts),
            attempt + 1,
        );

        let mut prepared_participants = Vec::new();
        let mut retry_prepare = false;
        for (participant, participant_tx) in &participants {
            match prepare_participant(
                local_committer,
                node_addresses,
                partition_map,
                &txn_id,
                *participant,
                participant_tx.clone(),
                &write_source,
                prepare_ts,
                cluster_auth.clone(),
            )
            .await
            {
                Ok(()) => prepared_participants.push(*participant),
                Err(err) => {
                    tracing::warn!(
                        "2PC Coordinator: prepare failed on {} for txn={}: {err:#}",
                        participant,
                        txn_id,
                    );
                    let retryable_prepare_ts_error =
                        is_retryable_prepare_ts_error(&err) && attempt + 1 < MAX_PREPARE_TS_RETRIES;
                    if !retryable_prepare_ts_error && !prepared_participants.is_empty() {
                        let decision = TwoPhaseDecision::RolledBack {
                            reason: err.to_string(),
                            participants: prepared_participants.iter().map(|p| p.0).collect(),
                        };
                        write_decision_record(local_committer, &txn_id, &decision)
                            .await
                            .with_context(|| {
                                format!(
                                    "2PC Coordinator: failed to write rollback decision for \
                                     txn={txn_id}"
                                )
                            })?;
                    }
                    for prepared in prepared_participants.iter().rev().copied() {
                        if let Err(rollback_err) = rollback_participant(
                            local_committer,
                            node_addresses,
                            partition_map,
                            &txn_id,
                            prepared,
                            cluster_auth.clone(),
                        )
                        .await
                        {
                            tracing::error!(
                                "2PC Coordinator: rollback failed on {} for txn={}: \
                                 {rollback_err:#}",
                                prepared,
                                txn_id,
                            );
                        }
                    }

                    if retryable_prepare_ts_error {
                        metrics::log_two_phase_prepare_retry();
                        tracing::info!(
                            "2PC Coordinator: retrying txn={} with a newer prepare timestamp \
                             after participant {} rejected ts on attempt {}",
                            txn_id,
                            participant,
                            attempt + 1,
                        );
                        last_retryable_error = Some(err);
                        retry_prepare = true;
                        break;
                    }
                    metrics::log_two_phase_error("prepare");
                    metrics::log_two_phase_decision("rollback");
                    metrics::log_two_phase_prepare_attempts(prepare_attempts);
                    return Err(err);
                },
            }
        }

        if retry_prepare {
            continue;
        }

        let decision = TwoPhaseDecision::Committed {
            commit_ts: u64::from(prepare_ts),
            participants: prepared_participants.iter().map(|p| p.0).collect(),
        };
        write_decision_record(local_committer, &txn_id, &decision)
            .await
            .with_context(|| {
                format!("2PC Coordinator: failed to write commit decision for txn={txn_id}")
            })?;

        let mut commit_results = Vec::new();
        for participant in &prepared_participants {
            let commit_ts = match commit_participant(
                local_committer,
                node_addresses,
                partition_map,
                &txn_id,
                *participant,
                cluster_auth.clone(),
            )
            .await
            {
                Ok(commit_ts) => commit_ts,
                Err(err) => {
                    metrics::log_two_phase_error("commit");
                    return Err(err);
                },
            };
            commit_results.push((*participant, commit_ts));
        }

        for (participant, commit_ts) in &commit_results {
            anyhow::ensure!(
                *commit_ts == prepare_ts,
                "Participant {} committed at ts={} but coordinator assigned ts={}",
                participant,
                commit_ts,
                prepare_ts,
            );
        }

        tracing::info!(
            "2PC Coordinator: committed txn={} at ts={}",
            txn_id,
            u64::from(prepare_ts),
        );
        metrics::log_two_phase_prepare_attempts(prepare_attempts);
        metrics::log_two_phase_decision("commit");
        if let Err(err) = delete_decision_record(local_committer, &txn_id).await {
            tracing::warn!(
                "2PC Coordinator: committed txn={} but failed to delete durable decision: {err:#}",
                txn_id,
            );
        }
        timer.finish();
        return Ok(prepare_ts);
    }

    metrics::log_two_phase_error("prepare");
    metrics::log_two_phase_decision("rollback");
    metrics::log_two_phase_prepare_attempts(prepare_attempts.max(1));
    Err(last_retryable_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "2PC coordinator exhausted prepare timestamp retries for txn={}",
            txn_id
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::is_retryable_prepare_ts_error;

    #[test]
    fn retryable_prepare_ts_error_detects_wrapped_grpc_status() {
        let err = anyhow::anyhow!(
            "Prepare failed: 2PC Prepare assigned ts=10 but this participant requires ts>=20"
        )
        .context("gRPC Prepare failed");
        assert!(is_retryable_prepare_ts_error(&err));
    }

    #[test]
    fn retryable_prepare_ts_error_rejects_unrelated_errors() {
        let err = anyhow::anyhow!("gRPC Prepare failed");
        assert!(!is_retryable_prepare_ts_error(&err));
    }
}
