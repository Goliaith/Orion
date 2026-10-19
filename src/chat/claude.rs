//! Minimal streaming client for the Claude Messages API (raw HTTP + SSE;
//! there is no official Rust SDK).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const API_VERSION: &str = "2023-06-01";
const MAX_RETRIES: u32 = 3;

/// What the UI hears while a response streams in.
pub enum Event<'a> {
    Text(&'a str),
    /// Text of a thinking block. With `display: "updates"` these are the
    /// model's progress notes between tool calls.
    Thinking(&'a str),
    ToolUse(&'a str),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    fn merge(&mut self, v: &Value) {
        let get = |k: &str| v.get(k).and_then(Value::as_u64);
        if let Some(n) = get("input_tokens") {
            self.input_tokens = n;
        }
        if let Some(n) = get("output_tokens") {
            self.output_tokens = n;
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.cache_read_input_tokens = n;
        }
        if let Some(n) = get("cache_creation_input_tokens") {
            self.cache_creation_input_tokens = n;
        }
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
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

enum Auth {
    ApiKey(String),
    Bearer(String),
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
    auth: Auth,
}

impl Client {
    /// Credentials come from `ANTHROPIC_API_KEY`, else `ANTHROPIC_AUTH_TOKEN`
    /// (an OAuth access token). `ANTHROPIC_BASE_URL` overrides `base_url`.
    pub fn from_env(base_url: &str) -> Result<Self> {
        let auth = if let Some(key) = non_empty_env("ANTHROPIC_API_KEY") {
            Auth::ApiKey(key)
        } else if let Some(token) = non_empty_env("ANTHROPIC_AUTH_TOKEN") {
            Auth::Bearer(token)
        } else {
            bail!(
                "no Claude credentials: set ANTHROPIC_API_KEY (from console.anthropic.com), or set \
                 ANTHROPIC_AUTH_TOKEN, for example to `ant auth print-credentials --access-token`"
            );
        };
        let base = non_empty_env("ANTHROPIC_BASE_URL").unwrap_or_else(|| base_url.to_string());
        Ok(Self::new(&base, auth))
    }

    pub fn with_api_key(base_url: &str, key: &str) -> Self {
        Self::new(base_url, Auth::ApiKey(key.to_string()))
    }

    fn new(base_url: &str, auth: Auth) -> Self {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(!bypass_proxy(base_url))
            .timeout_connect(Duration::from_secs(30))
            // Long thinking can leave a stream quiet for a while; the API
            // sends pings, so this only trips on a dead connection.
            .timeout_read(Duration::from_secs(600))
            .build();
        Self { agent, url: format!("{}/v1/messages", base_url.trim_end_matches('/')), auth }
    }

    /// POST a streaming request and assemble the response, reporting progress
    /// through `on`. Retries rate limits, overload and connection failures
    /// that happen before the stream starts.
    pub fn stream(&self, body: &Value, betas: &[&str], on: &mut dyn FnMut(Event)) -> Result<Message> {
        let body = serde_json::to_string(body)?;
        let mut attempt = 0;
        loop {
            let mut req = self
                .agent
                .post(&self.url)
                .set("content-type", "application/json")
                .set("anthropic-version", API_VERSION);
            let mut betas: Vec<&str> = betas.to_vec();
            match &self.auth {
                Auth::ApiKey(key) => req = req.set("x-api-key", key),
                Auth::Bearer(token) => {
                    req = req.set("authorization", &format!("Bearer {token}"));
                    betas.push("oauth-2025-04-20");
                }
            }
            if !betas.is_empty() {
                req = req.set("anthropic-beta", &betas.join(","));
            }
            let wait = match req.send_string(&body) {
                Ok(resp) => return parse_sse(BufReader::new(resp.into_reader()), on),
                Err(ureq::Error::Status(code, resp)) if retryable(code) && attempt < MAX_RETRIES => {
                    resp.header("retry-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs)
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    bail!("Claude API error {code}: {}", api_error_message(&text));
                }
                Err(ureq::Error::Transport(t)) if attempt < MAX_RETRIES => {
                    tracing::debug!("transport error, retrying: {t}");
                    None
                }
                Err(e) => return Err(anyhow!(e).context("calling the Claude API")),
            };
            attempt += 1;
            thread::sleep(wait.unwrap_or(Duration::from_secs(1 << attempt)).min(Duration::from_secs(60)));
        }
    }
}

/// Loopback hosts and hosts matched by `NO_PROXY` (which ureq ignores) are
/// reached directly.
fn bypass_proxy(base_url: &str) -> bool {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
    .to_ascii_lowercase();
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        return true;
    }
    let no_proxy = non_empty_env("NO_PROXY").or_else(|| non_empty_env("no_proxy")).unwrap_or_default();
    no_proxy.split(',').map(|p| p.trim().trim_start_matches('.').to_ascii_lowercase()).any(|p| {
        p == "*" || (!p.is_empty() && (host == p || host.ends_with(&format!(".{p}"))))
    })
}

fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}

fn api_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string())
}

/// Assemble a message from a Server-Sent Events stream.
pub fn parse_sse(reader: impl BufRead, on: &mut dyn FnMut(Event)) -> Result<Message> {
    let mut msg = Message::default();
    // Per block index: the block so far and any streamed tool-input JSON.
    let mut blocks: Vec<(Value, String)> = Vec::new();
    for line in reader.lines() {
        let line = line.context("reading the response stream")?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let ev: Value = serde_json::from_str(data.trim()).context("malformed stream event")?;
        let index = ev.get("index").and_then(Value::as_u64).map(|i| i as usize);
        match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                let m = &ev["message"];
                msg.model = m.get("model").and_then(Value::as_str).map(str::to_string);
                msg.usage.merge(&m["usage"]);
            }
            "content_block_start" => {
                let (Some(i), Some(block)) = (index, ev.get("content_block")) else { continue };
                if blocks.len() <= i {
                    blocks.resize(i + 1, (Value::Null, String::new()));
                }
                if block["type"] == "tool_use" {
                    on(Event::ToolUse(block["name"].as_str().unwrap_or_default()));
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
                msg.usage.merge(&ev["usage"]);
            }
            "message_stop" => break,
            "error" => bail!("Claude API stream error: {}", ev.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown")),
            _ => {} // ping and future event types
        }
    }
    msg.content = blocks.into_iter().map(|(b, _)| b).filter(|b| !b.is_null()).collect();
    Ok(msg)
}

#[cfg(test)]
mod tests {
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
                Event::ToolUse(n) => format!("tool:{n}"),
            })
        })
        .unwrap();
        assert_eq!(seen, ["thinking:Checking memory.", "text:Let me ", "text:look.", "tool:proc_list", "tool:fs_read"]);
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
    fn loopback_skips_the_proxy() {
        assert!(bypass_proxy("http://127.0.0.1:8080"));
        assert!(bypass_proxy("http://localhost/v1"));
        assert!(bypass_proxy("http://[::1]:9"));
    }

    #[test]
    fn surfaces_stream_errors() {
        let stream = sse(&[json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})]);
        let err = parse_sse(stream.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("Overloaded"));
    }
}
