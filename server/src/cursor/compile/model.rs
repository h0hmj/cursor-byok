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

/// How policy applies effort on the wire request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EffortAction {
    /// Unmatched / primary / disabled: leave effort parameters alone.
    #[default]
    Unchanged,
    /// Explicit Composer target: clear `effort`/`reasoning`; do not set effort.
    Clear,
    /// Explicit non-Composer target with required effort.
    Set(String),
}

impl EffortAction {
    pub fn as_set(&self) -> Option<&str> {
        match self {
            Self::Set(value) => Some(value.as_str()),
            Self::Unchanged | Self::Clear => None,
        }
    }

    pub fn rewrites_parameters(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

/// Selected model identity and effort/fast actions applied to a Run request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRewrite {
    pub model_id: String,
    pub effort: EffortAction,
    /// `None` leaves request `fast` alone; `Some` forces that boolean on the wire.
    pub fast: Option<bool>,
}

impl ModelRewrite {
    #[cfg(test)]
    pub fn model_only(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            effort: EffortAction::Unchanged,
            fast: None,
        }
    }
}

/// Apply the selected model/effort/fast to the wire request.
///
/// - Model change: clear source-model parameters and details; apply Set effort and forced fast.
/// - Same model + Set: override `effort`/`reasoning` aliases only; preserve other parameters.
/// - Same model + Clear: strip `effort`/`reasoning` only.
/// - Same model + forced fast: replace every `fast` parameter with the selected value.
/// - Same model + Unchanged effort + no forced fast: leave the request unchanged.
/// - Empty `requested_model.model_id` falls back to `model_details` for same-model detection.
pub fn rewrite_requested_model(request: &mut pb::AgentRunRequest, selection: &ModelRewrite) {
    let current_id = request
        .requested_model
        .as_ref()
        .map(|model| model.model_id.as_str())
        .filter(|model| !model.is_empty())
        .or_else(|| {
            request
                .model_details
                .as_ref()
                .map(|model| model.model_id.as_str())
                .filter(|model| !model.is_empty())
        });
    let model_changed = current_id != Some(selection.model_id.as_str());
    if model_changed {
        request.requested_model = Some(pb::RequestedModel {
            model_id: selection.model_id.clone(),
            parameters: selection_parameters(&selection.effort, selection.fast),
            ..Default::default()
        });
        request.model_details = None;
        return;
    }
    if matches!(selection.effort, EffortAction::Unchanged) && selection.fast.is_none() {
        return;
    }
    match request.requested_model.as_mut() {
        Some(requested) => {
            if requested.model_id.is_empty() {
                requested.model_id = selection.model_id.clone();
            }
            match &selection.effort {
                EffortAction::Unchanged => {}
                EffortAction::Clear => clear_effort_aliases(&mut requested.parameters),
                EffortAction::Set(effort) => {
                    apply_effort_override(&mut requested.parameters, effort)
                }
            }
            if let Some(fast) = selection.fast {
                apply_fast_override(&mut requested.parameters, fast);
            }
        }
        None => {
            request.requested_model = Some(pb::RequestedModel {
                model_id: selection.model_id.clone(),
                parameters: selection_parameters(&selection.effort, selection.fast),
                ..Default::default()
            });
        }
    }
}

fn selection_parameters(
    effort: &EffortAction,
    fast: Option<bool>,
) -> Vec<pb::requested_model::ModelParameterValue> {
    let mut parameters = effort_parameters(effort.as_set());
    if let Some(fast) = fast {
        parameters.push(fast_parameter(fast));
    }
    parameters
}

fn effort_parameters(effort: Option<&str>) -> Vec<pb::requested_model::ModelParameterValue> {
    effort
        .map(|effort| {
            vec![pb::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: effort.into(),
            }]
        })
        .unwrap_or_default()
}

fn clear_effort_aliases(parameters: &mut Vec<pb::requested_model::ModelParameterValue>) {
    parameters.retain(|parameter| parameter.id != "effort" && parameter.id != "reasoning");
}

fn apply_effort_override(
    parameters: &mut Vec<pb::requested_model::ModelParameterValue>,
    effort: &str,
) {
    clear_effort_aliases(parameters);
    parameters.push(pb::requested_model::ModelParameterValue {
        id: "effort".into(),
        value: effort.into(),
    });
}

fn apply_fast_override(parameters: &mut Vec<pb::requested_model::ModelParameterValue>, fast: bool) {
    parameters.retain(|parameter| parameter.id != "fast");
    parameters.push(fast_parameter(fast));
}

fn fast_parameter(fast: bool) -> pb::requested_model::ModelParameterValue {
    pb::requested_model::ModelParameterValue {
        id: "fast".into(),
        value: if fast { "true" } else { "false" }.into(),
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
    fn rewrite_clears_source_metadata_and_does_not_copy_ui_parameters() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                max_mode: true,
                parameters: vec![pb::requested_model::ModelParameterValue {
                    id: "thinking".into(),
                    value: "true".into(),
                }],
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
        rewrite_requested_model(&mut request, &ModelRewrite::model_only("luna-hash"));
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(requested.model_id, "luna-hash");
        assert!(requested.parameters.is_empty());
        assert!(!requested.max_mode);
        assert!(request.model_details.is_none());
    }

    #[test]
    fn rewrite_same_model_effort_overrides_conflict_aliases_and_preserves_other_params() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "official-A".into(),
                max_mode: true,
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "reasoning".into(),
                        value: "medium".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "effort".into(),
                        value: "low".into(),
                    },
                ],
                ..Default::default()
            }),
            model_details: Some(pb::ModelDetails {
                model_id: "official-A".into(),
                display_name: "A".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "official-A".into(),
                effort: EffortAction::Set("high".into()),
                fast: None,
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert!(requested.max_mode);
        assert!(request.model_details.is_some());
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("fast", "true"), ("effort", "high")]
        );
        let model = from_requested(requested, None).unwrap();
        assert_eq!(model.reasoning.effort.as_deref(), Some("high"));
        assert_eq!(model.latency, ModelLatency::Fast);
    }

    #[test]
    fn rewrite_same_model_fast_only_overrides_request_true_with_default_false() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "official-A".into(),
                max_mode: true,
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "thinking".into(),
                        value: "true".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "effort".into(),
                        value: "low".into(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "official-A".into(),
                effort: EffortAction::Unchanged,
                fast: Some(false),
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert!(requested.max_mode);
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("thinking", "true"), ("effort", "low"), ("fast", "false")]
        );
        assert_eq!(
            from_requested(requested, None).unwrap().latency,
            ModelLatency::Standard
        );
    }

    #[test]
    fn rewrite_changed_model_with_effort_clears_source_and_sets_effort() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "official-A".into(),
                max_mode: true,
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "effort".into(),
                        value: "low".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                ],
                ..Default::default()
            }),
            model_details: Some(pb::ModelDetails {
                model_id: "official-A".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "official-B".into(),
                effort: EffortAction::Set("high".into()),
                fast: Some(false),
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(requested.model_id, "official-B");
        assert!(!requested.max_mode);
        assert!(request.model_details.is_none());
        assert_eq!(
            requested.parameters,
            vec![
                pb::requested_model::ModelParameterValue {
                    id: "effort".into(),
                    value: "high".into(),
                },
                pb::requested_model::ModelParameterValue {
                    id: "fast".into(),
                    value: "false".into(),
                },
            ]
        );
    }

    #[test]
    fn rewrite_changed_model_injects_target_effort_and_fast() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "official-A".into(),
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "thinking".into(),
                        value: "true".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "false".into(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "official-B".into(),
                effort: EffortAction::Set("high".into()),
                fast: Some(true),
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("effort", "high"), ("fast", "true")]
        );
        assert_eq!(
            from_requested(requested, None).unwrap().latency,
            ModelLatency::Fast
        );
    }

    #[test]
    fn rewrite_same_model_without_effort_or_fast_is_a_noop() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "official-A".into(),
                max_mode: true,
                parameters: vec![pb::requested_model::ModelParameterValue {
                    id: "effort".into(),
                    value: "low".into(),
                }],
                ..Default::default()
            }),
            model_details: Some(pb::ModelDetails {
                model_id: "official-A".into(),
                display_name: "A".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let before = request.clone();
        rewrite_requested_model(&mut request, &ModelRewrite::model_only("official-A"));
        assert_eq!(request, before);
    }

    #[test]
    fn rewrite_empty_requested_id_with_details_same_model_preserves_other_params() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "".into(),
                max_mode: true,
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "reasoning".into(),
                        value: "medium".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                ],
                ..Default::default()
            }),
            model_details: Some(pb::ModelDetails {
                model_id: "official-A".into(),
                display_name: "A".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "official-A".into(),
                effort: EffortAction::Set("high".into()),
                fast: None,
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(requested.model_id, "official-A");
        assert!(requested.max_mode);
        assert!(request.model_details.is_some());
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("fast", "true"), ("effort", "high")]
        );
    }

    #[test]
    fn rewrite_composer_clear_strips_effort_aliases_same_model() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "effort".into(),
                        value: "high".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "composer-2.5".into(),
                effort: EffortAction::Clear,
                fast: None,
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("fast", "true")]
        );
    }

    #[test]
    fn rewrite_composer_clear_with_forced_fast_false_overrides_request() {
        let mut request = pb::AgentRunRequest {
            requested_model: Some(pb::RequestedModel {
                model_id: "composer-2.5".into(),
                parameters: vec![
                    pb::requested_model::ModelParameterValue {
                        id: "effort".into(),
                        value: "high".into(),
                    },
                    pb::requested_model::ModelParameterValue {
                        id: "fast".into(),
                        value: "true".into(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        rewrite_requested_model(
            &mut request,
            &ModelRewrite {
                model_id: "composer-2.5".into(),
                effort: EffortAction::Clear,
                fast: Some(false),
            },
        );
        let requested = request.requested_model.as_ref().unwrap();
        assert_eq!(
            requested
                .parameters
                .iter()
                .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("fast", "false")]
        );
    }

    #[test]
    fn disabled_override_remains_disabled() {
        let request = pb::AgentRunRequest {
            subagent_model_overrides: vec![pb::SubagentModelOverride {
                subagent_type: "explore".into(),
                selection: Some(pb::subagent_model_override::Selection::Disabled(true)),
            }],
            ..Default::default()
        };
        assert!(matches!(
            overrides(&request).unwrap()[0].1,
            SubagentModelOverride::Disabled
        ));
    }
}
