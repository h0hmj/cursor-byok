use super::*;
use crate::cursor::compile::EffortAction;
use crate::{cursor::prompting::PromptAssets, model::ModelInvocation, provider::ProviderStream};
use std::sync::Arc as StdArc;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

struct NoProvider;
impl Provider for NoProvider {
    fn stream(&self, _: ModelInvocation, _: CancellationToken) -> ProviderStream {
        panic!("model selection must not invoke a provider")
    }
}

async fn fixture() -> (tempfile::TempDir, TransportRegistry) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&format!(
        "sqlite://{}",
        dir.path().join("test.db").display()
    ))
    .await
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(NoProvider),
        PromptCompiler::new(PromptAssets::embedded().unwrap()),
    );
    (dir, registry)
}

fn child_binding(model: &str, effort: EffortAction, fast: Option<bool>) -> RunModelBinding {
    RunModelBinding {
        model: model.into(),
        effort,
        fast,
        role: ModelRole::Child,
    }
}

#[tokio::test]
async fn concurrent_selections_share_one_model_until_lifecycle_finishes() {
    let (_dir, registry) = fixture().await;
    let (first, second) = tokio::join!(
        registry.mark_upstream(
            "child",
            "model-a",
            EffortAction::Set("high".into()),
            Some(true),
            ModelRole::Child
        ),
        registry.mark_upstream(
            "child",
            "model-b",
            EffortAction::Set("low".into()),
            Some(false),
            ModelRole::Child
        ),
    );
    assert_eq!(first, second);
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some(first.model.as_str())
    );
    assert_eq!(
        registry.active_child_model("child").await.as_deref(),
        Some(first.model.as_str())
    );
    let TransportRoute::Upstream(generation) = registry.wait_route("child").await else {
        panic!("expected upstream route")
    };
    registry
        .finish_upstream("child".into(), generation)
        .await
        .unwrap();
    assert!(registry.run_model("child").await.is_none());
    let owned = registry
        .mark_upstream(
            "child",
            "model-c",
            EffortAction::Set("medium".into()),
            Some(false),
            ModelRole::Child,
        )
        .await;
    assert_eq!(owned.model, "model-c");
    assert_eq!(owned.effort, EffortAction::Set("medium".into()));
    assert_eq!(owned.fast, Some(false));
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("model-c")
    );
    registry.shutdown().await;
    assert!(registry.run_model("child").await.is_none());
}

#[tokio::test]
async fn stale_upstream_completion_keeps_newer_generation_selection() {
    let (_dir, registry) = fixture().await;
    registry
        .mark_upstream(
            "child",
            "selected",
            EffortAction::Set("high".into()),
            Some(true),
            ModelRole::Child,
        )
        .await;
    registry
        .mark_upstream(
            "child",
            "ignored-retry",
            EffortAction::Set("low".into()),
            Some(false),
            ModelRole::Child,
        )
        .await;
    registry.finish_upstream("child".into(), 1).await.unwrap();
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("selected")
    );
    assert_eq!(
        registry.wait_route("child").await,
        TransportRoute::Upstream(2)
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn replacing_closing_local_lifecycle_takes_new_child_selection() {
    let (_dir, registry) = fixture().await;
    let (first, owned_a) = registry
        .get_or_create_for_append(
            "child",
            true,
            Some(child_binding(
                "model-a",
                EffortAction::Set("high".into()),
                Some(true),
            )),
        )
        .await
        .unwrap();
    assert_eq!(
        owned_a.as_ref().map(|value| value.model.as_str()),
        Some("model-a")
    );
    assert_eq!(
        owned_a.as_ref().map(|value| value.effort.clone()),
        Some(EffortAction::Set("high".into()))
    );
    assert_eq!(owned_a.as_ref().and_then(|value| value.fast), Some(true));
    first.begin_close();
    let (second, owned_b) = registry
        .get_or_create_for_append(
            "child",
            true,
            Some(child_binding(
                "model-b",
                EffortAction::Set("low".into()),
                Some(false),
            )),
        )
        .await
        .unwrap();
    assert_eq!(second.request_id(), "child");
    assert_eq!(
        owned_b.as_ref().map(|value| value.model.as_str()),
        Some("model-b")
    );
    assert_eq!(
        owned_b.as_ref().map(|value| value.effort.clone()),
        Some(EffortAction::Set("low".into()))
    );
    assert_eq!(owned_b.as_ref().and_then(|value| value.fast), Some(false));
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("model-b")
    );
    first.close_transport();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        first.wait_transport_closed(),
    )
    .await
    .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some("model-b")
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn primary_model_updates_on_same_local_lifecycle() {
    let (_dir, registry) = fixture().await;
    let (handle, _) = registry
        .get_or_create_for_append("parent", true, None)
        .await
        .unwrap();
    let _admission = handle.admit().unwrap();
    registry
        .commit_primary_model("parent", "model-a")
        .await
        .unwrap();
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-a")
    );
    registry
        .commit_primary_model("parent", "model-b")
        .await
        .unwrap();
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-b")
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn concurrent_admit_run_model_keeps_decoded_route_and_owned_model_aligned() {
    let (_dir, registry) = fixture().await;
    let barrier = StdArc::new(Barrier::new(2));
    let prepare = |model: &str, effort: &str, fast: bool| PreparedRunModel {
        original: "source".into(),
        model: model.into(),
        effort: EffortAction::Set(effort.into()),
        fast: Some(fast),
        role: ModelRole::Child,
    };
    let left_barrier = barrier.clone();
    let left_registry = registry.clone();
    let right_barrier = barrier.clone();
    let right_registry = registry.clone();
    let (left, right) = tokio::join!(
        async move {
            left_barrier.wait().await;
            left_registry
                .admit_run_model("child", &prepare("model-a", "high", true))
                .await
                .unwrap()
        },
        async move {
            right_barrier.wait().await;
            right_registry
                .admit_run_model("child", &prepare("model-b", "low", false))
                .await
                .unwrap()
        },
    );
    assert_eq!(left.model, right.model);
    assert_eq!(left.effort, right.effort);
    assert_eq!(left.fast, right.fast);
    assert_eq!(left.local, right.local);
    assert!(!left.local);
    assert_eq!(
        registry.run_model("child").await.as_deref(),
        Some(left.model.as_str())
    );
    assert_eq!(
        registry.active_child_model("child").await.as_deref(),
        Some(left.model.as_str())
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn closing_between_prepare_and_admit_uses_candidate_not_stale_pin() {
    let (_dir, registry) = fixture().await;
    let prepared_a = PreparedRunModel {
        original: "source".into(),
        model: "plugin:example/provider/a".into(),
        effort: EffortAction::Set("high".into()),
        fast: Some(true),
        role: ModelRole::Child,
    };
    let first = registry
        .admit_run_model("local-child", &prepared_a)
        .await
        .unwrap();
    assert!(first.local);
    assert_eq!(first.model, "plugin:example/provider/a");
    assert_eq!(first.effort, EffortAction::Set("high".into()));
    assert_eq!(first.fast, Some(true));
    let closing = first.handle.unwrap();
    closing.begin_close();
    assert!(registry.active_child_model("local-child").await.is_none());

    let prepared_b = PreparedRunModel {
        original: "source".into(),
        model: "plugin:example/provider/b".into(),
        effort: EffortAction::Set("low".into()),
        fast: Some(false),
        role: ModelRole::Child,
    };
    let second = registry
        .admit_run_model("local-child", &prepared_b)
        .await
        .unwrap();
    assert_eq!(second.model, "plugin:example/provider/b");
    assert_eq!(second.effort, EffortAction::Set("low".into()));
    assert_eq!(second.fast, Some(false));
    assert_eq!(
        registry.run_model("local-child").await.as_deref(),
        Some("plugin:example/provider/b")
    );
    closing.close_transport();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        closing.wait_transport_closed(),
    )
    .await
    .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        registry.run_model("local-child").await.as_deref(),
        Some("plugin:example/provider/b")
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn rejected_transport_admit_does_not_commit_primary_identity() {
    let (_dir, registry) = fixture().await;
    let (handle, _) = registry
        .get_or_create_for_append("parent", true, None)
        .await
        .unwrap();
    let _admission = handle.admit().unwrap();
    registry
        .commit_primary_model("parent", "model-a")
        .await
        .unwrap();
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-a")
    );

    handle.begin_close();
    assert!(handle.admit().is_err());
    assert!(registry
        .commit_primary_model("parent", "model-b")
        .await
        .is_err());
    assert_eq!(
        registry.run_model("parent").await.as_deref(),
        Some("model-a")
    );
    registry.shutdown().await;
}
