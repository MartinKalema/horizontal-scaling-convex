//! Cluster membership directory for horizontal scaling.
//!
//! Placement answers "which partition owns this data?" Membership answers
//! "which physical nodes can serve that partition right now?" Keeping these
//! separate prevents placement metadata from turning into a hardcoded address
//! book as the cluster grows.

use std::{
    collections::BTreeMap,
    fmt,
};

use anyhow::Context;
use async_trait::async_trait;
use serde::{
    Deserialize,
    Serialize,
};

use crate::{
    partition::PartitionId,
    two_phase::NodeAddresses,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MembershipVersion(u64);

impl MembershipVersion {
    pub const BOOTSTRAP: Self = Self(0);

    pub fn new(version: u64) -> Self {
        Self(version)
    }
}

impl From<MembershipVersion> for u64 {
    fn from(version: MembershipVersion) -> Self {
        version.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClusterNodeId(String);

impl ClusterNodeId {
    pub fn new(node_id: impl Into<String>) -> anyhow::Result<Self> {
        let node_id = node_id.into();
        anyhow::ensure!(
            !node_id.trim().is_empty(),
            "cluster node id cannot be empty"
        );
        Ok(Self(node_id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClusterNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMembership {
    pub node_id: ClusterNodeId,
    pub partition: PartitionId,
    pub grpc_addr: String,
    pub http_origin: Option<String>,
    pub raft_addr: Option<String>,
    pub generation: u64,
    pub draining: bool,
}

impl NodeMembership {
    pub fn new(
        node_id: ClusterNodeId,
        partition: PartitionId,
        grpc_addr: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let grpc_addr = grpc_addr.into().trim().to_string();
        anyhow::ensure!(
            !grpc_addr.is_empty(),
            "cluster membership node {node_id} cannot have an empty gRPC address"
        );
        Ok(Self {
            node_id,
            partition,
            grpc_addr,
            http_origin: None,
            raft_addr: None,
            generation: 0,
            draining: false,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipSnapshot {
    version: MembershipVersion,
    nodes: BTreeMap<ClusterNodeId, NodeMembership>,
}

impl MembershipSnapshot {
    pub fn new(version: MembershipVersion, nodes: Vec<NodeMembership>) -> Self {
        Self {
            version,
            nodes: nodes
                .into_iter()
                .map(|node| (node.node_id.clone(), node))
                .collect(),
        }
    }

    pub fn from_node_addresses(version: MembershipVersion, addresses: &NodeAddresses) -> Self {
        let nodes = addresses
            .to_entries()
            .into_iter()
            .flat_map(|(partition, addresses)| {
                addresses
                    .into_iter()
                    .enumerate()
                    .filter_map(move |(index, address)| {
                        let node_id =
                            ClusterNodeId::new(format!("partition-{}-peer-{index}", partition.0))
                                .ok()?;
                        NodeMembership::new(node_id, partition, address).ok()
                    })
            })
            .collect();
        Self::new(version, nodes)
    }

    pub fn version(&self) -> MembershipVersion {
        self.version
    }

    pub fn nodes(&self) -> &BTreeMap<ClusterNodeId, NodeMembership> {
        &self.nodes
    }

    pub fn to_node_addresses(&self) -> NodeAddresses {
        let mut addresses: BTreeMap<PartitionId, Vec<String>> = BTreeMap::new();
        for node in self.nodes.values() {
            if node.draining {
                continue;
            }
            addresses
                .entry(node.partition)
                .or_default()
                .push(node.grpc_addr.clone());
        }
        NodeAddresses::from_entries(addresses)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedMembershipSnapshot {
    version: u64,
    nodes: Vec<SerializedNodeMembership>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedNodeMembership {
    node_id: String,
    partition: u32,
    grpc_addr: String,
    #[serde(default)]
    http_origin: Option<String>,
    #[serde(default)]
    raft_addr: Option<String>,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    draining: bool,
}

impl From<&MembershipSnapshot> for SerializedMembershipSnapshot {
    fn from(snapshot: &MembershipSnapshot) -> Self {
        Self {
            version: u64::from(snapshot.version()),
            nodes: snapshot
                .nodes()
                .values()
                .map(|node| SerializedNodeMembership {
                    node_id: node.node_id.as_str().to_string(),
                    partition: node.partition.0,
                    grpc_addr: node.grpc_addr.clone(),
                    http_origin: node.http_origin.clone(),
                    raft_addr: node.raft_addr.clone(),
                    generation: node.generation,
                    draining: node.draining,
                })
                .collect(),
        }
    }
}

impl TryFrom<SerializedMembershipSnapshot> for MembershipSnapshot {
    type Error = anyhow::Error;

    fn try_from(serialized: SerializedMembershipSnapshot) -> anyhow::Result<Self> {
        let nodes = serialized
            .nodes
            .into_iter()
            .map(|node| {
                let node_id = ClusterNodeId::new(node.node_id)?;
                let mut membership =
                    NodeMembership::new(node_id, PartitionId(node.partition), node.grpc_addr)?;
                membership.http_origin = node.http_origin;
                membership.raft_addr = node.raft_addr;
                membership.generation = node.generation;
                membership.draining = node.draining;
                Ok(membership)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(MembershipSnapshot::new(
            MembershipVersion::new(serialized.version),
            nodes,
        ))
    }
}

const MEMBERSHIP_KV_BUCKET: &str = "convex_membership";
const MEMBERSHIP_CURRENT_KEY: &str = "current";

#[async_trait]
pub trait MembershipStore: Send + Sync + 'static {
    async fn load(&self) -> anyhow::Result<Option<MembershipSnapshot>>;
    async fn ensure_initialized(
        &self,
        bootstrap_snapshot: MembershipSnapshot,
    ) -> anyhow::Result<MembershipSnapshot>;
}

pub struct NatsMembershipStore {
    kv: async_nats::jetstream::kv::Store,
}

impl NatsMembershipStore {
    pub async fn connect(nats_url: &str) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = async_nats::connect(nats_url)
            .await
            .with_context(|| format!("Membership: failed to connect to NATS at {nats_url}"))?;
        let jetstream = async_nats::jetstream::new(client);
        let kv = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: MEMBERSHIP_KV_BUCKET.to_string(),
                history: 16,
                ..Default::default()
            })
            .await
            .context("Membership: failed to create KV bucket")?;
        Ok(Self { kv })
    }

    fn encode(snapshot: &MembershipSnapshot) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(&SerializedMembershipSnapshot::from(snapshot))
            .context("Membership: failed to serialize snapshot")
    }

    fn decode(bytes: &[u8]) -> anyhow::Result<MembershipSnapshot> {
        let serialized: SerializedMembershipSnapshot =
            serde_json::from_slice(bytes).context("Membership: failed to parse snapshot")?;
        serialized.try_into()
    }
}

#[async_trait]
impl MembershipStore for NatsMembershipStore {
    async fn load(&self) -> anyhow::Result<Option<MembershipSnapshot>> {
        let Some(entry) = self
            .kv
            .entry(MEMBERSHIP_CURRENT_KEY)
            .await
            .context("Membership: failed to read current snapshot")?
        else {
            return Ok(None);
        };
        Ok(Some(Self::decode(&entry.value)?))
    }

    async fn ensure_initialized(
        &self,
        bootstrap_snapshot: MembershipSnapshot,
    ) -> anyhow::Result<MembershipSnapshot> {
        if let Some(existing) = self.load().await? {
            return Ok(existing);
        }
        let bytes = Self::encode(&bootstrap_snapshot)?;
        match self.kv.create(MEMBERSHIP_CURRENT_KEY, bytes.into()).await {
            Ok(_) => Ok(bootstrap_snapshot),
            Err(_) => self
                .load()
                .await?
                .context("Membership: failed to initialize and no current snapshot exists"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_membership_snapshot_converts_to_node_addresses() -> anyhow::Result<()> {
        let snapshot = MembershipSnapshot::new(
            MembershipVersion::new(7),
            vec![
                NodeMembership::new(ClusterNodeId::new("p0a")?, PartitionId(0), "node-p0a:50051")?,
                NodeMembership::new(ClusterNodeId::new("p0b")?, PartitionId(0), "node-p0b:50051")?,
                {
                    let mut node = NodeMembership::new(
                        ClusterNodeId::new("p1-draining")?,
                        PartitionId(1),
                        "node-p1-old:50051",
                    )?;
                    node.draining = true;
                    node
                },
                NodeMembership::new(ClusterNodeId::new("p1a")?, PartitionId(1), "node-p1a:50051")?,
            ],
        );

        let addresses = snapshot.to_node_addresses();
        assert_eq!(
            addresses.address_for(PartitionId(0)),
            Some("node-p0a:50051")
        );
        assert_eq!(
            addresses.addresses_for(PartitionId(0)).unwrap(),
            &["node-p0a:50051".to_string(), "node-p0b:50051".to_string(),],
        );
        assert_eq!(
            addresses.address_for(PartitionId(1)),
            Some("node-p1a:50051")
        );
        Ok(())
    }

    #[test]
    fn test_membership_snapshot_bootstraps_from_node_addresses() {
        let addresses =
            NodeAddresses::from_config("0=node-p0a:50051|node-p0b:50051,1=node-p1a:50051");

        let snapshot =
            MembershipSnapshot::from_node_addresses(MembershipVersion::BOOTSTRAP, &addresses);

        assert_eq!(snapshot.version(), MembershipVersion::BOOTSTRAP);
        assert_eq!(snapshot.nodes().len(), 3);
        assert_eq!(snapshot.to_node_addresses(), addresses);
    }

    #[test]
    fn test_membership_snapshot_serialization_round_trip() -> anyhow::Result<()> {
        let mut node =
            NodeMembership::new(ClusterNodeId::new("p0a")?, PartitionId(0), "node-p0a:50051")?;
        node.http_origin = Some("http://node-p0a:3210".to_string());
        node.raft_addr = Some("node-p0a:50061".to_string());
        node.generation = 3;

        let snapshot = MembershipSnapshot::new(MembershipVersion::new(11), vec![node]);
        let bytes = NatsMembershipStore::encode(&snapshot)?;
        let decoded = NatsMembershipStore::decode(&bytes)?;

        assert_eq!(decoded, snapshot);
        Ok(())
    }
}
