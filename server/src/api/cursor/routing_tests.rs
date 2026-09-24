//! Child Run routing and the actual nested protobuf body passed to the proxy.
use super::*;
use crate::cursor::compile::EffortAction;
use crate::{
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        subagent::SubagentModels,
    },
    model::ModelInvocation,
    provider::{Provider, ProviderStream},
    store::Store,
};
use prost::Message;
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct NoProvider;
impl Provider for NoProvider {
    fn stream(&self, _: ModelInvocation, _: CancellationToken) -> ProviderStream {
        panic!("routing tests must not invoke a provider")
    }
}

async fn fixture(policy: &str) -> (tempfile::TempDir, TransportRegistry) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&format!(
        "sqlite://{}",
        dir.path().join("test.db").display()
    ))
    .await
    .unwrap();
    let path = dir.path().join("subagent-models.yaml");
    tokio::fs::write(&path, policy).await.unwrap();
    let models = SubagentModels::load(path, store.clone()).await.unwrap();
    let compiler = PromptCompiler::new(
        PromptAssets::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("prompt/cursor")).unwrap(),
    );
    let registry =
        TransportRegistry::with_subagent_models(store, Arc::new(NoProvider), compiler, models);
    (dir, registry)
}

async fn admit_selected(
    registry: &TransportRegistry,
    decoded: &mut bidi::DecodedAppend,
    headers: &HeaderMap,
) -> crate::cursor::transport::AdmittedRun {
    let prepared = select_run_model(registry, decoded, headers)
        .await
        .unwrap()
        .unwrap();
    let admitted = registry
        .admit_run_model(&decoded.request_id, &prepared)
        .await
        .unwrap();
    sync_decoded_model(decoded, &admitted).unwrap();
    admitted
}

fn effort_and_fast(effort: &str, fast: bool) -> Vec<agent::requested_model::ModelParameterValue> {
    vec![
        agent::requested_model::ModelParameterValue {
            id: "effort".into(),
            value: effort.into(),
        },
        agent::requested_model::ModelParameterValue {
            id: "fast".into(),
            value: if fast { "true" } else { "false" }.into(),
        },
    ]
}

fn fast_only(fast: bool) -> Vec<agent::requested_model::ModelParameterValue> {
    vec![agent::requested_model::ModelParameterValue {
        id: "fast".into(),
        value: if fast { "true" } else { "false" }.into(),
    }]
}

fn request(id: &str, kind: Option<&str>, model: &str) -> ai::BidiAppendRequest {
    ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: id.into(),
        }),
        append_seqno: 7,
        data: hex::encode(
            agent::AgentClientMessage {
                message: Some(agent::agent_client_message::Message::RunRequest(
                    agent::AgentRunRequest {
                        requested_model: Some(agent::RequestedModel {
                            model_id: model.into(),
                            max_mode: true,
                            parameters: vec![agent::requested_model::ModelParameterValue {
                                id: "effort".into(),
                                value: "high".into(),
                            }],
                            ..Default::default()
                        }),
                        model_details: Some(agent::ModelDetails {
                            model_id: model.into(),
                            display_name: "Source display name".into(),
                            ..Default::default()
                        }),
                        subagent_type_name: kind.map(str::to_owned),
                        subagent_model_overrides: vec![agent::SubagentModelOverride {
                            subagent_type: "explore".into(),
                            selection: Some(agent::subagent_model_override::Selection::Model(
                                agent::RequestedModel {
                                    model_id: "plugin:example/provider/ui-model".into(),
                                    ..Default::default()
                                },
                            )),
                        }],
                        ..Default::default()
                    },
                )),
            }
            .encode_to_vec(),
        ),
        ..Default::default()
    }
}

struct MockUpstream {
    proxy: CursorProxy,
    requests: tokio::sync::mpsc::Receiver<(HeaderMap, Bytes)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockUpstream {
    async fn start(registry: &TransportRegistry) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = tokio::sync::mpsc::channel(8);
        let app = Router::new().route(
            "/aiserver.v1.BidiService/BidiAppend",
            post(move |request: Request<Body>| {
                let sender = sender.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = to_bytes(body, usize::MAX).await.unwrap();
                    sender.send((parts.headers, body)).await.unwrap();
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let proxy = CursorProxy::for_test_upstream(
            crate::network::NetworkClients::new(registry.store().clone()),
            format!("http://{address}"),
        );
        Self {
            proxy,
            requests,
            task,
        }
    }

    async fn append(&self, registry: &TransportRegistry, body: Bytes) -> Result<Response<Body>> {
        bidi_handler(
            State(registry.clone()),
            Extension(self.proxy.clone()),
            Request::post("/aiserver.v1.BidiService/BidiAppend")
                .header(header::CONTENT_TYPE, "application/proto")
                .header(header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn received(&mut self) -> (HeaderMap, Bytes) {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn handler_forwards_rewritten_official_body_to_real_http_upstream() {
    let (_dir, registry) =
        fixture("mapping:\n  official-A: {model: official-longer-B, effort: high}\n  official-longer-B: {model: recursive-C, effort: high}")
            .await;
    let mut upstream = MockUpstream::start(&registry).await;
    for framed in [false, true] {
        let wire = request(
            if framed { "http-framed" } else { "http-raw" },
            Some("explore"),
            "official-A",
        );
        let original: Bytes = if framed {
            connect::encode_message(&wire).unwrap()
        } else {
            wire.encode_to_vec().into()
        };
        let response = upstream.append(&registry, original.clone()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let (headers, received) = upstream.received().await;
        assert_ne!(received, original);
        assert_eq!(headers[header::CONTENT_LENGTH], received.len().to_string());
        let forwarded: ai::BidiAppendRequest = connect::decode_unary(&received).unwrap();
        assert_eq!(forwarded.request_id, wire.request_id);
        assert_eq!(forwarded.append_seqno, wire.append_seqno);
        let decoded = bidi::decode(&forwarded).unwrap();
        assert_eq!(decoded.model_id(), Some("official-longer-B"));
        let Some(agent::agent_client_message::Message::RunRequest(run)) = decoded.message.message
        else {
            panic!()
        };
        assert!(run.model_details.is_none());
        assert_eq!(
            run.requested_model.unwrap(),
            agent::RequestedModel {
                model_id: "official-longer-B".into(),
                parameters: effort_and_fast("high", false),
                ..Default::default()
            }
        );
    }
}

#[tokio::test]
async fn handler_preserves_primary_wire_including_empty_child_type_and_unknown_fields() {
    let (_dir, registry) =
        fixture("fallback: {model: plugin:example/provider/local, effort: high}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    for (id, kind) in [("primary-absent", None), ("primary-empty", Some(""))] {
        let mut body = request(id, kind, "official-A").encode_to_vec();
        body.extend_from_slice(&[0xa0, 0x06, 0x01]);
        let original: Bytes = body.into();
        assert_eq!(
            upstream
                .append(&registry, original.clone())
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
        let (headers, received) = upstream.received().await;
        assert_eq!(received, original);
        assert_eq!(headers[header::CONTENT_LENGTH], original.len().to_string());
    }
}

#[tokio::test]
async fn disabled_child_type_preserves_original_request_despite_local_yaml_target() {
    let (_dir, registry) =
        fixture("fallback: {model: plugin:example/provider/local, effort: high}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    let mut wire = request("disabled-child", Some("explore"), "official-A");
    let mut decoded = bidi::decode(&wire).unwrap();
    let Some(agent::agent_client_message::Message::RunRequest(run)) =
        decoded.message.message.as_mut()
    else {
        panic!()
    };
    run.subagent_model_overrides[0].selection =
        Some(agent::subagent_model_override::Selection::Disabled(true));
    wire.data = hex::encode(decoded.message.encode_to_vec());
    let original: Bytes = wire.encode_to_vec().into();
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_eq!(received, original);
    assert_eq!(
        registry.run_model("disabled-child").await.as_deref(),
        Some("official-A")
    );

    // Disabling one child type does not suppress policy selection for another type.
    let mut other = bidi::decode(&wire).unwrap();
    other.request_id = "enabled-child".into();
    let Some(agent::agent_client_message::Message::RunRequest(run)) =
        other.message.message.as_mut()
    else {
        panic!()
    };
    run.subagent_type_name = Some("shell".into());
    let prepared = select_run_model(&registry, &mut other, &HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(prepared.model, prepared.original);
    let admitted = registry
        .admit_run_model(&other.request_id, &prepared)
        .await
        .unwrap();
    sync_decoded_model(&mut other, &admitted).unwrap();
    assert_eq!(other.model_id(), Some("plugin:example/provider/local"));
}

#[tokio::test]
async fn official_target_rewrites_nested_hex_protobuf_and_content_length() {
    let (_dir, registry) =
        fixture("mapping:\n  source: {model: official-target, effort: high}\n  official-target: {model: wrong-recursive-target, effort: high}")
            .await;
    for framed in [false, true] {
        let wire = request(
            if framed { "framed" } else { "raw" },
            Some("explore"),
            "source",
        );
        let original: Bytes = if framed {
            connect::encode_message(&wire).unwrap()
        } else {
            wire.encode_to_vec().into()
        };
        let mut decoded = bidi::decode(&wire).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_LENGTH,
            original.len().to_string().parse().unwrap(),
        );
        let prepared = select_run_model(&registry, &mut decoded, &headers)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(prepared.model, prepared.original);
        let admitted = registry
            .admit_run_model(&decoded.request_id, &prepared)
            .await
            .unwrap();
        sync_decoded_model(&mut decoded, &admitted).unwrap();
        let changed = admitted.rewrites_wire(&prepared.original);
        assert!(changed);
        assert!(
            !resolves_as_local_model(&registry, decoded.model_id().unwrap())
                .await
                .unwrap()
        );
        let forwarded = forwarded_body(&decoded, &wire, &original, &mut headers, changed).unwrap();
        assert_ne!(forwarded, original);
        assert_eq!(headers[header::CONTENT_LENGTH], forwarded.len().to_string());
        let encoded: ai::BidiAppendRequest = connect::decode_unary(&forwarded).unwrap();
        assert_eq!(encoded.append_seqno, wire.append_seqno);
        assert_eq!(encoded.request_id, wire.request_id);
        let target = bidi::decode(&encoded).unwrap();
        assert_eq!(target.model_id(), Some("official-target"));
        let Some(agent::agent_client_message::Message::RunRequest(run)) = target.message.message
        else {
            panic!()
        };
        assert!(run.model_details.is_none());
        let model = run.requested_model.unwrap();
        assert!(!model.max_mode);
        assert_eq!(model.parameters, effort_and_fast("high", false));
        assert_eq!(run.subagent_model_overrides.len(), 1);
        assert_eq!(bidi::decode(&wire).unwrap().model_id(), Some("source"));
    }
}

#[tokio::test]
async fn primary_runs_and_unmatched_children_keep_original_bytes_and_ui_explicit_does_not_hijack() {
    for (policy, kind) in [
        ("fallback: {model: changed, effort: high}", None),
        ("{}", Some("explore")),
        (
            "mapping: {source: {model: source, effort: high}}",
            Some("explore"),
        ),
    ] {
        let (_dir, registry) = fixture(policy).await;
        let wire = request("unchanged", kind, "source");
        let mut original = wire.encode_to_vec();
        // Unknown field: a re-encode would lose it, so byte identity matters.
        original.extend_from_slice(&[0xa0, 0x06, 0x01]);
        let original: Bytes = original.into();
        let parsed = connect::decode_unary(&original).unwrap();
        let mut decoded = bidi::decode(&parsed).unwrap();
        let mut headers = HeaderMap::new();
        let prepared = select_run_model(&registry, &mut decoded, &headers)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.model, prepared.original);
        let changed = false;
        let forwarded =
            forwarded_body(&decoded, &parsed, &original, &mut headers, changed).unwrap();
        assert_eq!(forwarded, original);
        assert_eq!(forwarded.as_ptr(), original.as_ptr());
        assert!(
            !resolves_as_local_model(&registry, decoded.model_id().unwrap())
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn policy_applies_before_local_gate_in_both_directions() {
    for (source, target, local) in [
        ("official", "plugin:example/provider/local", true),
        ("plugin:example/provider/local", "official", false),
    ] {
        let (_dir, registry) =
            fixture(&format!("fallback: {{model: {target}, effort: high}}")).await;
        let mut decoded = bidi::decode(&request("child", Some("explore"), source)).unwrap();
        let admitted = admit_selected(&registry, &mut decoded, &HeaderMap::new()).await;
        assert_eq!(decoded.model_id(), Some(target));
        assert_eq!(admitted.local, local);
        assert_eq!(
            resolves_as_local_model(&registry, target).await.unwrap(),
            local
        );
    }
}

#[tokio::test]
async fn same_lifecycle_is_pinned_across_policy_reload_but_new_request_uses_new_policy() {
    let (dir, registry) = fixture("fallback: {model: original-target, effort: high}").await;
    let wire = request("existing", Some("explore"), "source");
    let mut first = bidi::decode(&wire).unwrap();
    let admitted = admit_selected(&registry, &mut first, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "original-target");
    let models = registry.subagent_models().clone();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let reload = tokio::spawn(async move { models.run_reload_loop(token).await });
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: replacement-target, effort: high}",
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.subagent_models().snapshot().version == 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut retry = bidi::decode(&wire).unwrap();
    let prepared = select_run_model(&registry, &mut retry, &HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.model, "replacement-target");
    let admitted = registry
        .admit_run_model(&retry.request_id, &prepared)
        .await
        .unwrap();
    assert_eq!(admitted.model, "original-target");
    sync_decoded_model(&mut retry, &admitted).unwrap();
    assert_eq!(retry.model_id(), Some("original-target"));
    let mut next = bidi::decode(&request("new", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut next, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "replacement-target");
    assert_eq!(next.model_id(), Some("replacement-target"));
    let crate::cursor::transport::TransportRoute::Upstream(generation) =
        registry.wait_route("existing").await
    else {
        panic!()
    };
    registry.finish_upstream("existing".into(), generation);
    tokio::time::timeout(Duration::from_secs(1), async {
        while registry.run_model("existing").await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut restarted = bidi::decode(&wire).unwrap();
    let admitted = admit_selected(&registry, &mut restarted, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "replacement-target");
    assert_eq!(restarted.model_id(), Some("replacement-target"));
    cancellation.cancel();
    reload.await.unwrap();
}

#[tokio::test]
async fn configured_hash_target_is_local_and_non_run_messages_are_not_rewritten() {
    let (dir, registry) = fixture("{}").await;
    let input = serde_json::from_value(serde_json::json!({
        "display_name": "Routing test", "type": "openai", "base_url": "https://example.com/v1",
        "api_key": "test", "tooltip_data": "Routing test", "model_id": "model"
    }))
    .unwrap();
    let local = registry.store().create_model(&input).await.unwrap();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        format!("fallback: {{model: {}, effort: high}}", local.model_hash),
    )
    .await
    .unwrap();
    let models = SubagentModels::load(
        dir.path().join("subagent-models.yaml"),
        registry.store().clone(),
    )
    .await
    .unwrap();
    let compiler = PromptCompiler::new(
        PromptAssets::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("prompt/cursor")).unwrap(),
    );
    let registry = TransportRegistry::with_subagent_models(
        registry.store().clone(),
        Arc::new(NoProvider),
        compiler,
        models,
    );
    let mut decoded = bidi::decode(&request("hash-child", Some("explore"), "official")).unwrap();
    let admitted = admit_selected(&registry, &mut decoded, &HeaderMap::new()).await;
    assert_eq!(decoded.model_id(), Some(local.model_hash.as_str()));
    assert!(admitted.local);
    assert!(
        resolves_as_local_model(&registry, decoded.model_id().unwrap())
            .await
            .unwrap()
    );
    let mut continuation = bidi::DecodedAppend {
        request_id: "hash-child".into(),
        seqno: 8,
        message: agent::AgentClientMessage::default(),
    };
    assert!(
        select_run_model(&registry, &mut continuation, &HeaderMap::new())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(continuation.message, agent::AgentClientMessage::default());

    // The retained policy still references the hash after its configuration is deleted.
    // It must fail locally, never leak the local ID to the official upstream.
    registry
        .store()
        .delete_model(&local.model_hash)
        .await
        .unwrap();
    let mut upstream = MockUpstream::start(&registry).await;
    let wire = request("deleted-hash-child", Some("explore"), "official");
    let error = upstream
        .append(&registry, wire.encode_to_vec().into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown local model hash"));
    assert!(matches!(
        upstream.requests.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn invalid_local_id_is_rejected_before_any_upstream_request() {
    let (_dir, registry) = fixture("{}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    for model in ["0123456789abcdef", "plugin:malformed"] {
        let wire = request(model, Some("explore"), model);
        let error = upstream
            .append(&registry, wire.encode_to_vec().into())
            .await
            .unwrap_err();
        assert!(matches!(error, crate::Error::Config(_)));
        assert!(matches!(
            upstream.requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn inherit_uses_parent_identity_without_applying_mapping_again() {
    let (_dir, registry) =
        fixture("mapping:\n  parent-model: {model: incorrect, effort: high}").await;
    let mut parent = bidi::decode(&request("parent", None, "parent-model")).unwrap();
    let prepared = select_run_model(&registry, &mut parent, &HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.model, prepared.original);
    assert_eq!(prepared.role, crate::cursor::transport::ModelRole::Primary);
    let admitted = registry.admit_run_model("parent", &prepared).await.unwrap();
    assert!(!admitted.local);
    assert_eq!(admitted.model, "parent-model");
    let mut child = bidi::decode(&request("child", Some("explore"), "inherit")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-parent-request-id", "parent".parse().unwrap());
    headers.insert("x-parent-agent-tool-call-id", "tool".parse().unwrap());
    let admitted = admit_selected(&registry, &mut child, &headers).await;
    assert_eq!(admitted.model, "parent-model");
    assert_eq!(child.model_id(), Some("parent-model"));
}

#[tokio::test]
async fn failed_startup_does_not_pin_so_repaired_yaml_same_id_can_retry() {
    let (dir, registry) = fixture("{}").await;
    let input = serde_json::from_value(serde_json::json!({
        "display_name": "Retry test", "type": "openai", "base_url": "https://example.com/v1",
        "api_key": "test", "tooltip_data": "Retry test", "model_id": "model"
    }))
    .unwrap();
    let local = registry.store().create_model(&input).await.unwrap();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        format!("fallback: {{model: {}, effort: high}}", local.model_hash),
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();
    registry
        .store()
        .delete_model(&local.model_hash)
        .await
        .unwrap();

    let mut upstream = MockUpstream::start(&registry).await;
    let wire = request("retry-child", Some("explore"), "official");
    let error = upstream
        .append(&registry, wire.encode_to_vec().into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown local model hash"));
    assert!(registry.run_model("retry-child").await.is_none());
    assert!(matches!(
        upstream.requests.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: official-repaired, effort: high}",
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();
    assert_eq!(
        upstream
            .append(&registry, wire.encode_to_vec().into())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    let forwarded: ai::BidiAppendRequest = connect::decode_unary(&received).unwrap();
    assert_eq!(
        bidi::decode(&forwarded).unwrap().model_id(),
        Some("official-repaired")
    );
    assert_eq!(
        registry.run_model("retry-child").await.as_deref(),
        Some("official-repaired")
    );
}

#[tokio::test]
async fn replacing_closing_lifecycle_uses_updated_policy_and_survives_stale_cleanup() {
    let (dir, registry) =
        fixture("fallback: {model: plugin:example/provider/first, effort: high}").await;
    let mut first = bidi::decode(&request("child", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut first, &HeaderMap::new()).await;
    assert!(admitted.local);
    let closing = admitted.handle.unwrap();
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("plugin:example/provider/first")
    );
    closing.begin_close();
    assert!(registry.active_child_model("child").await.is_none());

    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: plugin:example/provider/second, effort: high}",
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();

    let mut next = bidi::decode(&request("child", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut next, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "plugin:example/provider/second");
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("plugin:example/provider/second")
    );

    closing.close_transport();
    tokio::time::timeout(Duration::from_secs(1), closing.wait_transport_closed())
        .await
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("plugin:example/provider/second")
    );
    assert_eq!(
        registry.active_child_model("child").await.as_deref(),
        Some("plugin:example/provider/second")
    );
}

#[tokio::test]
async fn primary_model_updates_on_same_transport_so_child_inherit_sees_latest() {
    let (_dir, registry) =
        fixture("mapping:\n  model-a: {model: wrong-a, effort: high}\n  model-b: {model: wrong-b, effort: high}")
            .await;
    let mut first = bidi::decode(&request("parent", None, "model-a")).unwrap();
    let admitted = admit_selected(&registry, &mut first, &HeaderMap::new()).await;
    assert!(!admitted.local);
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-a")
    );

    let mut second = bidi::decode(&request("parent", None, "model-b")).unwrap();
    let admitted = admit_selected(&registry, &mut second, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "model-b");
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-b")
    );

    let mut child = bidi::decode(&request("child", Some("explore"), "inherit")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-parent-request-id", "parent".parse().unwrap());
    headers.insert("x-parent-agent-tool-call-id", "tool".parse().unwrap());
    let admitted = admit_selected(&registry, &mut child, &headers).await;
    assert_eq!(admitted.model, "model-b");
    assert_eq!(child.model_id(), Some("model-b"));
}

#[tokio::test]
async fn concurrent_handler_admissions_forward_the_same_owned_model() {
    let (_dir, registry) =
        fixture("mapping:\n  source-a: {model: owned-shared, effort: high}\n  source-b: {model: other-target, effort: high}")
            .await;
    let mut upstream = MockUpstream::start(&registry).await;
    let left = request("race-child", Some("explore"), "source-a").encode_to_vec();
    let right = request("race-child", Some("explore"), "source-b").encode_to_vec();
    let left_registry = registry.clone();
    let right_registry = registry.clone();
    let (left_result, right_result) = tokio::join!(
        upstream.append(&left_registry, left.into()),
        upstream.append(&right_registry, right.into()),
    );
    assert_eq!(left_result.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(right_result.unwrap().status(), StatusCode::NO_CONTENT);
    let owned = registry.run_model("race-child").await.unwrap();
    assert!(owned == "owned-shared" || owned == "other-target");
    let (_, first) = upstream.received().await;
    let (_, second) = upstream.received().await;
    let first_model = bidi::decode(&connect::decode_unary(&first).unwrap())
        .unwrap()
        .model_id()
        .unwrap()
        .to_owned();
    let second_model = bidi::decode(&connect::decode_unary(&second).unwrap())
        .unwrap()
        .model_id()
        .unwrap()
        .to_owned();
    assert_eq!(first_model, owned);
    assert_eq!(second_model, owned);
}

fn run_requested_model(decoded: &bidi::DecodedAppend) -> agent::RequestedModel {
    let Some(agent::agent_client_message::Message::RunRequest(run)) =
        decoded.message.message.clone()
    else {
        panic!("expected RunRequest")
    };
    run.requested_model.expect("requested model")
}

fn request_with_params(
    id: &str,
    kind: Option<&str>,
    model: &str,
    parameters: Vec<agent::requested_model::ModelParameterValue>,
) -> ai::BidiAppendRequest {
    let mut wire = request(id, kind, model);
    let mut decoded = bidi::decode(&wire).unwrap();
    let Some(agent::agent_client_message::Message::RunRequest(run)) =
        decoded.message.message.as_mut()
    else {
        panic!()
    };
    run.requested_model.as_mut().unwrap().parameters = parameters;
    wire.data = hex::encode(decoded.message.encode_to_vec());
    wire
}

#[tokio::test]
async fn official_a_to_b_with_high_effort_is_forwarded_to_upstream() {
    let (_dir, registry) =
        fixture("mapping: {official-A: {model: official-B, effort: high}}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    let original: Bytes = request("a-to-b-high", Some("explore"), "official-A")
        .encode_to_vec()
        .into();
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_ne!(received, original);
    let forwarded = bidi::decode(&connect::decode_unary(&received).unwrap()).unwrap();
    assert_eq!(forwarded.model_id(), Some("official-B"));
    let model = run_requested_model(&forwarded);
    assert_eq!(model.parameters, effort_and_fast("high", false));
    assert!(!model.max_mode);
}

#[tokio::test]
async fn official_same_model_effort_only_rewrites_forwarded_payload() {
    let (_dir, registry) =
        fixture("mapping: {official-A: {model: official-A, effort: high}}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    let wire = request_with_params(
        "a-to-a-high",
        Some("explore"),
        "official-A",
        vec![
            agent::requested_model::ModelParameterValue {
                id: "reasoning".into(),
                value: "medium".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "fast".into(),
                value: "true".into(),
            },
        ],
    );
    let original: Bytes = wire.encode_to_vec().into();
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_ne!(received, original);
    let forwarded = bidi::decode(&connect::decode_unary(&received).unwrap()).unwrap();
    assert_eq!(forwarded.model_id(), Some("official-A"));
    let model = run_requested_model(&forwarded);
    assert_eq!(
        model
            .parameters
            .iter()
            .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
            .collect::<Vec<_>>(),
        vec![("effort", "high"), ("fast", "false")]
    );
}

#[tokio::test]
async fn official_same_model_fast_only_rewrites_forwarded_payload() {
    let (_dir, registry) =
        fixture("mapping: {official-A: {model: official-A, effort: high, fast: true}}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    let wire = request_with_params(
        "a-to-a-fast",
        Some("explore"),
        "official-A",
        vec![
            agent::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: "low".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "thinking".into(),
                value: "true".into(),
            },
        ],
    );
    let original: Bytes = wire.encode_to_vec().into();
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_ne!(received, original);
    let model =
        run_requested_model(&bidi::decode(&connect::decode_unary(&received).unwrap()).unwrap());
    assert_eq!(
        model
            .parameters
            .iter()
            .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
            .collect::<Vec<_>>(),
        vec![("thinking", "true"), ("effort", "high"), ("fast", "true")]
    );
}

#[tokio::test]
async fn hash_and_plugin_targets_carry_final_effort_over_local_defaults() {
    let (dir, registry) = fixture("{}").await;
    let input = serde_json::from_value(serde_json::json!({
        "display_name": "Effort default model", "type": "openai",
        "base_url": "https://example.com/v1", "api_key": "test",
        "tooltip_data": "Effort default model", "model_id": "model",
        "reasoning_effort": "medium"
    }))
    .unwrap();
    let local = registry.store().create_model(&input).await.unwrap();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        format!(
            "types:\n  explore: {{model: {}, effort: high}}\n  shell: {{model: plugin:example/provider/plugin-model, effort: high}}",
            local.model_hash
        ),
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();

    let mut hash_child =
        bidi::decode(&request("hash-effort", Some("explore"), "official")).unwrap();
    let admitted = admit_selected(&registry, &mut hash_child, &HeaderMap::new()).await;
    assert!(admitted.local);
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));
    let requested = run_requested_model(&hash_child);
    assert_eq!(requested.parameters, effort_and_fast("high", false));
    let mut spec = crate::model::ModelSpec {
        model_id: local.model_hash.clone(),
        display_name: None,
        reasoning: crate::model::ReasoningSpec {
            enabled: true,
            effort: Some("high".into()),
        },
        latency: crate::model::ModelLatency::Standard,
        max_output_tokens: None,
        context_window_tokens: None,
        supports_image_generation: false,
        extra_params: serde_json::json!({}),
    };
    local.configure(&mut spec);
    assert_eq!(spec.reasoning.effort.as_deref(), Some("high"));

    let mut plugin_child =
        bidi::decode(&request("plugin-effort", Some("shell"), "official")).unwrap();
    let admitted = admit_selected(&registry, &mut plugin_child, &HeaderMap::new()).await;
    assert!(admitted.local);
    assert_eq!(admitted.model, "plugin:example/provider/plugin-model");
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));
    assert_eq!(
        run_requested_model(&plugin_child).parameters,
        effort_and_fast("high", false)
    );
}

#[tokio::test]
async fn composer_target_clears_effort_and_unmatched_passthrough_keeps_bytes() {
    let (_dir, registry) = fixture(
        "mapping:\n  composer-source: {model: composer-2.5}\n  source: {model: official-B, effort: high}",
    )
    .await;

    let mut composer = bidi::decode(&request_with_params(
        "composer-clear",
        Some("explore"),
        "composer-source",
        vec![
            agent::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: "high".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "reasoning".into(),
                value: "medium".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "fast".into(),
                value: "true".into(),
            },
        ],
    ))
    .unwrap();
    let admitted = admit_selected(&registry, &mut composer, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "composer-2.5");
    assert_eq!(admitted.effort, EffortAction::Clear);
    assert_eq!(admitted.fast, Some(false));
    assert!(admitted.rewrites_wire("composer-source"));
    let model = run_requested_model(&composer);
    assert!(!model.max_mode);
    assert_eq!(model.parameters, fast_only(false));

    let (_dir, registry) = fixture("{}").await;
    let wire = request("unmatched-bytes", Some("explore"), "source");
    let original: Bytes = wire.encode_to_vec().into();
    let mut upstream = MockUpstream::start(&registry).await;
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_eq!(received, original);

    let mut changed = bidi::decode(&request("changed-model", Some("explore"), "source")).unwrap();
    let (_dir, registry) = fixture("mapping: {source: {model: official-B, effort: high}}").await;
    let admitted = admit_selected(&registry, &mut changed, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "official-B");
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));
    let model = run_requested_model(&changed);
    assert!(!model.max_mode);
    assert_eq!(model.parameters, effort_and_fast("high", false));
}

#[tokio::test]
async fn local_byok_and_composer_honor_explicit_fast_true() {
    let (dir, registry) = fixture("{}").await;
    let input = serde_json::from_value(serde_json::json!({
        "display_name": "Fast latency model", "type": "openai",
        "base_url": "https://example.com/v1", "api_key": "test",
        "tooltip_data": "Fast latency model", "model_id": "model"
    }))
    .unwrap();
    let local = registry.store().create_model(&input).await.unwrap();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        format!(
            "types:\n  explore: {{model: {}, effort: high, fast: true}}\n  shell: {{model: composer-2.5, fast: true}}",
            local.model_hash
        ),
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();

    let mut hash_child = bidi::decode(&request_with_params(
        "hash-fast",
        Some("explore"),
        "official",
        vec![agent::requested_model::ModelParameterValue {
            id: "fast".into(),
            value: "false".into(),
        }],
    ))
    .unwrap();
    let admitted = admit_selected(&registry, &mut hash_child, &HeaderMap::new()).await;
    assert!(admitted.local);
    assert_eq!(admitted.fast, Some(true));
    assert_eq!(
        run_requested_model(&hash_child).parameters,
        effort_and_fast("high", true)
    );

    let mut composer = bidi::decode(&request_with_params(
        "composer-fast",
        Some("shell"),
        "composer-source",
        vec![agent::requested_model::ModelParameterValue {
            id: "fast".into(),
            value: "false".into(),
        }],
    ))
    .unwrap();
    let admitted = admit_selected(&registry, &mut composer, &HeaderMap::new()).await;
    assert!(!admitted.local);
    assert_eq!(admitted.model, "composer-2.5");
    assert_eq!(admitted.effort, EffortAction::Clear);
    assert_eq!(admitted.fast, Some(true));
    assert_eq!(run_requested_model(&composer).parameters, fast_only(true));
}

#[tokio::test]
async fn unmatched_child_keeps_request_fast_bytes_unchanged() {
    let (_dir, registry) = fixture("{}").await;
    let wire = request_with_params(
        "unmatched-fast",
        Some("explore"),
        "source",
        vec![
            agent::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: "medium".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "fast".into(),
                value: "true".into(),
            },
        ],
    );
    let original: Bytes = wire.encode_to_vec().into();
    let mut upstream = MockUpstream::start(&registry).await;
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_eq!(received, original);
    let admitted = admit_selected(
        &registry,
        &mut bidi::decode(&wire).unwrap(),
        &HeaderMap::new(),
    )
    .await;
    assert_eq!(admitted.effort, EffortAction::Unchanged);
    assert_eq!(admitted.fast, None);
    assert!(!admitted.rewrites_wire("source"));
}
#[tokio::test]
async fn conflict_aliases_are_cleared_for_explicit_effort() {
    let (_dir, registry) =
        fixture("mapping: {official-A: {model: official-A, effort: high}}").await;
    let mut decoded = bidi::decode(&request_with_params(
        "alias-conflict",
        Some("explore"),
        "official-A",
        vec![
            agent::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: "low".into(),
            },
            agent::requested_model::ModelParameterValue {
                id: "reasoning".into(),
                value: "medium".into(),
            },
        ],
    ))
    .unwrap();
    let admitted = admit_selected(&registry, &mut decoded, &HeaderMap::new()).await;
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));
    assert_eq!(
        run_requested_model(&decoded)
            .parameters
            .iter()
            .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
            .collect::<Vec<_>>(),
        vec![("effort", "high"), ("fast", "false")]
    );
}

#[tokio::test]
async fn primary_and_disabled_remain_unaffected_by_effort_policy() {
    let (_dir, registry) =
        fixture("fallback: {model: plugin:example/provider/local, effort: high}").await;
    let mut upstream = MockUpstream::start(&registry).await;

    let primary: Bytes = request("primary-effort", None, "official-A")
        .encode_to_vec()
        .into();
    assert_eq!(
        upstream
            .append(&registry, primary.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_eq!(received, primary);

    let mut wire = request("disabled-effort", Some("explore"), "official-A");
    let mut decoded = bidi::decode(&wire).unwrap();
    let Some(agent::agent_client_message::Message::RunRequest(run)) =
        decoded.message.message.as_mut()
    else {
        panic!()
    };
    run.subagent_model_overrides[0].selection =
        Some(agent::subagent_model_override::Selection::Disabled(true));
    wire.data = hex::encode(decoded.message.encode_to_vec());
    let original: Bytes = wire.encode_to_vec().into();
    assert_eq!(
        upstream
            .append(&registry, original.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let (_, received) = upstream.received().await;
    assert_eq!(received, original);
}

#[tokio::test]
async fn effort_only_hot_reload_keeps_existing_child_and_applies_to_new_child() {
    let (dir, registry) = fixture("fallback: {model: official-target, effort: high}").await;
    let mut existing =
        bidi::decode(&request("existing-effort", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut existing, &HeaderMap::new()).await;
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));

    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: official-target, effort: low, fast: true}",
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();

    let mut retry = bidi::decode(&request("existing-effort", Some("explore"), "source")).unwrap();
    let prepared = select_run_model(&registry, &mut retry, &HeaderMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.effort, EffortAction::Set("low".into()));
    assert_eq!(prepared.fast, Some(true));
    let admitted = registry
        .admit_run_model(&retry.request_id, &prepared)
        .await
        .unwrap();
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(false));
    sync_decoded_model(&mut retry, &admitted).unwrap();
    assert_eq!(
        run_requested_model(&retry).parameters,
        effort_and_fast("high", false)
    );

    let mut next = bidi::decode(&request("new-effort", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut next, &HeaderMap::new()).await;
    assert_eq!(admitted.effort, EffortAction::Set("low".into()));
    assert_eq!(admitted.fast, Some(true));
    assert_eq!(
        run_requested_model(&next).parameters,
        effort_and_fast("low", true)
    );
}

#[tokio::test]
async fn concurrent_effort_admission_and_closing_lifecycle_pin_effort() {
    let (_dir, registry) = fixture(
        "mapping:\n  source-a: {model: shared, effort: high, fast: true}\n  source-b: {model: other, effort: low, fast: false}",
    )
    .await;
    let mut upstream = MockUpstream::start(&registry).await;
    let left = request("race-effort", Some("explore"), "source-a").encode_to_vec();
    let right = request("race-effort", Some("explore"), "source-b").encode_to_vec();
    let (left_result, right_result) = tokio::join!(
        upstream.append(&registry, left.into()),
        upstream.append(&registry, right.into()),
    );
    assert_eq!(left_result.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(right_result.unwrap().status(), StatusCode::NO_CONTENT);
    let (_, first) = upstream.received().await;
    let (_, second) = upstream.received().await;
    let first_params =
        run_requested_model(&bidi::decode(&connect::decode_unary(&first).unwrap()).unwrap())
            .parameters
            .into_iter()
            .map(|parameter| (parameter.id, parameter.value))
            .collect::<Vec<_>>();
    let second_params =
        run_requested_model(&bidi::decode(&connect::decode_unary(&second).unwrap()).unwrap())
            .parameters
            .into_iter()
            .map(|parameter| (parameter.id, parameter.value))
            .collect::<Vec<_>>();
    assert_eq!(first_params, second_params);
    assert!(
        first_params
            == vec![
                ("effort".into(), "high".into()),
                ("fast".into(), "true".into())
            ]
            || first_params
                == vec![
                    ("effort".into(), "low".into()),
                    ("fast".into(), "false".into())
                ]
    );

    let (dir, registry) =
        fixture("fallback: {model: plugin:example/provider/first, effort: high, fast: true}").await;
    let mut first = bidi::decode(&request("close-effort", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut first, &HeaderMap::new()).await;
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
    assert_eq!(admitted.fast, Some(true));
    let closing = admitted.handle.unwrap();
    closing.begin_close();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: plugin:example/provider/second, effort: low, fast: false}",
    )
    .await
    .unwrap();
    registry.subagent_models().reload().await.unwrap();
    let mut next = bidi::decode(&request("close-effort", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut next, &HeaderMap::new()).await;
    assert_eq!(admitted.model, "plugin:example/provider/second");
    assert_eq!(admitted.effort, EffortAction::Set("low".into()));
    assert_eq!(admitted.fast, Some(false));
    closing.close_transport();
    tokio::time::timeout(Duration::from_secs(1), closing.wait_transport_closed())
        .await
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        registry.run_model("close-effort").await.as_deref(),
        Some("plugin:example/provider/second")
    );
}

#[tokio::test]
async fn invalid_effort_config_is_retained_and_does_not_publish() {
    let (dir, registry) = fixture("fallback: {model: official-target, effort: high}").await;
    let before = registry.subagent_models().snapshot();
    tokio::fs::write(
        dir.path().join("subagent-models.yaml"),
        "fallback: {model: official-target, effort: none}",
    )
    .await
    .unwrap();
    assert!(registry.subagent_models().reload().await.is_err());
    assert!(std::sync::Arc::ptr_eq(
        &before,
        &registry.subagent_models().snapshot()
    ));
    let mut child = bidi::decode(&request("retain-effort", Some("explore"), "source")).unwrap();
    let admitted = admit_selected(&registry, &mut child, &HeaderMap::new()).await;
    assert_eq!(admitted.effort, EffortAction::Set("high".into()));
}

#[tokio::test]
async fn routing_logs_distinguish_reloaded_candidate_from_pinned_selection() {
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (dir, registry) = fixture("fallback: {model: official-target, effort: high}").await;
    let mut upstream = MockUpstream::start(&registry).await;
    let output = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = LogWriter(output.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();

    async {
        let wire = request("logged-child", Some("explore"), "source");
        upstream
            .append(&registry, wire.encode_to_vec().into())
            .await
            .unwrap();
        upstream.received().await;
        output.lock().unwrap().clear();

        tokio::fs::write(
            dir.path().join("subagent-models.yaml"),
            "fallback: {model: official-new, effort: low, fast: true}",
        )
        .await
        .unwrap();
        registry.subagent_models().reload().await.unwrap();
        upstream
            .append(&registry, wire.encode_to_vec().into())
            .await
            .unwrap();
        let (_, received) = upstream.received().await;
        let decoded = bidi::decode(&connect::decode_unary(&received).unwrap()).unwrap();
        assert_eq!(decoded.model_id(), Some("official-target"));
        assert_eq!(
            run_requested_model(&decoded)
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("effort", "high"), ("fast", "false")]
        );
    }
    .with_subscriber(subscriber)
    .await;

    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    let candidate = logs
        .lines()
        .find(|line| line.contains("resolved child Run model policy candidate"))
        .unwrap();
    assert!(
        candidate.contains("candidate_model=\"official-new\""),
        "{candidate}"
    );
    assert!(!candidate.contains("selected_effort"), "{candidate}");
    let selected = logs
        .lines()
        .find(|line| line.contains("admitted Cursor Run model selection"))
        .unwrap();
    for field in [
        "candidate_model=\"official-new\"",
        "candidate_effort_action=Set(\"low\")",
        "candidate_fast=Some(true)",
        "selected_model=\"official-target\"",
        "selected_effort_action=Set(\"high\")",
        "selected_fast=Some(false)",
        "selection_differs_from_candidate=true",
    ] {
        assert!(selected.contains(field), "missing {field}: {selected}");
    }
}
