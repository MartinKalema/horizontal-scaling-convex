#![feature(try_blocks)]
#![feature(try_blocks_heterogeneous)]
#![feature(iterator_try_collect)]
#![feature(coroutines)]
#![feature(exhaustive_patterns)]

use std::{
    self,
    collections::BTreeMap,
    sync::Arc,
    time::Duration,
};

use ::authentication::{
    access_token_auth::NullAccessTokenAuth,
    application_auth::ApplicationAuth,
};
use ::storage::{
    LocalDirStorage,
    Storage,
    StorageUseCase,
};
use application::{
    self,
    api::ApplicationApi,
    log_visibility::RedactLogsToClient,
    Application,
    ApplicationWorkerStartupPolicy,
    QueryCache,
};
use common::{
    self,
    http::{
        fetch::ProxiedFetchClient,
        RouteMapper,
    },
    knobs::{
        ACTION_USER_TIMEOUT,
        DOCUMENT_RETENTION_RATE_LIMIT,
        UDF_CACHE_MAX_SIZE,
    },
    persistence::Persistence,
    runtime::{
        new_rate_limiter,
        Runtime,
    },
    shutdown::ShutdownSignal,
    types::{
        ConvexOrigin,
        ConvexSite,
        TEST_REGION_NAME,
    },
};
use config::LocalConfig;
use database::Database;
use events::usage::NoOpUsageEventLogger;
use exports::interface::InProcessExportProvider;
use file_storage::{
    FileStorage,
    TransactionalFileStorage,
};
use function_runner::{
    in_process_function_runner::InProcessFunctionRunner,
    server::DeploymentStorage,
    FunctionRunner,
};
use governor::Quota;
use http_client::CachedHttpClient;
use indexing::index_cache::SharedIndexCache;
use keybroker::InstanceSecret;
use model::{
    initialize_application_system_tables,
    virtual_system_mapping,
};
use node_executor::{
    local::LocalNodeExecutor,
    Actions,
};
use parking_lot::RwLock;
use runtime::prod::ProdRuntime;
use search::{
    searcher::InProcessSearcher,
    Searcher,
    SegmentTermMetadataFetcher,
};
use serde::Serialize;

pub mod admin;
mod app_metrics;
mod args_structs;
pub mod authentication;
pub mod beacon;
pub mod canonical_urls;
pub mod config;
pub mod custom_headers;
pub mod dashboard;
pub mod deploy_config;
pub mod deployment_info;
pub mod deployment_state;
pub mod environment_variables;
pub mod http_actions;
pub mod log_sinks;
pub mod logs;
mod metrics;
pub mod mutation_forwarder;
pub mod node_action_callbacks;
pub mod parse;
pub mod proxy;
pub mod public_api;
pub mod query_forwarding_api;
pub mod route_authority;
pub mod router;
pub mod scheduling;
pub mod schema;
pub mod snapshot_export;
pub mod snapshot_import;
pub mod storage;
pub mod streaming_export;
pub mod streaming_import;
pub mod subs;
#[cfg(test)]
mod test_helpers;
pub mod two_phase_service;

pub const MAX_CONCURRENT_REQUESTS: usize = 128;
const MEMBERSHIP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MEMBERSHIP_LEASE_TTL: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
pub struct SharedNodeAddresses(Arc<RwLock<Option<database::two_phase::NodeAddresses>>>);

impl SharedNodeAddresses {
    pub fn new(addresses: Option<database::two_phase::NodeAddresses>) -> Self {
        Self(Arc::new(RwLock::new(addresses)))
    }

    pub fn get(&self) -> Option<database::two_phase::NodeAddresses> {
        self.0.read().clone()
    }

    pub fn refresh_from_membership(&self, snapshot: &database::membership::MembershipSnapshot) {
        let addresses =
            snapshot.to_live_node_addresses(database::membership::current_unix_timestamp_millis());
        *self.0.write() = (!addresses.partitions().is_empty()).then_some(addresses);
    }
}

#[derive(Clone)]
pub struct BackendAppState {
    // Origin for the server (e.g. http://127.0.0.1:3210, https://demo.convex.cloud)
    pub origin: ConvexOrigin,
    // Origin for the corresponding convex.site (where we serve HTTP) (e.g. http://127.0.0.1:8001, https://crazy-giraffe-123.convex.site)
    pub site_origin: ConvexSite,
    // Name of the instance. (e.g. crazy-giraffe-123)
    pub instance_name: String,
    pub instance_secret: InstanceSecret,
    pub application: Application<ProdRuntime>,
    pub zombify_rx: async_broadcast::Receiver<()>,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub node_addresses: SharedNodeAddresses,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
    pub raft_peer_http_origins: Option<BTreeMap<u64, String>>,
    pub raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    pub cluster_grpc_auth: Option<common::grpc::ClusterGrpcAuth>,
    pub mutation_forwarder_pool: mutation_forwarder::MutationForwarderGrpcClientPool,
    pub replica_mutation_forwarder: Option<Arc<mutation_forwarder::MutationForwarderGrpcClient>>,
    pub placement_metadata_store: Option<Arc<dyn database::partition::PlacementMetadataStore>>,
    pub membership_store: Option<Arc<dyn database::membership::MembershipStore>>,
    /// Raft partition mailbox for receiving Raft messages from peers.
    /// None if Raft is not enabled.
    pub raft_mailbox_tx:
        Option<tokio::sync::mpsc::UnboundedSender<database::raft_node::RaftMessage>>,
}

impl BackendAppState {
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.application.shutdown().await?;

        Ok(())
    }
}

fn advertised_membership_node(
    config: &LocalConfig,
) -> anyhow::Result<Option<database::membership::NodeMembership>> {
    let Some(partition_id) = config.partition_id else {
        return Ok(None);
    };
    let Some(grpc_addr) = config.advertise_grpc_addr.as_deref() else {
        return Ok(None);
    };
    let node_id = config
        .cluster_node_id
        .clone()
        .or_else(|| {
            config.raft_node_id.map(|raft_node_id| {
                format!(
                    "partition-{partition_id}-peer-{}",
                    raft_node_id.saturating_sub(1)
                )
            })
        })
        .unwrap_or_else(|| config.name());
    let mut node = database::membership::NodeMembership::new(
        database::membership::ClusterNodeId::new(node_id)?,
        database::partition::PartitionId(partition_id),
        grpc_addr,
    )?;
    node.http_origin = match config.advertise_http_origin.clone() {
        Some(origin) => Some(origin),
        None => Some(config.convex_origin_url()?.to_string()),
    };
    node.raft_addr = config.advertise_raft_addr.clone();
    node.generation = config.cluster_node_generation;
    node.draining = config.cluster_node_draining;
    refresh_membership_lease(&mut node);
    Ok(Some(node))
}

fn refresh_membership_lease(node: &mut database::membership::NodeMembership) {
    let now_ms = database::membership::current_unix_timestamp_millis();
    let ttl_ms = MEMBERSHIP_LEASE_TTL
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    node.heartbeat_expires_at_ms = Some(now_ms.saturating_add(ttl_ms));
}

fn apply_membership_snapshot(
    database: &Database<ProdRuntime>,
    shared_node_addresses: &SharedNodeAddresses,
    snapshot: database::membership::MembershipSnapshot,
) -> anyhow::Result<()> {
    shared_node_addresses.refresh_from_membership(&snapshot);
    database
        .committer_client()
        .refresh_membership_snapshot(snapshot)?;
    Ok(())
}

fn start_membership_refresh_loop(
    runtime: ProdRuntime,
    database: Database<ProdRuntime>,
    store: Arc<dyn database::membership::MembershipStore>,
    shared_node_addresses: SharedNodeAddresses,
    advertised_node: Option<database::membership::NodeMembership>,
) {
    let refresh_runtime = runtime.clone();
    runtime.spawn_background("cluster_membership_refresh", async move {
        let mut backoff = Duration::from_millis(250);
        let mut last_live_addresses = None;
        loop {
            let refresh_result = async {
                let snapshot = if let Some(base_node) = advertised_node.as_ref() {
                    let mut node = base_node.clone();
                    refresh_membership_lease(&mut node);
                    let node_id = node.node_id.to_string();
                    let grpc_addr = node.grpc_addr.clone();
                    let snapshot = store.register_node(node).await?;
                    tracing::debug!(
                        node_id,
                        grpc_addr,
                        version = u64::from(snapshot.version()),
                        "Refreshed cluster membership heartbeat"
                    );
                    snapshot
                } else {
                    store.load().await?.ok_or_else(|| {
                        anyhow::anyhow!("Membership refresh found no current snapshot")
                    })?
                };
                let live_addresses = snapshot
                    .to_live_node_addresses(database::membership::current_unix_timestamp_millis());
                if last_live_addresses.as_ref() != Some(&live_addresses) {
                    tracing::info!(
                        version = u64::from(snapshot.version()),
                        ?live_addresses,
                        "Refreshed cluster membership snapshot"
                    );
                    last_live_addresses = Some(live_addresses);
                }
                apply_membership_snapshot(&database, &shared_node_addresses, snapshot)
            }
            .await;

            match refresh_result {
                Ok(()) => {
                    backoff = Duration::from_millis(250);
                    refresh_runtime.wait(MEMBERSHIP_HEARTBEAT_INTERVAL).await;
                },
                Err(e) => {
                    tracing::warn!(
                        "Cluster membership refresh failed; retrying after {:?}: {e:#}",
                        backoff
                    );
                    refresh_runtime.wait(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                },
            }
        }
    });
}

// Contains state needed to serve most http routes. Similar to BackendAppState,
// but uses ApplicationApi instead of Application, which allows it to be used
// in both Backend and Usher.
#[derive(Clone)]
pub struct RouterState {
    pub api: Arc<dyn ApplicationApi>,
    pub database: database::Database<ProdRuntime>,
    pub runtime: ProdRuntime,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub node_addresses: SharedNodeAddresses,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
    pub raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    pub cluster_grpc_auth: Option<common::grpc::ClusterGrpcAuth>,
    pub mutation_forwarder_pool: mutation_forwarder::MutationForwarderGrpcClientPool,
    pub replica_mutation_forwarder: Option<Arc<mutation_forwarder::MutationForwarderGrpcClient>>,
}

#[derive(Clone)]
pub struct DeployRouterState {
    pub instance_name: String,
    pub instance_secret: InstanceSecret,
    pub application: Application<ProdRuntime>,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub node_addresses: SharedNodeAddresses,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
    pub raft_peer_http_origins: Option<BTreeMap<u64, String>>,
}

impl From<&BackendAppState> for DeployRouterState {
    fn from(st: &BackendAppState) -> Self {
        Self {
            instance_name: st.instance_name.clone(),
            instance_secret: st.instance_secret,
            application: st.application.clone(),
            replica_mode: st.replica_mode,
            partition_id: st.partition_id,
            node_addresses: st.node_addresses.clone(),
            raft_state: st.raft_state.clone(),
            raft_peer_http_origins: st.raft_peer_http_origins.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AdminRouterState {
    pub origin: ConvexOrigin,
    pub site_origin: ConvexSite,
    pub instance_name: String,
    pub application: Application<ProdRuntime>,
    pub zombify_rx: async_broadcast::Receiver<()>,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
}

impl From<&BackendAppState> for AdminRouterState {
    fn from(st: &BackendAppState) -> Self {
        Self {
            origin: st.origin.clone(),
            site_origin: st.site_origin.clone(),
            instance_name: st.instance_name.clone(),
            application: st.application.clone(),
            zombify_rx: st.zombify_rx.clone(),
            replica_mode: st.replica_mode,
            partition_id: st.partition_id,
            raft_state: st.raft_state.clone(),
        }
    }
}

impl axum::extract::FromRef<BackendAppState> for AdminRouterState {
    fn from_ref(st: &BackendAppState) -> Self {
        Self::from(st)
    }
}

#[derive(Clone)]
pub struct ActionCallbackRouterState {
    pub application: Application<ProdRuntime>,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
}

impl From<&BackendAppState> for ActionCallbackRouterState {
    fn from(st: &BackendAppState) -> Self {
        Self {
            application: st.application.clone(),
            replica_mode: st.replica_mode,
            partition_id: st.partition_id,
            raft_state: st.raft_state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ImportExportRouterState {
    pub application: Application<ProdRuntime>,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
}

impl From<&BackendAppState> for ImportExportRouterState {
    fn from(st: &BackendAppState) -> Self {
        Self {
            application: st.application.clone(),
            replica_mode: st.replica_mode,
            partition_id: st.partition_id,
            raft_state: st.raft_state.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct EmptyResponse {}

fn replica_delta_consumer_start_ts(
    local_snapshot_ts: common::types::Timestamp,
    partition_id: Option<u32>,
) -> common::types::Timestamp {
    if partition_id.is_some() {
        // Partitioned nodes compare remote origin timestamps against this value
        // inside the NATS consumer. Until we have per-source durable replay
        // checkpoints, replay every remote partition delta instead of using a
        // local snapshot timestamp as a cross-node lower bound.
        common::types::Timestamp::MIN
    } else {
        local_snapshot_ts
    }
}

fn should_run_cluster_singleton_workers(
    replica_mode: bool,
    partition_id: Option<database::partition::PartitionId>,
    raft_state: Option<&database::raft_partition::RaftPartitionState>,
) -> bool {
    if !replica_mode && partition_id.is_none() {
        return true;
    }
    if replica_mode {
        return false;
    }
    let Some(partition_id) = partition_id else {
        return true;
    };
    if partition_id != route_authority::CLUSTER_COORDINATOR_PARTITION {
        return false;
    }
    raft_state
        .is_none_or(|raft_state| raft_state.is_leader() && raft_state.has_leader_serving_lease())
}

fn start_cluster_singleton_worker_supervisor(
    runtime: ProdRuntime,
    application: Application<ProdRuntime>,
    replica_mode: bool,
    partition_id: Option<database::partition::PartitionId>,
    raft_state: Option<database::raft_partition::RaftPartitionState>,
) {
    if !replica_mode && partition_id.is_none() {
        return;
    }
    let supervisor_runtime = runtime.clone();
    runtime.spawn_background("cluster_singleton_worker_authority", async move {
        let mut running = false;
        loop {
            let should_run = should_run_cluster_singleton_workers(
                replica_mode,
                partition_id,
                raft_state.as_ref(),
            );
            if should_run && !running {
                application.start_scheduled_and_cron_workers();
                running = true;
            } else if !should_run && running {
                application.stop_scheduled_and_cron_workers();
                running = false;
            }
            supervisor_runtime.wait(Duration::from_millis(250)).await;
        }
    });
}

fn application_worker_startup_policy(
    replication_mode: &str,
    partition_id: Option<database::partition::PartitionId>,
) -> ApplicationWorkerStartupPolicy {
    if replication_mode == "replica" || partition_id.is_some() {
        ApplicationWorkerStartupPolicy::clustered_fail_closed()
    } else {
        ApplicationWorkerStartupPolicy::single_node()
    }
}

pub async fn make_app(
    runtime: ProdRuntime,
    config: LocalConfig,
    persistence: Arc<dyn Persistence>,
    zombify_rx: async_broadcast::Receiver<()>,
    preempt_tx: ShutdownSignal,
) -> anyhow::Result<BackendAppState> {
    let key_broker = config.key_broker()?;
    let persistence_was_fresh = persistence.is_fresh();
    let in_process_searcher = Arc::new(InProcessSearcher::new(runtime.clone())?);
    let searcher: Arc<dyn Searcher> = in_process_searcher.clone();
    // TODO(CX-6572) Separate `SegmentMetadataFetcher` from `SearcherImpl`
    let segment_metadata_fetcher: Arc<dyn SegmentTermMetadataFetcher> = in_process_searcher;
    let (deleted_tablet_sender, deleted_tablet_receiver) = tokio::sync::mpsc::channel(100);
    let usage_event_logger = Arc::new(NoOpUsageEventLogger);
    // Set up distributed log based on replication mode.
    let distributed_log: Arc<dyn database::commit_delta::DistributedLog> = if let Some(nats_url) =
        &config.nats_url
    {
        let nats_config = database::nats_distributed_log::NatsConfig {
            url: nats_url.clone(),
            consumer_name: Some(config.name()),
            partition_id: config.partition_id,
        };
        Arc::new(database::nats_distributed_log::NatsDistributedLog::connect(nats_config).await?)
    } else {
        Arc::new(database::commit_delta::NoopDistributedLog)
    };

    // Set up the global Timestamp Oracle (TSO) for multi-node deployments.
    // Like TiDB's PD, this ensures globally unique timestamps across nodes.
    // In single-node mode, None falls back to the local clock.
    let timestamp_oracle: Option<Arc<dyn database::timestamp_oracle::TimestampOracle>> = if config
        .partition_id
        .is_some()
    {
        if let Some(nats_url) = &config.nats_url {
            let tso =
                database::timestamp_oracle::BatchTimestampOracle::connect(nats_url, None).await?;
            tracing::info!("Using BatchTimestampOracle (TiDB PD pattern) for global timestamps");
            Some(Arc::new(tso))
        } else {
            None
        }
    } else {
        None
    };

    let table_number_allocator: Arc<dyn database::TableNumberAllocator> =
        if config.partition_id.is_some() {
            if let Some(nats_url) = &config.nats_url {
                Arc::new(database::NatsTableNumberAllocator::connect(nats_url).await?)
            } else {
                Arc::new(database::LocalTableNumberAllocator)
            }
        } else {
            Arc::new(database::LocalTableNumberAllocator)
        };

    let two_phase_decision_log: Arc<dyn database::two_phase::TwoPhaseDecisionLog> =
        if config.partition_id.is_some() {
            if let Some(nats_url) = &config.nats_url {
                Arc::new(database::two_phase::NatsTwoPhaseDecisionLog::connect(nats_url).await?)
            } else {
                Arc::new(database::two_phase::NoopTwoPhaseDecisionLog)
            }
        } else {
            Arc::new(database::two_phase::NoopTwoPhaseDecisionLog)
        };

    let instance_secret = config.secret()?;
    let previous_cluster_grpc_secret = config.previous_cluster_grpc_secret()?;
    let cluster_grpc_auth = match previous_cluster_grpc_secret.as_ref() {
        Some(previous_secret) => common::grpc::ClusterGrpcAuth::from_shared_secret_with_previous(
            instance_secret.as_bytes(),
            previous_secret.as_bytes(),
        )?,
        None => common::grpc::ClusterGrpcAuth::from_shared_secret(instance_secret.as_bytes())?,
    };
    let mutation_forwarder_pool =
        mutation_forwarder::MutationForwarderGrpcClientPool::new(Some(cluster_grpc_auth.clone()));

    let replica_mutation_forwarder = if config.replication_mode == "replica" {
        match config.primary_grpc_url.as_deref() {
            Some(primary_grpc_url) => Some(mutation_forwarder_pool.client(primary_grpc_url).await?),
            None => None,
        }
    } else {
        None
    };
    let static_node_addresses = config
        .node_addresses
        .as_deref()
        .map(database::two_phase::NodeAddresses::from_config);
    let shared_node_addresses = SharedNodeAddresses::new(static_node_addresses.clone());
    let mut effective_node_addresses = static_node_addresses.clone();
    let static_placement_metadata = config.partition_id.map(|_| {
        let partition_map_str = config.partition_map.as_deref().unwrap_or("");
        let num_partitions = config.num_partitions.unwrap_or(1);
        database::partition::PlacementMetadata::from_static_config(
            database::partition::StaticPlacementConfig {
                table_assignments: partition_map_str,
                num_partitions,
                placement_version: database::partition::PlacementVersion::new(
                    config.partition_map_version.unwrap_or_default(),
                ),
            },
        )
    });

    let database = Database::load(
        persistence.clone(),
        runtime.clone(),
        searcher.clone(),
        preempt_tx.clone(),
        virtual_system_mapping().clone(),
        Some(SharedIndexCache),
        Arc::new(new_rate_limiter(
            runtime.clone(),
            Quota::per_second(*DOCUMENT_RETENTION_RATE_LIMIT),
        )),
        deleted_tablet_sender,
        distributed_log.clone(),
        config.replication_mode == "replica",
        config.partition_id.map(|id| {
            static_placement_metadata
                .as_ref()
                .expect("partitioned startup should have placement metadata")
                .into_partition_map(database::partition::PartitionId(id))
        }),
        static_node_addresses.clone(),
        two_phase_decision_log,
        Some(cluster_grpc_auth.clone()),
        timestamp_oracle.clone(),
        table_number_allocator,
        None, // raft_state: set after Raft node starts, not during Database::load
    )
    .await?;

    let placement_metadata_store: Option<Arc<dyn database::partition::PlacementMetadataStore>> =
        if let (Some(bootstrap_metadata), Some(nats_url)) = (
            static_placement_metadata.clone(),
            config.nats_url.as_deref(),
        ) {
            let store: Arc<dyn database::partition::PlacementMetadataStore> =
                Arc::new(database::partition::NatsPlacementMetadataStore::connect(nats_url).await?);
            let authoritative_metadata = store.ensure_initialized(bootstrap_metadata).await?;
            database
                .committer_client()
                .refresh_placement_metadata(authoritative_metadata)?;
            Some(store)
        } else {
            None
        };
    let membership_store: Option<Arc<dyn database::membership::MembershipStore>> =
        if let (Some(_), Some(nats_url)) = (config.partition_id, config.nats_url.as_deref()) {
            let store: Arc<dyn database::membership::MembershipStore> =
                Arc::new(database::membership::NatsMembershipStore::connect(nats_url).await?);
            let advertised_node = advertised_membership_node(&config)?;
            let seeded_snapshot =
                if let Some(static_node_addresses) = static_node_addresses.as_ref() {
                    let bootstrap_snapshot =
                        database::membership::MembershipSnapshot::from_node_addresses(
                            database::membership::MembershipVersion::BOOTSTRAP,
                            static_node_addresses,
                        );
                    Some(store.ensure_initialized(bootstrap_snapshot).await?)
                } else {
                    store.load().await?
                };
            let authoritative_snapshot = if let Some(advertised_node) = advertised_node.clone() {
                let node_id = advertised_node.node_id.to_string();
                let grpc_addr = advertised_node.grpc_addr.clone();
                let snapshot = store.register_node(advertised_node).await?;
                tracing::info!(
                    node_id,
                    grpc_addr,
                    version = u64::from(snapshot.version()),
                    "Registered cluster membership node"
                );
                snapshot
            } else {
                seeded_snapshot.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Partitioned startup requires NODE_ADDRESSES or ADVERTISE_GRPC_ADDR until \
                         the shared membership directory has been initialized"
                    )
                })?
            };
            apply_membership_snapshot(
                &database,
                &shared_node_addresses,
                authoritative_snapshot.clone(),
            )?;
            effective_node_addresses = shared_node_addresses.get();
            start_membership_refresh_loop(
                runtime.clone(),
                database.clone(),
                store.clone(),
                shared_node_addresses.clone(),
                advertised_node,
            );
            Some(store)
        } else {
            None
        };

    if config.replication_mode == "replica" && persistence_was_fresh {
        let checkpoint_path = config.checkpoint_storage_path.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "CHECKPOINT_STORAGE_PATH is required to bootstrap a fresh replica from checkpoint"
            )
        })?;
        let checkpoint_storage: Arc<dyn Storage> = Arc::new(LocalDirStorage::for_use_case(
            runtime.clone(),
            checkpoint_path,
            StorageUseCase::Checkpoints,
        )?);
        let checkpoint = match database::snapshot_checkpointer::load_latest_checkpoint(
            &checkpoint_storage,
        )
        .await
        {
            Ok(Some(checkpoint)) => checkpoint,
            Ok(None) => {
                metrics::log_replica_bootstrap_result(false);
                anyhow::bail!(
                    "Fresh replica startup requires a checkpoint, but none was found at {}",
                    checkpoint_path
                );
            },
            Err(e) => {
                metrics::log_replica_bootstrap_result(false);
                return Err(e);
            },
        };
        let checkpoint_ts = checkpoint.timestamp;
        if let Err(e) = database
            .committer_client()
            .install_snapshot(checkpoint)
            .await
        {
            metrics::log_replica_bootstrap_result(false);
            return Err(e);
        }
        metrics::log_replica_bootstrap_result(true);
        tracing::info!("Bootstrapped fresh replica from checkpoint at ts={checkpoint_ts}");
    }

    initialize_application_system_tables(&database).await?;
    let application_storage = if config.replication_mode == "replica" {
        // Replica uses local storage — doesn't write storage config to DB.
        let replica_path = config
            .replica_storage_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("REPLICA_STORAGE_PATH is required in replica mode"))?;
        let (storage, search_storage) =
            application::ApplicationStorage::new_local(runtime.clone(), replica_path)?;
        database.set_search_storage(search_storage);
        storage
    } else {
        Application::initialize_storage(
            runtime.clone(),
            &database,
            config.storage_tag_initializer(),
            config.name(),
        )
        .await?
    };

    let file_storage = FileStorage {
        transactional_file_storage: TransactionalFileStorage::new(
            runtime.clone(),
            application_storage.files_storage.clone(),
            config.convex_origin_url()?,
        ),
        database: database.clone(),
    };

    // Start the SnapshotCheckpointer on the Primary when NATS is configured.
    if config.replication_mode == "primary" && config.nats_url.is_some() {
        let checkpoint_path = config.checkpoint_storage_path.as_ref().ok_or_else(|| {
            anyhow::anyhow!("CHECKPOINT_STORAGE_PATH is required in primary mode with NATS_URL")
        })?;
        let checkpoint_storage: Arc<dyn Storage> = Arc::new(LocalDirStorage::for_use_case(
            runtime.clone(),
            checkpoint_path,
            StorageUseCase::Checkpoints,
        )?);
        let _checkpointer = database::snapshot_checkpointer::SnapshotCheckpointer::start(
            runtime.clone(),
            persistence.reader(),
            database.retention_validator(),
            checkpoint_storage,
        );
        tracing::info!("Started SnapshotCheckpointer for replication");
    }

    let node_process_timeout = *ACTION_USER_TIMEOUT + Duration::from_secs(5);
    let node_executor = Arc::new(LocalNodeExecutor::new(node_process_timeout).await?);
    let actions = Actions::new(
        node_executor,
        config.convex_origin_url()?,
        *ACTION_USER_TIMEOUT,
        runtime.clone(),
    );

    #[cfg(not(debug_assertions))]
    if config.convex_http_proxy.is_none() {
        tracing::warn!(
            "Running without a proxy in release mode -- UDF `fetch` requests are unrestricted!"
        );
    }
    let fetch_client = Arc::new(ProxiedFetchClient::new(
        config.convex_http_proxy.clone(),
        config.name(),
        reqwest::redirect::Policy::none(),
    ));
    let oidc_http_client = CachedHttpClient::new(
        config.convex_http_proxy.clone(),
        config.name(),
        reqwest::redirect::Policy::default(),
    );
    let function_runner: Arc<dyn FunctionRunner<ProdRuntime>> =
        Arc::new(InProcessFunctionRunner::new(
            config.name().clone(),
            key_broker.function_runner_keybroker(),
            config.convex_origin_url()?,
            runtime.clone(),
            persistence.reader(),
            DeploymentStorage {
                files_storage: application_storage.files_storage.clone(),
                modules_storage: application_storage.modules_storage.clone(),
            },
            database.clone(),
            fetch_client.clone(),
        )?);

    let worker_startup_policy = application_worker_startup_policy(
        &config.replication_mode,
        config.partition_id.map(database::partition::PartitionId),
    );
    let application = Application::new(
        runtime.clone(),
        database.clone(),
        file_storage.clone(),
        application_storage,
        usage_event_logger,
        key_broker.clone(),
        config.name(),
        Some(TEST_REGION_NAME.clone()),
        function_runner,
        config.convex_origin_url()?,
        config.convex_site_url()?,
        searcher.clone(),
        segment_metadata_fetcher,
        persistence,
        actions,
        Arc::new(RedactLogsToClient::new(config.redact_logs_to_client)),
        Arc::new(ApplicationAuth::new(
            key_broker.clone(),
            Arc::new(NullAccessTokenAuth),
        )),
        QueryCache::new(*UDF_CACHE_MAX_SIZE),
        fetch_client,
        config.local_log_sink.clone(),
        preempt_tx.clone(),
        Arc::new(InProcessExportProvider),
        deleted_tablet_receiver,
        oidc_http_client,
        worker_startup_policy,
    )
    .await?;

    let origin = config.convex_origin_url()?;
    let instance_name = config.name();
    let partition_id = config.partition_id.map(database::partition::PartitionId);
    let node_addresses = shared_node_addresses.clone();
    let mut raft_state_for_app = None;
    let mut raft_peer_http_origins = None;
    let mut raft_peer_grpc_urls = None;

    if !config.disable_beacon {
        let beacon_future = beacon::start_beacon(
            runtime.clone(),
            database.clone(),
            config.convex_http_proxy.clone(),
            config.name(),
            config.beacon_tag.clone(),
            config.beacon_fields.clone(),
        );
        runtime.spawn_background("beacon_worker", beacon_future);
    }

    if let Some(nats_url) = &config.nats_url {
        let nats_url = nats_url.clone();
        let registry_node_name = config.name();
        let delta_interest_tracker = database.delta_interest_tracker();
        let mut interest_rx = delta_interest_tracker.watch();
        runtime.spawn_background("selective_delivery_interest_publisher", async move {
            let registry = match database::selective_delivery::SelectiveDeliveryRegistry::connect(
                &nats_url,
                registry_node_name.clone(),
            )
            .await
            {
                Ok(registry) => registry,
                Err(e) => {
                    tracing::warn!(
                        "Failed to start selective-delivery interest publisher for {}: {e:#}",
                        registry_node_name,
                    );
                    return;
                },
            };

            loop {
                delta_interest_tracker.prune_expired();
                let interest_snapshot = interest_rx.borrow().clone();
                if let Err(e) = registry.publish_local_interest(&interest_snapshot).await {
                    tracing::warn!(
                        "Failed to publish selective-delivery interest for {}: {e:#}",
                        registry_node_name,
                    );
                }
                tokio::select! {
                    changed = interest_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {},
                }
            }
        });
    }

    if config.partition_id.is_some() {
        if let Some(nats_url) = &config.nats_url {
            let nats_url = nats_url.clone();
            let shadow_node_name = config.name();
            let shadow_from_ts = *database.now_ts_for_reads();
            runtime.spawn_background("selective_delivery_shadow_consumer", async move {
                let consumer = match database::nats_distributed_log::NatsDistributedLog::connect(
                    database::nats_distributed_log::NatsConfig {
                        url: nats_url,
                        consumer_name: Some(format!("{shadow_node_name}-selective-shadow")),
                        partition_id: None,
                    },
                )
                .await
                {
                    Ok(consumer) => consumer,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to start selective-delivery shadow consumer for {}: {e:#}",
                            shadow_node_name,
                        );
                        return;
                    },
                };

                match consumer
                    .subscribe_node_targeted_shadow(shadow_from_ts, &shadow_node_name)
                    .await
                {
                    Ok(mut stream) => {
                        tracing::info!(
                            "Selective-delivery shadow consumer subscribed for {}",
                            shadow_node_name
                        );
                        while let Some(result) = futures::StreamExt::next(&mut stream).await {
                            match result {
                                Ok(message) => {
                                    let (delta, _transport_id, ack) = message.into_parts();
                                    database::log_selective_delivery_shadow_receive();
                                    tracing::debug!(
                                        "Selective-delivery shadow delta observed for {} at ts={}",
                                        shadow_node_name,
                                        u64::from(delta.ts),
                                    );
                                    if let Err(e) = ack.ack().await {
                                        tracing::warn!(
                                            "Selective-delivery shadow consumer failed to ack for \
                                             {} at ts={}: {e:#}",
                                            shadow_node_name,
                                            u64::from(delta.ts),
                                        );
                                        break;
                                    }
                                },
                                Err(e) => {
                                    tracing::warn!(
                                        "Selective-delivery shadow consumer error for {}: {e:#}",
                                        shadow_node_name,
                                    );
                                    break;
                                },
                            }
                        }
                    },
                    Err(e) => tracing::warn!(
                        "Failed to subscribe selective-delivery shadow consumer for {}: {e:#}",
                        shadow_node_name,
                    ),
                }
            });
        }
    }

    // Start the ReplicaDeltaConsumer to tail NATS and apply deltas from other
    // nodes. Runs on:
    //   - Replicas (REPLICATION_MODE=replica): consumes Primary's deltas
    //   - Partitioned writers (PARTITION_ID set): consumes only other partitions'
    //     deltas, leaving same-partition convergence to Raft
    // Creates a fresh NATS connection dedicated to the consumer.
    let needs_delta_consumer =
        config.replication_mode == "replica" || config.partition_id.is_some();
    if needs_delta_consumer {
        if let Some(nats_url) = &config.nats_url {
            let nats_url = nats_url.clone();
            let committer = database.committer_client();
            let remote_partitions = config.partition_id.map(|local_partition| {
                if let Some(num_partitions) = config.num_partitions {
                    (0..num_partitions)
                        .map(database::partition::PartitionId)
                        .filter(|partition| partition.0 != local_partition)
                        .collect::<Vec<_>>()
                } else {
                    effective_node_addresses
                        .as_ref()
                        .map(|addresses| {
                            addresses
                                .partitions()
                                .into_iter()
                                .filter(|partition| partition.0 != local_partition)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                }
            });
            // In partitioned mode, start consuming from the beginning of the
            // stream. Each node has its own database with independent local
            // apply timestamps, so local snapshot timestamps are not safe lower
            // bounds for remote origin deltas. We explicitly restrict
            // partitioned nodes to other partitions' subjects so same-partition
            // convergence comes from Raft apply. In replica mode, start from the
            // locally visible snapshot, which may have just been bootstrapped
            // from the latest checkpoint.
            let from_ts =
                replica_delta_consumer_start_ts(*database.now_ts_for_reads(), config.partition_id);
            let consumer_name = config.name();
            let consumer_runtime = runtime.clone();
            runtime.spawn_background("replica_delta_consumer_setup", async move {
                let mut backoff = Duration::from_millis(250);
                loop {
                    tracing::info!(
                        delivery_mode = "broad_partition_subjects",
                        ?remote_partitions,
                        "ReplicaDeltaConsumer subscribing to NATS..."
                    );
                    let subscribe_result =
                        match database::nats_distributed_log::NatsDistributedLog::connect(
                            database::nats_distributed_log::NatsConfig {
                                url: nats_url.clone(),
                                consumer_name: Some(consumer_name.clone()),
                                partition_id: None,
                            },
                        )
                        .await
                        {
                            Ok(consumer) => {
                                let consumer_nats_dyn: Arc<
                                    dyn database::commit_delta::DistributedLog,
                                > = Arc::new(consumer);
                                consumer_nats_dyn
                                    .subscribe_filtered(from_ts.into(), remote_partitions.clone())
                                    .await
                            },
                            Err(e) => Err(e),
                        };
                    match subscribe_result {
                        Ok(stream) => {
                            tracing::info!("ReplicaDeltaConsumer subscribed, processing deltas...");
                            let result = database::replica::consume_replication_stream(
                                stream,
                                committer.clone(),
                                |ts, n| {
                                    tracing::info!(
                                        "Applied replica delta: ts={}, {} updates",
                                        u64::from(ts),
                                        n
                                    );
                                },
                            )
                            .await;
                            if let Err(e) = result {
                                tracing::error!(
                                    "ReplicaDeltaConsumer stream failed; restarting after {:?}: \
                                     {e:#}",
                                    backoff,
                                );
                            } else {
                                tracing::warn!(
                                    "ReplicaDeltaConsumer stream ended; restarting after {:?}",
                                    backoff,
                                );
                            }
                        },
                        Err(e) => {
                            tracing::error!(
                                "Failed to subscribe to NATS; retrying after {:?}: {e:#}",
                                backoff,
                            );
                        },
                    }
                    consumer_runtime.wait(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            });
            tracing::info!("Started ReplicaDeltaConsumer for replication");
        }
    }

    // Start the 2PC Transaction Watcher for crash recovery.
    if let Some(partition_id) = config.partition_id {
        if let Some(nats_url) = &config.nats_url {
            database::two_phase_watcher::start(
                runtime.clone(),
                database.committer_client(),
                nats_url.clone(),
                database::partition::PartitionId(partition_id),
            );
            tracing::info!("Started 2PC Transaction Watcher");
        }
    }

    let mut raft_mailbox_tx: Option<
        tokio::sync::mpsc::UnboundedSender<database::raft_node::RaftMessage>,
    > = None;

    // Start the Raft consensus node for this partition if configured.
    // When RAFT_NODE_ID and RAFT_PEERS are set, this node joins a Raft group
    // for its partition. Leadership changes activate/deactivate the Committer.
    if let (Some(raft_node_id), Some(raft_peers_str)) =
        (config.raft_node_id, config.raft_peers.as_deref())
    {
        use database::{
            raft_node::RaftNodeConfig,
            raft_partition::RaftPartitionManager,
            raft_state_machine::RaftStateMachineEntry,
            raft_storage::ConvexRaftStorage,
            raft_transport,
        };

        let partition_id = database::partition::PartitionId(config.partition_id.unwrap_or(0));

        // Parse peer addresses: "1=host:port,2=host:port,3=host:port"
        let mut peer_addresses = std::collections::HashMap::new();
        let mut peer_http_origins = BTreeMap::new();
        let mut peer_grpc_urls = BTreeMap::new();
        let mut peer_ids = Vec::new();
        for pair in raft_peers_str.split(',') {
            let pair = pair.trim();
            if let Some((id_str, addr)) = pair.split_once('=') {
                if let Ok(id) = id_str.trim().parse::<u64>() {
                    let normalized_grpc_addr = if addr.contains("://") {
                        addr.trim().to_string()
                    } else {
                        format!("http://{}", addr.trim())
                    };
                    peer_addresses.insert(id, normalized_grpc_addr.clone());
                    peer_grpc_urls.insert(id, normalized_grpc_addr);
                    if let Ok(origin) =
                        crate::deploy_config::http_origin_from_peer_addr(addr.trim())
                    {
                        peer_http_origins.insert(id, origin);
                    }
                    peer_ids.push(id);
                }
            }
        }

        let raft_config = RaftNodeConfig {
            node_id: raft_node_id,
            partition_id,
            peers: peer_ids,
            election_tick: 10, // 1 second
            heartbeat_tick: 3, // 300ms
        };

        // Open raft-engine for persistent Raft log storage (TiKV pattern).
        // One engine per node, shared across all partitions. Data survives
        // restarts — this prevents the "to_commit X out of range" panic
        // that MemStorage caused.
        let raft_engine_path = "/convex/data/raft-engine";
        let raft_engine = ConvexRaftStorage::open_engine(raft_engine_path)?;

        // Create transport channels for peer communication.
        let (peer_senders, transport_clients) = raft_transport::create_transport(
            &peer_addresses,
            raft_node_id,
            Some(cluster_grpc_auth.clone()),
        );

        let snapshot_provider = {
            let database = database.clone();
            Arc::new(move || {
                common::runtime::block_in_place(|| {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async { database.build_raft_snapshot_bytes().await })
                })
            }) as Arc<dyn database::raft_storage::RaftSnapshotProvider>
        };
        let mut manager = RaftPartitionManager::new(
            raft_config,
            raft_engine,
            peer_senders,
            Some(snapshot_provider),
            timestamp_oracle.clone(),
        )?;
        let raft_state = manager.state();
        raft_state_for_app = Some(raft_state.clone());
        raft_peer_http_origins = Some(peer_http_origins);
        raft_peer_grpc_urls = Some(peer_grpc_urls);
        let mb_tx = manager.mailbox_tx();
        raft_mailbox_tx = Some(mb_tx);

        // Start the Raft node in a background task.
        // TiKV Apply Worker pattern: committed Raft entries are applied
        // to the state machine. On the proposer, the future waits for this
        // Raft decision before applying locally, so on_committed skips live
        // local proposals. Followers deserialize typed entries and install
        // either commit deltas or 2PC prepare redo records through the
        // Committer loop.
        if let Some(mut node) = manager.take_node() {
            let committer = database.committer_client();
            let raft_state_for_apply = raft_state.clone();
            runtime.spawn_background("raft_node", async move {
                let committer_for_entries = committer.clone();
                let raft_state_for_entries = raft_state_for_apply.clone();
                let committer_for_snapshots = committer.clone();
                if let Err(e) = node
                    .run(
                        move |data| {
                            match RaftStateMachineEntry::from_bytes(data)? {
                                RaftStateMachineEntry::CommitDelta { envelope } => {
                                    let proposed_locally =
                                        envelope.source_raft_node_id() == Some(raft_node_id);
                                    let delta = envelope.to_delta()?;

                                    tracing::info!(
                                        "Applying committed Raft delta locally: ts={}, \
                                         proposed_locally={}, leader_now={}",
                                        u64::from(delta.ts),
                                        proposed_locally,
                                        raft_state_for_entries.is_leader(),
                                    );
                                    let committer = committer_for_entries.clone();
                                    let rt = tokio::runtime::Handle::current();
                                    rt.block_on(async {
                                        committer.apply_raft_commit_delta(delta).await.map_err(
                                            |e| {
                                                anyhow::anyhow!(
                                                    "Raft state-machine apply failed: {e:#}"
                                                )
                                            },
                                        )
                                    })?;
                                },
                                RaftStateMachineEntry::TwoPhasePrepare { redo } => {
                                    tracing::info!(
                                        "Applying committed Raft 2PC prepare locally: txn={}, \
                                         leader_now={}",
                                        redo.transaction_id,
                                        raft_state_for_entries.is_leader(),
                                    );
                                    let committer = committer_for_entries.clone();
                                    let rt = tokio::runtime::Handle::current();
                                    rt.block_on(async {
                                        committer.apply_raft_prepared_redo(redo).await.map_err(
                                            |e| {
                                                anyhow::anyhow!(
                                                    "Raft 2PC prepare apply failed: {e:#}"
                                                )
                                            },
                                        )
                                    })?;
                                },
                            }

                            Ok(())
                        },
                        move |snapshot_bytes| {
                            let checkpoint =
                                database::snapshot_checkpointer::checkpoint_from_bytes(
                                    snapshot_bytes,
                                )?;
                            let committer = committer_for_snapshots.clone();
                            let rt = tokio::runtime::Handle::current();
                            rt.block_on(async {
                                committer.install_snapshot(checkpoint).await?;
                                Ok::<_, anyhow::Error>(())
                            })
                        },
                    )
                    .await
                {
                    tracing::error!("Raft node stopped after fatal error: {e:#}");
                    panic!("Raft node stopped after fatal error: {e:#}");
                }
            });
        }

        // Defer leader enforcement until after local database/application
        // bootstrap is complete, then attach the live Raft state to the
        // Committer so normal writes propose through Raft and followers reject
        // non-leader writes.
        database.attach_raft_state(raft_state.clone()).await?;

        // Start transport clients for each peer.
        for client in transport_clients {
            runtime.spawn_background("raft_transport_client", async move {
                client.run().await;
            });
        }

        tracing::info!(
            "Started Raft node {} for partition {} with {} peers",
            raft_node_id,
            partition_id,
            peer_addresses.len(),
        );
    }

    start_cluster_singleton_worker_supervisor(
        runtime.clone(),
        application.clone(),
        config.replication_mode == "replica",
        partition_id,
        raft_state_for_app.clone(),
    );

    let app_state = BackendAppState {
        origin,
        site_origin: config.convex_site_url()?,
        instance_name,
        instance_secret,
        application,
        zombify_rx,
        replica_mode: config.replication_mode == "replica",
        partition_id,
        node_addresses,
        raft_state: raft_state_for_app,
        raft_peer_http_origins,
        raft_peer_grpc_urls,
        replica_mutation_forwarder,
        mutation_forwarder_pool,
        placement_metadata_store,
        membership_store,
        raft_mailbox_tx,
        cluster_grpc_auth: Some(cluster_grpc_auth),
    };

    Ok(app_state)
}

#[derive(Clone)]
pub struct HttpActionRouteMapper;

impl RouteMapper for HttpActionRouteMapper {
    fn map_route(&self, route: String) -> String {
        // Backend can receive arbitrary HTTP requests, so group all of these
        // under one tag.
        if route.starts_with("/http/") {
            "/http/:user_http_action".into()
        } else {
            route
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::Arc,
    };

    use common::{
        assert_obj,
        persistence::{
            NoopRetentionValidator,
            PersistenceReader,
            TimestampRange,
        },
        query::Order,
        shutdown::ShutdownSignal,
        testing::TestPersistence,
        types::Timestamp,
    };
    use futures::TryStreamExt;
    use keybroker::Identity;
    use runtime::prod::ProdRuntime;
    use storage::{
        LocalDirStorage,
        Storage,
        StorageUseCase,
        Upload,
    };
    use value::TableName;

    use crate::{
        config::LocalConfig,
        make_app,
    };

    #[test]
    fn partitioned_replica_consumers_do_not_start_from_local_snapshot_ts() -> anyhow::Result<()> {
        let local_snapshot_ts = Timestamp::try_from(1_000_000u64)?;

        assert_eq!(
            super::replica_delta_consumer_start_ts(local_snapshot_ts, Some(0)),
            Timestamp::MIN,
        );
        assert_eq!(
            super::replica_delta_consumer_start_ts(local_snapshot_ts, Some(1)),
            Timestamp::MIN,
        );
        assert_eq!(
            super::replica_delta_consumer_start_ts(local_snapshot_ts, None),
            local_snapshot_ts,
        );

        Ok(())
    }

    #[test]
    fn cluster_singleton_workers_follow_coordinator_authority() {
        assert!(super::should_run_cluster_singleton_workers(
            false, None, None,
        ));
        assert!(!super::should_run_cluster_singleton_workers(
            true, None, None,
        ));
        assert!(super::should_run_cluster_singleton_workers(
            false,
            Some(database::partition::PartitionId(0)),
            None,
        ));
        assert!(!super::should_run_cluster_singleton_workers(
            false,
            Some(database::partition::PartitionId(1)),
            None,
        ));
        let raft_state = database::raft_partition::RaftPartitionState::new_for_test(
            true,
            1,
            database::partition::PartitionId(0),
            1,
        );
        assert!(super::should_run_cluster_singleton_workers(
            false,
            Some(database::partition::PartitionId(0)),
            Some(&raft_state),
        ));
        raft_state.expire_leader_serving_lease_for_test();
        assert!(!super::should_run_cluster_singleton_workers(
            false,
            Some(database::partition::PartitionId(0)),
            Some(&raft_state),
        ));
    }

    #[test]
    fn application_workers_fail_closed_for_clustered_nodes() {
        assert_eq!(
            super::application_worker_startup_policy("primary", None),
            application::ApplicationWorkerStartupPolicy::single_node(),
        );
        assert_eq!(
            super::application_worker_startup_policy("replica", None),
            application::ApplicationWorkerStartupPolicy::clustered_fail_closed(),
        );
        assert_eq!(
            super::application_worker_startup_policy(
                "primary",
                Some(database::partition::PartitionId(0)),
            ),
            application::ApplicationWorkerStartupPolicy::clustered_fail_closed(),
        );
        assert_eq!(
            super::application_worker_startup_policy(
                "primary",
                Some(database::partition::PartitionId(1)),
            ),
            application::ApplicationWorkerStartupPolicy::clustered_fail_closed(),
        );
    }

    #[test]
    fn test_fresh_replica_bootstraps_from_checkpoint() -> anyhow::Result<()> {
        let tokio = ProdRuntime::init_tokio()?;
        let runtime = ProdRuntime::new(&tokio);
        let test_runtime = runtime.clone();
        runtime.block_on(
            "test_fresh_replica_bootstraps_from_checkpoint",
            async move {
                let primary_persistence = Arc::new(TestPersistence::new());
                let (_primary_shutdown_tx, primary_shutdown_rx) = async_broadcast::broadcast(1);
                let primary = make_app(
                    test_runtime.clone(),
                    LocalConfig::new_for_test()?,
                    primary_persistence.clone(),
                    primary_shutdown_rx,
                    ShutdownSignal::no_op(),
                )
                .await?;

                let table_name: TableName = "bootstrap_messages".parse()?;
                let mut tx = primary
                    .application
                    .database()
                    .begin(Identity::system())
                    .await?;
                database::TestFacingModel::new(&mut tx)
                    .insert(&table_name, assert_obj!("text" => "hello from checkpoint"))
                    .await?;
                primary.application.database().commit(tx).await?;

                let checkpoint = primary
                    .application
                    .database()
                    .build_raft_snapshot_checkpoint()
                    .await?;
                let checkpoint_dir = tempfile::tempdir()?;
                let checkpoint_storage: Arc<dyn Storage> = Arc::new(LocalDirStorage::for_use_case(
                    test_runtime.clone(),
                    checkpoint_dir.path().to_str().unwrap(),
                    StorageUseCase::Checkpoints,
                )?);
                let mut upload = checkpoint_storage
                    .start_upload_with_key(
                        database::snapshot_checkpointer::LATEST_CHECKPOINT_KEY.try_into()?,
                    )
                    .await?;
                upload
                    .write(
                        database::snapshot_checkpointer::checkpoint_to_bytes(&checkpoint)?.into(),
                    )
                    .await?;
                let _ = upload.complete().await?;

                let replica_persistence = Arc::new(TestPersistence::new());
                let replica_storage_dir = tempfile::tempdir()?;
                let (_shutdown_tx, shutdown_rx) = async_broadcast::broadcast(1);
                let mut config = LocalConfig::new_for_test()?;
                config.replication_mode = "replica".into();
                config.replica_storage_path =
                    Some(replica_storage_dir.path().to_str().unwrap().into());
                config.checkpoint_storage_path =
                    Some(checkpoint_dir.path().to_str().unwrap().into());

                let st = make_app(
                    test_runtime.clone(),
                    config,
                    replica_persistence.clone(),
                    shutdown_rx,
                    ShutdownSignal::no_op(),
                )
                .await?;

                assert_eq!(
                    *st.application.database().now_ts_for_reads(),
                    checkpoint.timestamp
                );

                let replica_docs: Vec<_> = replica_persistence
                    .load_documents(
                        TimestampRange::all(),
                        Order::Asc,
                        10_000,
                        Arc::new(NoopRetentionValidator),
                    )
                    .try_collect()
                    .await?;
                let expected_latest_docs = checkpoint
                    .documents
                    .iter()
                    .fold(BTreeMap::new(), |mut acc, entry| {
                        acc.insert(entry.id, entry.clone());
                        acc
                    })
                    .into_values()
                    .filter(|entry| entry.value.is_some())
                    .count();
                assert_eq!(replica_docs.len(), expected_latest_docs);

                primary.shutdown().await?;
                st.shutdown().await?;
                Ok(())
            },
        )
    }
}
