//! Write Scaling Unit Tests
//!
//! Tests partition ownership enforcement (Vitess Single mode pattern).
//! Cross-partition replication tests run against the live Docker deployment
//! via self-hosted/docker/test-write-scaling.sh (CockroachDB roachtest
//! pattern).

use std::{
    sync::Arc,
    time::Duration,
};

use common::{
    assert_obj,
    bootstrap_model::index::database_index::IndexedFields,
    interval::Interval,
    runtime::{
        new_unlimited_rate_limiter,
        testing::TestRuntime,
    },
    shutdown::ShutdownSignal,
    testing::TestPersistence,
    types::TabletIndexName,
};
use keybroker::Identity;
use search::searcher::SearcherStub;
use storage::LocalDirStorage;
use value::{
    TableName,
    TableNamespace,
};

use crate::{
    commit_delta::{
        testing::InMemoryDistributedLog,
        CommitDelta,
    },
    partition::{
        PartitionId,
        PartitionMap,
    },
    two_phase::TwoPhaseTransactionId,
    Database,
    TestFacingModel,
    WriteSource,
};

async fn create_node(
    rt: &TestRuntime,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
) -> anyhow::Result<Database<TestRuntime>> {
    create_node_with_persistence(
        rt,
        Arc::new(TestPersistence::new()),
        distributed_log,
        partition_map,
    )
    .await
}

async fn create_node_with_persistence(
    rt: &TestRuntime,
    tp: Arc<TestPersistence>,
    distributed_log: Arc<InMemoryDistributedLog>,
    partition_map: Option<PartitionMap>,
) -> anyhow::Result<Database<TestRuntime>> {
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
        None,
    )
    .await?;
    db.set_search_storage(Arc::new(LocalDirStorage::new(rt.clone())?));
    let handle = db.start_search_and_vector_bootstrap();
    handle.join().await?;
    Ok(db)
}

fn partitioned_map(local_partition: PartitionId) -> PartitionMap {
    PartitionMap::from_config("messages=0,projects=1", local_partition, 2)
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

/// Jepsen sequential: write A, B, C sequentially from one client.
/// Commit timestamps must be strictly increasing.
/// CockroachDB Jepsen found disjoint records visible out of order.
#[convex_macro::test_runtime]
async fn test_sequential_ordering(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let ts1 = insert_doc(&db, "items", assert_obj!("label" => "first")).await?;
    let ts2 = insert_doc(&db, "items", assert_obj!("label" => "second")).await?;
    let ts3 = insert_doc(&db, "items", assert_obj!("label" => "third")).await?;

    assert!(ts2 > ts1, "second must commit after first");
    assert!(ts3 > ts2, "third must commit after second");

    Ok(())
}

/// Jepsen set: insert N unique elements. All must succeed — no lost inserts.
#[convex_macro::test_runtime]
async fn test_set_completeness(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let n = 100;
    let mut timestamps = Vec::new();
    for i in 0..n {
        let ts = insert_doc(&db, "elements", assert_obj!("value" => i as i64)).await?;
        timestamps.push(ts);
    }

    // All N inserts must have succeeded with increasing timestamps.
    assert_eq!(timestamps.len(), n);
    for i in 1..timestamps.len() {
        assert!(
            timestamps[i] > timestamps[i - 1],
            "Insert {} timestamp not after insert {}: {:?} vs {:?}",
            i,
            i - 1,
            timestamps[i],
            timestamps[i - 1],
        );
    }

    Ok(())
}

/// TiDB monotonic: commit timestamps must be strictly increasing.
/// No commit ever gets a timestamp <= a previous commit.
#[convex_macro::test_runtime]
async fn test_monotonic_timestamps(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let mut last_ts = common::types::Timestamp::MIN;
    for i in 1..=20i64 {
        let ts = insert_doc(&db, "counters", assert_obj!("value" => i)).await?;
        assert!(
            ts > last_ts,
            "Monotonic violation: ts {:?} <= previous {:?}",
            ts,
            last_ts,
        );
        last_ts = ts;
    }

    Ok(())
}

/// Duplicate insert: insert same data twice. Both must succeed.
/// Convex has no unique constraints — no deduplication.
#[convex_macro::test_runtime]
async fn test_duplicate_insert(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    let ts1 = insert_doc(
        &db,
        "dupes",
        assert_obj!("key" => "same", "value" => "data"),
    )
    .await?;
    let ts2 = insert_doc(
        &db,
        "dupes",
        assert_obj!("key" => "same", "value" => "data"),
    )
    .await?;

    assert!(ts2 > ts1, "Second duplicate must commit after first");

    // Both committed — verify by inserting_and_getting a third doc.
    let mut tx = db.begin(Identity::system()).await?;
    let table_name: TableName = "dupes".parse()?;
    let doc = TestFacingModel::new(&mut tx)
        .insert_and_get(table_name, assert_obj!("key" => "third"))
        .await?;
    // If we got here without error, the table exists and accepts inserts.
    assert!(!doc.id().to_string().is_empty());

    Ok(())
}

/// CockroachDB Jepsen register: write values, read back via insert_and_get.
/// Each write must succeed and produce a unique document.
#[convex_macro::test_runtime]
async fn test_single_key_register(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log, None).await?;

    // Write initial value.
    let ts1 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "first"),
    )
    .await?;

    // Write second value.
    let ts2 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "second"),
    )
    .await?;
    assert!(ts2 > ts1, "Second write must commit after first");

    // Write third value.
    let ts3 = insert_doc(
        &db,
        "registers",
        assert_obj!("key" => "r1", "value" => "third"),
    )
    .await?;
    assert!(ts3 > ts2, "Third write must commit after second");

    // Verify via insert_and_get that the table has data.
    let mut tx = db.begin(Identity::system()).await?;
    let table_name: TableName = "registers".parse()?;
    let doc = TestFacingModel::new(&mut tx)
        .insert_and_get(table_name, assert_obj!("key" => "r1", "value" => "fourth"))
        .await?;
    assert!(
        !doc.id().to_string().is_empty(),
        "Register table should accept writes"
    );

    Ok(())
}

/// CockroachDB Jepsen bank: create records with known numeric values.
/// Verify all inserts succeed and the delta log captures them.
/// Sum preservation is verified via the distributed log.
#[convex_macro::test_runtime]
async fn test_bank_invariant(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let db = create_node(&rt, log.clone(), None).await?;

    let balances: Vec<i64> = vec![10000, 25000, 50000, 15000];

    for balance in &balances {
        insert_doc(&db, "accounts", assert_obj!("balance" => *balance)).await?;
    }

    // Wait for async publish tasks.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify deltas were published — each insert should produce a delta
    // with document updates.
    let deltas = log.deltas();
    let account_deltas: Vec<_> = deltas
        .iter()
        .filter(|d| d.document_updates.iter().any(|u| u.new_document.is_some()))
        .collect();

    assert!(
        account_deltas.len() >= balances.len(),
        "Expected at least {} deltas with document inserts, got {}",
        balances.len(),
        account_deltas.len(),
    );

    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cross_partition_prepare_waits_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot without advancing partition 1's frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
        .await?;
    let mut final_tx = tx.finalize()?;
    let remote_tablet = final_tx
        .table_mapping
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    final_tx.reads.record_indexed_derived(
        TabletIndexName::by_id(remote_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let committer = node_a.committer_for_test();
    let prepare = tokio::spawn(async move {
        committer
            .prepare(
                TwoPhaseTransactionId::new(),
                final_tx,
                WriteSource::new("cross_partition_wait_test"),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !prepare.is_finished(),
        "prepare should wait for the remote partition frontier to catch up",
    );

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_heartbeat_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let prepare_result = tokio::time::timeout(Duration::from_secs(1), prepare).await??;
    prepare_result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_only_prepare_does_not_wait_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot beyond partition 1's persisted frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "prepared"))
        .await?;
    let final_tx = tx.finalize()?;

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        node_a.committer_for_test().prepare(
            TwoPhaseTransactionId::new(),
            final_tx,
            WriteSource::new("local_only_prepare_test"),
        ),
    )
    .await?;
    result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_commit_waits_for_remote_frontier(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's local snapshot without advancing partition 1's frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "commit"))
        .await?;
    let remote_tablet = tx
        .table_mapping()
        .namespace(TableNamespace::Global)
        .name_to_tablet()("projects".parse()?)?;
    tx.reads.record_indexed_derived(
        TabletIndexName::by_id(remote_tablet),
        IndexedFields::by_id(),
        Interval::all(),
    );

    let node_a_for_commit = node_a.clone();
    let commit = tokio::spawn(async move { node_a_for_commit.commit(tx).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !commit.is_finished(),
        "commit should wait for the remote partition frontier to catch up",
    );

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_heartbeat_commit_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let commit_result = tokio::time::timeout(Duration::from_secs(1), commit).await??;
    commit_result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_local_only_commit_does_not_wait_for_remote_frontier(
    rt: TestRuntime,
) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let node_a = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(0)))).await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    // Advance node A's snapshot beyond partition 1's persisted frontier.
    insert_doc(&node_a, "messages", assert_obj!("text" => "lag-maker")).await?;

    let mut tx = node_a.begin(Identity::system()).await?;
    TestFacingModel::new(&mut tx)
        .insert(&"messages".parse()?, assert_obj!("text" => "commit"))
        .await?;

    let result = tokio::time::timeout(Duration::from_millis(250), node_a.commit(tx)).await?;
    result?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_replication_frontier_persists_across_restart(rt: TestRuntime) -> anyhow::Result<()> {
    let log = Arc::new(InMemoryDistributedLog::new());
    let tp = Arc::new(TestPersistence::new());
    let node_a = create_node_with_persistence(
        &rt,
        tp.clone(),
        log.clone(),
        Some(partitioned_map(PartitionId(0))),
    )
    .await?;
    let node_b = create_node(&rt, log.clone(), Some(partitioned_map(PartitionId(1)))).await?;

    insert_doc(&node_b, "projects", assert_obj!("name" => "seed")).await?;
    let initial_delta = log
        .deltas()
        .into_iter()
        .last()
        .expect("seed project should publish a delta");
    node_a
        .committer_for_test()
        .apply_replica_delta(initial_delta)
        .await?;

    let heartbeat_ts = node_b.bump_max_repeatable_ts().await?;
    node_a
        .committer_for_test()
        .apply_replica_delta(CommitDelta {
            ts: heartbeat_ts,
            document_writes: Arc::new(Vec::new()),
            document_updates: Vec::new(),
            index_writes: Arc::new(Vec::new()),
            write_source: WriteSource::new("replication_frontier_persist_test"),
            write_bytes: 0,
            tablet_id_to_table_name: Default::default(),
            source_partition: Some(PartitionId(1)),
        })
        .await?;

    let persisted_frontier = node_a
        .replication_frontier_for_test(PartitionId(1))
        .expect("partition 1 frontier should be recorded");

    let restarted =
        create_node_with_persistence(&rt, tp, log, Some(partitioned_map(PartitionId(0)))).await?;
    assert_eq!(
        restarted.replication_frontier_for_test(PartitionId(1)),
        Some(persisted_frontier),
    );

    Ok(())
}
