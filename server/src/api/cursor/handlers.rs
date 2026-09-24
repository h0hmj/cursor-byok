//! Implements Cursor HTTP endpoints outside the Agent Run stream.
#[cfg(test)]
#[path = "routing_tests.rs"]
mod routing_tests;
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{DefaultBodyLimit, Extension, State},
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    routing::{get, post},
    Router,
};
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    api::cursor::{
        bidi,
        proxy::{self, CursorProxy},
        run_sse,
    },
    cursor::{
        compile::{rewrite_requested_model, EffortAction, ModelRewrite},
        protocol::{
            connect,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        services::{
            account, analytics, commit_message, compatibility, entitlement::FreeEntitlementCache,
            knowledge, model_catalog, server_config, tab,
        },
        subagent::{is_composer_model, ResolutionReason},
        transport::{ModelRole, PreparedRunModel, TransportParent, TransportRegistry},
    },
    Result,
};

pub fn router(
    registry: TransportRegistry,
    clients: crate::network::NetworkClients,
) -> Result<Router> {
    let proxy = CursorProxy::cursor(clients);
    let knowledge = knowledge::KnowledgeService::managed()?;
    Ok(router_with_proxy(registry, proxy, knowledge))
}

fn router_with_proxy(
    registry: TransportRegistry,
    proxy: CursorProxy,
    knowledge_service: knowledge::KnowledgeService,
) -> Router {
    let web_cache = registry.web_cache().router();
    let free_entitlements = FreeEntitlementCache::default();
    Router::new()
        .route("/__byok-api__/healthz", get(health))
        .route("/agent.v1.AgentService/RunSSE", post(run_sse_handler))
        .route("/aiserver.v1.BidiService/BidiAppend", post(bidi_handler))
        .route(
            "/aiserver.v1.AiService/AvailableDocs",
            post(compatibility::available_docs),
        )
        .route(
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            post(compatibility::effective_user_plugins),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            post(compatibility::user_privacy_mode),
        )
        .route(
            "/agent.v1.AgentService/UpdateConversationMetadata",
            post(compatibility::update_conversation_metadata),
        )
        .route(
            "/aiserver.v1.AiService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.AiService/AvailableModels",
            post(model_catalog::available_models),
        )
        .route(
            "/agent.v1.AgentService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/WriteGitCommitMessage",
            post(commit_message::write_git_commit_message),
        )
        .route(
            "/aiserver.v1.NetworkService/IsConnected",
            post(is_connected),
        )
        .route(
            "/agent.v1.AgentService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModel",
            post(model_catalog::default_model),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            post(model_catalog::default_model_nudge),
        )
        .route(
            "/aiserver.v1.AuthService/GetEmail",
            post(account::get_email),
        )
        .route(
            "/aiserver.v1.AuthService/GetUserMeta",
            post(account::get_user_meta),
        )
        .route("/aiserver.v1.DashboardService/GetMe", post(account::get_me))
        .route(
            "/aiserver.v1.DashboardService/GetTeams",
            post(account::get_teams),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserProfile",
            post(account::get_user_profile),
        )
        .route(
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            post(account::current_period_usage),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
            post(account::usage_limit_status),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseAdd",
            post(knowledge::add),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseList",
            post(knowledge::list),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseUpdate",
            post(knowledge::update),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseRemove",
            post(knowledge::remove),
        )
        .route(
            analytics::BOOTSTRAP_STATSIG_PATH,
            post(analytics::bootstrap_statsig),
        )
        .route("/auth/full_stripe_profile", get(account::stripe_profile))
        .route("/auth/stripe_profile", get(account::stripe_profile))
        .merge(tab::router())
        .route_layer(DefaultBodyLimit::disable())
        .route_layer(RequestDecompressionLayer::new())
        .fallback(proxy::forward)
        .method_not_allowed_fallback(proxy::forward)
        .layer(Extension(proxy))
        .layer(Extension(knowledge_service))
        .layer(Extension(free_entitlements))
        .with_state(registry)
        .merge(web_cache)
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `NetworkService/IsConnected` probe. Cursor's always-local extension checks
/// connectivity roughly 10s after any slow request starts; a 404/error here is
/// treated as "network disconnected" and aborts in-flight work (e.g. commit
/// message generation) even while the model is still streaming. Always answer
/// connected with an empty `IsConnectedResponse` so local BYOK generation is
/// never cancelled by this probe.
async fn is_connected() -> Result<Response<Body>> {
    let payload = connect::encode_message(&ai::IsConnectedResponse {})?;
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn run_sse_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request).await?;
    let request: agent::BidiRequestId = connect::decode_unary(&body)?;
    let route = registry.wait_route(&request.request_id).await;
    let trace = registry.trace(&request.request_id);
    trace.resume();
    trace.request(
        "run_sse_request",
        body.clone(),
        serde_json::json!({"request_id": request.request_id}),
    );
    match route {
        crate::cursor::transport::TransportRoute::Local => {
            run_sse::stream(&registry, &request.request_id).await
        }
        crate::cursor::transport::TransportRoute::Upstream(generation) => {
            let response = proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await?;
            Ok(run_sse::upstream(
                registry,
                request.request_id,
                generation,
                response,
                Some(trace),
            )
            .await)
        }
    }
}

async fn bidi_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (mut parts, body) = buffered(request).await?;
    let request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    let mut decoded = bidi::decode(&request)?;
    let prepared = select_run_model(&registry, &mut decoded, &parts.headers).await?;
    let conversation_id = decoded.conversation_id().map(str::to_owned);
    let trace = registry.trace(&decoded.request_id);
    // Validate parent headers before admission so a rejected append cannot bind identity.
    let parent = match parent_headers(&parts.headers) {
        Ok(parent) => parent,
        Err(error) => {
            let trace_metadata = decoded.trace_metadata();
            if prepared.is_some() {
                trace.begin(conversation_id.as_deref(), "rejected", decoded.model_id());
            } else {
                trace.resume();
            }
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "invalid_parent",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    };

    let (local, forwarded_body, admitted) = if let Some(prepared) = prepared {
        // Ownership + locality are decided together; decoded/body sync to the owned model.
        let admitted = registry
            .admit_run_model(&decoded.request_id, &prepared)
            .await?;
        sync_decoded_model(&mut decoded, &admitted)?;
        let changed = admitted.rewrites_wire(&prepared.original);
        let forwarded_body =
            forwarded_body(&decoded, &request, &body, &mut parts.headers, changed)?;
        tracing::info!(
            request_id = decoded.request_id,
            original_model = prepared.original,
            candidate_model = prepared.model,
            candidate_effort_action = ?prepared.effort,
            candidate_fast = ?prepared.fast,
            selected_model = admitted.model,
            selected_effort_action = ?admitted.effort,
            selected_fast = ?admitted.fast,
            selection_differs_from_candidate = prepared.model != admitted.model
                || prepared.effort != admitted.effort
                || prepared.fast != admitted.fast,
            route = if admitted.local { "local_byok" } else { "cursor_official" },
            "admitted Cursor Run model selection"
        );
        (admitted.local, forwarded_body, Some(admitted))
    } else if registry.local(&decoded.request_id).await.is_some() {
        (true, body.clone(), None)
    } else if registry.upstream(&decoded.request_id).await {
        (false, body.clone(), None)
    } else {
        let trace_metadata = decoded.trace_metadata();
        trace.resume();
        trace.request(
            "bidi_request",
            body.clone(),
            trace_outcome(trace_metadata, false, "missing_transport", None),
        );
        return Err(crate::Error::Protocol(
            "first BidiAppend message must select a model".into(),
        ));
    };

    let first_model = decoded.model_id().map(str::to_owned);
    let trace_metadata = decoded.trace_metadata();
    if first_model.is_some() {
        trace.begin(
            conversation_id.as_deref(),
            if local {
                "local_byok"
            } else {
                "cursor_official"
            },
            first_model.as_deref(),
        );
    } else {
        trace.resume();
    }
    if !local {
        trace.request(
            "bidi_request",
            body.clone(),
            trace_outcome(trace_metadata, true, "upstream", None),
        );
        return proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from(forwarded_body)),
        )
        .await;
    }
    match bidi::append(&registry, decoded, parent, admitted).await {
        Ok(_) => trace.request(
            "bidi_request",
            body,
            trace_outcome(trace_metadata, true, "local", None),
        ),
        Err(error) => {
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "command_rejected",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    }
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

fn forwarded_body(
    decoded: &bidi::DecodedAppend,
    request: &ai::BidiAppendRequest,
    original: &Bytes,
    headers: &mut HeaderMap,
    changed: bool,
) -> Result<Bytes> {
    if !changed {
        return Ok(original.clone());
    }
    let rewritten = decoded.rewritten_body(request, original)?;
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&rewritten.len().to_string()).expect("body length is a valid header"),
    );
    Ok(rewritten)
}

/// Resolve a Run policy candidate. Ownership and routing bind only in `admit_run_model`.
async fn select_run_model(
    registry: &TransportRegistry,
    decoded: &mut bidi::DecodedAppend,
    headers: &HeaderMap,
) -> Result<Option<PreparedRunModel>> {
    let Some(original) = decoded.model_id().map(str::to_owned) else {
        return Ok(None);
    };
    let Some(agent::agent_client_message::Message::RunRequest(request)) =
        decoded.message.message.as_mut()
    else {
        return Ok(None);
    };
    let Some(kind) = request
        .subagent_type_name
        .as_deref()
        .filter(|kind| !kind.is_empty())
    else {
        return Ok(Some(PreparedRunModel {
            original: original.clone(),
            model: original,
            effort: EffortAction::Unchanged,
            fast: None,
            role: ModelRole::Primary,
        }));
    };
    let (model, effort, fast) = if request
        .subagent_model_overrides
        .iter()
        .find(|entry| entry.subagent_type == kind)
        .is_some_and(|entry| {
            matches!(
                entry.selection,
                Some(agent::subagent_model_override::Selection::Disabled(true))
            )
        }) {
        // A per-type Disabled selection must not be overridden by YAML routing.
        (original.clone(), EffortAction::Unchanged, None)
    } else {
        let snapshot = registry.subagent_models().snapshot();
        let resolution = snapshot.resolve(kind, &original);
        // Task.model=inherit remains Original; YAML targets never use inherit.
        let model = if resolution.target.model == "inherit" {
            let parent = parent_headers(headers)?.ok_or_else(|| {
                crate::Error::Protocol("subagent model inherit requires parent headers".into())
            })?;
            registry
                .run_model(&parent.request_id)
                .await
                .ok_or_else(|| {
                    crate::Error::Protocol(
                        "subagent model inherit requires an active parent model".into(),
                    )
                })?
        } else {
            resolution.target.model.clone()
        };
        let (effort, fast) = match resolution.reason {
            ResolutionReason::Original => (EffortAction::Unchanged, None),
            _ if is_composer_model(&model) => (EffortAction::Clear, Some(resolution.target.fast)),
            _ => (
                EffortAction::Set(resolution.target.effort.clone().ok_or_else(|| {
                    crate::Error::Config(format!(
                        "subagent policy non-Composer target {model} requires effort"
                    ))
                })?),
                Some(resolution.target.fast),
            ),
        };
        tracing::info!(
            request_id = decoded.request_id,
            subagent_type = kind,
            policy_version = snapshot.version,
            reason = ?resolution.reason,
            original_model = original,
            candidate_model = model,
            candidate_effort_action = ?effort,
            candidate_fast = ?fast,
            "resolved child Run model policy candidate"
        );
        (model, effort, fast)
    };
    Ok(Some(PreparedRunModel {
        original,
        model,
        effort,
        fast,
        role: ModelRole::Child,
    }))
}

fn sync_decoded_model(
    decoded: &mut bidi::DecodedAppend,
    admitted: &crate::cursor::transport::AdmittedRun,
) -> Result<()> {
    let Some(agent::agent_client_message::Message::RunRequest(request)) =
        decoded.message.message.as_mut()
    else {
        return Ok(());
    };
    rewrite_requested_model(
        request,
        &ModelRewrite {
            model_id: admitted.model.clone(),
            effort: admitted.effort.clone(),
            fast: admitted.fast,
        },
    );
    Ok(())
}

/// Local BYOK models: plugin adapter IDs, or a hash present in `model_configs`.
#[cfg(test)]
async fn resolves_as_local_model(registry: &TransportRegistry, model_id: &str) -> Result<bool> {
    if model_id.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
        if crate::plugin::parse_model_id(model_id).is_none() {
            return Err(crate::Error::Config(format!(
                "invalid plugin model ID: {model_id}"
            )));
        }
        return Ok(true);
    }
    if registry.store().model(model_id).await?.is_some() {
        return Ok(true);
    }
    if model_id.len() == 16 && model_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::Error::Config(format!(
            "unknown local model hash: {model_id}"
        )));
    }
    Ok(false)
}

fn trace_outcome(
    mut metadata: serde_json::Value,
    accepted: bool,
    route_outcome: &str,
    error: Option<String>,
) -> serde_json::Value {
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert("accepted".into(), accepted.into());
        metadata.insert("route_outcome".into(), route_outcome.into());
        if let Some(error) = error {
            metadata.insert("error".into(), error.into());
        }
    }
    metadata
}

async fn buffered(request: Request<Body>) -> Result<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    Ok((parts, body))
}

fn parent_headers(headers: &HeaderMap) -> Result<Option<TransportParent>> {
    let request_id = header_text(headers, "x-parent-request-id")?;
    let tool_call_id = header_text(headers, "x-parent-agent-tool-call-id")?;
    match (request_id, tool_call_id) {
        (None, None) => Ok(None),
        (Some(request_id), Some(tool_call_id)) => Ok(Some(TransportParent {
            request_id: request_id.into(),
            tool_call_id: tool_call_id.into(),
        })),
        _ => Err(crate::Error::Protocol(
            "Cursor subagent request must include both parent headers".into(),
        )),
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    headers
        .get(name)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| crate::Error::Protocol(format!("invalid {name} header: {error}")))
}
