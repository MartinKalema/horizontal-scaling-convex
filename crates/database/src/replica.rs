//! Replica node support for horizontal scaling.
//!
//! A [`ReplicaSnapshotUpdater`] consumes [`CommitDelta`]s from a
//! [`DistributedLog`] and applies them to a local [`SnapshotManager`],
//! keeping a Replica node's state in sync with the Primary.

use std::sync::Arc;

use anyhow::Context;
use common::{
    runtime::{
        Runtime,
        SpawnHandle,
    },
    sync::split_rw_lock::Writer,
    types::Timestamp,
};
use futures::StreamExt;

use crate::{
    commit_delta::DistributedLog,
    snapshot_manager::SnapshotManager,
    write_log::LogWriter,
};

/// Consumes [`CommitDelta`]s from a [`DistributedLog`] and applies them to a
/// local [`SnapshotManager`] and [`LogWriter`].
///
/// This is the core replication component. On a Replica node, it runs as a
/// background task that continuously tails the distributed log and replays
/// each delta to keep the Replica's in-memory state consistent with the
/// Primary.
pub struct ReplicaSnapshotUpdater {
    _handle: Box<dyn SpawnHandle>,
}

impl ReplicaSnapshotUpdater {
    /// Start the updater as a background task.
    ///
    /// It will subscribe to the distributed log starting after `from_ts` and
    /// apply each delta to the snapshot manager and write log in order.
    pub fn start<RT: Runtime>(
        runtime: RT,
        distributed_log: Arc<dyn DistributedLog>,
        snapshot_manager: Writer<SnapshotManager>,
        log_writer: LogWriter,
        from_ts: Timestamp,
    ) -> Self {
        let handle = runtime.spawn("replica_snapshot_updater", async move {
            if let Err(e) =
                Self::run(distributed_log, snapshot_manager, log_writer, from_ts).await
            {
                tracing::error!("ReplicaSnapshotUpdater failed: {e:?}");
            }
        });
        Self { _handle: handle }
    }

    async fn run(
        distributed_log: Arc<dyn DistributedLog>,
        mut snapshot_manager: Writer<SnapshotManager>,
        mut log_writer: LogWriter,
        from_ts: Timestamp,
    ) -> anyhow::Result<()> {
        let mut stream = distributed_log
            .subscribe(from_ts)
            .await
            .context("Failed to subscribe to distributed log")?;

        while let Some(result) = stream.next().await {
            let delta = result.context("Error reading from distributed log")?;
            let commit_ts = delta.ts;

            // Apply each document update to the snapshot, collecting index
            // updates along the way. This mirrors what the Primary's Committer
            // does in compute_writes + publish_commit.
            let mut sm = snapshot_manager.write();
            let mut snapshot = sm.latest_snapshot();
            for update in &delta.document_updates {
                snapshot
                    .update(update, commit_ts)
                    .with_context(|| format!("Failed to apply update at ts={commit_ts}"))?;
            }
            sm.push(commit_ts, snapshot, delta.write_bytes);
            drop(sm);

            // Append to the write log so subscription workers on this Replica
            // can detect invalidations from the Primary's commits.
            let writes = crate::write_log::index_keys_from_document_updates(
                &delta.document_updates,
                &snapshot_manager.read().latest_snapshot().index_registry,
            );
            log_writer.append(commit_ts, writes, delta.write_source);
        }

        Ok(())
    }
}
