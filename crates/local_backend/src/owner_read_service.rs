//! Authoritative cross-partition index reads.
//!
//! Transactions send remote ranges to the partition that owns their table.
//! Only that partition's current Raft leader may answer, and every range is
//! evaluated at the request's pinned placement version and snapshot timestamp.

use std::sync::Arc;

use common::grpc::ClusterGrpcAuth;
use database::{
    owner_read::{
        index_names_from_wire,
        interval_from_proto,
        order_from_proto,
        result_to_proto,
        tablet_id_from_bytes,
        OWNER_READ_GRPC_MAX_MESSAGE_SIZE,
    },
    partition::{
        PartitionId,
        PartitionMap,
        PlacementMetadataStore,
        PlacementVersion,
    },
    raft_partition::RaftPartitionState,
    CommitterClient,
    Database,
    ReadTimestampClosure,
};
use indexing::backend_in_memory_indexes::RangeRequest;
use pb::replication::{
    close_read_timestamp_response::Outcome as CloseReadTimestampOutcome,
    owner_read_service_server::{
        OwnerReadService,
        OwnerReadServiceServer as TonicOwnerReadServer,
    },
    CloseReadTimestampRequest,
    CloseReadTimestampResponse,
    OwnerReadIndexRange,
    OwnerReadIndexRangesRequest,
    OwnerReadIndexRangesResponse,
};
use runtime::prod::ProdRuntime;
use tonic::{
    Request,
    Response,
    Status,
};

pub struct OwnerReadGrpcService {
    database: Database<ProdRuntime>,
    committer: CommitterClient,
    raft_state: Option<RaftPartitionState>,
    placement_metadata_store: Option<Arc<dyn PlacementMetadataStore>>,
    cluster_auth: Option<ClusterGrpcAuth>,
}

impl OwnerReadGrpcService {
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

    pub fn into_server(self) -> TonicOwnerReadServer<Self> {
        TonicOwnerReadServer::new(self)
            .max_decoding_message_size(OWNER_READ_GRPC_MAX_MESSAGE_SIZE)
            .max_encoding_message_size(OWNER_READ_GRPC_MAX_MESSAGE_SIZE)
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
        let Some(metadata) = store.load().await.map_err(|error| {
            Status::unavailable(format!(
                "Failed to refresh placement metadata before owner read: {error:#}"
            ))
        })?
        else {
            return Ok(());
        };
        self.committer
            .refresh_placement_metadata(metadata)
            .map_err(|error| {
                Status::failed_precondition(format!(
                    "Refreshed placement metadata is invalid: {error:#}"
                ))
            })
    }

    async fn ensure_placement_version(
        &self,
        placement_version: PlacementVersion,
    ) -> Result<(), Status> {
        match self.committer.ensure_placement_version(placement_version) {
            Ok(()) => Ok(()),
            Err(original_error) => {
                self.refresh_placement_metadata().await?;
                self.committer
                    .ensure_placement_version(placement_version)
                    .map_err(|error| {
                        Status::failed_precondition(format!(
                            "Owner read rejected after placement refresh: {error:#}. Original \
                             rejection: {original_error:#}"
                        ))
                    })
            },
        }
    }

    fn ensure_serving_leader(&self) -> Result<(), Status> {
        let Some(raft_state) = self.raft_state.as_ref() else {
            return Ok(());
        };
        if !raft_state.is_leader() {
            return Err(Status::unavailable(format!(
                "Partition {} owner read reached Raft follower {}; current leader is {}",
                raft_state.partition_id(),
                raft_state.node_id(),
                raft_state.leader_id(),
            )));
        }
        if !raft_state.has_leader_serving_lease() {
            return Err(Status::unavailable(format!(
                "Partition {} leader {} has no valid serving lease",
                raft_state.partition_id(),
                raft_state.node_id(),
            )));
        }
        Ok(())
    }

    fn decode_range(
        partition_map: &PartitionMap,
        owner_partition: PartitionId,
        range: OwnerReadIndexRange,
    ) -> Result<RangeRequest, Status> {
        let table_name = range.table_name.parse().map_err(|error| {
            Status::invalid_argument(format!("Invalid owner-read table name: {error:#}"))
        })?;
        let actual_owner = partition_map.partition_for_table(&table_name);
        if actual_owner != owner_partition {
            return Err(Status::failed_precondition(format!(
                "Table {} belongs to {}, not requested owner {}",
                table_name, actual_owner, owner_partition,
            )));
        }
        let tablet_id = tablet_id_from_bytes(range.tablet_id).map_err(|error| {
            Status::invalid_argument(format!("Invalid owner-read tablet ID: {error:#}"))
        })?;
        let (index_name, printable_index_name) =
            index_names_from_wire(tablet_id, range.table_name, range.descriptor).map_err(
                |error| Status::invalid_argument(format!("Invalid owner-read index: {error:#}")),
            )?;
        let interval = interval_from_proto(
            range
                .interval
                .ok_or_else(|| Status::invalid_argument("Owner-read range missing interval"))?,
        )
        .map_err(|error| {
            Status::invalid_argument(format!("Invalid owner-read interval: {error:#}"))
        })?;
        let order = order_from_proto(range.order).map_err(|error| {
            Status::invalid_argument(format!("Invalid owner-read order: {error:#}"))
        })?;
        let max_size = usize::try_from(range.max_size)
            .map_err(|_| Status::invalid_argument("Owner-read max_size exceeds usize"))?;
        Ok(RangeRequest {
            index_name,
            printable_index_name,
            interval,
            order,
            max_size,
        })
    }
}

#[tonic::async_trait]
impl OwnerReadService for OwnerReadGrpcService {
    async fn close_read_timestamp(
        &self,
        request: Request<CloseReadTimestampRequest>,
    ) -> Result<Response<CloseReadTimestampResponse>, Status> {
        self.authenticate(&request)?;
        let request = request.into_inner();
        let owner_partition = PartitionId(request.owner_partition);
        let local_partition = self.committer.local_partition().ok_or_else(|| {
            Status::failed_precondition("Owner-read service requires a partitioned node")
        })?;
        if owner_partition != local_partition {
            return Err(Status::failed_precondition(format!(
                "Read barrier for {} reached {}",
                owner_partition, local_partition,
            )));
        }

        let placement_version = PlacementVersion::from(request.placement_version);
        self.ensure_placement_version(placement_version).await?;
        self.ensure_serving_leader()?;
        let target_ts = request.target_ts.try_into().map_err(|error| {
            Status::invalid_argument(format!("Invalid read barrier timestamp: {error:#}"))
        })?;
        let outcome = self
            .committer
            .close_latest_read_timestamp(target_ts)
            .await
            .map_err(|error| {
                Status::unavailable(format!("Failed to close owner read timestamp: {error:#}"))
            })?;
        // Recheck after the asynchronous persistence barrier. A leader that lost
        // its lease while closing must not certify a cluster snapshot.
        self.ensure_placement_version(placement_version).await?;
        self.ensure_serving_leader()?;
        let outcome = Some(match outcome {
            ReadTimestampClosure::Closed(ts) => CloseReadTimestampOutcome::ClosedTs(u64::from(ts)),
            ReadTimestampClosure::RetryAt(ts) => {
                CloseReadTimestampOutcome::RetryAtTs(u64::from(ts))
            },
            ReadTimestampClosure::Blocked(ts) => {
                CloseReadTimestampOutcome::BlockedTs(u64::from(ts))
            },
        });
        Ok(Response::new(CloseReadTimestampResponse { outcome }))
    }

    async fn read_index_ranges(
        &self,
        request: Request<OwnerReadIndexRangesRequest>,
    ) -> Result<Response<OwnerReadIndexRangesResponse>, Status> {
        self.authenticate(&request)?;
        let request = request.into_inner();
        let owner_partition = PartitionId(request.owner_partition);
        let local_partition = self.committer.local_partition().ok_or_else(|| {
            Status::failed_precondition("Owner-read service requires a partitioned node")
        })?;
        if owner_partition != local_partition {
            return Err(Status::failed_precondition(format!(
                "Owner read for {} reached {}",
                owner_partition, local_partition,
            )));
        }

        let placement_version = PlacementVersion::from(request.placement_version);
        self.ensure_placement_version(placement_version).await?;
        self.ensure_serving_leader()?;
        let partition_map = self.committer.partition_map().ok_or_else(|| {
            Status::failed_precondition("Owner-read service requires partition placement")
        })?;
        if partition_map.placement_version() != placement_version {
            return Err(Status::failed_precondition(format!(
                "Placement changed from {} before owner read could pin it",
                placement_version,
            )));
        }

        let snapshot_ts = request.snapshot_ts.try_into().map_err(|error| {
            Status::invalid_argument(format!("Invalid owner-read snapshot timestamp: {error:#}"))
        })?;
        match self
            .committer
            .close_read_timestamp(snapshot_ts)
            .await
            .map_err(|error| {
                Status::unavailable(format!(
                    "Owner could not verify read barrier {snapshot_ts}: {error:#}"
                ))
            })? {
            ReadTimestampClosure::Closed(closed) if closed == snapshot_ts => {},
            ReadTimestampClosure::Closed(closed) => {
                return Err(Status::internal(format!(
                    "Owner certified read barrier {closed} instead of {snapshot_ts}"
                )));
            },
            ReadTimestampClosure::RetryAt(local_floor) => {
                return Err(Status::unavailable(format!(
                    "Owner read barrier {snapshot_ts} is not portable to this leader; local floor \
                     is {local_floor}"
                )));
            },
            ReadTimestampClosure::Blocked(prepared_ts) => {
                return Err(Status::unavailable(format!(
                    "Owner read barrier {snapshot_ts} is blocked by prepared transaction at \
                     {prepared_ts}"
                )));
            },
        }
        self.ensure_placement_version(placement_version).await?;
        self.ensure_serving_leader()?;
        let ranges = request
            .ranges
            .into_iter()
            .map(|range| Self::decode_range(&partition_map, owner_partition, range))
            .collect::<Result<Vec<_>, _>>()?;
        let results = self
            .database
            .authoritative_index_ranges(snapshot_ts, ranges)
            .await
            .map_err(|error| {
                Status::unavailable(format!("Authoritative owner read failed: {error:#}"))
            })?
            .into_iter()
            .map(result_to_proto)
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|error| {
                Status::internal(format!("Failed to encode owner-read response: {error:#}"))
            })?;

        Ok(Response::new(OwnerReadIndexRangesResponse { results }))
    }
}
