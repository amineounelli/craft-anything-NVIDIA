//! The same agent model catalogue and validated selection for CLI/MCP clients.

use crate::{AiConfig, Effort, ModelContext, cli_agents};
use craft_anything_engine::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::ToolError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

/// Register headless catalogue and selection tools, including the CLI's possible process, network and cache effects.
pub fn specs() -> Vec<ToolSpec> {
    let effects = Effects::LAUNCHES_PROCESS.union(Effects::ACCESSES_NETWORK).union(Effects::WRITES_FILESYSTEM);
    vec![
        ToolSpec {
            id: "ai_models_list",
            title: "List an agent's models",
            summary: "List the visible chat models and supported thinking depths of a configured catalogue-capable coding agent. Uses the selected installation's configuration and credentials; only OpenCode supports SSH discovery. Returns a total and a page (offset, limit: default 200, maximum 1000). Discovery may start a process and update the CLI cache.",
            beginner: "Shows which models this agent can use.",
            toolset: Toolset::Setup,
            effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<ListParams>,
            run: ai_models_list,
        },
        ToolSpec {
            id: "ai_model_set",
            title: "Choose an agent's model and thinking",
            summary: "Save a configured coding agent's model and optional thinking depth, validated against that instance's current model catalogue. Omit model and thinking to use the agent's default; omit thinking to use the selected model's default. Unsupported selections are refused.",
            beginner: "Chooses the model and how deeply it thinks.",
            toolset: Toolset::Setup,
            effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<SetParams>,
            run: ai_model_set,
        },
    ]
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListParams {
    /// Configured backend name.
    name: String,
    #[serde(default)]
    offset: usize,
    /// Page size (default 200, at most 1000).
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetParams {
    name: String,
    model: Option<String>,
    thinking: Option<Effort>,
}

/// Capture a catalogue-capable backend from the tool's already-held session without locking it again.
fn context(ctx: &Ctx<'_>, name: &str) -> Result<ModelContext, ToolError> {
    let config = AiConfig::from_table(&ctx.session.settings.ai)?;
    let backend = config.get(name).cloned().ok_or_else(|| ToolError::invalid(format!("No AI backend named {name:?}.")))?;
    if backend.config.kind != crate::BackendKind::Nvidia || !cli_agents::adapter_for(&backend.config.model).has_model_catalog() {
        return Err(ToolError::invalid("Choose an agent backend with a live model catalogue."));
    }
    Ok(ModelContext { backend, directory: ctx.session.project.as_ref().map(|p| p.root.clone()) })
}

/// Discover models with tool-linked cancellation and return a bounded page alongside the total count.
fn ai_models_list(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ListParams = parse_params(params)?;
    let c = context(ctx, &p.name)?;
    let stop = crate::model::Stop::for_tool(ctx.cancel.clone());
    let models = cli_agents::adapter_for(&c.backend.config.model).model_catalog(&c.backend.config, &c.backend.options, c.directory.as_deref(), &stop)?;
    let total = models.len();
    let page: Vec<_> = models.into_iter().skip(p.offset).take(p.limit.unwrap_or(200).min(1000)).collect();
    Ok(ToolOutput::Value(json!({"name": p.name, "total": total, "offset": p.offset, "models": page})))
}

/// Validate and persist a model/depth selection, updating session settings only after the save succeeds.
fn ai_model_set(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: SetParams = parse_params(params)?;
    let c = context(ctx, &p.name)?;
    let options = cli_agents::models::select_with_stop(&c, p.model.as_deref(), p.thinking, &crate::model::Stop::for_tool(ctx.cancel.clone()))?;
    let result = json!({"name": p.name, "model": options.cli_model, "thinking": options.cli_thinking});
    let path = ctx.session.config_dir().join("config.toml");
    if !crate::config::save_agent_selection(ctx.session, &path, &c, options)? {
        return Err(ToolError::invalid("The AI backend was removed while its models were loading. Reload the settings and try again."));
    }
    Ok(ToolOutput::Value(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_anything_engine::{CallOptions, Caller, Engine};

    /// Build hub and tool clients sharing an isolated settings file and an explicit saved selection.
    fn selection_clients() -> (crate::AiHub, std::path::PathBuf) {
        let mut session = craft_anything_engine::testing::session();
        let path = session.config_dir().join("config.toml");
        let mut config = AiConfig::default();
        config.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "OpenCode".into(),
                kind: crate::BackendKind::Nvidia,
                model: "opencode".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            options: crate::BackendOptions { cli_model: Some("test/old".into()), ..Default::default() },
        });
        session.settings.ai = config.to_table().unwrap();
        session.settings.save_to(&path).unwrap();
        let hub = crate::AiHub::with_stores(Engine::shared(session, specs()), path.clone(), crate::KeyStore::file_only(path.with_file_name("ai-keys.toml")));
        (hub, path)
    }

    /// Reset through either public entry point so both exercise the shared persistence contract.
    fn reset_selection(hub: &crate::AiHub, tool: bool) -> Result<(), ToolError> {
        if tool {
            hub.engine.call("ai_model_set", json!({"name":"OpenCode"}), CallOptions::new(Caller::Cli)).map(|_| ())
        } else {
            hub.set_agent_model("OpenCode", None, None).and_then(|found| if found { Ok(()) } else { Err(ToolError::invalid("Backend was removed.")) })
        }
    }

    /// Both clients merge unrelated disk edits instead of replacing them with their older session snapshot.
    #[test]
    fn selection_saves_preserve_external_settings_and_other_backends() {
        // `tool = false` goes through `set_agent_model`, which is for agent CLIs and always
        // fails in the NVIDIA-only build. Only the engine-tool path applies here.
        let tool = true;
        let (hub, path) = selection_clients();
        let mut disk = hub.engine.session().settings.clone();
        disk.language = "zh-hans".into();
        let mut config = AiConfig::from_table(&disk.ai).unwrap();
        let mut other = config.get("OpenCode").unwrap().clone();
        other.config.name = "Other".into();
        other.options.timeout_secs = Some(123);
        config.upsert(other.clone());
        config.roles.insert("builder".into(), "Other".into());
        disk.ai = config.to_table().unwrap();
        disk.save_to(&path).unwrap();
        reset_selection(&hub, tool).unwrap();
        let saved = craft_anything_engine::Settings::load_from(&path);
        let config = AiConfig::from_table(&saved.ai).unwrap();
        assert_eq!(saved.language, "zh-hans");
        assert_eq!(config.get("Other"), Some(&other));
        assert_eq!(config.roles.get("builder").map(String::as_str), Some("Other"));
        assert!(config.get("OpenCode").unwrap().options.cli_model.is_none());
        assert_eq!(hub.engine.session().settings, saved);
    }

    /// Syntax errors, invalid AI tables and future versions remain byte-for-byte intact on refused saves.
    #[test]
    fn selection_saves_refuse_damaged_or_newer_disk_settings() {
        // `tool = false` goes through `set_agent_model`, which is for agent CLIs and always
        // fails in the NVIDIA-only build. Only the engine-tool path applies here.
        let tool = true;
        for damaged in [Some("not valid = ["), Some("[ai]\nbackends = 123\n"), None] {
            let (hub, path) = selection_clients();
            let before = hub.engine.session().settings.clone();
            let text = damaged.map(str::to_string).unwrap_or_else(|| {
                let mut newer = before.clone();
                newer.version = u32::MAX;
                toml::to_string(&newer).unwrap()
            });
            std::fs::write(&path, &text).unwrap();
            assert_eq!(reset_selection(&hub, tool).unwrap_err().code, craft_anything_evidence::ErrorCode::BadSettings);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            assert_eq!(hub.engine.session().settings, before);
        }
    }

    /// A backend changed or removed on disk cannot be restored or overwritten by a stale selection.
    #[test]
    fn selection_saves_reject_external_changes_to_the_selected_backend() {
        for tool in [false, true] {
            for removed in [false, true] {
                let (hub, path) = selection_clients();
                let before = hub.engine.session().settings.clone();
                let mut disk = before.clone();
                let mut config = AiConfig::from_table(&disk.ai).unwrap();
                if removed {
                    config.remove("OpenCode");
                } else {
                    let mut entry = config.get("OpenCode").unwrap().clone();
                    entry.options.command = Some("different-agent".into());
                    config.upsert(entry);
                }
                disk.ai = config.to_table().unwrap();
                disk.save_to(&path).unwrap();
                let bytes = std::fs::read(&path).unwrap();
                assert!(reset_selection(&hub, tool).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert_eq!(hub.engine.session().settings, before);
            }
        }
    }

    /// Engine dispatch rejects invalid requests and persists default selection without a running agent.
    #[test]
    fn model_tools_are_headless_and_default_selection_persists_in_the_session_store() {
        let mut session = craft_anything_engine::testing::session();
        let path = session.config_dir().join("config.toml");
        let mut cfg = AiConfig::default();
        cfg.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "OpenCode".into(),
                kind: crate::BackendKind::Nvidia,
                model: "opencode".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            options: crate::BackendOptions {
                command: Some("missing-test-agent".into()),
                cli_model: Some("test/old".into()),
                cli_thinking: Some(Effort::High),
                ..Default::default()
            },
        });
        session.settings.ai = cfg.to_table().unwrap();
        let engine = Engine::shared(session, specs());
        let call = |id, params| engine.call(id, params, CallOptions::new(Caller::Cli));
        assert!(call("ai_models_list", json!({"name":"missing"})).is_err());
        assert!(call("ai_model_set", json!({"name":"OpenCode","thinking":"unsupported"})).is_err());
        call("ai_model_set", json!({"name":"OpenCode"})).unwrap();
        let saved = AiConfig::from_settings(&craft_anything_engine::Settings::load_from(&path));
        assert!(saved.get("OpenCode").unwrap().options.cli_model.is_none());
        assert!(saved.get("OpenCode").unwrap().options.cli_thinking.is_none());
    }
}
