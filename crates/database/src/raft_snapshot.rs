//! Versioned payload and durable generation binding for Raft snapshots.

use anyhow::Context;
#[cfg(test)]
use common::types::Timestamp;
use common::{
    persistence::{
        PersistenceGlobalWrite,
        PersistenceReader,
    },
    sha256::Sha256,
};
use prost::Message;

use crate::{
    checkpoint::CheckpointData,
    snapshot_checkpointer::{
        checkpoint_from_bytes,
        checkpoint_to_bytes,
    },
};

pub const RAFT_STATE_MACHINE_INDEX_KEY: &str = "raft_state_machine_index";
const RAFT_SNAPSHOT_FORMAT_VERSION: u32 = 1;

#[derive(Debug)]
pub struct RaftSnapshotTemporarilyUnavailable;

impl std::fmt::Display for RaftSnapshotTemporarilyUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "Raft snapshot generation is temporarily unavailable while commits are in flight",
        )
    }
}

impl std::error::Error for RaftSnapshotTemporarilyUnavailable {}

#[derive(Clone)]
pub struct DecodedRaftSnapshot {
    pub raft_index: u64,
    pub raft_term: u64,
    pub state_machine_index: u64,
    pub checkpoint: CheckpointData,
}

#[derive(Clone, PartialEq, Message)]
struct RaftSnapshotEnvelope {
    #[prost(uint32, tag = "1")]
    format_version: u32,
    #[prost(uint64, tag = "2")]
    raft_index: u64,
    #[prost(uint64, tag = "3")]
    raft_term: u64,
    #[prost(uint64, tag = "4")]
    state_machine_index: u64,
    #[prost(uint64, tag = "5")]
    checkpoint_timestamp: u64,
    #[prost(bytes, tag = "6")]
    checkpoint_bytes: Vec<u8>,
    #[prost(string, tag = "7")]
    checkpoint_sha256: String,
}

pub fn state_machine_index_persistence_write(index: u64) -> PersistenceGlobalWrite {
    PersistenceGlobalWrite::new(RAFT_STATE_MACHINE_INDEX_KEY, index.into())
}

pub async fn load_state_machine_index(reader: &dyn PersistenceReader) -> anyhow::Result<u64> {
    let Some(value) = reader
        .get_persistence_global_raw(RAFT_STATE_MACHINE_INDEX_KEY)
        .await?
    else {
        return Ok(0);
    };
    value.as_u64().with_context(|| {
        format!(
            "Invalid durable Raft state-machine index value for {RAFT_STATE_MACHINE_INDEX_KEY}: \
             {value}"
        )
    })
}

pub fn encode_snapshot(
    raft_index: u64,
    raft_term: u64,
    state_machine_index: u64,
    checkpoint: &CheckpointData,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        state_machine_index <= raft_index,
        "Raft snapshot index {raft_index} cannot precede state-machine index {state_machine_index}",
    );
    let checkpoint_bytes = checkpoint_to_bytes(checkpoint)?;
    let checkpoint_sha256 = hex::encode(Sha256::hash(&checkpoint_bytes));
    Ok(RaftSnapshotEnvelope {
        format_version: RAFT_SNAPSHOT_FORMAT_VERSION,
        raft_index,
        raft_term,
        state_machine_index,
        checkpoint_timestamp: u64::from(checkpoint.timestamp),
        checkpoint_bytes,
        checkpoint_sha256,
    }
    .encode_to_vec())
}

pub fn decode_snapshot(
    bytes: &[u8],
    expected_raft_index: u64,
    expected_raft_term: u64,
) -> anyhow::Result<DecodedRaftSnapshot> {
    let envelope = RaftSnapshotEnvelope::decode(bytes)
        .context("Failed to decode versioned Raft snapshot envelope")?;
    anyhow::ensure!(
        envelope.format_version == RAFT_SNAPSHOT_FORMAT_VERSION,
        "Unsupported Raft snapshot format version {}",
        envelope.format_version,
    );
    anyhow::ensure!(
        envelope.raft_index == expected_raft_index,
        "Raft snapshot payload index {} does not match metadata index {}",
        envelope.raft_index,
        expected_raft_index,
    );
    anyhow::ensure!(
        envelope.raft_term == expected_raft_term,
        "Raft snapshot payload term {} does not match metadata term {}",
        envelope.raft_term,
        expected_raft_term,
    );
    anyhow::ensure!(
        envelope.state_machine_index <= envelope.raft_index,
        "Raft snapshot state-machine index {} exceeds snapshot index {}",
        envelope.state_machine_index,
        envelope.raft_index,
    );
    let actual_digest = hex::encode(Sha256::hash(&envelope.checkpoint_bytes));
    anyhow::ensure!(
        actual_digest == envelope.checkpoint_sha256,
        "Raft snapshot checkpoint digest mismatch",
    );
    let checkpoint = checkpoint_from_bytes(&envelope.checkpoint_bytes)?;
    anyhow::ensure!(
        u64::from(checkpoint.timestamp) == envelope.checkpoint_timestamp,
        "Raft snapshot checkpoint timestamp {} does not match envelope timestamp {}",
        u64::from(checkpoint.timestamp),
        envelope.checkpoint_timestamp,
    );
    let persisted_state_machine_index = checkpoint
        .globals
        .get(RAFT_STATE_MACHINE_INDEX_KEY)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    anyhow::ensure!(
        persisted_state_machine_index == envelope.state_machine_index,
        "Raft snapshot checkpoint state-machine index {} does not match envelope index {}",
        persisted_state_machine_index,
        envelope.state_machine_index,
    );
    Ok(DecodedRaftSnapshot {
        raft_index: envelope.raft_index,
        raft_term: envelope.raft_term,
        state_machine_index: envelope.state_machine_index,
        checkpoint,
    })
}

#[cfg(test)]
pub(crate) fn test_snapshot_bytes(raft_index: u64, raft_term: u64) -> anyhow::Result<Vec<u8>> {
    let checkpoint = CheckpointData {
        timestamp: Timestamp::try_from(raft_index.max(1))?,
        documents: Vec::new(),
        globals: std::collections::BTreeMap::from([(
            RAFT_STATE_MACHINE_INDEX_KEY.to_string(),
            raft_index.into(),
        )]),
    };
    encode_snapshot(raft_index, raft_term, raft_index, &checkpoint)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn checkpoint(state_machine_index: u64) -> CheckpointData {
        CheckpointData {
            timestamp: Timestamp::must(42),
            documents: Vec::new(),
            globals: BTreeMap::from([(
                RAFT_STATE_MACHINE_INDEX_KEY.to_string(),
                state_machine_index.into(),
            )]),
        }
    }

    #[test]
    fn test_snapshot_envelope_round_trip() -> anyhow::Result<()> {
        let bytes = encode_snapshot(9, 3, 8, &checkpoint(8))?;
        let decoded = decode_snapshot(&bytes, 9, 3)?;
        assert_eq!(decoded.raft_index, 9);
        assert_eq!(decoded.raft_term, 3);
        assert_eq!(decoded.state_machine_index, 8);
        assert_eq!(decoded.checkpoint.timestamp, Timestamp::must(42));
        Ok(())
    }

    #[test]
    fn test_snapshot_envelope_rejects_metadata_mismatch() -> anyhow::Result<()> {
        let bytes = encode_snapshot(9, 3, 8, &checkpoint(8))?;
        assert!(decode_snapshot(&bytes, 10, 3).is_err());
        assert!(decode_snapshot(&bytes, 9, 4).is_err());
        Ok(())
    }

    #[test]
    fn test_snapshot_envelope_rejects_corruption() -> anyhow::Result<()> {
        let bytes = encode_snapshot(9, 3, 8, &checkpoint(8))?;
        let mut envelope = RaftSnapshotEnvelope::decode(bytes.as_slice())?;
        envelope.checkpoint_bytes.push(0);
        assert!(decode_snapshot(&envelope.encode_to_vec(), 9, 3).is_err());
        Ok(())
    }
}
