use std::sync::Arc;

use async_trait::async_trait;
use common::{
    assert_obj,
    persistence::{
        Persistence,
        PersistenceGlobalKey,
    },
    runtime::{
        new_unlimited_rate_limiter,
        testing::TestRuntime,
    },
    shutdown::ShutdownSignal,
    testing::TestPersistence,
    types::Timestamp,
};
use futures::stream::BoxStream;
use keybroker::Identity;
use search::searcher::SearcherStub;
use storage::LocalDirStorage;
use value::TableName;

use crate::{
    commit_delta::{
        testing::InMemoryDistributedLog,
        CommitDelta,
        DistributedLog,
        ReplicationMessage,
        ReplicationPoisonGap,
        ReplicationPoisonKind,
    },
    Database,
    TestFacingModel,
};

async fn load_test_primary(
    rt: &TestRuntime,
    distributed_log: Arc<dyn DistributedLog>,
) -> anyhow::Result<Database<TestRuntime>> {
    load_test_primary_with_persistence(rt, Arc::new(TestPersistence::new()), distributed_log).await
}

async fn load_test_primary_with_persistence(
    rt: &TestRuntime,
    persistence: Arc<dyn Persistence>,
    distributed_log: Arc<dyn DistributedLog>,
) -> anyhow::Result<Database<TestRuntime>> {
    let searcher: Arc<dyn search::Searcher> = Arc::new(SearcherStub {});
    let (deleted_tablet_sender, _) = tokio::sync::mpsc::channel(100);
    let primary = Database::load(
        persistence,
        rt.clone(),
        searcher,
        ShutdownSignal::no_op(),
        Default::default(),
        None,
        Arc::new(new_unlimited_rate_limiter(rt.clone())),
        deleted_tablet_sender,
        distributed_log,
        false,
        None,
        None,
        Arc::new(crate::two_phase::NoopTwoPhaseDecisionLog),
        None,
        None,
        Arc::new(crate::LocalTableNumberAllocator),
        None,
    )
    .await?;
    primary.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = primary.start_search_and_vector_bootstrap();
    handle.join().await?;
    Ok(primary)
}

#[convex_macro::test_runtime]
async fn test_database_rejects_persisted_replication_poison_gap(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let persistence = Arc::new(TestPersistence::new());
    let gap = ReplicationPoisonGap::new(
        ReplicationPoisonKind::MalformedEnvelope,
        Some("node-1".to_string()),
        Some("convex.commits.0".to_string()),
        Some(42),
        Some("node-0".to_string()),
        Some(crate::partition::PartitionId(0)),
        Some(Timestamp::try_from(100u64)?),
        "invalid JSON",
    );
    persistence
        .write_persistence_global(
            PersistenceGlobalKey::ReplicationPoisonGap,
            serde_json::to_value(&gap)?,
        )
        .await?;

    let error = match load_test_primary_with_persistence(
        &rt,
        persistence,
        Arc::new(InMemoryDistributedLog::new()),
    )
    .await
    {
        Ok(_) => anyhow::bail!("database must not start with a poison-gap marker"),
        Err(error) => error,
    };
    let recovered_gap = error
        .downcast_ref::<ReplicationPoisonGap>()
        .expect("startup error must retain the poison-gap type");
    assert_eq!(recovered_gap, &gap);
    assert!(
        format!("{error:#}").contains("Rebootstrap its local persistence before serving"),
        "{error:#}",
    );
    Ok(())
}

struct FailingPublishDistributedLog;

#[async_trait]
impl DistributedLog for FailingPublishDistributedLog {
    async fn publish(&self, _delta: CommitDelta) -> anyhow::Result<()> {
        anyhow::bail!("forced replication publish failure")
    }

    async fn subscribe(
        &self,
        _from_ts: Timestamp,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<ReplicationMessage>>> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

/// Test that a commit on the Primary publishes a CommitDelta to the
/// distributed log with the correct document updates.
#[convex_macro::test_runtime]
async fn test_primary_commit_publishes_delta(rt: TestRuntime) -> anyhow::Result<()> {
    let distributed_log = Arc::new(InMemoryDistributedLog::new());
    let primary = load_test_primary(&rt, distributed_log.clone()).await?;

    // Commit a document on the Primary.
    let table_name: TableName = "test_table".parse()?;
    let mut tx = primary.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&table_name, assert_obj!("field" => "value"))
        .await?;
    primary.commit(tx).await?;

    // Verify the delta was published.
    let deltas = distributed_log.deltas();
    assert!(
        !deltas.is_empty(),
        "Expected at least one delta to be published"
    );

    // Find a delta that contains our document update (new_document is Some).
    let has_doc_update = deltas
        .iter()
        .any(|d| d.document_updates.iter().any(|u| u.new_document.is_some()));
    assert!(has_doc_update, "Expected a delta with a document insert");

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_commit_fails_when_replication_publish_fails(rt: TestRuntime) -> anyhow::Result<()> {
    let primary = load_test_primary(&rt, Arc::new(FailingPublishDistributedLog)).await?;

    let table_name: TableName = "test_table".parse()?;
    let mut tx = primary.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&table_name, assert_obj!("field" => "value"))
        .await?;

    let err = primary.commit(tx).await.unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("publish committed write to replication log"),
        "{message}"
    );
    assert!(
        message.contains("forced replication publish failure"),
        "{message}"
    );

    Ok(())
}
