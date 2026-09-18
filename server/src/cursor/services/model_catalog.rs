//! Publishes the configured model catalog to Cursor.
use std::{collections::HashSet, path::PathBuf, sync::OnceLock, time::Instant};

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri},
};
use bytes::{BufMut, BytesMut};
use parking_lot::Mutex;
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{
    api::cursor::proxy::{self, CursorProxy},
    cursor::{
        protocol::proto::agent::v1 as agent,
        services::startup_timing::{self, MetadataSource},
        transport::TransportRegistry,
    },
    model::{format_token_count, parse_token_count, ModelConfig},
    plugin::PluginModelDescriptor,
    Error, Result,
};

struct CatalogRefreshGate {
    inflight: Mutex<HashSet<String>>,
}

fn catalog_gate() -> &'static CatalogRefreshGate {
    static GATE: OnceLock<CatalogRefreshGate> = OnceLock::new();
    GATE.get_or_init(|| CatalogRefreshGate {
        inflight: Mutex::new(HashSet::new()),
    })
}

fn authorization_token_hash(headers: &HeaderMap) -> Option<[u8; 32]> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(Sha256::digest(token.as_bytes()).into())
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModelsAddition {
    #[prost(string, repeated, tag = "1")]
    model_names: Vec<String>,
    #[prost(message, repeated, tag = "2")]
    models: Vec<AvailableModel>,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModel {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(bool, tag = "2")]
    default_on: bool,
    #[prost(bool, optional, tag = "5")]
    supports_agent: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    degradation_status: Option<i32>,
    #[prost(message, optional, tag = "8")]
    tooltip_data: Option<TooltipData>,
    #[prost(bool, optional, tag = "9")]
    supports_thinking: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    supports_images: Option<bool>,
    #[prost(bool, optional, tag = "14")]
    supports_max_mode: Option<bool>,
    #[prost(string, optional, tag = "17")]
    client_display_name: Option<String>,
    #[prost(string, optional, tag = "18")]
    server_model_name: Option<String>,
    #[prost(bool, optional, tag = "19")]
    supports_non_max_mode: Option<bool>,
    #[prost(message, optional, tag = "20")]
    tooltip_data_for_max_mode: Option<TooltipData>,
    #[prost(bool, optional, tag = "21")]
    is_recommended_for_background_composer: Option<bool>,
    #[prost(bool, optional, tag = "22")]
    supports_plan_mode: Option<bool>,
    #[prost(string, optional, tag = "24")]
    inputbox_short_model_name: Option<String>,
    #[prost(bool, optional, tag = "25")]
    supports_sandboxing: Option<bool>,
    #[prost(bool, optional, tag = "26")]
    supports_cmd_k: Option<bool>,
    #[prost(message, repeated, tag = "29")]
    parameter_definitions: Vec<ModelParameterDefinition>,
    #[prost(message, repeated, tag = "30")]
    variants: Vec<ModelVariant>,
    #[prost(string, repeated, tag = "36")]
    legacy_slugs: Vec<String>,
    #[prost(int32, optional, tag = "38")]
    named_model_section_index: Option<i32>,
    #[prost(string, optional, tag = "41")]
    vendor_name: Option<String>,
    #[prost(message, optional, tag = "42")]
    vendor: Option<AvailableModelVendor>,
    #[prost(message, repeated, tag = "48")]
    model_picker_badges: Vec<ModelPickerBadge>,
}

#[derive(Clone, PartialEq, Message)]
struct TooltipData {
    #[prost(string, optional, tag = "7")]
    markdown_content: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterDefinition {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, optional, tag = "3")]
    markdown_tooltip: Option<String>,
    #[prost(message, optional, tag = "4")]
    parameter_type: Option<ModelParameterType>,
    #[prost(bool, optional, tag = "5")]
    is_cycleable_by_hotkey: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterType {
    #[prost(message, optional, tag = "1")]
    boolean_parameter: Option<BooleanParameter>,
    #[prost(message, optional, tag = "2")]
    enum_parameter: Option<EnumParameter>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<BooleanParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    increases_model_cost: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<EnumParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelVariant {
    #[prost(message, repeated, tag = "1")]
    parameter_values: Vec<ModelParameterValue>,
    #[prost(string, tag = "2")]
    display_name: String,
    #[prost(bool, tag = "3")]
    is_max_mode: bool,
    #[prost(bool, optional, tag = "4")]
    is_default_max_config: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    is_default_non_max_config: Option<bool>,
    #[prost(message, optional, tag = "6")]
    tooltip_data: Option<TooltipData>,
    #[prost(string, optional, tag = "8")]
    display_name_outside_picker: Option<String>,
    #[prost(string, optional, tag = "9")]
    variant_string_representation: Option<String>,
    #[prost(string, optional, tag = "11")]
    legacy_slug: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterValue {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct ModelPickerBadge {
    #[prost(string, tag = "1")]
    label: String,
    #[prost(int32, tag = "2")]
    variant: i32,
    #[prost(bool, tag = "3")]
    dismiss_on_selection: bool,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModelVendor {
    #[prost(int32, tag = "1")]
    id: i32,
    #[prost(string, tag = "2")]
    display_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct UsableModelsAddition {
    #[prost(message, repeated, tag = "1")]
    models: Vec<agent::ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelResponse {
    #[prost(string, tag = "1")]
    model: String,
    #[prost(string, tag = "2")]
    thinking_model: String,
    #[prost(bool, tag = "3")]
    max_mode: bool,
    #[prost(string, tag = "4")]
    next_default_set_date: String,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelNudgeDataResponse {
    #[prost(string, tag = "1")]
    nudge_date: String,
    #[prost(bool, tag = "2")]
    should_default_switch_on_new_chat: bool,
    #[prost(string, repeated, tag = "3")]
    models_with_no_default_switch: Vec<String>,
    #[prost(string, tag = "4")]
    conversion_model_override: String,
}

const CLI_LOCAL_MODEL_API_KEY: &str = "cursor-byok-local";

const CONTEXTS: [(&str, &str); 5] = [
    ("200k", "200K"),
    ("356k", "356K"),
    ("500k", "500K"),
    ("800k", "800K"),
    ("1m", "1M"),
];
const EFFORTS: [(&str, &str); 5] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra High"),
    ("max", "Max"),
];
const DEFAULT_CONTEXT: &str = "200k";

fn context_options(context_window_tokens: Option<u64>) -> Vec<(String, String)> {
    let mut contexts = CONTEXTS
        .into_iter()
        .map(|(value, display_name)| (value.to_owned(), display_name.to_owned()))
        .collect::<Vec<_>>();
    if let Some(tokens) = context_window_tokens {
        let value = tokens.to_string();
        let duplicate = contexts
            .iter()
            .any(|(existing, _)| parse_token_count(existing) == Some(tokens));
        if !duplicate {
            contexts.push((value, format!("{} (Custom)", format_token_count(tokens))));
        }
    }
    contexts
}

pub async fn available_models(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    tracing::info!(
        model_count = models.len(),
        plugin_model_count = plugin_models.len(),
        "appending BYOK models to Cursor AvailableModels"
    );
    let mut available_models = models.iter().map(available_model).collect::<Vec<_>>();
    available_models.extend(plugin_models.iter().map(available_plugin_model));
    let local = AvailableModelsAddition {
        model_names: models
            .iter()
            .map(|model| model.model_hash.clone())
            .chain(plugin_models.iter().map(|model| model.id.clone()))
            .collect(),
        models: available_models,
    }
    .encode_to_vec();
    merge_with_upstream_catalog(proxy, request, local).await
}

pub async fn usable_models(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    tracing::info!(
        model_count = models.len(),
        plugin_model_count = plugin_models.len(),
        "appending BYOK models to Cursor GetUsableModels"
    );
    let local = UsableModelsAddition {
        models: models
            .iter()
            .map(usable_model)
            .chain(plugin_models.iter().map(usable_plugin_model))
            .collect(),
    }
    .encode_to_vec();
    merge_with_upstream_catalog(proxy, request, local).await
}

async fn merge_with_upstream_catalog(
    proxy: CursorProxy,
    request: Request<Body>,
    local: Vec<u8>,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    let token = authorization_token_hash(&parts.headers);

    if let Some(token) = token {
        if let Some(cached) = read_catalog(&path, &token).await {
            if begin_catalog_refresh(&path, &token) {
                let proxy = proxy.clone();
                let method = parts.method.clone();
                let uri = parts.uri.clone();
                let headers = parts.headers.clone();
                let refresh_path = path.clone();
                tokio::spawn(async move {
                    refresh_catalog(proxy, refresh_path, token, method, uri, headers, body).await;
                });
            }
            let response = merge_response(cached_upstream(cached), local)?;
            startup_timing::log_metadata(
                &path,
                MetadataSource::Merged,
                started.elapsed(),
                None,
                Some(true),
                None,
            );
            return Ok(response);
        }
    }

    let upstream_started = Instant::now();
    match forward_catalog(proxy, parts.method, parts.uri, parts.headers, body).await {
        Ok(upstream) => {
            let upstream_ms = upstream_started.elapsed();
            if upstream.status.is_success() {
                if let Some(token) = token {
                    if let Err(error) = write_catalog(&path, &token, &upstream.body).await {
                        tracing::warn!(%error, "failed to persist model catalog cache");
                    }
                }
            }
            let response = merge_response(upstream, local)?;
            startup_timing::log_metadata(
                &path,
                MetadataSource::Merged,
                started.elapsed(),
                Some(upstream_ms),
                Some(false),
                None,
            );
            Ok(response)
        }
        Err(error) => {
            tracing::warn!(%error, path = %path, "Cursor model catalog upstream unavailable; using local catalog");
            startup_timing::log_metadata(
                &path,
                MetadataSource::Local,
                started.elapsed(),
                Some(upstream_started.elapsed()),
                Some(false),
                None,
            );
            Ok(local_response(local))
        }
    }
}

async fn refresh_catalog(
    proxy: CursorProxy,
    path: String,
    token: [u8; 32],
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) {
    let started = Instant::now();
    match forward_catalog(proxy, method, uri, headers, body).await {
        Ok(upstream) if upstream.status.is_success() => {
            if let Err(error) = write_catalog(&path, &token, &upstream.body).await {
                tracing::warn!(%error, "failed to persist model catalog cache");
            }
            let elapsed = started.elapsed();
            startup_timing::log_metadata(
                &path,
                MetadataSource::Upstream,
                elapsed,
                Some(elapsed),
                None,
                None,
            );
        }
        Ok(upstream) => {
            tracing::warn!(
                path,
                status = %upstream.status,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "model catalog refresh rejected"
            );
        }
        Err(error) => {
            tracing::warn!(
                path,
                %error,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "model catalog refresh failed"
            );
        }
    }
    finish_catalog_refresh(&path, &token);
}

async fn forward_catalog(
    proxy: CursorProxy,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<proxy::BufferedResponse> {
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    proxy::forward_buffered(&proxy, request).await
}

fn cached_upstream(body: Vec<u8>) -> proxy::BufferedResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    proxy::BufferedResponse {
        status: StatusCode::OK,
        headers,
        body: Bytes::from(body),
    }
}

fn begin_catalog_refresh(path: &str, token: &[u8; 32]) -> bool {
    catalog_gate()
        .inflight
        .lock()
        .insert(refresh_key(path, token))
}

fn finish_catalog_refresh(path: &str, token: &[u8; 32]) {
    catalog_gate()
        .inflight
        .lock()
        .remove(&refresh_key(path, token));
}

fn refresh_key(path: &str, token: &[u8; 32]) -> String {
    format!("{path}\0{}", hex(token))
}

async fn read_catalog(path: &str, token: &[u8; 32]) -> Option<Vec<u8>> {
    let file = catalog_file(path, token).ok()?;
    let body = tokio::fs::read(file).await.ok()?;
    (!body.is_empty()).then_some(body)
}

async fn write_catalog(path: &str, token: &[u8; 32], body: &[u8]) -> Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let file = catalog_file(path, token)?;
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
        }
    }
    let temporary = file.with_extension("tmp");
    tokio::fs::write(&temporary, body).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(temporary, file).await?;
    Ok(())
}

fn catalog_file(path: &str, token: &[u8; 32]) -> Result<PathBuf> {
    Ok(crate::config::managed_data_dir()?
        .join("cache")
        .join("model_catalog")
        .join(catalog_relative(token, path)))
}

fn catalog_relative(token: &[u8; 32], endpoint: &str) -> PathBuf {
    PathBuf::from(hex(token)).join(hex(Sha256::digest(endpoint.as_bytes()).as_slice()))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

pub async fn default_model_for_cli(
    State(registry): State<TransportRegistry>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        agent::GetDefaultModelForCliResponse {
            model: default_model_details(&models, &plugin_models),
        }
        .encode_to_vec(),
    ))
}

pub async fn default_model(State(registry): State<TransportRegistry>) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        default_model_response(&models, &plugin_models).encode_to_vec(),
    ))
}

pub async fn default_model_nudge(
    State(registry): State<TransportRegistry>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        default_model_nudge_response(&models, &plugin_models).encode_to_vec(),
    ))
}

async fn configured_plugin_models(registry: &TransportRegistry) -> Vec<PluginModelDescriptor> {
    match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    }
}

fn default_model_details(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> Option<agent::ModelDetails> {
    models
        .first()
        .map(usable_model)
        .or_else(|| plugin_models.first().map(usable_plugin_model))
}

fn default_model_id<'a>(
    models: &'a [ModelConfig],
    plugin_models: &'a [PluginModelDescriptor],
) -> &'a str {
    models
        .first()
        .map(|model| model.model_hash.as_str())
        .or_else(|| plugin_models.first().map(|model| model.id.as_str()))
        .unwrap_or_default()
}

fn default_model_response(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> DefaultModelResponse {
    let model = default_model_id(models, plugin_models).to_owned();
    DefaultModelResponse {
        thinking_model: model.clone(),
        model,
        max_mode: false,
        next_default_set_date: String::new(),
    }
}

fn default_model_nudge_response(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> DefaultModelNudgeDataResponse {
    DefaultModelNudgeDataResponse {
        nudge_date: "0".into(),
        should_default_switch_on_new_chat: false,
        models_with_no_default_switch: models
            .iter()
            .map(|model| model.model_hash.clone())
            .chain(plugin_models.iter().map(|model| model.id.clone()))
            .collect(),
        conversion_model_override: String::new(),
    }
}

fn merge_response(upstream: proxy::BufferedResponse, extra: Vec<u8>) -> Result<Response<Body>> {
    if !upstream.status.is_success() {
        tracing::warn!(status = %upstream.status, "Cursor model catalog upstream rejected request; using local catalog");
        return Ok(local_response(extra));
    }
    let (framed, payload) = unary_payload(&upstream.body)?;
    let body = if framed {
        let mut merged = BytesMut::with_capacity(5 + payload.len() + extra.len());
        merged.put_u8(0);
        merged.put_u32((payload.len() + extra.len()) as u32);
        merged.extend_from_slice(payload);
        merged.extend_from_slice(&extra);
        merged.freeze()
    } else {
        let mut merged = BytesMut::with_capacity(payload.len() + extra.len());
        merged.extend_from_slice(payload);
        merged.extend_from_slice(&extra);
        merged.freeze()
    };
    Ok(upstream.with_body(body))
}

fn local_response(body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
}

fn unary_payload(body: &Bytes) -> Result<(bool, &[u8])> {
    if body.len() < 5 {
        return Ok((false, body));
    }
    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if length != body.len() - 5 {
        return Ok((false, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot merge compressed or terminal model catalog frame: flags={flags}"
        )));
    }
    Ok((true, &body[5..]))
}

fn available_model(model: &ModelConfig) -> AvailableModel {
    let contexts = context_options(model.context_window_tokens);
    let tooltip = model_tooltip(model);
    let variants = model_variants(
        &model.model_hash,
        &model.display_name,
        &tooltip,
        &contexts,
        true,
    );
    let legacy_slugs = variants
        .iter()
        .filter_map(|variant| variant.legacy_slug.clone())
        .collect();
    AvailableModel {
        name: model.model_hash.clone(),
        default_on: true,
        supports_agent: Some(true),
        degradation_status: Some(0),
        tooltip_data: Some(tooltip.clone()),
        supports_thinking: Some(true),
        supports_images: Some(true),
        supports_max_mode: Some(true),
        client_display_name: Some(model.display_name.clone()),
        server_model_name: Some(model.model_hash.clone()),
        supports_non_max_mode: Some(true),
        tooltip_data_for_max_mode: Some(tooltip),
        is_recommended_for_background_composer: Some(false),
        supports_plan_mode: Some(true),
        inputbox_short_model_name: Some(model.display_name.clone()),
        supports_sandboxing: Some(true),
        supports_cmd_k: Some(false),
        parameter_definitions: model_parameters(&contexts, true),
        variants,
        legacy_slugs,
        named_model_section_index: Some(1),
        vendor_name: Some("cursor".into()),
        vendor: Some(AvailableModelVendor {
            id: 6,
            display_name: "Cursor".into(),
        }),
        model_picker_badges: vec![ModelPickerBadge {
            label: model
                .group_name
                .clone()
                .unwrap_or_else(|| provider_host(&model.base_url)),
            variant: 1,
            dismiss_on_selection: false,
        }],
    }
}

/// 徽章回退标签:base_url 的主机名。入库时已校验为带主机的 HTTP(S) URL,
/// 解析失败仅是理论分支,此时原样返回 base_url。
fn provider_host(base_url: &str) -> String {
    reqwest::Url::parse(base_url.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .unwrap_or_else(|| base_url.trim().into())
}

fn model_parameters(
    contexts: &[(String, String)],
    thinking: bool,
) -> Vec<ModelParameterDefinition> {
    let mut parameters = vec![ModelParameterDefinition {
        id: "context".into(),
        name: "Context".into(),
        markdown_tooltip: Some("Context size used to trigger conversation compaction.".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: None,
            enum_parameter: Some(EnumParameter {
                values: contexts
                    .iter()
                    .map(|(value, display_name)| EnumParameterValue {
                        value: value.clone(),
                        display_name: Some(display_name.clone()),
                    })
                    .collect(),
            }),
        }),
        is_cycleable_by_hotkey: Some(false),
    }];
    if thinking {
        parameters.push(ModelParameterDefinition {
            id: "reasoning".into(),
            name: "Effort".into(),
            markdown_tooltip: Some("Effort the model uses to generate its response.".into()),
            parameter_type: Some(ModelParameterType {
                boolean_parameter: None,
                enum_parameter: Some(EnumParameter {
                    values: EFFORTS
                        .into_iter()
                        .map(|(value, display_name)| EnumParameterValue {
                            value: value.into(),
                            display_name: Some(display_name.into()),
                        })
                        .collect(),
                }),
            }),
            is_cycleable_by_hotkey: Some(true),
        });
    }
    parameters.push(ModelParameterDefinition {
        id: "fast".into(),
        name: "Fast".into(),
        markdown_tooltip: Some("Significantly faster but consumes more usage".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: Some(BooleanParameter {
                values: vec![
                    BooleanParameterValue {
                        value: "false".into(),
                        display_name: None,
                        increases_model_cost: None,
                    },
                    BooleanParameterValue {
                        value: "true".into(),
                        display_name: Some("Fast".into()),
                        increases_model_cost: Some(true),
                    },
                ],
            }),
            enum_parameter: None,
        }),
        is_cycleable_by_hotkey: Some(false),
    });
    parameters
}

fn model_variants(
    name: &str,
    display_name: &str,
    tooltip: &TooltipData,
    contexts: &[(String, String)],
    thinking: bool,
) -> Vec<ModelVariant> {
    // 非思考模型没有 Effort 轴,变体网格只剩 Context × Fast。
    let efforts: &[Option<(&str, &str)>] = if thinking {
        &[
            Some(EFFORTS[0]),
            Some(EFFORTS[1]),
            Some(EFFORTS[2]),
            Some(EFFORTS[3]),
            Some(EFFORTS[4]),
        ]
    } else {
        &[None]
    };
    let mut variants = Vec::with_capacity(contexts.len() * efforts.len() * 2);
    for (context, context_name) in contexts {
        for effort in efforts {
            for fast in [false, true] {
                variants.push(model_variant(
                    name,
                    display_name,
                    tooltip,
                    context,
                    context_name,
                    *effort,
                    fast,
                ));
            }
        }
    }
    variants
}

fn model_variant(
    name: &str,
    display_name: &str,
    tooltip: &TooltipData,
    context: &str,
    context_name: &str,
    effort: Option<(&str, &str)>,
    fast: bool,
) -> ModelVariant {
    let mut suffix = Vec::with_capacity(3);
    if context != DEFAULT_CONTEXT {
        suffix.push(context_name);
    }
    if let Some((_, effort_name)) = effort {
        suffix.push(effort_name);
    }
    if fast {
        suffix.push("Fast");
    }
    let suffix = suffix.join(" ");
    let display_name = if suffix.is_empty() {
        display_name.to_owned()
    } else {
        format!(
            "{display_name} <span style=\"color: var(--cursor-text-tertiary);\">{suffix}</span>"
        )
    };
    let is_default =
        context == DEFAULT_CONTEXT && !fast && effort.is_none_or(|(effort, _)| effort == "high");
    let mut parameter_values = vec![ModelParameterValue {
        id: "context".into(),
        value: context.into(),
    }];
    if let Some((effort, _)) = effort {
        parameter_values.push(ModelParameterValue {
            id: "reasoning".into(),
            value: effort.into(),
        });
    }
    parameter_values.push(ModelParameterValue {
        id: "fast".into(),
        value: fast.to_string(),
    });
    ModelVariant {
        parameter_values,
        display_name: display_name.clone(),
        is_max_mode: false,
        is_default_max_config: is_default.then_some(true),
        is_default_non_max_config: is_default.then_some(true),
        tooltip_data: Some(tooltip.clone()),
        display_name_outside_picker: Some(display_name),
        variant_string_representation: Some(match effort {
            Some((effort, _)) => {
                format!("{name}[context={context},reasoning={effort},fast={fast}]")
            }
            None => format!("{name}[context={context},fast={fast}]"),
        }),
        legacy_slug: Some(format!(
            "{name}-{context}{}{}",
            effort
                .map(|(effort, _)| format!("-{effort}"))
                .unwrap_or_default(),
            if fast { "-fast" } else { "" }
        )),
    }
}

fn model_tooltip(model: &ModelConfig) -> TooltipData {
    TooltipData {
        markdown_content: Some(model.tooltip_data.clone()),
    }
}

fn available_plugin_model(model: &PluginModelDescriptor) -> AvailableModel {
    let tooltip = TooltipData {
        markdown_content: model.description.clone(),
    };
    // Effort 与上下文档位由宿主统一提供,与内置模型一致;插件不再声明这两项。
    let contexts = context_options(None);
    let variants = model_variants(&model.id, &model.display_name, &tooltip, &contexts, true);
    let legacy_slugs = variants
        .iter()
        .filter_map(|variant| variant.legacy_slug.clone())
        .collect();
    AvailableModel {
        name: model.id.clone(),
        default_on: true,
        supports_agent: Some(true),
        degradation_status: Some(0),
        tooltip_data: Some(tooltip.clone()),
        supports_thinking: Some(true),
        supports_images: Some(model.images),
        supports_max_mode: Some(false),
        client_display_name: Some(model.display_name.clone()),
        server_model_name: Some(model.id.clone()),
        supports_non_max_mode: Some(true),
        tooltip_data_for_max_mode: Some(tooltip.clone()),
        is_recommended_for_background_composer: Some(false),
        supports_plan_mode: Some(true),
        inputbox_short_model_name: Some(model.display_name.clone()),
        supports_sandboxing: Some(true),
        supports_cmd_k: Some(false),
        parameter_definitions: model_parameters(&contexts, true),
        variants,
        legacy_slugs,
        named_model_section_index: Some(1),
        vendor_name: Some(model.provider_type.clone()),
        vendor: Some(AvailableModelVendor {
            id: 6,
            display_name: model.provider_type.clone(),
        }),
        model_picker_badges: vec![ModelPickerBadge {
            label: model.plugin_name.clone(),
            variant: 1,
            dismiss_on_selection: false,
        }],
    }
}

fn cli_local_model_credentials() -> agent::model_details::Credentials {
    agent::model_details::Credentials::ApiKeyCredentials(agent::ApiKeyCredentials {
        api_key: CLI_LOCAL_MODEL_API_KEY.into(),
        base_url: None,
    })
}

fn usable_plugin_model(model: &PluginModelDescriptor) -> agent::ModelDetails {
    agent::ModelDetails {
        model_id: model.id.clone(),
        display_model_id: model.id.clone(),
        display_name: model.display_name.clone(),
        display_name_short: model.display_name.clone(),
        thinking_details: Some(agent::ThinkingDetails::default()),
        credentials: Some(cli_local_model_credentials()),
        ..Default::default()
    }
}

fn usable_model(model: &ModelConfig) -> agent::ModelDetails {
    agent::ModelDetails {
        model_id: model.model_hash.clone(),
        display_model_id: model.model_hash.clone(),
        display_name: model.display_name.clone(),
        display_name_short: model.display_name.clone(),
        thinking_details: Some(agent::ThinkingDetails::default()),
        credentials: Some(cli_local_model_credentials()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelType, OPENAI_CHAT_ENDPOINT};

    fn model() -> ModelConfig {
        ModelConfig {
            model_hash: "local-model-hash".into(),
            sort_order: 0,
            display_name: "Local Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://provider.example/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "provider-secret".into(),
            tooltip_data: "Local Model".into(),
            model_id: "upstream-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn cli_model_details_use_local_routing_credentials() {
        let details = usable_model(&model());
        assert_eq!(details.model_id, "local-model-hash");
        assert_eq!(details.display_name, "Local Model");
        let agent::model_details::Credentials::ApiKeyCredentials(credentials) =
            details.credentials.expect("API credentials")
        else {
            panic!("expected API key credentials");
        };
        assert_eq!(credentials.api_key, CLI_LOCAL_MODEL_API_KEY);
        assert_eq!(credentials.base_url, None);
        assert_ne!(credentials.api_key, "provider-secret");
    }

    #[test]
    fn cli_plugin_model_details_use_local_routing_credentials() {
        let details = usable_plugin_model(&PluginModelDescriptor {
            id: "plugin:test/provider/model".into(),
            plugin_id: "plugin:test".into(),
            plugin_name: "Test Plugin".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            display_name: "Plugin Model".into(),
            description: None,
            icon: String::new(),
            provider_type: "test".into(),
            max_output_tokens: None,
            images: false,
            enabled: true,
        });
        assert_eq!(details.model_id, "plugin:test/provider/model");
        let agent::model_details::Credentials::ApiKeyCredentials(credentials) =
            details.credentials.expect("API credentials")
        else {
            panic!("expected API key credentials");
        };
        assert_eq!(credentials.api_key, CLI_LOCAL_MODEL_API_KEY);
        assert_eq!(credentials.base_url, None);
    }

    #[test]
    fn cli_default_responses_use_the_local_model_hash() {
        let models = vec![model()];
        let details = default_model_details(&models, &[]).expect("default model");
        assert_eq!(details.model_id, "local-model-hash");

        let response = default_model_response(&models, &[]);
        assert_eq!(response.model, "local-model-hash");
        assert_eq!(response.thinking_model, "local-model-hash");

        let nudge = default_model_nudge_response(&models, &[]);
        assert_eq!(
            nudge.models_with_no_default_switch,
            vec!["local-model-hash"]
        );
    }

    fn auth_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn catalog_cache_files_are_scoped_by_token_and_endpoint() {
        let token_a = authorization_token_hash(&auth_headers("token-a")).unwrap();
        let token_b = authorization_token_hash(&auth_headers("token-b")).unwrap();
        let available = catalog_relative(&token_a, "/aiserver.v1.AiService/AvailableModels");
        let other_token = catalog_relative(&token_b, "/aiserver.v1.AiService/AvailableModels");
        let usable = catalog_relative(&token_a, "/agent.v1.AgentService/GetUsableModels");

        assert_ne!(available, other_token);
        assert_ne!(available, usable);
        assert!(!available.to_string_lossy().contains("token-a"));
        assert_eq!(available.components().count(), 2);
    }
}
