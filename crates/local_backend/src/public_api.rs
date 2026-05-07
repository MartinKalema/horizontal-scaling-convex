use std::sync::Arc;

use anyhow::Context;
use application::{
    api::ExecuteQueryTimestamp,
    redaction::{
        RedactedJsError,
        RedactedLogLines,
    },
};
use axum::{
    debug_handler,
    extract::{
        DefaultBodyLimit,
        FromRef,
        State,
    },
    response::IntoResponse,
};
use common::{
    components::{
        CanonicalizedComponentFunctionPath,
        ComponentPath,
    },
    http::{
        extract::{
            Json,
            Path,
            Query,
        },
        ExtractClientVersion,
        ExtractRequestId,
        ExtractResolvedHostname,
        HttpResponseError,
    },
    knobs::MAX_BACKEND_PUBLIC_API_REQUEST_SIZE,
    types::FunctionCaller,
    version::ClientVersion,
};
use errors::ErrorMetadata;
use isolate::UdfArgsJson;
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::Value as JsonValue;
use sync_types::Timestamp;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use value::{
    export::ValueFormat,
    ConvexValue,
    JsonPackedValue,
};

use crate::{
    args_structs::UdfPostRequestWithComponent,
    authentication::ExtractAuthenticationToken,
    mutation_forwarder::MutationForwarderGrpcClient,
    parse::{
        parse_export_path,
        parse_udf_path,
    },
    RouterState,
};
#[derive(Deserialize, Debug, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UdfPostRequest {
    pub path: String,
    #[schema(value_type = Object)]
    pub args: UdfArgsJson,

    pub format: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ts {
    pub ts: SerializedTs,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UdfPostWithTsRequest {
    pub path: String,
    #[schema(value_type = Object)]
    pub args: UdfArgsJson,
    pub ts: SerializedTs,

    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct SerializedTs(String);

impl From<Timestamp> for SerializedTs {
    fn from(ts: Timestamp) -> Self {
        let n: u64 = ts.into();
        let bytes = base64::encode(n.to_le_bytes());
        SerializedTs(bytes)
    }
}
impl TryFrom<SerializedTs> for Timestamp {
    type Error = anyhow::Error;

    fn try_from(value: SerializedTs) -> anyhow::Result<Self> {
        let bytes = base64::decode(value.0)?;
        let array: [u8; 8] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Wrong number of bytes for SerializedTs."))?;
        let n = u64::from_le_bytes(array);
        Timestamp::try_from(n)
    }
}

#[derive(Deserialize)]
pub struct UdfArgsQuery {
    pub path: String,
    pub args: UdfArgsJson,

    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(tag = "status")]
#[serde(rename_all = "camelCase")]
pub enum UdfResponse {
    #[serde(rename_all = "camelCase")]
    Success {
        value: JsonValue,

        #[serde(skip_serializing_if = "RedactedLogLines::is_empty")]
        #[schema(value_type = Vec<String>)]
        log_lines: RedactedLogLines,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        error_message: String,

        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<JsonValue>,

        #[serde(skip_serializing_if = "RedactedLogLines::is_empty")]
        #[serde(default = "RedactedLogLines::empty")]
        #[schema(value_type = Vec<String>)]
        log_lines: RedactedLogLines,
    },
}

impl UdfResponse {
    pub fn nested_error(
        error: RedactedJsError,
        log_lines: RedactedLogLines,
        value_format: Option<ValueFormat>,
        client_version: ClientVersion,
    ) -> anyhow::Result<Self> {
        Self::_error(
            error.nested_to_string(),
            error,
            log_lines,
            value_format,
            client_version,
        )
    }

    pub fn error(
        error: RedactedJsError,
        log_lines: RedactedLogLines,
        value_format: Option<ValueFormat>,
        client_version: ClientVersion,
    ) -> anyhow::Result<Self> {
        Self::_error(
            format!("{error}"),
            error,
            log_lines,
            value_format,
            client_version,
        )
    }

    fn _error(
        error_message: String,
        error: RedactedJsError,
        log_lines: RedactedLogLines,
        value_format: Option<ValueFormat>,
        client_version: ClientVersion,
    ) -> anyhow::Result<Self> {
        Ok(Self::Error {
            error_message,
            error_data: error
                .custom_data_if_any()
                .map(|value| export_value(value, value_format, client_version))
                .transpose()?,
            log_lines,
        })
    }
}

async fn wait_for_local_mutation_visibility(
    st: &RouterState,
    commit_ts: Timestamp,
    require_replication_frontier: bool,
) {
    st.database.wait_for_write_ts(commit_ts).await;
    if require_replication_frontier {
        if let Some(partition_id) = st.partition_id {
            st.database
                .wait_for_replication_write_frontier(partition_id, commit_ts)
                .await;
        }
    }
}

async fn maybe_forward_public_mutation(
    st: &RouterState,
    path: &str,
    serialized_args: &sync_types::types::SerializedArgs,
    format: Option<&str>,
    identity: keybroker::Identity,
    client_version: &ClientVersion,
) -> Result<Option<Result<UdfResponse, HttpResponseError>>, HttpResponseError> {
    enum ForwardingMode {
        Replica(Arc<MutationForwarderGrpcClient>),
        RaftFollower(Arc<MutationForwarderGrpcClient>),
    }

    let forwarding_mode = if st.replica_mode {
        Some(ForwardingMode::Replica(
            st.replica_mutation_forwarder
                .as_ref()
                .context(
                    "Replica mutation forwarding is not configured. Set PRIMARY_GRPC_URL for \
                     replica mode.",
                )
                .map_err(|e| {
                    HttpResponseError::from(
                        anyhow::anyhow!(ErrorMetadata::service_unavailable()).context(e),
                    )
                })?
                .clone(),
        ))
    } else if let Some(raft_state) = st.raft_state.as_ref() {
        if raft_state.is_leader() {
            None
        } else {
            let leader_id = raft_state.leader_id();
            if leader_id == 0 {
                tracing::warn!(
                    "Skipping follower mutation forwarding because partition has no elected Raft \
                     leader yet"
                );
                return Ok(None);
            }
            let Some(leader_grpc_url) = st
                .raft_peer_grpc_urls
                .as_ref()
                .and_then(|urls| urls.get(&leader_id))
                .cloned()
            else {
                tracing::warn!(
                    "Skipping follower mutation forwarding because RAFT_PEERS is missing leader \
                     node {leader_id}"
                );
                return Ok(None);
            };
            match MutationForwarderGrpcClient::connect(&leader_grpc_url).await {
                Ok(client) => Some(ForwardingMode::RaftFollower(Arc::new(client))),
                Err(e) => {
                    tracing::warn!(
                        "Skipping follower mutation forwarding because leader {} at {} is not \
                         reachable yet: {:#}",
                        leader_id,
                        leader_grpc_url,
                        e
                    );
                    return Ok(None);
                },
            }
        }
    } else {
        None
    };

    let Some(forwarding_mode) = forwarding_mode else {
        return Ok(None);
    };
    let forwarder = match &forwarding_mode {
        ForwardingMode::Replica(client) | ForwardingMode::RaftFollower(client) => client,
    };

    let value_format = format.map(|f| f.parse()).transpose()?;
    let forwarded = match forwarder
        .forward(
            path,
            serialized_args.get(),
            identity,
            &client_version.to_string(),
        )
        .await
    {
        Ok(forwarded) => forwarded,
        Err(e) => match forwarding_mode {
            ForwardingMode::Replica(_) => {
                return Ok(Some(Err(anyhow::anyhow!(
                    ErrorMetadata::service_unavailable()
                )
                .context(format!("Failed to forward mutation to primary: {e:#}"))
                .into())));
            },
            ForwardingMode::RaftFollower(_) => {
                tracing::warn!(
                    "Skipping follower mutation forwarding because the leader RPC failed: {:#}",
                    e
                );
                return Ok(None);
            },
        },
    };

    let response = match forwarded.result {
        Some(pb::replication::forward_mutation_response::Result::Success(success)) => {
            if matches!(forwarding_mode, ForwardingMode::RaftFollower(_)) {
                wait_for_local_mutation_visibility(st, Timestamp::try_from(success.ts)?, true)
                    .await;
            }
            let packed = JsonPackedValue::from_network(success.value).context(
                ErrorMetadata::bad_request(
                    "InvalidForwardedMutationValue",
                    "Primary returned an invalid forwarded mutation value.",
                ),
            )?;
            UdfResponse::Success {
                value: export_value(packed.unpack()?, value_format, client_version.clone())?,
                log_lines: RedactedLogLines::from_vec(success.log_lines),
            }
        },
        Some(pb::replication::forward_mutation_response::Result::Error(error)) => {
            UdfResponse::Error {
                error_message: error.error_message,
                error_data: error
                    .error_data
                    .map(|value| serde_json::from_str(&value))
                    .transpose()
                    .context(ErrorMetadata::bad_request(
                        "InvalidForwardedMutationErrorData",
                        "Primary returned invalid forwarded mutation error data.",
                    ))?,
                log_lines: RedactedLogLines::from_vec(error.log_lines),
            }
        },
        None => {
            return Ok(Some(Err(anyhow::anyhow!(
                ErrorMetadata::service_unavailable()
            )
            .context("Primary returned an empty forwarded mutation response.")
            .into())));
        },
    };
    Ok(Some(Ok(response)))
}

/// Execute any function
///
/// Execute a query, mutation, or action function by name.
#[utoipa::path(
    post,
    path = "/function",
    request_body = UdfPostRequestWithComponent,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
pub async fn public_function_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostRequestWithComponent>,
) -> Result<impl IntoResponse, HttpResponseError> {
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;

    let component = req.component_path(&identity)?;
    let udf_path = parse_udf_path(&req.path)?;
    let component_function_path = CanonicalizedComponentFunctionPath {
        component,
        udf_path,
    };
    let udf_result = st
        .api
        .execute_any_function(
            &host,
            request_id,
            identity,
            component_function_path,
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let response = match udf_result {
        Ok(write_return) => UdfResponse::Success {
            value: export_value(write_return.value.unpack()?, value_format, client_version)?,
            log_lines: write_return.log_lines,
        },
        Err(write_error) => UdfResponse::error(
            write_error.error,
            write_error.log_lines,
            value_format,
            client_version,
        )?,
    };
    Ok(Json(response))
}

#[derive(Deserialize, ToSchema)]
pub struct UdfPostRequestArgsOnly {
    #[schema(value_type = Object)]
    pub args: UdfArgsJson,
    pub format: Option<String>,
}

/// Execute function by URL path
///
/// Execute a query, mutation, or action function by path in URL.
#[utoipa::path(
    post,
    path = "/run/{*functionIdentifier}",
    params(
        ("functionIdentifier" = String, Path, description = "Function path like messages/list")
    ),
    request_body = UdfPostRequestArgsOnly,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
pub async fn public_function_post_with_path(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    Path(path): Path<String>,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostRequestArgsOnly>,
) -> Result<impl IntoResponse, HttpResponseError> {
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;

    let bad_request_error = || {
        anyhow::anyhow!(ErrorMetadata::bad_request(
            "MissingIdentifier",
            "Path or function name not provided in path, e.g. /api/run/messages/list",
        ))
    };
    println!("{path:?}");

    // messages/list -> ["messages", "list"]
    let mut path_parts = path
        .as_str()
        .split('/')
        .map(|p| urlencoding::decode(p).map_err(|_e| bad_request_error()))
        .try_collect::<Vec<_>>()?;
    println!("{path_parts:?}");
    if path_parts.len() < 2 {
        return Err(bad_request_error().into());
    }
    let function_name = path_parts.pop().ok_or_else(bad_request_error)?;
    let udf_path_str = format!("{}:{}", path_parts.join("/"), function_name);
    let udf_path = parse_udf_path(&udf_path_str)?;
    let udf_result = st
        .api
        .execute_any_function(
            &host,
            request_id,
            identity,
            CanonicalizedComponentFunctionPath {
                // Only functions exported at the root can be called through this endpoint
                component: ComponentPath::root(),
                udf_path,
            },
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
        )
        .await?;
    // Default to ConvexCleanJSON if no format is provided.
    let value_format = match req.format.as_ref().map(|f| f.parse()).transpose()? {
        Some(format) => Some(format),
        None => Some(ValueFormat::ConvexCleanJSON),
    };
    let response = match udf_result {
        Ok(write_return) => UdfResponse::Success {
            value: export_value(write_return.value.unpack()?, value_format, client_version)?,
            log_lines: write_return.log_lines,
        },
        Err(write_error) => UdfResponse::error(
            write_error.error,
            write_error.log_lines,
            value_format,
            client_version,
        )?,
    };
    Ok(Json(response))
}

pub fn export_value(
    value: ConvexValue,
    value_format: Option<ValueFormat>,
    client_version: ClientVersion,
) -> anyhow::Result<JsonValue> {
    let format = match value_format {
        Some(value_format) => value_format,
        None => client_version.default_format(),
    };

    Ok(value.export(format))
}

/// Execute query (GET)
///
/// Execute a query function via GET request.
#[utoipa::path(
    get,
    path = "/query",
    params(
        ("path" = String, Query, description = "Function path"),
        ("args" = String, Query, description = "Function arguments as JSON string"),
        ("format" = Option<String>, Query, description = "Response format")
    ),
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
#[fastrace::trace(properties = { "udf_type": "query"})]
pub async fn public_query_get(
    State(st): State<RouterState>,
    Query(req): Query<UdfArgsQuery>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
) -> Result<impl IntoResponse, HttpResponseError> {
    let export_path = parse_export_path(&req.path)?;
    let journal = None;
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    let query_result = st
        .api
        .execute_public_query(
            &host,
            request_id,
            identity,
            export_path,
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
            ExecuteQueryTimestamp::Latest,
            journal,
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let log_lines = query_result.log_lines;
    let response = match query_result.result {
        Ok(value) => UdfResponse::Success {
            value: export_value(value.unpack()?, value_format, client_version)?,
            log_lines,
        },
        Err(error) => UdfResponse::error(error, log_lines, value_format, client_version)?,
    };
    Ok(Json(response))
}

/// Execute query (POST)
///
/// Execute a query function via POST request.
#[utoipa::path(
    post,
    path = "/query",
    request_body = UdfPostRequest,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
#[fastrace::trace(properties = { "udf_type": "query"})]
pub async fn public_query_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let udf_path = parse_export_path(&req.path)?;
    let journal = None;
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    let query_return = st
        .api
        .execute_public_query(
            &host,
            request_id,
            identity,
            udf_path,
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
            ExecuteQueryTimestamp::Latest,
            journal,
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let response = match query_return.result {
        Ok(value) => UdfResponse::Success {
            value: export_value(value.unpack()?, value_format, client_version)?,
            log_lines: query_return.log_lines,
        },
        Err(error) => {
            UdfResponse::error(error, query_return.log_lines, value_format, client_version)?
        },
    };
    Ok(Json(response))
}

/// Get latest timestamp
///
/// Get the latest timestamp for queries.
#[utoipa::path(
    post,
    path = "/query_ts",
    responses((status = 200, body = Ts)),
)]
#[debug_handler]
pub async fn public_get_query_ts(
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    State(st): State<RouterState>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let ts = *(st.api.latest_timestamp(&host, request_id).await?);
    Ok(Json(Ts { ts: ts.into() }))
}

/// Execute query at timestamp
///
/// Execute a query function at a specific timestamp.
#[utoipa::path(
    post,
    path = "/query_at_ts",
    request_body = UdfPostWithTsRequest,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
#[fastrace::trace(properties = { "udf_type": "query"})]
pub async fn public_query_at_ts_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostWithTsRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let export_path = parse_export_path(&req.path)?;
    let journal = None;
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    let ts = Timestamp::try_from(req.ts)?;
    let query_return = st
        .api
        .execute_public_query(
            &host,
            request_id,
            identity,
            export_path,
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
            ExecuteQueryTimestamp::At(ts),
            journal,
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let response = match query_return.result {
        Ok(value) => UdfResponse::Success {
            value: export_value(value.unpack()?, value_format, client_version)?,
            log_lines: query_return.log_lines,
        },
        Err(error) => {
            UdfResponse::error(error, query_return.log_lines, value_format, client_version)?
        },
    };
    Ok(Json(response))
}

#[derive(Deserialize, ToSchema)]
pub struct QueryBatchArgs {
    queries: Vec<UdfPostRequest>,
}

#[derive(Serialize, ToSchema)]
pub struct QueryBatchResponse {
    results: Vec<UdfResponse>,
}

/// Execute query batch
///
/// Execute multiple query functions in a batch.
#[utoipa::path(
    post,
    path = "/query_batch",
    request_body = QueryBatchArgs,
    responses((status = 200, body = QueryBatchResponse)),
)]
#[debug_handler]
pub async fn public_query_batch_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req_batch): Json<QueryBatchArgs>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let mut results = vec![];
    // All queries execute at the same timestamp.
    let ts = st.api.latest_timestamp(&host, request_id.clone()).await?;
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    for req in req_batch.queries {
        let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
        let export_path = parse_export_path(&req.path)?;
        let udf_return = st
            .api
            .execute_public_query(
                &host,
                request_id.clone(),
                identity.clone(),
                export_path,
                req.args.into_serialized_args()?,
                FunctionCaller::HttpApi(client_version.clone()),
                ExecuteQueryTimestamp::At(*ts),
                None,
            )
            .await?;
        let response = match udf_return.result {
            Ok(value) => UdfResponse::Success {
                value: export_value(value.unpack()?, value_format, client_version.clone())?,
                log_lines: udf_return.log_lines,
            },
            Err(error) => UdfResponse::error(
                error,
                udf_return.log_lines,
                value_format,
                client_version.clone(),
            )?,
        };
        results.push(response);
    }
    Ok(Json(QueryBatchResponse { results }))
}

/// Execute mutation
///
/// Execute a mutation function.
#[utoipa::path(
    post,
    path = "/mutation",
    request_body = UdfPostRequest,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
#[fastrace::trace(properties = { "udf_type": "mutation"})]
pub async fn public_mutation_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let export_path = parse_export_path(&req.path)?;
    let serialized_args = req.args.into_serialized_args()?;
    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    if let Some(response) = maybe_forward_public_mutation(
        &st,
        &req.path,
        &serialized_args,
        req.format.as_deref(),
        identity.clone(),
        &client_version,
    )
    .await?
    {
        return response.map(Json);
    }
    let udf_result = st
        .api
        .execute_public_mutation(
            &host,
            request_id,
            identity,
            export_path,
            serialized_args,
            FunctionCaller::HttpApi(client_version.clone()),
            None,
            None,
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let response = match udf_result {
        Ok(write_return) => {
            wait_for_local_mutation_visibility(&st, write_return.ts, false).await;
            UdfResponse::Success {
                value: export_value(write_return.value.unpack()?, value_format, client_version)?,
                log_lines: write_return.log_lines,
            }
        },
        Err(write_error) => UdfResponse::error(
            write_error.error,
            write_error.log_lines,
            value_format,
            client_version,
        )?,
    };
    Ok(Json(response))
}

/// Execute action
///
/// Execute an action function.
#[utoipa::path(
    post,
    path = "/action",
    request_body = UdfPostRequest,
    responses((status = 200, body = UdfResponse)),
)]
#[debug_handler]
#[fastrace::trace(properties = { "udf_type": "action"})]
pub async fn public_action_post(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestId(request_id): ExtractRequestId,
    ExtractAuthenticationToken(auth_token): ExtractAuthenticationToken,
    ExtractClientVersion(client_version): ExtractClientVersion,
    Json(req): Json<UdfPostRequest>,
) -> Result<impl IntoResponse, HttpResponseError> {
    let export_path = parse_export_path(&req.path)?;

    // NOTE: We could coalesce authenticating and executing the query into one
    // rpc but we keep things simple by reusing the same method as the sync worker.
    // Round trip latency between Usher and Backend is much smaller than between
    // client and Usher.
    let identity = st
        .api
        .authenticate(&host, request_id.clone(), auth_token)
        .await?;
    let action_result = st
        .api
        .execute_public_action(
            &host,
            request_id,
            identity,
            export_path,
            req.args.into_serialized_args()?,
            FunctionCaller::HttpApi(client_version.clone()),
        )
        .await?;
    let value_format = req.format.as_ref().map(|f| f.parse()).transpose()?;
    let response = match action_result {
        Ok(action_return) => UdfResponse::Success {
            value: export_value(action_return.value.unpack()?, value_format, client_version)?,
            log_lines: action_return.log_lines,
        },
        Err(action_error) => UdfResponse::error(
            action_error.error,
            action_error.log_lines,
            value_format,
            client_version,
        )?,
    };
    Ok(Json(response))
}

// The public (stable, no auth required) API of a deployment.
pub fn public_api_router<S>() -> OpenApiRouter<S>
where
    RouterState: FromRef<S>,
    S: Clone + Send + Sync + 'static,
{
    OpenApiRouter::new()
        .routes(utoipa_axum::routes!(public_query_get))
        .routes(utoipa_axum::routes!(public_query_post))
        .routes(utoipa_axum::routes!(public_get_query_ts))
        .routes(utoipa_axum::routes!(public_query_at_ts_post))
        .routes(utoipa_axum::routes!(public_query_batch_post))
        .routes(utoipa_axum::routes!(public_mutation_post))
        .routes(utoipa_axum::routes!(public_action_post))
        .routes(utoipa_axum::routes!(public_function_post))
        .routes(utoipa_axum::routes!(public_function_post_with_path))
        .layer(DefaultBodyLimit::max(*MAX_BACKEND_PUBLIC_API_REQUEST_SIZE))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::Arc,
        time::Duration,
    };

    use anyhow::Context;
    use application::{
        api::{
            ApplicationApi,
            ExecuteQueryTimestamp,
        },
        test_helpers::ApplicationTestExt,
    };
    use axum::body::Body;
    use common::{
        http::{
            ConvexHttpService,
            NoopRouteMapper,
            RequestDestination,
            ResolvedHostname,
        },
        runtime::Runtime,
        types::FunctionCaller,
        version::ClientVersion,
    };
    use database::{
        partition::PartitionId,
        raft_partition::RaftPartitionState,
        two_phase::NodeAddresses,
    };
    use http::{
        Request,
        StatusCode,
    };
    use http_body_util::BodyExt;
    use keybroker::Identity;
    use metrics::SERVER_VERSION_STR;
    use runtime::prod::ProdRuntime;
    use serde_json::{
        json,
        Value as JsonValue,
    };
    use sync_types::types::SerializedArgs;
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;
    use tower::ServiceExt;

    use crate::{
        mutation_forwarder::{
            MutationForwarderGrpcClient,
            MutationForwarderService,
        },
        query_forwarding_api::SelectiveQueryForwardingApi,
        router::router,
        test_helpers::setup_backend_for_test,
        MAX_CONCURRENT_REQUESTS,
    };

    async fn expect_success_from_app<T: serde::de::DeserializeOwned>(
        app: &ConvexHttpService,
        req: Request<Body>,
    ) -> anyhow::Result<T> {
        let (parts, body) = app.router().clone().oneshot(req).await?.into_parts();
        let bytes = body.collect().await?.to_bytes();
        let msg = format!("Got response: {}", String::from_utf8_lossy(&bytes));
        assert_eq!(parts.status, StatusCode::OK, "{msg}");
        serde_json::from_slice(if bytes.is_empty() { b"null" } else { &bytes }).map_err(Into::into)
    }

    fn post_request(uri: &'static str, json_body: JsonValue) -> anyhow::Result<Request<Body>> {
        Ok(Request::builder()
            .uri(uri)
            .method("POST")
            .header("Content-Type", "application/json")
            .header("Host", "localhost")
            .body(Body::from(serde_json::to_vec(&json_body)?))?)
    }

    async fn http_format_tester(
        rt: ProdRuntime,
        uri: &'static str,
        udf: &'static str,
        args: JsonValue,
        format: Option<&'static str>,
        expected: Result<JsonValue, &'static str>,
    ) -> anyhow::Result<()> {
        let backend = setup_backend_for_test(rt).await?;
        backend.st.application.load_udf_tests_modules().await?;
        let mut json_body = json!({
            "path": udf,
            "args": args,
        });
        if let Some(format) = format {
            json_body["format"] = format.into();
        }
        let body = Body::from(serde_json::to_vec(&json_body)?);
        let req = Request::builder()
            .uri(uri)
            .method("POST")
            .header("Content-Type", "application/json")
            .header("Host", "localhost")
            .body(body)?;
        match expected {
            Ok(expected) => {
                let result: JsonValue = backend.expect_success(req).await?;
                assert_eq!(
                    result,
                    json!({
                        "status": "success",
                        "value": expected,
                    })
                );
            },
            Err(expected) => {
                backend
                    .expect_error(req, StatusCode::BAD_REQUEST, expected)
                    .await?;
            },
        };
        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_query_default(rt: ProdRuntime) -> anyhow::Result<()> {
        // The default format is clean JSON
        http_format_tester(
            rt,
            "/api/query",
            "values:intQuery",
            json!({}),
            None,
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_query_clean_json(rt: ProdRuntime) -> anyhow::Result<()> {
        http_format_tester(
            rt,
            "/api/query",
            "values:intQuery",
            json!({}),
            Some("json"),
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_query_records_recent_delta_interest(rt: ProdRuntime) -> anyhow::Result<()> {
        let backend = setup_backend_for_test(rt).await?;
        backend.st.application.load_udf_tests_modules().await?;

        let inserted: JsonValue = backend
            .expect_success(post_request(
                "/api/mutation",
                json!({
                    "path": "values:insertObject",
                    "args": { "obj": { "hello": "interest" } },
                }),
            )?)
            .await?;
        let inserted_id = inserted["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();

        let _: JsonValue = backend
            .expect_success(post_request(
                "/api/query",
                json!({
                    "path": "values:getObject",
                    "args": { "id": inserted_id },
                }),
            )?)
            .await?;

        let interest = backend.st.application.database().delta_interest_tracker();
        assert!(
            interest
                .snapshot()
                .iter()
                .any(|table| table.to_string() == "test"),
            "stateless HTTP query should warm recent delta interest for touched tables",
        );

        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_mutation_default(rt: ProdRuntime) -> anyhow::Result<()> {
        // The default format is clean JSON
        http_format_tester(
            rt,
            "/api/mutation",
            "values:intMutation",
            json!({}),
            None,
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_mutation_clean_json(rt: ProdRuntime) -> anyhow::Result<()> {
        http_format_tester(
            rt,
            "/api/mutation",
            "values:intMutation",
            json!({}),
            Some("json"),
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_mutation_forwards_when_replica_mode(rt: ProdRuntime) -> anyhow::Result<()> {
        let primary = setup_backend_for_test(rt.clone()).await?;
        let replica = setup_backend_for_test(rt.clone()).await?;
        primary.st.application.load_udf_tests_modules().await?;
        replica.st.application.load_udf_tests_modules().await?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let grpc_addr = listener.local_addr()?;
        let forwarder = MutationForwarderService::new(
            Arc::new(primary.st.application.clone()),
            primary.st.instance_name.clone(),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _server_handle = rt.spawn("test_mutation_forwarder", async move {
            let _ = tonic::transport::Server::builder()
                .add_service(forwarder.into_server())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let forwarder_client =
            MutationForwarderGrpcClient::connect(&format!("http://{grpc_addr}")).await?;
        let mut replica_state = replica.st.clone();
        replica_state.replica_mode = true;
        replica_state.replica_mutation_forwarder = Some(Arc::new(forwarder_client));
        let replica_app = ConvexHttpService::new(
            router(replica_state),
            "backend_forwarding_test",
            SERVER_VERSION_STR.to_string(),
            MAX_CONCURRENT_REQUESTS,
            Duration::from_secs(125),
            NoopRouteMapper,
        );

        let mutation_response: JsonValue = expect_success_from_app(
            &replica_app,
            post_request(
                "/api/mutation",
                json!({
                    "path": "values:insertObject",
                    "args": { "obj": { "hello": "forwarded" } },
                }),
            )?,
        )
        .await?;
        let inserted_id = mutation_response["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();

        let primary_query: JsonValue = primary
            .expect_success(post_request(
                "/api/query",
                json!({
                    "path": "values:getObject",
                    "args": { "id": inserted_id.clone() },
                }),
            )?)
            .await?;
        assert_eq!(primary_query["status"], "success");
        assert_eq!(primary_query["value"]["hello"], "forwarded");

        let replica_snapshot = replica.st.application.database().latest_snapshot()?;
        let mut replica_test_docs = 0;
        for entry in replica_snapshot.iter_table_summaries()? {
            let (_, _, table_name, summary) = entry?;
            if table_name.to_string() == "test" {
                replica_test_docs += summary.num_values();
            }
        }
        assert_eq!(
            replica_test_docs, 0,
            "replica should not execute the forwarded mutation locally",
        );

        let _ = shutdown_tx.send(());
        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_mutation_local_partitioned_write_does_not_wait_for_replication_frontier(
        rt: ProdRuntime,
    ) -> anyhow::Result<()> {
        let backend = setup_backend_for_test(rt).await?;
        backend.st.application.load_udf_tests_modules().await?;

        let mut partitioned_state = backend.st.clone();
        partitioned_state.partition_id = Some(PartitionId::DEFAULT);
        let partitioned_app = ConvexHttpService::new(
            router(partitioned_state),
            "backend_partitioned_local_mutation_test",
            SERVER_VERSION_STR.to_string(),
            MAX_CONCURRENT_REQUESTS,
            Duration::from_secs(125),
            NoopRouteMapper,
        );

        let response: JsonValue = tokio::time::timeout(
            Duration::from_secs(5),
            expect_success_from_app(
                &partitioned_app,
                post_request(
                    "/api/mutation",
                    json!({
                        "path": "values:insertObject",
                        "args": { "obj": { "hello": "partitioned-local" } },
                    }),
                )?,
            ),
        )
        .await
        .context(
            "local partitioned mutation should not hang waiting for a replication frontier",
        )??;

        let inserted_id = response["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();
        let stored: JsonValue = backend
            .expect_success(post_request(
                "/api/query",
                json!({
                    "path": "values:getObject",
                    "args": { "id": inserted_id },
                }),
            )?)
            .await?;
        assert_eq!(stored["value"]["hello"], "partitioned-local");

        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_query_forwarder_returns_touched_tables(rt: ProdRuntime) -> anyhow::Result<()> {
        let primary = setup_backend_for_test(rt.clone()).await?;
        primary.st.application.load_udf_tests_modules().await?;

        let inserted: JsonValue = primary
            .expect_success(post_request(
                "/api/mutation",
                json!({
                    "path": "values:insertObject",
                    "args": { "obj": { "hello": "forwarded query" } },
                }),
            )?)
            .await?;
        let inserted_id = inserted["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let grpc_addr = listener.local_addr()?;
        let forwarder = MutationForwarderService::new(
            Arc::new(primary.st.application.clone()),
            primary.st.instance_name.clone(),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _server_handle = rt.spawn("test_query_forwarder", async move {
            let _ = tonic::transport::Server::builder()
                .add_service(forwarder.into_server())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let forwarder_client =
            MutationForwarderGrpcClient::connect(&format!("http://{grpc_addr}")).await?;
        let args = SerializedArgs::from_args(vec![json!({ "id": inserted_id })])?;
        let forwarded = forwarder_client
            .forward_query(
                "values:getObject",
                args.get(),
                Identity::system(),
                FunctionCaller::HttpApi(ClientVersion::unknown()),
                None,
                None,
            )
            .await?;

        let touched_tables = primary
            .st
            .application
            .database()
            .token_table_names(&forwarded.token)?;
        assert_eq!(
            touched_tables
                .into_iter()
                .map(|table| table.to_string())
                .collect::<Vec<_>>(),
            vec!["test".to_string()]
        );
        let value: JsonValue = serde_json::from_str(forwarded.result?.as_str())?;
        assert_eq!(value["hello"], "forwarded query");

        let _ = shutdown_tx.send(());
        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_selective_query_forwarding_api_routes_partitioned_queries(
        rt: ProdRuntime,
    ) -> anyhow::Result<()> {
        let primary = setup_backend_for_test(rt.clone()).await?;
        let selective = setup_backend_for_test(rt.clone()).await?;
        primary.st.application.load_udf_tests_modules().await?;
        selective.st.application.load_udf_tests_modules().await?;

        let inserted: JsonValue = primary
            .expect_success(post_request(
                "/api/mutation",
                json!({
                    "path": "values:insertObject",
                    "args": { "obj": { "hello": "selective forwarded query" } },
                }),
            )?)
            .await?;
        let inserted_id = inserted["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let grpc_addr = listener.local_addr()?;
        let forwarder = MutationForwarderService::new(
            Arc::new(primary.st.application.clone()),
            primary.st.instance_name.clone(),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _server_handle = rt.spawn("test_selective_query_forwarder", async move {
            let _ = tonic::transport::Server::builder()
                .add_service(forwarder.into_server())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let api: Arc<dyn ApplicationApi> = Arc::new(SelectiveQueryForwardingApi::new(
            Arc::new(selective.st.application.clone()),
            selective.st.application.database().clone(),
            Some(PartitionId(1)),
            Some(NodeAddresses::from_config(&format!("0={grpc_addr}"))),
            None,
            None,
        ));
        let query_result = api
            .execute_public_query(
                &ResolvedHostname {
                    instance_name: selective.st.instance_name.clone(),
                    destination: RequestDestination::ConvexCloud,
                },
                common::RequestId::new(),
                Identity::system(),
                "values:getObject".parse()?,
                SerializedArgs::from_args(vec![json!({ "id": inserted_id })])?,
                FunctionCaller::HttpApi(ClientVersion::unknown()),
                ExecuteQueryTimestamp::Latest,
                None,
            )
            .await?;

        let value: JsonValue = serde_json::from_str(query_result.result?.as_str())?;
        assert_eq!(value["hello"], "selective forwarded query");

        let _ = shutdown_tx.send(());
        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_selective_query_forwarding_api_routes_authority_partition_follower_queries(
        rt: ProdRuntime,
    ) -> anyhow::Result<()> {
        let primary = setup_backend_for_test(rt.clone()).await?;
        let follower = setup_backend_for_test(rt.clone()).await?;
        primary.st.application.load_udf_tests_modules().await?;
        follower.st.application.load_udf_tests_modules().await?;

        let inserted: JsonValue = primary
            .expect_success(post_request(
                "/api/mutation",
                json!({
                    "path": "values:insertObject",
                    "args": { "obj": { "hello": "authority follower forwarded query" } },
                }),
            )?)
            .await?;
        let inserted_id = inserted["value"]
            .as_str()
            .context("insertObject should return a document id string")?
            .to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let grpc_addr = listener.local_addr()?;
        let forwarder = MutationForwarderService::new(
            Arc::new(primary.st.application.clone()),
            primary.st.instance_name.clone(),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _server_handle = rt.spawn("test_authority_partition_query_forwarder", async move {
            let _ = tonic::transport::Server::builder()
                .add_service(forwarder.into_server())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let mut raft_peer_grpc_urls = BTreeMap::new();
        raft_peer_grpc_urls.insert(3, format!("http://{grpc_addr}"));
        let api: Arc<dyn ApplicationApi> = Arc::new(SelectiveQueryForwardingApi::new(
            Arc::new(follower.st.application.clone()),
            follower.st.application.database().clone(),
            Some(PartitionId(0)),
            None,
            Some(RaftPartitionState::new_for_test(
                false,
                3,
                PartitionId(0),
                1,
            )),
            Some(raft_peer_grpc_urls),
        ));
        let query_result = api
            .execute_public_query(
                &ResolvedHostname {
                    instance_name: follower.st.instance_name.clone(),
                    destination: RequestDestination::ConvexCloud,
                },
                common::RequestId::new(),
                Identity::system(),
                "values:getObject".parse()?,
                SerializedArgs::from_args(vec![json!({ "id": inserted_id })])?,
                FunctionCaller::HttpApi(ClientVersion::unknown()),
                ExecuteQueryTimestamp::Latest,
                None,
            )
            .await?;

        let value: JsonValue = serde_json::from_str(query_result.result?.as_str())?;
        assert_eq!(value["hello"], "authority follower forwarded query");

        let _ = shutdown_tx.send(());
        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_action_default(rt: ProdRuntime) -> anyhow::Result<()> {
        // The default format is clean JSON
        http_format_tester(
            rt,
            "/api/action",
            "values:intAction",
            json!({}),
            None,
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_action_clean_json(rt: ProdRuntime) -> anyhow::Result<()> {
        http_format_tester(
            rt,
            "/api/action",
            "values:intAction",
            json!({}),
            Some("json"),
            Ok(json!("1")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_query_with_arg(rt: ProdRuntime) -> anyhow::Result<()> {
        http_format_tester(
            rt,
            "/api/query",
            "args_validation:stringArg",
            json!({"arg": "val"}),
            Some("json"),
            Ok(json!("val")),
        )
        .await
    }

    #[convex_macro::prod_rt_test]
    async fn test_http_query_legacy_list_args(rt: ProdRuntime) -> anyhow::Result<()> {
        http_format_tester(
            rt,
            "/api/query",
            "args_validation:stringArg",
            json!([{"arg": "val"}]),
            Some("json"),
            Ok(json!("val")),
        )
        .await
    }
}
