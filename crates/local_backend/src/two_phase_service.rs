//! gRPC service for Two-Phase Commit across partitions.
//!
//! Each partitioned node runs a [`TwoPhaseCommitGrpcService`] that accepts
//! Prepare/CommitPrepared/RollbackPrepared requests from coordinators on
//! other nodes.
//!
//! The coordinator uses [`TwoPhaseCommitGrpcClient`] to reach remote
//! partitions.

use anyhow::Context;
use database::{
    two_phase::TwoPhaseTransactionId,
    CommitterClient,
};
use pb::replication::{
    two_phase_commit_service_client::TwoPhaseCommitServiceClient as TonicTwoPcClient,
    two_phase_commit_service_server::{
        TwoPhaseCommitService,
        TwoPhaseCommitServiceServer as TonicTwoPcServer,
    },
    TwoPcCommitRequest,
    TwoPcCommitResponse,
    TwoPcPrepareRequest,
    TwoPcPrepareResponse,
    TwoPcRollbackRequest,
    TwoPcRollbackResponse,
};
use tonic::{
    transport::Channel,
    Request,
    Response,
    Status,
};

/// gRPC server implementing the 2PC participant role.
/// Receives Prepare/Commit/Rollback from remote coordinators and
/// forwards them to the local CommitterClient.
pub struct TwoPhaseCommitGrpcService {
    committer: CommitterClient,
}

impl TwoPhaseCommitGrpcService {
    pub fn new(committer: CommitterClient) -> Self {
        Self { committer }
    }

    pub fn into_server(self) -> TonicTwoPcServer<Self> {
        TonicTwoPcServer::new(self)
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for TwoPhaseCommitGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);

        // TODO: Deserialize the FinalTransaction from req.serialized_transaction.
        // For now, this is a placeholder — full transaction serialization requires
        // additional protobuf definitions for DocumentUpdate, ReadSet, etc.
        // The coordinator currently only uses local prepare (same node).
        Err(Status::unimplemented(
            "Remote 2PC prepare not yet implemented — requires FinalTransaction serialization",
        ))
    }

    async fn commit_prepared(
        &self,
        request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id.clone());

        let ts = self
            .committer
            .commit_prepared(txn_id)
            .await
            .map_err(|e| Status::internal(format!("CommitPrepared failed: {e:#}")))?;

        Ok(Response::new(TwoPcCommitResponse {
            commit_ts: u64::from(ts),
        }))
    }

    async fn rollback_prepared(
        &self,
        request: Request<TwoPcRollbackRequest>,
    ) -> Result<Response<TwoPcRollbackResponse>, Status> {
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id.clone());

        self.committer
            .rollback_prepared(txn_id)
            .await
            .map_err(|e| Status::internal(format!("RollbackPrepared failed: {e:#}")))?;

        Ok(Response::new(TwoPcRollbackResponse {}))
    }
}

/// gRPC client for reaching remote partitions' 2PC service.
pub struct TwoPhaseCommitGrpcClient {
    client: TonicTwoPcClient<Channel>,
}

impl TwoPhaseCommitGrpcClient {
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let client = TonicTwoPcClient::connect(addr.to_string())
            .await
            .with_context(|| format!("Failed to connect to 2PC service at {addr}"))?;
        Ok(Self { client })
    }

    pub async fn commit_prepared(
        &self,
        transaction_id: &TwoPhaseTransactionId,
    ) -> anyhow::Result<u64> {
        let request = TwoPcCommitRequest {
            transaction_id: transaction_id.0.clone(),
        };
        let response = self
            .client
            .clone()
            .commit_prepared(Request::new(request))
            .await
            .context("gRPC CommitPrepared failed")?;
        Ok(response.into_inner().commit_ts)
    }

    pub async fn rollback_prepared(
        &self,
        transaction_id: &TwoPhaseTransactionId,
    ) -> anyhow::Result<()> {
        let request = TwoPcRollbackRequest {
            transaction_id: transaction_id.0.clone(),
        };
        self.client
            .clone()
            .rollback_prepared(Request::new(request))
            .await
            .context("gRPC RollbackPrepared failed")?;
        Ok(())
    }
}
