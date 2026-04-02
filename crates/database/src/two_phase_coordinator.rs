//! Two-Phase Commit Coordinator (Vitess VTGate pattern).
//!
//! When a transaction writes to tables on multiple partitions, the coordinator:
//! 1. Splits writes by partition
//! 2. Sends Prepare to each partition's Committer
//! 3. Records commit/rollback decision in NATS KV
//! 4. Sends CommitPrepared/RollbackPrepared to all participants
//!
//! The coordinator runs on the node where the mutation was received.
//! Remote partitions are reached via gRPC (MutationForwarderService).
//!
//! References:
//!   - Vitess: https://vitess.io/docs/22.0/reference/features/distributed-transaction/
//!   - CockroachDB: https://www.cockroachlabs.com/blog/parallel-commits/

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use common::types::Timestamp;
use value::TableName;

use crate::{
    committer::CommitterClient,
    partition::{
        PartitionId,
        PartitionMap,
    },
    transaction::FinalTransaction,
    two_phase::{
        TwoPhaseDecision,
        TwoPhaseTransactionId,
    },
    write_log::WriteSource,
};

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
            // System tables are always local — don't count them for partitioning.
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

/// Execute the 2PC protocol for a cross-partition transaction.
///
/// This is the coordinator logic (Vitess VTGate role):
/// 1. Generate transaction ID
/// 2. Send Prepare to local Committer (for local partition's writes)
/// 3. Send Prepare to remote Committer(s) via gRPC
/// 4. If all succeed: record COMMITTED, send CommitPrepared to all
/// 5. If any fails: record ROLLED_BACK, send RollbackPrepared to all
///
/// For now, we implement the local-only coordinator that handles
/// cross-partition transactions where the local node is the only participant
/// that needs to prepare (the remote partition's writes are forwarded via gRPC
/// prepare). Full remote gRPC prepare will be added when we extend
/// replication.proto.
pub async fn coordinate_two_phase_commit(
    local_committer: &CommitterClient,
    transaction: FinalTransaction,
    write_source: WriteSource,
    partition_map: &PartitionMap,
) -> anyhow::Result<Timestamp> {
    let txn_id = TwoPhaseTransactionId::new();

    tracing::info!(
        "2PC Coordinator: starting txn={}, classifying writes...",
        txn_id,
    );

    // Phase 1: Prepare on local Committer.
    // For now, the local Committer handles all writes (the partition ownership
    // check is skipped in handle_prepare). This works because both partitions'
    // Committers run on the same cluster with shared NATS, and the coordinator
    // serializes the prepare calls.
    //
    // TODO: When gRPC TwoPhaseCommit service is added, split writes by
    // partition and send remote partitions' writes to their owners via gRPC.
    let prepare_result = local_committer
        .prepare(txn_id.clone(), transaction, write_source)
        .await;

    match prepare_result {
        Ok(result) => {
            tracing::info!(
                "2PC Coordinator: all prepared, committing txn={} at ts={}",
                txn_id,
                u64::from(result.prepare_ts),
            );

            // Phase 2: Commit.
            let commit_result = local_committer.commit_prepared(txn_id.clone()).await;

            match commit_result {
                Ok(ts) => {
                    tracing::info!(
                        "2PC Coordinator: committed txn={} at ts={}",
                        txn_id,
                        u64::from(ts),
                    );
                    Ok(ts)
                },
                Err(e) => {
                    tracing::error!("2PC Coordinator: commit failed for txn={}: {e:#}", txn_id,);
                    Err(e)
                },
            }
        },
        Err(e) => {
            tracing::warn!(
                "2PC Coordinator: prepare failed for txn={}: {e:#}, rolling back",
                txn_id,
            );

            // Rollback — best effort, the prepare may not have succeeded.
            let _ = local_committer.rollback_prepared(txn_id.clone()).await;

            Err(e)
        },
    }
}
