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

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use anyhow::{
    anyhow,
    Context,
};
use common::{
    bootstrap_model::{
        index::INDEX_TABLE,
        tables::TABLES_TABLE,
    },
    document::DocumentUpdateWithPrevTs,
    knobs::ENABLE_PARALLEL_2PC_EARLY_ACK,
    runtime::tokio_spawn,
    types::Timestamp,
};
use futures::{
    stream::FuturesUnordered,
    StreamExt,
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
        prepare_timestamp_too_low,
        transaction_has_user_catalog_writes,
        user_catalog_write_target,
        NodeAddresses,
        ParticipantTransaction,
        StagingRecoveryResult,
        TwoPhaseDecision,
        TwoPhaseParticipantIntent,
        TwoPhaseRedoEntry,
        TwoPhaseTransactionId,
    },
    write_log::WriteSource,
};

const MAX_PREPARE_TS_RETRIES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TwoPhaseCommitMode {
    DurableDecision,
    ParallelEarlyAck,
}

impl TwoPhaseCommitMode {
    fn from_knob() -> Self {
        if *ENABLE_PARALLEL_2PC_EARLY_ACK {
            Self::ParallelEarlyAck
        } else {
            Self::DurableDecision
        }
    }
}

struct PrepareParticipantFailure {
    participant: PartitionId,
    err: anyhow::Error,
}

struct PrepareParticipantsOutcome {
    prepared: Vec<PartitionId>,
    failures: Vec<PrepareParticipantFailure>,
}

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
) -> anyhow::Result<TransactionClassification> {
    let partitions = participant_write_indexes(transaction, partition_map, write_source)?;
    let mut prepare_participants: BTreeSet<_> = partitions.keys().copied().collect();
    prepare_participants.extend(participant_read_owners(
        transaction,
        partition_map,
        write_source,
    )?);

    Ok(if partitions.is_empty() {
        TransactionClassification::SinglePartition
    } else if prepare_participants.len() == 1 {
        let owner = *prepare_participants
            .iter()
            .next()
            .expect("single-partition classification should have one owner");
        if owner == partition_map.local_partition() {
            TransactionClassification::SinglePartition
        } else {
            TransactionClassification::RemoteSinglePartition { owner }
        }
    } else {
        TransactionClassification::CrossPartition { partitions }
    })
}

pub(crate) fn participant_write_indexes(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> anyhow::Result<BTreeMap<PartitionId, Vec<usize>>> {
    let mut participant_indexes: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();

    for (index, write) in transaction.writes.coalesced_writes().enumerate() {
        for participant in
            routed_partitions_for_write(transaction, write, partition_map, write_source)?
        {
            participant_indexes
                .entry(participant)
                .or_default()
                .push(index);
        }
    }

    participant_indexes.retain(|_, indexes| !indexes.is_empty());
    Ok(participant_indexes)
}

pub(crate) fn participant_prepare_indexes(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> anyhow::Result<BTreeMap<PartitionId, Vec<usize>>> {
    let mut participant_indexes =
        participant_write_indexes(transaction, partition_map, write_source)?;

    for participant in participant_read_owners(transaction, partition_map, write_source)? {
        participant_indexes.entry(participant).or_default();
    }

    Ok(participant_indexes)
}

fn participant_read_owners(
    transaction: &FinalTransaction,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> anyhow::Result<BTreeSet<PartitionId>> {
    let mut participants = BTreeSet::new();
    let metadata_authority = (partition_map.enforces_catalog_coherence()
        && transaction_has_user_catalog_writes(transaction)?)
    .then(|| partition_map.partition_for_table(&TABLES_TABLE));
    let mut visit_tablet = |tablet_id| {
        let Ok(table_name) = transaction.table_mapping.tablet_name(tablet_id) else {
            return;
        };
        if metadata_authority.is_some()
            && (&table_name == &*TABLES_TABLE || &table_name == &*INDEX_TABLE)
        {
            if let Some(metadata_authority) = metadata_authority {
                participants.insert(metadata_authority);
            }
            return;
        }
        if let Some(partition) =
            routed_partition_for_table(&table_name, partition_map, write_source)
        {
            participants.insert(partition);
        }
    };

    for (index_name, _) in transaction.reads.read_set().iter_indexed() {
        visit_tablet(*index_name.table());
    }
    for (index_name, _) in transaction.reads.read_set().iter_search() {
        visit_tablet(*index_name.table());
    }

    Ok(participants)
}

fn routed_partitions_for_write(
    transaction: &FinalTransaction,
    write: &DocumentUpdateWithPrevTs,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> anyhow::Result<BTreeSet<PartitionId>> {
    let mut participants = BTreeSet::new();
    let Some(table_name) = transaction
        .table_mapping
        .tablet_name(write.id.tablet_id)
        .ok()
    else {
        return Ok(participants);
    };
    if let Some(owner) = routed_partition_for_catalog_write(transaction, write, partition_map)? {
        if partition_map.enforces_catalog_coherence() {
            participants.insert(partition_map.partition_for_table(&TABLES_TABLE));
        }
        participants.insert(owner);
        return Ok(participants);
    }
    if let Some(partition) = routed_partition_for_table(&table_name, partition_map, write_source) {
        participants.insert(partition);
    }
    Ok(participants)
}

fn routed_partition_for_catalog_write(
    transaction: &FinalTransaction,
    write: &DocumentUpdateWithPrevTs,
    partition_map: &PartitionMap,
) -> anyhow::Result<Option<PartitionId>> {
    Ok(user_catalog_write_target(transaction, write)?
        .map(|metadata| partition_map.partition_for_table(&metadata.table_name)))
}

#[cfg(any(test, feature = "testing"))]
pub fn routed_partition_for_catalog_write_for_test(
    transaction: &FinalTransaction,
    write: &DocumentUpdateWithPrevTs,
    partition_map: &PartitionMap,
) -> anyhow::Result<Option<PartitionId>> {
    routed_partition_for_catalog_write(transaction, write, partition_map)
}

async fn prepare_participant(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    stateful_participants: &[PartitionId],
    participant: PartitionId,
    participant_tx: ParticipantTransaction,
    write_source: &WriteSource,
    prepare_ts: Timestamp,
) -> anyhow::Result<bool> {
    let stages_writes = !participant_tx.writes.is_empty();
    if participant == partition_map.local_partition() {
        if stages_writes {
            let result = local_committer
                .prepare_remote(
                    transaction_id.clone(),
                    participant_tx,
                    write_source.clone(),
                    prepare_ts,
                    stateful_participants.to_vec(),
                )
                .await?;
            verify_prepare_result(participant, prepare_ts, result)?;
            return Ok(true);
        }
        local_committer
            .validate_remote_reads(
                transaction_id.clone(),
                participant_tx,
                write_source.clone(),
                prepare_ts,
            )
            .await?;
        return Ok(false);
    } else {
        let node_addresses = node_addresses.with_context(|| {
            format!(
                "NODE_ADDRESSES is required to route this transaction to the authoritative owner \
                 {participant}. Route this mutation to the correct partition owner."
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
                    if stages_writes {
                        match client
                            .prepare(
                                transaction_id,
                                participant_tx.clone(),
                                write_source.clone(),
                                prepare_ts,
                                partition_map.placement_version(),
                                stateful_participants,
                            )
                            .await
                        {
                            Ok(result) => {
                                verify_prepare_result(participant, prepare_ts, result)?;
                                return Ok(true);
                            },
                            Err(err) if should_try_next_participant_address(&err) => {
                                tracing::warn!(
                                    "2PC Coordinator: prepare to participant {} via {} failed, \
                                     trying next address: {err:#}",
                                    participant,
                                    addr,
                                );
                                last_err = Some(err);
                            },
                            Err(err) => return Err(err),
                        }
                    } else {
                        match client
                            .validate_reads(
                                transaction_id,
                                participant_tx.clone(),
                                write_source.clone(),
                                prepare_ts,
                                partition_map.placement_version(),
                            )
                            .await
                        {
                            Ok(()) => return Ok(false),
                            Err(err) if should_try_next_participant_address(&err) => {
                                tracing::warn!(
                                    "2PC Coordinator: read validation on participant {} via {} \
                                     failed, trying next address: {err:#}",
                                    participant,
                                    addr,
                                );
                                last_err = Some(err);
                            },
                            Err(err) => return Err(err),
                        }
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
    }
}

async fn prepare_participants(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    stateful_participants: &[PartitionId],
    participants: &[(PartitionId, ParticipantTransaction)],
    write_source: &WriteSource,
    prepare_ts: Timestamp,
) -> PrepareParticipantsOutcome {
    let mut prepares = FuturesUnordered::new();
    for (participant, participant_tx) in participants {
        prepares.push(async move {
            let result = prepare_participant(
                local_committer,
                node_addresses,
                partition_map,
                transaction_id,
                stateful_participants,
                *participant,
                participant_tx.clone(),
                write_source,
                prepare_ts,
            )
            .await;
            (*participant, result)
        });
    }

    let mut prepared = Vec::new();
    let mut failures = Vec::new();
    while let Some((participant, result)) = prepares.next().await {
        match result {
            Ok(true) => prepared.push(participant),
            Ok(false) => {},
            Err(err) => failures.push(PrepareParticipantFailure { participant, err }),
        }
    }
    PrepareParticipantsOutcome { prepared, failures }
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

async fn rollback_prepared_participants(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participants: &[PartitionId],
) {
    for prepared in participants.iter().rev().copied() {
        match rollback_participant(
            local_committer,
            node_addresses,
            partition_map,
            transaction_id,
            prepared,
        )
        .await
        {
            Ok(()) => {
                if let Err(mark_err) =
                    mark_participant_resolved(local_committer, transaction_id, prepared).await
                {
                    tracing::warn!(
                        "2PC Coordinator: rollback resolved on {} for txn={} but failed to mark \
                         participant resolved: {mark_err:#}",
                        prepared,
                        transaction_id,
                    );
                }
            },
            Err(rollback_err) if is_unknown_rollback_error(&rollback_err) => {
                if let Err(mark_err) =
                    mark_participant_resolved(local_committer, transaction_id, prepared).await
                {
                    tracing::warn!(
                        "2PC Coordinator: rollback on {} for txn={} was already clean but failed \
                         to mark participant resolved: {mark_err:#}",
                        prepared,
                        transaction_id,
                    );
                }
            },
            Err(rollback_err) => {
                tracing::error!(
                    "2PC Coordinator: rollback failed on {} for txn={}: {rollback_err:#}",
                    prepared,
                    transaction_id,
                );
            },
        }
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

async fn commit_participants(
    local_committer: &CommitterClient,
    node_addresses: Option<&NodeAddresses>,
    partition_map: &PartitionMap,
    transaction_id: &TwoPhaseTransactionId,
    participants: &[PartitionId],
) -> anyhow::Result<Vec<(PartitionId, Timestamp)>> {
    let mut commits = FuturesUnordered::new();
    for participant in participants {
        commits.push(async move {
            let commit_ts = commit_participant(
                local_committer,
                node_addresses,
                partition_map,
                transaction_id,
                *participant,
            )
            .await?;
            mark_participant_resolved(local_committer, transaction_id, *participant)
                .await
                .with_context(|| {
                    format!(
                        "2PC Coordinator: participant {participant} committed \
                         txn={transaction_id} but resolution acknowledgement failed"
                    )
                })?;
            Ok((*participant, commit_ts))
        });
    }

    let mut commit_results = Vec::new();
    let mut first_err = None;
    while let Some(result) = commits.next().await {
        match result {
            Ok(result) => commit_results.push(result),
            Err(err) => {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            },
        }
    }
    if let Some(err) = first_err {
        Err(err)
    } else {
        Ok(commit_results)
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

fn participant_intents_for_staging(
    transaction_id: &TwoPhaseTransactionId,
    prepare_ts: Timestamp,
    all_participants: &[PartitionId],
    participants: &[(PartitionId, ParticipantTransaction)],
    write_source: &WriteSource,
) -> anyhow::Result<Vec<TwoPhaseParticipantIntent>> {
    let mut intents = participants
        .iter()
        .map(|(participant, transaction)| {
            let redo = TwoPhaseRedoEntry::new_with_participants(
                transaction_id,
                prepare_ts,
                *participant,
                all_participants,
                transaction.clone(),
                write_source,
            )?;
            Ok(TwoPhaseParticipantIntent::from_redo_entry(&redo))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    intents.sort_by_key(|intent| intent.partition_id);
    Ok(intents)
}

async fn write_complete_staging_decision(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
    prepare_ts: Timestamp,
    all_participants: &[PartitionId],
    participants: &[(PartitionId, ParticipantTransaction)],
    write_source: &WriteSource,
) -> anyhow::Result<TwoPhaseDecision> {
    let decision = TwoPhaseDecision::staging(
        u64::from(prepare_ts),
        all_participants
            .iter()
            .map(|participant| participant.0)
            .collect(),
        participant_intents_for_staging(
            transaction_id,
            prepare_ts,
            all_participants,
            participants,
            write_source,
        )?,
    );
    anyhow::ensure!(
        decision.all_participants_staged(),
        "2PC Coordinator: constructed incomplete staging decision for txn={transaction_id}",
    );
    write_decision_record(local_committer, transaction_id, &decision)
        .await
        .with_context(|| {
            format!("2PC Coordinator: failed to write staging decision for txn={transaction_id}")
        })?;
    Ok(decision)
}

async fn recover_complete_staging_decision(
    local_committer: &CommitterClient,
    transaction_id: &TwoPhaseTransactionId,
) -> anyhow::Result<TwoPhaseDecision> {
    let decision_log = local_committer.two_phase_decision_log();
    let recovered = decision_log
        .recover_staging_decision(transaction_id)
        .await
        .with_context(|| {
            format!(
                "2PC Coordinator: failed to recover complete staging decision for \
                 txn={transaction_id}"
            )
        })?;
    match recovered {
        StagingRecoveryResult::Committed(decision)
        | StagingRecoveryResult::AlreadyFinal(decision @ TwoPhaseDecision::Committed { .. }) => {
            Ok(decision)
        },
        StagingRecoveryResult::Pending(decision) => {
            anyhow::bail!(
                "2PC Coordinator: staging decision for txn={} is still pending: {:?}",
                transaction_id,
                decision,
            )
        },
        StagingRecoveryResult::AlreadyFinal(decision) => {
            anyhow::bail!(
                "2PC Coordinator: staging decision for txn={} recovered to non-committed final \
                 state: {:?}",
                transaction_id,
                decision,
            )
        },
        StagingRecoveryResult::Missing => {
            anyhow::bail!(
                "2PC Coordinator: staging decision for txn={} disappeared before recovery",
                transaction_id,
            )
        },
    }
}

fn spawn_parallel_commit_cleanup(
    local_committer: CommitterClient,
    node_addresses: Option<NodeAddresses>,
    partition_map: PartitionMap,
    transaction_id: TwoPhaseTransactionId,
    prepared_participants: Vec<PartitionId>,
    prepare_ts: Timestamp,
) {
    tokio_spawn("two_phase_parallel_commit_cleanup", async move {
        match commit_participants(
            &local_committer,
            node_addresses.as_ref(),
            &partition_map,
            &transaction_id,
            &prepared_participants,
        )
        .await
        {
            Ok(commit_results) => {
                for (participant, commit_ts) in &commit_results {
                    if *commit_ts != prepare_ts {
                        tracing::error!(
                            "2PC parallel cleanup: participant {} committed txn={} at ts={} but \
                             coordinator assigned ts={}",
                            participant,
                            transaction_id,
                            commit_ts,
                            prepare_ts,
                        );
                    }
                }
            },
            Err(err) => {
                tracing::warn!(
                    "2PC parallel cleanup for txn={} did not finish; watcher will retry from the \
                     durable decision: {err:#}",
                    transaction_id,
                );
            },
        }
    });
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
        tracing::info!(
            "2PC Coordinator: fully resolved decision for txn={} retained for follower recovery",
            transaction_id,
        );
    }
    Ok(())
}

fn required_prepare_ts(err: &anyhow::Error) -> Option<Timestamp> {
    prepare_timestamp_too_low(err).map(|retry| retry.required_ts)
}

fn is_retryable_prepare_ts_error(err: &anyhow::Error) -> bool {
    required_prepare_ts(err).is_some()
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

fn verify_committed_decision(
    transaction_id: &TwoPhaseTransactionId,
    decision: &TwoPhaseDecision,
    expected_ts: Timestamp,
    expected_participants: &[PartitionId],
) -> anyhow::Result<()> {
    let TwoPhaseDecision::Committed {
        commit_ts,
        participants,
        ..
    } = decision
    else {
        anyhow::bail!(
            "2PC Coordinator: expected committed decision for txn={} but found {:?}",
            transaction_id,
            decision,
        );
    };
    anyhow::ensure!(
        *commit_ts == u64::from(expected_ts),
        "2PC Coordinator: recovered commit decision for txn={} at ts={} but expected ts={}",
        transaction_id,
        commit_ts,
        u64::from(expected_ts),
    );
    let mut actual_participants = participants.clone();
    actual_participants.sort_unstable();
    actual_participants.dedup();
    let mut expected_participants: Vec<_> = expected_participants
        .iter()
        .map(|participant| participant.0)
        .collect();
    expected_participants.sort_unstable();
    expected_participants.dedup();
    anyhow::ensure!(
        actual_participants == expected_participants,
        "2PC Coordinator: recovered commit decision for txn={} has participants {:?} but expected \
         {:?}",
        transaction_id,
        actual_participants,
        expected_participants,
    );
    Ok(())
}

/// Execute the 2PC protocol for a transaction that cannot commit on the local
/// fast path.
pub async fn coordinate_two_phase_commit(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
) -> anyhow::Result<CommitOutcome> {
    coordinate_two_phase_commit_with_mode(
        local_committer,
        transaction,
        write_source,
        partition_map,
        TwoPhaseCommitMode::from_knob(),
    )
    .await
}

pub(crate) async fn coordinate_two_phase_commit_with_mode(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
    commit_mode: TwoPhaseCommitMode,
) -> anyhow::Result<CommitOutcome> {
    let timer = metrics::two_phase_coordinator_timer();
    let source_partition = match classify_transaction(&transaction, partition_map, &write_source)? {
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

    let participant_indexes =
        participant_prepare_indexes(&transaction, partition_map, &write_source)?;
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
    let stateful_participants: Vec<_> = participants
        .iter()
        .filter_map(|(participant, transaction)| {
            (!transaction.writes.is_empty()).then_some(*participant)
        })
        .collect();
    let stateful_participant_transactions: Vec<_> = participants
        .iter()
        .filter(|(_, transaction)| !transaction.writes.is_empty())
        .cloned()
        .collect();
    metrics::log_two_phase_participants(participants.len());

    let mut last_retryable_error = None;
    let mut prepare_attempts = 0usize;
    let mut min_prepare_ts = Timestamp::MIN;
    for attempt in 0..MAX_PREPARE_TS_RETRIES {
        let txn_id = TwoPhaseTransactionId::new();
        prepare_attempts = attempt + 1;
        let prepare_ts = local_committer
            .allocate_commit_ts_at_or_after(min_prepare_ts)
            .await?;
        tracing::info!(
            "2PC Coordinator: starting txn={}, participants={:?}, prepare_ts={}, attempt={}",
            txn_id,
            participant_indexes.keys().collect::<Vec<_>>(),
            u64::from(prepare_ts),
            attempt + 1,
        );

        let PrepareParticipantsOutcome {
            prepared: prepared_participants,
            mut failures,
        } = prepare_participants(
            local_committer,
            node_addresses.as_ref(),
            partition_map,
            &txn_id,
            &stateful_participants,
            &participants,
            &write_source,
            prepare_ts,
        )
        .await;
        let mut retry_prepare = false;
        if !failures.is_empty() {
            for failure in &failures {
                tracing::warn!(
                    "2PC Coordinator: prepare failed on {} for txn={}: {:#}",
                    failure.participant,
                    txn_id,
                    failure.err,
                );
            }

            let has_ambiguous_prepare = failures
                .iter()
                .any(|failure| is_ambiguous_prepare_error(&failure.err));
            let participants_to_rollback = if has_ambiguous_prepare {
                stateful_participants.clone()
            } else {
                prepared_participants.clone()
            };
            let all_failures_retryable = failures
                .iter()
                .all(|failure| is_retryable_prepare_ts_error(&failure.err));
            let required_retry_floor = failures
                .iter()
                .filter_map(|failure| required_prepare_ts(&failure.err))
                .max();
            let selected_failure_index = failures
                .iter()
                .position(|failure| !is_retryable_prepare_ts_error(&failure.err))
                .unwrap_or(0);
            let selected_failure = failures.remove(selected_failure_index);
            if !participants_to_rollback.is_empty() {
                let decision = TwoPhaseDecision::rolled_back(
                    selected_failure.err.to_string(),
                    participants_to_rollback.iter().map(|p| p.0).collect(),
                );
                write_decision_record(local_committer, &txn_id, &decision)
                    .await
                    .with_context(|| {
                        format!(
                            "2PC Coordinator: failed to write rollback decision for txn={txn_id}"
                        )
                    })?;
            }
            rollback_prepared_participants(
                local_committer,
                node_addresses.as_ref(),
                partition_map,
                &txn_id,
                &participants_to_rollback,
            )
            .await;

            let retryable_prepare_ts_error = is_retryable_prepare_ts_error(&selected_failure.err)
                && attempt + 1 < MAX_PREPARE_TS_RETRIES
                && all_failures_retryable;
            if retryable_prepare_ts_error {
                min_prepare_ts = required_retry_floor
                    .expect("all retryable timestamp failures must include a required floor");
                metrics::log_two_phase_prepare_retry();
                tracing::info!(
                    "2PC Coordinator: retrying txn={} at or above ts={} after participant {} \
                     rejected ts on attempt {}",
                    txn_id,
                    u64::from(min_prepare_ts),
                    selected_failure.participant,
                    attempt + 1,
                );
                last_retryable_error = Some(selected_failure.err);
                retry_prepare = true;
            } else {
                metrics::log_two_phase_error("prepare");
                metrics::log_two_phase_decision("rollback");
                metrics::log_two_phase_prepare_attempts(prepare_attempts);
                return Err(selected_failure.err);
            }
        }

        if retry_prepare {
            continue;
        }

        match commit_mode {
            TwoPhaseCommitMode::DurableDecision => {
                let decision = TwoPhaseDecision::committed(
                    u64::from(prepare_ts),
                    prepared_participants.iter().map(|p| p.0).collect(),
                );
                write_decision_record(local_committer, &txn_id, &decision)
                    .await
                    .with_context(|| {
                        format!("2PC Coordinator: failed to write commit decision for txn={txn_id}")
                    })?;

                let commit_results = match commit_participants(
                    local_committer,
                    node_addresses.as_ref(),
                    partition_map,
                    &txn_id,
                    &prepared_participants,
                )
                .await
                {
                    Ok(commit_results) => commit_results,
                    Err(err) => {
                        metrics::log_two_phase_error("commit");
                        return Err(err);
                    },
                };

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
                    read_after_write_partitions: prepared_participants.clone(),
                });
            },
            TwoPhaseCommitMode::ParallelEarlyAck => {
                write_complete_staging_decision(
                    local_committer,
                    &txn_id,
                    prepare_ts,
                    &stateful_participants,
                    &stateful_participant_transactions,
                    &write_source,
                )
                .await?;
                let recovered = recover_complete_staging_decision(local_committer, &txn_id).await?;
                verify_committed_decision(&txn_id, &recovered, prepare_ts, &prepared_participants)?;
                spawn_parallel_commit_cleanup(
                    local_committer.clone(),
                    node_addresses.clone(),
                    partition_map.clone(),
                    txn_id.clone(),
                    prepared_participants.clone(),
                    prepare_ts,
                );

                tracing::info!(
                    "2PC Coordinator: parallel-committed txn={} at ts={} with asynchronous \
                     participant cleanup",
                    txn_id,
                    u64::from(prepare_ts),
                );
                metrics::log_two_phase_prepare_attempts(prepare_attempts);
                metrics::log_two_phase_decision("parallel_commit");
                timer.finish();
                return Ok(CommitOutcome {
                    ts: prepare_ts,
                    source_partition,
                    read_after_write_partitions: prepared_participants.clone(),
                });
            },
        }
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
    use std::collections::BTreeMap;

    use common::types::{
        RepeatableTimestamp,
        Timestamp,
    };

    use super::{
        is_ambiguous_prepare_error,
        is_retryable_prepare_ts_error,
        participant_intents_for_staging,
        verify_committed_decision,
    };
    use crate::{
        partition::PartitionId,
        reads::ReadSet,
        two_phase::{
            ParticipantTransaction,
            PrepareTimestampTooLow,
            TwoPhaseDecision,
            TwoPhaseParticipantIntent,
            TwoPhaseRedoEntry,
            TwoPhaseTransactionId,
        },
        write_log::WriteSource,
    };

    fn empty_participant_transaction() -> ParticipantTransaction {
        ParticipantTransaction {
            begin_timestamp: RepeatableTimestamp::MIN,
            tablet_metadata: BTreeMap::new(),
            reads: ReadSet::empty(),
            writes: Vec::new(),
        }
    }

    #[test]
    fn retryable_prepare_ts_error_detects_wrapped_structured_floor() -> anyhow::Result<()> {
        let err = anyhow::Error::new(PrepareTimestampTooLow {
            proposed_ts: Timestamp::try_from(10u64)?,
            required_ts: Timestamp::try_from(20u64)?,
        })
        .context("gRPC Prepare failed");
        assert!(is_retryable_prepare_ts_error(&err));
        assert_eq!(super::required_prepare_ts(&err), Some(20u64.try_into()?));
        Ok(())
    }

    #[test]
    fn retryable_prepare_ts_error_rejects_unrelated_errors() {
        let err = anyhow::anyhow!("gRPC Prepare failed");
        assert!(!is_retryable_prepare_ts_error(&err));
    }

    #[test]
    fn retryable_prepare_ts_error_rejects_matching_text_without_structured_floor() {
        let err =
            anyhow::anyhow!("2PC Prepare assigned ts=10 but this participant requires ts>=20");
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
    fn ambiguous_prepare_error_detects_rpc_timeout() {
        let err = anyhow::anyhow!("gRPC Prepare failed: request timed out after 10s");
        assert!(is_ambiguous_prepare_error(&err));
    }

    #[test]
    fn ambiguous_prepare_error_rejects_application_failure() {
        let err = anyhow::anyhow!(
            "gRPC Prepare failed: status: Internal, message: \"forced prepare failure\""
        );
        assert!(!is_ambiguous_prepare_error(&err));
    }

    #[test]
    fn staging_intents_match_participant_redo_records() -> anyhow::Result<()> {
        let transaction_id = TwoPhaseTransactionId::new();
        let prepare_ts = Timestamp::try_from(12345u64)?;
        let write_source = WriteSource::unknown();
        let all_participants = vec![PartitionId(0), PartitionId(1)];
        let participant_tx = empty_participant_transaction();
        let participants = vec![
            (PartitionId(1), participant_tx.clone()),
            (PartitionId(0), participant_tx.clone()),
        ];

        let intents = participant_intents_for_staging(
            &transaction_id,
            prepare_ts,
            &all_participants,
            &participants,
            &write_source,
        )?;

        let mut expected_intents = participants
            .iter()
            .map(|(participant, transaction)| {
                let redo = TwoPhaseRedoEntry::new_with_participants(
                    &transaction_id,
                    prepare_ts,
                    *participant,
                    &all_participants,
                    transaction.clone(),
                    &write_source,
                )?;
                Ok(TwoPhaseParticipantIntent::from_redo_entry(&redo))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        expected_intents.sort_by_key(|intent| intent.partition_id);

        assert_eq!(intents, expected_intents);
        Ok(())
    }

    #[test]
    fn committed_decision_verifier_rejects_wrong_timestamp_or_participants() -> anyhow::Result<()> {
        let transaction_id = TwoPhaseTransactionId::new();
        let prepare_ts = Timestamp::try_from(12345u64)?;
        let committed = TwoPhaseDecision::committed(
            u64::from(prepare_ts),
            vec![PartitionId(1).0, PartitionId(0).0],
        );

        verify_committed_decision(
            &transaction_id,
            &committed,
            prepare_ts,
            &[PartitionId(0), PartitionId(1)],
        )?;

        let wrong_ts = TwoPhaseDecision::committed(u64::from(prepare_ts) + 1, vec![0, 1]);
        assert!(verify_committed_decision(
            &transaction_id,
            &wrong_ts,
            prepare_ts,
            &[PartitionId(0), PartitionId(1)],
        )
        .is_err());

        let wrong_participants = TwoPhaseDecision::committed(u64::from(prepare_ts), vec![0]);
        assert!(verify_committed_decision(
            &transaction_id,
            &wrong_participants,
            prepare_ts,
            &[PartitionId(0), PartitionId(1)],
        )
        .is_err());
        Ok(())
    }
}
