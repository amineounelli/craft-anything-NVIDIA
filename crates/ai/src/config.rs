//! The `[ai]` table of `config.toml`: backends, roles, the fallback chain and price overrides.
//!
//! ```toml
//! [ai]
//! fallback_chain = ["Claude", "OpenRouter", "Local Llama"]
//!
//! [[ai.backends]]
//! name = "Claude"
//! kind = "anthropic"
//! model = "claude-opus-5-5"
//! effort = "high"
//! # Conversation trimming budget; does not set the server's context window.
//! context_tokens = 120000
//!
//! [ai.roles]
//! lead = "Claude"
//! worker = "Local Llama"
//!
//! [ai.prices."my-model"]
//! input_per_mtok = 1.0
//! output_per_mtok = 2.0
//! ```

use crate::model::Effort;
use crate::{BackendConfig, BackendKind, Role};
use craft_anything_engine::Settings;
use craft_anything_evidence::{ErrorCode, ToolError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Read settings strictly before a model-selection save; only a missing file uses the current session.
/// Malformed files and I/O errors are returned rather than replaced by default settings.
fn settings_for_selection(path: &Path, current: &Settings) -> Result<Settings, ToolError> {
    match std::fs::read_to_string(path) {
        Ok(text) => craft_anything_engine::settings::read_settings(&text).map_err(|e| {
            ToolError::new(ErrorCode::BadSettings, format!("The settings couldn't be read: {e}"))
                .with_remediation("Fix config.toml before changing the AI's model.")
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(current.clone()),
        Err(e) => Err(ToolError::io(path.display(), e)),
    }
}

/// Commit validated options through one path for hub and tool callers, which already hold the session.
/// Preserve unrelated disk changes, reject a changed backend or project, and update memory only after saving.
pub(crate) fn save_agent_selection(
    session: &mut craft_anything_engine::Session,
    path: &Path,
    context: &crate::ModelContext,
    options: BackendOptions,
) -> Result<bool, ToolError> {
    let current = AiConfig::from_table(&session.settings.ai)?;
    let name = &context.backend.config.name;
    let mut next = settings_for_selection(path, &session.settings)?;
    let mut config = AiConfig::from_table(&next.ai)?;
    let Some(entry) = config.backends.iter_mut().find(|e| &e.config.name == name) else { return Ok(false) };
    if current.get(name) != Some(&context.backend) || *entry != context.backend || session.project.as_ref().map(|p| p.root.clone()) != context.directory {
        return Err(ToolError::invalid("The AI settings changed while its models were loading. Reload the models and try again."));
    }
    entry.options = options;
    next.ai = config.to_table()?;
    next.save_to(path)?;
    session.settings = next;
    Ok(true)
}

/// Per-backend options beyond [`BackendConfig`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendOptions {
    /// Thinking depth (`low`/`medium`/`high`/`xhigh`/`max`) for the Claude API and Claude Code;
    /// `high` unless set. A model without the level gets the nearest one below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Anthropic server-side fallbacks (default on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks_enabled: Option<bool>,
    /// Force the ReAct text protocol (models without tool calling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub react_fallback: Option<bool>,
    /// Most model turns per user message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Input tokens before trimming and summarising the conversation. Omitted or zero uses the
    /// local/cloud default; this does not change the model server's own context window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Agent CLIs: the program to run instead of the default for `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Agent CLIs: extra arguments placed before the prompt.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Agent CLIs: the model to ask the CLI for (passed with its model flag when it has one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_model: Option<String>,
    /// OpenCode's model variant. Absent means the model's own default thinking depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_thinking: Option<Effort>,
    /// Copilot's context tier. Unset keeps the CLI's own choice; token limits depend on the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_context: Option<CliContext>,
    /// Copilot's full named reasoning range. Unset falls back to `effort` or the CLI default;
    /// `none` is an explicit choice, not the absence of an override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_reasoning_effort: Option<CliReasoningEffort>,
    /// Wall-clock limit in seconds for one agent CLI turn (default: 24 hours for a builder, which may
    /// analyse and build a big game; 30 minutes for questions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Agent CLIs that speak ACP (`cli_agents::acp::ACP_AGENTS`): chat over it, one agent process
    /// per conversation (on for Copilot, off for other agents unless set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp: Option<bool>,
    /// Agent CLIs that keep their sign-in, settings and sessions in a folder of their own (Claude
    /// Code's `CLAUDE_CONFIG_DIR`): the folder to use instead of their usual one, so each AI can be
    /// another account. `~/` is the home folder; over SSH it's a folder on that computer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl BackendOptions {
    /// [`config_dir`](BackendOptions::config_dir) as written, if any (for another computer).
    pub fn config_dir_text(&self) -> Option<&str> {
        self.config_dir.as_deref().map(str::trim).filter(|d| !d.is_empty())
    }

    /// [`config_dir`](BackendOptions::config_dir) on this computer, with `~` as the home folder.
    pub fn config_dir_here(&self) -> Option<std::path::PathBuf> {
        config_dir_in(self.config_dir_text()?, dirs::home_dir())
    }
}

/// `dir` with a leading `~` as `home`; `None` when it needs a home folder there isn't.
pub(crate) fn config_dir_in(dir: &str, home: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    if dir == "~" {
        return home;
    }
    match dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
        Some(rest) => home.map(|h| h.join(rest)),
        None => Some(std::path::PathBuf::from(dir)),
    }
}

/// A provider's context tier, not a guessed token count. Copilot reports each model's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CliContext {
    #[serde(rename = "default")]
    Standard,
    #[serde(rename = "long_context")]
    Long,
}

impl CliContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "default",
            Self::Long => "long_context",
        }
    }
}

/// The CLI's named reasoning levels; these are not numeric thinking-token budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CliReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl CliReasoningEffort {
    pub const ALL: &'static [Self] = &[Self::None, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// One configured backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendEntry {
    #[serde(flatten)]
    pub config: BackendConfig,
    #[serde(flatten)]
    pub options: BackendOptions,
}

impl BackendEntry {
    /// Older configurations can put OpenCode's variant directly in its model reference.
    fn thinking_from_model(&mut self) {
        if self.config.kind != BackendKind::Nvidia || self.config.model != "opencode" {
            return;
        }
        let Some((model, variant)) = self.options.cli_model.as_deref().and_then(|m| m.rsplit_once('#')) else { return };
        let Some(depth) = Effort::parse(variant) else { return };
        self.options.cli_thinking.get_or_insert(depth);
        self.options.cli_model = Some(model.to_string());
    }
    /// An agent CLI's `model` names the agent (`opencode`), and the model it should use goes in
    /// `cli_model`. People also write the agent's program in `command` and the model in `model`
    /// (`command = "opencode"`, `model = "deepseek/deepseek-flash"`); that ran the plain adapter
    /// with the agent's own default model (#34). Read that way, it's the agent the command names,
    /// asked for that model.
    fn model_from_command(&mut self) {
        let known = crate::cli_agents::KNOWN;
        let model = self.config.model.trim();
        let names_an_agent = known.iter().any(|(id, prog, display)| model == *id || model == *prog || model.eq_ignore_ascii_case(display));
        if self.config.kind != BackendKind::Nvidia || model.is_empty() || names_an_agent {
            return;
        }
        // The file name without `.exe`, whichever system's separators the path uses.
        let program = self.options.command.as_deref().and_then(|c| c.trim().rsplit(['/', '\\']).next()).map(|f| f.strip_suffix(".exe").unwrap_or(f));
        let Some((id, _, _)) = program.and_then(|p| known.iter().find(|(_, prog, _)| prog.eq_ignore_ascii_case(p))) else { return };
        if self.options.cli_model.is_none() {
            self.options.cli_model = Some(model.to_string());
        }
        self.config.model = id.to_string();
    }
}

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Cache reads; defaults to 10% of input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    /// Cache writes; defaults to 125% of input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
}

/// The whole AI configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Backends saved by another build (anthropic, OpenRouter, agent CLI…) can't run here. Unknown
    /// kinds are skipped with a warning instead of refusing to read the whole [ai] section.
    #[serde(deserialize_with = "tolerant_backends")]
    pub backends: Vec<BackendEntry>,
    /// Role (`lead`, `worker`, `reviewer`, `builder`) → backend name.
    pub roles: BTreeMap<String, String>,
    /// Backends to try in order when one fails before answering.
    pub fallback_chain: Vec<String>,
    /// Price overrides by model id.
    pub prices: BTreeMap<String, Price>,
    /// This installation's OpenCode Go session id, made on first use and kept with the settings so
    /// prompt caching survives restarts (and tests stay inside their own settings file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
}

pub fn role_key(r: Role) -> &'static str {
    match r {
        Role::Lead => "lead",
        Role::Worker => "worker",
        Role::Reviewer => "reviewer",
        Role::Builder => "builder",
        Role::Extractor => "extractor",
    }
}

/// Read the saved backends, keeping only the ones this build understands.
/// Legacy backends (anthropic, OpenRouter, agent CLI…) from another build are skipped with a
/// warning instead of bricking the whole [ai] section.
fn tolerant_backends<'de, D>(deserializer: D) -> Result<Vec<BackendEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values: Vec<toml::Value> = Deserialize::deserialize(deserializer)?;
    let mut out = Vec::new();
    for value in values {
        let kind = value
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("");
        if kind != "nvidia" {
            log::warn!(
                "skipping saved AI backend with unsupported kind {kind:?}; only NVIDIA backends exist in this build"
            );
            continue;
        }
        let text = toml::to_string(&value).map_err(|e| serde::de::Error::custom(e.to_string()))?;
        match toml::from_str::<BackendEntry>(&text) {
            Ok(entry) => out.push(entry),
            Err(e) => return Err(serde::de::Error::custom(format!("invalid NVIDIA backend: {e}"))),
        }
    }
    Ok(out)
}

impl AiConfig {
    /// Parse `settings.ai`. A malformed table gives an error the UI can show (never a panic).
    pub fn from_table(t: &toml::Table) -> Result<AiConfig, ToolError> {
        let mut c: AiConfig = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| {
            ToolError::new(ErrorCode::BadSettings, format!("The [ai] settings couldn't be read: {e}"))
                .with_remediation("Fix or remove the [ai] section in config.toml.")
        })?;
        for backend in &mut c.backends {
            backend.model_from_command();
            backend.thinking_from_model();
        }
        Ok(c)
    }

    /// Parse from settings; unreadable config is logged and treated as empty.
    pub fn from_settings(s: &Settings) -> AiConfig {
        AiConfig::from_table(&s.ai).unwrap_or_else(|e| {
            log::warn!("{}", e.message);
            AiConfig::default()
        })
    }

    pub fn to_table(&self) -> Result<toml::Table, ToolError> {
        toml::Table::try_from(self).map_err(|e| ToolError::internal(format!("saving AI settings: {e}")))
    }

    pub fn get(&self, name: &str) -> Option<&BackendEntry> {
        self.backends.iter().find(|b| b.config.name == name)
    }

    pub fn enabled(&self) -> impl Iterator<Item = &BackendEntry> {
        self.backends.iter().filter(|b| b.config.enabled)
    }

    /// Add or replace a backend by name.
    pub fn upsert(&mut self, e: BackendEntry) {
        match self.backends.iter_mut().find(|b| b.config.name == e.config.name) {
            Some(slot) => *slot = e,
            None => self.backends.push(e),
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.backends.len();
        self.backends.retain(|b| b.config.name != name);
        self.roles.retain(|_, v| v != name);
        self.fallback_chain.retain(|v| v != name);
        before != self.backends.len()
    }

    /// The backend for a role, with the plan's defaults: lead = first enabled backend; worker =
    /// a local model if present, else lead; reviewer and extractor = only when set; builder = an
    /// agent CLI if present, else lead.
    pub fn backend_for(&self, role: Role) -> Option<&BackendEntry> {
        if let Some(b) = self.roles.get(role_key(role)).and_then(|n| self.get(n)).filter(|b| b.config.enabled) {
            return Some(b);
        }
        let lead = || self.enabled().find(|b| b.config.kind != BackendKind::Nvidia).or_else(|| self.enabled().next());
        match role {
            Role::Lead => lead(),
            Role::Worker => self.enabled().find(|b| b.config.kind == BackendKind::Nvidia).or_else(lead),
            Role::Reviewer | Role::Extractor => None,
            Role::Builder => self.enabled().find(|b| b.config.kind == BackendKind::Nvidia).or_else(lead),
        }
    }

    /// The chain to try for a conversation started on `first`: `first`, then the configured
    /// fallback chain (skipping duplicates and disabled backends).
    pub fn chain_from(&self, first: &str) -> Vec<String> {
        let mut out = vec![first.to_string()];
        for n in &self.fallback_chain {
            if !out.contains(n) && self.get(n).is_some_and(|b| b.config.enabled) {
                out.push(n.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_default_builder_is_the_same_from_the_list_as_from_the_config() {
        use crate::{BackendConfig, BackendKind, Role, default_builder};
        let b = |name: &str, kind, enabled| BackendConfig { name: name.into(), kind, model: String::new(), base_url: None, ssh_host: None, enabled };
        let cases: Vec<(Vec<BackendConfig>, Option<&str>)> = vec![
            (vec![], None),
            (vec![b("Key", BackendKind::Nvidia, true), b("Claude Code", BackendKind::Nvidia, true)], None),
            (vec![b("Key", BackendKind::Nvidia, true), b("Claude Code", BackendKind::Nvidia, false)], None),
            (vec![b("Key", BackendKind::Nvidia, true), b("Claude Code", BackendKind::Nvidia, true)], Some("Key")),
            (vec![b("Key", BackendKind::Nvidia, false), b("Claude Code", BackendKind::Nvidia, true)], Some("Key")),
            (vec![b("Local", BackendKind::Nvidia, true), b("Key", BackendKind::Nvidia, true)], None),
        ];
        for (list, chosen) in cases {
            let mut cfg = AiConfig::default();
            for c in &list {
                cfg.upsert(crate::BackendEntry { config: c.clone(), options: Default::default() });
            }
            if let Some(n) = chosen {
                cfg.roles.insert(role_key(Role::Builder).into(), n.into());
            }
            let from_cfg = cfg.backend_for(Role::Builder).map(|e| e.config.name.clone());
            assert_eq!(default_builder(&list, chosen).map(|c| c.name.clone()), from_cfg, "{list:?} {chosen:?}");
        }
    }

    use super::*;

    fn entry(name: &str, kind: BackendKind) -> BackendEntry {
        BackendEntry {
            config: BackendConfig { name: name.into(), kind, model: String::new(), base_url: None, ssh_host: None, enabled: true },
            options: BackendOptions::default(),
        }
    }

    #[test]
    fn round_trip_through_settings_toml() {
        let mut c = AiConfig::default();
        let mut a = entry("Claude", BackendKind::Nvidia);
        a.options.effort = Some(Effort::High);
        a.options.fallbacks_enabled = Some(false);
        a.options.context_tokens = Some(120_000);
        c.upsert(a);
        c.upsert(entry("Local", BackendKind::Nvidia));
        c.roles.insert("reviewer".into(), "Local".into());
        c.fallback_chain = vec!["Local".into()];
        c.prices.insert("m".into(), Price { input_per_mtok: 1.0, output_per_mtok: 2.0, cache_read_per_mtok: None, cache_write_per_mtok: None });
        let mut s = Settings { ai: c.to_table().unwrap(), ..Default::default() };
        let text = toml::to_string_pretty(&s).unwrap();
        s = toml::from_str(&text).unwrap();
        let back = AiConfig::from_settings(&s);
        assert_eq!(back, c);
        assert!(text.contains("[[ai.backends]]"), "{text}");
        assert!(text.contains("effort = \"high\""), "{text}");
        assert!(text.contains("context_tokens = 120000"), "{text}");
    }

    #[test]
    fn backend_context_budget_is_optional_and_validated() {
        let table = |line: &str| -> toml::Table {
            toml::from_str(&format!("[[backends]]\nname = \"Local\"\nkind = \"nvidia\"\nmodel = \"nvidia/nemotron-4-340b-instruct\"\n{line}")).unwrap()
        };
        for (line, expected) in [
            ("", None),
            ("context_tokens = 0", Some(0)),
            ("context_tokens = 8000", Some(8000)),
            ("context_tokens = 9223372036854775807", Some(i64::MAX as u64)),
        ] {
            let config = AiConfig::from_table(&table(line)).unwrap();
            assert_eq!(config.backends[0].options.context_tokens, expected);
            assert_eq!(AiConfig::from_table(&config.to_table().unwrap()).unwrap(), config);
        }
        for line in ["context_tokens = -1", "context_tokens = \"8000\"", "context_tokens = 8.5"] {
            assert_eq!(AiConfig::from_table(&table(line)).unwrap_err().code, ErrorCode::BadSettings);
        }
    }

    #[test]
    fn roles_and_chains() {
        let mut c = AiConfig::default();
        c.upsert(entry("Codex", BackendKind::Nvidia));
        c.upsert(entry("Claude", BackendKind::Nvidia));
        c.upsert(entry("Local", BackendKind::Nvidia));
        // All backends are Nvidia: Lead falls back to first enabled,
        // Worker and Builder pick first Nvidia (= first enabled).
        let first = c.enabled().next().unwrap().config.name.clone();
        assert_eq!(c.backend_for(Role::Lead).unwrap().config.name, first);
        assert_eq!(c.backend_for(Role::Worker).unwrap().config.name, first);
        assert_eq!(c.backend_for(Role::Builder).unwrap().config.name, first);
        assert!(c.backend_for(Role::Reviewer).is_none());
        assert!(c.backend_for(Role::Extractor).is_none(), "the extractor only runs when chosen");
        c.roles.insert("lead".into(), "Local".into());
        assert_eq!(c.backend_for(Role::Lead).unwrap().config.name, "Local");
        c.fallback_chain = vec!["Claude".into(), "Local".into(), "Gone".into()];
        assert_eq!(c.chain_from("Local"), vec!["Local".to_string(), "Claude".to_string()]);
        assert!(c.remove("Local"));
        assert!(c.roles.is_empty());
        assert!(!c.remove("Local"));
    }

    #[test]
    fn unknown_backend_kind_is_skipped_instead_of_rejected() {
        let t: toml::Table = toml::from_str(
            r#"
backends = [
  { name = "Old", kind = "agent_cli", model = "opencode" },
  { name = "NIM", kind = "nvidia", model = "nvidia/nemotron-4-340b-instruct", base_url = "https://integrate.api.nvidia.com/v1" },
]
"#,
        )
        .unwrap();
        let config = AiConfig::from_table(&t).unwrap();
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].config.name, "NIM");
        assert_eq!(config.backends[0].config.kind, BackendKind::Nvidia);
    }

    #[test]
    fn malformed_table_is_an_error_not_a_panic() {
        let t: toml::Table = toml::from_str("backends = 3").unwrap();
        assert_eq!(AiConfig::from_table(&t).unwrap_err().code, ErrorCode::BadSettings);
        let s = Settings { ai: t, ..Default::default() };
        assert!(AiConfig::from_settings(&s).backends.is_empty());
    }
}
