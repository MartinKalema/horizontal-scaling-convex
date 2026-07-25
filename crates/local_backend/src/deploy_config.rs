use std::{
    collections::BTreeMap,
    time::Duration,
};

use application::deploy_config::{
    EvaluatePushResponse,
    FinishPushDiff,
    SchemaStatusJson,
    StartPushRequest,
    StartPushResponse,
};
use axum::{
    debug_handler,
    extract::State,
    response::IntoResponse,
};
use common::{
    auth::{
        AuthInfo,
        SerializedAuthInfo,
    },
    bootstrap_model::components::definition::SerializedComponentDefinitionMetadata,
    components::ComponentId,
    http::{
        extract::{
            Json,
            MtState,
        },
        HttpResponseError,
    },
    types::Timestamp,
};
use database::partition::PartitionId;
use errors::{
    ErrorMetadata,
    ErrorMetadataAnyhowExt,
};
use fastrace::{
    collector::EventRecord,
    prelude::{
        SpanId,
        SpanRecord,
        TraceId,
    },
};
use keybroker::{
    AdminIdentityPrincipal,
    Identity,
    KeyBroker,
};
use model::{
    auth::types::AuthDiff,
    components::{
        config::{
            SerializedComponentDefinitionDiff,
            SerializedComponentDiff,
            SerializedSchemaChange,
        },
        type_checking::SerializedCheckedComponent,
        types::SerializedEvaluatedComponentDefinition,
    },
    config::ConfigModel,
    external_packages::types::ExternalDepsPackageId,
    modules::module_versions::SerializedAnalyzedModule,
    source_packages::{
        types::SourcePackage,
        SourcePackageModel,
    },
};
use serde::{
    de::DeserializeOwned,
    Deserialize,
    Serialize,
};
use serde_json::Value as JsonValue;
use value::{
    base64,
    DocumentObject,
    PublicDocumentId,
    TableNamespace,
};

use crate::{
    admin::{
        must_be_admin_from_key,
        must_be_admin_from_key_with_write_access,
    },
    authority_routing::{
        http_origin_from_peer_addr as resolve_http_origin_from_peer_addr,
        AttemptTimeoutDisposition,
        AuthorityAttemptError,
        AuthorityEndpointKind,
        AuthorityResolver,
    },
    DeployRouterState,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetConfigRequest {
    pub admin_key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetConfigHashesResponse {
    pub config: JsonValue,
    pub module_hashes: Vec<ModuleHashJson>,
    pub udf_server_version: Option<String>,
    pub node_version: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleHashJson {
    path: String,
    hash: String,
    environment: Option<String>,
}

pub async fn get_config_hashes(
    MtState(st): MtState<DeployRouterState>,
    Json(req): Json<GetConfigRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let identity = must_be_admin_from_key(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key,
    )
    .await?;

    let mut tx = st.application.begin(identity).await?;
    let component = ComponentId::Root; // This endpoint is not used in components push.
    let (config, modules, udf_config) = ConfigModel::new(&mut tx, component)
        .get_with_module_metadata()
        .await?;
    let module_hashes: Vec<_> = modules
        .into_iter()
        .map(|m| ModuleHashJson {
            path: m.path.clone().into(),
            hash: m.sha256.as_hex(),
            environment: Some(m.environment.to_string()),
        })
        .collect();
    let config = DocumentObject::try_from(config)?;
    let config: JsonValue = config.to_internal_json();

    let node_version = SourcePackageModel::new(&mut tx, TableNamespace::Global)
        .get_latest()
        .await?
        .and_then(|v| v.node_version.map(|v| v.into()));

    let udf_server_version = udf_config.map(|config| format!("{}", config.server_version));
    Ok(Json(GetConfigHashesResponse {
        config,
        module_hashes,
        udf_server_version,
        node_version,
    }))
}

fn deploy_target_partition(st: &DeployRouterState) -> anyhow::Result<Option<PartitionId>> {
    let Some(local_partition) = st.partition_id else {
        return Ok(None);
    };
    if local_partition != PartitionId::DEFAULT {
        return Ok(Some(PartitionId::DEFAULT));
    }
    let Some(raft_state) = st.raft_state.as_ref() else {
        return Ok(None);
    };
    anyhow::ensure!(
        raft_state.is_cluster_genesis_ready(),
        "Canonical cluster genesis is not Raft-confirmed by every partition"
    );
    if raft_state.is_leader_ready() {
        return Ok(None);
    }
    anyhow::ensure!(
        !raft_state.is_leader(),
        "Raft leader is still applying its current-term barrier and cannot serve deploy metadata"
    );
    Ok(Some(PartitionId::DEFAULT))
}

pub(crate) fn http_origin_from_peer_addr(addr: &str) -> anyhow::Result<String> {
    resolve_http_origin_from_peer_addr(addr)
}

trait AdminKeyCarrier {
    fn admin_key(&self) -> &str;
    fn set_admin_key(&mut self, admin_key: String);
}

async fn metadata_owner_instance_name(
    owner_origin: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let url = format!("{owner_origin}/instance_name");
    let response = reqwest::Client::builder()
        .timeout(timeout)
        .build()?
        .get(&url)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "Metadata owner instance-name request to {url} failed with HTTP {status}: {body}"
    );
    let instance_name = body.trim();
    anyhow::ensure!(
        !instance_name.is_empty(),
        "Metadata owner at {owner_origin} returned an empty instance name"
    );
    Ok(instance_name.to_owned())
}

fn issue_owner_admin_key(
    st: &DeployRouterState,
    owner_instance_name: &str,
    identity: &Identity,
) -> anyhow::Result<String> {
    let (principal, is_read_only) = match identity {
        Identity::InstanceAdmin(admin_identity) | Identity::ActingUser(admin_identity, _) => (
            admin_identity.principal().clone(),
            admin_identity.is_read_only(),
        ),
        _ => anyhow::bail!("Deploy metadata forwarding requires an authenticated admin identity"),
    };
    let owner_key_broker = KeyBroker::new(owner_instance_name, st.instance_secret)?;
    let owner_admin_key = match principal {
        AdminIdentityPrincipal::Member(member_id) => {
            if is_read_only {
                owner_key_broker.issue_read_only_admin_key(member_id)
            } else {
                owner_key_broker.issue_admin_key(member_id)
            }
        },
        AdminIdentityPrincipal::Team(_) => anyhow::bail!(
            "Deploy metadata forwarding does not yet support team-scoped admin identities"
        ),
    };
    Ok(owner_admin_key.as_string())
}

async fn maybe_forward_deploy_request<Req, Resp>(
    st: &DeployRouterState,
    path: &str,
    req: &mut Req,
    needs_write_access: bool,
) -> anyhow::Result<Option<Resp>>
where
    Req: Serialize + AdminKeyCarrier,
    Resp: DeserializeOwned,
{
    let Some(target_partition) = deploy_target_partition(st)? else {
        return Ok(None);
    };
    let identity = if needs_write_access {
        must_be_admin_from_key_with_write_access(
            st.application.app_auth(),
            st.instance_name.clone(),
            req.admin_key().to_owned(),
        )
        .await?
    } else {
        must_be_admin_from_key(
            st.application.app_auth(),
            st.instance_name.clone(),
            req.admin_key().to_owned(),
        )
        .await?
    };
    let resolver = AuthorityResolver::new(
        st.node_addresses.clone(),
        st.membership_store.clone(),
        st.raft_state.clone(),
        None,
        st.raft_peer_http_origins.clone(),
    );
    let resolved = resolver
        .route(
            target_partition,
            AuthorityEndpointKind::Http,
            AttemptTimeoutDisposition::Retry,
            |attempt| async move {
                metadata_owner_instance_name(&attempt.endpoint, attempt.timeout)
                    .await
                    .map(|instance_name| (attempt.endpoint, instance_name))
                    .map_err(AuthorityAttemptError::retry)
            },
        )
        .await?;
    let remaining = resolved.remaining()?;
    let (target_origin, target_instance_name) = resolved.value;
    let target_admin_key = issue_owner_admin_key(st, &target_instance_name, &identity)?;
    req.set_admin_key(target_admin_key);
    Ok(Some(
        forward_deploy_request(&target_origin, path, req, remaining).await?,
    ))
}

async fn forward_deploy_request<Req, Resp>(
    owner_origin: &str,
    path: &str,
    req: &Req,
    timeout: Duration,
) -> anyhow::Result<Resp>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    let url = format!("{owner_origin}{path}");
    let response = reqwest::Client::builder()
        .timeout(timeout)
        .build()?
        .post(&url)
        .json(req)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "Metadata owner request to {url} failed with HTTP {status}: {body}"
    );
    Ok(serde_json::from_str(&body)?)
}

impl TryFrom<StartPushResponse> for SerializedStartPushResponse {
    type Error = anyhow::Error;

    fn try_from(value: StartPushResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            environment_variables: value
                .environment_variables
                .into_iter()
                .map(|(k, v)| Ok((String::from(k), String::from(v))))
                .collect::<anyhow::Result<_>>()?,
            external_deps_id: value
                .external_deps_id
                .map(|id| String::from(PublicDocumentId::from(id))),
            component_definition_packages: value
                .component_definition_packages
                .into_iter()
                .map(|(k, v)| {
                    Ok((
                        String::from(k),
                        JsonValue::from(DocumentObject::try_from(v)?),
                    ))
                })
                .collect::<anyhow::Result<_>>()?,
            app_auth: value
                .app_auth
                .into_iter()
                .map(SerializedAuthInfo::try_from)
                .collect::<anyhow::Result<_>>()?,
            analysis: value
                .analysis
                .into_iter()
                .map(|(k, v)| Ok((String::from(k), v.try_into()?)))
                .collect::<anyhow::Result<_>>()?,
            app: value.app.try_into()?,
            schema_change: value.schema_change.try_into()?,
        })
    }
}

impl TryFrom<SerializedStartPushResponse> for StartPushResponse {
    type Error = anyhow::Error;

    fn try_from(value: SerializedStartPushResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            environment_variables: value
                .environment_variables
                .into_iter()
                .map(|(k, v)| Ok((k.parse()?, v.parse()?)))
                .collect::<anyhow::Result<_>>()?,
            external_deps_id: value
                .external_deps_id
                .map(|id| anyhow::Ok(ExternalDepsPackageId::from(id.parse::<PublicDocumentId>()?)))
                .transpose()?,
            component_definition_packages: value
                .component_definition_packages
                .into_iter()
                .map(|(k, v)| {
                    Ok((
                        k.parse()?,
                        SourcePackage::try_from(DocumentObject::try_from(v)?)?,
                    ))
                })
                .collect::<anyhow::Result<_>>()?,
            app_auth: value
                .app_auth
                .into_iter()
                .map(AuthInfo::try_from)
                .collect::<anyhow::Result<_>>()?,
            analysis: value
                .analysis
                .into_iter()
                .map(|(k, v)| Ok((k.parse()?, v.try_into()?)))
                .collect::<anyhow::Result<_>>()?,
            app: value.app.try_into()?,
            schema_change: value.schema_change.try_into()?,
        })
    }
}

impl AdminKeyCarrier for StartPushRequest {
    fn admin_key(&self) -> &str {
        &self.admin_key
    }

    fn set_admin_key(&mut self, admin_key: String) {
        self.admin_key = admin_key;
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedStartPushResponse {
    environment_variables: BTreeMap<String, String>,

    // Pointers to uploaded code.
    external_deps_id: Option<String>,
    component_definition_packages: BTreeMap<String, JsonValue>,

    // Analysis results.
    app_auth: Vec<SerializedAuthInfo>,
    analysis: BTreeMap<String, SerializedEvaluatedComponentDefinition>,

    // Typechecking results.
    app: SerializedCheckedComponent,

    // Schema changes.
    schema_change: SerializedSchemaChange,
}

impl TryFrom<EvaluatePushResponse> for SerializedEvaluatePushResponse {
    type Error = anyhow::Error;

    fn try_from(value: EvaluatePushResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            schema_change: value.schema_change.try_into()?,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedEvaluatePushResponse {
    schema_change: SerializedSchemaChange,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzedComponent {
    definition: SerializedComponentDefinitionMetadata,
    schema: Option<JsonValue>,
    modules: BTreeMap<String, SerializedAnalyzedModule>,
}

#[debug_handler]
pub async fn start_push(
    State(st): State<DeployRouterState>,
    Json(mut req): Json<StartPushRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    if let Some(forwarded) =
        maybe_forward_deploy_request(&st, "/api/deploy/start_push", &mut req, true).await?
    {
        return Ok(Json(forwarded));
    }

    let _identity = must_be_admin_from_key_with_write_access(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key.clone(),
    )
    .await?;
    let config = req.into_project_config().map_err(|e| {
        anyhow::Error::new(ErrorMetadata::bad_request("InvalidConfig", e.to_string()))
    })?;
    let result =
        st.application.start_push(&config).await.map_err(|e| {
            e.wrap_error_message(|msg| format!("Hit an error while pushing:\n{msg}"))
        })?;
    Ok(Json(SerializedStartPushResponse::try_from(
        result.response,
    )?))
}

// This endpoint is similar to `start_push`, but it doesn’t save the schema (so
// it won’t start schema validation/index backfill). It can be used to determine
// what will be the effects of a large push without starting work that can take
// a long time on large instances.
pub async fn evaluate_push(
    MtState(st): MtState<DeployRouterState>,
    Json(mut req): Json<StartPushRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    if let Some(forwarded) =
        maybe_forward_deploy_request(&st, "/api/deploy/evaluate_push", &mut req, true).await?
    {
        return Ok(Json(forwarded));
    }

    let _identity = must_be_admin_from_key_with_write_access(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key.clone(),
    )
    .await?;
    let config = req.into_project_config().map_err(|e| {
        anyhow::Error::new(ErrorMetadata::bad_request("InvalidConfig", e.to_string()))
    })?;
    let resp =
        st.application.evaluate_push(&config).await.map_err(|e| {
            e.wrap_error_message(|msg| format!("Hit an error while pushing:\n{msg}"))
        })?;

    Ok(Json(SerializedEvaluatePushResponse::try_from(resp)?))
}

const DEFAULT_SCHEMA_TIMEOUT_MS: u32 = 10_000;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForSchemaRequest {
    admin_key: String,
    schema_change: SerializedSchemaChange,
    timeout_ms: Option<u32>,
}

impl AdminKeyCarrier for WaitForSchemaRequest {
    fn admin_key(&self) -> &str {
        &self.admin_key
    }

    fn set_admin_key(&mut self, admin_key: String) {
        self.admin_key = admin_key;
    }
}

pub async fn wait_for_schema(
    MtState(st): MtState<DeployRouterState>,
    Json(mut req): Json<WaitForSchemaRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    if let Some(forwarded) =
        maybe_forward_deploy_request(&st, "/api/deploy/wait_for_schema", &mut req, false).await?
    {
        return Ok(Json(forwarded));
    }

    let identity = must_be_admin_from_key(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key,
    )
    .await?;
    let timeout = Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_SCHEMA_TIMEOUT_MS) as u64);
    let schema_change = req.schema_change.try_into()?;

    // In dry_run mode, we commit the schema changes in start_push so we can
    // validate the schema against existing data.
    let resp = st
        .application
        .wait_for_schema(identity, schema_change, timeout)
        .await?;
    Ok(Json(SchemaStatusJson::from(resp)))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishPushRequest {
    pub admin_key: String,
    start_push: SerializedStartPushResponse,
    pub dry_run: bool,
}

impl AdminKeyCarrier for FinishPushRequest {
    fn admin_key(&self) -> &str {
        &self.admin_key
    }

    fn set_admin_key(&mut self, admin_key: String) {
        self.admin_key = admin_key;
    }
}

/// Internal version that returns the commit timestamp for use by conductor
pub async fn finish_push_internal(
    st: &DeployRouterState,
    req: FinishPushRequest,
) -> anyhow::Result<(SerializedFinishPushDiff, Option<common::types::Timestamp>)> {
    let identity = must_be_admin_from_key_with_write_access(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key.clone(),
    )
    .await?;

    let start_push = StartPushResponse::try_from(req.start_push)?;

    // We can't actually run `finish_push` in a dry run, since we rolled back all of
    // our changes during start push.
    if req.dry_run {
        tracing::info!("Skipping finish_push in dry run");
        let empty_diff = FinishPushDiff::default();
        return Ok((SerializedFinishPushDiff::try_from(empty_diff)?, None));
    }

    let (resp, ts) = st
        .application
        .finish_push(identity, start_push)
        .await
        .map_err(|e| e.wrap_error_message(|msg| format!("Hit an error while pushing:\n{msg}")))?;
    Ok((SerializedFinishPushDiff::try_from(resp)?, Some(ts)))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedFinishPushResponse {
    #[serde(flatten)]
    diff: SerializedFinishPushDiff,
    commit_ts: Option<u64>,
}

pub async fn finish_push(
    MtState(st): MtState<DeployRouterState>,
    Json(mut req): Json<FinishPushRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    if let Some(forwarded) = maybe_forward_deploy_request::<
        FinishPushRequest,
        SerializedFinishPushResponse,
    >(&st, "/api/deploy/finish_push", &mut req, true)
    .await?
    {
        if let Some(commit_ts) = forwarded.commit_ts {
            st.application
                .database()
                .wait_for_write_ts(Timestamp::try_from(commit_ts)?)
                .await;
        }
        return Ok(Json(forwarded));
    }

    let (diff, commit_ts) = finish_push_internal(&st, req).await?;
    Ok(Json(SerializedFinishPushResponse {
        diff,
        commit_ts: commit_ts.map(u64::from),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPushCompletedRequest {
    admin_key: String,
    spans: Vec<SerializedCompletedSpan>,
}

pub async fn report_push_completed(
    st: DeployRouterState,
    req: ReportPushCompletedRequest,
) -> anyhow::Result<Vec<SpanRecord>> {
    let _identity = must_be_admin_from_key_with_write_access(
        st.application.app_auth(),
        st.instance_name.clone(),
        req.admin_key.clone(),
    )
    .await?;
    let spans = req
        .spans
        .into_iter()
        .map(|s| s.try_into())
        .collect::<anyhow::Result<Vec<SpanRecord>>>()?;
    Ok(spans)
}

#[debug_handler]
pub async fn report_push_completed_handler(
    State(st): State<DeployRouterState>,
    Json(req): Json<ReportPushCompletedRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let spans = report_push_completed(st, req).await?;
    tracing::debug!("Received spans: {:?}", spans);
    Ok(Json(()))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedFinishPushDiff {
    auth_diff: AuthDiff,
    definition_diffs: BTreeMap<String, SerializedComponentDefinitionDiff>,
    component_diffs: BTreeMap<String, SerializedComponentDiff>,
}

impl TryFrom<FinishPushDiff> for SerializedFinishPushDiff {
    type Error = anyhow::Error;

    fn try_from(value: FinishPushDiff) -> Result<Self, Self::Error> {
        Ok(Self {
            auth_diff: value.auth_diff,
            definition_diffs: value
                .definition_diffs
                .into_iter()
                .map(|(k, v)| Ok((String::from(k), v.try_into()?)))
                .collect::<anyhow::Result<_>>()?,
            component_diffs: value
                .component_diffs
                .into_iter()
                .map(|(k, v)| Ok((String::from(k), v.try_into()?)))
                .collect::<anyhow::Result<_>>()?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedCompletedSpan {
    trace_id: String,
    parent_id: String,
    span_id: String,
    begin_time_unix_ns: String,
    duration_ns: String,
    name: String,
    properties: BTreeMap<String, String>,
    events: Vec<SerializedEventRecord>,
}

impl TryFrom<SerializedCompletedSpan> for SpanRecord {
    type Error = anyhow::Error;

    fn try_from(value: SerializedCompletedSpan) -> Result<Self, Self::Error> {
        let trace_id_buf = base64::decode_urlsafe(&value.trace_id)?;
        let trace_id = u128::from_le_bytes(trace_id_buf[..].try_into()?);

        let parent_id_buf = base64::decode_urlsafe(&value.parent_id)?;
        let parent_id = u64::from_le_bytes(parent_id_buf[..].try_into()?);

        let span_id_buf = base64::decode_urlsafe(&value.span_id)?;
        let span_id = u64::from_le_bytes(span_id_buf[..].try_into()?);

        let begin_time_unix_ns_buf = base64::decode_urlsafe(&value.begin_time_unix_ns)?;
        let begin_time_unix_ns = u64::from_le_bytes(begin_time_unix_ns_buf[..].try_into()?);

        let duration_ns_buf = base64::decode_urlsafe(&value.duration_ns)?;
        let duration_ns = u64::from_le_bytes(duration_ns_buf[..].try_into()?);

        let properties = value
            .properties
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect::<Vec<_>>();

        let events = value
            .events
            .into_iter()
            .map(|e| e.try_into())
            .collect::<anyhow::Result<_>>()?;

        Ok(Self {
            trace_id: TraceId(trace_id),
            parent_id: SpanId(parent_id),
            span_id: SpanId(span_id),
            begin_time_unix_ns,
            duration_ns,
            name: value.name.into(),
            properties,
            events,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedEventRecord {
    name: String,
    timestamp_unix_ns: String,
    properties: BTreeMap<String, String>,
}

impl TryFrom<SerializedEventRecord> for EventRecord {
    type Error = anyhow::Error;

    fn try_from(value: SerializedEventRecord) -> Result<Self, Self::Error> {
        let timestamp_unix_ns_buf = base64::decode_urlsafe(&value.timestamp_unix_ns)?;
        let timestamp_unix_ns = u64::from_le_bytes(timestamp_unix_ns_buf[..].try_into()?);
        let properties = value
            .properties
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect::<Vec<_>>();
        Ok(Self {
            name: value.name.into(),
            timestamp_unix_ns,
            properties,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::http_origin_from_peer_addr;

    #[test]
    fn peer_addr_to_http_origin_supports_host_port_pairs() -> anyhow::Result<()> {
        assert_eq!(
            http_origin_from_peer_addr("node-p0a:50051")?,
            "http://node-p0a:3210"
        );
        Ok(())
    }

    #[test]
    fn peer_addr_to_http_origin_supports_urls() -> anyhow::Result<()> {
        assert_eq!(
            http_origin_from_peer_addr("http://node-p1a:50051")?,
            "http://node-p1a:3210"
        );
        Ok(())
    }
}
