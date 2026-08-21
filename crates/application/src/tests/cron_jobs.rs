use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use common::{
    components::{
        CanonicalizedComponentFunctionPath,
        ComponentId,
        ComponentPath,
    },
    pause::PauseController,
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
    },
    runtime::Runtime,
};
use database::{
    partition::PartitionId,
    query::TableFilter,
    raft_partition::RaftPartitionState,
    DeveloperQuery,
    TableModel,
    Transaction,
};
use errors::ErrorMetadata;
use events::{
    testing::BasicTestUsageEventLogger,
    usage::{
        FunctionCallUsageFields,
        UsageEvent,
    },
};
use keybroker::Identity;
use model::{
    backend_state::{
        types::BackendState,
        BackendStateModel,
    },
    cron_jobs::{
        types::{
            CronIdentifier,
            CronJob,
            CronJobState,
            CronSchedule,
            CronSpec,
        },
        CronModel,
        CRON_JOB_LOGS_INDEX_BY_NAME_TS,
        CRON_JOB_LOGS_NAME_FIELD,
    },
};
use runtime::testing::TestRuntime;
use serde_json::Value as JsonValue;
use udf::helpers::parse_udf_args;

use crate::{
    cron_jobs::{
        CRON_ACTION_CLAIMED,
        CRON_COMITTING,
        CRON_JOB_AUTHORITY_REJECTED,
        CRON_JOB_EXECUTED,
        CRON_JOB_QUERIED,
    },
    test_helpers::{
        ApplicationFixtureArgs,
        ApplicationTestExt,
        OBJECTS_TABLE,
        OBJECTS_TABLE_COMPONENT,
    },
    Application,
    ApplicationWorkerStartupPolicy,
    ScheduledAndCronWorkerAuthority,
    ScheduledAndCronWorkerStartup,
};

fn test_cron_identifier() -> CronIdentifier {
    CronIdentifier::from_str("test").unwrap()
}

async fn create_cron_job(
    tx: &mut Transaction<TestRuntime>,
) -> anyhow::Result<(
    BTreeMap<CronIdentifier, CronJob>,
    CronModel<'_, TestRuntime>,
)> {
    let mut cron_model = CronModel::new(tx, ComponentId::test_user());
    let mut map = serde_json::Map::new();
    map.insert(
        "key".to_string(),
        serde_json::Value::String("value".to_string()),
    );
    let path = CanonicalizedComponentFunctionPath {
        component: ComponentPath::test_user(),
        udf_path: "basic:insertObject".parse()?,
    };
    let cron_spec = CronSpec {
        udf_path: path.udf_path.clone(),
        udf_args: parse_udf_args(&path.udf_path, vec![JsonValue::Object(map)])?
            .into_serialized_args()?,
        cron_schedule: CronSchedule::Interval { seconds: 60 },
    };
    let original_jobs = cron_model.list().await?;
    let name = test_cron_identifier();
    cron_model.create(name, cron_spec).await?;
    Ok((original_jobs, cron_model))
}

fn cron_log_query<RT: Runtime>(
    tx: &mut Transaction<RT>,
    component: ComponentId,
) -> anyhow::Result<DeveloperQuery<RT>> {
    DeveloperQuery::new(
        tx,
        component.into(),
        Query::index_range(IndexRange {
            index_name: CRON_JOB_LOGS_INDEX_BY_NAME_TS.name(),
            range: vec![IndexRangeExpression::Eq(
                CRON_JOB_LOGS_NAME_FIELD.clone(),
                common::types::MaybeValue(Some(test_cron_identifier().to_string().try_into()?)),
            )],
            order: Order::Asc,
        }),
        TableFilter::IncludePrivateSystemTables,
    )
}

#[convex_macro::test_runtime]
pub(crate) async fn test_cron_jobs_success(rt: TestRuntime) -> anyhow::Result<()> {
    let application = Application::new_for_tests(&rt).await?;
    application.load_udf_tests_modules().await?;
    // udf-tests include crons, so we let them execute so that we can then add
    // a new cron without hitting an OCC.
    rt.wait(Duration::from_secs(100)).await;

    let mut tx = application.begin(Identity::system()).await?;

    let (original_jobs, mut cron_model) = create_cron_job(&mut tx).await?;

    let jobs = cron_model.list().await?;
    assert_eq!(jobs.len(), original_jobs.len() + 1);

    let mut table_model = TableModel::new(&mut tx);
    assert!(
        table_model
            .table_is_empty(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?
    );

    application.commit_test(tx).await?;

    // Cron jobs executor within application will pick up the job and
    // execute it. Add some wait time to make this less racy.
    rt.wait(Duration::from_secs(100)).await;
    let mut tx = application.begin(Identity::system()).await?;
    let mut table_model = TableModel::new(&mut tx);
    assert!(
        !table_model
            .table_is_empty(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?
    );
    let mut logs_query = cron_log_query(&mut tx, OBJECTS_TABLE_COMPONENT)?;
    assert!(logs_query.next(&mut tx, None).await?.is_some());
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cron_action_leadership_handoff_fences_stale_owner(
    rt: TestRuntime,
    pause_controller: PauseController,
) -> anyhow::Result<()> {
    let application = Application::new_for_tests_with_args(
        &rt,
        ApplicationFixtureArgs {
            worker_startup_policy: Some(ApplicationWorkerStartupPolicy {
                scheduled_and_cron: ScheduledAndCronWorkerStartup::Disabled,
                ..ApplicationWorkerStartupPolicy::single_node()
            }),
            ..Default::default()
        },
    )
    .await?;
    application.load_udf_tests_modules().await?;

    let name = CronIdentifier::from_str("handoff")?;
    let path = CanonicalizedComponentFunctionPath {
        component: ComponentPath::test_user(),
        udf_path: "action:insertObject".parse()?,
    };
    let mut args = serde_json::Map::new();
    args.insert(
        "key".to_string(),
        serde_json::Value::String("value".to_string()),
    );
    let cron_spec = CronSpec {
        udf_path: path.udf_path.clone(),
        udf_args: parse_udf_args(&path.udf_path, vec![JsonValue::Object(args)])?
            .into_serialized_args()?,
        cron_schedule: CronSchedule::Interval { seconds: 60 },
    };
    let mut tx = application.begin(Identity::system()).await?;
    let mut cron_model = CronModel::new(&mut tx, ComponentId::test_user());
    let existing_metadata = cron_model.list_metadata().await?;
    for metadata in existing_metadata.into_values() {
        cron_model.delete(metadata).await?;
    }
    cron_model.create(name.clone(), cron_spec).await?;
    let initial_job = cron_model
        .list()
        .await?
        .remove(&name)
        .expect("handoff cron should exist");
    let initial_next_ts = initial_job.next_ts;
    application.commit_test(tx).await?;
    rt.wait(Duration::from_secs(100)).await;

    let raft_state = RaftPartitionState::new_for_test(true, 1, PartitionId::DEFAULT, 1);
    let old_epoch = raft_state
        .current_leadership_epoch()
        .expect("old owner should begin with authority");
    let queried = pause_controller.hold(CRON_JOB_QUERIED);
    application.start_scheduled_and_cron_workers(ScheduledAndCronWorkerAuthority::raft(
        raft_state.clone(),
        old_epoch,
    ));
    let queried_pause = queried
        .wait_for_blocked()
        .await
        .expect("old owner should query the due cron action");

    raft_state.set_leadership_for_test(false, 2, 2);
    let rejected = pause_controller.hold(CRON_JOB_AUTHORITY_REJECTED);
    queried_pause.unpause();
    let rejected_pause = rejected
        .wait_for_blocked()
        .await
        .expect("stale cron owner should be rejected at dispatch");
    let mut tx = application.begin(Identity::system()).await?;
    let stale_job = CronModel::new(&mut tx, ComponentId::test_user())
        .list()
        .await?
        .remove(&name)
        .expect("handoff cron should remain pending");
    assert_eq!(stale_job.state, CronJobState::Pending);
    assert_eq!(stale_job.next_ts, initial_next_ts);
    assert_eq!(
        TableModel::new(&mut tx)
            .must_count(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?,
        0,
        "stale cron owner must not start the action",
    );
    rejected_pause.unpause();
    application.stop_scheduled_and_cron_workers();

    raft_state.set_leadership_for_test(true, 1, 3);
    let new_epoch = raft_state
        .current_leadership_epoch()
        .expect("new owner should expose a fresh authority epoch");
    assert_ne!(new_epoch, old_epoch);
    let executed = pause_controller.hold(CRON_JOB_EXECUTED);
    application.start_scheduled_and_cron_workers(ScheduledAndCronWorkerAuthority::raft(
        raft_state, new_epoch,
    ));
    if let Some(executed_pause) = executed.wait_for_blocked().await {
        executed_pause.unpause();
    }

    let mut tx = application.begin(Identity::system()).await?;
    let fresh_job = CronModel::new(&mut tx, ComponentId::test_user())
        .list()
        .await?
        .remove(&name)
        .expect("handoff cron should remain scheduled");
    assert_eq!(fresh_job.state, CronJobState::Pending);
    assert!(fresh_job.next_ts > initial_next_ts);
    assert_eq!(
        TableModel::new(&mut tx)
            .must_count(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?,
        1,
        "fresh cron owner should produce one action effect",
    );
    application.stop_scheduled_and_cron_workers();
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cron_action_handoff_after_claim_is_not_replayed(
    rt: TestRuntime,
    pause_controller: PauseController,
) -> anyhow::Result<()> {
    let application = Application::new_for_tests_with_args(
        &rt,
        ApplicationFixtureArgs {
            worker_startup_policy: Some(ApplicationWorkerStartupPolicy {
                scheduled_and_cron: ScheduledAndCronWorkerStartup::Disabled,
                ..ApplicationWorkerStartupPolicy::single_node()
            }),
            ..Default::default()
        },
    )
    .await?;
    application.load_udf_tests_modules().await?;

    let name = CronIdentifier::from_str("post-claim-handoff")?;
    let path = CanonicalizedComponentFunctionPath {
        component: ComponentPath::test_user(),
        udf_path: "action:insertObject".parse()?,
    };
    let mut args = serde_json::Map::new();
    args.insert(
        "key".to_string(),
        serde_json::Value::String("value".to_string()),
    );
    let cron_spec = CronSpec {
        udf_path: path.udf_path.clone(),
        udf_args: parse_udf_args(&path.udf_path, vec![JsonValue::Object(args)])?
            .into_serialized_args()?,
        cron_schedule: CronSchedule::Interval { seconds: 60 },
    };
    let mut tx = application.begin(Identity::system()).await?;
    let mut cron_model = CronModel::new(&mut tx, ComponentId::test_user());
    let existing_metadata = cron_model.list_metadata().await?;
    for metadata in existing_metadata.into_values() {
        cron_model.delete(metadata).await?;
    }
    cron_model.create(name.clone(), cron_spec).await?;
    let initial_next_ts = cron_model
        .list()
        .await?
        .remove(&name)
        .expect("post-claim cron should exist")
        .next_ts;
    application.commit_test(tx).await?;
    rt.wait(Duration::from_secs(100)).await;

    let raft_state = RaftPartitionState::new_for_test(true, 1, PartitionId::DEFAULT, 1);
    let old_epoch = raft_state
        .current_leadership_epoch()
        .expect("old owner should begin with authority");
    let claimed = pause_controller.hold(CRON_ACTION_CLAIMED);
    application.start_scheduled_and_cron_workers(ScheduledAndCronWorkerAuthority::raft(
        raft_state.clone(),
        old_epoch,
    ));
    let claimed_pause = claimed
        .wait_for_blocked()
        .await
        .expect("old owner should durably claim the cron action");

    raft_state.set_leadership_for_test(false, 2, 2);
    let rejected = pause_controller.hold(CRON_JOB_AUTHORITY_REJECTED);
    claimed_pause.unpause();
    let rejected_pause = rejected
        .wait_for_blocked()
        .await
        .expect("stale cron owner should be fenced after the durable claim");

    let mut tx = application.begin(Identity::system()).await?;
    let claimed_job = CronModel::new(&mut tx, ComponentId::test_user())
        .list()
        .await?
        .remove(&name)
        .expect("claimed cron should still exist");
    assert!(matches!(claimed_job.state, CronJobState::InProgress { .. }));
    assert_eq!(
        TableModel::new(&mut tx)
            .must_count(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?,
        0,
        "the stale cron owner must not dispatch the claimed action",
    );
    let stale_finished = pause_controller.hold(CRON_JOB_EXECUTED);
    rejected_pause.unpause();
    let stale_finished_pause = stale_finished
        .wait_for_blocked()
        .await
        .expect("stale claimed cron worker generation should finish without dispatching");
    stale_finished_pause.unpause();
    application.stop_scheduled_and_cron_workers();

    raft_state.set_leadership_for_test(true, 1, 3);
    let new_epoch = raft_state
        .current_leadership_epoch()
        .expect("new owner should expose a fresh authority epoch");
    let executed = pause_controller.hold(CRON_JOB_EXECUTED);
    application.start_scheduled_and_cron_workers(ScheduledAndCronWorkerAuthority::raft(
        raft_state, new_epoch,
    ));
    let executed_pause = executed
        .wait_for_blocked()
        .await
        .expect("fresh owner should resolve the uncertain cron claim");

    let mut tx = application.begin(Identity::system()).await?;
    let resolved_job = CronModel::new(&mut tx, ComponentId::test_user())
        .list()
        .await?
        .remove(&name)
        .expect("resolved cron should remain scheduled");
    assert_eq!(resolved_job.state, CronJobState::Pending);
    assert!(resolved_job.next_ts > initial_next_ts);
    assert_eq!(
        TableModel::new(&mut tx)
            .must_count(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?,
        0,
        "the fresh cron owner must not replay an action with an uncertain claim",
    );
    executed_pause.unpause();
    application.stop_scheduled_and_cron_workers();
    Ok(())
}

#[convex_macro::test_runtime]
pub(crate) async fn test_cron_jobs_race_condition(rt: TestRuntime) -> anyhow::Result<()> {
    let application = Application::new_for_tests(&rt).await?;
    application.load_udf_tests_modules().await?;
    // udf-tests include crons, so we let them execute so that we can then add
    // a new cron without hitting an OCC.
    rt.wait(Duration::from_secs(100)).await;

    let mut tx = application.begin(Identity::system()).await?;
    let (original_jobs, mut model) = create_cron_job(&mut tx).await?;

    let jobs = model.list().await?;
    assert_eq!(jobs.len(), original_jobs.len() + 1);
    let job = jobs.get(&test_cron_identifier()).unwrap();

    // Delete the cron job
    let job_metadata = model
        .list_metadata()
        .await?
        .remove(&test_cron_identifier())
        .unwrap();
    model.delete(job_metadata).await?;
    let jobs = model.list().await?;
    assert_eq!(jobs.len(), original_jobs.len());

    application.commit_test(tx).await?;

    // This simulates the race condition where the job executor picks up a cron
    // to execute after the cron was created but before it was deleted. We should
    // handle the race condition gracefully instead of trying to run the stale cron.
    application
        .test_one_off_cron_job_executor_run(job.clone())
        .await?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_cron_occ_gets_logged(
    rt: TestRuntime,
    pause_controller: PauseController,
) -> anyhow::Result<()> {
    let logger = BasicTestUsageEventLogger::new();
    let application = Application::new_for_tests_with_args(
        &rt,
        ApplicationFixtureArgs::with_event_logger(Arc::new(logger.clone())),
    )
    .await?;
    application.load_udf_tests_modules().await?;
    // Wait for built-in crons from udf-tests to execute before setting up
    // the pause, so they don't intercept it.
    rt.wait(Duration::from_secs(100)).await;
    let attempt_commit = pause_controller.hold(CRON_COMITTING);
    let mut tx = application.begin(Identity::system()).await?;
    create_cron_job(&mut tx).await?;
    application.commit_test(tx).await?;
    let mut pause_guard = attempt_commit.wait_for_blocked().await.unwrap();
    pause_guard.inject_error(anyhow::anyhow!(ErrorMetadata::user_occ(
        None, None, None, None, None
    )));
    // Hold the commit pause again so the retry blocks there, guaranteeing the
    // OCC event from the first attempt has been logged.
    let second_attempt_commit = pause_controller.hold(CRON_COMITTING);
    pause_guard.unpause();
    let pause_guard = second_attempt_commit.wait_for_blocked().await.unwrap();
    pause_guard.unpause();
    // Verify usage is tracked for the OCC'd attempt.
    let function_call_events: Vec<FunctionCallUsageFields> = logger
        .collect()
        .into_iter()
        .filter_map(|event| {
            if let UsageEvent::FunctionCall { fields } = event {
                if fields.udf_id.contains("insertObject") {
                    Some(fields)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    // We expect one function call event for the OCC'd attempt.
    assert_eq!(
        function_call_events.len(),
        1,
        "Expected 1 function call usage event (OCC), got {}: {:?}",
        function_call_events.len(),
        function_call_events,
    );
    let occ_event = &function_call_events[0];
    assert!(
        occ_event.is_occ,
        "Expected the function call event to be an OCC, got: {:?}",
        occ_event,
    );
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_paused_cron_jobs(rt: TestRuntime) -> anyhow::Result<()> {
    test_cron_jobs_helper(rt, BackendState::Paused).await?;
    Ok(())
}

#[convex_macro::test_runtime]
async fn test_disable_cron_jobs(rt: TestRuntime) -> anyhow::Result<()> {
    test_cron_jobs_helper(rt, BackendState::Disabled).await?;
    Ok(())
}

async fn test_cron_jobs_helper(rt: TestRuntime, backend_state: BackendState) -> anyhow::Result<()> {
    // Helper for testing behavior for pausing or disabling backends
    let application = Application::new_for_tests(&rt).await?;
    application.load_udf_tests_modules().await?;

    // Change backend state
    let mut tx = application.begin(Identity::system()).await?;
    let mut backend_state_model = BackendStateModel::new(&mut tx);
    backend_state_model
        .toggle_backend_state(backend_state)
        .await?;
    application.commit_test(tx).await?;

    let mut tx = application.begin(Identity::system()).await?;
    let (original_jobs, mut cron_model) = create_cron_job(&mut tx).await?;
    let jobs = cron_model.list().await?;
    assert_eq!(jobs.len(), original_jobs.len() + 1);
    let mut table_model = TableModel::new(&mut tx);
    assert!(
        table_model
            .table_is_empty(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?
    );
    application.commit_test(tx).await?;

    // Cron jobs executor within application will pick up the job and
    // execute it. Add some wait time to make this less racy. Job should not execute
    // because the backend is paused.
    rt.wait(Duration::from_secs(100)).await;
    let mut tx = application.begin(Identity::system()).await?;
    let mut table_model = TableModel::new(&mut tx);
    assert!(
        table_model
            .table_is_empty(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?
    );
    let mut logs_query = cron_log_query(&mut tx, ComponentId::test_user())?;
    assert!(logs_query.next(&mut tx, Some(1)).await?.is_none());

    // Resuming the backend should make the jobs execute.
    let mut model = BackendStateModel::new(&mut tx);
    model.toggle_backend_state(BackendState::Running).await?;
    application.commit_test(tx).await?;
    rt.wait(Duration::from_secs(100)).await;
    let mut tx = application.begin(Identity::system()).await?;
    let mut table_model = TableModel::new(&mut tx);
    assert!(
        !table_model
            .table_is_empty(OBJECTS_TABLE_COMPONENT.into(), &OBJECTS_TABLE)
            .await?
    );
    let mut logs_query = cron_log_query(&mut tx, ComponentId::Root)?;
    assert!(logs_query.next(&mut tx, None).await?.is_some());

    Ok(())
}
