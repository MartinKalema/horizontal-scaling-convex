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
    commit_delta::CommitDelta,
    nats_distributed_log::DeltaEnvelope,
    two_phase::TwoPhaseRedoEntry,
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
    ClusterGenesis {
        manifest: ClusterGenesisManifest,
        checkpoint_base64: String,
    },
}

impl RaftStateMachineEntry {
    pub fn commit_delta(delta: &CommitDelta, source_raft_node_id: u64) -> anyhow::Result<Self> {
        Ok(Self::CommitDelta {
            envelope: DeltaEnvelope::from_delta(delta, "", Some(source_raft_node_id))?,
        })
    }

    pub fn two_phase_prepare(redo: TwoPhaseRedoEntry) -> Self {
        Self::TwoPhasePrepare { redo }
    }

    pub fn cluster_genesis(payload: &CanonicalGenesisPayload) -> Self {
        Self::ClusterGenesis {
            manifest: payload.manifest.clone(),
            checkpoint_base64: base64::encode(&payload.checkpoint_bytes),
        }
    }

    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).context("Failed to serialize Raft state-machine entry")
    }

    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        match serde_json::from_slice(data) {
            Ok(entry) => Ok(entry),
            Err(entry_err) => {
                // Backward compatibility: older Raft log entries were raw
                // DeltaEnvelope JSON values without the typed wrapper.
                let envelope: DeltaEnvelope = serde_json::from_slice(data).with_context(|| {
                    format!(
                        "Failed to deserialize typed Raft entry ({entry_err}) or legacy delta \
                         envelope"
                    )
                })?;
                Ok(Self::CommitDelta { envelope })
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use common::types::Timestamp;

    use super::*;
    use crate::write_log::WriteSource;

    #[test]
    fn commit_delta_entry_roundtrips() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::must(7),
            document_writes: Default::default(),
            document_updates: Vec::new(),
            index_writes: Default::default(),
            write_source: WriteSource::new("raft-entry-test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: None,
        };

        let bytes = RaftStateMachineEntry::commit_delta(&delta, 3)?.to_bytes()?;
        match RaftStateMachineEntry::from_bytes(&bytes)? {
            RaftStateMachineEntry::CommitDelta { envelope } => {
                assert_eq!(envelope.source_raft_node_id(), Some(3));
                assert_eq!(envelope.to_delta()?.ts, delta.ts);
            },
            RaftStateMachineEntry::TwoPhasePrepare { .. } => {
                panic!("expected commit delta entry")
            },
            RaftStateMachineEntry::ClusterGenesis { .. } => {
                panic!("expected commit delta entry")
            },
        }
        Ok(())
    }

    #[test]
    fn legacy_delta_envelope_still_decodes_as_commit_delta() -> anyhow::Result<()> {
        let delta = CommitDelta {
            ts: Timestamp::must(8),
            document_writes: Default::default(),
            document_updates: Vec::new(),
            index_writes: Default::default(),
            write_source: WriteSource::new("legacy-raft-entry-test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: None,
        };
        let legacy = serde_json::to_vec(&DeltaEnvelope::from_delta(&delta, "", Some(4))?)?;

        match RaftStateMachineEntry::from_bytes(&legacy)? {
            RaftStateMachineEntry::CommitDelta { envelope } => {
                assert_eq!(envelope.source_raft_node_id(), Some(4));
                assert_eq!(envelope.to_delta()?.ts, delta.ts);
            },
            RaftStateMachineEntry::TwoPhasePrepare { .. } => {
                panic!("expected commit delta entry")
            },
            RaftStateMachineEntry::ClusterGenesis { .. } => {
                panic!("expected commit delta entry")
            },
        }
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
}
