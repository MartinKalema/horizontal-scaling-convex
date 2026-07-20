//! Direct reactive invalidation delivery to the cluster subscription authority.
//!
//! NATS remains the durable data-replication path. This control-plane RPC
//! closes the latency gap between a remote partition commit and the
//! coordinator's next subscription rerun without copying remote data into the
//! coordinator snapshot.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    grpc::ClusterGrpcAuth,
    types::Timestamp,
};
use futures::{
    stream::FuturesUnordered,
    StreamExt,
};
use pb::replication::{
    subscription_invalidation_service_client::SubscriptionInvalidationServiceClient,
    InvalidateTablesRequest,
};
use tokio::sync::Mutex;
use tonic::{
    transport::{
        Channel,
        Endpoint,
    },
    Request,
};
use value::TableName;

use crate::{
    committer::CommitterClient,
    partition::{
        PartitionId,
        PlacementVersion,
    },
};

const INVALIDATION_GRPC_TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait]
pub trait SubscriptionInvalidationClient: Send + Sync + 'static {
    async fn invalidate_tables(
        &self,
        authority_partition: PartitionId,
        placement_version: PlacementVersion,
        commit_ts: Timestamp,
        table_names: BTreeSet<TableName>,
    ) -> anyhow::Result<()>;
}

struct SubscriptionInvalidationGrpcClient {
    client: SubscriptionInvalidationServiceClient<Channel>,
    cluster_auth: Option<ClusterGrpcAuth>,
}

impl SubscriptionInvalidationGrpcClient {
    async fn connect(addr: &str, cluster_auth: Option<ClusterGrpcAuth>) -> anyhow::Result<Self> {
        let addr = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let endpoint = Endpoint::from_shared(addr.clone())
            .with_context(|| format!("Invalid subscription invalidation address {addr}"))?
            .connect_timeout(INVALIDATION_GRPC_TIMEOUT)
            .timeout(INVALIDATION_GRPC_TIMEOUT);
        let channel = endpoint.connect().await.with_context(|| {
            format!("Failed to connect to subscription invalidation service at {addr}")
        })?;
        Ok(Self {
            client: SubscriptionInvalidationServiceClient::new(channel),
            cluster_auth,
        })
    }

    async fn invalidate_tables(
        &self,
        authority_partition: PartitionId,
        placement_version: PlacementVersion,
        commit_ts: Timestamp,
        table_names: BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        let request = InvalidateTablesRequest {
            authority_partition: authority_partition.0,
            placement_version: u64::from(placement_version),
            commit_ts: u64::from(commit_ts),
            table_names: table_names
                .into_iter()
                .map(|table_name| table_name.to_string())
                .collect(),
        };
        let request = match &self.cluster_auth {
            Some(auth) => auth.request(request),
            None => Request::new(request),
        };
        self.client
            .clone()
            .invalidate_tables(request)
            .await
            .context("gRPC subscription invalidation failed")?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct GrpcSubscriptionInvalidationClient {
    committer: CommitterClient,
    cluster_auth: Option<ClusterGrpcAuth>,
    clients: Arc<Mutex<BTreeMap<String, Arc<SubscriptionInvalidationGrpcClient>>>>,
}

impl GrpcSubscriptionInvalidationClient {
    pub fn new(committer: CommitterClient, cluster_auth: Option<ClusterGrpcAuth>) -> Self {
        Self {
            committer,
            cluster_auth,
            clients: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    async fn client(
        &self,
        address: &str,
    ) -> anyhow::Result<Arc<SubscriptionInvalidationGrpcClient>> {
        if let Some(client) = self.clients.lock().await.get(address).cloned() {
            return Ok(client);
        }
        let client = Arc::new(
            SubscriptionInvalidationGrpcClient::connect(address, self.cluster_auth.clone()).await?,
        );
        let mut clients = self.clients.lock().await;
        Ok(clients
            .entry(address.to_string())
            .or_insert_with(|| client.clone())
            .clone())
    }
}

#[async_trait]
impl SubscriptionInvalidationClient for GrpcSubscriptionInvalidationClient {
    async fn invalidate_tables(
        &self,
        authority_partition: PartitionId,
        placement_version: PlacementVersion,
        commit_ts: Timestamp,
        table_names: BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        let addresses = self
            .committer
            .node_addresses()
            .and_then(|addresses| {
                addresses
                    .addresses_for(authority_partition)
                    .map(<[_]>::to_vec)
            })
            .with_context(|| {
                format!(
                    "No live subscription-authority candidates for partition {authority_partition}"
                )
            })?;
        let mut attempts = FuturesUnordered::new();
        for address in addresses {
            let client_pool = self.clone();
            let table_names = table_names.clone();
            attempts.push(async move {
                let result = match client_pool.client(&address).await {
                    Ok(client) => {
                        client
                            .invalidate_tables(
                                authority_partition,
                                placement_version,
                                commit_ts,
                                table_names,
                            )
                            .await
                    },
                    Err(error) => Err(error),
                };
                (address, result)
            });
        }

        let mut failures = Vec::new();
        while let Some((address, attempt)) = attempts.next().await {
            match attempt {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(format!("{address}: {error:#}")),
            }
        }
        anyhow::bail!(
            "All subscription-authority candidates for partition {} failed: {}",
            authority_partition,
            failures.join("; "),
        )
    }
}
