//! Model-specific CLI tuning. Reasoning choices come from the selected model's ACP metadata;
//! context uses the CLI's named tiers when it does not advertise numeric window limits.

use crate::config::{CliContext, CliReasoningEffort};
use crate::{AiConfig, BackendConfig, BackendKind};
use craft_anything_engine::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A cheap capability query for the settings UI; model discovery itself runs on a worker.
pub fn supported(config: &BackendConfig) -> bool {
    config.kind == BackendKind::Nvidia && crate::cli_agents::adapter_for(&config.model).supports_model_tuning()
}

/// Persistable choices, with the selected backend model captured to reject a stale picker.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetModelOptions {
    pub backend: String,
    /// The backend's cli_model when these choices were loaded; null means the CLI default.
    pub expected_cli_model: Option<String>,
    /// Null inherits the CLI's context choice.
    pub context: Option<CliContext>,
}

pub(crate) fn update(config: &mut AiConfig, params: &SetModelOptions) -> Result<(), ToolError> {
    let entry = config.backends.iter_mut().find(|entry| entry.config.name == params.backend).ok_or_else(|| ToolError::invalid("No such AI backend."))?;
    if !supported(&entry.config) {
        return Err(ToolError::invalid("This backend does not offer CLI model options."));
    }
    if entry.options.cli_model != params.expected_cli_model {
        return Err(ToolError::invalid("The model changed while its options were being edited.").with_remediation("Reload the model options and choose again."));
    }
    entry.options.cli_context = params.context;
    Ok(())
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShowModelOptions {
    backend: String,
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "ai_model_options_show",
            title: "Show model options",
            summary: "Discover the selected Copilot model's context tiers without sending a model prompt. Returns cli_model for the expected_cli_model guard when saving. Use ai_models_list for model and thinking choices.",
            beginner: "See this model's context choices.",
            toolset: Toolset::Setup,
            effects: Effects::READ_ONLY.union(Effects::LAUNCHES_PROCESS),
            requires: Requires::Nothing,
            input_schema: schema_of::<ShowModelOptions>,
            run: show,
        },
        ToolSpec {
            id: "ai_model_options_set",
            title: "Set model options",
            summary: "Save a Copilot backend's context tier. Null inherits the CLI default. Refuses a stale expected_cli_model and leaves thinking unchanged; use ai_model_set for model and thinking choices.",
            beginner: "Choose context for this model.",
            toolset: Toolset::Setup,
            effects: Effects::MUTATES_SESSION.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<SetModelOptions>,
            run: set,
        },
    ]
}

fn show(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let params: ShowModelOptions = parse_params(params)?;
    let config = AiConfig::from_table(&ctx.session.settings.ai)?;
    let entry = config.get(&params.backend).ok_or_else(|| ToolError::invalid("No such AI backend."))?;
    if !supported(&entry.config) {
        return Err(ToolError::invalid("This backend does not offer CLI model options."));
    }
    let stop = crate::model::Stop::for_tool(ctx.cancel.clone());
    let tuning = crate::cli_agents::acp_model_tuning(&entry.config, &entry.options, &stop)?;
    Ok(ToolOutput::Value(
        serde_json::json!({"backend": params.backend, "cli_model": entry.options.cli_model, "choices": tuning, "context": entry.options.cli_context, "reasoning": entry.options.cli_reasoning_effort}),
    ))
}

fn set(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let params: SetModelOptions = parse_params(params)?;
    let mut config = AiConfig::from_table(&ctx.session.settings.ai)?;
    update(&mut config, &params)?;
    let mut settings = ctx.session.settings.clone();
    settings.ai = config.to_table()?;
    settings.save_to(&ctx.session.config_dir().join("config.toml"))?;
    ctx.session.settings = settings;
    Ok(ToolOutput::Value(serde_json::json!({"backend": params.backend, "context": params.context})))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelOption<T> {
    pub value: T,
    pub label: String,
    pub description: Option<String>,
}

/// Choices for one selected model. A default is separate from an explicit `none` effort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelTuning {
    pub model: String,
    pub context: Vec<ModelOption<CliContext>>,
    pub reasoning: Vec<ModelOption<CliReasoningEffort>>,
}

impl ModelTuning {
    pub(crate) fn from_metadata(model: String, options: &[Value]) -> Self {
        fn choices<T: serde::de::DeserializeOwned>(options: &[Value], ids: &[&str]) -> Vec<ModelOption<T>> {
            options
                .iter()
                .find(|option| option.get("id").and_then(Value::as_str).is_some_and(|id| ids.contains(&id)))
                .and_then(|option| option.get("options"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let value = serde_json::from_value(option.get("value")?.clone()).ok()?;
                    let label = option.get("name").and_then(Value::as_str)?.to_string();
                    Some(ModelOption { value, label, description: option.get("description").and_then(Value::as_str).map(str::to_string) })
                })
                .collect()
        }
        let mut context = choices(options, &["context", "context_window", "context_tier"]);
        if context.is_empty() {
            context = vec![
                ModelOption { value: CliContext::Standard, label: "Standard".into(), description: None },
                ModelOption { value: CliContext::Long, label: "Extended".into(), description: None },
            ];
        }
        Self { model, context, reasoning: choices(options, &["reasoning_effort"]) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cancelled_context_discovery_stops_before_starting_a_cli() {
        let cancel = craft_anything_engine::CancelToken::default();
        cancel.cancel();
        let stop = crate::model::Stop::for_tool(cancel);
        let config =
            BackendConfig { name: "Test Copilot".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: None, enabled: true };
        let options = crate::BackendOptions { command: Some("craft-anything-no-such-test-agent".into()), ..Default::default() };
        assert_eq!(crate::cli_agents::acp_model_tuning(&config, &options, &stop).unwrap_err().code, craft_anything_evidence::ErrorCode::Cancelled);
    }

    #[test]
    fn model_choices_preserve_advertised_windows_and_exclude_unsupported_efforts() {
        let options = vec![
            json!({"id":"context_window", "options":[{"value":"default", "name":"272k"},{"value":"long_context", "name":"1M"}]}),
            json!({"id":"reasoning_effort", "options":[{"value":"none", "name":"None"},{"value":"high", "name":"High"},{"value":"unsupported", "name":"Unknown"}]}),
        ];
        let tuning = ModelTuning::from_metadata("test-model".into(), &options);
        assert_eq!(tuning.context[0].label, "272k");
        assert_eq!(tuning.context[1].value, CliContext::Long);
        assert_eq!(tuning.reasoning.iter().map(|option| option.value).collect::<Vec<_>>(), [CliReasoningEffort::None, CliReasoningEffort::High]);
        let fallback = ModelTuning::from_metadata("other-model".into(), &[]);
        assert_eq!(fallback.context[0].label, "Standard");
        assert!(fallback.reasoning.is_empty());
    }

    #[test]
    fn context_tools_preserve_shared_thinking_and_refuse_stale_model_edits() {
        use crate::{BackendEntry, BackendOptions};
        use craft_anything_engine::{CallOptions, Caller, Engine, Session};
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::isolated(directory.path());
        let mut config = AiConfig::default();
        config.upsert(BackendEntry {
            config: BackendConfig { name: "Copilot".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: None, enabled: true },
            options: BackendOptions { cli_model: Some("test-model".into()), cli_thinking: Some(crate::Effort::None), ..Default::default() },
        });
        session.settings.ai = config.to_table().unwrap();
        let engine = Engine::shared(session, specs());
        let params = json!({"backend":"Copilot", "expected_cli_model":"test-model", "context":"long_context"});
        engine.call("ai_model_options_set", params, CallOptions::new(Caller::Agent)).unwrap();
        let saved = AiConfig::from_settings(&engine.session().settings);
        assert_eq!(saved.get("Copilot").unwrap().options.cli_thinking, Some(crate::Effort::None));
        assert_eq!(saved.get("Copilot").unwrap().options.cli_context, Some(CliContext::Long));
        let mut stale = SetModelOptions { backend: "Copilot".into(), expected_cli_model: Some("old-model".into()), context: None };
        let mut saved = saved;
        assert!(update(&mut saved, &stale).is_err());
        stale.expected_cli_model = Some("test-model".into());
        update(&mut saved, &stale).unwrap();
        assert_eq!(saved.get("Copilot").unwrap().options.cli_thinking, Some(crate::Effort::None));
    }
}
