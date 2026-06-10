use database::{
    partition::PartitionId,
    raft_partition::RaftPartitionState,
};
use errors::ErrorMetadata;

use crate::{
    ActionCallbackRouterState,
    AdminRouterState,
    DeployRouterState,
    ImportExportRouterState,
    RouterState,
};

pub(crate) const CLUSTER_COORDINATOR_PARTITION: PartitionId = PartitionId(0);

pub(crate) trait ClusterAuthorityContext {
    fn replica_mode(&self) -> bool;
    fn partition_id(&self) -> Option<PartitionId>;
    fn raft_state(&self) -> Option<&RaftPartitionState>;
}

impl ClusterAuthorityContext for DeployRouterState {
    fn replica_mode(&self) -> bool {
        self.replica_mode
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn raft_state(&self) -> Option<&RaftPartitionState> {
        self.raft_state.as_ref()
    }
}

impl ClusterAuthorityContext for AdminRouterState {
    fn replica_mode(&self) -> bool {
        self.replica_mode
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn raft_state(&self) -> Option<&RaftPartitionState> {
        self.raft_state.as_ref()
    }
}

impl ClusterAuthorityContext for ActionCallbackRouterState {
    fn replica_mode(&self) -> bool {
        self.replica_mode
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn raft_state(&self) -> Option<&RaftPartitionState> {
        self.raft_state.as_ref()
    }
}

impl ClusterAuthorityContext for ImportExportRouterState {
    fn replica_mode(&self) -> bool {
        self.replica_mode
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn raft_state(&self) -> Option<&RaftPartitionState> {
        self.raft_state.as_ref()
    }
}

impl ClusterAuthorityContext for RouterState {
    fn replica_mode(&self) -> bool {
        self.replica_mode
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn raft_state(&self) -> Option<&RaftPartitionState> {
        self.raft_state.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteAuthorityClass {
    AnyNodeSafe,
    PartitionOwner,
    PartitionLeader,
    CoordinatorOwner,
    FollowerSafeRead,
    ExplicitForwarding,
    ExternalSideEffectOwner,
    FailClosed,
}

impl RouteAuthorityClass {
    fn label(self) -> &'static str {
        match self {
            Self::AnyNodeSafe => "any-node safe",
            Self::PartitionOwner => "partition owner",
            Self::PartitionLeader => "partition leader",
            Self::CoordinatorOwner => "coordinator owner",
            Self::FollowerSafeRead => "follower-safe read",
            Self::ExplicitForwarding => "explicit forwarding",
            Self::ExternalSideEffectOwner => "external side-effect owner",
            Self::FailClosed => "fail closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteMatcher {
    Exact(&'static str),
    Prefix(&'static str),
}

impl RouteMatcher {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::Exact(expected) => path == expected,
            Self::Prefix(prefix) => path.starts_with(prefix),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteAuthorityRule {
    pub(crate) surface: &'static str,
    matcher: RouteMatcher,
    pub(crate) class: RouteAuthorityClass,
    pub(crate) rationale: &'static str,
}

impl RouteAuthorityRule {
    const fn exact(
        surface: &'static str,
        path: &'static str,
        class: RouteAuthorityClass,
        rationale: &'static str,
    ) -> Self {
        Self {
            surface,
            matcher: RouteMatcher::Exact(path),
            class,
            rationale,
        }
    }

    const fn prefix(
        surface: &'static str,
        prefix: &'static str,
        class: RouteAuthorityClass,
        rationale: &'static str,
    ) -> Self {
        Self {
            surface,
            matcher: RouteMatcher::Prefix(prefix),
            class,
            rationale,
        }
    }
}

const ANY_NODE_STATIC_SCHEMA: &str = "static schema or deploy-introspection route";
const EXPLICIT_DEPLOY_FORWARDING: &str =
    "deploy protocol handlers forward to the metadata owner and Raft leader before mutating state";
const COORDINATOR_GLOBAL_STATE: &str =
    "route touches deployment-global state and must run on partition 0 / Raft leader";

pub(crate) const LOCAL_BACKEND_API_ROUTE_AUTHORITIES: &[RouteAuthorityRule] = &[
    RouteAuthorityRule::exact(
        "dashboard OpenAPI schema",
        "/dashboard_openapi.json",
        RouteAuthorityClass::AnyNodeSafe,
        ANY_NODE_STATIC_SCHEMA,
    ),
    RouteAuthorityRule::exact(
        "platform OpenAPI schema",
        "/v1/openapi.json",
        RouteAuthorityClass::AnyNodeSafe,
        ANY_NODE_STATIC_SCHEMA,
    ),
    RouteAuthorityRule::exact(
        "deploy config hash read",
        "/get_config_hashes",
        RouteAuthorityClass::AnyNodeSafe,
        "CLI reads the target node before the deploy protocol push so each node can materialize \
         local modules",
    ),
    RouteAuthorityRule::exact(
        "sync subscription setup",
        "/sync",
        RouteAuthorityClass::CoordinatorOwner,
        "subscription setup creates global sync worker state and must run on the coordinator \
         owner until follower-safe subscription routing exists",
    ),
    RouteAuthorityRule::prefix(
        "deploy protocol metadata operation",
        "/deploy/",
        RouteAuthorityClass::ExplicitForwarding,
        EXPLICIT_DEPLOY_FORWARDING,
    ),
    RouteAuthorityRule::prefix(
        "internal action callback",
        "/actions/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "snapshot export",
        "/export/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "log sink route",
        "/logs/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "streaming import",
        "/streaming_import/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "dashboard app metrics",
        "/app_metrics/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "platform API",
        "/v1/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard test function runner",
        "/run_test_function",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "UDF execution stream",
        "/stream_udf_execution",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "function log stream",
        "/stream_function_logs",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard check admin key",
        "/check_admin_key",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard shapes",
        "/shapes2",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard indexes",
        "/get_indexes",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard delete tables",
        "/delete_tables",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard delete component",
        "/delete_component",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "dashboard source code",
        "/get_source_code",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "scheduled function table delete",
        "/delete_scheduled_functions_table",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "environment variable update",
        "/update_environment_variables",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "environment variable list",
        "/list_environment_variables",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "canonical URL update",
        "/update_canonical_url",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "cancel all scheduled jobs",
        "/cancel_all_jobs",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "cancel scheduled job",
        "/cancel_job",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "snapshot import",
        "/import",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::prefix(
        "snapshot import upload",
        "/import/",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "perform snapshot import",
        "/perform_import",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "cancel snapshot import",
        "/cancel_import",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export document deltas",
        "/document_deltas",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export snapshot list",
        "/list_snapshot",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export JSON schemas",
        "/json_schemas",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export connection test",
        "/test_streaming_export_connection",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export tables and columns",
        "/get_tables_and_columns",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
    RouteAuthorityRule::exact(
        "streaming export table column names",
        "/get_table_column_names",
        RouteAuthorityClass::CoordinatorOwner,
        COORDINATOR_GLOBAL_STATE,
    ),
];

pub(crate) fn normalize_local_backend_api_path(path: &str) -> &str {
    path.strip_prefix("/api")
        .filter(|stripped| stripped.starts_with('/'))
        .unwrap_or(path)
}

pub(crate) fn classify_local_backend_api_route(path: &str) -> Option<&'static RouteAuthorityRule> {
    let normalized_path = normalize_local_backend_api_path(path);
    LOCAL_BACKEND_API_ROUTE_AUTHORITIES
        .iter()
        .find(|rule| rule.matcher.matches(normalized_path))
}

fn unsupported_local_backend_cluster_surface_error(
    surface: &str,
    class: RouteAuthorityClass,
    reason: String,
) -> anyhow::Error {
    anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(format!(
        "Local backend API route {surface} ({}) cannot execute locally on this clustered node: \
         {reason}. Route the request to the authoritative node, add an explicit forwarding path, \
         or add a route-specific follower-safe read path.",
        class.label()
    ))
}

fn ensure_cluster_coordinator_authority(
    st: &impl ClusterAuthorityContext,
    surface: &str,
    class: RouteAuthorityClass,
) -> anyhow::Result<()> {
    if !st.replica_mode() && st.partition_id().is_none() {
        return Ok(());
    }
    if st.replica_mode() {
        return Err(unsupported_local_backend_cluster_surface_error(
            surface,
            class,
            "replicas do not own deployment-global request surfaces".to_string(),
        ));
    }
    let Some(partition_id) = st.partition_id() else {
        return Ok(());
    };
    if partition_id != CLUSTER_COORDINATOR_PARTITION {
        return Err(unsupported_local_backend_cluster_surface_error(
            surface,
            class,
            format!(
                "partition {} is not the cluster coordinator partition {}",
                partition_id.0, CLUSTER_COORDINATOR_PARTITION.0
            ),
        ));
    }
    if let Some(raft_state) = st.raft_state()
        && !raft_state.is_leader()
    {
        return Err(unsupported_local_backend_cluster_surface_error(
            surface,
            class,
            "the coordinator partition is running as a Raft follower".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn ensure_local_backend_api_authority(
    st: &impl ClusterAuthorityContext,
    path: &str,
) -> anyhow::Result<()> {
    let (surface, class) = if let Some(rule) = classify_local_backend_api_route(path) {
        (rule.surface, rule.class)
    } else {
        (path, RouteAuthorityClass::FailClosed)
    };
    match class {
        RouteAuthorityClass::AnyNodeSafe | RouteAuthorityClass::ExplicitForwarding => Ok(()),
        RouteAuthorityClass::CoordinatorOwner => {
            ensure_cluster_coordinator_authority(st, surface, class)
        },
        RouteAuthorityClass::PartitionOwner
        | RouteAuthorityClass::PartitionLeader
        | RouteAuthorityClass::FollowerSafeRead
        | RouteAuthorityClass::ExternalSideEffectOwner
        | RouteAuthorityClass::FailClosed => Err(unsupported_local_backend_cluster_surface_error(
            surface,
            class,
            format!("the {class:?} authority class has no local execution strategy yet"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_local_backend_api_route,
        normalize_local_backend_api_path,
        RouteAuthorityClass,
        LOCAL_BACKEND_API_ROUTE_AUTHORITIES,
    };

    fn class_for(path: &str) -> Option<RouteAuthorityClass> {
        classify_local_backend_api_route(path).map(|rule| rule.class)
    }

    #[test]
    fn test_normalizes_nested_api_prefix() {
        assert_eq!(
            normalize_local_backend_api_path("/api/deploy/start_push"),
            "/deploy/start_push"
        );
        assert_eq!(
            normalize_local_backend_api_path("/deploy/start_push"),
            "/deploy/start_push"
        );
        assert_eq!(normalize_local_backend_api_path("/apiary"), "/apiary");
    }

    #[test]
    fn test_local_backend_api_route_authority_registry_classifies_core_surfaces() {
        assert_eq!(
            class_for("/api/get_config_hashes"),
            Some(RouteAuthorityClass::AnyNodeSafe)
        );
        assert_eq!(
            class_for("/api/deploy/start_push"),
            Some(RouteAuthorityClass::ExplicitForwarding)
        );
        assert_eq!(
            class_for("/api/sync"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(
            class_for("/api/actions/mutation"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(
            class_for("/api/streaming_import/replace_tables"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(
            class_for("/api/logs/datadog_sink"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(
            class_for("/api/stream_function_logs"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(
            class_for("/api/app_metrics/function_concurrency"),
            Some(RouteAuthorityClass::CoordinatorOwner)
        );
        assert_eq!(class_for("/api/unregistered_route"), None);
    }

    #[test]
    fn test_local_backend_api_route_authority_rules_are_documented() {
        assert!(LOCAL_BACKEND_API_ROUTE_AUTHORITIES
            .iter()
            .all(|rule| !rule.surface.is_empty() && !rule.rationale.is_empty()));
    }
}
