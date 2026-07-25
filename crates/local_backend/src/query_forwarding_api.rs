use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

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
    membership::MembershipStore,
    partition::PartitionId,
    raft_partition::RaftPartitionState,
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

use crate::{
    authority_routing::{
        authority_leader_hint,
        AttemptTimeoutDisposition,
        AuthorityAttemptError,
        AuthorityEndpointKind,
        AuthorityResolver,
    },
    mutation_forwarder::MutationForwarderGrpcClientPool,
    SharedNodeAddresses,
};

const SELECTIVE_QUERY_AUTHORITY_PARTITION: PartitionId = PartitionId(0);
const RECENT_QUERY_INTEREST_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SelectiveQueryForwardingApi {
    inner: Arc<dyn application::api::ApplicationApi>,
    database: Database<ProdRuntime>,
    replica_mode: bool,
    partition_id: Option<PartitionId>,
    raft_state: Option<RaftPartitionState>,
    authority_resolver: AuthorityResolver,
    mutation_forwarder_pool: MutationForwarderGrpcClientPool,
}

impl SelectiveQueryForwardingApi {
    pub fn new(
        inner: Arc<dyn application::api::ApplicationApi>,
        database: Database<ProdRuntime>,
        replica_mode: bool,
        partition_id: Option<PartitionId>,
        node_addresses: SharedNodeAddresses,
        raft_state: Option<RaftPartitionState>,
        raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
        membership_store: Option<Arc<dyn MembershipStore>>,
        cluster_grpc_auth: Option<ClusterGrpcAuth>,
    ) -> Self {
        let authority_resolver = AuthorityResolver::new(
            node_addresses,
            membership_store,
            raft_state.clone(),
            raft_peer_grpc_urls,
            None,
        );
        Self {
            inner,
            database,
            replica_mode,
            partition_id,
            raft_state,
            authority_resolver,
            mutation_forwarder_pool: MutationForwarderGrpcClientPool::new(cluster_grpc_auth),
        }
    }

    fn should_forward_public_query(&self) -> anyhow::Result<bool> {
        let Some(partition_id) = self.partition_id else {
            return Ok(false);
        };

        if partition_id != SELECTIVE_QUERY_AUTHORITY_PARTITION {
            return Ok(true);
        }

        let Some(raft_state) = self.raft_state.as_ref() else {
            return Ok(false);
        };
        anyhow::ensure!(
            raft_state.is_cluster_genesis_ready(),
            "Canonical cluster genesis is not ready"
        );
        Ok(!raft_state.can_serve_as_leader())
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
        if let Some(raft_state) = self.raft_state.as_ref()
            && !raft_state.is_cluster_genesis_ready()
        {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                "canonical cluster genesis is not Raft-confirmed by every partition".to_string(),
            ));
        }
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
            && !raft_state.is_leader_ready()
        {
            return Err(self.unsupported_cluster_surface_error(
                surface,
                "the coordinator leader is still applying its current-term barrier".to_string(),
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
        read_after_write: Option<Vec<application::api::ReadAfterWriteFence>>,
        journal: Option<SerializedQueryJournal>,
    ) -> anyhow::Result<application::RedactedQueryReturn> {
        let result = if self.should_forward_public_query()? {
            let path = String::from(path.clone());
            let args = args.get().to_owned();
            let ts = match ts {
                application::api::ExecuteQueryTimestamp::Latest => None,
                application::api::ExecuteQueryTimestamp::At(ts) => Some(u64::from(ts)),
            };
            let journal = journal.flatten();
            let pool = self.mutation_forwarder_pool.clone();
            self.authority_resolver
                .route(
                    SELECTIVE_QUERY_AUTHORITY_PARTITION,
                    AuthorityEndpointKind::Grpc,
                    AttemptTimeoutDisposition::RetryableRead,
                    move |attempt| {
                        let pool = pool.clone();
                        let path = path.clone();
                        let args = args.clone();
                        let identity = identity.clone();
                        let caller = caller.clone();
                        let read_after_write = read_after_write.clone();
                        let journal = journal.clone();
                        async move {
                            let started = Instant::now();
                            let client = pool.client(&attempt.endpoint).await.map_err(|error| {
                                AuthorityAttemptError::retry(error.context(format!(
                                    "Failed to connect to query authority at {}",
                                    attempt.endpoint
                                )))
                            })?;
                            let Some(request_timeout) =
                                attempt.timeout.checked_sub(started.elapsed())
                            else {
                                return Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                    "Query authority connection consumed the attempt deadline"
                                )));
                            };
                            client
                                .forward_query_with_timeout(
                                    &path,
                                    &args,
                                    identity,
                                    caller,
                                    ts,
                                    read_after_write,
                                    journal,
                                    request_timeout,
                                )
                                .await
                                .map_err(|error| {
                                    let leader_hint = authority_leader_hint(&error);
                                    AuthorityAttemptError::retry_with_leader(error, leader_hint)
                                })
                        }
                    },
                )
                .await
                .map(|resolved| resolved.value)
                .map_err(|e| {
                    anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(format!(
                        "Failed to route query to the current authority: {e:#}"
                    ))
                })?
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
