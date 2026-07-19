//! Authoritative cross-partition reads.
//!
//! A node-local NATS mirror is a cache, not a source of truth. Transactions use
//! this module to fetch remote index ranges from the partition that owns the
//! table at the transaction's pinned placement version and snapshot timestamp.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    document::{
        PackedDocument,
        ResolvedDocument,
    },
    grpc::ClusterGrpcAuth,
    index::IndexKeyBytes,
    interval::{
        BinaryKey,
        End,
        Interval,
        StartIncluded,
    },
    query::{
        CursorPosition,
        Order,
    },
    types::{
        IndexDescriptor,
        IndexId,
        IndexName,
        RepeatableTimestamp,
        TabletIndexName,
        Timestamp,
    },
    value::TableMapping,
};
use indexing::{
    backend_in_memory_indexes::{
        IndexEntry,
        IndexPage,
        IndexReader,
        RangeRequest,
    },
    index_registry::IndexRegistry,
};
use pb::{
    common::{
        interval::End as IntervalEndProto,
        Interval as IntervalProto,
        Order as OrderProto,
    },
    replication::{
        close_read_timestamp_response::Outcome as CloseReadTimestampOutcome,
        owner_read_index_range_result::Cursor as CursorProto,
        owner_read_service_client::OwnerReadServiceClient as TonicOwnerReadClient,
        CloseReadTimestampRequest,
        CloseReadTimestampResponse,
        OwnerReadIndexEntry,
        OwnerReadIndexRange,
        OwnerReadIndexRangeResult,
        OwnerReadIndexRangesRequest,
        OwnerReadIndexRangesResponse,
    },
};
use tokio::sync::Mutex;
use tonic::{
    transport::{
        Channel,
        Endpoint,
    },
    Request,
};
use value::TabletId;

use crate::{
    committer::{
        CommitterClient,
        ReadTimestampClosure,
    },
    partition::{
        PartitionId,
        PlacementVersion,
    },
};

const OWNER_READ_GRPC_TIMEOUT: Duration = Duration::from_secs(5);

// A transaction may read 16 MiB by default, so leave room for protobuf
// overhead and multi-range responses instead of inheriting tonic's 4 MiB cap.
pub const OWNER_READ_GRPC_MAX_MESSAGE_SIZE: usize = 1 << 26;

#[derive(Clone, Debug)]
pub struct OwnerIndexEntry {
    pub key: IndexKeyBytes,
    pub ts: Timestamp,
    pub document: PackedDocument,
}

#[derive(Clone, Debug)]
pub struct OwnerIndexRangeResult {
    pub entries: Vec<OwnerIndexEntry>,
    pub cursor: CursorPosition,
}

#[async_trait]
pub trait OwnerReadClient: Send + Sync + 'static {
    async fn close_read_timestamp(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        target_ts: Timestamp,
    ) -> anyhow::Result<ReadTimestampClosure>;

    async fn read_index_ranges(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        snapshot_ts: Timestamp,
        ranges: Vec<RangeRequest>,
    ) -> anyhow::Result<Vec<OwnerIndexRangeResult>>;
}

/// Index reader used by the in-process function runner in a partitioned
/// deployment. Local and system-table reads retain the normal persistence
/// path, while remote user-table reads go directly to their placement owner.
pub(crate) struct OwnerAwareIndexReader {
    timestamp: RepeatableTimestamp,
    local_reader: Arc<dyn IndexReader>,
    owner_read_client: Arc<dyn OwnerReadClient>,
    partition_map: crate::partition::PartitionMap,
    table_mapping: TableMapping,
    index_registry: IndexRegistry,
}

impl OwnerAwareIndexReader {
    pub(crate) fn new(
        timestamp: RepeatableTimestamp,
        local_reader: Arc<dyn IndexReader>,
        owner_read_client: Arc<dyn OwnerReadClient>,
        partition_map: crate::partition::PartitionMap,
        table_mapping: TableMapping,
        index_registry: IndexRegistry,
    ) -> Self {
        Self {
            timestamp,
            local_reader,
            owner_read_client,
            partition_map,
            table_mapping,
            index_registry,
        }
    }
}

#[async_trait]
impl IndexReader for OwnerAwareIndexReader {
    async fn index_page(
        &self,
        index_id: IndexId,
        tablet_id: TabletId,
        interval: &Interval,
        order: Order,
        max_results: usize,
    ) -> anyhow::Result<IndexPage> {
        let table_name = self.table_mapping.tablet_name(tablet_id)?;
        let owner = self.partition_map.partition_for_table(&table_name);
        if table_name.is_system() || owner == self.partition_map.local_partition() {
            return self
                .local_reader
                .index_page(index_id, tablet_id, interval, order, max_results)
                .await;
        }

        let tablet_index_name = self
            .index_registry
            .enabled_index_by_index_id(&index_id)
            .with_context(|| format!("Missing enabled index {index_id} for owner read"))?
            .name();
        anyhow::ensure!(
            tablet_index_name.table() == &tablet_id,
            "Index {index_id} belongs to tablet {}, not requested tablet {tablet_id}",
            tablet_index_name.table(),
        );
        let printable_index_name = tablet_index_name
            .clone()
            .map_table(&self.table_mapping.tablet_to_name())?;
        let mut results = self
            .owner_read_client
            .read_index_ranges(
                owner,
                self.partition_map.placement_version(),
                *self.timestamp,
                vec![RangeRequest {
                    index_name: tablet_index_name,
                    printable_index_name,
                    interval: interval.clone(),
                    order,
                    max_size: max_results,
                }],
            )
            .await?;
        anyhow::ensure!(
            results.len() == 1,
            "Owner {owner} returned {} index pages for one request",
            results.len(),
        );
        let result = results.pop().expect("result count checked above");
        Ok(IndexPage {
            entries: result
                .entries
                .into_iter()
                .map(|entry| IndexEntry {
                    key: entry.key,
                    ts: entry.ts,
                    value: entry.document,
                })
                .collect(),
            cursor: result.cursor,
        })
    }

    fn timestamp(&self) -> RepeatableTimestamp {
        self.timestamp
    }
}

fn tablet_id_to_bytes(tablet_id: TabletId) -> Vec<u8> {
    tablet_id.0 .0.to_vec()
}

pub fn tablet_id_from_bytes(bytes: Vec<u8>) -> anyhow::Result<TabletId> {
    Ok(TabletId(bytes.try_into()?))
}

pub fn index_names_from_wire(
    tablet_id: TabletId,
    table_name: String,
    descriptor: String,
) -> anyhow::Result<(TabletIndexName, IndexName)> {
    let table_name = table_name.parse()?;
    let names = match descriptor.as_str() {
        "by_id" => (
            TabletIndexName::by_id(tablet_id),
            IndexName::by_id(table_name),
        ),
        "by_creation_time" => (
            TabletIndexName::by_creation_time(tablet_id),
            IndexName::by_creation_time(table_name),
        ),
        _ => {
            let descriptor = IndexDescriptor::new(descriptor)?;
            (
                TabletIndexName::new(tablet_id, descriptor.clone())?,
                IndexName::new(table_name, descriptor)?,
            )
        },
    };
    Ok(names)
}

fn interval_to_proto(interval: Interval) -> IntervalProto {
    let end = match interval.end {
        End::Excluded(end) => IntervalEndProto::Exclusive(end.into()),
        End::Unbounded => IntervalEndProto::AfterAll(()),
    };
    IntervalProto {
        start_inclusive: interval.start.0.into(),
        end: Some(end),
    }
}

pub fn interval_from_proto(interval: IntervalProto) -> anyhow::Result<Interval> {
    let end = match interval.end.context("Owner read interval missing end")? {
        IntervalEndProto::Exclusive(end) => End::Excluded(BinaryKey::from(end)),
        IntervalEndProto::AfterAll(()) => End::Unbounded,
    };
    Ok(Interval {
        start: StartIncluded(BinaryKey::from(interval.start_inclusive)),
        end,
    })
}

fn order_to_proto(order: Order) -> i32 {
    match order {
        Order::Asc => OrderProto::Asc as i32,
        Order::Desc => OrderProto::Desc as i32,
    }
}

pub fn order_from_proto(order: i32) -> anyhow::Result<Order> {
    match OrderProto::try_from(order).context("Invalid owner read order")? {
        OrderProto::Asc => Ok(Order::Asc),
        OrderProto::Desc => Ok(Order::Desc),
    }
}

fn range_to_proto(range: RangeRequest) -> OwnerReadIndexRange {
    OwnerReadIndexRange {
        tablet_id: tablet_id_to_bytes(*range.index_name.table()),
        descriptor: range.index_name.descriptor().to_string(),
        table_name: range.printable_index_name.table().to_string(),
        interval: Some(interval_to_proto(range.interval)),
        order: order_to_proto(range.order),
        max_size: range.max_size.try_into().unwrap_or(u64::MAX),
    }
}

fn result_from_proto(result: OwnerReadIndexRangeResult) -> anyhow::Result<OwnerIndexRangeResult> {
    let entries = result
        .entries
        .into_iter()
        .map(
            |OwnerReadIndexEntry {
                 index_key,
                 commit_ts,
                 document,
             }| {
                let document = ResolvedDocument::try_from(
                    document.context("Owner read entry missing document")?,
                )?;
                Ok(OwnerIndexEntry {
                    key: IndexKeyBytes(index_key),
                    ts: commit_ts.try_into()?,
                    document: PackedDocument::pack(&document),
                })
            },
        )
        .collect::<anyhow::Result<Vec<_>>>()?;
    let cursor = match result.cursor.context("Owner read result missing cursor")? {
        CursorProto::After(key) => CursorPosition::After(IndexKeyBytes(key)),
        CursorProto::End(()) => CursorPosition::End,
    };
    Ok(OwnerIndexRangeResult { entries, cursor })
}

pub fn result_to_proto(result: OwnerIndexRangeResult) -> anyhow::Result<OwnerReadIndexRangeResult> {
    let entries = result
        .entries
        .into_iter()
        .map(|entry| {
            Ok(OwnerReadIndexEntry {
                index_key: entry.key.0,
                commit_ts: u64::from(entry.ts),
                document: Some(entry.document.unpack().try_into()?),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let cursor = Some(match result.cursor {
        CursorPosition::After(key) => CursorProto::After(key.0),
        CursorPosition::End => CursorProto::End(()),
    });
    Ok(OwnerReadIndexRangeResult { entries, cursor })
}

struct OwnerReadGrpcClient {
    client: TonicOwnerReadClient<Channel>,
    cluster_auth: Option<ClusterGrpcAuth>,
}

impl OwnerReadGrpcClient {
    async fn connect(addr: &str, cluster_auth: Option<ClusterGrpcAuth>) -> anyhow::Result<Self> {
        let addr = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let endpoint = Endpoint::from_shared(addr.clone())
            .with_context(|| format!("Invalid owner-read service address {addr}"))?
            .connect_timeout(OWNER_READ_GRPC_TIMEOUT)
            .timeout(OWNER_READ_GRPC_TIMEOUT);
        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Failed to connect to owner-read service at {addr}"))?;
        Ok(Self {
            client: TonicOwnerReadClient::new(channel)
                .max_decoding_message_size(OWNER_READ_GRPC_MAX_MESSAGE_SIZE)
                .max_encoding_message_size(OWNER_READ_GRPC_MAX_MESSAGE_SIZE),
            cluster_auth,
        })
    }

    async fn read_index_ranges(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        snapshot_ts: Timestamp,
        ranges: Vec<RangeRequest>,
    ) -> anyhow::Result<Vec<OwnerIndexRangeResult>> {
        let request = OwnerReadIndexRangesRequest {
            owner_partition: owner_partition.0,
            placement_version: u64::from(placement_version),
            snapshot_ts: u64::from(snapshot_ts),
            ranges: ranges.into_iter().map(range_to_proto).collect(),
        };
        let request = match &self.cluster_auth {
            Some(auth) => auth.request(request),
            None => Request::new(request),
        };
        let OwnerReadIndexRangesResponse { results } = self
            .client
            .clone()
            .read_index_ranges(request)
            .await
            .context("gRPC owner read failed")?
            .into_inner();
        results.into_iter().map(result_from_proto).collect()
    }

    async fn close_read_timestamp(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        target_ts: Timestamp,
    ) -> anyhow::Result<ReadTimestampClosure> {
        let request = CloseReadTimestampRequest {
            owner_partition: owner_partition.0,
            placement_version: u64::from(placement_version),
            target_ts: u64::from(target_ts),
        };
        let request = match &self.cluster_auth {
            Some(auth) => auth.request(request),
            None => Request::new(request),
        };
        let CloseReadTimestampResponse { outcome } = self
            .client
            .clone()
            .close_read_timestamp(request)
            .await
            .context("gRPC owner read barrier failed")?
            .into_inner();
        match outcome.context("Owner read barrier response missing outcome")? {
            CloseReadTimestampOutcome::ClosedTs(ts) => {
                Ok(ReadTimestampClosure::Closed(ts.try_into()?))
            },
            CloseReadTimestampOutcome::RetryAtTs(ts) => {
                Ok(ReadTimestampClosure::RetryAt(ts.try_into()?))
            },
            CloseReadTimestampOutcome::BlockedTs(ts) => {
                Ok(ReadTimestampClosure::Blocked(ts.try_into()?))
            },
        }
    }
}

#[derive(Clone)]
struct OwnerReadGrpcClientPool {
    cluster_auth: Option<ClusterGrpcAuth>,
    clients: Arc<Mutex<BTreeMap<String, Arc<OwnerReadGrpcClient>>>>,
}

impl OwnerReadGrpcClientPool {
    fn new(cluster_auth: Option<ClusterGrpcAuth>) -> Self {
        Self {
            cluster_auth,
            clients: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    async fn client(&self, addr: &str) -> anyhow::Result<Arc<OwnerReadGrpcClient>> {
        if let Some(client) = self.clients.lock().await.get(addr).cloned() {
            return Ok(client);
        }
        let client = Arc::new(OwnerReadGrpcClient::connect(addr, self.cluster_auth.clone()).await?);
        let mut clients = self.clients.lock().await;
        Ok(clients
            .entry(addr.to_string())
            .or_insert_with(|| client.clone())
            .clone())
    }
}

/// Dynamic, retrying owner-read router. Membership refreshes update the
/// `CommitterClient`'s address snapshot, so transactions do not pin node
/// addresses even though they pin a placement version.
#[derive(Clone)]
pub struct GrpcOwnerReadClient {
    committer: CommitterClient,
    pool: OwnerReadGrpcClientPool,
}

impl GrpcOwnerReadClient {
    pub fn new(committer: CommitterClient, cluster_auth: Option<ClusterGrpcAuth>) -> Self {
        Self {
            committer,
            pool: OwnerReadGrpcClientPool::new(cluster_auth),
        }
    }
}

#[async_trait]
impl OwnerReadClient for GrpcOwnerReadClient {
    async fn close_read_timestamp(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        target_ts: Timestamp,
    ) -> anyhow::Result<ReadTimestampClosure> {
        let addresses = self
            .committer
            .node_addresses()
            .and_then(|addresses| addresses.addresses_for(owner_partition).map(<[_]>::to_vec))
            .with_context(|| {
                format!("No live owner-read candidates for partition {owner_partition}")
            })?;
        let mut failures = Vec::new();
        for address in addresses {
            let attempt = async {
                self.pool
                    .client(&address)
                    .await?
                    .close_read_timestamp(owner_partition, placement_version, target_ts)
                    .await
            }
            .await;
            match attempt {
                Ok(result) => return Ok(result),
                Err(error) => failures.push(format!("{address}: {error:#}")),
            }
        }
        anyhow::bail!(
            "All owner-read barrier candidates for partition {} failed: {}",
            owner_partition,
            failures.join("; "),
        )
    }

    async fn read_index_ranges(
        &self,
        owner_partition: PartitionId,
        placement_version: PlacementVersion,
        snapshot_ts: Timestamp,
        ranges: Vec<RangeRequest>,
    ) -> anyhow::Result<Vec<OwnerIndexRangeResult>> {
        let addresses = self
            .committer
            .node_addresses()
            .and_then(|addresses| addresses.addresses_for(owner_partition).map(<[_]>::to_vec))
            .with_context(|| {
                format!("No live owner-read candidates for partition {owner_partition}")
            })?;
        let mut failures = Vec::new();
        for address in addresses {
            let attempt = async {
                self.pool
                    .client(&address)
                    .await?
                    .read_index_ranges(
                        owner_partition,
                        placement_version,
                        snapshot_ts,
                        ranges.clone(),
                    )
                    .await
            }
            .await;
            match attempt {
                Ok(results) => return Ok(results),
                Err(error) => failures.push(format!("{address}: {error:#}")),
            }
        }
        anyhow::bail!(
            "All owner-read candidates for partition {} failed: {}",
            owner_partition,
            failures.join("; "),
        )
    }
}

#[cfg(test)]
mod tests {
    use common::{
        assert_obj,
        document::CreationTime,
        interval::Interval,
        query::Order,
        testing::TestIdGenerator,
        types::Timestamp,
    };

    use super::*;

    #[test]
    fn owner_read_wire_round_trip_preserves_rows_and_cursor() -> anyhow::Result<()> {
        let mut ids = TestIdGenerator::new();
        let id = ids.user_generate(&"messages".parse()?);
        let document = ResolvedDocument::new(id, CreationTime::ONE, assert_obj!("body" => "hi"))?;
        let result = OwnerIndexRangeResult {
            entries: vec![OwnerIndexEntry {
                key: IndexKeyBytes(vec![1, 2, 3]),
                ts: Timestamp::must(42),
                document: PackedDocument::pack(&document),
            }],
            cursor: CursorPosition::After(IndexKeyBytes(vec![4, 5, 6])),
        };

        let decoded = result_from_proto(result_to_proto(result)?)?;
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].key, IndexKeyBytes(vec![1, 2, 3]));
        assert_eq!(decoded.entries[0].ts, Timestamp::must(42));
        assert_eq!(decoded.entries[0].document.unpack(), document);
        assert_eq!(
            decoded.cursor,
            CursorPosition::After(IndexKeyBytes(vec![4, 5, 6]))
        );
        Ok(())
    }

    #[test]
    fn owner_read_interval_and_order_round_trip() -> anyhow::Result<()> {
        let interval = Interval::prefix(BinaryKey::from(vec![1, 2, 3]));
        assert_eq!(
            interval_from_proto(interval_to_proto(interval.clone()))?,
            interval
        );
        assert_eq!(order_from_proto(order_to_proto(Order::Asc))?, Order::Asc);
        assert_eq!(order_from_proto(order_to_proto(Order::Desc))?, Order::Desc);
        Ok(())
    }
}
