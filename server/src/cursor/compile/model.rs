//! Resolves Cursor model selections to configured provider models.
use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{
        parse_token_count, ModelLatency, ModelSpec, ReasoningSpec, SubagentKind,
        SubagentModelOverride,
    },
    Error, Result,
};

pub fn requested_model(request: &pb::AgentRunRequest) -> Result<ModelSpec> {
    let details = request.model_details.as_ref();
    let model = if let Some(requested) = request.requested_model.as_ref() {
        from_requested(requested, details)?
    } else if let Some(model_id) = details
        .map(|model| model.model_id.as_str())
        .filter(|model| !model.is_empty())
    {
        ModelSpec {
            model_id: model_id.into(),
            display_name: details
                .map(|model| model.display_name.clone())
                .filter(|name| !name.is_empty()),
            reasoning: ReasoningSpec {
                enabled: details.is_some_and(|model| model.thinking_details.is_some()),
                effort: None,
            },
            latency: ModelLatency::Standard,
            max_output_tokens: None,
            context_window_tokens: None,
            supports_image_generation: false,
            extra_params: serde_json::json!({}),
        }
    } else {
        return Err(Error::Protocol("Cursor Run does not select a model".into()));
    };
    Ok(model)
}

pub fn overrides(
    request: &pb::AgentRunRequest,
) -> Result<Vec<(SubagentKind, SubagentModelOverride)>> {
    request
        .subagent_model_overrides
        .iter()
        .map(|value| {
            use pb::subagent_model_override::Selection;
            let kind = subagent_kind(&value.subagent_type);
            let selection = match value.selection.as_ref() {
                Some(Selection::Model(model)) => {
                    if model.model_id == "default" {
                        SubagentModelOverride::Inherit
                    } else {
                        SubagentModelOverride::Explicit(from_requested(model, None)?)
                    }
                }
                Some(Selection::Inherit(true)) => SubagentModelOverride::Inherit,
                Some(Selection::Disabled(true)) => SubagentModelOverride::Disabled,
                None | Some(Selection::Inherit(false) | Selection::Disabled(false)) => {
                    return Err(Error::Protocol(format!(
                        "Cursor subagent model override {} has no active selection",
                        value.subagent_type
                    )))
                }
            };
            Ok((kind, selection))
        })
        .collect()
}

pub fn subagent_kind(value: &str) -> SubagentKind {
    SubagentKind::from_type_name(value)
}

/// Candidate local model for hijacking an official subagent Run.
///
/// Returns `Some` only when this is a subagent Run (`subagent_type_name` set)
/// and that type has an `Explicit` override. The caller must still confirm the
/// model is a local BYOK target (`plugin:` adapter ID or a `model_configs` hash)
/// before rewriting / routing.
pub fn local_subagent_hijack_model(request: &pb::AgentRunRequest) -> Result<Option<ModelSpec>> {
    let Some(type_name) = request
        .subagent_type_name
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let kind = subagent_kind(type_name);
    let overrides = overrides(request)?;
    Ok(match crate::model::override_for(&overrides, &kind) {
        Some(SubagentModelOverride::Explicit(model)) => Some(model.clone()),
        Some(SubagentModelOverride::Inherit | SubagentModelOverride::Disabled) | None => None,
    })
}

/// Rewrite a RunRequest so subsequent local compile uses `model`.
pub fn rewrite_requested_model(request: &mut pb::AgentRunRequest, model: &ModelSpec) {
    let kind = request
        .subagent_type_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(subagent_kind);
    let from_override = kind.and_then(|kind| {
        request.subagent_model_overrides.iter().find_map(|value| {
            if subagent_kind(&value.subagent_type) != kind {
                return None;
            }
            use pb::subagent_model_override::Selection;
            match value.selection.as_ref()? {
                Selection::Model(requested) if requested.model_id == model.model_id => {
                    Some(requested.clone())
                }
                _ => None,
            }
        })
    });
    request.requested_model = Some(from_override.unwrap_or_else(|| pb::RequestedModel {
        model_id: model.model_id.clone(),
        ..Default::default()
    }));
    if let Some(details) = request.model_details.as_mut() {
        details.model_id = model.model_id.clone();
        if let Some(display_name) = &model.display_name {
            details.display_name = display_name.clone();
        }
    }
}

fn from_requested(
    model: &pb::RequestedModel,
    details: Option<&pb::ModelDetails>,
) -> Result<ModelSpec> {
    let mut spec = ModelSpec {
        model_id: model.model_id.clone(),
        display_name: details
            .map(|model| model.display_name.clone())
            .filter(|name| !name.is_empty()),
        reasoning: ReasoningSpec {
            enabled: model.max_mode
                || details.is_some_and(|model| model.thinking_details.is_some()),
            effort: None,
        },
        latency: ModelLatency::Standard,
        max_output_tokens: None,
        context_window_tokens: None,
        supports_image_generation: false,
        extra_params: serde_json::json!({}),
    };
    for parameter in &model.parameters {
        match parameter.id.as_str() {
            "effort" | "reasoning" => {
                let effort = parameter.value.trim();
                spec.reasoning.effort =
                    (effort != "none" && !effort.is_empty()).then(|| effort.to_string());
                spec.reasoning.enabled |= spec.reasoning.effort.is_some();
            }
            "thinking" => spec.reasoning.enabled |= parse_bool(parameter)?,
            "fast" => {
                if parse_bool(parameter)? {
                    spec.latency = ModelLatency::Fast;
                }
            }
            "context" => {
                spec.context_window_tokens =
                    Some(parse_token_count(&parameter.value).ok_or_else(|| {
                        Error::Protocol(format!(
                            "invalid Cursor context token count: {}",
                            parameter.value
                        ))
                    })?);
            }
            _ => {}
        }
    }
    Ok(spec)
}

fn parse_bool(parameter: &pb::requested_model::ModelParameterValue) -> Result<bool> {
    match parameter.value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::Protocol(format!(
            "invalid Cursor boolean model parameter {}={}",
            parameter.id, parameter.value
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{override_for, ModelSpec, SubagentKind, SubagentModelOverride};

    #[test]
    fn ignores_unknown_cursor_model_parameters() {
        let requested = pb::RequestedModel {
            model_id: "test-model".into(),
            parameters: vec![pb::requested_model::ModelParameterValue {
                id: "optimize_for".into(),
                value: "quality".into(),
            }],
            ..Default::default()
        };

        let model = from_requested(&requested, None).expect("unknown parameter should be ignored");

        assert_eq!(model.model_id, "test-model");
        assert_eq!(model.latency, ModelLatency::Standard);
        assert!(!model.reasoning.enabled);
    }

    fn explore_override(model_id: &str) -> pb::SubagentModelOverride {
        pb::SubagentModelOverride {
            subagent_type: "explore".into(),
            selection: Some(pb::subagent_model_override::Selection::Model(
                pb::RequestedModel {
                    model_id: model_id.into(),
                    parameters: vec![pb::requested_model::ModelParameterValue {
                        id: "reasoning".into(),
                        value: "medium".into(),
                    }],
                    ..Default::default()
                },
            )),
        }
    }

    fn inherit_override(subagent_type: &str) -> pb::SubagentModelOverride {
        pb::SubagentModelOverride {
            subagent_type: subagent_type.into(),
            selection: Some(pb::subagent_model_override::Selection::Inherit(true)),
        }
    }

    #[test]
    fn override_for_matches_named_kind() {
        let overrides = vec![
            (
                SubagentKind::Named("explore".into()),
                SubagentModelOverride::Explicit(ModelSpec::new("luna")),
            ),
            (SubagentKind::GeneralPurpose, SubagentModelOverride::Inherit),
        ];
        assert!(matches!(
            override_for(&overrides, &SubagentKind::Named("explore".into())),
            Some(SubagentModelOverride::Explicit(model)) if model.model_id == "luna"
        ));
        assert!(matches!(
            override_for(&overrides, &SubagentKind::GeneralPurpose),
            Some(SubagentModelOverride::Inherit)
        ));
        assert!(override_for(&overrides, &SubagentKind::Named("shell".into())).is_none());
    }

    #[test]
    fn hijack_requires_subagent_type_and_explicit_local_override() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                ..Default::default()
            }),
            subagent_model_overrides: vec![explore_override("luna-hash")],
            ..Default::default()
        };
        assert!(local_subagent_hijack_model(&request).unwrap().is_none());

        request.subagent_type_name = Some("explore".into());
        let hijack = local_subagent_hijack_model(&request)
            .unwrap()
            .expect("explore + explicit should hijack");
        assert_eq!(hijack.model_id, "luna-hash");
        assert_eq!(hijack.reasoning.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn hijack_accepts_plugin_adapter_model_ids() {
        let plugin_id = "plugin:dev.cursorbyok.examples.codex-auth/codex/gpt-6-luna";
        let request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "grok-4.5".into(),
                ..Default::default()
            }),
            subagent_type_name: Some("explore".into()),
            subagent_model_overrides: vec![explore_override(plugin_id)],
            ..Default::default()
        };
        let hijack = local_subagent_hijack_model(&request)
            .unwrap()
            .expect("explicit plugin override should be a hijack candidate");
        assert_eq!(hijack.model_id, plugin_id);
        assert!(hijack.model_id.starts_with("plugin:"));
    }

    #[test]
    fn hijack_skips_inherit_override() {
        let request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                ..Default::default()
            }),
            subagent_type_name: Some("explore".into()),
            subagent_model_overrides: vec![inherit_override("explore")],
            ..Default::default()
        };
        assert!(local_subagent_hijack_model(&request).unwrap().is_none());
    }

    #[test]
    fn rewrite_requested_model_copies_override_parameters() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                ..Default::default()
            }),
            model_details: Some(pb::ModelDetails {
                model_id: "composer-2.5".into(),
                display_name: "Composer".into(),
                ..Default::default()
            }),
            subagent_type_name: Some("explore".into()),
            subagent_model_overrides: vec![explore_override("luna-hash")],
            ..Default::default()
        };
        let model = local_subagent_hijack_model(&request).unwrap().unwrap();
        rewrite_requested_model(&mut request, &model);
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(requested.model_id, "luna-hash");
        assert_eq!(
            requested
                .parameters
                .iter()
                .find(|parameter| parameter.id == "reasoning")
                .map(|parameter| parameter.value.as_str()),
            Some("medium")
        );
        assert_eq!(
            request.model_details.as_ref().unwrap().model_id,
            "luna-hash"
        );
    }
}
