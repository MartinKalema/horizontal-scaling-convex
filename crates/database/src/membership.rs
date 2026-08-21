//! Cluster membership directory for horizontal scaling.
//!
//! Placement answers "which partition owns this data?" Membership answers
//! "which physical nodes can serve that partition right now?" Keeping these
//! separate prevents placement metadata from turning into a hardcoded address
//! book as the cluster grows.

use std::{
    collections::{
        btree_map::Entry,
        BTreeMap,
        BTreeSet,
    },
    fmt,
    time::{
        Duration,
        Instant,
        SystemTime,
        UNIX_EPOCH,
    },
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
    /// Monotonic renewal sequence assigned while updating the shared KV entry.
    /// `None` identifies static bootstrap entries that do not expire.
    pub lease_renewal_sequence: Option<u64>,
    /// Writer wall-clock deadline retained for diagnostics and compatibility.
    /// Liveness decisions must not use this value.
    pub heartbeat_expires_at_ms: Option<u64>,
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
            lease_renewal_sequence: None,
            heartbeat_expires_at_ms: None,
        })
    }

    fn lease_revision(&self) -> Option<LeaseRevision> {
        self.lease_renewal_sequence
            .or_else(|| self.heartbeat_expires_at_ms.map(|_| 0))
            .map(|renewal_sequence| LeaseRevision {
                generation: self.generation,
                renewal_sequence,
            })
    }
}

/// Returns wall-clock time for persisted heartbeat diagnostics only.
pub fn current_unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LeaseRevision {
    generation: u64,
    renewal_sequence: u64,
}

#[derive(Debug)]
struct LeaseObservation {
    revision: LeaseRevision,
    last_renewal_observed_at: Instant,
    expired: bool,
    last_clock_disagreement: Option<(bool, bool)>,
}

/// Resolves membership liveness from locally observed lease renewal age.
///
/// The persisted generation and renewal sequence identify a renewal, while the
/// elapsed time is measured only with this process's monotonic clock. A newly
/// started observer grants a leased member one full TTL to publish a renewal.
pub struct MembershipLivenessTracker {
    lease_ttl: Duration,
    observations: BTreeMap<ClusterNodeId, LeaseObservation>,
}

impl MembershipLivenessTracker {
    pub fn new(lease_ttl: Duration) -> Self {
        Self {
            lease_ttl,
            observations: BTreeMap::new(),
        }
    }

    pub fn live_snapshot(&mut self, snapshot: &MembershipSnapshot) -> MembershipSnapshot {
        self.live_snapshot_at(snapshot, Instant::now(), current_unix_timestamp_millis())
    }

    fn live_snapshot_at(
        &mut self,
        snapshot: &MembershipSnapshot,
        observed_at: Instant,
        diagnostic_now_ms: u64,
    ) -> MembershipSnapshot {
        let mut live_node_ids = BTreeSet::new();

        for node in snapshot.nodes().values() {
            let Some(revision) = node.lease_revision() else {
                if !node.draining {
                    live_node_ids.insert(node.node_id.clone());
                }
                continue;
            };

            let observation = match self.observations.entry(node.node_id.clone()) {
                Entry::Vacant(entry) => entry.insert(LeaseObservation {
                    revision,
                    last_renewal_observed_at: observed_at,
                    expired: false,
                    last_clock_disagreement: None,
                }),
                Entry::Occupied(entry) => entry.into_mut(),
            };

            if revision < observation.revision {
                tracing::warn!(
                    node_id = %node.node_id,
                    generation = revision.generation,
                    renewal_sequence = revision.renewal_sequence,
                    observed_generation = observation.revision.generation,
                    observed_renewal_sequence = observation.revision.renewal_sequence,
                    "Ignored regressed membership lease revision"
                );
                continue;
            }

            if revision > observation.revision {
                let recovered_after_expiry = observation.expired;
                observation.revision = revision;
                observation.last_renewal_observed_at = observed_at;
                observation.expired = false;
                if recovered_after_expiry {
                    tracing::info!(
                        node_id = %node.node_id,
                        generation = revision.generation,
                        renewal_sequence = revision.renewal_sequence,
                        "Membership lease renewed after local expiry"
                    );
                }
            }

            let lease_age =
                observed_at.saturating_duration_since(observation.last_renewal_observed_at);
            let renewal_is_live = lease_age < self.lease_ttl;
            if !renewal_is_live && !observation.expired {
                crate::metrics::log_membership_expiration();
                tracing::warn!(
                    node_id = %node.node_id,
                    generation = revision.generation,
                    renewal_sequence = revision.renewal_sequence,
                    lease_age_ms = duration_millis_saturated(lease_age),
                    lease_ttl_ms = duration_millis_saturated(self.lease_ttl),
                    diagnostic_expires_at_ms = ?node.heartbeat_expires_at_ms,
                    "Membership lease expired from locally observed renewal age"
                );
            }
            observation.expired = !renewal_is_live;

            if let Some(expires_at_ms) = node.heartbeat_expires_at_ms {
                let timestamp_is_live = expires_at_ms > diagnostic_now_ms;
                if timestamp_is_live != renewal_is_live {
                    let disagreement = (timestamp_is_live, renewal_is_live);
                    if observation.last_clock_disagreement != Some(disagreement) {
                        crate::metrics::log_membership_clock_skew_disagreement(
                            timestamp_is_live,
                            renewal_is_live,
                        );
                        tracing::warn!(
                            node_id = %node.node_id,
                            generation = revision.generation,
                            renewal_sequence = revision.renewal_sequence,
                            diagnostic_expires_at_ms = expires_at_ms,
                            diagnostic_now_ms,
                            timestamp_is_live,
                            renewal_is_live,
                            "Membership heartbeat timestamp disagrees with local lease age; \
                             renewal age remains authoritative"
                        );
                    }
                    observation.last_clock_disagreement = Some(disagreement);
                } else {
                    observation.last_clock_disagreement = None;
                }
            }

            if renewal_is_live && !node.draining {
                live_node_ids.insert(node.node_id.clone());
            }
        }

        MembershipSnapshot::new(
            snapshot.version(),
            snapshot
                .nodes()
                .values()
                .filter(|node| live_node_ids.contains(&node.node_id))
                .cloned()
                .collect(),
        )
    }
}

fn duration_millis_saturated(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
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

    pub fn with_registered_node(&self, mut node: NodeMembership) -> Self {
        let existing = self.nodes.get(&node.node_id);
        if let Some(existing) = existing {
            if node.generation < existing.generation {
                crate::metrics::log_membership_stale_generation_rejection();
                tracing::warn!(
                    node_id = %node.node_id,
                    rejected_generation = node.generation,
                    current_generation = existing.generation,
                    "Rejected stale membership generation"
                );
                return self.clone();
            }
        }

        let previous_renewal = existing
            .and_then(NodeMembership::lease_revision)
            .map(|revision| revision.renewal_sequence)
            .unwrap_or(0);
        node.lease_renewal_sequence = Some(previous_renewal.saturating_add(1));
        if existing == Some(&node) {
            return self.clone();
        }

        let mut nodes = self.nodes.clone();
        nodes.insert(node.node_id.clone(), node);
        MembershipSnapshot {
            version: MembershipVersion::new(u64::from(self.version).saturating_add(1)),
            nodes,
        }
    }

    pub fn to_node_addresses(&self) -> NodeAddresses {
        self.to_node_addresses_with_filter(|node| !node.draining)
    }

    fn to_node_addresses_with_filter(
        &self,
        keep: impl Fn(&NodeMembership) -> bool,
    ) -> NodeAddresses {
        let mut addresses: BTreeMap<PartitionId, Vec<String>> = BTreeMap::new();
        for node in self.nodes.values() {
            if !keep(node) {
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
    #[serde(default)]
    lease_renewal_sequence: Option<u64>,
    #[serde(default)]
    heartbeat_expires_at_ms: Option<u64>,
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
                    lease_renewal_sequence: node.lease_renewal_sequence,
                    heartbeat_expires_at_ms: node.heartbeat_expires_at_ms,
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
                membership.lease_renewal_sequence = node.lease_renewal_sequence;
                membership.heartbeat_expires_at_ms = node.heartbeat_expires_at_ms;
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
    async fn register_node(&self, node: NodeMembership) -> anyhow::Result<MembershipSnapshot>;
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

    async fn register_node(&self, node: NodeMembership) -> anyhow::Result<MembershipSnapshot> {
        for attempt in 0..10 {
            if let Some(entry) = self
                .kv
                .entry(MEMBERSHIP_CURRENT_KEY)
                .await
                .context("Membership: failed to read current snapshot")?
            {
                let current = Self::decode(&entry.value)?;
                let updated = current.with_registered_node(node.clone());
                if updated == current {
                    return Ok(current);
                }
                let bytes = Self::encode(&updated)?;
                let renewal_sequence = updated
                    .nodes()
                    .get(&node.node_id)
                    .and_then(|node| node.lease_renewal_sequence);
                match self
                    .kv
                    .update(MEMBERSHIP_CURRENT_KEY, bytes.into(), entry.revision)
                    .await
                {
                    Ok(kv_revision) => {
                        tracing::debug!(
                            node_id = %node.node_id,
                            generation = node.generation,
                            ?renewal_sequence,
                            kv_revision,
                            "Persisted membership lease renewal"
                        );
                        return Ok(updated);
                    },
                    Err(_) => {
                        tracing::debug!(
                            attempt,
                            node_id = %node.node_id,
                            "Membership: CAS conflict while registering node"
                        );
                    },
                }
            } else {
                let mut registered_node = node.clone();
                registered_node.lease_renewal_sequence = Some(1);
                let snapshot =
                    MembershipSnapshot::new(MembershipVersion::BOOTSTRAP, vec![registered_node]);
                let bytes = Self::encode(&snapshot)?;
                match self.kv.create(MEMBERSHIP_CURRENT_KEY, bytes.into()).await {
                    Ok(kv_revision) => {
                        tracing::debug!(
                            node_id = %node.node_id,
                            generation = node.generation,
                            renewal_sequence = 1,
                            kv_revision,
                            "Persisted initial membership lease"
                        );
                        return Ok(snapshot);
                    },
                    Err(_) => {
                        tracing::debug!(
                            attempt,
                            node_id = %node.node_id,
                            "Membership: create conflict while registering node"
                        );
                    },
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1 << attempt)).await;
        }
        anyhow::bail!(
            "Membership: failed to register node {} after CAS retries",
            node.node_id
        )
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
        node.lease_renewal_sequence = Some(17);
        node.heartbeat_expires_at_ms = Some(u64::MAX);

        let snapshot = MembershipSnapshot::new(MembershipVersion::new(11), vec![node]);
        let bytes = NatsMembershipStore::encode(&snapshot)?;
        let decoded = NatsMembershipStore::decode(&bytes)?;

        assert_eq!(decoded, snapshot);
        Ok(())
    }

    #[test]
    fn test_membership_snapshot_registration_replaces_advertised_address() -> anyhow::Result<()> {
        let existing = NodeMembership::new(
            ClusterNodeId::new("partition-0-peer-0")?,
            PartitionId(0),
            "node-p0a:50051",
        )?;
        let snapshot = MembershipSnapshot::new(MembershipVersion::BOOTSTRAP, vec![existing]);
        let mut replacement = NodeMembership::new(
            ClusterNodeId::new("partition-0-peer-0")?,
            PartitionId(0),
            "random-p0a-20260701:50051",
        )?;
        replacement.generation = 2;

        let updated = snapshot.with_registered_node(replacement.clone());

        assert_eq!(updated.version(), MembershipVersion::new(1));
        let registered = updated.nodes().get(&replacement.node_id).unwrap();
        assert_eq!(registered.grpc_addr, replacement.grpc_addr);
        assert_eq!(registered.generation, 2);
        assert_eq!(registered.lease_renewal_sequence, Some(1));
        assert_eq!(
            updated.to_node_addresses().address_for(PartitionId(0)),
            Some("random-p0a-20260701:50051")
        );

        let mut restarted = replacement;
        restarted.grpc_addr = "random-p0a-after-restart:50051".to_string();
        let renewed = updated.with_registered_node(restarted.clone());
        let registered = renewed.nodes().get(&restarted.node_id).unwrap();
        assert_eq!(registered.grpc_addr, restarted.grpc_addr);
        assert_eq!(registered.lease_renewal_sequence, Some(2));
        Ok(())
    }

    #[test]
    fn test_membership_liveness_uses_renewals_not_skewed_timestamps() -> anyhow::Result<()> {
        let healthy_past_id = ClusterNodeId::new("healthy-past")?;
        let healthy_future_id = ClusterNodeId::new("healthy-future")?;
        let stale_past_id = ClusterNodeId::new("stale-past")?;
        let stale_future_id = ClusterNodeId::new("stale-future")?;
        let mut snapshot = MembershipSnapshot::new(MembershipVersion::BOOTSTRAP, vec![]);

        for (node_id, heartbeat_expires_at_ms) in [
            (healthy_past_id.clone(), 1),
            (healthy_future_id.clone(), u64::MAX),
            (stale_past_id.clone(), 1),
            (stale_future_id.clone(), u64::MAX),
        ] {
            let mut node = NodeMembership::new(
                node_id.clone(),
                PartitionId(0),
                format!("{}:50051", node_id.as_str()),
            )?;
            node.heartbeat_expires_at_ms = Some(heartbeat_expires_at_ms);
            snapshot = snapshot.with_registered_node(node);
        }

        let ttl = Duration::from_secs(10);
        let started_at = Instant::now();
        let mut tracker = MembershipLivenessTracker::new(ttl);
        let initially_live = tracker.live_snapshot_at(&snapshot, started_at, 1_000);
        assert_eq!(initially_live.nodes().len(), 4);

        for node_id in [&healthy_past_id, &healthy_future_id] {
            snapshot =
                snapshot.with_registered_node(snapshot.nodes().get(node_id).unwrap().clone());
        }
        let renewed_at = started_at + Duration::from_secs(9);
        let renewed = tracker.live_snapshot_at(&snapshot, renewed_at, 1_000);
        assert_eq!(renewed.nodes().len(), 4);

        let live = tracker.live_snapshot_at(&snapshot, started_at + ttl, 1_000);
        assert!(live.nodes().contains_key(&healthy_past_id));
        assert!(live.nodes().contains_key(&healthy_future_id));
        assert!(!live.nodes().contains_key(&stale_past_id));
        assert!(!live.nodes().contains_key(&stale_future_id));
        Ok(())
    }

    #[test]
    fn test_membership_newer_generation_supersedes_older_generation() -> anyhow::Result<()> {
        let node_id = ClusterNodeId::new("partition-0-peer-0")?;
        let mut old = NodeMembership::new(node_id.clone(), PartitionId(0), "old:50051")?;
        old.generation = 7;
        old.heartbeat_expires_at_ms = Some(u64::MAX);
        let initial = MembershipSnapshot::new(MembershipVersion::BOOTSTRAP, vec![])
            .with_registered_node(old.clone());

        let mut replacement =
            NodeMembership::new(node_id.clone(), PartitionId(0), "replacement:50051")?;
        replacement.generation = 8;
        replacement.heartbeat_expires_at_ms = Some(1);
        let updated = initial.with_registered_node(replacement);
        let current = updated.nodes().get(&node_id).unwrap();
        assert_eq!(current.generation, 8);
        assert_eq!(current.grpc_addr, "replacement:50051");
        assert_eq!(current.lease_renewal_sequence, Some(2));

        old.grpc_addr = "late-old:50051".to_string();
        let rejected = updated.with_registered_node(old);
        assert_eq!(rejected, updated);

        let ttl = Duration::from_secs(10);
        let started_at = Instant::now();
        let mut tracker = MembershipLivenessTracker::new(ttl);
        tracker.live_snapshot_at(&initial, started_at, 1_000);
        let live = tracker.live_snapshot_at(&updated, started_at + ttl, 1_000);
        assert_eq!(live.nodes().get(&node_id).unwrap().generation, 8);
        Ok(())
    }

    #[test]
    fn test_membership_static_and_drain_liveness() -> anyhow::Result<()> {
        let static_node = NodeMembership::new(
            ClusterNodeId::new("bootstrap")?,
            PartitionId(0),
            "bootstrap:50051",
        )?;
        let draining_id = ClusterNodeId::new("draining")?;
        let mut draining =
            NodeMembership::new(draining_id.clone(), PartitionId(0), "draining:50051")?;
        draining.draining = true;
        draining.heartbeat_expires_at_ms = Some(u64::MAX);
        let snapshot = MembershipSnapshot::new(MembershipVersion::BOOTSTRAP, vec![static_node])
            .with_registered_node(draining);

        let ttl = Duration::from_secs(10);
        let started_at = Instant::now();
        let mut tracker = MembershipLivenessTracker::new(ttl);
        tracker.live_snapshot_at(&snapshot, started_at, 1_000);
        let live = tracker.live_snapshot_at(&snapshot, started_at + ttl * 10, 1_000);
        assert!(live.nodes().contains_key(&ClusterNodeId::new("bootstrap")?));
        assert!(!live.nodes().contains_key(&draining_id));
        Ok(())
    }
}
