//! OpenAI chat completions (and every OpenAI-compatible server: OpenRouter, Ollama, LM Studio,
//! llama.cpp, vLLM…): streaming, function tools, and a ReAct text fallback for models without
//! tool calling.

use crate::http::{self, SseEvent};
use crate::model::{Block, ChatModel, Delta, Msg, Speaker, Stop, StopReason, ToolDef, TurnRequest, TurnResult, Usage};
use craft_anything_evidence::{ErrorCode, ToolError};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// NVIDIA NIM API: OpenAI-compatible, keyed with NVIDIA_API_KEY.
pub const NVIDIA_BASE: &str = "https://integrate.api.nvidia.com/v1";
/// The model a new NVIDIA backend starts on, before NIM's own list is fetched.
pub const NVIDIA_DEFAULT_MODEL: &str = "nvidia/nemotron-4-340b-instruct";
/// NVIDIA NIM models offered when adding a key. Selected for reverse engineering,
/// code analysis, and heavy web/app decomposition work.
pub const NVIDIA_MODELS: &[&str] = &[
    NVIDIA_DEFAULT_MODEL,
    "nvidia/llama-3.1-nemotron-70b-instruct",
    "nvidia/llama-3.1-nemotron-ultra-253b-v1",
    "nvidia/nemotron-3-super-120b-a12b",
    "deepseek-ai/deepseek-coder-6.7b-instruct",
    "nvidia/nemotron-3.5-super-vl-120b-a12b",
];

#[derive(Debug, Clone)]
pub struct OpenAi {
    /// For messages ("OpenAI", "OpenRouter", "Ollama"…).
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub extra_headers: Vec<(&'static str, String)>,
    /// Use the ReAct text protocol instead of function tools.
    pub react: bool,
    /// Recover JSON text calls automatically, and switch to ReAct when the server rejects tools.
    pub auto_react: bool,
}

impl OpenAi {
    /// An NVIDIA NIM API key, through NVIDIA's OpenAI-compatible endpoint at integrate.api.nvidia.com.
    pub fn nvidia(base: Option<&str>, key: String, model: &str) -> Self {
        Self::compatible("NVIDIA", base.unwrap_or(NVIDIA_BASE), Some(key), if model.is_empty() { NVIDIA_DEFAULT_MODEL } else { model })
    }

    pub fn compatible(label: &str, base: &str, key: Option<String>, model: &str) -> Self {
        OpenAi {
            label: label.to_string(),
            base_url: base.trim_end_matches('/').to_string(),
            api_key: key.filter(|k| !k.is_empty()),
            model: model.to_string(),
            extra_headers: Vec::new(),
            react: false,
            auto_react: true,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = self.extra_headers.clone();
        if let Some(k) = &self.api_key {
            h.push(("authorization", format!("Bearer {k}")));
        }
        h
    }

    pub fn body(&self, req: &TurnRequest<'_>) -> Value {
        let mut system = req.system.to_string();
        if self.react && !req.tools.is_empty() {
            system.push_str(&react_instructions(req.tools));
        }
        let mut messages = vec![json!({"role": "system", "content": system})];
        for m in req.messages {
            messages.extend(messages_json(m, self.react));
        }
        let mut body = json!({
            "model": self.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": messages,
        });
        if !self.react
            && !req.tools.is_empty()
            && let Some(o) = body.as_object_mut()
        {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}))
                .collect();
            o.insert("tools".into(), Value::Array(tools));
            o.insert("tool_choice".into(), json!("auto"));
        }
        body
    }

    /// Model ids from `GET /models`.
    pub fn list_models(&self, timeout: std::time::Duration) -> Result<Vec<String>, ToolError> {
        let agent = http::quick_agent(timeout);
        let v = http::get_json(&agent, &self.label, &self.url("models"), &self.headers())?;
        let arr = v.get("data").or_else(|| v.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
        // A catalogue may list a model only for another API (Responses, Messages): such an entry
        // would fail on a chat-completions client, so skip it when the entry says which APIs it
        // serves. Empty and repeated ids are dropped too.
        let mut out: Vec<String> = Vec::new();
        for m in arr {
            if m.get("api").is_some_and(|a| a.get("chat-completions").is_none()) {
                continue;
            }
            let Some(id) = m.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else { continue };
            if !out.iter().any(|seen| seen == id) {
                out.push(id.to_string());
            }
        }
        Ok(out)
    }

    fn stream_once(&self, req: &TurnRequest<'_>, on_delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError> {
        let agent = http::stream_agent();
        let reader = http::post_json(&agent, &self.label, &self.url("chat/completions"), &self.headers(), &self.body(req))?;
        let mut st = ChatStream::default();
        http::parse_sse(reader, |ev| {
            stop.check()?;
            st.apply(&ev, on_delta)?;
            Ok(!st.done)
        })?;
        stop.check()?;
        let mut r = st.finish(&self.model)?;
        if self.react || self.auto_react {
            react_postprocess(&mut r, req.tools, self.react);
        }
        Ok(r)
    }
}

fn rejects_tools(e: &ToolError) -> bool {
    let m = e.message.to_ascii_lowercase();
    let status = e.details.as_ref().and_then(|d| d.get("status")).and_then(Value::as_u64).unwrap_or(0);
    (status == 400 || status == 422 || status == 500 || status == 404) && (m.contains("tool") || m.contains("function"))
}

impl ChatModel for OpenAi {
    fn turn(&mut self, req: &TurnRequest<'_>, on_delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError> {
        match self.stream_once(req, on_delta, stop) {
            Err(e) if !self.react && self.auto_react && !req.tools.is_empty() && rejects_tools(&e) => {
                log::info!("{} rejected tool calling; switching to the text protocol", self.label);
                self.react = true;
                self.stream_once(req, on_delta, stop)
            }
            other => other,
        }
    }

    fn model(&self) -> String {
        self.model.clone()
    }
}

/// Wire messages for one neutral message (a user message with several tool results becomes
/// several `tool` messages).
pub fn messages_json(m: &Msg, react: bool) -> Vec<Value> {
    match m.role {
        Speaker::User => {
            let mut out = Vec::new();
            let mut text = String::new();
            for b in &m.blocks {
                match b {
                    Block::Text { text: t } => text.push_str(t),
                    Block::ToolResult { tool_use_id, content, is_error } => {
                        if react {
                            let tag = if *is_error { "Observation (error)" } else { "Observation" };
                            text.push_str(&format!("{tag}: {content}\n"));
                        } else {
                            out.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
                        }
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
            out
        }
        Speaker::Assistant => {
            let text = m.text();
            if react {
                return vec![json!({"role": "assistant", "content": text})];
            }
            let calls: Vec<Value> = m
                .tool_uses()
                .into_iter()
                .map(|(id, name, input)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": input.to_string()}}))
                .collect();
            let mut v = json!({"role": "assistant", "content": text});
            if !calls.is_empty()
                && let Some(o) = v.as_object_mut()
            {
                o.insert("tool_calls".into(), Value::Array(calls));
            }
            vec![v]
        }
    }
}

#[derive(Debug, Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

/// Accumulates a streamed chat completion.
#[derive(Debug, Default)]
pub struct ChatStream {
    text: String,
    calls: BTreeMap<u64, CallAcc>,
    pub usage: Usage,
    pub finish: Option<String>,
    pub model: String,
    pub done: bool,
}

impl ChatStream {
    pub fn apply(&mut self, ev: &SseEvent, on_delta: &mut dyn FnMut(Delta)) -> Result<(), ToolError> {
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("The server sent a chunk we couldn't read: {e}")))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            let provider = err.get("code").and_then(Value::as_str).or_else(|| err.get("type").and_then(Value::as_str));
            let status = err.get("code").and_then(Value::as_u64);
            return Err(
                ToolError::new(ErrorCode::ProgramFailed, format!("Stream error: {msg}")).with_details(json!({"provider_error": provider, "status": status}))
            );
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = m.to_string();
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage.output_tokens = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(self.usage.output_tokens);
            let cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64);
            if let Some(c) = cached {
                self.usage.cache_read_tokens = c;
            }
            // prompt_tokens includes the cached ones; input_tokens is the rest, as Anthropic reports
            // it, so the context budget and the cost count cached tokens once (#252).
            if let Some(p) = u.get("prompt_tokens").and_then(Value::as_u64) {
                self.usage.input_tokens = p.saturating_sub(cached.unwrap_or(0));
            }
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|a| a.first()) else { return Ok(()) };
        let d = choice.get("delta").cloned().unwrap_or(Value::Null);
        if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            on_delta(Delta::Text(t.to_string()));
        }
        if let Some(t) = d.get("reasoning_content").or_else(|| d.get("reasoning")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
            on_delta(Delta::Thinking(t.to_string()));
        }
        if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
            for (pos, c) in calls.iter().enumerate() {
                let idx = c.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
                if self.calls.len() >= 256 && !self.calls.contains_key(&idx) {
                    continue;
                }
                let acc = self.calls.entry(idx).or_default();
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    acc.id = id.to_string();
                }
                if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                    acc.name.push_str(n);
                }
                if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                    acc.args.push_str(a);
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(f.to_string());
        }
        Ok(())
    }

    pub fn finish(self, fallback_model: &str) -> Result<TurnResult, ToolError> {
        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text });
        }
        let has_calls = !self.calls.is_empty();
        for (i, (_, c)) in self.calls.into_iter().enumerate() {
            let input = if c.args.trim().is_empty() { json!({}) } else { serde_json::from_str(&c.args).unwrap_or(json!({})) };
            let id = if c.id.is_empty() { format!("call_{i}") } else { c.id };
            blocks.push(Block::ToolUse { id, name: c.name, input });
        }
        let stop = match self.finish.as_deref() {
            Some(f) => StopReason::parse(f),
            None if has_calls => StopReason::ToolUse,
            None if self.done => StopReason::EndTurn,
            None => return Err(ToolError::new(ErrorCode::Truncated, "The answer stream ended early.")),
        };
        // Some servers say "stop" for tool calls. A limit or refusal must still prevent execution.
        let stop = if has_calls && stop == StopReason::EndTurn { StopReason::ToolUse } else { stop };
        let model = if self.model.is_empty() { fallback_model.to_string() } else { self.model };
        Ok(TurnResult { message: Msg { role: Speaker::Assistant, blocks }, stop, usage: self.usage, model })
    }
}

/// Extra system text for the ReAct protocol.
pub fn react_instructions(tools: &[ToolDef]) -> String {
    let mut s = String::from(
        "\n\n## Using tools (text protocol)\nYou can't call tools directly. To use one, reply with exactly two lines and nothing after them:\nAction: <tool name>\nAction Input: <JSON object with the parameters>\nYou'll get the result back as an Observation. When you have the answer, start a line with \"Final Answer:\" and write it.\n\nTools:\n",
    );
    for t in tools {
        s.push_str(&format!("- {}: {} Parameters: {}\n", t.name, t.description, t.schema));
    }
    s
}

/// A ReAct step parsed from model text.
#[derive(Debug, Clone, PartialEq)]
pub enum ReactStep {
    Action {
        tool: String,
        input: Value,
    },
    Final(String),
    /// Plain text with no protocol markers (treated as the answer).
    Text(String),
}

/// Parse a ReAct step, accepting a complete JSON call as an alternative.
pub fn parse_react(text: &str) -> ReactStep {
    if let Some(step) = json_action(text) {
        return step;
    }
    // Preserve a whole fenced ReAct step, but do not treat markers inside a JSON answer or
    // ordinary code example as instructions.
    let lead = text.trim();
    let fenced_protocol = backtick_fence(lead).is_some_and(|(_, body)| {
        let body = body.trim_start();
        ["Action:", "Thought:", "Final Answer:"].into_iter().any(|marker| body.starts_with(marker))
    });
    if lead.starts_with(['{', '[']) || (lead.starts_with("```") && !fenced_protocol) {
        return ReactStep::Text(text.trim().to_string());
    }
    if let Some(pos) = text.find("Final Answer:") {
        return ReactStep::Final(text.get(pos + "Final Answer:".len()..).unwrap_or_default().trim().to_string());
    }
    let Some(apos) = text.find("Action:") else { return ReactStep::Text(text.trim().to_string()) };
    let after = text.get(apos + "Action:".len()..).unwrap_or_default();
    let tool = after.lines().next().unwrap_or_default().trim().trim_matches('`').to_string();
    let input = match after.find("Action Input:") {
        Some(ipos) => {
            let raw = after.get(ipos + "Action Input:".len()..).unwrap_or_default().trim();
            let raw = raw.trim_start_matches("```json").trim_start_matches("```").trim();
            extract_json_object(raw).unwrap_or_else(|| json!({}))
        }
        None => json!({}),
    };
    if tool.is_empty() { ReactStep::Text(text.trim().to_string()) } else { ReactStep::Action { tool, input } }
}

/// A complete backtick fence. Its standalone closing line can be longer than its opener,
/// but an earlier closing line means the remaining text is outside that fence.
fn backtick_fence(text: &str) -> Option<(&str, &str)> {
    let (opening, rest) = text.trim().split_once('\n')?;
    let length = opening.bytes().take_while(|byte| *byte == b'`').count();
    if length < 3 {
        return None;
    }
    let language = opening.get(length..)?.trim();
    if language.contains('`') {
        return None;
    }
    let (body, closing) = rest.rsplit_once('\n')?;
    let closes = |line: &str| {
        let line = line.trim();
        line.len() >= length && line.bytes().all(|byte| byte == b'`')
    };
    if !closes(closing) || body.lines().any(closes) {
        return None;
    }
    Some((language, body))
}

/// Accept a complete call, not a JSON object quoted somewhere in an explanation.
fn json_action(text: &str) -> Option<ReactStep> {
    let text = text.trim();
    let raw = if text.starts_with("```") {
        let (language, body) = backtick_fence(text)?;
        if !matches!(language, "" | "json") {
            return None;
        }
        body.trim()
    } else {
        text
    };
    let value: Value = serde_json::from_str(raw).ok()?;
    let envelope = value.as_object()?;
    if envelope.contains_key("function")
        && (envelope.keys().any(|key| !matches!(key.as_str(), "function" | "type" | "id"))
            || envelope.get("type").is_some_and(|kind| kind.as_str() != Some("function")))
    {
        return None;
    }
    let function = value.get("function").unwrap_or(&value);
    let fields = function.as_object()?;
    if fields.keys().any(|key| !matches!(key.as_str(), "name" | "arguments" | "parameters"))
        || fields.contains_key("arguments") == fields.contains_key("parameters")
    {
        return None;
    }
    let tool = function.get("name")?.as_str()?.trim();
    if tool.is_empty() {
        return None;
    }
    let arguments = function.get("arguments").or_else(|| function.get("parameters"))?;
    let input: Value = match arguments.as_str() {
        Some(encoded) => serde_json::from_str(encoded).ok()?,
        None => arguments.clone(),
    };
    input.is_object().then(|| ReactStep::Action { tool: tool.to_string(), input })
}

/// The calls in a reply that ends in fenced JSON calls after a sentence or between them
/// ("Let's list the files." then a ```json fence), as small local models write them (#417).
/// Every fence must be a complete `json` (or unlabelled) call, and nothing but blank lines may
/// follow the last one; anything else (other code, a `~~~` fence, a fence inside a fence, an
/// unclosed fence, words after the call, a JSON object quoted inline) means the reply is text,
/// as before. So do the text protocol's own markers in the words (a final answer that ends in an
/// example is still the answer); inside a call's arguments they're only content.
fn fenced_calls(text: &str) -> Option<Vec<(String, Value)>> {
    let mut calls = Vec::new();
    let mut lines = text.lines();
    let mut words_after = false;
    while let Some(line) = lines.next() {
        let opening = line.trim();
        // Anything in a tilde fence is an example, whatever it holds.
        if opening.starts_with("~~~") {
            return None;
        }
        let length = opening.bytes().take_while(|byte| *byte == b'`').count();
        if length < 3 {
            if ["Final Answer:", "Action:"].into_iter().any(|marker| line.contains(marker)) {
                return None;
            }
            words_after |= !opening.is_empty() && !calls.is_empty();
            continue;
        }
        let closes = |line: &str| {
            let line = line.trim();
            line.len() >= length && line.bytes().all(|byte| byte == b'`')
        };
        let mut body = String::new();
        loop {
            let next = lines.next()?;
            if closes(next) {
                break;
            }
            body.push_str(next);
            body.push('\n');
        }
        let language = opening.get(length..)?.trim();
        // The body is the call itself: a fence inside it is a quoted example.
        if !matches!(language, "" | "json") || body.trim_start().starts_with(['`', '~']) {
            return None;
        }
        let Some(ReactStep::Action { tool, input }) = json_action(&body) else { return None };
        calls.push((tool, input));
        words_after = false;
    }
    (!calls.is_empty() && !words_after).then_some(calls)
}

/// The first balanced `{…}` in `s`, parsed.
fn extract_json_object(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < start) {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return serde_json::from_str(s.get(start..=i)?).ok();
                }
            }
            _ => {}
        }
    }
    None
}

static REACT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Recover complete text calls as neutral tool-use blocks.
fn react_postprocess(r: &mut TurnResult, tools: &[ToolDef], react: bool) {
    if !matches!(r.stop, StopReason::EndTurn | StopReason::ToolUse) || !r.message.tool_uses().is_empty() {
        return;
    }
    let text = r.message.text();
    let json_call = json_action(&text);
    let from_json = json_call.is_some();
    // Calls after a sentence run only when every one names a tool the model was offered.
    let offered = |tool: &str| tools.iter().any(|definition| definition.name == tool);
    if !from_json && let Some(calls) = fenced_calls(&text).filter(|calls| calls.iter().all(|(tool, _)| offered(tool))) {
        for (tool, input) in calls {
            let n = REACT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            r.message.blocks.push(Block::ToolUse { id: format!("react_{n}"), name: tool, input });
        }
        r.stop = StopReason::ToolUse;
        return;
    }
    let step = json_call.or_else(|| react.then(|| parse_react(&text)));
    if let Some(ReactStep::Action { tool, input }) = step
        && (!from_json || tools.iter().any(|definition| definition.name == tool))
    {
        let n = REACT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        r.message.blocks.push(Block::ToolUse { id: format!("react_{n}"), name: tool, input });
        r.stop = StopReason::ToolUse;
    } else if react {
        r.stop = StopReason::EndTurn;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Effort;

    #[test]
    fn calls_after_a_sentence_are_read_and_examples_are_not() {
        let call = |tool: &str| format!("```json\n{{\"name\":\"{tool}\",\"arguments\":{{\"path\":\"app\"}}}}\n```");
        let names = |text: &str| fenced_calls(text).map(|calls| calls.into_iter().map(|(tool, _)| tool).collect::<Vec<_>>());
        // What qwen2.5-coder:14b wrote in a live build.
        let after = format!("It looks like the file is missing. Let's list the files.\n\n{}", call("look"));
        assert_eq!(names(&after), Some(vec!["look".to_string()]));
        let several = format!("{}\n\nThen:\n{}\n\n", call("look"), call("write"));
        assert_eq!(names(&several), Some(vec!["look".to_string(), "write".to_string()]));
        assert_eq!(names("```\n{\"name\":\"look\",\"parameters\":{}}\n````"), Some(vec!["look".to_string()]), "unlabelled, longer close");
        for text in [
            format!("{}\nThis is an example.", call("look")),
            format!("Here's Rust:\n```rust\nfn main() {{}}\n```\n{}", call("look")),
            format!("{}\n```json\n{{\"name\":\"look\",\"arguments\":{{}}}}", call("look")),
            "For example, use {\"name\":\"look\",\"arguments\":{}}.".to_string(),
            "```json\n{\"name\":\"look\",\"arguments\":{},\"description\":\"x\"}\n```".to_string(),
            "```json\n[{\"name\":\"look\",\"arguments\":{}}]\n```".to_string(),
            "No calls here.".to_string(),
            // An example in a tilde fence, then a real-looking call.
            format!("~~~markdown\n{}\n~~~\n{}", call("write"), call("look")),
            // A four-backtick fence quoting a three-backtick call.
            format!("Like this:\n````\n{}\n````", call("write")),
        ] {
            assert_eq!(names(&text), None, "{text}");
        }

        // Only tools that were offered run, and then all of them.
        let tool = |name: &str| ToolDef { name: name.into(), description: String::new(), schema: json!({"type": "object"}) };
        let turn = |text: &str| TurnResult {
            message: Msg { role: Speaker::Assistant, blocks: vec![Block::Text { text: text.into() }] },
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            model: String::new(),
        };
        let mut r = turn(&several);
        react_postprocess(&mut r, &[tool("look"), tool("write")], false);
        assert_eq!(r.stop, StopReason::ToolUse);
        assert_eq!(r.message.tool_uses().into_iter().map(|(_, name, _)| name.to_string()).collect::<Vec<_>>(), ["look", "write"]);
        let mut r = turn(&several);
        react_postprocess(&mut r, &[tool("look")], false);
        assert!(r.message.tool_uses().is_empty() && r.stop == StopReason::EndTurn, "one isn't offered: none run");
        // The markers inside a call's arguments are only what it writes.
        let notes = "Writing the notes.\n```json\n{\"name\":\"write\",\"arguments\":{\"content\":\"Action: save\\nFinal Answer: done\"}}\n```";
        let mut r = turn(notes);
        react_postprocess(&mut r, &[tool("write")], false);
        assert_eq!(r.message.tool_uses().len(), 1, "{notes}");
        // A final answer that ends in an example stays the answer.
        let answer = format!("Final Answer: it's built. To list the files again:\n{}", call("look"));
        for react in [false, true] {
            let mut r = turn(&answer);
            react_postprocess(&mut r, &[tool("look")], react);
            assert!(r.message.tool_uses().is_empty() && r.stop == StopReason::EndTurn, "react={react}");
        }
    }

    #[test]
    fn json_text_calls_and_non_calls() {
        for text in [
            r#"{"name":"look","arguments":{"path":"app"}}"#,
            r#"{"name":"look","parameters":{"path":"app"}}"#,
            r#"{"name":"look","arguments":"{\"path\":\"app\"}"}"#,
            r#"{"type":"function","function":{"name":"look","arguments":{"path":"app"}}}"#,
            "```json\n{\"name\":\"look\",\"arguments\":{\"path\":\"app\"}}\n```",
            "```\n{\"name\":\"look\",\"arguments\":{\"path\":\"app\"}}\n```",
            "```json\n{\"name\":\"look\",\"arguments\":{\"path\":\"app\"}}\n````",
            "````json\n{\"name\":\"look\",\"arguments\":{\"path\":\"app\"}}\n````",
            "````\n{\"name\":\"look\",\"arguments\":{\"path\":\"app\"}}\n`````",
        ] {
            assert_eq!(parse_react(text), ReactStep::Action { tool: "look".into(), input: json!({"path": "app"}) }, "{text}");
        }
        for text in [
            r#"{"name":"look","arguments":{"path":"app"}"#,
            r#"{"name":"look","arguments":"not JSON"}"#,
            r#"{"name":"look","arguments":["app"]}"#,
            r#"{"name":"look"}"#,
            r#"{"name":"look","arguments":{},"description":"This is an example"}"#,
            r#"{"name":"look","arguments":{},"parameters":{"path":"different"}}"#,
            r#"{"type":"example","function":{"name":"look","arguments":{}}}"#,
            r#"{"name":"","arguments":{}}"#,
            r#"{"answer":"Action: look\nAction Input: {}"}"#,
            r#"[{"name":"look","arguments":{}}]"#,
            r#"For example, use {"name":"look","arguments":{}}."#,
            "```json\n{\"name\":\"look\",\"arguments\":{}}\n```\nThis is an example.",
            "```json\n{\"name\":\"look\",\"arguments\":{}}```",
            "```json`example\n{\"name\":\"look\",\"arguments\":{}}\n```",
            "````json\n{\"name\":\"look\",\"arguments\":{}}\n```",
            "```json\n{\"name\":\"look\",\"arguments\":{}}\n```\nThis is only an example.\n```",
        ] {
            assert!(matches!(parse_react(text), ReactStep::Text(_)), "{text}");
        }
        assert_eq!(
            parse_react(r#"{"name":"look","arguments":{"text":"Final Answer: is a label"}}"#),
            ReactStep::Action { tool: "look".into(), input: json!({"text":"Final Answer: is a label"}) }
        );
    }

    #[test]
    fn fenced_legacy_react_calls_still_parse() {
        for text in [
            "```\nAction: look\nAction Input: {\"path\":\"app\"}\n```",
            "```text\nAction: look\nAction Input: {\"path\":\"app\"}\n```",
            "```json\nThought: inspect the app\nAction: look\nAction Input: {\"path\":\"app\"}\n```",
            " \n```\r\nAction: look\r\nAction Input: {\"path\":\"app\"}\r\n```\n ",
            "```text\nAction: look\nAction Input: {\"path\":\"app\"}\n  ``` \t",
            "```text\nAction: look\nAction Input: {\"path\":\"app\"}\n````",
            "````text\nAction: look\nAction Input: {\"path\":\"app\"}\n````",
            "````text\nAction: look\nAction Input: ```json\n{\"path\":\"app\"}\n```\n````",
        ] {
            assert_eq!(parse_react(text), ReactStep::Action { tool: "look".into(), input: json!({"path": "app"}) }, "{text}");
        }
        for text in [
            "```json\n{\"answer\":\"Action: look\\nAction Input: {}\"}\n```",
            "```\n[\"Action: look\\nAction Input: {}\"]\n```",
            "```python\nprint(\"Action: look\\nAction Input: {}\")\n```",
            "```\nThis is an example.\nAction: look\nAction Input: {}\n```",
            "```\nAction: look\nAction Input: {}",
            "```\nAction: look\nAction Input: {}\n```\nThis is only an example.",
            "```text\nAction: look\nAction Input: {}\n```not a close```",
            "```text\nAction: look\nAction Input: {}```",
            "````text\nAction: look\nAction Input: {}\n```",
            "```text\nAction: look\nAction Input: {}\n```\nThis is only an example.\n```",
            "```text\nAction: look\nAction Input: {}\n````\nThis is only an example.\n```",
            "````text\nAction: look\nAction Input: {}\n````\nThis is only an example.\n````",
            "```json\n{\"name\":\"look\",\"arguments\":{}}```",
        ] {
            assert!(matches!(parse_react(text), ReactStep::Text(_)), "{text}");
        }
    }

    #[test]
    fn react_parsing() {
        assert_eq!(
            parse_react("Thought: look\nAction: core_session_show\nAction Input: {\"a\": \"}\"}\nmore"),
            ReactStep::Action { tool: "core_session_show".into(), input: json!({"a": "}"}) }
        );
        assert_eq!(parse_react("Action: core_targets_list\n"), ReactStep::Action { tool: "core_targets_list".into(), input: json!({}) });
        assert_eq!(parse_react("blah\nFinal Answer: it saves JSON"), ReactStep::Final("it saves JSON".into()));
        assert_eq!(parse_react("just words"), ReactStep::Text("just words".into()));
        assert_eq!(parse_react("Action: x\nAction Input: ```json\n{\"k\":1}\n```"), ReactStep::Action { tool: "x".into(), input: json!({"k": 1}) });
    }

    #[test]
    fn tool_call_accumulation() {
        let chunks = [
            json!({"model":"gpt-x","choices":[{"delta":{"role":"assistant","content":"Let me look"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"core_session_","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"show","arguments":"{\"x\""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"core_targets_list","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5}}),
        ];
        let mut st = ChatStream::default();
        let mut text = String::new();
        for c in chunks {
            st.apply(&SseEvent { event: None, data: c.to_string() }, &mut |d| {
                if let Delta::Text(t) = d {
                    text.push_str(&t)
                }
            })
            .unwrap();
        }
        st.apply(&SseEvent { event: None, data: "[DONE]".into() }, &mut |_| {}).unwrap();
        let r = st.finish("m").unwrap();
        assert_eq!(text, "Let me look");
        assert_eq!(r.stop, StopReason::ToolUse);
        assert_eq!(r.usage.input_tokens, 11);
        // #252: cached prompt tokens are part of prompt_tokens, and are counted once.
        let mut st = ChatStream::default();
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 100_000, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 65_536}}});
        let end = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        for data in [end.to_string(), usage.to_string(), "[DONE]".into()] {
            st.apply(&SseEvent { event: None, data }, &mut |_| {}).unwrap();
        }
        let u = st.finish("m").unwrap().usage;
        assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (34_464, 65_536, 10));
        assert_eq!(
            r.message.tool_uses(),
            vec![("c1".into(), "core_session_show".into(), json!({"x": 1})), ("c2".into(), "core_targets_list".into(), json!({}))]
        );
    }

    #[test]
    fn native_tool_calls_preserve_terminal_stop_reasons() {
        for (finish, expected) in [
            (Some("length"), StopReason::MaxTokens),
            (Some("content_filter"), StopReason::Refusal),
            (Some("pause_turn"), StopReason::PauseTurn),
            (Some("provider_stop"), StopReason::Other("provider_stop".into())),
            (Some("tool_calls"), StopReason::ToolUse),
            (Some("stop"), StopReason::ToolUse),
            (None, StopReason::ToolUse),
        ] {
            let mut stream = ChatStream::default();
            let call = json!({"choices":[{"delta":{"tool_calls":[{
                "index":0,"id":"call_a","function":{"name":"core_session_show","arguments":"{}"}
            }]}}]});
            stream.apply(&SseEvent { event: None, data: call.to_string() }, &mut |_| {}).unwrap();
            if let Some(reason) = finish {
                let end = json!({"choices":[{"delta":{},"finish_reason":reason}]});
                stream.apply(&SseEvent { event: None, data: end.to_string() }, &mut |_| {}).unwrap();
            }
            stream.apply(&SseEvent { event: None, data: "[DONE]".into() }, &mut |_| {}).unwrap();
            let result = stream.finish("m").unwrap();
            assert_eq!(result.stop, expected, "finish_reason={finish:?}");
            assert_eq!(result.message.tool_uses(), vec![("call_a".into(), "core_session_show".into(), json!({}))]);
        }
    }

    #[test]
    fn wire_messages() {
        let o = OpenAi::nvidia(None, "k".into(), "");
        assert_eq!(o.label, "NVIDIA");
        assert!(o.headers().iter().any(|(k, v)| *k == "authorization" && v == "Bearer k"));
        let local = OpenAi::compatible("NVIDIA", "http://127.0.0.1:11434/v1/", None, "llama3");
        assert!(local.headers().is_empty());
        assert_eq!(local.url("models"), "http://127.0.0.1:11434/v1/models");
        let assistant = Msg { role: Speaker::Assistant, blocks: vec![Block::ToolUse { id: "c1".into(), name: "t".into(), input: json!({"a":1}) }] };
        let results = Msg {
            role: Speaker::User,
            blocks: vec![
                Block::ToolResult { tool_use_id: "c1".into(), content: "ok".into(), is_error: false },
                Block::ToolResult { tool_use_id: "c2".into(), content: "bad".into(), is_error: true },
            ],
        };
        let msgs = vec![Msg::user_text("q"), assistant, results];
        let tools = vec![ToolDef { name: "t".into(), description: "d".into(), schema: json!({"type":"object"}) }];
        let b = local.body(&TurnRequest { system: "S", messages: &msgs, tools: &tools, effort: Effort::Low, max_tokens: 100 });
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(m[2]["content"], "");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[4]["tool_call_id"], "c2");
        assert_eq!(b["tool_choice"], "auto");
        let mut react = local.clone();
        react.react = true;
        let b = react.body(&TurnRequest { system: "S", messages: &msgs, tools: &tools, effort: Effort::Low, max_tokens: 100 });
        assert!(b.get("tools").is_none());
        assert!(b["messages"][0]["content"].as_str().unwrap().contains("Action Input:"));
        assert!(b["messages"][3]["content"].as_str().unwrap().contains("Observation (error): bad"));
    }
}
