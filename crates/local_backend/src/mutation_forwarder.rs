//! gRPC service for forwarding public UDFs to an authoritative node.
//!
//! The Primary runs a [`MutationForwarderService`] that accepts mutation
//! requests from Replicas via gRPC.
//!
//! The Replica or follower runs a [`MutationForwarderGrpcClient`] that forwards
//! supported public requests to the authoritative node when local execution is
//! not appropriate.

use std::sync::Arc;

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
};
use serde_json::Value as JsonValue;
use sync_types::types::SerializedArgs;
use tonic::{
    transport::Channel,
    Request,
    Response,
    Status,
};
use value::JsonPackedValue;

/// gRPC server for mutation forwarding. Runs on the Primary.
pub struct MutationForwarderService {
    api: Arc<dyn ApplicationApi>,
    instance_name: String,
    cluster_auth: Option<ClusterGrpcAuth>,
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
}

#[tonic::async_trait]
impl MutationForwarder for MutationForwarderService {
    async fn forward_mutation(
        &self,
        request: Request<ForwardMutationRequest>,
    ) -> Result<Response<ForwardMutationResponse>, Status> {
        self.authenticate(&request)?;
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
        let read_after_write = match req.read_after_write_ts {
            Some(ts) => Some(ReadAfterWriteFence {
                source_partition: req.read_after_write_source_partition.map(PartitionId),
                ts: ts.try_into().map_err(|e: anyhow::Error| {
                    Status::invalid_argument(format!("Invalid read-after-write ts: {e}"))
                })?,
            }),
            None => None,
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
        let normalized_url = if primary_url.contains("://") {
            primary_url.to_string()
        } else {
            format!("http://{primary_url}")
        };
        let client = TonicMutationForwarderClient::connect(normalized_url.clone())
            .await
            .with_context(|| format!("Failed to connect to Primary at {normalized_url}"))?;
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

    /// Forward a mutation to the Primary and return the result.
    pub async fn forward(
        &self,
        path: &str,
        args: &str,
        identity: Identity,
        caller: &str,
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
            .forward_mutation(self.request(request))
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
        read_after_write: Option<ReadAfterWriteFence>,
        journal: Option<String>,
    ) -> anyhow::Result<RedactedQueryReturn> {
        let identity_proto: pb::convex_identity::UncheckedIdentity = identity.into();
        let request = ForwardQueryRequest {
            path: path.to_string(),
            args: args.to_string(),
            identity: Some(identity_proto),
            caller: Some(caller.into()),
            ts,
            journal,
            read_after_write_source_partition: read_after_write
                .and_then(|fence| fence.source_partition.map(|partition| partition.0)),
            read_after_write_ts: read_after_write.map(|fence| u64::from(fence.ts)),
        };
        let response = self
            .client
            .clone()
            .forward_query(self.request(request))
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
