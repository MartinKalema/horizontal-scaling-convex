//! gRPC service for Two-Phase Commit across partitions.
//!
//! Each partitioned node runs a [`TwoPhaseCommitGrpcService`] that accepts
//! Prepare/CommitPrepared/RollbackPrepared requests from coordinators on
//! other nodes.
//!
//! The coordinator uses the database-layer gRPC client to reach remote
//! partitions.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use common::grpc::ClusterGrpcAuth;
use database::{
    partition::{
        PlacementMetadataStore,
        PlacementVersion,
    },
    raft_partition::RaftPartitionState,
    two_phase::{
        ParticipantTransaction,
        TwoPhaseCommitGrpcClient,
        TwoPhaseCommitGrpcClientPool,
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
    TwoPcValidateReadsRequest,
    TwoPcValidateReadsResponse,
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
    raft_state: Option<RaftPartitionState>,
    raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    placement_metadata_store: Option<Arc<dyn PlacementMetadataStore>>,
    cluster_auth: Option<ClusterGrpcAuth>,
    client_pool: TwoPhaseCommitGrpcClientPool,
}

impl TwoPhaseCommitGrpcService {
    pub fn new(
        committer: CommitterClient,
        raft_state: Option<RaftPartitionState>,
        raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
        placement_metadata_store: Option<Arc<dyn PlacementMetadataStore>>,
        cluster_auth: Option<ClusterGrpcAuth>,
    ) -> Self {
        Self {
            committer,
            raft_state,
            raft_peer_grpc_urls,
            placement_metadata_store,
            client_pool: TwoPhaseCommitGrpcClientPool::new(cluster_auth.clone()),
            cluster_auth,
        }
    }

    pub fn into_server(self) -> TonicTwoPcServer<Self> {
        TonicTwoPcServer::new(self)
    }

    async fn leader_client(&self) -> Result<Option<Arc<TwoPhaseCommitGrpcClient>>, Status> {
        let Some(raft_state) = self.raft_state.as_ref() else {
            return Ok(None);
        };
        if raft_state.is_leader() {
            return Ok(None);
        }
        let leader_id = raft_state.leader_id();
        if leader_id == 0 {
            return Err(Status::unavailable(
                "No elected Raft leader for this partition yet",
            ));
        }
        let leader_addr = self
            .raft_peer_grpc_urls
            .as_ref()
            .and_then(|urls| urls.get(&leader_id))
            .cloned()
            .ok_or_else(|| {
                Status::unavailable(format!(
                    "Missing RAFT_PEERS entry for Raft leader node {leader_id}"
                ))
            })?;
        let client = self.client_pool.client(&leader_addr).await.map_err(|e| {
            Status::unavailable(format!(
                "Failed to connect to Raft leader {} at {}: {e:#}",
                leader_id, leader_addr
            ))
        })?;
        Ok(Some(client))
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if let Some(auth) = &self.cluster_auth {
            auth.authenticate(request)?;
        }
        Ok(())
    }

    async fn refresh_placement_metadata(&self) -> Result<(), Status> {
        let Some(store) = self.placement_metadata_store.as_ref() else {
            return Ok(());
        };
        let Some(metadata) = store.load().await.map_err(|e| {
            Status::unavailable(format!(
                "Failed to refresh placement metadata before Prepare retry: {e:#}"
            ))
        })?
        else {
            return Ok(());
        };
        self.committer
            .refresh_placement_metadata(metadata)
            .map_err(|e| {
                Status::failed_precondition(format!(
                    "Refreshed placement metadata but local state is still invalid: {e:#}"
                ))
            })
    }

    async fn ensure_placement_version(
        &self,
        placement_version: PlacementVersion,
    ) -> Result<(), Status> {
        match self.committer.ensure_placement_version(placement_version) {
            Ok(()) => Ok(()),
            Err(original_err) => {
                self.refresh_placement_metadata().await?;
                self.committer
                    .ensure_placement_version(placement_version)
                    .map_err(|e| {
                        Status::failed_precondition(format!(
                            "Prepare rejected after placement metadata refresh: {e:#}. Original \
                             rejection: {original_err:#}"
                        ))
                    })
            },
        }
    }
}

#[tonic::async_trait]
impl TwoPhaseCommitService for TwoPhaseCommitGrpcService {
    async fn prepare(
        &self,
        request: Request<TwoPcPrepareRequest>,
    ) -> Result<Response<TwoPcPrepareResponse>, Status> {
        self.authenticate(&request)?;
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let transaction = req
            .transaction
            .ok_or_else(|| Status::invalid_argument("Prepare missing transaction payload"))?;
        let transaction = ParticipantTransaction::try_from(transaction)
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare payload: {e:#}")))?;
        let prepare_ts = req
            .prepare_ts
            .try_into()
            .map_err(|e| Status::invalid_argument(format!("Invalid prepare timestamp: {e:#}")))?;
        let placement_version = PlacementVersion::from(req.placement_version);
        let participants = req
            .participants
            .iter()
            .copied()
            .map(database::partition::PartitionId)
            .collect::<Vec<_>>();
        self.ensure_placement_version(placement_version).await?;

        if let Some(client) = self.leader_client().await? {
            let result = client
                .prepare(
                    &txn_id,
                    transaction,
                    req.write_source.into(),
                    prepare_ts,
                    placement_version,
                    &participants,
                )
                .await
                .map_err(|e| Status::internal(format!("Leader Prepare failed: {e:#}")))?;
            return Ok(Response::new(TwoPcPrepareResponse {
                prepare_ts: u64::from(result.prepare_ts),
            }));
        }

        let result = self
            .committer
            .prepare_remote(
                txn_id,
                transaction,
                req.write_source.into(),
                prepare_ts,
                participants,
            )
            .await
            .map_err(|e| Status::internal(format!("Prepare failed: {e:#}")))?;

        Ok(Response::new(TwoPcPrepareResponse {
            prepare_ts: u64::from(result.prepare_ts),
        }))
    }

    async fn validate_reads(
        &self,
        request: Request<TwoPcValidateReadsRequest>,
    ) -> Result<Response<TwoPcValidateReadsResponse>, Status> {
        self.authenticate(&request)?;
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id);
        let transaction = req
            .transaction
            .ok_or_else(|| Status::invalid_argument("ValidateReads missing transaction payload"))?;
        let transaction = ParticipantTransaction::try_from(transaction).map_err(|e| {
            Status::invalid_argument(format!("Invalid ValidateReads payload: {e:#}"))
        })?;
        let validate_ts = req.validate_ts.try_into().map_err(|e| {
            Status::invalid_argument(format!("Invalid ValidateReads timestamp: {e:#}"))
        })?;
        let placement_version = PlacementVersion::from(req.placement_version);
        self.ensure_placement_version(placement_version).await?;

        if let Some(client) = self.leader_client().await? {
            client
                .validate_reads(
                    &txn_id,
                    transaction,
                    req.write_source.into(),
                    validate_ts,
                    placement_version,
                )
                .await
                .map_err(|e| Status::internal(format!("Leader ValidateReads failed: {e:#}")))?;
            return Ok(Response::new(TwoPcValidateReadsResponse {}));
        }

        self.committer
            .validate_remote_reads(txn_id, transaction, req.write_source.into(), validate_ts)
            .await
            .map_err(|e| Status::internal(format!("ValidateReads failed: {e:#}")))?;

        Ok(Response::new(TwoPcValidateReadsResponse {}))
    }

    async fn commit_prepared(
        &self,
        request: Request<TwoPcCommitRequest>,
    ) -> Result<Response<TwoPcCommitResponse>, Status> {
        self.authenticate(&request)?;
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id.clone());

        if let Some(client) = self.leader_client().await? {
            let commit_ts = client
                .commit_prepared(&txn_id)
                .await
                .map_err(|e| Status::internal(format!("Leader CommitPrepared failed: {e:#}")))?;
            return Ok(Response::new(TwoPcCommitResponse { commit_ts }));
        }

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
        self.authenticate(&request)?;
        let req = request.into_inner();
        let txn_id = TwoPhaseTransactionId(req.transaction_id.clone());

        if let Some(client) = self.leader_client().await? {
            client
                .rollback_prepared(&txn_id)
                .await
                .map_err(|e| Status::internal(format!("Leader RollbackPrepared failed: {e:#}")))?;
            return Ok(Response::new(TwoPcRollbackResponse {}));
        }

        self.committer
            .rollback_prepared(txn_id)
            .await
            .map_err(|e| Status::internal(format!("RollbackPrepared failed: {e:#}")))?;

        Ok(Response::new(TwoPcRollbackResponse {}))
    }
}
