//! Maps request IDs to active transport handles.

#[cfg(test)]
#[path = "model_selection_tests.rs"]
mod model_selection_tests;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{
        conversation::ConversationRegistry, prompting::PromptCompiler,
        services::observability::CursorTraceService,
    },
    plugin::PluginRegistry,
    provider::Provider,
    search::WebCache,
    store::Store,
    Result,
};

use crate::cursor::compile::EffortAction;

use super::{OutputHub, TransportHandle};

#[derive(Clone)]
pub struct TransportRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    local: Mutex<HashMap<String, LocalTransport>>,
    next_local_generation: AtomicU64,
    upstream: Mutex<HashMap<String, UpstreamRoute>>,
    /// Serializes Run model ownership + route binding so select→admit cannot diverge.
    admit_gate: Mutex<()>,
    route_changed: Notify,
    store: Store,
    traces: CursorTraceService,
    web_cache: WebCache,
    plugins: Option<PluginRegistry>,
    conversations: ConversationRegistry,
    subagent_models: crate::cursor::subagent::SubagentModels,
}

#[derive(Clone)]
struct LocalTransport {
    generation: u64,
    handle: TransportHandle,
    selection: Option<RunModelBinding>,
}

#[derive(Clone)]
struct UpstreamRoute {
    generation: u64,
    selection: Option<RunModelBinding>,
}

/// Fixed child selection vs overwritable primary identity for inherit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRole {
    Primary,
    Child,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunModelBinding {
    pub(crate) model: String,
    pub(crate) effort: EffortAction,
    pub(crate) role: ModelRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRoute {
    Local,
    Upstream(u64),
}

/// Policy candidate for a Run; ownership is decided only at admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRunModel {
    /// Wire model before policy rewrite.
    pub original: String,
    /// Policy (or primary) candidate; Child may lose to an existing lifecycle pin.
    pub model: String,
    /// Policy effort action pinned with the child lifecycle; primary stays Unchanged.
    pub effort: EffortAction,
    pub role: ModelRole,
}

/// Authoritative model + route after atomic admission.
#[derive(Clone)]
pub struct AdmittedRun {
    pub model: String,
    pub effort: EffortAction,
    pub role: ModelRole,
    pub local: bool,
    /// Local transport when `local`; upstream admissions leave this `None`.
    pub handle: Option<TransportHandle>,
    /// Primary local identity commits only after transport admit + parent succeed.
    pub defer_primary: bool,
}

impl AdmittedRun {
    /// Whether the admitted selection requires rewriting the original wire body.
    pub fn rewrites_wire(&self, original_model: &str) -> bool {
        self.model != original_model || self.effort.rewrites_parameters()
    }
}

impl TransportRegistry {
    pub fn new(store: Store, provider: Arc<dyn Provider>, compiler: PromptCompiler) -> Self {
        Self::with_web_cache(store, provider, compiler, WebCache::default())
    }

    pub fn with_subagent_models(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        subagent_models: crate::cursor::subagent::SubagentModels,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            WebCache::default(),
            None,
            None,
            subagent_models,
        )
    }

    pub fn with_web_cache(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
    ) -> Self {
        let subagent_models = crate::cursor::subagent::SubagentModels::empty(store.clone());
        Self::build(
            store,
            provider,
            compiler,
            web_cache,
            None,
            None,
            subagent_models,
        )
    }

    /// 附带本地 rules 目录的构造;编译请求上下文时会合并该目录下的 md 规则。
    pub fn with_local_rules(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        local_rules_dir: std::path::PathBuf,
    ) -> Self {
        let subagent_models = crate::cursor::subagent::SubagentModels::empty(store.clone());
        Self::build(
            store,
            provider,
            compiler,
            WebCache::default(),
            None,
            Some(local_rules_dir),
            subagent_models,
        )
    }

    pub fn with_plugins(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: PluginRegistry,
        local_rules_dir: std::path::PathBuf,
        subagent_models: crate::cursor::subagent::SubagentModels,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            web_cache,
            Some(plugins),
            Some(local_rules_dir),
            subagent_models,
        )
    }

    fn build(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: Option<PluginRegistry>,
        local_rules_dir: Option<std::path::PathBuf>,
        subagent_models: crate::cursor::subagent::SubagentModels,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                local: Mutex::new(HashMap::new()),
                next_local_generation: AtomicU64::new(1),
                upstream: Mutex::new(HashMap::new()),
                admit_gate: Mutex::new(()),
                route_changed: Notify::new(),
                traces: CursorTraceService::new(store.clone()),
                conversations: ConversationRegistry::new(
                    store.clone(),
                    provider,
                    compiler,
                    web_cache.clone(),
                    local_rules_dir,
                    subagent_models.clone(),
                ),
                subagent_models,
                store,
                web_cache,
                plugins,
            }),
        }
    }

    /// Active Run model for child `inherit`, regardless of primary/child role.
    pub async fn run_model(&self, request_id: &str) -> Option<String> {
        if let Some(transport) = self.inner.local.lock().await.get(request_id) {
            if let Some(selection) = &transport.selection {
                return Some(selection.model.clone());
            }
        }
        self.inner
            .upstream
            .lock()
            .await
            .get(request_id)
            .and_then(|route| {
                route
                    .selection
                    .as_ref()
                    .map(|selection| selection.model.clone())
            })
    }

    /// Child pin for the still-accepting local lifecycle, or the active upstream route.
    pub async fn active_child_model(&self, request_id: &str) -> Option<String> {
        self.active_child_binding(request_id)
            .await
            .map(|selection| selection.model)
    }

    async fn active_child_binding(&self, request_id: &str) -> Option<RunModelBinding> {
        {
            let local = self.inner.local.lock().await;
            if let Some(selection) = active_local_child(&local, request_id) {
                return Some(selection);
            }
        }
        active_upstream_child(&*self.inner.upstream.lock().await, request_id)
    }

    pub fn subagent_models(&self) -> &crate::cursor::subagent::SubagentModels {
        &self.inner.subagent_models
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn trace(
        &self,
        request_id: &str,
    ) -> crate::cursor::services::observability::CursorTraceRecorder {
        self.inner.traces.recorder(request_id)
    }

    pub fn web_cache(&self) -> &WebCache {
        &self.inner.web_cache
    }

    pub fn plugins(&self) -> Option<&PluginRegistry> {
        self.inner.plugins.as_ref()
    }

    pub fn conversations(&self) -> &ConversationRegistry {
        &self.inner.conversations
    }

    pub async fn get_or_create(&self, request_id: &str) -> Result<TransportHandle> {
        self.get_or_create_for_append(request_id, false, None)
            .await
            .map(|(handle, _)| handle)
    }

    /// Atomically decide owned model, validate locality, and bind the matching route.
    ///
    /// Child first-write-wins against the active lifecycle. Primary local identity is
    /// deferred (`defer_primary`) until transport admit + parent succeed.
    pub async fn admit_run_model(
        &self,
        request_id: &str,
        prepared: &PreparedRunModel,
    ) -> Result<AdmittedRun> {
        let _gate = self.inner.admit_gate.lock().await;
        let owned = match prepared.role {
            ModelRole::Child => self
                .active_child_binding(request_id)
                .await
                .unwrap_or_else(|| RunModelBinding {
                    model: prepared.model.clone(),
                    effort: prepared.effort.clone(),
                    role: ModelRole::Child,
                }),
            ModelRole::Primary => RunModelBinding {
                model: prepared.model.clone(),
                effort: EffortAction::Unchanged,
                role: ModelRole::Primary,
            },
        };
        let local = self.model_is_local(&owned.model).await?;
        if local {
            let bind = match prepared.role {
                ModelRole::Child => Some(owned.clone()),
                ModelRole::Primary => None,
            };
            let (handle, bound) = self
                .get_or_create_for_append(request_id, true, bind)
                .await?;
            let selection = bound.unwrap_or(owned);
            Ok(AdmittedRun {
                model: selection.model,
                effort: selection.effort,
                role: prepared.role,
                local: true,
                handle: Some(handle),
                defer_primary: prepared.role == ModelRole::Primary,
            })
        } else {
            let selection = self
                .mark_upstream(
                    request_id,
                    &owned.model,
                    owned.effort.clone(),
                    prepared.role,
                )
                .await;
            Ok(AdmittedRun {
                model: selection.model,
                effort: selection.effort,
                role: prepared.role,
                local: false,
                handle: None,
                defer_primary: false,
            })
        }
    }

    /// Commit primary identity after local transport admit + parent headers succeed.
    pub async fn commit_primary_model(&self, request_id: &str, model: &str) -> Result<()> {
        let _gate = self.inner.admit_gate.lock().await;
        let mut local = self.inner.local.lock().await;
        let transport = local
            .get_mut(request_id)
            .ok_or_else(|| crate::Error::RunNotFound(request_id.to_owned()))?;
        if !transport.handle.accepting_appends() {
            return Err(crate::Error::RunNotFound(request_id.to_owned()));
        }
        transport.selection = Some(RunModelBinding {
            model: model.to_owned(),
            effort: EffortAction::Unchanged,
            role: ModelRole::Primary,
        });
        Ok(())
    }

    pub(crate) async fn get_or_create_for_append(
        &self,
        request_id: &str,
        replace_closing: bool,
        admission: Option<RunModelBinding>,
    ) -> Result<(TransportHandle, Option<RunModelBinding>)> {
        let mut local = self.inner.local.lock().await;
        if let Some(transport) = local.get_mut(request_id) {
            if transport.handle.accepting_appends() || !replace_closing {
                let owned = admission.map(|candidate| {
                    apply_selection(
                        &mut transport.selection,
                        &candidate.model,
                        candidate.effort.clone(),
                        candidate.role,
                    )
                });
                return Ok((transport.handle.clone(), owned));
            }
        }
        local.remove(request_id);
        let (commands, receiver) = mpsc::channel(128);
        let output = Arc::new(OutputHub::default());
        let trace = self.inner.traces.recorder(request_id);
        trace.resume();
        let handle = TransportHandle::new(request_id.into(), commands, output, trace);
        let generation = self
            .inner
            .next_local_generation
            .fetch_add(1, Ordering::Relaxed);
        let mut selection = None;
        let owned = admission.map(|candidate| {
            apply_selection(
                &mut selection,
                &candidate.model,
                candidate.effort.clone(),
                candidate.role,
            )
        });
        local.insert(
            request_id.into(),
            LocalTransport {
                generation,
                handle: handle.clone(),
                selection,
            },
        );
        drop(local);
        self.inner.route_changed.notify_waiters();
        self.inner
            .conversations
            .bind_transport(handle.clone(), receiver);

        let registry = Arc::downgrade(&self.inner);
        let request_id = request_id.to_string();
        let lifecycle = handle.clone();
        tokio::spawn(async move {
            lifecycle.wait_transport_closed().await;
            if let Some(registry) = registry.upgrade() {
                let mut local = registry.local.lock().await;
                if local
                    .get(&request_id)
                    .is_some_and(|transport| transport.generation == generation)
                {
                    local.remove(&request_id);
                }
            }
        });
        Ok((handle, owned))
    }

    pub async fn local(&self, request_id: &str) -> Option<TransportHandle> {
        self.inner
            .local
            .lock()
            .await
            .get(request_id)
            .map(|transport| transport.handle.clone())
    }

    /// Bind upstream routing; returns the selection that owns this generation.
    pub(crate) async fn mark_upstream(
        &self,
        request_id: &str,
        model: &str,
        effort: EffortAction,
        role: ModelRole,
    ) -> RunModelBinding {
        let mut upstream = self.inner.upstream.lock().await;
        let previous = upstream.get(request_id).cloned();
        let generation = previous.as_ref().map(|route| route.generation).unwrap_or(0) + 1;
        let owned = match role {
            ModelRole::Primary => RunModelBinding {
                model: model.to_owned(),
                effort: EffortAction::Unchanged,
                role,
            },
            ModelRole::Child => previous
                .and_then(|route| route.selection)
                .filter(|selection| selection.role == ModelRole::Child)
                .unwrap_or_else(|| RunModelBinding {
                    model: model.to_owned(),
                    effort,
                    role,
                }),
        };
        upstream.insert(
            request_id.into(),
            UpstreamRoute {
                generation,
                selection: Some(owned.clone()),
            },
        );
        drop(upstream);
        self.inner.route_changed.notify_waiters();
        owned
    }

    pub async fn upstream(&self, request_id: &str) -> bool {
        self.inner.upstream.lock().await.contains_key(request_id)
    }

    pub async fn wait_route(&self, request_id: &str) -> TransportRoute {
        loop {
            let changed = self.inner.route_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.local.lock().await.contains_key(request_id) {
                return TransportRoute::Local;
            }
            if let Some(generation) = self
                .inner
                .upstream
                .lock()
                .await
                .get(request_id)
                .map(|route| route.generation)
            {
                return TransportRoute::Upstream(generation);
            }
            changed.await;
        }
    }

    pub fn finish_upstream(
        &self,
        request_id: String,
        generation: u64,
    ) -> tokio::task::JoinHandle<()> {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut upstream = registry.inner.upstream.lock().await;
            if upstream
                .get(&request_id)
                .is_some_and(|route| route.generation == generation)
            {
                upstream.remove(&request_id);
            }
        })
    }

    pub async fn shutdown(&self) {
        self.inner.conversations.shutdown().await;
        let handles = std::mem::take(&mut *self.inner.local.lock().await);
        self.inner.upstream.lock().await.clear();
        for transport in handles.into_values() {
            transport.handle.disconnect().await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                transport.handle.wait_transport_closed(),
            )
            .await;
        }
    }

    async fn model_is_local(&self, model_id: &str) -> Result<bool> {
        if model_id.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
            if crate::plugin::parse_model_id(model_id).is_none() {
                return Err(crate::Error::Config(format!(
                    "invalid plugin model ID: {model_id}"
                )));
            }
            return Ok(true);
        }
        if self.store().model(model_id).await?.is_some() {
            return Ok(true);
        }
        if model_id.len() == 16 && model_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(crate::Error::Config(format!(
                "unknown local model hash: {model_id}"
            )));
        }
        Ok(false)
    }
}

fn active_local_child(
    local: &HashMap<String, LocalTransport>,
    request_id: &str,
) -> Option<RunModelBinding> {
    let transport = local.get(request_id)?;
    if !transport.handle.accepting_appends() {
        return None;
    }
    transport
        .selection
        .as_ref()
        .filter(|selection| selection.role == ModelRole::Child)
        .cloned()
}

fn active_upstream_child(
    upstream: &HashMap<String, UpstreamRoute>,
    request_id: &str,
) -> Option<RunModelBinding> {
    upstream.get(request_id).and_then(|route| {
        route
            .selection
            .as_ref()
            .filter(|selection| selection.role == ModelRole::Child)
            .cloned()
    })
}

/// Returns the selection that owns the slot after applying role rules.
fn apply_selection(
    slot: &mut Option<RunModelBinding>,
    model: &str,
    effort: EffortAction,
    role: ModelRole,
) -> RunModelBinding {
    match role {
        ModelRole::Primary => {
            let selection = RunModelBinding {
                model: model.to_owned(),
                effort: EffortAction::Unchanged,
                role,
            };
            *slot = Some(selection.clone());
            selection
        }
        ModelRole::Child => {
            if let Some(existing) = slot
                .as_ref()
                .filter(|selection| selection.role == ModelRole::Child)
            {
                return existing.clone();
            }
            let selection = RunModelBinding {
                model: model.to_owned(),
                effort,
                role,
            };
            *slot = Some(selection.clone());
            selection
        }
    }
}
