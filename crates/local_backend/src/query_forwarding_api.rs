use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use application::api::SubscriptionClient;
use async_trait::async_trait;
use axum::body::Bytes;
use common::{
    components::{
        CanonicalizedComponentFunctionPath,
        ComponentId,
        ExportPath,
    },
    grpc::ClusterGrpcAuth,
    http::ResolvedHostname,
    types::{
        ConvexOrigin,
        FunctionCaller,
        RepeatableTimestamp,
    },
    RequestId,
};
use database::{
    partition::PartitionId,
    raft_partition::RaftPartitionState,
    two_phase::NodeAddresses,
    Database,
};
use errors::ErrorMetadata;
use file_storage::FileStream;
use futures::stream::BoxStream;
use headers::{
    ContentLength,
    ContentType,
};
use keybroker::Identity;
use model::{
    file_storage::FileStorageId,
    session_requests::types::SessionRequestIdentifier,
};
use runtime::prod::ProdRuntime;
use sync_types::{
    types::SerializedArgs,
    AuthenticationToken,
    SerializedQueryJournal,
};
use udf::{
    HttpActionRequest,
    HttpActionResponseStreamer,
};
use value::{
    sha256::Sha256Digest,
    PublicDocumentId,
};

use crate::mutation_forwarder::MutationForwarderGrpcClient;

const SELECTIVE_QUERY_AUTHORITY_PARTITION: PartitionId = PartitionId(0);
const RECENT_QUERY_INTEREST_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SelectiveQueryForwardingApi {
    inner: Arc<dyn application::api::ApplicationApi>,
    database: Database<ProdRuntime>,
    replica_mode: bool,
    partition_id: Option<PartitionId>,
    node_addresses: Option<NodeAddresses>,
    raft_state: Option<RaftPartitionState>,
    raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    cluster_grpc_auth: Option<ClusterGrpcAuth>,
}

impl SelectiveQueryForwardingApi {
    pub fn new(
        inner: Arc<dyn application::api::ApplicationApi>,
        database: Database<ProdRuntime>,
        replica_mode: bool,
        partition_id: Option<PartitionId>,
        node_addresses: Option<NodeAddresses>,
        raft_state: Option<RaftPartitionState>,
        raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
        cluster_grpc_auth: Option<ClusterGrpcAuth>,
    ) -> Self {
        Self {
            inner,
            database,
            replica_mode,
            partition_id,
            node_addresses,
            raft_state,
            raft_peer_grpc_urls,
            cluster_grpc_auth,
        }
    }

    async fn public_query_target_grpc_url(&self) -> anyhow::Result<Option<String>> {
        let Some(partition_id) = self.partition_id else {
            return Ok(None);
        };

        if partition_id != SELECTIVE_QUERY_AUTHORITY_PARTITION {
            return Ok(Some(self.authority_grpc_url()?));
        }

        let Some(raft_state) = self.raft_state.as_ref() else {
            return Ok(None);
        };
        if raft_state.is_leader() {
            if raft_state.has_leader_serving_lease() {
                return Ok(None);
            }
            anyhow::bail!(
                "Coordinator partition is leader but does not have a fresh Raft serving lease"
            );
        }
        let peer_grpc_urls = self.raft_peer_grpc_urls.as_ref().context(
            "Follower public query forwarding requires RAFT_PEERS gRPC URLs to find the current \
             leader",
        )?;
        for _ in 0..50 {
            let leader_id = raft_state.leader_id();
            if leader_id != 0 {
                let leader_grpc_url = peer_grpc_urls.get(&leader_id).with_context(|| {
                    format!("Missing RAFT_PEERS entry for Raft leader node {leader_id}")
                })?;
                return Ok(Some(leader_grpc_url.clone()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(None)
    }

    fn authority_grpc_url(&self) -> anyhow::Result<String> {
        let addresses = self.node_addresses.as_ref().context(
            "Selective public query forwarding requires NODE_ADDRESSES to find the authoritative \
             query node",
        )?;
        addresses
            .address_for(SELECTIVE_QUERY_AUTHORITY_PARTITION)
            .map(ToOwned::to_owned)
            .context(
                "Selective public query forwarding requires a partition-0 NODE_ADDRESSES entry",
            )
    }

    fn note_recent_interest(&self, token: &database::Token) {
        if let Err(e) = self
            .database
            .note_recent_delta_interest_for_token(token, RECENT_QUERY_INTEREST_TTL)
        {
            tracing::debug!("Failed to record recent query interest: {e:#}");
        }
    }

    fn unsupported_cluster_surface_error(&self, surface: &str, reason: String) -> anyhow::Error {
        anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(format!(
            "{surface} cannot execute locally on this clustered node: {reason}. Route the request \
             to the authoritative node or add an explicit forwarding path for this surface."
        ))
    }

    fn ensure_cluster_coordinator_authority(&self, surface: &str) -> anyhow::Result<()> {
        if !self.replica_mode && self.partition_id.is_none() {
            return Ok(());
        }
        if self.replica_mode {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                "replicas do not own global request surfaces".to_string(),
            ));
        }
        let Some(partition_id) = self.partition_id else {
            return Ok(());
        };
        if partition_id != SELECTIVE_QUERY_AUTHORITY_PARTITION {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                format!(
                    "partition {} is not the cluster coordinator partition {}",
                    partition_id.0, SELECTIVE_QUERY_AUTHORITY_PARTITION.0
                ),
            ));
        }
        if let Some(raft_state) = self.raft_state.as_ref()
            && !raft_state.is_leader()
        {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                "the coordinator partition is running as a Raft follower".to_string(),
            ));
        }
        if let Some(raft_state) = self.raft_state.as_ref()
            && !raft_state.has_leader_serving_lease()
        {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                "the coordinator partition does not have a fresh Raft serving lease".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl application::api::ApplicationApi for SelectiveQueryForwardingApi {
    async fn authenticate(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        auth_token: AuthenticationToken,
    ) -> anyhow::Result<Identity> {
        self.inner.authenticate(host, request_id, auth_token).await
    }

    async fn execute_public_query(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: ExportPath,
        args: SerializedArgs,
        caller: FunctionCaller,
        ts: application::api::ExecuteQueryTimestamp,
        read_after_write: Option<application::api::ReadAfterWriteFence>,
        journal: Option<SerializedQueryJournal>,
    ) -> anyhow::Result<application::RedactedQueryReturn> {
        let result = if let Some(target_grpc_url) = self.public_query_target_grpc_url().await? {
            let client = match self.cluster_grpc_auth.clone() {
                Some(auth) => {
                    MutationForwarderGrpcClient::connect_with_auth(&target_grpc_url, auth).await
                },
                None => MutationForwarderGrpcClient::connect(&target_grpc_url).await,
            }
            .with_context(|| {
                format!("Failed to connect to selective query authority at {target_grpc_url}")
            })
            .map_err(|e| anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(e))?;
            client
                .forward_query(
                    &String::from(path.clone()),
                    args.get(),
                    identity,
                    caller,
                    match ts {
                        application::api::ExecuteQueryTimestamp::Latest => None,
                        application::api::ExecuteQueryTimestamp::At(ts) => Some(u64::from(ts)),
                    },
                    read_after_write,
                    journal.flatten(),
                )
                .await
                .map_err(|e| anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(e))?
        } else {
            self.inner
                .execute_public_query(
                    host,
                    request_id,
                    identity,
                    path,
                    args,
                    caller,
                    ts,
                    read_after_write,
                    journal,
                )
                .await?
        };
        self.note_recent_interest(&result.token);
        Ok(result)
    }

    async fn execute_admin_query(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: CanonicalizedComponentFunctionPath,
        args: SerializedArgs,
        caller: FunctionCaller,
        ts: application::api::ExecuteQueryTimestamp,
        journal: Option<SerializedQueryJournal>,
    ) -> anyhow::Result<application::RedactedQueryReturn> {
        self.ensure_cluster_coordinator_authority("Admin query")?;
        self.inner
            .execute_admin_query(host, request_id, identity, path, args, caller, ts, journal)
            .await
    }

    async fn execute_public_mutation(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: ExportPath,
        args: SerializedArgs,
        caller: FunctionCaller,
        mutation_identifier: Option<SessionRequestIdentifier>,
        mutation_queue_length: Option<usize>,
    ) -> anyhow::Result<
        Result<application::RedactedMutationReturn, application::RedactedMutationError>,
    > {
        self.inner
            .execute_public_mutation(
                host,
                request_id,
                identity,
                path,
                args,
                caller,
                mutation_identifier,
                mutation_queue_length,
            )
            .await
    }

    async fn execute_admin_mutation(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: CanonicalizedComponentFunctionPath,
        args: SerializedArgs,
        caller: FunctionCaller,
        mutation_identifier: Option<SessionRequestIdentifier>,
        mutation_queue_length: Option<usize>,
    ) -> anyhow::Result<
        Result<application::RedactedMutationReturn, application::RedactedMutationError>,
    > {
        self.ensure_cluster_coordinator_authority("Admin mutation")?;
        self.inner
            .execute_admin_mutation(
                host,
                request_id,
                identity,
                path,
                args,
                caller,
                mutation_identifier,
                mutation_queue_length,
            )
            .await
    }

    async fn execute_public_action(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: ExportPath,
        args: SerializedArgs,
        caller: FunctionCaller,
    ) -> anyhow::Result<Result<application::RedactedActionReturn, application::RedactedActionError>>
    {
        self.ensure_cluster_coordinator_authority("Public action")?;
        self.inner
            .execute_public_action(host, request_id, identity, path, args, caller)
            .await
    }

    async fn execute_admin_action(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: CanonicalizedComponentFunctionPath,
        args: SerializedArgs,
        caller: FunctionCaller,
    ) -> anyhow::Result<Result<application::RedactedActionReturn, application::RedactedActionError>>
    {
        self.ensure_cluster_coordinator_authority("Admin action")?;
        self.inner
            .execute_admin_action(host, request_id, identity, path, args, caller)
            .await
    }

    async fn execute_http_action(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        http_request_metadata: HttpActionRequest,
        identity: Identity,
        caller: FunctionCaller,
        response_streamer: HttpActionResponseStreamer,
    ) -> anyhow::Result<()> {
        self.ensure_cluster_coordinator_authority("HTTP action")?;
        self.inner
            .execute_http_action(
                host,
                request_id,
                http_request_metadata,
                identity,
                caller,
                response_streamer,
            )
            .await
    }

    async fn execute_any_function(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        identity: Identity,
        path: CanonicalizedComponentFunctionPath,
        args: SerializedArgs,
        caller: FunctionCaller,
    ) -> anyhow::Result<Result<application::FunctionReturn, application::FunctionError>> {
        self.ensure_cluster_coordinator_authority("Any function execution")?;
        self.inner
            .execute_any_function(host, request_id, identity, path, args, caller)
            .await
    }

    async fn latest_timestamp(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
    ) -> anyhow::Result<RepeatableTimestamp> {
        self.ensure_cluster_coordinator_authority("Latest timestamp")?;
        self.inner.latest_timestamp(host, request_id).await
    }

    async fn check_store_file_authorization(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        token: &str,
        validity: Duration,
    ) -> anyhow::Result<ComponentId> {
        self.ensure_cluster_coordinator_authority("File upload authorization")?;
        self.inner
            .check_store_file_authorization(host, request_id, token, validity)
            .await
    }

    async fn store_file(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        origin: ConvexOrigin,
        component: ComponentId,
        content_length: Option<ContentLength>,
        content_type: Option<ContentType>,
        expected_sha256: Option<Sha256Digest>,
        body: BoxStream<'_, anyhow::Result<Bytes>>,
    ) -> anyhow::Result<PublicDocumentId> {
        self.ensure_cluster_coordinator_authority("File upload")?;
        self.inner
            .store_file(
                host,
                request_id,
                origin,
                component,
                content_length,
                content_type,
                expected_sha256,
                body,
            )
            .await
    }

    async fn get_file_range(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        origin: ConvexOrigin,
        component: ComponentId,
        file_storage_id: FileStorageId,
        range: (Bound<u64>, Bound<u64>),
    ) -> anyhow::Result<FileStream> {
        self.ensure_cluster_coordinator_authority("File range read")?;
        self.inner
            .get_file_range(host, request_id, origin, component, file_storage_id, range)
            .await
    }

    async fn get_file(
        &self,
        host: &ResolvedHostname,
        request_id: RequestId,
        origin: ConvexOrigin,
        component: ComponentId,
        file_storage_id: FileStorageId,
    ) -> anyhow::Result<FileStream> {
        self.ensure_cluster_coordinator_authority("File read")?;
        self.inner
            .get_file(host, request_id, origin, component, file_storage_id)
            .await
    }

    async fn subscription_client(
        &self,
        host: &ResolvedHostname,
    ) -> anyhow::Result<Box<dyn SubscriptionClient>> {
        self.ensure_cluster_coordinator_authority("Subscription setup")?;
        self.inner.subscription_client(host).await
    }

    async fn partition_id(&self, host: &ResolvedHostname) -> anyhow::Result<u64> {
        self.inner.partition_id(host).await
    }

    fn read_after_write_source_partition(&self) -> Option<u32> {
        self.inner.read_after_write_source_partition()
    }
}
