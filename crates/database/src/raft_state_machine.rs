//! Serializable entries applied by the Raft state machine.
//!
//! Raft carries more than one kind of durable state-machine operation. Normal
//! commits install [`CommitDelta`]s, while 2PC prepare entries install durable
//! prepared redo records before a participant can acknowledge `Prepared`.

use anyhow::Context;
use serde::{
    Deserialize,
    Serialize,
};

use crate::{
    cluster_genesis::{
        CanonicalGenesisPayload,
        ClusterGenesisManifest,
    },
    commit_delta::{
        CommitDelta,
        RaftNatsOutboxOrigin,
    },
    nats_distributed_log::DeltaEnvelope,
    two_phase::{
        TwoPhaseRedoEntry,
        TwoPhaseTransactionId,
    },
};

/// Versioned Raft state-machine entry.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum RaftStateMachineEntry {
    CommitDelta {
        envelope: DeltaEnvelope,
    },
    TwoPhasePrepare {
        redo: TwoPhaseRedoEntry,
    },
    TwoPhaseRollback {
        transaction_id: TwoPhaseTransactionId,
    },
    ClusterGenesis {
        manifest: ClusterGenesisManifest,
        checkpoint_base64: String,
    },
}

impl RaftStateMachineEntry {
    pub fn commit_delta(
        delta: &CommitDelta,
        source_raft_node_id: u64,
        outbox_origin: Option<&RaftNatsOutboxOrigin>,
    ) -> anyhow::Result<Self> {
        let envelope = match outbox_origin {
            Some(origin) => {
                anyhow::ensure!(
                    origin.source_raft_node_id == source_raft_node_id,
                    "Raft proposer ID does not match durable NATS outbox origin",
                );
                DeltaEnvelope::from_raft_nats_outbox(delta, origin)?
            },
            None => DeltaEnvelope::from_delta(delta, "", Some(source_raft_node_id))?,
        };
        Ok(Self::CommitDelta { envelope })
    }

    pub fn two_phase_prepare(redo: TwoPhaseRedoEntry) -> Self {
        Self::TwoPhasePrepare { redo }
    }

    pub fn two_phase_rollback(transaction_id: TwoPhaseTransactionId) -> Self {
        Self::TwoPhaseRollback { transaction_id }
    }

    pub fn cluster_genesis(payload: &CanonicalGenesisPayload) -> Self {
        Self::ClusterGenesis {
            manifest: payload.manifest.clone(),
            checkpoint_base64: base64::encode(&payload.checkpoint_bytes),
        }
    }

    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        if let Self::CommitDelta { envelope } = self {
            envelope.validate_raft_transport()?;
        }
        serde_json::to_vec(self).context("Failed to serialize Raft state-machine entry")
    }

    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        let entry: Self = serde_json::from_slice(data)
            .context("Failed to deserialize typed Raft state-machine entry")?;
        if let Self::CommitDelta { envelope } = &entry {
            envelope.validate_raft_transport()?;
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use common::types::Timestamp;

    use super::*;
    use crate::write_log::WriteSource;

    fn test_origin() -> anyhow::Result<RaftNatsOutboxOrigin> {
        RaftNatsOutboxOrigin::new("node-p0a".to_string(), 3, crate::partition::PartitionId(0))
    }

    #[test]
    fn commit_delta_entry_roundtrips() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::must(7),
            document_writes: Default::default(),
            document_updates: Vec::new(),
            index_writes: Default::default(),
            write_source: WriteSource::new("raft-entry-test"),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(crate::partition::PartitionId(0)),
        };

        let origin = test_origin()?;
        let bytes = RaftStateMachineEntry::commit_delta(&delta, 3, Some(&origin))?.to_bytes()?;
        match RaftStateMachineEntry::from_bytes(&bytes)? {
            RaftStateMachineEntry::CommitDelta { envelope } => {
                assert_eq!(envelope.source_raft_node_id(), Some(3));
                assert_eq!(envelope.to_delta()?.ts, delta.ts);
            },
            RaftStateMachineEntry::TwoPhasePrepare { .. } => {
                panic!("expected commit delta entry")
            },
            RaftStateMachineEntry::TwoPhaseRollback { .. } => {
                panic!("expected commit delta entry")
            },
            RaftStateMachineEntry::ClusterGenesis { .. } => {
                panic!("expected commit delta entry")
            },
        }
        Ok(())
    }

    #[test]
    fn untyped_delta_envelope_fails_closed() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::must(8),
            document_writes: Default::default(),
            document_updates: Vec::new(),
            index_writes: Default::default(),
            write_source: WriteSource::new("untyped-raft-entry-test"),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(crate::partition::PartitionId(0)),
        };
        let origin =
            RaftNatsOutboxOrigin::new("node-p0b".to_string(), 4, crate::partition::PartitionId(0))?;
        let untyped = serde_json::to_vec(&DeltaEnvelope::from_raft_nats_outbox(&delta, &origin)?)?;
        let result = RaftStateMachineEntry::from_bytes(&untyped);
        assert!(
            result.is_err(),
            "raw DeltaEnvelope must not enter Raft apply"
        );
        let error = result.err().expect("raw envelope must fail");
        assert!(error.to_string().contains("typed Raft state-machine entry"),);
        Ok(())
    }

    #[test]
    fn raft_data_entry_requires_non_null_origin_fields() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::must(8),
            document_writes: Default::default(),
            document_updates: Vec::new(),
            index_writes: Default::default(),
            write_source: WriteSource::new("raft-origin-fields-test"),
            write_bytes: 0,
            tablet_id_to_table_identity: Default::default(),
            source_partition: Some(crate::partition::PartitionId(1)),
        };
        let origin =
            RaftNatsOutboxOrigin::new("node-p1a".to_string(), 4, crate::partition::PartitionId(1))?;
        let entry = RaftStateMachineEntry::commit_delta(&delta, 4, Some(&origin))?;
        let mut value = serde_json::to_value(entry)?;
        value["payload"]["envelope"]["document_updates_proto"] = serde_json::json!([[]]);
        value["payload"]["envelope"]["source_raft_node_id"] = serde_json::Value::Null;

        let bytes = serde_json::to_vec(&value)?;
        let error = RaftStateMachineEntry::from_bytes(&bytes)
            .err()
            .expect("Raft data entry with null proposer must fail closed");
        assert!(error.to_string().contains("source_raft_node_id"));

        value["payload"]["envelope"]["source_raft_node_id"] = serde_json::json!(4);
        value["payload"]["envelope"]["source_partition"] = serde_json::Value::Null;
        let bytes = serde_json::to_vec(&value)?;
        let error = RaftStateMachineEntry::from_bytes(&bytes)
            .err()
            .expect("Raft data entry with null partition must fail closed");
        assert!(error.to_string().contains("source_partition"));
        Ok(())
    }

    #[test]
    fn cluster_genesis_entry_roundtrips() -> anyhow::Result<()> {
        let payload =
            crate::cluster_genesis::canonicalize_checkpoint(crate::checkpoint::CheckpointData {
                timestamp: Timestamp::must(9),
                documents: Vec::new(),
                globals: Default::default(),
            })?;
        let bytes = RaftStateMachineEntry::cluster_genesis(&payload).to_bytes()?;
        match RaftStateMachineEntry::from_bytes(&bytes)? {
            RaftStateMachineEntry::ClusterGenesis {
                manifest,
                checkpoint_base64,
            } => {
                assert_eq!(manifest, payload.manifest);
                assert_eq!(base64::decode(checkpoint_base64)?, payload.checkpoint_bytes);
            },
            _ => panic!("expected cluster genesis entry"),
        }
        Ok(())
    }

    #[test]
    fn two_phase_rollback_entry_roundtrips() -> anyhow::Result<()> {
        let transaction_id = TwoPhaseTransactionId::new();
        let bytes = RaftStateMachineEntry::two_phase_rollback(transaction_id.clone()).to_bytes()?;

        match RaftStateMachineEntry::from_bytes(&bytes)? {
            RaftStateMachineEntry::TwoPhaseRollback {
                transaction_id: decoded,
            } => assert_eq!(decoded, transaction_id),
            _ => panic!("expected 2PC rollback entry"),
        }
        Ok(())
    }
}
