//! State machine bridge: connects Raft committed entries to the Committer.
//!
//! When a Raft entry is committed (replicated to a majority), it needs to be
//! applied to the Convex state machine — the Committer's SnapshotManager,
//! WriteLog, and Persistence. This module defines the serialization format
//! for proposals and the apply logic.
//!
//! This is TiKV's Apply Worker pattern: committed Raft entries are decoded
//! and applied to the local state machine in order.
//!
//! References:
//!   - [TiKV: Raft in TiKV](https://www.pingcap.com/blog/raft-in-tikv/)

use serde::{
    Deserialize,
    Serialize,
};

/// A client mutation proposal serialized into a Raft log entry.
///
/// When a client sends a mutation to the Raft leader, the leader serializes
/// it into a `RaftProposalData` and proposes it to Raft. Once committed
/// (replicated to majority), the entry is deserialized and applied to the
/// Committer on every node in the Raft group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaftProposalData {
    /// The mutation function path (e.g., "messages:send").
    pub path: String,
    /// Serialized function arguments.
    pub args: String,
    /// Write source identifier for logging.
    pub write_source: String,
    /// Unique proposal ID for matching responses.
    pub proposal_id: String,
}

impl RaftProposalData {
    /// Serialize to bytes for Raft log entry.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("RaftProposalData serialization failed")
    }

    /// Deserialize from Raft log entry bytes.
    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        serde_json::from_slice(data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize RaftProposalData: {e}"))
    }
}

/// Result of applying a committed Raft entry.
#[derive(Debug)]
pub enum ApplyResult {
    /// The entry was applied successfully.
    Success { proposal_id: String },
    /// The entry failed to apply (OCC conflict, validation error, etc.).
    /// The client should retry.
    Error { proposal_id: String, error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proposal_roundtrip() {
        let proposal = RaftProposalData {
            path: "messages:send".to_string(),
            args: r#"{"text":"hello","author":"test"}"#.to_string(),
            write_source: "test".to_string(),
            proposal_id: "abc-123".to_string(),
        };

        let bytes = proposal.to_bytes();
        let decoded = RaftProposalData::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.path, "messages:send");
        assert_eq!(decoded.proposal_id, "abc-123");
    }

    #[test]
    fn test_proposal_invalid_bytes() {
        let result = RaftProposalData::from_bytes(b"not json");
        assert!(result.is_err());
    }
}
