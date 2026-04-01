//! Write Scaling Unit Tests
//!
//! Tests partition ownership enforcement (Vitess Single mode pattern).
//! Cross-partition replication tests run against the live Docker deployment
//! via self-hosted/docker/test-write-scaling.sh (CockroachDB roachtest
//! pattern).

use std::sync::Arc;

use common::{
    assert_obj,
    runtime::{
        new_unlimited_rate_limiter,
        testing::TestRuntime,
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
    partition::{
        PartitionId,
        PartitionMap,
    },
    Database,
    TestFacingModel,
};

async fn create_node(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
) -> anyhow::Result<Database<TestRuntime>> {
    let tp = Arc::new(TestPersistence::new());
    let searcher: Arc<dyn search::Searcher> = Arc::new(SearcherStub {});
    let (deleted_tablet_sender, _) = tokio::sync::mpsc::channel(100);
    let db = Database::load(
        tp,
        rt.clone(),
        searcher,
        ShutdownSignal::panic(),
        Default::default(),
        None,
        Arc::new(new_unlimited_rate_limiter(rt.clone())),
        deleted_tablet_sender,
        distributed_log,
        false,
        partition_map,
        None,
    )
    .await?;
    db.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = db.start_search_and_vector_bootstrap();
    handle.join().await?;
    Ok(db)
}

async fn insert_doc(
    db: &Database<TestRuntime>,
    table: &str,
    fields: common::value::ConvexObject,
) -> anyhow::Result<common::types::Timestamp> {
    let table_name: TableName = table.parse()?;
    let mut tx = db.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&table_name, fields)
        .await?;
    db.commit(tx).await
}

/// Vitess Single mode pattern: writes to non-owned tables must be rejected
/// with a clear error identifying the correct partition owner.
#[convex_macro::test_runtime]
async fn test_partition_enforcement(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());

    let node_a = create_node(
        &rt,
        log.clone(),
        Some(PartitionMap::from_config(
            "messages=0,projects=1",
            PartitionId(0),
            2,
        )),
    )
    .await?;
    let node_b = create_node(
        &rt,
        log.clone(),
        Some(PartitionMap::from_config(
            "messages=0,projects=1",
            PartitionId(1),
            2,
        )),
    )
    .await?;

    // Node A writing to projects (owned by partition 1) — must fail.
    let result_a = insert_doc(&node_a, "projects", assert_obj!("name" => "wrong")).await;
    assert!(result_a.is_err(), "Node A should reject write to projects");
    assert!(
        result_a.unwrap_err().to_string().contains("partition-1"),
        "Error should mention the owning partition"
    );

    // Node B writing to messages (owned by partition 0) — must fail.
    let result_b = insert_doc(&node_b, "messages", assert_obj!("text" => "wrong")).await;
    assert!(result_b.is_err(), "Node B should reject write to messages");
    assert!(
        result_b.unwrap_err().to_string().contains("partition-0"),
        "Error should mention the owning partition"
    );

    // Correct-partition writes succeed.
    insert_doc(&node_a, "messages", assert_obj!("text" => "correct")).await?;
    insert_doc(&node_b, "projects", assert_obj!("name" => "correct")).await?;

    Ok(())
}
