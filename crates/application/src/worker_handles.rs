use std::sync::Arc;

use common::runtime::{
    shutdown_and_join,
    SpawnHandle,
};
use database::SearchIndexWorkers;
use parking_lot::Mutex;
use usage_gauges_tracking_worker::UsageGaugesTrackingWorker;

use crate::{
    scheduled_jobs::ScheduledJobRunner,
    table_summary_worker::TableSummaryClient,
};

#[derive(Clone)]
pub struct WorkerHandles {
    pub(crate) usage_gauges_tracking_worker: UsageGaugesTrackingWorker,
    pub(crate) scheduled_job_runner: Arc<Mutex<Option<ScheduledJobRunner>>>,
    pub(crate) cron_job_executor: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) index_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) fast_forward_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) search_worker: Arc<Mutex<Option<SearchIndexWorkers>>>,
    pub(crate) search_and_vector_bootstrap_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) table_summary_worker: Arc<Mutex<Option<TableSummaryClient>>>,
    pub(crate) schema_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) snapshot_import_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) export_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) system_table_cleanup_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
    pub(crate) migration_worker: Arc<Mutex<Option<Box<dyn SpawnHandle>>>>,
}

impl WorkerHandles {
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.usage_gauges_tracking_worker.shutdown().await?;
        let table_summary_worker = self.table_summary_worker.lock().take();
        if let Some(table_summary_worker) = table_summary_worker {
            table_summary_worker.shutdown().await?;
        }
        let system_table_cleanup_worker = self.system_table_cleanup_worker.lock().take();
        if let Some(mut system_table_cleanup_worker) = system_table_cleanup_worker {
            system_table_cleanup_worker.shutdown();
        }
        let schema_worker = self.schema_worker.lock().take();
        if let Some(mut schema_worker) = schema_worker {
            schema_worker.shutdown();
        }
        let index_worker = self.index_worker.lock().take();
        if let Some(index_worker) = index_worker {
            shutdown_and_join(index_worker).await?;
        }
        let search_worker = self.search_worker.lock().take();
        if let Some(mut search_worker) = search_worker {
            search_worker.shutdown();
        }
        let search_and_vector_bootstrap_worker =
            self.search_and_vector_bootstrap_worker.lock().take();
        if let Some(mut search_and_vector_bootstrap_worker) = search_and_vector_bootstrap_worker {
            search_and_vector_bootstrap_worker.shutdown();
        }
        let fast_forward_worker = self.fast_forward_worker.lock().take();
        if let Some(mut fast_forward_worker) = fast_forward_worker {
            fast_forward_worker.shutdown();
        }
        let export_worker = self.export_worker.lock().take();
        if let Some(export_worker) = export_worker {
            shutdown_and_join(export_worker).await?;
        }
        let snapshot_import_worker = self.snapshot_import_worker.lock().take();
        if let Some(snapshot_import_worker) = snapshot_import_worker {
            shutdown_and_join(snapshot_import_worker).await?;
        }
        let scheduled_job_runner = self.scheduled_job_runner.lock().take();
        if let Some(scheduled_job_runner) = scheduled_job_runner {
            scheduled_job_runner.shutdown();
        }
        let cron_job_executor = self.cron_job_executor.lock().take();
        if let Some(mut cron_job_executor) = cron_job_executor {
            cron_job_executor.shutdown();
        }
        let migration_worker = self.migration_worker.lock().take();
        if let Some(migration_worker) = migration_worker {
            shutdown_and_join(migration_worker).await?;
        }
        Ok(())
    }
}
