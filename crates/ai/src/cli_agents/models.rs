//! Model discovery through the selected agent's own CLI, in the same execution context as a run.
//! Only public model metadata is retained; provider headers and credentials are never returned.

use crate::model::{Effort, Stop};
use crate::{BackendConfig, BackendEntry, BackendOptions};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::Process;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModel {
    /// The selectable reference (provider/id), including aliases defined in OpenCode.
    pub id: String,
    pub name: String,
    pub thinking: Vec<Effort>,
}

/// A snapshot of the context a picker queried. Also used to reject stale background results.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelContext {
    pub backend: BackendEntry,
    pub directory: Option<PathBuf>,
}

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_MODELS: usize = 10_000;

#[derive(Deserialize)]
struct Response {
    data: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(rename = "providerID")]
    provider: String,
    name: String,
    enabled: bool,
    #[serde(rename = "type")]
    kind: Option<String>,
    variants: Vec<Variant>,
}

#[derive(Deserialize)]
struct Variant {
    id: String,
}

/// Describe an invalid catalogue without including potentially sensitive raw CLI output.
fn malformed() -> ToolError {
    ToolError::new(ErrorCode::GarbledOutput, "OpenCode returned an unreadable model list.")
        .with_remediation("Use OpenCode V2 on the selected computer and try loading its models again.")
}

/// Parse a bounded V2 catalogue into sorted, deduplicated enabled chat models and known depths.
/// Reject malformed references or oversized responses; discard unrelated provider metadata.
pub(crate) fn parse_models(bytes: &[u8]) -> Result<Vec<AgentModel>, ToolError> {
    if bytes.len() >= MAX_OUTPUT {
        return Err(ToolError::new(ErrorCode::OverLimit, "OpenCode's model list is too large."));
    }
    let response: Response = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    if response.data.len() > MAX_MODELS {
        return Err(ToolError::new(ErrorCode::OverLimit, "OpenCode returned too many models."));
    }
    let mut models = std::collections::BTreeMap::new();
    for m in response.data {
        if !m.enabled || m.kind.as_deref().is_some_and(|k| k != "chat") {
            continue;
        }
        if m.id.is_empty() || m.provider.is_empty() || m.id.contains('#') || m.provider.contains(['/', '#']) {
            return Err(malformed());
        }
        let id = format!("{}/{}", m.provider, m.id);
        let thinking = Effort::ALL.into_iter().filter(|d| m.variants.iter().any(|v| v.id == d.as_str())).collect();
        models.insert(id.clone(), AgentModel { id, name: m.name, thinking });
    }
    Ok(models.into_values().collect())
}

/// Only connection flags belong on both `api` and `run`; prompt/run flags never leak into discovery.
fn connection_args(options: &BackendOptions) -> Result<Vec<String>, ToolError> {
    let mut result = Vec::new();
    let mut args = options.args.iter();
    while let Some(arg) = args.next() {
        if arg == "--standalone" || arg.starts_with("--server=") {
            result.push(arg.clone());
        } else if arg == "--server" {
            let server =
                args.next().filter(|s| !s.is_empty() && !s.starts_with('-')).ok_or_else(|| ToolError::invalid("OpenCode's --server needs an address."))?;
            result.extend([arg.clone(), server.clone()]);
        }
    }
    Ok(result)
}

/// Build the model-list operation with connection flags shared by discovery and execution.
pub(crate) fn query_args(options: &BackendOptions) -> Result<Vec<String>, ToolError> {
    let mut args = vec!["api".into(), "model.list".into()];
    args.extend(connection_args(options)?);
    Ok(args)
}

/// Select the configured executable and catalogue location, quoting SSH arguments for the remote shell.
/// Local queries use the supplied directory or current directory; SSH queries expand remote `$PWD`.
fn query_command(config: &BackendConfig, options: &BackendOptions, directory: Option<&Path>) -> Result<(String, Vec<String>), ToolError> {
    let program = super::program_of(config, options);
    let mut args = query_args(options)?;
    match config.ssh_host.as_deref().filter(|h| !h.trim().is_empty()) {
        Some(host) => {
            let mut remote = vec![program];
            remote.extend(args);
            let mut args = crate::ssh::ssh_args(host, &remote);
            // Expand only the remote shell's working directory. No local path is sent to SSH.
            // The operation-id form accepts deep-object query parameters; raw `get` does not.
            if let Some(command) = args.last_mut() {
                command.push_str(" --param \"location[directory]=$PWD\"");
            }
            Ok(("ssh".into(), args))
        }
        None => {
            let directory = match directory {
                Some(dir) => dir.to_path_buf(),
                None => std::env::current_dir().map_err(|e| ToolError::io(".", e))?,
            };
            args.extend(["--param".into(), format!("location[directory]={}", directory.display())]);
            Ok((program, args))
        }
    }
}

/// Prepare an unstarted catalogue process with bounded output, deadlines and a local-only working directory.
pub(crate) fn query_process(config: &BackendConfig, options: &BackendOptions, directory: Option<&Path>) -> Result<Process, ToolError> {
    let (program, args) = query_command(config, options, directory)?;
    let mut process = Process::new(program).args(args);
    if let Some(dir) = directory.filter(|_| config.ssh_host.as_deref().is_none_or(|h| h.trim().is_empty())) {
        process = process.cwd(dir);
    }
    Ok(process.timeout(Duration::from_secs(20)).idle_timeout(Duration::from_secs(15)).stdout_cap(MAX_OUTPUT).stderr_cap(64 * 1024))
}

/// Query the selected installation, propagating cancellation, process failures and invalid catalogue errors.
pub(crate) fn discover(config: &BackendConfig, options: &BackendOptions, directory: Option<&Path>, stop: &Stop) -> Result<Vec<AgentModel>, ToolError> {
    let stop = stop.clone();
    let out = query_process(config, options, directory)?
        .cancel_when(move || stop.is_stopped())
        .remediation("Check OpenCode V2 and its provider setup on the selected computer, then reload the models.")
        .run_checked()?;
    parse_models(&out.stdout)
}

/// Validate explicit selections against the current instance before saving or starting a run.
pub(crate) fn validate(options: &BackendOptions, models: &[AgentModel]) -> Result<(), ToolError> {
    let Some(id) = options.cli_model.as_deref() else {
        return if options.cli_thinking.is_none() { Ok(()) } else { Err(ToolError::invalid("Choose a model before choosing its thinking depth.")) };
    };
    let Some(model) = models.iter().find(|m| m.id == id) else {
        return Err(ToolError::invalid(format!("The selected model {id:?} is no longer available."))
            .with_remediation("Reload the models on this computer and choose an available model or the agent's default."));
    };
    if let Some(depth) = options.cli_thinking.filter(|d| !model.thinking.contains(d)) {
        return Err(ToolError::invalid(format!("{} does not offer {} thinking.", model.id, depth.title()))
            .with_remediation("Choose a thinking depth listed for this model, or use its default."));
    }
    Ok(())
}

/// Shared by the hub and headless tools. An empty model means OpenCode chooses its own default.
pub fn select(context: &ModelContext, model: Option<&str>, thinking: Option<Effort>) -> Result<BackendOptions, ToolError> {
    select_with_stop(context, model, thinking, &Stop::default())
}

/// Return validated options for a catalogue-capable agent without persisting them.
/// Empty model names select the default; cancellation is checked before any discovery I/O.
pub fn select_with_stop(context: &ModelContext, model: Option<&str>, thinking: Option<Effort>, stop: &Stop) -> Result<BackendOptions, ToolError> {
    stop.check()?;
    let adapter = super::adapter_for(&context.backend.config.model);
    if context.backend.config.kind != crate::BackendKind::Nvidia || !adapter.has_model_catalog() {
        return Err(ToolError::invalid("This agent does not offer a live model and thinking catalogue."));
    }
    let mut options = context.backend.options.clone();
    options.cli_model = model.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    options.cli_thinking = thinking;
    if adapter.requires_acp() {
        options.cli_reasoning_effort = None;
        options.effort = None;
        if options.cli_model != context.backend.options.cli_model {
            options.cli_context = None;
        }
    }
    adapter.validate_model(&context.backend.config, &options, context.directory.as_deref(), stop)?;
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An owner's cancellation survives reset and prevents model-selection I/O.
    #[test]
    fn a_cancelled_tool_stops_selection_before_io() {
        let cancel = craft_anything_engine::CancelToken::default();
        let stop = Stop::for_tool(cancel.clone());
        cancel.cancel();
        assert!(stop.is_stopped());
        stop.reset();
        assert!(stop.is_stopped(), "a conversation reset cannot revive its cancelled owner");
        let context = ModelContext { backend: BackendEntry { config: config(), options: BackendOptions::default() }, directory: None };
        assert_eq!(select_with_stop(&context, None, None, &stop).unwrap_err().code, ErrorCode::Cancelled);
    }

    /// Build catalogue metadata with an alias distinct from its upstream model id.
    fn model(id: &str, enabled: bool, kind: &str, variants: &[&str]) -> serde_json::Value {
        json!({"id":id,"modelID":"upstream-name","providerID":"test","name":id,"enabled":enabled,"type":kind,
            "variants":variants.iter().map(|v| json!({"id":v})).collect::<Vec<_>>()})
    }

    /// Filtering retains selectable aliases and supported depths while rejecting malformed or oversized input.
    #[test]
    fn only_enabled_chat_models_and_their_actual_variants_are_offered() {
        let text = json!({"data":[
            model("alias",true,"chat", &["max","low","none","xhigh","high","medium"]),
            model("hidden",false,"chat", &["max"]), model("picture",true,"image", &[]),
            model("small",true,"chat", &["low","high"]), model("alias",true,"chat", &["low","medium","high","xhigh","max"])
        ]});
        let list = parse_models(text.to_string().as_bytes()).unwrap();
        assert_eq!(list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["test/alias", "test/small"]);
        assert_eq!(list[0].thinking, Effort::ALL);
        assert_eq!(list[1].thinking, [Effort::Low, Effort::High]);
        assert!(parse_models(br#"{"data":[]}"#).unwrap().is_empty());
        for invalid in [b"not json".as_slice(), br#"{"data":[{}]}"#, br#"{}"#] {
            assert_eq!(parse_models(invalid).unwrap_err().code, ErrorCode::GarbledOutput);
        }
        assert_eq!(parse_models(&vec![b' '; MAX_OUTPUT]).unwrap_err().code, ErrorCode::OverLimit);
    }

    /// Explicit unavailable models and unsupported depths fail instead of silently changing the selection.
    #[test]
    fn a_selection_never_falls_back_to_another_model_or_thinking_depth() {
        let list = vec![AgentModel { id: "test/model".into(), name: "Test".into(), thinking: vec![Effort::High] }];
        let mut options = BackendOptions::default();
        assert!(validate(&options, &list).is_ok());
        options.cli_thinking = Some(Effort::High);
        assert!(validate(&options, &list).is_err());
        options.cli_model = Some("test/model".into());
        assert!(validate(&options, &list).is_ok());
        options.cli_thinking = Some(Effort::Max);
        assert!(validate(&options, &list).is_err());
        options.cli_model = Some("test/removed".into());
        options.cli_thinking = None;
        assert!(validate(&options, &list).is_err());
    }

    /// An isolated local backend for catalogue fixtures.
    fn config() -> BackendConfig {
        BackendConfig { name: "OpenCode".into(), kind: crate::BackendKind::Nvidia, model: "opencode".into(), base_url: None, ssh_host: None, enabled: true }
    }

    /// Discovery preserves the chosen executable and server, omitting run-only flags and local paths over SSH.
    #[test]
    fn remote_discovery_uses_the_remote_program_and_connection_context() {
        let mut cfg = config();
        cfg.ssh_host = Some("test-box".into());
        let options = BackendOptions {
            command: Some("/opt/agent's tools/opencode".into()),
            args: ["--agent", "build", "--server", "http://localhost:4096"].map(String::from).into(),
            ..Default::default()
        };
        let (program, args) = query_command(&cfg, &options, None).unwrap();
        assert_eq!(program, "ssh");
        assert!(args.iter().any(|a| a == "BatchMode=yes"));
        assert!(args.iter().any(|a| a == "test-box"));
        assert_eq!(
            args.last().unwrap(),
            "'/opt/agent'\\''s tools/opencode' api model.list --server http://localhost:4096 --param \"location[directory]=$PWD\""
        );
        cfg.ssh_host = None;
        let (program, args) = query_command(&cfg, &options, Some(Path::new("/work/test"))).unwrap();
        assert_eq!(program, "/opt/agent's tools/opencode");
        assert_eq!(args, ["api", "model.list", "--server", "http://localhost:4096", "--param", "location[directory]=/work/test"]);
    }

    /// A shell stand-in checks remote directory expansion and quoting of paths with special characters.
    #[cfg(unix)]
    #[test]
    fn a_remote_query_expands_only_the_remote_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote & project # one");
        std::fs::create_dir(&remote).unwrap();
        let program = dir.path().join("agent's program");
        std::fs::write(&program, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = config();
        cfg.ssh_host = Some("test-box".into());
        let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
        let (_, args) = query_command(&cfg, &options, Some(Path::new("/local/ignored"))).unwrap();
        let output = Process::new("sh").arg("-c").arg(args.last().unwrap()).cwd(&remote).timeout(Duration::from_secs(2)).run_checked().unwrap();
        assert_eq!(output.stdout_text().lines().collect::<Vec<_>>(), ["api", "model.list", "--param", &format!("location[directory]={}", remote.display())]);
    }

    /// A local CLI stand-in verifies project-specific discovery, malformed output, exit errors and deadlines.
    #[cfg(unix)]
    #[test]
    fn discovery_reads_the_selected_program_in_the_project_and_reports_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake agent");
        let script = "#!/bin/sh\n[ \"$1 $2\" = 'api model.list' ] || exit 9\ncat models.json\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let options = BackendOptions { command: Some(program.display().to_string()), ..Default::default() };
        let data = json!({"data":[model("project-only",true,"chat", &["high"])]});
        std::fs::write(dir.path().join("models.json"), data.to_string()).unwrap();
        assert_eq!(discover(&config(), &options, Some(dir.path()), &Stop::default()).unwrap()[0].id, "test/project-only");
        std::fs::write(dir.path().join("models.json"), "invalid").unwrap();
        assert_eq!(discover(&config(), &options, Some(dir.path()), &Stop::default()).unwrap_err().code, ErrorCode::GarbledOutput);
        std::fs::write(&program, "#!/bin/sh\nexit 7\n").unwrap();
        assert!(discover(&config(), &options, Some(dir.path()), &Stop::default()).is_err());
        std::fs::write(&program, "#!/bin/sh\nsleep 5\n").unwrap();
        let err = query_process(&config(), &options, Some(dir.path())).unwrap().timeout(Duration::from_millis(50)).run_checked().unwrap_err();
        assert_eq!(err.code, ErrorCode::TimedOut);
    }
}
