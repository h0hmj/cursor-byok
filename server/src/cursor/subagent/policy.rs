//! Deterministic, single-pass model selection and provider-visible policy context.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::{plugin::parse_model_id, store::Store, Error, Result};

/// Composer model IDs in this repo/catalog use the `composer-` prefix (e.g. `composer-2.5`).
pub fn is_composer_model(model_id: &str) -> bool {
    model_id.starts_with("composer-")
}

/// One YAML rule target: required model identity plus effort rules by target kind.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelTarget {
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

impl ModelTarget {
    pub fn model_only(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            effort: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub fallback: Option<ModelTarget>,
    pub types: BTreeMap<String, ModelTarget>,
    pub mapping: BTreeMap<String, ModelTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionReason {
    Type,
    Mapping,
    Fallback,
    Original,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// Configured targets never use `inherit` (rejected at load). `inherit` appears only as
    /// [`ResolutionReason::Original`] when Task.model is `inherit`; the caller resolves the parent.
    pub target: ModelTarget,
    pub reason: ResolutionReason,
}

#[derive(Clone, Debug)]
pub struct PolicySnapshot {
    /// Increases only when the validated policy changes, not when YAML formatting changes.
    pub version: u64,
    pub policy: Policy,
}

impl PolicySnapshot {
    pub fn resolve(&self, subagent_type: &str, requested: &str) -> Resolution {
        self.policy.resolve(subagent_type, requested)
    }

    /// Stable content only: no version, timestamps, or file paths in model history.
    pub fn context(&self) -> String {
        self.policy.context()
    }
}

impl Policy {
    pub fn resolve(&self, subagent_type: &str, requested: &str) -> Resolution {
        let (target, reason) = if let Some(target) = self.types.get(subagent_type) {
            (target.clone(), ResolutionReason::Type)
        } else if let Some(target) = self.mapping.get(requested) {
            (target.clone(), ResolutionReason::Mapping)
        } else if let Some(target) = &self.fallback {
            (target.clone(), ResolutionReason::Fallback)
        } else {
            (
                ModelTarget::model_only(requested),
                ResolutionReason::Original,
            )
        };
        Resolution { target, reason }
    }

    fn configured_targets(&self) -> impl Iterator<Item = &ModelTarget> {
        self.fallback
            .iter()
            .chain(self.types.values())
            .chain(self.mapping.values())
    }

    fn referenced_models(&self) -> BTreeSet<&str> {
        // `inherit` remains a Task.model option in context; it is not a YAML target.
        std::iter::once("inherit")
            .chain(self.fallback.as_ref().map(|target| target.model.as_str()))
            .chain(self.types.values().map(|target| target.model.as_str()))
            .chain(self.mapping.keys().map(String::as_str))
            .chain(self.mapping.values().map(|target| target.model.as_str()))
            .collect()
    }

    pub fn context(&self) -> String {
        let mut context = String::from(
            "Subagent model policy (supersedes earlier model lists):\nUse only the referenced model IDs below when selecting Task.model. The latest server policy at child startup is authoritative, even if this context predates a reload.\nSelection is single-pass: exact type override > exact requested-model mapping > fallback > original requested model. Selected targets are never mapped again. A hit uses the whole target object; effort is not merged from lower-priority rules.\nYAML targets must not use model \"inherit\". Task.model \"inherit\" still selects the parent model identity, then this policy applies.\nNon-Composer YAML targets require effort; Composer targets (`composer-*`) forbid effort.\nReferenced model IDs (including mapping keys; not an exhaustive official-model catalog):\n",
        );
        for model in self.referenced_models() {
            context.push_str(&format!("- {model:?}\n"));
        }
        context.push_str("Type overrides:\n");
        for (kind, target) in &self.types {
            context.push_str(&format!("- {kind:?} -> {}\n", format_target(target)));
        }
        context.push_str("Requested-model mappings:\n");
        for (requested, target) in &self.mapping {
            context.push_str(&format!("- {requested:?} -> {}\n", format_target(target)));
        }
        match &self.fallback {
            Some(target) => context.push_str(&format!("Fallback: {}\n", format_target(target))),
            None => context.push_str("Fallback: none; preserve the original requested model.\n"),
        }
        context
    }

    pub(super) async fn parse(bytes: &[u8], store: &Store) -> Result<Self> {
        // An empty document is usually an interrupted save, not an explicit reset.
        let value: serde_yaml::Value = serde_yaml::from_slice(bytes)
            .map_err(|error| Error::Config(format!("invalid subagent model policy: {error}")))?;
        if !value.is_mapping() {
            return Err(Error::Config(
                "subagent model policy must be a YAML mapping; use {} to clear it".into(),
            ));
        }
        let policy: Self = serde_yaml::from_value(value)
            .map_err(|error| Error::Config(format!("invalid subagent model policy: {error}")))?;
        for kind in policy.types.keys() {
            validate_identifier(kind)?;
        }
        for target in policy.configured_targets() {
            validate_configured_target(target)?;
        }
        for model in policy.referenced_models() {
            validate_identifier(model)?;
            if model.starts_with("plugin:") {
                if parse_model_id(model).is_none() {
                    return Err(Error::Config(format!("invalid plugin model ID: {model}")));
                }
            } else if model.len() == 16
                && model.bytes().all(|byte| byte.is_ascii_hexdigit())
                && store.model(model).await?.is_none()
            {
                return Err(Error::Config(format!("unknown local model hash: {model}")));
            }
        }
        Ok(policy)
    }
}

fn format_target(target: &ModelTarget) -> String {
    match &target.effort {
        Some(effort) => format!("model={:?} effort={effort:?}", target.model),
        None => format!("model={:?}", target.model),
    }
}

fn validate_configured_target(target: &ModelTarget) -> Result<()> {
    if target.model == "inherit" {
        return Err(Error::Config(
            "subagent policy target model cannot be \"inherit\"; use Task.model inherit instead"
                .into(),
        ));
    }
    validate_identifier(&target.model)?;
    if is_composer_model(&target.model) {
        if target.effort.is_some() {
            return Err(Error::Config(format!(
                "subagent policy Composer target {} must not set effort",
                target.model
            )));
        }
        return Ok(());
    }
    let Some(effort) = target.effort.as_deref() else {
        return Err(Error::Config(format!(
            "subagent policy non-Composer target {} requires effort",
            target.model
        )));
    };
    validate_effort(effort)
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(Error::Config(format!(
            "invalid subagent policy identifier: {value:?}"
        )));
    }
    Ok(())
}

fn validate_effort(value: &str) -> Result<()> {
    if value.is_empty()
        || value.eq_ignore_ascii_case("none")
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(Error::Config(format!(
            "invalid subagent policy effort: {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(model: &str) -> ModelTarget {
        ModelTarget::model_only(model)
    }

    fn target_with_effort(model: &str, effort: &str) -> ModelTarget {
        ModelTarget {
            model: model.into(),
            effort: Some(effort.into()),
        }
    }

    #[test]
    fn composer_ids_use_composer_prefix() {
        assert!(is_composer_model("composer-2.5"));
        assert!(is_composer_model("composer-2.5-fast"));
        assert!(!is_composer_model("cursor-grok-4.5-high"));
        assert!(!is_composer_model("inherit"));
    }

    #[test]
    fn precedence_is_single_pass_and_does_not_merge_effort() {
        let policy = Policy {
            fallback: Some(target_with_effort("fallback", "low")),
            types: BTreeMap::from([("explore".into(), target_with_effort("type-model", "high"))]),
            mapping: BTreeMap::from([
                ("original".into(), target_with_effort("mapped", "medium")),
                ("mapped".into(), target_with_effort("recursive", "low")),
                ("inherit".into(), target_with_effort("not-parent", "high")),
            ]),
        };
        assert_eq!(
            policy.resolve("explore", "original"),
            Resolution {
                target: target_with_effort("type-model", "high"),
                reason: ResolutionReason::Type
            }
        );
        assert_eq!(
            policy.resolve("other", "original"),
            Resolution {
                target: target_with_effort("mapped", "medium"),
                reason: ResolutionReason::Mapping
            }
        );
        assert_eq!(
            policy.resolve("other", "unknown"),
            Resolution {
                target: target_with_effort("fallback", "low"),
                reason: ResolutionReason::Fallback
            }
        );
        assert_eq!(
            Policy::default().resolve("other", "inherit"),
            Resolution {
                target: target("inherit"),
                reason: ResolutionReason::Original
            }
        );
        // Type hit without effort must not pull fallback effort (in-memory; YAML rejects this).
        let no_merge = Policy {
            fallback: Some(target_with_effort("fallback", "low")),
            types: BTreeMap::from([("explore".into(), target("type-only"))]),
            mapping: BTreeMap::new(),
        };
        assert_eq!(
            no_merge.resolve("explore", "original"),
            Resolution {
                target: target("type-only"),
                reason: ResolutionReason::Type
            }
        );
    }

    #[test]
    fn context_is_sorted_complete_and_version_independent() {
        let policy = Policy {
            fallback: Some(target_with_effort("fallback", "high")),
            types: BTreeMap::from([
                ("z".into(), target("composer-2.5")),
                ("a".into(), target_with_effort("type-target", "medium")),
            ]),
            mapping: BTreeMap::from([(
                "mapping-key".into(),
                target_with_effort("mapped-target", "low"),
            )]),
        };
        let context = policy.context();
        for id in [
            "inherit",
            "fallback",
            "composer-2.5",
            "type-target",
            "mapping-key",
            "mapped-target",
        ] {
            assert!(context.contains(&format!("- {id:?}\n")));
        }
        assert!(context.contains("model=\"type-target\" effort=\"medium\""));
        assert!(context.contains("model=\"composer-2.5\""));
        assert!(context.contains("Fallback: model=\"fallback\" effort=\"high\""));
        assert!(context.contains("YAML targets must not use model \"inherit\""));
        assert!(context.find("- \"a\" ->").unwrap() < context.find("- \"z\" ->").unwrap());
        assert_eq!(
            context,
            PolicySnapshot {
                version: 99,
                policy
            }
            .context()
        );
    }

    #[test]
    fn configured_target_validation_rules() {
        assert!(validate_configured_target(&target_with_effort("official", "high")).is_ok());
        assert!(validate_configured_target(&target("composer-2.5")).is_ok());
        assert!(validate_configured_target(&target("official"))
            .unwrap_err()
            .to_string()
            .contains("requires effort"));
        assert!(
            validate_configured_target(&target_with_effort("composer-2.5", "high"))
                .unwrap_err()
                .to_string()
                .contains("must not set effort")
        );
        assert!(
            validate_configured_target(&target_with_effort("inherit", "high"))
                .unwrap_err()
                .to_string()
                .contains("cannot be \"inherit\"")
        );
        assert!(validate_configured_target(&target("inherit"))
            .unwrap_err()
            .to_string()
            .contains("cannot be \"inherit\""));
    }
}
