//! OpenCode (`opencode run`). Without `run` its first argument names the folder to open, so a
//! prompt given on its own was read as a folder and every run stopped at once (#32).

use super::*;

pub struct OpenCode;

impl AgentAdapter for OpenCode {
    fn ids(&self) -> &'static [&'static str] {
        &["opencode"]
    }

    /// OpenCode V2 exposes a catalogue through its authenticated CLI.
    fn has_model_catalog(&self) -> bool {
        true
    }

    /// Native model selection cannot yet carry this run's Craft Anything MCP server.
    fn check_native_options(&self, options: &BackendOptions) -> Result<(), ToolError> {
        if options.cli_model.is_some() || options.cli_thinking.is_some() {
            return Err(ToolError::new(
                ErrorCode::BadSettings,
                "OpenCode's native model/thinking selection cannot yet connect this run's Craft Anything tools.",
            )
            .with_remediation("For a local OpenCode run, clear the model and thinking selection and enable ACP, or choose another AI. Native MCP configuration is not implemented yet."));
        }
        Ok(())
    }

    /// Discover enabled chat models and thinking variants from the selected local or SSH installation.
    fn model_catalog(
        &self,
        config: &BackendConfig,
        options: &BackendOptions,
        directory: Option<&Path>,
        stop: &Stop,
    ) -> Result<Vec<models::AgentModel>, ToolError> {
        models::discover(config, options, directory, stop)
    }

    /// Validate explicit choices against the live catalogue and refuse conflicting extra model arguments.
    /// With no selection, leave extra arguments to OpenCode and skip the catalogue query.
    fn validate_model(&self, config: &BackendConfig, options: &BackendOptions, directory: Option<&Path>, stop: &Stop) -> Result<(), ToolError> {
        if options.cli_model.is_none() && options.cli_thinking.is_none() {
            return Ok(());
        }
        // Extra flags must not silently override the selected model or its thinking variant.
        if options.args.iter().any(|a| a == "--model" || a.starts_with("--model=") || a.starts_with("-m")) {
            return Err(ToolError::invalid("Choose OpenCode's model in its model setting, rather than in extra arguments."));
        }
        models::validate(options, &self.model_catalog(config, options, directory, stop)?)
    }

    /// Build native `run` arguments, encoding an optional thinking depth as `provider/model#variant`.
    fn args(&self, cx: &ArgsCx) -> Vec<String> {
        let mut a = vec!["run".to_string()];
        // A builder works unattended: what OpenCode would stop to ask is allowed, unless its own
        // settings deny it (as Codex gets a workspace-write sandbox).
        if cx.builder {
            a.push("--auto".into());
        }
        if let Some(m) = &cx.options.cli_model {
            let model = cx.options.cli_thinking.map_or_else(|| m.clone(), |d| format!("{m}#{}", d.as_str()));
            a.extend(["--model".into(), model]);
        }
        a.extend(cx.options.args.iter().cloned());
        a.extend(["--format".into(), "json".into()]);
        a.push(format!("{}{}", cx.preamble, cx.prompt));
        a
    }

    fn required_flags(&self) -> (&'static [&'static str], &'static [&'static str]) {
        (&["run", "--help"], &["--format"])
    }

    fn plain_fallback(&self) -> bool {
        false
    }

    fn parse(&self, p: &mut StreamParser, v: &Value) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let part = v.get("part").unwrap_or(&Value::Null);
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(AgentEvent::TextDelta { text: text.into() });
                }
            }
            Some("reasoning") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(AgentEvent::Status { text: summary_of(text) });
                }
            }
            Some("tool_use") => {
                if let (Some(id), Some(name), Some(state)) =
                    (part.get("callID").and_then(Value::as_str), part.get("tool").and_then(Value::as_str), part.get("state"))
                {
                    if !p.tool_names.contains_key(id) {
                        events.push(p.started(id, name, state.get("input").cloned().unwrap_or_else(|| json!({}))));
                    }
                    match state.get("status").and_then(Value::as_str) {
                        Some("completed") => events.push(p.finished(id, true, &content_text(state.get("output").unwrap_or(&Value::Null)))),
                        Some("error") => events.push(p.finished(id, false, &content_text(state.get("error").unwrap_or(&Value::Null)))),
                        _ => {}
                    }
                }
            }
            Some("step_finish") => {
                let tokens = part.get("tokens").unwrap_or(&Value::Null);
                let count = |path| tokens.pointer(path).and_then(Value::as_u64).unwrap_or(0);
                // OpenCode's step-finish counters exclude cache from input and reasoning from
                // output, so these components make the inclusive totals the feed records.
                p.input_tokens = p.input_tokens.saturating_add(count("/input")).saturating_add(count("/cache/read")).saturating_add(count("/cache/write"));
                p.output_tokens = p.output_tokens.saturating_add(count("/output")).saturating_add(count("/reasoning"));
                if !p.cost_incomplete {
                    p.cost_usd = part.get("cost").and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0).and_then(|cost| {
                        let total = p.cost_usd.unwrap_or(0.0) + cost;
                        total.is_finite().then_some(total)
                    });
                    p.cost_incomplete = p.cost_usd.is_none();
                }
            }
            Some("error") => {
                let error = v.get("error").unwrap_or(&Value::Null);
                let data = error.get("data").unwrap_or(&Value::Null);
                let why = data.get("message").and_then(Value::as_str).unwrap_or("the service returned no error details");
                p.failure = Some(opencode_failure(error));
                p.saw_result = true;
                events.push(AgentEvent::Error { message: format!("OpenCode stopped: {why}"), remediation: None, log: None });
            }
            _ => {}
        }
        events
    }
}

/// OpenCode's JSON error carries the provider status and its own retry decision. In particular,
/// an unclassified exit or an unsupported CLI flag cannot become retryable from display text.
fn opencode_failure(error: &Value) -> TurnFailure {
    let data = error.get("data").unwrap_or(&Value::Null);
    match error.get("name").and_then(Value::as_str) {
        Some("ProviderAuthError") => TurnFailure::new(FailureKind::Authentication),
        Some("APIError") => {
            let status = data.get("statusCode").and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok());
            let mut failure = status.map_or_else(
                || TurnFailure::new(FailureKind::Unknown),
                |status| {
                    TurnFailure::from_api(&crate::http::status_error("OpenCode", status, data.get("responseBody").and_then(Value::as_str).unwrap_or_default()))
                },
            );
            if failure.kind == FailureKind::Unknown && data.get("isRetryable").and_then(Value::as_bool) == Some(true) {
                failure.kind = FailureKind::Transient;
            } else if failure.retryable() && data.get("isRetryable").and_then(Value::as_bool) == Some(false) {
                failure.kind = FailureKind::Unknown;
            }
            failure.retry_after_secs = data.get("responseHeaders").and_then(Value::as_object).and_then(|headers| {
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                    .and_then(|(_, value)| value.as_str())
                    .and_then(|n| n.trim().parse::<u64>().ok())
                    .map(|n| n.clamp(1, crate::failure::MAX_RETRY_WAIT))
            });
            failure
        }
        _ => TurnFailure::new(FailureKind::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(builder: bool, options: &BackendOptions) -> Vec<String> {
        let mcp = McpCommand { command: "craft-anything-cli".into(), args: vec!["mcp".into()], notes: None };
        OpenCode.args(&ArgsCx { options, prompt: "make a game", preamble: "PRE ", mcp: &mcp, resume: None, builder, reads_checked: false, question: false })
    }

    #[test]
    fn the_prompt_goes_to_run_after_the_flags() {
        assert_eq!(args(false, &BackendOptions::default()), ["run", "--format", "json", "PRE make a game"]);
        let options =
            BackendOptions { cli_model: Some("anthropic/claude-sonnet-4-5".into()), args: vec!["--agent".into(), "build".into()], ..Default::default() };
        assert_eq!(
            args(true, &options),
            ["run", "--auto", "--model", "anthropic/claude-sonnet-4-5", "--agent", "build", "--format", "json", "PRE make a game"]
        );
        assert!(AgentAdapters::built_in().for_agent("opencode").ids().contains(&"opencode"), "registered, not the plain adapter");
    }

    #[test]
    fn structured_errors_select_retry_without_reading_english() {
        let error = |status, retryable| json!({"name":"APIError","data":{"message":"服务限流。Please retry in 19.05s.","statusCode":status,"isRetryable":retryable,"responseHeaders":{"Retry-After":"4"}}});
        let rate = opencode_failure(&error(429, true));
        assert_eq!(rate, TurnFailure { kind: FailureKind::RateLimited, retry_after_secs: Some(4) });
        for (status, kind) in
            [(401, FailureKind::Authentication), (402, FailureKind::Billing), (400, FailureKind::Configuration), (503, FailureKind::Transient)]
        {
            assert_eq!(opencode_failure(&error(status, true)).kind, kind);
        }
        assert!(!opencode_failure(&error(429, false)).retryable(), "an explicit nonretryable response is kept");
        assert!(!opencode_failure(&json!({"name":"UnknownError","data":{"message":"Please retry in 2s"}})).retryable());
        assert_eq!(opencode_failure(&json!({"name":"ProviderAuthError","data":{"message":"重新登录"}})).kind, FailureKind::Authentication);
        assert_eq!(opencode_failure(&json!({"name":"APIError","data":{"isRetryable":true}})).kind, FailureKind::Transient);
        let quota = json!({"name":"APIError","data":{"statusCode":429,"isRetryable":true,"responseBody":"{\"error\":{\"code\":\"insufficient_quota\"}}"}});
        assert_eq!(opencode_failure(&quota).kind, FailureKind::Billing);
    }

    #[test]
    fn json_output_keeps_text_tools_and_usage_without_ending_the_process_turn() {
        let mut p = StreamParser::new(Arc::new(OpenCode));
        let lines = [
            json!({"type":"step_start","sessionID":"reported-but-not-resumable","part":{}}),
            json!({"type":"text","part":{"text":"Built a page"}}),
            json!({"type":"tool_use","part":{"callID":"write-1","tool":"write","state":{"status":"completed","input":{"path":"index.html"},"output":"wrote the page"}}}),
            json!({"type":"step_finish","part":{"tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":3,"write":1}},"cost":0.01}}),
            json!({"type":"step_finish","part":{"tokens":{"input":1,"output":2},"cost":0.02}}),
        ];
        let events: Vec<_> = lines.iter().flat_map(|v| p.line(&v.to_string())).collect();
        assert!(
            matches!(events.as_slice(), [AgentEvent::TextDelta { text }, AgentEvent::ToolStarted { tool, .. }, AgentEvent::ToolFinished { ok:true, .. }] if text=="Built a page" && tool=="write")
        );
        assert_eq!((p.input_tokens, p.output_tokens), (15, 9));
        assert_eq!(p.cost_usd, Some(0.03));
        assert!(p.session_id.is_none(), "the adapter does not pretend it can resume");
        let events = p.line(&json!({"type":"error","error":{"name":"APIError","data":{"message":"暂时失败","statusCode":429,"isRetryable":true}}}).to_string());
        assert!(matches!(events.as_slice(), [AgentEvent::Error { .. }]), "run_cli alone owns TurnDone");
        assert_eq!(p.failure.unwrap().kind, FailureKind::RateLimited);
    }

    #[test]
    fn missing_invalid_and_overflowed_costs_stay_unknown_for_the_turn() {
        let step = |cost| json!({"type":"step_finish","part":{"cost":cost,"tokens":{}}}).to_string();
        for invalid in [Value::Null, json!(-1), json!("unknown")] {
            for unknown_first in [true, false] {
                let mut p = StreamParser::new(Arc::new(OpenCode));
                let costs = if unknown_first { [invalid.clone(), json!(0.02)] } else { [json!(0.02), invalid.clone()] };
                for cost in costs {
                    p.line(&step(cost));
                }
                p.line(&step(json!(0.01)));
                assert_eq!(p.cost_usd, None, "a later known step cannot fill an earlier gap");
            }
        }
        for unknown_first in [true, false] {
            let mut missing = StreamParser::new(Arc::new(OpenCode));
            if !unknown_first {
                missing.line(&step(json!(0.02)));
            }
            missing.line(r#"{"type":"step_finish","part":{"tokens":{}}}"#);
            missing.line(&step(json!(0.01)));
            assert_eq!(missing.cost_usd, None);
        }
        let mut overflow = StreamParser::new(Arc::new(OpenCode));
        overflow.line(&step(json!(f64::MAX)));
        overflow.line(&step(json!(f64::MAX)));
        overflow.line(&step(json!(0)));
        assert_eq!(overflow.cost_usd, None);
        let mut zero = StreamParser::new(Arc::new(OpenCode));
        zero.line(&step(json!(0)));
        zero.line(&step(json!(0)));
        assert_eq!(zero.cost_usd, Some(0.0), "an explicit zero is a known cost");
    }

    /// Every supported depth reaches native chat and builder runs in the selected model reference.
    #[test]
    fn thinking_depth_is_part_of_the_selected_model_reference() {
        for depth in crate::Effort::ALL {
            let options = BackendOptions { cli_model: Some("test/model".into()), cli_thinking: Some(depth), ..Default::default() };
            for builder in [false, true] {
                let argv = args(builder, &options);
                let model = argv.iter().position(|a| a == "--model").unwrap();
                assert_eq!(argv[model + 1], format!("test/model#{}", depth.as_str()));
                assert_eq!(argv.last().unwrap(), "PRE make a game");
            }
        }
        let argv = args(false, &BackendOptions::default());
        assert!(!argv.iter().any(|a| a == "--model"));
    }

    /// Extra model arguments work without a saved selection, but cannot override an explicit model or depth.
    #[test]
    fn extra_model_arguments_are_checked_only_for_explicit_selections() {
        let config = BackendConfig {
            name: "OpenCode".into(),
            kind: crate::BackendKind::Nvidia,
            model: "opencode".into(),
            base_url: None,
            ssh_host: None,
            enabled: true,
        };
        for extra in [vec!["--model", "test/legacy"], vec!["--model=test/legacy"], vec!["-m", "test/legacy"], vec!["-mtest/legacy"]] {
            let options =
                BackendOptions { command: Some("missing-test-opencode".into()), args: extra.iter().map(|s| s.to_string()).collect(), ..Default::default() };
            assert!(OpenCode.validate_model(&config, &options, None, &Stop::default()).is_ok());
            let argv = args(false, &options);
            assert_eq!(argv.get(1..1 + extra.len()).unwrap(), extra.as_slice());
            for (model, thinking) in [(Some("test/selected"), None), (None, Some(crate::Effort::High)), (Some("test/selected"), Some(crate::Effort::High))] {
                let selected = BackendOptions { cli_model: model.map(str::to_string), cli_thinking: thinking, ..options.clone() };
                let error = OpenCode.validate_model(&config, &selected, None, &Stop::default()).unwrap_err();
                assert_eq!(error.code, craft_anything_evidence::ErrorCode::BadInput);
            }
        }
    }
}
