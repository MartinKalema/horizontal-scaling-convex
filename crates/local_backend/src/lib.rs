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
pub mod deploy_config2;
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

#[derive(Clone)]
pub struct LocalAppState {
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
    pub node_addresses: Option<database::two_phase::NodeAddresses>,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
    pub raft_peer_http_origins: Option<BTreeMap<u64, String>>,
    pub raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    pub replica_mutation_forwarder: Option<Arc<mutation_forwarder::MutationForwarderGrpcClient>>,
    /// Raft partition mailbox for receiving Raft messages from peers.
    /// None if Raft is not enabled.
    pub raft_mailbox_tx:
        Option<tokio::sync::mpsc::UnboundedSender<database::raft_node::RaftMessage>>,
}

impl LocalAppState {
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.application.shutdown().await?;

        Ok(())
    }
}

// Contains state needed to serve most http routes. Similar to LocalAppState,
// but uses ApplicationApi instead of Application, which allows it to be used
// in both Backend and Usher.
#[derive(Clone)]
pub struct RouterState {
    pub api: Arc<dyn ApplicationApi>,
    pub database: database::Database<ProdRuntime>,
    pub runtime: ProdRuntime,
    pub replica_mode: bool,
    pub partition_id: Option<database::partition::PartitionId>,
    pub node_addresses: Option<database::two_phase::NodeAddresses>,
    pub raft_state: Option<database::raft_partition::RaftPartitionState>,
    pub raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    pub replica_mutation_forwarder: Option<Arc<mutation_forwarder::MutationForwarderGrpcClient>>,
}

#[derive(Serialize)]
pub struct EmptyResponse {}

pub async fn make_app(
    runtime: ProdRuntime,
    config: LocalConfig,
    persistence: Arc<dyn Persistence>,
    zombify_rx: async_broadcast::Receiver<()>,
    preempt_tx: ShutdownSignal,
) -> anyhow::Result<LocalAppState> {
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

    let replica_mutation_forwarder = if config.replication_mode == "replica" {
        match config.primary_grpc_url.as_deref() {
            Some(primary_grpc_url) => Some(Arc::new(
                mutation_forwarder::MutationForwarderGrpcClient::connect(primary_grpc_url).await?,
            )),
            None => None,
        }
    } else {
        None
    };

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
            let partition_map_str = config.partition_map.as_deref().unwrap_or("");
            let num_partitions = config.num_partitions.unwrap_or(1);
            database::partition::PartitionMap::from_config(
                partition_map_str,
                database::partition::PartitionId(id),
                num_partitions,
            )
        }),
        config
            .node_addresses
            .as_deref()
            .map(database::two_phase::NodeAddresses::from_config),
        two_phase_decision_log,
        timestamp_oracle,
        None, // raft_state: set after Raft node starts, not during Database::load
    )
    .await?;

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
    )
    .await?;

    let origin = config.convex_origin_url()?;
    let instance_name = config.name();
    let instance_secret = config.secret()?;
    let partition_id = config.partition_id.map(database::partition::PartitionId);
    let node_addresses = config
        .node_addresses
        .as_deref()
        .map(database::two_phase::NodeAddresses::from_config);
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

    if config.partition_id == Some(0) {
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
                    .subscribe_node_targeted(shadow_from_ts, &shadow_node_name)
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
                                    let (delta, ack) = message.into_parts();
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
            let use_selective_node_targeting = config
                .partition_id
                .is_some_and(|local_partition| local_partition != 0);
            let remote_partitions = config.partition_id.map(|local_partition| {
                if let Some(num_partitions) = config.num_partitions {
                    (0..num_partitions)
                        .map(database::partition::PartitionId)
                        .filter(|partition| partition.0 != local_partition)
                        .collect::<Vec<_>>()
                } else {
                    config
                        .node_addresses
                        .as_deref()
                        .map(database::two_phase::NodeAddresses::from_config)
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
            // stream. Each node has its own database with independent timestamps,
            // so we can't use the local database timestamp as a lower bound.
            // We explicitly restrict partitioned nodes to other partitions'
            // subjects so same-partition convergence comes from Raft apply.
            // In replica mode, start from the locally visible snapshot, which
            // may have just been bootstrapped from the latest checkpoint.
            let from_ts = if use_selective_node_targeting {
                *database.now_ts_for_reads()
            } else if config.partition_id.is_some() {
                common::types::Timestamp::MIN
            } else {
                *database.now_ts_for_reads()
            };
            let consumer_name = config.name();
            let broad_consumer_name = consumer_name.clone();
            let broad_nats_url = nats_url.clone();
            runtime.spawn_background("replica_delta_consumer_setup", async move {
                let consumer_nats =
                    match database::nats_distributed_log::NatsDistributedLog::connect(
                        database::nats_distributed_log::NatsConfig {
                            url: broad_nats_url,
                            consumer_name: Some(broad_consumer_name),
                            partition_id: None,
                        },
                    )
                    .await
                    {
                        Ok(n) => Arc::new(n),
                        Err(e) => {
                            tracing::error!(
                                "Failed to connect NATS for ReplicaDeltaConsumer: {e:#}"
                            );
                            return;
                        },
                    };
                let consumer_nats_dyn: Arc<dyn database::commit_delta::DistributedLog> =
                    consumer_nats;
                tracing::info!(
                    use_selective_node_targeting,
                    ?remote_partitions,
                    "ReplicaDeltaConsumer subscribing to NATS..."
                );
                let subscribe_result = if use_selective_node_targeting {
                    let selective_consumer =
                        database::nats_distributed_log::NatsDistributedLog::connect(
                            database::nats_distributed_log::NatsConfig {
                                url: nats_url.clone(),
                                consumer_name: Some(format!("{consumer_name}-selective")),
                                partition_id: None,
                            },
                        )
                        .await;
                    match selective_consumer {
                        Ok(consumer) => {
                            consumer
                                .subscribe_selective_node(from_ts.into(), &consumer_name)
                                .await
                        },
                        Err(e) => Err(e),
                    }
                } else {
                    consumer_nats_dyn
                        .subscribe_filtered(from_ts.into(), remote_partitions.clone())
                        .await
                };
                match subscribe_result {
                    Ok(mut stream) => {
                        tracing::info!("ReplicaDeltaConsumer subscribed, processing deltas...");
                        while let Some(result) = futures::StreamExt::next(&mut stream).await {
                            match result {
                                Ok(message) => {
                                    if use_selective_node_targeting {
                                        database::log_selective_delivery_shadow_receive();
                                    }
                                    match database::replica::apply_replication_message(
                                        &committer, message,
                                    )
                                    .await
                                    {
                                        Ok((ts, n)) => tracing::info!(
                                            "Applied replica delta: ts={}, {} updates",
                                            u64::from(ts),
                                            n
                                        ),
                                        Err(e) => {
                                            tracing::error!("Failed to apply NATS delta: {e:#}");
                                            break;
                                        },
                                    }
                                },
                                Err(e) => {
                                    tracing::error!("Error reading NATS delta: {e:#}");
                                    break;
                                },
                            }
                        }
                        tracing::warn!("ReplicaDeltaConsumer stream ended");
                    },
                    Err(e) => tracing::error!("Failed to subscribe to NATS: {e:#}"),
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
                        crate::deploy_config2::http_origin_from_peer_addr(addr.trim())
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
        let (peer_senders, transport_clients) =
            raft_transport::create_transport(&peer_addresses, raft_node_id);

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
        )?;
        let raft_state = manager.state();
        raft_state_for_app = Some(raft_state.clone());
        raft_peer_http_origins = Some(peer_http_origins);
        raft_peer_grpc_urls = Some(peer_grpc_urls);
        let mb_tx = manager.mailbox_tx();
        raft_mailbox_tx = Some(mb_tx);

        // Start the Raft node in a background task.
        // TiKV Apply Worker pattern: committed Raft entries are applied
        // to the state machine. On the leader, on_committed is a no-op
        // (already applied locally). On followers, on_committed deserializes
        // the CommitDelta and applies it via apply_replica_delta.
        if let Some(mut node) = manager.take_node() {
            let committer = database.committer_client();
            let raft_state_for_apply = raft_state.clone();
            runtime.spawn_background("raft_node", async move {
                node.run(
                    |data| {
                        // Deserialize the CommitDelta from the Raft entry.
                        let envelope: database::nats_distributed_log::DeltaEnvelope =
                            serde_json::from_slice(data).map_err(|e| {
                                anyhow::anyhow!("Failed to deserialize Raft entry: {e}")
                            })?;
                        let proposed_locally = envelope.source_raft_node_id() == Some(raft_node_id);
                        let delta = envelope.to_delta()?;

                        // Skip only the entries this node already committed
                        // locally before proposing them through Raft. Every
                        // other committed entry must be applied here, even if
                        // this node later became leader.
                        if !proposed_locally {
                            tracing::info!(
                                "Applying committed Raft delta locally: ts={}, leader_now={}",
                                u64::from(delta.ts),
                                raft_state_for_apply.is_leader(),
                            );
                            // apply_replica_delta is async — use block_in_place
                            // since we're in the Raft loop's sync callback.
                            let committer = committer.clone();
                            common::runtime::block_in_place(|| {
                                let rt = tokio::runtime::Handle::current();
                                rt.block_on(async {
                                    if let Err(e) = committer.apply_replica_delta(delta).await {
                                        tracing::error!("Raft follower apply failed: {e:#}");
                                    }
                                })
                            });
                        }

                        Ok(())
                    },
                    |snapshot_bytes| {
                        let checkpoint =
                            database::snapshot_checkpointer::checkpoint_from_bytes(snapshot_bytes)?;
                        let committer = committer.clone();
                        common::runtime::block_in_place(|| {
                            let rt = tokio::runtime::Handle::current();
                            rt.block_on(async {
                                committer.install_snapshot(checkpoint).await?;
                                Ok::<_, anyhow::Error>(())
                            })
                        })
                    },
                )
                .await;
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

    let app_state = LocalAppState {
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
        raft_mailbox_tx,
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
