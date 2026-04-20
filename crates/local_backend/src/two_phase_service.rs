//! gRPC service for Two-Phase Commit across partitions.
//!
//! Each partitioned node runs a [`TwoPhaseCommitGrpcService`] that accepts
//! Prepare/CommitPrepared/RollbackPrepared requests from coordinators on
//! other nodes.
//!
//! The coordinator uses the database-layer gRPC client to reach remote
//! partitions.

use database::{
    two_phase::{
        ParticipantTransaction,
        TwoPhaseTransactionId,
    },
    CommitterClient,
};
use pb::replication::{
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
        let transaction = req
            .transaction
            .ok_or_else(|| Status::invalid_argument("Prepare missing transaction payload"))?;
        let transaction = ParticipantTransaction::try_from(transaction)
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare payload: {e:#}")))?;
        let prepare_ts = req.prepare_ts.into();

        let result = self
            .committer
            .prepare_remote(txn_id, transaction, req.write_source.into(), prepare_ts)
            .await
            .map_err(|e| Status::internal(format!("Prepare failed: {e:#}")))?;

        Ok(Response::new(TwoPcPrepareResponse {
            prepare_ts: u64::from(result.prepare_ts),
        }))
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
