//! gRPC service for forwarding mutations from Replica to Primary.
//!
//! The Primary runs a [`MutationForwarderService`] that accepts mutation
//! requests from Replicas via gRPC.
//!
//! The Replica runs a [`MutationForwarderGrpcClient`] that forwards mutations
//! to the Primary when clients try to execute mutations on a Replica.

use std::sync::Arc;

use anyhow::Context;
use application::api::ApplicationApi;
use common::{
    http::RequestDestination,
    types::FunctionCaller,
    version::ClientVersion,
    RequestId,
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
    MutationError,
    MutationSuccess,
};
use sync_types::types::SerializedArgs;
use tonic::{
    transport::Channel,
    Request,
    Response,
    Status,
};

/// gRPC server for mutation forwarding. Runs on the Primary.
pub struct MutationForwarderService {
    api: Arc<dyn ApplicationApi>,
    instance_name: String,
}

impl MutationForwarderService {
    pub fn new(api: Arc<dyn ApplicationApi>, instance_name: String) -> Self {
        Self {
            api,
            instance_name,
        }
    }

    pub fn into_server(self) -> TonicMutationForwarderServer<Self> {
        TonicMutationForwarderServer::new(self)
    }
}

#[tonic::async_trait]
impl MutationForwarder for MutationForwarderService {
    async fn forward_mutation(
        &self,
        request: Request<ForwardMutationRequest>,
    ) -> Result<Response<ForwardMutationResponse>, Status> {
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
            Ok(Err(err)) => Ok(Response::new(ForwardMutationResponse {
                result: Some(forward_mutation_response::Result::Error(MutationError {
                    error_message: format!("{}", err.error),
                    error_data: None,
                    log_lines: err.log_lines.iter().cloned().collect(),
                })),
            })),
            Err(e) => Err(Status::internal(format!("Mutation failed: {e}"))),
        }
    }
}

/// gRPC client for forwarding mutations from Replica to Primary.
pub struct MutationForwarderGrpcClient {
    client: TonicMutationForwarderClient<Channel>,
}

impl MutationForwarderGrpcClient {
    /// Connect to the Primary's gRPC mutation forwarding service.
    pub async fn connect(primary_url: &str) -> anyhow::Result<Self> {
        let client = TonicMutationForwarderClient::connect(primary_url.to_string())
            .await
            .with_context(|| format!("Failed to connect to Primary at {primary_url}"))?;
        tracing::info!("Connected to Primary mutation forwarder at {primary_url}");
        Ok(Self { client })
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
            .forward_mutation(Request::new(request))
            .await
            .context("gRPC mutation forwarding failed")?;
        Ok(response.into_inner())
    }
}
