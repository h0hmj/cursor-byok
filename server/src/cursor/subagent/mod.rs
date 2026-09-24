//! Loads subagent model policy and atomically publishes validated snapshots.
mod policy;

pub use policy::{
    is_composer_model, ModelTarget, Policy, PolicySnapshot, Resolution, ResolutionReason,
};

use std::{io::ErrorKind, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{store::Store, Result};

const RELOAD_INTERVAL: Duration = Duration::from_millis(500);

/// Clones share one immutable current snapshot. Retained snapshots never change.
#[derive(Clone)]
pub struct SubagentModels {
    path: Option<PathBuf>,
    store: Store,
    current: Arc<RwLock<Arc<PolicySnapshot>>>,
}

impl SubagentModels {
    /// An explicit in-memory empty policy for runtimes without a policy file.
    pub fn empty(store: Store) -> Self {
        Self {
            path: None,
            store,
            current: Arc::new(RwLock::new(Arc::new(PolicySnapshot {
                version: 1,
                policy: Policy::default(),
            }))),
        }
    }

    /// Missing at startup means an empty policy; malformed startup configuration fails.
    pub async fn load(path: impl Into<PathBuf>, store: Store) -> Result<Self> {
        let path = path.into();
        let policy = match tokio::fs::read(&path).await {
            Ok(bytes) => Policy::parse(&bytes, &store)
                .await
                .map_err(|error| crate::Error::Config(format!("{}: {error}", path.display())))?,
            Err(error) if error.kind() == ErrorKind::NotFound => Policy::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path: Some(path),
            store,
            current: Arc::new(RwLock::new(Arc::new(PolicySnapshot { version: 1, policy }))),
        })
    }

    pub fn snapshot(&self) -> Arc<PolicySnapshot> {
        self.current.read().clone()
    }

    /// The application owns spawning, cancellation, and joining this loop.
    /// Read bytes rather than relying on modification times, so atomic replacement
    /// and same-size edits are both detected. Invalid/deleted files retain the policy.
    pub async fn run_reload_loop(&self, cancellation: CancellationToken) {
        let Some(path) = &self.path else {
            return;
        };
        let mut interval = tokio::time::interval(RELOAD_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_error = None;
        let mut observed = None;
        let mut attempted = None;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {}
            }
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                result = async {
                    let bytes = tokio::fs::read(path).await?;
                    if observed.as_ref() != Some(&bytes) {
                        observed = Some(bytes);
                        return Ok(());
                    }
                    if attempted.as_ref() == Some(&bytes) {
                        return Ok(());
                    }
                    attempted = Some(bytes.clone());
                    self.publish(&bytes).await
                } => result,
            };
            match result {
                Ok(()) => last_error = None,
                Err(error) => {
                    let error = error.to_string();
                    if last_error.as_ref() != Some(&error) {
                        tracing::warn!(path = %path.display(), %error, "keeping previous subagent model policy");
                    }
                    last_error = Some(error);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn reload(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = tokio::fs::read(path).await?;
        self.publish(&bytes).await
    }

    async fn publish(&self, bytes: &[u8]) -> Result<()> {
        let policy = Policy::parse(bytes, &self.store).await?;
        let mut current = self.current.write();
        if current.policy != policy {
            *current = Arc::new(PolicySnapshot {
                version: current.version + 1,
                policy,
            });
            tracing::info!(
                path = ?self.path,
                policy_version = current.version,
                "published subagent model policy"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, PathBuf, Store) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagent-models.yaml");
        let store = Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("test.db").display()
        ))
        .await
        .unwrap();
        (dir, path, store)
    }

    #[tokio::test]
    async fn startup_and_validation() {
        let (_dir, path, store) = fixture().await;
        let empty = SubagentModels::load(&path, store.clone()).await.unwrap();
        assert_eq!(empty.snapshot().policy, Policy::default());
        for invalid in [
            "",
            "null",
            "[]",
            "typo: value",
            "fallback: ''",
            "fallback: future-official-model",
            "fallback: {model: ''}",
            "fallback: {model: plugin:bad}",
            "fallback: {model: 0123456789abcdef}",
            "fallback: {model: ok}",
            "fallback: {model: ok, effort: ''}",
            "fallback: {model: ok, effort: none}",
            "fallback: {model: ok, effort: ' none'}",
            "fallback: {model: composer-2.5, effort: high}",
            "fallback: {model: official-A, effort: high, fast: yes}",
            "fallback: {model: official-A, effort: high, fast: \"true\"}",
            "types: {explore: string-target}",
            "types: {explore: {model: inherit}}",
            "types: {explore: {model: inherit, effort: high}}",
            "mapping: {source: {model: inherit}}",
            "mapping: {0123456789abcdef: {model: inherit, effort: high}}",
        ] {
            tokio::fs::write(&path, invalid).await.unwrap();
            assert!(
                SubagentModels::load(&path, store.clone()).await.is_err(),
                "accepted {invalid:?}"
            );
        }
        for valid in [
            "{}",
            "fallback: {model: future-official-model, effort: high}",
            "fallback: {model: plugin:example/provider/upstream/model, effort: high}",
            "fallback: {model: composer-2.5}",
            "fallback: {model: composer-2.5, fast: true}",
            "fallback: {model: official-A, effort: high, fast: false}",
            "fallback: {model: official-A, effort: high, fast: true}",
            "types: {explore: {model: x, effort: medium}}",
            "mapping: {composer-2.5: {model: official-B, effort: high}}",
            "mapping: {official-A: {model: composer-2.5}}",
            "mapping: {official-A: {model: composer-2.5, fast: true}}",
        ] {
            tokio::fs::write(&path, valid).await.unwrap();
            SubagentModels::load(&path, store.clone()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn non_composer_missing_effort_rejected_and_reload_keeps_previous_snapshot() {
        let (_dir, path, store) = fixture().await;
        tokio::fs::write(&path, "fallback: {model: official-target, effort: high}")
            .await
            .unwrap();
        let models = SubagentModels::load(&path, store).await.unwrap();
        let before = models.snapshot();
        tokio::fs::write(&path, "fallback: {model: official-target}")
            .await
            .unwrap();
        let error = models.reload().await.unwrap_err().to_string();
        assert!(error.contains("requires effort"), "{error}");
        assert!(Arc::ptr_eq(&before, &models.snapshot()));
    }

    #[tokio::test]
    async fn composer_effort_forbidden_and_omitted_allowed() {
        let (_dir, path, store) = fixture().await;
        tokio::fs::write(&path, "fallback: {model: composer-2.5, effort: high}")
            .await
            .unwrap();
        let error = SubagentModels::load(&path, store.clone())
            .await
            .err()
            .expect("composer effort must be rejected");
        assert!(error.to_string().contains("must not set effort"), "{error}");
        tokio::fs::write(&path, "types: {explore: {model: composer-2.5}}")
            .await
            .unwrap();
        let models = SubagentModels::load(&path, store).await.unwrap();
        assert_eq!(
            models.snapshot().resolve("explore", "source").target,
            ModelTarget {
                model: "composer-2.5".into(),
                effort: None,
                fast: false,
            }
        );
    }

    #[tokio::test]
    async fn fast_omitted_defaults_false_and_explicit_values_load() {
        let (_dir, path, store) = fixture().await;
        tokio::fs::write(
            &path,
            "types:\n  explore: {model: official-A, effort: high}\n  shell: {model: composer-2.5, fast: true}\nmapping:\n  source: {model: official-B, effort: medium, fast: false}",
        )
        .await
        .unwrap();
        let models = SubagentModels::load(&path, store).await.unwrap();
        let explore = models.snapshot().resolve("explore", "source").target;
        assert!(!explore.fast);
        assert_eq!(explore.model, "official-A");
        let shell = models.snapshot().resolve("shell", "source").target;
        assert!(shell.fast);
        assert_eq!(shell.model, "composer-2.5");
        let mapped = models.snapshot().resolve("other", "source").target;
        assert!(!mapped.fast);
        assert_eq!(mapped.model, "official-B");
    }

    #[tokio::test]
    async fn accepts_persisted_local_hashes_and_rejects_deleted_ones() {
        let (_dir, path, store) = fixture().await;
        let input = serde_json::from_value(serde_json::json!({
            "display_name": "Policy test model", "type": "openai",
            "base_url": "https://example.com/v1", "api_key": "test",
            "tooltip_data": "Policy test model", "model_id": "test-model"
        }))
        .unwrap();
        let model = store.create_model(&input).await.unwrap();
        tokio::fs::write(
            &path,
            format!("fallback: {{model: '{}', effort: high}}", model.model_hash),
        )
        .await
        .unwrap();
        let models = SubagentModels::load(&path, store.clone()).await.unwrap();
        assert_eq!(
            models.snapshot().resolve("explore", "inherit").target,
            ModelTarget {
                model: model.model_hash.clone(),
                effort: Some("high".into()),
                fast: false,
            }
        );
        store.delete_model(&model.model_hash).await.unwrap();
        assert!(SubagentModels::load(&path, store).await.is_err());
        assert!(models.reload().await.is_err());
        assert_eq!(models.snapshot().version, 1);
    }

    #[tokio::test]
    async fn reload_retains_invalid_and_deleted_but_empty_mapping_clears() {
        let (_dir, path, store) = fixture().await;
        tokio::fs::write(&path, "fallback: {model: original, effort: high}")
            .await
            .unwrap();
        let models = SubagentModels::load(&path, store).await.unwrap();
        let original = models.snapshot();
        let clone = models.clone();
        tokio::fs::write(&path, "fallback: [invalid]")
            .await
            .unwrap();
        assert!(models.reload().await.is_err());
        assert!(Arc::ptr_eq(&original, &clone.snapshot()));
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(models.reload().await.is_err());
        assert!(Arc::ptr_eq(&original, &clone.snapshot()));
        tokio::fs::write(
            &path,
            "# formatting only\nfallback: {model: original, effort: high}\n",
        )
        .await
        .unwrap();
        models.reload().await.unwrap();
        assert!(Arc::ptr_eq(&original, &clone.snapshot()));
        tokio::fs::write(&path, "fallback: {model: original, effort: ''}")
            .await
            .unwrap();
        assert!(models.reload().await.is_err());
        assert!(Arc::ptr_eq(&original, &clone.snapshot()));
        tokio::fs::write(&path, "{}").await.unwrap();
        models.reload().await.unwrap();
        assert_eq!(clone.snapshot().version, 2);
        assert_eq!(clone.snapshot().policy, Policy::default());
        assert_eq!(
            original
                .policy
                .fallback
                .as_ref()
                .map(|target| (target.model.as_str(), target.effort.as_deref())),
            Some(("original", Some("high")))
        );
        tokio::fs::write(&path, "fallback: {model: original, effort: high}")
            .await
            .unwrap();
        models.reload().await.unwrap();
        assert_eq!(clone.snapshot().version, 3);
        assert_eq!(clone.snapshot().policy, original.policy);
        assert!(!Arc::ptr_eq(&original, &clone.snapshot()));
    }

    #[tokio::test]
    async fn polling_observes_atomic_replacement_and_stops_on_cancellation() {
        let (_dir, path, store) = fixture().await;
        let models = SubagentModels::load(&path, store).await.unwrap();
        let cancellation = CancellationToken::new();
        let worker_models = models.clone();
        let worker_token = cancellation.clone();
        let task = tokio::spawn(async move { worker_models.run_reload_loop(worker_token).await });
        let replacement = path.with_extension("tmp");
        tokio::fs::write(
            &replacement,
            "fallback: {model: replacement, effort: medium}",
        )
        .await
        .unwrap();
        tokio::fs::rename(&replacement, &path).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while models.snapshot().version == 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            models
                .snapshot()
                .policy
                .fallback
                .as_ref()
                .map(|target| (target.model.as_str(), target.effort.as_deref())),
            Some(("replacement", Some("medium")))
        );
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        tokio::fs::write(&path, "{}").await.unwrap();
        assert_eq!(models.snapshot().version, 2);
    }
}
