//! Authenticated delivery of remote commit invalidations to the subscription
//! authority.

use std::{
    collections::BTreeSet,
    sync::Arc,
};

use common::grpc::ClusterGrpcAuth;
use database::{
    partition::{
        PartitionId,
        PlacementMetadataStore,
        PlacementVersion,
    },
    raft_partition::RaftPartitionState,
    CommitterClient,
    Database,
};
use pb::replication::{
    subscription_invalidation_service_server::{
        SubscriptionInvalidationService,
        SubscriptionInvalidationServiceServer,
    },
    InvalidateTablesRequest,
};
use runtime::prod::ProdRuntime;
use tonic::{
    Request,
    Response,
    Status,
};
use value::TableName;

pub struct SubscriptionInvalidationGrpcService {
    database: Database<ProdRuntime>,
    committer: CommitterClient,
    raft_state: Option<RaftPartitionState>,
    placement_metadata_store: Option<Arc<dyn PlacementMetadataStore>>,
    cluster_auth: Option<ClusterGrpcAuth>,
}

fn ensure_subscription_authority(
    authority_partition: PartitionId,
    local_partition: PartitionId,
    raft_state: Option<&RaftPartitionState>,
) -> Result<(), Status> {
    if authority_partition != PartitionId::DEFAULT || local_partition != authority_partition {
        return Err(Status::failed_precondition(format!(
            "Subscription authority {} reached partition {}",
            authority_partition, local_partition,
        )));
    }
    let Some(raft_state) = raft_state else {
        return Ok(());
    };
    if !raft_state.is_leader() {
        return Err(Status::unavailable(format!(
            "Subscription invalidation reached Raft follower {}; current leader is {}",
            raft_state.node_id(),
            raft_state.leader_id(),
        )));
    }
    if !raft_state.has_leader_serving_lease() {
        return Err(Status::unavailable(format!(
            "Subscription authority leader {} has no valid serving lease",
            raft_state.node_id(),
        )));
    }
    Ok(())
}

impl SubscriptionInvalidationGrpcService {
    pub fn new(
        database: Database<ProdRuntime>,
        committer: CommitterClient,
        raft_state: Option<RaftPartitionState>,
        placement_metadata_store: Option<Arc<dyn PlacementMetadataStore>>,
        cluster_auth: Option<ClusterGrpcAuth>,
    ) -> Self {
        Self {
            database,
            committer,
            raft_state,
            placement_metadata_store,
            cluster_auth,
        }
    }

    pub fn into_server(self) -> SubscriptionInvalidationServiceServer<Self> {
        SubscriptionInvalidationServiceServer::new(self)
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if let Some(auth) = &self.cluster_auth {
            auth.authenticate(request)?;
        }
        Ok(())
    }

    async fn ensure_placement_version(
        &self,
        placement_version: PlacementVersion,
    ) -> Result<(), Status> {
        if self
            .committer
            .ensure_placement_version(placement_version)
            .is_ok()
        {
            return Ok(());
        }
        if let Some(store) = self.placement_metadata_store.as_ref()
            && let Some(metadata) = store.load().await.map_err(|error| {
                Status::unavailable(format!(
                    "Failed to refresh placement before subscription invalidation: {error:#}"
                ))
            })?
        {
            self.committer
                .refresh_placement_metadata(metadata)
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "Refreshed placement metadata is invalid: {error:#}"
                    ))
                })?;
        }
        self.committer
            .ensure_placement_version(placement_version)
            .map_err(|error| {
                Status::failed_precondition(format!(
                    "Subscription invalidation rejected by placement guard: {error:#}"
                ))
            })
    }
}

#[tonic::async_trait]
impl SubscriptionInvalidationService for SubscriptionInvalidationGrpcService {
    async fn invalidate_tables(
        &self,
        request: Request<InvalidateTablesRequest>,
    ) -> Result<Response<()>, Status> {
        self.authenticate(&request)?;
        let request = request.into_inner();
        let authority_partition = PartitionId(request.authority_partition);
        let local_partition = self.committer.local_partition().ok_or_else(|| {
            Status::failed_precondition(
                "Subscription invalidation service requires a partitioned node",
            )
        })?;
        ensure_subscription_authority(
            authority_partition,
            local_partition,
            self.raft_state.as_ref(),
        )?;

        let placement_version = PlacementVersion::from(request.placement_version);
        self.ensure_placement_version(placement_version).await?;
        ensure_subscription_authority(
            authority_partition,
            local_partition,
            self.raft_state.as_ref(),
        )?;
        let commit_ts = request
            .commit_ts
            .try_into()
            .map_err(|error| Status::invalid_argument(format!("Invalid commit ts: {error:#}")))?;
        let table_names = request
            .table_names
            .into_iter()
            .map(|table_name| {
                table_name.parse::<TableName>().map_err(|error| {
                    Status::invalid_argument(format!(
                        "Invalid subscription table {table_name:?}: {error:#}"
                    ))
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        self.database
            .invalidate_subscriptions_for_tables(table_names, commit_ts)
            .map_err(|error| {
                Status::unavailable(format!(
                    "Failed to invalidate coordinator subscriptions: {error:#}"
                ))
            })?;
        ensure_subscription_authority(
            authority_partition,
            local_partition,
            self.raft_state.as_ref(),
        )?;
        Ok(Response::new(()))
    }
}

#[cfg(test)]
mod tests {
    use database::{
        partition::PartitionId,
        raft_partition::RaftPartitionState,
    };
    use tonic::Code;

    use super::ensure_subscription_authority;

    #[test]
    fn subscription_authority_accepts_only_partition_zero_serving_leader() {
        let leader = RaftPartitionState::new_for_test(true, 1, PartitionId::DEFAULT, 1);
        assert!(ensure_subscription_authority(
            PartitionId::DEFAULT,
            PartitionId::DEFAULT,
            Some(&leader),
        )
        .is_ok());

        let follower = RaftPartitionState::new_for_test(false, 1, PartitionId::DEFAULT, 2);
        let follower_error = ensure_subscription_authority(
            PartitionId::DEFAULT,
            PartitionId::DEFAULT,
            Some(&follower),
        )
        .unwrap_err();
        assert_eq!(follower_error.code(), Code::Unavailable);

        leader.expire_leader_serving_lease_for_test();
        let expired_lease = ensure_subscription_authority(
            PartitionId::DEFAULT,
            PartitionId::DEFAULT,
            Some(&leader),
        )
        .unwrap_err();
        assert_eq!(expired_lease.code(), Code::Unavailable);

        let wrong_partition =
            ensure_subscription_authority(PartitionId::DEFAULT, PartitionId(1), None).unwrap_err();
        assert_eq!(wrong_partition.code(), Code::FailedPrecondition);
    }
}
