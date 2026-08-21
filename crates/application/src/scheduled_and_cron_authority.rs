use std::fmt;

use database::raft_partition::{
    RaftLeadershipEpoch,
    RaftPartitionState,
};

/// Authority captured by a scheduled-job/cron worker generation.
///
/// Single-node deployments retain the upstream behavior. Clustered workers
/// carry a Raft term token and revalidate it at each claim and dispatch
/// boundary.
#[derive(Clone)]
pub struct ScheduledAndCronWorkerAuthority {
    kind: AuthorityKind,
}

#[derive(Clone)]
enum AuthorityKind {
    SingleNode,
    Raft {
        state: RaftPartitionState,
        epoch: RaftLeadershipEpoch,
    },
}

impl ScheduledAndCronWorkerAuthority {
    pub fn single_node() -> Self {
        Self {
            kind: AuthorityKind::SingleNode,
        }
    }

    pub fn raft(state: RaftPartitionState, epoch: RaftLeadershipEpoch) -> Self {
        Self {
            kind: AuthorityKind::Raft { state, epoch },
        }
    }

    pub(crate) fn ensure_current(
        &self,
        boundary: &'static str,
    ) -> Result<(), ScheduledAndCronAuthorityLost> {
        let AuthorityKind::Raft { state, epoch } = &self.kind else {
            return Ok(());
        };
        if state.is_current_leadership_epoch(*epoch) {
            return Ok(());
        }
        Err(ScheduledAndCronAuthorityLost {
            boundary,
            expected: *epoch,
            current: state.current_leadership_epoch(),
        })
    }
}

impl fmt::Debug for ScheduledAndCronWorkerAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AuthorityKind::SingleNode => f.write_str("SingleNode"),
            AuthorityKind::Raft { epoch, .. } => f.debug_tuple("Raft").field(&epoch).finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("scheduled job authority lost at {boundary}: expected {expected}, current {current:?}")]
pub(crate) struct ScheduledAndCronAuthorityLost {
    boundary: &'static str,
    expected: RaftLeadershipEpoch,
    current: Option<RaftLeadershipEpoch>,
}

pub(crate) fn is_scheduled_and_cron_authority_lost(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ScheduledAndCronAuthorityLost>()
        .is_some()
}
