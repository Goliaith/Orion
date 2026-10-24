//! The Claude backend: a minimal streaming client for the Messages API (raw
//! HTTP + SSE; there is no official Rust SDK).

use std::collections::HashMap;
use std::io::BufRead;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::http::{non_empty_env, secret_env, sse_data, Http};
use super::{Backend, Event, Reply, Stop, ToolCall, ToolResult, ToolSpec, Usage};

const API_VERSION: &str = "2023-06-01";

/// `[chat.claude]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    pub model: String,
    /// `low`, `medium`, `high`, `xhigh` or `max`.
    pub effort: String,
    pub max_tokens: u32,
    /// `updates` shows the model's progress notes between tool calls,
    /// `summarized` shows a summary of its reasoning, `omitted` shows nothing.
    pub thinking_display: String,
    /// Let the API retry a declined request on another model.
    pub fallbacks: bool,
    /// `ANTHROPIC_BASE_URL` overrides this.
    pub base_url: String,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            max_tokens: 64000,
            thinking_display: "updates".into(),
            fallbacks: true,
            base_url: "https://api.anthropic.com".into(),
        }
    }
}

/// A fully assembled response.
#[derive(Debug, Default)]
pub struct Message {
    /// Content blocks exactly as the API sent them (thinking signatures
    /// included), ready to be appended to the history unchanged.
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    /// Raw input of `tool_use` blocks whose streamed JSON did not parse, by
    /// block id. Their `input` is left as `{}` and must not be run.
    pub invalid_tool_inputs: HashMap<String, String>,
}

pub struct Client {
    http: Http,
    /// Betas every request needs because of how it authenticates.
    auth_betas: Vec<&'static str>,
}

impl Client {
    /// Credentials come from `ANTHROPIC_API_KEY`, else `ANTHROPIC_AUTH_TOKEN`
    /// (an OAuth access token). `ANTHROPIC_BASE_URL` overrides `base_url`.
    pub fn from_env(base_url: &str) -> Result<Self> {
        let base = non_empty_env("ANTHROPIC_BASE_URL").unwrap_or_else(|| base_url.to_string());
        if let Some(key) = secret_env("ANTHROPIC_API_KEY")? {
            Ok(Self::with_api_key(&base, &key))
        } else if let Some(token) = secret_env("ANTHROPIC_AUTH_TOKEN")? {
            let headers = vec![("authorization".into(), format!("Bearer {token}"))];
            Ok(Self { http: Http::new(&base, headers), auth_betas: vec!["oauth-2025-04-20"] })
        } else {
            bail!(
                "no Claude credentials: set ANTHROPIC_API_KEY (from console.anthropic.com), or set \
                 ANTHROPIC_AUTH_TOKEN, for example to `ant auth print-credentials --access-token`. \
                 Or run a local model with `orion chat --local`"
            );
        }
    }

    pub fn with_api_key(base_url: &str, key: &str) -> Self {
        Self { http: Http::new(base_url, vec![("x-api-key".into(), key.to_string())]), auth_betas: Vec::new() }
    }

    /// POST a streaming request and assemble the response, reporting progress
    /// through `on`.
    pub fn stream(&self, body: &Value, betas: &[&str], on: &mut dyn FnMut(Event)) -> Result<Message> {
        let mut headers = vec![("anthropic-version", API_VERSION.to_string())];
        let betas: Vec<&str> = betas.iter().chain(&self.auth_betas).copied().collect();
        if !betas.is_empty() {
            headers.push(("anthropic-beta", betas.join(",")));
        }
        let reader = self.http.post_stream("/v1/messages", body, &headers).context("calling the Claude API")?;
        parse_sse(reader, on)
    }
}

pub struct ClaudeBackend {
    client: Client,
    config: ClaudeConfig,
    system: String,
    tools: Vec<Value>,
    /// Append-only, so thinking blocks and the prompt cache stay valid.
    messages: Vec<Value>,
    usage: Usage,
}

impl ClaudeBackend {
    pub fn from_env(config: ClaudeConfig, system: String, tools: &[ToolSpec]) -> Result<Self> {
        let client = Client::from_env(&config.base_url)?;
        Ok(Self::new(client, config, system, tools))
    }

    pub fn new(client: Client, config: ClaudeConfig, system: String, tools: &[ToolSpec]) -> Self {
        let tools = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema,
                    "eager_input_streaming": true,
                })
            })
            .collect();
        Self { client, config, system, tools, messages: Vec::new(), usage: Usage::default() }
    }

    fn request_body(&self) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "stream": true,
            "system": self.system,
            "tools": self.tools,
            "messages": self.messages,
            "thinking": { "type": "adaptive", "display": self.config.thinking_display },
            "output_config": { "effort": self.config.effort },
            // Caches the growing conversation prefix between requests.
            "cache_control": { "type": "ephemeral" },
        });
        if self.config.fallbacks {
            body["fallbacks"] = json!("default");
        }
        body
    }

    fn betas(&self) -> Vec<&'static str> {
        let mut betas = Vec::new();
        if self.config.fallbacks {
            betas.push("server-side-fallback-2026-07-01");
        }
        if self.config.thinking_display == "updates" {
            betas.push("thinking-display-updates-2026-08-18");
        }
        betas
    }
}

impl Backend for ClaudeBackend {
    fn describe(&self) -> String {
        format!("{} · effort {}", self.config.model, self.config.effort)
    }

    fn model(&self) -> String {
        self.config.model.clone()
    }

    fn set_model(&mut self, model: &str) {
        self.config.model = model.to_string();
    }

    fn set_effort(&mut self, effort: &str) -> Result<(), String> {
        const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
        if !EFFORTS.contains(&effort) {
            return Err(format!("effort must be one of {}", EFFORTS.join(", ")));
        }
        self.config.effort = effort.to_string();
        Ok(())
    }

    fn push_user(&mut self, text: &str) {
        self.messages.push(json!({ "role": "user", "content": text }));
    }

    fn push_assistant_text(&mut self, text: &str) {
        self.messages.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] }));
    }

    fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply> {
        let msg = self.client.stream(&self.request_body(), &self.betas(), on)?;
        self.usage.add(&msg.usage);
        let stop = msg.stop_reason.as_deref().unwrap_or_default();
        if stop == "refusal" {
            // A partial reply is discarded, not kept in the history.
            return Ok(Reply { calls: Vec::new(), stop: Stop::Refused });
        }
        let content = echo_content(msg.content);
        let calls: Vec<ToolCall> = content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                let id = b["id"].as_str().unwrap_or_default().to_string();
                ToolCall {
                    invalid_input: msg.invalid_tool_inputs.get(&id).cloned(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                    input: b["input"].clone(),
                    id,
                }
            })
            .collect();
        if !content.is_empty() {
            self.messages.push(json!({ "role": "assistant", "content": content }));
        }
        let stop = match stop {
            "max_tokens" => Stop::Truncated,
            _ if !calls.is_empty() => Stop::ToolUse,
            _ => Stop::Done,
        };
        Ok(Reply { calls, stop })
    }

    fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        // Every tool_use gets a result, all in a single user message.
        let blocks: Vec<Value> = results
            .into_iter()
            .map(|r| json!({ "type": "tool_result", "tool_use_id": r.id, "content": r.content, "is_error": r.is_error }))
            .collect();
        self.messages.push(json!({ "role": "user", "content": blocks }));
    }

    fn clear(&mut self) {
        self.messages.clear();
    }

    fn set_system(&mut self, system: &str) {
        self.system = system.to_string();
    }

    fn export_history(&self) -> Value {
        Value::Array(self.messages.clone())
    }

    fn import_history(&mut self, history: Value) -> Result<()> {
        let Value::Array(messages) = history else { bail!("saved Claude history is not a list") };
        // Verbatim, thinking blocks and all, so they stay valid.
        self.messages = messages;
        Ok(())
    }

    fn usage(&self) -> Usage {
        self.usage
    }
}

/// The blocks of a response that go back into the history. After a
/// mid-response server-side fallback, only text survives from before the last
/// `fallback` marker; the declined model's tool calls and thinking are dropped
/// (and so are never run).
fn echo_content(content: Vec<Value>) -> Vec<Value> {
    let Some(boundary) = content.iter().rposition(|b| b["type"] == "fallback") else { return content };
    content
        .into_iter()
        .enumerate()
        .filter(|(i, b)| *i >= boundary || b["type"] == "text")
        .map(|(_, b)| b)
        .collect()
}

fn merge_usage(usage: &mut Usage, v: &Value) {
    let get = |k: &str| v.get(k).and_then(Value::as_u64);
    if let Some(n) = get("input_tokens") {
        usage.input_tokens = n;
    }
    if let Some(n) = get("output_tokens") {
        usage.output_tokens = n;
    }
    if let Some(n) = get("cache_read_input_tokens") {
        usage.cache_read_input_tokens = n;
    }
    if let Some(n) = get("cache_creation_input_tokens") {
        usage.cache_creation_input_tokens = n;
    }
}

/// Assemble a message from a Server-Sent Events stream.
pub fn parse_sse(reader: impl BufRead, on: &mut dyn FnMut(Event)) -> Result<Message> {
    let mut msg = Message::default();
    // Per block index: the block so far and any streamed tool-input JSON.
    let mut blocks: Vec<(Value, String)> = Vec::new();
    for data in sse_data(reader) {
        let ev: Value = serde_json::from_str(&data?).context("malformed stream event")?;
        let index = ev.get("index").and_then(Value::as_u64).map(|i| i as usize);
        match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                let m = &ev["message"];
                msg.model = m.get("model").and_then(Value::as_str).map(str::to_string);
                merge_usage(&mut msg.usage, &m["usage"]);
            }
            "content_block_start" => {
                let (Some(i), Some(block)) = (index, ev.get("content_block")) else { continue };
                if blocks.len() <= i {
                    blocks.resize(i + 1, (Value::Null, String::new()));
                }
                blocks[i] = (block.clone(), String::new());
            }
            "content_block_delta" => {
                let Some((block, json_buf)) = index.and_then(|i| blocks.get_mut(i)) else { continue };
                let delta = &ev["delta"];
                let append = |block: &mut Value, field: &str, piece: &str| {
                    let cur = block[field].as_str().unwrap_or_default();
                    block[field] = Value::String(format!("{cur}{piece}"));
                };
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let t = delta["text"].as_str().unwrap_or_default();
                        append(block, "text", t);
                        on(Event::Text(t));
                    }
                    "thinking_delta" => {
                        let t = delta["thinking"].as_str().unwrap_or_default();
                        append(block, "thinking", t);
                        on(Event::Thinking(t));
                    }
                    "signature_delta" => append(block, "signature", delta["signature"].as_str().unwrap_or_default()),
                    "input_json_delta" => json_buf.push_str(delta["partial_json"].as_str().unwrap_or_default()),
                    "citations_delta" => {
                        if !block["citations"].is_array() {
                            block["citations"] = json!([]);
                        }
                        block["citations"].as_array_mut().unwrap().push(delta["citation"].clone());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some((block, json_buf)) = index.and_then(|i| blocks.get_mut(i)) else { continue };
                if block["type"] == "tool_use" {
                    let raw = std::mem::take(json_buf);
                    let parsed = if raw.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str(&raw) };
                    match parsed {
                        Ok(input @ Value::Object(_)) => block["input"] = input,
                        _ => {
                            block["input"] = json!({});
                            if let Some(id) = block["id"].as_str() {
                                msg.invalid_tool_inputs.insert(id.to_string(), raw);
                            }
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(r) = ev.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    msg.stop_reason = Some(r.to_string());
                }
                merge_usage(&mut msg.usage, &ev["usage"]);
            }
            "message_stop" => break,
            "error" => bail!(
                "Claude API stream error: {}",
                ev.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown")
            ),
            _ => {} // ping and future event types
        }
    }
    msg.content = blocks.into_iter().map(|(b, _)| b).filter(|b| !b.is_null()).collect();
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fake_server, orion, Recorder};
    use super::super::{offered_tools, Mode, Provider, Session};
    use super::*;

    fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    }

    #[test]
    fn assembles_blocks() {
        let stream = sse(&[
            json!({"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":4}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Checking memory."}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me "}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"look."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"proc_list","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"sort\": \"mem"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"ory\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"fs_read","input":{}}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"/x"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            json!({"type":"message_stop"}),
        ]);
        let mut seen = Vec::new();
        let msg = parse_sse(stream.as_bytes(), &mut |e| {
            seen.push(match e {
                Event::Text(t) => format!("text:{t}"),
                Event::Thinking(t) => format!("thinking:{t}"),
            })
        })
        .unwrap();
        assert_eq!(seen, ["thinking:Checking memory.", "text:Let me ", "text:look."]);
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(msg.usage.input_tokens, 10);
        assert_eq!(msg.usage.output_tokens, 42);
        assert_eq!(msg.content[0], json!({"type":"thinking","thinking":"Checking memory.","signature":"sig=="}));
        assert_eq!(msg.content[1]["text"], "Let me look.");
        assert_eq!(msg.content[2]["input"], json!({"sort":"memory"}));
        assert_eq!(msg.content[3]["input"], json!({}));
        assert_eq!(msg.invalid_tool_inputs["t2"], "{\"path\": \"/x");
    }

    #[test]
    fn surfaces_stream_errors() {
        let stream = sse(&[json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})]);
        let err = parse_sse(stream.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("Overloaded"));
    }

    #[test]
    fn tool_loop_round_trip() {
        let first = sse(&[
            json!({"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":100}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Checking the host."}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"system_info","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"fs_write","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/tmp/orion-test-root/x\",\"content\":\"hi\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}),
            json!({"type":"message_stop"}),
        ]);
        let second = sse(&[
            json!({"type":"message_start","message":{"usage":{"input_tokens":300}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, requests) = fake_server(vec![first, second]);
        let orion = orion();
        let backend = ClaudeBackend::new(
            Client::with_api_key(&url, "test"),
            ClaudeConfig::default(),
            "system".into(),
            &offered_tools(&orion),
        );
        let mut session = Session::with_backends(&orion, Mode::Claude, Some(Box::new(backend)), None);
        let mut ui = Recorder::default();
        session.turn("what machine is this?", &mut ui).unwrap();

        assert_eq!(ui.out, "[Checking the host.]<system_info><ok><fs_write><err>Done.");
        assert_eq!(ui.approvals, 1, "fs_write is `ask` by default");
        let usage = session.backend(Provider::Claude).unwrap().usage();
        assert_eq!((usage.input_tokens, usage.output_tokens), (400, 25));

        let (path, req1) = requests.recv().unwrap();
        assert_eq!(path, "/v1/messages");
        assert_eq!(req1["model"], "claude-opus-5-5");
        assert_eq!(req1["fallbacks"], "default");
        let offered: Vec<&str> = req1["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(offered.contains(&"proc_run") && offered.contains(&"fs_write"));

        let (_, req2) = requests.recv().unwrap();
        let msgs = req2["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // The thinking block goes back unchanged, signature included.
        assert_eq!(msgs[1]["content"][0], json!({"type":"thinking","thinking":"Checking the host.","signature":"abc"}));
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[0]["is_error"], false);
        assert_eq!(results[1]["is_error"], true);
        assert!(results[1]["content"].as_str().unwrap().contains("declined"));
    }

    #[test]
    fn history_round_trips_verbatim() {
        let mut b = ClaudeBackend::new(Client::with_api_key("http://127.0.0.1:9", "k"), ClaudeConfig::default(), "s".into(), &[]);
        let history = json!([
            {"role":"user","content":"hi"},
            {"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"EqQBCkgIARABGAIi"},{"type":"text","text":"Hello."}]},
        ]);
        b.import_history(history.clone()).unwrap();
        assert_eq!(b.export_history(), history);
        assert!(b.import_history(json!({"not": "a list"})).is_err());
    }

    #[test]
    fn fallback_echo_drops_declined_partial() {
        let content = vec![
            json!({"type":"thinking","thinking":"","signature":"a"}),
            json!({"type":"text","text":"Partial"}),
            json!({"type":"tool_use","id":"t1","name":"fs_read","input":{}}),
            json!({"type":"fallback","from":{"model":"x"},"to":{"model":"y"}}),
            json!({"type":"thinking","thinking":"","signature":"b"}),
            json!({"type":"text","text":" continued"}),
        ];
        let kept: Vec<Value> = echo_content(content);
        let types: Vec<&str> = kept.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["text", "fallback", "thinking", "text"]);
        assert_eq!(kept[2]["signature"], "b");
    }
}
