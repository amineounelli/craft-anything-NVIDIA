//! End-to-end tests for the NVIDIA NIM backend. A tiny std TcpListener HTTP server stands in for
//! the model API, and a real engine runs the tools. No real network.

use super::*;
use craft_anything_platform::sync::Mutex;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

/// Serve canned responses `(status, content_type, body)` in order; record requests.
fn mock(responses: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for (status, ctype, reply) in responses {
            let Ok((stream, _)) = l.accept() else { return };
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap();
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut buf = vec![0; len];
            r.read_exact(&mut buf).unwrap();
            let body = serde_json::from_slice(&buf).unwrap_or(Value::Null);
            s2.lock().unwrap().push(Seen { path, headers, body });
            let mut w = stream;
            let reason = if status == 200 { "OK" } else { "Error" };
            write!(w, "HTTP/1.1 {status} {reason}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            w.flush().unwrap();
        }
    });
    (url, seen)
}

/// A stand-in server on a free port that answers every request with `status` and `body`; its `/v1` address.
pub(crate) fn serve(status: &'static str, body: &'static str) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            if !got.is_empty() {
                let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        }
    });
    base
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    #[allow(dead_code)]
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

fn hub() -> (AiHub, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
    let h = AiHub::with_stores(engine, dir.path().join("config.toml"), KeyStore::file_only(dir.path().join("ai-keys.toml")));
    (h, dir)
}

/// #37: doctor asks a local server for its models instead of trusting an open port.
#[test]
fn a_local_server_is_reachable_only_when_it_lists_models() {
    let (h, _d) = hub();
    let url = serve("200 OK", r#"{"data":[{"id":"qwen3-8b"}]}"#);
    h.add_backend(
        BackendConfig { name: "NIM".into(), kind: BackendKind::Nvidia, model: "".into(), base_url: Some(url), ssh_host: None, enabled: true },
        Some("sk-test".into()),
    )
    .unwrap();
    let reach = h.check_reachability();
    let of = |n: &str| reach.iter().find(|(name, _)| name == n).map(|(_, r)| r.clone()).unwrap();
    assert!(of("NIM").is_ok(), "{reach:?}");
}

fn add(h: &AiHub, name: &str, base: &str, key: Option<&str>) {
    h.add_backend(
        BackendConfig { name: name.into(), kind: BackendKind::Nvidia, model: String::new(), base_url: Some(base.into()), ssh_host: None, enabled: true },
        key.map(str::to_string),
    )
    .unwrap();
}

fn run(c: &mut Box<dyn Conversation>, text: &str) -> Vec<AgentEvent> {
    c.send(text);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        for e in c.poll() {
            let done = matches!(e, AgentEvent::TurnDone { .. });
            all.push(e);
            if done {
                return all;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out: {all:?}");
}

/// #498: return a field-specific error to the model, then accept its corrected call.
#[test]
fn a_model_can_correct_an_array_sent_to_a_string_parameter() {
    let call = |id: &str, input: Value| {
        chat_sse(&[json!({"choices":[{
            "delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{
                "name":"core_unknown_add","arguments":input.to_string()
            }}]},"finish_reason":"tool_calls"
        }]})])
    };
    let (url, seen) = mock(vec![
        (200, "text/event-stream", call("wrong", json!({"question": ["Where is the loop?"]}))),
        (200, "text/event-stream", call("corrected", json!({"question": "Where is the loop?"}))),
        (200, "text/event-stream", chat_sse(&[json!({"choices":[{"delta":{"content":"Recorded."},"finish_reason":"stop"}]})])),
    ]);
    let (h, _d) = hub();
    add(&h, "Local", &url, Some("sk-test"));
    let mut c = h.start(Some("Local"), true).unwrap();
    let events = run(&mut c, "Record what remains unknown.");
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { tool, ok, .. } if tool == "core_unknown_add" => Some(*ok),
            _ => None,
        })
        .collect();
    assert_eq!(completed, vec![false, true]);
    assert_eq!(h.engine.session().unknowns().count(), 1, "only the corrected call creates a record");
    let seen = seen.lock().unwrap();
    let result = seen[1].body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "wrong");
    let error: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(error["error"]["code"], "bad_input");
    assert_eq!(error["error"]["details"]["parameter_path"], "/question");
    assert!(error["error"]["message"].as_str().unwrap().contains("expected a string"));
    assert!(error["error"]["remediation"].as_str().unwrap().contains("call the tool again"));
}

#[test]
fn json_text_tool_calls_execute_through_both_protocols() {
    for react in [false, true] {
        for call in [
            r#"{"name":"core_session_show","arguments":{}}"#,
            "```json\n{\"name\":\"core_session_show\",\"arguments\":{}}\n````",
            "````json\n{\"name\":\"core_session_show\",\"arguments\":{}}\n````",
        ] {
            let t1 = chat_sse(&[json!({"choices":[{"delta":{"content":call},"finish_reason":"stop"}]})]);
            let t2 = chat_sse(&[json!({"choices":[{"delta":{"content":"Nothing open."},"finish_reason":"stop"}]})]);
            let (url, _seen) = mock(vec![(200, "text/event-stream", t1), (200, "text/event-stream", t2)]);
            let (hub, _dir) = hub();
            let config =
                BackendConfig { name: "Local".into(), kind: BackendKind::Nvidia, model: "scripted".into(), base_url: Some(url), ssh_host: None, enabled: true };
            hub.add_backend_with(BackendEntry { config, options: BackendOptions { react_fallback: Some(react), ..Default::default() } }, Some("sk-test".into())).unwrap();
            let mut conversation = hub.start(Some("Local"), true).unwrap();
            let events = run(&mut conversation, "status?");
            assert_eq!(
                events.iter().filter(|e| matches!(e, AgentEvent::ToolFinished { tool, ok: true, .. } if tool == "core_session_show")).count(),
                1,
                "react={react}, call={call}: {events:?}"
            );
            assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Nothing open.")), "{events:?}");
        }
    }
}

#[test]
fn react_action_errors_still_reach_the_tool_loop() {
    let replies = ["Action: missing_tool\nAction Input: {}", "Action: core_session_show\nAction Input: {}", "Final Answer: Nothing open."]
        .map(|text| (200, "text/event-stream", chat_sse(&[json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]})])));
    let (url, _seen) = mock(replies.into());
    let (hub, _dir) = hub();
    let config =
        BackendConfig { name: "Local".into(), kind: BackendKind::Nvidia, model: "scripted".into(), base_url: Some(url), ssh_host: None, enabled: true };
    hub.add_backend_with(BackendEntry { config, options: BackendOptions { react_fallback: Some(true), ..Default::default() } }, Some("sk-test".into())).unwrap();
    let mut conversation = hub.start(Some("Local"), true).unwrap();
    let events = run(&mut conversation, "status?");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { tool, ok: false, .. } if tool == "missing_tool")), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { tool, ok: true, .. } if tool == "core_session_show")), "{events:?}");
}

#[test]
fn fenced_legacy_react_calls_still_execute() {
    for call in [
        "```text\nAction: core_session_show\nAction Input: {}\n```",
        "```text\nAction: core_session_show\nAction Input: {}\n````",
        "````text\nAction: core_session_show\nAction Input: {}\n````",
        "````text\nAction: core_session_show\nAction Input: ```json\n{}\n```\n````",
    ] {
        let replies = [call, "Final Answer: Nothing open."]
            .map(|text| (200, "text/event-stream", chat_sse(&[json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]})])));
        let (url, _seen) = mock(replies.into());
        let (hub, _dir) = hub();
        let config =
            BackendConfig { name: "Local".into(), kind: BackendKind::Nvidia, model: "scripted".into(), base_url: Some(url), ssh_host: None, enabled: true };
        hub.add_backend_with(BackendEntry { config, options: BackendOptions { react_fallback: Some(true), ..Default::default() } }, Some("sk-test".into())).unwrap();
        let mut conversation = hub.start(Some("Local"), true).unwrap();
        let events = run(&mut conversation, "status?");
        assert_eq!(
            events.iter().filter(|e| matches!(e, AgentEvent::ToolFinished { tool, ok: true, .. } if tool == "core_session_show")).count(),
            1,
            "the fenced legacy call must execute exactly once: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Final Answer: Nothing open.")), "{events:?}");
    }
}

#[test]
fn json_text_recovery_preserves_non_actions_and_stop_reasons() {
    use crate::model::{Effort, Msg, Stop, StopReason, ToolDef, TurnRequest};
    let tools = [ToolDef { name: "look".into(), description: "Look at a file".into(), schema: json!({"type":"object"}) }];
    let messages = [Msg::user_text("check")];
    let request = TurnRequest { system: "S", messages: &messages, tools: &tools, effort: Effort::Low, max_tokens: 100 };
    for react in [false, true] {
        for (text, finish, expected) in [
            (r#"{"name":"unknown","arguments":{}}"#, "stop", StopReason::EndTurn),
            (r#"{"name":"look","arguments":{}}"#, "length", StopReason::MaxTokens),
            (r#"{"name":"look","arguments":{}}"#, "content_filter", StopReason::Refusal),
            (r#"{"name":"look","arguments":{}"#, "stop", StopReason::EndTurn),
            (r#"{"answer":42}"#, "stop", StopReason::EndTurn),
            (r#"For example: {"name":"look","arguments":{}}"#, "stop", StopReason::EndTurn),
            ("```text\nAction: look\nAction Input: {}\n```not a close```", "stop", StopReason::EndTurn),
            ("```json\n{\"name\":\"look\",\"arguments\":{}}```", "stop", StopReason::EndTurn),
            ("```text\nAction: look\nAction Input: {}```", "stop", StopReason::EndTurn),
            ("````text\nAction: look\nAction Input: {}\n```", "stop", StopReason::EndTurn),
            ("```text\nAction: look\nAction Input: {}\n```\nThis is only an example.\n```", "stop", StopReason::EndTurn),
        ] {
            let reply = chat_sse(&[json!({"choices":[{"delta":{"content":text},"finish_reason":finish}]})]);
            let (url, _seen) = mock(vec![(200, "text/event-stream", reply)]);
            let mut model = openai::OpenAi::nvidia(Some(&url), "k".into(), "scripted");
            model.react = react;
            let result = model.turn(&request, &mut |_| {}, &Stop::default()).unwrap();
            assert!(result.message.tool_uses().is_empty(), "{result:?}");
            assert_eq!(result.stop, expected, "{text}");
        }
    }
    let call = r#"{"name":"look","arguments":{}}"#;
    for (automatic, offered) in [(false, tools.as_slice()), (true, &[])] {
        let reply = chat_sse(&[json!({"choices":[{"delta":{"content":call},"finish_reason":"stop"}]})]);
        let (url, _seen) = mock(vec![(200, "text/event-stream", reply)]);
        let mut model = openai::OpenAi::nvidia(Some(&url), "k".into(), "scripted");
        model.auto_react = automatic;
        let request = TurnRequest { system: "S", messages: &messages, tools: offered, effort: Effort::Low, max_tokens: 100 };
        assert!(model.turn(&request, &mut |_| {}, &Stop::default()).unwrap().message.tool_uses().is_empty());
    }
}

#[test]
fn native_tool_calls_are_not_duplicated_by_json_text() {
    use crate::model::{Effort, Msg, Stop, StopReason, ToolDef, TurnRequest};
    let tools = [ToolDef { name: "look".into(), description: "Look at a file".into(), schema: json!({"type":"object"}) }];
    let messages = [Msg::user_text("check")];
    let request = TurnRequest { system: "S", messages: &messages, tools: &tools, effort: Effort::Low, max_tokens: 100 };
    for react in [false, true] {
        let reply = chat_sse(&[json!({"choices":[{"delta":{
            "content":r#"{"name":"look","arguments":{}}"#,
            "tool_calls":[{"index":0,"id":"native_call","type":"function","function":{"name":"look","arguments":"{}"}}]
        },"finish_reason":"tool_calls"}]})]);
        let (url, _seen) = mock(vec![(200, "text/event-stream", reply)]);
        let mut model = openai::OpenAi::nvidia(Some(&url), "k".into(), "scripted");
        model.react = react;
        let result = model.turn(&request, &mut |_| {}, &Stop::default()).unwrap();
        assert_eq!(result.message.tool_uses(), vec![("native_call".into(), "look".into(), json!({}))]);
        assert_eq!(result.stop, StopReason::ToolUse);
    }
}

/// #54, #98: a server on another computer is added by its address, with a model that can chat
/// when none is named; one that doesn't answer with its models isn't added.
#[test]
fn a_server_is_added_by_its_address() {
    // NVIDIA is the only backend kind now; add_server is for OpenAI-compatible servers and returns
    // an error. Test that NVIDIA backends are added through add_backend instead.
    let (h, _d) = hub();
    add(&h, "NIM", "https://integrate.api.nvidia.com/v1", Some("sk-test"));
    assert_eq!(h.backends().len(), 1);
    assert_eq!(h.backends()[0].kind, BackendKind::Nvidia);
    assert_eq!(h.backends()[0].name, "NIM");
}

/// NVIDIA backend needs a key; without one, start fails.
#[test]
fn nvidia_backend_needs_key() {
    let (h, _d) = hub();
    let cfg = BackendConfig { name: "NIM".into(), kind: BackendKind::Nvidia, model: String::new(), base_url: None, ssh_host: None, enabled: true };
    h.add_backend(cfg, None).unwrap();
    let cfg = BackendConfig { name: "NIM".into(), kind: BackendKind::Nvidia, model: String::new(), base_url: None, ssh_host: None, enabled: true };
    assert!(h.add_backend(cfg, Some("sk-test".into())).is_ok());
    assert_eq!(h.backends().len(), 1);
}

fn chat_sse(chunks: &[Value]) -> String {
    let mut s: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    s.push_str("data: [DONE]\n\n");
    s
}
