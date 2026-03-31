//! Trait abstraction over how transactions are committed.
//!
//! On a Primary node, [`LocalCommitClient`] sends transactions to the
//! in-process [`Committer`] via an mpsc channel (the existing behavior).
//!
//! On a Replica node, a future `RemoteCommitClient` will forward transactions
//! to the Primary over gRPC.

use std::{
    collections::BTreeSet,
    sync::Arc,
};

use async_trait::async_trait;
use common::{
    persistence::PersistenceReader,
    runtime::Runtime,
    types::Timestamp,
};
use futures::future::BoxFuture;
use value::TableName;

use crate::{
    committer::CommitterClient,
    write_log::WriteSource,
    Transaction,
};

/// Trait for committing transactions. Abstracts over local (in-process) and
/// remote (over-the-network) commit paths.
#[async_trait]
pub trait CommitClient: Send + Sync + 'static {
    /// Commit a transaction, returning the assigned commit timestamp.
    fn commit<RT: Runtime>(
        &self,
        transaction: Transaction<RT>,
        write_source: WriteSource,
    ) -> BoxFuture<'_, anyhow::Result<Timestamp>>;

    /// Load indexes into memory for the given tables.
    async fn load_indexes_into_memory(&self, tables: BTreeSet<TableName>) -> anyhow::Result<()>;

    /// Get a reader for the persistence layer.
    fn persistence_reader(&self) -> Arc<dyn PersistenceReader>;
}

/// Local commit client that delegates to the in-process [`CommitterClient`].
/// This is the default for Primary nodes and single-node deployments.
pub struct LocalCommitClient {
    inner: CommitterClient,
}

impl LocalCommitClient {
    pub fn new(inner: CommitterClient) -> Self {
        Self { inner }
    }

    /// Access the underlying [`CommitterClient`] for operations that are
    /// specific to the Primary (bootstrap, etc.).
    pub fn committer(&self) -> &CommitterClient {
        &self.inner
    }
}

#[async_trait]
impl CommitClient for LocalCommitClient {
    fn commit<RT: Runtime>(
        &self,
        transaction: Transaction<RT>,
        write_source: WriteSource,
    ) -> BoxFuture<'_, anyhow::Result<Timestamp>> {
        self.inner.commit(transaction, write_source)
    }

    async fn load_indexes_into_memory(&self, tables: BTreeSet<TableName>) -> anyhow::Result<()> {
        self.inner.load_indexes_into_memory(tables).await
    }

    fn persistence_reader(&self) -> Arc<dyn PersistenceReader> {
        self.inner.persistence_reader()
    }
}

/// A read-only commit client for Replica nodes. Rejects all mutations locally
/// — in a full implementation, this would forward to the Primary via gRPC.
pub struct ReadOnlyCommitClient;

#[async_trait]
impl CommitClient for ReadOnlyCommitClient {
    fn commit<RT: Runtime>(
        &self,
        _transaction: Transaction<RT>,
        _write_source: WriteSource,
    ) -> BoxFuture<'_, anyhow::Result<Timestamp>> {
        Box::pin(async {
            anyhow::bail!(
                "Cannot commit on a Replica node. Mutations must be forwarded to the Primary."
            )
        })
    }

    async fn load_indexes_into_memory(&self, _tables: BTreeSet<TableName>) -> anyhow::Result<()> {
        anyhow::bail!("Cannot load indexes on a Replica node.")
    }

    fn persistence_reader(&self) -> Arc<dyn PersistenceReader> {
        panic!("ReadOnlyCommitClient does not have a persistence reader")
    }
}
