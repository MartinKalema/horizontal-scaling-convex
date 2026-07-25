use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    future::Future,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use database::{
    membership::MembershipStore,
    partition::PartitionId,
    raft_partition::RaftPartitionState,
};
use tokio::time::Instant;
use tonic::{
    metadata::{
        MetadataMap,
        MetadataValue,
    },
    Code,
    Status,
};

use crate::SharedNodeAddresses;

const AUTHORITY_REDIRECT_METADATA: &str = "x-convex-authority-redirect";
const AUTHORITY_LEADER_GRPC_URL_METADATA: &str = "x-convex-authority-grpc-url";
const INTERNAL_BACKEND_HTTP_PORT: u16 = 3210;

#[derive(Clone, Copy, Debug)]
pub(crate) enum AuthorityEndpointKind {
    Grpc,
    Http,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AttemptTimeoutDisposition {
    Retry,
    Fail,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthorityRoutingPolicy {
    total_timeout: Duration,
    per_attempt_timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Default for AuthorityRoutingPolicy {
    fn default() -> Self {
        Self {
            total_timeout: Duration::from_secs(5),
            per_attempt_timeout: Duration::from_secs(1),
            initial_backoff: Duration::from_millis(25),
            max_backoff: Duration::from_millis(250),
        }
    }
}

#[derive(Debug)]
pub(crate) struct AuthorityAttempt {
    pub endpoint: String,
    pub timeout: Duration,
}

#[derive(Debug)]
pub(crate) enum AuthorityAttemptError {
    Retry {
        error: anyhow::Error,
        leader_hint: Option<String>,
    },
    Fail(anyhow::Error),
}

impl AuthorityAttemptError {
    pub fn retry(error: impl Into<anyhow::Error>) -> Self {
        Self::Retry {
            error: error.into(),
            leader_hint: None,
        }
    }

    pub fn retry_with_leader(error: impl Into<anyhow::Error>, leader_hint: Option<String>) -> Self {
        Self::Retry {
            error: error.into(),
            leader_hint,
        }
    }

    pub fn fail(error: impl Into<anyhow::Error>) -> Self {
        Self::Fail(error.into())
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedAuthority<T> {
    pub value: T,
    deadline: Instant,
}

impl<T> ResolvedAuthority<T> {
    pub fn remaining(&self) -> anyhow::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Authority routing deadline elapsed before request dispatch"
        );
        Ok(remaining)
    }
}

#[derive(Clone)]
pub(crate) struct AuthorityResolver {
    node_addresses: SharedNodeAddresses,
    membership_store: Option<Arc<dyn MembershipStore>>,
    raft_state: Option<RaftPartitionState>,
    raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
    raft_peer_http_origins: Option<BTreeMap<u64, String>>,
    policy: AuthorityRoutingPolicy,
}

impl AuthorityResolver {
    pub fn new(
        node_addresses: SharedNodeAddresses,
        membership_store: Option<Arc<dyn MembershipStore>>,
        raft_state: Option<RaftPartitionState>,
        raft_peer_grpc_urls: Option<BTreeMap<u64, String>>,
        raft_peer_http_origins: Option<BTreeMap<u64, String>>,
    ) -> Self {
        Self {
            node_addresses,
            membership_store,
            raft_state,
            raft_peer_grpc_urls,
            raft_peer_http_origins,
            policy: AuthorityRoutingPolicy::default(),
        }
    }

    #[cfg(test)]
    pub fn with_policy(mut self, policy: AuthorityRoutingPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub async fn route<T, F, Fut>(
        &self,
        partition: PartitionId,
        endpoint_kind: AuthorityEndpointKind,
        timeout_disposition: AttemptTimeoutDisposition,
        mut attempt: F,
    ) -> anyhow::Result<ResolvedAuthority<T>>
    where
        F: FnMut(AuthorityAttempt) -> Fut,
        Fut: Future<Output = Result<T, AuthorityAttemptError>>,
    {
        let deadline = Instant::now() + self.policy.total_timeout;
        let mut backoff = self.policy.initial_backoff;
        let mut preferred_leader = None;
        let mut failures = Vec::new();

        loop {
            let candidates =
                self.candidates(partition, endpoint_kind, preferred_leader.as_deref())?;
            let mut received_leader_hint = false;
            let mut attempted = BTreeSet::new();
            if candidates.is_empty() {
                failures.push("membership has no live authority candidates".to_string());
            }

            for endpoint in candidates {
                attempted.insert(endpoint.clone());
                let remaining = deadline.saturating_duration_since(Instant::now());
                anyhow::ensure!(
                    !remaining.is_zero(),
                    "Authority routing to partition {} timed out after {:?}: {}",
                    partition,
                    self.policy.total_timeout,
                    failures.join("; "),
                );
                // Read-only attempts can use short probes and move on. Once a
                // side-effecting request may be dispatched, give it the full
                // remaining deadline because timing it out is ambiguous and we
                // intentionally will not replay it.
                let attempt_timeout = match timeout_disposition {
                    AttemptTimeoutDisposition::Retry => {
                        remaining.min(self.policy.per_attempt_timeout)
                    },
                    AttemptTimeoutDisposition::Fail => remaining,
                };
                let outcome = tokio::time::timeout(
                    attempt_timeout,
                    attempt(AuthorityAttempt {
                        endpoint: endpoint.clone(),
                        timeout: attempt_timeout,
                    }),
                )
                .await;
                match outcome {
                    Ok(Ok(value)) => return Ok(ResolvedAuthority { value, deadline }),
                    Ok(Err(AuthorityAttemptError::Fail(error))) => return Err(error),
                    Ok(Err(AuthorityAttemptError::Retry { error, leader_hint })) => {
                        failures.push(format!("{endpoint}: {error:#}"));
                        if let Some(leader_hint) = leader_hint
                            && !attempted.contains(&leader_hint)
                        {
                            preferred_leader = Some(leader_hint);
                            received_leader_hint = true;
                        }
                    },
                    Err(_) if matches!(timeout_disposition, AttemptTimeoutDisposition::Fail) => {
                        anyhow::bail!(
                            "Authority request to {endpoint} timed out after {attempt_timeout:?}; \
                             the request may have reached the authority and will not be replayed"
                        );
                    },
                    Err(_) => {
                        failures.push(format!(
                            "{endpoint}: attempt timed out after {attempt_timeout:?}"
                        ));
                    },
                }
                if received_leader_hint {
                    break;
                }
            }

            if received_leader_hint {
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "Authority routing to partition {} timed out after {:?}: {}",
                partition,
                self.policy.total_timeout,
                failures.join("; "),
            );
            let refresh_timeout = remaining.min(self.policy.per_attempt_timeout);
            match tokio::time::timeout(refresh_timeout, self.refresh_membership()).await {
                Ok(Ok(())) => {},
                Ok(Err(error)) => {
                    failures.push(format!("membership refresh failed: {error:#}"));
                },
                Err(_) => {
                    failures.push(format!(
                        "membership refresh timed out after {refresh_timeout:?}"
                    ));
                },
            }
            tokio::time::sleep(backoff.min(remaining)).await;
            backoff = (backoff * 2).min(self.policy.max_backoff);
        }
    }

    fn candidates(
        &self,
        partition: PartitionId,
        endpoint_kind: AuthorityEndpointKind,
        preferred_leader: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let mut candidates = Vec::new();
        if let Some(preferred_leader) = preferred_leader {
            candidates.push(preferred_leader.to_string());
        }
        if let Some(raft_state) = self
            .raft_state
            .as_ref()
            .filter(|state| state.partition_id() == partition)
        {
            let leader_id = raft_state.leader_id();
            if leader_id != 0 {
                let leader = match endpoint_kind {
                    AuthorityEndpointKind::Grpc => self
                        .raft_peer_grpc_urls
                        .as_ref()
                        .and_then(|urls| urls.get(&leader_id)),
                    AuthorityEndpointKind::Http => self
                        .raft_peer_http_origins
                        .as_ref()
                        .and_then(|origins| origins.get(&leader_id)),
                };
                if let Some(leader) = leader {
                    candidates.push(leader.clone());
                }
            }
        }
        let advertised_http_origins = self.node_addresses.http_origins_for(partition);
        if matches!(endpoint_kind, AuthorityEndpointKind::Http)
            && !advertised_http_origins.is_empty()
        {
            candidates.extend(advertised_http_origins);
        } else if let Some(addresses) = self
            .node_addresses
            .get()
            .and_then(|addresses| addresses.addresses_for(partition).map(<[_]>::to_vec))
        {
            for address in addresses {
                candidates.push(match endpoint_kind {
                    AuthorityEndpointKind::Grpc => address,
                    AuthorityEndpointKind::Http => http_origin_from_peer_addr(&address)?,
                });
            }
        }

        let mut seen = BTreeSet::new();
        candidates.retain(|candidate| seen.insert(candidate.clone()));
        Ok(candidates)
    }

    async fn refresh_membership(&self) -> anyhow::Result<()> {
        let Some(store) = self.membership_store.as_ref() else {
            return Ok(());
        };
        let snapshot = store
            .load()
            .await
            .context("Failed to reload cluster membership while routing")?;
        if let Some(snapshot) = snapshot {
            self.node_addresses.refresh_from_membership(&snapshot);
        }
        Ok(())
    }
}

pub(crate) fn http_origin_from_peer_addr(addr: &str) -> anyhow::Result<String> {
    let normalized = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    let mut url = url::Url::parse(&normalized)?;
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.set_port(Some(INTERNAL_BACKEND_HTTP_PORT))
        .map_err(|_| anyhow::anyhow!("Failed to map peer address {addr} to backend HTTP port"))?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub(crate) fn authority_redirect_status(
    reason: impl Into<String>,
    raft_state: Option<&RaftPartitionState>,
    raft_peer_grpc_urls: Option<&BTreeMap<u64, String>>,
) -> Status {
    let mut metadata = MetadataMap::new();
    metadata.insert(
        AUTHORITY_REDIRECT_METADATA,
        MetadataValue::from_static("true"),
    );
    if let Some(leader_url) = raft_state
        .map(RaftPartitionState::leader_id)
        .filter(|leader_id| *leader_id != 0)
        .and_then(|leader_id| raft_peer_grpc_urls.and_then(|urls| urls.get(&leader_id)))
        .and_then(|url| MetadataValue::try_from(url.as_str()).ok())
    {
        metadata.insert(AUTHORITY_LEADER_GRPC_URL_METADATA, leader_url);
    }
    Status::with_metadata(Code::FailedPrecondition, reason, metadata)
}

pub(crate) fn is_authority_redirect(error: &anyhow::Error) -> bool {
    authority_status(error)
        .is_some_and(|status| status.metadata().get(AUTHORITY_REDIRECT_METADATA).is_some())
}

pub(crate) fn authority_leader_hint(error: &anyhow::Error) -> Option<String> {
    authority_status(error)
        .and_then(|status| status.metadata().get(AUTHORITY_LEADER_GRPC_URL_METADATA))
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn authority_status(error: &anyhow::Error) -> Option<&Status> {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<Status>())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use database::membership::{
        ClusterNodeId,
        MembershipSnapshot,
        MembershipVersion,
        NodeMembership,
    };

    use super::*;

    struct StaticMembershipStore {
        snapshot: MembershipSnapshot,
    }

    struct BlockingMembershipStore;

    #[async_trait::async_trait]
    impl MembershipStore for StaticMembershipStore {
        async fn load(&self) -> anyhow::Result<Option<MembershipSnapshot>> {
            Ok(Some(self.snapshot.clone()))
        }

        async fn ensure_initialized(
            &self,
            _bootstrap_snapshot: MembershipSnapshot,
        ) -> anyhow::Result<MembershipSnapshot> {
            Ok(self.snapshot.clone())
        }

        async fn register_node(&self, _node: NodeMembership) -> anyhow::Result<MembershipSnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    #[async_trait::async_trait]
    impl MembershipStore for BlockingMembershipStore {
        async fn load(&self) -> anyhow::Result<Option<MembershipSnapshot>> {
            std::future::pending().await
        }

        async fn ensure_initialized(
            &self,
            _bootstrap_snapshot: MembershipSnapshot,
        ) -> anyhow::Result<MembershipSnapshot> {
            std::future::pending().await
        }

        async fn register_node(&self, _node: NodeMembership) -> anyhow::Result<MembershipSnapshot> {
            std::future::pending().await
        }
    }

    fn test_policy() -> AuthorityRoutingPolicy {
        AuthorityRoutingPolicy {
            total_timeout: Duration::from_millis(100),
            per_attempt_timeout: Duration::from_millis(20),
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
        }
    }

    fn snapshot(address: &str) -> anyhow::Result<MembershipSnapshot> {
        Ok(MembershipSnapshot::new(
            MembershipVersion::new(1),
            vec![NodeMembership::new(
                ClusterNodeId::new("p0-node")?,
                PartitionId::DEFAULT,
                address,
            )?],
        ))
    }

    fn snapshot_with_http(
        grpc_address: &str,
        http_origin: &str,
    ) -> anyhow::Result<MembershipSnapshot> {
        let mut node = NodeMembership::new(
            ClusterNodeId::new("p0-node")?,
            PartitionId::DEFAULT,
            grpc_address,
        )?;
        node.http_origin = Some(http_origin.to_string());
        Ok(MembershipSnapshot::new(
            MembershipVersion::new(1),
            vec![node],
        ))
    }

    #[tokio::test]
    async fn retries_live_candidates_when_first_endpoint_is_dead() -> anyhow::Result<()> {
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=dead:50051|live:50051",
            ))),
            None,
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        if attempt.endpoint == "live:50051" {
                            Ok("served")
                        } else {
                            Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "connection refused"
                            )))
                        }
                    }
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        assert_eq!(*attempts.lock().unwrap(), vec!["dead:50051", "live:50051"]);
        Ok(())
    }

    #[tokio::test]
    async fn stale_leader_redirect_targets_current_leader_immediately() -> anyhow::Result<()> {
        let raft_state = RaftPartitionState::new_for_test(false, 1, PartitionId::DEFAULT, 3);
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=follower:50051",
            ))),
            None,
            Some(raft_state),
            Some(BTreeMap::from([(1, "stale-leader:50051".to_string())])),
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        match attempt.endpoint.as_str() {
                            "stale-leader:50051" => Err(AuthorityAttemptError::retry_with_leader(
                                anyhow::anyhow!("not leader"),
                                Some("current-leader:50051".to_string()),
                            )),
                            "current-leader:50051" => Ok("served"),
                            endpoint => Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "unexpected endpoint {endpoint}"
                            ))),
                        }
                    }
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["stale-leader:50051", "current-leader:50051"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn leader_redirect_does_not_wait_for_membership_refresh() -> anyhow::Result<()> {
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=follower:50051",
            ))),
            Some(Arc::new(BlockingMembershipStore)),
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            resolver.route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        match attempt.endpoint.as_str() {
                            "follower:50051" => Err(AuthorityAttemptError::retry_with_leader(
                                anyhow::anyhow!("not leader"),
                                Some("current-leader:50051".to_string()),
                            )),
                            "current-leader:50051" => Ok("served"),
                            endpoint => Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "unexpected endpoint {endpoint}"
                            ))),
                        }
                    }
                },
            ),
        )
        .await
        .context("leader redirect waited for unavailable membership storage")??;

        assert_eq!(result.value, "served");
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["follower:50051", "current-leader:50051"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn tries_cached_candidates_before_membership_refresh() -> anyhow::Result<()> {
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=dead:50051|live:50051",
            ))),
            Some(Arc::new(BlockingMembershipStore)),
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            resolver.route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        if attempt.endpoint == "live:50051" {
                            Ok("served")
                        } else {
                            Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "connection refused"
                            )))
                        }
                    }
                },
            ),
        )
        .await
        .context("cached live candidate was blocked by membership refresh")??;

        assert_eq!(result.value, "served");
        assert_eq!(*attempts.lock().unwrap(), vec!["dead:50051", "live:50051"]);
        Ok(())
    }

    #[tokio::test]
    async fn stale_redirect_does_not_starve_untried_live_candidate() -> anyhow::Result<()> {
        let raft_state = RaftPartitionState::new_for_test(false, 1, PartitionId::DEFAULT, 3);
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=follower:50051|live-leader:50051",
            ))),
            None,
            Some(raft_state),
            Some(BTreeMap::from([(1, "dead-leader:50051".to_string())])),
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        match attempt.endpoint.as_str() {
                            "dead-leader:50051" => {
                                Err(AuthorityAttemptError::retry(anyhow::anyhow!("offline")))
                            },
                            "follower:50051" => Err(AuthorityAttemptError::retry_with_leader(
                                anyhow::anyhow!("stale not-leader response"),
                                Some("dead-leader:50051".to_string()),
                            )),
                            "live-leader:50051" => Ok("served"),
                            endpoint => Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "unexpected endpoint {endpoint}"
                            ))),
                        }
                    }
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["dead-leader:50051", "follower:50051", "live-leader:50051"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn refreshes_membership_after_endpoint_replacement() -> anyhow::Result<()> {
        let shared = SharedNodeAddresses::new(Some(
            database::two_phase::NodeAddresses::from_config("0=old:50051"),
        ));
        let resolver = AuthorityResolver::new(
            shared,
            Some(Arc::new(StaticMembershipStore {
                snapshot: snapshot("replacement:50051")?,
            })),
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                move |attempt| {
                    observed.lock().unwrap().push(attempt.endpoint.clone());
                    async move {
                        if attempt.endpoint == "replacement:50051" {
                            Ok("served")
                        } else {
                            Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                                "old endpoint unavailable"
                            )))
                        }
                    }
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["old:50051", "replacement:50051"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn uses_advertised_http_origin_after_membership_refresh() -> anyhow::Result<()> {
        let shared = SharedNodeAddresses::new(Some(
            database::two_phase::NodeAddresses::from_config("0=old:50051"),
        ));
        shared.refresh_from_membership(&snapshot_with_http(
            "replacement:55000",
            "http://replacement:38080/",
        )?);
        let resolver =
            AuthorityResolver::new(shared, None, None, None, None).with_policy(test_policy());

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Http,
                AttemptTimeoutDisposition::Retry,
                |attempt| async move {
                    if attempt.endpoint == "http://replacement:38080" {
                        Ok("served")
                    } else {
                        Err(AuthorityAttemptError::retry(anyhow::anyhow!(
                            "unexpected endpoint {}",
                            attempt.endpoint
                        )))
                    }
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        Ok(())
    }

    #[tokio::test]
    async fn all_unavailable_candidates_fail_within_total_deadline() -> anyhow::Result<()> {
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=dead-a:50051|dead-b:50051",
            ))),
            None,
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let started = Instant::now();

        let error = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                |_attempt| async {
                    Err::<(), _>(AuthorityAttemptError::retry(anyhow::anyhow!("unavailable")))
                },
            )
            .await
            .expect_err("all unavailable candidates must fail");

        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(format!("{error:#}").contains("timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn membership_refresh_cannot_outlive_total_deadline() -> anyhow::Result<()> {
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=dead:50051",
            ))),
            Some(Arc::new(BlockingMembershipStore)),
            None,
            None,
            None,
        )
        .with_policy(test_policy());
        let started = Instant::now();

        let error = tokio::time::timeout(
            Duration::from_millis(500),
            resolver.route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Retry,
                |_attempt| async {
                    Err::<(), _>(AuthorityAttemptError::retry(anyhow::anyhow!("unavailable")))
                },
            ),
        )
        .await
        .context("membership refresh outlived the routing deadline")?
        .expect_err("unavailable authority must fail");

        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(format!("{error:#}").contains("timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn side_effect_attempt_receives_remaining_total_deadline() -> anyhow::Result<()> {
        let policy = test_policy();
        let resolver = AuthorityResolver::new(
            SharedNodeAddresses::new(Some(database::two_phase::NodeAddresses::from_config(
                "0=leader:50051",
            ))),
            None,
            None,
            None,
            None,
        )
        .with_policy(policy);

        let result = resolver
            .route(
                PartitionId::DEFAULT,
                AuthorityEndpointKind::Grpc,
                AttemptTimeoutDisposition::Fail,
                move |attempt| async move {
                    assert!(
                        attempt.timeout > policy.per_attempt_timeout,
                        "a dispatched side effect must not inherit the short candidate timeout"
                    );
                    Ok("served")
                },
            )
            .await?;

        assert_eq!(result.value, "served");
        Ok(())
    }

    #[test]
    fn redirect_status_preserves_current_leader_endpoint() -> anyhow::Result<()> {
        let raft_state = RaftPartitionState::new_for_test(false, 2, PartitionId::DEFAULT, 1);
        let status = authority_redirect_status(
            "not leader",
            Some(&raft_state),
            Some(&BTreeMap::from([(2, "leader:50051".to_string())])),
        );
        let error = anyhow::Error::new(status);

        assert!(is_authority_redirect(&error));
        assert_eq!(
            authority_leader_hint(&error).as_deref(),
            Some("leader:50051")
        );
        Ok(())
    }
}
