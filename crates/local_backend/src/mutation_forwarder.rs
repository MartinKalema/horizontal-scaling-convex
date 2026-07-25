//! gRPC service for forwarding public UDFs to an authoritative node.
//!
//! The Primary runs a [`MutationForwarderService`] that accepts mutation
//! requests from Replicas via gRPC.
//!
//! The Replica or follower runs a [`MutationForwarderGrpcClient`] that forwards
//! supported public requests to the authoritative node when local execution is
//! not appropriate.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use application::{
    api::{
        ApplicationApi,
        ExecuteQueryTimestamp,
        ReadAfterWriteFence,
    },
    RedactedQueryReturn,
};
use common::{
    grpc::ClusterGrpcAuth,
    http::RequestDestination,
    types::FunctionCaller,
    version::ClientVersion,
    RequestId,
};
use database::{
    partition::PartitionId,
    raft_partition::RaftPartitionState,
    Token,
};
use keybroker::Identity;
use pb::replication::{
    forward_mutation_response,
    mutation_forwarder_client::MutationForwarderClient as TonicMutationForwarderClient,
    mutation_forwarder_server::{
        MutationForwarder,
        MutationForwarderServer as TonicMutationForwarderServer,
    },
    ForwardMutationRequest,
    ForwardMutationResponse,
    ForwardQueryRequest,
    ForwardQueryResponse,
    MutationError,
    MutationSuccess,
    QueryError,
    QuerySuccess,
    ReadAfterWriteFence as PbReadAfterWriteFence,
};
use serde_json::Value as JsonValue;
use sync_types::types::SerializedArgs;
use tokio::sync::Mutex;
use tonic::{
    transport::{
        Channel,
        Endpoint,
    },
    Request,
    Response,
    Status,
};
use value::JsonPackedValue;

use crate::authority_routing::authority_redirect_status;

const FORWARDER_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const FORWARDER_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// gRPC server for mutation forwarding. Runs on the Primary.
pub struct MutationForwarderService {
    api: Arc<dyn ApplicationApi>,
    instance_name: String,
    cluster_auth: Option<ClusterGrpcAuth>,
    raft_state: Option<RaftPartitionState>,
    raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
}

impl MutationForwarderService {
    pub fn new(
        api: Arc<dyn ApplicationApi>,
        instance_name: String,
        cluster_auth: Option<ClusterGrpcAuth>,
    ) -> Self {
        Self {
            api,
            instance_name,
            cluster_auth,
            raft_state: None,
            raft_peer_grpc_urls: None,
        }
    }

    pub fn new_with_raft(
        api: Arc<dyn ApplicationApi>,
        instance_name: String,
        cluster_auth: Option<ClusterGrpcAuth>,
        raft_state: Option<RaftPartitionState>,
        raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    ) -> Self {
        Self {
            api,
            instance_name,
            cluster_auth,
            raft_state,
            raft_peer_grpc_urls,
        }
    }

    pub fn into_server(self) -> TonicMutationForwarderServer<Self> {
        TonicMutationForwarderServer::new(self)
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if let Some(auth) = &self.cluster_auth {
            auth.authenticate(request)?;
        }
        Ok(())
    }

    fn ensure_authority(&self, require_serving_lease: bool) -> Result<(), Status> {
        let Some(raft_state) = self.raft_state.as_ref() else {
            return Ok(());
        };
        if raft_state.is_cluster_genesis_ready()
            && raft_state.is_leader_ready()
            && (!require_serving_lease || raft_state.has_leader_serving_lease())
        {
            return Ok(());
        }
        Err(authority_redirect_status(
            format!(
                "Node {} is not ready to serve as the authority for partition {}; current leader \
                 is {}",
                raft_state.node_id(),
                raft_state.partition_id(),
                raft_state.leader_id(),
            ),
            Some(raft_state),
            self.raft_peer_grpc_urls.as_ref(),
        ))
    }
}

#[tonic::async_trait]
impl MutationForwarder for MutationForwarderService {
    async fn forward_mutation(
        &self,
        request: Request<ForwardMutationRequest>,
    ) -> Result<Response<ForwardMutationResponse>, Status> {
        self.authenticate(&request)?;
        self.ensure_authority(false)?;
        let req = request.into_inner();

        let identity = req
            .identity
            .ok_or_else(|| Status::invalid_argument("Missing identity"))
            .and_then(|proto| {
                Identity::from_proto_unchecked(proto)
                    .map_err(|e| Status::invalid_argument(format!("Invalid identity: {e}")))
            })?;

        let args = SerializedArgs::from_slice(req.args.as_bytes())
            .map_err(|e| Status::invalid_argument(format!("Invalid args: {e}")))?;

        let path = req
            .path
            .parse()
            .map_err(|e: anyhow::Error| Status::invalid_argument(format!("Invalid path: {e}")))?;

        let caller = FunctionCaller::HttpApi(
            req.caller
                .parse()
                .unwrap_or_else(|_| ClientVersion::unknown()),
        );

        let host = common::http::ResolvedHostname {
            instance_name: self.instance_name.clone(),
            destination: RequestDestination::ConvexCloud,
        };

        let result = self
            .api
            .execute_public_mutation(
                &host,
                RequestId::new(),
                identity,
                path,
                args,
                caller,
                None,
                req.mutation_queue_length.map(|n| n as usize),
            )
            .await;

        match result {
            Ok(Ok(ret)) => Ok(Response::new(ForwardMutationResponse {
                result: Some(forward_mutation_response::Result::Success(
                    MutationSuccess {
                        value: ret.value.as_str().to_string(),
                        log_lines: ret.log_lines.iter().cloned().collect(),
                        ts: u64::from(ret.ts),
                        source_partition: ret.source_partition.map(|partition| partition.0),
                        read_after_write_fences: ret
                            .read_after_write_partitions
                            .iter()
                            .map(|partition| PbReadAfterWriteFence {
                                source_partition: Some(partition.0),
                                ts: u64::from(ret.ts),
                            })
                            .collect(),
                    },
                )),
            })),
            Ok(Err(err)) => {
                let error_message = format!("{}", err.error);
                let error_data = err
                    .error
                    .custom_data_if_any()
                    .map(JsonValue::from)
                    .map(|value| serde_json::to_string(&value))
                    .transpose()
                    .map_err(|e| {
                        Status::internal(format!("Failed to serialize error data: {e}"))
                    })?;
                Ok(Response::new(ForwardMutationResponse {
                    result: Some(forward_mutation_response::Result::Error(MutationError {
                        error_message,
                        error_data,
                        log_lines: err.log_lines.iter().cloned().collect(),
                    })),
                }))
            },
            Err(e) => Err(Status::internal(format!("Mutation failed: {e}"))),
        }
    }

    async fn forward_query(
        &self,
        request: Request<ForwardQueryRequest>,
    ) -> Result<Response<ForwardQueryResponse>, Status> {
        self.authenticate(&request)?;
        self.ensure_authority(true)?;
        let req = request.into_inner();

        let identity = req
            .identity
            .ok_or_else(|| Status::invalid_argument("Missing identity"))
            .and_then(|proto| {
                Identity::from_proto_unchecked(proto)
                    .map_err(|e| Status::invalid_argument(format!("Invalid identity: {e}")))
            })?;

        let args = SerializedArgs::from_slice(req.args.as_bytes())
            .map_err(|e| Status::invalid_argument(format!("Invalid args: {e}")))?;

        let path = req
            .path
            .parse()
            .map_err(|e: anyhow::Error| Status::invalid_argument(format!("Invalid path: {e}")))?;

        let caller = req
            .caller
            .context("Missing caller")
            .and_then(FunctionCaller::try_from)
            .map_err(|e| Status::invalid_argument(format!("Invalid caller: {e:#}")))?;

        let host = common::http::ResolvedHostname {
            instance_name: self.instance_name.clone(),
            destination: RequestDestination::ConvexCloud,
        };

        let ts = match req.ts {
            Some(ts) => ExecuteQueryTimestamp::At(ts.try_into().map_err(|e: anyhow::Error| {
                Status::invalid_argument(format!("Invalid ts: {e}"))
            })?),
            None => ExecuteQueryTimestamp::Latest,
        };
        let read_after_write = if !req.read_after_write_fences.is_empty() {
            Some(
                req.read_after_write_fences
                    .into_iter()
                    .map(|fence| {
                        Ok(ReadAfterWriteFence {
                            source_partition: fence.source_partition.map(PartitionId),
                            ts: fence.ts.try_into().map_err(|e: anyhow::Error| {
                                Status::invalid_argument(format!(
                                    "Invalid read-after-write fence ts: {e}"
                                ))
                            })?,
                        })
                    })
                    .collect::<Result<Vec<_>, Status>>()?,
            )
        } else {
            match req.read_after_write_ts {
                Some(ts) => Some(vec![ReadAfterWriteFence {
                    source_partition: req.read_after_write_source_partition.map(PartitionId),
                    ts: ts.try_into().map_err(|e: anyhow::Error| {
                        Status::invalid_argument(format!("Invalid read-after-write ts: {e}"))
                    })?,
                }]),
                None => None,
            }
        };

        let result = self
            .api
            .execute_public_query(
                &host,
                RequestId::new(),
                identity,
                path,
                args,
                caller,
                ts,
                read_after_write,
                Some(req.journal),
            )
            .await;

        match result {
            Ok(ret) => {
                let token = Some(ret.token.clone().try_into().map_err(|e: anyhow::Error| {
                    Status::internal(format!("Failed to serialize query token: {e}"))
                })?);
                let response = match ret.result {
                    Ok(value) => ForwardQueryResponse {
                        token,
                        journal: ret.journal,
                        result: Some(pb::replication::forward_query_response::Result::Success(
                            QuerySuccess {
                                value: value.as_str().to_string(),
                                log_lines: Some(ret.log_lines.into()),
                            },
                        )),
                    },
                    Err(error) => ForwardQueryResponse {
                        token,
                        journal: ret.journal,
                        result: Some(pb::replication::forward_query_response::Result::Error(
                            QueryError {
                                error: Some(error.try_into().map_err(|e: anyhow::Error| {
                                    Status::internal(format!(
                                        "Failed to serialize query error: {e}"
                                    ))
                                })?),
                                log_lines: Some(ret.log_lines.into()),
                            },
                        )),
                    },
                };
                Ok(Response::new(response))
            },
            Err(e) => Err(Status::internal(format!("Query failed: {e}"))),
        }
    }
}

/// gRPC client for forwarding mutations from Replica to Primary.
pub struct MutationForwarderGrpcClient {
    client: TonicMutationForwarderClient<Channel>,
    cluster_auth: Option<ClusterGrpcAuth>,
}

fn normalize_grpc_url(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

#[derive(Clone)]
pub struct MutationForwarderGrpcClientPool {
    cluster_auth: Option<ClusterGrpcAuth>,
    clients: Arc<Mutex<BTreeMap<String, Arc<MutationForwarderGrpcClient>>>>,
}

impl MutationForwarderGrpcClientPool {
    pub fn new(cluster_auth: Option<ClusterGrpcAuth>) -> Self {
        Self {
            cluster_auth,
            clients: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub async fn client(&self, url: &str) -> anyhow::Result<Arc<MutationForwarderGrpcClient>> {
        let normalized_url = normalize_grpc_url(url);
        if let Some(client) = self.clients.lock().await.get(&normalized_url).cloned() {
            return Ok(client);
        }

        let client = Arc::new(
            MutationForwarderGrpcClient::connect_inner(&normalized_url, self.cluster_auth.clone())
                .await?,
        );
        let mut clients = self.clients.lock().await;
        Ok(clients
            .entry(normalized_url)
            .or_insert_with(|| client.clone())
            .clone())
    }
}

impl MutationForwarderGrpcClient {
    /// Connect to the Primary's gRPC mutation forwarding service.
    pub async fn connect(primary_url: &str) -> anyhow::Result<Self> {
        Self::connect_inner(primary_url, None).await
    }

    pub async fn connect_with_auth(
        primary_url: &str,
        cluster_auth: ClusterGrpcAuth,
    ) -> anyhow::Result<Self> {
        Self::connect_inner(primary_url, Some(cluster_auth)).await
    }

    async fn connect_inner(
        primary_url: &str,
        cluster_auth: Option<ClusterGrpcAuth>,
    ) -> anyhow::Result<Self> {
        let normalized_url = normalize_grpc_url(primary_url);
        let channel = Endpoint::from_shared(normalized_url.clone())?
            .connect_timeout(FORWARDER_CONNECT_TIMEOUT)
            .connect()
            .await
            .with_context(|| format!("Failed to connect to Primary at {normalized_url}"))?;
        let client = TonicMutationForwarderClient::new(channel);
        tracing::info!("Connected to Primary mutation forwarder at {normalized_url}");
        Ok(Self {
            client,
            cluster_auth,
        })
    }

    fn request<T>(&self, message: T) -> Request<T> {
        match &self.cluster_auth {
            Some(auth) => auth.request(message),
            None => Request::new(message),
        }
    }

    fn request_with_timeout<T>(&self, message: T, timeout: Duration) -> Request<T> {
        let mut request = self.request(message);
        request.set_timeout(timeout);
        request
    }

    /// Forward a mutation to the Primary and return the result.
    pub async fn forward(
        &self,
        path: &str,
        args: &str,
        identity: Identity,
        caller: &str,
    ) -> anyhow::Result<ForwardMutationResponse> {
        self.forward_with_timeout(path, args, identity, caller, FORWARDER_DEFAULT_TIMEOUT)
            .await
    }

    pub async fn forward_with_timeout(
        &self,
        path: &str,
        args: &str,
        identity: Identity,
        caller: &str,
        timeout: Duration,
    ) -> anyhow::Result<ForwardMutationResponse> {
        let identity_proto: pb::convex_identity::UncheckedIdentity = identity.into();
        let request = ForwardMutationRequest {
            path: path.to_string(),
            args: args.to_string(),
            identity: Some(identity_proto),
            caller: caller.to_string(),
            mutation_identifier: None,
            mutation_queue_length: None,
        };
        let response = self
            .client
            .clone()
            .forward_mutation(self.request_with_timeout(request, timeout))
            .await
            .context("gRPC mutation forwarding failed")?;
        Ok(response.into_inner())
    }

    pub async fn forward_query(
        &self,
        path: &str,
        args: &str,
        identity: Identity,
        caller: FunctionCaller,
        ts: Option<u64>,
        read_after_write: Option<Vec<ReadAfterWriteFence>>,
        journal: Option<String>,
    ) -> anyhow::Result<RedactedQueryReturn> {
        self.forward_query_with_timeout(
            path,
            args,
            identity,
            caller,
            ts,
            read_after_write,
            journal,
            FORWARDER_DEFAULT_TIMEOUT,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn forward_query_with_timeout(
        &self,
        path: &str,
        args: &str,
        identity: Identity,
        caller: FunctionCaller,
        ts: Option<u64>,
        read_after_write: Option<Vec<ReadAfterWriteFence>>,
        journal: Option<String>,
        timeout: Duration,
    ) -> anyhow::Result<RedactedQueryReturn> {
        let identity_proto: pb::convex_identity::UncheckedIdentity = identity.into();
        let read_after_write_fences = read_after_write.clone().unwrap_or_default();
        let legacy_read_after_write_fence = read_after_write_fences.first().copied();
        let request = ForwardQueryRequest {
            path: path.to_string(),
            args: args.to_string(),
            identity: Some(identity_proto),
            caller: Some(caller.into()),
            ts,
            journal,
            read_after_write_source_partition: legacy_read_after_write_fence
                .and_then(|fence| fence.source_partition.map(|partition| partition.0)),
            read_after_write_ts: legacy_read_after_write_fence.map(|fence| u64::from(fence.ts)),
            read_after_write_fences: read_after_write_fences
                .into_iter()
                .map(|fence| PbReadAfterWriteFence {
                    source_partition: fence.source_partition.map(|partition| partition.0),
                    ts: u64::from(fence.ts),
                })
                .collect(),
        };
        let response = self
            .client
            .clone()
            .forward_query(self.request_with_timeout(request, timeout))
            .await
            .context("gRPC query forwarding failed")?;
        let response = response.into_inner();
        let token: Token = response
            .token
            .context("Forwarded query response missing token")?
            .try_into()?;
        let journal = response.journal;
        let (result, log_lines) = match response.result {
            Some(pb::replication::forward_query_response::Result::Success(success)) => (
                Ok(JsonPackedValue::from_network(success.value)
                    .context("Forwarded query response contained an invalid packed JSON value")?),
                success.log_lines.unwrap_or_default().try_into()?,
            ),
            Some(pb::replication::forward_query_response::Result::Error(error)) => (
                Err(error
                    .error
                    .context("Forwarded query error missing error payload")?
                    .try_into()?),
                error.log_lines.unwrap_or_default().try_into()?,
            ),
            None => anyhow::bail!("Forwarded query response missing result"),
        };
        Ok(RedactedQueryReturn {
            result,
            log_lines,
            token,
            journal,
        })
    }
}
