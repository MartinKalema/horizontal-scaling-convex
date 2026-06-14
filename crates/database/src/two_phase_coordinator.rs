//! Two-Phase Commit Coordinator (Vitess VTGate pattern).
//!
//! When a transaction writes to tables on multiple partitions, the coordinator:
//! 1. Splits writes by partition
//! 2. Allocates one global commit timestamp
//! 3. Sends Prepare to each participant
//! 4. If all succeed: sends CommitPrepared to all
//! 5. If any fail: records rollback for participants that may have prepared
//!
//! The coordinator runs on the node where the mutation was received.
//! Remote partitions are reached via gRPC.

use std::collections::BTreeMap;

use anyhow::{
    anyhow,
    Context,
};
use common::{
    bootstrap_model::{
        index::{
            TabletIndexMetadata,
            INDEX_TABLE,
        },
        tables::{
            TableMetadata,
            TABLES_TABLE,
        },
    },
    document::{
        DocumentUpdateWithPrevTs,
        ParseDocument,
        ResolvedDocument,
    },
    types::Timestamp,
};

use crate::{
    committer::{
        CommitOutcome,
        CommitterClient,
    },
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
        if let Some(partition) =
            routed_partition_for_write(transaction, write, partition_map, write_source)
        {
            partitions.entry(partition).or_default().push(i);
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

pub(crate) fn participant_write_indexes(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> BTreeMap<PartitionId, Vec<usize>> {
    let mut participant_indexes: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();

    for (index, write) in transaction.writes.coalesced_writes().enumerate() {
        let Some(participant) =
            routed_partition_for_write(transaction, write, partition_map, write_source)
        else {
            continue;
        };
        participant_indexes
            .entry(participant)
            .or_default()
            .push(index);
    }

    participant_indexes.retain(|_, indexes| !indexes.is_empty());
    participant_indexes
}

fn routed_partition_for_write(
    transaction: &FinalTransaction,
    write: &DocumentUpdateWithPrevTs,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> Option<PartitionId> {
    let table_name = transaction
        .table_mapping
        .tablet_name(write.id.tablet_id)
        .ok()?;
    if let Some(partition) = routed_partition_for_table(&table_name, partition_map, write_source) {
        return Some(partition);
    }
    routed_partition_for_catalog_write(transaction, write, partition_map)
}

fn routed_partition_for_catalog_write(
    transaction: &FinalTransaction,
    write: &DocumentUpdateWithPrevTs,
    partition_map: &PartitionMap,
) -> Option<PartitionId> {
    let catalog_table = transaction
        .table_mapping
        .tablet_name(write.id.tablet_id)
        .ok()?;
    if &catalog_table != &*TABLES_TABLE && &catalog_table != &*INDEX_TABLE {
        return None;
    }
    let document = write
        .new_document
        .as_ref()
        .or_else(|| write.old_document.as_ref().map(|(document, _)| document))?;
    let user_table = if &catalog_table == &*TABLES_TABLE {
        catalog_write_user_table(document)?
    } else {
        let index: common::document::ParsedDocument<TabletIndexMetadata> = document.parse().ok()?;
        let index = index.into_value();
        let indexed_tablet = *index.name.table();
        transaction.table_mapping.tablet_name(indexed_tablet).ok()?
    };
    if user_table.is_system() {
        return None;
    }
    Some(partition_map.partition_for_table(&user_table))
}

fn catalog_write_user_table(document: &ResolvedDocument) -> Option<value::TableName> {
    let metadata: common::document::ParsedDocument<TableMetadata> = document.parse().ok()?;
    Some(metadata.into_value().name)
}

async fn prepare_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    all_participants: &[PartitionId],
    participant: PartitionId,
    participant_tx: ParticipantTransaction,
    write_source: &WriteSource,
    prepare_ts: Timestamp,
) -> anyhow::Result<()> {
    let result = if participant == partition_map.local_partition() {
        local_committer
            .prepare_remote(
                transaction_id.clone(),
                participant_tx,
                write_source.clone(),
                prepare_ts,
                all_participants.to_vec(),
            )
            .await?
    } else {
        let node_addresses = node_addresses.with_context(|| {
            format!(
                "NODE_ADDRESSES is required to route this transaction to the authoritative owner \
                 {participant}"
            )
        })?;
        let addresses = node_addresses
            .addresses_for(participant)
            .filter(|addresses| !addresses.is_empty())
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let mut last_err = None;
        for addr in addresses {
            match local_committer.two_phase_client(addr).await {
                Ok(client) => {
                    match client
                        .prepare(
                            transaction_id,
                            participant_tx.clone(),
                            write_source.clone(),
                            prepare_ts,
                            partition_map.placement_version(),
                            all_participants,
                        )
                        .await
                    {
                        Ok(result) => {
                            return verify_prepare_result(participant, prepare_ts, result)
                        },
                        Err(err) if should_try_next_participant_address(&err) => {
                            tracing::warn!(
                                "2PC Coordinator: prepare to participant {} via {} failed, trying \
                                 next address: {err:#}",
                                participant,
                                addr,
                            );
                            last_err = Some(err);
                        },
                        Err(err) => return Err(err),
                    }
                },
                Err(err) if should_try_next_participant_address(&err) => {
                    tracing::warn!(
                        "2PC Coordinator: failed to connect to participant {} via {}, trying next \
                         address: {err:#}",
                        participant,
                        addr,
                    );
                    last_err = Some(err);
                },
                Err(err) => return Err(err),
            }
        }
        return Err(last_err.unwrap_or_else(|| {
            anyhow!("2PC: No usable NODE_ADDRESSES entry for participant {participant}")
        }));
    };

    verify_prepare_result(participant, prepare_ts, result)
}

async fn rollback_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
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
        let addresses = node_addresses
            .addresses_for(participant)
            .filter(|addresses| !addresses.is_empty())
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        try_participant_addresses("rollback", participant, addresses, |addr| async move {
            let client = local_committer.two_phase_client(addr).await?;
            client.rollback_prepared(transaction_id).await
        })
        .await
    }
}

async fn commit_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
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
        let addresses = node_addresses
            .addresses_for(participant)
            .filter(|addresses| !addresses.is_empty())
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        try_participant_addresses("commit", participant, addresses, |addr| async move {
            let client = local_committer.two_phase_client(addr).await?;
            Ok(client.commit_prepared(transaction_id).await?.try_into()?)
        })
        .await
    }
}

fn verify_prepare_result(
    participant: PartitionId,
    prepare_ts: Timestamp,
    result: crate::two_phase::PrepareResult,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        result.prepare_ts == prepare_ts,
        "Participant {} prepared at ts={} but coordinator assigned ts={}",
        participant,
        result.prepare_ts,
        prepare_ts,
    );
    Ok(())
}

async fn try_participant_addresses<'a, T, Fut>(
    operation: &'static str,
    participant: PartitionId,
    addresses: &'a [String],
    mut call: impl FnMut(&'a str) -> Fut,
) -> anyhow::Result<T>
where
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut last_err = None;
    for addr in addresses {
        match call(addr).await {
            Ok(result) => return Ok(result),
            Err(err) if should_try_next_participant_address(&err) => {
                tracing::warn!(
                    "2PC Coordinator: {} to participant {} via {} failed, trying next address: \
                     {err:#}",
                    operation,
                    participant,
                    addr,
                );
                last_err = Some(err);
            },
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow!("2PC: No usable NODE_ADDRESSES entry for participant {participant}")
    }))
}

async fn write_decision_record(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
    decision: &TwoPhaseDecision,
) -> anyhow::Result<()> {
    let decision_log = local_committer.two_phase_decision_log();
    if decision_log
        .write_decision_if_absent(transaction_id, decision)
        .await?
    {
        return Ok(());
    }
    let existing = decision_log
        .get_decision(transaction_id)
        .await?
        .with_context(|| {
            format!(
                "2PC Coordinator: decision for txn={transaction_id} already existed but could not \
                 be read"
            )
        })?;
    anyhow::ensure!(
        existing == *decision,
        "2PC Coordinator: durable decision for txn={} is already {:?}, cannot overwrite with {:?}",
        transaction_id,
        existing,
        decision,
    );
    Ok(())
}

async fn mark_participant_resolved(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
    participant: PartitionId,
) -> anyhow::Result<()> {
    let decision_log = local_committer.two_phase_decision_log();
    let Some(decision) = decision_log
        .mark_participant_resolved(transaction_id, participant)
        .await?
    else {
        return Ok(());
    };
    if decision.all_participants_resolved() {
        decision_log.delete_decision(transaction_id).await?;
        tracing::info!(
            "2PC Coordinator: deleted fully resolved decision for txn={}",
            transaction_id,
        );
    }
    Ok(())
}

fn is_retryable_prepare_ts_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("2PC Prepare assigned ts="))
}

fn is_ambiguous_prepare_error(err: &anyhow::Error) -> bool {
    should_try_next_participant_address(err)
}

fn should_try_next_participant_address(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("transport error")
        || message.contains("connection error")
        || message.contains("connection refused")
        || message.contains("broken pipe")
        || message.contains("deadline has elapsed")
        || message.contains("timed out")
        || message.contains("unavailable")
        || message.contains("no elected raft leader")
        || message.contains("missing raft_peers entry for raft leader")
        || message.contains("failed to connect to raft leader")
        || message.contains("leader prepare failed")
        || message.contains("leader commitprepared failed")
        || message.contains("leader rollbackprepared failed")
}

fn is_unknown_rollback_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("unknown transaction")
}

/// Execute the 2PC protocol for a transaction that cannot commit on the local
/// fast path.
pub async fn coordinate_two_phase_commit(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
) -> anyhow::Result<CommitOutcome> {
    let timer = metrics::two_phase_coordinator_timer();
    let source_partition = match classify_transaction(&transaction, partition_map, &write_source) {
        TransactionClassification::SinglePartition => {
            anyhow::bail!("2PC coordinator called for a local single-partition transaction");
        },
        TransactionClassification::RemoteSinglePartition { owner } => {
            tracing::info!(
                "2PC Coordinator: routing single-partition transaction to remote owner {}",
                owner,
            );
            Some(owner)
        },
        TransactionClassification::CrossPartition { partitions: _ } => None,
    };

    let participant_indexes = participant_write_indexes(&transaction, partition_map, &write_source);
    let node_addresses = local_committer.node_addresses();
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
    let all_participants: Vec<_> = participants
        .iter()
        .map(|(participant, _)| *participant)
        .collect();
    metrics::log_two_phase_participants(participants.len());

    let mut last_retryable_error = None;
    let mut prepare_attempts = 0usize;
    for attempt in 0..MAX_PREPARE_TS_RETRIES {
        let txn_id = TwoPhaseTransactionId::new();
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
        let mut attempted_participants = Vec::new();
        let mut retry_prepare = false;
        for (participant, participant_tx) in &participants {
            attempted_participants.push(*participant);
            match prepare_participant(
                local_committer,
                node_addresses,
                partition_map,
                &txn_id,
                &all_participants,
                *participant,
                participant_tx.clone(),
                &write_source,
                prepare_ts,
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
                    let rollback_participants = if is_ambiguous_prepare_error(&err) {
                        attempted_participants.clone()
                    } else {
                        prepared_participants.clone()
                    };
                    if !rollback_participants.is_empty() {
                        let decision = TwoPhaseDecision::rolled_back(
                            err.to_string(),
                            rollback_participants.iter().map(|p| p.0).collect(),
                        );
                        write_decision_record(local_committer, &txn_id, &decision)
                            .await
                            .with_context(|| {
                                format!(
                                    "2PC Coordinator: failed to write rollback decision for \
                                     txn={txn_id}"
                                )
                            })?;
                    }
                    for prepared in rollback_participants.iter().rev().copied() {
                        match rollback_participant(
                            local_committer,
                            node_addresses,
                            partition_map,
                            &txn_id,
                            prepared,
                        )
                        .await
                        {
                            Ok(()) => {
                                if let Err(mark_err) =
                                    mark_participant_resolved(local_committer, &txn_id, prepared)
                                        .await
                                {
                                    tracing::warn!(
                                        "2PC Coordinator: rollback resolved on {} for txn={} but \
                                         failed to mark participant resolved: {mark_err:#}",
                                        prepared,
                                        txn_id,
                                    );
                                }
                            },
                            Err(rollback_err) if is_unknown_rollback_error(&rollback_err) => {
                                if let Err(mark_err) =
                                    mark_participant_resolved(local_committer, &txn_id, prepared)
                                        .await
                                {
                                    tracing::warn!(
                                        "2PC Coordinator: rollback on {} for txn={} was already \
                                         clean but failed to mark participant resolved: \
                                         {mark_err:#}",
                                        prepared,
                                        txn_id,
                                    );
                                }
                            },
                            Err(rollback_err) => {
                                tracing::error!(
                                    "2PC Coordinator: rollback failed on {} for txn={}: \
                                     {rollback_err:#}",
                                    prepared,
                                    txn_id,
                                );
                            },
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

        let decision = TwoPhaseDecision::committed(
            u64::from(prepare_ts),
            prepared_participants.iter().map(|p| p.0).collect(),
        );
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
            )
            .await
            {
                Ok(commit_ts) => commit_ts,
                Err(err) => {
                    metrics::log_two_phase_error("commit");
                    return Err(err);
                },
            };
            mark_participant_resolved(local_committer, &txn_id, *participant)
                .await
                .with_context(|| {
                    format!(
                        "2PC Coordinator: participant {participant} committed txn={txn_id} but \
                         resolution acknowledgement failed"
                    )
                })?;
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
        timer.finish();
        return Ok(CommitOutcome {
            ts: prepare_ts,
            source_partition,
        });
    }

    metrics::log_two_phase_error("prepare");
    metrics::log_two_phase_decision("rollback");
    metrics::log_two_phase_prepare_attempts(prepare_attempts.max(1));
    Err(last_retryable_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "2PC coordinator exhausted prepare timestamp retries after {} attempt(s)",
            prepare_attempts.max(1),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        is_ambiguous_prepare_error,
        is_retryable_prepare_ts_error,
    };

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

    #[test]
    fn ambiguous_prepare_error_detects_transport_failure() {
        let err = anyhow::anyhow!(
            "gRPC Prepare failed: status: Unknown, message: \"transport error\", details: [], \
             metadata: MetadataMap {{ headers: {{}} }}: transport error: connection error: stream \
             closed because of a broken pipe"
        );
        assert!(is_ambiguous_prepare_error(&err));
    }

    #[test]
    fn ambiguous_prepare_error_rejects_application_failure() {
        let err = anyhow::anyhow!(
            "gRPC Prepare failed: status: Internal, message: \"forced prepare failure\""
        );
        assert!(!is_ambiguous_prepare_error(&err));
    }
}
