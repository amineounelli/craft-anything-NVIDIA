//! Agent CLIs (Claude Code, Codex, Gemini CLI, OpenCode, Hermes…) run as owned subprocesses with
//! our MCP server, optionally on another machine over ssh. Their JSON event streams become
//! [`AgentEvent`]s.
//!
//! How each CLI is driven lives in [`adapters`] ([`AgentAdapter`], registered in
//! [`AgentAdapters`]). Flags: Claude Code's were checked against `claude --help`. For the others,
//! the flags we rely on are checked at run time against `<cli> --help`; when they're missing we
//! fall back to the plain adapter (prompt as an argument, stdout streamed as text) and say so,
//! unless the CLI's plain form is interactive (Codex), which keeps its adapter.

pub mod acp;
pub mod adapters;
pub mod models;
mod shared;

// SharedConversation is dead code in the NVIDIA-only fork (agent CLI code paths are not active).

pub use acp::AcpConversation;
pub use adapters::{AgentAdapter, AgentAdapters, ArgsCx};

use crate::config::BackendOptions;
use crate::model::Stop;
use crate::{AgentEvent, BackendConfig, Conversation, FailureKind, TurnFailure};
use craft_anything_engine::CancelToken;
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::process::Process;
use craft_anything_platform::sync::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// Name our MCP server is registered under.
pub const MCP_NAME: &str = "craft-anything";

/// Known agent CLIs: (agent id, program, display name).
pub const KNOWN: &[(&str, &str, &str)] = &[
    ("claude-code", "claude", "Claude Code"),
    ("codex", "codex", "Codex"),
    ("copilot", "copilot", "GitHub Copilot"),
    ("gemini-cli", "gemini", "Gemini CLI"),
    ("antigravity", "agy", "Antigravity"),
    ("opencode", "opencode", "OpenCode"),
    ("hermes", "hermes", "Hermes Agent"),
    ("qwen-code", "qwen", "Qwen Code"),
    ("goose", "goose", "Goose"),
    ("maki", "maki", "Maki"),
];

pub fn program_for(agent: &str) -> String {
    KNOWN.iter().find(|(id, _, _)| *id == agent).map(|(_, p, _)| p.to_string()).unwrap_or_else(|| agent.to_string())
}

/// The program a backend runs: the one chosen for it, else the agent's usual name. A chosen path
/// that's gone from this computer is looked for again (#721): an agent that installs each version
/// in a folder of its own (Codex's `bin/<build>/codex.exe`) moves on every update, so the same file
/// in the newest folder beside the old one is used, else the usual name on PATH. On another
/// computer (ssh) the chosen command is used as it is.
pub fn program_of(config: &BackendConfig, options: &BackendOptions) -> String {
    let usual = program_for(&config.model);
    let Some(chosen) = options.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) else { return usual };
    let local = config.ssh_host.as_deref().is_none_or(|h| h.trim().is_empty());
    // A folder or app bundle saved before set_command refused it (#467) can't be started: the usual
    // program is used, as when none is chosen.
    if local && Path::new(chosen).is_dir() {
        return usual;
    }
    let path = Path::new(chosen);
    if !local || path.parent().is_none_or(|p| p.as_os_str().is_empty()) || path.exists() {
        return chosen.to_string();
    }
    moved_program(path).or_else(|| craft_anything_platform::which(&usual).map(|_| usual)).unwrap_or_else(|| chosen.to_string())
}

/// The same file in the most recently changed sibling of its missing folder, if there is one.
fn moved_program(missing: &Path) -> Option<String> {
    let file = missing.file_name()?;
    let versions = missing.parent()?.parent()?;
    std::fs::read_dir(versions)
        .ok()?
        .flatten()
        .take(256)
        .map(|e| e.path().join(file))
        .filter(|p| p.is_file())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
        .map(|p| p.display().to_string())
}

pub fn display_for(agent: &str) -> String {
    KNOWN.iter().find(|(id, _, _)| *id == agent).map(|(_, _, d)| d.to_string()).unwrap_or_else(|| agent.to_string())
}

/// The built-in adapter for an agent id (the plain one for agents without their own).
pub fn adapter_for(agent: &str) -> Arc<dyn AgentAdapter> {
    AgentAdapters::built_in().for_agent(agent)
}

/// The MCP server command handed to the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCommand {
    pub command: String,
    pub args: Vec<String>,
    /// Where the person's notes wait for a builder on this computer (`rebuild::notes`), so an
    /// agent with tool hooks can hand them over at its next step. None for remote runs.
    pub notes: Option<PathBuf>,
}

impl McpCommand {
    /// Our installed CLI (`craft-anything-cli mcp [--project <dir>] [--inbox <dir>]`). Remote runs
    /// use the name on the remote PATH and have no local project or inbox.
    pub fn local_or_remote(remote: bool, project: Option<PathBuf>, inbox: Option<PathBuf>) -> McpCommand {
        let command = if remote {
            "craft-anything-cli".to_string()
        } else {
            craft_anything_platform::self_cli_path().map(|p| p.display().to_string()).unwrap_or_else(|| "craft-anything-cli".to_string())
        };
        let mut args = vec!["mcp".to_string()];
        if let Some(p) = project.filter(|_| !remote) {
            args.push("--project".into());
            args.push(p.display().to_string());
        }
        let notes = inbox.as_deref().filter(|_| !remote).map(crate::rebuild::notes::notes_dir);
        if let Some(i) = inbox.filter(|_| !remote) {
            args.push("--inbox".into());
            args.push(i.display().to_string());
        }
        McpCommand { command, args, notes }
    }
}

/// Whether the `craft-anything-cli` a builder's tool server runs from (None: there isn't one) can
/// start. Without it Claude Code stops at its first approval with a stack trace (#91). A check
/// that ran out of time says nothing about the program, so the build goes ahead. Stopping the
/// build ends the check, as `ErrorCode::Cancelled`.
pub fn cli_can_start(cli: Option<&Path>, stop: &CancelToken) -> Result<(), ToolError> {
    const REINSTALL: &str = "Install the latest Craft Anything again: it keeps this helper next to the app. If you built the app yourself, run `cargo build -p craft-anything-cli` too.";
    let Some(cli) = cli else {
        return Err(ToolError::new(ErrorCode::EngineMissing, "Craft Anything's helper program (craft-anything-cli) is missing, so the builder can't start.")
            .with_remediation(REINSTALL));
    };
    let failed = |why: String| {
        ToolError::new(ErrorCode::EngineMissing, format!("Craft Anything's helper program at {} wouldn't start: {why}", cli.display()))
            .with_remediation(REINSTALL)
    };
    let stop = stop.clone();
    match Process::new(cli.as_os_str()).arg("--version").timeout(Duration::from_secs(30)).stdout_cap(64 * 1024).cancel_when(move || stop.is_cancelled()).run() {
        Ok(out) if out.success => Ok(()),
        Ok(out) => Err(failed(out.code.map_or_else(|| "it was stopped by the system".to_string(), |c| format!("it exited with code {c}")))),
        Err(e) if e.code == ErrorCode::TimedOut => Ok(()),
        Err(e) if e.code == ErrorCode::Cancelled => Err(e),
        Err(e) => Err(failed(e.message)),
    }
}

/// Whether a local build by `agent` can have our tool server: over ACP every agent is handed it,
/// else its adapter says.
pub fn check_builder_tools(adapter: &dyn AgentAdapter, agent: &str, options: &BackendOptions, stop: &CancelToken) -> Result<(), ToolError> {
    if selected_acp_args(adapter, agent, options).is_some() {
        return cli_can_start(craft_anything_platform::self_cli_path().as_deref(), stop);
    }
    adapter.check_native_options(options)?;
    adapter.check_tool_server(stop)
}

/// The requested transport, shared by builder preflight and execution. ACP cannot carry explicit
/// catalogue model/variant selections yet, so those must pass the native adapter's checks.
fn selected_acp_args(adapter: &dyn AgentAdapter, agent: &str, options: &BackendOptions) -> Option<&'static [&'static str]> {
    if adapter.has_model_catalog() && !adapter.requires_acp() && (options.cli_model.is_some() || options.cli_thinking.is_some()) {
        return None;
    }
    acp::acp_args(agent).filter(|_| options.acp.unwrap_or_else(|| adapter.requires_acp()))
}

fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The permission-prompt tool a builder run routes its approvals through.
pub const APPROVAL_TOOL: &str = craft_anything_engine::tools::interaction_tools::APPROVE_TOOL;

/// What an agent run may do with files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Only analyse: no edits.
    Analyse,
    /// The same in Clean room (#564): the agent's own file, shell and web tools are refused outright,
    /// so it works only through our tool server, which keeps the original out of reach. It isn't
    /// left to the agent's defaults (they allow reads in its folder and may change).
    CleanRoomAnalyse,
    /// Build: read freely, edit through our approval tool (which asks the person when needed).
    Build,
    /// Build in Clean room: every read and edit goes through our approval tool, which keeps the
    /// original out of reach.
    CleanRoomBuild,
    /// A question about a build: read the project's files, change nothing, keep no session.
    Question,
    /// The same in Clean room: no file tools at all (the answer comes from what it knows and
    /// what the question says), so the original stays out of reach.
    CleanRoomQuestion,
}

/// argv (after the program) for one prompt.
pub fn build_args(
    adapter: &dyn AgentAdapter,
    opts: &BackendOptions,
    prompt: &str,
    preamble: &str,
    mcp: &McpCommand,
    resume: Option<&str>,
    access: Access,
) -> Vec<String> {
    // A leading dash would be read as a flag.
    let prompt = if prompt.starts_with('-') { format!(" {prompt}") } else { prompt.to_string() };
    let builder = matches!(access, Access::Build | Access::CleanRoomBuild);
    let question = matches!(access, Access::Question | Access::CleanRoomQuestion);
    let reads_checked = matches!(access, Access::CleanRoomBuild | Access::CleanRoomQuestion | Access::CleanRoomAnalyse);
    adapter.args(&ArgsCx { options: opts, prompt: &prompt, preamble, mcp, resume, builder, reads_checked, question })
}

/// What a run may do with files: a question reads only, a builder builds, anything else analyses;
/// Deep mode or not decides whether the original is in reach (#564).
fn access_for(builder: bool, deep: bool, question: bool) -> Access {
    match (builder, deep, question) {
        (_, true, true) => Access::Question,
        (_, false, true) => Access::CleanRoomQuestion,
        (true, true, false) => Access::Build,
        (true, false, false) => Access::CleanRoomBuild,
        (false, true, false) => Access::Analyse,
        (false, false, false) => Access::CleanRoomAnalyse,
    }
}

/// True when `program`'s help mentions every required flag (10 s limit, stdin closed).
pub fn verify_flags(program: &str, help_args: &[&str], needed: &[&str]) -> bool {
    if needed.is_empty() {
        return true;
    }
    match Process::new(program).args(help_args).timeout(Duration::from_secs(10)).stdout_cap(256 * 1024).run() {
        Ok(out) => {
            let text = format!("{}{}", out.stdout_text(), out.stderr_text());
            needed.iter().all(|f| text.contains(f))
        }
        Err(_) => false,
    }
}

/// The fix for a sign-in failure, in plain words. Sign-in always happens in Claude Code itself;
/// we never see or handle the account.
pub fn sign_in_hint(message: &str) -> Option<String> {
    sign_in_hint_in(message, adapters::in_flatpak(), None)
}

/// [`sign_in_hint`] for a Flatpak or not, and for Claude Code in a folder chosen for it
/// (`folder`), whose sign-in is its own: the command signs in there. In a Flatpak, Claude Code
/// keeps its sign-in and conversations in a folder of the app's own (#506), so signing in from a
/// normal terminal signs in the other copy: it's done through the sandbox, with that folder.
pub(crate) fn sign_in_hint_in(message: &str, flatpak: bool, folder: Option<&Path>) -> Option<String> {
    let m = message.to_ascii_lowercase();
    let wants_sign_in = ["authenticat", "oauth", "log in", "login", "not logged", "api key", "unauthorized"].iter().any(|w| m.contains(w));
    if !wants_sign_in {
        return None;
    }
    let folder = folder.map(|f| f.to_string_lossy().into_owned());
    Some(match (flatpak, folder) {
        (true, None) => format!("Sign in to Claude Code inside this app's sandbox: open a terminal and run `{FLATPAK_SIGN_IN}`, then try again."),
        (true, Some(f)) => format!(
            "Sign in to Claude Code inside this app's sandbox, in the folder chosen for it: open a terminal and run `{}`, then try again.",
            flatpak_sign_in(&crate::ssh::shell_quote(&f))
        ),
        (false, None) => "Sign in to Claude Code: open a terminal and run `claude auth login` (or start `claude` and type /login), then try again.".to_string(),
        (false, Some(f)) if cfg!(windows) => format!(
            "Sign in to Claude Code in the folder chosen for it: open PowerShell and run `$env:CLAUDE_CONFIG_DIR='{}'; claude auth login`, then try again.",
            f.replace('\'', "''")
        ),
        (false, Some(f)) => format!(
            "Sign in to Claude Code in the folder chosen for it: open a terminal and run `CLAUDE_CONFIG_DIR={} claude auth login`, then try again.",
            crate::ssh::shell_quote(&f)
        ),
    })
}

/// Signing in for the Claude Code that a Flatpak build starts: the same folder it will use.
const FLATPAK_SIGN_IN: &str = "flatpak run --command=sh ai.storyteller.craft-anything -c 'CLAUDE_CONFIG_DIR=\"$XDG_DATA_HOME/craft-anything/claude\" \"$HOME/.local/bin/claude\" auth login'";

/// [`FLATPAK_SIGN_IN`] for a chosen folder, already quoted for the sandbox's shell.
fn flatpak_sign_in(quoted: &str) -> String {
    let inner = format!("CLAUDE_CONFIG_DIR={quoted} \"$HOME/.local/bin/claude\" auth login");
    format!("flatpak run --command=sh ai.storyteller.craft-anything -c {}", crate::ssh::shell_quote(&inner))
}

/// Words in an API's answer when the installed Claude Code is older than the model it was asked
/// to use (#165).
pub const TOO_OLD_FOR_MODEL: &str = "does not support this model";

/// Claude Code's failure in plain words, and its fix when the person can make one: a version too
/// old for the model (#165), or a sign-in.
pub fn claude_code_failure(why: &str) -> (String, Option<String>) {
    claude_code_failure_in(why, None)
}

/// [`claude_code_failure`] for Claude Code in a folder chosen for it (`folder`), whose sign-in is
/// its own.
pub(crate) fn claude_code_failure_in(why: &str, folder: Option<&Path>) -> (String, Option<String>) {
    if why.to_ascii_lowercase().contains(TOO_OLD_FOR_MODEL) {
        return (
            format!("Claude Code is too old for the model it was asked to use ({}).", why.chars().take(200).collect::<String>()),
            Some(
                "Update the `claude` command-line tool, which is installed and updated apart from the Claude desktop app: open a terminal and run `claude update`, then try again. Or choose another model for Claude Code in Settings → AI."
                    .into(),
            ),
        );
    }
    (format!("Claude Code stopped: {why}"), sign_in_hint_in(why, adapters::in_flatpak(), folder))
}

/// Variables a Claude Code session sets for its own child processes. When Craft Anything itself
/// runs inside such a session, they'd make the person's `claude` act as part of that session
/// instead of as their own install, so they're removed. The person's own settings (provider
/// choice `CLAUDE_CODE_USE_*`, `CLAUDE_CONFIG_DIR`, `ANTHROPIC_*`) are kept.
pub fn host_session_vars(vars: impl Iterator<Item = String>) -> Vec<String> {
    let all: Vec<String> = vars.collect();
    if !all.iter().any(|k| k == "CLAUDECODE") {
        return Vec::new();
    }
    all.into_iter()
        .filter(|k| {
            matches!(k.as_str(), "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT")
                || k.starts_with("CLAUDE_AGENT_SDK_")
                || k.starts_with("CLAUDE_PREVIEW_")
                || (k.starts_with("CLAUDE_CODE_") && !k.starts_with("CLAUDE_CODE_USE_"))
        })
        .collect()
}

/// `program` as the person's own standalone Claude Code (see [`host_session_vars`]).
fn standalone(program: &str) -> Process {
    host_session_vars(std::env::vars().map(|(k, _)| k)).into_iter().fold(Process::new(program), |p, k| p.env_remove(k))
}

/// Parses one CLI's output stream: the adapter reads each line, this keeps what it learned.
pub struct StreamParser {
    adapter: Arc<dyn AgentAdapter>,
    pub session_id: Option<String>,
    pub tool_names: HashMap<String, String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    /// A missing or invalid per-step cost cannot become a complete turn total later.
    cost_incomplete: bool,
    pub saw_result: bool,
    /// The run was asked to continue a saved session the agent doesn't have (it lives on another
    /// computer, or under another folder), so it did nothing.
    pub session_missing: bool,
    /// A protocol fact about the failed turn, independent of the text shown to the person.
    pub failure: Option<TurnFailure>,
}

fn short_tool(name: &str) -> String {
    name.strip_prefix(&format!("mcp__{MCP_NAME}__")).or_else(|| name.strip_prefix(&format!("{MCP_NAME}__"))).unwrap_or(name).to_string()
}

fn find_evidence_id(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(pos) = rest.find("ev_") {
        let cand = rest.get(pos..pos + 67).unwrap_or_default();
        if craft_anything_evidence::is_evidence_id(cand) {
            return Some(cand.to_string());
        }
        rest = rest.get(pos + 3..).unwrap_or_default();
    }
    None
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn summary_of(s: &str) -> String {
    s.chars().take(200).collect()
}

impl StreamParser {
    /// A parser for one run, reading lines the way `adapter` says.
    pub fn new(adapter: Arc<dyn AgentAdapter>) -> Self {
        StreamParser {
            adapter,
            session_id: None,
            tool_names: HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: None,
            cost_incomplete: false,
            saw_result: false,
            session_missing: false,
            failure: None,
        }
    }

    /// Events for one output line.
    pub fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        if !self.adapter.streams_json() {
            return vec![AgentEvent::TextDelta { text: format!("{line}\n") }];
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            // Non-JSON lines (warnings, banners) are shown as status, not answer text.
            let t = line.trim();
            return if t.is_empty() { Vec::new() } else { vec![AgentEvent::Status { text: summary_of(t) }] };
        };
        let adapter = self.adapter.clone();
        adapter.parse(self, &v)
    }

    /// A tool call began (adapters call this from `parse`).
    pub fn started(&mut self, id: &str, name: &str, input: Value) -> AgentEvent {
        let tool = short_tool(name);
        self.tool_names.insert(id.to_string(), tool.clone());
        AgentEvent::ToolStarted { call_id: id.to_string(), tool, input }
    }

    /// A tool call ended.
    pub fn finished(&mut self, id: &str, ok: bool, text: &str) -> AgentEvent {
        let tool = self.tool_names.get(id).cloned().unwrap_or_default();
        AgentEvent::ToolFinished { call_id: id.to_string(), tool, ok, summary: summary_of(text), evidence_id: find_evidence_id(text) }
    }
}

/// Everything needed to run one CLI backend.
#[derive(Debug, Clone)]
pub struct CliSpec {
    pub config: BackendConfig,
    pub options: BackendOptions,
    pub beginner: bool,
    /// The working folder (and, without `builder`, the investigation handed to our MCP server).
    pub project: Option<PathBuf>,
    /// The desktop's open targets, active last; the MCP child restores them before serving.
    pub targets: Vec<craft_anything_engine::target::TargetContext>,
    /// Build mode: the agent may edit files in the working folder.
    pub builder: Option<Builder>,
    /// A saved agent session to continue.
    pub resume: Option<String>,
    /// Deep mode: our MCP server runs in it, and a builder sees every tool and may read files
    /// anywhere. In Clean room a builder sees only Clean room's tools, and every file it reads
    /// goes through the approval policy.
    pub deep: bool,
    /// Replaces the default analysis preamble.
    pub preamble: Option<String>,
    /// How each agent CLI is driven.
    pub adapters: AgentAdapters,
    /// A question about a build: read-only, with a fresh session or an unkept fork, so the
    /// builder's own session is never changed. Only adapters that
    /// [answer questions](adapters::AgentAdapter::answers_questions) take it.
    pub question: bool,
    /// Where a failed run writes what's known about it ([`crate::failure_log`]). None writes no log.
    pub logs: Option<PathBuf>,
}

/// What a building run needs besides the working folder.
#[derive(Debug, Clone)]
pub struct Builder {
    /// The investigation project our MCP server keeps evidence in.
    pub investigation: Option<PathBuf>,
    /// Where questions and approvals for the person go.
    pub inbox: PathBuf,
}

/// Our MCP server for `spec`, in the run's mode whatever the saved settings say; a builder's lists
/// every tool in Deep mode.
fn mcp_for(spec: &CliSpec, remote: bool) -> McpCommand {
    let mut mcp = match &spec.builder {
        Some(b) => McpCommand::local_or_remote(remote, b.investigation.clone(), Some(b.inbox.clone())),
        None => McpCommand::local_or_remote(remote, spec.project.clone(), None),
    };
    mcp.args.push(if spec.deep { "--deep" } else { "--clean-room" }.into());
    if spec.deep && spec.builder.is_some() {
        mcp.args.push("--all-tools".into());
    }
    if !remote {
        for target in spec.targets.iter().skip(spec.targets.len().saturating_sub(128)) {
            if let Ok(target) = serde_json::to_string(target) {
                mcp.args.extend(["--target".into(), target]);
            }
        }
    }
    mcp
}

impl CliSpec {
    pub fn new(config: BackendConfig, options: BackendOptions, beginner: bool, project: Option<PathBuf>) -> Self {
        CliSpec {
            config,
            options,
            beginner,
            project,
            targets: Vec::new(),
            builder: None,
            resume: None,
            deep: false,
            preamble: None,
            logs: None,
            adapters: AgentAdapters::built_in(),
            question: false,
        }
    }
}

/// The conversation for a chat with an agent CLI: over ACP when the backend enables or requires
/// it and the agent speaks it (a local, non-building run), else one CLI run per message.
pub fn conversation(spec: CliSpec) -> Box<dyn Conversation> {
    match acp_for(&spec).filter(|_| spec.builder.is_none()) {
        // In Clean room the agent's own file, shell and web requests are refused, not put to the
        // person as a card they could allow with one click (#564).
        Some(c) if !spec.deep => Box::new(c.with_approver(acp::clean_room_chat_approver())),
        Some(c) => Box::new(c),
        None => Box::new(CliConversation::new(spec)),
    }
}

/// The conversation for a builder run: over ACP (permission requests decided by `approver`, the
/// approval policy) when the backend enables or requires it and the agent speaks it, else one
/// CLI run per message (approvals through our approval tool).
pub fn builder_conversation(spec: CliSpec, approver: acp::Approver) -> Box<dyn Conversation> {
    match acp_for(&spec) {
        Some(c) => Box::new(c.with_approver(approver)),
        None => Box::new(CliConversation::new(spec)),
    }
}

/// An ACP conversation for a local backend that enables or requires ACP. The adapter chooses
/// the tool restrictions for this access mode; questions omit MCP startup flags and never load
/// the builder's session. Unsupported routes return to the CLI runner, which refuses ACP-only agents.
fn acp_for(spec: &CliSpec) -> Option<AcpConversation> {
    let agent = spec.config.model.clone();
    let adapter = spec.adapters.for_agent(&agent);
    if spec.question && !adapter.answers_questions() {
        return None;
    }
    let local = spec.config.ssh_host.as_deref().is_none_or(|h| h.trim().is_empty());
    let acp_args = selected_acp_args(adapter.as_ref(), &agent, &spec.options).filter(|_| local)?;
    let program = program_of(&spec.config, &spec.options);
    let access = match (spec.question, spec.deep, spec.builder.is_some()) {
        (true, true, _) => Access::Question,
        (true, false, _) => Access::CleanRoomQuestion,
        (false, true, true) => Access::Build,
        (false, false, true) => Access::CleanRoomBuild,
        _ => Access::Analyse,
    };
    let mut args = adapter.acp_options(&spec.options, access);
    args.extend(acp_args.iter().map(|a| a.to_string()));
    args.extend(acp::model_args(&agent, spec.options.cli_model.as_deref()));
    let cwd = spec.project.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_else(std::env::temp_dir);
    let mcp = mcp_for(spec, false);
    if !spec.question {
        args.extend(adapter.acp_mcp_options(&mcp));
    }
    let preamble = spec.preamble.clone().unwrap_or_else(|| crate::prompt::cli_preamble(spec.beginner));
    Some(
        AcpConversation::new(&spec.config.name, &agent, program, args, cwd, mcp, if spec.question { None } else { spec.resume.clone() })
            .with_validation(adapter.validate_options(&spec.options))
            .with_adapter(adapter)
            .with_question(spec.question)
            .with_preamble(preamble)
            .with_logs(spec.logs.clone()),
    )
}

/// Copilot's selectable models from a fresh local ACP session, with `auto` offered once at the
/// front. No MCP server or model prompt is sent, and a stale model override can't prevent the
/// catalogue from loading. Blocking, with a 15-second metadata wait: call off the UI thread.
pub fn acp_model_choices(config: &BackendConfig, options: &BackendOptions) -> Result<Vec<String>, ToolError> {
    let mut spec = CliSpec::new(config.clone(), options.clone(), false, None);
    spec.question = true;
    spec.options.cli_model = None;
    spec.options.cli_context = None;
    spec.options.cli_reasoning_effort = None;
    spec.options.effort = None;
    let conversation = acp_for(&spec).ok_or_else(|| ToolError::invalid("This agent needs a local ACP connection to list its models."))?;
    let mut models = conversation.model_choices(Duration::from_secs(15))?;
    models.retain(|model| model != "auto");
    models.insert(0, "auto".into());
    Ok(models)
}

/// Discover settings for the backend's selected model, without a tool server or model prompt.
pub fn acp_model_tuning(config: &BackendConfig, options: &BackendOptions, stop: &Stop) -> Result<crate::model_options::ModelTuning, ToolError> {
    stop.check()?;
    let metadata = acp_model_metadata(config, options, None, stop)?;
    Ok(crate::model_options::ModelTuning::from_metadata(metadata.current_model, &metadata.config_options))
}

pub(crate) fn acp_model_metadata(
    config: &BackendConfig,
    options: &BackendOptions,
    directory: Option<&Path>,
    stop: &Stop,
) -> Result<acp::ModelMetadata, ToolError> {
    let mut spec = CliSpec::new(config.clone(), options.clone(), false, directory.map(Path::to_path_buf));
    spec.question = true;
    spec.options.cli_context = None;
    spec.options.cli_reasoning_effort = None;
    spec.options.cli_thinking = None;
    spec.options.effort = None;
    let conversation = acp_for(&spec).ok_or_else(|| ToolError::invalid("Model settings need a local ACP connection."))?;
    conversation.model_metadata_with_stop(Duration::from_secs(15), stop)
}

/// A conversation backed by an agent CLI. Each message is one CLI run; Claude Code runs resume
/// the previous session.
pub struct CliConversation {
    spec: CliSpec,
    tx: Sender<AgentEvent>,
    rx: Receiver<AgentEvent>,
    busy: Arc<AtomicBool>,
    stop: Stop,
    session: Arc<Mutex<Option<String>>>,
    verified: Arc<Mutex<Option<bool>>>,
    /// The last run's saved session wasn't there (see [`Conversation::session_lost`]).
    lost: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<TurnFailure>>>,
}

impl CliConversation {
    /// A conversation run as `spec` says; with `spec.resume`, its first message continues that
    /// saved session.
    pub fn new(spec: CliSpec) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Arc::new(Mutex::new(spec.resume.clone()));
        CliConversation {
            spec,
            tx,
            rx,
            busy: Arc::default(),
            stop: Stop::default(),
            session,
            verified: Arc::default(),
            lost: Arc::default(),
            failure: Arc::default(),
        }
    }

    /// The agent's session id once a run has reported it (to continue later).
    pub fn session_id(&self) -> Option<String> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn send_prompt(&mut self, text: &str, retry: bool) {
        if self.busy.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(AgentEvent::Error {
                message: "Still working on the last message.".into(),
                remediation: Some("Wait for it to finish or press stop.".into()),
                log: None,
            });
            return;
        }
        self.stop.reset();
        self.lost.store(false, Ordering::SeqCst);
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let (spec, tx, stop, busy) = (self.spec.clone(), self.tx.clone(), self.stop.clone(), self.busy.clone());
        let state = RunState { session: self.session.clone(), verified: self.verified.clone(), lost: self.lost.clone(), failure: self.failure.clone() };
        let text = text.to_string();
        let spawned = std::thread::Builder::new().name("ai-cli".into()).spawn(move || {
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_cli(&spec, &text, retry, &tx, &stop, &state))).unwrap_or_else(|_| {
                let _ = tx.send(AgentEvent::Error { message: "The agent runner hit a bug and stopped safely.".into(), remediation: None, log: None });
                nothing_done(&spec.config.name)
            });
            end_turn(&busy, done, |e| {
                let _ = tx.send(e);
            });
        });
        if spawned.is_err() {
            let _ = self.tx.send(AgentEvent::Error { message: "Couldn't start a worker thread.".into(), remediation: None, log: None });
            end_turn(&self.busy, nothing_done(&self.spec.config.name), |e| {
                let _ = self.tx.send(e);
            });
        }
    }
}

/// The environment an agent program runs with: Claude Code's limits raised for a long build, and
/// the effort it thinks with. Other agents ignore these names.
fn agent_env(builder: bool, effort: Option<crate::model::Effort>) -> Vec<(&'static str, String)> {
    let (tool, shell) = if builder { (24 * 3600, 8 * 3600) } else { (40 * 60, 10 * 60) };
    let ms = |secs: u64| (secs * 1000).to_string();
    vec![
        ("MCP_TOOL_TIMEOUT", ms(tool)),
        // Progress updates keep a long tool call alive; this also covers one that sends none.
        ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0".into()),
        // A builder waits for every tool result (Ghidra above all) instead of carrying on without it.
        ("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", if builder { "0" } else { "120000" }.into()),
        ("BASH_DEFAULT_TIMEOUT_MS", ms(shell / 2)),
        ("BASH_MAX_TIMEOUT_MS", ms(shell)),
        // High effort for Claude Code unless chosen. An environment variable rather than
        // `--effort`: it ranks highest, and an older Claude Code that doesn't know it ignores it.
        ("CLAUDE_CODE_EFFORT_LEVEL", crate::prompt::choose_effort(effort).as_str().into()),
    ]
}

/// The command run on another computer over SSH. ssh passes on none of our environment, so it is
/// set there with `env`. The result is the line for the remote shell, every word quoted, except
/// that a value starting with `~/` gets that computer's home folder (a quoted `~` is just a name).
fn remote_command(program: &str, args: Vec<String>, env: &[(&'static str, String)]) -> String {
    use crate::ssh::shell_quote;
    let set = env.iter().map(|(k, v)| match v.strip_prefix("~/") {
        Some(rest) => format!("{k}=\"$HOME\"/{}", shell_quote(rest)),
        None if v == "~" => format!("{k}=\"$HOME\""),
        None => shell_quote(&format!("{k}={v}")),
    });
    let words = std::iter::once(program.to_string()).chain(args).map(|a| shell_quote(&a));
    std::iter::once("env".to_string()).chain(set).chain(words).collect::<Vec<_>>().join(" ")
}

/// The run's shared state: the session it continues and reports, whether the agent's flags were
/// checked, and whether the session it was to continue was missing.
struct RunState {
    session: Arc<Mutex<Option<String>>>,
    verified: Arc<Mutex<Option<bool>>>,
    lost: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<TurnFailure>>>,
}

/// End a turn: take the next message first, then say the turn is done (`send(done)`), since
/// whoever reads `TurnDone` may send the next message at once. The other order refuses it ("Still
/// working") with no `TurnDone`, and the caller waits forever.
fn end_turn(busy: &AtomicBool, done: AgentEvent, send: impl FnOnce(AgentEvent)) {
    busy.store(false, Ordering::SeqCst);
    send(done);
}

/// The `TurnDone` that ends a turn of `backend` with no usage to report.
fn nothing_done(backend: &str) -> AgentEvent {
    AgentEvent::TurnDone { backend: backend.to_string(), input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false }
}

/// One message as one run of the agent's program (here or over SSH), its events sent on `tx`
/// as they come. Returns the `TurnDone` that ends the turn, whatever happened, for the caller to
/// send once the conversation can take the next message. The session it reports is kept for the
/// next message; one it was asked to continue but doesn't have is forgotten and flagged in
/// `state.lost` before the error that says so is sent.
fn run_cli(spec: &CliSpec, text: &str, retry: bool, tx: &Sender<AgentEvent>, stop: &Stop, state: &RunState) -> AgentEvent {
    let RunState { session, verified, lost, failure } = state;
    let name = spec.config.name.clone();
    let agent = spec.config.model.clone();
    let program = program_of(&spec.config, &spec.options);
    let remote = spec.config.ssh_host.clone().filter(|h| !h.trim().is_empty());
    // A folder chosen for it is on the computer it runs on: on another one, it's set below.
    let mut adapter = match remote {
        None => spec.adapters.for_backend(&agent, &spec.options),
        Some(_) => spec.adapters.for_agent(&agent),
    };
    if adapter.requires_acp() {
        let sign_in = adapter.sign_in_fix().map(|fix| format!(" {fix}")).unwrap_or_default();
        let _ = tx.send(AgentEvent::Error {
            message: format!("{name} requires a local ACP connection."),
            remediation: Some(format!("Run {} on this computer, remove the SSH host and do not disable ACP.{sign_in}", display_for(&agent))),
            log: None,
        });
        return nothing_done(&name);
    }
    if let Err(e) = adapter.check_native_options(&spec.options) {
        let _ = tx.send(AgentEvent::Error { message: e.message, remediation: e.remediation, log: None });
        return nothing_done(&name);
    }
    if let Err(e) = adapter.validate_model(&spec.config, &spec.options, spec.project.as_deref(), stop) {
        let event = if e.code == ErrorCode::Cancelled {
            AgentEvent::Status { text: "Stopped.".into() }
        } else {
            AgentEvent::Error { message: e.message, remediation: e.remediation, log: None }
        };
        let _ = tx.send(event);
        return nothing_done(&name);
    }
    let (help, needed) = adapter.required_flags();
    if !needed.is_empty() {
        let ok = match remote {
            Some(_) => true, // can't cheaply check a remote CLI; its errors are streamed back
            None => {
                let mut v = verified.lock().unwrap_or_else(PoisonError::into_inner);
                *v.get_or_insert_with(|| verify_flags(&program, help, needed))
            }
        };
        if !ok && adapter.plain_fallback() {
            let _ = tx.send(AgentEvent::Status {
                text: format!("{program}'s help doesn't list {} here, so it runs with the plain adapter (prompt in, text out).", needed.join(" / ")),
            });
            adapter = Arc::new(adapters::Plain);
            // A saved session cannot be continued through the adapter that will actually run.
            // Forget it before retry chooses between the original request and a continuation.
            *session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        } else if !ok {
            let _ = tx.send(AgentEvent::Status { text: format!("Couldn't confirm that {program} has {}; trying it anyway.", needed.join(" / ")) });
        }
    }
    let preamble = spec.preamble.clone().unwrap_or_else(|| crate::prompt::cli_preamble(spec.beginner));
    let mcp = mcp_for(spec, remote.is_some());
    let resume = session.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let text = if retry && adapter.resumes() && resume.is_some() { crate::failure::CONTINUE_TURN } else { text };
    let builder = spec.builder.is_some();
    // A question goes only to an agent that can be held to reading (a fallback adapter can't).
    if spec.question && !adapter.answers_questions() {
        let _ = tx.send(AgentEvent::Error {
            message: format!("{name} can't be kept to only reading here, so it isn't asked questions about a build."),
            remediation: Some("Ask with Claude Code, or with an AI that has an API key.".into()),
            log: None,
        });
        return nothing_done(&name);
    }
    let access = access_for(spec.builder.is_some(), spec.deep, spec.question);
    let args = build_args(adapter.as_ref(), &spec.options, text, &preamble, &mcp, if adapter.resumes() { resume.as_deref() } else { None }, access);
    let shown = FailureContext::new(&program, &args, text, remote.as_deref(), spec);
    // A builder turn may take hours and sit silent for long stretches: Ghidra's first look at a big
    // game, an engine build, or the person answering a question. Claude Code's own limits are
    // raised to match, so nothing cuts a long clone short (its defaults stop a quiet tool call at
    // 30 minutes and a background build at 10).
    let (turn, quiet) = if builder { (24 * 3600, 8 * 3600) } else { (30 * 60, 15 * 60) };
    let mut env = agent_env(builder, spec.options.effort);
    // Here the adapter's process runs the agent in its folder (`AgentAdapter::in_folder`); on
    // another computer over SSH it's the folder chosen there, else the agent keeps its own.
    if remote.is_some()
        && let (Some(var), Some(dir)) = (adapter.folder_var(), spec.options.config_dir_text())
    {
        env.push((var, dir.to_string()));
    }
    let (prog, argv) = match &remote {
        Some(host) => ("ssh".to_string(), crate::ssh::ssh_line_args(host, remote_command(&program, args, &env))),
        None => (program.clone(), args),
    };
    let _ =
        tx.send(AgentEvent::Status { text: format!("Asking {}{}…", display_for(&agent), remote.as_ref().map(|h| format!(" on {h}")).unwrap_or_default()) });
    let parser = Arc::new(Mutex::new(StreamParser::new(adapter.clone())));
    let (p2, tx2, tx_err, lost2, failure2) = (parser.clone(), tx.clone(), tx.clone(), lost.clone(), failure.clone());
    let failed_tools = Arc::new(AtomicBool::new(false));
    let (failed_tools2, failed_tools3) = (failed_tools.clone(), failed_tools.clone());
    let stop2 = stop.clone();
    let mut proc = (if remote.is_none() { adapter.process(&prog) } else { Process::new(&prog) })
        .args(&argv)
        .timeout(Duration::from_secs(spec.options.timeout_secs.unwrap_or(turn)))
        .idle_timeout(Duration::from_secs(quiet));
    for (k, v) in &env {
        proc = proc.env(k, v.as_str());
    }
    let mut proc = proc
        .stdout_cap(4 * 1024 * 1024)
        .cancel_when(move || stop2.is_stopped() || failed_tools2.load(Ordering::SeqCst))
        .on_stdout_line(move |l| {
            if failed_tools3.load(Ordering::SeqCst) {
                return;
            }
            let mut p = p2.lock().unwrap_or_else(PoisonError::into_inner);
            let events = p.line(l);
            // Said before the error that follows it, so whoever reads the error can tell.
            if p.session_missing {
                lost2.store(true, Ordering::SeqCst);
            }
            if let Some(failed) = p.failure {
                *failure2.lock().unwrap_or_else(PoisonError::into_inner) = Some(failed);
                if failed.kind == FailureKind::ToolsUnavailable {
                    failed_tools3.store(true, Ordering::SeqCst);
                }
            }
            for e in events {
                let _ = tx2.send(e);
            }
        })
        .on_stderr_line(move |l| {
            if !l.trim().is_empty() {
                let _ = tx_err.send(AgentEvent::Output { text: l.to_string() });
            }
        })
        .remediation(format!("Install {}, or choose its program in Settings → AI.", display_for(&agent)));
    if let Some(p) = spec.project.as_ref().filter(|_| remote.is_none()) {
        proc = proc.cwd(p);
    }
    if adapter.prompt_on_stdin() {
        proc = proc.stdin_bytes(adapter.stdin_text(&preamble, text));
    }
    let result = proc.run();
    let p = parser.lock().unwrap_or_else(PoisonError::into_inner);
    if p.session_missing {
        // The id it reported is the one it couldn't find: the next message starts a new session.
        *session.lock().unwrap_or_else(PoisonError::into_inner) = None;
    } else if let Some(id) = &p.session_id {
        *session.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.clone());
    }
    if p.failure.is_none() {
        let failed = match &result {
            Ok(out) if !out.success => Some(TurnFailure::process_exit(out.code)),
            Err(e) if e.code != ErrorCode::Cancelled => Some(TurnFailure::from_process(e)),
            _ => None,
        };
        *failure.lock().unwrap_or_else(PoisonError::into_inner) = failed;
    }
    match result {
        // The protocol already named the required tool server. Stop the owned process without
        // replacing that useful error with the cancellation used to stop it.
        result if failed_tools.load(Ordering::SeqCst) => {
            let (out, error) = match &result {
                Ok(out) => (Some(out), None),
                Err(e) => (None, Some(e.message.as_str())),
            };
            if let Some(path) = shown.save_with(&agent, out, Some("Craft Anything's tools did not start."), error).0 {
                let _ = tx.send(AgentEvent::Status { text: format!("The details are in {}", path.display()) });
            }
        }
        // It said why already, and there's nothing more in its output to keep.
        Ok(_) if p.session_missing => {}
        Ok(out) if !out.success && !p.saw_result => {
            let _ = tx.send(shown.exit_event(&agent, &out));
        }
        Ok(out) if !out.success => {
            // The agent already said what went wrong; keep the rest for anyone looking into it.
            if let Some(path) = shown.save(&agent, Some(&out), None).0 {
                let _ = tx.send(AgentEvent::Status { text: format!("The details are in {}", path.display()) });
            }
        }
        Ok(_) => {}
        Err(e) if e.code == craft_anything_evidence::ErrorCode::Cancelled => {
            let _ = tx.send(AgentEvent::Status { text: "Stopped.".into() });
        }
        Err(e) => {
            let _ = tx.send(shown.error_event(&agent, e));
        }
    }
    AgentEvent::TurnDone { backend: name, input_tokens: p.input_tokens, output_tokens: p.output_tokens, cost_usd: p.cost_usd, cost_is_estimate: true }
}

/// What's needed to explain a failed CLI run: the command with the person's message left out, and
/// where to write the log.
struct FailureContext {
    command: String,
    check: String,
    program: String,
    host: Option<String>,
    backend: String,
    cwd: Option<PathBuf>,
    logs: Option<PathBuf>,
}

impl FailureContext {
    fn new(program: &str, args: &[String], message: &str, host: Option<&str>, spec: &CliSpec) -> Self {
        use craft_anything_platform::process::printable_command;
        let message = message.trim();
        let args: Vec<String> = args
            .iter()
            .map(|a| if !message.is_empty() && a.contains(message) { format!("[your message, {} characters]", message.chars().count()) } else { a.clone() })
            .collect();
        let local = printable_command(OsStr::new(program), &args);
        let (command, check) = match host {
            Some(h) => {
                let options = crate::ssh::SSH_OPTIONS.join(" ");
                (format!("ssh {options} -T {} -- {local}", crate::ssh::shell_quote(h)), crate::ssh::check_command(h, program))
            }
            None => (local, format!("{} --version", crate::ssh::shell_quote(program))),
        };
        FailureContext {
            command,
            check,
            program: program.to_string(),
            host: host.map(str::to_string),
            backend: spec.config.name.clone(),
            cwd: spec.project.clone().filter(|_| host.is_none()),
            logs: spec.logs.clone(),
        }
    }

    /// Why this run failed, when it was ssh (or the remote computer) that says so.
    fn ssh_failure(&self, out: &craft_anything_platform::Output) -> Option<crate::ssh::SshFailure> {
        let host = self.host.as_deref()?;
        crate::ssh::diagnose(host, &self.program, crate::ssh::is_git_server(host), out.code, &out.stderr_text())
    }

    /// Write the log. Returns its path, or what to tell the person when it couldn't be written.
    fn save(&self, agent: &str, out: Option<&craft_anything_platform::Output>, diagnosis: Option<&str>) -> (Option<PathBuf>, Option<String>) {
        self.save_with(agent, out, diagnosis, None)
    }

    fn save_with(
        &self,
        agent: &str,
        out: Option<&craft_anything_platform::Output>,
        diagnosis: Option<&str>,
        error: Option<&str>,
    ) -> (Option<PathBuf>, Option<String>) {
        let Some(dir) = &self.logs else { return (None, None) };
        let report = crate::failure_log::Report {
            what: format!("{} ({})", display_for(agent), self.backend),
            command: self.command.clone(),
            check: self.check.clone(),
            diagnosis: diagnosis.map(str::to_string),
            error: error.map(str::to_string),
            cwd: self.cwd.clone(),
            exit_code: out.and_then(|o| o.code),
            duration: out.map(|o| o.duration),
            stdout: out.map(|o| o.stdout.clone()).unwrap_or_default(),
            stderr: out.map(|o| o.stderr.clone()).unwrap_or_default(),
        };
        match crate::failure_log::write(dir, &report) {
            Ok(path) => {
                log::warn!("{} failed; details in {}", display_for(agent), path.display());
                (Some(path), None)
            }
            Err(e) => {
                log::warn!("couldn't write a failure log: {e}");
                (None, Some(format!("The details couldn't be saved: {e}")))
            }
        }
    }

    fn exit_event(&self, agent: &str, out: &craft_anything_platform::Output) -> AgentEvent {
        let name = display_for(agent);
        let plain = craft_anything_platform::process::nonzero_error_with(&name, out, Some(&self.command));
        let excerpt = craft_anything_platform::process::output_excerpt(&String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }));
        let (message, mut remediation) = match self.ssh_failure(out) {
            Some(f) => {
                let host = self.host.as_deref().unwrap_or_default();
                let said = if excerpt.is_empty() { String::new() } else { format!("\n\nssh printed:\n{excerpt}") };
                (format!("Couldn't run {name} on {host}: {}.", f.what), format!("{}{said}\n\nTo test it yourself, run:\n{}", f.fix, self.check))
            }
            None => (plain.message, plain.remediation.unwrap_or_default()),
        };
        let (log, unsaved) = self.save(agent, Some(out), Some(&message));
        if let Some(u) = unsaved {
            remediation.push_str(&format!("\n\n{u}"));
        }
        AgentEvent::Error { message, remediation: Some(remediation), log }
    }

    fn error_event(&self, agent: &str, e: ToolError) -> AgentEvent {
        let details = e.details.as_ref().map(ToString::to_string);
        let text = match details {
            Some(d) => format!("{}\n{d}", e.message),
            None => e.message.clone(),
        };
        let (log, unsaved) = self.save_with(agent, None, None, Some(&text));
        let remediation = match (e.remediation, unsaved) {
            (Some(r), Some(u)) => Some(format!("{r}\n\n{u}")),
            (r, u) => r.or(u),
        };
        AgentEvent::Error { message: e.message, remediation, log }
    }
}

impl Conversation for CliConversation {
    fn retry(&mut self, text: &str) {
        self.send_prompt(text, true);
    }

    fn last_failure(&self) -> Option<TurnFailure> {
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn session_id(&self) -> Option<String> {
        CliConversation::session_id(self)
    }

    /// Set by the last run (see `run_cli`) and cleared when the next message is sent.
    fn session_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Runs the message on a thread of its own; a message while one is running is refused with an
    /// error event. Every turn ends with `TurnDone`, a runner that panics or can't start included,
    /// and the conversation takes the next message before that's sent: whoever reads it may send
    /// the next message at once (a session that wasn't there is followed by a fresh start).
    fn send(&mut self, text: &str) {
        self.send_prompt(text, false);
    }

    fn poll(&mut self) -> Vec<AgentEvent> {
        self.rx.try_iter().collect()
    }

    /// Agent CLIs ask for approval through their own UI; our MCP server refuses consent-gated
    /// tools unless it was started with `--allow-run`.
    fn answer_consent(&mut self, _call_id: &str, _allow: bool) {}

    fn stop(&mut self) {
        self.stop.stop();
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

impl Drop for CliConversation {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #721: Codex's desktop app keeps its CLI in `bin/<build>/codex.exe` and moves it on every
    /// update; a backend set to the old path kept failing on a file that was gone.
    #[test]
    fn a_chosen_program_that_moved_with_an_update_is_found_again() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("Codex/bin");
        let new = bin.join("build-2").join("codex.exe");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, b"").unwrap();
        let gone = bin.join("build-1").join("codex.exe");
        let config = |ssh: Option<&str>| BackendConfig {
            name: "Codex".into(),
            kind: crate::BackendKind::Nvidia,
            model: "codex".into(),
            base_url: None,
            ssh_host: ssh.map(str::to_string),
            enabled: true,
        };
        let chosen = |p: &Path| BackendOptions { command: Some(p.display().to_string()), ..Default::default() };
        assert_eq!(program_of(&config(None), &chosen(&gone)), new.display().to_string(), "the same file in the newer build folder");
        assert_eq!(program_of(&config(None), &chosen(&new)), new.display().to_string(), "a path that's there is used as it is");
        assert_eq!(program_of(&config(Some("box")), &chosen(&gone)), gone.display().to_string(), "another computer's path isn't checked here");
        assert_eq!(program_of(&config(None), &BackendOptions::default()), "codex", "nothing chosen: the usual name");
        // #467: a saved app bundle is never started; the usual name is looked up instead.
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Codex.app");
        std::fs::create_dir_all(&bundle).unwrap();
        assert_eq!(program_of(&config(None), &chosen(&bundle)), "codex", "an app bundle is not the program");
        assert_eq!(program_of(&config(Some("box")), &chosen(&bundle)), bundle.display().to_string(), "another computer's path isn't checked here");
        let bare = BackendOptions { command: Some("my-codex".into()), ..Default::default() };
        assert_eq!(program_of(&config(None), &bare), "my-codex", "a bare name is left to PATH");
    }

    #[test]
    fn failed_craft_tool_server_is_reported_from_its_init_status() {
        let mut p = StreamParser::new(adapter_for("claude-code"));
        let events = p.line(r#"{"type":"system","subtype":"init","session_id":"cannot-build","mcp_servers":[{"name":"craft-anything","status":"failed"}]}"#);
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("tools") && message.contains("start"))),
            "a failed required tool server must end the turn with an actionable error: {events:?}"
        );
        assert_eq!(p.failure, Some(TurnFailure::new(FailureKind::ToolsUnavailable)));
    }

    #[test]
    fn optional_or_pending_mcp_servers_do_not_block_the_builder() {
        for status in ["connected", "pending"] {
            let mut p = StreamParser::new(adapter_for("claude-code"));
            let init = json!({"type":"system","subtype":"init","mcp_servers":[{"name":"craft-anything","status":status},{"name":"optional-service","status":"failed"}]});
            assert!(p.line(&init.to_string()).is_empty());
            assert!(p.failure.is_none());
        }
        let mut p = StreamParser::new(adapter_for("claude-code"));
        assert!(!p.line(r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"craft-anything","status":"needs-auth"}]}"#).is_empty());
        assert_eq!(p.failure, Some(TurnFailure::new(FailureKind::ToolsUnavailable)));
    }

    #[test]
    fn claude_assistant_error_codes_survive_localized_display_messages() {
        for (code, kind) in [
            ("authentication_failed", FailureKind::Authentication),
            ("billing_error", FailureKind::Billing),
            ("rate_limit", FailureKind::RateLimited),
            ("server_error", FailureKind::Transient),
        ] {
            let mut p = StreamParser::new(adapter_for("claude-code"));
            let events = p.line(&json!({"type":"assistant","error":code,"message":{"content":[{"type":"text","text":"服务暂时不可用"}]}}).to_string());
            assert!(matches!(events.as_slice(), [AgentEvent::Error { .. }]));
            assert_eq!(p.failure, Some(TurnFailure::new(kind)));
            p.line(r#"{"type":"result","is_error":true,"result":"the turn ended"}"#);
            assert_eq!(p.failure, Some(TurnFailure::new(kind)), "result must keep the more specific code");
        }
    }

    /// #506: under a Flatpak `~/.claude` is read-only, so Claude Code gets a folder of ours to save
    /// its conversations in (so `--resume` finds them); the person's own `CLAUDE_CONFIG_DIR` and a
    /// normal install are left as they are, and the sign-in hint says how to sign in there.
    #[test]
    fn under_a_flatpak_claude_code_keeps_its_conversations_in_a_folder_it_can_write() {
        let data = || Some(std::path::PathBuf::from("/home/u/.var/app/ai.storyteller.craft-anything/data"));
        let dir = adapters::flatpak_claude_dir_in(true, false, data()).unwrap();
        assert!(dir.ends_with("craft-anything/claude") && dir.starts_with("/home/u/.var/app/ai.storyteller.craft-anything/data"), "{dir:?}");
        assert_eq!(adapters::flatpak_claude_dir_in(true, true, data()), None, "their own choice stays");
        assert_eq!(adapters::flatpak_claude_dir_in(false, false, data()), None, "a normal install keeps ~/.claude");
        assert_eq!(adapters::flatpak_claude_dir_in(true, false, None), None);
        let hint = sign_in_hint_in("Not logged in", true, None).unwrap();
        assert!(hint.contains("flatpak run") && hint.contains("CLAUDE_CONFIG_DIR") && hint.contains("auth login"), "{hint}");
        assert!(sign_in_hint_in("Not logged in", false, None).unwrap().contains("`claude auth login`"));
        assert_eq!(sign_in_hint_in("all fine", true, None), None);
    }

    /// Claude Code in a folder chosen for it signs in there: the fix names that folder, quoted.
    #[test]
    fn signing_in_claude_code_in_its_chosen_folder_names_the_folder() {
        let folder = Path::new("/Users/someone/Claude work");
        let hint = sign_in_hint_in("Not logged in", false, Some(folder)).unwrap();
        if cfg!(windows) {
            assert!(hint.contains("$env:CLAUDE_CONFIG_DIR='/Users/someone/Claude work'; claude auth login"), "{hint}");
        } else {
            assert!(hint.contains("`CLAUDE_CONFIG_DIR='/Users/someone/Claude work' claude auth login`"), "{hint}");
        }
        let hint = sign_in_hint_in("Not logged in", true, Some(folder)).unwrap();
        assert!(hint.starts_with("Sign in to Claude Code inside this app's sandbox, in the folder") && hint.contains("Claude work"), "{hint}");
        assert!(!hint.contains("XDG_DATA_HOME"), "the chosen folder, not the sandbox's own: {hint}");
        let (_, fix) = claude_code_failure_in("Invalid API key · Please run /login", Some(folder));
        assert!(fix.is_some_and(|f| f.contains("Claude work")));
    }

    /// Over SSH our environment doesn't reach the other computer, so the chosen effort (and Claude
    /// Code's raised limits) go in the remote command itself.
    #[test]
    fn an_agent_over_ssh_gets_its_effort_on_the_other_computer() {
        let env = agent_env(true, Some(crate::model::Effort::Max));
        assert!(env.contains(&("CLAUDE_CODE_EFFORT_LEVEL", "max".to_string())));
        assert!(env.contains(&("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", "0".to_string())));
        let line = remote_command("claude", vec!["-p".into(), "it's".into()], &env);
        assert!(line.starts_with("env ") && line.contains(" CLAUDE_CODE_EFFORT_LEVEL=max ") && line.ends_with(" claude -p 'it'\\''s'"), "{line}");
        assert_eq!(crate::ssh::ssh_line_args("box", line.clone()).pop(), Some(line));
        assert!(agent_env(false, None).contains(&("CLAUDE_CODE_EFFORT_LEVEL", "high".to_string())), "high unless chosen");
    }

    /// A folder under `~/` is in the other computer's home: its shell expands it, which it won't
    /// do for a quoted `~`. Anything else stays quoted.
    #[test]
    fn a_folder_over_ssh_is_in_the_other_computers_home() {
        let set = |v: &str| remote_command("claude", vec![], &[("CLAUDE_CONFIG_DIR", v.to_string())]);
        assert_eq!(set("~/.claude-work"), "env CLAUDE_CONFIG_DIR=\"$HOME\"/.claude-work claude");
        assert_eq!(set("~/Claude work"), "env CLAUDE_CONFIG_DIR=\"$HOME\"/'Claude work' claude");
        assert_eq!(set("~"), "env CLAUDE_CONFIG_DIR=\"$HOME\" claude");
        assert_eq!(set("/srv/claude/a b"), "env 'CLAUDE_CONFIG_DIR=/srv/claude/a b' claude");
        assert_eq!(set("/x/~/$(rm)"), "env 'CLAUDE_CONFIG_DIR=/x/~/$(rm)' claude");
    }

    /// A sign-in that stopped working is told apart from none by the account `.claude.json` still
    /// names; anything else there names none.
    #[test]
    fn claude_code_was_signed_in_when_its_settings_name_an_account() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        let names = |text: &[u8]| {
            std::fs::write(&file, text).unwrap();
            adapters::claude_names_account(&file)
        };
        assert!(names(br#"{"numStartups":3,"oauthAccount":{"accountUuid":"0000","emailAddress":"someone@example.com"}}"#));
        for none in [&br#"{"numStartups":3}"#[..], br#"{"oauthAccount":null}"#, br#"{"oauthAccount":"x"}"#, b"not json", b"", b"[1,2]"] {
            assert!(!names(none), "{}", String::from_utf8_lossy(none));
        }
        assert!(!adapters::claude_names_account(&dir.path().join("missing.json")));
        assert!(!adapter_for("codex").signed_in_before());
    }

    /// Claude Code in a chosen folder runs, checks its sign-in and keeps its sessions there; an
    /// agent without such a folder is the same as before.
    #[test]
    fn claude_code_uses_the_folder_chosen_for_it() {
        let config = tempfile::tempdir().unwrap();
        let id = "0e4a6b1c-1111-4222-8333-944455556666";
        let folder = config.path().join("projects").join("-somewhere");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("{id}.jsonl")), "{}\n").unwrap();
        let options = crate::BackendOptions { config_dir: Some(config.path().display().to_string()), ..Default::default() };
        let claude = AgentAdapters::built_in().for_backend("claude-code", &options);
        assert_eq!(claude.session_file(id), Some(folder.join(format!("{id}.jsonl"))));
        assert!(claude.sign_in_fix().is_some_and(|f| f.contains(&config.path().display().to_string())), "signs in there");
        assert_eq!(claude.folder_var(), Some("CLAUDE_CONFIG_DIR"));
        assert!(!claude.signed_in_before(), "nothing there names an account");
        std::fs::write(config.path().join(".claude.json"), r#"{"oauthAccount":{"accountUuid":"0000"},"projects":{}}"#).unwrap();
        assert!(claude.signed_in_before(), "its own folder's .claude.json, not the usual one");
        let codex = AgentAdapters::built_in().for_backend("codex", &options);
        assert_eq!((codex.folder_var(), codex.ids()), (None, adapter_for("codex").ids()));
        assert_eq!(crate::config::config_dir_in("~/a/b", Some("/home/u".into())), Some(std::path::PathBuf::from("/home/u/a/b")));
        assert_eq!(crate::config::config_dir_in("~", Some("/home/u".into())), Some(std::path::PathBuf::from("/home/u")));
        assert_eq!(crate::config::config_dir_in("~/a", None), None, "no home to put it in");
        assert_eq!(crate::config::config_dir_in("/srv/c", None), Some(std::path::PathBuf::from("/srv/c")));
    }
    use crate::BackendKind;

    /// Explicit OpenCode picks cannot pass ACP preflight then run natively without our MCP server.
    #[test]
    fn explicit_opencode_picks_are_refused_before_native_chat_or_build() {
        let adapter = adapter_for("opencode");
        let defaults = BackendOptions { acp: Some(true), ..Default::default() };
        assert!(selected_acp_args(adapter.as_ref(), "opencode", &defaults).is_some());
        for (model, thinking) in [(Some("test/model"), None), (None, Some(crate::Effort::High)), (Some("test/model"), Some(crate::Effort::Max))] {
            for acp in [false, true] {
                let options = BackendOptions {
                    command: Some("missing-test-opencode".into()),
                    cli_model: model.map(str::to_string),
                    cli_thinking: thinking,
                    acp: Some(acp),
                    ..Default::default()
                };
                let refused = check_builder_tools(adapter.as_ref(), "opencode", &options, &CancelToken::default()).unwrap_err();
                assert_eq!(refused.code, ErrorCode::BadSettings);
                assert!(refused.message.contains("cannot yet connect"));
                for host in [None, Some("test-host")] {
                    for building in [false, true] {
                        let config = BackendConfig {
                            name: "OpenCode".into(),
                            kind: BackendKind::Nvidia,
                            model: "opencode".into(),
                            base_url: None,
                            ssh_host: host.map(str::to_string),
                            enabled: true,
                        };
                        let mut spec = CliSpec::new(config, options.clone(), false, None);
                        if building {
                            spec.builder = Some(Builder { investigation: None, inbox: PathBuf::from("test-inbox") });
                        }
                        assert!(acp_for(&spec).is_none());
                        let mut c = if building { builder_conversation(spec, Arc::new(|_, _| false)) } else { conversation(spec) };
                        c.send("test");
                        let deadline = std::time::Instant::now() + Duration::from_secs(5);
                        let mut events = Vec::new();
                        while !events.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
                            assert!(std::time::Instant::now() < deadline, "missing terminal event: {events:?}");
                            events.extend(c.poll());
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        assert!(!c.is_busy());
                        events.extend(c.poll());
                        assert!(
                            matches!(events.as_slice(), [
                            AgentEvent::Error { message, remediation: Some(_), log: None },
                            AgentEvent::TurnDone { backend, input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false }
                        ] if message == &refused.message && backend == "OpenCode"),
                            "{events:?}"
                        );
                    }
                }
            }
        }
    }

    /// Validation failures finish exactly once, preserving cancellation status and other error details.
    #[test]
    fn cancelled_model_validation_reports_stopped_and_other_failures_stay_errors() {
        struct RefusedModel(ErrorCode);
        impl AgentAdapter for RefusedModel {
            /// Register the fixture without replacing any built-in agent.
            fn ids(&self) -> &'static [&'static str] {
                &["test-validation"]
            }
            /// Fail the test if a rejected model reaches command construction.
            fn args(&self, _cx: &ArgsCx) -> Vec<String> {
                panic!("failed validation must not start the agent")
            }
            /// Return the selected validation failure without performing I/O.
            fn validate_model(&self, _config: &BackendConfig, _options: &BackendOptions, _directory: Option<&Path>, _stop: &Stop) -> Result<(), ToolError> {
                Err(ToolError::new(self.0, "Validation stopped").with_remediation("Reload the models."))
            }
        }
        for code in [ErrorCode::Cancelled, ErrorCode::BadInput, ErrorCode::TimedOut] {
            let config = BackendConfig {
                name: "Test".into(),
                kind: BackendKind::Nvidia,
                model: "test-validation".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            };
            let mut spec = CliSpec::new(config, BackendOptions::default(), false, None);
            spec.adapters.register(Arc::new(RefusedModel(code)));
            let mut conversation = CliConversation::new(spec);
            conversation.send("test");
            let events: Vec<_> = (0..2).map(|_| conversation.rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            assert!(!conversation.is_busy(), "the next message is accepted before TurnDone");
            assert!(conversation.poll().is_empty(), "exactly one terminal event");
            if code == ErrorCode::Cancelled {
                assert!(matches!(events.as_slice(), [AgentEvent::Status { text }, AgentEvent::TurnDone { .. }] if text == "Stopped."), "{events:?}");
            } else {
                assert!(
                    matches!(events.as_slice(), [AgentEvent::Error { message, remediation: Some(help), log: None }, AgentEvent::TurnDone { .. }]
                    if message == "Validation stopped" && help == "Reload the models."),
                    "{events:?}"
                );
            }
            assert!(
                matches!(events.last(), Some(AgentEvent::TurnDone {
                backend, input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false,
            }) if backend == "Test"),
                "{events:?}"
            );
        }
    }

    fn mcp() -> McpCommand {
        McpCommand { command: "/opt/ca/craft-anything-cli".into(), args: vec!["mcp".into(), "--project".into(), "/p/x.cany".into()], notes: None }
    }

    #[test]
    fn claude_args() {
        let a = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "-what?", "PRE", &mcp(), Some("sess-1"), Access::Analyse);
        assert_eq!(a[0], "-p");
        // #402: the message goes in on stdin, so a long one can't pass Windows' command-line limit.
        assert!(adapter_for("claude-code").prompt_on_stdin() && !a.iter().any(|x| x.contains("what?")), "{a:?}");
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert_eq!(a[pos("--output-format") + 1], "stream-json");
        assert!(a.contains(&"--verbose".to_string()));
        assert_eq!(a[pos("--tools") + 1], "", "built-in tools are opt-in");
        for access in [Access::Build, Access::CleanRoomBuild] {
            let builder = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "build it", "PRE", &mcp(), None, access);
            let builder_pos = builder.iter().position(|x| x == "--tools").unwrap();
            let tools = &builder[builder_pos + 1];
            assert!(tools.split(',').any(|tool| tool == "Bash"), "{access:?}: {tools}");
            assert!(tools.split(',').any(|tool| tool == "Write"), "{access:?}: {tools}");
        }
        assert_eq!(a[pos("--resume") + 1], "sess-1");
        assert_eq!(a[pos("--allowedTools") + 1], "mcp__craft-anything__*");
        assert_eq!(a[pos("--model") + 1], crate::openai::NVIDIA_DEFAULT_MODEL, "NVIDIA default model unless configured otherwise");
        let o = BackendOptions { cli_model: Some("opus".into()), ..Default::default() };
        let b = build_args(adapter_for("claude-code").as_ref(), &o, "q", "PRE", &mcp(), None, Access::Analyse);
        assert_eq!(b[b.iter().position(|x| x == "--model").unwrap() + 1], "opus");
        let cfg: Value = serde_json::from_str(&a[pos("--mcp-config") + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["craft-anything"]["args"][0], "mcp");
        // variadic flags come last: nothing after them is read as one of their values
        assert!(pos("--mcp-config") > pos("--resume"));
    }

    /// #540: only tools that ask the person themselves are approved on Codex's side.
    #[test]
    fn every_tool_approved_for_codex_is_registered_and_asks_for_consent() {
        let e = craft_anything_engine::Engine::shared(craft_anything_engine::testing::session(), crate::rebuild::tools::specs());
        for id in craft_anything_engine::AGENT_APP_PREAPPROVED_TOOLS {
            let spec = e.registry().get(id).unwrap_or_else(|| panic!("{id} isn't registered"));
            assert!(spec.effects.needs_consent(), "{id} must ask for consent itself");
        }
    }

    /// #540: a refused Codex tool call came back as "null"; it says why and how to fix it.
    #[test]
    fn a_codex_tool_refused_for_approval_explains_the_way_out() {
        let mut p = StreamParser::new(adapter_for("codex"));
        let line = r#"{"type":"item.completed","item":{"id":"i1","type":"mcp_tool_call","server":"craft-anything","tool":"rebuild_start","status":"failed","result":null,"error":{"message":"tool rebuild_start requires approval, but approval policy is never"}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let events = adapter_for("codex").parse(&mut p, &v);
        let text = format!("{events:?}");
        assert!(text.contains("setup --client codex --yes"), "{text}");
        // Another server's refusal keeps Codex's words: our setup can't fix it.
        let mut p = StreamParser::new(adapter_for("codex"));
        let theirs = line.replace("craft-anything", "other-server");
        let text = format!("{:?}", adapter_for("codex").parse(&mut p, &serde_json::from_str::<Value>(&theirs).unwrap()));
        assert!(text.contains("approval policy is never") && !text.contains("setup --client codex"), "{text}");
        let mut p = StreamParser::new(adapter_for("codex"));
        let v: Value = serde_json::from_str(line).unwrap();
        let events = adapter_for("codex").parse(&mut p, &v);
        let text = format!("{events:?}");
        assert!(!text.contains("\"null\""), "{text}");
        // Any other failure shows its own message, not the JSON around it.
        let mut p = StreamParser::new(adapter_for("codex"));
        let other =
            r#"{"type":"item.completed","item":{"id":"i2","type":"mcp_tool_call","tool":"x","status":"failed","result":null,"error":{"message":"boom"}}}"#;
        let text = format!("{:?}", adapter_for("codex").parse(&mut p, &serde_json::from_str::<Value>(other).unwrap()));
        assert!(text.contains("boom") && !text.contains("null"), "{text}");
    }

    #[test]
    fn codex_and_gemini_args() {
        let a = build_args(adapter_for("codex").as_ref(), &BackendOptions::default(), "q", "P:", &mcp(), None, Access::Analyse);
        assert_eq!(&a[..2], &["exec".to_string(), "--json".to_string()]);
        // #69: project folders aren't git repositories; the flag comes once, also when the
        // backend's own args already have it.
        let skip = |a: &[String]| a.iter().filter(|x| *x == "--skip-git-repo-check").count();
        assert_eq!(skip(&a), 1);
        let o = BackendOptions { args: vec!["--skip-git-repo-check".into()], ..Default::default() };
        assert_eq!(skip(&build_args(adapter_for("codex").as_ref(), &o, "q", "P:", &mcp(), None, Access::Build)), 1);
        assert!(a.contains(&"mcp_servers.craft-anything.command=\"/opt/ca/craft-anything-cli\"".to_string()));
        // #538: Codex doesn't refresh its callable tools after enable_toolset, so list all permitted
        // tools from startup. Clean room still filters what the server lists and allows.
        assert!(a.contains(&"mcp_servers.craft-anything.args=[\"mcp\", \"--project\", \"/p/x.cany\", \"--all-tools\"]".to_string()));
        let mut already_all = mcp();
        already_all.args.push("--all-tools".into());
        let deep = build_args(adapter_for("codex").as_ref(), &BackendOptions::default(), "q", "P:", &already_all, None, Access::Build);
        let args = deep.iter().find(|a| a.starts_with("mcp_servers.craft-anything.args=")).unwrap();
        assert_eq!(args.matches("--all-tools").count(), 1, "a Deep build already lists every tool");
        // #528: the message goes in on stdin; `-` tells `codex exec` to read it there.
        assert_eq!(a.last().unwrap(), "-");
        // #540: `codex exec` can't ask, so the tools that ask for themselves are approved in the
        // same -c list, and nothing wider.
        assert!(a.contains(&"mcp_servers.craft-anything.tools.rebuild_start.approval_mode=\"approve\"".to_string()));
        assert!(!a.iter().any(|x| x.contains("default_tools_approval_mode")));
        let g = build_args(adapter_for("gemini-cli").as_ref(), &BackendOptions::default(), "q", "", &mcp(), None, Access::Analyse);
        assert!(g.windows(2).any(|w| w[0] == "--output-format" && w[1] == "stream-json"));
        let o = BackendOptions { args: vec!["run".into()], ..Default::default() };
        assert_eq!(build_args(adapter_for("aider").as_ref(), &o, "q", "", &mcp(), None, Access::Analyse), vec!["run".to_string(), "q".to_string()]);
    }

    #[test]
    fn codex_long_prompt_goes_in_on_stdin() {
        // #528: a long message in argv passes Windows' command-line limit (os error 206).
        let codex = adapter_for("codex");
        let prompt = "x".repeat(40_831);
        let a = build_args(codex.as_ref(), &BackendOptions::default(), &prompt, "P:", &mcp(), None, Access::Build);
        assert!(codex.prompt_on_stdin(), "codex must read its message from stdin");
        assert!(!a.iter().any(|x| x.contains(&prompt) || x.contains("P:")), "{a:?}");
        assert!(a.iter().map(|x| x.len() + 1).sum::<usize>() < 4096, "argv stays short");
        assert_eq!(a.last().unwrap(), "-", "codex exec reads stdin when told to");
        assert_eq!(&a[..2], &["exec".to_string(), "--json".to_string()]);
        assert!(a.contains(&"--skip-git-repo-check".to_string()));
        assert!(a.windows(2).any(|w| w[0] == "--sandbox" && w[1] == "workspace-write"));
        assert!(a.contains(&"mcp_servers.craft-anything.command=\"/opt/ca/craft-anything-cli\"".to_string()));
        assert!(a.contains(&"mcp_servers.craft-anything.args=[\"mcp\", \"--project\", \"/p/x.cany\", \"--all-tools\"]".to_string()));
    }

    #[test]
    fn gemini_long_prompt_goes_in_on_stdin() {
        // npm starts Gemini CLI through `gemini.cmd`, where cmd.exe allows 8,191 characters.
        let gemini = adapter_for("gemini-cli");
        let prompt = "x".repeat(40_831);
        let o = BackendOptions { cli_model: Some("gemini-2.5-pro".into()), ..Default::default() };
        let a = build_args(gemini.as_ref(), &o, &prompt, "P:", &mcp(), None, Access::Analyse);
        assert!(gemini.prompt_on_stdin(), "gemini must read its message from stdin");
        assert_eq!(gemini.stdin_text("P:", &prompt), format!("P:{prompt}"), "the whole message, preamble first");
        assert!(!a.iter().any(|x| x.contains(&prompt) || x.contains("P:")), "{a:?}");
        assert!(a.iter().map(|x| x.len() + 1).sum::<usize>() < 4096, "argv stays short");
        // Gemini CLI appends `-p` to stdin; empty, it adds nothing but keeps the run headless.
        assert_eq!(&a[a.len() - 2..], &["-p".to_string(), String::new()]);
        assert!(a.windows(2).any(|w| w[0] == "-m" && w[1] == "gemini-2.5-pro"));
        assert!(a.windows(2).any(|w| w[0] == "--output-format" && w[1] == "stream-json"));
    }

    #[test]
    fn nested_session_variables_are_dropped_but_user_settings_kept() {
        let v = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert!(host_session_vars(v(&["CLAUDE_CODE_ENTRYPOINT", "PATH"])).is_empty(), "outside a session nothing is touched");
        let mut dropped = host_session_vars(v(&[
            "CLAUDECODE",
            "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_AGENT_SDK_VERSION",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "PATH",
        ]));
        dropped.sort();
        assert_eq!(dropped, ["CLAUDECODE", "CLAUDE_AGENT_SDK_VERSION", "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH", "CLAUDE_CODE_SESSION_ID"]);
    }

    fn failed(code: i32, stderr: &str) -> craft_anything_platform::Output {
        craft_anything_platform::Output {
            code: Some(code),
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration: Duration::from_millis(400),
        }
    }

    fn context(host: Option<&str>, logs: Option<PathBuf>) -> FailureContext {
        let config = BackendConfig {
            name: "Mine".into(),
            kind: BackendKind::Nvidia,
            model: "claude-code".into(),
            base_url: None,
            ssh_host: host.map(str::to_string),
            enabled: true,
        };
        let spec = CliSpec { logs, ..CliSpec::new(config, BackendOptions::default(), false, None) };
        let args = ["-p".to_string(), "PRE private plans for my app".to_string(), "--verbose".to_string()];
        FailureContext::new("claude", &args, "private plans for my app", host, &spec)
    }

    #[test]
    fn a_failed_ssh_run_says_what_happened_and_saves_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let ev = context(Some("github.com"), Some(dir.path().to_path_buf())).exit_event("claude-code", &failed(255, "exec request failed on channel 0\n"));
        let AgentEvent::Error { message, remediation, log } = ev else { panic!("not an error") };
        let help = remediation.unwrap();
        assert!(message.contains("Couldn't run Claude Code on github.com: github.com is a git server"), "{message}");
        assert!(help.contains("exec request failed on channel 0") && help.contains("ssh -o BatchMode=yes -T github.com -- claude --version"), "{help}");
        assert!(!help.contains("by hand") && !help.contains("below"), "{help}");
        let text = std::fs::read_to_string(log.unwrap()).unwrap();
        for part in ["Exit code: 255", "exec request failed on channel 0", "git server", "claude --version", "--verbose", "[your message, 24 characters]"] {
            assert!(text.contains(part), "{part} is missing from:\n{text}");
        }
        assert!(!text.contains("private plans"), "the message stays out of the log:\n{text}");
    }

    #[test]
    fn a_failed_local_run_shows_its_output_and_command() {
        let dir = tempfile::tempdir().unwrap();
        let ev = context(None, Some(dir.path().to_path_buf())).exit_event("claude-code", &failed(3, "first\nreal reason\n"));
        let AgentEvent::Error { message, remediation, log } = ev else { panic!("not an error") };
        let help = remediation.unwrap();
        assert!(message.contains("exit code 3: real reason"), "{message}");
        assert!(help.contains("first\nreal reason") && help.contains("claude -p '[your message, 24 characters]' --verbose"), "{help}");
        assert!(log.is_some());
    }

    #[test]
    fn without_a_log_folder_nothing_is_written_and_a_failed_write_is_reported() {
        let ev = context(None, None).exit_event("claude-code", &failed(1, "x"));
        assert!(matches!(ev, AgentEvent::Error { log: None, .. }));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        let AgentEvent::Error { remediation, log, .. } = context(None, Some(file)).exit_event("claude-code", &failed(1, "x")) else { panic!("not an error") };
        assert!(log.is_none());
        assert!(remediation.unwrap().contains("The details couldn't be saved"));
    }

    #[test]
    fn sign_in_failures_get_a_plain_fix() {
        let mut p = StreamParser::new(adapter_for("claude-code"));
        let ev = p.line(&json!({"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate: OAuth session expired"}).to_string());
        match &ev[0] {
            AgentEvent::Error { message, remediation, .. } => {
                assert_eq!(message, "Claude Code stopped: Failed to authenticate: OAuth session expired");
                assert!(remediation.as_deref().unwrap_or_default().contains("claude auth login"));
            }
            other => panic!("{other:?}"),
        }
        assert!(sign_in_hint("rate limited").is_none());
        // #165: an install too old for the model says so, and how to update it.
        let raw =
            r#"API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"Claude Code 2.1.81 does not support this model; version 2"}}"#;
        let (message, fix) = claude_code_failure(raw);
        assert!(message.starts_with("Claude Code is too old for the model it was asked to use") && message.contains("2.1.81"), "{message}");
        let fix = fix.unwrap_or_default();
        assert!(fix.contains("claude update") && fix.contains("desktop app") && fix.contains("Settings → AI"), "{fix}");
        assert_eq!(claude_code_failure("rate limited"), ("Claude Code stopped: rate limited".to_string(), None));
    }

    #[test]
    fn builder_args_route_permissions_and_questions_to_us() {
        let a = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "build it", "PRE", &mcp(), None, Access::Build);
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert_eq!(a[pos("--permission-prompt-tool") + 1], "mcp__craft-anything__core_action_approve");
        assert_eq!(a[pos("--disallowedTools") + 1], "AskUserQuestion");
        assert!(a[pos("--allowedTools")..].contains(&"Read".to_string()));
        assert!(!a.iter().any(|x| x == "Write" || x == "Bash"), "writes and commands always go through the approval tool");
        let analysis = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "q", "PRE", &mcp(), None, Access::Analyse);
        assert!(!analysis.contains(&"--permission-prompt-tool".to_string()));
        // A local builder gets the hook that hands over notes at its next step; analysis runs don't.
        assert!(!a.contains(&"--settings".to_string()), "no notes folder, no hook");
        let local = McpCommand { notes: Some(PathBuf::from(r"C:\p\inbox\notes")), ..mcp() };
        let b = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "build it", "PRE", &local, None, Access::Build);
        let settings: serde_json::Value = serde_json::from_str(&b[b.iter().position(|x| x == "--settings").unwrap() + 1]).unwrap();
        let hook = &settings["hooks"]["PostToolUse"][0];
        assert_eq!(hook["matcher"], "*");
        assert_eq!(hook["hooks"][0]["command"], "\"/opt/ca/craft-anything-cli\" builder-notes \"C:/p/inbox/notes\"");
        let no_hook = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "q", "PRE", &local, None, Access::Analyse);
        assert!(!no_hook.contains(&"--settings".to_string()));
        assert!(McpCommand::local_or_remote(true, None, Some(PathBuf::from("/p/inbox"))).notes.is_none(), "a remote builder can't reach this computer's files");
        let m = McpCommand::local_or_remote(false, Some("/p/inv.cany".into()), Some("/p/inbox".into()));
        assert!(m.args.windows(2).any(|w| w[0] == "--inbox" && w[1] == "/p/inbox"));
        assert!(!McpCommand::local_or_remote(true, None, Some("/p/inbox".into())).args.contains(&"--inbox".to_string()));
    }

    /// #564: in Clean room a chat's reads aren't left to Claude Code's defaults. Its own file, shell
    /// and web tools are refused outright (a deny rule beats any allow), so only our tool server,
    /// which keeps the original out of reach, is left; Deep mode is unchanged.
    #[test]
    fn a_clean_room_chat_refuses_the_agents_own_file_tools_outright() {
        let args = |access| build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "q", "PRE", &mcp(), None, access);
        let clean = args(Access::CleanRoomAnalyse);
        let denied = &clean[clean.iter().position(|x| x == "--disallowedTools").expect("a deny list") + 1..];
        for t in ["Read", "Glob", "Grep", "LS", "NotebookRead", "Write", "Edit", "Bash", "PowerShell", "WebFetch", "WebSearch"] {
            assert!(denied.contains(&t.to_string()), "{t} is refused: {denied:?}");
        }
        let allowed = &clean[clean.iter().position(|x| x == "--allowedTools").unwrap() + 1..clean.iter().position(|x| x == "--disallowedTools").unwrap()];
        assert_eq!(allowed, ["mcp__craft-anything__*"], "only our tools are allowed");
        let deep = args(Access::Analyse);
        assert!(!deep.contains(&"--disallowedTools".to_string()), "Deep mode keeps its defaults: {deep:?}");
        // A Clean room question refuses reading too (it used to leave that to the default).
        let q = args(Access::CleanRoomQuestion);
        let denied = &q[q.iter().position(|x| x == "--disallowedTools").unwrap() + 1..];
        for t in ["Read", "Glob", "Grep", "LS", "NotebookRead", "Write", "Bash"] {
            assert!(denied.contains(&t.to_string()), "{t} is refused in a Clean room question");
        }
        assert!(!q.contains(&"--allowedTools".to_string()));
    }

    /// #564: chats take their access from the mode, as builds and questions do.
    #[test]
    fn a_run_gets_its_access_from_what_it_is_and_the_mode() {
        assert_eq!(access_for(false, true, false), Access::Analyse);
        assert_eq!(access_for(false, false, false), Access::CleanRoomAnalyse);
        assert_eq!(access_for(true, true, false), Access::Build);
        assert_eq!(access_for(true, false, false), Access::CleanRoomBuild);
        assert_eq!(access_for(false, true, true), Access::Question);
        assert_eq!(access_for(false, false, true), Access::CleanRoomQuestion);
        assert_eq!(access_for(true, false, true), Access::CleanRoomQuestion);
    }

    /// #564: an ACP chat in Clean room refuses the agent's own reads, writes, commands and fetches
    /// instead of putting them to the person.
    #[test]
    fn a_clean_room_acp_chat_refuses_the_agents_own_actions() {
        let approve = acp::clean_room_chat_approver();
        for tool in ["Read", "Write", "Bash", "WebFetch"] {
            assert!(!approve(tool, &serde_json::json!({"file_path": "/original/secret.txt"})), "{tool}");
        }
        assert!(approve("TodoWrite", &serde_json::Value::Null));
    }

    #[test]
    fn a_question_reads_a_copy_of_the_builders_session_and_changes_nothing() {
        let q = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "where?", "PRE", &mcp(), Some("sess-1"), Access::Question);
        let has = |a: &[String], x: &str| a.iter().any(|y| y == x);
        assert!(q.windows(3).any(|w| w[0] == "--resume" && w[1] == "sess-1" && w[2] == "--fork-session"), "a copy, never the session itself: {q:?}");
        assert!(has(&q, "--no-session-persistence") && has(&q, "--strict-mcp-config"));
        assert!(!has(&q, "--mcp-config") && !has(&q, "--permission-prompt-tool"), "no tool server: {q:?}");
        let refused = &q[q.iter().position(|x| x == "--disallowedTools").unwrap()..];
        for t in ["Write", "Edit", "Bash", "PowerShell", "WebFetch"] {
            assert!(has(refused, t), "{t} is refused");
        }
        let allowed = &q[q.iter().position(|x| x == "--allowedTools").unwrap()..];
        assert_eq!(&allowed[1..], ["Read", "Glob", "Grep"]);
        // Clean room: no file tools at all; no session: a fresh one, still not kept.
        let clean = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "where?", "PRE", &mcp(), None, Access::CleanRoomQuestion);
        assert!(!has(&clean, "--allowedTools") && !has(&clean, "--fork-session") && has(&clean, "--no-session-persistence"));
        // Only agents that can be held to reading are asked.
        assert!(adapter_for("claude-code").answers_questions());
        assert!(!adapter_for("codex").answers_questions() && !adapter_for("gemini-cli").answers_questions() && !adapter_for("aider").answers_questions());
    }

    #[test]
    fn a_clean_room_builder_reads_through_us_and_a_deep_one_sees_every_tool() {
        let clean = build_args(adapter_for("claude-code").as_ref(), &BackendOptions::default(), "build it", "PRE", &mcp(), None, Access::CleanRoomBuild);
        let allowed = &clean[clean.iter().position(|x| x == "--allowedTools").unwrap()..];
        assert!(allowed.contains(&"TodoWrite".to_string()));
        assert!(!["Read", "Glob", "Grep"].iter().any(|t| allowed.contains(&t.to_string())), "every read goes through the approval tool: {allowed:?}");
        let spec = |deep, builder: bool| CliSpec {
            deep,
            builder: builder.then(|| Builder { investigation: None, inbox: PathBuf::from("/p/inbox") }),
            ..CliSpec::new(
                BackendConfig { name: "Claude".into(), kind: BackendKind::Nvidia, model: "claude-code".into(), base_url: None, ssh_host: None, enabled: true },
                BackendOptions::default(),
                false,
                None,
            )
        };
        let args = |deep, builder| mcp_for(&spec(deep, builder), false).args;
        assert!(args(true, true).contains(&"--all-tools".to_string()) && args(true, true).contains(&"--deep".to_string()));
        assert!(!args(false, true).contains(&"--all-tools".to_string()) && args(false, true).contains(&"--clean-room".to_string()));
        // A chat's server runs in the chat's mode too, whatever was saved.
        assert!(args(true, false).contains(&"--deep".to_string()) && !args(true, false).contains(&"--all-tools".to_string()));
        assert!(args(false, false).contains(&"--clean-room".to_string()));
    }

    #[test]
    fn parses_claude_stream() {
        let ev = format!("ev_{}", "a".repeat(64));
        let lines = [
            json!({"type":"system","subtype":"init","session_id":"s1"}).to_string(),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Looking"},{"type":"tool_use","id":"t1","name":"mcp__craft-anything__core_session_show","input":{}}]}}).to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":format!("{{\"evidence_id\":\"{ev}\"}}")}]}]}}).to_string(),
            json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.12,"usage":{"input_tokens":5,"cache_read_input_tokens":5,"output_tokens":7}}).to_string(),
            "not json warning".to_string(),
        ];
        let mut p = StreamParser::new(adapter_for("claude-code"));
        let events: Vec<AgentEvent> = lines.iter().flat_map(|l| p.line(l)).collect();
        assert_eq!(p.session_id.as_deref(), Some("s1"));
        assert!(matches!(&events[0], AgentEvent::TextDelta { text } if text == "Looking"));
        assert!(matches!(&events[1], AgentEvent::ToolStarted { tool, .. } if tool == "core_session_show"));
        assert!(matches!(&events[2], AgentEvent::ToolFinished { tool, ok: true, evidence_id: Some(id), .. } if tool == "core_session_show" && *id == ev));
        assert!(matches!(&events[3], AgentEvent::Status { .. }));
        assert_eq!((p.input_tokens, p.output_tokens, p.cost_usd), (10, 7, Some(0.12)));
    }

    /// #446: a project copied from another computer names a session this one doesn't have. The
    /// result line is Claude Code 2.1's own.
    #[test]
    fn a_session_that_is_not_on_this_computer_is_told_apart() {
        let missing = json!({"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"session_id":"39b5b312-5678-1234-961c-32112666ffab","total_cost_usd":0,"errors":["No conversation found with session ID: 39b5b312-5678-1234-961c-32112666ffab"]});
        let mut p = StreamParser::new(adapter_for("claude-code"));
        let events = p.line(&missing.to_string());
        assert!(p.session_missing);
        let [AgentEvent::Error { message, remediation: Some(fix), .. }] = events.as_slice() else { panic!("{events:?}") };
        assert!(message.contains("isn't on this computer") && fix.contains("Ask again"), "{message} / {fix}");
        // Any other failure is still that failure.
        let other = json!({"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":3,"errors":["Something else"]});
        let mut p = StreamParser::new(adapter_for("claude-code"));
        let events = p.line(&other.to_string());
        assert!(!p.session_missing);
        assert!(matches!(events.as_slice(), [AgentEvent::Error { message, .. }] if message.contains("error_during_execution")), "{events:?}");
    }

    /// #451: Claude Code's session is found wherever it ran, and only by a real id.
    #[test]
    fn a_claude_code_session_is_found_by_its_id_alone() {
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("projects").join("-Users-someone-Documents-Craft-Anything-notes");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("0e4a6b1c-1111-4222-8333-944455556666.jsonl"), "{}\n").unwrap();
        std::fs::write(config.path().join("projects").join("stray.jsonl"), "{}\n").unwrap();
        let found = adapters::claude_session_file(config.path(), "0e4a6b1c-1111-4222-8333-944455556666");
        assert_eq!(found, Some(folder.join("0e4a6b1c-1111-4222-8333-944455556666.jsonl")));
        assert_eq!(adapters::claude_session_file(config.path(), "1b70e5c4-0000-0000-0000-000000000000"), None);
        for bad in ["", "../stray", "a/b", &"a".repeat(65)] {
            assert_eq!(adapters::claude_session_file(config.path(), bad), None, "{bad:?}");
        }
        assert_eq!(adapters::claude_session_file(&config.path().join("nowhere"), "0e4a6b1c-1111-4222-8333-944455556666"), None);
    }

    /// #451: a session kept in the project is put back where Claude Code finds it, once, and
    /// only as the session it was.
    #[test]
    fn a_kept_claude_code_session_is_put_back_on_another_computer() {
        let config = tempfile::tempdir().unwrap();
        let id = "0e4a6b1c-1111-4222-8333-944455556666";
        let saved = format!("{{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"content\":\"Make a notes app.\"}}}}\n");
        let cwd = Path::new("/home/someone/Craft Anything/notes.app");
        assert!(adapters::claude_restore(config.path(), id, saved.as_bytes(), cwd).unwrap());
        let put = config.path().join("projects").join("-home-someone-Craft-Anything-notes-app").join(format!("{id}.jsonl"));
        assert_eq!(std::fs::read_to_string(&put).unwrap(), saved);
        assert_eq!(adapters::claude_session_file(config.path(), id), Some(put.clone()), "Claude Code finds it by its id");
        // There already: left as it is (it may have moved on since the copy was kept).
        std::fs::write(&put, "newer").unwrap();
        assert!(!adapters::claude_restore(config.path(), id, saved.as_bytes(), cwd).unwrap());
        assert_eq!(std::fs::read_to_string(&put).unwrap(), "newer");
        // A copy of another session, or an id that isn't one, puts nothing back.
        let other = "1b70e5c4-0000-0000-0000-000000000000";
        assert!(adapters::claude_restore(config.path(), other, saved.as_bytes(), cwd).unwrap_err().message.contains("another session"));
        assert!(adapters::claude_restore(config.path(), "../../x", saved.as_bytes(), cwd).is_err());
        assert_eq!(adapters::claude_session_file(config.path(), other), None);
        assert!(!adapter_for("codex").restore_session(id, saved.as_bytes(), cwd).unwrap());
    }

    /// #451: a kept session read back as a handoff: what was asked and answered, nothing else.
    /// The line shapes are Claude Code 2.1's.
    #[test]
    fn a_kept_claude_code_session_reads_back_as_its_requests_and_replies() {
        let lines = [
            json!({"type":"queue-operation","operation":"enqueue","content":"queued copy"}),
            json!({"type":"user","isSidechain":false,"message":{"role":"user","content":"Make a notes app."}}),
            json!({"type":"assistant","isSidechain":false,"message":{"content":[{"type":"thinking","thinking":"secret plan"}]}}),
            json!({"type":"assistant","isSidechain":false,"message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            json!({"type":"user","isSidechain":false,"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"app/"}]}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"side task"}]}}),
            json!({"type":"assistant","isSidechain":false,"message":{"content":[{"type":"text","text":"Built a notes app with a sidebar."}]}}),
            json!({"type":"assistant","isSidechain":false,"message":{"content":[{"type":"text","text":"No response requested."}]}}),
            json!({"type":"user","isSidechain":false,"message":{"content":"The person asks: \"make it blue\""}}),
            json!({"type":"assistant","isSidechain":false,"message":{"content":[{"type":"text","text":"Made it blue."}]}}),
        ];
        let saved = lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\nnot json\n";
        let all = adapters::claude_handoff(&saved, 10_000).unwrap();
        assert_eq!(
            all,
            "Craft Anything asked you: Make a notes app.\n\nYou answered: Built a notes app with a sidebar.\n\nCraft Anything asked you: The person asks: \"make it blue\"\n\nYou answered: Made it blue."
        );
        // Short of room, the newest is kept.
        let short = adapters::claude_handoff(&saved, 80).unwrap();
        assert!(short.ends_with("You answered: Made it blue.") && !short.contains("notes app"), "{short}");
        // A first brief runs long: it's cut short.
        let long = json!({"type":"user","message":{"content":"x".repeat(10_000)}}).to_string();
        assert!(adapters::claude_handoff(&long, 10_000).unwrap().chars().count() < 1600);
        assert_eq!(adapters::claude_handoff("", 10_000), None);
        assert_eq!(adapters::claude_handoff("\u{0}garbage", 10_000), None);
        assert!(adapter_for("claude-code").handoff(&saved, 10_000).is_some() && adapter_for("codex").handoff(&saved, 10_000).is_none());
    }

    #[test]
    fn parses_codex_and_gemini_streams() {
        let mut p = StreamParser::new(adapter_for("codex"));
        let mut ev = Vec::new();
        for l in [
            json!({"type":"thread.started","thread_id":"th"}),
            json!({"type":"item.started","item":{"id":"i1","type":"mcp_tool_call","server":"craft-anything","tool":"core_target_open","arguments":{"path":"/bin/ls"}}}),
            json!({"type":"item.completed","item":{"id":"i1","type":"mcp_tool_call","tool":"core_target_open","status":"completed","result":{"ok":true}}}),
            json!({"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"It lists files."}}),
            json!({"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}),
        ] {
            ev.extend(p.line(&l.to_string()));
        }
        assert!(matches!(&ev[0], AgentEvent::ToolStarted { tool, .. } if tool == "core_target_open"));
        assert!(matches!(&ev[1], AgentEvent::ToolFinished { ok: true, .. }));
        assert!(matches!(&ev[2], AgentEvent::TextDelta { text } if text.starts_with("It lists")));
        assert!(p.saw_result && p.output_tokens == 4);
        let mut g = StreamParser::new(adapter_for("gemini-cli"));
        let ev: Vec<_> = [
            json!({"type":"message","role":"assistant","content":"Hi","delta":true}),
            json!({"type":"tool_use","tool_name":"craft-anything__core_targets_list","tool_id":"x","parameters":{}}),
            json!({"type":"tool_result","tool_id":"x","status":"error","output":"boom"}),
        ]
        .iter()
        .flat_map(|l| g.line(&l.to_string()))
        .collect();
        assert!(matches!(&ev[1], AgentEvent::ToolStarted { tool, .. } if tool == "core_targets_list"));
        assert!(matches!(&ev[2], AgentEvent::ToolFinished { ok: false, .. }));
    }

    #[cfg(unix)]
    #[test]
    fn generic_cli_runs_through_owned_process_and_streams_text() {
        // `echo` stands in for an unknown agent CLI: prompt via argv, stdout as text.
        let spec = CliSpec::new(
            BackendConfig { name: "Echo".into(), kind: BackendKind::Nvidia, model: "echo".into(), base_url: None, ssh_host: None, enabled: true },
            BackendOptions::default(),
            true,
            None,
        );
        let mut c = CliConversation::new(spec);
        c.send("hello there");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
            all.extend(c.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        let text: String = all.iter().filter_map(|e| if let AgentEvent::TextDelta { text } = e { Some(text.clone()) } else { None }).collect();
        assert!(text.contains("hello there"), "{all:?}");
        assert!(text.contains("Request"), "preamble is passed too");
    }

    #[cfg(unix)]
    #[test]
    fn a_retry_uses_the_adapter_selected_after_checking_required_flags() {
        retry_after_checking_flags(true);
    }

    #[cfg(unix)]
    #[test]
    fn a_first_retry_waits_for_the_actual_adapter_selection() {
        retry_after_checking_flags(false);
    }

    #[cfg(unix)]
    fn retry_after_checking_flags(send_first: bool) {
        use std::os::unix::fs::PermissionsExt;
        for supported in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let program = dir.path().join("agy");
            let help = if supported { "--output-format stream-json" } else { "old command options" };
            let script = format!(
                "#!/bin/sh\nd=$(dirname \"$0\")\nif [ \"$1\" = --help ]; then echo '{help}'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$d/argv\"\necho run >> \"$d/runs\"\nexit 7\n"
            );
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            let config = BackendConfig {
                name: "Antigravity".into(),
                kind: BackendKind::Nvidia,
                model: "antigravity".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            };
            let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
            let mut spec = CliSpec::new(config, options, false, None);
            spec.preamble = Some("P:".into());
            spec.resume = Some("saved".into());
            let mut c = CliConversation::new(spec);
            let finish = |c: &mut CliConversation| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut all = Vec::new();
                while !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
                    assert!(std::time::Instant::now() < deadline, "no TurnDone: {all:?}");
                    all.extend(c.poll());
                    std::thread::sleep(Duration::from_millis(10));
                }
                all
            };
            let original = "Build the requested blue header.";
            if send_first {
                c.send(original);
                let first = finish(&mut c);
                assert!(first.iter().any(|e| matches!(e, AgentEvent::Error { .. })), "{first:?}");
            }
            c.retry(original);
            let retried = finish(&mut c);
            assert_eq!(retried.iter().any(|e| matches!(e, AgentEvent::Status { text } if text.contains("plain adapter"))), !supported);
            let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
            let expected = format!("P:{}", if supported { crate::failure::CONTINUE_TURN } else { original });
            assert_eq!(argv.lines().last(), Some(expected.as_str()), "supported={supported}: {argv}");
            assert_eq!(argv.contains("--conversation\nsaved\n"), supported, "{argv}");
            assert_eq!(std::fs::read_to_string(dir.path().join("runs")).unwrap(), if send_first { "run\nrun\n" } else { "run\n" });
        }
    }

    /// #402: a 34,641-character message in argv passed Windows' command-line limit (os error 206);
    /// Claude Code reads it from stdin instead.
    #[cfg(unix)]
    #[test]
    fn claude_code_reads_a_long_message_on_stdin() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        let reply = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"read %s bytes"}]}}\n"#;
        std::fs::write(&program, format!("#!/bin/sh\nn=$(wc -c)\nprintf '{reply}' $n\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config =
            BackendConfig { name: "Claude".into(), kind: BackendKind::Nvidia, model: "claude-code".into(), base_url: None, ssh_host: None, enabled: true };
        let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
        let mut c = CliConversation::new(CliSpec::new(config, options, false, None));
        c.send(&"a".repeat(40_000));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
            all.extend(c.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        let text: String = all.iter().filter_map(|e| if let AgentEvent::TextDelta { text } = e { Some(text.clone()) } else { None }).collect();
        assert_eq!(text, "read 40000 bytes", "{all:?}");
    }

    /// #528: a stand-in `codex` gets the whole message (preamble first) on stdin and only short
    /// options in argv, for a normal message and for one longer than Windows' command line.
    #[cfg(unix)]
    #[test]
    fn codex_reads_its_message_on_stdin() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("codex");
        let script = r#"#!/bin/sh
d=$(dirname "$0")
if [ "$2" = "--help" ]; then echo "--json"; exit 0; fi
printf '%s\n' "$@" > "$d/argv"
cat > "$d/stdin"
echo '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
"#;
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        for prompt in ["q".to_string(), "x".repeat(40_831)] {
            let config =
                BackendConfig { name: "Codex".into(), kind: BackendKind::Nvidia, model: "codex".into(), base_url: None, ssh_host: None, enabled: true };
            let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
            let mut spec = CliSpec::new(config, options, false, None);
            spec.preamble = Some("P:".into());
            let mut c = CliConversation::new(spec);
            c.send(&prompt);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut all = Vec::new();
            while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
                all.extend(c.poll());
                std::thread::sleep(Duration::from_millis(10));
            }
            let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap_or_default();
            let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap_or_default();
            assert_eq!(stdin.len(), prompt.len() + 2, "{all:?}");
            assert!(stdin == format!("P:{prompt}"), "the whole message, preamble first");
            assert!(!argv.contains("P:"), "{argv}");
            assert_eq!(argv.lines().last(), Some("-"), "{argv}");
            assert_eq!(argv.lines().take(2).collect::<Vec<_>>(), ["exec", "--json"]);
        }
    }

    /// #528, on every platform: the Codex command line stays short, and a child process started
    /// with a message longer than Windows' 32,767-character limit gets all of it on stdin. The
    /// child is this test binary, running `stdin_echo_child`.
    #[test]
    fn codex_long_message_reaches_a_child_process_whole() {
        let prompt = "x".repeat(40_831);
        let codex = adapter_for("codex");
        let a = build_args(codex.as_ref(), &BackendOptions::default(), &prompt, "P:", &mcp(), None, Access::Build);
        assert!(a.iter().map(|x| x.len() + 1).sum::<usize>() < 32_767, "argv must fit Windows' command line");
        assert!(codex.prompt_on_stdin());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("received");
        let exe = std::env::current_exe().unwrap();
        let message = format!("P:{prompt}");
        let run = Process::new(exe.as_os_str())
            // The Codex argv rides along after `--` (extra name filters that match no test).
            .args(["--exact", "--test-threads=1", "--", "cli_agents::tests::stdin_echo_child"])
            .args(&a)
            .env(STDIN_ECHO_ENV, out.as_os_str())
            .stdin_bytes(message.clone())
            .timeout(Duration::from_secs(60))
            .run();
        assert!(run.is_ok(), "{run:?}");
        assert_eq!(std::fs::read_to_string(&out).unwrap_or_default(), message);
    }

    /// A stand-in `gemini.cmd`, started the way npm installs Gemini CLI on Windows, gets the whole
    /// message (preamble first) on stdin and only short options in argv, for a normal message and
    /// for one longer than cmd.exe's 8,191 characters. Its stdin goes to `stdin_echo_child`.
    #[cfg(windows)]
    #[test]
    fn gemini_cmd_reads_its_message_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("gemini.cmd");
        let exe = std::env::current_exe().unwrap();
        let script = format!(
            "@echo off\r\nif \"%~1\"==\"--help\" (echo --output-format stream-json& exit /b 0)\r\necho %*> \"%~dp0argv\"\r\nset {STDIN_ECHO_ENV}=%~dp0stdin\r\n\"{}\" --exact --test-threads=1 cli_agents::tests::stdin_echo_child >nul\r\necho {{\"type\":\"result\",\"stats\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\r\n",
            exe.display()
        );
        std::fs::write(&program, script).unwrap();
        for prompt in ["q".to_string(), "x".repeat(40_831)] {
            let _ = std::fs::remove_file(dir.path().join("stdin"));
            let config =
                BackendConfig { name: "Gemini".into(), kind: BackendKind::Nvidia, model: "gemini-cli".into(), base_url: None, ssh_host: None, enabled: true };
            let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
            let mut spec = CliSpec::new(config, options, false, None);
            spec.preamble = Some("P:".into());
            let mut c = CliConversation::new(spec);
            c.send(&prompt);
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            let mut all = Vec::new();
            while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
                all.extend(c.poll());
                std::thread::sleep(Duration::from_millis(10));
            }
            let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap_or_default();
            let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap_or_default();
            assert_eq!(stdin.len(), prompt.len() + 2, "{all:?}");
            assert!(stdin == format!("P:{prompt}"), "the whole message, preamble first");
            assert!(!argv.contains("P:"), "{argv}");
            assert!(argv.trim_end().ends_with("-p \"\""), "{argv}");
        }
    }

    const STDIN_ECHO_ENV: &str = "CRAFT_ANYTHING_TEST_STDIN_ECHO";

    /// Not a test on its own: when run as the child of the test above, writes its stdin to the
    /// file it's told to.
    #[test]
    fn stdin_echo_child() {
        if let Some(path) = std::env::var_os(STDIN_ECHO_ENV) {
            let mut got = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut got).unwrap();
            std::fs::write(path, got).unwrap();
        }
    }

    /// Whoever reads `TurnDone` may send the next message at once (a session that wasn't there is
    /// followed by a fresh start), so the conversation takes it before the turn is said to be
    /// done. The race this prevents lasts nanoseconds, too short to catch from outside, so the
    /// order is checked where it's made.
    #[test]
    fn a_turn_is_said_to_be_done_only_once_the_next_message_can_be_taken() {
        let busy = AtomicBool::new(true);
        let mut sent = None;
        end_turn(&busy, nothing_done("Claude"), |e| {
            assert!(!busy.load(Ordering::SeqCst), "TurnDone sent while still busy");
            sent = Some(e);
        });
        assert!(matches!(sent, Some(AgentEvent::TurnDone { .. })));
    }

    /// Message after message, each turn ends with `TurnDone`, with nothing refused as "still
    /// working" when the next is sent the moment the last is done.
    #[cfg(unix)]
    #[test]
    fn the_next_message_is_taken_as_soon_as_a_turn_is_done() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        let reply = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"s1"}"#;
        std::fs::write(&program, format!("#!/bin/sh\ncat >/dev/null\necho '{reply}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config =
            BackendConfig { name: "Claude".into(), kind: BackendKind::Nvidia, model: "claude-code".into(), base_url: None, ssh_host: None, enabled: true };
        let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
        let mut c = CliConversation::new(CliSpec::new(config, options, false, None));
        for round in 0..25 {
            c.send("go");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut events = Vec::new();
            loop {
                let got = c.poll();
                let done = got.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. }));
                events.extend(got);
                if done {
                    assert!(!c.is_busy(), "round {round}: the turn was said to be done while still busy");
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "round {round}: no TurnDone: {events:?}");
                std::hint::spin_loop();
            }
            assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })), "round {round}: {events:?}");
        }
    }

    /// #91: a Mac app with no CLI beside it reached Claude Code, which stopped with a stack trace.
    #[test]
    fn a_missing_or_broken_helper_is_said_plainly() {
        let stop = CancelToken::default();
        let none = cli_can_start(None, &stop).unwrap_err();
        assert!(none.message.contains("craft-anything-cli") && none.message.contains("missing"), "{none:?}");
        assert!(none.remediation.as_deref().is_some_and(|r| r.contains("Install the latest")), "{none:?}");

        let dir = tempfile::tempdir().unwrap();
        let gone = cli_can_start(Some(&dir.path().join("craft-anything-cli")), &stop).unwrap_err();
        assert!(gone.message.contains("wouldn't start"), "{gone:?}");
        assert!(gone.remediation.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_is_judged_by_whether_it_runs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = |name: &str, script: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, script).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let stop = CancelToken::default();
        assert!(cli_can_start(Some(&helper("good", "#!/bin/sh\necho 1.0\n")), &stop).is_ok());
        let bad = cli_can_start(Some(&helper("bad", "#!/bin/sh\nexit 3\n")), &stop).unwrap_err();
        assert!(bad.message.contains("exited with code 3"), "{bad:?}");
    }

    /// Stopping a build doesn't wait for a helper that hangs, and reads as stopped, not as broken.
    #[cfg(unix)]
    #[test]
    fn stopping_ends_the_check_of_a_hung_helper() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let hung = dir.path().join("hung");
        std::fs::write(&hung, "#!/bin/sh\nexec sleep 25\n").unwrap();
        std::fs::set_permissions(&hung, std::fs::Permissions::from_mode(0o755)).unwrap();
        let stop = CancelToken::default();
        let stopper = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            stopper.cancel();
        });
        let began = std::time::Instant::now();
        let e = cli_can_start(Some(&hung), &stop).unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled, "{e:?}");
        assert!(began.elapsed() < Duration::from_secs(10), "it waited for the helper: {:?}", began.elapsed());
    }

    /// Agents that are never handed the tool server build without the helper.
    #[test]
    fn only_agents_handed_the_tool_server_need_the_helper() {
        for agent in ["gemini-cli", "aider", "no-such-agent"] {
            assert!(check_builder_tools(adapter_for(agent).as_ref(), agent, &BackendOptions::default(), &CancelToken::default()).is_ok(), "{agent}");
        }
    }

    #[test]
    fn missing_cli_is_a_clear_error() {
        let spec = CliSpec::new(
            BackendConfig {
                name: "Nope".into(),
                kind: BackendKind::Nvidia,
                model: "no-such-agent-cli-xyz".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            BackendOptions::default(),
            false,
            None,
        );
        let mut c = CliConversation::new(spec);
        c.send("x");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
            all.extend(c.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(all.iter().any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("isn't installed"))), "{all:?}");
    }

    /// #199: plain `codex <prompt>` is its interactive TUI ("stdin is not a terminal"), so a
    /// failed check of `codex exec --help` keeps Codex's own adapter.
    #[test]
    fn codex_is_never_run_plain() {
        assert!(!adapter_for("codex").plain_fallback() && adapters::Plain.plain_fallback());
        let options = BackendOptions { command: Some("no-such-codex-xyz".into()), ..Default::default() };
        let config = BackendConfig { name: "Codex".into(), kind: BackendKind::Nvidia, model: "codex".into(), base_url: None, ssh_host: None, enabled: true };
        let mut c = CliConversation::new(CliSpec::new(config, options, false, None));
        c.send("What does this program do?");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentEvent::TurnDone { .. })) {
            all.extend(c.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        let said = |w: &str| all.iter().any(|e| matches!(e, AgentEvent::Status { text } if text.contains(w)));
        assert!(said("trying it anyway") && !said("plain adapter"), "{all:?}");
    }

    /// A fork's own agent: JSON lines of `{"say": …}`, registered under its id.
    struct EchoAgent;
    impl AgentAdapter for EchoAgent {
        fn ids(&self) -> &'static [&'static str] {
            &["echo-agent"]
        }
        fn args(&self, cx: &ArgsCx) -> Vec<String> {
            vec!["--json".into(), cx.prompt.to_string()]
        }
        fn parse(&self, _p: &mut StreamParser, v: &Value) -> Vec<AgentEvent> {
            v.get("say").and_then(Value::as_str).map(|t| AgentEvent::TextDelta { text: t.into() }).into_iter().collect()
        }
    }

    #[test]
    fn a_registered_adapter_drives_its_agent_and_unknown_ones_get_plain() {
        let mut reg = AgentAdapters::built_in();
        reg.register(Arc::new(EchoAgent));
        let echo = reg.for_agent("echo-agent");
        assert_eq!(build_args(echo.as_ref(), &BackendOptions::default(), "hi", "", &mcp(), None, Access::Analyse), ["--json", "hi"]);
        let mut p = StreamParser::new(echo);
        assert!(matches!(p.line(r#"{"say":"hello"}"#).as_slice(), [AgentEvent::TextDelta { text }] if text == "hello"));
        // Unknown agents stream their output as text; the built-ins keep their own adapters.
        assert!(!reg.for_agent("aider").streams_json());
        assert!(reg.for_agent("claude").resumes() && !reg.for_agent("codex").resumes() && reg.for_agent("antigravity").resumes());
        // Registering an adapter for an existing id replaces that one.
        reg.register(Arc::new(EchoAgent));
        assert_eq!(format!("{reg:?}"), r#"["claude-code", "codex", "gemini-cli", "opencode", "antigravity", "hermes", "maki", "copilot", "echo-agent"]"#);
    }

    #[test]
    fn copilot_defaults_to_local_acp_without_changing_other_agents() {
        let config =
            BackendConfig { name: "Copilot".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: None, enabled: true };
        let mut spec = CliSpec::new(config, BackendOptions::default(), false, None);
        assert!(acp_for(&spec).is_some());
        assert_eq!(program_for("copilot"), "copilot");
        assert_eq!(display_for("copilot"), "GitHub Copilot");
        spec.options.acp = Some(false);
        assert!(acp_for(&spec).is_none());
        spec.options.acp = Some(true);
        spec.config.ssh_host = Some("build-machine".into());
        assert!(acp_for(&spec).is_none());
        spec.config.ssh_host = None;
        spec.options.acp = None;
        spec.config.model = "gemini-cli".into();
        assert!(acp_for(&spec).is_none());
        spec.options.acp = Some(true);
        assert!(acp_for(&spec).is_some());
        spec.question = true;
        assert!(acp_for(&spec).is_none());
        spec.config.model = "copilot".into();
        assert!(acp_for(&spec).is_some());
    }

    #[test]
    fn mcp_target_handoff_is_bounded_and_keeps_the_active_target_last() {
        use craft_anything_engine::{TargetKind, target::TargetContext};
        let directory = tempfile::tempdir().unwrap();
        for count in [0, 128, 129, 130] {
            let config =
                BackendConfig { name: "Test".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: None, enabled: true };
            let mut spec = CliSpec::new(config, BackendOptions::default(), false, None);
            spec.targets = (0..count)
                .map(|index| TargetContext {
                    path: directory.path().join(format!("target-{index}")),
                    sha256: format!("{index:064x}"),
                    kind: TargetKind::Elf,
                    raw: None,
                })
                .collect();
            let command = mcp_for(&spec, false);
            let shared: Vec<TargetContext> =
                command.args.windows(2).filter(|pair| pair[0] == "--target").map(|pair| serde_json::from_str(&pair[1]).unwrap()).collect();
            assert_eq!(shared.len(), count.min(128));
            assert_eq!(
                shared.iter().map(|target| &target.path).collect::<Vec<_>>(),
                spec.targets.iter().skip(count.saturating_sub(128)).map(|target| &target.path).collect::<Vec<_>>()
            );
            assert_eq!(shared.last().map(|target| &target.path), spec.targets.last().map(|target| &target.path));
            assert!(!mcp_for(&spec, true).args.iter().any(|argument| argument == "--target"));
        }
    }

    #[test]
    fn copilot_never_falls_back_to_native_or_ssh_execution() {
        for host in [None, Some("build-machine".to_string())] {
            let config =
                BackendConfig { name: "Copilot".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: host, enabled: true };
            let options = BackendOptions { acp: Some(false), command: Some("no-such-copilot-xyz".into()), ..Default::default() };
            let spec = CliSpec::new(config, options, false, None);
            let (sender, receiver) = std::sync::mpsc::channel();
            let state = RunState { session: Arc::default(), verified: Arc::default(), lost: Arc::default(), failure: Arc::default() };
            let done = run_cli(&spec, "hello", false, &sender, &Stop::default(), &state);
            let mut output: Vec<_> = receiver.try_iter().collect();
            output.push(done);
            assert!(matches!(&output[..], [AgentEvent::Error { message, .. }, AgentEvent::TurnDone { .. }] if message.contains("local ACP")));
        }
    }

    #[test]
    fn acp_only_refusals_use_the_registered_adapters_sign_in_guidance() {
        struct AcpOnlyAgent(Option<&'static str>);
        impl AgentAdapter for AcpOnlyAgent {
            fn ids(&self) -> &'static [&'static str] {
                &["test-acp-agent"]
            }
            fn args(&self, _context: &ArgsCx) -> Vec<String> {
                Vec::new()
            }
            fn requires_acp(&self) -> bool {
                true
            }
            fn sign_in_fix(&self) -> Option<String> {
                self.0.map(str::to_string)
            }
        }
        for guidance in [None, Some("Use test-agent sign-in.")] {
            let config = BackendConfig {
                name: "Test agent".into(),
                kind: BackendKind::Nvidia,
                model: "test-acp-agent".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            };
            let mut spec = CliSpec::new(config, BackendOptions::default(), false, None);
            spec.adapters.register(Arc::new(AcpOnlyAgent(guidance)));
            let (sender, receiver) = std::sync::mpsc::channel();
            let state = RunState { session: Arc::default(), verified: Arc::default(), lost: Arc::default(), failure: Arc::default() };
            let done = run_cli(&spec, "hello", false, &sender, &Stop::default(), &state);
            let mut output: Vec<_> = receiver.try_iter().collect();
            output.push(done);
            let [AgentEvent::Error { remediation: Some(fix), .. }, AgentEvent::TurnDone { .. }] = output.as_slice() else { panic!("{output:?}") };
            assert!(fix.contains("test-acp-agent") && fix.contains("remove the SSH host") && fix.contains("do not disable ACP"), "{fix}");
            assert!(!fix.to_lowercase().contains("copilot"), "{fix}");
            assert_eq!(fix.contains("Use test-agent sign-in."), guidance.is_some());
        }
    }
}
