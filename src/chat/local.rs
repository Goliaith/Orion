//! The local backend: models served on this machine through the OpenAI-style
//! `/v1/chat/completions` API that Ollama, llama.cpp's `llama-server`, vLLM
//! and LM Studio all provide.

use std::io::BufRead;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::http::{non_empty_env, sse_data, Http};
use super::{Backend, Event, Reply, Stop, ToolCall, ToolResult, ToolSpec, Usage};

/// `[chat.local]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// The server's OpenAI-compatible base URL, ending in `/v1`. Defaults:
    /// Ollama :11434, llama-server :8080, vLLM :8000, LM Studio :1234.
    pub base_url: String,
    /// Empty means the first model the server lists.
    pub model: String,
    /// Output limit per reply; unset leaves it to the server.
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    /// Environment variable holding an API key, for servers that want one.
    pub api_key_env: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: String::new(),
            max_tokens: None,
            temperature: None,
            api_key_env: String::new(),
        }
    }
}

pub struct LocalBackend {
    http: Http,
    config: LocalConfig,
    tools: Vec<Value>,
    /// Starts with the system message; append-only.
    messages: Vec<Value>,
    usage: Usage,
    next_call_id: u64,
}

impl LocalBackend {
    /// Check that the server answers, and pick a model if none is set.
    pub fn connect(mut config: LocalConfig, system: String, tools: &[ToolSpec]) -> Result<Self> {
        let mut headers = Vec::new();
        if !config.api_key_env.is_empty() {
            let key = non_empty_env(&config.api_key_env)
                .with_context(|| format!("{} is not set (chat.local.api_key_env)", config.api_key_env))?;
            headers.push(("authorization".into(), format!("Bearer {key}")));
        }
        let mut backend = Self {
            http: Http::new(&config.base_url, headers),
            tools: tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": { "name": t.name, "description": t.description, "parameters": t.schema },
                    })
                })
                .collect(),
            messages: vec![json!({ "role": "system", "content": system })],
            usage: Usage::default(),
            next_call_id: 0,
            config: LocalConfig::default(),
        };
        if config.model.is_empty() {
            let models = backend.list_models().with_context(|| format!("asking {} for its models", config.base_url))?;
            let Some(first) = models.into_iter().next() else {
                bail!("{} has no models; install one first (for Ollama: `ollama pull qwen3`)", config.base_url);
            };
            config.model = first;
        }
        backend.config = config;
        Ok(backend)
    }

    fn list_models_with(http: &Http) -> Result<Vec<String>> {
        let v = http.get_json("/models")?;
        let mut ids: Vec<String> = v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        ids.sort();
        Ok(ids)
    }

    fn request_body(&self) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": self.messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !self.tools.is_empty() {
            body["tools"] = json!(self.tools);
        }
        if let Some(n) = self.config.max_tokens {
            body["max_tokens"] = json!(n);
        }
        if let Some(t) = self.config.temperature {
            body["temperature"] = json!(t);
        }
        body
    }
}

impl Backend for LocalBackend {
    fn describe(&self) -> String {
        format!("{} · local at {}", self.config.model, self.http.base())
    }

    fn set_model(&mut self, model: &str) {
        self.config.model = model.to_string();
    }

    fn list_models(&self) -> Result<Vec<String>> {
        Self::list_models_with(&self.http)
    }

    fn push_user(&mut self, text: &str) {
        self.messages.push(json!({ "role": "user", "content": text }));
    }

    fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply> {
        let reader = self
            .http
            .post_stream("/chat/completions", &self.request_body(), &[])
            .with_context(|| format!("calling the local model {}", self.config.model))?;
        let done = parse_stream(reader, on)?;
        self.usage.add(&done.usage);
        if done.finish.as_deref() == Some("content_filter") {
            return Ok(Reply { calls: Vec::new(), stop: Stop::Refused });
        }

        let mut calls = Vec::new();
        let mut wire_calls = Vec::new();
        for c in done.calls {
            let id = c.id.unwrap_or_else(|| {
                self.next_call_id += 1;
                format!("call_{}", self.next_call_id)
            });
            let (input, invalid_input) = match c.arguments.trim() {
                "" => (json!({}), None),
                raw => match serde_json::from_str::<Value>(raw) {
                    Ok(v @ Value::Object(_)) => (v, None),
                    _ => (json!({}), Some(raw.to_string())),
                },
            };
            // Servers re-parse the arguments in the history, so broken ones go back as {}.
            let arguments = if invalid_input.is_some() { "{}".to_string() } else { c.arguments };
            wire_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": c.name, "arguments": arguments },
            }));
            calls.push(ToolCall { id, name: c.name, input, invalid_input });
        }
        let mut message = json!({ "role": "assistant", "content": done.content });
        if !wire_calls.is_empty() {
            message["tool_calls"] = json!(wire_calls);
        }
        self.messages.push(message);

        let stop = match done.finish.as_deref() {
            Some("length") => Stop::Truncated,
            // Some servers report "stop" even when they return tool calls.
            _ if !calls.is_empty() => Stop::ToolUse,
            _ => Stop::Done,
        };
        Ok(Reply { calls, stop })
    }

    fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        for r in results {
            let content = if r.is_error { format!("Error: {}", r.content) } else { r.content };
            self.messages.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
        }
    }

    fn clear(&mut self) {
        self.messages.truncate(1);
    }

    fn usage(&self) -> Usage {
        self.usage
    }
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct Completion {
    /// The visible answer, with any `<think>` sections removed.
    content: String,
    calls: Vec<PartialCall>,
    finish: Option<String>,
    usage: Usage,
}

/// Assemble a streamed chat completion.
fn parse_stream(reader: impl BufRead, on: &mut dyn FnMut(Event)) -> Result<Completion> {
    let mut done = Completion::default();
    let mut think = ThinkSplitter::default();
    fn emit(on: &mut dyn FnMut(Event), done: &mut Completion, pieces: Vec<(bool, String)>) {
        for (thinking, text) in pieces {
            if thinking {
                on(Event::Thinking(&text));
            } else {
                // Drop the blank lines models put between reasoning and answer.
                let text = if done.content.is_empty() { text.trim_start().to_string() } else { text };
                if !text.is_empty() {
                    on(Event::Text(&text));
                    done.content.push_str(&text);
                }
            }
        }
    }
    for data in sse_data(reader) {
        let data = data?;
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(&data).context("malformed stream chunk")?;
        if let Some(err) = chunk.get("error") {
            let msg = err.get("message").and_then(Value::as_str).or(err.as_str()).unwrap_or("unknown");
            bail!("local model error: {msg}");
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            done.usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
            done.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = chunk["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        // llama.cpp and vLLM say reasoning_content; Ollama says reasoning.
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta[key].as_str().filter(|t| !t.is_empty()) {
                on(Event::Thinking(t));
            }
        }
        if let Some(t) = delta["content"].as_str() {
            let pieces = think.feed(t);
            emit(on, &mut done, pieces);
        }
        for tc in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = match tc["index"].as_u64() {
                Some(i) => i as usize,
                // Without an index, a new id starts a new call.
                None if tc["id"].is_string() || done.calls.is_empty() => done.calls.len(),
                None => done.calls.len() - 1,
            };
            if done.calls.len() <= index {
                done.calls.resize_with(index + 1, PartialCall::default);
            }
            let call = &mut done.calls[index];
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                call.id = Some(id.to_string());
            }
            let f = &tc["function"];
            if let Some(name) = f["name"].as_str() {
                call.name.push_str(name);
            }
            match &f["arguments"] {
                Value::String(s) => call.arguments.push_str(s),
                Value::Null => {}
                // A few servers send the arguments as an object.
                other => call.arguments = other.to_string(),
            }
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            done.finish = Some(r.to_string());
        }
    }
    let rest = think.finish();
    emit(on, &mut done, rest);
    done.calls.retain(|c| !c.name.is_empty());
    Ok(done)
}

/// Separates `<think>…</think>` sections, which many local reasoning models
/// write inline, from the answer, even when a tag is split across chunks.
#[derive(Default)]
struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

impl ThinkSplitter {
    /// Pieces of text, each flagged as thinking or not.
    fn feed(&mut self, chunk: &str) -> Vec<(bool, String)> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending.drain(..pos).collect();
                self.pending.drain(..tag.len());
                if !before.is_empty() {
                    out.push((self.in_think, before));
                }
                self.in_think = !self.in_think;
                continue;
            }
            // Hold back a suffix that could be the start of the tag.
            let keep = (1..tag.len()).rev().find(|&k| self.pending.ends_with(&tag[..k])).unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                out.push((self.in_think, self.pending.drain(..emit_len).collect()));
            }
            return out;
        }
    }

    fn finish(&mut self) -> Vec<(bool, String)> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![(self.in_think, std::mem::take(&mut self.pending))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fake_server, orion, Recorder};
    use super::super::{offered_tools, Session};
    use super::*;

    fn chunks(deltas: &[Value]) -> String {
        let mut s: String = deltas.iter().map(|d| format!("data: {d}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn delta(d: Value) -> Value {
        json!({ "choices": [{ "index": 0, "delta": d, "finish_reason": null }] })
    }

    fn finish(reason: &str) -> Value {
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
    }

    #[test]
    fn splits_think_tags_across_chunks() {
        let mut s = ThinkSplitter::default();
        let mut all = Vec::new();
        for c in ["<th", "ink>plan", " it</th", "ink>\n\nAnswer <b>", "x</b>"] {
            all.extend(s.feed(c));
        }
        all.extend(s.finish());
        let thinking: String = all.iter().filter(|p| p.0).map(|p| p.1.as_str()).collect();
        let text: String = all.iter().filter(|p| !p.0).map(|p| p.1.as_str()).collect();
        assert_eq!(thinking, "plan it");
        assert_eq!(text, "\n\nAnswer <b>x</b>");
    }

    #[test]
    fn assembles_parallel_tool_calls() {
        let stream = chunks(&[
            delta(json!({"role":"assistant","reasoning_content":"Need two lookups."})),
            delta(json!({"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"system_info","arguments":""}}]})),
            delta(json!({"tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"fs_list","arguments":"{\"pa"}}]})),
            delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]})),
            delta(json!({"tool_calls":[{"index":1,"function":{"arguments":"th\":\"~\"}"}}]})),
            finish("tool_calls"),
            json!({"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":9}}),
        ]);
        let mut thoughts = String::new();
        let done = parse_stream(stream.as_bytes(), &mut |e| {
            if let Event::Thinking(t) = e {
                thoughts.push_str(t)
            }
        })
        .unwrap();
        assert_eq!(thoughts, "Need two lookups.");
        assert_eq!(done.finish.as_deref(), Some("tool_calls"));
        assert_eq!((done.usage.input_tokens, done.usage.output_tokens), (50, 9));
        assert_eq!(done.calls.len(), 2);
        assert_eq!((done.calls[0].name.as_str(), done.calls[0].arguments.as_str()), ("system_info", "{}"));
        assert_eq!(done.calls[1].arguments, r#"{"path":"~"}"#);
    }

    #[test]
    fn tolerates_ollama_style_whole_calls() {
        // One chunk per call, no index, arguments as an object.
        let stream = chunks(&[
            delta(json!({"content":"","tool_calls":[{"id":"x1","function":{"name":"proc_list","arguments":{"sort":"memory"}}}]})),
            finish("stop"),
        ]);
        let done = parse_stream(stream.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(done.calls.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&done.calls[0].arguments).unwrap(), json!({"sort":"memory"}));
    }

    #[test]
    fn stream_errors_surface() {
        let stream = "data: {\"error\":{\"message\":\"model ran out of memory\"}}\n\n";
        let err = parse_stream(stream.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("out of memory"));
    }

    #[test]
    fn tool_loop_round_trip() {
        let models = json!({"object":"list","data":[{"id":"qwen3:8b"},{"id":"llama3.2:3b"}]}).to_string();
        let first = chunks(&[
            delta(json!({"content":"<think>Check the host.</think>"})),
            delta(json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"system_info","arguments":"{}"}}]})),
            delta(json!({"tool_calls":[{"index":1,"id":"c2","function":{"name":"fs_read","arguments":"{\"path\":"}}]})),
            finish("tool_calls"),
        ]);
        let second = chunks(&[delta(json!({"content":"\n\nIt is a Linux box."})), finish("stop")]);
        let (url, requests) = fake_server(vec![models, first, second]);
        let orion = orion();
        let config = LocalConfig { base_url: format!("{url}/v1"), ..Default::default() };
        let backend = LocalBackend::connect(config, "system".into(), &offered_tools(&orion)).unwrap();
        assert!(backend.describe().starts_with("llama3.2:3b"), "picks the first listed model, sorted");

        let mut session = Session::new(&orion, Box::new(backend));
        let mut ui = Recorder::default();
        session.turn("what is this?", &mut ui).unwrap();
        assert_eq!(ui.out, "[Check the host.]<system_info><ok>It is a Linux box.");

        assert_eq!(requests.recv().unwrap().0, "/v1/models");
        let (path, req1) = requests.recv().unwrap();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(req1["model"], "llama3.2:3b");
        assert_eq!(req1["messages"][0]["role"], "system");
        assert!(req1["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "proc_run"));

        let (_, req2) = requests.recv().unwrap();
        let msgs = req2["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "system, user, assistant, two tool results");
        assert_eq!(msgs[2]["content"], "", "thinking is not sent back");
        assert_eq!(msgs[2]["tool_calls"][1]["function"]["arguments"], "{}");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[4]["tool_call_id"], "c2");
        assert!(msgs[4]["content"].as_str().unwrap().contains("INVALID_JSON"));
    }
}
