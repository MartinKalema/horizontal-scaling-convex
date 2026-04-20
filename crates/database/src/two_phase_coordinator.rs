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
use common::types::Timestamp;

use crate::{
    committer::CommitterClient,
    partition::{
        PartitionId,
        PartitionMap,
    },
    transaction::FinalTransaction,
    two_phase::{
        NodeAddresses,
        ParticipantTransaction,
        TwoPhaseCommitGrpcClient,
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
) -> TransactionClassification {
    let mut partitions: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();

    for (i, write) in transaction.writes.coalesced_writes().enumerate() {
        let tablet_id = write.id.tablet_id;
        if let Ok(table_name) = transaction.table_mapping.tablet_name(tablet_id) {
            // System tables stay on the coordinator path.
            if table_name.is_system() {
                continue;
            }
            let partition = partition_map.partition_for_table(&table_name);
            partitions.entry(partition).or_default().push(i);
        }
    }

    if partitions.len() <= 1 {
        TransactionClassification::SinglePartition
    } else {
        TransactionClassification::CrossPartition { partitions }
    }
}

fn participant_write_indexes(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    user_partitions: &BTreeMap<PartitionId, Vec<usize>>,
) -> BTreeMap<PartitionId, Vec<usize>> {
    let mut participant_indexes = user_partitions.clone();
    let local_partition = partition_map.local_partition();

    for (index, write) in transaction.writes.coalesced_writes().enumerate() {
        if transaction
            .table_mapping
            .tablet_name(write.id.tablet_id)
            .is_ok_and(|table_name| table_name.is_system())
        {
            participant_indexes
                .entry(local_partition)
                .or_default()
                .push(index);
        }
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
        let node_addresses = node_addresses.context(
            "NODE_ADDRESSES is required for cross-partition transactions with remote participants",
        )?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = TwoPhaseCommitGrpcClient::connect(addr).await?;
        client
            .prepare(
                transaction_id,
                participant_tx,
                write_source.clone(),
                prepare_ts,
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
) -> anyhow::Result<()> {
    if participant == partition_map.local_partition() {
        local_committer
            .rollback_prepared(transaction_id.clone())
            .await
    } else {
        let node_addresses = node_addresses.context(
            "NODE_ADDRESSES is required for cross-partition transactions with remote participants",
        )?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = TwoPhaseCommitGrpcClient::connect(addr).await?;
        client.rollback_prepared(transaction_id).await
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
        let node_addresses = node_addresses.context(
            "NODE_ADDRESSES is required for cross-partition transactions with remote participants",
        )?;
        let addr = node_addresses
            .address_for(participant)
            .with_context(|| format!("Missing NODE_ADDRESSES entry for {participant}"))?;
        let client = TwoPhaseCommitGrpcClient::connect(addr).await?;
        Ok(client.commit_prepared(transaction_id).await?.try_into()?)
    }
}

fn is_retryable_prepare_ts_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("2PC Prepare assigned ts=")
}

/// Execute the 2PC protocol for a cross-partition transaction.
pub async fn coordinate_two_phase_commit(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
) -> anyhow::Result<Timestamp> {
    let TransactionClassification::CrossPartition { partitions } =
        classify_transaction(&transaction, partition_map)
    else {
        anyhow::bail!("2PC coordinator called for a single-partition transaction");
    };

    let participant_indexes = participant_write_indexes(&transaction, partition_map, &partitions);
    let txn_id = TwoPhaseTransactionId::new();
    let node_addresses = local_committer.node_addresses();
    let participants: Vec<_> = participant_indexes
        .iter()
        .map(|(participant, write_indexes)| -> anyhow::Result<_> {
            Ok((
                *participant,
                ParticipantTransaction::from_final_transaction(&transaction, write_indexes)?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;

    let mut last_retryable_error = None;
    for attempt in 0..MAX_PREPARE_TS_RETRIES {
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
                    for prepared in prepared_participants.iter().rev().copied() {
                        if let Err(rollback_err) = rollback_participant(
                            local_committer,
                            node_addresses,
                            partition_map,
                            &txn_id,
                            prepared,
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

                    if is_retryable_prepare_ts_error(&err) && attempt + 1 < MAX_PREPARE_TS_RETRIES {
                        last_retryable_error = Some(err);
                        retry_prepare = true;
                        break;
                    }
                    return Err(err);
                },
            }
        }

        if retry_prepare {
            continue;
        }

        let mut commit_results = Vec::new();
        for participant in &prepared_participants {
            let commit_ts = commit_participant(
                local_committer,
                node_addresses,
                partition_map,
                &txn_id,
                *participant,
            )
            .await?;
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
        return Ok(prepare_ts);
    }

    Err(last_retryable_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "2PC coordinator exhausted prepare timestamp retries for txn={}",
            txn_id
        )
    }))
}
