use std::sync::Arc;

use common::{
    assert_obj,
    runtime::{
        new_unlimited_rate_limiter,
        testing::TestRuntime,
        Runtime,
    },
    shutdown::ShutdownSignal,
    testing::TestPersistence,
};
use keybroker::Identity;
use search::searcher::SearcherStub;
use storage::LocalDirStorage;
use value::TableName;

use crate::{
    commit_delta::testing::InMemoryDistributedLog,
    Database,
    TestFacingModel,
};

/// Test that a commit on the Primary publishes a CommitDelta to the
/// distributed log with the correct document updates.
#[convex_macro::test_runtime]
async fn test_primary_commit_publishes_delta(rt: TestRuntime) -> anyhow::Result<()> {
    let distributed_log = Arc::new(InMemoryDistributedLog::new());

    let tp = Arc::new(TestPersistence::new());
    let searcher: Arc<dyn search::Searcher> = Arc::new(SearcherStub {});
    let (deleted_tablet_sender, _) = tokio::sync::mpsc::channel(100);
    let primary = Database::load(
        tp.clone(),
        rt.clone(),
        searcher.clone(),
        ShutdownSignal::panic(),
        Default::default(),
        None,
        Arc::new(new_unlimited_rate_limiter(rt.clone())),
        deleted_tablet_sender,
        distributed_log.clone(),
    )
    .await?;
    primary.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = primary.start_search_and_vector_bootstrap();
    handle.join().await?;

    // Commit a document on the Primary.
    let table_name: TableName = "test_table".parse()?;
    let mut tx = primary.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&table_name, assert_obj!("field" => "value"))
        .await?;
    primary.commit(tx).await?;

    // Give the async publish task a moment to complete.
    rt.wait(std::time::Duration::from_millis(100)).await;

    // Verify the delta was published.
    let deltas = distributed_log.deltas();
    assert!(
        !deltas.is_empty(),
        "Expected at least one delta to be published"
    );

    // Find a delta that contains our document update (new_document is Some).
    let has_doc_update = deltas.iter().any(|d| {
        d.document_updates
            .iter()
            .any(|u| u.new_document.is_some())
    });
    assert!(has_doc_update, "Expected a delta with a document insert");

    Ok(())
}
